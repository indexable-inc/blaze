//! What a call may do that iterations running at once could see: a per-instance effect
//! summary, bottom-up, as tracked queries.
//!
//! [`effects`] answers for one instance of a function (its generic arguments given), over its
//! monomorphized MIR, and [`closure_effects`] for one of a closure. A summary says which
//! effects the instance has ([`Effect`]), which memory of its parameters it may write, where
//! the pointers it returns may come from, and the first thing on the way that rules a loop out,
//! to name in a message ([`Why`]). [`cost`] and [`closure_cost`] count the work a call runs.
//!
//! Memory is followed by where pointers point ([`Origin`]), flow-insensitively, on two levels
//! for each parameter: the memory the parameter's own pointers point at (what `&mut self`
//! names: an iterator's fields), and all memory reachable from there through more pointers
//! (what the iterator's pointers point at: the slice). So `slice::Iter::next` writes its
//! parameter's memory on the first level only, and a caller passing `&mut iter` sees a write to
//! its own local, not to the slice. A write through a pointer that may come from nowhere a
//! parameter reaches, nor from memory the call allocated, is [`Effect::RawWrite`]; one into a
//! static, [`Effect::Global`]. What a pointer stored through a pointer is, is kept for all
//! memory at once ([`Frame::stored`]), so a pointer loaded back through any pointer may be it.
//!
//! Roots: a function in an `extern` block is [`Effect::Foreign`] (I/O, the OS, C), but the
//! allocator's (`#[rustc_allocator]` and its family), which only gives new memory or takes it
//! back; an atomic intrinsic is [`Effect::Atomic`]; reading a static that changes (`static
//! mut`, one with interior mutability, a thread-local, a foreign one) is [`Effect::Global`];
//! inline assembly is [`Effect::Asm`]; a call through `dyn`, a function pointer, a trait
//! method for a type not known, an intrinsic blaze has no rule for or a body that doesn't lower
//! is [`Effect::Unknown`]. A function that returns `!` panics, and is not looked into: what it
//! prints is no result. Dropping a value runs its `Drop` impls, the fields' included. Cycles of
//! calls start with no effect and are iterated to their fixpoint; summaries only grow, so they
//! get there.

use base_db::Db;
use hir_def::{
    AdtId, BuiltinDeriveImplId, CallableDefId, FunctionId, ItemContainerId, StaticId,
    attrs::AttrFlags, builtin_derive::BuiltinDeriveImplMethod, lang_item::lang_items,
    signatures::{FunctionSignature, StaticFlags, StaticSignature},
};
use hir_ty::{
    InferBodyId, ParamEnvAndCrate,
    db::{HirDatabase as _, InternedClosureId},
    drop::{DropGlue, destructor, has_drop_glue, is_freeze},
    intrinsic::Intrinsic,
    mir::{CastKind, MirBody, Operand, OperandKind, ProjectionElem, Rvalue, StatementKind},
    mir::{BinOp, StoredPlace, TerminatorKind},
    next_solver::{
        DbInterner, GenericArg, GenericArgs, ParamEnv, StoredGenericArgs, Ty, TyKind, TypingMode,
        infer::{DbInternerInferExt as _, InferCtxt},
    },
    traits::StoredParamEnvAndCrate,
};
use rustc_type_ir::inherent::{AdtDef as _, GenericArgs as _, IntoKind as _};

use super::super::alloc::{self, Resolved};
use super::region::Cycles;

/// The most type nodes an instance's arguments may have: past it a function instantiates
/// itself with ever larger arguments (polymorphic recursion), which rustc refuses at its
/// `type_length_limit`; such an instance is [`Effect::Unknown`].
const LONGEST: usize = 256;

/// How deep a type's drop glue is followed into its fields.
const DROP_DEPTH: u32 = 8;

/// The parameters an [`Origin`] names one by one; a later one shares the last bit.
const PARAMS: u32 = 30;
/// The memory each parameter's own pointers point at: bit `i` for parameter `i`.
pub(crate) const DIRECT: u64 = (1 << PARAMS) - 1;
/// The memory reachable from there: bit `30 + i` for parameter `i`.
pub(crate) const DEEP: u64 = DIRECT << PARAMS;
/// A static's memory.
pub(crate) const STATIC: u64 = 1 << 60;
/// Memory the call allocated, or its own frame: no caller sees it before it is returned.
pub(crate) const FRESH: u64 = 1 << 61;
/// Memory blaze can't trace: an address made from a number, what an unknown call returns.
pub(crate) const UNKNOWN: u64 = 1 << 63;

/// Where the pointers of a value may point: bits of [`DIRECT`] and [`DEEP`] for parameters'
/// memory, and [`STATIC`], [`FRESH`], [`UNKNOWN`].
pub(crate) type Origin = u64;

pub(crate) fn direct(index: usize) -> Origin {
    1 << (index as u32).min(PARAMS - 1)
}

pub(crate) fn deep(index: usize) -> Origin {
    direct(index) << PARAMS
}

/// What a call may do that the order of iterations shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub(crate) enum Effect {
    /// Calls a foreign function: I/O, the OS, C.
    Foreign,
    /// Reads or writes state all threads share: a `static mut`, a static with interior
    /// mutability, a thread-local.
    Global,
    /// An atomic operation, which other threads see in some order.
    Atomic,
    /// Inline assembly.
    Asm,
    /// Writes through a pointer blaze can't trace to an argument or to new memory.
    RawWrite,
    /// Calls what blaze can't see into.
    Unknown,
    /// May panic. Not an effect two orders of independent iterations differ by: in either
    /// order the loop panics if an iteration does.
    Panic,
}

impl Effect {
    const fn bit(self) -> u8 {
        1 << self as u8
    }
}

/// The effects that rule a loop out; [`Effect::Panic`] does not.
const RULED_OUT: u8 = Effect::Foreign.bit()
    | Effect::Global.bit()
    | Effect::Atomic.bit()
    | Effect::Asm.bit()
    | Effect::RawWrite.bit()
    | Effect::Unknown.bit();

/// What had the first effect that rules a loop out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Culprit {
    /// A function of an `extern` block.
    Foreign(FunctionId),
    /// A static that changes.
    Static(StaticId),
    /// A write into a static's memory through a pointer.
    StaticWrite,
    /// An atomic intrinsic.
    Atomic(FunctionId),
    /// An intrinsic blaze has no rule for.
    Intrinsic(FunctionId),
    /// A trait's method called on `dyn Trait`.
    Dyn(FunctionId),
    /// A trait's method for a type not known (a type parameter of the body linted).
    Generic(FunctionId),
    /// A function whose MIR blaze can't build, or one past [`LONGEST`].
    Opaque(FunctionId),
    /// A method a built-in derive made that goes through what its caller gives (`fmt`, `hash`).
    Derived(FunctionId),
    /// A closure whose MIR blaze can't build.
    OpaqueClosure,
    /// A call of a function pointer.
    FnPointer,
    /// Inline assembly.
    Asm,
    /// A write through a pointer from nowhere blaze can trace.
    RawPointer,
    /// The drop of a value whose type is not known (`dyn`, a type parameter).
    UnknownDrop,
    /// A coroutine's `yield`.
    Yield,
}

/// What had the first effect that rules a loop out, and the function called on the way to it
/// from the body summarized, if it is not that body's own.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Why {
    pub(crate) culprit: Culprit,
    pub(crate) through: Option<FunctionId>,
}

impl Why {
    /// What it does, said of the loop: "calls `write`, a foreign function", "calls `log`,
    /// which uses the static `N`, which every thread shares and something changes".
    pub(crate) fn said(&self, db: &Db) -> String {
        let name = |f: FunctionId| FunctionSignature::of(db, f).name.as_str().to_owned();
        let what = match self.culprit {
            Culprit::Foreign(f) => format!("calls `{}`, a foreign function (I/O, the OS)", name(f)),
            Culprit::Static(id) => format!(
                "uses the static `{}`, which every thread shares and something changes",
                StaticSignature::of(db, id).name.as_str()
            ),
            Culprit::StaticWrite => "writes into a static".to_owned(),
            Culprit::Atomic(f) => format!("does an atomic operation (`{}`)", name(f)),
            Culprit::Intrinsic(f) => {
                format!("calls the intrinsic `{}`, whose effects blaze has no rule for", name(f))
            }
            Culprit::Dyn(f) => format!("calls `{}` through `dyn`, whose impl is not known", name(f)),
            Culprit::Generic(f) => {
                format!("calls `{}` for a type parameter, whose impl is not known", name(f))
            }
            Culprit::Opaque(f) => format!("calls `{}`, whose MIR blaze can't read", name(f)),
            Culprit::Derived(f) => {
                format!("calls a derived `{}`, which goes through what its caller gives", name(f))
            }
            Culprit::OpaqueClosure => "calls a closure whose MIR blaze can't read".to_owned(),
            Culprit::FnPointer => "calls a function pointer, whose target is not known".to_owned(),
            Culprit::Asm => "runs inline assembly".to_owned(),
            Culprit::RawPointer => {
                "writes through a raw pointer blaze can't trace to its arguments".to_owned()
            }
            Culprit::UnknownDrop => {
                "drops a value whose type is not known, so neither is its destructor".to_owned()
            }
            Culprit::Yield => "yields, as a coroutine".to_owned(),
        };
        match self.through {
            Some(f) => format!("calls `{}`, which {}", name(f), what),
            None => what,
        }
    }
}

/// What one instance does, as its callers see it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub(crate) struct Effects {
    /// Bit `e as u8` for each [`Effect`] it may have.
    pub(crate) kinds: u8,
    /// The memory of its parameters it may write: bits of [`DIRECT`] and [`DEEP`].
    pub(crate) writes: u64,
    /// Where the pointers it returns may come from ([`Origin`]).
    pub(crate) returns: u64,
    /// The first effect it has that rules a loop out.
    pub(crate) why: Option<Why>,
}

impl Effects {
    pub(crate) const NONE: Effects = Effects { kinds: 0, writes: 0, returns: 0, why: None };

    pub(crate) fn unknown(culprit: Culprit) -> Effects {
        Effects {
            kinds: Effect::Unknown.bit(),
            writes: 0,
            returns: UNKNOWN,
            why: Some(Why { culprit, through: None }),
        }
    }

    pub(crate) fn has(&self, effect: Effect) -> bool {
        self.kinds & effect.bit() != 0
    }

    /// Whether nothing it does rules a loop out.
    pub(crate) fn allowed(&self) -> bool {
        self.kinds & RULED_OUT == 0
    }

    /// Whether what it does that rules a loop out is all known, and seen by other iterations
    /// only in the order they run: I/O, a static that changes, an atomic. Not what blaze
    /// can't see into, a raw write it can't trace, or assembly.
    pub(crate) fn only_ordered(&self) -> bool {
        self.kinds & (Effect::Unknown.bit() | Effect::RawWrite.bit() | Effect::Asm.bit()) == 0
    }

    fn add(&mut self, effect: Effect, culprit: Culprit) {
        self.kinds |= effect.bit();
        if effect.bit() & RULED_OUT != 0 && self.why.is_none() {
            self.why = Some(Why { culprit, through: None });
        }
    }

    /// Takes in the kinds of a callee, called as `through`.
    fn add_callee(&mut self, callee: &Effects, through: Option<FunctionId>) {
        self.kinds |= callee.kinds;
        if self.why.is_none()
            && let Some(why) = callee.why
        {
            self.why = Some(Why { through: why.through.or(through), ..why });
        }
    }
}

/// What the instance `f` with `args`, in `env`, does.
#[blaze_query::tracked(returns(copy), cycle_initial = |_, _, _, _, _| Effects::NONE)]
pub(crate) fn effects(
    db: &Db,
    env: StoredParamEnvAndCrate,
    f: FunctionId,
    args: StoredGenericArgs,
) -> Effects {
    // clone: two handles of interned values, for the body query's key
    match db.monomorphized_mir_body(InferBodyId::from(f), args.clone(), env.clone()) {
        Ok(body) => Frame::new(db, env.as_ref(db), body).summarize(),
        Err(_) => Effects::unknown(Culprit::Opaque(f)),
    }
}

/// What a closure does when called, its parent's arguments `parent` given. Its parameter 0 is
/// the closure itself, by reference or by value, through which it reaches what it captures.
#[blaze_query::tracked(returns(copy), cycle_initial = |_, _, _, _, _| Effects::NONE)]
pub(crate) fn closure_effects<'db>(
    db: &'db Db,
    env: StoredParamEnvAndCrate,
    closure: InternedClosureId<'db>,
    parent: StoredGenericArgs,
) -> Effects {
    // clone: a handle of an interned value, for the body query's key
    match db.monomorphized_mir_body_for_closure(closure, parent, env.clone()) {
        Ok(body) => Frame::new(db, env.as_ref(db), body).summarize(),
        Err(_) => Effects::unknown(Culprit::OpaqueClosure),
    }
}

/// What a call of a recursive function counts as: its depth is not known.
const RECURSIVE_COST: u32 = 1 << 12;

/// How much a block in a loop counts for: each loop is taken to run this many times.
pub(crate) const LOOP_WEIGHT: u64 = 16;

/// The work a call of the instance runs, in MIR statements and terminators, its callees'
/// included; a block in a loop counts [`LOOP_WEIGHT`] times. A call that recurses counts
/// [`RECURSIVE_COST`], with no fixpoint to find.
#[blaze_query::tracked(returns(copy), cycle_result = recursive_cost)]
pub(crate) fn cost(
    db: &Db,
    env: StoredParamEnvAndCrate,
    f: FunctionId,
    args: StoredGenericArgs,
) -> u32 {
    // clone: a handle of an interned value, for the body query's key
    match db.monomorphized_mir_body(InferBodyId::from(f), args, env.clone()) {
        Ok(body) => body_cost(db, env.as_ref(db), body),
        Err(_) => RECURSIVE_COST,
    }
}

fn recursive_cost(
    _db: &Db,
    _: blaze_query::db::Id,
    _env: StoredParamEnvAndCrate,
    _f: FunctionId,
    _args: StoredGenericArgs,
) -> u32 {
    RECURSIVE_COST
}

fn recursive_closure_cost<'db>(
    _db: &'db Db,
    _: blaze_query::db::Id,
    _env: StoredParamEnvAndCrate,
    _closure: InternedClosureId<'db>,
    _parent: StoredGenericArgs,
) -> u32 {
    RECURSIVE_COST
}

/// [`cost`] of a closure.
#[blaze_query::tracked(returns(copy), cycle_result = recursive_closure_cost)]
pub(crate) fn closure_cost<'db>(
    db: &'db Db,
    env: StoredParamEnvAndCrate,
    closure: InternedClosureId<'db>,
    parent: StoredGenericArgs,
) -> u32 {
    // clone: a handle of an interned value, for the body query's key
    match db.monomorphized_mir_body_for_closure(closure, parent, env.clone()) {
        Ok(body) => body_cost(db, env.as_ref(db), body),
        Err(_) => RECURSIVE_COST,
    }
}

/// The work of one run of `body`, the blocks in its loops weighted.
fn body_cost<'db>(db: &'db Db, env: ParamEnvAndCrate<'db>, body: &MirBody<'db>) -> u32 {
    let cycles = Cycles::of(body, |_| true);
    let total = body.basic_blocks.iter().filter(|(_, it)| !it.is_cleanup).fold(
        0u64,
        |total, (id, block)| {
            let weight = if cycles.contains(id) { LOOP_WEIGHT } else { 1 };
            total.saturating_add(block_cost(db, env, block).saturating_mul(weight))
        },
    );
    total.min(u64::from(u32::MAX)) as u32
}

/// The work of one run of `block`: its statements, its terminator and what that calls.
pub(crate) fn block_cost<'db>(
    db: &'db Db,
    env: ParamEnvAndCrate<'db>,
    block: &hir_ty::mir::BasicBlock,
) -> u64 {
    let mut cost = block.statements.len() as u64 + 1;
    if let Some(terminator) = &block.terminator
        && let TerminatorKind::Call { func, .. } = &terminator.kind
    {
        cost += u64::from(call_cost(db, env, func));
    }
    cost
}

/// The work a call of `func` runs past the call itself.
pub(crate) fn call_cost<'db>(db: &'db Db, env: ParamEnvAndCrate<'db>, func: &Operand) -> u32 {
    let Some((f, args)) = func.fn_item() else { return 1 };
    if FunctionSignature::is_intrinsic(db, f) || is_foreign(db, f) || diverges(db, f) {
        return 1;
    }
    match callee(db, env, f, args) {
        Callee::Function(g, args) if FunctionSignature::of(db, g).has_body() && short(args) => {
            cost(db, env.store(), g, args.store())
        }
        Callee::Closure(id, parent) => closure_cost(db, env.store(), id, parent.store()),
        _ => 1,
    }
}

fn short(args: GenericArgs<'_>) -> bool {
    args.types().map(|it| it.walk().count()).sum::<usize>() <= LONGEST
}

pub(crate) fn is_foreign(db: &Db, f: FunctionId) -> bool {
    matches!(f.loc(db).container, ItemContainerId::ExternBlockId(_))
}

pub(crate) fn diverges(db: &Db, f: FunctionId) -> bool {
    let output = db.callable_item_signature(f.into()).skip_binder().skip_binder().output();
    matches!(output.kind(), TyKind::Never)
}

/// A function of the allocator: it gives new memory or takes it back, and no thread sees
/// another's.
fn is_allocator(db: &Db, f: FunctionId) -> bool {
    let flags = AttrFlags::query(db, f.into());
    if flags.intersects(
        AttrFlags::RUSTC_ALLOCATOR
            | AttrFlags::RUSTC_ALLOCATOR_ZEROED
            | AttrFlags::RUSTC_REALLOCATOR
            | AttrFlags::RUSTC_DEALLOCATOR,
    ) {
        return true;
    }
    if !is_foreign(db, f) {
        return false;
    }
    let name = AttrFlags::link_name(db, f.into())
        .unwrap_or_else(|| FunctionSignature::of(db, f).name.symbol().clone());
    matches!(
        name.as_str(),
        "__rust_alloc" | "__rust_alloc_zeroed" | "__rust_realloc" | "__rust_dealloc"
    )
}

/// What a call of `f` with `args` runs.
pub(crate) enum Callee<'db> {
    /// A function with a body, or a trait's method no impl was found for.
    Function(FunctionId, GenericArgs<'db>),
    /// A closure called through `Fn*`: its id and its parent's arguments.
    Closure(InternedClosureId<'db>, GenericArgs<'db>),
    /// A method a built-in derive made, which has no body.
    Derived(BuiltinDeriveImplId, BuiltinDeriveImplMethod, GenericArgs<'db>),
    /// A trait's method called on `dyn Trait`.
    Dyn(FunctionId),
    /// A function pointer called through `Fn*`.
    FnPointer,
}

/// Whether `f` is the method of `Fn`, `FnMut` or `FnOnce`, whose parameters after the first
/// come in one tuple.
pub(crate) fn is_fn_trait_method(db: &Db, krate: base_db::Crate, f: FunctionId) -> bool {
    let ItemContainerId::TraitId(trait_) = f.loc(db).container else { return false };
    let lang = lang_items(db, krate);
    [lang.Fn, lang.FnMut, lang.FnOnce].contains(&Some(trait_))
}

/// What a call of `f` with `args` runs, as far as `env` knows: the impl's method for a trait's,
/// the closure or function `Self` is for `Fn*`.
pub(crate) fn callee<'db>(
    db: &'db Db,
    env: ParamEnvAndCrate<'db>,
    f: FunctionId,
    args: GenericArgs<'db>,
) -> Callee<'db> {
    if is_fn_trait_method(db, env.krate, f) {
        return fn_trait_callee(db, f, args, args.type_at(0));
    }
    match alloc::resolve(db, env, f, args) {
        Resolved::Derived(impl_, method, args) => Callee::Derived(impl_, method, args),
        Resolved::Function(g, args) => {
            if alloc::in_trait(db, g) && args.len() > 0 && is_dyn(args.type_at(0)) {
                return Callee::Dyn(g);
            }
            Callee::Function(g, args)
        }
    }
}

/// What `Fn*::call*` runs for `Self` = `self_ty`.
fn fn_trait_callee<'db>(
    db: &'db Db,
    f: FunctionId,
    args: GenericArgs<'db>,
    self_ty: Ty<'db>,
) -> Callee<'db> {
    match self_ty.kind() {
        TyKind::Closure(id, closure_args) => Callee::Closure(
            id.0,
            GenericArgs::new_from_slice(closure_args.as_closure().parent_args()),
        ),
        TyKind::FnDef(def, fn_args) => match def.0 {
            CallableDefId::FunctionId(g) => Callee::Function(g, fn_args.skip_binder()),
            _ => Callee::Function(f, args),
        },
        TyKind::FnPtr(..) => Callee::FnPointer,
        TyKind::Dynamic(..) => Callee::Dyn(f),
        TyKind::Ref(_, inner, _) => fn_trait_callee(db, f, args, inner),
        _ => Callee::Function(f, args),
    }
}

fn is_dyn(ty: Ty<'_>) -> bool {
    match ty.kind() {
        TyKind::Dynamic(..) => true,
        TyKind::Ref(_, inner, _) => is_dyn(inner),
        _ => false,
    }
}

/// What a call of `func` runs, summarized, and whether it is called through `Fn*` (its
/// parameters after the first in one tuple, the call's second argument), as a caller in `env`
/// sees it. A call of what is not a function item is a function pointer's.
pub(crate) fn call_summary<'db>(
    db: &'db Db,
    env: ParamEnvAndCrate<'db>,
    func: &Operand,
    intrinsic: Option<Intrinsic>,
) -> (Effects, bool) {
    let Some((f, args)) = func.fn_item() else {
        return (Effects::unknown(Culprit::FnPointer), false);
    };
    if let Some(intrinsic) = intrinsic {
        return (intrinsic_effects(db, env, f, args, intrinsic), false);
    }
    if FunctionSignature::is_intrinsic(db, f) {
        return (Effects::unknown(Culprit::Intrinsic(f)), false);
    }
    if lang_items(db, env.krate).DropInPlace == Some(f) {
        let mut out = Effects::NONE;
        drop_effects(db, env, args.type_at(0), &mut out, 0);
        // `drop_in_place(p)` drops what `p` points at: its memory and what it owns.
        out.writes |= direct(0) | deep(0);
        return (out, false);
    }
    if is_allocator(db, f) {
        // Freeing or growing memory changes what its first argument points at.
        let name = FunctionSignature::of(db, f).name.symbol().clone();
        let writes = match name.as_str() {
            "__rust_alloc" | "__rust_alloc_zeroed" => 0,
            _ => direct(0),
        };
        return (Effects { kinds: 0, writes, returns: FRESH, why: None }, false);
    }
    if is_foreign(db, f) {
        let mut out = Effects { returns: UNKNOWN, ..Effects::NONE };
        out.add(Effect::Foreign, Culprit::Foreign(f));
        return (out, false);
    }
    if diverges(db, f) {
        return (Effects { kinds: Effect::Panic.bit(), ..Effects::NONE }, false);
    }
    let fn_call = is_fn_trait_method(db, env.krate, f);
    let summary = match callee(db, env, f, args) {
        Callee::Closure(id, parent) => closure_effects(db, env.store(), id, parent.store()),
        Callee::Function(g, gargs) => {
            if alloc::in_trait(db, g) && !FunctionSignature::of(db, g).has_body() {
                Effects::unknown(Culprit::Generic(g))
            } else if !short(gargs) {
                Effects::unknown(Culprit::Opaque(g))
            } else {
                let mut summary = effects(db, env.store(), g, gargs.store());
                if summary.why.is_some_and(|it| it.through.is_none()) && g != f {
                    summary.why = summary.why.map(|it| Why { through: Some(g), ..it });
                }
                summary
            }
        }
        Callee::Derived(impl_, method, args) => derived_effects(db, env, impl_, method, args),
        Callee::Dyn(g) => Effects::unknown(Culprit::Dyn(g)),
        Callee::FnPointer => Effects::unknown(Culprit::FnPointer),
    };
    (summary, fn_call)
}

/// What an intrinsic does: arithmetic and pointer math nothing; a copy or store, a write
/// through its destination argument; an atomic, [`Effect::Atomic`]; a volatile access,
/// [`Effect::Foreign`] (it is device I/O); `const_eval_select`, what its run-time function
/// does; the rest [`Effect::Unknown`].
fn intrinsic_effects<'db>(
    db: &'db Db,
    env: ParamEnvAndCrate<'db>,
    f: FunctionId,
    args: GenericArgs<'db>,
    intrinsic: Intrinsic,
) -> Effects {
    use Intrinsic as I;
    use hir_ty::intrinsic::SimdOp;
    let mut out = Effects::NONE;
    match intrinsic {
        I::Atomic(..) => out.add(Effect::Atomic, Culprit::Atomic(f)),
        I::Copy | I::CopyNonoverlapping => out.writes = direct(1),
        I::WriteBytes | I::WriteViaMove | I::WriteBoxViaMove | I::NontemporalStore => {
            out.writes = direct(0)
        }
        I::TypedSwapNonoverlapping => out.writes = direct(0) | direct(1),
        I::Simd(SimdOp::MaskedStore) | I::Simd(SimdOp::Scatter) => out.writes = direct(1),
        I::VolatileLoad
        | I::VolatileStore
        | I::UnalignedVolatileLoad
        | I::UnalignedVolatileStore
        | I::VolatileCopyMemory
        | I::VolatileCopyNonoverlappingMemory
        | I::VolatileSetMemory => out.add(Effect::Foreign, Culprit::Intrinsic(f)),
        I::AbortImmediate | I::Unreachable => out.kinds |= Effect::Panic.bit(),
        I::ConstEvalSelect => {
            // `const_eval_select(args, const_fn, runtime_fn)` calls `runtime_fn` with the
            // tuple `args`, generic argument 2.
            let runtime = args.types().nth(2);
            match runtime.map(|it| it.kind()) {
                Some(TyKind::FnDef(def, fn_args)) => match (def.0, fn_args.skip_binder()) {
                    (CallableDefId::FunctionId(g), fn_args) if short(fn_args) => {
                        let summary = effects(db, env.store(), g, fn_args.store());
                        out.add_callee(&summary, Some(g));
                        let spread = |bits: u64| match bits & (DIRECT | DEEP) {
                            0 => 0,
                            it if it & DIRECT != 0 => direct(0) | (it & DEEP != 0) as u64 * deep(0),
                            _ => deep(0),
                        };
                        out.writes = spread(summary.writes);
                        out.returns = spread(summary.returns)
                            | summary.returns & (STATIC | FRESH | UNKNOWN);
                    }
                    _ => return Effects::unknown(Culprit::Intrinsic(f)),
                },
                _ => return Effects::unknown(Culprit::Intrinsic(f)),
            }
        }
        I::BoxNew => out.returns = FRESH,
        I::Transmute | I::TransmuteUnchecked => out.returns = direct(0) | UNKNOWN,
        I::ReadViaCopy | I::Offset | I::ArithOffset | I::AggregateRawPtr | I::PtrMask => {
            out.returns = direct(0) | deep(0)
        }
        I::Forget
        | I::AddWithOverflow
        | I::SubWithOverflow
        | I::MulWithOverflow
        | I::WrappingAdd
        | I::WrappingSub
        | I::WrappingMul
        | I::SaturatingAdd
        | I::SaturatingSub
        | I::UncheckedAdd
        | I::UncheckedSub
        | I::UncheckedMul
        | I::UncheckedDiv
        | I::UncheckedRem
        | I::UncheckedShl
        | I::UncheckedShr
        | I::UncheckedFunnelShl
        | I::UncheckedFunnelShr
        | I::ExactDiv
        | I::DisjointBitor
        | I::CarryingMulAdd
        | I::CarrylessMul
        | I::ThreeWayCompare
        | I::RotateLeft
        | I::RotateRight
        | I::Ctpop
        | I::Ctlz
        | I::CtlzNonzero
        | I::Cttz
        | I::CttzNonzero
        | I::Bswap
        | I::Bitreverse
        | I::IntegerMax
        | I::IntegerMin
        | I::CopySign(_)
        | I::Maximum(_)
        | I::Minimum(_)
        | I::MaximumNumberNsz(_)
        | I::MinimumNumberNsz(_)
        | I::Sqrt
        | I::Powi
        | I::Powf
        | I::Sin
        | I::Cos
        | I::Exp
        | I::Exp2
        | I::Log
        | I::Log10
        | I::Log2
        | I::Fma
        | I::Fmuladd
        | I::Fabs
        | I::Floor
        | I::Ceil
        | I::Trunc
        | I::Round
        | I::RoundTiesEven
        | I::FaddFast
        | I::FsubFast
        | I::FmulFast
        | I::FdivFast
        | I::FremFast
        | I::FaddAlgebraic
        | I::FsubAlgebraic
        | I::FmulAlgebraic
        | I::FdivAlgebraic
        | I::FremAlgebraic
        | I::FloatToIntUnchecked
        | I::SizeOf
        | I::SizeOfVal
        | I::AlignOf
        | I::AlignOfVal
        | I::Assume
        | I::ColdPath
        | I::BlackBox
        | I::UbChecks
        | I::OverflowChecks
        | I::NeedsDrop
        | I::TypeId
        | I::TypeIdEq
        | I::TypeName
        | I::VariantCount
        | I::DiscriminantValue
        | I::PtrOffsetFrom
        | I::PtrOffsetFromUnsigned
        | I::PtrGuaranteedCmp
        | I::PtrMetadata
        | I::RawEq
        | I::CompareBytes
        | I::SelectUnpredictable
        | I::AssertInhabited
        | I::AssertZeroValid
        | I::AssertMemUninitializedValid
        | I::CallerLocation
        | I::IsValStaticallyKnown
        | I::PrefetchReadData
        | I::PrefetchWriteData
        | I::PrefetchReadInstruction
        | I::PrefetchWriteInstruction => {}
        I::SliceGetUnchecked => out.returns = direct(0) | deep(0),
        I::Simd(_) => out.returns = direct(0) | deep(0),
        _ => return Effects::unknown(Culprit::Intrinsic(f)),
    }
    out
}

/// A built-in derive's method: `clone`, `default`, `eq`, `ne`, `cmp` and `partial_cmp` call
/// the trait's method on every field; `fmt` and `hash` go through a formatter or hasher the
/// caller gives, which blaze does not follow.
fn derived_effects<'db>(
    db: &'db Db,
    env: ParamEnvAndCrate<'db>,
    impl_: BuiltinDeriveImplId,
    method: BuiltinDeriveImplMethod,
    args: GenericArgs<'db>,
) -> Effects {
    let Some(trait_method) = method.trait_method(db, impl_) else {
        return Effects::unknown(Culprit::OpaqueClosure);
    };
    let two = match method {
        BuiltinDeriveImplMethod::clone | BuiltinDeriveImplMethod::default => false,
        BuiltinDeriveImplMethod::eq
        | BuiltinDeriveImplMethod::cmp
        | BuiltinDeriveImplMethod::partial_cmp => true,
        _ => return Effects::unknown(Culprit::Derived(trait_method)),
    };
    let interner = DbInterner::new_no_crate(db);
    let adt = impl_.loc(db).adt;
    let mut out = Effects::NONE;
    alloc::any_variant(db, adt, |variant| {
        for (_, field) in db.field_types(variant).iter() {
            let ty = field.ty().instantiate(interner, args).skip_norm_wip();
            let field_args = match two {
                true => GenericArgs::new_from_slice(&[GenericArg::from(ty), GenericArg::from(ty)]),
                false => GenericArgs::new_from_slice(&[GenericArg::from(ty)]),
            };
            let func = (trait_method, field_args);
            let summary = match alloc::resolve(db, env, func.0, func.1) {
                Resolved::Function(g, gargs) if FunctionSignature::of(db, g).has_body() => {
                    effects(db, env.store(), g, gargs.store())
                }
                Resolved::Function(g, _) => Effects::unknown(Culprit::Generic(g)),
                Resolved::Derived(impl_, method, args) => {
                    derived_effects(db, env, impl_, method, args)
                }
            };
            out.add_callee(&summary, None);
            out.writes |= summary.writes;
        }
        false
    });
    out.returns = match method {
        BuiltinDeriveImplMethod::clone => FRESH | direct(0) | deep(0),
        _ => 0,
    };
    out
}

/// The effects of the drop glue of `ty`: its `Drop` impl's, then its fields'. What is dropped
/// is the caller's own, so its memory changing is the write `drop_in_place` makes.
fn drop_effects<'db>(
    db: &'db Db,
    env: ParamEnvAndCrate<'db>,
    ty: Ty<'db>,
    out: &mut Effects,
    depth: u32,
) {
    if depth > DROP_DEPTH {
        out.add(Effect::Unknown, Culprit::UnknownDrop);
        return;
    }
    {
        let infcx = infer_ctxt(db, env.krate);
        if has_drop_glue(&infcx, ty, env.param_env) == DropGlue::None {
            return;
        }
    }
    match ty.kind() {
        TyKind::Adt(def, args) => {
            let adt = def.def_id();
            let lang = lang_items(db, env.krate);
            if destructor(db, adt).is_some()
                && let Some(drop_trait) = lang.Drop
                && let Some(drop_fn) = drop_trait
                    .trait_items(db)
                    .method_by_name(&hir::Name::new_symbol_root(intern::sym::drop))
            {
                let self_args = GenericArgs::new_from_slice(&[GenericArg::from(ty)]);
                let summary = match alloc::resolve(db, env, drop_fn, self_args) {
                    Resolved::Function(g, gargs)
                        if g != drop_fn && FunctionSignature::of(db, g).has_body() =>
                    {
                        effects(db, env.store(), g, gargs.store())
                    }
                    _ => Effects::unknown(Culprit::UnknownDrop),
                };
                out.add_callee(&summary, None);
            }
            if let AdtId::StructId(it) = adt
                && lang.OwnedBox == Some(it)
            {
                drop_effects(db, env, args.type_at(0), out, depth + 1);
                return;
            }
            let interner = DbInterner::new_no_crate(db);
            alloc::any_variant(db, adt, |variant| {
                for (_, field) in db.field_types(variant).iter() {
                    let field = field.ty().instantiate(interner, args).skip_norm_wip();
                    drop_effects(db, env, field, out, depth + 1);
                }
                false
            });
        }
        TyKind::Tuple(fields) => {
            for field in fields.iter() {
                drop_effects(db, env, field, out, depth + 1);
            }
        }
        TyKind::Array(element, _) | TyKind::Slice(element) => {
            drop_effects(db, env, element, out, depth + 1);
        }
        TyKind::Closure(_, args) => {
            drop_effects(db, env, args.as_closure().tupled_upvars_ty(), out, depth + 1);
        }
        _ => out.add(Effect::Unknown, Culprit::UnknownDrop),
    }
}

/// What dropping a value of `ty` does.
pub(crate) fn drop_summary<'db>(db: &'db Db, env: ParamEnvAndCrate<'db>, ty: Ty<'db>) -> Effects {
    let mut out = Effects::NONE;
    drop_effects(db, env, ty, &mut out, 0);
    out
}

pub(crate) fn infer_ctxt(db: &Db, krate: base_db::Crate) -> InferCtxt<'_> {
    DbInterner::new_with(db, krate).with_regions().infer_ctxt().build(TypingMode::non_body_analysis())
}

/// Whether reading the static `id` reads state that changes: a `static mut`, a thread-local, a
/// foreign static (but the allocator's), or one with interior mutability.
pub(crate) fn static_changes(db: &Db, krate: base_db::Crate, id: StaticId) -> bool {
    let signature = StaticSignature::of(db, id);
    if signature.flags.contains(StaticFlags::EXTERN) {
        let name =
            AttrFlags::link_name(db, id.into()).unwrap_or_else(|| signature.name.symbol().clone());
        return !name.as_str().starts_with("__rust_");
    }
    if signature.flags.contains(StaticFlags::MUTABLE)
        || AttrFlags::query(db, id.into()).contains(AttrFlags::THREAD_LOCAL)
    {
        return true;
    }
    let ty = db.type_for_static(id).skip_binder();
    !is_freeze(&infer_ctxt(db, krate), ty, ParamEnv::empty())
}

/// How many dereferences `place` goes through.
pub(crate) fn derefs(place: &StoredPlace) -> usize {
    place.projection.as_slice().iter().filter(|it| matches!(it, ProjectionElem::Deref)).count()
}

/// The summary of one body as it is built: where each local's pointers point, then what the
/// body does.
struct Frame<'a, 'db> {
    db: &'db Db,
    env: ParamEnvAndCrate<'db>,
    body: &'a MirBody<'db>,
    /// Each local's [`Origin`].
    origin: Vec<Origin>,
    /// Whether the body takes the local's address.
    taken: Vec<bool>,
    /// What any pointer stored through a pointer may be.
    stored: Origin,
    /// What the pointers in this frame's memory may be: the locals whose address is taken, and
    /// what is stored.
    fresh: Origin,
    out: Effects,
}

impl<'a, 'db> Frame<'a, 'db> {
    fn new(db: &'db Db, env: ParamEnvAndCrate<'db>, body: &'a MirBody<'db>) -> Frame<'a, 'db> {
        let mut origin = vec![0; body.locals.len()];
        for (index, &local) in body.param_locals.iter().enumerate() {
            origin[index_of(local)] = direct(index);
        }
        let taken = vec![false; body.locals.len()];
        Frame { db, env, body, origin, taken, stored: 0, fresh: 0, out: Effects::NONE }
    }

    fn summarize(mut self) -> Effects {
        let calls = self.calls();
        while self.propagate(&calls) {}
        self.effects(&calls);
        self.out.returns = self.origin[0];
        self.out.writes &= DIRECT | DEEP;
        self.out
    }

    /// The summary of each call terminator of the body, by block, once.
    fn calls(&self) -> Vec<Option<(Effects, bool)>> {
        let mut calls = Vec::with_capacity(self.body.basic_blocks.len());
        for (_, block) in self.body.basic_blocks.iter() {
            let call = match block.terminator.as_ref().map(|it| &it.kind) {
                Some(TerminatorKind::Call { func, intrinsic, .. }) if !block.is_cleanup => {
                    Some(call_summary(self.db, self.env, func, *intrinsic))
                }
                _ => None,
            };
            calls.push(call);
        }
        calls
    }

    /// Where a pointer loaded through a pointer of `origin` may point.
    fn load(&self, origin: Origin) -> Origin {
        let mut out = (origin & DIRECT) << PARAMS | origin & (DEEP | STATIC | UNKNOWN);
        if origin & FRESH != 0 {
            out |= self.fresh;
        }
        if origin != 0 {
            out |= self.stored;
        }
        out
    }

    /// All memory reachable from pointers of `origin`.
    fn reach(&self, origin: Origin) -> Origin {
        let mut reach = origin;
        loop {
            let next = reach | self.load(reach);
            if next == reach {
                return reach;
            }
            reach = next;
        }
    }

    /// Where the value at `place` points.
    fn value(&self, place: &StoredPlace) -> Origin {
        let mut origin = self.origin[index_of(place.local)];
        for _ in 0..derefs(place) {
            origin = self.load(origin);
        }
        origin
    }

    /// The memory `place` is in: this frame's, or what the pointer it goes through points at.
    fn memory(&self, place: &StoredPlace) -> Origin {
        match derefs(place) {
            0 => FRESH,
            n => {
                let mut origin = self.origin[index_of(place.local)];
                for _ in 1..n {
                    origin = self.load(origin);
                }
                origin
            }
        }
    }

    fn operand(&self, operand: &Operand) -> Origin {
        match &operand.kind {
            OperandKind::Copy(place) | OperandKind::Move(place) => self.value(place),
            OperandKind::Static(_) => STATIC,
            OperandKind::Constant { .. } | OperandKind::Allocation { .. } => 0,
        }
    }

    fn rvalue(&mut self, rvalue: &Rvalue) -> Origin {
        match rvalue {
            Rvalue::Use(it) | Rvalue::Repeat(it, _) | Rvalue::UnaryOp(_, it) => self.operand(it),
            Rvalue::Cast(CastKind::PointerFromExposedAddress | CastKind::Transmute, it, _) => {
                self.operand(it) | UNKNOWN
            }
            Rvalue::Cast(_, it, _) => self.operand(it),
            Rvalue::Ref(_, place) => {
                if derefs(place) == 0 {
                    self.taken[index_of(place.local)] = true;
                }
                self.memory(place)
            }
            Rvalue::CopyForDeref(place) => self.value(place),
            Rvalue::BinaryOp(BinOp::Offset, it, _) | Rvalue::CheckedBinaryOp(BinOp::Offset, it, _) => {
                self.operand(it)
            }
            Rvalue::BinaryOp(..) | Rvalue::CheckedBinaryOp(..) => 0,
            Rvalue::Aggregate(_, operands) => {
                operands.iter().fold(0, |sum, it| sum | self.operand(it))
            }
            Rvalue::Len(_) | Rvalue::Discriminant(_) => 0,
            Rvalue::ThreadLocalRef(n) | Rvalue::AddressOf(n) | Rvalue::NullaryOp(n) => match *n {},
        }
    }

    /// Puts `origin` in what `place` holds.
    fn assign(&mut self, place: &StoredPlace, origin: Origin) {
        match derefs(place) {
            0 => self.origin[index_of(place.local)] |= origin,
            _ => self.stored |= origin,
        }
    }

    /// Where the callee's memory `bits` is in the caller: a parameter's own memory is what the
    /// argument points at, and what is reachable from it, all that is reachable from there.
    /// Through `Fn*` the parameters after the first are the fields of the second argument.
    fn map(&self, bits: u64, args: &[Operand], fn_call: bool) -> Origin {
        let mut origin = 0;
        for (index, arg) in args.iter().enumerate() {
            let (own, deeper) = match (fn_call, index) {
                (true, 0) => (bits & direct(0), bits & deep(0)),
                (true, 1) => (bits & DIRECT & !direct(0), bits & DEEP & !deep(0)),
                (true, _) => (0, 0),
                (false, _) => (bits & direct(index), bits & deep(index)),
            };
            if own | deeper == 0 {
                continue;
            }
            let points = self.operand(arg);
            if own != 0 {
                origin |= points;
            }
            if deeper != 0 {
                origin |= self.reach(self.load(points));
            }
        }
        origin
    }

    /// One pass of where pointers point over the body; whether anything changed.
    fn propagate(&mut self, calls: &[Option<(Effects, bool)>]) -> bool {
        // clone: the state before the pass, to tell whether it changed; once per pass
        let before = (self.stored, self.fresh, self.origin.clone(), self.taken.clone());
        let body = self.body;
        for ((_, block), call) in body.basic_blocks.iter().zip(calls) {
            if block.is_cleanup {
                continue;
            }
            for statement in &block.statements {
                if let StatementKind::Assign(place, rvalue) = &statement.kind {
                    let origin = self.rvalue(rvalue);
                    self.assign(place, origin);
                }
            }
            let Some(terminator) = &block.terminator else { continue };
            match &terminator.kind {
                TerminatorKind::Call { args, destination, .. } => {
                    let Some((summary, fn_call)) = call else { continue };
                    let returns = self.map(summary.returns, args, *fn_call)
                        | summary.returns & (STATIC | FRESH | UNKNOWN);
                    self.assign(destination, returns);
                    if summary.writes != 0 {
                        // What the callee stores may be any pointer its arguments reach.
                        let reached = self.map(DIRECT | DEEP, args, *fn_call);
                        self.stored |= reached;
                    }
                }
                TerminatorKind::DropAndReplace { place, value, .. } => {
                    let origin = self.operand(value);
                    self.assign(place, origin);
                }
                _ => {}
            }
        }
        let mut fresh = self.stored;
        for (index, &taken) in self.taken.iter().enumerate() {
            if taken {
                fresh |= self.origin[index];
            }
        }
        self.fresh = fresh;
        (self.stored, self.fresh, &self.origin, &self.taken)
            != (before.0, before.1, &before.2, &before.3)
    }

    /// The effects of the body, once where its pointers point is known.
    fn effects(&mut self, calls: &[Option<(Effects, bool)>]) {
        let (db, env, body) = (self.db, self.env, self.body);
        for ((_, block), call) in body.basic_blocks.iter().zip(calls) {
            if block.is_cleanup {
                continue;
            }
            for statement in &block.statements {
                if statement.kind.may_panic() {
                    self.out.kinds |= Effect::Panic.bit();
                }
                match &statement.kind {
                    StatementKind::Assign(place, _) if derefs(place) > 0 => {
                        let memory = self.memory(place);
                        self.write(memory);
                    }
                    StatementKind::InlineAsm(_) => self.out.add(Effect::Asm, Culprit::Asm),
                    _ => {}
                }
                statement.kind.for_each_operand(&mut |it| self.read(it));
            }
            let Some(terminator) = &block.terminator else { continue };
            terminator.kind.for_each_operand(&mut |it| self.read(it));
            match &terminator.kind {
                TerminatorKind::Call { args, destination, .. } => {
                    let Some((summary, fn_call)) = call else { continue };
                    self.out.add_callee(summary, None);
                    let written = self.map(summary.writes, args, *fn_call);
                    self.write(written & !FRESH);
                    if derefs(destination) > 0 {
                        let memory = self.memory(destination);
                        self.write(memory);
                    }
                }
                TerminatorKind::Assert { .. } => self.out.kinds |= Effect::Panic.bit(),
                TerminatorKind::Drop { place, .. } | TerminatorKind::DropAndReplace { place, .. } => {
                    let infcx = infer_ctxt(db, env.krate);
                    let ty = place.as_ref().ty(body, &infcx, env.param_env).ty;
                    let mut dropped = Effects::NONE;
                    drop_effects(db, env, ty, &mut dropped, 0);
                    self.out.add_callee(&dropped, None);
                    if derefs(place) > 0 {
                        let memory = self.memory(place);
                        self.write(memory);
                    }
                }
                TerminatorKind::Yield { .. } => self.out.add(Effect::Unknown, Culprit::Yield),
                _ => {}
            }
        }
    }

    /// A write to memory of `origin`.
    fn write(&mut self, origin: Origin) {
        self.out.writes |= origin & (DIRECT | DEEP);
        if origin & UNKNOWN != 0 {
            self.out.add(Effect::RawWrite, Culprit::RawPointer);
        }
        if origin & STATIC != 0 {
            self.out.add(Effect::Global, Culprit::StaticWrite);
        }
    }

    fn read(&mut self, operand: &Operand) {
        if let OperandKind::Static(id) = operand.kind
            && static_changes(self.db, self.env.krate, id)
        {
            self.out.add(Effect::Global, Culprit::Static(id));
        }
    }
}

pub(crate) fn index_of(local: hir_ty::mir::LocalId) -> usize {
    local.into_raw().into_u32() as usize
}
