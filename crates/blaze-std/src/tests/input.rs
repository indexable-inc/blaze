//! Standard input ([`In`]) on the simulation: a world whose test scripted `ab\n` and then the
//! input's end (blaze-core `Sim::stdin`, `Sim::stdin_ends`) reads exactly those bytes, then `0`
//! (the end), on every seed; a read asking for one byte reads `a` alone and the next read goes
//! on from `b`. Plant: [`InPlant::KeepsSlot`] (a read keeps the slot it asked for, not the count
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
    /// A read gave other bytes than the scripted ones.
    Bytes { seed: u64, at: &'static str, read: usize, held: [u8; 8], len: usize },
}

/// `io`'s standard input, with `bytes` scripted and then the end.
fn scripted(io: &Io, bytes: &[u8]) -> Result<In, Finding> {
    io.with_sim(|sim| {
        sim.stdin(bytes);
        sim.stdin_ends();
    })
    .ok_or(Finding::NoInput)?;
    io.need_input().map_err(|_| Finding::NoInput)
}

/// One read of at most `most` bytes, which must add exactly `want`.
fn read_exactly(seed: u64, at: &'static str, input: &In, cx: &crate::cx::Cx, most: usize, want: &[u8], plant: InPlant) -> Result<(), Finding> {
    let mut into = cx.bytes();
    let read = input.read_in(cx, most, &mut into, plant).map_err(|error| Finding::Read { seed, at, error })?;
    if read == want.len() && into.as_slice() == want {
        return Ok(());
    }
    let mut held = [0u8; 8];
    let len = into.len().min(held.len());
    held[..len].copy_from_slice(&into.as_slice()[..len]);
    Err(Finding::Bytes { seed, at, read, held, len: into.len() })
}

/// One seed: `ab\n` read whole then the end; and in a second world, one byte then the rest.
fn run(seed: u64, plant: InPlant) -> Result<(), Finding> {
    let io = Io::sim(seed);
    let cx = root(&io);
    let input = scripted(&io, b"ab\n")?;
    read_exactly(seed, "the whole input", &input, &cx, 16, b"ab\n", plant)?;
    read_exactly(seed, "after the end", &input, &cx, 16, b"", plant)?;

    let io = Io::sim(seed);
    let cx = root(&io);
    let input = scripted(&io, b"ab\n")?;
    read_exactly(seed, "one byte asked", &input, &cx, 1, b"a", plant)?;
    read_exactly(seed, "the rest", &input, &cx, 16, b"b\n", plant)?;
    read_exactly(seed, "after the end", &input, &cx, 16, b"", plant)
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
