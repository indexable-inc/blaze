//! The store's cold tier (`crates/blaze-cas/src/cold.rs`, what `read.rs` hands it): zstd
//! frames with and without a kind's trained dictionary, LZ4 blocks, and the dictionary records
//! a head names, which `Store::open` loads. Read through the public `Store::get`,
//! `Reader::get` and `Store::verify` in `blaze_core`'s simulation.
//!
//! The input's first byte says what the rest is. Even: a value record of zstd or LZ4 bytes
//! the input gives, with the raw length and content hash it names, after a dictionary record of
//! bytes the input gives where it says so, in the one pack a head names (with the dictionary
//! where it says so). Odd: values put through `Store::put`, flushed and settled
//! (`cas_shared::values`; three settles move the long ones to zstd), then damage in place to
//! one of their compressed records or dictionaries; bit 1 reseals that record's header.
//!
//! Invariants, each checked below (a violation panics, which the engine reports as a crash):
//! - opening, reading and scrubbing never panic or hang;
//! - no allocation is larger than `cas_shared::bound` of the bytes on disk: a raw length the
//!   frame cannot decode to is `Corrupt`, not room made for it;
//! - a store that did not open says why (`Store::broken`);
//! - a value read is of the key asked and hashes to its content, and for a value put is the
//!   bytes put: a damaged frame or dictionary is `Corrupt`, never other bytes; `Store::get` and
//!   `Reader::get` agree; a scrub finds every corrupt value a read finds;
//! - round trip: values put, compressed, with no damage, read back as they were put.

#![forbid(unsafe_code)]

use std::time::Duration;

use blaze_cas::{Kind, Key};
use blaze_fuzzer::Target;
use blaze_ids::ContentId;

#[path = "cas_shared.rs"]
mod shared;

use shared::{HEAD, Input, Record, Value};

const TARGET: Target = Target {
    name: "cas_cold",
    check,
    seeds,
    dictionary: &[
        b"\x28\xb5\x2f\xfd",
        b"\x37\xa4\x30\xec",
        b"\xff\xff\xff\xff",
        b"\x00\x00\x00\x00\x00\x01\x00\x00",
        b"\x00\x00\x00\x00\x00\x00\x00\x40",
    ],
    max_len: 1 << 17,
    budget: Duration::from_secs(600),
};

/// The pack holds a dictionary record and the head names it.
const WITH_DICTIONARY: u8 = 1;
/// The value is an LZ4 block, not a zstd frame.
const LZ4: u8 = 2;
/// The value's header names dictionary 0, not none.
const NAMES_DICTIONARY: u8 = 4;

fn check(data: &[u8]) {
    let mut input = Input::new(data);
    let form = input.u8();
    if form & 1 == 0 {
        let kind = input.u8();
        let flags = input.u8();
        let dictionary = match flags & WITH_DICTIONARY != 0 {
            true => {
                let len = input.u32() as usize;
                Some(input.bytes(len))
            }
            false => None,
        };
        let raw = input.u64();
        let content = input.id();
        let payload = input.rest();
        let mut pack = shared::pack_header(0).to_vec();
        let mut dictionaries = Vec::new();
        if let Some(bytes) = dictionary {
            let record = Record {
                kind,
                form: shared::DICTIONARY,
                dictionary: 0,
                stored: bytes.len() as u64,
                raw: bytes.len() as u64,
                key: Key(ContentId::of(shared::DICTIONARY_DOMAIN, bytes)),
                content: shared::content_of(bytes),
            };
            let offset = shared::append_record(&mut pack, &record, bytes);
            dictionaries.push(shared::DictionaryRef { kind, pack: 0, offset: offset as u64 });
        }
        let value = Record {
            kind,
            form: if flags & LZ4 != 0 { shared::LZ4 } else { shared::ZSTD },
            dictionary: if flags & NAMES_DICTIONARY != 0 { 0 } else { shared::NO_DICTIONARY },
            stored: payload.len() as u64,
            raw,
            key: shared::key(0),
            content,
        };
        shared::append_record(&mut pack, &value, payload);
        let pack = shared::padded(pack);
        let head = shared::head(
            None,
            &[shared::PackRef { committed: pack.len() as u64, ..shared::lone_ref(0) }],
            &dictionaries,
        );
        let mut sim =
            shared::store_of(&[(HEAD, head.as_slice()), (shared::pack_file(0).as_str(), pack.as_slice())]);
        shared::read_back(&mut sim, &[(value.key, None)]);
    } else {
        let (values, settles) = shared::values(&mut input);
        let mut sim = shared::write(&values, settles);
        let damaged = shared::damage_record(&mut sim, &mut input, form & 2 != 0, |it| {
            [shared::ZSTD, shared::LZ4, shared::DICTIONARY].contains(&it.form)
        });
        let back = shared::read_back(&mut sim, &shared::wants(&values));
        if !damaged {
            shared::assert_whole(back, values.len());
        }
    }
}

/// The input `check` reads for `value`, after `dictionary` when there is one.
fn encode(dictionary: Option<&[u8]>, value: &Record, payload: &[u8]) -> Vec<u8> {
    let mut flags = 0;
    if dictionary.is_some() {
        flags |= WITH_DICTIONARY;
    }
    if value.form == shared::LZ4 {
        flags |= LZ4;
    }
    if value.dictionary != shared::NO_DICTIONARY {
        flags |= NAMES_DICTIONARY;
    }
    let mut out = vec![0, value.kind, flags];
    if let Some(bytes) = dictionary {
        out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(bytes);
    }
    out.extend_from_slice(&value.raw.to_le_bytes());
    out.extend_from_slice(&value.content.0);
    out.extend_from_slice(payload);
    out
}

/// The compressed records of a store: each value record's header and payload, and the
/// dictionary record's payload, where there is one.
fn compressed(values: &[Value], settles: u8) -> (Vec<(Record, Vec<u8>)>, Option<Vec<u8>>) {
    let mut sim = shared::write(values, settles);
    let (mut records, mut dictionary) = (Vec::new(), None);
    for pack in shared::packs(&mut sim) {
        let bytes = sim.contents(&pack);
        for (at, record) in shared::walk(&bytes) {
            let start = at + shared::RECORD_HEADER;
            let payload = || bytes[start..start + record.stored as usize].to_vec();
            match record.form {
                shared::DICTIONARY => dictionary = Some(payload()),
                shared::ZSTD | shared::LZ4 => records.push((record, payload())),
                _ => {}
            }
        }
    }
    (records, dictionary)
}

/// What the store compresses: long values it moves to zstd, an output's LZ4 chunks, debug
/// values compressed with the dictionary trained on them, as records and as values damaged;
/// and frames naming more bytes than they hold.
fn seeds() -> Vec<Vec<u8>> {
    let zstd_frame = [0x28, 0xb5, 0x2f, 0xfd, 0x00, 0x00, 0x01, 0x00, 0x00];
    let mut out = vec![
        encode(None, &cold(Kind::Debug, shared::ZSTD, shared::NO_DICTIONARY, 1 << 40), &zstd_frame),
        encode(None, &cold(Kind::Output, shared::LZ4, shared::NO_DICTIONARY, 1 << 40), &[0xf0; 16]),
        encode(Some(b"not a dictionary"), &cold(Kind::Debug, shared::ZSTD, 0, 64), &zstd_frame),
        encode(None, &cold(Kind::Debug, shared::ZSTD, 0, 64), &zstd_frame),
    ];
    for values in shared::sample_inputs() {
        let (written, settles) = shared::values(&mut Input::new(&values));
        let (records, _) = compressed(&written, settles);
        for (record, payload) in &records {
            out.push(encode(None, record, payload));
        }
        out.push([&[1][..], values.as_slice()].concat());
        for damage in [
            &[0, 0, 0, 0x70, 0, 0, 0, 0x01][..],
            &[0, 0, 1, 0x18, 0, 0, 0, 0, 0, 0, 0, 0, 0x01, 0, 0],
            &[0, 0, 4, 0x64, 0, 0, 0, 0x10, 0],
        ] {
            out.push([&[1][..], values.as_slice(), damage].concat());
            out.push([&[3][..], values.as_slice(), damage].concat());
        }
    }
    let debug: Vec<Value> = (0..300)
        .map(|n| Value {
            key: shared::key(n),
            kind: Kind::Debug,
            bytes: format!("DW_TAG_subprogram name=f{n} src/lib.rs:{n}\n").repeat(200).into_bytes(),
        })
        .collect();
    let (records, dictionary) = compressed(&debug, 5);
    let trained: Vec<&(Record, Vec<u8>)> =
        records.iter().filter(|(it, _)| it.dictionary != shared::NO_DICTIONARY).take(4).collect();
    for (record, payload) in trained {
        out.push(encode(dictionary.as_deref(), record, payload));
    }
    out
}

/// A compressed value record of key 0 naming `raw` bytes, its content no hash of them.
fn cold(kind: Kind, form: u8, dictionary: u32, raw: u64) -> Record {
    Record { kind: kind.code(), form, dictionary, stored: 0, raw, key: shared::key(0), content: ContentId([0; 32]) }
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
