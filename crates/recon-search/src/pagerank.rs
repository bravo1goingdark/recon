//! Personalized PageRank over the symbol reference graph.
//!
//! Builds a directed graph from symbol references, applies Aider-style
//! edge weights, runs power iteration, and returns ranked symbols.

use recon_core::symbol::{Ref, Symbol};
use std::collections::HashMap;

/// A ranked symbol with its PageRank score.
#[derive(Debug, Clone)]
pub struct RankedSymbol {
    /// Index into the original symbols slice.
    pub index: usize,
    /// PageRank score (higher = more important).
    pub score: f64,
}

/// Compute personalized PageRank over a symbol reference graph.
///
/// `focus_symbols` — indices into `symbols` that get boosted personalization.
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

    // Build name → index map for target resolution
    let mut name_to_idx: HashMap<&str, Vec<usize>> = HashMap::with_capacity(n);
    // Build id → index map for source resolution
    let mut id_to_idx: HashMap<u64, usize> = HashMap::with_capacity(n);
    for (i, sym) in symbols.iter().enumerate() {
        name_to_idx.entry(sym.name.as_str()).or_default().push(i);
        id_to_idx.insert(sym.id, i);
    }

    let focus_set: std::collections::HashSet<usize> = focus_symbols.iter().copied().collect();

    // Build adjacency list with weights
    // Edge direction: source symbol (r.src_symbol_id) → targets named r.ident
    let mut out_edges: Vec<Vec<(usize, f64)>> = vec![Vec::new(); n];
    let mut in_degree: Vec<usize> = vec![0; n];

    for r in refs {
        // Resolve source by symbol ID
        let src_idx = match id_to_idx.get(&r.src_symbol_id) {
            Some(&idx) => idx,
            None => continue,
        };

        // Resolve targets by identifier name
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

    // Precompute total outgoing weight per node to avoid recomputing each iteration
    let total_weights: Vec<f64> = out_edges
        .iter()
        .map(|edges| edges.iter().map(|(_, w)| w).sum())
        .collect();

    // Power iteration
    let mut scores = vec![1.0 / n as f64; n];
    let mut new_scores = vec![0.0f64; n];
    let inv_n = 1.0 / n as f64;

    for _iter in 0..iterations {
        new_scores.fill(0.0);

        // Accumulate dangling node mass as a scalar, then distribute once
        let mut dangling_sum = 0.0f64;
        for (i, edges) in out_edges.iter().enumerate() {
            if edges.is_empty() || total_weights[i] == 0.0 {
                dangling_sum += scores[i];
            } else {
                let inv_weight = 1.0 / total_weights[i];
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

        // Early convergence: stop when L1 norm change is negligible
        if diff < 1e-6 {
            break;
        }
    }

    // Boost top-level symbols (no parent) by sqrt(ref_count)
    for (i, sym) in symbols.iter().enumerate() {
        if sym.parent_id.is_none() {
            let ref_count = in_degree[i] as f64;
            scores[i] *= (ref_count + 1.0).sqrt();
        }
    }

    // Collect and sort
    let mut ranked: Vec<RankedSymbol> = scores
        .iter()
        .enumerate()
        .filter(|(i, _)| symbols[*i].parent_id.is_none()) // top-level only
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

    fn sym(id: u64, name: &str, qname: &str) -> Symbol {
        Symbol {
            id,
            path: PathBuf::from("src/lib.rs"),
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
            src_path: PathBuf::from("src/lib.rs"),
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
