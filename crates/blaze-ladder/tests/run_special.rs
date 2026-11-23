//! The run's roots and its special records (run/special.rs, D124: docs/v2/plan/design/run.md,
//! "The cut to the lazy parent"), and the start rule over the names records.
//!
//! What each test asserts:
//! - [`a_std_hello_starts_at_the_shim`]: the roots of `fn main() { println!("hi"); }` with
//!   Rust's std name the C entry SHIM as the entry, in the `c-main` form, and exactly two root
//!   rows: `lang_start::<()>` (std's `#[lang = "start"]` item at one argument) and `main` (at
//!   none), each with the symbol of its name; the shim's symbol is made of the two.
//! - [`each_special_is_found_the_second_time`]: the shim, the thread-local getter and the
//!   ender, each made twice against ONE store, are made (and put) the first time and FOUND the
//!   second: the same key, no second write. The planted control: a shim of other callees is a
//!   new key, so the key is not blind to the roots.
//! - [`the_start_rule_agrees_over_records`]: `start_of` over the names RECORDS (written from the
//!   solve, read with nothing solved) equals `start_of` over the solved names, and so do the
//!   roots (entry, form, rows).
//!
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain` (all but the second), as
//! tests/front2_start_rule.rs.

#![feature(allocator_api)]

use std::collections::HashMap;

use blaze_code::record::Symbol;
use blaze_emit::{Isa, Os};
use blaze_id::CrateItemKey;
use blaze_ladder::code_records::{Builder, CodeStore, MemoryStore, Settings, TierName};
use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::keys::checker_of;
use blaze_ladder::front2::program::{StartOf, start_of};
use blaze_ladder::front2::source::{Load, LoadedExpansion, Log, Records};
use blaze_ladder::front2::start::linked_lang_start;
use blaze_ladder::run::special::{EntryForm, EntryLine, RootRow, Roots, Shim, Special, make_special, roots, shim_symbol, special_of};
use blaze_ladder::symbols::{encode_instance, fn_symbol_of_name};
use blaze_own::Alloc;
use blaze_scope::names::{self, Written};
use blaze_scope::resolve::{Names, aligned};
use blaze_scope::{items_record, names_record};
use blaze_surface::{Namespace, PathKind};
use blaze_toolchain::ToolchainId;
use blaze_typeck::{ItemKey, List, PortableTys};

const HELLO: &str = "fn main() {\n    println!(\"hi\");\n}\n";

fn alloc() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

fn shelf() -> Toolchain {
    let home = std::env::var("HOME").expect("HOME is set");
    let place = format!("{home}/.blaze/v2/toolchain");
    Toolchain::open(&mut DirShelf::new(place.clone())).unwrap_or_else(|e| panic!("the toolchain at {place}: {e}"))
}

/// The crate root's `fn main`, by path.
fn main_of(names: &Names<'_>) -> blaze_id::ItemKey {
    let written = Written { kind: PathKind::Plain, segments: &["main"] };
    let found = names::path(names, names.root(0), Namespace::Values, written, alloc()).ok();
    found.and_then(|f| f.whole(1)).expect("the crate root's `fn main` resolves").key
}

/// A builder as tests/code_records.rs makes one.
fn builder() -> Builder {
    Builder {
        isa: Isa { lse: true, os: Os::MacOs },
        tier: TierName::Reference,
        settings: Settings::baseline(blaze_build::OverflowChecks::On, blaze_build::Panic::Unwind),
        toolchain: ToolchainId([7; 32]),
        producer: blaze_ids::producer::Producers::for_tests(b"run special test builder").get::<blaze_ids::producer::CodeBuilder>(),
        edition: edition::PackageEdition::of_manifest("2024").expect("a known edition"),
    }
}

/// Runs `each` over the solved names of `HELLO` on the shelf.
fn with_hello(each: impl FnOnce(&Names<'_>, &Inputs<'_>)) {
    let tc = shelf();
    let files = [("/hello/src/main.rs".to_string(), HELLO.to_string())];
    let edition = edition::PackageEdition::of_manifest("2024").expect("a known edition");
    let user = UserCrate { name: "hello", dir: "/hello/", root: "/hello/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names: Names<'_> = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    each(&names, &inputs);
}

/// The roots of the std hello world: the shim in the `c-main` form; `lang_start::<()>` and
/// `main` as the two rows.
#[test]
fn a_std_hello_starts_at_the_shim() {
    with_hello(|names, _| {
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame");
        let got: Roots<'_> = roots(names, &frame, alloc(), alloc()).unwrap_or_else(|e| panic!("the roots: {e}"));
        let lang_start = linked_lang_start(names, alloc()).unwrap_or_else(|e| panic!("the start rule: {e}")).expect("std links `#[lang = \"start\"]`");
        let main = main_of(names);
        assert_eq!(got.start, StartOf::LangStart(lang_start), "a Rust program with std starts through `#[lang = \"start\"]`");
        assert_eq!(got.form, EntryForm::CMain, "the shim is `main(argc, argv)`");
        assert_eq!(got.len(), 2, "two root instances: `lang_start::<()>` and `main`");
        let shim = got.shim().expect("the entry is the shim");
        // Row 0: `lang_start` at ONE argument (`T`, `main`'s return type `()`).
        assert_eq!(shim.lang_start.tabled, checker_of(lang_start));
        assert_eq!(&shim.lang_start.name[..16], &checker_of(lang_start).0.to_le_bytes(), "row 0's name starts with `lang_start`'s key");
        assert_eq!(&shim.lang_start.name[16..20], &1u32.to_le_bytes(), "`lang_start` at one argument");
        assert_eq!(shim.lang_start.symbol, fn_symbol_of_name(shim.lang_start.name));
        // Row 1: `main` at none: its key and a count of zero, nothing else.
        assert_eq!(shim.main.tabled, checker_of(main));
        let mut want: Vec<u8> = Vec::new();
        want.extend_from_slice(&checker_of(main).0.to_le_bytes());
        want.extend_from_slice(&0u32.to_le_bytes());
        assert_eq!(shim.main.name, &want[..], "`main` at no argument");
        assert_eq!(shim.main.symbol, fn_symbol_of_name(&want));
        assert_eq!(got.row(0), (shim.lang_start.tabled, shim.lang_start.name), "the walk's root rows are the shim's callees");
        assert_eq!(got.row(1), (shim.main.tabled, shim.main.name));
        // The entry is the shim, which is special; the rows are instances, which are not.
        assert_eq!(got.entry, shim_symbol(shim.lang_start.symbol, shim.main.symbol));
        assert!(matches!(special_of(&got.entry, &got), Some(Special::Shim(s)) if s == shim));
        assert_eq!(special_of(&shim.main.symbol, &got), None, "`main` is an instance, not a special record");
        let line = got.line().to_string();
        assert!(line.starts_with("c-main:"), "{line}");
        assert_eq!(line.len(), got.line().len());
        assert_eq!(line.len(), 7 + 64);
    });
}

/// A root row of a non-generic fn `key`, as `roots` makes one.
fn row_of(key: u128, name: &mut Vec<u8>) -> (ItemKey, Symbol) {
    let tys = PortableTys::new_in(alloc());
    let mut out: blaze_own::AVec<'_, u8> = blaze_own::Vec::new_in(alloc());
    let item = ItemKey(key);
    encode_instance(item, List::EMPTY, tys.view(), alloc(), &mut out).unwrap_or_else(|e| panic!("the name: {e}"));
    name.extend_from_slice(&out);
    (item, fn_symbol_of_name(&out))
}

/// Each special record, made twice against one store: made the first time, found the second.
#[test]
fn each_special_is_found_the_second_time() {
    let (mut a, mut b, mut c) = (Vec::new(), Vec::new(), Vec::new());
    let (ls, ls_symbol) = row_of(0x11, &mut a);
    let (main, main_symbol) = row_of(0x22, &mut b);
    let (other, other_symbol) = row_of(0x33, &mut c);
    let shim = Shim { lang_start: RootRow { tabled: ls, name: &a, symbol: ls_symbol }, main: RootRow { tabled: main, name: &b, symbol: main_symbol } };
    let builder = builder();
    let mut store = MemoryStore::new_in(alloc());
    let mut keys = Vec::new();
    for which in [Special::Shim(shim), Special::Getter, Special::Ender] {
        let first = make_special(which, &builder, &mut store, alloc()).unwrap_or_else(|e| panic!("{}: {e}", which.name()));
        assert!(!first.found, "{}: made the first time", which.name());
        let puts = store.puts;
        assert!(store.get(first.key).expect("the store reads").is_some(), "{}: put under its key", which.name());
        let second = make_special(which, &builder, &mut store, alloc()).unwrap_or_else(|e| panic!("{}: {e}", which.name()));
        assert!(second.found, "{}: found the second time, not made", which.name());
        assert_eq!(second.key, first.key, "{}: one key", which.name());
        assert_eq!(store.puts, puts, "{}: no second write", which.name());
        keys.push(first.key);
    }
    keys.sort();
    keys.dedup();
    assert_eq!(keys.len(), 3, "the three specials are three keys");
    // The control: the shim of other callees is another key (its key holds the roots).
    let moved = Shim { main: RootRow { tabled: other, name: &c, symbol: other_symbol }, ..shim };
    let made = make_special(Special::Shim(moved), &builder, &mut store, alloc()).unwrap_or_else(|e| panic!("the moved shim: {e}"));
    assert!(!made.found, "a shim of another `main` is not found under the first one's key");
    assert_ne!(made.symbol, shim_symbol(ls_symbol, main_symbol));
}

/// What a loader hands the records reader (as tests/front2_project_records.rs keeps them).
struct Kept {
    names: Vec<&'static [u8]>,
    items: Vec<&'static [u8]>,
    files: Vec<HashMap<String, &'static str>>,
    expansions: Vec<HashMap<blaze_id::ItemKey, &'static str>>,
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

    /// The texts, with no origins kept.
    fn expansion(&self, krate: usize, call: blaze_id::ItemKey) -> Option<LoadedExpansion<'static>> {
        let text = self.expansions.get(krate)?.get(&call).copied()?;
        Some(LoadedExpansion { text, origins: None })
    }
}

fn leak(s: &str) -> &'static str {
    Box::leak(s.to_string().into_boxed_str())
}

/// `start_of` and `roots` over the names records equal them over the solved names.
#[test]
fn the_start_rule_agrees_over_records() {
    with_hello(|names, inputs| {
        let g = alloc();
        let solved = start_of(names, g).unwrap_or_else(|e| panic!("the start rule over the solved names: {e}"));
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame");
        let solved_roots = roots(names, &frame, g, g).unwrap_or_else(|e| panic!("the roots over the solved names: {e}"));
        // The records written from the solve.
        let crates = inputs.crates();
        let crate_names: &'static [&'static str] = Vec::leak(crates.iter().map(|c| leak(c.name)).collect());
        let keys: &'static [CrateItemKey] = Vec::leak(crates.iter().map(|c| c.key).collect());
        let mut kept = Kept { names: Vec::new(), items: Vec::new(), files: Vec::new(), expansions: Vec::new() };
        for c in names.crates().iter() {
            kept.names.push(aligned(&names_record::encode(c, g).unwrap_or_else(|e| panic!("the names record of `{}`: {e:?}", c.name)), blaze_own::testing::bump()));
            kept.items.push(aligned(&items_record::encode(c, g).unwrap_or_else(|e| panic!("the items record of `{}`: {e:?}", c.name)), blaze_own::testing::bump()));
            kept.files.push(c.files.iter().map(|f| (f.package.as_str().to_string(), leak(f.text))).collect());
            kept.expansions.push(c.expansions.iter().filter_map(|(call, e)| e.made.map(|m| (*call, leak(m.text)))).collect());
        }
        let reader_frame: &'static blaze_mem::Frame = Box::leak(Box::new(blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the records reader's frame")));
        let log: &'static Log<'static> = Box::leak(Box::new(Log::new(crate_names, g)));
        let load: &'static Kept = Box::leak(Box::new(kept));
        let records: &'static Records<'static> = Box::leak(Box::new(Records::new(keys, load, log, reader_frame, g)));
        let over_records = start_of(records, g).unwrap_or_else(|e| panic!("the start rule over records: {e}"));
        let errors: Vec<String> = records.errors().iter().map(|e| e.to_string()).collect();
        assert!(errors.is_empty(), "the records filed {errors:?}");
        assert_eq!(over_records, solved, "one start rule, one answer, over either source");
        let records_roots = roots(records, reader_frame, g, g).unwrap_or_else(|e| panic!("the roots over records: {e}"));
        assert_eq!(records_roots.entry, solved_roots.entry, "the same entry");
        assert_eq!(records_roots.form, solved_roots.form, "the same form");
        assert_eq!(records_roots.len(), solved_roots.len());
        for i in 0..solved_roots.len() {
            assert_eq!(records_roots.root(i), solved_roots.root(i), "root row {i}");
        }
        // The line is a function of the form and the symbol alone.
        assert_eq!(records_roots.line(), EntryLine { form: solved_roots.form, symbol: solved_roots.entry });
    });
}
