//! bstd, blaze's own standard library, step S1 (docs/v2/plan/design/std.md): the interface,
//! over blaze-core's drivers and its simulation.
//!
//! - [`io::Io`] is one concrete handle to the process's driver, the target's
//!   [`blaze_core::Native`] or [`blaze_core::sim::Sim`]. A call turns into a
//!   [`blaze_core::Effect`] and waits for its [`blaze_core::Completion`] under the task's
//!   deadline.
//! - [`cx::Cx`] is the task's context, the first parameter of every call that can wait: its
//!   deadline (which only narrows), its cancellation, its span and its frame.
//! - The capabilities ([`fs::Fs`], [`fs::Dir`], [`fs::File`], [`clock::Clock`], [`rng::Rng`],
//!   [`env::Env`], [`args::Args`], [`host::Host`], [`mem::Mem`], [`net::Net`]) have private
//!   fields and are made only from an `Io` or by narrowing one, which never widens.
//! - Tasks are started by a context: [`cx::Cx::scope`] makes a [`task::Scope`] value and
//!   [`task::Scope::spawn`] a [`task::Task`] ([`task`]), in the `Inline` layout.
//! - What must be finished is linear (`#[blaze::linear]`, crates/hir-ty-linear): a
//!   [`task::Scope`] and its [`task::Task`]s are joined, an op in flight
//!   ([`pending::Pending`]) is waited for or cancelled, a writable [`fs::File`] is closed and a
//!   [`net::Conn`] or [`net::Listener`] is finished. Leaving one is a compile error; a panic
//!   that unwinds past one runs its abandonment path, which each type's doc names.
//! - A capability the host does not offer is `None` at the root ([`io::Io::fs`], ...;
//!   [`io::Offers`]), with a `need_` form whose [`io::Missing`] names it.
//! - Nothing is global: the crate holds no `static` with interior mutability, no `static mut`
//!   and no thread-local. Spans and events go through [`cx::Cx`] into the recorder its world
//!   owns ([`trace::Trace`]).
//! - [`path::Path`] is relative and cannot hold a root or `..`. Names meet authority typed:
//!   a [`Path`], an [`env::VarName`], a [`net::HostPort`], each checked as it is made, and a
//!   literal checked as the crate compiles ([`path!`], [`var!`], [`host!`]).
//! - Every wait ends in a typed outcome: what it waited for, or a [`Stopped`].
//!
//! What S1 does not have yet, and why, is in the crate's report and in std.md: the calling
//! fiber does not park on a scheduler (the tree has no fiber park yet); a call waits by
//! polling the driver inline behind one seam, [`io::Io`]'s `wait`, which a fiber park replaces.
//! Standard input, output and error (`Out`), child processes (`Process`), the process's own
//! signals, the host's name, wall time, randomness from the system, name lookup, directories
//! made empty and opens resolved beneath a directory by the kernel each need an effect
//! blaze-core does not have.
//!
//! # Batteries
//!
//! bstd is also the facade over blaze's piece crates (std.md, "What is in it"): each module
//! below re-exports one piece's public items under bstd's names, so code imports `bstd::..`
//! only. Nothing is copied, merged or moved; where two crates offer one concept, the module
//! says which one it took. Left out on purpose: what reaches state through the thread or the
//! process (blaze-mem's `frame::current`, blaze-radix's `set_ambient` and its ambient sorts,
//! blaze-http's `roots` and `proxy_env`, which read the machine), and `chan` (the crossbeam
//! queue and channel are not yet dependencies of this crate).
//!
//! | module | what | from |
//! |---|---|---|
//! | [`mem`] | allocators by lifetime | blaze-mem |
//! | [`bytes`], [`text`] | held bytes and UTF-8 text, lent and never copied | blaze-bytes |
//! | [`map`], [`set`], [`hash`] | the one hash map and set, the identity and content hashers | blaze-hash |
//! | [`id`] | content ids, dense indices, the interner | blaze-ids |
//! | [`sync`], [`cell`] | `Shared`, named locks, `Atomic`, token cells, the spin, reclamation | std, blaze-core, blaze-spin, blaze-reclaim |
//! | [`sort`], [`phf`] | the radix sort; perfect hashes of fixed name sets | blaze-radix, blaze-phf |
//! | [`digest`] | BLAKE3 and SHA-256 | blaze-ids' `b3`, blaze-sha256 |
//! | [`rng`] | the seeded generator | blaze-rng |
//! | [`record`] | zero-copy records | blaze-format |
//! | [`report`] | what a program says, as data | blaze-report |
//! | [`quic`], [`http`] | sans-IO QUIC (WebTransport over HTTP/3) and HTTP/1.1 | blaze-http |
//! | [`test`] | the simulation, the model checker, allocation counting | blaze-core, blaze-check, blaze-alloc-count |
//!
//! Until S4, `alloc`'s global allocator is still underneath: the few global allocations here
//! are the effects' own buffers (blaze-core's `Effect` owns its bytes) and errors' messages,
//! each with its `// alloc:` line. What bstd hands back to a caller is made in the caller's
//! allocator ([`mem::Bytes`]).

#![deny(unsafe_code)]
#![feature(allocator_api, generic_atomic, register_tool, custom_inner_attributes)]
#![register_tool(blaze)]

pub mod args;
#[expect(
    unsafe_code,
    reason = "a token cell's two views of its value and its `Sync`, each with its SAFETY"
)]
pub mod cell;
pub mod clock;
pub mod cx;
pub mod env;
pub mod fs;
pub mod host;
pub mod io;
pub mod mem;
pub mod net;
pub mod path;
pub mod pending;
pub mod quic;
pub mod rng;
pub mod sync;
pub mod task;
pub mod trace;

/// Bytes held without a copy: their own `Arc`, a range their owner keeps in place, or a
/// `'static` slice (blaze-bytes). Not [`mem::Bytes`], which are bytes being written into an
/// allocator the caller handed in.
pub mod bytes {
    pub use blaze_bytes::{Bytes, le};
}

/// UTF-8 text held without a copy: [`bytes::Bytes`] checked once (blaze-bytes). Owned text in
/// an allocator is [`mem::String`].
pub mod text {
    pub use blaze_bytes::{Text, text_of};
}

/// The one hash map: hashbrown's, on foldhash with one fixed seed, in the allocator its type
/// names; shared between threads, papaya's lock-free map (blaze-hash).
pub mod map {
    pub use blaze_hash::{
        ByHash, Equivalent, HashMap as Map, Operation, SharedByHash, SharedMap, hash_map::Entry,
    };
}

/// The one hash set, as [`map`] (blaze-hash).
pub mod set {
    pub use blaze_hash::{ByHashSet, HashSet as Set, SharedSet};
}

/// The hashers behind [`map`] and [`set`]: the fixed seed, the identity hasher for keys that are
/// hashes already, SipHash under a key for keys an attacker picks, and a value's `Hash` fed to
/// BLAKE3 for what is persisted (blaze-hash). `LazyKey`, a key drawn for a table in a `static`,
/// is left out: bstd has no statics with interior mutability.
pub mod hash {
    pub use blaze_hash::{
        Blake3Hasher, BuildKeyHasher, FixedHasher, FixedState, KeyHasher, SipHasher, SipKey,
    };
}

/// Identity: a [`id::ContentId`] is the BLAKE3 of what a thing is under its domain, an
/// [`id::Idx`] a dense `u32` index into one arena, and [`id::Sharded`] the interner between them
/// (blaze-ids). The interner is blaze-ids'; `intern` and `blaze-hashcons` are not merged into it
/// yet, and are not re-exported.
pub mod id {
    pub use blaze_ids::{
        ContentId, Domain, Fingerprint, Hasher as ContentHasher, Idx, Interner, Sharded, index_key,
    };
}

/// The stable radix sort (blaze-radix), on workers named at the call ([`sort::on`], with
/// [`sort::Serial`] for the calling thread alone). The free functions that sort on the
/// thread's ambient workers are left out.
pub mod sort {
    pub use blaze_radix::{
        Csr, Key, PARALLEL, Radix, SMALL, Serial, WIDE, Workers, on, pack_32, pack_64, signed_32,
        signed_64,
    };
}

/// Perfect hashes of fixed name sets, made as the crate compiles (blaze-phf).
pub mod phf {
    pub use blaze_phf::{
        NAME_K, Phf, buckets, bytes_eq, hash, name_hash, name_hash_finish, name_hash_start,
        name_hash_word, slots,
    };
}

/// Digests: BLAKE3 (blaze-ids' `b3`, the one copy) and SHA-256 (blaze-sha256).
pub mod digest {
    pub use blaze_ids::b3::{Hasher as Blake3, OUT_LEN as BLAKE3_LEN, hash as blake3};
    pub use blaze_sha256::{Sha256, digest as sha256};
}

/// Zero-copy records: the bytes in memory are the bytes on disk (blaze-format).
pub mod record {
    pub use blaze_format::{
        ALIGN, Aligned, FormatError, HEADER_LEN, Header, MAGIC, Part, Persist, Placed, Record,
        Rel, SCHEMA_DOMAIN, SchemaHash, Slice, Str, Sym, Writer, buffer_id, bytes_of_slice,
        header, open, open_as, open_verified, read_verified, record_at, schema, schema_of,
        to_bytes,
    };
}

/// The `Report`: what a program says to a person or an agent, as data, and its renderings
/// (blaze-report). Its `rules` (blaze's own rule texts) and `script` (build scripts' reports)
/// are about building Rust and stay in blaze-report.
pub mod report {
    pub use blaze_report::{
        Blocks, Caps, Fact, Fix, NAMED, Report, Roots, Severity, Span, Text, json, json_message,
        render, render_into, terminal,
    };
}

/// HTTP/1.1 as a sans-IO core: blaze-http's [`http::Client`], bytes and completions in, IO to
/// do, events and the next deadline out, with its request writer and head parser ([`http::h1`])
/// and proxies. Its tokio driver is behind blaze-http's `tokio` feature, which bstd does not
/// turn on; `roots` and `proxy_env` read the machine and are left out. HTTP/3 and WebTransport
/// are [`quic`].
pub mod http {
    pub use blaze_http::{
        Client, Completion, Config, ConnId, Error, Event, IDLE_FOR, IDLE_MOST, Io, Kind, Proxies,
        Proxy, ProxyError, ProxyKind, REDIRECTS, Request, RequestId, Step, WINDOW, h1,
    };
}

/// Testing: blaze-core's seeded simulation ([`test::Sim`]: a virtual clock, shuffled
/// completions, failures, crashes), the model checker for lock-free code ([`test::model`], its
/// shims in [`test::checked`]), and allocation counting ([`test::Counting`], [`test::measure`]).
/// Hegel stays a direct dev-dependency: its `#[hegel::test]` expands to `::hegel::` paths,
/// which a re-export cannot satisfy.
pub mod test {
    pub use blaze_alloc_count::{Counting, Used, assert_at_most, installed, measure};
    pub use blaze_check::{Builder, MAX_THREADS, model};
    pub use blaze_core::sim::{Faults, Journal, KILLED, Program, Ran, Sim};

    /// The model checker's shims, with std's API: a crate's code switches to them under
    /// `cfg(blaze_check)`.
    pub mod checked {
        pub use blaze_check::{cell, hint, linear, sync, thread};
    }
}

#[cfg(test)]
mod tests;

use std::{fmt, time::Duration};

pub use crate::{
    clock::Clock,
    cx::Cx,
    env::VarName,
    io::{Io, Missing, Offers},
    net::HostPort,
    path::Path,
    pending::Pending,
    task::{Scope, Task},
};
pub use blaze_core::wait::Deadline;

/// Why a wait or an allocation stopped short of what it was for. Every blocking call ends in
/// what it waited for or in one of these (rules bounded-progress, deadline-waits).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Stopped {
    /// The task's deadline passed first, at `at` on the driver's clock.
    Deadline { span: &'static str, at: Duration },
    /// The task's scope was cancelled.
    Cancelled { span: &'static str },
    /// `arena` could not hand out `asked` more bytes within its limit.
    OutOfMemory { arena: &'static str, asked: usize },
    /// The driver said nothing was ready this many times in a row before the deadline with
    /// its clock not past it: a driver that does not keep its own deadline.
    Stalled { span: &'static str, polls: u32 },
}

impl fmt::Display for Stopped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Stopped::Deadline { span, at } => {
                write!(f, "{span}: its deadline ({at:?} on the driver's clock) passed first")
            }
            Stopped::Cancelled { span } => write!(f, "{span}: its scope was cancelled"),
            Stopped::OutOfMemory { arena, asked } => {
                write!(f, "arena {arena}: {asked} more bytes would pass its limit")
            }
            Stopped::Stalled { span, polls } => write!(
                f,
                "{span}: the driver said nothing was ready {polls} times in a row before the \
                 deadline (rule bounded-progress)"
            ),
        }
    }
}

impl std::error::Error for Stopped {}

/// Consumes a linear value that has no fields left to move out (a fieldless marker such as
/// [`fs::ReadWrite`]): the one way such a value ends, besides its type's own consuming calls.
#[blaze::consumes]
pub(crate) fn consume<#[blaze::linear] T>(_: T) {}

/// A safeguard a test turns off to prove its oracle sees the bug (skill `testing`, planted-bug
/// controls). Always [`Plant::None`] outside this crate's tests: only their constructor
/// (`Io::planted`) sets another.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[cfg_attr(not(test), expect(dead_code, reason = "the planted bugs are made by the tests only"))]
pub(crate) enum Plant {
    None,
    /// `Dir::subdir` keeps its parent's base: a narrowed directory reaches its parent.
    SubdirKeepsParent,
    /// `Dir::open` of a missing file makes it, through a read-only directory too.
    OpenCreates,
    /// A path's `..` is let in and resolved by text after the join, as `a/../../x`.
    LexicalParent,
    /// A wait polls with no deadline, so it ends when its effect does.
    WaitIgnoresDeadline,
    /// `Cx::until` takes the deadline it is given, later than its parent's or not.
    UntilWidens,
    /// A wait does not look at its task's cancellation.
    WaitIgnoresCancel,
    /// An arena is made with no limit of its own.
    ArenaUnlimited,
    /// `File::sync` says it synced without asking the driver.
    SyncSkipped,
    /// `Env::only` replaces what is allowed rather than narrowing it.
    EnvOnlyReplaces,
    /// `Pending::cancel` returns without waiting for its op to end.
    CancelSkipsWait,
    /// `File::close` says it closed without syncing.
    CloseSkipsSync,
    /// `Scope::join` returns `Ok` though a task failed.
    JoinDropsFailure,
    /// Every root accessor of `Io` hands its capability out, offered or not.
    OffersIgnored,
}
