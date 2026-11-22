//! For generated sequences of field lists, encode then decode is the identity, and the encoder's
//! and the decoder's dynamic tables stay equal after every block; table sizes, string policies,
//! sensitive fields and mid-sequence changes of the peer's maximum are all drawn.
//!
//! Prints blaze-sim's one pass line for `round_trip_is_the_identity`, and for the control
//! `the_oracle_catches_an_eviction_off_by_one` the plant's line ("plant EvictOneTooFew: caught at
//! case k of n"). The plant (hpack/table.rs `Table::insert`): the decoder's table evicts one entry
//! too few when an insertion makes room, so it holds one entry more than the encoder's and the
//! indices part ways; the oracle says "tables differ" or "fields differ".

use std::{format, vec::Vec as StdVec};

use blaze_own::AVec;
use blaze_sim::{Broke, Config, Tape, check, control};

use super::alloc;
use crate::{
    head::{Field, Fields},
    hpack::{Decoder, Encoder, Strings},
    plant::{self, Plant},
};

/// Names drawn from: static names (indexed by name), pseudo-headers, and new ones.
const NAMES: [&[u8]; 10] = [
    b":path",
    b":status",
    b"cache-control",
    b"content-type",
    b"date",
    b"x-a",
    b"x-record-digest",
    b"authorization",
    b"custom-key",
    b"user-agent",
];

/// Values drawn from: a few repeated ones (so full matches happen), and fresh ones.
const VALUES: [&[u8]; 5] = [b"", b"200", b"no-cache", b"application/octet-stream", b"gzip, deflate"];

struct Drawn {
    name: StdVec<u8>,
    value: StdVec<u8>,
    sensitive: bool,
}

fn draw_field(tape: &Tape) -> Drawn {
    let name = tape.pick(&NAMES, "name").copied().unwrap_or(b"x-a").to_vec();
    let value = if tape.coin("fresh value") {
        let len = tape.int_in(0usize..=300, "value len");
        // Visible ASCII and spaces inside: what a field value holds.
        (0..len).map(|i| b"abcdefghijklmnopqrstuvwxyz0123456789-_ /:;=,"[(i * 7 + len) % 44]).collect()
    } else {
        tape.pick(&VALUES, "value").copied().unwrap_or(b"").to_vec()
    };
    Drawn { name, value, sensitive: tape.one_in(6, "sensitive") }
}

fn round_trip(tape: &Tape) -> Result<(), Broke> {
    let sizes = [0u32, 33, 64, 256, 1024, 4096];
    let allowed = *tape.pick(&sizes, "decoder table").unwrap_or(&4096);
    let own = *tape.pick(&sizes, "encoder cap").unwrap_or(&4096);
    let strings = *tape.pick(&[Strings::Raw, Strings::Huffman, Strings::Shorter], "strings").unwrap_or(&Strings::Raw);
    let mut decoder = Decoder::new(allowed, alloc());
    let mut encoder = Encoder::new(own, strings, alloc());
    encoder.peer_allows(allowed);
    let blocks = tape.int_in(1usize..=12, "blocks");
    for k in 0..blocks {
        if tape.one_in(5, "peer changes its maximum") {
            let to = *tape.pick(&sizes, "new maximum").unwrap_or(&0);
            encoder.peer_allows(to.min(allowed));
        }
        let drawn = tape.vec(0..=20, "fields", draw_field);
        let fields: StdVec<Field<'_>> =
            drawn.iter().map(|d| Field { name: &d.name, value: &d.value, sensitive: d.sensitive }).collect();
        let mut block = AVec::new_in(alloc());
        encoder.encode(fields.iter().copied(), &mut block).map_err(|e| Broke::Harness(format!("encode: {e}")))?;
        let mut got = Fields::new_in(alloc());
        decoder
            .decode(block.as_slice(), &mut got, u32::MAX)
            .map_err(|e| Broke::Oracle(format!("fields differ: block {k} refused: {e:?}")))?;
        let got: StdVec<Field<'_>> = got.iter().collect();
        if got != fields {
            return Err(Broke::Oracle(format!("fields differ in block {k}: {} decoded, {} sent", got.len(), fields.len())));
        }
        let enc: StdVec<_> = encoder.table().iter().collect();
        let dec: StdVec<_> = decoder.table().iter().collect();
        if enc != dec || encoder.table().size() != decoder.table().size() {
            return Err(Broke::Oracle(format!(
                "tables differ after block {k}: encoder {} entries ({} bytes), decoder {} ({} bytes)",
                enc.len(),
                encoder.table().size(),
                dec.len(),
                decoder.table().size()
            )));
        }
        if decoder.table().size() > u64::from(decoder.table().max()) {
            return Err(Broke::Oracle(format!("tables differ from their maximum after block {k}")));
        }
    }
    Ok(())
}

/// The invariant: HPACK encode followed by decode is the identity across arbitrary header lists and dynamic table resizes, and encoder and decoder dynamic tables stay identical after every block.
#[test]
fn round_trip_is_the_identity() {
    check(Config::cases(200), round_trip);
}

#[test]
fn the_oracle_catches_an_eviction_off_by_one() {
    control(Config::cases(200), Plant::Clean, Plant::EvictOneTooFew, "differ", |tape, p| {
        plant::with(p, || round_trip(tape))
    });
}
