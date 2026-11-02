//! The one unsafe module: the callbacks SanitizerCoverage and AddressSanitizer call in an
//! instrumented build, and the engine's reads of the counters they fill.
//!
//! Everything in a fuzz build is instrumented, this module too. So the comparison callbacks
//! (`__sanitizer_cov_trace_*cmp*`, `_div*`, `_gep`, `_pc_indir`) hold no comparison, no `match`
//! and no bounds check, even unoptimized: trace-cmp turns each of those into a call of a
//! callback, and one in a callback would call it again without end. They write through masked
//! indices with the atomic intrinsics, never the `Atomic*` methods (whose `match` on the
//! ordering is a `switch` in an unoptimized build). Their counters, and those of
//! [`Coverage::run`] and [`Coverage::drain`], count the engine rather than the target:
//! [`Coverage::new`] finds them in the PC table and masks them out. The callbacks run on every
//! comparison the target makes, so they spend no locked instruction: a bit or a slot two
//! threads write at once may lose one write, as in libFuzzer.
//!
//! The counters are SanitizerCoverage's plain bytes, which instrumented code on every thread
//! increments without atomics, as the instrumentation defines them. The engine reads and clears
//! them only through raw pointers (no Rust reference to them is ever made, so nothing is
//! assumed about them), on the fuzzing thread between runs, when the target has returned and
//! joined its threads. The one other thread is the engine's watchdog, which runs a few blocks
//! per wake-up; as in libFuzzer, an increment of one of those racing a clear is at worst a lost
//! count of a counter that is masked out anyway.

use std::{
    ffi::{c_char, c_int, c_void},
    intrinsics::{self, AtomicOrdering::Relaxed},
    marker::PhantomData,
    panic::{AssertUnwindSafe, catch_unwind},
    ptr,
    sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, AtomicUsize, Ordering},
};

/// How many instrumented modules (the executable and each instrumented shared library) are
/// kept apart; one statically linked binary is one.
const MODULES: usize = 64;

/// The value profile: libFuzzer's 64 Ki bits, each a (comparison site, distance) pair.
pub(crate) const VALUE_BITS: usize = 1 << 16;
const VALUE_WORDS: usize = VALUE_BITS / 64;

/// The table of recent comparisons: operand pairs of 2, 4 and 8 bytes, newest overwriting.
pub(crate) const TORC: usize = 64;
/// Recent `memcmp`/`bcmp` operands, up to [`WORD`] bytes of each.
pub(crate) const TORC_WORDS: usize = 32;
pub(crate) const WORD: usize = 32;
const WORD_U64S: usize = WORD / 8;
/// A slot of [`TORC_WORDS`]: its length, then each operand's words.
const WORD_SLOT: usize = 1 + 2 * WORD_U64S;

static COUNTERS: [AtomicPtr<u8>; 2 * MODULES] = [const { AtomicPtr::new(ptr::null_mut()) }; 2 * MODULES];
static COUNTER_MODULES: AtomicUsize = AtomicUsize::new(0);
static PCS: [AtomicPtr<usize>; 2 * MODULES] = [const { AtomicPtr::new(ptr::null_mut()) }; 2 * MODULES];
static PC_MODULES: AtomicUsize = AtomicUsize::new(0);
static TOO_MANY: AtomicBool = AtomicBool::new(false);
static ASAN: AtomicBool = AtomicBool::new(false);

static VALUES: [AtomicU64; VALUE_WORDS] = [const { AtomicU64::new(0) }; VALUE_WORDS];
/// `a`, `b` and the width in bytes, per slot.
static RECENT: [AtomicU64; 3 * TORC] = [const { AtomicU64::new(0) }; 3 * TORC];
static RECENT_NEXT: AtomicU64 = AtomicU64::new(0);
static RECENT_WORDS: [AtomicU64; WORD_SLOT * TORC_WORDS] =
    [const { AtomicU64::new(0) }; WORD_SLOT * TORC_WORDS];
static RECENT_WORDS_NEXT: AtomicUsize = AtomicUsize::new(0);

/// The input being run, for a crash report from any thread.
static CURRENT: AtomicPtr<u8> = AtomicPtr::new(ptr::null_mut());
static CURRENT_LEN: AtomicUsize = AtomicUsize::new(0);
/// [`IDLE`], [`DUMPING`], or the millisecond the running input started at, plus [`STARTED`].
static STATE: AtomicU64 = AtomicU64::new(IDLE);
const IDLE: u64 = 0;
const DUMPING: u64 = 1;
pub(crate) const STARTED: u64 = 2;

// SanitizerCoverage's registrations, called by each instrumented module's constructor before
// `main`. A module's counters and its PC table come in the same order, one entry each per
// instrumented block.

#[unsafe(no_mangle)]
pub(crate) extern "C" fn __sanitizer_cov_8bit_counters_init(start: *mut u8, stop: *mut u8) {
    register(&COUNTERS, &COUNTER_MODULES, start, stop);
}

#[unsafe(no_mangle)]
pub(crate) extern "C" fn __sanitizer_cov_pcs_init(start: *const usize, stop: *const usize) {
    register(&PCS, &PC_MODULES, start.cast_mut(), stop.cast_mut());
}

fn register<T>(table: &[AtomicPtr<T>; 2 * MODULES], count: &AtomicUsize, start: *mut T, stop: *mut T) {
    let n = count.load(Ordering::Acquire);
    // Every object of a module calls with the module's whole section: the first call counts.
    if (0..n).any(|module| table[2 * module].load(Ordering::Relaxed) == start) {
        return;
    }
    if n == MODULES {
        TOO_MANY.store(true, Ordering::Relaxed);
        return;
    }
    table[2 * n].store(start, Ordering::Relaxed);
    table[2 * n + 1].store(stop, Ordering::Relaxed);
    count.store(n + 1, Ordering::Release);
}

// The comparison callbacks: no comparison, no `match`, no bounds check in any of them (above).

const VALUE_MASK: u64 = VALUE_BITS as u64 - 1;
const TORC_MASK: u64 = TORC as u64 - 1;

/// A load and a store, not a locked `or`: this runs on every comparison the target makes, and
/// a bit another thread sets between the two may be lost, as in libFuzzer's map.
#[inline(always)]
fn value(at: u64) {
    let bit = at & VALUE_MASK;
    let word = VALUES.as_ptr().wrapping_add(bit.wrapping_shr(6) as usize).cast::<u64>().cast_mut();
    // SAFETY: `bit` is masked below `VALUE_BITS`, so `word` is one of `VALUES`' words, which
    // are `AtomicU64`s, loaded and stored atomically.
    let old = unsafe { intrinsics::atomic_load::<u64, { Relaxed }, false>(word) };
    // SAFETY: as above.
    unsafe { intrinsics::atomic_store::<u64, { Relaxed }, false>(word, old | 1u64.wrapping_shl(bit as u32)) };
}

/// libFuzzer's `HandleCmp`: the Hamming distance and the distance's leading zeros of the
/// operands, per site, into the value profile (equal operands: 65, apart from every distance).
#[inline(always)]
fn distance(pc: u64, a: u64, b: u64) {
    let site = pc.wrapping_mul(256);
    value(site.wrapping_add((a ^ b).count_ones() as u64));
    value(site.wrapping_add(65).wrapping_add(a.wrapping_sub(b).leading_zeros() as u64));
}

#[inline(always)]
fn recent(a: u64, b: u64, width: u64) {
    // SAFETY: `RECENT_NEXT` is an `AtomicU64`, loaded and stored atomically; two threads that
    // take the same slot at once overwrite one comparison, as in libFuzzer's table.
    let n = unsafe { intrinsics::atomic_load::<u64, { Relaxed }, false>(RECENT_NEXT.as_ptr()) };
    // SAFETY: as above.
    unsafe { intrinsics::atomic_store::<u64, { Relaxed }, false>(RECENT_NEXT.as_ptr(), n.wrapping_add(1)) };
    let slot = RECENT.as_ptr().wrapping_add((n & TORC_MASK).wrapping_mul(3) as usize).cast::<u64>().cast_mut();
    // SAFETY: the slot index is masked below `TORC`, so `slot` and the two words after it are
    // `RECENT`'s `AtomicU64`s, stored to atomically.
    unsafe { intrinsics::atomic_store::<u64, { Relaxed }, false>(slot, a) };
    // SAFETY: as above.
    unsafe { intrinsics::atomic_store::<u64, { Relaxed }, false>(slot.wrapping_add(1), b) };
    // SAFETY: as above.
    unsafe { intrinsics::atomic_store::<u64, { Relaxed }, false>(slot.wrapping_add(2), width) };
}

#[inline(always)]
fn caller() -> u64 {
    intrinsics::return_address() as usize as u64
}

macro_rules! compares {
    ($($name:ident($ty:ty) $width:literal;)*) => {$(
        #[unsafe(no_mangle)]
        #[sanitize(address = "off")]
        pub(crate) extern "C" fn $name(a: $ty, b: $ty) {
            distance(caller(), a as u64, b as u64);
            recent(a as u64, b as u64, $width);
        }
    )*};
}

compares! {
    __sanitizer_cov_trace_cmp2(u16) 2;
    __sanitizer_cov_trace_cmp4(u32) 4;
    __sanitizer_cov_trace_cmp8(u64) 8;
    __sanitizer_cov_trace_const_cmp2(u16) 2;
    __sanitizer_cov_trace_const_cmp4(u32) 4;
    __sanitizer_cov_trace_const_cmp8(u64) 8;
}

// One byte is no word for the dictionary: the value profile alone.

#[unsafe(no_mangle)]
#[sanitize(address = "off")]
pub(crate) extern "C" fn __sanitizer_cov_trace_cmp1(a: u8, b: u8) {
    distance(caller(), a as u64, b as u64);
}

#[unsafe(no_mangle)]
#[sanitize(address = "off")]
pub(crate) extern "C" fn __sanitizer_cov_trace_const_cmp1(a: u8, b: u8) {
    distance(caller(), a as u64, b as u64);
}

#[unsafe(no_mangle)]
#[sanitize(address = "off")]
pub(crate) extern "C" fn __sanitizer_cov_trace_div4(divisor: u32) {
    distance(caller(), divisor as u64, 0);
}

#[unsafe(no_mangle)]
#[sanitize(address = "off")]
pub(crate) extern "C" fn __sanitizer_cov_trace_div8(divisor: u64) {
    distance(caller(), divisor, 0);
}

#[unsafe(no_mangle)]
#[sanitize(address = "off")]
pub(crate) extern "C" fn __sanitizer_cov_trace_gep(index: usize) {
    distance(caller(), index as u64, 0);
}

/// An indirect call: the (caller, callee) pair, as libFuzzer's `HandleCallerCallee`.
#[unsafe(no_mangle)]
#[sanitize(address = "off")]
pub(crate) extern "C" fn __sanitizer_cov_trace_pc_indir(callee: usize) {
    value(caller().wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ callee as u64);
}

/// A `switch`: `cases[0]` cases of `cases[1]` bits, then the cases. Not a comparison callback:
/// the comparisons here call those, which call nothing.
#[unsafe(no_mangle)]
#[sanitize(address = "off")]
pub(crate) extern "C" fn __sanitizer_cov_trace_switch(val: u64, cases: *const u64) {
    let pc = caller();
    // SAFETY: SanitizerCoverage passes its table of the switch: a count, a width and the count
    // of cases, all readable for the program's life.
    let count = unsafe { cases.read() };
    // SAFETY: as above, the table's second word.
    let bits = unsafe { cases.add(1).read() };
    let mut nearest = (u32::MAX, 0);
    for i in 0..count as usize {
        // SAFETY: `i` is below the table's count of cases, which follow its two words.
        let case = unsafe { cases.add(2 + i).read() };
        let apart = (val ^ case).count_ones();
        value(pc.wrapping_add(i as u64).wrapping_mul(256).wrapping_add(apart as u64));
        if apart < nearest.0 {
            nearest = (apart, case);
        }
    }
    if count > 0 && bits >= 16 {
        recent(val, nearest.1, bits / 8);
    }
}

/// What AddressSanitizer's `memcmp` and `bcmp` interceptors tell: the operands of a byte
/// comparison that differed, for the dictionary, and how long a prefix they share, per site.
#[unsafe(no_mangle)]
#[sanitize(address = "off")]
pub(crate) extern "C" fn __sanitizer_weak_hook_memcmp(
    caller: *const c_void,
    s1: *const c_void,
    s2: *const c_void,
    n: usize,
    result: c_int,
) {
    if result == 0 || n < 2 {
        return;
    }
    let len = n.min(WORD);
    let (mut a, mut b) = ([0u8; WORD], [0u8; WORD]);
    // SAFETY: `memcmp(s1, s2, n)` was given `n` readable bytes at each, and `len <= n`.
    unsafe { ptr::copy_nonoverlapping(s1.cast::<u8>(), a.as_mut_ptr(), len) };
    // SAFETY: as above.
    unsafe { ptr::copy_nonoverlapping(s2.cast::<u8>(), b.as_mut_ptr(), len) };
    let shared = a[..len].iter().zip(&b[..len]).take_while(|(x, y)| x == y).count();
    value((caller as usize as u64).wrapping_mul(256).wrapping_add(shared as u64));
    // A load and a store, not a locked `fetch_add`: two threads that take the same slot at once
    // overwrite one comparison, as in `recent`'s table.
    let next = RECENT_WORDS_NEXT.load(Ordering::Relaxed);
    RECENT_WORDS_NEXT.store(next.wrapping_add(1), Ordering::Relaxed);
    let slot = next % TORC_WORDS * WORD_SLOT;
    RECENT_WORDS[slot].store(len as u64, Ordering::Relaxed);
    for (i, (x, y)) in a.chunks_exact(8).zip(b.chunks_exact(8)).enumerate() {
        RECENT_WORDS[slot + 1 + i].store(u64::from_le_bytes(x.try_into().unwrap()), Ordering::Relaxed);
        RECENT_WORDS[slot + 1 + WORD_U64S + i]
            .store(u64::from_le_bytes(y.try_into().unwrap()), Ordering::Relaxed);
    }
}

// AddressSanitizer's hooks. Its runtime reads its options before `main`, which is how a run
// knows it was built with it.

#[unsafe(no_mangle)]
pub(crate) extern "C" fn __asan_default_options() -> *const c_char {
    ASAN.store(true, Ordering::Relaxed);
    // A report ends the process with SIGABRT, which `minimize` tells from a clean exit; an
    // allocation past 2 GiB is a finding, not a null; a leak at exit is not looked for (the
    // fuzzer runs for hours in one process and ends by `exit`).
    c"abort_on_error=1:allocator_may_return_null=0:max_allocation_size_mb=2048:detect_leaks=0".as_ptr()
}

#[unsafe(no_mangle)]
pub(crate) extern "C" fn __asan_on_error() {
    crate::dump(crate::Finding::Crash);
}

/// Whether AddressSanitizer's runtime is in this process.
pub(crate) fn asan() -> bool {
    ASAN.load(Ordering::Relaxed)
}

/// The engine's callbacks that run while a target does: their counters count comparisons,
/// not code the input reached.
fn engine_functions() -> [usize; 18] {
    [
        __sanitizer_cov_trace_cmp1 as usize,
        __sanitizer_cov_trace_cmp2 as usize,
        __sanitizer_cov_trace_cmp4 as usize,
        __sanitizer_cov_trace_cmp8 as usize,
        __sanitizer_cov_trace_const_cmp1 as usize,
        __sanitizer_cov_trace_const_cmp2 as usize,
        __sanitizer_cov_trace_const_cmp4 as usize,
        __sanitizer_cov_trace_const_cmp8 as usize,
        __sanitizer_cov_trace_div4 as usize,
        __sanitizer_cov_trace_div8 as usize,
        __sanitizer_cov_trace_gep as usize,
        __sanitizer_cov_trace_pc_indir as usize,
        __sanitizer_cov_trace_switch as usize,
        __sanitizer_weak_hook_memcmp as usize,
        __sanitizer_blaze_fuzzer_run as usize,
        __sanitizer_blaze_fuzzer_drain as usize,
        Coverage::run as usize,
        Coverage::drain as usize,
    ]
}

/// One module's counters, and which of them are the engine's.
struct Module {
    start: *mut u8,
    len: usize,
    /// The global index of its first counter.
    first: usize,
    /// Per 8 counters, a byte of ones over each one to drop.
    ignored: Vec<u64>,
}

/// The counters of every instrumented module, read and cleared between runs.
pub(crate) struct Coverage<'a> {
    modules: Vec<Module>,
    total: usize,
    _counters: PhantomData<&'a mut [u8]>,
}

impl Coverage<'static> {
    /// The counters SanitizerCoverage registered, with those of this module's callbacks and of
    /// the functions at `engine` dropped, or why this build can't be fuzzed.
    pub(crate) fn new(engine: &[usize]) -> Result<Self, String> {
        if TOO_MANY.load(Ordering::Relaxed) {
            return Err(format!("more than {MODULES} instrumented modules registered counters"));  // alloc: error message
        }
        let counted = COUNTER_MODULES.load(Ordering::Acquire);
        if counted == 0 {
            return Err(format!("this binary was built without SanitizerCoverage's counters: {}", crate::FLAGS));  // alloc: error message
        }
        let tabled = PC_MODULES.load(Ordering::Acquire);
        if tabled != counted {
            return Err(format!(  // alloc: error message
                "{counted} modules registered counters and {tabled} a PC table: build with `pc-table` ({})",
                crate::FLAGS
            ));
        }
        let ours = engine_functions();
        let mut modules = Vec::with_capacity(counted);
        let mut total = 0;
        for module in 0..counted {
            let start = COUNTERS[2 * module].load(Ordering::Relaxed);
            let stop = COUNTERS[2 * module + 1].load(Ordering::Relaxed);
            let pcs = PCS[2 * module].load(Ordering::Relaxed).cast_const();
            let pcs_stop = PCS[2 * module + 1].load(Ordering::Relaxed).cast_const();
            let len = (stop as usize).saturating_sub(start as usize);
            let entries = (pcs_stop as usize).saturating_sub(pcs as usize) / (2 * size_of::<usize>());
            if entries != len {
                return Err(format!(  // alloc: error message
                    "module {module} has {len} counters and {entries} PC table entries; they must be one per block"
                ));
            }
            // SAFETY: SanitizerCoverage registered `[pcs, pcs_stop)`, a constant array of
            // (pc, flags) pairs, one per counter (checked just above), for the program's life.
            let table = unsafe { std::slice::from_raw_parts(pcs, 2 * len) };
            let mut ignored = vec![0u64; len.div_ceil(8)];  // alloc: the engine's mask, once per module at start
            for &function in ours.iter().chain(engine) {
                // Its entry block (flag 1) at its address, then its other blocks up to the next
                // entry. An engine function that is not instrumented has none.
                let Some(entry) = (0..len).find(|&k| table[2 * k] == function && table[2 * k + 1] & 1 == 1)
                else {
                    continue;
                };
                let end = (entry + 1..len).find(|&k| table[2 * k + 1] & 1 == 1).unwrap_or(len);
                for k in entry..end {
                    ignored[k / 8] |= 0xff << (8 * (k % 8));
                }
            }
            modules.push(Module { start, len, first: total, ignored });
            total += len;
        }
        if total > (u32::MAX as usize >> 3) - VALUE_BITS {
            return Err(format!("{total} counters are more than features can number"));  // alloc: error message
        }
        Ok(Coverage { modules, total, _counters: PhantomData })
    }
}

impl<'a> Coverage<'a> {
    /// `counters` as if they were one module's, with `ignored` dropped: for the tests.
    #[cfg(test)]
    pub(crate) fn of(counters: &'a mut [u8], ignored: &[usize]) -> Self {
        let len = counters.len();
        let mut mask = vec![0u64; len.div_ceil(8)];
        for &k in ignored {
            mask[k / 8] |= 0xff << (8 * (k % 8));
        }
        let module = Module { start: counters.as_mut_ptr(), len, first: 0, ignored: mask };
        Coverage { modules: vec![module], total: len, _counters: PhantomData }
    }

    /// How many counters there are.
    pub(crate) fn total(&self) -> usize {
        self.total
    }

    /// Pushes the feature of each counter the run reached (its global index times 8 plus its
    /// AFL bucket), and with `values` of each value-profile bit set (after every counter's).
    /// It leaves them set: [`Self::run`] clears them, since the engine's own code between two
    /// runs reaches counters too. `features` must have room for [`Self::total`] plus
    /// [`VALUE_BITS`] more, so that nothing here allocates while the counters fill.
    pub(crate) fn drain(&self, features: &mut Vec<u32>, values: bool) {
        __sanitizer_blaze_fuzzer_drain(self, features, values);
    }

    /// Clears every counter and the value profile, then runs `check(data)`; false when it
    /// panicked. While it runs, [`current`] gives `data` to a crash report.
    pub(crate) fn run(&self, check: fn(&[u8]), data: &[u8], now: u64) -> bool {
        __sanitizer_blaze_fuzzer_run(self, check, data, now)
    }
}

/// The mutator's search for an operand it replaces ([`crate::mutate::find_from`]). Named
/// `__sanitizer_*` and kept out of line so SanitizerCoverage leaves it alone, it adds no
/// trace-cmp calls. It compares a byte at a time, in loops with no closure to be compiled out
/// of line, because a slice `==` is a `memcmp`, whose operands an ASan build's hook keeps as
/// dictionary words.
#[unsafe(no_mangle)]
#[inline(never)]
pub(crate) fn __sanitizer_blaze_fuzzer_find(data: &[u8], from: &[u8], start: usize) -> Option<usize> {
    let ends = data.len() - from.len() + 1;
    for (low, end) in [(start, ends), (0, start)] {
        'at: for at in low..end {
            for (i, byte) in from.iter().enumerate() {
                if data[at + i] != *byte {
                    continue 'at;
                }
            }
            return Some(at);
        }
    }
    None
}

/// How many counter words [`__sanitizer_blaze_fuzzer_drain`] tests for zero at once: a cache
/// line, which the compiler reads in vector loads.
const CHUNK_WORDS: usize = 8;

/// The features of counter word `w` of `module`, which reads `word`.
#[inline(always)]
fn push_word(features: &mut Vec<u32>, module: &Module, w: usize, word: u64) {
    let mut rest = word & !module.ignored[w];
    while rest != 0 {
        let byte = rest.trailing_zeros() / 8;
        let counter = (rest >> (8 * byte)) as u8;
        rest &= !(0xff << (8 * byte));
        let index = module.first + 8 * w + byte as usize;
        features.push(((index as u32) << 3) | crate::features::bucket(counter) as u32);
    }
}

// SanitizerCoverage instruments no function named `__sanitizer_*` (LLVM's
// `SanitizerCoverage.cpp`, and blaze's backend by the same rule), so the drain and the run
// count nothing, call no comparison callback while they read what the callbacks wrote, and
// spend no instrumentation on the scan, which is the engine's whole cost per run.

/// Counter word `w` of the counters at `start`.
///
/// # Safety
///
/// Bytes `8w..8w + 8` from `start` are registered counters.
#[inline(always)]
unsafe fn counter_word(start: *const u8, w: usize) -> u64 {
    // SAFETY: the caller's; read unaligned since the section need not be 8-aligned. The
    // counters live as long as the program, and between runs, as the module doc says, only
    // the watchdog's masked counters can change under the read.
    unsafe { start.add(8 * w).cast::<u64>().read_unaligned() }
}

/// [`Coverage::drain`]: the value profile is read out first, so that what anything here
/// traces (a helper the optimizer kept out of line) lands after it was read; then the
/// counters are read a cache line at a time, and a line of zeros, most of them, costs two
/// vector loads and a test.
#[unsafe(no_mangle)]
#[sanitize(address = "off")]
#[inline(never)]
fn __sanitizer_blaze_fuzzer_drain(coverage: &Coverage<'_>, features: &mut Vec<u32>, values: bool) {
    let mut snapshot = [0u64; VALUE_WORDS];
    if values {
        for (slot, word) in snapshot.iter_mut().zip(&VALUES) {
            *slot = word.load(Ordering::Relaxed);
        }
    }
    debug_assert!(features.capacity() - features.len() >= coverage.total + VALUE_BITS);
    for module in &coverage.modules {
        let words = module.len / 8;
        let lines = words / CHUNK_WORDS * CHUNK_WORDS;
        let start = module.start.cast_const();
        let mut w = 0;
        while w < lines {
            let mut any = 0;
            for i in 0..CHUNK_WORDS {
                // SAFETY: `w + i < words`, inside the module's counters.
                any |= unsafe { counter_word(start, w + i) };
            }
            if any != 0 {
                for i in 0..CHUNK_WORDS {
                    // SAFETY: as above.
                    push_word(features, module, w + i, unsafe { counter_word(start, w + i) });
                }
            }
            w += CHUNK_WORDS;
        }
        for w in lines..words {
            // SAFETY: `w < words`.
            push_word(features, module, w, unsafe { counter_word(start, w) });
        }
        let tail = module.len - 8 * words;
        if tail > 0 {
            let mut bytes = [0u8; 8];
            // SAFETY: the module's last `tail < 8` counters, as `counter_word` reads them.
            unsafe { ptr::copy_nonoverlapping(start.add(8 * words), bytes.as_mut_ptr(), tail) };
            push_word(features, module, words, u64::from_le_bytes(bytes));
        }
    }
    if values {
        let base = (coverage.total as u32) << 3;
        for (w, &word) in snapshot.iter().enumerate() {
            let mut bits = word;
            while bits != 0 {
                let bit = bits.trailing_zeros();
                bits &= bits - 1;
                features.push(base + (64 * w) as u32 + bit);
            }
        }
    }
}

/// Zeroes the module's counters a cache line at a time, writing only the lines that hold a
/// count: the last run's and what the engine reached since. A `memset` would store every
/// byte at half the load bandwidth and dirty every line, which costs a write-back per run
/// once the counters outgrow L2.
#[inline(always)]
fn clear(module: &Module) {
    let words = module.len / 8;
    let lines = words / CHUNK_WORDS * CHUNK_WORDS;
    let start = module.start;
    let mut w = 0;
    while w < lines {
        let mut any = 0;
        for i in 0..CHUNK_WORDS {
            // SAFETY: `w + i < words`, inside the module's counters.
            any |= unsafe { counter_word(start.cast_const(), w + i) };
        }
        if any != 0 {
            // SAFETY: the line just read, inside the counters; cleared between runs as the
            // module doc says.
            unsafe { ptr::write_bytes(start.add(8 * w), 0, 8 * CHUNK_WORDS) };
        }
        w += CHUNK_WORDS;
    }
    // SAFETY: the counters past the last whole line, fewer than 64.
    unsafe { ptr::write_bytes(start.add(8 * lines), 0, module.len - 8 * lines) };
}

/// [`Coverage::run`]. Nothing between the clears and the call of `check` is traced.
#[unsafe(no_mangle)]
#[sanitize(address = "off")]
#[inline(never)]
fn __sanitizer_blaze_fuzzer_run(coverage: &Coverage<'_>, check: fn(&[u8]), data: &[u8], now: u64) -> bool {
    for module in &coverage.modules {
        clear(module);
    }
    // Atomic stores, not a `memset`: the watchdog's comparisons can trace into them at any
    // time. 1 Ki plain stores on x86-64 and AArch64.
    for word in &VALUES {
        word.store(0, Ordering::Relaxed);
    }
    CURRENT.store(data.as_ptr().cast_mut(), Ordering::Relaxed);
    CURRENT_LEN.store(data.len(), Ordering::Relaxed);
    STATE.store(STARTED + now, Ordering::Release);
    let passed = catch_unwind(AssertUnwindSafe(|| check(data))).is_ok();
    if STATE.swap(IDLE, Ordering::AcqRel) == DUMPING {
        // The watchdog is writing `data` out; it ends the process when done.
        loop {
            std::thread::park();
        }
    }
    CURRENT.store(ptr::null_mut(), Ordering::Relaxed);
    CURRENT_LEN.store(0, Ordering::Relaxed);
    passed
}

/// A copy of the input being run, for a report: from the running thread or one the target
/// started (a panic, an AddressSanitizer report), or from the watchdog once
/// [`claim_timed_out`] said it may.
pub(crate) fn current() -> Option<Vec<u8>> {
    let at = CURRENT.load(Ordering::Relaxed);
    if at.is_null() {
        return None;
    }
    let len = CURRENT_LEN.load(Ordering::Relaxed);
    // SAFETY: `run` publishes `data` here before calling the target and withdraws it after
    // the target returned; a caller is either inside that call (the slice is borrowed and
    // unchanged for its whole length) or the watchdog holding `DUMPING`, which keeps `run`
    // from returning.
    Some(unsafe { std::slice::from_raw_parts(at, len) }.to_vec())  // alloc: a crash report's copy of the input, once per process
}

/// For the watchdog: when the running input started more than `limit` milliseconds before
/// `now`, keeps `run` from returning (so [`current`] stays valid) and says so.
pub(crate) fn claim_timed_out(now: u64, limit: u64) -> bool {
    let state = STATE.load(Ordering::Acquire);
    state >= STARTED
        && now.saturating_sub(state - STARTED) > limit
        && STATE.compare_exchange(state, DUMPING, Ordering::AcqRel, Ordering::Relaxed).is_ok()
}

/// A recent comparison: its operands and their width in bytes (0: an empty slot).
pub(crate) fn recent_compare(slot: usize) -> (u64, u64, usize) {
    let at = 3 * (slot % TORC);
    (
        RECENT[at].load(Ordering::Relaxed),
        RECENT[at + 1].load(Ordering::Relaxed),
        RECENT[at + 2].load(Ordering::Relaxed) as usize,
    )
}

/// A recent byte comparison: its operands, and how many bytes of each are in them (0: an
/// empty slot).
pub(crate) fn recent_bytes(slot: usize) -> ([u8; WORD], [u8; WORD], usize) {
    let at = (slot % TORC_WORDS) * WORD_SLOT;
    let len = (RECENT_WORDS[at].load(Ordering::Relaxed) as usize).min(WORD);
    let (mut a, mut b) = ([0u8; WORD], [0u8; WORD]);
    for i in 0..WORD_U64S {
        a[8 * i..8 * i + 8].copy_from_slice(&RECENT_WORDS[at + 1 + i].load(Ordering::Relaxed).to_le_bytes());
        b[8 * i..8 * i + 8]
            .copy_from_slice(&RECENT_WORDS[at + 1 + WORD_U64S + i].load(Ordering::Relaxed).to_le_bytes());
    }
    (a, b, len)
}

/// Whether the code generator gives `return_address`, which tells comparison sites apart in
/// the value profile.
#[inline(never)]
pub(crate) fn return_address_works() -> bool {
    !intrinsics::return_address().is_null()
}

/// Keeps every callback in the link: the linker would drop an object no one refers to.
pub(crate) fn keep() {
    std::hint::black_box((
        engine_functions(),
        __sanitizer_cov_8bit_counters_init as usize,
        __sanitizer_cov_pcs_init as usize,
        __asan_default_options as usize,
        __asan_on_error as usize,
    ));
}
