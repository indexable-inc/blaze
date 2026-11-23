//! blaze's lints, rebuilt for the item layer (docs/v2/plan/design/lint.md).
//!
//! A rule is a pure function of one item: the item's own bytes (its text, with its child items'
//! ranges left out as holes) and the [`Scope`] its parent's text gives it. Every enabled rule
//! runs in one pass over the item's tokens ([`lint`]): the bytes are lexed once, and each token
//! goes to the rules registered for its kind (a word to the rules that read that word, a `[` to
//! the index rule, a line comment to the hatch rule), so the rules together cost about one lex.
//!
//! A [`Finding`] holds what rule 81 asks of a record and nothing else: its template (which names
//! its [`Rule`], a stable small number with a name and a version), an item-relative byte range
//! (never `file:line:col`, rule 71), its message as integer arguments (a range of the item, or a
//! row of the rule set's tables), and zero or more [`Fix`]es, each a list of [`Edit`]s. It has
//! no level: whether it is an error, advice or off is decided when it is printed, from the
//! [`Levels`] the configuration gives ([`Levels::of`]).
//!
//! What the results are a function of is a [`RuleSet`]'s identity ([`RuleSet::id`], a digest of
//! the rules' ids, versions and tables), the item's text and holes, and the scope's
//! [`Scope::key`]: the three go into the record's key (rule 65, computed before the work).
//!
//! # The floor (rule 67)
//!
//! A lint pass must read every byte of the item's own text once: the floor is one lex of those
//! bytes, plus one table probe per identifier. Each identifier gets its word number while it is
//! lexed, one probe of a perfect hash fixed at compile time (`sequences::WORDS`); a word no rule
//! reads costs nothing more. Every rule that looks for a token sequence (`todo !`, `. await`,
//! `Vec :: new (`) shares one trie built at compile time (`matcher`): per token, one load for
//! the root and one per live state (at most the longest sequence's length, usually none), the
//! same for one sequence rule or fifty. The other rules keep their dispatch on the word, with
//! short forward scans at a handful of words (`fn`, `type`, `[`, a test attribute, `loop {`). The example (`examples/run.rs`) prints the lex
//! of the same bytes beside the lint, and their ratio. Measured 2026-11-16 on the integration
//! tree's `crates/` (4,767 files, 89 MB, 290,858 units, one thread): lint 492 ms, lex 161 ms, a
//! ratio of 3.05; with no rule enabled 379 ms. So the distance to the floor is per unit (scratch
//! allocated per unit, a lexer start per run between holes), not the rules; scratch reused
//! across units is the next step.
//!
//! # What a typed rule will take
//!
//! Every rule here reads text and tokens only. A rule that needs types (a clone of a type that is
//! `Copy`, a receiver's real type for a rename) is the second kind: it takes the item's typed
//! body record as well as its text, and its key adds that record's key. [`Input`] names the two
//! kinds; only [`Input::Text`] is built.

#![no_std]
#![forbid(unsafe_code)]
#![feature(allocator_api, variant_count)]

#[cfg(test)]
extern crate std;

mod allowed;
mod autofix;
pub mod dup;
mod engine;
mod fix;
mod marks;
mod matcher;
mod message;
pub mod pgo;
mod sequences;
mod units;
mod variants;

#[cfg(test)]
mod tests;

use blaze_own::{AVec, Alloc};

pub use allowed::{ALLOWED_STATICS, Allowed, AllowedStatic, is_hermetic_path, statics_of_path};
pub use engine::Scratch;
pub use fix::{FixError, apply, apply_all};
pub use marks::{OwnHeads, RootMarks};
pub use message::show_message;
pub use units::{ChildHoles, HOLE_MARK, HOLE_MARK_LEN, OwnUnit, UnitLinted, UnitPlan, Units, holes_in, own_place, own_text, real_place, real_range};
pub use variants::{Geometry, Variants, VariantsView};

/// A byte range of an item's text, relative to the item's first byte (rule 71).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

impl Span {
    #[inline]
    pub const fn new(start: u32, end: u32) -> Span {
        Span { start, end }
    }

    /// The text of `self` in `text`, which it is a range of.
    #[inline]
    pub fn of<'t>(self, text: &'t str) -> &'t str {
        &text[self.start as usize..self.end as usize]
    }

    #[inline]
    pub const fn len(self) -> u32 {
        self.end - self.start
    }

    #[inline]
    pub const fn is_empty(self) -> bool {
        self.start == self.end
    }
}

/// A run of entries of one of [`Findings`]' columns, from `from` up to `to`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Hash)]
pub struct Run {
    pub from: u32,
    pub to: u32,
}

impl Run {
    #[inline]
    pub const fn len(self) -> usize {
        (self.to - self.from) as usize
    }

    #[inline]
    pub const fn is_empty(self) -> bool {
        self.from == self.to
    }

    #[inline]
    fn range(self) -> core::ops::Range<usize> {
        self.from as usize..self.to as usize
    }
}

/// A lint rule: its number is its stable id (cited by records and configuration), its name the
/// one people write.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u16)]
pub enum Rule {
    /// A bare `as` cast to a primitive integer type (casts.rs).
    Casts = 1,
    /// A `.clone()` or `Clone::clone(..)` call, by what it duplicates (clones.rs).
    Clones = 2,
    /// What is left of the global heap: `Global`, a signature that fixes its caller's allocator,
    /// an `// alloc:` hatch, an expected `blaze::implicit_allocator` (allocs.rs).
    Allocs = 3,
    /// A function signature or alias whose error is untyped (untyped.rs).
    Untyped = 4,
    /// A function that calls itself directly (recursion.rs).
    Recursion = 5,
    /// A place a library can stop the program (stops.rs).
    Stops = 6,
    /// A path from `std` or `alloc` (ambient.rs).
    Ambient = 7,
    /// An `unsafe` block or impl with no comment stating its assumption above it.
    UnsafeComment = 8,
    /// A formatting macro (`format!`, `write!`, `println!` and family) outside test code.
    FormatMacro = 9,
    /// A name that moved: an old method name of blaze-own's collections.
    Renamed = 10,
    /// A heap reached for instead of handed down: the process's allocator (`Alloc::global()`,
    /// `Heap::process()`), or std's heap with no allocator named (`Vec::new()`, `format!`,
    /// `.to_string()`).
    AmbientHeap = 11,
    /// `async` code: blaze's own code is explicit state machines (`async fn`, `async {`,
    /// `.await`).
    NoAsync = 12,
    /// A read of process state not passed in (rule 75): the clock, the environment, randomness,
    /// a hash seed, the process itself.
    AmbientRead = 13,
    /// Work left undone: `todo!`, `unimplemented!`, `dbg!`, and `unreachable!()` with no message.
    LeftUndone = 14,
    /// A wait with no deadline (rule 18): `.recv()`, `.join()`, `.wait()`, `.lock()` with no
    /// argument, and a `loop` with no `break`, `return` or `?`.
    UnboundedWait = 15,
    /// A hash table whose hasher is not written at the use (decision D125): std's or hashbrown's
    /// tables and hashers (`std::collections::HashMap`, `hash_map::RandomState`,
    /// `DefaultHasher`), and the constructors that build the hasher from `Default`
    /// (`HashMap::new_in(`, `IndexSet::with_capacity_in(`).
    AmbientHasher = 16,
    /// An integer literal equal to the number of variants of an enum of its file, where a count
    /// is meant (a `const`'s whole value, an array's length, a range's end): written
    /// `core::mem::variant_count::<E>()` it cannot go stale (variants.rs). The one rule that
    /// reads its file, through [`Variants`].
    VariantCount = 17,
    /// An options struct (`#[blaze::args]`, rule 83, D126) written other than as a literal
    /// directly in the call that takes it: a TYPED rule, decided by the body check (blaze-typeck
    /// rules/args_inline.rs), never by this crate's token pass ([`Rule::typed`]). On from
    /// [`Step::Edition`] (advice), an error in [`Step::Strict`] ([`Step::levels`]).
    ArgsInline = 18,
    /// An operation only `unsafe` code may do, written in an `unsafe fn`'s body of edition 2024
    /// outside any `unsafe` block: rustc's `unsafe_op_in_unsafe_fn`, warn by default in 2024 (the
    /// Edition Guide, "unsafe_op_in_unsafe_fn warning"). A TYPED rule (blaze-typeck
    /// rules/unsafety.rs), filed only for 2024 bodies; advice at every step, as rustc warns.
    UnsafeOpInUnsafeFn = 19,
    /// State the process keeps with no owner (D170 C3): `thread_local!`, `static mut`, and a
    /// `static` whose type holds interior state (a cell, a lock, an atomic, a once or lazy
    /// value). C3's three (the foreign heap, the profile ring, the signal handler) are not
    /// reported in their crates, by name ([`ALLOWED_STATICS`], D173); a `#[global_allocator]`
    /// attribute alone allows nothing. The lint `blaze::process_static` ([`Rule::lint`]).
    ProcessStatic = 20,
    /// In a sans-IO core (a crate or module whose root says `#![blaze::sans_io]`, which its
    /// [`Scope::sans_io`] carries): a path to the file system, the network, a process, the OS,
    /// the environment or a thread, a print, a blocking wait, or a wall-clock read. A core
    /// returns what to do as its machine's output; its driver (`#![blaze::driver]`) does it. The
    /// lint `blaze::sans_io_core`.
    SansIoCore = 21,
    /// A lock constructed in shipped code with no reason (D179): `Lock::new`, `Lock::unrecorded`,
    /// `Lock::probed`, `ReadLock::new`, `ReadLock::unrecorded` (blaze-core's lock.rs), bstd's
    /// `Lock::new`, `RwLock::new` and `RankedLock::new`, and `SharedMap::new_in`, where no
    /// `#[blaze::lock_exception(tried = "..", why = "..")]` is on the statement, field,
    /// `static`, `const` or item that holds the call ([`Scope::lock_reason`] for one on an
    /// ancestor item). State shared between workers is published, gathered or claimed; a lock
    /// is the named exception, and says which shape it could not be. The lint
    /// `blaze::lock_without_reason`.
    LockWithoutReason = 22,
    /// `unsafe` code (`unsafe {`, `unsafe fn`, `unsafe impl`, `unsafe trait`, `unsafe extern`,
    /// `#[unsafe(..)]`) in a crate whose root does not carry `#![blaze::trust(unsafe, reason = "..")]`.
    /// The lint `blaze::workspace_unsafe`.
    WorkspaceUnsafe = 23,
    /// `Ordering::SeqCst` without a `// seqcst: <why>` comment explaining why one total order is
    /// needed. The lint `blaze::seqcst_unexplained`.
    SeqCstUnexplained = 24,
    /// Raw synchronization primitives (`sync::{Mutex, RwLock, Condvar, Barrier}`,
    /// `parking_lot::{Mutex, RwLock, Condvar}`, `dashmap::DashMap`, `mpsc::{channel, sync_channel}`).
    /// The lint `blaze::raw_sync`.
    RawSync = 25,
    /// Blocking or OS-thread operations on a pool (`thread::{sleep, spawn, scope, yield_now, park,
    /// available_parallelism}`, `hint::spin_loop`). The lint `blaze::blocking_on_pool`.
    BlockingOnPool = 26,
    /// `mem::transmute` or `mem::transmute_copy` (direct call or turbofish). The lint
    /// `blaze::transmute`.
    Transmute = 27,
    /// A bare `bool` parameter on a public function where a two-variant enum or `#[blaze::args]`
    /// options struct should name the choice. The lint `blaze::bool_param`.
    BoolParam = 28,
    /// Two adjacent parameters of the same primitive or string type on a public function that can
    /// be silently transposed at call sites. The lint `blaze::swappable_parameters`.
    SwappableParameters = 29,
    /// A bare numeric parameter on a public function whose name encodes a unit or entity identity
    /// (`_ns`, `_ms`, `_bytes`, `_id`, `timeout`, `port`, ...) and should be a zero-cost newtype.
    /// The lint `blaze::primitive_meaning`.
    PrimitiveMeaning = 30,
    /// A function returning an anonymous tuple carrying unnamed state (e.g. `(Option<T>, bool)`,
    /// `(T, bool)`, `Option<(A, B, C)>`) where a named struct should name each field.
    /// The lint `blaze::anon_tuple_return`.
    AnonTupleReturn = 31,
    /// A large `match` (`>= 8` arms) hand-mapping enum variants to sequential integer tags where
    /// `#[repr(..)]`, `core::mem::discriminant`, or `bstd::comptime` derive belongs.
    /// The lint `blaze::hand_written_derivable`.
    HandWrittenDerivable = 32,
    /// A procedural macro definition (`#[proc_macro]`, `#[proc_macro_derive]`,
    /// `#[proc_macro_attribute]`) where Blaze's `comptime` reflection should be used instead.
    /// The lint `blaze::proc_macro_derive`.
    ProcMacroDef = 33,
    /// Double heap indirection via `Arc<Vec<...>>` or `Arc<String>` in type syntax where
    /// `blaze_own::ThinArc` / `blaze_own::ThinStr` gives a single 8-byte thin pointer and a single
    /// allocation. The lint `blaze::double_indirection_arc`.
    DoubleIndirectionArc = 34,
    /// Raw `"sandbox-exec"` or `"/usr/bin/sandbox-exec"` process spawn outside
    /// `bstd::procs::hermetic`, where `bstd::procs::hermetic::Hermetic` provides cross-platform
    /// macOS Seatbelt + Linux Landlock/seccomp from one sans-IO `Plan`.
    /// The lint `blaze::raw_sandbox_exec`.
    RawSandboxExec = 35,
    /// Deprecated `&mut` / `ref mut` unique reference spelling (`docs/v2/plan/design/edition.md`,
    /// change 10): Blaze spells unique references `&uniq` and `ref uniq`, with mechanical auto-fix.
    /// Does not flag `let mut`, `*mut`, `&raw mut`, or `mut x` bindings.
    /// The lint `blaze::mut_ref`.
    MutRef = 36,
    /// Hardcoded magic slice range length (`bytes.get(0..3) == Some(..)`, `bytes.get(at..at + 2)`,
    /// `&s[..7] == b".."`, or `starts_with` followed by a hardcoded advance) where the length should
    /// be derived via `strip_prefix`, `starts_with`, `.len()`, `size_of::<T>()`, or a named `const`.
    /// The lint `blaze::magic_slice_len`.
    MagicSliceLen = 37,
    /// Boolean-to-integer `as` cast (`(a == b) as u32`, `x.is_some() as usize`) that should use
    /// infallible `T::from(..)` (with mechanical auto-fix). The lint `blaze::bool_as_int`.
    BoolAsInt = 38,
    /// Manual slice length/index idioms (`x.len() == 0` / `x.len() > 0` -> `is_empty()`,
    /// `for i in 0..xs.len()` indexing `xs[i]`, `.find(..)` followed by manual slicing instead of
    /// `split_once`). The lint `blaze::manual_slice_idioms`.
    ManualSliceIdioms = 39,
    /// Test-code ambient leaks (`Alloc::global()` in tests and `#[cfg(test)] extern crate std;`)
    /// that trip Blaze's `alloc_global_tests` and `std-alloc-tests` ratchets.
    /// The lint `blaze::test_ambient_leak`.
    TestAmbientLeak = 40,
    /// `let mut x = ..;` where `x` is never rebound (`x = ..`) and only borrowed uniquely
    /// (`&uniq x` / `&mut x`); under Blaze's edition `let x` permits `&uniq x` directly and
    /// `mut` is reserved for local rebinding. The lint `blaze::mut_for_uniq_borrow`.
    MutForUniqBorrow = 41,
    /// Borrowed `&Io` / `&mut Io` / `&bstd::io::Io` capability bundle outside `blaze-std` and
    /// tests: functions and structs should take only the narrow capabilities they actually use
    /// (`&Out`, `&Fs`, `&Clock`, `&Host`, `&Args`, `CxSpawn`, ...).
    /// The lint `blaze::io_ref`.
    IoRef = 42,
    /// Unused private `use` bindings or redundant braces (`use a::{X, Y};` -> `use a::Y;`,
    /// `use a::{X};` -> `use a::X;`), with a mechanical fix minimizing imports.
    /// The lint `blaze::unused_import`.
    UnusedImport = 43,
    /// Unnecessary `mut` on a `let mut x = ...;` binding or `fn` parameter that is never mutated,
    /// with a mechanical fix removing `mut `.
    /// The lint `blaze::unused_mut`.
    UnusedMut = 44,
    /// Calling `.leak()` / `Box::leak` / `Vec::leak` outside process-root initialization: bump
    /// arenas should use `.into_bump_slice()` / `.into_bump_str()` / `frame.alloc_str()` (backed
    /// by `BumpAlloc`), and intentional `'static` allocations should use `ProgramArena` or `const`.
    /// The lint `blaze::heap_leak`.
    HeapLeak = 45,
}

impl Rule {
    /// How many rules there are: the largest id.
    pub const COUNT: usize = core::mem::variant_count::<Rule>();

    /// Every rule, by id.
    pub const ALL: [Rule; Rule::COUNT] = [
        Rule::Casts,
        Rule::Clones,
        Rule::Allocs,
        Rule::Untyped,
        Rule::Recursion,
        Rule::Stops,
        Rule::Ambient,
        Rule::UnsafeComment,
        Rule::FormatMacro,
        Rule::Renamed,
        Rule::AmbientHeap,
        Rule::NoAsync,
        Rule::AmbientRead,
        Rule::LeftUndone,
        Rule::UnboundedWait,
        Rule::AmbientHasher,
        Rule::VariantCount,
        Rule::ArgsInline,
        Rule::UnsafeOpInUnsafeFn,
        Rule::ProcessStatic,
        Rule::SansIoCore,
        Rule::LockWithoutReason,
        Rule::WorkspaceUnsafe,
        Rule::SeqCstUnexplained,
        Rule::RawSync,
        Rule::BlockingOnPool,
        Rule::Transmute,
        Rule::BoolParam,
        Rule::SwappableParameters,
        Rule::PrimitiveMeaning,
        Rule::AnonTupleReturn,
        Rule::HandWrittenDerivable,
        Rule::ProcMacroDef,
        Rule::DoubleIndirectionArc,
        Rule::RawSandboxExec,
        Rule::MutRef,
        Rule::MagicSliceLen,
        Rule::BoolAsInt,
        Rule::ManualSliceIdioms,
        Rule::TestAmbientLeak,
        Rule::MutForUniqBorrow,
        Rule::IoRef,
        Rule::UnusedImport,
        Rule::UnusedMut,
        Rule::HeapLeak,
    ];

    /// The ten rules ported from blaze's scripts: what [`Step::Strict`] runs. The rules after
    /// them run only where a workspace names them ([`Rule::from_name`], [`RuleSet::with`]).
    pub const HOUSE: [Rule; 10] = [
        Rule::Casts,
        Rule::Clones,
        Rule::Allocs,
        Rule::Untyped,
        Rule::Recursion,
        Rule::Stops,
        Rule::Ambient,
        Rule::UnsafeComment,
        Rule::FormatMacro,
        Rule::Renamed,
    ];

    #[inline]
    pub const fn id(self) -> u16 {
        self as u16
    }

    pub const fn name(self) -> &'static str {
        match self {
            Rule::Casts => "casts",
            Rule::Clones => "clones",
            Rule::Allocs => "allocs",
            Rule::Untyped => "untyped",
            Rule::Recursion => "recursion",
            Rule::Stops => "stops",
            Rule::Ambient => "ambient",
            Rule::UnsafeComment => "unsafe-comment",
            Rule::FormatMacro => "format-macro",
            Rule::Renamed => "renamed",
            Rule::AmbientHeap => "ambient-heap",
            Rule::NoAsync => "no-async",
            Rule::AmbientRead => "ambient-read",
            Rule::LeftUndone => "left-undone",
            Rule::UnboundedWait => "unbounded-wait",
            Rule::AmbientHasher => "ambient-hasher",
            Rule::VariantCount => "variant-count",
            Rule::ArgsInline => "args-inline",
            Rule::UnsafeOpInUnsafeFn => "unsafe-op-in-unsafe-fn",
            Rule::ProcessStatic => "process-static",
            Rule::SansIoCore => "sans-io-core",
            Rule::LockWithoutReason => "lock-without-reason",
            Rule::WorkspaceUnsafe => "workspace-unsafe",
            Rule::SeqCstUnexplained => "seqcst-unexplained",
            Rule::RawSync => "raw-sync",
            Rule::BlockingOnPool => "blocking-on-pool",
            Rule::Transmute => "transmute",
            Rule::BoolParam => "bool-param",
            Rule::SwappableParameters => "swappable-parameters",
            Rule::PrimitiveMeaning => "primitive-meaning",
            Rule::AnonTupleReturn => "anon-tuple-return",
            Rule::HandWrittenDerivable => "hand-written-derivable",
            Rule::ProcMacroDef => "proc-macro-def",
            Rule::DoubleIndirectionArc => "double-indirection-arc",
            Rule::RawSandboxExec => "raw-sandbox-exec",
            Rule::MutRef => "mut-ref",
            Rule::MagicSliceLen => "magic-slice-len",
            Rule::BoolAsInt => "bool-as-int",
            Rule::ManualSliceIdioms => "manual-slice-idioms",
            Rule::TestAmbientLeak => "test-ambient-leak",
            Rule::MutForUniqBorrow => "mut-for-uniq-borrow",
            Rule::IoRef => "io-ref",
            Rule::UnusedImport => "unused-import",
            Rule::UnusedMut => "unused-mut",
            Rule::HeapLeak => "heap-leak",
        }
    }

    /// The tool lint a crate names this rule by in an attribute (`#![deny(blaze::std_use)]`),
    /// without its `blaze::`: one rule per lint, so a level a crate sets lands on one rule. The
    /// lints are blaze-lint-defs' (`lints.rs`); a rule no lint names has none.
    pub const fn lint(self) -> Option<&'static str> {
        match self {
            Rule::Casts => Some("as_cast"),
            Rule::Untyped => Some("untyped_error"),
            Rule::Ambient => Some("std_use"),
            Rule::AmbientHeap => Some("implicit_allocator"),
            Rule::NoAsync => Some("async_fn"),
            Rule::UnboundedWait => Some("unbounded_wait"),
            Rule::AmbientHasher => Some("default_hasher"),
            Rule::ProcessStatic => Some("process_static"),
            Rule::SansIoCore => Some("sans_io_core"),
            Rule::LockWithoutReason => Some("lock_without_reason"),
            Rule::WorkspaceUnsafe => Some("workspace_unsafe"),
            Rule::SeqCstUnexplained => Some("seqcst_unexplained"),
            Rule::RawSync => Some("raw_sync"),
            Rule::BlockingOnPool => Some("blocking_on_pool"),
            Rule::Transmute => Some("transmute"),
            Rule::BoolParam => Some("bool_param"),
            Rule::SwappableParameters => Some("swappable_parameters"),
            Rule::PrimitiveMeaning => Some("primitive_meaning"),
            Rule::AnonTupleReturn => Some("anon_tuple_return"),
            Rule::HandWrittenDerivable => Some("hand_written_derivable"),
            Rule::ProcMacroDef => Some("proc_macro_derive"),
            Rule::DoubleIndirectionArc => Some("double_indirection_arc"),
            Rule::RawSandboxExec => Some("raw_sandbox_exec"),
            Rule::MutRef => Some("mut_ref"),
            Rule::MagicSliceLen => Some("magic_slice_len"),
            Rule::BoolAsInt => Some("bool_as_int"),
            Rule::ManualSliceIdioms => Some("manual_slice_idioms"),
            Rule::TestAmbientLeak => Some("test_ambient_leak"),
            Rule::MutForUniqBorrow => Some("mut_for_uniq_borrow"),
            Rule::IoRef => Some("io_ref"),
            Rule::UnusedImport => Some("unused_import"),
            Rule::UnusedMut => Some("unused_mut"),
            Rule::HeapLeak => Some("heap_leak"),
            _ => None,
        }
    }

    /// The rule the tool lint `name` is ([`Rule::lint`]'s spelling, without `blaze::`), if any.
    pub fn from_lint(name: &str) -> Option<Rule> {
        Rule::ALL.into_iter().find(|rule| rule.lint() == Some(name))
    }

    /// Whether the rule is typed: decided by the body check from types (its findings are the
    /// `BodyCheck` task's records, filed whatever the step, rule 81), never by the token pass.
    /// A typed rule is in no [`RuleSet`] a step makes, so a token record's key never moves with
    /// it; the step turns it on through its level alone ([`Step::levels`]).
    pub const fn typed(self) -> bool {
        matches!(self, Rule::ArgsInline | Rule::UnsafeOpInUnsafeFn)
    }

    /// The rule configuration names `name` ([`Rule::name`]'s spelling), if any.
    pub fn from_name(name: &str) -> Option<Rule> {
        Rule::ALL.into_iter().find(|rule| rule.name() == name)
    }

    /// The rule's version: raised whenever what it finds changes, so a record of the old one is
    /// not read as the new one's (the version joins [`RuleSet::id`]).
    pub const fn version(self) -> u16 {
        match self {
            Rule::AmbientHeap | Rule::AmbientHasher => 2,
            _ => 1,
        }
    }

    #[inline]
    const fn bit(self) -> u64 {
        1u64 << (self as u16)
    }

    /// The test code a site in exempts it from this rule: allocs.rs reads only `#[cfg(test)]`
    /// (and test files), every other rule test functions (`#[test]`, `#[x::test]`) too; an
    /// `unsafe` needs its comment in tests as anywhere, and a count goes stale in a test as
    /// anywhere.
    #[inline]
    pub const fn exempt(self) -> TestBits {
        match self {
            Rule::Allocs => TestBits::CFG,
            Rule::UnsafeComment | Rule::VariantCount | Rule::MutRef | Rule::TestAmbientLeak | Rule::MutForUniqBorrow | Rule::UnusedImport | Rule::UnusedMut => TestBits::NONE,
            _ => TestBits::ANY,
        }
    }
}

/// What a finding's message says: one variant per kind of finding, each of one [`Rule`]. Its
/// arguments are integers ([`Arg`]); the words are the template's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u16)]
pub enum Template {
    /// `x as u32`; argument: the type's range.
    CastToInt = 1,
    /// A clone whose comment, or whose receiver's declaration, says it is a refcount's.
    CloneHandle = 2,
    /// A clone whose comment says it is on the error path.
    CloneErrorPath = 3,
    /// A clone whose comment says it copies an allocation.
    CloneDeep = 4,
    /// A clone nothing says the kind of.
    CloneUnknown = 5,
    /// The word `Global` outside a `use`.
    AllocGlobal = 10,
    /// A signature that fixes its caller's allocator; argument: the type's range.
    AllocSignature = 11,
    /// An `// alloc: <reason>` hatch.
    AllocComment = 12,
    /// `#[allow(..)]` or `#[expect(..)]` of `blaze::implicit_allocator`.
    AllocExpect = 13,
    /// `Result<_, String>` in a signature; argument: the error type's range.
    UntypedString = 20,
    /// `Result<_, &str>`; argument: the error type's range.
    UntypedStr = 21,
    /// `Result<_, Box<dyn Error>>`; argument: the error type's range.
    UntypedBoxed = 22,
    /// `anyhow::Error` or `anyhow::Result<T>`; argument: its range.
    UntypedAnyhow = 23,
    /// A `type` alias of such a `Result`; argument: the aliased type's range.
    UntypedAlias = 24,
    /// A function whose body calls it; argument: the call's range.
    Recursion = 30,
    StopUnwrap = 40,
    StopExpect = 41,
    StopPanic = 42,
    StopUnreachable = 43,
    StopTodo = 44,
    StopUnimplemented = 45,
    /// An index that can be out of bounds.
    StopIndex = 46,
    /// A path from `std`; argument: the segment after `std::`, when there is one.
    AmbientStd = 50,
    /// A path from `alloc`; argument: the segment after `alloc::`, when there is one.
    AmbientAlloc = 51,
    /// A collection named through `std::` or `alloc::`; argument: the path's first segment.
    AmbientCollection = 52,
    /// An `unsafe` block or impl with no `// SAFETY:` comment above it.
    UnsafeNoComment = 60,
    /// A formatting macro; argument: its name's range.
    FormatMacro = 70,
    /// An old name whose receiver is known to be blaze-own's, with its fix; argument: the
    /// row of [`RuleSet::renames`].
    RenamedFixed = 80,
    /// An old name only blaze-own has, on a receiver of unknown type: for a person; argument:
    /// the row.
    RenamedReport = 81,
    /// The process's heap reached for (`Alloc::global(`, `Heap::process(`); argument: the row of
    /// the sequence table, whose words say what to take instead.
    HeapReach = 90,
    /// A std constructor that allocates with no allocator named (`Vec::new(`); argument: the
    /// row. Whether the type is std's is not known from text (a user type may be named `Vec`).
    HeapConstructor = 91,
    /// An allocating macro (`vec!`, `format!`); argument: the row.
    HeapMacro = 92,
    /// An allocating method (`.to_string(`, `.to_owned(`, `.to_vec(`); argument: the row.
    HeapMethod = 93,
    /// `async` before `fn`, `unsafe`, `move`, `{` or `|`.
    Async = 100,
    /// `.await`.
    Await = 101,
    /// The clock read (`Instant::now(`); argument: the row.
    ReadClock = 110,
    /// The environment read (`env::var(`); argument: the row.
    ReadEnv = 111,
    /// Randomness read (`thread_rng(`); argument: the row.
    ReadRandom = 112,
    /// A hash seed drawn from the process (`HashMap::new(`); argument: the row.
    ReadHashSeed = 113,
    /// The process's own state (`process::exit(`, `process::id(`); argument: the row.
    ReadProcess = 114,
    UndoneTodo = 120,
    UndoneUnimplemented = 121,
    /// `dbg!`; fixed, by taking the macro away, where its argument is a value (a call, a
    /// literal), so the program keeps the value and loses only the print.
    UndoneDbg = 122,
    /// `unreachable!()` with no message.
    UndoneUnreachable = 123,
    /// A wait call with no argument (`.recv()`, `.join()`, `.wait()`, `.lock()`); argument: the
    /// row.
    WaitCall = 130,
    /// A `loop` whose braces hold no `break`, `return` or `?`.
    WaitLoop = 131,
    /// A std or hashbrown table or hasher named by its path (`collections::HashMap`,
    /// `hash_map::RandomState`, `hashbrown::HashSet`); argument: the row.
    HasherStd = 140,
    /// A constructor that builds its table's hasher from `Default` (`HashMap::new_in(`);
    /// argument: the row, whose words are the explicit form.
    HasherDefault = 141,
    /// A literal that is the variant count of one enum of the file; arguments: why it is not
    /// fixed (`variants::FIXED` when it is) and the enum's name. Fixed to
    /// `core::mem::variant_count::<E>()` where the bare name reaches it and the type is `usize`.
    VariantCount = 150,
    /// A literal that is the variant count of two or more enums of the file: which it counts is
    /// for a person; arguments: a placeholder, then each enum's name.
    VariantCountMany = 151,
    /// `thread_local!`.
    StaticThreadLocal = 160,
    /// `static mut`.
    StaticMut = 161,
    /// A `static` whose type holds interior state; argument: the stateful type's range.
    StaticState = 162,
    /// In a sans-IO core, a path to IO (`fs::`, `std::net`, `File::open`, `libc::`); the range
    /// is the word that names it.
    SansIoPath = 170,
    /// In a sans-IO core, a blocking wait (`.recv()`, `.join()`, `.lock()`, `.wait(`,
    /// `block_on(`).
    SansIoBlocking = 171,
    /// In a sans-IO core, the wall clock read (`Instant::now`, `SystemTime::now`).
    SansIoClock = 172,
    /// In a sans-IO core, a print to the process's streams (`println!`, `dbg!`).
    SansIoPrint = 173,
    /// A lock's constructor called where no `#[blaze::lock_exception(..)]` holds it; the range
    /// is the constructor's path (`Lock::new`); argument, when its first argument is a string
    /// literal: the lock's name, the text between the quotes.
    LockNoReason = 180,
    /// A `#[blaze::lock_exception(..)]` whose `tried` is not `"published"`, `"gathered"` or
    /// `"claimed"`, whose `why` is missing or empty, or which says anything else; the range is
    /// the attribute.
    LockBadReason = 181,
    /// `unsafe` code in a crate without `#![blaze::trust(unsafe, reason = "..")]`.
    WorkspaceUnsafe = 190,
    /// `Ordering::SeqCst` without a `// seqcst: <why>` comment above or beside it.
    SeqCstUnexplained = 200,
    /// A raw synchronization primitive (`sync::Mutex`, `parking_lot::RwLock`, `dashmap::DashMap`,
    /// `mpsc::channel(`, etc.); argument: the row of the sequence table.
    RawSync = 210,
    /// A blocking or OS-thread call on a pool (`thread::sleep(`, `thread::spawn(`,
    /// `hint::spin_loop(`, etc.); argument: the row of the sequence table.
    BlockingOnPool = 220,
    /// `mem::transmute` or `mem::transmute_copy`; argument: the row of the sequence table.
    Transmute = 230,
    /// A bare `bool` parameter on a public function; argument: the parameter's name span.
    BoolParam = 240,
    /// Two adjacent parameters of the same primitive/string type; argument: the shared type span.
    SwappableParameters = 250,
    /// A bare numeric parameter whose name carries a unit or entity identity; argument: the type span.
    PrimitiveMeaning = 260,
    /// A function returning an anonymous tuple carrying unnamed state; argument: the tuple span.
    AnonTupleReturn = 270,
    /// A large `match` hand-mapping enum variants to integer tags; argument: arm count.
    HandWrittenDiscriminant = 280,
    /// A `#[proc_macro*]` attribute where Blaze `comptime` reflection belongs.
    ProcMacroDef = 290,
    /// `Arc<Vec<...>>` or `Arc<String>` in type syntax; argument: the inner type (`Vec` or `String`).
    DoubleIndirectionArc = 300,
    /// Literal `"sandbox-exec"` or `"/usr/bin/sandbox-exec"` outside `bstd::procs::hermetic`.
    RawSandboxExec = 310,
    /// Deprecated `&mut` (or `&'a mut`) unique reference; fixed mechanically to `&uniq` (`&'a uniq`).
    MutRef = 320,
    /// Deprecated `ref mut` binding mode; fixed mechanically to `ref uniq`.
    RefMut = 321,
    /// Hardcoded slice range compared to a prefix (`bytes.get(0..3) == Some(rhs)`,
    /// `bytes.get(at..at + 2) == Some(rhs)`, `&s[..7] == rhs`); argument: `rhs` span.
    /// Fixed mechanically to `.starts_with(..)` when the range starts at 0 and uses `== Some(..)`.
    MagicSlicePrefix = 330,
    /// Hardcoded slice range whose literal length does NOT match the compared literal prefix (bug!);
    /// argument: `rhs` span.
    MagicSlicePrefixMismatch = 331,
    /// `x.starts_with(lit)` followed immediately by a hardcoded advance (`+= N`, `= N`, or `[N..]`);
    /// argument: `lit` span.
    MagicSliceAdvance = 332,
    /// Hardcoded byte-width slice `.get(0..N)` / `.get(at..at + N)` feeding `.try_into()` or a
    /// fixed-width integer decode (`2`, `4`, `8`, `16` bytes).
    MagicSliceBytes = 333,
    /// Standalone hardcoded magic slice range `.get(0..N)` / `.get(..N)` / `.get(at..at + N)`
    /// where `N` is a bare integer literal instead of a derived length or named `const`.
    MagicSliceRange = 334,
    /// Boolean-to-integer `as` cast (`(a == b) as u32`, `x.is_some() as usize`); argument: target
    /// integer type span. Fixed mechanically to `T::from(..)`.
    BoolAsInt = 340,
    /// `x.len() == 0` / `x.len() > 0` / `x.len() != 0` / `x.len() >= 1`; fixed mechanically to
    /// `x.is_empty()` / `!x.is_empty()`.
    LenZeroCheck = 350,
    /// `for i in 0..xs.len()` indexing `xs[i]` in its body; argument: indexed slice span.
    IndexLoop = 351,
    /// `.find(pat)` followed by manual slice splitting `[..pos]` / `[pos + 1..]`; argument: `pat`.
    ManualFindSplit = 352,
    /// `Alloc::global()` inside test code (trips `alloc_global_tests`).
    TestGlobalAlloc = 360,
    /// `#[cfg(test)] extern crate std;` (trips `std-alloc-tests`).
    TestExternStd = 361,
    /// `let mut x = ..;` when `x` is never rebound and only uniquely borrowed (`&uniq x`/`&mut x`);
    /// fixed mechanically by removing `mut`.
    MutForUniqBorrow = 370,
    /// Borrowed `&Io` / `&mut Io` / `&bstd::io::Io` capability bundle outside `blaze-std` and tests.
    IoRef = 380,
    /// Unused private `use` binding or redundant single-item braced import; argument: name span.
    UnusedImport = 390,
    /// Unnecessary `mut` on a `let mut` binding or `fn` parameter; argument: binding name span.
    UnusedMut = 400,
    /// `.leak()` / `Box::leak` / `Vec::leak` call outside `Root::leak` and tests.
    HeapLeak = 410,
}

impl Template {
    pub const fn rule(self) -> Rule {
        match self as u16 {
            1 => Rule::Casts,
            2..=5 => Rule::Clones,
            10..=13 => Rule::Allocs,
            20..=24 => Rule::Untyped,
            30 => Rule::Recursion,
            40..=46 => Rule::Stops,
            50..=52 => Rule::Ambient,
            60 => Rule::UnsafeComment,
            70 => Rule::FormatMacro,
            80..=81 => Rule::Renamed,
            90..=93 => Rule::AmbientHeap,
            100..=101 => Rule::NoAsync,
            110..=114 => Rule::AmbientRead,
            120..=123 => Rule::LeftUndone,
            130..=131 => Rule::UnboundedWait,
            140..=141 => Rule::AmbientHasher,
            150..=151 => Rule::VariantCount,
            160..=162 => Rule::ProcessStatic,
            170..=173 => Rule::SansIoCore,
            180..=181 => Rule::LockWithoutReason,
            190 => Rule::WorkspaceUnsafe,
            200 => Rule::SeqCstUnexplained,
            210 => Rule::RawSync,
            220 => Rule::BlockingOnPool,
            230 => Rule::Transmute,
            240 => Rule::BoolParam,
            250 => Rule::SwappableParameters,
            260 => Rule::PrimitiveMeaning,
            270 => Rule::AnonTupleReturn,
            280 => Rule::HandWrittenDerivable,
            290 => Rule::ProcMacroDef,
            300 => Rule::DoubleIndirectionArc,
            310 => Rule::RawSandboxExec,
            320..=321 => Rule::MutRef,
            330..=334 => Rule::MagicSliceLen,
            340 => Rule::BoolAsInt,
            350..=352 => Rule::ManualSliceIdioms,
            360..=361 => Rule::TestAmbientLeak,
            370 => Rule::MutForUniqBorrow,
            380 => Rule::IoRef,
            390 => Rule::UnusedImport,
            400 => Rule::UnusedMut,
            410 => Rule::HeapLeak,
            _ => Rule::VariantCount,
        }
    }

    /// The kind's name, as the scripts name their kinds (`blaze-findings/1`'s `kind`).
    pub const fn kind(self) -> &'static str {
        match self {
            Template::CastToInt => "cast",
            Template::CloneHandle => "handle",
            Template::CloneErrorPath => "error-path",
            Template::CloneDeep => "deep",
            Template::CloneUnknown => "unknown",
            Template::AllocGlobal => "global",
            Template::AllocSignature => "signature",
            Template::AllocComment => "comment",
            Template::AllocExpect => "expect",
            Template::UntypedString => "string",
            Template::UntypedStr => "str",
            Template::UntypedBoxed => "boxed",
            Template::UntypedAnyhow => "anyhow",
            Template::UntypedAlias => "alias",
            Template::Recursion => "recursion",
            Template::StopUnwrap => "unwrap",
            Template::StopExpect => "expect",
            Template::StopPanic => "panic",
            Template::StopUnreachable => "unreachable",
            Template::StopTodo => "todo",
            Template::StopUnimplemented => "unimplemented",
            Template::StopIndex => "index",
            Template::AmbientStd => "std",
            Template::AmbientAlloc => "alloc",
            Template::AmbientCollection => "vec_box",
            Template::UnsafeNoComment => "no-safety-comment",
            Template::FormatMacro => "format",
            Template::RenamedFixed => "fixed",
            Template::RenamedReport => "for-a-person",
            Template::HeapReach => "reach",
            Template::HeapConstructor => "constructor",
            Template::HeapMacro => "macro",
            Template::HeapMethod => "method",
            Template::Async => "async",
            Template::Await => "await",
            Template::ReadClock => "clock",
            Template::ReadEnv => "env",
            Template::ReadRandom => "random",
            Template::ReadHashSeed => "hash-seed",
            Template::ReadProcess => "process",
            Template::UndoneTodo => "todo",
            Template::UndoneUnimplemented => "unimplemented",
            Template::UndoneDbg => "dbg",
            Template::UndoneUnreachable => "unreachable-no-message",
            Template::WaitCall => "call",
            Template::WaitLoop => "loop",
            Template::HasherStd => "std-table",
            Template::HasherDefault => "default-hasher",
            Template::VariantCount => "count",
            Template::VariantCountMany => "count-many",
            Template::StaticThreadLocal => "thread-local",
            Template::StaticMut => "static-mut",
            Template::StaticState => "static-state",
            Template::SansIoPath => "io",
            Template::SansIoBlocking => "blocking",
            Template::SansIoClock => "clock",
            Template::SansIoPrint => "print",
            Template::LockNoReason => "no-reason",
            Template::LockBadReason => "bad-reason",
            Template::WorkspaceUnsafe => "workspace-unsafe",
            Template::SeqCstUnexplained => "seqcst-unexplained",
            Template::RawSync => "raw-sync",
            Template::BlockingOnPool => "blocking-on-pool",
            Template::Transmute => "transmute",
            Template::BoolParam => "bool-param",
            Template::SwappableParameters => "swappable-parameters",
            Template::PrimitiveMeaning => "primitive-meaning",
            Template::AnonTupleReturn => "anon-tuple-return",
            Template::HandWrittenDiscriminant => "hand-written-discriminant",
            Template::ProcMacroDef => "proc-macro-def",
            Template::DoubleIndirectionArc => "double-indirection-arc",
            Template::RawSandboxExec => "raw-sandbox-exec",
            Template::MutRef => "mut-ref",
            Template::RefMut => "ref-mut",
            Template::MagicSlicePrefix => "magic-slice-prefix",
            Template::MagicSlicePrefixMismatch => "magic-slice-prefix-mismatch",
            Template::MagicSliceAdvance => "magic-slice-advance",
            Template::MagicSliceBytes => "magic-slice-bytes",
            Template::MagicSliceRange => "magic-slice-range",
            Template::BoolAsInt => "bool-as-int",
            Template::LenZeroCheck => "len-zero-check",
            Template::IndexLoop => "index-loop",
            Template::ManualFindSplit => "manual-find-split",
            Template::TestGlobalAlloc => "test-global-alloc",
            Template::TestExternStd => "test-extern-std",
            Template::MutForUniqBorrow => "mut-for-uniq-borrow",
            Template::IoRef => "io-ref",
            Template::UnusedImport => "unused-import",
            Template::UnusedMut => "unused-mut",
            Template::HeapLeak => "heap-leak",
        }
    }
}

/// One argument of a message: a range of the item's text, an integer (a table row), or text
/// from outside the item (a name the file declares elsewhere), a run of [`Findings::text`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Arg {
    Span(Span),
    Int(u32),
    Text(Run),
}

/// One finding of one rule in one item (rule 81): no level, an item-relative range.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Finding {
    pub template: Template,
    pub range: Span,
    /// Its message's arguments, in [`Findings::args`].
    pub args: Run,
    /// Its fixes, in [`Findings::fixes`]; empty when no mechanical fix is correct.
    pub fixes: Run,
}

impl Finding {
    #[inline]
    pub const fn rule(&self) -> Rule {
        self.template.rule()
    }
}

/// One way to fix a finding: its edits, in [`Findings::edits`], applied together.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Fix {
    pub edits: Run,
}

/// One edit: the item-relative `range` replaced by the bytes `text` names in [`Findings::text`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Edit {
    pub range: Span,
    pub text: Run,
}

/// The findings of one or more items, as columns in the caller's allocator: what a finding
/// record holds.
pub struct Findings<'a> {
    pub list: AVec<'a, Finding>,
    pub args: AVec<'a, Arg>,
    pub fixes: AVec<'a, Fix>,
    pub edits: AVec<'a, Edit>,
    pub text: AVec<'a, u8>,
}

impl<'a> Findings<'a> {
    pub fn new_in(alloc: Alloc<'a>) -> Findings<'a> {
        Findings {
            list: AVec::new_in(alloc),
            args: AVec::new_in(alloc),
            fixes: AVec::new_in(alloc),
            edits: AVec::new_in(alloc),
            text: AVec::new_in(alloc),
        }
    }

    /// Empties every column, keeping their memory.
    pub fn clear(&mut self) {
        self.list.clear();
        self.args.clear();
        self.fixes.clear();
        self.edits.clear();
        self.text.clear();
    }

    pub fn args_of(&self, finding: &Finding) -> &[Arg] {
        &self.args[finding.args.range()]
    }

    pub fn fixes_of(&self, finding: &Finding) -> &[Fix] {
        &self.fixes[finding.fixes.range()]
    }

    pub fn edits_of(&self, fix: &Fix) -> &[Edit] {
        &self.edits[fix.edits.range()]
    }

    /// The replacement text of `edit`.
    pub fn text_of(&self, edit: &Edit) -> &str {
        self.text_in(edit.text)
    }

    /// The run `run` of the text column: an edit's replacement or an [`Arg::Text`].
    pub fn text_in(&self, run: Run) -> &str {
        // The text column holds only whole `&str`s pushed by the rules (`push_edit`,
        // variants.rs's names, copied from the file's text at token bounds).
        match self.text.get(run.range()).map(core::str::from_utf8) {
            Some(Ok(text)) => text,
            _ => unreachable_text(),
        }
    }

    fn push_edit(&mut self, range: Span, text: &str) -> Result<(), LintError> {
        let from = count(self.text.len())?;
        self.text.extend_from_slice(text.as_bytes());
        let to = count(self.text.len())?;
        self.edits.push(Edit { range, text: Run { from, to } });
        Ok(())
    }
}

#[cold]
fn unreachable_text() -> &'static str {
    ""
}

/// Which test code a site is in: `#[cfg(test)]` code (and test files), test functions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Hash)]
pub struct TestBits(pub u8);

impl TestBits {
    pub const NONE: TestBits = TestBits(0);
    /// `#[cfg(test)]`, `#[cfg(all(test, ..))]`, `#![cfg(test)]`, a test file.
    pub const CFG: TestBits = TestBits(1);
    /// `#[test]`, `#[x::test]`.
    pub const ATTR: TestBits = TestBits(2);
    pub const ANY: TestBits = TestBits(3);

    #[inline]
    pub const fn union(self, other: TestBits) -> TestBits {
        TestBits(self.0 | other.0)
    }

    #[inline]
    pub const fn meets(self, other: TestBits) -> bool {
        self.0 & other.0 != 0
    }
}

/// The kind of the innermost `{` block a site is in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Hash)]
#[repr(u8)]
pub enum Block {
    /// Directly in the file.
    #[default]
    File = 0,
    /// An inline `mod m { .. }`.
    Module = 1,
    /// `impl Type { .. }`.
    Impl = 2,
    /// `impl Trait for Type { .. }`: its methods' signatures are the trait's.
    TraitImpl = 3,
    /// `trait T { .. }`.
    Trait = 4,
    /// A function's body.
    Fn = 5,
    /// Any other block.
    Other = 6,
}

impl Block {
    /// An `impl` or `trait` block: a function directly in it is associated.
    #[inline]
    pub const fn container(self) -> bool {
        matches!(self, Block::Impl | Block::TraitImpl | Block::Trait)
    }
}

/// The comment marks a site's rule looks for above it: what the first line that decides is,
/// counting lines up from a reference line.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Hash)]
pub struct Walk {
    /// How many lines above the reference line the deciding line is (1: the line just above);
    /// 0 when no line decides within the look-up.
    pub dist: u8,
    pub mark: Mark,
}

impl Walk {
    pub const NONE: Walk = Walk { dist: 0, mark: Mark::Stop };
}

/// What a deciding line says.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Hash)]
#[repr(u8)]
pub enum Mark {
    /// The line ends the look: no mark is this site's.
    #[default]
    Stop = 0,
    /// `// clone:` saying a refcount (handle words).
    Handle = 1,
    /// `// clone:` saying the error path.
    ErrorPath = 2,
    /// `// clone:` saying a deep copy.
    Deep = 3,
    /// `// clone:` saying none of the three.
    Unsorted = 4,
    /// A `// SAFETY:` comment.
    Safety = 5,
    /// A `// seqcst: <why>` comment.
    SeqCst = 6,
}

/// What the parent's text around a child item says that the child's rules read: the comments
/// above it and after it on its last line. Computed by the parent's pass ([`Linted::children`]),
/// part of the child's [`Scope::key`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Hash)]
pub struct Context {
    /// The `// clone:` walk from the line above the item's first line.
    pub clone_above: Walk,
    /// A `// clone:` mark after the item on its last line.
    pub clone_after: Option<Mark>,
    /// The `// SAFETY:` walk from the line above the item's first line.
    pub safety_above: Walk,
    /// A `// SAFETY:` mark after the item on its last line.
    pub safety_after: bool,
    /// The `// seqcst: <why>` walk from the line above the item's first line.
    pub seqcst_above: Walk,
    /// A `// seqcst: <why>` mark after the item on its last line.
    pub seqcst_after: bool,
    /// How many lines above the item's first line the nearest `// recursion: calls another
    /// impl` mark is, within three; 0 for none.
    pub recursion_above: u8,
}

/// What a site's rules read from outside its item: the file's facts, and what the parent items'
/// text says of where the item is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Hash)]
pub struct Scope {
    /// The test code the item is in (its file's, or an attribute of an ancestor's).
    pub test: TestBits,
    /// The innermost block the item is directly in.
    pub block: Block,
    /// The nearest enclosing function body or container is a container (`impl`, `trait`).
    pub assoc: bool,
    /// Some ancestor is an inline `mod`.
    pub in_module: bool,
    /// The file is a binary's crate root (`src/main.rs`, `src/bin/*.rs`, `examples/*.rs`).
    pub bin_root: bool,
    /// The file is a build script (`build.rs` outside `src`).
    pub build_script: bool,
    /// The file is in a sans-IO core: its crate's root, or the file's own top, says
    /// `#![blaze::sans_io]` ([`RootMarks`]). What `sans-io-core` reads.
    pub sans_io: bool,
    /// The IO module names the file makes its own ([`OwnHeads`]): a bare head of one (`io::x`)
    /// is the crate's module there, which `sans-io-core` does not report.
    pub own_heads: OwnHeads,
    /// The rows of [`ALLOWED_STATICS`] of the file's crate, a bit each ([`statics_of_path`]):
    /// the statics `process-static` does not report there.
    pub statics: u16,
    /// An ancestor item carries `#[blaze::lock_exception(..)]` (D179): the locks the item
    /// constructs have their reason, which `lock-without-reason` reads.
    pub lock_reason: bool,
    /// The crate root carries a valid `#![blaze::trust(unsafe, reason = "..")]` grant
    /// ([`RootMarks::trust_unsafe`]): what `workspace-unsafe` reads.
    pub trust_unsafe: bool,
    /// The file belongs to `bstd::procs::hermetic` ([`is_hermetic_path`]), which is allowed to
    /// invoke the OS's `sandbox-exec` binary directly: what `raw-sandbox-exec` reads.
    pub in_hermetic: bool,
    pub context: Context,
}

impl Scope {
    /// A file's root scope, from its facts; not in a sans-IO core ([`Scope::in_core`]).
    pub const fn file(test: TestBits, bin_root: bool, build_script: bool) -> Scope {
        Scope {
            test,
            block: Block::File,
            assoc: false,
            in_module: false,
            bin_root,
            build_script,
            sans_io: false,
            own_heads: OwnHeads::NONE,
            statics: 0,
            lock_reason: false,
            trust_unsafe: false,
            in_hermetic: false,
            context: Context {
                clone_above: Walk::NONE,
                clone_after: None,
                safety_above: Walk::NONE,
                safety_after: false,
                seqcst_above: Walk::NONE,
                seqcst_after: false,
                recursion_above: 0,
            },
        }
    }

    /// The same scope in a sans-IO core: what a driver gives a file whose crate root or own top
    /// says `#![blaze::sans_io]` ([`RootMarks::scope`]).
    pub const fn in_core(self) -> Scope {
        Scope { sans_io: true, ..self }
    }

    /// The same scope with a valid `#![blaze::trust(unsafe, reason = "..")]` grant
    /// ([`RootMarks::scope`]).
    pub const fn with_trust_unsafe(self) -> Scope {
        Scope { trust_unsafe: true, ..self }
    }

    /// The same scope inside `bstd::procs::hermetic`, where invoking `sandbox-exec` directly is
    /// permitted ([`is_hermetic_path`]).
    pub const fn in_hermetic(self) -> Scope {
        Scope { in_hermetic: true, ..self }
    }

    /// The same scope in a file with these facts of its own text and crate: the IO module
    /// names it makes its own ([`OwnHeads::read`] of the file) and its crate's allowed statics
    /// ([`statics_of_path`] of its path). What a driver gives every file.
    pub const fn with_file_facts(self, own_heads: OwnHeads, statics: u16) -> Scope {
        Scope { own_heads, statics, ..self }
    }

    /// Whether a pass of `rules` in this scope (or a scope it gives a child: a child's
    /// `sans_io` is its parent's) reads [`Scope::own_heads`]: only `sans-io-core` does, in a
    /// core (engine.rs `sans_io_word`). A driver that would lex a whole file for
    /// [`OwnHeads::read`] only to fill a fact no rule reads skips it (rule 77); the one reader
    /// is named here, beside the fact, so a second one is added here too.
    pub const fn reads_own_heads(&self, rules: &RuleSet) -> bool {
        self.sans_io && rules.runs(Rule::SansIoCore)
    }

    /// Everything in the scope as one integer, for the record's key: two scopes with one key
    /// are equal.
    pub const fn key(&self) -> u64 {
        let c = &self.context;
        let clone_above = if c.clone_above.dist == 0 {
            0u64
        } else {
            let d = if c.clone_above.dist > 8 { 8 } else { c.clone_above.dist } as u64;
            let m = c.clone_above.mark as u64;
            let m = if m > 4 { 4 } else { m };
            1 + (d - 1) * 5 + m
        };
        let clone_after = match c.clone_after {
            None => 0u64,
            Some(mark) => {
                let m = mark as u64;
                1 + if m > 4 { 4 } else { m }
            }
        };
        // 41 * 6 = 246 < 256 (fits in 8 bits: bits 9..17).
        let clone_bits = clone_above * 6 + clone_after;
        let safety_above = if c.safety_above.dist == 0 {
            0u64
        } else {
            let d = if c.safety_above.dist > 8 { 8 } else { c.safety_above.dist } as u64;
            let hit = matches!(c.safety_above.mark, Mark::Safety) as u64;
            1 + (d - 1) * 2 + hit
        };
        // 17 * 2 = 34 < 64 (fits in 6 bits: bits 17..23).
        let safety_bits = safety_above * 2 + (c.safety_after as u64);
        let seqcst_above = if c.seqcst_above.dist == 0 {
            0u64
        } else {
            let d = if c.seqcst_above.dist > 8 { 8 } else { c.seqcst_above.dist } as u64;
            let hit = matches!(c.seqcst_above.mark, Mark::SeqCst) as u64;
            1 + (d - 1) * 2 + hit
        };
        // 17 * 2 = 34 < 64 (6 bits: low 4 in bits 23..27, high 2 in bits 62..64).
        let seqcst_bits = seqcst_above * 2 + (c.seqcst_after as u64);
        (self.test.0 as u64)
            | (self.block as u64) << 2
            | (self.assoc as u64) << 5
            | (self.in_module as u64) << 6
            | (self.bin_root as u64) << 7
            | (self.build_script as u64) << 8
            | clone_bits << 9
            | safety_bits << 17
            | (seqcst_bits & 0xF) << 23
            | (c.recursion_above as u64) << 27
            | (self.sans_io as u64) << 35
            | (self.own_heads.0 as u64) << 36
            | ((self.statics & 0x7FFF) as u64) << 44
            | (self.in_hermetic as u64) << 59
            | (self.lock_reason as u64) << 60
            | (self.trust_unsafe as u64) << 61
            | (seqcst_bits >> 4) << 62
    }
}

/// The two kinds of input a rule can take. Only [`Input::Text`] is built: every rule of this
/// crate is one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Input {
    /// The item's own text and tokens, and its [`Scope`].
    Text,
    /// The item's typed body record too, keyed by its own key (not built).
    Typed,
}

/// The receiver types an old name of [`RuleSet::renames`] is blaze-own's on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Owner {
    /// `blaze_own::Vec`, `AVec`.
    Vec = 0,
    /// `blaze_own::VecDeque`, `ADeque`.
    Deque = 1,
    /// `blaze_own::FixedVec`.
    Fixed = 2,
    /// `blaze_own::Soa`, `ASoa`.
    Soa = 3,
}

/// One row of the rename table: on `owner`, the method `old` is now `new`, with `suffix` put
/// after the call's `)` (`contains(x)` is `scan_for(x).is_some()`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Rename {
    pub owner: Owner,
    pub old: &'static str,
    pub new: &'static str,
    pub suffix: &'static str,
}

const fn row(owner: Owner, old: &'static str, new: &'static str, suffix: &'static str) -> Rename {
    Rename { owner, old, new, suffix }
}

/// The table seeded from blaze-own's `#[deprecated]` methods: each old name forwards to its new
/// one, so the rename keeps the program's meaning.
pub const RENAMES: [Rename; 15] = [
    row(Owner::Vec, "insert", "insert_shift", ""),
    row(Owner::Vec, "remove", "remove_shift", ""),
    row(Owner::Vec, "swap_remove", "remove_swap", ""),
    row(Owner::Vec, "insert_many", "insert_slice_shift", ""),
    row(Owner::Vec, "try_insert_many", "try_insert_slice_shift", ""),
    row(Owner::Vec, "contains", "scan_for", ".is_some()"),
    row(Owner::Vec, "position", "scan_for", ""),
    row(Owner::Fixed, "insert", "insert_shift", ""),
    row(Owner::Fixed, "remove", "remove_shift", ""),
    row(Owner::Fixed, "swap_remove", "remove_swap", ""),
    row(Owner::Deque, "insert", "insert_shift", ""),
    row(Owner::Deque, "remove", "remove_shift", ""),
    row(Owner::Soa, "swap_remove", "remove_swap", ""),
    row(Owner::Soa, "insert", "insert_shift", ""),
    row(Owner::Soa, "remove", "remove_shift", ""),
];

/// The old names no collection but blaze-own's `Vec` has: a call of one on a receiver of
/// unknown type is reported for a person (its `Vec` row), not fixed.
const ONLY_OWN: [&str; 2] = ["insert_many", "try_insert_many"];

/// Which rules run, with their tables: what [`RuleSet::id`] digests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RuleSet {
    enabled: u64,
    renames: &'static [Rename],
}

impl RuleSet {
    /// Every rule, with [`RENAMES`].
    pub const ALL: RuleSet = RuleSet { enabled: u64::MAX, renames: &RENAMES };

    /// No rule runs: a driver makes no lint task for it.
    pub const fn is_empty(&self) -> bool {
        self.enabled == 0
    }

    /// Only `rules`.
    pub const fn only(rules: &[Rule]) -> RuleSet {
        let mut enabled = 0;
        let mut at = 0;
        // Bounded: one turn per rule given.
        while at < rules.len() {
            enabled |= rules[at].bit();
            at += 1;
        }
        RuleSet { enabled, renames: &RENAMES }
    }

    /// `self` and `rule`: how a workspace adds a rule its step does not run.
    pub const fn with(self, rule: Rule) -> RuleSet {
        RuleSet { enabled: self.enabled | rule.bit(), renames: self.renames }
    }

    #[inline]
    pub const fn runs(&self, rule: Rule) -> bool {
        self.enabled & rule.bit() != 0
    }

    pub const fn renames(&self) -> &'static [Rename] {
        self.renames
    }

    /// The rule set's identity: a digest of the format, the number of rules this build has
    /// ([`Rule::COUNT`]), each enabled rule's id and version, and the rename table, in order. It
    /// joins every finding record's key. The count is in it because a record's layout follows
    /// it (a unit record's cost is one word per rule, blaze-cli lints.rs `COST_BYTES`): a build
    /// with another count keys every record apart, so an older record is a miss, never read with
    /// the wrong width (rule 21).
    pub fn id(&self) -> [u8; 32] {
        let mut hasher = blaze_blake3::Hasher::new_derive_key("blaze-lint rule set 1");
        hasher.update(&(Rule::COUNT as u32).to_le_bytes());
        for rule in Rule::ALL {
            if self.runs(rule) {
                hasher.update(&rule.id().to_le_bytes());
                hasher.update(&rule.version().to_le_bytes());
            }
        }
        for rename in self.renames {
            hasher.update(&[rename.owner as u8]);
            for part in [rename.old, rename.new, rename.suffix] {
                hasher.update(&(part.len() as u32).to_le_bytes());
                hasher.update(part.as_bytes());
            }
        }
        hasher.finalize()
    }
}

/// What a finding is when printed: decided only then, from configuration, never stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    /// Printed before the answer, and the command fails.
    Error,
    /// Printed after the answer.
    Advice,
    Off,
}

/// A level per rule: the configuration's, read at print time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Levels(pub [Level; Rule::COUNT]);

impl Levels {
    /// Everything advice: the scripts' ratchets are counts that go down, not errors yet.
    pub const ADVICE: Levels = Levels([Level::Advice; Rule::COUNT]);

    pub const fn of(&self, rule: Rule) -> Level {
        self.0[rule as usize - 1]
    }
}

/// The step of AGENTS.md's adoption ladder a project is on: which of these rules it gets, and at
/// which level (docs/v2/plan/design/lint.md, "Which rules a project gets"). A project that names
/// none is on [`Step::Rust`]: every rule here is one of blaze's house rules, which a plain Rust
/// project did not choose.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Step {
    /// None of these rules: only what Rust itself refuses.
    Rust,
    /// Advice for what is a defect class in any Rust: an `unsafe` with no stated assumption, an
    /// untyped error, a function that calls itself.
    Suggest,
    /// And what the blaze edition makes an error later: stops, casts, clones; still advice.
    Edition,
    /// The ten house rules ([`Rule::HOUSE`]), at blaze's own levels, and `variant-count`, which
    /// every step from [`Step::Suggest`] runs. The other rules after them (`ambient-heap`,
    /// `no-async`, `ambient-read`, `left-undone`, `unbounded-wait`, `ambient-hasher`,
    /// `process-static`, `sans-io-core`) are on only where a workspace names them, or a crate
    /// denies their lint ([`RootMarks::rules`]): a step adds them with [`RuleSet::with`].
    Strict,
}

impl Step {
    /// The rules of [`Step::Suggest`]. `variant-count` is a defect class in any Rust (a count
    /// that a new variant leaves stale); its fix names a nightly feature, which blaze's own
    /// toolchain has.
    pub const SUGGEST: [Rule; 4] = [Rule::UnsafeComment, Rule::Untyped, Rule::Recursion, Rule::VariantCount];
    /// The rules of [`Step::Edition`].
    pub const EDITION: [Rule; 13] = [
        Rule::UnsafeComment,
        Rule::Untyped,
        Rule::Recursion,
        Rule::VariantCount,
        Rule::Stops,
        Rule::Casts,
        Rule::Clones,
        Rule::MutRef,
        Rule::MagicSliceLen,
        Rule::BoolAsInt,
        Rule::ManualSliceIdioms,
        Rule::TestAmbientLeak,
        Rule::MutForUniqBorrow,
    ];

    /// The step as configuration writes it (`rust`, `suggest`, `edition`, `strict`).
    pub fn from_name(name: &str) -> Option<Step> {
        match name {
            "rust" => Some(Step::Rust),
            "suggest" => Some(Step::Suggest),
            "edition" => Some(Step::Edition),
            "strict" => Some(Step::Strict),
            _ => None,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Step::Rust => "rust",
            Step::Suggest => "suggest",
            Step::Edition => "edition",
            Step::Strict => "strict",
        }
    }

    /// The rules this step runs. [`Step::Rust`] runs none, so a driver makes no lint task.
    pub const fn rules(self) -> RuleSet {
        match self {
            Step::Rust => RuleSet::only(&[]),
            Step::Suggest => RuleSet::only(&Step::SUGGEST),
            Step::Edition => RuleSet::only(&Step::EDITION),
            Step::Strict => RuleSet::only(&Rule::HOUSE).with(Rule::VariantCount),
        }
    }

    /// The levels this step prints its findings at: advice for every token rule (one a step
    /// does not run makes no finding). A typed rule ([`Rule::typed`]) files its findings at
    /// every step, so its level is what turns it on: `args-inline` is off below
    /// [`Step::Edition`] (a blaze-edition API rule), advice there, an error in [`Step::Strict`];
    /// `unsafe-op-in-unsafe-fn` is Rust's own warning, advice at every step (the levels start
    /// as advice).
    pub const fn levels(self) -> Levels {
        let mut levels = Levels::ADVICE.0;
        levels[Rule::ArgsInline as usize - 1] = match self {
            Step::Rust | Step::Suggest => Level::Off,
            Step::Edition => Level::Advice,
            Step::Strict => Level::Error,
        };
        Levels(levels)
    }
}

/// One item to lint: its whole text (its range of the file, its children's text included), its
/// children's ranges (item-relative, in order, apart), and its scope.
#[derive(Clone, Copy, Debug)]
pub struct Unit<'t> {
    pub text: &'t str,
    pub holes: &'t [Span],
    pub scope: Scope,
}

/// What a pass did for each rule, by [`Rule`] id: how many sites it tried for that rule (one
/// entry into the rule's matcher: a word dispatched to it, a `[` to the index rule, a line
/// comment to the hatch rule, a sequence of the shared trie that ended under it, a probe of the
/// file's `variant-count` fact). Deterministic (a count of the bytes' tokens, never a time), so
/// it is a record's and a profile's alike: what a rule costs is printed beside what it found
/// (D135: one linter's rules cost 72% of a check's CPU, so each rule's share is said).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Cost(pub [u32; Rule::COUNT]);

impl Cost {
    /// Nothing tried.
    pub const NONE: Cost = Cost([0; Rule::COUNT]);

    /// The sites tried for `rule`.
    #[inline]
    pub const fn of(&self, rule: Rule) -> u32 {
        self.0[rule as usize - 1]
    }

    /// One more site tried for `rule`.
    #[inline]
    pub(crate) fn add(&mut self, rule: Rule) {
        let at = rule as usize - 1;
        self.0[at] = self.0[at].saturating_add(1);
    }

    /// `other`'s sites added to these, rule by rule: a file's or a request's cost from its units'.
    pub fn add_all(&mut self, other: &Cost) {
        for (mine, theirs) in self.0.iter_mut().zip(other.0.iter()) {
            *mine = mine.saturating_add(*theirs);
        }
    }
}

/// What a pass gives besides its findings.
pub struct Linted<'a> {
    /// The findings it pushed, in [`Findings::list`].
    pub findings: Run,
    /// Each hole's scope, in order: the scope its child item is linted in.
    pub children: AVec<'a, Scope>,
    /// The test code the item's last byte is in (an out-of-line `mod m;` declared in test
    /// code makes `m`'s file test code).
    pub test_at_end: TestBits,
    /// The sites each rule tried in this pass ([`Cost`]).
    pub cost: Cost,
    /// The tokens of the item's own bytes the one lex made: the cost every rule shares (the
    /// floor, rule 67), counted once whatever the number of rules.
    pub tokens: u32,
}

/// Why a unit could not be linted: a bad input, named.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LintError {
    /// The text is longer than a `u32` counts.
    TooLong { len: usize },
    /// A hole is out of order, overlaps the one before, ends past the text, or splits a
    /// character.
    BadHole { index: usize, hole: Span },
    /// A column would hold more entries than a `u32` counts.
    TooMany,
    /// A file's table would pass its cap (rule 18): what it counts, and the cap.
    Capped { what: &'static str, cap: u32 },
    /// A rule runs that reads a fact of the whole file its driver did not gather (the driver
    /// gathers it exactly when the rule runs: [`lint_own_in`]).
    NoFact { rule: Rule },
}

#[inline]
fn count(n: usize) -> Result<u32, LintError> {
    u32::try_from(n).map_err(|_| LintError::TooMany)
}

/// Lints one item that is its whole file (a test's text, a one-item file): [`lint_in`] with the
/// file's [`Variants`] gathered from `unit.text` itself, when `variant-count` runs. A driver
/// linting the units of a larger file calls [`lint_in`] with the file's fact, gathered once.
pub fn lint<'a>(rules: &RuleSet, unit: &Unit<'_>, alloc: Alloc<'a>, out: &mut Findings<'a>) -> Result<Linted<'a>, LintError> {
    let variants = match rules.runs(Rule::VariantCount) {
        true => Variants::gather(unit.text, alloc)?,
        false => Variants::new_in(alloc),
    };
    lint_in(rules, unit, &variants, 0, alloc, out)
}

/// Lints one item of a file using reusable [`Scratch`] buffers, leaving child scopes in
/// `scratch.children()` without copying them.
pub(crate) fn lint_in_scratch<'a>(
    rules: &RuleSet,
    unit: &Unit<'_>,
    variants: &Variants<'_>,
    base: u32,
    scratch: &mut Scratch<'a>,
    out: &mut Findings<'a>,
) -> Result<engine::PassDone, LintError> {
    let mut done = engine::run_with(rules, unit, scratch, out)?;
    if rules.runs(Rule::VariantCount) {
        // One probe of the file's fact a unit.
        done.cost.add(Rule::VariantCount);
        variants.find(unit, base, out)?;
        done.findings.to = count(out.list.len())?;
    }
    Ok(done)
}

fn copy_children<'a>(scratch: &Scratch<'a>) -> AVec<'a, Scope> {
    let mut children = AVec::with_capacity_in(scratch.children.len(), scratch.alloc);
    children.extend_from_slice(&scratch.children);
    children
}

/// Lints one item of a file reusing `scratch`'s work buffers across calls: every rule of `rules`
/// in one pass over the tokens of `unit`'s own bytes, findings pushed onto `out`.
pub fn lint_in_with<'a>(
    rules: &RuleSet,
    unit: &Unit<'_>,
    variants: &Variants<'_>,
    base: u32,
    scratch: &mut Scratch<'a>,
    out: &mut Findings<'a>,
) -> Result<Linted<'a>, LintError> {
    let done = lint_in_scratch(rules, unit, variants, base, scratch, out)?;
    let children = copy_children(scratch);
    Ok(Linted { findings: done.findings, children, test_at_end: done.test_at_end, cost: done.cost, tokens: done.tokens })
}

/// Lints one item of a file: every rule of `rules` in one pass over the tokens of `unit`'s own
/// bytes, findings pushed onto `out`, scratch in `alloc` (freed on return); no recursion, no
/// allocation per token. `variant-count` reads `variants`, the fact of the file `unit` is the
/// range from `base` of ([`Variants::gather`]); its findings follow the pass's.
pub fn lint_in<'a>(
    rules: &RuleSet,
    unit: &Unit<'_>,
    variants: &Variants<'_>,
    base: u32,
    alloc: Alloc<'a>,
    out: &mut Findings<'a>,
) -> Result<Linted<'a>, LintError> {
    let mut scratch = Scratch::new_in(alloc);
    lint_in_with(rules, unit, variants, base, &mut scratch, out)
}

/// Lints one unit of a file as its OWN text ([`Units::own`]) reusing `scratch`'s work buffers
/// across calls.
pub fn lint_own_with<'a>(
    rules: &RuleSet,
    own: &OwnUnit<'_>,
    scope: Scope,
    at: Geometry<'_>,
    variants: Option<VariantsView<'_>>,
    scratch: &mut Scratch<'a>,
    out: &mut Findings<'a>,
) -> Result<Linted<'a>, LintError> {
    let mut done = engine::run_with(rules, &own.unit(scope), scratch, out)?;
    if rules.runs(Rule::VariantCount) {
        let variants = variants.ok_or(LintError::NoFact { rule: Rule::VariantCount })?;
        // One probe of the file's fact a unit.
        done.cost.add(Rule::VariantCount);
        variants.find_own(at, out)?;
        done.findings.to = count(out.list.len())?;
    }
    let children = copy_children(scratch);
    Ok(Linted { findings: done.findings, children, test_at_end: done.test_at_end, cost: done.cost, tokens: done.tokens })
}

/// Lints one unit of a file as its OWN text ([`Units::own`]: each child one [`HOLE_MARK`]):
/// [`lint_in`]'s one pass over `own` in `scope`, then `variant-count` from the file's fact by the
/// unit's place `at` in its file. Every finding and fix is placed in the own text, so no child's
/// length is in any of them (rule 71); a printer gives them back ([`real_range`]). `variants` is
/// the file's fact, which a driver gathers (a whole-file lex) only when `variant-count` runs: a
/// set that runs it with none given is refused ([`LintError::NoFact`]), never linted without.
pub fn lint_own_in<'a>(
    rules: &RuleSet,
    own: &OwnUnit<'_>,
    scope: Scope,
    at: Geometry<'_>,
    variants: Option<VariantsView<'_>>,
    alloc: Alloc<'a>,
    out: &mut Findings<'a>,
) -> Result<Linted<'a>, LintError> {
    let mut scratch = Scratch::new_in(alloc);
    lint_own_with(rules, own, scope, at, variants, &mut scratch, out)
}

/// Runs [`Rule::UnusedImport`] over top-level `use` statements in `full_text` (a whole file),
/// pushing findings and mechanical fixes into `out`. Separate from [`lint`] because top-level
/// `use` statements are their own outline items whose references live in sibling items.
pub fn lint_file_imports<'a>(
    full_text: &str,
    scope: Scope,
    alloc: Alloc<'a>,
    out: &mut Findings<'a>,
) -> Result<Run, LintError> {
    let mut scratch = Scratch::new_in(alloc);
    let done = engine::run_file_imports(full_text, scope, &mut scratch, out)?;
    Ok(done.findings)
}

/// Runs [`Rule::UnusedImport`] over top-level `use` statements in `full_text` and appends those
/// whose range lies inside `base..end` (a leaf `use` unit with no holes) into `out`, subtracting
/// `base` so every finding/arg span is unit-relative.
pub fn unused_imports_for_range<'a>(
    full_text: &str,
    scope: Scope,
    base: u32,
    end: u32,
    alloc: Alloc<'a>,
    out: &mut Findings<'a>,
    linted: &mut Linted<'a>,
) -> Result<(), LintError> {
    let mut scratch = Scratch::new_in(alloc);
    let mut temp = Findings::new_in(alloc);
    let done = engine::run_file_imports(full_text, scope, &mut scratch, &mut temp)?;
    linted.cost.add_all(&done.cost);
    let map_span = |s: Span| -> Option<Span> {
        if s.start < base || s.end > end {
            return None;
        }
        Some(Span::new(s.start - base, s.end - base))
    };
    for finding in temp.list.iter() {
        let Some(range) = map_span(finding.range) else { continue };
        let args_from = count(out.args.len())?;
        for arg in temp.args_of(finding) {
            match *arg {
                Arg::Span(span) => {
                    if let Some(mapped) = map_span(span) {
                        out.args.push(Arg::Span(mapped));
                    }
                }
                Arg::Int(v) => out.args.push(Arg::Int(v)),
                Arg::Text(run) => {
                    let from = count(out.text.len())?;
                    out.text.extend_from_slice(temp.text_in(run).as_bytes());
                    let to = count(out.text.len())?;
                    out.args.push(Arg::Text(Run { from, to }));
                }
            }
        }
        let args = Run { from: args_from, to: count(out.args.len())? };
        let fixes_from = count(out.fixes.len())?;
        for fix in temp.fixes_of(finding) {
            let edits_from = count(out.edits.len())?;
            let mut ok = true;
            for edit in temp.edits_of(fix) {
                // Clamp line-delete trailing newline to unit end in unit-local storage;
                // file-level `--fix` reconstructs the full-file edit directly.
                let clamped = Span::new(edit.range.start.max(base), edit.range.end.min(end));
                match map_span(clamped) {
                    Some(mapped) => out.push_edit(mapped, temp.text_of(edit))?,
                    None => {
                        ok = false;
                        break;
                    }
                }
            }
            if ok {
                out.fixes.push(Fix { edits: Run { from: edits_from, to: count(out.edits.len())? } });
            }
        }
        let fixes = Run { from: fixes_from, to: count(out.fixes.len())? };
        out.list.push(Finding { template: finding.template, range, args, fixes });
    }
    linted.findings.to = count(out.list.len())?;
    Ok(())
}

/// The tokens of a whole text, counted: what the floor of a lint pass over it is (one lex). The
/// example times it beside [`lint`].
pub fn lex_floor(text: &str) -> usize {
    blaze_lex::tokenize(text, blaze_lex::FrontmatterAllowed::No, blaze_lex::Cpu::BASELINE).count()
}
