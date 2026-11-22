//! A task as the engine hands it to a worker and hears it back (drive.md, "Tasks and Need"): the
//! command it serves, the records it is handed ([`Input`]), what it asks for ([`Want`]), why it
//! failed ([`TaskFailure`]), the finding it prints ([`Print`]) and the drive's own finding rules.
//!
//! These are the types the old request machine (request.rs, deleted 2026-11-19 with its
//! simulation) shared with the engine (crates/blaze-engine, blaze-cli engine_*.rs); the machine
//! itself is gone, and `check`, `run` and `test` start from the engine.

use core::fmt;

use blaze_id::ContentId;
use blaze_own::{Alloc, Vec};

use crate::condition::Condition;
use crate::key::{Key, Name};

/// What the command asks.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Command {
    Check,
    Run,
    Test,
}

/// A name a task needs, and the record that lists it (its file's `FileItems`, a `Fetch`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Want {
    pub name: Name,
    pub home: Name,
}

/// A task that could not answer: not a user's error (that is a finding), the tool's. It fails
/// that one job (rule 20): the command goes on, every other record is written, and the answer
/// is not, so the next command runs the job again.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct TaskFailure {
    pub name: Name,
    pub why: Why,
}

/// Why a task failed, typed (rule 19); the caller names the file.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Why {
    /// No task of the name's kind is built on the new item layer yet (drive.md, "Tasks and Need").
    NotBuilt,
    /// The task panicked (its message went to the error stream): an allocation its frame
    /// refused, or a bug. `used` is the most its scratch frame held, `limit` that frame's size.
    Panicked { used: u64, limit: u64 },
    /// The task's input is one this build refuses: `what`, under `rule`.
    Refused { what: &'static str, rule: &'static str },
    /// A task this one needs failed this command (its own failure is printed); this one has
    /// no record and prints nothing of its own: the closing note counts it.
    Waited { on: Name },
    /// The task's file (its name's) changed between the sweep and the task's read: the bytes
    /// read are not the content id its key holds (`verified::verify`). It puts nothing, and the
    /// command's answer is refused naming the file: no re-read, no retry (rule 3).
    Changed,
}

impl fmt::Display for TaskFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.why {
            Why::NotBuilt => write!(f, "{}: this task kind is not built on the new item layer yet (drive.md, \"Tasks and Need\")", self.name),
            Why::Panicked { used, limit } => write!(
                f,
                "{}: the task panicked (its message is above) with {used} of its frame's {limit} bytes used; it failed alone (rule 20)",
                self.name
            ),
            Why::Refused { what, rule } => write!(f, "{}: {what} ({rule})", self.name),
            Why::Waited { on } => write!(f, "{}: needs {on}, which failed", self.name),
            Why::Changed => write!(f, "{}: changed while the command ran; run again", self.name),
        }
    }
}

/// Which work a task is: the person's (what the command asked) or background (speculative code,
/// the optimizing tier), which runs only on workers the person's work does not need (adapted
/// from makepad/relax (MIT), libs/cpu_budget/src/lib.rs).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Class {
    /// What the command asked: `check`'s walk, or what a running program waits on.
    Foreground,
    /// `run`'s and `test`'s check of the program's scope beside it (drive.md, "The gate"):
    /// after what the program waits on, before speculative work.
    Gate,
    Background,
}

/// A record a task is handed: where it is and the digest a read of it records.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Input {
    pub name: Name,
    pub at: Key,
    pub out: ContentId,
}

/// A task to run.
#[derive(Debug)]
pub struct Task<'a> {
    /// The job's name: what its record is of. Under a condition it is a fork's name
    /// (`key::at_condition`), and the item is [`Task::base`].
    pub name: Name,
    /// The item the task is about: `name` under [`Condition::ANY`]. Workers find the item, and
    /// name what they read and want, by base names.
    pub base: Name,
    /// The cfg values the task may read.
    pub condition: Condition,
    pub key: Key,
    pub source: ContentId,
    pub class: Class,
    /// How many times it has been entered before (0 the first time).
    pub entry: u32,
    pub home: Name,
    pub place: u32,
    /// A `FileItems` task's file, an index into the command's path table.
    pub file: Option<u32>,
    /// The edition its file is read in: a `FileItems` task's own file's, an item's task's
    /// home file's; 0 for a task of no file.
    pub edition: u8,
    /// Its inputs: last time's read list first, then what it needed since.
    pub inputs: Vec<Input, Alloc<'a>>,
    /// What it needed that settled with no record: each's task failed this command (rule 20),
    /// sorted. A task tells "not asked yet" from "asked and failed" by it, so it never asks
    /// again for what failed (drive.md, "Owed, step 1").
    pub failed: Vec<Name, Alloc<'a>>,
    /// A `Group`'s members.
    pub members: Vec<Name, Alloc<'a>>,
}

/// A finding to print: the caller places it with the file's line table.
#[derive(Debug)]
pub struct Print<'a> {
    /// The item (a base name, never a fork's).
    pub name: Name,
    /// The other names whose records held this same finding, printed once under `name` (a
    /// crate's finding that a library and its test unit both hold), by name: a task that waited
    /// on one of them waited on this failure.
    pub also: Vec<Name, Alloc<'a>>,
    /// The configurations it holds in: every one that implies this (bottom.md 11: a finding
    /// carries the reduced condition it was found under).
    pub condition: Condition,
    /// The file it is in, an index into the command's path table; `None` for a manifest's or a
    /// crate's.
    pub file: Option<u32>,
    /// Its item's start in the file.
    pub place: u32,
    pub rule: u16,
    pub start: u32,
    pub end: u32,
    pub message: Vec<u8, Alloc<'a>>,
}

/// The rule of a `mod x;` whose file is at neither `x.rs` nor `x/mod.rs` (arguments: the two
/// paths, `\x1f` between); an error under every command.
pub const MODULE_FILE_MISSING_RULE: u16 = 0xC0F7;
/// The rule of a `#[path]` module whose file is not in the checkout (argument: the path).
pub const PATH_FILE_MISSING_RULE: u16 = 0xC0F8;
