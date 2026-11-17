//! The driver of one command on the new item layer (docs/v2/plan/design/drive.md), as a sans-IO
//! machine: [`Request`] takes [`Event`]s and pushes [`Effect`]s; the caller (crates/blaze-cli)
//! performs them in any order. Nothing here touches the system, keeps a clock or picks an
//! allocator: each is passed in (rule 75).
//!
//! | module | what |
//! |---|---|
//! | [`key`] | names, sources, keys, the certificate key |
//! | [`record`] | the drive's record envelope: whole, shape, findings, parts |
//! | [`answers`] | the answer table's rows, pages and root: the forward index and the reverse one |
//! | [`request`] | the request machine |
//! | [`lines`] | a file's line table: `file:line:col` when printed |
//! | [`profile`] | counts per task kind, on every command |
//! | [`items`] | the production `Items` and `Source` over the records a task holds |
//! | [`fetch_task`] | the `Fetch` task: one lock entry's sources, its name, record and finding |
//! | [`run`] | blaze-run's compiles and records as drive names |
//!
//! # Collections
//!
//! blaze-own's, every one made in the allocator the caller passed; `alloc` is not linked and
//! `Global` is named only in the tests.

#![no_std]
#![feature(allocator_api, register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![deny(blaze::sans_io_core)]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

pub mod answers;
pub mod condition;
pub mod fetch_task;
pub mod items;
pub mod key;
pub mod lines;
pub mod pointers;
pub mod profile;
pub mod record;
pub mod request;
pub mod run;
pub mod settled;
pub mod table;
pub mod unit_graph_task;
pub mod verified;
pub mod worker;
mod wire;

#[cfg(test)]
mod sim;

pub use crate::condition::{Condition, ConditionError, Space, Var};
pub use crate::key::{Key, Kind, Name, Tool};
pub use crate::record::{Finding, Locate, Part, PathOf};
pub use crate::request::{
    Answer, Ask, Batch, BatchId, Builder, Class, Command, Config, DriveError, Effect, Event, FileEntry, Gate, Input,
    Outcome, Output, Print, Ran, Request, Task, TaskFailure, Ticket, Verdict, Want, Why,
};
pub use crate::settled::{Settled, TableFull};
pub use crate::wire::{WireError, WireWhy};

/// Every byte form the drive writes, by name: what a tool's key holds (a bump changes every key,
/// so a record of another form is never fetched, only made again) and what drive.md's formats
/// table lists. A file at a fixed path (`last`) is checked by its magic instead.
pub const FORMATS: [(&str, &[u8; 4]); 5] = [
    ("record", record::MAGIC),
    ("answer page", answers::PAGE_MAGIC),
    ("answer root", answers::ROOT_MAGIC),
    ("last answer", answers::LAST_MAGIC),
    ("certificate", request::CERT_MAGIC),
];

/// A bug the simulation plants to show its oracle catches it (`blaze_sim::control`). Always
/// `None` outside the tests; public so a caller's simulation can plant it too.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Plant {
    None,
    /// A settled record whose out digest changed does not dirty its readers: the validity check
    /// skips a changed read.
    SkipChangedRead,
    /// A name with no record (never made, or dropped) keeps no watch row: the readers that read
    /// it absent are forgotten, so its appearing reruns none of them.
    ForgetAbsentReaders,
    /// A read that found nothing is handed to the task's next run as a hint: the name, still
    /// not there, fails, and its reader fails `Waited` on it every run.
    HintAbsentReads,
    /// A want of a name whose task is running starts a second run of the same key.
    DoubleFlight,
    /// A batch's records are applied a second time, past the ticket check.
    BatchTwice,
    /// A settled row is published to the workers' table ahead of its record's `Put`: a worker
    /// can answer a need before the record is written.
    PublishBeforePut,
    /// A program's success is reported at its exit, before the gate's check is done.
    EarlySuccess,
    /// A record is stored under the condition its job began at, not the one it read: an asker
    /// that does not imply what it read is served it.
    RanUnderCondition,
    /// A file's row keeps its part's place in its home, not its edition: a repeat whose home
    /// stood reads the file in the edition its place spells.
    EditionAsPlace,
    /// A run's answer is given while the program it started is still running (stopped or
    /// not): the caller then steps the answered machine with the program's exit.
    AnswerWhileRunning,
    /// `test` starts the package's program as `run` does: `main` is no test, and until tests
    /// run on this path `test` is its check alone (decision D61 item 5).
    TestStartsProgram,
    /// One finding held by two names' records (a library's and its test unit's names) prints
    /// and counts once a name.
    PrintEachName,
    /// A settled job keeps the needs its runs were handed, at the capacity they grew to (the
    /// clone's 5.2 GB of `Name` lists, 2026-11-16).
    KeepSettledNeeds,
    /// Puts are dropped once the request holds a failure: a request with findings would leave
    /// nothing it computed for the next one (the coordinator's rule, 2026-11-16 20:10).
    DropPutsOnFailure,
    /// A task that read a file by path parses its bytes without comparing them with the
    /// content id its key was made from (`verified::verify`).
    SkipContentCompare,
}
