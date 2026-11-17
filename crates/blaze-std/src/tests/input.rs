//! Standard input ([`In`]) on the simulation: a world whose test scripted `ab\n` and then the
//! input's end (blaze-core `Sim::stdin`, `Sim::stdin_ends`) reads exactly those bytes, then `0`
//! (the end), on every seed, however the sim splits them into short reads (each read gives at
//! least one byte while bytes are left, never more than asked); a read asking for one byte reads
//! `a` alone and the reads after go on from `b`. Plant: [`InPlant::KeepsSlot`] (a read keeps the slot it asked for, not the count
//! that came, so a short read leaves zeros after what came), caught.
//!
//! The Rust Reference does not speak of standard input; the contract is blaze-core's
//! `Effect::ReadStdin` doc: "What the process's standard input holds next, at most `most`
//! bytes ... nothing appended once the input has ended ... Reads are answered in the order
//! asked."

use super::root;
use crate::{
    grant::{Grant, Kind},
    input::{In, InError, InPlant},
    io::Io,
};

/// The seeds the test runs.
const SEEDS: u64 = 64;

/// What a run found wrong.
#[derive(Debug)]
enum Finding {
    /// The world offers no standard input, or is not simulated.
    NoInput,
    /// A read failed.
    Read { seed: u64, at: &'static str, error: InError },
    /// The reads gave other bytes than the scripted ones.
    Bytes { seed: u64, at: &'static str, read: usize, held: [u8; 8], len: usize },
    /// One read said it gave more than it was asked for.
    TooMany { seed: u64, at: &'static str, most: usize, read: usize },
    /// The input did not end within [`READS`] reads.
    NoEnd { seed: u64, at: &'static str },
}

/// The most reads one stretch of the test makes: the sim gives each read at least one byte
/// while bytes are left, so `ab\n` ends within four.
const READS: usize = 8;

/// `io`'s standard input, with `bytes` scripted and then the end.
fn scripted(io: &Io, bytes: &[u8]) -> Result<In, Finding> {
    io.with_sim(|sim| {
        sim.stdin(bytes);
        sim.stdin_ends();
    })
    .ok_or(Finding::NoInput)?;
    io.need_input().map_err(|_| Finding::NoInput)
}

/// `into` against `want`: equal, or the finding naming what the reads held.
fn holds(seed: u64, at: &'static str, read: usize, into: &[u8], want: &[u8]) -> Result<(), Finding> {
    if into == want {
        return Ok(());
    }
    let mut held = [0u8; 8];
    let len = into.len().min(held.len());
    held[..len].copy_from_slice(&into[..len]);
    Err(Finding::Bytes { seed, at, read, held, len: into.len() })
}

/// Reads of at most `most` bytes each until one gives `0` (the end), which must add exactly
/// `want` in all. A read may give fewer bytes than asked (the sim picks how many, as a pipe
/// does), never more, and never `0` while bytes are left.
fn read_to_end(seed: u64, at: &'static str, input: &In, cx: &crate::cx::Cx, most: usize, want: &[u8], plant: InPlant) -> Result<(), Finding> {
    let mut into = cx.bytes();
    let mut total = 0;
    for _ in 0..READS {
        let read = input.read_in(cx, most, &mut into, plant).map_err(|error| Finding::Read { seed, at, error })?;
        if read > most {
            return Err(Finding::TooMany { seed, at, most, read });
        }
        if read == 0 {
            return holds(seed, at, total, into.as_slice(), want);
        }
        total += read;
    }
    Err(Finding::NoEnd { seed, at })
}

/// One seed: `ab\n` read to the end in reads of at most 16 bytes, then the end again; and in a
/// second world a read asking for one byte gets `a` alone, and the rest reads on from `b`.
fn run(seed: u64, plant: InPlant) -> Result<(), Finding> {
    let io = Io::sim(seed);
    let cx = root(&io);
    let input = scripted(&io, b"ab\n")?;
    read_to_end(seed, "the whole input", &input, &cx, 16, b"ab\n", plant)?;
    read_to_end(seed, "after the end", &input, &cx, 16, b"", plant)?;

    let io = Io::sim(seed);
    let cx = root(&io);
    let input = scripted(&io, b"ab\n")?;
    let mut first = cx.bytes();
    let read = input.read_in(&cx, 1, &mut first, plant).map_err(|error| Finding::Read { seed, at: "one byte asked", error })?;
    holds(seed, "one byte asked", read, first.as_slice(), b"a")?;
    read_to_end(seed, "the rest", &input, &cx, 16, b"b\n", plant)
}

/// The invariant, on every seed of [`SEEDS`].
///
/// Prints `test tests::input::stdin_reads_what_the_world_holds_then_ends ... ok`.
#[test]
fn stdin_reads_what_the_world_holds_then_ends() {
    for seed in 0..SEEDS {
        if let Err(finding) = run(seed, InPlant::None) {
            panic!("{finding:?}");
        }
    }
}

/// Control: a read that keeps the slot it asked for, not what came, is caught on every seed.
///
/// Prints `test tests::input::stdin_test_catches_a_read_that_keeps_its_slot ... ok`.
#[test]
fn stdin_test_catches_a_read_that_keeps_its_slot() {
    for seed in 0..SEEDS {
        let found = run(seed, InPlant::KeepsSlot);
        assert!(matches!(found, Err(Finding::Bytes { .. })), "seed {seed}: the planted kept slot was not caught: {found:?}");
    }
}

/// `In` is a grant that fills itself, named in the manifest as `in`.
///
/// Prints `test tests::input::stdin_is_a_grant_that_fills_itself ... ok`.
#[test]
fn stdin_is_a_grant_that_fills_itself() {
    assert_eq!(<In as Grant>::KIND, Kind::In);
    assert!(Kind::In.fills_itself());
    assert!(Kind::fills_itself_named(b"in"));
    assert_eq!(Kind::In.name(), "in");
}
