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
        layer.put(*key, *kind, bytes).map_err(|why| format!("{key:?}: {why:?}"))?;
        if copy {
            home.put(*key, *kind, bytes).map_err(|why| format!("{key:?}: {why:?}"))?;
        }
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
        assert!(caught.is_err_and(|why| why.contains("keeps 2 records")), "seed {seed}: the oracle missed a home that copies");
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
