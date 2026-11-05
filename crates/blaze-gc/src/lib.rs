//! blaze's collector of what it keeps on disk (docs/gc.md): the per-workspace build dirs
//! (`<home>/build/<workspace id>/`, a profile each below), the machine's shared standard-library
//! stores (`<machine>/std/<makers>/`), temporaries left behind, and its own trash.
//!
//! - [`plan::Collector`] is the core: a synchronous state machine that reads time only from
//!   [`plan::Collector::advance_clock`], owns no file, thread or timer, and hands back
//!   [`plan::Decision`]s and a deadline ([`plan::Collector::next_deadline`]). It decides when a
//!   slice runs and how big it is (paced by each volume's free space, [`pace`]; an idle daemon;
//!   a period) and, given what a scan found, what
//!   goes and why.
//! - [`fs`] is the driver: it scans, renames into trash and unlinks with `std::fs`, on whatever
//!   thread it is called on (the daemon calls it from a background job of the pool, never from
//!   its loop or a build's path).
//! - [`lease`] is what keeps a directory in use: a shared `flock` on its `.blaze-lease`, held by
//!   every process that uses it, which the collector must take exclusively, without waiting,
//!   before it renames the directory away.
//! - [`record`] is what a build dir says of itself (its checkout, when it was last used), and
//!   [`ledger`] what the collector keeps of its runs across restarts.
//!
//! # Why nothing in use is removed
//!
//! 1. Only directories of the classes in [`plan::Class`] are ever touched, and only under the
//!    areas the driver is given; the daemon's state (`<home>/v2`, its store and image) is never
//!    listed, so never a candidate.
//! 2. A directory is removed by one `rename` into `<area>/.blaze-trash/` (which carries blaze's
//!    marker, [`fs::TRASH_MARK`]; one without it is refused), on the same file system, and only
//!    while the collector holds an exclusive lock on its `.blaze-lease`, taken with `try_lock`
//!    (never waiting) and checked to be the directory's own lease by the nonce it holds (a lease
//!    file of a directory renamed and made again is not). Every user of a directory holds a
//!    shared lock on that file for as long as it uses it ([`lease::Lease`]), so the rename
//!    happens only when no user exists. A user that opens the lease while the collector holds
//!    it waits at most [`lease::SHARED_WAIT`], then sees by the nonce that its directory is gone
//!    and makes it afresh. Network file systems, whose locks do not hold, are refused.
//!    While the lock is held the daemon's job checks its [`fs::Gate`]: a lease taken in the
//!    process or a head changed since the rename was asked for, or the run over, stops it.
//! 3. A directory with no lease file, or whose record is not of a format whose users take
//!    leases, is refused, never collected ([`plan::Keep::Refused`]).
//! 4. Roots the daemon knows (live heads, pins, roots an agent holds, the last transactions of a
//!    workspace) are checked twice: when the run marks, and again just before each rename is
//!    asked for. Anything rooted or used since the mark ([`plan::Keep::UsedAfterMark`]) stays.
//! 5. A crash leaves each directory either where it was or whole in trash; trash is unlinked by
//!    a later step of the same or the next run. Nothing outside trash is ever partly removed,
//!    and in trash only entries named as blaze names them ([`fs::is_trash_name`]) are listed or
//!    unlinked.
//! 6. A run is refused when no build dir was used within the TTL of now (a clock set forward),
//!    and a scan that cannot tell whether a checkout is gone refuses its workspace.

// One module asks the kernel for a file system's kind (`statfs`), which needs `unsafe`.
#![deny(unsafe_code)]
#![feature(allocator_api)]
// `#![blaze::sans_io]` marks a sans-IO core for `blaze::sans_io_core`; inert under rustc.
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]

pub mod fs;
pub mod lease;
pub mod ledger;
mod netfs;
pub mod pace;
pub mod plan;
pub mod record;
pub mod settings;
pub mod space;

#[cfg(test)]
mod fs_tests;
#[cfg(test)]
mod pace_tests;
#[cfg(test)]
mod tests;

pub use crate::{
    ledger::Ledger,
    plan::{
        Class, Collector, Config, Decision, Inventory, Keep, Object, Outcome, Reason, RunReport,
        Trigger, Verdict,
    },
};
