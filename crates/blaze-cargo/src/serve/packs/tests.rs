//! The drivers against real stores: an export imported into an empty store on disk, the
//! same with a byte flipped and with a signed head, and on the simulation an import followed
//! by a collection of everything unrooted. The cores' own invariants (crashes, truncation,
//! lying sources) are simulated in `blaze_pack`'s tests; a build that makes 0 units after an
//! import is `blaze-engine`'s revision case `pack-import`.

use std::{fs, path::Path, time::Duration};

use blaze_cas::{Driven, Key, Kind, Store, collect::Pace};
use blaze_ids::ContentId;
use blaze_pack::{Manifest, Record, Trust, head::Head, layout};

use super::{
    B3, Base, Config, id_of,
    stored::{self, Driving},
};

/// A store this test drives, as packs read and write it.
fn on<D: Driving>(driving: &mut D) -> stored::Driven<'_, D> {
    stored::Driven::new(driving)
}

fn key(n: u64) -> Key {
    Key(ContentId::of("blaze packs test key", &n.to_le_bytes()))
}

fn value(n: u64) -> Vec<u8> {
    (0..200 + (n * 977) % 3000).map(|at| (at as u8).wrapping_mul(n as u8 | 1).wrapping_add(n as u8)).collect()
}

fn kind(n: u64) -> Kind {
    [Kind::Code, Kind::Query, Kind::Debug][n as usize % 3]
}

/// Windows a test's store reserves in its pool: its few packs, where a store reserves 4 TiB of
/// address space, and a property test opens stores case after case.
const WINDOWS: u32 = 16;

/// A store on disk under `root` holding `count` records, pinned as `toolchain`.
fn origin(root: &Path, count: u64) -> Driven {
    fs::create_dir_all(root).unwrap();
    let mut driven = Driven::open_reserving(root, blaze_cas::Loader::Pumped, WINDOWS).unwrap();
    for n in 0..count {
        driven.store().put(key(n), kind(n), &value(n)).unwrap();
    }
    let keys: Vec<Key> = (0..count).map(key).collect();
    driven.store().pin("toolchain", "the test's records", 1, &keys).unwrap();
    driven.flush().unwrap();
    driven
}

fn empty(root: &Path) -> Driven {
    fs::create_dir_all(root).unwrap();
    Driven::open_reserving(root, blaze_cas::Loader::Pumped, WINDOWS).unwrap()
}

/// The invariant: a pin exported and imported into an empty store leaves, under every key,
/// the origin's kind and bytes, pinned under the same name and durable; exporting twice
/// gives identical files; importing twice puts nothing the second time. An unsigned head is
/// taken only on the user's word.
#[test]
fn an_export_imported_into_an_empty_store_holds_the_origins_records_pinned() {
    let dir = tempfile::tempdir().unwrap();
    let mut first = origin(&dir.path().join("a"), 60);
    let (out, again_out) = (dir.path().join("out"), dir.path().join("again"));
    let done = stored::export(&mut on(&mut first), "toolchain", "t1", &out, None, false, false, &|_| false).unwrap();
    assert_eq!((done.records, done.signed, done.absent), (60, false, 0));
    // A pin that also names keys the store has no record of (slots never filled): they are
    // counted, not in the pack, and no reason to refuse it.
    let mut wider: Vec<Key> = (0..62).map(key).collect();
    wider.sort_unstable();
    first.store().pin("wider", "the test's records and two keys of nothing", 2, &wider).unwrap();
    first.flush().unwrap();
    let got = stored::export(&mut on(&mut first), "wider", "t1", &dir.path().join("wider"), None, false, false, &|_| false).unwrap();
    assert_eq!((got.records, got.absent), (60, 2));
    // No record that holds a directory of the exporting machine travels: of everything it
    // is left out and counted; a pin that keeps one is refused naming the key. The control
    // is every export above, whose guard finds none.
    let holds = |bytes: &[u8]| bytes == &value(7)[..];
    let all = stored::export(&mut on(&mut first), "build", "t1", &dir.path().join("held"), None, true, false, &holds).unwrap();
    assert_eq!((all.records, all.held), (59, 1));
    let refused = stored::export(&mut on(&mut first), "toolchain", "t1", &dir.path().join("held-pin"), None, false, false, &holds).unwrap_err();
    assert!(refused.contains(&id_of(&key(7)).to_string()) && refused.contains("a directory of this machine"), "{refused}");
    let again = stored::export(&mut on(&mut first), "toolchain", "t1", &again_out, None, false, false, &|_| false).unwrap();
    assert_eq!(done, again);
    let files = [
        layout::manifest_path(&done.pack),
        layout::frames_path(&done.pack),
        layout::spans_path(&done.pack),
        layout::head_path("t1", "toolchain").unwrap(),
    ];
    for path in &files {
        assert_eq!(fs::read(out.join(path)).unwrap(), fs::read(again_out.join(path)).unwrap(), "{path}");
    }
    assert!(stored::export(&mut on(&mut first), "nothing", "t1", &out, None, false, false, &|_| false).unwrap_err().contains("no pin `nothing`"));
    // Everything that may travel: the 60 records, and not the pin's own record.
    let all = stored::export(&mut on(&mut first), "build", "t1", &dir.path().join("all"), None, true, false, &|_| false).unwrap();
    assert_eq!(all.records, 60);

    let base = Base::Dir(out);
    let trust = Trust::new(Vec::new());
    let mut second = empty(&dir.path().join("b"));
    let refused = stored::import(&mut on(&mut second), &base, "t1", "toolchain", &trust, false).unwrap_err();
    assert!(refused.contains("unsigned"), "{refused}");
    assert!(second.store().get(&key(0)).unwrap().is_none(), "a refused pack left a record");
    let got = stored::import(&mut on(&mut second), &base, "t1", "toolchain", &trust, true).unwrap().unwrap();
    assert_eq!((got.summary.put, got.summary.present, got.pack), (60, 0, done.pack));
    let held = |store: &Store| {
        for n in 0..60 {
            let entry = store.get(&key(n)).unwrap().unwrap_or_else(|| panic!("record {n} is not there"));
            assert_eq!((entry.kind, entry.bytes), (kind(n), &value(n)[..]), "record {n}");
        }
    };
    held(second.store());
    assert!(
        second.store().pins().any(|pin| pin.name == "toolchain" && (0..60).all(|n| pin.keys.contains(&key(n)))),
        "the import did not pin what it imported"
    );
    let got = stored::import(&mut on(&mut second), &base, "t1", "toolchain", &trust, true).unwrap().unwrap();
    assert_eq!((got.summary.put, got.summary.present), (0, 60));
    assert!(stored::import(&mut on(&mut second), &base, "t1", "bstd", &trust, true).unwrap().is_none());
    assert!(stored::import(&mut on(&mut second), &base, "t2", "toolchain", &trust, true).unwrap().is_none());
    drop(second);
    held(empty(&dir.path().join("b")).store());
}

/// The invariant: a byte flipped anywhere in a pack's body refuses the import naming the
/// record it is in (a raw body), or naming a record and what could not be read of the body (a
/// compressed one, where the span does not decompress or gives other bytes). The control is
/// the same pack unflipped, which imports.
#[test]
fn a_flipped_byte_in_a_packs_body_is_refused_naming_the_record() {
    let dir = tempfile::tempdir().unwrap();
    let trust = Trust::new(Vec::new());
    // A raw body, as another machine's `export --raw` wrote it.
    let raw = dir.path().join("raw");
    let base = pack_in(&raw, 30);
    let head = base.head("t1", "toolchain").unwrap().unwrap();
    let manifest = base.manifest(&head).unwrap();
    let mut control = empty(&dir.path().join("control"));
    stored::import(&mut on(&mut control), &base, "t1", "toolchain", &trust, true).unwrap().unwrap();
    let body_at = raw.join(layout::body_path(&head.pack));
    let whole = fs::read(&body_at).unwrap();
    for (case, at) in [0, whole.len() / 2, whole.len() - 1].into_iter().enumerate() {
        let mut body = whole.clone();
        body[at] ^= 0x10;
        fs::write(&body_at, &body).unwrap();
        let hit = (0..manifest.records().len() as u32)
            .find(|index| (at as u64) < manifest.at(*index) + manifest.records()[*index as usize].len)
            .unwrap();
        let mut store = empty(&dir.path().join(format!("flipped-{case}")));
        let refused = stored::import(&mut on(&mut store), &base, "t1", "toolchain", &trust, true).unwrap_err();
        let named = manifest.records()[hit as usize].key.to_string();
        assert!(refused.contains(&named) && refused.contains("is refused"), "byte {at}: {refused}");
    }
    // The manifest flipped: it is not the pack its head names.
    fs::write(&body_at, &whole).unwrap();
    let manifest_at = raw.join(layout::manifest_path(&head.pack));
    let mut bytes = fs::read(&manifest_at).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 1;
    fs::write(&manifest_at, &bytes).unwrap();
    let mut store = empty(&dir.path().join("manifest"));
    let refused = stored::import(&mut on(&mut store), &base, "t1", "toolchain", &trust, true).unwrap_err();
    assert!(refused.contains("the manifest is refused"), "{refused}");

    // A compressed body, as `export` writes it: it imports (the control), and with a byte
    // of its frames flipped it is refused naming a record of the pack.
    let mut first = origin(&dir.path().join("a"), 30);
    let out = dir.path().join("out");
    let done = stored::export(&mut on(&mut first), "toolchain", "t1", &out, None, false, false, &|_| false).unwrap();
    assert!(!out.join(layout::body_path(&done.pack)).exists(), "a compressed pack kept its raw body");
    let base = Base::Dir(out.clone());
    let mut control = empty(&dir.path().join("control-z"));
    stored::import(&mut on(&mut control), &base, "t1", "toolchain", &trust, true).unwrap().unwrap();
    let frames_at = out.join(layout::frames_path(&done.pack));
    let mut frames = fs::read(&frames_at).unwrap();
    let middle = frames.len() / 2;
    frames[middle] ^= 0x10;
    fs::write(&frames_at, &frames).unwrap();
    let mut store = empty(&dir.path().join("flipped-z"));
    let refused = stored::import(&mut on(&mut store), &base, "t1", "toolchain", &trust, true).unwrap_err();
    assert!((0..30).any(|n| refused.contains(&id_of(&key(n)).to_string())), "{refused}");
    assert!(store.store().get(&key(0)).unwrap().is_none() || refused.contains("is refused"), "{refused}");
}

/// The invariant: a signed head is taken under a trusted key with no word from the user,
/// and under no other key; a head changed after it was signed is refused.
#[cfg(feature = "native")]
#[test]
fn a_signed_pack_is_taken_under_its_trusted_key_only() {
    let dir = tempfile::tempdir().unwrap();
    let mut first = origin(&dir.path().join("a"), 10);
    let out = dir.path().join("out");
    let seed = [7u8; 32];
    let (public, _) = super::sign(&seed, b"").unwrap();
    let done = stored::export(&mut on(&mut first), "toolchain", "t1", &out, Some(&seed), false, false, &|_| false).unwrap();
    assert!(done.signed);
    let base = Base::Dir(out.clone());
    let mut store = empty(&dir.path().join("b"));
    let stranger = Trust::new(vec![[9; 32]]);
    let refused = stored::import(&mut on(&mut store), &base, "t1", "toolchain", &stranger, false).unwrap_err();
    assert!(refused.contains("not among the keys you trust"), "{refused}");
    // Not on the user's word either: a signature that is not trusted is not an unsigned head.
    assert!(stored::import(&mut on(&mut store), &base, "t1", "toolchain", &stranger, true).is_err());
    let trust = Trust::new(vec![public]);
    let got = stored::import(&mut on(&mut store), &base, "t1", "toolchain", &trust, false).unwrap().unwrap();
    assert_eq!(got.vouched, blaze_pack::head::Vouched::Signer(public));
    // The same head saying another body length, under the old signature.
    let head_at = out.join(layout::head_path("t1", "toolchain").unwrap());
    let mut head = Head::decode(&fs::read(&head_at).unwrap()).unwrap();
    head.body_len += 1;
    fs::write(&head_at, head.encode()).unwrap();
    let mut other = empty(&dir.path().join("c"));
    let refused = stored::import(&mut on(&mut other), &base, "t1", "toolchain", &trust, false).unwrap_err();
    assert!(refused.contains("signature"), "{refused}");
}

#[test]
fn the_settings_refuse_what_they_do_not_know() {
    let config = Config::parse(r#"{ "sources": ["/mnt/packs", "https://packs.example.invalid/x/"], "trusted": [] }"#).unwrap();
    assert_eq!(config.sources, [Base::Dir("/mnt/packs".into()), Base::Url("https://packs.example.invalid/x".to_owned())]);
    assert_eq!(config.pins, ["toolchain", "bstd"]);
    assert!(config.serve.is_none() && config.peers.is_empty() && config.install.is_none());
    assert!(config.prebuilt, "records built elsewhere are taken unless the settings say not");
    assert!(!Config::parse(r#"{ "prebuilt": false }"#).unwrap().prebuilt);
    let key = "ab".repeat(32);
    let serving = Config::parse(&format!(
        r#"{{ "key_file": "/k", "serve": {{ "bind": "10.0.0.4:7420", "readers": ["{key}"] }}, "peers": [{{ "url": "http://10.0.0.5:7420/", "key": "{key}" }}] }}"#
    ))
    .unwrap();
    assert_eq!(serving.serve.unwrap().readers, [[0xab; 32]]);
    assert_eq!((serving.peers[0].url.as_str(), serving.peers[0].key), ("http://10.0.0.5:7420", [0xab; 32]));
    for (bad, names) in [
        (r#"{ "source": [] }"#, "`source` is not a setting"),
        (r#"{ "sources": ["relative/dir"] }"#, "neither an absolute directory"),
        (r#"{ "trusted": ["abc"] }"#, "is not a key"),
        (r#"{ "pins": ["../x"] }"#, "the pin"),
        (r#"{ "key_file": "/k", "peers": [{ "url": "/dir", "key": "00" }] }"#, "not an http(s) address"),
        (r#"{ "key_file": "/k", "serve": { "bind": "10.0.0.4:7420", "readers": [], "allow": ["10.0.0.0/8"] } }"#, "`serve.allow` is not a setting"),
        (r#"{ "key_file": "/k", "serve": { "bind": "10.0.0.4:7420" } }"#, "`serve` has no `readers`"),
        // A machine that asks or answers peers signs with its own key.
        (r#"{ "serve": { "bind": "10.0.0.4:7420", "readers": [] } }"#, "need `key_file`"),
        (r#"{ "prebuilt": "no" }"#, "`prebuilt` is not true or false"),
    ] {
        let refused = Config::parse(bad).unwrap_err();
        assert!(refused.contains(names), "{bad}: {refused}");
    }
    // An address is shown without what may hold a secret.
    assert_eq!(super::shown("https://user:token@host.invalid/a/b?sig=1"), "https://host.invalid/a/b");
}

// ---- on the simulation: import, then a collection of everything unrooted

struct Simmed {
    sim: blaze_core::sim::Sim,
    store: Store,
}

fn driven(sim: &mut blaze_core::sim::Sim, store: &mut Store, effects: &mut Vec<blaze_core::Effect>) {
    blaze_core::run_until_idle(sim, effects, |completion, effects| {
        if store.owns(completion.tag) {
            store.complete(completion, effects);
        }
    })
    .expect("the effects end");
}

impl Driving for Simmed {
    fn store(&mut self) -> &mut Store {
        &mut self.store
    }

    fn flush(&mut self) -> Result<(), String> {
        let mut effects = Vec::new();
        self.store.flush(&mut effects);
        driven(&mut self.sim, &mut self.store, &mut effects);
        Ok(())
    }
}

/// Periods ended until `periods` have, each a scan run here.
fn periods(simmed: &mut Simmed, periods: u32) {
    let mut ended = 0;
    for _ in 0..200 {
        if ended == periods {
            return;
        }
        simmed.flush().unwrap();
        ended += u32::from(simmed.store.end_period());
    }
    panic!("the store ended {ended} of {periods} periods in 200 asks");
}

/// A pack of `count` records in a directory, its head unsigned: written here from the
/// format's own parts, as another machine's export would have.
fn pack_in(dir: &Path, count: u64) -> Base {
    let records: Vec<Record> = (0..count)
        .map(|n| Record {
            key: id_of(&key(n)),
            kind: Kind::Code.code(),
            len: value(n).len() as u64,
            content: blaze_pack::Hashes::content(&B3, &value(n)),
        })
        .collect();
    let manifest = Manifest::new("toolchain", "t1", records).unwrap();
    let bytes = manifest.encode();
    let pack = blaze_pack::Hashes::manifest(&B3, &bytes);
    let mut body = Vec::new();
    for record in manifest.records() {
        let n = (0..count).find(|n| id_of(&key(*n)) == record.key).unwrap();
        body.extend_from_slice(&value(n));
    }
    let write = |path: String, bytes: &[u8]| {
        let at = dir.join(path);
        fs::create_dir_all(at.parent().unwrap()).unwrap();
        fs::write(at, bytes).unwrap();
    };
    write(layout::manifest_path(&pack), &bytes);
    write(layout::body_path(&pack), &body);
    write(layout::head_path("t1", "toolchain").unwrap(), &Head::of(&manifest, pack, &bytes).encode());
    Base::Dir(dir.to_owned())
}

/// An import into an empty store, then a store's worth of other records, then every
/// unrooted record collected: whether each imported record is still there, and how many
/// records were forgotten. `pin`: the import pins what it imports.
fn imported_then_collected(pin: bool) -> (Vec<bool>, u64) {
    use blaze_core::Driver as _;
    let dir = tempfile::tempdir().unwrap();
    let base = pack_in(dir.path(), 40);
    let name = |it: &str| blaze_core::FileName::new(it).unwrap();
    let config = blaze_cas::Config {
        dir: name("home/v2/store"),
        layers: Vec::new(),
        tags: 1 << 40,
        load_tags: (1 << 40) + (1 << 32),
        loader: blaze_cas::Loader::Pumped,
    };
    let mut sim = blaze_core::sim::Sim::new(31, blaze_core::sim::Faults::default());
    let mut effects = Vec::new();
    let mut store = Store::open(config, &mut effects);
    driven(&mut sim, &mut store, &mut effects);
    let mut simmed = Simmed { sim, store };

    let mut records = on(&mut simmed);
    records.plant_no_pin = !pin;
    let got = stored::import(&mut records, &base, "t1", "toolchain", &Trust::new(Vec::new()), true)
        .unwrap()
        .unwrap();
    assert_eq!(got.summary.put, 40);
    // Past the 65,536 records after which the store writes its index table: only a record
    // the index holds can be forgotten.
    for n in 0..70_000u64 {
        simmed.store.put(key(1_000_000 + n), Kind::Code, &n.to_le_bytes().repeat(8)).unwrap();
    }
    simmed.flush().unwrap();
    // The records go unused for periods, and everything unrooted is asked to go.
    periods(&mut simmed, 4);
    let gone = std::env::temp_dir().join(format!("blaze-packs-none-{}", std::process::id()));
    let pace = Pace { grace: Duration::ZERO, ..Pace::default() };
    let mut collects =
        crate::serve::collect::Collects::new(&gone, crate::serve::settle::Compress::Inline, pace);
    collects.ask(simmed.sim.now(), false, true);
    for _ in 0..20_000 {
        let mut effects = Vec::new();
        collects.turn(&mut simmed.store, None, true, simmed.sim.now(), &mut effects);
        driven(&mut simmed.sim, &mut simmed.store, &mut effects);
        if !collects.asked_waits(simmed.sim.now())
            && simmed.store.usage().counts.packs_retired > 0
            && !simmed.store.committing()
        {
            break;
        }
        simmed.sim.advance(Duration::from_millis(60));
    }
    let guard = blaze_cas::enter();
    let reader = simmed.store.reader();
    let there = (0..40).map(|n| reader.get(&guard, &key(n)).unwrap().is_some()).collect();
    (there, simmed.store.usage().counts.records_forgotten)
}

/// The invariant: what an import makes visible is pinned before it is put, so a collection
/// of everything unrooted between the import and the first build takes none of it: every
/// record a build of the same sources would ask for is there, and it makes 0 units. The
/// control is the import without its pin: the same collection forgets them.
#[test]
fn an_import_pins_what_it_imports_and_a_collection_before_the_first_build_takes_none_of_it() {
    let (there, forgotten) = imported_then_collected(true);
    assert!(forgotten > 1000, "the collection forgot {forgotten} records: it did not run");
    assert!(there.iter().all(|it| *it), "an imported, pinned record was forgotten: {there:?}");
    let (there, forgotten) = imported_then_collected(false);
    assert!(forgotten > 1000, "the control's collection forgot {forgotten} records: it did not run");
    assert!(there.iter().any(|it| !*it), "the control kept every record with no pin: it tests nothing");
}

/// The invariant: a sealed layer (`serve::sealed`) is installed only from records that hash to
/// their signed manifest's entries. On the layer's own import path (`cli::store`'s
/// `imported_into_layer`, what `blaze store install` and a home's first use run), a pack with
/// one record's bytes changed after it was signed is a hard error naming the source and the
/// record's key, never a layer that holds it. The control is the check removed
/// (`stored::UNCHECKED`): the same tampered pack installs, so the refusal is the check's.
#[cfg(feature = "native")]
#[test]
fn a_tampered_record_on_the_layer_import_is_refused_naming_the_source_and_the_key() {
    use crate::cli::store::imported_into_layer;
    use crate::serve::sealed::Cover;
    let dir = tempfile::tempdir().unwrap();
    let seed = [7u8; 32];
    let (public, _) = super::sign(&seed, b"").unwrap();
    let trust = Trust::new(vec![public]);
    let mut first = origin(&dir.path().join("a"), 30);
    let out = dir.path().join("out");
    // A raw body, signed: what a release's `seal` publishes, uncompressed so one byte of one
    // record can be changed.
    let done = stored::export(&mut on(&mut first), "toolchain", "t1", &out, Some(&seed), false, true, &|_| false).unwrap();
    assert!(done.signed);
    let base = Base::Dir(out.clone());
    let cover = Cover(ContentId::of("blaze packs test cover", b"one"));
    let shared = |name: &str| {
        let at = dir.path().join(name);
        fs::create_dir_all(at.join("store")).unwrap();
        at
    };
    // The path itself works: the pack as it was signed installs whole.
    let got = imported_into_layer(&shared("good"), cover, &base, "t1", "toolchain", &trust).unwrap().unwrap();
    assert_eq!(got.summary.put, 30, "the untampered pack did not install whole: {got:?}");

    let head = base.head("t1", "toolchain").unwrap().unwrap();
    let manifest = base.manifest(&head).unwrap();
    let body_at = out.join(layout::body_path(&head.pack));
    let mut body = fs::read(&body_at).unwrap();
    let at = body.len() / 2;
    body[at] ^= 0x10;
    fs::write(&body_at, &body).unwrap();
    let hit = (0..manifest.records().len() as u32)
        .find(|index| (at as u64) < manifest.at(*index) + manifest.records()[*index as usize].len)
        .unwrap();
    let named = manifest.records()[hit as usize].key.to_string();
    let refused = imported_into_layer(&shared("tampered"), cover, &base, "t1", "toolchain", &trust).unwrap_err();
    assert!(refused.contains(&named), "the refusal does not name the record {named}: {refused}");
    assert!(refused.contains(&base.named()), "the refusal does not name the source: {refused}");

    // The control: with the check removed the same tampered pack installs into a layer.
    stored::UNCHECKED.set(true);
    let planted = imported_into_layer(&shared("planted"), cover, &base, "t1", "toolchain", &trust);
    stored::UNCHECKED.set(false);
    let planted = planted.unwrap().unwrap();
    assert_eq!(planted.summary.put, 30, "the planted bug's import did not take the pack: it tests nothing");
}

/// The invariant: a job's packs are its request's (`JobInput::packs`, made by
/// `packs::given`): for a request that said `--no-prebuilt`, a source that has the key is
/// never asked (its packs are not even loaded) and the key stays a miss, built here; for one
/// that did not, the record is taken from the source into the job's store, its pack whole, so
/// the unit is replayed, not built. The control is the switch ignored (the request that said
/// `--no-prebuilt` given the packs anyway): the same lookup asks the source and finds the key,
/// so the first half's miss is the switch's doing, not a source that could not answer.
#[cfg(feature = "native")]
#[test]
fn with_no_prebuilt_a_source_that_has_the_key_is_never_asked() {
    use std::sync::Arc;

    use super::{Packs, SourceState, given};
    let dir = tempfile::tempdir().unwrap();
    let seed = [7u8; 32];
    let (public, _) = super::sign(&seed, b"").unwrap();
    let mut first = origin(&dir.path().join("a"), 12);
    let out = dir.path().join("out");
    let done = stored::export(&mut on(&mut first), "toolchain", "t1", &out, Some(&seed), false, false, &|_| false).unwrap();
    assert!(done.signed);
    let config = Config::parse(&format!(
        r#"{{ "sources": ["{}"], "trusted": ["{}"] }}"#,
        out.display(),
        blaze_pack::peer::hex(&public)
    ))
    .unwrap();
    let home = dir.path().join("home");
    let mut store = empty(&home);
    // alloc: the test's store handle, which lives for the process as the daemon's does.
    let handle: &'static blaze_cas::Handle = Box::leak(Box::new(blaze_cas::Handle::new(store.store(), home.clone())));
    let packs = Arc::new(Packs::new(config, "t1".to_owned(), None, handle));
    // What `unit::compile::names` does on a miss with the packs its job carries.
    let look = |carried: Option<Arc<Packs>>, key: &Key| match carried {
        Some(carried) => super::asked(&carried, &[], false, || Ok(super::missed(key))).unwrap(),
        None => false,
    };
    let asked = |packs: &Packs| !matches!(packs.state.lock().sources[0], SourceState::New);

    // `--no-prebuilt`: nothing is asked and nothing arrives.
    assert!(given(Some(Arc::clone(&packs)), false).is_none(), "a request that said --no-prebuilt carries packs");
    assert!(!look(given(Some(Arc::clone(&packs)), false), &key(3)));
    assert!(!asked(&packs), "a source was asked for a request that said --no-prebuilt");
    assert!((0..12).all(|n| !handle.has(&key(n))), "a record arrived for a request that said --no-prebuilt");

    // The control, the switch ignored: the very lookup asks the source and takes the record.
    let ignored = Some(Arc::clone(&packs));
    assert!(look(ignored, &key(3)), "the source has the key and the lookup did not find it: the test tests nothing");
    assert!(asked(&packs));

    // Prebuilt (the default): the record and the rest of its pack are in the job's store,
    // as its record was put there, so the unit that looks it up finds it and is not built.
    let mut again = empty(&dir.path().join("home-2"));
    // alloc: the second home's handle, as above.
    let second: &'static blaze_cas::Handle = Box::leak(Box::new(blaze_cas::Handle::new(again.store(), dir.path().join("home-2"))));
    let config = Config::parse(&format!(
        r#"{{ "sources": ["{}"], "trusted": ["{}"] }}"#,
        out.display(),
        blaze_pack::peer::hex(&public)
    ))
    .unwrap();
    let prebuilt = given(Some(Arc::new(Packs::new(config, "t1".to_owned(), None, second))), true);
    assert!(prebuilt.is_some());
    assert!(look(prebuilt, &key(5)));
    for n in 0..12 {
        let got = second.read(&key(n), |entry| (entry.kind, entry.bytes.to_vec())).unwrap();
        assert_eq!(got, Some((kind(n), value(n))), "record {n} of the pack is not in the job's store as its source had it");
    }
}
