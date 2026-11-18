//! The command's store hands a pool a snapshot (blaze-cas plain/snapshot.rs): taken after a
//! phase's commit, on bstd's simulated world with the store on the world's lent driver as the
//! command opens it, and read from eight threads at once. Each thread reads every record
//! byte-equal to what `Store::get` reads on the command's thread; a record put and committed
//! after the take is absent through it. The records stand in for a names record (a few KiB, not
//! a drive record, so filed `LOCAL`): making a real one runs the names stage, which this test of
//! the store's seam does not need. The snapshot's own invariant, with its planted controls, is
//! blaze-cas plain/snapshot_tests.rs.

use blaze_cas::Key as CasKey;
use blaze_cas::plain::Settings;
use blaze_drive::Key;
use blaze_ids::ContentId;
use bstd::io::{Io, Limits, SimWorld};
use bstd::mem::TASK_FRAME;
use bstd::test::BUDGET;

use super::{CommandStore, Facts, Home, Store};

const HOME: &str = "/h";
/// Records the phase puts; record `RECORDS` is put after the snapshot.
const RECORDS: u32 = 24;

fn key(n: u32) -> Key {
    Key(ContentId::of("command store snapshot test record", &n.to_le_bytes()))
}

/// Record `n`'s one value: its number, then 2 to 6 KiB, as a names record's size runs.
fn value(n: u32) -> std::vec::Vec<u8> {
    let mut bytes = n.to_le_bytes().to_vec();
    bytes.extend((0..2048 + (n as usize * 811) % 4096).map(|i| (n as usize).wrapping_mul(97).wrapping_add(i) as u8));
    bytes
}

#[test]
fn a_snapshot_after_a_phase_commit_reads_its_records_from_eight_threads() {
    let io = Io::simulated(SimWorld::new(31, Limits::USUAL));
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let fs = io.need_fs().expect("files");
    let err = io.need_err().expect("err");
    let clock = io.need_clock().expect("a clock");
    let home = Home::of(&cx, &fs, Some(HOME)).expect("the home");
    let heap = blaze_own::testing::heap();
    let lent = home.open_store(&Settings::default(), Facts { boot: Some(1), nonce: 31 }, heap.alloc()).expect("the store opens");
    let frame = cx.frame();
    let erased = blaze_own::Erased::new(frame);
    let mut store = CommandStore::new(&cx, &lent, false, 0, None, clock, &err, erased.handle()).expect("the command's store");
    for n in 0..RECORDS {
        store.put(&cx, key(n), &value(n)).expect("a put");
    }
    store.commit_puts().expect("the phase's commit");
    let snapshot = store.snapshot_in(heap.shared()).expect("a snapshot after the phase's commit");
    // What `Store::get` reads on the command's thread: what every thread must read.
    let mut want: std::vec::Vec<std::vec::Vec<u8>> = std::vec::Vec::new();
    for n in 0..RECORDS {
        let held = store.get(&cx, key(n)).expect("a get").expect("a committed record");
        assert_eq!(held.bytes, value(n).as_slice(), "record {n} through Store::get");
        want.push(held.bytes.to_vec());
    }
    // Put and committed after the take: absent through the snapshot.
    store.put(&cx, key(RECORDS), &value(RECORDS)).expect("a put after the snapshot");
    store.commit_puts().expect("the next phase's commit");
    let (snapshot, want) = (&snapshot, &want);
    std::thread::scope(|scope| {
        let readers: std::vec::Vec<_> = (0..8u32)
            .map(|t| {
                scope.spawn(move || {
                    for round in 0..16u32 {
                        for i in 0..=RECORDS {
                            let n = (i + t * 3 + round) % (RECORDS + 1);
                            let got = snapshot.get(&CasKey(key(n).0)).expect("a snapshot read");
                            match want.get(n as usize) {
                                Some(bytes) => assert_eq!(got, Some(bytes.as_slice()), "thread {t}, record {n}"),
                                None => assert_eq!(got, None, "thread {t}, record {n}: committed after the take"),
                            }
                        }
                    }
                })
            })
            .collect();
        for reader in readers {
            reader.join().expect("a reader thread");
        }
    });
    println!("command store snapshot: 8 threads read {RECORDS} records through one snapshot of {} segments, byte-equal to Store::get; a later record absent", snapshot.segments());
}
