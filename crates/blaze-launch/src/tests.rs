//! The tests: the core under every schedule a case's tape draws, with its planted bugs
//! ([`sim`]); the wire's bytes against blaze-protocol's check of the deterministic encoding
//! ([`wire`]); and here, what one call of the core refuses and what the area gives back.

mod image;
mod record;
mod sim;
mod wire;

use crate::area::Area;
use crate::{
    Bind, CodeId, Config, Edit, Error, Frame, FrameKind, KEPT, Kept, Launch, Range, Slot, Target,
    Thread, Tier, Txn, Write,
};

const CONFIG: Config =
    Config { slots: 4, area: 4096, page: 256, gate_head: 16, gate_entry: 16, gate_align: 16 };

fn alloc() -> Kept {
    KEPT
}

fn bind(slot: u32, len: u64) -> Bind {
    Bind { slot: Slot(slot), len, align: 16, tier: Tier::Zero, code: CodeId([slot as u8; 32]) }
}

fn edit(txn: u64, binds: &[Bind], together: bool) -> Edit<'_> {
    Edit { txn: Txn(txn), binds, together }
}

/// Binds slot 0 to a new body of 16 bytes: staged and committed, the writes dropped.
fn rebind(launch: &mut Launch, txn: u64) {
    let mut out = Vec::new_in(alloc());
    launch.stage(&edit(txn, &[bind(0, 16)], false), &mut out).expect("staged");
    launch.commit(Txn(txn), &mut out).expect("committed");
}

/// A core with every slot declared, and the writes of that taken.
fn core() -> (Launch, Vec<Write, Kept>) {
    let mut launch = Launch::new(CONFIG, alloc()).expect("the config is whole");
    let mut out = Vec::new_in(alloc());
    assert_eq!(launch.declare(CONFIG.slots, &mut out), Ok(Slot(0)));
    out.clear();
    (launch, out)
}

/// What a caller can see of a core.
fn seen(launch: &Launch) -> (u64, u64, Option<Txn>, usize, [Target; 4]) {
    let targets = [0, 1, 2, 3].map(|slot| launch.target(Slot(slot)).expect("declared"));
    (launch.epoch(), launch.room(), launch.staged(), launch.waiting(), targets)
}

#[test]
fn a_refused_stage_changes_nothing() {
    let (mut launch, mut out) = core();
    let before = seen(&launch);
    let refusals: [(&[Bind], Error); 6] = [
        (&[], Error::Empty { txn: Txn(1) }),
        (&[bind(0, 16), bind(4, 16)], Error::NoSlot { slot: Slot(4), declared: 4 }),
        (&[bind(0, 16), bind(1, 16), bind(0, 16)], Error::Twice { txn: Txn(1), slot: Slot(0) }),
        (
            &[bind(0, 16), bind(1, 0)],
            Error::Code { txn: Txn(1), slot: Slot(1), what: "the code has no length", value: 0 },
        ),
        // The second bind does not fit after the first.
        (&[bind(0, 4000), bind(1, 200)], Error::Full { need: 4200, room: 4096 }),
        // The bodies fit and the gate does not.
        (&[bind(0, 2000), bind(1, 2064)], Error::Full { need: 4112, room: 4096 }),
    ];
    for (binds, refusal) in refusals {
        assert_eq!(launch.stage(&edit(1, binds, true), &mut out), Err(refusal));
        assert!(out.is_empty(), "a refused stage handed out {out:?}");
        assert_eq!(seen(&launch), before);
    }

    // One transaction at a time; and only the staged one commits or aborts.
    assert_eq!(launch.stage(&edit(1, &[bind(0, 16)], true), &mut out), Ok(()));
    let staged = seen(&launch);
    assert_eq!(
        launch.stage(&edit(2, &[bind(1, 16)], true), &mut out),
        Err(Error::Open { open: Txn(1), asked: Txn(2) })
    );
    let not_staged = Error::NotStaged { asked: Txn(2), open: Some(Txn(1)) };
    assert_eq!(launch.commit(Txn(2), &mut out), Err(not_staged));
    assert_eq!(launch.abort(Txn(2), &mut out), Err(not_staged));
    assert_eq!(seen(&launch), staged);
}

#[test]
fn an_edit_that_switches_together_goes_through_its_gate_and_the_epoch_comes_last() {
    let (mut launch, mut out) = core();
    let binds = [bind(0, 32), bind(1, 16)];
    launch.stage(&edit(1, &binds, true), &mut out).expect("staged");
    let (a, b) = (Range { at: 0, len: 32 }, Range { at: 32, len: 16 });
    let gate = Range { at: 48, len: 48 };
    assert_eq!(
        out[..],
        [
            Write::Place { bind: 0, range: a },
            Write::Place { bind: 1, range: b },
            Write::Gate { range: gate, entries: 2 },
            Write::GateEntry { gate, entry: 0, old: Target::Stub, new: a },
            Write::GateEntry { gate, entry: 1, old: Target::Stub, new: b },
        ]
    );
    // Staged is not bound: the words are stubs, and a frame in a staged body names nothing.
    assert_eq!(launch.target(Slot(0)), Ok(Target::Stub));
    assert_eq!(launch.placed(Frame { at: 0, kind: FrameKind::Pc }), None);

    out.clear();
    let committed = launch.commit(Txn(1), &mut out).expect("committed");
    assert_eq!((committed.epoch, committed.unlinked), (1, 1), "the gate alone was unlinked");
    assert_eq!(
        out[..],
        [
            Write::Slot { slot: Slot(0), to: Target::Gate { gate, entry: 0 } },
            Write::Slot { slot: Slot(1), to: Target::Gate { gate, entry: 1 } },
            Write::Commit { gate },
            Write::Slot { slot: Slot(0), to: Target::Body(a) },
            Write::Slot { slot: Slot(1), to: Target::Body(b) },
            Write::Epoch { epoch: 1 },
        ]
    );
    assert_eq!(launch.target(Slot(1)), Ok(Target::Body(b)));

    // A return address at the first body's end is in the first body, not the second.
    let at_end = launch.placed(Frame { at: 32, kind: FrameKind::Return }).expect("a body");
    assert_eq!((at_end.slot, at_end.range, at_end.bound), (Slot(0), a, true));
    let at_start = launch.placed(Frame { at: 32, kind: FrameKind::Pc }).expect("a body");
    assert_eq!(at_start.slot, Slot(1));
}

#[test]
fn a_thread_born_before_an_unlink_holds_it_and_one_born_after_does_not() {
    let (mut launch, mut out) = core();
    rebind(&mut launch, 1);
    // Told late of a thread that loaded epoch 1 before the second edit unlinked the first
    // body: it may be in that body.
    rebind(&mut launch, 2);
    assert_eq!(launch.waiting(), 1);
    assert_eq!(launch.thread_began(Thread(7), 1), Ok(()));
    assert_eq!(launch.collect(&mut out), 0, "the late-told thread holds the batch");
    // A thread that loaded epoch 2 was born after the unlinking stores: it holds nothing.
    assert_eq!(launch.thread_began(Thread(8), 2), Ok(()));
    assert_eq!(launch.report(Thread(7), 2, &[], true), Ok(()));
    assert_eq!(launch.collect(&mut out), 1);
    assert_eq!(out[..], [Write::Forget { range: Range { at: 0, len: 16 } }]);

    assert_eq!(launch.thread_began(Thread(7), 2), Err(Error::ThreadTwice { thread: Thread(7) }));
    assert_eq!(launch.thread_ended(Thread(9)), Err(Error::NoThread { thread: Thread(9) }));
    assert!(matches!(launch.report(Thread(8), 3, &[], true), Err(Error::Epoch { said: 3, .. })));
}

#[test]
fn an_incomplete_walk_holds_everything() {
    let (mut launch, mut out) = core();
    launch.thread_began(Thread(0), 0).unwrap();
    rebind(&mut launch, 1);
    rebind(&mut launch, 2);
    launch.report(Thread(0), 2, &[], false).unwrap();
    assert_eq!(launch.collect(&mut out), 0);
    launch.report(Thread(0), 2, &[], true).unwrap();
    assert_eq!(launch.collect(&mut out), 1);
}

#[test]
fn the_area_gives_back_each_whole_page_once_and_no_offset_twice() {
    let mut area = Area::new(4096, 256, alloc()).unwrap();
    // Three ranges: the first two share page 0, the second runs through page 1 into page 2,
    // the third is in page 2.
    let a = area.append(100, 4).unwrap();
    let b = area.append(500, 4).unwrap();
    let c = area.append(100, 4).unwrap();
    assert_eq!((a.at, b.at, c.at), (0, 100, 600));
    let mut gone = Vec::new();
    area.free(b, |pages| gone.push(pages));
    assert_eq!(gone, [Range { at: 256, len: 256 }], "only page 1 held nothing but b");
    gone.clear();
    area.free(a, |pages| gone.push(pages));
    assert_eq!(gone, [Range { at: 0, len: 256 }]);
    gone.clear();
    // Page 2 is where the end is: it is not given back under the end.
    area.free(c, |pages| gone.push(pages));
    assert!(gone.is_empty(), "{gone:?}");
    // What follows goes after everything ever handed out.
    assert_eq!(area.append(16, 16).unwrap().at, 704);
    assert_eq!(area.append(4096, 1), Err(Error::Full { need: 4096, room: 4096 - 720 }));
    assert!(matches!(Area::new(1000, 256, alloc()), Err(Error::Config { .. })));
}
