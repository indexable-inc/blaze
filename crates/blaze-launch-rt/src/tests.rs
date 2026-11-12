//! The drivers against real memory and real threads: bodies of hand-assembled code placed in
//! a mapped area, called through their slot words, replaced while another thread calls them,
//! and freed once every thread reported; a gate's entry run before and after its flag; a
//! stub that fires and is served over a socket by a stand-in for the daemon.
//!
//! What these can show is that the machine code, the mappings and the stores are right and
//! that a run sees only what the design allows. That no *schedule* breaks the order of the
//! stores is the simulation's to show (`blaze-launch/src/tests/sim.rs`), whose machine this
//! crate implements.

use std::collections::BTreeMap;
use std::io::{Read, Write as _};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use blaze_launch::wire::{self, Ask, List, ProgramId, RecordId, Tell};
use blaze_launch::{CodeId, Frame, FrameKind, Slot, Target, Thread, Tier, Txn};

use crate::sys::{self, Code};
use crate::{Config, Driver, EditError, Error, Place, Records, Shared, arch};

/// How long a test waits for another thread before it fails, naming what it waited for.
const PATIENCE: Duration = Duration::from_secs(30);

/// The code of `extern "C" fn() -> u32 { n }`: `mov w0, #n; ret`.
#[cfg(target_arch = "aarch64")]
fn returns(n: u16) -> Vec<u8> {
    let mov = 0x5280_0000u32 | (u32::from(n) << 5);
    let ret = 0xd65f_03c0u32;
    [mov.to_le_bytes(), ret.to_le_bytes()].concat()
}

/// The code of `extern "C" fn() -> u32 { n }`: `mov eax, n; ret`.
#[cfg(target_arch = "x86_64")]
fn returns(n: u16) -> Vec<u8> {
    let mut code = vec![0xb8];
    code.extend_from_slice(&u32::from(n).to_le_bytes());
    code.push(0xc3);
    code
}

/// Calls the function at `at`.
fn call_at(at: usize) -> u32 {
    // SAFETY: every caller passes the address of code this file placed, or of a stub or a
    // gate entry that leads to such code: a function `extern "C" fn() -> u32`, in a mapping
    // that is never unmapped (`sys::Code`), whose body the driver gives back to the system
    // only once no thread can be in it (what these tests are about).
    let function: extern "C" fn() -> u32 = unsafe { core::mem::transmute(at) };
    function()
}

/// Calls `slot` as blaze's code does: load the word, jump.
fn call(shared: &Shared, slot: u32) -> u32 {
    let to = shared.target(Slot(slot)).expect("a slot of the table");
    assert_ne!(to, 0, "slot {slot} was called before it was declared");
    call_at(to)
}

/// The records of a test: code by record id.
#[derive(Default)]
struct Table {
    code: BTreeMap<[u8; 32], Vec<u8>>,
}

impl Table {
    /// Adds `code` as a record, and gives the bind of `slot` to it.
    fn bind(&mut self, slot: u32, code: Vec<u8>, align: u32) -> wire::Bind<'static> {
        let id = [self.code.len() as u8 + 1; 32];
        let len = code.len() as u64;
        self.code.insert(id, code);
        wire::Bind {
            slot: Slot(slot),
            record: RecordId(id),
            code: CodeId(id),
            tier: Tier::Zero,
            len,
            align,
            refs: List::of(&[]),
        }
    }
}

impl Records for Table {
    fn record(&mut self, id: RecordId, bytes: &[u8]) -> Result<(), String> {
        self.code.insert(id.0, bytes.to_vec());
        Ok(())
    }

    fn statics(&mut self, _defs: &[wire::StaticDef<'_>], _place: Place) -> Result<(), String> {
        Err("a table of code has no statics".to_owned())
    }

    fn placed(&mut self, _at: u64, _len: u64) -> Result<(), String> {
        Ok(())
    }

    fn forget(&mut self, _at: u64) {}

    fn release(&mut self) {}

    fn static_at(&self, _number: u32) -> Option<usize> {
        None
    }

    fn code(
        &mut self,
        bind: &wire::Bind<'_>,
        _place: Place,
        into: &mut Vec<u8>,
    ) -> Result<(), String> {
        into.clear();
        match self.code.get(&bind.record.0) {
            Some(code) => {
                into.extend_from_slice(code);
                Ok(())
            }
            None => Err(format!("record {:#04x} is not in the store", bind.record.0[0])),
        }
    }
}

/// A driver of a few slots and 64 pages of area.
fn driver(slots: u32) -> Driver {
    let page = sys::page().expect("a page size") as u64;
    Driver::new(Config { slots, area: 64 * page }).expect("a driver")
}

/// Yields until `done`, or fails naming `what` after [`PATIENCE`].
fn until(what: &str, mut done: impl FnMut() -> bool) {
    let end = Instant::now() + PATIENCE;
    while !done() {
        assert!(Instant::now() < end, "{what}: not within {PATIENCE:?}");
        std::thread::yield_now();
    }
}

/// A program thread that calls in a loop until told to stop. Dropped (a test that fails
/// included), it stops and joins the thread, so a failing test ends and does not leave a
/// thread spinning.
struct Caller {
    stop: Arc<AtomicBool>,
    /// How many rounds of calls it has made.
    rounds: Arc<AtomicU64>,
    /// The rounds made when it first saw the new code, plus one; 0 until then.
    new_at: Arc<AtomicU64>,
    thread: Option<JoinHandle<Result<(), String>>>,
}

impl Caller {
    /// Starts a thread that runs `round` until stopped. A round calls through slots and
    /// gives whether it saw the new program (`Ok(true)`), the old (`Ok(false)`), or
    /// something that must not be (`Err`); after each round the thread is at a safepoint.
    fn start(
        shared: &Arc<Shared>,
        mut round: impl FnMut(&Shared) -> Result<bool, String> + Send + 'static,
    ) -> Caller {
        let stop = Arc::new(AtomicBool::new(false));
        let rounds = Arc::new(AtomicU64::new(0));
        let new_at = Arc::new(AtomicU64::new(0));
        // The birth is announced before the thread starts.
        let birth = shared.birth();
        let thread = std::thread::spawn({
            let (stop, rounds, new_at) =
                (Arc::clone(&stop), Arc::clone(&rounds), Arc::clone(&new_at));
            move || {
                let me = birth.begin();
                let mut seen_new = false;
                while !stop.load(Ordering::Relaxed) {
                    let new = round(&**me.shared())?;
                    if seen_new && !new {
                        return Err("the old program ran after the new one".to_owned());
                    }
                    let made = rounds.fetch_add(1, Ordering::Relaxed) + 1;
                    if new && !seen_new {
                        seen_new = true;
                        new_at.store(made, Ordering::Relaxed);
                    }
                    me.safepoint();
                }
                Ok(())
            }
        });
        Caller { stop, rounds, new_at, thread: Some(thread) }
    }

    fn rounds(&self) -> u64 {
        self.rounds.load(Ordering::Relaxed)
    }

    /// Whether it has made `more` rounds, each ending in a safepoint, since it first saw the
    /// new program.
    fn past_new(&self, more: u64) -> bool {
        match self.new_at.load(Ordering::Relaxed) {
            0 => false,
            at => self.rounds() >= at + more,
        }
    }

    /// Stops the thread and gives what it found.
    fn finish(mut self) -> Result<(), String> {
        self.stop.store(true, Ordering::Relaxed);
        let thread = self.thread.take().expect("joined once");
        thread.join().map_err(|_| "the caller panicked".to_owned())?
    }
}

impl Drop for Caller {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            // The test is already failing; the thread's own finding is not this one.
            drop(thread.join());
        }
    }
}

/// The range of the body `slot`'s word names.
fn body(driver: &Driver, slot: u32) -> blaze_launch::Range {
    match driver.target(Slot(slot)) {
        Ok(Target::Body(range)) => range,
        other => panic!("slot {slot} names {other:?}, not a body"),
    }
}

#[test]
fn two_bodies_run_through_their_slots_and_one_is_replaced_under_a_caller_then_freed() {
    let mut records = Table::default();
    let mut driver = driver(8);
    let shared = driver.shared();
    assert_eq!(driver.declare(2), Ok(Slot(0)));
    let first = [records.bind(0, returns(1), 16), records.bind(1, returns(2), 16)];
    let committed = driver.edit(Txn(1), &first, false, &mut records).expect("bound");
    assert_eq!((committed.epoch, committed.unlinked), (1, 0));
    let me = shared.birth().begin();
    assert_eq!((call(&shared, 0), call(&shared, 1)), (1, 2));
    let old = body(&driver, 0);

    // A thread that was announced and never reports: it holds whatever is unlinked while
    // it lives.
    let silent = shared.birth();
    let caller = Caller::start(&shared, |shared| {
        let (a, b) = (call(shared, 0), call(shared, 1));
        match (a, b) {
            (1, 2) => Ok(false),
            (3, 2) => Ok(true),
            other => Err(format!("slots 0 and 1 returned {other:?}")),
        }
    });
    until("the caller's first round", || caller.rounds() > 0);

    // Slot 0 is replaced while the caller calls it.
    let second = [records.bind(0, returns(3), 16)];
    let committed = driver.edit(Txn(2), &second, false, &mut records).expect("replaced");
    assert_eq!((committed.epoch, committed.unlinked), (2, 1));
    assert_eq!(call(&shared, 0), 3);
    let retired = driver.placed(Frame { at: old.at, kind: FrameKind::Pc }).expect("retired");
    assert!(!retired.bound, "the replaced body waits to be freed");

    // This thread and the caller report past the epoch; the silent thread does not, and the
    // body stays.
    until("the caller's rounds on the new body", || caller.past_new(3));
    me.safepoint();
    assert_eq!(driver.settle(&mut records), Ok(0), "freed with a thread that has not reported");
    assert_eq!(driver.waiting(), 1);

    // The silent thread ends: now every thread alive at the unlink has cleared it.
    drop(silent);
    let mut freed = 0;
    until("the replaced body's batch to be freed", || {
        me.safepoint();
        freed += driver.settle(&mut records).expect("settled");
        driver.waiting() == 0
    });
    assert_eq!(freed, 1);
    assert_eq!(driver.placed(Frame { at: old.at, kind: FrameKind::Pc }), None);
    assert_eq!((call(&shared, 0), call(&shared, 1)), (3, 2));
    assert_eq!(caller.finish(), Ok(()));
}

#[test]
fn an_edit_of_two_functions_switches_them_together_under_a_caller() {
    let mut records = Table::default();
    let mut driver = driver(8);
    let shared = driver.shared();
    assert_eq!(driver.declare(2), Ok(Slot(0)));
    let first = [records.bind(0, returns(1), 16), records.bind(1, returns(2), 16)];
    driver.edit(Txn(1), &first, true, &mut records).expect("bound through a gate");
    let me = shared.birth().begin();

    // Each round calls both functions: one new and then the other old is what a gate is for.
    let mut both_new = false;
    let caller = Caller::start(&shared, move |shared| {
        let (a, b) = (call(shared, 0), call(shared, 1));
        match (a, b) {
            (1, 2) => Ok(false),
            // The commit between the two calls: old, then new. Never new, then old, in one
            // round or across two.
            (1, 20) if !both_new => Ok(true),
            (10, 20) => {
                both_new = true;
                Ok(true)
            }
            other => Err(format!("slots 0 and 1 returned {other:?}")),
        }
    });
    until("the caller's first round", || caller.rounds() > 0);

    let second = [records.bind(0, returns(10), 16), records.bind(1, returns(20), 16)];
    let committed = driver.edit(Txn(2), &second, true, &mut records).expect("switched");
    assert_eq!(committed.unlinked, 3, "two bodies and the gate");
    assert_eq!((call(&shared, 0), call(&shared, 1)), (10, 20));

    // The first edit's gate is one batch, the second edit's bodies and gate another.
    let mut freed = 0;
    until("both batches to be freed", || {
        me.safepoint();
        freed += driver.settle(&mut records).expect("settled");
        driver.waiting() == 0
    });
    assert_eq!(freed, 2);
    assert_eq!(caller.finish(), Ok(()));
}

#[test]
fn a_gate_entry_runs_the_old_body_until_its_flag_is_set_and_the_new_one_after() {
    let page = sys::page().expect("a page size");
    let mut code = Code::map(page).expect("a mapping");
    let base = code.base() as u64;
    // Two bodies, then a gate of one entry: its flag at 32, the entry at 40.
    code.write(0, &returns(7)).expect("the old body");
    code.write(16, &returns(9)).expect("the new body");
    code.write(32, &[0; 8]).expect("the flag, clear");
    let entry = arch::gate_entry(base + 40, base + 32, base, base + 16).expect("in reach");
    code.write(40, &entry).expect("the entry");

    assert_eq!(call_at(code.base() + 40), 7);
    code.store(32, 1).expect("the commit");
    assert_eq!(call_at(code.base() + 40), 9);

    assert!(code.write(page - 4, &[0; 8]).is_err(), "a write past the mapping's end");
    assert!(code.store(4, 1).is_err(), "a store that is not aligned");
}

#[test]
fn a_functions_address_is_its_entry_thunk_which_follows_the_slot() {
    let mut records = Table::default();
    let mut driver = driver(4);
    let shared = driver.shared();
    assert_eq!(driver.declare(2), Ok(Slot(0)));
    let first = [records.bind(1, returns(1), 16)];
    driver.edit(Txn(1), &first, false, &mut records).expect("bound");
    // What a `fn` pointer to slot 1's function holds.
    let pointer = shared.thunk(Slot(1)).expect("a slot of the table");
    assert_ne!(Some(pointer), shared.target(Slot(1)), "a pointer never names a body");
    assert_eq!(call_at(pointer), 1);
    let second = [records.bind(1, returns(2), 16)];
    driver.edit(Txn(2), &second, false, &mut records).expect("replaced");
    assert_eq!(call_at(pointer), 2, "the same pointer runs the new body");
}

#[test]
fn a_page_whose_bodies_are_all_freed_is_given_back() {
    let page = sys::page().expect("a page size");
    let mut records = Table::default();
    let mut driver = driver(4);
    let shared = driver.shared();
    assert_eq!(driver.declare(2), Ok(Slot(0)));
    // Slot 0's body is one whole page; slot 1's follows it, so the area's end leaves the page.
    let mut whole = returns(5);
    whole.resize(page, 0);
    let first = [records.bind(0, whole, page as u32)];
    driver.edit(Txn(1), &first, false, &mut records).expect("a page of code");
    let second = [records.bind(1, returns(6), 16)];
    driver.edit(Txn(2), &second, false, &mut records).expect("bound");
    assert_eq!((call(&shared, 0), call(&shared, 1)), (5, 6));

    // No thread is known, so what the edit unlinks is free at once.
    let third = [records.bind(0, returns(8), 16)];
    driver.edit(Txn(3), &third, false, &mut records).expect("replaced");
    assert_eq!(driver.decommitted(), 0);
    assert_eq!(driver.settle(&mut records), Ok(1));
    assert_eq!(driver.decommitted(), page as u64);
    assert_eq!((call(&shared, 0), call(&shared, 1)), (8, 6));
}

#[test]
fn an_edit_that_cannot_be_filled_changes_nothing_a_caller_can_reach() {
    let mut records = Table::default();
    let mut driver = driver(4);
    let shared = driver.shared();
    assert_eq!(driver.declare(2), Ok(Slot(0)));
    let first = [records.bind(0, returns(1), 16)];
    driver.edit(Txn(1), &first, false, &mut records).expect("bound");

    // A record the store does not have, after one it has.
    let mut missing = records.bind(1, returns(4), 16);
    missing.record = RecordId([0xee; 32]);
    let binds = [records.bind(0, returns(2), 16), missing];
    let refused = driver.edit(Txn(2), &binds, true, &mut records);
    let why = "record 0xee is not in the store".to_owned();
    let record = Error::Record { txn: Txn(2), slot: Slot(1), why };
    assert_eq!(refused, Err(EditError::Failed(record)));
    assert_eq!(call(&shared, 0), 1);
    assert_eq!(driver.target(Slot(1)), Ok(Target::Stub));

    // A record of another length than its bind says.
    let mut short = records.bind(0, returns(2), 16);
    short.len += 4;
    let refused = driver.edit(Txn(3), &[short], false, &mut records);
    assert!(matches!(refused, Err(EditError::Failed(Error::Length { .. }))), "{refused:?}");
    assert_eq!(call(&shared, 0), 1);

    // And the next edit goes through: nothing was left staged.
    let third = [records.bind(0, returns(3), 16)];
    driver.edit(Txn(4), &third, false, &mut records).expect("bound");
    assert_eq!(call(&shared, 0), 3);
}

/// The next message the launcher wrote: its frame's item.
fn next_frame(from: &mut UnixStream) -> Vec<u8> {
    let mut len = [0; 4];
    from.read_exact(&mut len).expect("a frame's length from the launcher");
    let mut item = vec![0; u32::from_be_bytes(len) as usize];
    from.read_exact(&mut item).expect("a frame's item from the launcher");
    item
}

/// Writes one message of the daemon's.
fn tell(to: &mut UnixStream, id: u64, tell: &Tell<'_>) {
    let mut frame = Vec::new();
    tell.encode(id, &mut frame).expect("a message that fits a frame");
    to.write_all(&frame).expect("written to the launcher");
}

#[test]
fn a_stub_asks_the_daemon_and_its_caller_resumes_in_the_code_it_sent() {
    let (ours, mut daemon) = UnixStream::pair().expect("a socket pair");
    daemon.set_read_timeout(Some(PATIENCE)).expect("a read timeout");
    let mut records = Table::default();
    let one = records.bind(0, returns(1), 16);
    let two = records.bind(0, returns(2), 16);
    let mut absent = records.bind(1, returns(3), 16);
    absent.record = RecordId([0xee; 32]);

    let driver = driver(8);
    let shared = driver.shared();
    let (ready, entry) = crossbeam_channel::bounded(1);
    let control = std::thread::spawn(move || {
        let mut records = records;
        let stay = |why: &str| format!("a test's launcher does not restart: {why}");
        let served = crate::run(driver, ours, ProgramId([7; 32]), &mut records, ready, stay);
        (served, records)
    });

    let item = next_frame(&mut daemon);
    let (id, hello) = Ask::decode(&item).expect("a hello");
    let pid = u64::from(std::process::id());
    assert!(
        matches!(hello, Ask::Hello { program: ProgramId(program), pid: said, .. }
            if program == [7; 32] && said == pid),
        "{hello:?}"
    );
    assert_eq!(id, 1);
    tell(&mut daemon, 1, &Tell::Welcome { slots: 2 });
    tell(&mut daemon, 2, &Tell::Start { entry: Slot(1), inits: List::of(&[]) });
    // Nothing calls a slot before it is declared: the program's first call comes after the
    // launcher took the daemon's welcome and its word to start, which is when its main
    // thread is given the address of the entry, the entry slot's thunk.
    let start = crate::Start { entry: shared.thunk(Slot(1)).expect("a slot"), inits: Vec::new() };
    assert_eq!(entry.recv_timeout(PATIENCE), Ok(Ok(start)));
    assert_ne!(shared.target(Slot(0)), Some(0));

    // A thread calls slot 0, which has no code: its stub fires and the thread waits.
    let (results, result) = crossbeam_channel::bounded::<u32>(2);
    let (go, gone) = crossbeam_channel::bounded::<()>(1);
    let birth = shared.birth();
    let caller = std::thread::spawn(move || {
        let me = birth.begin();
        drop(results.send(call(me.shared(), 0)));
        if gone.recv_timeout(PATIENCE).is_ok() {
            drop(results.send(call(me.shared(), 0)));
        }
        me.safepoint();
    });
    let item = next_frame(&mut daemon);
    assert_eq!(Ask::decode(&item), Ok((2, Ask::Need { slot: Slot(0), thread: Thread(1) })));

    // The daemon answers with the code; the launcher applies it and the caller goes on.
    let binds = [one];
    let edit = Tell::Edit { txn: Txn(1), binds: List::of(&binds), declare: 0, together: false };
    tell(&mut daemon, 10, &edit);
    let item = next_frame(&mut daemon);
    assert_eq!(Ask::decode(&item), Ok((10, Ask::Applied { txn: Txn(1), epoch: 1 })));
    assert_eq!(result.recv_timeout(PATIENCE), Ok(1));

    // An edit whose record the store does not have fails by name, and nothing changes.
    let binds = [absent];
    let edit = Tell::Edit { txn: Txn(2), binds: List::of(&binds), declare: 0, together: false };
    tell(&mut daemon, 11, &edit);
    let item = next_frame(&mut daemon);
    let why = "edit 2, slot 1: record 0xee is not in the store";
    assert_eq!(Ask::decode(&item), Ok((11, Ask::Failed { txn: Txn(2), why })));

    // An edit of the body: the caller's next call runs it.
    let binds = [two];
    let edit = Tell::Edit { txn: Txn(3), binds: List::of(&binds), declare: 0, together: false };
    tell(&mut daemon, 12, &edit);
    let item = next_frame(&mut daemon);
    assert_eq!(Ask::decode(&item), Ok((12, Ask::Applied { txn: Txn(3), epoch: 2 })));
    go.send(()).expect("the caller waits for this");
    assert_eq!(result.recv_timeout(PATIENCE), Ok(2));
    caller.join().expect("the caller ended");

    // Stopped, the control thread gives the driver back; the caller has ended, so the body
    // the second edit replaced is free.
    shared.stop();
    let ((mut driver, served), mut records) = control.join().expect("the control thread ended");
    assert_eq!(served, Ok(()));
    driver.settle(&mut records).expect("settled");
    assert_eq!(driver.waiting(), 0);
    assert_eq!(call(&shared, 0), 2);
}

/// A launcher's start from an image of placed code (`blaze_launch::image`): the daemon's one
/// message declares the slots, defines the static and binds the function, and the program's
/// main thread is given the entry's thunk and the initializer's address. The image is made
/// as the daemon makes it, against the addresses of no process; the function runs and the
/// static holds its value where this process has them.
#[test]
fn a_launcher_starts_from_an_image_of_placed_code() {
    use blaze_launch::image::{Bases, Input, lay};
    use blaze_launch::record::{Record, What};

    let (ours, mut daemon) = UnixStream::pair().expect("a socket pair");
    daemon.set_read_timeout(Some(PATIENCE)).expect("a read timeout");
    // A program's own launcher: an image is made for its mapping, not a test's small one.
    let driver = Driver::new(Config::PROGRAM).expect("a launcher's mapping");
    let shared = driver.shared();
    let (ready, entry) = crossbeam_channel::bounded(1);
    let control = std::thread::spawn(move || {
        let mut records = crate::Loaded::new().expect("the program's data mapping");
        let stay = |why: &str| format!("a test's launcher does not restart: {why}");
        crate::run(driver, ours, ProgramId([7; 32]), &mut records, ready, stay).1
    });
    let item = next_frame(&mut daemon);
    let Ok((_, Ask::Hello { arch, os, page, .. })) = Ask::decode(&item) else {
        panic!("the launcher's first message is no hello");
    };

    let code = returns(7);
    let value = 0x1122_3344_5566_7788u64.to_le_bytes();
    let function = Record {
        arch,
        what: What::Code,
        align: 16,
        size: code.len() as u64,
        bytes: &code,
        fixups: List::of(&[]),
        places: List::of(&[]),
        data: List::of(&[]),
        unwind: None,
        name: "app::seven",
    };
    let holds = Record {
        arch,
        what: What::Static { mutable: false, thread_local: false },
        align: 8,
        size: 8,
        bytes: &value,
        fixups: List::of(&[]),
        places: List::of(&[]),
        data: List::of(&[]),
        unwind: None,
        name: "app::VALUE",
    };
    let statics = [wire::StaticDef { number: 0, record: RecordId([1; 32]), refs: List::of(&[]) }];
    let binds = [wire::Bind {
        slot: Slot(1),
        record: RecordId([2; 32]),
        code: CodeId([2; 32]),
        tier: Tier::Zero,
        // Its code, then its pool, which begins at a multiple of 8 and holds nothing.
        len: (code.len() as u64).next_multiple_of(8),
        align: 16,
        refs: List::of(&[]),
    }];
    let input = Input {
        arch,
        os,
        page: u64::from(page),
        slots: 2,
        statics: &statics,
        binds: &binds,
        entry: Slot(1),
        inits: &[0],
    };
    let records = |id: &RecordId| match id.0[0] {
        1 => Some(holds),
        2 => Some(function),
        _ => None,
    };
    let image = lay(&input, &records, &Bases::NONE).unwrap_or_else(|why| panic!("{why}"));
    let mut bytes = Vec::new();
    image.encode(&mut bytes);
    tell(&mut daemon, 1, &Tell::Image { bytes: &bytes });

    let start = entry.recv_timeout(PATIENCE).expect("the launcher answers its image").expect("the image is taken");
    assert_eq!(start.entry, shared.thunk(Slot(1)).expect("a slot"));
    assert_eq!(start.inits.len(), 1);
    // SAFETY: the address the launcher gave for the static the image defines: 8 bytes of the
    // program's data mapping, written before the launcher answered and never unmapped.
    assert_eq!(unsafe { (start.inits[0] as *const u64).read_unaligned() }, u64::from_le_bytes(value));
    // Slot 1 is bound to the image's body; slot 0 is declared and has no code.
    let birth = shared.birth();
    let caller = std::thread::spawn(move || {
        let me = birth.begin();
        let got = (call(me.shared(), 1), call_at(me.shared().thunk(Slot(1)).expect("a slot")));
        me.safepoint();
        got
    });
    assert_eq!(caller.join().expect("the caller ended"), (7, 7));
    shared.stop();
    assert_eq!(control.join().expect("the control thread ended"), Ok(()));
}

/// An image made for another mapping than this launcher's is refused by name, and the thread
/// that waits to run the program is told why: nothing of it is used.
#[test]
fn an_image_of_another_geometry_is_refused() {
    use blaze_launch::image::{Bases, Input, lay};

    let (ours, mut daemon) = UnixStream::pair().expect("a socket pair");
    daemon.set_read_timeout(Some(PATIENCE)).expect("a read timeout");
    let driver = Driver::new(Config::PROGRAM).expect("a launcher's mapping");
    let shared = driver.shared();
    let (ready, entry) = crossbeam_channel::bounded(1);
    let control = std::thread::spawn(move || {
        let mut records = crate::Loaded::new().expect("the program's data mapping");
        let stay = |why: &str| format!("a test's launcher does not restart: {why}");
        crate::run(driver, ours, ProgramId([7; 32]), &mut records, ready, stay).1
    });
    let item = next_frame(&mut daemon);
    let Ok((_, Ask::Hello { arch, os, page, .. })) = Ask::decode(&item) else {
        panic!("the launcher's first message is no hello");
    };
    let input =
        Input { arch, os, page: u64::from(page), slots: 1, statics: &[], binds: &[], entry: Slot(0), inits: &[] };
    let none = |_: &RecordId| -> Option<blaze_launch::record::Record<'static>> { None };
    let mut image = lay(&input, &none, &Bases::NONE).unwrap_or_else(|why| panic!("{why}"));
    image.geometry.area += u64::from(page);
    let mut bytes = Vec::new();
    image.encode(&mut bytes);
    tell(&mut daemon, 1, &Tell::Image { bytes: &bytes });
    let why = entry.recv_timeout(PATIENCE).expect("the launcher answers its image").expect_err("refused");
    assert!(why.contains("made for a mapping whose thunks are at"), "{why}");
    shared.stop();
    assert!(control.join().expect("the control thread ended").is_err());
}

/// A safepoint's walk of its own stack reaches the stack's end where frames keep a frame
/// record. AArch64 only: there every frame of this test keeps one (on macOS by the ABI); on
/// x86-64 the test's own frames keep one only when built to (`table::walk` says what the
/// walk needs).
#[cfg(target_arch = "aarch64")]
#[test]
fn a_walk_of_a_threads_own_stack_reaches_its_end_and_finds_no_frame_in_an_area_it_is_not_in() {
    let page = sys::page().expect("a page size");
    let code = Code::map(page).expect("a mapping");
    let area = code.base()..code.base() + page;
    let walked = std::thread::spawn(move || {
        let mut frames = Vec::new();
        (crate::table::walk(&area, &mut frames), frames)
    });
    assert_eq!(walked.join().expect("walked"), (true, Vec::new()));
}

/// Rule `macos-private-apis`: each symbol `sys` calls beyond POSIX is looked up at run time,
/// so an OS that drops one fails here, naming it, and not in a launcher that cannot start.
#[cfg(target_os = "macos")]
#[test]
fn every_macos_symbol_resolves() {
    for (name, why) in sys::SYMBOLS {
        // SAFETY: a lookup, by a C string's name, among the images this process has loaded.
        let found = unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) };
        assert!(!found.is_null(), "{name:?} is gone from this macOS; the launcher uses it for: {why}");
    }
}
