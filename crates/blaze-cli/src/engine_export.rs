//! `blaze export [dir] [-p pkg] [--bin name] [-o output_path] [--workers N] [--stats]`:
//! builds a standalone native AArch64 executable (Mach-O on macOS arm64, ELF PIE on Linux arm64)
//! from the whole-program reachability closure of the selected binary target, powered by
//! `blaze-export`.
//!
//! Flow:
//! 1. Validate CLI flags and check the workspace via `engine_check::check_for_start`.
//! 2. Pick the target binary via `run_package::pick` (`-p`, `--bin`, or workspace defaults).
//! 3. Resolve crates, names records, body ids, and (when present) laid-out native C image +
//!    C exports via `engine_native::export_native_for`.
//! 4. Compute entry roots (`blaze_ladder::run::special::run_roots`, including `lang_start`,
//!    `main`, `personality`, and any C-called Rust exports) and perform a whole-program
//!    reachability walk via `blaze_ladder::body_ids::Walk` over the home `HomeCodeStore`
//!    (gating every row with `RunIds::runnable`).
//! 5. Materialize any code records that carry item-relative source positions (`positions() != 0`)
//!    into position-free canonical `CodeRecord`s using today's names records.
//! 6. Detect Rust thread-local usage (`Space::Tls`, `RowKind::ThreadLocal`, `tls_getter_symbol`,
//!    `tls_desc_symbol`), emit `Special::Getter` and `Special::Ender`, and pass `TlsRuntime`.
//! 7. Call `blaze_export::export` to emit the standalone executable bytes, atomically publish to
//!    the output path via `bstd::fs`, mark executable (`0o755`), and print a concise summary (or
//!    detailed breakdown with `--stats`).

use core::fmt;

use blaze_code::datum::{DatumView, OWN_DATUM, Place as DatumPlace};
use blaze_code::record::{CodeRecord, CodeView, NO_TARGET, PositionPart, Site, SiteKind, Space, Symbol as CodeSymbol, Target};
use blaze_export::{Data, DataPlace, DataWord, Entry, ExportError, Exported, Function, Panic, Platform, Profile, ProgramIn, Tier, TlsRuntime};
use blaze_ladder::body_ids::{self, RowKind, Scratch, Stepped, Walk};
use blaze_ladder::code_records::{CodeKey, entry_of, statics};
use blaze_ladder::front2::program::lazy::{SharedLazyFront, SharedLazyFrontHandle, SharedLazyFrontStats};
use blaze_ladder::run::make_one::{MOST_WALKED, Maker, RecordsProgram, Typing, TypingCounts};
use blaze_ladder::run::special::{EntryForm, Roots, Special, make_special, run_roots};
use blaze_ladder::run::RunError;
use blaze_own::{AString, AVec, Alloc, Box, Bump, Erased, SharedAlloc, String, Vec, VecDeque};
use blaze_typeck::ItemKey;
use bstd::Cx;
use bstd::entry::Status;
use bstd::fs::{Content, Durable, Fs, FsError, HomeError};
use bstd::mem::Frame;
use bstd::out::Out;
use bstd::path::{Path, PathError};

use crate::body_types::{SyncPackDirs, TreesSeen};
use crate::code_store::HomeCodeStore;
use crate::engine_check::{Standing, check_for_start};
use crate::engine_records::EngineRecords;
use crate::engine_start::{RunProcs, StartError, body_ids, end_procs, start_facts, write_start_profile};
use crate::phases::Phases;
use crate::run_ids::NotRunnable;
use crate::{CliError, program, run_names, run_package};

/// Virtual reservation for the export scratch frame (walk tables, materialized records, data
/// words, native image layout, and the emitted executable bytes). Untouched pages cost zero RSS.
const EXPORT_FRAME_RESERVE: usize = 4 << 30;

/// Maximum concurrent codegen workers during `blaze build` whole-program reachability closure.
/// Workers share one sharded [`SharedLazyFront`] (frozen heads, evaluated constants/statics,
/// crate member indexes, and checked dep/toolchain `Typed` records) while keeping only a small
/// bounded per-worker epoch frame (~16–64 MiB/worker), so concurrency scales linearly to all
/// host cores (18 on Mac, 128 on server; bounded per Rule 18).
const MAX_EXPORT_WORKERS: usize = 256;

/// Cold misses handled serially before spinning up parallel codegen workers so warm no-op builds
/// and single-function edits pay zero worker/frame startup overhead.
const EXPORT_PAR_MIN_MISSES: usize = 4;

/// Per-worker frame and heap-epoch reset budgets during parallel export codegen. Because
/// [`SharedLazyFront`] retains frozen heads, constants, member indexes, and dep/toolchain typed
/// bodies and [`ScopeRecords`] retains shared module verification bitmaps + `FileItems` outlines
/// across worker epoch resets, each worker's local `Frame` + `Maker` keeps a warm working set
/// (~512 MiB–1 GiB / 1M–2M `Store::tys`) without thrashing `BZFZ` head re-imports every few dozen
/// functions, while still resetting cleanly before unbounded growth.
const EXPORT_WORKER_FRAME_HEADROOM: usize = 512 << 20;
const EXPORT_WORKER_FRAME_HARD_RESET_AT: usize = 1024 << 20;
const EXPORT_WORKER_FRAME_KEEP: usize = 64 << 20;
/// Soft/hard caps on distinct live lowered `tabled` bodies and `Store::tys` nodes retained in one
/// worker epoch (sibling monomorphizations of the same `tabled` fn count as 1 live body and stay
/// warm across the soft cap).
const EXPORT_WORKER_MAX_LIVE_BODIES: usize = 2048;
const EXPORT_WORKER_HARD_LIVE_BODIES: usize = 4096;
const EXPORT_WORKER_MAX_STORE_TYS: usize = 1_048_576;
const EXPORT_WORKER_HARD_STORE_TYS: usize = 2_097_152;
const EXPORT_WORKER_SCRATCH: usize = 1 << 20;
/// Per-worker in-flight inbox depth and work-stealing low-water mark: keeping 2–4 jobs in each
/// worker's private inbox eliminates coordinator round-trip waits while keeping the bulk of queued
/// jobs inside the coordinator's stealable `worker_queues` (so at most `EXPORT_PER_WORKER_IN_FLIGHT
/// - 1` = 3 jobs can ever sit behind a giant function like `visit_seq`).
const EXPORT_PER_WORKER_IN_FLIGHT: usize = 4;
const EXPORT_STEAL_LOW_WATER: usize = 2;
const EXPORT_MAX_QUEUED_PER_WORKER: usize = 8192;
const EXPORT_HAND_WAIT: core::time::Duration = core::time::Duration::from_secs(30);
const EXPORT_PROCS_WAIT: core::time::Duration = core::time::Duration::from_secs(60);
const EXPORT_RECV_SLICE: core::time::Duration = core::time::Duration::from_millis(2);
const EXPORT_END_WAIT: core::time::Duration = core::time::Duration::from_secs(300);

/// Program root shape domain for `blaze build` (`start_list::ListName::of` -> `RootName::program`),
/// distinct from `engine_start.rs`'s `RUN_SHAPE` so `blaze build`'s release-profile code root
/// and `blaze run`'s dev-profile code + start-list root never overwrite or unroot each other.
const BUILD_SHAPE: &str = "blaze build: the code of a program 1";

/// Computes the checkout-independent program root shape for `blaze build` (`RootName::program`).
/// Distinct from `engine_start.rs`'s `RUN_SHAPE`, and includes `(workspace, program_crate,
/// package, bin, root)` without any checkout path so two unrelated workspaces with the same
/// package/bin/root stay disjoint while two worktrees of the same repository share one root
/// (rules 25, 30, 36).
fn build_program_shape(
    picked: &run_package::Program<'_>,
    workspace: Option<blaze_ids::ContentId>,
    program_crate: blaze_id::CrateItemKey,
) -> blaze_ids::ContentId {
    let mut h = blaze_ids::Hasher::new(BUILD_SHAPE);
    match workspace {
        Some(id) => {
            h.update(&[1]);
            h.id(&id);
        }
        None => {
            h.update(&[0]);
        }
    }
    h.id(&program_crate.0);
    h.part(picked.package.as_bytes());
    h.part(picked.bin.as_bytes());
    h.part(picked.root.as_bytes());
    h.finish()
}

/// Outcome of persisting `blaze build`'s closing program and toolchain roots.
struct ExportRootsKept {
    program: Result<crate::store::roots::Rooted<crate::store::roots::Program>, crate::store::roots::RootError>,
    toolchain: Result<Option<crate::store::roots::Rooted<crate::store::roots::Toolchain>>, crate::store::roots::RootError>,
}

/// Roots the program's reached non-toolchain code + `RunIds` table (`keep_program`) and any newly
/// emitted toolchain code (`keep_toolchain`) before the command's closing store commit.
fn keep_export_roots<D: blaze_core::Driver>(
    lent: &blaze_cas::plain::Lent<D, crate::store::plain_place::StoreAlloc>,
    today: &crate::start_list::Today,
    toolchain: blaze_toolchain::ToolchainId,
    walked: &[blaze_cas::Key],
    toolchain_made: &[blaze_cas::Key],
) -> ExportRootsKept {
    let program = crate::store::roots::keep_program(lent, today, None, walked).map(|(rooted, _)| rooted);
    let toolchain = crate::store::roots::keep_toolchain(lent, toolchain, toolchain_made, &[]);
    ExportRootsKept { program, toolchain }
}

/// Persists a parallel export worker's newly typechecked dependency/toolchain `Checked` records
/// into `lent` and records program-class hits/writes and newly written toolchain keys for closing
/// GC root registration.
fn persist_worker_typed_batch<'a, D: blaze_core::Driver>(
    lent: &blaze_cas::plain::Lent<D, crate::store::plain_place::StoreAlloc>,
    batch: Option<crate::engine_body_record::WorkerTypedBatch>,
    dep_typed_program: &mut AVec<'a, blaze_cas::Key>,
    dep_typed_toolchain: &mut AVec<'a, blaze_cas::Key>,
    frame: &Frame,
    heap: &'static blaze_own::Heap,
) -> Result<(), CliError> {
    let Some(batch) = batch else { return Ok(()) };
    dep_typed_program.extend_from_slice(batch.hit_program.as_slice());
    for put in batch.puts {
        let cas_key = blaze_cas::Key(put.key.0);
        let wrote = crate::engine_body_record::put_dep_typed_if_absent(lent, put.key, put.bytes.as_slice()).map_err(|why| {
            box_err(
                ExportCliError::KeepRoots {
                    class: "dep/toolchain typed body",
                    shown: owned_fmt(format_args!("{why}"), frame, heap.alloc()),
                },
                heap,
            )
        })?;
        match put.class {
            blaze_ladder::code_records::CodeClass::Program => dep_typed_program.push(cas_key),
            blaze_ladder::code_records::CodeClass::Toolchain if wrote => dep_typed_toolchain.push(cas_key),
            blaze_ladder::code_records::CodeClass::Toolchain => {}
        }
    }
    Ok(())
}

/// Surfaces any main-thread typed-record persistence error, restores any concurrently swept
/// entries in `codes`, roots all reached `CodeRecord`s and persisted dep/toolchain `Typed` keys
/// (`keep_program` / `keep_toolchain`), and commits `lent` immediately if any puts or roots are
/// uncommitted.
///
/// Called **immediately after the reachability walk finishes** (before LTO, `wasm64`, and
/// `blaze_export::export()`) and again before `blaze_export::export()` if LTO/TLS emitted extra
/// entries, so a linker/LTO/output failure never discards compiled `CodeRecord`s or `Typed`
/// batches, and unlent store arena chunks are freed before linking.
#[expect(clippy::too_many_arguments, reason = "export root checkpoint joins code store and dep/toolchain typed keys")]
fn checkpoint_export_roots<D: blaze_core::Driver>(
    lent: &blaze_cas::plain::Lent<D, crate::store::plain_place::StoreAlloc>,
    today: &crate::start_list::Today,
    toolchain: blaze_toolchain::ToolchainId,
    codes: &mut HomeCodeStore<'_, D>,
    typed: &crate::engine_body_record::StoreTyped<'_>,
    dep_typed_program: &[blaze_cas::Key],
    dep_typed_toolchain: &[blaze_cas::Key],
    frame: &Frame,
    heap: &'static blaze_own::Heap,
) -> Result<(), CliError> {
    if let Some(why) = typed.persist_error() {
        return Err(box_err(
            ExportCliError::KeepRoots {
                class: "dep/toolchain typed body",
                shown: owned_fmt(format_args!("{why}"), frame, heap.alloc()),
            },
            heap,
        ));
    }
    codes.ensure_persisted().map_err(|why| {
        box_err(
            ExportCliError::KeepRoots { class: "program code entries", shown: owned_fmt(format_args!("{why}"), frame, heap.alloc()) },
            heap,
        )
    })?;
    let mut walked = codes.reached();
    let mut toolchain_made = codes.toolchain_made();
    walked.extend_from_slice(typed.kept_program());
    walked.extend_from_slice(dep_typed_program);
    toolchain_made.extend_from_slice(typed.kept_toolchain());
    toolchain_made.extend_from_slice(dep_typed_toolchain);
    let kept_roots = keep_export_roots(lent, today, toolchain, &walked, &toolchain_made);
    kept_roots.program.map_err(|why| {
        box_err(
            ExportCliError::KeepRoots { class: "program code", shown: owned_fmt(format_args!("{why}"), frame, heap.alloc()) },
            heap,
        )
    })?;
    kept_roots.toolchain.map_err(|why| {
        box_err(
            ExportCliError::KeepRoots { class: "toolchain code", shown: owned_fmt(format_args!("{why}"), frame, heap.alloc()) },
            heap,
        )
    })?;
    if lent.needs_commit() {
        lent.commit().map_err(|why| {
            box_err(
                ExportCliError::KeepRoots { class: "store commit", shown: owned_fmt(format_args!("{why}"), frame, heap.alloc()) },
                heap,
            )
        })?;
    }
    Ok(())
}

/// Why `blaze export` stopped before writing a valid executable, naming the thing (rule 19).
#[derive(Debug)]
pub enum ExportCliError {
    /// This host architecture/OS has no native executable writer in `blaze-export`.
    UnsupportedHost,
    /// Could not reserve the export scratch frame.
    Frame,
    /// Native C image layout or export resolution failed.
    Native { shown: AString<'static> },
    /// Building the code builder configuration failed.
    Builder { shown: AString<'static> },
    /// Computing the program's entry roots failed.
    Roots { shown: AString<'static> },
    /// A reached workspace body did not check clean (the call/reachability gate, D151).
    NotChecked {
        krate: AString<'static>,
        function: ItemKey,
        symbol: [u8; 32],
        row: usize,
        reached: usize,
        compiled: usize,
        cached: usize,
        why: NotRunnable,
    },
    /// Stepping an instance/static/TLS row in the reachability walk failed.
    Walk { symbol: [u8; 32], shown: AString<'static> },
    /// Whole-program cross-function LIR inlining and LTO emission failed.
    Lto { shown: AString<'static> },
    /// Making a special record (TLS getter or ender) failed.
    Special { what: &'static str, shown: AString<'static> },
    /// Reading a code or datum entry from the store failed.
    Store { symbol: [u8; 32], shown: AString<'static> },
    /// A code or datum entry was missing from the store right after stepping.
    MissingEntry { symbol: [u8; 32] },
    /// Decoding a code entry, code record, or datum record failed.
    Decode { symbol: [u8; 32], what: &'static str, shown: AString<'static> },
    /// Resolving an item-relative source position row failed.
    Position { symbol: [u8; 32], shown: AString<'static> },
    /// Materializing a position-free code record failed.
    Materialize { symbol: [u8; 32], shown: AString<'static> },
    /// `blaze_export::export` refused the program.
    Export(ExportError),
    /// Whole-program `wasm64` (`memory64`) compilation failed.
    Wasm64 { shown: AString<'static> },
    /// Persisting the closing program or toolchain store root failed (Rule 3: never silently drop roots).
    KeepRoots { class: &'static str, shown: AString<'static> },
    /// Invalid output path supplied via `-o` / `--output`.
    BadOutputPath { path: AString<'static>, why: OutputPathIssue },
    /// Opening the output folder failed.
    OutputHome(HomeError),
    /// Output path validation failed.
    OutputPath(PathError),
    /// Publishing the output executable failed.
    WriteOutput(FsError),
    /// Marking the published file executable (`0o755`) failed.
    #[cfg(unix)]
    Chmod { path: AString<'static>, errno: blaze_sys::Errno },
}

struct Hex<'a>(&'a [u8; 32]);

impl fmt::Display for Hex<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0.iter() {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Display for ExportCliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExportCliError::UnsupportedHost => write!(f, "blaze build: only arm64 macOS and arm64 Linux hosts are supported (engine_export.rs)"),
            ExportCliError::Frame => write!(f, "blaze build: could not reserve the build scratch frame (engine_export.rs)"),
            ExportCliError::Native { shown } => write!(f, "blaze build: native image: {} (engine_native.rs)", shown.as_str()),
            ExportCliError::Builder { shown } => write!(f, "blaze build: code builder: {} (engine_export.rs)", shown.as_str()),
            ExportCliError::Roots { shown } => write!(f, "blaze build: program roots: {} (engine_export.rs)", shown.as_str()),
            ExportCliError::NotChecked { krate, function, symbol, row, reached, compiled, cached, why } => write!(
                f,
                "blaze build: reached fn {:#x} in crate `{}` (symbol {}; row {row}/{reached}, {compiled} compiled, {cached} cached) may not be built: {why} (engine_export.rs)",
                function.0,
                krate.as_str(),
                Hex(symbol)
            ),
            ExportCliError::Walk { symbol, shown } => write!(f, "blaze build: compiling symbol {}: {} (engine_export.rs)", Hex(symbol), shown.as_str()),
            ExportCliError::Lto { shown } => write!(f, "blaze build: {} (engine_export.rs)", shown.as_str()),
            ExportCliError::Special { what, shown } => write!(f, "blaze build: building {what}: {} (engine_export.rs)", shown.as_str()),
            ExportCliError::Store { symbol, shown } => write!(f, "blaze build: reading code entry {}: {} (engine_export.rs)", Hex(symbol), shown.as_str()),
            ExportCliError::MissingEntry { symbol } => write!(f, "blaze build: code entry {} missing from store after step (engine_export.rs)", Hex(symbol)),
            ExportCliError::Decode { symbol, what, shown } => write!(f, "blaze build: decoding {what} {}: {} (engine_export.rs)", Hex(symbol), shown.as_str()),
            ExportCliError::Position { symbol, shown } => write!(f, "blaze build: resolving source position in {}: {} (engine_export.rs)", Hex(symbol), shown.as_str()),
            ExportCliError::Materialize { symbol, shown } => write!(f, "blaze build: materializing position-free record {}: {} (engine_export.rs)", Hex(symbol), shown.as_str()),
            ExportCliError::Export(error) => write!(f, "blaze build: {error}"),
            ExportCliError::Wasm64 { shown } => write!(f, "blaze build --target wasm64: {} (engine_export.rs)", shown.as_str()),
            ExportCliError::KeepRoots { class, shown } => write!(f, "blaze build: keeping {class} root failed: {} (engine_export.rs)", shown.as_str()),
            ExportCliError::BadOutputPath { path, why } => write!(f, "blaze build: invalid output path `{}`: {why} (engine_export.rs)", path.as_str()),
            ExportCliError::OutputHome(error) => write!(f, "blaze build: output directory: {error} (engine_export.rs)"),
            ExportCliError::OutputPath(error) => write!(f, "blaze build: output path: {error} (engine_export.rs)"),
            ExportCliError::WriteOutput(error) => write!(f, "blaze build: writing executable: {error} (engine_export.rs)"),
            #[cfg(unix)]
            ExportCliError::Chmod { path, errno } => write!(f, "blaze build: chmod 0755 `{}` failed: {errno:?} (engine_export.rs)", path.as_str()),
        }
    }
}

impl core::error::Error for ExportCliError {}

fn owned_str(text: &str, alloc: Alloc<'static>) -> AString<'static> {
    let mut s = String::new_in(alloc);
    s.push_str(text);
    s
}

fn crate_name_of_item<'a>(list: &'a blaze_world::crate_list::CrateList<'_>, item: ItemKey) -> &'a str {
    let b = item.0.to_be_bytes();
    let prefix = [b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]];
    for at in 0..blaze_world::crate_list::CrateGraph::len(list) {
        if list.key(at).prefix() == prefix {
            return blaze_world::crate_list::CrateGraph::name(list, at);
        }
    }
    "<unknown>"
}

fn crate_of_item(list: &blaze_world::crate_list::CrateList<'_>, item: ItemKey, alloc: Alloc<'static>) -> AString<'static> {
    owned_str(crate_name_of_item(list, item), alloc)
}

/// Formats a deterministic, human-readable Rust symbol name for one reached function instance
/// into `bump` (for Mach-O local `N_SECT` symbols so `nm`, `lldb`, `sample`, `xctrace`, and
/// `otool`/`llvm-objdump` show Rust function names instead of raw addresses).
fn format_symbol_name<'a, 'n>(
    symbol: CodeSymbol,
    tabled: ItemKey,
    shown: Option<blaze_ladder::run::make_one::InstanceShown>,
    records: &'n blaze_ladder::front2::source::Records<'n>,
    list: &blaze_world::crate_list::CrateList<'_>,
    buf: &mut AString<'_>,
    bump: Bump<'a>,
) -> &'a str {
    use core::fmt::Write as _;
    use blaze_ladder::front2::source::NamesSource as _;

    buf.clear();
    if let Some(shown) = shown
        && let Some(path) = shown.item.path()
    {
        buf.push_str(path);
        if let Some(args) = shown.args.as_deref()
            && !args.is_empty()
        {
            if let Some(inner) = args.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
                buf.push('<');
                buf.push_str(inner);
                buf.push('>');
            } else {
                buf.push_str(args);
            }
        }
        return bump.alloc_str(buf.as_str());
    }
    if let Some(lib) = records.key_beginning(tabled.0.to_be_bytes()) {
        let item = blaze_ladder::front2::item_shown::ItemShown::of_lib(records, lib);
        if let Some(path) = item.path() {
            return bump.alloc_str(path);
        }
    }
    let krate = crate_name_of_item(list, tabled);
    let short = u64::from_be_bytes([
        symbol.0[0], symbol.0[1], symbol.0[2], symbol.0[3],
        symbol.0[4], symbol.0[5], symbol.0[6], symbol.0[7],
    ]);
    let _ = write!(buf, "{krate}::fn_{short:016x}");
    bump.alloc_str(buf.as_str())
}

fn owned_fmt(args: fmt::Arguments<'_>, frame: &Frame, alloc: Alloc<'static>) -> AString<'static> {
    let formatted = bstd::mem::format_in!(frame, "{args}");
    owned_str(formatted.as_str(), alloc)
}

fn box_err(error: ExportCliError, heap: &'static blaze_own::Heap) -> CliError {
    CliError::Export(Box::new_in(error, heap.alloc()))
}

/// Host target platform for `blaze export`.
fn host_platform() -> Option<Platform> {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some(Platform::MacosArm64)
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        Some(Platform::LinuxArm64)
    } else {
        None
    }
}

/// Validates CLI flags specific to `blaze export`.
fn check_export_flags(flags: &crate::Flags<'_>) -> Result<(), CliError> {
    if flags.fix.is_some() {
        return Err(CliError::Usage("--fix is `blaze check`'s"));
    }
    if flags.each {
        return Err(CliError::Usage("--each is `blaze check`'s"));
    }
    if !flags.grants.is_empty() || !flags.program.is_empty() {
        return Err(CliError::Usage("--grant and the words after `--` are `blaze run`'s"));
    }
    if flags.status_every.is_some() {
        return Err(CliError::Usage("--status-every is `blaze check`'s"));
    }
    if flags.open == blaze_run::viewer::OpenExplore::Open {
        return Err(CliError::Usage("--open is `blaze run`'s"));
    }
    Ok(())
}

/// Target format selected by `blaze build [--target ...]`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum BuildTarget {
    Native,
    Wasm64,
}

fn parse_build_target(flag: Option<&str>) -> Result<BuildTarget, CliError> {
    match flag {
        None | Some("native") => Ok(BuildTarget::Native),
        Some("wasm64" | "wasm64-blaze-wasip1") => Ok(BuildTarget::Wasm64),
        Some(_) => Err(CliError::Usage("--target is `native`, `wasm64`, or `wasm64-blaze-wasip1`")),
    }
}

/// Top-level `blaze export` command entry point, dispatched from `main.rs`.
#[expect(clippy::too_many_arguments, reason = "top-level command entry: each argument one capability extracted at main")]
pub(crate) fn command(
    cx: &Cx,
    args: &bstd::args::Args,
    fs: &Fs,
    env: Option<&bstd::env::Env>,
    net: Option<&bstd::net::Net>,
    host: &bstd::host::Host,
    clock: &bstd::clock::Clock,
    rng: &mut bstd::rng::Rng,
    out: &Out,
    err: &Out,
    phases: &mut Phases,
    heap: &'static blaze_own::Heap,
) -> Result<Status, CliError> {
    let mut packages = Vec::new_in(cx.frame());
    let flags = crate::parse_flags(args, |name| packages.push(name))?;
    check_export_flags(&flags)?;
    let target = parse_build_target(flags.target)?;
    let select = run_package::Select { test_packages: packages.as_slice(), ..flags.select };
    let started_ns = phases.now();
    let stats = flags.stats;
    let output_flag = flags.output;
    let view = flags.view.unwrap_or(blaze_report::frame::plain::View::Person);
    let line = crate::run_line::LineOpts::from_env(env, err, flags.color, blaze_screen::Command::Run, view, false);
    let line = crate::run_line::LineOpts { live: false, ..line };
    let lined = crate::run_line::watched(cx, err, clock, line, |watch, _bg| {
        watch.see(blaze_screen::Step::Began(blaze_screen::Stage::Checking));
        check_for_start(cx, Some(args), fs, env, net, host, rng, err, err, phases, heap, &flags, true, crate::engine_check::StartScope::Bin(select), None, |standing, phases, _rng| {
            let (root, frame) = (standing.checkout, standing.frame);
            let select = run_package::Select { member: standing.member, ..select };
            let picked = match standing.picked {
                Some(picked) => picked,
                None => run_package::pick(cx, root, select, frame)?,
            };
            let summary = export_program(cx, standing, &picked, fs, output_flag, target, started_ns, stats, phases, watch)?;
            write_start_profile(cx, root, phases, frame)?;
            let wall_ns = phases.since(started_ns);
            print_summary(cx, &summary, wall_ns, stats, out, err, frame)?;
            Ok(Status::OK)
        })
    });
    lined.map_err(CliError::RunLine)?
}

/// Maximum number of entries retained in each per-worker and merged top-N compile profile table
/// (Rule 18: strictly bounded memory).
const TOP_FN_LIMIT: usize = 10;

/// Wall-clock phase durations (in nanoseconds) recorded across one `blaze build` / `blaze export`.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
struct ExportPhaseTimes {
    front_ns: u64,
    walk_ns: u64,
    lto_ns: u64,
    link_ns: u64,
}

/// Borrowed comparison key shared by [`FnCompileStat`] and [`blaze_ladder::run::make_one::MakeSizes`]
/// so fast-path rejection before allocation and bounded insertion use one ordering (Rule 30).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FnCompileKey<'a> {
    name: &'a str,
    lower_ns: u64,
    build_ns: u64,
    emit_ns: u64,
    insts: u64,
    blocks: u64,
    frame_estimate: u64,
    entry_bytes: u64,
}

impl<'a> FnCompileKey<'a> {
    fn of_sizes(sizes: &'a blaze_ladder::run::make_one::MakeSizes) -> Self {
        Self {
            name: &sizes.function.path,
            lower_ns: u64::try_from(sizes.lower.as_nanos()).unwrap_or(u64::MAX),
            build_ns: u64::try_from(sizes.build.as_nanos()).unwrap_or(u64::MAX),
            emit_ns: u64::try_from(sizes.emit.as_nanos()).unwrap_or(u64::MAX),
            insts: sizes.insts,
            blocks: sizes.blocks,
            frame_estimate: sizes.frame_estimate,
            entry_bytes: sizes.entry_bytes,
        }
    }

    #[inline]
    const fn total_ns(&self) -> u64 {
        self.lower_ns.saturating_add(self.build_ns).saturating_add(self.emit_ns)
    }
}

/// Deterministic top-N ordering for slowest compiled functions: total compile time descending,
/// breaking ties deterministically by function name ascending then size/phase fields (Rule 25).
fn cmp_slowest_key(a: &FnCompileKey<'_>, b: &FnCompileKey<'_>) -> core::cmp::Ordering {
    b.total_ns()
        .cmp(&a.total_ns())
        .then_with(|| a.name.cmp(b.name))
        .then_with(|| b.insts.cmp(&a.insts))
        .then_with(|| b.frame_estimate.cmp(&a.frame_estimate))
        .then_with(|| b.blocks.cmp(&a.blocks))
        .then_with(|| b.entry_bytes.cmp(&a.entry_bytes))
        .then_with(|| b.lower_ns.cmp(&a.lower_ns))
        .then_with(|| b.build_ns.cmp(&a.build_ns))
        .then_with(|| b.emit_ns.cmp(&a.emit_ns))
}

/// Deterministic top-N ordering for largest compiled functions: LIR `insts` descending, then
/// `frame_estimate` descending, breaking ties deterministically by function name ascending then
/// remaining fields (Rule 25).
fn cmp_largest_key(a: &FnCompileKey<'_>, b: &FnCompileKey<'_>) -> core::cmp::Ordering {
    b.insts
        .cmp(&a.insts)
        .then_with(|| b.frame_estimate.cmp(&a.frame_estimate))
        .then_with(|| a.name.cmp(b.name))
        .then_with(|| b.blocks.cmp(&a.blocks))
        .then_with(|| b.entry_bytes.cmp(&a.entry_bytes))
        .then_with(|| b.total_ns().cmp(&a.total_ns()))
        .then_with(|| b.lower_ns.cmp(&a.lower_ns))
        .then_with(|| b.build_ns.cmp(&a.build_ns))
        .then_with(|| b.emit_ns.cmp(&a.emit_ns))
}

/// One compiled function's bounded profiling sample held in a worker or coordinator top-10 table.
#[derive(Clone, Debug, PartialEq, Eq)]
struct FnCompileStat {
    name: String<SharedAlloc<'static>>,
    lower_ns: u64,
    build_ns: u64,
    emit_ns: u64,
    insts: u64,
    blocks: u64,
    frame_estimate: u64,
    entry_bytes: u64,
}

impl FnCompileStat {
    fn from_key(key: FnCompileKey<'_>, alloc: SharedAlloc<'static>) -> Self {
        let mut name = String::with_capacity_in(key.name.len(), alloc);
        name.push_str(key.name);
        Self {
            name,
            lower_ns: key.lower_ns,
            build_ns: key.build_ns,
            emit_ns: key.emit_ns,
            insts: key.insts,
            blocks: key.blocks,
            frame_estimate: key.frame_estimate,
            entry_bytes: key.entry_bytes,
        }
    }

    fn key(&self) -> FnCompileKey<'_> {
        FnCompileKey {
            name: self.name.as_str(),
            lower_ns: self.lower_ns,
            build_ns: self.build_ns,
            emit_ns: self.emit_ns,
            insts: self.insts,
            blocks: self.blocks,
            frame_estimate: self.frame_estimate,
            entry_bytes: self.entry_bytes,
        }
    }
}

fn would_accept_key(
    list: &[FnCompileStat],
    key: &FnCompileKey<'_>,
    cmp: fn(&FnCompileKey<'_>, &FnCompileKey<'_>) -> core::cmp::Ordering,
) -> bool {
    if list.len() < TOP_FN_LIMIT {
        return true;
    }
    list.last().is_some_and(|last| cmp(key, &last.key()) == core::cmp::Ordering::Less)
}

fn insert_bounded(
    list: &mut Vec<FnCompileStat, SharedAlloc<'static>>,
    entry: FnCompileStat,
    cmp: fn(&FnCompileKey<'_>, &FnCompileKey<'_>) -> core::cmp::Ordering,
) {
    let key = entry.key();
    if !would_accept_key(list.as_slice(), &key, cmp) {
        return;
    }
    let pos = list.binary_search_by(|existing| cmp(&existing.key(), &key)).unwrap_or_else(|i| i);
    if pos >= TOP_FN_LIMIT {
        return;
    }
    if list.len() == TOP_FN_LIMIT {
        list.pop();
    }
    list.insert_shift(pos, entry);
}

/// Lock-free per-worker (and post-join merged coordinator) bounded top-10 slowest and largest
/// compiled function tables collected when `--stats` is active.
#[derive(Debug)]
struct CompileTopStats {
    slowest: Vec<FnCompileStat, SharedAlloc<'static>>,
    largest: Vec<FnCompileStat, SharedAlloc<'static>>,
    alloc: SharedAlloc<'static>,
}

impl CompileTopStats {
    fn new_in(alloc: SharedAlloc<'static>) -> Self {
        Self {
            slowest: Vec::with_capacity_in(TOP_FN_LIMIT, alloc),
            largest: Vec::with_capacity_in(TOP_FN_LIMIT, alloc),
            alloc,
        }
    }

    fn record_sizes(&mut self, sizes: &blaze_ladder::run::make_one::MakeSizes) {
        self.record_key(FnCompileKey::of_sizes(sizes));
    }

    fn record_key(&mut self, key: FnCompileKey<'_>) {
        let enter_slow = would_accept_key(self.slowest.as_slice(), &key, cmp_slowest_key);
        let enter_large = would_accept_key(self.largest.as_slice(), &key, cmp_largest_key);
        if !enter_slow && !enter_large {
            return;
        }
        let stat = FnCompileStat::from_key(key, self.alloc);
        match (enter_slow, enter_large) {
            (true, true) => {
                let dup = stat.clone();
                insert_bounded(&mut self.slowest, stat, cmp_slowest_key);
                insert_bounded(&mut self.largest, dup, cmp_largest_key);
            }
            (true, false) => insert_bounded(&mut self.slowest, stat, cmp_slowest_key),
            (false, true) => insert_bounded(&mut self.largest, stat, cmp_largest_key),
            (false, false) => {}
        }
    }

    /// Deterministically merges another worker's bounded top-10 tables into `self`.
    fn merge(&mut self, other: CompileTopStats) {
        for entry in other.slowest {
            insert_bounded(&mut self.slowest, entry, cmp_slowest_key);
        }
        for entry in other.largest {
            insert_bounded(&mut self.largest, entry, cmp_largest_key);
        }
    }
}

/// Summary metrics collected during one `blaze export`.
struct ExportSummary<'f> {
    output: &'f str,
    exported: Exported,
    data_count: usize,
    tls_count: usize,
    compiled: usize,
    cached: usize,
    unkeyed: usize,
    stubs: usize,
    lto_bodies: usize,
    lto_inlined: usize,
    lto_hot: usize,
    phase_times: ExportPhaseTimes,
    shared_front_stats: SharedLazyFrontStats,
    top_stats: Option<CompileTopStats>,
}

/// How one walked row's entry bytes are held until assembly.
enum RowOutcome<'a> {
    Keyed(CodeKey),
    Unkeyed(AVec<'a, u8>),
    /// Whole-program cross-function LTO-optimized `CodeRecord` emitted for the host CPU
    /// (`Machine::SME2 | Machine::CSSC | Machine::SHA3`), kept ephemeral in `export_bump` so
    /// host-specific code never enters portable `HomeCodeStore` (Rule 25).
    Lto {
        record: Vec<u8, SharedAlloc<'static>>,
        vtable_data: Vec<(CodeSymbol, Vec<u8, SharedAlloc<'static>>), SharedAlloc<'static>>,
    },
    /// A non-root library/cold function that `blaze-build` cannot lower to LIR yet: replaced by a
    /// deterministic trap stub (`write(2, msg, len); _exit(125); brk #0`) that reports the exact
    /// refusal if ever called at runtime (same contract as `blaze-ladder/src/run.rs` `Stubs`).
    UnbuiltStub(AVec<'a, u8>),
}

/// Maximum message length embedded in an unbuilt-function trap stub (fits in AArch64 `movz w2`).
const MAX_STUB_MSG: usize = 512;

/// Builds a canonical position-free `CodeRecord` for an unbuilt cold callee that writes `message`
/// to stderr (fd 2) and terminates with exit status 125 (`program::NOT_RUN`).
fn make_unbuilt_stub_record<'a>(
    symbol: CodeSymbol,
    message: &str,
    frame: &Frame,
    alloc: Alloc<'a>,
    heap: &'static blaze_own::Heap,
) -> Result<AVec<'a, u8>, ExportCliError> {
    let mut msg_len = message.len().min(MAX_STUB_MSG);
    while msg_len > 0 && !message.is_char_boundary(msg_len) {
        msg_len -= 1;
    }
    let slice = message.get(..msg_len).unwrap_or("");
    let mut rec = CodeRecord::new_in(alloc);
    rec.relro.extend_from_slice(slice.as_bytes());
    if !slice.ends_with('\n') {
        rec.relro.push(b'\n');
    }
    let len_u32 = u32::try_from(rec.relro.len()).unwrap_or(0);
    let mov_w2_len = 0x5280_0002u32 | ((len_u32 & 0xffff) << 5);
    let exit_imm = u32::from(program::NOT_RUN);
    let mov_w0_exit = 0x5280_0000u32 | ((exit_imm & 0xffff) << 5);
    let insts: [u32; 14] = [
        0xA9BF_7BFD, // 0:  stp x29, x30, [sp, #-16]!
        0x9100_03FD, // 4:  mov x29, sp
        0x5280_0040, // 8:  mov w0, #2 (stderr)
        0x9000_0001, // 12: adrp x1, relro
        0x9100_0021, // 16: add x1, x1, #0
        mov_w2_len,  // 20: movz w2, #len
        0x9000_0010, // 24: adrp x16, write
        0xF940_0210, // 28: ldr x16, [x16, #0]
        0xD63F_0200, // 32: blr x16
        mov_w0_exit, // 36: mov w0, #125 (NOT_RUN)
        0x9000_0010, // 40: adrp x16, _exit
        0xF940_0210, // 44: ldr x16, [x16, #0]
        0xD63F_0200, // 48: blr x16
        0xD420_0000, // 52: brk #0
    ];
    for inst in insts {
        rec.code.extend_from_slice(&inst.to_le_bytes());
    }
    let write_idx = rec.outside("write");
    let exit_idx = rec.outside("_exit");
    rec.sites.push(Site { at: 12, kind: SiteKind::Relro, target: NO_TARGET, addend: 0 });
    rec.sites.push(Site { at: 24, kind: SiteKind::Call, target: write_idx, addend: 0 });
    rec.sites.push(Site { at: 40, kind: SiteKind::Call, target: exit_idx, addend: 0 });
    let sealed = rec.seal().map_err(|why| ExportCliError::Materialize {
        symbol: symbol.0,
        shown: owned_fmt(format_args!("unbuilt stub seal: {why}"), frame, heap.alloc()),
    })?;
    let mut out = Vec::new_in(alloc);
    sealed.encode(&mut out);
    Ok(out)
}

/// Maps `blaze_ladder::run::special::Roots` to `blaze_export::Entry`.
fn entry_of_roots(roots: &Roots<'_>) -> Entry {
    if let Some(shim) = roots.shim() {
        return Entry::LangStart { lang_start: shim.lang_start.symbol, main: shim.main.symbol };
    }
    match roots.form {
        EntryForm::Unit => Entry::RustMain { main: roots.entry },
        EntryForm::CMain | EntryForm::Comptime => Entry::CMain { main: roots.entry },
        EntryForm::Hosted => Entry::Hosted { start: roots.entry },
    }
}

/// Converts a `blaze_code::datum::DatumView` into a `blaze_export::Data`, allocating its
/// `DataWord` slice in `bump`.
fn datum_to_export<'a>(target: Target, view: &DatumView<'a>, bump: Bump<'a>, heap: &'static blaze_own::Heap) -> Result<Data<'a>, ExportCliError> {
    let place = match view.place() {
        DatumPlace::ReadOnly => DataPlace::ReadOnly,
        DatumPlace::Writable => DataPlace::Writable,
        DatumPlace::Zeroed { len } => DataPlace::Zeroed { len },
    };
    let align = u32::try_from(view.align()).map_err(|_| ExportCliError::Decode {
        symbol: target.symbol.0,
        what: "datum alignment",
        shown: owned_str("datum alignment exceeds u32", heap.alloc()),
    })?;
    let word_count = view.words();
    let mut words: Vec<DataWord, Bump<'a>> = Vec::with_capacity_in(word_count, bump);
    let mut outside: Vec<(CodeSymbol, &'a str), Bump<'a>> = Vec::new_in(bump);
    for w in 0..word_count {
        let word = view.word(w);
        let word_target = if word.target == OWN_DATUM {
            None
        } else {
            let idx = usize::try_from(word.target).map_err(|_| ExportCliError::Decode {
                symbol: target.symbol.0,
                what: "datum word target index",
                shown: owned_str("word target index exceeds usize", heap.alloc()),
            })?;
            let t = view.target(idx);
            if t.space.is_outside()
                && let Some(name) = view.name(t.symbol)
                && !outside.iter().any(|(s, _)| *s == t.symbol)
            {
                outside.push((t.symbol, name));
            }
            Some(t)
        };
        words.push(DataWord { at: word.at, target: word_target, addend: word.addend });
    }
    Ok(Data {
        target,
        bytes: view.bytes(),
        align,
        words: words.into_bump_slice(),
        outside: outside.into_bump_slice(),
        place,
    })
}

/// Materializes a `CodeView` into a position-free `CodeView`. When `view.positions() == 0`,
/// returns `view` directly with zero copies. Otherwise resolves every item-relative position row
/// against `source` and encodes a fresh canonical position-free `CodeRecord` in `bump`.
fn materialize_positionless<'a>(
    symbol: CodeSymbol,
    view: CodeView<'a>,
    mut source: impl FnMut(ItemKey, u32) -> Result<(u32, u32), blaze_ladder::seam::PositionGap>,
    frame: &Frame,
    bump: Bump<'a>,
    heap: &'static blaze_own::Heap,
) -> Result<CodeView<'a>, ExportCliError> {
    let pos_count = view.positions();
    if pos_count == 0 {
        return Ok(view);
    }
    let alloc = bump.alloc();
    let mut rec = CodeRecord::new_in(alloc);
    rec.code.extend_from_slice(view.code());
    rec.relro.extend_from_slice(view.relro());
    rec.frame = view.frame();
    rec.code_align = view.code_align();
    rec.relro_align = view.relro_align();

    let mut previous: Option<((ItemKey, u32), (u32, u32))> = None;
    for i in 0..pos_count {
        let row = view.position(i);
        let site = (ItemKey(row.item), row.site);
        let answer = match previous {
            Some((last, answer)) if last == site => answer,
            _ => {
                let answer = source(site.0, site.1).map_err(|why| ExportCliError::Position {
                    symbol: symbol.0,
                    shown: owned_fmt(format_args!("{row:?}: {why}"), frame, heap.alloc()),
                })?;
                previous = Some((site, answer));
                answer
            }
        };
        let resolved = row.field.value(answer);
        if resolved == 0 {
            return Err(ExportCliError::Position {
                symbol: symbol.0,
                shown: owned_fmt(format_args!("{row:?}: resolved to 0"), frame, heap.alloc()),
            });
        }
        let dst = match row.part {
            PositionPart::Code => rec.code.as_mut_slice(),
            PositionPart::Relro => rec.relro.as_mut_slice(),
        };
        let start = usize::try_from(row.at).map_err(|_| ExportCliError::Position {
            symbol: symbol.0,
            shown: owned_str("position offset exceeds usize", heap.alloc()),
        })?;
        let Some(end) = start.checked_add(4) else {
            return Err(ExportCliError::Position {
                symbol: symbol.0,
                shown: owned_str("position offset overflow", heap.alloc()),
            });
        };
        let Some(slot) = dst.get_mut(start..end) else {
            return Err(ExportCliError::Position {
                symbol: symbol.0,
                shown: owned_fmt(format_args!("position offset {start} out of bounds"), frame, heap.alloc()),
            });
        };
        slot.copy_from_slice(&resolved.to_le_bytes());
    }

    for i in 0..view.targets() {
        let t = view.target(i);
        if t.space.is_outside() {
            let Some(name) = view.name(t.symbol) else {
                return Err(ExportCliError::Materialize {
                    symbol: symbol.0,
                    shown: owned_fmt(format_args!("missing outside name for {}", t.symbol), frame, heap.alloc()),
                });
            };
            if t.space == Space::WeakOutside {
                rec.weak_outside(name);
            } else {
                rec.outside(name);
            }
        } else {
            rec.target(t);
        }
    }
    for i in 0..view.sites() {
        rec.sites.push(view.site(i));
    }
    for i in 0..view.words() {
        rec.words.push(view.word(i));
    }
    for i in 0..view.pads() {
        rec.pads.push(view.pad(i));
    }
    for i in 0..view.traps() {
        rec.traps.push(view.trap(i));
    }
    let sealed = rec.seal().map_err(|why| ExportCliError::Materialize {
        symbol: symbol.0,
        shown: owned_fmt(format_args!("seal: {why}"), frame, heap.alloc()),
    })?;
    let mut encoded: AVec<'a, u8> = Vec::new_in(alloc);
    sealed.encode(&mut encoded);
    let bytes: &'a [u8] = bump.alloc_slice_copy(encoded.as_slice());
    CodeView::read(bytes).map_err(|why| ExportCliError::Materialize {
        symbol: symbol.0,
        shown: owned_fmt(format_args!("read back: {why}"), frame, heap.alloc()),
    })
}

/// Decodes `datum_bytes` and appends it to `data` if `target` has not been seen yet.
#[expect(clippy::too_many_arguments, reason = "datum deduplication helper over caller-owned vectors and allocators")]
fn push_datum<'a>(
    data: &mut AVec<'a, Data<'a>>,
    seen_data: &mut AVec<'a, Target>,
    tls_count: &mut usize,
    target: Target,
    datum_bytes: &'a [u8],
    frame: &Frame,
    bump: Bump<'a>,
    heap: &'static blaze_own::Heap,
) -> Result<(), CliError> {
    if let Err(pos) = seen_data.binary_search(&target) {
        let view = DatumView::read(datum_bytes)
            .map_err(|why| box_err(ExportCliError::Decode { symbol: target.symbol.0, what: "datum record", shown: owned_fmt(format_args!("{why}"), frame, heap.alloc()) }, heap))?;
        let datum = datum_to_export(target, &view, bump, heap).map_err(|e| box_err(e, heap))?;
        seen_data.insert_shift(pos, target);
        data.push(datum);
        if target.space == Space::Tls {
            *tls_count += 1;
        }
    }
    Ok(())
}

/// Checks whether a `CodeView` references any Rust thread-local symbol (`Space::Tls`,
/// `tls_getter_symbol`, or `tls_desc_symbol`).
fn code_touches_tls(view: &CodeView<'_>, getter: CodeSymbol, descriptor: CodeSymbol) -> bool {
    for i in 0..view.targets() {
        let t = view.target(i);
        match t.space {
            Space::Tls => return true,
            Space::Func if t.symbol == getter => return true,
            Space::Static if t.symbol == descriptor => return true,
            _ => {}
        }
    }
    false
}

/// Virtual reservation for each parallel export LTO worker's reusable scratch frame (reset after
/// every body, trimmed when committed exceeds [`EXPORT_LTO_WORKER_TRIM`]). Untouched virtual pages
/// cost zero RSS; 2 GiB avoids exhausting bump space on large post-inline functions.
const EXPORT_LTO_WORKER_FRAME: usize = 2 << 30;
const EXPORT_LTO_WORKER_KEEP: usize = 4 << 20;
const EXPORT_LTO_WORKER_TRIM: usize = 32 << 20;
/// Minimum batch count to fan out export LTO work across worker threads.
const EXPORT_LTO_PAR_MIN: usize = 4;

/// Chooses the dynamic work-stealing batch size for `count` items across `worker_count` workers.
#[inline]
const fn lto_batch_size(count: usize, worker_count: usize) -> usize {
    if count >= worker_count.saturating_mul(16) { 4 } else { 1 }
}

/// Atomically claims the next `[start, end)` batch of indices from `next`, returning `None` once
/// all `count` items have been claimed.
#[inline]
fn steal_next_batch(
    next: &bstd::sync::CachePadded<core::sync::atomic::AtomicUsize>,
    batch: usize,
    count: usize,
) -> Option<(usize, usize)> {
    let start = next.fetch_add(batch, core::sync::atomic::Ordering::Relaxed);
    if start >= count {
        return None;
    }
    Some((start, start.saturating_add(batch).min(count)))
}

/// Validates and merges out-of-order `(index, T)` worker outputs into exact ascending `0..count`
/// order (Rule 25), refusing any worker/scope failure, out-of-bounds index, duplicate index, or
/// missing index (Rule 3).
fn collect_lto_worker_parts<T>(
    count: usize,
    parts: Vec<Result<Vec<(usize, T), SharedAlloc<'static>>, &'static str>, SharedAlloc<'static>>,
    scope_failed: bool,
    out_alloc: SharedAlloc<'static>,
) -> Result<Vec<T, SharedAlloc<'static>>, blaze_ladder::run::make_one::export_lto::ExportLtoSimplifyError> {
    use blaze_ladder::run::make_one::export_lto::ExportLtoSimplifyError;

    let mut slots: Vec<Option<T>, SharedAlloc<'static>> = Vec::with_capacity_in(count, out_alloc);
    for _ in 0..count {
        slots.push(None);
    }
    let mut err_what: Option<&'static str> = None;
    for part_res in parts {
        match part_res {
            Ok(part) => {
                for (idx, item) in part {
                    let Some(slot) = slots.get_mut(idx) else {
                        err_what.get_or_insert("export LTO worker index out of range");
                        continue;
                    };
                    if slot.is_some() {
                        err_what.get_or_insert("export LTO worker duplicated index");
                        continue;
                    }
                    *slot = Some(item);
                }
            }
            Err(what) => {
                err_what.get_or_insert(what);
            }
        }
    }
    if scope_failed {
        err_what.get_or_insert("export LTO worker scope join failed");
    }
    if let Some(what) = err_what {
        return Err(ExportLtoSimplifyError::Refused { inst: 0, what });
    }
    let mut out = Vec::with_capacity_in(count, out_alloc);
    for slot in slots {
        let Some(item) = slot else {
            return Err(ExportLtoSimplifyError::Refused { inst: 0, what: "export LTO worker missed index" });
        };
        out.push(item);
    }
    Ok(out)
}

/// `Cx::scope`-backed parallel executor for `optimize_export_workspace_par`: dynamically steals
/// work across `workers` with one reusable bump [`Frame`] per worker and returns results in exact
/// ascending index order (Rule 25).
struct CliLtoRunner<'c> {
    cx: &'c Cx,
    workers: core::num::NonZeroU32,
}

impl blaze_ladder::run::make_one::export_lto::ExportLtoRunner for CliLtoRunner<'_> {
    fn par_map<T: Send, F>(
        &self,
        count: usize,
        out_alloc: SharedAlloc<'static>,
        f: F,
    ) -> Result<Vec<T, SharedAlloc<'static>>, blaze_ladder::run::make_one::export_lto::ExportLtoSimplifyError>
    where
        F: Fn(usize, Alloc<'_>) -> T + Sync,
    {
        use blaze_ladder::run::make_one::export_lto::ExportLtoSimplifyError;

        let mut out = Vec::with_capacity_in(count, out_alloc);
        if count == 0 {
            return Ok(out);
        }
        let active_workers = self.workers.get().min(self.cx.workers());
        if count < EXPORT_LTO_PAR_MIN || active_workers <= 1 {
            let mut scratch = Frame::with_reserve(EXPORT_LTO_WORKER_FRAME, bstd::mem::Tag::Other)
                .map_err(|_| ExportLtoSimplifyError::Refused { inst: 0, what: "export LTO scratch frame reserve failed" })?;
            for idx in 0..count {
                if self.cx.check().is_err() {
                    return Err(ExportLtoSimplifyError::Refused { inst: 0, what: "export LTO worker stopped" });
                }
                scratch.reset();
                if scratch.committed() > EXPORT_LTO_WORKER_TRIM {
                    scratch.trim(EXPORT_LTO_WORKER_KEEP);
                }
                let erased = Erased::new(&scratch);
                out.push(f(idx, erased.handle()));
            }
            return Ok(out);
        }

        let worker_count = usize::try_from(active_workers).unwrap_or(1).min(count);
        let batch = lto_batch_size(count, worker_count);
        let next = bstd::sync::CachePadded::new(core::sync::atomic::AtomicUsize::new(0));
        let next_ref = &next;
        let f_ref = &f;
        let scope = self.cx.scope();
        scope.narrow_workers(self.workers, bstd::task::Class::Foreground);
        let mut handles = Vec::with_capacity_in(worker_count, out_alloc);
        let est_per_worker = (count / worker_count).saturating_add(batch);
        for _ in 0..worker_count {
            handles.push(scope.may_overlap(
                "export: LTO worker",
                4 << 20,
                move |worker_cx: &Cx| -> Result<_, core::convert::Infallible> {
                    let mut scratch = match Frame::with_reserve(EXPORT_LTO_WORKER_FRAME, bstd::mem::Tag::Other) {
                        Ok(frame) => frame,
                        Err(_) => return Ok(Err("export LTO worker scratch frame reserve failed")),
                    };
                    let mut local: Vec<(usize, T), SharedAlloc<'static>> = Vec::with_capacity_in(est_per_worker, out_alloc);
                    while let Some((start, end)) = steal_next_batch(next_ref, batch, count) {
                        if worker_cx.check().is_err() {
                            return Ok(Err("export LTO worker stopped"));
                        }
                        for idx in start..end {
                            scratch.reset();
                            if scratch.committed() > EXPORT_LTO_WORKER_TRIM {
                                scratch.trim(EXPORT_LTO_WORKER_KEEP);
                            }
                            let erased = Erased::new(&scratch);
                            local.push((idx, f_ref(idx, erased.handle())));
                        }
                    }
                    Ok(Ok(local))
                },
            ));
        }

        let mut parts: Vec<Result<Vec<(usize, T), SharedAlloc<'static>>, &'static str>, SharedAlloc<'static>> =
            Vec::with_capacity_in(handles.len(), out_alloc);
        for handle in handles {
            match handle.join(self.cx) {
                Ok(Ok(part)) => parts.push(Ok(part)),
                Ok(Err(what)) => parts.push(Err(what)),
                Err(_) => parts.push(Err("export LTO worker task panicked or stopped")),
            }
        }
        let scope_failed = scope.join(self.cx).is_err();
        collect_lto_worker_parts(count, parts, scope_failed, out_alloc)
    }
}

struct ExportJob {
    row: usize,
    order: blaze_ladder::body_ids::Order,
    proof: crate::run_ids::Runnable,
    name: Vec<u8, SharedAlloc<'static>>,
    enclosers: crate::test_drive::suite_cache::LocalEnclosers,
}

enum ExportWorkerErr {
    ScopeOne(String<SharedAlloc<'static>>),
    Fatal(String<SharedAlloc<'static>>),
}

struct ExportDone {
    worker: usize,
    row: usize,
    result: Result<Vec<u8, SharedAlloc<'static>>, ExportWorkerErr>,
    typed: Option<crate::engine_body_record::WorkerTypedBatch>,
}

type ExportProcsAnswer = Result<&'static crate::proc_runner::ProcRunner<'static>, String<SharedAlloc<'static>>>;

struct ExportWorkerProcs<'c, 'f> {
    worker: usize,
    cx: &'c Cx,
    clock: &'c bstd::clock::Clock,
    frame: &'f Frame,
    procs_asked: &'c bstd::sync::chan::OwnedChan<usize, SharedAlloc<'static>>,
    procs_answer: &'c bstd::sync::chan::OwnedChan<ExportProcsAnswer, SharedAlloc<'static>>,
    got: core::cell::OnceCell<crate::job_texts::MadeProcs<'f>>,
}

impl<'f> crate::job_texts::ProcsByNeed<'f> for ExportWorkerProcs<'_, 'f> {
    fn procs(&self) -> crate::job_texts::MadeProcs<'f> {
        *self.got.get_or_init(|| {
            let by = self.cx.until(bstd::Deadline::after(self.clock.now().since_start(), EXPORT_PROCS_WAIT));
            let waiting = by.span("blaze build: wait for proc-macro runner");
            if self.procs_asked.send(&waiting, self.worker).is_err() {
                return Err("no proc-macro runner: export worker channel closed before request was taken (engine_export.rs)");
            }
            match self.procs_answer.recv(&waiting) {
                Ok(Ok(runner)) => Ok(runner),
                Ok(Err(why)) => Err(self.frame.alloc_str(why.as_str())),
                Err(_) => Err("no proc-macro runner: export worker channel closed or timed out awaiting answer (engine_export.rs)"),
            }
        })
    }
}

fn export_err_string(why: fmt::Arguments<'_>, alloc: SharedAlloc<'static>) -> String<SharedAlloc<'static>> {
    use core::fmt::Write as _;
    let mut s = String::new_in(alloc);
    let _ = write!(s, "{why}");
    s
}

enum ExportEpochEnd {
    Done,
    Reset(Option<ExportJob>),
}

/// `Typing` adapter used by both parallel export workers and the coordinator `Maker`:
/// 1. Attaches the command-scoped [`SharedLazyFront`] to the worker's [`RecordsProgram`] on the
///    first body check (zero changes to `make_one.rs`, zero thread-locals / ambient state).
/// 2. Shares freshly typechecked dependency/toolchain `Checked` records via
///    [`SharedLazyFront::get_typed_bytes`] / [`SharedLazyFront::put_typed_bytes`] so no dep or
///    toolchain body is typechecked more than once across 128 workers or worker epoch resets,
///    and deduplicates `WorkerTypedBatch` CAS puts sent to the coordinator.
struct SharedExportTyping<K> {
    pub kept: K,
    shared: SharedLazyFrontHandle,
    alloc: Alloc<'static>,
    counts: TypingCounts,
}

impl<K> SharedExportTyping<K> {
    fn new(kept: K, shared: &SharedLazyFrontHandle, alloc: Alloc<'static>) -> Self {
        Self { kept, shared: std::sync::Arc::clone(shared), alloc, counts: TypingCounts::default() }
    }
}

struct OneTyped {
    key: ItemKey,
    got: Option<Result<blaze_typeck::Checked<'static>, blaze_ladder::front2::program::lazy::typed::TypedGap>>,
}

impl blaze_ladder::front2::program::lazy::typed::TypedRecords for OneTyped {
    fn typed(
        &mut self,
        key: ItemKey,
    ) -> Result<blaze_typeck::Checked<'static>, blaze_ladder::front2::program::lazy::typed::TypedGap> {
        use blaze_ladder::front2::program::lazy::typed::TypedGap;
        if key != self.key {
            return Err(TypedGap::NoBody { key });
        }
        self.got.take().unwrap_or(Err(TypedGap::NoRecord { key }))
    }
}

impl<'n, K: blaze_ladder::front2::program::lazy::typed::KeptBodies> Typing<RecordsProgram<'n>>
    for SharedExportTyping<K>
{
    fn check_one(
        &mut self,
        program: &mut RecordsProgram<'n>,
        key: ItemKey,
    ) -> Result<Option<(usize, blaze_typeck::Checked<'static>)>, RunError> {
        use blaze_ladder::front2::program::lazy::typed::{BodySource, RunChecked, is_local};
        use blaze_ladder::seam::FrontEnd as _;
        program.attach_shared(&self.shared);
        if !is_local(key)
            && let Some(bytes) = self.shared.get_typed_bytes(key)
            && let Ok(checked) = blaze_typeck::checked_record::decode(bytes.as_slice(), self.alloc)
        {
            let made = program.check_one_typed(key, &mut OneTyped { key, got: Some(Ok(checked)) })?;
            if made.is_some() {
                self.counts.read += 1;
            }
            return Ok(made);
        }
        match self.kept.source(key) {
            Ok(BodySource::Run(kind)) => {
                let made = program.check_one(key)?;
                if let Some((_, ref checked)) = made {
                    self.counts.checked[kind.index()] += 1;
                    let mut should_persist = true;
                    if !is_local(key)
                        && matches!(kind, RunChecked::Dependency | RunChecked::Toolchain)
                        && let Ok(encoded) = blaze_typeck::checked_record::encode(checked, self.alloc)
                    {
                        should_persist = self.shared.put_typed_bytes(key, encoded.as_slice());
                    }
                    if should_persist {
                        self.kept.keep_checked(key, kind, checked);
                    }
                }
                Ok(made)
            }
            Ok(BodySource::Read(checked)) => {
                let made = program.check_one_typed(key, &mut OneTyped { key, got: Some(Ok(checked)) })?;
                if made.is_some() {
                    self.counts.read += 1;
                }
                Ok(made)
            }
            Err(gap) => program.check_one_typed(key, &mut OneTyped { key, got: Some(Err(gap)) }),
        }
    }

    fn counts(&self) -> Option<TypingCounts> {
        Some(self.counts)
    }
}

/// Defers `maker.prepare_local(...)` during `Walk::begin` so the coordinator thread never lowers
/// enclosing bodies on cold misses that are immediately dispatched to workers, while still
/// preparing locals whenever `Walk::begin` hits a cached entry whose read list needs verification.
struct LazyVerifyMaker<'m, 'n, 's, 'e> {
    maker: &'m mut Maker<'n, crate::run_ids::Runnable, SharedExportTyping<crate::engine_body_record::StoreTyped<'n>>>,
    shared: &'s SharedLazyFrontHandle,
    enclosers: &'e crate::test_drive::suite_cache::LocalEnclosers,
    prepared: bool,
}

impl<'m, 'n, 's, 'e> LazyVerifyMaker<'m, 'n, 's, 'e> {
    fn ensure_prepared(&mut self) {
        if !self.prepared {
            self.prepared = true;
            if let Some(prog) = self.maker.program() {
                prog.attach_shared(self.shared);
            }
            self.maker.prepare_local(self.enclosers.locals(), self.enclosers.enclosers());
            if let Some(prog) = self.maker.program() {
                prog.attach_shared(self.shared);
            }
        }
    }
}

impl blaze_ladder::body_ids::Make for LazyVerifyMaker<'_, '_, '_, '_> {
    type Error = RunError;
    type Grant = crate::run_ids::Runnable;

    fn make(&mut self, grant: &Self::Grant, tabled: ItemKey, name: &[u8], entry: &mut AVec<'_, u8>) -> Result<(), Self::Error> {
        self.ensure_prepared();
        self.maker.make(grant, tabled, name, entry)
    }

    fn make_static(&mut self, grant: &Self::Grant, item: ItemKey, entry: &mut AVec<'_, u8>) -> Result<(), Self::Error> {
        self.ensure_prepared();
        self.maker.make_static(grant, item, entry)
    }

    fn make_thread_local(&mut self, grant: &Self::Grant, item: ItemKey, entry: &mut AVec<'_, u8>) -> Result<(), Self::Error> {
        self.ensure_prepared();
        self.maker.make_thread_local(grant, item, entry)
    }

    fn verify(&mut self, tabled: ItemKey, name: &[u8], entry: &[u8]) -> Result<Option<blaze_ladder::code_records::reads::Verdict>, Self::Error> {
        self.ensure_prepared();
        self.maker.verify(tabled, name, entry)
    }

    fn verify_keyed(&mut self, key: CodeKey, under: CodeKey, tabled: ItemKey, name: &[u8], entry: &[u8]) -> Result<Option<blaze_ladder::code_records::reads::Verdict>, Self::Error> {
        self.ensure_prepared();
        self.maker.verify_keyed(key, under, tabled, name, entry)
    }
}

/// Sans-IO affinity-first work-stealing scheduler for parallel export codegen (Rule 14).
///
/// - **Affinity fast path (LIFO per home deque)**: sibling monomorphizations of the same `tabled`
///   fn route to `home_of(tabled)` and pop LIFO from the back of its deque so the lowered body and
///   `MakerState` stay hot in that worker's epoch.
/// - **Zero-starvation work stealing (FIFO front-steal from deepest victim)**: whenever any worker
///   falls below `steal_low_water` while `queued > 0`, it steals from the **front** (oldest
///   unrelated `tabled` cluster) of the deepest non-empty queue, breaking ties deterministically by
///   lowest worker index (Rule 25). This prevents one giant function (`visit_seq`,
///   `emit_sota_a64_with_abi`) or one heavily monomorphized generic fn from stranding idle peers.
struct ExportScheduler<'a, J> {
    active_workers: usize,
    per_worker_cap: usize,
    steal_low_water: usize,
    worker_in_flight: AVec<'a, usize>,
    worker_queues: AVec<'a, VecDeque<(ItemKey, J), Alloc<'a>>>,
    in_flight: usize,
    queued: usize,
    steals: u64,
    #[cfg(test)]
    allow_steal: bool,
}

impl<'a, J> ExportScheduler<'a, J> {
    fn new_in(active_workers: usize, per_worker_cap: usize, steal_low_water: usize, alloc: Alloc<'a>) -> Self {
        let mut worker_in_flight = Vec::with_capacity_in(active_workers, alloc);
        worker_in_flight.resize(active_workers, 0);
        let mut worker_queues = Vec::with_capacity_in(active_workers, alloc);
        for _ in 0..active_workers {
            worker_queues.push(VecDeque::new_in(alloc));
        }
        Self {
            active_workers,
            per_worker_cap: per_worker_cap.max(1),
            steal_low_water: steal_low_water.clamp(1, per_worker_cap.max(1)),
            worker_in_flight,
            worker_queues,
            in_flight: 0,
            queued: 0,
            steals: 0,
            #[cfg(test)]
            allow_steal: true,
        }
    }

    #[cfg(test)]
    fn disable_stealing_for_test(&mut self) {
        self.allow_steal = false;
    }

    #[inline]
    fn stealing_enabled(&self) -> bool {
        #[cfg(test)]
        {
            self.allow_steal
        }
        #[cfg(not(test))]
        {
            true
        }
    }

    #[inline]
    fn home_of(&self, tabled: ItemKey) -> usize {
        let mixed = ((tabled.0 as u64) ^ ((tabled.0 >> 64) as u64)).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 32;
        (mixed as usize) % self.active_workers
    }

    #[inline]
    fn in_flight(&self) -> usize {
        self.in_flight
    }

    #[inline]
    fn queued(&self) -> usize {
        self.queued
    }

    #[inline]
    fn max_in_flight(&self) -> usize {
        self.active_workers.saturating_mul(self.per_worker_cap).max(1)
    }

    /// Enqueues `job` for `tabled`, returning an immediate `(worker, job)` dispatch when either
    /// `home` has capacity or an idle/underloaded peer can immediately steal work.
    fn enqueue(&mut self, tabled: ItemKey, job: J) -> Option<(usize, J)> {
        let home = self.home_of(tabled);
        if self.worker_in_flight[home] == 0 {
            self.worker_in_flight[home] = 1;
            self.in_flight += 1;
            return Some((home, job));
        }
        // Never queue a second job behind a busy worker (which may be compiling a giant function)
        // while any peer is completely idle.
        if self.stealing_enabled()
            && let Some(idle) = self.least_loaded_below(1)
        {
            self.worker_in_flight[idle] = 1;
            self.in_flight += 1;
            self.steals = self.steals.saturating_add(1);
            return Some((idle, job));
        }
        let target_cap = if self.stealing_enabled() && self.least_loaded_below(self.steal_low_water).is_some() {
            self.steal_low_water
        } else {
            self.per_worker_cap
        };
        if self.worker_in_flight[home] < target_cap {
            self.worker_in_flight[home] += 1;
            self.in_flight += 1;
            return Some((home, job));
        }
        self.worker_queues[home].push_back((tabled, job));
        self.queued += 1;
        if self.stealing_enabled() {
            if let Some(thief) = self.least_loaded_below(self.steal_low_water) {
                if let Some(stolen) = self.next_dispatch_for(thief) {
                    return Some((thief, stolen));
                }
            }
        }
        None
    }

    /// Records completion of one job on `worker`.
    fn on_complete(&mut self, worker: usize) {
        self.in_flight = self.in_flight.saturating_sub(1);
        if let Some(slot) = self.worker_in_flight.get_mut(worker) {
            *slot = slot.saturating_sub(1);
        }
    }

    /// Pops the next job to dispatch to `worker`: own deque LIFO first, then FIFO front-steal from
    /// the deepest non-empty peer queue when `worker_in_flight[worker] < steal_low_water`.
    fn next_dispatch_for(&mut self, worker: usize) -> Option<J> {
        if worker >= self.active_workers || self.worker_in_flight[worker] >= self.per_worker_cap {
            return None;
        }
        if let Some((_, job)) = self.worker_queues[worker].pop_back() {
            self.queued -= 1;
            self.worker_in_flight[worker] += 1;
            self.in_flight += 1;
            return Some(job);
        }
        if self.stealing_enabled() && self.worker_in_flight[worker] < self.steal_low_water && self.queued > 0 {
            let victim = self.deepest_queue()?;
            let (_, job) = self.worker_queues[victim].pop_front()?;
            self.queued -= 1;
            self.worker_in_flight[worker] += 1;
            self.in_flight += 1;
            self.steals = self.steals.saturating_add(1);
            return Some(job);
        }
        None
    }

    /// Selects the next `(worker, job)` pair to refill any starved worker (`< steal_low_water`)
    /// or any worker with a non-empty own queue below `per_worker_cap`.
    fn next_refill(&mut self) -> Option<(usize, J)> {
        if self.queued == 0 {
            return None;
        }
        if self.stealing_enabled()
            && let Some(starved) = self.least_loaded_below(self.steal_low_water)
            && let Some(job) = self.next_dispatch_for(starved)
        {
            return Some((starved, job));
        }
        for w in 0..self.active_workers {
            if !self.worker_queues[w].is_empty() && self.worker_in_flight[w] < self.per_worker_cap {
                if let Some(job) = self.next_dispatch_for(w) {
                    return Some((w, job));
                }
            }
        }
        None
    }

    fn least_loaded_below(&self, limit: usize) -> Option<usize> {
        let mut best: Option<(usize, usize)> = None;
        for (w, &load) in self.worker_in_flight.iter().enumerate() {
            if load < limit && best.is_none_or(|(_, best_load)| load < best_load) {
                best = Some((w, load));
                if load == 0 {
                    break;
                }
            }
        }
        best.map(|(w, _)| w)
    }

    fn deepest_queue(&self) -> Option<usize> {
        let mut best: Option<(usize, usize)> = None;
        for (w, q) in self.worker_queues.iter().enumerate() {
            let len = q.len();
            if len > 0 && best.is_none_or(|(_, best_len)| len > best_len) {
                best = Some((w, len));
            }
        }
        best.map(|(w, _)| w)
    }
}

#[expect(clippy::too_many_arguments, reason = "one worker frame epoch receives shared read-only program inputs and channels")]
fn run_export_worker_epoch<'f>(
    worker: usize,
    cx: &Cx,
    clock: &bstd::clock::Clock,
    root: &bstd::fs::Dir<bstd::fs::ReadWrite>,
    crates: run_names::RunCrates<'f>,
    sync_typed: crate::engine_body_record::SyncTypedFrom<'f>,
    shared_front: &SharedLazyFrontHandle,
    builder: &blaze_ladder::code_records::Builder,
    maker_entry: Option<ItemKey>,
    maker_start: blaze_ladder::run::special::MakerStart,
    jobs: &bstd::sync::chan::OwnedChan<ExportJob, SharedAlloc<'static>>,
    done: &bstd::sync::chan::OwnedChan<ExportDone, SharedAlloc<'static>>,
    procs_asked: &bstd::sync::chan::OwnedChan<usize, SharedAlloc<'static>>,
    procs_answer: &bstd::sync::chan::OwnedChan<ExportProcsAnswer, SharedAlloc<'static>>,
    worker_frame: &'f Frame,
    carry: &mut Option<ExportJob>,
    reset_at: &mut usize,
    mut worker_top: Option<&mut CompileTopStats>,
    heap: &'static blaze_own::Heap,
) -> ExportEpochEnd {
    let shared = heap.shared();
    let seen: &'f TreesSeen = worker_frame.alloc(TreesSeen::default());
    let worker_procs = ExportWorkerProcs {
        worker,
        cx,
        clock,
        frame: worker_frame,
        procs_asked,
        procs_answer,
        got: core::cell::OnceCell::new(),
    };
    let names = match run_names::names_in(cx, root, crates, crate::job_texts::Procs::ByNeed(&worker_procs), seen, worker_frame) {
        Ok(names) => names,
        Err(why) => {
            if let Some(job) = carry.take() {
                let msg = export_err_string(format_args!("export worker names init failed: {why}"), shared);
                let _ = done.send(cx, ExportDone { worker, row: job.row, result: Err(ExportWorkerErr::Fatal(msg)), typed: None });
            }
            return ExportEpochEnd::Done;
        }
    };
    let reader = sync_typed.reader_persisting(cx, crates.snapshot(), heap.alloc(), shared);
    let mut maker: Maker<'_, crate::run_ids::Runnable, SharedExportTyping<_>> = Maker::of_start(
        names,
        maker_entry,
        maker_start,
        SharedExportTyping::new(reader, shared_front, heap.alloc()),
        builder,
        Some(crate::run_parent::command_make_watch()),
        heap.alloc(),
    );
    let mut scratch: AVec<'_, u8> = Vec::new_in(heap.alloc());
    let mut makes = 0usize;

    loop {
        let job = match carry.take() {
            Some(j) => j,
            None => match jobs.recv(cx) {
                Ok(j) => j,
                Err(_) => return ExportEpochEnd::Done,
            },
        };
        let last_tabled = job.order.tabled;
        scratch.clear();
        let was_warm = makes > 0;
        let made = {
            let _span = cx.span("blaze build: code worker makes one entry");
            crate::work::unwinding(|| {
                if let Some(prog) = maker.program() {
                    prog.attach_shared(shared_front);
                }
                maker.prepare_local(job.enclosers.locals(), job.enclosers.enclosers());
                if let Some(prog) = maker.program() {
                    prog.attach_shared(shared_front);
                }
                job.order.make(&mut maker, &job.proof, job.name.as_slice(), &mut scratch)
            })
        };
        if let Some(prog) = maker.program() {
            prog.attach_shared(shared_front);
        }
        let result = match made {
            Ok(Ok(())) => {
                makes = makes.saturating_add(1);
                if makes == 1 {
                    let used = worker_frame.used();
                    *reset_at = used.saturating_add(EXPORT_WORKER_FRAME_HEADROOM).min(EXPORT_WORKER_FRAME_HARD_RESET_AT);
                }
                if let Some(top) = worker_top.as_deref_mut()
                    && let Some(sizes) = maker.take_last_make()
                {
                    top.record_sizes(&sizes);
                }
                let mut out = Vec::with_capacity_in(scratch.len(), shared);
                out.extend_from_slice(scratch.as_slice());
                Ok(out)
            }
            Ok(Err(why)) => {
                makes = makes.saturating_add(1);
                let msg = export_err_string(format_args!("{why}"), shared);
                if why.scope() == blaze_ladder::scope::Scope::One {
                    Err(ExportWorkerErr::ScopeOne(msg))
                } else {
                    Err(ExportWorkerErr::Fatal(msg))
                }
            }
            Err(_panicked) => {
                if was_warm {
                    return ExportEpochEnd::Reset(Some(job));
                }
                let msg = export_err_string(format_args!("code worker panicked while compiling item {:?}", job.order.tabled), shared);
                Err(ExportWorkerErr::Fatal(msg))
            }
        };
        let typed = maker.typing().kept.take_pending();
        let by = cx.until(bstd::Deadline::after(clock.now().since_start(), EXPORT_HAND_WAIT));
        let sending = by.span("blaze build: send compiled entry");
        if done.send(&sending, ExportDone { worker, row: job.row, result, typed }).is_err() {
            return ExportEpochEnd::Done;
        }
        let (live_bodies, store_tys) = maker.program().map_or((0, 0), |p| p.worker_epoch_load());
        let frame_used = worker_frame.used();
        let hard_reset = frame_used >= EXPORT_WORKER_FRAME_HARD_RESET_AT
            || live_bodies >= EXPORT_WORKER_HARD_LIVE_BODIES
            || store_tys >= EXPORT_WORKER_HARD_STORE_TYS;
        if hard_reset {
            return ExportEpochEnd::Reset(None);
        }
        let soft_reset = frame_used >= *reset_at
            || live_bodies >= EXPORT_WORKER_MAX_LIVE_BODIES
            || store_tys >= EXPORT_WORKER_MAX_STORE_TYS;
        if soft_reset {
            // Preserve warm bursts of sibling monomorphizations of the same `tabled` fn across the
            // soft threshold; reset cleanly as soon as the inbox switches to another `tabled` fn
            // (or is empty).
            match jobs.try_recv() {
                Ok(next_job) if next_job.order.tabled == last_tabled => {
                    *carry = Some(next_job);
                }
                Ok(next_job) => return ExportEpochEnd::Reset(Some(next_job)),
                Err(_) => return ExportEpochEnd::Reset(None),
            }
        }
    }
}

#[expect(clippy::too_many_arguments, reason = "worker task entry receives shared read-only program inputs and channels")]
fn run_export_code_worker(
    worker: usize,
    cx: &Cx,
    clock: &bstd::clock::Clock,
    root: &bstd::fs::Dir<bstd::fs::ReadWrite>,
    crates: run_names::RunCrates<'_>,
    sync_typed: crate::engine_body_record::SyncTypedFrom<'_>,
    shared_front: &SharedLazyFrontHandle,
    builder: &blaze_ladder::code_records::Builder,
    maker_entry: Option<ItemKey>,
    maker_start: blaze_ladder::run::special::MakerStart,
    jobs: &bstd::sync::chan::OwnedChan<ExportJob, SharedAlloc<'static>>,
    done: &bstd::sync::chan::OwnedChan<ExportDone, SharedAlloc<'static>>,
    procs_asked: &bstd::sync::chan::OwnedChan<usize, SharedAlloc<'static>>,
    procs_answer: &bstd::sync::chan::OwnedChan<ExportProcsAnswer, SharedAlloc<'static>>,
    collect_stats: bool,
    heap: &'static blaze_own::Heap,
) -> Option<CompileTopStats> {
    let mut worker_top = collect_stats.then(|| CompileTopStats::new_in(heap.shared()));
    let Ok(mut worker_frame) = Frame::new(bstd::mem::Tag::Other) else {
        return worker_top;
    };
    let mut carry: Option<ExportJob> = None;
    let mut reset_at = EXPORT_WORKER_FRAME_HEADROOM;

    loop {
        worker_frame.reset();
        worker_frame.trim(EXPORT_WORKER_FRAME_KEEP);
        match run_export_worker_epoch(
            worker,
            cx,
            clock,
            root,
            crates,
            sync_typed,
            shared_front,
            builder,
            maker_entry,
            maker_start,
            jobs,
            done,
            procs_asked,
            procs_answer,
            &worker_frame,
            &mut carry,
            &mut reset_at,
            worker_top.as_mut(),
            heap,
        ) {
            ExportEpochEnd::Done => return worker_top,
            ExportEpochEnd::Reset(next) => carry = next,
        }
    }
}

/// Finishes walking the remaining rows of `walk` (`*next_row..`) using `worker_count` parallel
/// codegen workers on `cx.scope()`, driving `Walk::begin` / `Order::make` / `Walk::finish`.
#[expect(clippy::too_many_arguments, reason = "parallel reachability walk borrows every phase state directly")]
fn walk_remaining_parallel<'a, 'f, 'p, 'n, D: blaze_core::Driver>(
    cx: &Cx,
    clock: &bstd::clock::Clock,
    root: &bstd::fs::Dir<bstd::fs::ReadWrite>,
    crates: run_names::RunCrates<'_>,
    sync_typed: crate::engine_body_record::SyncTypedFrom<'_>,
    shared_front: &SharedLazyFrontHandle,
    procs: &dyn crate::job_texts::ProcsByNeed<'p>,
    list: &blaze_world::crate_list::CrateList<'_>,
    ids: &crate::run_ids::RunIds<'_>,
    builder: &blaze_ladder::code_records::Builder,
    maker_entry: Option<ItemKey>,
    maker_start: blaze_ladder::run::special::MakerStart,
    worker_count: usize,
    root_count: usize,
    walk: &mut Walk<'a>,
    next_row: &mut usize,
    lent: &blaze_cas::plain::Lent<D, crate::store::plain_place::StoreAlloc>,
    codes: &mut HomeCodeStore<'_, D>,
    dep_typed_program: &mut AVec<'a, blaze_cas::Key>,
    dep_typed_toolchain: &mut AVec<'a, blaze_cas::Key>,
    maker: &mut Maker<'n, crate::run_ids::Runnable, SharedExportTyping<crate::engine_body_record::StoreTyped<'n>>>,
    scratch: &mut Scratch<'a>,
    outcomes: &mut AVec<'a, RowOutcome<'a>>,
    compiled: &mut usize,
    cached: &mut usize,
    unkeyed: &mut usize,
    stubs: &mut usize,
    mut top_stats: Option<&mut CompileTopStats>,
    frame: &'f Frame,
    alloc: Alloc<'a>,
    heap: &'static blaze_own::Heap,
) -> Result<(), CliError> {
    use bstd::sync::chan::owned;
    use core::num::NonZeroUsize;

    let collect_stats = top_stats.is_some();
    let shared = heap.shared();
    let start_row = *next_row;
    let inbox_cap = NonZeroUsize::new(EXPORT_PER_WORKER_IN_FLIGHT * 2).expect("non-zero inbox cap");
    let done_cap = NonZeroUsize::new((worker_count * EXPORT_PER_WORKER_IN_FLIGHT).max(16)).expect("non-zero done cap");
    let worker_nz = NonZeroUsize::new(worker_count).expect("non-zero worker count");
    let mut inboxes: Vec<bstd::sync::chan::OwnedChan<ExportJob, SharedAlloc<'static>>, SharedAlloc<'static>> =
        Vec::with_capacity_in(worker_count, shared);
    let mut procs_answers: Vec<bstd::sync::chan::OwnedChan<ExportProcsAnswer, SharedAlloc<'static>>, SharedAlloc<'static>> =
        Vec::with_capacity_in(worker_count, shared);
    for _ in 0..worker_count {
        inboxes.push(owned("blaze build: export worker inbox", inbox_cap, shared));
        procs_answers.push(owned("blaze build: export procs answer", NonZeroUsize::MIN, shared));
    }
    let done_ch = owned::<ExportDone, _>("blaze build: export done", done_cap, shared);
    let procs_asked = owned::<usize, _>("blaze build: export procs asks", worker_nz, shared);

    let scope = cx.scope();
    let mut tasks = Vec::with_capacity_in(worker_count, heap.alloc());
    for worker_id in 0..worker_count {
        let jobs_ref = &inboxes[worker_id];
        let done_ref = &done_ch;
        let asked_ref = &procs_asked;
        let answer_ref = &procs_answers[worker_id];
        match scope.must_overlap("blaze build: a code worker", EXPORT_WORKER_SCRATCH, move |wcx: &Cx| {
            let worker_top = run_export_code_worker(
                worker_id,
                wcx,
                clock,
                root,
                crates,
                sync_typed,
                shared_front,
                builder,
                maker_entry,
                maker_start,
                jobs_ref,
                done_ref,
                asked_ref,
                answer_ref,
                collect_stats,
                heap,
            );
            Ok::<Option<CompileTopStats>, core::convert::Infallible>(worker_top)
        }) {
            Ok(task) => tasks.push(task),
            Err(_) => break,
        }
    }

    let active_workers = tasks.len();
    let max_queued = (active_workers * EXPORT_MAX_QUEUED_PER_WORKER).max(1);
    let mut par_outcomes: AVec<'a, Option<RowOutcome<'a>>> = Vec::new_in(alloc);
    let mut pending_rows: AVec<'a, Option<blaze_ladder::body_ids::Pending>> = Vec::new_in(alloc);
    let mut scheduler: ExportScheduler<'a, ExportJob> = ExportScheduler::new_in(
        active_workers.max(1),
        EXPORT_PER_WORKER_IN_FLIGHT,
        EXPORT_STEAL_LOW_WATER,
        alloc,
    );

    let serve_procs = |procs_asked: &bstd::sync::chan::OwnedChan<usize, SharedAlloc<'static>>,
                       procs_answers: &[bstd::sync::chan::OwnedChan<ExportProcsAnswer, SharedAlloc<'static>>]| {
        while let Ok(worker_id) = procs_asked.try_recv() {
            let answer: ExportProcsAnswer = procs.procs().map_err(|why| export_err_string(format_args!("{why}"), shared));
            if let Some(ch) = procs_answers.get(worker_id) {
                let by = cx.until(bstd::Deadline::after(clock.now().since_start(), EXPORT_HAND_WAIT));
                let _ = ch.send(&by, answer);
            }
        }
    };

    let run_res: Result<(), CliError> = (|| {
        if active_workers == 0 {
            while *next_row < walk.len() {
                let row = *next_row;
                *next_row += 1;
                let tabled = walk.tabled(row);
                let symbol = walk.symbol(row);
                let kind = walk.kind(row);
                let proof = ids.runnable(tabled).map_err(|why| {
                    let krate = crate_of_item(list, tabled, heap.alloc());
                    box_err(ExportCliError::NotChecked { krate, function: tabled, symbol: symbol.0, row, reached: walk.len(), compiled: *compiled, cached: *cached, why }, heap)
                })?;
                let enclosers = crate::test_drive::suite_cache::local_enclosers_of(walk, row, tabled);
                maker.prepare_local(enclosers.locals(), enclosers.enclosers());
                match walk.one(row, ids, codes, builder, &builder.producer, maker, &proof, &body_ids::EntryForm, MOST_WALKED, scratch) {
                    Ok(one) => match one.stepped {
                        Stepped::Found(key) => {
                            *cached += 1;
                            outcomes.push(RowOutcome::Keyed(key));
                        }
                        Stepped::Made(key) => {
                            *compiled += 1;
                            if let Some(top) = top_stats.as_deref_mut()
                                && let Some(sizes) = maker.take_last_make()
                            {
                                top.record_sizes(&sizes);
                            }
                            outcomes.push(RowOutcome::Keyed(key));
                        }
                        Stepped::Unkeyed(_) => {
                            *unkeyed += 1;
                            if let Some(top) = top_stats.as_deref_mut()
                                && let Some(sizes) = maker.take_last_make()
                            {
                                top.record_sizes(&sizes);
                            }
                            let mut copied: AVec<'_, u8> = Vec::with_capacity_in(scratch.made().len(), alloc);
                            copied.extend_from_slice(scratch.made());
                            outcomes.push(RowOutcome::Unkeyed(copied));
                        }
                    },
                    Err(why) if row >= root_count && kind == RowKind::Instance && why.scope() == blaze_ladder::scope::Scope::One => {
                        let msg = bstd::mem::format_in!(frame, "blaze build: called unbuilt function: {why}");
                        let stub_bytes = make_unbuilt_stub_record(symbol, msg.as_str(), frame, alloc, heap).map_err(|e| box_err(e, heap))?;
                        *stubs += 1;
                        outcomes.push(RowOutcome::UnbuiltStub(stub_bytes));
                    }
                    Err(why) => {
                        return Err(box_err(
                            ExportCliError::Walk { symbol: symbol.0, shown: owned_fmt(format_args!("{why}"), frame, heap.alloc()) },
                            heap,
                        ));
                    }
                }
            }
            return Ok(());
        }

        let send_job = |worker: usize,
                        job: ExportJob,
                        inboxes: &[bstd::sync::chan::OwnedChan<ExportJob, SharedAlloc<'static>>]|
         -> Result<(), CliError> {
            let row = job.row;
            let by = cx.until(bstd::Deadline::after(clock.now().since_start(), EXPORT_HAND_WAIT));
            let sending = by.span("blaze build: dispatch export job");
            if inboxes[worker].send(&sending, job).is_err() {
                return Err(box_err(
                    ExportCliError::Walk {
                        symbol: [0; 32],
                        shown: owned_fmt(format_args!("export worker {worker} inbox closed while dispatching row {row}"), frame, heap.alloc()),
                    },
                    heap,
                ));
            }
            Ok(())
        };

        let refill_workers = |scheduler: &mut ExportScheduler<'a, ExportJob>,
                              inboxes: &[bstd::sync::chan::OwnedChan<ExportJob, SharedAlloc<'static>>]|
         -> Result<(), CliError> {
            while let Some((worker, job)) = scheduler.next_refill() {
                send_job(worker, job, inboxes)?;
            }
            Ok(())
        };

        let finish_one_done = |done: ExportDone,
                               scheduler: &mut ExportScheduler<'a, ExportJob>,
                               inboxes: &[bstd::sync::chan::OwnedChan<ExportJob, SharedAlloc<'static>>],
                               walk: &mut Walk<'a>,
                               codes: &mut HomeCodeStore<'_, D>,
                               dep_typed_program: &mut AVec<'a, blaze_cas::Key>,
                               dep_typed_toolchain: &mut AVec<'a, blaze_cas::Key>,
                               scratch: &mut Scratch<'a>,
                               pending_rows: &mut AVec<'a, Option<blaze_ladder::body_ids::Pending>>,
                               par_outcomes: &mut AVec<'a, Option<RowOutcome<'a>>>,
                               compiled: &mut usize,
                               cached: &mut usize,
                               unkeyed: &mut usize,
                               stubs: &mut usize|
         -> Result<(), CliError> {
            scheduler.on_complete(done.worker);
            // Refill the completing worker immediately (own LIFO queue, or FIFO steal if starved)
            // before spending coordinator time on `walk.finish`, keeping worker pipelines full.
            while let Some(job) = scheduler.next_dispatch_for(done.worker) {
                send_job(done.worker, job, inboxes)?;
            }
            persist_worker_typed_batch(lent, done.typed, dep_typed_program, dep_typed_toolchain, frame, heap)?;
            let row = done.row;
            let rel = row - start_row;
            let pend = pending_rows[rel].take().ok_or_else(|| {
                box_err(
                    ExportCliError::Walk { symbol: walk.symbol(row).0, shown: owned_fmt(format_args!("duplicate or unknown worker completion for row {row}"), frame, heap.alloc()) },
                    heap,
                )
            })?;
            let symbol = walk.symbol(row);
            let kind = walk.kind(row);
            match done.result {
                Ok(entry_bytes) => {
                    let one = walk
                        .finish::<RunError>(pend, entry_bytes.as_slice(), codes, &body_ids::EntryForm, MOST_WALKED, scratch)
                        .map_err(|why| {
                            box_err(
                                ExportCliError::Walk { symbol: symbol.0, shown: owned_fmt(format_args!("{why}"), frame, heap.alloc()) },
                                heap,
                            )
                        })?;
                    let outcome = match one.stepped {
                        Stepped::Found(key) => {
                            *cached += 1;
                            RowOutcome::Keyed(key)
                        }
                        Stepped::Made(key) => {
                            *compiled += 1;
                            RowOutcome::Keyed(key)
                        }
                        Stepped::Unkeyed(_) => {
                            *unkeyed += 1;
                            let mut copied: AVec<'_, u8> = Vec::with_capacity_in(entry_bytes.len(), alloc);
                            copied.extend_from_slice(entry_bytes.as_slice());
                            RowOutcome::Unkeyed(copied)
                        }
                    };
                    par_outcomes[rel] = Some(outcome);
                }
                Err(ExportWorkerErr::ScopeOne(why_str)) if row >= root_count && kind == RowKind::Instance => {
                    let _ = walk.abandon::<RunError>(pend);
                    let msg = bstd::mem::format_in!(frame, "blaze build: called unbuilt function: {}", why_str.as_str());
                    let stub_bytes = make_unbuilt_stub_record(symbol, msg.as_str(), frame, alloc, heap).map_err(|e| box_err(e, heap))?;
                    *stubs += 1;
                    par_outcomes[rel] = Some(RowOutcome::UnbuiltStub(stub_bytes));
                }
                Err(ExportWorkerErr::ScopeOne(why_str) | ExportWorkerErr::Fatal(why_str)) => {
                    let _ = walk.abandon::<RunError>(pend);
                    return Err(box_err(
                        ExportCliError::Walk { symbol: symbol.0, shown: owned_fmt(format_args!("{}", why_str.as_str()), frame, heap.alloc()) },
                        heap,
                    ));
                }
            }
            Ok(())
        };

        let max_in_flight = scheduler.max_in_flight();
        let drain_burst = active_workers.max(8);

        while *next_row < walk.len() || scheduler.in_flight() > 0 || scheduler.queued() > 0 {
            serve_procs(&procs_asked, procs_answers.as_slice());

            // Drain completions in bounded bursts interleaved with immediate worker refills and
            // newly discovered frontier dispatches so idle workers never wait behind a long drain.
            let mut drained = 0usize;
            while drained < drain_burst {
                let Ok(done) = done_ch.try_recv() else { break };
                drained += 1;
                finish_one_done(
                    done,
                    &mut scheduler,
                    &inboxes,
                    walk,
                    codes,
                    dep_typed_program,
                    dep_typed_toolchain,
                    scratch,
                    &mut pending_rows,
                    &mut par_outcomes,
                    compiled,
                    cached,
                    unkeyed,
                    stubs,
                )?;
            }
            refill_workers(&mut scheduler, &inboxes)?;

            // Dispatch waiting frontier rows until all workers are saturated (`in_flight ==
            // max_in_flight`), the frontier is drained, or the safety backlog cap is reached.
            while *next_row < walk.len() && scheduler.in_flight() < max_in_flight && scheduler.queued() < max_queued {
                let row = *next_row;
                *next_row += 1;
                let need_len = walk.len() - start_row;
                while par_outcomes.len() < need_len {
                    par_outcomes.push(None);
                }
                while pending_rows.len() < need_len {
                    pending_rows.push(None);
                }
                let rel = row - start_row;
                let tabled = walk.tabled(row);
                let symbol = walk.symbol(row);
                let kind = walk.kind(row);
                let proof = ids.runnable(tabled).map_err(|why| {
                    let krate = crate_of_item(list, tabled, heap.alloc());
                    box_err(ExportCliError::NotChecked { krate, function: tabled, symbol: symbol.0, row, reached: walk.len(), compiled: *compiled, cached: *cached, why }, heap)
                })?;
                let enclosers = crate::test_drive::suite_cache::local_enclosers_of(walk, row, tabled);
                let mut verify_maker = LazyVerifyMaker { maker, shared: shared_front, enclosers: &enclosers, prepared: false };
                match walk.begin(row, ids, codes, builder, &builder.producer, &mut verify_maker, &body_ids::EntryForm, MOST_WALKED, scratch) {
                    Ok(blaze_ladder::body_ids::Begun::Stepped(one)) => match one.stepped {
                        Stepped::Found(key) => {
                            *cached += 1;
                            par_outcomes[rel] = Some(RowOutcome::Keyed(key));
                        }
                        Stepped::Made(key) => {
                            *compiled += 1;
                            if let Some(top) = top_stats.as_deref_mut()
                                && let Some(sizes) = maker.take_last_make()
                            {
                                top.record_sizes(&sizes);
                            }
                            par_outcomes[rel] = Some(RowOutcome::Keyed(key));
                        }
                        Stepped::Unkeyed(_) => {
                            *unkeyed += 1;
                            if let Some(top) = top_stats.as_deref_mut()
                                && let Some(sizes) = maker.take_last_make()
                            {
                                top.record_sizes(&sizes);
                            }
                            let mut copied: AVec<'_, u8> = Vec::with_capacity_in(scratch.made().len(), alloc);
                            copied.extend_from_slice(scratch.made());
                            par_outcomes[rel] = Some(RowOutcome::Unkeyed(copied));
                        }
                    },
                    Ok(blaze_ladder::body_ids::Begun::Make(pend)) => {
                        let name_slice = walk.name(row);
                        let mut name = Vec::with_capacity_in(name_slice.len(), shared);
                        name.extend_from_slice(name_slice);
                        let job = ExportJob { row, order: pend.order(), proof, name, enclosers };
                        pending_rows[rel] = Some(pend);
                        if let Some((target_worker, dispatch_job)) = scheduler.enqueue(tabled, job) {
                            send_job(target_worker, dispatch_job, &inboxes)?;
                        }
                    }
                    Err(why) if row >= root_count && kind == RowKind::Instance && why.scope() == blaze_ladder::scope::Scope::One => {
                        let msg = bstd::mem::format_in!(frame, "blaze build: called unbuilt function: {why}");
                        let stub_bytes = make_unbuilt_stub_record(symbol, msg.as_str(), frame, alloc, heap).map_err(|e| box_err(e, heap))?;
                        *stubs += 1;
                        par_outcomes[rel] = Some(RowOutcome::UnbuiltStub(stub_bytes));
                    }
                    Err(why) => {
                        return Err(box_err(
                            ExportCliError::Walk { symbol: symbol.0, shown: owned_fmt(format_args!("{why}"), frame, heap.alloc()) },
                            heap,
                        ));
                    }
                }
            }
            refill_workers(&mut scheduler, &inboxes)?;

            // If workers are busy and we cannot dispatch more right now, wait briefly on `done_ch`
            // (bounded slice so `procs_asked` is always serviced without deadlock).
            if scheduler.in_flight() > 0
                && (*next_row >= walk.len() || scheduler.in_flight() >= max_in_flight || scheduler.queued() >= max_queued)
            {
                if cx.check().is_err() {
                    return Err(box_err(
                        ExportCliError::Walk { symbol: [0; 32], shown: owned_fmt(format_args!("export cancelled while waiting for code workers"), frame, heap.alloc()) },
                        heap,
                    ));
                }
                let slice_cx = cx.until(bstd::Deadline::after(clock.now().since_start(), EXPORT_RECV_SLICE));
                match done_ch.recv(&slice_cx) {
                    Ok(done) => {
                        finish_one_done(
                            done,
                            &mut scheduler,
                            &inboxes,
                            walk,
                            codes,
                            dep_typed_program,
                            dep_typed_toolchain,
                            scratch,
                            &mut pending_rows,
                            &mut par_outcomes,
                            compiled,
                            cached,
                            unkeyed,
                            stubs,
                        )?;
                        refill_workers(&mut scheduler, &inboxes)?;
                    }
                    Err(bstd::sync::RecvError::Stopped { .. } | bstd::sync::RecvError::Empty { .. }) => {
                        // Slice expired: loop back to service `procs_asked` and check cancellation.
                    }
                    Err(bstd::sync::RecvError::Closed { .. }) => {
                        let in_flight = scheduler.in_flight();
                        return Err(box_err(
                            ExportCliError::Walk { symbol: [0; 32], shown: owned_fmt(format_args!("export code worker done channel closed with {in_flight} jobs in flight"), frame, heap.alloc()) },
                            heap,
                        ));
                    }
                }
            }
        }

        let need_len = walk.len() - start_row;
        for rel in 0..need_len {
            let outcome = par_outcomes.get_mut(rel).and_then(Option::take).ok_or_else(|| {
                let row = start_row + rel;
                box_err(
                    ExportCliError::Walk { symbol: walk.symbol(row).0, shown: owned_fmt(format_args!("missing outcome for walked row {row}"), frame, heap.alloc()) },
                    heap,
                )
            })?;
            outcomes.push(outcome);
        }
        Ok(())
    })();

    // Always close channels, drain unstarted queued jobs, and join all worker tasks before
    // returning (linear scope invariant).
    for ch in inboxes.iter() {
        ch.close();
        while ch.try_recv().is_ok() {}
    }
    done_ch.close();
    procs_asked.close();
    for ch in procs_answers.iter() {
        ch.close();
    }
    while done_ch.try_recv().is_ok() {}
    let ending = cx.until(bstd::Deadline::after(clock.now().since_start(), EXPORT_END_WAIT));
    for task in tasks {
        if let Ok(Some(worker_top)) = task.join(&ending)
            && let Some(dst) = top_stats.as_deref_mut()
        {
            dst.merge(worker_top);
        }
    }
    let _ = scope.join(&ending);
    run_res
}

/// Core whole-program reachability closure + `blaze_export::export` (or `wasm64`) + atomic write.
#[expect(clippy::too_many_arguments, reason = "build pipeline entry: each argument one handle or selected option")]
fn export_program<'f>(
    cx: &Cx,
    mut standing: Standing<'f, '_>,
    picked: &run_package::Program<'f>,
    fs: &Fs,
    output_flag: Option<&str>,
    target: BuildTarget,
    started_at: bstd::clock::Instant,
    stats: bool,
    phases: &mut Phases,
    watch: &dyn blaze_screen::Watch,
) -> Result<ExportSummary<'f>, CliError> {
    let (frame, heap) = (standing.frame, standing.heap);
    let Some(platform) = host_platform() else {
        return Err(box_err(ExportCliError::UnsupportedHost, heap));
    };
    let output_path: &'f str = match output_flag {
        Some(path) => frame.alloc_str(path),
        None => match target {
            BuildTarget::Native => picked.bin,
            BuildTarget::Wasm64 => {
                let name = bstd::mem::format_in!(frame, "{}.wasm", picked.bin);
                frame.alloc_str(name.as_str())
            }
        },
    };
    let export_frame = Frame::with_reserve(EXPORT_FRAME_RESERVE, bstd::mem::Tag::Other).map_err(|_| box_err(ExportCliError::Frame, heap))?;
    let export_erased = Erased::new(&export_frame);
    let export_bump = export_erased.bump();
    let alloc = export_bump.alloc();

    let erased = Erased::new(frame);
    let bump = erased.bump();

    watch.see(blaze_screen::Step::Began(blaze_screen::Stage::Crates));
    // Ensure the store is mapped before reading records: on a warm certificate hit, `engine_check`
    // skipped pool tasks and never triggered `Store::map_all`.
    standing.store.map_all(cx)?;
    let mut read = EngineRecords::new(heap.alloc());
    let seen = TreesSeen::default();
    let pack_dirs = SyncPackDirs::default();
    let root = standing.checkout;
    let lints = crate::lints::of_root(cx, root, frame)?;
    let mut facts = start_facts(&standing, fs, phases, lints);
    let answered = run_names::FromEngine { graph: standing.graph, names: standing.names };
    let at = run_names::program_row(cx, root, &answered, &mut *standing.store, &mut facts, picked.root, heap, frame).map_err(CliError::RunNames)?;
    let owner = run_names::Owner::At(at);
    let found = run_names::found_answered(cx, &answered, owner, &mut *standing.store, &mut facts.bodies, &mut read, heap, frame).map_err(CliError::RunNames)?;
    let program_found = found.program().ok_or(CliError::Start(StartError::NoProgram))?;
    let closure = program_found.closure_in(bump.alloc());

    // Native C image layout & C exports (returns `None` when the program links no C objects or
    // non-system libraries).
    let native_export = crate::engine_native::export_native_for(cx, &mut standing, &closure, watch, &export_frame, export_bump).map_err(|error| {
        box_err(
            ExportCliError::Native { shown: owned_fmt(format_args!("{error}"), frame, heap.alloc()) },
            heap,
        )
    })?;

    watch.see(blaze_screen::Step::Began(blaze_screen::Stage::BodyIds));
    let list = found.list().ok_or(CliError::Start(StartError::NoProgram))?;
    let asked = crate::run_ids::Asked::default();
    let began_ids = phases.now();
    let ids = body_ids(cx, &mut standing, list, &asked, bump)?;
    let ids_ns = phases.since(began_ids);
    let ids_line = ids.line(&asked, ids_ns);
    let cert = crate::engine_start::body_ids_cert(standing.root, standing.scope);
    let workspace_id = crate::engine_start::workspace_anchor_id(list, alloc);
    let build_shape = build_program_shape(picked, workspace_id, list.key(at));
    let ids_table = blaze_cas::Key(crate::run_ids::table_key(cert, standing.toolchain, &standing.tools).0);
    let today = crate::start_list::Today {
        certificate: crate::start_list::Certificate::of_answer(&standing),
        name: crate::start_list::ListName::of(&build_shape),
        tables: [ids_table, ids_table],
    };

    // `blaze export` produces a release native executable: use `[profile.release]` overflow and
    // panic settings (`OverflowChecks::Off` by default) so hot arithmetic loops compile without
    // per-add/sub panic branches and small helpers become pure leaves for bottom-up LTO inlining.
    let overflow = program::release_overflow(cx, root, frame)?;
    let panic_profile = program::release_profile_panic(cx, root, frame)?;
    let panic_ladder = blaze_ladder::panic_strategy::of_profile(panic_profile);
    let export_panic = match panic_profile {
        blaze_manifest::layout::panic::ProfilePanic::Unwind => Panic::Unwind,
        blaze_manifest::layout::panic::ProfilePanic::Abort => Panic::Abort,
    };
    let isa = crate::run_parent::isa().map_err(|why| {
        box_err(
            ExportCliError::Builder { shown: owned_fmt(format_args!("{why}"), frame, heap.alloc()) },
            heap,
        )
    })?;
    let producer = standing.producers.get::<blaze_ids::producer::CodeBuilder>();
    let builder = crate::run_parent::builder_for(picked.edition, overflow, panic_ladder, producer, isa).map_err(|why| {
        box_err(
            ExportCliError::Builder { shown: owned_fmt(format_args!("{why}"), frame, heap.alloc()) },
            heap,
        )
    })?;

    let lent = standing.lent;
    let typed = crate::engine_body_record::TypedFrom { cx, lent, records: standing.records(), alloc: heap.alloc() };
    let env = &facts.fetch_env;
    let inputs = RunProcs { cx, root, snapshot: found.snapshot(), graph: standing.graph, env, frame };
    let maker_procs = crate::job_texts::MadeOnce::new(|| inputs.make());
    let procs = crate::job_texts::Procs::ByNeed(&maker_procs);

    let scope_records = crate::front_load::ScopeRecords::new(found.snapshot(), blaze_world::crate_list::CrateGraph::len(list), heap);
    let crates = run_names::crates_of_by_need(cx, &found, owner, &mut *standing.store, &mut facts.bodies, &mut read, &scope_records, env, &pack_dirs, frame, heap).map_err(CliError::RunNames)?;
    let names = run_names::names_in(cx, root, crates, procs, &seen, frame).map_err(CliError::RunNames)?;

    watch.see(blaze_screen::Step::Began(blaze_screen::Stage::Roots));
    let export_items: &[blaze_id::ItemKey] = native_export.as_ref().map_or(&[], |n| n.items.as_slice());
    let roots = run_roots(names.records, names.lang_crates, builder.settings.panic, export_items, names.frame, heap.alloc(), alloc).map_err(|why| {
        box_err(
            ExportCliError::Roots { shown: owned_fmt(format_args!("{why}"), frame, heap.alloc()) },
            heap,
        )
    })?;

    let entry = entry_of_roots(&roots);
    let personality = roots.head().personality;
    let maker_entry = match roots.shim() {
        Some(shim) => Some(shim.main.tabled),
        None => (!roots.is_empty()).then(|| roots.root(0).tabled),
    };
    let shared_front: SharedLazyFrontHandle =
        std::sync::Arc::new_in(SharedLazyFront::new_in(heap.shared()), heap.shared());
    let mut maker = Maker::of_roots(
        names,
        maker_entry,
        &roots,
        SharedExportTyping::new(typed.over(ids), &shared_front, heap.alloc()),
        &builder,
        Some(crate::run_parent::command_make_watch()),
        heap.alloc(),
    );
    let mut codes = HomeCodeStore::new(lent);

    // Whole-program reachability closure from all roots.
    let mut walk = Walk::new_in(alloc);
    for i in 0..roots.len() {
        walk.root::<RunError>(roots.row(i), MOST_WALKED).map_err(|why| {
            box_err(
                ExportCliError::Walk { symbol: roots.entry.0, shown: owned_fmt(format_args!("{why}"), frame, heap.alloc()) },
                heap,
            )
        })?;
    }

    let mut scratch = Scratch::new_in(alloc);
    let mut outcomes: AVec<'_, RowOutcome<'_>> = Vec::new_in(alloc);
    let mut dep_typed_program: AVec<'_, blaze_cas::Key> = Vec::new_in(alloc);
    let mut dep_typed_toolchain: AVec<'_, blaze_cas::Key> = Vec::new_in(alloc);
    let (mut compiled, mut cached, mut unkeyed, mut stubs) = (0usize, 0usize, 0usize, 0usize);
    let root_count = roots.len();
    let export_workers = usize::try_from(standing.workers.get().min(cx.workers()).saturating_sub(1))
        .unwrap_or(0)
        .min(MAX_EXPORT_WORKERS);

    let mut top_stats = stats.then(|| CompileTopStats::new_in(heap.shared()));
    let front_ns = phases.since(started_at);
    let walk_began = phases.now();
    let mut next_row = 0usize;
    {
        let _walk_span = cx.span("export: walk reachable functions");
        while next_row < walk.len() {
            if export_workers >= 1 && compiled >= EXPORT_PAR_MIN_MISSES {
                let sync_typed = crate::engine_body_record::SyncTypedFrom { records: standing.records(), ids: ids.lookup() };
                walk_remaining_parallel(
                    cx,
                    &env.clock,
                    root,
                    crates,
                    sync_typed,
                    &shared_front,
                    &maker_procs,
                    list,
                    &ids,
                    &builder,
                    maker_entry,
                    roots.maker_start,
                    export_workers,
                    root_count,
                    &mut walk,
                    &mut next_row,
                    lent,
                    &mut codes,
                    &mut dep_typed_program,
                    &mut dep_typed_toolchain,
                    &mut maker,
                    &mut scratch,
                    &mut outcomes,
                    &mut compiled,
                    &mut cached,
                    &mut unkeyed,
                    &mut stubs,
                    top_stats.as_mut(),
                    frame,
                    alloc,
                    heap,
                )?;
                break;
            }
            let row = next_row;
            next_row += 1;
            let tabled = walk.tabled(row);
            let symbol = walk.symbol(row);
            let kind = walk.kind(row);
            let proof = ids.runnable(tabled).map_err(|why| {
                let krate = crate_of_item(list, tabled, heap.alloc());
                box_err(ExportCliError::NotChecked { krate, function: tabled, symbol: symbol.0, row, reached: walk.len(), compiled, cached, why }, heap)
            })?;
            let enclosers = crate::test_drive::suite_cache::local_enclosers_of(&walk, row, tabled);
            maker.prepare_local(enclosers.locals(), enclosers.enclosers());
            if let Some(prog) = maker.program() {
                prog.attach_shared(&shared_front);
            }
            match walk.one(row, &ids, &mut codes, &builder, &builder.producer, &mut maker, &proof, &body_ids::EntryForm, MOST_WALKED, &mut scratch) {
                Ok(one) => match one.stepped {
                    Stepped::Found(key) => {
                        cached += 1;
                        outcomes.push(RowOutcome::Keyed(key));
                    }
                    Stepped::Made(key) => {
                        compiled += 1;
                        if let Some(top) = top_stats.as_mut()
                            && let Some(sizes) = maker.take_last_make()
                        {
                            top.record_sizes(&sizes);
                        }
                        outcomes.push(RowOutcome::Keyed(key));
                    }
                    Stepped::Unkeyed(_) => {
                        unkeyed += 1;
                        if let Some(top) = top_stats.as_mut()
                            && let Some(sizes) = maker.take_last_make()
                        {
                            top.record_sizes(&sizes);
                        }
                        let mut copied: AVec<'_, u8> = Vec::with_capacity_in(scratch.made().len(), alloc);
                        copied.extend_from_slice(scratch.made());
                        outcomes.push(RowOutcome::Unkeyed(copied));
                    }
                },
                Err(why) if row >= root_count && kind == RowKind::Instance && why.scope() == blaze_ladder::scope::Scope::One => {
                    let msg = bstd::mem::format_in!(frame, "blaze build: called unbuilt function: {why}");
                    let stub_bytes = make_unbuilt_stub_record(symbol, msg.as_str(), frame, alloc, heap).map_err(|e| box_err(e, heap))?;
                    stubs += 1;
                    outcomes.push(RowOutcome::UnbuiltStub(stub_bytes));
                }
                Err(why) => {
                    return Err(box_err(
                        ExportCliError::Walk { symbol: symbol.0, shown: owned_fmt(format_args!("{why}"), frame, heap.alloc()) },
                        heap,
                    ));
                }
            }
        }
    }
    let walk_ns = phases.since(walk_began);

    // Commit and root all compiled `CodeRecord`s and persisted dep/toolchain `Typed` batches
    // BEFORE `wasm64`, LTO, or `blaze_export::export()` runs, so a downstream LTO/linker/output
    // failure never discards compiled work, and unlent store arena chunks are freed before linking.
    checkpoint_export_roots(
        lent,
        &today,
        builder.toolchain,
        &mut codes,
        &maker.typing().kept,
        dep_typed_program.as_slice(),
        dep_typed_toolchain.as_slice(),
        frame,
        heap,
    )?;

    if target == BuildTarget::Wasm64 {
        let link_began = phases.now();
        let mut wasm_bytes: AVec<'_, u8> = Vec::new_in(alloc);
        let wasm_opts = blaze_ladder::run::make_one::wasm64::Wasm64BuildOpts {
            module_name: picked.bin,
            start_symbol: Some(roots.entry),
            exports: &[],
            export_globals: &[],
            include_blaze_host_v1: false,
            opts: blaze_export::wasm64::Wasm64Opts::USER_PROGRAM,
        };
        let built = blaze_ladder::run::make_one::wasm64::compile_walk_to_wasm64(
            &walk,
            |tabled| ids.runnable(tabled).ok(),
            &mut maker,
            &wasm_opts,
            blaze_ladder::run::make_one::wasm64::default_wasm64_import,
            &mut wasm_bytes,
            alloc,
        )
        .map_err(|why| {
            box_err(
                ExportCliError::Wasm64 { shown: owned_fmt(format_args!("{why}"), frame, heap.alloc()) },
                heap,
            )
        })?;
        end_procs(cx, &maker_procs, standing.err, "build")?;
        let ids_line = crate::run_ids::IdsLine { read: asked.bodies(), folded: asked.reads(), ..ids_line };
        phases.run_names = Some(run_names::NamesRead::of(read.counts(), &seen).with_ids(ids_line));
        if let Some(why) = seen.refused() {
            return Err(CliError::RunNames(run_names::RunNamesError::Trees { why }));
        }
        if lent.needs_commit() {
            checkpoint_export_roots(
                lent,
                &today,
                builder.toolchain,
                &mut codes,
                &maker.typing().kept,
                dep_typed_program.as_slice(),
                dep_typed_toolchain.as_slice(),
                frame,
                heap,
            )?;
        }
        drop(codes);
        write_output_file(cx, fs, output_path, wasm_bytes.as_slice(), false, heap)?;
        let link_ns = phases.since(link_began);
        let exported = Exported {
            platform: Platform::Wasm64,
            functions: u32::try_from(built.func_count).unwrap_or(u32::MAX),
            folded: 0,
            fast: 0,
            optimized: 0,
            made: 0,
            imports: u32::try_from(built.import_count).unwrap_or(u32::MAX),
            len: wasm_bytes.len() as u64,
            identity: built.exported.identity,
            profile: None,
            panics: blaze_export::Panics::Abort { functions: 0 },
        };
        return Ok(ExportSummary {
            output: output_path,
            exported,
            data_count: built.static_count,
            tls_count: 0,
            compiled,
            cached,
            unkeyed,
            stubs,
            lto_bodies: 0,
            lto_inlined: 0,
            lto_hot: 0,
            phase_times: ExportPhaseTimes { front_ns, walk_ns, lto_ns: 0, link_ns },
            shared_front_stats: shared_front.stats(),
            top_stats,
        });
    }

    // Whole-program demand-driven cross-function LIR inlining + host-ISA SOTA emission over
    // workspace functions and their non-cold reachable callees (kept ephemeral in `export_bump`,
    // never written into portable `HomeCodeStore`).
    let lto_began = phases.now();
    let mut lto_seeds: AVec<'_, usize> = Vec::new_in(alloc);
    for (row, outcome) in outcomes.iter().enumerate() {
        if walk.kind(row) == RowKind::Instance
            && !matches!(outcome, RowOutcome::UnbuiltStub(_))
            && ids.grain(walk.tabled(row)) == Some(crate::run_ids::Grain::Workspace)
        {
            lto_seeds.push(row);
        }
    }
    let lto_opts = blaze_ladder::run::make_one::export_lto::ExportLtoOpts::for_export(isa);
    let lto_runner = CliLtoRunner { cx, workers: standing.workers };
    let lto_out = {
        let _lto_span = cx.span("export: workspace LTO");
        blaze_ladder::run::make_one::export_lto::optimize_export_workspace_par(
            &walk,
            lto_seeds.as_slice(),
            |row| walk.kind(row) == RowKind::Instance && !matches!(outcomes.get(row), Some(RowOutcome::UnbuiltStub(_))),
            |tabled| ids.runnable(tabled).ok(),
            &mut maker,
            lto_opts,
            heap.shared(),
            alloc,
            &lto_runner,
        )
        .map_err(|why| {
            box_err(
                ExportCliError::Lto { shown: owned_fmt(format_args!("{why}"), frame, heap.alloc()) },
                heap,
            )
        })?
    };
    let lto_bodies = lto_out.bodies.len();
    let lto_inlined = lto_out.total_inlined;
    let lto_hot_symbols = lto_out.hot_symbols;
    let lto_profile_digest = lto_out.profile_digest;
    let lto_hot = lto_hot_symbols.len();
    let mut extra_roots: Vec<(ItemKey, Vec<u8, SharedAlloc<'static>>), SharedAlloc<'static>> = Vec::new_in(heap.shared());
    for body in lto_out.bodies {
        if let Some(slot) = outcomes.get_mut(body.row) {
            *slot = RowOutcome::Lto {
                record: body.record,
                vtable_data: body.vtable_data,
            };
        }
        for decl in body.extra_declared {
            extra_roots.push(decl);
        }
    }
    for (tabled, name_bytes) in extra_roots {
        walk.root::<RunError>((tabled, name_bytes.as_slice()), MOST_WALKED).map_err(|why| {
            box_err(
                ExportCliError::Walk { symbol: roots.entry.0, shown: owned_fmt(format_args!("{why}"), frame, heap.alloc()) },
                heap,
            )
        })?;
    }
    while next_row < walk.len() {
        let row = next_row;
        next_row += 1;
        let tabled = walk.tabled(row);
        let symbol = walk.symbol(row);
        let proof = ids.runnable(tabled).map_err(|why| {
            let krate = crate_of_item(list, tabled, heap.alloc());
            box_err(ExportCliError::NotChecked { krate, function: tabled, symbol: symbol.0, row, reached: walk.len(), compiled, cached, why }, heap)
        })?;
        match walk.one(row, &ids, &mut codes, &builder, &builder.producer, &mut maker, &proof, &body_ids::EntryForm, MOST_WALKED, &mut scratch) {
            Ok(one) => match one.stepped {
                Stepped::Found(key) => {
                    cached += 1;
                    outcomes.push(RowOutcome::Keyed(key));
                }
                Stepped::Made(key) => {
                    compiled += 1;
                    if let Some(top) = top_stats.as_mut()
                        && let Some(sizes) = maker.take_last_make()
                    {
                        top.record_sizes(&sizes);
                    }
                    outcomes.push(RowOutcome::Keyed(key));
                }
                Stepped::Unkeyed(_) => {
                    unkeyed += 1;
                    if let Some(top) = top_stats.as_mut()
                        && let Some(sizes) = maker.take_last_make()
                    {
                        top.record_sizes(&sizes);
                    }
                    let mut copied: AVec<'_, u8> = Vec::with_capacity_in(scratch.made().len(), alloc);
                    copied.extend_from_slice(scratch.made());
                    outcomes.push(RowOutcome::Unkeyed(copied));
                }
            },
            Err(why) => {
                return Err(box_err(
                    ExportCliError::Walk { symbol: symbol.0, shown: owned_fmt(format_args!("{why}"), frame, heap.alloc()) },
                    heap,
                ));
            }
        }
    }
    end_procs(cx, &maker_procs, standing.err, "build")?;
    let lto_ns = phases.since(lto_began);
    let link_began = phases.now();

    let ids_line = crate::run_ids::IdsLine { read: asked.bodies(), folded: asked.reads(), ..ids_line };
    phases.run_names = Some(run_names::NamesRead::of(read.counts(), &seen).with_ids(ids_line));
    if let Some(why) = seen.refused() {
        return Err(CliError::RunNames(run_names::RunNamesError::Trees { why }));
    }

    // Detect whether any walked row or code record touches Rust thread-locals.
    let tls_getter_sym = blaze_ladder::symbols::tls_getter_symbol();
    let tls_desc_sym = blaze_ladder::symbols::tls_desc_symbol();
    let tls_ender_sym = blaze_ladder::tls_index::tls_ender_symbol();
    let mut has_tls = false;
    for (row, outcome) in outcomes.iter().enumerate() {
        match walk.kind(row) {
            RowKind::ThreadLocal => {
                has_tls = true;
                break;
            }
            RowKind::Instance => {
                let symbol = walk.symbol(row);
                let rec_bytes = match outcome {
                    RowOutcome::Keyed(key) => {
                        let entry_bytes = entry_of(&codes, *key)
                            .map_err(|why| box_err(ExportCliError::Store { symbol: symbol.0, shown: owned_fmt(format_args!("{why}"), frame, heap.alloc()) }, heap))?
                            .ok_or_else(|| box_err(ExportCliError::MissingEntry { symbol: symbol.0 }, heap))?;
                        statics::entry_record(entry_bytes)
                            .map_err(|why| box_err(ExportCliError::Decode { symbol: symbol.0, what: "code entry", shown: owned_fmt(format_args!("{why}"), frame, heap.alloc()) }, heap))?
                    }
                    RowOutcome::Unkeyed(bytes) => statics::entry_record(bytes.as_slice())
                        .map_err(|why| box_err(ExportCliError::Decode { symbol: symbol.0, what: "code entry", shown: owned_fmt(format_args!("{why}"), frame, heap.alloc()) }, heap))?,
                    RowOutcome::Lto { record, .. } => record.as_slice(),
                    RowOutcome::UnbuiltStub(bytes) => bytes.as_slice(),
                };
                let view = CodeView::read(rec_bytes)
                    .map_err(|why| box_err(ExportCliError::Decode { symbol: symbol.0, what: "code record", shown: owned_fmt(format_args!("{why}"), frame, heap.alloc()) }, heap))?;
                if code_touches_tls(&view, tls_getter_sym, tls_desc_sym) {
                    has_tls = true;
                    break;
                }
            }
            RowKind::Static => {}
        }
    }

    // Emit TLS getter + ender special records into `codes` before borrowing `&codes` immutably.
    let tls_keys = if has_tls {
        let getter = make_special(Special::Getter, &builder, &mut codes, alloc).map_err(|why| {
            box_err(
                ExportCliError::Special { what: "TLS getter", shown: owned_fmt(format_args!("{why:?}"), frame, heap.alloc()) },
                heap,
            )
        })?;
        let ender = make_special(Special::Ender, &builder, &mut codes, alloc).map_err(|why| {
            box_err(
                ExportCliError::Special { what: "TLS ender", shown: owned_fmt(format_args!("{why:?}"), frame, heap.alloc()) },
                heap,
            )
        })?;
        Some((getter.key, ender.key))
    } else {
        None
    };

    // Commit and root any extra LTO-discovered or TLS special records BEFORE `ProgramIn` borrows
    // `&codes` and BEFORE `blaze_export::export()` runs, so a linker error never discards them.
    if lent.needs_commit() || tls_keys.is_some() {
        checkpoint_export_roots(
            lent,
            &today,
            builder.toolchain,
            &mut codes,
            &maker.typing().kept,
            dep_typed_program.as_slice(),
            dep_typed_toolchain.as_slice(),
            frame,
            heap,
        )?;
    }

    // Assemble `ProgramIn` functions and data (zero-copy for cached positionless records).
    let mut functions: AVec<'_, Function<'_>> = Vec::with_capacity_in(outcomes.len() + 2, alloc);
    let mut symbol_names: AVec<'_, (CodeSymbol, &'_ str)> = Vec::with_capacity_in(outcomes.len() + 2, alloc);
    let mut sym_name_buf: AString<'_> = String::new_in(alloc);
    let mut data: AVec<'_, Data<'_>> = Vec::new_in(alloc);
    let mut seen_data: AVec<'_, Target> = Vec::new_in(alloc);
    let mut tls_count = 0usize;

    for (row, outcome) in outcomes.iter().enumerate() {
        let symbol = walk.symbol(row);
        if walk.kind(row) == RowKind::Instance {
            let tabled = walk.tabled(row);
            let shown = maker.instance_shown(tabled, walk.name(row));
            let sym_name = format_symbol_name(symbol, tabled, shown, names.records, list, &mut sym_name_buf, export_bump);
            symbol_names.push((symbol, sym_name));
        }
        if let RowOutcome::Lto { record, vtable_data } = outcome {
            let raw_view = CodeView::read(record.as_slice())
                .map_err(|why| box_err(ExportCliError::Decode { symbol: symbol.0, what: "LTO code record", shown: owned_fmt(format_args!("{why}"), frame, heap.alloc()) }, heap))?;
            let view = materialize_positionless(
                symbol,
                raw_view,
                |item, site| names.records.source_position(item, site, alloc),
                frame,
                export_bump,
                heap,
            )
            .map_err(|e| box_err(e, heap))?;
            functions.push(Function { symbol, record: view, tier: Tier::Optimized });
            for (vtable_sym, datum_bytes) in vtable_data.iter() {
                push_datum(
                    &mut data,
                    &mut seen_data,
                    &mut tls_count,
                    Target { space: Space::Vtable, symbol: *vtable_sym },
                    datum_bytes.as_slice(),
                    frame,
                    export_bump,
                    heap,
                )?;
            }
            continue;
        }
        if let RowOutcome::UnbuiltStub(stub_bytes) = outcome {
            let view = CodeView::read(stub_bytes.as_slice())
                .map_err(|why| box_err(ExportCliError::Decode { symbol: symbol.0, what: "stub code record", shown: owned_fmt(format_args!("{why}"), frame, heap.alloc()) }, heap))?;
            functions.push(Function { symbol, record: view, tier: Tier::Fast });
            continue;
        }
        let entry_bytes = match outcome {
            RowOutcome::Keyed(key) => entry_of(&codes, *key)
                .map_err(|why| box_err(ExportCliError::Store { symbol: symbol.0, shown: owned_fmt(format_args!("{why}"), frame, heap.alloc()) }, heap))?
                .ok_or_else(|| box_err(ExportCliError::MissingEntry { symbol: symbol.0 }, heap))?,
            RowOutcome::Unkeyed(bytes) => bytes.as_slice(),
            RowOutcome::Lto { .. } | RowOutcome::UnbuiltStub(_) => continue,
        };
        let rec_bytes = statics::entry_record(entry_bytes)
            .map_err(|why| box_err(ExportCliError::Decode { symbol: symbol.0, what: "code entry", shown: owned_fmt(format_args!("{why}"), frame, heap.alloc()) }, heap))?;
        match walk.kind(row) {
            RowKind::Instance => {
                let raw_view = CodeView::read(rec_bytes)
                    .map_err(|why| box_err(ExportCliError::Decode { symbol: symbol.0, what: "code record", shown: owned_fmt(format_args!("{why}"), frame, heap.alloc()) }, heap))?;
                let view = materialize_positionless(
                    symbol,
                    raw_view,
                    |item, site| names.records.source_position(item, site, alloc),
                    frame,
                    export_bump,
                    heap,
                )
                .map_err(|e| box_err(e, heap))?;
                functions.push(Function { symbol, record: view, tier: Tier::Fast });
                statics::each_vtable_datum(
                    entry_bytes,
                    |vtable_sym, datum_bytes| {
                        push_datum(
                            &mut data,
                            &mut seen_data,
                            &mut tls_count,
                            Target { space: Space::Vtable, symbol: vtable_sym },
                            datum_bytes,
                            frame,
                            export_bump,
                            heap,
                        )
                    },
                    |why| box_err(ExportCliError::Decode { symbol: symbol.0, what: "vtable entry", shown: owned_fmt(format_args!("{why}"), frame, heap.alloc()) }, heap),
                )?;
            }
            RowKind::Static => {
                push_datum(
                    &mut data,
                    &mut seen_data,
                    &mut tls_count,
                    Target { space: Space::Static, symbol },
                    rec_bytes,
                    frame,
                    export_bump,
                    heap,
                )?;
                statics::each_vtable_datum(
                    entry_bytes,
                    |vtable_sym, datum_bytes| {
                        push_datum(
                            &mut data,
                            &mut seen_data,
                            &mut tls_count,
                            Target { space: Space::Vtable, symbol: vtable_sym },
                            datum_bytes,
                            frame,
                            export_bump,
                            heap,
                        )
                    },
                    |why| box_err(ExportCliError::Decode { symbol: symbol.0, what: "vtable entry", shown: owned_fmt(format_args!("{why}"), frame, heap.alloc()) }, heap),
                )?;
            }
            RowKind::ThreadLocal => {
                push_datum(
                    &mut data,
                    &mut seen_data,
                    &mut tls_count,
                    Target { space: Space::Tls, symbol },
                    rec_bytes,
                    frame,
                    export_bump,
                    heap,
                )?;
                statics::each_vtable_datum(
                    entry_bytes,
                    |vtable_sym, datum_bytes| {
                        push_datum(
                            &mut data,
                            &mut seen_data,
                            &mut tls_count,
                            Target { space: Space::Vtable, symbol: vtable_sym },
                            datum_bytes,
                            frame,
                            export_bump,
                            heap,
                        )
                    },
                    |why| box_err(ExportCliError::Decode { symbol: symbol.0, what: "vtable entry", shown: owned_fmt(format_args!("{why}"), frame, heap.alloc()) }, heap),
                )?;
            }
        }
    }

    let tls_runtime = match tls_keys {
        Some((getter_key, ender_key)) => {
            for (sym, key, what, sym_name) in [
                (tls_getter_sym, getter_key, "TLS getter", "__blaze_tls_getter"),
                (tls_ender_sym, ender_key, "TLS ender", "__blaze_tls_ender"),
            ] {
                let entry_bytes = entry_of(&codes, key)
                    .map_err(|why| box_err(ExportCliError::Store { symbol: sym.0, shown: owned_fmt(format_args!("{why}"), frame, heap.alloc()) }, heap))?
                    .ok_or_else(|| box_err(ExportCliError::MissingEntry { symbol: sym.0 }, heap))?;
                let rec_bytes = statics::entry_record(entry_bytes)
                    .map_err(|why| box_err(ExportCliError::Decode { symbol: sym.0, what, shown: owned_fmt(format_args!("{why}"), frame, heap.alloc()) }, heap))?;
                let view = CodeView::read(rec_bytes)
                    .map_err(|why| box_err(ExportCliError::Decode { symbol: sym.0, what, shown: owned_fmt(format_args!("{why}"), frame, heap.alloc()) }, heap))?;
                functions.push(Function { symbol: sym, record: view, tier: Tier::Fast });
                symbol_names.push((sym, sym_name));
            }
            Some(TlsRuntime { descriptor: tls_desc_sym, ender: tls_ender_sym })
        }
        None => None,
    };

    let export_profile = (!lto_hot_symbols.is_empty()).then_some(Profile {
        digest: lto_profile_digest,
        hot: lto_hot_symbols.as_slice(),
    });
    let data_count = data.len();
    let program_in = ProgramIn {
        name: picked.bin,
        functions: functions.as_slice(),
        data: data.as_slice(),
        entry,
        panic: export_panic,
        personality,
        tls: tls_runtime,
        profile: export_profile,
        natives: native_export.as_ref().map(|n| &n.image),
        exports: native_export.as_ref().map_or(&[], |n| n.exports.as_slice()),
        symbol_names: symbol_names.as_slice(),
    };

    let mut exe_bytes: AVec<'_, u8> = Vec::new_in(heap.alloc());
    let exported = blaze_export::export(&program_in, platform, &mut exe_bytes, heap.alloc()).map_err(|error| box_err(ExportCliError::Export(error), heap))?;

    // Release immutable borrows of `codes` and ensure any remaining store mutations are committed
    // and rooted (Rule 3: refuse instead of silently dropping).
    drop(functions);
    drop(symbol_names);
    drop(data);
    drop(seen_data);
    if lent.needs_commit() {
        checkpoint_export_roots(
            lent,
            &today,
            builder.toolchain,
            &mut codes,
            &maker.typing().kept,
            dep_typed_program.as_slice(),
            dep_typed_toolchain.as_slice(),
            frame,
            heap,
        )?;
    }
    drop(codes);

    write_executable(cx, fs, output_path, exe_bytes.as_slice(), heap)?;
    let link_ns = phases.since(link_began);
    Ok(ExportSummary {
        output: output_path,
        exported,
        data_count,
        tls_count,
        compiled,
        cached,
        unkeyed,
        stubs,
        lto_bodies,
        lto_inlined,
        lto_hot,
        phase_times: ExportPhaseTimes { front_ns, walk_ns, lto_ns, link_ns },
        shared_front_stats: shared_front.stats(),
        top_stats,
    })
}

/// Why an `-o` / `--output` path was rejected before writing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OutputPathIssue {
    Empty,
    IsDirectory,
    NoSeparator,
    InvalidFileName,
    FilesystemRoot,
    InvalidDirectory,
    DottedDirectory,
    ContainsNulOrTooLong,
}

impl fmt::Display for OutputPathIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OutputPathIssue::Empty => f.write_str("output path is empty"),
            OutputPathIssue::IsDirectory => f.write_str("output path names a directory, not a file"),
            OutputPathIssue::NoSeparator => f.write_str("absolute output path has no separator"),
            OutputPathIssue::InvalidFileName => f.write_str("output file name is invalid"),
            OutputPathIssue::FilesystemRoot => f.write_str("cannot write directly into filesystem root `/`"),
            OutputPathIssue::InvalidDirectory => f.write_str("output directory is invalid"),
            OutputPathIssue::DottedDirectory => f.write_str("output directory contains empty or dotted segment"),
            OutputPathIssue::ContainsNulOrTooLong => f.write_str("path contains NUL or exceeds 4095 bytes"),
        }
    }
}

/// Splits an explicit or default output path into either a relative path beneath `fs.root()`
/// or an absolute `(parent_root, subdir_name, file_name)` triple for `fs.home(...)`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum OutputTarget<'a> {
    Relative(&'a str),
    Absolute { parent: &'a str, dir: &'a str, file: &'a str },
}

fn parse_output_target(raw: &str) -> Result<OutputTarget<'_>, OutputPathIssue> {
    if raw.is_empty() {
        return Err(OutputPathIssue::Empty);
    }
    if raw.ends_with('/') {
        return Err(OutputPathIssue::IsDirectory);
    }
    if raw.starts_with('/') {
        let Some((folder, file)) = raw.rsplit_once('/') else {
            return Err(OutputPathIssue::NoSeparator);
        };
        if file.is_empty() || file == "." || file == ".." {
            return Err(OutputPathIssue::InvalidFileName);
        }
        if folder.is_empty() {
            return Err(OutputPathIssue::FilesystemRoot);
        }
        let Some((parent, dir)) = folder.rsplit_once('/') else {
            return Err(OutputPathIssue::InvalidDirectory);
        };
        let parent = if parent.is_empty() { "/" } else { parent };
        if dir.is_empty() || dir == "." || dir == ".." {
            return Err(OutputPathIssue::DottedDirectory);
        }
        Ok(OutputTarget::Absolute { parent, dir, file })
    } else {
        let mut rel = raw;
        while let Some(rest) = rel.strip_prefix("./") {
            rel = rest;
        }
        if rel.is_empty() || rel == "." {
            return Err(OutputPathIssue::Empty);
        }
        Ok(OutputTarget::Relative(rel))
    }
}

/// Atomically publishes `bytes` to `output_path` through `bstd::fs`, optionally marking the file
/// executable (`0o755`) on real Unix hosts.
fn write_output_file(cx: &Cx, fs: &Fs, output_path: &str, bytes: &[u8], executable: bool, heap: &'static blaze_own::Heap) -> Result<(), CliError> {
    let target = parse_output_target(output_path).map_err(|why| {
        box_err(ExportCliError::BadOutputPath { path: owned_str(output_path, heap.alloc()), why }, heap)
    })?;
    match target {
        OutputTarget::Relative(rel) => {
            let path = Path::new(rel).map_err(|e| box_err(ExportCliError::OutputPath(e), heap))?;
            fs.root()
                .publish(cx, &path, Content::bytes(bytes), Durable::No)
                .map_err(|e| box_err(ExportCliError::WriteOutput(e), heap))?;
        }
        OutputTarget::Absolute { parent, dir, file } => {
            let dir_path = Path::new(dir).map_err(|e| box_err(ExportCliError::OutputPath(e), heap))?;
            let file_path = Path::new(file).map_err(|e| box_err(ExportCliError::OutputPath(e), heap))?;
            let out_dir = fs.home(cx, parent, &dir_path).map_err(|e| box_err(ExportCliError::OutputHome(e), heap))?;
            out_dir
                .publish(cx, &file_path, Content::bytes(bytes), Durable::No)
                .map_err(|e| box_err(ExportCliError::WriteOutput(e), heap))?;
        }
    }
    #[cfg(unix)]
    if executable && !cx.is_simulated() {
        chmod_executable(output_path, heap)?;
    }
    Ok(())
}

/// Atomically publishes `bytes` to `output_path` through `bstd::fs` and marks the resulting file
/// executable (`0o755`) on real Unix hosts.
fn write_executable(cx: &Cx, fs: &Fs, output_path: &str, bytes: &[u8], heap: &'static blaze_own::Heap) -> Result<(), CliError> {
    write_output_file(cx, fs, output_path, bytes, true, heap)
}

/// Marks `output_path` executable (`0o755`) via safe `blaze_sys::posix` calls (`openat` + `fchmod`).
#[cfg(unix)]
fn chmod_executable(output_path: &str, heap: &'static blaze_own::Heap) -> Result<(), CliError> {
    use blaze_sys::posix::io::{CWD, OpenFlags, openat};
    use blaze_sys::posix::proc::change_mode;

    let bytes = output_path.as_bytes();
    let mut buf = [0u8; 4096];
    if bytes.len() >= buf.len() || bytes.contains(&0) {
        return Err(box_err(
            ExportCliError::BadOutputPath { path: owned_str(output_path, heap.alloc()), why: OutputPathIssue::ContainsNulOrTooLong },
            heap,
        ));
    }
    let Some(dst) = buf.get_mut(..bytes.len()) else {
        return Err(box_err(
            ExportCliError::BadOutputPath { path: owned_str(output_path, heap.alloc()), why: OutputPathIssue::ContainsNulOrTooLong },
            heap,
        ));
    };
    dst.copy_from_slice(bytes);
    let Some(with_nul) = buf.get(..=bytes.len()) else {
        return Err(box_err(
            ExportCliError::BadOutputPath { path: owned_str(output_path, heap.alloc()), why: OutputPathIssue::ContainsNulOrTooLong },
            heap,
        ));
    };
    let Ok(cstr) = core::ffi::CStr::from_bytes_with_nul(with_nul) else {
        return Err(box_err(
            ExportCliError::BadOutputPath { path: owned_str(output_path, heap.alloc()), why: OutputPathIssue::ContainsNulOrTooLong },
            heap,
        ));
    };
    let fd = openat(CWD, blaze_sys::Path::new(cstr), OpenFlags::READ.with(OpenFlags::CLOSE_ON_EXEC), 0).map_err(|errno| {
        box_err(ExportCliError::Chmod { path: owned_str(output_path, heap.alloc()), errno }, heap)
    })?;
    change_mode(fd.fd(), 0o755).map_err(|errno| {
        box_err(ExportCliError::Chmod { path: owned_str(output_path, heap.alloc()), errno }, heap)
    })?;
    Ok(())
}

struct StackBuf {
    buf: [u8; 64],
    len: usize,
}

impl StackBuf {
    fn new() -> Self {
        Self { buf: [0u8; 64], len: 0 }
    }

    fn as_str(&self) -> &str {
        core::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
    }
}

impl fmt::Write for StackBuf {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let bytes = s.as_bytes();
        let take = bytes.len().min(self.buf.len().saturating_sub(self.len));
        if let Some(dst) = self.buf.get_mut(self.len..self.len + take)
            && let Some(src) = bytes.get(..take)
        {
            dst.copy_from_slice(src);
            self.len += take;
        }
        Ok(())
    }
}

fn write_ms1(out: &mut impl fmt::Write, ns: u64) -> fmt::Result {
    let tenths = ns.saturating_add(50_000) / 100_000;
    write!(out, "{}.{}", tenths / 10, tenths % 10)
}

fn write_kb1(out: &mut impl fmt::Write, bytes: u64) -> fmt::Result {
    let tenths = bytes.saturating_mul(10).saturating_add(512) / 1024;
    write!(out, "{}.{}", tenths / 10, tenths % 10)
}

fn format_time_col(lower_ns: u64, build_ns: u64, emit_ns: u64) -> StackBuf {
    use core::fmt::Write as _;
    let total = lower_ns.saturating_add(build_ns).saturating_add(emit_ns);
    let mut buf = StackBuf::new();
    let _ = write_ms1(&mut buf, total);
    let _ = buf.write_str("ms (");
    let _ = write_ms1(&mut buf, lower_ns);
    let _ = buf.write_str("/");
    let _ = write_ms1(&mut buf, build_ns);
    let _ = buf.write_str("/");
    let _ = write_ms1(&mut buf, emit_ns);
    let _ = buf.write_str(")");
    buf
}

/// Formats the per-phase wall-clock summary line (and optional peak RSS / footprint in MiB).
fn format_build_phases_line(
    out: &mut impl fmt::Write,
    phase_times: ExportPhaseTimes,
    total_ns: u64,
    peak_rss_bytes: Option<u64>,
) {
    let front_ms = phase_times.front_ns / 1_000_000;
    let walk_ms = phase_times.walk_ns / 1_000_000;
    let lto_ms = phase_times.lto_ns / 1_000_000;
    let link_ms = phase_times.link_ns / 1_000_000;
    let total_ms = total_ns / 1_000_000;
    let _ = write!(
        out,
        "build phases: front={front_ms}ms walk={walk_ms}ms lto={lto_ms}ms link={link_ms}ms total={total_ms}ms"
    );
    if let Some(bytes) = peak_rss_bytes {
        let mb_tenths = bytes.saturating_mul(10).saturating_add(512 * 1024) / (1024 * 1024);
        let _ = write!(out, " peak_rss_mb={}.{}", mb_tenths / 10, mb_tenths % 10);
    }
}

/// Formats the top-10 slowest and top-10 largest compiled function tables for `--stats`.
fn format_top_fn_tables(out: &mut impl fmt::Write, top: &CompileTopStats) {
    if !top.slowest.is_empty() {
        let _ = writeln!(out, "build top-{TOP_FN_LIMIT} slowest compiled fns:");
        let _ = writeln!(
            out,
            "  {:>2}  {:<30}  {:>8}  {:>6}  {:>12}  {}",
            "#", "total_ms (lower/build/emit)", "insts", "blocks", "frame_est_kb", "fn"
        );
        for (idx, entry) in top.slowest.iter().enumerate() {
            let time_buf = format_time_col(entry.lower_ns, entry.build_ns, entry.emit_ns);
            let mut kb_buf = StackBuf::new();
            let _ = write_kb1(&mut kb_buf, entry.frame_estimate);
            let _ = writeln!(
                out,
                "  {:>2}  {:<30}  {:>8}  {:>6}  {:>12}  {}",
                idx + 1,
                time_buf.as_str(),
                entry.insts,
                entry.blocks,
                kb_buf.as_str(),
                entry.name.as_str()
            );
        }
    }
    if !top.largest.is_empty() {
        let _ = writeln!(out, "build top-{TOP_FN_LIMIT} largest compiled fns (by LIR insts / frame_estimate):");
        let _ = writeln!(
            out,
            "  {:>2}  {:>8}  {:>6}  {:>12}  {:<30}  {}",
            "#", "insts", "blocks", "frame_est_kb", "total_ms (lower/build/emit)", "fn"
        );
        for (idx, entry) in top.largest.iter().enumerate() {
            let time_buf = format_time_col(entry.lower_ns, entry.build_ns, entry.emit_ns);
            let mut kb_buf = StackBuf::new();
            let _ = write_kb1(&mut kb_buf, entry.frame_estimate);
            let _ = writeln!(
                out,
                "  {:>2}  {:>8}  {:>6}  {:>12}  {:<30}  {}",
                idx + 1,
                entry.insts,
                entry.blocks,
                kb_buf.as_str(),
                time_buf.as_str(),
                entry.name.as_str()
            );
        }
    }
}

/// Prints the concise one-line summary to `out`, and detailed metrics to `err` when `--stats` is set.
fn print_summary(
    cx: &Cx,
    summary: &ExportSummary<'_>,
    wall_ns: u64,
    stats: bool,
    out: &Out,
    err: &Out,
    frame: &Frame,
) -> Result<(), CliError> {
    let wall_ms = wall_ns / 1_000_000;
    match (summary.tls_count > 0, summary.stubs > 0) {
        (true, true) => out.line(
            cx,
            format_args!(
                "built {} ({} bytes, {} functions, {} stubs, {} data, {} tls, {} ms)",
                summary.output, summary.exported.len, summary.exported.functions, summary.stubs, summary.data_count, summary.tls_count, wall_ms
            ),
        )?,
        (true, false) => out.line(
            cx,
            format_args!(
                "built {} ({} bytes, {} functions, {} data, {} tls, {} ms)",
                summary.output, summary.exported.len, summary.exported.functions, summary.data_count, summary.tls_count, wall_ms
            ),
        )?,
        (false, true) => out.line(
            cx,
            format_args!(
                "built {} ({} bytes, {} functions, {} stubs, {} data, {} ms)",
                summary.output, summary.exported.len, summary.exported.functions, summary.stubs, summary.data_count, wall_ms
            ),
        )?,
        (false, false) => out.line(
            cx,
            format_args!(
                "built {} ({} bytes, {} functions, {} data, {} ms)",
                summary.output, summary.exported.len, summary.exported.functions, summary.data_count, wall_ms
            ),
        )?,
    }
    if stats {
        let erased = Erased::new(frame);
        let mut report = String::new_in(erased.handle());
        let _ = summary.exported.report(&mut report);
        for line in report.as_str().lines() {
            err.line(cx, format_args!("build: {line}"))?;
        }
        err.line(
            cx,
            format_args!(
                "build stats: functions={} folded={} stubs={} data={} tls={} compiled={} cached={} unkeyed={} lto={} inlined={} hot={} bytes={} wall={}ms",
                summary.exported.functions,
                summary.exported.folded,
                summary.stubs,
                summary.data_count,
                summary.tls_count,
                summary.compiled,
                summary.cached,
                summary.unkeyed,
                summary.lto_bodies,
                summary.lto_inlined,
                summary.lto_hot,
                summary.exported.len,
                wall_ms
            ),
        )?;
        let peak_rss_bytes = (!cx.is_simulated())
            .then(blaze_core::own_footprint)
            .flatten()
            .map(|(_, most)| most);
        let mut phase_line = String::new_in(erased.handle());
        format_build_phases_line(&mut phase_line, summary.phase_times, wall_ns, peak_rss_bytes);
        err.line(cx, format_args!("{}", phase_line.as_str()))?;
        let sf = summary.shared_front_stats;
        if sf.head_hits
            + sf.head_misses
            + sf.pure_const_hits
            + sf.pure_const_misses
            + sf.typed_hits
            + sf.typed_misses
            + sf.poly_usage_hits
            + sf.poly_usage_misses
            + sf.poly_generic_calls
            > 0
        {
            err.line(
                cx,
                format_args!(
                    "build shared-front: heads={}/{} (puts={}) const_bits={}/{} pure_consts={}/{} pure_statics={}/{} members={}/{} typed={}/{} (puts={}) poly_usage={}/{} (puts={}) poly_canon={}/{}",
                    sf.head_hits,
                    sf.head_hits + sf.head_misses,
                    sf.head_puts,
                    sf.const_bit_hits,
                    sf.const_bit_hits + sf.const_bit_misses,
                    sf.pure_const_hits,
                    sf.pure_const_hits + sf.pure_const_misses,
                    sf.pure_static_hits,
                    sf.pure_static_hits + sf.pure_static_misses,
                    sf.member_index_hits,
                    sf.member_index_hits + sf.member_index_misses,
                    sf.typed_hits,
                    sf.typed_hits + sf.typed_misses,
                    sf.typed_puts,
                    sf.poly_usage_hits,
                    sf.poly_usage_hits + sf.poly_usage_misses,
                    sf.poly_usage_puts,
                    sf.poly_canonicalized_calls,
                    sf.poly_generic_calls,
                ),
            )?;
        }
        if let Some(top) = summary.top_stats.as_ref() {
            let mut tables = String::new_in(erased.handle());
            format_top_fn_tables(&mut tables, top);
            for line in tables.as_str().lines() {
                err.line(cx, format_args!("{line}"))?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use blaze_code::datum::{Datum, OWN_DATUM, Place as DatumPlace};
    use blaze_code::record::{CodeRecord, CodeView, PositionField, PositionPart, PositionRow, Space, Symbol as CodeSymbol, Target};
    use blaze_own::Erased;
    use bstd::mem::Frame;

    use super::*;

    #[test]
    fn parse_output_target_handles_relative_and_absolute_paths() {
        assert_eq!(parse_output_target("hello"), Ok(OutputTarget::Relative("hello")));
        assert_eq!(parse_output_target("./hello"), Ok(OutputTarget::Relative("hello")));
        assert_eq!(parse_output_target("././out/hello"), Ok(OutputTarget::Relative("out/hello")));
        assert_eq!(
            parse_output_target("/tmp/hello"),
            Ok(OutputTarget::Absolute { parent: "/", dir: "tmp", file: "hello" })
        );
        assert_eq!(
            parse_output_target("/tmp/blaze-out/mybin"),
            Ok(OutputTarget::Absolute { parent: "/tmp", dir: "blaze-out", file: "mybin" })
        );
        assert!(parse_output_target("").is_err());
        assert!(parse_output_target("dir/").is_err());
        assert!(parse_output_target("/").is_err());
        assert!(parse_output_target("/mybin").is_err());
    }

    #[test]
    fn materialize_positionless_patches_line_and_column_slots() {
        let frame = Frame::with_reserve(1 << 20, bstd::mem::Tag::Other).expect("frame");
        let erased = Erased::new(&frame);
        let bump = erased.bump();
        let alloc = bump.alloc();
        let heap = blaze_own::testing::heap();

        let mut rec = CodeRecord::new_in(alloc);
        rec.code.extend_from_slice(&0xD65F_03C0u32.to_le_bytes()); // ret
        rec.relro.extend_from_slice(&[0u8; 8]);
        rec.positions.push(PositionRow {
            part: PositionPart::Relro,
            at: 0,
            field: PositionField::Line,
            item: 0x1234,
            site: 10,
        });
        rec.positions.push(PositionRow {
            part: PositionPart::Relro,
            at: 4,
            field: PositionField::Col,
            item: 0x1234,
            site: 10,
        });
        let sealed = rec.seal().expect("seal");
        let mut encoded = Vec::new_in(alloc);
        sealed.encode(&mut encoded);
        let raw_view = CodeView::read(encoded.as_slice()).expect("read");
        assert_eq!(raw_view.positions(), 2);

        let mut calls = 0usize;
        let sym = CodeSymbol::of_name("test_pos");
        let materialized = materialize_positionless(
            sym,
            raw_view,
            |item, site| {
                calls += 1;
                assert_eq!(item, ItemKey(0x1234));
                assert_eq!(site, 10);
                Ok((42, 7))
            },
            &frame,
            bump,
            heap,
        )
        .expect("materialize");

        assert_eq!(calls, 1, "adjacent position rows at the same site reuse the lookup");
        assert_eq!(materialized.positions(), 0);
        assert_eq!(&materialized.relro()[0..4], &42u32.to_le_bytes());
        assert_eq!(&materialized.relro()[4..8], &7u32.to_le_bytes());
    }

    #[test]
    fn datum_to_export_converts_words_and_places() {
        let frame = Frame::with_reserve(1 << 20, bstd::mem::Tag::Other).expect("frame");
        let erased = Erased::new(&frame);
        let bump = erased.bump();
        let alloc = bump.alloc();
        let heap = blaze_own::testing::heap();

        let mut d = Datum::new_in(DatumPlace::Writable, 3, alloc);
        d.bytes.extend_from_slice(&[0u8; 16]);
        let func_target = Target { space: Space::Func, symbol: CodeSymbol::of_name("f") };
        let idx = d.target(func_target);
        d.words.push(blaze_code::record::Word { at: 0, target: OWN_DATUM, addend: 8 });
        d.words.push(blaze_code::record::Word { at: 8, target: idx, addend: 0 });
        let sealed = d.seal().expect("seal");
        let mut bytes = Vec::new_in(alloc);
        sealed.encode(&mut bytes);
        let view = DatumView::read(bytes.as_slice()).expect("read");

        let target = Target { space: Space::Static, symbol: CodeSymbol::of_name("S") };
        let exported = datum_to_export(target, &view, bump, heap).expect("convert");
        assert_eq!(exported.target, target);
        assert_eq!(exported.align, 8);
        assert_eq!(exported.place, DataPlace::Writable);
        assert_eq!(exported.words.len(), 2);
        assert_eq!(exported.words[0], DataWord { at: 0, target: None, addend: 8 });
        assert_eq!(exported.words[1], DataWord { at: 8, target: Some(func_target), addend: 0 });
    }

    #[test]
    fn code_touches_tls_detects_space_tls_and_getter() {
        let frame = Frame::with_reserve(1 << 20, bstd::mem::Tag::Other).expect("frame");
        let erased = Erased::new(&frame);
        let alloc = erased.handle();
        let getter = CodeSymbol::of_name("tls_getter");
        let desc = CodeSymbol::of_name("tls_desc");

        let mut plain = CodeRecord::new_in(alloc);
        plain.code.extend_from_slice(&0xD65F_03C0u32.to_le_bytes());
        let mut plain_bytes = Vec::new_in(alloc);
        plain.seal().expect("seal").encode(&mut plain_bytes);
        let plain_view = CodeView::read(plain_bytes.as_slice()).expect("read");
        assert!(!code_touches_tls(&plain_view, getter, desc));

        let mut with_tls = CodeRecord::new_in(alloc);
        with_tls.code.extend_from_slice(&0xD65F_03C0u32.to_le_bytes());
        with_tls.target(Target { space: Space::Tls, symbol: CodeSymbol::of_name("var") });
        let mut tls_bytes = Vec::new_in(alloc);
        with_tls.seal().expect("seal").encode(&mut tls_bytes);
        let tls_view = CodeView::read(tls_bytes.as_slice()).expect("read");
        assert!(code_touches_tls(&tls_view, getter, desc));
    }

    #[test]
    fn build_program_shape_is_disjoint_from_run_and_across_workspaces() {
        let app_a = run_package::Program {
            package: "app",
            bin: "app",
            manifest: "Cargo.toml",
            root: "src/main.rs",
            edition: "2024",
        };
        let app_b = run_package::Program {
            package: "app-b",
            bin: "app-b",
            manifest: "Cargo.toml",
            root: "src/main.rs",
            edition: "2024",
        };
        let ws_1 = Some(blaze_ids::ContentId::of("ws test", b"workspace-1"));
        let ws_2 = Some(blaze_ids::ContentId::of("ws test", b"workspace-2"));
        let crate_1 = blaze_id::CrateItemKey(blaze_ids::ContentId::of("crate test", b"crate-1"));
        let crate_2 = blaze_id::CrateItemKey(blaze_ids::ContentId::of("crate test", b"crate-2"));

        let run_shape = blaze_ids::ContentId::of("blaze run: the start list of a program 1", app_a.root.as_bytes());
        assert_ne!(build_program_shape(&app_a, ws_1, crate_1), run_shape, "blaze build root must not overwrite blaze run root");
        assert_ne!(
            build_program_shape(&app_a, ws_1, crate_1),
            build_program_shape(&app_a, ws_2, crate_1),
            "unrelated workspaces with identical package/bin/root must not clobber each other"
        );
        assert_ne!(
            build_program_shape(&app_a, ws_1, crate_1),
            build_program_shape(&app_a, ws_1, crate_2),
            "unrelated single-crate projects with different crate identity must not collide"
        );
        assert_ne!(
            build_program_shape(&app_a, ws_1, crate_1),
            build_program_shape(&app_b, ws_1, crate_1),
            "distinct packages with src/main.rs must not collide"
        );
        assert_eq!(
            build_program_shape(&app_a, ws_1, crate_1),
            build_program_shape(&app_a, ws_1, crate_1),
            "same program across worktrees shares one root"
        );
    }

    #[test]
    fn export_keep_program_roots_non_toolchain_code_across_gc_and_control_sweeps_it() {
        use core::time::Duration;
        use blaze_cas::Key as CasKey;
        use blaze_cas::plain::{GRACE_NS, LEASE_NS, Liveness, Settings as StoreSettings};
        use blaze_core::FileName;
        use blaze_core::sim::{Faults, Sim};
        use blaze_ids::ContentId;
        use blaze_keyed::kind::{Namespace, ReservedTag};
        use blaze_ladder::code_records::{CodeClass, CodeStore};
        use crate::store::plain_place::{Facts, after_answer, config, open, test_cx};

        let run_history = |root_program: bool| -> (usize, usize, bool) {
            let alloc = blaze_own::testing::heap().alloc();
            let open_proc = |sim: Sim, nonce: u64| {
                let mut cfg = config(FileName::new("store").expect("dir"), &StoreSettings::default(), Facts { boot: Some(1), nonce }, alloc);
                cfg.liveness = Liveness::Reached;
                open(sim, cfg, alloc).expect("open store")
            };
            let code_key = |tag: &'static str, n: u32| CodeKey(ContentId::of(tag, &n.to_le_bytes()));
            let payload = |n: u32| -> std::vec::Vec<u8> {
                (0..4096).map(|i| ((n as usize).wrapping_mul(131).wrapping_add(i) & 0xff) as u8).collect()
            };
            let content_key = |bytes: &[u8]| CodeKey(ContentId::of("export gc test content", bytes));

            let picked = run_package::Program {
                package: "tieline-registryd",
                bin: "tieline-registryd",
                manifest: "crates/tieline-registryd/Cargo.toml",
                root: "crates/tieline-registryd/src/main.rs",
                edition: "2024",
            };
            let ws_id = Some(ContentId::of("export gc test ws", b"tieline"));
            let crate_key = blaze_id::CrateItemKey(ContentId::of("export gc test crate", b"tieline-registryd"));
            let ids_table = CasKey(ContentId::of("export gc test run_ids", b"table"));
            let today = crate::start_list::Today {
                certificate: crate::start_list::Certificate::for_tests([0x42; 32]),
                name: crate::start_list::ListName::of(&build_program_shape(&picked, ws_id, crate_key)),
                tables: [ids_table, ids_table],
            };

            let sim = Sim::new(77, Faults::default());
            let lent = open_proc(sim, 1);
            lent.put(&ids_table, Namespace::LOCAL, ReservedTag::Drive.tag(), b"run-ids-table-bytes").expect("put ids table");
            let mut codes = HomeCodeStore::new(&lent);
            for i in 0..8u32 {
                let bytes = payload(i);
                let ck = content_key(&bytes);
                codes.put(CodeClass::Program, ck, &bytes).expect("put prog");
                codes.point(CodeClass::Program, code_key("export prog key", i), ck).expect("point prog");
            }
            for i in 100..104u32 {
                let bytes = payload(i);
                let ck = content_key(&bytes);
                codes.put(CodeClass::Toolchain, ck, &bytes).expect("put tool");
                codes.point(CodeClass::Toolchain, code_key("export tool key", i), ck).expect("point tool");
            }
            let walked = codes.reached();
            let tool_made = codes.toolchain_made();
            drop(codes);
            if root_program {
                let kept = keep_export_roots(&lent, &today, blaze_toolchain::PINNED, &walked, &tool_made);
                kept.program.expect("program root");
                kept.toolchain.expect("toolchain root");
            } else {
                // Control (rule 23): pre-fix behavior that only called `keep_toolchain`.
                crate::store::roots::keep_toolchain(&lent, blaze_toolchain::PINNED, &tool_made, &[]).expect("toolchain root");
            }
            after_answer(&test_cx(), &lent, 0).expect("commit export");
            let mut sim = lent.into_store().into_driver();

            // Run GC compaction slices past lease + grace.
            sim.advance(Duration::from_nanos(LEASE_NS.max(GRACE_NS)) + Duration::from_secs(60));
            let gc = open_proc(sim, 2);
            let _ = gc.compact_foreground(32).expect("gc compact");
            let mut sim = gc.into_store().into_driver();
            sim.advance(Duration::from_nanos(GRACE_NS) + Duration::from_secs(60));
            let gc2 = open_proc(sim, 3);
            let _ = gc2.compact_foreground(32).expect("gc2 compact");
            let sim = gc2.into_store().into_driver();

            // Next `blaze build` probes all entries and the RunIds table.
            let verify = open_proc(sim, 4);
            let codes = HomeCodeStore::new(&verify);
            let mut prog_hits = 0usize;
            for i in 0..8u32 {
                if entry_of(&codes, code_key("export prog key", i)).expect("read prog") == Some(payload(i).as_slice()) {
                    prog_hits += 1;
                }
            }
            let mut tool_hits = 0usize;
            for i in 100..104u32 {
                if entry_of(&codes, code_key("export tool key", i)).expect("read tool") == Some(payload(i).as_slice()) {
                    tool_hits += 1;
                }
            }
            let has_ids_table = verify.get(&ids_table).expect("read ids table").is_some();
            (prog_hits, tool_hits, has_ids_table)
        };

        let (prog_hits, tool_hits, has_ids_table) = run_history(true);
        assert_eq!(prog_hits, 8, "keep_export_roots must keep all 8 non-toolchain program code entries across GC");
        assert_eq!(tool_hits, 4, "keep_export_roots must keep all 4 toolchain entries across GC");
        assert!(has_ids_table, "keep_export_roots must keep the RunIds table across GC");

        let (ctrl_prog, ctrl_tool, ctrl_ids) = run_history(false);
        assert_eq!(ctrl_prog, 0, "control without keep_program must sweep all unrooted program code entries");
        assert_eq!(ctrl_tool, 4, "control still keeps toolchain entries");
        assert!(!ctrl_ids, "control without keep_program must sweep the unrooted RunIds table");
    }

    #[test]
    fn keep_roots_error_formats_class_and_cause() {
        let heap = blaze_own::testing::heap();
        let err = ExportCliError::KeepRoots {
            class: "program code",
            shown: owned_str("disk full", heap.alloc()),
        };
        let msg = std::format!("{err}");
        assert!(msg.contains("keeping program code root failed"), "{msg}");
        assert!(msg.contains("disk full"), "{msg}");
    }

    #[test]
    fn cli_lto_scheduler_dst_interleavings_failures_and_oracle() {
        use core::sync::atomic::AtomicUsize;

        let shared = blaze_own::testing::heap().shared();
        blaze_sim::check(blaze_sim::Config::cases(100), |tape| {
            let worker_count = usize::try_from(tape.int_in(1..=8u64, "workers")).expect("workers");
            let count = usize::try_from(tape.int_in(0..=64u64, "count")).expect("count");
            let inject_worker_err = tape.coin("inject worker error");
            let inject_scope_err = tape.coin("inject scope error");
            let fail_worker = if inject_worker_err {
                Some(usize::try_from(tape.int_in(0..=u64::try_from(worker_count - 1).expect("w"), "fail worker")).expect("fw"))
            } else {
                None
            };
            let fail_after_steps = usize::try_from(tape.int_in(0..=4u64, "fail after steps")).expect("steps");

            let batch = lto_batch_size(count, worker_count.min(count.max(1)));
            let next = bstd::sync::CachePadded::new(AtomicUsize::new(0));
            let eval_item = |idx: usize| -> u64 {
                let x = u64::try_from(idx).expect("idx");
                x.wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(13) ^ x.wrapping_add(17)
            };

            // Sequential oracle.
            let mut oracle: Vec<u64, SharedAlloc<'static>> = Vec::with_capacity_in(count, shared);
            for idx in 0..count {
                oracle.push(eval_item(idx));
            }

            // Simulate concurrent step-by-step interleavings across `worker_count` workers sharing
            // the production atomic `steal_next_batch` cursor.
            struct SimWorker {
                pending: Option<(usize, usize)>,
                done: bool,
                steps: usize,
                failed: Option<&'static str>,
                out: Vec<(usize, u64), SharedAlloc<'static>>,
            }
            let mut workers: Vec<SimWorker, SharedAlloc<'static>> = Vec::with_capacity_in(worker_count, shared);
            for _ in 0..worker_count {
                workers.push(SimWorker {
                    pending: None,
                    done: false,
                    steps: 0,
                    failed: None,
                    out: Vec::new_in(shared),
                });
            }
            let mut hits: Vec<usize, SharedAlloc<'static>> = Vec::with_capacity_in(count, shared);
            for _ in 0..count {
                hits.push(0);
            }

            loop {
                let mut active: Vec<usize, SharedAlloc<'static>> = Vec::with_capacity_in(worker_count, shared);
                for (w_idx, w) in workers.iter().enumerate() {
                    if !w.done {
                        active.push(w_idx);
                    }
                }
                if active.is_empty() {
                    break;
                }
                let pick_pos = usize::try_from(
                    tape.int_in(0..=u64::try_from(active.len() - 1).expect("active"), "pick worker"),
                )
                .expect("pick");
                let w_idx = active[pick_pos];
                let w = &mut workers[w_idx];

                if fail_worker == Some(w_idx) && w.steps >= fail_after_steps {
                    w.failed = Some("injected worker cancellation");
                    w.done = true;
                    continue;
                }

                if let Some((cur, end)) = w.pending {
                    hits[cur] += 1;
                    w.out.push((cur, eval_item(cur)));
                    w.steps += 1;
                    if cur + 1 < end {
                        w.pending = Some((cur + 1, end));
                    } else {
                        w.pending = None;
                    }
                } else if let Some((start, end)) = steal_next_batch(&next, batch, count) {
                    w.pending = Some((start, end));
                } else {
                    w.done = true;
                }
            }

            // Shuffle worker completion order before merging through `collect_lto_worker_parts`.
            let mut order: Vec<usize, SharedAlloc<'static>> = Vec::with_capacity_in(worker_count, shared);
            for i in 0..worker_count {
                order.push(i);
            }
            for i in (1..order.len()).rev() {
                let j = usize::try_from(tape.int_in(0..=u64::try_from(i).expect("i"), "shuffle")).expect("j");
                order.swap(i, j);
            }

            let mut any_failed = inject_scope_err;
            let mut raw_parts: Vec<Option<Result<Vec<(usize, u64), SharedAlloc<'static>>, &'static str>>, SharedAlloc<'static>> =
                Vec::with_capacity_in(worker_count, shared);
            for w in workers {
                if let Some(err) = w.failed {
                    any_failed = true;
                    raw_parts.push(Some(Err(err)));
                } else {
                    raw_parts.push(Some(Ok(w.out)));
                }
            }
            let mut shuffled_parts: Vec<Result<Vec<(usize, u64), SharedAlloc<'static>>, &'static str>, SharedAlloc<'static>> =
                Vec::with_capacity_in(worker_count, shared);
            for idx in order {
                if let Some(part) = raw_parts[idx].take() {
                    shuffled_parts.push(part);
                }
            }

            let got = collect_lto_worker_parts(count, shuffled_parts, inject_scope_err, shared);
            if any_failed {
                assert!(
                    got.is_err(),
                    "expected worker/scope failure to propagate (worker_err={inject_worker_err}, scope_err={inject_scope_err})"
                );
            } else {
                let got = got.expect("clean concurrent schedule must succeed");
                assert_eq!(got.as_slice(), oracle.as_slice(), "merged output must match sequential oracle");
                for (idx, &h) in hits.iter().enumerate() {
                    assert_eq!(h, 1, "index {idx} must be executed exactly once, got {h}");
                }
            }
        });
    }

    #[test]
    fn cli_lto_scheduler_dst_catches_planted_bugs() {
        let shared = blaze_own::testing::heap().shared();
        let oracle: [u64; 4] = [10, 20, 30, 40];

        // Planted bug 1: skipping index-order restoration (concatenating shuffled worker outputs).
        let mut p0 = Vec::new_in(shared);
        p0.push((2usize, 30u64));
        p0.push((3usize, 40u64));
        let mut p1 = Vec::new_in(shared);
        p1.push((0usize, 10u64));
        p1.push((1usize, 20u64));
        let mut buggy_concat: Vec<u64, SharedAlloc<'static>> = Vec::new_in(shared);
        for &(_, v) in p0.iter().chain(p1.iter()) {
            buggy_concat.push(v);
        }
        assert_ne!(
            buggy_concat.as_slice(),
            &oracle,
            "Rule 23 control: oracle must catch unordered worker concatenation"
        );
        let mut good_parts = Vec::new_in(shared);
        good_parts.push(Ok(p0));
        good_parts.push(Ok(p1));
        assert_eq!(
            collect_lto_worker_parts(4, good_parts, false, shared).expect("valid").as_slice(),
            &oracle
        );

        // Planted bug 2: duplicate index emitted by two workers must be refused.
        let mut d0 = Vec::new_in(shared);
        d0.push((0usize, 10u64));
        d0.push((1usize, 20u64));
        let mut d1 = Vec::new_in(shared);
        d1.push((1usize, 20u64));
        d1.push((2usize, 30u64));
        d1.push((3usize, 40u64));
        let mut dup_parts = Vec::new_in(shared);
        dup_parts.push(Ok(d0));
        dup_parts.push(Ok(d1));
        assert!(
            collect_lto_worker_parts(4, dup_parts, false, shared).is_err(),
            "Rule 23 control: duplicate index must be caught"
        );

        // Planted bug 3: dropped index must be refused.
        let mut m0 = Vec::new_in(shared);
        m0.push((0usize, 10u64));
        m0.push((1usize, 20u64));
        let mut m1 = Vec::new_in(shared);
        m1.push((3usize, 40u64));
        let mut missing_parts = Vec::new_in(shared);
        missing_parts.push(Ok(m0));
        missing_parts.push(Ok(m1));
        assert!(
            collect_lto_worker_parts(4, missing_parts, false, shared).is_err(),
            "Rule 23 control: missing index must be caught"
        );

        // Planted bug 4: worker cancellation/failure must never be swallowed.
        let mut f0 = Vec::new_in(shared);
        f0.push((0usize, 10u64));
        let mut err_parts = Vec::new_in(shared);
        err_parts.push(Ok(f0));
        err_parts.push(Err("export LTO worker stopped"));
        assert!(
            collect_lto_worker_parts(4, err_parts, false, shared).is_err(),
            "Rule 23 control: worker failure must propagate"
        );
    }

    #[test]
    fn cli_lto_runner_parallel_workers_match_sequential_lto() {
        use blaze_ladder::run::make_one::export_lto::ExportLtoRunner;
        use blaze_lir::{
            BlockKind, Flags, Op, Ty, View,
            body::{Bodies, BodyId, Target},
            builder::{Builder, Full, Param, Variadic},
            func::{Conv, RefId, RefKind},
            inline::{InlinePolicy, NoWeights},
            lto::{ModuleBody, lto_optimize_module, lto_optimize_module_par},
            verify,
        };
        use bstd::io::{Io, Limits, SimWorld};
        use core::num::NonZeroU32;
        use core::sync::atomic::{AtomicUsize, Ordering};

        let shared = blaze_own::testing::heap().shared();
        let alloc = Alloc::from(shared);
        let io = Io::simulated(SimWorld::new(101, Limits::USUAL));
        let workers = NonZeroU32::new(4).expect("4 workers");

        io.with_test_workers(workers, |cx| {
            assert_eq!(cx.workers(), 4, "test pool must run with 4 real workers");
            let runner = CliLtoRunner { cx, workers };

            // 1. Direct `CliLtoRunner::par_map` across serial threshold (<4) and parallel (>=4)
            //    batches, exercising scratch bump allocations and verifying exact-once execution.
            for &count in &[0usize, 1, 3, 4, 15, 64, 137] {
                let mut hit_vec: Vec<AtomicUsize, SharedAlloc<'static>> = Vec::with_capacity_in(count, shared);
                for _ in 0..count {
                    hit_vec.push(AtomicUsize::new(0));
                }
                let hits = hit_vec.as_slice();
                let got = runner
                    .par_map(count, shared, |idx, scratch| {
                        hits[idx].fetch_add(1, Ordering::Relaxed);
                        // Exercise per-worker scratch frame reset/allocation.
                        let mut tmp: Vec<u64, Alloc<'_>> = Vec::with_capacity_in(16, scratch);
                        for k in 0..16u64 {
                            tmp.push(u64::try_from(idx).expect("idx").wrapping_mul(31).wrapping_add(k));
                        }
                        tmp.iter().copied().fold(0u64, u64::wrapping_add)
                    })
                    .expect("par_map succeeds");
                assert_eq!(got.len(), count);
                for idx in 0..count {
                    assert_eq!(hits[idx].load(Ordering::Relaxed), 1, "count={count} idx={idx}");
                    let mut want = 0u64;
                    for k in 0..16u64 {
                        want = want.wrapping_add(u64::try_from(idx).expect("idx").wrapping_mul(31).wrapping_add(k));
                    }
                    assert_eq!(got[idx], want, "count={count} idx={idx}");
                }
            }

            // 2. End-to-end `lto_optimize_module_par` running on real multi-threaded `CliLtoRunner`
            //    across an 8-body multi-level DAG (forcing parallel pre-simplify AND parallel
            //    wavefront SCC inlining with >=4 SCCs in a level) vs `lto_optimize_module`.
            let leaf_ids: [BodyId; 4] = [
                BodyId([31u8; 32]),
                BodyId([32u8; 32]),
                BodyId([33u8; 32]),
                BodyId([34u8; 32]),
            ];
            let root_ids: [BodyId; 4] = [
                BodyId([41u8; 32]),
                BodyId([42u8; 32]),
                BodyId([43u8; 32]),
                BodyId([44u8; 32]),
            ];

            let mut leaf_funcs = Vec::with_capacity_in(4, shared);
            for i in 0..4u64 {
                let mut b = Builder::<_, Full>::new_in(64, alloc);
                let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
                b.set_signature(sig);
                let entry = b.create_block(BlockKind::Normal);
                b.begin(entry);
                let x = b.param(Ty::I64);
                let c = b.iconst(Ty::I64, u128::from(i + 3));
                let y = b.binary(Op::Add, Flags::NONE, x, c);
                b.ret(&[y]);
                leaf_funcs.push(b.finish_checked().expect("leaf").view().to_func_in(shared));
            }

            let mut root_funcs = Vec::with_capacity_in(4, shared);
            let mut root_refs: [(RefId, RefId); 4] = [(RefId(0), RefId(0)); 4];
            for i in 0..4usize {
                let mut b = Builder::<_, Full>::new_in(64, alloc);
                let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
                b.set_signature(sig);
                let r0 = b.reference(RefKind::Func, 0);
                let r1 = b.reference(RefKind::Func, 1);
                let entry = b.create_block(BlockKind::Normal);
                b.begin(entry);
                let x = b.param(Ty::I64);
                let a = b.call(Flags::NONE, r0, sig, &[x]);
                let bb = b.call(Flags::NONE, r1, sig, &[a]);
                b.ret(&[bb]);
                root_refs[i] = (r0, r1);
                root_funcs.push(b.finish_checked().expect("root").view().to_func_in(shared));
            }

            struct EightBodyBank<'a> {
                leaf_ids: [BodyId; 4],
                root_ids: [BodyId; 4],
                root_refs: [(RefId, RefId); 4],
                leaf_views: [View<'a>; 4],
            }

            impl Bodies for EightBodyBank<'_> {
                fn callee(&self, from: BodyId, reference: RefId) -> Option<(BodyId, View<'_>)> {
                    for i in 0..4 {
                        if from == self.root_ids[i] {
                            let (r0, r1) = self.root_refs[i];
                            if reference == r0 {
                                return Some((self.leaf_ids[i], self.leaf_views[i]));
                            }
                            if reference == r1 {
                                let next = (i + 1) % 4;
                                return Some((self.leaf_ids[next], self.leaf_views[next]));
                            }
                        }
                    }
                    None
                }

                fn target(&self, from: BodyId, reference: RefId) -> Option<Target> {
                    self.callee(from, reference).map(|(id, _)| Target::Func(id))
                }
            }

            let bank = EightBodyBank {
                leaf_ids,
                root_ids,
                root_refs,
                leaf_views: [
                    leaf_funcs[0].view(),
                    leaf_funcs[1].view(),
                    leaf_funcs[2].view(),
                    leaf_funcs[3].view(),
                ],
            };

            let mut seq_mod = Vec::with_capacity_in(8, alloc);
            let mut par_mod = Vec::with_capacity_in(8, shared);
            for i in 0..4 {
                seq_mod.push(ModuleBody { id: root_ids[i], func: root_funcs[i].view().to_func_in(alloc) });
                par_mod.push(ModuleBody { id: root_ids[i], func: root_funcs[i].view().to_func_in(shared) });
            }
            for i in 0..4 {
                seq_mod.push(ModuleBody { id: leaf_ids[i], func: leaf_funcs[i].view().to_func_in(alloc) });
                par_mod.push(ModuleBody { id: leaf_ids[i], func: leaf_funcs[i].view().to_func_in(shared) });
            }

            let policy = InlinePolicy::default();
            let seq = lto_optimize_module(seq_mod, &bank, &NoWeights, policy, alloc).expect("seq lto");
            let par = lto_optimize_module_par(par_mod, &bank, &NoWeights, policy, shared, &runner).expect("par lto");

            assert_eq!(par.scc_count, seq.scc_count);
            assert_eq!(par.total_inlined_bodies, seq.total_inlined_bodies);
            assert!(par.total_inlined_bodies >= 8, "expected all 8 root->leaf calls inlined");
            assert_eq!(par.bodies.len(), seq.bodies.len());
            for (p, s) in par.bodies.iter().zip(seq.bodies.iter()) {
                assert_eq!(p.id, s.id);
                assert_eq!(p.summary.summary_hash, s.summary.summary_hash);
                assert_eq!(p.prev_summary.summary_hash, s.prev_summary.summary_hash);
                assert_eq!(p.read.as_slice(), s.read.as_slice());
                assert_eq!(p.imported_refs.as_slice(), s.imported_refs.as_slice());
                assert_eq!(p.func.view().ops, s.func.view().ops);
                assert_eq!(p.func.view().data, s.func.view().data);
                assert_eq!(p.func.view().extra, s.func.view().extra);
                verify(p.func.view()).expect("verified post-LTO IR");
            }
        });
    }

    #[test]
    fn top_slow_and_large_bounds_eviction_and_tie_breaking() {
        let shared = blaze_own::testing::heap().shared();
        let mut top = CompileTopStats::new_in(shared);

        // Insert 25 functions with distinct compile times and LIR sizes.
        for i in 0..25u64 {
            let name = std::format!("crate_a::fn_{i:02}");
            top.record_key(FnCompileKey {
                name: name.as_str(),
                lower_ns: i * 1_000_000,
                build_ns: i * 2_000_000,
                emit_ns: i * 500_000,
                insts: (24 - i) * 100,
                blocks: (24 - i) * 5,
                frame_estimate: (24 - i) * 1024,
                entry_bytes: 512,
            });
        }
        assert_eq!(top.slowest.len(), TOP_FN_LIMIT);
        assert_eq!(top.largest.len(), TOP_FN_LIMIT);
        // Slowest: fn_24 down to fn_15.
        for (rank, entry) in top.slowest.iter().enumerate() {
            let want = 24 - u64::try_from(rank).expect("rank");
            assert_eq!(entry.name.as_str(), std::format!("crate_a::fn_{want:02}"));
            assert_eq!(entry.key().total_ns(), want * 3_500_000);
        }
        // Largest: fn_00 down to fn_09.
        for (rank, entry) in top.largest.iter().enumerate() {
            let want = u64::try_from(rank).expect("rank");
            assert_eq!(entry.name.as_str(), std::format!("crate_a::fn_{want:02}"));
            assert_eq!(entry.insts, (24 - want) * 100);
        }

        // Tie-breaking (Rule 25): 12 functions with identical total_ns and insts, inserted in
        // reverse alphabetical order; top-10 must keep the lexicographically first 10 names.
        let mut tied = CompileTopStats::new_in(shared);
        for c in (b'a'..=b'l').rev() {
            let name = std::format!("tied::fn_{}", c as char);
            tied.record_key(FnCompileKey {
                name: name.as_str(),
                lower_ns: 1_000_000,
                build_ns: 2_000_000,
                emit_ns: 3_000_000,
                insts: 500,
                blocks: 10,
                frame_estimate: 8192,
                entry_bytes: 1024,
            });
        }
        assert_eq!(tied.slowest.len(), TOP_FN_LIMIT);
        assert_eq!(tied.largest.len(), TOP_FN_LIMIT);
        for (rank, c) in (b'a'..=b'j').enumerate() {
            let want = std::format!("tied::fn_{}", c as char);
            assert_eq!(tied.slowest[rank].name.as_str(), want);
            assert_eq!(tied.largest[rank].name.as_str(), want);
        }

        // Secondary size tie-breaker: equal `insts`, higher `frame_estimate` ranks first.
        let mut frame_tie = CompileTopStats::new_in(shared);
        frame_tie.record_key(FnCompileKey {
            name: "a::small_frame",
            lower_ns: 10,
            build_ns: 10,
            emit_ns: 10,
            insts: 100,
            blocks: 4,
            frame_estimate: 2048,
            entry_bytes: 256,
        });
        frame_tie.record_key(FnCompileKey {
            name: "z::huge_frame",
            lower_ns: 10,
            build_ns: 10,
            emit_ns: 10,
            insts: 100,
            blocks: 4,
            frame_estimate: 65536,
            entry_bytes: 256,
        });
        assert_eq!(frame_tie.largest[0].name.as_str(), "z::huge_frame");
        assert_eq!(frame_tie.largest[1].name.as_str(), "a::small_frame");
    }

    #[test]
    fn top_slow_parallel_worker_merge_matches_sequential_for_all_schedules() {
        let shared = blaze_own::testing::heap().shared();
        blaze_sim::check(blaze_sim::Config::cases(80), |tape| {
            let worker_count = usize::try_from(tape.int_in(1..=8u64, "workers")).expect("workers");
            let fn_count = usize::try_from(tape.int_in(0..=64u64, "fns")).expect("fns");

            let mut seq = CompileTopStats::new_in(shared);
            let mut workers: Vec<CompileTopStats, SharedAlloc<'static>> = Vec::with_capacity_in(worker_count, shared);
            for _ in 0..worker_count {
                workers.push(CompileTopStats::new_in(shared));
            }

            for i in 0..fn_count {
                let w = usize::try_from(tape.int_in(0..=u64::try_from(worker_count - 1).expect("w"), "owner")).expect("owner");
                // Deliberately small value domains to force frequent ties across workers.
                let lower_ns = tape.int_in(0..=6u64, "lower") * 1_000_000;
                let build_ns = tape.int_in(0..=6u64, "build") * 1_000_000;
                let emit_ns = tape.int_in(0..=6u64, "emit") * 1_000_000;
                let insts = tape.int_in(1..=20u64, "insts") * 50;
                let blocks = tape.int_in(1..=10u64, "blocks");
                let frame_estimate = tape.int_in(1..=10u64, "frame") * 1024;
                let name = std::format!("mod_{}::fn_{i:02}", i % 5);
                let key = FnCompileKey {
                    name: name.as_str(),
                    lower_ns,
                    build_ns,
                    emit_ns,
                    insts,
                    blocks,
                    frame_estimate,
                    entry_bytes: 512,
                };
                seq.record_key(key);
                workers[w].record_key(key);
            }

            // Shuffle worker join order.
            let mut order: Vec<usize, SharedAlloc<'static>> = Vec::with_capacity_in(worker_count, shared);
            for w in 0..worker_count {
                order.push(w);
            }
            for i in (1..order.len()).rev() {
                let j = usize::try_from(tape.int_in(0..=u64::try_from(i).expect("i"), "shuffle")).expect("j");
                order.swap(i, j);
            }

            let mut opt_workers: Vec<Option<CompileTopStats>, SharedAlloc<'static>> = Vec::with_capacity_in(worker_count, shared);
            for w in workers {
                assert!(w.slowest.len() <= TOP_FN_LIMIT);
                assert!(w.largest.len() <= TOP_FN_LIMIT);
                opt_workers.push(Some(w));
            }
            let mut merged = CompileTopStats::new_in(shared);
            for w in order {
                if let Some(part) = opt_workers[w].take() {
                    merged.merge(part);
                }
            }

            assert_eq!(merged.slowest, seq.slowest, "merged top-10 slowest must match sequential oracle");
            assert_eq!(merged.largest, seq.largest, "merged top-10 largest must match sequential oracle");
        });
    }

    #[test]
    fn top_slow_format_phases_and_tables_renders_expected_columns() {
        let frame = Frame::with_reserve(1 << 20, bstd::mem::Tag::Other).expect("frame");
        let erased = Erased::new(&frame);
        let shared = blaze_own::testing::heap().shared();

        let mut phase_line = String::new_in(erased.handle());
        format_build_phases_line(
            &mut phase_line,
            ExportPhaseTimes {
                front_ns: 42_000_000,
                walk_ns: 318_000_000,
                lto_ns: 85_000_000,
                link_ns: 19_000_000,
            },
            466_000_000,
            Some(184 * 1024 * 1024 + 205 * 1024),
        );
        assert_eq!(
            phase_line.as_str(),
            "build phases: front=42ms walk=318ms lto=85ms link=19ms total=466ms peak_rss_mb=184.2"
        );

        let mut no_rss = String::new_in(erased.handle());
        format_build_phases_line(
            &mut no_rss,
            ExportPhaseTimes { front_ns: 1_000_000, walk_ns: 2_000_000, lto_ns: 3_000_000, link_ns: 4_000_000 },
            10_000_000,
            None,
        );
        assert_eq!(
            no_rss.as_str(),
            "build phases: front=1ms walk=2ms lto=3ms link=4ms total=10ms"
        );

        let mut top = CompileTopStats::new_in(shared);
        let mut empty_tables = String::new_in(erased.handle());
        format_top_fn_tables(&mut empty_tables, &top);
        assert!(empty_tables.as_str().is_empty());

        top.record_key(FnCompileKey {
            name: "my_crate::big_parser::parse_expr",
            lower_ns: 10_100_000,
            build_ns: 25_400_000,
            emit_ns: 6_800_000,
            insts: 12_450,
            blocks: 312,
            frame_estimate: 145_920, // 142.5 KB
            entry_bytes: 40_960,
        });
        let mut tables = String::new_in(erased.handle());
        format_top_fn_tables(&mut tables, &top);
        let rendered = tables.as_str();
        assert!(rendered.contains("build top-10 slowest compiled fns:"), "{rendered}");
        assert!(rendered.contains("build top-10 largest compiled fns (by LIR insts / frame_estimate):"), "{rendered}");
        assert!(rendered.contains("42.3ms (10.1/25.4/6.8)"), "{rendered}");
        assert!(rendered.contains("12450"), "{rendered}");
        assert!(rendered.contains("312"), "{rendered}");
        assert!(rendered.contains("142.5"), "{rendered}");
        assert!(rendered.contains("my_crate::big_parser::parse_expr"), "{rendered}");
    }

    #[test]
    fn export_scheduler_dst_eliminates_starvation_under_skewed_giant_workloads_and_control_catches_it() {
        let heap = blaze_own::testing::heap();
        let alloc = heap.alloc();

        // 1. Randomized blaze-sim DST across worker counts, skewed `tabled` keys, and arbitrary
        //    completion orders: verifies exact-once execution and zero starvation whenever work is
        //    queued.
        blaze_sim::check(blaze_sim::Config::cases(80), |tape| {
            let workers = usize::try_from(tape.int_in(2..=12u64, "workers")).expect("workers");
            let total_jobs = usize::try_from(tape.int_in(1..=96u64, "jobs")).expect("jobs");
            let hot_keys = usize::try_from(tape.int_in(1..=4u64, "hot_keys")).expect("hot_keys");
            let cap = 6usize;
            let low_water = 2usize;

            let mut sched: ExportScheduler<'_, usize> = ExportScheduler::new_in(workers, cap, low_water, alloc);
            let mut running: Vec<Vec<usize, Alloc<'static>>, Alloc<'static>> = Vec::with_capacity_in(workers, alloc);
            for _ in 0..workers {
                running.push(Vec::new_in(alloc));
            }
            let mut completed: Vec<usize, Alloc<'static>> = Vec::with_capacity_in(total_jobs, alloc);
            completed.resize(total_jobs, 0);

            let mut next_job = 0usize;
            while next_job < total_jobs || sched.in_flight() > 0 || sched.queued() > 0 {
                // Enqueue a random burst of jobs (strongly skewed toward `hot_keys`).
                let burst = usize::try_from(tape.int_in(1..=8u64, "burst")).expect("burst");
                for _ in 0..burst {
                    if next_job >= total_jobs || sched.in_flight() >= sched.max_in_flight() {
                        break;
                    }
                    let skew = tape.coin("skewed");
                    let tabled = if skew {
                        let k = tape.int_in(0..=u64::try_from(hot_keys - 1).expect("hk"), "hot_k");
                        ItemKey(u128::from(k) + 1)
                    } else {
                        ItemKey(u128::from(u64::try_from(next_job).expect("nj")) + 10_000)
                    };
                    let job_id = next_job;
                    next_job += 1;
                    if let Some((w, id)) = sched.enqueue(tabled, job_id) {
                        running[w].push(id);
                    }
                }
                while let Some((w, id)) = sched.next_refill() {
                    running[w].push(id);
                }

                // Core zero-starvation invariant: whenever queued work exists, NO worker may sit
                // below `low_water`.
                if sched.queued() > 0 {
                    for w in 0..workers {
                        assert!(
                            running[w].len() >= low_water,
                            "starved worker {w} has {} < {low_water} while {} jobs queued",
                            running[w].len(),
                            sched.queued()
                        );
                    }
                }

                // Complete one job from a randomly chosen non-empty worker.
                let mut busy: Vec<usize, Alloc<'static>> = Vec::with_capacity_in(workers, alloc);
                for w in 0..workers {
                    if !running[w].is_empty() {
                        busy.push(w);
                    }
                }
                if busy.is_empty() {
                    break;
                }
                let pick = usize::try_from(tape.int_in(0..=u64::try_from(busy.len() - 1).expect("b"), "done_w")).expect("pick");
                let w = busy[pick];
                let finished = running[w].remove_shift(0);
                completed[finished] += 1;
                sched.on_complete(w);
                while let Some(id) = sched.next_dispatch_for(w) {
                    running[w].push(id);
                }
                while let Some((rw, id)) = sched.next_refill() {
                    running[rw].push(id);
                }
            }

            assert_eq!(sched.in_flight(), 0);
            assert_eq!(sched.queued(), 0);
            for (job_id, &hits) in completed.iter().enumerate() {
                assert_eq!(hits, 1, "job {job_id} must complete exactly once, got {hits}");
            }
        });

        // 2. Deterministic giant-function scenario + Rule 23 planted-bug control:
        //    8 workers, 64 jobs all hashing to the same `home` worker where job 0 is a giant
        //    function taking 40 ticks and jobs 1..64 take 1 tick each.
        let run_giant_scenario = |steal: bool| -> (usize, usize) {
            let workers = 8usize;
            let total = 64usize;
            let mut sched: ExportScheduler<'_, (usize, usize)> = ExportScheduler::new_in(workers, 4, 2, alloc);
            if !steal {
                sched.disable_stealing_for_test();
            }
            let giant_key = ItemKey(0xDEAD_BEEF_CAFE_BABEu128);
            let mut in_prog: Vec<VecDeque<(usize, usize), Alloc<'static>>, Alloc<'static>> = Vec::with_capacity_in(workers, alloc);
            for _ in 0..workers {
                in_prog.push(VecDeque::new_in(alloc));
            }
            for id in 0..total {
                let dur = if id == 0 { 40usize } else { 1usize };
                if let Some((w, job)) = sched.enqueue(giant_key, (id, dur)) {
                    in_prog[w].push_back(job);
                }
            }
            while let Some((w, job)) = sched.next_refill() {
                in_prog[w].push_back(job);
            }
            let mut ticks = 0usize;
            let mut idle_worker_ticks_while_queued = 0usize;
            let mut done = 0usize;
            while done < total {
                ticks += 1;
                if sched.queued() > 0 {
                    for w in 0..workers {
                        if in_prog[w].is_empty() {
                            idle_worker_ticks_while_queued += 1;
                        }
                    }
                }
                for w in 0..workers {
                    if let Some(front) = in_prog[w].front_mut() {
                        front.1 -= 1;
                        if front.1 == 0 {
                            let _ = in_prog[w].pop_front();
                            done += 1;
                            sched.on_complete(w);
                            while let Some(job) = sched.next_dispatch_for(w) {
                                in_prog[w].push_back(job);
                            }
                        }
                    }
                }
                while let Some((w, job)) = sched.next_refill() {
                    in_prog[w].push_back(job);
                }
                assert!(ticks < 500, "bounded simulation progress");
            }
            (ticks, idle_worker_ticks_while_queued)
        };

        let (steal_ticks, steal_idle) = run_giant_scenario(true);
        assert_eq!(steal_idle, 0, "work-stealing scheduler must have 0 idle worker ticks while jobs are queued");
        // At most `per_worker_cap - 1` (= 3) pre-buffered small jobs can sit in the home worker's
        // inbox behind the 40-tick giant job; all remaining 60 small jobs are stolen by the 7
        // peers and finish in parallel well before tick 40 (independent of total job count).
        assert_eq!(steal_ticks, 43, "expected exact makespan 40 + (per_worker_cap - 1) = 43 ticks under 40-tick giant job, got {steal_ticks}");

        let (ctrl_ticks, ctrl_idle) = run_giant_scenario(false);
        assert!(ctrl_idle > 200, "Rule 23 control: disabling stealing must expose severe idle worker starvation ({ctrl_idle})");
        assert!(ctrl_ticks > 100, "Rule 23 control: disabling stealing serializes all 64 jobs behind the giant job ({ctrl_ticks})");
    }

    #[test]
    fn pre_export_checkpoint_survives_linker_failure_and_gc_and_control_loses_it() {
        use core::time::Duration;
        use blaze_cas::Key as CasKey;
        use blaze_cas::plain::{GRACE_NS, LEASE_NS, Liveness, Settings as StoreSettings};
        use blaze_core::FileName;
        use blaze_core::sim::{Faults, Sim};
        use blaze_ids::ContentId;
        use blaze_keyed::kind::{Namespace, ReservedTag};
        use blaze_ladder::code_records::{CodeClass, CodeStore};
        use crate::store::plain_place::{Facts, config, open};

        let run_with_linker_failure = |checkpoint_before_export: bool| -> (usize, usize, usize) {
            let heap = blaze_own::testing::heap();
            let alloc = heap.alloc();
            let open_proc = |sim: Sim, nonce: u64| {
                let mut cfg = config(FileName::new("store").expect("dir"), &StoreSettings::default(), Facts { boot: Some(1), nonce }, alloc);
                cfg.liveness = Liveness::Reached;
                open(sim, cfg, alloc).expect("open store")
            };
            let code_key = |tag: &'static str, n: u32| CodeKey(ContentId::of(tag, &n.to_le_bytes()));
            let payload = |n: u32| -> std::vec::Vec<u8> {
                (0..1024).map(|i| ((n as usize).wrapping_mul(97).wrapping_add(i) & 0xff) as u8).collect()
            };
            let content_key = |bytes: &[u8]| CodeKey(ContentId::of("pre-export checkpoint content", bytes));

            let picked = run_package::Program {
                package: "blaze-cli",
                bin: "blaze",
                manifest: "crates/blaze-cli/Cargo.toml",
                root: "crates/blaze-cli/src/main.rs",
                edition: "2024",
            };
            let ws_id = Some(ContentId::of("pre-export ws", b"blaze"));
            let crate_key = blaze_id::CrateItemKey(ContentId::of("pre-export crate", b"blaze-cli"));
            let ids_table = CasKey(ContentId::of("pre-export run_ids", b"table"));
            let today = crate::start_list::Today {
                certificate: crate::start_list::Certificate::for_tests([0x77; 32]),
                name: crate::start_list::ListName::of(&build_program_shape(&picked, ws_id, crate_key)),
                tables: [ids_table, ids_table],
            };

            let sim = Sim::new(91, Faults::default());
            let lent = open_proc(sim, 1);
            lent.put(&ids_table, Namespace::LOCAL, ReservedTag::Drive.tag(), b"run-ids-table").expect("put ids table");

            let mut codes = HomeCodeStore::new(&lent);
            for i in 0..6u32 {
                let bytes = payload(i);
                let ck = content_key(&bytes);
                codes.put(CodeClass::Program, ck, &bytes).expect("put prog");
                codes.point(CodeClass::Program, code_key("pre-export prog", i), ck).expect("point prog");
            }
            for i in 100..104u32 {
                let bytes = payload(i);
                let ck = content_key(&bytes);
                codes.put(CodeClass::Toolchain, ck, &bytes).expect("put tool");
                codes.point(CodeClass::Toolchain, code_key("pre-export tool", i), ck).expect("point tool");
            }
            let mut dep_typed_keys: std::vec::Vec<CasKey> = std::vec::Vec::new();
            for i in 200..204u32 {
                let key = blaze_drive::Key(ContentId::of("pre-export dep typed", &i.to_le_bytes()));
                let bytes = payload(i);
                let wrote = crate::engine_body_record::put_dep_typed_if_absent(&lent, key, &bytes).expect("put dep typed");
                assert!(wrote);
                dep_typed_keys.push(CasKey(key.0));
            }

            if checkpoint_before_export {
                // Phase 3: checkpoint roots & commit BEFORE `blaze_export::export()` runs.
                codes.ensure_persisted().expect("ensure persisted");
                let mut walked = codes.reached();
                walked.extend_from_slice(&dep_typed_keys);
                let tool_made = codes.toolchain_made();
                let kept = keep_export_roots(&lent, &today, blaze_toolchain::PINNED, &walked, &tool_made);
                kept.program.expect("program root");
                kept.toolchain.expect("toolchain root");
                if lent.needs_commit() {
                    lent.commit().expect("pre-export commit");
                }
            }
            // Simulate `blaze_export::export()` failing with a linker error: `export_program`
            // returns `Err(...)`, so `engine_check`'s closing `after_answer_printed_with` NEVER
            // runs — `codes` and `lent` are simply dropped on error unwind!
            drop(codes);
            let mut sim = lent.into_store().into_driver();

            // Advance past lease + grace and run two GC compaction rounds.
            sim.advance(Duration::from_nanos(LEASE_NS.max(GRACE_NS)) + Duration::from_secs(60));
            let gc1 = open_proc(sim, 2);
            let _ = gc1.compact_foreground(32).expect("gc1");
            let mut sim = gc1.into_store().into_driver();
            sim.advance(Duration::from_nanos(GRACE_NS) + Duration::from_secs(60));
            let gc2 = open_proc(sim, 3);
            let _ = gc2.compact_foreground(32).expect("gc2");
            let sim = gc2.into_store().into_driver();

            // Reopen in the next `blaze build` and probe all compiled CodeRecords and Typed entries.
            let verify = open_proc(sim, 4);
            let codes = HomeCodeStore::new(&verify);
            let mut prog_hits = 0usize;
            for i in 0..6u32 {
                if entry_of(&codes, code_key("pre-export prog", i)).expect("read prog") == Some(payload(i).as_slice()) {
                    prog_hits += 1;
                }
            }
            let mut tool_hits = 0usize;
            for i in 100..104u32 {
                if entry_of(&codes, code_key("pre-export tool", i)).expect("read tool") == Some(payload(i).as_slice()) {
                    tool_hits += 1;
                }
            }
            let mut typed_hits = 0usize;
            for (idx, cas_key) in dep_typed_keys.iter().enumerate() {
                if verify.get(cas_key).expect("read typed").is_some_and(|l| l.bytes == payload(200 + idx as u32).as_slice()) {
                    typed_hits += 1;
                }
            }
            (prog_hits, tool_hits, typed_hits)
        };

        let (prog, tool, typed) = run_with_linker_failure(true);
        assert_eq!(prog, 6, "pre-export checkpoint must preserve all 6 program CodeRecords across linker failure + GC");
        assert_eq!(tool, 4, "pre-export checkpoint must preserve all 4 toolchain CodeRecords across linker failure + GC");
        assert_eq!(typed, 4, "pre-export checkpoint must preserve all 4 persisted dep Typed records across linker failure + GC");

        let (ctrl_prog, ctrl_tool, ctrl_typed) = run_with_linker_failure(false);
        assert_eq!(ctrl_prog, 0, "Rule 23 control: skipping pre-export checkpoint discards uncommitted program CodeRecords on linker failure");
        assert_eq!(ctrl_tool, 0, "Rule 23 control: skipping pre-export checkpoint discards uncommitted toolchain CodeRecords on linker failure");
        assert_eq!(ctrl_typed, 0, "Rule 23 control: skipping pre-export checkpoint discards uncommitted dep Typed records on linker failure");
    }
}
