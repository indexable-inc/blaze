//! Fatal signals as rows: a blaze process that dies of SIGSEGV, SIGBUS, SIGILL, SIGFPE or SIGABRT
//! leaves a record of it, with the faulting thread's stack, that the next daemon to open the
//! history turns into a `panics` row of kind `signal` (`crate::history`).
//!
//! - [`install`] (once, on the main thread, after anything else that handles these signals:
//!   blaze-cargo's `signals::record_ends`) puts an async-signal-safe handler on each, on the
//!   alternate signal stack (`SA_ONSTACK`, so a stack overflow still has room; [`install`] gives
//!   the calling thread one if it has none, and std gives each thread it spawns one), with
//!   `SA_SIGINFO`, `SA_RESETHAND` and `SA_NODEFER`.
//! - [`arm`] maps `<history>/crash.<pid>`, one page of the fixed layout below, shared with the
//!   file, and writes its header: magic, layout version, the BLAKE3 hash of the executable and
//!   where it was loaded. The page is the kernel's once written, so it survives the process.
//! - On a fatal signal the handler writes into the page, with no allocation, no lock and only
//!   async-signal-safe calls: the signal, its code and fault address, the thread (id and name),
//!   the faulting PC, then each return address up the frame pointers from the signal's context
//!   ([`crate::stack::walk`], each record probed readable first), the spans the thread was inside
//!   and its last panic, if any (an abort a panic led to). Then it hands the signal to the
//!   handler that was there before (blaze-cargo's, which logs a line and goes on to std's), or,
//!   where that was the default, raises it again, which ends the process with it as it would
//!   have. Only the first fatal signal of a process is recorded.
//! - The history's core (`history::files`) reads each `crash.<pid>` of a process that is gone
//!   through [`read`]: refused, with a typed [`CrashError`], when its layout or its executable is
//!   not this one (its addresses would be named by another binary's symbols); else a row, named
//!   by this executable's symbols at the dead process's load slide, and the file is deleted once
//!   the commit that lists the row consumed it. A file whose process recorded nothing (it ended
//!   cleanly, or was killed) is deleted at once.
//!
//! The record is one fixed page so it can live in any mapping: the flight recorder's rings may
//! move into mapped files of their own per process, and the record is then a region of that
//! file, with the same layout and the same recovery.

use std::{
    fmt, fs, io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicPtr, AtomicU64, Ordering},
};

use crate::panics::{Kind, PanicRow};

/// What a crash file starts with.
pub const MAGIC: [u8; 8] = *b"BLZCRSH\0";
/// The layout's version: bumped when any offset below moves or changes meaning. A file of
/// another is refused, never guessed at.
pub const LAYOUT: u32 = 1;
/// Bytes of a record: one page.
pub const SIZE: usize = 4096;

// The layout: little-endian on every target blaze runs on, each word 8-aligned.
const MAGIC_AT: usize = 0;
const LAYOUT_AT: usize = 8;
const SIZE_AT: usize = 12;
/// The BLAKE3 hash of the executable, 32 bytes; all zero when it could not be read.
const HASH_AT: usize = 16;
const PID_AT: usize = 48;
/// How far the executable was loaded from its file's addresses (`ix_prof::symbols::own_slide`).
const SLIDE_AT: usize = 56;
/// Where it was loaded: its first byte.
const BASE_AT: usize = 64;
/// [`ARMED`], [`WRITING`] or [`WHOLE`].
const STATE_AT: usize = 72;
const SIGNAL_AT: usize = 80;
const CODE_AT: usize = 84;
const ADDR_AT: usize = 88;
const T_NS_AT: usize = 96;
const TID_AT: usize = 104;
const BUILD_AT: usize = 112;
const SPAN_AT: usize = 120;
const DEPTH_AT: usize = 128;
const PCS_AT: usize = 136;
/// Return addresses a record keeps, the faulting PC first.
pub const PCS: usize = 64;
const NAME_LEN_AT: usize = PCS_AT + PCS * 8;
const NAME_AT: usize = NAME_LEN_AT + 8;
const NAME_BYTES: usize = 64;
const OPENED_AT: usize = NAME_AT + NAME_BYTES;
const OPEN_AT: usize = OPENED_AT + 8;
/// Each open span's name: its length, then up to [`OPEN_BYTES`] bytes.
const OPEN_ROW: usize = 64;
const OPEN_BYTES: usize = OPEN_ROW - 8;
const MESSAGE_LEN_AT: usize = OPEN_AT + crate::panics::OPEN * OPEN_ROW;
const MESSAGE_AT: usize = MESSAGE_LEN_AT + 8;
const MESSAGE_BYTES: usize = crate::panics::MESSAGE_BYTES;
const LOCATION_LEN_AT: usize = MESSAGE_AT + MESSAGE_BYTES;
const LOCATION_AT: usize = LOCATION_LEN_AT + 8;
const LOCATION_BYTES: usize = 256;
const END: usize = LOCATION_AT + LOCATION_BYTES;
const _: () = assert!(END <= SIZE, "a record fits its page");
const _: () = assert!(NAME_LEN_AT % 8 == 0 && MESSAGE_LEN_AT % 8 == 0 && LOCATION_LEN_AT % 8 == 0);

/// The process armed the record and nothing happened (yet).
const ARMED: u64 = 0;
/// The handler began writing: a record left so was cut short (the handler itself faulted).
const WRITING: u64 = 1;
const WHOLE: u64 = 2;

/// The name of the crash file of process `pid` in a history's directory.
pub fn file_name(pid: u32) -> String {
    // alloc: a path, once a process or a file recovered
    format!("crash.{pid}")
}

/// Why a crash file was not made or not read: each names the file and what refused it.
#[derive(Debug)]
pub enum CrashError {
    Io {
        path: PathBuf,
        doing: &'static str,
        error: io::Error,
    },
    /// Not a crash record: too short, or no [`MAGIC`].
    NotARecord {
        path: PathBuf,
        bytes: usize,
    },
    /// A record of another layout: its offsets mean something else, so nothing of it is read.
    Layout {
        path: PathBuf,
        version: u32,
        size: u32,
    },
    /// The process that wrote it could not hash its executable, so which binary's symbols name
    /// its addresses is not known.
    Unidentified {
        path: PathBuf,
        pid: u32,
    },
    /// It was written by another blaze executable: this one's symbols would name its addresses
    /// wrongly.
    OtherBlaze {
        path: PathBuf,
        pid: u32,
        theirs: String,
        ours: String,
    },
    /// This executable could not be hashed, so no record can be checked against it.
    OursUnhashed {
        path: PathBuf,
        error: io::Error,
    },
    /// No crash record on this system (no POSIX signals).
    Unsupported,
}

impl fmt::Display for CrashError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CrashError::Io { path, doing, error } => {
                write!(f, "the crash record {}: {doing}: {error}", path.display())
            }
            CrashError::NotARecord { path, bytes } => {
                write!(
                    f,
                    "the crash record {} is no crash record: {bytes} bytes, of {SIZE} starting with {MAGIC:?}",
                    path.display()
                )
            }
            CrashError::Layout { path, version, size } => write!(
                f,
                "the crash record {} is of layout {version} ({size} bytes), and this blaze reads only layout {LAYOUT} ({SIZE} bytes): \
                 refused, as its offsets mean something else (a record is read only by a blaze of its layout)",
                path.display()
            ),
            CrashError::Unidentified { path, pid } => write!(
                f,
                "the crash record {} of pid {pid} has no hash of its blaze executable, so no symbols are known to name its \
                 addresses: refused",
                path.display()
            ),
            CrashError::OtherBlaze { path, pid, theirs, ours } => write!(
                f,
                "the crash record {} of pid {pid} was written by blaze {theirs}, and this is blaze {ours}: refused, as this \
                 executable's symbols would name its addresses wrongly (read it with blaze {theirs})",
                path.display()
            ),
            CrashError::OursUnhashed { path, error } => write!(
                f,
                "the crash record {} can't be checked against this blaze, whose executable could not be hashed: {error}",
                path.display()
            ),
            CrashError::Unsupported => {
                write!(f, "this system has no POSIX signals, so blaze keeps no crash record")
            }
        }
    }
}

impl std::error::Error for CrashError {}

/// A record read back: what the handler wrote of a process's fatal signal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Crash {
    pub pid: u32,
    pub hash: [u8; 32],
    pub slide: u64,
    pub base: u64,
    pub signal: i32,
    pub code: i32,
    pub addr: u64,
    pub t_ns: i64,
    pub tid: u64,
    pub build: u64,
    pub span: u64,
    /// The faulting PC first, then each return address up.
    pub pcs: Vec<u64>,
    pub thread: String,
    /// Outermost first.
    pub open: Vec<String>,
    /// The thread's last panic, if it had one: its message and location.
    pub panic: Option<(String, String)>,
    /// The handler did not finish: the record holds what it wrote before.
    pub cut: bool,
}

/// The name of each fatal signal the handler records.
pub fn signal_name(signal: i32) -> Option<&'static str> {
    #[cfg(unix)]
    {
        Some(match signal {
            libc::SIGSEGV => "SIGSEGV",
            libc::SIGBUS => "SIGBUS",
            libc::SIGILL => "SIGILL",
            libc::SIGFPE => "SIGFPE",
            libc::SIGABRT => "SIGABRT",
            _ => return None,
        })
    }
    #[cfg(not(unix))]
    {
        _ = signal;
        None
    }
}

impl Crash {
    /// Its `panics` row: its addresses named by this executable's symbols and DWARF at its
    /// slide ([`crate::symbols::stack_slid`]), which [`read`] checked is the same executable.
    pub fn row(&self) -> PanicRow {
        let said = blaze_core::signal::described(self.signal).trim_start_matches(", ");
        // alloc: a row of a crash, once a file recovered
        let mut message = match said.is_empty() {
            true => format!("signal {}", self.signal),
            false => said.to_owned(),
        };
        if matches!(signal_name(self.signal), Some("SIGSEGV" | "SIGBUS")) {
            // alloc: as above
            message.push_str(&format!(" at {:#x}", self.addr));
        }
        if self.cut {
            message.push_str(" (the record was cut short: its handler did not finish)");
        }
        if let Some((panicked, _)) = &self.panic {
            // alloc: as above
            message.push_str(&format!(", after the thread's panic: {panicked}"));
        }
        PanicRow {
            build_id: (self.build != 0).then_some(self.build),
            process: self.pid,
            thread: self.tid as u32,
            // clone: a row of a crash
            thread_name: self.thread.clone(),
            t_ns: self.t_ns,
            kind: Kind::Signal,
            job: None,
            span_id: (self.span != 0 && self.span != crate::flight::UNRECORDED)
                .then_some(self.span),
            message: Some(message),
            // clone: a row of a crash
            location: self.panic.as_ref().map(|(_, at)| at.clone()).filter(|it| !it.is_empty()),
            stack: crate::symbols::stack_slid(&self.pcs, self.slide),
            // alloc: a row of a crash
            stack_pcs: self.pcs.iter().rev().copied().collect(),
            boot: None,
            caught: false,
            // clone: a row of a crash
            open_spans: self.open.clone(),
            signal: signal_name(self.signal),
            fault_addr: Some(self.addr),
        }
    }

    /// The `images` row of the executable its addresses are in: this one's ([`read`] checked
    /// the dead process ran the same file), where the dead process had its code (this one's
    /// code start moved by the two slides), with no boot, as its `panics` row has none. Its
    /// libraries' addresses stay in no image: where it loaded them was not recorded. `None`
    /// when this process can't list its own executable.
    pub(crate) fn image(&self) -> Option<crate::batches::ImageRow> {
        let ours = ix_prof::images::loaded().into_iter().find(|it| it.main)?;
        Some(crate::batches::ImageRow {
            base: ours.base.wrapping_sub(ours.slide).wrapping_add(self.slide),
            size: ours.size,
            slide: self.slide,
            build_id: ours.build_id,
            path: ours.path,
            // The hash `read` matched against this executable's.
            blake3: Ok(crate::identity::hex_of(&self.hash)),
            names: ix_prof::symbols::names_kind(),
        })
    }
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    let mut word = [0; 8];
    word.copy_from_slice(&bytes[at..at + 8]);
    u64::from_le_bytes(word)
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    let mut word = [0; 4];
    word.copy_from_slice(&bytes[at..at + 4]);
    u32::from_le_bytes(word)
}

/// The text of `len_at`'s length and the bytes after it, cut at `cap`.
fn text_at(bytes: &[u8], len_at: usize, cap: usize) -> String {
    let len = (u64_at(bytes, len_at) as usize).min(cap);
    String::from_utf8_lossy(&bytes[len_at + 8..len_at + 8 + len]).into_owned()
}

/// The record in `bytes`, the file `path`: `None` when its process recorded nothing; refused
/// when it is no record, of another layout, or of another executable than `ours` (this one's
/// hash).
pub fn read(path: &Path, bytes: &[u8], ours: &[u8; 32]) -> Result<Option<Crash>, CrashError> {
    // alloc: an error
    let path_of = || path.to_owned();
    if bytes.len() < 16 || bytes[MAGIC_AT..MAGIC_AT + 8] != MAGIC {
        return Err(CrashError::NotARecord { path: path_of(), bytes: bytes.len() });
    }
    let (version, size) = (u32_at(bytes, LAYOUT_AT), u32_at(bytes, SIZE_AT));
    if version != LAYOUT || size as usize != SIZE || bytes.len() < SIZE {
        return Err(CrashError::Layout { path: path_of(), version, size });
    }
    let pid = u32_at(bytes, PID_AT);
    let mut hash = [0u8; 32];
    hash.copy_from_slice(&bytes[HASH_AT..HASH_AT + 32]);
    if hash == [0; 32] {
        return Err(CrashError::Unidentified { path: path_of(), pid });
    }
    if &hash != ours {
        return Err(CrashError::OtherBlaze {
            path: path_of(),
            pid,
            theirs: crate::identity::hex_of(&hash),
            ours: crate::identity::hex_of(ours),
        });
    }
    let state = u64_at(bytes, STATE_AT);
    if state == ARMED {
        return Ok(None);
    }
    let depth = (u64_at(bytes, DEPTH_AT) as usize).min(PCS);
    let opened = (u64_at(bytes, OPENED_AT) as usize).min(crate::panics::OPEN);
    let message = text_at(bytes, MESSAGE_LEN_AT, MESSAGE_BYTES);
    let location = text_at(bytes, LOCATION_LEN_AT, LOCATION_BYTES);
    Ok(Some(Crash {
        pid,
        hash,
        slide: u64_at(bytes, SLIDE_AT),
        base: u64_at(bytes, BASE_AT),
        signal: u32_at(bytes, SIGNAL_AT) as i32,
        code: u32_at(bytes, CODE_AT) as i32,
        addr: u64_at(bytes, ADDR_AT),
        t_ns: u64_at(bytes, T_NS_AT) as i64,
        tid: u64_at(bytes, TID_AT),
        build: u64_at(bytes, BUILD_AT),
        span: u64_at(bytes, SPAN_AT),
        // alloc: a crash read back
        pcs: (0..depth).map(|at| u64_at(bytes, PCS_AT + at * 8)).collect(),
        thread: text_at(bytes, NAME_LEN_AT, NAME_BYTES),
        // alloc: a crash read back
        open: (0..opened).map(|at| text_at(bytes, OPEN_AT + at * OPEN_ROW, OPEN_BYTES)).collect(),
        panic: (!message.is_empty() || !location.is_empty()).then_some((message, location)),
        cut: state != WHOLE,
    }))
}

/// The row of the crash record of process `pid` in the history `dir`, read and left in place (the
/// next daemon's history recovers it): what the CLI a daemon hung up on says of how it died.
/// `None` when it left no record, or recorded nothing.
pub fn of_process(dir: &Path, pid: u32) -> Result<Option<PanicRow>, CrashError> {
    let path = dir.join(file_name(pid));
    let bytes = match fs::read(&path) {
        Ok(it) => it,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(CrashError::Io { path, doing: "reading", error }),
    };
    // alloc: an error
    let ours = crate::identity::binary_hash()
        .map_err(|error| CrashError::OursUnhashed { path: path.clone(), error })?;
    Ok(read(&path, &bytes, &ours)?.map(|crash| crash.row()))
}

/// What the history's core found of crash records in a history's directory as it opened.
#[derive(Debug, Default)]
pub struct Recovered {
    /// A row of each crash recorded.
    pub rows: Vec<PanicRow>,
    /// The `images` row of each crash's executable, by its process: what names its row's
    /// `stack_pcs` ([`Crash::image`]).
    pub(crate) images: Vec<(u32, crate::batches::ImageRow)>,
    /// Their files' names in the history's directory: deleted once the commit that lists
    /// their rows consumed them ([`Written`]).
    pub files: Vec<String>,
    /// Each one's process and the instant of its signal, as `rows` and `files`: what the
    /// manifest names a record by once a file it lists holds its row
    /// (`history::manifest::Consumed::crashes`).
    pub(crate) ids: Vec<(u32, i64)>,
    /// The files refused, each renamed `crash.<pid>.refused` so it is not read again, and why.
    pub refused: Vec<CrashError>,
}

/// Crash records whose rows a flush wrote: listed in the manifest's commit as consumed
/// (`ids`), then deleted (`files`), so a crash between the two never writes a row twice.
#[derive(Debug, Default)]
pub(crate) struct Written {
    /// Their files' names in the history's directory.
    pub(crate) files: Vec<String>,
    pub(crate) ids: Vec<(u32, i64)>,
}

impl Written {
    pub(crate) fn is_empty(&self) -> bool {
        self.files.is_empty() && self.ids.is_empty()
    }

    pub(crate) fn append(&mut self, other: &mut Written) {
        self.files.append(&mut other.files);
        self.ids.append(&mut other.ids);
    }
}

impl Recovered {
    /// The rows are written to a file not yet listed: they leave this, and their records
    /// go once the commit that lists them consumed them ([`Written`]).
    pub(crate) fn take_written(&mut self) -> Written {
        self.rows.clear();
        Written { files: std::mem::take(&mut self.files), ids: std::mem::take(&mut self.ids) }
    }
}

/// The pid of a crash record named `crash.<pid>`; `None` for any other name (a refused one,
/// `crash.<pid>.refused`, among them).
pub(crate) fn pid_of(name: &str) -> Option<u32> {
    name.strip_prefix("crash.")?.parse().ok()
}

/// A record's page, mapped: what the handler writes through.
#[derive(Clone, Copy)]
struct Page(*mut u8);

impl Page {
    /// # Safety
    ///
    /// `at + 8 <= SIZE`, `at` 8-aligned, and the page is mapped writable (it is never unmapped).
    unsafe fn put_u64(self, at: usize, value: u64) {
        // SAFETY: as this function's own `# Safety` says; the page is page-aligned, so `at` is
        // 8-aligned in memory too.
        unsafe { self.0.add(at).cast::<u64>().write_volatile(value.to_le()) };
    }

    /// # Safety
    ///
    /// As [`Page::put_u64`], 4 bytes.
    unsafe fn put_u32(self, at: usize, value: u32) {
        // SAFETY: as this function's own `# Safety` says.
        unsafe { self.0.add(at).cast::<u32>().write_volatile(value.to_le()) };
    }

    /// The text field at `len_at` (its length word, then `cap` bytes) set to `bytes`, cut.
    ///
    /// # Safety
    ///
    /// `len_at + 8 + cap <= SIZE`, `len_at` 8-aligned, the page mapped writable.
    unsafe fn put_text(self, len_at: usize, cap: usize, bytes: &[u8]) {
        let n = bytes.len().min(cap);
        // SAFETY: as this function's own `# Safety` says: `n <= cap` bytes after the length.
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.0.add(len_at + 8), n);
            self.put_u64(len_at, n as u64);
        }
    }

    /// The state word, as the handler's one atomic: the first fatal signal takes it.
    fn state(self) -> &'static AtomicU64 {
        // SAFETY: `STATE_AT` is 8-aligned inside the page, which is mapped for the process's life
        // (never unmapped) and written as this word only through this atomic.
        unsafe { &*self.0.add(STATE_AT).cast::<AtomicU64>() }
    }

    /// Writes the header: what [`read`] checks before anything else.
    ///
    /// # Safety
    ///
    /// The page is mapped writable, [`SIZE`] bytes, and no handler writes it yet.
    unsafe fn header(self, pid: u32, hash: &[u8; 32], slide: u64, base: u64) {
        // SAFETY: as this function's own `# Safety` says; each offset is inside the page.
        unsafe {
            std::ptr::copy_nonoverlapping(MAGIC.as_ptr(), self.0.add(MAGIC_AT), 8);
            self.put_u32(LAYOUT_AT, LAYOUT);
            self.put_u32(SIZE_AT, SIZE as u32);
            std::ptr::copy_nonoverlapping(hash.as_ptr(), self.0.add(HASH_AT), 32);
            self.put_u32(PID_AT, pid);
            self.put_u64(SLIDE_AT, slide);
            self.put_u64(BASE_AT, base);
        }
        self.state().store(ARMED, Ordering::Release);
    }

    /// Records `fault` on the calling thread: async-signal-safe (atomics, plain stores into the
    /// page, and the system calls of [`crate::stack::probe`], `clock_gettime` and the thread's
    /// id and name). Only the first fault of a process is recorded; each word the walk adds is
    /// counted as it is written, so a fault inside the walk leaves what came before.
    fn record(self, fault: &Fault) {
        if self
            .state()
            .compare_exchange(ARMED, WRITING, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
        {
            return;
        }
        let tid = crate::sampler::os_tid();
        // SAFETY: each offset and length below is inside the page (the layout's asserts), which
        // `arm` mapped writable for the process's life; the state word says this thread alone
        // writes it now.
        unsafe {
            self.put_u32(SIGNAL_AT, fault.signal as u32);
            self.put_u32(CODE_AT, fault.code as u32);
            self.put_u64(ADDR_AT, fault.addr);
            self.put_u64(T_NS_AT, crate::clock::mono_ns() as u64);
            self.put_u64(TID_AT, tid);
            let mut name = [0u8; NAME_BYTES];
            let named = crate::panics::system_thread_name(&mut name);
            self.put_text(NAME_LEN_AT, NAME_BYTES, &name[..named]);
            self.put_u64(DEPTH_AT, 0);
        }
        let mut depth = 0;
        let mut push = |pc: u64| {
            if pc == 0 || depth == PCS {
                return depth < PCS;
            }
            // SAFETY: `depth < PCS`, so the word is inside the pcs field; as above.
            unsafe {
                self.put_u64(PCS_AT + depth * 8, pc);
                self.put_u64(DEPTH_AT, depth as u64 + 1);
            }
            depth += 1;
            depth < PCS
        };
        if push(fault.pc & crate::stack::ADDRESS) {
            crate::stack::walk(fault.fp, &mut crate::stack::probed, &mut push);
        }
        let mut opened = 0;
        let innermost = crate::flight::open_of_thread(tid as u32, &mut |_, span| {
            if opened < crate::panics::OPEN {
                // SAFETY: `opened < OPEN`, so the row is inside the open field; as above.
                unsafe { self.put_text(OPEN_AT + opened * OPEN_ROW, OPEN_BYTES, span.as_bytes()) };
                opened += 1;
            }
        });
        // SAFETY: the page's own fields; as above. The message and location fields are written
        // through slices of the page this thread alone writes now.
        unsafe {
            self.put_u64(OPENED_AT, opened as u64);
            self.put_u64(SPAN_AT, innermost);
            self.put_u64(BUILD_AT, u64::from(crate::flight::build_of(innermost)));
            let message = std::slice::from_raw_parts_mut(self.0.add(MESSAGE_AT), MESSAGE_BYTES);
            let location = std::slice::from_raw_parts_mut(self.0.add(LOCATION_AT), LOCATION_BYTES);
            let (said, at) =
                crate::panics::last_of_thread(tid, message, location).unwrap_or((0, 0));
            self.put_u64(MESSAGE_LEN_AT, said as u64);
            self.put_u64(LOCATION_LEN_AT, at as u64);
        }
        self.state().store(WHOLE, Ordering::Release);
    }
}

/// A fatal signal as the handler read it: the signal, its code and address, and the faulting
/// thread's PC and frame pointer from its context.
#[derive(Clone, Copy, Debug)]
struct Fault {
    signal: i32,
    code: i32,
    addr: u64,
    pc: u64,
    fp: u64,
}

/// The page [`arm`] mapped, null before: what the handler writes to.
static PAGE: AtomicPtr<u8> = AtomicPtr::new(std::ptr::null_mut());

/// Maps `<dir>/crash.<pid>` (made, or made again) as this process's crash record and writes its
/// header. Once a process: a later call keeps the first record. The executable's hash is read
/// here (once a process, `identity::binary_hash`): call it after the history opened, which reads
/// it too. A record that can't be made is the error, naming the file.
#[cfg(unix)]
pub fn arm(dir: &Path) -> Result<PathBuf, CrashError> {
    use std::os::fd::AsRawFd as _;

    let pid = std::process::id();
    let path = dir.join(file_name(pid));
    if !PAGE.load(Ordering::Acquire).is_null() {
        return Ok(path);
    }
    crate::stack::ready();
    // alloc: an error
    let error = |doing: &'static str| {
        move |error: io::Error| CrashError::Io { path: dir.join(file_name(pid)), doing, error }
    };
    let hash = crate::identity::binary_hash()
        .map_err(|error| CrashError::OursUnhashed { path: path.clone(), error })?;
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .map_err(error("making"))?;
    file.set_len(SIZE as u64).map_err(error("sizing"))?;
    // SAFETY: a new shared mapping of the `SIZE` bytes the file was just given, readable and
    // writable; it outlives the descriptor, which closes as `file` drops.
    let at = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            SIZE,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            file.as_raw_fd(),
            0,
        )
    };
    if at == libc::MAP_FAILED {
        return Err(error("mapping")(io::Error::last_os_error()));
    }
    let page = Page(at.cast());
    // SAFETY: the mapping is `SIZE` writable bytes, never unmapped, and no handler has it yet
    // (it is published below).
    unsafe {
        page.header(
            pid,
            &hash,
            ix_prof::symbols::own_slide().unwrap_or(0),
            ix_prof::symbols::own_load_base().unwrap_or(0),
        )
    };
    if PAGE
        .compare_exchange(std::ptr::null_mut(), page.0, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        // Another thread armed first: this mapping is let go, its file is the same path.
        // SAFETY: the mapping made above, never published, unmapped once.
        unsafe { libc::munmap(at, SIZE) };
    }
    Ok(path)
}

#[cfg(not(unix))]
pub fn arm(_dir: &Path) -> Result<PathBuf, CrashError> {
    Err(CrashError::Unsupported)
}

/// The signals the handler records.
#[cfg(unix)]
const FATAL: [libc::c_int; 5] =
    [libc::SIGSEGV, libc::SIGBUS, libc::SIGILL, libc::SIGFPE, libc::SIGABRT];

/// Each of [`FATAL`]'s handler and flags before [`install`]: atomics, as the handler reads them.
#[cfg(unix)]
static BEFORE: [(std::sync::atomic::AtomicUsize, std::sync::atomic::AtomicI32); 5] = [const {
    (std::sync::atomic::AtomicUsize::new(libc::SIG_DFL), std::sync::atomic::AtomicI32::new(0))
}; 5];

/// Puts the recording handler on each of [`FATAL`], before whatever was there (which it hands
/// each signal to after recording), and gives the calling thread an alternate signal stack if it
/// has none. Once a process. Call it after anything else that installs a handler for these
/// signals and leaves one that is there alone (blaze-cargo's `signals::record_ends`), on the
/// main thread. Records nothing until [`arm`].
#[cfg(unix)]
pub fn install() {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    INSTALLED.call_once(|| {
        crate::stack::ready();
        altstack();
        for (at, signal) in FATAL.into_iter().enumerate() {
            // SAFETY: `sigaction` is integers, a mask of integers and a handler address, for
            // which all-zero bytes are a valid value (no handler, no flags, an empty mask).
            let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
            // SAFETY: `old` is live for the call to fill; a null new action changes nothing.
            if unsafe { libc::sigaction(signal, std::ptr::null(), &mut old) } != 0 {
                continue;
            }
            BEFORE[at].0.store(old.sa_sigaction, Ordering::Release);
            BEFORE[at].1.store(old.sa_flags, Ordering::Release);
            // SAFETY: as for `old`.
            let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
            action.sa_sigaction = on_fatal
                as extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void)
                as libc::sighandler_t;
            // On the alternate stack: a stack overflow leaves no room on the thread's own. Reset
            // as it is entered and not blocked in it, so the handler before (or the default)
            // ends the process with the same signal when it raises it again.
            action.sa_flags =
                libc::SA_SIGINFO | libc::SA_ONSTACK | libc::SA_RESETHAND | libc::SA_NODEFER;
            // SAFETY: `action.sa_mask` is a live `sigset_t` of this frame for the call to fill.
            unsafe { libc::sigemptyset(&mut action.sa_mask) };
            // SAFETY: `action` is live and whole. `on_fatal` only writes the mapped page through
            // atomics and plain stores, calls async-signal-safe functions (`write`, `read`,
            // `clock_gettime`, the thread's id and name, `sigaction`, `raise`), and then the
            // handler that was there, as it would have been called: it neither allocates nor
            // locks, which could deadlock a process interrupted inside malloc.
            unsafe { libc::sigaction(signal, &action, std::ptr::null_mut()) };
        }
    });
}

#[cfg(not(unix))]
pub fn install() {}

/// Gives the calling thread an alternate signal stack when it has none: std gives each thread
/// it spawns one, and the main thread when it installs its own handler, but a process whose
/// handler std did not install (one something else handled first) has none on its main thread.
#[cfg(unix)]
fn altstack() {
    // SAFETY: `stack_t` is a pointer, a size and flags; zeroed is a valid value to fill.
    let mut old: libc::stack_t = unsafe { std::mem::zeroed() };
    // SAFETY: `old` is live for the call to fill; a null new stack changes nothing.
    if unsafe { libc::sigaltstack(std::ptr::null(), &mut old) } != 0
        || old.ss_flags & libc::SS_DISABLE == 0
    {
        return;
    }
    const BYTES: usize = 64 << 10;
    // SAFETY: a new private anonymous mapping, never unmapped: the thread's signal stack for the
    // process's life.
    let at = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            BYTES,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANON,
            -1,
            0,
        )
    };
    if at == libc::MAP_FAILED {
        return;
    }
    let stack = libc::stack_t { ss_sp: at, ss_flags: 0, ss_size: BYTES };
    // SAFETY: `stack` names the mapping above, `BYTES` long and never unmapped.
    unsafe { libc::sigaltstack(&stack, std::ptr::null_mut()) };
}

/// The handler: records the fault when armed, then hands it on.
#[cfg(unix)]
extern "C" fn on_fatal(
    signal: libc::c_int,
    info: *mut libc::siginfo_t,
    context: *mut libc::c_void,
) {
    let page = PAGE.load(Ordering::Acquire);
    // SAFETY: the kernel passes an `SA_SIGINFO` handler a live `siginfo_t` and `ucontext_t`.
    let fault = unsafe { fault_of(signal, info, context) };
    if !page.is_null() {
        Page(page).record(&fault);
    }
    let Some(at) = FATAL.iter().position(|it| *it == signal) else { return };
    let (handler, flags) =
        (BEFORE[at].0.load(Ordering::Acquire), BEFORE[at].1.load(Ordering::Acquire));
    if handler == libc::SIG_DFL || handler == libc::SIG_IGN {
        // `SA_RESETHAND` made the action the default as this handler was entered, and
        // `SA_NODEFER` left the signal unblocked, so raising it again ends the process with it
        // now: a sent one (`abort`, `kill`) comes no second time by itself, and which codes
        // mean "sent" differs by system (macOS's `SI_USER` is positive), so a fault is raised
        // again too rather than left to come again as the instruction reruns.
        // SAFETY: `raise` is async-signal-safe.
        unsafe { libc::raise(signal) };
        return;
    }
    if flags & libc::SA_SIGINFO != 0 {
        // SAFETY: `handler` was the `sa_sigaction` of an `SA_SIGINFO` action, so it is an
        // `extern "C" fn(c_int, *mut siginfo_t, *mut c_void)`, given what this one was.
        let handler: extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void) =
            unsafe { std::mem::transmute(handler) };
        handler(signal, info, context);
    } else {
        // SAFETY: an action without `SA_SIGINFO` has an `extern "C" fn(c_int)` handler.
        let handler: extern "C" fn(libc::c_int) = unsafe { std::mem::transmute(handler) };
        handler(signal);
    }
}

/// The fault `info` and `context` describe.
///
/// # Safety
///
/// `info` and `context` are the live arguments of an `SA_SIGINFO` handler (either may be null).
#[cfg(unix)]
unsafe fn fault_of(
    signal: libc::c_int,
    info: *const libc::siginfo_t,
    context: *const libc::c_void,
) -> Fault {
    let (code, addr) = match info.is_null() {
        true => (0, 0),
        false => {
            // SAFETY: as this function's own `# Safety` says.
            let code = unsafe { (*info).si_code };
            #[cfg(any(target_os = "linux", target_os = "android"))]
            // SAFETY: as above.
            let addr = unsafe { (*info).si_addr() };
            #[cfg(not(any(target_os = "linux", target_os = "android")))]
            // SAFETY: as above.
            let addr = unsafe { (*info).si_addr };
            (code, addr.addr() as u64)
        }
    };
    // SAFETY: as this function's own `# Safety` says.
    let (pc, fp) = if context.is_null() { (0, 0) } else { unsafe { registers(context) } };
    Fault { signal, code, addr, pc, fp }
}

/// The faulting thread's PC and frame pointer from a signal's `ucontext_t`.
///
/// # Safety
///
/// `context` is the live, non-null `ucontext_t` of an `SA_SIGINFO` handler.
#[cfg(all(target_vendor = "apple", target_arch = "aarch64"))]
unsafe fn registers(context: *const libc::c_void) -> (u64, u64) {
    /// `struct __darwin_ucontext`, up to its machine context.
    #[repr(C)]
    struct Context {
        onstack: i32,
        sigmask: u32,
        /// `stack_t`: a pointer, a size and flags (padded).
        stack: [u64; 3],
        link: *const libc::c_void,
        mcsize: usize,
        mcontext: *const Machine,
    }
    /// `struct __darwin_mcontext64` on arm64: the exception state, then the thread state.
    #[repr(C)]
    struct Machine {
        far: u64,
        esr: u32,
        exception: u32,
        x: [u64; 29],
        fp: u64,
        lr: u64,
        sp: u64,
        pc: u64,
    }
    // SAFETY: as this function's own `# Safety` says; on arm64 macOS the context is a
    // `__darwin_ucontext` whose `uc_mcontext` points at the thread's saved state.
    let machine = unsafe { (*context.cast::<Context>()).mcontext };
    if machine.is_null() {
        return (0, 0);
    }
    // SAFETY: as above, a live `__darwin_mcontext64`.
    unsafe { ((*machine).pc, (*machine).fp) }
}

/// As above.
///
/// # Safety
///
/// As above.
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
unsafe fn registers(context: *const libc::c_void) -> (u64, u64) {
    // SAFETY: as this function's own `# Safety` says.
    let machine = unsafe { &(*context.cast::<libc::ucontext_t>()).uc_mcontext };
    (machine.pc, machine.regs[29])
}

/// As above.
///
/// # Safety
///
/// As above.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
unsafe fn registers(context: *const libc::c_void) -> (u64, u64) {
    // SAFETY: as this function's own `# Safety` says.
    let machine = unsafe { &(*context.cast::<libc::ucontext_t>()).uc_mcontext };
    (machine.gregs[libc::REG_RIP as usize] as u64, machine.gregs[libc::REG_RBP as usize] as u64)
}

/// Elsewhere the context is not read: a record holds the signal and its address, no stack.
///
/// # Safety
///
/// None needed.
#[cfg(all(
    unix,
    not(any(
        all(target_vendor = "apple", target_arch = "aarch64"),
        all(target_os = "linux", any(target_arch = "aarch64", target_arch = "x86_64"))
    ))
))]
unsafe fn registers(_context: *const libc::c_void) -> (u64, u64) {
    (0, 0)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("blaze-crash-{name}-{}", std::process::id()));
        _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A pid no process has: past every system's largest.
    const GONE: u32 = 0x7fff_fff0;

    /// A page of a file of its own, mapped as `arm` maps the process's, for a test to write
    /// through the handler's own code.
    fn page_in(path: &Path, pid: u32) -> Page {
        use std::os::fd::AsRawFd as _;
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .unwrap();
        file.set_len(SIZE as u64).unwrap();
        // SAFETY: as in `arm`: a shared mapping of the file's `SIZE` bytes, kept for the test
        // process's life.
        let at = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                SIZE,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        assert_ne!(at, libc::MAP_FAILED);
        let page = Page(at.cast());
        let hash = crate::identity::binary_hash().unwrap();
        // SAFETY: the mapping above, no handler has it.
        unsafe {
            page.header(
                pid,
                &hash,
                ix_prof::symbols::own_slide().unwrap(),
                ix_prof::symbols::own_load_base().unwrap(),
            )
        };
        page
    }

    #[inline(never)]
    fn faults_here(page: Page) {
        let fault = Fault {
            signal: libc::SIGSEGV,
            code: 1,
            addr: 0x10,
            pc: faults_here as fn(Page) as usize as u64 + 4,
            fp: crate::stack::frame_pointer(),
        };
        page.record(&fault);
        std::hint::black_box(());
    }

    /// The record of `path` as the history's core reads it: the file's bytes through [`read`],
    /// against this executable's hash.
    fn read_back(path: &Path) -> Result<Option<Crash>, CrashError> {
        let ours = crate::identity::binary_hash().unwrap();
        read(path, &fs::read(path).unwrap(), &ours)
    }

    /// A record written by the handler's own code round-trips through [`read`]: one `signal`
    /// row naming the signal, its address and the function that faulted, and the image that
    /// names its stack.
    #[test]
    fn a_crash_file_round_trips_through_recovery() {
        crate::stack::ready();
        let dir = scratch("round-trip");
        let path = dir.join(file_name(GONE));
        let page = page_in(&path, GONE);
        faults_here(page);
        let crash = read_back(&path).unwrap().expect("a crash recorded");
        let row = &crash.row();
        assert_eq!(row.kind, Kind::Signal);
        assert_eq!(row.process, GONE);
        assert_eq!(row.signal, Some("SIGSEGV"));
        assert_eq!(row.fault_addr, Some(0x10));
        assert!(!row.caught);
        assert!(
            row.message.as_deref().is_some_and(|it| it.contains("SIGSEGV") && it.contains("0x10")),
            "{:?}",
            row.message
        );
        assert!(
            row.stack.len() >= row.stack_pcs.len(),
            "a frame of the unwind is one name or more"
        );
        assert!(
            row.stack.iter().all(|it| !it.contains("+0x")),
            "named by function: {:#?}",
            row.stack
        );
        assert!(row.stack.last().is_some_and(|it| it.contains("faults_here")), "{:#?}", row.stack);
        assert!(
            row.stack.iter().any(|it| it.contains("a_crash_file_round_trips_through_recovery")),
            "{:#?}",
            row.stack
        );
        assert_eq!(row.boot, None);
        let image = crash.image().expect("an image of the crash");
        assert_eq!(crash.pid, GONE);
        assert_eq!(image.blake3.as_deref(), Ok(crate::identity::binary().unwrap().as_str()));
        let inner = *row.stack_pcs.last().unwrap();
        assert!(
            inner >= image.base && inner - image.base < image.size,
            "the faulting pc is in the image's code"
        );
        let batch = crate::panics::batch(std::slice::from_ref(row)).unwrap();
        assert_eq!(batch.num_rows(), 1);
        _ = fs::remove_dir_all(&dir);
    }

    /// A record whose process recorded nothing reads as no row; a second fault of the same
    /// process is not recorded over the first.
    #[test]
    fn only_the_first_fault_is_recorded_and_an_armed_record_alone_is_no_row() {
        let dir = scratch("armed");
        let path = dir.join(file_name(GONE));
        let page = page_in(&path, GONE);
        assert!(read_back(&path).unwrap().is_none(), "an armed record holds no crash");
        fs::remove_file(&path).unwrap();
        let page2 = page_in(&path, GONE);
        page2.record(&Fault { signal: libc::SIGBUS, code: 1, addr: 1, pc: 0, fp: 0 });
        page2.record(&Fault { signal: libc::SIGSEGV, code: 1, addr: 2, pc: 0, fp: 0 });
        let crash = read_back(&path).unwrap().expect("the first fault recorded");
        assert_eq!(crash.row().signal, Some("SIGBUS"));
        _ = page;
        _ = fs::remove_dir_all(&dir);
    }

    /// A record of another layout, or of another executable, is refused with a typed error naming
    /// the file. (Setting a refused file aside is the history's core's, tested there.)
    #[test]
    fn a_record_of_another_layout_or_blaze_is_refused() {
        let dir = scratch("refused");
        let path = dir.join(file_name(GONE));
        let page = page_in(&path, GONE);
        page.record(&Fault { signal: libc::SIGSEGV, code: 1, addr: 1, pc: 0, fp: 0 });
        let mut bytes = fs::read(&path).unwrap();
        let ours = crate::identity::binary_hash().unwrap();
        assert!(read(&path, &bytes, &ours).unwrap().is_some());
        bytes[LAYOUT_AT..LAYOUT_AT + 4].copy_from_slice(&(LAYOUT + 1).to_le_bytes());
        let refused = read(&path, &bytes, &ours).unwrap_err();
        assert!(
            matches!(refused, CrashError::Layout { version, .. } if version == LAYOUT + 1),
            "{refused}"
        );
        assert!(refused.to_string().contains(&path.display().to_string()));
        bytes[LAYOUT_AT..LAYOUT_AT + 4].copy_from_slice(&LAYOUT.to_le_bytes());
        let mut other = ours;
        other[0] ^= 1;
        let refused = read(&path, &bytes, &other).unwrap_err();
        assert!(matches!(refused, CrashError::OtherBlaze { pid: GONE, .. }), "{refused}");
        assert!(matches!(read(&path, &bytes[..8], &ours), Err(CrashError::NotARecord { .. })));
        _ = page;
        _ = fs::remove_dir_all(&dir);
    }

    /// Only a record's own name gives a pid: a refused one set aside is not read again.
    #[test]
    fn a_record_s_pid_is_its_name_s() {
        assert_eq!(pid_of("crash.12"), Some(12));
        assert_eq!(pid_of("crash.12.refused"), None);
        assert_eq!(pid_of("flight-12.map"), None);
    }
}
