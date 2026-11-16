//! The template's layout, and the scheme run for real: the getter and a function bumping a
//! thread-local emitted by blaze-emit, called on two threads, each seeing its own copy.

use std::alloc::Global;

use blaze_typeck::ItemKey;

use super::{TlsError, template};
use crate::data::{DataAlloc, DataRecord, DataReloc, DataTarget};

fn record(allocs: Vec<DataAlloc<Global>>) -> DataRecord<Global> {
    let mut v = Vec::new_in(Global);
    v.extend(allocs);
    DataRecord { allocs: v }
}

fn alloc(bytes: &[u8], align: u8, relocs: &[DataReloc]) -> DataAlloc<Global> {
    DataAlloc { bytes: bytes.to_vec_in(Global), align, mutable: false, relocs: relocs.to_vec_in(Global) }
}

/// Each thread-local at its alignment in key order; a `&str` initialiser's text stays a shared
/// allocation, renumbered, its pointer a relocation of the block.
#[test]
fn the_template_lays_out_every_thread_local() {
    let (a, b) = (ItemKey(2), ItemKey(1));
    let ra = record(vec![alloc(&[7], 0, &[])]);
    let mut wide = [0u8; 16];
    wide[8..].copy_from_slice(&2u64.to_le_bytes());
    let rb = record(vec![alloc(&wide, 3, &[DataReloc { offset: 0, target: DataTarget::Local(1) }]), alloc(b"hi", 0, &[])]);
    let (t, layout) = template(&[(a, &ra), (b, &rb)], Global).unwrap().expect("two thread-locals");
    assert_eq!((layout.offset(b), layout.offset(a)), (Some(0), Some(16)), "B (key 1) first, A after its 16 bytes");
    assert_eq!((layout.size, layout.align), (24, 3), "17 bytes rounded to the alignment, 8");
    assert_eq!(t.allocs.len(), 2);
    assert_eq!(t.allocs[0].bytes[16], 7);
    assert_eq!(t.allocs[0].relocs[0], DataReloc { offset: 0, target: DataTarget::Local(1) });
    assert_eq!(&t.allocs[1].bytes[..], b"hi");
    assert!(template::<Global, Global>(&[], Global).unwrap().is_none(), "no thread-locals, no key");
    let selfish = record(vec![alloc(&[0; 8], 3, &[DataReloc { offset: 0, target: DataTarget::Local(0) }])]);
    assert_eq!(template(&[(a, &selfish)], Global).err(), Some(TlsError::SelfPointer(super::SelfPointer { static_: a })));
}

#[cfg(target_arch = "aarch64")]
mod run {
    use std::alloc::Global;

    use blaze_emit::{Code, Isa, Os, RelocTo, Via, emit};
    use blaze_inproc::exec::Executable;
    use blaze_lir::builder::{MemAttrs, Param, Variadic};
    use blaze_lir::func::{BlockKind, Conv, RefKind};
    use blaze_lir::{Builder, Flags, Full, Op, Ty};
    use blaze_typeck::ItemKey;

    use crate::data::DataRecord;
    use crate::tls::{DESC_BYTES, DESC_KEY, IMPORTS, descriptor, getter, template};

    /// This Mac's: every Apple M-series core has FEAT_LSE (Armv8.1).
    const ISA: Isa = Isa { lse: true, os: Os::MacOs };

    unsafe extern "C" {
        fn pthread_key_create(key: *mut usize, destructor: Option<unsafe extern "C" fn(*mut u8)>) -> i32;
        fn pthread_getspecific(key: usize) -> *mut u8;
        fn pthread_setspecific(key: usize, value: *const u8) -> i32;
        fn aligned_alloc(align: usize, size: usize) -> *mut u8;
        fn memcpy(dst: *mut u8, src: *const u8, n: usize) -> *mut u8;
        fn free(p: *mut u8);
    }

    fn symbol(name: &str) -> u64 {
        match name {
            "pthread_getspecific" => pthread_getspecific as usize as u64,
            "pthread_setspecific" => pthread_setspecific as usize as u64,
            "aligned_alloc" => aligned_alloc as usize as u64,
            "memcpy" => memcpy as usize as u64,
            other => panic!("no symbol {other}"),
        }
    }

    fn place(code: &Code<Global>, fill: impl Fn(RelocOrFixup) -> u64) -> Executable {
        Executable::written(code.bytes.len(), 0, |out, _| {
            let base = out.as_ptr() as u64;
            out.copy_from_slice(&code.bytes);
            for f in code.fixups.iter() {
                let at = f.at as usize;
                out[at..at + 8].copy_from_slice(&fill(RelocOrFixup::Fixup(f.place)).to_le_bytes());
            }
            for r in code.relocs.iter() {
                let to = match r.to {
                    RelocTo::Code(off) => base + u64::from(off),
                    RelocTo::Static(p) => fill(RelocOrFixup::Static(p)),
                    RelocTo::Func(p) => fill(RelocOrFixup::Fixup(p)),
                    other => fill(RelocOrFixup::Other(other)),
                };
                let at = r.at as usize;
                out[at..at + 8].copy_from_slice(&to.wrapping_add_signed(r.addend).to_le_bytes());
            }
        })
        .expect("the code maps")
    }

    enum RelocOrFixup {
        Fixup(u32),
        Static(u32),
        Other(RelocTo),
    }

    /// `#[thread_local] static mut N: u64 = 40;` and `fn bump() -> u64 { N += 1; N }`, on two
    /// threads at once: each counts 41, 42, 43 from its own copy.
    #[test]
    fn two_threads_each_see_their_own_thread_local() {
        let n = ItemKey(1);
        let mut init = DataRecord::literal(&40u64.to_le_bytes(), Global);
        init.allocs[0].align = 3;
        let (tpl, layout) = template(&[(n, &init)], Global).unwrap().unwrap();
        let off = layout.offset(n).unwrap();
        // The template and descriptor, placed by hand (no relocations in this template).
        let tpl_bytes: &'static [u8] = Box::leak(tpl.allocs[0].bytes.to_vec().into_boxed_slice());
        let desc = descriptor(ItemKey(99), &layout, Global);
        let mut d = vec![0u8; DESC_BYTES];
        d.copy_from_slice(&desc.allocs[0].bytes);
        d[24..32].copy_from_slice(&(tpl_bytes.as_ptr() as u64).to_le_bytes());
        let desc_bytes: &'static mut [u8] = Box::leak(d.into_boxed_slice());
        let mut key = 0usize;
        // SAFETY: `key` is a live out-parameter; `free` matches `aligned_alloc`.
        assert_eq!(unsafe { pthread_key_create(&mut key, Some(free)) }, 0);
        desc_bytes[DESC_KEY as usize..][..8].copy_from_slice(&(key as u64).to_le_bytes());
        let desc_at = desc_bytes.as_ptr() as u64;

        let get = getter(Global).expect("the getter builds");
        let mut get_code = Code::new_in(Global);
        emit(get.view(), &[Via::Address; 4], ISA, &mut get_code).expect("the getter emits");
        let get_exec = place(&get_code, |r| match r {
            RelocOrFixup::Fixup(p) => symbol(IMPORTS[p as usize]),
            RelocOrFixup::Static(_) => unreachable!("the getter names no static"),
            RelocOrFixup::Other(o) => panic!("the getter names {o:?}"),
        });

        let mut b = Builder::<Global, Full>::new_in(64, Global);
        let sig = b.sig(Conv::C, Variadic::No, &[], &[Param::of(Ty::I64)]);
        b.set_signature(sig);
        let tls = b.reference(RefKind::Tls, 0);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let p = b.tls_addr(tls);
        let v = b.load(Ty::I64, Flags::NONE, p, 0, MemAttrs::aligned(3));
        let one = b.iconst(Ty::I64, 1);
        let v = b.binary(Op::Add, Flags::NONE, v, one);
        b.store(Flags::NONE, v, p, 0, MemAttrs::aligned(3));
        b.ret(&[v]);
        let bump = b.finish_checked().expect("bump builds");
        let mut bump_code = Code::new_in(Global);
        emit(bump.view(), &[], ISA, &mut bump_code).expect("bump emits");
        let getter_at = get_exec.code_address();
        let bump_exec = place(&bump_code, |r| match r {
            RelocOrFixup::Other(RelocTo::TlsDesc) => desc_at,
            RelocOrFixup::Other(RelocTo::TlsGetter) => getter_at,
            RelocOrFixup::Other(RelocTo::TlsOffset(0)) => u64::from(off),
            RelocOrFixup::Fixup(p) => panic!("bump names fixup {p}"),
            RelocOrFixup::Static(p) => panic!("bump names static {p}"),
            RelocOrFixup::Other(o) => panic!("bump names {o:?}"),
        });
        let bump_at = bump_exec.code_address() as usize;

        let gate = std::sync::Barrier::new(2);
        let run = || {
            // SAFETY: `bump` is an emitted C function of no parameters returning a u64; its
            // words hold the getter's and the descriptor's addresses, both alive (leaked or
            // held by the test until the threads are joined).
            let f = unsafe { std::mem::transmute::<usize, extern "C" fn() -> u64>(bump_at) };
            let barrier_free: Vec<u64> = (0..3).map(|_| f()).collect();
            // SAFETY: the key was created above.
            let block = unsafe { pthread_getspecific(key) } as usize;
            // Both threads alive here, so neither block is freed and reused for the other.
            gate.wait();
            (barrier_free, block)
        };
        let (a, b) = std::thread::scope(|s| {
            let one = s.spawn(|| {
                gate.wait();
                run()
            });
            let two = s.spawn(|| {
                gate.wait();
                run()
            });
            (one.join().unwrap(), two.join().unwrap())
        });
        assert_eq!(a.0, [41, 42, 43], "thread one counts from the template's 40");
        assert_eq!(b.0, [41, 42, 43], "thread two counts from its own copy");
        assert_ne!(a.1, b.1, "two blocks");
        assert_eq!(&tpl_bytes[..8], &40u64.to_le_bytes(), "the template is never written");
        drop((get_exec, bump_exec));
    }

    thread_local! {
        static NATIVE: core::cell::Cell<u64> = const { core::cell::Cell::new(40) };
    }

    /// The cost of one thread-local access (rule 67's floor), in a loop of `n` increments
    /// emitted as `loop { p = tls_addr N; *p += 1 }`: the macOS form (the direct read of the
    /// thread's slot, the getter only on a miss), the portable form (the getter called each
    /// time, as on Linux), and a native `thread_local!` for scale. Both emitted forms run the
    /// real emitted getter. Prints ns per access; asserts only the counts.
    #[cfg(target_os = "macos")]
    #[test]
    fn thread_local_access_cost() {
        const N: u64 = 20_000_000;
        let n_item = ItemKey(1);
        let mut init = DataRecord::literal(&40u64.to_le_bytes(), Global);
        init.allocs[0].align = 3;
        let (tpl, layout) = template(&[(n_item, &init)], Global).unwrap().unwrap();
        let off = layout.offset(n_item).unwrap();
        let tpl_bytes: &'static [u8] = Box::leak(tpl.allocs[0].bytes.to_vec().into_boxed_slice());
        let desc = descriptor(ItemKey(99), &layout, Global);
        let mut d = vec![0u8; DESC_BYTES];
        d.copy_from_slice(&desc.allocs[0].bytes);
        d[24..32].copy_from_slice(&(tpl_bytes.as_ptr() as u64).to_le_bytes());
        let desc_bytes: &'static mut [u8] = Box::leak(d.into_boxed_slice());
        let mut key = 0usize;
        // SAFETY: `key` is a live out-parameter; `free` matches `aligned_alloc`.
        assert_eq!(unsafe { pthread_key_create(&mut key, Some(free)) }, 0);
        desc_bytes[DESC_KEY as usize..][..8].copy_from_slice(&(key as u64).to_le_bytes());
        let desc_at = desc_bytes.as_ptr() as u64;

        let get = getter(Global).expect("the getter builds");
        let mut get_code = Code::new_in(Global);
        emit(get.view(), &[Via::Address; 4], ISA, &mut get_code).expect("the getter emits");
        let get_exec = place(&get_code, |r| match r {
            RelocOrFixup::Fixup(p) => symbol(IMPORTS[p as usize]),
            RelocOrFixup::Static(_) => unreachable!("the getter names no static"),
            RelocOrFixup::Other(o) => panic!("the getter names {o:?}"),
        });
        let getter_at = get_exec.code_address();

        // fn(n) -> u64 { let mut i = 0; loop { let p = tls_addr N; *p += 1; i += 1; if i == n { return *p } } }
        let mut b = Builder::<Global, Full>::new_in(64, Global);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b.set_signature(sig);
        let tls = b.reference(RefKind::Tls, 0);
        let entry = b.create_block(BlockKind::Normal);
        let body = b.create_block(BlockKind::Normal);
        let done = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let n = b.param(Ty::I64);
        let zero = b.iconst(Ty::I64, 0);
        b.jump((body, &[zero]));
        b.begin(body);
        let i = b.param(Ty::I64);
        let p = b.tls_addr(tls);
        let v = b.load(Ty::I64, Flags::NONE, p, 0, MemAttrs::aligned(3));
        let one = b.iconst(Ty::I64, 1);
        let v = b.binary(Op::Add, Flags::NONE, v, one);
        b.store(Flags::NONE, v, p, 0, MemAttrs::aligned(3));
        let i = b.binary(Op::Add, Flags::NONE, i, one);
        let end = b.icmp(blaze_lir::IntCond::Eq, i, n);
        b.br(Flags::NONE, end, (done, &[v]), (body, &[i]));
        b.begin(done);
        let r = b.param(Ty::I64);
        b.ret(&[r]);
        let looped = b.finish_checked().expect("the loop builds");

        // The same loop on a plain pointer (no thread-local): the baseline tier's own cost of
        // the loop, to subtract (every value lives in a stack cell at this tier).
        let mut b = Builder::<Global, Full>::new_in(64, Global);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64), Param::of(Ty::PTR)], &[Param::of(Ty::I64)]);
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        let body = b.create_block(BlockKind::Normal);
        let done = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let n = b.param(Ty::I64);
        let p = b.param(Ty::PTR);
        let zero = b.iconst(Ty::I64, 0);
        b.jump((body, &[zero]));
        b.begin(body);
        let i = b.param(Ty::I64);
        let v = b.load(Ty::I64, Flags::NONE, p, 0, MemAttrs::aligned(3));
        let one = b.iconst(Ty::I64, 1);
        let v = b.binary(Op::Add, Flags::NONE, v, one);
        b.store(Flags::NONE, v, p, 0, MemAttrs::aligned(3));
        let i = b.binary(Op::Add, Flags::NONE, i, one);
        let end = b.icmp(blaze_lir::IntCond::Eq, i, n);
        b.br(Flags::NONE, end, (done, &[v]), (body, &[i]));
        b.begin(done);
        let r = b.param(Ty::I64);
        b.ret(&[r]);
        let plain = b.finish_checked().expect("the plain loop builds");

        let time = |f: &dyn Fn() -> u64| {
            let warm = f();
            let t = std::time::Instant::now();
            let got = f();
            (t.elapsed().as_nanos() as f64 / N as f64, warm, got)
        };
        let mut report = Vec::new();
        {
            let mut code = Code::new_in(Global);
            emit(plain.view(), &[], ISA, &mut code).expect("the plain loop emits");
            let exec = place(&code, |r| match r {
                RelocOrFixup::Fixup(p) => panic!("fixup {p}"),
                RelocOrFixup::Static(p) => panic!("static {p}"),
                RelocOrFixup::Other(o) => panic!("{o:?}"),
            });
            let at = exec.code_address() as usize;
            let mut cell = 40u64;
            let cell_at = &raw mut cell;
            // SAFETY: an emitted C function `fn(i64, ptr) -> i64` touching only `cell`.
            let f = unsafe { std::mem::transmute::<usize, extern "C" fn(u64, *mut u64) -> u64>(at) };
            let (ns, warm, got) = time(&|| f(N, cell_at));
            assert_eq!((warm, got), (40 + N, 40 + 2 * N), "plain");
            report.push(("emitted, no thread-local (the loop alone)".to_string(), ns));
        }
        for os in [Os::MacOs, Os::Linux] {
            let mut code = Code::new_in(Global);
            emit(looped.view(), &[], Isa { lse: true, os }, &mut code).expect("the loop emits");
            let exec = place(&code, |r| match r {
                RelocOrFixup::Other(RelocTo::TlsDesc) => desc_at,
                RelocOrFixup::Other(RelocTo::TlsGetter) => getter_at,
                RelocOrFixup::Other(RelocTo::TlsOffset(0)) => u64::from(off),
                RelocOrFixup::Fixup(p) => panic!("fixup {p}"),
                RelocOrFixup::Static(p) => panic!("static {p}"),
                RelocOrFixup::Other(o) => panic!("{o:?}"),
            });
            let at = exec.code_address() as usize;
            // Each run on a fresh thread: its block starts from the template's 40.
            let (ns, warm, got) = std::thread::scope(|s| {
                s.spawn(|| {
                    // SAFETY: an emitted C function `fn(i64) -> i64` whose words hold the
                    // descriptor (leaked) and the getter (alive in `get_exec`).
                    let f = unsafe { std::mem::transmute::<usize, extern "C" fn(u64) -> u64>(at) };
                    time(&|| f(N))
                })
                .join()
                .unwrap()
            });
            assert_eq!((warm, got), (40 + N, 40 + 2 * N), "{os:?}");
            report.push((format!("emitted, {os:?} form"), ns));
            drop(exec);
        }
        let (ns, warm, got) = std::thread::scope(|s| {
            s.spawn(|| {
                time(&|| {
                    let mut last = 0;
                    for _ in 0..core::hint::black_box(N) {
                        last = NATIVE.with(|c| {
                            let v = c.get() + 1;
                            c.set(v);
                            v
                        });
                        core::hint::black_box(&last);
                    }
                    last
                })
            })
            .join()
            .unwrap()
        });
        assert_eq!((warm, got), (40 + N, 40 + 2 * N), "native");
        report.push(("native thread_local!".to_string(), ns));
        for (what, ns) in report {
            println!("tls access cost: {what}: {ns:.2} ns per increment");
        }
        drop(get_exec);
    }
}
