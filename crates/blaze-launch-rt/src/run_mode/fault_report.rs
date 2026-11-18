//! What a run's crash says (rule 19: a crash in a run says what and where). A program under
//! `blaze run` that faults (SIGSEGV, SIGBUS), runs an undefined instruction (SIGILL) or a trap
//! (SIGTRAP: `brk`, what `core::intrinsics::abort` and a failed check lower to) dies by that
//! signal, as it would linked; before it dies the handler here writes ONE line to standard
//! error naming the signal, the pc and the faulting address, each as WHAT IS PLACED THERE:
//!
//!     blaze run: the program was killed by SIGBUS: pc 0x7002aa78c0 is +0x40 in the code of
//!     function 3f2a.. (record 9c01..); it wrote 0x7002aa6410, which is +0x0 in the read-only
//!     datum of static 51d0.. (record 77ab..)
//!
//! (one line). The table it reads is the run's placements, appended by the control thread (the
//! one writer of code and words) as each record's code and relro and each static's datum is
//! published ([`note`], from [`super::Memory::note`] of the native memory), and the regions of
//! the run's one reservation (slot words, address words, the C names' words, the statics'
//! words, stubs, thunks, the code area, the hot region).
//!
//! The handler is async-signal-safe: atomic loads, reads of rows published before the count it
//! loaded, byte copies into a stack buffer, one `write(2)`, `sigaction`. It then hands the
//! signal on: to the action that was installed before it (the launcher's std handler, which
//! names a stack overflow on a guard page and otherwise puts the default back and returns), or
//! the default, so the fault repeats and the process ends by the signal: `blaze run`'s status
//! stays 128 + the signal and the system's crash report is still made.
//!
//! The names are the symbols' and records' first eight bytes in hex: the child knows no paths.
//! The parent knows each symbol's item (TO BUILD: the line sent over the socket so the parent
//! prints the paths; this module's [`describe`] is the one place both would read).

use core::ffi::{c_int, c_void};
use core::ptr;
use core::sync::atomic::{AtomicPtr, AtomicU64, AtomicUsize, Ordering};

use blaze_code::record::Symbol as CodeSymbol;
use blaze_launch::wire::RecordId;

use super::RunModeError;

/// What a placement row holds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Kind {
    /// A function's code.
    Code,
    /// A function's relro part (its words), read-only in the run view.
    Relro,
    /// A static's (or vtable's, or thread-local template's) read-only datum.
    ReadOnlyDatum,
    /// A static's datum in the run's writable data area (`static mut`, interior mutability).
    WritableDatum,
}

/// One placement: `len` bytes at `at` (run view) of `kind`, of `symbol`'s `record`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct Row {
    pub at: u64,
    pub len: u64,
    pub kind: Kind,
    pub symbol: [u8; 32],
    pub record: [u8; 32],
}

impl Row {
    pub fn new(kind: Kind, at: u64, len: u64, symbol: CodeSymbol, record: RecordId) -> Row {
        Row { at, len, kind, symbol: symbol.0, record: record.0 }
    }
}

/// The most placements the table keeps (rule 18: a table has a cap). Past it a fault's line
/// says the table was full, and an address placed after it is named by its region alone.
pub const MOST_ROWS: usize = 1 << 18;

/// The regions of the run's reservation, by run-view address: region `i` is
/// `edges[i]..edges[i + 1]`, named [`REGION_NAMES`]`[i]`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Regions {
    pub edges: [u64; REGION_NAMES.len() + 1],
}

pub const REGION_NAMES: [&str; 8] = [
    "the slot words",
    "the address words",
    "the C names' words",
    "the statics' words",
    "the stubs",
    "the entry thunks",
    "the code area, in no placed record",
    "the hot region, in no placed record",
];

/// The rows' mapping (`MOST_ROWS` rows, mapped once by [`install`], never unmapped).
static ROWS: AtomicPtr<Row> = AtomicPtr::new(ptr::null_mut());
/// Rows published: each row below it is written whole and never written again.
static COUNT: AtomicUsize = AtomicUsize::new(0);
/// Placements not kept because the table was full.
static DROPPED: AtomicUsize = AtomicUsize::new(0);
/// [`Regions::edges`], stored by [`install`] before the handler is.
static EDGES: [AtomicU64; REGION_NAMES.len() + 1] = [const { AtomicU64::new(0) }; REGION_NAMES.len() + 1];
/// The signals reported, and the action each had before [`install`]: its handler word and
/// flags (stored before the handler is installed, never changed after).
const SIGNALS: [c_int; 4] = [libc::SIGSEGV, libc::SIGBUS, libc::SIGILL, libc::SIGTRAP];
static PREVIOUS: [AtomicUsize; 4] = [const { AtomicUsize::new(0) }; 4];
static PREVIOUS_FLAGS: [AtomicUsize; 4] = [const { AtomicUsize::new(0) }; 4];

/// Maps the table, keeps `regions`, and installs the report on [`SIGNALS`], each chained to
/// the action that was there.
///
/// # Safety
///
/// Called once, by the control thread, before any program code runs (so no program action is
/// replaced and no fault can come while `PREVIOUS` is written).
pub unsafe fn install(regions: Regions) -> Result<(), RunModeError> {
    let bytes = MOST_ROWS * core::mem::size_of::<Row>();
    // SAFETY: an anonymous private mapping; the kernel picks the address. Its pages are made
    // as rows are written, so the cap costs address space, not memory.
    let rows = unsafe { libc::mmap(ptr::null_mut(), bytes, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_PRIVATE | libc::MAP_ANON, -1, 0) };
    if rows == libc::MAP_FAILED {
        return Err(RunModeError::Sys(crate::sys::SysError { call: "mmap (the run's placement table for crash reports)", errno: super::errno(), at: 0, len: bytes }));
    }
    for (i, edge) in regions.edges.iter().enumerate() {
        EDGES[i].store(*edge, Ordering::Relaxed);
    }
    // Release: a handler that acquires the table's pointer sees the edges.
    ROWS.store(rows.cast(), Ordering::Release);
    for (i, &sig) in SIGNALS.iter().enumerate() {
        // SAFETY: an all-zero `sigaction` is a valid value of the C struct.
        let mut before: libc::sigaction = unsafe { core::mem::zeroed() };
        // SAFETY: reads the current action into a live local.
        if unsafe { libc::sigaction(sig, ptr::null(), &mut before) } != 0 {
            return Err(RunModeError::Sys(crate::sys::SysError { call: "sigaction (reading an action before the crash report)", errno: super::errno(), at: 0, len: 0 }));
        }
        PREVIOUS[i].store(before.sa_sigaction, Ordering::Relaxed);
        PREVIOUS_FLAGS[i].store(before.sa_flags as usize, Ordering::Relaxed);
        // SAFETY: an all-zero `sigaction` is a valid value of the C struct.
        let mut ours: libc::sigaction = unsafe { core::mem::zeroed() };
        ours.sa_sigaction = on_signal as extern "C" fn(c_int, *mut libc::siginfo_t, *mut c_void) as libc::sighandler_t;
        // SA_ONSTACK: a thread with an alternate stack (the launcher's std gives its threads
        // one) reports a stack overflow too; a thread without one runs the handler on its own.
        ours.sa_flags = libc::SA_SIGINFO | libc::SA_ONSTACK;
        // SAFETY: empties a live local mask.
        unsafe { libc::sigemptyset(&mut ours.sa_mask) };
        // SAFETY: `on_signal` is async-signal-safe and a function of this binary.
        if unsafe { libc::sigaction(sig, &ours, ptr::null_mut()) } != 0 {
            return Err(RunModeError::Sys(crate::sys::SysError { call: "sigaction (installing the crash report)", errno: super::errno(), at: 0, len: 0 }));
        }
    }
    Ok(())
}

/// Appends `row`. Only the control thread calls it (the one writer of code and words), so a
/// row is written whole before the count that publishes it.
pub fn note(row: Row) {
    let rows = ROWS.load(Ordering::Acquire);
    if rows.is_null() {
        // No table: a run mode in a test with no reservation; nothing reads one.
        return;
    }
    let n = COUNT.load(Ordering::Relaxed);
    if n >= MOST_ROWS {
        DROPPED.fetch_add(1, Ordering::Relaxed);
        return;
    }
    // SAFETY: `rows` maps `MOST_ROWS` rows and `n` is below; row `n` is above the count, so no
    // reader reads it until the store below publishes it, and no other thread writes rows.
    unsafe { rows.add(n).write(row) };
    COUNT.store(n + 1, Ordering::Release);
}

/// The handler (the module's doc).
extern "C" fn on_signal(sig: c_int, info: *mut libc::siginfo_t, ctx: *mut c_void) {
    // SAFETY: the kernel hands an SA_SIGINFO handler a live `siginfo_t` and `ucontext_t`.
    let (address, (pc, write)) = unsafe { ((*info).si_addr() as u64, regs(ctx)) };
    let rows = ROWS.load(Ordering::Acquire);
    let count = COUNT.load(Ordering::Acquire);
    let placed: &[Row] = match rows.is_null() {
        true => &[],
        // SAFETY: rows below `count` were written whole before the count was stored (Release,
        // loaded Acquire above) and are never written again; the mapping is never unmapped.
        false => unsafe { core::slice::from_raw_parts(rows, count) },
    };
    let mut edges = [0u64; REGION_NAMES.len() + 1];
    for (i, edge) in EDGES.iter().enumerate() {
        edges[i] = edge.load(Ordering::Relaxed);
    }
    let mut line = Line::new();
    report(&mut line, Struck { sig, pc, address, write }, placed, &Regions { edges }, DROPPED.load(Ordering::Relaxed));
    // SAFETY: `at` live bytes of the buffer to descriptor 2; `write` is async-signal-safe.
    unsafe { libc::write(2, line.bytes.as_ptr().cast(), line.at) };
    hand_on(sig, info, ctx);
}

/// Hands the signal to the action before ours, or puts the default back: a fault then repeats
/// under it and the process ends by the signal.
fn hand_on(sig: c_int, info: *mut libc::siginfo_t, ctx: *mut c_void) {
    let Some(i) = SIGNALS.iter().position(|&s| s == sig) else { return };
    let handler = PREVIOUS[i].load(Ordering::Relaxed);
    let flags = PREVIOUS_FLAGS[i].load(Ordering::Relaxed) as c_int;
    if handler != libc::SIG_DFL && handler != libc::SIG_IGN {
        if flags & libc::SA_SIGINFO != 0 {
            // SAFETY: the previous action was installed with SA_SIGINFO, so its word is a
            // three-argument handler; it gets this delivery's own `info` and `ctx`. Its mask is
            // not applied around the call (the signal itself is blocked while this runs).
            let previous = unsafe { core::mem::transmute::<usize, extern "C" fn(c_int, *mut libc::siginfo_t, *mut c_void)>(handler) };
            previous(sig, info, ctx);
        } else {
            // SAFETY: installed without SA_SIGINFO: a one-argument handler.
            let previous = unsafe { core::mem::transmute::<usize, extern "C" fn(c_int)>(handler) };
            previous(sig);
        }
        return;
    }
    // SAFETY: an all-zero `sigaction` is SIG_DFL with no flags and an empty mask.
    let dfl: libc::sigaction = unsafe { core::mem::zeroed() };
    // SAFETY: async-signal-safe; the fault repeats under the default when this returns.
    unsafe { libc::sigaction(sig, &dfl, ptr::null_mut()) };
}

/// The interrupted pc, and whether the fault was a write (macOS: the exception syndrome's WnR
/// bit for a data abort; `None` where it is not read).
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
unsafe fn regs(ctx: *mut c_void) -> (Option<u64>, Option<bool>) {
    // SAFETY: the caller's contract; `uc_mcontext` points at the state the kernel saved.
    let mc = unsafe { &*(*ctx.cast::<libc::ucontext_t>()).uc_mcontext };
    let esr = mc.__es.__esr;
    // ESR_EL1.EC (bits 26..32): 0x24/0x25 a data abort; then bit 6, WnR, is a write.
    let ec = esr >> 26;
    let write = matches!(ec, 0x24 | 0x25).then_some(esr & (1 << 6) != 0);
    (Some(mc.__ss.__pc), write)
}

#[cfg(all(target_arch = "aarch64", target_os = "linux"))]
unsafe fn regs(ctx: *mut c_void) -> (Option<u64>, Option<bool>) {
    // SAFETY: the caller's contract; Linux's `ucontext_t` holds the saved registers inline.
    let mc = unsafe { &(*ctx.cast::<libc::ucontext_t>()).uc_mcontext };
    (Some(mc.pc), None)
}

#[cfg(not(target_arch = "aarch64"))]
unsafe fn regs(_ctx: *mut c_void) -> (Option<u64>, Option<bool>) {
    // Run mode is AArch64 only (`run_mode::run` refuses x86-64 before any code).
    (None, None)
}

/// One delivery: the signal, the interrupted pc (`None` where it is not read), the faulting
/// address (`si_addr`), and whether the access was a write (`None` where it is not known).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Struck {
    pub sig: c_int,
    pub pc: Option<u64>,
    pub address: u64,
    pub write: Option<bool>,
}

/// The report's one line, into `line`: pure, so it is tested without a signal.
pub fn report(line: &mut Line, struck: Struck, rows: &[Row], regions: &Regions, dropped: usize) {
    let Struck { sig, pc, address, write } = struck;
    line.put(b"blaze run: the program was killed by ");
    line.put(signal_name(sig));
    line.put(b": pc ");
    match pc {
        Some(pc) => {
            line.hex(pc);
            line.put(b" is ");
            describe(line, pc, rows, regions);
        }
        None => line.put(b"unknown"),
    }
    let access: &[u8] = match write {
        Some(true) => b"; it wrote ",
        Some(false) => b"; it read ",
        None => b"; the address ",
    };
    line.put(access);
    line.hex(address);
    line.put(b", which is ");
    describe(line, address, rows, regions);
    if dropped > 0 {
        line.put(b" (the placement table was full: ");
        line.dec(dropped as u64);
        line.put(b" placements not kept)");
    }
    line.put(b"\n");
}

/// What is at `at`: a placed row's `+offset in <kind> of <symbol> (record <id>)`, else the
/// region of the reservation, else outside it.
pub fn describe(line: &mut Line, at: u64, rows: &[Row], regions: &Regions) {
    // The latest row holding `at` (rows never overlap; the latest is the one placed last).
    if let Some(row) = rows.iter().rev().find(|row| row.at <= at && at - row.at < row.len) {
        line.put(b"+");
        line.hex(at - row.at);
        let what: &[u8] = match row.kind {
            Kind::Code => b" in the code of function ",
            Kind::Relro => b" in the relro words of function ",
            Kind::ReadOnlyDatum => b" in the read-only datum of static ",
            Kind::WritableDatum => b" in the writable datum of static ",
        };
        line.put(what);
        line.short(&row.symbol);
        line.put(b" (record ");
        line.short(&row.record);
        line.put(b")");
        return;
    }
    for (i, name) in REGION_NAMES.iter().enumerate() {
        if regions.edges[i] <= at && at < regions.edges[i + 1] {
            line.put(b"in ");
            line.put(name.as_bytes());
            line.put(b" (+");
            line.hex(at - regions.edges[i]);
            line.put(b")");
            return;
        }
    }
    line.put(b"outside the run's reservation");
}

fn signal_name(sig: c_int) -> &'static [u8] {
    match sig {
        libc::SIGSEGV => b"SIGSEGV".as_slice(),
        libc::SIGBUS => b"SIGBUS".as_slice(),
        libc::SIGILL => b"SIGILL".as_slice(),
        libc::SIGTRAP => b"SIGTRAP".as_slice(),
        _ => b"a signal".as_slice(),
    }
}

/// The line's room: one report, built by byte copies (async-signal-safe).
pub const LINE_MAX: usize = 640;

pub struct Line {
    pub bytes: [u8; LINE_MAX],
    pub at: usize,
}

impl Line {
    pub fn new() -> Line {
        Line { bytes: [0; LINE_MAX], at: 0 }
    }

    pub fn text(&self) -> &[u8] {
        &self.bytes[..self.at]
    }

    fn put(&mut self, text: &[u8]) {
        for &byte in text {
            if let Some(into) = self.bytes.get_mut(self.at) {
                *into = byte;
                self.at += 1;
            }
        }
    }

    fn hex(&mut self, value: u64) {
        self.put(b"0x");
        let mut started = false;
        for shift in (0..u64::BITS).step_by(4).rev() {
            let digit = ((value >> shift) & 0xf) as usize;
            if digit != 0 || started || shift == 0 {
                started = true;
                self.put(&[b"0123456789abcdef"[digit]]);
            }
        }
    }

    fn dec(&mut self, mut value: u64) {
        let mut digits = [0u8; 20];
        let mut n = 0;
        loop {
            digits[n] = b'0' + (value % 10) as u8;
            n += 1;
            value /= 10;
            if value == 0 {
                break;
            }
        }
        while n > 0 {
            n -= 1;
            self.put(&[digits[n]]);
        }
    }

    /// The first eight bytes of `id` in hex, then `..`.
    fn short(&mut self, id: &[u8; 32]) {
        for &byte in &id[..8] {
            self.put(&[b"0123456789abcdef"[usize::from(byte >> 4)], b"0123456789abcdef"[usize::from(byte & 0xf)]]);
        }
        self.put(b"..");
    }
}

impl Default for Line {
    fn default() -> Line {
        Line::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: u64 = 0x70_0000_0000;

    fn regions() -> Regions {
        let mut edges = [0u64; REGION_NAMES.len() + 1];
        for (i, edge) in edges.iter_mut().enumerate() {
            *edge = BASE + 0x1000 * i as u64;
        }
        Regions { edges }
    }

    fn said(sig: c_int, pc: Option<u64>, address: u64, write: Option<bool>, rows: &[Row]) -> std::string::String {
        let mut line = Line::new();
        report(&mut line, Struck { sig, pc, address, write }, rows, &regions(), 0);
        std::string::String::from_utf8(line.text().to_vec()).expect("the line is text")
    }

    /// The coordinator's stop of 11:40 on 2026-11-17, in its shape: a SIGBUS whose pc is inside
    /// one function's code and whose address is inside a static's read-only datum names both,
    /// with their offsets; an address in no row is named by its region; one past the
    /// reservation is outside it. Prints the first line.
    #[test]
    fn a_crash_names_the_function_and_the_static_placed_where_it_struck() {
        let area = BASE + 0x6000;
        let f = Row::new(Kind::Code, area + 0x100, 0x80, CodeSymbol([0xf1; 32]), RecordId([0xc1; 32]));
        let s = Row::new(Kind::ReadOnlyDatum, area + 0x40, 0x10, CodeSymbol([0x51; 32]), RecordId([0x77; 32]));
        let line = said(libc::SIGBUS, Some(area + 0x140), area + 0x48, Some(true), &[s, f]);
        assert_eq!(
            line,
            "blaze run: the program was killed by SIGBUS: pc 0x7000006140 is +0x40 in the code of function f1f1f1f1f1f1f1f1.. (record c1c1c1c1c1c1c1c1..); it wrote 0x7000006048, which is +0x8 in the read-only datum of static 5151515151515151.. (record 7777777777777777..)\n"
        );
        let stub = said(libc::SIGSEGV, Some(area + 0x200), BASE + 0x3008, None, &[s, f]);
        assert!(stub.contains("pc 0x7000006200 is in the code area, in no placed record (+0x200)"), "{stub}");
        assert!(stub.contains("the address 0x7000003008, which is in the statics' words (+0x8)"), "{stub}");
        let outside = said(libc::SIGILL, None, BASE + 0x9000, Some(false), &[]);
        assert!(outside.contains("SIGILL: pc unknown; it read 0x7000009000, which is outside the run's reservation"), "{outside}");
        std::println!("{}", line.trim_end());
    }
}
