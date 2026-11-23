//! The one probe table (script_setting.rs) answers what tempo's census exporter used to stop
//! on: autocfg's empty crate (num-traits, memoffset, eyre: `CannotAnswerYet`, export-9), and a
//! `#![feature]` probe from the toolchain's own declarations, over a world made from the
//! fixture's shelf.

use std::boxed::Box as StdBox;
use std::collections::BTreeMap as StdMap;
use std::string::String as StdString;
use std::vec::Vec as StdVec;

use blaze_build_script::probe::{Row, Table};
use blaze_build_script::probe_ask::{Answered, Ask, By, Needs, Open, Rejection, Verdict, answer, classify};
use blaze_manifest::lock::{Lock, LockId};
use blaze_own::{AString, AVec, Alloc};
use blaze_toolchain::Shelf;
use blaze_unit_graph::{CfgAtom, Resolver, Unit, UnitGraph, UnitKind};
use edition::{Edition as Syntax, PackageEdition};

use blaze_id::ContentId;

use crate::lib_features_record::{FeatureMemory, FeatureRecordWhy, FeatureRecords, decode, encode, features_key, phases};
use crate::script_setting::{FeatureTableError, feature_table, feature_table_by, pinned_table, probe_table, scan_phases, shipped};
use crate::{PackOrigin, Packages, Place, Side, Tree, World, WorldError, open_list, world};

const TRIPLE: &str = "aarch64-apple-darwin";
/// The tool the tests' library-features keys are made with.
const TOOL: ContentId = ContentId([3; 32]);
const TARGET_CFG: &[(&str, Option<&str>)] = &[("target_os", Some("macos")), ("unix", None)];

/// The fixture's std, staged, declaring one library feature of its own (and its crate's stable
/// one), as library/std declares its features.
const STAGED_STD: &str = "#![no_std]\n#![feature(staged_api)]\n#![stable(feature = \"xp_rust1\", since = \"1.0.0\")]\npub mod prelude { pub mod rust_2021 {} pub mod rust_2024 {} }\n#[unstable(feature = \"xp_probe\", issue = \"none\")]\npub struct Probe;\n";

/// num-traits' autocfg line as tempo's check printed it, for the target side.
const AUTOCFG: &[&str] = &[
    "--crate-name",
    "autocfg_dca8b2ac880013e3_0",
    "--crate-type=lib",
    "--out-dir",
    "/target/debug/build/num-traits-b230a6c6998cc448/out",
    "--emit=llvm-ir",
    "--target",
    "aarch64-apple-darwin",
    "-",
];
const FEATURE: &[&str] = &["--crate-type=lib", "--emit=metadata", "--target", "aarch64-apple-darwin", "-"];

fn a() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

fn b() -> blaze_own::Bump<'static> {
    blaze_own::testing::bump()
}

struct Memory(StdMap<StdString, StdVec<u8>>);

impl Shelf for Memory {
    type Error = &'static str;
    type Bytes = StdVec<u8>;
    fn place(&self) -> &str {
        "/shelf"
    }
    fn size(&mut self, name: &str) -> Result<Option<u64>, &'static str> {
        Ok(self.0.get(name).map(|b| b.len() as u64))
    }
    fn read(&mut self, name: &str, _size: u64) -> Result<StdVec<u8>, &'static str> {
        self.0.get(name).cloned().ok_or("gone")
    }
}

struct Member;

impl Packages<'static> for Member {
    fn place(&mut self, _package: LockId) -> Result<Place<'static>, WorldError<'static>> {
        Ok(Place::Checkout { dir: "hello" })
    }
}

struct NoFiles;

impl Tree for NoFiles {
    fn file(&self, _path: &str) -> Option<&[u8]> {
        None
    }
}

/// The world of `hello` (Rust 2021, so it opens std) over the fixture's shelf whose std is
/// `std`.
fn hello(std: &str) -> World<'static> {
    let text = "version = 4\n\n[[package]]\nname = \"hello\"\nversion = \"0.1.0\"\n";
    let lock: &'static Lock<'static> = StdBox::leak(StdBox::new(Lock::read(text.as_bytes(), a()).expect("the lock reads")));
    let s = |text: &str| AString::from_str_in(text, a());
    let mut cfg = AVec::new_in(a());
    cfg.push(CfgAtom::pair("target_os", "macos", a()));
    cfg.push(CfgAtom::flag("unix", a()));
    let member = Unit {
        package: lock.named("hello").next().expect("in the lock"),
        name: s("hello"),
        version: s("0.1.0"),
        kind: UnitKind::Lib,
        for_host: false,
        edition: PackageEdition { syntax: Syntax::Edition2021, blaze: None },
        crate_name: s("hello"),
        root: s("src/lib.rs"),
        features: AVec::new_in(a()),
        cfg,
        deps: AVec::new_in(a()),
        build_script: None,
        links: None,
    };
    let mut units = AVec::new_in(a());
    units.push(member);
    let graph: &'static UnitGraph<'static> = StdBox::leak(StdBox::new(UnitGraph { units, resolver: Resolver::V2, meta: AVec::new_in(a()), left_out: AVec::new_in(a()) }));
    let (id, files) = crate::fixture::shelf(TRIPLE, std, TARGET_CFG, a());
    let mut shelf = Memory(files.into_iter().collect());
    let listed = open_list(&mut shelf, id, TRIPLE, graph, lock, &mut Member, b()).unwrap_or_else(|e| panic!("{e}"));
    let listed = StdBox::leak(StdBox::new(listed));
    world(graph, lock, listed, &mut Member, &NoFiles, b()).unwrap_or_else(|e| panic!("{e}"))
}

fn answered<'t>(args: &[&'t str], snippet: &str, rows: &[Row]) -> Answered<'t> {
    match classify(args, a()) {
        Ok(Ask::Compile(compile)) => answer(&compile, TRIPLE, snippet.as_bytes(), &Table { rows }),
        other => panic!("{args:?} is not a try-compile: {other:?}"),
    }
}

/// The table blaze-cli's `Script` task and the census's exporter both take
/// (`ScriptSetup::open` makes it with [`probe_table`]) answers autocfg's empty crate for the
/// target (By::Decided, std shipped), accepts `#![feature(xp_probe)]` (std declares it) and
/// rejects an undeclared feature (E0635, the listing being there).
/// CONTROL: the table the exporter made before (`pinned_rows` alone, no ships facts and no
/// listing) leaves the autocfg probe `Unanswered(Sysroot(Std))`: export-9's `CannotAnswerYet`
/// for eyre, memoffset and num-traits. CONTROL: the same world with the fixture's unstaged std
/// lists no `xp_probe`, so that probe is refused. Prints
/// `script setting: autocfg decided, xp_probe declared, <n> rows (pinned alone: <m>)`.
#[test]
fn script_setting_the_shared_table_answers_autocfg_and_the_toolchains_features() {
    let w = hello(STAGED_STD);
    assert!(shipped(&w, Side::Target).any(|name| name == "std"), "the world opens std");
    let rows = probe_table(&w, Side::Target, TRIPLE, TOOL, &mut FeatureMemory::new(a()), a()).unwrap_or_else(|e| panic!("{e}"));
    assert!(rows.windows(2).all(|pair| pair[0].key <= pair[1].key), "sorted by key");
    assert_eq!(answered(AUTOCFG, "", &rows).verdict, Verdict::Accepted(By::Decided), "autocfg's empty crate");
    assert_eq!(answered(FEATURE, "#![feature(xp_probe)]\n", &rows).verdict, Verdict::Accepted(By::Decided), "std's own declared feature");
    let absent = "#![feature(xp_absent)]\n";
    assert_eq!(answered(FEATURE, absent, &rows).verdict, Verdict::Rejected(Rejection::UnknownFeature { at: 11, len: 9 }), "declared by none: E0635");
    // The macro side's table is keyed for its own triple: the world has no macro cut, so it
    // ships nothing and decides nothing for the target.
    let macro_rows = probe_table(&w, Side::Macro, "wasm32-wasip1", TOOL, &mut FeatureMemory::new(a()), a()).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(answered(AUTOCFG, "", &macro_rows).verdict, Verdict::Unanswered(Open::Sysroot(Needs::Std)));

    let mut pinned = blaze_build_script::probe::pinned_rows(TRIPLE, blaze_build_script::pinned::TEMPO, a());
    pinned.sort_by(|x, y| x.key.cmp(&y.key));
    assert_eq!(answered(AUTOCFG, "", &pinned).verdict, Verdict::Unanswered(Open::Sysroot(Needs::Std)), "the export's old table");
    // The ships facts are the difference: the pinned table without the feature listings
    // already decides autocfg's probe.
    let mut ships = pinned_table(TRIPLE, shipped(&w, Side::Target), a());
    ships.sort_by(|x, y| x.key.cmp(&y.key));
    assert_eq!(answered(AUTOCFG, "", &ships).verdict, Verdict::Accepted(By::Decided), "the ships facts decide it");

    let plain = hello(crate::fixture::STD);
    let plain_rows = probe_table(&plain, Side::Target, TRIPLE, TOOL, &mut FeatureMemory::new(a()), a()).unwrap_or_else(|e| panic!("{e}"));
    assert!(
        matches!(answered(FEATURE, "#![feature(xp_probe)]\n", &plain_rows).verdict, Verdict::Rejected(Rejection::UnknownFeature { .. })),
        "an unstaged std declares nothing"
    );
    std::println!("script setting: autocfg decided, xp_probe declared, {} rows (pinned alone: {})", rows.len(), pinned.len());
}

/// Rows as plain values, to compare two tables.
fn pairs(rows: &[Row]) -> StdVec<([u8; 32], bool)> {
    rows.iter().map(|row| (row.key.0, row.accepted)).collect()
}

/// The library-features key of the target side of `w`.
fn key_of(w: &World<'static>, triple: &str, tool: ContentId) -> ContentId {
    let phases = phases(w, Side::Target, a());
    features_key(w, &phases, triple, tool, a()).unwrap_or_else(|e| panic!("{e}"))
}

/// Rule 26: the library-features record round-trips. The miss walks and writes one record; the
/// hit writes nothing and answers the rows the walk made; the record held is, byte for byte,
/// the walk's listing written, and that record read back and written again is the same bytes.
/// Prints `library features record: <n> bytes, <m> rows`.
#[test]
fn library_features_record_round_trips_in_bytes() {
    let w = hello(STAGED_STD);
    let mut held = FeatureMemory::new(a());
    let made = feature_table(&w, Side::Target, TRIPLE, TOOL, &mut held, a()).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(held.len(), 1, "the miss wrote one record");
    let read = feature_table(&w, Side::Target, TRIPLE, TOOL, &mut held, a()).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(held.len(), 1, "the hit wrote nothing");
    assert_eq!(pairs(&made), pairs(&read), "the rows read back are the rows walked");

    let phases = phases(&w, Side::Target, a());
    let key = features_key(&w, &phases, TRIPLE, TOOL, a()).unwrap_or_else(|e| panic!("{e}"));
    let walked = scan_phases(&w, &phases, TRIPLE, a()).unwrap_or_else(|e| panic!("{e}"));
    let bytes = encode(key, &walked, a()).unwrap_or_else(|e| panic!("{e}"));
    let back = decode(key, &bytes, a()).unwrap_or_else(|e| panic!("{e}"));
    assert!(back.iter().any(|phase| phase.needs == Needs::Std && phase.declared.contains(&"xp_probe")), "std's own feature is in the record");
    let again = encode(key, &back, a()).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(bytes[..], again[..], "read back and written again: the same bytes");
    let stored = held.get(key).unwrap_or_else(|e| panic!("{e}")).expect("the record is under its key");
    assert_eq!(stored, &bytes[..], "the record held is the walk's listing written");
    std::println!("library features record: {} bytes, {} rows", bytes.len(), made.len());
}

/// Rule 21: the key moves with each input of the walk, and only with them. Two worlds made
/// apart from one input give one key; another triple, tool, crate cfg, crate features, pack
/// digest (what the toolchain's id names) or toolchain each give another.
#[test]
fn library_features_key_moves_with_each_input() {
    let base = hello(STAGED_STD);
    let at = key_of(&base, TRIPLE, TOOL);
    assert_eq!(at, key_of(&hello(STAGED_STD), TRIPLE, TOOL), "one input, one key");
    assert_ne!(at, key_of(&base, "aarch64-unknown-linux-gnu", TOOL), "the triple");
    assert_ne!(at, key_of(&base, TRIPLE, ContentId([4; 32])), "the tool");

    let std_at = base.crates.iter().position(|krate| krate.input.name == "std" && matches!(krate.side, Side::Target)).expect("the world opens std");
    let mut cfg = hello(STAGED_STD);
    cfg.crates[std_at].input.cfg = &[("target_os", Some("linux")), ("unix", None)];
    assert_ne!(at, key_of(&cfg, TRIPLE, TOOL), "std's cfg");

    let mut features = hello(STAGED_STD);
    features.crates[std_at].input.features = &["xp_extra"];
    assert_ne!(at, key_of(&features, TRIPLE, TOOL), "std's features");

    let mut pack = hello(STAGED_STD);
    let root = pack.crates[std_at].input.root;
    let tree: usize = root.strip_prefix('/').and_then(|rest| rest.split_once('/')).and_then(|(tree, _)| tree.parse().ok()).expect("a root names its tree");
    assert!(matches!(pack.texts.origins[tree - 1], PackOrigin::Shelf { .. }), "std's tree is a shelf pack");
    pack.texts.origins[tree - 1] = PackOrigin::Shelf { digest: [7; 32] };
    assert_ne!(at, key_of(&pack, TRIPLE, TOOL), "std's pack");

    assert_ne!(at, key_of(&hello(crate::fixture::STD), TRIPLE, TOOL), "another toolchain (another std)");
}

/// Rule 77: a hit lexes nothing. The walk is a seam; on a store that holds the record, a seam
/// that panics is never called and the rows are the walk's. CONTROL: on an empty store the
/// seam is called exactly once (it is the one way the listing is made).
#[test]
fn library_features_hit_never_lexes() {
    let w = hello(STAGED_STD);
    let mut held = FeatureMemory::new(a());
    let mut walks = 0;
    let made = feature_table_by(&w, Side::Target, TRIPLE, TOOL, &mut held, a(), |phases, alloc| {
        walks += 1;
        scan_phases(&w, phases, TRIPLE, alloc)
    })
    .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(walks, 1, "the miss walked once");
    let read = feature_table_by(&w, Side::Target, TRIPLE, TOOL, &mut held, a(), |_, _| panic!("a hit lexed the toolchain")).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(pairs(&made), pairs(&read));
    // Another triple is another key: a miss, so the seam is asked again.
    let mut asked = false;
    let other = feature_table_by(&w, Side::Target, "aarch64-unknown-linux-gnu", TOOL, &mut held, a(), |phases, alloc| {
        asked = true;
        scan_phases(&w, phases, "aarch64-unknown-linux-gnu", alloc)
    });
    assert!(other.is_ok() && asked, "another key walks");
}

/// Rule 3: a record under the key that does not read is the typed error naming the key and the
/// byte, and nothing walks in its place (the seam panics): an old form, another key's record,
/// one cut short, one with bytes after its end.
#[test]
fn library_features_record_that_does_not_read_is_refused_by_name() {
    let w = hello(STAGED_STD);
    let phases = phases(&w, Side::Target, a());
    let key = features_key(&w, &phases, TRIPLE, TOOL, a()).unwrap_or_else(|e| panic!("{e}"));
    let walked = scan_phases(&w, &phases, TRIPLE, a()).unwrap_or_else(|e| panic!("{e}"));
    let good: StdVec<u8> = encode(key, &walked, a()).unwrap_or_else(|e| panic!("{e}")).iter().copied().collect();
    let other_key = ContentId([9; 32]);
    let mut old = good.clone();
    old[4..8].copy_from_slice(&0u32.to_le_bytes());
    let another: StdVec<u8> = encode(other_key, &walked, a()).unwrap_or_else(|e| panic!("{e}")).iter().copied().collect();
    let short = StdVec::from(&good[..good.len() - 1]);
    let mut trailing = good.clone();
    trailing.push(0);
    let cases: [(&str, StdVec<u8>, fn(&FeatureRecordWhy) -> bool); 4] = [
        ("an old form", old, |why| *why == FeatureRecordWhy::Version { found: 0 }),
        ("another key's record", another, |why| *why == FeatureRecordWhy::Key { found: ContentId([9; 32]) }),
        ("cut short", short, |why| matches!(why, FeatureRecordWhy::Short { .. })),
        ("bytes after its end", trailing, |why| matches!(why, FeatureRecordWhy::Trailing { .. })),
    ];
    for (what, bytes, expected) in cases {
        let mut held = FeatureMemory::new(a());
        held.put(key, &bytes).unwrap_or_else(|e| panic!("{e}"));
        match feature_table_by(&w, Side::Target, TRIPLE, TOOL, &mut held, a(), |_, _| panic!("{what}: walked in place of a record")) {
            Err(FeatureTableError::Record(error)) => {
                assert_eq!(error.key, key, "{what}: the error names the key");
                assert!(expected(&error.why), "{what}: {:?}", error.why);
            }
            Err(other) => panic!("{what}: another refusal: {other}"),
            Ok(_) => panic!("{what}: read as a record"),
        }
    }
}
