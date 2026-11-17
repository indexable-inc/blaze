//! `Lent`'s one promise (lent.rs, ruling 6): bytes handed out early stay byte-equal AND at the
//! same address after later gets that map more segments and after puts that add chunks. The
//! planted control is the arena as one growing buffer (`LentPlant::OneBuffer`): its first
//! reallocation moves what it lent, and the address check catches the move. The planted run
//! compares addresses only (a moved lend is never read).

use std::{alloc::Global, path::Path};

use blaze_core::{
    FileName,
    sim::{Faults, Sim},
};
use blaze_ids::ContentId;
use blaze_keyed::kind::{Namespace, Tag};

use super::super::{
    core::{PlainConfig, TAGS},
    driven::Driven,
};
use super::{Lent, LentPlant};
use crate::Key;

const TAG: Tag = Tag(1);

fn cfg(n: u64) -> PlainConfig {
    let mut writes = Vec::new_in(Global);
    writes.push(Namespace::LOCAL);
    let mut reads = Vec::new_in(Global);
    reads.push(Namespace::LOCAL);
    PlainConfig {
        dir: FileName::new("plain").expect("a name"),
        tags: n * TAGS,
        cap_bytes: u64::MAX,
        boot: 1,
        nonce: 700 + n,
        durable: false,
        machine: Vec::new_in(Global),
        writes,
        reads,
    }
}

fn key(n: u32) -> Key {
    Key(ContentId::of("plain lent test key", &n.to_le_bytes()))
}

fn value(n: u32) -> Vec<u8> {
    (0..40 + n as usize % 300).map(|i| (n as usize).wrapping_mul(7).wrapping_add(i) as u8).collect()
}

/// Puts larger than a chunk: each takes a chunk of its own.
fn big(n: u32) -> Vec<u8> {
    vec![n as u8; (1 << 20) + 16]
}

/// Three committed segments (keys 0..10, 10..30, 30..60) and a lazy store over them, in a Lent.
fn lent(seed: u64, plant: LentPlant) -> Lent<Sim, Global> {
    let mut w = Driven::with(Sim::new(seed, Faults::default()), Path::new("/sim"), cfg(0)).expect("the writer opens");
    for range in [0..10u32, 10..30, 30..60] {
        for n in range {
            w.put(key(n), Namespace::LOCAL, TAG, &value(n)).unwrap();
        }
        w.commit_with(&[]).unwrap();
    }
    let r = Driven::with(w.into_driver(), Path::new("/sim"), cfg(1)).expect("the reader opens");
    Lent::planted(r, Global, plant)
}

/// The address of `n`'s bytes as `lent` lends them now.
fn address(lent: &Lent<Sim, Global>, k: &Key) -> usize {
    lent.get(k).unwrap().expect("held").bytes.as_ptr() as usize
}

#[test]
fn lends_keep_their_address_and_bytes_across_maps_and_chunks() {
    let lent = lent(21, LentPlant::None);
    // Early lends: a small own put and a record of one segment.
    lent.put(&key(1000), Namespace::LOCAL, TAG, &value(1000)).unwrap();
    let mut early = Vec::new();
    early.push((key(1000), lent.get(&key(1000)).unwrap().unwrap(), value(1000)));
    early.push((key(15), lent.get(&key(15)).unwrap().unwrap(), value(15)));
    let addresses: Vec<usize> = early.iter().map(|(_, lend, _)| lend.bytes.as_ptr() as usize).collect();
    // Later puts that add chunks, and gets that map the other segments.
    for n in 0..5 {
        lent.put(&key(2000 + n), Namespace::LOCAL, TAG, &big(n)).unwrap();
        early.push((key(2000 + n), lent.get(&key(2000 + n)).unwrap().unwrap(), big(n)));
    }
    for n in (0..10).chain(30..60) {
        early.push((key(n), lent.get(&key(n)).unwrap().unwrap(), value(n)));
    }
    let (mapped, named) = lent.mapped_of_named();
    assert_eq!((mapped, named), (3, 3));
    assert!(lent.counts().chunks >= 5, "the puts added fewer chunks than the test needs: {:?}", lent.counts());
    for (i, (k, lend, want)) in early.iter().enumerate() {
        assert_eq!(lend.bytes, &want[..], "lend {i} changed its bytes");
        assert_eq!(address(&lent, k), lend.bytes.as_ptr() as usize, "lend {i} moved");
    }
    assert_eq!(addresses, early[..2].iter().map(|(_, lend, _)| lend.bytes.as_ptr() as usize).collect::<Vec<_>>());
    println!("lent: {} lends kept their address and bytes across {mapped} maps and {} chunks", early.len(), lent.counts().chunks);
}

#[test]
fn the_planted_one_buffer_arena_moves_a_lend() {
    let lent = lent(22, LentPlant::OneBuffer);
    lent.put(&key(1000), Namespace::LOCAL, TAG, &value(1000)).unwrap();
    let first = address(&lent, &key(1000));
    for n in 0..5 {
        lent.put(&key(2000 + n), Namespace::LOCAL, TAG, &big(n)).unwrap();
    }
    // Addresses only: the planted arena's first lend may be freed memory now.
    let now = address(&lent, &key(1000));
    assert_ne!(first, now, "the one growing buffer did not move: the plant was not exercised");
    println!("lent, planted OneBuffer: caught (a lent address moved)");
}
