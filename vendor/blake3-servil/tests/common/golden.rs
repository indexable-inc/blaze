//! A sweep's expected outputs, fixed in the source as their fold: a test
//! adds each case's output and checks the fold against a constant that
//! official BLAKE3 1.8.7 generated, so it needs no implementation to
//! compare against. Shared by the library's tests (src/test.rs includes
//! this file) and the integration tests (`mod common;`).
//!
//! Regenerating is a reviewed step, run by hand: with `BLAKE3_GOLDEN=1`
//! set, every `add` computes its case with official BLAKE3 too and stops at
//! the first that differs, and `check` prints the fold to write (`golden
//! NAME: 0x...`). Where a test runs several platforms, the first's outputs
//! are added and the others must equal them, so the constant is the same
//! on every machine.

// Named in full: the library is no_std, its tests std.
#[allow(unused_imports)]
use std::{println, vec::Vec};

pub struct Golden {
    name: &'static str,
    ours: Vec<u8>,
    theirs: Option<Vec<u8>>,
}

impl Golden {
    pub fn new(name: &'static str) -> Golden {
        Golden { name, ours: Vec::new(), theirs: std::env::var_os("BLAKE3_GOLDEN").map(|_| Vec::new()) }
    }

    /// One case: `ours`, and official BLAKE3's output for it, which only a
    /// regenerating run computes.
    pub fn add(&mut self, ours: &[u8], official: impl FnOnce() -> Vec<u8>) {
        if let Some(theirs) = &mut self.theirs {
            let case = official();
            assert!(ours == &case[..], "{}: the case at byte {} differs from official BLAKE3", self.name, theirs.len());
            theirs.extend_from_slice(&case);
        }
        self.ours.extend_from_slice(ours);
    }

    /// The sweep's fold (64-bit FNV-1a, the startup self-test's) against `want`.
    pub fn check(self, want: u64) {
        let got = self.ours.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3));
        if self.theirs.is_some() {
            println!("golden {}: {got:#018x}", self.name);
        }
        assert_eq!(got, want, "{}: the outputs' fold (BLAKE3_GOLDEN=1 regenerates it, a reviewed step)", self.name);
    }
}
