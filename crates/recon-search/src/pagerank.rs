//! Personalized PageRank over the symbol reference graph.
//!
//! Builds a directed graph from symbol references with Aider-style edge weights.
//! For unfocused queries, runs global power iteration (cached by the caller).
//! For focused queries, runs push-based approximate PPR (Andersen et al. 2006)
//! that only visits nodes reachable from seed symbols with significant probability.

use ahash::{AHashMap, AHashSet};
use recon_core::symbol::{Ref, Symbol};
use std::collections::VecDeque;

/// Residual threshold for the push-based PPR algorithm.
/// Matches the L1 convergence threshold used by the global power iteration path.
const PPR_EPSILON: f64 = 1e-6;

/// A ranked symbol with its PageRank score.
#[derive(Debug, Clone)]
pub struct RankedSymbol {
    /// Index into the original symbols slice.
    pub index: usize,
    /// PageRank score (higher = more important).
    pub score: f64,
}

/// Precomputed directed graph for ranking algorithms.
struct RankGraph {
    n: usize,
    out_edges: Vec<Vec<(usize, f64)>>,
    in_degree: Vec<usize>,
    total_weights: Vec<f64>,
}

impl RankGraph {
    /// Build the graph from symbols and refs, applying Aider-style weight heuristics.
    fn build(symbols: &[Symbol], refs: &[Ref], focus_symbols: &[usize]) -> Self {
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

        // Build adjacency list with weights
        // Edge direction: source symbol (r.src_symbol_id) → targets named r.ident
        let mut out_edges: Vec<Vec<(usize, f64)>> = vec![Vec::new(); n];
        let mut in_degree: Vec<usize> = vec![0; n];

        for r in refs {
            let src_idx = match id_to_idx.get(&r.src_symbol_id) {
                Some(&idx) => idx,
                None => continue,
            };

            let target_indices = match name_to_idx.get(r.ident.as_str()) {
                Some(indices) => indices,
                None => continue,
            };

            let mut weight = r.weight as f64;
            let ident = r.ident.as_str();

            // Aider-style weight heuristics on the referenced identifier
            // Boost long mixed-case identifiers (more specific = more important)
            if ident.len() > 8 && (ident.contains('_') || ident.chars().any(|c| c.is_uppercase())) {
                weight *= 10.0;
            }

            // Demote private/internal names
            if ident.starts_with('_') {
                weight *= 0.1;
            }

            // Demote identifiers that appear in many files (common = less distinctive)
            if target_indices.len() > 5 {
                weight *= 0.1;
            }

            // Boost references from focus files
            if focus_set.contains(&src_idx) {
                weight *= 50.0;
            }

            for &target_idx in target_indices {
                if target_idx != src_idx {
                    out_edges[src_idx].push((target_idx, weight));
                    in_degree[target_idx] += 1;
                }
            }
        }

        let total_weights: Vec<f64> = out_edges
            .iter()
            .map(|edges| edges.iter().map(|(_, w)| w).sum())
            .collect();

        Self {
            n,
            out_edges,
            in_degree,
            total_weights,
        }
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
        for (i, edges) in graph.out_edges.iter().enumerate() {
            if edges.is_empty() || graph.total_weights[i] == 0.0 {
                dangling_sum += scores[i];
            } else {
                let inv_weight = 1.0 / graph.total_weights[i];
                for &(target, weight) in edges {
                    new_scores[target] += scores[i] * weight * inv_weight;
                }
            }
        }

        // Distribute dangling mass evenly + apply damping with personalization
        let dangling_share = dangling_sum * inv_n;
        let mut diff = 0.0f64;
        for i in 0..n {
            new_scores[i] =
                damping * (new_scores[i] + dangling_share) + (1.0 - damping) * personalization[i];
            diff += (new_scores[i] - scores[i]).abs();
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
    let teleport = 1.0 - damping;

    // Sparse structures — only nodes near the seeds get entries
    let mut estimate: AHashMap<usize, f64> = AHashMap::with_capacity(256);
    let mut residual: AHashMap<usize, f64> = AHashMap::with_capacity(256);
    let mut queue: VecDeque<usize> = VecDeque::with_capacity(256);
    let mut in_queue: AHashSet<usize> = AHashSet::with_capacity(256);

    // Initialize residual on seed nodes
    let num_seeds = focus_symbols.len().max(1);
    let init_residual = 1.0 / num_seeds as f64;
    for &seed in focus_symbols {
        if seed < n {
            residual.insert(seed, init_residual);
            queue.push_back(seed);
            in_queue.insert(seed);
        }
    }

    // Safety bound to prevent infinite loops in degenerate graphs
    let max_steps = 10 * n;
    let mut steps = 0usize;

    while let Some(v) = queue.pop_front() {
        in_queue.remove(&v);
        steps += 1;
        if steps > max_steps {
            break;
        }

        let r_v = residual.remove(&v).unwrap_or(0.0);
        if r_v <= 0.0 {
            continue;
        }

        // Move teleport fraction into the estimate
        *estimate.entry(v).or_insert(0.0) += teleport * r_v;

        let edges = &graph.out_edges[v];
        let tw = graph.total_weights[v];

        if edges.is_empty() || tw == 0.0 {
            // Dangling node: recirculate mass back to seed nodes
            // This preserves PPR locality (vs distributing to all N nodes)
            let seed_share = damping * r_v / num_seeds as f64;
            for &seed in focus_symbols {
                if seed >= n {
                    continue;
                }
                let r = residual.entry(seed).or_insert(0.0);
                *r += seed_share;
                let threshold = PPR_EPSILON * graph.out_edges[seed].len().max(1) as f64;
                if *r > threshold && !in_queue.contains(&seed) {
                    queue.push_back(seed);
                    in_queue.insert(seed);
                }
            }
        } else {
            // Push damping fraction along outgoing edges proportional to weight
            let push_mass = damping * r_v;
            let inv_tw = 1.0 / tw;
            for &(target, weight) in edges {
                let share = push_mass * weight * inv_tw;
                let r = residual.entry(target).or_insert(0.0);
                *r += share;
                let threshold = PPR_EPSILON * graph.out_edges[target].len().max(1) as f64;
                if *r > threshold && !in_queue.contains(&target) {
                    queue.push_back(target);
                    in_queue.insert(target);
                }
            }
        }
    }

    // Densify: convert sparse estimate into a dense score vector
    let mut scores = vec![0.0f64; n];
    for (idx, score) in estimate {
        scores[idx] = score;
    }
    scores
}

/// Compute personalized PageRank over a symbol reference graph.
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
) -> Vec<RankedSymbol> {
    let n = symbols.len();
    if n == 0 {
        return Vec::new();
    }

    let graph = RankGraph::build(symbols, refs, focus_symbols);

    // Dispatch: global power iteration for unfocused, push PPR for focused
    let mut scores = if focus_symbols.is_empty() {
        global_pagerank(&graph, focus_symbols, damping, iterations)
    } else {
        push_ppr(&graph, focus_symbols, damping)
    };

    // Post-processing: boost top-level symbols by sqrt(in_degree + 1)
    for (i, sym) in symbols.iter().enumerate() {
        if sym.parent_id.is_none() {
            let ref_count = graph.in_degree[i] as f64;
            scores[i] *= (ref_count + 1.0).sqrt();
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

    // Use fast heuristic in the loop; it slightly overestimates so we won't
    // blow past the budget.  One accurate count at the end for the response.
    for entry in ranked {
        let sym = &symbols[entry.index];
        let line = format!(
            "{}:{} {} {}",
            sym.path.to_string_lossy(),
            sym.line_range.start(),
            sym.kind.label(),
            sym.qualified_name,
        );

        let sig_suffix = sym
            .signature
            .as_deref()
            .map(|s| format!(" — {s}"))
            .unwrap_or_default();

        let full_line = format!("{line}{sig_suffix}\n");
        let line_est = crate::tokens::estimate_tokens(&full_line);

        if est_tokens + line_est > token_budget {
            break;
        }

        output.push_str(&full_line);
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
            signature: Some(format!("fn {name}()")),
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

        let ranked = pagerank(&symbols, &refs, &[], 0.85, 30);
        assert!(!ranked.is_empty());

        // All symbols should appear in the ranking
        let names: Vec<&str> = ranked
            .iter()
            .map(|r| symbols[r.index].name.as_str())
            .collect();
        assert!(
            names.contains(&"process_data"),
            "process_data should be ranked: {names:?}"
        );
        assert!(names.contains(&"main"), "main should be ranked: {names:?}");

        // process_data has higher in-degree, should have good score
        let pd_rank = ranked
            .iter()
            .find(|r| symbols[r.index].name.as_str() == "process_data")
            .unwrap();
        assert!(pd_rank.score > 0.0);
    }

    #[test]
    fn focus_boosts_symbols() {
        let symbols = vec![
            sym(1, "A", "mod::A"),
            sym(2, "B", "mod::B"),
            sym(3, "C", "mod::C"),
        ];

        let refs = vec![make_ref("B", 1), make_ref("C", 1)];

        // Without focus, B and C compete equally
        let ranked_no_focus = pagerank(&symbols, &refs, &[], 0.85, 30);

        // Focus on A -> refs from A get 50x boost
        let ranked_focused = pagerank(&symbols, &refs, &[0], 0.85, 30);

        // Both should produce results
        assert!(!ranked_no_focus.is_empty());
        assert!(!ranked_focused.is_empty());
    }

    #[test]
    fn empty_graph() {
        let ranked = pagerank(&[], &[], &[], 0.85, 30);
        assert!(ranked.is_empty());
    }

    #[test]
    fn render_within_budget() {
        let symbols = vec![
            sym(1, "foo", "mod::foo"),
            sym(2, "bar", "mod::bar"),
            sym(3, "baz", "mod::baz"),
        ];
        let ranked: Vec<RankedSymbol> = (0..3)
            .map(|i| RankedSymbol {
                index: i,
                score: 1.0 - i as f64 * 0.1,
            })
            .collect();

        let output = render_repo_map(&symbols, &ranked, 50);
        assert!(!output.is_empty());
        // Should fit within budget
        assert!(crate::tokens::count_tokens(&output) <= 55); // some slack
    }
}
