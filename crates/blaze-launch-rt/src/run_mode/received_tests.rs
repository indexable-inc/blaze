//! The child's half of the inline records (run_mode/received.rs): N records sent in parts are
//! held as exactly the bytes their ids name, read with no file; and each breach is refused by
//! name: a planted wrong digest, a record cut short (another message before its last part), a
//! part out of order, a part of another record between two of one, a record sent twice, a total
//! past the run's bound, and a mix of the two ways a run reads records.

use blaze_launch::wire::run::{BYTES_HELD_MOST, BYTES_PART_MOST, Declared, Down, Symbol as WireSymbol};
use blaze_launch::wire::{List, RecordId};
use blaze_launch::{Slot, Tier};
use blaze_own::{AVec, Alloc, Vec};

use super::super::{Act, Event, RecordSource, RunMode, RunModeError};
use super::{Received, SentRefused};

fn alloc() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

/// A store with no file: every read is refused, so a record that is found was sent.
struct NoFiles;

impl RecordSource for NoFiles {
    fn bytes(&self, id: RecordId) -> Result<&[u8], RunModeError> {
        Err(RunModeError::Missing { record: id, errno: 2 })
    }
}

/// A store of one record, read as the file path reads.
struct OneFile {
    id: RecordId,
    bytes: std::vec::Vec<u8>,
}

impl RecordSource for OneFile {
    fn bytes(&self, id: RecordId) -> Result<&[u8], RunModeError> {
        match id == self.id {
            true => Ok(&self.bytes),
            false => Err(RunModeError::Missing { record: id, errno: 2 }),
        }
    }
}

/// Record `n`'s bytes: distinct per `n`, of a length that takes several parts of `part`.
fn record(n: u8, len: usize) -> std::vec::Vec<u8> {
    (0..len).map(|i| (i as u8).wrapping_mul(31).wrapping_add(n)).collect()
}

fn id_of(bytes: &[u8]) -> RecordId {
    RecordId(blaze_blake3::hash(bytes))
}

/// `bytes` sent as record `record` in parts of `part` bytes.
fn send(received: &mut Received<'_>, record: RecordId, bytes: &[u8], part: usize) -> Result<(), RunModeError> {
    let total = bytes.len() as u64;
    for (i, chunk) in bytes.chunks(part).enumerate() {
        received.part(record, (i * part) as u64, total, chunk)?;
    }
    Ok(())
}

fn refusal(error: RunModeError) -> (RecordId, SentRefused) {
    match error {
        RunModeError::Sent { record, why } => (record, why),
        other => panic!("not a refusal of sent bytes: {other:?}"),
    }
}

/// N records, each in several parts (the last one short), are held as exactly the bytes their
/// ids name and read with no file; a record sent and never completed is what `assembling`
/// names; a record named and never sent is `NotSent`, not looked for in a file. Expected:
/// passes with no output.
#[test]
fn n_records_are_held_as_their_ids_name_with_no_file() {
    let mut received = Received::new(alloc());
    let records: std::vec::Vec<std::vec::Vec<u8>> = (0..12u8).map(|n| record(n, 100 + 37 * usize::from(n))).collect();
    for bytes in &records {
        send(&mut received, id_of(bytes), bytes, 64).expect("a record sent in order is held");
        assert!(received.assembling().is_none(), "a whole record is not being assembled");
    }
    let mut scratch = std::vec::Vec::new();
    for bytes in &records {
        let got = received.bytes_of(id_of(bytes), &NoFiles, &mut scratch).expect("a sent record is read with no file");
        assert_eq!(got, &bytes[..], "the bytes held are the bytes sent");
        assert_eq!(id_of(got), id_of(bytes), "they hash to the id the child asks by");
    }
    assert!(scratch.is_empty(), "no file was read");
    let never = id_of(b"a record never sent");
    assert_eq!(refusal(received.bytes_of(never, &NoFiles, &mut scratch).expect_err("a record never sent")), (never, SentRefused::NotSent));
    assert_eq!(refusal(received.again(never, &NoFiles, &[]).expect_err("a record never sent, the second read")), (never, SentRefused::NotSent));
}

/// Each breach the module doc names is refused naming the record, and nothing of a refused
/// record is held. The control first: the same bytes, right, are held. Expected: passes with
/// no output.
#[test]
fn each_breach_is_refused_by_name() {
    let good = record(1, 300);
    let id = id_of(&good);

    // A planted wrong digest: the bytes of `good` sent under another record's id.
    let mut received = Received::new(alloc());
    let other = id_of(b"another record");
    let (named, why) = refusal(send(&mut received, other, &good, 64).expect_err("a wrong digest"));
    assert_eq!(named, other);
    assert_eq!(why, SentRefused::Digest { got: id.0 });
    assert!(received.held(other).is_none(), "a record whose digest failed is not held");
    // The control: the same bytes under their own id are held.
    let mut received = Received::new(alloc());
    send(&mut received, id, &good, 64).expect("the control: right bytes under their id");
    assert_eq!(received.held(id), Some(&good[..]));

    // A truncated total: two of three parts, then another message (`RunMode::down` asks
    // `assembling` before every message that is not a part).
    let mut received = Received::new(alloc());
    received.part(id, 0, 300, &good[..100]).expect("part 1");
    received.part(id, 100, 300, &good[100..200]).expect("part 2");
    assert_eq!(received.assembling().map(refusal), Some((id, SentRefused::Partial { got: 200, total: 300 })));

    // An out-of-order part: the third before the second; and a first part not at 0.
    let mut received = Received::new(alloc());
    received.part(id, 0, 300, &good[..100]).expect("part 1");
    assert_eq!(refusal(received.part(id, 200, 300, &good[200..]).expect_err("part 3 before part 2")), (id, SentRefused::OutOfOrder { at: 200, want: 100 }));
    let mut received = Received::new(alloc());
    assert_eq!(refusal(received.part(id, 100, 300, &good[100..200]).expect_err("a first part at 100")), (id, SentRefused::FirstAt { at: 100 }));

    // A part of another record between two of one; a total that changes.
    let mut received = Received::new(alloc());
    received.part(id, 0, 300, &good[..100]).expect("part 1");
    let (named, why) = refusal(received.part(other, 0, 10, &[1; 10]).expect_err("another record's part"));
    assert_eq!((named, why), (other, SentRefused::Interleaved { assembling: id }));
    assert_eq!(refusal(received.part(id, 100, 301, &good[100..200]).expect_err("a changed total")), (id, SentRefused::TotalChanged { total: 301, was: 300 }));

    // A record sent twice.
    let mut received = Received::new(alloc());
    send(&mut received, id, &good, 64).expect("once");
    assert_eq!(refusal(send(&mut received, id, &good, 64).expect_err("twice")), (id, SentRefused::Again));

    // A total past what the run holds, refused before any allocation (the count planted near
    // the bound: no test allocates gigabytes).
    let mut received = Received::new(alloc());
    received.counted = BYTES_HELD_MOST - 10;
    assert_eq!(
        refusal(received.part(id, 0, 300, &good[..100]).expect_err("past the bound")),
        (id, SentRefused::HeldFull { total: 300, held: BYTES_HELD_MOST - 10 })
    );
    assert!(received.assembling().is_none(), "nothing was opened for a refused total");
}

/// One run reads one way: a record read from the files first makes a later `Bytes` a
/// refusal; the file path itself (today's control) reads and checks as before. Expected: passes
/// with no output.
#[test]
fn a_run_reads_its_records_one_way() {
    let bytes = record(3, 90);
    let file = OneFile { id: id_of(&bytes), bytes: bytes.clone() };
    let mut received = Received::new(alloc());
    let mut scratch = std::vec::Vec::new();
    assert_eq!(received.bytes_of(file.id, &file, &mut scratch).expect("the file path reads"), &bytes[..]);
    assert_eq!(received.again(file.id, &file, &scratch).expect("and reads again"), &bytes[..]);
    assert_eq!(refusal(received.part(file.id, 0, 90, &bytes).expect_err("bytes after files")), (file.id, SentRefused::AfterFiles));
}

/// Through the run mode's wire: a `Publish` that comes while its record has only part of its
/// bytes stops the program naming the record (a truncated total, caught at the message after
/// the cut); and the parts whole, then the `Declare` and `Publish`, are taken with the record
/// held. The parts are as long as the wire allows, so a record of three parts is 8 MiB and
/// more. Expected: passes with no output.
#[test]
fn the_wire_refuses_a_record_cut_short_and_takes_a_whole_one() {
    let part = BYTES_PART_MOST as usize;
    let bytes = record(7, 2 * part + 5);
    let id = id_of(&bytes);
    let total = bytes.len() as u64;
    let pairs = [Declared { symbol: WireSymbol([4; 32]), slot: Slot(2) }];
    let mut acts: AVec<'static, Act> = Vec::new_in(alloc());

    let mut cut = std::vec::Vec::new();
    Down::Bytes { record: id, at: 0, total, part: &bytes[..part] }.encode(1, &mut cut).expect("part 1 encodes");
    Down::Declare { pairs: List::of(&pairs) }.encode(2, &mut cut).expect("Declare encodes");
    let mut mode = RunMode::new(alloc());
    let error = mode.on(Event::Frame(&cut), &mut acts).expect_err("a record cut short");
    assert_eq!(refusal(error), (id, SentRefused::Partial { got: part as u64, total }));

    let mut whole = std::vec::Vec::new();
    for (i, chunk) in bytes.chunks(part).enumerate() {
        Down::Bytes { record: id, at: (i * part) as u64, total, part: chunk }.encode(1 + i as u64, &mut whole).expect("a part encodes");
    }
    Down::Declare { pairs: List::of(&pairs) }.encode(9, &mut whole).expect("Declare encodes");
    Down::Publish { slot: Slot(2), tier: Tier::Zero, record: id }.encode(10, &mut whole).expect("Publish encodes");
    let mut mode = RunMode::new(alloc());
    acts.clear();
    mode.on(Event::Frame(&whole), &mut acts).expect("whole parts, then the record's publication");
    let mut scratch = std::vec::Vec::new();
    assert_eq!(mode.received.bytes_of(id, &NoFiles, &mut scratch).expect("held"), &bytes[..]);
}
