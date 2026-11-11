//! The effects of the one walk (docs/v2/plan/design/pure-fns.md, "Effects"): every kind of thing
//! an instance may do ([`Rule`]), as the bitset ([`EffectSet`]) each instance's summary carries
//! with a witness per kind; the attribute effects that refuse some kinds ([`EffectDef`]:
//! `#[blaze::pure]` here; beside it `nopanic`, `noalloc`, `nonblocking` and `terminates`, which
//! `#[blaze::may(..)]` asks for by the words it leaves out, [`crate::may`]); and the
//! toolchain's entry points a call of which counts as kinds at the call ([`MODELS`]).
//!
//! One walk answers every effect: checking one reads the root's summary, never walks again. A
//! new effect is an [`EffectDef`] in [`EFFECTS`] over the kinds the walk already records, with
//! rows in [`MODELS`] (toolchain functions) or `effect_attrs` (other crates, foreign symbols)
//! for kinds only a table can tell.

use base_db::{CrateOrigin, Db};
use hir_def::{
    AttrDefId, FunctionId, ItemContainerId, Lookup, TraitId,
    attrs::{AttrFlags, MayWord, PureAttr},
};
use hir_ty::db::HirDatabase;
use intern::Symbol;

use crate::models::{Home, Only, Path, Prim, SelfTy, Site};

/// A kind of effect: one bit of an [`EffectSet`]. At most 32.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum Rule {
    /// Files, the network, stdio, processes, ending the process.
    Io,
    /// Environment variables, arguments, the working directory, the process id.
    Env,
    /// `Instant::now`, `SystemTime::now`.
    Clock,
    /// Spawning, sleeping, parking, the current thread's state.
    Thread,
    /// `static mut`, a static with interior mutability, a thread-local.
    MutableState,
    /// A pointer made a number, or an address made data (formatted, hashed, ordered).
    Address,
    /// `RandomState` and the system's randomness.
    Random,
    /// A foreign function or static blaze does not know to be deterministic, inline assembly.
    Foreign,
    /// What the walk can't follow: a body that does not lower, a call it can't resolve.
    Unchecked,
    /// May panic: an `Assert`, a panic entry point.
    Panic,
    /// May fail an optional overflow check (an `Assert` a crate without overflow checks drops).
    Overflow,
    /// The global allocator (`Global`, `System`, a `GlobalAlloc`).
    Alloc,
    /// An `Allocator` the code is given that is not the global one (`Vec::new_in(frame)`).
    AllocIn,
    /// A call that may block the thread (the effects lane's tables).
    Block,
    /// A foreign function no table knows (and inline assembly).
    ForeignUnknown,
    /// A foreign function whose ABI may unwind.
    ForeignUnwind,
    /// A call only an instance decides (a generic check only).
    Open,
    /// A loop not known to end: blocks of the body that reach themselves again, with no
    /// bounding block (`crate::loops`).
    Loop,
    /// The instance can call itself again, through any calls (a cycle of instances).
    Recursion,
    /// A call through a function pointer or a `dyn`, or the drop of a value that may hold a
    /// `dyn`: what it reaches was walked where the pointer or vtable was made, but it may
    /// reach the caller again, a recursion no cycle of instances shows.
    Indirect,
}

impl Rule {
    pub const ALL: [Rule; 20] = [
        Rule::Io,
        Rule::Env,
        Rule::Clock,
        Rule::Thread,
        Rule::MutableState,
        Rule::Address,
        Rule::Random,
        Rule::Foreign,
        Rule::Unchecked,
        Rule::Panic,
        Rule::Overflow,
        Rule::Alloc,
        Rule::AllocIn,
        Rule::Block,
        Rule::ForeignUnknown,
        Rule::ForeignUnwind,
        Rule::Open,
        Rule::Loop,
        Rule::Recursion,
        Rule::Indirect,
    ];

    /// What an instance with this kind does: "reads the environment".
    pub fn said(self) -> &'static str {
        match self {
            Rule::Io => "does input or output",
            Rule::Env => "reads the environment",
            Rule::Clock => "reads the clock",
            Rule::Thread => "uses threads",
            Rule::MutableState => "reads or writes state that changes",
            Rule::Address => "observes an address",
            Rule::Random => "uses randomness",
            Rule::Foreign => "runs code blaze can't see",
            Rule::Unchecked => "does what the check can't follow",
            Rule::Panic => "may panic",
            Rule::Overflow => "may fail an overflow check",
            Rule::Alloc => "allocates",
            Rule::AllocIn => "allocates through an allocator it is given",
            Rule::Block => "may block",
            Rule::ForeignUnknown => "calls a foreign function no table knows",
            Rule::ForeignUnwind => "calls a foreign function that may unwind",
            Rule::Open => "makes a call only an instance decides",
            Rule::Loop => "has a loop not known to end",
            Rule::Recursion => "may call itself again",
            Rule::Indirect => "makes a call that may reach it again",
        }
    }

    /// The kind's name, for JSON and tests: "env".
    pub fn name(self) -> &'static str {
        match self {
            Rule::Io => "io",
            Rule::Env => "env",
            Rule::Clock => "clock",
            Rule::Thread => "thread",
            Rule::MutableState => "mutable_state",
            Rule::Address => "address",
            Rule::Random => "random",
            Rule::Foreign => "foreign",
            Rule::Unchecked => "unchecked",
            Rule::Panic => "panic",
            Rule::Overflow => "overflow",
            Rule::Alloc => "alloc",
            Rule::AllocIn => "alloc_in",
            Rule::Block => "block",
            Rule::ForeignUnknown => "foreign_unknown",
            Rule::ForeignUnwind => "foreign_unwind",
            Rule::Open => "open",
            Rule::Loop => "loop",
            Rule::Recursion => "recursion",
            Rule::Indirect => "indirect",
        }
    }
}

/// A set of [`Rule`]s, one bit each.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default, PartialOrd, Ord)]
pub struct EffectSet(pub u32);

impl EffectSet {
    pub const EMPTY: EffectSet = EffectSet(0);

    pub const fn one(rule: Rule) -> EffectSet {
        EffectSet(1 << rule as u8)
    }

    pub const fn of(rules: &[Rule]) -> EffectSet {
        let mut bits = 0;
        let mut i = 0;
        while i < rules.len() {
            bits |= 1 << rules[i] as u8;
            i += 1;
        }
        EffectSet(bits)
    }

    pub const fn has(self, rule: Rule) -> bool {
        self.0 & (1 << rule as u8) != 0
    }

    pub const fn union(self, other: EffectSet) -> EffectSet {
        EffectSet(self.0 | other.0)
    }

    pub const fn intersect(self, other: EffectSet) -> EffectSet {
        EffectSet(self.0 & other.0)
    }

    pub const fn without(self, other: EffectSet) -> EffectSet {
        EffectSet(self.0 & !other.0)
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Its kinds, in [`Rule`] order.
    pub fn iter(self) -> impl Iterator<Item = Rule> {
        Rule::ALL.into_iter().filter(move |&rule| self.has(rule))
    }
}

/// An effect an attribute asks for: `#[blaze::<name>]` refuses an instance whose summary has a
/// kind in `refuses`; `#[blaze::<name>(trusted = "why")]` on a function, or on the trait of the
/// impl or trait it is a method of, takes those kinds off every call of it (its body is still
/// walked for the other effects and for build scripts).
pub struct EffectDef {
    /// The attribute's word: "pure". (`terminates` has no attribute of its own: it is what a
    /// `#[blaze::may(..)]` without `diverge` asks.)
    pub name: &'static str,
    /// The word of `#[blaze::may(..)]` that allows what this refuses: a list without it asks
    /// for this effect. None for `pure`, which no list names.
    pub word: Option<MayWord>,
    pub refuses: EffectSet,
    /// What the attribute on an item says: checked, trusted with a reason, or malformed.
    pub attr: fn(&Db, AttrDefId) -> Option<PureAttr>,
    /// What an instance with a kind does, as the refusal says it.
    pub says: fn(Rule) -> &'static str,
    /// The refusal's last clause: what the attribute asks of the code.
    pub fix: &'static str,
    /// Whether what a callee that never returns (its type's output is `!`) does counts. Not for
    /// purity: such a call gives no result to keep, so what it does can't make one differ.
    pub through_never: bool,
}

/// What `#[blaze::pure]` refuses: everything that makes a result depend on more than the
/// arguments, or that a skipped call would not do.
pub const PURE_REFUSES: EffectSet = EffectSet::of(&[
    Rule::Io,
    Rule::Env,
    Rule::Clock,
    Rule::Thread,
    Rule::MutableState,
    Rule::Address,
    Rule::Random,
    Rule::Foreign,
    Rule::Unchecked,
]);

/// `#[blaze::pure]`.
pub const PURE: EffectDef = EffectDef {
    name: "pure",
    word: None,
    refuses: PURE_REFUSES,
    attr: AttrFlags::pure_attr,
    says: Rule::said,
    fix: "a pure function reads the outside world only through its arguments",
    through_never: false,
};

/// Every attribute effect, in the order each summary keeps their sets. The effects lane appends
/// `&NOPANIC, &NOALLOC, &NONBLOCKING, &TERMINATES` (defined in `effect_attrs`). A summary's
/// inline room (`walk::Sets`) is this many.
pub const EFFECTS: &[&EffectDef] = &[
    &PURE,
    &crate::effect_attrs::NOPANIC,
    &crate::effect_attrs::NOALLOC,
    &crate::effect_attrs::NONBLOCKING,
    &crate::effect_attrs::TERMINATES,
];

/// The index of `effect` in [`EFFECTS`], by name: a copy with fewer `refuses` (a profile's
/// `nopanic` without overflow checks) is the same effect.
pub fn effect_index(effect: &EffectDef) -> Option<usize> {
    EFFECTS.iter().position(|it| it.name == effect.name)
}

/// Why `func` is trusted for `effect`, if it is: its own `trusted = ".."`, or the one of the
/// trait whose method it is or implements (a door: every impl's method of the trait, and every
/// call of it through `dyn`). The trait it came through, for the latter.
pub(crate) fn trusted(
    db: &Db,
    func: FunctionId,
    effect: &EffectDef,
) -> Option<(Symbol, Option<TraitId>)> {
    if let Some(PureAttr::Trusted(why)) = (effect.attr)(db, AttrDefId::FunctionId(func)) {
        return Some((why, None));
    }
    if let Some(why) = may_trusted(db, AttrDefId::FunctionId(func), effect) {
        return Some((why, None));
    }
    let trait_ = match func.lookup(db).container {
        ItemContainerId::TraitId(it) => it,
        ItemContainerId::ImplId(imp) => db.impl_trait(imp)?.skip_binder().def_id.0,
        _ => return None,
    };
    trusted_trait(db, trait_, effect).map(|why| (why, Some(trait_)))
}

/// Why `trait_` is trusted for `effect`, if it is: its `#[blaze::<effect>(trusted = "..")]`,
/// or its `#[blaze::may(.., trusted = "..")]` that leaves the effect's word out.
pub(crate) fn trusted_trait(db: &Db, trait_: TraitId, effect: &EffectDef) -> Option<Symbol> {
    match (effect.attr)(db, AttrDefId::TraitId(trait_)) {
        Some(PureAttr::Trusted(why)) => Some(why),
        _ => may_trusted(db, AttrDefId::TraitId(trait_), effect),
    }
}

/// The reason of `owner`'s `#[blaze::may(.., trusted = "why")]` when its list leaves out
/// `effect`'s word: the list is taken at its word, so the item does not have the effect.
fn may_trusted(db: &Db, owner: AttrDefId, effect: &EffectDef) -> Option<Symbol> {
    let word = effect.word?;
    let may = AttrFlags::may_attr(db, owner)?;
    if may.malformed.is_some() || may.has(word) {
        return None;
    }
    may.trusted
}

/// Bit `i` set for each effect `EFFECTS[i]` that a call of the trait's method `method` through
/// a `dyn` or a type parameter cannot have: the method's `#[blaze::may(..)]`, or without one
/// its trait's, leaves the effect's word out, and every impl's method is held within that list
/// where it is written (`blaze::undeclared_effect`). Nothing for a method with no list.
pub(crate) fn bound_mask(db: &Db, method: FunctionId) -> u32 {
    let ItemContainerId::TraitId(trait_) = method.lookup(db).container else { return 0 };
    let may = AttrFlags::may_attr(db, AttrDefId::FunctionId(method))
        .or_else(|| AttrFlags::may_attr(db, AttrDefId::TraitId(trait_)));
    let Some(may) = may else { return 0 };
    if may.malformed.is_some() {
        return 0;
    }
    EFFECTS
        .iter()
        .enumerate()
        .filter(|(_, effect)| effect.word.is_some_and(|word| !may.has(word)))
        .fold(0, |mask, (index, _)| mask | 1 << index)
}

/// Whether a body of `krate` is the code a person writes: a crate of the workspace. The address
/// rules of user code ([`Rule::Address`] for casts and transmutes, rows with `from_workspace`)
/// hold there; the toolchain and other libraries use addresses for alignment and layout, which
/// their results do not show.
pub(crate) fn workspace(db: &Db, krate: base_db::Crate) -> bool {
    matches!(krate.data(db).origin, CrateOrigin::Local { .. })
}

/// Whether the foreign `symbol` is deterministic and touches only memory it is given, so a call
/// of it is no [`Rule::Foreign`]: the allocator's shims, `memcpy` and its kin, libm, and LLVM's
/// intrinsics but those that read or set the machine's state. fncache's `pure_foreign`, which
/// this replaces.
pub(crate) fn deterministic_foreign(symbol: &str) -> bool {
    const PREFIXES: &[&str] = &["__rust_", "__rdl_", "__rg_"];
    const MEMORY: &[&str] = &["memcpy", "memmove", "memset", "memcmp", "bcmp", "strlen"];
    const LIBM: &[&str] = &[
        "acos",
        "asin",
        "atan",
        "atan2",
        "cbrt",
        "ceil",
        "cos",
        "cosh",
        "exp",
        "exp2",
        "expm1",
        "fabs",
        "fdim",
        "floor",
        "fma",
        "fmax",
        "fmin",
        "fmod",
        "hypot",
        "log",
        "log10",
        "log1p",
        "log2",
        "pow",
        "round",
        "sin",
        "sinh",
        "sqrt",
        "tan",
        "tanh",
        "tgamma",
        "lgamma_r",
        "trunc",
        "erf",
        "erfc",
        "ldexp",
        "frexp",
        "nextafter",
        "remainder",
    ];
    const MACHINE: &[&str] = &[
        "rdtsc",
        "rdpmc",
        "rdpid",
        "rdpru",
        "rdrand",
        "rdseed",
        "rndr",
        "irg",
        "readcyclecounter",
        "readsteadycounter",
        "xgetbv",
        "xsave",
        "xrstor",
        "mxcsr",
        "fpcr",
        "fpsr",
        "fpenv",
        "rounding",
        "flags.read",
        "flags.write",
        "read_register",
        "write_register",
        "fsbase",
        "gsbase",
        "pkru",
        "rdssp",
        "address",
        "thread.pointer",
        "stacksave",
        "sponentry",
    ];
    if PREFIXES.iter().any(|it| symbol.starts_with(it)) || MEMORY.contains(&symbol) {
        return true;
    }
    if let Some(rest) = symbol.strip_prefix("llvm.") {
        return !MACHINE.iter().any(|it| rest.contains(it));
    }
    LIBM.iter().any(|&it| symbol == it || symbol.strip_prefix(it) == Some("f"))
}

/// A toolchain function a call of which counts as `kinds` at the call. Its body is still walked
/// (for build scripts, memory and the kinds it does not cover): at the call, each effect counts
/// `kinds`, and what the callee's summary has outside `covers`.
#[derive(Clone, Copy)]
pub(crate) struct Modeled {
    pub(crate) home: Home,
    /// The path as said to a person: `std::time::Instant::now`.
    pub(crate) what: &'static str,
    pub(crate) site: Site,
    pub(crate) kinds: EffectSet,
    /// The kinds of its body the row stands for: [`PURE_REFUSES`] and `kinds` for purity's rows
    /// (what is under `Instant::now` is the clock), `kinds` alone for a row that only adds one.
    pub(crate) covers: EffectSet,
    /// What a call of it does: "reads the clock".
    pub(crate) does: &'static str,
    pub(crate) only: Only,
    /// Counted only where the caller's body is a workspace crate's ([`workspace`]).
    pub(crate) from_workspace: bool,
}

const fn row(
    home: Home,
    what: &'static str,
    site: Site,
    kinds: EffectSet,
    does: &'static str,
) -> Modeled {
    Modeled {
        home,
        what,
        site,
        kinds,
        covers: PURE_REFUSES.union(kinds),
        does,
        only: Only::All,
        from_workspace: false,
    }
}

const fn std_fn(what: &'static str, path: Path, rule: Rule, does: &'static str) -> Modeled {
    row(Home::Std, what, Site::Fn(path), EffectSet::one(rule), does)
}

const fn std_method(
    what: &'static str,
    ty: Path,
    name: &'static str,
    rule: Rule,
    does: &'static str,
) -> Modeled {
    row(Home::Std, what, Site::Inherent { ty, name }, EffectSet::one(rule), does)
}

/// The effects lane's row of a toolchain function that may block: it adds `kinds` at the call
/// and covers only them, so what else its body does is followed as any call's.
const fn block_row(
    what: &'static str,
    site: Site,
    kinds: EffectSet,
    does: &'static str,
) -> Modeled {
    Modeled {
        home: Home::Std,
        what,
        site,
        kinds,
        covers: kinds,
        does,
        only: Only::All,
        from_workspace: false,
    }
}

/// A std lock, condvar, channel or sleep: [`Rule::Thread`] (what purity says of std::sync's
/// waits) and [`Rule::Block`].
const WAITS: EffectSet = EffectSet::of(&[Rule::Thread, Rule::Block]);
/// A one-time initialization another thread may be running: [`Rule::Block`] only, so a local
/// `OnceLock` stays pure.
const INIT: EffectSet = EffectSet::one(Rule::Block);
/// A process, socket or terminal wait: [`Rule::Io`] and [`Rule::Block`].
const IO_WAIT: EffectSet = EffectSet::of(&[Rule::Io, Rule::Block]);
const SLEEPS: EffectSet = WAITS;

const fn wait_method(
    what: &'static str,
    ty: Path,
    name: &'static str,
    kinds: EffectSet,
    does: &'static str,
) -> Modeled {
    block_row(what, Site::Inherent { ty, name }, kinds, does)
}

/// A row counted only from a workspace crate's body.
const fn own(row: Modeled) -> Modeled {
    Modeled { from_workspace: true, ..row }
}

const ENV: Rule = Rule::Env;
const THREAD: Rule = Rule::Thread;
const ADDRESS: EffectSet = EffectSet::one(Rule::Address);
const GIVES_ADDRESS: &str = "gives an address as a number";

/// The toolchain's entry points that count as kinds, by path; resolved per toolchain crate with
/// the models' table ([`crate::models::crate_models`]), and named by `check_resolved` when they
/// do not resolve. The effects lane adds its rows (`Rule::Block`) here.
pub(crate) const MODELS: &[Modeled] = &[
    std_fn("std::env::vars", &["env", "vars"], ENV, "reads the environment"),
    std_fn("std::env::vars_os", &["env", "vars_os"], ENV, "reads the environment"),
    std_fn("std::env::args", &["env", "args"], ENV, "reads the program's arguments"),
    std_fn("std::env::args_os", &["env", "args_os"], ENV, "reads the program's arguments"),
    std_fn("std::env::current_dir", &["env", "current_dir"], ENV, "reads the working directory"),
    std_fn(
        "std::env::set_current_dir",
        &["env", "set_current_dir"],
        ENV,
        "changes the working directory",
    ),
    std_fn("std::env::current_exe", &["env", "current_exe"], ENV, "reads the program's path"),
    std_fn("std::env::home_dir", &["env", "home_dir"], ENV, "reads the environment"),
    std_fn("std::env::temp_dir", &["env", "temp_dir"], ENV, "reads the environment"),
    std_fn("std::env::set_var", &["env", "set_var"], ENV, "changes the environment"),
    std_fn("std::env::remove_var", &["env", "remove_var"], ENV, "changes the environment"),
    std_fn("std::process::id", &["process", "id"], ENV, "reads the process id"),
    std_method(
        "std::time::Instant::now",
        &["time", "Instant"],
        "now",
        Rule::Clock,
        "reads the clock",
    ),
    std_method(
        "std::time::SystemTime::now",
        &["time", "SystemTime"],
        "now",
        Rule::Clock,
        "reads the clock",
    ),
    std_fn("std::thread::spawn", &["thread", "spawn"], THREAD, "starts a thread"),
    row(
        Home::Std,
        "std::thread::scope",
        Site::Fn(&["thread", "scope"]),
        SLEEPS,
        "starts threads and waits for them",
    ),
    std_method(
        "std::thread::Builder::spawn",
        &["thread", "Builder"],
        "spawn",
        THREAD,
        "starts a thread",
    ),
    std_method(
        "std::thread::Builder::spawn_scoped",
        &["thread", "Builder"],
        "spawn_scoped",
        THREAD,
        "starts a thread",
    ),
    row(
        Home::Std,
        "std::thread::JoinHandle::join",
        Site::Inherent { ty: &["thread", "JoinHandle"], name: "join" },
        SLEEPS,
        "waits for a thread",
    ),
    row(Home::Std, "std::thread::sleep", Site::Fn(&["thread", "sleep"]), SLEEPS, "sleeps"),
    row(Home::Std, "std::thread::park", Site::Fn(&["thread", "park"]), SLEEPS, "parks the thread"),
    row(
        Home::Std,
        "std::thread::park_timeout",
        Site::Fn(&["thread", "park_timeout"]),
        SLEEPS,
        "parks the thread",
    ),
    std_fn("std::thread::yield_now", &["thread", "yield_now"], THREAD, "yields the thread"),
    std_fn("std::thread::current", &["thread", "current"], THREAD, "reads the current thread"),
    std_fn(
        "std::thread::panicking",
        &["thread", "panicking"],
        THREAD,
        "reads whether the thread is panicking",
    ),
    std_fn(
        "std::thread::available_parallelism",
        &["thread", "available_parallelism"],
        ENV,
        "reads how many threads the machine runs",
    ),
    std_method(
        "std::hash::RandomState::new",
        &["hash", "RandomState"],
        "new",
        Rule::Random,
        "makes a random seed",
    ),
    row(
        Home::Std,
        "<std::hash::RandomState as Default>",
        Site::Impls {
            trait_: (Home::Core, &["default", "Default"]),
            ty: SelfTy::Adt(Home::Std, &["hash", "RandomState"]),
            methods: &[],
        },
        EffectSet::one(Rule::Random),
        "makes a random seed",
    ),
    row(
        Home::Core,
        "core::fmt::pointer_fmt_inner",
        Site::Fn(&["fmt", "pointer_fmt_inner"]),
        ADDRESS,
        "formats an address",
    ),
    row(
        Home::Core,
        "core::fmt::wide_pointer_fmt_inner",
        Site::Fn(&["fmt", "wide_pointer_fmt_inner"]),
        ADDRESS,
        "formats an address",
    ),
    row(
        Home::Core,
        "<*const T as Hash>",
        Site::Impls { trait_: (Home::Core, &["hash", "Hash"]), ty: SelfTy::RawPtr, methods: &[] },
        ADDRESS,
        "hashes an address",
    ),
    row(
        Home::Core,
        "<*const T as PartialOrd>",
        Site::Impls {
            trait_: (Home::Core, &["cmp", "PartialOrd"]),
            ty: SelfTy::RawPtr,
            methods: &[],
        },
        ADDRESS,
        "orders pointers by their addresses",
    ),
    row(
        Home::Core,
        "<*const T as Ord>",
        Site::Impls { trait_: (Home::Core, &["cmp", "Ord"]), ty: SelfTy::RawPtr, methods: &[] },
        ADDRESS,
        "orders pointers by their addresses",
    ),
    own(row(
        Home::Core,
        "<*const T>::addr",
        Site::Prim { prim: Prim::RawPtr, name: "addr" },
        ADDRESS,
        GIVES_ADDRESS,
    )),
    own(row(
        Home::Core,
        "<*const T>::expose_provenance",
        Site::Prim { prim: Prim::RawPtr, name: "expose_provenance" },
        ADDRESS,
        GIVES_ADDRESS,
    )),
    own(row(
        Home::Core,
        "<*const T>::align_offset",
        Site::Prim { prim: Prim::RawPtr, name: "align_offset" },
        ADDRESS,
        GIVES_ADDRESS,
    )),
    own(row(
        Home::Core,
        "<*const T>::is_aligned",
        Site::Prim { prim: Prim::RawPtr, name: "is_aligned" },
        ADDRESS,
        GIVES_ADDRESS,
    )),
    own(row(
        Home::Core,
        "<*const T>::is_aligned_to",
        Site::Prim { prim: Prim::RawPtr, name: "is_aligned_to" },
        ADDRESS,
        GIVES_ADDRESS,
    )),
    own(row(
        Home::Core,
        "core::ptr::NonNull::addr",
        Site::Inherent { ty: &["ptr", "NonNull"], name: "addr" },
        ADDRESS,
        GIVES_ADDRESS,
    )),
    own(row(
        Home::Core,
        "core::ptr::NonNull::expose_provenance",
        Site::Inherent { ty: &["ptr", "NonNull"], name: "expose_provenance" },
        ADDRESS,
        GIVES_ADDRESS,
    )),
    own(row(
        Home::Core,
        "core::ptr::NonNull::align_offset",
        Site::Inherent { ty: &["ptr", "NonNull"], name: "align_offset" },
        ADDRESS,
        GIVES_ADDRESS,
    )),
    own(row(
        Home::Core,
        "core::ptr::NonNull::is_aligned",
        Site::Inherent { ty: &["ptr", "NonNull"], name: "is_aligned" },
        ADDRESS,
        GIVES_ADDRESS,
    )),
    own(row(
        Home::Core,
        "<[T]>::align_to",
        Site::Prim { prim: Prim::Slice, name: "align_to" },
        ADDRESS,
        "splits a slice where its address is aligned",
    )),
    own(row(
        Home::Core,
        "<[T]>::align_to_mut",
        Site::Prim { prim: Prim::Slice, name: "align_to_mut" },
        ADDRESS,
        "splits a slice where its address is aligned",
    )),
    // The effects lane's rows: what blocks (docs/v2/plan/design/effects.md).
    block_row("std::thread::sleep_ms", Site::Fn(&["thread", "sleep_ms"]), SLEEPS, "sleeps"),
    block_row("std::thread::sleep_until", Site::Fn(&["thread", "sleep_until"]), SLEEPS, "sleeps"),
    block_row(
        "std::thread::park_timeout_ms",
        Site::Fn(&["thread", "park_timeout_ms"]),
        SLEEPS,
        "parks the thread",
    ),
    wait_method("std::sync::Mutex::lock", &["sync", "Mutex"], "lock", WAITS, "waits for the lock"),
    wait_method(
        "std::sync::RwLock::read",
        &["sync", "RwLock"],
        "read",
        WAITS,
        "waits for the lock",
    ),
    wait_method(
        "std::sync::RwLock::write",
        &["sync", "RwLock"],
        "write",
        WAITS,
        "waits for the lock",
    ),
    wait_method(
        "std::sync::ReentrantLock::lock",
        &["sync", "ReentrantLock"],
        "lock",
        WAITS,
        "waits for the lock",
    ),
    wait_method(
        "std::sync::Condvar::wait",
        &["sync", "Condvar"],
        "wait",
        WAITS,
        "waits on a condition variable",
    ),
    wait_method(
        "std::sync::Condvar::wait_while",
        &["sync", "Condvar"],
        "wait_while",
        WAITS,
        "waits on a condition variable",
    ),
    wait_method(
        "std::sync::Condvar::wait_timeout",
        &["sync", "Condvar"],
        "wait_timeout",
        WAITS,
        "waits on a condition variable",
    ),
    wait_method(
        "std::sync::Condvar::wait_timeout_ms",
        &["sync", "Condvar"],
        "wait_timeout_ms",
        WAITS,
        "waits on a condition variable",
    ),
    wait_method(
        "std::sync::Condvar::wait_timeout_while",
        &["sync", "Condvar"],
        "wait_timeout_while",
        WAITS,
        "waits on a condition variable",
    ),
    wait_method(
        "std::sync::Barrier::wait",
        &["sync", "Barrier"],
        "wait",
        WAITS,
        "waits at a barrier",
    ),
    wait_method(
        "std::sync::mpsc::Receiver::recv",
        &["sync", "mpsc", "Receiver"],
        "recv",
        WAITS,
        "waits for a message",
    ),
    wait_method(
        "std::sync::mpsc::Receiver::recv_timeout",
        &["sync", "mpsc", "Receiver"],
        "recv_timeout",
        WAITS,
        "waits for a message",
    ),
    wait_method(
        "std::sync::mpsc::Receiver::recv_deadline",
        &["sync", "mpsc", "Receiver"],
        "recv_deadline",
        WAITS,
        "waits for a message",
    ),
    wait_method(
        "std::sync::mpsc::SyncSender::send",
        &["sync", "mpsc", "SyncSender"],
        "send",
        WAITS,
        "waits for room in the channel",
    ),
    wait_method(
        "std::sync::Once::call_once",
        &["sync", "Once"],
        "call_once",
        INIT,
        "waits for another thread's initialization",
    ),
    wait_method(
        "std::sync::Once::call_once_force",
        &["sync", "Once"],
        "call_once_force",
        INIT,
        "waits for another thread's initialization",
    ),
    wait_method(
        "std::sync::Once::wait",
        &["sync", "Once"],
        "wait",
        INIT,
        "waits for another thread's initialization",
    ),
    wait_method(
        "std::sync::Once::wait_force",
        &["sync", "Once"],
        "wait_force",
        INIT,
        "waits for another thread's initialization",
    ),
    wait_method(
        "std::sync::OnceLock::get_or_init",
        &["sync", "OnceLock"],
        "get_or_init",
        INIT,
        "waits for another thread's initialization",
    ),
    wait_method(
        "std::sync::OnceLock::get_or_try_init",
        &["sync", "OnceLock"],
        "get_or_try_init",
        INIT,
        "waits for another thread's initialization",
    ),
    wait_method(
        "std::sync::OnceLock::wait",
        &["sync", "OnceLock"],
        "wait",
        INIT,
        "waits for another thread's initialization",
    ),
    wait_method(
        "std::sync::LazyLock::force",
        &["sync", "LazyLock"],
        "force",
        INIT,
        "waits for another thread's initialization",
    ),
    wait_method(
        "std::process::Child::wait",
        &["process", "Child"],
        "wait",
        IO_WAIT,
        "waits for the child process",
    ),
    wait_method(
        "std::process::Child::wait_with_output",
        &["process", "Child"],
        "wait_with_output",
        IO_WAIT,
        "waits for the child process",
    ),
    wait_method(
        "std::process::Command::output",
        &["process", "Command"],
        "output",
        IO_WAIT,
        "runs a process",
    ),
    wait_method(
        "std::process::Command::status",
        &["process", "Command"],
        "status",
        IO_WAIT,
        "runs a process",
    ),
    wait_method(
        "std::process::Command::spawn",
        &["process", "Command"],
        "spawn",
        IO_WAIT,
        "runs a process",
    ),
    wait_method(
        "std::net::TcpStream::connect",
        &["net", "TcpStream"],
        "connect",
        IO_WAIT,
        "waits for a connection",
    ),
    wait_method(
        "std::net::TcpStream::connect_timeout",
        &["net", "TcpStream"],
        "connect_timeout",
        IO_WAIT,
        "waits for a connection",
    ),
    wait_method(
        "std::net::TcpListener::accept",
        &["net", "TcpListener"],
        "accept",
        IO_WAIT,
        "waits for a connection",
    ),
    wait_method(
        "std::io::Stdin::read_line",
        &["io", "Stdin"],
        "read_line",
        IO_WAIT,
        "waits for standard input",
    ),
    wait_method(
        "std::io::Stdin::lock",
        &["io", "Stdin"],
        "lock",
        IO_WAIT,
        "waits for standard input",
    ),
];

/// What a leaf of the models does, as kinds.
pub(crate) fn leaf_kinds(leaf: crate::models::Leaf) -> (EffectSet, &'static str) {
    use crate::models::Leaf;
    // A leaf's body is not walked, so its kinds stand for everything under it, for every
    // effect: std's IO takes locks (stdout's, the environment's `ENV_LOCK`), makes syscalls that
    // wait, allocates its buffers and paths, and panics where it fails (`_print`).
    let io = EffectSet::of(&[Rule::Io, Rule::Block, Rule::Alloc, Rule::Panic]);
    match leaf {
        Leaf::EnvVar => (
            EffectSet::of(&[Rule::Env, Rule::Block, Rule::Alloc, Rule::Panic]),
            "reads the environment",
        ),
        Leaf::Print => (io, "prints"),
        Leaf::ReadFile => (io, "reads a file"),
        Leaf::WriteFile => (io, "writes a file"),
        Leaf::Copy => (io, "copies a file"),
        Leaf::Probe => (io, "tests whether a path exists"),
        Leaf::Handle => {
            (EffectSet::of(&[Rule::Io, Rule::Block, Rule::Panic]), "reads or writes a file")
        }
        Leaf::RawHandle => {
            (EffectSet::of(&[Rule::Io, Rule::Panic]), "makes a file handle of a number")
        }
        Leaf::Exit => (io, "ends the process"),
        // The panic machinery and the allocator's body are never counted (by design): what is
        // under them runs only while the job fails, or is the heap the call itself refuses.
        Leaf::Panic => (EffectSet::one(Rule::Panic), "panics"),
        Leaf::Alloc => (EffectSet::one(Rule::Alloc), "allocates or frees memory"),
        Leaf::ErrorText => (EffectSet::of(&[Rule::Alloc, Rule::Panic]), "gives an error's text"),
    }
}
