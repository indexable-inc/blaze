//! The BODY of an item a macro wrote resolves its paths by its words' marks (drive.md, "A token
//! keeps its mark"; blaze-lower `lower_body_expanding_marked`), over the solved names and over the
//! records alike.
//!
//! The Reference, "Macros By Example", Hygiene: `$crate` names "the crate where the macro was
//! defined". The names library writes an item an expansion holds as text, another crate's
//! `$crate` as the word `crate`, that crate kept only in the word's mark (blaze-scope crate_roots
//! `root_mark`). A body lowered from that text alone reads `crate` as the crate that expands it:
//! `blaze run` on hello world refused bstd's `main!` (crates/blaze-std/src/entry.rs writes
//! `$crate::launch_body!(..)` into `fn main`) with "Macro path `crate:: launch_body`, segment 0:
//! not found", and tempo's 1,067 derived bodies (`<impl crate::default::Default for
//! TempoOverrides>::default`, body `crate::default::Default::default()`) were checked in the
//! user's crate.
//!
//! Rows, each checked over both sources:
//! - (i) `in_body`: `dep::outer!()` in a fn body, `outer` writing `$crate::inner!()`, `inner`
//!   `7u8`: the body checks clean as a `u8`.
//! - (ii) `by_item` and `by_item_stmt`: `dep::outer_item!(..)` in item position writing `pub fn ..()
//!   -> u8 { $crate::inner!() }` and `pub fn ..() { $crate::inner!(); }` (bstd's entry shape):
//!   both check clean.
//! - (iii) `default` of `#[derive(Default)] pub struct Overrides`: checks with no diagnostic (needs
//!   the installed toolchain under `$HOME/.blaze/v2/toolchain`, as tests/front2_records.rs).
//!
//! THE CONTROL is in the program itself: its root defines its own `#[macro_export] macro_rules!
//! inner` writing `only_the_wrong_inner_names_this`, which names nothing. Read without the mark,
//! (ii)'s `crate::inner!()` finds the program's `inner` and its body is E0425
//! (`UnresolvedPath`); (iii)'s `crate::default` is not found in the program's crate.
//!
//! What it prints when it passes:
//! ```text
//! front2_token_mark_bodies names: in_body: clean; by_item: clean; by_item_stmt: clean
//! front2_token_mark_bodies records: in_body: clean; by_item: clean; by_item_stmt: clean
//! test macro_written_bodies_resolve_by_their_marks ... ok
//! front2_token_mark_bodies names: main: clean
//! front2_token_mark_bodies records: main: clean
//! test macro_written_local_items_resolve_by_their_marks ... ok
//! front2_token_mark_bodies names: default: clean
//! front2_token_mark_bodies records: default: clean
//! test a_derived_body_checks_in_its_derive_s_crate ... ok
//! ```

#![feature(allocator_api)]

use std::collections::HashMap;

use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate, syntax_of};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::body::{lower_fn, lower_member};
use blaze_ladder::front2::check::{BuildOrder, CheckFail, Records as Check, check_body};
use blaze_ladder::front2::heads::{Store, Want};
use blaze_ladder::front2::index::Index;
use blaze_ladder::front2::keys::{Keys, MemberKind, checker_of};
use blaze_ladder::front2::source::{Load, Log, NamesSource, Records};
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::resolve::{Ns, Paths};
use blaze_lower::scoped::Prepares;
use blaze_own::Alloc;
use blaze_scope::expansion_origins::OriginsError;
use blaze_scope::graph::Sysroot;
use blaze_scope::resolve::{CrateInput, Names, SourceTexts, aligned, resolve_crates};
use blaze_scope::task::Serial;
use blaze_scope::{items_record, names_record};
use blaze_surface::ItemDetail;
use blaze_unit_graph::implicit;
use edition::PackageEdition;

const DEP: &str = "#![feature(no_core, lang_items)]
#![no_core]

#[lang = \"sized\"]
pub trait Sized {}

#[macro_export]
macro_rules! inner { () => { 7u8 } }

#[macro_export]
macro_rules! outer { () => { $crate::inner!() } }

#[macro_export]
macro_rules! outer_item {
    ($name:ident) => { pub fn $name() -> u8 { $crate::inner!() } };
    (stmt $name:ident) => { pub fn $name() { $crate::inner!(); } };
}
";

const ROOT: &str = "#![feature(no_core)]
#![no_core]
#![no_main]

#[macro_export]
macro_rules! inner { () => { only_the_wrong_inner_names_this } }

pub fn in_body() -> u8 { dep::outer!() }

dep::outer_item!(by_item);
dep::outer_item!(stmt by_item_stmt);
";

const ROOT_PATH: &str = "/marks/src/main.rs";
const DEP_PATH: &str = "/dep/src/lib.rs";

struct Files;

impl<'t> SourceTexts<'t> for Files {
    fn read(&self, krate: usize, path: &str) -> Option<&'t str> {
        match (krate, path) {
            (0, ROOT_PATH) => Some(ROOT),
            (1, DEP_PATH) => Some(DEP),
            _ => None,
        }
    }
}

fn leak(s: &str) -> &'static str {
    Box::leak(s.to_string().into_boxed_str())
}

/// The records and texts as kept after the solve, each expansion's origin column with its text:
/// the loader serves the marks (tests/front2_records.rs `Kept`, its origins too).
struct Kept {
    names: Vec<&'static [u8]>,
    items: Vec<&'static [u8]>,
    files: Vec<HashMap<String, &'static str>>,
    expansions: Vec<HashMap<ItemKey, &'static str>>,
    origins: Vec<HashMap<ItemKey, &'static [u8]>>,
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

    fn expansion(&self, krate: usize, call: ItemKey) -> Option<&'static str> {
        self.expansions.get(krate)?.get(&call).copied()
    }

    fn expansion_origins(&self, krate: usize, call: ItemKey) -> Option<&'static [u8]> {
        self.origins.get(krate)?.get(&call).copied()
    }
}

/// The origin column an expansion's text keeps: empty for a text that keeps none by design.
fn made_origins(call: ItemKey, origins: &Result<&[u8], OriginsError>) -> &'static [u8] {
    match origins {
        Ok(bytes) => Box::leak(bytes.to_vec().into_boxed_slice()),
        Err(OriginsError::Rewritten { .. } | OriginsError::Unplaced { .. }) => &[],
        Err(e) => panic!("the origins of expansion {call:x}: {e:?}"),
    }
}

/// Every record and text of the solve, kept.
fn kept_of(names: &Names<'_>) -> Kept {
    let g = Alloc::global();
    let mut kept = Kept { names: Vec::new(), items: Vec::new(), files: Vec::new(), expansions: Vec::new(), origins: Vec::new() };
    for c in names.crates().iter() {
        kept.names.push(aligned(&names_record::encode(c, g).unwrap_or_else(|e| panic!("the names record of `{}`: {e:?}", c.name)), g));
        kept.items.push(aligned(&items_record::encode_with(c, names, g).unwrap_or_else(|e| panic!("the items record of `{}`: {e:?}", c.name)), g));
        kept.files.push(c.files.iter().map(|f| (f.package.as_str().to_string(), leak(f.text))).collect());
        kept.expansions.push(c.expansions.iter().filter_map(|(call, e)| e.made.map(|m| (*call, leak(m.text)))).collect());
        kept.origins.push(c.expansions.iter().filter_map(|(call, e)| e.made.as_ref().map(|m| (*call, made_origins(*call, &m.origins)))).collect());
    }
    kept
}

/// A reader over `kept` alone, leaked: a `Records` borrows itself for its whole life.
fn reader(keys: &'static [CrateItemKey], crate_names: &'static [&'static str], kept: Kept) -> (&'static Records<'static>, &'static blaze_mem::Frame) {
    let frame: &'static blaze_mem::Frame = Box::leak(Box::new(blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the records reader's frame")));
    let log: &'static Log<'static> = Box::leak(Box::new(Log::new(crate_names, Alloc::global())));
    let load: &'static Kept = Box::leak(Box::new(kept));
    (Box::leak(Box::new(Records::new(keys, load, log, frame, Alloc::global()))), frame)
}

/// A body to check: a free fn, or a member fn of an impl.
#[derive(Clone, Copy)]
enum Asked {
    Free(ItemKey),
    Member { impl_: ItemKey, name: &'static str },
}

/// `f` lowered and checked over `names`: `clean`, or what refused it. ONE function over both
/// sources: what differs between two calls is the source alone.
fn checked<'n, S: NamesSource<'n> + Copy>(names: S, prepared: &'n Prepares<'n>, keys: &mut Keys, index: &Index<'n>, store: &mut Store<'n>, f: Asked) -> String {
    let lowered = match f {
        Asked::Free(key) => match store.ensure(names, keys, Want::Fn(key)) {
            Ok(()) => lower_fn(names, prepared, keys, store, key),
            Err(e) => return format!("no head: {e:?}"),
        },
        // The member by front2's own key (keys.rs `member_key`: its container, kind and name),
        // filed when the impl's head is made; the names library's member key is another key.
        Asked::Member { impl_, name } => match store.ensure(names, keys, Want::Impl(impl_)) {
            Ok(()) => match keys.filed_member(checker_of(impl_), MemberKind::Fn, name) {
                Some(member) => lower_member(names, prepared, keys, store, member),
                None => return format!("impl {impl_:x}'s head files no fn `{name}`"),
            },
            Err(e) => return format!("no impl head: {e:?}"),
        },
    };
    let body = match lowered {
        Ok(body) => body,
        Err(e) => return format!("does not lower: {e}"),
    };
    let view = body.lowered.view();
    let spell = Spelling::of(&view).expect("the body's spelling");
    let scope = body.scope(&spell);
    let mut buckets = None;
    let mut check = Check { names, keys, index, store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: None };
    let said = match check_body(&mut check, &body, &scope) {
        Ok(done) if done.diags.is_empty() => "clean".to_string(),
        Ok(done) => format!("Ok with diagnostics {:?}", done.diags),
        Err(CheckFail::Diags { diags, .. }) => format!("diagnostics {diags:?}"),
        Err(other) => format!("{other:?}"),
    };
    said.chars().take(300).collect()
}

/// Each row's body over `names`: `name: said`, and the rows that did not check clean.
fn rows<'n, S: NamesSource<'n> + Copy>(names: S, index: &Index<'n>, keys: &mut Keys, frame: &'n blaze_mem::Frame, sized: Option<ItemKey>, fns: &[(&str, Asked)], failed: &mut Vec<String>) -> String {
    let prepared = Prepares::new_in(Alloc::global());
    let mut store = Store::new(sized, frame, Alloc::global());
    let mut said = Vec::new();
    for &(name, f) in fns {
        let got = checked(names, prepared, keys, index, &mut store, f);
        if got != "clean" {
            failed.push(format!("{name}: {got}"));
        }
        said.push(format!("{name}: {got}"));
    }
    said.join("; ")
}

/// (i) and (ii) over the solved names, then over the records of the same solve.
///
/// Prints `front2_token_mark_bodies names: in_body: clean; by_item: clean; by_item_stmt: clean`,
/// the same line for `records`, and `test macro_written_bodies_resolve_by_their_marks ... ok`.
#[test]
fn macro_written_bodies_resolve_by_their_marks() {
    let tc = Toolchain::none(Alloc::global());
    let cfg = tc.program_cfg(blaze_build::Panic::Unwind);
    let rust = PackageEdition { syntax: edition::Edition::Edition2024, blaze: None };
    let lib = implicit(rust, &blaze_unit_graph::UnitKind::Lib);
    let deps: [(&str, usize); 1] = [("dep", 1)];
    let crates = [
        CrateInput {
            name: "marks",
            key: CrateItemKey(ContentId::of("front2_token_mark_bodies program", b"marks")),
            edition: syntax_of(rust),
            implicit: lib,
            root: ROOT_PATH,
            shown_from: "/marks/",
            features: &[],
            deps: &deps,
            cfg: &cfg,
            script: None,
        },
        CrateInput {
            name: "dep",
            key: CrateItemKey(ContentId::of("front2_token_mark_bodies test crate", b"dep")),
            edition: syntax_of(rust),
            implicit: lib,
            root: DEP_PATH,
            shown_from: "/dep/",
            features: &[],
            deps: &[],
            cfg: &cfg,
            script: None,
        },
    ];
    let crate_names: &'static [&'static str] = &["marks", "dep"];
    let keys: &'static [CrateItemKey] = Vec::leak(crates.iter().map(|c| c.key).collect());
    let alloc = blaze_own::testing::heap().alloc();
    let names = resolve_crates(&crates, Sysroot::default(), &Files, &Serial, alloc).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let fn_of = |name: &str| Paths { names: &names }.path(names.root(0), false, &[name], Ns::Value).unwrap_or_else(|e| panic!("`{name}` from the program's root: {e:?}")).key;
    let fns = [("in_body", Asked::Free(fn_of("in_body"))), ("by_item", Asked::Free(fn_of("by_item"))), ("by_item_stmt", Asked::Free(fn_of("by_item_stmt")))];
    let mut failed = Vec::new();

    let sources = [SourceFile { path: "src/main.rs", text: ROOT }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame");
    let solved = {
        let mut front = Front2::new(&names, &frame, &sources, Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
        rows(&names, &front.index, &mut front.keys, front.frame, front.index.lang("sized"), &fns, &mut failed)
    };
    println!("front2_token_mark_bodies names: {solved}");

    let (records, frame) = reader(keys, crate_names, kept_of(&names));
    let mut front = Front2::from_records(records, frame, &[0, 1]).unwrap_or_else(|e| panic!("front2 over records: {e}"));
    let over_records = rows(records, &front.index, &mut front.keys, front.frame, front.index.lang("sized"), &fns, &mut failed);
    println!("front2_token_mark_bodies records: {over_records}");
    let errors: Vec<String> = records.errors().iter().map(|e| e.to_string()).collect();
    assert!(errors.is_empty(), "the records filed {errors:?}");
    assert!(failed.is_empty(), "bodies that did not check clean: {failed:?}");
}

const LOCAL_DEP: &str = "#![feature(no_core, lang_items)]
#![no_core]

#[lang = \"sized\"]
pub trait Sized {}

pub struct T;

#[macro_export]
macro_rules! noop { () => {} }

#[macro_export]
macro_rules! tail { () => { struct Z; static N: $crate::T = $crate::T; let _n = &N; $crate::noop!(); } }

#[macro_export]
macro_rules! entry {
    () => {
        fn main() { static M: $crate::T = $crate::T; fn inner(_: &$crate::T) {} struct L(#[allow(dead_code)] $crate::T); inner(&M); let _l = L($crate::T); $crate::tail!(); }
    };
}
";

const LOCAL_ROOT: &str = "#![feature(no_core)]
#![no_core]

dep::entry!();
";

const LOCAL_ROOT_PATH: &str = "/locals/src/main.rs";
const LOCAL_DEP_PATH: &str = "/ldep/src/lib.rs";

struct LocalFiles;

impl<'t> SourceTexts<'t> for LocalFiles {
    fn read(&self, krate: usize, path: &str) -> Option<&'t str> {
        match (krate, path) {
            (0, LOCAL_ROOT_PATH) => Some(LOCAL_ROOT),
            (1, LOCAL_DEP_PATH) => Some(LOCAL_DEP),
            _ => None,
        }
    }
}

/// The local items of a `main` another crate's macro wrote resolve their `$crate` paths by their
/// marks, over the solved names and over the records. `dep::entry!()` writes `main` with a local
/// `static M: $crate::T`, `fn inner(_: &$crate::T)` and `struct L($crate::T)` (written in main's
/// own text: R1's `Enclosing::marks`; `L` also lands in the block's module, blaze-scope
/// resolve.rs `make_blocks`, whose text keeps the body's marks moved by `module_text`'s copies),
/// and the statement call `$crate::tail!()`, whose `static N: $crate::T` is a row a call INSIDE
/// the body wrote: its text a copy into the frame, its marks made with it (front2 locals.rs
/// `Locals::keep`, `View::marks_in`, `mark_site::column_of_copy`). The program defines no `T`,
/// `tail`, `noop` or `inner`: a `crate` read as the program's names nothing.
///
/// Without `make_blocks`' moved column, the block module's `crate::T` (L's field) and
/// `crate::noop!()` (in `tail!`'s statements) are not found in `locals` and the names line is
/// not `main: clean`. Without the copy's column, `N`'s head reads `crate::T` in `locals` and
/// `main` does not lower. The `Expand` trait's `path_mark` (blaze-lower driver.rs) is reached
/// only by `lower_file`, not by front2, so this test does not cover it.
///
/// Prints `front2_token_mark_bodies names: main: clean`, the same line for `records`, and
/// `test macro_written_local_items_resolve_by_their_marks ... ok`.
#[test]
fn macro_written_local_items_resolve_by_their_marks() {
    let tc = Toolchain::none(Alloc::global());
    let cfg = tc.program_cfg(blaze_build::Panic::Unwind);
    let rust = PackageEdition { syntax: edition::Edition::Edition2024, blaze: None };
    let lib = implicit(rust, &blaze_unit_graph::UnitKind::Lib);
    let deps: [(&str, usize); 1] = [("dep", 1)];
    let crates = [
        CrateInput {
            name: "locals",
            key: CrateItemKey(ContentId::of("front2_token_mark_bodies locals program", b"locals")),
            edition: syntax_of(rust),
            implicit: lib,
            root: LOCAL_ROOT_PATH,
            shown_from: "/locals/",
            features: &[],
            deps: &deps,
            cfg: &cfg,
            script: None,
        },
        CrateInput {
            name: "dep",
            key: CrateItemKey(ContentId::of("front2_token_mark_bodies locals dep", b"dep")),
            edition: syntax_of(rust),
            implicit: lib,
            root: LOCAL_DEP_PATH,
            shown_from: "/ldep/",
            features: &[],
            deps: &[],
            cfg: &cfg,
            script: None,
        },
    ];
    let crate_names: &'static [&'static str] = &["locals", "dep"];
    let keys: &'static [CrateItemKey] = Vec::leak(crates.iter().map(|c| c.key).collect());
    let alloc = blaze_own::testing::heap().alloc();
    let names = resolve_crates(&crates, Sysroot::default(), &LocalFiles, &Serial, alloc).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let main = Paths { names: &names }.path(names.root(0), false, &["main"], Ns::Value).unwrap_or_else(|e| panic!("`main` from the program's root: {e:?}")).key;
    let fns = [("main", Asked::Free(main))];
    let mut failed = Vec::new();

    let sources = [SourceFile { path: "src/main.rs", text: LOCAL_ROOT }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame");
    let solved = {
        let mut front = Front2::new(&names, &frame, &sources, Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
        rows(&names, &front.index, &mut front.keys, front.frame, front.index.lang("sized"), &fns, &mut failed)
    };
    println!("front2_token_mark_bodies names: {solved}");

    let (records, frame) = reader(keys, crate_names, kept_of(&names));
    let mut front = Front2::from_records(records, frame, &[0, 1]).unwrap_or_else(|e| panic!("front2 over records: {e}"));
    let over_records = rows(records, &front.index, &mut front.keys, front.frame, front.index.lang("sized"), &fns, &mut failed);
    println!("front2_token_mark_bodies records: {over_records}");
    let errors: Vec<String> = records.errors().iter().map(|e| e.to_string()).collect();
    assert!(errors.is_empty(), "the records filed {errors:?}");
    assert!(failed.is_empty(), "bodies that did not check clean: {failed:?}");
}

/// The derived `default` of `#[derive(Default)]` (whose body is `crate::default::Default::default()`
/// in the impl's text, `crate` core's by its mark), over the solved names and over the records.
///
/// Prints `front2_token_mark_bodies names: default: clean`, the same line for `records`, and
/// `test a_derived_body_checks_in_its_derive_s_crate ... ok`.
#[test]
fn a_derived_body_checks_in_its_derive_s_crate() {
    const MAIN: &str = "#[derive(Default)]
pub struct Overrides {
    pub a: u8,
    pub b: bool,
}

fn main() {}
";
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/derived/src/main.rs".to_string(), MAIN.to_string())];
    let edition = PackageEdition::of_manifest("blaze-2026.11").expect("a known edition");
    let user = UserCrate { name: "derived", dir: "/derived/", root: "/derived/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let crate_names: &'static [&'static str] = Vec::leak(crates.iter().map(|c| leak(c.name)).collect());
    let keys: &'static [CrateItemKey] = Vec::leak(crates.iter().map(|c| c.key).collect());
    let position = |name: &str| crate_names.iter().position(|n| *n == name);
    let lang_crates: Vec<usize> = [Some(0), position("core"), position("alloc"), position("std"), position("bstd"), position("blaze_std")].into_iter().flatten().collect();
    let names = inputs.resolve(&crates, Alloc::global()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));

    // The one impl of the program (the derive's), and its `default`.
    let impls: Vec<ItemKey> = names.items(0).map(|(key, _, _)| key).filter(|&key| NamesSource::detail_of(&names, key).is_some_and(|d| matches!(d, ItemDetail::Impl))).collect();
    let [impl_] = impls.as_slice() else { panic!("the program holds {} impls, not the derive's one: {impls:?}", impls.len()) };
    let fns = [("default", Asked::Member { impl_: *impl_, name: "default" })];
    let mut failed = Vec::new();

    let sources = [SourceFile { path: "src/main.rs", text: MAIN }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame");
    let solved = {
        let mut front = Front2::new(&names, &frame, &sources, Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
        rows(&names, &front.index, &mut front.keys, front.frame, front.index.lang("sized"), &fns, &mut failed)
    };
    println!("front2_token_mark_bodies names: {solved}");

    let (records, frame) = reader(keys, crate_names, kept_of(&names));
    let mut front = Front2::from_records(records, frame, &lang_crates).unwrap_or_else(|e| panic!("front2 over records: {e}"));
    let over_records = rows(records, &front.index, &mut front.keys, front.frame, front.index.lang("sized"), &fns, &mut failed);
    println!("front2_token_mark_bodies records: {over_records}");
    assert!(failed.is_empty(), "bodies that did not check clean: {failed:?}");
}
