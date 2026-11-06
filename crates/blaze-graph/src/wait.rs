//! What a thread blocks on, said: every blocking wait of a pool thread goes through [`wait`]
//! (or [`Flights`](crate::pump::Flights) for a key), which opens a span `wait` whose `awaits`
//! names it and whose `owner` names who runs it, when known. A block that does not leaves its
//! time unaccounted for, and the profile's gate on raw blocks (docs/profiling.md) fails on it.

use std::fmt::Write as _;

use crate::pump::Waiting;

/// What a wait waits on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitOn<'a> {
    /// A key another task runs, by its name.
    Key(&'a str),
    /// A lock, by the name of what it guards.
    Lock(&'static str),
    /// A channel or condition, by the name of what is sent on it.
    Channel(&'static str),
    /// A read or write, by what it reads or writes.
    Io(&'a str),
    /// A child process, by its pid.
    Child(u32),
    /// The jobs of a scope its caller waits for, by the scope's counter. The profiler's span of
    /// the wait adds what the scope is for: the span its caller waits in, which opened it.
    Scope(u32),
}

impl WaitOn<'_> {
    /// Its `awaits`: `lock <name>`, `channel <name>`, `io <what>`, `child <pid>`, `scope <counter>`,
    /// or the key.
    fn write(self, out: &mut String) {
        // Writing to a `String` does not fail.
        _ = match self {
            WaitOn::Key(key) => write!(out, "{key}"),
            WaitOn::Lock(name) => write!(out, "lock {name}"),
            WaitOn::Channel(name) => write!(out, "channel {name}"),
            WaitOn::Io(what) => write!(out, "io {what}"),
            WaitOn::Child(pid) => write!(out, "child {pid}"),
            WaitOn::Scope(counter) => write!(out, "scope {counter}"),
        };
    }

    /// Which kind of wait it is, without what it names.
    pub fn kind(self) -> WaitKind {
        match self {
            WaitOn::Key(_) => WaitKind::Key,
            WaitOn::Lock(_) => WaitKind::Lock,
            WaitOn::Channel(_) => WaitKind::Channel,
            WaitOn::Io(_) => WaitKind::Io,
            WaitOn::Child(_) => WaitKind::Child,
            WaitOn::Scope(_) => WaitKind::Scope,
        }
    }
}

/// The variant of a [`WaitOn`], what a profiler groups the time of waits by (blaze-profile's
/// latency sketch of each).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WaitKind {
    Key,
    Lock,
    Channel,
    Io,
    Child,
    Scope,
}

impl WaitKind {
    /// Every kind, each at its [`WaitKind::index`].
    pub const ALL: [WaitKind; 6] = [
        WaitKind::Key,
        WaitKind::Lock,
        WaitKind::Channel,
        WaitKind::Io,
        WaitKind::Child,
        WaitKind::Scope,
    ];

    pub const fn index(self) -> usize {
        self as usize
    }

    pub const fn name(self) -> &'static str {
        match self {
            WaitKind::Key => "key",
            WaitKind::Lock => "lock",
            WaitKind::Channel => "channel",
            WaitKind::Io => "io",
            WaitKind::Child => "child",
            WaitKind::Scope => "scope",
        }
    }
}

/// Runs `block`, which blocks on `on`, inside a span `wait` saying so; `owner` is the job or
/// thread that will end the wait, when known.
pub fn wait<R>(on: WaitOn<'_>, owner: Option<u64>, block: impl FnOnce() -> R) -> R {
    let waiting = Waiting::begin(on.kind(), &mut |it| on.write(it), || owner);
    let result = block();
    drop(waiting);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wait_names_what_it_waits_on_by_kind() {
        let named = |on: WaitOn<'_>| {
            let mut out = String::new();
            on.write(&mut out);
            out
        };
        assert_eq!(named(WaitOn::Lock("db write")), "lock db write");
        assert_eq!(named(WaitOn::Channel("readers gone")), "channel readers gone");
        assert_eq!(named(WaitOn::Io("store pack")), "io store pack");
        assert_eq!(named(WaitOn::Child(42)), "child 42");
        assert_eq!(named(WaitOn::Scope(9)), "scope 9");
        assert_eq!(named(WaitOn::Key("infer_query(Id(7))")), "infer_query(Id(7))");
    }
}
