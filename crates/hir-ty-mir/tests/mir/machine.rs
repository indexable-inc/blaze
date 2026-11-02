//! What the evaluator tells its `Machine`: a recorder keeps every hook it is called with, and
//! the initialized bytes and provenance of memory as the hooks say, so a pointer written and
//! read back keeps its provenance. Every program also runs with `Plain`, which must give the
//! same result and output.

use std::{collections::BTreeMap, ops::Range};

use hir_ty::next_solver::{Ty, TyKind};
use hir_ty_mir::mir::{
    Access, AtomicOrd, EvalThread, MResult, Machine, MemKind, Plain, Ptr, RetagMode, Shadow, Site,
    TypedCx, run_function,
};
use rustc_type_ir::inherent::IntoKind as _;
use hir_expand::fixture;

use crate::function;

const INIT: u8 = 1;
/// A byte of a pointer, which carries the pointer's provenance.
const POINTER: u8 = 2;
/// The first byte of a pointer.
const START: u8 = 4;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Byte {
    flags: u8,
    prov: Option<u32>,
}

/// Per byte: whether it is initialized, and the provenance of the pointer it is part of.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Bytes(Vec<Byte>);

impl Shadow<u32> for Bytes {
    fn init(len: usize) -> Bytes {
        Bytes(vec![Byte { flags: INIT, prov: None }; len])
    }

    fn pointer(size: usize, prov: Option<u32>) -> Bytes {
        let mut bytes = vec![Byte { flags: INIT | POINTER, prov }; size];
        if let Some(first) = bytes.first_mut() {
            first.flags |= START;
        }
        Bytes(bytes)
    }

    fn slice(&self, range: Range<usize>) -> Bytes {
        Bytes(range.map(|i| self.0.get(i).copied().unwrap_or_default()).collect())
    }

    fn put(&mut self, at: usize, other: &Bytes) {
        let end = at + other.0.len();
        if self.0.len() < end {
            self.0.resize(end, Byte::default());
        }
        self.0[at..end].copy_from_slice(&other.0);
    }

    fn prov_at(&self, at: usize, size: usize) -> Option<u32> {
        let (first, rest) = self.0.get(at..at + size)?.split_first()?;
        let inside = |it: &Byte| it.flags & (POINTER | START) == POINTER && it.prov == first.prov;
        let whole = first.flags & START != 0 && rest.iter().all(inside);
        let longer = self.0.get(at + size).is_some_and(inside);
        match whole && !longer {
            true => first.prov,
            false => None,
        }
    }
}

/// The type a typed copy is at, as far as these tests ask.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TyClass {
    Ref,
    RawPtr,
    Other,
}

impl TyClass {
    fn of(ty: Ty<'_>) -> TyClass {
        match ty.kind() {
            TyKind::Ref(..) => TyClass::Ref,
            TyKind::RawPtr(..) => TyClass::RawPtr,
            _ => TyClass::Other,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Event {
    Step,
    Alloc { addr: u64, size: u64, kind: MemKind, prov: u32 },
    Free { addr: u64, prov: Option<u32>, kind: MemKind },
    Read { addr: u64, prov: Option<u32>, size: u64, align: u64, access: Access },
    Write { addr: u64, prov: Option<u32>, size: u64, align: u64, access: Access },
    Copy { dst: u64, dst_prov: Option<u32>, src: u64, src_prov: Option<u32>, size: u64 },
    RequireInit { len: usize, init: bool },
    AtomicLoad { addr: u64, prov: Option<u32>, size: u64, ord: AtomicOrd },
    AtomicStore { addr: u64, prov: Option<u32>, ord: AtomicOrd },
    Fence { ord: AtomicOrd, compiler_only: bool },
    /// `value`: the first 8 bytes of the value, little-endian (a pointer's address).
    Typed { addr: u64, prov: Option<u32>, mode: RetagMode, ty: TyClass, value: u64 },
    Offset { addr: u64, prov: Option<u32>, by: i64 },
    Expose { addr: u64, prov: Option<u32> },
    FromExposed { addr: u64, prov: Option<u32> },
    Freeze { addr: u64 },
    Call,
    Protect { addr: u64, prov: Option<u32>, size: u64 },
    Ret,
    Spawn { thread: EvalThread },
    Schedule { running: EvalThread, chosen: EvalThread },
    Join { thread: EvalThread },
    Detach { thread: EvalThread },
    Exit,
    Yield,
    ThreadName { thread: EvalThread },
    Release { object: u64 },
    Acquire { object: u64 },
    Finish,
}

impl Event {
    /// What the evaluator touches of program memory, as opposed to allocating it, switching
    /// frames or threads.
    fn is_memory(&self) -> bool {
        matches!(
            self,
            Event::Read { .. }
                | Event::Write { .. }
                | Event::Copy { .. }
                | Event::RequireInit { .. }
                | Event::AtomicLoad { .. }
                | Event::AtomicStore { .. }
                | Event::Typed { .. }
                | Event::Offset { .. }
                | Event::Expose { .. }
                | Event::FromExposed { .. }
        )
    }

    /// The bytes this reads, `(addr, prov, size)`: a `Read`, or the source of a `Copy` (a
    /// place moved or copied as a whole).
    fn read(&self) -> Option<(u64, Option<u32>, u64)> {
        match *self {
            Event::Read { addr, prov, size, .. } => Some((addr, prov, size)),
            Event::Copy { src, src_prov, size, .. } => Some((src, src_prov, size)),
            _ => None,
        }
    }
}

struct Allocation {
    size: u64,
    prov: u32,
    bytes: Bytes,
}

/// Provenance is a counter `alloc` hands out, from 1.
#[derive(Default)]
struct Recorder {
    events: Vec<Event>,
    /// By start address; zero-sized allocations aren't kept, as they hold no bytes.
    memory: BTreeMap<u64, Allocation>,
    next: u32,
    exposed: Vec<u32>,
}

impl Recorder {
    fn containing(&self, addr: u64) -> Option<(u64, &Allocation)> {
        let (&start, allocation) = self.memory.range(..=addr).next_back()?;
        (addr < start + allocation.size).then_some((start, allocation))
    }

    fn load(&self, addr: u64, size: u64) -> Bytes {
        match self.containing(addr) {
            Some((start, allocation)) => {
                let at = (addr - start) as usize;
                allocation.bytes.slice(at..at + size as usize)
            }
            None => Bytes::init(size as usize),
        }
    }

    fn store(&mut self, addr: u64, shadow: &Bytes) {
        let Some((&start, allocation)) = self.memory.range_mut(..=addr).next_back() else {
            return;
        };
        if addr < start + allocation.size {
            allocation.bytes.put((addr - start) as usize, shadow);
            allocation.bytes.0.truncate(allocation.size as usize);
        }
    }
}

fn le_u64(bytes: &[u8]) -> u64 {
    let mut word = [0; 8];
    let n = bytes.len().min(8);
    word[..n].copy_from_slice(&bytes[..n]);
    u64::from_le_bytes(word)
}

impl<'db> Machine<'db> for Recorder {
    type Prov = u32;
    type Shadow = Bytes;

    fn step(&mut self, _: Site<'db>) {
        self.events.push(Event::Step);
    }

    fn alloc(&mut self, addr: u64, size: u64, _: u64, kind: MemKind, zeroed: bool) -> MResult<u32> {
        self.next += 1;
        let prov = self.next;
        self.events.push(Event::Alloc { addr, size, kind, prov });
        if size != 0 {
            let bytes = match zeroed {
                true => Bytes::init(size as usize),
                false => Bytes(vec![Byte::default(); size as usize]),
            };
            self.memory.insert(addr, Allocation { size, prov, bytes });
        }
        Ok(prov)
    }

    fn free(
        &mut self,
        ptr: Ptr<u32>,
        _: Option<u64>,
        _: Option<u64>,
        kind: MemKind,
    ) -> MResult<()> {
        self.events.push(Event::Free { addr: ptr.addr, prov: ptr.prov, kind });
        self.memory.remove(&ptr.addr);
        Ok(())
    }

    fn root(&mut self, addr: u64) -> Option<u32> {
        self.containing(addr).map(|(_, allocation)| allocation.prov)
    }

    fn read(&mut self, ptr: Ptr<u32>, size: u64, align: u64, access: Access) -> MResult<Bytes> {
        self.events.push(Event::Read { addr: ptr.addr, prov: ptr.prov, size, align, access });
        Ok(self.load(ptr.addr, size))
    }

    fn write(
        &mut self,
        ptr: Ptr<u32>,
        size: u64,
        align: u64,
        access: Access,
        shadow: &Bytes,
    ) -> MResult<()> {
        self.events.push(Event::Write { addr: ptr.addr, prov: ptr.prov, size, align, access });
        self.store(ptr.addr, &shadow.slice(0..size as usize));
        Ok(())
    }

    fn copy(&mut self, dst: Ptr<u32>, src: Ptr<u32>, size: u64, _: u64, _: bool) -> MResult<()> {
        self.events.push(Event::Copy {
            dst: dst.addr,
            dst_prov: dst.prov,
            src: src.addr,
            src_prov: src.prov,
            size,
        });
        let shadow = self.load(src.addr, size);
        self.store(dst.addr, &shadow);
        Ok(())
    }

    fn require_init(&mut self, shadow: &Bytes, range: Range<usize>) -> MResult<()> {
        let len = range.len();
        let init = shadow.slice(range).0.iter().all(|it| it.flags & INIT != 0);
        self.events.push(Event::RequireInit { len, init });
        Ok(())
    }

    fn atomic_load(
        &mut self,
        ptr: Ptr<u32>,
        size: u64,
        ord: AtomicOrd,
        _: &[u8],
    ) -> MResult<Option<Vec<u8>>> {
        self.events.push(Event::AtomicLoad { addr: ptr.addr, prov: ptr.prov, size, ord });
        Ok(None)
    }

    fn atomic_store(&mut self, ptr: Ptr<u32>, ord: AtomicOrd, _: &[u8], _: &[u8]) -> MResult<()> {
        self.events.push(Event::AtomicStore { addr: ptr.addr, prov: ptr.prov, ord });
        Ok(())
    }

    fn fence(&mut self, ord: AtomicOrd, compiler_only: bool) -> MResult<()> {
        self.events.push(Event::Fence { ord, compiler_only });
        Ok(())
    }

    fn offset_from(&mut self, _: Ptr<u32>, _: Ptr<u32>, _: bool) -> MResult<()> {
        Ok(())
    }

    fn typed(
        &mut self,
        _: &TypedCx<'_, 'db>,
        place: Ptr<u32>,
        ty: Ty<'db>,
        bytes: &[u8],
        mode: RetagMode,
    ) -> MResult<()> {
        self.events.push(Event::Typed {
            addr: place.addr,
            prov: place.prov,
            mode,
            ty: TyClass::of(ty),
            value: le_u64(bytes),
        });
        Ok(())
    }

    fn offset(&mut self, ptr: Ptr<u32>, by: i64) -> MResult<()> {
        self.events.push(Event::Offset { addr: ptr.addr, prov: ptr.prov, by });
        Ok(())
    }

    fn expose(&mut self, ptr: Ptr<u32>) -> MResult<()> {
        self.events.push(Event::Expose { addr: ptr.addr, prov: ptr.prov });
        if let Some(prov) = ptr.prov {
            self.exposed.push(prov);
        }
        Ok(())
    }

    fn from_exposed(&mut self, addr: u64) -> Option<u32> {
        let prov = self.root(addr).filter(|it| self.exposed.contains(it));
        self.events.push(Event::FromExposed { addr, prov });
        prov
    }

    fn freeze(&mut self, addr: u64) {
        self.events.push(Event::Freeze { addr });
    }

    fn initialized(&mut self, _: u64) {}

    fn call(&mut self) -> MResult<()> {
        self.events.push(Event::Call);
        Ok(())
    }

    fn protect(&mut self, place: Ptr<u32>, size: u64) -> MResult<()> {
        self.events.push(Event::Protect { addr: place.addr, prov: place.prov, size });
        Ok(())
    }

    fn ret(&mut self) -> MResult<()> {
        self.events.push(Event::Ret);
        Ok(())
    }

    fn spawn(&mut self, thread: EvalThread) -> MResult<()> {
        self.events.push(Event::Spawn { thread });
        Ok(())
    }

    fn schedule(&mut self, running: EvalThread, runnable: &[EvalThread]) -> MResult<EvalThread> {
        let chosen = <Plain as Machine<'db>>::schedule(&mut Plain, running, runnable)?;
        self.events.push(Event::Schedule { running, chosen });
        Ok(chosen)
    }

    fn join(&mut self, thread: EvalThread) -> MResult<()> {
        self.events.push(Event::Join { thread });
        Ok(())
    }

    fn detach(&mut self, thread: EvalThread) -> MResult<()> {
        self.events.push(Event::Detach { thread });
        Ok(())
    }

    fn exit(&mut self) -> MResult<()> {
        self.events.push(Event::Exit);
        Ok(())
    }

    fn yield_now(&mut self) {
        self.events.push(Event::Yield);
    }

    fn thread_name(&mut self, thread: EvalThread, _: &[u8]) {
        self.events.push(Event::ThreadName { thread });
    }

    fn release(&mut self, object: u64) -> MResult<()> {
        self.events.push(Event::Release { object });
        Ok(())
    }

    fn acquire(&mut self, object: u64) -> MResult<()> {
        self.events.push(Event::Acquire { object });
        Ok(())
    }

    fn finish(&mut self) -> MResult<()> {
        self.events.push(Event::Finish);
        Ok(())
    }
}

fn fixture(program: &str) -> String {
    format!("//- minicore: sized, copy, index, slice\n{program}")
}

struct Run {
    events: Vec<Event>,
    stdout: String,
    ok: bool,
}

/// Runs `name` with `Plain` and with the recorder, which must agree on the result and the
/// output.
fn run(program: &str, name: &str) -> Run {
    let fx = fixture::db(&fixture(program));
    let _ = fx.only_file();
    let db = fx.into_db();
    hir_ty::attach_db(&db, || {
        let func = function(&db, name);
        let (plain, plain_out, _) = run_function(&db, func, Plain);
        let (recorded, recorded_out, recorder) = run_function(&db, func, Recorder::default());
        assert_eq!(plain, recorded);
        assert_eq!(plain_out.stdout(), recorded_out.stdout());
        assert_eq!(plain_out.stderr(), recorded_out.stderr());
        Run { events: recorder.events, stdout: plain_out.stdout().into_owned(), ok: plain.is_ok() }
    })
}

/// The events of `main`, which must run to its end.
fn record(program: &str) -> Vec<Event> {
    let run = run(program, "main");
    assert!(run.ok, "the program failed: {:#?}", run.events);
    run.events
}

/// A pointer an assignment made: the index of its `Typed`, the place it went to, and the
/// address it points to.
struct Made {
    at: usize,
    place: u64,
    to: u64,
}

/// Every assignment of a pointer of type `ty` retagged by `mode`, in order.
fn all_made(events: &[Event], ty: TyClass, mode: RetagMode) -> Vec<Made> {
    events
        .iter()
        .enumerate()
        .filter_map(|(at, event)| match *event {
            Event::Typed { addr, mode: m, ty: t, value, .. } if t == ty && m == mode => {
                Some(Made { at, place: addr, to: value })
            }
            _ => None,
        })
        .collect()
}

/// The first assignment of a pointer of type `ty` retagged by `mode`.
#[track_caller]
fn made(events: &[Event], ty: TyClass, mode: RetagMode) -> Made {
    all_made(events, ty, mode)
        .into_iter()
        .next()
        .unwrap_or_else(|| panic!("no `Typed` of a {ty:?} by {mode:?}: {events:#?}"))
}

/// The first assignment of a non-pointer `value` (as `Typed` records it): its index and place.
#[track_caller]
fn assigned(events: &[Event], value: u64) -> Made {
    events
        .iter()
        .enumerate()
        .find_map(|(at, event)| match *event {
            Event::Typed { addr, ty: TyClass::Other, value: v, .. } if v == value => {
                Some(Made { at, place: addr, to: value })
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("no assignment of {value}: {events:#?}"))
}

/// The latest stack allocation before event `before` that `addr` is in: its index, start and
/// root provenance.
#[track_caller]
fn stack_root(events: &[Event], before: usize, addr: u64) -> (usize, u64, u32) {
    events[..before]
        .iter()
        .enumerate()
        .rev()
        .find_map(|(i, event)| match *event {
            Event::Alloc { addr: start, size, kind: MemKind::Stack, prov }
                if start <= addr && addr < start + size =>
            {
                Some((i, start, prov))
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("no stack allocation holds {addr:#x}: {events:#?}"))
}

/// Whether some event from `from` reads `size` bytes at `addr` with provenance `prov`.
fn reads(events: &[Event], from: usize, addr: u64, prov: u32, size: u64) -> bool {
    events[from..].iter().filter_map(Event::read).any(|it| it == (addr, Some(prov), size))
}

#[test]
fn a_read_through_a_reference_carries_the_root_provenance() {
    let events = record("fn main() { let x = 5u32; let r = &x; let _y = *r; }");
    let borrow = made(&events, TyClass::Ref, RetagMode::Default);
    let (alloc, _, x) = stack_root(&events, borrow.at, borrow.to);
    assert!(
        events[..alloc].contains(&Event::Step),
        "`x` is allocated by its StorageLive: {events:#?}"
    );
    // The dereference loads `r`, whose bytes kept `x`'s provenance in the shadow.
    let load = events[borrow.at..]
        .iter()
        .position(|it| matches!(*it, Event::Read { addr, size: 8, .. } if addr == borrow.place))
        .map(|it| borrow.at + it)
        .unwrap_or_else(|| panic!("`*r` doesn't load `r`: {events:#?}"));
    assert!(
        reads(&events, load, borrow.to, x, 4),
        "`*r` doesn't read `x` with its root: {events:#?}"
    );
}

#[test]
fn casts_between_pointers_and_integers_expose_and_take_provenance() {
    let events = record(
        r#"
fn main() {
    let x = 5u32;
    let r = &x;
    let a = r as *const u32 as usize;
    let p = a as *const u32;
    let _y = unsafe { *p };
}
"#,
    );
    let borrow = made(&events, TyClass::Ref, RetagMode::Default);
    let (_, _, x) = stack_root(&events, borrow.at, borrow.to);
    let expose = events
        .iter()
        .position(|it| *it == Event::Expose { addr: borrow.to, prov: Some(x) })
        .unwrap_or_else(|| panic!("no `Expose` of `x`: {events:#?}"));
    let from = events[expose..]
        .iter()
        .position(|it| *it == Event::FromExposed { addr: borrow.to, prov: Some(x) })
        .map(|it| expose + it)
        .unwrap_or_else(|| panic!("no `FromExposed` of `x`'s address: {events:#?}"));
    assert!(
        reads(&events, from, borrow.to, x, 4),
        "`*p` doesn't read `x` with its root: {events:#?}"
    );
}

#[test]
fn an_atomic_rmw_is_a_read_a_write_and_a_store_in_a_row() {
    let events = record(
        r#"
#[rustc_intrinsic]
unsafe fn atomic_xadd_seqcst<T>(dst: *mut T, src: T) -> T;

fn main() {
    let mut x = 1u32;
    let p = &raw mut x;
    let _old = unsafe { atomic_xadd_seqcst(p, 2) };
}
"#,
    );
    let raw = made(&events, TyClass::RawPtr, RetagMode::Raw);
    let (_, _, x) = stack_root(&events, raw.at, raw.to);
    let rmw = Access::Atomic { ord: AtomicOrd::SeqCst, rmw: true, cxchg_failed: false };
    let read = events
        .iter()
        .position(|it| matches!(*it, Event::Read { access, .. } if access == rmw))
        .unwrap_or_else(|| panic!("no atomic RMW read: {events:#?}"));
    let addr = raw.to;
    assert!(
        matches!(
            events[read],
            Event::Read { addr: a, prov: Some(p), size: 4, .. } if a == addr && p == x
        ),
        "{events:#?}"
    );
    // The value read is used as a number: its `require_init` comes between.
    let mut after = events[read + 1..].iter().filter(|it| !matches!(it, Event::RequireInit { .. }));
    assert!(
        matches!(
            after.next(),
            Some(&Event::Write { addr: a, prov: Some(p), size: 4, access, .. })
                if a == addr && p == x && access == rmw
        ),
        "the RMW's write doesn't follow its read: {events:#?}"
    );
    assert!(
        matches!(
            after.next(),
            Some(&Event::AtomicStore { addr: a, prov: Some(p), ord: AtomicOrd::SeqCst })
                if a == addr && p == x
        ),
        "the RMW's `atomic_store` doesn't follow its write: {events:#?}"
    );
}

const CXCHG: &str = r#"
#[rustc_intrinsic]
unsafe fn atomic_cxchg_seqcst_acquire<T>(dst: *mut T, old: T, src: T) -> (T, bool);
"#;

/// The atomic accesses of `x`'s address in a program whose `main` makes `p = &raw mut x`.
fn atomics_of(events: &[Event]) -> Vec<Event> {
    let raw = made(events, TyClass::RawPtr, RetagMode::Raw);
    events
        .iter()
        .filter(|it| match **it {
            Event::Read { addr, access: Access::Atomic { .. }, .. }
            | Event::Write { addr, access: Access::Atomic { .. }, .. }
            | Event::AtomicStore { addr, .. } => addr == raw.to,
            _ => false,
        })
        .copied()
        .collect()
}

/// Miri's `atomic_compare_exchange_scalar`: a failed compare-exchange is one load at the failure
/// ordering, of the latest store, and writes nothing. The race checker's `failed_cxchg` sees it
/// alone, not after a read of the read-modify-write.
#[test]
fn a_failed_compare_exchange_is_one_read_at_the_failure_ordering() {
    let events = record(&format!(
        "{CXCHG}
fn main() {{
    let mut x = 1u32;
    let p = &raw mut x;
    let _ = unsafe {{ atomic_cxchg_seqcst_acquire(p, 5, 2) }};
}}
"
    ));
    let failed = Access::Atomic { ord: AtomicOrd::Acquire, rmw: true, cxchg_failed: true };
    let atomics = atomics_of(&events);
    assert!(
        matches!(atomics[..], [Event::Read { size: 4, access, .. }] if access == failed),
        "{atomics:#?}"
    );
}

/// A compare-exchange that exchanges is a read-modify-write at its success ordering: one read,
/// then the write and its `atomic_store`.
#[test]
fn a_compare_exchange_that_exchanges_is_one_rmw() {
    let events = record(&format!(
        "{CXCHG}
fn main() {{
    let mut x = 1u32;
    let p = &raw mut x;
    let _ = unsafe {{ atomic_cxchg_seqcst_acquire(p, 1, 2) }};
}}
"
    ));
    let rmw = Access::Atomic { ord: AtomicOrd::SeqCst, rmw: true, cxchg_failed: false };
    let atomics = atomics_of(&events);
    assert!(
        matches!(
            atomics[..],
            [
                Event::Read { size: 4, access: read, .. },
                Event::Write { size: 4, access: write, .. },
                Event::AtomicStore { ord: AtomicOrd::SeqCst, .. },
            ] if read == rmw && write == rmw
        ),
        "{atomics:#?}"
    );
}

#[test]
fn references_are_retagged_by_how_they_are_made() {
    let events = record(
        r#"
struct S {
    v: u32,
}

impl S {
    fn bump(&mut self) {
        self.v += 1;
    }
}

fn main() {
    let x = 5u32;
    let _r = &x;
    let _p = &raw const x;
    let mut s = S { v: 0 };
    s.bump();
}
"#,
    );
    let shared = made(&events, TyClass::Ref, RetagMode::Default);
    let raw = made(&events, TyClass::RawPtr, RetagMode::Raw);
    assert_eq!(shared.to, raw.to, "`&x` and `&raw const x` point to different places");
    let two_phase = made(&events, TyClass::Ref, RetagMode::TwoPhase);
    assert!(raw.at < two_phase.at, "{events:#?}");
}

#[test]
fn storage_dead_frees_a_block_scoped_local() {
    let events = record(
        r#"
fn main() {
    let a = 1u32;
    {
        let b = 2u32;
        let _c = &b;
    }
    let _d = a + 40;
}
"#,
    );
    let borrow = made(&events, TyClass::Ref, RetagMode::Default);
    let (alloc, start, b) = stack_root(&events, borrow.at, borrow.to);
    assert!(
        events[..alloc].contains(&Event::Step),
        "`b` is allocated by its StorageLive: {events:#?}"
    );
    let free = events[borrow.at..]
        .iter()
        .position(|it| *it == Event::Free { addr: start, prov: Some(b), kind: MemKind::Stack })
        .map(|it| borrow.at + it)
        .unwrap_or_else(|| panic!("`b` is never freed: {events:#?}"));
    let sum = events
        .iter()
        .position(|it| matches!(*it, Event::Typed { value, .. } if value as u32 == 41))
        .unwrap_or_else(|| panic!("no assignment of `a + 40`: {events:#?}"));
    assert!(free < sum, "`b` is freed at the frame's end, not at its StorageDead: {events:#?}");
}

/// rustc's interning (`intern_const_alloc_recursive`): a constant's value and what it points
/// to are read-only, so Miri reports a write through `R as *const u32 as *mut u32`.
#[test]
fn a_constant_and_what_it_points_to_are_frozen() {
    let events = record(
        r#"
const R: &u32 = &7;
fn main() {
    let r = R;
    let _x = *r;
}
"#,
    );
    let copied = made(&events, TyClass::Ref, RetagMode::Default);
    let pointee = events[..copied.at].iter().position(|it| *it == Event::Freeze { addr: copied.to });
    assert!(pointee.is_some(), "`&7` is not frozen before `R` is used: {events:#?}");
    let frozen = events.iter().filter(|it| matches!(it, Event::Freeze { .. })).count();
    assert!(frozen >= 2, "`R`'s own copy is not frozen: {events:#?}");
}

/// A static is read-only unless it is `static mut` (or not `Freeze`), as rustc interns it.
#[test]
fn a_static_is_frozen_and_a_static_mut_is_not() {
    let events = record(
        r#"
static A: u32 = 1;
static mut B: u32 = 2;
fn main() {
    let a = &A;
    let b = unsafe { &raw const B };
    let _x = *a;
    let _y = unsafe { *b };
}
"#,
    );
    let a = made(&events, TyClass::Ref, RetagMode::Default);
    let b = made(&events, TyClass::RawPtr, RetagMode::Raw);
    assert!(events.contains(&Event::Freeze { addr: a.to }), "`A` is writable: {events:#?}");
    assert!(!events.contains(&Event::Freeze { addr: b.to }), "`B` is read-only: {events:#?}");
}

#[test]
fn a_step_comes_before_each_statement_s_memory_events() {
    let events = record("fn main() { let x = 5u32; let r = &x; let _y = *r; }");
    let first_step = events.iter().position(|it| *it == Event::Step).expect("no step");
    let first_memory = events.iter().position(Event::is_memory).expect("no memory event");
    assert!(first_step < first_memory, "{events:#?}");
    // One assignment is one statement: its `Typed` ends its statement's memory events.
    for statement in events.split(|it| *it == Event::Step) {
        let typed: Vec<usize> = statement
            .iter()
            .enumerate()
            .filter(|(_, it)| matches!(it, Event::Typed { .. }))
            .map(|(i, _)| i)
            .collect();
        assert!(typed.len() <= 1, "two typed copies without a step between: {statement:#?}");
        if let Some(&typed) = typed.first() {
            assert!(
                !statement[typed + 1..].iter().any(|it| it.read().is_some()
                    || matches!(it, Event::Write { .. })),
                "memory events after an assignment's `Typed` before the next step: {statement:#?}"
            );
        }
    }
}

#[test]
fn plain_and_the_recorder_agree_on_output() {
    let run = run(
        r#"
extern "C" {
    fn write(fd: i32, buf: *const u8, count: usize) -> isize;
}

fn main() {
    let buf = [b'o', b'k', b'\n'];
    let r = &buf;
    unsafe { write(1, r as *const [u8; 3] as *const u8, 3) };
}
"#,
        "main",
    );
    assert!(run.ok, "{:#?}", run.events);
    assert_eq!(run.stdout, "ok\n");
}

#[test]
fn plain_and_the_recorder_agree_on_a_failure() {
    let run = run(
        r#"
fn divide(a: u32, b: u32) -> u32 {
    a / b
}

fn main() {
    let _q = divide(1, 0);
}
"#,
        "main",
    );
    assert!(!run.ok, "dividing by zero passed: {:#?}", run.events);
}

// --- The SIMD shim ---

const SIMD: &str = r#"
#[repr(simd)]
#[derive(Clone, Copy)]
struct P2([*const u8; 2]);
#[repr(simd)]
#[derive(Clone, Copy)]
struct U2([usize; 2]);
#[repr(simd)]
#[derive(Clone, Copy)]
struct I2([i64; 2]);
#[repr(simd)]
#[derive(Clone, Copy)]
struct B2([u8; 2]);
#[repr(simd)]
#[derive(Clone, Copy)]
struct M2([i8; 2]);

#[rustc_intrinsic]
unsafe fn simd_shuffle<T, U, V>(a: T, b: T, idx: U) -> V;
#[rustc_intrinsic]
unsafe fn simd_gather<T, U, V>(val: T, ptr: U, mask: V) -> T;
#[rustc_intrinsic]
unsafe fn simd_expose_provenance<T, U>(ptr: T) -> U;
#[rustc_intrinsic]
unsafe fn simd_with_exposed_provenance<T, U>(addr: T) -> U;
#[rustc_intrinsic]
unsafe fn simd_add<T>(a: T, b: T) -> T;
#[rustc_intrinsic]
unsafe fn arith_offset<T>(ptr: *const T, count: isize) -> *const T;
"#;

fn simd(body: &str) -> String {
    format!("{SIMD}\nfn main() {{\n{body}\n}}\n")
}

#[test]
fn shuffled_pointers_keep_their_provenance() {
    let events = record(&simd(
        r#"
    let a = 1u8;
    let b = 2u8;
    let v = P2([&a, &b]);
    let s: P2 = unsafe { simd_shuffle(v, v, [1u32, 0]) };
    let _x = unsafe { *s.0[0] };
"#,
    ));
    let init = assigned(&events, 2);
    let (_, _, b) = stack_root(&events, init.at, init.place);
    assert!(
        reads(&events, init.at, init.place, b, 1),
        "lane 0 of the shuffle doesn't read `b` with its root: {events:#?}"
    );
}

#[test]
fn a_gather_reads_through_its_lane_s_provenance() {
    let run = run(
        &simd(
            r#"
    let v = [0u8; 4];
    let p = unsafe { arith_offset(&v as *const [u8; 4] as *const u8, 5) };
    let _g: B2 = unsafe { simd_gather(B2([0, 0]), P2([p, p]), M2([-1, 0])) };
"#,
        ),
        "main",
    );
    let events = &run.events;
    let borrow = made(events, TyClass::Ref, RetagMode::Default);
    let (_, start, v) = stack_root(events, borrow.at, borrow.to);
    let lane = start + 5;
    let gathered: Vec<_> = events[borrow.at..]
        .iter()
        .filter_map(Event::read)
        .filter(|&(addr, _, size)| addr == lane && size == 1)
        .collect();
    assert_eq!(gathered, [(lane, Some(v), 1)], "only the enabled lane is read: {events:#?}");
}

#[test]
fn an_address_never_exposed_has_no_provenance() {
    let run = run(
        &simd(
            r#"
    let p: P2 = unsafe { simd_with_exposed_provenance(U2([64, 64])) };
    let _x = unsafe { *p.0[0] };
"#,
        ),
        "main",
    );
    assert!(!run.ok, "a read of address 64 passed: {:#?}", run.events);
    let lanes = run.events.iter().filter(|it| **it == Event::FromExposed { addr: 64, prov: None });
    assert_eq!(lanes.count(), 2, "{:#?}", run.events);
}

#[test]
fn exposed_lanes_get_their_provenance_back() {
    let events = record(&simd(
        r#"
    let a = 1u8;
    let u: U2 = unsafe { simd_expose_provenance(P2([&a, &a])) };
    let p: P2 = unsafe { simd_with_exposed_provenance(u) };
    let _x = unsafe { *p.0[0] };
"#,
    ));
    let init = assigned(&events, 1);
    let (_, _, a) = stack_root(&events, init.at, init.place);
    let exposed = Event::Expose { addr: init.place, prov: Some(a) };
    assert_eq!(events.iter().filter(|it| **it == exposed).count(), 2, "{events:#?}");
    let back = Event::FromExposed { addr: init.place, prov: Some(a) };
    let from = events
        .iter()
        .position(|it| *it == back)
        .unwrap_or_else(|| panic!("no `FromExposed` of `a`'s address: {events:#?}"));
    assert!(reads(&events, from, init.place, a, 1), "`*p.0[0]` doesn't read `a`: {events:#?}");
}

#[test]
fn adding_an_uninitialized_lane_requires_it_initialized() {
    let events = record(&simd(
        r#"
    union U {
        v: I2,
        b: (),
    }
    let u = U { b: () };
    let x = unsafe { u.v };
    let _s = unsafe { simd_add(x, I2([1, 1])) };
"#,
    ));
    assert!(
        events.iter().any(|it| matches!(it, Event::RequireInit { init: false, .. })),
        "the lanes of `u.v` are used as numbers without `require_init` failing: {events:#?}"
    );
}
