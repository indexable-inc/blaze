//! blaze's own lints, `blaze::`: what costs in hot code, the rules `tools/v2/hot_lint.py`
//! checks by pattern, from types and resolutions instead
//! ([docs/lints.md](../../../docs/lints.md)).
//!
//! The walk calls [`check_node`] on every node. A path type is checked where it is written
//! (`default_hasher`'s type form); the calls of a body are checked once, from its root, over the
//! body's HIR, so each sees what inference resolved: the callee and its `Self`, the value's
//! type, the allocator reach of the instance called. A finding is placed at its expression, or
//! at the outermost macro call that wrote it (`format!`, `vec!`), and dropped where its
//! exemption comment is.

mod alloc;
mod arc_clone_only_borrowed;
mod blocking_in_async;
mod blocking_on_pool;
mod bool_param;
mod borrow_workaround;
#[cfg(test)]
mod borrow_workaround_tests;
mod bounded_heap_field;
mod bounds_check_in_loop;
mod box_large_new;
mod boxed_recursive_node;
mod build_script_ambient;
mod checkfree;
mod clone;
mod cold_error;
mod could_be_program_lifetime;
mod could_be_ref32;
mod collect_then_consume;
mod combined_identity;
#[cfg(test)]
mod correctness_general_tests;
mod deep_drop_at_exit;
mod dispatch;
mod dos_hasher_on_input;
mod dropped_error;
mod durable_write;
mod dyn_fn_param;
mod dyn_in_hot_field;
mod effects;
#[cfg(test)]
mod effects_tests;
mod may;
#[cfg(test)]
mod may_tests;
mod enum_vec_padded;
mod error_debug;
#[cfg(test)]
mod errors_tests;
mod exempt;
pub(crate) mod facts;
mod false_sharing;
mod fiber;
mod fixed_key_lookup;
mod fmt_allocates;
mod front_vec_ops;
mod frozen_collection;
mod general_facts;
mod goose_fields;
mod goose_locals;
#[cfg(test)]
mod goose_perf_tests;
mod hash_order_output;
mod hash_twice;
mod hashmap;
mod held_across_wait;
mod implicit_allocator;
mod intern_candidate;
mod jagged_vec;
mod key_ignores_field;
mod large_move;
mod lenient_deserialize;
#[cfg(test)]
mod lenient_deserialize_tests;
mod lock_across_query;
mod lock_held_across_slow;
mod loops;
mod many_bools;
mod missing_size_hint;
mod native_contracts;
#[cfg(test)]
mod opaque_tests;
mod open_wait_loop;
mod outline_generic_body;
mod packable;
mod panic_in_drop;
mod parallel;
mod per_item;
#[cfg(test)]
mod perf_a_tests;
#[cfg(test)]
mod perf_general_tests;
#[cfg(test)]
mod perf_tests;
mod pod_serialized_by_field;
mod proc_macro;
#[cfg(test)]
mod profile_tests;
mod profiled;
mod program_lifetime;
#[cfg(test)]
mod program_lifetime_tests;
mod push_without_reserve;
mod query_in_loop;
mod relaxed_publish;
mod sans_io_core;
mod scratch_in_loop;
mod seqcst_unexplained;
mod shared_collection_lock;
mod small_map;
mod soa_candidate;
mod stable_sort_needless;
mod std_use;
#[cfg(test)]
mod std_use_tests;
mod stdout_lock_in_loop;
mod string_key;
mod struct_update;
mod syscall_in_loop;
#[cfg(test)]
mod tests;
mod thread_local_in_waiting_fn;
#[cfg(test)]
mod thread_local_tests;
mod unbounded_from_input;
mod unbounded_wait;
mod unbuffered_io;
mod unnamed_wait;
mod unrecorded_span;
mod untyped_error;
mod unwind_across_ffi;
#[cfg(test)]
mod waits_tests;
mod wide_index;
mod wildcard_arm;

use base_db::Db;
use blaze_diagnostics::lints::registry::ready;
use hir_def::{DefWithBodyId, FunctionId, HasModule as _, TraitId, hir::Expr};
use hir_ty::{ParamEnvAndCrate, db::HirDatabase as _, next_solver::DbInterner};
use intern::sym;
use syntax::{NodeOrToken, TextRange, view as ast};

use ast::AstNode as _;

use crate::{check::Cx, diag::Applicability};

macro_rules! docs {
    () => {
        "https://github.com/indexable-inc/blaze/blob/main/docs/lints.md"
    };
}

/// The page of the general lints, written for any workspace (`docs/lints.md`).
macro_rules! general_docs {
    () => {
        "https://github.com/indexable-inc/blaze/blob/main/docs/lints.md"
    };
}

macro_rules! parallel_docs {
    () => {
        "https://github.com/indexable-inc/blaze/blob/main/docs/lints.md"
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Lint {
    HotClone,
    HotAlloc,
    HotDyn,
    DefaultHasher,
    Parallelizable,
    SerialDependence,
    FiberBoundary,
    HotAllocCall,
    ErrorDebug,
    WallClock,
    UntypedError,
    DroppedError,
    HashOrderOutput,
    HeldAcrossWait,
    LockAcrossQuery,
    BlockingOnPool,
    UnnamedWait,
    SharedCollectionLock,
    OpenWaitLoop,
    UnrecordedSpan,
    ImplicitAllocator,
    FixedKeyLookup,
    StructUpdate,
    BoolParam,
    ManyBools,
    SansIoCore,
    KeyIgnoresField,
    WildcardArm,
    FalseSharing,
    SeqcstUnexplained,
    LargeMove,
    CollectThenConsume,
    HashTwice,
    SyscallInLoop,
    QueryInLoop,
    StringKey,
    OutlineGenericBody,
    ColdError,
    FrozenCollection,
    InternCandidate,
    SmallMap,
    ArcCloneOnlyBorrowed,
    BlockingInAsync,
    RelaxedPublish,
    UnwindAcrossFfi,
    UnboundedFromInput,
    DosHasherOnInput,
    PanicInDrop,
    BoundsCheckInLoop,
    UnbufferedIo,
    StdoutLockInLoop,
    FrontVecOps,
    MissingSizeHint,
    FmtAllocates,
    JaggedVec,
    StableSortNeedless,
    DynInHotField,
    ProfileHot,
    StaleHotMarker,
    HotMarkedCold,
    PushWithoutReserve,
    ScratchInLoop,
    BoxedRecursiveNode,
    WideIndex,
    BoundedHeapField,
    EnumVecPadded,
    DeepDropAtExit,
    DynFnParam,
    BoxLargeNew,
    PodSerializedByField,
    Checkfree,
    Nopanic,
    Noalloc,
    Nonblocking,
    Pure,
    Packable,
    SoaCandidate,
    BorrowWorkaround,
    NllWorkaround,
    NativeContracts,
    ThreadLocalInWaitingFn,
    LenientDeserialize,
    LockHeldAcrossSlow,
    UndeclaredEffect,
    UnneededEffect,
    NegativeEffect,
    DurableWrite,
    UnboundedWait,
    CombinedIdentity,
    ProcMacroDerive,
    ProcMacroAttribute,
    ProcMacroFnLike,
    BuildScriptAmbient,
    StdUse,
    ProgramLifetime,
    CouldBeProgramLifetime,
    CouldBeRef32,
}

/// Which workspaces a lint is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    /// Its rule names only std, core, alloc or widely used crates' types: on at warn in every
    /// workspace `blaze check` checks, the group `blaze::general`.
    General,
    /// Its rule needs blaze's own types or conventions (`blaze::hot`, `blaze_hash`, the
    /// scheduler, the query database): allowed until the workspace turns it, or the group
    /// `blaze::internal`, on under `[workspace.metadata.blaze.lints]`.
    Internal,
    /// Its advice trades something else for speed (a leak at exit, a byte layout the format
    /// depends on) or it can't see whether the code is hot: allowed until a workspace or crate
    /// turns it, or the group `blaze::pedantic_perf`, on.
    PedanticPerf,
    /// It flags what the blaze edition removes (docs/v2/plan/design/edition.md), here the code
    /// blaze cannot analyse and what blaze's standard library replaces (`std_use`): the group
    /// `blaze::edition`, allowed until a workspace or crate turns it on, but for a lint whose
    /// advice this blaze already has ([`Lint::warns_by_default`]), which warns everywhere.
    /// It asks for code written the way blaze's edition wants it (docs/v2/plan/design/edition.md):
    /// allowed until a crate turns it, or the group `blaze::edition`, on; a check of what the
    /// code declared (`program_lifetime`) is `deny` where it fires ([`DENY_BY_DEFAULT`]).
    Edition,
}

/// The lints that are `deny` by default rather than their kind's level: they fire only where
/// the code asked for them (`#[blaze::checkfree]`, `#[blaze::nopanic]`, `#[blaze::noalloc]`,
/// `#[blaze::nonblocking]`, `#[blaze::pure]`, and for `undeclared_effect` a `#[blaze::may(..)]`
/// on the function, its trait or a module around it, or `#![blaze::effects]`), so a finding is
/// a broken promise.
pub(crate) const DENY_BY_DEFAULT: &[Lint] = &[
    Lint::Checkfree,
    Lint::Nopanic,
    Lint::Noalloc,
    Lint::Nonblocking,
    Lint::Pure,
    Lint::UndeclaredEffect,
    // Fires only on a type declared `#[blaze::program_lifetime]`.
    Lint::ProgramLifetime,
];

/// Each lint of [`Kind::Edition`] with whether what its help tells code to move to is in this
/// blaze, so that it warns by default and not only where a workspace turns `blaze::edition`
/// on. `const fn` and `const` blocks are Rust's, so a function-like macro's lint warns now; the
/// others wait for one switch each (`blaze_lint_defs::ready`, which the compile's registry
/// reads for the same default).
pub(crate) const EDITION_READY: &[(Lint, bool)] = &[
    (Lint::ProcMacroDerive, ready::REFLECTION_REPLACES_MACROS),
    (Lint::ProcMacroAttribute, ready::REFLECTION_REPLACES_MACROS),
    (Lint::ProcMacroFnLike, true),
    (Lint::BuildScriptAmbient, ready::CAPABILITIES_FOR_BUILD_SCRIPTS),
    // Never by default, in any workspace: a crate asks for it (`#![blaze::capabilities]`), and
    // blaze's own workspace does once bstd is in the tree (its manifest's `lints-when-ready`
    // table and `ready::BSTD_IN_TREE`).
    (Lint::StdUse, false),
];

/// The group `blaze::proc_macro`: one lint per kind of procedural macro a workspace crate
/// defines, so each kind has its own default level and one name sets all three.
pub(crate) const PROC_MACRO: &[Lint] =
    &[Lint::ProcMacroDerive, Lint::ProcMacroAttribute, Lint::ProcMacroFnLike];

/// The group `blaze::hot`: every lint of hot code, a hasher nobody chose included
/// (`default_hasher`, which grew from `hot_lint.py`'s SipHash maps), and a value that leaves
/// its allocator to the default.
pub(crate) const HOT: &[Lint] = &[
    Lint::HotClone,
    Lint::HotAlloc,
    Lint::HotAllocCall,
    Lint::HotDyn,
    Lint::DefaultHasher,
    Lint::ImplicitAllocator,
    Lint::LargeMove,
    Lint::ColdError,
];

/// The lints a function the profile shows hot is linted with besides those on where it is
/// (`blaze::profile_hot`): the hot lints, which are `allow` outside hot code.
pub(crate) const PROFILE_RAISES: &[Lint] =
    &[Lint::HotClone, Lint::HotAlloc, Lint::HotAllocCall, Lint::HotDyn];

/// The perf lints whose findings in a profile-hot function say so, or are given
/// `blaze::profile_hot`'s level where their own is `allow` by default.
pub(crate) const PROFILE_NOTES: &[Lint] = &[
    Lint::HotClone,
    Lint::HotAlloc,
    Lint::HotAllocCall,
    Lint::HotDyn,
    Lint::LargeMove,
    Lint::ColdError,
    Lint::BoundsCheckInLoop,
    Lint::CollectThenConsume,
    Lint::HashTwice,
    Lint::UnbufferedIo,
    Lint::StdoutLockInLoop,
    Lint::FrontVecOps,
    Lint::FmtAllocates,
    Lint::StableSortNeedless,
    Lint::ArcCloneOnlyBorrowed,
    Lint::SyscallInLoop,
    Lint::QueryInLoop,
];

/// The general lints whose advice runs against what `blaze::internal` holds a workspace to:
/// allowed by default where the workspace turns `blaze::internal` on (`level::Levels`), so a
/// workspace that chose blaze's conventions is not told the opposite. `dos_hasher_on_input`
/// asks for a randomly seeded hasher where blaze keys every map by a fixed seed for
/// deterministic output (AGENTS.md rules 12 and 25). Set by the group's level, never by a lint's
/// name; a level set for the lint itself, or for `blaze::general`, still wins.
pub(crate) const YIELDS_TO_INTERNAL: &[Lint] = &[Lint::DosHasherOnInput];

/// The old names of renamed lints, after `blaze::`, and what each stands for now; the compile's
/// registry has them as renamed too (`blaze_lint_defs`' `RENAMED`).
pub(crate) const RENAMED: &[(&str, Lint)] = &[("std_hashmap", Lint::DefaultHasher)];

impl Lint {
    /// In discriminant order.
    pub(crate) const ALL: &[Lint] = &[
        Lint::HotClone,
        Lint::HotAlloc,
        Lint::HotDyn,
        Lint::DefaultHasher,
        Lint::Parallelizable,
        Lint::SerialDependence,
        Lint::FiberBoundary,
        Lint::HotAllocCall,
        Lint::ErrorDebug,
        Lint::WallClock,
        Lint::UntypedError,
        Lint::DroppedError,
        Lint::HashOrderOutput,
        Lint::HeldAcrossWait,
        Lint::LockAcrossQuery,
        Lint::BlockingOnPool,
        Lint::UnnamedWait,
        Lint::SharedCollectionLock,
        Lint::OpenWaitLoop,
        Lint::UnrecordedSpan,
        Lint::ImplicitAllocator,
        Lint::FixedKeyLookup,
        Lint::StructUpdate,
        Lint::BoolParam,
        Lint::ManyBools,
        Lint::SansIoCore,
        Lint::KeyIgnoresField,
        Lint::WildcardArm,
        Lint::FalseSharing,
        Lint::SeqcstUnexplained,
        Lint::LargeMove,
        Lint::CollectThenConsume,
        Lint::HashTwice,
        Lint::SyscallInLoop,
        Lint::QueryInLoop,
        Lint::StringKey,
        Lint::OutlineGenericBody,
        Lint::ColdError,
        Lint::FrozenCollection,
        Lint::InternCandidate,
        Lint::SmallMap,
        Lint::ArcCloneOnlyBorrowed,
        Lint::BlockingInAsync,
        Lint::RelaxedPublish,
        Lint::UnwindAcrossFfi,
        Lint::UnboundedFromInput,
        Lint::DosHasherOnInput,
        Lint::PanicInDrop,
        Lint::BoundsCheckInLoop,
        Lint::UnbufferedIo,
        Lint::StdoutLockInLoop,
        Lint::FrontVecOps,
        Lint::MissingSizeHint,
        Lint::FmtAllocates,
        Lint::JaggedVec,
        Lint::StableSortNeedless,
        Lint::DynInHotField,
        Lint::ProfileHot,
        Lint::StaleHotMarker,
        Lint::HotMarkedCold,
        Lint::PushWithoutReserve,
        Lint::ScratchInLoop,
        Lint::BoxedRecursiveNode,
        Lint::WideIndex,
        Lint::BoundedHeapField,
        Lint::EnumVecPadded,
        Lint::DeepDropAtExit,
        Lint::DynFnParam,
        Lint::BoxLargeNew,
        Lint::PodSerializedByField,
        Lint::Checkfree,
        Lint::Nopanic,
        Lint::Noalloc,
        Lint::Nonblocking,
        Lint::Pure,
        Lint::Packable,
        Lint::SoaCandidate,
        Lint::BorrowWorkaround,
        Lint::NllWorkaround,
        Lint::NativeContracts,
        Lint::ThreadLocalInWaitingFn,
        Lint::LenientDeserialize,
        Lint::LockHeldAcrossSlow,
        Lint::UndeclaredEffect,
        Lint::UnneededEffect,
        Lint::NegativeEffect,
        Lint::DurableWrite,
        Lint::UnboundedWait,
        Lint::CombinedIdentity,
        Lint::ProcMacroDerive,
        Lint::ProcMacroAttribute,
        Lint::ProcMacroFnLike,
        Lint::BuildScriptAmbient,
        Lint::StdUse,
        Lint::ProgramLifetime,
        Lint::CouldBeProgramLifetime,
        Lint::CouldBeRef32,
    ];

    /// Which workspaces the lint is for ([`Kind`]); written out per lint, so a new one chooses.
    pub(crate) const fn kind(self) -> Kind {
        match self {
            Lint::OutlineGenericBody
            | Lint::ColdError
            | Lint::FrozenCollection
            | Lint::CollectThenConsume
            | Lint::HashTwice
            | Lint::FalseSharing
            | Lint::SeqcstUnexplained
            | Lint::InternCandidate
            | Lint::SmallMap
            | Lint::ArcCloneOnlyBorrowed
            | Lint::BlockingInAsync
            | Lint::RelaxedPublish
            | Lint::UnwindAcrossFfi
            | Lint::UnboundedFromInput
            | Lint::DosHasherOnInput
            | Lint::PanicInDrop
            | Lint::BoundsCheckInLoop
            | Lint::UnbufferedIo
            | Lint::StdoutLockInLoop
            | Lint::FrontVecOps
            | Lint::MissingSizeHint
            | Lint::FmtAllocates
            | Lint::JaggedVec
            | Lint::StableSortNeedless
            | Lint::DynInHotField
            // Read a profile the workspace names, and are silent without one.
            | Lint::ProfileHot
            | Lint::StaleHotMarker
            | Lint::HotMarkedCold
            | Lint::PushWithoutReserve
            | Lint::ScratchInLoop
            | Lint::BoxedRecursiveNode
            | Lint::WideIndex
            | Lint::BoundedHeapField
            | Lint::EnumVecPadded
            | Lint::DynFnParam
            | Lint::BoxLargeNew
            // Fires only on `#[blaze::checkfree]` functions, `deny` there (`DENY_BY_DEFAULT`).
            | Lint::Checkfree
            // Fire only on functions marked with their attribute, `deny` there.
            | Lint::Nopanic
            | Lint::Noalloc
            | Lint::Nonblocking
            | Lint::Pure
            // Its fix passes stable Rust's borrow checker too, so it is right for any crate.
            | Lint::BorrowWorkaround
            // Fires only in a crate with `#![feature(blaze_verify)]`.
            | Lint::NativeContracts
            // Fire only where the code declares what a function may do (`#[blaze::may(..)]`, a
            // module's default, `#![blaze::effects]`): `undeclared_effect` is `deny` there.
            | Lint::UndeclaredEffect
            | Lint::UnneededEffect
            | Lint::NegativeEffect => Kind::General,
            // `packable`'s fix is a field with no address and a shift and mask per access,
            // and its `#[blaze::bits]` is an opt-in extension.
            Lint::DeepDropAtExit | Lint::PodSerializedByField | Lint::Packable | Lint::SoaCandidate => {
                Kind::PedanticPerf
            }
            // Code blaze cannot analyse, which the blaze edition removes; right for any
            // workspace once the replacement exists ([`Lint::warns_by_default`]).
            Lint::ProcMacroDerive
            | Lint::ProcMacroAttribute
            | Lint::ProcMacroFnLike
            | Lint::BuildScriptAmbient
            // What blaze's standard library replaces, which the edition's `std` no longer has.
            | Lint::StdUse => Kind::Edition,
            // `blaze::hot` and its comments (`// clone:`, `// alloc:`, `// dyn:`).
            Lint::HotClone
            | Lint::HotAlloc
            | Lint::HotAllocCall
            | Lint::HotDyn
            | Lint::LargeMove
            // `blaze_hash`, `blaze_mem`'s frames, `blaze_phf`, `intern::Symbol`.
            | Lint::DefaultHasher
            | Lint::ImplicitAllocator
            | Lint::FixedKeyLookup
            | Lint::StringKey
            // blaze's pool, scheduler, fibers and query database.
            | Lint::Parallelizable
            | Lint::SerialDependence
            | Lint::FiberBoundary
            | Lint::HeldAcrossWait
            | Lint::ThreadLocalInWaitingFn
            | Lint::LockAcrossQuery
            | Lint::LockHeldAcrossSlow
            | Lint::BlockingOnPool
            | Lint::UnnamedWait
            | Lint::SharedCollectionLock
            | Lint::OpenWaitLoop
            | Lint::QueryInLoop
            | Lint::SyscallInLoop
            | Lint::SansIoCore
            | Lint::WallClock
            // blaze's rules for errors, keys, output and spans (AGENTS.md 14, 16, 18, 22).
            | Lint::ErrorDebug
            | Lint::UntypedError
            | Lint::DroppedError
            | Lint::HashOrderOutput
            | Lint::KeyIgnoresField
            | Lint::UnrecordedSpan
            // Style rules blaze holds itself to; clippy keeps their like in pedantic or
            // restriction, off by default.
            | Lint::WildcardArm
            | Lint::StructUpdate
            | Lint::BoolParam
            | Lint::ManyBools
            // Its fix passes only blaze's (and nightly Rust's) borrow checker: on in workspaces
            // that turn on `internal`, allowed in the rest, so a published crate keeps building
            // with stable Rust.
            | Lint::NllWorkaround
            // AGENTS.md rule 3 (no fallback) for input: what a workspace reads must say what it
            // holds. Another crate may read a format that grows on purpose, so it opts in.
            | Lint::LenientDeserialize
            // Rule guard-the-cause's lints: a sync says what must survive, a wait with no
            // deadline names its waker, and hashes become an identity only in `blaze_ids::keyed`.
            | Lint::DurableWrite
            | Lint::UnboundedWait
            | Lint::CombinedIdentity => Kind::Internal,
            | Lint::LenientDeserialize => Kind::Internal,
            // docs/v2/plan/design/facts.md: the check of `#[blaze::program_lifetime]` and the
            // two lints that say where the attribute and `Ref32` would pay.
            Lint::ProgramLifetime | Lint::CouldBeProgramLifetime | Lint::CouldBeRef32 => {
                Kind::Edition
            }
        }
    }

    /// Whether a lint of [`Kind::Edition`] warns by default: what its help tells code to move
    /// to is in this blaze ([`EDITION_READY`]). `false` for a lint of another kind.
    pub(crate) const fn warns_by_default(self) -> bool {
        let mut at = 0;
        while at < EDITION_READY.len() {
            if EDITION_READY[at].0 as u8 == self as u8 {
                return EDITION_READY[at].1;
            }
            at += 1;
        }
        false
    }

    /// The lint `name`, after `blaze::`, stands for: its own, or the one it was renamed to
    /// ([`RENAMED`]).
    pub(crate) fn named(name: &str) -> Option<Lint> {
        let renamed = RENAMED.iter().find(|(old, _)| *old == name).map(|(_, new)| *new);
        Lint::ALL.iter().copied().find(|it| it.name() == name).or(renamed)
    }

    /// The name after `blaze::`.
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Lint::HotClone => "hot_clone",
            Lint::HotAlloc => "hot_alloc",
            Lint::HotDyn => "hot_dyn",
            Lint::DefaultHasher => "default_hasher",
            Lint::Parallelizable => "parallelizable",
            Lint::SerialDependence => "serial_dependence",
            Lint::FiberBoundary => "fiber_boundary",
            Lint::HotAllocCall => "hot_alloc_call",
            Lint::ErrorDebug => "error_debug",
            Lint::WallClock => "wall_clock",
            Lint::UntypedError => "untyped_error",
            Lint::DroppedError => "dropped_error",
            Lint::HashOrderOutput => "hash_order_output",
            Lint::HeldAcrossWait => "held_across_wait",
            Lint::LockAcrossQuery => "lock_across_query",
            Lint::BlockingOnPool => "blocking_on_pool",
            Lint::UnnamedWait => "unnamed_wait",
            Lint::SharedCollectionLock => "shared_collection_lock",
            Lint::OpenWaitLoop => "open_wait_loop",
            Lint::UnrecordedSpan => "unrecorded_span",
            Lint::ImplicitAllocator => "implicit_allocator",
            Lint::FixedKeyLookup => "fixed_key_lookup",
            Lint::StructUpdate => "struct_update",
            Lint::BoolParam => "bool_param",
            Lint::ManyBools => "many_bools",
            Lint::SansIoCore => "sans_io_core",
            Lint::KeyIgnoresField => "key_ignores_field",
            Lint::WildcardArm => "wildcard_arm",
            Lint::FalseSharing => "false_sharing",
            Lint::SeqcstUnexplained => "seqcst_unexplained",
            Lint::LargeMove => "large_move",
            Lint::CollectThenConsume => "collect_then_consume",
            Lint::HashTwice => "hash_twice",
            Lint::SyscallInLoop => "syscall_in_loop",
            Lint::QueryInLoop => "query_in_loop",
            Lint::StringKey => "string_key",
            Lint::OutlineGenericBody => "outline_generic_body",
            Lint::ColdError => "cold_error",
            Lint::FrozenCollection => "frozen_collection",
            Lint::InternCandidate => "intern_candidate",
            Lint::SmallMap => "small_map",
            Lint::ArcCloneOnlyBorrowed => "arc_clone_only_borrowed",
            Lint::BlockingInAsync => "blocking_in_async",
            Lint::RelaxedPublish => "relaxed_publish",
            Lint::UnwindAcrossFfi => "unwind_across_ffi",
            Lint::UnboundedFromInput => "unbounded_from_input",
            Lint::DosHasherOnInput => "dos_hasher_on_input",
            Lint::PanicInDrop => "panic_in_drop",
            Lint::BoundsCheckInLoop => "bounds_check_in_loop",
            Lint::UnbufferedIo => "unbuffered_io",
            Lint::StdoutLockInLoop => "stdout_lock_in_loop",
            Lint::FrontVecOps => "front_vec_ops",
            Lint::MissingSizeHint => "missing_size_hint",
            Lint::FmtAllocates => "fmt_allocates",
            Lint::JaggedVec => "jagged_vec",
            Lint::StableSortNeedless => "stable_sort_needless",
            Lint::DynInHotField => "dyn_in_hot_field",
            Lint::ProfileHot => "profile_hot",
            Lint::StaleHotMarker => "stale_hot_marker",
            Lint::HotMarkedCold => "hot_marked_cold",
            Lint::PushWithoutReserve => "push_without_reserve",
            Lint::ScratchInLoop => "scratch_in_loop",
            Lint::BoxedRecursiveNode => "boxed_recursive_node",
            Lint::WideIndex => "wide_index",
            Lint::BoundedHeapField => "bounded_heap_field",
            Lint::EnumVecPadded => "enum_vec_padded",
            Lint::DeepDropAtExit => "deep_drop_at_exit",
            Lint::DynFnParam => "dyn_fn_param",
            Lint::BoxLargeNew => "box_large_new",
            Lint::PodSerializedByField => "pod_serialized_by_field",
            Lint::Checkfree => "checkfree",
            Lint::Nopanic => "nopanic",
            Lint::Noalloc => "noalloc",
            Lint::Nonblocking => "nonblocking",
            Lint::Pure => "pure",
            Lint::Packable => "packable",
            Lint::SoaCandidate => "soa_candidate",
            Lint::BorrowWorkaround => "borrow_workaround",
            Lint::NllWorkaround => "nll_workaround",
            Lint::NativeContracts => "native_contracts",
            Lint::ThreadLocalInWaitingFn => "thread_local_in_waiting_fn",
            Lint::LenientDeserialize => "lenient_deserialize",
            Lint::LockHeldAcrossSlow => "lock_held_across_slow",
            Lint::UndeclaredEffect => "undeclared_effect",
            Lint::UnneededEffect => "unneeded_effect",
            Lint::NegativeEffect => "negative_effect",
            Lint::DurableWrite => "durable_write",
            Lint::UnboundedWait => "unbounded_wait",
            Lint::CombinedIdentity => "combined_identity",
            Lint::ProcMacroDerive => "proc_macro_derive",
            Lint::ProcMacroAttribute => "proc_macro_attribute",
            Lint::ProcMacroFnLike => "proc_macro_fn_like",
            Lint::BuildScriptAmbient => "build_script_ambient",
            Lint::StdUse => "std_use",
            Lint::ProgramLifetime => "program_lifetime",
            Lint::CouldBeProgramLifetime => "could_be_program_lifetime",
            Lint::CouldBeRef32 => "could_be_ref32",
        }
    }

    /// The lint's bit in [`crate::lints::On`], above every clippy lint's.
    pub(crate) const fn bit(self) -> crate::lints::On {
        crate::lints::On::bit(crate::lints::BLAZE_BITS + self as usize)
    }

    /// Where the lint is described, for the help line of each finding.
    pub(crate) const fn docs_url(self) -> &'static str {
        match self {
            Lint::HotClone => concat!(docs!(), "#hot_clone"),
            Lint::HotAlloc => concat!(docs!(), "#hot_alloc"),
            Lint::HotAllocCall => concat!(docs!(), "#hot_alloc_call"),
            Lint::HotDyn => concat!(docs!(), "#hot_dyn"),
            Lint::DefaultHasher => concat!(docs!(), "#default_hasher"),
            Lint::Parallelizable => concat!(parallel_docs!(), "#parallelizable"),
            Lint::SerialDependence => concat!(parallel_docs!(), "#serial_dependence"),
            Lint::ErrorDebug => concat!(docs!(), "#error_debug"),
            Lint::WallClock => concat!(docs!(), "#wall_clock"),
            Lint::UntypedError => concat!(docs!(), "#untyped_error"),
            Lint::DroppedError => concat!(docs!(), "#dropped_error"),
            Lint::HashOrderOutput => concat!(docs!(), "#hash_order_output"),
            Lint::HeldAcrossWait => concat!(docs!(), "#held_across_wait"),
            Lint::LockAcrossQuery => concat!(docs!(), "#lock_across_query"),
            Lint::BlockingOnPool => concat!(docs!(), "#blocking_on_pool"),
            Lint::UnnamedWait => concat!(docs!(), "#unnamed_wait"),
            Lint::SharedCollectionLock => concat!(docs!(), "#shared_collection_lock"),
            Lint::OpenWaitLoop => concat!(docs!(), "#open_wait_loop"),
            Lint::UnrecordedSpan => concat!(docs!(), "#unrecorded_span"),
            Lint::ImplicitAllocator => concat!(docs!(), "#implicit_allocator"),
            Lint::FixedKeyLookup => concat!(docs!(), "#fixed_key_lookup"),
            Lint::StructUpdate => concat!(docs!(), "#struct_update"),
            Lint::BoolParam => concat!(docs!(), "#bool_param"),
            Lint::ManyBools => concat!(docs!(), "#many_bools"),
            Lint::SansIoCore => concat!(docs!(), "#sans_io_core"),
            Lint::KeyIgnoresField => concat!(docs!(), "#key_ignores_field"),
            Lint::WildcardArm => concat!(docs!(), "#wildcard_arm"),
            Lint::FalseSharing => concat!(general_docs!(), "#false_sharing"),
            Lint::SeqcstUnexplained => concat!(general_docs!(), "#seqcst_unexplained"),
            Lint::LargeMove => concat!(docs!(), "#large_move"),
            Lint::CollectThenConsume => concat!(general_docs!(), "#collect_then_consume"),
            Lint::HashTwice => concat!(general_docs!(), "#hash_twice"),
            Lint::SyscallInLoop => concat!(docs!(), "#syscall_in_loop"),
            Lint::QueryInLoop => concat!(docs!(), "#query_in_loop"),
            Lint::StringKey => concat!(docs!(), "#string_key"),
            Lint::OutlineGenericBody => concat!(general_docs!(), "#outline_generic_body"),
            Lint::ColdError => concat!(general_docs!(), "#cold_error"),
            Lint::FrozenCollection => concat!(general_docs!(), "#frozen_collection"),
            Lint::InternCandidate => concat!(general_docs!(), "#intern_candidate"),
            Lint::SmallMap => concat!(general_docs!(), "#small_map"),
            Lint::ArcCloneOnlyBorrowed => concat!(general_docs!(), "#arc_clone_only_borrowed"),
            Lint::BlockingInAsync => concat!(general_docs!(), "#blocking_in_async"),
            Lint::RelaxedPublish => concat!(general_docs!(), "#relaxed_publish"),
            Lint::UnwindAcrossFfi => concat!(general_docs!(), "#unwind_across_ffi"),
            Lint::UnboundedFromInput => concat!(general_docs!(), "#unbounded_from_input"),
            Lint::DosHasherOnInput => concat!(general_docs!(), "#dos_hasher_on_input"),
            Lint::PanicInDrop => concat!(general_docs!(), "#panic_in_drop"),
            Lint::BoundsCheckInLoop => concat!(general_docs!(), "#bounds_check_in_loop"),
            Lint::UnbufferedIo => concat!(general_docs!(), "#unbuffered_io"),
            Lint::StdoutLockInLoop => concat!(general_docs!(), "#stdout_lock_in_loop"),
            Lint::FrontVecOps => concat!(general_docs!(), "#front_vec_ops"),
            Lint::MissingSizeHint => concat!(general_docs!(), "#missing_size_hint"),
            Lint::FmtAllocates => concat!(general_docs!(), "#fmt_allocates"),
            Lint::JaggedVec => concat!(general_docs!(), "#jagged_vec"),
            Lint::StableSortNeedless => concat!(general_docs!(), "#stable_sort_needless"),
            Lint::DynInHotField => concat!(general_docs!(), "#dyn_in_hot_field"),
            Lint::ProfileHot => concat!(general_docs!(), "#profile_hot"),
            Lint::StaleHotMarker => concat!(general_docs!(), "#stale_hot_marker"),
            Lint::HotMarkedCold => concat!(general_docs!(), "#hot_marked_cold"),
            Lint::PushWithoutReserve => concat!(general_docs!(), "#push_without_reserve"),
            Lint::ScratchInLoop => concat!(general_docs!(), "#scratch_in_loop"),
            Lint::BoxedRecursiveNode => concat!(general_docs!(), "#boxed_recursive_node"),
            Lint::WideIndex => concat!(general_docs!(), "#wide_index"),
            Lint::BoundedHeapField => concat!(general_docs!(), "#bounded_heap_field"),
            Lint::EnumVecPadded => concat!(general_docs!(), "#enum_vec_padded"),
            Lint::DeepDropAtExit => concat!(general_docs!(), "#deep_drop_at_exit"),
            Lint::DynFnParam => concat!(general_docs!(), "#dyn_fn_param"),
            Lint::BoxLargeNew => concat!(general_docs!(), "#box_large_new"),
            Lint::PodSerializedByField => concat!(general_docs!(), "#pod_serialized_by_field"),
            Lint::Checkfree => concat!(general_docs!(), "#checkfree"),
            Lint::Nopanic => concat!(general_docs!(), "#nopanic-noalloc-nonblocking"),
            Lint::Noalloc => concat!(general_docs!(), "#nopanic-noalloc-nonblocking"),
            Lint::Nonblocking => concat!(general_docs!(), "#nopanic-noalloc-nonblocking"),
            Lint::Pure => concat!(general_docs!(), "#pure"),
            Lint::Packable => concat!(general_docs!(), "#packable"),
            Lint::SoaCandidate => concat!(general_docs!(), "#soa_candidate"),
            Lint::BorrowWorkaround => concat!(general_docs!(), "#borrow_workaround"),
            Lint::NllWorkaround => concat!(general_docs!(), "#nll_workaround"),
            Lint::NativeContracts => concat!(general_docs!(), "#native_contracts"),
            Lint::ThreadLocalInWaitingFn => concat!(docs!(), "#thread_local_in_waiting_fn"),
            Lint::LenientDeserialize => concat!(docs!(), "#lenient_deserialize"),
            Lint::LockHeldAcrossSlow => concat!(docs!(), "#lock_held_across_slow"),
            Lint::UndeclaredEffect | Lint::UnneededEffect | Lint::NegativeEffect => {
                concat!(general_docs!(), "#undeclared_effect-unneeded_effect-negative_effect")
            }
            Lint::DurableWrite => concat!(docs!(), "#durable_write"),
            Lint::UnboundedWait => concat!(docs!(), "#unbounded_wait"),
            Lint::CombinedIdentity => concat!(docs!(), "#combined_identity"),
            Lint::ProcMacroDerive | Lint::ProcMacroAttribute | Lint::ProcMacroFnLike => {
                concat!(general_docs!(), "#proc_macro")
            }
            Lint::BuildScriptAmbient => concat!(general_docs!(), "#build_script_ambient"),
            Lint::StdUse => concat!(general_docs!(), "#std_use"),
            Lint::ProgramLifetime | Lint::CouldBeProgramLifetime | Lint::CouldBeRef32 => {
                concat!(general_docs!(), "#program_lifetime-could_be_program_lifetime-could_be_ref32")
            }
            Lint::FiberBoundary => {
                "https://github.com/indexable-inc/blaze/blob/main/docs/v2/plan.md"
            }
        }
    }

    /// The help the finding ends with, past the lint's own.
    const fn help(self) -> &'static str {
        match self {
            Lint::HotClone => "borrow it, copy an ID, or make the type not `Clone`",
            Lint::HotAlloc => {
                "write into a buffer the caller keeps, an arena, or intern it; format only at the edge"
            }
            Lint::HotAllocCall => {
                "keep the allocation out of what the hot path calls, or say why it is needed there with `// alloc: <why>`"
            }
            Lint::HotDyn => "use a concrete type, a generic, or a `match` on a tag",
            Lint::DefaultHasher => {
                "write the hasher out: `blaze_hash::HashMap` for `std::collections::HashMap` (and `HashSet`), `with_hasher(blaze_hash::FixedState)` for another crate's collection, or `#[expect(blaze::default_hasher, reason = \"..\")]` on a random seed kept for HashDoS"
            }
            Lint::Parallelizable => {
                "run the iterations with rayon's parallel iterators or blaze's pool"
            }
            Lint::SerialDependence => {
                "make the dependence a reduction to run the iterations in parallel, or keep it serial"
            }
            Lint::FiberBoundary => {
                "go through `blaze_graph::sched`'s own API (`sched::wait`, `sched::carry`, ...): fibers are the scheduler's alone"
            }
            Lint::ErrorDebug => {
                "name the item with `Display`, and render what needs the database into the error while it is attached"
            }
            Lint::WallClock => {
                "read time from the injected clock (`advance_clock(now)`) in a core, or say why this reads the wall clock with `#[expect(blaze::wall_clock, reason = \"..\")]`"
            }
            Lint::UntypedError => {
                "return an error enum of what can fail, each variant naming its item"
            }
            Lint::DroppedError => {
                "hand the error on with `?` or keep it as the new error's source, or say why with `#[expect(blaze::dropped_error, reason = \"..\")]`"
            }
            Lint::HashOrderOutput => {
                "sort what comes out, or keep it in a `BTreeMap` or an ordered `Vec`"
            }
            Lint::HeldAcrossWait => {
                "end the value before the call, or carry it with the job (`blaze_query::carry`) and read the thread-local again after the wait"
            }
            Lint::LockAcrossQuery => {
                "copy out what the query needs and drop the guard before the call, or make the locked state a query input"
            }
            Lint::LockHeldAcrossSlow => {
                "copy out what the call needs and drop the guard before it, then take the lock again to write the result; or move the state to an atomic, `papaya::HashMap` or one owner thread"
            }
            Lint::UndeclaredEffect => {
                "add the word to the function's `#[blaze::may(..)]` (`blaze check --fix` writes the list), handle the effect so the function no longer has it, or mark a callee proven by hand `#[blaze::may(.., trusted = \"why\")]` without the word"
            }
            Lint::UnneededEffect => {
                "take the word off the list (`blaze check --fix` does), or keep the right to the effect with `#[expect(blaze::unneeded_effect, reason = \"..\")]`"
            }
            Lint::NegativeEffect => {
                "write what the function may do, `#[blaze::may(..)]`, in place of what it may not (`blaze check --fix` rewrites it)"
            Lint::DurableWrite => {
                "take `Effect::WriteUnsynced` or `Effect::RenameUnsynced` where a build can make the data again and its reader checks it, or say what must survive a crash with `// durable: <why>` on its line or the line above"
            }
            Lint::UnboundedWait => {
                "wait to a `Deadline` through `blaze_core::wait` and end in a typed error when it passes, or name what always wakes this wait, and why it cannot end first, with `// wakes: <who>` on its line or the line above"
            }
            Lint::CombinedIdentity => {
                "keep the set in a `blaze_ids::keyed::Keyed` and take its root, or hash every entry with `blaze_ids::Hasher`; an XOR or a sum that makes no identity says so with `// identity: <why>` on its line or the line above"
            }
            Lint::BlockingOnPool => {
                "wait through the scheduler (`blaze_graph::sched::wait`, an `Event`, a `Wait::Key`), or move the blocking call to a driver thread"
            }
            Lint::UnnamedWait => {
                "name what is awaited (the key, the lock's data, the channel's message) where the wait is written"
            }
            Lint::SharedCollectionLock => {
                "share a map as `papaya::HashMap`; take the results of parallel items from `blaze_graph::par`, in order, not from a locked `Vec`"
            }
            Lint::OpenWaitLoop => {
                "count the rounds or take a deadline from the injected clock, compare it where the loop ends, and return an error naming what it waited for"
            }
            Lint::UnrecordedSpan => {
                "name `target: \"blaze\"` (`target = \"blaze\"` in `#[instrument]`), or make the span or event `debug` where it runs too often to record"
            }
            Lint::ImplicitAllocator => {
                "name the allocator inside `blaze_mem::frame::current(|frame| ..)` or with the request's `ArenaLocal`, or say why it needs the global heap with `// alloc: <why>` or `#[expect(blaze::implicit_allocator, reason = \"..\")]`"
            }
            Lint::FixedKeyLookup => {
                "put the keys in `const KEYS: &[&str]` and declare `blaze_phf::phf_table!(static TABLE = KEYS;)`; `TABLE.get(key)` (`TABLE.index(bytes)` for bytes) is the key's index into a const array of values. Or say why it stays with `#[expect(blaze::fixed_key_lookup, reason = \"..\")]`"
            }
            Lint::StructUpdate => {
                "write every field out, so a field added to the struct is a compile error at each literal until its value is chosen; or say why the base decides with `#[expect(blaze::struct_update, reason = \"..\")]`"
            }
            Lint::BoolParam => {
                "take a two-variant enum named for the choice instead of the `bool`, or say why it stays with `#[expect(blaze::bool_param, reason = \"..\")]`"
            }
            Lint::ManyBools => {
                "name the choices at the call: a record parameter with defaults (`#![blaze::records]`) or two-variant enums; or say why the call stays with `#[expect(blaze::many_bools, reason = \"..\")]`"
            }
            Lint::SansIoCore => {
                "take what the call reads as a parameter (`advance_clock(now)`) and hand back decisions (`take_decisions`, `next_deadline`); a driver injects IO through a trait object or a type parameter the core takes. Or say why with `#[expect(blaze::sans_io_core, reason = \"..\")]`"
            }
            Lint::KeyIgnoresField => {
                "destructure every field (`let Self { a, b, cache: _ } = self;`), each left out with `// not in key: <why>`, or say why with `#[expect(blaze::key_ignores_field, reason = \"..\")]`"
            }
            Lint::WildcardArm => {
                "list the variants the catch-all stands for, so a variant added to the enum is a compile error at this `match` until its arm is chosen; or say why the catch-all stays with `#[expect(blaze::wildcard_arm, reason = \"..\")]`"
            }
            Lint::FalseSharing => {
                "pad each field written apart to its own cache line (`#[repr(align(128))]` or `crossbeam_utils::CachePadded`), or say why they stay together with `#[expect(blaze::false_sharing, reason = \"read-mostly\")]`"
            }
            Lint::SeqcstUnexplained => {
                "take the weaker ordering the operation needs (a load `Acquire`, a store `Release`, a read-modify-write `AcqRel`), or say why it needs one total order with `// seqcst: <why>` on its line or the line above"
            }
            Lint::LargeMove => {
                "pass by reference or box the large variant, so a move copies a pointer"
            }
            Lint::CollectThenConsume => {
                "use the iterator where the collection is used, and drop the `collect` and its allocation"
            }
            Lint::HashTwice => {
                "look the key up once: the entry API (`entry(k).or_insert(v)`), the value `insert` or `remove` returns, or on papaya `get_or_insert_with` / `compute`"
            }
            Lint::SyscallInLoop => {
                "stat or list the paths as one batch: `blaze_core::Effect::Stamp { files, tag }` (one `Stamp` per name, `IORING_OP_STATX`s in flight together), `Effect::Entries { dir, tag }` (a directory's names with their stamps), or `Effect::Journal { dir, since, tag }` (what changed since a cursor)"
            }
            Lint::QueryInLoop => {
                "call the batch query once with every key, so the caller records one dependency edge instead of one per element"
            }
            Lint::StringKey => {
                "key the map by `intern::Symbol` (`Name::symbol()`, `Symbol::intern`): four bytes to hash and compare; `as_str()` gives the text back, `Symbol::cmp_order` an order for output"
            }
            Lint::OutlineGenericBody => {
                "convert at entry in the generic shell and call a non-generic `fn inner(..)` with the result, so the body is compiled once; or say why with `#[expect(blaze::outline_generic_body, reason = \"..\")]`"
            }
            Lint::ColdError => {
                "box a large error (or its large variant), and build an error in a `#[cold] #[inline(never)] fn` that returns it; or say why with `#[expect(blaze::cold_error, reason = \"..\")]`"
            }
            Lint::InternCandidate => {
                "intern it (hash-consing): keep each value once in an interner and pass a small `Copy` id, so `==` and hashing are one integer; `internment`, `lasso`, `hashconsing`, `id-arena` and salsa's `#[salsa::interned]` are interners in use. Or say why with `#[expect(blaze::intern_candidate, reason = \"..\")]`"
            }
            Lint::SmallMap => {
                "keep the few entries in a sorted array (`[(K, V); N]`, looked up with `binary_search_by_key` or a linear `iter().find`), or `match` on the key when every key is a constant; or say why with `#[expect(blaze::small_map, reason = \"..\")]`"
            }
            Lint::ArcCloneOnlyBorrowed => {
                "borrow through the `Arc` you have (`&*x`, or call the method on `x`); on an `ArcSwap`, `load()` gives a guard that borrows without the count. Or say why with `#[expect(blaze::arc_clone_only_borrowed, reason = \"..\")]`"
            }
            Lint::BlockingInAsync => {
                "use the runtime's async version (`tokio::fs`, `tokio::time::sleep`, `tokio::net`, an async channel), or move the blocking call into `spawn_blocking`; or say why with `#[expect(blaze::blocking_in_async, reason = \"..\")]`"
            }
            Lint::RelaxedPublish => {
                "store with `Ordering::Release` and load with `Ordering::Acquire`, so what was written before the store is visible after the load; or say why `Relaxed` is enough with `// relaxed: <why>` on the field or static"
            }
            Lint::UnwindAcrossFfi => {
                "catch the panic with `std::panic::catch_unwind` and return an error the caller understands, or use the `\"C-unwind\"` ABI when the caller can unwind"
            }
            Lint::UnboundedFromInput => {
                "bound it: a bounded channel (`std::sync::mpsc::sync_channel(n)`, `crossbeam_channel::bounded(n)`, `tokio::sync::mpsc::channel(n)`) that pushes back on the reader, or check the collection's `len()` against a cap in the loop and stop or drop past it"
            }
            Lint::DosHasherOnInput => {
                "key maps of caller-chosen text with `std::collections::hash_map::RandomState` (the default) or a hasher seeded at random per map; or say why with `#[expect(blaze::dos_hasher_on_input, reason = \"..\")]`"
            }
            Lint::PanicInDrop => {
                "handle the error in `drop` (log or ignore it, or give the type a `close` method that returns it), or skip the fallible work when `std::thread::panicking()` is true"
            }
            Lint::BoundsCheckInLoop => {
                "prove the bound once before the loop: reslice (`let xs = &xs[..n];`), `assert!(n <= xs.len());`, or iterate (`xs.iter()`, `zip`) instead of indexing; or say why with `#[expect(blaze::bounds_check_in_loop, reason = \"..\")]`"
            }
            Lint::UnbufferedIo => {
                "wrap the file or socket once in `std::io::BufWriter` (`BufReader` for reads) and make the calls on the wrapper, then `flush()` it; or say why each call must reach the system with `#[expect(blaze::unbuffered_io, reason = \"..\")]`"
            }
            Lint::StdoutLockInLoop => {
                "lock the stream once before the loop (`let mut out = std::io::stdout().lock();`, wrapped in a `BufWriter` for many lines) and `writeln!(out, ..)` inside it"
            }
            Lint::FrontVecOps => {
                "use `std::collections::VecDeque` (`pop_front`, `push_front`, `push_back`), or reverse the `Vec` once and work at its end with `pop` and `push`"
            }
            Lint::MissingSizeHint => {
                "implement `size_hint` returning the items left (`(n, Some(n))` when exact), and `ExactSizeIterator` when the count is exact"
            }
            Lint::FmtAllocates => {
                "write the parts into the formatter: `write!(f, \"..\", ..)`, `f.write_str(..)` or the value's own `fmt(f)`, with no `String` between"
            }
            Lint::JaggedVec => {
                "store every row's items in one flat `Vec<T>` (`String` for text) with a `Vec<u32>` of where each row starts; or say why with `#[expect(blaze::jagged_vec, reason = \"..\")]`"
            }
            Lint::StableSortNeedless => {
                "use `sort_unstable`, `sort_unstable_by` or `sort_unstable_by_key`; or say why the old order of equal elements matters with `#[expect(blaze::stable_sort_needless, reason = \"..\")]`"
            }
            Lint::ProfileHot => {
                "fix what the lint names in this function, which the profile shows running hot; or allow the lint here if the profile's workload is not the one that matters"
            }
            Lint::StaleHotMarker => {
                "take the marker off, so the optimizer and the hot lints spend their effort where the time goes; or profile the workload the marker is for and name that profile"
            }
            Lint::HotMarkedCold => {
                "take `#[cold]` / `#[inline(never)]` off: the optimizer lays this function out as unlikely and keeps it out of line, where the profile shows it is called often"
            }
            Lint::PushWithoutReserve => {
                "make it with `with_capacity(n)` for the loop's count, so it allocates once"
            }
            Lint::ScratchInLoop => {
                "make it once before the loop and `.clear()` it each round, so it keeps its capacity"
            }
            Lint::BoxedRecursiveNode => {
                "keep the nodes in a `Vec` (an arena) and link them by `u32` indices; or say why with `#[expect(blaze::boxed_recursive_node, reason = \"..\")]`"
            }
            Lint::WideIndex => {
                "store the index as a `u32` (or a newtype over one) and widen it with `as usize` where it indexes"
            }
            Lint::BoundedHeapField => {
                "hold it inline: an array (`[T; N]`, `arrayvec::ArrayVec`) or a small string (`arrayvec::ArrayString`, `smol_str`), sized for the bound"
            }
            Lint::EnumVecPadded => {
                "keep one `Vec` per variant, or the values packed as bytes, so each takes its own size; or say why with `#[expect(blaze::enum_vec_padded, reason = \"..\")]`"
            }
            Lint::DeepDropAtExit => {
                "`std::mem::forget` it at the end of `main`, or flush and `std::process::exit(0)`; the operating system frees it at once"
            }
            Lint::DynFnParam => {
                "take the closure as `impl Fn(..)` (a type parameter), so the call is direct and inlinable"
            }
            Lint::BoxLargeNew => {
                "build it in the heap: `vec![..].into_boxed_slice()`, `Box::new_uninit()` and a write, or `Box::default()`"
            }
            Lint::PodSerializedByField => {
                "derive `bytemuck::Pod` (or zerocopy's traits) and move slices of it as bytes"
            }
            Lint::Checkfree => {
                "prove the index or the arithmetic in range before the loop (`assert!(n <= v.len())`, `get`, `checked_*`, iterators), or take `#[blaze::checkfree]` off"
            }
            Lint::Nopanic => {
                "check the index or the arithmetic (`get`, `checked_*`, `wrapping_*`), mark the callee proven by hand `#[blaze::nopanic(trusted = \"why\")]`, or take `#[blaze::nopanic]` off"
            }
            Lint::Noalloc => {
                "allocate from the task's frame (`Vec::new_in(frame)`), mark the callee proven by hand `#[blaze::noalloc(trusted = \"why\")]`, or take `#[blaze::noalloc]` off"
            }
            Lint::Nonblocking => {
                "wait through the scheduler and do IO on a driver thread, mark the callee proven by hand `#[blaze::nonblocking(trusted = \"why\")]`, or take `#[blaze::nonblocking]` off"
            }
            Lint::Pure => {
                "read the outside world only through the arguments (pass the value in, take a type parameter for a `dyn` or a function pointer), mark a callee proven by hand `#[blaze::pure(trusted = \"why\")]`, or take `#[blaze::pure]` off"
            }
            Lint::Packable => {
                "mark it `#[blaze::bits]` to store its `bool`, enum and small-integer fields at their bit widths, after checking that nothing borrows a field (`&s.f`, `&mut s.f`): a packed field has no address; or say why with `#[expect(blaze::packable, reason = \"..\")]`"
            }
            Lint::BorrowWorkaround => {
                "look it up once, `if let Some(found) = map.get(&k)`, and use `found` in the branch; or say why with `#[expect(blaze::borrow_workaround, reason = \"..\")]`"
            }
            Lint::NllWorkaround => {
                "look it up once, `if let Some(found) = map.get_mut(&k)`, if the crate is built only by blaze (or nightly Rust); a crate that must build with stable Rust keeps the second lookup, with `#[expect(blaze::nll_workaround, reason = \"builds with stable Rust\")]`"
            }
            Lint::ThreadLocalInWaitingFn => {
                "read the thread-local in a small `#[inline(never)]` function that does not wait, and call that here (as `blaze_graph`'s `pool::with_caches` and `pool::local` do)"
            }
            Lint::LenientDeserialize => {
                "add `#[serde(deny_unknown_fields)]` under the derive (`blaze check --fix` does), so an unknown field is an error naming it; or say why the format must accept fields it doesn't know with `#[expect(blaze::lenient_deserialize, reason = \"..\")]`"
            }
            Lint::NativeContracts => {
                "write the contract as `requires`/`ensures` clauses between the signature and the body (`blaze check --fix` does), naming the return `-> (r: T)` for the `ensures`"
            }
            Lint::SoaCandidate => {
                "split the fields these loops touch into a struct of their own, kept in a `Vec` beside the rest, or store the collection as columns with a struct-of-arrays derive (`soa-rs`'s `#[derive(Soars)]` and `Soa<T>`, one slice per field; or `soa_derive`'s `#[derive(StructOfArray)]`); or say why with `#[expect(blaze::soa_candidate, reason = \"..\")]`"
            }
            Lint::DynInHotField => {
                "take the implementation as a type parameter (`struct S<F: Fn(..)>`) so the call is direct and inlinable, which costs one copy of the code per type used; or say why with `#[expect(blaze::dyn_in_hot_field, reason = \"..\")]`"
            }
            Lint::ProcMacroDerive => {
                if ready::REFLECTION_REPLACES_MACROS {
                    "write the derive as a generic impl over the type's fields by compile-time reflection: `for const f in fields_of::<T>()` reading `value.[f]`, bounded by `for<F: Field<T>> F::Type: Trait`, under `#![feature(blaze_comptime)]` (docs/v2/plan/design/comptime.md); or say why the macro stays with `#[expect(blaze::proc_macro_derive, reason = \"..\")]`"
                } else {
                    "nothing to change yet: the replacement is a generic impl over the type's fields by compile-time reflection (`for const f in fields_of::<T>()`), phase 2 of docs/v2/plan/design/comptime.md, which is behind the opt-in `#![feature(blaze_comptime)]` and not complete in this blaze; this lint is `allow` by default until it is"
                }
            }
            Lint::ProcMacroAttribute => {
                if ready::REFLECTION_REPLACES_MACROS {
                    "write what the attribute generates as a generic function over the item by compile-time reflection: its fields with `for const f in fields_of::<T>()`, a function's facts with `fn_info(&f)`, under `#![feature(blaze_comptime)]` (docs/v2/plan/design/comptime.md), called where the impls or the registration are needed; or say why the macro stays with `#[expect(blaze::proc_macro_attribute, reason = \"..\")]`"
                } else {
                    "nothing to change yet: the replacement is reflection over the item (`fields_of::<T>()`, `fn_info(&f)`) and a generic function that does what the generated impls or registrations do, phase 2 of docs/v2/plan/design/comptime.md, which is behind the opt-in `#![feature(blaze_comptime)]` and not complete in this blaze; this lint is `allow` by default until it is"
                }
            }
            Lint::ProcMacroFnLike => {
                "a macro that builds data at compile time is a `const fn` or a `const { .. }` block, which blaze checks once, analyses and caches as ordinary code (`core::mem::type_info` answers there, docs/v2/plan/design/comptime.md); a macro that parses a foreign syntax (a DSL in a string or in tokens) is not replaced by reflection and may stay: say so with `#[expect(blaze::proc_macro_fn_like, reason = \"parses ..\")]`"
            }
            Lint::BuildScriptAmbient => {
                if ready::CAPABILITIES_FOR_BUILD_SCRIPTS {
                    "take the capability as a parameter instead (`Fs`, `Env`, `Process`, `Net`, `Clock` of blaze's standard library, docs/v2/plan/design/std.md): the script's signature then states what it can touch, blaze proves what it reads, and reruns it only when that changes; or say why with `#[expect(blaze::build_script_ambient, reason = \"..\")]`"
                } else {
                    "nothing to change yet: the replacement is a capability the script is handed (`Fs`, `Env`, `Process`, `Net`, `Clock` of blaze's standard library, docs/v2/plan/design/std.md), which states in the script's signature what it can touch, lets blaze prove what it reads, and reruns it only when that changes; that library is not in this blaze, and this lint is `allow` by default until it is"
                }
            }
            Lint::StdUse => {
                if ready::BSTD_IN_TREE {
                    "take the capability, or write the form of blaze's standard library, that the finding names (bstd, docs/v2/plan/design/std.md): a function then says in its signature everything it can touch, wait for and allocate from; where the finding has a fix, `blaze check --fix` makes it; or say why std's stays with `#[expect(blaze::std_use, reason = \"..\")]`"
                } else {
                    "nothing to change yet: what the finding names is of blaze's standard library (bstd, docs/v2/plan/design/std.md), which is not in this blaze; this lint is `allow` by default, a crate asks for it with `#![blaze::capabilities]`, and blaze's own workspace has it at `warn` once bstd is in the tree"
                }
            Lint::ProgramLifetime => {
                "a `#[blaze::program_lifetime]` type has no parameter and no `Drop`, holds only plain data, `&'static` references and other program-lifetime types, and is not `pub` unless its crate has `#![blaze::program_lifetime_api]`: change the field or take the attribute off"
            }
            Lint::CouldBeProgramLifetime => {
                "mark the type `#[blaze::program_lifetime]` and allocate it in the program arena (`blaze_mem::ProgramArena::alloc`), which gives a 4-byte `Ref32<T>` with no count to touch; or say why its instances must be freed with `#[expect(blaze::could_be_program_lifetime, reason = \"..\")]`"
            }
            Lint::CouldBeRef32 => {
                "hold it as `blaze_mem::Ref32<T>` (4 bytes, `Copy`, derefs to `&'static T`; `Option<Ref32<T>>` is 4 bytes too), from `ProgramArena::alloc`; or say why with `#[expect(blaze::could_be_ref32, reason = \"..\")]`"
            }
            Lint::FrozenCollection => {
                "make the field a `Box<[T]>` (`Box<str>` for a string), built once with `into_boxed_slice()` / `into_boxed_str()`; or say why it stays growable with `#[expect(blaze::frozen_collection, reason = \"..\")]`"
            }
        }
    }
}

pub(crate) fn check_node<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if !cx.on_any(crate::lints::On::BLAZE) {
        return;
    }
    if let Some(ty) = ast::PathType::cast(node)
        && cx.on_blaze(Lint::DefaultHasher)
        && let Some(written) = hashmap::check_type(cx, ty)
        && let Some(range) = cx.range(node)
    {
        let span = cx.span(range);
        let fix = written.fix.map(|(at, to)| (cx.span(at), to.to_owned())); // alloc: the fix's text
        let diagnostic = cx.blaze(Lint::DefaultHasher, node, span, written.message);
        if let Some(part) = fix {
            // `MaybeIncorrect`: `HashMap::new()` and `with_capacity` are `RandomState`'s only, and
            // the type may meet a signature that names std's map; the uses change with it.
            // alloc: the fix's one part
            diagnostic.multipart_suggestion(
                "choose blaze's fixed-seed hasher",
                vec![part],
                Applicability::MaybeIncorrect,
            );
        }
        diagnostic
            .help(Lint::DefaultHasher.help())
            .help(format!("for further information visit {}", Lint::DefaultHasher.docs_url()));
    }
    // An `extern crate` is placed at the name it writes: the item's own range starts at the
    // comments the parser attaches to it.
    if cx.on_blaze(Lint::FiberBoundary)
        && let Some(message) = fiber::check(cx, node)
        && let Some(range) = cx.range(
            ast::ExternCrate::cast(node)
                .and_then(|it| it.name_ref())
                .map_or(node, |it| it.syntax()),
        )
    {
        let span = cx.span(range);
        cx.blaze(Lint::FiberBoundary, node, span, message)
            .help(Lint::FiberBoundary.help())
            .help(format!("for further information visit {}", Lint::FiberBoundary.docs_url()));
    }
    if cx.on_blaze(Lint::ImplicitAllocator)
        && let Some(ty) = ast::Type::cast(node)
    {
        implicit_allocator::check_type(cx, ty);
    }
    if let Some(adt) = ast::Adt::cast(node) {
        error_debug::check_attrs(cx, adt);
        lenient_deserialize::check(cx, adt);
        program_lifetime::check(cx, adt);
        could_be_program_lifetime::check(cx, adt);
        intern_candidate::check(cx, adt);
        boxed_recursive_node::check(cx, adt);
    }
    if let Some(f) = ast::Fn::cast(node) {
        untyped_error::check(cx, f);
        bool_param::check(cx, f);
        large_move::check_fn(cx, f);
        cold_error::check_fn(cx, f);
        profiled::check_fn(cx, f);
        native_contracts::check(cx, f);
        proc_macro::check(cx, f);
    }
    if let Some(item) = ast::Enum::cast(node) {
        large_move::check_enum(cx, item);
    }
    if let Some(item) = ast::Struct::cast(node) {
        false_sharing::check(cx, item);
        frozen_collection::check(cx, item);
        could_be_ref32::check(cx, item);
        small_map::check_struct(cx, item);
        jagged_vec::check(cx, item);
        dyn_in_hot_field::check(cx, item);
        wide_index::check(cx, item);
        bounded_heap_field::check(cx, item);
        enum_vec_padded::check(cx, item);
        pod_serialized_by_field::check(cx, item);
        packable::check(cx, item);
        soa_candidate::check(cx, item);
    }
    if let Some(item) = ast::Impl::cast(node) {
        missing_size_hint::check(cx, item);
    }
    if let Some(record) = ast::RecordExpr::cast(node) {
        struct_update::check(cx, record);
    }
    if let Some(call) = ast::CallExpr::cast(node) {
        many_bools::check(cx, node, ast::HasArgList::arg_list(&call));
    }
    if let Some(call) = ast::MethodCallExpr::cast(node) {
        many_bools::check(cx, node, ast::HasArgList::arg_list(&call));
    }
    if matches!(
        node.kind(),
        syntax::SyntaxKind::RECORD_FIELD
            | syntax::SyntaxKind::TUPLE_FIELD
            | syntax::SyntaxKind::STATIC
    ) {
        shared_collection_lock::check_declaration(cx, node);
    }
    unrecorded_span::check(cx, node);
    std_use::check_declaration(cx, node);
    if node == cx.root() && cx.in_file() {
        error_debug::check(cx);
        dropped_error::check(cx);
        hash_order_output::check(cx);
        held_across_wait::check(cx);
        thread_local_in_waiting_fn::check(cx);
        lock_across_query::check(cx);
        lock_held_across_slow::check(cx);
        blocking_on_pool::check(cx);
        unnamed_wait::check(cx);
        shared_collection_lock::check(cx);
        open_wait_loop::check(cx);
        fixed_key_lookup::check(cx);
        sans_io_core::check(cx);
        key_ignores_field::check(cx);
        wildcard_arm::check(cx);
        seqcst_unexplained::check(cx);
        durable_write::check(cx);
        unbounded_wait::check(cx);
        combined_identity::check(cx);
        large_move::check(cx);
        collect_then_consume::check(cx);
        hash_twice::check(cx);
        borrow_workaround::check(cx);
        syscall_in_loop::check(cx);
        query_in_loop::check(cx);
        string_key::check(cx);
        outline_generic_body::check(cx);
        cold_error::check(cx);
        small_map::check(cx);
        arc_clone_only_borrowed::check(cx);
        blocking_in_async::check(cx);
        relaxed_publish::check(cx);
        unwind_across_ffi::check(cx);
        unbounded_from_input::check(cx);
        dos_hasher_on_input::check(cx);
        panic_in_drop::check(cx);
        bounds_check_in_loop::check(cx);
        push_without_reserve::check(cx);
        scratch_in_loop::check(cx);
        deep_drop_at_exit::check(cx);
        dyn_fn_param::check(cx);
        box_large_new::check(cx);
        checkfree::check(cx);
        effects::check(cx);
        may::check(cx);
        unbuffered_io::check(cx);
        stdout_lock_in_loop::check(cx);
        front_vec_ops::check(cx);
        fmt_allocates::check(cx);
        stable_sort_needless::check(cx);
        build_script_ambient::check(cx);
        std_use::check(cx);
        check_body(cx);
    }
}

/// What a body's walk knows: the database, the body's environment, and the items its lints
/// compare against.
struct Known<'db> {
    db: &'db Db,
    interner: DbInterner<'db>,
    env: ParamEnvAndCrate<'db>,
    target: hir::DisplayTarget,
    /// `Clone` and its `clone`.
    clone: Option<(TraitId, FunctionId)>,
    std: hashmap::Std,
    alloc: alloc::Std,
    implicit: implicit_allocator::Allocators,
}

impl<'db> Known<'db> {
    fn new(cx: &Cx<'db, '_>, def: DefWithBodyId) -> Known<'db> {
        let db = cx.db();
        let clone = cx.lang_trait(hir::LangItem::Clone).and_then(|it| {
            let id = TraitId::from(it);
            let clone_fn =
                id.trait_items(db).method_by_name(&hir::Name::new_symbol_root(sym::clone));
            Some((id, clone_fn?))
        });
        Known {
            db,
            interner: DbInterner::new_no_crate(db),
            env: ParamEnvAndCrate {
                param_env: db.trait_environment(def.generic_def(db)),
                krate: def.krate(db),
            },
            target: cx.krate().to_display_target(db),
            clone,
            std: hashmap::Std::new(cx),
            alloc: alloc::Std::new(cx),
            implicit: implicit_allocator::Allocators::new(cx),
        }
    }
}

/// One finding of a body, before it is reported.
struct Finding {
    lint: Lint,
    range: TextRange,
    message: String,
    note: Option<String>,
}

fn check_body(cx: &mut Cx<'_, '_>) {
    parallel::check(cx);
    let on = |lint| cx.on_blaze(lint);
    let (on_clone, on_alloc, on_dyn, on_map) =
        (on(Lint::HotClone), on(Lint::HotAlloc), on(Lint::HotDyn), on(Lint::DefaultHasher));
    let on_alloc_call = on(Lint::HotAllocCall);
    let on_implicit = on(Lint::ImplicitAllocator);
    if !(on_clone || on_alloc || on_alloc_call || on_dyn || on_map || on_implicit) {
        return;
    }
    let mut found: Vec<Finding> = Vec::new();
    {
        let Some(analysis) = cx.analysis() else { return };
        let Ok(def) = DefWithBodyId::try_from(analysis.def()) else { return };
        let k = Known::new(cx, def);
        let db = k.db;
        let body = analysis.body();
        let infer = analysis.infer();
        let text = cx.text();
        let mut push = |lint, range, message, note| {
            if exempt::exempt(text, range, lint)
                || found.iter().any(|it: &Finding| it.lint == lint && it.range == range)
            {
                return;
            }
            found.push(Finding { lint, range, message, note });
        };
        for (id, expr) in body.exprs() {
            let (first, turbofish) = match expr {
                Expr::MethodCall { receiver, generic_args, .. } => {
                    (Some(receiver), generic_args.is_some())
                }
                Expr::Call { args, .. } => (args.first().copied(), false),
                _ => continue,
            };
            let Some(range) = analysis.expr_range(id) else { continue };
            let Some((callee, args)) = alloc::written_callee(infer, body, id) else { continue };
            let is_clone = clone::is_clone(&k, callee);
            if on_clone
                && is_clone
                && let Some(first) = first
            {
                let receiver = infer
                    .type_of_expr_with_adjust(first)
                    .unwrap_or_else(|| infer.expr_ty(first))
                    .strip_reference();
                if let Some((message, note)) = clone::finding(&k, &analysis.ty(receiver)) {
                    push(Lint::HotClone, range, message, note);
                }
            }
            if on_dyn && let Some(message) = dispatch::finding(&k, callee, args) {
                push(Lint::HotDyn, range, message, None);
            }
            let reaches = |through_workspace| {
                !is_clone
                    && alloc::instance_reaches(
                        db,
                        k.env,
                        alloc::resolve(db, k.env, callee, args),
                        through_workspace,
                    )
                    && !alloc::grows_kept_buffer(db, body, id, callee)
            };
            let call = || match macro_name(cx.snippet(range)) {
                Some(called) => format!("this `{called}!`"),
                None => format!("this call to `{}`", hir::Function::from(callee).name(db).as_str()),
            };
            if on_alloc && reaches(false) {
                push(
                    Lint::HotAlloc,
                    range,
                    format!("{} reaches the global allocator", call()),
                    None,
                );
            } else if on_alloc_call && reaches(true) && !reaches(false) {
                let message = format!(
                    "{} reaches the global allocator through the workspace's own code",
                    call()
                );
                let note = "`hot_alloc` finds the allocation in the function of the workspace that \
                            makes it; a hot function promises that nothing it calls allocates";
                push(Lint::HotAllocCall, range, message, Some(note.to_owned()));
            } else if on_alloc
                && !is_clone
                && let Some(made) = alloc::makes_global(&k, callee, infer.expr_ty(id))
            {
                let made = hir::Adt::from(made).name(db);
                let message =
                    format!("{} makes a `{}` of the global allocator", call(), made.as_str());
                let note = "it allocates from the global heap once it grows: name its allocator \
                            (`Vec::new_in(alloc)`), grow a buffer the caller keeps, or say why it \
                            needs the global heap with `// alloc: <why>`";
                push(Lint::HotAlloc, range, message, Some(note.to_owned()));
            }
            if on_map
                && let Some(message) = hashmap::check_construct(
                    &k,
                    || body_item(db, analysis),
                    callee,
                    infer.expr_ty(id),
                    turbofish,
                )
            {
                push(Lint::DefaultHasher, range, message, None);
            }
        }
        if on_implicit {
            implicit_allocator::check_body(cx, &k, analysis, &mut push);
        }
    }
    let root = cx.root();
    for finding in found {
        let at = match root.text_range().contains_range(finding.range) {
            true => match root.covering_element(finding.range) {
                NodeOrToken::Node(it) => it,
                NodeOrToken::Token(it) => it.parent().unwrap_or(root),
            },
            false => root,
        };
        let span = cx.span(finding.range);
        let diagnostic = cx.blaze(finding.lint, at, span, finding.message);
        if let Some(note) = finding.note {
            diagnostic.note(note);
        }
        diagnostic
            .help(finding.lint.help())
            .help(format!("for further information visit {}", finding.lint.docs_url()));
    }
}

/// How a message names the item whose body `analysis` is of.
fn body_item(db: &Db, analysis: &hir::BodyAnalysis<'_>) -> String {
    match analysis.def() {
        hir::DefWithBody::Function(f) => format!("`fn {}`", f.name(db).as_str()),
        _ => "this item".to_owned(), // alloc: a message's part
    }
}

/// The name of the macro `snippet` calls, when it is a macro call (`format!(..)`, `vec![..]`).
fn macro_name(snippet: &str) -> Option<&str> {
    let end = snippet
        .find(|it: char| !(it.is_alphanumeric() || it == '_' || it == ':'))
        .filter(|&it| it > 0)?;
    let called = &snippet[..end];
    snippet[end..].starts_with('!').then(|| called.rsplit("::").next().unwrap_or(called))
}
