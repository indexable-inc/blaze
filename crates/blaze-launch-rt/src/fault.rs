//! The fault handler's body: what to say when a thread faults on an address the launcher
//! owns, and then stop.
//!
//! A thread that faults in the code area has almost always run into a *retired* body: code the
//! control thread froze and gave back to the system because it believed no thread could be in
//! it any more (`blaze_launch::launch`, promise 4). That belief is the launcher's hardest
//! invariant, so a fault there is a blaze bug, and the report names exactly which function and
//! version the address was, so the bug can be found. [`crate::retired::which`] answers that
//! from the table the driver appends a row to as each body is decommitted (`drive`).
//!
//! The handler runs in signal context, so it is async-signal-safe: it touches no lock and no
//! allocator, formats into a stack buffer ([`crate::retired::REPORT_MAX`]) with only byte
//! copies, writes it with one `write(2)`, and ends with `_exit`. `sys.rs` installs it (a
//! `SIGSEGV`/`SIGBUS` handler on a dedicated alternate stack) and calls [`fault`] with the
//! faulting address and the retired table; see this crate's report for where.

use crate::retired::{self, Table};

/// Handles a fault at `address` against `table`: if the address is retired code, writes the
/// stop report to stderr and ends the process; otherwise returns, so the default disposition
/// (a crash the OS reports) stands for a fault the launcher does not own.
///
/// # Safety
///
/// Called from a signal handler: `table` must be a `Table` that lives as long as the process
/// (the launcher leaks one), reachable without a lock. Nothing here allocates, locks or
/// formats through `core::fmt`.
pub unsafe fn fault<const N: usize>(table: &Table<N>, address: usize) {
    let Some(retired) = table.which(address) else {
        // Not the launcher's: let the default handler take it.
        return;
    };
    let mut buffer = [0u8; retired::REPORT_MAX];
    // `report` never needs more than `REPORT_MAX`, but on the impossible overflow we still
    // write what fits rather than nothing.
    let wrote = match retired::report(&retired, address, &mut buffer) {
        Ok(wrote) => wrote,
        Err(_) => buffer.len(),
    };
    // Both operations are async-signal-safe, even if the faulting process holds a lock.
    let _ = blaze_sys::posix::io::write(blaze_sys::Fd::STDERR, &buffer[..wrote]);
    blaze_sys::posix::signal::exit_now(134) // 128 + SIGABRT, as a crash would
}
