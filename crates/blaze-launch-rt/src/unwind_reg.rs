//! Code placed at run time made findable by the system's unwinder, through ONE lookup over the
//! placed records' unwind entries (docs/v2/plan/design/backend-next.md, the critic's point 10;
//! run.md piece 11, part C; what lifts D91's stop at a raised panic).
//!
//! Every tier's record carries its unwind rows (blaze-tier `Emitted::unwind_frame`,
//! `Emitted::unwind`). [`eh_bytes`] makes them the one `.eh_frame` entry blaze-code eh.rs writes
//! (a CIE, its FDE, the ending zero, the LSDA), for the address they will live at; the placer
//! copies them there and tells this file the [`Span`]: the code's range and the entry's.
//!
//! **macOS: a lookup callback, not one registration a function.** The system's libunwind takes
//! a finder, `__unw_add_find_dynamic_unwind_sections(find)`, and asks it, for a pc no loaded image
//! holds, for the `.eh_frame` bytes that cover it ([`find`] answers from the [`Registry`] by a
//! binary search). One call covers every record placed for the life of the process; nothing is
//! walked on a throw. Rule 45 (a private API where it is better): the symbol is exported by
//! libSystem's libunwind (macOS 14 and later, from LLVM's libunwind, `libunwind.h`'s
//! `unw_find_dynamic_unwind_sections`) but is in no public SDK header: it is looked up at run
//! time with `dlsym`, and when it is missing [`install`] REFUSES by name ([`RegError::Missing`]):
//! there is no second route (`__register_frame` a function is code_unwind.rs's path today, and
//! this one replaces it, rule 3). `tests::the_dynamic_sections_finder_exists` checks the symbol.
//!
//! libunwind caches an FDE it found through a finder in its `DwarfFDECache`, keyed by the
//! sections' `dso_base`: [`find`] gives each record's first code byte as its `dso_base`, and
//! [`Writer::remove`] drops that cache's rows with `__deregister_frame(dso_base)`, which in LLVM's
//! libunwind is `__unw_remove_dynamic_fde`, "remove all with that mh" (UnwindLevel1-gcc-ext.c).
//! TO VERIFY by a test on a real machine: that a record placed where a freed one was is unwound
//! by its own entry (the cache row gone).
//!
//! **Linux:** libgcc's `__register_frame` takes a run of entries ended by a zero length, which
//! eh.rs's bytes are: [`Writer::add`] registers each record's entry (crate::unwind), and
//! [`Writer::remove`] takes it back. libgcc sorts its objects' FDEs on the first search, so a
//! throw is a binary search there too. The [`Registry`] is kept on both hosts: backtraces and the
//! debugger interface (jit_debug.rs) read it.
//!
//! **The callback runs during a throw**, on any thread, possibly one that holds any lock of the
//! program's: it allocates nothing and takes no lock. The registry is a left-right pair of sorted
//! arrays of fixed capacity: the one writer (the control thread, [`Writer`]) edits the side no
//! reader can be in, publishes it with one store of `current`, and waits until no reader is left
//! on the old side before it returns (so the caller may free what an old span named). A reader
//! counts itself on the side it read `current` as, then reads `current` again: if it moved, it
//! backs off without reading (the side may be under edit). All of `current` and the counts are
//! `SeqCst`, which is what makes "the reader saw `current == side` after counting itself" mean
//! "the writer's drain will see the count".

use core::cell::UnsafeCell;
use core::fmt;
use core::marker::PhantomData;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use blaze_code::eh::{self, EhError, Entry, Placed};
use blaze_code::unwind::{self, UnwindError};
use blaze_own::{AVec, SharedAlloc, Vec};
use blaze_tier::Emitted;

/// How many times a reader re-counts itself when the writer flipped sides under it. A flip
/// happens once an edit, so this is never reached unless a writer edits in a tight loop while a
/// throw runs; then the throw is told "not found" and [`Registry::missed`] counts it (the stop
/// line reports it): never a silent loss.
pub const READ_TRIES: u32 = 1 << 10;

/// How many spins the writer waits for the old side's readers (rule 18: a bound). A reader is
/// a binary search: thousands of spins are already a stuck reader.
pub const DRAIN_SPINS: u32 = 1 << 22;

/// One placed record: its code `[code_lo, code_hi)` and its unwind entry's `.eh_frame` part
/// (CIE, FDE and the ending zero, not the LSDA) at `[eh_at, eh_at + eh_len)`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Span {
    pub code_lo: u64,
    pub code_hi: u64,
    pub eh_at: u64,
    pub eh_len: u64,
    /// The placer's own name for the record (an index into its table): what a backtrace or the
    /// debugger interface reads its positions from.
    pub id: u64,
}

/// What [`eh_bytes`] made.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Made {
    /// Where the FDE and the LSDA are in the bytes.
    pub entry: Entry,
    /// The `.eh_frame` part's length: a [`Span::eh_len`].
    pub eh_len: u64,
}

/// Why a record could not be registered, or the registry could not be made or installed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RegError {
    /// The system's unwinder lacks the symbol (rule 45: looked up at run time, refused by name).
    Missing { symbol: &'static str },
    /// The symbol refused the call with libunwind's code (`UNW_EINVAL`, `UNW_ENOMEM`).
    Refused { symbol: &'static str, code: i32 },
    /// A registry is installed already: the system takes one finder of ours for the process.
    AlreadyInstalled,
    /// Another writer holds the registry (there is one: the control thread).
    WriterTaken,
    /// The registry's arrays could not be had.
    Alloc { cap: usize },
    /// The registry holds `cap` records (rule 18: a table has a cap; it is sized at start).
    Full { cap: usize },
    /// A span covers no code, or its entry has no bytes.
    Empty { code_lo: u64, code_hi: u64 },
    /// A span overlaps one registered.
    Overlaps { code_lo: u64, other_lo: u64 },
    /// No record starts at `code_lo`.
    NotRegistered { code_lo: u64 },
    /// A reader stayed on the old side for [`DRAIN_SPINS`] spins: the old span may still be
    /// read, so the caller must not free what it named.
    ReadersHeld { code_lo: u64 },
    /// A record's code is past what an entry's length holds.
    CodeTooLarge { len: usize },
    /// The record's rows were refused (blaze-code unwind.rs).
    Rows(UnwindError),
    /// The entry could not be made (blaze-code eh.rs).
    Eh(EhError),
}

impl fmt::Display for RegError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let rule = "(blaze-launch-rt unwind_reg.rs)";
        match *self {
            RegError::Missing { symbol } => write!(
                f,
                "the system's unwinder has no `{symbol}`, through which placed code is found when a panic unwinds (rule 45; macOS 14 or later has it) {rule}"
            ),
            RegError::Refused { symbol, code } => write!(f, "`{symbol}` refused the registry's finder with code {code} {rule}"),
            RegError::AlreadyInstalled => write!(f, "an unwind registry is installed already: the process has one {rule}"),
            RegError::WriterTaken => write!(f, "the unwind registry's writer is taken: the control thread is its one writer {rule}"),
            RegError::Alloc { cap } => write!(f, "the unwind registry's two arrays of {cap} records could not be allocated {rule}"),
            RegError::Full { cap } => write!(f, "the unwind registry holds its {cap} records: it is sized when the launcher starts (rule 18) {rule}"),
            RegError::Empty { code_lo, code_hi } => write!(f, "the placed record at {code_lo:#x}..{code_hi:#x} has no code or no unwind entry {rule}"),
            RegError::Overlaps { code_lo, other_lo } => {
                write!(f, "the placed record at {code_lo:#x} overlaps the one registered at {other_lo:#x} {rule}")
            }
            RegError::NotRegistered { code_lo } => write!(f, "no placed record is registered at {code_lo:#x} {rule}"),
            RegError::ReadersHeld { code_lo } => write!(
                f,
                "a throw stayed in the unwind registry for {DRAIN_SPINS} spins after the record at {code_lo:#x} changed: its entry must not be freed (rule 18) {rule}"
            ),
            RegError::CodeTooLarge { len } => write!(f, "a placed record's {len} bytes of code are past what its unwind entry holds {rule}"),
            RegError::Rows(error) => write!(f, "{error}"),
            RegError::Eh(error) => write!(f, "{error}"),
        }
    }
}

impl core::error::Error for RegError {}

/// Writes `record`'s unwind entry into `out` (cleared first) for the address `at` says, from the
/// record's rows: blaze-code eh.rs's bytes, through `unwind::of_rows`, so every tier's record is
/// unwound by the one entry writer. `out`'s bytes are then copied to `at.entry_at` by the placer.
pub fn eh_bytes(record: &Emitted<'_>, at: Placed, out: &mut AVec<'_, u8>) -> Result<Made, RegError> {
    let len = record.code.len();
    let code_len = u32::try_from(len).map_err(|_| RegError::CodeTooLarge { len })?;
    let rows = unwind::of_rows(record.unwind_frame, record.unwind.as_slice(), code_len).map_err(RegError::Rows)?;
    let entry = eh::entry(&rows, at, out).map_err(RegError::Eh)?;
    let eh_len = entry.lsda.unwrap_or(out.len()) as u64;
    Ok(Made { entry, eh_len })
}

/// The span whose code holds `pc` in `spans` (sorted by `code_lo`, disjoint): a binary search.
/// Allocates nothing, so the finder may call it during a throw.
pub fn find(spans: &[Span], pc: u64) -> Option<Span> {
    let past = spans.partition_point(|span| span.code_lo <= pc);
    let span = *spans.get(past.checked_sub(1)?)?;
    (pc < span.code_hi).then_some(span)
}

/// Puts `span` in `spans` at its place: refused when it is empty, overlaps a neighbour, or the
/// array is at its capacity (it never grows: the reader's side must not move).
pub fn insert<A: core::alloc::Allocator>(spans: &mut Vec<Span, A>, span: Span) -> Result<(), RegError> {
    if span.code_lo >= span.code_hi || span.eh_len == 0 {
        return Err(RegError::Empty { code_lo: span.code_lo, code_hi: span.code_hi });
    }
    let at = spans.partition_point(|other| other.code_lo < span.code_lo);
    if let Some(next) = spans.get(at)
        && next.code_lo < span.code_hi
    {
        return Err(RegError::Overlaps { code_lo: span.code_lo, other_lo: next.code_lo });
    }
    if let Some(before) = at.checked_sub(1).and_then(|i| spans.get(i))
        && before.code_hi > span.code_lo
    {
        return Err(RegError::Overlaps { code_lo: span.code_lo, other_lo: before.code_lo });
    }
    if spans.len() == spans.capacity() {
        return Err(RegError::Full { cap: spans.capacity() });
    }
    spans.insert(at, span);
    Ok(())
}

/// Takes the span starting at `code_lo` out of `spans`.
pub fn remove<A: core::alloc::Allocator>(spans: &mut Vec<Span, A>, code_lo: u64) -> Result<Span, RegError> {
    match spans.binary_search_by_key(&code_lo, |span| span.code_lo) {
        Ok(at) => Ok(spans.remove(at)),
        Err(_) => Err(RegError::NotRegistered { code_lo }),
    }
}

/// The placed records, read by a throw on any thread and written by one [`Writer`] (the module
/// says how).
pub struct Registry {
    sides: [UnsafeCell<Vec<Span, SharedAlloc<'static>>>; 2],
    /// The side readers read: 0 or 1.
    current: AtomicUsize,
    /// How many readers are counted on each side.
    readers: [AtomicUsize; 2],
    writer: AtomicBool,
    /// Lookups that gave up after [`READ_TRIES`].
    missed: AtomicU64,
    cap: usize,
}

// SAFETY: the sides are written only by the one `Writer` (claimed by `writer`), and only the side
// no reader is on (the module's protocol: a reader reads a side only after counting itself on it
// and seeing `current` still name it, and the writer edits the side `current` does not name and
// drains a side before its next edit of it); everything else is atomics. `Vec<Span,
// SharedAlloc>` itself is `Send + Sync`.
unsafe impl Sync for Registry {}

impl Registry {
    /// A registry for at most `cap` records, its two arrays in `alloc` (which must outlive the
    /// process's use of it: the system's unwinder keeps the finder for the process).
    pub fn new_in(cap: usize, alloc: SharedAlloc<'static>) -> Result<Registry, RegError> {
        let side = || Vec::try_with_capacity_in(cap, alloc).map_err(|_| RegError::Alloc { cap });
        Ok(Registry {
            sides: [UnsafeCell::new(side()?), UnsafeCell::new(side()?)],
            current: AtomicUsize::new(0),
            readers: [AtomicUsize::new(0), AtomicUsize::new(0)],
            writer: AtomicBool::new(false),
            missed: AtomicU64::new(0),
            cap,
        })
    }

    pub fn cap(&self) -> usize {
        self.cap
    }

    /// Lookups that gave up (the stop line names the count when a throw found nothing).
    pub fn missed(&self) -> u64 {
        self.missed.load(Ordering::Relaxed)
    }

    /// The span whose code holds `pc`. No allocation, no lock: safe in a throw.
    pub fn lookup(&self, pc: u64) -> Option<Span> {
        for _ in 0..READ_TRIES {
            let side = self.current.load(Ordering::SeqCst) & 1;
            self.readers[side].fetch_add(1, Ordering::SeqCst);
            if self.current.load(Ordering::SeqCst) == side {
                // SAFETY: counted on `side` and `current` still names it, so the writer's drain
                // of `side` (before any edit of it) waits for this count: nothing writes the
                // array while it is read here.
                let spans = unsafe { &*self.sides[side].get() };
                let found = find(spans.as_slice(), pc);
                self.readers[side].fetch_sub(1, Ordering::SeqCst);
                return found;
            }
            self.readers[side].fetch_sub(1, Ordering::SeqCst);
        }
        self.missed.fetch_add(1, Ordering::Relaxed);
        None
    }

    /// The one writer, or [`RegError::WriterTaken`].
    pub fn writer(&self) -> Result<Writer<'_>, RegError> {
        match self.writer.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed) {
            Ok(_) => Ok(Writer { registry: self, _one_thread: PhantomData }),
            Err(_) => Err(RegError::WriterTaken),
        }
    }
}

/// The registry's one writer: the control thread's, never sent to another (`!Send`).
pub struct Writer<'r> {
    registry: &'r Registry,
    _one_thread: PhantomData<*const ()>,
}

impl Drop for Writer<'_> {
    fn drop(&mut self) {
        self.registry.writer.store(false, Ordering::Release);
    }
}

impl Writer<'_> {
    /// Copies the live side to the other, applies `change` there, publishes it, and waits until
    /// no reader is left on the side it replaced. `code_lo` names the record in a drain error.
    fn edit<T>(&mut self, code_lo: u64, change: impl FnOnce(&mut Vec<Span, SharedAlloc<'static>>) -> Result<T, RegError>) -> Result<T, RegError> {
        let registry = self.registry;
        let live = registry.current.load(Ordering::SeqCst) & 1;
        let spare = live ^ 1;
        // SAFETY: the live side is only read here (readers read it too); this writer is the only
        // one that writes either side, and it does not write the live one.
        let from = unsafe { &*registry.sides[live].get() };
        // SAFETY: `spare` is the side `current` does not name, drained at the end of the edit
        // that left it (below); a reader that counts itself on it now sees `current == live` on
        // its second load and backs off without reading. So nothing reads it, and this writer is
        // the one writer.
        let next = unsafe { &mut *registry.sides[spare].get() };
        next.clear();
        // Within capacity (both sides have `cap`, and `from` holds at most `cap`): no allocation.
        next.extend_from_slice(from.as_slice());
        let out = change(next)?;
        registry.current.store(spare, Ordering::SeqCst);
        let mut spins = 0u32;
        while registry.readers[live].load(Ordering::SeqCst) != 0 {
            spins += 1;
            if spins >= DRAIN_SPINS {
                return Err(RegError::ReadersHeld { code_lo });
            }
            core::hint::spin_loop();
        }
        Ok(out)
    }

    /// Registers `span`: from the return, a throw through its code finds its entry.
    ///
    /// # Safety
    ///
    /// The entry's bytes ([`eh_bytes`]'s, made for `span.eh_at` and the code at `span.code_lo`)
    /// are at `span.eh_at`, and they and the code stay as they are until [`Writer::remove`] of
    /// the span has returned `Ok`.
    pub unsafe fn add(&mut self, span: Span) -> Result<(), RegError> {
        self.edit(span.code_lo, |spans| insert(spans, span))?;
        // SAFETY: the caller's contract, passed on.
        unsafe { os_add(span) };
        Ok(())
    }

    /// Takes the record at `code_lo` back. From an `Ok` return no throw reads its entry, and the
    /// caller may free the entry and the code (when no thread is in the code: the caller's).
    ///
    /// # Safety
    ///
    /// No thread is, or will be, in the record's code.
    pub unsafe fn remove(&mut self, code_lo: u64) -> Result<Span, RegError> {
        let span = self.edit(code_lo, |spans| remove(spans, code_lo))?;
        // SAFETY: the span was in the registry, so `add` registered it, once; no thread is in
        // its code (the caller's contract).
        unsafe { os_remove(span) };
        Ok(span)
    }
}

/// Linux: the record's entry told to libgcc (macOS: nothing; the finder answers).
///
/// # Safety
///
/// As [`Writer::add`].
#[cfg(target_os = "linux")]
unsafe fn os_add(span: Span) {
    // SAFETY: the caller's contract: an ended run of entries at `eh_at` that outlives its
    // registration (crate::unwind::register's own contract; on Linux its `fde` is unused).
    unsafe { crate::unwind::register(span.eh_at as usize, 0) }
}

#[cfg(target_os = "macos")]
unsafe fn os_add(_span: Span) {}

/// Linux: the entry taken back from libgcc. macOS: libunwind's cached FDE rows of the record
/// dropped.
///
/// # Safety
///
/// `span` was registered by [`os_add`], once, and no thread is in its code.
#[cfg(target_os = "linux")]
unsafe fn os_remove(span: Span) {
    // SAFETY: the caller's contract (crate::unwind::deregister's own).
    unsafe { crate::unwind::deregister(span.eh_at as usize, 0) }
}

#[cfg(target_os = "macos")]
unsafe fn os_remove(span: Span) {
    // SAFETY: `__deregister_frame(p)` only drops libunwind's cached FDE rows whose image base
    // is `p` (LLVM libunwind: `__unw_remove_dynamic_fde`, "remove all with that mh"); it reads
    // nothing at `p`. `p` is the `dso_base` the finder gave for this span.
    unsafe { macos::__deregister_frame(span.code_lo as *const u8) }
}

#[cfg(target_os = "macos")]
pub use macos::{FINDER_SYMBOL, install};

#[cfg(target_os = "macos")]
mod macos {
    use blaze_sys::posix::thread::{RTLD_DEFAULT, dlsym};
    use core::ffi::{c_int, c_void};
    use core::sync::atomic::{AtomicPtr, Ordering};

    use super::{RegError, Registry};

    /// The finder's registration (rule 45: private by convention, looked up at run time).
    pub const FINDER_SYMBOL: &str = "__unw_add_find_dynamic_unwind_sections";
    const FINDER_C: &core::ffi::CStr = c"__unw_add_find_dynamic_unwind_sections";

    unsafe extern "C" {
        pub(super) fn __deregister_frame(entry: *const u8);
    }

    /// libunwind.h's `unw_dynamic_unwind_sections` (LLVM libunwind, the form Apple's has): five
    /// words (`unw_word_t` and `size_t` are both 64 bits on arm64 macOS), as an array so that
    /// the fields only C reads are no dead fields here. Indexed by the constants below.
    pub(super) type DynamicSections = [usize; 5];
    pub(super) const DSO_BASE: usize = 0;
    pub(super) const DWARF_SECTION: usize = 1;
    pub(super) const DWARF_SECTION_LENGTH: usize = 2;
    // 3 and 4: `compact_unwind_section` and its length, always 0 (the entries are DWARF).

    /// libunwind.h's `unw_find_dynamic_unwind_sections`.
    type Find = unsafe extern "C" fn(addr: usize, info: *mut DynamicSections) -> c_int;
    /// `__unw_add_find_dynamic_unwind_sections`'s type.
    type AddFind = unsafe extern "C" fn(find: Find) -> c_int;

    /// The installed registry. A static because the system's finder takes no context pointer:
    /// set once by [`install`] from a `&'static`, never cleared (the finder stays registered for
    /// the life of the process).
    static INSTALLED: AtomicPtr<Registry> = AtomicPtr::new(core::ptr::null_mut());

    /// Installs `registry` as the system unwinder's finder for code no image holds. Once a
    /// process.
    pub fn install(registry: &'static Registry) -> Result<(), RegError> {
        let ptr = core::ptr::from_ref(registry).cast_mut();
        if INSTALLED.compare_exchange(core::ptr::null_mut(), ptr, Ordering::AcqRel, Ordering::Acquire).is_err() {
            return Err(RegError::AlreadyInstalled);
        }
        // SAFETY: `dlsym` with `RTLD_DEFAULT` and a NUL-ended name only reads the name.
        let symbol = unsafe { dlsym(RTLD_DEFAULT, FINDER_C.as_ptr()) };
        if symbol.is_null() {
            INSTALLED.store(core::ptr::null_mut(), Ordering::Release);
            return Err(RegError::Missing { symbol: FINDER_SYMBOL });
        }
        // SAFETY: the symbol is libunwind's `__unw_add_find_dynamic_unwind_sections`, whose C
        // type (libunwind.h) is `int (unw_find_dynamic_unwind_sections)`: `AddFind`.
        let add: AddFind = unsafe { core::mem::transmute::<*mut c_void, AddFind>(symbol) };
        // SAFETY: `find` has the finder's C type and stays valid for the process (a fn item).
        let code = unsafe { add(find) };
        if code != 0 {
            INSTALLED.store(core::ptr::null_mut(), Ordering::Release);
            return Err(RegError::Refused { symbol: FINDER_SYMBOL, code });
        }
        Ok(())
    }

    /// The finder libunwind calls during a throw (or a backtrace) for a pc no image holds: the
    /// record's `.eh_frame` part, its first code byte as the image base. Allocates nothing,
    /// takes no lock, cannot panic (a `find` of slices and atomics).
    unsafe extern "C" fn find(addr: usize, info: *mut DynamicSections) -> c_int {
        let registry = INSTALLED.load(Ordering::Acquire);
        if registry.is_null() || info.is_null() {
            return 0;
        }
        // SAFETY: `INSTALLED` holds null or a `&'static Registry` (`install`).
        let registry = unsafe { &*registry };
        let Some(span) = registry.lookup(addr as u64) else {
            return 0;
        };
        let mut sections: DynamicSections = [0; 5];
        sections[DSO_BASE] = span.code_lo as usize;
        sections[DWARF_SECTION] = span.eh_at as usize;
        sections[DWARF_SECTION_LENGTH] = span.eh_len as usize;
        // SAFETY: libunwind passes its own `unw_dynamic_unwind_sections` to fill (non-null:
        // checked above), writable and aligned as the C struct `DynamicSections` mirrors.
        unsafe { info.write(sections) };
        1
    }

    /// For the tests: the finder as libunwind calls it.
    #[cfg(test)]
    pub(super) fn call_find(addr: u64) -> Option<DynamicSections> {
        let mut out: DynamicSections = [0; 5];
        // SAFETY: `out` is a valid `DynamicSections` for the call.
        let found = unsafe { find(addr as usize, &mut out) };
        (found != 0).then_some(out)
    }
}

#[cfg(test)]
#[path = "unwind_reg_tests.rs"]
mod tests;
