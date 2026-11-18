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

use crate::script_setting::{pinned_table, probe_table, shipped};
use crate::{Packages, Place, Side, Tree, World, WorldError, open_list, world};

const TRIPLE: &str = "aarch64-apple-darwin";
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
        incomplete: false,
    };
    let mut units = AVec::new_in(a());
    units.push(member);
    let graph: &'static UnitGraph<'static> = StdBox::leak(StdBox::new(UnitGraph { units, unavailable: AVec::new_in(a()), resolver: Resolver::V2, meta: AVec::new_in(a()) }));
    let (id, files) = crate::fixture::shelf(TRIPLE, std, TARGET_CFG, a());
    let mut shelf = Memory(files.into_iter().collect());
    let listed = open_list(&mut shelf, id, TRIPLE, graph, lock, &mut Member, a()).unwrap_or_else(|e| panic!("{e}"));
    let listed = StdBox::leak(StdBox::new(listed));
    world(graph, lock, listed, &mut Member, &NoFiles, a()).unwrap_or_else(|e| panic!("{e}"))
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
    let rows = probe_table(&w, Side::Target, TRIPLE, a()).unwrap_or_else(|e| panic!("{e}"));
    assert!(rows.windows(2).all(|pair| pair[0].key <= pair[1].key), "sorted by key");
    assert_eq!(answered(AUTOCFG, "", &rows).verdict, Verdict::Accepted(By::Decided), "autocfg's empty crate");
    assert_eq!(answered(FEATURE, "#![feature(xp_probe)]\n", &rows).verdict, Verdict::Accepted(By::Decided), "std's own declared feature");
    let absent = "#![feature(xp_absent)]\n";
    assert_eq!(answered(FEATURE, absent, &rows).verdict, Verdict::Rejected(Rejection::UnknownFeature { at: 11, len: 9 }), "declared by none: E0635");
    // The macro side's table is keyed for its own triple: the world has no macro cut, so it
    // ships nothing and decides nothing for the target.
    let macro_rows = probe_table(&w, Side::Macro, "wasm32-wasip1", a()).unwrap_or_else(|e| panic!("{e}"));
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
    let plain_rows = probe_table(&plain, Side::Target, TRIPLE, a()).unwrap_or_else(|e| panic!("{e}"));
    assert!(
        matches!(answered(FEATURE, "#![feature(xp_probe)]\n", &plain_rows).verdict, Verdict::Rejected(Rejection::UnknownFeature { .. })),
        "an unstaged std declares nothing"
    );
    std::println!("script setting: autocfg decided, xp_probe declared, {} rows (pinned alone: {})", rows.len(), pinned.len());
}
