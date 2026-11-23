//! Lock-free multi-producer single-consumer (MPSC) shared-memory telemetry ring
//! (bounded Vyukov ring, zero allocations, zero locks, O(1) non-blocking drop when full).
//!
//! Program threads push fixed 64-byte [`Event`]s from observe stubs (`observe.rs`),
//! intercepted C/OS calls (`run_mode/bind.rs`), and thread lifecycle hooks (`threads.rs`)
//! without ever touching the fd 3 CBOR control channel. The parent process or local viewer
//! attaches to the same shared-memory mapping out-of-band via [`Telemetry::attach`] and
//! drains events with [`Telemetry::pop`] / [`Telemetry::drain`].
//!
//! # Ordering & safety
//!
//! Each ring cell carries a lap-relative turn counter `seq: AtomicU64` (`turn = pos - idx`),
//! initialized to `0` (so zero-filled OS pages after `O_TRUNC` + `ftruncate` are already valid
//! lap-0 slots without touching or allocating pages past the header):
//!
//! - **Producer (`try_push`)**: claims ticket `pos` via CAS on `tail` (bounded to
//!   [`MAX_CAS_TRIES`] attempts per rule 18; full or contended increments `dropped` and
//!   returns `false`), verifies `cell.seq.load(Acquire) == pos - idx`, writes `cell.event`, and
//!   publishes `cell.seq.store((pos - idx) + 1, Release)`.
//! - **Consumer (`pop`)**: checks `cell.seq.load(Acquire) == (head - idx) + 1`, reads
//!   `cell.event`, publishes `cell.seq.store((head - idx) + cap, Release)`, and advances `head`.
//!
//! Because `cell.event` is written strictly before the `Release` store of `pos + 1` and read
//! strictly after the matching `Acquire` load, and never overwritten until the consumer's
//! `Release` store of `head + cap` synchronizes with the next producer's `Acquire` load, a
//! consumer never observes a torn event.

use core::cell::UnsafeCell;
use core::fmt;
use core::marker::PhantomData;
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicPtr, AtomicU64, Ordering, fence};

pub use blaze_launch::telemetry::{
    AllocOp, Event, EventKind, IoOp, MAGIC, SHADOW_DEPTH, SpanId, VAR, VERSION,
};

/// Default ring capacity when creating a per-run telemetry file (2,048 events = 256 KiB;
/// APFS materializes `MAP_SHARED` mappings up to EOF on first dirty page, so sizing at 256 KiB
/// keeps per-run disk & RAM 8x smaller than 2 MiB while still buffering >100k events/s at a
/// 16 ms viewer poll; Linux stays sparse past the header via [`Telemetry::init_zeroed`]).
pub const DEFAULT_CAP: u32 = 1 << 11;

/// Maximum CAS attempts per `try_push` before counting a contention drop (rule 18).
pub const MAX_CAS_TRIES: u32 = 64;

/// Minimum supported ring capacity (power of two).
pub const MIN_CAP: u32 = 16;

/// Why a telemetry ring could not be initialized, attached, or mapped.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TelemetryError {
    /// `cap` is not a power of two or is below [`MIN_CAP`].
    BadCap { cap: u32 },
    /// Base pointer is not aligned to `align`.
    Unaligned { addr: usize, align: usize },
    /// Buffer has `have` bytes where `need` are required.
    TooSmall { have: usize, need: usize },
    /// Shared header does not begin with [`MAGIC`].
    BadMagic { got: [u8; 8] },
    /// Shared header version does not match [`VERSION`].
    BadVersion { got: u32 },
    /// Header `mask` does not equal `cap - 1`.
    BadMask { cap: u32, mask: u64 },
    /// OS call failed while opening, sizing, or mapping the shared file.
    Os { call: &'static str, errno: i32 },
}

impl fmt::Display for TelemetryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            TelemetryError::BadCap { cap } => {
                write!(f, "telemetry ring capacity {cap} must be a power of two >= {MIN_CAP}")
            }
            TelemetryError::Unaligned { addr, align } => {
                write!(f, "telemetry ring base {addr:#x} is not {align}-byte aligned")
            }
            TelemetryError::TooSmall { have, need } => {
                write!(f, "telemetry ring region is {have} bytes, needs at least {need}")
            }
            TelemetryError::BadMagic { got } => {
                write!(f, "telemetry ring header magic {got:02x?} is not {MAGIC:02x?}")
            }
            TelemetryError::BadVersion { got } => {
                write!(f, "telemetry ring version {got} != expected {VERSION}")
            }
            TelemetryError::BadMask { cap, mask } => {
                write!(f, "telemetry ring mask {mask:#x} does not match capacity {cap}")
            }
            TelemetryError::Os { call, errno } => {
                write!(f, "telemetry ring `{call}` failed with errno {errno}")
            }
        }
    }
}

impl core::error::Error for TelemetryError {}

/// Planted bugs for the ring concurrency test (rules 43 & 53).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum RingPlant {
    None,
    /// Skips the turn-counter check on push, overwriting unread slots under wraparound.
    SkipTurnCheck,
}

/// Shared-memory ring header (cache-line aligned).
#[repr(C, align(64))]
pub struct Header {
    magic: [u8; 8],
    version: u32,
    cap: u32,
    mask: u64,
    tail: AtomicU64,
    head: AtomicU64,
    dropped: AtomicU64,
    shadow_overflows: AtomicU64,
    unwound_spans: AtomicU64,
}

const _: () = assert!(core::mem::align_of::<Header>() == 64);

/// One ring slot: turn counter + 64-byte event, padded to 128 bytes to avoid false sharing
/// between adjacent producers on Apple Silicon (128-byte cache lines).
#[repr(C, align(128))]
struct Cell {
    seq: AtomicU64,
    event: UnsafeCell<Event>,
}

const _: () = assert!(core::mem::size_of::<Cell>() == 128);
const _: () = assert!(core::mem::align_of::<Cell>() == 128);

/// Snapshot of ring counters for diagnostics and viewers.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Stats {
    pub cap: u32,
    pub pushed: u64,
    pub popped: u64,
    pub dropped: u64,
    pub shadow_overflows: u64,
    pub unwound_spans: u64,
}

/// A live view of a shared-memory telemetry ring valid for `'r`.
pub struct Telemetry<'r> {
    header: NonNull<Header>,
    cells: NonNull<Cell>,
    cap: u32,
    mask: u64,
    _region: PhantomData<&'r ()>,
}

// SAFETY: `Telemetry` only accesses atomic header/turn words and synchronized `Cell::event`
// slots according to the Vyukov MPSC turn protocol.
unsafe impl Send for Telemetry<'_> {}
unsafe impl Sync for Telemetry<'_> {}

impl<'r> Telemetry<'r> {
    /// Byte length required for a ring of `cap` events (`cap` power of two >= [`MIN_CAP`]).
    pub const fn bytes_for(cap: u32) -> Result<usize, TelemetryError> {
        if cap < MIN_CAP || !cap.is_power_of_two() {
            return Err(TelemetryError::BadCap { cap });
        }
        let header_off = Self::cells_offset();
        let Some(cells_bytes) = (cap as usize).checked_mul(core::mem::size_of::<Cell>()) else {
            return Err(TelemetryError::BadCap { cap });
        };
        match header_off.checked_add(cells_bytes) {
            Some(total) => Ok(total),
            None => Err(TelemetryError::BadCap { cap }),
        }
    }

    const fn cells_offset() -> usize {
        let h = core::mem::size_of::<Header>();
        let a = core::mem::align_of::<Cell>();
        (h + a - 1) & !(a - 1)
    }

    fn check_region(base: NonNull<u8>, bytes: usize, cap: u32) -> Result<usize, TelemetryError> {
        let need = Self::bytes_for(cap)?;
        let addr = base.as_ptr().addr();
        let align = core::mem::align_of::<Cell>();
        if addr % align != 0 {
            return Err(TelemetryError::Unaligned { addr, align });
        }
        if bytes < need {
            return Err(TelemetryError::TooSmall { have: bytes, need });
        }
        Ok(need)
    }

    /// Initializes a fresh ring in `base..base + bytes` before any producer or consumer
    /// attaches.
    ///
    /// # Safety
    ///
    /// `base..base + bytes` must be a live, writable, exclusively-owned memory region for `'r`.
    pub unsafe fn init(base: NonNull<u8>, bytes: usize, cap: u32) -> Result<Telemetry<'r>, TelemetryError> {
        let _ = Self::check_region(base, bytes, cap)?;
        let cells = unsafe { NonNull::new_unchecked(base.as_ptr().add(Self::cells_offset()).cast::<Cell>()) };
        for i in 0..cap {
            // SAFETY: `i < cap`, within the initialized region (`bytes >= need` checked above).
            let cell = unsafe { &*cells.as_ptr().add(usize::try_from(i).unwrap_or(usize::MAX)) };
            cell.seq.store(0, Ordering::Relaxed);
            unsafe { ptr::write(cell.event.get(), Event::EMPTY) };
        }
        // SAFETY: every cell is now zero-initialized for lap 0.
        unsafe { Self::init_zeroed(base, bytes, cap) }
    }

    /// Initializes a ring over a memory region whose cell bytes are ALREADY zeroed (such as a
    /// freshly truncated sparse file mapping from `O_TRUNC` + `ftruncate`). Writes only the
    /// header page so untouched slots stay sparse on disk and uncommitted in RAM.
    ///
    /// # Safety
    ///
    /// `base..base + bytes` must be live, writable, exclusively-owned for `'r`, and every byte
    /// from `Self::cells_offset()` through `Self::bytes_for(cap)` must be zero.
    pub unsafe fn init_zeroed(base: NonNull<u8>, bytes: usize, cap: u32) -> Result<Telemetry<'r>, TelemetryError> {
        let _ = Self::check_region(base, bytes, cap)?;
        let header = base.cast::<Header>();
        // SAFETY: `base` is aligned to `Cell` (128 >= 64) and `bytes >= need` covers both the
        // header and all `cap` cells. Exclusive access is guaranteed by the caller.
        let cells = unsafe { NonNull::new_unchecked(base.as_ptr().add(Self::cells_offset()).cast::<Cell>()) };
        let mask = u64::from(cap - 1);
        // SAFETY: exclusive write to header before publishing `MAGIC` with a release fence.
        unsafe {
            ptr::write(
                header.as_ptr(),
                Header {
                    magic: [0; 8],
                    version: VERSION,
                    cap,
                    mask,
                    tail: AtomicU64::new(0),
                    head: AtomicU64::new(0),
                    dropped: AtomicU64::new(0),
                    shadow_overflows: AtomicU64::new(0),
                    unwound_spans: AtomicU64::new(0),
                },
            );
            fence(Ordering::Release);
            (*header.as_ptr()).magic = MAGIC;
            fence(Ordering::Release);
        }
        Ok(Telemetry { header, cells, cap, mask, _region: PhantomData })
    }

    /// Attaches to an already-initialized shared-memory ring (e.g. from the parent process or
    /// local viewer).
    ///
    /// # Safety
    ///
    /// `base..base + bytes` must be a live, shared read/write mapping initialized by
    /// [`Telemetry::init`] that stays mapped for `'r`.
    pub unsafe fn attach(base: NonNull<u8>, bytes: usize) -> Result<Telemetry<'r>, TelemetryError> {
        let addr = base.as_ptr() as usize;
        let align = core::mem::align_of::<Cell>();
        if addr % align != 0 {
            return Err(TelemetryError::Unaligned { addr, align });
        }
        let min = Self::cells_offset();
        if bytes < min {
            return Err(TelemetryError::TooSmall { have: bytes, need: min });
        }
        let header = base.cast::<Header>();
        fence(Ordering::Acquire);
        // SAFETY: `bytes >= size_of::<Header>()` and aligned.
        let (magic, version, cap, mask) = unsafe {
            let h = &*header.as_ptr();
            (h.magic, h.version, h.cap, h.mask)
        };
        if magic != MAGIC {
            return Err(TelemetryError::BadMagic { got: magic });
        }
        if version != VERSION {
            return Err(TelemetryError::BadVersion { got: version });
        }
        let need = Self::bytes_for(cap)?;
        if mask != u64::from(cap - 1) {
            return Err(TelemetryError::BadMask { cap, mask });
        }
        if bytes < need {
            return Err(TelemetryError::TooSmall { have: bytes, need });
        }
        // SAFETY: `bytes >= need` covers header + `cap` cells.
        let cells = unsafe { NonNull::new_unchecked(base.as_ptr().add(Self::cells_offset()).cast::<Cell>()) };
        Ok(Telemetry { header, cells, cap, mask, _region: PhantomData })
    }

    fn header(&self) -> &Header {
        // SAFETY: valid for `'r`.
        unsafe { self.header.as_ref() }
    }

    fn cell(&self, index: usize) -> &Cell {
        // SAFETY: caller masks `index` with `self.mask < self.cap`.
        unsafe { &*self.cells.as_ptr().add(index) }
    }

    /// Non-blocking multi-producer push. Never allocates or locks; bounded to
    /// [`MAX_CAS_TRIES`] attempts. Returns `false` and increments `dropped` if full or heavily
    /// contended.
    pub fn try_push(&self, event: Event) -> bool {
        self.try_push_with(event, RingPlant::None)
    }

    pub(crate) fn try_push_with(&self, event: Event, plant: RingPlant) -> bool {
        let h = self.header();
        let mut pos = h.tail.load(Ordering::Relaxed);
        for _ in 0..MAX_CAS_TRIES {
            let idx = pos & self.mask;
            let expected = pos.wrapping_sub(idx);
            let cell = self.cell(usize::try_from(idx).unwrap_or(0));
            let turn = cell.seq.load(Ordering::Acquire);
            let diff = turn.wrapping_sub(expected).cast_signed();
            if diff == 0 || matches!(plant, RingPlant::SkipTurnCheck) {
                match h.tail.compare_exchange_weak(
                    pos,
                    pos.wrapping_add(1),
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => {
                        // SAFETY: we exclusively claimed turn `pos` for `cell` (its lap turn was
                        // `pos - idx`, meaning the consumer finished reading any prior entry).
                        unsafe { ptr::write(cell.event.get(), event) };
                        cell.seq.store(expected.wrapping_add(1), Ordering::Release);
                        return true;
                    }
                    Err(actual) => pos = actual,
                }
            } else if diff < 0 {
                // Ring is full.
                h.dropped.fetch_add(1, Ordering::Relaxed);
                return false;
            } else {
                // Another producer advanced `tail`; reload and retry within bound.
                pos = h.tail.load(Ordering::Relaxed);
            }
        }
        h.dropped.fetch_add(1, Ordering::Relaxed);
        false
    }

    /// Single-consumer pop. Returns `None` if empty or if the next slot's producer is mid-write.
    pub fn pop(&self) -> Option<Event> {
        let h = self.header();
        let pos = h.head.load(Ordering::Relaxed);
        let idx = pos & self.mask;
        let expected = pos.wrapping_sub(idx);
        let cell = self.cell(usize::try_from(idx).unwrap_or(0));
        let turn = cell.seq.load(Ordering::Acquire);
        let diff = turn.wrapping_sub(expected.wrapping_add(1)).cast_signed();
        if diff != 0 {
            return None;
        }
        // SAFETY: `turn == (pos - idx) + 1` with `Acquire` synchronizes with the producer's
        // `Release` after writing `cell.event`; this is the sole consumer.
        let event = unsafe { ptr::read(cell.event.get()) };
        cell.seq.store(expected.wrapping_add(u64::from(self.cap)), Ordering::Release);
        h.head.store(pos.wrapping_add(1), Ordering::Relaxed);
        Some(event)
    }

    /// Drains up to `limit` available events (rule 18: bounded drain). Returns how many were read.
    pub fn drain(&self, limit: usize, mut each: impl FnMut(Event)) -> usize {
        let mut count = 0;
        while count < limit {
            let Some(ev) = self.pop() else { break };
            each(ev);
            count += 1;
        }
        count
    }

    pub fn record_shadow_overflow(&self) {
        self.header().shadow_overflows.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_unwound_span(&self) {
        self.header().unwound_spans.fetch_add(1, Ordering::Relaxed);
    }

    pub fn stats(&self) -> Stats {
        let h = self.header();
        Stats {
            cap: self.cap,
            pushed: h.tail.load(Ordering::Relaxed),
            popped: h.head.load(Ordering::Relaxed),
            dropped: h.dropped.load(Ordering::Relaxed),
            shadow_overflows: h.shadow_overflows.load(Ordering::Relaxed),
            unwound_spans: h.unwound_spans.load(Ordering::Relaxed),
        }
    }
}

/// An owned `MAP_SHARED` file-backed telemetry mapping (used by `blaze-launcher` and by
/// parents/viewers attaching out-of-process).
pub struct MappedRing {
    base: NonNull<u8>,
    bytes: usize,
    ring: Telemetry<'static>,
}

// SAFETY: owns a `MAP_SHARED` mapping and its `Telemetry` view; unmapped on `Drop`.
unsafe impl Send for MappedRing {}
unsafe impl Sync for MappedRing {}

impl MappedRing {
    /// Creates (or truncates) `path`, sizes it for `cap` events, mmaps it `MAP_SHARED`, and
    /// initializes the ring header.
    pub fn create(path: &core::ffi::CStr, cap: u32) -> Result<MappedRing, TelemetryError> {
        let bytes = Telemetry::bytes_for(cap)?;
        // SAFETY: `path` is a live NUL-terminated string.
        let fd = unsafe {
            libc::open(
                path.as_ptr(),
                libc::O_RDWR | libc::O_CREAT | libc::O_TRUNC,
                0o600 as libc::c_uint,
            )
        };
        if fd < 0 {
            return Err(TelemetryError::Os { call: "open(create)", errno: last_errno() });
        }
        // SAFETY: valid open writable descriptor.
        if unsafe { libc::ftruncate(fd, bytes as libc::off_t) } != 0 {
            let errno = last_errno();
            unsafe { libc::close(fd) };
            return Err(TelemetryError::Os { call: "ftruncate", errno });
        }
        let base = map_fd(fd, bytes)?;
        // SAFETY: mapping stays valid after closing `fd`.
        unsafe { libc::close(fd) };
        // SAFETY: `O_TRUNC` + `ftruncate` yields a zero-filled sparse mapping exclusively ours
        // until `Drop`, so `init_zeroed` touches only the header page.
        let ring = match unsafe { Telemetry::init_zeroed(base, bytes, cap) } {
            Ok(ring) => ring,
            Err(err) => {
                unsafe { libc::munmap(base.as_ptr().cast(), bytes) };
                return Err(err);
            }
        };
        Ok(MappedRing { base, bytes, ring })
    }

    /// Opens an existing telemetry ring file at `path` and attaches to it.
    pub fn attach_file(path: &core::ffi::CStr) -> Result<MappedRing, TelemetryError> {
        // SAFETY: `path` is a live NUL-terminated string.
        let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDWR) };
        if fd < 0 {
            return Err(TelemetryError::Os { call: "open(attach)", errno: last_errno() });
        }
        let mut st: libc::stat = unsafe { core::mem::zeroed() };
        // SAFETY: `fd` is open and `st` is valid writable memory.
        if unsafe { libc::fstat(fd, &mut st) } != 0 {
            let errno = last_errno();
            unsafe { libc::close(fd) };
            return Err(TelemetryError::Os { call: "fstat", errno });
        }
        let Ok(bytes) = usize::try_from(st.st_size) else {
            unsafe { libc::close(fd) };
            return Err(TelemetryError::TooSmall { have: 0, need: Telemetry::cells_offset() });
        };
        let base = map_fd(fd, bytes)?;
        unsafe { libc::close(fd) };
        // SAFETY: `base..base + bytes` stays mapped until `Drop`.
        let ring = match unsafe { Telemetry::attach(base, bytes) } {
            Ok(ring) => ring,
            Err(err) => {
                unsafe { libc::munmap(base.as_ptr().cast(), bytes) };
                return Err(err);
            }
        };
        Ok(MappedRing { base, bytes, ring })
    }

    pub fn ring(&self) -> &Telemetry<'static> {
        &self.ring
    }

    /// Leaks the mapping for the lifetime of the launcher process and installs it as the
    /// active global telemetry sink.
    pub fn install_forever(self) -> &'static Telemetry<'static> {
        let leaked = std::boxed::Box::leak(std::boxed::Box::new(self));
        let _ = install_global(leaked.base, leaked.bytes);
        &leaked.ring
    }
}

impl Drop for MappedRing {
    fn drop(&mut self) {
        // SAFETY: `self.base..self.base + self.bytes` was returned by `mmap`.
        unsafe {
            libc::munmap(self.base.as_ptr().cast(), self.bytes);
        }
    }
}

fn map_fd(fd: libc::c_int, bytes: usize) -> Result<NonNull<u8>, TelemetryError> {
    if bytes == 0 {
        return Err(TelemetryError::TooSmall { have: 0, need: Telemetry::cells_offset() });
    }
    // SAFETY: `fd` is an open file descriptor of at least `bytes` bytes.
    let ptr = unsafe {
        libc::mmap(
            ptr::null_mut(),
            bytes,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            0,
        )
    };
    if ptr == libc::MAP_FAILED || ptr.is_null() {
        return Err(TelemetryError::Os { call: "mmap", errno: last_errno() });
    }
    Ok(unsafe { NonNull::new_unchecked(ptr.cast::<u8>()) })
}

fn last_errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

// --- Process-global active telemetry sink (single Acquire load when disabled) ---

static ACTIVE_HEADER: AtomicPtr<Header> = AtomicPtr::new(ptr::null_mut());
static ACTIVE_CELLS: AtomicPtr<Cell> = AtomicPtr::new(ptr::null_mut());
static ACTIVE_MASK: AtomicU64 = AtomicU64::new(0);
static ACTIVE_CAP: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Serializes unit tests that install the process-global telemetry ring.
#[cfg(test)]
pub(crate) static TEST_GLOBAL_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Installs an initialized shared-memory ring as this process's global telemetry sink.
/// Validates the mapping once up front so `emit` is a single Acquire load + direct ring push.
pub fn install_global(base: NonNull<u8>, bytes: usize) -> Result<(), TelemetryError> {
    // SAFETY: caller passes a live initialized ring valid until `clear_global` or process exit.
    let ring = unsafe { Telemetry::attach(base, bytes) }?;
    ACTIVE_CELLS.store(ring.cells.as_ptr(), Ordering::Relaxed);
    ACTIVE_MASK.store(ring.mask, Ordering::Relaxed);
    ACTIVE_CAP.store(ring.cap, Ordering::Relaxed);
    ACTIVE_HEADER.store(ring.header.as_ptr(), Ordering::Release);
    Ok(())
}

/// Clears the global telemetry sink (used by tests between runs).
pub fn clear_global() {
    ACTIVE_HEADER.store(ptr::null_mut(), Ordering::Release);
    ACTIVE_CELLS.store(ptr::null_mut(), Ordering::Relaxed);
    ACTIVE_MASK.store(0, Ordering::Relaxed);
    ACTIVE_CAP.store(0, Ordering::Relaxed);
}

#[inline]
fn active_ring() -> Option<Telemetry<'static>> {
    let header = NonNull::new(ACTIVE_HEADER.load(Ordering::Acquire))?;
    let cells = NonNull::new(ACTIVE_CELLS.load(Ordering::Relaxed))?;
    let mask = ACTIVE_MASK.load(Ordering::Relaxed);
    let cap = ACTIVE_CAP.load(Ordering::Relaxed);
    Some(Telemetry { header, cells, cap, mask, _region: PhantomData })
}

/// Returns whether a global telemetry ring is currently installed (one Acquire load).
#[inline]
pub fn is_active() -> bool {
    !ACTIVE_HEADER.load(Ordering::Acquire).is_null()
}

/// Reads the current hardware CPU core ID and NUMA/cluster flags in userspace (zero allocation).
#[inline]
pub fn current_cpu_and_node() -> (u16, u8) {
    #[cfg(target_os = "macos")]
    {
        unsafe extern "C" {
            fn pthread_cpu_number_np(cpu_number_out: *mut libc::size_t) -> libc::c_int;
        }
        let mut cpu: libc::size_t = 0;
        // SAFETY: `cpu` is valid writable stack memory; `pthread_cpu_number_np` reads the userspace commpage.
        if unsafe { pthread_cpu_number_np(&mut cpu) } == 0 {
            let core = u16::try_from(cpu).unwrap_or(blaze_launch::telemetry::CPU_UNKNOWN);
            (core, 0)
        } else {
            (blaze_launch::telemetry::CPU_UNKNOWN, 0)
        }
    }
    #[cfg(target_os = "linux")]
    {
        // SAFETY: `sched_getcpu` is a vDSO userspace read on Linux.
        let cpu = unsafe { libc::sched_getcpu() };
        if cpu >= 0 {
            (u16::try_from(cpu).unwrap_or(blaze_launch::telemetry::CPU_UNKNOWN), 0)
        } else {
            (blaze_launch::telemetry::CPU_UNKNOWN, 0)
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        (blaze_launch::telemetry::CPU_UNKNOWN, 0)
    }
}

/// Emits `event` into the active global telemetry ring, stamping its hardware CPU core + NUMA
/// node automatically when unset.
#[inline]
pub fn emit(mut event: Event) -> bool {
    let Some(ring) = active_ring() else {
        return false;
    };
    if event.cpu == blaze_launch::telemetry::CPU_UNKNOWN {
        let (cpu, node) = current_cpu_and_node();
        event.cpu = cpu;
        event.flags = node;
    }
    ring.try_push(event)
}

/// Increments the global ring's `shadow_overflows` counter, if installed.
pub fn record_shadow_overflow() {
    if let Some(ring) = active_ring() {
        ring.record_shadow_overflow();
    }
}

/// Increments the global ring's `unwound_spans` counter, if installed.
pub fn record_unwound_span() {
    if let Some(ring) = active_ring() {
        ring.record_unwound_span();
    }
}

#[cfg(target_arch = "aarch64")]
static CNTFRQ_HZ: AtomicU64 = AtomicU64::new(0);

/// Monotonic timestamp in nanoseconds (zero allocation).
/// On AArch64 reads the 1-cycle userspace hardware counter `cntvct_el0` scaled by cached
/// `cntfrq_el0`, avoiding `clock_gettime` syscall/vDSO overhead on every telemetry event.
#[inline]
pub fn now_ns() -> u64 {
    #[cfg(target_arch = "aarch64")]
    {
        let ticks: u64;
        // SAFETY: `cntvct_el0` is an unprivileged read-only architectural counter on AArch64.
        unsafe { core::arch::asm!("mrs {}, cntvct_el0", out(reg) ticks, options(nomem, nostack, preserves_flags)) };
        let mut freq = CNTFRQ_HZ.load(Ordering::Relaxed);
        if freq == 0 {
            let f: u64;
            // SAFETY: `cntfrq_el0` is an unprivileged read-only architectural register on AArch64.
            unsafe { core::arch::asm!("mrs {}, cntfrq_el0", out(reg) f, options(nomem, nostack, preserves_flags)) };
            freq = if f == 0 { 1 } else { f };
            CNTFRQ_HZ.store(freq, Ordering::Relaxed);
        }
        ((u128::from(ticks) * 1_000_000_000u128) / u128::from(freq)) as u64
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        // SAFETY: `ts` is valid writable memory.
        if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) } == 0 {
            (ts.tv_sec as u64).saturating_mul(1_000_000_000).saturating_add(ts.tv_nsec as u64)
        } else {
            0
        }
    }
}

/// Compact per-thread identifier for telemetry events (zero allocations, cached in TLS after
/// first call, safe to call inside `malloc`/`free` wrappers).
#[inline]
pub fn current_thread_id() -> u32 {
    std::thread_local! {
        static CACHED_TID: core::cell::Cell<u32> = const { core::cell::Cell::new(0) };
    }
    CACHED_TID.with(|cell| {
        let cached = cell.get();
        if cached != 0 {
            return cached;
        }
        #[cfg(target_os = "macos")]
        let raw = blaze_sys::posix::thread::pthread_self() as usize as u64;
        #[cfg(not(target_os = "macos"))]
        // SAFETY: `pthread_self` returns the calling thread's opaque handle.
        let raw = unsafe { libc::pthread_self() } as u64;
        let mixed = ((raw >> 32) ^ raw).wrapping_mul(0x9e37_79b9_7f4a_7c15);
        let id = ((mixed >> 32) as u32) | 1;
        cell.set(id);
        id
    })
}

/// Default Go/YourKit-style allocation byte-sampling stride (64 KiB). Set to `0` for exact
/// 100% live-heap tracking (every alloc/free recorded).
pub const DEFAULT_ALLOC_SAMPLE_BYTES: u64 = 64 * 1024;

static ALLOC_SAMPLE_BYTES: AtomicU64 = AtomicU64::new(DEFAULT_ALLOC_SAMPLE_BYTES);

/// Configures allocation sampling stride in bytes (`0` = exact tracking of every allocation).
pub fn set_alloc_sample_bytes(stride: u64) {
    ALLOC_SAMPLE_BYTES.store(stride, Ordering::Relaxed);
}

/// Current allocation sampling stride in bytes.
#[inline]
pub fn alloc_sample_bytes() -> u64 {
    ALLOC_SAMPLE_BYTES.load(Ordering::Relaxed)
}

std::thread_local! {
    /// Shared per-thread re-entrancy guard across both Rust `__rust_alloc*` slot hooks and
    /// `ObservedBind` C `malloc`/`free` wrappers so Rust -> `System` -> `malloc` is counted
    /// once (never double-counted) and internal hook code never recurses.
    static IN_ALLOC_HOOK: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
    static ALLOC_SAMPLE_BUDGET: core::cell::Cell<u64> = const { core::cell::Cell::new(0) };
}

/// RAII re-entrancy guard for allocation hooks (returns `None` when already inside an alloc hook).
pub struct AllocHookGuard {
    _private: (),
}

impl Drop for AllocHookGuard {
    #[inline]
    fn drop(&mut self) {
        IN_ALLOC_HOOK.with(|flag| flag.set(false));
    }
}

/// Enters an allocation hook on this thread. Returns `None` if already inside a hook (e.g.
/// `__rust_alloc` calling `std::alloc::System` → libc `malloc`, preventing double-counting).
#[inline]
pub fn try_enter_alloc_hook() -> Option<AllocHookGuard> {
    IN_ALLOC_HOOK.with(|flag| {
        if flag.get() {
            None
        } else {
            flag.set(true);
            Some(AllocHookGuard { _private: () })
        }
    })
}

/// Byte-budget sampling decision shared by `__rust_alloc*` slot hooks and `ObservedBind`.
/// Always returns `true` when `alloc_sample_bytes() == 0` (exact mode).
#[inline]
pub fn should_sample_alloc(bytes: usize) -> bool {
    let stride = ALLOC_SAMPLE_BYTES.load(Ordering::Relaxed);
    if stride == 0 {
        return true;
    }
    ALLOC_SAMPLE_BUDGET.with(|budget| {
        let acc = budget.get().saturating_add(bytes as u64);
        if acc >= stride {
            budget.set(acc % stride);
            true
        } else {
            budget.set(acc);
            false
        }
    })
}

/// Captures up to 3 return-address frames by walking the AArch64 `x29` frame-pointer chain
/// strictly within this thread's stack bounds (zero allocation, bounded to 3 frames).
/// Skips `skip` immediate frames (e.g. wrapper + trampoline).
#[inline(never)]
pub fn capture_caller_frames(skip: usize) -> [u64; 3] {
    let mut out = [0u64; 3];
    #[cfg(target_arch = "aarch64")]
    {
        let Ok((low, high)) = crate::sys::stack() else {
            return out;
        };
        let mut fp: usize;
        // SAFETY: reading the current frame pointer register `x29` is read-only.
        unsafe { core::arch::asm!("mov {}, x29", out(reg) fp, options(nomem, nostack, preserves_flags)) };
        let mut idx = 0usize;
        let mut skipped = 0usize;
        for _ in 0..(skip + 4) {
            if idx >= 3 || fp < low || fp + 16 > high || (fp & 7) != 0 {
                break;
            }
            // SAFETY: `fp` is 8-aligned and strictly inside `[low, high - 16]`.
            let next_fp = unsafe { *(fp as *const usize) };
            let lr = unsafe { *((fp + 8) as *const u64) };
            if lr != 0 {
                if skipped < skip {
                    skipped += 1;
                } else {
                    out[idx] = lr;
                    idx += 1;
                }
            }
            if next_fp <= fp {
                break;
            }
            fp = next_fp;
        }
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        let _ = skip;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use blaze_launch::Slot;

    /// Multi-producer single-consumer concurrency proof: 4 producers push 4,000 events each
    /// into a small 64-slot ring while a consumer drains concurrently. Verifies:
    /// - zero torn or invalid events (every popped event's checksum `b == a ^ (thread << 32)`)
    /// - exact accounting: `received + dropped == total_sent`
    /// - planted bug `SkipTurnCheck` is caught!
    fn run_mpsc(plant: RingPlant) -> Result<(u64, u64), String> {
        const CAP: u32 = 64;
        const PRODUCERS: u32 = 4;
        const PER_PRODUCER: u64 = 4_000;
        let bytes = Telemetry::bytes_for(CAP).map_err(|e| e.to_string())?;
        let layout = core::alloc::Layout::from_size_align(bytes, 128).map_err(|e| e.to_string())?;
        // SAFETY: non-zero layout, freed after scope ends.
        let raw = unsafe { std::alloc::alloc_zeroed(layout) };
        let base = NonNull::new(raw).ok_or_else(|| String::from("alloc failed"))?;
        let ring = unsafe { Telemetry::init(base, bytes, CAP) }.map_err(|e| e.to_string())?;
        let ring_ref = &ring;
        let done = core::sync::atomic::AtomicBool::new(false);
        let received = core::sync::atomic::AtomicU64::new(0);
        let bad = core::sync::atomic::AtomicU64::new(0);

        let start_consumer = core::sync::atomic::AtomicBool::new(false);
        let start_consumer_ref = &start_consumer;
        std::thread::scope(|s| {
            let consumer = s.spawn(|| {
                while !start_consumer_ref.load(Ordering::Acquire) {
                    core::hint::spin_loop();
                }
                let mut per_thread_last = [0u64; PRODUCERS as usize];
                loop {
                    let n = ring_ref.drain(256, |ev| {
                        let ok_kind = ev.event_kind() == Some(EventKind::HookHit);
                        let t = ev.thread;
                        let seq = ev.a;
                        let tag = ev.b;
                        let expect_tag = seq ^ (u64::from(t) << 32);
                        let ok_order = t < PRODUCERS && seq > per_thread_last[t as usize];
                        if !ok_kind || tag != expect_tag || !ok_order {
                            bad.fetch_add(1, Ordering::Relaxed);
                        } else {
                            per_thread_last[t as usize] = seq;
                        }
                        received.fetch_add(1, Ordering::Relaxed);
                    });
                    if n == 0 && done.load(Ordering::Acquire) {
                        // One final drain after producers finished.
                        ring_ref.drain(256, |ev| {
                            let t = ev.thread;
                            let seq = ev.a;
                            let expect_tag = seq ^ (u64::from(t) << 32);
                            if ev.event_kind() != Some(EventKind::HookHit) || ev.b != expect_tag {
                                bad.fetch_add(1, Ordering::Relaxed);
                            }
                            received.fetch_add(1, Ordering::Relaxed);
                        });
                        break;
                    }
                    if n == 0 {
                        std::thread::yield_now();
                    }
                }
            });

            let mut handles = Vec::new();
            for tid in 0..PRODUCERS {
                handles.push(s.spawn(move || {
                    for seq in 1..=PER_PRODUCER {
                        let mut ev = Event::hook_hit(tid, Slot(tid), seq, seq);
                        ev.b = seq ^ (u64::from(tid) << 32);
                        let _ = ring_ref.try_push_with(ev, plant);
                        if tid == 0 && seq == u64::from(CAP) * 2 {
                            start_consumer_ref.store(true, Ordering::Release);
                        }
                    }
                }));
            }
            for h in handles {
                h.join().expect("producer thread");
            }
            start_consumer.store(true, Ordering::Release);
            done.store(true, Ordering::Release);
            consumer.join().expect("consumer thread");
        });

        let stats = ring.stats();
        let rec = received.load(Ordering::Relaxed);
        let errs = bad.load(Ordering::Relaxed);
        unsafe { std::alloc::dealloc(raw, layout) };

        if errs > 0 {
            return Err(format!("saw {errs} torn/corrupted/reordered events"));
        }
        let total_sent = u64::from(PRODUCERS) * PER_PRODUCER;
        if rec + stats.dropped != total_sent {
            return Err(format!(
                "accounting mismatch: received({rec}) + dropped({}) != sent({total_sent})",
                stats.dropped
            ));
        }
        Ok((rec, stats.dropped))
    }

    /// The invariant: the lock-free MPSC telemetry ring delivers whole events without tearing or out-of-turn reads and accounts for every dropped event exactly.
    #[test]
    fn mpsc_ring_delivers_whole_events_with_exact_drop_accounting() {
        let (received, dropped) = run_mpsc(RingPlant::None).expect("clean ring must pass");
        assert!(received > 0, "consumer drained events (received={received}, dropped={dropped})");
    }

    #[test]
    fn the_oracle_catches_skip_turn_check() {
        assert!(
            run_mpsc(RingPlant::SkipTurnCheck).is_err(),
            "skipping the turn check must break integrity or accounting under wraparound"
        );
    }

    #[test]
    fn mapped_ring_file_round_trips_across_independent_attach() {
        let path = std::env::temp_dir().join(format!("blaze-telem-test-{}.ring", std::process::id()));
        let cpath = std::ffi::CString::new(path.to_str().expect("utf8 path")).expect("cstr");
        let writer = MappedRing::create(&cpath, 64).expect("create mapped ring");
        let reader = MappedRing::attach_file(&cpath).expect("attach mapped ring");
        let ev1 = Event::span_enter(1, Slot(7), 100, 0x1234, SpanId(1), SpanId::NONE);
        let ev2 = Event::span_exit(1, Slot(7), 250, 150, SpanId(1), SpanId::NONE);
        assert!(writer.ring().try_push(ev1));
        assert!(writer.ring().try_push(ev2));
        assert_eq!(reader.ring().pop(), Some(ev1));
        assert_eq!(reader.ring().pop(), Some(ev2));
        assert_eq!(reader.ring().pop(), None);
        drop(reader);
        drop(writer);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn mapped_ring_create_bounds_disk_and_round_trips_zeroed_init() {
        let mut buf = [0u8; 64];
        let prefix = b"/tmp/blaze-telem-sparse-";
        buf[..prefix.len()].copy_from_slice(prefix);
        let mut len = prefix.len();
        let mut pid = unsafe { libc::getpid() }.cast_unsigned();
        let mut digits = [0u8; 16];
        let mut n = 0;
        loop {
            digits[n] = b'0' + u8::try_from(pid % 10).unwrap_or(0);
            n += 1;
            pid /= 10;
            if pid == 0 {
                break;
            }
        }
        while n > 0 {
            n -= 1;
            buf[len] = digits[n];
            len += 1;
        }
        let suffix = b".ring\0";
        buf[len..len + suffix.len()].copy_from_slice(suffix);
        let cpath = core::ffi::CStr::from_bytes_with_nul(&buf[..len + suffix.len()]).expect("cstr");

        let writer = MappedRing::create(cpath, DEFAULT_CAP).expect("create default telemetry ring");
        let ev = Event::span_enter(1, Slot(3), 42, 0x99, SpanId(1), SpanId::NONE);
        assert!(writer.ring().try_push(ev));
        let reader = MappedRing::attach_file(cpath).expect("attach telemetry ring");
        assert_eq!(reader.ring().pop(), Some(ev));
        assert_eq!(reader.ring().pop(), None);
        drop(reader);
        drop(writer);
        let mut st: libc::stat = unsafe { core::mem::zeroed() };
        let rc = unsafe { libc::stat(cpath.as_ptr(), &mut st) };
        unsafe { libc::unlink(cpath.as_ptr()) };
        assert_eq!(rc, 0, "stat telemetry ring");
        let allocated = u64::try_from(st.st_blocks).unwrap_or(u64::MAX).saturating_mul(512);
        // Linux keeps untouched pages sparse (<= 64 KiB); macOS APFS materializes the full
        // MAP_SHARED file on first dirty page, bounded by DEFAULT_CAP (256 KiB + header).
        let cap_bytes = if cfg!(target_os = "macos") { 320 * 1024 } else { 64 * 1024 };
        assert!(
            allocated <= cap_bytes,
            "telemetry ring allocated {allocated} bytes on disk (limit {cap_bytes})"
        );
    }
}
