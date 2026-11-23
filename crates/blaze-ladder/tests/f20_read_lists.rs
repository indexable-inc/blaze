//! F20: a found code entry is taken only when its read list holds today. Every question the
//! live build asks goes through the one recorder (run/make_one.rs `Asking` over code_reads.rs
//! `Recorded`), the entry stores that list, and the walk asks it again of today's program
//! before taking the entry (run/make_one.rs `verify_instance`), on the real path: the lazy
//! program over NAMES RECORDS (`Maker`, `walk_records`), as tests/d106_records.rs walks it.
//!
//! THE STAND-IN ([`TextIds`]): each fn's and static's BodyCheck key is a digest of its OWN text,
//! every crate fact constant. So an edit of a dependency (a struct's field type, a constant's
//! value, a static's kind, a callee's ABI) moves no key of the fn that reads it: only its read
//! list can see the edit. That is the class each case tests.
//!
//! THE ORACLE: after the edit, the walk over the store kept from before is compared with a walk
//! over an EMPTY store of the edited text: every instance's entry must be under the same
//! content key (equal bytes: equal code, equal read list). A stale entry taken is a key that
//! differs. Each case also says the reader was made again (`stale_reads`, its row made).
//!
//! THE CONTROL (rule 23): the recorder planted to record no layout question
//! (`ReadPlant::SkipsLayouts`): the field-type edit is then missed, the stale entry is found
//! holding, and the oracle catches its key. TWINS: an edit nothing reads remakes nothing.

use std::collections::HashMap;

use blaze_emit::{Isa, Os};
use blaze_id::{CrateItemKey, ItemKey as LibKey};
use blaze_ids::producer::{CodeBuilder, ProducerId, Producers};
use blaze_ids::{ContentId, Hasher};
use blaze_ladder::body_ids::{BodyId, BodyIds, Counts, How, Read, Stepped};
use blaze_ladder::code_records::reads::{ReadKind, ReadPlant};
use blaze_ladder::code_records::{Builder, CodeKey, CodeStore, EntryView, MemoryStore, Settings, TierName};
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
/// The crates that can declare a lang item: the program's own, the only one.
const LANG_CRATES: [usize; 1] = [0];

/// The no_core head of every program (tests/d106_records.rs's: `main` is "C-unwind", so the
/// program needs no `panic_cannot_unwind`).
const HEAD: &str = "#![feature(no_core, lang_items)]\n#![no_core]\n#![no_main]\n\n#[lang = \"sized\"]\ntrait Sized {}\n\n#[lang = \"copy\"]\ntrait Copy {}\n\n";

fn builder() -> Builder {
    Builder {
        isa: Isa { lse: true, os: Os::MacOs },
        tier: TierName::Reference,
        settings: Settings::baseline(OverflowChecks::Off, blaze_build::Panic::Unwind),
        toolchain: ToolchainId([7; 32]),
        producer: producer(),
        edition: edition::PackageEdition::of_manifest("2024").expect("2024 is an edition"),
    }
}

fn producer() -> ProducerId<CodeBuilder> {
    Producers::for_tests(b"f20 read lists").get::<CodeBuilder>()
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

/// `HEAD` + `body` solved once, its two records written from the solve.
fn fixture(body: &str) -> Fixture {
    let a = alloc();
    let files = [(String::from(ROOT), format!("{HEAD}{body}"))];
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

const READS: [Read; 1] = [Read { fact: [3; 32], digest: [4; 32] }];

/// The command's stand-in (the module doc): each free fn's and static's key from its own text.
struct TextIds {
    rows: Vec<(ItemKey, ContentId)>,
    entry: ItemKey,
}

impl BodyIds for TextIds {
    fn body_check(&self, item: ItemKey) -> Option<BodyId<'_>> {
        let n = self.rows.binary_search_by_key(&item, |r| r.0).ok()?;
        Some(BodyId { key: self.rows[n].1, reads: &READS })
    }

    fn today(&self, fact: &[u8; 32]) -> Option<[u8; 32]> {
        (*fact == READS[0].fact).then_some(READS[0].digest)
    }

    fn crate_fact(&self, _item: ItemKey) -> Option<Read> {
        Some(Read { fact: [8; 32], digest: [9; 32] })
    }
}

fn ids_of(f: &Fixture) -> TextIds {
    let records = reader(f).records;
    let mut libs: Vec<LibKey> = Vec::new();
    assert!(records.each_item(0, &mut |key, _| libs.push(key)), "the program's items record lists its items");
    let mut rows = Vec::new();
    let mut entry = None;
    for &lib in libs.iter() {
        let kind = records.kind_of_item(lib);
        if !matches!(kind, Some(ItemKind::Function | ItemKind::Static)) || records.member_owner(lib).is_some() {
            continue;
        }
        let text = records.text_of(lib).unwrap_or_else(|| panic!("item {lib:x} has a text: {:?}", logged(records)));
        rows.push((checker_of(lib), Hasher::new("f20 test body check").part(text.text.as_bytes()).finish()));
        if kind == Some(ItemKind::Function) && records.name_of(lib) == Some("main") {
            entry = Some(checker_of(lib));
        }
    }
    assert!(logged(records).is_empty(), "the stand-in's reads: {:?}", logged(records));
    rows.sort_by_key(|r| r.0);
    TextIds { rows, entry: entry.expect("the program has a `main`") }
}

/// The checker key of the free fn named `name`.
fn fn_named(f: &Fixture, name: &str) -> ItemKey {
    let records = reader(f).records;
    let mut found = None;
    assert!(
        records.each_item(0, &mut |key, _| if found.is_none() && records.kind_of_item(key) == Some(ItemKind::Function) && records.name_of(key) == Some(name) {
            found = Some(checker_of(key));
        }),
        "the program's items record lists its items"
    );
    found.unwrap_or_else(|| panic!("no fn named `{name}`: {:?}", logged(records)))
}

/// One walked row: its fn (or static), its instance name, its content key, whether found.
struct Row {
    tabled: ItemKey,
    name: Vec<u8>,
    key: Option<CodeKey>,
    found: bool,
}

struct Walked {
    counts: Counts,
    rows: Vec<Row>,
}

impl Walked {
    /// The one row of fn `tabled` (each case's reader has one instance).
    fn of(&self, tabled: ItemKey) -> &Row {
        let mut of = self.rows.iter().filter(|r| r.tabled == tabled);
        let row = of.next().unwrap_or_else(|| panic!("no row of {:#x}", tabled.0));
        assert!(of.next().is_none(), "two rows of {:#x}", tabled.0);
        row
    }
}

/// One walk of `f` from `main` over `store`, a fresh reader and a fresh maker (one process), its
/// recorder planted with `plant`.
fn walk(f: &Fixture, store: &mut MemoryStore<'static>, plant: ReadPlant) -> Walked {
    let ids = ids_of(f);
    let names = reader(f);
    let b = builder();
    let mut maker = Maker::new(names, Some(ids.entry), &b, alloc());
    maker.plant_reads(plant);
    let (walk, counts) = walk_records(ids.entry, &ids, store, &b, &producer(), &mut maker, alloc()).unwrap_or_else(|e| panic!("the record walk: {e}; the records filed {:?}", logged(names.records)));
    assert!(logged(names.records).is_empty(), "the records filed {:?}", logged(names.records));
    let rows = (0..walk.len())
        .map(|row| Row { tabled: walk.tabled(row), name: walk.name(row).to_vec(), key: walk.key(row), found: matches!(walk.how(row), How::Done(Stepped::Found(_))) })
        .collect();
    Walked { counts, rows }
}

/// The rows of `fresh` whose entry `kept` holds under another key: stale code served.
fn stale_served(kept: &Walked, fresh: &Walked) -> Vec<String> {
    assert_eq!(kept.rows.len(), fresh.rows.len(), "the two walks reached the same instances");
    let mut stale = Vec::new();
    for r in fresh.rows.iter() {
        let k = kept.rows.iter().find(|k| k.tabled == r.tabled && k.name == r.name).unwrap_or_else(|| panic!("the kept walk reached no row of {:#x}", r.tabled.0));
        if k.key != r.key {
            stale.push(format!("{:#x} ({})", r.tabled.0, if k.found { "found" } else { "made" }));
        }
    }
    stale
}

/// What one edit did: the walk after it over the kept store, and over an empty one.
struct Edited {
    kept: Walked,
    fresh: Walked,
    before: Fixture,
    after: Fixture,
}

/// Walk `before` into a store, then `after` over that store (and over an empty one), each
/// recorder planted with `plant`; the fresh walk is never planted.
fn edit(before: &str, after: &str, plant: ReadPlant) -> Edited {
    assert_ne!(before, after, "the edit applies");
    let before = fixture(before);
    let after = fixture(after);
    let mut store = MemoryStore::new_in(alloc());
    walk(&before, &mut store, plant);
    let kept = walk(&after, &mut store, plant);
    let fresh = walk(&after, &mut MemoryStore::new_in(alloc()), ReadPlant::None);
    Edited { kept, fresh, before, after }
}

/// Edit ONLY a dependency of fn `reader`: its row made again over the kept store, its read list
/// the cause (`stale_reads`), every entry the fresh walk's. Returns the line it prints.
fn dependency_edit(what: &str, before: &str, after: &str, reader_fn: &str) -> String {
    let e = edit(before, after, ReadPlant::None);
    assert!(e.before.keys.len() == e.after.keys.len());
    let reader_key = fn_named(&e.after, reader_fn);
    let row = e.kept.of(reader_key);
    let stale = stale_served(&e.kept, &e.fresh);
    let line = format!("{what}: {}; `{reader_fn}` {}; stale served {stale:?}", e.kept.counts, if row.found { "FOUND" } else { "made again" });
    println!("{line}");
    assert!(!row.found, "{what}: `{reader_fn}`'s text did not move and only its read list sees the edit: it must be made again ({line})");
    assert!(e.kept.counts.stale_reads >= 1, "{what}: the reader was made again for its read list ({line})");
    assert!(stale.is_empty(), "{what}: an entry served is not the one today's text makes ({line})");
    line
}

/// A struct's field type (`u8` to `u64`): `get`'s `p.b` moves; only its `Adt` row sees it.
#[test]
fn a_dependency_field_type_remakes_its_reader() {
    let tail = "fn get(p: &P) -> i32 {\n    p.b\n}\n\nfn mk() -> P {\n    P { a: 1, b: 7 }\n}\n\n#[unsafe(no_mangle)]\npub extern \"C-unwind\" fn main() -> i32 {\n    let p = mk();\n    get(&p)\n}\n";
    dependency_edit("field type", &format!("struct P {{\n    a: u8,\n    b: i32,\n}}\n\n{tail}"), &format!("struct P {{\n    a: u64,\n    b: i32,\n}}\n\n{tail}"), "get");
}

/// A constant's value (`2` to `5`): `get`'s text reads `N`'s type only; the value is its
/// `Const` row.
#[test]
fn a_dependency_const_value_remakes_its_reader() {
    let tail = "fn get() -> usize {\n    N\n}\n\n#[unsafe(no_mangle)]\npub extern \"C-unwind\" fn main() -> i32 {\n    get() as i32\n}\n";
    dependency_edit("const", &format!("const N: usize = 2;\n\n{tail}"), &format!("const N: usize = 5;\n\n{tail}"), "get");
}

/// A static's kind (`static` to `static mut`): `get`'s read of it is its `Static` row.
#[test]
fn a_dependency_static_kind_remakes_its_reader() {
    let tail = "fn get() -> i32 {\n    unsafe { S }\n}\n\n#[unsafe(no_mangle)]\npub extern \"C-unwind\" fn main() -> i32 {\n    get()\n}\n";
    dependency_edit("static kind", &format!("static S: i32 = 3;\n\n{tail}"), &format!("static mut S: i32 = 3;\n\n{tail}"), "get");
}

/// A callee's signature: its ABI (`extern "C-unwind"` to Rust's), which `f`'s call reads as
/// the callee's `Abi` row; `f`'s text names `g` only.
#[test]
fn a_callee_abi_remakes_its_caller() {
    let tail = "fn f() -> i32 {\n    g(2)\n}\n\n#[unsafe(no_mangle)]\npub extern \"C-unwind\" fn main() -> i32 {\n    f()\n}\n";
    dependency_edit("callee abi", &format!("extern \"C-unwind\" fn g(x: i32) -> i32 {{\n    x + 1\n}}\n\n{tail}"), &format!("fn g(x: i32) -> i32 {{\n    x + 1\n}}\n\n{tail}"), "f");
}

/// A callee's by-value parameter growing past what registers pass (8 bytes to 32): the call in
/// `f` passes it otherwise; `f`'s and `g`'s texts are unchanged (`mk`'s moves).
#[test]
fn a_by_value_parameter_growing_remakes_the_caller() {
    let tail = "fn g(q: Q) -> i64 {\n    q.a\n}\n\nfn f() -> i64 {\n    g(mk())\n}\n\n#[unsafe(no_mangle)]\npub extern \"C-unwind\" fn main() -> i32 {\n    f() as i32\n}\n";
    dependency_edit(
        "by-value parameter",
        &format!("struct Q {{\n    a: i64,\n}}\n\nfn mk() -> Q {{\n    Q {{ a: 4 }}\n}}\n\n{tail}"),
        &format!("struct Q {{\n    a: i64,\n    b: i64,\n    c: i64,\n    d: i64,\n}}\n\nfn mk() -> Q {{\n    Q {{ a: 4, b: 0, c: 0, d: 0 }}\n}}\n\n{tail}"),
        "f",
    );
}

/// TWINS: a struct no reached code reads gaining a wider field, and a fn nothing calls added:
/// nothing is made, every entry found and verified.
#[test]
fn an_unrelated_edit_remakes_nothing() {
    let tail = "struct P {\n    a: u8,\n    b: i32,\n}\n\nfn get(p: &P) -> i32 {\n    p.b\n}\n\n#[unsafe(no_mangle)]\npub extern \"C-unwind\" fn main() -> i32 {\n    let p = P { a: 1, b: 7 };\n    get(&p)\n}\n";
    let e = edit(&format!("struct R {{\n    a: u8,\n}}\n\n{tail}"), &format!("struct R {{\n    a: u64,\n}}\n\nfn unused() -> i32 {{\n    0\n}}\n\n{tail}"), ReadPlant::None);
    let stale = stale_served(&e.kept, &e.fresh);
    let c = e.kept.counts;
    let line = format!("unrelated: {c}; stale served {stale:?}");
    println!("{line}");
    assert_eq!((c.made, c.stale_reads, c.found), (0, 0, c.reached), "an edit nothing reached reads remade something ({line})");
    assert!(c.verified >= 1, "the found entries' read lists were asked ({line})");
    assert!(stale.is_empty(), "{line}");
}

/// THE CONTROL: the field-type edit with the recorder planted to record no layout question:
/// `get`'s entry is found holding and its key is not the fresh walk's: stale code, caught.
#[test]
fn the_planted_recorder_serves_stale_code_and_the_oracle_catches_it() {
    let tail = "fn get(p: &P) -> i32 {\n    p.b\n}\n\nfn mk() -> P {\n    P { a: 1, b: 7 }\n}\n\n#[unsafe(no_mangle)]\npub extern \"C-unwind\" fn main() -> i32 {\n    let p = mk();\n    get(&p)\n}\n";
    let e = edit(&format!("struct P {{\n    a: u8,\n    b: i32,\n}}\n\n{tail}"), &format!("struct P {{\n    a: u64,\n    b: i32,\n}}\n\n{tail}"), ReadPlant::SkipsLayouts);
    let get = fn_named(&e.after, "get");
    let stale = stale_served(&e.kept, &e.fresh);
    let line = format!("planted (SkipsLayouts): {}; `get` {}; stale served {stale:?}", e.kept.counts, if e.kept.of(get).found { "found holding" } else { "made" });
    println!("{line}");
    assert!(e.kept.of(get).found, "the plant changed nothing: `get` was made again ({line})");
    assert!(stale.iter().any(|s| s.starts_with(&format!("{:#x} ", get.0))), "the oracle missed the stale `get` ({line})");
}

/// The read list holds the answers of the build that SUCCEEDED only: `pick::<E>` over a
/// `#[repr(u8)]` enum, made again (its text edited) in a process where `main`, which names `E`,
/// was found and not checked, so `E`'s head may be unmade at the first build's ask (the lazy
/// front then answers `repr` by default; the build is made again once the head is). Its entry
/// holds ONE `Repr` row, the fresh walk's (the right answer), and the next walk finds and
/// verifies every entry.
#[test]
fn a_rebuilt_make_keeps_one_repr_row_the_right_one() {
    let tail = "#[repr(u8)]\nenum E {\n    A = 3,\n    B = 9,\n}\n\nfn tag(e: &E) -> u8 {\n    match e {\n        E::A => 1,\n        E::B => 2,\n    }\n}\n\n#[unsafe(no_mangle)]\npub extern \"C-unwind\" fn main() -> i32 {\n    let e = pick(false, E::A, E::B);\n    tag(&e) as i32\n}\n";
    let before = format!("fn pick<T>(c: bool, a: T, b: T) -> T {{\n    if c {{ a }} else {{ b }}\n}}\n\n{tail}");
    let after = format!("fn pick<T>(c: bool, a: T, b: T) -> T {{\n    match c {{\n        true => a,\n        false => b,\n    }}\n}}\n\n{tail}");
    let before = fixture(&before);
    let after = fixture(&after);
    let mut store = MemoryStore::new_in(alloc());
    walk(&before, &mut store, ReadPlant::None);
    let kept = walk(&after, &mut store, ReadPlant::None);
    let mut fresh_store = MemoryStore::new_in(alloc());
    let fresh = walk(&after, &mut fresh_store, ReadPlant::None);
    let pick = fn_named(&after, "pick");
    assert!(!kept.of(pick).found, "`pick`'s text moved: its instance is made again");
    let repr_rows = |store: &MemoryStore<'static>, row: &Row| -> Vec<(Vec<u8>, ContentId)> {
        let key = row.key.unwrap_or_else(|| panic!("`pick::<E>` has a key"));
        let bytes = store.get(key).unwrap_or_else(|e| panic!("the store: {e:?}")).unwrap_or_else(|| panic!("`pick::<E>`'s entry is in the store"));
        let view = EntryView::read(bytes).unwrap_or_else(|e| panic!("`pick::<E>`'s entry: {e:?}"));
        view.reads()
            .map(|r| r.unwrap_or_else(|e| panic!("a read-list row: {e:?}")))
            .filter(|(q, _)| q.first().copied().and_then(ReadKind::of) == Some(ReadKind::Repr))
            .map(|(q, d)| (q.to_vec(), d))
            .collect()
    };
    let made = repr_rows(&store, kept.of(pick));
    let right = repr_rows(&fresh_store, fresh.of(pick));
    let again = walk(&after, &mut store, ReadPlant::None);
    let line = format!("repr: {} Repr row(s) in `pick::<E>`'s entry, the fresh walk's: {}; next walk {}", made.len(), made == right, again.counts);
    println!("{line}");
    assert_eq!(made.len(), 1, "one `repr` question, asked once ({line})");
    assert_eq!(made, right, "the row is the answer today's program gives once `E`'s head is made ({line})");
    assert_eq!((again.counts.made, again.counts.found, again.counts.stale_reads), (0, again.counts.reached, 0), "the next walk finds and verifies every entry ({line})");
}
