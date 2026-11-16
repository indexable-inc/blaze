//! Every search on every vector this target has, and the word on every target, against a
//! byte-at-a-time search, over haystacks of a small alphabet so that hits fall in every lane of
//! every vector, at every start; and a planted-bug control (rule 53) per vector, a mask that
//! loses each vector's last lane, which the oracle must catch.

use std::{format, vec::Vec};

use blaze_sim::{Broke, Config, Tape};

use crate::{Lanes, Native, Portable, Word, find, find_bytes, memchr_iter, memmem, rfind};

/// The bytes the haystacks are made of: few, so hits are common; the ones the tree searches for.
const ALPHABET: &[u8] = b"ab\n\r\"\\*/";

fn byte(tape: &Tape) -> u8 {
    ALPHABET[tape.index(ALPHABET.len(), "byte")]
}

/// The reference model: the first byte of `haystack` in `needles`, one byte at a time.
fn naive_find(needles: &[u8], haystack: &[u8]) -> Option<usize> {
    haystack.iter().position(|b| needles.contains(b))
}

fn naive_rfind(needles: &[u8], haystack: &[u8]) -> Option<usize> {
    haystack.iter().rposition(|b| needles.contains(b))
}

/// The reference model: the first start of `needle`, compared at every start.
fn naive_find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    let last = haystack.len().checked_sub(needle.len())?;
    (0..=last).find(|&at| haystack[at..at + needle.len()] == *needle)
}

/// The reference model of `find_iter`: the starts that do not overlap.
fn naive_find_all(haystack: &[u8], needle: &[u8]) -> Vec<usize> {
    let mut found = Vec::new();
    let mut at = 0;
    while at <= haystack.len() {
        let Some(start) = naive_find_bytes(&haystack[at..], needle) else { break };
        found.push(at + start);
        at += start + needle.len().max(1);
    }
    found
}

fn same<T: PartialEq + core::fmt::Debug>(what: &str, got: T, want: T) -> Result<(), Broke> {
    if got == want {
        Ok(())
    } else {
        Err(Broke::Oracle(format!("{what}: got {got:?}, the naive search {want:?}")))
    }
}

/// The refinement: each search on the vector `L` equals its byte-at-a-time model.
fn case<L: Lanes>(tape: &Tape) -> Result<(), Broke> {
    let buffer = tape.vec(0..=150, "haystack", byte);
    let start = tape.index(buffer.len() + 1, "start");
    let haystack = &buffer[start..];
    let [a, b, c] = [byte(tape), byte(tape), byte(tape)];
    same(&format!("find {a:?} in {haystack:?}"), find::<L, 1>([a], haystack), naive_find(&[a], haystack))?;
    same(&format!("find {a:?} {b:?} in {haystack:?}"), find::<L, 2>([a, b], haystack), naive_find(&[a, b], haystack))?;
    same(
        &format!("find {a:?} {b:?} {c:?} in {haystack:?}"),
        find::<L, 3>([a, b, c], haystack),
        naive_find(&[a, b, c], haystack),
    )?;
    same(&format!("rfind {a:?} in {haystack:?}"), rfind::<L, 1>([a], haystack), naive_rfind(&[a], haystack))?;
    same(
        &format!("rfind {a:?} {b:?} in {haystack:?}"),
        rfind::<L, 2>([a, b], haystack),
        naive_rfind(&[a, b], haystack),
    )?;
    let needle: Vec<u8> = if !haystack.is_empty() && tape.coin("a needle drawn apart from the haystack") {
        tape.vec(0..=6, "needle", byte)
    } else {
        let from = tape.index(haystack.len() + 1, "needle start");
        let len = tape.index(haystack.len() - from + 1, "needle length").min(20);
        haystack[from..from + len].to_vec()
    };
    same(
        &format!("find {needle:?} in {haystack:?}"),
        find_bytes::<L>(haystack, &needle),
        naive_find_bytes(haystack, &needle),
    )
}

/// The public paths, on this target's vector: the iterators from both ends and the finder.
fn iterators(tape: &Tape) -> Result<(), Broke> {
    let haystack = tape.vec(0..=150, "haystack", byte);
    let needle = byte(tape);
    let want: Vec<usize> = (0..haystack.len()).filter(|&at| haystack[at] == needle).collect();
    same("memchr_iter forward", memchr_iter(needle, &haystack).collect::<Vec<_>>(), want.clone())?;
    let mut backward: Vec<usize> = memchr_iter(needle, &haystack).rev().collect();
    backward.reverse();
    same("memchr_iter backward", backward, want)?;
    let bytes = tape.vec(0..=3, "needle", byte);
    same(
        &format!("find_iter {bytes:?} in {haystack:?}"),
        memmem::find_iter(&haystack, &bytes[..]).collect::<Vec<_>>(),
        naive_find_all(&haystack, &bytes),
    )?;
    same("a finder", memmem::Finder::new(&bytes[..]).find(&haystack), naive_find_bytes(&haystack, &bytes))
}

#[test]
fn every_search_on_the_word_is_the_naive_search() {
    blaze_sim::check(Config::cases(500), case::<Word>);
}

#[test]
fn every_search_on_the_portable_vector_is_the_naive_search() {
    blaze_sim::check(Config::cases(500), case::<Portable<16>>);
    blaze_sim::check(Config::cases(500), case::<Portable<32>>);
}

/// The vector and its scalar twin give one answer at every start of one drawn buffer, across
/// more than two of the widest vector's four-vector steps, for every search.
fn alignments<L: Lanes>(tape: &Tape) -> Result<(), Broke> {
    let buffer = tape.vec(0..=300, "haystack", byte);
    let [a, b, c] = [byte(tape), byte(tape), byte(tape)];
    let needle = tape.vec(2..=5, "needle", byte);
    for start in 0..=buffer.len() {
        let haystack = &buffer[start..];
        let at = format!("at start {start} of {buffer:?}");
        same(&format!("find {a:?} {at}"), find::<L, 1>([a], haystack), find::<Word, 1>([a], haystack))?;
        same(&format!("find3 {a:?} {b:?} {c:?} {at}"), find::<L, 3>([a, b, c], haystack), find::<Word, 3>([a, b, c], haystack))?;
        same(&format!("rfind2 {a:?} {b:?} {at}"), rfind::<L, 2>([a, b], haystack), rfind::<Word, 2>([a, b], haystack))?;
        same(&format!("find {needle:?} {at}"), find_bytes::<L>(haystack, &needle), find_bytes::<Word>(haystack, &needle))?;
    }
    Ok(())
}

#[test]
fn the_vector_and_the_word_agree_at_every_alignment() {
    blaze_sim::check(Config::cases(100), alignments::<Native>);
    blaze_sim::check(Config::cases(100), alignments::<Portable<32>>);
}

#[test]
fn the_iterators_and_the_finder_are_the_naive_search() {
    blaze_sim::check(Config::cases(300), iterators);
}

/// A vector whose mask loses the last lane: the planted bug.
#[derive(Clone, Copy)]
struct DropsLastLane<L>(L);

impl<L: Lanes> Lanes for DropsLastLane<L> {
    type Hit = L::Hit;
    const WIDTH: usize = L::WIDTH;
    const BITS: u32 = L::BITS;

    fn splat(byte: u8) -> Self {
        DropsLastLane(L::splat(byte))
    }

    fn load(bytes: &[u8]) -> Self {
        DropsLastLane(L::load(bytes))
    }

    fn eq(self, other: Self) -> L::Hit {
        self.0.eq(other.0)
    }

    fn or(a: L::Hit, b: L::Hit) -> L::Hit {
        L::or(a, b)
    }

    fn and(a: L::Hit, b: L::Hit) -> L::Hit {
        L::and(a, b)
    }

    fn mask(hit: L::Hit) -> u64 {
        let lane = (1u64 << L::BITS) - 1;
        let last = u32::try_from(L::WIDTH - 1).expect("a vector of under 2^32 lanes") * L::BITS;
        L::mask(hit) & !(lane << last)
    }
}

#[derive(Clone, Copy, Debug)]
enum Plant {
    Clean,
    DropsLastLane,
}

#[test]
fn the_oracle_catches_a_word_mask_that_drops_the_last_lane() {
    let run = |tape: &Tape, plant: Plant| match plant {
        Plant::Clean => case::<Word>(tape),
        Plant::DropsLastLane => case::<DropsLastLane<Word>>(tape),
    };
    blaze_sim::control(Config::cases(500), Plant::Clean, Plant::DropsLastLane, "the naive search", run);
}

#[test]
fn the_oracle_catches_a_native_mask_that_drops_the_last_lane() {
    let run = |tape: &Tape, plant: Plant| match plant {
        Plant::Clean => case::<Native>(tape),
        Plant::DropsLastLane => case::<DropsLastLane<Native>>(tape),
    };
    blaze_sim::control(Config::cases(500), Plant::Clean, Plant::DropsLastLane, "the naive search", run);
}
