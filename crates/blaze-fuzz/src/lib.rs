//! Bytes nobody vouches for, for the tests of what reads the store: a pack, an entry and
//! LLVM's code are read from files and from other machines.
//!
//! [`inputs`] gives the files of a corpus directory, the seeds a test builds, and changes of
//! both, the same ones in every run. [`largest_allocation`] tells how much memory a read
//! asked for at once, so that a length in the bytes is seen not to be believed. It counts
//! through `blaze-alloc-count`, so a test binary that calls it installs
//! `blaze_alloc_count::Counting` as its global allocator, as budget tests do: it is for tests
//! alone.

#![forbid(unsafe_code)]

use std::path::Path;

use blaze_ids::rng::SplitMix64;

/// How many changed inputs a target is given, unless `BLAZE_FUZZ_RUNS` says.
const RUNS: usize = 20_000;

/// Bytes that are a length, a tag or an index somewhere, at their edges.
const EDGES: [u8; 8] = [0, 1, 2, 0x7f, 0x80, 0xfe, 0xff, 0x40];

/// The inputs for the corpus in `dir`: its files, `seeds`, and changes of them.
pub fn inputs(dir: impl AsRef<Path>, seeds: Vec<Vec<u8>>) -> impl Iterator<Item = Vec<u8>> {
    let dir = dir.as_ref();
    let target = dir.file_name().map_or(0, |it| it.len());
    let mut known = corpus(dir);
    known.extend(seeds);
    let runs = std::env::var("BLAZE_FUZZ_RUNS").ok().and_then(|it| it.parse().ok());
    let mut random = Random(SplitMix64::new(0x9e37_79b9_7f4a_7c15 ^ target as u64));
    let changed: Vec<Vec<u8>> = (0..runs.unwrap_or(RUNS))
        .map(|run| {
            let mut bytes = known[run % known.len()].clone();
            for _ in 0..=random.0.below(4) {
                random.change(&mut bytes, &known);
            }
            bytes
        })
        .collect();
    known.into_iter().chain(changed)
}

/// The files of `dir`, each the hexadecimal of an input. Space in them is for the reader.
fn corpus(dir: &Path) -> Vec<Vec<u8>> {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .map(|it| it.unwrap().path())
        .collect();
    files.sort();
    files.iter().map(|file| unhex(&std::fs::read_to_string(file).unwrap())).collect()
}

fn unhex(text: &str) -> Vec<u8> {
    let digits: Vec<u8> = text.bytes().filter(|it| !it.is_ascii_whitespace()).collect();
    digits
        .chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

/// The changes of inputs, the same in every run.
struct Random(SplitMix64);

impl Random {
    fn edge(&mut self) -> u8 {
        EDGES[self.0.index(EDGES.len())]
    }

    /// Changes `bytes` in one of the ways a damaged or a made-up input differs from a
    /// written one.
    fn change(&mut self, bytes: &mut Vec<u8>, known: &[Vec<u8>]) {
        let at = self.0.index(bytes.len());
        match self.0.below(8) {
            0 => bytes.truncate(at),
            1 if !bytes.is_empty() => bytes[at] ^= 1 << self.0.below(8),
            2 if !bytes.is_empty() => bytes[at] = self.edge(),
            3 => bytes.insert(at, self.edge()),
            4 if !bytes.is_empty() => _ = bytes.remove(at),
            // A length that says there is far more than there is.
            5 => _ = bytes.splice(at..at, [0xff; 9]),
            6 => {
                let other = &known[self.0.index(known.len())];
                let from = self.0.index(other.len());
                bytes.splice(at.., other[from..].iter().copied());
            }
            _ => {
                for _ in 0..self.0.below(16) {
                    let byte = self.0.next_u64() as u8;
                    bytes.insert(self.0.index(bytes.len() + 1), byte);
                }
            }
        }
    }
}

/// The most bytes `read` asked for at once on this thread, and what it returned. Panics when
/// the binary's allocator is not `blaze_alloc_count::Counting`, whose zero would pass any bound.
pub fn largest_allocation<T>(read: impl FnOnce() -> T) -> (usize, T) {
    assert!(
        blaze_alloc_count::installed(),
        "largest_allocation: this test binary's #[global_allocator] is not blaze_alloc_count::Counting"
    );
    let (read, used) = blaze_alloc_count::measure(read);
    (used.largest as usize, read)
}

/// The most a read of `len` bytes may ask for at once: serde makes room for at most a
/// megabyte of what a length promises before the bytes are there.
pub fn allowed(len: usize) -> usize {
    (1 << 20) + 64 * len
}
