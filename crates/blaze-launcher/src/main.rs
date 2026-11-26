//! blaze's launcher: the process a program runs in under `blaze run` and `blaze test`
//! (docs/v2/plan/design/launcher.md, section 2).
//!
//! The file a program runs from is a clone of this binary; beside it, the sidecar
//! `<exe>.blaze-program` names the program and the socket of the process that serves its code
//! records. Started, with the program's own arguments and environment, the launcher:
//!
//! 1. reads the sidecar and connects to the socket it names; with nothing listening there it
//!    stops and says so;
//! 2. maps its code area and starts its control thread, which says `Hello` and from then on
//!    is the one thread that writes code (`blaze_launch_rt::control`);
//! 3. waits for the word to start, runs the program's initializers as the system's loader
//!    would (`argc`, `argv`, `envp`), and calls its `main(argc, argv)` on this, the process's
//!    first thread;
//! 4. ends with what `main` returned, through `exit`, as a linked program's start code does.
//!
//! The program is this process: its arguments, environment, descriptors, signals and exit
//! status are the process's own, and nothing of it runs anywhere else. No function of it has
//! code here until it is first called.
//!
//! Told to start again (an edit that cannot be applied to a running program), the control
//! thread `execv`s this file with the same arguments: the same process id and terminal.

// The launcher's own messages go to its stderr, as a loader's do: it has no other channel.
#![cfg(any(
    all(target_os = "macos", target_arch = "aarch64"),
    all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")),
))]
// comptime.rs: blaze-comptime-sys's `Limits { page, .. }` (its tunables defaulted, rule 83).
#![feature(default_field_values)]

mod comptime;

use std::env;
use std::ffi::{CString, OsStr, OsString, c_char, c_int};
use std::io::Write;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use blaze_launch::wire::ProgramId;
use blaze_launch_rt::{Config, Driver, Loaded};

/// The exit status of a launcher that could not start its program: as a shell's for a
/// command it could not run.
const CANNOT_RUN: i32 = 127;

/// Says `why` and ends the process with `status`.
fn stop(status: i32, why: &str) -> ! {
    // The process ends below whether or not its last words could be written.
    let _ = writeln!(std::io::stderr().lock(), "blaze: {why}");
    std::process::exit(status)
}

/// What a sidecar says.
enum Sidecar {
    /// Legacy daemon sidecar: `socket` + `program` (64 hex digits).
    Daemon {
        /// The socket of the process that serves the program's code records.
        socket: PathBuf,
        program: ProgramId,
    },
    /// `mode=run` stub sidecar (e.g. `blaze test` `CARGO_BIN_EXE_*` launcher stubs): connects to
    /// `socket`, writes the 12-byte little-endian `[bin, owner_suite, owner_row]` handshake, then
    /// speaks the `run_mode` wire protocol (`blaze_launch::wire::run`) directly over that stream.
    Run {
        socket: PathBuf,
        bin: u32,
        owner: (u32, u32),
        store: PathBuf,
        entry: OsString,
        native: Option<OsString>,
    },
}

/// Decodes an even-length hex string into a `PathBuf` using a stack buffer (used for `store_hex=`
/// so arbitrary Unix paths containing newlines or non-UTF-8 bytes survive line-oriented sidecars).
fn hex_path(hex: &str) -> Option<PathBuf> {
    let digits = hex.as_bytes();
    if !digits.len().is_multiple_of(2) {
        return None;
    }
    let out_len = digits.len() / 2;
    let mut buf = [0u8; 4096];
    let dst = buf.get_mut(..out_len)?;
    let digit = |it: u8| match it {
        b'0'..=b'9' => Some(it - b'0'),
        b'a'..=b'f' => Some(it - b'a' + 10),
        b'A'..=b'F' => Some(it - b'A' + 10),
        _ => None,
    };
    for (slot, pair) in dst.iter_mut().zip(digits.chunks_exact(2)) {
        let high = digit(pair[0])?;
        let low = digit(pair[1])?;
        *slot = (high << 4) | low;
    }
    Some(PathBuf::from(OsStr::from_bytes(dst)))
}

/// Capacity of the in-binary stub configuration slot patched by `blaze test` when creating
/// self-describing `CARGO_BIN_EXE_*` launcher stubs (16 KiB, 4 KiB-aligned so macOS Mach-O
/// ad-hoc page re-hashing touches at most 4 code pages).
const STUB_SLOT_SIZE: usize = 16 * 1024;
const STUB_SLOT_HEADER: usize = 36; // 16-byte magic + 16-byte state tag + 4-byte LE length
const STUB_SLOT_MAGIC: &[u8; 16] = b"BLAZE_STUB_SLOT1";
const STUB_SLOT_PATCHED: &[u8; 16] = b"PATCHED_STUB_001";

#[repr(C)]
struct StubSlotBytes {
    header: [u8; 32],
    rest: [u8; STUB_SLOT_SIZE - 32],
}

#[repr(C, align(4096))]
pub struct EmbeddedStubSlot(core::cell::UnsafeCell<StubSlotBytes>);

// SAFETY: `EMBEDDED_STUB_SLOT` is patched offline in the stub executable before process start
// and is only ever read at startup (never mutated at runtime).
unsafe impl Sync for EmbeddedStubSlot {}

#[unsafe(no_mangle)]
#[used]
pub static EMBEDDED_STUB_SLOT: EmbeddedStubSlot = EmbeddedStubSlot(core::cell::UnsafeCell::new(StubSlotBytes {
    // Magic (16 bytes) + unpatched tag (16 bytes). `blaze test` locates this exact 36-byte header
    // (including the trailing 0u32 length) once in `blaze-launcher`, replaces the tag with
    // `STUB_SLOT_PATCHED`, and writes the length + UTF-8 config payload so stubs stay runnable
    // even if copied to another path without an adjacent `.blaze-program` file.
    header: *b"BLAZE_STUB_SLOT1UNPATCHED_CFG_01",
    rest: [0u8; STUB_SLOT_SIZE - 32],
}));

/// Reads the stub's embedded configuration slot when patched by `blaze test`, otherwise reads the
/// adjacent `<exe>.blaze-program` sidecar (`name=value` per line).
fn sidecar(exe: &Path) -> Result<Sidecar, String> {
    // Pass the slot pointer through `black_box` so LLVM cannot constant-fold the initial bytes
    // even under LTO.
    let ptr: *const StubSlotBytes = core::hint::black_box(EMBEDDED_STUB_SLOT.0.get().cast_const());
    // SAFETY: `EMBEDDED_STUB_SLOT` is a `'static`, 4096-byte aligned `StubSlotBytes` of
    // `STUB_SLOT_SIZE` bytes, never mutated at runtime.
    let slot: &[u8] = unsafe { core::slice::from_raw_parts(ptr.cast::<u8>(), STUB_SLOT_SIZE) };
    let (label, owned_text);
    let text: &str = if slot.get(..16) == Some(STUB_SLOT_MAGIC.as_slice())
        && slot.get(16..32) == Some(STUB_SLOT_PATCHED.as_slice())
    {
        let Some(&[b0, b1, b2, b3]) = slot.get(32..36) else {
            return Err(format!("{} has a truncated embedded stub header", exe.display()));
        };
        let Ok(len) = usize::try_from(u32::from_le_bytes([b0, b1, b2, b3])) else {
            return Err(format!("{} has an out-of-range embedded stub length", exe.display()));
        };
        let end = match STUB_SLOT_HEADER.checked_add(len) {
            Some(end) if end <= STUB_SLOT_SIZE => end,
            _ => return Err(format!("{} has an invalid embedded stub length ({len})", exe.display())),
        };
        let Some(payload) = slot.get(STUB_SLOT_HEADER..end) else {
            return Err(format!("{} has an unreadable embedded stub payload", exe.display()));
        };
        label = exe.display().to_string();
        core::str::from_utf8(payload)
            .map_err(|why| format!("{} has non-UTF-8 embedded stub config: {why}", exe.display()))?
    } else {
        let mut path = exe.as_os_str().to_owned();
        path.push(".blaze-program");
        let path = PathBuf::from(path);
        owned_text = std::fs::read_to_string(&path).map_err(|why| {
            format!(
                "{} is a clone of blaze's launcher and its sidecar {} does not read: {why}. Run it with \
                 `blaze run`",
                exe.display(),
                path.display()
            )
        })?;
        label = path.display().to_string();
        &owned_text
    };

    let line = |name: &str| text.lines().find_map(|line| line.strip_prefix(name)?.strip_prefix('='));
    let field = |name: &str| {
        line(name).ok_or_else(|| format!("the sidecar {label} has no `{name}`: run the program with `blaze run`"))
    };
    let socket = PathBuf::from(field("socket")?);
    if line("mode") == Some("run") {
        let bin_raw = field("bin")?;
        let bin = bin_raw.parse::<u32>().map_err(|_| {
            format!("the sidecar {label} names bin `{bin_raw}`, which is no u32")
        })?;
        let owner = match line("owner") {
            None => (u32::MAX, u32::MAX),
            Some(raw) => {
                let (s, r) = raw.split_once(':').ok_or_else(|| {
                    format!("the sidecar {label} names owner `{raw}`, expected `<suite>:<row>`")
                })?;
                let suite = s.parse::<u32>().map_err(|_| {
                    format!("the sidecar {label} names owner suite `{s}`, which is no u32")
                })?;
                let row = r.parse::<u32>().map_err(|_| {
                    format!("the sidecar {label} names owner row `{r}`, which is no u32")
                })?;
                (suite, row)
            }
        };
        let store = match line("store_hex") {
            Some(hex) => hex_path(hex).ok_or_else(|| {
                format!("the sidecar {label} has malformed `store_hex`")
            })?,
            None => PathBuf::from(field("store")?),
        };
        let entry = OsString::from(field("entry")?);
        let native = line("native").map(OsString::from);
        return Ok(Sidecar::Run { socket, bin, owner, store, entry, native });
    }
    let hex = field("program")?;
    let Some(id) = hex32(hex) else {
        return Err(format!(
            "the sidecar {label} names program `{hex}`, which is no 64 hex digits: run the program with `blaze run`"
        ));
    };
    Ok(Sidecar::Daemon { socket, program: ProgramId(id) })
}

/// 64 hex digits as 32 bytes.
fn hex32(hex: &str) -> Option<[u8; 32]> {
    let mut id = [0u8; 32];
    let digits = hex.as_bytes();
    let digit = |it: u8| match it {
        b'0'..=b'9' => Some(it - b'0'),
        b'a'..=b'f' => Some(it - b'a' + 10),
        b'A'..=b'F' => Some(it - b'A' + 10),
        _ => None,
    };
    let whole = digits.len() == 64
        && id.iter_mut().zip(digits.chunks(2)).all(|(byte, pair)| match (digit(pair[0]), digit(pair[1])) {
            (Some(high), Some(low)) => {
                *byte = high << 4 | low;
                true
            }
            _ => false,
        });
    whole.then_some(id)
}

/// The path this process was started from, read once at its start: a new file may be placed
/// at the path while the program runs, and the file this process maps is then one the system
/// no longer names.
static STARTED_FROM: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Starts the program again in place: the file at the path this process was started from,
/// with this process's arguments and environment. It does not return: where the program
/// cannot be started again the process ends, saying why, since what it would run on is code
/// that was replaced (rule `no-fallback`).
fn restart(why: &str) -> String {
    let _ = writeln!(std::io::stderr().lock(), "blaze: restart needed: {why}");
    let Some(exe) = STARTED_FROM.get() else {
        stop(101, "the program must start again, and the path it was started from was never read");
    };
    let c = |text: OsString| CString::new(text.into_vec()).ok();
    let Some(path) = c(exe.clone().into_os_string()) else {
        stop(101, &format!("the program must start again, and {} has a NUL in its path", exe.display()));
    };
    // alloc: the arguments of the one `execv`
    let args: Option<Vec<CString>> = std::env::args_os().map(c).collect();
    let Some(args) = args else {
        stop(101, "the program must start again, and an argument of this process has a NUL in it");
    };
    let mut argv: Vec<*const c_char> = args.iter().map(|it| it.as_ptr()).collect(); // alloc: as above
    argv.push(core::ptr::null());
    // SAFETY: `path` and every element of `argv` are C strings that live across the call,
    // and `argv` ends in a null. The environment is the process's own.
    unsafe { libc::execv(path.as_ptr(), argv.as_ptr()) };
    let failed = std::io::Error::last_os_error();
    stop(101, &format!("the program must start again, and starting {} failed: {failed}", exe.display()))
}

unsafe extern "C" {
    /// The process's environment, as the C library keeps it.
    static environ: *const *const c_char;
}

/// The bytes of the exchange a hosted program's calls pass through (bstd `Hosting`): the size
/// `blaze run`'s in-process host lends (blaze-cli program.rs `EXCHANGE`), which goes with that
/// path (run.md piece 13), leaving this one.
const EXCHANGE: usize = 4096;

/// The threads the hosted world's native driver may run file calls on: one, since the hosted
/// wire carries no file effect yet (run.md, "The hosted world": `Timer` and `Random` alone).
const HOSTED_FILE_THREADS: usize = 1;

/// The run path's mode (docs/v2/plan/design/run.md, piece 5): the parent, `blaze run`, set
/// `BLAZE_LAUNCH_FD=3` (its socket pair's end), `BLAZE_LAUNCH_STORE` (the folder of code records)
/// and `BLAZE_LAUNCH_ENTRY` (`<unit|c-main|hosted|comptime>:<symbol>`), and for a hosted entry
/// `BLAZE_LAUNCH_GRANTS` (blaze-launch grants.rs: the `(kind, name)` pairs the command line
/// granted), and for a program that links C `BLAZE_LAUNCH_NATIVE` (its native library record's
/// 64 hex digits: run_mode.rs `run`'s `native`). Each is read once here and removed from the
/// environment the program gets. A
/// `comptime:` entry (D175) is a compile-time program: its host on descriptor 4 (comptime.rs).
/// Never returns: the program, or for a compile-time one this launcher, ends the process.
fn run_mode(fd: OsString, os: &'static blaze_core::Os, shared: blaze_own::SharedAlloc<'static>) -> ! {
    use blaze_launch::{grants, telemetry};
    use blaze_launch_rt::run_mode::{self, Dlsym, End, EntryForm, Folder, Handed, LauncherOwn, ObservedBind, STOPPED, Start};
    use std::os::fd::{FromRawFd, OwnedFd};

    if fd.as_encoded_bytes() != b"3" {
        stop(
            CANNOT_RUN,
            &format!("BLAZE_LAUNCH_FD is `{}`: the run mode takes its parent's socket on descriptor 3 only", fd.display()),
        );
    }
    let Some(store) = std::env::var_os("BLAZE_LAUNCH_STORE") else {
        stop(CANNOT_RUN, "BLAZE_LAUNCH_FD is set and BLAZE_LAUNCH_STORE (the folder of the run's code records) is not");
    };
    let Some(entry) = std::env::var_os("BLAZE_LAUNCH_ENTRY") else {
        stop(CANNOT_RUN, "BLAZE_LAUNCH_FD is set and BLAZE_LAUNCH_ENTRY (`<unit|c-main|hosted|comptime>:<symbol>`) is not");
    };
    let start = match Start::parse(entry.as_encoded_bytes()) {
        Ok(start) => start,
        Err(why) => stop(CANNOT_RUN, &why.to_string()),
    };
    // The program's native library (blaze-launch wire/run.rs `Down::Native`'s rule): set only
    // when the program links C, and then the parent's first frame is its `Native`. A
    // compile-time program has none.
    let native = match (std::env::var_os("BLAZE_LAUNCH_NATIVE"), start.form) {
        (None, _) => None,
        (Some(_), EntryForm::Comptime) => stop(CANNOT_RUN, "BLAZE_LAUNCH_NATIVE is set for a compile-time program, which links no native library"),
        (Some(raw), _) => match run_mode::parse_native_start(raw.as_encoded_bytes()) {
            Ok(start) => Some(start),
            Err(why) => stop(CANNOT_RUN, &format!("BLAZE_LAUNCH_NATIVE (the program's native library record): {why}")),
        },
    };
    /// The entry's form with what only a hosted one has: its grants' text.
    enum Form {
        Unit,
        CMain,
        Hosted(OsString),
        Comptime,
    }
    // Grants are a hosted program's alone: the parent sets the variable for a hosted entry
    // (the empty text when the command line granted nothing) and for no other.
    let form = match (start.form, std::env::var_os(grants::VAR)) {
        (EntryForm::Unit, None) => Form::Unit,
        (EntryForm::CMain, None) => Form::CMain,
        (EntryForm::Comptime, None) => Form::Comptime,
        (EntryForm::Hosted, Some(granted)) => Form::Hosted(granted),
        (EntryForm::Hosted, None) => stop(
            CANNOT_RUN,
            &format!("BLAZE_LAUNCH_ENTRY names a hosted entry and {} (the grants of its command line) is not set", grants::VAR),
        ),
        (EntryForm::Unit | EntryForm::CMain | EntryForm::Comptime, Some(_)) => stop(
            CANNOT_RUN,
            &format!("{} is set and BLAZE_LAUNCH_ENTRY names no hosted entry: grants are a hosted program's", grants::VAR),
        ),
    };
    // Optional per-run shared-memory telemetry ring file (`BLAZE_LAUNCH_TELEMETRY`): read once
    // and removed before any program thread starts.
    let telemetry_path = match (std::env::var_os(telemetry::VAR), start.form) {
        (None, _) => None,
        (Some(_), EntryForm::Comptime) => stop(
            CANNOT_RUN,
            &format!("{} is set for a compile-time program, which runs walled under Table", telemetry::VAR),
        ),
        (Some(path), _) => Some(path),
    };
    // Optional unwind marker path (`BLAZE_LAUNCH_UNWOUND_FILE`, set only by `blaze test` for
    // harnessed tests): runs the entry with `End::Return` so an uncaught Rust panic unwinding
    // out of the test entry writes the marker and exits 101 instead of aborting with SIGABRT,
    // keeping genuine SIGABRTs (C `abort()`, double-panic, allocator abort) distinct.
    enum UnwoundSink {
        File(PathBuf),
        Fd(OwnedFd),
    }
    let unwound_sink = match (std::env::var_os("BLAZE_LAUNCH_UNWOUND_FILE"), start.form) {
        (None, _) => None,
        (Some(_), EntryForm::Comptime | EntryForm::Hosted) => stop(
            CANNOT_RUN,
            "BLAZE_LAUNCH_UNWOUND_FILE is set for a non-test entry",
        ),
        (Some(raw), EntryForm::Unit | EntryForm::CMain) => {
            if let Some(fd_str) = raw.to_str().and_then(|s| s.strip_prefix("fd:")) {
                let fd: i32 = match fd_str.parse() {
                    Ok(n) if n > 3 => n,
                    _ => stop(CANNOT_RUN, "BLAZE_LAUNCH_UNWOUND_FILE fd target is invalid"),
                };
                // SAFETY: validates `fd` is open and immediately marks it close-on-exec so no
                // subprocess spawned by the test inherits the launcher-owned unwind channel.
                if unsafe { libc::fcntl(fd, libc::F_GETFD) } == -1
                    || unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } == -1
                {
                    stop(CANNOT_RUN, "BLAZE_LAUNCH_UNWOUND_FILE fd is not open");
                }
                // SAFETY: `fd` was inherited from `blaze test` exclusively for this launcher.
                Some(UnwoundSink::Fd(unsafe { OwnedFd::from_raw_fd(fd) }))
            } else {
                Some(UnwoundSink::File(PathBuf::from(raw)))
            }
        }
    };
    #[cfg(target_os = "linux")]
    let sandbox_temp = match (std::env::var_os("BLAZE_LAUNCH_SANDBOX_TEMP"), start.form) {
        (None, _) => None,
        (Some(_), EntryForm::Comptime | EntryForm::Hosted) => stop(
            CANNOT_RUN,
            "BLAZE_LAUNCH_SANDBOX_TEMP is set for a non-test entry",
        ),
        (Some(raw), EntryForm::Unit | EntryForm::CMain) => Some(raw),
    };
    #[cfg(target_os = "linux")]
    let bins_dir = match (env::var_os("BLAZE_LAUNCH_BINS_DIR"), start.form) {
        (None, _) => None,
        (Some(_), EntryForm::Comptime | EntryForm::Hosted) => stop(
            CANNOT_RUN,
            "BLAZE_LAUNCH_BINS_DIR is set for a non-test entry",
        ),
        (Some(raw), EntryForm::Unit | EntryForm::CMain) => Some(raw),
    };
    #[cfg(target_os = "linux")]
    let bins_sock_dir = match (env::var_os("BLAZE_LAUNCH_BINS_SOCK_DIR"), start.form) {
        (None, _) => None,
        (Some(_), EntryForm::Comptime | EntryForm::Hosted) => stop(
            CANNOT_RUN,
            "BLAZE_LAUNCH_BINS_SOCK_DIR is set for a non-test entry",
        ),
        (Some(raw), EntryForm::Unit | EntryForm::CMain) => Some(raw),
    };
    let bin_map = match (env::var_os("BLAZE_LAUNCH_BIN_MAP"), start.form) {
        (None, _) => None,
        (Some(_), EntryForm::Comptime | EntryForm::Hosted) => stop(
            CANNOT_RUN,
            "BLAZE_LAUNCH_BIN_MAP is set for a non-test entry",
        ),
        (Some(raw), EntryForm::Unit | EntryForm::CMain) => Some(raw),
    };
    let open_explore = env::var_os("BLAZE_LAUNCH_EXPLORE").is_some_and(|v| v == "1");
    if let Some(raw_stride) = env::var_os("BLAZE_ALLOC_SAMPLE_BYTES") {
        if let Some(stride) = raw_stride.to_str().and_then(|s| s.parse::<u64>().ok()) {
            blaze_launch_rt::telemetry::set_alloc_sample_bytes(stride);
        }
    }
    // SAFETY: one thread runs (nothing has been started yet), so nothing reads the environment
    // while it changes.
    const LAUNCH_VARS: &[&str] = &[
        "BLAZE_LAUNCH_FD",
        "BLAZE_LAUNCH_STORE",
        "BLAZE_LAUNCH_ENTRY",
        "BLAZE_LAUNCH_NATIVE",
        "BLAZE_LAUNCH_UNWOUND_FILE",
        "BLAZE_LAUNCH_SANDBOX_TEMP",
        "BLAZE_LAUNCH_BINS_DIR",
        "BLAZE_LAUNCH_BINS_SOCK_DIR",
        "BLAZE_LAUNCH_BIN_MAP",
        grants::VAR,
        telemetry::VAR,
        "BLAZE_LAUNCH_EXPLORE",
        "BLAZE_ALLOC_SAMPLE_BYTES",
    ];
    for var in LAUNCH_VARS {
        unsafe { env::remove_var(var) };
    }
    let remap_bins = match bin_map {
        None => false,
        Some(raw) => match run_mode::bin_remap::install(raw.as_encoded_bytes(), shared.local()) {
            Ok(active) => active,
            Err(why) => stop(CANNOT_RUN, &why.to_string()),
        },
    };
    #[cfg(target_os = "linux")]
    if let Some(raw_temp) = sandbox_temp {
        let Ok(temp_c) = CString::new(raw_temp.into_vec()) else {
            stop(CANNOT_RUN, "BLAZE_LAUNCH_SANDBOX_TEMP holds a NUL byte");
        };
        let Ok(store_c) = CString::new(store.as_encoded_bytes()) else {
            stop(CANNOT_RUN, "BLAZE_LAUNCH_STORE holds a NUL byte");
        };
        let cwd_c = match std::env::current_dir() {
            Ok(p) => match CString::new(p.into_os_string().into_vec()) {
                Ok(c) => c,
                Err(_) => stop(CANNOT_RUN, "current working directory holds a NUL byte"),
            },
            Err(why) => stop(CANNOT_RUN, &format!("current working directory could not be read: {why}")),
        };
        let bins_c = match bins_dir {
            None => None,
            Some(raw) => match CString::new(raw.into_vec()) {
                Ok(c) => Some(c),
                Err(_) => stop(CANNOT_RUN, "BLAZE_LAUNCH_BINS_DIR holds a NUL byte"),
            },
        };
        let sock_c = match bins_sock_dir {
            None => None,
            Some(raw) => match CString::new(raw.into_vec()) {
                Ok(c) => Some(c),
                Err(_) => stop(CANNOT_RUN, "BLAZE_LAUNCH_BINS_SOCK_DIR holds a NUL byte"),
            },
        };
        let mut keep_ro_buf: [&core::ffi::CStr; 4] = [store_c.as_c_str(); 4];
        let mut keep_ro_len = 1usize;
        if let Some(slot) = keep_ro_buf.get_mut(keep_ro_len) {
            *slot = cwd_c.as_c_str();
            keep_ro_len += 1;
        }
        if let Some(bins) = bins_c.as_ref()
            && let Some(slot) = keep_ro_buf.get_mut(keep_ro_len)
        {
            *slot = bins.as_c_str();
            keep_ro_len += 1;
        }
        if let Some(sock) = sock_c.as_ref()
            && let Some(slot) = keep_ro_buf.get_mut(keep_ro_len)
        {
            *slot = sock.as_c_str();
            keep_ro_len += 1;
        }
        let keep_ro = keep_ro_buf.get(..keep_ro_len).unwrap_or(&[]);
        let temp_mount = match blaze_sys::linux::confine::enter_test_sandbox(
            temp_c.as_c_str(),
            keep_ro,
            Some(cwd_c.as_c_str()),
            bins_c.as_deref(),
        ) {
            Ok(mount) => mount,
            Err(why) => stop(CANNOT_RUN, &format!("Linux test sandbox refused: {why:?}")),
        };
        if let Ok(mount_str) = temp_mount.to_str() {
            // SAFETY: still single-threaded before any launcher/guest thread starts; point
            // `$TMPDIR` at the exact non-colliding short mount chosen for this test's temp folder.
            unsafe {
                env::set_var("TMPDIR", mount_str);
            }
        }
    }
    let telemetry_on = match telemetry_path {
        Some(path) => {
            let path_buf = PathBuf::from(&path);
            let meta_path = path_buf.with_extension("meta");
            let pkg = std::env::args_os()
                .next()
                .and_then(|s| s.into_string().ok())
                .unwrap_or_else(|| String::from("program"));
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
                .unwrap_or(0);
            let _ = std::fs::write(&meta_path, format!("pid={}\npackage={pkg}\nstarted_ms={now_ms}\n", std::process::id()));
            let Ok(cpath) = CString::new(path.into_vec()) else {
                stop(CANNOT_RUN, &format!("{} holds a NUL byte", telemetry::VAR));
            };
            let mapped = match blaze_launch_rt::telemetry::MappedRing::attach_file(&cpath) {
                Ok(ring) => ring,
                Err(_) => match blaze_launch_rt::telemetry::MappedRing::create(
                    &cpath,
                    blaze_launch_rt::telemetry::DEFAULT_CAP,
                ) {
                    Ok(ring) => ring,
                    Err(why) => stop(CANNOT_RUN, &format!("{} could not open telemetry ring: {why}", telemetry::VAR)),
                },
            };
            let _ = mapped.install_forever();
            if open_explore {
                if let (Ok(exe), Some(runs_dir)) = (std::env::current_exe(), path_buf.parent()) {
                    let _ = std::process::Command::new(exe)
                        .arg("--open-explore")
                        .arg("--runs-dir")
                        .arg(runs_dir)
                        .stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .spawn();
                }
            }
            true
        }
        None => false,
    };
    // SAFETY: asks whether descriptor 3 is open; changes nothing.
    if unsafe { libc::fcntl(3, libc::F_GETFD) } == -1 {
        stop(CANNOT_RUN, "BLAZE_LAUNCH_FD is 3 and descriptor 3 is not open: start the program with `blaze run`");
    }
    // SAFETY: descriptor 3 is open (checked above) and is the parent's socket, which nothing
    // else in this process owns: this is its one owner.
    let socket = unsafe { OwnedFd::from_raw_fd(3) };
    let store = store.into_vec();
    let folder = Folder::new(&store);
    // An ordinary run always goes through `LauncherOwn<Dlsym>` (so `pthread_create` registers
    // every user thread with the sampler/epoch table and `_Unwind_RaiseException` restores
    // span_exit frames). When `BLAZE_LAUNCH_TELEMETRY` is set, `ObservedBind` layers zero-touch
    // alloc/IO/mutex telemetry on top; otherwise plain `LauncherOwn<Dlsym>` pays zero overhead.
    let ordinary = |socket, handed| {
        let end = if unwound_sink.is_some() { End::ExitOrReturnUnwound } else { End::Exit };
        let ran = if telemetry_on {
            run_mode::run(
                socket,
                &folder,
                start.entry,
                native,
                handed,
                ObservedBind::new(LauncherOwn::with_bin_remap(Dlsym, remap_bins)),
                end,
                shared,
            )
        } else {
            run_mode::run(
                socket,
                &folder,
                start.entry,
                native,
                handed,
                LauncherOwn::with_bin_remap(Dlsym, remap_bins),
                end,
                shared,
            )
        };
        match ran {
            Ok(status) => stop(STOPPED, &format!("the run returned status {status} where it ends the process")),
            Err(run_mode::RunModeError::Unwound) => {
                if let Some(sink) = unwound_sink.as_ref() {
                    match sink {
                        UnwoundSink::File(path) => {
                            let _ = std::fs::write(path, b"1");
                        }
                        UnwoundSink::Fd(fd) => {
                            use std::os::fd::AsRawFd;
                            let frame: [u8; 5] = [0, 0, 0, 1, b'1'];
                            // SAFETY: `fd` is the open launcher-owned unwind socket; writes a
                            // single 5-byte length-prefixed sentinel frame before exiting 101.
                            let _ = unsafe { libc::write(fd.as_raw_fd(), frame.as_ptr().cast(), frame.len()) };
                        }
                    }
                    // SAFETY: flush stdio buffers and exit directly with `_exit(101)` so dyld TLS
                    // destructors cannot attempt JIT stub resolution after the control thread stopped.
                    unsafe {
                        libc::fflush(core::ptr::null_mut());
                        libc::_exit(101);
                    }
                }
                stop(STOPPED, "an uncaught panic unwound out of the program's entry")
            }
            Err(why) => why.to_string(),
        }
    };
    let why = match form {
        // Nothing of the hosted world is made for these: the start stays the launcher's own.
        Form::Unit => ordinary(socket, Handed::Unit),
        Form::CMain => ordinary(socket, Handed::CMain),
        // A compile-time program (D175): the layer on descriptor 4 first, then the run with
        // its C names bound through the table, which RETURNS the entry's status (G5); the
        // launcher ends the process as the entry's return from C `main` does, through the
        // layer's `exit`.
        Form::Comptime => 'comptime: {
            if let Err(why) = comptime::install(shared.local()) {
                break 'comptime why.to_string();
            }
            let (argc, argv) = match comptime::argv() {
                Ok(args) => args,
                Err(why) => break 'comptime why.to_string(),
            };
            // SAFETY: `argc` and `argv` are the layer's (`child::argv`): `argc` strings and a
            // null, in words the installed layer keeps, unchanged, for the process's life.
            let args = unsafe { run_mode::ComptimeArgs::new(argc, argv) };
            // The table's data rows bound HERE, on the thread that installed the layer, before
            // `run` starts its threads: its control thread binds the rest from constants and
            // never touches the layer's cell (run_mode/bind.rs `Table`).
            let table = match run_mode::Table::bind(comptime::OS) {
                Ok(table) => table,
                Err(why) => break 'comptime why.to_string(),
            };
            match run_mode::run(socket, &folder, start.entry, None, Handed::Comptime(args), table, End::Return, shared) {
                Ok(status) => comptime::exit(status).to_string(),
                Err(why) => why.to_string(),
            }
        }
        Form::Hosted(granted) => {
            let grants = match grants::parse(granted.as_encoded_bytes()) {
                Ok(grants) => grants,
                Err(why) => stop(CANNOT_RUN, &why.to_string()),
            };
            // The host's world, its arguments and grants, and the table into it: locals of this
            // block, which `run` never leaves once the program starts (it ends the process), so
            // they live, unmoved, until the process ends.
            let host = make_host(os, &grants, shared);
            let table = host.table();
            // SAFETY: `table` is made by `host`, a `Hosting` of this process; both are locals of
            // this block, never moved after this line, and dropped only after `run` returned,
            // which it does only when the program could not start (its control thread joined,
            // no program code ran): `HostTableAt::new`'s contract.
            let at = unsafe { run_mode::HostTableAt::new(core::ptr::NonNull::from(&table).cast()) };
            ordinary(socket, Handed::Hosted(at))
        }
    };
    stop(STOPPED, &format!("the program could not start: {why}"))
}

/// The host of a hosted program, made in its own process (run.md, "Every program runs in the
/// child"): bstd's `Hosting` over this process's real world, giving the process's command line
/// (the program's name first, as the parent spawned it: blaze-cli program.rs `argv`) and the
/// command line's `grants`. The world is made with `os`, the root of real-OS authority `main`
/// took, and is the native driver's over the start directory with no folder above it (a hosted
/// program reaches no file yet), an empty environment (the environment is owed on the hosted
/// wire: run.md, "The hosted world") and an empty command line of its own: the program's
/// arguments are the host's, answered by the table's `arg`.
fn make_host(
    os: &'static blaze_core::Os,
    grants: &blaze_launch::grants::Grants<'_>,
    shared: blaze_own::SharedAlloc<'static>,
) -> Box<bstd::hosting::Hosting> {
    use bstd::args::ArgList;
    use bstd::env::Vars;
    use bstd::io::{Io, Limits, NativeWorld};

    // bstd's host takes its arguments and grants as owned strings: once per run.
    let mut args: Vec<String> = Vec::new();
    for (at, arg) in std::env::args_os().enumerate() {
        match arg.into_string() {
            Ok(arg) => args.push(arg),
            Err(_) => stop(CANNOT_RUN, &format!("argument {at} of this process is not UTF-8, and a hosted program's arguments are text")),
        }
    }
    let grants: Vec<(String, String)> = grants.iter().map(|(kind, name)| (String::from(kind), String::from(name))).collect();
    let world = NativeWorld {
        root: std::path::Path::new("."),
        threads: HOSTED_FILE_THREADS,
        env: Vars::given(Vec::new()),
        args: ArgList::given(Vec::new()),
        host_name: std::sync::Arc::from(""),
        // Drawn at the first `Io::rng`, not here: the start draws no randomness.
        seed: None,
        above: 0,
        limits: Limits::USUAL,
    };
    let io = match Io::native_in(os, world) {
        Ok(io) => io,
        Err(why) => stop(CANNOT_RUN, &format!("the hosted program's world could not be made: {why}")),
    };
    bstd::hosting::Hosting::new(io, args, grants, EXCHANGE, shared)
}

fn viewer_config_from_args() -> blaze_launch_rt::viewer::ViewerConfig {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    let mut cfg = blaze_launch_rt::viewer::ViewerConfig::for_home(&home);
    let mut args = std::env::args().skip(2);
    while let Some(arg) = args.next() {
        if arg == "--runs-dir" {
            if let Some(dir) = args.next() {
                cfg.runs_dir = PathBuf::from(dir);
            }
        } else if arg == "--port" {
            if let Some(p) = args.next().and_then(|s| s.parse::<u16>().ok()) {
                cfg.port = p;
            }
        }
    }
    cfg
}

/// Runs the local loopback telemetry viewer daemon until idle expiry.
fn run_viewer_daemon() -> ! {
    let cfg = viewer_config_from_args();
    match blaze_launch_rt::viewer::start(cfg) {
        Ok(handle) => {
            handle.wait();
            std::process::exit(0);
        }
        Err(why) => {
            // Another viewer is already listening on the port: exit quietly with 0.
            let _ = writeln!(std::io::stderr().lock(), "{why}");
            std::process::exit(0);
        }
    }
}

/// Ensures the local loopback viewer daemon is running and opens `https://ix.dev/blaze/explore`
/// (or local `/explore` when `BLAZE_EXPLORE_LOCAL=1`) ONLY when `active_viewers == 0`.
fn ensure_viewer_and_open_explore() -> ! {
    let cfg = viewer_config_from_args();
    let mut active = blaze_launch_rt::viewer::probe_active_viewers(cfg.port);
    if active.is_none() {
        if let Ok(exe) = std::env::current_exe() {
            let _ = std::process::Command::new(exe)
                .arg("--viewer")
                .arg("--runs-dir")
                .arg(&cfg.runs_dir)
                .arg("--port")
                .arg(cfg.port.to_string())
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
            // Bounded wait (20 * 25ms = 500ms) for the local viewer to bind.
            for _ in 0..20 {
                std::thread::sleep(std::time::Duration::from_millis(25));
                active = blaze_launch_rt::viewer::probe_active_viewers(cfg.port);
                if active.is_some() {
                    break;
                }
            }
        }
    }
    if active == Some(0) {
        let url = if std::env::var_os("BLAZE_EXPLORE_LOCAL").is_some() {
            format!("http://127.0.0.1:{}/explore", cfg.port)
        } else {
            String::from(blaze_launch_rt::viewer::DEFAULT_EXPLORE_URL)
        };
        let _ = blaze_launch_rt::viewer::open_browser_url(&url);
    }
    std::process::exit(0);
}

fn main() {
    // Before any thread and any wait: a stub's wait and the control thread's use direct
    // system calls, which keep no per-thread state (THREADS.md D6; blaze_launch_rt::stub_wait).
    blaze_core::wait::choose(blaze_core::wait::Waits::Direct);
    // An entry that takes no grant yet (a plain `fn main`), and the one heap here that must be
    // shared across threads and freed: a frame is neither, so this stays the global heap's
    // handle until the launcher's entry is handed one (lane H3's report).
    let shared = blaze_own::SharedAlloc::global();
    // The root of real-OS authority, taken once here, in the entry, and handed down: only a
    // hosted program's world is made with it (`make_host`); taking it costs nothing.
    let os = blaze_core::Os::process();
    if let Some(fd) = std::env::var_os("BLAZE_LAUNCH_FD") {
        run_mode(fd, os, shared);
    }
    if let Some(first) = std::env::args().nth(1) {
        if first == "--viewer" {
            run_viewer_daemon();
        } else if first == "--open-explore" {
            ensure_viewer_and_open_explore();
        }
    }
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(why) => stop(CANNOT_RUN, &format!("this process's own path does not read: {why}")),
    };
    // Kept for a restart in place; set once, here.
    let _ = STARTED_FROM.set(exe.clone()); // clone: the path, once a process
    let side = match sidecar(&exe) {
        Ok(found) => found,
        Err(why) => stop(CANNOT_RUN, &why),
    };
    let (socket_path, program) = match side {
        Sidecar::Daemon { socket, program } => (socket, program),
        Sidecar::Run { socket, bin, owner: (owner_suite, owner_row), store, entry, native } => {
            use blaze_launch_rt::run_mode::{self, Dlsym, End, EntryForm, Folder, Handed, LauncherOwn, STOPPED, Start};

            let start = match Start::parse(entry.as_encoded_bytes()) {
                Ok(start) => start,
                Err(why) => stop(CANNOT_RUN, &why.to_string()),
            };
            let handed = match start.form {
                EntryForm::Unit => Handed::Unit,
                EntryForm::CMain => Handed::CMain,
                EntryForm::Hosted | EntryForm::Comptime => {
                    stop(CANNOT_RUN, "sidecar `mode=run` supports `unit` and `c-main` entries only")
                }
            };
            let native = match native {
                None => None,
                Some(raw) => match run_mode::parse_native_start(raw.as_encoded_bytes()) {
                    Ok(start) => Some(start),
                    Err(why) => stop(CANNOT_RUN, &format!("sidecar `native`: {why}")),
                },
            };
            let mut stream = match UnixStream::connect(&socket) {
                Ok(stream) => stream,
                Err(why) => stop(
                    CANNOT_RUN,
                    &format!(
                        "{} runs under blaze's launcher, and nothing serves its code at {}: {why}",
                        exe.display(),
                        socket.display()
                    ),
                ),
            };
            let [b0, b1, b2, b3] = bin.to_le_bytes();
            let [s0, s1, s2, s3] = owner_suite.to_le_bytes();
            let [r0, r1, r2, r3] = owner_row.to_le_bytes();
            let hello = [b0, b1, b2, b3, s0, s1, s2, s3, r0, r1, r2, r3];
            if let Err(why) = stream.write_all(&hello) {
                stop(
                    CANNOT_RUN,
                    &format!("{} could not identify bin {bin} to {}: {why}", exe.display(), socket.display()),
                );
            }
            let fd: OwnedFd = stream.into();
            let store_bytes = store.into_os_string().into_vec();
            let folder = Folder::new(&store_bytes);
            match run_mode::run(
                fd,
                &folder,
                start.entry,
                native,
                handed,
                LauncherOwn::new(Dlsym),
                End::Exit,
                shared,
            ) {
                Ok(status) => stop(STOPPED, &format!("the run returned status {status} where it ends the process (End::Exit)")),
                Err(why) => stop(STOPPED, &why.to_string()),
            }
        }
    };
    let socket = match UnixStream::connect(&socket_path) {
        Ok(socket) => socket,
        Err(why) => stop(
            CANNOT_RUN,
            &format!(
                "{} runs under blaze's launcher, and nothing serves its code at {}: {why}. Run it with \
                 `blaze run`",
                exe.display(),
                socket_path.display()
            ),
        ),
    };
    let driver = match Driver::new(Config::PROGRAM) {
        Ok(driver) => driver,
        Err(why) => stop(CANNOT_RUN, &format!("the launcher could not map its code area: {why}")),
    };
    /// Code given back and made inaccessible, read by the fault handler without a lock
    /// (blaze-launch-rt retired.rs); lives as long as the process.
    static RETIRED: blaze_launch_rt::retired::Table<{ 1 << 14 }> = blaze_launch_rt::retired::Table::new();
    let area = match usize::try_from(Config::PROGRAM.area) {
        Ok(len) => driver.base()..driver.base() + len,
        Err(_) => stop(CANNOT_RUN, "the launcher's code area does not fit this machine's addresses"),
    };
    // SAFETY: the launcher's main thread, before the control thread starts and before any
    // program code runs; nothing else installs a SIGSEGV or SIGBUS action.
    if let Err(why) = unsafe { blaze_launch_rt::signals::install(&RETIRED, area) } {
        stop(CANNOT_RUN, &format!("the launcher could not install its fault handler: {why}"));
    }
    let shared = driver.shared();
    let me = match blaze_launch_rt::threads::adopt(&shared) {
        Ok(me) => me,
        Err(errno) => stop(CANNOT_RUN, &format!("pthread_key_create failed with errno {errno}")),
    };
    let (ready, start) = blaze_launch_rt::ready();
    // The control thread: the one writer of code, for the life of the process. It is never
    // joined: the process ends through the program's `main` or an `exit` of its own.
    let control = std::thread::Builder::new().name("blaze-launch".to_owned()).spawn(move || {
        // A fault here with no program action runs the launcher's own std handler, which
        // names this thread (blaze-launch-rt signals.rs, step 3).
        if let Err(why) = blaze_launch_rt::signals::launcher_thread() {
            stop(CANNOT_RUN, &format!("the launcher's control thread could not be marked: {why}"));
        }
        let mut records = match Loaded::new() {
            Ok(records) => records,
            Err(why) => stop(CANNOT_RUN, &format!("the launcher could not map the program's data: {why}")),
        };
        let (_driver, served) = blaze_launch_rt::run(driver, socket, None, program, &mut records, ready, restart);
        if let Err(why) = served {
            stop(101, &format!("the launcher stopped serving code: {why}"));
        }
    });
    if let Err(why) = control {
        stop(CANNOT_RUN, &format!("the launcher's control thread did not start: {why}"));
    }
    // The wait: for the word to start the program (its statics are defined), or for why it
    // cannot; it ends when the other side answers or its socket closes.
    let start = match start.recv() {
        Ok(Ok(start)) => start,
        Ok(Err(why)) => stop(CANNOT_RUN, &format!("{} cannot start: {why}", exe.display())),
        Err(_) => stop(CANNOT_RUN, "the launcher's control thread ended before the program could start"),
    };

    // The program's arguments, as `main` takes them: this process's, leaked for its life.
    // alloc: the program's `argv`, once
    let args: Vec<*const c_char> = std::env::args_os()
        .map(|it| match CString::new(it.into_vec()) {
            Ok(it) => it.into_raw().cast_const(),
            // The kernel hands a process its arguments as C strings: none holds a NUL.
            Err(_) => stop(CANNOT_RUN, "an argument of this process has a NUL in it"),
        })
        .chain([core::ptr::null()])
        .collect();
    let argc = (args.len() - 1) as c_int;
    let argv = args.leak().as_ptr();
    // SAFETY: reads the C library's pointer to the environment, as a program's start does.
    let envp = unsafe { environ };
    for init in &start.inits {
        // SAFETY: `init` is the address of a static of the program's initializer section,
        // which the launcher placed and never unmaps: one word, the address of a function
        // (its entry thunk) that takes what the system's loader passes an initializer.
        let function: extern "C" fn(c_int, *const *const c_char, *const *const c_char) =
            unsafe { core::mem::transmute((*init as *const usize).read()) };
        function(argc, argv, envp);
    }
    // SAFETY: `start.entry` is the entry slot's thunk, in a mapping that is never unmapped:
    // the program's C `main`, compiled when this first call fires its stub.
    let entry: extern "C" fn(c_int, *const *const c_char) -> c_int =
        unsafe { core::mem::transmute(start.entry) };
    let status = entry(argc, argv);
    // The main thread stays a mutator to the end: `exit` runs the program's `atexit`
    // functions, which are its code.
    core::mem::forget(me);
    // SAFETY: ends the process as a C program's return from `main` does.
    unsafe { libc::exit(status) }
}
