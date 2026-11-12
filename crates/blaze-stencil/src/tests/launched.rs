//! Tier 0 code in blaze's launcher (docs/v2/plan/design/launcher.md, "Step 3"): functions
//! compiled by this crate under [`crate::Target::slots`], made records
//! (`blaze_launch::record`) and run in a real launcher (`blaze-launch-rt`) whose daemon is
//! this test. Nothing is linked: a function is compiled when its stub first asks for it,
//! placed by the launcher, and replaced while a thread calls it.
//!
//! What only this can show, and the unit tests of the parts cannot: that the bytes the
//! glue writes under `slots` and the launcher's reading of their relocations agree, on a
//! real machine, for each thing code names: a callee (its slot word), a function's address
//! (its entry thunk), a static and an outside symbol (the record's pool), a thread-local
//! (the pool's descriptor and the launcher's thunk), and its own unwind rows (a panic
//! passes through placed code).
//!
//! Unsafe, as `exec` is: it calls code the launcher placed, by address.

#![allow(unsafe_code)]

use std::io::{Read, Write as _};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Duration;

use blaze_launch::record::{Fixup, Kind, Place, Record, To, Unwind, What};
use blaze_launch::wire::{self, Ask, List, ProgramId, RecordId, Tell};
use blaze_launch::{CodeId, KEPT, Slot, Tier, Txn};
use blaze_launch_rt::{Config, Driver, Loaded};
use cranelift_codegen::binemit::Reloc;
use cranelift_codegen::ir::{self, InstBuilder, types::I64};

use super::func::{self, FUNCS, Mk};
use super::host;
use crate::code::{Code, Name, Rule};

/// How long the test waits for another thread before it fails.
const PATIENCE: Duration = Duration::from_secs(30);

/// The namespace of user names that are the program's statics, by number.
const STATICS: u32 = 1;

/// The program's functions, by slot.
const LEAF: u32 = 0;
const TWICE: u32 = 1;
const MIXED: u32 = 2;
const BUMP: u32 = 3;
const THROUGH: u32 = 4;
const SLOTS: u32 = 5;

/// The host's target, its code made for the launcher.
fn target() -> crate::Target {
    let host = host::host().expect("a host the tests run code on");
    crate::Target { slots: true, counts: false, ..host }
}

/// Function `slot` of the program: `leaf` adds `k`, which is what an edit changes.
fn function(mk: &Mk, slot: u32, k: i64) -> ir::Function {
    let unary = mk.sig_of(&[I64], &[I64]);
    match slot {
        // leaf(x) = x + k
        LEAF => mk.func(slot, unary, |b, p| {
            let k = b.ins().iconst(I64, k);
            let sum = b.ins().iadd(p[0], k);
            b.ins().return_(&[sum]);
        }),
        // twice(x) = leaf(x) + leaf(x): two calls through the callee's slot word.
        TWICE => mk.func(slot, unary.clone(), |b, p| {
            let leaf = func::import(b, FUNCS, LEAF, unary.clone(), true);
            let one = func::call(b, leaf, &[p[0]])[0];
            let two = func::call(b, leaf, &[p[0]])[0];
            let sum = b.ins().iadd(one, two);
            b.ins().return_(&[sum]);
        }),
        // mixed(x) = (*&leaf)(x) + STATIC0 + labs(x): a function's address, a static's
        // value, an outside symbol.
        MIXED => mk.func(slot, unary.clone(), |b, p| {
            let leaf = func::import(b, FUNCS, LEAF, unary.clone(), true);
            let address = b.ins().func_addr(I64, leaf);
            let sig = b.import_signature(unary.clone());
            let called = func::call_indirect(b, sig, address, &[p[0]])[0];
            let name = b.func.declare_imported_user_function(ir::UserExternalName::new(STATICS, 0));
            let global = b.create_global_value(ir::GlobalValueData::Symbol {
                name: ir::ExternalName::User(name),
                offset: ir::immediates::Imm64::new(0),
                colocated: true,
                tls: false,
            });
            let at = b.ins().symbol_value(I64, global);
            let held = b.ins().load(I64, func::mem(), at, 0);
            let labs = func::import(b, func::HOST, 0, unary.clone(), false);
            let abs = func::call(b, labs, &[p[0]])[0];
            let sum = b.ins().iadd(called, held);
            let sum = b.ins().iadd(sum, abs);
            b.ins().return_(&[sum]);
        }),
        // bump() = { TLS1 += 1; TLS1 }: this thread's copy of a thread-local.
        BUMP => mk.func(slot, mk.sig_of(&[], &[I64]), |b, _| {
            let name = b.func.declare_imported_user_function(ir::UserExternalName::new(STATICS, 1));
            let global = b.create_global_value(ir::GlobalValueData::Symbol {
                name: ir::ExternalName::User(name),
                offset: ir::immediates::Imm64::new(0),
                colocated: false,
                tls: true,
            });
            let at = b.ins().tls_value(I64, global);
            let held = b.ins().load(I64, func::mem(), at, 0);
            let one = b.ins().iconst(I64, 1);
            let more = b.ins().iadd(held, one);
            b.ins().store(func::mem(), more, at, 0);
            b.ins().return_(&[more]);
        }),
        // through(f, x) = f(x) + 1: a frame an unwind passes through.
        THROUGH => mk.func(slot, mk.sig_of(&[I64, I64], &[I64]), |b, p| {
            let sig = b.import_signature(unary.clone());
            let called = func::call_indirect(b, sig, p[0], &[p[1]])[0];
            let one = b.ins().iconst(I64, 1);
            let sum = b.ins().iadd(called, one);
            b.ins().return_(&[sum]);
        }),
        other => panic!("the program has no function {other}"),
    }
}

/// A record's kind of a relocation the glue writes: `hir-ty-codegen`'s `reloc_kind`, then
/// `blaze_codegen::RelocKind` by name.
fn kind(reloc: Reloc) -> Kind {
    match reloc {
        Reloc::Abs8 => Kind::Abs64,
        Reloc::X86PCRel4 | Reloc::X86CallPCRel4 | Reloc::X86CallPLTRel4 => Kind::PcRel32,
        Reloc::X86GOTPCRel4 => Kind::GotPcRel32,
        Reloc::Arm64Call => Kind::Call26,
        Reloc::Aarch64AdrPrelPgHi21 => Kind::AdrpPage21,
        Reloc::Aarch64AddAbsLo12Nc => Kind::PageOffset12,
        Reloc::Aarch64AdrGotPage21 => Kind::GotPage21,
        Reloc::Aarch64Ld64GotLo12Nc => Kind::GotPageOffset12,
        Reloc::MachOX86_64Tlv => Kind::TlvRel32,
        Reloc::MachOAarch64TlsAdrPage21 => Kind::TlvPage21,
        Reloc::MachOAarch64TlsAdrPageOff12 => Kind::TlvPageOffset12,
        Reloc::Aarch64TlsDescAdrPage21 => Kind::TlsDescPage21,
        Reloc::Aarch64TlsDescLd64Lo12 => Kind::TlsDescLd64Lo12,
        Reloc::Aarch64TlsDescAddLo12 => Kind::TlsDescAddLo12,
        Reloc::Aarch64TlsDescCall => Kind::TlsDescCall,
        other => panic!("a {other:?} relocation, which a record has no kind for"),
    }
}

fn uleb(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        out.push(if value == 0 { byte } else { byte | 0x80 });
        if value == 0 {
            return;
        }
    }
}

/// The CIE every function of the host's instruction set shares, as Cranelift's
/// `create_systemv_cie` has it: the alignments, the return address register, and the
/// initial instructions.
fn cie() -> (u32, i32, u16, &'static [u8]) {
    if cfg!(target_arch = "aarch64") {
        // `DW_CFA_def_cfa sp, 0`.
        (4, -8, 30, &[0x0c, 31, 0])
    } else {
        // `DW_CFA_def_cfa rsp, 8`; `DW_CFA_offset ra, cfa - 8`.
        (1, -8, 16, &[0x0c, 7, 8, 0x80 | 16, 1])
    }
}

/// The FDE instructions of a function's rows: the call frame information hir-ty-codegen
/// writes through gimli, by hand for the rows this file's functions have.
fn fde(rows: &[(u32, Rule)]) -> Vec<u8> {
    let (code_align, data_align, ..) = cie();
    let mut out = Vec::new();
    let mut at = 0;
    for (offset, rule) in rows {
        let delta = (offset - at) / code_align;
        at = *offset;
        match delta {
            0 => {}
            1..=0x3f => out.push(0x40 | delta as u8),
            0x40..=0xff => out.extend_from_slice(&[0x02, delta as u8]),
            _ => {
                out.push(0x03);
                out.extend_from_slice(&(delta as u16).to_le_bytes());
            }
        }
        match *rule {
            Rule::Cfa(register, offset) => {
                out.push(0x0c);
                uleb(&mut out, u64::from(register));
                uleb(&mut out, offset as u64);
            }
            Rule::CfaRegister(register) => {
                out.push(0x0d);
                uleb(&mut out, u64::from(register));
            }
            Rule::CfaOffset(offset) => {
                out.push(0x0e);
                uleb(&mut out, offset as u64);
            }
            Rule::Offset(register, offset) => {
                let factored = offset / data_align;
                assert!(factored >= 0 && offset % data_align == 0, "{rule:?}");
                match register {
                    0..64 => out.push(0x80 | register as u8),
                    // `DW_CFA_offset_extended`.
                    _ => {
                        out.push(0x05);
                        uleb(&mut out, u64::from(register));
                    }
                }
                uleb(&mut out, factored as u64);
            }
            Rule::Restore(register) => match register {
                0..64 => out.push(0xc0 | register as u8),
                // `DW_CFA_restore_extended`.
                _ => {
                    out.push(0x06);
                    uleb(&mut out, u64::from(register));
                }
            },
            Rule::SameValue(register) => {
                out.push(0x08);
                uleb(&mut out, u64::from(register));
            }
            Rule::RememberState => out.push(0x0a),
            Rule::RestoreState => out.push(0x0b),
            other => panic!("{other:?}: a row this file's functions do not have"),
        }
    }
    out
}

/// A function's code as the launcher takes it, with what its bind gives for each place.
struct Made {
    /// The record's bytes.
    bytes: Vec<u8>,
    id: RecordId,
    /// The body's placed length.
    len: u64,
    align: u32,
    refs: Vec<u32>,
}

/// Compiles `function` and makes its record: each relocation a fixup, each name a place.
fn made(function: &ir::Function, name: &str) -> Made {
    let code: Code = crate::compile(function, &target()).unwrap_or_else(|e| panic!("{name}: {e}"));
    let mut places: Vec<Place<'_>> = Vec::new();
    let mut numbers: Vec<(u32, u32)> = Vec::new();
    let mut refs = Vec::new();
    let mut fixups = Vec::new();
    for reloc in &code.relocs {
        let (to, addend) = match reloc.name {
            Name::Own(offset) => (To::Own, reloc.addend + i64::from(offset)),
            Name::User { namespace, index } => {
                let known = numbers.iter().position(|it| *it == (namespace, index));
                let place = known.unwrap_or_else(|| {
                    numbers.push((namespace, index));
                    places.push(match namespace {
                        FUNCS => Place::Function,
                        STATICS => Place::Static,
                        // The one outside symbol the program names.
                        _ => Place::Extern("labs"),
                    });
                    if namespace == FUNCS || namespace == STATICS {
                        refs.push(index);
                    }
                    places.len() - 1
                });
                (To::Place(place as u32), reloc.addend)
            }
            other => panic!("{name}: a relocation against {other:?}"),
        };
        fixups.push(Fixup { at: reloc.at, kind: kind(reloc.kind), to, addend });
    }
    let (code_align, data_align, return_address, initial) = cie();
    let rows = match &code.unwind {
        Some(crate::code::Unwind::Dwarf { rows, .. }) => fde(rows),
        other => panic!("{name}: unwind information {other:?} on a Unix host"),
    };
    let record = Record {
        arch: match target().arch {
            blaze_codegen::Arch::Aarch64 => wire::Arch::Aarch64,
            _ => wire::Arch::X86_64,
        },
        what: What::Code,
        align: code.align,
        size: code.bytes.len() as u64,
        bytes: &code.bytes,
        fixups: List::of(&fixups),
        places: List::of(&places),
        data: List::of(&[]),
        unwind: Some(Unwind {
            code_align,
            data_align,
            return_address,
            cie: initial,
            fde: &rows,
            personality: None,
            lsda: None,
        }),
        name,
    };
    let mut bytes = Vec::new();
    record.encode(&mut bytes);
    let len = blaze_launch::place::pool(&record, KEPT).expect("laid out").len;
    let id = RecordId(*blake3::hash(&bytes).as_bytes());
    Made { bytes, id, len, align: code.align, refs }
}

/// A static's record: 8 bytes of `value`.
fn static_record(value: u64, thread_local: bool, name: &str) -> (Vec<u8>, RecordId) {
    let value = value.to_le_bytes();
    let record = Record {
        arch: if cfg!(target_arch = "aarch64") { wire::Arch::Aarch64 } else { wire::Arch::X86_64 },
        what: What::Static { mutable: thread_local, thread_local },
        align: 8,
        size: 8,
        bytes: &value,
        fixups: List::of(&[]),
        places: List::of(&[]),
        data: List::of(&[]),
        unwind: None,
        name,
    };
    let mut bytes = Vec::new();
    record.encode(&mut bytes);
    let id = RecordId(*blake3::hash(&bytes).as_bytes());
    (bytes, id)
}

fn next_frame(from: &mut UnixStream) -> Option<Vec<u8>> {
    let mut len = [0; 4];
    from.read_exact(&mut len).ok()?;
    let mut item = vec![0; u32::from_be_bytes(len) as usize];
    from.read_exact(&mut item).ok()?;
    Some(item)
}

fn tell(to: &mut UnixStream, id: u64, tell: &Tell<'_>) {
    let mut frame = Vec::new();
    tell.encode(id, &mut frame).expect("a message that fits a frame");
    to.write_all(&frame).expect("written to the launcher");
}

/// What the test's daemon did, for the test to check.
#[derive(Default)]
struct Served {
    /// The slots the launcher asked for, in order: each when it was first called.
    needs: Vec<u32>,
    /// The edits the launcher said it applied.
    applied: Vec<u64>,
    failed: Vec<String>,
}

/// The daemon's side: answers `Hello` with the program's shape and its statics, compiles a
/// function when the launcher needs it (never before), and sends an edit of `leaf` each
/// time `leaf_k` is seen to have changed. Ends when the launcher closes the socket.
fn daemon(mut socket: UnixStream, leaf_k: Arc<AtomicI64>) -> Served {
    let mk = Mk::new(target());
    let mut served = Served::default();
    let mut txn = 0u64;
    let mut sent_k = leaf_k.load(Ordering::SeqCst);
    let mut leaf_bound = false;
    socket.set_read_timeout(Some(Duration::from_millis(5))).expect("a read timeout");
    let mut pending: Vec<u8> = Vec::new();
    loop {
        // An edit of `leaf`'s body, once it is bound: the changed function compiled and sent.
        let k = leaf_k.load(Ordering::SeqCst);
        if leaf_bound && k != sent_k {
            sent_k = k;
            txn += 1;
            let made = made(&function(&mk, LEAF, k), "leaf");
            tell(&mut socket, 100 + txn, &Tell::Record { id: made.id, bytes: &made.bytes });
            let binds = [wire::Bind {
                slot: Slot(LEAF),
                record: made.id,
                code: CodeId(made.id.0),
                tier: Tier::Zero,
                len: made.len,
                align: made.align,
                refs: List::of(&made.refs),
            }];
            let edit =
                Tell::Edit { txn: Txn(txn), binds: List::of(&binds), declare: 0, together: true };
            tell(&mut socket, 100 + txn, &edit);
        }
        // The launcher's next whole frame, if one is here.
        let mut chunk = [0u8; 4096];
        match socket.read(&mut chunk) {
            Ok(0) => return served,
            Ok(got) => pending.extend_from_slice(&chunk[..got]),
            Err(why)
                if matches!(
                    why.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => return served,
        }
        while let Ok(Some((item, took))) = wire::split(&pending) {
            let (_, ask) = Ask::decode(item).expect("a message of the launcher's");
            match ask {
                Ask::Hello { .. } => {
                    tell(&mut socket, 1, &Tell::Welcome { slots: SLOTS });
                    // The program's statics: 0 holds 1000; 1 is a thread-local of 42.
                    let (plain, plain_id) = static_record(1000, false, "STATIC0");
                    let (local, local_id) = static_record(42, true, "TLS1");
                    tell(&mut socket, 2, &Tell::Record { id: plain_id, bytes: &plain });
                    tell(&mut socket, 3, &Tell::Record { id: local_id, bytes: &local });
                    let defs = [
                        wire::StaticDef { number: 0, record: plain_id, refs: List::of(&[]) },
                        wire::StaticDef { number: 1, record: local_id, refs: List::of(&[]) },
                    ];
                    tell(&mut socket, 4, &Tell::Statics { defs: List::of(&defs) });
                    let start = Tell::Start { entry: Slot(TWICE), inits: List::of(&[0]) };
                    tell(&mut socket, 5, &start);
                }
                Ask::Need { slot, .. } => {
                    served.needs.push(slot.0);
                    // Compiled now, on its first call.
                    let k = leaf_k.load(Ordering::SeqCst);
                    let name = ["leaf", "twice", "mixed", "bump", "through"][slot.0 as usize];
                    let made = made(&function(&mk, slot.0, k), name);
                    if slot.0 == LEAF {
                        (leaf_bound, sent_k) = (true, k);
                    }
                    txn += 1;
                    tell(&mut socket, 100 + txn, &Tell::Record { id: made.id, bytes: &made.bytes });
                    let binds = [wire::Bind {
                        slot,
                        record: made.id,
                        code: CodeId(made.id.0),
                        tier: Tier::Zero,
                        len: made.len,
                        align: made.align,
                        refs: List::of(&made.refs),
                    }];
                    let edit = Tell::Edit {
                        txn: Txn(txn),
                        binds: List::of(&binds),
                        declare: 0,
                        together: false,
                    };
                    tell(&mut socket, 100 + txn, &edit);
                }
                Ask::Applied { txn, .. } => served.applied.push(txn.0),
                Ask::Failed { txn, why } => served.failed.push(format!("edit {}: {why}", txn.0)),
                other => panic!("the launcher said {other:?}"),
            }
            drop(pending.drain(..took));
        }
    }
}

/// What `through` is given to call: it panics, and the panic unwinds through `through`.
extern "C-unwind" fn boom(x: i64) -> i64 {
    std::panic::panic_any(x)
}

/// The invariant (launcher.md, "Step 3"): tier 0 code compiled under `Target::slots` runs
/// in the launcher with nothing linked. A function is compiled and placed when it is first
/// called, never before (the daemon's needs come in the order of the first calls); a call
/// reaches its callee through the slot word, a function's address is its thunk, a static and
/// an outside symbol are reached through the record's pool, each thread has its own copy of
/// a thread-local, and a panic unwinds through placed code. A body edit while a thread is
/// calling switches that thread to the new code at its next call, with no call ever seeing
/// the old code after the new, and the replaced body is freed once every thread reported.
#[test]
fn tier_zero_code_runs_in_the_launcher_compiled_on_first_call_and_edited_while_it_runs() {
    let (ours, theirs) = UnixStream::pair().expect("a socket pair");
    let leaf_k = Arc::new(AtomicI64::new(1));
    let daemon = std::thread::spawn({
        let leaf_k = Arc::clone(&leaf_k);
        move || daemon(theirs, leaf_k)
    });

    let page = blaze_launch_rt::sys::page().expect("a page size") as u64;
    let driver = Driver::new(Config { slots: 64, area: 256 * page }).expect("a driver");
    let shared = driver.shared();
    let (ready, entry) = crossbeam_channel::bounded(1);
    let control = std::thread::spawn(move || {
        let mut records = Loaded::new().expect("the program's data");
        let stay = |why: &str| format!("a test's launcher does not restart: {why}");
        let served =
            blaze_launch_rt::run(driver, ours, ProgramId([3; 32]), &mut records, ready, stay);
        (served, records)
    });
    let start = entry.recv_timeout(PATIENCE).expect("the daemon's word to start").expect("an entry");
    assert_eq!(Some(start.entry), shared.thunk(Slot(TWICE)), "the entry is the entry slot's thunk");
    // The one initializer the daemon named: static 0, at an address that holds its value.
    assert_eq!(start.inits.len(), 1);
    // SAFETY: the address of static 0, 8 bytes the launcher placed and never unmaps.
    assert_eq!(unsafe { (start.inits[0] as *const u64).read() }, 1000);

    let me = shared.birth().begin();
    let address = |slot: u32| shared.thunk(Slot(slot)).expect("a slot of the table");
    // SAFETY (each transmute below): the address is a slot's entry thunk, in a mapping that
    // is never unmapped; it leads to the slot's stub or to code the launcher placed for the
    // slot, a function of the host's C convention with this signature (`function`).
    let unary = |slot: u32| -> extern "C-unwind" fn(i64) -> i64 {
        unsafe { core::mem::transmute(address(slot)) }
    };
    let (twice, mixed) = (unary(TWICE), unary(MIXED));
    let bump: extern "C" fn() -> i64 = unsafe { core::mem::transmute(address(BUMP)) };
    let through: extern "C-unwind" fn(extern "C-unwind" fn(i64) -> i64, i64) -> i64 =
        unsafe { core::mem::transmute(address(THROUGH)) };

    // Nothing is compiled until it is called: `twice`, then `leaf` under it.
    assert_eq!(twice(5), 12);
    // A function's address (the thunk), a static (1000), an outside symbol (`labs`).
    assert_eq!(mixed(-3), (-3 + 1) + 1000 + 3);
    // A thread-local: this thread's copy counts up from 42, another thread's from 42 too.
    assert_eq!((bump(), bump()), (43, 44));
    let other = shared.birth();
    let theirs = std::thread::spawn(move || {
        let _me = other.begin();
        bump()
    });
    assert_eq!(theirs.join().expect("the other thread"), 43);
    assert_eq!(bump(), 45);
    // A panic passes through placed code: its unwind entry is the launcher's.
    let caught = std::panic::catch_unwind(|| through(boom, 7));
    assert_eq!(caught.unwrap_err().downcast_ref::<i64>(), Some(&7));
    assert_eq!(through(unary(LEAF), 7), 7 + 1 + 1);

    // A body edit while a thread calls: `leaf` adds 100 from now on.
    let stop = Arc::new(AtomicBool::new(false));
    let birth = shared.birth();
    let caller = std::thread::spawn({
        let stop = Arc::clone(&stop);
        move || -> Result<(u64, u64), String> {
            let me = birth.begin();
            // `twice` calls `leaf` twice: both old (12), the edit landing between them
            // (6 + 105, at most once), or both new (210); never old after new.
            let (mut old, mut straddled, mut new) = (0u64, 0u64, 0u64);
            while !stop.load(Ordering::Relaxed) || new < 3 {
                match twice(5) {
                    12 if straddled + new == 0 => old += 1,
                    111 if straddled + new == 0 => straddled += 1,
                    12 | 111 => return Err("the old body ran after the new one".to_owned()),
                    210 => new += 1,
                    other => return Err(format!("twice(5) was {other}")),
                }
                me.safepoint();
            }
            Ok((old, new))
        }
    });
    leaf_k.store(100, Ordering::SeqCst);
    let end = std::time::Instant::now() + PATIENCE;
    while unary(LEAF)(5) != 105 {
        assert!(std::time::Instant::now() < end, "the edit did not land within {PATIENCE:?}");
        std::thread::yield_now();
    }
    stop.store(true, Ordering::Relaxed);
    let (_, new) = caller.join().expect("the caller").expect("no call saw old after new");
    assert!(new >= 3);
    me.safepoint();
    drop(me);

    // Stopped, the control thread gives the driver back: the replaced body is freed, and
    // the daemon saw each function needed once, in the order of the first calls.
    shared.stop();
    let ((mut driver, served), mut records) = control.join().expect("the control thread");
    assert_eq!(served, Ok(()));
    driver.settle(&mut records).expect("settled");
    assert_eq!(driver.waiting(), 0, "the replaced body of `leaf` still waits");
    drop(driver);
    let served = daemon.join().expect("the daemon");
    assert_eq!(served.needs, [TWICE, LEAF, MIXED, BUMP, THROUGH]);
    assert_eq!(served.failed, [] as [String; 0]);
    assert_eq!(served.applied.len(), 6, "five first calls and one edit: {:?}", served.applied);
}
