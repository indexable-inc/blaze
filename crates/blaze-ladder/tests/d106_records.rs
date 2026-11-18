//! D106 and D124 piece M: `blaze run` on records, through the real maker over NAMES RECORDS
//! (crates/blaze-ladder/src/run/make_one.rs `Maker`, the lazy program over front2 source.rs
//! `Records`) and the record walk (body_ids.rs `Walk`), on a real program (rule 8, each claim
//! with its control).
//!
//! The program is code_records.rs's `PROGRAM` (one `#![no_core]` crate). Six instances: `main`,
//! `hello`, `beta`, `alpha`, and `pick` at `i32` and at `u8`; five fns with bodies; `write` is a
//! C name declared by an `extern "C"` block, no instance.
//!
//! `main` is `extern "C-unwind"`, not code_records.rs's `extern "C"`: built under `panic =
//! unwind`, a "C" body is of the Reference's non-unwinding category, so each call in it lands on
//! a terminate block calling `#[lang = "panic_cannot_unwind"]` (blaze-build walk/nounwind.rs, as
//! rustc's), which this `#![no_core]` program does not declare. "C-unwind" is the same calling
//! convention and unwinds, so the program needs no lang item beyond the two it declares.
//!
//! THE NAMES RECORDS ([`fixture`]): the program is solved ONCE in the fixture, its names record
//! and items record written from that solve (`names_record::encode`, `items_record::
//! encode_with`, as tests/front2_records.rs writes them), and the solved names DROPPED. Every
//! walk then reads a FRESH `Records` over those bytes ([`reader`]): a new process of `blaze run`
//! that solves nothing. No `resolve_crates` runs under the maker.
//!
//! THE COMMAND'S STAND-IN ([`TextIds`]): the command's BodyCheck keys come from the drive; here
//! each fn's key is a digest of its item's TEXT, read from the items record (its own reader, not
//! the maker's: its reads are not counted). Every crate fact constant. So a body edit moves
//! exactly that fn's key, as the command's does.

use std::collections::HashMap;

use blaze_emit::{Isa, Os};
use blaze_id::{CrateItemKey, ItemKey as LibKey};
use blaze_ids::producer::{CodeBuilder, ProducerId, Producers};
use blaze_ids::{ContentId, Hasher};
use blaze_ladder::body_ids::{BodyId, BodyIds, Counts, Read};
use blaze_ladder::code_records::{Builder, CodeStore, MemoryStore, Settings, TierName};
use blaze_ladder::crates::{Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::keys::checker_of;
use blaze_ladder::front2::source::{Load, Log, NamesSource, Records};
use blaze_ladder::run::OverflowChecks;
use blaze_ladder::run::make_one::{CheckCount, Maker, NamesRecords, RecordsProgram, walk_records};
use blaze_ladder::seam::FrontEnd;
use blaze_typeck::ExprId;
use blaze_own::Alloc;
use blaze_scope::resolve::aligned;
use blaze_scope::{items_record, names_record};
use blaze_surface::ItemKind;
use blaze_toolchain::ToolchainId;
use blaze_typeck::ItemKey;

const PROGRAM: &str = "#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = \"sized\"]
trait Sized {}

#[lang = \"copy\"]
trait Copy {}

unsafe extern \"C\" {
    fn write(fd: i32, buf: *const u8, len: usize) -> isize;
}

fn hello() -> isize {
    unsafe { write(1, b\"hello\\n\" as *const u8, 6) }
}

fn alpha() -> i32 {
    1
}

fn beta() -> i32 {
    alpha()
}

fn pick<T>(c: bool, a: T, b: T) -> T {
    if c { a } else { b }
}

#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {
    hello();
    let x: i32 = pick(true, beta(), 2);
    let y: u8 = pick(false, 3, 4);
    x + y as i32
}
";

const DIR: &str = "/ladder/";
const ROOT: &str = "/ladder/src/main.rs";
const OVERFLOW: OverflowChecks = OverflowChecks::Off;
const REACHED: usize = 6;
const BODIES: usize = 5;
/// The crates that can declare a lang item: the program's own, the only one.
const LANG_CRATES: [usize; 1] = [0];

fn isa() -> Isa {
    Isa { lse: true, os: Os::MacOs }
}

fn builder() -> Builder {
    builder_with(blaze_build::Panic::Unwind)
}

/// As [`builder`], under the panic strategy `panic` (the fixture's crates are made with it too).
fn builder_with(panic: blaze_build::Panic) -> Builder {
    Builder {
        isa: isa(),
        tier: TierName::Reference,
        settings: Settings::baseline(OVERFLOW, panic),
        toolchain: ToolchainId([7; 32]),
        producer: producer(),
        edition: edition::PackageEdition::of_manifest("2024").expect("2024 is an edition"),
    }
}

fn producer() -> ProducerId<CodeBuilder> {
    Producers::for_tests(b"d106 make one").get::<CodeBuilder>()
}

fn alloc() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

fn leak(s: &str) -> &'static str {
    Box::leak(s.to_string().into_boxed_str())
}

/// The records and texts as kept after the solve: the test's effect layer (the command's is
/// blaze-cli front_load.rs `StoreLoad`).
#[derive(Clone, Default)]
struct Kept {
    names: Vec<&'static [u8]>,
    items: Vec<&'static [u8]>,
    files: Vec<HashMap<String, &'static str>>,
    expansions: Vec<HashMap<LibKey, &'static str>>,
    origins: Vec<HashMap<LibKey, &'static [u8]>>,
}

impl Load<'static> for Kept {
    fn names(&self, krate: usize) -> Option<&'static [u8]> {
        self.names.get(krate).copied()
    }

    fn items(&self, krate: usize) -> Option<&'static [u8]> {
        self.items.get(krate).copied()
    }

    fn file(&self, krate: usize, path: blaze_scope::package_path::PackagePath<'_>) -> Option<&'static str> {
        self.files.get(krate)?.get(path.as_str()).copied()
    }

    fn expansion(&self, krate: usize, call: LibKey) -> Option<&'static str> {
        self.expansions.get(krate)?.get(&call).copied()
    }

    fn expansion_origins(&self, krate: usize, call: LibKey) -> Option<&'static [u8]> {
        self.origins.get(krate)?.get(&call).copied()
    }
}

/// The origin column an expansion's text keeps, as a loader serves it (as tests/front2_records.rs
/// `made_origins`): empty for a text that keeps none by design.
fn made_origins(call: LibKey, origins: &Result<&[u8], blaze_scope::expansion_origins::OriginsError>) -> &'static [u8] {
    use blaze_scope::expansion_origins::OriginsError;
    match origins {
        Ok(bytes) => Box::leak(bytes.to_vec().into_boxed_slice()),
        Err(OriginsError::Rewritten { .. } | OriginsError::Unplaced { .. }) => &[],
        Err(e) => panic!("the origins of expansion {call:x}: {e:?}"),
    }
}

/// One program's names records: the solve's output, the solve itself gone.
struct Fixture {
    kept: Kept,
    keys: &'static [CrateItemKey],
    crate_names: &'static [&'static str],
}

/// `text` solved once, each crate's two records written from the solve; nothing of the solved
/// names leaves this function.
fn fixture(text: &str) -> Fixture {
    fixture_of(&[(String::from(ROOT), String::from(text))], blaze_build::Panic::Unwind)
}

/// As [`fixture`], over the crate's `files` (each by its path under [`DIR`], [`ROOT`] among
/// them), its crates made under `panic`.
fn fixture_of(files: &[(String, String)], panic: blaze_build::Panic) -> Fixture {
    let a = alloc();
    let edition = edition::PackageEdition::of_manifest("2024").expect("2024 is an edition");
    let user = UserCrate { name: "ladder", dir: DIR, root: ROOT, files, edition, panic };
    let tc = Toolchain::none(a);
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let keys: &'static [CrateItemKey] = Vec::leak(crates.iter().map(|c| c.key).collect());
    let crate_names: &'static [&'static str] = Vec::leak(crates.iter().map(|c| leak(c.name)).collect());
    let names = inputs.resolve(&crates, a).unwrap_or_else(|e| panic!("the fixture's one solve: {e}"));
    let mut kept = Kept::default();
    for c in names.crates().iter() {
        kept.names.push(aligned(&names_record::encode(c, a).unwrap_or_else(|e| panic!("the names record of `{}`: {e:?}", c.name)), a));
        kept.items.push(aligned(&items_record::encode_with(c, &names, a).unwrap_or_else(|e| panic!("the items record of `{}`: {e:?}", c.name)), a));
        kept.files.push(c.files.iter().map(|f| (f.package.as_str().to_string(), leak(f.text))).collect());
        kept.expansions.push(c.expansions.iter().filter_map(|(call, e)| e.made.map(|m| (*call, leak(m.text)))).collect());
        kept.origins.push(c.expansions.iter().filter_map(|(call, e)| e.made.as_ref().map(|m| (*call, made_origins(*call, &m.origins)))).collect());
    }
    Fixture { kept, keys, crate_names }
}

/// A FRESH reader of `f`'s records, as one `blaze run` process makes it: its frame, log, loader
/// and itself live for the test (leaked: a `Records` borrows itself for its whole life).
fn reader(f: &Fixture) -> NamesRecords<'static> {
    let a = alloc();
    let frame: &'static blaze_mem::Frame = Box::leak(Box::new(blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the reader's frame")));
    let log: &'static Log<'static> = Box::leak(Box::new(Log::new(f.crate_names, a)));
    let load: &'static Kept = Box::leak(Box::new(f.kept.clone()));
    let records: &'static Records<'static> = Box::leak(Box::new(Records::new(f.keys, load, log, frame, a)));
    NamesRecords { records, frame, lang_crates: &LANG_CRATES, body_sources: None }
}

/// What the records' log holds, as text.
fn logged(records: &Records<'_>) -> Vec<String> {
    records.errors().iter().map(|e| e.to_string()).collect()
}

const READS: [Read; 1] = [Read { fact: [3; 32], digest: [4; 32] }];

/// The command's stand-in (the module doc).
struct TextIds {
    rows: Vec<(ItemKey, ContentId)>,
    /// `false`: the control, the seam's answers dropped (no BodyCheck record for any fn).
    recorded: bool,
    entry: ItemKey,
}

impl BodyIds for TextIds {
    fn body_check(&self, item: ItemKey) -> Option<BodyId<'_>> {
        let n = self.rows.binary_search_by_key(&item, |r| r.0).ok()?;
        Some(BodyId { key: self.rows[n].1, reads: if self.recorded { &READS } else { &[] } })
    }

    fn today(&self, fact: &[u8; 32]) -> Option<[u8; 32]> {
        (*fact == READS[0].fact).then_some(READS[0].digest)
    }

    fn crate_fact(&self, _item: ItemKey) -> Option<Read> {
        Some(Read { fact: [8; 32], digest: [9; 32] })
    }
}

/// Each free fn's key from its item's text, read from the items record by a reader of its own;
/// the entry is the fn named `main`.
fn ids_of(f: &Fixture, recorded: bool) -> TextIds {
    let records = reader(f).records;
    let mut libs: Vec<LibKey> = Vec::new();
    assert!(records.each_item(0, &mut |key, _| libs.push(key)), "the program's items record lists its items");
    let mut rows = Vec::new();
    let mut entry = None;
    for &lib in libs.iter() {
        if records.kind_of_item(lib) != Some(ItemKind::Function) || records.member_owner(lib).is_some() {
            continue;
        }
        let text = records.text_of(lib).unwrap_or_else(|| panic!("fn {lib:x} has a text: {:?}", logged(records)));
        rows.push((checker_of(lib), Hasher::new("d106 test body check").part(text.text.as_bytes()).finish()));
        if records.name_of(lib) == Some("main") {
            entry = Some(checker_of(lib));
        }
    }
    assert!(logged(records).is_empty(), "the stand-in's reads: {:?}", logged(records));
    rows.sort_by_key(|r| r.0);
    TextIds { rows, recorded, entry: entry.expect("the program has a `main`") }
}

/// One walk of `f`'s program over `store`, with a fresh reader and a fresh `Maker`: its counts and
/// the maker's checks. The records' log must be empty after it.
fn walk(f: &Fixture, ids: &TextIds, store: &mut dyn CodeStore) -> (Counts, CheckCount) {
    let (counts, maker) = walk_keeping(f, ids, ids.entry, store, &builder());
    (counts, maker.checks())
}

/// One walk from `entry` with a fresh reader and a fresh `Maker` built for `b`: its counts and
/// the maker, its lazy program kept for the seam's questions after the walk. The records' log
/// must be empty after it.
fn walk_keeping(f: &Fixture, ids: &dyn BodyIds, entry: ItemKey, store: &mut dyn CodeStore, b: &Builder) -> (Counts, Maker<'static>) {
    let names = reader(f);
    let mut maker = Maker::new(names, Some(entry), b, alloc());
    let (_, counts) = walk_records(entry, ids, store, b, &producer(), &mut maker, alloc()).unwrap_or_else(|e| {
        // Printed too: the test's own output is what a failed run shows first, the records' log
        // with it (a gap the source filed is often the cause).
        println!("the record walk: {e}; the records filed {:?}", logged(names.records));
        panic!("the record walk: {e}")
    });
    assert!(logged(names.records).is_empty(), "the records filed {:?}", logged(names.records));
    (counts, maker)
}

/// The checker key of crate 0's fn named `name` (a free fn or a member), read by a reader of its
/// own.
fn fn_named(f: &Fixture, name: &str) -> ItemKey {
    let records = reader(f).records;
    let mut found = None;
    assert!(records.each_item(0, &mut |key, _| if found.is_none() && records.kind_of_item(key) == Some(ItemKind::Function) && records.name_of(key) == Some(name) {
        found = Some(checker_of(key));
    }), "the program's items record lists its items");
    found.unwrap_or_else(|| panic!("no fn named `{name}`: {:?}", logged(records)))
}

/// A stand-in for the command's BodyCheck keys that answers for EVERY item: its key a digest of
/// the item's key, so one run's instances are each keyed (the async program's impl methods and
/// coroutine owners among them) and two runs of one program agree.
struct EveryItem;

impl BodyIds for EveryItem {
    fn body_check(&self, item: ItemKey) -> Option<BodyId<'_>> {
        Some(BodyId { key: Hasher::new("d106 test every item").part(&item.0.to_le_bytes()[..]).finish(), reads: &READS })
    }

    fn today(&self, fact: &[u8; 32]) -> Option<[u8; 32]> {
        (*fact == READS[0].fact).then_some(READS[0].digest)
    }

    fn crate_fact(&self, _item: ItemKey) -> Option<Read> {
        Some(Read { fact: [8; 32], digest: [9; 32] })
    }
}

/// Run twice: the first run makes every instance over the names records (each body checked
/// once, one program constructed, none of its bodies checked by construction); the second finds
/// every record (found = reached), checks no body and constructs no program. Control: the seam's
/// answers dropped over the SAME full store: every instance is made unkeyed, and the bodies
/// checked are the reached bodies.
#[test]
fn a_second_run_over_names_records_finds_every_record_and_checks_no_body() {
    let f = fixture(PROGRAM);
    let mut store = MemoryStore::new_in(alloc());
    let ids = ids_of(&f, true);
    let (first, first_checks) = walk(&f, &ids, &mut store);
    assert_eq!((first.reached, first.found, first.made), (REACHED, 0, REACHED), "first run {first}");
    assert_eq!((first_checks.check_one, first_checks.by_construction, first_checks.constructed), (BODIES, 0, 1), "first run {first_checks:?}");
    let (second, second_checks) = walk(&f, &ids, &mut store);
    assert_eq!((second.reached, second.found, second.made, second.make_calls()), (REACHED, REACHED, 0, 0), "second run {second}");
    assert_eq!(second_checks, CheckCount::default(), "the second run checked or constructed");
    let control_ids = TextIds { recorded: false, ..ids_of(&f, true) };
    let (control, control_checks) = walk(&f, &control_ids, &mut store);
    assert_eq!((control.found, control.no_record), (0, REACHED), "control {control}");
    assert_eq!((control_checks.check_one, control_checks.constructed), (BODIES, 1), "control {control_checks:?}");
    println!(
        "d106 maker over names records twice: first {first}, check_one {}, programs {}; second {second}, check_one {}, programs {}; control {control}, check_one {}",
        first_checks.check_one, first_checks.constructed, second_checks.check_one, second_checks.constructed, control_checks.check_one
    );
}

/// One body's text edited between runs (`alpha`'s `1` to `5`), its records written again from
/// the edited text: the ONE instance of `alpha` is made, every other found; `check_one` is asked
/// of that one body, and constructing the lazy program checks none. Control: the same edit with
/// every record dropped (a fresh store) makes all six.
#[test]
fn an_edited_body_is_made_alone_over_names_records() {
    let edited = PROGRAM.replace("fn alpha() -> i32 {\n    1\n}", "fn alpha() -> i32 {\n    5\n}");
    assert_ne!(edited, PROGRAM, "the edit applies");
    let before = fixture(PROGRAM);
    let mut store = MemoryStore::new_in(alloc());
    walk(&before, &ids_of(&before, true), &mut store);
    let after_fixture = fixture(&edited);
    let ids = ids_of(&after_fixture, true);
    let (after, checks) = walk(&after_fixture, &ids, &mut store);
    assert_eq!((after.reached, after.found, after.made), (REACHED, REACHED - 1, 1), "after the edit {after}");
    assert_eq!((checks.check_one, checks.constructed, checks.by_construction), (1, 1, 0), "the edit's checks {checks:?}");
    let (control, _) = walk(&after_fixture, &ids, &mut MemoryStore::new_in(alloc()));
    assert_eq!((control.found, control.made), (0, REACHED), "control {control}");
    println!("d106 maker over names records edit: {after}, check_one {}, by construction {}; control {control}", checks.check_one, checks.by_construction);
}

/// tests/track_caller.rs's program: a tracked `here()` reads its caller's `Location`.
const TRACKED: &str = "#![feature(no_core, lang_items, intrinsics, rustc_attrs)]
#![no_core]
#![no_main]
#![allow(internal_features)]

#[lang = \"sized\"]
trait Sized {}

#[lang = \"copy\"]
trait Copy {}

impl Copy for u32 {}

#[lang = \"panic_location\"]
struct Location<'a> {
    file: &'a str,
    line: u32,
    col: u32,
}

#[rustc_intrinsic]
const fn caller_location() -> &'static Location<'static>;

#[track_caller]
fn here() -> u32 {
    caller_location().line
}

#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {
    here() as i32
}
";

/// What the lazy program over names records answers about `here()`'s call in `main`.
struct Tracked {
    /// `track_caller` of `here` and of `main`.
    here: Option<bool>,
    main: Option<bool>,
    /// The call's place as a code record keeps it: its byte in `main`'s text, and the file.
    site: u32,
    file: String,
    /// `source_position` of that place.
    position: (u32, u32),
}

/// `main` checked by the maker's lazy program over a fresh reader of `f`'s records (as
/// `Maker::make` constructs it), then the seam asked about the call of `here()`: of `main`'s
/// expressions with a caller site, the one placed at `want` (line, column). Every place the
/// seam gives is printed when none is.
fn tracked_call(f: &Fixture, want: (u32, u32)) -> Tracked {
    let names = reader(f);
    let here = fn_named(f, "here");
    let main = fn_named(f, "main");
    let mut program = RecordsProgram::from_records(names.records, names.frame, names.lang_crates, Some(main), alloc()).unwrap_or_else(|e| panic!("the lazy program over records: {e}"));
    program.check_one(main).unwrap_or_else(|e| panic!("`main`'s check: {e}")).expect("`main` is a fn of the program");
    let i = program.fn_index(main).expect("`main` has a row once checked");
    let exprs = program.function(i).body.expect("`main`'s body is live until `done`").0.exprs.len() as u32;
    let mut placed = Vec::new();
    for e in 0..exprs {
        if let Some(site) = program.caller_site(main, ExprId(e)) {
            placed.push((site.offset, site.file.to_string(), program.source_position(main, site.offset)));
        }
    }
    let (site, file, _) = placed.iter().find(|p| p.2 == Ok(want)).cloned().unwrap_or_else(|| panic!("no caller site of `main` is placed at {want:?}: {placed:?}"));
    let tracked = Tracked { here: program.track_caller(here), main: program.track_caller(main), site, file, position: want };
    assert!(logged(names.records).is_empty(), "the records filed {:?}", logged(names.records));
    tracked
}

/// The line and column (from 1, the column in chars) of the first `here()` in `text`.
fn call_place(text: &str) -> (u32, u32) {
    let (n, line) = text.lines().enumerate().find(|(_, l)| l.contains("here() as i32")).expect("the call");
    (n as u32 + 1, line.find("here()").expect("the call") as u32 + 1)
}

/// A `#[track_caller]` fn's caller observed over NAMES RECORDS (the lazy program `Maker` makes,
/// front2/program/seam_rules.rs, the rules `Front2Program` answers by): `here` is tracked and
/// `main` is not; `main`'s call of `here()` has a caller site in the file `src/main.rs`, placed
/// at the call's line and column. Two blank lines inserted above `main` (records written again
/// from the edited text): the call's site, what the code record keeps, is the SAME byte of
/// `main`'s text, and its position is two lines lower (rule 71: an edit above an item moves the
/// answer, no record). Control: the place asked at the old line in the edited program is not
/// found (the panic message lists the places), so the answer is read from today's text.
///
/// Not walked by the record walk: run.rs `FrontAdts::track_caller` holds every answer at `None`
/// until the code record carries a position field (its doc), so the build of `here()`'s
/// `caller_location` refuses on every front end (tests/track_caller.rs says the same).
///
/// Prints: `d106 track_caller over names records: here Some(true), main Some(false); call at byte <B> of main in <file>, line <N> col <C>, after the edit line <N+2> col <C>`
#[test]
fn a_tracked_callers_location_is_answered_over_names_records() {
    let want = call_place(TRACKED);
    let before = tracked_call(&fixture(TRACKED), want);
    assert_eq!((before.here, before.main), (Some(true), Some(false)), "`here` is tracked, `main` is not");
    assert!(before.file.ends_with("src/main.rs"), "the call's file: {}", before.file);
    let moved = TRACKED.replace("#[unsafe(no_mangle)]", "\n\n#[unsafe(no_mangle)]");
    let moved_want = call_place(&moved);
    assert_eq!(moved_want, (want.0 + 2, want.1), "the edit is two lines above the call");
    let moved_fixture = fixture(&moved);
    let after = tracked_call(&moved_fixture, moved_want);
    assert_eq!(after.site, before.site, "the call's byte in `main`'s text is what the record keeps: unmoved by an edit above `main`");
    let stale = std::panic::catch_unwind(|| tracked_call(&moved_fixture, want).position);
    assert!(stale.is_err(), "control: the old line is not where the edited program places the call");
    println!(
        "d106 track_caller over names records: here {:?}, main {:?}; call at byte {} of main in {}, line {} col {}, after the edit line {} col {}",
        before.here, before.main, before.site, before.file, before.position.0, before.position.1, after.position.0, after.position.1
    );
}

/// The async rung (crates/blaze-cli/tests/ladder/front2/x0-async-01: `async fn add_slowly`
/// awaiting a hand-written future twice, an executor written there) as the crate's files under
/// [`DIR`], its `main` driving `add_slowly(13, 10)` (23) where the rung drives `compute(13)`
/// (37): `compute` stays in the file, unreached (the control below). `compute` itself, an
/// `async fn` awaiting another, is walked by
/// [`an_async_fn_awaiting_another_is_walked_over_names_records`].
fn async_rung() -> Vec<(String, String)> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../blaze-cli/tests/ladder/front2/x0-async-01/src");
    ["main.rs", "future.rs", "prelude.rs"]
        .iter()
        .map(|f| {
            let text = std::fs::read_to_string(dir.join(f)).unwrap_or_else(|e| panic!("the async rung's {f}: {e}"));
            let text = match *f {
                "main.rs" => {
                    let drives = "block_on(compute(13), &mut polls, &mut wakes)";
                    assert!(text.contains(drives) && text.contains("got != 37"), "the rung's main drives `compute(13)` for 37");
                    text.replace(drives, "block_on(add_slowly(13, 10), &mut polls, &mut wakes)").replace("got != 37", "got != 23")
                }
                _ => text,
            };
            (format!("{DIR}src/{f}"), text)
        })
        .collect()
}

/// An async program WALKED by the record walk with `Maker` over names records: every instance
/// it reaches is made, `add_slowly`'s coroutine state laid out from the recipe the lazy program
/// made with its check (run/lay_states.rs asks `coroutine_recipe`; fill.rs `body_recipes`), and
/// after the walk the program answers that recipe. Controls: `main`, which holds no async
/// block, has none; `compute`, an async fn the walk never reaches, was never checked, so it has
/// none either (the lazy program checks what the walk reaches, nothing else). Built under
/// `panic = abort`, as the ladder's rungs are run (run.rs `build_survey`): its `extern "C" fn
/// main` then needs no `panic_cannot_unwind`.
///
/// Prints: `d106 async over names records: <counts>, check_one <K>; recipes: add_slowly yes, main none, compute (unreached) none`
#[test]
fn an_async_program_is_walked_over_names_records() {
    let f = fixture_of(&async_rung(), blaze_build::Panic::Abort);
    let entry = fn_named(&f, "main");
    let mut store = MemoryStore::new_in(alloc());
    let (counts, maker) = walk_keeping(&f, &EveryItem, entry, &mut store, &builder_with(blaze_build::Panic::Abort));
    assert!(counts.reached > 0 && counts.made == counts.reached && counts.found == 0, "every instance reached is made: {counts}");
    let program = maker.program().expect("a first run constructs the program");
    match program.coroutine_recipe(fn_named(&f, "add_slowly"), 0) {
        Some(Ok(_)) => {}
        Some(Err(refusal)) => panic!("`add_slowly`'s async body: its recipe was refused: {refusal:?}"),
        None => panic!("`add_slowly`'s async body: no recipe made with its check"),
    }
    assert!(program.coroutine_recipe(entry, 0).is_none(), "control: `main` has no async block");
    assert!(program.coroutine_recipe(fn_named(&f, "compute"), 0).is_none(), "control: `compute` is not reached, so never checked");
    println!("d106 async over names records: {counts}, check_one {}; recipes: add_slowly yes, main none, compute (unreached) none", maker.checks().check_one);
}

/// The async rung's library (its future.rs and prelude.rs, unchanged) under a `main.rs` of
/// [`ASYNC_HEAD`] (the rung's `Twice` and `block_on`, verbatim) followed by `items`, which hold
/// the program's `main`: the shapes a real async program has that `add_slowly` alone does not.
fn async_program(items: &str) -> Vec<(String, String)> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../blaze-cli/tests/ladder/front2/x0-async-01/src");
    let mut files: Vec<(String, String)> = ["future.rs", "prelude.rs"]
        .iter()
        .map(|f| (format!("{DIR}src/{f}"), std::fs::read_to_string(dir.join(f)).unwrap_or_else(|e| panic!("the async rung's {f}: {e}"))))
        .collect();
    files.push((String::from(ROOT), format!("{ASYNC_HEAD}{items}")));
    files
}

/// The head of [`async_program`]'s `main.rs`: the rung's crate attributes, `Twice` (pending
/// `left` times, a wake counted each time, then ready with `value`) and its executor `block_on`
/// (polls until ready, counting polls), as x0-async-01's main.rs writes them.
const ASYNC_HEAD: &str = "#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures, arbitrary_self_types)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod future;
mod prelude;
use future::*;
use prelude::*;

struct Twice {
    left: u32,
    value: u32,
}

impl Future for Twice {
    type Output = u32;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<u32> {
        let this = self.pointer;
        if this.left == 0 {
            Poll::Ready(this.value)
        } else {
            this.left = this.left - 1;
            *cx.wakes = *cx.wakes + 1;
            Poll::Pending
        }
    }
}

fn block_on<F: Future<Output = u32>>(fut: F, polls: &mut u32, wakes: &mut u32) -> u32 {
    let mut fut = fut;
    let mut cx = Context { wakes };
    loop {
        if *polls == 64 {
            return 1000;
        }
        *polls = *polls + 1;
        let pinned = Pin { pointer: &mut fut };
        match <F as Future>::poll(pinned, &mut cx) {
            Poll::Ready(v) => return v,
            Poll::Pending => {}
        }
    }
}
";

/// An `async fn` awaiting ANOTHER `async fn` (the rung's own `compute`, its future holding
/// `add_slowly`'s by value across its `.await`, `kept` live across it).
const AWAITS_ANOTHER: &str = "
async fn add_slowly(a: u32, b: u32) -> u32 {
    let x = Twice { left: 2, value: a }.await;
    let y = Twice { left: 2, value: b }.await;
    x + y
}

async fn compute(seed: u32) -> u32 {
    let kept = seed + 1;
    let sum = add_slowly(seed, 10).await;
    sum + kept
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let mut polls = 0u32;
    let mut wakes = 0u32;
    if block_on(compute(13), &mut polls, &mut wakes) == 37 { 42 } else { 1 }
}
";

/// An `async move` block capturing by move: a future (`Twice`, not `Copy`) and a `u32`, both
/// moved into the block's state, the future awaited there.
const BY_MOVE: &str = "
fn by_move(k: u32, polls: &mut u32, wakes: &mut u32) -> u32 {
    let pending = Twice { left: 2, value: k };
    let base = k + 1;
    block_on(async move { pending.await + base }, polls, wakes)
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let mut polls = 0u32;
    let mut wakes = 0u32;
    if by_move(13, &mut polls, &mut wakes) == 27 { 42 } else { 1 }
}
";

/// A `Poll::Pending` path woken ONCE: one pending poll (one wake), then ready.
const PENDING_ONCE: &str = "
async fn once(v: u32) -> u32 {
    Twice { left: 1, value: v }.await
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let mut polls = 0u32;
    let mut wakes = 0u32;
    let got = block_on(once(5), &mut polls, &mut wakes);
    if got == 5 && polls == 2 && wakes == 1 { 42 } else { 1 }
}
";

/// [`async_program`] of `items` walked by the record walk over names records (as
/// [`an_async_program_is_walked_over_names_records`]: `panic = abort`, every instance reached
/// made, none found); each of `async_owners` (by fn name, `main` among them for a block in
/// `main`) answers its first async body's recipe after the walk; a second walk over the same
/// store finds every instance and makes none. Control: `block_on`, reached and no async body,
/// has no recipe. Returns both walks' counts.
fn walk_async(items: &str, async_owners: &[&str]) -> (Counts, Counts) {
    let f = fixture_of(&async_program(items), blaze_build::Panic::Abort);
    let entry = fn_named(&f, "main");
    let b = builder_with(blaze_build::Panic::Abort);
    let mut store = MemoryStore::new_in(alloc());
    let (first, maker) = walk_keeping(&f, &EveryItem, entry, &mut store, &b);
    assert!(first.reached > 0 && first.made == first.reached && first.found == 0, "every instance reached is made: {first}");
    let program = maker.program().expect("a first run constructs the program");
    for &owner in async_owners {
        match program.coroutine_recipe(fn_named(&f, owner), 0) {
            Some(Ok(_)) => {}
            Some(Err(refusal)) => panic!("`{owner}`'s async body: its recipe was refused: {refusal:?}"),
            None => panic!("`{owner}`'s async body: no recipe made with its check"),
        }
    }
    assert!(program.coroutine_recipe(fn_named(&f, "block_on"), 0).is_none(), "control: `block_on` has no async body");
    let (again, _) = walk_keeping(&f, &EveryItem, entry, &mut store, &b);
    assert_eq!((again.reached, again.found, again.make_calls()), (first.reached, first.reached, 0), "a second walk finds every instance: {again}");
    (first, again)
}

/// An `async fn` awaiting another ([`AWAITS_ANOTHER`]): `compute`'s state holds `add_slowly`'s
/// future; both coroutines' instances are made over records.
///
/// Prints: `d106 async awaiting another over names records: first <counts>; again <counts>`
#[test]
fn an_async_fn_awaiting_another_is_walked_over_names_records() {
    let (first, again) = walk_async(AWAITS_ANOTHER, &["compute", "add_slowly"]);
    println!("d106 async awaiting another over names records: first {first}; again {again}");
}

/// An `async move` block capturing a future and a value by move ([`BY_MOVE`]): the block is
/// `by_move`'s async body 0, its state made over records.
///
/// Prints: `d106 async move block over names records: first <counts>; again <counts>`
#[test]
fn an_async_move_block_is_walked_over_names_records() {
    let (first, again) = walk_async(BY_MOVE, &["by_move"]);
    println!("d106 async move block over names records: first {first}; again {again}");
}

/// A future pending once, woken once ([`PENDING_ONCE`]): `once`'s state made over records.
///
/// Prints: `d106 async pending once over names records: first <counts>; again <counts>`
#[test]
fn an_async_fn_pending_once_is_walked_over_names_records() {
    let (first, again) = walk_async(PENDING_ONCE, &["once"]);
    println!("d106 async pending once over names records: first {first}; again {again}");
}

/// The ladder rung x0-dyn-01 (no_core: provided and required methods called through `&dyn Tr`,
/// `&mut dyn Tr` and `&dyn Sub`, whose supertrait is `Base`), its files unchanged.
fn dyn_rung() -> Vec<(String, String)> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../blaze-cli/tests/ladder/front2/x0-dyn-01-provided-override/src");
    ["main.rs", "prelude.rs"]
        .iter()
        .map(|f| (format!("{DIR}src/{f}"), std::fs::read_to_string(dir.join(f)).unwrap_or_else(|e| panic!("the dyn rung's {f}: {e}"))))
        .collect()
}

/// The checker key of crate 0's item of `kind` named `name`.
fn item_named(f: &Fixture, kind: ItemKind, name: &str) -> ItemKey {
    let records = reader(f).records;
    let mut found = None;
    assert!(records.each_item(0, &mut |key, _| if found.is_none() && records.kind_of_item(key) == Some(kind) && records.name_of(key) == Some(name) {
        found = Some(checker_of(key));
    }), "the program's items record lists its items");
    found.unwrap_or_else(|| panic!("no {kind:?} named `{name}`: {:?}", logged(records)))
}

/// Trait objects' tables over names records: a body naming `dyn Tr` or `dyn Sub` asks the lazy
/// program for the trait's object table (fill.rs `body_objects`, program.rs `object_tables`),
/// and over records the key map files no item, so the trait's library key must come from the
/// names source (`Keys::library_in`, now the only form). Before the fix every such trait was
/// refused "as a trait object sees it: no library key" (hello world's `core::error::Error`,
/// 2026-11-17 g053512). After `m_of` and `b_of` are checked, alone, the program answers each
/// table, the supertrait `Base`'s too (asked under `Sub`, never named as a principal).
/// Control: before any check no table is made; `One`, a struct, never has one.
///
/// Prints: `d106 dyn tables over names records: Tr, Sub, Base`
#[test]
fn trait_object_tables_are_made_over_names_records() {
    let f = fixture_of(&dyn_rung(), blaze_build::Panic::Abort);
    let names = reader(&f);
    let main = fn_named(&f, "main");
    let mut program = RecordsProgram::from_records(names.records, names.frame, names.lang_crates, Some(main), alloc()).unwrap_or_else(|e| panic!("the lazy program over records: {e}"));
    let traits = ["Tr", "Sub", "Base"].map(|t| (t, item_named(&f, ItemKind::Trait, t)));
    for (t, k) in traits {
        assert!(program.object_trait(k).is_none(), "control: `{t}`'s table before any body named it");
    }
    for owner in ["m_of", "b_of"] {
        program.check_one(fn_named(&f, owner)).unwrap_or_else(|e| panic!("`{owner}`'s check: {e}")).expect("a fn of the program");
    }
    for (t, k) in traits {
        assert!(program.object_trait(k).is_some(), "the trait `{t}`'s object table was made over records");
    }
    assert!(program.object_trait(item_named(&f, ItemKind::Struct, "One")).is_none(), "control: a struct has no object table");
    assert!(logged(names.records).is_empty(), "the records filed {:?}", logged(names.records));
    println!("d106 dyn tables over names records: Tr, Sub, Base");
}

/// The x0-dyn-01 rung WALKED over names records: its vtables' words are tabled on demand
/// (make_one.rs `vtable_rows` files a word no row tables, `drop_in_place` first, as an untabled
/// callee: checked, then the instance made again), and a word's impl no check selected (`impl
/// Base for Two`, under `dyn Sub`'s table) is made at instance time (lane UZ: a hole is
/// `Missing`, `make_asked` fills it). Every instance is made; a second walk finds every one.
///
/// Prints: `d106 dyn walk over names records: first <counts>; again <counts>`
#[test]
fn a_dyn_walks_vtable_words_are_tabled() {
    let f = fixture_of(&dyn_rung(), blaze_build::Panic::Abort);
    let entry = fn_named(&f, "main");
    let mut store = MemoryStore::new_in(alloc());
    let b = builder_with(blaze_build::Panic::Abort);
    let (first, _) = walk_keeping(&f, &EveryItem, entry, &mut store, &b);
    assert!(first.reached > 0 && first.made == first.reached && first.found == 0, "every instance reached is made: {first}");
    let (again, _) = walk_keeping(&f, &EveryItem, entry, &mut store, &b);
    assert_eq!((again.reached, again.found, again.make_calls()), (first.reached, first.reached, 0), "a second walk finds every instance: {again}");
    println!("d106 dyn walk over names records: first {first}; again {again}");
}

/// The entry WRITTEN BY A MACRO at module level (as bstd's `main!` writes `fn main`), calling a
/// fn declared in its own body (`nested`), a fn written in the file (`from_file`) and an impl's
/// fn (`Unit::three`) whose body declares a fn of its own (`two`). Five instances.
const MACRO_ENTRY: &str = "#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = \"sized\"]
trait Sized {}

#[lang = \"copy\"]
trait Copy {}

struct Unit;

impl Unit {
    fn three() -> i32 {
        fn two() -> i32 {
            2
        }
        two() + 1
    }
}

fn from_file() -> i32 {
    4
}

macro_rules! entry {
    () => {
        #[unsafe(no_mangle)]
        pub extern \"C-unwind\" fn main() -> i32 {
            fn nested() -> i32 {
                1
            }
            nested() + from_file() + Unit::three()
        }
    };
}

entry!();
";

/// The entry EXPORTED FROM A BODY a macro writes (bstd's `main!` declares `bstd_hosted_start`
/// in the `fn main` it writes): the start rule finds `main` in `outer`'s body. Three instances.
const NESTED_ENTRY: &str = "#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = \"sized\"]
trait Sized {}

#[lang = \"copy\"]
trait Copy {}

fn from_file() -> i32 {
    4
}

macro_rules! hosted {
    () => {
        fn outer() -> i32 {
            #[unsafe(no_mangle)]
            pub extern \"C-unwind\" fn main() -> i32 {
                fn inner() -> i32 {
                    1
                }
                inner() + from_file()
            }
            0
        }
    };
}

hosted!();
";

/// [`NESTED_ENTRY`]'s shape as bstd's `main!` writes `bstd_hosted_start`: the exported entry,
/// declared in a body, hands a fn declared in ITS body to another fn as a VALUE
/// (`guest::start(table, hosted)`), never calling it by name. Four instances: `main`, `apply`,
/// `hosted` and `from_file`.
const LOCAL_FN_VALUE: &str = "#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = \"sized\"]
trait Sized {}

#[lang = \"copy\"]
trait Copy {}

fn from_file() -> i32 {
    4
}

fn apply(f: fn() -> i32) -> i32 {
    f()
}

macro_rules! hosted {
    () => {
        fn outer() -> i32 {
            #[unsafe(no_mangle)]
            pub extern \"C-unwind\" fn main() -> i32 {
                fn hosted() -> i32 {
                    from_file()
                }
                apply(hosted)
            }
            0
        }
    };
}

hosted!();
";

/// The program's personality routine, declared by its crate as a `#![no_core]` program's own
/// (std's `rust_eh_personality` is the same lang item in std's crate).
const PERSONALITY_ROUTINE: &str = "
#[lang = \"eh_personality\"]
fn rust_eh_personality() {}
";

/// The run's roots over names records for a program built to unwind (run/special.rs
/// `run_roots`; UNWIND.md 3.3, piece P3): a program whose lang crates declare
/// `#[lang = \"eh_personality\"]` names that fn in its head, as the last root row at no
/// arguments (so the walk makes it at its first call); built to abort, the same program names
/// none. Control: the program without the routine has roots built to unwind too (rule 4: a
/// program that never lands needs no routine; the parent refuses the first record that lands,
/// blaze-cli run_parent.rs `NoPersonality`), naming none and one root fewer; none built to abort.
///
/// Prints: `d106 personality over names records: named <hex>; none declared: no routine named`
#[test]
fn the_personality_routine_is_a_root_over_names_records() {
    use blaze_ladder::code_records::PanicStrategy;
    use blaze_ladder::run::special::run_roots;
    let with = fixture(&std::format!("{PROGRAM}{PERSONALITY_ROUTINE}"));
    let names = reader(&with);
    let routine = fn_named(&with, "rust_eh_personality");
    let got = run_roots(names.records, names.lang_crates, PanicStrategy::Unwind, names.frame, alloc(), alloc()).unwrap_or_else(|e| panic!("the run's roots: {e}"));
    let last = got.root(got.len() - 1);
    assert_eq!(last.tabled, routine, "the last root row is not the routine");
    assert_eq!(got.head().personality, Some(last.symbol), "the head does not name the routine's row");
    assert_ne!(Some(got.head().line.symbol), got.head().personality, "the routine was named as the entry");
    let abort = run_roots(names.records, names.lang_crates, PanicStrategy::Abort, names.frame, alloc(), alloc()).unwrap_or_else(|e| panic!("the run's roots, abort: {e}"));
    assert_eq!(abort.head().personality, None, "a program built to abort named a routine");
    assert_eq!(abort.len() + 1, got.len(), "built to abort, the routine is still a root");

    let without = fixture(PROGRAM);
    let names = reader(&without);
    let unwinds = run_roots(names.records, names.lang_crates, PanicStrategy::Unwind, names.frame, alloc(), alloc()).unwrap_or_else(|e| panic!("a program with no routine, built to unwind, was refused at its start: {e}"));
    assert_eq!(unwinds.head().personality, None, "a program with no routine named one");
    assert_eq!(unwinds.len() + 1, got.len(), "a program with no routine has a root row for one");
    let abort = run_roots(names.records, names.lang_crates, PanicStrategy::Abort, names.frame, alloc(), alloc()).unwrap_or_else(|e| panic!("the run's roots, abort, no routine: {e}"));
    assert_eq!(abort.head().personality, None);
    println!("d106 personality over names records: named {:x}; none declared: no routine named", routine.0);
}

/// Two structs no body of [`PROGRAM`] names: `Lone`, holding `Inner` by value.
const LONE: &str = "
struct Inner {
    a: u32,
}

struct Lone {
    n: u32,
    inner: Inner,
}
";

/// An ADT whose row is not made is a NEED the lazy program fills, never a refusal (hello
/// world's stop of 2026-11-17 g082011: `start`'s signature, "a type whose ADT definition the
/// world did not answer, met while listing the coroutines a value holds"). The seam's `adt` of
/// such a key notes its head; `make_asked` makes it, and the same question is then answered,
/// its fields read from the answer's own table. Controls: with nothing noted `make_asked` makes
/// nothing; a fn's key asked as an ADT notes nothing; once answered, nothing is noted again.
///
/// Prints: `d106 unmade ADT over names records: Lone made on demand, 2 fields`
#[test]
fn an_adt_no_body_named_is_a_need_the_lazy_program_fills() {
    let f = fixture(&std::format!("{PROGRAM}{LONE}"));
    let names = reader(&f);
    let main = fn_named(&f, "main");
    let mut program = RecordsProgram::from_records(names.records, names.frame, names.lang_crates, Some(main), alloc()).unwrap_or_else(|e| panic!("the lazy program over records: {e}"));
    program.check_one(main).unwrap_or_else(|e| panic!("`main`'s check: {e}")).expect("a fn of the program");
    let i = program.fn_index(main).expect("`main` has a row");
    let made = |program: &mut RecordsProgram<'static>| program.make_asked(i).unwrap_or_else(|e| panic!("`make_asked`: {e}"));
    // Whatever the check left noted, made; then nothing is.
    made(&mut program);
    assert!(!made(&mut program), "control: nothing noted, yet something was made");
    assert!(program.adt(main).is_none(), "a fn is no ADT");
    assert!(!made(&mut program), "control: a fn's key asked as an ADT was noted as a need");
    let (lone, inner) = (item_named(&f, ItemKind::Struct, "Lone"), item_named(&f, ItemKind::Struct, "Inner"));
    assert!(program.adt(lone).is_none(), "control: no body names `Lone`, yet its row was made");
    assert!(made(&mut program), "`Lone`, asked and not made, was not noted as a need");
    let adt = program.adt(lone).unwrap_or_else(|| panic!("`Lone` made on demand is not answered: {:?}", logged(names.records)));
    assert_eq!(adt.fields.len(), 2, "`Lone`'s fields");
    assert!(matches!(adt.types.node(adt.fields[0].ty), Ok(blaze_typeck::PNode::Uint(_))), "`Lone.n` read from the answer's table: {:?}", adt.types.node(adt.fields[0].ty));
    assert!(matches!(adt.types.node(adt.fields[1].ty), Ok(blaze_typeck::PNode::Adt(k, _)) if k == inner), "`Lone.inner` read from the answer's table: {:?}", adt.types.node(adt.fields[1].ty));
    assert!(!made(&mut program), "an answered ADT was noted again");
    assert!(logged(names.records).is_empty(), "the records filed {:?}", logged(names.records));
    println!("d106 unmade ADT over names records: Lone made on demand, 2 fields");
}

/// A fn whose signature holds a struct (`Holder<T, F>`) one of whose fields is a projection
/// (`<T as Pick>::Out`) that only the instance names, at `T = u8`, as `Hidden`, a struct no body
/// the walk has checked names when the instance's coroutines are listed; its other field holds
/// an async block's future by value. Built from [`ASYNC_HEAD`] (`Twice`, `block_on`).
const HELD_THROUGH_PROJECTION: &str = "
trait Pick {
    type Out;
    fn make() -> Self::Out;
    fn read(o: &Self::Out) -> u32;
}

struct Hidden {
    v: u32,
}

impl Pick for u8 {
    type Out = Hidden;
    fn make() -> Hidden {
        Hidden { v: 7 }
    }
    fn read(o: &Hidden) -> u32 {
        o.v
    }
}

struct Holder<T: Pick, F> {
    out: T::Out,
    fut: F,
}

struct Unreached {
    w: u32,
}

fn hold<T: Pick, F: Future<Output = u32>>(_pick: T, fut: F) -> Holder<T, F> {
    Holder { out: T::make(), fut }
}

fn finish<T: Pick, F: Future<Output = u32>>(h: Holder<T, F>, polls: &mut u32, wakes: &mut u32) -> u32 {
    let Holder { out, fut } = h;
    block_on(fut, polls, wakes) + T::read(&out)
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let mut polls = 0u32;
    let mut wakes = 0u32;
    let h = hold(0u8, async { Twice { left: 1, value: 5 }.await });
    if finish(h, &mut polls, &mut wakes) == 12 { 42 } else { 1 }
}
";

/// [`HELD_THROUGH_PROJECTION`] WALKED over names records: every instance is made, `Hidden`
/// (met grounding `Holder<u8, _>`'s fields while listing the coroutines `main`'s and `finish`'s
/// values hold) made on demand and answered after the walk. Control: `Unreached`, a struct
/// nothing reaches, has no row (the lazy program makes only what is asked). A second walk finds
/// every instance.
///
/// Prints: `d106 ADT through a projection over names records: first <counts>; again <counts>`
#[test]
fn a_struct_met_through_a_projection_while_listing_coroutines_is_made_over_names_records() {
    let f = fixture_of(&async_program(HELD_THROUGH_PROJECTION), blaze_build::Panic::Abort);
    let entry = fn_named(&f, "main");
    let b = builder_with(blaze_build::Panic::Abort);
    let mut store = MemoryStore::new_in(alloc());
    let (first, maker) = walk_keeping(&f, &EveryItem, entry, &mut store, &b);
    assert!(first.reached > 0 && first.made == first.reached && first.found == 0, "every instance reached is made: {first}");
    let program = maker.program().expect("a first run constructs the program");
    assert!(program.adt(item_named(&f, ItemKind::Struct, "Hidden")).is_some(), "`Hidden` has no row after the walk");
    assert!(program.adt(item_named(&f, ItemKind::Struct, "Unreached")).is_none(), "control: `Unreached` has a row");
    let (again, _) = walk_keeping(&f, &EveryItem, entry, &mut store, &b);
    assert_eq!((again.reached, again.found, again.make_calls()), (first.reached, first.reached, 0), "a second walk finds every instance: {again}");
    println!("d106 ADT through a projection over names records: first {first}; again {again}");
}

/// A local fn passed as a value from a macro-written entry ([`LOCAL_FN_VALUE`]) is tabled and
/// made on demand: before the fix the build refused it, "`main` reaches item .. which no fn row
/// tables" (a body's check tables the fns it CALLS; one named as a value was owed no row).
/// Hello world's stop of 2026-11-17 g080047 (`bstd_hosted_start` handing `hosted` to
/// `guest::start`). A second walk finds every instance.
///
/// Prints: `d106 local fn value over names records: first <counts>; again <counts>`
#[test]
fn a_local_fn_passed_as_a_value_is_made_over_names_records() {
    use blaze_ladder::front2::program::{StartOf, start_of};
    use blaze_ladder::front2::start::Entry2;
    let f = fixture(LOCAL_FN_VALUE);
    let entry = {
        let records = reader(&f).records;
        match start_of(records, alloc()).unwrap_or_else(|e| panic!("the start rule over records: {e}")) {
            StartOf::Exported(Entry2::CMain(lib)) => checker_of(lib),
            other => panic!("the start is `main` exported from `outer`'s body, not {other:?}"),
        }
    };
    let b = builder();
    let mut store = MemoryStore::new_in(alloc());
    let (first, _) = walk_keeping(&f, &EveryItem, entry, &mut store, &b);
    assert_eq!((first.reached, first.found, first.made), (4, 0, 4), "`main`, `apply`, the local `hosted` and `from_file` are made: {first}");
    let (again, _) = walk_keeping(&f, &EveryItem, entry, &mut store, &b);
    assert_eq!((again.reached, again.found, again.make_calls()), (4, 4, 0), "a second walk makes none: {again}");
    println!("d106 local fn value over names records: first {first}; again {again}");
}

/// Whether checker key `k` is of an item declared in a body (`LibKey::INNER`, kept in a
/// checker key's byte 8).
fn is_inner(k: ItemKey) -> bool {
    k.0.to_be_bytes()[8] & LibKey::INNER != 0
}

/// [`EveryItem`], but each key `salted` picks answers another BodyCheck key: over a store a run
/// of [`EveryItem`] filled, exactly those instances are made, by a FRESH maker whose key map is
/// empty (a found caller's callee asked cold, as the command's process asks it).
struct Salted {
    salted: fn(ItemKey) -> bool,
}

impl BodyIds for Salted {
    fn body_check(&self, item: ItemKey) -> Option<BodyId<'_>> {
        let id = EveryItem.body_check(item)?;
        if !(self.salted)(item) {
            return Some(id);
        }
        Some(BodyId { key: Hasher::new("d106 test salted").part(&id.key.0[..]).finish(), reads: &READS })
    }

    fn today(&self, fact: &[u8; 32]) -> Option<[u8; 32]> {
        EveryItem.today(fact)
    }

    fn crate_fact(&self, item: ItemKey) -> Option<Read> {
        EveryItem.crate_fact(item)
    }
}

/// D124 lazy-parent piece M2: every kind of fn a from-source front end finds is found by the
/// lazy program over names records (front2/program/lazy/locate.rs), each asked COLD.
///
/// 1. The first walk makes all five instances of [`MACRO_ENTRY`] (`main` is an expansion's
///    item row; `nested` is filed by lowering `main`; `Unit::three` by its impl's head; `two`
///    by lowering `three`); a second walk finds all five and makes none.
/// 2. Cold body-locals: the keys of `nested` and `two` salted, so a fresh maker is asked them
///    while `main` and `three`, whose bodies declare them, are found, never lowered by a check:
///    the search reads `main`'s and `three`'s texts by the one derivation (nested_exports.rs
///    `walk_every`) and lowers them, filing both. Exactly two made.
/// 3. Cold items and members: the keys of `from_file` and `Unit::three` salted: `from_file` by
///    its item row, `three` by its crate's member index (its impl's head made on demand).
///    Exactly two made.
///
/// Control: with no key salted, a third walk makes none (so 2 and 3 make only what is salted).
///
/// Prints: `d106 macro entry over names records: first <counts>; again <counts>; cold locals <counts>; cold members <counts>`
#[test]
fn a_macro_written_entry_and_every_kind_of_fn_are_found_over_names_records() {
    let f = fixture(MACRO_ENTRY);
    let entry = fn_named(&f, "main");
    let b = builder();
    let mut store = MemoryStore::new_in(alloc());
    let (first, _) = walk_keeping(&f, &EveryItem, entry, &mut store, &b);
    println!("d106 macro entry: first {first}");
    assert_eq!((first.reached, first.found, first.made), (5, 0, 5), "every instance of the macro-written entry is made: {first}");
    let (again, _) = walk_keeping(&f, &EveryItem, entry, &mut store, &b);
    println!("d106 macro entry: again {again}");
    assert_eq!((again.reached, again.found, again.make_calls()), (5, 5, 0), "a second walk makes none: {again}");
    // `Unit::three`'s member key: a member key's byte 8 is a hash byte, so its inner bit says
    // nothing (front2/keys.rs `member_key`); the locals are the inner keys other than it.
    let three = {
        let records = reader(&f).records;
        // Its container is the fixture's one impl (`impl Unit`), not the struct.
        let mut unit = None;
        assert!(records.each_item(0, &mut |key, _| if records.kind_of_item(key) == Some(ItemKind::Impl) {
            unit = Some(checker_of(key));
        }));
        let unit = unit.unwrap_or_else(|| panic!("no impl: {:?}", logged(records)));
        blaze_ladder::front2::keys::member_key_of(unit, blaze_ladder::front2::keys::MemberKind::Fn, "three")
    };
    MAIN.with(|m| m.set(Some(entry)));
    THREE.with(|t| t.set(Some(three)));
    let from_file = fn_named(&f, "from_file");
    println!("d106 macro entry: inner bits: main {}, from_file {}, Unit::three {}", is_inner(entry), is_inner(from_file), is_inner(three));
    fn local(k: ItemKey) -> bool {
        is_inner(k) && THREE.with(|t| t.get() != Some(k))
    }
    let (locals, _) = walk_keeping(&f, &Salted { salted: local }, entry, &mut store, &b);
    println!("d106 macro entry: cold locals {locals}");
    assert_eq!((locals.reached, locals.found, locals.made), (5, 3, 2), "the two body-local fns, asked cold, are made: {locals}");
    fn outer_but_main(k: ItemKey) -> bool {
        !local(k) && MAIN.with(|m| m.get() != Some(k))
    }
    let (members, _) = walk_keeping(&f, &Salted { salted: outer_but_main }, entry, &mut store, &b);
    println!("d106 macro entry: cold members {members}");
    assert_eq!((members.reached, members.found, members.made), (5, 3, 2), "`from_file` and `Unit::three`, asked cold, are made: {members}");
    println!("d106 macro entry over names records: first {first}; again {again}; cold locals {locals}; cold members {members}");
}

thread_local! {
    /// The entry [`a_macro_written_entry_and_every_kind_of_fn_are_found_over_names_records`]
    /// keeps unsalted (a `fn` pointer captures nothing).
    static MAIN: core::cell::Cell<Option<ItemKey>> = const { core::cell::Cell::new(None) };
    /// `Unit::three`'s member key, which [`is_inner`] cannot tell from a local's.
    static THREE: core::cell::Cell<Option<ItemKey>> = const { core::cell::Cell::new(None) };
}

/// The process's start exported from a body a macro wrote ([`NESTED_ENTRY`]), its key given by
/// the start rule (program.rs `start_of`): a fresh maker is asked it with nothing filed and finds
/// it by the start rule's chain (`outer` lowered, which files `main`), then `inner` by `main`'s
/// lowering. All three made; a second walk makes none. Then `main` and `inner` asked cold over
/// the full store with their keys salted: made again, by a fresh maker each.
///
/// Prints: `d106 nested entry over names records: first <counts>; again <counts>; cold <counts>`
#[test]
fn an_entry_exported_from_a_macro_written_body_is_found_over_names_records() {
    use blaze_ladder::front2::program::{StartOf, start_of};
    use blaze_ladder::front2::start::Entry2;
    let f = fixture(NESTED_ENTRY);
    let entry = {
        let records = reader(&f).records;
        let start = start_of(records, alloc()).unwrap_or_else(|e| panic!("the start rule over records: {e}"));
        assert!(logged(records).is_empty(), "the start rule's reads: {:?}", logged(records));
        match start {
            StartOf::Exported(Entry2::CMain(lib)) => {
                assert!(lib.is_inner(), "`main` is declared in `outer`'s body");
                checker_of(lib)
            }
            other => panic!("the start is `main` exported from `outer`'s body, not {other:?}"),
        }
    };
    let b = builder();
    let mut store = MemoryStore::new_in(alloc());
    let (first, _) = walk_keeping(&f, &EveryItem, entry, &mut store, &b);
    assert_eq!((first.reached, first.found, first.made), (3, 0, 3), "the nested entry, its local fn and the file's fn are made: {first}");
    let (again, _) = walk_keeping(&f, &EveryItem, entry, &mut store, &b);
    assert_eq!((again.reached, again.found, again.make_calls()), (3, 3, 0), "a second walk makes none: {again}");
    let (cold, _) = walk_keeping(&f, &Salted { salted: is_inner }, entry, &mut store, &b);
    assert_eq!((cold.reached, cold.found, cold.made), (3, 1, 2), "`main` and `inner`, asked cold, are made: {cold}");
    println!("d106 nested entry over names records: first {first}; again {again}; cold {cold}");
}

/// Statics in the run (run.md, "Statics in the run", steps 1 and 2's static words): `main`
/// reads `AT`, a `&[u8; 6]` to `GREETING`.
const STATICS: &str = "#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = \"sized\"]
trait Sized {}

#[lang = \"copy\"]
trait Copy {}

unsafe extern \"C\" {
    fn write(fd: i32, buf: *const u8, len: usize) -> isize;
}

static GREETING: [u8; 6] = [104, 101, 108, 108, 111, 10];
static AT: &[u8; 6] = &GREETING;

#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {
    unsafe { write(1, AT as *const [u8; 6] as *const u8, 6) };
    0
}
";

/// The statics' rows over names records: `main`'s code record DECLARES `AT` (by item key, beside
/// its instances), `AT` is a row of its own whose entry is a static's (a read-only datum of one
/// word naming `GREETING`'s symbol, and `GREETING` declared), and `GREETING`'s datum is its six
/// bytes, read-only, no word. A second run finds all three, evaluating nothing. Control: an edit
/// of `GREETING`'s initializer makes `GREETING`'s datum alone; `main` and `AT` are found (code
/// names a static's word, a static's value names another's address, never its bytes).
/// Prints "d106 statics: first made 3; AT one word to GREETING; GREETING 6 bytes read-only; second found 3; edit made 1".
#[test]
fn a_static_is_a_record_of_its_own_named_by_the_code() {
    use blaze_code::datum::Place;
    use blaze_code::record::{Space, Target};
    use blaze_ladder::body_ids::RowKind;
    use blaze_ladder::code_records::statics::StaticEntryView;
    use blaze_ladder::code_records::EntryView;
    use blaze_ladder::symbols::static_symbol;

    let f = fixture(STATICS);
    let main = fn_named(&f, "main");
    let mut store = MemoryStore::new_in(alloc());
    let names = reader(&f);
    let mut maker = Maker::new(names, Some(main), &builder(), alloc());
    let (walk, first) = walk_records(main, &EveryItem, &mut store, &builder(), &producer(), &mut maker, alloc()).unwrap_or_else(|e| {
        println!("the record walk: {e}");
        panic!("the record walk: {e}")
    });
    assert!(logged(names.records).is_empty(), "the records filed {:?}", logged(names.records));
    assert_eq!((first.reached, first.made), (3, 3), "main, AT and GREETING: {first}");
    let statics: Vec<usize> = (0..walk.len()).filter(|&r| walk.kind(r) == RowKind::Static).collect();
    assert_eq!(statics.len(), 2, "two static rows");
    let entry_of = |row: usize| store.get(walk.key(row).expect("keyed")).expect("the store reads").expect("put");
    let main_entry = EntryView::read(entry_of(0)).expect("main's entry reads");
    let mut declared = Vec::new();
    main_entry.each_static(|k| declared.push(k));
    let (at, greeting) = (walk.tabled(statics[0]), walk.tabled(statics[1]));
    assert_eq!(declared, [at], "main's record declares the static it reads");
    let at_view = StaticEntryView::read(entry_of(statics[0])).expect("AT's entry reads");
    assert_eq!((at_view.datum.place(), at_view.datum.bytes(), at_view.datum.words()), (Place::ReadOnly, &[0u8; 8][..], 1), "AT is one word, its bytes zero");
    let word = at_view.datum.word(0);
    assert_eq!((word.at, word.addend), (0, 0));
    assert_eq!(at_view.datum.target(word.target as usize), Target { space: Space::Static, symbol: static_symbol(greeting) }, "AT's word names GREETING");
    let mut at_statics = Vec::new();
    at_view.each_static(|k| at_statics.push(k));
    assert_eq!(at_statics, [greeting], "AT declares GREETING");
    let g_view = StaticEntryView::read(entry_of(statics[1])).expect("GREETING's entry reads");
    assert_eq!((g_view.datum.place(), g_view.datum.bytes(), g_view.datum.words()), (Place::ReadOnly, &b"hello\n"[..], 0));
    assert_eq!(walk.symbol(statics[1]), static_symbol(greeting), "a static's row is by its symbol");

    let (_, second) = walk_records(main, &EveryItem, &mut store, &builder(), &producer(), &mut Maker::new(reader(&f), Some(main), &builder(), alloc()), alloc()).expect("the second walk");
    assert_eq!((second.found, second.made), (3, 0), "second run {second}");

    // The edit: GREETING's text moves its key alone (the stand-in keys GREETING by its text).
    struct Edited(ItemKey);
    impl BodyIds for Edited {
        fn body_check(&self, item: ItemKey) -> Option<BodyId<'_>> {
            if item != self.0 {
                let every: &'static EveryItem = &EveryItem;
                return every.body_check(item);
            }
            Some(BodyId { key: Hasher::new("d106 test every item").part(&item.0.to_le_bytes()[..]).part(b"edited").finish(), reads: &READS })
        }
        fn today(&self, fact: &[u8; 32]) -> Option<[u8; 32]> {
            EveryItem.today(fact)
        }
        fn crate_fact(&self, item: ItemKey) -> Option<Read> {
            EveryItem.crate_fact(item)
        }
    }
    let edited = fixture(&STATICS.replace("[104, 101, 108, 108, 111, 10]", "[104, 105, 33, 33, 33, 10]"));
    let (walk, third) = walk_records(main, &Edited(greeting), &mut store, &builder(), &producer(), &mut Maker::new(reader(&edited), Some(main), &builder(), alloc()), alloc()).expect("the edited walk");
    assert_eq!((third.found, third.made), (2, 1), "edit {third}");
    let row = (0..walk.len()).find(|&r| walk.tabled(r) == greeting).expect("GREETING's row");
    let g = StaticEntryView::read(store.get(walk.key(row).expect("keyed")).expect("reads").expect("put")).expect("reads");
    assert_eq!(g.datum.bytes(), b"hi!!!\n", "the edited value");
    println!("d106 statics: first made 3; AT one word to GREETING; GREETING 6 bytes read-only; second found {}; edit made {}", second.found, third.made);
}

/// A `format_args!` with a `{}`, a `{:?}` and a `{:x}` placeholder, its library the pinned
/// core's shape in miniature (library/core/src/fmt/rt.rs: `#[lang = "format_argument"] struct
/// Argument`, its constructors `new_display<T: Display>(x: &T)`, `new_debug`, `new_lower_hex`
/// in `impl Argument<'_>`; mod.rs: `#[lang = "format_arguments"] struct Arguments { template,
/// args }`). Each constructor here writes its own code, so `main` reads which one made each
/// entry of the array and the byte its reference points at: 42 when the three are 1, 2, 3 and
/// the values 7, 8, 9.
const FMT: &str = "#![feature(no_core, lang_items, rustc_attrs)]
#![no_core]
#![no_main]
#![allow(internal_features)]

#[lang = \"sized\"]
trait Sized {}

#[lang = \"copy\"]
trait Copy {}

impl Copy for u32 {}

#[rustc_builtin_macro]
macro_rules! format_args {
    ($fmt:expr) => {{ /* compiler built-in */ }};
    ($fmt:expr, $($args:tt)*) => {{ /* compiler built-in */ }};
}

pub mod fmt {
    pub trait Display {}
    pub trait Debug {}
    pub trait LowerHex {}
    impl Display for u32 {}
    impl Debug for u32 {}
    impl LowerHex for u32 {}

    #[lang = \"format_arguments\"]
    pub struct Arguments<'a> {
        pub template: *const u8,
        pub args: *const rt::Argument<'a>,
    }

    pub mod rt {
        #[lang = \"format_argument\"]
        pub struct Argument<'a> {
            pub value: &'a u8,
            pub code: u32,
        }

        impl Argument<'_> {
            pub fn new_display<T: super::Display>(x: &T) -> Argument<'_> {
                Argument { value: unsafe { &*(x as *const T as *const u8) }, code: 1 }
            }
            pub fn new_debug<T: super::Debug>(x: &T) -> Argument<'_> {
                Argument { value: unsafe { &*(x as *const T as *const u8) }, code: 2 }
            }
            pub fn new_lower_hex<T: super::LowerHex>(x: &T) -> Argument<'_> {
                Argument { value: unsafe { &*(x as *const T as *const u8) }, code: 3 }
            }
        }
    }
}

#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {
    let (a, b, c) = (7u32, 8u32, 9u32);
    let f = format_args!(\"{} {:?} {:x}\", a, b, c);
    let [x, y, z] = unsafe { &*(f.args as *const [fmt::rt::Argument<'_>; 3]) };
    if x.code == 1 && y.code == 2 && z.code == 3 && *x.value == 7 && *y.value == 8 && *z.value == 9 { 42 } else { 1 }
}
";

/// [`FMT`] through the LAZY program over names records (the `blaze run` path): each
/// placeholder's `rt::Argument` constructor is asked when `main` is checked (fill.rs
/// `fmt_rows`, through fmt_template.rs `argument_new`, the one lookup both programs make) and
/// the build of `main` finds it (`fmt_argument_new`); all five instances are made (`main` and
/// the three constructors at `u32`), a second walk makes none. Controls: a trait no placeholder
/// asks (`{:o}`) has no constructor answered (no fallback), and the same program run through
/// the whole program (`run_with`, the new front) exits 42: the three constructors were called
/// in order with the arguments' own bytes. Before the fix the lazy walk stopped at `main`:
/// "`main`: expression ..: needs the `new_display` lang item fn, which the program does not
/// declare" (hello world's stop at g104400).
///
/// Prints: `d106 fmt over names records: first <counts>; again <counts>; run status 42`
#[test]
fn format_args_constructors_are_found_over_names_records() {
    let f = fixture(FMT);
    let main = fn_named(&f, "main");
    let b = builder();
    let mut store = MemoryStore::new_in(alloc());
    let (first, maker) = walk_keeping(&f, &EveryItem, main, &mut store, &b);
    assert_eq!((first.reached, first.found, first.made), (4, 0, 4), "`main` and `new_display`, `new_debug`, `new_lower_hex` at `u32` are made: {first}");
    let program = maker.program().expect("a first run constructs the program");
    for t in [blaze_typeck::FmtTrait::Display, blaze_typeck::FmtTrait::Debug, blaze_typeck::FmtTrait::LowerHex] {
        assert!(program.fmt_argument_new(t).is_some(), "the constructor for {t:?} is answered");
    }
    assert!(program.fmt_argument_new(blaze_typeck::FmtTrait::Octal).is_none(), "control: no placeholder asks `{{:o}}`, so none is answered");
    let (again, _) = walk_keeping(&f, &EveryItem, main, &mut store, &b);
    assert_eq!((again.reached, again.found, again.make_calls()), (4, 4, 0), "a second walk makes none: {again}");
    let files = [blaze_ladder::refusal::SourceFile { path: "src/main.rs", text: FMT }];
    let new = blaze_ladder::run::NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::Off };
    let status = blaze_ladder::run::run_with(&files, "src/main.rs", None, blaze_ladder::run::FrontChoice::New(new), blaze_own::testing::heap().shared())
        .unwrap_or_else(|e| panic!("the whole program's run: {e}"));
    assert_eq!(status, 42, "each placeholder's entry made by its own constructor, from the argument's own bytes");
    println!("d106 fmt over names records: first {first}; again {again}; run status {status}");
}

/// A `&dyn Tr` made of `&S` and called through (the Reference, "Trait objects": the call runs
/// `S`'s method, found in the vtable). The lang items as the pinned core's (marker.rs,
/// ops/unsize.rs); no `drop_in_place`, so the table's drop word is 0.
const DYN: &str = "#![feature(no_core, lang_items)]
#![no_core]
#![no_main]
#![allow(internal_features)]

#[lang = \"pointee_sized\"]
trait PointeeSized {}

#[lang = \"meta_sized\"]
trait MetaSized: PointeeSized {}

#[lang = \"sized\"]
trait Sized: MetaSized {}

#[lang = \"legacy_receiver\"]
trait LegacyReceiver: PointeeSized {}

impl<T: PointeeSized> LegacyReceiver for &T {}

#[lang = \"copy\"]
trait Copy {}

impl Copy for u32 {}

#[lang = \"unsize\"]
trait Unsize<T: PointeeSized>: PointeeSized {}

#[lang = \"coerce_unsized\"]
trait CoerceUnsized<T: PointeeSized> {}

impl<'a, 'b: 'a, T: PointeeSized + Unsize<U>, U: PointeeSized> CoerceUnsized<&'a U> for &'b T {}

#[lang = \"dispatch_from_dyn\"]
trait DispatchFromDyn<T> {}

impl<'a, T: PointeeSized + Unsize<U>, U: PointeeSized> DispatchFromDyn<&'a U> for &'a T {}

trait Tr {
    fn get(&self) -> u32;
}

struct S {
    v: u32,
}

impl Tr for S {
    fn get(&self) -> u32 {
        self.v
    }
}

fn via(t: &dyn Tr) -> u32 {
    t.get()
}

#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {
    let s = S { v: 7 };
    via(&s) as i32
}
";

/// A vtable is DATA its namer's entry carries (code_records.rs `VtableDatum`, form 6): `main`'s
/// record names one `Space::Vtable` target, and its entry carries that symbol's datum: read-only,
/// four words (no drop glue: 0; `S`'s size 4 and alignment 4; `get`'s entry), the method word a
/// `Space::Func` target by the symbol of an instance `main`'s entry DECLARES (so the run's
/// parent declares its slot before the datum is filled). Control: an edit of `get`'s body
/// makes `get` alone; `main` is found, its vtable's datum the same bytes (the datum names the
/// method by symbol, never by its code).
/// Prints "d106 vtable: main carries 1 datum of 32 bytes, its method word a declared instance; edit made 1".
#[test]
fn a_vtable_is_a_datum_its_namer_carries() {
    use blaze_code::datum::Place;
    use blaze_code::record::Space;
    use blaze_ladder::code_records::EntryView;

    let f = fixture(DYN);
    let main = fn_named(&f, "main");
    let mut store = MemoryStore::new_in(alloc());
    let names = reader(&f);
    let mut maker = Maker::new(names, Some(main), &builder(), alloc());
    let (walk, first) = walk_records(main, &EveryItem, &mut store, &builder(), &producer(), &mut maker, alloc()).unwrap_or_else(|e| {
        println!("the record walk: {e}");
        panic!("the record walk: {e}")
    });
    assert!(logged(names.records).is_empty(), "the records filed {:?}", logged(names.records));
    let main_bytes = store.get(walk.key(0).expect("keyed")).expect("the store reads").expect("put").to_vec();
    let entry = EntryView::read(&main_bytes).expect("main's entry reads");
    let named: Vec<_> = (0..entry.record.targets()).map(|i| entry.record.target(i)).filter(|t| t.space == Space::Vtable).map(|t| t.symbol).collect();
    assert_eq!(named.len(), 1, "main's record names one vtable");
    let mut carried = Vec::new();
    entry.each_vtable(|symbol, datum| -> Result<(), ()> {
        carried.push((symbol, datum.place(), datum.bytes().to_vec(), (0..datum.words()).map(|i| (datum.word(i).at, datum.target(datum.word(i).target as usize))).collect::<Vec<_>>()));
        Ok(())
    }, |e| panic!("main's vtables: {e}")).expect("read");
    assert_eq!(carried.len(), 1, "main's entry carries one vtable");
    let (symbol, place, bytes, words) = &carried[0];
    assert_eq!(*symbol, named[0], "the datum is the named vtable's");
    assert_eq!((*place, bytes.len()), (Place::ReadOnly, 32), "header and one method, read-only");
    assert_eq!(&bytes[..24], &[0, 0, 0, 0, 0, 0, 0, 0, 4, 0, 0, 0, 0, 0, 0, 0, 4, 0, 0, 0, 0, 0, 0, 0][..], "no drop glue; S's size 4, alignment 4");
    assert_eq!(words.len(), 1, "one address word: get's");
    let (at, target) = words[0];
    assert_eq!((at, target.space), (24, Space::Func));
    let mut declared = Vec::new();
    entry.each_declared(|d| -> Result<(), ()> { declared.push(blaze_ladder::symbols::fn_symbol_of_name(d.name)); Ok(()) }, |e| panic!("declared: {e}")).expect("read");
    assert!(declared.contains(&target.symbol), "the method word's instance is declared by main's entry");

    /// `get`'s key moved: a member's key is its impl's and its name's (front2/keys.rs
    /// `member_key_of`), so each impl's `get` key is listed; only `impl Tr for S` has one.
    struct Edited(Vec<ItemKey>);
    impl BodyIds for Edited {
        fn body_check(&self, item: ItemKey) -> Option<BodyId<'_>> {
            if !self.0.contains(&item) {
                let every: &'static EveryItem = &EveryItem;
                return every.body_check(item);
            }
            Some(BodyId { key: Hasher::new("d106 test every item").part(&item.0.to_le_bytes()[..]).part(b"edited").finish(), reads: &READS })
        }
        fn today(&self, fact: &[u8; 32]) -> Option<[u8; 32]> {
            EveryItem.today(fact)
        }
        fn crate_fact(&self, item: ItemKey) -> Option<Read> {
            EveryItem.crate_fact(item)
        }
    }
    let edited = fixture(&DYN.replace("        self.v\n", "        let v = self.v;\n        v\n"));
    let gets = {
        let records = reader(&edited).records;
        let mut gets = Vec::new();
        assert!(records.each_item(0, &mut |key, _| if records.kind_of_item(key) == Some(ItemKind::Impl) {
            gets.push(blaze_ladder::front2::keys::member_key_of(checker_of(key), blaze_ladder::front2::keys::MemberKind::Fn, "get"));
        }));
        gets
    };
    let (walk2, third) = walk_records(main, &Edited(gets),&mut store, &builder(), &producer(), &mut Maker::new(reader(&edited), Some(main), &builder(), alloc()), alloc()).expect("the edited walk");
    assert_eq!((third.reached, third.made), (first.reached, 1), "the edit made get alone: {third}");
    let again = store.get(walk2.key(0).expect("keyed")).expect("the store reads").expect("put");
    assert_eq!(again, &main_bytes[..], "main's entry, its vtable's datum with it, is the same");
    println!("d106 vtable: main carries 1 datum of {} bytes, its method word a declared instance; edit made {}", bytes.len(), third.made);
}

/// The pinned std's `panicking::catch_unwind` in miniature (library/std/src/panicking.rs): a
/// cfg'd-out twin of the same name before it, then a body declaring `union Data<F, R>` with
/// `ManuallyDrop` fields, writing `Data { f: ManuallyDrop::new(f) }`, passing `&raw mut data` to
/// the local `do_call` declared after a `return`, and moving the result out of `data.r`.
/// Built and run (front2_local_union.rs runs the same form whole), 5.
const CATCH: &str = "#![feature(no_core, lang_items)]
#![no_core]
#![no_main]
#![allow(internal_features)]

#[lang = \"pointee_sized\"]
trait PointeeSized {}

#[lang = \"meta_sized\"]
trait MetaSized: PointeeSized {}

#[lang = \"sized\"]
trait Sized: MetaSized {}

#[lang = \"copy\"]
trait Copy {}

impl Copy for u32 {}

#[lang = \"manually_drop\"]
#[repr(transparent)]
struct ManuallyDrop<T> {
    value: T,
}

impl<T> ManuallyDrop<T> {
    const fn new(value: T) -> ManuallyDrop<T> {
        ManuallyDrop { value }
    }

    const fn into_inner(slot: ManuallyDrop<T>) -> T {
        slot.value
    }
}

#[cfg(any())]
unsafe fn catch<R: Copy, F: Copy>(f: F) -> R {
    f
}

unsafe fn catch<R: Copy, F: Copy>(f: F, call: fn(F) -> R) -> R {
    union Data<F, R> {
        f: ManuallyDrop<F>,
        r: ManuallyDrop<R>,
    }

    let mut data = Data { f: ManuallyDrop::new(f) };
    unsafe {
        do_call(&raw mut data, call);
        return ManuallyDrop::into_inner(data.r);
    }

    #[inline]
    unsafe fn do_call<F: Copy, R: Copy>(data: *mut Data<F, R>, call: fn(F) -> R) {
        unsafe {
            let f = (*data).f.value;
            (*data).r = ManuallyDrop::new(call(f));
        }
    }
}

fn id(x: u32) -> u32 {
    x
}

#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {
    unsafe { catch(5u32, id) as i32 }
}
";

/// Every struct and union declared in a body is found over NAMES RECORDS: the block's module
/// binds its name (source.rs `Records::binds`, by blaze-scope `names::binds`, the module's own
/// table). The cause (2026-11-17, std-hello's `catch_unwind`): `binds` walked a written
/// `self::P`, and a written `self` passes over a block's module (names.rs `written_up`, since
/// names record version 3), so no block's own item was ever found and each local struct, enum
/// or union of a body was refused as "in a block that a macro call wrote". Control: the walk
/// of `self::P` at the same module does not find `P` (the old reading, removed).
/// Prints nothing; fails listing each form's walk.
#[test]
fn local_types_of_a_body_are_found_over_names_records() {
    let twin = "#[cfg(any())]\nunsafe fn catch<R: Copy, F: Copy>(f: F) -> R {\n    f\n}\n";
    assert!(CATCH.contains(twin));
    let head = &CATCH[..CATCH.find(twin).expect("twin")];
    let main = "\n#[unsafe(no_mangle)]\npub extern \"C-unwind\" fn main() -> i32 {\n    g() as i32\n}\n";
    let variants = [
        ("plain struct", format!("{head}fn g() -> u32 {{\n    struct P {{ a: u32 }}\n    let p = P {{ a: 3 }};\n    p.a\n}}\n{main}")),
        ("generic struct", format!("{head}fn g() -> u32 {{\n    struct P<T> {{ a: T }}\n    let p = P {{ a: 3u32 }};\n    p.a\n}}\n{main}")),
        ("plain union", format!("{head}fn g() -> u32 {{\n    union P {{ a: u32 }}\n    let p = P {{ a: 3 }};\n    unsafe {{ p.a }}\n}}\n{main}")),
        ("struct and fn", format!("{head}fn g() -> u32 {{\n    struct P {{ a: u32 }}\n    fn h(p: &P) -> u32 {{ p.a }}\n    h(&P {{ a: 3 }})\n}}\n{main}")),
        ("unsafe fn struct", format!("{head}unsafe fn g() -> u32 {{\n    struct P {{ a: u32 }}\n    let p = P {{ a: 3 }};\n    p.a\n}}\n#[unsafe(no_mangle)]\npub extern \"C-unwind\" fn main() -> i32 {{\n    unsafe {{ g() as i32 }}\n}}\n")),
    ];
    let mut seen = Vec::new();
    {
        let f = fixture(&variants[0].1);
        let records = reader(&f).records;
        let mut g = None;
        assert!(records.each_item(0, &mut |key, _| if records.kind_of_item(key) == Some(ItemKind::Function) && records.name_of(key) == Some("g") {
            g = Some(key);
        }));
        let g = g.expect("g");
        let text = records.text_of(g).expect("text").text;
        let nested = blaze_file_items::nested::scan(text, alloc());
        let modules = blaze_scope::blocks::block_modules(f.keys[0], g, &nested, alloc());
        let ms: Vec<_> = modules.iter().copied().flatten().collect();
        assert_eq!(ms.len(), 1, "g's body is one block module");
        assert!(records.binds(ms[0], blaze_surface::Namespace::Types, "P"), "the block's module binds P");
        let walked = records.walk(ms[0], blaze_surface::Namespace::Types, blaze_scope::names::Written { kind: blaze_surface::PathKind::SelfMod, segments: &["P"] });
        assert!(walked.is_err(), "control: a written `self::P` passes over the block's module: {walked:?}");
        assert!(logged(records).is_empty(), "{:?}", logged(records));
    }
    for (name, text) in variants.iter() {
        let f = fixture(text);
        let main = fn_named(&f, "main");
        let mut store = MemoryStore::new_in(alloc());
        let names = reader(&f);
        let mut maker = Maker::new(names, Some(main), &builder(), alloc());
        let got = walk_records(main, &EveryItem, &mut store, &builder(), &producer(), &mut maker, alloc());
        let shown = format!("{:?}", got.map(|(_, c)| c.to_string()).map_err(|e| e.to_string()));
        seen.push(format!("PROBE {name}: {}", shown.char_indices().nth(300).map_or(shown.clone(), |(i, _)| shown[..i].to_string())));
    }
    assert!(seen.iter().all(|s| s.contains(": Ok(")), "{}", seen.join("\n"));
}

/// A local item whose form is not built yet is refused as a user reads it (rule 19): the
/// enclosing item's path, the item's head as written and its `file:line`, never an
/// item-relative range and raw keys. The form: a struct in the body of a fn that is itself
/// declared in a body (locals.rs `LocalKind::unplaced`).
/// Prints "d106 local refusal: <the refusal>".
#[test]
fn a_local_item_not_built_is_refused_at_its_file_and_line() {
    let twin = "#[cfg(any())]";
    let head = &CATCH[..CATCH.find(twin).expect("twin")];
    let text = format!(
        "{head}fn g() -> u32 {{\n    fn h() -> u32 {{\n        struct P {{ a: u32 }}\n        let p = P {{ a: 3 }};\n        p.a\n    }}\n    h()\n}}\n\n#[unsafe(no_mangle)]\npub extern \"C-unwind\" fn main() -> i32 {{\n    g() as i32\n}}\n"
    );
    let line = text[..text.find("struct P").expect("P")].lines().count();
    let f = fixture(&text);
    let main = fn_named(&f, "main");
    let mut store = MemoryStore::new_in(alloc());
    let mut maker = Maker::new(reader(&f), Some(main), &builder(), alloc());
    let said = match walk_records(main, &EveryItem, &mut store, &builder(), &producer(), &mut maker, alloc()) {
        Ok((_, counts)) => panic!("the struct in a local fn's body is not built yet, and the walk made {counts}"),
        Err(e) => e.to_string(),
    };
    println!("d106 local refusal: {said}");
    assert!(said.contains("`struct P `") || said.contains("`struct P`"), "the head as written: {said}");
    assert!(said.contains(&format!("main.rs:{line})")), "its file and line {line}: {said}");
    assert!(said.contains("in `") && said.contains("h"), "the enclosing item: {said}");
    assert!(!said.contains("Range {"), "no item-relative range: {said}");
}

/// A fixture whose local fn `h` reads a trait method in a block that also holds `{the_use}`.
fn block_use(the_use: &str) -> String {
    let twin = "#[cfg(any())]";
    let head = &CATCH[..CATCH.find(twin).expect("twin")];
    format!(
        "{head}trait Get {{\n    fn get(&self) -> u32;\n}}\n\nimpl Get for u32 {{\n    fn get(&self) -> u32 {{\n        *self\n    }}\n}}\n\nfn g() -> u32 {{\n    fn h() -> u32 {{\n        {the_use}\n        3u32.get()\n    }}\n    h()\n}}\n\n#[unsafe(no_mangle)]\npub extern \"C-unwind\" fn main() -> i32 {{\n    g() as i32\n}}\n"
    )
}

/// A `use` of a block whose traits are not known is refused as a user reads it (rule 19): the
/// enclosing fn, the `use` as written (its attributes with it) and its `file:line`, never a raw
/// key or an item-relative range. The form the pinned std's `sanitize_standard_fds` stopped
/// `blaze run` with (fill.rs `check_refusal`, locals.rs `Locals::row_of_gap`).
/// Prints "d106 block use refusal: <the refusal>".
#[test]
fn a_block_use_not_resolved_is_refused_at_its_file_and_line() {
    let text = block_use("#[allow(unused_imports)] use nowhere::Missing;");
    let line = text[..text.find("#[allow(unused_imports)]").expect("the use")].lines().count();
    let f = fixture(&text);
    let main = fn_named(&f, "main");
    let mut store = MemoryStore::new_in(alloc());
    let mut maker = Maker::new(reader(&f), Some(main), &builder(), alloc());
    let said = match walk_records(main, &EveryItem, &mut store, &builder(), &producer(), &mut maker, alloc()) {
        Ok((_, counts)) => panic!("a block's `use` of no item is refused, and the walk made {counts}"),
        Err(e) => e.to_string(),
    };
    println!("d106 block use refusal: {said}");
    assert!(said.contains("`#[allow(unused_imports)] use nowhere::Missing;`"), "the use as written: {said}");
    assert!(said.contains(&format!("main.rs:{line})")), "its file and line {line}: {said}");
    assert!(said.contains("in `") && said.contains("h`"), "the enclosing fn: {said}");
    assert!(!said.contains("Range {") && !said.contains("ItemKey("), "no raw range or key: {said}");
}

/// A `use` of a block whose `cfg` is false is removed before anything reads it (the Reference,
/// "Conditional compilation"): the pinned std's `sanitize_standard_fds` writes `#[cfg(all(
/// target_os = "linux", target_env = "gnu"))] use libc::open64 as open;`, whose path names
/// nothing on macOS. The block's method call is checked, built and run: 3. The control is
/// [`a_block_use_not_resolved_is_refused_at_its_file_and_line`]: the same `use` with no false
/// `cfg` is refused.
/// Prints "d106 cfg'd-out block use: {counts}; run status 3".
#[test]
fn a_block_use_whose_cfg_is_false_is_not_read() {
    let text = block_use("#[cfg(any())] use nowhere::Missing;");
    let f = fixture(&text);
    let main = fn_named(&f, "main");
    let mut store = MemoryStore::new_in(alloc());
    let mut maker = Maker::new(reader(&f), Some(main), &builder(), alloc());
    let counts = match walk_records(main, &EveryItem, &mut store, &builder(), &producer(), &mut maker, alloc()) {
        Ok((_, counts)) => counts,
        Err(e) => panic!("the cfg'd-out use is removed, and the walk refused: {e}"),
    };
    let files = [blaze_ladder::refusal::SourceFile { path: "src/main.rs", text: &text }];
    let new = blaze_ladder::run::NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::Off };
    let status = blaze_ladder::run::run_with(&files, "src/main.rs", None, blaze_ladder::run::FrontChoice::New(new), blaze_own::testing::heap().shared())
        .unwrap_or_else(|e| panic!("the whole program's run: {e}"));
    assert_eq!(status, 3);
    println!("d106 cfg'd-out block use: {counts}; run status {status}");
}

/// [`CATCH`] over NAMES RECORDS, as `blaze run` makes it: every instance (`main`, `catch`,
/// `do_call`, `id`, `ManuallyDrop`'s two at their arguments) is made, the local union laid out
/// from its own `<F, R>` at the caller's arguments; a second walk makes none. Then the same
/// program run whole: 5.
/// Prints "d106 local union: first {counts}; again {counts}; run status 5".
#[test]
fn a_local_generic_union_is_made_over_names_records() {
    let f = fixture(CATCH);
    let main = fn_named(&f, "main");
    let mut store = MemoryStore::new_in(alloc());
    let b = builder();
    let (first, _) = walk_keeping(&f, &EveryItem, main, &mut store, &b);
    assert_eq!(first.made, first.reached, "every instance is made: {first}");
    let (again, _) = walk_keeping(&f, &EveryItem, main, &mut store, &b);
    assert_eq!((again.found, again.make_calls()), (again.reached, 0), "a second walk makes none: {again}");
    let files = [blaze_ladder::refusal::SourceFile { path: "src/main.rs", text: CATCH }];
    let new = blaze_ladder::run::NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::Off };
    let status = blaze_ladder::run::run_with(&files, "src/main.rs", None, blaze_ladder::run::FrontChoice::New(new), blaze_own::testing::heap().shared())
        .unwrap_or_else(|e| panic!("the whole program's run: {e}"));
    assert_eq!(status, 5);
    println!("d106 local union: first {first}; again {again}; run status {status}");
}
