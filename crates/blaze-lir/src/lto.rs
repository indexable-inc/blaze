//! Cross-function bottom-up ThinLTO & JIT tier-up planning over LIR modules.
//!
//! Given a module of `(BodyId, Func)` bodies and a PGO profile ([`crate::pgo::PgoSiteTable`] or
//! any [`SiteWeights`]):
//! 1. Builds the intra-module direct call graph in CSR form and condenses it with deterministic
//!    **iterative Tarjan SCC** into reverse topological (bottom-up) order so every callee SCC is
//!    optimized before its callers.
//! 2. For each SCC, runs pre-inline intra-procedural simplification
//!    (`scalar_replace -> propagate -> dead`), computes and caches [`FnSummary`], runs PGO-guided
//!    priority inlining ([`crate::inline::inline_guided_with_cache`]), and when sites were spliced
//!    runs a post-inline `scalar_replace -> propagate -> dead` pass so `SlotAddr` arguments and
//!    constant parameters fold across function boundaries.
//! 3. Produces [`ModuleLto`] carrying updated [`FnSummary`] digests (for ThinLTO early cutoff)
//!    and [`advise_jit_tier_up`] candidates ranked for atomic live patching (`blaze-inproc`).
//!
//! Pure sans-IO, `#![forbid(unsafe_code)]`, explicit allocator only.

use alloc::vec::Vec;
use core::alloc::Allocator;

use blaze_hash::{FixedState, Map, MapOpts};
use blaze_own::{Alloc, SharedAlloc, Vec as OwnVec};

use crate::{
    Op, View,
    body::{Bodies, BodyId, Datum, DatumId, Target},
    func::{Func, RefId},
    inline::{ImportedRef, InlineError, InlinePolicy, SiteWeights, inline_guided_with_base_cache, inline_guided_with_cache},
    pgo::PgoSiteTable,
    simplify::{
        RegBudget, SimplifyError, SuperoptOpts, auto_vectorize_loops, dead, eliminate_redundant_checks, if_convert,
        memopt::scalarize_small_memops, optimize_memory, propagate, scalar_replace, schedule_dag, slp_vectorize,
        sota_optimize_fixpoint, speculative_devirt, superopt_a64, tail_duplicate_dispatch, thread_jumps,
        unroll_scalar_loops, unswitch_loops, vectorize_memchr_loops, IndirectTarget,
    },
    summary::FnSummary,
};

/// One function body supplied to [`lto_optimize_module`].
pub struct ModuleBody<A: Allocator> {
    pub id: BodyId,
    pub func: Func<A>,
}

/// One optimized function body returned by [`lto_optimize_module`].
pub struct OptimizedBody<A: Allocator> {
    pub id: BodyId,
    pub func: Func<A>,
    /// Summary computed before LTO optimization (for incremental early-cutoff comparison).
    pub prev_summary: FnSummary,
    /// Summary computed after LTO optimization.
    pub summary: FnSummary,
    /// Callee bodies spliced into `func`, sorted and deduplicated.
    pub read: Vec<BodyId, A>,
    /// External references imported from spliced callees into `func.refs`.
    pub imported_refs: Vec<ImportedRef, A>,
    /// True when `summary.summary_hash != prev_summary.summary_hash`.
    pub summary_changed: bool,
}

/// Output of [`lto_optimize_module`].
pub struct ModuleLto<A: Allocator> {
    /// Optimized bodies in the same index order as the input slice.
    pub bodies: OwnVec<OptimizedBody<A>, A>,
    /// Number of strongly connected components discovered in the intra-module call graph.
    pub scc_count: usize,
    /// Total number of callee bodies spliced across all functions in the module.
    pub total_inlined_bodies: usize,
}

/// One hot body recommended for JIT Tier-1 optimization and live atomic trampoline patching
/// (`blaze-inproc::JitPool::retarget`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TierUpCandidate {
    pub body: BodyId,
    pub weight: u64,
    pub insts: u32,
    pub has_loops: bool,
    pub priority: u64,
    pub summary_hash: [u8; 32],
}

/// Compressed strongly connected components in reverse topological (callees-first) order.
pub struct SccOrder<A: Allocator> {
    /// Flat node indices (`0..n`), grouped by SCC.
    pub nodes: OwnVec<u32, A>,
    /// CSR offsets into `nodes`: SCC `k` is `nodes[offsets[k]..offsets[k + 1]]`.
    pub offsets: OwnVec<u32, A>,
}

impl<A: Allocator> SccOrder<A> {
    #[inline]
    pub fn len(&self) -> usize {
        self.offsets.len().saturating_sub(1)
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[inline]
    pub fn component(&self, k: usize) -> &[u32] {
        let start = self.offsets[k] as usize;
        let end = self.offsets[k + 1] as usize;
        &self.nodes[start..end]
    }
}

/// Deterministic iterative Tarjan SCC decomposition over CSR adjacency lists (`adj_offsets`, `adj`).
///
/// Emits components in **reverse topological order** (sink/leaf SCCs first, root callers last),
/// with nodes inside each SCC sorted ascending by node index.
pub fn tarjan_sccs<A: Allocator + Copy>(
    node_count: usize,
    adj_offsets: &[u32],
    adj: &[u32],
    alloc: A,
) -> SccOrder<A> {
    const UNVISITED: u32 = u32::MAX;
    let mut index = OwnVec::with_capacity_in(node_count, alloc);
    index.resize(node_count, UNVISITED);
    let mut lowlink = OwnVec::with_capacity_in(node_count, alloc);
    lowlink.resize(node_count, 0u32);
    let mut on_stack = OwnVec::with_capacity_in(node_count, alloc);
    on_stack.resize(node_count, false);

    let mut tarjan_stack: OwnVec<u32, A> = OwnVec::with_capacity_in(node_count, alloc);
    struct Frame {
        v: u32,
        edge_pos: u32,
    }
    let mut dfs: OwnVec<Frame, A> = OwnVec::with_capacity_in(node_count, alloc);

    let mut out_nodes: OwnVec<u32, A> = OwnVec::with_capacity_in(node_count, alloc);
    let mut out_offsets: OwnVec<u32, A> = OwnVec::with_capacity_in(node_count + 1, alloc);
    out_offsets.push(0);

    let mut next_index = 0u32;

    for root in 0..node_count {
        if index[root] != UNVISITED {
            continue;
        }
        let r = root as u32;
        index[root] = next_index;
        lowlink[root] = next_index;
        next_index += 1;
        tarjan_stack.push(r);
        on_stack[root] = true;
        dfs.push(Frame { v: r, edge_pos: adj_offsets[root] });

        while let Some(top) = dfs.last_mut() {
            let v = top.v as usize;
            let end = adj_offsets[v + 1];
            if top.edge_pos < end {
                let w = adj[top.edge_pos as usize] as usize;
                top.edge_pos += 1;
                if index[w] == UNVISITED {
                    index[w] = next_index;
                    lowlink[w] = next_index;
                    next_index += 1;
                    tarjan_stack.push(w as u32);
                    on_stack[w] = true;
                    dfs.push(Frame { v: w as u32, edge_pos: adj_offsets[w] });
                } else if on_stack[w] && index[w] < lowlink[v] {
                    lowlink[v] = index[w];
                }
            } else {
                let finished_v = top.v;
                let finished_low = lowlink[v];
                dfs.pop();
                if let Some(parent) = dfs.last() {
                    let pv = parent.v as usize;
                    if finished_low < lowlink[pv] {
                        lowlink[pv] = finished_low;
                    }
                }
                if finished_low == index[v] {
                    let comp_start = out_nodes.len();
                    while let Some(member) = tarjan_stack.pop() {
                        on_stack[member as usize] = false;
                        out_nodes.push(member);
                        if member == finished_v {
                            break;
                        }
                    }
                    out_nodes[comp_start..].sort_unstable();
                    out_offsets.push(out_nodes.len() as u32);
                }
            }
        }
    }

    SccOrder { nodes: out_nodes, offsets: out_offsets }
}

/// Internal `Bodies` overlay that serves the module's latest optimized `Func`s for intra-module
/// callees while transitively resolving `ImportedRef`s added in earlier SCC passes and forwarding
/// external bodies, targets, data, and source positions to `external`.
struct ModuleBodies<'a, A: Allocator> {
    bodies: &'a [ModuleBody<A>],
    imports: &'a [Vec<ImportedRef, A>],
    id_to_index: &'a Map<BodyId, usize, FixedState, A>,
    external: &'a dyn Bodies,
}

impl<A: Allocator> ModuleBodies<'_, A> {
    fn resolve_origin(&self, mut from: BodyId, mut reference: RefId) -> (BodyId, RefId) {
        for _ in 0..32 {
            let Some(&idx) = self.id_to_index.get(&from) else {
                break;
            };
            let Some(body_imports) = self.imports.get(idx) else {
                break;
            };
            let Some(imp) = body_imports.iter().find(|r| r.caller_ref == reference) else {
                break;
            };
            from = imp.source_body;
            reference = imp.source_ref;
        }
        (from, reference)
    }
}

impl<A: Allocator> Bodies for ModuleBodies<'_, A> {
    fn callee(&self, from: BodyId, reference: RefId) -> Option<(BodyId, View<'_>)> {
        let (orig_from, orig_ref) = self.resolve_origin(from, reference);
        let (callee_id, ext_view) = self.external.callee(orig_from, orig_ref)?;
        if let Some(&idx) = self.id_to_index.get(&callee_id)
            && let Some(mb) = self.bodies.get(idx)
        {
            return Some((callee_id, mb.func.view()));
        }
        Some((callee_id, ext_view))
    }

    fn target(&self, from: BodyId, reference: RefId) -> Option<Target> {
        let (orig_from, orig_ref) = self.resolve_origin(from, reference);
        self.external.target(orig_from, orig_ref)
    }

    fn datum(&self, id: DatumId) -> Option<Datum<'_>> {
        self.external.datum(id)
    }

    fn body(&self, id: BodyId) -> Option<View<'_>> {
        if let Some(&idx) = self.id_to_index.get(&id) {
            return self.bodies.get(idx).map(|b| b.func.view());
        }
        self.external.body(id)
    }

    fn position(&self, item: u128, site: u32) -> Option<(u32, u32)> {
        self.external.position(item, site)
    }
}

/// Runs `scalar_replace -> propagate -> thread_jumps -> propagate -> dead -> coalesce_cfg` on `view` in `alloc`.
///
/// Threading phi-discriminant `Option` checks and coalescing single-predecessor `Jump` chains
/// after inlining restores 2-block `HeaderBody` loop shapes so downstream loop passes
/// (bounds-check hoisting, GEMM, vectorization, unrolling, LICM) see the inlined loop body as
/// a single block.
fn simplify_body<A: Allocator + Copy>(view: View<'_>, alloc: A) -> Result<Func<A>, SimplifyError> {
    let scalarized = scalarize_small_memops(view, alloc)?;
    let sroa_in = scalarized.as_ref().map_or(view, |f| f.view());
    let sroa = scalar_replace(sroa_in, alloc)?;
    let prop1 = propagate(sroa.view(), alloc)?;
    let threaded = thread_jumps(prop1.view(), alloc)?;
    let prop2 = propagate(threaded.view(), alloc)?;
    let dce = dead(prop2.view(), alloc)?;
    crate::simplify::coalesce_cfg(dce.view(), alloc)
}

fn from_inline(error: InlineError) -> SimplifyError {
    match error {
        InlineError::Build(e) => SimplifyError::Build(e),
        InlineError::Verify(e) => SimplifyError::Verify(e),
        InlineError::Refused { inst, what } => SimplifyError::Refused { inst, what },
    }
}

/// Builds the intra-module call graph in CSR form, including transitive edges through external
/// bodies (`A -> E1 -> ... -> B` where `E*` are provided by `external.callee` and `B` is in
/// `module`). Including transitive external callback edges ensures that both
/// [`lto_optimize_module`] and [`lto_optimize_module_par`] order `B`'s SCC before `A`'s SCC (or
/// place them in the same SCC if mutually recursive) so sequential and parallel LTO observe
/// identical pre-simplified and post-inlined bodies.
fn build_module_call_graph<A: Allocator + Copy>(
    module: &[ModuleBody<A>],
    id_to_index: &Map<BodyId, usize, FixedState, A>,
    external: &dyn Bodies,
    alloc: A,
) -> (OwnVec<u32, A>, OwnVec<u32, A>) {
    let n = module.len();
    let mut adj_offsets: OwnVec<u32, A> = OwnVec::with_capacity_in(n + 1, alloc);
    let mut adj: OwnVec<u32, A> = OwnVec::with_capacity_in(n.saturating_mul(2), alloc);
    let mut ext_cache: Map<BodyId, (usize, usize), FixedState, A> = Map::new(MapOpts {
        hasher: FixedState,
        alloc,
        capacity: None,
    });
    let mut ext_pool: OwnVec<u32, A> = OwnVec::new_in(alloc);
    adj_offsets.push(0);

    for mb in module.iter() {
        let view = mb.func.view();
        let edge_start = adj.len();
        for at in 0..view.len() {
            if !matches!(view.op(at), Op::Call | Op::Invoke) {
                continue;
            }
            let Some(extra_start) = view.data.get(at).and_then(|&d| usize::try_from(d).ok()) else {
                continue;
            };
            let Some(&ref_word) = view.extra.get(extra_start) else {
                continue;
            };
            let reference = RefId(ref_word);
            let Some((callee_id, callee_view)) = external.callee(mb.id, reference) else {
                continue;
            };
            if let Some(&callee_idx) = id_to_index.get(&callee_id) {
                if let Ok(c_u32) = u32::try_from(callee_idx) {
                    adj.push(c_u32);
                }
                continue;
            }
            if let Some(&(pool_start, pool_end)) = ext_cache.get(&callee_id) {
                if let Some(slice) = ext_pool.get(pool_start..pool_end) {
                    adj.extend_from_slice(slice);
                }
                continue;
            }
            // Transitive external closure: walk external bodies reachable from `callee_id` until
            // hitting module bodies, caching the discovered module target indices for `callee_id`.
            let mut seen_ext: Map<BodyId, (), FixedState, A> = Map::new(MapOpts {
                hasher: FixedState,
                alloc,
                capacity: None,
            });
            let mut ext_stack: OwnVec<(BodyId, View<'_>), A> = OwnVec::new_in(alloc);
            let mut found: OwnVec<u32, A> = OwnVec::new_in(alloc);
            seen_ext.insert(callee_id, ());
            ext_stack.push((callee_id, callee_view));
            while let Some((cur_id, cur_view)) = ext_stack.pop() {
                for cur_at in 0..cur_view.len() {
                    if !matches!(cur_view.op(cur_at), Op::Call | Op::Invoke) {
                        continue;
                    }
                    let Some(es) = cur_view.data.get(cur_at).and_then(|&d| usize::try_from(d).ok()) else {
                        continue;
                    };
                    let Some(&rw) = cur_view.extra.get(es) else {
                        continue;
                    };
                    let Some((next_id, next_view)) = external.callee(cur_id, RefId(rw)) else {
                        continue;
                    };
                    if let Some(&next_idx) = id_to_index.get(&next_id) {
                        if let Ok(c_u32) = u32::try_from(next_idx) {
                            found.push(c_u32);
                        }
                    } else if !seen_ext.contains_key(&next_id) {
                        seen_ext.insert(next_id, ());
                        ext_stack.push((next_id, next_view));
                    }
                }
            }
            if let Some(slice) = found.get_mut(..) {
                slice.sort_unstable();
            }
            let mut fw = 0usize;
            for fr in 0..found.len() {
                let cur = found.get(fr).copied().unwrap_or(0);
                let prev = fw.checked_sub(1).and_then(|p| found.get(p).copied());
                if fw == 0 || prev != Some(cur) {
                    if let Some(slot) = found.get_mut(fw) {
                        *slot = cur;
                    }
                    fw += 1;
                }
            }
            found.truncate(fw);
            let pool_start = ext_pool.len();
            ext_pool.extend_from_slice(found.as_slice());
            let pool_end = ext_pool.len();
            ext_cache.insert(callee_id, (pool_start, pool_end));
            adj.extend_from_slice(found.as_slice());
        }
        if let Some(edge_slice) = adj.get_mut(edge_start..) {
            edge_slice.sort_unstable();
        }
        let mut w = edge_start;
        for r in edge_start..adj.len() {
            let cur = adj.get(r).copied().unwrap_or(0);
            let prev = w.checked_sub(1).and_then(|p| adj.get(p).copied());
            if w == edge_start || prev != Some(cur) {
                if let Some(slot) = adj.get_mut(w) {
                    *slot = cur;
                }
                w += 1;
            }
        }
        adj.truncate(w);
        adj_offsets.push(u32_of(adj.len()));
    }
    (adj_offsets, adj)
}

/// Performs deterministic bottom-up SCC ThinLTO optimization over all functions in `module`.
///
/// Every callee SCC is simplified and summarized before its callers run PGO-guided inlining
/// (`inline_guided_with_cache`), followed by post-inline SROA + constant/copy propagation + DCE.
pub fn lto_optimize_module<A: Allocator + Copy>(
    mut module: OwnVec<ModuleBody<A>, A>,
    external: &dyn Bodies,
    weights: &dyn SiteWeights,
    policy: InlinePolicy,
    alloc: A,
) -> Result<ModuleLto<A>, SimplifyError> {
    let n = module.len();
    let mut id_to_index: Map<BodyId, usize, FixedState, A> = Map::new(MapOpts {
        hasher: FixedState,
        alloc: alloc,
        capacity: Some(n),
    });
    for (idx, mb) in module.iter().enumerate() {
        id_to_index.insert(mb.id, idx);
    }

    // Build intra-module call graph (including transitive external callback edges) in CSR form.
    let (adj_offsets, adj) = build_module_call_graph(module.as_slice(), &id_to_index, external, alloc);

    // Condense call graph into reverse-topological SCCs (callees before callers).
    let sccs = tarjan_sccs(n, &adj_offsets, &adj, alloc);

    // Phase 0 + Phase 1 (100% intra-procedural): compute initial summaries for early-cutoff
    // comparison, run pre-inline `simplify_body` on every module body, and refresh summaries.
    // Running Phase 1 across all bodies up front matches `lto_optimize_module_par` identically.
    let mut prev_summaries: OwnVec<FnSummary, A> = OwnVec::with_capacity_in(n, alloc);
    let mut summaries: Map<BodyId, FnSummary, FixedState, A> = Map::new(MapOpts {
        hasher: FixedState,
        alloc: alloc,
        capacity: Some(n),
    });
    for mb in module.iter_mut() {
        let prev = FnSummary::compute(mb.func.view(), &alloc);
        prev_summaries.push(prev);
        let simplified = simplify_body(mb.func.view(), alloc)?;
        let post = FnSummary::compute(simplified.view(), &alloc);
        summaries.insert(mb.id, post);
        mb.func = simplified;
    }

    let mut reads: OwnVec<Vec<BodyId, A>, A> = OwnVec::with_capacity_in(n, alloc);
    let mut imports: OwnVec<Vec<ImportedRef, A>, A> = OwnVec::with_capacity_in(n, alloc);
    for _ in 0..n {
        reads.push(Vec::new_in(alloc));
        imports.push(Vec::new_in(alloc));
    }

    let mut total_inlined_bodies = 0usize;

    // Optimize SCC by SCC, bottom-up.
    for scc_idx in 0..sccs.len() {
        let comp = sccs.component(scc_idx);

        // Phase 2: PGO-guided priority inlining + post-inline simplify (SROA + const-fold + DCE).
        for &node in comp {
            let idx = node as usize;
            let body_id = module[idx].id;
            let inlined = {
                let overlay = ModuleBodies {
                    bodies: &module,
                    imports: &imports,
                    id_to_index: &id_to_index,
                    external,
                };
                inline_guided_with_cache(
                    module[idx].func.view(),
                    body_id,
                    &overlay,
                    weights,
                    policy,
                    &mut summaries,
                    alloc,
                )
                .map_err(from_inline)?
            };

            let spliced_count = inlined.read.len();
            total_inlined_bodies += spliced_count;
            let final_func = if spliced_count > 0 {
                simplify_body(inlined.func.view(), alloc)?
            } else {
                inlined.func
            };
            let final_summary = FnSummary::compute(final_func.view(), &alloc);
            summaries.insert(body_id, final_summary);
            module[idx].func = final_func;
            reads[idx] = inlined.read;
            imports[idx] = inlined.imported_refs;

            let tce = if let Some(mb) = module.get(idx) {
                let overlay = ModuleBodies {
                    bodies: &module,
                    imports: &imports,
                    id_to_index: &id_to_index,
                    external,
                };
                eliminate_self_tail_calls_if_any(mb.func.view(), body_id, &overlay, alloc)?
            } else {
                None
            };
            if let Some((tce_func, _)) = tce {
                let post_tce = simplify_body(tce_func.view(), alloc)?;
                let sum = FnSummary::compute(post_tce.view(), &alloc);
                summaries.insert(body_id, sum);
                if let Some(mb) = module.get_mut(idx) {
                    mb.func = post_tce;
                }
            }
        }
    }

    let mut out_bodies: OwnVec<OptimizedBody<A>, A> = OwnVec::with_capacity_in(n, alloc);
    let mut mod_iter = module.into_iter();
    let mut read_iter = reads.into_iter();
    let mut imp_iter = imports.into_iter();
    for idx in 0..n {
        let mb = mod_iter.next().expect("same length");
        let read = read_iter.next().expect("same length");
        let imported_refs = imp_iter.next().expect("same length");
        let prev_summary = prev_summaries[idx];
        let summary = *summaries.get(&mb.id).expect("summary computed");
        let summary_changed = summary.summary_hash != prev_summary.summary_hash;
        out_bodies.push(OptimizedBody {
            id: mb.id,
            func: mb.func,
            prev_summary,
            summary,
            read,
            imported_refs,
            summary_changed,
        });
    }

    Ok(ModuleLto {
        bodies: out_bodies,
        scc_count: sccs.len(),
        total_inlined_bodies,
    })
}

/// Parallel execution strategy for [`lto_optimize_module_par`] (Rule 78: strategy-is-a-type-parameter).
///
/// Runs `f(idx, worker_scratch_alloc)` for every `idx` in `0..count` and returns results in exact
/// ascending `idx` order (Rule 25). Each worker provides a reusable scratch [`Alloc`] that is
/// reset between items while persistent outputs allocate in `out_alloc` (`SharedAlloc<'static>`).
pub trait LtoRunner {
    fn par_map<T: Send, F>(
        &self,
        count: usize,
        out_alloc: SharedAlloc<'static>,
        f: F,
    ) -> Result<OwnVec<T, SharedAlloc<'static>>, SimplifyError>
    where
        F: Fn(usize, Alloc<'_>) -> T + Sync;
}

/// Sequential [`LtoRunner`] running every item inline on the caller's thread.
#[derive(Clone, Copy, Default, Debug)]
pub struct SequentialLtoRunner;

impl LtoRunner for SequentialLtoRunner {
    fn par_map<T: Send, F>(
        &self,
        count: usize,
        out_alloc: SharedAlloc<'static>,
        f: F,
    ) -> Result<OwnVec<T, SharedAlloc<'static>>, SimplifyError>
    where
        F: Fn(usize, Alloc<'_>) -> T + Sync,
    {
        let mut out = OwnVec::with_capacity_in(count, out_alloc);
        let scratch = out_alloc.local();
        for idx in 0..count {
            out.push(f(idx, scratch));
        }
        Ok(out)
    }
}

/// Read-only overlay for a multi-node recursive SCC running inside one worker task: checks
/// SCC-local updated bodies/imports first, falling back to the frozen module from lower levels.
struct SccLocalBodies<'a, 'b, A: Allocator> {
    local_indices: &'b [u32],
    local_funcs: &'b [Option<Func<Alloc<'a>>>],
    local_imports: &'b [Option<OwnVec<ImportedRef, Alloc<'a>>>],
    base_bodies: &'b [ModuleBody<A>],
    base_imports: &'b [OwnVec<ImportedRef, A>],
    id_to_index: &'b Map<BodyId, usize, FixedState, A>,
    external: &'b (dyn Bodies + Sync),
}

impl<A: Allocator> SccLocalBodies<'_, '_, A> {
    fn local_pos(&self, idx: usize) -> Option<usize> {
        let idx_u32 = u32::try_from(idx).ok()?;
        self.local_indices.binary_search(&idx_u32).ok()
    }

    fn resolve_origin(&self, mut from: BodyId, mut reference: RefId) -> (BodyId, RefId) {
        for _ in 0..32 {
            let Some(&idx) = self.id_to_index.get(&from) else {
                break;
            };
            let found = if let Some(lpos) = self.local_pos(idx)
                && let Some(Some(imps)) = self.local_imports.get(lpos)
            {
                imps.iter().find(|r| r.caller_ref == reference).copied()
            } else {
                self.base_imports
                    .get(idx)
                    .and_then(|imps| imps.iter().find(|r| r.caller_ref == reference).copied())
            };
            let Some(imp) = found else {
                break;
            };
            from = imp.source_body;
            reference = imp.source_ref;
        }
        (from, reference)
    }
}

impl<A: Allocator> Bodies for SccLocalBodies<'_, '_, A> {
    fn callee(&self, from: BodyId, reference: RefId) -> Option<(BodyId, View<'_>)> {
        let (orig_from, orig_ref) = self.resolve_origin(from, reference);
        let (callee_id, ext_view) = self.external.callee(orig_from, orig_ref)?;
        if let Some(&idx) = self.id_to_index.get(&callee_id) {
            if let Some(lpos) = self.local_pos(idx)
                && let Some(Some(f)) = self.local_funcs.get(lpos)
            {
                return Some((callee_id, f.view()));
            }
            if let Some(mb) = self.base_bodies.get(idx) {
                return Some((callee_id, mb.func.view()));
            }
        }
        Some((callee_id, ext_view))
    }

    fn target(&self, from: BodyId, reference: RefId) -> Option<Target> {
        let (orig_from, orig_ref) = self.resolve_origin(from, reference);
        self.external.target(orig_from, orig_ref)
    }

    fn datum(&self, id: DatumId) -> Option<Datum<'_>> {
        self.external.datum(id)
    }

    fn body(&self, id: BodyId) -> Option<View<'_>> {
        if let Some(&idx) = self.id_to_index.get(&id) {
            if let Some(lpos) = self.local_pos(idx)
                && let Some(Some(f)) = self.local_funcs.get(lpos)
            {
                return Some(f.view());
            }
            return self.base_bodies.get(idx).map(|b| b.func.view());
        }
        self.external.body(id)
    }

    fn position(&self, item: u128, site: u32) -> Option<(u32, u32)> {
        self.external.position(item, site)
    }
}

/// Result of optimizing one node inside an SCC wavefront task.
struct SccNodeResult {
    idx: usize,
    func: Func<SharedAlloc<'static>>,
    summary: FnSummary,
    read: OwnVec<BodyId, SharedAlloc<'static>>,
    imported_refs: OwnVec<ImportedRef, SharedAlloc<'static>>,
}

/// Copies a slice of `Copy` items into an `OwnVec<T, A>`.
fn copy_own_vec<T: Copy, A: Allocator>(src: &[T], alloc: A) -> OwnVec<T, A> {
    let mut out = OwnVec::with_capacity_in(src.len(), alloc);
    out.extend_from_slice(src);
    out
}

/// Read-only overlay for parallel singleton SCC tasks backed by `OwnVec` import tables.
struct ParModuleBodies<'a, A: Allocator> {
    bodies: &'a [ModuleBody<A>],
    imports: &'a [OwnVec<ImportedRef, A>],
    id_to_index: &'a Map<BodyId, usize, FixedState, A>,
    external: &'a (dyn Bodies + Sync),
}

impl<A: Allocator> ParModuleBodies<'_, A> {
    fn resolve_origin(&self, mut from: BodyId, mut reference: RefId) -> (BodyId, RefId) {
        for _ in 0..32 {
            let Some(&idx) = self.id_to_index.get(&from) else {
                break;
            };
            let Some(body_imports) = self.imports.get(idx) else {
                break;
            };
            let Some(imp) = body_imports.iter().find(|r| r.caller_ref == reference) else {
                break;
            };
            from = imp.source_body;
            reference = imp.source_ref;
        }
        (from, reference)
    }
}

impl<A: Allocator> Bodies for ParModuleBodies<'_, A> {
    fn callee(&self, from: BodyId, reference: RefId) -> Option<(BodyId, View<'_>)> {
        let (orig_from, orig_ref) = self.resolve_origin(from, reference);
        let (callee_id, ext_view) = self.external.callee(orig_from, orig_ref)?;
        if let Some(&idx) = self.id_to_index.get(&callee_id)
            && let Some(mb) = self.bodies.get(idx)
        {
            return Some((callee_id, mb.func.view()));
        }
        Some((callee_id, ext_view))
    }

    fn target(&self, from: BodyId, reference: RefId) -> Option<Target> {
        let (orig_from, orig_ref) = self.resolve_origin(from, reference);
        self.external.target(orig_from, orig_ref)
    }

    fn datum(&self, id: DatumId) -> Option<Datum<'_>> {
        self.external.datum(id)
    }

    fn body(&self, id: BodyId) -> Option<View<'_>> {
        if let Some(&idx) = self.id_to_index.get(&id) {
            return self.bodies.get(idx).map(|b| b.func.view());
        }
        self.external.body(id)
    }

    fn position(&self, item: u128, site: u32) -> Option<(u32, u32)> {
        self.external.position(item, site)
    }
}

/// Parallel deterministic bottom-up SCC ThinLTO optimization over `module`.
///
/// 1. Runs intra-procedural pre-inline `simplify_body` + `FnSummary` across all `n` bodies in
///    parallel using per-worker scratch frames.
/// 2. Partitions the Tarjan SCC DAG into reverse-topological levels (wavefronts) and optimizes
///    every independent SCC within each level in parallel, merging outputs in exact ascending
///    node index order so output is bit-for-bit identical to [`lto_optimize_module`] (Rule 25).
pub fn lto_optimize_module_par<R: LtoRunner>(
    mut module: OwnVec<ModuleBody<SharedAlloc<'static>>, SharedAlloc<'static>>,
    external: &(dyn Bodies + Sync),
    weights: &(dyn SiteWeights + Sync),
    policy: InlinePolicy,
    alloc: SharedAlloc<'static>,
    runner: &R,
) -> Result<ModuleLto<SharedAlloc<'static>>, SimplifyError> {
    let n = module.len();
    let mut id_to_index: Map<BodyId, usize, FixedState, SharedAlloc<'static>> = Map::new(MapOpts {
        hasher: FixedState,
        alloc,
        capacity: Some(n),
    });
    for (idx, mb) in module.iter().enumerate() {
        id_to_index.insert(mb.id, idx);
    }

    // Build intra-module call graph (including transitive external callback edges) in CSR form.
    let (adj_offsets, adj) = build_module_call_graph(module.as_slice(), &id_to_index, external, alloc);

    // Condense call graph into reverse-topological SCCs (callees before callers).
    let sccs = tarjan_sccs(n, &adj_offsets, &adj, alloc);
    let scc_count = sccs.len();

    // Phase 0 + Phase 1 (100% intra-procedural): compute initial summary, run pre-inline
    // `simplify_body` in worker scratch, and refresh summary across all `n` bodies in parallel.
    let module_ref = module.as_slice();
    let pre_results = runner.par_map(n, alloc, |idx, scratch| -> Result<(FnSummary, FnSummary, Func<SharedAlloc<'static>>), SimplifyError> {
        let Some(mb) = module_ref.get(idx) else {
            return Err(SimplifyError::Refused { inst: 0, what: "module index out of range" });
        };
        let view = mb.func.view();
        let prev = FnSummary::compute(view, &scratch);
        let simplified = simplify_body(view, scratch)?;
        let post = FnSummary::compute(simplified.view(), &scratch);
        let owned = simplified.view().to_func_in(alloc);
        Ok((prev, post, owned))
    })?;

    let mut prev_summaries: OwnVec<FnSummary, SharedAlloc<'static>> = OwnVec::with_capacity_in(n, alloc);
    let mut summaries: Map<BodyId, FnSummary, FixedState, SharedAlloc<'static>> = Map::new(MapOpts {
        hasher: FixedState,
        alloc,
        capacity: Some(n),
    });
    for (idx, res) in pre_results.into_iter().enumerate() {
        let (prev, post, simplified) = res?;
        prev_summaries.push(prev);
        if let Some(mb) = module.get_mut(idx) {
            summaries.insert(mb.id, post);
            mb.func = simplified;
        }
    }

    // Group SCCs into topological wavefront levels (0 = leaves, max = roots).
    let mut node_scc: OwnVec<u32, SharedAlloc<'static>> = OwnVec::from_elem_in(0u32, n, alloc);
    for scc_idx in 0..scc_count {
        let scc_u32 = u32_of(scc_idx);
        for &node in sccs.component(scc_idx) {
            if let Some(slot) = node_scc.get_mut(to_idx(node)) {
                *slot = scc_u32;
            }
        }
    }
    let mut scc_level: OwnVec<u32, SharedAlloc<'static>> = OwnVec::from_elem_in(0u32, scc_count, alloc);
    let mut max_level = 0u32;
    for scc_idx in 0..scc_count {
        let mut lvl = 0u32;
        for &node in sccs.component(scc_idx) {
            let ni = to_idx(node);
            let start = adj_offsets.get(ni).copied().map_or(0, to_idx);
            let end = adj_offsets.get(ni + 1).copied().map_or(start, to_idx);
            if let Some(callees) = adj.get(start..end) {
                for &callee in callees {
                    let cs = node_scc.get(to_idx(callee)).copied().map_or(scc_idx, to_idx);
                    if cs != scc_idx {
                        let cl = scc_level.get(cs).copied().unwrap_or(0).saturating_add(1);
                        if cl > lvl {
                            lvl = cl;
                        }
                    }
                }
            }
        }
        if let Some(slot) = scc_level.get_mut(scc_idx) {
            *slot = lvl;
        }
        if lvl > max_level {
            max_level = lvl;
        }
    }
    let num_levels = to_idx(max_level).saturating_add(1);
    let mut level_counts: OwnVec<usize, SharedAlloc<'static>> = OwnVec::from_elem_in(0usize, num_levels, alloc);
    for &lvl in scc_level.iter() {
        if let Some(slot) = level_counts.get_mut(to_idx(lvl)) {
            *slot += 1;
        }
    }
    let mut level_offsets: OwnVec<usize, SharedAlloc<'static>> = OwnVec::with_capacity_in(num_levels + 1, alloc);
    let mut sum_off = 0usize;
    for &c in level_counts.iter() {
        level_offsets.push(sum_off);
        sum_off += c;
    }
    level_offsets.push(sum_off);
    let mut level_fill: OwnVec<usize, SharedAlloc<'static>> = OwnVec::with_capacity_in(num_levels, alloc);
    level_fill.extend_from_slice(level_offsets.get(..num_levels).unwrap_or(&[]));
    let mut level_sccs: OwnVec<u32, SharedAlloc<'static>> = OwnVec::from_elem_in(0u32, scc_count, alloc);
    for scc_idx in 0..scc_count {
        let lvl = scc_level.get(scc_idx).copied().map_or(0, to_idx);
        if let Some(pos_slot) = level_fill.get_mut(lvl) {
            let pos = *pos_slot;
            *pos_slot += 1;
            if let Some(slot) = level_sccs.get_mut(pos) {
                *slot = u32_of(scc_idx);
            }
        }
    }

    let mut reads: OwnVec<OwnVec<BodyId, SharedAlloc<'static>>, SharedAlloc<'static>> = OwnVec::with_capacity_in(n, alloc);
    let mut imports: OwnVec<OwnVec<ImportedRef, SharedAlloc<'static>>, SharedAlloc<'static>> = OwnVec::with_capacity_in(n, alloc);
    for _ in 0..n {
        reads.push(OwnVec::new_in(alloc));
        imports.push(OwnVec::new_in(alloc));
    }

    let mut total_inlined_bodies = 0usize;

    // Phase 2: bottom-up SCC wavefronts. Every SCC in the same level depends only on levels < lvl.
    for lvl in 0..num_levels {
        let l_start = level_offsets.get(lvl).copied().unwrap_or(0);
        let l_end = level_offsets.get(lvl + 1).copied().unwrap_or(l_start);
        let level_slice = level_sccs.get(l_start..l_end).unwrap_or(&[]);
        if level_slice.is_empty() {
            continue;
        }
        let sccs_ref = &sccs;
        let module_ref = module.as_slice();
        let imports_ref = imports.as_slice();
        let id_to_index_ref = &id_to_index;
        let summaries_ref = &summaries;

        let level_results = runner.par_map(level_slice.len(), alloc, |pos, scratch| -> Result<OwnVec<SccNodeResult, SharedAlloc<'static>>, SimplifyError> {
            let Some(&scc_u32) = level_slice.get(pos) else {
                return Ok(OwnVec::new_in(alloc));
            };
            let comp = sccs_ref.component(to_idx(scc_u32));
            let mut results = OwnVec::with_capacity_in(comp.len(), alloc);

            if comp.len() == 1 {
                let Some(&node_u32) = comp.first() else {
                    return Ok(results);
                };
                let idx = to_idx(node_u32);
                let Some(mb) = module_ref.get(idx) else {
                    return Ok(results);
                };
                let body_id = mb.id;
                let overlay = ParModuleBodies {
                    bodies: module_ref,
                    imports: imports_ref,
                    id_to_index: id_to_index_ref,
                    external,
                };
                let mut local_sums: Map<BodyId, FnSummary, FixedState, Alloc<'_>> = Map::new(MapOpts {
                    hasher: FixedState,
                    alloc: scratch,
                    capacity: None,
                });
                let inlined = inline_guided_with_base_cache(
                    mb.func.view(),
                    body_id,
                    &overlay,
                    weights,
                    policy,
                    Some(summaries_ref),
                    &mut local_sums,
                    scratch,
                )
                .map_err(from_inline)?;
                let spliced_count = inlined.read.len();
                let mut cur_func = if spliced_count > 0 {
                    simplify_body(inlined.func.view(), scratch)?
                } else {
                    inlined.func
                };
                let read_shared = copy_own_vec(inlined.read.as_slice(), alloc);
                let imp_shared = copy_own_vec(inlined.imported_refs.as_slice(), alloc);
                let imp_local = copy_own_vec(inlined.imported_refs.as_slice(), scratch);
                let local_idx_arr = [node_u32];
                let local_funcs_arr = [None::<Func<Alloc<'_>>>];
                let local_imps_arr = [Some(imp_local)];
                let tce_overlay = SccLocalBodies {
                    local_indices: &local_idx_arr,
                    local_funcs: &local_funcs_arr,
                    local_imports: &local_imps_arr,
                    base_bodies: module_ref,
                    base_imports: imports_ref,
                    id_to_index: id_to_index_ref,
                    external,
                };
                if let Some((tce_func, _)) = eliminate_self_tail_calls_if_any(cur_func.view(), body_id, &tce_overlay, scratch)? {
                    cur_func = simplify_body(tce_func.view(), scratch)?;
                }
                let final_summary = FnSummary::compute(cur_func.view(), &scratch);
                results.push(SccNodeResult {
                    idx,
                    func: cur_func.view().to_func_in(alloc),
                    summary: final_summary,
                    read: read_shared,
                    imported_refs: imp_shared,
                });
                return Ok(results);
            }

            // Multi-node recursive SCC: process nodes in deterministic ascending index order
            // within this worker task using a local overlay over the frozen base module.
            let mut local_funcs: OwnVec<Option<Func<Alloc<'_>>>, Alloc<'_>> = OwnVec::with_capacity_in(comp.len(), scratch);
            let mut local_imps: OwnVec<Option<OwnVec<ImportedRef, Alloc<'_>>>, Alloc<'_>> = OwnVec::with_capacity_in(comp.len(), scratch);
            for _ in 0..comp.len() {
                local_funcs.push(None);
                local_imps.push(None);
            }
            let mut local_sums: Map<BodyId, FnSummary, FixedState, Alloc<'_>> = Map::new(MapOpts {
                hasher: FixedState,
                alloc: scratch,
                capacity: Some(comp.len()),
            });
            for (lpos, &node_u32) in comp.iter().enumerate() {
                let idx = to_idx(node_u32);
                let Some(mb) = module_ref.get(idx) else {
                    continue;
                };
                let body_id = mb.id;
                let inlined = {
                    let overlay = SccLocalBodies {
                        local_indices: comp,
                        local_funcs: local_funcs.as_slice(),
                        local_imports: local_imps.as_slice(),
                        base_bodies: module_ref,
                        base_imports: imports_ref,
                        id_to_index: id_to_index_ref,
                        external,
                    };
                    inline_guided_with_base_cache(
                        mb.func.view(),
                        body_id,
                        &overlay,
                        weights,
                        policy,
                        Some(summaries_ref),
                        &mut local_sums,
                        scratch,
                    )
                    .map_err(from_inline)?
                };
                let spliced_count = inlined.read.len();
                let mut cur_func = if spliced_count > 0 {
                    simplify_body(inlined.func.view(), scratch)?
                } else {
                    inlined.func
                };
                let read_shared = copy_own_vec(inlined.read.as_slice(), alloc);
                let imp_shared = copy_own_vec(inlined.imported_refs.as_slice(), alloc);
                if let Some(slot) = local_imps.get_mut(lpos) {
                    *slot = Some(copy_own_vec(inlined.imported_refs.as_slice(), scratch));
                }
                let tce = {
                    let overlay = SccLocalBodies {
                        local_indices: comp,
                        local_funcs: local_funcs.as_slice(),
                        local_imports: local_imps.as_slice(),
                        base_bodies: module_ref,
                        base_imports: imports_ref,
                        id_to_index: id_to_index_ref,
                        external,
                    };
                    eliminate_self_tail_calls_if_any(cur_func.view(), body_id, &overlay, scratch)?
                };
                if let Some((tce_func, _)) = tce {
                    cur_func = simplify_body(tce_func.view(), scratch)?;
                }
                let final_summary = FnSummary::compute(cur_func.view(), &scratch);
                local_sums.insert(body_id, final_summary);
                let shared_func = cur_func.view().to_func_in(alloc);
                if let Some(slot) = local_funcs.get_mut(lpos) {
                    *slot = Some(cur_func);
                }
                results.push(SccNodeResult {
                    idx,
                    func: shared_func,
                    summary: final_summary,
                    read: read_shared,
                    imported_refs: imp_shared,
                });
            }
            Ok(results)
        })?;

        for scc_res in level_results {
            let nodes = scc_res?;
            for item in nodes {
                total_inlined_bodies += item.read.len();
                if let Some(mb) = module.get_mut(item.idx) {
                    summaries.insert(mb.id, item.summary);
                    mb.func = item.func;
                }
                if let Some(slot) = reads.get_mut(item.idx) {
                    *slot = item.read;
                }
                if let Some(slot) = imports.get_mut(item.idx) {
                    *slot = item.imported_refs;
                }
            }
        }
    }

    let mut out_bodies: OwnVec<OptimizedBody<SharedAlloc<'static>>, SharedAlloc<'static>> = OwnVec::with_capacity_in(n, alloc);
    let mut mod_iter = module.into_iter();
    let mut read_iter = reads.into_iter();
    let mut imp_iter = imports.into_iter();
    for idx in 0..n {
        let Some(mb) = mod_iter.next() else {
            return Err(SimplifyError::Refused { inst: 0, what: "module length mismatch" });
        };
        let Some(read_own) = read_iter.next() else {
            return Err(SimplifyError::Refused { inst: 0, what: "reads length mismatch" });
        };
        let Some(imp_own) = imp_iter.next() else {
            return Err(SimplifyError::Refused { inst: 0, what: "imports length mismatch" });
        };
        let prev_summary = prev_summaries.get(idx).copied().unwrap_or_else(|| FnSummary::compute(mb.func.view(), &alloc));
        let summary = summaries.get(&mb.id).copied().unwrap_or(prev_summary);
        let summary_changed = summary.summary_hash != prev_summary.summary_hash;
        let mut read = Vec::with_capacity_in(read_own.len(), alloc);
        read.extend_from_slice(read_own.as_slice());
        let mut imported_refs = Vec::with_capacity_in(imp_own.len(), alloc);
        imported_refs.extend_from_slice(imp_own.as_slice());
        out_bodies.push(OptimizedBody {
            id: mb.id,
            func: mb.func,
            prev_summary,
            summary,
            read,
            imported_refs,
            summary_changed,
        });
    }

    Ok(ModuleLto {
        bodies: out_bodies,
        scc_count,
        total_inlined_bodies,
    })
}

/// Ranks optimized bodies for on-the-fly JIT Tier-1 recompilation and atomic trampoline patching
/// (`blaze-inproc::JitPool::retarget`).
///
/// Cold functions (`summary.is_cold`) and bodies below `min_weight` are excluded. Functions with
/// loops receive a `4x` priority boost because loop bodies amortize JIT compile time fastest.
pub fn advise_jit_tier_up<A: Allocator + Copy>(
    bodies: &[OptimizedBody<A>],
    pgo: &PgoSiteTable<A>,
    min_weight: u64,
    alloc: A,
) -> OwnVec<TierUpCandidate, A> {
    let mut out = OwnVec::with_capacity_in(bodies.len(), alloc);
    for b in bodies {
        if b.summary.is_cold || b.summary.has_forbidden_ops {
            continue;
        }
        let weight = pgo.body_weight(b.id);
        if weight < min_weight {
            continue;
        }
        let loop_boost = if b.summary.has_loops { 4u64 } else { 1u64 };
        let priority = weight.saturating_mul(loop_boost);
        out.push(TierUpCandidate {
            body: b.id,
            weight,
            insts: b.summary.insts,
            has_loops: b.summary.has_loops,
            priority,
            summary_hash: b.summary.summary_hash,
        });
    }
    out.sort_unstable_by(|a, b| {
        b.priority
            .cmp(&a.priority)
            .then_with(|| b.weight.cmp(&a.weight))
            .then_with(|| a.insts.cmp(&b.insts))
            .then_with(|| a.body.cmp(&b.body))
    });
    out
}

/// Supplies PGO-observed indirect-call targets (`dyn Trait` / fn pointers) for each body in a
/// module during [`lto_optimize_module_sota`].
pub trait IndirectProfiles {
    fn targets(&self, body: BodyId) -> &[IndirectTarget];
}

/// Empty indirect-call profile provider (disables speculative devirtualization).
pub struct NoIndirectProfiles;
impl IndirectProfiles for NoIndirectProfiles {
    fn targets(&self, _body: BodyId) -> &[IndirectTarget] {
        &[]
    }
}

/// Configuration for [`lto_optimize_module_sota`].
#[derive(Clone, Copy, Debug)]
pub struct SotaLtoOpts {
    pub reg_budget: RegBudget,
    pub max_aegraph_rounds: u32,
    pub min_devirt_hit_percent: u8,
    pub auto_vectorize: bool,
    pub scalar_unroll: bool,
    pub superopt: Option<SuperoptOpts>,
}

impl Default for SotaLtoOpts {
    fn default() -> Self {
        Self {
            reg_budget: RegBudget::AARCH64,
            max_aegraph_rounds: 3,
            min_devirt_hit_percent: 80,
            auto_vectorize: true,
            scalar_unroll: true,
            superopt: Some(SuperoptOpts::apple_silicon()),
        }
    }
}

/// Runs the full cross-function post-inline SOTA pipeline on one body:
/// `SROA -> prop -> if_convert(4) -> thread_jumps -> prop -> DCE -> aegraph fixpoint -> BCE -> Loop Unswitching -> if_convert(32) -> Memory LICM/DSE -> 4x SIMD Auto-Vectorization -> 4x Scalar Unrolling -> Bounded E-Graph Superopt`.
fn sota_optimize_body<A: Allocator + Copy>(
    view: View<'_>,
    opts: SotaLtoOpts,
    alloc: A,
) -> Result<Func<A>, SimplifyError> {
    let mut cur = scalar_replace(view, alloc)?;
    cur = propagate(cur.view(), alloc)?;
    cur = if_convert(cur.view(), 4, alloc)?.func;
    cur = thread_jumps(cur.view(), alloc)?;
    cur = propagate(cur.view(), alloc)?;
    cur = dead(cur.view(), alloc)?;
    cur = sota_optimize_fixpoint(cur.view(), opts.reg_budget, opts.max_aegraph_rounds, |_| 1, alloc)?.func;
    let bce = eliminate_redundant_checks(cur.view(), alloc)?;
    cur = bce.func;
    let unsw = unswitch_loops(cur.view(), 2, alloc)?;
    cur = unsw.func;
    let ifc = if_convert(cur.view(), 32, alloc)?;
    cur = ifc.func;
    if bce.stats.checks_eliminated > 0 || unsw.stats.loops_unswitched > 0 || ifc.stats.branches_converted > 0 {
        cur = sota_optimize_fixpoint(cur.view(), opts.reg_budget, 2, |_| 1, alloc)?.func;
    }
    cur = optimize_memory(cur.view(), alloc)?.func;
    let mut vectorized = false;
    if opts.auto_vectorize {
        // Vectorize every eligible loop in the function (bounded at 16 rounds; vectorized
        // loops' scalar tails gain a third predecessor so they are never re-matched).
        for _ in 0..16 {
            let vout = auto_vectorize_loops(cur.view(), alloc)?;
            if vout.stats.loops_vectorized == 0 {
                break;
            }
            vectorized = true;
            cur = vout.func;
        }
        if vectorized {
            cur = propagate(cur.view(), alloc)?;
            cur = dead(cur.view(), alloc)?;
        }
        if !vectorized {
            let mout = vectorize_memchr_loops(cur.view(), alloc)?;
            vectorized = mout.stats.loops_vectorized > 0;
            cur = mout.func;
        }
        if !vectorized {
            cur = slp_vectorize(cur.view(), alloc)?.func;
        }
    }
    if opts.scalar_unroll && !vectorized {
        cur = unroll_scalar_loops(cur.view(), alloc)?.func;
    }
    if opts.auto_vectorize {
        cur = tail_duplicate_dispatch(cur.view(), alloc)?.func;
    }
    if let Some(sopts) = opts.superopt {
        if let Ok(sout) = superopt_a64(cur.view(), sopts, |_| true, alloc) {
            cur = sout.func;
        }
    }
    cur = schedule_dag(cur.view(), alloc)?.func;
    Ok(cur)
}

#[inline]
fn to_idx(v: u32) -> usize {
    usize::try_from(v).unwrap_or(usize::MAX)
}

#[inline]
fn u32_of(v: usize) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}

#[inline]
fn trunc_u32(v: u64) -> u32 {
    let b = v.to_le_bytes();
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

#[inline]
fn op_byte(op: Op) -> u8 {
    Op::ALL.iter().position(|&o| o == op).and_then(|i| u8::try_from(i).ok()).unwrap_or(0)
}

#[inline]
fn at<T: Copy>(s: &[T], i: usize, default: T) -> T {
    s.get(i).copied().unwrap_or(default)
}

#[inline]
fn set_at<T>(s: &mut [T], i: usize, val: T) {
    if let Some(slot) = s.get_mut(i) {
        *slot = val;
    }
}

/// Clones `callee` with entry parameter `param_idx` specialized to compile-time integer constant
/// `const_val` (preserving the original C/Rust signature so callers keep an identical ABI) and runs
/// the full SOTA pipeline (`SROA -> aegraph -> BCE -> unswitch -> ifconv -> SIMD -> superopt`)
/// over the specialized clone!
///
/// Ideal for hot functions called with constant mode flags, enum tags, strides, or divisors that
/// are too large to inline directly.
pub fn specialize_body_const_param<A: Allocator + Copy>(
    callee: View<'_>,
    param_idx: usize,
    const_val: u64,
    opts: SotaLtoOpts,
    alloc: A,
) -> Result<Func<A>, SimplifyError> {
    use crate::{
        Block, Flags, Ty,
        op::{Field, Layout},
        verify,
    };

    if callee.blocks() == 0 {
        return Ok(callee.to_func_in(alloc));
    }
    let entry_params = callee.params(Block(0));
    if param_idx >= entry_params.len() {
        return Err(SimplifyError::Refused { inst: 0, what: "specialize param_idx out of range" });
    }
    let target_param = entry_params.start + param_idx;
    let pty = Ty(at(callee.tys, target_param, 0));
    if !(pty.is_scalar() && pty.is_int() && pty.bits(64) <= 64) {
        return Err(SimplifyError::Refused { inst: target_param, what: "non-integer specialize parameter" });
    }

    let len = callee.len();
    let mut func = callee.to_func_in(alloc);
    func.ops.clear();
    func.flags.clear();
    func.tys.clear();
    func.data.clear();
    func.extra.clear();
    func.attrs.clear();
    func.locs.clear();
    func.block_starts.clear();
    func.block_kinds.clear();

    let mut val_map: OwnVec<u32, A> = OwnVec::from_elem_in(crate::func::NONE, len, alloc);
    for b in 0..callee.blocks() {
        func.block_starts.push(u32_of(func.ops.len()));
        func.block_kinds.push(at(callee.block_kinds, b, 0));
        let blk = Block(u32_of(b));
        let range = callee.block(blk);
        let params_end = callee.params(blk).end;

        // Copy all `Op::Param`s first (preserving exact function signature!).
        for inst in range.start..params_end {
            let idx = u32_of(func.ops.len());
            func.ops.push(at(callee.ops, inst, 0));
            func.flags.push(at(callee.flags, inst, 0));
            func.tys.push(at(callee.tys, inst, 0));
            func.data.push(at(callee.data, inst, 0));
            func.attrs.push(at(callee.attrs, inst, 0));
            func.locs.push(at(callee.locs, inst, 0));
            set_at(&mut val_map, inst, idx);
        }
        // Immediately after block 0's params, materialize `const_val` and redirect all uses of
        // `target_param` to that constant!
        if b == 0 {
            let c_idx = u32_of(func.ops.len());
            if pty.bits(64) <= 32 {
                func.ops.push(op_byte(Op::Iconst));
                func.flags.push(Flags::NONE.0);
                func.tys.push(pty.0);
                func.data.push(trunc_u32(const_val));
            } else if let Ok(v32) = i32::try_from(const_val.cast_signed()) {
                func.ops.push(op_byte(Op::Iconst));
                func.flags.push(Flags::NONE.0);
                func.tys.push(pty.0);
                func.data.push(v32.cast_unsigned());
            } else {
                let extra_at = u32_of(func.extra.len());
                func.extra.push(trunc_u32(const_val));
                func.extra.push(trunc_u32(const_val >> 32));
                func.ops.push(op_byte(Op::IconstWide));
                func.flags.push(Flags::NONE.0);
                func.tys.push(pty.0);
                func.data.push(extra_at);
            }
            func.attrs.push(0);
            func.locs.push(0);
            set_at(&mut val_map, target_param, c_idx);
        }

        for inst in params_end..range.end {
            let op = callee.op(inst);
            let data = match op.info().layout {
                Layout::None => 0,
                Layout::Data(Field::Value) => at(&val_map, to_idx(at(callee.data, inst, 0)), crate::func::NONE),
                Layout::Data(_) => at(callee.data, inst, 0),
                Layout::Extra(fields) => {
                    let src = to_idx(at(callee.data, inst, 0));
                    let dst = u32_of(func.extra.len());
                    let mut p = src;
                    for &f in fields {
                        match f {
                            Field::Value => {
                                func.extra.push(at(&val_map, to_idx(at(callee.extra, p, 0)), crate::func::NONE));
                                p += 1;
                            }
                            Field::Values => {
                                let n_u32 = at(callee.extra, p, 0);
                                let n = to_idx(n_u32);
                                func.extra.push(n_u32);
                                p += 1;
                                for k in 0..n {
                                    func.extra.push(at(&val_map, to_idx(at(callee.extra, p + k, 0)), crate::func::NONE));
                                }
                                p += n;
                            }
                            Field::Target => {
                                func.extra.push(at(callee.extra, p, 0));
                                let n_u32 = at(callee.extra, p + 1, 0);
                                let n = to_idx(n_u32);
                                func.extra.push(n_u32);
                                p += 2;
                                for k in 0..n {
                                    func.extra.push(at(&val_map, to_idx(at(callee.extra, p + k, 0)), crate::func::NONE));
                                }
                                p += n;
                            }
                            Field::Cases => {
                                let n_u32 = at(callee.extra, p, 0);
                                let n = to_idx(n_u32);
                                func.extra.push(n_u32);
                                p += 1;
                                let scrutinee = at(callee.extra, src, 0);
                                let w = to_idx(Ty(at(callee.tys, to_idx(scrutinee), 0)).const_words());
                                for _ in 0..n {
                                    for k in 0..w {
                                        func.extra.push(at(callee.extra, p + k, 0));
                                    }
                                    p += w;
                                    func.extra.push(at(callee.extra, p, 0));
                                    let an_u32 = at(callee.extra, p + 1, 0);
                                    let an = to_idx(an_u32);
                                    func.extra.push(an_u32);
                                    p += 2;
                                    for k in 0..an {
                                        func.extra.push(at(&val_map, to_idx(at(callee.extra, p + k, 0)), crate::func::NONE));
                                    }
                                    p += an;
                                }
                            }
                            Field::Mask | Field::Blocks => {
                                let n_u32 = at(callee.extra, p, 0);
                                let n = to_idx(n_u32);
                                func.extra.push(n_u32);
                                p += 1;
                                for k in 0..n {
                                    func.extra.push(at(callee.extra, p + k, 0));
                                }
                                p += n;
                            }
                            Field::Const => {
                                let w = to_idx(Ty(at(callee.tys, inst, 0)).const_words());
                                for k in 0..w {
                                    func.extra.push(at(callee.extra, p + k, 0));
                                }
                                p += w;
                            }
                            _ => {
                                func.extra.push(at(callee.extra, p, 0));
                                p += 1;
                            }
                        }
                    }
                    dst
                }
            };
            let idx = u32_of(func.ops.len());
            func.ops.push(at(callee.ops, inst, 0));
            func.flags.push(at(callee.flags, inst, 0));
            func.tys.push(at(callee.tys, inst, 0));
            func.data.push(data);
            func.attrs.push(at(callee.attrs, inst, 0));
            func.locs.push(at(callee.locs, inst, 0));
            set_at(&mut val_map, inst, idx);
        }
    }

    crate::verify::verify_in(func.view(), func.allocator()).map_err(SimplifyError::Verify)?;
    sota_optimize_body(func.view(), opts, alloc)
}

/// Telemetry from [`eliminate_self_tail_calls`].
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct SelfTceStats {
    /// Number of direct self-tail-call sites rewritten into entry loop back-edges.
    pub tail_calls_eliminated: u32,
}

/// Output of [`eliminate_self_tail_calls`].
#[derive(Debug)]
pub struct SelfTceOutput<A: Allocator> {
    pub func: Func<A>,
    pub stats: SelfTceStats,
}

/// Pending value operand fixup for forward dominating references across non-RPO blocks.
#[derive(Clone, Copy)]
struct TceValFixup {
    in_extra: bool,
    pos: usize,
    old_val: u32,
}

/// Resolves whether a direct call reference in `self_id` targets `self_id` itself.
fn is_self_target(bodies: &dyn Bodies, self_id: BodyId, reference: RefId) -> bool {
    if let Some((callee_id, _)) = bodies.callee(self_id, reference) {
        return callee_id == self_id;
    }
    matches!(bodies.target(self_id, reference), Some(Target::Func(callee_id)) if callee_id == self_id)
}

/// Returns `true` if `inst` is safe to discard when it appears between a self-tail-call and `Op::Return`.
///
/// Only non-trapping pure operations, compound `Project` extractions, `Assume`, and stack-slot
/// lifetime markers (`SlotLive`/`SlotDead`) are permitted; all memory accesses, calls, trapping
/// divisions/remainders, provenance exposures, and meaning-flagged ops stop the scan.
fn is_tail_trailing_allowed(view: &View<'_>, inst: usize) -> bool {
    let op = view.op(inst);
    if matches!(op, Op::Project | Op::SlotDead | Op::SlotLive | Op::Assume) {
        return true;
    }
    if op.info().flags.meanings_only(view.flags(inst)).0 != 0 {
        return false;
    }
    matches!(
        op,
        Op::Iconst
            | Op::IconstWide
            | Op::Fconst
            | Op::FconstWide
            | Op::Vconst
            | Op::Null
            | Op::Uninit
            | Op::Add
            | Op::Sub
            | Op::Mul
            | Op::Neg
            | Op::And
            | Op::Or
            | Op::Xor
            | Op::Not
            | Op::Shl
            | Op::Lshr
            | Op::Ashr
            | Op::Rotl
            | Op::Rotr
            | Op::Umulhi
            | Op::Smulhi
            | Op::UaddSat
            | Op::SaddSat
            | Op::UsubSat
            | Op::SsubSat
            | Op::Umin
            | Op::Umax
            | Op::Smin
            | Op::Smax
            | Op::UaddOverflow
            | Op::SaddOverflow
            | Op::UsubOverflow
            | Op::SsubOverflow
            | Op::UmulOverflow
            | Op::SmulOverflow
            | Op::Clz
            | Op::Ctz
            | Op::Popcnt
            | Op::Bswap
            | Op::Bitrev
            | Op::Icmp
            | Op::Fcmp
            | Op::Select
            | Op::Zext
            | Op::Sext
            | Op::Trunc
            | Op::Fpext
            | Op::Fptrunc
            | Op::FptoUi
            | Op::FptoSi
            | Op::UitoFp
            | Op::SitoFp
            | Op::Bitcast
            | Op::Fadd
            | Op::Fsub
            | Op::Fmul
            | Op::Fdiv
            | Op::Frem
            | Op::FminNum
            | Op::FmaxNum
            | Op::Fminimum
            | Op::Fmaximum
            | Op::Fcopysign
            | Op::Fneg
            | Op::Fabs
            | Op::Sqrt
            | Op::Floor
            | Op::Ceil
            | Op::Ftrunc
            | Op::Nearest
            | Op::Fround
            | Op::Fma
            | Op::Fmuladd
            | Op::Freeze
            | Op::PtrAdd
            | Op::PtrAddr
            | Op::FromExposed
            | Op::PtrWithAddr
            | Op::FuncAddr
            | Op::DataAddr
            | Op::TlsAddr
            | Op::SlotAddr
            | Op::Splat
            | Op::Extract
            | Op::Insert
            | Op::Shuffle
            | Op::Swizzle
            | Op::ReduceAdd
            | Op::ReduceMul
            | Op::ReduceAnd
            | Op::ReduceOr
            | Op::ReduceXor
            | Op::ReduceUmin
            | Op::ReduceUmax
            | Op::ReduceSmin
            | Op::ReduceSmax
            | Op::ReduceFadd
            | Op::ReduceFmul
            | Op::ReduceFmin
            | Op::ReduceFmax
            | Op::Bitmask
    )
}

/// Returns `true` if any value operand of `inst` is tainted in `taint`.
fn any_value_operand_tainted(view: &View<'_>, inst: usize, taint: &[bool]) -> bool {
    use crate::{
        Ty,
        op::{Field, Layout},
    };

    let op = view.op(inst);
    match op.info().layout {
        Layout::None => false,
        Layout::Data(Field::Value) => at(taint, to_idx(at(view.data, inst, 0)), false),
        Layout::Data(_) => false,
        Layout::Extra(fields) => {
            let src = to_idx(at(view.data, inst, 0));
            let mut p = src;
            for &f in fields {
                match f {
                    Field::Value => {
                        if at(taint, to_idx(at(view.extra, p, 0)), false) {
                            return true;
                        }
                        p += 1;
                    }
                    Field::Values => {
                        let n = to_idx(at(view.extra, p, 0));
                        p += 1;
                        for k in 0..n {
                            if at(taint, to_idx(at(view.extra, p + k, 0)), false) {
                                return true;
                            }
                        }
                        p += n;
                    }
                    Field::Target => {
                        let n = to_idx(at(view.extra, p + 1, 0));
                        p += 2;
                        for k in 0..n {
                            if at(taint, to_idx(at(view.extra, p + k, 0)), false) {
                                return true;
                            }
                        }
                        p += n;
                    }
                    Field::Cases => {
                        let n = to_idx(at(view.extra, p, 0));
                        p += 1;
                        let scrutinee = at(view.extra, src, 0);
                        let w = to_idx(Ty(at(view.tys, to_idx(scrutinee), 0)).const_words());
                        for _ in 0..n {
                            p += w;
                            let an = to_idx(at(view.extra, p + 1, 0));
                            p += 2;
                            for k in 0..an {
                                if at(taint, to_idx(at(view.extra, p + k, 0)), false) {
                                    return true;
                                }
                            }
                            p += an;
                        }
                    }
                    Field::Mask | Field::Blocks => {
                        let n = to_idx(at(view.extra, p, 0));
                        p += 1 + n;
                    }
                    Field::Const => {
                        let w = to_idx(Ty(at(view.tys, inst, 0)).const_words());
                        p += w;
                    }
                    Field::Imm
                    | Field::Offset
                    | Field::IntCond
                    | Field::FloatCond
                    | Field::Rmw
                    | Field::Ref
                    | Field::Sig
                    | Field::Slot
                    | Field::Intrinsic
                    | Field::Asm
                    | Field::Block => {
                        p += 1;
                    }
                }
            }
            false
        }
    }
}

/// Sound escape check for local `StackSlot`s: returns `true` if any `Op::SlotAddr` (or pointer
/// derived from it through `PtrAdd`/`PtrWithAddr`/`Freeze`/`Select`/block-param edges) could
/// escape into memory, a callee, a return value, an address cast, or an entry parameter.
///
/// Local stack slots used strictly as address operands of `Load`/`Store`/`Memcpy`/`Memmove`/
/// `Memset`/`Memcmp`/atomics/`MaskedLoad`/`MaskedStore` (such as `slice::swap`'s temporary
/// `StackSlot` in `Op::Memmove`) do NOT escape and remain 100% sound to reuse across loop
/// iterations after tail-call elimination.
fn any_slot_escapes<A: Allocator + Copy>(view: &View<'_>, alloc: A) -> bool {
    use crate::Block;

    if view.slots.is_empty() {
        return false;
    }
    let len = view.len();
    let mut taint: OwnVec<bool, A> = OwnVec::from_elem_in(false, len, alloc);
    let mut has_slot_addr = false;
    for inst in 0..len {
        if view.op(inst) == Op::SlotAddr {
            set_at(&mut taint, inst, true);
            has_slot_addr = true;
        }
    }
    if !has_slot_addr {
        return false;
    }

    // Phase 1: monotone fixed-point propagation of slot-derived pointers (bounded by `len + 1`).
    for _ in 0..=len {
        let mut changed = false;
        for inst in 0..len {
            match view.op(inst) {
                Op::PtrAdd | Op::PtrWithAddr => {
                    let mut o = view.operands(inst);
                    let base = o.value().index();
                    if at(&taint, base, false) && !at(&taint, inst, false) {
                        set_at(&mut taint, inst, true);
                        changed = true;
                    }
                }
                Op::Freeze => {
                    let v = to_idx(at(view.data, inst, 0));
                    if at(&taint, v, false) && !at(&taint, inst, false) {
                        set_at(&mut taint, inst, true);
                        changed = true;
                    }
                }
                Op::Select => {
                    let mut o = view.operands(inst);
                    let _cond = o.value();
                    let a = o.value().index();
                    let b = o.value().index();
                    if (at(&taint, a, false) || at(&taint, b, false)) && !at(&taint, inst, false) {
                        set_at(&mut taint, inst, true);
                        changed = true;
                    }
                }
                Op::Jump => {
                    let mut o = view.operands(inst);
                    let target = o.target();
                    let params = view.params(target.block);
                    for (k, arg) in target.args.iter().enumerate() {
                        if at(&taint, arg.index(), false) {
                            let p_inst = params.start + k;
                            if !at(&taint, p_inst, false) {
                                set_at(&mut taint, p_inst, true);
                                changed = true;
                            }
                        }
                    }
                }
                Op::Br => {
                    let mut o = view.operands(inst);
                    let _cond = o.value();
                    for target in [o.target(), o.target()] {
                        let params = view.params(target.block);
                        for (k, arg) in target.args.iter().enumerate() {
                            if at(&taint, arg.index(), false) {
                                let p_inst = params.start + k;
                                if !at(&taint, p_inst, false) {
                                    set_at(&mut taint, p_inst, true);
                                    changed = true;
                                }
                            }
                        }
                    }
                }
                Op::Switch => {
                    let mut o = view.operands(inst);
                    let scrut = o.value();
                    let def = o.target();
                    let def_params = view.params(def.block);
                    for (k, arg) in def.args.iter().enumerate() {
                        if at(&taint, arg.index(), false) {
                            let p_inst = def_params.start + k;
                            if !at(&taint, p_inst, false) {
                                set_at(&mut taint, p_inst, true);
                                changed = true;
                            }
                        }
                    }
                    for (_, target) in o.cases(scrut) {
                        let params = view.params(target.block);
                        for (k, arg) in target.args.iter().enumerate() {
                            if at(&taint, arg.index(), false) {
                                let p_inst = params.start + k;
                                if !at(&taint, p_inst, false) {
                                    set_at(&mut taint, p_inst, true);
                                    changed = true;
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        if !changed {
            break;
        }
    }

    // If any back-edge carried a slot pointer into an entry param, refuse conservatively.
    for p in view.params(Block(0)) {
        if at(&taint, p, false) {
            return true;
        }
    }

    // Phase 2: verify every use of every slot-derived value is strictly a non-escaping address use.
    for inst in 0..len {
        match view.op(inst) {
            Op::Param
            | Op::SlotAddr
            | Op::PtrAdd
            | Op::PtrWithAddr
            | Op::Freeze
            | Op::Select
            | Op::Jump
            | Op::Br
            | Op::Switch
            | Op::Load
            | Op::AtomicLoad
            | Op::Memcpy
            | Op::Memmove
            | Op::Memset
            | Op::Memcmp
            | Op::Icmp => {}
            Op::Store | Op::AtomicStore => {
                let mut o = view.operands(inst);
                let val = o.value();
                if at(&taint, val.index(), false) {
                    return true;
                }
            }
            Op::AtomicRmw => {
                let mut o = view.operands(inst);
                let _rmw = o.rmw();
                let _ptr = o.value();
                let val = o.value();
                if at(&taint, val.index(), false) {
                    return true;
                }
            }
            Op::Cmpxchg => {
                let mut o = view.operands(inst);
                let _ptr = o.value();
                let exp = o.value();
                let new = o.value();
                if at(&taint, exp.index(), false) || at(&taint, new.index(), false) {
                    return true;
                }
            }
            Op::MaskedLoad => {
                let mut o = view.operands(inst);
                let _ptr = o.value();
                let _mask = o.value();
                let pass = o.value();
                if at(&taint, pass.index(), false) {
                    return true;
                }
            }
            Op::MaskedStore => {
                let mut o = view.operands(inst);
                let val = o.value();
                if at(&taint, val.index(), false) {
                    return true;
                }
            }
            _ => {
                if any_value_operand_tainted(view, inst, &taint) {
                    return true;
                }
            }
        }
    }
    false
}

/// Checks whether `call_inst` (an `Op::Call` inside a block ending with `Op::Return` at `term`)
/// is a sound direct self-tail-call matching `self_id`'s entry signature and return values.
fn is_eligible_self_tail_call(
    view: &View<'_>,
    call_inst: usize,
    term: usize,
    self_id: BodyId,
    bodies: &dyn Bodies,
    entry_params: &core::ops::Range<usize>,
    own_sig: &crate::func::Sig<'_>,
) -> bool {
    use crate::{
        Ty,
        func::{RefKind, SigRec},
    };

    let mut o = view.operands(call_inst);
    let callee_ref = o.reference();
    let call_sig_id = o.sig();
    let call_args = o.values();

    if RefKind::from_word(view.reference(callee_ref).kind) != Some(RefKind::Func) {
        return false;
    }
    if !is_self_target(bodies, self_id, callee_ref) {
        return false;
    }

    let call_sig = view.sig(call_sig_id);
    if (call_sig.bits & SigRec::VARIADIC) != 0 || call_sig.conv != own_sig.conv {
        return false;
    }
    if call_args.len() != entry_params.len() {
        return false;
    }
    for (k, param_inst) in (entry_params.start..entry_params.end).enumerate() {
        if view.ty(call_args.get(k)) != Ty(at(view.tys, param_inst, 0)) {
            return false;
        }
    }
    if call_sig.rets.len() != own_sig.rets.len()
        || !call_sig.rets.iter().map(|r| r.ty).eq(own_sig.rets.iter().map(|r| r.ty))
    {
        return false;
    }

    let mut ro = view.operands(term);
    let ret_vals = ro.values();
    if ret_vals.len() != own_sig.rets.len() {
        return false;
    }
    match ret_vals.len() {
        0 => Ty(at(view.tys, call_inst, 0)) == Ty::NONE,
        1 => ret_vals.get(0).index() == call_inst,
        k => {
            for idx in 0..k {
                let rv = ret_vals.get(idx).index();
                if rv <= call_inst || rv >= term || view.op(rv) != Op::Project {
                    return false;
                }
                let mut po = view.operands(rv);
                let src = po.value();
                let member = po.imm();
                if src.index() != call_inst || member != u32_of(idx) {
                    return false;
                }
            }
            true
        }
    }
}

fn emit_remapped_inst<A: Allocator>(
    view: &View<'_>,
    inst: usize,
    blk_shift: u32,
    val_map: &[u32],
    fixups: &mut OwnVec<TceValFixup, A>,
    func: &mut Func<A>,
) -> u32 {
    use crate::{
        Ty,
        op::{Field, Layout},
    };

    let op = view.op(inst);
    let new_idx = func.ops.len();
    let data = match op.info().layout {
        Layout::None => 0,
        Layout::Data(Field::Value) => {
            let old_v = at(view.data, inst, 0);
            let mapped = at(val_map, to_idx(old_v), crate::func::NONE);
            if mapped == crate::func::NONE {
                fixups.push(TceValFixup { in_extra: false, pos: new_idx, old_val: old_v });
            }
            mapped
        }
        Layout::Data(Field::Block) => at(view.data, inst, 0).wrapping_add(blk_shift),
        Layout::Data(_) => at(view.data, inst, 0),
        Layout::Extra(fields) => {
            let src = to_idx(at(view.data, inst, 0));
            let dst = u32_of(func.extra.len());
            let mut p = src;
            for &f in fields {
                match f {
                    Field::Value => {
                        let old_v = at(view.extra, p, 0);
                        let mapped = at(val_map, to_idx(old_v), crate::func::NONE);
                        let pos = func.extra.len();
                        func.extra.push(mapped);
                        if mapped == crate::func::NONE {
                            fixups.push(TceValFixup { in_extra: true, pos, old_val: old_v });
                        }
                        p += 1;
                    }
                    Field::Block => {
                        func.extra.push(at(view.extra, p, 0).wrapping_add(blk_shift));
                        p += 1;
                    }
                    Field::Values => {
                        let n_u32 = at(view.extra, p, 0);
                        let n = to_idx(n_u32);
                        func.extra.push(n_u32);
                        p += 1;
                        for k in 0..n {
                            let old_v = at(view.extra, p + k, 0);
                            let mapped = at(val_map, to_idx(old_v), crate::func::NONE);
                            let pos = func.extra.len();
                            func.extra.push(mapped);
                            if mapped == crate::func::NONE {
                                fixups.push(TceValFixup { in_extra: true, pos, old_val: old_v });
                            }
                        }
                        p += n;
                    }
                    Field::Target => {
                        func.extra.push(at(view.extra, p, 0).wrapping_add(blk_shift));
                        let n_u32 = at(view.extra, p + 1, 0);
                        let n = to_idx(n_u32);
                        func.extra.push(n_u32);
                        p += 2;
                        for k in 0..n {
                            let old_v = at(view.extra, p + k, 0);
                            let mapped = at(val_map, to_idx(old_v), crate::func::NONE);
                            let pos = func.extra.len();
                            func.extra.push(mapped);
                            if mapped == crate::func::NONE {
                                fixups.push(TceValFixup { in_extra: true, pos, old_val: old_v });
                            }
                        }
                        p += n;
                    }
                    Field::Cases => {
                        let n_u32 = at(view.extra, p, 0);
                        let n = to_idx(n_u32);
                        func.extra.push(n_u32);
                        p += 1;
                        let scrutinee = at(view.extra, src, 0);
                        let w = to_idx(Ty(at(view.tys, to_idx(scrutinee), 0)).const_words());
                        for _ in 0..n {
                            for k in 0..w {
                                func.extra.push(at(view.extra, p + k, 0));
                            }
                            p += w;
                            func.extra.push(at(view.extra, p, 0).wrapping_add(blk_shift));
                            let an_u32 = at(view.extra, p + 1, 0);
                            let an = to_idx(an_u32);
                            func.extra.push(an_u32);
                            p += 2;
                            for k in 0..an {
                                let old_v = at(view.extra, p + k, 0);
                                let mapped = at(val_map, to_idx(old_v), crate::func::NONE);
                                let pos = func.extra.len();
                                func.extra.push(mapped);
                                if mapped == crate::func::NONE {
                                    fixups.push(TceValFixup { in_extra: true, pos, old_val: old_v });
                                }
                            }
                            p += an;
                        }
                    }
                    Field::Blocks => {
                        let n_u32 = at(view.extra, p, 0);
                        let n = to_idx(n_u32);
                        func.extra.push(n_u32);
                        p += 1;
                        for k in 0..n {
                            func.extra.push(at(view.extra, p + k, 0).wrapping_add(blk_shift));
                        }
                        p += n;
                    }
                    Field::Mask => {
                        let n_u32 = at(view.extra, p, 0);
                        let n = to_idx(n_u32);
                        func.extra.push(n_u32);
                        p += 1;
                        for k in 0..n {
                            func.extra.push(at(view.extra, p + k, 0));
                        }
                        p += n;
                    }
                    Field::Const => {
                        let w = to_idx(Ty(at(view.tys, inst, 0)).const_words());
                        for k in 0..w {
                            func.extra.push(at(view.extra, p + k, 0));
                        }
                        p += w;
                    }
                    Field::Imm
                    | Field::Offset
                    | Field::IntCond
                    | Field::FloatCond
                    | Field::Rmw
                    | Field::Ref
                    | Field::Sig
                    | Field::Slot
                    | Field::Intrinsic
                    | Field::Asm => {
                        func.extra.push(at(view.extra, p, 0));
                        p += 1;
                    }
                }
            }
            dst
        }
    };
    func.ops.push(at(view.ops, inst, 0));
    func.flags.push(at(view.flags, inst, 0));
    func.tys.push(at(view.tys, inst, 0));
    func.data.push(data);
    func.attrs.push(at(view.attrs, inst, 0));
    func.locs.push(at(view.locs, inst, 0));
    u32_of(new_idx)
}

fn emit_tail_jump<A: Allocator>(
    view: &View<'_>,
    call_inst: usize,
    loop_header_blk: u32,
    val_map: &[u32],
    fixups: &mut OwnVec<TceValFixup, A>,
    func: &mut Func<A>,
) {
    use crate::{Flags, Ty};

    let mut o = view.operands(call_inst);
    let _callee_ref = o.reference();
    let _sig = o.sig();
    let args = o.values();

    let extra_at = u32_of(func.extra.len());
    func.extra.push(loop_header_blk);
    func.extra.push(u32_of(args.len()));
    for arg in args.iter() {
        let old_v = arg.0;
        let mapped = at(val_map, to_idx(old_v), crate::func::NONE);
        let pos = func.extra.len();
        func.extra.push(mapped);
        if mapped == crate::func::NONE {
            fixups.push(TceValFixup { in_extra: true, pos, old_val: old_v });
        }
    }
    func.ops.push(op_byte(Op::Jump));
    func.flags.push(Flags::NONE.0);
    func.tys.push(Ty::NONE.0);
    func.data.push(extra_at);
    func.attrs.push(0);
    func.locs.push(at(view.locs, call_inst, 0));
}

/// Internal zero-clone helper: returns `Ok(None)` when no eligible self-tail-call sites exist.
fn eliminate_self_tail_calls_if_any<A: Allocator + Copy>(
    view: View<'_>,
    self_id: BodyId,
    bodies: &dyn Bodies,
    alloc: A,
) -> Result<Option<(Func<A>, SelfTceStats)>, SimplifyError> {
    use crate::{
        Block, BlockKind, Flags, Ty,
        func::{ParamKind, SigId, SigRec},
        verify,
    };

    let old_blocks = view.blocks();
    let len = view.len();
    if old_blocks == 0 || len == 0 || view.block_kind(Block(0)) != BlockKind::Normal {
        return Ok(None);
    }

    let own_sig = view.sig(SigId(view.head.sig));
    if (own_sig.bits & SigRec::VARIADIC) != 0 {
        return Ok(None);
    }
    if own_sig.params.iter().any(|p| ParamKind::from_byte(p.kind) == Some(ParamKind::Byval)) {
        return Ok(None);
    }

    // Refuse coroutines and frame-introspection ops; also collect slots closed by `SlotDead`.
    let mut has_slot_dead: OwnVec<bool, A> = OwnVec::from_elem_in(false, view.slots.len(), alloc);
    for inst in 0..len {
        match view.op(inst) {
            Op::Suspend | Op::FrameAddress | Op::ReturnAddress | Op::StackPointer => return Ok(None),
            Op::SlotDead => set_at(&mut has_slot_dead, to_idx(at(view.data, inst, 0)), true),
            _ => {}
        }
    }

    let entry_range = view.block(Block(0));
    let entry_params = view.params(Block(0));
    let mut tail_call_at: OwnVec<u32, A> = OwnVec::from_elem_in(crate::func::NONE, old_blocks, alloc);
    let mut sites = 0u32;

    for b in 0..old_blocks {
        let blk = Block(u32_of(b));
        let range = view.block(blk);
        let term = range.end - 1;
        if view.op(term) != Op::Return {
            continue;
        }
        let params_end = view.params(blk).end;
        for inst in (params_end..term).rev() {
            if view.op(inst) == Op::Call {
                if is_eligible_self_tail_call(&view, inst, term, self_id, bodies, &entry_params, &own_sig) {
                    set_at(&mut tail_call_at, b, u32_of(inst));
                    sites += 1;
                }
                break;
            }
            if !is_tail_trailing_allowed(&view, inst) {
                break;
            }
        }
    }

    if sites == 0 {
        return Ok(None);
    }

    // Soundness check: if any local StackSlot address can escape to memory, a callee, or a return,
    // skip TCE conservatively so distinct recursive frames never alias a single loop frame.
    if any_slot_escapes(&view, alloc) {
        return Ok(None);
    }

    let mut func = view.to_func_in(alloc);
    func.ops.clear();
    func.flags.clear();
    func.tys.clear();
    func.data.clear();
    func.extra.clear();
    func.attrs.clear();
    func.locs.clear();
    func.block_starts.clear();
    func.block_kinds.clear();

    let mut val_map: OwnVec<u32, A> = OwnVec::from_elem_in(crate::func::NONE, len, alloc);
    let mut fixups: OwnVec<TceValFixup, A> = OwnVec::new_in(alloc);

    // New Block(0): tiny entry pre-header forwarding entry params to `loop_header` (Block(1)).
    let param_count = entry_params.len();
    func.block_starts.push(0);
    func.block_kinds.push(at(view.block_kinds, 0, 0));
    for inst in entry_params.start..entry_params.end {
        func.ops.push(at(view.ops, inst, 0));
        func.flags.push(at(view.flags, inst, 0));
        func.tys.push(at(view.tys, inst, 0));
        func.data.push(at(view.data, inst, 0));
        func.attrs.push(at(view.attrs, inst, 0));
        func.locs.push(at(view.locs, inst, 0));
    }
    let pre_jump_extra = u32_of(func.extra.len());
    func.extra.push(1); // Block(1) = loop_header
    func.extra.push(u32_of(param_count));
    for k in 0..param_count {
        func.extra.push(u32_of(k));
    }
    func.ops.push(op_byte(Op::Jump));
    func.flags.push(Flags::NONE.0);
    func.tys.push(Ty::NONE.0);
    func.data.push(pre_jump_extra);
    func.attrs.push(0);
    func.locs.push(0);

    // New Block(1): `loop_header` receiving original entry params and holding original Block(0)'s body.
    func.block_starts.push(u32_of(func.ops.len()));
    func.block_kinds.push(at(view.block_kinds, 0, 0));
    for inst in entry_params.start..entry_params.end {
        let idx = u32_of(func.ops.len());
        func.ops.push(at(view.ops, inst, 0));
        func.flags.push(at(view.flags, inst, 0));
        func.tys.push(at(view.tys, inst, 0));
        func.data.push(at(view.data, inst, 0));
        func.attrs.push(at(view.attrs, inst, 0));
        func.locs.push(at(view.locs, inst, 0));
        set_at(&mut val_map, inst, idx);
    }
    for (slot_idx, &had_dead) in has_slot_dead.iter().enumerate() {
        if had_dead {
            func.ops.push(op_byte(Op::SlotLive));
            func.flags.push(Flags::NONE.0);
            func.tys.push(Ty::NONE.0);
            func.data.push(u32_of(slot_idx));
            func.attrs.push(0);
            func.locs.push(0);
        }
    }
    let tc0 = at(&tail_call_at, 0, crate::func::NONE);
    if tc0 == crate::func::NONE {
        for inst in entry_params.end..entry_range.end {
            let idx = emit_remapped_inst(&view, inst, 1, &val_map, &mut fixups, &mut func);
            set_at(&mut val_map, inst, idx);
        }
    } else {
        let call_inst = to_idx(tc0);
        for inst in entry_params.end..call_inst {
            let idx = emit_remapped_inst(&view, inst, 1, &val_map, &mut fixups, &mut func);
            set_at(&mut val_map, inst, idx);
        }
        emit_tail_jump(&view, call_inst, 1, &val_map, &mut fixups, &mut func);
    }

    // Remaining blocks `1..old_blocks` shifted to `2..=old_blocks`.
    for b in 1..old_blocks {
        func.block_starts.push(u32_of(func.ops.len()));
        func.block_kinds.push(at(view.block_kinds, b, 0));
        let range = view.block(Block(u32_of(b)));
        let tc = at(&tail_call_at, b, crate::func::NONE);
        if tc == crate::func::NONE {
            for inst in range {
                let idx = emit_remapped_inst(&view, inst, 1, &val_map, &mut fixups, &mut func);
                set_at(&mut val_map, inst, idx);
            }
        } else {
            let call_inst = to_idx(tc);
            for inst in range.start..call_inst {
                let idx = emit_remapped_inst(&view, inst, 1, &val_map, &mut fixups, &mut func);
                set_at(&mut val_map, inst, idx);
            }
            emit_tail_jump(&view, call_inst, 1, &val_map, &mut fixups, &mut func);
        }
    }

    // Patch any forward cross-block dominating value references.
    for fix in fixups.iter() {
        let mapped = at(&val_map, to_idx(fix.old_val), crate::func::NONE);
        if mapped == crate::func::NONE {
            return Err(SimplifyError::Refused {
                inst: to_idx(fix.old_val),
                what: "unmapped value in self-tail-call elimination",
            });
        }
        if fix.in_extra {
            set_at(&mut func.extra, fix.pos, mapped);
        } else {
            set_at(&mut func.data, fix.pos, mapped);
        }
    }

    crate::verify::verify_in(func.view(), func.allocator()).map_err(SimplifyError::Verify)?;
    Ok(Some((func, SelfTceStats { tail_calls_eliminated: sites })))
}

/// Converts direct self-tail-calls (`Op::Call` to `self_id` whose exact results feed `Op::Return`
/// with no intervening side effects and no escaping local `StackSlot`s) into `Op::Jump` back-edges
/// to a newly created entry loop header (`Block(1)`).
pub fn eliminate_self_tail_calls<A: Allocator + Copy>(
    view: View<'_>,
    self_id: BodyId,
    bodies: &dyn Bodies,
    alloc: A,
) -> Result<SelfTceOutput<A>, SimplifyError> {
    match eliminate_self_tail_calls_if_any(view, self_id, bodies, alloc)? {
        Some((func, stats)) => Ok(SelfTceOutput { func, stats }),
        None => Ok(SelfTceOutput { func: view.to_func_in(alloc), stats: SelfTceStats::default() }),
    }
}

/// Internal `Bodies` overlay over [`OptimizedBody`]s that resolves transitive `ImportedRef` chains
/// created during bottom-up inlining so post-inline passes (like [`eliminate_self_tail_calls`])
/// resolve self-calls exposed by inlining.
struct OptimizedBodies<'a, A: Allocator> {
    bodies: &'a [OptimizedBody<A>],
    id_to_index: &'a Map<BodyId, usize, FixedState, A>,
    external: &'a dyn Bodies,
}

impl<A: Allocator> OptimizedBodies<'_, A> {
    fn resolve_origin(&self, mut from: BodyId, mut reference: RefId) -> (BodyId, RefId) {
        for _ in 0..32 {
            let Some(&idx) = self.id_to_index.get(&from) else {
                break;
            };
            let Some(ob) = self.bodies.get(idx) else {
                break;
            };
            let Some(imp) = ob.imported_refs.iter().find(|r| r.caller_ref == reference) else {
                break;
            };
            from = imp.source_body;
            reference = imp.source_ref;
        }
        (from, reference)
    }
}

impl<A: Allocator> Bodies for OptimizedBodies<'_, A> {
    fn callee(&self, from: BodyId, reference: RefId) -> Option<(BodyId, View<'_>)> {
        let (orig_from, orig_ref) = self.resolve_origin(from, reference);
        let (callee_id, ext_view) = self.external.callee(orig_from, orig_ref)?;
        if let Some(&idx) = self.id_to_index.get(&callee_id)
            && let Some(ob) = self.bodies.get(idx)
        {
            return Some((callee_id, ob.func.view()));
        }
        Some((callee_id, ext_view))
    }

    fn target(&self, from: BodyId, reference: RefId) -> Option<Target> {
        let (orig_from, orig_ref) = self.resolve_origin(from, reference);
        self.external.target(orig_from, orig_ref)
    }

    fn datum(&self, id: DatumId) -> Option<Datum<'_>> {
        self.external.datum(id)
    }

    fn body(&self, id: BodyId) -> Option<View<'_>> {
        if let Some(&idx) = self.id_to_index.get(&id) {
            return self.bodies.get(idx).map(|b| b.func.view());
        }
        self.external.body(id)
    }

    fn position(&self, item: u128, site: u32) -> Option<(u32, u32)> {
        self.external.position(item, site)
    }
}

/// Full SOTA Cross-Function Bottom-Up ThinLTO:
///
/// 1. Runs PGO mono/bimorphic `dyn Trait` / fn-pointer [`speculative_devirt`] across the module
///    BEFORE Tarjan SCC condensation, exposing newly direct `caller -> hot_dyn_impl` edges to the
///    bottom-up call graph.
/// 2. Condenses the enriched call graph with iterative Tarjan SCCs (callees before callers).
/// 3. For each SCC bottom-up: simplifies callees, runs PGO-guided multi-function inlining
///    (`inline_guided_with_cache`, splicing chains `a -> b -> c` and devirtualized `dyn` methods),
///    eliminates direct self-tail-calls into loops ([`eliminate_self_tail_calls`]), and runs the
///    full cross-function SOTA pipeline (`SROA -> aegraph -> BCE -> Loop Unswitching -> Memory
///    LICM/DSE -> 4x SIMD Auto-Vectorization -> 4x Scalar Unrolling -> Superopt`) over the merged
///    multi-function body!
pub fn lto_optimize_module_sota<A: Allocator + Copy>(
    mut module: OwnVec<ModuleBody<A>, A>,
    external: &dyn Bodies,
    weights: &dyn SiteWeights,
    indirect: &dyn IndirectProfiles,
    policy: InlinePolicy,
    opts: SotaLtoOpts,
    alloc: A,
) -> Result<ModuleLto<A>, SimplifyError> {
    // Phase 0: Speculatively devirtualize hot `dyn Trait` / fn-pointer call sites BEFORE call-graph
    // construction so newly direct calls enter Tarjan SCC order and get inlined bottom-up!
    for mb in module.iter_mut() {
        let targets = indirect.targets(mb.id);
        if !targets.is_empty() {
            let dev = speculative_devirt(mb.func.view(), targets, opts.min_devirt_hit_percent, alloc)?;
            if dev.stats.sites_devirtualized > 0 {
                mb.func = dev.func;
            }
        }
    }

    // Phase 1: Bottom-up Tarjan SCC multi-function PGO inlining (reusing `lto_optimize_module`).
    let mut lto = lto_optimize_module(module, external, weights, policy, alloc)?;

    let mut id_to_index: Map<BodyId, usize, FixedState, A> = Map::new(MapOpts {
        hasher: FixedState,
        alloc,
        capacity: Some(lto.bodies.len()),
    });
    for (idx, ob) in lto.bodies.iter().enumerate() {
        id_to_index.insert(ob.id, idx);
    }

    // Phase 2: Self-tail-call elimination (TCE) + full cross-function SOTA optimization over every
    // merged body so newly formed loops get full SROA/aegraph/BCE/unswitch/memory-LICM/SIMD/unroll/
    // superopt treatment!
    for idx in 0..lto.bodies.len() {
        let pre_tce = if let Some(ob) = lto.bodies.get(idx) {
            let overlay = OptimizedBodies {
                bodies: &lto.bodies,
                id_to_index: &id_to_index,
                external,
            };
            eliminate_self_tail_calls_if_any(ob.func.view(), ob.id, &overlay, alloc)?
        } else {
            None
        };
        if let Some(ob) = lto.bodies.get_mut(idx) {
            if let Some((tce_func, _)) = pre_tce {
                ob.func = tce_func;
            }
            ob.func = sota_optimize_body(ob.func.view(), opts, alloc)?;
        }

        let post_tce = if let Some(ob) = lto.bodies.get(idx) {
            let overlay = OptimizedBodies {
                bodies: &lto.bodies,
                id_to_index: &id_to_index,
                external,
            };
            eliminate_self_tail_calls_if_any(ob.func.view(), ob.id, &overlay, alloc)?
        } else {
            None
        };
        if let Some(ob) = lto.bodies.get_mut(idx) {
            if let Some((tce_func, _)) = post_tce {
                ob.func = sota_optimize_body(tce_func.view(), opts, alloc)?;
            }
            ob.summary = FnSummary::compute(ob.func.view(), &alloc);
            ob.summary_changed = ob.summary.summary_hash != ob.prev_summary.summary_hash;
        }
    }

    Ok(lto)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        BlockKind, Flags, IntCond, Ty,
        builder::{Builder, Full, MemAttrs, Param, Variadic},
        eval::{self, Outcome, Val},
        func::{Conv, RefKind, SlotKind},
        inline::NoWeights,
        verify,
    };

    struct SingleSelfBodies<'a> {
        id: BodyId,
        ref_id: RefId,
        view: View<'a>,
    }

    impl Bodies for SingleSelfBodies<'_> {
        fn callee(&self, from: BodyId, reference: RefId) -> Option<(BodyId, View<'_>)> {
            (from == self.id && reference == self.ref_id).then_some((self.id, self.view))
        }

        fn target(&self, from: BodyId, reference: RefId) -> Option<Target> {
            (from == self.id && reference == self.ref_id).then_some(Target::Func(self.id))
        }
    }

    struct TwoBodyBank<'a> {
        driver_id: BodyId,
        driver_to_dc: RefId,
        dc_id: BodyId,
        dc_self_ref: RefId,
        dc_view: View<'a>,
    }

    impl Bodies for TwoBodyBank<'_> {
        fn callee(&self, from: BodyId, reference: RefId) -> Option<(BodyId, View<'_>)> {
            if (from == self.driver_id && reference == self.driver_to_dc)
                || (from == self.dc_id && reference == self.dc_self_ref)
            {
                return Some((self.dc_id, self.dc_view));
            }
            None
        }

        fn target(&self, from: BodyId, reference: RefId) -> Option<Target> {
            if (from == self.driver_id && reference == self.driver_to_dc)
                || (from == self.dc_id && reference == self.dc_self_ref)
            {
                return Some(Target::Func(self.dc_id));
            }
            None
        }
    }

    #[test]
    fn self_tce_turns_tail_recursive_sum_into_loop_and_matches_eval() {
        let alloc = blaze_own::testing::heap().alloc();
        let mut b = Builder::<_, Full>::new_in(64, alloc);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let self_ref = b.reference(RefKind::Func, 0);
        let [entry, done, recurse] = [(); 3].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let n = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let zero = b.iconst(Ty::I64, 0);
        let is_zero = b.icmp(IntCond::Eq, n, zero);
        b.br(Flags::NONE, is_zero, (done, &[]), (recurse, &[]));

        b.begin(done);
        b.ret(&[acc]);

        b.begin(recurse);
        let one = b.iconst(Ty::I64, 1);
        let next_n = b.binary(Op::Sub, Flags::NONE, n, one);
        let next_acc = b.binary(Op::Add, Flags::NONE, acc, n);
        let res = b.call(Flags::NONE, self_ref, sig, &[next_n, next_acc]);
        b.ret(&[res]);

        let func = b.finish_checked().unwrap();
        let self_id = BodyId([1u8; 32]);
        let bank = SingleSelfBodies { id: self_id, ref_id: self_ref, view: func.view() };

        let out = eliminate_self_tail_calls(func.view(), self_id, &bank, alloc).unwrap();
        assert_eq!(out.stats.tail_calls_eliminated, 1);
        verify(out.func.view()).unwrap();

        let call_count = (0..out.func.view().len())
            .filter(|&i| out.func.view().op(i) == Op::Call)
            .count();
        assert_eq!(call_count, 0, "expected direct self-tail-call eliminated into a loop");

        for &(nv, accv) in &[(0u64, 5u64), (1, 0), (10, 0), (25, 100)] {
            let args = [Val::Int(u128::from(nv)), Val::Int(u128::from(accv))];
            let before = eval::run_calls_in(func.view(), self_id, &args, 100_000, &bank, alloc).unwrap();
            let after = eval::run_in(out.func.view(), &args, 100_000, alloc).unwrap();
            assert_eq!(before, after, "tail-recursive sum mismatch on ({nv}, {accv})");
            let expected = accv + nv * (nv + 1) / 2;
            match after {
                Outcome::Returned(vals) => assert_eq!(vals.as_slice(), &[Val::Int(u128::from(expected))]),
                other => panic!("expected Returned, got {other:?}"),
            }
        }

        // Also verify `lto_optimize_module` itself eliminates the direct self-tail-call.
        let mut plain_module = OwnVec::with_capacity_in(1, alloc);
        plain_module.push(ModuleBody { id: self_id, func: func.view().to_func_in(alloc) });
        let plain_lto = lto_optimize_module(
            plain_module,
            &bank,
            &NoWeights,
            InlinePolicy::default(),
            alloc,
        )
        .unwrap();
        let plain_view = plain_lto.bodies.first().expect("one body").func.view();
        verify(plain_view).unwrap();
        let plain_calls = (0..plain_view.len()).filter(|&i| plain_view.op(i) == Op::Call).count();
        assert_eq!(plain_calls, 0, "expected lto_optimize_module to eliminate self-tail-call");
        let after_plain = eval::run_in(plain_view, &[Val::Int(25), Val::Int(100)], 100_000, alloc).unwrap();
        assert_eq!(
            after_plain,
            eval::run_calls_in(func.view(), self_id, &[Val::Int(25), Val::Int(100)], 100_000, &bank, alloc).unwrap()
        );

        // Also verify `lto_optimize_module_sota` eliminates the self-tail-call end-to-end.
        let mut module = OwnVec::with_capacity_in(1, alloc);
        module.push(ModuleBody { id: self_id, func: func.view().to_func_in(alloc) });
        let lto = lto_optimize_module_sota(
            module,
            &bank,
            &NoWeights,
            &NoIndirectProfiles,
            InlinePolicy::default(),
            SotaLtoOpts::default(),
            alloc,
        )
        .unwrap();
        let lto_view = lto.bodies[0].func.view();
        verify(lto_view).unwrap();
        let lto_calls = (0..lto_view.len()).filter(|&i| lto_view.op(i) == Op::Call).count();
        assert_eq!(lto_calls, 0, "expected lto_optimize_module_sota to eliminate self-tail-call");
        let after_lto = eval::run_in(lto_view, &[Val::Int(25), Val::Int(100)], 100_000, alloc).unwrap();
        assert_eq!(
            after_lto,
            eval::run_calls_in(func.view(), self_id, &[Val::Int(25), Val::Int(100)], 100_000, &bank, alloc).unwrap()
        );
    }

    #[test]
    fn self_tce_divide_and_conquer_keeps_first_call_eliminates_tail_call_with_memmove_slot() {
        let alloc = blaze_own::testing::heap().alloc();

        // Build `dc(ptr: PTR, lo: I64, hi: I64) -> ()`:
        //   if lo >= hi { return; }
        //   swap(ptr[lo], ptr[hi]) using a local non-escaping 8-byte StackSlot + Op::Memmove;
        //   mid = (lo + hi) >> 1;
        //   dc(ptr, lo, mid);      // non-tail self-call (MUST remain Op::Call)
        //   dc(ptr, mid + 1, hi);  // tail self-call (MUST become loop back-edge)
        //   slot_dead(tmp);
        //   return;
        let mut b = Builder::<_, Full>::new_in(64, alloc);
        let dc_sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::PTR), Param::of(Ty::I64), Param::of(Ty::I64)],
            &[],
        );
        b.set_signature(dc_sig);
        let dc_self_ref = b.reference(RefKind::Func, 0);
        let tmp_slot = b.slot(8, 3, SlotKind::Temp);
        let [entry, done, work] = [(); 3].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let ptr = b.param(Ty::PTR);
        let lo = b.param(Ty::I64);
        let hi = b.param(Ty::I64);
        let base_case = b.icmp(IntCond::Uge, lo, hi);
        b.br(Flags::NONE, base_case, (done, &[]), (work, &[]));

        b.begin(done);
        b.ret(&[]);

        b.begin(work);
        let tmp_ptr = b.slot_addr(tmp_slot);
        let c8 = b.iconst(Ty::I64, 8);
        let lo_off = b.binary(Op::Mul, Flags::NONE, lo, c8);
        let hi_off = b.binary(Op::Mul, Flags::NONE, hi, c8);
        let p_lo = b.ptr_add(Flags::NONE, ptr, lo_off);
        let p_hi = b.ptr_add(Flags::NONE, ptr, hi_off);
        b.bulk(Op::Memmove, Flags::NONE, tmp_ptr, p_lo, c8, MemAttrs::aligned(3));
        b.bulk(Op::Memmove, Flags::NONE, p_lo, p_hi, c8, MemAttrs::aligned(3));
        b.bulk(Op::Memmove, Flags::NONE, p_hi, tmp_ptr, c8, MemAttrs::aligned(3));
        let sum = b.binary(Op::Add, Flags::NONE, lo, hi);
        let one = b.iconst(Ty::I64, 1);
        let mid = b.binary(Op::Lshr, Flags::NONE, sum, one);
        let mid_plus_1 = b.binary(Op::Add, Flags::NONE, mid, one);
        // Non-tail first recursive call:
        b.call(Flags::NONE, dc_self_ref, dc_sig, &[ptr, lo, mid]);
        // Tail second recursive call:
        b.call(Flags::NONE, dc_self_ref, dc_sig, &[ptr, mid_plus_1, hi]);
        b.slot_dead(tmp_slot);
        b.ret(&[]);

        let dc_func = b.finish_checked().unwrap();
        let dc_id = BodyId([2u8; 32]);
        let self_bank = SingleSelfBodies { id: dc_id, ref_id: dc_self_ref, view: dc_func.view() };

        let out = eliminate_self_tail_calls(dc_func.view(), dc_id, &self_bank, alloc).unwrap();
        assert_eq!(out.stats.tail_calls_eliminated, 1);
        verify(out.func.view()).unwrap();

        let calls_after = (0..out.func.view().len())
            .filter(|&i| out.func.view().op(i) == Op::Call)
            .count();
        assert_eq!(
            calls_after, 1,
            "expected ONLY the second (tail) self-call eliminated while keeping the first non-tail self-call"
        );

        // Build driver `run_dc(n: i64) -> i64` that initializes an 8-element u64 array on stack,
        // invokes `dc(arr, 0, n - 1)`, and returns a position-weighted checksum.
        let mut d = Builder::<_, Full>::new_in(64, alloc);
        let driver_sig = d.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        d.set_signature(driver_sig);
        let d_dc_sig = d.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::PTR), Param::of(Ty::I64), Param::of(Ty::I64)],
            &[],
        );
        let d_dc_ref = d.reference(RefKind::Func, 1);
        let arr_slot = d.slot(64, 3, SlotKind::Named);
        let d_entry = d.create_block(BlockKind::Normal);
        d.begin(d_entry);
        let n = d.param(Ty::I64);
        let arr = d.slot_addr(arr_slot);
        for k in 0..8u64 {
            let v = d.iconst(Ty::I64, u128::from((k * 13 + 7) ^ 0x5a));
            d.store(Flags::NONE, v, arr, (k * 8) as i32, MemAttrs::aligned(3));
        }
        let zero = d.iconst(Ty::I64, 0);
        let one = d.iconst(Ty::I64, 1);
        let last = d.binary(Op::Sub, Flags::NONE, n, one);
        d.call(Flags::NONE, d_dc_ref, d_dc_sig, &[arr, zero, last]);
        let mut acc = d.iconst(Ty::I64, 0);
        for k in 0..8u64 {
            let elem = d.load(Ty::I64, Flags::NONE, arr, (k * 8) as i32, MemAttrs::aligned(3));
            let w = d.iconst(Ty::I64, u128::from(k + 1));
            let term = d.binary(Op::Mul, Flags::NONE, elem, w);
            acc = d.binary(Op::Add, Flags::NONE, acc, term);
        }
        d.ret(&[acc]);
        let driver_func = d.finish_checked().unwrap();

        let driver_id = BodyId([3u8; 32]);
        let bank_before = TwoBodyBank {
            driver_id,
            driver_to_dc: d_dc_ref,
            dc_id,
            dc_self_ref,
            dc_view: dc_func.view(),
        };
        let bank_after = TwoBodyBank {
            driver_id,
            driver_to_dc: d_dc_ref,
            dc_id,
            dc_self_ref,
            dc_view: out.func.view(),
        };

        for nv in [1u64, 2, 4, 6, 8] {
            let args = [Val::Int(u128::from(nv))];
            let before = eval::run_calls_in(driver_func.view(), driver_id, &args, 100_000, &bank_before, alloc).unwrap();
            let after = eval::run_calls_in(driver_func.view(), driver_id, &args, 100_000, &bank_after, alloc).unwrap();
            assert_eq!(before, after, "divide-and-conquer TCE mismatch for n={nv}");
        }
    }

    #[test]
    fn self_tce_skips_when_slot_addr_escapes_into_tail_call_argument() {
        let alloc = blaze_own::testing::heap().alloc();
        let mut b = Builder::<_, Full>::new_in(64, alloc);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::PTR), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let self_ref = b.reference(RefKind::Func, 0);
        let local_slot = b.slot(8, 3, SlotKind::Named);
        let [entry, done, recurse] = [(); 3].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let p = b.param(Ty::PTR);
        let n = b.param(Ty::I64);
        let sp = b.slot_addr(local_slot);
        b.store(Flags::NONE, n, sp, 0, MemAttrs::aligned(3));
        let zero = b.iconst(Ty::I64, 0);
        let is_zero = b.icmp(IntCond::Eq, n, zero);
        b.br(Flags::NONE, is_zero, (done, &[]), (recurse, &[]));

        b.begin(done);
        let loaded = b.load(Ty::I64, Flags::NONE, p, 0, MemAttrs::aligned(3));
        b.ret(&[loaded]);

        b.begin(recurse);
        let one = b.iconst(Ty::I64, 1);
        let next_n = b.binary(Op::Sub, Flags::NONE, n, one);
        // Passes local `SlotAddr` as an argument to the self-tail-call -> MUST NOT fire TCE!
        let res = b.call(Flags::NONE, self_ref, sig, &[sp, next_n]);
        b.ret(&[res]);

        let func = b.finish_checked().unwrap();
        let self_id = BodyId([4u8; 32]);
        let bank = SingleSelfBodies { id: self_id, ref_id: self_ref, view: func.view() };

        let out = eliminate_self_tail_calls(func.view(), self_id, &bank, alloc).unwrap();
        assert_eq!(
            out.stats.tail_calls_eliminated, 0,
            "TCE must NOT fire when a local StackSlot address escapes into a self-tail-call argument"
        );
        let call_count = (0..out.func.view().len())
            .filter(|&i| out.func.view().op(i) == Op::Call)
            .count();
        assert_eq!(call_count, 1);
    }

    #[test]
    fn lto_optimize_module_par_matches_sequential() {
        let shared = blaze_own::testing::heap().shared();
        let alloc = Alloc::from(shared);

        // Build a 4-body multi-level DAG:
        //   leaf(x)   = x + 7
        //   mid(x)    = leaf(x) * 3
        //   root_a(x) = mid(x) + leaf(x)
        //   root_b(x) = leaf(x) ^ 1
        let leaf_id = BodyId([11u8; 32]);
        let mid_id = BodyId([12u8; 32]);
        let root_a_id = BodyId([13u8; 32]);
        let root_b_id = BodyId([14u8; 32]);

        let build_unary_leaf = || {
            let mut b = Builder::<_, Full>::new_in(32, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let x = b.param(Ty::I64);
            let c7 = b.iconst(Ty::I64, 7);
            let sum = b.binary(Op::Add, Flags::NONE, x, c7);
            b.ret(&[sum]);
            b.finish_checked().unwrap()
        };

        let leaf_func = build_unary_leaf();

        let (mid_func, mid_to_leaf) = {
            let mut b = Builder::<_, Full>::new_in(32, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
            b.set_signature(sig);
            let leaf_ref = b.reference(RefKind::Func, 0);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let x = b.param(Ty::I64);
            let r = b.call(Flags::NONE, leaf_ref, sig, &[x]);
            let c3 = b.iconst(Ty::I64, 3);
            let prod = b.binary(Op::Mul, Flags::NONE, r, c3);
            b.ret(&[prod]);
            (b.finish_checked().unwrap(), leaf_ref)
        };

        let (root_a_func, ra_to_mid, ra_to_leaf) = {
            let mut b = Builder::<_, Full>::new_in(32, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
            b.set_signature(sig);
            let mid_ref = b.reference(RefKind::Func, 0);
            let leaf_ref = b.reference(RefKind::Func, 1);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let x = b.param(Ty::I64);
            let m = b.call(Flags::NONE, mid_ref, sig, &[x]);
            let l = b.call(Flags::NONE, leaf_ref, sig, &[x]);
            let sum = b.binary(Op::Add, Flags::NONE, m, l);
            b.ret(&[sum]);
            (b.finish_checked().unwrap(), mid_ref, leaf_ref)
        };

        let (root_b_func, rb_to_leaf) = {
            let mut b = Builder::<_, Full>::new_in(32, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
            b.set_signature(sig);
            let leaf_ref = b.reference(RefKind::Func, 0);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let x = b.param(Ty::I64);
            let l = b.call(Flags::NONE, leaf_ref, sig, &[x]);
            let c1 = b.iconst(Ty::I64, 1);
            let xored = b.binary(Op::Xor, Flags::NONE, l, c1);
            b.ret(&[xored]);
            (b.finish_checked().unwrap(), leaf_ref)
        };

        struct DagBodies<'a> {
            leaf_id: BodyId,
            mid_id: BodyId,
            root_a_id: BodyId,
            root_b_id: BodyId,
            mid_to_leaf: RefId,
            ra_to_mid: RefId,
            ra_to_leaf: RefId,
            rb_to_leaf: RefId,
            leaf_view: View<'a>,
            mid_view: View<'a>,
        }

        impl Bodies for DagBodies<'_> {
            fn callee(&self, from: BodyId, reference: RefId) -> Option<(BodyId, View<'_>)> {
                if (from == self.mid_id && reference == self.mid_to_leaf)
                    || (from == self.root_a_id && reference == self.ra_to_leaf)
                    || (from == self.root_b_id && reference == self.rb_to_leaf)
                {
                    return Some((self.leaf_id, self.leaf_view));
                }
                if from == self.root_a_id && reference == self.ra_to_mid {
                    return Some((self.mid_id, self.mid_view));
                }
                None
            }

            fn target(&self, from: BodyId, reference: RefId) -> Option<Target> {
                self.callee(from, reference).map(|(id, _)| Target::Func(id))
            }
        }

        let bank = DagBodies {
            leaf_id,
            mid_id,
            root_a_id,
            root_b_id,
            mid_to_leaf,
            ra_to_mid,
            ra_to_leaf,
            rb_to_leaf,
            leaf_view: leaf_func.view(),
            mid_view: mid_func.view(),
        };

        let mut seq_mod = OwnVec::with_capacity_in(4, alloc);
        seq_mod.push(ModuleBody { id: root_a_id, func: root_a_func.view().to_func_in(alloc) });
        seq_mod.push(ModuleBody { id: root_b_id, func: root_b_func.view().to_func_in(alloc) });
        seq_mod.push(ModuleBody { id: mid_id, func: mid_func.view().to_func_in(alloc) });
        seq_mod.push(ModuleBody { id: leaf_id, func: leaf_func.view().to_func_in(alloc) });

        let mut par_mod = OwnVec::with_capacity_in(4, shared);
        par_mod.push(ModuleBody { id: root_a_id, func: root_a_func.view().to_func_in(shared) });
        par_mod.push(ModuleBody { id: root_b_id, func: root_b_func.view().to_func_in(shared) });
        par_mod.push(ModuleBody { id: mid_id, func: mid_func.view().to_func_in(shared) });
        par_mod.push(ModuleBody { id: leaf_id, func: leaf_func.view().to_func_in(shared) });

        let policy = InlinePolicy::default();
        let seq = lto_optimize_module(seq_mod, &bank, &NoWeights, policy, alloc).unwrap();
        let par = lto_optimize_module_par(par_mod, &bank, &NoWeights, policy, shared, &SequentialLtoRunner).unwrap();

        assert_eq!(par.total_inlined_bodies, seq.total_inlined_bodies);
        assert!(par.total_inlined_bodies >= 3, "expected transitive bottom-up inlining");
        assert_eq!(par.bodies.len(), seq.bodies.len());
        for (p, s) in par.bodies.iter().zip(seq.bodies.iter()) {
            assert_eq!(p.id, s.id);
            assert_eq!(p.summary.summary_hash, s.summary.summary_hash);
            assert_eq!(p.prev_summary.summary_hash, s.prev_summary.summary_hash);
            assert_eq!(p.read.as_slice(), s.read.as_slice());
            assert_eq!(p.imported_refs.as_slice(), s.imported_refs.as_slice());
            assert_eq!(p.func.view().ops, s.func.view().ops);
            assert_eq!(p.func.view().data, s.func.view().data);
            assert_eq!(p.func.view().extra, s.func.view().extra);
            verify(p.func.view()).unwrap();
        }
    }

    #[test]
    fn lto_optimize_module_par_matches_sequential_with_transitive_external_callback() {
        let shared = blaze_own::testing::heap().shared();
        let alloc = Alloc::from(shared);

        // Module contains `[A, B, C]` in that order (`A` at index 0, `B` at index 1, `C` at index 2).
        // `A(x)` calls external `E(x)` (not in module, provided by `Bodies::callee`), which calls
        // back into module body `B(x)`, which has a local stack-slot store/load simplified by
        // `simplify_body` AND calls module body `C(x)`.
        //
        // Without transitive external callback edges `A -> E -> B`, `A` would have no edge to `B`
        // and parallel wavefronts could run `A` before `B`'s pre-simplification/inlining finished.
        let a_id = BodyId([21u8; 32]);
        let b_id = BodyId([22u8; 32]);
        let c_id = BodyId([23u8; 32]);
        let ext_id = BodyId([24u8; 32]);

        let c_func = {
            let mut b = Builder::<_, Full>::new_in(64, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let x = b.param(Ty::I64);
            let c5 = b.iconst(Ty::I64, 5);
            let y = b.binary(Op::Add, Flags::NONE, x, c5);
            b.ret(&[y]);
            b.finish_checked().unwrap()
        };

        let (b_func, b_to_c) = {
            let mut b = Builder::<_, Full>::new_in(64, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
            b.set_signature(sig);
            let c_ref = b.reference(RefKind::Func, 0);
            let slot = b.slot(8, 3, SlotKind::Named);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let x = b.param(Ty::I64);
            // Stack-slot store + reload transformed by `simplify_body` (SROA).
            let sp = b.slot_addr(slot);
            b.store(Flags::NONE, x, sp, 0, MemAttrs::aligned(3));
            let reloaded = b.load(Ty::I64, Flags::NONE, sp, 0, MemAttrs::aligned(3));
            let from_c = b.call(Flags::NONE, c_ref, sig, &[reloaded]);
            let c2 = b.iconst(Ty::I64, 2);
            let res = b.binary(Op::Mul, Flags::NONE, from_c, c2);
            b.ret(&[res]);
            (b.finish_checked().unwrap(), c_ref)
        };

        let (ext_func, ext_to_b) = {
            let mut b = Builder::<_, Full>::new_in(64, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
            b.set_signature(sig);
            let b_ref = b.reference(RefKind::Func, 0);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let x = b.param(Ty::I64);
            let r = b.call(Flags::NONE, b_ref, sig, &[x]);
            b.ret(&[r]);
            (b.finish_checked().unwrap(), b_ref)
        };

        let (a_func, a_to_ext) = {
            let mut b = Builder::<_, Full>::new_in(64, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
            b.set_signature(sig);
            let ext_ref = b.reference(RefKind::Func, 0);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let x = b.param(Ty::I64);
            let r = b.call(Flags::NONE, ext_ref, sig, &[x]);
            let c1 = b.iconst(Ty::I64, 1);
            let out = b.binary(Op::Add, Flags::NONE, r, c1);
            b.ret(&[out]);
            (b.finish_checked().unwrap(), ext_ref)
        };

        struct ExtCallbackBodies<'a> {
            a_id: BodyId,
            b_id: BodyId,
            c_id: BodyId,
            ext_id: BodyId,
            a_to_ext: RefId,
            ext_to_b: RefId,
            b_to_c: RefId,
            b_view: View<'a>,
            c_view: View<'a>,
            ext_view: View<'a>,
        }

        impl Bodies for ExtCallbackBodies<'_> {
            fn callee(&self, from: BodyId, reference: RefId) -> Option<(BodyId, View<'_>)> {
                if from == self.a_id && reference == self.a_to_ext {
                    return Some((self.ext_id, self.ext_view));
                }
                if from == self.ext_id && reference == self.ext_to_b {
                    return Some((self.b_id, self.b_view));
                }
                if from == self.b_id && reference == self.b_to_c {
                    return Some((self.c_id, self.c_view));
                }
                None
            }

            fn target(&self, from: BodyId, reference: RefId) -> Option<Target> {
                self.callee(from, reference).map(|(id, _)| Target::Func(id))
            }
        }

        let bank = ExtCallbackBodies {
            a_id,
            b_id,
            c_id,
            ext_id,
            a_to_ext,
            ext_to_b,
            b_to_c,
            b_view: b_func.view(),
            c_view: c_func.view(),
            ext_view: ext_func.view(),
        };

        let mut seq_mod = OwnVec::with_capacity_in(3, alloc);
        seq_mod.push(ModuleBody { id: a_id, func: a_func.view().to_func_in(alloc) });
        seq_mod.push(ModuleBody { id: b_id, func: b_func.view().to_func_in(alloc) });
        seq_mod.push(ModuleBody { id: c_id, func: c_func.view().to_func_in(alloc) });

        let mut par_mod = OwnVec::with_capacity_in(3, shared);
        par_mod.push(ModuleBody { id: a_id, func: a_func.view().to_func_in(shared) });
        par_mod.push(ModuleBody { id: b_id, func: b_func.view().to_func_in(shared) });
        par_mod.push(ModuleBody { id: c_id, func: c_func.view().to_func_in(shared) });

        let policy = InlinePolicy::default();
        let seq = lto_optimize_module(seq_mod, &bank, &NoWeights, policy, alloc).unwrap();
        let par = lto_optimize_module_par(par_mod, &bank, &NoWeights, policy, shared, &SequentialLtoRunner).unwrap();

        assert_eq!(par.scc_count, seq.scc_count);
        assert_eq!(par.total_inlined_bodies, seq.total_inlined_bodies);
        assert_eq!(par.bodies.len(), seq.bodies.len());
        for (p, s) in par.bodies.iter().zip(seq.bodies.iter()) {
            assert_eq!(p.id, s.id);
            assert_eq!(p.summary.summary_hash, s.summary.summary_hash);
            assert_eq!(p.prev_summary.summary_hash, s.prev_summary.summary_hash);
            assert_eq!(p.read.as_slice(), s.read.as_slice());
            assert_eq!(p.imported_refs.as_slice(), s.imported_refs.as_slice());
            assert_eq!(p.func.view().ops, s.func.view().ops);
            assert_eq!(p.func.view().data, s.func.view().data);
            assert_eq!(p.func.view().extra, s.func.view().extra);
            verify(p.func.view()).unwrap();
        }
        // `A` should have inlined `E -> B -> C` completely, evaluating `(x + 5) * 2 + 1`.
        let a_par = &par.bodies[0].func;
        let eval_res = eval::run_in(a_par.view(), &[Val::Int(10)], 10_000, alloc).unwrap();
        assert_eq!(eval_res, Outcome::Returned(alloc::vec![Val::Int(31)]));
    }
}
