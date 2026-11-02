//! What the `cas_*` targets share. It is a module of each (`#[path = "cas_shared.rs"] mod
//! shared;`), not a target of its own.
//!
//! - The store runs in `blaze_core`'s simulation: the files it reads are in memory, written by
//!   the store itself (`write`) or by hand (`store_of`), and nothing is left between runs.
//! - The bytes a target writes by hand are laid out as `crates/blaze-cas/src/disk.rs` and
//!   `index.rs` lay them out: packs, record headers, the head and the index table. The crate
//!   keeps its encoders to itself, so these are this module's own, checked against the store by
//!   the round trips (`read_back` of what they wrote, undamaged, gives back every value).
//! - `read_back` is the one check of a read every store target makes.

#![allow(dead_code)]

use std::{sync::OnceLock, time::Duration};

use blaze_cas::{Config, Key, Kind, Store};
use blaze_core::{
    Done, Effect, FileName, Tag, run_until_idle,
    sim::{Faults, Sim},
};
use blaze_ids::ContentId;

// The input.

/// The input of one run, read front to back; past its end every number is zero.
pub(crate) struct Input<'a>(&'a [u8]);

impl<'a> Input<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Input<'a> {
        Input(bytes)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The next `len` bytes, or what is left.
    pub(crate) fn bytes(&mut self, len: usize) -> &'a [u8] {
        let (taken, rest) = self.0.split_at(len.min(self.0.len()));
        self.0 = rest;
        taken
    }

    pub(crate) fn rest(&mut self) -> &'a [u8] {
        std::mem::take(&mut self.0)
    }

    fn word<const N: usize>(&mut self) -> [u8; N] {
        let mut out = [0; N];
        let taken = self.bytes(N);
        out[..taken.len()].copy_from_slice(taken);
        out
    }

    pub(crate) fn u8(&mut self) -> u8 {
        self.word::<1>()[0]
    }

    pub(crate) fn u16(&mut self) -> u16 {
        u16::from_le_bytes(self.word())
    }

    pub(crate) fn u32(&mut self) -> u32 {
        u32::from_le_bytes(self.word())
    }

    pub(crate) fn u64(&mut self) -> u64 {
        u64::from_le_bytes(self.word())
    }

    pub(crate) fn id(&mut self) -> ContentId {
        ContentId(self.word())
    }
}

// The bytes on disk, as `disk.rs` and `index.rs` lay them out.

/// `crate::FORMAT` of blaze-cas: the head and the index table carry it.
pub(crate) const FORMAT: u32 = 3;
pub(crate) const BLOCK: usize = 4096;
pub(crate) const PACK_HEADER: usize = 16;
pub(crate) const RECORD_HEADER: usize = 96;
pub(crate) const NO_DICTIONARY: u32 = u32::MAX;
pub(crate) const PACK_MAGIC: &[u8; 8] = b"BLZPACK\x04";
pub(crate) const RECORD_MAGIC: &[u8; 4] = b"BLZR";
pub(crate) const HEAD_MAGIC: &[u8; 8] = b"BLZHEAD\x04";
pub(crate) const INDEX_MAGIC: &[u8; 8] = b"BLZINDX\x03";
pub(crate) const VALUE_DOMAIN: &str = "blaze v2 store value 1";
pub(crate) const DICTIONARY_DOMAIN: &str = "blaze v2 store dictionary 1";

/// `Form::code` of each of a record's forms.
pub(crate) const RAW: u8 = 0;
pub(crate) const ZSTD: u8 = 1;
pub(crate) const CHUNKED: u8 = 2;
pub(crate) const EXTERNAL: u8 = 3;
pub(crate) const DICTIONARY: u8 = 4;
pub(crate) const LZ4: u8 = 5;

/// A chunk in a chunked value's payload: its key, then its length.
pub(crate) const CHUNK_REF: usize = 40;

/// The head's fixed part, and where its fields are.
pub(crate) const HEAD_FIXED: usize = 72;
const HEAD_INDEX_SLOT: usize = 24;
const HEAD_INDEX_LEN: usize = 32;
const HEAD_INDEX_CHECK: usize = 40;
const HEAD_INDEX_COUNT: usize = 48;
const HEAD_PACKS: usize = 56;
const PACK_REF: usize = 32;

const INDEX_HEADER: usize = 64;
const INDEX_ROW: usize = 64;
const INDEX_GROUP: usize = 16;
const INDEX_EMPTY: u8 = 0x80;
const INDEX_LEAST_SLOTS: usize = 1024;

const CRC_TABLE: [u32; 256] = crc_table();

const fn crc_table() -> [u32; 256] {
    let mut out = [0; 256];
    let mut byte = 0;
    while byte < 256 {
        let mut crc = byte as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 == 1 { (crc >> 1) ^ 0x82F6_3B78 } else { crc >> 1 };
            bit += 1;
        }
        out[byte] = crc;
        byte += 1;
    }
    out
}

/// CRC-32C (Castagnoli), the check of record headers, the head and the index table.
pub(crate) fn crc32c(bytes: &[u8]) -> u32 {
    !bytes.iter().fold(!0u32, |crc, &byte| (crc >> 8) ^ CRC_TABLE[((crc ^ u32::from(byte)) & 0xff) as usize])
}

pub(crate) fn content_of(bytes: &[u8]) -> ContentId {
    ContentId::of(VALUE_DOMAIN, bytes)
}

pub(crate) const fn aligned(n: usize) -> usize {
    (n + 15) & !15
}

pub(crate) const fn blocks(n: usize) -> usize {
    n.div_ceil(BLOCK) * BLOCK
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four bytes"))
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().expect("eight bytes"))
}

fn put_u64(bytes: &mut [u8], at: usize, value: u64) {
    bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

pub(crate) fn pack_header(unit: u32) -> [u8; PACK_HEADER] {
    let mut out = [0; PACK_HEADER];
    out[..8].copy_from_slice(PACK_MAGIC);
    out[8..12].copy_from_slice(&unit.to_le_bytes());
    out
}

/// A record's header as its fields; `encode` gives it its check.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Record {
    pub(crate) kind: u8,
    pub(crate) form: u8,
    pub(crate) dictionary: u32,
    pub(crate) stored: u64,
    pub(crate) raw: u64,
    pub(crate) key: Key,
    pub(crate) content: ContentId,
}

impl Record {
    /// The record of `bytes` kept as they are.
    pub(crate) fn raw_value(key: Key, kind: Kind, bytes: &[u8]) -> Record {
        let len = bytes.len() as u64;
        Record { kind: kind.code(), form: RAW, dictionary: 0, stored: len, raw: len, key, content: content_of(bytes) }
    }

    pub(crate) fn encode(&self) -> [u8; RECORD_HEADER] {
        let mut out = [0; RECORD_HEADER];
        out[..4].copy_from_slice(RECORD_MAGIC);
        out[4] = self.kind;
        out[5] = self.form;
        out[8..12].copy_from_slice(&self.dictionary.to_le_bytes());
        put_u64(&mut out, 16, self.stored);
        put_u64(&mut out, 24, self.raw);
        out[32..64].copy_from_slice(&self.key.0.0);
        out[64..96].copy_from_slice(&self.content.0);
        seal_record(&mut out);
        out
    }

    /// The fields of a header with a record's magic, its check not looked at.
    pub(crate) fn decode(bytes: &[u8]) -> Option<Record> {
        let bytes = bytes.get(..RECORD_HEADER)?;
        (&bytes[..4] == RECORD_MAGIC).then(|| Record {
            kind: bytes[4],
            form: bytes[5],
            dictionary: u32_at(bytes, 8),
            stored: u64_at(bytes, 16),
            raw: u64_at(bytes, 24),
            key: Key(ContentId(bytes[32..64].try_into().expect("32 bytes"))),
            content: ContentId(bytes[64..96].try_into().expect("32 bytes")),
        })
    }
}

/// Gives a record header its CRC-32C, taken with the check's own bytes zero.
fn seal_record(header: &mut [u8]) {
    header[12..16].fill(0);
    let check = crc32c(&header[..RECORD_HEADER]);
    header[12..16].copy_from_slice(&check.to_le_bytes());
}

/// Appends a record (header, `payload`, zeros to the next 16 bytes) to `pack`; where it starts.
pub(crate) fn append_record(pack: &mut Vec<u8>, record: &Record, payload: &[u8]) -> usize {
    let at = pack.len();
    pack.extend_from_slice(&record.encode());
    pack.extend_from_slice(payload);
    pack.resize(at + aligned(RECORD_HEADER + payload.len()), 0);
    at
}

/// `pack` with zeros to a whole number of blocks, at least one, as every commit leaves a pack.
pub(crate) fn padded(mut pack: Vec<u8>) -> Vec<u8> {
    let len = blocks(pack.len()).max(BLOCK);
    pack.resize(len, 0);
    pack
}

/// The records of `pack` and where each starts, found as the store finds them at open: from its
/// header on, each record's span on, a run of zeros to the next block. It stops where the bytes
/// are neither, and after a record whose span does not fit in 64 bits.
pub(crate) fn walk(pack: &[u8]) -> Vec<(usize, Record)> {
    let mut out = Vec::new();
    let mut at = PACK_HEADER;
    while at + 4 <= pack.len() {
        if pack[at..at + 4] == [0; 4] {
            let next = blocks(at);
            if next == at {
                break;
            }
            at = next;
            continue;
        }
        let Some(record) = Record::decode(&pack[at..]) else { break };
        out.push((at, record));
        let span = (RECORD_HEADER as u64).checked_add(record.stored).and_then(|it| it.checked_add(15));
        let Some(end) = span.and_then(|it| usize::try_from(it & !15).ok()).and_then(|it| at.checked_add(it))
        else {
            break;
        };
        at = end;
    }
    out
}

/// Gives every record header that starts at one of `starts` its CRC-32C again.
pub(crate) fn reseal_records(pack: &mut [u8], starts: &[usize]) {
    for &at in starts {
        if let Some(header) = pack.get_mut(at..at.saturating_add(RECORD_HEADER)) {
            seal_record(header);
        }
    }
}

/// Gives a head its CRC-32C again: the last four bytes check the rest.
pub(crate) fn reseal_head(head: &mut [u8]) {
    if let Some(body) = head.len().checked_sub(4) {
        let check = crc32c(&head[..body]);
        head[body..].copy_from_slice(&check.to_le_bytes());
    }
}

pub(crate) struct PackRef {
    pub(crate) removed: bool,
    pub(crate) unit: u32,
    pub(crate) committed: u64,
    pub(crate) indexed: u64,
    pub(crate) dead: u64,
}

pub(crate) struct IndexRef {
    pub(crate) slot: u8,
    pub(crate) len: u64,
    pub(crate) check: u32,
    pub(crate) count: u64,
}

impl IndexRef {
    /// What a head says of `table` in slot 0, its count read from the table's own header so the
    /// two agree whatever the table holds.
    pub(crate) fn of(table: &[u8]) -> IndexRef {
        let count = table.get(24..32).map_or(0, |it| u64_at(it, 0));
        IndexRef { slot: 0, len: table.len() as u64, check: crc32c(table), count }
    }
}

pub(crate) struct DictionaryRef {
    pub(crate) kind: u8,
    pub(crate) pack: u32,
    pub(crate) offset: u64,
}

/// A head naming `packs`, `dictionaries` and the index table `index`, with no files of their
/// own and no units.
pub(crate) fn head(index: Option<IndexRef>, packs: &[PackRef], dictionaries: &[DictionaryRef]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEAD_FIXED + packs.len() * PACK_REF + dictionaries.len() * 16 + 4);
    out.extend_from_slice(HEAD_MAGIC);
    out.extend_from_slice(&FORMAT.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&1u64.to_le_bytes());
    let index = index.unwrap_or(IndexRef { slot: u8::MAX, len: 0, check: 0, count: 0 });
    out.extend_from_slice(&u64::from(index.slot).to_le_bytes());
    out.extend_from_slice(&index.len.to_le_bytes());
    out.extend_from_slice(&index.check.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&index.count.to_le_bytes());
    for count in [packs.len(), dictionaries.len(), 0, 0] {
        out.extend_from_slice(&(count as u32).to_le_bytes());
    }
    for pack in packs {
        out.push(u8::from(pack.removed));
        out.extend_from_slice(&[0; 3]);
        out.extend_from_slice(&pack.unit.to_le_bytes());
        out.extend_from_slice(&pack.committed.to_le_bytes());
        out.extend_from_slice(&pack.indexed.to_le_bytes());
        out.extend_from_slice(&pack.dead.to_le_bytes());
    }
    for dictionary in dictionaries {
        out.push(dictionary.kind);
        out.extend_from_slice(&[0; 3]);
        out.extend_from_slice(&dictionary.pack.to_le_bytes());
        out.extend_from_slice(&dictionary.offset.to_le_bytes());
    }
    let check = crc32c(&out);
    out.extend_from_slice(&check.to_le_bytes());
    out
}

/// `head` (a store's own, whole) naming `table` in index slot 0 and each of its packs indexed
/// to its end, so its records are found through the table alone.
pub(crate) fn index_whole(head: &mut [u8], table: &[u8]) {
    let index = IndexRef::of(table);
    put_u64(head, HEAD_INDEX_SLOT, u64::from(index.slot));
    put_u64(head, HEAD_INDEX_LEN, index.len);
    head[HEAD_INDEX_CHECK..HEAD_INDEX_CHECK + 4].copy_from_slice(&index.check.to_le_bytes());
    put_u64(head, HEAD_INDEX_COUNT, index.count);
    for pack in 0..u32_at(head, HEAD_PACKS) as usize {
        let at = HEAD_FIXED + pack * PACK_REF;
        let committed = u64_at(head, at + 8);
        put_u64(head, at + 16, committed);
    }
    reseal_head(head);
}

/// One slot of an index table: a key and its record's place.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Slot {
    pub(crate) key: Key,
    pub(crate) pack: u32,
    pub(crate) kind: u8,
    pub(crate) form: u8,
    pub(crate) offset: u64,
    pub(crate) stored: u64,
    pub(crate) raw: u64,
}

impl Slot {
    pub(crate) fn of(pack: u32, offset: usize, record: &Record) -> Slot {
        Slot {
            key: record.key,
            pack,
            kind: record.kind,
            form: record.form,
            offset: offset as u64,
            stored: record.stored,
            raw: record.raw,
        }
    }
}

/// An index table of `slots`, each key once: SwissTable groups of 16 slots probed at
/// triangular steps from the group the key's first eight bytes name, a slot's control byte its
/// key's ninth byte's low seven bits, at most 7/8 of the slots full.
pub(crate) fn index(slots: &[Slot]) -> Vec<u8> {
    let mut count = INDEX_LEAST_SLOTS;
    while slots.len() * 8 > count * 7 {
        count *= 2;
    }
    let mut out = vec![0; INDEX_HEADER + count * (1 + INDEX_ROW)];
    out[..8].copy_from_slice(INDEX_MAGIC);
    out[8..12].copy_from_slice(&FORMAT.to_le_bytes());
    put_u64(&mut out, 16, count as u64);
    put_u64(&mut out, 24, slots.len() as u64);
    let (control, rows) = out[INDEX_HEADER..].split_at_mut(count);
    control.fill(INDEX_EMPTY);
    let mask = (count / INDEX_GROUP - 1) as u64;
    for slot in slots {
        let mut group = u64_at(&slot.key.0.0, 0) & mask;
        let mut step = 1;
        let at = loop {
            let first = group as usize * INDEX_GROUP;
            if let Some(lane) = (0..INDEX_GROUP).find(|&lane| control[first + lane] == INDEX_EMPTY) {
                break first + lane;
            }
            group = (group + step) & mask;
            step += 1;
        };
        control[at] = slot.key.0.0[8] & 0x7f;
        let row = &mut rows[at * INDEX_ROW..(at + 1) * INDEX_ROW];
        row[..32].copy_from_slice(&slot.key.0.0);
        row[32..36].copy_from_slice(&slot.pack.to_le_bytes());
        row[36] = slot.kind;
        row[37] = slot.form;
        put_u64(row, 40, slot.offset);
        put_u64(row, 48, slot.stored);
        put_u64(row, 56, slot.raw);
    }
    out
}

/// The keys in the full slots of an index table whose header gives a number of slots its
/// length agrees with; none for one that does not.
pub(crate) fn index_keys(table: &[u8]) -> Vec<Key> {
    let Some(count) = table.get(16..24).map(|it| u64_at(it, 0)) else { return Vec::new() };
    let Some(count) = usize::try_from(count).ok().filter(|&it| it >= INDEX_GROUP) else { return Vec::new() };
    if count.checked_mul(1 + INDEX_ROW).and_then(|it| it.checked_add(INDEX_HEADER)) != Some(table.len()) {
        return Vec::new();
    }
    let (control, rows) = table[INDEX_HEADER..].split_at(count);
    control
        .iter()
        .enumerate()
        .filter(|(_, it)| **it < INDEX_EMPTY)
        .map(|(at, _)| Key(ContentId(rows[at * INDEX_ROW..at * INDEX_ROW + 32].try_into().expect("32 bytes"))))
        .collect()
}

// The store, in the simulation.

pub(crate) const DIR: &str = "store";
pub(crate) const HEAD: &str = "head";
pub(crate) const INDEX: &str = "index-0";

pub(crate) fn pack_file(at: usize) -> String {
    format!("pack-{at:06}")
}

pub(crate) fn file(name: &str) -> FileName {
    FileName::new(&format!("{DIR}/{name}")).expect("a name in the store's directory")
}

fn is_pack(file: &FileName) -> bool {
    file.as_str().strip_prefix(DIR).and_then(|it| it.strip_prefix("/pack-")).is_some()
}

/// A machine that never fails and answers at once: the bytes are the only thing that varies.
pub(crate) fn sim() -> Sim {
    Sim::new(0, Faults { fail_one_in: 0, max_latency: Duration::ZERO, ..Faults::default() })
}

pub(crate) fn config() -> Config {
    Config { dir: FileName::new(DIR).expect("the store's directory"), layers: Vec::new(), tags: 0 }
}

/// Runs what the store asked for, handing it each completion, until nothing is in flight.
pub(crate) fn drive(sim: &mut Sim, store: &mut Store, effects: &mut Vec<Effect>) {
    run_until_idle(sim, effects, |completion, effects| {
        if store.owns(completion.tag) {
            store.complete(completion, effects);
        }
    });
}

/// The store in the simulation's directory, opened as far as its bytes let it.
pub(crate) fn open(sim: &mut Sim) -> Store {
    let mut effects = Vec::new();
    let mut store = Store::open(config(), &mut effects);
    drive(sim, &mut store, &mut effects);
    store
}

/// The store's files and their lengths.
pub(crate) fn files(sim: &mut Sim) -> Vec<(FileName, u64)> {
    let dir = FileName::new(DIR).expect("the store's directory");
    let mut effects = vec![Effect::List { dir, tag: Tag(0) }];
    let mut found = Vec::new();
    run_until_idle(sim, &mut effects, |completion, _| {
        if let Done::Listed(listed) = completion.done {
            found = listed.into_iter().filter(|(_, meta)| !meta.dir).map(|(file, meta)| (file, meta.len)).collect();
        }
    });
    found
}

/// A simulation whose store directory holds `files`, by name, and nothing else.
pub(crate) fn store_of(files: &[(&str, &[u8])]) -> Sim {
    let mut sim = sim();
    for (name, bytes) in files {
        sim.change(&file(name), bytes);
    }
    sim
}

/// The value of the one record of [`lone_pack`].
pub(crate) const LONE: &[u8] = b"the one value of a pack a target writes by hand";

pub(crate) fn lone_key() -> Key {
    key(u32::MAX)
}

/// One block: a pack of unit 0 holding [`LONE`] as a raw query, its record at [`PACK_HEADER`].
pub(crate) fn lone_pack() -> Vec<u8> {
    let mut pack = pack_header(0).to_vec();
    append_record(&mut pack, &Record::raw_value(lone_key(), Kind::Query, LONE), LONE);
    padded(pack)
}

/// The slot of [`lone_pack`]'s record in pack 0.
pub(crate) fn lone_slot() -> Slot {
    Slot::of(0, PACK_HEADER, &Record::raw_value(lone_key(), Kind::Query, LONE))
}

/// [`lone_pack`] as pack 0 of a head, indexed up to `indexed`.
pub(crate) fn lone_ref(indexed: u64) -> PackRef {
    PackRef { removed: false, unit: 0, committed: BLOCK as u64, indexed, dead: 0 }
}

// Values written through the API.

const KEY_DOMAIN: &str = "blaze fuzz cas key 1";
pub(crate) const VALUES_MOST: u8 = 6;
/// Long enough for an output to be kept in content-defined chunks (256 KiB and up).
pub(crate) const LONGEST: usize = 320 << 10;

pub(crate) fn key(n: u32) -> Key {
    Key(ContentId::of(KEY_DOMAIN, &n.to_le_bytes()))
}

pub(crate) struct Value {
    pub(crate) key: Key,
    pub(crate) kind: Kind,
    pub(crate) bytes: Vec<u8>,
}

/// A value as an input describes it: `len << (2 * shift)` bytes (at most [`LONGEST`]) of
/// `pattern` over and over, or of the value's number when `pattern` is empty.
pub(crate) struct Spec {
    pub(crate) kind: Kind,
    pub(crate) len: u16,
    pub(crate) shift: u8,
    pub(crate) pattern: &'static [u8],
}

/// Values to put, each of [`key`] of its number, and how many settles follow their flush.
pub(crate) fn values(input: &mut Input<'_>) -> (Vec<Value>, u8) {
    let count = input.u8() % (VALUES_MOST + 1);
    let values = (0..u32::from(count))
        .map(|n| {
            let kind = Kind::ALL[usize::from(input.u8()) % Kind::ALL.len()];
            let len = (usize::from(input.u16()) << (input.u8() % 3 * 2)) % (LONGEST + 1);
            let pattern_len = usize::from(input.u8());
            let bytes = match input.bytes(pattern_len) {
                [] => vec![n as u8; len],
                pattern => pattern.iter().copied().cycle().take(len).collect(),
            };
            Value { key: key(n), kind, bytes }
        })
        .collect();
    (values, input.u8() % 5)
}

/// The input [`values`] reads as `specs` and `settles`.
pub(crate) fn encode_values(specs: &[Spec], settles: u8) -> Vec<u8> {
    let mut out = vec![specs.len() as u8];
    for spec in specs {
        out.push(spec.kind.code());
        out.extend_from_slice(&spec.len.to_le_bytes());
        out.push(spec.shift);
        out.push(spec.pattern.len() as u8);
        out.extend_from_slice(spec.pattern);
    }
    out.push(settles);
    out
}

/// Values of every kind and form a put and settles make: raw, an output in LZ4 chunks, long
/// ones that three settles move to zstd.
pub(crate) fn sample_values() -> Vec<Spec> {
    vec![
        Spec { kind: Kind::Query, len: 300, shift: 0, pattern: b"a query's result, " },
        Spec { kind: Kind::Code, len: 5000, shift: 0, pattern: b"\x55\x48\x89\xe5\x48\x83\xec\x10\xc3" },
        Spec { kind: Kind::Debug, len: 20000, shift: 0, pattern: b"DW_TAG_subprogram src/lib.rs:12\n" },
        Spec { kind: Kind::Run, len: 9000, shift: 0, pattern: b"cargo:rustc-cfg=has_feature\n" },
        Spec { kind: Kind::Output, len: 20000, shift: 2, pattern: b"\x7fELF\x02\x01\x01 an object's bytes" },
        Spec { kind: Kind::Image, len: 0, shift: 0, pattern: b"" },
    ]
}

/// The inputs [`values`] reads for [`sample_values`]: all of them settled and not, and each
/// alone.
pub(crate) fn sample_inputs() -> Vec<Vec<u8>> {
    let specs = sample_values();
    let mut out = vec![encode_values(&specs, 0), encode_values(&specs, 3)];
    for spec in specs {
        out.push(encode_values(&[spec], 3));
    }
    out
}

/// A simulation whose store holds `values`, flushed, then settled `settles` times.
pub(crate) fn write(values: &[Value], settles: u8) -> Sim {
    let mut sim = sim();
    let mut store = open(&mut sim);
    assert!(store.ready(), "an empty store opens: {:?}", store.broken());
    for value in values {
        store.put(value.key, value.kind, value.bytes.clone()).expect("a put of a key put once");
    }
    let mut effects = Vec::new();
    store.flush(&mut effects);
    drive(&mut sim, &mut store, &mut effects);
    for _ in 0..settles {
        store.settle(&mut effects);
        drive(&mut sim, &mut store, &mut effects);
    }
    assert!(store.broken().is_none(), "a store on a machine that never fails goes on: {:?}", store.broken());
    sim
}

/// Each value's key and the bytes it was put with.
pub(crate) fn wants(values: &[Value]) -> Vec<(Key, Option<&[u8]>)> {
    values.iter().map(|it| (it.key, Some(it.bytes.as_slice()))).collect()
}

/// The store's packs, by name.
pub(crate) fn packs(sim: &mut Sim) -> Vec<FileName> {
    files(sim).into_iter().map(|(file, _)| file).filter(is_pack).collect()
}

/// A pack's number, from its name.
pub(crate) fn pack_number(file: &FileName) -> u32 {
    file.as_str().rsplit('-').next().and_then(|it| it.parse().ok()).expect("a pack's name ends in its number")
}

/// The slots of an index table of every record in the store's packs, the last record of a key
/// shadowing those before it, as the store's own table would hold them.
pub(crate) fn slots_of_packs(sim: &mut Sim) -> Vec<Slot> {
    let mut slots: Vec<Slot> = Vec::new();
    for pack in packs(sim) {
        let number = pack_number(&pack);
        for (at, record) in walk(&sim.contents(&pack)) {
            if record.form == DICTIONARY {
                continue;
            }
            let slot = Slot::of(number, at, &record);
            match slots.iter_mut().find(|it| it.key == record.key) {
                Some(it) => *it = slot,
                None => slots.push(slot),
            }
        }
    }
    slots
}

// Damage.

/// How many changes one input makes at most.
const DAMAGES_MOST: usize = 16;

/// Changes `bytes` as the input says: flips a byte, writes a word over a field, cuts, inserts,
/// zeros a run. With `grow` false, only the changes that keep its length.
pub(crate) fn damage(bytes: &mut Vec<u8>, input: &mut Input<'_>, grow: bool) {
    for _ in 0..DAMAGES_MOST {
        if input.is_empty() {
            return;
        }
        let op = input.u8();
        let at = input.u32() as usize;
        let len = bytes.len();
        match op % 6 {
            0 if len > 0 => bytes[at % len] ^= input.u8().max(1),
            1 if len > 0 => {
                let word = input.u64().to_le_bytes();
                let at = at % len;
                let end = (at + 8).min(len);
                bytes[at..end].copy_from_slice(&word[..end - at]);
            }
            2 if grow => bytes.truncate(at % (len + 1)),
            3 if grow => {
                let more = usize::from(input.u8());
                let more = input.bytes(more);
                let at = at % (len + 1);
                _ = bytes.splice(at..at, more.iter().copied());
            }
            4 if len > 0 => {
                let at = at % len;
                let end = at.saturating_add(usize::from(input.u16())).min(len);
                bytes[at..end].fill(0);
            }
            5 if len > 0 => {
                let word = input.u32().to_le_bytes();
                let at = at % len;
                let end = (at + 4).min(len);
                bytes[at..end].copy_from_slice(&word[..end - at]);
            }
            _ => {}
        }
    }
}

/// Damages `file` as the input says, then hands `after` the damaged bytes and the bytes before;
/// whether any byte changed.
pub(crate) fn damage_file(
    sim: &mut Sim,
    file: &FileName,
    input: &mut Input<'_>,
    after: impl FnOnce(&mut Vec<u8>, &[u8]),
) -> bool {
    let before = sim.contents(file);
    let mut bytes = before.clone();
    damage(&mut bytes, input, true);
    after(&mut bytes, &before);
    let changed = bytes != before;
    if changed {
        sim.change(file, &bytes);
    }
    changed
}

/// Damages one of the store's packs (the input's next byte picks which); with `reseal`, every
/// record header that was whole before gets its check again, so the damage reaches what is
/// behind the check.
pub(crate) fn damage_packs(sim: &mut Sim, input: &mut Input<'_>, reseal: bool) -> bool {
    let packs = packs(sim);
    if packs.is_empty() || input.is_empty() {
        return false;
    }
    let pack = &packs[usize::from(input.u8()) % packs.len()];
    damage_file(sim, pack, input, |bytes, before| {
        if reseal {
            let starts: Vec<usize> = walk(before).iter().map(|(at, _)| *at).collect();
            reseal_records(bytes, &starts);
        }
    })
}

/// Damages the one record of the store's packs the input's next two bytes pick, among those
/// `pick` keeps, in place and within its span; with `reseal`, its header gets its check again.
pub(crate) fn damage_record(
    sim: &mut Sim,
    input: &mut Input<'_>,
    reseal: bool,
    pick: impl Fn(&Record) -> bool,
) -> bool {
    let mut records = Vec::new();
    for pack in packs(sim) {
        let bytes = sim.contents(&pack);
        records.extend(walk(&bytes).into_iter().filter(|(_, it)| pick(it)).map(|(at, it)| (pack.clone(), at, it)));
    }
    if records.is_empty() || input.is_empty() {
        return false;
    }
    let (pack, at, record) = &records[usize::from(input.u16()) % records.len()];
    let before = sim.contents(pack);
    let span = usize::try_from(record.stored)
        .ok()
        .and_then(|it| it.checked_add(RECORD_HEADER + 15))
        .map_or(before.len(), |it| it & !15)
        .min(before.len() - at);
    let mut piece = before[*at..at + span].to_vec();
    damage(&mut piece, input, false);
    let mut bytes = before.clone();
    bytes[*at..at + span].copy_from_slice(&piece);
    if reseal {
        reseal_records(&mut bytes, &[*at]);
    }
    let changed = bytes != before;
    if changed {
        sim.change(pack, &bytes);
    }
    changed
}

// Reading back.

/// zstd's densest frame: an RLE block, a 3-byte header and its byte, stands for 128 KiB.
pub(crate) const EXPANSION: usize = (128 << 10) / 4;

/// The most one allocation of a read of a store with `disk` bytes on disk may ask for: what a
/// read of those bytes may make room for (`blaze_fuzz::allowed`) and what their densest zstd
/// frames decode to, or what opening an empty store asks for (its fixed tables), the larger.
pub(crate) fn bound(disk: usize) -> usize {
    baseline().max(blaze_fuzz::allowed(disk).saturating_add(disk.saturating_mul(EXPANSION)))
}

/// The most one allocation of taking `len` bytes from the wire may ask for: nothing on the wire
/// is compressed, so what those bytes imply (`blaze_fuzz::allowed`), or what opening an empty
/// store asks for, the larger.
pub(crate) fn wire_bound(len: usize) -> usize {
    baseline().max(blaze_fuzz::allowed(len))
}

/// The largest allocation of opening an empty store: the same in every run.
fn baseline() -> usize {
    static BASELINE: OnceLock<usize> = OnceLock::new();
    *BASELINE.get_or_init(|| {
        let mut sim = sim();
        blaze_fuzz::largest_allocation(|| drop(open(&mut sim))).0
    })
}

/// How a read back went.
#[derive(Clone, Copy, Default, Debug)]
pub(crate) struct Back {
    pub(crate) ready: bool,
    pub(crate) whole: usize,
    pub(crate) missing: usize,
    pub(crate) corrupt: usize,
}

/// Opens the store in `sim` and reads each of `wants` (with the bytes it was put with, where
/// known) through `Store::get` and `Reader::get`, then scrubs it with `Store::verify`. Asserts:
/// - no allocation of all that is larger than [`bound`] of the bytes on disk;
/// - a store that did not open says why;
/// - a value read is of the key asked, hashes to its content, and is the bytes it was put with
///   where they are known: damaged bytes are `Corrupt`, never other bytes;
/// - `Store::get` and `Reader::get` read the same;
/// - a scrub finds every corrupt value a read found.
pub(crate) fn read_back(sim: &mut Sim, wants: &[(Key, Option<&[u8]>)]) -> Back {
    let disk: u64 = files(sim).iter().map(|(_, len)| len).sum();
    let disk = usize::try_from(disk).unwrap_or(usize::MAX);
    let most = bound(disk);
    let (largest, back) = blaze_fuzz::largest_allocation(|| read(sim, wants));
    assert!(
        largest <= most,
        "bounded: a read of a store of {disk} bytes on disk asked for {largest} bytes at once, \
         more than the {most} those bytes imply"
    );
    back
}

fn read(sim: &mut Sim, wants: &[(Key, Option<&[u8]>)]) -> Back {
    let store = open(sim);
    let ready = store.ready();
    assert!(ready || store.broken().is_some(), "a store that did not open, its IO done, says why");
    let reader = store.reader();
    let guard = blaze_cas::enter();
    let mut back = Back { ready, ..Back::default() };
    let mut corrupt = Vec::new();
    for (key, put) in wants {
        match (store.get(key), reader.get(&guard, key)) {
            (Ok(Some(entry)), Ok(Some(other))) => {
                assert_eq!(entry.key, *key, "a read gives the value of the key asked for");
                assert!(entry.content == content_of(entry.bytes), "a verified read gives bytes of its content hash");
                assert!(
                    (entry.kind, entry.content, entry.bytes) == (other.kind, other.content, other.bytes),
                    "Store::get and Reader::get read the same value"
                );
                if let Some(put) = put {
                    assert!(entry.bytes == *put, "a verified read gives the bytes put, never other bytes");
                }
                back.whole += 1;
            }
            (Ok(None), Ok(None)) => back.missing += 1,
            (Err(found), Err(_)) => {
                corrupt.push(found.key);
                back.corrupt += 1;
            }
            (got, again) => panic!(
                "Store::get and Reader::get disagree on {:x}: {:?} and {:?}",
                key.0,
                got.map(|it| it.map(|entry| entry.content)),
                again.map(|it| it.map(|entry| entry.content)),
            ),
        }
    }
    let scrubbed = store.verify(None);
    for key in corrupt {
        assert!(scrubbed.iter().any(|it| it.key == key), "a scrub finds every corrupt value a read finds: {:x}", key.0);
    }
    back
}

/// The round trip: values put and not damaged all read back as they were put.
pub(crate) fn assert_whole(back: Back, values: usize) {
    assert!(back.ready && back.whole == values, "round trip: {values} values put and not damaged read back {back:?}");
}
