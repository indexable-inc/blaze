//! The names and items records filed in a crate's `CrateScope` record (drive.md, "End state",
//! piece 3, NAMES AND ITEMS FILED). A child of main.rs's tests: its helpers are theirs.

use blaze_scope::items_record::{LoadItems, RecordItems};
use blaze_scope::names::{Written, path};
use blaze_scope::names_record::{LoadNames, RecordNames, Verified};
use blaze_scope::resolve::aligned;
use blaze_surface::{DefKind, ItemKind, ItemKey, Namespace, PathKind};

use super::*;
use crate::names::sections::{HEAD, Section, SectionsError, WATCHED, sections};

/// The one crate's names record, 16-aligned.
struct OneNames(&'static [u8]);

impl LoadNames<'static> for OneNames {
    fn load(&self, krate: usize) -> Option<&'static [u8]> {
        (krate == 0).then_some(self.0)
    }
}

/// The one crate's items record, 16-aligned; no file's text is asked for (`identity` reads the row
/// only).
struct OneItems(&'static [u8]);

impl LoadItems<'static> for OneItems {
    fn load(&self, krate: usize) -> Option<&'static [u8]> {
        (krate == 0).then_some(self.0)
    }

    fn file(&self, _krate: usize, _path: blaze_scope::package_path::PackagePath<'_>) -> Option<&'static str> {
        None
    }

    fn expansion(&self, _krate: usize, _call: ItemKey) -> Option<&'static str> {
        None
    }
}

/// A virtual workspace of two members: `filed`, whose lib is `pub mod m { pub fn f() {} }`, and
/// `reader`, which depends on it by path and calls `filed::m::f`. The check resolves `filed`'s
/// names first and `reader`'s from `filed`'s record as the store hands it back: that payload,
/// kept by the names step's test watch, is what this test reads.
///
/// 1. `sections` of it gives five sections (the tests section empty: a plain library); the names section, through `RecordNames`, answers
///    the path `crate::m::f` in the value namespace from `filed`'s root (two segments
///    consumed: the Rust Reference, "Paths", `crate` names the crate root), and `crate::m` in
///    the type namespace as a module;
/// 2. the items section, through `RecordItems`, gives the row of the item that path named: a
///    function named `f` whose module is `m`;
/// 3. the payload cut short at every section boundary (the head's end, each section's start and
///    end) is a typed refusal: `Head` inside the head, `Short` after it; one byte more is
///    `Trailing`; another form's number is `Form`.
///
/// A payload without the two records (form 1: exports then inner) fails 1; a reader that trusts
/// the head's lengths fails 3.
///
/// Expected, from `LANECHECK_WAIT=540 bash /Volumes/Projects/blaze-int-1112-notes/lanecheck.sh
/// blaze-cli test names_records_tests`:
/// `test tests::names_records_tests::a_crate_scope_record_files_its_names_and_items ... ok`.
#[test]
fn a_crate_scope_record_files_its_names_and_items() {
    {
        let mut watched = WATCHED.lock();
        watched.krate = "filed";
        watched.payload.clear();
    }
    let io = Io::simulated(SimWorld::new(13, Limits::USUAL));
    write(&io, "Cargo.toml", "[workspace]\nmembers = [\"filed\", \"reader\"]\nresolver = \"2\"\n");
    write(&io, "filed/Cargo.toml", "[package]\nname = \"filed\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    write(&io, "filed/src/lib.rs", "pub mod m { pub fn f() {} }\n");
    write(&io, "reader/Cargo.toml", "[package]\nname = \"reader\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nfiled = { path = \"../filed\" }\n");
    write(&io, "reader/src/lib.rs", "pub fn h() {\n    filed::m::f()\n}\n");
    let checked = check(&io);
    let out = stdout(&io);
    let payload: std::vec::Vec<u8> = {
        let mut watched = WATCHED.lock();
        watched.krate = "";
        let kept = watched.payload.to_vec();
        watched.payload.clear();
        kept
    };
    assert_eq!(checked.failures, 0, "{out}\n{}", checked.profile);
    assert!(!payload.is_empty(), "`reader`'s names step read no record of `filed`:\n{out}\n{}", checked.profile);

    // 1. The names section answers `crate::m::f` and `crate::m`.
    let parts = sections(&payload).unwrap_or_else(|e| panic!("`filed`'s payload: {e}"));
    assert!(!parts.exports.is_empty() && !parts.names.is_empty() && !parts.items.is_empty(), "an empty section: {parts:?}");
    assert!(parts.tests.is_empty(), "`filed`'s plain library is not compiled with `test`, so it has no tests record: {parts:?}");
    let g = blaze_own::Alloc::global();
    let b = blaze_own::testing::bump();
    let names_bytes = aligned(parts.names, b);
    let items_bytes = aligned(parts.items, b);
    let key = Verified::check(names_bytes).unwrap_or_else(|e| panic!("the names section does not check: {e:?}")).key();
    let world = RecordNames::new(&[key], OneNames(names_bytes), g);
    let root = world.record(0).unwrap_or_else(|| panic!("no names record of `filed`")).root_module();
    let (m_f, just_m): ([&'static str; 2], [&'static str; 1]) = (["m", "f"], ["m"]);
    let f = path(&world, root, Namespace::Values, Written { kind: PathKind::Crate, segments: &m_f[..] }, g)
        .unwrap_or_else(|e| panic!("`crate::m::f` through the names section: {e:?}"));
    assert_eq!(f.consumed, 2, "`crate::m::f` resolved only in part: {f:?}");
    let m = path(&world, root, Namespace::Types, Written { kind: PathKind::Crate, segments: &just_m[..] }, g)
        .unwrap_or_else(|e| panic!("`crate::m` through the names section: {e:?}"));
    assert_eq!((m.consumed, m.def.kind), (1, DefKind::Module), "`crate::m`: {m:?}");

    // 2. The items section gives `f`'s row.
    let items = RecordItems::new(&[key], OneItems(items_bytes), b);
    let row = items
        .identity(f.def.key)
        .unwrap_or_else(|e| panic!("`f`'s row through the items section: {e:?}"))
        .unwrap_or_else(|| panic!("the items section has no row for `f` ({:?})", f.def.key));
    assert_eq!((row.name, row.kind, row.module), (Some("f"), ItemKind::Function, m.def.key), "`f`'s row: {row:?}");

    // 3. Cut short at every section boundary: a typed refusal.
    let start = payload.as_ptr().addr();
    let mut cuts = std::vec::Vec::from([0, HEAD - 1, HEAD]);
    for part in [parts.exports, parts.inner, parts.names, parts.items, parts.tests, parts.expansions, parts.links, parts.cells] {
        let at = part.as_ptr().addr() - start;
        cuts.extend([at, at + part.len()]);
    }
    for cut in cuts.into_iter().filter(|cut| *cut < payload.len()) {
        match sections(&payload[..cut]) {
            Err(SectionsError::Head { len }) => assert!(cut < HEAD && len == cut, "cut at {cut}: refused as a short head of {len}"),
            Err(SectionsError::Short { section, len, .. }) => assert!(cut >= HEAD && len == cut, "cut at {cut}: {section:?} short at {len}"),
            other => panic!("a payload cut at byte {cut} of {} was not refused as short: {other:?}", payload.len()),
        }
    }
    // The last section is cut: the cell table, never empty in a resolved crate's record (its
    // root's cell at least: blaze-scope names_cells.rs).
    assert!(
        matches!(sections(&payload[..payload.len() - 1]), Err(SectionsError::Short { section: Section::Cells, .. })),
        "one byte off the end is the cell table cut short"
    );
    let mut longer = payload.clone();
    longer.push(0);
    assert!(matches!(sections(&longer), Err(SectionsError::Trailing { .. })), "a byte past the items is refused");
    let mut older = payload.clone();
    older[4] = 1;
    assert!(matches!(sections(&older), Err(SectionsError::Form { form: 1, .. })), "another form's number is refused");
}

/// The tests record filed as the fifth section of a TEST unit's `CrateScope` payload (test.md,
/// "The tests record", piece 6). A one-member workspace whose library holds a `#[cfg(test)]`
/// module of two tests, one `#[ignore = "slow"]`, and one `#[should_panic(expected = "boom")]`
/// at the root; the check makes the library's plain unit and its test unit (the unit graph's
/// `Test { bin: None }`), and the names step writes the test unit's payload, kept by its watch.
///
/// The Reference, "Testing attributes": tests are "only compiled when in test mode"; `ignore`
/// "may optionally be written with the MetaNameValueStr syntax to specify a reason"; the
/// `should_panic` "input string ... must appear within the panic message". So: the payload's
/// `sections` give a non-empty tests section; `TestsRecord::open` of it lists exactly the three
/// tests in path order (`boom`, `tests::fast`, `tests::slow`) with their flags, and not the
/// plain `helper`; the payload, written again from its own five sections, is the same bytes
/// (the round trip); and the plain unit's payload (the first test) has an empty tests section.
///
/// A names step that drops the tests record (form 2's four sections) fails at "no payload with
/// a tests section"; a tests section put in the items' place fails `TestsRecord::open`.
///
/// Expected, from `LANECHECK_WAIT=540 bash /Volumes/Projects/blaze-int-1112-notes/lanecheck.sh
/// blaze-cli test names_records_tests`:
/// `test tests::names_records_tests::a_test_units_payload_files_its_tests_record ... ok`.
#[test]
fn a_test_units_payload_files_its_tests_record() {
    use blaze_scope::tests_record::{ShouldPanic, TestsRecord};

    use crate::names::sections::{WATCHED_TESTS, write as write_payload};

    {
        let mut watched = WATCHED_TESTS.lock();
        watched.krate = "tested";
        watched.payload.clear();
    }
    let io = Io::simulated(SimWorld::new(17, Limits::USUAL));
    write(&io, "Cargo.toml", "[workspace]\nmembers = [\"tested\"]\nresolver = \"2\"\n");
    write(&io, "tested/Cargo.toml", "[package]\nname = \"tested\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    write(
        &io,
        "tested/src/lib.rs",
        "pub fn helper() {}\n\n#[test]\n#[should_panic(expected = \"boom\")]\nfn boom() {\n    panic!(\"boom\")\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn fast() {}\n\n    #[test]\n    #[ignore = \"slow\"]\n    fn slow() {}\n}\n",
    );
    let checked = check(&io);
    let out = stdout(&io);
    let payload: std::vec::Vec<u8> = {
        let mut watched = WATCHED_TESTS.lock();
        watched.krate = "";
        let kept = watched.payload.to_vec();
        watched.payload.clear();
        kept
    };
    assert_eq!(checked.failures, 0, "{out}\n{}", checked.profile);
    assert!(!payload.is_empty(), "no payload with a tests section was written for `tested`'s test unit:\n{out}\n{}", checked.profile);

    let parts = sections(&payload).unwrap_or_else(|e| panic!("the test unit's payload: {e}"));
    assert!(!parts.tests.is_empty(), "the test unit's tests section is empty: {parts:?}");
    let record = TestsRecord::open(aligned(parts.tests, blaze_own::testing::bump())).unwrap_or_else(|e| panic!("the tests section does not open: {e}"));
    let mut rows = std::vec::Vec::new();
    for at in 0..record.len() {
        let row = record.row(at).unwrap_or_else(|e| panic!("row {at}: {e}")).unwrap_or_else(|| panic!("no row {at} below {}", record.len()));
        rows.push((row.path, row.ignore, row.should_panic, row.bench));
    }
    assert_eq!(
        rows,
        [
            ("boom", None, ShouldPanic::WithMessage("boom"), false),
            ("tests::fast", None, ShouldPanic::No, false),
            ("tests::slow", Some("slow"), ShouldPanic::No, false),
        ],
        "the test unit's tests, in path order"
    );
    assert!(record.find("helper").unwrap_or_else(|e| panic!("find: {e}")).is_none(), "`helper` is no test");

    // The round trip: the payload written again from its own sections is the same bytes.
    let again = write_payload(parts.unresolved, [parts.exports, parts.inner, parts.names, parts.items, parts.tests, parts.expansions, parts.links, parts.cells]);
    assert_eq!(again.as_slice(), payload.as_slice(), "the payload written from its eight sections differs from the one filed");
}
