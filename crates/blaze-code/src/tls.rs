//! Thread-locals by index: the descriptor, row and table forms byte for byte (port27/tempo-run
//! THREADS.md, D2), in one home for the three that meet them: blaze-build's getter and ender
//! (`tls::index`, which re-exports these), blaze-emit's access sequence (`pass/tls_index.rs`)
//! and the launcher's run mode, which makes the descriptor and writes each row
//! (blaze-launch-rt `run_mode.rs`, "Thread-locals").
//!
//! - One run-wide DESCRIPTOR ([`DESC_BYTES`], 8-aligned): `key` (the pthread key), `root`
//!   (where a thread's table pointer lives, from the thread register; [`ROOT_OWN`] set = the
//!   Linux form, where the getter stores the root itself), `count` (rows written), `cap` (rows
//!   reserved, at most [`CAP`]), `vars` (the first row). Written by the launcher only; the
//!   getter and the access sequence read it.
//! - A `VarRow` ([`var_row`]): `size`, `align` (a power of two), `init`, `init_len` (at most
//!   `size`). Written once by the control thread before the first record naming its index is
//!   published.
//! - A thread's TABLE: `len`, `rounds`, `desc`, `root_at`, then `len` pointers from
//!   [`TABLE_SLOTS`], null until touched.

/// The descriptor's fields: byte offsets, and its size.
pub const DESC_KEY: i32 = 0;
pub const DESC_ROOT: i32 = 8;
pub const DESC_COUNT: i32 = 16;
pub const DESC_CAP: i32 = 24;
pub const DESC_VARS: i32 = 32;
pub const DESC_BYTES: usize = 40;

/// Bit 63 of the descriptor's `root`: the Linux form, where the root is the launcher's own
/// thread-local and the getter stores the table's address there itself.
pub const ROOT_OWN: u64 = 1 << 63;

/// A `VarRow`'s fields: byte offsets, and its size.
pub const ROW_SIZE: i32 = 0;
pub const ROW_ALIGN: i32 = 8;
pub const ROW_INIT: i32 = 16;
pub const ROW_INIT_LEN: i32 = 24;
pub const ROW_BYTES: usize = 32;

/// A thread's table header: byte offsets; the slots start at [`TABLE_SLOTS`].
pub const TABLE_LEN: i32 = 0;
pub const TABLE_ROUNDS: i32 = 8;
pub const TABLE_DESC: i32 = 16;
pub const TABLE_ROOT_AT: i32 = 24;
pub const TABLE_SLOTS: i32 = 32;

/// The rows reserved at start: a thread-local past it is a typed refusal naming it.
pub const CAP: u64 = 65_536;

/// One `VarRow`, little-endian: `size`, `align`, `init`, `init_len`.
pub fn var_row(size: u64, align: u64, init: u64, init_len: u64) -> [u8; ROW_BYTES] {
    let mut row = [0u8; ROW_BYTES];
    let fields = [(ROW_SIZE, size), (ROW_ALIGN, align), (ROW_INIT, init), (ROW_INIT_LEN, init_len)];
    for (at, value) in fields {
        let at = at as usize;
        row[at..at + 8].copy_from_slice(&value.to_le_bytes());
    }
    row
}

/// The descriptor's bytes, little-endian: `key`, `root`, `count`, `cap`, `vars`.
pub fn descriptor(key: u64, root: u64, count: u64, cap: u64, vars: u64) -> [u8; DESC_BYTES] {
    let mut desc = [0u8; DESC_BYTES];
    let fields = [(DESC_KEY, key), (DESC_ROOT, root), (DESC_COUNT, count), (DESC_CAP, cap), (DESC_VARS, vars)];
    for (at, value) in fields {
        let at = at as usize;
        desc[at..at + 8].copy_from_slice(&value.to_le_bytes());
    }
    desc
}
