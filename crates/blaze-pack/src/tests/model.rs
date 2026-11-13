//! What the simulations run against: a seeded generator, a hash, records of an origin, a
//! body that can be cut, and a model of a store that keeps what is put visible at once and
//! durable only at a flush, loses what is not durable at a crash, and collects what no pin
//! names. Its oracle is in `put`: every record put is checked against the manifest.

use std::collections::{BTreeMap, BTreeSet};

use crate::{
    Hashes, Id, Manifest, Record, cbor,
    import::{Body, Store, Verified},
};

pub(super) struct Rng(u64);

impl Rng {
    pub(super) fn new(seed: u64) -> Rng {
        Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ 0x2545_f491_4f6c_dd1d)
    }

    pub(super) fn next(&mut self) -> u64 {
        // splitmix64
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// A number under `n`, which is not 0.
    pub(super) fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    pub(super) fn shuffle<T>(&mut self, items: &mut [T]) {
        for at in (1..items.len()).rev() {
            let with = self.below(at as u64 + 1) as usize;
            items.swap(at, with);
        }
    }
}

/// A 256-bit hash for the tests: four passes of a 64-bit mix under four seeds. Not BLAKE3
/// (the crate has no dependency; the driver hands the real one in): any change of a byte or
/// of the length changes it, which is all the checks under test ask of a hash.
pub(super) struct TestHashes;

fn mixed(domain: u64, bytes: &[u8]) -> Id {
    let mut out = [0u8; 32];
    for lane in 0..4u64 {
        let mut state = Rng::new(domain ^ lane.wrapping_mul(0x1234_5678_9abc_def1) ^ bytes.len() as u64);
        let mut acc = state.next();
        for byte in bytes {
            acc = (acc ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3).rotate_left(23) ^ state.next();
        }
        out[lane as usize * 8..][..8].copy_from_slice(&acc.to_le_bytes());
    }
    Id(out)
}

impl Hashes for TestHashes {
    fn content(&self, bytes: &[u8]) -> Id {
        mixed(1, bytes)
    }

    fn manifest(&self, bytes: &[u8]) -> Id {
        mixed(2, bytes)
    }
}

/// `count` records of an origin store, their keys distinct, each 8 to 300 bytes.
pub(super) fn origin(rng: &mut Rng, count: usize) -> Vec<(Record, Vec<u8>)> {
    let mut out: Vec<(Record, Vec<u8>)> = Vec::with_capacity(count);
    while out.len() < count {
        let mut key = [0u8; 32];
        for chunk in key.chunks_mut(8) {
            chunk.copy_from_slice(&rng.next().to_le_bytes());
        }
        let key = Id(key);
        if out.iter().any(|(it, _)| it.key == key) {
            continue;
        }
        let len = 8 + rng.below(293);
        let bytes: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
        let record = Record { key, kind: rng.below(5) as u8, len, content: TestHashes.content(&bytes) };
        out.push((record, bytes));
    }
    out
}

/// The body of `manifest`: its records' bytes, from `records`, in its order.
pub(super) fn packed(manifest: &Manifest, records: &[(Record, Vec<u8>)]) -> Vec<u8> {
    let mut body = Vec::new();
    for record in manifest.records() {
        let (_, bytes) = records.iter().find(|(it, _)| it.key == record.key).unwrap();
        body.extend_from_slice(bytes);
    }
    assert_eq!(body.len() as u64, manifest.body_len());
    body
}

/// A manifest's bytes written field by field as given, in the order given: what another
/// layout, a hand or an attacker writes.
pub(super) fn raw_manifest(version: u64, coding: u64, pin: &str, scope: &str, records: &[Record]) -> Vec<u8> {
    let mut out = Vec::new();
    cbor::array(&mut out, 6);
    cbor::text(&mut out, "blaze pack manifest");
    cbor::uint(&mut out, version);
    cbor::uint(&mut out, coding);
    cbor::text(&mut out, pin);
    cbor::text(&mut out, scope);
    cbor::array(&mut out, records.len() as u64);
    for record in records {
        cbor::array(&mut out, 4);
        cbor::bytes(&mut out, &record.key.0);
        cbor::uint(&mut out, u64::from(record.kind));
        cbor::uint(&mut out, record.len);
        cbor::bytes(&mut out, &record.content.0);
    }
    out
}

/// A body of which only the first `len` bytes arrived.
pub(super) struct Cut<'a> {
    body: &'a [u8],
}

impl<'a> Cut<'a> {
    pub(super) fn whole(body: &'a [u8]) -> Cut<'a> {
        Cut { body }
    }

    pub(super) fn at(body: &'a [u8], len: u64) -> Cut<'a> {
        Cut { body: &body[..len as usize] }
    }
}

impl Body for Cut<'_> {
    fn read(&mut self, at: u64, len: u64, into: &mut Vec<u8>) -> Result<(), String> {
        into.clear();
        let from = (at as usize).min(self.body.len());
        let to = ((at + len) as usize).min(self.body.len());
        into.extend_from_slice(&self.body[from..to]);
        Ok(())
    }
}

pub(super) struct Model {
    /// What a crash keeps.
    pub(super) durable: BTreeMap<Id, (u8, Vec<u8>)>,
    /// Put and readable, not yet flushed.
    pending: BTreeMap<Id, (u8, Vec<u8>)>,
    pub(super) pins: BTreeMap<String, BTreeSet<Id>>,
    pending_pins: BTreeMap<String, BTreeSet<Id>>,
    /// The manifest's content hash of each key: what the oracle holds a put to.
    truth: BTreeMap<Id, Id>,
    /// Records put whose bytes are not what the manifest says of their key.
    pub(super) violations: u64,
    pub(super) puts: u64,
    /// The operation (counted from 0) that fails, as a crash under way would.
    pub(super) fail_at: Option<u64>,
    ops: u64,
}

impl Model {
    pub(super) fn new(manifest: &Manifest) -> Model {
        Model {
            durable: BTreeMap::new(),
            pending: BTreeMap::new(),
            pins: BTreeMap::new(),
            pending_pins: BTreeMap::new(),
            truth: manifest.records().iter().map(|it| (it.key, it.content)).collect(),
            violations: 0,
            puts: 0,
            fail_at: None,
            ops: 0,
        }
    }

    fn op(&mut self) -> Result<(), String> {
        let at = self.ops;
        self.ops += 1;
        match self.fail_at == Some(at) {
            true => Err(format!("the store stopped at its operation {at}")),
            false => Ok(()),
        }
    }

    /// The process ends: what was not flushed is gone.
    pub(super) fn crash(&mut self) {
        self.pending.clear();
        self.pending_pins.clear();
        self.ops = 0;
    }

    /// Every durable record no pin names goes.
    pub(super) fn collect_unrooted(&mut self) {
        let pins = &self.pins;
        self.durable.retain(|key, _| pins.values().any(|keys| keys.contains(key)));
    }

    /// How many of `manifest`'s records a build would have to make.
    pub(super) fn missing(&self, manifest: &Manifest) -> usize {
        manifest
            .records()
            .iter()
            .filter(|it| !self.durable.contains_key(&it.key) && !self.pending.contains_key(&it.key))
            .count()
    }
}

impl Store for Model {
    fn look(&mut self, key: &Id) -> Result<Option<Id>, String> {
        self.op()?;
        let kept = self.pending.get(key).or_else(|| self.durable.get(key));
        Ok(kept.map(|(_, bytes)| TestHashes.content(bytes)))
    }

    fn put(&mut self, record: Verified<'_>) -> Result<(), String> {
        self.op()?;
        // The oracle: what becomes readable is what the manifest names.
        let honest = self.truth.get(&record.key()) == Some(&TestHashes.content(record.bytes()));
        self.violations += u64::from(!honest);
        self.puts += 1;
        self.pending.insert(record.key(), (record.kind(), record.bytes().to_vec()));
        Ok(())
    }

    fn pin(&mut self, name: &str, keys: &[Id]) -> Result<(), String> {
        self.op()?;
        self.pending_pins.entry(name.to_owned()).or_default().extend(keys.iter().copied());
        Ok(())
    }

    fn flush(&mut self) -> Result<(), String> {
        self.op()?;
        // One commit: the records and their pin together.
        self.durable.append(&mut self.pending);
        for (name, keys) in std::mem::take(&mut self.pending_pins) {
            self.pins.entry(name).or_default().extend(keys);
        }
        Ok(())
    }
}
