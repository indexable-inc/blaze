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
//!   ([`pending::Pending`]) is waited for or cancelled, a writable [`fs::File`] is closed, a
//!   buffered writer ([`fs::Buffered`]) is finished and a [`net::Conn`] or [`net::Listener`]
//!   is finished. Leaving one is a compile error when blaze builds the crate; under rustc,
//!   which ignores the mark, it is a `#[must_use]` warning and runs the abandonment path, as a
//!   panic that unwinds past one does: each type's doc names it (std.md, "Two builders").
//! - A capability the host does not offer is `None` at the root ([`io::Io::fs`], ...;
//!   [`io::Offers`]), with a `need_` form whose [`io::Missing`] names it.
//! - Nothing is global: the crate holds no `static` with interior mutability, no `static mut`
//!   and no thread-local. Spans and events go through [`cx::Cx`] into the recorder its world
//!   owns ([`trace::Trace`]).
//! - [`path::Path`] is relative and cannot hold a root or `..`. Names meet authority typed:
//!   a [`Path`], an [`env::VarName`], a [`net::HostPort`], each checked as it is made, and a
//!   literal checked as the crate compiles ([`path!`], [`var!`], [`host!`]).
//! - Every wait ends in a typed outcome: what it waited for, or a [`Stopped`].
//! - A cost or a guarantee the caller should decide is an argument or a type, never a default
//!   (std.md, "The second rule"): how durable a file change is ([`fs::Durable`], and
//!   [`fs::Written`] against [`fs::Flushed`]), how big a task's frame is, how a request
//!   talks on the wire. The simulated disk counts what was paid ([`io::Io::disk`]).
//! - The rest of the capability set (std.md, "The rest of the set", "Sound, and the device
//!   capabilities") exists as types with their narrowing: [`term::Term`], [`keys::Keys`],
//!   [`roots::Roots`], [`locale::Locale`], [`user::User`], [`procs::Procs`], [`code::Code`],
//!   [`window::Window`], [`automation::Automation`], [`opener::Opener`], [`power::Power`],
//!   [`audio::AudioOut`] and [`audio::AudioIn`], and the devices of [`device`]. blaze-core has
//!   no effect for any of them yet, so only the simulation provides them
//!   ([`hosted::SimHost`]) and a native `Io` withholds them.
//! - A lower capability makes the higher one (std.md, "Capabilities are layered"): a `Net`
//!   gives a [`resolver::Resolver`] and [`requests::Http`] ([`requests::Http::over`]), a
//!   [`fs::Dir`] gives [`kv::Kv`], [`blobs::Blobs`] and [`local::Local`]. Each of those has a
//!   real provider. [`streams::Streams`] has a handler and the simulation only.
//! - Budgets ride the same way ([`budget`]): [`net::Net::limit`] is a byte limit and
//!   [`fs::Dir::quota`] a quota, each counted by bstd and ending in a typed outcome.
//! - How a pure function's value is got is a value too: [`eval::Eval`] runs a memo point
//!   (`#[blaze::memo]`), remembers it or hands it to a handler ([`eval`]).
//! - Media as pure cores: [`image`] (PNG decode and encode, functions of bytes, an allocator
//!   and limits) and [`video`] (an MP4 muxer, and an HEVC [`video::Encoder`] whose one provider
//!   is VideoToolbox on macOS, made from a [`gpu::Gpu`], a placeholder had only from
//!   [`io::Io::gpu`]).
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
//! blaze-http's `roots` and `proxy_env`, which read the machine). The channel is bstd's own,
//! bounded only ([`sync::Chan`]), not a re-export.
//!
//! | module | what | from |
//! |---|---|---|
//! | [`mem`] | allocators by lifetime, and the handle a function takes ([`mem::Alloc`]) | blaze-mem, blaze-own |
//! | [`vec`], [`boxed`], [`rows`], [`soa`] | the vectors (growable, inline, segmented, packed), one value in a block, rows in two arrays, a struct of arrays | blaze-own, blaze-kit |
//! | [`bytes`], [`text`] | held bytes and UTF-8 text, lent and never copied; many strings in one buffer | blaze-bytes, blaze-own |
//! | [`map`], [`set`], [`hash`] | one map and set per need (hashed, by hash, by content, insertion-ordered, flat sorted, B-tree, keyed, shared), and their hashers | blaze-hash, blaze-hashcons, blaze-own |
//! | [`id`] | content ids, dense indices, the interner; typed ids and the tables they index | blaze-ids, blaze-kit |
//! | [`bits`], [`paths`] | the bit set and the sparse set of ids; paths as a table | blaze-kit, blaze-own |
//!
//! Each form of [`vec`], [`boxed`], [`rows`], [`soa`], [`text`], [`map`], [`set`], [`id`],
//! [`bits`], [`paths`] and [`mem`] is in a table in its module: what to reach for it instead of,
//! and its cost (D120: one form per need, its cost said, its allocator named).
//! | [`sync`], [`cell`] | `Shared`, named and ranked locks, `Swap`, `Frozen`, `Flight`, the bounded `Chan`, named atomics, one-thread cells, token cells, the spin, reclamation | blaze-core, blaze-own, blaze-spin, blaze-reclaim, new |
//! | [`par`] | `for_each`, `map_into`, `reduce` over disjoint chunks, deterministic in any schedule | new, over [`task::Scope`] |
//! | [`own`] | ownership properties derived by reflection (`Acyclic`), `Shared` and `Shares` | new |
//! | [`sort`], [`phf`] | the radix sort; perfect hashes of fixed name sets | blaze-radix, blaze-phf |
//! | [`digest`] | BLAKE3 and SHA-256 | blaze-ids' `b3`, blaze-sha256 |
//! | [`rng`] | the seeded generator | blaze-rng |
//! | [`record`] | zero-copy records | blaze-format |
//! | [`report`] | what a program says, as data | blaze-report |
//! | [`quic`], [`http`] | sans-IO QUIC (WebTransport over HTTP/3) and HTTP/1.1 | blaze-http |
//! | [`test`] | `#[bstd::test]` and its runner, the simulation, the model checker, allocation counting | new, blaze-core, blaze-check, blaze-alloc-count |
//!
//! bstd is `#![no_std]`; `alloc`'s global allocator is still underneath until S4's runtime: the few global allocations here
//! are the effects' own buffers (blaze-core's `Effect` owns its bytes) and errors' messages,
//! each with its `// alloc:` line. What bstd hands back to a caller is made in the caller's
//! allocator ([`mem::Bytes`]).

// `#![no_std]` (S4, docs/v2/plan/design/sys.md): bstd names `core`, `alloc` and blaze's
// crates only. What Rust's `std` alone provides is in `rt`, the one module that names it.
// `alloc` is linked for its types (`Arc`, `Vec<T, A>`, `String`, `Box`): what bstd hands a
// caller is in the caller's allocator, and each use of the global heap that remains has its
// `// alloc:` line until the effects stop owning their bytes. Tests are built on `std` (the
// harness, `eprintln!`).
#![cfg_attr(not(test), no_std)]
#![deny(unsafe_code)]
// A field marked `#[blaze::policy]` (a cost or a guarantee its maker decides) is never chosen
// by a `Default` or a `new()` that takes nothing (std.md, "The second rule").
#![warn(blaze::default_policy)]
// Two builders (std.md, "Two builders"): what is plain Rust is compiled by rustc and by blaze;
// what needs blaze's extensions is under `cfg(blaze)`, the cfg blaze sets for every crate it
// compiles, each with a plain form under `cfg(not(blaze))` that says what the extension adds.
#![cfg_attr(blaze, feature(blaze_comptime, blaze_const_exprs, blaze_verify))]
#![feature(
    allocator_ext,
    atomic_internals,
    coerce_unsized,
    const_cmp,
    const_trait_impl,
    const_type_name,
    contracts,
    core_intrinsics,
    custom_inner_attributes,
    fn_static,
    freeze,
    generic_atomic,
    macro_attr,
    marker_trait_attr,
    portable_simd,
    register_tool,
    rustc_attrs,
    thread_local,
    type_info,
    unsize
)]
#![allow(
    internal_features,
    reason = "`core_intrinsics` for the one reflection intrinsic `own::raw` asks directly \
              (`type_id_vtable`, of an unsized type); `atomic_internals` for the bound of \
              `Atomic<T>` (`AtomicPrimitive`), which `own`'s one `Owns` impl for every atomic \
              has to name; `rustc_attrs` for `#[rustc_comptime]` on `own`'s walk, which calls \
              reflection's compile-time-only functions and so is compile-time-only itself"
)]
#![allow(
    incomplete_features,
    reason = "`contracts`: nightly's `#[core::contracts::ensures]`, the one contract form both \
              builders read"
)]
#![register_tool(blaze)]

// Contracts (ledger V3; docs/v2/plan/design/contracts.md): bstd's functions state what they
// promise as nightly's `#[core::contracts::ensures(..)]`, which rustc and blaze both read (blaze
// lowers it as the pinned nightly does: contracts.md, part 0), and which both run under
// `cfg(contract_checks)` (`-Zcontract-checks`). The native `requires`/`ensures` clauses
// (`blaze_verify`) are syntax rustc cannot parse, so they wait until blaze is the only builder;
// `blaze::native_contracts` then rewrites these. The first are on `budget`, `clock`, `kv` and
// `fs` (std.md, "Contracts"); each `ensures` names only its return and by-value arguments.
extern crate alloc;

pub mod args;
/// Zero-alloc ANSI escape sequence parsing, stripping, terminal display width, and styling.
pub mod ansi;
/// Sound: `AudioOut` and `AudioIn`, PCM streams pulled by the device.
pub mod audio;
/// Driving other applications: `Automation`, narrowed to one.
pub mod automation;
/// An object store beneath a directory: `Blobs`, bytes under the BLAKE3 of what they are.
pub mod blobs;
/// Block devices (D182): `Block` over a `Device` (a file, the simulated device, a raw device).
pub mod block;
/// Budgets: the byte limit of a `Net` and the quota of a `Dir`, and the `Over` each ends in.
pub mod budget;
#[expect(
    unsafe_code,
    reason = "a token cell's two views of its value and its `Sync`, each with its SAFETY"
)]
pub mod cell;
/// Declarative sans-IO CLI argument parser, subcommand router, and ANSI-styled `--help` formatter.
pub mod cli;
/// IPv4/IPv6 CIDR subnet arithmetic, SSRF global-routability classifiers, and binary-trie LPM (`PrefixMap`).
pub mod cidr;
pub mod clock;
/// Turning data into instructions: `Code`, executable memory and native modules.
pub mod code;
/// Strict RFC 6265 HTTP `Cookie` and `Set-Cookie` parser, validator, formatter, and matcher.
pub mod cookie;
/// Strict, bounded, zero-dependency RFC 4180 CSV / TSV reader and writer.
pub mod csv;
/// Authenticated encryption (ChaCha20-Poly1305 RFC 8439), key derivation (HKDF-SHA256 RFC 5869,
/// PBKDF2-HMAC-SHA256 RFC 2898), and strict Ed25519 verification (`blaze-ed25519`).
pub mod crypto;
/// Standard 5-field cron schedule parser and next-occurrence iterator over [`time::DateTime`].
pub mod cron;
pub mod cx;
/// The device capabilities: `Camera`, `Screen`, `Location`, `Bluetooth`, `Usb`, `Serial`,
/// `Clipboard`, `Notify`.
pub mod device;
/// Text and slice diffing (Myers O(ND) linear-space & Patience), unified patch creation,
/// parsing, forward/reverse application, ANSI highlighting, edit distance, and similarity.
pub mod diff;
/// Cryptographic digests (BLAKE3, SHA-256), HMAC-SHA256, constant-time equality, and CRC-32 / Adler-32.
pub mod digest;
/// The simulated disk's ledger: what file calls paid, and which names a crash may undo.
pub mod disk;
/// Sans-IO RFC 1035/3596/2782 DNS wire message encoder and loop-safe decoder.
pub mod dns;
/// Strict, bounded, sans-IO `.env` file parser, variable interpolator, and formatter.
pub mod dotenv;
pub mod entry;
pub mod env;
/// Who decides how a pure function is computed: `Eval` and its providers, memo points, keys.
pub mod eval;
/// Flat values: `Flat`, bytes that mean the same on any thread, process or machine.
#[expect(
    unsafe_code,
    reason = "`Flat` is an `unsafe` property trait: its plain blanket impl and its derived one, each with its SAFETY"
)]
pub mod flat;
pub mod fs;
/// Unix shell glob matching (`Pattern`, `GlobSet`) and capability-confined directory globbing (`glob_dir`).
pub mod glob;
pub mod gpu;
/// Strict, zero-dependency RFC 4648 §8 hexadecimal encoding and decoding.
pub mod hex;
/// HTML/XML entity codec, zero-alloc streaming tokenizer, tag stripper, and allowlist XSS sanitizer.
pub mod html;
pub mod host;
// The heavy areas, each behind its feature (Cargo.toml, "[features]"; std.md, "What a program
// pays for at start"): a program that turns none on links none of their dependencies.
#[cfg(feature = "image")]
pub mod image;
/// The simulated host behind the capabilities blaze-core has no effect for, and how a call
/// on one fails.
pub mod hosted;
/// Standard input: `In`, the byte stream a program was started with.
pub mod input;
pub mod io;
/// RFC 7519 JWT and RFC 7515 JWS (`HS256` & `EdDSA`) signing and verification with explicit clock.
pub mod jwt;
/// The keychain: `Keys`, by named key, and the `Secret` a fetch gives.
pub mod keys;
/// A key-value store beneath a directory: `Kv`, by typed `Key`.
pub mod kv;
/// Local connections: an in-memory `Pair` of linear `End`s, and `Local`, the named sockets
/// beneath a directory.
pub mod local;
/// The locale: time zone, language and formats, as plain data.
pub mod locale;
/// Bounded LZ4 Raw Block and LZ4 Frame v1.6.4 compression and decompression (`XXH32` checksums).
pub mod lz4;
/// Bounded, non-recursive CommonMark + GFM Markdown parser with HTML and ANSI terminal renderers.
pub mod markdown;
pub mod mem;
/// Typed names: a key's, a device's, a module's, an application's.
pub mod name;
mod narrow;
pub mod net;
/// The whole network, a `main` grant narrowed by the program itself (`Network`, D104 B1).
pub mod network;
/// Handing a URL to the user's default application: `Opener`, narrowed to a scheme.
pub mod opener;
/// RFC 4226 HOTP, RFC 6238 TOTP (`otpauth://`), RFC 4648 & Crockford Base32, and RFC 7468 PEM armor.
pub mod otp;
pub use crate::otp::{base32, pem};
pub mod out;
/// Data parallelism over disjoint chunks: `for_each`, `map_into`, `reduce`.
pub mod par;
/// Safe plain-old-data (`Pod`), `Zeroable`, `CheckedBitPattern`, and `TransparentWrapper`
/// conversions (`blaze_own::pod`): Blaze's safe replacement for `core::mem::transmute`.
pub use blaze_own::pod;
/// Ownership properties derived by reflection (`Acyclic`, `NoCapability`), the walk they are
/// instances of, and the handles built on them: `Shared`, `LocalShared`, `Shares`.
#[expect(
    unsafe_code,
    reason = "the trusted assertions of how std's containers hold their contents, the derived \
              impls of the `unsafe` property traits, and `Shares`' one pointer, each with its SAFETY"
)]
pub mod own;
pub mod path;
// `main`'s parameters as grants, and the run spec that grants them (run.md).
pub mod grant;
pub mod spec;
// The hosted world (run.md, "The hosted world"): the wire, the program's side, the host's.
pub mod wire;
#[expect(
    unsafe_code,
    reason = "copies into and out of the host's exchange and calls through its table, each with its SAFETY"
)]
pub mod guest;
#[expect(
    unsafe_code,
    reason = "the C table's entries, its pointer back to its host and views of the exchange, each with its SAFETY"
)]
pub mod hosting;
// The `dry` and `ask` layers and the `fake` providers, at the host's seam (run.md).
pub mod layers;
/// High-performance Wasmtime 64 (`memory64`) batched sans-IO host/guest ring ABI (`blaze_host_v1`).
pub mod wasm64;
/// Compile-time reflection & zero-proc-macro derive bridge (`cfg(blaze)` comptime-first).
pub mod comptime;
/// Order-independent, zero-overhead compile-time Type Sets with safe disjoint borrows & `Lacks<T>`.
pub mod type_set;
/// Anonymous Union Types (`A | B | C`) with small local tags, by-value widening & narrowing.
pub mod union_ty;
/// Zero-cost, `#![forbid(unsafe_code)]` View Types (`bstd::views!`, `ProjectView`, `DisjointSplit`).
pub mod view;
/// Structural Records (`{ a: T, b: U }`) and named-argument functions with defaults (`bstd::record_fn!`, `bstd::call!`).
pub mod record_ty;
/// Zero-copy shared Prefabs (`LocalShare` + `TypeSet!`) and reactive Templates with retractions.
/// Zero-unsafe, sans-IO, Flecs 4.2-equivalent archetype Entity-Component-System (`World`,
/// generational `Entity`, `(Rel, Target)` pairs with `ChildOf` cascade & `IsA` prefab inheritance,
/// incrementally synced `CachedQuery`, reactive observers, and conflict-free phased `Pipeline`).
pub mod ecs;
pub mod prefab;
/// The native backend's reads of its host's facts.
#[cfg(any(target_os = "linux", target_os = "macos", windows, target_os = "wasi"))]
mod native;
pub mod pending;
/// RFC 6901 JSON Pointer, RFC 7396 JSON Merge Patch, and bounded JSONPath queries over `blaze_data::Value`.
pub mod pointer;
/// Power: the battery, and keeping the machine awake.
pub mod power;
/// Other processes: `Procs`, from `Host`, narrowed to one.
pub mod procs;
#[cfg(feature = "net")]
pub mod quic;
/// RFC 3986 percent-encoding, WHATWG `application/x-www-form-urlencoded` query strings, and sans-IO
/// RFC 7578 `multipart/form-data` parsing and formatting.
pub mod query;
/// Linear-time Thompson NFA / PikeVM regular expression engine with bounded step budgets.
pub mod regex;
/// Sans-IO explicit-clock rate limiters (`Gcra`, `TokenBucket`), `CircuitBreaker`, and jittered `Backoff`.
pub mod rate;
/// Making requests: `Http`, one type over a `Net`, the host or a handler. Behind `requests`.
#[cfg(feature = "requests")]
pub mod requests;
/// Name lookup alone: `Resolver`, from a `Net`, with no connect.
pub mod resolver;
pub mod rng;
/// What bstd still takes from Rust's `std`: the one module that names it (S4's ban list).
#[expect(
    clippy::disallowed_types,
    reason = "`LazyLock` and `OnceLock`, named here once for `bstd::sync`'s re-export, which carries the ban's reason"
)]
pub(crate) mod rt;
/// Whom TLS trusts: `Roots`, a set of trust anchors.
pub mod roots;
/// Fast sans-IO HTTP URL router (`Static > :param > *wildcard`, 404 vs 405 `Allow`).
pub mod router;
/// `#[bstd::test]`: a test shaped like `main`, run on the simulation under many seeds with its
/// faults on. Its items are [`test`]'s.
mod simtest;
/// Bounded Google Snappy raw block and Snappy Framing Format compression (`crc32c` masked checksums).
pub mod snappy;
/// Streams to an origin: `Streams`, from the host or a handler.
pub mod streams;
/// Everyday text transformations: case conversions, Unicode `slugify_in`, `natural_cmp`, `dedent_in`/`indent_in`, and ANSI/CJK `truncate_width_in`.
pub mod strx;
/// Sans-IO W3C Server-Sent Events (SSE) streaming parser and event formatter.
pub mod sse;
pub mod sync;
/// ANSI-aware word wrapping (`wrap_ansi_in`) and box-drawn Unicode/ASCII/Markdown tables.
pub mod table;
pub mod task;
// What a run used and where its time went, kept per worker and merged into one `Report` at
// exit (D178): spans, distributions, effects by kind, the pool, the longest chain.
pub mod telemetry;
/// Bounded, iterative (zero-recursion) Mustache-style template engine over `blaze_data::Value`.
pub mod template;
/// The terminal as a device: `Term`, its size, raw mode, styled text and keys.
pub mod term;
/// Civil calendar dates, Unix epoch conversions, strict RFC 3339 parsing/formatting, and human durations.
pub mod time;
pub mod trace;
/// The rustc debug build's check that a must-finish value was finished, not dropped.
pub(crate) mod unfinished;
#[cfg(feature = "video")]
pub mod video;
/// Addresses as types: `Scheme`, `Origin`, `Url`.
pub mod url;
/// Who is running: `User`, its name, ids and home directory.
pub mod user;
/// RFC 9562 UUIDs (v4 random, v7 Unix-millisecond time-ordered, deterministic v8 via BLAKE3)
/// with zero ambient RNG or clock access.
pub mod uuid;
/// The program's windows: `Window`, a surface's size and its events.
pub mod window;
/// Sans-IO RFC 6455 WebSocket handshake (`accept_key`) and frame encoder/decoder.
pub mod ws;
/// Bounded, iterative, XXE-proof XML 1.0 pull parser and canonical writer.
pub mod xml;
/// Strict, bounded, non-recursive YAML 1.2 Core Schema parser and emitter over `blaze_data::Value`.
pub mod yaml;
/// Bounded, traversal-safe Store/Deflate ZIP archive reader and writer over [`compress`] and [`digest::Crc32`].
pub mod zip;
/// Pure `#![no_std]` RFC 8878 Zstandard compression and decompression (`XXH64` checksums).
pub mod zstd;
/// A worker's state as a value the pool owns, in typed columns (D171): `Workers`, `Slot<T>`,
/// `cx.on_worker`.
#[expect(
    unsafe_code,
    reason = "the cells' sparse reservation: typed columns reached by offset, each cell's making and drop, the keys' and handles' `Send`/`Sync`, and a gather's reach of a column (`each_made`), each with its SAFETY"
)]
pub mod workers;
/// One value per worker, written with no lock, merged once at a boundary no task can hold one
/// across (D179's GATHERED).
#[expect(
    unsafe_code,
    reason = "two calls of `Workers::each_made`, whose contract `&mut Gathered` (the only key, lent only to `with`) meets, each with its SAFETY"
)]
pub mod gathered;

/// Bytes held without a copy: their own `Arc`, a range their owner keeps in place, or a
/// `'static` slice (blaze-bytes). Not [`mem::Bytes`], which are bytes being written into an
/// allocator the caller handed in.
pub mod bytes {
    pub use blaze_bytes::{Bytes, le};
}

/// Text. Held without a copy: [`text::Text`], [`bytes::Bytes`] checked once (blaze-bytes). One
/// owned string in an allocator is [`mem::String`]. Many strings are one of these (blaze-own):
///
/// | form | reach for it instead of | cost |
/// |---|---|---|
/// | [`text::Texts`] | `Vec<String>` | one byte buffer and one array of `u32` ends: two blocks in all, 4 bytes a string past its text (`Vec<String>` is 24 bytes and a block a string) |
/// | [`text::SortedTexts`] | a sorted `Vec<String>` searched by hand, a `HashSet<String>` built once | the same two blocks, sorted and each once; `contains` and `find` are O(log n) compares |
/// | [`text::FixedString`] | a `String` for a short text with a known bound | `N` bytes inline, no allocation; a write past `N` is refused and the refusal stays ([`text::Overflow`]) |
pub mod text {
    pub use blaze_bytes::{Text, text_of};
    pub use blaze_own::{FixedString, Overflow, SortedTexts, Texts};
}

/// The vectors, one per need (blaze-own, blaze-kit); every one is made in an allocator the
/// caller names (`Vec::new_in(cx.frame())`), and none has a `new()` but the inline ones.
///
/// | form | reach for it instead of | cost |
/// |---|---|---|
/// | [`vec::Vec`], [`vec_in!`](vec::vec_in!) | `alloc`'s `Vec<T>` on the global heap | one block; index and push O(1), a push past capacity copies every value once; `remove_shift`, `insert_shift` and `scan_for` say they are linear |
/// | [`vec::FixedVec`] | a `Vec` that never holds more than `N` | `N` values inline, no allocation; a push past `N` is refused with the value handed back ([`vec::Full`]) |
/// | [`vec::InlineVec`] | `smallvec`, a `Vec` that is short nearly always | `N` values inline, then one block of the allocator |
/// | [`vec::Segs`] | one `Vec` grown to gigabytes, a `Vec` whose values must not move | segments that double and never move: a push copies nothing, an index is two shifts and one load |
/// | [`vec::SharedSegs`], [`vec::Appender`] | a `Vec` behind a lock that readers read while one thread pushes | as [`vec::Segs`], read without a lock; one appender at a time |
/// | [`vec::Packed`] | a `Vec` of an enum whose widest variant sets every element's size | each value its own bytes end to end; iterated, not indexed; a seek starts from a checkpoint every `K` values |
/// | [`rows::Lists`] | `Vec<Vec<T>>` | see [`rows`] |
pub mod vec {
    pub use blaze_kit::InlineVec;
    pub use blaze_own::{Appender, CollectIn, FixedVec, Full, Pack, Packed, Refused, Segs, SharedSegs, Vec, vec_in};
}

/// One value in a block (blaze-own): [`boxed::Box`] owns it, [`boxed::Rc`] and [`boxed::Arc`]
/// share it, each in an allocator the caller names (`Box::new_in(value, alloc)`).
///
/// | form | reach for it instead of | cost |
/// |---|---|---|
/// | [`boxed::Box`] | `alloc`'s `Box<T>` on the global heap | one block (none for a zero-sized value), one pointer word |
/// | [`boxed::Rc`] | a value shared on one thread | one block for the value and its counts; a clone is a plain increment |
/// | [`boxed::Arc`] | a value shared between threads | one block; every clone and drop writes the one shared count: a value many workers read is borrowed from a scope instead (std.md, "What std makes easy to get slow") |
pub mod boxed {
    pub use blaze_own::arc::Weak as WeakArc;
    pub use blaze_own::rc::Weak as WeakRc;
    pub use blaze_own::{Arc, Box, CallOnce, Rc};
}

/// Rows of items in two arrays (blaze-own's [`rows::Lists`]): every item end to end in one, and
/// where each row ends in the other.
///
/// | form | reach for it instead of | cost |
/// |---|---|---|
/// | [`rows::Lists`] | `Vec<Vec<T>>`; a `HashMap<K, Vec<V>>` keyed by a dense index (`Lists::from_pairs_in`) | two blocks in all, 4 bytes a row past its items (`Vec<Vec<T>>` is a block and 24 bytes a row); a row is pushed whole or opened and filled, and cannot grow once a later one is pushed |
pub mod rows {
    pub use blaze_own::Lists;
    pub use blaze_own::lists::OpenRow;
}

/// A struct-of-arrays vector (blaze-own's [`soa::Soa`], and [`soa!`](soa::soa!), which names the
/// columns of a struct).
///
/// | form | reach for it instead of | cost |
/// |---|---|---|
/// | [`soa::Soa`] | a `Vec` of a struct whose passes each read one or two fields | one block, one packed column a field: a pass over one field reads only that field's bytes, and no field pads another |
pub mod soa {
    pub use blaze_own::soa::{ColumnAt, Columns};
    pub use blaze_own::{Soa, soa};
}

/// The bit set, one: blaze-kit's [`bits::ChunkedBitSet`], and beside it the sparse set of ids.
/// Of the three bit sets in the tree, blaze-ids' `Bits` keeps its words in `alloc`'s `Vec<u64>`
/// on the global heap (no allocator named) and blaze-flow's `Bits` is rows of a dataflow's
/// matrix above bstd; `ChunkedBitSet` names its allocator and is the one a set copied at every
/// block entry needs. A plain bit per id of a table is [`id::TableSet`].
///
/// | form | reach for it instead of | cost |
/// |---|---|---|
/// | [`bits::ChunkedBitSet`] | a bit set copied per block, a `HashSet<u32>` over a large domain | 8 bytes a uniform chunk of 2,048 ids, 264 a mixed one shared by its copies; `contains` reads one or two lines; a copy costs a word a chunk |
/// | [`bits::SparseSet`] | a worklist as a `Vec<u32>` beside a `Vec<bool>` of marks | insert, remove and contains O(1), a clear costs its members; a bit and 4 bytes an id of the domain, plus 4 a member |
pub mod bits {
    pub use blaze_kit::{ChunkedBitSet, SparseSet};
}

/// Paths as a table (blaze-own's [`paths::Paths`]): directories as (parent, name) rows and files
/// as (directory, name) rows.
///
/// | form | reach for it instead of | cost |
/// |---|---|---|
/// | [`paths::Paths`] | `Vec<PathBuf>`, `Vec<String>` of paths, a `HashMap<String, V>` keyed by a path | each name stored once for its directory; a [`paths::PathId`] is 4 bytes; a lookup in a directory is one probe, a whole path's one probe a component |
/// | [`paths::SealedPaths`], [`paths::PathsView`] | a path table rebuilt from text on every read | sealed once, its columns stored as they are and read in place, checked once |
pub mod paths {
    pub use blaze_own::paths::{BadColumns, PathColumns, Remap};
    pub use blaze_own::{DirId, PathId, Paths, PathsView, SealedPaths};
}

/// The maps, one per need (docs/v2/plan/design/std.md, "Collections"); none has a `new()` that
/// takes nothing.
///
/// A hash table names its hasher, in its type and as a value when it is made, as it names its
/// allocator (decision D125), and it is made by one constructor taking one options struct whose
/// every field is written (D126): `Map<K, V, FixedState, A>` made by
/// `Map::new(MapOpts { hasher: FixedState, alloc: frame, capacity: None })` (`Some(n)` to
/// reserve room for `n`), [`map::OrderMap`] by `OrderMap::new(OrderMapOpts { .. })` the same
/// way, and a shared one by `SharedMap::new(SharedMapOpts { name, hasher, alloc, capacity })`.
/// A map of an iterator's pairs is `new` then `extend`. The hashers are in [`hash`]
/// (`FixedState` for ordinary keys, `SipKey` from [`crate::io::Io::sip_key`] for keys an
/// outsider picks, `BuildKeyHasher` for keys that are hashes already). The aliases below that
/// hide the hasher (`HashMap`, `IndexMap`, `ByHash`, `KeyedMap`) and the older constructors
/// (`new_in`, `with_capacity_in`, `from_iter_in`, `with_hasher_in`) go at D125's sweep.
///
/// | need | type |
/// |---|---|
/// | ordinary keys | [`map::Map`] on [`hash::FixedState`] (was [`map::HashMap`]) |
/// | keys that are hashes already | [`map::Map`] on [`hash::BuildKeyHasher`] (was [`map::ByHash`]) |
/// | values stored once, found by their content | [`map::ContentMap`] (one owner), [`id::Sharded`] (shared, dense indices) |
/// | iteration in insertion order, lookup by position | [`map::OrderMap`] on [`hash::FixedState`] (was [`map::IndexMap`]) |
/// | sorted keys, built once, then read | [`map::FlatMap`] |
/// | sorted keys, changed after the build, ranges | [`map::BTreeMap`] |
/// | dense ids as keys | [`id::TableMap`] |
/// | dense ids with a generation | [`mem::Slab`] and [`mem::Handle`] |
/// | keys an attacker may pick | [`map::Map`] on a [`hash::SipKey`] (was [`map::KeyedMap`]) |
/// | shared between threads | [`map::SharedMap`], [`map::SharedByHash`] |
///
/// The ordered ones and their costs (blaze-own), against what code writes without them:
///
/// | form | reach for it instead of | cost |
/// |---|---|---|
/// | [`map::FlatMap`] | a sorted `Vec<(K, V)>` searched with `binary_search_by_key` | one sorted array: lookup O(log n) compares, iteration a slice walk, one block; insert and remove O(n) moves, so build it once from sorted entries (`FlatMap::from_sorted`) |
/// | [`map::BTreeMap`] | `alloc`'s `BTreeMap`, a sorted `Vec` with inserts after the build | a B-tree of up to 15 entries a node, keys and values in separate columns, a node on a cache line: lookup, insert and remove O(log n), ranges |
/// | a [`map::HashMap`] keyed by an [`id::Sharded`] id, or [`paths::Paths`] | a `HashMap<String, V>` | the key text stored once, the key a 4-byte id: a hit allocates nothing and compares an integer |
pub mod map {
    /// Sorted by key, built once: blaze-own's `SortedMap`, one sorted array (lookup O(log n),
    /// insert O(n)).
    pub use blaze_own::SortedMap as FlatMap;
    /// Sorted by key, changed after it is built, with ranges: blaze-own's B-tree.
    pub use blaze_own::BTreeMap;
    pub use blaze_own::sorted::NotSorted;
    pub use blaze_core::{SharedByHash, SharedMap};
    pub use blaze_hash::{ByHash, Equivalent, HashMap, IndexMap, KeyedMap, OrderMap, hash_map::Entry};
    /// The single-owner hash map, its hasher named in the type and at construction (D125), and
    /// what it is made from: `Map::new(MapOpts { hasher, alloc, capacity: None })` (D126).
    pub use blaze_hash::{Map, MapOpts, OrderMapOpts};
    /// The shared tables' options: `SharedMap::new(SharedMapOpts { name, hasher, alloc,
    /// capacity: None })` (D126).
    pub use blaze_hash::SharedMapOpts;
    /// The hashers, here too so that a map's whole explicit form is under one path.
    pub use blaze_hash::{BuildKeyHasher, FixedState, SipKey};
    pub use blaze_hashcons::{Consed as Content, ContentMap, Mismatch, content_of};
}

/// The sets, as [`map`]; a set of dense ids is [`id::TableSet`] or one of [`bits`]. A hash set
/// names its hasher as a map does (D125): `Set<T, FixedState, A>` made by
/// `Set::new(SetOpts { hasher: FixedState, alloc, capacity: None })` (D126), and the same on
/// [`set::OrderSet`] (`OrderSetOpts`) and a shared set (`SharedSetOpts`, with its locks' name);
/// the aliases that hide it (`HashSet`, `IndexSet`, `ByHashSet`, `KeyedSet`) and the older
/// constructors go at the sweep.
///
/// | form | reach for it instead of | cost |
/// |---|---|---|
/// | [`set::FlatSet`] | a sorted, deduplicated `Vec<T>` searched by hand | one sorted array: `contains` O(log n) compares, one block; insert O(n) moves, so build it once (`FlatSet::from_sorted`) |
/// | [`set::BTreeSet`] | `alloc`'s `BTreeSet` | a [`map::BTreeMap`] of the values to `()`: insert, remove and `contains` O(log n), ranges |
pub mod set {
    /// Sorted, built once: blaze-own's `SortedSet`, one sorted array.
    pub use blaze_own::SortedSet as FlatSet;
    /// Sorted, changed after it is built, with ranges: blaze-own's B-tree set.
    pub use blaze_own::BTreeSet;
    pub use blaze_core::SharedSet;
    pub use blaze_hash::{ByHashSet, HashSet, IndexSet, KeyedSet, OrderSet};
    /// The single-owner hash set, its hasher named in the type and at construction (D125), and
    /// what the sets are made from (D126).
    pub use blaze_hash::{OrderSetOpts, Set, SetOpts, SharedSetOpts};
    /// The hashers, as in [`super::map`].
    pub use blaze_hash::{BuildKeyHasher, FixedState, SipKey};
}

/// The hashers behind [`map`] and [`set`]: the fixed seed, the identity hasher for keys that are
/// hashes already, SipHash under a key for keys an attacker picks, and a value's `Hash` fed to
/// BLAKE3 for what is persisted (blaze-hash). A `SipKey` comes from [`crate::io::Io::sip_key`].
/// `LazyKey`, a key drawn for a table in a `static`, is left out: bstd has no statics with
/// interior mutability.
pub mod hash {
    pub use blaze_hash::{
        Blake3Hasher, BuildKeyHasher, FixedHasher, FixedState, KeyHasher, SipHasher, SipKey,
    };
}

/// Identity: a [`id::ContentId`] is the BLAKE3 of what a thing is under its domain, an
/// [`id::Idx`] a dense `u32` index into one arena, and [`id::Sharded`] the interner between them
/// (blaze-ids). The interner is blaze-ids'; `intern` and `blaze-hashcons` are not merged into it
/// yet, and are not re-exported.
///
/// Typed ids and the tables they index (blaze-kit): an id of one table cannot index another.
/// blaze-ids' [`id::Idx`] and blaze-kit's [`id::Id`] are two homes of one concept, to merge
/// (D120 (2)); both are here until then.
///
/// | form | reach for it instead of | cost |
/// |---|---|---|
/// | [`id::Id`], [`id!`](id::id!), [`id::NonMax`] | a `usize` or `u32` index into anything | 4 bytes, and an `Option` of an [`id!`](id::id!) type is 4 bytes too |
/// | [`id::Table`] | a `Vec<T>` indexed by `usize` | one block; a push gives the new id, a read by id is O(1) and indexes only its own table |
/// | [`id::TableSet`] | a `Vec<bool>` of marks, a `HashSet<u32>` of dense ids | one bit an id, up to the largest inserted |
/// | [`id::TableMap`] | a `HashMap<u32, V>` (or `usize`, `u64`) of dense ids | a slot an id up to the largest inserted: a read is one index, no hash |
pub mod id {
    pub use blaze_ids::{
        ContentId, Domain, Fingerprint, Hasher as ContentHasher, Idx, Interner, Sharded, index_key,
    };
    pub use blaze_kit::table::{Branded, Ix};
    pub use blaze_kit::{Id, NonMax, Table, TableMap, TableSet, id};
}

/// Explicit zero-cost newtypes, affine spaces (`ByteOffset` vs `ByteLen` + `ByteSpan`,
/// `TextOffset` vs `TextLen` + `TextSpan`), power-of-two [`newtype::Alignment`], quantities
/// (`Count`, `Capacity`, `Stride`), opaque wrappers (`Port`, `LineNo`, `ColNo`), Kotlin-style
/// selective `delegate { ... }` forwarding (`newtype!`), and `TypeSet!`-powered unit/invariant
/// sets (`Tagged`, `Dim`, `slice_from`).
pub mod newtype;

/// Symbolic algebra, DAG differentiation (`diff`, `grad`, `hessian`, `taylor`), bounded e-graph
/// equality saturation (`simplify`), symbolic equation solving (`solve_for`), `const fn`
/// compile-time expression graphs (`comptime::ConstGraph`), and CSE Rust SSA emission
/// (`blaze-algebra`; `docs/v2/plan/design/symbolic-algebra.md`).
pub mod algebra {
    pub use blaze_algebra::*;
}

/// The stable radix sort (blaze-radix), on workers named at the call ([`sort::on`], with
/// [`sort::Serial`] for the calling thread alone) and type-selected sorting/searching/comparing
/// (`blaze-sort`). The free functions that sort on the thread's ambient workers are left out.
pub mod sort {
    pub use blaze_radix::{
        Csr, Key, PARALLEL, Radix, SMALL, Serial, WIDE, Workers, on, pack_32, pack_64, signed_32,
        signed_64,
    };
    pub use blaze_sort::{
        LSD_FROM, RadixKey, TINY, binary_search, cmp, contains, dedup, eq, is_sorted, kernels,
        lower_bound, lsd_from, max, min, position, sort, sort_by_key_in, sort_in,
        sort_stable_by_in, sort_stable_in, sort_unstable, sort_unstable_by,
    };
}

/// Perfect hashes of fixed name sets, made as the crate compiles (blaze-phf).
pub mod phf {
    pub use blaze_phf::{
        NAME_K, Phf, buckets, bytes_eq, hash, name_hash, name_hash_finish, name_hash_start,
        name_hash_word, slots,
    };
}

/// Format-neutral structured data model (`Value`, `Map`, `Cursor`, `FromValue`, `ToValue`,
/// `value!`), shared across [`json`], [`toml`], and [`cbor`] (`blaze-data`).
pub mod data {
    pub use blaze_data::*;
}

/// Strict RFC 8259 JSON streaming pull reader, writer, and `Value` codec (`blaze-json`).
pub mod json {
    pub use blaze_json::*;
}

/// Deterministic RFC 8949 §4.2.1 CBOR reader, streaming encoder/decoder, and `Value` codec (`blaze-cbor`).
pub mod cbor {
    pub use blaze_cbor::*;
}

/// Strict TOML 1.0 parser, writer, and lossless format-preserving editor (`blaze-toml`).
pub mod toml {
    pub use blaze_toml::*;
}

/// LLM-first tabular structured data format (TAB) over [`data::Value`] (`blaze-tab`).
pub mod tab {
    pub use blaze_tab::*;

    #[cfg(test)]
    mod tests {
        use crate::data::{Int, Map, Value};
        use crate::tab::{Limits, read, write_pretty};
        use blaze_own::{String, Vec};

        #[test]
        fn sparse_table_round_trip_via_bstd_tab() {
            let alloc = blaze_own::testing::heap().alloc();

            let mut row1 = Map::new_in(alloc);
            row1.insert(String::from_str_in("kind", alloc), Value::Text(String::from_str_in("error", alloc)));
            row1.insert(String::from_str_in("file", alloc), Value::Text(String::from_str_in("src/main.rs", alloc)));
            row1.insert(String::from_str_in("line", alloc), Value::Int(Int::from_u64(12)));

            let mut row2 = Map::new_in(alloc);
            row2.insert(String::from_str_in("kind", alloc), Value::Text(String::from_str_in("warning", alloc)));
            row2.insert(String::from_str_in("file", alloc), Value::Text(String::from_str_in("Cargo.toml", alloc)));
            row2.insert(String::from_str_in("line", alloc), Value::Int(Int::from_u64(8)));
            row2.insert(String::from_str_in("fix", alloc), Value::Text(String::from_str_in("remove dep", alloc)));

            let mut rows = Vec::new_in(alloc);
            rows.push(Value::Map(row1));
            rows.push(Value::Map(row2));
            let doc = Value::List(rows);

            let rendered = write_pretty(&doc, alloc).expect("bstd::tab::write_pretty failed");
            let text = rendered.as_str();
            assert!(text.starts_with("[["), "expected tabular header:\n{text}");
            assert!(text.contains("src/main.rs"), "expected bare path:\n{text}");
            assert!(text.contains('_'), "expected sparse absent sentinel `_`:\n{text}");

            let parsed = read(text.as_bytes(), Limits::SHIPPED, alloc).expect("bstd::tab::read failed");
            assert_eq!(parsed, doc);
        }
    }
}

/// Canonical RFC 4648 Base64 encoding and decoding (`STANDARD`, `URL_SAFE`, padded and unpadded; `blaze-base64`).
pub mod base64 {
    pub use blaze_base64::*;
}

/// SemVer 2.0.0 versions and Cargo version requirement matching (`blaze-semver`).
pub mod semver {
    pub use blaze_semver::*;
}

/// Bounded streaming DEFLATE (RFC 1951), zlib (RFC 1950), and gzip (RFC 1952) compression,
/// plus CRC-32 and Adler-32 checksums (`blaze-deflate`).
pub mod compress {
    pub use blaze_deflate::*;
}

/// Deterministic ustar tar writer and traversal-safe sans-IO tar reader (`blaze-tar`).
pub mod tar {
    pub use blaze_tar::*;
}

/// SIMD/SWAR byte and substring search (`memchr`, `memchr2`, `memchr3`, `memrchr`, `memmem::Finder`; `blaze-memchr`).
pub mod search {
    pub use blaze_memchr::*;
}

/// Zero-dependency typed bitflags macro and `Flags` trait (`blaze-bitflags`).
pub mod bitflags {
    pub use blaze_bitflags::*;
}

/// Official UCD Unicode tables: `char_width`, `str_width`, `graphemes`, `nfd_chars`, `is_xid_start`,
/// `is_xid_continue`, `is_emoji` (`blaze-unicode`).
pub mod unicode {
    pub use blaze_unicode::*;
}

/// Deterministic scalar & SIMD floating-point math (fdlibm/musl `sin`/`cos`/`exp`/`ln`/`pow`/...)
/// and 3D geometry (`Vec2`, `Vec3`, `Mat3`, `Affine3`, `Aabb3`, `Interval`; `blaze-math`).
pub mod math {
    pub use blaze_math::*;
}

/// Typed quantities (lengths, angles, areas, volumes, masses, `Nanos`, `Bytes`, `Count`; `blaze-units`).
pub mod units {
    pub use blaze_units::*;
}

/// Named integer conversions (`widen_to`, `narrow`, `truncate_to`, `saturate`; `blaze-int`).
pub mod int {
    pub use blaze_int::*;
}

/// Sans-IO RFC 9113 HTTP/2 with RFC 7541 HPACK (`blaze-h2`).
pub mod h2 {
    pub use blaze_h2::*;
}

/// Columnar in-memory tables and typed batches (`blaze-columns`).
pub mod columns {
    pub use blaze_columns::*;
}

/// Zero-dependency Apache Arrow IPC stream and file reader/writer over [`columns`] (`blaze-arrow-ipc`).
pub mod arrow {
    pub use blaze_arrow_ipc::*;
}

/// Zero-copy records: the bytes in memory are the bytes on disk (blaze-format).
pub mod record {
    pub use blaze_format::{
        ALIGN, Aligned, FormatError, HEADER_LEN, Header, MAGIC, Part, Persist, Placed, Record,
        Rel, SCHEMA_DOMAIN, SchemaHash, Slice, Str, Writer, buffer_id, bytes_of_slice,
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
/// do, events and the next deadline out, over blaze-h1's request writer and response parser ([`http::h1`])
/// and proxies. Its tokio driver is behind blaze-http's `tokio` feature, which bstd does not
/// turn on; `roots` and `proxy_env` read the machine and are left out. HTTP/3 and WebTransport
/// are [`quic`]. Behind `net`.
#[cfg(feature = "net")]
pub mod http {
    pub use blaze_h1 as h1;
    pub use blaze_http::{
        Client, Completion, Config, ConnId, Error, Event, IDLE_FOR, IDLE_MOST, Io, Kind, Method,
        Proxies, Proxy, ProxyError, ProxyKind, REDIRECTS, Range, Request, RequestId, Step, WINDOW,
    };
}

/// Testing. `#[bstd::test]` (the attribute, [`test!`]) is the default test of a bstd program:
/// a function shaped like `main` run on the simulation under [`test::Seeds`], 256 by default,
/// with the [`test::Fault`] kinds its parameters can meet on unless it leaves one out by name, a failure saying its
/// seed and the line that replays it ([`test::granted`]; docs/v2/plan/design/bstd-sim-test.md).
/// Then blaze-core's seeded simulation itself ([`test::Sim`]: a virtual clock, shuffled
/// completions, failures, crashes), the model checker for lock-free code ([`test::model`], its
/// shims in [`test::checked`]), and allocation counting ([`test::Counting`], [`test::measure`]).
/// A property test over generated inputs is `blaze_sim::check`, a direct dev-dependency
/// (docs/v2/plan/design/sim.md), not re-exported here: it is blaze's own tests' engine.
pub mod test {
    pub use blaze_alloc_count::{Counting, Used, assert_at_most, installed, measure};
    pub use blaze_check::{Builder, MAX_THREADS, model};
    pub use blaze_core::sim::{
        Faults, Journal, KILLED, Program, Ran, Sim, SimAbove,
        seeds::{Broke, Counts, Ended, Failed, Fault, Given, Passed, Plan, Replay, Run, Seeds},
    };

    pub use crate::simtest::{BUDGET, Outcome, seeded};
    /// `#[bstd::test]` with grants (D109): the attribute's setup, the plan by parameter kind,
    /// the test entry, and its one-block report.
    pub use crate::simtest::granted::{
        ALL_FAULTS, Broken, Endpoint, File, Files, MOST_GRANTS, Planned, Setup, SetupError, Test, Var, bit, granted,
        one_seed, reach, report, simulated, spec_of,
    };
    /// What makes each parameter of a `#[bstd::test]` (D109): a capability or a drawn value.
    pub use crate::simtest::param::{
        Draws, DrawnReport, From, Making, Param, Slot, drawn, grant_count, grants, slot,
    };
    /// The drawn half (D109), with bstd's feature `sim-draws`.
    #[cfg(any(test, feature = "sim-draws"))]
    pub use crate::simtest::drawn::{CASES, CASES_MOST, DrawnGiven, TAPE_SALT, cases};
    /// A type of a user's drawn in a test: `bstd::test::AnyIn`, `bstd::test::any_in_by_any!`.
    #[cfg(feature = "sim-draws")]
    pub use blaze_sim::{Any, AnyIn, any_in_by_any};

    /// A native world over a test's directory, made with a test's root of real-OS authority
    /// (`blaze_core::testing::os()`): the one way a test makes a native world once batch B1
    /// deletes `Io::native`. Behind the `testing` feature.
    #[cfg(any(test, feature = "testing"))]
    pub use crate::rt::testing_native::native;

    /// The model checker's shims, with std's API: a crate's code switches to them under
    /// `cfg(blaze_check)`.
    pub mod checked {
        pub use blaze_check::{cell, hint, linear, sync, thread};
    }
}

/// What a crate of the blaze edition has in scope unasked (docs/v2/plan/design/std.md, "The
/// blaze edition: opting in"): one module an edition, named for it, as Rust's `std::prelude`
/// has one a Rust edition.
pub mod prelude {
    /// The prelude of `edition = "blaze-2026.11"`: `core`'s prelude of Rust 2024, the types
    /// a `main` names (`Cx`, the entry's `Error`, and every grant: `Out`, `Args`, `Clock`,
    /// `Dir<ReadOnly>`, `Net`, `Env`, the whole capabilities), and `core`'s macros that Rust's `std` hands on from its
    /// root. Nothing here reaches the global heap (D120): the collections are blaze-own's
    /// `Vec` and `Box`, which have no `new()` and are made in an allocator the code was handed
    /// from `main` down (`Vec::new_in(cx.frame())`, `Box::new_in(value, alloc)`); `Frame` (what
    /// `cx.frame()` is) and `Alloc` (the one-word handle a function takes) are here so a
    /// program can name the allocator in a type (`Box<dyn Area + '_, &Frame>`). `Heap` is the
    /// process's heap, a grant `main` names (`heap: Heap`) for what outlives its tasks and for a
    /// pinned box (`Box::pin_in(future, heap)`); nothing else hands it out. `String` is
    /// blaze-own's, made in one, with `format_in!` in place of `format!`; the entry's `Error`
    /// keeps its words inline. `vec!`, `format!`, `ToString` and `ToOwned`, which reach the
    /// global heap by themselves, are not here (std.md, "Memory is explicit").
    pub mod blaze_2026_11 {
        pub use core::prelude::rust_2024::*;
        pub use core::{
            assert_eq, assert_ne, debug_assert, debug_assert_eq, debug_assert_ne, matches, panic, todo,
            unimplemented, unreachable, write, writeln,
        };

        pub use crate::{
            boxed::Box,
            cx::Cx,
            entry::Error,
            mem::{Alloc, Frame, Heap, String, format_in},
            vec::Vec,
        };

        // Every type a `main` may take as a grant (crate::grant, each `impl Grant for`), so
        // a program names what it takes without a `use` (D104).
        pub use crate::{
            args::Args,
            audio::Audio,
            automation::Automation,
            clock::Clock,
            code::Code,
            device::{Bluetooth, Camera, Clipboard, Location, Notify, Screen, Serial, Usb},
            env::Env,
            fs::{Dir, Fs, ReadOnly, ReadWrite},
            grant::Spec,
            host::Host,
            input::In,
            keys::Keys,
            locale::Locale,
            mem::Mem,
            net::Net,
            opener::Opener,
            out::Out,
            power::Power,
            rng::Rng,
            roots::Roots,
            streams::Streams,
            term::Term,
            user::User,
            window::Window,
        };
        #[cfg(feature = "requests")]
        pub use crate::requests::Http;
    }
}

#[cfg(test)]
mod tests;

use core::{fmt, time::Duration};

pub use crate::{
    clock::Clock,
    cx::Cx,
    env::VarName,
    eval::Eval,
    io::{Io, Missing, Offers},
    net::HostPort,
    path::Path,
    pending::Pending,
    task::{Scope, Task},
};
#[cfg(feature = "requests")]
pub use crate::requests::Http;
pub use crate::{
    url::{Origin, Url},
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

impl core::error::Error for Stopped {}

/// A safeguard a test turns off to prove its oracle sees the bug (skill `testing`, planted-bug
/// controls). Always [`Plant::None`] outside this crate's tests: only their constructor
/// (`Io::planted`) sets another.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[cfg_attr(not(test), expect(dead_code, reason = "the planted bugs are made by the tests only"))]
#[cfg_attr(not(test), expect(blaze::unwired, reason = "wired: a planted-bug control only its tests make"))]
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
    /// `File::flush` says it flushed without asking the driver.
    SyncSkipped,
    /// `Env::only` replaces what is allowed rather than narrowing it.
    EnvOnlyReplaces,
    /// `Pending::cancel` returns without waiting for its op to end.
    CancelSkipsWait,
    /// `File::close_synced` says it closed without flushing.
    CloseSkipsSync,
    /// `Scope::join` returns `Ok` though a task failed.
    JoinDropsFailure,
    /// Every root accessor of `Io` hands its capability out, offered or not.
    OffersIgnored,
    /// A narrowing of the capabilities on `narrow::Allowed` (`Keys::only`, `Opener::only`, ...)
    /// and of `Location` and `Clipboard` gives what was asked for, reached before or not.
    OnlyWidens,
    /// `Kv::put_synced` publishes the value unsynced: nothing of it is flushed and its name
    /// is not synced, so a crash loses or tears it.
    KvPutUnsynced,
    /// A write to a local pair behind bytes not yet read says it wrote its chunk and drops it.
    LocalDropsChunk,
    /// `Http::only` hands back the capability it was given.
    HttpOnlyIgnored,
    /// A `Net`'s byte limit and a `Dir`'s quota are looked at after the bytes are sent or
    /// written, not before.
    BudgetCheckedAfter,
    /// A tuple's key leaves its last element out: two calls that differ only there share one.
    KeyIgnoresLast,
    /// `Eval::memo` never drops a generation: its entries pass its limit.
    MemoNeverEvicts,
    /// `Eval::memo` keeps every entry under one point: two points with equal arguments share.
    MemoSharesPoints,
    /// A nested call is handed its caller's parent as its own: provenance skips a level.
    NestedUnderGrandparent,
    /// A publish hands its rename to the driver and reports done without waiting for it.
    PublishBeforeRename,
    /// A `_synced` file call skips the sync of its directory's names.
    DurableSkipsNameSync,
    /// An `_unsynced` rename or publish renames through the syncing `Effect::Rename`.
    UnsyncedRenameSyncs,
    /// `Written::flush` asks for the flush and hands out its `Flushed` without waiting for it.
    FlushedBeforeFlush,
    /// `Flushed::clone_to` does not look at whether its file was changed behind it.
    StaleFlushedCloned,
    /// `par::reduce` and `par::map_into` join their chunks in the order they ran, not by
    /// position.
    ParJoinsInRunOrder,
    /// `Building::freeze` keeps one count of its own: the value is never dropped.
    FreezeLeaks,
    /// `Swap::publish` drops the value it replaced at once, not after its grace.
    SwapFreesAtOnce,
    /// A `Flight` asked for a key already being computed starts a second computation.
    FlightComputesTwice,
    /// A full `Chan` takes one more value.
    ChanPassesCapacity,
    /// A second `PublishOnce::set` replaces the value readers saw.
    PublishOnceOverwrites,
    /// `Cx::lock_order` hands out a second token while the first is alive.
    OrderTokenTwice,
    /// A task resumed from a wait is put on another worker than the one it started on.
    TaskMigrates,
    /// `Fs::ancestors` asks the driver for every folder above the start, not only the granted
    /// ones: the bound not checked.
    AncestorsUnbounded,
}
