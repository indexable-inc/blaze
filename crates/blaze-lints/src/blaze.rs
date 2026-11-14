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

mod absolute_position;
mod alloc;
mod arc_clone_only_borrowed;
mod as_cast;
mod async_fn;
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
mod cbor_field_order;
mod not_latest_edition;
mod checkfree;
mod claims;
mod clone;
mod cold_error;
mod could_be_program_lifetime;
mod could_be_ref32;
mod collect_then_consume;
mod combined_identity;
mod complexity;
mod construct;
#[cfg(test)]
mod correctness_general_tests;
mod deep_drop_at_exit;
mod default_policy;
mod dup;
mod dispatch;
mod dos_hasher_on_input;
mod dropped_error;
mod durable_write;
mod dyn_fn_param;
mod dyn_in_hot_field;
#[cfg(test)]
mod edition_tests;
mod effects;
#[cfg(test)]
mod effects_tests;
mod expect;
#[cfg(test)]
mod expect_tests;
mod enum_vec_padded;
mod env_read;
#[cfg(test)]
mod env_read_tests;
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
mod gapped_literal;
mod general_facts;
mod global_state;
mod goose_fields;
mod goose_locals;
#[cfg(test)]
mod goose_perf_tests;
mod hash_order_output;
mod hash_twice;
mod hashmap;
mod held_across_wait;
mod implicit_allocator;
mod implied_bound_written;
#[cfg(test)]
mod implied_bound_written_tests;
mod intern_candidate;
mod jagged_vec;
mod key_ignores_field;
mod large_move;
mod lenient_deserialize;
#[cfg(test)]
mod lenient_deserialize_tests;
mod lock_across_query;
mod lock_held_across_slow;
mod lock_order;
#[cfg(test)]
mod lock_order_tests;
mod loops;
mod implicit_overflow;
#[cfg(test)]
mod implicit_overflow_tests;
mod magic;
mod many_bools;
mod missing_size_hint;
mod mut_ref;
mod narrowed_len;
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
mod process_static;
#[cfg(test)]
mod profile_tests;
mod profiled;
mod program_lifetime;
mod proved_unreachable;
#[cfg(test)]
mod reach_tests;
mod refinement;
#[cfg(test)]
mod refinement_tests;
#[cfg(test)]
mod program_lifetime_tests;
mod push_without_reserve;
mod query_in_loop;
mod reads_positions;
mod relaxed_publish;
mod retry;
mod sans_io_core;
mod roles;
#[cfg(test)]
mod roles_tests;
mod scratch_in_loop;
mod seqcst_unexplained;
mod shared_collection_lock;
mod small_map;
mod soa_candidate;
mod span_field_unrecorded;
mod stable_sort_needless;
mod std_mistakes;
mod std_use;
#[cfg(test)]
mod std_use_tests;
mod stdout_lock_in_loop;
mod string_key;
mod suggest;
mod suspicious;
mod struct_update;
mod structural;
mod syscall_in_loop;
mod test_code;
mod test_strength;
#[cfg(test)]
mod tests;
mod thread_local_in_waiting_fn;
mod todo;
#[cfg(test)]
mod thread_local_tests;
mod tmp_path;
mod truncating_cast;
pub mod trust;
mod unbounded_from_input;
mod unbounded_test_wait;
mod unbounded_wait;
mod unbuffered_io;
mod unmarked_effects;
mod unnamed_wait;
mod unrecorded_span;
mod untied_child;
mod untyped_error;
mod unwind_across_ffi;
mod unwired;
#[cfg(test)]
mod waits_tests;
mod weight;
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
pub(crate) use std_mistakes::library_code;

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
    DefaultPolicy,
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
    Capabilities,
    Packable,
    SoaCandidate,
    BorrowWorkaround,
    NllWorkaround,
    NativeContracts,
    ImpliedBoundWritten,
    ThreadLocalInWaitingFn,
    LenientDeserialize,
    LockHeldAcrossSlow,
    AsCast,
    ImplicitOverflow,
    GlobalState,
    AsyncFn,
    UnmarkedPure,
    UnmarkedNopanic,
    UnmarkedNoalloc,
    UnmarkedNonblocking,
    MutRef,
    UndeclaredEffect,
    UnfulfilledEffect,
    NegativeEffect,
    DurableWrite,
    UnboundedWait,
    CombinedIdentity,
    ProcMacroDerive,
    ProcMacroAttribute,
    ProcMacroFnLike,
    BuildScriptAmbient,
    NotLatestEdition,
    StdUse,
    ProgramLifetime,
    CouldBeProgramLifetime,
    CouldBeRef32,
    SpanFieldUnrecorded,
    Unreachable,
    Refinement,
    Todo,
    EnvRead,
    UntiedChild,
    UnboundedTestWait,
    ReadsPositions,
    AbsolutePosition,
    CborFieldOrder,
    GappedLiteral,
    Retry,
    Unwired,
    TmpPath,
    NarrowedLen,
    ProcessStatic,
    // docs/v2/plan/design/effect-summary.md: the lints of the one effect summary.
    LoopTurnEffects,
    SyncOnAnswerPath,
    LockOrder,
    LockAcrossWait,
    // docs/lints.md#as_cast: the wrapping operations `as_cast`'s fix writes.
    TruncatingCast,
    // docs/lints.md#the-std-mistakes: std's API used in the ways people and LLMs get wrong.
    PartialWrite,
    PartialCmpUnwrap,
    UnboundedRead,
    LenMinus,
    StrByteSlice,
    Forget,
    PathToStrUnwrap,
    DetachedThread,
    UnboundedChannel,
    Transmute,
    CloneToSatisfy,
    UncheckedIndexInLoop,
    DefaultNumericFallbackLen,
    RefcellBorrowAcrossCall,
    ProcessExit,
    PrintInLib,
    LockUnwrap,
    // The suspicious-code lints (`suspicious`): what compiles and reads as right but leaves
    // something implicit that the next reader reconstructs wrong.
    SkippedPair,
    HalfDone,
    HiddenExit,
    QuestionMarkConverts,
    OptionQuestionMarkHides,
    UnseenTarget,
    GlobImport,
    ShadowChangesMeaning,
    VersionedName,
    UnusedParameter,
    Fallback,
    PlaceholderBody,
    TestAssertsNothing,
    AssertionCannotFail,
    TestMissesItsSubject,
    IgnoredWithoutReason,
    DocNamesNothing,
    // The magic literals (`magic`): a number or a name stated in place of its one home.
    MagicNumber,
    MagicString,
    RepeatedFormat,
    // The workspace pass's (`crates/blaze-findings`, from `magic::shared::merge_strings`): a
    // key-like string two sibling crates write, the same or nearly.
    StringSharedAcrossCrates,
    StringNearMissAcrossCrates,
    // Code that carries no weight (`weight`): a field nothing reads, a helper only tests call,
    // a trait with one impl, a function that only forwards, arguments in the wrong order.
    SwappedArguments,
    WriteOnlyField,
    OnlyUsedInTests,
    SingleImplTrait,
    ForwardingFn,
    ConstantArgument,
    AlwaysSameResult,
    NeverConstructed,
    NeverMatched,
    SwappableParameters,
    // How hard code is to hold in one's head (`complexity`): each measure over its bound, an
    // existing item held at its value (`baseline::items`).
    FunctionWeight,
    PathCount,
    LiveState,
    EffectBreadth,
    Exits,
    LongFunction,
    LongFile,
    ManyParams,
    WideType,
    DeepNesting,
    FanOut,
    ModuleCycle,
    // Correct by construction (`construct`): a check at run time, a convention or a name
    // standing in for a type.
    ProofForgotten,
    UnitMismatch,
    PrimitiveMeaning,
    SentinelValue,
    StateInFlags,
    BoolPairState,
    OutParameter,
    ParallelCollections,
    UntypedIndex,
    ValidatedButRaw,
    // A promise of a doc comment (`claims`, docs/v2/plan/design/claims.md): derived, checked,
    // or pointing at what proves it; a `SAFETY:` comment naming its invariant; a body
    // comment's claim and its checked form.
    ClaimContradictsFacts,
    ClaimIsDerivable,
    ClaimWithoutEvidence,
    EvidenceIsStale,
    SafetyWithoutInvariant,
    CommentClaims,
    // Duplicated code and copy-paste mistakes (`dup`), by the shape of code: locals numbered
    // by first use, everything else as written.
    SameFn,
    SameImplFn,
    SameBlock,
    NearDuplicate,
    PasteMismatch,
    SameType,
    SameConst,
    ExtractableDuplicate,
    TestsDifferByLiteral,
    ImplsDifferByType,
    // The workspace pass's (`crates/blaze-findings`, from `dup::index::merge_siblings`): a
    // function, constant or type two sibling crates each write.
    DuplicateAcrossCrates,
    // Test strength (`test_strength`): which parts of what a function hands back no test
    // observes, found without running anything.
    ResultPartUnobserved,
    WeakAssertion,
    MirrorAssertion,
    PseudoTested,
    UnreachedByTests,
    ErrorPathUntested,
    PropertyOverExamples,
    // Suggestions (`suggest`, the adoption ladder's second step): where a plain Rust project
    // gains from a blaze feature or a tighter type.
    CouldBeNarrower,
    CouldBeSansIo,
    CouldBeLinear,
    CouldHaveInvariant,
    // Code written by hand that the type's structure gives (`structural`): one analysis, is
    // this body uniform over the type's fields or variants, and the bugs of the one that is
    // nearly.
    HandWrittenDerivable,
    ImplMissesField,
    HandKeptVariantList,
    VariantListDrift,
    HandWrittenCodec,
    FieldWiseConversion,
    ConversionMissesField,
    HandWrittenVisitor,
    RepeatedFieldList,
    RegistrySpread,
    // Duplicated behaviour (`dup::semantic`): two functions whose canonical runtime MIR is one.
    SameBehaviour,
    // Trust (`trust`): what a dependency may do only when the workspace grants it.
    DependencyCapability,
    DependencyUnsafe,
    TrustGrantUnused,
    DependencyCapabilityUnreached,
    WorkspaceUnsafe,
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
    /// It flags what the blaze edition removes or respells, an effect it writes at a crate's
    /// border that the code lacks, code blaze cannot analyse and what blaze's standard library
    /// replaces (`std_use`), or asks for code written the way the edition wants it
    /// (docs/v2/plan/design/edition.md): the group `blaze::edition`, allowed until a workspace
    /// or crate turns it on to get ready for the edition, but for a lint whose advice this
    /// blaze already has ([`Lint::warns_by_default`]), which warns everywhere; a check of what
    /// the code declared (`program_lifetime`) is `deny` where it fires ([`DENY_BY_DEFAULT`]).
    Edition,
    /// Advice: where a plain Rust project gains from a blaze feature or a tighter type. Allowed
    /// until the adoption ladder's `suggest` step, or the group `blaze::suggest`, turns it on;
    /// never part of `blaze::edition`, which is what the edition changes. The effect lints
    /// (`unmarked_*`) are the edition's: the edition writes effects at a crate's border.
    Suggest,
}

/// The lints that are `deny` by default rather than their kind's level: they fire only where
/// the code asked for them (`#[blaze::checkfree]`, `#[blaze::nopanic]`, `#[blaze::noalloc]`,
/// `#[blaze::nonblocking]`, `#[blaze::pure]`, `#![blaze::capabilities]`, and for
/// `undeclared_effect` a `#[blaze::expect(..)]` on the function, its trait or a module around it,
/// or `#![blaze::effects]`, for `unfulfilled_effect` a function's own `#[blaze::expect(..)]`),
/// so a finding is a broken promise.
pub(crate) const DENY_BY_DEFAULT: &[Lint] = &[
    Lint::Checkfree,
    Lint::Nopanic,
    Lint::Noalloc,
    Lint::Nonblocking,
    Lint::Pure,
    Lint::Capabilities,
    Lint::UndeclaredEffect,
    // An expectation is exact: a listed word the function is proved not to have is refused.
    Lint::UnfulfilledEffect,
    // Fires only on a type declared `#[blaze::program_lifetime]`.
    Lint::ProgramLifetime,
    // Fires only on a `blaze::unreachable!()`.
    Lint::Unreachable,
    // Fires only where code states an invariant or a contract
    // (docs/v2/plan/design/refined-values.md).
    Lint::Refinement,
    // Fire only on a function marked with its role (docs/v2/plan/design/effect-summary.md).
    Lint::LoopTurnEffects,
    Lint::SyncOnAnswerPath,
    // A dependency doing what the workspace did not grant it: a compromised version stops the
    // build that pulls it in. Fires only where the driver gave the grants (`Conf::trust`).
    Lint::DependencyCapability,
    Lint::DependencyUnsafe,
];

/// The group `blaze::trust`: what a dependency may do only when the workspace grants it, and
/// a workspace crate's own `unsafe` ([`trust`]).
pub(crate) const TRUST: &[Lint] = &trust::LINTS;

/// The lints no level waives (docs/v2/plan/design/proved-unreachable.md): `deny` whatever an
/// attribute, a flag, a manifest table or `--cap-lints` says ([`crate::level::Levels::of`]).
/// An attribute that names one below `deny` is itself a finding of the lint, at the attribute
/// (`proved_unreachable::check_waiver`), and a flag that does refuses the crate's check
/// ([`crate::diag::CheckError::Unwaivable`]). A group or `warnings` that covers one does not
/// reach it and is no error: only naming the lint is.
pub(crate) const UNWAIVABLE: &[Lint] = &[Lint::Unreachable, Lint::Refinement];

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
    // Once a package of the edition builds wherever blaze runs (bstd ships with blaze).
    (Lint::NotLatestEdition, ready::EDITION_SHIPS),
    // Never by default, in any workspace: a crate asks for it (`#![blaze::capabilities]`), a
    // workspace by its `lints-when-ready` table once `ready::BSTD_IN_TREE`, and blaze's own
    // denies it in its `lints-ratchet` ahead of the switch.
    (Lint::StdUse, false),
    // What the edition removes or respells, and the marks it writes: a workspace getting ready
    // for the edition turns the group on.
    (Lint::AsCast, false),
    // Its advice (`checked_*`, `wrapping_*`, `try_from`) is Rust's; a workspace or crate turns
    // it on where arithmetic must mean one thing in every profile (`blaze-std`, `blaze-mem`).
    (Lint::ImplicitOverflow, false),
    (Lint::GlobalState, false),
    (Lint::AsyncFn, false),
    (Lint::UnmarkedPure, false),
    (Lint::UnmarkedNopanic, false),
    (Lint::UnmarkedNoalloc, false),
    (Lint::UnmarkedNonblocking, false),
    (Lint::MutRef, false),
    // docs/v2/plan/design/facts.md: `program_lifetime` checks what the code declared and is
    // `deny` where it fires ([`DENY_BY_DEFAULT`], which wins over this); the two that say where
    // the attribute and `Ref32` would pay are [`Kind::Suggest`]'s.
    (Lint::ProgramLifetime, false),
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
pub(crate) const RENAMED: &[(&str, Lint)] =
    &[("std_hashmap", Lint::DefaultHasher), ("lossy_as", Lint::AsCast)];

/// The `clippy.toml` key of the modules `env_read` allows, which the configuration's load
/// checks is a list of paths.
pub(crate) const ENV_READ_ALLOWED_KEY: &str = env_read::ALLOWED_KEY;

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
        Lint::DefaultPolicy,
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
        Lint::Capabilities,
        Lint::Packable,
        Lint::SoaCandidate,
        Lint::BorrowWorkaround,
        Lint::NllWorkaround,
        Lint::NativeContracts,
        Lint::ImpliedBoundWritten,
        Lint::ThreadLocalInWaitingFn,
        Lint::LenientDeserialize,
        Lint::LockHeldAcrossSlow,
        Lint::AsCast,
        Lint::ImplicitOverflow,
        Lint::GlobalState,
        Lint::AsyncFn,
        Lint::UnmarkedPure,
        Lint::UnmarkedNopanic,
        Lint::UnmarkedNoalloc,
        Lint::UnmarkedNonblocking,
        Lint::MutRef,
        Lint::UndeclaredEffect,
        Lint::UnfulfilledEffect,
        Lint::NegativeEffect,
        Lint::DurableWrite,
        Lint::UnboundedWait,
        Lint::CombinedIdentity,
        Lint::ProcMacroDerive,
        Lint::ProcMacroAttribute,
        Lint::ProcMacroFnLike,
        Lint::BuildScriptAmbient,
        Lint::NotLatestEdition,
        Lint::StdUse,
        Lint::ProgramLifetime,
        Lint::CouldBeProgramLifetime,
        Lint::CouldBeRef32,
        Lint::SpanFieldUnrecorded,
        Lint::Unreachable,
        Lint::Refinement,
        Lint::Todo,
        Lint::EnvRead,
        Lint::UntiedChild,
        Lint::UnboundedTestWait,
        Lint::ReadsPositions,
        Lint::AbsolutePosition,
        Lint::CborFieldOrder,
        Lint::GappedLiteral,
        Lint::Retry,
        Lint::Unwired,
        Lint::TmpPath,
        Lint::NarrowedLen,
        Lint::ProcessStatic,
        Lint::LoopTurnEffects,
        Lint::SyncOnAnswerPath,
        Lint::LockOrder,
        Lint::LockAcrossWait,
        Lint::TruncatingCast,
        Lint::PartialWrite,
        Lint::PartialCmpUnwrap,
        Lint::UnboundedRead,
        Lint::LenMinus,
        Lint::StrByteSlice,
        Lint::Forget,
        Lint::PathToStrUnwrap,
        Lint::DetachedThread,
        Lint::UnboundedChannel,
        Lint::Transmute,
        Lint::CloneToSatisfy,
        Lint::UncheckedIndexInLoop,
        Lint::DefaultNumericFallbackLen,
        Lint::RefcellBorrowAcrossCall,
        Lint::ProcessExit,
        Lint::PrintInLib,
        Lint::LockUnwrap,
        Lint::SkippedPair,
        Lint::HalfDone,
        Lint::HiddenExit,
        Lint::QuestionMarkConverts,
        Lint::OptionQuestionMarkHides,
        Lint::UnseenTarget,
        Lint::GlobImport,
        Lint::ShadowChangesMeaning,
        Lint::VersionedName,
        Lint::UnusedParameter,
        Lint::Fallback,
        Lint::PlaceholderBody,
        Lint::TestAssertsNothing,
        Lint::AssertionCannotFail,
        Lint::TestMissesItsSubject,
        Lint::IgnoredWithoutReason,
        Lint::DocNamesNothing,
        Lint::MagicNumber,
        Lint::MagicString,
        Lint::RepeatedFormat,
        Lint::StringSharedAcrossCrates,
        Lint::StringNearMissAcrossCrates,
        Lint::SwappedArguments,
        Lint::WriteOnlyField,
        Lint::OnlyUsedInTests,
        Lint::SingleImplTrait,
        Lint::ForwardingFn,
        Lint::ConstantArgument,
        Lint::AlwaysSameResult,
        Lint::NeverConstructed,
        Lint::NeverMatched,
        Lint::SwappableParameters,
        Lint::FunctionWeight,
        Lint::PathCount,
        Lint::LiveState,
        Lint::EffectBreadth,
        Lint::Exits,
        Lint::LongFunction,
        Lint::LongFile,
        Lint::ManyParams,
        Lint::WideType,
        Lint::DeepNesting,
        Lint::FanOut,
        Lint::ModuleCycle,
        Lint::ProofForgotten,
        Lint::UnitMismatch,
        Lint::PrimitiveMeaning,
        Lint::SentinelValue,
        Lint::StateInFlags,
        Lint::BoolPairState,
        Lint::OutParameter,
        Lint::ParallelCollections,
        Lint::UntypedIndex,
        Lint::ValidatedButRaw,
        Lint::ClaimContradictsFacts,
        Lint::ClaimIsDerivable,
        Lint::ClaimWithoutEvidence,
        Lint::EvidenceIsStale,
        Lint::SafetyWithoutInvariant,
        Lint::CommentClaims,
        Lint::SameFn,
        Lint::SameImplFn,
        Lint::SameBlock,
        Lint::NearDuplicate,
        Lint::PasteMismatch,
        Lint::SameType,
        Lint::SameConst,
        Lint::ExtractableDuplicate,
        Lint::TestsDifferByLiteral,
        Lint::ImplsDifferByType,
        Lint::DuplicateAcrossCrates,
        Lint::ResultPartUnobserved,
        Lint::WeakAssertion,
        Lint::MirrorAssertion,
        Lint::PseudoTested,
        Lint::UnreachedByTests,
        Lint::ErrorPathUntested,
        Lint::PropertyOverExamples,
        Lint::CouldBeNarrower,
        Lint::CouldBeSansIo,
        Lint::CouldBeLinear,
        Lint::CouldHaveInvariant,
        Lint::HandWrittenDerivable,
        Lint::ImplMissesField,
        Lint::HandKeptVariantList,
        Lint::VariantListDrift,
        Lint::HandWrittenCodec,
        Lint::FieldWiseConversion,
        Lint::ConversionMissesField,
        Lint::HandWrittenVisitor,
        Lint::RepeatedFieldList,
        Lint::RegistrySpread,
        Lint::SameBehaviour,
        Lint::DependencyCapability,
        Lint::DependencyUnsafe,
        Lint::TrustGrantUnused,
        Lint::DependencyCapabilityUnreached,
        Lint::WorkspaceUnsafe,
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
            // Fires only on a `blaze::unreachable!()`, `deny` there.
            | Lint::Unreachable
            // Fires only where code states an invariant or a contract, `deny` there.
            | Lint::Refinement
            // A hole left in code: a warning everywhere, an error where the gate denies it.
            | Lint::Todo
            // Fire only on functions marked with their attribute, `deny` there.
            | Lint::Nopanic
            | Lint::Noalloc
            | Lint::Nonblocking
            | Lint::Pure
            // Fire only on functions marked with their role (`#[blaze::bounded_turn]`,
            // `#[blaze::answers_request]`), `deny` there.
            | Lint::LoopTurnEffects
            | Lint::SyncOnAnswerPath
            // Fires only in a crate with `#![blaze::capabilities]`, `deny` there.
            | Lint::Capabilities
            // Its fix passes stable Rust's borrow checker too, so it is right for any crate.
            | Lint::BorrowWorkaround
            // Fires only in a crate with `#![feature(blaze_verify)]`.
            | Lint::NativeContracts
            // Fires only in a crate with `#![feature(blaze_implied_bounds)]`.
            | Lint::ImpliedBoundWritten
            // Fire only where the code declares what a function is expected to do
            // (`#[blaze::expect(..)]`, a module's default, `#![blaze::effects]`):
            // `undeclared_effect` and `unfulfilled_effect` are `deny` there.
            | Lint::UndeclaredEffect
            | Lint::UnfulfilledEffect
            | Lint::NegativeEffect => Kind::General,
            // `packable`'s fix is a field with no address and a shift and mask per access,
            // and its `#[blaze::bits]` is an opt-in extension.
            Lint::DeepDropAtExit | Lint::PodSerializedByField | Lint::Packable | Lint::SoaCandidate => {
                Kind::PedanticPerf
            }
            // What the blaze edition removes or respells; right for any crate getting ready
            // for it.
            Lint::AsCast | Lint::GlobalState | Lint::AsyncFn | Lint::MutRef => Kind::Edition,
            // Arithmetic whose meaning is the profile's, which the edition removes.
            Lint::ImplicitOverflow => Kind::Edition,
            // What the blaze edition writes at a crate's border: an effect the code already has.
            Lint::UnmarkedPure
            | Lint::UnmarkedNopanic
            | Lint::UnmarkedNoalloc
            | Lint::UnmarkedNonblocking => Kind::Edition,
            // Code blaze cannot analyse, which the blaze edition removes; right for any
            // workspace once the replacement exists ([`Lint::warns_by_default`]).
            Lint::ProcMacroDerive
            | Lint::ProcMacroAttribute
            | Lint::ProcMacroFnLike
            | Lint::BuildScriptAmbient
            // A package not on the latest blaze edition: the edition itself, asked for.
            | Lint::NotLatestEdition
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
            // Allowed by default: two locks the class can't tell apart (one type, handed in)
            // share a class, so a cycle between them may be false.
            | Lint::LockOrder
            | Lint::LockAcrossWait
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
            | Lint::DefaultPolicy
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
            | Lint::CombinedIdentity
            // Rule guard-the-cause for the profile: a span's field that reaches no column of
            // `spans` (`blaze_profile::span::ad_hoc_fields`) is refused where it is written.
            | Lint::SpanFieldUnrecorded
            // Rule keys-cover-every-input: the environment is read in the settings modules
            // `env-read-allowed` names, and nowhere else.
            | Lint::EnvRead
            // Rules bounded-progress and clean-up-lane for tests: a process a test starts ends
            // with it, and every wait in a test has a deadline.
            | Lint::UntiedChild
            | Lint::UnboundedTestWait
            // Rule keys-cover-every-input for the front end: a query reads positions only in a
            // function marked `#[blaze::renders_positions]`, and a key or record holds none.
            | Lint::ReadsPositions
            | Lint::AbsolutePosition
            // blaze-api's deterministic CBOR: a `#![blaze::canonical_cbor]` type declares its
            // fields in their keys' order.
            | Lint::CborFieldOrder
            // Faults seen in the tree, refused where written: a lost line continuation, a
            // retry, a design silenced instead of wired, a home under `/tmp`, a length
            // narrowed with a panic, and a static that grows for the process's life.
            | Lint::GappedLiteral
            | Lint::Retry
            | Lint::Unwired
            | Lint::TmpPath
            | Lint::NarrowedLen
            | Lint::ProcessStatic
            // The wrapping operations `as_cast`'s fix writes: a list of places to decide,
            // allowed in blaze's ratchet too.
            | Lint::TruncatingCast
            // The std mistakes: internal while each is proved on blaze's own tree.
            | Lint::PartialWrite
            | Lint::PartialCmpUnwrap
            | Lint::UnboundedRead
            | Lint::LenMinus
            | Lint::StrByteSlice
            | Lint::Forget
            | Lint::PathToStrUnwrap
            | Lint::DetachedThread
            | Lint::UnboundedChannel
            | Lint::Transmute
            | Lint::CloneToSatisfy
            | Lint::UncheckedIndexInLoop
            | Lint::DefaultNumericFallbackLen
            | Lint::RefcellBorrowAcrossCall
            | Lint::ProcessExit
            | Lint::PrintInLib
            | Lint::LockUnwrap
            | Lint::SkippedPair
            | Lint::HalfDone
            | Lint::HiddenExit
            | Lint::QuestionMarkConverts
            | Lint::OptionQuestionMarkHides
            | Lint::UnseenTarget
            | Lint::GlobImport
            | Lint::ShadowChangesMeaning
            | Lint::VersionedName
            | Lint::UnusedParameter
            | Lint::Fallback
            | Lint::PlaceholderBody
            | Lint::TestAssertsNothing
            | Lint::AssertionCannotFail
            | Lint::TestMissesItsSubject
            | Lint::IgnoredWithoutReason
            | Lint::DocNamesNothing
            | Lint::MagicNumber
            | Lint::MagicString
            | Lint::RepeatedFormat
            | Lint::StringSharedAcrossCrates
            | Lint::StringNearMissAcrossCrates
            | Lint::SwappedArguments
            | Lint::WriteOnlyField
            | Lint::OnlyUsedInTests
            | Lint::SingleImplTrait
            | Lint::ForwardingFn
            | Lint::ConstantArgument
            | Lint::AlwaysSameResult
            | Lint::NeverConstructed
            | Lint::NeverMatched
            | Lint::SwappableParameters
            | Lint::FunctionWeight
            | Lint::PathCount
            | Lint::LiveState
            | Lint::EffectBreadth
            | Lint::Exits
            | Lint::LongFunction
            | Lint::LongFile
            | Lint::ManyParams
            | Lint::WideType
            | Lint::DeepNesting
            | Lint::FanOut
            | Lint::ModuleCycle
            | Lint::ProofForgotten
            | Lint::UnitMismatch
            | Lint::PrimitiveMeaning
            | Lint::SentinelValue
            | Lint::StateInFlags
            | Lint::BoolPairState
            | Lint::OutParameter
            | Lint::ParallelCollections
            | Lint::UntypedIndex
            | Lint::ValidatedButRaw
            // Opt-in elsewhere, from the adoption ladder's `suggest` step.
            | Lint::ClaimContradictsFacts
            | Lint::ClaimIsDerivable
            | Lint::ClaimWithoutEvidence
            | Lint::EvidenceIsStale
            | Lint::SafetyWithoutInvariant
            | Lint::CommentClaims
            // Allowed elsewhere: what counts as one copy too many is the workspace's call.
            | Lint::SameFn
            | Lint::SameImplFn
            | Lint::SameBlock
            | Lint::NearDuplicate
            | Lint::PasteMismatch
            | Lint::SameType
            | Lint::SameConst
            | Lint::ExtractableDuplicate
            | Lint::TestsDifferByLiteral
            | Lint::ImplsDifferByType
            | Lint::DuplicateAcrossCrates
            // Allowed elsewhere: how strong a project's tests must be is its own call.
            | Lint::ResultPartUnobserved
            | Lint::WeakAssertion
            | Lint::MirrorAssertion
            | Lint::PseudoTested
            | Lint::UnreachedByTests
            | Lint::ErrorPathUntested
            | Lint::PropertyOverExamples
            // Allowed elsewhere: which hand-written impls and lists a project derives is its
            // call, from the adoption ladder's `suggest` step.
            | Lint::HandWrittenDerivable
            | Lint::ImplMissesField
            | Lint::HandKeptVariantList
            | Lint::VariantListDrift
            | Lint::HandWrittenCodec
            | Lint::FieldWiseConversion
            | Lint::ConversionMissesField
            | Lint::HandWrittenVisitor
            | Lint::RepeatedFieldList
            | Lint::RegistrySpread => Kind::Internal,
            // docs/v2/plan/design/facts.md: the check of `#[blaze::program_lifetime]` and the
            // two lints that say where the attribute and `Ref32` would pay.
            Lint::ProgramLifetime => Kind::Edition,
            // The suggestions: blaze features a plain Rust project adopts one step at a time,
            // off until the ladder's `suggest` step turns their group on; the two that say
            // where `#[blaze::program_lifetime]` and `Ref32` would pay among them.
            Lint::CouldBeProgramLifetime
            | Lint::CouldBeRef32
            | Lint::CouldBeNarrower
            | Lint::CouldBeSansIo
            | Lint::CouldBeLinear
            | Lint::CouldHaveInvariant => Kind::Suggest,
            // Allowed elsewhere, as the other duplicate lints: held by the ratchet baselines.
            Lint::SameBehaviour => Kind::Internal,
            // A dependency's capabilities are every workspace's concern: the two denials are
            // `deny` ([`DENY_BY_DEFAULT`], which wins over this), the other two warn. A
            // workspace crate's own `unsafe` is a policy blaze holds itself to by its ratchet.
            Lint::DependencyCapability
            | Lint::DependencyUnsafe
            | Lint::TrustGrantUnused
            | Lint::DependencyCapabilityUnreached => Kind::General,
            Lint::WorkspaceUnsafe => Kind::Internal,
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
            Lint::DefaultPolicy => "default_policy",
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
            Lint::Unreachable => "unreachable",
            Lint::Refinement => "refinement",
            Lint::Todo => "todo",
            Lint::Nopanic => "nopanic",
            Lint::Noalloc => "noalloc",
            Lint::Nonblocking => "nonblocking",
            Lint::Pure => "pure",
            Lint::Capabilities => "capabilities",
            Lint::Packable => "packable",
            Lint::SoaCandidate => "soa_candidate",
            Lint::BorrowWorkaround => "borrow_workaround",
            Lint::NllWorkaround => "nll_workaround",
            Lint::NativeContracts => "native_contracts",
            Lint::ImpliedBoundWritten => "implied_bound_written",
            Lint::ThreadLocalInWaitingFn => "thread_local_in_waiting_fn",
            Lint::LenientDeserialize => "lenient_deserialize",
            Lint::LockHeldAcrossSlow => "lock_held_across_slow",
            Lint::AsCast => "as_cast",
            Lint::ImplicitOverflow => "implicit_overflow",
            Lint::GlobalState => "global_state",
            Lint::AsyncFn => "async_fn",
            Lint::UnmarkedPure => "unmarked_pure",
            Lint::UnmarkedNopanic => "unmarked_nopanic",
            Lint::UnmarkedNoalloc => "unmarked_noalloc",
            Lint::UnmarkedNonblocking => "unmarked_nonblocking",
            Lint::MutRef => "mut_ref",
            Lint::UndeclaredEffect => "undeclared_effect",
            Lint::UnfulfilledEffect => "unfulfilled_effect",
            Lint::NegativeEffect => "negative_effect",
            Lint::DurableWrite => "durable_write",
            Lint::UnboundedWait => "unbounded_wait",
            Lint::CombinedIdentity => "combined_identity",
            Lint::ProcMacroDerive => "proc_macro_derive",
            Lint::ProcMacroAttribute => "proc_macro_attribute",
            Lint::ProcMacroFnLike => "proc_macro_fn_like",
            Lint::BuildScriptAmbient => "build_script_ambient",
            Lint::NotLatestEdition => "not_latest_edition",
            Lint::StdUse => "std_use",
            Lint::ProgramLifetime => "program_lifetime",
            Lint::CouldBeProgramLifetime => "could_be_program_lifetime",
            Lint::CouldBeRef32 => "could_be_ref32",
            Lint::SpanFieldUnrecorded => "span_field_unrecorded",
            Lint::EnvRead => "env_read",
            Lint::UntiedChild => "untied_child",
            Lint::UnboundedTestWait => "unbounded_test_wait",
            Lint::ReadsPositions => "reads_positions",
            Lint::AbsolutePosition => "absolute_position",
            Lint::CborFieldOrder => "cbor_field_order",
            Lint::GappedLiteral => "gapped_literal",
            Lint::Retry => "retry",
            Lint::Unwired => "unwired",
            Lint::TmpPath => "tmp_path",
            Lint::NarrowedLen => "narrowed_len",
            Lint::ProcessStatic => "process_static",
            Lint::LoopTurnEffects => "loop_turn_effects",
            Lint::SyncOnAnswerPath => "sync_on_answer_path",
            Lint::LockOrder => "lock_order",
            Lint::LockAcrossWait => "lock_across_wait",
            Lint::TruncatingCast => "truncating_cast",
            Lint::PartialWrite => "partial_write",
            Lint::PartialCmpUnwrap => "partial_cmp_unwrap",
            Lint::UnboundedRead => "unbounded_read",
            Lint::LenMinus => "len_minus",
            Lint::StrByteSlice => "str_byte_slice",
            Lint::Forget => "forget",
            Lint::PathToStrUnwrap => "path_to_str_unwrap",
            Lint::DetachedThread => "detached_thread",
            Lint::UnboundedChannel => "unbounded_channel",
            Lint::Transmute => "transmute",
            Lint::CloneToSatisfy => "clone_to_satisfy",
            Lint::UncheckedIndexInLoop => "unchecked_index_in_loop",
            Lint::DefaultNumericFallbackLen => "default_numeric_fallback_len",
            Lint::RefcellBorrowAcrossCall => "refcell_borrow_across_call",
            Lint::ProcessExit => "process_exit",
            Lint::PrintInLib => "print_in_lib",
            Lint::LockUnwrap => "lock_unwrap",
            Lint::SkippedPair => "skipped_pair",
            Lint::HalfDone => "half_done",
            Lint::HiddenExit => "hidden_exit",
            Lint::QuestionMarkConverts => "question_mark_converts",
            Lint::OptionQuestionMarkHides => "option_question_mark_hides",
            Lint::UnseenTarget => "unseen_target",
            Lint::GlobImport => "glob_import",
            Lint::ShadowChangesMeaning => "shadow_changes_meaning",
            Lint::VersionedName => "versioned_name",
            Lint::UnusedParameter => "unused_parameter",
            Lint::Fallback => "fallback",
            Lint::PlaceholderBody => "placeholder_body",
            Lint::TestAssertsNothing => "test_asserts_nothing",
            Lint::AssertionCannotFail => "assertion_cannot_fail",
            Lint::TestMissesItsSubject => "test_misses_its_subject",
            Lint::IgnoredWithoutReason => "ignored_without_reason",
            Lint::DocNamesNothing => "doc_names_nothing",
            Lint::MagicNumber => "magic_number",
            Lint::MagicString => "magic_string",
            Lint::RepeatedFormat => "repeated_format",
            Lint::StringSharedAcrossCrates => "string_shared_across_crates",
            Lint::StringNearMissAcrossCrates => "string_near_miss_across_crates",
            Lint::SwappedArguments => "swapped_arguments",
            Lint::WriteOnlyField => "write_only_field",
            Lint::OnlyUsedInTests => "only_used_in_tests",
            Lint::SingleImplTrait => "single_impl_trait",
            Lint::ForwardingFn => "forwarding_fn",
            Lint::ConstantArgument => "constant_argument",
            Lint::AlwaysSameResult => "always_same_result",
            Lint::NeverConstructed => "never_constructed",
            Lint::NeverMatched => "never_matched",
            Lint::SwappableParameters => "swappable_parameters",
            Lint::FunctionWeight => "function_weight",
            Lint::PathCount => "path_count",
            Lint::LiveState => "live_state",
            Lint::EffectBreadth => "effect_breadth",
            Lint::Exits => "exits",
            Lint::LongFunction => "long_function",
            Lint::LongFile => "long_file",
            Lint::ManyParams => "many_params",
            Lint::WideType => "wide_type",
            Lint::DeepNesting => "deep_nesting",
            Lint::FanOut => "fan_out",
            Lint::ModuleCycle => "module_cycle",
            Lint::ProofForgotten => "proof_forgotten",
            Lint::UnitMismatch => "unit_mismatch",
            Lint::PrimitiveMeaning => "primitive_meaning",
            Lint::SentinelValue => "sentinel_value",
            Lint::StateInFlags => "state_in_flags",
            Lint::BoolPairState => "bool_pair_state",
            Lint::OutParameter => "out_parameter",
            Lint::ParallelCollections => "parallel_collections",
            Lint::UntypedIndex => "untyped_index",
            Lint::ValidatedButRaw => "validated_but_raw",
            Lint::ClaimContradictsFacts => "claim_contradicts_facts",
            Lint::ClaimIsDerivable => "claim_is_derivable",
            Lint::ClaimWithoutEvidence => "claim_without_evidence",
            Lint::EvidenceIsStale => "evidence_is_stale",
            Lint::SafetyWithoutInvariant => "safety_without_invariant",
            Lint::CommentClaims => "comment_claims",
            Lint::SameFn => "same_fn",
            Lint::SameImplFn => "same_impl_fn",
            Lint::SameBlock => "same_block",
            Lint::NearDuplicate => "near_duplicate",
            Lint::PasteMismatch => "paste_mismatch",
            Lint::SameType => "same_type",
            Lint::SameConst => "same_const",
            Lint::ExtractableDuplicate => "extractable_duplicate",
            Lint::TestsDifferByLiteral => "tests_differ_by_literal",
            Lint::ImplsDifferByType => "impls_differ_by_type",
            Lint::DuplicateAcrossCrates => "duplicate_across_crates",
            Lint::ResultPartUnobserved => "result_part_unobserved",
            Lint::WeakAssertion => "weak_assertion",
            Lint::MirrorAssertion => "mirror_assertion",
            Lint::PseudoTested => "pseudo_tested",
            Lint::UnreachedByTests => "unreached_by_tests",
            Lint::ErrorPathUntested => "error_path_untested",
            Lint::PropertyOverExamples => "property_over_examples",
            Lint::CouldBeNarrower => "could_be_narrower",
            Lint::CouldBeSansIo => "could_be_sans_io",
            Lint::CouldBeLinear => "could_be_linear",
            Lint::CouldHaveInvariant => "could_have_invariant",
            Lint::HandWrittenDerivable => "hand_written_derivable",
            Lint::ImplMissesField => "impl_misses_field",
            Lint::HandKeptVariantList => "hand_kept_variant_list",
            Lint::VariantListDrift => "variant_list_drift",
            Lint::HandWrittenCodec => "hand_written_codec",
            Lint::FieldWiseConversion => "field_wise_conversion",
            Lint::ConversionMissesField => "conversion_misses_field",
            Lint::HandWrittenVisitor => "hand_written_visitor",
            Lint::RepeatedFieldList => "repeated_field_list",
            Lint::RegistrySpread => "registry_spread",
            Lint::SameBehaviour => "same_behaviour",
            Lint::DependencyCapability => "dependency_capability",
            Lint::DependencyUnsafe => "dependency_unsafe",
            Lint::TrustGrantUnused => "trust_grant_unused",
            Lint::DependencyCapabilityUnreached => "dependency_capability_unreached",
            Lint::WorkspaceUnsafe => "workspace_unsafe",
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
            Lint::DefaultPolicy => concat!(docs!(), "#default_policy"),
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
            Lint::Unreachable => concat!(general_docs!(), "#unreachable"),
            Lint::Refinement => concat!(general_docs!(), "#refinement"),
            Lint::Todo => concat!(general_docs!(), "#todo"),
            Lint::Nopanic => concat!(general_docs!(), "#nopanic-noalloc-nonblocking"),
            Lint::Noalloc => concat!(general_docs!(), "#nopanic-noalloc-nonblocking"),
            Lint::Nonblocking => concat!(general_docs!(), "#nopanic-noalloc-nonblocking"),
            Lint::Pure => concat!(general_docs!(), "#pure"),
            Lint::Capabilities => concat!(general_docs!(), "#capabilities"),
            Lint::Packable => concat!(general_docs!(), "#packable"),
            Lint::SoaCandidate => concat!(general_docs!(), "#soa_candidate"),
            Lint::BorrowWorkaround => concat!(general_docs!(), "#borrow_workaround"),
            Lint::NllWorkaround => concat!(general_docs!(), "#nll_workaround"),
            Lint::NativeContracts => concat!(general_docs!(), "#native_contracts"),
            Lint::ImpliedBoundWritten => concat!(general_docs!(), "#implied_bound_written"),
            Lint::ThreadLocalInWaitingFn => concat!(docs!(), "#thread_local_in_waiting_fn"),
            Lint::LenientDeserialize => concat!(docs!(), "#lenient_deserialize"),
            Lint::LockHeldAcrossSlow => concat!(docs!(), "#lock_held_across_slow"),
            Lint::AsCast => concat!(general_docs!(), "#as_cast"),
            Lint::ImplicitOverflow => concat!(general_docs!(), "#implicit_overflow"),
            Lint::GlobalState => concat!(general_docs!(), "#global_state"),
            Lint::AsyncFn => concat!(general_docs!(), "#async_fn"),
            Lint::UnmarkedPure
            | Lint::UnmarkedNopanic
            | Lint::UnmarkedNoalloc
            | Lint::UnmarkedNonblocking => concat!(general_docs!(), "#unmarked_pure-unmarked_nopanic-unmarked_noalloc-unmarked_nonblocking"),
            Lint::MutRef => concat!(general_docs!(), "#mut_ref"),
            Lint::UndeclaredEffect | Lint::UnfulfilledEffect | Lint::NegativeEffect => {
                concat!(general_docs!(), "#undeclared_effect-unfulfilled_effect-negative_effect")
            }
            Lint::DurableWrite => concat!(docs!(), "#durable_write"),
            Lint::UnboundedWait => concat!(docs!(), "#unbounded_wait"),
            Lint::CombinedIdentity => concat!(docs!(), "#combined_identity"),
            Lint::ProcMacroDerive | Lint::ProcMacroAttribute | Lint::ProcMacroFnLike => {
                concat!(general_docs!(), "#proc_macro")
            }
            Lint::BuildScriptAmbient => concat!(general_docs!(), "#build_script_ambient"),
            Lint::NotLatestEdition => concat!(general_docs!(), "#not_latest_edition"),
            Lint::StdUse => concat!(general_docs!(), "#std_use"),
            Lint::ProgramLifetime | Lint::CouldBeProgramLifetime | Lint::CouldBeRef32 => {
                concat!(general_docs!(), "#program_lifetime-could_be_program_lifetime-could_be_ref32")
            }
            Lint::SpanFieldUnrecorded => concat!(docs!(), "#span_field_unrecorded"),
            Lint::EnvRead => concat!(docs!(), "#env_read"),
            Lint::UntiedChild => concat!(docs!(), "#untied_child"),
            Lint::UnboundedTestWait => concat!(docs!(), "#unbounded_test_wait"),
            Lint::ReadsPositions => concat!(docs!(), "#reads_positions"),
            Lint::AbsolutePosition => concat!(docs!(), "#absolute_position"),
            Lint::CborFieldOrder => concat!(docs!(), "#cbor_field_order"),
            Lint::GappedLiteral => concat!(docs!(), "#gapped_literal"),
            Lint::Retry => concat!(docs!(), "#retry"),
            Lint::Unwired => concat!(docs!(), "#unwired"),
            Lint::TmpPath => concat!(docs!(), "#tmp_path"),
            Lint::NarrowedLen => concat!(docs!(), "#narrowed_len"),
            Lint::ProcessStatic => concat!(docs!(), "#process_static"),
            Lint::TruncatingCast => concat!(docs!(), "#truncating_cast"),
            Lint::PartialWrite => concat!(docs!(), "#partial_write"),
            Lint::PartialCmpUnwrap => concat!(docs!(), "#partial_cmp_unwrap"),
            Lint::UnboundedRead => concat!(docs!(), "#unbounded_read"),
            Lint::LenMinus => concat!(docs!(), "#len_minus"),
            Lint::StrByteSlice => concat!(docs!(), "#str_byte_slice"),
            Lint::Forget => concat!(docs!(), "#forget"),
            Lint::PathToStrUnwrap => concat!(docs!(), "#path_to_str_unwrap"),
            Lint::DetachedThread => concat!(docs!(), "#detached_thread"),
            Lint::UnboundedChannel => concat!(docs!(), "#unbounded_channel"),
            Lint::Transmute => concat!(docs!(), "#transmute"),
            Lint::CloneToSatisfy => concat!(docs!(), "#clone_to_satisfy"),
            Lint::UncheckedIndexInLoop => concat!(docs!(), "#unchecked_index_in_loop"),
            Lint::DefaultNumericFallbackLen => concat!(docs!(), "#default_numeric_fallback_len"),
            Lint::RefcellBorrowAcrossCall => concat!(docs!(), "#refcell_borrow_across_call"),
            Lint::ProcessExit => concat!(docs!(), "#process_exit"),
            Lint::PrintInLib => concat!(docs!(), "#print_in_lib"),
            Lint::LockUnwrap => concat!(docs!(), "#lock_unwrap"),
            Lint::SkippedPair => concat!(docs!(), "#skipped_pair"),
            Lint::HalfDone => concat!(docs!(), "#half_done"),
            Lint::HiddenExit => concat!(docs!(), "#hidden_exit"),
            Lint::QuestionMarkConverts => concat!(docs!(), "#question_mark_converts"),
            Lint::OptionQuestionMarkHides => concat!(docs!(), "#option_question_mark_hides"),
            Lint::UnseenTarget => concat!(docs!(), "#unseen_target"),
            Lint::GlobImport => concat!(docs!(), "#glob_import"),
            Lint::ShadowChangesMeaning => concat!(docs!(), "#shadow_changes_meaning"),
            Lint::VersionedName => concat!(docs!(), "#versioned_name"),
            Lint::UnusedParameter => concat!(docs!(), "#unused_parameter"),
            Lint::Fallback => concat!(docs!(), "#fallback"),
            Lint::PlaceholderBody => concat!(docs!(), "#placeholder_body"),
            Lint::TestAssertsNothing => concat!(docs!(), "#test_asserts_nothing"),
            Lint::AssertionCannotFail => concat!(docs!(), "#assertion_cannot_fail"),
            Lint::TestMissesItsSubject => concat!(docs!(), "#test_misses_its_subject"),
            Lint::IgnoredWithoutReason => concat!(docs!(), "#ignored_without_reason"),
            Lint::DocNamesNothing => concat!(docs!(), "#doc_names_nothing"),
            Lint::MagicNumber => concat!(docs!(), "#magic_number"),
            Lint::MagicString => concat!(docs!(), "#magic_string"),
            Lint::RepeatedFormat => concat!(docs!(), "#repeated_format"),
            Lint::StringSharedAcrossCrates => concat!(docs!(), "#string_shared_across_crates"),
            Lint::StringNearMissAcrossCrates => concat!(docs!(), "#string_near_miss_across_crates"),
            Lint::SwappedArguments => concat!(docs!(), "#swapped_arguments"),
            Lint::WriteOnlyField => concat!(docs!(), "#write_only_field"),
            Lint::OnlyUsedInTests => concat!(docs!(), "#only_used_in_tests"),
            Lint::SingleImplTrait => concat!(docs!(), "#single_impl_trait"),
            Lint::ForwardingFn => concat!(docs!(), "#forwarding_fn"),
            Lint::ConstantArgument => concat!(docs!(), "#constant_argument"),
            Lint::AlwaysSameResult => concat!(docs!(), "#always_same_result"),
            Lint::NeverConstructed => concat!(docs!(), "#never_constructed-never_matched"),
            Lint::NeverMatched => concat!(docs!(), "#never_constructed-never_matched"),
            Lint::SwappableParameters => concat!(docs!(), "#swappable_parameters"),
            Lint::FunctionWeight => concat!(docs!(), "#function_weight"),
            Lint::PathCount => concat!(docs!(), "#path_count"),
            Lint::LiveState => concat!(docs!(), "#live_state"),
            Lint::EffectBreadth => concat!(docs!(), "#effect_breadth"),
            Lint::Exits => concat!(docs!(), "#exits"),
            Lint::LongFunction => concat!(docs!(), "#long_function"),
            Lint::LongFile => concat!(docs!(), "#long_file"),
            Lint::ManyParams => concat!(docs!(), "#many_params"),
            Lint::WideType => concat!(docs!(), "#wide_type"),
            Lint::DeepNesting => concat!(docs!(), "#deep_nesting"),
            Lint::FanOut => concat!(docs!(), "#fan_out"),
            Lint::ModuleCycle => concat!(docs!(), "#module_cycle"),
            Lint::ProofForgotten => concat!(docs!(), "#proof_forgotten"),
            Lint::UnitMismatch => concat!(docs!(), "#unit_mismatch-primitive_meaning"),
            Lint::PrimitiveMeaning => concat!(docs!(), "#unit_mismatch-primitive_meaning"),
            Lint::SentinelValue => concat!(docs!(), "#sentinel_value"),
            Lint::StateInFlags => concat!(docs!(), "#state_in_flags-bool_pair_state"),
            Lint::BoolPairState => concat!(docs!(), "#state_in_flags-bool_pair_state"),
            Lint::OutParameter => concat!(docs!(), "#out_parameter"),
            Lint::ParallelCollections => concat!(docs!(), "#parallel_collections"),
            Lint::UntypedIndex => concat!(docs!(), "#untyped_index"),
            Lint::ValidatedButRaw => concat!(docs!(), "#validated_but_raw"),
            Lint::ClaimContradictsFacts => concat!(docs!(), "#claim_contradicts_facts"),
            Lint::ClaimIsDerivable => concat!(docs!(), "#claim_is_derivable"),
            Lint::ClaimWithoutEvidence => concat!(docs!(), "#claim_without_evidence"),
            Lint::EvidenceIsStale => concat!(docs!(), "#evidence_is_stale"),
            Lint::SafetyWithoutInvariant => concat!(docs!(), "#safety_without_invariant"),
            Lint::CommentClaims => concat!(docs!(), "#comment_claims"),
            Lint::SameFn => concat!(docs!(), "#same_fn-same_impl_fn"),
            Lint::SameImplFn => concat!(docs!(), "#same_fn-same_impl_fn"),
            Lint::SameBlock => concat!(docs!(), "#same_block"),
            Lint::NearDuplicate => concat!(docs!(), "#near_duplicate"),
            Lint::PasteMismatch => concat!(docs!(), "#paste_mismatch"),
            Lint::SameType => concat!(docs!(), "#same_type-same_const"),
            Lint::SameConst => concat!(docs!(), "#same_type-same_const"),
            Lint::ExtractableDuplicate => concat!(docs!(), "#extractable_duplicate"),
            Lint::TestsDifferByLiteral => concat!(docs!(), "#tests_differ_by_literal"),
            Lint::ImplsDifferByType => concat!(docs!(), "#impls_differ_by_type"),
            Lint::DuplicateAcrossCrates => concat!(docs!(), "#duplicate_across_crates"),
            Lint::ResultPartUnobserved => concat!(docs!(), "#result_part_unobserved"),
            Lint::WeakAssertion => concat!(docs!(), "#weak_assertion"),
            Lint::MirrorAssertion => concat!(docs!(), "#mirror_assertion"),
            Lint::PseudoTested => concat!(docs!(), "#pseudo_tested"),
            Lint::UnreachedByTests => concat!(docs!(), "#unreached_by_tests"),
            Lint::ErrorPathUntested => concat!(docs!(), "#error_path_untested"),
            Lint::PropertyOverExamples => concat!(docs!(), "#property_over_examples"),
            Lint::CouldBeNarrower => concat!(docs!(), "#could_be_narrower"),
            Lint::CouldBeSansIo => concat!(docs!(), "#could_be_sans_io"),
            Lint::CouldBeLinear => concat!(docs!(), "#could_be_linear"),
            Lint::CouldHaveInvariant => concat!(docs!(), "#could_have_invariant"),
            Lint::SameBehaviour => concat!(docs!(), "#same_behaviour"),
            Lint::HandWrittenDerivable => concat!(docs!(), "#hand_written_derivable"),
            Lint::ImplMissesField => concat!(docs!(), "#impl_misses_field"),
            Lint::HandKeptVariantList | Lint::VariantListDrift => {
                concat!(docs!(), "#hand_kept_variant_list-variant_list_drift")
            }
            Lint::HandWrittenCodec => concat!(docs!(), "#hand_written_codec"),
            Lint::FieldWiseConversion | Lint::ConversionMissesField => {
                concat!(docs!(), "#field_wise_conversion-conversion_misses_field")
            }
            Lint::HandWrittenVisitor => concat!(docs!(), "#hand_written_visitor"),
            Lint::RepeatedFieldList => concat!(docs!(), "#repeated_field_list"),
            Lint::RegistrySpread => concat!(docs!(), "#registry_spread"),
            Lint::LoopTurnEffects | Lint::SyncOnAnswerPath => {
                concat!(general_docs!(), "#loop_turn_effects-sync_on_answer_path")
            }
            Lint::LockOrder | Lint::LockAcrossWait => {
                concat!(docs!(), "#lock_order-lock_across_wait")
            }
            Lint::FiberBoundary => {
                "https://github.com/indexable-inc/blaze/blob/main/docs/v2/plan.md"
            }
            Lint::DependencyCapability => concat!(general_docs!(), "#dependency_capability"),
            Lint::DependencyUnsafe => concat!(general_docs!(), "#dependency_unsafe"),
            Lint::TrustGrantUnused => concat!(general_docs!(), "#trust_grant_unused"),
            Lint::DependencyCapabilityUnreached => {
                concat!(general_docs!(), "#dependency_capability_unreached")
            }
            Lint::WorkspaceUnsafe => concat!(docs!(), "#workspace_unsafe"),
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
                "keep the allocation out of what the hot path calls: don't allocate (a borrow, a fixed buffer, the caller's buffer), or allocate in the task's frame; where one must stay, say why with `#[expect(blaze::hot_alloc, reason = \"..\")]`"
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
                "add the word to the function's `#[blaze::expect(..)]` (`blaze check --fix` writes the list), handle the effect so the function no longer has it, or mark a callee proven by hand `#[blaze::expect(.., trusted = \"why\")]` without the word"
            }
            Lint::UnfulfilledEffect => {
                "take the word off the list (`blaze check --fix` does): a function's own list is exact; a right kept for impls to come is a trait method's list, which bounds them"
            }
            Lint::NegativeEffect => {
                "write what the function may do, `#[blaze::expect(..)]`, in place of what it may not (`blaze check --fix` rewrites it)"
            }
            Lint::DurableWrite => {
                "take `Effect::WriteUnsynced` or `Effect::RenameUnsynced` where a build can make the data again and its reader checks it, or say what must survive a crash with `// durable: <why>` on its line or the line above"
            }
            Lint::UnboundedWait => {
                "wait to a `Deadline` through `blaze_core::wait` and end in a typed error when it passes, or name what always wakes this wait, and why it cannot end first, with `// wakes: <who>` on its line or the line above"
            }
            Lint::CombinedIdentity => {
                "keep the set in a `blaze_ids::keyed::Keyed` and take its root, or hash every entry with `blaze_ids::Hasher`; an XOR or a sum that makes no identity says so with `// identity: <why>` on its line or the line above"
            }
            Lint::EnvRead => {
                "read the setting in a settings module (a module `env-read-allowed` in clippy.toml names), typed, with no default for a missing value, and hand it to this code; a process that hands its whole environment on (a client to its daemon, a runner to a child) says so with `#[expect(blaze::env_read, reason = \"..\")]`"
            }
            Lint::UntiedChild => {
                "own the child in `stdx::JodChild`, which kills and reaps it on drop (`stdx::JodChild::spawn(command)`, or `.spawn().map(stdx::JodChild)`; a struct's field holds a `JodChild`, not a `Child` with a hand-written `Drop`), and set `BLAZE_DAEMON_EXIT_UNHELD=on` on every command that runs `blaze`; or `#[expect(blaze::untied_child, reason = \"..\")]` naming what ends the process instead"
            }
            Lint::UnboundedTestWait => {
                "in a test, wait to a deadline and fail naming what was awaited: `rx.recv_timeout(WAIT).expect(\"<what>\")`, `stdx::child::exit_within(&mut child, \"<what>\", WAIT)` (or `stdx::child::output_within`) for a child, `park_timeout` or `Condvar::wait_timeout`/`Signal::wait_for` in a loop that compares `elapsed()`, a `blaze_core::wait` call given `Some(deadline)`, and a `try_wait` loop that compares a deadline or a round count; where something the lint cannot see bounds the wait, `#[expect(blaze::unbounded_test_wait, reason = \"<what bounds it>\")]`"
            }
            Lint::ReadsPositions => {
                "read the position-free fact instead (the body, `ExpressionStore::is_synthesized`, the outline, the item index), or mark the function `#[blaze::renders_positions]` when what it makes is about where code is written (a diagnostic's words, debug info, an IDE lookup); else `#[expect(blaze::reads_positions, reason = \"<what must change>\")]`"
            }
            Lint::AbsolutePosition => {
                "hold the position item-relative (the item by key with `blaze_format::SpanRef` or `blaze_format::RelSpan`; in the process a `blaze_core::site::Site`) and make `file:line:column` only where it is printed, from the item's start; or, for a value that is shown and never a key, say why with `#[expect(blaze::absolute_position, reason = \"..\")]`"
            }
            Lint::CborFieldOrder => {
                "declare the fields in their keys' order, shorter key first, then bytewise (`blaze check --fix` moves each field whole; review the reorder when the type derives `PartialOrd`, `Ord` or `Hash`, whose derived order follows the declaration); or say why the declaration order must stay with `#[expect(blaze::cbor_field_order, reason = \"..\")]`"
            }
            Lint::GappedLiteral => {
                "put the `\\` line continuation back (the suggestion does), make an aligned table a raw literal, or keep alignment on purpose with `#[expect(blaze::gapped_literal, reason = \"<what is aligned>\")]` on the item"
            }
            Lint::Retry => {
                "remove the retry: the first failure is the error, typed, naming what failed and where; a count that is not a retry says so with `#[expect(blaze::retry, reason = \"..\")]`"
            }
            Lint::Unwired => {
                "wire the item where it was meant to be used, delete it, give an item used only on some targets their `cfg`, or keep it with `#[expect(blaze::unwired, reason = \"<what wires it, and when>\")]` beside the attribute"
            }
            Lint::TmpPath => {
                "make the directory with `blaze_core::temp::TempDir::new(prefix)` (`TempDir::for_test(prefix)` in a test) and hold it as long as it is used; a file beside the system's temporary files takes its directory from `blaze_core::temp::base()`; a path that names the real `/tmp` as policy says so with `#[expect(blaze::tmp_path, reason = \"..\")]`"
            }
            Lint::NarrowedLen => {
                "return a typed error naming the thing and its size (`map_err(|_| Error::TooLarge { what, len })?`), or, where a check just before bounds it, say where with `#[expect(blaze::narrowed_len, reason = \"<the bound and where it is checked>\")]`"
            }
            Lint::ProcessStatic => {
                "give the state an owner with a lifetime (the daemon, a build, a task) that drops it, or say what bounds it with `#[expect(blaze::process_static, reason = \"..\")]`"
            }
            Lint::LoopTurnEffects => {
                "hand work that blocks, does IO or syncs to the driver as an effect or to a plane task on a worker, mark the accessor of a lock only the loop takes `#[blaze::bounded_turn(trusted = \"why\")]`, or take `#[blaze::bounded_turn]` off"
            }
            Lint::SyncOnAnswerPath => {
                "sync after the reply is handed to the driver, mark a callee proven not to sync `#[blaze::answers_request(trusted = \"why\")]`, or take `#[blaze::answers_request]` off"
            }
            Lint::LockOrder => {
                "take the locks in one order everywhere (the outer one first), or drop the one held before taking the other"
            }
            Lint::LockAcrossWait => {
                "copy out what the wait needs and drop the guard before the call, or hand the guard to the wait (`signal.wait(&mut guard)` releases it)"
            }
            Lint::TruncatingCast => {
                "where a value that doesn't fit is a bug, convert with `T::try_from(x)` and a typed error naming the thing; where wrapping is meant, say why with `#[expect(blaze::truncating_cast, reason = \"..\")]`"
            }
            Lint::PartialWrite => {
                "call `write_all`, which writes every byte or returns the error; where the count is the point, loop on it until the buffer is written"
            }
            Lint::PartialCmpUnwrap => {
                "compare with `total_cmp`, or refuse NaN before the comparison with a typed error naming the value"
            }
            Lint::UnboundedRead => {
                "bound the read (`.take(limit)`, a checked `metadata().len()`) and refuse the input past the limit, or say what bounds it with `#[expect(blaze::unbounded_read, reason = \"..\")]`"
            }
            Lint::LenMinus => {
                "`checked_sub` with a typed error naming both lengths, a check before that the length is the larger, or `saturating_sub` where zero is the meant answer"
            }
            Lint::StrByteSlice => {
                "`s.get(a..b)` or `split_at_checked` with the `None` handed on or refused with a typed error, or find the index with `char_indices`/`find` so it is on a boundary"
            }
            Lint::Forget => {
                "end the `ManuallyDrop` with `drop`, `into_inner` or `take`, keep the `Box` with the owner that drops it, or say why the value lives for the process with `#[expect(blaze::forget, reason = \"..\")]`"
            }
            Lint::PathToStrUnwrap => {
                "keep the `Path`/`OsStr` where the name goes back to the OS or into a key, `to_str().ok_or_else(..)?` with an error naming the path where UTF-8 is needed, `.display()` where it is shown"
            }
            Lint::DetachedThread => {
                "keep the `JoinHandle` and `join` it, or spawn in `std::thread::scope`, which joins every thread it spawned"
            }
            Lint::UnboundedChannel => {
                "make the channel bounded (`crossbeam_channel::bounded(n)`), so a full channel pushes back on its sender, or say what bounds it with `#[expect(blaze::unbounded_channel, reason = \"..\")]`"
            }
            Lint::Transmute => {
                "use the safe function for the pair (`to_bits`, `from_bits`, `to_ne_bytes`, `from_ne_bytes`, `cast_signed`, `cast`), or say why no safe form exists with `#[expect(blaze::transmute, reason = \"..\")]`"
            }
            Lint::CloneToSatisfy => {
                "borrow the value itself (`&x`, `x.method()`); where the borrow then conflicts, end the conflicting borrow first instead of copying the value"
            }
            Lint::UncheckedIndexInLoop => {
                "iterate the items (`v.iter()`, `v.iter().enumerate()`, `v.iter().zip(w)`, `v.windows(n)`), or index with `get` and hand the `None` on"
            }
            Lint::DefaultNumericFallbackLen => {
                "write the type the value is (`let mut n: usize = 0;`) and drop the casts"
            }
            Lint::RefcellBorrowAcrossCall => {
                "end the guard before the call (`drop(guard)`, or a block around its use), or copy out what the call needs"
            }
            Lint::ProcessExit => {
                "return from `main` (`-> ExitCode`, or `-> Result<(), E>` with a typed error) so destructors and flushes run; exit only in the binary's entry module, or say why with `#[expect(blaze::process_exit, reason = \"..\")]`"
            }
            Lint::PrintInLib => {
                "hand the caller what it shows (a value, a typed error), take a `&mut impl Write`, or emit a `tracing` event; printing is the binary's choice, made in its entry module"
            }
            Lint::LockUnwrap => {
                "use a lock that does not poison (`parking_lot::Mutex`, `blaze_core::lock::Lock`), or handle the `PoisonError`: `into_inner` where the data is still whole after a panic, a typed error naming the lock where it is not"
            }
            Lint::SkippedPair => {
                "make the second half the drop of a guard the first returns, or a `#[blaze::linear]` value that must be ended; then no exit can skip it"
            }
            Lint::HalfDone => {
                "compute the new state first and commit it in one step after the last fallible call, or change a copy and swap it in"
            }
            Lint::HiddenExit => {
                "make the exit visible at the call: have the macro produce a value (a `Result`, an `Option`, a `ControlFlow`) and write the `?`, `return` or `break` beside the call"
            }
            Lint::QuestionMarkConverts => {
                "convert at the `?` with what the error is about: `.map_err(|source| Error::Variant { path, source })?`, a variant that carries the subject"
            }
            Lint::OptionQuestionMarkHides => {
                "return a `Result` whose error names the key or text that was missing or did not parse; keep `Option` for an answer whose absence is normal"
            }
            Lint::UnseenTarget => {
                "write the target where the conversion is: `.parse::<u32>()`, `.collect::<Vec<_>>()`, `let n: u64 = ...`"
            }
            Lint::GlobImport => {
                "import each name the scope uses (`use a::{B, C};`); a trait used for its methods only is `Trait as _`"
            }
            Lint::ShadowChangesMeaning => {
                "give each value its own name, or end the first one's scope with a block"
            }
            Lint::VersionedName => {
                "keep one item under the plain name and move every caller to it (`.clippy.toml` ban, `blaze check --replace`); or name each for what it does differently"
            }
            Lint::UnusedParameter => {
                "read the parameter again where the body lost its use, or remove it with its argument at every call; a signature fixed for a reason says it in an `#[expect(.., reason)]`"
            }
            Lint::Fallback => {
                "return the error (with `?` and what it was about), or match the one error that has another route and say why it has one"
            }
            Lint::PlaceholderBody => {
                "do what the signature and doc promise, or make the stub loud (`todo!()`, a typed `Unsupported` error naming the item); delete an override that repeats the trait's default"
            }
            Lint::TestAssertsNothing => {
                "assert the result the test is for (`assert_eq!` against the expected value), or let it fail through `?` or `unwrap` where running without a panic is the claim"
            }
            Lint::AssertionCannotFail => {
                "compare the value under test with an expected value computed another way, or delete the assertion"
            }
            Lint::TestMissesItsSubject => {
                "call the crate's code the test is named for; a test of a helper or of std belongs beside that code, or nowhere"
            }
            Lint::IgnoredWithoutReason => {
                "say why and until when: `#[ignore = \"needs a network; runs in the nightly job\"]`"
            }
            Lint::DocNamesNothing => {
                "write the path of an item that exists (`[`crate::store::Store`]`), or plain words for what is not an item"
            }
            Lint::MagicNumber => {
                "derive the number from what it depends on (an array's `len()`, an enum's variant count, `size_of`, another `const`), or name it: a `const` whose doc comment says why that value, used everywhere the fact is used"
            }
            Lint::MagicString => {
                "give the name one home: an enum where the set of names is closed, else a `const` (or a field of the typed settings) used everywhere the name is"
            }
            Lint::RepeatedFormat => {
                "give the sentence one owner: a function that formats it, or a variant of the typed error whose `Display` writes it, called from every place"
            }
            Lint::StringSharedAcrossCrates => {
                "name the string once, in a `const` of the crate that owns what it names (or a crate both depend on), and use it from both"
            }
            Lint::StringNearMissAcrossCrates => {
                "make the two one name: one `const` both crates use, so the writer and the reader cannot spell it differently"
            }
            Lint::SwappedArguments => {
                "pass each argument as the parameter its name matches; give the two a newtype each, or take a struct of named fields, so a swap no longer compiles"
            }
            Lint::WriteOnlyField => {
                "delete the field and its writes, or read it where it was meant to be used"
            }
            Lint::OnlyUsedInTests => {
                "move the function into the tests' module, or call it from the code it was written for"
            }
            Lint::SingleImplTrait => {
                "give the type inherent methods and drop the trait until a second implementation needs it"
            }
            Lint::ForwardingFn => {
                "call the function it forwards to and delete this one"
            }
            Lint::ConstantArgument => {
                "put the value inside the function, as a `const` or a local, and drop the parameter from the signature and every call"
            }
            Lint::AlwaysSameResult => {
                "return the value itself: a `const` for a constant, `T` for an `Option<T>` or `Result<T, E>` that never fails"
            }
            Lint::NeverConstructed => {
                "delete the variant, or construct it where the case it names happens"
            }
            Lint::NeverMatched => {
                "handle the variant by name where it differs, or merge it into the variant it means the same as"
            }
            Lint::SwappableParameters => {
                "give each parameter a newtype, or take one struct of named fields, so that passing them in each other's place is a type error"
            }
            Lint::FunctionWeight => {
                "move the part the help points at into its own function, named for what it does; or return early in place of nesting"
            }
            Lint::PathCount => {
                "move a branching part into its own function, so each can be tested on its own paths"
            }
            Lint::LiveState => {
                "finish each piece of state before the call, or move it with the code that changes it into a function that returns its result"
            }
            Lint::EffectBreadth => {
                "move the calls of one kind of effect into a function of this crate of their own"
            }
            Lint::Exits => {
                "return from one place while a value whose drop finishes something is held, or finish it explicitly in one place"
            }
            Lint::LongFunction => "split the function at its steps, each step a function",
            Lint::LongFile => "move a group of items that belong together into a module of its own",
            Lint::ManyParams => {
                "group the parameters that travel together into one struct, or make the function a method"
            }
            Lint::WideType => {
                "group the fields (or variants) that are used together into a type of their own, held by this one"
            }
            Lint::DeepNesting => {
                "return early (`let .. else`, `?`, a guard `if` that returns) in place of nesting, or move the deep part into its own function"
            }
            Lint::FanOut => "move the calls of one concern into a function of their own",
            Lint::ModuleCycle => {
                "cut the edge the help names: move what one module uses of the other into it, or both into a module neither depends on"
            }
            Lint::ProofForgotten => {
                "keep what the check proved: bind it (`if let Some(x) = x`, `let Ok(x) = r else { .. }`, `v.get(i)`) and use the binding, so no second check is left to fail"
            }
            Lint::UnitMismatch => {
                "carry the unit in the type (`Duration`, a byte-count newtype) and convert with a named call; a sum of two units is then a type error"
            }
            Lint::PrimitiveMeaning => {
                "take `Duration` for a time and a typed id (a newtype per thing, or `la_arena::Idx<T>`) for an identity, so the signature says what the number is"
            }
            Lint::SentinelValue => {
                "make \"none\" a variant: `Option<T>`, with a niche (`Option<NonZero<T>>`, a `NonMax` newtype) where the size matters"
            }
            Lint::StateInFlags => {
                "replace the flags with an enum of the states the value has (or one type per state), so a combination that means nothing can't be made"
            }
            Lint::BoolPairState => {
                "name the states: an enum with one variant per state that occurs, made once where the `bool`s are"
            }
            Lint::OutParameter => {
                "return the value: `Option<T>` when it may not be made, `T` when it always is, `Result<T, E>` when the flag was an error"
            }
            Lint::ParallelCollections => {
                "keep one `Vec` of a struct holding each element's parts, or a struct-of-arrays type with one `push`, so the lengths can't drift"
            }
            Lint::UntypedIndex => {
                "give the index a type made only by its collection: `la_arena::Idx<T>`, or a newtype with `Index<ThatIdx>` on the collection"
            }
            Lint::ValidatedButRaw => {
                "return a newtype whose one constructor is the check (`impl TryFrom<T> for Valid`); with ranged integers or `#[blaze::invariant]` the check is the type's"
            }
            Lint::ClaimContradictsFacts => {
                "make the code hold what the doc says and mark it with the attribute, or say in the doc what the code does"
            }
            Lint::ClaimIsDerivable => {
                "write the attribute the effect walk proves (`#[blaze::nopanic]`, `noalloc`, `nonblocking`, `pure`) and delete the sentence it replaces"
            }
            Lint::ClaimWithoutEvidence => {
                "list the promise under `# Guarantees` (or `# Invariants`) ending in `Evidence: [`a_test`]`, `Evidence: Lean `Theorem``, or `Unverified: why`"
            }
            Lint::EvidenceIsStale => {
                "point at a test or oracle that calls the item, by a path that resolves from it, or at a Lean theorem by its name"
            }
            Lint::SafetyWithoutInvariant => {
                "name the invariant by its path and where it is upheld: `// SAFETY: `Ring::cap` is a power of two; upheld by `Ring::new``"
            }
            Lint::CommentClaims => {
                "state the claim in a form that is checked: an assertion, `blaze::unreachable!()`, a `strict_` operation, an effect attribute, or a type that rules the case out"
            }
            Lint::SameFn => {
                "keep one copy and call it from where the others were called; two that must stay apart say why with `#[expect(blaze::same_fn, reason = ..)]`"
            }
            Lint::SameImplFn => {
                "write the body once: a default method of the trait, a blanket impl, or a macro the impls share"
            }
            Lint::SameBlock => {
                "make the code a function of its free locals, named for what it does, and call it from each place"
            }
            Lint::NearDuplicate => {
                "make one function of the two, the lines that differ its parameters, or say with `#[expect(blaze::near_duplicate, reason = ..)]` why the two must drift apart"
            }
            Lint::PasteMismatch => {
                "finish the rename: the name the note points at is the copy's original, left where the copy renamed it everywhere else"
            }
            Lint::SameType => {
                "keep one type and name it from the other's places (a `use`, or a type alias), so the two cannot drift apart"
            }
            Lint::SameConst => {
                "keep one constant and use it where the other was, so the value has one home"
            }
            Lint::ExtractableDuplicate => {
                "make one function of the two whose parameters are the holes the message names, and call it from both"
            }
            Lint::TestsDifferByLiteral => {
                "write one test over a table of the values, or a property where the expected value can be computed"
            }
            Lint::ImplsDifferByType => {
                "make one generic function or a blanket impl over the types, or `for const` reflection where they are a closed list"
            }
            Lint::DuplicateAcrossCrates => {
                "the same fn, const or type is written in two crates: keep it in their nearest common dependency, or a crate both depend on, and use it from both"
            }
            Lint::ResultPartUnobserved => {
                "assert on the whole value (`assert_eq!(f(..), expected)`), or on the part the message names, in a test that calls the function"
            }
            Lint::WeakAssertion => {
                "compare the value itself with the one expected (`assert_eq!(f(..), Some(expected))`), not one property of it"
            }
            Lint::MirrorAssertion => {
                "write the expected value out, or compute it a way that shares no code with the code under test"
            }
            Lint::PseudoTested => {
                "assert on what the function returns or writes, in a test that calls it"
            }
            Lint::UnreachedByTests => {
                "call the function from a test that checks what it returns, or delete it"
            }
            Lint::ErrorPathUntested => {
                "add a test that hands it a bad input and asserts which error comes back (`assert_eq!(f(bad), Err(..))`)"
            }
            Lint::PropertyOverExamples => {
                "add a property test (Hegel) that draws a value and asserts the inverse gives it back"
            }
            Lint::CouldBeNarrower => {
                "return the narrower type (callers widen with `.into()` where they need to); or say why with `#[expect(blaze::could_be_narrower, reason = \"..\")]`"
            }
            Lint::CouldBeSansIo => {
                "add `#![blaze::sans_io]` to the module, so `blaze::sans_io_core` keeps it free of IO"
            }
            Lint::CouldBeLinear => {
                "mark the type `#[blaze::linear]` and its finishing method `#[blaze::consumes]` (docs/v2/plan/design/linear.md)"
            }
            Lint::CouldHaveInvariant => {
                "write the condition once as `#[blaze::invariant(..)]` on the struct, in terms of its fields, and drop the repeated assertions"
            }
            Lint::SameBehaviour => {
                "keep the original and call it; where the two must stay apart, say why with `#[expect(blaze::same_behaviour, reason = \"..\")]`"
            }
            Lint::HandWrittenDerivable => {
                "derive the impls (`#[derive(Clone, PartialEq, Hash)]`) and delete the hand-written bodies; the fix does both"
            }
            Lint::ImplMissesField => {
                "cover the field the way the others are, or destructure every field and write `field: _, // left out: <why>` for the one left out on purpose"
            }
            Lint::HandKeptVariantList => {
                "derive the list from the declaration where a derive or table exists; the end state is `variants_of::<E>()` with `Variant::NAME` and `Variant::INDEX`"
            }
            Lint::VariantListDrift => {
                "make the list the declaration's again (every variant once, in order, named by the one rule); the fix regenerates a list that is one array or literal"
            }
            Lint::HandWrittenCodec => {
                "write the record's layout once with `blaze_format::record!`, or one generic codec over `for const f in fields_of::<T>()`, in place of a body per type"
            }
            Lint::FieldWiseConversion => {
                "write the field mapping once (a declarative macro over the shared field list today, a generic conversion over `fields_of` in the end state)"
            }
            Lint::ConversionMissesField => {
                "fill each field from its namesake, or say why one is not with `field: _, // left out: <why>` in a destructuring of the source"
            }
            Lint::HandWrittenVisitor => {
                "write one generic walk over `for const f in fields_of::<T>()` in place of one per type"
            }
            Lint::RepeatedFieldList => {
                "derive what a derive writes, and give the rest one body over the field list (a declarative macro today, `for const f in fields_of::<T>()` in the end state)"
            }
            Lint::RegistrySpread => {
                "give each variant one row of one table (a `const` array of records or one declarative macro invocation) and derive every list from the rows"
            }
            Lint::SpanFieldUnrecorded => {
                "name the field for the column it fills (`function` for what the span is of, `awaits` for what it waits for), say it in the span's name, write it as an event inside the span (every field of an event is a row of `event_fields`), or make the span a kind with typed fields in `blaze_profile::span`"
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
                "first, don't allocate: borrow, use a fixed buffer, or write into the caller's buffer; where an allocation is needed, name the allocator: the frame from `blaze_mem::frame::current(|frame| ..)`, the request's `ArenaLocal`, or `Global` written out (`Vec::new_in(Global)`) where the global heap is the value's lifetime; no comment keeps a finding, `#[expect(blaze::implicit_allocator, reason = \"..\")]` does and is counted"
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
            Lint::DefaultPolicy => {
                "take the marked field as an argument of every constructor (a named `const` of the type is a value written at the call, not a default), or say why the type decides with `#[expect(blaze::default_policy, reason = \"..\")]`"
            }
            Lint::SansIoCore => {
                "take what the call reads as a parameter (`advance_clock(now)`) and hand back decisions (`take_decisions`, `next_deadline`); a driver injects IO through a trait object or a type parameter the core takes. Or say why with `#[expect(blaze::sans_io_core, reason = \"..\")]`"
            }
            Lint::KeyIgnoresField => {
                "destructure every field (`let Self { a, b, cache: _ } = self;`), each left out with `// left out: <why>`, or say why with `#[expect(blaze::key_ignores_field, reason = \"..\")]`"
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
            Lint::Unreachable => {
                "add the check that rules the path out (a `match` or `if let` on the value, `is_some`, a comparison with the length or with zero) so that it dominates the site, or write `unreachable!()` for a promise blaze does not check"
            }
            Lint::Refinement => {
                "add the check that shows it (an `if` or a `match` that dominates the site), take the value as a narrower type (`u30`, a struct with its own invariant), or state it as a `requires` of the function"
            }
            Lint::Todo => {
                "write the expression the hole stands for; a test may keep a `todo!()`"
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
            Lint::Capabilities => {
                "take the IO, the allocator or the unsafe interface as an argument, list the crate that does it under `[workspace.metadata.blaze.trusted]` with an `owner` and a `reason`, or take `#![blaze::capabilities]` off"
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
            Lint::ImpliedBoundWritten => {
                "remove the bound: the type or trait the header names declares it once, for every item that names it; or say why it is written again with `#[expect(blaze::implied_bound_written, reason = \"..\")]`"
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
            Lint::AsCast => {
                "write the operation the cast is (`T::from(x)`, `x.truncate::<T>()`, `cast_signed`, `cast_unsigned`, blaze-cast's named casts, `p.cast::<T>()`, `ptr::from_ref`), as `blaze check --fix` does; or say why with `#[expect(blaze::as_cast, reason = \"..\")]`"
            }
            Lint::ImplicitOverflow => {
                "say what overflow means here: `checked_*` with `?` (or a typed error) where it is a failure, `wrapping_*` or `Wrapping<T>` where the wrap is meant, `saturating_*`, `overflowing_*`, `T::try_from(x)` for a cast; or bound the operands with a check blaze can see (`i < v.len()`, `if a <= b`); or say why with `#[expect(blaze::implicit_overflow, reason = \"..\")]`"
            }
            Lint::GlobalState => {
                "give the state one owner and pass it, or a handle to it, as a context to the code the finding lists as using it; or say why it stays global with `#[expect(blaze::global_state, reason = \"..\")]`"
            }
            Lint::AsyncFn => {
                "write it as a plain function run on a fiber: a wait parks the fiber and the may-suspend effect is inferred, so neither `async` nor `.await` is written; or say why with `#[expect(blaze::async_fn, reason = \"..\")]`"
            }
            Lint::UnmarkedPure => {
                "write `#[blaze::pure]` above the function (`blaze check --fix` does); or say why it stays unmarked with `#[expect(blaze::unmarked_pure, reason = \"..\")]`"
            }
            Lint::UnmarkedNopanic => {
                "write `#[blaze::nopanic]` above the function (`blaze check --fix` does); or say why it stays unmarked with `#[expect(blaze::unmarked_nopanic, reason = \"..\")]`"
            }
            Lint::UnmarkedNoalloc => {
                "write `#[blaze::noalloc]` above the function (`blaze check --fix` does); or say why it stays unmarked with `#[expect(blaze::unmarked_noalloc, reason = \"..\")]`"
            }
            Lint::UnmarkedNonblocking => {
                "write `#[blaze::nonblocking]` above the function (`blaze check --fix` does); or say why it stays unmarked with `#[expect(blaze::unmarked_nonblocking, reason = \"..\")]`"
            }
            Lint::MutRef => {
                "spell the unique reference `&uniq` (`ref uniq` in a binding), as `blaze check --fix` does; or say why with `#[expect(blaze::mut_ref, reason = \"..\")]`"
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
            Lint::NotLatestEdition => {
                if ready::EDITION_SHIPS {
                    "set `edition = \"blaze-2026.11\"` under `[package]` in Cargo.toml (`blaze fix --edition`, which will rewrite the sources, is planned and not in this blaze), or keep the package as it is and say so: `not_latest_edition = \"allow\"` under `[workspace.metadata.blaze.lints]`, or `#![allow(blaze::not_latest_edition)]` at the crate root"
                } else {
                    "nothing to change yet: the opt-in is `edition = \"blaze-2026.11\"` under `[package]` in Cargo.toml, and a package of it names bstd as a dependency until blaze ships bstd with itself (docs/v2/plan/design/std.md, row S5d), which is planned and not in this blaze; this lint is `allow` by default until it is, and `blaze fix --edition` is planned too"
                }
            }
            Lint::StdUse => {
                if ready::BSTD_IN_TREE {
                    "take the capability, or write the form of blaze's standard library, that the finding names (bstd, docs/v2/plan/design/std.md): a function then says in its signature everything it can touch, wait for and allocate from; where the finding has a fix, `blaze check --fix` makes it; or say why std's stays with `#[expect(blaze::std_use, reason = \"..\")]`"
                } else {
                    "nothing to change yet: what the finding names is of blaze's standard library (bstd, docs/v2/plan/design/std.md), which is not in this blaze; this lint is `allow` by default, a crate asks for it with `#![blaze::capabilities]`, and blaze's own workspace has it at `warn` once bstd is in the tree"
                }
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
            Lint::DependencyCapability => {
                "read what the dependency does there; if it is trusted with it, add the capability to its entry under `[workspace.metadata.blaze.trust]` in the root Cargo.toml with a `reason` (`blaze audit grants` prints the table the workspace needs today); if not, pin or drop the dependency"
            }
            Lint::DependencyUnsafe => {
                "read the dependency's `unsafe` code; if it is trusted with it, add `\"unsafe\"` to its `allow` under `[workspace.metadata.blaze.trust]` in the root Cargo.toml with a `reason`; if not, pin or drop the dependency"
            }
            Lint::TrustGrantUnused => {
                "remove what the grant no longer needs from `[workspace.metadata.blaze.trust]`, so a version that starts to use it again is told"
            }
            Lint::DependencyCapabilityUnreached => {
                "no build reaches it yet; grant it under `[workspace.metadata.blaze.trust]` before a call does, or keep the workspace from calling it"
            }
            Lint::WorkspaceUnsafe => {
                "say why the crate runs `unsafe` code at its root: `#![blaze::trust(unsafe, reason = \"..\")]`, or make the code safe"
            }
        }
    }
}

pub(crate) fn check_node<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if !cx.on_any(crate::lints::On::BLAZE) {
        return;
    }
    proved_unreachable::check_waiver(cx, node);
    refinement::check_waiver(cx, node);
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
        cbor_field_order::check(cx, adt);
        absolute_position::check(cx, adt);
        program_lifetime::check(cx, adt);
        could_be_program_lifetime::check(cx, adt);
        refinement::check_adt(cx, adt);
        default_policy::check_adt(cx, adt);
        intern_candidate::check(cx, adt);
        boxed_recursive_node::check(cx, adt);
    }
    if node.kind() == syntax::SyntaxKind::ATTR {
        expect::check_deleted(cx, node);
    }
    if let Some(f) = ast::Fn::cast(node) {
        untyped_error::check(cx, f);
        bool_param::check(cx, f);
        large_move::check_fn(cx, f);
        cold_error::check_fn(cx, f);
        profiled::check_fn(cx, f);
        native_contracts::check(cx, f);
        implied_bound_written::check_fn(cx, f);
        proc_macro::check(cx, f);
        async_fn::check_fn(cx, f);
    }
    if matches!(node.kind(), syntax::SyntaxKind::BLOCK_EXPR | syntax::SyntaxKind::CLOSURE_EXPR) {
        async_fn::check_expr(cx, node);
    }
    if matches!(
        node.kind(),
        syntax::SyntaxKind::REF_TYPE
            | syntax::SyntaxKind::REF_EXPR
            | syntax::SyntaxKind::SELF_PARAM
            | syntax::SyntaxKind::IDENT_PAT
            | syntax::SyntaxKind::REF_PAT
    ) {
        mut_ref::check(cx, node);
    }
    if let Some(item) = ast::Static::cast(node) {
        global_state::check(cx, item);
        process_static::check_static(cx, item);
    }
    if let Some(call) = ast::MacroCall::cast(node) {
        global_state::check_thread_local(cx, call);
        process_static::check_thread_local(cx, call);
    }
    if let Some(item) = ast::Enum::cast(node) {
        large_move::check_enum(cx, item);
        complexity::check_enum(cx, item);
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
        complexity::check_struct(cx, item);
    }
    if let Some(item) = ast::Impl::cast(node) {
        missing_size_hint::check(cx, item);
        default_policy::check_impl(cx, item);
        implied_bound_written::check_impl(cx, item);
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
    span_field_unrecorded::check(cx, node);
    gapped_literal::check(cx, node);
    tmp_path::check(cx, node);
    unwired::check(cx, node);
    retry::check(cx, node);
    narrowed_len::check(cx, node);
    std_mistakes::check_node(cx, node);
    suspicious::check_node(cx, node);
    claims::check_node(cx, node);
    weight::check_node(cx, node);
    suggest::check_node(cx, node);
    construct::check_node(cx, node);
    dup::check_item(cx, node);
    if node == cx.root() && cx.in_file() {
        complexity::check(cx);
        dup::check(cx);
        suspicious::check_body(cx);
        claims::check_body(cx);
        error_debug::check(cx);
        dropped_error::check(cx);
        hash_order_output::check(cx);
        held_across_wait::check(cx);
        thread_local_in_waiting_fn::check(cx);
        lock_across_query::check(cx);
        lock_held_across_slow::check(cx);
        lock_order::check(cx);
        roles::check(cx);
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
        magic::check(cx);
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
        proved_unreachable::check(cx);
        refinement::check(cx);
        todo::check(cx);
        unmarked_effects::check(cx);
        expect::check(cx);
        unbuffered_io::check(cx);
        stdout_lock_in_loop::check(cx);
        front_vec_ops::check(cx);
        fmt_allocates::check(cx);
        stable_sort_needless::check(cx);
        as_cast::check(cx);
        implicit_overflow::check(cx);
        truncating_cast::check(cx);
        build_script_ambient::check(cx);
        not_latest_edition::check(cx);
        trust::check(cx);
        std_use::check(cx);
        env_read::check(cx);
        untied_child::check(cx);
        unbounded_test_wait::check(cx);
        reads_positions::check(cx);
        tmp_path::check_body(cx);
        construct::check_body(cx);
        test_strength::check(cx);
        structural::check(cx);
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
    // `implicit_allocator`'s machine-applicable rewrites, by the range of the call each is of.
    let mut fixes: Vec<(TextRange, Vec<(TextRange, String)>)> = Vec::new();
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
                let note = "it allocates from the global heap once it grows: grow a buffer the \
                            caller keeps, or name its allocator (`Vec::new_in(frame)`)";
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
            implicit_allocator::check_body(cx, &k, analysis, &mut push, &mut fixes);
        }
    }
    let root = cx.root();
    for finding in found {
        // A `let`'s finding carries the fix of the call it holds.
        let fix = (finding.lint == Lint::ImplicitAllocator)
            .then(|| fixes.iter().find(|(at, _)| finding.range.contains_range(*at)))
            .flatten()
            .map(|(_, parts)| {
                parts.iter().map(|(at, text)| (cx.span(*at), text.clone())).collect::<Vec<_>>()
            });
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
        if let Some(parts) = fix {
            diagnostic.multipart_suggestion(
                "name the global allocator where it is used",
                parts,
                Applicability::MachineApplicable,
            );
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
