//! The execution engine (docs/v2/plan/design/engine.md, decision D131) as a sans-IO core: phases
//! over shared tables in place of a machine stepped per task event.
//!
//! | module | what |
//! |---|---|
//! | [`engine`] | the plan, phases, the bulk merge, an edit's dirtying by cell, the answer |
//! | [`last`] | the seam to the last answer, in any stored form |
//! | [`overlay`] | this request's writes over the last answer: every table question |
//! | `last_mem` | the in-memory [`Answer`] behind the seam: `Engine<Answer>` |
//! | [`ready`] | the ready-set driver (D157): the single-threaded core over the engine |
//! | [`shell`] | the core on a host's threads: batches, panics as faults, the deadline |
//!
//! A request is: [`Engine::new_in`] with the last answer (an [`Answer`], or any [`LastAnswer`] form,
//! [`Fold`] for the end) and the [`Edit`] (every planned task
//! of each changed file, made from its text before anything runs); then, phase by phase,
//! [`Engine::next_phase`] names the tasks to enter, a [`Pool`] runs them into per-worker
//! [`Buffer`]s, and [`Engine::merge`] takes the buffers in bulk, sorted by name (rule 25);
//! [`Engine::finish`] gives the new answer. [`drive`] is that loop.
//!
//! Nothing here touches the system, keeps a clock or picks an allocator (rule 75).

#![no_std]
#![feature(allocator_api, register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![deny(blaze::sans_io_core)]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

pub mod cell_table;
pub mod code_pool;
pub mod engine;
pub mod kinds;
pub mod last;
mod last_mem;
pub mod overlay;
pub mod pages;
pub mod readers;
pub mod ready;
pub mod runs;
pub mod shell;

#[cfg(test)]
mod finish_scale_sim;
#[cfg(test)]
mod headers_sim;
#[cfg(test)]
mod last_gen_sim;
#[cfg(test)]
mod last_sim;
#[cfg(test)]
mod owed_by_generation_sim;
#[cfg(test)]
mod owed_rows_sim;
#[cfg(test)]
mod pages_sim;
#[cfg(test)]
mod plan_file_sim;
#[cfg(test)]
mod readers_sim;
#[cfg(test)]
mod ready_sim;
#[cfg(test)]
mod shell_sim;
#[cfg(test)]
mod sim;

pub use crate::engine::{
    Answer, Buffer, Cause, CellDigests, CellId, Done, Edit, Engine, EngineError, Entered, Everything, Finished, Gen, Made, Phase, Plant,
    Planned, Pool, Report, Row, SMap, SSet, STAGES, SVec, Scope, Settle, Stage, StageCount, Stands, Task, Tools, View, by_cell, drive, drive_growing, GROWN_CAP,
    READS_CAP, WAVE_CAP,
};
pub use crate::last::{Found, LastAnswer, LastError, LastRow, ReadOf, Touched, search};
pub use crate::last_mem::{Ask, MemDefines, MemReads, MemSeen, StoredPages};
pub use crate::overlay::{Fold, Generations, Overlay, RowFacts, Taken, Work};
pub use crate::readers::{Edits, FLOOR_BYTES_PER_EDGE, IndexCost, Mapped, Paired, ReaderEdits, ReaderIndex};
pub use crate::runs::{Runs, RunsCost};
