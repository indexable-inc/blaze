//! Parallel execution and sub-phase timing strategy for `blaze-export` (Rule 78:
//! strategy-is-a-type-parameter).
//!
//! Keeps `blaze-export` strictly `#![no_std]`, `#![blaze::sans_io]`, and `#![forbid(unsafe_code)]`
//! while allowing callers (`blaze-cli`) to fan out fold hashing, text copy + ARM64 relocation
//! patching, segment writes, and Mach-O ad-hoc SHA-256 page signing across all host cores with
//! bit-for-bit deterministic output (Rule 25).

/// Fine-grained sub-phase wall-clock durations (in nanoseconds) recorded during one
/// [`crate::export_par`] call when the [`ExportRunner`] provides a monotonic clock.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct ExportLinkTimes {
    pub eh_room_ns: u64,
    pub order_ns: u64,
    pub fold_ns: u64,
    pub reloc_ns: u64,
    pub fixups_ns: u64,
    pub eh_sections_ns: u64,
    pub write_ns: u64,
    pub uuid_ns: u64,
    pub sign_ns: u64,
}

/// Strategy trait for parallel slice partitioning and optional monotonic timing inside
/// `blaze-export`.
pub trait ExportRunner {
    /// Applies `f(chunk_start_index, chunk)` across contiguous disjoint chunks covering `out`
    /// exactly once. `min_chunk` is a granularity floor (`>= 1`).
    fn for_each_chunk_mut<T: Send, F>(&self, out: &mut [T], min_chunk: usize, f: F)
    where
        F: Fn(usize, &mut [T]) + Sync;

    /// Splits `buf` at monotonically non-decreasing offsets `splits` (`splits.len() >= 1`,
    /// `splits[0] == 0`, `*splits.last() == buf.len()`) and applies `f(part_idx, part_slice)` to
    /// each of the `splits.len() - 1` disjoint slices, storing the result in `results[part_idx]`.
    fn map_splits_mut<T: Send, R: Send, F>(
        &self,
        buf: &mut [T],
        splits: &[usize],
        results: &mut [Option<R>],
        f: F,
    ) where
        F: Fn(usize, &mut [T]) -> R + Sync;

    /// Returns a monotonic timestamp in nanoseconds, or `0` when timing is disabled.
    fn now_ns(&self) -> u64 {
        0
    }
}

/// Sequential [`ExportRunner`] running every chunk inline on the caller's thread with zero
/// overhead.
#[derive(Clone, Copy, Default, Debug)]
pub struct SequentialExportRunner;

impl ExportRunner for SequentialExportRunner {
    fn for_each_chunk_mut<T: Send, F>(&self, out: &mut [T], _min_chunk: usize, f: F)
    where
        F: Fn(usize, &mut [T]) + Sync,
    {
        if !out.is_empty() {
            f(0, out);
        }
    }

    fn map_splits_mut<T: Send, R: Send, F>(
        &self,
        buf: &mut [T],
        splits: &[usize],
        results: &mut [Option<R>],
        f: F,
    ) where
        F: Fn(usize, &mut [T]) -> R + Sync,
    {
        let mut rest = buf;
        let mut prev = 0usize;
        for (part_idx, &end) in splits.iter().skip(1).enumerate() {
            let len = end.saturating_sub(prev);
            let (head, tail) = rest.split_at_mut(len);
            let val = f(part_idx, head);
            if let Some(slot) = results.get_mut(part_idx) {
                *slot = Some(val);
            }
            rest = tail;
            prev = end;
        }
    }
}
