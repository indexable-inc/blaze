//! The cores under simulation: a model store that crashes, bodies that are cut and flipped,
//! sources that fail, stall and lie, each run under many seeds on a generator of its own
//! (the crate has no dependency, so not Hegel's; a failing seed is printed and replays).
//! Every invariant test has a control that plants the bug its oracle is there to catch.

use std::collections::BTreeSet;

use crate::{
    Hashes, Id, Manifest, Record, Refused, Signatures, Trust, cbor,
    fetch::{self, Answer, Effect, Fetch, Outcome, Source, Why},
    head::{Head, Vouched},
    import::{self, Body, Import, Verified},
    layout, manifest,
    peer::{self, Asked, Endpoint, Statement},
};

mod model;

use model::{Cut, Model, Rng, TestHashes, origin, packed};

const SEEDS: u64 = 200;
/// The kinds the tests' imports admit.
const ADMIT: &[u8] = &[0, 1, 2, 3, 4];

fn imported(model: &mut Model, manifest: &Manifest, body: &mut Cut) -> Result<import::Summary, Refused> {
    let mut import = Import::new(manifest.clone(), ADMIT)?;
    import::run(&mut import, model, body, &TestHashes)
}

// ---- the manifest

/// The invariant: one selection has one manifest, whatever order its records were found in.
/// The control: the same records written in another order are bytes a reader refuses.
#[test]
fn exporting_one_selection_twice_gives_identical_bytes() {
    for seed in 0..SEEDS {
        let mut rng = Rng::new(seed);
        let records = origin(&mut rng, 40);
        let listed: Vec<Record> = records.iter().map(|(record, _)| *record).collect();
        let mut shuffled = listed.clone();
        rng.shuffle(&mut shuffled);
        shuffled.push(shuffled[0]);
        let first = Manifest::new("toolchain", "t1", listed).unwrap();
        let second = Manifest::new("toolchain", "t1", shuffled.clone()).unwrap();
        assert_eq!(first.encode(), second.encode(), "seed {seed}");
        assert_eq!(Manifest::decode(&first.encode()).unwrap(), first, "seed {seed}");
        let (a, b) = (packed(&first, &records), packed(&second, &records));
        assert_eq!(a, b, "seed {seed}: the bodies differ");
        // The control: the order a walk happened to find them in.
        shuffled.pop();
        if shuffled.windows(2).all(|pair| pair[0].key < pair[1].key) {
            continue;
        }
        let unsorted = model::raw_manifest(manifest::LAYOUT, 0, "toolchain", "t1", &shuffled);
        assert!(
            matches!(Manifest::decode(&unsorted), Err(Refused::Malformed { .. })),
            "seed {seed}: a manifest in another order was read"
        );
    }
}

#[test]
fn two_records_of_one_key_that_differ_are_refused_naming_the_key() {
    let mut rng = Rng::new(7);
    let records = origin(&mut rng, 3);
    let mut listed: Vec<Record> = records.iter().map(|(record, _)| *record).collect();
    let mut other = listed[1];
    other.content = Id([9; 32]);
    listed.push(other);
    match Manifest::new("bstd", "t1", listed) {
        Err(Refused::Differs { key, .. }) => assert_eq!(key, other.key),
        got => panic!("{got:?}"),
    }
}

#[test]
fn a_previous_or_foreign_layout_is_refused_by_name() {
    let record = Record { key: Id([1; 32]), kind: 1, len: 3, content: Id([2; 32]) };
    let newer = model::raw_manifest(2, 0, "toolchain", "t1", &[record]);
    match Manifest::decode(&newer) {
        Err(Refused::Layout { found, .. }) => assert!(found.contains("layout 2"), "{found}"),
        got => panic!("{got:?}"),
    }
    let coded = model::raw_manifest(manifest::LAYOUT, 5, "toolchain", "t1", &[record]);
    match Manifest::decode(&coded) {
        Err(Refused::Layout { found, .. }) => assert!(found.contains("coding 5"), "{found}"),
        got => panic!("{got:?}"),
    }
    for foreign in [&b"PK\x03\x04 a zip"[..], &b""[..], &[0x86, 0x63, b'f', b'o', b'o'][..]] {
        match Manifest::decode(foreign) {
            Err(Refused::Layout { found, .. }) => assert!(found.contains("not a blaze pack manifest")),
            got => panic!("{got:?}"),
        }
    }
    // The control: the same writer with this layout's numbers is read.
    let this = model::raw_manifest(manifest::LAYOUT, 0, "toolchain", "t1", &[record]);
    assert!(Manifest::decode(&this).is_ok());
}

#[test]
fn a_path_escaping_name_or_an_oversized_length_is_refused() {
    let record = Record { key: Id([1; 32]), kind: 1, len: 3, content: Id([2; 32]) };
    let long_name = "a".repeat(layout::NAME_MOST + 1);
    for name in ["../x", "a/b", ".hidden", "", "-rf", "a\\b", "A", "a b", "a\0", long_name.as_str()] {
        let as_pin = model::raw_manifest(manifest::LAYOUT, 0, name, "t1", &[record]);
        assert!(matches!(Manifest::decode(&as_pin), Err(Refused::Name { .. })), "pin {name:?}");
        let as_scope = model::raw_manifest(manifest::LAYOUT, 0, "toolchain", name, &[record]);
        assert!(matches!(Manifest::decode(&as_scope), Err(Refused::Name { .. })), "scope {name:?}");
        assert!(layout::head_path(name, "toolchain").is_err(), "{name:?}");
        assert!(Manifest::new(name, "t1", vec![record]).is_err(), "{name:?}");
    }
    assert_eq!(layout::head_path("t1", "toolchain").unwrap(), "v1/t1/toolchain.head");
    let long = Record { len: manifest::RECORD_MOST + 1, ..record };
    let bytes = model::raw_manifest(manifest::LAYOUT, 0, "toolchain", "t1", &[long]);
    assert!(matches!(Manifest::decode(&bytes), Err(Refused::TooLarge { .. })));
    // A count that promises more records than the input has bytes: refused before a table
    // is sized by it.
    let mut lying = Vec::new();
    cbor::array(&mut lying, 6);
    cbor::text(&mut lying, "blaze pack manifest");
    cbor::uint(&mut lying, manifest::LAYOUT);
    cbor::uint(&mut lying, 0);
    cbor::text(&mut lying, "toolchain");
    cbor::text(&mut lying, "t1");
    cbor::array(&mut lying, manifest::RECORDS_MOST - 1);
    assert!(matches!(Manifest::decode(&lying), Err(Refused::Malformed { .. })));
    // The control: the same record at the bound is read.
    let most = Record { len: manifest::RECORD_MOST, ..record };
    let bytes = model::raw_manifest(manifest::LAYOUT, 0, "toolchain", "t1", &[most]);
    assert!(Manifest::decode(&bytes).is_ok());
}

#[test]
fn a_number_not_in_its_shortest_form_is_refused() {
    let mut reader = cbor::Reader::new(&[0x18, 0x05]);
    assert!(reader.uint().is_err());
    let mut reader = cbor::Reader::new(&[0x18, 0x18]);
    assert_eq!(reader.uint().unwrap(), 24);
    let mut reader = cbor::Reader::new(&[0x5f]);
    assert!(reader.bytes().is_err(), "an indefinite length was read");
}

#[test]
fn a_manifest_that_is_not_the_packs_is_refused_before_it_is_read() {
    let mut rng = Rng::new(3);
    let records = origin(&mut rng, 5);
    let manifest = Manifest::new("toolchain", "t1", records.iter().map(|(it, _)| *it).collect()).unwrap();
    let bytes = manifest.encode();
    let pack = TestHashes.manifest(&bytes);
    assert_eq!(Manifest::named(&pack, &bytes, &TestHashes).unwrap(), manifest);
    let mut flipped = bytes.clone();
    let at = flipped.len() / 2;
    flipped[at] ^= 1;
    assert!(matches!(Manifest::named(&pack, &flipped, &TestHashes), Err(Refused::Manifest { .. })));
}

/// The invariant: a span index says where each span's frame is and never asks for more than
/// its bounds; one with another number of frames than its body has spans, a span or a frame
/// past its bound, is refused as it is read.
#[test]
fn a_span_index_round_trips_and_is_bounded() {
    use crate::spans::{self, Spans};
    let index = Spans::new(100, 250, &[40, 100, 7]).unwrap();
    assert_eq!(Spans::decode(&index.encode()).unwrap(), index);
    assert_eq!((index.frame(0), index.frame(1), index.frame(2)), ((0, 40, 100), (40, 100, 100), (140, 7, 50)));
    assert_eq!(index.covering(0, 250), (0, 3));
    assert_eq!(index.covering(99, 2), (0, 2));
    assert_eq!(index.covering(100, 100), (1, 2));
    assert_eq!(index.covering(249, 50), (2, 3));
    assert_eq!((index.covering(250, 1), index.covering(10, 0)), ((0, 0), (0, 0)));
    assert!(Spans::new(100, 250, &[40, 100]).is_err(), "fewer frames than spans");
    assert!(Spans::new(100, 250, &[40, 100, 7, 1]).is_err(), "more frames than spans");
    assert!(Spans::new(0, 250, &[]).is_err());
    assert!(Spans::new(spans::SPAN_MOST + 1, 1, &[1]).is_err());
    assert!(Spans::new(100, 250, &[40, 100 + spans::FRAME_SLACK + 1, 7]).is_err());
    assert!(Spans::new(100, 0, &[]).is_ok());
    let mut after = index.encode();
    after.push(0);
    assert!(Spans::decode(&after).is_err());
    assert!(matches!(Spans::decode(b"nope"), Err(Refused::Layout { .. })));
}

// ---- the import

/// The invariant: an export imported into an empty store leaves, under every key of the
/// manifest, the origin's bytes, and a build then misses none of them. A second import puts
/// nothing: a record already present is not written again.
#[test]
fn a_round_trip_leaves_the_origins_bytes_and_a_second_import_puts_nothing() {
    for seed in 0..SEEDS {
        let mut rng = Rng::new(seed);
        let records = origin(&mut rng, 30);
        let manifest = Manifest::new("toolchain", "t1", records.iter().map(|(it, _)| *it).collect()).unwrap();
        let body = packed(&manifest, &records);
        let mut model = Model::new(&manifest);
        let summary = imported(&mut model, &manifest, &mut Cut::whole(&body)).unwrap();
        assert_eq!(summary.put as usize, records.len(), "seed {seed}");
        assert_eq!(model.violations, 0, "seed {seed}");
        model.crash();
        for (record, bytes) in &records {
            assert_eq!(model.durable.get(&record.key).map(|it| &it.1), Some(bytes), "seed {seed}");
        }
        assert_eq!(model.missing(&manifest), 0, "seed {seed}");
        let puts = model.puts;
        let again = imported(&mut model, &manifest, &mut Cut::whole(&body)).unwrap();
        assert_eq!((again.put, again.present as usize), (0, records.len()), "seed {seed}");
        assert_eq!(model.puts, puts, "seed {seed}: a present record was written again");
    }
}

/// One byte of the body flipped, anywhere: what `plant` (records put unchecked) leaves.
fn flipped(seed: u64, plant: bool) -> (u64, Result<import::Summary, Refused>, Id) {
    let mut rng = Rng::new(seed);
    let records = origin(&mut rng, 20);
    let manifest = Manifest::new("toolchain", "t1", records.iter().map(|(it, _)| *it).collect()).unwrap();
    let mut body = packed(&manifest, &records);
    let at = rng.below(body.len() as u64);
    body[at as usize] ^= 1 << rng.below(8);
    let hit = (0..manifest.records().len() as u32)
        .find(|index| at < manifest.at(*index) + manifest.records()[*index as usize].len)
        .unwrap();
    let mut model = Model::new(&manifest);
    let mut import = Import::new(manifest.clone(), ADMIT).unwrap();
    import.plant_unchecked = plant;
    let done = import::run(&mut import, &mut model, &mut Cut::whole(&body), &TestHashes);
    (model.violations, done, manifest.records()[hit as usize].key)
}

/// The invariant: no record whose bytes are not the manifest's is ever put, and the import
/// stops naming the record the flipped byte is in. The control puts unchecked: the oracle
/// (the model checks every put against the manifest) must see it.
#[test]
fn a_flipped_byte_anywhere_in_a_pack_is_refused_naming_the_record() {
    let mut caught = 0;
    for seed in 0..SEEDS {
        let (violations, done, hit) = flipped(seed, false);
        assert_eq!(violations, 0, "seed {seed}: an unchecked record was put");
        match done {
            Err(Refused::Record { key, .. }) => assert_eq!(key, hit, "seed {seed}"),
            got => panic!("seed {seed}: {got:?}"),
        }
        let (violations, _, _) = flipped(seed, true);
        caught += u64::from(violations > 0);
    }
    assert_eq!(caught, SEEDS, "the control put flipped bytes and the oracle saw {caught} of {SEEDS}");
}

/// The invariant: a body that ends early stops the import naming the record it ends in,
/// nothing unchecked is put, and the same import with the whole body then completes.
#[test]
fn a_truncated_transfer_is_refused_and_a_second_import_completes() {
    for seed in 0..SEEDS {
        let mut rng = Rng::new(seed);
        let records = origin(&mut rng, 20);
        let manifest = Manifest::new("toolchain", "t1", records.iter().map(|(it, _)| *it).collect()).unwrap();
        let body = packed(&manifest, &records);
        let cut = rng.below(body.len() as u64);
        let mut model = Model::new(&manifest);
        match imported(&mut model, &manifest, &mut Cut::at(&body, cut)) {
            Err(Refused::Truncated { index, .. }) => {
                let record = manifest.records()[index as usize];
                assert!(manifest.at(index) + record.len > cut, "seed {seed}: named a whole record");
            }
            got => panic!("seed {seed}: {got:?}"),
        }
        assert_eq!(model.violations, 0, "seed {seed}");
        model.crash();
        assert!(model.durable.is_empty(), "seed {seed}: an import that did not end left records durable");
        imported(&mut model, &manifest, &mut Cut::whole(&body)).unwrap();
        assert_eq!((model.violations, model.missing(&manifest)), (0, 0), "seed {seed}");
    }
}

/// A crash at every step of an import in turn (the store's operation `fail_at` fails and
/// what was not flushed is gone): what is visible at any time, and after the import is run
/// again. `plant`: records are put unchecked and the body has a flipped byte.
fn crashed_at_every_step(seed: u64, plant: bool) -> u64 {
    let mut rng = Rng::new(seed);
    let records = origin(&mut rng, 12);
    let manifest = Manifest::new("toolchain", "t1", records.iter().map(|(it, _)| *it).collect()).unwrap();
    let mut body = packed(&manifest, &records);
    if plant {
        let at = rng.below(body.len() as u64) as usize;
        body[at] ^= 0x40;
    }
    let mut violations = 0;
    // Looks, puts, the pin and the flush: one more than that many crash points.
    let steps = 2 * records.len() as u64 + 2;
    for fail_at in 0..=steps {
        let mut model = Model::new(&manifest);
        model.fail_at = Some(fail_at);
        let mut import = Import::new(manifest.clone(), ADMIT).unwrap();
        import.plant_unchecked = plant;
        let first = import::run(&mut import, &mut model, &mut Cut::whole(&body), &TestHashes);
        violations += model.violations;
        model.crash();
        if plant {
            continue;
        }
        if fail_at < steps {
            assert!(matches!(first, Err(Refused::Driver { .. })), "seed {seed} step {fail_at}: {first:?}");
            assert!(model.durable.is_empty(), "seed {seed} step {fail_at}: durable before the flush");
        }
        // Whatever is durable is pinned: records never outlive a crash without their pin.
        let pinned = model.pins.get("toolchain").cloned().unwrap_or_default();
        assert!(model.durable.keys().all(|key| pinned.contains(key)), "seed {seed} step {fail_at}");
        model.fail_at = None;
        imported(&mut model, &manifest, &mut Cut::whole(&body)).unwrap();
        model.crash();
        assert_eq!((model.violations, model.missing(&manifest)), (0, 0), "seed {seed} step {fail_at}");
        for (record, bytes) in &records {
            assert_eq!(model.durable.get(&record.key).map(|it| &it.1), Some(bytes));
        }
    }
    violations
}

/// The invariant: through a crash at any step no unchecked record is ever visible, nothing
/// is durable without its pin, and the import run again completes with the origin's bytes.
/// The control puts unchecked over a flipped body.
#[test]
fn a_crash_mid_import_shows_no_unverified_record_and_a_re_import_completes() {
    let mut caught = 0;
    for seed in 0..40 {
        assert_eq!(crashed_at_every_step(seed, false), 0, "seed {seed}");
        caught += u64::from(crashed_at_every_step(seed, true) > 0);
    }
    assert_eq!(caught, 40, "the control's unchecked puts were seen in {caught} of 40 seeds");
}

/// Import, then every unrooted record collected, then a build of the same sources: how many
/// of the manifest's records it has to make. `pin`: the import pins what it imports.
fn collected_after_import(seed: u64, pin: bool) -> usize {
    let mut rng = Rng::new(seed);
    let records = origin(&mut rng, 25);
    let manifest = Manifest::new("toolchain", "t1", records.iter().map(|(it, _)| *it).collect()).unwrap();
    let body = packed(&manifest, &records);
    let mut model = Model::new(&manifest);
    // A pin of the name from before, with a key of its own: the import adds, it does not
    // replace.
    model.pins.entry("toolchain".to_owned()).or_default().insert(Id([0xee; 32]));
    let mut import = Import::new(manifest.clone(), ADMIT).unwrap();
    import.plant_no_pin = !pin;
    import::run(&mut import, &mut model, &mut Cut::whole(&body), &TestHashes).unwrap();
    model.collect_unrooted();
    assert!(model.pins["toolchain"].contains(&Id([0xee; 32])), "seed {seed}: the pin's earlier keys went");
    model.missing(&manifest)
}

/// The invariant: what an import made visible is pinned by the same commit, so a collection
/// of everything unrooted between the import and the first build takes none of it and the
/// build makes 0 units. The control imports without the pin: the collection takes them all.
#[test]
fn an_import_pins_what_it_imports_so_a_collection_before_the_first_build_takes_nothing() {
    for seed in 0..SEEDS {
        assert_eq!(collected_after_import(seed, true), 0, "seed {seed}");
        assert_eq!(collected_after_import(seed, false), 25, "seed {seed}: the control kept records with no pin");
    }
}

#[test]
fn a_kind_not_admitted_refuses_the_pack_before_anything_is_put() {
    let mut rng = Rng::new(11);
    let mut records = origin(&mut rng, 6);
    records[4].0.kind = 6;
    let manifest = Manifest::new("toolchain", "t1", records.iter().map(|(it, _)| *it).collect()).unwrap();
    match Import::new(manifest, ADMIT) {
        Err(Refused::Kind { key, kind: 6 }) => assert_eq!(key, records[4].0.key),
        got => panic!("{:?}", got.map(|_| ())),
    }
}

#[test]
fn a_key_the_store_keeps_with_other_bytes_refuses_the_import_naming_it() {
    let mut rng = Rng::new(12);
    let records = origin(&mut rng, 6);
    let manifest = Manifest::new("toolchain", "t1", records.iter().map(|(it, _)| *it).collect()).unwrap();
    let body = packed(&manifest, &records);
    let mut model = Model::new(&manifest);
    let clash = manifest.records()[3].key;
    model.durable.insert(clash, (1, b"other bytes".to_vec()));
    match imported(&mut model, &manifest, &mut Cut::whole(&body)) {
        Err(Refused::Differs { key, .. }) => assert_eq!(key, clash),
        got => panic!("{got:?}"),
    }
    assert_eq!(model.durable[&clash].1, b"other bytes");
}

// ---- the fetch

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Does {
    Honest,
    /// Has every other record only.
    Half,
    Fails,
    Silent,
    Lies,
}

struct Fetched {
    outcome: Outcome,
    /// Records kept whose bytes are not the origin's: the oracle's count.
    wrong: u32,
    kept: BTreeSet<u32>,
    dropped: Vec<(Source, Why)>,
}

/// A fetch of `count` records from `sources`, answers given in an order `seed` picks.
fn fetched(seed: u64, count: usize, sources: &[Does], plant: bool) -> Fetched {
    let mut rng = Rng::new(seed);
    let records = origin(&mut rng, count);
    let wants: Vec<Record> = records.iter().map(|(it, _)| *it).collect();
    let mut now = core::time::Duration::ZERO;
    let mut fetch = Fetch::new(now, &wants);
    fetch.plant_unchecked = plant;
    for at in 0..sources.len() {
        fetch.add_source(Source(at as u32));
    }
    let mut out = Fetched { outcome: Outcome::Fetching, wrong: 0, kept: BTreeSet::new(), dropped: Vec::new() };
    let mut waiting: Vec<(fetch::Ask, Source, u32)> = Vec::new();
    // Each turn answers an ask or passes a deadline, and each record is asked of each
    // source at most once: the bound is the proof the fetch ends.
    for _ in 0..(count * sources.len() + sources.len() + 2) * 2 {
        for effect in fetch.take_effects() {
            match effect {
                Effect::Ask { ask, source, index } => waiting.push((ask, source, index)),
                Effect::Keep { index, bytes } => {
                    let record = *fetch.want(index);
                    let (_, truth) = records.iter().find(|(it, _)| it.key == record.key).unwrap();
                    out.wrong += u32::from(&bytes != truth);
                    assert!(out.kept.insert(index), "seed {seed}: record {index} kept twice");
                }
                Effect::Dropped { source, why } => out.dropped.push((source, why)),
            }
        }
        if *fetch.outcome() != Outcome::Fetching {
            break;
        }
        let answerable: Vec<usize> = (0..waiting.len())
            .filter(|at| sources[waiting[*at].1.0 as usize] != Does::Silent)
            .collect();
        if answerable.is_empty() {
            now = fetch.next_deadline().expect("a fetch that is not done waits on something");
            fetch.advance_clock(now);
            waiting.retain(|(_, source, _)| sources[source.0 as usize] != Does::Silent);
            continue;
        }
        let (ask, source, index) = waiting.swap_remove(answerable[rng.below(answerable.len() as u64) as usize]);
        let record = *fetch.want(index);
        let (_, truth) = records.iter().find(|(it, _)| it.key == record.key).unwrap();
        let answer = match sources[source.0 as usize] {
            Does::Honest => Answer::Bytes(truth.clone()),
            Does::Half if index % 2 == 0 => Answer::Bytes(truth.clone()),
            Does::Half => Answer::Lacks,
            Does::Fails => Answer::Failed,
            Does::Lies => {
                let mut bytes = truth.clone();
                bytes[0] ^= 1;
                Answer::Bytes(bytes)
            }
            Does::Silent => unreachable!(),
        };
        fetch.answered(ask, answer, &TestHashes);
    }
    out.outcome = fetch.outcome().clone();
    out
}

/// The invariant: nothing is kept that is not the record's bytes; a source that lies ends
/// the fetch naming it and the key, with nothing of the lie kept. The control keeps
/// unchecked: the oracle (each kept record against the origin) must see it.
#[test]
fn of_two_sources_one_lying_the_lie_is_named_and_nothing_of_it_is_kept() {
    let mut caught = 0;
    for seed in 0..SEEDS {
        let got = fetched(seed, 16, &[Does::Honest, Does::Lies], false);
        assert_eq!(got.wrong, 0, "seed {seed}: a lie was kept");
        match got.outcome {
            Outcome::Lied { source, want, got: said, .. } => {
                assert_eq!(source, Source(1), "seed {seed}");
                assert_ne!(want, said);
            }
            other => panic!("seed {seed}: {other:?}"),
        }
        assert!(got.dropped.contains(&(Source(1), Why::Lied)), "seed {seed}");
        let control = fetched(seed, 16, &[Does::Honest, Does::Lies], true);
        caught += u64::from(control.wrong > 0);
    }
    assert_eq!(caught, SEEDS, "the control kept lies and the oracle saw {caught} of {SEEDS}");
}

/// The invariant: a source that fails or never answers is dropped once, said once, and what
/// it was asked comes from the sources left; the fetch ends within its bound.
#[test]
fn a_source_that_fails_or_stalls_is_dropped_once_and_the_rest_is_fetched() {
    for seed in 0..SEEDS {
        for bad in [Does::Fails, Does::Silent] {
            let got = fetched(seed, 20, &[bad, Does::Honest], false);
            assert_eq!(got.outcome, Outcome::Done { kept: 20, missed: 0 }, "seed {seed} {bad:?}");
            assert_eq!(got.wrong, 0);
            let drops: Vec<_> = got.dropped.iter().filter(|(source, _)| *source == Source(0)).collect();
            assert_eq!(drops.len(), 1, "seed {seed} {bad:?}: {drops:?}");
        }
    }
}

/// The invariant: a record no source has is a miss, an outcome and not an error; with every
/// source failing, everything is a miss.
#[test]
fn a_record_no_source_has_is_a_miss() {
    for seed in 0..SEEDS {
        let got = fetched(seed, 20, &[Does::Half, Does::Half], false);
        assert_eq!(got.outcome, Outcome::Done { kept: 10, missed: 10 }, "seed {seed}");
        assert!(got.kept.iter().all(|index| index % 2 == 0));
        let got = fetched(seed, 20, &[Does::Fails, Does::Silent], false);
        assert_eq!(got.outcome, Outcome::Done { kept: 0, missed: 20 }, "seed {seed}");
        // The control: one honest source beside them has every record.
        let got = fetched(seed, 20, &[Does::Half, Does::Honest], false);
        assert_eq!(got.outcome, Outcome::Done { kept: 20, missed: 0 }, "seed {seed}");
    }
}

// ---- the head and the peer

/// A signature scheme for the tests: the signature is a hash of the signer and the message
/// (so "secret" keys are the public ones: it tests the checks, not the cipher, which is the
/// driver's Ed25519).
struct TestSignatures;

fn test_sign(signer: &[u8; 32], message: &[u8]) -> [u8; 64] {
    let mut input = signer.to_vec();
    input.extend_from_slice(message);
    let half = TestHashes.content(&input).0;
    let mut out = [0; 64];
    out[..32].copy_from_slice(&half);
    out[32..].copy_from_slice(&half);
    out
}

impl Signatures for TestSignatures {
    fn verify(&self, signer: &[u8; 32], message: &[u8], signature: &[u8; 64]) -> bool {
        test_sign(signer, message) == *signature
    }
}

/// The invariant: a head is taken only when a trusted key's signature over exactly its
/// fields verifies, or when it is unsigned and the user said so; a signature that fails is
/// never read as an unsigned head.
#[test]
fn a_head_is_taken_only_under_a_trusted_signature_or_the_users_word() {
    let mut rng = Rng::new(5);
    let records = origin(&mut rng, 4);
    let manifest = Manifest::new("toolchain", "t1", records.iter().map(|(it, _)| *it).collect()).unwrap();
    let bytes = manifest.encode();
    let head = Head::of(&manifest, TestHashes.manifest(&bytes), &bytes);
    head.agrees(&manifest).unwrap();
    let (release, stranger) = ([7u8; 32], [8u8; 32]);
    let trust = Trust::new(vec![release]);
    let sign = |head: &Head, key: [u8; 32]| head.clone().signed_by(key, test_sign(&key, &head.message(&key)));

    let signed = sign(&head, release);
    let round = Head::decode(&signed.encode()).unwrap();
    assert_eq!(round, signed);
    assert_eq!(round.vouched(&trust, &TestSignatures, false), Ok(Vouched::Signer(release)));

    assert!(matches!(head.vouched(&trust, &TestSignatures, false), Err(Refused::Untrusted { .. })));
    assert_eq!(head.vouched(&trust, &TestSignatures, true), Ok(Vouched::User));
    for unsigned in [false, true] {
        let by_stranger = sign(&head, stranger);
        assert!(by_stranger.vouched(&trust, &TestSignatures, unsigned).is_err(), "a stranger's head was taken");
        // Every field is under the signature: another pack, pin, scope or length fails it.
        let mut moved = signed.clone();
        moved.pack = Id([1; 32]);
        assert!(moved.vouched(&trust, &TestSignatures, unsigned).is_err());
        let mut renamed = signed.clone();
        renamed.pin = "bstd".to_owned();
        assert!(renamed.vouched(&trust, &TestSignatures, unsigned).is_err());
        let mut longer = signed.clone();
        longer.body_len += 1;
        assert!(longer.vouched(&trust, &TestSignatures, unsigned).is_err());
        // A trusted key named as the signer over a stranger's signature.
        let mut claimed = by_stranger.clone();
        claimed.signed = Some((release, by_stranger.signed.unwrap().1));
        assert!(claimed.vouched(&trust, &TestSignatures, unsigned).is_err());
    }
    let mut other = signed.clone();
    other.records += 1;
    assert!(other.agrees(&manifest).is_err());
    let mut bytes = signed.encode();
    bytes.push(0);
    assert!(Head::decode(&bytes).is_err(), "bytes after a head were read");
}

/// The invariant: an endpoint answers a request only when a listed key signed exactly its
/// path; no address, no unsigned request and no other key's signature reads anything.
#[test]
fn an_endpoint_answers_only_requests_a_listed_key_signed() {
    let untrusted = |got: Result<Endpoint, Refused>| matches!(got, Err(Refused::Untrusted { .. }));
    let (reader, stranger) = ([5u8; 32], [6u8; 32]);
    assert!(untrusted(Endpoint::new("0.0.0.0:7420", vec![reader])));
    assert!(untrusted(Endpoint::new("[::]:7420", vec![reader])));
    assert!(untrusted(Endpoint::new("10.0.0.4:7420", Vec::new())));
    assert!(Endpoint::new("10.0.0.4", vec![reader]).is_err());
    let endpoint = Endpoint::new("127.0.0.1:7420", vec![reader]).unwrap();
    let key = Id([0xab; 32]);
    let path = peer::record_path(&key);
    let head = |signer: Option<[u8; 32]>, claimed: [u8; 32], signed_path: &str| {
        let auth = match signer {
            Some(signer) => format!(
                "{}: {}\r\n{}: {}\r\n",
                peer::CALLER,
                peer::hex(&claimed),
                peer::SIGNATURE,
                peer::hex(&test_sign(&signer, &peer::request_message(signed_path)))
            ),
            None => String::new(),
        };
        format!("GET {path} HTTP/1.1\r\nHost: x\r\n{auth}\r\n")
    };
    let lets = |head: String| endpoint.lets(&peer::asked(head.as_bytes()).unwrap(), &TestSignatures);
    assert!(lets(head(Some(reader), reader, &path)));
    // The controls: no signature; a stranger's; a stranger's under the reader's name; the
    // reader's signature of another path.
    assert!(!lets(head(None, reader, &path)));
    assert!(!lets(head(Some(stranger), stranger, &path)));
    assert!(!lets(head(Some(stranger), reader, &path)));
    assert!(!lets(head(Some(reader), reader, &peer::record_path(&Id([1; 32])))));
}

#[test]
fn a_request_names_keys_and_nothing_else() {
    let key = Id([0xab; 32]);
    let other = Id([0xcd; 32]);
    let get = |path: &str, more: &str| {
        let head = format!("GET {path} HTTP/1.1\r\nHost: x\r\n{more}\r\n");
        peer::asked(head.as_bytes()).map(|request| request.asked).map_err(|it| it.0)
    };
    assert_eq!(get(&peer::has_path(&[key]), ""), Ok(Asked::Has { keys: vec![key] }));
    assert_eq!(get(&peer::has_path(&[key, other]), ""), Ok(Asked::Has { keys: vec![key, other] }));
    assert_eq!(get(&peer::record_path(&key), ""), Ok(Asked::Record { key, range: None }));
    assert_eq!(
        get(&peer::record_path(&key), "Range: bytes=4-9\r\n"),
        Ok(Asked::Record { key, range: Some((4, 9)) })
    );
    let under = format!("/v1/record/{key}/x");
    let many = peer::has_path(&vec![key; peer::HAS_MOST + 1]);
    for path in ["/v1/record/../../etc/passwd", "/v1/record/", "/", "/v1/has/ABCD", "/v2/has/00", under.as_str()] {
        assert_eq!(get(path, ""), Err(404), "{path}");
    }
    assert!(get(&many, "").is_err(), "more keys than one `has` takes were read");
    assert_eq!(get(&peer::record_path(&key), "Range: bytes=9-4\r\n"), Err(416));
    assert_eq!(peer::asked(b"POST /v1/has/00 HTTP/1.1\r\n\r\n").map_err(|it| it.0), Err(405));
    assert_eq!(peer::asked(&vec![b'a'; peer::HEAD_MOST + 1]).map_err(|it| it.0), Err(431));
    assert_eq!(peer::slice(10, Some((4, 9))), Ok((4, 10)));
    assert_eq!(peer::slice(10, Some((4, 10))).map_err(|it| it.0), Err(416));
    assert_eq!(peer::slice(10, None), Ok((0, 10)));
    // A record longer than one answer is read by ranges, never whole.
    assert_eq!(peer::slice(peer::RANGE_MOST + 1, None).map_err(|it| it.0), Err(416));
    assert_eq!(peer::slice(peer::RANGE_MOST + 1, Some((1, peer::RANGE_MOST))), Ok((1, peer::RANGE_MOST + 1)));
}

/// The answer to a `has` of many keys: each statement is read back as it was written, and a
/// list of more than one request may ask is refused before it is read.
#[test]
fn a_batch_of_statements_round_trips_and_is_bounded() {
    let signer = [3u8; 32];
    let each: Vec<Statement> = (0..5u8)
        .map(|n| {
            let record = Record { key: Id([n; 32]), kind: 1, len: u64::from(n) * 9, content: Id([n + 1; 32]) };
            Statement { record, signer, signature: test_sign(&signer, &Statement::message(&record, &signer)) }
        })
        .collect();
    let bytes = peer::statements(&each.iter().map(Statement::encode).collect::<Vec<_>>());
    assert_eq!(peer::statements_of(&bytes).unwrap(), each);
    assert_eq!(peer::statements_of(&peer::statements(&[])).unwrap(), Vec::new());
    let too_many = vec![each[0].encode(); peer::HAS_MOST + 1];
    assert!(matches!(peer::statements_of(&peer::statements(&too_many)), Err(Refused::TooLarge { .. })));
    let mut after = bytes.clone();
    after.push(0);
    assert!(peer::statements_of(&after).is_err());
}

/// The invariant: a statement is taken for a key only when it is about that key and a
/// trusted key signed exactly it.
#[test]
fn a_peers_statement_is_taken_only_about_its_key_under_a_trusted_signature() {
    let record = Record { key: Id([1; 32]), kind: 1, len: 40, content: Id([2; 32]) };
    let (peer_key, stranger) = ([3u8; 32], [4u8; 32]);
    let trust = Trust::new(vec![peer_key]);
    let state = |record: Record, signer: [u8; 32]| Statement {
        record,
        signer,
        signature: test_sign(&signer, &Statement::message(&record, &signer)),
    };
    let good = state(record, peer_key);
    let round = Statement::decode(&good.encode()).unwrap();
    assert_eq!(round, good);
    assert_eq!(round.vouched(&record.key, &trust, &TestSignatures), Ok(record));
    assert!(good.vouched(&Id([9; 32]), &trust, &TestSignatures).is_err(), "a statement about another key");
    assert!(state(record, stranger).vouched(&record.key, &trust, &TestSignatures).is_err());
    let mut swapped = good.clone();
    swapped.record.content = Id([5; 32]);
    assert!(swapped.vouched(&record.key, &trust, &TestSignatures).is_err(), "another content under the signature");
    let mut claimed = state(record, stranger);
    claimed.signer = peer_key;
    assert!(claimed.vouched(&record.key, &trust, &TestSignatures).is_err());
}

/// What a [`Verified`] is: only [`Import::read`] makes one, with the manifest's key and kind.
#[test]
fn a_verified_record_carries_the_manifests_key_and_kind() {
    let mut rng = Rng::new(21);
    let records = origin(&mut rng, 1);
    let manifest = Manifest::new("toolchain", "t1", vec![records[0].0]).unwrap();
    let mut import = Import::new(manifest, ADMIT).unwrap();
    assert!(matches!(import.read(&records[0].1, &TestHashes), Err(Refused::Order { .. })));
    import.looked(None).unwrap();
    let verified: Verified<'_> = import.read(&records[0].1, &TestHashes).unwrap();
    assert_eq!((verified.key(), verified.kind(), verified.bytes()), (records[0].0.key, records[0].0.kind, &records[0].1[..]));
    let mut cut = Cut::whole(&records[0].1);
    let mut into = Vec::new();
    cut.read(0, 4, &mut into).unwrap();
    assert_eq!(into, records[0].1[..4]);
}
