//! `blaze run`'s parent reads its crates' records IN PLACE and opens a tree only when a text of
//! it is read (runtime.md C3; run_names.rs `open`, engine_records.rs `EngineRecords`,
//! body_types.rs `LazyTrees`). A child of main.rs's tests.
//!
//! The store is the command's own form (store/plain_place.rs over blaze-core's simulation, as
//! code_store_tests.rs opens it): each crate's whole `CrateScope` record put, committed into a
//! segment, and read back through the one reader `blaze check` reads through.


use blaze_cas::plain::{Lent, NotInPlace, Settings as StoreSettings};
use blaze_core::FileName;
use blaze_core::sim::{Faults, Sim};
use blaze_drive::{Input, Key, Name};
use blaze_fetch::Mode;
use blaze_fetch::pack::{Builder, Kind as PackKind, Limits as PackLimits, PackView};
use blaze_id::CrateItemKey;
use blaze_ids::ContentId;
use blaze_own::Alloc;
use blaze_scope::graph::Sysroot;
use blaze_scope::resolve::{CrateInput, Names, SourceTexts};
use blaze_surface::Edition;
use blaze_unit_graph::{Implicit, Prelude, StdIs};
use bstd::cx::Cx;
use bstd::io::{Io, Limits, SimWorld};
use bstd::mem::Frame;

use crate::CliError;
use crate::body_types::{LazyTrees, OpenTree, TreesSeen};
use crate::engine_records::{EngineRecords, InPlace, ReadError, Route};
use crate::job_texts::Packs;
use crate::names::sections;
use crate::store::plain_place::{Facts, PlainPlace, StoreAlloc, config, open};
use crate::store::{Held, Store, StoreKept};

const RUST: Implicit = Implicit { std: StdIs::Rust, prelude: Prelude::Rust(edition::Edition::Edition2021), proc_macro: false, entry: false };

/// Three crates: `app` reads `lib`, `lib` reads `base`.
const APP: &str = "#![no_core]\nuse lib::m::make;\npub struct Own;\nfn main() { let _ = make(); }\n";
const LIB: &str = "#![no_core]\npub mod m {\n    pub struct Thing;\n    pub fn make() -> Thing { Thing }\n}\npub use base::b;\n";
const BASE: &str = "#![no_core]\npub fn b() {}\npub struct Base;\n";

struct Files;

impl<'a> SourceTexts<'a> for Files {
    fn read(&self, _krate: usize, path: &str) -> Option<&'a str> {
        match path {
            "/0/app/lib.rs" => Some(APP),
            "/1/lib/lib.rs" => Some(LIB),
            "/2/base/lib.rs" => Some(BASE),
            _ => None,
        }
    }
}

fn input(name: &'static str, root: &'static str, deps: &'static [(&'static str, usize)]) -> CrateInput<'static> {
    CrateInput {
        name,
        key: CrateItemKey(ContentId::of("blaze-cli run records in place test crate", name.as_bytes())),
        edition: Edition::E2021,
        implicit: RUST,
        root,
        shown_from: "/",
        features: &[],
        deps,
        cfg: &[],
        script: None,
    }
}

/// One crate's stored record: its key, its `CrateScope` input, and its payload's three sections'
/// bytes as names.rs `run` writes them.
struct Stored {
    key: CrateItemKey,
    input: Input,
    whole: std::vec::Vec<u8>,
    names: std::vec::Vec<u8>,
    items: std::vec::Vec<u8>,
    expansions: std::vec::Vec<u8>,
    /// The expansion records its directory names, by key, put beside it.
    extras: std::vec::Vec<(Key, std::vec::Vec<u8>)>,
}

/// The three crates solved in memory and each one's whole `CrateScope` record (a plain head,
/// then the payload: blaze-drive record.rs), as the store holds it.
fn stored() -> std::vec::Vec<Stored> {
    let g = blaze_own::testing::heap().alloc();
    let inputs = [input("app", "/0/app/lib.rs", &[("lib", 1)]), input("lib", "/1/lib/lib.rs", &[("base", 2)]), input("base", "/2/base/lib.rs", &[])];
    let sysroot = Sysroot { core: None, alloc: None, std: None, bstd: None };
    let names: Names<'static> = blaze_scope::resolve::resolve_crates(&inputs, sysroot, &Files, &blaze_scope::task::Serial, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("the three crates' names: {e:?}"));
    let mut out = std::vec::Vec::new();
    for c in names.crates().iter() {
        let n = blaze_scope::names_record::encode(c, g).unwrap_or_else(|e| panic!("names record of `{}`: {e:?}", c.name));
        let i = blaze_scope::items_record::encode(c, g).unwrap_or_else(|e| panic!("items record of `{}`: {e:?}", c.name));
        // The writer names.rs `scope_record` writes through: the directory is the section, the
        // expansion records are put beside the crate's record (front_load.rs's tests' `payloads`).
        let split = blaze_scope::expansion_records::write(c, g).unwrap_or_else(|e| panic!("expansions of `{}`: {e}", c.name));
        let extras: std::vec::Vec<(Key, std::vec::Vec<u8>)> = split.records.iter().map(|r| (Key(r.key), r.bytes.to_vec())).collect();
        let payload = sections::write(0, [&[], &[], &n[..], &i[..], &[], &split.directory[..], &[], &[]]);
        let parts = sections::sections(&payload).unwrap_or_else(|e| panic!("the sections of `{}`: {e}", c.name));
        let (names_bytes, items_bytes, expansions_bytes) = (parts.names.to_vec(), parts.items.to_vec(), parts.expansions.to_vec());
        let name = blaze_drive::key::crate_scope_name(&c.key);
        let source = ContentId::of("blaze-cli run records in place test source", c.name.as_bytes());
        let made = ContentId::of("blaze-cli run records in place test out", c.name.as_bytes());
        let head = blaze_drive::record::write_plain_head(name, source, made, &[], payload.len(), g);
        let mut whole = std::vec::Vec::new();
        whole.extend_from_slice(&head);
        whole.extend_from_slice(&payload);
        let at = Key(ContentId::of("blaze-cli run records in place test key", c.name.as_bytes()));
        out.push(Stored { key: c.key, input: Input { name, at, out: made }, whole, names: names_bytes, items: items_bytes, expansions: expansions_bytes, extras });
    }
    assert_eq!(out.len(), 3, "the fixture is three crates");
    // The three crates call no macro: no expansion record, so each directory is empty and the
    // counts below (3 records, 3 gets) are the crates' records alone. A macro call here adds a
    // record read and a get per expansion record served (engine_records.rs `InPlaceRecords`).
    assert!(out.iter().all(|one| one.extras.is_empty()), "the fixture grew a macro call: its counts no longer hold");
    out
}

/// Process `n` of one boot over `sim`: the command's store as `open_for_command` opens it.
fn process(sim: Sim, n: u64) -> Lent<Sim, StoreAlloc> {
    let alloc = blaze_own::testing::heap().alloc();
    let cfg = config(FileName::new("store").expect("a name"), &StoreSettings::default(), Facts { boot: Some(1), nonce: 9100 + n }, alloc);
    open(sim, cfg, alloc).expect("the store opens")
}

/// The command's store over the simulation, counting its gets.
struct Placed<'m> {
    place: PlainPlace<'m, Sim>,
    gets: u32,
}

impl<'m> Store<'m> for Placed<'m> {
    fn get(&mut self, _cx: &Cx, key: Key) -> Result<Option<Held<'m>>, CliError> {
        self.gets += 1;
        Ok(self.place.get(key)?)
    }
    fn put(&mut self, _cx: &Cx, key: Key, bytes: &[u8]) -> Result<(), CliError> {
        Ok(self.place.put(key, false, bytes)?)
    }
    fn put_parts(&mut self, _cx: &Cx, key: Key, parts: &[&[u8]]) -> Result<(), CliError> {
        Ok(self.place.put_parts(key, false, parts)?)
    }
    fn put_record(&mut self, _cx: &Cx, key: Key, tag: blaze_keyed::kind::Tag, _owner: &[&[u8]], bytes: &[u8]) -> Result<blaze_keyed::store::Put, CliError> {
        // Every put here is local (`put` and `put_parts` above), so the owner's namespace is too.
        Ok(self.place.put_record(key, false, tag, bytes)?)
    }
    fn map_all(&mut self, _cx: &Cx) -> Result<(), CliError> {
        Ok(())
    }
    fn after_answer(&mut self, _cx: &Cx, _profile: &mut blaze_drive::profile::Profile) -> Result<(), CliError> {
        Ok(())
    }
    fn public_crates(&mut self, _subjects: &[ContentId]) {}
    fn toolchain_crates(&mut self, _subjects: &[ContentId]) {}
    fn keep_with_toolchain(&mut self, _key: Key) -> Result<(), CliError> {
        Ok(())
    }
    fn index_beside(&mut self, _cx: &Cx, owner: Key, keys: &[Key]) -> Result<(), CliError> {
        self.place.index_beside(owner, keys)?;
        Ok(())
    }
    fn kept(&self) -> StoreKept {
        StoreKept::default()
    }
}

fn root_cx(seed: u64) -> (Io, Cx) {
    let io = Io::simulated(SimWorld::new(seed, Limits::USUAL));
    let cx = io.cx(bstd::test::BUDGET, bstd::mem::TASK_FRAME).expect("a root context");
    (io, cx)
}

/// (a) and (b): THE RUN'S READER COPIES NOTHING. Three crates' records put and committed into a
/// segment, then read through `EngineRecords` (what run_names.rs `open` reads through): each
/// crate's names, items and expansion texts sections are byte-equal to its payload's, and the
/// names and items sections lie INSIDE the segment's mapping (the record the store lends, which
/// `Held::place` proves is a segment's, not the arena's); 0 bytes are counted copied, every
/// section byte counted read. A second ask of a crate is a hit: no store get, the same borrow.
/// CONTROL for the pointers: engine_records.rs's `Copying` route lands outside the record
/// (front_load.rs `the_copying_route_counts_every_byte_it_copies`).
/// Prints `run records: 3 crates read in place (B section bytes, 0 copied), a second ask a hit`.
#[test]
fn the_runs_reader_reads_each_crates_sections_in_place() {
    let (_io, cx) = root_cx(41);
    let g: Alloc<'static> = blaze_own::testing::heap().alloc();
    let crates = stored();
    let lent = process(Sim::new(41, Faults::default()), 1);
    let mut store = Placed { place: PlainPlace::new(&lent, false), gets: 0 };
    for one in crates.iter() {
        store.put(&cx, one.input.at, &one.whole).unwrap_or_else(|e| panic!("the put of {:?}: {e}", one.input.name));
        for (key, bytes) in one.extras.iter() {
            store.put_record(&cx, *key, blaze_keyed::kind::ReservedTag::Expansion.tag(), &[&one.whole], bytes).unwrap_or_else(|e| panic!("the expansion record {key:?} of {:?}: {e}", one.input.name));
        }
    }
    store.place.commit().unwrap_or_else(|e| panic!("the commit: {e:?}"));
    let mut read = EngineRecords::new(g);
    let mut sections_read = 0u64;
    for one in crates.iter() {
        let records = read.records(&cx, &mut store, &one.input, one.key, g).unwrap_or_else(|e| panic!("in place: {e}"));
        assert_eq!(records.key, one.key, "the record read is another crate's");
        assert_eq!(records.names, &one.names[..], "the names section differs from the payload's");
        assert_eq!(records.items, &one.items[..], "the items section differs from the payload's");
        let held = store.place.get(one.input.at).unwrap_or_else(|e| panic!("the get: {e:?}")).expect("the record is held");
        assert!(held.place.is_ok(), "the record was lent from the arena, not a segment: {:?}", held.place.as_ref().err());
        let mapped = held.bytes.as_ptr_range();
        assert!(mapped.contains(&records.names.as_ptr()), "the names section is not borrowed from the segment's mapping");
        assert!(mapped.contains(&records.items.as_ptr()), "the items section is not borrowed from the segment's mapping");
        sections_read += (one.names.len() + one.items.len() + one.expansions.len()) as u64;
    }
    let first = read.counts();
    assert_eq!(first.records, 3, "each crate's record opened once: {first:?}");
    assert_eq!(first.copied, 0, "a byte of a section was copied: {first:?}");
    assert_eq!(first.sections, sections_read, "the sections' bytes are not counted read: {first:?}");
    assert_eq!(store.gets, 3, "one store get a crate");
    // (b) The same crate asked again: a hit, no get, the same bytes where they lie.
    let again = read.records(&cx, &mut store, &crates[1].input, crates[1].key, g).unwrap_or_else(|e| panic!("the second ask: {e}"));
    let once = read.records(&cx, &mut store, &crates[1].input, crates[1].key, g).unwrap_or_else(|e| panic!("the third ask: {e}"));
    assert_eq!(store.gets, 3, "a second ask went to the store");
    assert_eq!(again.names.as_ptr(), once.names.as_ptr(), "a hit handed out other bytes");
    let after = read.counts();
    assert_eq!((after.records, after.hits, after.copied), (3, 2, 0), "the asks again were not hits: {after:?}");
    let line = crate::run_names::NamesRead::of(after, &TreesSeen::default()).to_string();
    assert!(line.starts_with("names read: crates 3, sections in place ") && line.contains("copied 0 bytes; trees opened 0 of 0"), "{line}");
    println!("run records: 3 crates read in place ({} section bytes, 0 copied), a second ask a hit", after.sections);
}

/// (d) Under `Visibility::AfterCommit`, a record this command put and did not commit is lent from
/// the unshared arena: the reader refuses it BY NAME (`ReadError::NotInPlace`, the arena), never
/// copies it to an aligned place (rule 3). Under the live `Visibility::InArena` default, the same
/// uncommitted record lives in the 16-byte-aligned shared arena and reads in place with 0 copies.
/// CONTROL: the same record after the commit reads (the test above).
#[test]
fn a_record_still_in_the_arena_is_refused_by_name() {
    let (_io, cx) = root_cx(43);
    let g: Alloc<'static> = blaze_own::testing::heap().alloc();
    let crates = stored();
    let cfg = config(FileName::new("store").expect("a name"), &StoreSettings::default(), Facts { boot: Some(1), nonce: 9101 }, g);
    let driven = blaze_cas::plain::Driven::with(Sim::new(43, Faults::default()), std::path::Path::new("/"), cfg, g).expect("the store opens");
    let lent = Lent::with_visibility(driven, g, blaze_cas::plain::Visibility::AfterCommit);
    let mut store = Placed { place: PlainPlace::new(&lent, false), gets: 0 };
    let one = &crates[2];
    store.put(&cx, one.input.at, &one.whole).unwrap_or_else(|e| panic!("the put: {e}"));
    let mut read = EngineRecords::new(g);
    match read.records(&cx, &mut store, &one.input, one.key, g) {
        Err(error @ ReadError::NotInPlace { why: NotInPlace::Arena { .. }, .. }) => {
            let ReadError::NotInPlace { name, .. } = &error else { unreachable!() };
            assert_eq!(*name, one.input.name, "the refusal names another record");
            assert!(error.to_string().contains("not read in place"), "{error}");
        }
        Err(other) => panic!("an uncommitted record was refused for another reason: {other}"),
        Ok(_) => panic!("an uncommitted record was read: from the arena, not the mapping"),
    }
    assert_eq!(read.counts().copied, 0, "the refusal copied");

    // Companion check under the live `Visibility::InArena` default: the uncommitted put is read
    // directly from the 16-byte-aligned shared arena with zero copies.
    let live = process(Sim::new(43, Faults::default()), 2);
    let mut live_store = Placed { place: PlainPlace::new(&live, false), gets: 0 };
    live_store.put(&cx, one.input.at, &one.whole).unwrap_or_else(|e| panic!("the put: {e}"));
    let mut live_read = EngineRecords::new(g);
    let _ = live_read.records(&cx, &mut live_store, &one.input, one.key, g).unwrap_or_else(|e| panic!("InArena read: {e}"));
    assert_eq!(live_read.counts().copied, 0, "InArena read copied");
}

/// (d) A section that does not start at a multiple of sixteen is the typed refusal naming the
/// record and the section, never a copy (engine_records.rs `InPlace`). A whole record laid by
/// the store cannot be misaligned (blaze-cas lays every record and payload at sixteen, and the
/// open refuses a segment that does not: format.rs `check_segment`), so the section is cut at
/// one byte past an aligned start here, the one place a misaligned slice can arise.
#[test]
fn a_misaligned_section_is_refused_by_name() {
    let g: Alloc<'static> = blaze_own::testing::heap().alloc();
    let base: &'static [u8] = blaze_scope::resolve::aligned(&[7u8; 64], blaze_own::testing::bump());
    let name: Name = blaze_drive::key::crate_scope_name(&CrateItemKey(ContentId::of("blaze-cli run records in place test crate", b"bent")));
    let mut counts = crate::engine_records::ReadCounts::default();
    let whole = <InPlace as Route>::section(name, "names section", &base[..32], &mut counts, g).unwrap_or_else(|e| panic!("CONTROL: an aligned section was refused: {e}"));
    assert_eq!(whole.as_ptr(), base.as_ptr(), "CONTROL: the aligned section was not borrowed");
    match <InPlace as Route>::section(name, "names section", &base[1..33], &mut counts, g) {
        Err(error @ ReadError::Misaligned { which: "names section", .. }) => {
            let ReadError::Misaligned { name: named, .. } = &error else { unreachable!() };
            assert_eq!(*named, name, "the refusal names another record");
        }
        other => panic!("a misaligned section was {other:?}"),
    }
    assert_eq!(counts.copied, 0);
}

/// Three packs of one file each, `src/lib.rs` saying which tree it is.
fn pack(k: usize) -> PackView<'static> {
    let alloc = blaze_own::testing::heap().alloc();
    let limits = PackLimits { most_files: 16, most_bytes: 1 << 16, most_path: 128 };
    let mut builder = Builder::new(PackKind::Crate, [k as u8; 32], limits, 0, alloc).expect("a builder");
    let text: &'static str = std::boxed::Box::leak(std::format!("pub fn tree_{k}() {{}}\n").into_boxed_str());
    builder.file("src/lib.rs", Mode::File, text.as_bytes()).expect("a file");
    let bytes: &'static [u8] = std::boxed::Box::leak(builder.finish().expect("a pack").to_vec().into_boxed_slice());
    PackView::open(bytes).unwrap_or_else(|_| panic!("pack {k} opens"))
}

/// An opener that counts its opens and refuses tree 3 (a pack whose file is not on the shelf).
struct Counted {
    packs: [PackView<'static>; 2],
    opens: core::cell::Cell<u32>,
}

impl OpenTree for Counted {
    fn open(&self, k: usize) -> Result<PackView<'static>, &'static str> {
        self.opens.set(self.opens.get() + 1);
        match k {
            1 | 2 => Ok(self.packs[k - 1]),
            _ => Err("tree 3 of the crate list, `<home>/shelf/..`, could not be mapped (the test's)"),
        }
    }
}

/// (c) TREES ON FIRST READ: a list of three trees opens none until a text is read; a read of
/// one text of tree 1 opens tree 1 alone (opened 1 of 3); a second read of it opens nothing
/// again; tree 0 (the checkout) is never a pack. A tree that cannot be opened is answered none
/// and its refusal is kept, once, for the run to stop on (main.rs). The texts come through
/// job_texts.rs `Packs`, the one interface a job reads trees by.
#[test]
fn a_tree_opens_at_the_first_read_of_a_text_from_it() {
    let (_io, cx) = root_cx(46);
    let frame = Frame::new(bstd::mem::Tag::Other).expect("a frame");
    let opener = Counted { packs: [pack(1), pack(2)], opens: core::cell::Cell::new(0) };
    let seen = TreesSeen::default();
    let lazy = LazyTrees::new(&opener, 3, &seen, &frame);
    let packs: Packs<'_> = Packs::from(&lazy);
    assert_eq!(seen.opened(), (0, 3), "a tree was opened before any read");
    let text = packs.tree(&cx, &frame, 1).and_then(|view| view.find("src/lib.rs")).map(|file| file.bytes);
    assert_eq!(text, Some(&b"pub fn tree_1() {}\n"[..]), "tree 1's text");
    assert_eq!(seen.opened(), (1, 3), "one read of tree 1 opened other trees");
    assert!(packs.tree(&cx, &frame, 1).is_some() && packs.tree(&cx, &frame, 0).is_none(), "tree 1 again, and tree 0 is the checkout");
    assert_eq!((opener.opens.get(), seen.opened()), (1, (1, 3)), "a second read of tree 1 opened it again");
    assert!(seen.refused().is_none(), "a refusal before any failed open");
    assert!(packs.tree(&cx, &frame, 3).is_none() && packs.tree(&cx, &frame, 3).is_none(), "a tree that could not be opened was served");
    assert_eq!(opener.opens.get(), 2, "a refused tree was opened again at its second read");
    assert!(seen.refused().is_some_and(|why| why.starts_with("tree 3 of the crate list")), "the refusal was not kept: {:?}", seen.refused());
    assert!(packs.tree(&cx, &frame, 4).is_none(), "a tree past the list's");
    assert_eq!(seen.opened(), (2, 3));
    // CONTROL: the opened slice, every tree before any read, serves the same text.
    let views = [pack(1), pack(2)];
    let open: Packs<'_> = Packs::from(&views[..]);
    assert_eq!(open.tree(&cx, &frame, 1).and_then(|view| view.find("src/lib.rs")).map(|file| file.bytes), text, "the two schedules serve other texts");
}

#[test]
fn exports_cache_serves_repeat_reads_without_store_gets() {
    let (_io, cx) = root_cx(47);
    let g: Alloc<'static> = blaze_own::testing::heap().alloc();
    let crates = stored();
    let lent = process(Sim::new(47, Faults::default()), 1);
    let mut store = Placed { place: PlainPlace::new(&lent, false), gets: 0 };
    for one in crates.iter() {
        store.put(&cx, one.input.at, &one.whole).unwrap_or_else(|e| panic!("put: {e}"));
    }
    store.place.commit().unwrap_or_else(|e| panic!("commit: {e:?}"));
    let mut read = EngineRecords::new(g);
    let first = read.exports::<crate::engine_records::InPlace>(&cx, &mut store, &crates[0].input, g).expect("first exports read");
    assert_eq!(store.gets, 1, "first read hits the store once");
    for _ in 0..10 {
        let again = read.exports::<crate::engine_records::InPlace>(&cx, &mut store, &crates[0].input, g).expect("cached exports read");
        assert_eq!(again.exports.as_ptr(), first.exports.as_ptr(), "cached borrow points to same mapping");
    }
    assert_eq!(store.gets, 1, "repeat exports reads never hit the store again");
    assert_eq!(read.counts().records, 1);
    assert_eq!(read.counts().hits, 10);
    assert_eq!(read.counts().copied, 0);
}
