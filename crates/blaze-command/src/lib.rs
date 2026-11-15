//! `blaze check --no-daemon`, and `build` and `run` with it ([`run::build`]: the units, then
//! the one program's roots, then the program walked, coded and linked): one process turns a
//! checkout into a request for
//! [`blaze_host::host::run`](blaze_host::run), runs it, files its records under
//! `<home>/v2/keyed` and prints what it did. No daemon, no watcher
//! (docs/v2/plan/design/host.md, target.md).
//!
//! - [`graph`]: the checkout's units in dependency order, the standard library's included,
//!   from blaze-units' own load;
//! - [`leaves`]: the leaf memory: the sweep of the checkout against the last command's table,
//!   the request's reads and their answers, and the table kept for the next command;
//! - [`front`] and [`snapshot`]: the [`blaze_keyed::front::Front`] over the ported front end
//!   (lane K2b's);
//! - [`run`]: the request built and run: plan, reads, late reads, the store, the memo, the
//!   machine;
//! - [`images`]: where `build`'s program images are kept in the home and how the path the user
//!   runs is placed onto one;
//! - [`print`]: what the command says.
//!
//! The order of a command: [`graph::load`] (whose owners are the front end's crate keys), then
//! `front::Command::new(&graph, alloc)`, then [`run::check`] (which sweeps, looks the
//! certificate up, gathers every unit's text through the disk only when it misses, then runs
//! the request), then [`print::summary`], then
//! `front::Command::exit`. Units the command cannot build yet ([`graph::Unit::blocked`]) stop
//! it before anything is read, every one named ([`NOT_BUILT_YET`]).

#![forbid(unsafe_code)]
#![feature(allocator_api, register_tool, custom_inner_attributes)]
// `#![blaze::sans_io]` in each sans-IO module; inert under rustc.
#![register_tool(blaze)]

use std::{
    alloc::{Allocator, Global},
    fmt,
};

use blaze_host::HostError;

pub mod code;
pub mod front;
pub mod graph;
pub mod images;
pub mod leaves;
pub mod print;
pub mod program;
pub mod run;
pub mod snapshot;

#[cfg(test)]
mod front_tests;
#[cfg(test)]
mod tests;

/// What the command says of a unit it stops at because it cannot build it yet.
pub const NOT_BUILT_YET: &str = "not built yet: script runs and macro expansion under the keyed driver";

/// Why the command stopped, naming the thing and the rule.
#[derive(Debug)]
pub enum CommandError {
    /// The workspace could not be resolved into units: `what`, as the load says it.
    Load { what: Box<str, Global> },
    /// A unit of the resolved workspace (`unit`, as the load names it) breaks rule `rule` of
    /// the graph: `what`.
    Graph { unit: Box<str, Global>, rule: &'static str, what: Box<str, Global> },
    /// Unit `unit` of the graph (its index; [`print::error`] names it) breaks rule `rule`.
    Unit { unit: u32, rule: &'static str },
    /// The machine the command runs on breaks rule `rule` (its target, the toolchain).
    Host { rule: &'static str },
    /// Units in the graph this command cannot build yet: every one, each named with why
    /// ([`graph::Blocked`]'s words), one per line: the work order of the next piece.
    NotBuiltYet { count: usize, units: Box<str, Global> },
    /// The front end refused unit `unit` (`item`: the item, empty for the unit itself): `what`.
    Front { unit: Box<str, Global>, item: Box<str, Global>, what: Box<str, Global> },
    /// A look at `path` failed.
    File { path: Box<str, Global>, what: Box<str, Global> },
    /// A file the command keeps between runs (`path`) is not one it reads: `what`.
    Memory { path: Box<str, Global>, what: Box<str, Global> },
    /// The store under `<home>/v2/keyed`.
    Store { what: Box<str, Global> },
    /// The command's certificate look before gathering (`run::check`) stopped: `what` (a
    /// store read, a read the look answers, or a record that is not what its key says).
    Certificate { what: Box<str, Global> },
    /// The host stopped the request.
    Request(HostError),
    /// The program `build` or `run` links (`run::build`) breaks rule `rule`: `what`.
    Program { rule: &'static str, what: Box<str, Global> },
}

impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CommandError::Load { what } => write!(f, "{what}"),
            CommandError::Graph { unit, rule, what } => write!(f, "{unit}: {what} (rule: {rule})"),
            CommandError::Unit { unit, rule } => write!(f, "unit {unit} of the graph: {rule}"),
            CommandError::Host { rule } => write!(f, "this machine: {rule}"),
            CommandError::NotBuiltYet { count, units } => write!(f, "{NOT_BUILT_YET}; {count} units stop it:\n{units}"),
            CommandError::Front { unit, item, what } if item.is_empty() => write!(f, "{unit}: {what}"),
            CommandError::Front { unit, item, what } => write!(f, "{unit}: `{item}`: {what}"),
            CommandError::File { path, what } => write!(f, "{path}: {what}"),
            CommandError::Memory { path, what } => {
                write!(f, "{path}: {what} (rule: a kept file is read whole or refused, never taken as empty)")
            }
            CommandError::Store { what } => write!(f, "the keyed store: {what}"),
            CommandError::Certificate { what } => write!(f, "the certificate's look before gathering: {what}"),
            CommandError::Request(error) => write!(f, "{error}"),
            CommandError::Program { rule, what } => write!(f, "the program: {what} (rule: {rule})"),
        }
    }
}

impl std::error::Error for CommandError {}

impl From<HostError> for CommandError {
    fn from(error: HostError) -> CommandError {
        CommandError::Request(error)
    }
}

/// `text` in `alloc`.
pub(crate) fn boxed<A: Allocator + Clone>(text: &str, alloc: A) -> Box<str, A> {
    blaze_mem::String::from_str_in(text, alloc).into_boxed_str()
}
