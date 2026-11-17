//! The direct reader against a model store, on the simulation under many seeds: the store's own
//! writer puts, flushes and crashes, the simulation fails reads, cuts them short and rots bytes,
//! and every answer [`Direct`] gives is checked against what was put.
//!
//! The planted bugs ([`Plant`]) are the oracle's controls: a batch's values placed back in disk
//! order, a record taken with no check, a run read one byte short, a chunked value's chunks put
//! together out of order, a file of its own put over its neighbour's place, a pack's first
//! touch read before the system is asked, each pack mapped in a wait of its own, and another
//! pack's mapping lent. Each has a directed plan the oracle must refuse.

use std::collections::BTreeMap;

use blaze_core::{
    Effect, FileName, run_until_idle,
    sim::{Faults, Rng, Sim},
};
use blaze_sim::{Config as SimConfig, Tape};

use super::*;
use crate::{Config, Kind, Store};

const DIR: &str = "store";

/// Keys the plans draw from.
const KEYS: u64 = 24;

/// The direct reader's tags: clear of the store's (`1 << 40` and its loads').
const DIRECT_TAGS: u64 = 1 << 44;

fn name(it: &str) -> FileName {
    FileName::new(it).unwrap()
}

fn config() -> Config {
    Config {
        dir: name(DIR),
        layers: Vec::new(),
        tags: 1 << 40,
        load_tags: (1 << 40) + (1 << 32),
        loader: crate::Loader::Pumped,
    }
}

fn key(n: u64) -> Key {
    Key(ContentId::of("blaze v2 store direct test key", &n.to_le_bytes()))
}

/// The value put under key `n`, and its kind: one in four of [`MAP_FROM`] bytes or more,
/// mapped; one in four cold text, which the store may compress; one in eight an output past
/// [`crate::CHUNK_FROM`], kept in chunks (two or more: a chunk is at most `chunk::MAX`); one in
/// eight machine code, kept as a file of its own; the rest short noise. Its last byte is never
/// zero, so a run read one byte short shows.
fn value(n: u64) -> (Kind, Vec<u8>) {
    let mut rng = Rng::new(n ^ 0x5eed_d1ec);
    let (kind, len, text) = match n % 8 {
        0 | 4 => (Kind::Query, MAP_FROM + rng.below(40 << 10), false),
        1 | 5 => (Kind::Debug, 1 + rng.below(1500), true),
        3 => (Kind::Output, crate::CHUNK_FROM as u64 + 1 + rng.below(300 << 10), false),
        7 => (Kind::Machine, 1 + rng.below(4000), false),
        _ => (Kind::Query, 1 + rng.below(1500), false),
    };
    let mut bytes: Vec<u8> = match text {
        true => format!("cargo:rustc-cfg=feature_{n}\ncargo:warning=compiled {n}\n")
            .into_bytes()
            .into_iter()
            .cycle()
            .take(len as usize)
            .collect(),
        false => (0..len).map(|_| rng.next_u64() as u8).collect(),
    };
    if let Some(last) = bytes.last_mut() {
        *last |= 1;
    }
    (kind, bytes)
}

fn drive(sim: &mut Sim, store: &mut Store, effects: &mut Vec<Effect>) {
    run_until_idle(sim, effects, |completion, effects| {
        if store.owns(completion.tag) {
            store.complete(completion, effects);
        }
    })
    .expect("the store's effects end");
}

/// The store, opened; an open the clone probe stopped on an injected failure is tried again,
/// as `crate::tests::open_with` does.
fn open(sim: &mut Sim) -> Store {
    for _ in 0..64 {
        let mut effects = Vec::new();
        let mut store = Store::open(config(), &mut effects);
        drive(sim, &mut store, &mut effects);
        let probe_failed = store.broken().is_some_and(|it| {
            it.what.starts_with("trying one clone in the store's directory")
                && it.what.ends_with("injected by the simulation")
        });
        if probe_failed {
            continue;
        }
        assert!(store.ready(), "{:?}", store.broken());
        return store;
    }
    panic!("the clone probe met an injected failure in 64 opens in a row")
}

fn flush(sim: &mut Sim, store: &mut Store) {
    let mut effects = Vec::new();
    store.flush(&mut effects);
    drive(sim, store, &mut effects);
}

/// One step of a plan.
#[derive(Clone, Debug)]
enum Op {
    Put(u64),
    Flush,
    Read(u64),
    /// Keys in the caller's order, unsorted, a key maybe more than once.
    ReadMany(Vec<u64>),
    Map(u64),
    /// Reads under the store's directory fail, one in two, until the crash that follows.
    FailReads,
    /// Reads under the store's directory end short, one in two, until the crash that follows.
    ShortReads,
    /// A byte of key `n`'s payload rots on the disk, and is put back after the reads.
    Rot(u64),
    Crash,
}

impl Op {
    fn decode(it: u16) -> Op {
        let n = u64::from(it >> 4) % KEYS;
        match it % 16 {
            0..=4 => Op::Put(n),
            5..=6 => Op::Flush,
            7..=8 => Op::Read(n),
            9..=10 => {
                let count = 1 + (it >> 9) % 6;
                let mut keys: Vec<u64> = (0..u64::from(count)).map(|at| (n + at * 7) % KEYS).collect();
                if it & 0x100 != 0 {
                    keys.push(n);
                }
                Op::ReadMany(keys)
            }
            11 => Op::Map(n),
            12 => [Op::FailReads, Op::ShortReads, Op::Rot(n)][usize::from(it >> 9) % 3].clone(),
            _ => Op::Crash,
        }
    }
}

/// What the model knows of a key.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    /// Put and not flushed: in a writer's segment, readable.
    Held,
    /// Flushed: durable.
    Durable,
    /// Put and not flushed before a crash: there or not.
    Maybe,
}

/// What a read said of one key.
enum Answer<'a> {
    Found(&'a [u8]),
    Absent,
    Failed(&'a Failed),
}

/// The invariant: whatever the direct reader answers of a key is what the store's writer was
/// given for it, and nothing else. A value found is the bytes put under the key, exactly; a
/// key put and not lost to a crash is never absent; a key never put is never found; a read
/// fails only while the simulation injects a fault (failed, short or rotted reads), and then
/// it fails rather than answering wrong bytes. A batch answers each key as a read of that key
/// alone does, in the caller's order, and leaves the caller's bytes before it as they were. A
/// value of [`MAP_FROM`] bytes or more that is durable maps, to its bytes.
fn check(model: &BTreeMap<u64, State>, n: u64, answer: Answer<'_>, faulty: bool) -> Result<(), String> {
    let state = model.get(&n).copied();
    match answer {
        Answer::Found(bytes) if state.is_none() => {
            Err(format!("key {n} was never put, and a read found {} bytes", bytes.len()))
        }
        Answer::Found(bytes) if bytes != value(n).1 => Err(format!(
            "key {n} ({state:?}) read as {} bytes that are not the {} put",
            bytes.len(),
            value(n).1.len()
        )),
        Answer::Found(_) => Ok(()),
        Answer::Absent if matches!(state, Some(State::Held | State::Durable)) => {
            Err(format!("key {n} ({state:?}) read as absent"))
        }
        Answer::Absent => Ok(()),
        Answer::Failed(failed) if !faulty => Err(format!("key {n} ({state:?}) failed with no fault: {failed}")),
        Answer::Failed(failed) if failed.key() != key(n) => {
            Err(format!("key {n}'s read failed naming another key: {failed}"))
        }
        Answer::Failed(_) => Ok(()),
    }
}

/// The run of `ops` with `plant` in the reader: why the oracle refused it, if it did.
fn run(seed: u64, ops: &[Op], plant: Plant) -> Result<(), String> {
    let mut sim = Sim::new(seed, Faults::default());
    let mut store = open(&mut sim);
    let fresh = |store: &Store| {
        let mut direct = Direct::new(store.reader(), DIRECT_TAGS, Residency::model());
        direct.plant(plant);
        direct
    };
    let mut direct = fresh(&store);
    let mut model: BTreeMap<u64, State> = BTreeMap::new();
    let prefix: &[u8] = b"the caller's own bytes";
    for (step, op) in ops.iter().enumerate() {
        let at = |why: String| format!("step {step} ({op:?}): {why}");
        match op {
            Op::Put(n) => {
                if model.get(n).is_some_and(|it| *it != State::Maybe) {
                    continue;
                }
                let (kind, bytes) = value(*n);
                store.put(key(*n), kind, &bytes).map_err(|why| at(format!("the put: {why:?}")))?;
                if store.broken().is_some() {
                    return Err(at(format!("the store broke: {:?}", store.broken())));
                }
                model.insert(*n, State::Held);
            }
            Op::Flush => {
                flush(&mut sim, &mut store);
                for state in model.values_mut() {
                    if *state == State::Held {
                        *state = State::Durable;
                    }
                }
            }
            Op::Read(n) => read_one(&mut sim, &mut direct, &model, *n, false).map_err(at)?,
            Op::ReadMany(keys) => read_batch(&mut sim, &mut direct, &model, keys, prefix, false).map_err(at)?,
            Op::Map(n) => map_one(&mut sim, &mut direct, &model, *n, false).map_err(at)?,
            Op::FailReads | Op::ShortReads => {
                match op {
                    Op::FailReads => sim.fail_reads(name(DIR), 2),
                    _ => sim.short_reads(name(DIR), 2),
                }
                let keys: Vec<u64> = model.keys().copied().collect();
                read_batch(&mut sim, &mut direct, &model, &keys, prefix, true).map_err(at)?;
                for n in &keys {
                    read_one(&mut sim, &mut direct, &model, *n, true).map_err(at)?;
                }
                // The faults last until a crash.
                drop(direct);
                store = crash(&mut sim, store, &mut model);
                direct = fresh(&store);
            }
            Op::Rot(n) => {
                let Some((file, payload, _)) = direct.where_of(&key(*n)) else { continue };
                let (_, bytes) = value(*n);
                let Some(&byte) = sim.contents(&file).get(payload as usize) else { continue };
                if bytes.is_empty() {
                    continue;
                }
                sim.rot(&file, payload, &[!byte]);
                let read = read_one(&mut sim, &mut direct, &model, *n, true);
                sim.rot(&file, payload, &[byte]);
                read.map_err(at)?;
                // A mapping taken while the byte was rotted is a copy of it (the simulation's
                // map): dropped, as a page cache drops a page the disk no longer holds.
                drop(direct);
                store.direct_maps.clear();
                direct = fresh(&store);
            }
            Op::Crash => {
                drop(direct);
                store = crash(&mut sim, store, &mut model);
                direct = fresh(&store);
            }
        }
        if !direct.others().is_empty() {
            return Err(at("the reader was handed completions of effects not its own".into()));
        }
    }
    Ok(())
}

/// The power goes out: what was not flushed may be lost, and the store is opened again.
fn crash(sim: &mut Sim, store: Store, model: &mut BTreeMap<u64, State>) -> Store {
    sim.crash();
    drop(store);
    for state in model.values_mut() {
        if *state == State::Held {
            *state = State::Maybe;
        }
    }
    open(sim)
}

fn read_one(sim: &mut Sim, direct: &mut Direct, model: &BTreeMap<u64, State>, n: u64, faulty: bool) -> Result<(), String> {
    let mut into = b"before".to_vec();
    let read = direct.read(sim, &key(n), &mut into);
    if &into[..6] != b"before" {
        return Err(format!("a read of key {n} changed the bytes before its own"));
    }
    match &read {
        Read::Found { len } if 6 + len != into.len() => {
            Err(format!("a read of key {n} says {len} bytes and appended {}", into.len() - 6))
        }
        Read::Found { .. } => check(model, n, Answer::Found(&into[6..]), faulty),
        Read::Absent if into.len() != 6 => Err(format!("an absent key {n} appended bytes")),
        Read::Absent => check(model, n, Answer::Absent, faulty),
        Read::Failed(failed) => check(model, n, Answer::Failed(failed), faulty),
    }
}

fn read_batch(
    sim: &mut Sim,
    direct: &mut Direct,
    model: &BTreeMap<u64, State>,
    keys: &[u64],
    prefix: &[u8],
    faulty: bool,
) -> Result<(), String> {
    let asked: Vec<Key> = keys.iter().map(|it| key(*it)).collect();
    let mut into = prefix.to_vec();
    let mut places = Vec::new();
    direct.read_many(sim, &asked, &mut into, &mut places);
    if places.len() != keys.len() {
        return Err(format!("a batch of {} keys gave {} answers", keys.len(), places.len()));
    }
    if &into[..prefix.len()] != prefix {
        return Err("a batch changed the caller's bytes before its own".into());
    }
    for (n, place) in keys.iter().zip(&places) {
        let answer = match place {
            Placed::Found { at, len } => match into.get(*at..at + len) {
                Some(bytes) if *at >= prefix.len() => Answer::Found(bytes),
                _ => return Err(format!("key {n}'s place {at}..{} is not the batch's bytes", at + len)),
            },
            Placed::Absent => Answer::Absent,
            Placed::Failed(failed) => Answer::Failed(failed),
        };
        check(model, *n, answer, faulty)?;
    }
    if faulty {
        return Ok(());
    }
    // With no fault, each key's answer is what a read of it alone gives.
    for (n, place) in keys.iter().zip(&places) {
        let mut alone = Vec::new();
        let same = match (direct.read(sim, &key(*n), &mut alone), place) {
            (Read::Found { .. }, Placed::Found { at, len }) => alone == into[*at..at + len],
            (Read::Absent, Placed::Absent) => true,
            _ => false,
        };
        if !same {
            return Err(format!("key {n} in a batch is not as it reads alone"));
        }
    }
    Ok(())
}

fn map_one(sim: &mut Sim, direct: &mut Direct, model: &BTreeMap<u64, State>, n: u64, faulty: bool) -> Result<(), String> {
    match direct.map(sim, &key(n)) {
        Ok(Some(mapped)) => check(model, n, Answer::Found(&mapped), faulty),
        Ok(None) => check(model, n, Answer::Absent, faulty),
        // A short value, one in a writer's segment, and a chunked one (made whole in memory)
        // are read, not mapped.
        Err(Failed::NotMappable { .. })
            if (value(n).1.len() as u64) < MAP_FROM
                || model.get(&n) != Some(&State::Durable)
                || value(n).0 == Kind::Output =>
        {
            Ok(())
        }
        Err(failed) => check(model, n, Answer::Failed(&failed), faulty),
    }
}

/// The property's body: a drawn seed and plan, run with no bug planted.
fn answers_what_was_put(tape: &Tape) {
    let seed: u64 = tape.next_u64("sim seed");
    let ops: Vec<u16> = tape.vec(0..=80, "ops", |tape| tape.int::<u16>("op"));
    let ops: Vec<Op> = ops.into_iter().map(Op::decode).collect();
    if let Err(why) = run(seed, &ops, Plant::None) {
        panic!("seed {seed}: {why}\nops: {ops:?}");
    }
}

#[test]
fn the_direct_reader_answers_what_was_put() {
    blaze_sim::check(SimConfig::cases(200), answers_what_was_put);
}

/// The case the search found: keys 15 and 23, each a file of its own once flushed, read in one
/// batch, where the first's place came to cover the second's bytes.
#[test]
fn the_direct_reader_answers_what_was_put_in_the_found_case() {
    answers_what_was_put(&Tape::replay(&[
        1, 1, 6, 12, 1, 5, 0, 0, 1, 6, 7405, 1, 6, 12, 1, 6, 44, 1, 6, 38256, 1, 6, 22640, 1, 6, 405, 1, 0, 4,
        12,
    ]));
}

/// Plans whose one batch holds a value kept as a file of its own (keys 7, 15 and 23) beside a
/// neighbour: another such file, or a value of another form (key 14 short noise kept raw, key 3
/// an output kept in chunks). A file of its own stands in the first round as a place of no
/// bytes, at the same position as the answer after it.
fn neighbours_of_files() -> [Vec<Op>; 6] {
    let batch = |keys: &[u64]| {
        let mut ops: Vec<Op> = keys.iter().map(|n| Op::Put(*n)).collect();
        ops.push(Op::Flush);
        ops.push(Op::ReadMany(keys.to_vec()));
        ops
    };
    [batch(&[7, 15]), batch(&[7, 15, 23]), batch(&[7, 14]), batch(&[14, 7]), batch(&[7, 3]), batch(&[3, 7])]
}

/// The invariant: a batch that holds neighbouring values kept as files of their own, or such a
/// file beside a value of another form, hands each key its own bytes, under every seed.
#[test]
fn a_batch_hands_each_file_of_its_own_its_own_bytes() {
    for ops in neighbours_of_files() {
        for seed in 0..16 {
            if let Err(why) = run(seed, &ops, Plant::None) {
                panic!("seed {seed}: {why}\nops: {ops:?}");
            }
        }
    }
}

#[test]
fn the_oracle_catches_a_file_put_over_its_neighbour() {
    let [files, _, file_then_raw, ..] = neighbours_of_files();
    control(&files, Plant::StrictlyAfter);
    control(&file_then_raw, Plant::StrictlyAfter);
}

/// Each control's plan passes with no bug planted and is refused with it, under every seed.
fn control(ops: &[Op], plant: Plant) {
    for seed in 0..16 {
        if let Err(why) = run(seed, ops, Plant::None) {
            panic!("seed {seed}: the control's plan fails with no bug planted: {why}");
        }
        assert!(run(seed, ops, plant).is_err(), "seed {seed}: the oracle missed {plant:?}");
    }
}

#[test]
fn the_oracle_catches_a_batch_placed_back_in_disk_order() {
    control(&[Op::Put(1), Op::Put(2), Op::Flush, Op::ReadMany(vec![2, 1])], Plant::SortedOrder);
}

#[test]
fn the_oracle_catches_a_read_with_no_check() {
    // Key 2's value is short noise, kept raw: a payload byte rotted under an intact header
    // decodes, and only the record's CRC (format 9's, over the payload) sees it.
    control(&[Op::Put(2), Op::Flush, Op::Rot(2)], Plant::NoCheck);
}

#[test]
fn the_oracle_catches_chunks_put_together_out_of_order() {
    // Key 3's value is an output kept in two chunks or more: durable, and still in its
    // writer's segment.
    control(&[Op::Put(3), Op::Flush, Op::Read(3)], Plant::ChunkOrder);
    control(&[Op::Put(3), Op::ReadMany(vec![2, 3, 2])], Plant::ChunkOrder);
}

/// The invariant: a store's format-8 records (CRC over the header only) read right through
/// the direct reader, each hashed as format 8 was read, beside format-9 records; a format-8
/// record's payload byte rotted is refused by that hash, as a format-9 record's is by its CRC.
#[test]
fn the_direct_reader_reads_format_8_records() {
    let mut sim = Sim::new(5, Faults::default());
    let mut store = open(&mut sim);
    crate::disk::WRITE_FORMAT_8.set(true);
    for n in [2, 3, 6] {
        let (kind, bytes) = value(n);
        store.put(key(n), kind, &bytes).unwrap();
    }
    flush(&mut sim, &mut store);
    crate::disk::WRITE_FORMAT_8.set(false);
    for n in [10, 11] {
        let (kind, bytes) = value(n);
        store.put(key(n), kind, &bytes).unwrap();
    }
    flush(&mut sim, &mut store);
    let mut direct = Direct::new(store.reader(), DIRECT_TAGS, Residency::model());
    let model: BTreeMap<u64, State> = [2, 3, 6, 10, 11].into_iter().map(|n| (n, State::Durable)).collect();
    let keys: Vec<u64> = model.keys().copied().collect();
    read_batch(&mut sim, &mut direct, &model, &keys, b"before", false).unwrap();
    for n in [2, 10] {
        let (file, payload, _) = direct.where_of(&key(n)).unwrap();
        let byte = sim.contents(&file)[payload as usize];
        sim.rot(&file, payload, &[!byte]);
        // A reader of its own, holding no page: `direct` read the record's block above, so it
        // takes the record from its pack's mapping, which in the simulation is a copy made
        // before the rot (a page cache holding the bytes as they were written). The rot is on
        // the disk, so it is a read that must see it.
        let mut cold = Direct::new(store.reader(), DIRECT_TAGS, Residency::model());
        let mut into = Vec::new();
        let read = cold.read(&mut sim, &key(n), &mut into);
        sim.rot(&file, payload, &[byte]);
        assert!(matches!(read, Read::Failed(Failed::Corrupt { .. })), "key {n}'s rotted byte: {read:?}");
    }
}

#[test]
fn the_oracle_catches_a_run_read_one_byte_short() {
    control(&[Op::Put(2), Op::Flush, Op::Read(2)], Plant::RunEnd);
}

/// The key of the `n`th record one build puts in the one-by-one tests: clear of [`key`]'s.
fn beside_key(n: u64) -> Key {
    Key(ContentId::of("blaze v2 store direct test record beside", &n.to_le_bytes()))
}

/// The value of the `n`th record one build puts: short noise, as a file's per-item entries
/// are (tens to hundreds of bytes), kept raw in a pack.
fn beside_value(n: u64) -> Vec<u8> {
    let mut rng = Rng::new(n ^ 0xb10c_5eed);
    let len = 1 + rng.below(1500);
    (0..len).map(|_| rng.next_u64() as u8).collect()
}

/// What one reader's asks of a build's records cost, against what they had to.
struct Cost {
    /// The reader's counts over the asks.
    counts: Counts,
    /// The blocks of [`AROUND`] bytes the records lie in, each `(file, block start)`.
    blocks: u64,
    /// The packs the records lie in.
    files: u64,
    /// The bytes of those blocks, each clipped to what its pack commits.
    block_bytes: u64,
}

/// `count` records put by one build (one flush), then asked one by one, in `order` (indices
/// into them), each by a call of its own of one fresh reader with `plant` planted: why a value
/// read is not the one put, if one is not, and what the asks cost.
fn asked_one_by_one(seed: u64, count: u64, order: &[u64], plant: Plant) -> Result<Cost, String> {
    let mut sim = Sim::new(seed, Faults::default());
    let mut store = open(&mut sim);
    for n in 0..count {
        store.put(beside_key(n), Kind::Query, &beside_value(n)).map_err(|why| format!("the put of {n}: {why:?}"))?;
    }
    flush(&mut sim, &mut store);
    let mut direct = Direct::new(store.reader(), DIRECT_TAGS, Residency::model());
    direct.plant(plant);
    let mut blocks = std::collections::BTreeSet::new();
    let mut files = std::collections::BTreeSet::new();
    let mut usable_of = BTreeMap::new();
    for n in 0..count {
        let (file, at, len, usable) =
            direct.record_of(&beside_key(n)).ok_or_else(|| format!("record {n} is in no pack after the flush"))?;
        // Bounded: a record of at most 1,500 bytes and its header lie in one or two blocks.
        let mut block = at - at % AROUND;
        while block < at + len {
            blocks.insert((file.clone(), block));
            block += AROUND;
        }
        usable_of.insert(file.clone(), usable);
        files.insert(file);
    }
    let block_bytes = blocks.iter().map(|(file, block)| (block + AROUND).min(usable_of[file]) - block).sum();
    for &n in order {
        let mut into = Vec::new();
        match direct.read(&mut sim, &beside_key(n), &mut into) {
            Read::Found { len } if into == beside_value(n) && len == into.len() => {}
            other => return Err(format!("record {n} read as {other:?}, {} bytes, not the value put", into.len())),
        }
        if !direct.others().is_empty() {
            return Err("the reader was handed completions of effects not its own".into());
        }
    }
    Ok(Cost { counts: direct.own_counts(), blocks: blocks.len() as u64, files: files.len() as u64, block_bytes })
}

/// The oracle over one run's cost: the bound on device reads (the simulation's `ReadInto`s,
/// `Counts::reads`) and waits, and the bytes read.
fn costs_at_most_a_read_a_block(cost: &Cost, asked: u64) -> Result<(), String> {
    let Cost { counts, blocks, files, block_bytes } = cost;
    if counts.reads > *blocks {
        return Err(format!("{asked} records in {blocks} blocks cost {} reads: {counts:?}", counts.reads));
    }
    // A wait a read, and one for the round that maps the packs (at most one a pack; they do
    // not grow after the build).
    if counts.waits > blocks + files {
        return Err(format!("{asked} records in {blocks} blocks of {files} packs cost {} waits: {counts:?}", counts.waits));
    }
    if counts.bytes != *block_bytes {
        return Err(format!(
            "{asked} records read {} bytes, not the {block_bytes} of the blocks they lie in, each once: {counts:?}",
            counts.bytes
        ));
    }
    if counts.records != asked || counts.asked != asked {
        return Err(format!("{asked} records asked, and the reader counts otherwise: {counts:?}"));
    }
    Ok(())
}

/// The property's body: a drawn count of records, all put by one build, asked one by one in a
/// drawn order (each once), with `plant` planted: why the oracle refused it, if it did.
fn one_by_one_body(tape: &Tape, plant: Plant) -> Result<(), String> {
    let seed = tape.next_u64("sim seed");
    let count = 1 + tape.below(160, "records");
    let mut order: Vec<u64> = (0..count).collect();
    // Fisher-Yates, each swap drawn: every order can be drawn.
    for at in (1..order.len()).rev() {
        let with = tape.index(at + 1, "swap");
        order.swap(at, with);
    }
    let cost = asked_one_by_one(seed, count, &order, plant)?;
    costs_at_most_a_read_a_block(&cost, count)
}

/// The invariant: `n` records one build put, asked one by one by a reader in any order, are
/// each the value put, and cost at most one device read (and one wait) per block of [`AROUND`]
/// bytes they lie in, plus one wait a pack for its mapping; the bytes read are exactly those
/// blocks', each once, whatever the order of the asks (the floor: each byte the records lie in
/// read once, and not one ask that finds its record in memory waits). With 160 records of at
/// most 1.5 KB the bound is a few reads where one read an ask is up to 160.
///
/// The control is the planted bug `Plant::OneByOne`, each ask its own read with nothing taken
/// from memory: the oracle refuses it ([`the_oracle_catches_each_ask_its_own_read`]).
#[test]
fn records_put_together_and_asked_one_by_one_cost_a_read_a_block() {
    blaze_sim::check(SimConfig::cases(200), |tape: &Tape| {
        if let Err(why) = one_by_one_body(tape, Plant::None) {
            panic!("{why}");
        }
    });
}

/// The invariant: the bytes read for records one build put equal the bytes of the blocks they
/// lie in, each once, for every order of asks: here every order of six records spread over
/// more than one block (every 60th of 360 put, some 300 KB of pack; asserted), all 720 of
/// them.
#[test]
fn the_bytes_read_are_the_blocks_once_for_every_order_of_asks() {
    const COUNT: u64 = 6;
    // The asked records are every `STRIDE`th of the build's puts, so they lie in several
    // blocks.
    const STRIDE: u64 = 60;
    let all = COUNT * STRIDE;
    let asked: Vec<u64> = (0..COUNT).map(|it| it * STRIDE).collect();
    // One build's puts; each order is asked by a fresh reader, holding no page, of the same
    // store (a read changes nothing a later reader sees).
    let mut sim = Sim::new(7, Faults::default());
    let mut store = open(&mut sim);
    for n in 0..all {
        store.put(beside_key(n), Kind::Query, &beside_value(n)).unwrap();
    }
    flush(&mut sim, &mut store);
    let probe = Direct::new(store.reader(), DIRECT_TAGS, Residency::model());
    let mut blocks = std::collections::BTreeSet::new();
    for &n in &asked {
        let (file, at, len, usable) = probe.record_of(&beside_key(n)).expect("in a pack after the flush");
        // Bounded: a record of at most 1,500 bytes and its header lie in one or two blocks.
        let mut block = at - at % AROUND;
        while block < at + len {
            blocks.insert((file.clone(), block, (block + AROUND).min(usable) - block));
            block += AROUND;
        }
    }
    let block_bytes: u64 = blocks.iter().map(|it| it.2).sum();
    assert!(blocks.len() > 1, "the asked records lie in {} block: the test needs more", blocks.len());
    let mut try_order = |order: &[u64]| {
        let mut direct = Direct::new(store.reader(), DIRECT_TAGS, Residency::model());
        for &n in order {
            let mut into = Vec::new();
            let read = direct.read(&mut sim, &beside_key(n), &mut into);
            assert!(matches!(read, Read::Found { .. }) && into == beside_value(n), "record {n} in order {order:?}: {read:?}");
        }
        let read = direct.own_counts().bytes;
        assert_eq!(read, block_bytes, "order {order:?}: the bytes read are the asked records' blocks', each once");
    };
    // Heap's algorithm over the six asked records: every order once.
    let mut order = asked.clone();
    let mut runs = 1;
    try_order(&order);
    let mut c = [0usize; COUNT as usize];
    let mut at = 1;
    // Bounded: Heap's algorithm makes `COUNT!` orders, each turn one swap or one step of `at`.
    while at < COUNT as usize {
        if c[at] < at {
            match at % 2 {
                0 => order.swap(0, at),
                _ => order.swap(c[at], at),
            }
            try_order(&order);
            runs += 1;
            c[at] += 1;
            at = 1;
        } else {
            c[at] = 0;
            at += 1;
        }
    }
    assert_eq!(runs, 720, "every order of six");
}

/// The control: each ask its own read, nothing taken from memory (`Plant::OneByOne`), is
/// refused by the oracle, under every seed: 64 records cost 64 reads where their blocks are a
/// few, and the bytes read are the records' alone, not their blocks'.
#[test]
fn the_oracle_catches_each_ask_its_own_read() {
    for seed in 0..16 {
        let order: Vec<u64> = (0..64).rev().collect();
        let honest = asked_one_by_one(seed, 64, &order, Plant::None).unwrap();
        costs_at_most_a_read_a_block(&honest, 64).unwrap_or_else(|why| panic!("seed {seed}, no bug planted: {why}"));
        let planted = asked_one_by_one(seed, 64, &order, Plant::OneByOne).unwrap();
        assert_eq!(planted.counts.reads, 64, "seed {seed}: the planted bug reads once an ask");
        assert!(
            costs_at_most_a_read_a_block(&planted, 64).is_err(),
            "seed {seed}: the oracle missed each ask its own read: {:?}",
            planted.counts
        );
    }
}

/// The crate version of the `p`th pack's unit in the tests of packs of their own.
fn crate_version(p: u64) -> ContentId {
    ContentId::of("blaze v2 store direct test crate version", &p.to_le_bytes())
}

/// A store holding `packs[p]` records in a pack of their own for each `p` (a unit's, a crate
/// version's), flushed: the records are `beside_key(n)`, numbered across the packs in order.
/// Each pack is asserted a file of its own.
fn packs_of_their_own(seed: u64, packs: &[u64]) -> (Sim, Store) {
    let mut sim = Sim::new(seed, Faults::default());
    let mut store = open(&mut sim);
    let mut n = 0;
    for (p, count) in packs.iter().enumerate() {
        let unit = store.unit(crate_version(p as u64));
        for _ in 0..*count {
            store.put_in(unit, beside_key(n), Kind::Query, &beside_value(n)).unwrap();
            n += 1;
        }
    }
    flush(&mut sim, &mut store);
    let probe = Direct::new(store.reader(), DIRECT_TAGS, Residency::model());
    let files: std::collections::BTreeSet<FileName> =
        (0..n).map(|n| probe.record_of(&beside_key(n)).expect("in a pack after the flush").0).collect();
    assert_eq!(files.len(), packs.len(), "each unit's records lie in a pack of their own");
    (sim, store)
}

/// A model of a page cache that holds, whole, each pack one of `records` lies in: as after a
/// daemon's restart, the packs the build before it wrote and read still in memory.
fn holding(store: &Store, records: impl IntoIterator<Item = u64>) -> Residency {
    let probe = Direct::new(store.reader(), DIRECT_TAGS, Residency::model());
    let mut residency = Residency::model();
    for n in records {
        let (file, _, _, usable) = probe.record_of(&beside_key(n)).expect("in a pack after the flush");
        residency.read(&file, 0, usable);
    }
    residency
}

/// Reads record `n` alone through `direct`: an error unless it is the value put.
fn read_beside(sim: &mut Sim, direct: &mut Direct, n: u64) -> Result<(), String> {
    let mut into = Vec::new();
    match direct.read(sim, &beside_key(n), &mut into) {
        Read::Found { len } if into == beside_value(n) && len == into.len() => Ok(()),
        other => Err(format!("record {n} read as {other:?}, {} bytes, not the value put", into.len())),
    }
}

/// Reads `order` as one batch through `direct`: an error unless each is the value put.
fn read_beside_batch(sim: &mut Sim, direct: &mut Direct, order: &[u64]) -> Result<(), String> {
    let keys: Vec<Key> = order.iter().map(|n| beside_key(*n)).collect();
    let mut into = Vec::new();
    let mut places = Vec::new();
    direct.read_many(sim, &keys, &mut into, &mut places);
    for (n, place) in order.iter().zip(&places) {
        match place {
            Placed::Found { at, len } if into.get(*at..at + len) == Some(&beside_value(*n)[..]) => {}
            other => return Err(format!("record {n} in a batch read as {other:?}, not the value put")),
        }
    }
    match places.len() == order.len() {
        true => Ok(()),
        false => Err(format!("a batch of {} gave {} answers", order.len(), places.len())),
    }
}

/// Packs of their own whose every page the page cache holds, each record asked by a call of
/// its own of a reader holding no mapping: the reader's counts, or why a value was not the one
/// put.
fn first_touch_of_held_packs(seed: u64, plant: Plant) -> Result<Counts, String> {
    const PACKS: [u64; 4] = [3, 1, 5, 2];
    let count: u64 = PACKS.iter().sum();
    let (mut sim, store) = packs_of_their_own(seed, &PACKS);
    let mut direct = Direct::new(store.reader(), DIRECT_TAGS, holding(&store, 0..count));
    direct.plant(plant);
    // Last first: each pack's records against the order they were put in.
    for n in (0..count).rev() {
        read_beside(&mut sim, &mut direct, n)?;
    }
    Ok(direct.own_counts())
}

/// The oracle over packs the page cache holds whole: no device read at all, the first touch
/// of each pack included; every record a hit; one wait (the round that maps every pack); and
/// every block of a first touch found in memory.
fn held_packs_cost_no_device_read(counts: &Counts, records: u64, packs: u64) -> Result<(), String> {
    if counts.reads != 0 || counts.blocks_read != 0 {
        return Err(format!("packs the page cache holds cost {} device reads: {counts:?}", counts.reads));
    }
    if counts.hits != records {
        return Err(format!("{records} records held, {} taken from memory: {counts:?}", counts.hits));
    }
    if counts.waits != 1 || counts.map_waits != 1 || counts.widest_maps != packs {
        return Err(format!("{packs} packs cost {} waits, {} of maps: {counts:?}", counts.waits, counts.map_waits));
    }
    if counts.first_touch_blocks < packs || counts.first_touch_held != counts.first_touch_blocks {
        return Err(format!(
            "{packs} packs touched first: {} of {} blocks found in memory: {counts:?}",
            counts.first_touch_held, counts.first_touch_blocks
        ));
    }
    Ok(())
}

/// The invariant: a pack whose blocks the page cache all holds costs no device read on its
/// first touch, nor after: the pack is mapped first (in the one round that maps every pack
/// of the store) and its pages asked of the system before any read, so each record is taken
/// from memory, checked as a read one is.
///
/// The control is the planted bug `Plant::ReadBeforeMap`, a first touch that reads the
/// record's window from the device before the system is asked: the oracle refuses it.
#[test]
fn a_pack_the_page_cache_holds_costs_no_device_read_on_its_first_touch() {
    for seed in 0..16 {
        let honest = first_touch_of_held_packs(seed, Plant::None).unwrap_or_else(|why| panic!("seed {seed}: {why}"));
        held_packs_cost_no_device_read(&honest, 11, 4).unwrap_or_else(|why| panic!("seed {seed}: {why}"));
        let planted = first_touch_of_held_packs(seed, Plant::ReadBeforeMap)
            .unwrap_or_else(|why| panic!("seed {seed}, read before map: {why}"));
        assert_eq!(planted.reads, 4, "seed {seed}: the planted bug reads once a pack's first touch");
        assert!(
            held_packs_cost_no_device_read(&planted, 11, 4).is_err(),
            "seed {seed}: the oracle missed a read before the map: {planted:?}"
        );
    }
}

/// `packs` records, each in a pack of its own, asked as one batch by a reader holding no
/// mapping, the page cache holding every pack (`warm`) or none: the reader's counts.
fn batch_across_packs(seed: u64, packs: u64, warm: bool, plant: Plant) -> Result<Counts, String> {
    let (mut sim, store) = packs_of_their_own(seed, &vec![1; packs as usize]);
    let residency = match warm {
        true => holding(&store, 0..packs),
        false => Residency::model(),
    };
    let mut direct = Direct::new(store.reader(), DIRECT_TAGS, residency);
    direct.plant(plant);
    let order: Vec<u64> = (0..packs).rev().collect();
    read_beside_batch(&mut sim, &mut direct, &order)?;
    Ok(direct.own_counts())
}

/// The oracle over a batch of `packs` records in as many unmapped packs: one round of maps,
/// as wide as the packs, and (when the page cache holds none of them) one round of reads, as
/// wide: two waits cold, one warm. Rounds are counted, not timed.
fn a_batch_costs_a_round_of_each(counts: &Counts, packs: u64, warm: bool) -> Result<(), String> {
    if counts.map_waits != 1 || counts.widest_maps != packs {
        return Err(format!("{packs} packs mapped in {} waits, the widest {}: {counts:?}", counts.map_waits, counts.widest_maps));
    }
    let (waits, reads) = match warm {
        true => (1, 0),
        false => (2, packs),
    };
    if counts.waits != waits || counts.reads != reads || counts.widest_reads != reads {
        return Err(format!(
            "{packs} records in {packs} packs ({}) cost {} waits and {} reads, the widest round {}, not {waits} and {reads}: \
             {counts:?}",
            if warm { "held" } else { "not held" },
            counts.waits,
            counts.reads,
            counts.widest_reads
        ));
    }
    Ok(())
}

/// The invariant: `n` records in `n` packs no reader mapped, asked as one batch, cost one
/// round of concurrent maps and one of concurrent reads (one round of maps alone when the
/// page cache holds them), whatever `n` (within [`MAPS_A_ROUND`]); asked one at a time this
/// was two waits a pack.
///
/// The control is the planted bug `Plant::MapEach`, each pack mapped in a wait of its own:
/// the oracle refuses it.
#[test]
fn records_in_many_packs_asked_as_one_batch_cost_one_round() {
    const PACKS: u64 = 12;
    for seed in 0..8 {
        for warm in [false, true] {
            let honest = batch_across_packs(seed, PACKS, warm, Plant::None).unwrap_or_else(|why| panic!("seed {seed}: {why}"));
            a_batch_costs_a_round_of_each(&honest, PACKS, warm).unwrap_or_else(|why| panic!("seed {seed}: {why}"));
            let planted =
                batch_across_packs(seed, PACKS, warm, Plant::MapEach).unwrap_or_else(|why| panic!("seed {seed}, map each: {why}"));
            assert_eq!(planted.map_waits, PACKS, "seed {seed}: the planted bug waits once a map");
            assert!(
                a_batch_costs_a_round_of_each(&planted, PACKS, warm).is_err(),
                "seed {seed}: the oracle missed a wait a map: {planted:?}"
            );
        }
    }
}

/// Every order of the records of [`ORDERED_PACKS`] (the first pack held by the page cache, the
/// others not, the first the shortest), asked by fresh readers of a store whose mappings are
/// dropped before each: one asking a record a call, one asking the order as one batch. Why an
/// order's bytes were not the ones put, if one's were not, and how many orders ran.
fn every_order_across_packs(plant: Plant) -> Result<u64, String> {
    // The asked records: 0 and 1 in pack 0, 2 and 3 in pack 1 (records 2 to 21), 22 and 23 in
    // pack 2 (22 to 41); the rest are not asked, there to make packs 1 and 2 longer than pack
    // 0, so another pack's mapping reaches past each record of it.
    let (mut sim, store) = packs_of_their_own(3, &ORDERED_PACKS);
    let mut order: Vec<u64> = vec![0, 1, 2, 3, 22, 23];
    let mut try_order = |order: &[u64]| -> Result<(), String> {
        let at = |why: String| format!("order {order:?}: {why}");
        store.direct_maps.clear();
        let mut direct = Direct::new(store.reader(), DIRECT_TAGS, holding(&store, [0]));
        direct.plant(plant);
        for &n in order {
            read_beside(&mut sim, &mut direct, n).map_err(at)?;
        }
        store.direct_maps.clear();
        let mut direct = Direct::new(store.reader(), DIRECT_TAGS, holding(&store, [0]));
        direct.plant(plant);
        read_beside_batch(&mut sim, &mut direct, order).map_err(at)
    };
    // Heap's algorithm over the six: every order once.
    let mut runs = 1;
    try_order(&order)?;
    let mut c = [0usize; 6];
    let mut at = 1;
    // Bounded: Heap's algorithm makes 6! orders, each turn one swap or one step of `at`.
    while at < 6 {
        if c[at] < at {
            match at % 2 {
                0 => order.swap(0, at),
                _ => order.swap(c[at], at),
            }
            try_order(&order)?;
            runs += 1;
            c[at] += 1;
            at = 1;
        } else {
            c[at] = 0;
            at += 1;
        }
    }
    Ok(runs)
}

/// The packs of [`every_order_across_packs`]: two records in the first, 20 in each other.
const ORDERED_PACKS: [u64; 3] = [2, 20, 20];

/// The invariant: the bytes a reader returns are the bytes put, for every order of asks of
/// records spread over packs some of which the page cache holds, asked a record a call or as
/// one batch: a mapping lent from the store's table is its own pack's, a record taken from
/// memory is checked as a read one is.
///
/// The control is the planted bug `Plant::OtherPacksMapping`, the table lending a mapping of
/// another pack: a held record is then taken from another pack's bytes, which its check
/// refuses, and the oracle sees the failure.
#[test]
fn the_bytes_returned_are_the_bytes_put_for_every_order_of_asks_across_packs() {
    assert_eq!(every_order_across_packs(Plant::None), Ok(720), "every order of six");
    assert!(every_order_across_packs(Plant::OtherPacksMapping).is_err(), "the oracle missed another pack's mapping lent");
}

/// The core alone: neighbours join, a far record starts a run, and every record's bytes are
/// where the plan says.
#[test]
fn the_plan_joins_neighbours_within_the_gap() {
    let mut plan = plan::Plan::new(0);
    plan.begin();
    let far = plan.want(0, 1 << 30, 100);
    let a = plan.want(0, 0, 100);
    let b = plan.want(0, 100 + plan::GAP_MOST, 50);
    let other = plan.want(1, 0, 10);
    let total = plan.plan();
    assert_eq!(plan.runs(), 3, "a and b join; far and the other file's do not");
    assert_eq!(total, 100 + plan::GAP_MOST + 50 + 100 + 10);
    let mut effects = Vec::new();
    let mut region = vec![0u8; total as usize];
    // SAFETY: the buffer outlives the guard, and no effect on it is ever run: the test plays
    // the driver.
    let registered =
        unsafe { Spans::register("direct plan test", NonNull::new(region.as_mut_ptr()).unwrap(), region.len()) }
            .unwrap();
    plan.issue(registered.region(), |file| name(&format!("f{file}")), &mut effects);
    assert_eq!(effects.len(), 3);
    for effect in effects {
        let Effect::ReadInto { into, tag, .. } = effect else { unreachable!() };
        assert!(plan.complete(Completion { tag, done: Done::ReadInto { len: into.len } }).is_none());
    }
    assert_eq!(plan.left(), 0);
    assert_eq!(plan.outcome(a), Ok(0));
    assert_eq!(plan.outcome(b), Ok(100 + plan::GAP_MOST));
    assert_eq!(plan.outcome(far), Ok(100 + plan::GAP_MOST + 50));
    assert_eq!(plan.outcome(other), Ok(100 + plan::GAP_MOST + 50 + 100));
}
