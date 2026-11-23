//! D106 piece Z, item 2: the lazy program (front2/program/lazy) makes a body's statics and the
//! object tables of the `dyn` principals it names when THAT body is checked, and answers the
//! seam's `static_item`, `static_records` and `object_trait` as the whole program does.
//!
//! The rule tested (AGENTS.md rule 78: one seam, two instantiations, tested equal), over one
//! `no_core` program solved once. The language's rules: a static's initializer is evaluated at
//! compile time and a reference to another static is a relocation to it (the Reference,
//! items.static.init); a trait object's vtable holds the methods of its principal and of every
//! supertrait (the Reference, types.trait-object: "the trait object ... includes ... the
//! supertraits"), so `dyn Tr` with `Tr: Base` makes Base's table too.
//!
//! Planted control (the cause removed): without fill.rs's `self.body_statics(..)` line,
//! `static_item(B)` is None in the lazy program and `lazy_statics_answer_as_the_whole` fails.
//!
//! Expected lines (with `--nocapture`):
//!   "lazy statics = whole: 2 statics, 0 differences"
//!   "lazy objects = whole: 2 tables, 0 differences"
//!   "one body: checked 1, statics 2, object tables 2"

#![feature(allocator_api)]

use blaze_ladder::crates::{Inputs, Toolchain};
use blaze_ladder::front2::keys::checker_of;
use blaze_ladder::front2::program::Front2Program;
use blaze_ladder::front2::program::lazy::LazyProgram;
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::seam::FrontEnd;
use blaze_own::Alloc;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};

/// Two statics (B points at A), a trait with a supertrait, and `main` naming both: B by value,
/// `dyn Tr` through a fn pointer's type (no coercion, so no lang item beyond `sized`).
const PROGRAM: &str = "#![feature(no_core, lang_items, auto_traits)]
#![no_core]

#[lang = \"sized\"]
trait Sized {}

// A static's type must be `Sync` (the Reference, items.static.sync).
#[lang = \"sync\"]
unsafe auto trait Sync {}

static A: i32 = 7;
static B: &i32 = &A;

trait Base {
    fn base(&self) -> i32;
}

trait Tr: Base {
    fn m(&self) -> i32;
}

fn via(t: &dyn Tr) -> i32 {
    t.m()
}

fn main() {
    let _b = B;
    let _f: fn(&dyn Tr) -> i32 = via;
}
";

fn with_program(each: impl FnOnce(&blaze_scope::resolve::Names<'_>, &[SourceFile<'_>])) {
    let files = [SourceFile { path: "src/main.rs", text: PROGRAM }];
    let tc = Toolchain::none(blaze_own::Alloc::global());
    let new = blaze_ladder::run::NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::On };
    let texts = blaze_ladder::program_texts::ProgramTexts::new(&files, "src/main.rs", new, &tc, blaze_build::Panic::Unwind).unwrap_or_else(|e| panic!("the program's texts: {e}"));
    let user = texts.user();
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    each(&names, &files);
}

/// The checker key of the item `name` of the root, by path in `ns`.
fn key_of(names: &blaze_scope::resolve::Names<'_>, ns: Namespace, name: &'static str) -> blaze_typeck::ItemKey {
    let segments: &[&str] = &[name];
    let written = Written { kind: PathKind::Plain, segments };
    let found = names::path(names, names.root(0), ns, written, Alloc::global()).unwrap_or_else(|e| panic!("`{name}` by path: {e:?}"));
    checker_of(found.whole(1).unwrap_or_else(|| panic!("`{name}` resolves whole")).key)
}

/// A static's record as plain data: each allocation's bytes, alignment and relocation count.
fn record_shown(r: &blaze_build::data::DataRecord<std::alloc::Global>) -> String {
    r.allocs.iter().map(|a| format!("{:?}/{:?}/{}", &a.bytes[..], a.align, a.relocs.len())).collect::<Vec<_>>().join(";")
}

fn trait_shown<P: FrontEnd>(p: &P, k: blaze_typeck::ItemKey) -> String {
    match p.object_trait(k) {
        None => String::from("none"),
        Some(t) => format!("supers {:?} methods {} shape {:?}", t.supers, t.methods.len(), t.shape),
    }
}

#[test]
fn lazy_statics_answer_as_the_whole() {
    with_program(|names, files| {
        let (wf, lf) = (blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("a frame"), blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("a frame"));
        let whole = Front2Program::new(names, &wf, files, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("the whole program: {e}"));
        let main = key_of(names, Namespace::Values, "main");
        let mut lazy = LazyProgram::new(names, &lf, files, Some(main), blaze_own::testing::heap().alloc()).unwrap_or_else(|e| panic!("the lazy program: {e}"));
        lazy.check_one(main).unwrap_or_else(|e| panic!("check_one(main): {e}")).expect("main is a fn");
        let mut diffs: Vec<String> = Vec::new();
        for name in ["A", "B"] {
            let k = key_of(names, Namespace::Values, name);
            let (w, l) = (format!("{:?}", whole.static_item(k)), format!("{:?}", lazy.static_item(k)));
            if w != l || w == "None" {
                diffs.push(format!("static_item({name}): whole {w}, lazy {l}"));
            }
        }
        let shown = |rs: &[(blaze_typeck::ItemKey, blaze_build::data::DataRecord<std::alloc::Global>)]| rs.iter().map(|(k, r)| format!("{k:?} {}", record_shown(r))).collect::<Vec<_>>();
        let (w, l) = (shown(whole.static_records()), shown(lazy.static_records()));
        if w != l {
            diffs.push(format!("static_records: whole {w:?}, lazy {l:?}"));
        }
        println!("lazy statics = whole: {} statics, {} differences", l.len(), diffs.len());
        assert!(diffs.is_empty(), "the lazy program answered otherwise:\n{}", diffs.join("\n"));
        assert_eq!(l.len(), 2, "B and the static its record points at (A)");
    });
}

#[test]
fn lazy_objects_answer_as_the_whole() {
    with_program(|names, files| {
        let (wf, lf) = (blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("a frame"), blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("a frame"));
        let whole = Front2Program::new(names, &wf, files, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("the whole program: {e}"));
        let main = key_of(names, Namespace::Values, "main");
        let mut lazy = LazyProgram::new(names, &lf, files, Some(main), blaze_own::testing::heap().alloc()).unwrap_or_else(|e| panic!("the lazy program: {e}"));
        lazy.check_one(main).unwrap_or_else(|e| panic!("check_one(main): {e}")).expect("main is a fn");
        let mut diffs: Vec<String> = Vec::new();
        let mut tables = 0;
        for name in ["Tr", "Base"] {
            let k = key_of(names, Namespace::Types, name);
            let (w, l) = (trait_shown(&whole, k), trait_shown(&lazy, k));
            if w != l || w == "none" {
                diffs.push(format!("object_trait({name}): whole {w}, lazy {l}"));
            } else {
                tables += 1;
            }
        }
        println!("lazy objects = whole: {tables} tables, {} differences", diffs.len());
        assert!(diffs.is_empty(), "the lazy program answered otherwise:\n{}", diffs.join("\n"));
    });
}

#[test]
fn one_body_makes_its_own_data_alone() {
    with_program(|names, files| {
        let lf = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("a frame");
        let main = key_of(names, Namespace::Values, "main");
        let mut lazy = LazyProgram::new(names, &lf, files, Some(main), blaze_own::testing::heap().alloc()).unwrap_or_else(|e| panic!("the lazy program: {e}"));
        assert_eq!(lazy.data_counts(), (0, 0), "nothing is made before a body is asked");
        lazy.check_one(main).unwrap_or_else(|e| panic!("check_one(main): {e}")).expect("main is a fn");
        let c = lazy.counts();
        let (statics, objects) = lazy.data_counts();
        println!("one body: checked {}, statics {statics}, object tables {objects}", c.checked);
        assert_eq!((c.checked, statics, objects), (1, 2, 2), "main's statics (B, A) and tables (Tr, Base), and no other body checked");
    });
}
