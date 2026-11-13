//! A command alone drives the kernel (docs/v2/plan/design/end-state.md, the no-daemon end
//! state, piece 1): every command is a process that reads the checkout's on-disk state, redoes
//! only what changed, saves, and exits. No daemon, no watcher, no lease.
//!
//! - [`table`]: the leaf table, one a checkout: sorted rows `(path, stamp, content id or not
//!   hashed yet, kind)` for every file and directory of the view, and the instant it was
//!   looked at. Written whole to a `.part` and renamed; a table that does not decode is an
//!   error naming the file, never an empty table.
//! - [`sweep`]: the table against the disk: every row stamped, the directories whose stamp
//!   moved (or is racy) listed, a racy or moved hashed row hashed again, git's racy rule by
//!   [`blaze_vfs::racy`]. Out: the rows now, and the paths added, removed and changed.
//! - [`world`]: the kernel's [`blaze_kernel::World`] over the rows, for the front end's
//!   reads; its pin and what it covers.
//! - [`root`]: the session root, a file of two slots a (checkout, registry), saved under an
//!   exclusive lock only if no other command saved since this one opened.
//! - [`run`]: the command as one state machine: load the table and the slots, sweep, hash
//!   what is read, open the session, carry it by the exact change, ask `items` of the files,
//!   save, write the table.
//! - [`drive`]: the thin driver over any [`blaze_core::Driver`]; the binary
//!   (`src/bin/solo.rs`) is it over the native driver and blaze-cas's store.
//!
//! Every module but [`drive`] is a sans-IO core (rule 14): no file, clock or store is touched
//! there; each look at the disk is an effect the driver carries out.
//!
//! # The floors (rule 67), and where this is above them
//!
//! With no change: one stat a row, one read of the table and of the slot file, 0 hashes, 0
//! store reads beyond the session root. The design does exactly that IO: the rows are stamped
//! in batches of [`run::STAMP_BATCH`] (a directory is listed only when its own stamp moved or
//! is racy), the table and the slot file are each read once, no row is hashed unless its
//! stamp moved or is racy, and the session's root record is the one store read
//! ([`blaze_kernel::Session::open`]; with [`run::Ask::Changed`] nothing is asked, so no entry
//! is read). Above the floor: the table is decoded whole into rows and its pin is a hash
//! pass over them (CPU over the table's bytes, no IO), where a table read in place would
//! touch only the rows it compares; nothing is written (the table and the slot are written
//! only when something in them moved).
//!
//! One file edited: that, plus one read and one hash of the file, its `parse` and `items`, and
//! the postings of its changed entries. The design reads the file once through a 64 KiB
//! buffer ([`run::CHUNK`]) and hashes it as it reads (the bytes kept for the parse), carries
//! the session with the file as the one exact difference, and asks `items` of it alone.
//! Above the floor: one more stat of the file after its read (the stamp, read, stamp rule:
//! the first stamp is the sweep's, so the cost is one stat, not two); the table is rewritten
//! whole (O(rows) bytes, where the floor writes the one row); and the store's flush and the
//! slot write. A file written inside its racy window (within 20 ms on a file system with
//! sub-second stamps, 2 s on a coarse one) is hashed again by every run until a run's look
//! is past its window: git's cost too, the price of never trusting a stamp that cannot vouch.
//!
//! The steps' code identity is a parameter ([`Codes`]); the binary gives the daemon's carry's
//! (`src/bin/solo.rs`, `codes`), so an entry either driver keeps is the other's hit. A
//! session root saved by the daemon is not one this crate opens (the daemon's pin is a VFS
//! root's, this crate's the table's): the two share entries in the store, not sessions.
//!
//! Above both floors in the binary: the store's single writer (`store.lock`, held the whole
//! command) and its open; a second command on one state is refused, where the floor would let
//! two read at once.

#![forbid(unsafe_code)]
// `#![blaze::sans_io]` (on each core module) marks a sans-IO core for `blaze::sans_io_core`;
// inert under rustc.
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]

pub mod drive;
pub mod root;
pub mod run;
pub mod sweep;
pub mod table;
pub mod world;

#[cfg(test)]
mod tests;

pub use crate::run::{Ask, Codes, Command, Config, Outcome, Phases, Poll, Report, SoloError};

/// A bug planted in a command, for a test's control: with it the command gives an answer or
/// keeps a state it must not, and the simulation's oracle says so. Never planted outside a
/// test.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Plant {
    None,
    /// A hashed row whose stamp is equal but racy is trusted ([`sweep`]).
    TrustsRacy,
    /// The slot is saved without checking that its sequence is still the one opened
    /// ([`run`]).
    SkipsSequence,
    /// A row whose file is gone is kept ([`sweep`]).
    KeepsRemoved,
    /// The slot naming the new root is written before the store's flush made the run's puts
    /// durable ([`run`]): a crash between them leaves a slot naming records the store lost.
    SlotBeforeFlush,
}

#[cfg(any(test, feature = "plants"))]
thread_local! {
    pub static PLANT: std::cell::Cell<Plant> = const { std::cell::Cell::new(Plant::None) };
}

#[cfg(any(test, feature = "plants"))]
pub(crate) fn planted(plant: Plant) -> bool {
    PLANT.get() == plant
}

#[cfg(not(any(test, feature = "plants")))]
pub(crate) fn planted(plant: Plant) -> bool {
    let _ = plant;
    false
}
