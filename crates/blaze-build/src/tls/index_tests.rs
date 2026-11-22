//! Thread-locals by index (tls/index.rs, THREADS.md D2) run for real on the host: the getter and
//! the ender built here, emitted by blaze-emit with the indexed access sequence
//! (blaze-emit `pass/tls_index.rs`), placed and called on threads.
//!
//! The rule they keep (LIB library/std/src/thread/local.rs:83, "a "best effort" is made to
//! ensure that destructors for types stored in thread local storage are run"; LIB
//! library/std/src/sys/thread_local/native/mod.rs: each `thread_local!` is one
//! `#[thread_local] static`): every thread sees its own copy of each thread-local, made from
//! the thread-local's initial bytes, at one address for the thread's life, even when the
//! thread-local is placed after the thread touched others (a lazily placed record); and a
//! thread's copies outlive the destructor rounds in which the program's own thread-local
//! destructors run.
//!
//! Without the piece there is no index form: the old block (tls.rs `template`) is laid out
//! before the first instruction and has no room for a thread-local placed later, so
//! `a_thread_local_first_touched_after_another_was_placed` cannot pass.

#![cfg(target_arch = "aarch64")]

use std::alloc::Global;
use std::sync::atomic::{AtomicU64, Ordering};

use blaze_emit::{Code, Isa, Os, RelocTo, Via, emit};
use blaze_inproc::exec::Executable;
use blaze_lir::builder::{MemAttrs, Param, Variadic};
use blaze_lir::func::{BlockKind, Conv, Func, RefKind};
use blaze_lir::{Block, Builder, Flags, Full, Op, Ty, Value};
use blaze_sim::{Broke, Config, Tape};

use super::index::{
    DESC_BYTES, DESC_CAP, DESC_COUNT, DESC_KEY, DESC_ROOT, DESC_VARS, ENDER_IMPORTS, GETTER_IMPORTS, ROOT_OWN,
    ROUNDS, ROW_BYTES, ender, getter, var_row,
};

/// This host's: every Apple M-series core has FEAT_LSE; the Linux arm64 hosts blaze runs on too.
#[cfg(target_os = "macos")]
const ISA: Isa = Isa { lse: true, os: Os::MacOs };
#[cfg(target_os = "linux")]
const ISA: Isa = Isa { lse: true, os: Os::Linux };

/// `_SC_THREAD_DESTRUCTOR_ITERATIONS`: macOS `<unistd.h>` 85, glibc `<bits/confname.h>` 73.
#[cfg(target_os = "macos")]
const SC_THREAD_DESTRUCTOR_ITERATIONS: i32 = 85;
#[cfg(target_os = "linux")]
const SC_THREAD_DESTRUCTOR_ITERATIONS: i32 = 73;

type Destructor = unsafe extern "C" fn(*mut u8);

unsafe extern "C" {
    fn pthread_key_create(key: *mut usize, destructor: Option<Destructor>) -> i32;
    fn pthread_getspecific(key: usize) -> *mut u8;
    fn pthread_setspecific(key: usize, value: *const u8) -> i32;
    fn realloc(p: *mut u8, size: usize) -> *mut u8;
    fn calloc(count: usize, size: usize) -> *mut u8;
    fn aligned_alloc(align: usize, size: usize) -> *mut u8;
    fn memcpy(dst: *mut u8, src: *const u8, n: usize) -> *mut u8;
    fn memset(dst: *mut u8, value: i32, n: usize) -> *mut u8;
    fn free(p: *mut u8);
    fn sysconf(name: i32) -> i64;
}

fn libc_symbol(name: &str) -> u64 {
    match name {
        "pthread_getspecific" => pthread_getspecific as usize as u64,
        "pthread_setspecific" => pthread_setspecific as usize as u64,
        "realloc" => realloc as usize as u64,
        "aligned_alloc" => aligned_alloc as usize as u64,
        "memcpy" => memcpy as usize as u64,
        "memset" => memset as usize as u64,
        "free" => free as usize as u64,
        other => panic!("no symbol {other}"),
    }
}

/// The ender's re-arming calls and frees, counted by the wrapped symbols of the first test.
static ENDER_SETS: AtomicU64 = AtomicU64::new(0);
static FREES: AtomicU64 = AtomicU64::new(0);
/// `ENDER_SETS` when the first free came (`u64::MAX` until then).
static SETS_AT_FIRST_FREE: AtomicU64 = AtomicU64::new(u64::MAX);

extern "C" fn counting_set(key: usize, value: *const u8) -> i32 {
    ENDER_SETS.fetch_add(1, Ordering::SeqCst);
    // SAFETY: forwarded unchanged from the ender, which passes its key and table.
    unsafe { pthread_setspecific(key, value) }
}

extern "C" fn counting_free(p: *mut u8) {
    let sets = ENDER_SETS.load(Ordering::SeqCst);
    let _ = SETS_AT_FIRST_FREE.compare_exchange(u64::MAX, sets, Ordering::SeqCst, Ordering::SeqCst);
    FREES.fetch_add(1, Ordering::SeqCst);
    // SAFETY: forwarded unchanged from the ender: a copy from `aligned_alloc` or the table from
    // `realloc`, each freed once.
    unsafe { free(p) }
}

/// `code` placed: each fixup (a called import, by place) filled by `import`, each relocation by
/// `reloc`, plus its addend. Leaked by the caller: a key's destructor may name it.
fn place(code: &Code<Global>, import: impl Fn(u32) -> u64, reloc: impl Fn(RelocTo) -> u64) -> Executable {
    Executable::written(code.bytes.len(), 0, |out, _| {
        let base = out.as_ptr() as u64;
        out.copy_from_slice(&code.bytes);
        for f in code.fixups.iter() {
            let at = f.at as usize;
            out[at..at + 8].copy_from_slice(&import(f.place).to_le_bytes());
        }
        for r in code.relocs.iter() {
            let to = match r.to {
                RelocTo::Code(off) => base + u64::from(off),
                RelocTo::Func(p) => import(p),
                other => reloc(other),
            };
            let at = r.at as usize;
            out[at..at + 8].copy_from_slice(&to.wrapping_add_signed(r.addend).to_le_bytes());
        }
    })
    .expect("the code maps")
}

/// The getter emitted and placed against libc; its address.
fn placed_getter() -> u64 {
    let get = getter(Global).expect("the getter builds");
    let mut code = Code::new_in(Global);
    emit(get.verified(), &[Via::Address; 6], ISA, &mut code).expect("the getter emits");
    let exec = place(&code, |p| libc_symbol(GETTER_IMPORTS[p as usize]), |o| panic!("the getter names {o:?}"));
    let at = exec.code_address();
    std::mem::forget(exec);
    at
}

/// The ender emitted and placed, its three imports filled by `import`; its address.
fn placed_ender(import: impl Fn(u32) -> u64) -> u64 {
    let end = ender(Global).expect("the ender builds");
    let mut code = Code::new_in(Global);
    emit(end.verified(), &[Via::Address; 3], ISA, &mut code).expect("the ender emits");
    let exec = place(&code, import, |o| panic!("the ender names {o:?}"));
    let at = exec.code_address();
    std::mem::forget(exec);
    at
}

/// A function of `i: i64` that switches over `places` thread-locals: case `k` takes `tls_addr`
/// of thread-local place `k` and gives `then(b, p)`.
fn by_index(places: u32, ret: Ty, then: impl Fn(&mut Builder<Global, Full>, Value) -> Value) -> Func<Global> {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(ret)]);
    b.set_signature(sig);
    let refs: Vec<_> = (0..places).map(|k| b.reference(RefKind::Tls, k)).collect();
    let entry = b.create_block(BlockKind::Normal);
    let refused = b.create_block(BlockKind::Cold);
    let blocks: Vec<_> = (0..places).map(|_| b.create_block(BlockKind::Normal)).collect();
    b.begin(entry);
    let i = b.param(Ty::I64);
    let cases: Vec<(u128, (Block, &[Value]))> =
        blocks.iter().enumerate().map(|(k, &blk)| (k as u128, (blk, &[][..]))).collect();
    b.switch(i, (refused, &[]), &cases);
    b.begin(refused);
    b.trap(0xfe10);
    for (k, &blk) in blocks.iter().enumerate() {
        b.begin(blk);
        let p = b.tls_addr(refs[k]);
        let v = then(&mut b, p);
        b.ret(&[v]);
    }
    b.finish_checked().expect("the switch over thread-locals builds")
}

/// `func` emitted and placed with the descriptor at `desc` and the getter at `getter_at`;
/// thread-local place `k` has index `k`. Its address.
fn placed_access(func: &Func<Global>, desc: u64, getter_at: u64) -> usize {
    let mut code = Code::new_in(Global);
    emit(blaze_lir::verify(func.view()).expect("the access is LIR"), &[], ISA, &mut code).expect("the access emits");
    let exec = place(&code, |p| panic!("the access names fixup {p}"), |o| match o {
        RelocTo::TlsDesc => desc,
        RelocTo::TlsGetter => getter_at,
        RelocTo::TlsIndex(k) => u64::from(k),
        other => panic!("the access names {other:?}"),
    });
    let at = exec.code_address() as usize;
    std::mem::forget(exec);
    at
}

/// A leaked descriptor of `cap` rows, its key made with `ender_at` as the destructor and its
/// root in this host's form. Its address (the rows follow the 40-byte descriptor).
fn descriptor(cap: u64, ender_at: u64) -> u64 {
    let words = (DESC_BYTES + ROW_BYTES * cap as usize) / 8;
    let mem: &'static mut [u64] = Box::leak(vec![0u64; words].into_boxed_slice());
    let at = mem.as_mut_ptr() as u64;
    let mut key = 0usize;
    // SAFETY: the ender was emitted from `ender`, a C function of one pointer parameter, placed
    // and leaked: it lives as long as the process, so the key's destructor stays callable.
    let destructor = unsafe { std::mem::transmute::<u64, Destructor>(ender_at) };
    // SAFETY: `key` is a live out-parameter.
    assert_eq!(unsafe { pthread_key_create(&mut key, Some(destructor)) }, 0);
    write(at, DESC_KEY, key as u64);
    write(at, DESC_ROOT, root(key as u64));
    write(at, DESC_COUNT, 0);
    write(at, DESC_CAP, cap);
    write(at, DESC_VARS, at + DESC_BYTES as u64);
    at
}

/// macOS: the key's slot, `key * 8` from `tpidrro_el0 & !7` (rule 45).
#[cfg(target_os = "macos")]
fn root(key: u64) -> u64 {
    key * 8
}

#[cfg(target_os = "linux")]
std::thread_local! {
    static ROOT: core::cell::Cell<*mut u8> = const { core::cell::Cell::new(core::ptr::null_mut()) };
}

/// Linux: this executable's own thread-local `ROOT`, at one offset from `tpidr_el0` on every
/// thread (the test binary is the executable, as the launcher is), with the flag set.
#[cfg(target_os = "linux")]
fn root(_key: u64) -> u64 {
    let tp: u64;
    // SAFETY: reading `tpidr_el0` has no effect; user code may read it.
    unsafe { core::arch::asm!("mrs {}, tpidr_el0", out(reg) tp) };
    let at = ROOT.with(|c| c.as_ptr() as u64);
    (at - tp) | ROOT_OWN
}

/// The 8-byte word at `desc + off`.
fn write(desc: u64, off: i32, value: u64) {
    // SAFETY: `desc` is a leaked, 8-aligned descriptor with its rows; `off` is inside it.
    // Written by the thread that is the control thread for the test, before any access that
    // reads the word (the spawn or program order orders them).
    unsafe { ((desc + off as u64) as *mut u64).write(value) }
}

/// Row `index` of the descriptor at `desc`, then `count = index + 1` (as a later placement
/// does: the row before the count).
fn place_row(desc: u64, index: u64, row: [u8; 32]) {
    let rows = desc + DESC_BYTES as u64;
    // SAFETY: row `index` < cap is inside the leaked descriptor.
    unsafe { core::ptr::copy_nonoverlapping(row.as_ptr(), (rows + index * ROW_BYTES as u64) as *mut u8, ROW_BYTES) };
    write(desc, DESC_COUNT, index + 1);
}

/// `#[thread_local] static mut A: u64 = 40;` (index 0) and `static mut B: u64 = 7;` (index 1,
/// placed only after thread A touched A), `bump(i)` adding one to thread-local `i`.
///
/// Prints, among its lines:
/// "index 1 placed after thread A's first touch: A reads 7"
/// "the ender frees on round 4"
#[test]
fn a_thread_local_first_touched_after_another_was_placed() {
    // SAFETY: `sysconf` reads a constant of the system.
    let rounds = unsafe { sysconf(SC_THREAD_DESTRUCTOR_ITERATIONS) };
    assert!(rounds >= ROUNDS as i64, "PTHREAD_DESTRUCTOR_ITERATIONS is {rounds}, the ender frees in round {ROUNDS}");
    static INIT_A: u64 = 40;
    static INIT_B: u64 = 7;
    let getter_at = placed_getter();
    let ender_at = placed_ender(|p| match ENDER_IMPORTS[p as usize] {
        "pthread_setspecific" => counting_set as usize as u64,
        "free" => counting_free as usize as u64,
        other => panic!("the ender names {other}"),
    });
    let desc = descriptor(2, ender_at);
    place_row(desc, 0, var_row(8, 8, &INIT_A as *const u64 as u64, 8));
    let bump = by_index(2, Ty::I64, |b, p| {
        let v = b.load(Ty::I64, Flags::NONE, p, 0, MemAttrs::aligned(3));
        let one = b.iconst(Ty::I64, 1);
        let v = b.binary(Op::Add, Flags::NONE, v, one);
        b.store(Flags::NONE, v, p, 0, MemAttrs::aligned(3));
        v
    });
    let bump_at = placed_access(&bump, desc, getter_at);
    // SAFETY: `bump` is an emitted C function of one integer returning an integer; its words
    // hold the getter's and the descriptor's addresses, both leaked.
    let f = unsafe { std::mem::transmute::<usize, extern "C" fn(u64) -> u64>(bump_at) };

    let a = std::thread::spawn(move || {
        let first = f(0);
        // A later placement: row 1 and `count = 2`, while thread A's table holds index 0 only.
        place_row(desc, 1, var_row(8, 8, &INIT_B as *const u64 as u64, 8));
        let b_first = f(1);
        println!("index 1 placed after thread A's first touch: A reads {}", b_first - 1);
        (first, b_first, f(0), f(1))
    })
    .join()
    .expect("thread A");
    assert_eq!(a, (41, 8, 42, 9), "A counts each thread-local from its own copy, B from 7");
    let b = std::thread::spawn(move || (f(0), f(1))).join().expect("thread B");
    assert_eq!(b, (41, 8), "thread B: its own copies, from 40 and 7");
    assert_eq!((INIT_A, INIT_B), (40, 7), "the initial bytes are never written");

    // Thread C alone: its exit runs the ender through four rounds.
    ENDER_SETS.store(0, Ordering::SeqCst);
    FREES.store(0, Ordering::SeqCst);
    SETS_AT_FIRST_FREE.store(u64::MAX, Ordering::SeqCst);
    let c = std::thread::spawn(move || (f(0), f(1))).join().expect("thread C");
    assert_eq!(c, (41, 8));
    let sets = SETS_AT_FIRST_FREE.load(Ordering::SeqCst);
    assert_eq!(sets, ROUNDS - 1, "re-armed in rounds 1 to 3, nothing freed before round 4");
    assert_eq!(FREES.load(Ordering::SeqCst), 3, "round 4 frees both copies and the table");
    assert_eq!(ENDER_SETS.load(Ordering::SeqCst), ROUNDS - 1, "no re-arming in round 4");
    println!("the ender frees on round {}", sets + 1);
}

/// One step of a case: a thread-local placed (its row written and the count raised, as a lazily
/// placed record does), or thread-local `index` touched.
#[derive(Clone, Copy, Debug)]
enum Step {
    Place { size: u64, align: u64, init_len: u64 },
    Touch(u64),
}

const PROPERTY_CAP: u64 = 64;

/// The steps of one case, drawn from the tape: at most 64 placements (four tables' growth: 16,
/// 32, 64 slots), touches of placed indices only.
fn steps(tape: &Tape) -> Vec<Step> {
    let mut count = 0u64;
    let mut out = Vec::new();
    for _ in 0..tape.below(96, "steps") {
        let place = count == 0 || (count < PROPERTY_CAP && tape.bool("place"));
        if place {
            let size = tape.int_in(1u64..=48, "size");
            let align = 1u64 << tape.below(7, "align log2");
            let init_len = tape.below(size + 1, "init_len");
            out.push(Step::Place { size, align, init_len });
            count += 1;
        } else {
            out.push(Step::Touch(tape.below(count, "touch")));
        }
    }
    out
}

/// The initial byte `j` of thread-local `index`, and the mark a touch writes.
fn init_byte(index: u64, j: u64) -> u8 {
    (index * 31 + j * 7 + 1) as u8
}
fn mark_byte(index: u64) -> u8 {
    0xa5 ^ index as u8
}

/// Runs `steps` on a fresh thread through `touch` (the emitted switch giving each index's copy)
/// and checks the oracle: a first touch gives an aligned copy holding the row's initial bytes
/// then zeros, disjoint from every other copy; a later touch gives the same address, holding
/// what the thread wrote.
fn run_case(steps: &[Step], desc: u64, touch: extern "C" fn(u64) -> u64) -> Result<(), Broke> {
    write(desc, DESC_COUNT, 0);
    let steps = steps.to_vec();
    std::thread::spawn(move || {
        // Per index: (address, size) once touched; the rows' sizes.
        let mut seen: Vec<Option<(u64, u64)>> = Vec::new();
        let mut rows: Vec<(u64, u64, u64)> = Vec::new();
        for step in steps {
            match step {
                Step::Place { size, align, init_len } => {
                    let index = rows.len() as u64;
                    let init: Vec<u8> = (0..init_len).map(|j| init_byte(index, j)).collect();
                    let init_at = Box::leak(init.into_boxed_slice()).as_ptr() as u64;
                    place_row(desc, index, var_row(size, align, init_at, init_len));
                    rows.push((size, align, init_len));
                    seen.push(None);
                }
                Step::Touch(index) => {
                    let (size, align, init_len) = rows[index as usize];
                    let p = touch(index);
                    // SAFETY: the getter's copy is at least `size` bytes, alive until the
                    // thread's ender frees it in round 4.
                    let bytes = unsafe { core::slice::from_raw_parts_mut(p as *mut u8, size as usize) };
                    match seen[index as usize] {
                        None => {
                            if p % align != 0 {
                                return Err(Broke::Oracle(format!("index {index}: copy {p:#x} not {align}-aligned")));
                            }
                            for (j, &byte) in bytes.iter().enumerate() {
                                let want = if (j as u64) < init_len { init_byte(index, j as u64) } else { 0 };
                                if byte != want {
                                    return Err(Broke::Oracle(format!(
                                        "index {index}: first touch byte {j} is {byte}, the row gives {want}"
                                    )));
                                }
                            }
                            for (other, s) in seen.iter().enumerate() {
                                if let Some((q, qs)) = *s
                                    && p < q + qs
                                    && q < p + size
                                {
                                    return Err(Broke::Oracle(format!("index {index}'s copy overlaps index {other}'s")));
                                }
                            }
                            bytes.fill(mark_byte(index));
                            seen[index as usize] = Some((p, size));
                        }
                        Some((q, _)) => {
                            if p != q {
                                return Err(Broke::Oracle(format!(
                                    "index {index}: the copy moved from {q:#x} to {p:#x} (a write would be lost)"
                                )));
                            }
                            if let Some(j) = bytes.iter().position(|&b| b != mark_byte(index)) {
                                return Err(Broke::Oracle(format!("index {index}: byte {j} lost the thread's write")));
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    })
    .join()
    .map_err(|_| Broke::Harness("the case's thread panicked".to_owned()))?
}

/// The planted bug: D2's getter, but growth makes a NEW zeroed table without the old slots, so a
/// thread's earlier copies are forgotten once an index past the table is touched.
extern "C" fn planted_getter(desc: *const u8, index: u64, root_at: *mut *mut u8) -> *mut u8 {
    let desc = desc as u64;
    // SAFETY (each read below): `desc` is the test's leaked descriptor, its words and rows set
    // by the case before the touch.
    let word = |at: u64| unsafe { (at as *const u64).read() };
    let key = word(desc + DESC_KEY as u64) as usize;
    // SAFETY: the key was created by the test.
    let mut t = unsafe { pthread_getspecific(key) };
    let len = if t.is_null() { 0 } else { word(t as u64) };
    if index >= len {
        let n = (index + 1).max(2 * len).max(16);
        // SAFETY: a plain zeroed allocation of the header and `n` slots.
        t = unsafe { calloc(1, 32 + 8 * n as usize) };
        assert!(!t.is_null());
        // SAFETY: the header's four words are inside the allocation.
        unsafe {
            let h = t.cast::<u64>();
            h.write(n);
            h.add(2).write(desc);
            h.add(3).write(root_at as u64);
        }
        // SAFETY: as above.
        assert_eq!(unsafe { pthread_setspecific(key, t) }, 0);
        if word(desc + DESC_ROOT as u64) & ROOT_OWN != 0 {
            // SAFETY: the Linux root, this thread's `ROOT`.
            unsafe { root_at.write(t) };
        }
    }
    // SAFETY: slot `index` < `len` is inside the table.
    let slot = unsafe { t.add(32).cast::<*mut u8>().add(index as usize) };
    // SAFETY: as above.
    let mut p = unsafe { slot.read() };
    if p.is_null() {
        let row = word(desc + DESC_VARS as u64) + index * ROW_BYTES as u64;
        let (size, align, init, init_len) = (word(row), word(row + 8).max(16), word(row + 16), word(row + 24));
        let rounded = (size.max(1) + align - 1) & !(align - 1);
        // SAFETY: `align` is a power of two at least 16, `rounded` a multiple of it; the row's
        // initial bytes are `init_len` long and `init_len <= size`.
        unsafe {
            p = aligned_alloc(align as usize, rounded as usize);
            assert!(!p.is_null());
            memcpy(p, init as *const u8, init_len as usize);
            memset(p.add(init_len as usize), 0, (size - init_len) as usize);
            slot.write(p);
        }
    }
    p
}

/// The search's world: one descriptor and one `touch` placed against `getter_at`.
fn world(getter_at: u64) -> (u64, extern "C" fn(u64) -> u64) {
    let ender_at = placed_ender(|p| libc_symbol(ENDER_IMPORTS[p as usize]));
    let desc = descriptor(PROPERTY_CAP, ender_at);
    let touch = by_index(PROPERTY_CAP as u32, Ty::PTR, |_, p| p);
    let at = placed_access(&touch, desc, getter_at);
    // SAFETY: `touch` is an emitted C function of one integer returning a pointer (as an
    // integer); its words hold the getter's and the descriptor's addresses, both leaked.
    (desc, unsafe { std::mem::transmute::<usize, extern "C" fn(u64) -> u64>(at) })
}

/// Placements and touches in any order on one thread: every copy starts from its row, stays at
/// one address and keeps the thread's writes across the table's growth (16, 32, 64 slots). The
/// same search over the planted getter (growth forgets the old slots) must fail: the oracle has
/// teeth.
///
/// Prints nothing; passes as `test tls::index_tests::copies_stay_put_across_growth ... ok`.
#[test]
fn copies_stay_put_across_growth() {
    let (desc, touch) = world(placed_getter());
    blaze_sim::check(Config::cases(200), |tape| run_case(&steps(tape), desc, touch));

    let (desc, touch) = world(planted_getter as usize as u64);
    let verdict = blaze_sim::search(Config::cases(400), |tape| run_case(&steps(tape), desc, touch));
    assert!(
        matches!(&verdict, blaze_sim::Verdict::Failed(failed) if failed.failure.origin == blaze_sim::Origin::Oracle),
        "the planted getter (growth forgets the old slots) was not caught"
    );
}
