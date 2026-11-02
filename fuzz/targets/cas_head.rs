//! The store's head (`crates/blaze-cas/src/disk.rs`, `Head::decode`), the file `Store::open`
//! reads first and maps everything else by: its packs, how far the index covers each, its
//! dictionaries, its files of their own, its units. Fed through the public `Store` run in
//! `blaze_core`'s simulation.
//!
//! The input's first byte says what the rest is. Even: the head's bytes, beside a pack 0 of one
//! record written by hand (`cas_shared::lone_pack`), whose value is read. Odd: values put through
//! `Store::put`, flushed and settled (`cas_shared::values`), then damage to their head. Bit 1
//! reseals: the head's last four bytes become the CRC-32C of the rest, so the damage reaches
//! the checks behind it.
//!
//! Invariants, each checked below (a violation panics, which the engine reports as a crash):
//! - opening, reading and scrubbing never panic or hang;
//! - no allocation is larger than `cas_shared::bound` of the bytes on disk;
//! - a store that did not open says why (`Store::broken`);
//! - a value read is of the key asked, hashes to its content, and is the bytes put: a head that
//!   names other lengths gives `Corrupt` or no value, never other bytes; `Store::get` and
//!   `Reader::get` agree; a scrub finds every corrupt value a read finds;
//! - round trip: values put, with no damage, open and read back as they were put.

#![forbid(unsafe_code)]

use std::time::Duration;

use blaze_fuzzer::Target;

#[path = "cas_shared.rs"]
mod shared;

use shared::{HEAD, Input};

const TARGET: Target = Target {
    name: "cas_head",
    check,
    seeds,
    dictionary: &[
        b"BLZHEAD\x04",
        b"\x03\x00\x00\x00",
        b"\xff\x00\x00\x00\x00\x00\x00\x00",
        b"\x00\x10\x00\x00\x00\x00\x00\x00",
        b"\xff\x0f\x00\x00\x00\x00\x00\x00",
        b"\x01\x00\x00\x00",
    ],
    max_len: 1 << 12,
    budget: Duration::from_secs(300),
};

fn check(data: &[u8]) {
    let mut input = Input::new(data);
    let form = input.u8();
    let reseal = form & 2 != 0;
    if form & 1 == 0 {
        let mut head = input.rest().to_vec();
        if reseal {
            shared::reseal_head(&mut head);
        }
        let pack = shared::lone_pack();
        let mut sim =
            shared::store_of(&[(HEAD, head.as_slice()), (shared::pack_file(0).as_str(), pack.as_slice())]);
        shared::read_back(&mut sim, &[(shared::lone_key(), Some(shared::LONE))]);
    } else {
        let (values, settles) = shared::values(&mut input);
        let mut sim = shared::write(&values, settles);
        let damaged = shared::damage_file(&mut sim, &shared::file(HEAD), &mut input, |bytes, _| {
            if reseal {
                shared::reseal_head(bytes);
            }
        });
        let back = shared::read_back(&mut sim, &shared::wants(&values));
        if !damaged {
            shared::assert_whole(back, values.len());
        }
    }
}

/// The heads a store writes of the sample values; a head naming the hand-written pack whole,
/// unindexed and indexed; and the values with their head's fields overwritten and resealed.
fn seeds() -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for indexed in [0, shared::BLOCK as u64] {
        let head = shared::head(None, &[shared::lone_ref(indexed)], &[]);
        out.push([&[0][..], head.as_slice()].concat());
    }
    for values in shared::sample_inputs() {
        let (written, settles) = shared::values(&mut Input::new(&values));
        let sim = shared::write(&written, settles);
        let head = sim.contents(&shared::file(HEAD));
        out.push([&[0][..], head.as_slice()].concat());
        out.push([&[1][..], values.as_slice()].concat());
        for damage in [
            &[0, 0x58, 0, 0, 0, 0x01][..],
            &[1, 0x58, 0, 0, 0, 0xff, 0x0f, 0, 0, 0, 0, 0, 0],
            &[1, 0x50, 0, 0, 0, 0x00, 0x20, 0, 0, 0, 0, 0, 0],
            &[5, 0x38, 0, 0, 0, 0x02, 0, 0, 0],
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
