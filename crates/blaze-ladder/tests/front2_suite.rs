//! A crate's tests as ONE program (docs/v2/plan/design/test.md, pieces 8, 13 and 18), on a small
//! library with three tests, in the style of tests/front2_project.rs. Needs the installed
//! toolchain under `$HOME/.blaze/v2/toolchain`.
//!
//! The Rust Reference, "Testing attributes" (`attributes.testing.test.allowed-positions`): "The
//! test attribute may only be applied to free functions that are monomorphic, that take no
//! arguments, and where the return type implements the Termination trait." Types that implement
//! it include `()` and `Result<T, E> where T: Termination, E: Debug`; "The test harness calls the
//! returned value's report method, and classifies the test as passed or failed depending on
//! whether the resulting ExitCode represents successful termination." `ignore` "may optionally be
//! written with the MetaNameValueStr syntax to specify a reason"; `should_panic`'s "input string
//! ... must appear within the panic message".
//!
//! Run by the front-end integrator:
//! `LANECHECK_WAIT=540 bash /Volumes/Projects/blaze-int-1112-notes/lanecheck.sh blaze-ladder test "--test front2_suite -- --nocapture"`.

#![feature(allocator_api)]

use blaze_id::{ContentId, CrateItemKey};
use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::keys::checker_of;
use blaze_ladder::front2::program::Front2Program;
use blaze_ladder::front2::program::suite::{Reach, Suite, SuiteEntry};
use blaze_ladder::front2::project::{BodyRef, BodyVerdict, ProjectWorld};
use blaze_ladder::front2::shared_keys::{ByPlace, Look, SharedReads, SharedRecords, UnitBase, atom_digest, cfg_reads, relative_reads, shared_key};
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::seam::FrontEnd;
use blaze_own::{AVec, Alloc};
use blaze_scope::resolve::{Names, aligned};
use blaze_scope::tests_record::{ShouldPanic, TestsRecord, encode};
use blaze_surface::Edition;

/// The library: `add` reads no cfg and is shared by two tests; `width` reads `test` (its body
/// differs under it: a `u64` given to a `u32` in the test unit, E0308); `bump` reads no cfg in
/// its text, but its method call resolves differently under `test` (the `#[cfg(test)]` inherent
/// `step` is found before the trait's); `helper` is `#[cfg(test)]`; the three tests are of the
/// three shapes the brief names.
const LIB: &str = "pub fn add(a: u32, b: u32) -> u32 {
    a + b
}

pub fn width() -> u32 {
    #[cfg(not(test))]
    let w: u32 = 4;
    #[cfg(test)]
    let w: u64 = 8;
    let out: u32 = w;
    out
}

pub struct Counter(pub u32);

pub trait Step {
    fn step(&self) -> u32 {
        1
    }
}

impl Step for Counter {}

#[cfg(test)]
impl Counter {
    fn step(&self) -> u32 {
        2
    }
}

pub fn bump(c: &Counter) -> u32 {
    c.step() + c.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn helper() -> u32 {
        add(1, 2)
    }

    #[test]
    fn adds() -> Result<(), String> {
        if helper() == 3 { Ok(()) } else { Err(String::from(\"not three\")) }
    }

    #[test]
    #[should_panic(expected = \"boom\")]
    fn panics() {
        if add(2, 2) == 4 {
            panic!(\"boom\")
        }
    }

    #[test]
    #[ignore = \"slow\"]
    fn ignored() {
        let _ = bump(&Counter(1));
    }
}
";

/// The source key both units share: the library unit's crate key, which holds no unit kind.
const SOURCE_NAME: &[u8] = b"suite";

fn source() -> ContentId {
    ContentId::of("blaze program crate", SOURCE_NAME)
}

fn toolchain() -> Toolchain {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"))
}

/// The crate `suite` with `text` as its `src/lib.rs`, solved as its library unit, or (`test`)
/// as its test unit: `test` added to its cfg and the unit's own crate key, as `blaze test` keys
/// a test unit apart today (blaze-scope units.rs `unit_key`).
fn with_unit<R>(tc: &Toolchain, text: &str, test: bool, f: impl FnOnce(&Names<'_>, &[(&str, Option<&str>)], &[SourceFile<'_>]) -> R) -> R {
    let files = [("/suite/src/lib.rs".to_string(), text.to_string())];
    let edition = edition::PackageEdition::of_manifest("2024").expect("a known edition");
    let user = UserCrate { name: "suite", dir: "/suite/", root: "/suite/src/lib.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let mut crates = inputs.crates();
    let cfg: Vec<(&str, Option<&str>)> = crates[0].cfg.iter().copied().chain(test.then_some(("test", None))).collect();
    crates[0].cfg = &cfg;
    if test {
        crates[0].key = CrateItemKey(ContentId::of("blaze program crate test", SOURCE_NAME));
    }
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/lib.rs", text }];
    f(&names, &cfg, &sources)
}

/// The free item of crate 0 named `name`.
fn item(names: &Names<'_>, name: &str) -> blaze_id::ItemKey {
    names.items(0).find(|(k, _, _)| names.item_text(*k).and_then(|t| t.name) == Some(name)).map(|(k, _, _)| k).unwrap_or_else(|| panic!("no item `{name}` in crate 0"))
}

/// One row as printed and compared: its path, its entry's shape (with the report fn's path),
/// its `#[ignore]` and its `#[should_panic]`.
#[derive(Debug, PartialEq, Eq)]
struct Shown {
    path: String,
    entry: String,
    ignore: Option<String>,
    should_panic: String,
}

fn show(suite: &Suite<'_, '_, '_>) -> Vec<Shown> {
    suite
        .rows
        .iter()
        .map(|r| Shown {
            path: r.path.to_string(),
            entry: match &r.entry {
                Ok(SuiteEntry::Direct { .. }) => "direct".to_string(),
                Ok(SuiteEntry::Report { report, .. }) => format!("report({})", suite.program.function(*report).path),
                Err(e) => format!("refused({e:?})"),
            },
            ignore: r.ignore.map(str::to_string),
            should_panic: match r.should_panic {
                ShouldPanic::No => "no".to_string(),
                ShouldPanic::Yes => "yes".to_string(),
                ShouldPanic::WithMessage(m) => format!("with {m:?}"),
            },
        })
        .collect()
}

/// The suite of `text`'s test unit, made in `f`'s hands.
fn with_suite<R>(tc: &Toolchain, text: &str, f: impl FnOnce(&Names<'_>, &Suite<'_, '_, '_>) -> R) -> R {
    with_unit(tc, text, true, |names, _cfg, sources| {
        let g = Alloc::global();
        let bytes = encode(names.krate(0), g).unwrap_or_else(|e| panic!("the tests record: {e}")).bytes;
        let record = TestsRecord::open(aligned(&bytes, blaze_own::testing::bump())).unwrap_or_else(|e| panic!("the tests record does not open: {e}"));
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command's frame");
        let mut left: Vec<(Box<str>, blaze_ladder::front2::program::LeftOut)> = Vec::new();
        let suite = Front2Program::suite(names, &frame, sources, &record, Some(&mut left), g).unwrap_or_else(|e| panic!("the suite: {e}"));
        for (what, why) in &left {
            println!("SUITE left out {what}: {why}");
        }
        f(names, &suite)
    })
}

/// PART A (piece 8). The suite of the test unit is ONE program from three entries: the rows in
/// path order; `tests::adds` (`-> Result<(), String>`) entered through the report shim, whose
/// `report` fn is `<Result<T, E> as Termination>::report`; `tests::ignored` and `tests::panics`
/// entered directly; `#[ignore = "slow"]` and `#[should_panic(expected = "boom")]` carried as
/// data; and `add`, called by `helper` (from `adds`) and by `panics`, tabled ONCE.
///
/// Expected lines (the report fn's path as the table prints it, K the table's size):
/// `SUITE tests::adds entry=report(..::report) ignore=None should_panic=no`
/// `SUITE tests::ignored entry=direct ignore=Some("slow") should_panic=no`
/// `SUITE tests::panics entry=direct ignore=None should_panic=with "boom"`
/// `SUITE add tabled 1 time(s) of K fns`
/// and `test suite_is_one_program_with_three_entries ... ok`.
#[test]
fn suite_is_one_program_with_three_entries() {
    let tc = toolchain();
    with_suite(&tc, LIB, |names, suite| {
        let shown = show(suite);
        for s in &shown {
            println!("SUITE {} entry={} ignore={:?} should_panic={}", s.path, s.entry, s.ignore, s.should_panic);
        }
        let paths: Vec<&str> = shown.iter().map(|s| s.path.as_str()).collect();
        assert_eq!(paths, ["tests::adds", "tests::ignored", "tests::panics"], "{shown:?}");
        assert!(shown[0].entry.starts_with("report(") && shown[0].entry.ends_with("report)"), "adds: {:?}", shown[0]);
        assert_eq!((shown[1].entry.as_str(), shown[1].ignore.as_deref(), shown[1].should_panic.as_str()), ("direct", Some("slow"), "no"), "ignored: {:?}", shown[1]);
        assert_eq!((shown[2].entry.as_str(), shown[2].ignore.as_deref(), shown[2].should_panic.as_str()), ("direct", None, "with \"boom\""), "panics: {:?}", shown[2]);
        match suite.rows[0].entry {
            Ok(SuiteEntry::Report { test, .. }) => assert_eq!(suite.program.function(test).key, suite.rows[0].test, "the shim's test fn is the row's"),
            ref other => panic!("adds: {other:?}"),
        }
        let add = checker_of(item(names, "add"));
        let tabled = (0..suite.program.fn_count()).filter(|&i| suite.program.function(i).key == add).count();
        println!("SUITE add tabled {tabled} time(s) of {} fns", suite.program.fn_count());
        assert_eq!(tabled, 1, "the shared callee `add` is tabled once");
        assert!(suite.program.entry().is_none(), "a suite has no one entry: each row has its own");
    });
}

/// What a body's check came to, as a record keeps it: its diagnostics' codes, sorted, and
/// whether it is clean.
type Verdict = (Vec<&'static str>, bool);

fn verdict_of(v: &BodyVerdict<'_>) -> Verdict {
    let mut codes: Vec<&'static str> = v.diagnostics.iter().map(|d| d.code).collect();
    codes.sort_unstable();
    (codes, v.is_clean())
}

/// One body checked in one unit, in shared form.
struct Body {
    name: &'static str,
    checker: u128,
    key: ContentId,
    reads: SharedReads<'static>,
    /// THE PLANTED BUG: the same list with the `test` atom dropped.
    planted: SharedReads<'static>,
    verdict: Verdict,
}

const ITEMS: [&str; 3] = ["add", "width", "bump"];

fn check_items(names: &Names<'_>, cfg: &[(&str, Option<&str>)], sources: &[SourceFile<'_>]) -> Vec<Body> {
    let g = Alloc::global();
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command's frame");
    let mut world = ProjectWorld::new(names, &frame, sources, g).unwrap_or_else(|e| panic!("front2: {e}"));
    let bodies = world.bodies(g);
    let base = UnitBase { source: source(), relative: ByPlace::of_names(names, 0, g), cfg, edition: Edition::E2024 };
    let test_atom = atom_digest("test", None);
    let mut out = Vec::new();
    for name in ITEMS {
        let (body, lib) = bodies
            .iter()
            .find_map(|(b, _)| match b {
                BodyRef::Free(k) if names.item_text(*k).and_then(|t| t.name) == Some(name) => Some((*b, *k)),
                _ => None,
            })
            .unwrap_or_else(|| panic!("`{name}` is not listed"));
        let text = names.item_text(lib).expect("its text").text;
        let verdict = world.check(body, g);
        let facts = verdict.facts.as_ref().unwrap_or_else(|e| panic!("`{name}`'s read list: {e:?}"));
        let cfg_read = cfg_reads(text, Edition::E2024, cfg, g).unwrap_or_else(|e| panic!("`{name}`'s cfg reads: {e:?}"));
        let mut kept = AVec::new_in(g);
        for c in cfg_read.iter().filter(|c| c.atom != test_atom) {
            kept.push(*c);
        }
        let reads = relative_reads(&base, cfg_read, &facts.rows, g);
        let planted = relative_reads(&base, kept, &facts.rows, g);
        let checker = checker_of(lib).0;
        out.push(Body { name, checker, key: shared_key(&base, checker, text), reads, planted, verdict: verdict_of(&verdict) });
    }
    out
}

/// PART B (piece 18). The library unit's bodies are filed as shared records (key: the source, the
/// item's place and its text; reads: the cfg atoms its text evaluates and its facts, keys made
/// relative); the test unit looks each up by the same key and takes it only when every read
/// answers alike.
///
/// Expected: found N = 1, recomputed M = 2.
/// - `add` is FOUND: it reads no cfg and resolves alike (its facts name only core's `u32` and
///   `Add`, and its own head, whose relative key is the same in both units).
/// - `width` is RECOMPUTED, `CfgMoved` on the atom `test`: its text evaluates `test`, which
///   holds in the test unit only (and there its body is E0308).
/// - `bump` is RECOMPUTED, `FactMoved`: its text reads no cfg, but its method question on
///   `Counter` answers differently (the `#[cfg(test)]` inherent `step`).
/// The oracle: every FOUND record's verdict equals today's fresh one. THE PLANTED BUG (the `test`
/// atom dropped from every cfg read list) makes `width` FOUND with the library's verdict (clean)
/// while today's is E0308: the oracle's failure is asserted, so the bug is caught.
///
/// Also the finding's evidence, asserted: `add`'s checker key differs between the units in its
/// prefix AND its low 64 bits (blaze-scope task/lower.rs `key_of` hashes the parent's whole key).
/// When that assertion fails, `key_of` has been fixed: switch `UnitBase` to `PrefixSwap`.
///
/// Expected lines:
/// `SHARED add: lib key 0x.., test key 0x.. (prefix and low bits differ)`
/// `SHARED add: found`
/// `SHARED width: recomputed (cfg atom test moved)`
/// `SHARED bump: recomputed (fact moved: Fact { .. })`
/// `SHARED found 1, recomputed 2`
/// `SHARED planted bug (test atom dropped): width found with ([], true), today ([\"E0308\"], false): caught`
/// and `test lib_and_test_units_share_what_reads_no_test ... ok`.
#[test]
fn lib_and_test_units_share_what_reads_no_test() {
    let tc = toolchain();
    let lib = with_unit(&tc, LIB, false, check_items);
    let test = with_unit(&tc, LIB, true, check_items);
    let g = Alloc::global();
    let mut records: SharedRecords<'static, Verdict> = SharedRecords::new_in(g);
    let mut planted: SharedRecords<'static, Verdict> = SharedRecords::new_in(g);
    let add_lib = lib.iter().find(|b| b.name == "add").expect("add").checker;
    for b in lib {
        planted.file(b.key, b.planted, b.verdict.clone());
        records.file(b.key, b.reads, b.verdict);
    }
    let add_test = test.iter().find(|b| b.name == "add").expect("add").checker;
    println!("SHARED add: lib key {add_lib:#034x}, test key {add_test:#034x} (prefix and low bits differ)");
    assert!(add_lib >> 64 != add_test >> 64, "the units' crate prefixes are equal");
    assert!(
        add_lib as u64 != add_test as u64,
        "`add`'s low 64 bits are equal in both units: blaze-scope `key_of` no longer hashes the parent's prefix; switch UnitBase to PrefixSwap"
    );

    let test_atom = atom_digest("test", None);
    let mut found = Vec::new();
    let mut recomputed = Vec::new();
    for b in &test {
        match records.look(b.key, &b.reads) {
            Look::Found(filed) => {
                println!("SHARED {}: found", b.name);
                assert_eq!(filed, &b.verdict, "`{}` was found with a verdict today's check does not give", b.name);
                found.push(b.name);
            }
            Look::NoRecord => panic!("`{}`: no record under its shared key: the key holds something of the unit", b.name),
            Look::CfgMoved { atom } => {
                let shown = if atom == test_atom { "test" } else { "another" };
                println!("SHARED {}: recomputed (cfg atom {shown} moved)", b.name);
                recomputed.push((b.name, "cfg"));
            }
            Look::FactMoved { fact } => {
                println!("SHARED {}: recomputed (fact moved: {fact:?})", b.name);
                recomputed.push((b.name, "fact"));
            }
        }
    }
    println!("SHARED found {}, recomputed {}", found.len(), recomputed.len());
    assert_eq!(found, ["add"], "found");
    assert_eq!(recomputed, [("width", "cfg"), ("bump", "fact")], "recomputed");

    let width = test.iter().find(|b| b.name == "width").expect("width");
    match planted.look(width.key, &width.planted) {
        Look::Found(filed) => {
            let caught = filed != &width.verdict;
            println!("SHARED planted bug (test atom dropped): width found with {filed:?}, today {:?}: {}", width.verdict, if caught { "caught" } else { "NOT caught" });
            assert!(caught, "the planted bug gave the library's verdict and today's alike: the fixture no longer tells the units apart");
        }
        other => panic!("with the `test` atom dropped, `width` should be wrongly found; it was {other:?}"),
    }
}

/// The own crate's names in a reach (last path segment, sorted) and how many fns outside it.
fn own_part(suite: &Suite<'_, '_, '_>, own: u64, reach: &Reach<'_>) -> (Vec<String>, usize) {
    let mut out = Vec::new();
    let mut outside = 0usize;
    for &k in reach.fns.iter() {
        if (k.0 >> 64) as u64 != own {
            outside += 1;
            continue;
        }
        let path = suite.program.fn_index(k).map_or("?", |i| suite.program.function(i).path);
        out.push(path.rsplit("::").next().unwrap_or(path).to_string());
    }
    out.sort();
    (out, outside)
}

/// Each row's reach (own names, the count outside the crate) and digest.
fn reaches(tc: &Toolchain, text: &str) -> Vec<(String, Vec<String>, usize, ContentId)> {
    with_suite(tc, text, |names, suite| {
        let own = (checker_of(names.root(0)).0 >> 64) as u64;
        let mut out = Vec::new();
        for (i, r) in suite.rows.iter().enumerate() {
            let reach = suite.reach_of(i, Alloc::global()).unwrap_or_else(|| panic!("row {i}'s reach"));
            let again = suite.reach_of(i, Alloc::global()).expect("again");
            assert_eq!(reach, again, "{}'s reach is not deterministic", r.path);
            let (names_, outside) = own_part(suite, own, &reach);
            out.push((r.path.to_string(), names_, outside, reach.digest));
        }
        out
    })
}

/// PART C (piece 13). Each row's reach: the sorted keys its entry reaches and their digest. The
/// own crate's part: `tests::adds` reaches `adds`, `helper`, `add` (and not `bump`; its report fn
/// is std's); `tests::panics` exactly `add` and `panics`; `tests::ignored` reaches `ignored`,
/// `bump` and the `#[cfg(test)]` inherent `step`, and not `add`. Each reach is the same twice.
/// Then `bump` is edited (`c.step() + c.0` to `c.0 + c.step()`): ONLY `tests::ignored`'s digest
/// moves.
///
/// Expected lines (N, the count of fns outside the crate, from the library's and the walk's
/// ambient part):
/// `REACH tests::adds own ["add", "adds", "helper"] +N outside`
/// `REACH tests::ignored own [.., "bump", "ignored", "step"] +N outside`
/// `REACH tests::panics own ["add", "panics"] +N outside`
/// `REACH edit bump: tests::adds same, tests::ignored moved, tests::panics same`
/// and `test reach_digest_moves_only_for_the_test_that_reaches_the_edit ... ok`.
#[test]
fn reach_digest_moves_only_for_the_test_that_reaches_the_edit() {
    let tc = toolchain();
    let before = reaches(&tc, LIB);
    for (path, own, outside, _) in &before {
        println!("REACH {path} own {own:?} +{outside} outside");
    }
    let own_of = |p: &str| -> &Vec<String> { &before.iter().find(|r| r.0 == p).unwrap_or_else(|| panic!("no row {p}")).1 };
    let has = |p: &str, n: &str| own_of(p).iter().any(|x| x == n);
    for n in ["adds", "helper", "add"] {
        assert!(has("tests::adds", n), "tests::adds reaches `{n}`: {:?}", own_of("tests::adds"));
    }
    assert!(!has("tests::adds", "bump"), "tests::adds does not reach `bump`: {:?}", own_of("tests::adds"));
    assert_eq!(own_of("tests::panics"), &["add".to_string(), "panics".to_string()], "tests::panics");
    for n in ["ignored", "bump", "step"] {
        assert!(has("tests::ignored", n), "tests::ignored reaches `{n}`: {:?}", own_of("tests::ignored"));
    }
    assert!(!has("tests::ignored", "add"), "tests::ignored does not reach `add`: {:?}", own_of("tests::ignored"));

    let edited = LIB.replace("c.step() + c.0", "c.0 + c.step()");
    assert_ne!(edited, LIB, "the edit applies");
    let after = reaches(&tc, &edited);
    let mut shown = Vec::new();
    for ((path, _, _, d0), (path1, _, _, d1)) in before.iter().zip(after.iter()) {
        assert_eq!(path, path1, "the rows are the same");
        shown.push(format!("{path} {}", if d0 == d1 { "same" } else { "moved" }));
    }
    println!("REACH edit bump: {}", shown.join(", "));
    assert_eq!(shown, ["tests::adds same", "tests::ignored moved", "tests::panics same"]);
}
