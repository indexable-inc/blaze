//! A foreign fn (and a foreign static) declared in EVERY position an `extern` block can stand in,
//! each answered by its block's ABI over NAMES RECORDS and each walked and run, so one run of this
//! file finds the whole class "a fn with no body whose `extern` block the names source did not
//! answer" (std hello's stop of g201533, through `std::io::stdio::OUTPUT_CAPTURE`) instead of
//! one position per run (rule 82).
//!
//! The Reference, "External blocks": "External blocks provide declarations of items that are not
//! defined in the current crate"; [items.extern.abi.default] a bare `extern` is "C"; "Statements",
//! "Item declarations": an `extern` block is an item, so it may be written in a block (a fn's
//! body, a `const`'s or a `static`'s initializer) and by a macro's expansion, in item or
//! statement position or as the block of an expression.
//!
//! The positions, each its own test (a failure names its position) and all of them together in
//! [`every_position_in_one_program`]:
//! 1. module level, with each ABI spelling: "C", "C-unwind", "system", "system-unwind", a bare
//!    `unsafe extern` ([`abi_spellings_are_in_their_rows`]);
//! 2. an inline module;
//! 3. under `#[cfg(all())]`, with a `#[cfg(any())]` twin of another ABI that must not be read;
//! 4. written by a `macro_rules!` expansion at module level;
//! 5. inside a fn's body;
//! 6. written by an expansion in STATEMENT position inside a body (the body's block is then a
//!    module of the names library: blaze-scope expanded_blocks.rs `written`);
//! 7. inside a `const`'s initializer block, called through the fn pointer the constant holds;
//! 8. the same written by an expansion in EXPRESSION position (std's `thread_local_inner!`
//!    shape: `const OUTPUT_CAPTURE: LocalKey<..> = thread_local_inner!(..)`, whose block declares
//!    items and hands out a fn pointer);
//! 9. inside a `static`'s initializer block;
//! 10. a foreign STATIC declared in a body (heads.rs `fn_head`: its head is the block's).
//!
//! Each foreign fn is called only behind `if never() { .. }`: the checker keeps the branch, so
//! the head is made and the call is built, and no foreign code runs. Over the names records an
//! item with an items row (positions 1-4 and 6) must answer `extern_abi` with its block's ABI,
//! and a fn with a body `Some(None)`; an item declared in a body (5, 7-10) has no row (its key
//! is its body's lowering's, front2 locals.rs) and is answered by that lowering (`Locals::
//! foreign`), which the walk exercises. Every walk must leave the records' log empty and every
//! program must run to status 3.
//!
//! The fixture, reader and walk are tests/d106_records.rs's (one test file cannot use another's
//! helpers): the program is solved ONCE, its two records written, the solve dropped, and each
//! question asked of a FRESH `Records` over those bytes, as one `blaze run` process does.

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
/// The crates that can declare a lang item: the program's own, the only one.
const LANG_CRATES: [usize; 1] = [0];
/// The words of the refusal this file exists to find (front2/foreign_head.rs `UNANSWERED`).
const UNANSWERED: &str = "whose `extern` block the names source did not answer";

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
    Producers::for_tests(b"foreign abi everywhere").get::<CodeBuilder>()
}

fn alloc() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

fn leak(s: &str) -> &'static str {
    Box::leak(s.to_string().into_boxed_str())
}

/// The records and texts as kept after the solve (as tests/d106_records.rs `Kept`).
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

/// The origin column an expansion's text keeps, as a loader serves it: empty for a text that
/// keeps none by design.
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
    let a = alloc();
    let files = [(String::from(ROOT), String::from(text))];
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

/// What the records' log holds, as text.
fn logged(records: &Records<'_>) -> Vec<String> {
    records.errors().iter().map(|e| e.to_string()).collect()
}

const READS: [Read; 1] = [Read { fact: [3; 32], digest: [4; 32] }];

/// A stand-in for the command's BodyCheck keys that answers for every item (as
/// tests/d106_records.rs `EveryItem`).
struct EveryItem;

impl BodyIds for EveryItem {
    fn body_check(&self, item: ItemKey) -> Option<BodyId<'_>> {
        Some(BodyId { key: Hasher::new("foreign abi everywhere every item").part(&item.0.to_le_bytes()[..]).finish(), reads: &READS })
    }

    fn today(&self, fact: &[u8; 32]) -> Option<[u8; 32]> {
        (*fact == READS[0].fact).then_some(READS[0].digest)
    }

    fn crate_fact(&self, _item: ItemKey) -> Option<Read> {
        Some(Read { fact: [8; 32], digest: [9; 32] })
    }
}

/// The library keys of crate 0's items of `kind` named `name`, read by a fresh reader.
fn keys_named(f: &Fixture, kind: ItemKind, name: &str) -> Vec<LibKey> {
    let records = reader(f).records;
    let mut found = Vec::new();
    assert!(records.each_item(0, &mut |key, _| if records.kind_of_item(key) == Some(kind) && records.name_of(key) == Some(name) {
        found.push(key);
    }), "the program's items record lists its items");
    found
}

/// What a FRESH reader answers `extern_abi` of crate 0's ONE item of `kind` named `name`; its
/// log must stay empty.
fn abi_of(f: &Fixture, kind: ItemKind, name: &str) -> Option<Option<&'static str>> {
    let keys = keys_named(f, kind, name);
    let [key] = keys.as_slice() else { panic!("{} items of kind {kind:?} named `{name}` (one wanted)", keys.len()) };
    let records = reader(f).records;
    let abi = records.extern_abi(*key);
    assert!(logged(records).is_empty(), "`extern_abi` of `{name}` filed {:?}", logged(records));
    abi
}

/// The checker key of crate 0's free fn named `name`.
fn fn_named(f: &Fixture, name: &str) -> ItemKey {
    let keys = keys_named(f, ItemKind::Function, name);
    let [key] = keys.as_slice() else { panic!("{} fns named `{name}` (one wanted)", keys.len()) };
    checker_of(*key)
}

/// One walk of `f`'s program from `main` over a fresh reader and a fresh `Maker`: its counts, or
/// the walk's refusal with the records' log beside it.
fn try_walk(f: &Fixture) -> Result<String, String> {
    let main = fn_named(f, "main");
    let names = reader(f);
    let b = builder();
    let mut store = MemoryStore::new_in(alloc());
    let mut maker = Maker::new(names, Some(main), &b, alloc());
    match walk_records(main, &EveryItem, &mut store, &b, &producer(), &mut maker, alloc()) {
        Ok((_, counts)) if logged(names.records).is_empty() => Ok(counts.to_string()),
        Ok((_, counts)) => Err(format!("the walk ({counts}) left the records' log {:?}", logged(names.records))),
        Err(e) => Err(format!("the record walk: {e}; the records filed {:?}", logged(names.records))),
    }
}

/// The walk of [`try_walk`], which must succeed.
fn walk(f: &Fixture, position: &str) -> String {
    try_walk(f).unwrap_or_else(|e| {
        // Printed too: a failed run shows its test's own output first.
        println!("{position}: {e}");
        panic!("{position}: {e}")
    })
}

/// The program `text` run whole: its status.
fn run_whole(text: &str) -> u8 {
    let files = [blaze_ladder::refusal::SourceFile { path: "src/main.rs", text }];
    let new = blaze_ladder::run::NewFront { package: "ladder", edition: "2024", shelf: None, overflow: OverflowChecks::Off };
    blaze_ladder::run::run_with(&files, "src/main.rs", None, blaze_ladder::run::FrontChoice::New(new), blaze_own::testing::heap().shared())
        .unwrap_or_else(|e| panic!("the whole program's run: {e}"))
}

/// A `#![no_core]` program: the lang items it needs, `never()` (false at run time, a branch
/// the checker keeps), `items`, and a `main` that does `body` and returns `ret` (an `i32`
/// expression).
fn program(items: &str, body: &str, ret: &str) -> String {
    format!(
        "#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = \"sized\"]
trait Sized {{}}

#[lang = \"copy\"]
trait Copy {{}}

fn never() -> bool {{
    false
}}

{items}

#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {{
    {body}
    {ret}
}}
"
    )
}

/// One position: `items` and `body` as [`program`] writes them, returning 3. Each `(kind, name,
/// abi)` of `rows` is asked of the names records and must answer `abi`; `main` answers no block.
/// Then the program is walked over the records (no refusal, nothing filed) and run whole: 3.
fn position(label: &str, items: &str, body: &str, rows: &[(ItemKind, &str, &str)]) {
    let text = program(items, body, "3");
    let f = fixture(&text);
    for &(kind, name, abi) in rows {
        assert_eq!(abi_of(&f, kind, name), Some(Some(abi)), "{label}: `{name}`'s row holds its block's ABI");
    }
    assert_eq!(abi_of(&f, ItemKind::Function, "main"), Some(None), "{label}: `main` is in no block");
    assert_eq!(abi_of(&f, ItemKind::Function, "never"), Some(None), "{label}: `never` is in no block");
    let counts = walk(&f, label);
    let status = run_whole(&text);
    assert_eq!(status, 3, "{label}: the run's status");
    println!("foreign abi {label}: {counts}; run status {status}");
}

/// Position 1: each ABI spelling at module level, the row holding the block's own string
/// (a bare `unsafe extern` is "C": [items.extern.abi.default]).
/// Prints "foreign abi spellings: {counts}; run status 3".
#[test]
fn abi_spellings_are_in_their_rows() {
    position(
        "spellings",
        "unsafe extern \"C\" {
    fn abs(x: i32) -> i32;
}

unsafe extern \"C-unwind\" {
    fn labs(x: i64) -> i64;
}

unsafe extern \"system\" {
    fn llabs(x: i64) -> i64;
}

unsafe extern \"system-unwind\" {
    fn toupper(c: i32) -> i32;
}

unsafe extern {
    fn tolower(c: i32) -> i32;
}",
        "if never() {
        unsafe {
            abs(0);
            labs(0);
            llabs(0);
            toupper(0);
            tolower(0);
        }
    }",
        &[
            (ItemKind::Function, "abs", "C"),
            (ItemKind::Function, "labs", "C-unwind"),
            (ItemKind::Function, "llabs", "system"),
            (ItemKind::Function, "toupper", "system-unwind"),
            (ItemKind::Function, "tolower", "C"),
        ],
    );
}

/// Position 2: a block in an inline module.
#[test]
fn a_foreign_fn_in_an_inline_module() {
    position(
        "inline module",
        "mod inl {
    unsafe extern \"C\" {
        pub fn isalpha(c: i32) -> i32;
    }
}",
        "if never() {
        unsafe {
            inl::isalpha(0);
        }
    }",
        &[(ItemKind::Function, "isalpha", "C")],
    );
}

/// Position 3: a block under a true `cfg`; its false twin, of another ABI, is no item (one
/// `isdigit` is listed, and its row says "C-unwind", never "system").
#[test]
fn a_foreign_fn_under_a_true_cfg() {
    position(
        "true cfg",
        "#[cfg(all())]
unsafe extern \"C-unwind\" {
    fn isdigit(c: i32) -> i32;
}

#[cfg(any())]
unsafe extern \"system\" {
    fn isdigit(c: i32) -> i32;
}",
        "if never() {
        unsafe {
            isdigit(0);
        }
    }",
        &[(ItemKind::Function, "isdigit", "C-unwind")],
    );
}

/// Position 4: a block a `macro_rules!` expansion writes at module level.
#[test]
fn a_foreign_fn_a_module_level_expansion_writes() {
    position(
        "module-level expansion",
        "macro_rules! decl_top {
    () => {
        unsafe extern \"system\" {
            fn isspace(c: i32) -> i32;
        }
    };
}

decl_top!();",
        "if never() {
        unsafe {
            isspace(0);
        }
    }",
        &[(ItemKind::Function, "isspace", "system")],
    );
}

/// Position 5: a block in a fn's body (std's `guard::apple::enable` declares `_tlv_atexit` so).
/// No row: answered by the body's lowering, which the walk reaches through `in_fn`.
#[test]
fn a_foreign_fn_declared_in_a_fn_body() {
    position(
        "fn body",
        "fn in_fn() {
    unsafe extern \"C\" {
        fn isupper(c: i32) -> i32;
    }
    if never() {
        unsafe {
            isupper(65);
        }
    }
}",
        "in_fn();",
        &[],
    );
}

/// Position 6: a block an expansion in STATEMENT position writes in a body: the body's block
/// is then a module of the names library (blaze-scope expanded_blocks.rs `written`), so
/// `islower` is an item of the records with a row, which holds "C".
#[test]
fn a_foreign_fn_an_expansion_writes_in_a_body() {
    position(
        "statement expansion in a body",
        "macro_rules! decl_in_body {
    () => {
        unsafe extern \"C\" {
            fn islower(c: i32) -> i32;
        }
    };
}

fn in_body_macro() {
    decl_in_body!();
    if never() {
        unsafe {
            islower(97);
        }
    }
}",
        "in_body_macro();",
        &[(ItemKind::Function, "islower", "C")],
    );
}

/// Position 7: a block in a `const`'s initializer, beside the fn whose pointer the constant
/// is; `main` calls through it, so the fn's instance (and its call of the foreign fn) is made.
/// The constant's record names `get`'s instance (blaze-consteval `ConstRecord::fns`), so the
/// walk needs no check of `get` before it asks (tests/const_fn_pointers.rs holds the class).
#[test]
fn a_foreign_fn_declared_in_a_const_initializer() {
    position(
        "const initializer",
        "const IN_CONST: fn() = {
    unsafe extern \"C\" {
        fn isxdigit(c: i32) -> i32;
    }
    fn get() {
        if never() {
            unsafe {
                isxdigit(70);
            }
        }
    }
    get
};",
        "IN_CONST();",
        &[],
    );
}

/// Position 8: std's `thread_local!` shape: the constant's initializer is an expansion in
/// EXPRESSION position whose block declares an `extern` block and the fn the constant points to.
#[test]
fn a_foreign_fn_an_expression_expansion_writes_in_a_const() {
    position(
        "expression expansion in a const",
        "macro_rules! tl_inner {
    () => {{
        unsafe extern \"C\" {
            fn ispunct(c: i32) -> i32;
        }
        fn get() {
            if never() {
                unsafe {
                    ispunct(33);
                }
            }
        }
        get
    }};
}

const TL: fn() = tl_inner!();",
        "TL();",
        &[],
    );
}

/// Position 9: a block in a `static`'s initializer; the static is read by `main` (its value 3
/// is the run's status), so its initializer's block is lowered.
#[test]
fn a_foreign_fn_declared_in_a_static_initializer() {
    let items = "static IN_STATIC: i32 = {
    unsafe extern \"C\" {
        fn isprint(c: i32) -> i32;
    }
    fn unused() {
        if never() {
            unsafe {
                isprint(32);
            }
        }
    }
    3
};";
    let text = program(items, "", "IN_STATIC");
    let f = fixture(&text);
    assert_eq!(abi_of(&f, ItemKind::Static, "IN_STATIC"), Some(None), "a static with an initializer is in no block");
    let counts = walk(&f, "static initializer");
    let status = run_whole(&text);
    assert_eq!(status, 3, "the static's value is the run's status");
    println!("foreign abi static initializer: {counts}; run status {status}");
}

/// Position 10: a foreign STATIC declared in a body (dragonfly's and rtems' std `errno()`
/// shape). Its head is its block's (heads.rs `fn_head` takes `Locals::foreign` for a static as
/// for a fn): the walk must never stop with "the names source did not answer". The lazy run
/// path does not import a foreign static yet (tests/d106_records.rs
/// `a_foreign_static_a_macro_writes_is_made_over_names_records`, ignored), so a walk that stops
/// for THAT reason passes here, by its own words; nothing is run.
/// Prints "foreign abi body static: {Ok(counts) | Err(the other refusal)}".
#[test]
fn a_foreign_static_declared_in_a_body_is_answered_by_its_block() {
    let items = "fn read_optind() -> i32 {
    unsafe extern \"C\" {
        static optind: i32;
    }
    if never() {
        return unsafe { optind };
    }
    3
}";
    let text = program(items, "", "read_optind()");
    let f = fixture(&text);
    let walked = try_walk(&f);
    if let Err(e) = &walked {
        assert!(!e.contains(UNANSWERED), "a body's foreign static asked the names source: {e}");
        assert!(!e.contains("with no body, and no `extern` block of that body declares it"), "a body's foreign static was not found in its block: {e}");
    }
    println!("foreign abi body static: {walked:?}");
}

/// Every position above in ONE program (`main` reaches each), so one walk and one run cover the
/// class together, as std's hello world reaches several positions in one program.
/// Prints "foreign abi every position: {counts}; run status 3".
#[test]
fn every_position_in_one_program() {
    let items = "unsafe extern \"C\" {
    fn abs(x: i32) -> i32;
}

unsafe extern \"C-unwind\" {
    fn labs(x: i64) -> i64;
}

unsafe extern \"system\" {
    fn llabs(x: i64) -> i64;
}

unsafe extern \"system-unwind\" {
    fn toupper(c: i32) -> i32;
}

unsafe extern {
    fn tolower(c: i32) -> i32;
}

mod inl {
    unsafe extern \"C\" {
        pub fn isalpha(c: i32) -> i32;
    }
}

#[cfg(all())]
unsafe extern \"C-unwind\" {
    fn isdigit(c: i32) -> i32;
}

#[cfg(any())]
unsafe extern \"system\" {
    fn isdigit(c: i32) -> i32;
}

macro_rules! decl_top {
    () => {
        unsafe extern \"system\" {
            fn isspace(c: i32) -> i32;
        }
    };
}

decl_top!();

fn in_fn() {
    unsafe extern \"C\" {
        fn isupper(c: i32) -> i32;
    }
    if never() {
        unsafe {
            isupper(65);
        }
    }
}

macro_rules! decl_in_body {
    () => {
        unsafe extern \"C\" {
            fn islower(c: i32) -> i32;
        }
    };
}

fn in_body_macro() {
    decl_in_body!();
    if never() {
        unsafe {
            islower(97);
        }
    }
}

const IN_CONST: fn() = {
    unsafe extern \"C\" {
        fn isxdigit(c: i32) -> i32;
    }
    fn get() {
        if never() {
            unsafe {
                isxdigit(70);
            }
        }
    }
    get
};

macro_rules! tl_inner {
    () => {{
        unsafe extern \"C\" {
            fn ispunct(c: i32) -> i32;
        }
        fn get() {
            if never() {
                unsafe {
                    ispunct(33);
                }
            }
        }
        get
    }};
}

const TL: fn() = tl_inner!();

static IN_STATIC: i32 = {
    unsafe extern \"C\" {
        fn isprint(c: i32) -> i32;
    }
    fn unused() {
        if never() {
            unsafe {
                isprint(32);
            }
        }
    }
    3
};";
    let body = "if never() {
        unsafe {
            abs(0);
            labs(0);
            llabs(0);
            toupper(0);
            tolower(0);
            inl::isalpha(0);
            isdigit(0);
            isspace(0);
        }
    }
    in_fn();
    in_body_macro();
    IN_CONST();
    TL();";
    let text = program(items, body, "IN_STATIC");
    let f = fixture(&text);
    let rows: [(ItemKind, &str, Option<Option<&str>>); 12] = [
        (ItemKind::Function, "abs", Some(Some("C"))),
        (ItemKind::Function, "labs", Some(Some("C-unwind"))),
        (ItemKind::Function, "llabs", Some(Some("system"))),
        (ItemKind::Function, "toupper", Some(Some("system-unwind"))),
        (ItemKind::Function, "tolower", Some(Some("C"))),
        (ItemKind::Function, "isalpha", Some(Some("C"))),
        (ItemKind::Function, "isdigit", Some(Some("C-unwind"))),
        (ItemKind::Function, "isspace", Some(Some("system"))),
        (ItemKind::Function, "islower", Some(Some("C"))),
        (ItemKind::Function, "main", Some(None)),
        (ItemKind::Function, "in_fn", Some(None)),
        (ItemKind::Static, "IN_STATIC", Some(None)),
    ];
    let mut wrong = Vec::new();
    for (kind, name, want) in rows {
        let got = abi_of(&f, kind, name);
        if got != want {
            wrong.push(format!("`{name}`: {got:?}, wanted {want:?}"));
        }
    }
    assert!(wrong.is_empty(), "rows whose block ABI is not their block's: {wrong:?}");
    let counts = walk(&f, "every position");
    let status = run_whole(&text);
    assert_eq!(status, 3, "the run's status");
    println!("foreign abi every position: {counts}; run status {status}");
}
