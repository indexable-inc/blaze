//! A home's store reads shared directories' writers as layers ([`Config::layers`], `peers`):
//! the machine's std records kept once and read in place by every home.

use super::*;

/// Writer `n` of the shared directory `root`, with tags apart from a home's (`1 << 40`).
fn writer_in(root: &str, n: u64) -> SharedConfig {
    let tags = (1 << 44) + (n << 33);
    SharedConfig { root: name(root), writer: writer(n), tags, load_tags: tags + (1 << 32), loader: Loader::Pumped }
}

/// The directory of writer `n` in the shared directory `root`.
pub(super) fn writer_dir(root: &str, n: u64) -> FileName {
    name(&format!("{root}/w-{:x}", writer(n)))
}

/// Writer `n` of the shared directory `root`, open.
pub(super) fn open_writer(sim: &mut Sim, root: &str, n: u64) -> Store {
    let mut effects = Vec::new();
    let mut store = Store::open_shared(writer_in(root, n), &mut effects);
    drive(sim, &mut store, &mut effects);
    assert!(store.ready(), "{:?}", store.broken());
    store
}

fn home(dir: &str, layers: &[&str]) -> Config {
    Config {
        dir: name(dir),
        layers: layers.iter().map(|it| name(it)).collect(),
        tags: 1 << 40,
        load_tags: (1 << 40) + (1 << 32),
        loader: Loader::Pumped,
    }
}

/// What std's writer keeps: one value of each kind a std unit's record and its outputs are.
fn std_values(seed: u64) -> Vec<(Key, Kind, Vec<u8>)> {
    vec![
        (key(1), Kind::Code, b"std's code".to_vec()),
        (key(2), Kind::Query, text(seed, 9_000)),
        (key(3), Kind::Output, noise(400 << 10, seed)),
        (key(4), Kind::Run, run_output(seed)),
    ]
}

/// The oracle: every value of `values` is got from `home` byte for byte, and a put of it into
/// `home` (what a build that made it again does) keeps nothing; the bytes written under
/// `home`'s own directory and under the layer's for them are 0. `copy`, when given, is a
/// store the hits are copied into as they are got: today's `kept_under`.
fn read_in_place(
    sim: &mut Sim,
    home: &mut Store,
    mut copy: Option<&mut Store>,
    values: &[(Key, Kind, Vec<u8>)],
) -> Result<(), String> {
    let written = sim.bytes_written();
    let recorded = sim.recorded().len();
    for (key, kind, bytes) in values {
        let got = bytes_of(home, key).ok_or_else(|| format!("{key:?} is not found through the layer"))?;
        if got != *bytes {
            return Err(format!("{key:?} reads {} bytes, not the {} its writer put", got.len(), bytes.len()));
        }
        let ticket = home.put(*key, *kind, &got).map_err(|why| format!("{key:?}: {why:?}"))?;
        if !home.durable(ticket) {
            return Err(format!("a put of {key:?}, which a layer holds, keeps a copy"));
        }
        if let Some(copy) = copy.as_deref_mut() {
            copy.put(*key, *kind, &got).map_err(|why| format!("{key:?}: {why:?}"))?;
        }
    }
    flush(sim, home);
    if let Some(copy) = copy {
        flush(sim, copy);
    }
    let wrote = sim.bytes_written() - written;
    let files: Vec<String> = sim.recorded()[recorded..].iter().map(|(_, file, _)| file.to_string()).collect();
    match (wrote, files.is_empty()) {
        (0, true) => Ok(()),
        _ => Err(format!("{wrote} bytes written for values a layer holds, into {files:?}")),
    }
}

/// The invariant: a key put by writer W of directory D is got by a store opened with
/// `layers: [D]`, byte for byte, with 0 bytes written under the store's own directory for it
/// and 0 under D, whatever order the IO answers in (each seed). The control: a home that copies
/// a layer's hit into itself (today's `kept_under`, compile.rs) fails the byte count.
#[test]
fn a_home_store_reads_a_layer_in_place_and_never_writes_it() {
    for seed in 0..12 {
        let mut sim = Sim::new(seed, quiet());
        let values = std_values(seed);
        let mut std = open_writer(&mut sim, "std", 1);
        for (key, kind, bytes) in &values {
            std.put(*key, *kind, bytes).unwrap();
        }
        flush(&mut sim, &mut std);
        drop(std);
        let before = sim.contents(&name(&format!("{}/head", writer_dir("std", 1))));
        let mut mine = open_with(&mut sim, home("home", &["std"]));
        sim.record(vec![name("home"), name("std"), name("copied")]);
        assert_eq!(read_in_place(&mut sim, &mut mine, None, &values), Ok(()), "seed {seed}");
        mine.put(key(9), Kind::Code, b"the home's own").unwrap();
        flush(&mut sim, &mut mine);
        assert_eq!(bytes_of(&mine, &key(9)).unwrap(), b"the home's own");
        assert!(
            sim.recorded().iter().all(|(_, file, _)| !file.is_under(&name("std"))),
            "seed {seed}: the home wrote under its layer: {:?}",
            sim.recorded()
        );
        assert_eq!(sim.contents(&name(&format!("{}/head", writer_dir("std", 1)))), before);
        assert_eq!(mine.verify(None), Vec::new());
        let census = mine.census();
        assert_eq!(census.kinds.iter().map(|it| it.records).sum::<u64>(), 1, "seed {seed}: {census:?}");
        assert_eq!(census.layers.len(), 1);
        assert_eq!(census.layers[0].0, writer_dir("std", 1));
        assert!(census.layers[0].1 >= values.len() as u64, "{census:?}");

        let mut copied = open(&mut sim, "copied");
        let caught = read_in_place(&mut sim, &mut mine, Some(&mut copied), &values);
        assert!(caught.is_err(), "seed {seed}: the oracle missed a home that copies its layer's hits");
    }
}

/// The invariant: a record a writer commits to a layer after a home opened is found by that
/// home once it looks again ([`Store::look_again`], what a miss asks), and not before it does.
/// The control is the miss before the look: without it the look would prove nothing.
#[test]
fn a_miss_looks_again_and_finds_what_the_layer_committed_since() {
    let mut sim = Sim::new(3, quiet());
    let mut std = open_writer(&mut sim, "std", 1);
    flush(&mut sim, &mut std);
    let mut mine = open_with(&mut sim, home("home", &["std"]));
    std.put(key(5), Kind::Code, b"made after the home opened").unwrap();
    flush(&mut sim, &mut std);
    assert_eq!(bytes_of(&mine, &key(5)), None, "found before the home looked again");
    let mut effects = Vec::new();
    let asked = mine.look_again(&mut effects);
    drive(&mut sim, &mut mine, &mut effects);
    assert!(mine.looked() >= asked);
    assert_eq!(bytes_of(&mine, &key(5)).unwrap(), b"made after the home opened");
}

/// The invariant: a layer's bytes that do not verify are refused by name: a record whose bytes
/// rotted (its block checks written again over the rot, so the content hash is what refuses)
/// is an error naming the layer's pack, and a head that does not decode stops the home naming
/// that head; nothing is read another way. The control: the same layer untouched reads whole.
#[test]
fn a_layer_whose_bytes_do_not_verify_is_refused_by_name() {
    let mut sim = Sim::new(4, quiet());
    let value = noise(64 << 10, 4);
    let mut std = open_writer(&mut sim, "std", 1);
    std.put(key(1), Kind::Code, &value).unwrap();
    flush(&mut sim, &mut std);
    drop(std);
    let mine = open_with(&mut sim, home("home", &["std"]));
    assert_eq!(bytes_of(&mine, &key(1)).unwrap(), value, "the control: an untouched layer reads whole");
    drop(mine);

    let pack = name(&format!("{}/pack-000000", writer_dir("std", 1)));
    sim.rot(&pack, 32 << 10, b"rotted");
    recheck(&mut sim, &pack);
    let mine = open_with(&mut sim, home("home", &["std"]));
    let corrupt = mine.get(&key(1)).err().expect("a rotted record of a layer is refused");
    assert_eq!(corrupt.pack, pack, "{corrupt}");

    let head = name(&format!("{}/head", writer_dir("std", 1)));
    sim.change(&head, b"not a head");
    let mut effects = Vec::new();
    let mut mine = Store::open(home("home", &["std"]), &mut effects);
    drive(&mut sim, &mut mine, &mut effects);
    assert_eq!(mine.broken().expect("a layer's head that does not decode stops the home").file, head);
}

/// The invariant: a layer that is not there stops the store, naming the directory; a home is
/// never opened as if its layer held nothing.
#[test]
fn a_missing_layer_stops_the_store() {
    let mut sim = Sim::new(12, quiet());
    let mut effects = Vec::new();
    let mut store = Store::open(home("mine", &["nowhere"]), &mut effects);
    drive(&mut sim, &mut store, &mut effects);
    assert_eq!(store.broken().expect("a missing layer stops it").file, name("nowhere"));
}

/// The invariant: a process has one home handle ([`Handle::install`]; a second is refused,
/// naming the first) and one layer handle installed after it ([`Handle::install_layer`]; one
/// installed before its home, and a second, are refused by name), and the home reads that
/// layer live ([`Store::read_live`]): a record put through the layer's handle is found by the
/// home's lookup (its handle's read, its `has`) at once, before the layer commits, and is not
/// in the home's own store (its tiers find nothing of it, its census holds no record of it).
/// Once the layer commits, a second home that never wrote it finds it there. The control: the
/// same bytes put through the home's own handle land in the home (its census holds that one
/// record) and the second home does not find them. One test, since the handles are the
/// process's.
#[test]
fn a_record_put_through_the_layer_handle_is_found_live_by_the_home_and_kept_once_a_machine() {
    let mut sim = Sim::new(41, quiet());
    let sealed = noise(40 << 10, 41);
    let mut std = open_writer(&mut sim, "std", 1);
    let early = Handle::install_layer(Handle::new(&mut std, "/layer".into())).err().unwrap();
    assert!(early.contains("before the process's home store"), "{early}");
    let mut first = open_with(&mut sim, home("first", &["std"]));
    let installed = Handle::install(Handle::new(&mut first, "/first".into())).unwrap();
    assert!(std::ptr::eq(installed, Handle::installed().unwrap()));
    let refused = Handle::install(Handle::new(&mut first, "/second".into())).err().unwrap();
    assert!(refused.contains("/first"), "{refused}");
    let layer = Handle::install_layer(Handle::new(&mut std, "/layer".into())).unwrap();
    assert!(std::ptr::eq(layer, Handle::layer().unwrap()));
    let again = Handle::install_layer(Handle::new(&mut std, "/again".into())).err().unwrap();
    assert!(again.contains("/layer"), "{again}");

    // A sealed crate's record, built in the first home, goes through the layer's handle alone.
    layer.put(key(7), Kind::Code, &sealed).unwrap();
    assert_eq!(installed.read(&key(7), |it| it.bytes.to_vec()).unwrap(), Some(sealed.clone()), "found live, in the same build");
    assert!(installed.has(&key(7)) && first.has(&key(7)));
    assert!(first.look().find(&key(7)).is_none(), "the home's own tiers hold nothing of it");
    // The control: the same bytes under another key through the home's own handle.
    installed.put(key(8), Kind::Code, &sealed).unwrap();
    flush(&mut sim, &mut std);
    flush(&mut sim, &mut first);
    let census = first.census();
    assert_eq!(census.kinds.iter().map(|it| it.records).sum::<u64>(), 1, "only the control's put is the home's: {census:?}");
    assert_eq!(bytes_of(&first, &key(8)).unwrap(), sealed, "the control's put is in the first home");
    drop(std);
    let second = open_with(&mut sim, home("second", &["std"]));
    assert_eq!(bytes_of(&second, &key(7)).unwrap(), sealed, "the layer's record is every home's");
    assert!(second.look().find(&key(7)).is_some_and(|it| it.is_ok_and(|found| found.tier > 0)), "read from the layer, not copied");
    assert!(bytes_of(&second, &key(8)).is_none(), "the control: a put into a home is that home's alone");
}

/// What a sealed crate's build leaves: a record and its output, as a unit's share is.
/// Keys `first` and `first + 1`.
fn sealed_values(seed: u64, first: u64) -> Vec<(Key, Kind, Vec<u8>)> {
    vec![(key(first), Kind::Query, text(seed, 6_000)), (key(first + 1), Kind::Output, noise(300 << 10, seed))]
}

/// One home's build of a sealed crate on a machine layer it reads live through `layer` (its
/// daemon's writer of the layer): every value put into `layer` (and, for `copy`, into `home`
/// too: the home-and-share put this replaces), then read by the home's reader and its output
/// placed from the home before anything is flushed. The oracle: each value is read whole and
/// placed whole in that build, and after both flush the home's own tier holds no record of
/// any of them.
fn build_sealed(
    sim: &mut Sim,
    home: &mut Store,
    layer: &mut Store,
    values: &[(Key, Kind, Vec<u8>)],
    copy: bool,
) -> Result<(), String> {
    for (key, kind, bytes) in values {
        // The copy is put first, as the home-and-share put wrote it: put after the share, the
        // home finds the value in the layer it reads live and keeps nothing (the second home's
        // invariant below), so the control would plant no copy at all.
        if copy {
            home.put(*key, *kind, bytes).map_err(|why| format!("{key:?}: {why:?}"))?;
        }
        layer.put(*key, *kind, bytes).map_err(|why| format!("{key:?}: {why:?}"))?;
    }
    let reader = home.reader();
    for (key, _, bytes) in values {
        let got = reader.get(&enter(), key).map_err(|it| it.to_string())?.map(|it| it.bytes.to_vec());
        if got.as_ref() != Some(bytes) {
            return Err(format!("{key:?} is not read whole by the home that built it, in its build"));
        }
    }
    let (output, _, bytes) = &values[1];
    let placed = place_key(sim, home, *output, "target/sealed.rlib");
    if placed.result != Ok(()) || sim.contents(&name("target/sealed.rlib")) != *bytes {
        return Err(format!("the output is not placed from the layer in the build that made it: {placed:?}"));
    }
    flush(sim, layer);
    flush(sim, home);
    let census = home.census();
    let records: u64 = census.kinds.iter().map(|it| it.records).sum();
    match records {
        0 => Ok(()),
        _ => Err(format!("the home keeps {records} records of a sealed crate a layer holds: {census:?}")),
    }
}

/// The invariant: on one machine layer, each home with its own writer of it read live (each
/// daemon's `std`), what the first home's build of a sealed crate puts into its writer is read
/// and placed by that home in the same build with nothing of it in the home (`build_sealed`);
/// and the second home, once it looks again, finds every value, and a put of each there (its
/// build making it again) is durable at once and writes nothing: 0 bytes under the second
/// home and its writer. Whatever order the IO answers in (each seed). The control: a first
/// home that keeps a copy too (the home-and-share put) fails the oracle, naming the records.
#[test]
fn a_second_home_finds_what_the_first_built_on_their_layer_and_writes_nothing_of_it() {
    for seed in 0..8 {
        let mut sim = Sim::new(seed, quiet());
        let values = sealed_values(seed, 21);
        let mut a_layer = open_writer(&mut sim, "std", 1);
        let mut b_layer = open_writer(&mut sim, "std", 2);
        flush(&mut sim, &mut a_layer);
        flush(&mut sim, &mut b_layer);
        let mut a = open_with(&mut sim, home("a", &["std"]));
        let mut b = open_with(&mut sim, home("b", &["std"]));
        a.read_live(&a_layer.reader()).unwrap();
        b.read_live(&b_layer.reader()).unwrap();
        assert_eq!(build_sealed(&mut sim, &mut a, &mut a_layer, &values, false), Ok(()), "seed {seed}");

        let mut effects = Vec::new();
        let asked = b.look_again(&mut effects);
        drive(&mut sim, &mut b, &mut effects);
        assert!(b.looked() >= asked);
        sim.record(vec![name("b"), writer_dir("std", 2)]);
        let written = sim.bytes_written();
        for (key, kind, bytes) in &values {
            assert_eq!(bytes_of(&b, key).as_ref(), Some(bytes), "seed {seed}: the second home finds {key:?}");
            let ticket = b.put(*key, *kind, bytes).unwrap();
            assert!(b.durable(ticket), "seed {seed}: a put of {key:?}, which the layer holds, keeps a copy");
        }
        flush(&mut sim, &mut b);
        flush(&mut sim, &mut b_layer);
        assert_eq!(sim.bytes_written() - written, 0, "seed {seed}: {:?}", sim.recorded());
        assert!(sim.recorded().is_empty(), "seed {seed}: the second home wrote {:?}", sim.recorded());

        // The control: a home that copies what it shares keeps it twice a machine.
        let mut c_layer = open_writer(&mut sim, "std", 3);
        let mut c = open_with(&mut sim, home("c", &["std"]));
        c.read_live(&c_layer.reader()).unwrap();
        let copied = sealed_values(seed + 100, 31);
        let caught = build_sealed(&mut sim, &mut c, &mut c_layer, &copied, true);
        // Its records: the query's and the output's, and the output's chunks (`CHUNK_FROM`).
        assert!(
            caught.as_ref().is_err_and(|why| why.contains("records of a sealed crate a layer holds")),
            "seed {seed}: the oracle missed a home that copies: {caught:?}"
        );
    }
}

/// The invariant: a store reads live only a writer of one of its layers, and one: a store of
/// another directory, and a second writer, are refused by name; the same one again is not.
#[test]
fn a_store_reads_live_only_one_writer_of_its_layers() {
    let mut sim = Sim::new(43, quiet());
    let one = open_writer(&mut sim, "std", 1);
    let two = open_writer(&mut sim, "std", 2);
    let other = open_writer(&mut sim, "elsewhere", 1);
    let mut mine = open_with(&mut sim, home("home", &["std"]));
    let refused = mine.read_live(&other.reader()).err().unwrap();
    assert!(refused.contains("elsewhere") && refused.contains("std"), "{refused}");
    mine.read_live(&one.reader()).unwrap();
    mine.read_live(&one.reader()).unwrap();
    let second = mine.read_live(&two.reader()).err().unwrap();
    assert!(second.contains("live already"), "{second}");
    let me = mine.reader();
    assert!(mine.read_live(&me).is_err(), "a store is no live layer of its own");
}

/// One std unit as a home's build leaves it in the machine's layer: its outputs, then its
/// record, which names them (`unit::compile::store` puts the outputs as the unit ends, the
/// record is filed after the build's answer). The same bytes in every home: the keys cover
/// every input, so two homes that build it put equal bytes under one key.
struct StdUnit {
    record: (Key, Vec<u8>),
    outputs: Vec<(Key, Vec<u8>)>,
}

fn std_units(seed: u64, count: u64) -> Vec<StdUnit> {
    (0..count)
        .map(|n| StdUnit {
            record: (key(7_000 + n), text(seed * 64 + n, 2_000 + (n as usize) * 300)),
            outputs: vec![
                (key(8_000 + 2 * n), noise(24 << 10, seed * 64 + n)),
                (key(8_001 + 2 * n), text(seed * 64 + 32 + n, 900)),
            ],
        })
        .collect()
}

/// A bug a control plants in a home's build of std into the layer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum LayerPlant {
    None,
    /// The job ends without the layer's flush and says its records kept as they are put
    /// (what `Jobs`' answer did were the layer's tickets dropped at the job's end).
    DroppedAtJobEnd,
    /// Each record put and committed before its outputs are put.
    RecordFirst,
}

/// What one home's build asks of its writer of the layer, in order.
#[derive(Clone, Copy, Debug)]
enum LayerOp {
    /// Unit `.0`'s output `.1`, or its record when `.1` is none.
    Put(usize, Option<usize>),
    /// The job's end: the layer flushed, its commit a task after the answer.
    Flush,
}

fn build_ops(units: usize, plant: LayerPlant) -> Vec<LayerOp> {
    let mut ops = Vec::new();
    for unit in 0..units {
        if plant == LayerPlant::RecordFirst {
            ops.extend([LayerOp::Put(unit, None), LayerOp::Flush, LayerOp::Put(unit, Some(0)), LayerOp::Put(unit, Some(1))]);
            continue;
        }
        ops.extend([LayerOp::Put(unit, Some(0)), LayerOp::Put(unit, Some(1)), LayerOp::Put(unit, None)]);
    }
    if plant == LayerPlant::None {
        ops.push(LayerOp::Flush);
    }
    ops
}

/// Delivers what `stores` asked, `polls` times (none: until nothing is in flight), each
/// completion to the store that owns it. Bounded on the simulation's clock: a poll that
/// completes nothing by its deadline is the error.
fn pump_all(sim: &mut Sim, stores: &mut [&mut Store], effects: &mut Vec<Effect>, polls: Option<u64>) -> Result<(), String> {
    let mut left = polls;
    let mut done = Vec::new();
    while left != Some(0) {
        sim.submit(effects);
        if sim.in_flight() == 0 {
            break;
        }
        let end = blaze_core::wait::Deadline::after(sim.now(), Duration::from_secs(600));
        sim.poll(&mut done, end);
        if done.is_empty() {
            return Err(format!("{} effects in flight and none completed by {end:?}", sim.in_flight()));
        }
        for completion in done.drain(..) {
            if let Some(store) = stores.iter_mut().find(|it| it.owns(completion.tag)) {
                store.complete(completion, effects);
            }
        }
        left = left.map(|it| it - 1);
    }
    sim.submit(effects);
    Ok(())
}

/// The oracle, as a second home that opens after the crash (or the builds' end) sees the
/// layer: every record a home reported kept (its layer ticket durable, what the job's settling
/// waits for before the certificate says the build is kept) is a hit with equal bytes; a
/// record that was not yet durable may be a miss, which that home builds itself, never a
/// record torn or refused; and a record it finds has every output it names, whole.
fn second_home_sees(sim: &mut Sim, units: &[StdUnit], kept: &[(u64, usize)]) -> Result<(), String> {
    let second = open_with(sim, home("second", &["std"]));
    let got = |key: &Key| -> Result<Option<Vec<u8>>, String> {
        second.get(key).map(|it| it.map(|it| it.bytes.to_vec())).map_err(|why| format!("{key:?} is refused: {why}"))
    };
    for (writer, unit) in kept {
        let (key, bytes) = &units[*unit].record;
        match got(key)? {
            Some(found) if found == *bytes => {}
            Some(found) => {
                return Err(format!("home {writer} reported {key:?} kept; the second home reads {} other bytes", found.len()));
            }
            None => {
                return Err(format!("home {writer} reported the record {key:?} of unit {unit} kept, and the second home misses it"));
            }
        }
    }
    for (n, unit) in units.iter().enumerate() {
        let (key, bytes) = &unit.record;
        let Some(found) = got(key)? else { continue };
        if found != *bytes {
            return Err(format!("the record of unit {n} reads {} bytes, not the {} put", found.len(), bytes.len()));
        }
        for (output, want) in &unit.outputs {
            match got(output)? {
                Some(it) if it == *want => {}
                Some(it) => return Err(format!("unit {n}'s output {output:?} reads {} other bytes", it.len())),
                None => return Err(format!("the record of unit {n} is visible to the second home before its output {output:?}")),
            }
        }
    }
    Ok(())
}

/// Two homes on one machine build std at once, each through its own writer of the machine's
/// layer (each daemon's `std`, `serve::daemon`), their puts, job-end flushes and IO
/// completions interleaved by `seed`, and the machine may lose power at any step. Then the
/// oracle ([`second_home_sees`]). Home 1 builds one unit home 2 does not: once both are
/// idle (no crash came), home 2, having looked again, is refused other bytes under that
/// unit's record key: the typed refusal of two values under one key ([`Mismatch`]).
fn std_race(seed: u64, plant: LayerPlant) -> Result<(), String> {
    const UNITS: u64 = 5;
    let mut sim = Sim::new(seed, Faults { max_latency: Duration::from_micros(400), ..Faults::default() });
    let units = std_units(seed, UNITS);
    let mut writers = [open_writer(&mut sim, "std", 1), open_writer(&mut sim, "std", 2)];
    let ops = [build_ops(UNITS as usize, plant), build_ops(UNITS as usize - 1, plant)];
    let mut next = [0usize; 2];
    // Each home's record tickets not yet durable, and what it reported kept.
    let mut waiting: [Vec<(usize, Ticket)>; 2] = [Vec::new(), Vec::new()];
    let mut kept: Vec<(u64, usize)> = Vec::new();
    let mut effects = Vec::new();
    let mut rng = Rng::new(seed ^ 0x5eed);
    let total = (ops[0].len() + ops[1].len()) as u64;
    // A crash at a step of the builds, for the race; the controls' at their end.
    let crash_at = (plant == LayerPlant::None && rng.below(4) != 0).then(|| rng.below(total + 2));
    let mut crashed = false;
    for step in 0.. {
        if crash_at == Some(step) {
            sim.crash();
            crashed = true;
            break;
        }
        let open: Vec<usize> = (0..2).filter(|it| next[*it] < ops[*it].len()).collect();
        if open.is_empty() {
            break;
        }
        let home = open[rng.below(open.len() as u64) as usize];
        let op = ops[home][next[home]];
        next[home] += 1;
        let writer = &mut writers[home];
        match op {
            LayerOp::Put(unit, which) => {
                let (key, kind, bytes) = match which {
                    Some(at) => (units[unit].outputs[at].0, Kind::Output, &units[unit].outputs[at].1),
                    None => (units[unit].record.0, Kind::Query, &units[unit].record.1),
                };
                let ticket = writer
                    .put(key, kind, bytes)
                    .map_err(|why| format!("home {}'s put of equal bytes under {key:?} was refused: {why:?}", home + 1))?;
                if which.is_none() {
                    match plant {
                        LayerPlant::DroppedAtJobEnd => kept.push((home as u64 + 1, unit)),
                        _ => waiting[home].push((unit, ticket)),
                    }
                }
            }
            LayerOp::Flush => writer.flush(&mut effects),
        }
        let [one, two] = &mut writers;
        pump_all(&mut sim, &mut [one, two], &mut effects, Some(rng.below(3)))?;
        for (home, writer) in writers.iter().enumerate() {
            waiting[home].retain(|(unit, ticket)| {
                let durable = writer.durable(*ticket);
                if durable {
                    kept.push((home as u64 + 1, *unit));
                }
                !durable
            });
        }
    }
    if !crashed {
        let [one, two] = &mut writers;
        pump_all(&mut sim, &mut [one, two], &mut effects, None)?;
        for (home, writer) in writers.iter().enumerate() {
            kept.extend(waiting[home].iter().filter(|(_, ticket)| writer.durable(*ticket)).map(|(unit, _)| (home as u64 + 1, *unit)));
        }
        if plant == LayerPlant::None {
            // Both idle: home 2 looks again and finds home 1's commit; other bytes under a
            // key home 1 alone built are refused, naming the key.
            let mut asked = Vec::new();
            let look = writers[1].look_again(&mut asked);
            let [one, two] = &mut writers;
            pump_all(&mut sim, &mut [one, two], &mut asked, None)?;
            if writers[1].looked() < look {
                return Err(format!("home 2's look of the layer did not come in: {:?}", writers[1].broken()));
            }
            let (alone, _) = &units[UNITS as usize - 1].record;
            match writers[1].put(*alone, Kind::Query, b"other bytes under one key") {
                Err(mismatch) if mismatch.key == *alone => {}
                other => return Err(format!("other bytes under {alone:?}, which home 1 committed, came to {other:?}")),
            }
        }
        // The machine goes down once the builds are answered: what is durable stays.
        sim.crash();
    }
    drop(writers);
    second_home_sees(&mut sim, &units, &kept)?;
    // The writers' directories open again after the crash, each at its last durable state.
    let mut again = open_writer(&mut sim, "std", 1);
    flush(&mut sim, &mut again);
    Ok(())
}

/// The invariant: two homes on one machine building std at once into the machine's layer,
/// each through its own writer, under every interleaving of their puts and commits and a power
/// loss at any step (each seed): a second home's lookup of a record either home reported kept
/// is a hit with equal bytes (the first was durable before it said kept); one not yet durable
/// is a miss the second home builds itself, never a torn record; a record it finds has every
/// output it names; both homes' puts of one key are equal bytes and keep one value, and other
/// bytes under a key the layer holds are the typed refusal ([`Mismatch`]). The controls plant
/// the two ways this breaks: [`the_oracle_catches_layer_puts_dropped_at_job_end`],
/// [`the_oracle_catches_a_record_visible_before_its_outputs`].
#[test]
fn two_homes_building_std_at_once_leave_a_second_home_every_record_they_reported_kept() {
    for seed in 0..48 {
        assert_eq!(std_race(seed, LayerPlant::None), Ok(()), "seed {seed}");
    }
}

/// Control: a job that ends without the layer's flush and says its records kept as they are
/// put (the layer's tickets dropped at the job's end) leaves records a second home misses
/// after the machine goes down: the oracle names one.
#[test]
fn the_oracle_catches_layer_puts_dropped_at_job_end() {
    for seed in 0..6 {
        let caught = std_race(seed, LayerPlant::DroppedAtJobEnd);
        assert!(caught.as_ref().is_err_and(|why| why.contains("misses it")), "seed {seed}: {caught:?}");
    }
}

/// Control: a record committed before its outputs are put is visible to a second home with an
/// output it names missing once the machine goes down before the outputs' commit: the oracle
/// names the output.
#[test]
fn the_oracle_catches_a_record_visible_before_its_outputs() {
    for seed in 0..6 {
        let caught = std_race(seed, LayerPlant::RecordFirst);
        assert!(caught.as_ref().is_err_and(|why| why.contains("before its output")), "seed {seed}: {caught:?}");
    }
}

/// The invariant: a writer directory of the machine's layer in another layout (its head's
/// magic naming another format, as an older blaze's store of the same makers would leave) is
/// refused by name: the home that reads the layer stops, naming that head and the format,
/// and reads nothing of it another way. The control: the same directory as written opens.
#[test]
fn a_layer_writer_of_another_layout_is_refused_by_name() {
    let mut sim = Sim::new(52, quiet());
    let mut std = open_writer(&mut sim, "std", 1);
    std.put(key(1), Kind::Query, b"a std unit's record").unwrap();
    flush(&mut sim, &mut std);
    drop(std);
    let mine = open_with(&mut sim, home("home", &["std"]));
    assert_eq!(bytes_of(&mine, &key(1)).unwrap(), b"a std unit's record", "the control: the layer as written opens");
    drop(mine);

    let head = name(&format!("{}/head", writer_dir("std", 1)));
    let mut older = sim.contents(&head);
    older[7] = older[7].wrapping_sub(1);
    sim.change(&head, &older);
    let mut effects = Vec::new();
    let mut mine = Store::open(home("home", &["std"]), &mut effects);
    drive(&mut sim, &mut mine, &mut effects);
    let broken = mine.broken().expect("a layer's writer of another layout stops the home");
    assert_eq!(broken.file, head, "{broken}");
    assert!(broken.what.contains("not the head of a store of this format"), "{broken}");
}
