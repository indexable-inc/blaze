//! The engine's tables as a task reads them (docs/v2/plan/design/engine.md, E2, E2a and E5): a
//! `Copy`, read-only view over every crate's records ([`view::Tables`]), a per-task recorder
//! of each cell a task read ([`recorder::Recorder`]), the body task over them (task.rs), and
//! the frozen heads ([`head`]: one item's heads as a position-independent record with its own
//! type tables, made by a pure function of the item, [`head::freeze`], read into a body's own
//! table, [`head::import`]; [`head::HeadSource`] is the seam the view reads heads through).
//! Nothing mutable is shared between tasks and nothing is kept from one task to the next.

pub mod cost;
#[cfg(test)]
mod enter_once_tests;
#[cfg(test)]
mod failed_once_tests;
#[cfg(test)]
mod fn_text_tests;
pub mod goals;
pub mod head;
#[cfg(test)]
pub(crate) mod head_tests;
#[cfg(test)]
mod local_fns_tests;
pub mod owned_reads;
#[cfg(test)]
mod owned_reads_tests;
pub mod recorder;
#[cfg(test)]
mod recorder_tests;
#[cfg(test)]
mod receiver_freeze_tests;
pub mod seeds;
pub mod task;
pub mod view;
