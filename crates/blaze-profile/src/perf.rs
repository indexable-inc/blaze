//! Performance history of the programs blaze builds (docs/v2/plan/design/perf-history.md): each
//! profiled run is one row set in the history, its functions named by path and by code
//! identity, so `blaze perf diff` lines up two builds' functions, bounds the noise, and says
//! why each changed.
//!
//! - [`map`]: the code map a link writes beside its image (`<target-dir>/perf/maps`): per
//!   function its address, identity, code, key and what it refers to, and the Merkle `deep`
//!   hash over that graph.
//! - [`record`]: `.ixprof` repetitions of a run, read through the code maps of the builds they
//!   ran, as rows; every input missing is a named gap.
//! - [`rows`]: the `perf_runs` and `perf_functions` rows, written to and read from the
//!   history's directory as Parquet.
//! - [`diff`]: two runs joined by identity, each function's change with its noise bound and
//!   its cause class, as text or JSON.

pub mod diff;
pub mod map;
pub mod record;
pub mod rows;

#[cfg(test)]
mod tests;

/// A 32-byte content identity, as the code map and the rows hold it.
pub type Id = [u8; 32];

/// `id` as 64 lowercase hex digits.
pub fn hex(id: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    // alloc: a hex string, once a row
    let mut out = String::with_capacity(id.len() * 2);
    for byte in id {
        out.push(DIGITS[usize::from(byte >> 4)] as char);
        out.push(DIGITS[usize::from(byte & 15)] as char);
    }
    out
}
