//! What a thread blocks on, said: every blocking wait of a pool thread goes through [`wait`]
//! (or [`Flights`](crate::pump::Flights) for a key), which opens a span `wait` whose `awaits`
//! names it and whose [`Owner`] says who ends it: the job running it, or what stands in a job's
//! place (never nothing). A block that does not leaves its time unaccounted for, and the
//! profile's gate on raw blocks (docs/profiling.md) fails on it.

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

/// Who ends a wait: the job running what it waits for, or what stands in that job's place and
/// why no one job does. Every wait says one (rule every-wait-says-what): a wait's row never
/// leaves its owner unsaid, so a reader of the critical path knows whether a wait it could not
/// cross names no job by its nature (the kernel, any sender) or by a caller's omission
/// ([`Owner::Unsaid`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Owner {
    /// The job running it, as the switches name it (its generation << 32 | its slot,
    /// `sched::observed`).
    Job(u64),
    /// The jobs of the scope waited for: each job's switches name it, and the one that ended the
    /// wait is on the waiter's `ready` row.
    ScopeJobs,
    /// Whoever sends on the channel or signals the condition: no one job is known as the wait
    /// begins. A waiter that learns who woke it says so as the wait ends ([`wait_woken`]).
    Sender,
    /// The kernel: a read, a write or a poll ends when the device or a peer does.
    Kernel,
    /// The child process waited for.
    Child,
    /// The lock's holder: a thread, which the lock's own row names (`lock_waits`), not a job.
    Holder,
    /// Another process: the daemon a client waits on for its answer.
    Daemon,
    /// A key's wait whose caller named no runner: an omission at the call site, said so that it
    /// can be found and fixed, never left null.
    Unsaid,
}

impl Owner {
    /// The job, when the owner is one.
    pub const fn job(self) -> Option<u64> {
        match self {
            Owner::Job(job) => Some(job),
            _ => None,
        }
    }

    /// What kind of owner it is, without the job.
    pub const fn by(self) -> OwnedBy {
        match self {
            Owner::Job(_) => OwnedBy::Job,
            Owner::ScopeJobs => OwnedBy::ScopeJobs,
            Owner::Sender => OwnedBy::Sender,
            Owner::Kernel => OwnedBy::Kernel,
            Owner::Child => OwnedBy::Child,
            Owner::Holder => OwnedBy::Holder,
            Owner::Daemon => OwnedBy::Daemon,
            Owner::Unsaid => OwnedBy::Unsaid,
        }
    }
}

/// The variant of an [`Owner`]: what a wait's row says of who ends it (blaze-profile's
/// `spans.owned_by`), `job` exactly when its `owner` names one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OwnedBy {
    Job,
    ScopeJobs,
    Sender,
    Kernel,
    Child,
    Holder,
    Daemon,
    Unsaid,
}

impl OwnedBy {
    /// Every kind of owner, each at its [`OwnedBy::index`].
    pub const ALL: [OwnedBy; 8] = [
        OwnedBy::Job,
        OwnedBy::ScopeJobs,
        OwnedBy::Sender,
        OwnedBy::Kernel,
        OwnedBy::Child,
        OwnedBy::Holder,
        OwnedBy::Daemon,
        OwnedBy::Unsaid,
    ];

    pub const fn index(self) -> usize {
        self as usize
    }

    pub const fn name(self) -> &'static str {
        match self {
            OwnedBy::Job => "job",
            OwnedBy::ScopeJobs => "the scope's jobs",
            OwnedBy::Sender => "whoever sends",
            OwnedBy::Kernel => "the kernel",
            OwnedBy::Child => "the child",
            OwnedBy::Holder => "the lock's holder",
            OwnedBy::Daemon => "the daemon",
            OwnedBy::Unsaid => "not said",
        }
    }
}

impl WaitKind {
    /// Who ends a wait of this kind when its caller names no job.
    pub const fn unowned(self) -> Owner {
        match self {
            WaitKind::Key => Owner::Unsaid,
            WaitKind::Lock => Owner::Holder,
            WaitKind::Channel => Owner::Sender,
            WaitKind::Io => Owner::Kernel,
            WaitKind::Child => Owner::Child,
            WaitKind::Scope => Owner::ScopeJobs,
        }
    }
}

/// Runs `block`, which blocks on `on`, inside a span `wait` saying so; `owner` is the job that
/// will end the wait, when one is known as it begins. With none, the span says who ends a wait
/// of its kind instead ([`WaitKind::unowned`]).
pub fn wait<R>(on: WaitOn<'_>, owner: Option<u64>, block: impl FnOnce() -> R) -> R {
    let kind = on.kind();
    let waiting =
        Waiting::begin(kind, &mut |it| on.write(it), || owner.map_or(kind.unowned(), Owner::Job));
    let result = block();
    drop(waiting);
    result
}

/// [`wait`] for a wait whose waker is known only as it ends (a loop woken by whichever job
/// posts to it): `block` gives back its result and the job that woke it (as the switches name
/// it, `sched::observed`), which the span then names as its owner, so the critical path crosses
/// from the waiter into that job. `None` leaves the span saying who ends a wait of its kind.
pub fn wait_woken<R>(on: WaitOn<'_>, block: impl FnOnce() -> (R, Option<u64>)) -> R {
    let kind = on.kind();
    let waiting = Waiting::begin(kind, &mut |it| on.write(it), || kind.unowned());
    let (result, by) = block();
    if let Some(by) = by {
        waiting.woken(by);
    }
    drop(waiting);
    result
}

/// Runs `block`, in which a client blocks until its daemon answers `what`, inside a span `wait`
/// (`io <what>`) owned by the daemon ([`Owner::Daemon`]): another process, whose rows of the
/// same build the critical path crosses into at the daemon's answer.
pub fn wait_for_daemon<R>(what: &str, block: impl FnOnce() -> R) -> R {
    let on = WaitOn::Io(what);
    let waiting = Waiting::begin(on.kind(), &mut |it| on.write(it), || Owner::Daemon);
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

    /// A wait given a job names it; one given none says who ends a wait of its kind, each kind
    /// another, and only a key's says "not said". (The invariant that every `wait` row says who
    /// ends it is blaze-profile's, where the rows are: `every_wait_row_says_who_ends_it_...`.)
    #[test]
    fn every_kind_of_wait_has_an_owner_to_say() {
        for kind in WaitKind::ALL {
            let owner = kind.unowned();
            assert_eq!(owner.job(), None);
            assert_ne!(owner.by(), OwnedBy::Job, "{kind:?} with no job never reads as a job's");
            assert_eq!(owner.by() == OwnedBy::Unsaid, kind == WaitKind::Key, "{kind:?}");
        }
        assert_eq!(Owner::Job(7).job(), Some(7));
        assert_eq!(Owner::Job(7).by(), OwnedBy::Job);
        for (at, by) in OwnedBy::ALL.into_iter().enumerate() {
            assert_eq!(by.index(), at);
        }
        let mut names: Vec<&str> = OwnedBy::ALL.iter().map(|it| it.name()).collect(); // alloc: a test's
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), OwnedBy::ALL.len(), "each owner's name is its own");
    }
}
