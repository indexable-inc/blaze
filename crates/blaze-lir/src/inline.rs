//! The LIR-to-LIR inliner:
//!
//! - [`inline_trivial`] (D98's first point): splices small loop-free bodies and `#[inline(always)]`
//!   ones into their callers in deterministic DFS-preorder within a [`Bound`], keeping callees
//!   CLOSED (no external refs imported).
//! - [`inline_guided`]: PGO + cost-model priority-scored inliner driven by [`SiteWeights`],
//!   [`InlinePolicy`], and cached [`FnSummary`] metadata. Replaces greedy DFS-preorder budget
//!   exhaustion with a deterministic priority worklist (tie-broken by layout order and depth),
//!   scores PGO hotness, caller loop depth, `#[inline]` hints, constant/SROA/`func_addr`
//!   specialization opportunities, refuses cold sites and `#[cold]` callees, and (when
//!   `allow_external_refs` is enabled) lifts the CLOSED barrier by importing external `RefRec`s
//!   and remapping `DataRec` relocations and source positions.

use alloc::vec::Vec;
use alloc::alloc::Allocator;
use core::cmp::Ordering;
use core::convert::Infallible;
use core::fmt;
use core::{hash::BuildHasher, hash::Hash, hash::Hasher};

use blaze_hash::{FixedState, Map, MapOpts};
use blaze_own::Vec as OwnVec;

use crate::{
    body::{Bodies, BodyId, Datum, DatumId, Target},
    builder::BuildError,
    func::{
        Block, BlockKind, CompoundRec, DataId, DataRec, DataRelocRec, Func, Inline,
        InlinedRec, IntrinsicId, IntrinsicRec, NONE, ParamFactsRec, ParamKind, ParamRec, RefId,
        RefKind, RefRec, SCOPE_IN, SCOPE_OUTSIDE, SigId, SigRec, View,
    },
    op::{Field, Layout, Op, Word, walk},
    summary::{FnSummary, carried},
    ty::Ty,
    verify::{VerifyError, verify},
};

/// How much the trivial inliner may splice.
#[derive(Clone, Copy, Debug)]
pub struct Bound {
    /// A callee without `#[inline(always)]` has at most this many instructions.
    pub insts: u32,
    /// Sites are nested at most this deep.
    pub depth: u8,
    /// The spliced instructions of one function, in total, are at most this many.
    pub growth: u32,
}

/// The fast tier's bound: estimates until the measurement named in backend-next.md's section.
pub const TRIVIAL: Bound = Bound { insts: 16, depth: 4, growth: 256 };

/// A function with its trivial sites spliced.
pub struct Inlined<A: Allocator> {
    pub func: Func<A>,
    /// The spliced bodies, sorted, unique: the fast record's read list gains exactly these.
    pub read: Vec<BodyId, A>,
}

/// Hotness weight for a call site `(caller, site_inst)`, e.g. from PGO profile counts.
pub trait SiteWeights {
    /// Returns the execution weight of call instruction `site_inst` in `caller`.
    fn weight(&self, caller: BodyId, site_inst: u32) -> u64;
}

/// Default uniform site weights: every call site has weight `1`.
#[derive(Clone, Copy, Default, Debug)]
pub struct NoWeights;

impl SiteWeights for NoWeights {
    fn weight(&self, _caller: BodyId, _site_inst: u32) -> u64 {
        1
    }
}

/// Cost model, thresholds, and budget policy for [`inline_guided`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct InlinePolicy {
    /// Base instruction cost threshold for normal call sites.
    pub cost_threshold: u32,
    /// Hard upper bound on a single callee's instruction count (unless `#[inline(always)]`).
    pub max_callee_insts: u32,
    /// Threshold multiplier applied to hot call sites (`weight >= hot_weight_threshold` or inside a loop).
    pub hot_multiplier: u32,
    /// Minimum [`SiteWeights::weight`] considered hot.
    pub hot_weight_threshold: u64,
    /// Maximum nesting depth of spliced calls.
    pub max_depth: u8,
    /// Maximum total instructions spliced into one caller.
    pub max_growth: u32,
    /// When true, lifts the CLOSED restriction: callees referencing external `Func`/`Static`/
    /// `Tls`/`Vtable` or relocated/positioned `Data` may be inlined, importing their `RefRec`s
    /// into the caller's `refs` table and recording them in [`GuidedInlined::imported_refs`].
    pub allow_external_refs: bool,
    /// Whether non-`#[inline(always)]` callees containing loops may be considered under the cost model.
    pub allow_loops: bool,
}

impl Default for InlinePolicy {
    fn default() -> Self {
        Self {
            cost_threshold: 45,
            max_callee_insts: 128,
            hot_multiplier: 4,
            hot_weight_threshold: 10,
            max_depth: 6,
            max_growth: 512,
            allow_external_refs: true,
            allow_loops: true,
        }
    }
}

/// One external reference imported from a spliced callee into the caller's `refs` table.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ImportedRef {
    /// The newly allocated `RefId` in the output function's `refs` table.
    pub caller_ref: RefId,
    /// The callee body this reference originated from.
    pub source_body: BodyId,
    /// The original `RefId` in `source_body`.
    pub source_ref: RefId,
    /// The `RefRec` written at `func.refs[caller_ref]`.
    pub rec: RefRec,
}

/// A function with its priority-guided sites spliced.
pub struct GuidedInlined<A: Allocator> {
    pub func: Func<A>,
    /// The spliced bodies, sorted, unique.
    pub read: Vec<BodyId, A>,
    /// External references imported into `func.refs`, in deterministic order of `caller_ref`.
    pub imported_refs: Vec<ImportedRef, A>,
}

/// [`Bodies`] adapter that resolves a root function's [`ImportedRef`]s back to their source
/// bodies in `inner`, enabling evaluation or downstream resolution after [`inline_guided`].
pub struct WithImported<'a, B: ?Sized> {
    pub root: BodyId,
    pub imported: &'a [ImportedRef],
    pub inner: &'a B,
}

impl<'a, B: Bodies + ?Sized> WithImported<'a, B> {
    #[inline]
    fn find(&self, reference: RefId) -> Option<&'a ImportedRef> {
        let idx = self
            .imported
            .binary_search_by_key(&reference, |imp| imp.caller_ref)
            .ok()?;
        Some(&self.imported[idx])
    }
}

impl<B: Bodies + ?Sized> Bodies for WithImported<'_, B> {
    fn callee(&self, from: BodyId, reference: RefId) -> Option<(BodyId, View<'_>)> {
        if from == self.root
            && let Some(imp) = self.find(reference)
        {
            return self.inner.callee(imp.source_body, imp.source_ref);
        }
        self.inner.callee(from, reference)
    }

    fn target(&self, from: BodyId, reference: RefId) -> Option<Target> {
        if from == self.root
            && let Some(imp) = self.find(reference)
        {
            return self.inner.target(imp.source_body, imp.source_ref);
        }
        self.inner.target(from, reference)
    }

    fn datum(&self, id: DatumId) -> Option<Datum<'_>> {
        self.inner.datum(id)
    }

    fn body(&self, id: BodyId) -> Option<View<'_>> {
        self.inner.body(id)
    }

    fn position(&self, item: u128, site: u32) -> Option<(u32, u32)> {
        self.inner.position(item, site)
    }
}

/// Why the inliner made no function.
#[derive(Debug)]
pub enum InlineError {
    /// A table past what its index can name.
    Build(BuildError),
    /// The output breaks a rule of the verifier: an inliner bug.
    Verify(VerifyError),
    /// Instruction `inst` of the body holding it is a form the inliner does not take.
    Refused { inst: usize, what: &'static str },
}

impl fmt::Display for InlineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InlineError::Build(error) => write!(f, "the inlined function: {error}"),
            InlineError::Verify(error) => write!(f, "the inlined function is not LIR: {error}"),
            InlineError::Refused { inst, what } => {
                write!(f, "v{inst}: {what} (refused by the inliner, blaze_lir::inline)")
            }
        }
    }
}

impl core::error::Error for InlineError {}

/// `view` (the body `root`) with its trivial sites spliced, the callees found through
/// `bodies`, within `bound`, in `alloc`.
pub fn inline_trivial<A: Allocator + Copy>(
    view: View<'_>,
    root: BodyId,
    bodies: &dyn Bodies,
    bound: Bound,
    alloc: A,
) -> Result<Inlined<A>, InlineError> {
    inline_as(view, root, bodies, bound, false, alloc)
}

/// [`inline_trivial`] with a planted bug (test builds only): the first spliced call's first two
/// arguments are swapped.
#[cfg(test)]
pub(crate) fn inline_swapping_first_args<A: Allocator + Copy>(
    view: View<'_>,
    root: BodyId,
    bodies: &dyn Bodies,
    bound: Bound,
    alloc: A,
) -> Result<Inlined<A>, InlineError> {
    inline_as(view, root, bodies, bound, true, alloc)
}

fn inline_as<A: Allocator + Copy>(
    view: View<'_>,
    root: BodyId,
    bodies: &dyn Bodies,
    bound: Bound,
    swap: bool,
    alloc: A,
) -> Result<Inlined<A>, InlineError> {
    let mut summaries = new_map(&alloc);
    let (nodes, children) = plan_trivial(view, root, bodies, bound, &mut summaries, &alloc)?;
    let (func, _imported) = emit(&nodes, &children, bodies, false, swap, &alloc)?;
    let mut read = Vec::with_capacity_in(nodes.len().saturating_sub(1), alloc);
    read.extend(nodes.iter().skip(1).map(|node| node.id));
    read.sort_unstable();
    read.dedup();
    Ok(Inlined { func, read })
}

/// PGO + cost-model priority inliner: splices sites into `view` (body `root`) using `weights`
/// and `policy`, caching [`FnSummary`] per callee body, in `alloc`.
pub fn inline_guided<A: Allocator + Copy>(
    view: View<'_>,
    root: BodyId,
    bodies: &dyn Bodies,
    weights: &dyn SiteWeights,
    policy: InlinePolicy,
    alloc: A,
) -> Result<GuidedInlined<A>, InlineError> {
    let mut summaries = new_map(&alloc);
    inline_guided_with_cache(view, root, bodies, weights, policy, &mut summaries, alloc)
}

/// Shared-cache entry point used by [`crate::lto`] so callee summaries are computed at most
/// once across an entire module.
pub(crate) fn inline_guided_with_cache<A: Allocator + Copy>(
    view: View<'_>,
    root: BodyId,
    bodies: &dyn Bodies,
    weights: &dyn SiteWeights,
    policy: InlinePolicy,
    summaries: &mut Map<BodyId, FnSummary, FixedState, A>,
    alloc: A,
) -> Result<GuidedInlined<A>, InlineError> {
    inline_guided_with_base_cache(view, root, bodies, weights, policy, None::<&Map<BodyId, FnSummary, FixedState, A>>, summaries, alloc)
}

/// Parallel-friendly entry point used by [`crate::lto::lto_optimize_module_par`]: looks up
/// immutable precomputed module summaries in `base` first and stores any newly computed external
/// summaries in the worker-local `local` map without mutating `base`.
#[expect(clippy::too_many_arguments, reason = "internal LTO inliner entry over view, root, bodies, weights, policy, base cache, local cache, and scratch allocator")]
pub(crate) fn inline_guided_with_base_cache<A: Allocator + Copy, B: Allocator>(
    view: View<'_>,
    root: BodyId,
    bodies: &dyn Bodies,
    weights: &dyn SiteWeights,
    policy: InlinePolicy,
    base: Option<&Map<BodyId, FnSummary, FixedState, B>>,
    local: &mut Map<BodyId, FnSummary, FixedState, A>,
    alloc: A,
) -> Result<GuidedInlined<A>, InlineError> {
    let (nodes, children) =
        plan_guided(view, root, bodies, weights, policy, base, local, &alloc)?;
    let (func, imported_refs) =
        emit(&nodes, &children, bodies, policy.allow_external_refs, false, &alloc)?;
    let mut read = Vec::with_capacity_in(nodes.len().saturating_sub(1), alloc);
    read.extend(nodes.iter().skip(1).map(|node| node.id));
    read.sort_unstable();
    read.dedup();
    Ok(GuidedInlined { func, read, imported_refs })
}

/// Maximum number of scalar/vector return values supported for multi-return inlining.
const MAX_MULTI_RET: usize = 8;

/// Decoded call or invoke site operands from a body's `extra` column.
#[derive(Clone, Copy)]
struct CallSite<'v> {
    reference: RefId,
    sig: SigId,
    args: &'v [u32],
    /// For `Op::Invoke`, its normal continuation block in the caller; `None` for `Op::Call`.
    invoke_normal: Option<u32>,
    /// For `Op::Invoke`, its unwind landing block in the caller; `None` for `Op::Call`.
    invoke_unwind: Option<u32>,
}

/// Extracts callee ref, signature, arguments, and optional `Op::Invoke` normal/unwind blocks.
fn call_site(view: View<'_>, at: usize) -> Option<CallSite<'_>> {
    let op = view.op(at);
    let data_word = *view.data.get(at)?;
    let start = usize::try_from(data_word).ok()?;
    let words = view.extra.get(start..)?;
    match op {
        Op::Call => {
            let reference = RefId(*words.first()?);
            let sig = SigId(*words.get(1)?);
            let count = usize::try_from(*words.get(2)?).ok()?;
            let end = 3usize.checked_add(count)?;
            let args = words.get(3..end)?;
            Some(CallSite { reference, sig, args, invoke_normal: None, invoke_unwind: None })
        }
        Op::Invoke => {
            let reference = RefId(*words.first()?);
            let sig = SigId(*words.get(1)?);
            let normal = *words.get(2)?;
            let unwind = *words.get(3)?;
            let count = usize::try_from(*words.get(4)?).ok()?;
            let end = 5usize.checked_add(count)?;
            let args = words.get(5..end)?;
            Some(CallSite { reference, sig, args, invoke_normal: Some(normal), invoke_unwind: Some(unwind) })
        }
        _ => None,
    }
}

/// One body in the inlining tree: the root (node 0) or an instance spliced at `site` of
/// `parent`.
#[derive(Clone, Copy)]
struct Node<'v> {
    id: BodyId,
    view: View<'v>,
    parent: u32,
    site: usize,
    depth: u8,
    /// Enclosing loop depth inherited from the call site that spliced this node.
    loop_depth: u8,
    /// Enclosing call-site weight inherited from the call site that spliced this node.
    site_weight: u64,
    /// Whether the parent's site instruction is `Op::Invoke` (rather than `Op::Call`).
    is_invoke: bool,
    /// For `Op::Invoke`, the `BlockKind` of the parent's unwind landing block.
    invoke_unwind_kind: Option<BlockKind>,
    /// Whether the parent's call site lies inside a cleanup/terminate path of the parent.
    in_cleanup: bool,
}

#[inline]
fn new_map<K, V, A: Allocator + Copy>(alloc: &A) -> Map<K, V, FixedState, A> {
    Map::new(MapOpts { hasher: FixedState, alloc: *alloc, capacity: None })
}

fn too_large(table: &'static str) -> InlineError {
    InlineError::Build(BuildError::Full { table })
}

fn row(len: usize, table: &'static str) -> Result<u32, InlineError> {
    u32::try_from(len).map_err(|_| too_large(table))
}

fn filled<T: Clone, A: Allocator + Copy>(len: usize, item: T, alloc: &A) -> Vec<T, A> {
    let mut column = Vec::with_capacity_in(len, *alloc);
    column.resize(len, item);
    column
}

fn own_filled<T: Clone, A: Allocator + Copy>(len: usize, item: T, alloc: &A) -> OwnVec<T, A> {
    OwnVec::from_elem_in(item, len, *alloc)
}

/// Cached [`FnSummary`] lookup for `id`/`view`, consulting optional read-only `base` first.
fn summary_of_with_base<A: Allocator + Copy, B: Allocator>(
    id: BodyId,
    view: View<'_>,
    base: Option<&Map<BodyId, FnSummary, FixedState, B>>,
    summaries: &mut Map<BodyId, FnSummary, FixedState, A>,
    alloc: &A,
) -> Result<FnSummary, InlineError> {
    if let Some(&summary) = summaries.get(&id) {
        return Ok(summary);
    }
    if let Some(base_map) = base
        && let Some(&summary) = base_map.get(&id)
    {
        return Ok(summary);
    }
    let summary = FnSummary::try_compute(view, alloc)
        .map_err(|e| InlineError::Refused { inst: e.inst, what: e.what })?;
    summaries.insert(id, summary);
    Ok(summary)
}

/// Cached [`FnSummary`] lookup for `id`/`view`.
fn summary_of<A: Allocator + Copy>(
    id: BodyId,
    view: View<'_>,
    summaries: &mut Map<BodyId, FnSummary, FixedState, A>,
    alloc: &A,
) -> Result<FnSummary, InlineError> {
    summary_of_with_base(id, view, None::<&Map<BodyId, FnSummary, FixedState, A>>, summaries, alloc)
}

/// Hands `f` every reference instruction `at` names.
fn each_ref(view: View<'_>, at: usize, f: &mut impl FnMut(RefId)) -> Result<(), InlineError> {
    match view.op(at).info().layout {
        Layout::Data(Field::Ref) => f(RefId(view.data[at])),
        Layout::Extra(fields) => {
            let words = &view.extra[view.data[at] as usize..];
            walk(words, fields, Ty(view.tys[at]), view.tys, &mut |word| -> Result<(), Infallible> {
                if let Word::Of(Field::Ref, reference) = word {
                    f(RefId(reference));
                }
                Ok(())
            })
            .map_err(|_| InlineError::Refused { inst: at, what: "operands that run past their words" })?;
        }
        Layout::None | Layout::Data(_) => {}
    }
    Ok(())
}

/// Hands `f` every block the terminator `at` leads to.
fn each_successor(view: View<'_>, at: usize, f: &mut impl FnMut(u32)) -> Result<(), InlineError> {
    if let Layout::Extra(fields) = view.op(at).info().layout {
        let words = &view.extra[view.data[at] as usize..];
        walk(words, fields, Ty(view.tys[at]), view.tys, &mut |word| -> Result<(), Infallible> {
            if let Word::Block(block) = word {
                f(block);
            }
            Ok(())
        })
        .map_err(|_| InlineError::Refused { inst: at, what: "operands that run past their words" })?;
    }
    Ok(())
}

/// Whether every reference node `n` keeps in the output is carried (used when
/// `allow_external_refs` is false).
fn closed_with<A: Allocator + Copy>(
    view: View<'_>,
    summary: FnSummary,
    n: u32,
    children: &Map<(u32, usize), u32, FixedState, A>,
) -> Result<bool, InlineError> {
    if summary.is_closed {
        return Ok(true);
    }
    for at in 0..view.len() {
        let mut open = false;
        each_ref(view, at, &mut |reference| open |= !carried(view, reference))?;
        if open && !(matches!(view.op(at), Op::Call | Op::Invoke) && children.contains_key(&(n, at))) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Computes which blocks of `view` are reachable from any `BlockKind::Cleanup` or
/// `BlockKind::Terminate` landing block via ordinary (non-unwind) edges — matching
/// `blaze-emit`'s cleanup-call rule (a plain `Op::Call` inside cleanup aborts on panic rather
/// than unwinding to an outer handler). Returns an empty vector when `view` has no cleanup or
/// terminate blocks (fast path: zero allocation).
fn cleanup_blocks<A: Allocator + Copy>(
    view: View<'_>,
    alloc: &A,
) -> Result<OwnVec<bool, A>, InlineError> {
    let blocks = view.blocks();
    let mut any = false;
    for b in 0..blocks {
        let Ok(b_u32) = u32::try_from(b) else { break };
        if matches!(view.block_kind(Block(b_u32)), BlockKind::Cleanup | BlockKind::Terminate) {
            any = true;
            break;
        }
    }
    if !any {
        return Ok(OwnVec::new_in(*alloc));
    }
    let mut is_cleanup = own_filled(blocks, false, alloc);
    let mut todo: OwnVec<u32, A> = OwnVec::new_in(*alloc);
    for b in 0..blocks {
        let Ok(b_u32) = u32::try_from(b) else { break };
        if matches!(view.block_kind(Block(b_u32)), BlockKind::Cleanup | BlockKind::Terminate) {
            if let Some(slot) = is_cleanup.get_mut(b) {
                *slot = true;
            }
            todo.push(b_u32);
        }
    }
    while let Some(cb) = todo.pop() {
        let term = view.terminator(Block(cb));
        let op = view.op(term);
        if matches!(op, Op::Invoke | Op::InvokeIndirect) {
            if let Some(start) = view.data.get(term).and_then(|&d| usize::try_from(d).ok())
                && let Some(&normal) = view.extra.get(start.saturating_add(2))
                && let Ok(n_idx) = usize::try_from(normal)
                && let Some(slot) = is_cleanup.get_mut(n_idx)
                && !*slot
            {
                *slot = true;
                todo.push(normal);
            }
        } else {
            each_successor(view, term, &mut |to| {
                if let Ok(t_idx) = usize::try_from(to)
                    && let Some(slot) = is_cleanup.get_mut(t_idx)
                    && !*slot
                {
                    *slot = true;
                    todo.push(to);
                }
            })?;
        }
    }
    Ok(is_cleanup)
}

/// Returns the block containing instruction `inst` in `view`.
fn block_of_inst(view: View<'_>, inst: usize) -> Option<u32> {
    let pos = view.block_starts.partition_point(|&start| usize::try_from(start).ok().is_some_and(|s| s <= inst));
    let blk = pos.checked_sub(1)?;
    u32::try_from(blk).ok()
}

/// Summary of residual exception-handling behavior for node `n` after splicing its planned
/// `children`, used to decide legality of inlining into `Catch` / `Terminate` invoke sites and
/// cleanup-path calls.
#[derive(Clone, Copy, Default)]
struct ResidualEh {
    /// Whether `n` (or any spliced child) executes any potentially-unwinding operation.
    may_unwind: bool,
    /// Whether `n` (or any spliced child) retains an inner landing-pad handler (`Invoke`,
    /// `InvokeIndirect`, or `Resume`), which must not shadow an outer `Catch` / `Terminate` pad.
    has_inner_eh: bool,
    /// Whether `n` (or any spliced non-cleanup child) executes an `Op::Resume` that propagates
    /// out of `n`.
    has_resume: bool,
}

/// Whether the `Op::Call` / `Op::CallIndirect` instruction at `at` in `view` invokes a C-variadic
/// signature (`SigRec::VARIADIC`). In Rust/blaze-build semantics a `"C"` variadic callee does not
/// unwind (`blaze-build/src/walk/c_variadic.rs`) and `blaze-emit/src/pass/variadic.rs` refuses
/// `Op::Invoke` with stacked variadic arguments; the inliner must never promote it to `Op::Invoke`.
fn call_is_variadic(view: View<'_>, at: usize) -> bool {
    let Some(&extra_start) = view.data.get(at) else { return false };
    let Ok(start) = usize::try_from(extra_start) else { return false };
    let Some(&sig_word) = view.extra.get(start.saturating_add(1)) else { return false };
    let Ok(sig_idx) = usize::try_from(sig_word) else { return false };
    view.sigs.get(sig_idx).is_some_and(|s| (s.bits & SigRec::VARIADIC) != 0)
}

/// Computes [`ResidualEh`] for node `n` bottom-up from its already-computed `child_eh` entries.
fn residual_eh_with<A: Allocator + Copy>(
    view: View<'_>,
    n: u32,
    children: &Map<(u32, usize), u32, FixedState, A>,
    child_eh: &[ResidualEh],
    cleanup: &[bool],
) -> ResidualEh {
    let mut out = ResidualEh::default();
    for at in 0..view.len() {
        let op = view.op(at);
        match op {
            Op::Call => {
                if let Some(&child) = children.get(&(n, at)) {
                    let c_eh = usize::try_from(child)
                        .ok()
                        .and_then(|ci| child_eh.get(ci).copied())
                        .unwrap_or(ResidualEh { may_unwind: true, has_inner_eh: true, has_resume: true });
                    let in_clean = block_of_inst(view, at)
                        .and_then(|b| usize::try_from(b).ok())
                        .and_then(|bi| cleanup.get(bi).copied())
                        .unwrap_or(false);
                    if !in_clean {
                        out.may_unwind |= c_eh.may_unwind;
                        out.has_inner_eh |= c_eh.has_inner_eh;
                        out.has_resume |= c_eh.has_resume;
                    }
                } else {
                    let in_clean = block_of_inst(view, at)
                        .and_then(|b| usize::try_from(b).ok())
                        .and_then(|bi| cleanup.get(bi).copied())
                        .unwrap_or(false);
                    if !in_clean && !call_is_variadic(view, at) {
                        out.may_unwind = true;
                    }
                }
            }
            Op::Invoke => {
                if let Some(&child) = children.get(&(n, at)) {
                    let c_eh = usize::try_from(child)
                        .ok()
                        .and_then(|ci| child_eh.get(ci).copied())
                        .unwrap_or(ResidualEh { may_unwind: true, has_inner_eh: true, has_resume: true });
                    if c_eh.may_unwind {
                        out.may_unwind = true;
                        out.has_inner_eh = true;
                    }
                    out.has_inner_eh |= c_eh.has_inner_eh;
                } else {
                    out.may_unwind = true;
                    out.has_inner_eh = true;
                }
            }
            Op::CallIndirect => {
                let in_clean = block_of_inst(view, at)
                    .and_then(|b| usize::try_from(b).ok())
                    .and_then(|bi| cleanup.get(bi).copied())
                    .unwrap_or(false);
                if !in_clean && !call_is_variadic(view, at) {
                    out.may_unwind = true;
                }
            }
            Op::InvokeIndirect => {
                out.may_unwind = true;
                out.has_inner_eh = true;
            }
            Op::Resume => {
                out.may_unwind = true;
                out.has_inner_eh = true;
                out.has_resume = true;
            }
            _ => {}
        }
    }
    out
}

/// Returns whether every use of multi-return call/invoke value `target` in `view` is an
/// `Op::Project(target, k)` with `k < ret_count`.
fn only_projected_uses(view: View<'_>, target: u32, ret_count: usize) -> Result<bool, InlineError> {
    for at in 0..view.len() {
        let op = view.op(at);
        if op == Op::Project {
            let Some(&data_word) = view.data.get(at) else {
                return Ok(false);
            };
            let Ok(start) = usize::try_from(data_word) else {
                return Ok(false);
            };
            let Some(words) = view.extra.get(start..) else {
                return Ok(false);
            };
            let Some(&src) = words.first() else {
                return Ok(false);
            };
            if src == target {
                let Some(&idx_word) = words.get(1) else {
                    return Ok(false);
                };
                let Ok(idx) = usize::try_from(idx_word) else {
                    return Ok(false);
                };
                if idx >= ret_count {
                    return Ok(false);
                }
            }
            continue;
        }
        match op.info().layout {
            Layout::None => {}
            Layout::Data(Field::Value) => {
                if view.data.get(at).copied() == Some(target) {
                    return Ok(false);
                }
            }
            Layout::Data(_) => {}
            Layout::Extra(fields) => {
                let Some(&data_word) = view.data.get(at) else {
                    return Ok(false);
                };
                let Ok(start) = usize::try_from(data_word) else {
                    return Ok(false);
                };
                let Some(words) = view.extra.get(start..) else {
                    return Ok(false);
                };
                let ty = view.tys.get(at).copied().map_or(Ty::NONE, Ty);
                let mut bad = false;
                walk(words, fields, ty, view.tys, &mut |word| -> Result<(), Infallible> {
                    if let Word::Value(v) = word
                        && v == target
                    {
                        bad = true;
                    }
                    Ok(())
                })
                .map_err(|_| InlineError::Refused {
                    inst: at,
                    what: "operands that run past their words",
                })?;
                if bad {
                    return Ok(false);
                }
            }
        }
    }
    Ok(true)
}

/// Checks ABI/structural legality shared by `inline_trivial` and `inline_guided`:
/// pointer width, ancestor cycle check, features, signature, and param/return types
/// (including multi-return compound calls projected solely via `Op::Project`).
fn check_legal(
    nodes: &[Node<'_>],
    n: u32,
    at: usize,
    site_sig: SigId,
    id: BodyId,
    callee: View<'_>,
    args: &[u32],
) -> Result<bool, InlineError> {
    let Ok(n_idx) = usize::try_from(n) else {
        return Ok(false);
    };
    let Some(&caller) = nodes.get(n_idx) else {
        return Ok(false);
    };
    let Some(&root_node) = nodes.first() else {
        return Ok(false);
    };
    let view = caller.view;
    let root = root_node.view;
    if callee.head.ptr_bits != root.head.ptr_bits {
        return Err(InlineError::Refused {
            inst: at,
            what: "a callee of another pointer width than its caller",
        });
    }
    // Never a body already on the inlining stack.
    let mut up = n;
    loop {
        let Ok(up_idx) = usize::try_from(up) else {
            return Ok(false);
        };
        let Some(&up_node) = nodes.get(up_idx) else {
            return Ok(false);
        };
        if up_node.id == id {
            return Ok(false);
        }
        if up == 0 {
            break;
        }
        up = up_node.parent;
    }
    if callee.features != root.features {
        return Ok(false);
    }
    let sig = callee.sig(SigId(callee.head.sig));
    if sig.bits & SigRec::VARIADIC != 0
        || sig.params.iter().any(|param| ParamKind::from_byte(param.kind) == Some(ParamKind::Byval))
    {
        return Ok(false);
    }
    let has = view.tys.get(at).copied().map_or(Ty::NONE, Ty);
    match sig.rets {
        [] => {
            if !has.is_none() {
                return Ok(false);
            }
        }
        [one] => {
            if Ty(one.ty) != has || has.compound_row().is_some() {
                return Ok(false);
            }
        }
        many => {
            if many.len() > MAX_MULTI_RET || has.compound_row().is_none() {
                return Ok(false);
            }
            let members = view.members(has);
            if members.len() != many.len()
                || !members
                    .iter()
                    .zip(many.iter())
                    .all(|(&m, ret)| m == ret.ty && Ty(m).compound_row().is_none())
            {
                return Ok(false);
            }
            let caller_sig = view.sig(site_sig);
            if caller_sig.rets.len() != many.len()
                || !caller_sig.rets.iter().zip(many.iter()).all(|(a, b)| a.ty == b.ty)
            {
                return Ok(false);
            }
            let Ok(at_u32) = u32::try_from(at) else {
                return Ok(false);
            };
            if !only_projected_uses(view, at_u32, many.len())? {
                return Ok(false);
            }
        }
    }
    let params = callee.params(Block(0));
    if params.len() != args.len() {
        return Ok(false);
    }
    for (k, param) in params.enumerate() {
        let Some(&arg) = args.get(k) else {
            return Ok(false);
        };
        let Ok(arg_idx) = usize::try_from(arg) else {
            return Ok(false);
        };
        let Some(&want_ty) = callee.tys.get(param) else {
            return Ok(false);
        };
        let Some(&has_ty) = view.tys.get(arg_idx) else {
            return Ok(false);
        };
        let (want, has) = (Ty(want_ty), Ty(has_ty));
        if want != has || want.compound_row().is_some() {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The callee of the call `at` of node `n`, if the trivial site is inlined.
fn candidate_trivial<'v, A: Allocator + Copy>(
    nodes: &[Node<'v>],
    n: u32,
    at: usize,
    bodies: &'v dyn Bodies,
    bound: Bound,
    used: u64,
    summaries: &mut Map<BodyId, FnSummary, FixedState, A>,
    alloc: &A,
) -> Result<Option<(BodyId, View<'v>, FnSummary)>, InlineError> {
    let Ok(n_idx) = usize::try_from(n) else {
        return Ok(None);
    };
    let Some(&caller) = nodes.get(n_idx) else {
        return Ok(None);
    };
    let Some(&root_node) = nodes.first() else {
        return Ok(None);
    };
    let view = caller.view;
    let Some(cs) = call_site(view, at) else {
        return Ok(None);
    };
    let Some((id, callee)) = bodies.callee(caller.id, cs.reference) else {
        return Ok(None);
    };
    let root = root_node.view;
    if callee.head.ptr_bits != root.head.ptr_bits {
        return Err(InlineError::Refused {
            inst: at,
            what: "a callee of another pointer width than its caller",
        });
    }
    if caller.depth >= bound.depth {
        return Ok(None);
    }
    let callee_len = u64::try_from(callee.len()).unwrap_or(u64::MAX);
    if used.saturating_add(callee_len) > u64::from(bound.growth) {
        return Ok(None);
    }
    let summary = summary_of(id, callee, summaries, alloc)?;
    if summary.inline_attr == Inline::Never {
        return Ok(None);
    }
    if summary.inline_attr != Inline::Always
        && (summary.insts > bound.insts || summary.has_loops)
    {
        return Ok(None);
    }
    if summary.has_forbidden_ops || !summary.has_return {
        return Ok(None);
    }
    if !check_legal(nodes, n, at, cs.sig, id, callee, cs.args)? {
        return Ok(None);
    }
    Ok(Some((id, callee, summary)))
}

/// The trivial inlining tree, in the output's layout order (preorder): node 0 is the root.
fn plan_trivial<'v, A: Allocator + Copy>(
    view: View<'v>,
    root: BodyId,
    bodies: &'v dyn Bodies,
    bound: Bound,
    summaries: &mut Map<BodyId, FnSummary, FixedState, A>,
    alloc: &A,
) -> Result<(Vec<Node<'v>, A>, Map<(u32, usize), u32, FixedState, A>), InlineError> {
    let mut nodes = Vec::new_in(*alloc);
    let mut node_summaries: OwnVec<Option<FnSummary>, A> = OwnVec::new_in(*alloc);
    let mut children: Map<(u32, usize), u32, FixedState, A> = new_map(alloc);
    nodes.push(Node {
        id: root,
        view,
        parent: NONE,
        site: 0,
        depth: 0,
        loop_depth: 0,
        site_weight: 0,
        is_invoke: false,
        invoke_unwind_kind: None,
        in_cleanup: false,
    });
    node_summaries.push(None);
    // Per node being walked: the node and its next instruction.
    let mut stack: OwnVec<(u32, usize), A> = OwnVec::new_in(*alloc);
    stack.push((0, 0));
    let mut used = 0u64;
    while let Some(&(n, at)) = stack.last() {
        let body = nodes[n as usize].view;
        if at == body.len() {
            stack.pop();
            if n != 0 {
                let summary = node_summaries[n as usize].expect("callee summary recorded");
                if !closed_with(body, summary, n, &children)? {
                    for node in &nodes[n as usize..] {
                        used -= node.view.len() as u64;
                        children.remove(&(node.parent, node.site));
                    }
                    nodes.truncate(n as usize);
                    node_summaries.truncate(n as usize);
                }
            }
            continue;
        }
        if let Some(top) = stack.last_mut() {
            top.1 = at + 1;
        }
        if body.op(at) != Op::Call {
            continue;
        }
        if let Some((id, callee, summary)) =
            candidate_trivial(&nodes, n, at, bodies, bound, used, summaries, alloc)?
        {
            let child = row(nodes.len(), "inlined bodies")?;
            used += callee.len() as u64;
            let depth = nodes[n as usize].depth + 1;
            nodes.push(Node {
                id,
                view: callee,
                parent: n,
                site: at,
                depth,
                loop_depth: 0,
                site_weight: 0,
                is_invoke: false,
                invoke_unwind_kind: None,
                in_cleanup: false,
            });
            node_summaries.push(Some(summary));
            children.insert((n, at), child);
            stack.push((child, 0));
        }
    }
    Ok((nodes, children))
}

/// Computes natural loop depth per block of `view` via iterative DFS back-edge detection.
/// Fast path: returns all zeroes with zero graph allocation when all edges go forward (`to > from`).
fn loop_depths<A: Allocator + Copy>(view: View<'_>, alloc: &A) -> Result<OwnVec<u8, A>, InlineError> {
    let blocks = view.blocks();
    let mut depths = own_filled(blocks, 0u8, alloc);
    if blocks <= 1 {
        return Ok(depths);
    }
    let mut edges: OwnVec<(u32, u32), A> = OwnVec::new_in(*alloc);
    let mut succ_starts = own_filled(blocks + 1, 0usize, alloc);
    let mut any_back = false;
    for b in 0..blocks {
        succ_starts[b] = edges.len();
        let term = view.terminator(Block(b as u32));
        each_successor(view, term, &mut |to| {
            if (to as usize) <= b {
                any_back = true;
            }
            edges.push((b as u32, to));
        })?;
    }
    succ_starts[blocks] = edges.len();
    if !any_back {
        return Ok(depths);
    }

    // Build reverse adjacency (predecessors) in CSR.
    let mut pred_counts = own_filled(blocks + 1, 0usize, alloc);
    for &(_, to) in edges.iter() {
        if (to as usize) < blocks {
            pred_counts[to as usize + 1] += 1;
        }
    }
    for b in 0..blocks {
        pred_counts[b + 1] += pred_counts[b];
    }
    let pred_starts = pred_counts;
    let mut pred_fill = own_filled(blocks, 0usize, alloc);
    let mut preds = own_filled(pred_starts[blocks], 0u32, alloc);
    for &(from, to) in edges.iter() {
        let t = to as usize;
        if t < blocks {
            preds[pred_starts[t] + pred_fill[t]] = from;
            pred_fill[t] += 1;
        }
    }

    // Iterative DFS from block 0 to find back-edges `(u -> h)` where `h` is on stack.
    let mut state = own_filled(blocks, 0u8, alloc); // 0=unvisited, 1=on-stack, 2=done
    let mut dfs: OwnVec<(u32, usize), A> = OwnVec::with_capacity_in(blocks, *alloc);
    let mut back_edges: OwnVec<(u32, u32), A> = OwnVec::new_in(*alloc);
    state[0] = 1;
    dfs.push((0, succ_starts[0]));
    while let Some(top) = dfs.last_mut() {
        let b = top.0 as usize;
        if top.1 < succ_starts[b + 1] {
            let to = edges[top.1].1;
            top.1 += 1;
            let t = to as usize;
            if t >= blocks {
                continue;
            }
            match state[t] {
                0 => {
                    state[t] = 1;
                    dfs.push((to, succ_starts[t]));
                }
                1 => back_edges.push((top.0, to)),
                _ => {}
            }
        } else {
            state[b] = 2;
            dfs.pop();
        }
    }
    if back_edges.is_empty() {
        return Ok(depths);
    }

    // Group back-edges by header `h` (sorted deterministically) and mark each natural loop once.
    back_edges.sort_unstable_by_key(|&(u, h)| (h, u));
    let mut seen_stamp = own_filled(blocks, 0u32, alloc);
    let mut stamp = 0u32;
    let mut work: OwnVec<u32, A> = OwnVec::with_capacity_in(blocks, *alloc);
    let mut i = 0;
    while i < back_edges.len() {
        let header = back_edges[i].1;
        stamp += 1;
        seen_stamp[header as usize] = stamp;
        depths[header as usize] = depths[header as usize].saturating_add(1);
        while i < back_edges.len() && back_edges[i].1 == header {
            let tail = back_edges[i].0;
            if seen_stamp[tail as usize] != stamp {
                seen_stamp[tail as usize] = stamp;
                depths[tail as usize] = depths[tail as usize].saturating_add(1);
                work.push(tail);
            }
            i += 1;
        }
        while let Some(x) = work.pop() {
            let xs = x as usize;
            for &p in &preds[pred_starts[xs]..pred_starts[xs + 1]] {
                let ps = p as usize;
                if seen_stamp[ps] != stamp {
                    seen_stamp[ps] = stamp;
                    depths[ps] = depths[ps].saturating_add(1);
                    work.push(p);
                }
            }
        }
    }
    Ok(depths)
}

/// Allocator-aware deterministic binary max-heap (zero global-heap allocations).
struct MaxHeap<T, A: Allocator> {
    data: OwnVec<T, A>,
}

impl<T: Ord, A: Allocator + Copy> MaxHeap<T, A> {
    fn new_in(alloc: &A) -> Self {
        Self { data: OwnVec::new_in(*alloc) }
    }

    fn push(&mut self, item: T) {
        let mut idx = self.data.len();
        self.data.push(item);
        while idx > 0 {
            let parent = (idx - 1) / 2;
            if self.data[idx] > self.data[parent] {
                self.data.swap(idx, parent);
                idx = parent;
            } else {
                break;
            }
        }
    }

    fn pop(&mut self) -> Option<T> {
        if self.data.is_empty() {
            return None;
        }
        let last = self.data.len() - 1;
        self.data.swap(0, last);
        let top = self.data.pop();
        let len = self.data.len();
        let mut idx = 0;
        loop {
            let left = 2 * idx + 1;
            let right = left + 1;
            let mut largest = idx;
            if left < len && self.data[left] > self.data[largest] {
                largest = left;
            }
            if right < len && self.data[right] > self.data[largest] {
                largest = right;
            }
            if largest == idx {
                break;
            }
            self.data.swap(idx, largest);
            idx = largest;
        }
        top
    }
}

/// One candidate call site in [`plan_guided`]'s priority worklist.
struct SiteCandidate<'v> {
    always: bool,
    score: u64,
    /// Position of the enclosing top-level call site in root (layout-order tie-breaker).
    root_site: usize,
    depth: u8,
    loop_depth: u8,
    site_weight: u64,
    is_invoke: bool,
    invoke_unwind_kind: Option<BlockKind>,
    in_cleanup: bool,
    parent: u32,
    site: usize,
    callee_id: BodyId,
    callee_view: View<'v>,
    callee_summary: FnSummary,
}

impl PartialEq for SiteCandidate<'_> {
    fn eq(&self, other: &Self) -> bool {
        (self.always, self.score, self.root_site, self.depth, self.parent, self.site)
            == (other.always, other.score, other.root_site, other.depth, other.parent, other.site)
    }
}

impl Eq for SiteCandidate<'_> {}

impl PartialOrd for SiteCandidate<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for SiteCandidate<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        // Max-heap: higher `always` and `score` win; ties broken deterministically by
        // earlier `(root_site, depth, parent, site)` (smaller wins).
        self.always
            .cmp(&other.always)
            .then_with(|| self.score.cmp(&other.score))
            .then_with(|| other.root_site.cmp(&self.root_site))
            .then_with(|| other.depth.cmp(&self.depth))
            .then_with(|| other.parent.cmp(&self.parent))
            .then_with(|| other.site.cmp(&self.site))
    }
}

/// Scans node `n`'s blocks and pushes all policy-eligible call/invoke sites onto `heap`.
#[expect(clippy::too_many_arguments)]
fn enqueue_sites<'v, A: Allocator + Copy, B: Allocator>(
    nodes: &[Node<'v>],
    n: u32,
    root_site_for_node: usize,
    bodies: &'v dyn Bodies,
    weights: &dyn SiteWeights,
    policy: InlinePolicy,
    base: Option<&Map<BodyId, FnSummary, FixedState, B>>,
    summaries: &mut Map<BodyId, FnSummary, FixedState, A>,
    heap: &mut MaxHeap<SiteCandidate<'v>, A>,
    alloc: &A,
) -> Result<(), InlineError> {
    let Ok(n_idx) = usize::try_from(n) else {
        return Ok(());
    };
    let Some(&caller) = nodes.get(n_idx) else {
        return Ok(());
    };
    let Some(&root_node) = nodes.first() else {
        return Ok(());
    };
    if caller.depth >= policy.max_depth {
        return Ok(());
    }
    let view = caller.view;
    let depths = loop_depths(view, alloc)?;
    let cleanup = cleanup_blocks(view, alloc)?;
    let blocks = view.blocks();
    for b in 0..blocks {
        let Ok(b_u32) = u32::try_from(b) else {
            break;
        };
        let block = Block(b_u32);
        let block_kind = view.block_kind(block);
        let in_cleanup = cleanup.get(b).copied().unwrap_or(false);
        let block_loop_depth = depths.get(b).copied().unwrap_or(0);
        let loop_depth = caller.loop_depth.saturating_add(block_loop_depth);
        for at in view.block(block) {
            let Some(cs) = call_site(view, at) else {
                continue;
            };
            let is_invoke = cs.invoke_normal.is_some();
            let invoke_unwind_kind = cs.invoke_unwind.map(|u| view.block_kind(Block(u)));
            let Some((id, callee)) = bodies.callee(caller.id, cs.reference) else {
                continue;
            };
            let root = root_node.view;
            if callee.head.ptr_bits != root.head.ptr_bits {
                return Err(InlineError::Refused {
                    inst: at,
                    what: "a callee of another pointer width than its caller",
                });
            }
            let summary = summary_of_with_base(id, callee, base, summaries, alloc)?;
            if summary.inline_attr == Inline::Never
                || summary.has_forbidden_ops
                || !summary.has_return
            {
                continue;
            }
            let always = summary.inline_attr == Inline::Always;
            if !always {
                // Strong refusal for cold/landing call sites and `#[cold]` callees.
                if block_kind != BlockKind::Normal || summary.is_cold {
                    continue;
                }
                if !policy.allow_loops && summary.has_loops {
                    continue;
                }
                if summary.insts > policy.max_callee_insts {
                    continue;
                }
            }
            if !check_legal(nodes, n, at, cs.sig, id, callee, cs.args)? {
                continue;
            }

            // Specialization bonuses from call-site arguments.
            let mut const_hits = 0u64;
            let mut sroa_hits = 0u64;
            let mut devirt_hits = 0u64;
            for (k, &arg) in cs.args.iter().enumerate() {
                let Ok(arg_idx) = usize::try_from(arg) else {
                    continue;
                };
                if arg_idx >= view.len() {
                    continue;
                }
                let arg_op = view.op(arg_idx);
                if k < 64 {
                    let bit = 1u64 << k;
                    if (summary.const_param_mask & bit) != 0
                        && matches!(
                            arg_op,
                            Op::Iconst
                                | Op::IconstWide
                                | Op::Fconst
                                | Op::FconstWide
                                | Op::Null
                        )
                    {
                        const_hits += 1;
                    }
                    let is_slot_ptr = arg_op == Op::SlotAddr
                        || (arg_op == Op::PtrAdd
                            && view
                                .data
                                .get(arg_idx)
                                .and_then(|&d| usize::try_from(d).ok())
                                .and_then(|start| view.extra.get(start))
                                .and_then(|&base| usize::try_from(base).ok())
                                .is_some_and(|base_idx| {
                                    base_idx < view.len() && view.op(base_idx) == Op::SlotAddr
                                }));
                    if (summary.sroa_ptr_param_mask & bit) != 0 && is_slot_ptr {
                        sroa_hits += 1;
                    }
                }
                if arg_op == Op::FuncAddr {
                    devirt_hits += 1;
                }
            }

            let base_cost = u64::from(summary.insts)
                + if summary.has_loops && !always { 16 } else { 0 };
            let Ok(at_u32) = u32::try_from(at) else {
                continue;
            };
            let own_weight = weights.weight(caller.id, at_u32);
            let site_weight = if n == 0 {
                own_weight
            } else {
                own_weight.max(caller.site_weight)
            };
            if !always {
                let mut threshold = u64::from(policy.cost_threshold);
                if summary.inline_attr == Inline::Hint {
                    threshold = threshold.saturating_mul(2);
                }
                let is_hot =
                    site_weight >= policy.hot_weight_threshold || loop_depth > 0;
                if is_hot {
                    threshold = threshold.saturating_mul(u64::from(policy.hot_multiplier.max(1)));
                }
                if loop_depth > 1 {
                    threshold = threshold.saturating_add(
                        u64::from(policy.cost_threshold)
                            .saturating_mul(u64::from(loop_depth - 1)),
                    );
                }
                threshold = threshold
                    .saturating_add(const_hits.saturating_mul(16))
                    .saturating_add(sroa_hits.saturating_mul(24))
                    .saturating_add(devirt_hits.saturating_mul(20));
                if base_cost > threshold {
                    continue;
                }
            }

            // Priority score: monotonic in PGO weight, loop depth, hints, specialization, and inverse size.
            let w = site_weight.saturating_add(1).min(1u64 << 36);
            let loop_mult = 1u64 << loop_depth.min(6);
            let hint_mult = if summary.inline_attr == Inline::Hint { 2u64 } else { 1u64 };
            let spec_pct = 100u64
                + const_hits.saturating_mul(40)
                + sroa_hits.saturating_mul(60)
                + devirt_hits.saturating_mul(50);
            let score = w
                .saturating_mul(loop_mult)
                .saturating_mul(hint_mult)
                .saturating_mul(spec_pct)
                / base_cost.max(1);

            let root_site = if n == 0 { at } else { root_site_for_node };
            heap.push(SiteCandidate {
                always,
                score,
                root_site,
                depth: caller.depth.saturating_add(1),
                loop_depth,
                site_weight,
                is_invoke,
                invoke_unwind_kind,
                in_cleanup,
                parent: n,
                site: at,
                callee_id: id,
                callee_view: callee,
                callee_summary: summary,
            });
        }
    }
    Ok(())
}

/// Priority-scored planning for [`inline_guided`], canonicalized into DFS preorder for [`emit`].
#[expect(clippy::too_many_arguments)]
fn plan_guided<'v, A: Allocator + Copy, B: Allocator>(
    view: View<'v>,
    root: BodyId,
    bodies: &'v dyn Bodies,
    weights: &dyn SiteWeights,
    policy: InlinePolicy,
    base: Option<&Map<BodyId, FnSummary, FixedState, B>>,
    summaries: &mut Map<BodyId, FnSummary, FixedState, A>,
    alloc: &A,
) -> Result<(Vec<Node<'v>, A>, Map<(u32, usize), u32, FixedState, A>), InlineError> {
    let mut raw_nodes: OwnVec<Node<'v>, A> = OwnVec::new_in(*alloc);
    let mut raw_summaries: OwnVec<Option<FnSummary>, A> = OwnVec::new_in(*alloc);
    let mut raw_children: Map<(u32, usize), u32, FixedState, A> = new_map(alloc);
    raw_nodes.push(Node {
        id: root,
        view,
        parent: NONE,
        site: 0,
        depth: 0,
        loop_depth: 0,
        site_weight: 0,
        is_invoke: false,
        invoke_unwind_kind: None,
        in_cleanup: false,
    });
    raw_summaries.push(None);

    let mut heap = MaxHeap::new_in(alloc);
    enqueue_sites(&raw_nodes, 0, 0, bodies, weights, policy, base, summaries, &mut heap, alloc)?;

    let mut used = 0u64;
    while let Some(cand) = heap.pop() {
        let added = u64::try_from(cand.callee_view.len()).unwrap_or(u64::MAX);
        if used.saturating_add(added) > u64::from(policy.max_growth) {
            continue;
        }
        let child = row(raw_nodes.len(), "inlined bodies")?;
        used = used.saturating_add(added);
        raw_nodes.push(Node {
            id: cand.callee_id,
            view: cand.callee_view,
            parent: cand.parent,
            site: cand.site,
            depth: cand.depth,
            loop_depth: cand.loop_depth,
            site_weight: cand.site_weight,
            is_invoke: cand.is_invoke,
            invoke_unwind_kind: cand.invoke_unwind_kind,
            in_cleanup: cand.in_cleanup,
        });
        raw_summaries.push(Some(cand.callee_summary));
        raw_children.insert((cand.parent, cand.site), child);
        enqueue_sites(
            &raw_nodes,
            child,
            cand.root_site,
            bodies,
            weights,
            policy,
            base,
            summaries,
            &mut heap,
            alloc,
        )?;
    }

    // Bottom-up pruning in reverse creation order (every child has index > parent):
    // - when `allow_external_refs` is false, prune open subtrees;
    // - for `Op::Invoke` targeting `BlockKind::Catch | BlockKind::Terminate`, prune only if the
    //   subtree retains an inner `Invoke`/`InvokeIndirect`/`Resume` that would shadow the outer
    //   `Catch`/`Terminate` LSDA action during DWARF Phase 1 (plain `Call`/`CallIndirect` rewrite
    //   directly to `Invoke`/`InvokeIndirect` targeting the `Catch`/`Terminate` pad and stay!);
    // - for `Op::Invoke` targeting `BlockKind::Cleanup`, ALL unwinding callees are kept and
    //   inlined (rewriting inner `Call`/`CallIndirect` -> `Invoke`/`InvokeIndirect` and inner
    //   `Resume` -> `Jump` to the split cleanup body);
    // - for plain `Op::Call` inside a cleanup path, prune if the callee retains `Op::Resume`.
    let needs_eh_prune = raw_nodes.iter().skip(1).any(|node| {
        matches!(node.invoke_unwind_kind, Some(BlockKind::Catch | BlockKind::Terminate))
            || (!node.is_invoke && node.in_cleanup)
    });
    let mut alive = own_filled(raw_nodes.len(), true, alloc);
    if (!policy.allow_external_refs || needs_eh_prune) && raw_nodes.len() > 1 {
        let mut node_eh = own_filled(raw_nodes.len(), ResidualEh::default(), alloc);
        for i in (1..raw_nodes.len()).rev() {
            if !alive.get(i).copied().unwrap_or(false) {
                continue;
            }
            let Some(&node) = raw_nodes.get(i) else {
                continue;
            };
            let Ok(i_u32) = u32::try_from(i) else {
                continue;
            };
            let mut keep = true;
            if !policy.allow_external_refs {
                if let Some(Some(summary)) = raw_summaries.get(i).copied() {
                    if !closed_with(node.view, summary, i_u32, &raw_children)? {
                        keep = false;
                    }
                } else {
                    keep = false;
                }
            }
            if needs_eh_prune {
                let cleanup = cleanup_blocks(node.view, alloc)?;
                let eh = residual_eh_with(node.view, i_u32, &raw_children, &node_eh, &cleanup);
                if let Some(slot) = node_eh.get_mut(i) {
                    *slot = eh;
                }
                if matches!(node.invoke_unwind_kind, Some(BlockKind::Catch | BlockKind::Terminate))
                    && eh.has_inner_eh
                {
                    keep = false;
                }
                if !node.is_invoke && node.in_cleanup && eh.has_resume {
                    keep = false;
                }
            }
            if !keep {
                if let Some(slot) = alive.get_mut(i) {
                    *slot = false;
                }
                if let Some(slot) = node_eh.get_mut(i) {
                    *slot = ResidualEh { may_unwind: true, has_inner_eh: true, has_resume: true };
                }
                raw_children.remove(&(node.parent, node.site));
            }
        }
        // Propagate dead parents downward.
        for i in 1..raw_nodes.len() {
            let parent_alive = raw_nodes
                .get(i)
                .and_then(|node| usize::try_from(node.parent).ok())
                .and_then(|p| alive.get(p).copied())
                .unwrap_or(false);
            if !parent_alive
                && let Some(slot) = alive.get_mut(i)
            {
                *slot = false;
            }
        }
    }

    // Canonicalize surviving nodes into DFS preorder (children sorted by `site` ascending)
    // so `emit` visits every frame in exact layout order.
    let mut adj: OwnVec<OwnVec<(usize, u32), A>, A> = OwnVec::with_capacity_in(raw_nodes.len(), *alloc);
    for _ in 0..raw_nodes.len() {
        adj.push(OwnVec::new_in(*alloc));
    }
    for i in 1..raw_nodes.len() {
        if alive[i] {
            let node = raw_nodes[i];
            adj[node.parent as usize].push((node.site, i as u32));
        }
    }
    for list in adj.iter_mut() {
        list.sort_unstable_by_key(|&(site, _)| site);
    }

    let mut ordered = Vec::with_capacity_in(raw_nodes.len(), *alloc);
    let mut children: Map<(u32, usize), u32, FixedState, A> = new_map(alloc);
    let mut stack: OwnVec<(u32, u32), A> = OwnVec::with_capacity_in(raw_nodes.len(), *alloc);
    // Stack holds `(raw_idx, new_parent_idx)`.
    stack.push((0, NONE));
    while let Some((raw_idx, new_parent)) = stack.pop() {
        let new_idx = row(ordered.len(), "inlined bodies")?;
        let mut node = raw_nodes[raw_idx as usize];
        node.parent = new_parent;
        ordered.push(node);
        if new_parent != NONE {
            children.insert((new_parent, node.site), new_idx);
        }
        // Push children in reverse site order so pop yields ascending site order (DFS preorder).
        for &(_, child_raw) in adj[raw_idx as usize].iter().rev() {
            stack.push((child_raw, new_idx));
        }
    }
    Ok((ordered, children))
}

/// What a fixup names: a value or block of its frame's body, its frame's continuation, or the
/// non-landing body block split off a landing pad targeted by inlined `Op::Resume` jumps.
#[derive(Clone, Copy)]
enum Fix {
    Value,
    Block,
    Cont,
    ResumeBody,
}

/// Enclosing unwind landing pad inherited by an inlined frame (from the nearest ancestor
/// `Op::Invoke` site not separated by a cleanup-path call).
#[derive(Clone, Copy)]
struct UnwindTarget {
    frame: u32,
    block: u32,
}

/// A word of the output to set once every block is copied: `data[place]` if `in_data`, else
/// `extra[place]`, holding the body's own number until then.
#[derive(Clone, Copy)]
struct Fixup {
    place: u32,
    in_data: bool,
    frame: u32,
    fix: Fix,
}

/// One node as the output has it. Frame `k` is node `k`.
struct Frame<A: Allocator> {
    /// Whether this is the root, whose tables are the output's.
    root: bool,
    /// Per value of the body: its value here.
    values: OwnVec<u32, A>,
    /// Per block of the body: its block here.
    blocks: OwnVec<u32, A>,
    /// Per landing block split for inlined `Op::Resume` jumps: its `BlockKind::Cold` body block
    /// (empty when no `Op::Resume` targets any landing pad in this frame).
    resume_bodies: OwnVec<u32, A>,
    /// The block the body's returns jump to.
    cont: u32,
    /// Continuation block parameters created for multi-return callees (`rets.len() >= 2`).
    cont_params: [u32; MAX_MULTI_RET],
    /// Whether debug locations from this frame are dropped (e.g., spliced at a `loc == 0` site).
    drop_locs: bool,
    slot_off: u32,
    span_off: u32,
    range_off: u32,
    mem_off: u32,
    /// Per row of the body's tables: its row here, [`NONE`] until first named.
    refs: OwnVec<u32, A>,
    sigs: OwnVec<u32, A>,
    intrinsics: OwnVec<u32, A>,
}

/// Where the emission is in one frame: its body's block and next instruction.
#[derive(Clone, Copy)]
struct Cursor {
    frame: u32,
    block: u32,
    at: usize,
}

#[inline]
fn hash_val(v: impl Hash) -> u64 {
    let mut h = FixedState.build_hasher();
    v.hash(&mut h);
    h.finish()
}

/// O(1) expected-time table interning with exact equality check and collision fallback
/// (matching [`crate::builder::Builder`]'s `intern` pattern).
fn intern<A: Allocator + Copy>(
    map: &mut Map<u64, u32, FixedState, A>,
    hash: u64,
    rows: u32,
    same: impl Fn(u32) -> bool,
) -> Result<u32, u32> {
    match map.get(&hash) {
        Some(&r) if same(r) => Ok(r),
        Some(_) => (0..rows).find(|&r| same(r)).ok_or(rows),
        None => {
            map.insert(hash, rows);
            Err(rows)
        }
    }
}

/// Resolves the semantic [`Target`] of a non-data reference in `body`, checking both
/// [`Bodies::target`] and (for `RefKind::Func`) [`Bodies::callee`].
fn resolve_semantic_target(
    bodies: &dyn Bodies,
    body: BodyId,
    reference: RefId,
    kind: RefKind,
) -> Option<Target> {
    if let Some(target) = bodies.target(body, reference) {
        return Some(target);
    }
    if kind == RefKind::Func
        && let Some((callee_id, _)) = bodies.callee(body, reference)
    {
        return Some(Target::Func(callee_id));
    }
    None
}

struct Emit<'p, 'v, 'b, A: Allocator + Copy> {
    nodes: &'p [Node<'v>],
    children: &'p Map<(u32, usize), u32, FixedState, A>,
    bodies: &'b dyn Bodies,
    allow_external_refs: bool,
    out: Func<A>,
    frames: Vec<Frame<A>, A>,
    /// Per frame: cleanup/terminate-reachable block flags (empty if the body has no cleanup blocks).
    frame_cleanup: OwnVec<OwnVec<bool, A>, A>,
    /// Per frame: enclosing unwind landing pad inherited from an ancestor `Op::Invoke` site.
    frame_unwind: OwnVec<Option<UnwindTarget>, A>,
    /// `(frame, block)` landing pads that must be split into `(landing -> Jump(cold_body))`
    /// because an inlined callee's `Op::Resume` jumps into them.
    needs_resume_body: Map<(u32, u32), (), FixedState, A>,
    /// Per frame: whether `needs_resume_body` names at least one landing block in that frame.
    frame_needs_resume_body: OwnVec<bool, A>,
    fixups: Vec<Fixup, A>,
    /// Deferred `Op::Project(spliced_multi_ret_call, k)` aliases: `(frame, proj_inst, child_frame, ret_idx)`.
    project_aliases: OwnVec<(u32, usize, u32, u8), A>,
    /// O(1) interning tables for merged columns.
    compound_rows: Map<u64, u32, FixedState, A>,
    fact_rows: Map<ParamFactsRec, u32, FixedState, A>,
    sig_rows: Map<u64, u32, FixedState, A>,
    intrinsic_rows: Map<u64, u32, FixedState, A>,
    plain_data_ref_rows: Map<u64, u32, FixedState, A>,
    /// Deduplication for imported external refs by resolved `(ref_kind, Target)`.
    target_ref_rows: Map<(u32, Target), u32, FixedState, A>,
    /// Fallback deduplication for opaque external refs by `(source_body, source_ref)`.
    source_ref_rows: Map<(BodyId, RefId), u32, FixedState, A>,
    /// Next collision-free `place` per `RefKind` when appending imported external refs.
    next_place: [u32; 5],
    /// Newly imported external refs in deterministic order of creation.
    imported_refs: Vec<ImportedRef, A>,
    /// Whether the output's spans have scopes: as the root's do (all or none).
    scoped: bool,
    /// The plant (test builds only): swap the first spliced call's first two arguments.
    swap: bool,
    alloc: A,
}

fn remap_range(range: u32, off: u32) -> u32 {
    match range {
        0 => 0,
        r => r + off,
    }
}

impl<A: Allocator + Copy> Emit<'_, '_, '_, A> {
    fn seed_interners(&mut self) {
        let out = &self.out;
        for (i, rec) in out.compounds.iter().enumerate() {
            let members = &out.compound_tys[rec.first as usize..][..rec.len as usize];
            self.compound_rows.entry(hash_val(members)).or_insert(i as u32);
        }
        for (i, &rec) in out.param_facts.iter().enumerate() {
            self.fact_rows.entry(rec).or_insert(i as u32);
        }
        for (i, rec) in out.sigs.iter().enumerate() {
            let len = rec.params as usize + rec.rets as usize;
            let params = &out.sig_params[rec.first as usize..][..len];
            let h = hash_val((rec.conv, rec.bits, rec.params, rec.rets, params));
            self.sig_rows.entry(h).or_insert(i as u32);
        }
        for (i, rec) in out.intrinsics.iter().enumerate() {
            let name = &out.names[rec.name_first as usize..][..rec.name_len as usize];
            let h = hash_val((rec.sig, name));
            self.intrinsic_rows.entry(h).or_insert(i as u32);
        }
        let root_id = self.nodes[0].id;
        for (i, rec) in out.refs.iter().enumerate() {
            if let Some(kind) = RefKind::from_word(rec.kind) {
                let k_idx = kind as usize;
                let next = rec.place.saturating_add(1);
                if next > self.next_place[k_idx] {
                    self.next_place[k_idx] = next;
                }
                if kind == RefKind::Data {
                    if let Some(data) = out.datas.get(rec.place as usize)
                        && data.relocs_len == 0
                        && data.positions_len == 0
                    {
                        let bytes =
                            &out.data_bytes[data.bytes_first as usize..][..data.bytes_len as usize];
                        let h = hash_val((data.align, bytes));
                        self.plain_data_ref_rows.entry(h).or_insert(i as u32);
                    }
                } else if self.allow_external_refs
                    && let Some(target) =
                        resolve_semantic_target(self.bodies, root_id, RefId(i as u32), kind)
                {
                    self.target_ref_rows.entry((rec.kind, target)).or_insert(i as u32);
                }
            }
        }
    }

    fn begin(&mut self, kind: BlockKind) -> Result<u32, InlineError> {
        let number = row(self.out.block_starts.len(), "blocks")?;
        let start = row(self.out.ops.len(), "instructions")?;
        self.out.block_starts.push(start);
        self.out.block_kinds.push(kind as u8);
        Ok(number)
    }

    fn raw(&mut self, op: Op, flags: u8, ty: Ty, data: u32, attrs: u32, loc: u32) -> Result<u32, InlineError> {
        let at = row(self.out.ops.len(), "instructions")?;
        self.out.ops.push(op as u8);
        self.out.flags.push(flags);
        self.out.tys.push(ty.0);
        self.out.data.push(data);
        self.out.attrs.push(attrs);
        self.out.locs.push(loc);
        Ok(at)
    }

    fn fix_extra(&mut self, frame: u32, fix: Fix, word: u32) -> Result<(), InlineError> {
        let place = row(self.out.extra.len(), "extra")?;
        self.fixups.push(Fixup { place, in_data: false, frame, fix });
        self.out.extra.push(word);
        Ok(())
    }

    fn loc(&self, frame: u32, loc: u32) -> u32 {
        let Some(frame_rec) = usize::try_from(frame).ok().and_then(|i| self.frames.get(i)) else {
            return 0;
        };
        match loc {
            0 => 0,
            _ if frame_rec.drop_locs => 0,
            _ if frame_rec.root => loc,
            _ => loc + frame_rec.span_off,
        }
    }

    fn attrs(&self, frame: u32, attrs: u32) -> u32 {
        let frame = &self.frames[frame as usize];
        match attrs {
            0 => 0,
            _ if frame.root => attrs,
            _ => attrs + frame.mem_off,
        }
    }

    fn slot(&self, frame: u32, slot: u32) -> u32 {
        slot + self.frames[frame as usize].slot_off
    }

    /// The type `ty` of frame `frame`'s body, as the output names it: a compound by its
    /// members, found in O(1) or appended.
    fn ty(&mut self, frame: u32, ty: Ty) -> Result<Ty, InlineError> {
        let Some(compound) = ty.compound_row() else {
            return Ok(ty);
        };
        if self.frames[frame as usize].root {
            return Ok(ty);
        }
        let view = self.nodes[frame as usize].view;
        let rec = view.compounds[compound as usize];
        let members = &view.compound_tys[rec.first as usize..][..rec.len as usize];
        let rows = row(self.out.compounds.len(), "compounds")?;
        let hash = hash_val(members);
        let out = &mut self.out;
        let at = match intern(&mut self.compound_rows, hash, rows, |r| {
            let it = out.compounds[r as usize];
            &out.compound_tys[it.first as usize..][..it.len as usize] == members
        }) {
            Ok(at) => at,
            Err(at) => {
                let first = row(out.compound_tys.len(), "compound_tys")?;
                out.compounds.push(CompoundRec { first, len: rec.len });
                out.compound_tys.extend_from_slice(members);
                at
            }
        };
        Ty::compound(at).ok_or(too_large("compounds"))
    }

    /// A row of `param_facts` of frame `frame`'s body (0, or 1 + a row), found in O(1) or appended.
    fn facts(&mut self, frame: u32, facts: u32) -> Result<u32, InlineError> {
        if facts == 0 {
            return Ok(0);
        }
        let view = self.nodes[frame as usize].view;
        let mut rec = view.param_facts[facts as usize - 1];
        rec.range = remap_range(rec.range, self.frames[frame as usize].range_off);
        let at = match self.fact_rows.get(&rec) {
            Some(&at) => at,
            None => {
                let at = row(self.out.param_facts.len(), "param_facts")?;
                self.out.param_facts.push(rec);
                self.fact_rows.insert(rec, at);
                at
            }
        };
        Ok(at + 1)
    }

    /// A signature of frame `frame`'s body, as the output names it: found in O(1) or appended.
    fn sig(&mut self, frame: u32, sig: u32) -> Result<u32, InlineError> {
        if self.frames[frame as usize].root {
            return Ok(sig);
        }
        let cached = self.frames[frame as usize].sigs[sig as usize];
        if cached != NONE {
            return Ok(cached);
        }
        let view = self.nodes[frame as usize].view;
        let rec = view.sigs[sig as usize];
        let len = rec.params as usize + rec.rets as usize;
        let mut params: OwnVec<ParamRec, A> = OwnVec::with_capacity_in(len, self.alloc);
        for param in &view.sig_params[rec.first as usize..][..len] {
            let mut param = *param;
            param.ty = self.ty(frame, Ty(param.ty))?.0;
            param.facts = self.facts(frame, param.facts)?;
            params.push(param);
        }
        let rows = row(self.out.sigs.len(), "sigs")?;
        let hash = hash_val((rec.conv, rec.bits, rec.params, rec.rets, &params[..]));
        let out = &mut self.out;
        let at = match intern(&mut self.sig_rows, hash, rows, |r| {
            let it = out.sigs[r as usize];
            it.conv == rec.conv
                && it.bits == rec.bits
                && it.params == rec.params
                && it.rets == rec.rets
                && out.sig_params[it.first as usize..][..len] == params[..]
        }) {
            Ok(at) => at,
            Err(at) => {
                let first = row(out.sig_params.len(), "sig_params")?;
                out.sigs.push(SigRec { first, ..rec });
                out.sig_params.extend_from_slice(&params);
                at
            }
        };
        self.frames[frame as usize].sigs[sig as usize] = at;
        Ok(at)
    }

    /// An intrinsic of frame `frame`'s body, as the output names it: found in O(1) or appended.
    fn intrinsic(&mut self, frame: u32, intrinsic: u32) -> Result<u32, InlineError> {
        if self.frames[frame as usize].root {
            return Ok(intrinsic);
        }
        let cached = self.frames[frame as usize].intrinsics[intrinsic as usize];
        if cached != NONE {
            return Ok(cached);
        }
        let view = self.nodes[frame as usize].view;
        let rec = view.intrinsics[intrinsic as usize];
        let name = view.intrinsic_name(IntrinsicId(intrinsic));
        let sig = self.sig(frame, rec.sig)?;
        let rows = row(self.out.intrinsics.len(), "intrinsics")?;
        let hash = hash_val((sig, name));
        let out = &mut self.out;
        let at = match intern(&mut self.intrinsic_rows, hash, rows, |r| {
            let it = out.intrinsics[r as usize];
            it.sig == sig && &out.names[it.name_first as usize..][..it.name_len as usize] == name
        }) {
            Ok(at) => at,
            Err(at) => {
                let name_first = row(out.names.len(), "names")?;
                out.intrinsics.push(IntrinsicRec { name_first, name_len: rec.name_len, sig });
                out.names.extend_from_slice(name);
                at
            }
        };
        self.frames[frame as usize].intrinsics[intrinsic as usize] = at;
        Ok(at)
    }

    /// Resolves one reference of `frame`, pushing any nested `DataRelocRec` fixups onto
    /// `reloc_work` so `out.datas` always tiles `data_bytes`/`data_relocs`/`data_positions`
    /// in strict ascending row order (even for cyclic data relocations).
    fn resolve_ref_step(
        &mut self,
        frame: u32,
        reference: u32,
        inst: usize,
        reloc_work: &mut OwnVec<(usize, u32), A>,
    ) -> Result<u32, InlineError> {
        if self.frames[frame as usize].root {
            return Ok(reference);
        }
        let cached = self.frames[frame as usize].refs[reference as usize];
        if cached != NONE {
            return Ok(cached);
        }
        let node = self.nodes[frame as usize];
        let view = node.view;
        let rec = view.reference(RefId(reference));
        let Some(kind) = RefKind::from_word(rec.kind) else {
            return Err(InlineError::Refused { inst, what: "a reference of no known kind" });
        };

        if kind == RefKind::Data {
            let place = rec.place;
            let data = view.datas[place as usize];
            let bytes = view.data_bytes_of(DataId(place));
            if data.relocs_len == 0 && data.positions_len == 0 {
                let rows = row(self.out.refs.len(), "refs")?;
                let hash = hash_val((data.align, bytes));
                let out = &mut self.out;
                let at = match intern(&mut self.plain_data_ref_rows, hash, rows, |r| {
                    let it = out.refs[r as usize];
                    it.kind == RefKind::Data as u32
                        && out.datas.get(it.place as usize).is_some_and(|have| {
                            have.relocs_len == 0
                                && have.positions_len == 0
                                && have.align == data.align
                                && &out.data_bytes[have.bytes_first as usize..][..have.bytes_len as usize] == bytes
                        })
                }) {
                    Ok(at) => at,
                    Err(at) => {
                        let new_place = row(out.datas.len(), "datas")?;
                        let bytes_first = row(out.data_bytes.len(), "data_bytes")?;
                        let relocs_first = row(out.data_relocs.len(), "data_relocs")?;
                        let positions_first = row(out.data_positions.len(), "data_positions")?;
                        out.datas.push(DataRec {
                            bytes_first,
                            bytes_len: data.bytes_len,
                            relocs_first,
                            relocs_len: 0,
                            align: data.align,
                            positions_first,
                            positions_len: 0,
                        });
                        out.data_bytes.extend_from_slice(bytes);
                        out.refs.push(RefRec { place: new_place, kind: RefKind::Data as u32 });
                        at
                    }
                };
                self.frames[frame as usize].refs[reference as usize] = at;
                return Ok(at);
            }

            // Data with relocations and/or source positions: requires `allow_external_refs`.
            if !self.allow_external_refs {
                return Err(InlineError::Refused {
                    inst,
                    what: "a spliced body's reference to relocated or positioned data",
                });
            }
            let relocs = view.data_relocs_of(DataId(place));
            let positions = view.data_positions_of(DataId(place));
            let new_place = row(self.out.datas.len(), "datas")?;
            let bytes_first = row(self.out.data_bytes.len(), "data_bytes")?;
            let relocs_first = row(self.out.data_relocs.len(), "data_relocs")?;
            let positions_first = row(self.out.data_positions.len(), "data_positions")?;
            self.out.datas.push(DataRec {
                bytes_first,
                bytes_len: data.bytes_len,
                relocs_first,
                relocs_len: data.relocs_len,
                align: data.align,
                positions_first,
                positions_len: data.positions_len,
            });
            self.out.data_bytes.extend_from_slice(bytes);
            for reloc in relocs {
                let out_reloc_idx = self.out.data_relocs.len();
                self.out.data_relocs.push(DataRelocRec {
                    addend: reloc.addend,
                    offset: reloc.offset,
                    target: 0, // Patched when `reloc_work` drains.
                });
                reloc_work.push((out_reloc_idx, reloc.target));
            }
            self.out.data_positions.extend_from_slice(positions);
            let at = row(self.out.refs.len(), "refs")?;
            self.out.refs.push(RefRec { place: new_place, kind: RefKind::Data as u32 });
            self.frames[frame as usize].refs[reference as usize] = at;
            return Ok(at);
        }

        // External reference (`Func`, `Static`, `Tls`, `Vtable`).
        if !self.allow_external_refs {
            return Err(InlineError::Refused {
                inst,
                what: "a spliced body's reference to a function, static, thread-local, vtable or relocated data: its place is the callee's, which the caller's references cannot name",
            });
        }

        let source_ref = RefId(reference);
        let semantic = resolve_semantic_target(self.bodies, node.id, source_ref, kind);
        if let Some(target) = semantic
            && let Some(&existing) = self.target_ref_rows.get(&(rec.kind, target))
        {
            self.frames[frame as usize].refs[reference as usize] = existing;
            return Ok(existing);
        }
        if let Some(&existing) = self.source_ref_rows.get(&(node.id, source_ref)) {
            self.frames[frame as usize].refs[reference as usize] = existing;
            return Ok(existing);
        }

        // Allocate a fresh collision-free `place` in the caller's namespace for this kind.
        let k_idx = kind as usize;
        let new_place = self.next_place[k_idx];
        self.next_place[k_idx] = new_place
            .checked_add(1)
            .ok_or(too_large("refs"))?;
        let new_rec = RefRec { place: new_place, kind: rec.kind };
        let at = row(self.out.refs.len(), "refs")?;
        self.out.refs.push(new_rec);
        self.imported_refs.push(ImportedRef {
            caller_ref: RefId(at),
            source_body: node.id,
            source_ref,
            rec: new_rec,
        });
        if let Some(target) = semantic {
            self.target_ref_rows.insert((rec.kind, target), at);
        }
        self.source_ref_rows.insert((node.id, source_ref), at);
        self.frames[frame as usize].refs[reference as usize] = at;
        Ok(at)
    }

    /// A reference of frame `frame`'s body named at its instruction `inst`, as the output
    /// names it.
    fn reference(&mut self, frame: u32, reference: u32, inst: usize) -> Result<u32, InlineError> {
        if self.frames[frame as usize].root {
            return Ok(reference);
        }
        let cached = self.frames[frame as usize].refs[reference as usize];
        if cached != NONE {
            return Ok(cached);
        }
        let mut reloc_work: OwnVec<(usize, u32), A> = OwnVec::new_in(self.alloc);
        let top = self.resolve_ref_step(frame, reference, inst, &mut reloc_work)?;
        let mut cursor = 0;
        while cursor < reloc_work.len() {
            let (out_reloc_idx, callee_target) = reloc_work[cursor];
            cursor += 1;
            let mapped = self.resolve_ref_step(frame, callee_target, inst, &mut reloc_work)?;
            self.out.data_relocs[out_reloc_idx].target = mapped;
        }
        Ok(top)
    }

    /// Appends the target at `words[at..]` of frame `frame`; where its words end.
    fn target(&mut self, frame: u32, words: &[u32], mut at: usize) -> Result<usize, InlineError> {
        self.fix_extra(frame, Fix::Block, words[at])?;
        let n = words[at + 1] as usize;
        self.out.extra.push(n as u32);
        at += 2;
        for _ in 0..n {
            self.fix_extra(frame, Fix::Value, words[at])?;
            at += 1;
        }
        Ok(at)
    }

    /// Appends the operands `fields` at `words` of frame `frame`'s instruction `inst` (of
    /// result type `ty`), each as the output names it. The mirror of [`walk`].
    fn rewrite(&mut self, frame: u32, inst: usize, words: &[u32], fields: &[Field], ty: Ty) -> Result<(), InlineError> {
        let tys = self.nodes[frame as usize].view.tys;
        let mut at = 0;
        let mut scrutinee = None;
        for &field in fields {
            match field {
                Field::Value => {
                    scrutinee = Some(words[at]);
                    self.fix_extra(frame, Fix::Value, words[at])?;
                    at += 1;
                }
                Field::Block => {
                    self.fix_extra(frame, Fix::Block, words[at])?;
                    at += 1;
                }
                Field::Slot => {
                    let slot = self.slot(frame, words[at]);
                    self.out.extra.push(slot);
                    at += 1;
                }
                Field::Ref => {
                    let reference = self.reference(frame, words[at], inst)?;
                    self.out.extra.push(reference);
                    at += 1;
                }
                Field::Sig => {
                    let sig = self.sig(frame, words[at])?;
                    self.out.extra.push(sig);
                    at += 1;
                }
                Field::Intrinsic => {
                    let intrinsic = self.intrinsic(frame, words[at])?;
                    self.out.extra.push(intrinsic);
                    at += 1;
                }
                Field::Asm => {
                    if !self.frames[frame as usize].root {
                        return Err(InlineError::Refused { inst, what: "an `asm!` block in a spliced body" });
                    }
                    self.out.extra.push(words[at]);
                    at += 1;
                }
                Field::Imm | Field::Offset | Field::IntCond | Field::FloatCond | Field::Rmw => {
                    self.out.extra.push(words[at]);
                    at += 1;
                }
                Field::Target => at = self.target(frame, words, at)?,
                Field::Values => {
                    let n = words[at] as usize;
                    self.out.extra.push(n as u32);
                    at += 1;
                    for _ in 0..n {
                        self.fix_extra(frame, Fix::Value, words[at])?;
                        at += 1;
                    }
                }
                Field::Blocks => {
                    let n = words[at] as usize;
                    self.out.extra.push(n as u32);
                    at += 1;
                    for _ in 0..n {
                        self.fix_extra(frame, Fix::Block, words[at])?;
                        at += 1;
                    }
                }
                Field::Mask => {
                    let n = words[at] as usize;
                    self.out.extra.extend_from_slice(&words[at..at + 1 + n]);
                    at += 1 + n;
                }
                Field::Const => {
                    let n = ty.const_words() as usize;
                    self.out.extra.extend_from_slice(&words[at..at + n]);
                    at += n;
                }
                Field::Cases => {
                    let width = scrutinee.map_or(0, |it: u32| Ty(tys[it as usize]).const_words()) as usize;
                    let n = words[at] as usize;
                    self.out.extra.push(n as u32);
                    at += 1;
                    for _ in 0..n {
                        self.out.extra.extend_from_slice(&words[at..at + width]);
                        at += width;
                        at = self.target(frame, words, at)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Copies frame `frame`'s instruction `at`, its operands as the output names them.
    fn copy(&mut self, frame: u32, at: usize) -> Result<(), InlineError> {
        let view = self.nodes[frame as usize].view;
        let op = view.op(at);
        let original = Ty(view.tys[at]);
        let ty = self.ty(frame, original)?;
        let new = row(self.out.ops.len(), "instructions")?;
        let data = match op.info().layout {
            Layout::None => 0,
            Layout::Data(Field::Value) => {
                self.fixups.push(Fixup { place: new, in_data: true, frame, fix: Fix::Value });
                view.data[at]
            }
            Layout::Data(Field::Slot) => self.slot(frame, view.data[at]),
            Layout::Data(Field::Ref) => self.reference(frame, view.data[at], at)?,
            Layout::Data(_) => view.data[at],
            Layout::Extra(fields) => {
                let start = row(self.out.extra.len(), "extra")?;
                let words: &[u32] = &view.extra[view.data[at] as usize..];
                self.rewrite(frame, at, words, fields, original)?;
                start
            }
        };
        let (attrs, loc) = (self.attrs(frame, view.attrs[at]), self.loc(frame, view.locs[at]));
        let made = self.raw(op, view.flags[at], ty, data, attrs, loc)?;
        self.frames[frame as usize].values[at] = made;
        Ok(())
    }

    fn is_cleanup(&self, frame: u32, block: u32) -> bool {
        let Ok(f_idx) = usize::try_from(frame) else { return false };
        let Ok(b_idx) = usize::try_from(block) else { return false };
        self.frame_cleanup
            .get(f_idx)
            .and_then(|v| v.get(b_idx).copied())
            .unwrap_or(false)
    }

    /// Rewrites an un-spliced `Op::Call` or `Op::CallIndirect` at `at` in a non-cleanup block of
    /// `frame` into `Op::Invoke` / `Op::InvokeIndirect` targeting `ut` (enclosing caller unwind
    /// pad), splitting the current block immediately into a fresh normal/cold continuation block.
    fn copy_as_invoke(
        &mut self,
        frame: u32,
        block: u32,
        at: usize,
        ut: UnwindTarget,
    ) -> Result<(), InlineError> {
        let Ok(f_idx) = usize::try_from(frame) else {
            return Err(InlineError::Refused { inst: at, what: "a frame index past usize" });
        };
        let Some(view) = self.nodes.get(f_idx).map(|n| n.view) else {
            return Err(InlineError::Refused { inst: at, what: "a frame past the plan's nodes" });
        };
        let op = view.op(at);
        let original = Ty(view.tys.get(at).copied().unwrap_or(0));
        let ty = self.ty(frame, original)?;
        let Some(extra_start) = view.data.get(at).and_then(|&d| usize::try_from(d).ok()) else {
            return Err(InlineError::Refused { inst: at, what: "call operands past extra table" });
        };
        let Some(words) = view.extra.get(extra_start..) else {
            return Err(InlineError::Refused { inst: at, what: "call operands past extra table" });
        };
        let Some(&callee_word) = words.first() else {
            return Err(InlineError::Refused { inst: at, what: "call missing callee operand" });
        };
        let Some(&sig_word) = words.get(1) else {
            return Err(InlineError::Refused { inst: at, what: "call missing signature operand" });
        };
        let Some(&argc_word) = words.get(2) else {
            return Err(InlineError::Refused { inst: at, what: "call missing argument count" });
        };
        let Ok(argc) = usize::try_from(argc_word) else {
            return Err(InlineError::Refused { inst: at, what: "call argument count past usize" });
        };
        let Some(arg_end) = 3usize.checked_add(argc) else {
            return Err(InlineError::Refused { inst: at, what: "call arguments overflow" });
        };
        let Some(args) = words.get(3..arg_end) else {
            return Err(InlineError::Refused { inst: at, what: "call arguments past extra table" });
        };

        let start = row(self.out.extra.len(), "extra")?;
        let invoke_op = match op {
            Op::Call => {
                let reference = self.reference(frame, callee_word, at)?;
                self.out.extra.push(reference);
                Op::Invoke
            }
            Op::CallIndirect => {
                self.fix_extra(frame, Fix::Value, callee_word)?;
                Op::InvokeIndirect
            }
            _ => {
                return Err(InlineError::Refused { inst: at, what: "non-call rewritten as invoke" });
            }
        };
        let sig = self.sig(frame, sig_word)?;
        self.out.extra.push(sig);
        let split_blk = row(self.out.block_starts.len(), "blocks")?;
        self.out.extra.push(split_blk);
        self.fix_extra(ut.frame, Fix::Block, ut.block)?;
        self.out.extra.push(argc_word);
        for &arg in args {
            self.fix_extra(frame, Fix::Value, arg)?;
        }

        let attrs = self.attrs(frame, view.attrs.get(at).copied().unwrap_or(0));
        let loc = self.loc(frame, view.locs.get(at).copied().unwrap_or(0));
        let flags = view.flags.get(at).copied().unwrap_or(0);
        let made = self.raw(invoke_op, flags, ty, start, attrs, loc)?;
        if let Some(slot) = self.frames.get_mut(f_idx).and_then(|f| f.values.get_mut(at)) {
            *slot = made;
        }
        let cont_kind = match view.block_kind(Block(block)) {
            BlockKind::Normal => BlockKind::Normal,
            BlockKind::Cold | BlockKind::Cleanup | BlockKind::Catch | BlockKind::Terminate => BlockKind::Cold,
        };
        let actual_split = self.begin(cont_kind)?;
        if actual_split != split_blk {
            return Err(InlineError::Refused { inst: at, what: "invoke split block id mismatch" });
        }
        Ok(())
    }

    /// Rewrites an inlined frame's `Op::Resume(exc)` into `Op::Jump(resume_body(ut), [exc])`
    /// so unwinding transfers directly into the enclosing caller's split landing-pad body.
    fn resume_to_unwind(
        &mut self,
        frame: u32,
        at: usize,
        ut: UnwindTarget,
    ) -> Result<(), InlineError> {
        let Ok(f_idx) = usize::try_from(frame) else {
            return Err(InlineError::Refused { inst: at, what: "a frame index past usize" });
        };
        let Some(view) = self.nodes.get(f_idx).map(|n| n.view) else {
            return Err(InlineError::Refused { inst: at, what: "a frame past the plan's nodes" });
        };
        let Some(&exc) = view.data.get(at) else {
            return Err(InlineError::Refused { inst: at, what: "resume missing exception operand" });
        };
        let start = row(self.out.extra.len(), "extra")?;
        self.fix_extra(ut.frame, Fix::ResumeBody, ut.block)?;
        self.out.extra.push(1);
        self.fix_extra(frame, Fix::Value, exc)?;
        let loc = self.loc(frame, view.locs.get(at).copied().unwrap_or(0));
        self.raw(Op::Jump, 0, Ty::NONE, start, 0, loc)?;
        Ok(())
    }

    /// Begins the output's copy of block `block` of frame `frame`'s body with its parameters;
    /// where its other instructions start. When `(frame, block)` is a landing pad targeted by an
    /// inlined callee's `Op::Resume`, splits it into a 1-instruction landing trampoline
    /// (`landing_param -> Jump(body_block, [landing_param])`) followed by the `BlockKind::Cold`
    /// body block whose parameter receives both unwind entries and inlined `Resume` jumps.
    fn body_block(&mut self, frame: u32, block: u32) -> Result<usize, InlineError> {
        let Ok(f_idx) = usize::try_from(frame) else {
            return Err(InlineError::Refused { inst: 0, what: "a frame index past usize" });
        };
        let Ok(b_idx) = usize::try_from(block) else {
            return Err(InlineError::Refused { inst: 0, what: "a block index past usize" });
        };
        let Some(view) = self.nodes.get(f_idx).map(|n| n.view) else {
            return Err(InlineError::Refused { inst: 0, what: "a frame past the plan's nodes" });
        };
        let kind = view.block_kind(Block(block));
        let params = view.params(Block(block));
        if self.needs_resume_body.contains_key(&(frame, block)) {
            if !kind.is_landing() || params.len() != 1 {
                return Err(InlineError::Refused {
                    inst: params.start,
                    what: "an unwind target block that is not a single-param landing block",
                });
            }
            let p0 = params.start;
            let landing_num = self.begin(kind)?;
            if let Some(slot) = self.frames.get_mut(f_idx).and_then(|f| f.blocks.get_mut(b_idx)) {
                *slot = landing_num;
            }
            let p_ty = self.ty(frame, Ty(view.tys.get(p0).copied().unwrap_or(0)))?;
            let p_flags = view.flags.get(p0).copied().unwrap_or(0);
            let p_attrs = self.attrs(frame, view.attrs.get(p0).copied().unwrap_or(0));
            let p_loc = self.loc(frame, view.locs.get(p0).copied().unwrap_or(0));
            let lp_param = self.raw(Op::Param, p_flags, p_ty, 0, p_attrs, p_loc)?;
            let body_num = row(self.out.block_starts.len(), "blocks")?;
            let jump_start = row(self.out.extra.len(), "extra")?;
            self.out.extra.push(body_num);
            self.out.extra.push(1);
            self.out.extra.push(lp_param);
            self.raw(Op::Jump, 0, Ty::NONE, jump_start, 0, p_loc)?;

            let actual_body = self.begin(BlockKind::Cold)?;
            if actual_body != body_num {
                return Err(InlineError::Refused { inst: p0, what: "landing split block id mismatch" });
            }
            if let Some(slot) = self.frames.get_mut(f_idx).and_then(|f| f.resume_bodies.get_mut(b_idx)) {
                *slot = actual_body;
            }
            let body_param = self.raw(Op::Param, p_flags, p_ty, 0, p_attrs, p_loc)?;
            if let Some(slot) = self.frames.get_mut(f_idx).and_then(|f| f.values.get_mut(p0)) {
                *slot = body_param;
            }
            return Ok(params.end);
        }
        let number = self.begin(kind)?;
        if let Some(slot) = self.frames.get_mut(f_idx).and_then(|f| f.blocks.get_mut(b_idx)) {
            *slot = number;
        }
        for at in params.start..params.end {
            let ty = self.ty(frame, Ty(view.tys.get(at).copied().unwrap_or(0)))?;
            let flags = view.flags.get(at).copied().unwrap_or(0);
            let attrs = self.attrs(frame, view.attrs.get(at).copied().unwrap_or(0));
            let loc = self.loc(frame, view.locs.get(at).copied().unwrap_or(0));
            let made = self.raw(Op::Param, flags, ty, 0, attrs, loc)?;
            if let Some(slot) = self.frames.get_mut(f_idx).and_then(|f| f.values.get_mut(at)) {
                *slot = made;
            }
        }
        Ok(params.end)
    }

    /// Frame `child`, its body's tables appended to the output's.
    fn make_frame(&mut self, child: u32) -> Result<(), InlineError> {
        let node = self.nodes[child as usize];
        let view = node.view;
        let parent = self.nodes[node.parent as usize].view;
        let call_loc = self.loc(node.parent, parent.locs[node.site]);
        let out = &mut self.out;
        let slot_off = row(out.slots.len(), "slots")?;
        out.slots.extend_from_slice(view.slots);
        let range_off = row(out.ranges.len(), "ranges")?;
        out.ranges.extend_from_slice(view.ranges);
        let mem_off = row(out.mem_attrs.len(), "mem_attrs")?;
        for rec in view.mem_attrs {
            let mut rec = *rec;
            rec.range = remap_range(rec.range, range_off);
            out.mem_attrs.push(rec);
        }
        let scope_off = row(out.scope_parents.len(), "scope_parents")?;
        for &up in view.scope_parents {
            out.scope_parents.push(if up == NONE { NONE } else { up + scope_off });
        }
        for var in view.variables {
            let mut var = *var;
            var.slot += slot_off;
            if var.scope != NONE {
                var.scope += scope_off;
            }
            out.variables.push(var);
        }
        let mut span_off = 0;
        let mut drop_locs = false;
        if !view.spans.is_empty() {
            if let Some(call_row) = usize::try_from(call_loc).ok().and_then(|v| v.checked_sub(1))
                && let Some(&call_site) = out.spans.get(call_row)
            {
                if out.span_inlined.len() < out.spans.len() {
                    out.span_inlined.resize(out.spans.len(), NONE);
                }
                let up = out.span_inlined.get(call_row).copied().unwrap_or(NONE);
                let instance = row(out.inlined.len(), "inlined")?;
                out.inlined.push(InlinedRec { parent: up, call_site });
                let inlined_off = row(out.inlined.len(), "inlined")?;
                for rec in view.inlined {
                    let parent = if rec.parent == NONE { instance } else { rec.parent + inlined_off };
                    out.inlined.push(InlinedRec { parent, call_site: rec.call_site });
                }
                span_off = row(out.spans.len(), "spans")?;
                for (r, &span) in view.spans.iter().enumerate() {
                    out.spans.push(span);
                    let inlined = match view.span_inlined.get(r) {
                        Some(&it) if it != NONE => it + inlined_off,
                        _ => instance,
                    };
                    out.span_inlined.push(inlined);
                    if self.scoped {
                        let scope = match view.span_scopes.get(r) {
                            Some(&it) if it >= SCOPE_IN => it + scope_off,
                            Some(&it) => it,
                            None => SCOPE_OUTSIDE,
                        };
                        out.span_scopes.push(scope);
                    }
                }
            } else {
                drop_locs = true;
            }
        }
        if usize::try_from(child).ok() != Some(self.frames.len()) {
            return Err(InlineError::Refused {
                inst: node.site,
                what: "a site the emission reached out of the plan's order",
            });
        }
        let alloc = &self.alloc;
        let needs_rb = usize::try_from(child)
            .ok()
            .and_then(|ci| self.frame_needs_resume_body.get(ci).copied())
            .unwrap_or(false);
        let resume_bodies = if needs_rb {
            own_filled(view.blocks(), NONE, alloc)
        } else {
            OwnVec::new_in(*alloc)
        };
        self.frames.push(Frame {
            root: false,
            values: own_filled(view.len(), NONE, alloc),
            blocks: own_filled(view.blocks(), NONE, alloc),
            resume_bodies,
            cont: NONE,
            cont_params: [NONE; MAX_MULTI_RET],
            drop_locs,
            slot_off,
            span_off,
            range_off,
            mem_off,
            refs: own_filled(view.refs.len(), NONE, alloc),
            sigs: own_filled(view.sigs.len(), NONE, alloc),
            intrinsics: own_filled(view.intrinsics.len(), NONE, alloc),
        });
        Ok(())
    }

    /// If `Op::Project` at `at` of `frame` projects a return value of a spliced multi-return
    /// call or invoke in `frame`, returns `(child_frame, ret_idx)`.
    fn spliced_project(&self, frame: u32, at: usize) -> Option<(u32, u8)> {
        let frame_idx = usize::try_from(frame).ok()?;
        let view = self.nodes.get(frame_idx)?.view;
        let start = usize::try_from(*view.data.get(at)?).ok()?;
        let words = view.extra.get(start..)?;
        let src_inst = usize::try_from(*words.first()?).ok()?;
        let &child = self.children.get(&(frame, src_inst))?;
        let child_idx = usize::try_from(child).ok()?;
        let callee = self.nodes.get(child_idx)?.view;
        let rets = callee.sig(SigId(callee.head.sig)).rets;
        if rets.len() < 2 {
            return None;
        }
        let ret_idx_usize = usize::try_from(*words.get(1)?).ok()?;
        if ret_idx_usize >= rets.len() {
            return None;
        }
        let ret_idx = u8::try_from(ret_idx_usize).ok()?;
        Some((child, ret_idx))
    }

    /// The site `at` of frame `frame`, spliced as node `child`: a jump to the copy of the
    /// callee's entry, which is begun; where its other instructions start.
    fn enter(&mut self, frame: u32, at: usize, child: u32) -> Result<usize, InlineError> {
        self.make_frame(child)?;
        let Some(view) = usize::try_from(frame).ok().and_then(|i| self.nodes.get(i)).map(|n| n.view) else {
            return Err(InlineError::Refused { inst: at, what: "a frame past the plan's nodes" });
        };
        let Some(cs) = call_site(view, at) else {
            return Err(InlineError::Refused { inst: at, what: "a spliced site that is not a call or invoke" });
        };
        let args = cs.args;
        let count = args.len();
        let start = row(self.out.extra.len(), "extra")?;
        self.fix_extra(child, Fix::Block, 0)?;
        self.out.extra.push(row(count, "extra")?);
        // Planted (test builds only): the first spliced call's first two arguments swapped.
        let swap = cfg!(test) && self.swap && child == 1 && count >= 2;
        for k in 0..count {
            let idx = match swap && k < 2 {
                true => 1 - k,
                false => k,
            };
            let Some(&arg) = args.get(idx) else {
                return Err(InlineError::Refused { inst: at, what: "call arguments past their words" });
            };
            self.fix_extra(frame, Fix::Value, arg)?;
        }
        let loc = self.loc(frame, view.locs.get(at).copied().unwrap_or(0));
        self.raw(Op::Jump, 0, Ty::NONE, start, 0, loc)?;
        self.body_block(child, 0)
    }

    /// Frame `frame`'s `return` at `at`: a jump to its continuation.
    fn ret(&mut self, frame: u32, at: usize) -> Result<(), InlineError> {
        let view = self.nodes[frame as usize].view;
        let words = &view.extra[view.data[at] as usize..];
        let count = words[0] as usize;
        let start = row(self.out.extra.len(), "extra")?;
        self.fix_extra(frame, Fix::Cont, 0)?;
        self.out.extra.push(count as u32);
        for &value in &words[1..1 + count] {
            self.fix_extra(frame, Fix::Value, value)?;
        }
        let loc = self.loc(frame, view.locs[at]);
        self.raw(Op::Jump, 0, Ty::NONE, start, 0, loc)?;
        Ok(())
    }

    /// The continuation of node `child`, in its parent at `cursor`: a block whose parameter(s) are
    /// the call's return value(s), where the rest of the caller's block (or an `Op::Invoke`'s
    /// normal target) goes.
    fn after(&mut self, child: u32, cursor: Cursor) -> Result<(), InlineError> {
        let Ok(child_idx) = usize::try_from(child) else {
            return Err(InlineError::Refused { inst: cursor.at, what: "a child frame index past usize" });
        };
        let Some(&node) = self.nodes.get(child_idx) else {
            return Err(InlineError::Refused { inst: cursor.at, what: "a child frame past the plan's nodes" });
        };
        let parent = cursor.frame;
        let Ok(parent_idx) = usize::try_from(parent) else {
            return Err(InlineError::Refused { inst: node.site, what: "a parent frame index past usize" });
        };
        let Some(view) = self.nodes.get(parent_idx).map(|n| n.view) else {
            return Err(InlineError::Refused { inst: node.site, what: "a parent frame past the plan's nodes" });
        };
        let kind = match view.block_kind(Block(cursor.block)) {
            BlockKind::Normal => BlockKind::Normal,
            BlockKind::Cold | BlockKind::Cleanup | BlockKind::Catch | BlockKind::Terminate => BlockKind::Cold,
        };
        let number = self.begin(kind)?;
        if let Some(cf) = self.frames.get_mut(child_idx) {
            cf.cont = number;
        }
        let loc = self.loc(parent, view.locs.get(node.site).copied().unwrap_or(0));
        let callee_rets = node.view.sig(SigId(node.view.head.sig)).rets;
        match callee_rets {
            [] => {}
            [_one] => {
                let site_ty = view.tys.get(node.site).copied().map_or(Ty::NONE, Ty);
                let ty = self.ty(parent, site_ty)?;
                if !ty.is_none() {
                    let made = self.raw(Op::Param, 0, ty, 0, 0, loc)?;
                    if let Some(slot) = self.frames.get_mut(parent_idx).and_then(|pf| pf.values.get_mut(node.site)) {
                        *slot = made;
                    }
                }
            }
            many => {
                let mut cont_params = [NONE; MAX_MULTI_RET];
                for (k, ret_rec) in many.iter().enumerate() {
                    let ret_ty = self.ty(child, Ty(ret_rec.ty))?;
                    let made = self.raw(Op::Param, 0, ret_ty, 0, 0, loc)?;
                    if let Some(slot) = cont_params.get_mut(k) {
                        *slot = made;
                    }
                }
                if let Some(cf) = self.frames.get_mut(child_idx) {
                    cf.cont_params = cont_params;
                }
            }
        }
        if node.is_invoke
            && let Some(cs) = call_site(view, node.site)
            && let Some(normal) = cs.invoke_normal
        {
            let start = row(self.out.extra.len(), "extra")?;
            self.fix_extra(parent, Fix::Block, normal)?;
            self.out.extra.push(0);
            self.raw(Op::Jump, 0, Ty::NONE, start, 0, loc)?;
        }
        Ok(())
    }
}

/// The output: the plan's tree, spliced, plus any external references imported into `out.refs`.
fn emit<A: Allocator + Copy>(
    nodes: &[Node<'_>],
    children: &Map<(u32, usize), u32, FixedState, A>,
    bodies: &dyn Bodies,
    allow_external_refs: bool,
    swap: bool,
    alloc: &A,
) -> Result<(Func<A>, Vec<ImportedRef, A>), InlineError> {
    let view = nodes[0].view;
    // No-op fast path: nothing spliced -> verify and copy directly.
    if nodes.len() == 1 {
        crate::verify::verify_in(view, alloc).map_err(InlineError::Verify)?;
        return Ok((view.to_func_in(*alloc), Vec::new_in(*alloc)));
    }
    let mut out = view.to_func_in(*alloc);
    out.ops.clear();
    out.flags.clear();
    out.tys.clear();
    out.data.clear();
    out.extra.clear();
    out.attrs.clear();
    out.locs.clear();
    out.block_starts.clear();
    out.block_kinds.clear();
    // Precompute per-frame cleanup-path masks, inherited unwind landing pads, and which landing
    // pads require a split `BlockKind::Cold` body block for inlined `Op::Resume` jumps.
    let mut frame_cleanup: OwnVec<OwnVec<bool, A>, A> = OwnVec::with_capacity_in(nodes.len(), *alloc);
    for node in nodes {
        frame_cleanup.push(cleanup_blocks(node.view, alloc)?);
    }
    let mut frame_unwind: OwnVec<Option<UnwindTarget>, A> = OwnVec::with_capacity_in(nodes.len(), *alloc);
    frame_unwind.push(None);
    for node in nodes.iter().skip(1) {
        let p = node.parent;
        let p_idx = usize::try_from(p).unwrap_or(0);
        let ut = if node.is_invoke {
            let parent_view = nodes.get(p_idx).map(|n| n.view);
            parent_view
                .and_then(|pv| call_site(pv, node.site))
                .and_then(|cs| cs.invoke_unwind)
                .map(|blk| UnwindTarget { frame: p, block: blk })
        } else {
            let in_clean = nodes
                .get(p_idx)
                .and_then(|pn| block_of_inst(pn.view, node.site))
                .and_then(|b| usize::try_from(b).ok())
                .and_then(|bi| frame_cleanup.get(p_idx).and_then(|c| c.get(bi).copied()))
                .unwrap_or(false);
            if in_clean {
                None
            } else {
                frame_unwind.get(p_idx).copied().flatten()
            }
        };
        frame_unwind.push(ut);
    }
    let mut needs_resume_body: Map<(u32, u32), (), FixedState, A> = new_map(alloc);
    let mut frame_needs_resume_body = own_filled(nodes.len(), false, alloc);
    for (f_idx, node) in nodes.iter().enumerate().skip(1) {
        if let Some(Some(ut)) = frame_unwind.get(f_idx).copied()
            && (0..node.view.len()).any(|i| node.view.op(i) == Op::Resume)
        {
            needs_resume_body.insert((ut.frame, ut.block), ());
            if let Ok(uf_idx) = usize::try_from(ut.frame)
                && let Some(slot) = frame_needs_resume_body.get_mut(uf_idx)
            {
                *slot = true;
            }
        }
    }
    let root_resume_bodies = if frame_needs_resume_body.first().copied().unwrap_or(false) {
        own_filled(view.blocks(), NONE, alloc)
    } else {
        OwnVec::new_in(*alloc)
    };

    let mut e = Emit {
        nodes,
        children,
        bodies,
        allow_external_refs,
        out,
        frames: Vec::with_capacity_in(nodes.len(), *alloc),
        frame_cleanup,
        frame_unwind,
        needs_resume_body,
        frame_needs_resume_body,
        fixups: Vec::new_in(*alloc),
        project_aliases: OwnVec::new_in(*alloc),
        compound_rows: new_map(alloc),
        fact_rows: new_map(alloc),
        sig_rows: new_map(alloc),
        intrinsic_rows: new_map(alloc),
        plain_data_ref_rows: new_map(alloc),
        target_ref_rows: new_map(alloc),
        source_ref_rows: new_map(alloc),
        next_place: [0; 5],
        imported_refs: Vec::new_in(*alloc),
        scoped: !view.span_scopes.is_empty(),
        swap,
        alloc: *alloc,
    };
    e.seed_interners();
    e.frames.push(Frame {
        root: true,
        values: own_filled(view.len(), NONE, alloc),
        blocks: own_filled(view.blocks(), NONE, alloc),
        resume_bodies: root_resume_bodies,
        cont: NONE,
        cont_params: [NONE; MAX_MULTI_RET],
        drop_locs: false,
        slot_off: 0,
        span_off: 0,
        range_off: 0,
        mem_off: 0,
        refs: OwnVec::new_in(*alloc),
        sigs: OwnVec::new_in(*alloc),
        intrinsics: OwnVec::new_in(*alloc),
    });
    let mut stack: OwnVec<Cursor, A> = OwnVec::new_in(*alloc);
    let at = e.body_block(0, 0)?;
    stack.push(Cursor { frame: 0, block: 0, at });
    while let Some(&cursor) = stack.last() {
        let body = nodes[cursor.frame as usize].view;
        if cursor.at == body.block(Block(cursor.block)).end {
            let next = cursor.block + 1;
            if (next as usize) < body.blocks() {
                let at = e.body_block(cursor.frame, next)?;
                stack.pop();
                stack.push(Cursor { frame: cursor.frame, block: next, at });
                continue;
            }
            stack.pop();
            if let Some(&parent) = stack.last() {
                e.after(cursor.frame, parent)?;
            }
            continue;
        }
        if let Some(top) = stack.last_mut() {
            top.at += 1;
        }
        let at = cursor.at;
        let op = body.op(at);
        if matches!(op, Op::Call | Op::Invoke)
            && let Some(&child) = e.children.get(&(cursor.frame, at))
        {
            let at = e.enter(cursor.frame, at, child)?;
            stack.push(Cursor { frame: child, block: 0, at });
            continue;
        }
        if op == Op::Project
            && let Some((child, ret_idx)) = e.spliced_project(cursor.frame, at)
        {
            e.project_aliases.push((cursor.frame, at, child, ret_idx));
            continue;
        }
        if op == Op::Return && cursor.frame != 0 {
            e.ret(cursor.frame, at)?;
        } else if op == Op::Resume
            && let Some(ut) = usize::try_from(cursor.frame)
                .ok()
                .and_then(|fi| e.frame_unwind.get(fi).copied().flatten())
        {
            e.resume_to_unwind(cursor.frame, at, ut)?;
        } else if matches!(op, Op::Call | Op::CallIndirect)
            && let Some(ut) = usize::try_from(cursor.frame)
                .ok()
                .and_then(|fi| e.frame_unwind.get(fi).copied().flatten())
            && !e.is_cleanup(cursor.frame, cursor.block)
            && !call_is_variadic(body, at)
        {
            e.copy_as_invoke(cursor.frame, cursor.block, at, ut)?;
        } else {
            e.copy(cursor.frame, at)?;
        }
    }
    // Resolve deferred multi-return `Op::Project` aliases to continuation block parameters.
    let Emit { mut out, mut frames, fixups, project_aliases, imported_refs, .. } = e;
    for &(frame, proj_inst, child, ret_idx) in project_aliases.iter() {
        let Ok(child_idx) = usize::try_from(child) else {
            return Err(InlineError::Refused {
                inst: proj_inst,
                what: "a multi-return projection whose child frame index overflows usize",
            });
        };
        let Some(param) = frames
            .get(child_idx)
            .and_then(|cf| cf.cont_params.get(usize::from(ret_idx)).copied())
            .filter(|&p| p != NONE)
        else {
            return Err(InlineError::Refused {
                inst: proj_inst,
                what: "a multi-return projection past its callee's continuation parameters",
            });
        };
        let Ok(frame_idx) = usize::try_from(frame) else {
            return Err(InlineError::Refused {
                inst: proj_inst,
                what: "a multi-return projection whose frame index overflows usize",
            });
        };
        if let Some(slot) = frames.get_mut(frame_idx).and_then(|f| f.values.get_mut(proj_inst)) {
            *slot = param;
        }
    }
    // Every value and block named in a copied operand becomes its number here.
    for fixup in fixups.iter() {
        let word = match fixup.in_data {
            true => &mut out.data[fixup.place as usize],
            false => &mut out.extra[fixup.place as usize],
        };
        let frame = &frames[fixup.frame as usize];
        let to = match fixup.fix {
            Fix::Value => frame.values.get(*word as usize).copied(),
            Fix::Block => frame.blocks.get(*word as usize).copied(),
            Fix::Cont => Some(frame.cont),
            Fix::ResumeBody => frame.resume_bodies.get(*word as usize).copied(),
        };
        match to {
            Some(to) if to != NONE => *word = to,
            _ => {
                return Err(InlineError::Refused {
                    inst: fixup.place as usize,
                    what: "an operand naming a value or block of a body that the output does not make",
                });
            }
        }
    }
    row(out.extra.len(), "extra")?;
    crate::verify::verify_in(out.view(), out.allocator()).map_err(InlineError::Verify)?;
    Ok((out, imported_refs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Flags, IntCond,
        builder::{Builder, Facts, Full, MemAttrs, Param, Variadic},
        eval::{Outcome, Val, run_calls_in, run_in},
        func::{Conv, Ext, FnAttrs, SlotKind},
    };

    const TEST_FUEL: u64 = 100_000;

    fn body_id(tag: u8) -> BodyId {
        BodyId([tag; 32])
    }

    struct LocalBank<A: Allocator> {
        bodies: OwnVec<(BodyId, Func<A>), A>,
        links: OwnVec<(BodyId, RefId, BodyId), A>,
    }

    impl<A: Allocator> Bodies for LocalBank<A> {
        fn callee(&self, from: BodyId, reference: RefId) -> Option<(BodyId, View<'_>)> {
            let &(_, _, to) = self.links.iter().find(|link| link.0 == from && link.1 == reference)?;
            let (_, func) = self.bodies.iter().find(|body| body.0 == to)?;
            Some((to, func.view()))
        }
    }

    #[test]
    fn inlines_multi_return_wide_ptr_wrapper() {
        let heap = blaze_own::testing::heap();
        let alloc = heap.alloc();

        // Callee: `wide_wrap(p: ptr [NONNULL], len: i64) -> (ptr, i64)`
        // returns `(ptr_add(p, 8), sub(len, 1))`, mirroring `slice::iter` / `IntoIterator::into_iter`.
        let wide_wrap = {
            let mut b = Builder::<_, Full>::new_in(64, alloc);
            let p_param = Param {
                facts: Facts { bits: ParamFactsRec::NONNULL, ..Param::of(Ty::PTR).facts },
                ..Param::of(Ty::PTR)
            };
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[p_param, Param::of(Ty::I64)],
                &[Param::of(Ty::PTR), Param::of(Ty::I64)],
            );
            b.set_signature(sig);
            b.set_attrs(FnAttrs::default().with_inline(Inline::Hint));
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let p = b.param(Ty::PTR);
            let len = b.param(Ty::I64);
            let eight = b.iconst(Ty::I64, 8);
            let one = b.iconst(Ty::I64, 1);
            let next_p = b.ptr_add(Flags::INBOUNDS, p, eight);
            let next_len = b.binary(Op::Sub, Flags::NONE, len, one);
            b.ret(&[next_p, next_len]);
            b.finish_checked().expect("wide_wrap builds")
        };

        // Caller: `caller(x: i64, y: i64, len: i64) -> i64`
        // spills `(x, y)` to a 16B slot, calls `wide_wrap(slot_addr, len)`, projects `(p2, len2)`,
        // loads `*p2` (which is `y`) and returns `y + len2`.
        let caller = {
            let mut b = Builder::<_, Full>::new_in(64, alloc);
            let own_sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I64), Param::of(Ty::I64), Param::of(Ty::I64)],
                &[Param::of(Ty::I64)],
            );
            b.set_signature(own_sig);
            let call_sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::PTR), Param::of(Ty::I64)],
                &[Param::of(Ty::PTR), Param::of(Ty::I64)],
            );
            let callee_ref = b.reference(RefKind::Func, 0);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let x = b.param(Ty::I64);
            let y = b.param(Ty::I64);
            let len = b.param(Ty::I64);
            let buf = b.slot(16, 3, SlotKind::Temp);
            let base = b.slot_addr(buf);
            b.store(Flags::NONE, x, base, 0, MemAttrs::aligned(3));
            b.store(Flags::NONE, y, base, 8, MemAttrs::aligned(3));
            let pair = b.call(Flags::NONE, callee_ref, call_sig, &[base, len]);
            let p2 = b.project(pair, 0);
            let len2 = b.project(pair, 1);
            let loaded_y = b.load(Ty::I64, Flags::NONE, p2, 0, MemAttrs::aligned(3));
            let sum = b.binary(Op::Add, Flags::NONE, loaded_y, len2);
            b.ret(&[sum]);
            b.finish_checked().expect("caller builds")
        };

        let mut bodies = OwnVec::new_in(alloc);
        bodies.push((body_id(1), caller));
        bodies.push((body_id(2), wide_wrap));
        let mut links = OwnVec::new_in(alloc);
        links.push((body_id(1), RefId(0), body_id(2)));
        let bank = LocalBank { bodies, links };

        let root_view = bank.bodies[0].1.view();
        let guided = inline_guided(
            root_view,
            body_id(1),
            &bank,
            &NoWeights,
            InlinePolicy::default(),
            alloc,
        )
        .expect("multi-return inline_guided succeeds");

        let out_view = guided.func.view();
        assert_eq!(guided.read[..], [body_id(2)]);
        assert_eq!((0..out_view.len()).filter(|&i| out_view.op(i) == Op::Call).count(), 0);
        assert_eq!((0..out_view.len()).filter(|&i| out_view.op(i) == Op::Project).count(), 0);
        verify(out_view).expect("inlined multi-return func verifies");

        for (x, y, len) in [(10u128, 42u128, 5u128), (0, 99, 1), (7, 1000, 123)] {
            let args = [Val::Int(x), Val::Int(y), Val::Int(len)];
            let before = run_calls_in(root_view, body_id(1), &args, TEST_FUEL, &bank, alloc)
                .expect("original runs");
            let after = run_in(out_view, &args, TEST_FUEL, alloc).expect("inlined runs");
            assert_eq!(after, before);
            assert_eq!(after, Outcome::<_>::Returned({
                let mut v = Vec::new_in(alloc);
                v.push(Val::Int(y + (len - 1)));
                v
            }));
        }
    }

    #[test]
    fn inlines_nested_partial_ord_lt_through_min() {
        let heap = blaze_own::testing::heap();
        let alloc = heap.alloc();

        // Leaf callee: `lt(&usize, &usize) -> bool (i8 zext)`
        let lt_fn = {
            let mut b = Builder::<_, Full>::new_in(64, alloc);
            let ref_param = Param {
                facts: Facts {
                    bits: ParamFactsRec::NONNULL | ParamFactsRec::READONLY,
                    ..Param::of(Ty::PTR).facts
                },
                ..Param::of(Ty::PTR)
            };
            let bool_ret = Param { ext: Ext::Zext, ..Param::of(Ty::I8) };
            let sig = b.sig(Conv::C, Variadic::No, &[ref_param, ref_param], &[bool_ret]);
            b.set_signature(sig);
            b.set_attrs(FnAttrs::default().with_inline(Inline::Always));
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let a = b.param(Ty::PTR);
            let rhs = b.param(Ty::PTR);
            let va = b.load(Ty::I64, Flags::NONE, a, 0, MemAttrs::aligned(3));
            let vb = b.load(Ty::I64, Flags::NONE, rhs, 0, MemAttrs::aligned(3));
            let cmp = b.icmp(IntCond::Ult, va, vb);
            let byte = b.convert(Op::Zext, Flags::NONE, Ty::I8, cmp);
            b.ret(&[byte]);
            b.finish_checked().expect("lt builds")
        };

        // Mid callee: `min(self: i64, other: i64) -> i64`
        // spills `other` and `self` into stack slots, calls `lt(&other, &self) -> i8`, selects min.
        let min_fn = {
            let mut b = Builder::<_, Full>::new_in(64, alloc);
            let own_sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I64), Param::of(Ty::I64)],
                &[Param::of(Ty::I64)],
            );
            b.set_signature(own_sig);
            b.set_attrs(FnAttrs::default().with_inline(Inline::Hint));
            let bool_ret = Param { ext: Ext::Zext, ..Param::of(Ty::I8) };
            let lt_sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::PTR), Param::of(Ty::PTR)],
                &[bool_ret],
            );
            let lt_ref = b.reference(RefKind::Func, 0);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let self_v = b.param(Ty::I64);
            let other_v = b.param(Ty::I64);
            let s_other = b.slot(8, 3, SlotKind::Temp);
            let s_self = b.slot(8, 3, SlotKind::Temp);
            let p_other = b.slot_addr(s_other);
            let p_self = b.slot_addr(s_self);
            b.store(Flags::NONE, other_v, p_other, 0, MemAttrs::aligned(3));
            b.store(Flags::NONE, self_v, p_self, 0, MemAttrs::aligned(3));
            let is_lt = b.call(Flags::NONE, lt_ref, lt_sig, &[p_other, p_self]);
            let zero = b.iconst(Ty::I8, 0);
            let cond = b.icmp(IntCond::Ne, is_lt, zero);
            let chosen = b.select(Flags::NONE, cond, other_v, self_v);
            b.ret(&[chosen]);
            b.finish_checked().expect("min builds")
        };

        // Caller: `caller(x: i64, y: i64) -> i64` calls `min(x, y)`.
        let caller = {
            let mut b = Builder::<_, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I64), Param::of(Ty::I64)],
                &[Param::of(Ty::I64)],
            );
            b.set_signature(sig);
            let min_ref = b.reference(RefKind::Func, 0);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let x = b.param(Ty::I64);
            let y = b.param(Ty::I64);
            let m = b.call(Flags::NONE, min_ref, sig, &[x, y]);
            b.ret(&[m]);
            b.finish_checked().expect("caller builds")
        };

        // Also build an `Op::Invoke` variant of `caller` (e.g. under `panic = unwind` with cleanup pad)
        // calling the unwind-free `min -> lt` chain.
        let invoke_caller = {
            let mut b = Builder::<_, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I64), Param::of(Ty::I64)],
                &[Param::of(Ty::I64)],
            );
            b.set_signature(sig);
            let min_ref = b.reference(RefKind::Func, 0);
            let entry = b.create_block(BlockKind::Normal);
            let normal = b.create_block(BlockKind::Normal);
            let pad = b.create_block(BlockKind::Cleanup);
            b.begin(entry);
            let x = b.param(Ty::I64);
            let y = b.param(Ty::I64);
            let m = b.invoke(Flags::NONE, min_ref, sig, &[x, y], normal, pad);
            b.begin(pad);
            let exn = b.param(Ty::PTR);
            b.resume(exn);
            b.begin(normal);
            b.ret(&[m]);
            b.finish_checked().expect("invoke_caller builds")
        };

        let mut bodies = OwnVec::new_in(alloc);
        bodies.push((body_id(1), caller));
        bodies.push((body_id(2), min_fn));
        bodies.push((body_id(3), lt_fn));
        bodies.push((body_id(4), invoke_caller));
        let mut links = OwnVec::new_in(alloc);
        links.push((body_id(1), RefId(0), body_id(2)));
        links.push((body_id(2), RefId(0), body_id(3)));
        links.push((body_id(4), RefId(0), body_id(2)));
        let bank = LocalBank { bodies, links };

        // 1. Direct `Op::Call`: both `min` and nested `lt` inline completely.
        let root_view = bank.bodies[0].1.view();
        let guided = inline_guided(
            root_view,
            body_id(1),
            &bank,
            &NoWeights,
            InlinePolicy::default(),
            alloc,
        )
        .expect("nested inline_guided succeeds");

        let out_view = guided.func.view();
        assert_eq!(guided.read[..], [body_id(2), body_id(3)]);
        assert_eq!((0..out_view.len()).filter(|&i| out_view.op(i) == Op::Call).count(), 0);
        verify(out_view).expect("nested inlined func verifies");

        for (x, y) in [(10u128, 3u128), (3, 10), (7, 7), (0, u64::MAX as u128)] {
            let args = [Val::Int(x), Val::Int(y)];
            let before = run_calls_in(root_view, body_id(1), &args, TEST_FUEL, &bank, alloc)
                .expect("original runs");
            let after = run_in(out_view, &args, TEST_FUEL, alloc).expect("inlined runs");
            assert_eq!(after, before);
            assert_eq!(after, Outcome::<_>::Returned({
                let mut v = Vec::new_in(alloc);
                v.push(Val::Int(x.min(y)));
                v
            }));
        }

        // 2. Unwind-free `Op::Invoke`: both `min` and nested `lt` inline completely with 0 invokes/calls.
        let inv_view = bank.bodies[3].1.view();
        let inv_guided = inline_guided(
            inv_view,
            body_id(4),
            &bank,
            &NoWeights,
            InlinePolicy::default(),
            alloc,
        )
        .expect("invoke nested inline_guided succeeds");
        let inv_out = inv_guided.func.view();
        assert_eq!(inv_guided.read[..], [body_id(2), body_id(3)]);
        assert_eq!(
            (0..inv_out.len())
                .filter(|&i| matches!(inv_out.op(i), Op::Call | Op::Invoke))
                .count(),
            0
        );
        verify(inv_out).expect("invoke inlined func verifies");
        for (x, y) in [(10u128, 3u128), (3, 10), (7, 7)] {
            let after = run_in(inv_out, &[Val::Int(x), Val::Int(y)], TEST_FUEL, alloc)
                .expect("invoke inlined runs");
            assert_eq!(after, Outcome::<_>::Returned({
                let mut v = Vec::new_in(alloc);
                v.push(Val::Int(x.min(y)));
                v
            }));
        }
    }

    #[test]
    fn inlines_unwinding_invoke_rewriting_call_and_call_indirect() {
        let heap = blaze_own::testing::heap();
        let alloc = heap.alloc();

        // Callee: `helper(x: i64, fn_ptr: ptr) -> i64`: calls opaque extern `ext(x)` and
        // `call_indirect fn_ptr(r)` — both may unwind!
        let helper = {
            let mut b = Builder::<_, Full>::new_in(64, alloc);
            let own_sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I64), Param::of(Ty::PTR)],
                &[Param::of(Ty::I64)],
            );
            let one_sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I64)],
                &[Param::of(Ty::I64)],
            );
            b.set_signature(own_sig);
            b.set_attrs(FnAttrs::default().with_inline(Inline::Hint));
            let ext_ref = b.reference(RefKind::Func, 0);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let x = b.param(Ty::I64);
            let fp = b.param(Ty::PTR);
            let r1 = b.call(Flags::NONE, ext_ref, one_sig, &[x]);
            let r2 = b.call_indirect(Flags::NONE, fp, one_sig, &[r1]);
            let sum = b.binary(Op::Add, Flags::NONE, r1, r2);
            b.ret(&[sum]);
            b.finish_checked().expect("helper builds")
        };

        // Caller: `invoke helper(x, fp) -> normal, pad(Cleanup)`
        let caller = {
            let mut b = Builder::<_, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I64), Param::of(Ty::PTR)],
                &[Param::of(Ty::I64)],
            );
            b.set_signature(sig);
            let helper_ref = b.reference(RefKind::Func, 0);
            let entry = b.create_block(BlockKind::Normal);
            let normal = b.create_block(BlockKind::Normal);
            let pad = b.create_block(BlockKind::Cleanup);
            b.begin(entry);
            let x = b.param(Ty::I64);
            let fp = b.param(Ty::PTR);
            let res = b.invoke(Flags::NONE, helper_ref, sig, &[x, fp], normal, pad);
            b.begin(normal);
            b.ret(&[res]);
            b.begin(pad);
            let exc = b.param(Ty::PTR);
            b.resume(exc);
            b.finish_checked().expect("caller builds")
        };

        let mut bodies = OwnVec::new_in(alloc);
        bodies.push((body_id(1), caller));
        bodies.push((body_id(2), helper));
        let mut links = OwnVec::new_in(alloc);
        links.push((body_id(1), RefId(0), body_id(2)));
        let bank = LocalBank { bodies, links };

        let guided = inline_guided(
            bank.bodies[0].1.view(),
            body_id(1),
            &bank,
            &NoWeights,
            InlinePolicy::default(),
            alloc,
        )
        .expect("unwinding invoke inline succeeds");
        let out = guided.func.view();
        verify(out).expect("rewritten unwinding invoke verifies");
        assert_eq!(guided.read[..], [body_id(2)]);
        assert_eq!((0..out.len()).filter(|&i| matches!(out.op(i), Op::Call | Op::CallIndirect)).count(), 0);
        assert_eq!((0..out.len()).filter(|&i| out.op(i) == Op::Invoke).count(), 1);
        assert_eq!((0..out.len()).filter(|&i| out.op(i) == Op::InvokeIndirect).count(), 1);
    }

    #[test]
    fn inlines_unwinding_invoke_with_callee_cleanup_and_resume() {
        let heap = blaze_own::testing::heap();
        let alloc = heap.alloc();

        // Callee has its own `Op::Invoke` -> `callee_pad(Cleanup)` which calls `drop_callee(p)`
        // (plain `Op::Call` inside cleanup — must NOT be rewritten to `Invoke`!) and then
        // executes `Op::Resume(exc)` (must be rewritten to `Jump(caller_pad_body, [exc])`).
        let callee = {
            let mut b = Builder::<_, Full>::new_in(64, alloc);
            let own_sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
            let drop_sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[]);
            b.set_signature(own_sig);
            b.set_attrs(FnAttrs::default().with_inline(Inline::Hint));
            let may_panic = b.reference(RefKind::Func, 0);
            let drop_fn = b.reference(RefKind::Func, 1);
            let entry = b.create_block(BlockKind::Normal);
            let norm = b.create_block(BlockKind::Normal);
            let pad = b.create_block(BlockKind::Cleanup);
            b.begin(entry);
            let x = b.param(Ty::I64);
            let r = b.invoke(Flags::NONE, may_panic, own_sig, &[x], norm, pad);
            b.begin(norm);
            let one = b.iconst(Ty::I64, 1);
            let r2 = b.binary(Op::Add, Flags::NONE, r, one);
            b.ret(&[r2]);
            b.begin(pad);
            let exc = b.param(Ty::PTR);
            b.call(Flags::NONE, drop_fn, drop_sig, &[x]);
            b.resume(exc);
            b.finish_checked().expect("callee builds")
        };

        // Caller invokes `callee(x)` with its own `caller_pad(Cleanup)` that runs `drop_caller(x)`
        // and resumes.
        let caller = {
            let mut b = Builder::<_, Full>::new_in(64, alloc);
            let own_sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
            let drop_sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[]);
            b.set_signature(own_sig);
            let callee_ref = b.reference(RefKind::Func, 0);
            let drop_caller = b.reference(RefKind::Func, 1);
            let entry = b.create_block(BlockKind::Normal);
            let norm = b.create_block(BlockKind::Normal);
            let pad = b.create_block(BlockKind::Cleanup);
            b.begin(entry);
            let x = b.param(Ty::I64);
            let r = b.invoke(Flags::NONE, callee_ref, own_sig, &[x], norm, pad);
            b.begin(norm);
            b.ret(&[r]);
            b.begin(pad);
            let exc = b.param(Ty::PTR);
            b.call(Flags::NONE, drop_caller, drop_sig, &[x]);
            b.resume(exc);
            b.finish_checked().expect("caller builds")
        };

        let mut bodies = OwnVec::new_in(alloc);
        bodies.push((body_id(1), caller));
        bodies.push((body_id(2), callee));
        let mut links = OwnVec::new_in(alloc);
        links.push((body_id(1), RefId(0), body_id(2)));
        let bank = LocalBank { bodies, links };

        let guided = inline_guided(
            bank.bodies[0].1.view(),
            body_id(1),
            &bank,
            &NoWeights,
            InlinePolicy::default(),
            alloc,
        )
        .expect("callee with cleanup+resume inlines cleanly");
        let out = guided.func.view();
        verify(out).expect("inlined cleanup+resume verifies");
        assert_eq!(guided.read[..], [body_id(2)]);
        // Only caller's final `Op::Resume` remains; callee's `Op::Resume` became `Op::Jump` into
        // caller's split `BlockKind::Cold` cleanup body!
        assert_eq!((0..out.len()).filter(|&i| out.op(i) == Op::Resume).count(), 1);
        // Both cleanup drops stay plain `Op::Call` (never rewritten to `Op::Invoke`).
        assert_eq!((0..out.len()).filter(|&i| out.op(i) == Op::Call).count(), 2);
        // `may_panic` stays `Op::Invoke` to callee's cleanup pad.
        assert_eq!((0..out.len()).filter(|&i| out.op(i) == Op::Invoke).count(), 1);
    }

    #[test]
    fn inlines_three_level_nested_invoke_call_resume_chain() {
        let heap = blaze_own::testing::heap();
        let alloc = heap.alloc();

        // Leaf `c`: calls opaque `ext(x)` (plain `Op::Call`) AND invokes `ext2(x)` with cleanup+resume.
        let leaf_c = {
            let mut b = Builder::<_, Full>::new_in(64, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
            b.set_signature(sig);
            b.set_attrs(FnAttrs::default().with_inline(Inline::Hint));
            let ext1 = b.reference(RefKind::Func, 0);
            let ext2 = b.reference(RefKind::Func, 1);
            let entry = b.create_block(BlockKind::Normal);
            let norm = b.create_block(BlockKind::Normal);
            let pad = b.create_block(BlockKind::Cleanup);
            b.begin(entry);
            let x = b.param(Ty::I64);
            let y = b.call(Flags::NONE, ext1, sig, &[x]);
            let z = b.invoke(Flags::NONE, ext2, sig, &[y], norm, pad);
            b.begin(norm);
            b.ret(&[z]);
            b.begin(pad);
            let exc = b.param(Ty::PTR);
            b.resume(exc);
            b.finish_checked().expect("leaf_c builds")
        };

        // Mid `mid_b`: calls `leaf_c(x)` via plain `Op::Call` (inherits `top_a`'s unwind pad!).
        let mid_b = {
            let mut b = Builder::<_, Full>::new_in(64, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
            b.set_signature(sig);
            b.set_attrs(FnAttrs::default().with_inline(Inline::Hint));
            let c_ref = b.reference(RefKind::Func, 0);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let x = b.param(Ty::I64);
            let r = b.call(Flags::NONE, c_ref, sig, &[x]);
            b.ret(&[r]);
            b.finish_checked().expect("mid_b builds")
        };

        // Top `top_a`: invokes `mid_b(x)` with `pad_a(Cleanup)`.
        let top_a = {
            let mut b = Builder::<_, Full>::new_in(64, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
            b.set_signature(sig);
            let b_ref = b.reference(RefKind::Func, 0);
            let entry = b.create_block(BlockKind::Normal);
            let norm = b.create_block(BlockKind::Normal);
            let pad = b.create_block(BlockKind::Cleanup);
            b.begin(entry);
            let x = b.param(Ty::I64);
            let r = b.invoke(Flags::NONE, b_ref, sig, &[x], norm, pad);
            b.begin(norm);
            b.ret(&[r]);
            b.begin(pad);
            let exc = b.param(Ty::PTR);
            b.resume(exc);
            b.finish_checked().expect("top_a builds")
        };

        let mut bodies = OwnVec::new_in(alloc);
        bodies.push((body_id(1), top_a));
        bodies.push((body_id(2), mid_b));
        bodies.push((body_id(3), leaf_c));
        let mut links = OwnVec::new_in(alloc);
        links.push((body_id(1), RefId(0), body_id(2)));
        links.push((body_id(2), RefId(0), body_id(3)));
        let bank = LocalBank { bodies, links };

        let guided = inline_guided(
            bank.bodies[0].1.view(),
            body_id(1),
            &bank,
            &NoWeights,
            InlinePolicy::default(),
            alloc,
        )
        .expect("3-level nested invoke/call/resume inlines");
        let out = guided.func.view();
        verify(out).expect("3-level nested unwinding LIR verifies");
        assert_eq!(guided.read[..], [body_id(2), body_id(3)]);
        // `ext1` became `Op::Invoke` to `top_a`'s pad, `ext2` stayed `Op::Invoke` to `leaf_c`'s pad.
        assert_eq!((0..out.len()).filter(|&i| out.op(i) == Op::Call).count(), 0);
        assert_eq!((0..out.len()).filter(|&i| out.op(i) == Op::Invoke).count(), 2);
        assert_eq!((0..out.len()).filter(|&i| out.op(i) == Op::Resume).count(), 1);
    }

    #[test]
    fn catch_invoke_inlines_plain_calls_but_refuses_inner_eh_shadowing() {
        let heap = blaze_own::testing::heap();
        let alloc = heap.alloc();

        // Plain-call callee: safe to inline into `BlockKind::Catch` (Call -> Invoke(..., catch_pad)).
        let plain_callee = {
            let mut b = Builder::<_, Full>::new_in(64, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
            b.set_signature(sig);
            b.set_attrs(FnAttrs::default().with_inline(Inline::Hint));
            let ext = b.reference(RefKind::Func, 0);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let x = b.param(Ty::I64);
            let r = b.call(Flags::NONE, ext, sig, &[x]);
            b.ret(&[r]);
            b.finish_checked().expect("plain_callee builds")
        };

        // Inner-cleanup callee: must NOT inline into `BlockKind::Catch` (would shadow Phase 1 Catch).
        let eh_callee = {
            let mut b = Builder::<_, Full>::new_in(64, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
            b.set_signature(sig);
            b.set_attrs(FnAttrs::default().with_inline(Inline::Hint));
            let ext = b.reference(RefKind::Func, 0);
            let entry = b.create_block(BlockKind::Normal);
            let norm = b.create_block(BlockKind::Normal);
            let pad = b.create_block(BlockKind::Cleanup);
            b.begin(entry);
            let x = b.param(Ty::I64);
            let r = b.invoke(Flags::NONE, ext, sig, &[x], norm, pad);
            b.begin(norm);
            b.ret(&[r]);
            b.begin(pad);
            let exc = b.param(Ty::PTR);
            b.resume(exc);
            b.finish_checked().expect("eh_callee builds")
        };

        let make_catch_caller = || {
            let mut b = Builder::<_, Full>::new_in(64, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
            b.set_signature(sig);
            let target = b.reference(RefKind::Func, 0);
            let entry = b.create_block(BlockKind::Normal);
            let norm = b.create_block(BlockKind::Normal);
            let catch = b.create_block(BlockKind::Catch);
            b.begin(entry);
            let x = b.param(Ty::I64);
            let r = b.invoke(Flags::NONE, target, sig, &[x], norm, catch);
            b.begin(norm);
            b.ret(&[r]);
            b.begin(catch);
            let _exc = b.param(Ty::PTR);
            let zero = b.iconst(Ty::I64, 0);
            b.ret(&[zero]);
            b.finish_checked().expect("catch caller builds")
        };

        let mut bodies = OwnVec::new_in(alloc);
        bodies.push((body_id(1), make_catch_caller()));
        bodies.push((body_id(2), plain_callee));
        bodies.push((body_id(3), make_catch_caller()));
        bodies.push((body_id(4), eh_callee));
        let mut links = OwnVec::new_in(alloc);
        links.push((body_id(1), RefId(0), body_id(2)));
        links.push((body_id(3), RefId(0), body_id(4)));
        let bank = LocalBank { bodies, links };

        let ok_inlined = inline_guided(
            bank.bodies[0].1.view(),
            body_id(1),
            &bank,
            &NoWeights,
            InlinePolicy::default(),
            alloc,
        )
        .expect("plain callee into catch succeeds");
        verify(ok_inlined.func.view()).expect("plain callee inlined into catch verifies");
        assert_eq!(ok_inlined.read[..], [body_id(2)]);

        let refused_shadow = inline_guided(
            bank.bodies[2].1.view(),
            body_id(3),
            &bank,
            &NoWeights,
            InlinePolicy::default(),
            alloc,
        )
        .expect("inner-eh callee into catch stays un-inlined cleanly");
        verify(refused_shadow.func.view()).expect("un-inlined catch caller verifies");
        assert!(refused_shadow.read.is_empty());
    }

    #[test]
    fn inlines_multi_return_unwinding_invoke() {
        let heap = blaze_own::testing::heap();
        let alloc = heap.alloc();

        // Multi-return callee `(ptr, i64) -> (ptr, i64)` with an un-inlined `Call` that may unwind.
        let callee = {
            let mut b = Builder::<_, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::PTR), Param::of(Ty::I64)],
                &[Param::of(Ty::PTR), Param::of(Ty::I64)],
            );
            let check_sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
            b.set_signature(sig);
            b.set_attrs(FnAttrs::default().with_inline(Inline::Hint));
            let check_fn = b.reference(RefKind::Func, 0);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let p = b.param(Ty::PTR);
            let len = b.param(Ty::I64);
            let new_len = b.call(Flags::NONE, check_fn, check_sig, &[len]);
            b.ret(&[p, new_len]);
            b.finish_checked().expect("multi-ret unwinding callee builds")
        };

        let caller = {
            let mut b = Builder::<_, Full>::new_in(64, alloc);
            let own_sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::PTR), Param::of(Ty::I64)],
                &[Param::of(Ty::I64)],
            );
            let call_sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::PTR), Param::of(Ty::I64)],
                &[Param::of(Ty::PTR), Param::of(Ty::I64)],
            );
            b.set_signature(own_sig);
            let callee_ref = b.reference(RefKind::Func, 0);
            let entry = b.create_block(BlockKind::Normal);
            let norm = b.create_block(BlockKind::Normal);
            let pad = b.create_block(BlockKind::Cleanup);
            b.begin(entry);
            let p = b.param(Ty::PTR);
            let len = b.param(Ty::I64);
            let pair = b.invoke(Flags::NONE, callee_ref, call_sig, &[p, len], norm, pad);
            b.begin(norm);
            let _p2 = b.project(pair, 0);
            let len2 = b.project(pair, 1);
            b.ret(&[len2]);
            b.begin(pad);
            let exc = b.param(Ty::PTR);
            b.resume(exc);
            b.finish_checked().expect("multi-ret invoke caller builds")
        };

        let mut bodies = OwnVec::new_in(alloc);
        bodies.push((body_id(1), caller));
        bodies.push((body_id(2), callee));
        let mut links = OwnVec::new_in(alloc);
        links.push((body_id(1), RefId(0), body_id(2)));
        let bank = LocalBank { bodies, links };

        let guided = inline_guided(
            bank.bodies[0].1.view(),
            body_id(1),
            &bank,
            &NoWeights,
            InlinePolicy::default(),
            alloc,
        )
        .expect("multi-ret unwinding invoke inlines");
        let out = guided.func.view();
        verify(out).expect("multi-ret unwinding invoke verifies");
        assert_eq!(guided.read[..], [body_id(2)]);
        assert_eq!((0..out.len()).filter(|&i| out.op(i) == Op::Project).count(), 0);
        assert_eq!((0..out.len()).filter(|&i| out.op(i) == Op::Call).count(), 0);
        assert_eq!((0..out.len()).filter(|&i| out.op(i) == Op::Invoke).count(), 1);
    }

    #[test]
    fn inlining_variadic_c_call_into_invoke_keeps_plain_call() {
        let heap = blaze_own::testing::heap();
        let alloc = heap.alloc();

        // Callee wraps a C-variadic call (like `fcntl(fd, cmd, arg)`).
        let callee = {
            let mut b = Builder::<_, Full>::new_in(64, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I32)], &[Param::of(Ty::I32)]);
            let var_sig = b.sig(Conv::C, Variadic::Yes, &[Param::of(Ty::I32), Param::of(Ty::I32)], &[Param::of(Ty::I32)]);
            b.set_signature(sig);
            b.set_attrs(FnAttrs::default().with_inline(Inline::Hint));
            let ext_fcntl = b.reference(RefKind::Func, 99);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let fd = b.param(Ty::I32);
            let cmd = b.iconst(Ty::I32, 2);
            let arg = b.iconst(Ty::I32, 1);
            let r = b.call(Flags::NONE, ext_fcntl, var_sig, &[fd, cmd, arg]);
            b.ret(&[r]);
            b.finish_checked().expect("variadic wrapper callee builds")
        };

        // Caller calls `callee` via `Op::Invoke` with a cleanup landing pad.
        let caller = {
            let mut b = Builder::<_, Full>::new_in(64, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I32)], &[Param::of(Ty::I32)]);
            b.set_signature(sig);
            let callee_ref = b.reference(RefKind::Func, 0);
            let entry = b.create_block(BlockKind::Normal);
            let norm = b.create_block(BlockKind::Normal);
            let pad = b.create_block(BlockKind::Cleanup);
            b.begin(entry);
            let fd = b.param(Ty::I32);
            let r = b.invoke(Flags::NONE, callee_ref, sig, &[fd], norm, pad);
            b.begin(norm);
            b.ret(&[r]);
            b.begin(pad);
            let exc = b.param(Ty::PTR);
            b.resume(exc);
            b.finish_checked().expect("invoke caller builds")
        };

        let mut bodies = OwnVec::new_in(alloc);
        bodies.push((body_id(1), caller));
        bodies.push((body_id(2), callee));
        let mut links = OwnVec::new_in(alloc);
        links.push((body_id(1), RefId(0), body_id(2)));
        let bank = LocalBank { bodies, links };

        let guided = inline_guided(
            bank.bodies[0].1.view(),
            body_id(1),
            &bank,
            &NoWeights,
            InlinePolicy::default(),
            alloc,
        )
        .expect("variadic wrapper inlines into invoke caller");
        let out = guided.func.view();
        verify(out).expect("inlined variadic call verifies");
        assert_eq!((0..out.len()).filter(|&i| out.op(i) == Op::Call).count(), 1, "variadic C call must stay Op::Call");
        assert_eq!((0..out.len()).filter(|&i| out.op(i) == Op::Invoke).count(), 0, "variadic C call must never be promoted to Op::Invoke");
    }
}
