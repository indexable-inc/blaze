//! `tls_addr` run on two threads with a getter written in Rust (blaze-build's getter is the
//! real one; this crate cannot depend on it), and the macOS direct read it rests on.

#![cfg(all(target_arch = "aarch64", target_os = "macos"))]

use std::{
    alloc::Global,
    sync::atomic::{AtomicU64, Ordering},
};

use blaze_lir::{
    Builder, Flags, Full, Op, Ty,
    builder::{MemAttrs, Param, Variadic},
    func::{BlockKind, Conv, RefKind},
};

use crate::{
    Isa, Os, RelocTo,
    tests::{emitted_for, run},
};

unsafe extern "C" {
    fn pthread_key_create(key: *mut usize, destructor: Option<unsafe extern "C" fn(*mut u8)>) -> i32;
    fn pthread_getspecific(key: usize) -> *mut u8;
    fn pthread_setspecific(key: usize, value: *const u8) -> i32;
    fn calloc(count: usize, size: usize) -> *mut u8;
    fn free(p: *mut u8);
}

/// The thread's specific-data slot `key` read as `_pthread_getspecific_direct` does: the
/// slots at `tpidrro_el0 & ~7`, one word per key.
fn direct(key: usize) -> usize {
    let base: usize;
    // SAFETY: reading `tpidrro_el0` has no effect; user code may read it on macOS.
    unsafe { core::arch::asm!("mrs {}, tpidrro_el0", out(reg) base) };
    let slots = (base & !7) as *const usize;
    // SAFETY: libpthread keeps the thread's slots there for the thread's life, a key indexing
    // its own word (the private ABI this test pins).
    unsafe { slots.add(key).read() }
}

/// macOS private ABI (rule 45): the direct read `tls_addr` emits equals `pthread_getspecific`,
/// on two threads with different values.
#[test]
fn the_direct_tsd_read_equals_pthread_getspecific() {
    let mut key = 0usize;
    // SAFETY: `key` is a live out-parameter.
    assert_eq!(unsafe { pthread_key_create(&mut key, None) }, 0);
    let check = |mark: usize| {
        assert_eq!(direct(key), 0, "a fresh thread's slot is empty");
        // SAFETY: the key was created above; the value is never dereferenced.
        assert_eq!(unsafe { pthread_setspecific(key, mark as *const u8) }, 0);
        // SAFETY: as above.
        let official = unsafe { pthread_getspecific(key) } as usize;
        assert_eq!(official, mark);
        assert_eq!(direct(key), official, "the direct read of key {key}");
    };
    std::thread::scope(|s| {
        s.spawn(|| check(0x1111_0000));
        s.spawn(|| check(0x2222_0000));
    });
}

/// The test's descriptor: blaze-build's layout's first word, the key.
#[repr(C)]
struct Desc {
    key: u64,
    size: u64,
}

static GETTER_CALLS: AtomicU64 = AtomicU64::new(0);

/// The getter's contract: the calling thread's block for `desc`, made zeroed on first use.
extern "C" fn getter(desc: *const Desc) -> *mut u8 {
    GETTER_CALLS.fetch_add(1, Ordering::Relaxed);
    // SAFETY: `desc` is the test's descriptor, alive for the test.
    let desc = unsafe { &*desc };
    // SAFETY: the key was created by the test.
    let have = unsafe { pthread_getspecific(desc.key as usize) };
    if !have.is_null() {
        return have;
    }
    // SAFETY: a plain allocation, freed by the key's destructor at thread exit.
    let block = unsafe { calloc(1, desc.size as usize) };
    assert!(!block.is_null());
    // SAFETY: as above.
    assert_eq!(unsafe { pthread_setspecific(desc.key as usize, block) }, 0);
    block
}

/// `bump() { N += 1; N }` for a thread-local `N` at offset 16 of the block, run three times on
/// each of two threads: each counts 1, 2, 3 from its own zeroed copy. On macOS the getter runs
/// once per thread (later accesses take the direct read); the Linux sequence asks it each time.
#[test]
fn tls_addr_gives_each_thread_its_own_copy() {
    const OFFSET: u64 = 16;
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(Conv::C, Variadic::No, &[], &[Param::of(Ty::I64)]);
    b.set_signature(sig);
    let n = b.reference(RefKind::Tls, 0);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let p = b.tls_addr(n);
    let v = b.load(Ty::I64, Flags::NONE, p, 0, MemAttrs::aligned(3));
    let one = b.iconst(Ty::I64, 1);
    let v = b.binary(Op::Add, Flags::NONE, v, one);
    b.store(Flags::NONE, v, p, 0, MemAttrs::aligned(3));
    b.ret(&[v]);
    let func = b.finish_checked().expect("builds");

    for (os, calls_per_thread) in [(Os::MacOs, 1), (Os::Linux, 3)] {
        let mut key = 0usize;
        // SAFETY: `key` is a live out-parameter; `free` matches `calloc`.
        assert_eq!(unsafe { pthread_key_create(&mut key, Some(free)) }, 0);
        let desc: &'static Desc = Box::leak(Box::new(Desc { key: key as u64, size: 32 }));
        let code = emitted_for(func.view(), &[], Isa { lse: true, os }).expect("emits");
        let mut named: Vec<RelocTo> = code.relocs.iter().map(|r| r.to).collect();
        named.sort_by_key(|it| format!("{it:?}"));
        assert_eq!(named, [RelocTo::TlsDesc, RelocTo::TlsGetter, RelocTo::TlsOffset(0)], "{os:?}");
        let exec = run::place_all(&code, |_| unreachable!(), |to| match to {
            RelocTo::TlsDesc => desc as *const Desc as u64,
            RelocTo::TlsGetter => getter as usize as u64,
            RelocTo::TlsOffset(0) => OFFSET,
            other => panic!("{other:?}"),
        });
        let entry = exec.code_address() as usize;
        GETTER_CALLS.store(0, Ordering::Relaxed);
        let gate = std::sync::Barrier::new(2);
        let run = || {
            // SAFETY: an emitted C function of no parameters returning an integer; its words
            // hold the descriptor (leaked) and the getter.
            let f = unsafe { std::mem::transmute::<usize, extern "C" fn() -> u64>(entry) };
            gate.wait();
            let counts: Vec<u64> = (0..3).map(|_| f()).collect();
            // SAFETY: the key was created above.
            let block = unsafe { pthread_getspecific(key) } as usize;
            // Both threads alive here, so neither block is freed and reused for the other.
            gate.wait();
            (counts, block)
        };
        let (a, b) = std::thread::scope(|s| {
            let one = s.spawn(run);
            let two = s.spawn(run);
            (one.join().unwrap(), two.join().unwrap())
        });
        assert_eq!(a.0, [1, 2, 3], "{os:?}: thread one");
        assert_eq!(b.0, [1, 2, 3], "{os:?}: thread two, from its own copy");
        assert_ne!(a.1, b.1, "{os:?}: two blocks");
        assert_eq!(GETTER_CALLS.load(Ordering::Relaxed), 2 * calls_per_thread, "{os:?}: getter calls");
    }
}
