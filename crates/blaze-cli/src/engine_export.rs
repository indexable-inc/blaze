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
use blaze_export::{Data, DataPlace, DataWord, Entry, ExportError, Exported, Function, Panic, Platform, ProgramIn, Tier, TlsRuntime};
use blaze_ladder::body_ids::{self, RowKind, Scratch, Stepped, Walk};
use blaze_ladder::code_records::{CodeKey, entry_of, statics};
use blaze_ladder::run::make_one::{ByKind, MOST_WALKED, Maker};
use blaze_ladder::run::special::{EntryForm, Roots, Special, make_special, run_roots};
use blaze_ladder::run::RunError;
use blaze_own::{AString, AVec, Alloc, Box, Bump, Erased, String, Vec};
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
const EXPORT_FRAME_RESERVE: usize = 2 << 30;

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
    NotChecked { function: ItemKey, symbol: [u8; 32], why: NotRunnable },
    /// Stepping an instance/static/TLS row in the reachability walk failed.
    Walk { symbol: [u8; 32], shown: AString<'static> },
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
            ExportCliError::UnsupportedHost => write!(f, "blaze export: only arm64 macOS and arm64 Linux hosts are supported (engine_export.rs)"),
            ExportCliError::Frame => write!(f, "blaze export: could not reserve the export scratch frame (engine_export.rs)"),
            ExportCliError::Native { shown } => write!(f, "blaze export: native image: {} (engine_native.rs)", shown.as_str()),
            ExportCliError::Builder { shown } => write!(f, "blaze export: code builder: {} (engine_export.rs)", shown.as_str()),
            ExportCliError::Roots { shown } => write!(f, "blaze export: program roots: {} (engine_export.rs)", shown.as_str()),
            ExportCliError::NotChecked { function, symbol, why } => write!(
                f,
                "blaze export: reached fn {:#x} (symbol {}) may not be exported: {why} (engine_export.rs)",
                function.0,
                Hex(symbol)
            ),
            ExportCliError::Walk { symbol, shown } => write!(f, "blaze export: compiling symbol {}: {} (engine_export.rs)", Hex(symbol), shown.as_str()),
            ExportCliError::Special { what, shown } => write!(f, "blaze export: building {what}: {} (engine_export.rs)", shown.as_str()),
            ExportCliError::Store { symbol, shown } => write!(f, "blaze export: reading code entry {}: {} (engine_export.rs)", Hex(symbol), shown.as_str()),
            ExportCliError::MissingEntry { symbol } => write!(f, "blaze export: code entry {} missing from store after step (engine_export.rs)", Hex(symbol)),
            ExportCliError::Decode { symbol, what, shown } => write!(f, "blaze export: decoding {what} {}: {} (engine_export.rs)", Hex(symbol), shown.as_str()),
            ExportCliError::Position { symbol, shown } => write!(f, "blaze export: resolving source position in {}: {} (engine_export.rs)", Hex(symbol), shown.as_str()),
            ExportCliError::Materialize { symbol, shown } => write!(f, "blaze export: materializing position-free record {}: {} (engine_export.rs)", Hex(symbol), shown.as_str()),
            ExportCliError::Export(error) => write!(f, "blaze export: {error}"),
            ExportCliError::BadOutputPath { path, why } => write!(f, "blaze export: invalid output path `{}`: {why} (engine_export.rs)", path.as_str()),
            ExportCliError::OutputHome(error) => write!(f, "blaze export: output directory: {error} (engine_export.rs)"),
            ExportCliError::OutputPath(error) => write!(f, "blaze export: output path: {error} (engine_export.rs)"),
            ExportCliError::WriteOutput(error) => write!(f, "blaze export: writing executable: {error} (engine_export.rs)"),
            #[cfg(unix)]
            ExportCliError::Chmod { path, errno } => write!(f, "blaze export: chmod 0755 `{}` failed: {errno:?} (engine_export.rs)", path.as_str()),
        }
    }
}

impl core::error::Error for ExportCliError {}

fn owned_str(text: &str, alloc: Alloc<'static>) -> AString<'static> {
    let mut s = String::new_in(alloc);
    s.push_str(text);
    s
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
    let select = run_package::Select { test_packages: packages.as_slice(), ..flags.select };
    let started_ns = phases.now();
    let stats = flags.stats;
    let output_flag = flags.output;
    let view = flags.view.unwrap_or(blaze_report::frame::plain::View::Person);
    let line = crate::run_line::LineOpts::from_env(env, err, flags.color, blaze_screen::Command::Run, view, false);
    let line = crate::run_line::LineOpts { live: false, ..line };
    let lined = crate::run_line::watched(cx, err, clock, line, |watch, _bg| {
        watch.see(blaze_screen::Step::Began(blaze_screen::Stage::Checking));
        check_for_start(cx, Some(args), fs, env, net, host, rng, err, err, phases, heap, &flags, true, None, |standing, phases, _rng| {
            let (root, frame) = (standing.checkout, standing.frame);
            let select = run_package::Select { member: standing.member, ..select };
            let picked = run_package::pick(cx, root, select, frame)?;
            let summary = export_program(cx, standing, &picked, fs, output_flag, phases, watch)?;
            write_start_profile(cx, root, phases, frame)?;
            let wall_ns = phases.since(started_ns);
            print_summary(cx, &summary, wall_ns, stats, out, err, frame)?;
            Ok(Status::OK)
        })
    });
    lined.map_err(CliError::RunLine)?
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
}

/// How one walked row's entry bytes are held until assembly.
enum RowOutcome<'a> {
    Keyed(CodeKey),
    Unkeyed(AVec<'a, u8>),
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
            if t.space == Space::Outside
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
        if t.space == Space::Outside {
            let Some(name) = view.name(t.symbol) else {
                return Err(ExportCliError::Materialize {
                    symbol: symbol.0,
                    shown: owned_fmt(format_args!("missing outside name for {}", t.symbol), frame, heap.alloc()),
                });
            };
            rec.outside(name);
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

/// Core whole-program reachability closure + `blaze_export::export` + atomic write + chmod.
fn export_program<'f>(
    cx: &Cx,
    mut standing: Standing<'f, '_>,
    picked: &run_package::Program<'f>,
    fs: &Fs,
    output_flag: Option<&str>,
    phases: &mut Phases,
    watch: &dyn blaze_screen::Watch,
) -> Result<ExportSummary<'f>, CliError> {
    let (frame, heap) = (standing.frame, standing.heap);
    let Some(platform) = host_platform() else {
        return Err(box_err(ExportCliError::UnsupportedHost, heap));
    };
    let output_path: &'f str = match output_flag {
        Some(path) => frame.alloc_str(path),
        None => picked.bin,
    };
    let export_frame = Frame::with_reserve(EXPORT_FRAME_RESERVE, bstd::mem::Tag::Other).map_err(|_| box_err(ExportCliError::Frame, heap))?;
    let export_erased = Erased::new(&export_frame);
    let export_bump = export_erased.bump();
    let alloc = export_bump.alloc();

    let erased = Erased::new(frame);
    let bump = erased.bump();

    watch.see(blaze_screen::Step::Began(blaze_screen::Stage::Crates));
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

    let overflow = program::dev_overflow(cx, root, frame)?;
    let panic_profile = program::dev_profile_panic(cx, root, frame)?;
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

    let names = run_names::names_of(cx, root, &found, owner, &mut *standing.store, &mut facts.bodies, &mut read, env, &pack_dirs, procs, &seen, frame, heap).map_err(CliError::RunNames)?;

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
    let mut maker = Maker::of_roots(names, maker_entry, &roots, ByKind::new(typed.over(ids)), &builder, heap.alloc());
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
    let (mut compiled, mut cached, mut unkeyed, mut stubs) = (0usize, 0usize, 0usize, 0usize);
    let root_count = roots.len();

    let mut next_row = 0usize;
    while next_row < walk.len() {
        let row = next_row;
        next_row += 1;
        let tabled = walk.tabled(row);
        let symbol = walk.symbol(row);
        let kind = walk.kind(row);
        let proof = ids.runnable(tabled).map_err(|why| box_err(ExportCliError::NotChecked { function: tabled, symbol: symbol.0, why }, heap))?;
        match walk.one(row, &ids, &mut codes, &builder, &builder.producer, &mut maker, &proof, &body_ids::EntryForm, MOST_WALKED, &mut scratch) {
            Ok(one) => match one.stepped {
                Stepped::Found(key) => {
                    cached += 1;
                    outcomes.push(RowOutcome::Keyed(key));
                }
                Stepped::Made(key) => {
                    compiled += 1;
                    outcomes.push(RowOutcome::Keyed(key));
                }
                Stepped::Unkeyed(_) => {
                    unkeyed += 1;
                    let mut copied: AVec<'_, u8> = Vec::with_capacity_in(scratch.made().len(), alloc);
                    copied.extend_from_slice(scratch.made());
                    outcomes.push(RowOutcome::Unkeyed(copied));
                }
            },
            Err(why) if row >= root_count && kind == RowKind::Instance && why.scope() == blaze_ladder::scope::Scope::One => {
                let msg = bstd::mem::format_in!(frame, "blaze export: called unbuilt function: {why}");
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
    end_procs(cx, &maker_procs, standing.err, "export")?;

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

    // Assemble `ProgramIn` functions and data (zero-copy for cached positionless records).
    let mut functions: AVec<'_, Function<'_>> = Vec::with_capacity_in(outcomes.len() + 2, alloc);
    let mut data: AVec<'_, Data<'_>> = Vec::new_in(alloc);
    let mut seen_data: AVec<'_, Target> = Vec::new_in(alloc);
    let mut tls_count = 0usize;

    for (row, outcome) in outcomes.iter().enumerate() {
        let symbol = walk.symbol(row);
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
            RowOutcome::UnbuiltStub(_) => continue,
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
            }
        }
    }

    let tls_runtime = match tls_keys {
        Some((getter_key, ender_key)) => {
            for (sym, key, what) in [(tls_getter_sym, getter_key, "TLS getter"), (tls_ender_sym, ender_key, "TLS ender")] {
                let entry_bytes = entry_of(&codes, key)
                    .map_err(|why| box_err(ExportCliError::Store { symbol: sym.0, shown: owned_fmt(format_args!("{why}"), frame, heap.alloc()) }, heap))?
                    .ok_or_else(|| box_err(ExportCliError::MissingEntry { symbol: sym.0 }, heap))?;
                let rec_bytes = statics::entry_record(entry_bytes)
                    .map_err(|why| box_err(ExportCliError::Decode { symbol: sym.0, what, shown: owned_fmt(format_args!("{why}"), frame, heap.alloc()) }, heap))?;
                let view = CodeView::read(rec_bytes)
                    .map_err(|why| box_err(ExportCliError::Decode { symbol: sym.0, what, shown: owned_fmt(format_args!("{why}"), frame, heap.alloc()) }, heap))?;
                functions.push(Function { symbol: sym, record: view, tier: Tier::Fast });
            }
            Some(TlsRuntime { descriptor: tls_desc_sym, ender: tls_ender_sym })
        }
        None => None,
    };

    let data_count = data.len();
    let program_in = ProgramIn {
        name: picked.bin,
        functions: functions.as_slice(),
        data: data.as_slice(),
        entry,
        panic: export_panic,
        personality,
        tls: tls_runtime,
        profile: None,
        natives: native_export.as_ref().map(|n| &n.image),
        exports: native_export.as_ref().map_or(&[], |n| n.exports.as_slice()),
    };

    let mut exe_bytes: AVec<'_, u8> = Vec::new_in(alloc);
    let exported = blaze_export::export(&program_in, platform, &mut exe_bytes, alloc).map_err(|error| box_err(ExportCliError::Export(error), heap))?;

    // Release immutable borrows of `codes` before persisting toolchain code.
    drop(functions);
    drop(data);
    drop(seen_data);
    let toolchain_made = codes.toolchain_made();
    drop(codes);
    if let Err(why) = crate::store::roots::keep_toolchain(lent, builder.toolchain, &toolchain_made, &[]) {
        standing.err.line(cx, format_args!("blaze export: warning: toolchain code was not kept: {why}")).map_err(CliError::Out)?;
    }

    write_executable(cx, fs, output_path, exe_bytes.as_slice(), heap)?;
    Ok(ExportSummary {
        output: output_path,
        exported,
        data_count,
        tls_count,
        compiled,
        cached,
        unkeyed,
        stubs,
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

/// Atomically publishes `bytes` to `output_path` through `bstd::fs` and marks the resulting file
/// executable (`0o755`) on real Unix hosts.
fn write_executable(cx: &Cx, fs: &Fs, output_path: &str, bytes: &[u8], heap: &'static blaze_own::Heap) -> Result<(), CliError> {
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
    if !cx.is_simulated() {
        chmod_executable(output_path, heap)?;
    }
    Ok(())
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
                "exported {} ({} bytes, {} functions, {} stubs, {} data, {} tls, {} ms)",
                summary.output, summary.exported.len, summary.exported.functions, summary.stubs, summary.data_count, summary.tls_count, wall_ms
            ),
        )?,
        (true, false) => out.line(
            cx,
            format_args!(
                "exported {} ({} bytes, {} functions, {} data, {} tls, {} ms)",
                summary.output, summary.exported.len, summary.exported.functions, summary.data_count, summary.tls_count, wall_ms
            ),
        )?,
        (false, true) => out.line(
            cx,
            format_args!(
                "exported {} ({} bytes, {} functions, {} stubs, {} data, {} ms)",
                summary.output, summary.exported.len, summary.exported.functions, summary.stubs, summary.data_count, wall_ms
            ),
        )?,
        (false, false) => out.line(
            cx,
            format_args!(
                "exported {} ({} bytes, {} functions, {} data, {} ms)",
                summary.output, summary.exported.len, summary.exported.functions, summary.data_count, wall_ms
            ),
        )?,
    }
    if stats {
        let erased = Erased::new(frame);
        let mut report = String::new_in(erased.handle());
        let _ = summary.exported.report(&mut report);
        for line in report.as_str().lines() {
            err.line(cx, format_args!("export: {line}"))?;
        }
        err.line(
            cx,
            format_args!(
                "export stats: functions={} folded={} stubs={} data={} tls={} compiled={} cached={} unkeyed={} bytes={} wall={}ms",
                summary.exported.functions,
                summary.exported.folded,
                summary.stubs,
                summary.data_count,
                summary.tls_count,
                summary.compiled,
                summary.cached,
                summary.unkeyed,
                summary.exported.len,
                wall_ms
            ),
        )?;
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
}
