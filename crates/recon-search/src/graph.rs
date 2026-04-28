//! Reference-graph traversal primitives — shortest path, layered transitive
//! callers / callees.
//!
//! Builds a directed graph from `Symbol` + `Ref` slices in CSR layout (forward
//! and reverse adjacency), then exposes:
//!
//! - [`CallGraph::shortest_path`] — bidirectional BFS, returns an ordered chain
//!   of node indices from any `src` to any `dst`. Used by `code_path`.
//! - [`CallGraph::transitive_callers`] / [`CallGraph::transitive_callees`] —
//!   layered BFS bounded by depth, returns one tier per ring. Used by
//!   `code_callers` / `code_callees` and as the engine behind `code_impact`.
//!
//! All traversals enforce two universal caps to bound god-node fan-out:
//! [`GraphCaps::max_visited`] (total node-visit budget) and
//! [`GraphCaps::max_per_tier`] (per-ring fan-out cap, callers/callees only).
//! Both are signalled in the result via `truncated: true` so the caller can
//! warn the agent that more results were available.
//!
//! Edge resolution mirrors `pagerank::RankGraph::build`: a ref `(src, ident,
//! dst)` becomes a forward edge from `src` to every symbol whose `name`
//! matches `ident`. When `ident` resolves to *no* symbol, that ref counts
//! toward [`CallGraph::unresolved_out`] for the source — used by `code_path`
//! to report "path may exist via dyn dispatch / FFI: N unresolved near hop K".
//!
//! No weights are stored — BFS doesn't need them. Indices are `u32` for cache
//! locality (graphs ≤ 4B nodes).
//!
//! # Cycle safety
//!
//! Every traversal carries an `AHashSet<u32>` of already-visited nodes. A
//! node is recorded the first time it is reached and never re-expanded. The
//! cycle test in `tests::cycle_emits_each_node_once` proves this on
//! `a → b → a`.

use ahash::{AHashMap, AHashSet};
use recon_core::symbol::{Ref, Symbol};

/// Default per-tier fan-out cap for layered BFS.
///
/// Picked so that `code_callers --depth 3` on a god node returns at most
/// 50 × 3 = 150 nodes — a few KB of JSON, not a 5 MB blast.
pub const DEFAULT_MAX_PER_TIER: usize = 50;

/// Default total node-visit budget for any single traversal.
///
/// 50K is roughly the size of a small subsystem; god-node closures will
/// always exceed this. The cap is what makes traversal latency bounded
/// even in the worst case.
pub const DEFAULT_MAX_VISITED: usize = 50_000;

/// Default `code_path` hop limit. Practical paths in real codebases bottom
/// out well before 8 hops; longer "paths" are usually meaningless to an agent.
pub const DEFAULT_MAX_HOPS: u32 = 8;

/// Hard upper bound on `max_hops` arguments. Anything larger is rejected by
/// the tool layer rather than letting an agent depth-bomb the server.
pub const MAX_ALLOWED_HOPS: u32 = 16;

/// Hard upper bound on `depth` arguments for callers/callees.
pub const MAX_ALLOWED_DEPTH: u32 = 6;

/// Caps applied to a single graph traversal.
///
/// Pre-built via [`GraphCaps::default_for_path`] and
/// [`GraphCaps::default_for_callers`] so each call site picks a reasonable
/// preset; raw construction is possible for tests.
#[derive(Debug, Clone, Copy)]
pub struct GraphCaps {
    /// Maximum total nodes visited across the entire traversal. When hit,
    /// the traversal stops and `truncated: true` is reported.
    pub max_visited: usize,
    /// Maximum nodes reported per tier (callers/callees only — `shortest_path`
    /// stops at the first match).
    pub max_per_tier: usize,
    /// Maximum hops or depth.
    pub max_depth: u32,
}

impl GraphCaps {
    /// Caps tuned for `code_path`: depth 8, visit cap 50K. The per-tier cap
    /// is unused for shortest-path queries.
    pub fn default_for_path(max_hops: u32) -> Self {
        Self {
            max_visited: DEFAULT_MAX_VISITED,
            max_per_tier: DEFAULT_MAX_PER_TIER,
            max_depth: max_hops.min(MAX_ALLOWED_HOPS),
        }
    }

    /// Caps tuned for `code_callers` / `code_callees`: depth from the user
    /// (default 1), per-tier cap 50, visit cap 50K.
    pub fn default_for_callers(depth: u32) -> Self {
        Self {
            max_visited: DEFAULT_MAX_VISITED,
            max_per_tier: DEFAULT_MAX_PER_TIER,
            max_depth: depth.min(MAX_ALLOWED_DEPTH),
        }
    }
}

/// Outcome of [`CallGraph::shortest_path`].
#[derive(Debug, Clone)]
pub enum ShortestPathResult {
    /// A path was found. Indices are ordered hop-sequence: `path[0]` is one
    /// of the requested sources, `path[last]` is one of the requested dests.
    /// `path.len() == 1` means src and dst are the same node.
    Found {
        /// Ordered chain of node indices.
        path: Vec<u32>,
    },
    /// No path exists within `max_hops`. `unresolved_near` reports the index
    /// of the deepest reached node that has unresolved out-refs (likely
    /// dynamic dispatch, FFI, or external functions) — the canonical
    /// "path may exist via dyn dispatch" signal for the agent.
    Unreachable {
        /// Optional hint of the deepest unresolved boundary, if any.
        unresolved_near: Option<u32>,
    },
    /// Traversal hit [`GraphCaps::max_visited`] before finding a path.
    /// The graph is too dense to answer at the configured cap; the agent
    /// should narrow the source/destination or accept the partial result.
    VisitCapHit,
}

/// One ring of a layered traversal.
#[derive(Debug, Clone)]
pub struct TraversalTier {
    /// Hops from the seed set (1 = direct callers/callees, 2 = next ring, ...).
    pub depth: u32,
    /// Node indices in this tier. Order is BFS-order; ties broken by index.
    pub nodes: Vec<u32>,
    /// True if this tier was capped at `max_per_tier`. The omitted neighbors
    /// are still counted by [`TraversalResult::truncated`].
    pub truncated_at_cap: bool,
}

/// Outcome of [`CallGraph::transitive_callers`] / [`CallGraph::transitive_callees`].
#[derive(Debug, Clone)]
pub struct TraversalResult {
    /// One entry per depth, starting at depth 1. Depth 0 (the seed set) is
    /// not included.
    pub tiers: Vec<TraversalTier>,
    /// True if any tier was truncated or [`GraphCaps::max_visited`] was hit.
    pub truncated: bool,
}

/// Directed reference graph in dual-CSR layout.
///
/// Forward CSR (`out_*`) lists out-neighbors per node; reverse CSR (`in_*`)
/// lists in-neighbors. Both are built in one pass over the refs — total
/// memory is `~2 * 4 * n_edges` bytes plus `O(n)` offset arrays.
pub struct CallGraph {
    /// Number of nodes (== input `symbols.len()`).
    pub n: usize,
    /// Forward CSR offsets — `out_offsets[i]..out_offsets[i+1]` slices into `out_edges`.
    out_offsets: Vec<u32>,
    /// Forward CSR edges — out-neighbor indices, contiguous per source.
    out_edges: Vec<u32>,
    /// Reverse CSR offsets — `in_offsets[i]..in_offsets[i+1]` slices into `in_edges`.
    in_offsets: Vec<u32>,
    /// Reverse CSR edges — in-neighbor indices, contiguous per destination.
    in_edges: Vec<u32>,
    /// Count of refs originating from each source whose `ident` did not match
    /// any indexed symbol. These are the "unresolved" refs — likely dyn
    /// dispatch, FFI, intrinsics, or external functions.
    unresolved_out: Vec<u32>,
}

impl CallGraph {
    /// Build the call graph from indexed symbols and refs.
    ///
    /// Mirrors the edge-resolution logic of `pagerank::RankGraph::build`:
    /// each `Ref` resolves its `ident` to all symbols with that `name`
    /// (one-to-many for duplicate names) and emits a forward edge to each.
    /// Self-loops are dropped. Refs whose `src_symbol_id` is unknown are
    /// dropped silently. Refs whose `ident` matches no symbol increment
    /// the source's `unresolved_out` counter.
    pub fn build(symbols: &[Symbol], refs: &[Ref]) -> Self {
        let n = symbols.len();

        let mut id_to_idx: AHashMap<u64, u32> = AHashMap::with_capacity(n);
        let mut name_to_idx: AHashMap<&str, smallvec::SmallVec<[u32; 4]>> =
            AHashMap::with_capacity(n);
        for (i, sym) in symbols.iter().enumerate() {
            let idx = i as u32;
            id_to_idx.insert(sym.id, idx);
            name_to_idx.entry(sym.name.as_str()).or_default().push(idx);
        }

        // Phase 1: count out-edges and in-edges per node.
        let mut out_counts = vec![0u32; n];
        let mut in_counts = vec![0u32; n];
        let mut unresolved_out = vec![0u32; n];
        let mut total_edges: usize = 0;

        for r in refs {
            let src_idx = match id_to_idx.get(&r.src_symbol_id) {
                Some(&i) => i,
                None => continue,
            };
            let targets = match name_to_idx.get(r.ident.as_str()) {
                Some(t) => t,
                None => {
                    unresolved_out[src_idx as usize] =
                        unresolved_out[src_idx as usize].saturating_add(1);
                    continue;
                }
            };
            for &dst_idx in targets {
                if dst_idx != src_idx {
                    out_counts[src_idx as usize] = out_counts[src_idx as usize].saturating_add(1);
                    in_counts[dst_idx as usize] = in_counts[dst_idx as usize].saturating_add(1);
                    total_edges += 1;
                }
            }
        }

        // Phase 2: prefix-sum into offsets.
        let mut out_offsets = Vec::with_capacity(n + 1);
        let mut in_offsets = Vec::with_capacity(n + 1);
        out_offsets.push(0u32);
        in_offsets.push(0u32);
        let mut running_out: u32 = 0;
        let mut running_in: u32 = 0;
        for i in 0..n {
            running_out = running_out.saturating_add(out_counts[i]);
            running_in = running_in.saturating_add(in_counts[i]);
            out_offsets.push(running_out);
            in_offsets.push(running_in);
        }

        // Phase 3: fill edges using cursor arrays.
        let mut out_edges = vec![0u32; total_edges];
        let mut in_edges = vec![0u32; total_edges];
        let mut out_cursors = vec![0u32; n];
        let mut in_cursors = vec![0u32; n];
        for r in refs {
            let src_idx = match id_to_idx.get(&r.src_symbol_id) {
                Some(&i) => i,
                None => continue,
            };
            let targets = match name_to_idx.get(r.ident.as_str()) {
                Some(t) => t,
                None => continue,
            };
            for &dst_idx in targets {
                if dst_idx != src_idx {
                    let op =
                        (out_offsets[src_idx as usize] + out_cursors[src_idx as usize]) as usize;
                    out_edges[op] = dst_idx;
                    out_cursors[src_idx as usize] += 1;

                    let ip = (in_offsets[dst_idx as usize] + in_cursors[dst_idx as usize]) as usize;
                    in_edges[ip] = src_idx;
                    in_cursors[dst_idx as usize] += 1;
                }
            }
        }

        Self {
            n,
            out_offsets,
            out_edges,
            in_offsets,
            in_edges,
            unresolved_out,
        }
    }

    /// Out-neighbors of node `i` (callees).
    #[inline]
    pub fn out_neighbors(&self, i: u32) -> &[u32] {
        let start = self.out_offsets[i as usize] as usize;
        let end = self.out_offsets[i as usize + 1] as usize;
        &self.out_edges[start..end]
    }

    /// In-neighbors of node `i` (callers).
    #[inline]
    pub fn in_neighbors(&self, i: u32) -> &[u32] {
        let start = self.in_offsets[i as usize] as usize;
        let end = self.in_offsets[i as usize + 1] as usize;
        &self.in_edges[start..end]
    }

    /// Number of unresolved out-refs from this node — `ident` matched no
    /// indexed symbol. Use this to flag dyn-dispatch / FFI gaps to the agent.
    #[inline]
    pub fn unresolved_out(&self, i: u32) -> u32 {
        self.unresolved_out[i as usize]
    }

    /// Find the shortest path from any node in `srcs` to any node in `dsts`.
    ///
    /// Uses bidirectional BFS — alternate expanding the smaller frontier
    /// from the source side and the destination side, stopping the moment
    /// the two frontiers intersect. Reduces the worst-case node visits
    /// from `O(b^d)` (one-sided) to `O(b^(d/2) + b^(d/2))` for branching
    /// factor `b` and path length `d`.
    ///
    /// Returns:
    /// - [`ShortestPathResult::Found`] with the ordered chain (src→...→dst).
    /// - [`ShortestPathResult::Unreachable`] if no path within `max_depth`,
    ///   with `unresolved_near` pointing at the deepest reached source-side
    ///   frontier node that has unresolved out-refs (best-effort
    ///   "via dyn dispatch" hint).
    /// - [`ShortestPathResult::VisitCapHit`] if combined visited-set size
    ///   exceeds `caps.max_visited`.
    pub fn shortest_path(
        &self,
        srcs: &[u32],
        dsts: &[u32],
        caps: &GraphCaps,
    ) -> ShortestPathResult {
        if srcs.is_empty() || dsts.is_empty() {
            return ShortestPathResult::Unreachable {
                unresolved_near: None,
            };
        }

        // Source/destination overlap → trivial path of length 0.
        let dst_set: AHashSet<u32> = dsts.iter().copied().collect();
        for &s in srcs {
            if dst_set.contains(&s) {
                return ShortestPathResult::Found { path: vec![s] };
            }
        }

        // Forward BFS state: visited[node] = predecessor on the shortest
        // src→node path. Roots (nodes in `srcs`) map to themselves.
        // Same shape on the reverse side, but `predecessor` is "successor on
        // the dst-bound continuation."
        let mut fwd_pred: AHashMap<u32, u32> = AHashMap::with_capacity(64);
        let mut bwd_succ: AHashMap<u32, u32> = AHashMap::with_capacity(64);

        let mut fwd_frontier: Vec<u32> = srcs.to_vec();
        let mut bwd_frontier: Vec<u32> = dsts.to_vec();
        for &s in srcs {
            fwd_pred.insert(s, s);
        }
        for &d in dsts {
            bwd_succ.insert(d, d);
        }

        let mut total_visited: usize = fwd_pred.len() + bwd_succ.len();
        let mut deepest_unresolved: Option<u32> = None;
        let mut depth: u32 = 0;

        while !fwd_frontier.is_empty() && !bwd_frontier.is_empty() {
            if depth >= caps.max_depth {
                return ShortestPathResult::Unreachable {
                    unresolved_near: deepest_unresolved,
                };
            }

            // Expand whichever frontier is smaller — keeps total work tight.
            let expand_forward = fwd_frontier.len() <= bwd_frontier.len();
            let mut next: Vec<u32> = Vec::with_capacity(fwd_frontier.len() * 4);

            if expand_forward {
                for &u in &fwd_frontier {
                    if self.unresolved_out(u) > 0 {
                        deepest_unresolved = Some(u);
                    }
                    for &v in self.out_neighbors(u) {
                        if !fwd_pred.contains_key(&v) {
                            fwd_pred.insert(v, u);
                            total_visited += 1;
                            if total_visited > caps.max_visited {
                                return ShortestPathResult::VisitCapHit;
                            }
                            // Meet-in-the-middle check
                            if bwd_succ.contains_key(&v) {
                                return ShortestPathResult::Found {
                                    path: reconstruct_path(&fwd_pred, &bwd_succ, v),
                                };
                            }
                            next.push(v);
                        }
                    }
                }
                fwd_frontier = next;
            } else {
                for &u in &bwd_frontier {
                    for &p in self.in_neighbors(u) {
                        if !bwd_succ.contains_key(&p) {
                            bwd_succ.insert(p, u);
                            total_visited += 1;
                            if total_visited > caps.max_visited {
                                return ShortestPathResult::VisitCapHit;
                            }
                            if fwd_pred.contains_key(&p) {
                                return ShortestPathResult::Found {
                                    path: reconstruct_path(&fwd_pred, &bwd_succ, p),
                                };
                            }
                            next.push(p);
                        }
                    }
                }
                bwd_frontier = next;
            }

            depth += 1;
        }

        ShortestPathResult::Unreachable {
            unresolved_near: deepest_unresolved,
        }
    }

    /// Layered BFS over **reverse** edges — returns transitive callers.
    ///
    /// `seeds` is the set of starting nodes (the "callees"). Tier 1 contains
    /// their direct callers, tier 2 the callers of those, etc., up to
    /// `caps.max_depth`. A node visited at tier `k` is never re-emitted at
    /// tier `k+1` (cycle-safe).
    pub fn transitive_callers(&self, seeds: &[u32], caps: &GraphCaps) -> TraversalResult {
        self.layered_bfs(seeds, caps, /* reverse */ true)
    }

    /// Layered BFS over **forward** edges — returns transitive callees.
    pub fn transitive_callees(&self, seeds: &[u32], caps: &GraphCaps) -> TraversalResult {
        self.layered_bfs(seeds, caps, /* reverse */ false)
    }

    /// Compute weakly-connected components over the undirected graph
    /// (forward ∪ reverse edges). Returns a `Vec<u32>` where index `i`
    /// holds the component-id of node `i`. Component ids are dense
    /// (`0..k` for `k` components), assigned by union-find iteration order.
    ///
    /// This is the simple-and-fast substitute for Leiden community
    /// detection in Phase 2: it doesn't optimize modularity, but it does
    /// cleanly separate disconnected subsystems (e.g. `crates/recon-search`
    /// vs `crates/recon-storage`) and runs in `O((V+E) α(V))`.
    pub fn connected_components(&self) -> Vec<u32> {
        let n = self.n;
        let mut parent: Vec<u32> = (0..n as u32).collect();
        let mut rank: Vec<u8> = vec![0; n];
        // Path-compressed find.
        fn find(parent: &mut [u32], mut x: u32) -> u32 {
            while parent[x as usize] != x {
                let p = parent[x as usize];
                let pp = parent[p as usize];
                parent[x as usize] = pp;
                x = pp;
            }
            x
        }
        let union = |parent: &mut Vec<u32>, rank: &mut Vec<u8>, a: u32, b: u32| {
            let ra = find(parent, a);
            let rb = find(parent, b);
            if ra == rb {
                return;
            }
            match rank[ra as usize].cmp(&rank[rb as usize]) {
                std::cmp::Ordering::Less => parent[ra as usize] = rb,
                std::cmp::Ordering::Greater => parent[rb as usize] = ra,
                std::cmp::Ordering::Equal => {
                    parent[rb as usize] = ra;
                    rank[ra as usize] = rank[ra as usize].saturating_add(1);
                }
            }
        };

        // Union along every directed edge — this gives weakly-connected components.
        for src in 0..n as u32 {
            for &dst in self.out_neighbors(src) {
                union(&mut parent, &mut rank, src, dst);
            }
        }

        // Re-root each node to its representative, then densify ids.
        let mut roots: Vec<u32> = (0..n as u32).map(|i| find(&mut parent, i)).collect();
        let mut id_of_root: AHashMap<u32, u32> = AHashMap::with_capacity(n / 16 + 1);
        let mut next: u32 = 0;
        for r in roots.iter_mut() {
            let id = match id_of_root.get(r) {
                Some(&id) => id,
                None => {
                    let id = next;
                    id_of_root.insert(*r, id);
                    next += 1;
                    id
                }
            };
            *r = id;
        }
        roots
    }

    /// Per-node out-degree (number of resolved out-edges, excluding
    /// unresolved refs).
    #[inline]
    pub fn out_degree(&self, i: u32) -> u32 {
        self.out_neighbors(i).len() as u32
    }

    /// Per-node in-degree (number of incoming resolved edges).
    #[inline]
    pub fn in_degree(&self, i: u32) -> u32 {
        self.in_neighbors(i).len() as u32
    }

    fn layered_bfs(&self, seeds: &[u32], caps: &GraphCaps, reverse: bool) -> TraversalResult {
        let mut visited: AHashSet<u32> = AHashSet::with_capacity(64);
        for &s in seeds {
            visited.insert(s);
        }
        let mut frontier: Vec<u32> = seeds.to_vec();
        let mut tiers: Vec<TraversalTier> = Vec::with_capacity(caps.max_depth as usize);
        let mut truncated = false;

        for d in 1..=caps.max_depth {
            let mut next: Vec<u32> = Vec::with_capacity(frontier.len() * 4);
            // Emission ordering: BFS visit order, deduped on the fly.
            let mut seen_in_tier: AHashSet<u32> = AHashSet::with_capacity(frontier.len() * 4);
            for &u in &frontier {
                let neighbors = if reverse {
                    self.in_neighbors(u)
                } else {
                    self.out_neighbors(u)
                };
                for &v in neighbors {
                    if visited.insert(v) {
                        if visited.len() > caps.max_visited {
                            // Capacity overflow — keep what we have and stop.
                            truncated = true;
                            break;
                        }
                        if seen_in_tier.insert(v) {
                            next.push(v);
                        }
                    }
                }
                if truncated {
                    break;
                }
            }

            if next.is_empty() {
                break;
            }

            // Per-tier fan-out cap. Order is deterministic (BFS-order, ties by
            // insertion); higher tiers still see callers of the *capped*
            // selection only — that's the documented semantics.
            let truncated_at_cap = next.len() > caps.max_per_tier;
            if truncated_at_cap {
                next.truncate(caps.max_per_tier);
                truncated = true;
            }

            tiers.push(TraversalTier {
                depth: d,
                nodes: next.clone(),
                truncated_at_cap,
            });

            frontier = next;
            if truncated {
                break;
            }
        }

        TraversalResult { tiers, truncated }
    }
}

/// Reconstruct the full src→...→dst chain after bidirectional BFS meets at `meet`.
///
/// `fwd_pred[v] = u` means `u → v` on a shortest src-rooted path.
/// `bwd_succ[v] = u` means `v → u` on a shortest dst-rooted continuation.
/// Walk the predecessor chain backward from `meet` to a root, reverse it,
/// then walk the successor chain forward from `meet` to a destination.
fn reconstruct_path(
    fwd_pred: &AHashMap<u32, u32>,
    bwd_succ: &AHashMap<u32, u32>,
    meet: u32,
) -> Vec<u32> {
    let mut prefix: Vec<u32> = Vec::with_capacity(8);
    let mut cur = meet;
    loop {
        prefix.push(cur);
        let p = match fwd_pred.get(&cur) {
            Some(&p) => p,
            None => break,
        };
        if p == cur {
            break;
        }
        cur = p;
    }
    prefix.reverse();

    // suffix walks dst-bound, *excluding* `meet` (already in prefix).
    let mut cur = meet;
    while let Some(&n) = bwd_succ.get(&cur) {
        if n == cur {
            break;
        }
        prefix.push(n);
        cur = n;
    }
    prefix
}

#[cfg(test)]
mod tests {
    use super::*;
    use compact_str::CompactString;
    use recon_core::lang::Language;
    use recon_core::symbol::SymbolKind;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn sym(id: u64, name: &str) -> Symbol {
        Symbol {
            id,
            path: Arc::new(PathBuf::from("src/lib.rs")),
            name: CompactString::new(name),
            qualified_name: CompactString::new(format!("crate::{name}")),
            kind: SymbolKind::Function,
            signature: None,
            doc: None,
            parent_id: None,
            byte_range: 0..1,
            line_range: 1..=1,
            body_hash: [0u8; 32],
            lang: Language::Rust,
        }
    }

    fn r(src_id: u64, ident: &str) -> Ref {
        Ref {
            src_path: Arc::new(PathBuf::from("src/lib.rs")),
            src_symbol_id: src_id,
            ident: CompactString::new(ident),
            dst_symbol_id: None,
            weight: 1.0,
        }
    }

    /// Linear chain `a → b → c → d`.
    fn linear() -> (Vec<Symbol>, Vec<Ref>) {
        let symbols = vec![sym(1, "a"), sym(2, "b"), sym(3, "c"), sym(4, "d")];
        let refs = vec![r(1, "b"), r(2, "c"), r(3, "d")];
        (symbols, refs)
    }

    #[test]
    fn empty_graph() {
        let g = CallGraph::build(&[], &[]);
        assert_eq!(g.n, 0);
        let r = g.shortest_path(&[], &[], &GraphCaps::default_for_path(8));
        assert!(matches!(r, ShortestPathResult::Unreachable { .. }));
    }

    #[test]
    fn linear_path_found() {
        let (symbols, refs) = linear();
        let g = CallGraph::build(&symbols, &refs);
        let res = g.shortest_path(&[0], &[3], &GraphCaps::default_for_path(8));
        match res {
            ShortestPathResult::Found { path } => assert_eq!(path, vec![0, 1, 2, 3]),
            other => panic!("expected Found, got {other:?}"),
        }
    }

    #[test]
    fn src_equals_dst_returns_singleton() {
        let (symbols, refs) = linear();
        let g = CallGraph::build(&symbols, &refs);
        let res = g.shortest_path(&[2], &[2], &GraphCaps::default_for_path(8));
        match res {
            ShortestPathResult::Found { path } => assert_eq!(path, vec![2]),
            other => panic!("expected singleton path, got {other:?}"),
        }
    }

    #[test]
    fn disconnected_unreachable() {
        // a → b ; c → d (two disconnected components)
        let symbols = vec![sym(1, "a"), sym(2, "b"), sym(3, "c"), sym(4, "d")];
        let refs = vec![r(1, "b"), r(3, "d")];
        let g = CallGraph::build(&symbols, &refs);
        let res = g.shortest_path(&[0], &[3], &GraphCaps::default_for_path(8));
        assert!(matches!(res, ShortestPathResult::Unreachable { .. }));
    }

    #[test]
    fn diamond_picks_shortest() {
        // a → b → d, a → c → d. Both paths are length 3, either is acceptable.
        let symbols = vec![sym(1, "a"), sym(2, "b"), sym(3, "c"), sym(4, "d")];
        let refs = vec![r(1, "b"), r(1, "c"), r(2, "d"), r(3, "d")];
        let g = CallGraph::build(&symbols, &refs);
        let res = g.shortest_path(&[0], &[3], &GraphCaps::default_for_path(8));
        match res {
            ShortestPathResult::Found { path } => {
                assert_eq!(path.first().copied(), Some(0));
                assert_eq!(path.last().copied(), Some(3));
                assert_eq!(path.len(), 3, "expected 3-hop path: {path:?}");
            }
            other => panic!("expected Found, got {other:?}"),
        }
    }

    #[test]
    fn cycle_emits_each_node_once() {
        // a → b → a (2-cycle).
        let symbols = vec![sym(1, "a"), sym(2, "b")];
        let refs = vec![r(1, "b"), r(2, "a")];
        let g = CallGraph::build(&symbols, &refs);
        let caps = GraphCaps::default_for_callers(3);
        // From `a`, callees with depth 3 must terminate (cycle-safe).
        let res = g.transitive_callees(&[0], &caps);
        // Tier 1: just b. Tier 2: would be a but a is already visited.
        assert_eq!(
            res.tiers.len(),
            1,
            "cycle should emit one tier: {:?}",
            res.tiers
        );
        assert_eq!(res.tiers[0].nodes, vec![1]);
    }

    #[test]
    fn callers_layered_depth_2() {
        // a → b → c → d (linear). Callers of d at depth 2 are c (tier1) and b (tier2).
        let (symbols, refs) = linear();
        let g = CallGraph::build(&symbols, &refs);
        let caps = GraphCaps::default_for_callers(2);
        let res = g.transitive_callers(&[3], &caps);
        assert_eq!(res.tiers.len(), 2);
        assert_eq!(res.tiers[0].depth, 1);
        assert_eq!(res.tiers[0].nodes, vec![2]);
        assert_eq!(res.tiers[1].depth, 2);
        assert_eq!(res.tiers[1].nodes, vec![1]);
    }

    #[test]
    fn callees_layered_depth_2() {
        // a → b → c → d. Callees of a at depth 2: b, c.
        let (symbols, refs) = linear();
        let g = CallGraph::build(&symbols, &refs);
        let caps = GraphCaps::default_for_callers(2);
        let res = g.transitive_callees(&[0], &caps);
        assert_eq!(res.tiers.len(), 2);
        assert_eq!(res.tiers[0].nodes, vec![1]);
        assert_eq!(res.tiers[1].nodes, vec![2]);
    }

    #[test]
    fn unresolved_ref_counted() {
        // a refs `external_fn` which is not indexed.
        let symbols = vec![sym(1, "a")];
        let refs = vec![r(1, "external_fn")];
        let g = CallGraph::build(&symbols, &refs);
        assert_eq!(g.unresolved_out(0), 1);
    }

    #[test]
    fn per_tier_cap_truncates() {
        // Hub `h` has 100 callers; depth-1 tier should be capped at max_per_tier.
        let mut symbols = vec![sym(1000, "h")];
        let mut refs = Vec::new();
        for i in 0..100 {
            symbols.push(sym(i as u64 + 1, &format!("c{i}")));
            refs.push(r(i as u64 + 1, "h"));
        }
        let g = CallGraph::build(&symbols, &refs);
        let caps = GraphCaps {
            max_visited: DEFAULT_MAX_VISITED,
            max_per_tier: 10,
            max_depth: 1,
        };
        let res = g.transitive_callers(&[0], &caps);
        assert_eq!(res.tiers.len(), 1);
        assert_eq!(res.tiers[0].nodes.len(), 10);
        assert!(res.tiers[0].truncated_at_cap);
        assert!(res.truncated);
    }

    #[test]
    fn visit_cap_hit() {
        // 1000-node ring, visit cap small.
        let symbols: Vec<Symbol> = (0..1000)
            .map(|i| sym(i as u64 + 1, &format!("n{i}")))
            .collect();
        let refs: Vec<Ref> = (0..1000)
            .map(|i| r(i as u64 + 1, &format!("n{}", (i + 1) % 1000)))
            .collect();
        let g = CallGraph::build(&symbols, &refs);
        let caps = GraphCaps {
            max_visited: 50,
            max_per_tier: DEFAULT_MAX_PER_TIER,
            max_depth: 1000,
        };
        // Path from n0 to n999 across a 1000-node ring, but visit cap is 50.
        let res = g.shortest_path(&[0], &[999], &caps);
        // Should EITHER be Found (visited <50 if ring direction is right) OR VisitCapHit.
        match res {
            ShortestPathResult::Found { .. } | ShortestPathResult::VisitCapHit => {}
            ShortestPathResult::Unreachable { .. } => {
                panic!("ring is connected; should not be Unreachable")
            }
        }
    }

    #[test]
    fn name_collision_finds_path_through_any_match() {
        // Two `foo` symbols: id=1 and id=2. `caller` refs `foo` (ambiguous).
        // Path from caller to id=1's foo should still be found via the
        // additive-edges semantics.
        let symbols = vec![sym(1, "foo"), sym(2, "foo"), sym(3, "caller")];
        let refs = vec![r(3, "foo")]; // caller → both foos
        let g = CallGraph::build(&symbols, &refs);
        let res1 = g.shortest_path(&[2], &[0], &GraphCaps::default_for_path(8));
        let res2 = g.shortest_path(&[2], &[1], &GraphCaps::default_for_path(8));
        assert!(matches!(res1, ShortestPathResult::Found { .. }));
        assert!(matches!(res2, ShortestPathResult::Found { .. }));
    }

    #[test]
    fn self_loop_filtered_at_build() {
        // Symbol `a` refs itself by name — should not appear in adjacency.
        let symbols = vec![sym(1, "a")];
        let refs = vec![r(1, "a")];
        let g = CallGraph::build(&symbols, &refs);
        assert_eq!(g.out_neighbors(0).len(), 0);
        assert_eq!(g.in_neighbors(0).len(), 0);
    }

    #[test]
    fn connected_components_groups_disjoint_clusters() {
        // Two disjoint chains: a→b, c→d.
        let symbols = vec![sym(1, "a"), sym(2, "b"), sym(3, "c"), sym(4, "d")];
        let refs = vec![r(1, "b"), r(3, "d")];
        let g = CallGraph::build(&symbols, &refs);
        let comps = g.connected_components();
        assert_eq!(comps.len(), 4);
        assert_eq!(comps[0], comps[1], "a and b should share a component");
        assert_eq!(comps[2], comps[3], "c and d should share a component");
        assert_ne!(comps[0], comps[2], "the two chains are disjoint");
    }

    #[test]
    fn connected_components_singletons() {
        let symbols = vec![sym(1, "a"), sym(2, "b"), sym(3, "c")];
        let refs: Vec<Ref> = vec![]; // No edges
        let g = CallGraph::build(&symbols, &refs);
        let comps = g.connected_components();
        // 3 distinct components, ids 0..3 (in some order)
        let mut sorted: Vec<u32> = comps.to_vec();
        sorted.sort();
        assert_eq!(sorted, vec![0, 1, 2]);
    }

    #[test]
    fn degree_counts() {
        let (symbols, refs) = linear();
        let g = CallGraph::build(&symbols, &refs);
        // a→b→c→d
        assert_eq!(g.out_degree(0), 1);
        assert_eq!(g.in_degree(0), 0);
        assert_eq!(g.out_degree(3), 0);
        assert_eq!(g.in_degree(3), 1);
        assert_eq!(g.out_degree(1), 1);
        assert_eq!(g.in_degree(1), 1);
    }
}
