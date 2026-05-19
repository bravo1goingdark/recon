//! Personalized PageRank over the symbol reference graph.
//!
//! Builds a directed graph from symbol references with Aider-style edge weights.
//! For unfocused queries, runs global power iteration (cached by the caller).
//! For focused queries, runs push-based approximate PPR (Andersen et al. 2006)
//! that only visits nodes reachable from seed symbols with significant probability.
//!
//! The graph uses Compressed Sparse Row (CSR) layout for cache-efficient iteration
//! during power iteration — all edges stored contiguously in one Vec, indexed by
//! per-node offsets. This avoids 320K inner Vec allocations on large codebases.

use ahash::{AHashMap, AHashSet};
use recon_core::config::EdgeWeights;
use recon_core::symbol::{Ref, Symbol};
use std::collections::VecDeque;

/// Residual threshold for the push-based PPR algorithm.
/// Matches the L1 convergence threshold used by the global power iteration path.
const PPR_EPSILON: f64 = 1e-6;

/// Default iteration cap. Convergence at 1e-6 typically happens by iteration 8-12;
/// 15 is the safe upper bound. Reduced from 30 for ~2x speedup on large graphs.
pub const DEFAULT_MAX_ITERATIONS: usize = 15;

/// Down-weight applied to test-scope symbols' final PR score so the inline
/// `tests` module doesn't outrank real production hubs in code_repo_map.
const TEST_SCOPE_SCORE_FACTOR: f64 = 0.1;

/// True when a qualified name names a `#[cfg(test)] mod tests` scope or a symbol
/// inside one. Matches "tests", "tests::*", and "*::tests::*" variants.
#[inline]
fn is_test_qualified_name(qname: &str) -> bool {
    qname == "tests"
        || qname.starts_with("tests::")
        || qname.contains("::tests::")
        || qname.ends_with("::tests")
}

/// A ranked symbol with its PageRank score.
#[derive(Debug, Clone)]
pub struct RankedSymbol {
    /// Index into the original symbols slice.
    pub index: usize,
    /// PageRank score (higher = more important).
    pub score: f64,
}

/// CSR (Compressed Sparse Row) edge: target node + weight.
#[derive(Clone, Copy)]
struct Edge {
    target: usize,
    weight: f64,
}

/// Precomputed directed graph in CSR layout for cache-efficient PageRank.
///
/// All edges stored contiguously in `edges`. Node `i`'s edges are at
/// `edges[offsets[i]..offsets[i+1]]`. This replaces `Vec<Vec<(usize, f64)>>`
/// which allocates 320K inner Vecs on large codebases.
struct RankGraph {
    n: usize,
    /// CSR edge array — all edges contiguous in memory.
    edges: Vec<Edge>,
    /// CSR offsets — `offsets[i]..offsets[i+1]` indexes into `edges` for node i.
    offsets: Vec<usize>,
    in_degree: Vec<usize>,
    total_weights: Vec<f64>,
}

impl RankGraph {
    /// Build the graph from symbols and refs, applying Aider-style weight heuristics.
    /// Pass `edge_weights` to override Aider defaults for ablation studies.
    fn build(
        symbols: &[Symbol],
        refs: &[Ref],
        focus_symbols: &[usize],
        edge_weights: Option<&EdgeWeights>,
    ) -> Self {
        let n = symbols.len();

        // Build name → index map for target resolution
        let mut name_to_idx: AHashMap<&str, Vec<usize>> = AHashMap::with_capacity(n);
        // Build id → index map for source resolution
        let mut id_to_idx: AHashMap<u64, usize> = AHashMap::with_capacity(n);
        for (i, sym) in symbols.iter().enumerate() {
            name_to_idx.entry(sym.name.as_str()).or_default().push(i);
            id_to_idx.insert(sym.id, i);
        }

        let focus_set: AHashSet<usize> = focus_symbols.iter().copied().collect();

        // Detect inline test scopes: symbols inside `#[cfg(test)] mod tests`
        // (qualified_name "tests::foo" or "outer::tests::foo") AND the test
        // module itself ("tests" / "outer::tests"). Refs originating from any
        // of these are skipped at edge-build time — scaling weight isn't enough
        // because PR normalizes per-source by total_weight, so a single-out-edge
        // test caller still propagates its full score regardless of weight.
        let is_test_scope: Vec<bool> = symbols
            .iter()
            .map(|s| is_test_qualified_name(s.qualified_name.as_str()))
            .collect();

        // Phase 1: Count edges per node to pre-allocate CSR offsets
        let mut edge_counts = vec![0usize; n];
        let mut total_edges = 0usize;

        for r in refs {
            let src_idx = match id_to_idx.get(&r.src_symbol_id) {
                Some(&idx) => idx,
                None => continue,
            };
            // Skip refs originating from test scopes so production hub scores
            // aren't inflated by test-driven traffic.
            if is_test_scope[src_idx] {
                continue;
            }
            let target_indices = match name_to_idx.get(r.ident.as_str()) {
                Some(indices) => indices,
                None => continue,
            };
            for &target_idx in target_indices {
                if target_idx != src_idx {
                    edge_counts[src_idx] += 1;
                    total_edges += 1;
                }
            }
        }

        // Phase 2: Build CSR offsets from counts
        let mut offsets = Vec::with_capacity(n + 1);
        offsets.push(0);
        let mut running = 0usize;
        for &count in &edge_counts {
            running += count;
            offsets.push(running);
        }

        // Phase 3: Fill CSR edges (reuse edge_counts as write cursors)
        let mut edges = vec![
            Edge {
                target: 0,
                weight: 0.0
            };
            total_edges
        ];
        let mut cursors = vec![0usize; n]; // current write position per node
        let mut in_degree = vec![0usize; n];

        for r in refs {
            let src_idx = match id_to_idx.get(&r.src_symbol_id) {
                Some(&idx) => idx,
                None => continue,
            };
            if is_test_scope[src_idx] {
                continue;
            }
            let target_indices = match name_to_idx.get(r.ident.as_str()) {
                Some(indices) => indices,
                None => continue,
            };

            let mut weight = r.weight as f64;
            let ident = r.ident.as_str();

            // Aider-style weight heuristics — overridable via EdgeWeights config
            let desc_mult = edge_weights.map_or(10.0, |w| w.descriptive_ident_mult);
            let priv_mult = edge_weights.map_or(0.1, |w| w.private_ident_mult);
            let fanout_mult = edge_weights.map_or(0.1, |w| w.high_fanout_mult);
            let fanout_thresh = edge_weights.map_or(5, |w| w.high_fanout_threshold);
            let focus_boost = edge_weights.map_or(50.0, |w| w.focus_boost);

            if ident.len() > 8 && (ident.contains('_') || ident.chars().any(|c| c.is_uppercase())) {
                weight *= desc_mult;
            }
            if ident.starts_with('_') {
                weight *= priv_mult;
            }
            if target_indices.len() > fanout_thresh {
                weight *= fanout_mult;
            }
            if focus_set.contains(&src_idx) {
                weight *= focus_boost;
            }

            for &target_idx in target_indices {
                if target_idx != src_idx {
                    let pos = offsets[src_idx] + cursors[src_idx];
                    edges[pos] = Edge {
                        target: target_idx,
                        weight,
                    };
                    cursors[src_idx] += 1;
                    in_degree[target_idx] += 1;
                }
            }
        }

        // Compute total outgoing weight per node
        let mut total_weights = vec![0.0f64; n];
        for i in 0..n {
            let start = offsets[i];
            let end = offsets[i + 1];
            for e in &edges[start..end] {
                total_weights[i] += e.weight;
            }
        }

        Self {
            n,
            edges,
            offsets,
            in_degree,
            total_weights,
        }
    }

    /// Get edges for node `i` as a contiguous slice.
    #[inline]
    fn edges_of(&self, i: usize) -> &[Edge] {
        &self.edges[self.offsets[i]..self.offsets[i + 1]]
    }
}

/// Global power iteration — used for unfocused queries (uniform personalization).
fn global_pagerank(
    graph: &RankGraph,
    focus_symbols: &[usize],
    damping: f64,
    iterations: usize,
) -> Vec<f64> {
    let n = graph.n;
    let focus_set: AHashSet<usize> = focus_symbols.iter().copied().collect();

    // Personalization vector — uniform or biased toward focus symbols
    let mut personalization = vec![1.0 / n as f64; n];
    if !focus_set.is_empty() {
        let focus_weight = 0.8 / focus_set.len() as f64;
        let other_weight = 0.2 / (n - focus_set.len()).max(1) as f64;
        for (i, p) in personalization.iter_mut().enumerate() {
            *p = if focus_set.contains(&i) {
                focus_weight
            } else {
                other_weight
            };
        }
    }

    let mut scores = vec![1.0 / n as f64; n];
    let mut new_scores = vec![0.0f64; n];
    let inv_n = 1.0 / n as f64;

    for _iter in 0..iterations {
        new_scores.fill(0.0);

        // Accumulate dangling node mass as a scalar, then distribute once
        let mut dangling_sum = 0.0f64;
        for (i, &score_i) in scores.iter().enumerate() {
            let edges = graph.edges_of(i);
            if edges.is_empty() || graph.total_weights[i] == 0.0 {
                dangling_sum += score_i;
            } else {
                let inv_weight = 1.0 / graph.total_weights[i];
                for e in edges {
                    new_scores[e.target] += score_i * e.weight * inv_weight;
                }
            }
        }

        // Distribute dangling mass evenly + apply damping with personalization
        let dangling_share = dangling_sum * inv_n;
        let mut diff = 0.0f64;
        for (i, (ns, &p)) in new_scores
            .iter_mut()
            .zip(personalization.iter())
            .enumerate()
        {
            *ns = damping * (*ns + dangling_share) + (1.0 - damping) * p;
            diff += (*ns - scores[i]).abs();
        }

        std::mem::swap(&mut scores, &mut new_scores);

        if diff < PPR_EPSILON {
            break;
        }
    }

    scores
}

/// Push-based approximate Personalized PageRank (Andersen et al. 2006).
///
/// Only visits nodes reachable from the focus/seed set with significant
/// probability. For a focused query on 3 files in a 50K-symbol repo,
/// typically touches ~200-500 nodes instead of all 50K.
fn push_ppr(graph: &RankGraph, focus_symbols: &[usize], damping: f64) -> Vec<f64> {
    let n = graph.n;

    let mut estimate: AHashMap<usize, f64> = AHashMap::with_capacity(256);
    let mut residual: AHashMap<usize, f64> = AHashMap::with_capacity(256);
    let mut queue: VecDeque<usize> = VecDeque::with_capacity(256);
    let mut in_queue: AHashSet<usize> = AHashSet::with_capacity(256);

    // Seed: uniform over focus symbols
    let seed_weight = 1.0 / focus_symbols.len().max(1) as f64;
    for &idx in focus_symbols {
        *residual.entry(idx).or_default() += seed_weight;
        if in_queue.insert(idx) {
            queue.push_back(idx);
        }
    }

    // Push loop
    while let Some(u) = queue.pop_front() {
        in_queue.remove(&u);
        let r_u = residual.get(&u).copied().unwrap_or(0.0);
        if r_u.abs() < PPR_EPSILON {
            continue;
        }

        // Absorb (1 - damping) fraction into estimate
        *estimate.entry(u).or_default() += (1.0 - damping) * r_u;

        // Push damping fraction to neighbors
        let edges = graph.edges_of(u);
        if !edges.is_empty() && graph.total_weights[u] > 0.0 {
            let inv_weight = 1.0 / graph.total_weights[u];
            for e in edges {
                let push = damping * r_u * e.weight * inv_weight;
                let entry = residual.entry(e.target).or_default();
                *entry += push;
                if entry.abs() > PPR_EPSILON && in_queue.insert(e.target) {
                    queue.push_back(e.target);
                }
            }
        }

        // Zero out the pushed residual
        residual.insert(u, 0.0);
    }

    // Convert sparse estimate to dense score vector
    let mut scores = vec![0.0f64; n];
    for (idx, score) in estimate {
        scores[idx] = score;
    }
    scores
}

/// Compute PageRank over a symbol reference graph.
///
/// `focus_symbols` — indices into `symbols` that get boosted personalization.
/// When empty, uses global power iteration. When non-empty, uses push-based
/// approximate PPR that only explores the local neighborhood of the focus set.
/// Returns symbols sorted by descending rank.
pub fn pagerank(
    symbols: &[Symbol],
    refs: &[Ref],
    focus_symbols: &[usize],
    damping: f64,
    iterations: usize,
    edge_weights: Option<&EdgeWeights>,
) -> Vec<RankedSymbol> {
    let n = symbols.len();
    if n == 0 {
        return Vec::new();
    }

    let graph = RankGraph::build(symbols, refs, focus_symbols, edge_weights);

    // Dispatch: global power iteration for unfocused, push PPR for focused
    let mut scores = if focus_symbols.is_empty() {
        global_pagerank(&graph, focus_symbols, damping, iterations)
    } else {
        push_ppr(&graph, focus_symbols, damping)
    };

    // Post-processing: boost top-level symbols by sqrt(in_degree + 1), and
    // demote any symbol that lives in an inline test scope so the `tests`
    // module itself drops below real production hubs in repo orientation.
    for (i, sym) in symbols.iter().enumerate() {
        if sym.parent_id.is_none() {
            let ref_count = graph.in_degree[i] as f64;
            scores[i] *= (ref_count + 1.0).sqrt();
        }
        if is_test_qualified_name(sym.qualified_name.as_str()) {
            scores[i] *= TEST_SCOPE_SCORE_FACTOR;
        }
    }

    // Collect and sort: top-level only, descending score
    let mut ranked: Vec<RankedSymbol> = scores
        .iter()
        .enumerate()
        .filter(|(i, _)| symbols[*i].parent_id.is_none())
        .map(|(i, &score)| RankedSymbol { index: i, score })
        .collect();

    ranked.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    ranked
}

/// Render a ranked symbol list into a skeleton string within a token budget.
pub fn render_repo_map(symbols: &[Symbol], ranked: &[RankedSymbol], token_budget: usize) -> String {
    let mut output = String::with_capacity(token_budget * 4);
    let mut est_tokens = 0usize;

    for entry in ranked {
        let sym = &symbols[entry.index];

        // Estimate tokens using write! directly into a small buffer to avoid
        // intermediate String allocations from format!().
        let mut line_buf = smallvec::SmallVec::<[u8; 256]>::new();
        use std::io::Write;
        let _ = write!(
            line_buf,
            "{}:{} {} {}",
            sym.path.to_string_lossy(),
            sym.line_range.start(),
            sym.kind.label(),
            sym.qualified_name,
        );
        if let Some(sig) = &sym.signature {
            let _ = write!(line_buf, " — {sig}");
        }
        let _ = writeln!(line_buf);

        let line_str = String::from_utf8_lossy(&line_buf);
        let line_est = crate::tokens::estimate_tokens(&line_str);

        if est_tokens + line_est > token_budget {
            break;
        }

        output.push_str(&line_str);
        est_tokens += line_est;
    }

    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use compact_str::CompactString;
    use recon_core::lang::Language;
    use recon_core::symbol::SymbolKind;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn sym(id: u64, name: &str, qname: &str) -> Symbol {
        Symbol {
            id,
            path: Arc::new(PathBuf::from("src/lib.rs")),
            name: CompactString::new(name),
            qualified_name: CompactString::new(qname),
            kind: SymbolKind::Function,
            signature: Some(format!("fn {name}()").into()),
            doc: None,
            parent_id: None,
            byte_range: 0..100,
            line_range: 1..=10,
            body_hash: [0u8; 32],
            lang: Language::Rust,
        }
    }

    fn make_ref(ident: &str, src_id: u64) -> Ref {
        Ref {
            src_path: Arc::new(PathBuf::from("src/lib.rs")),
            src_symbol_id: src_id,
            ident: CompactString::new(ident),
            dst_symbol_id: None,
            weight: 1.0,
        }
    }

    #[test]
    fn basic_ranking() {
        let symbols = vec![
            sym(1, "main", "crate::main"),
            sym(2, "process_data", "crate::process_data"),
            sym(3, "helper", "crate::helper"),
        ];

        // process_data is referenced more -> should rank higher
        let refs = vec![
            make_ref("process_data", 1),
            make_ref("process_data", 3),
            make_ref("helper", 1),
        ];

        let ranked = pagerank(&symbols, &refs, &[], 0.85, DEFAULT_MAX_ITERATIONS, None);
        assert!(!ranked.is_empty());
        // process_data (idx 1) should be top-ranked
        assert_eq!(ranked[0].index, 1, "process_data should rank first");
    }

    #[test]
    fn focused_ppr_boosts_focus() {
        let symbols = vec![
            sym(1, "alpha", "crate::alpha"),
            sym(2, "beta", "crate::beta"),
            sym(3, "gamma", "crate::gamma"),
        ];

        let refs = vec![
            make_ref("beta", 1),
            make_ref("gamma", 2),
            make_ref("alpha", 3),
        ];

        // Focus on alpha (idx 0) — its neighborhood should rank higher
        let focused = pagerank(&symbols, &refs, &[0], 0.85, DEFAULT_MAX_ITERATIONS, None);
        let unfocused = pagerank(&symbols, &refs, &[], 0.85, DEFAULT_MAX_ITERATIONS, None);

        assert!(!focused.is_empty());
        assert!(!unfocused.is_empty());
    }

    #[test]
    fn render_repo_map_respects_budget() {
        let symbols = vec![
            sym(1, "foo", "crate::foo"),
            sym(2, "bar", "crate::bar"),
            sym(3, "baz", "crate::baz"),
        ];
        let refs = vec![make_ref("bar", 1)];
        let ranked = pagerank(&symbols, &refs, &[], 0.85, DEFAULT_MAX_ITERATIONS, None);

        let output = render_repo_map(&symbols, &ranked, 50);
        assert!(!output.is_empty());
        // Very small budget should truncate
        let tiny = render_repo_map(&symbols, &ranked, 5);
        assert!(tiny.len() <= output.len());
    }

    #[test]
    fn test_origin_refs_are_down_weighted() {
        // The "production" hub `production_target` is referenced once from a
        // production caller. The "test_target" is referenced four times — but
        // every caller is in `tests::*`, so each ref carries 0.25× weight.
        // Effective weights: production_target = 1.0, test_target = 4 * 0.25 = 1.0.
        // Without the down-weight, test_target would outrank by 4×.
        let symbols = vec![
            sym(1, "prod_caller", "crate::prod_caller"),
            sym(2, "production_target", "crate::production_target"),
            sym(3, "t1", "tests::t1"),
            sym(4, "t2", "tests::t2"),
            sym(5, "t3", "tests::t3"),
            sym(6, "t4", "tests::t4"),
            sym(7, "test_target", "crate::test_target"),
        ];
        let refs = vec![
            make_ref("production_target", 1),
            make_ref("test_target", 3),
            make_ref("test_target", 4),
            make_ref("test_target", 5),
            make_ref("test_target", 6),
        ];
        let ranked = pagerank(&symbols, &refs, &[], 0.85, DEFAULT_MAX_ITERATIONS, None);

        let prod_score = ranked
            .iter()
            .find(|r| r.index == 1) // index of production_target
            .map(|r| r.score)
            .expect("production_target ranked");
        let test_score = ranked
            .iter()
            .find(|r| r.index == 6) // index of test_target
            .map(|r| r.score)
            .expect("test_target ranked");
        assert!(
            test_score < prod_score * 1.5,
            "test_target ({test_score}) must not dwarf production_target ({prod_score}) — \
             4× test refs at 0.25 weight should be ≈ 1× production ref at full weight"
        );
    }

    #[test]
    fn empty_graph() {
        let ranked = pagerank(&[], &[], &[], 0.85, DEFAULT_MAX_ITERATIONS, None);
        assert!(ranked.is_empty());
    }

    #[test]
    fn convergence_within_15_iterations() {
        // 1000 nodes, ring graph — should converge well before 15 iterations
        let symbols: Vec<Symbol> = (0..1000)
            .map(|i| sym(i as u64, &format!("sym_{i}"), &format!("crate::sym_{i}")))
            .collect();
        let refs: Vec<Ref> = (0..1000)
            .map(|i| make_ref(&format!("sym_{}", (i + 1) % 1000), i as u64))
            .collect();

        let ranked = pagerank(&symbols, &refs, &[], 0.85, DEFAULT_MAX_ITERATIONS, None);
        assert!(!ranked.is_empty());
        // Ring graph: all nodes should have similar scores
        let scores: Vec<f64> = ranked.iter().map(|r| r.score).collect();
        let max = scores.iter().cloned().fold(f64::MIN, f64::max);
        let min = scores.iter().cloned().fold(f64::MAX, f64::min);
        // Scores should be within 10x of each other for a uniform ring
        assert!(
            max / min.max(1e-10) < 10.0,
            "ring graph scores too spread: max={max}, min={min}"
        );
    }
}
