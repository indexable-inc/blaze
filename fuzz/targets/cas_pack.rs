//! The store's packs (`crates/blaze-cas/src/disk.rs`): what `Store::open` reads of them (the
//! pack header, each record header past what the index covers, the zeros that end a commit) and
//! what `Store::get`, `Reader::get` and `Store::verify` then read, through the public `Store`
//! run in `blaze_core`'s simulation.
//!
//! The input's first byte says what the rest is. Even: the bytes of the one pack a head names,
//! padded with zeros to a block, every key its record headers name read. Odd: values put through
//! `Store::put`, flushed and settled (`cas_shared::values`), then damage to one of their packs.
//! Bit 1 reseals: each record header the store would read gets its CRC-32C again, so the damage
//! reaches the checks behind the CRC.
//!
//! Invariants, each checked below (a violation panics, which the engine reports as a crash):
//! - opening, reading and scrubbing never panic or hang;
//! - no allocation is larger than `cas_shared::bound` of the bytes on disk: `blaze_fuzz::allowed`
//!   of them plus what their densest zstd frames decode to (a record names its own length);
//! - a store that did not open says why (`Store::broken`);
//! - a value read is of the key asked, hashes to its content, and for a value put is the bytes
//!   put: damage is `Corrupt`, never other bytes; `Store::get` and `Reader::get` agree; a scrub
//!   (`Store::verify`) finds every corrupt value a read finds;
//! - round trip: values put, with no damage, open and read back as they were put.

#![forbid(unsafe_code)]

use std::time::Duration;

use blaze_cas::Key;
use blaze_fuzzer::Target;

#[path = "cas_shared.rs"]
mod shared;

use shared::{HEAD, Input};

const TARGET: Target = Target {
    name: "cas_pack",
    check,
    seeds,
    dictionary: &[
        b"BLZPACK\x04",
        b"BLZR",
        b"\x00\x10\x00\x00\x00\x00\x00\x00",
        b"\xff\xff\xff\xff",
        b"\xa0\xff\xff\xff\xff\xff\xff\xff",
        b"\xff\xff\xff\xff\xff\xff\xff\xff",
        b"\x28\xb5\x2f\xfd",
    ],
    max_len: 1 << 16,
    budget: Duration::from_secs(600),
};

fn check(data: &[u8]) {
    let mut input = Input::new(data);
    let form = input.u8();
    let reseal = form & 2 != 0;
    if form & 1 == 0 {
        let mut pack = shared::padded(input.rest().to_vec());
        if reseal {
            let starts: Vec<usize> = shared::walk(&pack).iter().map(|(at, _)| *at).collect();
            shared::reseal_records(&mut pack, &starts);
        }
        let keys: Vec<Key> = shared::walk(&pack).iter().map(|(_, record)| record.key).collect();
        let head = shared::head(None, &[shared::PackRef { committed: pack.len() as u64, ..shared::lone_ref(0) }], &[]);
        let mut sim =
            shared::store_of(&[(HEAD, head.as_slice()), (shared::pack_file(0).as_str(), pack.as_slice())]);
        let wants: Vec<(Key, Option<&[u8]>)> = keys.iter().map(|key| (*key, None)).collect();
        shared::read_back(&mut sim, &wants);
    } else {
        let (values, settles) = shared::values(&mut input);
        let mut sim = shared::write(&values, settles);
        let damaged = shared::damage_packs(&mut sim, &mut input, reseal);
        let back = shared::read_back(&mut sim, &shared::wants(&values));
        if !damaged {
            shared::assert_whole(back, values.len());
        }
    }
}

/// The packs a store writes of the sample values, as packs and as values, resealed and not;
/// and the values with a byte of their pack flipped, a field overwritten, the pack cut short.
fn seeds() -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for values in shared::sample_inputs() {
        let (written, settles) = shared::values(&mut Input::new(&values));
        let mut sim = shared::write(&written, settles);
        for pack in shared::packs(&mut sim) {
            let bytes = sim.contents(&pack);
            out.push([&[0][..], bytes.as_slice()].concat());
            out.push([&[2][..], bytes.as_slice()].concat());
        }
        out.push([&[1][..], values.as_slice()].concat());
        for damage in [
            &[0, 0, 0x40, 0, 0, 0, 0x01][..],
            &[0, 1, 0x20, 0, 0, 0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0, 0],
            &[0, 2, 0x00, 0x08, 0, 0],
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
