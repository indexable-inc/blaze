//! The engine's tables as a task reads them (docs/v2/plan/design/engine.md, E2 and E5): a
//! `Copy`, read-only view over every crate's records ([`view::Tables`]), a per-task recorder
//! of each cell a task read ([`recorder::Recorder`]), and the body task over them (task.rs).
//! Nothing mutable is shared between tasks and nothing is kept from one task to the next.
//!
//! Lane FH writes the frozen heads beside these (head*.rs).

pub mod recorder;
pub mod view;
