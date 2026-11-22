//! The run wire's [`super::run::Down::Bytes`]: a record's bytes carried inline, in parts, before
//! the message that names the record (run schema 6).
//!
//! Why inline: the record a first call needs is often one the parent made a moment ago, still in
//! its store's arena and in no file or segment yet (blaze-cas `Visibility::InArena`), so the one
//! copy that exists is the parent's; a second file per record under `run/records` was a copy
//! made only to be read once (thoughts3.md section 2). The bytes go over descriptor 3, which the
//! program already holds, so no sandbox gains a path.
//!
//! A record is sent as consecutive parts `{record, at, total, part}`: `at` the offset of `part`
//! in the record, `total` the record's length, the same in every part. One message is checked
//! here, once, by its shape: a part is not empty, not past [`BYTES_PART_MOST`], and ends within
//! `total`; a `total` past [`BYTES_RECORD_MOST`] is refused before the receiver allocates a byte.
//! What spans messages (the parts in order, nothing between them, the sum, the digest, the
//! receiver's [`BYTES_HELD_MOST`]) is the receiver's state machine's (blaze-launch-rt
//! run_mode/received.rs). The body is `{"Bytes": {"at": uint, "part": bytes, "total": uint,
//! "record": bytes(32)}}`, its keys in the order of their encodings (by length, then bytes).

use core::alloc::Allocator;

use super::cbor::{Reader, Writer};
use super::{RecordId, WireError, id, variant};

/// The variant's name in a body.
pub(super) const NAME: &str = "Bytes";

/// The longest part one message carries: 4 MiB, a quarter of a frame ([`super::LONGEST_FRAME`]),
/// so a part and its envelope always fit one frame, and few enough frames for a large record.
pub const BYTES_PART_MOST: u64 = 4 << 20;

/// The longest record sent inline: 1 GiB. No record is placed that is larger than the program's
/// data area (blaze-launch-rt run_mode.rs `DATA_BYTES`) or half its code reach (drive.rs
/// `REACH`), so a longer `total` is a broken or hostile sender, refused before any allocation.
pub const BYTES_RECORD_MOST: u64 = 1 << 30;

/// The most record bytes a program holds from its parent in one run: 3 GiB, the code reach and
/// the data area together (drive.rs `REACH` 2 GiB, run_mode.rs `DATA_BYTES` 1 GiB). A run that
/// could place more has nowhere to put it; one more record is refused by name, never dropped.
pub const BYTES_HELD_MOST: u64 = 3 << 30;

/// Writes the variant's body: `{"Bytes": {"at", "part", "total", "record"}}`.
pub(super) fn write<A: Allocator>(to: &mut Writer<'_, A>, record: RecordId, at: u64, total: u64, part: &[u8]) {
    variant(to, NAME, 4);
    to.text("at");
    to.uint(at);
    to.text("part");
    to.bytes(part);
    to.text("total");
    to.uint(total);
    to.text("record");
    to.bytes(&record.0);
}

/// Reads the variant's fields, the reader just past its name: `(record, at, total, part)`, its
/// shape checked (the module doc).
pub(super) fn read<'a>(from: &mut Reader<'a>) -> Result<(RecordId, u64, u64, &'a [u8]), WireError> {
    from.map(4, "Bytes, a map of 4")?;
    from.key("at")?;
    let at = from.uint()?;
    from.key("part")?;
    let opened = from.at();
    let part = from.bytes()?;
    if part.is_empty() {
        return Err(WireError::Range { at: opened, what: "an empty part of a record's bytes" });
    }
    if part.len() as u64 > BYTES_PART_MOST {
        return Err(WireError::Range { at: opened, what: "a part of a record's bytes longer than BYTES_PART_MOST (4 MiB)" });
    }
    from.key("total")?;
    let counted = from.at();
    let total = from.uint()?;
    if total > BYTES_RECORD_MOST {
        return Err(WireError::Range { at: counted, what: "a record's total longer than BYTES_RECORD_MOST (1 GiB)" });
    }
    // `at <= total` first, so `total - at` cannot wrap.
    if at > total || part.len() as u64 > total - at {
        return Err(WireError::Range { at: opened, what: "a part of a record's bytes that ends past its total" });
    }
    from.key("record")?;
    let record = RecordId(id(from, "a record id is 32 bytes")?);
    Ok((record, at, total, part))
}

#[cfg(test)]
mod tests {
    use super::super::run::Down;
    use super::super::{RecordId, WireError, split};
    use super::{BYTES_PART_MOST, BYTES_RECORD_MOST};

    fn item_of(down: &Down<'_>) -> std::vec::Vec<u8> {
        let mut out = std::vec::Vec::new();
        down.encode(4, &mut out).expect("Bytes encodes");
        let (item, took) = split(&out).expect("a frame").expect("a whole frame");
        assert_eq!(took, out.len(), "one frame");
        item.to_vec()
    }

    /// A part reads back as written, its frame deterministic CBOR; every cut is refused, never
    /// read as another message; and each shape the module doc refuses is refused at its byte:
    /// an empty part, a part past its total (at past the total, and at plus the part past it),
    /// and a total past BYTES_RECORD_MOST. Expected: passes with no output.
    #[test]
    fn bytes_round_trips_and_a_bad_shape_is_refused() {
        let part = [7u8; 300];
        let down = Down::Bytes { record: RecordId([0xab; 32]), at: 1000, total: 1300, part: &part };
        let item = item_of(&down);
        if let Err(why) = blaze_protocol::cbor::check(&item) {
            panic!("Bytes is not deterministic CBOR: {why}");
        }
        assert_eq!(Down::decode(&item), Ok((4, down)));
        for cut in 0..item.len() {
            assert!(Down::decode(&item[..cut]).is_err(), "a Bytes cut to {cut} bytes was read");
        }

        let refused = |down: Down<'_>, what: &str| match Down::decode(&item_of(&down)) {
            Err(WireError::Range { what: said, .. }) => assert_eq!(said, what),
            other => panic!("{down:?} was not refused by its shape: {other:?}"),
        };
        refused(Down::Bytes { record: RecordId([1; 32]), at: 0, total: 9, part: &[] }, "an empty part of a record's bytes");
        refused(Down::Bytes { record: RecordId([1; 32]), at: 10, total: 9, part: &[1] }, "a part of a record's bytes that ends past its total");
        refused(Down::Bytes { record: RecordId([1; 32]), at: 5, total: 9, part: &[1; 5] }, "a part of a record's bytes that ends past its total");
        refused(Down::Bytes { record: RecordId([1; 32]), at: 0, total: BYTES_RECORD_MOST + 1, part: &[1] }, "a record's total longer than BYTES_RECORD_MOST (1 GiB)");
        let long = std::vec![0u8; BYTES_PART_MOST as usize + 1];
        refused(Down::Bytes { record: RecordId([1; 32]), at: 0, total: long.len() as u64, part: &long }, "a part of a record's bytes longer than BYTES_PART_MOST (4 MiB)");
    }
}
