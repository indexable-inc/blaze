//! The drive's names, keys, records and task types on the new item layer
//! (docs/v2/plan/design/drive.md), which the engine (crates/blaze-engine, blaze-cli engine_*.rs)
//! builds on. The request machine that once stepped them (request.rs and its simulation) is
//! gone: `check`, `run` and `test` start from the engine. Nothing here touches the system, keeps
//! a clock or picks an allocator: each is passed in (rule 75).
//!
//! | module | what |
//! |---|---|
//! | [`key`] | names, sources, keys |
//! | [`record`] | the drive's record envelope: whole, shape, findings, parts |
//! | [`answers`] | the answer table's rows, pages and root: the forward index and the reverse one |
//! | [`task`] | a task, its inputs, its wants, its failure and its printed finding |
//! | [`cells`] | part reads: one cell of a record, and its digest |
//! | [`condition`] | cfg conditions and the space a check covers |
//! | [`profile`] | counts per task kind, on every command |
//! | [`not_run`] | what did not run, grouped by the failure that stopped it |
//! | [`verified`] | bytes read by path, proven to be the swept ones |
//! | [`view`] | a unit's view of a body it shares: the name at a view and the plan (D191) |
//! | [`pointers`] | the checkout's last-answer pointers |
//! | [`table`] | the open-addressing map keyed by content ids |
//! | [`fetch_task`] | the `Fetch` task: one lock entry's sources, its name, record and finding |
//! | [`unit_graph_task`] | the `UnitGraph` task |
//!
//! What a command gives a task (a file's content read by path, a fact such as an environment
//! variable) is a [`Kind::Given`] name like any other ([`key::content_of`], [`key::fact_of`]),
//! found by its name, never an ambient input.
//!
//! # Collections
//!
//! blaze-own's, every one made in the allocator the caller passed; `alloc` is not linked and
//! `Global` is named only in the tests.

#![no_std]
#![feature(allocator_api, register_tool, custom_inner_attributes, variant_count)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![deny(blaze::sans_io_core)]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

pub mod answers;
pub mod cells;
pub mod condition;
pub mod fetch_task;
pub mod key;
pub mod not_run;
pub mod pointers;
pub mod profile;
pub mod record;
pub mod table;
pub mod task;
pub mod unit_graph_task;
pub mod verified;
pub mod view;
mod wire;

pub use crate::cells::{Cell, CellOf, CellRead, CellRefused, Cells, Seen};
pub use crate::condition::{Condition, ConditionError, Space, Var};
pub use crate::key::{Key, Kind, Name, Tool};
pub use crate::record::{Finding, Locate, Part, PathOf};
pub use crate::task::{Class, Command, Input, Print, Task, TaskFailure, Want, Why};
pub use crate::wire::{WireError, WireWhy};

/// Every byte form the drive writes, by name: what a tool's key holds (a bump changes every key,
/// so a record of another form is never fetched, only made again) and what drive.md's formats
/// table lists. A file at a fixed path (`last`) is checked by its magic instead.
pub const FORMATS: [(&str, &[u8; 4]); 4] = [
    ("record", record::MAGIC),
    ("answer page", answers::PAGE_MAGIC),
    ("answer root", answers::ROOT_MAGIC),
    ("last answer", answers::LAST_MAGIC),
];
