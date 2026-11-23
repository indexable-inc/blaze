//! The rule of front2/intrinsic_rule.rs on a real run over NAMES RECORDS (the lazy program
//! `blaze run` builds each fn with on its first call: run/make_one.rs `Maker`, as
//! tests/d106_records.rs walks it), and whole through the new front end (`run_with`).
//!
//! The intrinsic is core's own `is_val_statically_known<T: Copy>(_arg: T) -> bool`, declared
//! here as the pinned core declares it (library/core/src/intrinsics/mod.rs, its fallback body
//! `false`), a name blaze-build's table does not hold (asserted, so a row added later fails
//! this test by name rather than changing what it shows):
//! - WITH its body, it is built from the body: `main` returns 7, the `false` arm.
//! - The control (rule 23, the cause removed): the same declaration with NO body (`;`) is
//!   refused when the walk reaches its call, naming the intrinsic, that it has no body, and the
//!   `file:line` where it is declared (rule 19).
//!
//! The helpers below are tests/d106_records.rs's (fixture, reader, builder, the every-item
//! stand-in), copied: a test file is its own crate and that file is shared by many lanes.

use std::collections::HashMap;

use blaze_emit::{Isa, Os};
use blaze_id::{CrateItemKey, ItemKey as LibKey};
use blaze_ids::Hasher;
use blaze_ids::producer::{CodeBuilder, ProducerId, Producers};
use blaze_ladder::body_ids::{BodyId, BodyIds, Read};
use blaze_ladder::code_records::{Builder, MemoryStore, Settings, TierName};
use blaze_ladder::crates::{Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::keys::checker_of;
use blaze_ladder::front2::source::{Load, LoadedExpansion, Log, NamesSource, Records};
use blaze_ladder::run::OverflowChecks;
use blaze_ladder::run::make_one::{Maker, NamesRecords, walk_records};
use blaze_own::Alloc;
use blaze_scope::resolve::aligned;
use blaze_scope::{items_record, names_record};
use blaze_surface::ItemKind;
use blaze_toolchain::ToolchainId;
use blaze_typeck::ItemKey;

const DIR: &str = "/ladder/";
const ROOT: &str = "/ladder/src/main.rs";
const OVERFLOW: OverflowChecks = OverflowChecks::Off;
/// The crates that can declare a lang item: the program's own, the only one.
const LANG_CRATES: [usize; 1] = [0];

/// The intrinsic both programs declare: core's, with a fallback body there.
const INTRINSIC: &str = "is_val_statically_known";

/// The program around `declaration`, the intrinsic's `fn` item (with its body, or `;`).
fn program(declaration: &str) -> String {
    format!(
        "#![feature(no_core, lang_items, intrinsics, rustc_attrs)]
#![no_core]
#![no_main]
#![allow(internal_features)]

#[lang = \"sized\"]
trait Sized {{}}

#[lang = \"copy\"]
trait Copy {{}}

impl Copy for u32 {{}}

#[rustc_intrinsic]
#[rustc_nounwind]
{declaration}

#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {{
    if {INTRINSIC}(5u32) {{ 1 }} else {{ 7 }}
}}
"
    )
}

/// As core declares it: the fallback body is `false`.
const WITH_BODY: &str = "const fn is_val_statically_known<T: Copy>(_arg: T) -> bool {\n    false\n}";
/// The control: the same declaration with no body.
const NO_BODY: &str = "const fn is_val_statically_known<T: Copy>(_arg: T) -> bool;";

fn isa() -> Isa {
    Isa { lse: true, os: Os::MacOs }
}

fn builder() -> Builder {
    Builder {
        isa: isa(),
        tier: TierName::Reference,
        settings: Settings::baseline(OVERFLOW, blaze_build::Panic::Unwind),
        toolchain: ToolchainId([7; 32]),
        producer: producer(),
        edition: edition::PackageEdition::of_manifest("2024").expect("2024 is an edition"),
    }
}

fn producer() -> ProducerId<CodeBuilder> {
    Producers::for_tests(b"d106 intrinsic fallback").get::<CodeBuilder>()
}

fn alloc() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

fn leak(s: &str) -> &'static str {
    Box::leak(s.to_string().into_boxed_str())
}

/// The records and texts as kept after the solve (tests/d106_records.rs `Kept`).
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

    fn expansion(&self, krate: usize, call: LibKey) -> Option<LoadedExpansion<'static>> {
        let text = self.expansions.get(krate)?.get(&call).copied()?;
        // The fixture keeps the origins in the joined form: its two parts, cut where the column
        // starts (empty: the text keeps none). None kept for the call: none served.
        let origins = self.origins.get(krate).and_then(|by_call| by_call.get(&call)).copied().map(|bytes| {
            if bytes.is_empty() {
                (&[][..], &[][..])
            } else {
                blaze_scope::expansion_origins::split(bytes).unwrap_or_else(|e| panic!("the fixture's origins of {call:?}: {e}"))
            }
        });
        Some(LoadedExpansion { text, origins })
    }
}

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

/// `text` solved once, each crate's two records written from the solve.
fn fixture(text: &str) -> Fixture {
    let files = [(String::from(ROOT), String::from(text))];
    let a = alloc();
    let edition = edition::PackageEdition::of_manifest("2024").expect("2024 is an edition");
    let user = UserCrate { name: "ladder", dir: DIR, root: ROOT, files: &files, edition, panic: blaze_build::Panic::Unwind };
    let tc = Toolchain::none(a);
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let keys: &'static [CrateItemKey] = Vec::leak(crates.iter().map(|c| c.key).collect());
    let crate_names: &'static [&'static str] = Vec::leak(crates.iter().map(|c| leak(c.name)).collect());
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("the fixture's one solve: {e}"));
    let mut kept = Kept::default();
    for c in names.crates().iter() {
        kept.names.push(aligned(&names_record::encode(c, a).unwrap_or_else(|e| panic!("the names record of `{}`: {e:?}", c.name)), blaze_own::testing::bump()));
        kept.items.push(aligned(&items_record::encode_with(c, &names, a).unwrap_or_else(|e| panic!("the items record of `{}`: {e:?}", c.name)), blaze_own::testing::bump()));
        kept.files.push(c.files.iter().map(|f| (f.package.as_str().to_string(), leak(f.text))).collect());
        kept.expansions.push(c.expansions.iter().filter_map(|(call, e)| e.made.map(|m| (*call, leak(m.text)))).collect());
        kept.origins.push(c.expansions.iter().filter_map(|(call, e)| e.made.as_ref().map(|m| (*call, made_origins(*call, &m.origins)))).collect());
    }
    Fixture { kept, keys, crate_names }
}

/// A FRESH reader of `f`'s records, as one `blaze run` process makes it.
fn reader(f: &Fixture) -> NamesRecords<'static> {
    let a = alloc();
    let frame: &'static blaze_mem::Frame = Box::leak(Box::new(blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the reader's frame")));
    let log: &'static Log<'static> = Box::leak(Box::new(Log::new(f.crate_names, a)));
    let load: &'static Kept = Box::leak(Box::new(f.kept.clone()));
    let records: &'static Records<'static> = Box::leak(Box::new(Records::new(f.keys, load, log, frame, a)));
    NamesRecords { records, frame, lang_crates: &LANG_CRATES, body_sources: None }
}

fn logged(records: &Records<'_>) -> Vec<String> {
    records.errors().iter().map(|e| e.to_string()).collect()
}

/// The checker key of crate 0's fn named `name`.
fn fn_named(f: &Fixture, name: &str) -> ItemKey {
    let records = reader(f).records;
    let mut found = None;
    assert!(records.each_item(0, &mut |key, _| if found.is_none() && records.kind_of_item(key) == Some(ItemKind::Function) && records.name_of(key) == Some(name) {
        found = Some(checker_of(key));
    }), "the program's items record lists its items");
    found.unwrap_or_else(|| panic!("no fn named `{name}`: {:?}", logged(records)))
}

const READS: [Read; 1] = [Read { fact: [3; 32], digest: [4; 32] }];

/// The command's BodyCheck keys, answered for EVERY item (tests/d106_records.rs `EveryItem`).
struct EveryItem;

impl BodyIds for EveryItem {
    fn body_check(&self, item: ItemKey) -> Option<BodyId<'_>> {
        Some(BodyId { key: Hasher::new("d106 intrinsic fallback every item").part(&item.0.to_le_bytes()[..]).finish(), reads: &READS })
    }

    fn today(&self, fact: &[u8; 32]) -> Option<[u8; 32]> {
        (*fact == READS[0].fact).then_some(READS[0].digest)
    }

    fn crate_fact(&self, _item: ItemKey) -> Option<Read> {
        Some(Read { fact: [8; 32], digest: [9; 32] })
    }
}

/// The walk of `text` over names records from `main`, every instance made: its counts, or the
/// refusal's text.
fn walk_text(text: &str) -> Result<String, String> {
    let f = fixture(text);
    let main = fn_named(&f, "main");
    let mut store = MemoryStore::new_in(alloc());
    let mut maker = Maker::new(reader(&f), Some(main), &builder(), alloc());
    walk_records(main, &EveryItem, &mut store, &builder(), &producer(), &mut maker, alloc()).map(|(_, counts)| counts.to_string()).map_err(|e| e.to_string())
}

/// `text` built by the new front end and run whole: its status, or the refusal's text.
fn run_whole(text: &str) -> Result<u8, String> {
    let files = [blaze_ladder::refusal::SourceFile { path: "src/main.rs", text }];
    let new = blaze_ladder::run::NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::Off };
    blaze_ladder::run::run_with(&files, "src/main.rs", None, blaze_ladder::run::FrontChoice::New(new), blaze_own::testing::heap().shared()).map_err(|e| e.to_string())
}

/// The test's premise: the table does not name the intrinsic (else rule 1, not rule 2, decides).
fn assert_unnamed() {
    assert!(
        blaze_build::intrinsics::Which::named(INTRINSIC).is_err(),
        "blaze-build's table now names `{INTRINSIC}`: this test shows the fallback-body rule only for a name the table lacks; pick another such intrinsic of core (tests/intrinsics_of_core.rs lists them)"
    );
}

/// An intrinsic the table does not name, declared with a body, is built from that body: the
/// walk over names records makes every instance, and the whole run returns 7, the `false` arm
/// of `main`'s `if`.
/// Prints "d106 intrinsic fallback body: walk <counts>; run status 7".
#[test]
fn an_unnamed_intrinsic_with_a_body_is_built_from_its_body() {
    assert_unnamed();
    let text = program(WITH_BODY);
    let counts = walk_text(&text).unwrap_or_else(|e| panic!("the walk refused the intrinsic with a fallback body: {e}"));
    let status = run_whole(&text).unwrap_or_else(|e| panic!("the whole run refused the intrinsic with a fallback body: {e}"));
    assert_eq!(status, 7, "`{INTRINSIC}` built from its body answers `false`");
    println!("d106 intrinsic fallback body: walk {counts}; run status {status}");
}

/// The control: the same declaration with NO body is refused when the walk reaches its call,
/// by the intrinsic's name, as having no body, at its declaration's `file:line`.
/// Prints "d106 intrinsic with no body: <the refusal>".
#[test]
fn an_unnamed_intrinsic_with_no_body_is_refused_naming_it_and_its_place() {
    assert_unnamed();
    let text = program(NO_BODY);
    // The declaration's place: the line of its `#[rustc_intrinsic]` (where its item's text
    // begins) or of its `fn` (1-based).
    let line_of = |needle: &str| text.lines().position(|l| l.trim() == needle).map(|i| i + 1).unwrap_or_else(|| panic!("the program has `{needle}`"));
    let (attr_line, fn_line) = (line_of("#[rustc_intrinsic]"), line_of(NO_BODY));
    let refusal = match walk_text(&text) {
        Ok(counts) => panic!("the walk made a body-less intrinsic the table does not name: {counts}"),
        Err(e) => e,
    };
    println!("d106 intrinsic with no body: {refusal}");
    assert!(refusal.contains(INTRINSIC), "the refusal names the intrinsic: {refusal}");
    assert!(refusal.contains("the build's table does not name"), "the refusal is the intrinsic one: {refusal}");
    assert!(refusal.contains("declared with no body"), "the refusal says it has no body: {refusal}");
    assert!(
        refusal.contains(&format!("main.rs:{attr_line}")) || refusal.contains(&format!("main.rs:{fn_line}")),
        "the refusal places the declaration at main.rs:{attr_line} (or :{fn_line}): {refusal}"
    );
    // Whole, the same program is refused too (the walk the run takes tables the fn left out).
    match run_whole(&text) {
        Ok(status) => panic!("the whole run built a body-less intrinsic the table does not name: status {status}"),
        Err(e) => assert!(e.contains(INTRINSIC), "the whole run's refusal names the intrinsic: {e}"),
    }
}
