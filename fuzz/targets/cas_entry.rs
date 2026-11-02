//! The store's entries: one record (`crates/blaze-cas/src/disk.rs`, `RecordHeader`) and what
//! a read makes of its payload by its form (`read.rs`: raw bytes, a list of chunk references,
//! zstd and LZ4 frames, a file of its own), read through the public `Store::get`,
//! `Reader::get` and `Store::verify` in `blaze_core`'s simulation.
//!
//! The input's first byte says what the rest is. Even: up to four records, each its header's
//! fields and payload as the input gives them (lengths and content hash are the payload's
//! unless given), sealed, in the one pack a head names; each record's key is read. Odd: values
//! put through `Store::put`, flushed and settled (`cas_shared::values`), then damage in place
//! to one of their records; bit 1 reseals that record's header, so the damage reaches what the
//! header names: its lengths, its form, its key, its payload.
//!
//! Invariants, each checked below (a violation panics, which the engine reports as a crash):
//! - opening, reading and scrubbing never panic or hang;
//! - no allocation is larger than `cas_shared::bound` of the bytes on disk: a header naming a
//!   length its bytes cannot hold is `Corrupt`, not room made for it;
//! - a store that did not open says why (`Store::broken`);
//! - a value read is of the key asked and hashes to its content, so for a record whose content
//!   is its payload's hash it is that payload, and for a value put it is the bytes put: damage
//!   is `Corrupt`, never other bytes; `Store::get` and `Reader::get` agree; a scrub finds every
//!   corrupt value a read finds;
//! - round trip: values put, with no damage, open and read back as they were put.

#![forbid(unsafe_code)]

use std::time::Duration;

use blaze_cas::{Kind, Key};
use blaze_fuzzer::Target;
use blaze_ids::ContentId;

#[path = "cas_shared.rs"]
mod shared;

use shared::{HEAD, Input, Record};

const TARGET: Target = Target {
    name: "cas_entry",
    check,
    seeds,
    dictionary: &[
        b"BLZR",
        b"\xff\xff\xff\xff",
        b"\x00\x00\x00\x00\x00\x01\x00\x00",
        b"\x00\x00\x00\x00\x00\x00\x00\x40",
        b"\xff\xff\xff\xff\xff\xff\xff\xff",
        b"\x28\xb5\x2f\xfd",
        b"\x04\x22\x4d\x18",
    ],
    max_len: 1 << 16,
    budget: Duration::from_secs(600),
};

const STORED_GIVEN: u8 = 1;
const RAW_GIVEN: u8 = 2;
const CONTENT_GIVEN: u8 = 4;
const RECORDS_MOST: u8 = 4;

fn check(data: &[u8]) {
    let mut input = Input::new(data);
    let form = input.u8();
    if form & 1 == 0 {
        let mut pack = shared::pack_header(0).to_vec();
        let mut wants: Vec<(Key, Option<&[u8]>)> = Vec::new();
        for n in 0..=input.u8() % RECORDS_MOST {
            let (record, payload, hashed) = record(&mut input, Key(ContentId([n; 32])));
            shared::append_record(&mut pack, &record, payload);
            wants.push((record.key, hashed.then_some(payload)));
        }
        let pack = shared::padded(pack);
        let head = shared::head(None, &[shared::PackRef { committed: pack.len() as u64, ..shared::lone_ref(0) }], &[]);
        let mut sim =
            shared::store_of(&[(HEAD, head.as_slice()), (shared::pack_file(0).as_str(), pack.as_slice())]);
        shared::read_back(&mut sim, &wants);
    } else {
        let (values, settles) = shared::values(&mut input);
        let mut sim = shared::write(&values, settles);
        let damaged = shared::damage_record(&mut sim, &mut input, form & 2 != 0, |_| true);
        let back = shared::read_back(&mut sim, &shared::wants(&values));
        if !damaged {
            shared::assert_whole(back, values.len());
        }
    }
}

/// A record of `key` as the input gives it, its payload, and whether its content is the
/// payload's hash.
fn record<'a>(input: &mut Input<'a>, key: Key) -> (Record, &'a [u8], bool) {
    let kind = input.u8();
    let form = input.u8();
    let given = input.u8();
    let dictionary = input.u32();
    let stored = (given & STORED_GIVEN != 0).then(|| input.u64());
    let raw = (given & RAW_GIVEN != 0).then(|| input.u64());
    let content = (given & CONTENT_GIVEN != 0).then(|| input.id());
    let len = usize::from(input.u16());
    let payload = input.bytes(len);
    let len = payload.len() as u64;
    let record = Record {
        kind,
        form,
        dictionary,
        stored: stored.unwrap_or(len),
        raw: raw.unwrap_or(len),
        key,
        content: content.unwrap_or_else(|| shared::content_of(payload)),
    };
    (record, payload, content.is_none())
}

/// The input `record` reads for a record of these fields; `None` for the payload's own.
fn encode(kind: u8, form: u8, dictionary: u32, stored: Option<u64>, raw: Option<u64>, payload: &[u8]) -> Vec<u8> {
    let given = u8::from(stored.is_some()) * STORED_GIVEN + u8::from(raw.is_some()) * RAW_GIVEN;
    let mut out = vec![kind, form, given];
    out.extend_from_slice(&dictionary.to_le_bytes());
    for it in [stored, raw].into_iter().flatten() {
        out.extend_from_slice(&it.to_le_bytes());
    }
    out.extend_from_slice(&(payload.len() as u16).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// A chunk reference: the chunk's key and length.
fn chunk(key: Key, len: u64) -> Vec<u8> {
    [&key.0.0[..], &len.to_le_bytes()].concat()
}

/// Records of every form, whole and naming lengths their bytes cannot hold; and the sample
/// values with one record's header or payload damaged, resealed and not.
fn seeds() -> Vec<Vec<u8>> {
    let query = Kind::Query.code();
    let mut out = vec![
        [&[0, 0][..], &encode(query, shared::RAW, 0, None, None, b"hello")].concat(),
        [&[0, 1][..], &encode(query, shared::RAW, 0, None, None, b"one"), &encode(query, shared::RAW, 0, None, None, b"")]
            .concat(),
        [&[0, 0][..], &encode(query, shared::RAW, 0, None, Some(6), b"hello")].concat(),
        [&[0, 0][..], &encode(query, 9, 0, None, None, b"no form")].concat(),
        [&[0, 0][..], &encode(9, shared::RAW, 0, None, None, b"no kind")].concat(),
        [&[0, 0][..], &encode(query, shared::ZSTD, shared::NO_DICTIONARY, None, Some(1 << 40), &[0x28, 0xb5, 0x2f, 0xfd])]
            .concat(),
        [&[0, 0][..], &encode(query, shared::LZ4, shared::NO_DICTIONARY, None, Some(1 << 40), &[0xf0; 16])].concat(),
        [&[0, 0][..], &encode(Kind::Output.code(), shared::CHUNKED, 0, None, Some(1 << 40), &chunk(shared::key(0), 1 << 40))]
            .concat(),
        [
            &[0, 0][..],
            &encode(
                Kind::Output.code(),
                shared::CHUNKED,
                0,
                None,
                Some(2),
                &[chunk(shared::key(0), u64::MAX), chunk(shared::key(1), 3)].concat(),
            ),
        ]
        .concat(),
        [&[0, 0][..], &encode(query, shared::RAW, 0, Some(u64::MAX - 0x5f), None, b"")].concat(),
        [&[0, 0][..], &encode(query, shared::EXTERNAL, 0, None, Some(1 << 20), &[0; 32])].concat(),
    ];
    for values in shared::sample_inputs() {
        out.push([&[1][..], values.as_slice()].concat());
        for damage in [
            &[0, 0, 1, 0x10, 0, 0, 0, 0, 0, 0, 0, 0, 0x40, 0, 0][..],
            &[0, 0, 1, 0x18, 0, 0, 0, 0, 0, 0, 0, 0, 0x01, 0, 0],
            &[1, 0, 5, 0x04, 0, 0, 0, 0x01, 0x01, 0, 0],
            &[0, 0, 0, 0x70, 0, 0, 0, 0x01],
            &[2, 0, 4, 0x60, 0, 0, 0, 0xff, 0xff],
        ] {
            out.push([&[1][..], values.as_slice(), damage].concat());
            out.push([&[3][..], values.as_slice(), damage].concat());
        }
    }
    out
}

#[cfg_attr(test, allow(dead_code))]
fn main() -> std::process::ExitCode {
    blaze_fuzzer::main(&TARGET)
}

#[cfg(test)]
mod tests {
    #[test]
    fn seeds() {
        blaze_fuzzer::seeds(&super::TARGET);
    }
}
