//! A MAKE IS A FUNCTION OF ITS KEY, NEVER OF WHAT WAS MADE BEFORE IT (D194; rules 21, 25).
//!
//! Under `Codes::Pooled` (blaze-cli test_drive.rs) each code worker keeps its OWN maker
//! (make_one.rs `Maker`, the lazy program over names records) and is handed the instances in
//! the pool's order, never the walk's: a callee may be made before its caller on a maker that
//! never checked that caller, and two instances of one walk land on two makers. Tempo's t2000p
//! under Pooled (2026-11-21) passed 80 tests where Serial passed 301: facts the lazy program
//! filed as a side effect of ANOTHER body's check (a trait object's table from that body's
//! `dyn` principals, fill.rs `body_objects`) were read by a make that never asked for them.
//!
//! The claim, on real programs over NAMES RECORDS (rule 7): every instance a walk makes is made
//! again, by fresh makers, (1) in the reverse order (callees first), (2) round-robin over two
//! makers (two workers), and (3) each ALONE on a maker of its own (cold), and every one of
//! those makes gives the SAME RECORD BYTES as the walk's. A record holding an id that depends
//! on what the maker made before (a ground table's `PTy`, a layout's slot) is two values under
//! one key (rule 21) and fails here, naming the instance and the first byte that differs.
//!
//! The warm run: a second walk over the first's store, on a fresh maker, finds every caller in
//! the store and makes one callee (its key left out of the stand-in's answers, so it is made on
//! every walk): its bytes equal the cold walk's.
//!
//! The programs:
//! - THE STD WORLD ([`STD_PROGRAM`]): a `#![no_std]` crate over core's records (a dependency
//!   OVER RECORDS: tests/toolchain_store, the test store under the cargo target dir): a
//!   `#[track_caller]` fn reading `Location::caller()`, a call through `&mut dyn FnMut` (core's
//!   `FnMut` object table: tempo's `NoTrait` for `core::ops::FnMut::call_mut`), a call through
//!   `&mut F` as `F: FnMut` (core's `impl FnMut for &mut F`), and a generic at two types.
//! - THE DYN RUNG (blaze-cli tests/ladder/front2/x0-dyn-01-provided-override, `#![no_core]`):
//!   provided and required methods through `&dyn Tr`, `&mut dyn Tr` and `&dyn Sub`.
//!
//! The control (rule 23): each alone-make is on a maker that constructed its program for that
//! make and checked fewer bodies than the walk's maker (it made no caller), so "alone" is cold;
//! and the comparison is shown to see a byte moved (a planted one-byte change is reported).
//!
//! Prints: `make order: <case>: <n> instances, reversed/two makers/alone equal; walk checks <c>,
//! alone checks <min>..<max>; warm: <counts>`

mod toolchain_store;

use std::collections::HashMap;

use blaze_emit::{Isa, Os};
use blaze_ids::Hasher;
use blaze_ids::producer::{CodeBuilder, ProducerId, Producers};
use blaze_ladder::body_ids::{BodyId, BodyIds, Counts, Fixture as Grant, Make, Read};
use blaze_ladder::code_records::reads::Verdict;
use blaze_ladder::code_records::{Builder, MemoryStore, Settings, TierName};
use blaze_ladder::crates::UserCrate;
use blaze_ladder::front2::keys::checker_of;
use blaze_ladder::front2::source::{NamesSource, Records};
use blaze_ladder::run::make_one::{Maker, NamesRecords, walk_records};
use blaze_ladder::run::{OverflowChecks, RunError};
use blaze_own::{AVec, Alloc};
use blaze_surface::ItemKind;
use blaze_toolchain::ToolchainId;
use blaze_typeck::ItemKey;

use toolchain_store::{front_over_records, shelf_dir};

/// No `#[panic_handler]` of its own: the records hold the toolchain's crates, std among them,
/// whose handler is the program's (a second is E0152, refused by front2 as "declared twice").
const STD_PROGRAM: &str = "#![no_std]
#![no_main]

#[track_caller]
fn here() -> u32 {
    core::panic::Location::caller().line()
}

fn via_dyn(f: &mut dyn FnMut(u8) -> u8, x: u8) -> u8 {
    f(x)
}

fn via_ref<F: FnMut(u8) -> u8>(mut f: F, x: u8) -> u8 {
    f(x)
}

fn pick<T>(c: bool, a: T, b: T) -> T {
    if c { a } else { b }
}

#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {
    let mut seen = 0u8;
    let mut add = |x: u8| {
        seen = seen.wrapping_add(x);
        seen
    };
    let a = via_dyn(&mut add, 2);
    let b = via_ref(&mut add, 3);
    let line = here();
    let small: u8 = pick(true, a, b);
    let wide: u32 = pick(false, line, 7);
    small as i32 + wide as i32
}
";

const DIR: &str = "/make-order/";
const ROOT: &str = "/make-order/src/main.rs";

fn alloc() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

fn builder(panic: blaze_build::Panic) -> Builder {
    Builder {
        isa: Isa { lse: true, os: Os::MacOs },
        tier: TierName::Reference,
        settings: Settings::baseline(OverflowChecks::Off, panic),
        toolchain: ToolchainId([7; 32]),
        producer: producer(),
        edition: edition::PackageEdition::of_manifest("2024").expect("2024 is an edition"),
    }
}

fn producer() -> ProducerId<CodeBuilder> {
    Producers::for_tests(b"make order").get::<CodeBuilder>()
}

/// What a walk row made: a fn instance, a static's datum or a thread-local's template.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum What {
    Fn,
    Static,
    ThreadLocal,
}

/// One make: what, of which item, under which instance name (empty for a datum), and its bytes.
#[derive(Clone, Debug)]
struct Made {
    what: What,
    tabled: ItemKey,
    name: Vec<u8>,
    bytes: Vec<u8>,
}

impl Made {
    fn key(&self) -> (What, ItemKey, Vec<u8>) {
        (self.what, self.tabled, self.name.clone())
    }
}

/// A maker that keeps every entry it makes, in the order asked, and the walk's grant for the
/// makes after the walk (the grant is the walk's: made only in body_ids.rs).
struct Recording<M> {
    inner: M,
    made: Vec<Made>,
    grant: Option<Grant>,
}

impl<M: Make<Error = RunError, Grant = Grant>> Recording<M> {
    fn keep(&mut self, grant: &Grant, what: What, tabled: ItemKey, name: &[u8], entry: &AVec<'_, u8>, start: usize) {
        self.grant = Some(*grant);
        self.made.push(Made { what, tabled, name: name.to_vec(), bytes: entry.as_slice()[start..].to_vec() });
    }
}

impl<M: Make<Error = RunError, Grant = Grant>> Make for Recording<M> {
    type Error = RunError;
    type Grant = Grant;

    fn make(&mut self, grant: &Grant, tabled: ItemKey, name: &[u8], entry: &mut AVec<'_, u8>) -> Result<(), RunError> {
        let start = entry.len();
        self.inner.make(grant, tabled, name, entry)?;
        self.keep(grant, What::Fn, tabled, name, entry, start);
        Ok(())
    }

    fn make_static(&mut self, grant: &Grant, item: ItemKey, entry: &mut AVec<'_, u8>) -> Result<(), RunError> {
        let start = entry.len();
        self.inner.make_static(grant, item, entry)?;
        self.keep(grant, What::Static, item, &[], entry, start);
        Ok(())
    }

    fn make_thread_local(&mut self, grant: &Grant, item: ItemKey, entry: &mut AVec<'_, u8>) -> Result<(), RunError> {
        let start = entry.len();
        self.inner.make_thread_local(grant, item, entry)?;
        self.keep(grant, What::ThreadLocal, item, &[], entry, start);
        Ok(())
    }

    fn verify(&mut self, tabled: ItemKey, name: &[u8], entry: &[u8]) -> Result<Option<Verdict>, RunError> {
        self.inner.verify(tabled, name, entry)
    }
}

const READS: [Read; 1] = [Read { fact: [3; 32], digest: [4; 32] }];

/// The command's BodyCheck keys' stand-in: every item keyed by a digest of its key, so every
/// instance is keyed and two walks agree; `left_out`'s fn has no BodyCheck record (its body id
/// reads nothing: body_ids.rs `Unkeyed::NoRecord`, as tests/d106_records.rs `TextIds` when not
/// recorded), so it is unkeyed and made on every walk. Never `None` for it: that says the
/// command has no such fn, which the walk refuses (`WalkError::NoBody`).
struct EveryItemBut {
    left_out: Option<ItemKey>,
}

impl BodyIds for EveryItemBut {
    fn body_check(&self, item: ItemKey) -> Option<BodyId<'_>> {
        let key = Hasher::new("make order every item").part(&item.0.to_le_bytes()[..]).finish();
        let reads: &[Read] = if self.left_out == Some(item) { &[] } else { &READS };
        Some(BodyId { key, reads })
    }

    fn today(&self, fact: &[u8; 32]) -> Option<[u8; 32]> {
        (*fact == READS[0].fact).then_some(READS[0].digest)
    }

    fn crate_fact(&self, _item: ItemKey) -> Option<Read> {
        Some(Read { fact: [8; 32], digest: [9; 32] })
    }
}

/// A program a test makes over names records: each call is a FRESH reader (one worker's own,
/// as blaze-cli test_drive.rs `SuiteMakers` opens one per worker), never shared with another
/// maker, so no maker's history reaches another's through the reader's memo.
trait Program {
    fn reader(&self) -> NamesRecords<'static>;
    fn panic(&self) -> blaze_build::Panic;
}

/// The std world's program over core's records (tests/toolchain_store).
struct StdWorld {
    user: &'static UserCrate<'static>,
    shelf: String,
}

impl Program for StdWorld {
    fn reader(&self) -> NamesRecords<'static> {
        let over = front_over_records(&self.shelf, self.user, alloc()).unwrap_or_else(|e| panic!("the std world: {e}"));
        NamesRecords { records: over.records, frame: over.frame, lang_crates: over.lang_crates, body_sources: None }
    }

    fn panic(&self) -> blaze_build::Panic {
        self.user.panic
    }
}

fn std_world(text: &str) -> StdWorld {
    let files: &'static [(String, String)] = Vec::leak(vec![(ROOT.to_string(), text.to_string())]);
    let edition = edition::PackageEdition::of_manifest("2024").expect("2024 is an edition");
    let user: &'static UserCrate<'static> =
        Box::leak(Box::new(UserCrate { name: "make_order", dir: DIR, root: ROOT, files, edition, panic: blaze_build::Panic::Unwind }));
    StdWorld { user, shelf: shelf_dir() }
}

/// A `#![no_core]` program solved alone, its records kept (no toolchain).
struct Alone {
    kept: Kept,
    keys: &'static [blaze_id::CrateItemKey],
    crate_names: &'static [&'static str],
}

/// The crates that can declare a lang item in a program solved alone: its own.
const LANG_CRATES: [usize; 1] = [0];

impl Program for Alone {
    fn reader(&self) -> NamesRecords<'static> {
        let a = alloc();
        let frame: &'static blaze_mem::Frame = Box::leak(Box::new(blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the reader's frame")));
        let log: &'static blaze_ladder::front2::source::Log<'static> = Box::leak(Box::new(blaze_ladder::front2::source::Log::new(self.crate_names, a)));
        let load: &'static Kept = Box::leak(Box::new(self.kept.clone()));
        let records: &'static Records<'static> = Box::leak(Box::new(Records::new(self.keys, load, log, frame, a)));
        NamesRecords { records, frame, lang_crates: &LANG_CRATES, body_sources: None }
    }

    fn panic(&self) -> blaze_build::Panic {
        blaze_build::Panic::Abort
    }
}

/// The records and texts as kept after the solve (as tests/d106_records.rs keeps them).
#[derive(Clone, Default)]
struct Kept {
    names: Vec<&'static [u8]>,
    items: Vec<&'static [u8]>,
    files: Vec<HashMap<String, &'static str>>,
    expansions: Vec<HashMap<blaze_id::ItemKey, &'static str>>,
    origins: Vec<HashMap<blaze_id::ItemKey, &'static [u8]>>,
}

impl blaze_ladder::front2::source::Load<'static> for Kept {
    fn names(&self, krate: usize) -> Option<&'static [u8]> {
        self.names.get(krate).copied()
    }

    fn items(&self, krate: usize) -> Option<&'static [u8]> {
        self.items.get(krate).copied()
    }

    fn file(&self, krate: usize, path: blaze_scope::package_path::PackagePath<'_>) -> Option<&'static str> {
        self.files.get(krate)?.get(path.as_str()).copied()
    }

    fn expansion(&self, krate: usize, call: blaze_id::ItemKey) -> Option<blaze_ladder::front2::source::LoadedExpansion<'static>> {
        let text = self.expansions.get(krate)?.get(&call).copied()?;
        // Kept in the joined form (as tests/d106_records.rs): empty, the text keeps none.
        let origins = self.origins.get(krate).and_then(|by_call| by_call.get(&call)).copied().map(|bytes| {
            if bytes.is_empty() {
                (&[][..], &[][..])
            } else {
                blaze_scope::expansion_origins::split(bytes).unwrap_or_else(|e| panic!("the kept origins of {call:?}: {e}"))
            }
        });
        Some(blaze_ladder::front2::source::LoadedExpansion { text, origins })
    }
}

/// The origin column an expansion's text keeps, as a loader serves it: empty for a text that
/// keeps none by design (as tests/d106_records.rs `made_origins`).
fn made_origins(call: blaze_id::ItemKey, origins: &Result<&[u8], blaze_scope::expansion_origins::OriginsError>) -> &'static [u8] {
    use blaze_scope::expansion_origins::OriginsError;
    match origins {
        Ok(bytes) => Box::leak(bytes.to_vec().into_boxed_slice()),
        Err(OriginsError::Rewritten { .. } | OriginsError::Unplaced { .. }) => &[],
        Err(e) => panic!("the origins of expansion {call:x}: {e:?}"),
    }
}

fn leak(s: &str) -> &'static str {
    Box::leak(s.to_string().into_boxed_str())
}

/// `files` (each by its path under [`DIR`], [`ROOT`] among them) solved once, alone, under
/// `panic = abort`; each crate's two records written from the solve.
fn alone(files: &[(String, String)]) -> Alone {
    use blaze_ladder::crates::{Inputs, Toolchain};
    use blaze_scope::resolve::aligned;
    use blaze_scope::{items_record, names_record};
    let a = alloc();
    let edition = edition::PackageEdition::of_manifest("2024").expect("2024 is an edition");
    let user = UserCrate { name: "make_order", dir: DIR, root: ROOT, files, edition, panic: blaze_build::Panic::Abort };
    let tc = Toolchain::none(a);
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let keys: &'static [blaze_id::CrateItemKey] = Vec::leak(crates.iter().map(|c| c.key).collect());
    let crate_names: &'static [&'static str] = Vec::leak(crates.iter().map(|c| leak(c.name)).collect());
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("the one solve: {e}"));
    let mut kept = Kept::default();
    for c in names.crates().iter() {
        kept.names.push(aligned(&names_record::encode(c, a).unwrap_or_else(|e| panic!("the names record of `{}`: {e:?}", c.name)), blaze_own::testing::bump()));
        kept.items.push(aligned(&items_record::encode_with(c, &names, a).unwrap_or_else(|e| panic!("the items record of `{}`: {e:?}", c.name)), blaze_own::testing::bump()));
        kept.files.push(c.files.iter().map(|f| (f.package.as_str().to_string(), leak(f.text))).collect());
        kept.expansions.push(c.expansions.iter().filter_map(|(call, e)| e.made.map(|m| (*call, leak(m.text)))).collect());
        kept.origins.push(c.expansions.iter().filter_map(|(call, e)| e.made.as_ref().map(|m| (*call, made_origins(*call, &m.origins)))).collect());
    }
    Alone { kept, keys, crate_names }
}

/// The ladder rung x0-dyn-01, its files unchanged.
fn dyn_rung() -> Vec<(String, String)> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../blaze-cli/tests/ladder/front2/x0-dyn-01-provided-override/src");
    ["main.rs", "prelude.rs"]
        .iter()
        .map(|f| (format!("{DIR}src/{f}"), std::fs::read_to_string(dir.join(f)).unwrap_or_else(|e| panic!("the dyn rung's {f}: {e}"))))
        .collect()
}

/// The checker key of crate 0's fn named `name`.
fn fn_named(records: &'static Records<'static>, name: &str) -> ItemKey {
    let mut found = None;
    assert!(
        records.each_item(0, &mut |key, _| if found.is_none() && records.kind_of_item(key) == Some(ItemKind::Function) && records.name_of(key) == Some(name) {
            found = Some(checker_of(key));
        }),
        "the program's items record lists its items"
    );
    found.unwrap_or_else(|| panic!("no fn named `{name}` in crate 0"))
}

/// What the records' log holds, as text.
fn logged(records: &Records<'_>) -> Vec<String> {
    records.errors().iter().map(|e| e.to_string()).collect()
}

/// One walk from `main` over `store` on a fresh reader and maker, every entry kept in the
/// walk's order (callers first): its counts, the entries, the walk's grant and its checks.
fn walk(p: &dyn Program, ids: &dyn BodyIds, store: &mut MemoryStore<'static>) -> (Counts, Vec<Made>, Option<Grant>, usize) {
    let names = p.reader();
    let entry = fn_named(names.records, "main");
    let b = builder(p.panic());
    let mut rec = Recording { inner: Maker::new(names, Some(entry), &b, alloc()), made: Vec::new(), grant: None };
    let (_, counts) = walk_records(entry, ids, store, &b, &producer(), &mut rec, alloc()).unwrap_or_else(|e| {
        println!("the record walk: {e}; the records filed {:?}", logged(names.records));
        panic!("the record walk: {e}")
    });
    assert!(logged(names.records).is_empty(), "the walk's records filed {:?}", logged(names.records));
    (counts, rec.made, rec.grant, rec.inner.checks().check_one)
}

/// A fresh maker over a fresh reader of `p`, for the program whose entry is `main`.
fn fresh(p: &dyn Program) -> (Maker<'static>, NamesRecords<'static>) {
    let names = p.reader();
    let entry = fn_named(names.records, "main");
    (Maker::new(names, Some(entry), &builder(p.panic()), alloc()), names)
}

/// `m` made again by `maker`: its bytes, or the refusal's words.
fn make_again(maker: &mut Maker<'static>, grant: &Grant, m: &Made) -> Result<Vec<u8>, String> {
    let mut entry: AVec<'_, u8> = blaze_own::Vec::new_in(alloc());
    let made = match m.what {
        What::Fn => maker.make(grant, m.tabled, &m.name, &mut entry),
        What::Static => maker.make_static(grant, m.tabled, &mut entry),
        What::ThreadLocal => maker.make_thread_local(grant, m.tabled, &mut entry),
    };
    made.map(|()| entry.as_slice().to_vec()).map_err(|e| e.to_string())
}

/// Where `got` first differs from `want`, as words; `None`: equal.
fn differs(want: &[u8], got: &[u8]) -> Option<String> {
    if want == got {
        return None;
    }
    let at = want.iter().zip(got.iter()).position(|(w, g)| w != g).unwrap_or(want.len().min(got.len()));
    Some(format!("{} bytes against the walk's {}, first difference at byte {at}", got.len(), want.len()))
}

/// Every failure of one order, each naming the instance (rule 19).
fn check_order(case: &str, order: &str, walked: &[Made], got: &[(usize, Result<Vec<u8>, String>)], failures: &mut Vec<String>) {
    for (i, result) in got {
        let m = &walked[*i];
        let what = format!("{case}: {order}: {:?} {:#x} (name {} bytes)", m.what, m.tabled.0, m.name.len());
        match result {
            Ok(bytes) => {
                if let Some(diff) = differs(&m.bytes, bytes) {
                    failures.push(format!("{what}: TWO VALUES UNDER ONE KEY (rule 21): {diff}"));
                }
            }
            Err(why) => failures.push(format!("{what}: refused when made in this order: {why}")),
        }
    }
}

/// The claim of the module doc for `p`; `left_out`: the callee the warm walk makes.
fn same_bytes_in_every_order(case: &str, p: &dyn Program, left_out: &str) {
    let mut store = MemoryStore::new_in(alloc());
    let (counts, walked, grant, walk_checks) = walk(p, &EveryItemBut { left_out: None }, &mut store);
    let grant = grant.unwrap_or_else(|| panic!("{case}: the walk made nothing: {counts}"));
    assert!(walked.len() > 1 && counts.found == 0, "{case}: a cold walk makes every entry it reaches: {counts}, {} kept", walked.len());
    let mut keys = std::collections::HashSet::new();
    for m in walked.iter() {
        assert!(keys.insert(m.key()), "{case}: the walk made {:?} {:#x} twice", m.what, m.tabled.0);
    }
    let mut failures = Vec::new();

    // (1) Reversed: callees first, on one fresh maker.
    let (mut maker, _) = fresh(p);
    let reversed: Vec<_> = (0..walked.len()).rev().map(|i| (i, make_again(&mut maker, &grant, &walked[i]))).collect();
    check_order(case, "reversed", &walked, &reversed, &mut failures);

    // (2) Two makers, round-robin in the walk's order (two workers of the pool).
    let (mut one, _) = fresh(p);
    let (mut two, _) = fresh(p);
    let pooled: Vec<_> = (0..walked.len()).map(|i| (i, make_again(if i % 2 == 0 { &mut one } else { &mut two }, &grant, &walked[i]))).collect();
    check_order(case, "two makers", &walked, &pooled, &mut failures);

    // (3) Each alone, cold, on a maker of its own.
    let mut alone_checks = Vec::new();
    let mut alone = Vec::new();
    for i in 0..walked.len() {
        let (mut maker, names) = fresh(p);
        alone.push((i, make_again(&mut maker, &grant, &walked[i])));
        let checks = maker.checks();
        // The control: the alone maker constructed its program for this make and made no caller
        // (a refusal is said with the others, below).
        if alone[i].1.is_ok() {
            assert_eq!(checks.constructed, 1, "{case}: control: the alone make of {:#x} constructed one program: {checks:?}", walked[i].tabled.0);
            assert!(logged(names.records).is_empty(), "{case}: the alone make of {:#x} filed {:?}", walked[i].tabled.0, logged(names.records));
        }
        alone_checks.push(checks.check_one);
    }
    check_order(case, "alone", &walked, &alone, &mut failures);
    let (least, most) = (alone_checks.iter().min().copied().unwrap_or(0), alone_checks.iter().max().copied().unwrap_or(0));
    assert!(least < walk_checks, "{case}: control: some alone make checked fewer bodies ({least}) than the walk ({walk_checks}), so it is cold");

    // The comparison sees a moved byte (its own control).
    let mut planted = walked[0].bytes.clone();
    if let Some(last) = planted.last_mut() {
        *last ^= 1;
    }
    assert!(differs(&walked[0].bytes, &planted).is_some(), "{case}: control: a planted one-byte change is seen");

    // The warm run: callers found in the store, one callee made, on a fresh maker.
    let (_, first) = fresh(p);
    let callee = fn_named(first.records, left_out);
    let mut warm_store = MemoryStore::new_in(alloc());
    let ids = EveryItemBut { left_out: Some(callee) };
    walk(p, &ids, &mut warm_store);
    let (warm, warm_made, _, _) = walk(p, &ids, &mut warm_store);
    assert!(warm.found > 0 && warm.no_record > 0, "{case}: the warm walk finds callers and makes `{left_out}`: {warm}");
    let by_key: HashMap<_, _> = walked.iter().map(|m| (m.key(), &m.bytes)).collect();
    for m in warm_made.iter() {
        match by_key.get(&m.key()) {
            Some(cold) => {
                if let Some(diff) = differs(cold, &m.bytes) {
                    failures.push(format!("{case}: warm: {:?} {:#x}: made after its callers were found: TWO VALUES UNDER ONE KEY (rule 21): {diff}", m.what, m.tabled.0));
                }
            }
            None => failures.push(format!("{case}: warm: {:?} {:#x} was made and the cold walk made no such instance", m.what, m.tabled.0)),
        }
    }

    assert!(failures.is_empty(), "{case}: {} makes depend on what was made before them:\n{}", failures.len(), failures.join("\n"));
    println!(
        "make order: {case}: {} instances, reversed/two makers/alone equal; walk checks {walk_checks}, alone checks {least}..{most}; warm: {warm}",
        walked.len()
    );
}

/// The std world's program (the module doc): a dependency over records, `#[track_caller]`,
/// `&mut dyn FnMut` and `&mut F`.
#[test]
fn a_std_world_programs_records_are_the_same_in_every_make_order() {
    same_bytes_in_every_order("std world", &std_world(STD_PROGRAM), "via_dyn");
}

/// The dyn rung: trait objects' tables made for a make that never checked the body naming them.
#[test]
fn a_dyn_rungs_records_are_the_same_in_every_make_order() {
    same_bytes_in_every_order("dyn rung", &alone(&dyn_rung()), "m_of");
}
