//! The statics D170 C3 and D173 allow, by crate and name: the foreign heap, the profile ring and
//! the signal handler, "each named and capped". The one table of them: `process-static` reads it
//! here (a driver gives each file the rows of its crate, [`statics_of_path`], in its
//! [`crate::Scope`]), and scripts/src/bin/ambient/sansio.rs reads this file's tokens for its
//! `statics` column (each `AllowedStatic { krate: "..", name: "..", .. }`), so a crate holding
//! one of the three can reach zero and a fourth static is refused by both. A row is added only
//! with the change that makes the static one of the three; the script refuses a row whose crate
//! does not declare that name exactly once outside test code.

/// Which of the three a static is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Allowed {
    /// The foreign heap (D173): Rust's `#[global_allocator]`, in the binary crate alone, for the
    /// third-party code compiled into the binary (Wasmtime, the vendored cryptography), which
    /// allocates through Rust's hook. It goes when the last third-party package goes.
    ForeignHeap,
    /// The profile ring below bstd (end-state.md D170: "the trace ring below bstd stays the one
    /// static C3 allows").
    ProfileRing,
    /// The one word a signal handler can reach (a handler takes no argument).
    SignalHandler,
}

/// One allowed static: its crate (its directory under `crates/`), its name as declared (a
/// `static NAME` or a `thread_local!`'s `static NAME`), and which of the three it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AllowedStatic {
    pub krate: &'static str,
    pub name: &'static str,
    pub why: Allowed,
}

/// Every allowed static, sorted by crate then name. Read as tokens by the sansio script: keep
/// each row an `AllowedStatic { krate: "..", name: "..", why: .. }` with string literals.
pub const ALLOWED_STATICS: [AllowedStatic; 5] = [
    AllowedStatic { krate: "blaze-cli", name: "HEAP", why: Allowed::ForeignHeap },
    AllowedStatic { krate: "blaze-core", name: "ASKED", why: Allowed::SignalHandler },
    AllowedStatic { krate: "blaze-core", name: "HELD", why: Allowed::SignalHandler },
    AllowedStatic { krate: "blaze-core", name: "TERMINATE", why: Allowed::SignalHandler },
    AllowedStatic { krate: "blaze-trace", name: "PROFILE", why: Allowed::ProfileRing },
];

// A file's rows are a bit each in the low 15 bits of `Scope::statics` (bit 15 is reserved in
// `Scope::key` for `in_hermetic`).
const _: () = assert!(ALLOWED_STATICS.len() <= 15, "the allowed statics fit Scope::statics");

/// Whether `path` (relative to the workspace's root) belongs to `bstd::procs::hermetic`, the one
/// module allowed to invoke the OS's `sandbox-exec` binary directly (`Rule::RawSandboxExec`).
pub fn is_hermetic_path(path: &str) -> bool {
    let rest = path
        .strip_prefix("crates/blaze-std/src/procs/")
        .or_else(|| path.strip_prefix("crates/bstd/src/procs/"))
        .or_else(|| path.strip_prefix("bstd/src/procs/"));
    matches!(rest, Some("hermetic.rs")) || rest.is_some_and(|r| r.starts_with("hermetic/"))
}

/// The rows of the crate the file at `path` (relative to the workspace's root,
/// `crates/<crate>/..`) is in, as [`crate::Scope::statics`]'s mask; none for a file outside
/// `crates/`.
pub fn statics_of_path(path: &str) -> u16 {
    let Some(rest) = path.strip_prefix("crates/") else { return 0 };
    let mut mask = 0u16;
    for (at, row) in ALLOWED_STATICS.iter().enumerate() {
        let in_crate = rest.strip_prefix(row.krate).is_some_and(|after| after.starts_with('/'));
        if in_crate {
            mask |= 1 << at;
        }
    }
    mask
}

/// Whether a static named `name` is one of the rows `mask` gives.
pub(crate) fn allows(mask: u16, name: &str) -> bool {
    ALLOWED_STATICS.iter().enumerate().any(|(at, row)| mask & (1 << at) != 0 && row.name == name)
}
