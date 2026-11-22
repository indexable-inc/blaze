//! The machines' simulations (rules 43 and 64): each draws a schedule from one tape, drives the
//! real machine through it as its driver, and checks an oracle; every oracle has a planted-bug
//! control that must be caught (`blaze_sim::control`).
//!
//! What they do not prove: the rings' cells here are sequentially consistent, so the ring
//! simulation interleaves the protocol at every atomic operation but cannot reorder within one
//! thread. The orderings watch.md states are the claim a weak-memory checker (blaze-check) is to
//! take over; this simulation proves the protocol's logic, not its fences.

mod helper;
mod pack;
mod resume;
mod ring;
mod since;
mod viewer;

use std::cell::Cell;
use std::fmt::Display;
use std::sync::atomic::Ordering;

use blaze_sim::{Broke, Tape};

use crate::Random;
use crate::ring::Cells;

/// One simulation test's lap, printed as one line on standard error when the test ends (passed
/// or failed: it prints from `Drop`, so a failing test's unwind prints it too; `cargo test --
/// --nocapture` shows it): the test's name, the cases it ran (a control's clean run and its
/// planted one together), its wall time, and the virtual time its cases' clocks advanced in
/// all (`not clocked` for a simulation with no clock). The profile of a slow suite: which test
/// took the time, and whether by many cases or long ones.
pub(crate) struct Lap {
    name: &'static str,
    born: std::time::Instant,
    cases: Cell<u64>,
    virtual_ns: Cell<u64>,
    clocked: Cell<bool>,
}

impl Lap {
    pub(crate) fn new(name: &'static str) -> Lap {
        Lap { name, born: std::time::Instant::now(), cases: Cell::new(0), virtual_ns: Cell::new(0), clocked: Cell::new(false) }
    }

    /// One case ran; `result` is passed through.
    pub(crate) fn case<R>(&self, result: R) -> R {
        self.cases.set(self.cases.get() + 1);
        result
    }

    /// One case ran, its clock having advanced `ns`; `result` is passed through.
    pub(crate) fn clocked<R>(&self, ns: u64, result: R) -> R {
        self.clocked.set(true);
        self.virtual_ns.set(self.virtual_ns.get().saturating_add(ns));
        self.case(result)
    }
}

impl Drop for Lap {
    #[expect(clippy::print_stderr, reason = "a simulation test's lap is one line on the test's own standard error, which cargo's harness captures and `--nocapture` shows")]
    fn drop(&mut self) {
        let wall = self.born.elapsed();
        let cases = self.cases.get();
        let per_case_us = if cases == 0 { 0 } else { wall.as_micros() / u128::from(cases) };
        let virtual_time = match self.clocked.get() {
            true => format!("{:.1} s virtual in all", self.virtual_ns.get() as f64 / 1e9),
            false => "not clocked".to_string(),
        };
        eprintln!("blaze-watch sim lap: {}: {cases} cases, {:.1} ms wall ({per_case_us} us a case), {virtual_time}", self.name, wall.as_secs_f64() * 1e3);
    }
}

/// The harness went wrong: a machine refused a call the simulation should not have made.
pub(crate) fn harness(error: impl Display) -> Broke {
    Broke::Harness(error.to_string())
}

/// An oracle's finding when `holds` is false.
pub(crate) fn oracle(holds: bool, why: impl FnOnce() -> String) -> Result<(), Broke> {
    match holds {
        true => Ok(()),
        false => Err(Broke::Oracle(why())),
    }
}

/// A 32-byte key whose first eight bytes are `n`.
pub(crate) fn key(n: u64) -> [u8; 32] {
    let mut out = [0; 32];
    out[..8].copy_from_slice(&n.to_le_bytes());
    out
}

/// Words the simulation runs every operation on, one at a time, in the order it draws.
pub(crate) struct SimCells {
    words: std::vec::Vec<Cell<u64>>,
}

impl SimCells {
    pub(crate) fn new(len: usize) -> SimCells {
        SimCells { words: (0..len).map(|_| Cell::new(0)).collect() }
    }
}

impl Cells for SimCells {
    fn load(&self, at: usize, _order: Ordering) -> u64 {
        self.words[at].get()
    }

    fn store(&self, at: usize, value: u64, _order: Ordering) {
        self.words[at].set(value);
    }

    fn fence(&self, _order: Ordering) {}
}

/// Randomness drawn from the tape.
pub(crate) struct TapeRandom<'t>(pub(crate) &'t Tape);

impl Random for TapeRandom<'_> {
    fn fill(&mut self, out: &mut [u8]) {
        for byte in out {
            *byte = self.0.below(256, "random byte") as u8;
        }
    }
}

/// Fixed randomness: `n` in every byte.
pub(crate) struct Fixed(pub(crate) u8);

impl Random for Fixed {
    fn fill(&mut self, out: &mut [u8]) {
        out.fill(self.0);
    }
}

/// A key as a page sends it: base64url without padding.
pub(crate) fn base64url(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    let mut bits = 0u32;
    let mut held = 0u32;
    for byte in bytes {
        bits = (bits << 8) | u32::from(*byte);
        held += 8;
        while held >= 6 {
            held -= 6;
            out.push(DIGITS[((bits >> held) & 63) as usize] as char);
        }
    }
    if held > 0 {
        out.push(DIGITS[((bits << (6 - held)) & 63) as usize] as char);
    }
    out
}

/// blaze-run's wire forms as run.md lays them out: the entry's fields at their offsets, and the
/// toggle's CBOR byte for byte.
#[test]
fn blaze_runs_wire_forms_keep_their_layout() {
    use crate::run_wire::{ENTRY_LEN, Entry, RunName, toggle};
    let entry = Entry { name: RunName { pid: 0x0102_0304, start: 5 }, checkout: [6; 32], program: [7; 32], began: 8 };
    let bytes = entry.encode();
    assert_eq!(ENTRY_LEN, 88);
    assert_eq!(bytes[..4], 1u32.to_le_bytes());
    assert_eq!(bytes[4..8], 0x0102_0304u32.to_le_bytes());
    assert_eq!(bytes[8..16], 5u64.to_le_bytes());
    assert_eq!(bytes[16..48], [6; 32]);
    assert_eq!(bytes[48..80], [7; 32]);
    assert_eq!(bytes[80..88], 8u64.to_le_bytes());
    assert_eq!(Entry::decode(&bytes), Ok(entry));
    let mut out = blaze_own::Vec::new_in(std::alloc::Global);
    toggle(&[3, 300], 5, true, &mut out);
    // [1, 0, 5, true, [3, 300]]
    assert_eq!(out[..], [0x85, 0x01, 0x00, 0x05, 0xf5, 0x82, 0x03, 0x19, 0x01, 0x2c]);
}

#[test]
fn a_key_survives_base64url() {
    let lap = Lap::new("a key survives base64url");
    blaze_sim::check(blaze_sim::Config::cases(500), |tape| {
        let mut key = [0u8; 32];
        TapeRandom(tape).fill(&mut key);
        let text = base64url(&key);
        lap.case(oracle(crate::http::key_from_base64url(&text) == Some(key), || format!("{text} did not decode to its key")))
    });
}
