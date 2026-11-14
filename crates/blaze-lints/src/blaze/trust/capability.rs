//! The capabilities a dependency is granted or denied ([`Capability`]), and how each thing the
//! walk says a crate does ([`hir_ty_pure::trust::Done`], a set of [`Rule`]s and the toolchain
//! function called) maps onto them ([`of_done`]): the one home of the mapping.
//!
//! The walk's [`Rule::Io`] is split by the toolchain function the crate calls ([`PATHS`]): a
//! file or a standard stream is [`Capability::Fs`], a socket [`Capability::Net`], a process
//! [`Capability::Process`]. An `Io` the path does not tell (one the crate's own body does
//! through a foreign function, or a toolchain call outside those modules that does IO below,
//! `Iterator::for_each` over a closure std calls back) is all three, said as not split.

#[cfg(test)]
use hir_ty_pure::EffectSet;
use hir_ty_pure::{Rule, trust::Done};

/// A capability: what a dependency may do only when the workspace grants it by name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum Capability {
    /// `unsafe` code anywhere in the crate: an `unsafe` block, `unsafe fn`, `unsafe impl` or
    /// `unsafe trait`, `asm!`, an `extern` block, `#[no_mangle]`, `#[export_name]`,
    /// `#[link_section]`.
    Unsafe,
    /// Foreign code: a foreign function called, an `extern` block declared, or C its build
    /// script compiles.
    Ffi,
    /// Files and the standard streams.
    Fs,
    /// Sockets.
    Net,
    /// Spawning, running, signalling or ending a process.
    Process,
    /// The environment, the arguments, the working directory.
    Env,
    /// The clock.
    Clock,
    /// Spawning, sleeping or parking threads.
    Thread,
    /// A `static mut`, a static with interior mutability, a thread-local.
    GlobalState,
    /// The system's randomness (not the keys std draws for a `HashMap`).
    Random,
    /// A build script, which runs at build time.
    BuildScript,
    /// A proc-macro crate, whose code runs inside the compiler.
    ProcMacro,
}

impl Capability {
    pub const ALL: [Capability; 12] = [
        Capability::Unsafe,
        Capability::Ffi,
        Capability::Fs,
        Capability::Net,
        Capability::Process,
        Capability::Env,
        Capability::Clock,
        Capability::Thread,
        Capability::GlobalState,
        Capability::Random,
        Capability::BuildScript,
        Capability::ProcMacro,
    ];

    /// The word a grant's `allow` list writes it as.
    pub const fn word(self) -> &'static str {
        match self {
            Capability::Unsafe => "unsafe",
            Capability::Ffi => "ffi",
            Capability::Fs => "fs",
            Capability::Net => "net",
            Capability::Process => "process",
            Capability::Env => "env",
            Capability::Clock => "clock",
            Capability::Thread => "thread",
            Capability::GlobalState => "global_state",
            Capability::Random => "random",
            Capability::BuildScript => "build_script",
            Capability::ProcMacro => "proc_macro",
        }
    }

    /// What a crate that uses it does, as a finding says it: "opens a socket".
    pub const fn said(self) -> &'static str {
        match self {
            Capability::Unsafe => "runs `unsafe` code",
            Capability::Ffi => "calls foreign code",
            Capability::Fs => "touches files",
            Capability::Net => "opens a socket",
            Capability::Process => "runs or ends a process",
            Capability::Env => "reads the environment",
            Capability::Clock => "reads the clock",
            Capability::Thread => "spawns or parks threads",
            Capability::GlobalState => "keeps global state",
            Capability::Random => "draws the system's randomness",
            Capability::BuildScript => "has a build script",
            Capability::ProcMacro => "is a proc macro",
        }
    }

    /// The capability `word` names, as a grant writes it.
    pub fn of_word(word: &str) -> Option<Capability> {
        Capability::ALL.into_iter().find(|it| it.word() == word)
    }

    const fn bit(self) -> u16 {
        1 << self as u8
    }
}

/// A set of [`Capability`]s, one bit each.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Caps(u16);

const _: () = assert!(Capability::ALL.len() <= 16, "`Caps` keeps one bit per capability in a `u16`");

impl Caps {
    pub const EMPTY: Caps = Caps(0);

    pub const fn of(capabilities: &[Capability]) -> Caps {
        let mut bits = 0;
        let mut at = 0;
        while at < capabilities.len() {
            bits |= capabilities[at].bit();
            at += 1;
        }
        Caps(bits)
    }

    pub const fn one(capability: Capability) -> Caps {
        Caps(capability.bit())
    }

    pub const fn has(self, capability: Capability) -> bool {
        self.0 & capability.bit() != 0
    }

    pub const fn union(self, other: Caps) -> Caps {
        Caps(self.0 | other.0)
    }

    pub const fn without(self, other: Caps) -> Caps {
        Caps(self.0 & !other.0)
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Its capabilities, in [`Capability::ALL`]'s order.
    pub fn iter(self) -> impl Iterator<Item = Capability> {
        Capability::ALL.into_iter().filter(move |it| self.has(*it))
    }
}

/// What a thing the walk could not see stands for: every capability code can use at run time.
/// `unsafe`, a build script and a proc macro are read off the crate's items, never guessed.
pub const UNSEEN_STANDS_FOR: Caps = Caps::of(&[
    Capability::Ffi,
    Capability::Fs,
    Capability::Net,
    Capability::Process,
    Capability::Env,
    Capability::Clock,
    Capability::Thread,
    Capability::GlobalState,
    Capability::Random,
]);

/// The capabilities counted anywhere in a crate, reached or not: what its items say, not what a
/// call does.
pub const ANYWHERE: Caps =
    Caps::of(&[Capability::Unsafe, Capability::BuildScript, Capability::ProcMacro]);

/// The capabilities an `Io` stands for when the toolchain function does not tell which.
pub const IO_SPLIT: Caps = Caps::of(&[Capability::Fs, Capability::Net, Capability::Process]);

/// The toolchain's modules whose IO is one capability, by the path a refusal names a function
/// of them with (a trait impl's method starts with `<`, which is dropped first). bstd's are
/// read as std's.
pub const PATHS: &[(&str, Capability)] = &[
    ("std::fs::", Capability::Fs),
    ("std::io::", Capability::Fs),
    ("std::path::", Capability::Fs),
    ("std::os::unix::fs::", Capability::Fs),
    ("std::os::windows::fs::", Capability::Fs),
    ("std::os::fd::", Capability::Fs),
    ("std::net::", Capability::Net),
    ("std::os::unix::net::", Capability::Net),
    ("std::process::", Capability::Process),
    ("std::os::unix::process::", Capability::Process),
];

/// The toolchain's module whose [`Rule::Thread`] is [`Capability::Thread`]: std's waits on a
/// lock or a channel are [`Rule::Thread`] too, and use no thread of their own.
const THREADS: &str = "std::thread::";

/// The toolchain functions whose [`Rule::Random`] is no capability: the keys std draws for a
/// `HashMap`'s `RandomState`, which change no behaviour a program may rely on.
const SEEDS: &[&str] = &["RandomState", "hashmap_random_keys", "std::hash::"];

/// What a [`Done`] uses.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mapped {
    pub caps: Caps,
    /// Some of `caps` stand for a thing the walk could not see ([`UNSEEN_STANDS_FOR`]).
    pub unseen: bool,
    /// Some of `caps` stand for an `Io` the toolchain path did not split ([`PATHS`]).
    pub unsplit: bool,
}

/// The capabilities `done` uses. A call into the toolchain counts what the toolchain may do
/// for its caller (files, sockets, processes, the environment, the clock, threads, randomness);
/// what the toolchain does for itself (its own `unsafe`, foreign calls, statics and `dyn`s) is
/// the toolchain's, which is trusted. A body's own doings count as they are, and what the walk
/// could not see counts as everything.
pub fn of_done(done: &Done) -> Mapped {
    let mut out = Mapped::default();
    let kinds = done.kinds;
    match &done.leaf {
        Some(path) => {
            let path = shown_as_std(path);
            if kinds.has(Rule::Io) {
                match PATHS.iter().find(|(prefix, _)| path.starts_with(prefix)) {
                    Some(&(_, capability)) => out.caps = out.caps.union(Caps::one(capability)),
                    None => {
                        out.caps = out.caps.union(IO_SPLIT);
                        out.unsplit = true;
                    }
                }
            }
            if kinds.has(Rule::Thread) && path.starts_with(THREADS) {
                out.caps = out.caps.union(Caps::one(Capability::Thread));
            }
            if kinds.has(Rule::Random) && !SEEDS.iter().any(|it| path.contains(it)) {
                out.caps = out.caps.union(Caps::one(Capability::Random));
            }
            for (rule, capability) in [(Rule::Env, Capability::Env), (Rule::Clock, Capability::Clock)] {
                if kinds.has(rule) {
                    out.caps = out.caps.union(Caps::one(capability));
                }
            }
        }
        None => {
            for (rule, caps) in OWN {
                if kinds.has(*rule) {
                    out.caps = out.caps.union(*caps);
                }
            }
            if kinds.has(Rule::Io) {
                out.unsplit = true;
            }
            if !kinds.intersect(hir_ty_pure::trust::UNSEEN).is_empty() {
                out.caps = out.caps.union(UNSEEN_STANDS_FOR);
                out.unseen = true;
            }
        }
    }
    out
}

/// What each kind a body does itself uses. Not here: `unsafe` (read off the crate's items, with
/// the rest of [`ANYWHERE`]), and the kinds that are no capability (a panic, an allocation, a
/// wait, a loop, an address made a number, a lock, a call only an instance decides, a call
/// through a pointer whose targets the walk follows where it is made).
const OWN: &[(Rule, Caps)] = &[
    (Rule::Io, IO_SPLIT),
    (Rule::Env, Caps::one(Capability::Env)),
    (Rule::Clock, Caps::one(Capability::Clock)),
    (Rule::Thread, Caps::one(Capability::Thread)),
    (Rule::MutableState, Caps::one(Capability::GlobalState)),
    (Rule::Random, Caps::one(Capability::Random)),
    (Rule::Foreign, Caps::one(Capability::Ffi)),
    (Rule::ForeignUnknown, Caps::one(Capability::Ffi)),
    (Rule::ForeignUnwind, Caps::one(Capability::Ffi)),
];

/// The kinds of a body's own [`Done`] that map to no capability: kept so a new [`Rule`] is
/// placed on purpose (`every_rule_is_placed`).
#[cfg(test)]
pub(super) const NONE: EffectSet = EffectSet::of(&[
    Rule::Address,
    Rule::Unsafe,
    Rule::Panic,
    Rule::Overflow,
    Rule::Alloc,
    Rule::AllocIn,
    Rule::Block,
    Rule::Open,
    Rule::Loop,
    Rule::Recursion,
    Rule::Indirect,
    Rule::Precondition,
    Rule::Sync,
    Rule::Lock,
]);

/// The kinds of [`OWN`], and those [`hir_ty_pure::trust::UNSEEN`] adds.
#[cfg(test)]
pub(super) fn placed() -> EffectSet {
    OWN.iter().fold(hir_ty_pure::trust::UNSEEN, |set, (rule, _)| set.union(EffectSet::one(*rule)))
}

/// `path` with bstd's crate name read as std's and a trait impl's `<` dropped.
fn shown_as_std(path: &str) -> &str {
    let path = path.strip_prefix('<').unwrap_or(path);
    match path.strip_prefix('b') {
        Some(rest) if rest.starts_with("std::") => rest,
        _ => path,
    }
}
