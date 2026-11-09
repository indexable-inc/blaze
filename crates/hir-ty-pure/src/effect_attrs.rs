//! `#[blaze::nopanic]`, `#[blaze::noalloc]` and `#[blaze::nonblocking]`
//! (docs/v2/plan/design/effects.md): their [`EffectDef`]s, and the rules and tables the walk
//! ([`crate::walk`]) asks for the kinds of a site: an `Assert` ([`assert_rule`]), a method of an
//! allocator ([`allocator_rule`]), a foreign function ([`foreign_kinds`]), inline assembly
//! ([`asm_kinds`]), an intrinsic ([`intrinsic_kinds`]) and a function of a library outside the
//! toolchain ([`row_of`]). The walk and [`crate::effects`] (the vocabulary, the toolchain's rows
//! in `MODELS`) are lane pure's; this file is lane effects'.
//!
//! How `noalloc` tells an explicit allocator from the global one in MIR: a call that dispatches
//! to a method of an `Allocator` impl is [`Rule::AllocIn`] unless the impl's `Self`, the
//! instance's arguments substituted and references taken off, is alloc's `Global` or std's
//! `System`, the global heap ([`Rule::Alloc`]); every method of a `GlobalAlloc` impl is
//! [`Rule::Alloc`]. So `Vec::new_in(frame).push(x)` reaches `<&Frame as Allocator>::allocate`,
//! `AllocIn`, which `noalloc` allows (AGENTS.md rule 11's proof), and `Vec::new().push(x)` reaches
//! `<Global as Allocator>::allocate`, `Alloc`, which it refuses. By type, never by data flow; the
//! allocator's own body is not walked.

use base_db::{Crate, CrateOrigin, Db, LangCrateOrigin};
use blaze_hash::HashMap;
use hir_def::{
    AdtId, AttrDefId, FunctionId, HasModule, ItemContainerId, Lookup, ModuleDefId, ModuleId,
    attrs::{AttrFlags, EffectWord, PureAttr},
    nameres::{ModuleData, crate_def_map},
    signatures::{FunctionSignature, StructSignature, TraitSignature},
};
use hir_expand::name::Name;
use hir_ty::{
    MemoryMap,
    db::HirDatabase,
    intrinsic::Intrinsic,
    method_resolution::InherentImpls,
    next_solver::{ConstKind, DbInterner, GenericArgs, SimplifiedType, SolverDefId, TyKind},
};
use hir_ty_mir::mir::{
    BasicBlock, BasicBlockId, BinOp, InlineAsm, LocalId, MirBody, Operand, OperandKind,
    ProjectionElem, Rvalue, Statement, StatementKind, TerminatorKind,
};
use intern::Symbol;
use rustc_type_ir::inherent::IntoKind as _;
use std::alloc::Global;

use crate::effects::{EffectDef, EffectSet, Rule};

// The attribute effects.

/// `#[blaze::nopanic]` where the crate built keeps overflow checks (a dev profile, or
/// `overflow-checks = true`): no panic or abort entry point, no `Assert` the MIR keeps (optional
/// overflow checks included), no foreign call whose ABI may unwind, nothing the walk can't see.
pub const NOPANIC: EffectDef = EffectDef {
    name: "nopanic",
    refuses: EffectSet::of(&[Rule::Panic, Rule::Overflow, Rule::ForeignUnwind, Rule::Unchecked]),
    attr: nopanic_attr,
    says,
    fix: "a nopanic function and everything it calls cannot panic or abort: check the index or the arithmetic (`get`, `checked_*`, `wrapping_*`), or mark a callee proven by hand `#[blaze::nopanic(trusted = \"why\")]`",
    through_never: true,
};

/// [`NOPANIC`] where the crate built drops optional overflow checks (release without
/// `overflow-checks`): an optional overflow `Assert` is not code that runs there.
pub const NOPANIC_NO_OVERFLOW_CHECKS: EffectDef = EffectDef {
    refuses: EffectSet::of(&[Rule::Panic, Rule::ForeignUnwind, Rule::Unchecked]),
    ..NOPANIC
};

/// `#[blaze::noalloc]`: no call of the global allocator, transitively; an `Allocator` the code
/// names (`Vec::new_in(frame)`) is allowed.
pub const NOALLOC: EffectDef = EffectDef {
    name: "noalloc",
    refuses: EffectSet::of(&[Rule::Alloc, Rule::ForeignUnknown, Rule::Unchecked]),
    attr: noalloc_attr,
    says,
    fix: "allocate from the task's frame (`Vec::new_in(frame)`, `Box::new_in(.., frame)` with `blaze_mem::frame::current`), or mark a callee proven by hand `#[blaze::noalloc(trusted = \"why\")]`",
    through_never: true,
};

/// `#[blaze::nonblocking]`: no call that can block the thread (std's and `parking_lot`'s locks,
/// condvars, channels, sleeps, joins; the blocking syscalls of [`FOREIGN`]), transitively.
pub const NONBLOCKING: EffectDef = EffectDef {
    name: "nonblocking",
    refuses: EffectSet::of(&[Rule::Block, Rule::ForeignUnknown, Rule::Unchecked]),
    attr: nonblocking_attr,
    says,
    fix: "on a pool thread or fiber wait through the scheduler (a key, a counter, a `sched::Event`) and do IO on a driver thread, or mark a callee proven by hand `#[blaze::nonblocking(trusted = \"why\")]`",
    through_never: true,
};

/// The `nopanic` a check uses for a crate built with `overflow_checks`
/// (`hir_ty_mir::mir::overflow::crate_checks_overflow` of the crate that makes the instance):
/// the profile being built, never one guessed.
pub const fn nopanic_for(overflow_checks: bool) -> &'static EffectDef {
    if overflow_checks { &NOPANIC } else { &NOPANIC_NO_OVERFLOW_CHECKS }
}

fn nopanic_attr(db: &Db, owner: AttrDefId) -> Option<PureAttr> {
    AttrFlags::effect_attr(db, owner, EffectWord::Nopanic)
}

fn noalloc_attr(db: &Db, owner: AttrDefId) -> Option<PureAttr> {
    AttrFlags::effect_attr(db, owner, EffectWord::Noalloc)
}

fn nonblocking_attr(db: &Db, owner: AttrDefId) -> Option<PureAttr> {
    AttrFlags::effect_attr(db, owner, EffectWord::Nonblocking)
}

/// What a kind is said as in these effects' refusals.
fn says(rule: Rule) -> &'static str {
    match rule {
        Rule::Panic => "may panic or abort",
        Rule::Overflow => {
            "may panic on an arithmetic overflow, which this profile checks (overflow-checks is on)"
        }
        Rule::Alloc => "calls the global allocator",
        Rule::Block => "may block the thread",
        Rule::ForeignUnwind => "calls a foreign function whose ABI may unwind",
        Rule::ForeignUnknown => {
            "calls a foreign function (or runs inline assembly) blaze knows nothing of"
        }
        Rule::Unchecked => "runs code the check can't see",
        other => other.said(),
    }
}

// The site rules.

/// The kind of the `Assert` that ends `block`: `None` when its condition is a constant that
/// holds (the runtime MIR is not folded, so the one fold that matters is taken here: a constant,
/// or a local the block last assigns a comparison of two unsigned integers the block makes
/// constants, which is `arr[2]` on a `[T; 4]` or a `&[T; 4]`), [`Rule::Overflow`] for an optional overflow check of a body that
/// inherits the setting from the crate it is compiled in (`#[rustc_inherit_overflow_checks]`:
/// whether it runs is the marked function's profile), and [`Rule::Panic`] for any other: bounds,
/// division, a UB check, and an overflow check of a body whose own crate keeps them (the MIR has
/// one only then: `transform::asserts` builds them per the body's crate, so a check here runs).
pub(crate) fn assert_rule(body: &MirBody<'_>, block: BasicBlockId) -> Option<Rule> {
    let data = &body.basic_blocks[block];
    let terminator = data.terminator.as_ref()?;
    let TerminatorKind::Assert { cond, expected, msg, .. } = &terminator.kind else { return None };
    if condition(data, cond) == Some(*expected) {
        return None;
    }
    Some(if msg.is_optional_overflow_check() && body.inherits_overflow_checks {
        Rule::Overflow
    } else {
        Rule::Panic
    })
}

/// An assert's condition when it is known without running the body.
fn condition(block: &BasicBlock, cond: &Operand) -> Option<bool> {
    if let Some(bits) = constant_bits(cond) {
        return Some(bits != 0);
    }
    let local = operand_local(cond)?;
    // The last assignment of the local in the block; the condition is never written another way.
    let (at, rvalue) = block.statements.iter().enumerate().rev().find_map(|(at, statement)| {
        match &statement.kind {
            StatementKind::Assign(to, rvalue) if to.as_ref().as_local() == Some(local) => {
                Some((at, rvalue))
            }
            _ => None,
        }
    })?;
    let Rvalue::BinaryOp(op, a, b) = rvalue else { return None };
    // A bounds check compares the index's local (`_3 = const 2_usize` before it, for `a[2]`)
    // with the array's constant length; the bits compare as unsigned `usize`s.
    let before = &block.statements[..at];
    let (a, b) = (known_bits(before, a)?, known_bits(before, b)?);
    Some(match op {
        BinOp::Lt => a < b,
        BinOp::Le => a <= b,
        BinOp::Gt => a > b,
        BinOp::Ge => a >= b,
        BinOp::Eq => a == b,
        BinOp::Ne => a != b,
        _ => return None,
    })
}

/// The local an operand copies or moves, when it is a whole local.
fn operand_local(operand: &Operand) -> Option<LocalId> {
    let (OperandKind::Copy(place) | OperandKind::Move(place)) = &operand.kind else { return None };
    place.as_ref().as_local()
}

/// The bits `operand` has once `statements` have run: a constant, or a local whose last write
/// among them is a constant (`_3 = const 2_usize`). Anything after that write that could change
/// the local another way (a write through a pointer, to a part of it, `Deinit`, its storage
/// ending, inline assembly) leaves it unknown, as does a write in another block.
fn known_bits(statements: &[Statement], operand: &Operand) -> Option<u128> {
    if let Some(bits) = constant_bits(operand) {
        return Some(bits);
    }
    let local = operand_local(operand)?;
    for statement in statements.iter().rev() {
        match &statement.kind {
            StatementKind::Assign(to, rvalue) => {
                let to = to.as_ref();
                if to.as_local() == Some(local) {
                    let Rvalue::Use(value) = rvalue else { return None };
                    return constant_bits(value);
                }
                if to.local == local
                    || to.projection.as_slice().iter().any(|it| matches!(it, ProjectionElem::Deref))
                {
                    return None;
                }
            }
            StatementKind::Deinit(to) if to.local == local => return None,
            StatementKind::StorageDead(it) | StatementKind::StorageLive(it) if *it == local => {
                return None;
            }
            StatementKind::InlineAsm(_) => return None,
            _ => {}
        }
    }
    None
}

/// The bits of a constant scalar operand (as `optimize::cfg::constant_bits`, which is private to
/// the optimizer).
fn constant_bits(operand: &Operand) -> Option<u128> {
    match &operand.kind {
        OperandKind::Allocation { allocation } => {
            let allocation = allocation.as_ref();
            let bytes = &allocation.memory;
            if bytes.len() > 16 || !matches!(allocation.memory_map, MemoryMap::Empty) {
                return None;
            }
            let mut padded = [0u8; 16];
            padded[..bytes.len()].copy_from_slice(bytes);
            Some(u128::from_le_bytes(padded))
        }
        OperandKind::Constant { konst, .. } => match konst.as_ref().kind() {
            ConstKind::Value(value) => value.try_to_leaf().map(|it| it.to_bits_unchecked()),
            _ => None,
        },
        _ => None,
    }
}

/// For a call of `func` with `args` (an impl's arguments first): `Some` when `func` is a method
/// of an `Allocator` or `GlobalAlloc` impl, which the walk stops at with that kind:
/// [`Rule::Alloc`] for a `GlobalAlloc` and for `Global`'s and `System`'s `Allocator`,
/// [`Rule::AllocIn`] for any other `Allocator` (the module docs say why).
pub(crate) fn allocator_rule<'db>(
    db: &'db Db,
    func: FunctionId,
    args: GenericArgs<'db>,
) -> Option<Rule> {
    let ItemContainerId::ImplId(imp) = func.lookup(db).container else { return None };
    let trait_ = db.impl_trait(imp)?.skip_binder().def_id.0;
    let core = matches!(
        HasModule::krate(&trait_, db).data(db).origin,
        CrateOrigin::Lang(LangCrateOrigin::Core)
    );
    if !core {
        return None;
    }
    match TraitSignature::of(db, trait_).name.symbol().as_str() {
        "GlobalAlloc" => return Some(Rule::Alloc),
        "Allocator" => {}
        _ => return None,
    }
    // `by_ref` hands back the allocator itself: no allocation.
    if FunctionSignature::of(db, func).name.symbol().as_str() == "by_ref" {
        return None;
    }
    let interner = DbInterner::new_with(db, HasModule::krate(&func, db));
    let mut self_ty = db.impl_self_ty(imp).instantiate(interner, args).skip_norm_wip();
    while let TyKind::Ref(_, inner, _) = self_ty.kind() {
        self_ty = inner;
    }
    let global = match self_ty.kind() {
        TyKind::Adt(def, _) => is_global_heap(db, def.def_id()),
        // A type left open (a generic check) is decided per instance; until then the strict
        // answer.
        _ => true,
    };
    Some(if global { Rule::Alloc } else { Rule::AllocIn })
}

/// Whether `adt` is alloc's `Global` or std's `System`.
fn is_global_heap(db: &Db, adt: AdtId) -> bool {
    let AdtId::StructId(it) = adt else { return false };
    let origin = &HasModule::krate(&adt, db).data(db).origin;
    match StructSignature::of(db, it).name.symbol().as_str() {
        "Global" => matches!(origin, CrateOrigin::Lang(LangCrateOrigin::Alloc)),
        "System" => matches!(origin, CrateOrigin::Lang(LangCrateOrigin::Std)),
        _ => false,
    }
}

/// The kinds of a call of the foreign function `func`, linking to `link`: [`Rule::Foreign`]
/// (the walk clears it for `deterministic_foreign`), then [`FOREIGN`]'s kinds of `link`, or
/// [`Rule::ForeignUnknown`] for a symbol it does not name, with [`Rule::ForeignUnwind`] when its
/// ABI may unwind and it is not `#[rustc_nounwind]`.
pub(crate) fn foreign_kinds(db: &Db, func: FunctionId, link: &str) -> EffectSet {
    let foreign = EffectSet::one(Rule::Foreign);
    if let Some(kinds) = foreign_row(link) {
        return foreign.union(kinds);
    }
    let mut kinds = foreign.union(EffectSet::one(Rule::ForeignUnknown));
    let nounwind = AttrFlags::query(db, func.into()).contains(AttrFlags::RUSTC_NOUNWIND);
    if may_unwind(FunctionSignature::of(db, func).abi.as_str()) && !nounwind {
        kinds = kinds.union(EffectSet::one(Rule::ForeignUnwind));
    }
    kinds
}

/// [`FOREIGN`]'s kinds of `symbol`, if it names it (a clean one is the empty set).
pub(crate) fn foreign_row(symbol: &str) -> Option<EffectSet> {
    FOREIGN.iter().find(|(it, _)| *it == symbol).map(|&(_, kind)| match kind {
        Foreign::Block => EffectSet::one(Rule::Block),
        Foreign::Alloc => EffectSet::one(Rule::Alloc),
        Foreign::Panic => EffectSet::one(Rule::Panic),
        Foreign::Clean => EffectSet::EMPTY,
    })
}

/// Whether a function of the ABI named `abi` may unwind into its caller.
fn may_unwind(abi: &str) -> bool {
    matches!(abi, "Rust" | "rust-call" | "rust-cold" | "rust-preserve-none" | "tail")
        || abi.ends_with("-unwind")
}

/// The kinds of an `asm!` block: [`Rule::Foreign`] and [`Rule::ForeignUnknown`] (it may be a
/// syscall, or call anything), and [`Rule::ForeignUnwind`] with `options(may_unwind)`.
pub(crate) fn asm_kinds(asm: &InlineAsm) -> EffectSet {
    let kinds = EffectSet::of(&[Rule::Foreign, Rule::ForeignUnknown]);
    if asm.options.contains(hir_def::hir::AsmOptions::MAY_UNWIND) {
        return kinds.union(EffectSet::one(Rule::ForeignUnwind));
    }
    kinds
}

/// The kinds of an intrinsic's call beside what purity makes of it: `abort` ends the process
/// as a panic with `panic = "abort"` does; `box_new`, where it is still a call, allocates from
/// the global heap.
pub(crate) fn intrinsic_kinds(intrinsic: Intrinsic) -> EffectSet {
    match intrinsic {
        Intrinsic::AbortImmediate => EffectSet::one(Rule::Panic),
        Intrinsic::BoxNew => EffectSet::one(Rule::Alloc),
        _ => EffectSet::EMPTY,
    }
}

// The library rows.

/// A function of a registry crate that blocks: a call of it counts as [`Rule::Thread`] and
/// [`Rule::Block`] at the call (its `covers` is those kinds: the rest of what is under it is
/// followed into its body).
pub(crate) struct Row {
    /// The path as said to a person: `lock_api::Mutex::lock`.
    pub(crate) what: &'static str,
    /// What a call does: "waits for the lock".
    pub(crate) says: &'static str,
    /// The crates.io name of its crate.
    krate: &'static str,
    at: At,
}

impl Row {
    pub(crate) fn kinds(&self) -> EffectSet {
        EffectSet::of(&[Rule::Thread, Rule::Block])
    }
}

/// Where in its crate a row's functions are.
#[derive(Clone, Copy)]
enum At {
    /// The free function at this path.
    Fn(&'static [&'static str]),
    /// The inherent methods so named of the type at this path.
    Methods(&'static [&'static str], &'static [&'static str]),
}

const fn row(what: &'static str, krate: &'static str, at: At, says: &'static str) -> Row {
    Row { what, says, krate, at }
}

const LOCK: &str = "waits for the lock";
const CONDVAR: &str = "waits on a condition variable";
const MESSAGE: &str = "waits for a message";
const ROOM: &str = "waits for room in the channel";
const PARK: &str = "parks the thread";
const ONCE: &str = "waits for another thread's initialization";

/// The blocking functions of the registry crates blaze and its users wait through, and of
/// blaze's own lock home ([`WORKSPACE_ROW_CRATES`]). The toolchain's are
/// `crate::effects::MODELS` rows; any other crate's waits reach a [`FOREIGN`] leaf (`futex`,
/// `read`).
pub(crate) const ROWS: &[Row] = &[
    row(
        "blaze_core::lock::Lock::{lock, try_lock_for}",
        "blaze_core",
        At::Methods(&["lock", "Lock"], &["lock", "try_lock_for"]),
        LOCK,
    ),
    row(
        "blaze_core::lock::ReadLock::{read, write}",
        "blaze_core",
        At::Methods(&["lock", "ReadLock"], &["read", "write"]),
        LOCK,
    ),
    row(
        "blaze_core::lock::Signal::wait*",
        "blaze_core",
        At::Methods(
            &["lock", "Signal"],
            &["wait", "wait_while", "wait_until", "wait_for", "wait_while_until", "wait_while_for"],
        ),
        CONDVAR,
    ),
    row("parking_lot_core::park", "parking_lot_core", At::Fn(&["park"]), PARK),
    row("lock_api::Mutex::lock", "lock_api", At::Methods(&["Mutex"], &["lock"]), LOCK),
    row(
        "lock_api::ReentrantMutex::lock",
        "lock_api",
        At::Methods(&["ReentrantMutex"], &["lock"]),
        LOCK,
    ),
    row(
        "lock_api::RwLock::{read, write, upgradable_read, read_recursive}",
        "lock_api",
        At::Methods(&["RwLock"], &["read", "write", "upgradable_read", "read_recursive"]),
        LOCK,
    ),
    row(
        "parking_lot::Condvar::wait*",
        "parking_lot",
        At::Methods(
            &["Condvar"],
            &["wait", "wait_for", "wait_until", "wait_while", "wait_while_for", "wait_while_until"],
        ),
        CONDVAR,
    ),
    row(
        "parking_lot::Once::call_once*",
        "parking_lot",
        At::Methods(&["Once"], &["call_once", "call_once_force"]),
        ONCE,
    ),
    row(
        "crossbeam_channel::Receiver::recv*",
        "crossbeam_channel",
        At::Methods(&["Receiver"], &["recv", "recv_timeout", "recv_deadline"]),
        MESSAGE,
    ),
    row(
        "crossbeam_channel::Sender::send*",
        "crossbeam_channel",
        At::Methods(&["Sender"], &["send", "send_timeout", "send_deadline"]),
        ROOM,
    ),
    row(
        "crossbeam_channel::Select::{select, ready}*",
        "crossbeam_channel",
        At::Methods(
            &["Select"],
            &[
                "select",
                "select_timeout",
                "select_deadline",
                "ready",
                "ready_timeout",
                "ready_deadline",
            ],
        ),
        "waits for a channel",
    ),
    row(
        "crossbeam_utils::sync::Parker::park*",
        "crossbeam_utils",
        At::Methods(&["sync", "Parker"], &["park", "park_timeout", "park_deadline"]),
        PARK,
    ),
    row(
        "crossbeam_utils::sync::WaitGroup::wait",
        "crossbeam_utils",
        At::Methods(&["sync", "WaitGroup"], &["wait"]),
        "waits for the group",
    ),
];

/// The workspace crates rows are of, known by their crate name: blaze's lock home
/// (docs/v2/plan/design/named-locks.md), whose `Lock::lock` and `ReadLock::{read, write}` are
/// lock acquires as `lock_api`'s are.
const WORKSPACE_ROW_CRATES: &[&str] = &["blaze_core"];

/// The row of `func`, a function of a crate a row of [`ROWS`] names. A registry crate is known
/// by its crates.io name, and only with a registry origin, so a workspace crate named
/// `lock_api` is never taken for it; a workspace member only when it is one of
/// [`WORKSPACE_ROW_CRATES`].
pub(crate) fn row_of(db: &Db, func: FunctionId) -> Option<&'static Row> {
    let krate = HasModule::krate(&func, db);
    registry_name(db, krate)?;
    crate_rows(db, krate).by_fn.get(&func).map(|&at| &ROWS[usize::from(at)])
}

/// The name rows know `krate` by, when a row is of it: its crates.io name for a registry
/// crate, its crate name for one of [`WORKSPACE_ROW_CRATES`].
fn registry_name(db: &Db, krate: Crate) -> Option<&'static str> {
    match &krate.data(db).origin {
        CrateOrigin::Library { name, .. } => {
            ROWS.iter().map(|row| row.krate).find(|it| *it == name.as_str())
        }
        CrateOrigin::Local { .. } => {
            let display = krate.extra_data(db).display_name.as_ref()?;
            let name = display.crate_name().symbol().as_str();
            WORKSPACE_ROW_CRATES.iter().copied().find(|it| *it == name)
        }
        CrateOrigin::Rustc { .. } | CrateOrigin::Lang(_) => None,
    }
}

/// [`ROWS`] resolved in one crate, and the rows of it that resolve to nothing (a stale table).
#[derive(Debug, PartialEq, Eq)]
pub struct CrateRows {
    by_fn: HashMap<FunctionId, u16, Global>,
    unresolved: Box<[&'static str]>,
}

/// [`ROWS`] resolved in `krate`, once per crate, through its def map (re-exports followed).
#[blaze_query::tracked(returns(ref))]
pub fn crate_rows(db: &Db, krate: Crate) -> CrateRows {
    let mut by_fn = HashMap::new_in(Global);
    let Some(name) = registry_name(db, krate) else {
        return CrateRows { by_fn, unresolved: Box::default() };
    };
    // alloc: once per crate, the paths a stale table misses
    let mut unresolved = Vec::new();
    let root = crate_def_map(db, krate).root_module_id();
    for (index, row) in ROWS.iter().enumerate() {
        if row.krate != name {
            continue;
        }
        let before = by_fn.len();
        match row.at {
            At::Fn(path) => {
                if let Some(ModuleDefId::FunctionId(func)) = item(db, root, path, false)
                    && HasModule::krate(&func, db) == krate
                {
                    by_fn.entry(func).or_insert(index as u16);
                }
            }
            At::Methods(ty, names) => {
                if let Some(ModuleDefId::AdtId(adt)) = item(db, root, ty, true) {
                    let self_ty = SimplifiedType::Adt(SolverDefId::AdtId(adt));
                    for &imp in InherentImpls::for_crate(db, krate).for_self_ty(&self_ty) {
                        for (method, assoc) in imp.impl_items(db).items.iter() {
                            if let hir_def::AssocItemId::FunctionId(func) = *assoc
                                && names.contains(&method.symbol().as_str())
                            {
                                by_fn.entry(func).or_insert(index as u16);
                            }
                        }
                    }
                }
            }
        }
        if by_fn.len() == before {
            unresolved.push(row.what);
        }
    }
    CrateRows { by_fn, unresolved: unresolved.into_boxed_slice() }
}

/// The rows of `krate` (a registry crate of [`ROWS`]) that resolve to nothing in it: empty when
/// the table matches the crate's version, as `models::check_resolved` is for the toolchain.
pub fn check_rows_resolved(db: &Db, krate: Crate) -> &[&'static str] {
    &crate_rows(db, krate).unresolved
}

fn data(db: &Db, module: ModuleId) -> &ModuleData {
    module.to_db(db).def_map(db).module(db, module)
}

fn name_of(segment: &str) -> Name {
    Name::new_symbol_root(Symbol::intern(segment))
}

/// The item at `path` under `root`, through re-exports, in the type or the value namespace.
fn item(db: &Db, root: ModuleId, path: &[&str], types: bool) -> Option<ModuleDefId> {
    let (last, modules) = path.split_last()?;
    let mut module = root;
    for segment in modules {
        match data(db, module).name_in_scope(db, &name_of(segment)).take_types()? {
            ModuleDefId::ModuleId(it) => module = it,
            _ => return None,
        }
    }
    let per_ns = data(db, module).name_in_scope(db, &name_of(last));
    if types { per_ns.take_types() } else { per_ns.take_values() }
}

// The foreign table.

/// What a [`FOREIGN`] symbol does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Foreign {
    /// May block the thread.
    Block,
    /// Is or calls the global heap.
    Alloc,
    /// Panics or aborts.
    Panic,
    /// Neither blocks nor allocates nor unwinds.
    Clean,
}

use Foreign::{Alloc as FA, Block as FB, Clean as FC, Panic as FP};

/// Foreign functions by the symbol they link to. A syscall on a file descriptor is `Block`
/// whatever the descriptor: the check can't prove one `O_NONBLOCK`, so a wrapper that knows says
/// so with `#[blaze::nonblocking(trusted = "..")]`. `syscall` is `Block`: std waits on a futex
/// through it on Linux.
pub(crate) const FOREIGN: &[(&str, Foreign)] = &[
    // The allocator's shim and the C heap (the `System` allocator).
    ("__rust_alloc", FA),
    ("__rust_dealloc", FA),
    ("__rust_realloc", FA),
    ("__rust_alloc_zeroed", FA),
    ("__rust_no_alloc_shim_is_unstable_v2", FC),
    ("malloc", FA),
    ("calloc", FA),
    ("realloc", FA),
    ("reallocf", FA),
    ("free", FA),
    ("posix_memalign", FA),
    ("aligned_alloc", FA),
    ("memalign", FA),
    ("valloc", FA),
    ("HeapAlloc", FA),
    ("HeapReAlloc", FA),
    ("HeapFree", FA),
    // The panic runtime.
    ("__rust_start_panic", FP),
    ("rust_begin_unwind", FP),
    ("panic_impl", FP),
    ("__rust_alloc_error_handler", FP),
    ("__rust_panic_cleanup", FP),
    ("abort", FP),
    // Blocking: waits.
    ("syscall", FB),
    ("futex", FB),
    ("__ulock_wait", FB),
    ("__ulock_wait2", FB),
    ("os_sync_wait_on_address", FB),
    ("os_sync_wait_on_address_with_deadline", FB),
    ("os_sync_wait_on_address_with_timeout", FB),
    ("pthread_cond_wait", FB),
    ("pthread_cond_timedwait", FB),
    ("pthread_cond_clockwait", FB),
    ("pthread_mutex_lock", FB),
    ("pthread_mutex_timedlock", FB),
    ("pthread_rwlock_rdlock", FB),
    ("pthread_rwlock_wrlock", FB),
    ("pthread_rwlock_timedrdlock", FB),
    ("pthread_rwlock_timedwrlock", FB),
    ("pthread_join", FB),
    ("sem_wait", FB),
    ("sem_timedwait", FB),
    ("dispatch_semaphore_wait", FB),
    ("nanosleep", FB),
    ("clock_nanosleep", FB),
    ("usleep", FB),
    ("sleep", FB),
    ("pause", FB),
    ("sigsuspend", FB),
    ("sigwait", FB),
    ("sigwaitinfo", FB),
    ("sigtimedwait", FB),
    ("wait", FB),
    ("waitpid", FB),
    ("wait4", FB),
    ("waitid", FB),
    ("poll", FB),
    ("ppoll", FB),
    ("select", FB),
    ("pselect", FB),
    ("epoll_wait", FB),
    ("epoll_pwait", FB),
    ("epoll_pwait2", FB),
    ("kevent", FB),
    ("kevent64", FB),
    ("WaitForSingleObject", FB),
    ("WaitForSingleObjectEx", FB),
    ("WaitForMultipleObjects", FB),
    ("WaitOnAddress", FB),
    ("Sleep", FB),
    ("SleepEx", FB),
    ("SleepConditionVariableSRW", FB),
    ("AcquireSRWLockExclusive", FB),
    ("AcquireSRWLockShared", FB),
    ("GetQueuedCompletionStatus", FB),
    ("GetQueuedCompletionStatusEx", FB),
    ("WSAPoll", FB),
    // Blocking: IO on a descriptor, opening, syncing and locking files, sockets, randomness that
    // waits for entropy, name lookups.
    ("read", FB),
    ("readv", FB),
    ("pread", FB),
    ("pread64", FB),
    ("preadv", FB),
    ("write", FB),
    ("writev", FB),
    ("pwrite", FB),
    ("pwrite64", FB),
    ("pwritev", FB),
    ("recv", FB),
    ("recvfrom", FB),
    ("recvmsg", FB),
    ("send", FB),
    ("sendto", FB),
    ("sendmsg", FB),
    ("sendfile", FB),
    ("copy_file_range", FB),
    ("accept", FB),
    ("accept4", FB),
    ("connect", FB),
    ("open", FB),
    ("open64", FB),
    ("openat", FB),
    ("openat64", FB),
    ("close", FB),
    ("fsync", FB),
    ("fdatasync", FB),
    ("flock", FB),
    ("lockf", FB),
    ("fcntl", FB),
    ("getrandom", FB),
    ("getentropy", FB),
    ("getaddrinfo", FB),
    ("ReadFile", FB),
    ("WriteFile", FB),
    ("NtReadFile", FB),
    ("NtWriteFile", FB),
    ("ConnectNamedPipe", FB),
    ("WSARecv", FB),
    ("WSARecvFrom", FB),
    ("WSASend", FB),
    ("WSASendTo", FB),
    // Clean: memory, the clock, ids, thread-locals and their destructors, wakes, tries, yields,
    // mapping pages.
    ("memcpy", FC),
    ("memmove", FC),
    ("memset", FC),
    ("memcmp", FC),
    ("bcmp", FC),
    ("memchr", FC),
    ("memrchr", FC),
    ("strlen", FC),
    ("strnlen", FC),
    ("strcmp", FC),
    ("strncmp", FC),
    ("clock_gettime", FC),
    ("clock_getres", FC),
    ("gettimeofday", FC),
    ("mach_absolute_time", FC),
    ("mach_continuous_time", FC),
    ("mach_timebase_info", FC),
    ("QueryPerformanceCounter", FC),
    ("QueryPerformanceFrequency", FC),
    ("getpid", FC),
    ("getppid", FC),
    ("gettid", FC),
    ("pthread_self", FC),
    ("pthread_threadid_np", FC),
    ("GetCurrentThreadId", FC),
    ("GetCurrentProcessId", FC),
    ("pthread_getspecific", FC),
    ("pthread_setspecific", FC),
    ("pthread_key_create", FC),
    ("__cxa_thread_atexit_impl", FC),
    ("_tlv_atexit", FC),
    ("__errno_location", FC),
    ("__error", FC),
    ("_errno", FC),
    ("GetLastError", FC),
    ("SetLastError", FC),
    ("sched_yield", FC),
    ("sched_getcpu", FC),
    ("SwitchToThread", FC),
    ("sysconf", FC),
    ("getpagesize", FC),
    ("getauxval", FC),
    ("pthread_mutex_unlock", FC),
    ("pthread_mutex_trylock", FC),
    ("pthread_rwlock_unlock", FC),
    ("pthread_rwlock_tryrdlock", FC),
    ("pthread_rwlock_trywrlock", FC),
    ("pthread_cond_signal", FC),
    ("pthread_cond_broadcast", FC),
    ("__ulock_wake", FC),
    ("os_sync_wake_by_address_any", FC),
    ("os_sync_wake_by_address_all", FC),
    ("WakeByAddressSingle", FC),
    ("WakeByAddressAll", FC),
    ("ReleaseSRWLockExclusive", FC),
    ("ReleaseSRWLockShared", FC),
    ("TryAcquireSRWLockExclusive", FC),
    ("TryAcquireSRWLockShared", FC),
    ("WakeConditionVariable", FC),
    ("WakeAllConditionVariable", FC),
    ("arc4random_buf", FC),
    ("CCRandomGenerateBytes", FC),
    ("ProcessPrng", FC),
    ("mmap", FC),
    ("mmap64", FC),
    ("munmap", FC),
    ("mprotect", FC),
    ("madvise", FC),
];
