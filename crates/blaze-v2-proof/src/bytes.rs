//! Output and trace comparison for the oracle: where two files first differ, mapped and
//! compared in place (no copy into a buffer, no read per megabyte), across cores from
//! [`ACROSS_CORES`] bytes on.
//!
//! A slice is compared by [`blaze_store_check::bytes`], the one byte comparison blaze's checks share
//! (the store check names a recomputed value's first differing byte with it too); it is
//! re-exported here, and this module adds only what reads files and spreads one comparison over
//! every core.
//!
//! Unsafe is the map of [`Mapped`].

#![allow(unsafe_code)]

use std::{io, path::Path};

pub use blaze_store_check::bytes::{
    Simd, available, fastest, first_difference, first_difference_with,
};

/// Inputs at least this long are compared across cores: below it one core streams them in
/// under 3 ms, about what starting the threads and faulting the pages in from each costs.
pub const ACROSS_CORES: usize = 64 << 20;

/// What a piece compares between looks at whether an earlier piece has found a difference.
const BLOCK: usize = 4 << 20;

/// [`first_difference`] with the bytes cut into one piece per core, each compared by
/// [`fastest`] on a thread of its own: memory-bound on every core at once rather than one. A
/// piece stops once an earlier one has found a difference; the earliest piece with one never
/// stops, so the answer is the same offset as on one core. The pieces are consecutive, so the
/// least offset any piece finds is the answer: one atomic minimum, nothing collected.
pub fn first_difference_across_cores(a: &[u8], b: &[u8]) -> Option<usize> {
    first_difference_in(fastest(), a, b, crate::join::cores(), BLOCK)
}

fn first_difference_in(
    path: Simd,
    a: &[u8],
    b: &[u8],
    pieces: usize,
    block: usize,
) -> Option<usize> {
    use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
    let n = a.len().min(b.len());
    let pieces = pieces.clamp(1, n.div_ceil(block).max(1));
    if pieces == 1 {
        return first_difference_with(path, a, b);
    }
    let longer = a.len() != b.len();
    let first = AtomicUsize::new(usize::MAX);
    let piece = |p: usize| {
        let (start, end) = (p * n / pieces, (p + 1) * n / pieces);
        let mut at = start;
        while at < end && first.load(Relaxed) >= start {
            let to = (at + block).min(end);
            if let Some(found) = first_difference_with(path, &a[at..to], &b[at..to]) {
                first.fetch_min(at + found, Relaxed);
                return;
            }
            at = to;
        }
    };
    std::thread::scope(|scope| {
        let piece = &piece;
        for p in 1..pieces {
            scope.spawn(move || piece(p));
        }
        piece(0);
    });
    let first = first.into_inner();
    (first != usize::MAX).then_some(first).or(longer.then_some(n))
}

/// A file's bytes, mapped read-only; an empty file maps nothing.
///
/// The file must not change while mapped: what is mapped here are the outputs and traces of
/// builds that have finished, which nothing writes while they are compared.
pub struct Mapped(blaze_core::fs::Mapped);

impl Mapped {
    /// Mapped and faulted in at once by the kernel (`MAP_POPULATE` on Linux), for bytes one
    /// thread reads.
    pub fn open(path: &Path) -> io::Result<Mapped> {
        Mapped::map(path, true)
    }

    /// Mapped with no page faulted in, for bytes many threads read: each faults in the pages of
    /// its own piece, so the faults run on every core rather than serially in `mmap`.
    pub fn open_lazily(path: &Path) -> io::Result<Mapped> {
        Mapped::map(path, false)
    }

    fn map(path: &Path, populate: bool) -> io::Result<Mapped> {
        let options = blaze_core::fs::MapOptions::new().populate(populate);
        // SAFETY: a read-only map of a file nothing writes while it is mapped (the type's
        // contract above).
        Ok(Mapped(unsafe { options.open(path) }?))
    }

    pub fn bytes(&self) -> &[u8] {
        self.0.bytes()
    }
}

/// Where two files first differ, or where the shorter ends; `None` when their bytes are the
/// same. Both are mapped and compared in place by [`fastest`], across cores from
/// [`ACROSS_CORES`] bytes on.
pub fn first_file_difference(a: &Path, b: &Path) -> io::Result<Option<u64>> {
    let long = |path: &Path| std::fs::metadata(path).map(|m| m.len() >= ACROSS_CORES as u64);
    if long(a)? && long(b)? {
        let (a, b) = (Mapped::open_lazily(a)?, Mapped::open_lazily(b)?);
        return Ok(first_difference_across_cores(a.bytes(), b.bytes()).map(|at| at as u64));
    }
    let (a, b) = (Mapped::open(a)?, Mapped::open(b)?);
    Ok(first_difference(a.bytes(), b.bytes()).map(|at| at as u64))
}

#[cfg(test)]
mod tests {
    use super::{Simd, first_difference_in, first_difference_with};
    use crate::Rng;

    #[test]
    fn pieces_across_cores_find_the_first_difference_one_core_does() {
        let mut rng = Rng::new(4242);
        const BLOCK: usize = 4096;
        let len = 3 * BLOCK + 1_234;
        let a: Vec<u8> = (0..len).map(|_| rng.below(256) as u8).collect();
        let mut wants = vec![None, Some(0), Some(len - 1), Some(BLOCK), Some(BLOCK - 1)];
        wants.extend((0..12).map(|_| Some(rng.below(len as u64) as usize)));
        for want in wants {
            let mut b = a.clone();
            if let Some(at) = want {
                b[at] ^= 1;
                // A later difference in every later piece: the earliest still wins.
                for later in (at + 1..len).step_by(BLOCK / 3) {
                    b[later] ^= 0x80;
                }
            }
            let serial = first_difference_with(Simd::Scalar, &a, &b);
            assert_eq!(serial, want);
            for pieces in [1, 2, 3, 4, 5, 8, 64] {
                assert_eq!(
                    first_difference_in(Simd::Scalar, &a, &b, pieces, BLOCK),
                    want,
                    "{pieces} pieces, {want:?}"
                );
                assert_eq!(
                    first_difference_in(super::fastest(), &a, &b, pieces, BLOCK),
                    want,
                    "{pieces} pieces, {want:?}"
                );
            }
        }
        let mut longer = a.clone();
        longer.push(7);
        for pieces in [1, 3, 8] {
            assert_eq!(first_difference_in(Simd::Scalar, &a, &longer, pieces, BLOCK), Some(len));
            assert_eq!(first_difference_in(Simd::Scalar, &longer, &a, pieces, BLOCK), Some(len));
        }
    }

    /// The first difference on one core, by every path, reads the two slices in place and
    /// allocates nothing once the path is detected.
    #[test]
    fn a_first_difference_allocates_nothing() {
        let mut rng = Rng::new(31);
        let len = 3 * 4096 + 77;
        let a: Vec<u8> = (0..len).map(|_| rng.below(256) as u8).collect();
        let mut b = a.clone();
        b[len - 3] ^= 1;
        let fastest = super::fastest();
        for path in super::available() {
            let found = blaze_alloc_count::assert_at_most(0, "a first difference", || {
                first_difference_with(path, &a, &b)
            });
            assert_eq!(found, Some(len - 3), "{path:?}");
            let short =
                blaze_alloc_count::assert_at_most(0, "a first difference of a short input", || {
                    first_difference_with(path, &a[..9], &b[..10])
                });
            assert_eq!(short, Some(9), "{path:?}");
        }
        let same = blaze_alloc_count::assert_at_most(0, "equal bytes by the fastest path", || {
            super::first_difference(&a, &a)
        });
        assert_eq!(same, None);
        let one = blaze_alloc_count::assert_at_most(0, "one piece, on this thread", || {
            first_difference_in(fastest, &a, &b, 1, 4096)
        });
        assert_eq!(one, Some(len - 3));
    }
}
