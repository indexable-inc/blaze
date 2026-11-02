//! rustc's MIR `Inline` pass (`compiler/rustc_mir_transform/src/inline.rs`, its
//! `NormalInliner`): calls of small functions replaced by their optimized bodies, where rustc
//! runs it, after the first `InstSimplify` of level 2. It decides what rustc decides, from the
//! same inputs, each a query or a tracked read: the callee's attributes, target features and
//! optimized body, the caller's [`MirOptions`] and the settings of the crates. A callee it does
//! not take stays a call, as one rustc refuses does; where it refuses what rustc takes, the
//! check says why.
//!
//! An optimized body says which instances it inlined ([`Inlined::callees`], each once): the
//! code generator keys the caller's code on their code too. [`spans`] says where the code of
//! an inlined body was written.
//!
//! [`Inlined::callees`]: crate::mir::Inlined::callees

mod cost;
mod cycle;
mod integrate;
pub(super) mod spans;
#[cfg(test)]
mod tests;

use std::ops::Range;

use base_db::{Crate, Db};
use blaze_hash::{HashMap, HashSet};
use either::Either;
use hir_def::{
    CallableDefId, FunctionId, HasModule, ItemContainerId, Lookup, attrs::AttrFlags,
    signatures::FunctionSignature,
};
use intern::Symbol;
use rustc_abi::ExternAbi;
use rustc_type_ir::{
    ClosureKind, FloatTy, TypeFlags, TypeFoldable, TypeFolder, TypeSuperFoldable, TypeVisitableExt,
    inherent::{GenericArgs as _, IntoKind, Ty as _},
};
use smallvec::SmallVec;

use crate::{
    db::HirDatabase,
    method_resolution::is_dyn_method,
    mir::{
        BasicBlockId, InlinedBody, InlinedCallee, MirBody, MirOptLevel, MirOptions, MirSpan,
        Operand, OperandKind, StatementKind, Terminator, TerminatorKind, monomorphized, overflow,
        return_slot, transform::Ctx,
    },
    next_solver::{
        Const, ConstKind, DbInterner, GenericArgs, Region, RegionExt, RegionKind, Ty, TyKind,
        references_non_lt_error,
    },
    traits::ParamEnvAndCrate,
};

use super::{block_id, fn_can_unwind};

pub use spans::{CallerLocation, caller_location, owner_of, source_span, written_checked_at};

/// rustc's `HISTORY_DEPTH_LIMIT`: past this many callees inlined into one another, only those
/// asking to be inlined are, and only with one block.
const HISTORY_DEPTH_LIMIT: usize = 20;
/// rustc's `TOP_DOWN_DEPTH_LIMIT`: as many inlined callees that made more than one call.
const TOP_DOWN_DEPTH_LIMIT: usize = 5;
/// The defaults of `-Zinline-mir-threshold`, `-Zinline-mir-hint-threshold` and
/// `-Zinline-mir-forwarder-threshold`.
const THRESHOLD: usize = 50;
const HINT_THRESHOLD: usize = 100;
const FORWARDER_THRESHOLD: usize = 30;

/// rustc's `InlineAttr`; `#[rustc_force_inline]` is its `Force`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InlineAttr {
    None,
    Hint,
    Always,
    Never,
    Force,
}

/// What the inliner reads of the attributes of a body: rustc's `CodegenFnAttrs` of it. A
/// closure's are not read and are none.
#[derive(Debug, Clone, Copy)]
pub(super) struct Attrs {
    inline: InlineAttr,
    flags: AttrFlags,
}

impl Attrs {
    pub(super) fn of_fn(db: &Db, func: FunctionId) -> Self {
        let flags = AttrFlags::query(db, func.into());
        let inline = if flags.contains(AttrFlags::RUSTC_FORCE_INLINE) {
            InlineAttr::Force
        } else if flags.contains(AttrFlags::INLINE_NEVER) {
            InlineAttr::Never
        } else if flags.contains(AttrFlags::INLINE_ALWAYS) {
            InlineAttr::Always
        } else if flags.contains(AttrFlags::IS_INLINE) {
            InlineAttr::Hint
        } else {
            InlineAttr::None
        };
        Attrs { inline, flags }
    }

    fn of(db: &Db, body: InlinedBody<'_>) -> Self {
        match body {
            InlinedBody::Fn(func) => Self::of_fn(db, func),
            InlinedBody::Closure(_) => {
                Attrs { inline: InlineAttr::None, flags: AttrFlags::empty() }
            }
        }
    }

    /// `#[inline]` in any of its forms but `never`.
    pub(super) fn hinted(self) -> bool {
        matches!(self.inline, InlineAttr::Hint | InlineAttr::Always | InlineAttr::Force)
    }

    fn has(self, flag: AttrFlags) -> bool {
        self.flags.contains(flag)
    }

    /// rustc's `contains_extern_indicator`: something outside the crate names it.
    fn extern_indicator(self) -> bool {
        self.flags.intersects(
            AttrFlags::NO_MANGLE
                | AttrFlags::HAS_EXPORT_NAME
                | AttrFlags::HAS_LINKAGE
                | AttrFlags::LINKED_BY_NAME,
        )
    }
}

/// The target features `body` is compiled with, as declared: a closure has its function's.
fn target_features<'db>(
    db: &'db Db,
    body: InlinedBody<'db>,
) -> &'db HashSet<Symbol> {
    let func = match body {
        InlinedBody::Fn(func) => Some(func),
        InlinedBody::Closure(_) => owner_of(db, body).as_function(),
    };
    match func {
        Some(func) => AttrFlags::target_features(db, func),
        None => const { &HashSet::with_hasher(blaze_hash::FixedState) },
    }
}

/// rustc's `fn_can_unwind` where panics unwind: whether unwinding may leave the body.
fn can_unwind(db: &Db, body: InlinedBody<'_>) -> bool {
    let InlinedBody::Fn(func) = body else { return true };
    fn_can_unwind(db, func)
}

/// Whether unwinding may leave `body`: an edge that continues unwinding out of it.
fn unwinds_out(body: &MirBody<'_>) -> bool {
    body.basic_blocks.iter().any(|(_, block)| match block.terminator.as_ref().map(|it| &it.kind) {
        Some(TerminatorKind::UnwindResume) => true,
        Some(
            TerminatorKind::Call { cleanup: None, .. }
            | TerminatorKind::Drop { unwind: None, .. }
            | TerminatorKind::Assert { cleanup: None, .. },
        ) => !block.is_cleanup,
        _ => false,
    })
}

/// rustc's `Inline` pass over the body of `caller` with `options`; says whether it inlined
/// anything, after which rustc's `SimplifyCfg` runs.
pub(super) fn inline<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &mut MirBody<'db>,
    caller: InlinedBody<'db>,
    options: MirOptions,
) -> bool {
    // rustc's `check_caller_mir_body`: a coroutine's layout is computed from its optimized body.
    if body.coroutine.is_some() {
        return false;
    }
    let db = ctx.db;
    let krate = ctx.krate();
    let caller_attrs = Attrs::of(db, caller);
    let mut inliner = Inliner {
        ctx,
        db,
        caller,
        caller_attrs,
        caller_features: target_features(db, caller),
        options,
        session: cost::Session {
            ub_checks: options.ub_checks,
            overflow_checks: overflow::crate_checks_overflow(db, krate).unwrap_or(false),
        },
        env: ParamEnvAndCrate { param_env: ctx.env, krate },
        history: SmallVec::new(),
        top_down_counter: 0,
        changed: false,
        caller_is_inline_forwarder: caller_attrs.hinted() && body_is_forwarder(body),
        callgraph: None,
        listed: HashMap::default(),
    };
    inliner.run(body);
    inliner.changed
}

/// rustc's `NormalInliner` and its `CallSite`s.
struct Inliner<'a, 'f, 'db> {
    ctx: &'a Ctx<'f, 'db>,
    db: &'db Db,
    caller: InlinedBody<'db>,
    caller_attrs: Attrs,
    caller_features: &'db HashSet<Symbol>,
    options: MirOptions,
    session: cost::Session,
    env: ParamEnvAndCrate<'db>,
    /// The callees inlined into one another down to the blocks being looked at.
    history: SmallVec<[InlinedBody<'db>; 8]>,
    /// How many callees inlined down to here made more than one call.
    top_down_counter: usize,
    changed: bool,
    /// The caller is `#[inline]` and only calls another function: as it is inlined itself,
    /// less is inlined into it.
    caller_is_inline_forwarder: bool,
    /// [`cycle::callgraph_cyclic`] of the caller, once a callee of its crate asks.
    callgraph: Option<Option<&'db cycle::Callgraph<'db>>>,
    /// Where each instance is in the caller's [`Inlined::callees`](crate::mir::Inlined::callees).
    listed: HashMap<(InlinedBody<'db>, GenericArgs<'db>), u32>,
}

/// A call that resolves to a body.
struct CallSite<'db> {
    block: BasicBlockId,
    callee: InlinedBody<'db>,
    args: GenericArgs<'db>,
    /// The call passes a closure's arguments as the tuple of the `rust-call` ABI, which the
    /// closure's body takes one by one.
    spread: bool,
}

impl<'a, 'f, 'db> Inliner<'a, 'f, 'db> {
    fn past_depth_limit(&self) -> bool {
        self.history.len() > HISTORY_DEPTH_LIMIT || self.top_down_counter > TOP_DOWN_DEPTH_LIMIT
    }

    /// rustc's `process_blocks`, without recursion: the blocks of each inlined callee are
    /// looked at with it in the history, before the blocks after the call.
    fn run(&mut self, body: &mut MirBody<'db>) {
        let mut stack: SmallVec<[(Range<usize>, bool); 8]> = SmallVec::new();
        stack.push((0..body.basic_blocks.len(), false));
        while let Some((blocks, _)) = stack.last_mut() {
            let Some(index) = blocks.next() else {
                if let Some((_, true)) = stack.pop() {
                    self.history.pop();
                    if self.history.is_empty() {
                        self.top_down_counter = 0;
                    }
                }
                continue;
            };
            let block = block_id(index);
            if body.basic_blocks[block].is_cleanup {
                continue;
            }
            let Some(site) = self.resolve_callsite(body, block) else { continue };
            let Some(new_blocks) = self.try_inlining(body, &site) else { continue };
            self.changed = true;
            let db = self.db;
            let calls = new_blocks
                .clone() // clone: a `Range<usize>`, two words
                .filter(|&it| {
                    let written = |span| written_checked_at(db, body, span);
                    cost::block_is_call_like(&body.basic_blocks[block_id(it)], written)
                })
                .count();
            if calls > 1 {
                self.top_down_counter += 1;
            }
            self.history.push(site.callee);
            stack.push((new_blocks, true));
        }
    }

    /// rustc's `resolve_callsite`: the call that ends `block`, if it calls a function by name
    /// that resolves to a body not inlined down to here.
    fn resolve_callsite(&self, body: &MirBody<'db>, block: BasicBlockId) -> Option<CallSite<'db>> {
        let Some(Terminator { kind: TerminatorKind::Call { func, args, .. }, .. }) =
            &body.basic_blocks[block].terminator
        else {
            return None;
        };
        let (func, fn_args) = fn_def(body, func)?;
        let fn_args = fold(self.ctx, fn_args, None)?;
        let (callee, callee_args) = resolve(self.ctx, self.env, func, fn_args)?;
        if self.history.contains(&callee) {
            return None;
        }
        let declared = FunctionSignature::of(self.db, func).abi;
        // The body agrees with the ABI of the trait method it implements.
        if let InlinedBody::Fn(item) = callee
            && FunctionSignature::of(self.db, item).abi != declared
        {
            return None;
        }
        let spread = declared == ExternAbi::RustCall
            && matches!(callee, InlinedBody::Closure(_))
            && args.len() == 2;
        Some(CallSite { block, callee, args: callee_args, spread })
    }

    /// rustc's `try_inlining`: the blocks of the callee in the caller, if it inlined it.
    fn try_inlining(
        &mut self,
        body: &mut MirBody<'db>,
        site: &CallSite<'db>,
    ) -> Option<Range<usize>> {
        let db = self.db;
        let ctx = self.ctx;
        self.check_mir_is_available(site.callee)?;
        let attrs = Attrs::of(db, site.callee);
        is_inline_valid_on_fn(db, site.callee, attrs)?;
        self.check_codegen_attributes(site, attrs)?;

        let Some(Terminator { kind: TerminatorKind::Call { args, destination, .. }, .. }) =
            &body.basic_blocks[site.block].terminator
        else {
            return None;
        };
        let arg_tys: SmallVec<[Ty<'db>; 4]> =
            // alloc: inline `SmallVec`, on the stack for up to four arguments
            args.iter().map(|it| operand_ty(ctx, body, it)).collect();
        // Inlining an unsized argument would make an unsized local.
        if !arg_tys.iter().all(|&ty| ctx.is_sized(ty)) {
            return None;
        }
        let destination_ty =
            normalize_ty(ctx, destination.as_ref().ty(body, &ctx.infcx, ctx.env).ty);

        let callee_options = self.callee_options(site.callee);
        // The callee's optimization inlines its own callees in turn, a query inside this one.
        let generic =
            stdx::ensure_sufficient_stack(|| optimized(db, site.callee, callee_options))?;
        is_inline_valid_on_body(db, site.callee, generic)?;
        if self.past_depth_limit() && generic.basic_blocks.len() > 1 {
            return None;
        }
        // rustc's `try_instantiate_mir_and_normalize_erasing_regions`, which it refuses a callee
        // it fails for (one whose types overflow the recursion limit, E0275, among them), and
        // its `validate_types`.
        let callee = match monomorphized(db, generic, &site.args.store(), &self.env.store()) {
            Ok(it) => it,
            Err(error) => {
                // A type's `Debug` reads the attached database.
                hir_ty::attach_db(db, || {
                    tracing::debug!(callee = ?site.callee, ?error, "not inlined: not monomorphized")
                });
                return None;
            }
        };
        if callee.locals.iter().any(|(_, it)| references_non_lt_error(&it.ty.as_ref())) {
            return None;
        }
        self.check_callee_mir_body(site, &callee)?;
        check_signature(ctx, site, &callee, &arg_tys, destination_ty)?;

        let written_checked = overflow::body_written_checked(db, generic);
        let index = self.list(body, site.callee, site.args, written_checked);
        // `monomorphized` instantiated the arguments of the callee's own inlined callees.
        let nested: SmallVec<[u32; 8]> = callee
            .inlined
            .callees
            .iter()
            .map(|it| self.list(body, it.body, it.args.as_ref(), it.written_checked))
            .collect(); // alloc: inline `SmallVec`, on the stack for up to eight callees
        let site = integrate::Site {
            block: site.block,
            spread: site.spread,
            callee: index,
            nested: &nested,
        };
        Some(integrate::inline_call(ctx, body, site, callee))
    }

    /// The place of the instance in the caller's [`Inlined::callees`], listing it the first
    /// time.
    ///
    /// [`Inlined::callees`]: crate::mir::Inlined::callees
    fn list(
        &mut self,
        body: &mut MirBody<'db>,
        callee: InlinedBody<'db>,
        args: GenericArgs<'db>,
        written_checked: bool,
    ) -> u32 {
        let interner = self.ctx.infcx.interner;
        let args = rustc_type_ir::fold_regions(interner, args, |_, _| Region::new_erased(interner));
        *self.listed.entry((callee, args)).or_insert_with(|| {
            let at = body.inlined.callees.len() as u32;
            let listed = InlinedCallee { body: callee, args: args.store(), written_checked };
            body.inlined.callees.push(listed);
            at
        })
    }

    /// rustc's `check_mir_is_available`: a body of the caller's crate the caller's body may be
    /// reached from is not inlined into it, so no optimized body asks for itself.
    fn check_mir_is_available(&mut self, callee: InlinedBody<'db>) -> Option<()> {
        if callee == self.caller {
            return None;
        }
        let db = self.db;
        if owner_of(db, callee).krate(db) != self.env.krate {
            return Some(());
        }
        let caller = self.caller;
        let graph = (*self.callgraph.get_or_insert_with(|| cycle::callgraph_cyclic(db, caller)))?;
        (!graph.is_cyclic(callee) && graph.reached(callee)).then_some(())
    }

    /// rustc's `check_codegen_attributes`, with `check_codegen_attributes_extra`.
    fn check_codegen_attributes(&self, site: &CallSite<'db>, attrs: Attrs) -> Option<()> {
        if attrs.inline == InlineAttr::Never || attrs.has(AttrFlags::OPTIMIZE_NONE) {
            return None;
        }
        if self.past_depth_limit() && attrs.inline == InlineAttr::None {
            return None;
        }
        // Only what rustc makes available to other crates is inlined, so that no crate's code
        // names a symbol another does not export.
        if !is_generic(site.args) && !self.cross_crate_inlinable(site.callee) {
            return None;
        }
        // rustc compares the sanitizer sets and instruction sets, which are not read here: a
        // caller or callee with `#[sanitize]` is refused, as is a callee with
        // `#[instruction_set]`, which rustc refuses unless the caller's is the same.
        if attrs.has(AttrFlags::HAS_SANITIZE)
            || self.caller_attrs.has(AttrFlags::HAS_SANITIZE)
            || attrs.has(AttrFlags::HAS_INSTRUCTION_SET)
        {
            return None;
        }
        // The ABI of the calls in the callee depends on the target features of the function
        // they are in.
        (target_features(self.db, site.callee) == self.caller_features).then_some(())
    }

    /// rustc's `check_callee_mir_body`, of the callee instantiated.
    fn check_callee_mir_body(&self, site: &CallSite<'db>, callee: &MirBody<'db>) -> Option<()> {
        let mut threshold = if self.caller_is_inline_forwarder || self.past_depth_limit() {
            FORWARDER_THRESHOLD
        } else if self.cross_crate_inlinable(site.callee) {
            HINT_THRESHOLD
        } else {
            THRESHOLD
        };
        if callee.basic_blocks.len() <= 3 {
            threshold += threshold / 4;
        }
        // Inline assembly is not moved across instruction sets; a callee with one of its own
        // was refused already.
        if self.caller_attrs.has(AttrFlags::HAS_INSTRUCTION_SET) && has_asm(callee) {
            return None;
        }
        let ctx = self.ctx;
        let cost = cost::cost(callee, self.session, written_checker(self.db, callee), |place| {
            ctx.needs_drop(place.as_ref().ty(callee, &ctx.infcx, ctx.env).ty)
        })?;
        (cost <= threshold).then_some(())
    }

    fn cross_crate_inlinable(&self, callee: InlinedBody<'db>) -> bool {
        match callee {
            InlinedBody::Fn(func) => {
                cross_crate_inlinable(self.db, func, self.callee_options(callee))
            }
            InlinedBody::Closure(_) => true,
        }
    }

    fn callee_options(&self, callee: InlinedBody<'db>) -> MirOptions {
        let krate = owner_of(self.db, callee).krate(self.db);
        callee_options(self.db, self.env.krate, krate, self.options)
    }
}

/// rustc's `is_inline_valid_on_fn`. An intrinsic never resolves to a callee.
fn is_inline_valid_on_fn(
    db: &Db,
    callee: InlinedBody<'_>,
    attrs: Attrs,
) -> Option<()> {
    if attrs.has(AttrFlags::RUSTC_NO_MIR_INLINE) || attrs.has(AttrFlags::COLD) {
        return None;
    }
    match callee {
        InlinedBody::Fn(func) => (!FunctionSignature::of(db, func).is_varargs()).then_some(()),
        InlinedBody::Closure(_) => Some(()),
    }
}

/// rustc's `is_inline_valid_on_body` (a tail call, which this MIR has none of), and what this
/// inliner refuses that rustc takes.
fn is_inline_valid_on_body(
    db: &Db,
    callee: InlinedBody<'_>,
    body: &MirBody<'_>,
) -> Option<()> {
    // rustc never calls a coroutine's body: it calls its `poll`.
    if body.coroutine.is_some() {
        return None;
    }
    // rustc ends unwinding out of a body that cannot unwind with a terminate edge in its MIR
    // (`AbortUnwindingCalls`); this MIR has none, and inlined, the unwinding would go on into
    // the caller. rustc inlines such a body.
    if !can_unwind(db, callee) && unwinds_out(body) {
        return None;
    }
    let runtime = body.basic_blocks.iter().all(|(_, block)| {
        matches!(
            block.terminator.as_ref().map(|it| &it.kind),
            Some(
                TerminatorKind::Goto { .. }
                    | TerminatorKind::SwitchInt { .. }
                    | TerminatorKind::UnwindResume
                    | TerminatorKind::Abort
                    | TerminatorKind::Return
                    | TerminatorKind::Unreachable
                    | TerminatorKind::Drop { .. }
                    | TerminatorKind::Call { .. }
                    | TerminatorKind::Assert { .. }
            )
        )
    });
    runtime.then_some(())
}

fn has_asm(body: &MirBody<'_>) -> bool {
    body.basic_blocks.iter().any(|(_, block)| {
        block.statements.iter().any(|it| matches!(it.kind, StatementKind::InlineAsm(_)))
    })
}

/// rustc's check that the call's signature is the callee's, after both are normalized:
/// `sub_types` of the return type and of each argument. Lifetimes make no difference to the
/// code, and are erased on both sides.
fn check_signature<'db>(
    ctx: &Ctx<'_, 'db>,
    site: &CallSite<'db>,
    callee: &MirBody<'db>,
    arg_tys: &[Ty<'db>],
    destination_ty: Ty<'db>,
) -> Option<()> {
    let interner = ctx.infcx.interner;
    let erase = |ty: Ty<'db>| {
        rustc_type_ir::fold_regions(interner, ty, |_, _| Region::new_erased(interner))
    };
    let same = |a: Ty<'db>, b: Ty<'db>| erase(a) == erase(b);
    if !same(callee.locals[return_slot()].ty.as_ref(), destination_ty) {
        return None;
    }
    let params = &callee.param_locals;
    let param = |at: usize| callee.locals[params[at]].ty.as_ref();
    if site.spread {
        let [receiver, tuple] = arg_tys else { return None };
        let TyKind::Tuple(fields) = tuple.kind() else { return None };
        if params.len() != 1 + fields.len() || !same(param(0), *receiver) {
            return None;
        }
        fields.iter().enumerate().all(|(at, field)| same(param(at + 1), field)).then_some(())
    } else {
        if params.len() != arg_tys.len() {
            return None;
        }
        arg_tys.iter().enumerate().all(|(at, &ty)| same(param(at), ty)).then_some(())
    }
}

/// The optimized body of `body` with `options`, if it has one.
fn optimized<'db>(
    db: &'db Db,
    body: InlinedBody<'db>,
    options: MirOptions,
) -> Option<&'db MirBody<'db>> {
    match body {
        InlinedBody::Fn(func) => db.optimized_mir_body(func.into(), options),
        InlinedBody::Closure(closure) => db.optimized_mir_body_for_closure(closure, options),
    }
    .ok()
}

/// The options the code generator optimizes the bodies of the crate `callee` with when it
/// compiles one of the crate `caller` with `options`, as `Build::options` gives them
/// (`hir-ty-codegen/v2/build.rs`): the standard library's are its own, at level 2 without UB
/// checks; a crate of the build has the caller's, its `debug_assertions` deciding its UB
/// checks.
fn callee_options(
    db: &Db,
    caller: Crate,
    callee: Crate,
    options: MirOptions,
) -> MirOptions {
    if callee == caller {
        return options;
    }
    if callee.data(db).origin.is_lang() {
        let level = MirOptLevel::Release;
        return MirOptions { level, ub_checks: false, inline: true, ..options };
    }
    let debug_assertions =
        callee.cfg_options(db).check_atom(&cfg::CfgAtom::Flag(intern::sym::debug_assertions));
    MirOptions { ub_checks: debug_assertions, ..options }
}

/// rustc's `cross_crate_inlinable` of a function whose bodies are optimized with `options`:
/// whether other crates may build its body into theirs. rustc infers it for a small leaf
/// function only where its inliner runs and the build is not incremental, which `inline`
/// says.
#[blaze_query::tracked(returns(copy))]
fn cross_crate_inlinable(db: &Db, func: FunctionId, options: MirOptions) -> bool {
    let attrs = Attrs::of_fn(db, func);
    if attrs.extern_indicator() {
        return false;
    }
    if attrs.has(AttrFlags::RUSTC_INTRINSIC) {
        return true;
    }
    if attrs.has(AttrFlags::RUSTC_COMPTIME) {
        return false;
    }
    match attrs.inline {
        InlineAttr::Never => return false,
        InlineAttr::Hint | InlineAttr::Always | InlineAttr::Force => return true,
        InlineAttr::None => {}
    }
    // Code generators may not support the float types in calls, and inlining avoids them.
    let sig = db.callable_item_signature(func.into()).instantiate_identity().skip_norm_wip();
    let floats = sig.skip_binder().inputs_and_output.iter().any(|ty| {
        matches!(ty.kind(), TyKind::Float(FloatTy::F16 | FloatTy::F128))
    });
    if floats {
        return true;
    }
    if !options.inline {
        return false;
    }
    let Ok(body) = db.optimized_mir_body(func.into(), options) else { return false };
    blaze_mem::frame::task(blaze_mem::Tag::Mir, |frame| {
        let ctx = Ctx::new(db, func.into(), frame);
        cost::is_small_leaf(body, written_checker(db, body), |place| {
            place.as_ref().ty(body, &ctx.infcx, ctx.env).ty.is_trivially_pure_clone_copy()
        })
    })
}

/// [`written_checked_at`] of `body`, its own setting read once.
fn written_checker<'b, 'db>(
    db: &'db Db,
    body: &'b MirBody<'db>,
) -> impl FnMut(MirSpan) -> bool + use<'b, 'db> {
    let own = overflow::body_written_checked(db, body);
    move |span| match span {
        MirSpan::Inlined(_) => written_checked_at(db, body, span),
        _ => own,
    }
}

/// rustc's `body_is_forwarder`: the body calls a function and returns what it returns, with
/// little else.
fn body_is_forwarder(body: &MirBody<'_>) -> bool {
    let kind =
        |block: BasicBlockId| body.basic_blocks[block].terminator.as_ref().map(|it| &it.kind);
    let Some(TerminatorKind::Call { target, .. }) = kind(body.start_block) else { return false };
    if let Some(target) = *target
        && !matches!(kind(target), Some(TerminatorKind::Return))
    {
        return false;
    }
    let polymorphic = body.locals.iter().any(|(_, it)| it.ty.as_ref().has_non_region_param());
    let max_blocks = if !polymorphic {
        2
    } else if target.is_none() {
        3
    } else {
        4
    };
    if body.basic_blocks.len() > max_blocks {
        return false;
    }
    body.basic_blocks.iter().all(|(id, block)| {
        id == body.start_block
            || matches!(
                block.terminator.as_ref().map(|it| &it.kind),
                Some(
                    TerminatorKind::Return
                        | TerminatorKind::Drop { .. }
                        | TerminatorKind::UnwindResume
                        | TerminatorKind::Abort
                )
            )
    })
}

/// The arguments name a type or a constant: rustc's `non_erasable_generics`.
pub(super) fn is_generic(args: GenericArgs<'_>) -> bool {
    args.types().next().is_some() || args.consts().next().is_some()
}

/// The function `func` names and its generic arguments, of a call by name.
pub(super) fn fn_def<'db>(
    body: &MirBody<'db>,
    func: &Operand,
) -> Option<(FunctionId, GenericArgs<'db>)> {
    let ty = match &func.kind {
        OperandKind::Constant { ty, .. } => ty.as_ref(),
        OperandKind::Allocation { allocation } => allocation.as_ref().ty,
        OperandKind::Copy(place) | OperandKind::Move(place) if place.projection.is_empty() => {
            body.locals[place.local].ty.as_ref()
        }
        _ => return None,
    };
    match ty.kind() {
        TyKind::FnDef(def, args) => match def.0 {
            CallableDefId::FunctionId(func) => Some((func, args.skip_binder())),
            _ => None,
        },
        _ => None,
    }
}

/// The body a call of `func` with `args` runs, with its generic arguments, where the caller's
/// typing environment `env` decides it: rustc's `Instance::try_resolve` for what it resolves
/// to an item with MIR. A virtual call, an intrinsic, a shim and a call whose impl is not
/// known resolve to nothing.
pub(super) fn resolve<'db>(
    ctx: &Ctx<'_, 'db>,
    env: ParamEnvAndCrate<'db>,
    func: FunctionId,
    args: GenericArgs<'db>,
) -> Option<(InlinedBody<'db>, GenericArgs<'db>)> {
    let db = ctx.db;
    if FunctionSignature::is_intrinsic(db, func) {
        return None;
    }
    let interner = ctx.infcx.interner;
    let lang = interner.lang_items();
    // `drop_glue`'s body is a placeholder the compiler replaces with the type's drop glue, a
    // shim: inlined, it would drop nothing.
    if [lang.DropInPlace, lang.DropGlue].contains(&Some(func)) {
        return None;
    }
    let ItemContainerId::TraitId(_) = func.loc(db).container else {
        return FunctionSignature::of(db, func).has_body().then_some((InlinedBody::Fn(func), args));
    };
    let called_as = if Some(func) == lang.Fn_call {
        Some(ClosureKind::Fn)
    } else if Some(func) == lang.FnMut_call_mut {
        Some(ClosureKind::FnMut)
    } else if Some(func) == lang.FnOnce_call_once {
        Some(ClosureKind::FnOnce)
    } else {
        None
    };
    if let Some(called_as) = called_as {
        // A closure called as what it is runs its body; as less, or anything else called
        // through the traits, a shim.
        let self_ty = args.as_slice().first()?.ty()?;
        let TyKind::Closure(closure, closure_args) = self_ty.kind() else { return None };
        let closure_args = closure_args.as_closure();
        if closure_args.kind_ty().to_opt_closure_kind() != Some(called_as) {
            return None;
        }
        let parent = GenericArgs::new_from_slice(closure_args.parent_args());
        return Some((InlinedBody::Closure(closure.0), parent));
    }
    if is_dyn_method(interner, env.param_env, func, args).is_some() {
        return None;
    }
    let (Either::Left(implemented), impl_args) = db.lookup_impl_method(env, func, args) else {
        return None;
    };
    let unsettled = TypeFlags::HAS_PARAM | TypeFlags::HAS_TY_INFER | TypeFlags::HAS_CT_INFER;
    if implemented == func {
        // No impl was found: the trait's default body is the callee only where the arguments
        // leave no other impl to choose and the self type is one an impl is for.
        let self_ty = args.as_slice().first()?.ty()?;
        let concrete = !args.has_type_flags(unsettled)
            && !matches!(
                self_ty.kind(),
                TyKind::Dynamic(..) | TyKind::Param(_) | TyKind::Alias(..) | TyKind::Infer(_)
            );
        return (concrete && FunctionSignature::of(db, func).has_body())
            .then_some((InlinedBody::Fn(func), args));
    }
    // A `default fn` a more specialized impl may still override for some arguments.
    if FunctionSignature::of(db, implemented).is_default() && impl_args.has_type_flags(unsettled) {
        return None;
    }
    FunctionSignature::of(db, implemented)
        .has_body()
        .then_some((InlinedBody::Fn(implemented), impl_args))
}

/// The type of `operand` in `body`, normalized with its lifetimes erased.
pub(super) fn operand_ty<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &MirBody<'db>,
    operand: &Operand,
) -> Ty<'db> {
    normalize_ty(ctx, ctx.operand_ty(body, operand))
}

/// `value`, of the body `args` instantiate, in the terms of the body `args` are of: `None`
/// where `args` lacks a parameter `value` names or an alias does not normalize.
pub(super) fn instantiate<'db>(
    ctx: &Ctx<'_, 'db>,
    value: GenericArgs<'db>,
    args: GenericArgs<'db>,
) -> Option<GenericArgs<'db>> {
    fold(ctx, value, Some(args))
}

fn normalize_ty<'db>(ctx: &Ctx<'_, 'db>, ty: Ty<'db>) -> Ty<'db> {
    ty.fold_with(&mut Instantiate { ctx, args: None, missing: false })
}

fn fold<'db, T: TypeFoldable<DbInterner<'db>>>(
    ctx: &Ctx<'_, 'db>,
    value: T,
    args: Option<GenericArgs<'db>>,
) -> Option<T> {
    let mut folder = Instantiate { ctx, args, missing: false };
    let value = value.fold_with(&mut folder);
    (!folder.missing && !references_non_lt_error(&value)).then_some(value)
}

/// Replaces the generic parameters by `args` where there are some, normalizes each alias
/// where `ctx` can and erases the lifetimes not bound inside.
struct Instantiate<'a, 'f, 'db> {
    ctx: &'a Ctx<'f, 'db>,
    args: Option<GenericArgs<'db>>,
    missing: bool,
}

impl<'db> TypeFolder<DbInterner<'db>> for Instantiate<'_, '_, 'db> {
    fn cx(&self) -> DbInterner<'db> {
        self.ctx.infcx.interner
    }

    fn fold_ty(&mut self, ty: Ty<'db>) -> Ty<'db> {
        let flags = TypeFlags::HAS_PARAM | TypeFlags::HAS_ALIAS | TypeFlags::HAS_FREE_REGIONS;
        if !ty.has_type_flags(flags) {
            return ty;
        }
        match ty.kind() {
            TyKind::Param(param) => {
                let Some(args) = self.args else { return ty };
                match args.as_slice().get(param.index as usize).and_then(|it| it.ty()) {
                    Some(ty) => ty,
                    None => {
                        self.missing = true;
                        ty
                    }
                }
            }
            TyKind::Alias(..) => {
                let ty = ty.super_fold_with(self);
                if ty.has_escaping_bound_vars() { ty } else { self.ctx.normalize(ty) }
            }
            _ => ty.super_fold_with(self),
        }
    }

    fn fold_region(&mut self, region: Region<'db>) -> Region<'db> {
        match region.kind() {
            RegionKind::ReBound(..) => region,
            _ => Region::new_erased(self.cx()),
        }
    }

    fn fold_const(&mut self, konst: Const<'db>) -> Const<'db> {
        if let ConstKind::Param(param) = konst.kind()
            && let Some(args) = self.args
        {
            return match args.as_slice().get(param.index as usize).and_then(|it| it.konst()) {
                Some(konst) => konst,
                None => {
                    self.missing = true;
                    konst
                }
            };
        }
        konst.super_fold_with(self)
    }
}
