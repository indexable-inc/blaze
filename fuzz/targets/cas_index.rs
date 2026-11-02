//! The store's index table (`crates/blaze-cas/src/index.rs`, `Table::new`, `Table::get`,
//! `Table::iter`): mapped at open, its header and control column checked, then every lookup
//! and every scrub reads its slots. Fed through the public `Store` run in `blaze_core`'s
//! simulation.
//!
//! The store writes a table only once 2^16 records are not in one, too many for a run, so the
//! tables here are written by `cas_shared::index` as `index.rs` lays them out, and the head
//! naming one says its length, CRC-32C and count as the table's bytes give them: the damage
//! always reaches the table itself.
//!
//! The input's first byte says what the rest is. Even: the table's bytes, in slot 0 of a head
//! whose one pack (`cas_shared::lone_pack`) is indexed whole, so its record is found only
//! through the table; the pack's key and every key in a full slot are read. Odd: values put
//! through `Store::put`, flushed and settled (`cas_shared::values`), a table of every record of
//! their packs in place of the scan of them at open, then damage to the table.
//!
//! Invariants, each checked below (a violation panics, which the engine reports as a crash):
//! - opening, reading and scrubbing never panic or hang;
//! - no allocation is larger than `cas_shared::bound` of the bytes on disk;
//! - a store that did not open says why (`Store::broken`);
//! - a value read is of the key asked, hashes to its content, and for a value put is the bytes
//!   put: a slot naming another place gives `Corrupt` or no value, never other bytes;
//!   `Store::get` and `Reader::get` agree; a scrub finds every corrupt value a read finds;
//! - round trip: values put, found through an undamaged table, read back as they were put.

#![forbid(unsafe_code)]

use std::time::Duration;

use blaze_cas::Key;
use blaze_fuzzer::Target;

#[path = "cas_shared.rs"]
mod shared;

use shared::{HEAD, INDEX, Input};

const TARGET: Target = Target {
    name: "cas_index",
    check,
    seeds,
    dictionary: &[
        b"BLZINDX\x03",
        b"\x03\x00\x00\x00",
        b"\x00\x04\x00\x00\x00\x00\x00\x00",
        b"\x00\x08\x00\x00\x00\x00\x00\x00",
        b"\x80\x80\x80\x80\x80\x80\x80\x80",
        b"\x10\x00\x00\x00\x00\x00\x00\x00",
    ],
    max_len: 1 << 17,
    budget: Duration::from_secs(600),
};

fn check(data: &[u8]) {
    let mut input = Input::new(data);
    if input.u8() & 1 == 0 {
        let table = input.rest();
        let head = shared::head(Some(shared::IndexRef::of(table)), &[shared::lone_ref(shared::BLOCK as u64)], &[]);
        let pack = shared::lone_pack();
        let mut sim = shared::store_of(&[
            (HEAD, head.as_slice()),
            (shared::pack_file(0).as_str(), pack.as_slice()),
            (INDEX, table),
        ]);
        let mut wants: Vec<(Key, Option<&[u8]>)> = vec![(shared::lone_key(), Some(shared::LONE))];
        wants.extend(shared::index_keys(table).into_iter().filter(|it| *it != shared::lone_key()).map(|key| (key, None)));
        shared::read_back(&mut sim, &wants);
    } else {
        let (values, settles) = shared::values(&mut input);
        let mut sim = shared::write(&values, settles);
        let table = shared::index(&shared::slots_of_packs(&mut sim));
        let head = shared::file(HEAD);
        let mut whole = sim.contents(&head);
        if whole.is_empty() {
            assert!(values.is_empty(), "a flush of {} values put commits a head", values.len());
            shared::assert_whole(shared::read_back(&mut sim, &[]), 0);
            return;
        }
        shared::index_whole(&mut whole, &table);
        sim.change(&head, &whole);
        let index = shared::file(INDEX);
        sim.change(&index, &table);
        let damaged = shared::damage_file(&mut sim, &index, &mut input, |_, _| {});
        if damaged {
            shared::index_whole(&mut whole, &sim.contents(&index));
            sim.change(&head, &whole);
        }
        let back = shared::read_back(&mut sim, &shared::wants(&values));
        if !damaged {
            shared::assert_whole(back, values.len());
        }
    }
}

/// A table of the hand-written pack's one record; tables of the sample values' records; and
/// those with a slot's place, a control byte and the header's counts overwritten.
fn seeds() -> Vec<Vec<u8>> {
    let mut out = vec![[&[0][..], shared::index(&[shared::lone_slot()]).as_slice()].concat()];
    let elsewhere = shared::Slot { offset: 32, ..shared::lone_slot() };
    out.push([&[0][..], shared::index(&[elsewhere]).as_slice()].concat());
    for values in shared::sample_inputs() {
        let (written, settles) = shared::values(&mut Input::new(&values));
        let mut sim = shared::write(&written, settles);
        let table = shared::index(&shared::slots_of_packs(&mut sim));
        out.push([&[0][..], table.as_slice()].concat());
        out.push([&[1][..], values.as_slice()].concat());
        for damage in [
            &[0, 0x40, 0, 0, 0, 0x7f][..],
            &[1, 0x18, 0, 0, 0, 0x09, 0, 0, 0, 0, 0, 0, 0],
            &[1, 0x10, 0, 0, 0, 0x00, 0x08, 0, 0, 0, 0, 0, 0],
            &[4, 0x40, 0, 0, 0, 0x00, 0x04],
            &[2, 0x00, 0x01, 0, 0],
        ] {
            out.push([&[1][..], values.as_slice(), damage].concat());
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
