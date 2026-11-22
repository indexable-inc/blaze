//! `tls_addr` by index (pass/tls_index.rs): the words it names in both forms, and the macOS form
//! run on two threads against a getter written in Rust to D2's table layout (blaze-build's
//! `tls::index::getter` is the real one; this crate cannot depend on it, and blaze-build's
//! `tls::index_tests` runs this sequence against that getter).
//!
//! Prints nothing; passes as `test tls_index_tests::... ok`.

use std::alloc::Global;

use blaze_lir::{
    Builder, Flags, Full, Op, Ty,
    builder::{MemAttrs, Param, Variadic},
    func::{BlockKind, Conv, Func, RefKind},
};

use crate::{Isa, Os, RelocTo, tests::emitted_for};

/// `bump() { N += 1; N }` for thread-local place 0 (tier_tests/thread_locals.rs runs it through
/// both tiers).
pub(crate) fn bump() -> Func<Global> {
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
    b.finish_checked().expect("bump builds")
}

/// Both forms name the descriptor, the getter and the index word of place 0, and nothing else
/// (no block offset: the old form's `TlsOffset` is gone from this sequence).
#[test]
fn the_indexed_access_names_the_descriptor_the_getter_and_the_index() {
    let func = bump();
    for os in [Os::MacOs, Os::Linux] {
        let code = emitted_for(func.view(), &[], Isa { lse: true, os }).expect("emits");
        let mut named: Vec<RelocTo> = code.relocs.iter().map(|r| r.to).collect();
        named.sort_by_key(|it| format!("{it:?}"));
        assert_eq!(named, [RelocTo::TlsDesc, RelocTo::TlsGetter, RelocTo::TlsIndex(0)], "{os:?}");
    }
}

/// The test getter, shared with tier_tests/thread_locals.rs.
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
pub(crate) mod run {
    use std::cell::Cell;

    use super::{Isa, Os, RelocTo, bump, emitted_for};
    use crate::tests::run::place_all;

    unsafe extern "C" {
        pub(crate) fn pthread_key_create(key: *mut usize, destructor: Option<unsafe extern "C" fn(*mut u8)>) -> i32;
        pub(crate) fn pthread_getspecific(key: usize) -> *mut u8;
        fn pthread_setspecific(key: usize, value: *const u8) -> i32;
        fn calloc(count: usize, size: usize) -> *mut u8;
    }

    /// D2's descriptor: `key`, `root`, `count`, `cap`, `vars` (the test's getter reads only
    /// `key`; the sequence reads `root`).
    #[repr(C)]
    pub(crate) struct Desc {
        pub(crate) key: u64,
        pub(crate) root: u64,
        pub(crate) count: u64,
        pub(crate) cap: u64,
        pub(crate) vars: u64,
    }

    impl Desc {
        /// A descriptor of a fresh pthread key, leaked for the test.
        pub(crate) fn leaked() -> &'static Desc {
            let mut key = 0usize;
            // SAFETY: `key` is a live out-parameter.
            assert_eq!(unsafe { pthread_key_create(&mut key, None) }, 0);
            Box::leak(Box::new(Desc { key: key as u64, root: key as u64 * 8, count: 1, cap: 1, vars: 0 }))
        }
    }

    std::thread_local! {
        /// The getter's calls on this thread (per thread, so tests running at once do not mix).
        pub(crate) static GETTER_CALLS: Cell<u64> = const { Cell::new(0) };
    }

    /// D2's getter contract, cut to what the test needs: a table of 16 slots (header 32 bytes),
    /// made on the thread's first call, and a zeroed 8-byte copy per index. Leaks at thread exit
    /// (the test's key has no destructor).
    pub(crate) extern "C" fn getter(desc: *const Desc, index: u64, root_at: *mut *mut u8) -> *mut u8 {
        GETTER_CALLS.with(|n| n.set(n.get() + 1));
        assert!(index < 16, "the test's table has 16 slots");
        // SAFETY: `desc` is the test's descriptor, leaked for the test.
        let desc = unsafe { &*desc };
        // SAFETY: the key was created by the test.
        let mut table = unsafe { pthread_getspecific(desc.key as usize) };
        if table.is_null() {
            // SAFETY: a plain zeroed allocation of the header and 16 slots.
            table = unsafe { calloc(1, 32 + 8 * 16) };
            assert!(!table.is_null());
            // SAFETY: `table` holds 32 + 128 bytes, 16-aligned; `len` is its first word.
            unsafe { table.cast::<u64>().write(16) };
            // SAFETY: as above.
            assert_eq!(unsafe { pthread_setspecific(desc.key as usize, table) }, 0);
        }
        // SAFETY: the sequence passes the root's address, the thread's slot for the key on
        // macOS, which now holds `table` (pthread_setspecific wrote it).
        assert_eq!(unsafe { root_at.read() }, table, "the root the sequence found is the key's slot");
        // SAFETY: slot `index` < 16 is inside the table.
        let slot = unsafe { table.add(32).cast::<*mut u8>().add(index as usize) };
        // SAFETY: as above.
        let mut copy = unsafe { slot.read() };
        if copy.is_null() {
            // SAFETY: a plain zeroed 8-byte allocation.
            copy = unsafe { calloc(1, 8) };
            assert!(!copy.is_null());
            // SAFETY: as above.
            unsafe { slot.write(copy) };
        }
        copy
    }

    /// `bump` run three times on each of two threads: each counts 1, 2, 3 from its own copy;
    /// the getter runs once per thread (no table yet), later accesses take the in-line path.
    #[test]
    fn the_indexed_access_gives_each_thread_its_own_copy() {
        let func = bump();
        let desc = Desc::leaked();
        let key = desc.key as usize;
        let code = emitted_for(func.view(), &[], Isa { lse: true, os: Os::MacOs }).expect("emits");
        let exec = place_all(&code, |_| unreachable!(), |to| match to {
            RelocTo::TlsDesc => desc as *const Desc as u64,
            RelocTo::TlsGetter => getter as usize as u64,
            RelocTo::TlsIndex(0) => 0,
            other => panic!("{other:?}"),
        });
        let entry = exec.code_address() as usize;
        let gate = std::sync::Barrier::new(2);
        let run = || {
            // SAFETY: an emitted C function of no parameters returning an integer; its words
            // hold the descriptor (leaked) and the getter.
            let f = unsafe { std::mem::transmute::<usize, extern "C" fn() -> u64>(entry) };
            gate.wait();
            let counts: Vec<u64> = (0..3).map(|_| f()).collect();
            // SAFETY: the key was created above.
            let table = unsafe { pthread_getspecific(key) } as usize;
            // Both threads alive here, so neither table is freed and reused for the other.
            gate.wait();
            (counts, table, GETTER_CALLS.with(Cell::get))
        };
        let (a, b) = std::thread::scope(|s| {
            let one = s.spawn(run);
            let two = s.spawn(run);
            (one.join().unwrap(), two.join().unwrap())
        });
        assert_eq!(a.0, [1, 2, 3], "thread one");
        assert_eq!(b.0, [1, 2, 3], "thread two, from its own copy");
        assert_ne!(a.1, b.1, "two tables");
        assert_eq!((a.2, b.2), (1, 1), "the getter once per thread");
        drop(exec);
    }
}
