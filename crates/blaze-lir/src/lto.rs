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
use blaze_own::Vec as OwnVec;

use crate::{
    Op, View,
    body::{Bodies, BodyId, Datum, DatumId, Target},
    func::{Func, RefId},
    inline::{ImportedRef, InlineError, InlinePolicy, SiteWeights, inline_guided_with_cache},
    pgo::PgoSiteTable,
    simplify::{
        RegBudget, SimplifyError, SuperoptOpts, auto_vectorize_loops, dead, eliminate_redundant_checks, if_convert,
        optimize_memory, propagate, scalar_replace, schedule_dag, slp_vectorize, sota_optimize_fixpoint,
        speculative_devirt, superopt_a64, tail_duplicate_dispatch, unroll_scalar_loops, unswitch_loops,
        vectorize_memchr_loops, IndirectTarget,
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
/// callees while forwarding external bodies, targets, data, and source positions to `external`.
struct ModuleBodies<'a, A: Allocator> {
    bodies: &'a [ModuleBody<A>],
    id_to_index: &'a Map<BodyId, usize, FixedState, A>,
    external: &'a dyn Bodies,
}

impl<A: Allocator> Bodies for ModuleBodies<'_, A> {
    fn callee(&self, from: BodyId, reference: RefId) -> Option<(BodyId, View<'_>)> {
        let (callee_id, ext_view) = self.external.callee(from, reference)?;
        if let Some(&idx) = self.id_to_index.get(&callee_id) {
            return Some((callee_id, self.bodies[idx].func.view()));
        }
        Some((callee_id, ext_view))
    }

    fn target(&self, from: BodyId, reference: RefId) -> Option<Target> {
        self.external.target(from, reference)
    }

    fn datum(&self, id: DatumId) -> Option<Datum<'_>> {
        self.external.datum(id)
    }

    fn body(&self, id: BodyId) -> Option<View<'_>> {
        if let Some(&idx) = self.id_to_index.get(&id) {
            return Some(self.bodies[idx].func.view());
        }
        self.external.body(id)
    }

    fn position(&self, item: u128, site: u32) -> Option<(u32, u32)> {
        self.external.position(item, site)
    }
}

/// Runs `scalar_replace -> propagate -> dead` on `view` in `alloc`.
fn simplify_body<A: Allocator + Copy>(view: View<'_>, alloc: A) -> Result<Func<A>, SimplifyError> {
    let sroa = scalar_replace(view, alloc)?;
    let prop = propagate(sroa.view(), alloc)?;
    dead(prop.view(), alloc)
}

fn from_inline(error: InlineError) -> SimplifyError {
    match error {
        InlineError::Build(e) => SimplifyError::Build(e),
        InlineError::Verify(e) => SimplifyError::Verify(e),
        InlineError::Refused { inst, what } => SimplifyError::Refused { inst, what },
    }
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

    // Build intra-module direct call graph in CSR form.
    let mut adj_offsets: OwnVec<u32, A> = OwnVec::with_capacity_in(n + 1, alloc);
    let mut adj: OwnVec<u32, A> = OwnVec::with_capacity_in(n * 2, alloc);
    adj_offsets.push(0);
    for mb in module.iter() {
        let view = mb.func.view();
        let edge_start = adj.len();
        for at in 0..view.len() {
            if !matches!(view.op(at), Op::Call | Op::Invoke) {
                continue;
            }
            let words = &view.extra[view.data[at] as usize..];
            let reference = RefId(words[0]);
            if let Some((callee_id, _)) = external.callee(mb.id, reference)
                && let Some(&callee_idx) = id_to_index.get(&callee_id)
            {
                adj.push(callee_idx as u32);
            }
        }
        adj[edge_start..].sort_unstable();
        let mut w = edge_start;
        for r in edge_start..adj.len() {
            if w == edge_start || adj[r] != adj[w - 1] {
                adj[w] = adj[r];
                w += 1;
            }
        }
        adj.truncate(w);
        adj_offsets.push(adj.len() as u32);
    }

    // Condense call graph into reverse-topological SCCs (callees before callers).
    let sccs = tarjan_sccs(n, &adj_offsets, &adj, alloc);

    // Compute initial summaries for early-cutoff comparison.
    let mut prev_summaries: OwnVec<FnSummary, A> = OwnVec::with_capacity_in(n, alloc);
    let mut summaries: Map<BodyId, FnSummary, FixedState, A> = Map::new(MapOpts {
        hasher: FixedState,
        alloc: alloc,
        capacity: Some(n),
    });
    for mb in module.iter() {
        let s = FnSummary::compute(mb.func.view(), &alloc);
        prev_summaries.push(s);
        summaries.insert(mb.id, s);
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

        // Phase 1: pre-inline simplify each body in the SCC and refresh its summary.
        for &node in comp {
            let idx = node as usize;
            let simplified = simplify_body(module[idx].func.view(), alloc)?;
            let sum = FnSummary::compute(simplified.view(), &alloc);
            summaries.insert(module[idx].id, sum);
            module[idx].func = simplified;
        }

        // Phase 2: PGO-guided priority inlining + post-inline simplify (SROA + const-fold + DCE).
        for &node in comp {
            let idx = node as usize;
            let body_id = module[idx].id;
            let inlined = {
                let overlay = ModuleBodies { bodies: &module, id_to_index: &id_to_index, external };
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
/// `SROA -> aegraph fixpoint -> BCE -> Loop Unswitching -> Memory LICM/DSE -> 4x SIMD Auto-Vectorization -> 4x Scalar Unrolling -> Bounded E-Graph Superopt`.
fn sota_optimize_body<A: Allocator + Copy>(
    view: View<'_>,
    opts: SotaLtoOpts,
    alloc: A,
) -> Result<Func<A>, SimplifyError> {
    let mut cur = scalar_replace(view, alloc)?;
    cur = sota_optimize_fixpoint(cur.view(), opts.reg_budget, opts.max_aegraph_rounds, |_| 1, alloc)?.func;
    let bce = eliminate_redundant_checks(cur.view(), alloc)?;
    cur = bce.func;
    let unsw = unswitch_loops(cur.view(), 2, alloc)?;
    cur = unsw.func;
    let ifc = if_convert(cur.view(), 8, alloc)?;
    cur = ifc.func;
    if bce.stats.checks_eliminated > 0 || unsw.stats.loops_unswitched > 0 || ifc.stats.branches_converted > 0 {
        cur = sota_optimize_fixpoint(cur.view(), opts.reg_budget, 2, |_| 1, alloc)?.func;
    }
    cur = optimize_memory(cur.view(), alloc)?.func;
    let mut vectorized = false;
    if opts.auto_vectorize {
        let vout = auto_vectorize_loops(cur.view(), alloc)?;
        vectorized = vout.stats.loops_vectorized > 0;
        cur = vout.func;
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
    if !vectorized {
        cur = schedule_dag(cur.view(), alloc)?.func;
    }
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

    verify(func.view()).map_err(SimplifyError::Verify)?;
    sota_optimize_body(func.view(), opts, alloc)
}

/// Full SOTA Cross-Function Bottom-Up ThinLTO:
///
/// 1. Runs PGO mono/bimorphic `dyn Trait` / fn-pointer [`speculative_devirt`] across the module
///    BEFORE Tarjan SCC condensation, exposing newly direct `caller -> hot_dyn_impl` edges to the
///    bottom-up call graph.
/// 2. Condenses the enriched call graph with iterative Tarjan SCCs (callees before callers).
/// 3. For each SCC bottom-up: simplifies callees, runs PGO-guided multi-function inlining
///    (`inline_guided_with_cache`, splicing chains `a -> b -> c` and devirtualized `dyn` methods),
///    and runs the full cross-function SOTA pipeline (`SROA -> aegraph -> BCE -> Loop Unswitching
///    -> Memory LICM/DSE -> 4x SIMD Auto-Vectorization -> 4x Scalar Unrolling -> Superopt`) over
///    the merged multi-function body!
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

    // Phase 2: Full cross-function SOTA optimization over every merged body (SROA + aegraph + BCE +
    // Loop Unswitching + Memory LICM/DSE + 4x SIMD + 4x Scalar Unroll + Bounded E-Graph Superopt)!
    for ob in lto.bodies.iter_mut() {
        ob.func = sota_optimize_body(ob.func.view(), opts, alloc)?;
        ob.summary = FnSummary::compute(ob.func.view(), &alloc);
        ob.summary_changed = ob.summary.summary_hash != ob.prev_summary.summary_hash;
    }

    Ok(lto)
}
