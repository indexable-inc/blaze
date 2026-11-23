//! A program's lang rows are filed from ITS link closure, never from every crate of the
//! workspace (front2 linked.rs, index.rs `Index::new`; wiring lang-rows-per-program-1.md).
//!
//! The rule: the pinned library's library/std/src/alloc.rs:53, "The `#[global_allocator]` can
//! only be used once in a crate or its recursive dependencies."; the Reference
//! [panic.panic_handler.unique], "There must be a single panic_handler function in the
//! dependency graph." So two binaries of one workspace, each with its own
//! `#[global_allocator]`, are two programs with one allocator each; a binary whose dependency
//! also declares one is refused, naming both crates.
//!
//! The workspace here (every crate `#![no_core]`, no toolchain: the rows come from the names
//! library's per-crate tables alone):
//! - crate 0 `bin_a` (deps `shared`): its own `#[global_allocator]`;
//! - crate 1 `bin_b` (deps `shared`): its own `#[global_allocator]`;
//! - crate 2 `shared`: a library with none;
//! - crate 3 `bin_c` (deps `alloc_lib`): its own `#[global_allocator]`;
//! - crate 4 `alloc_lib`: a library that declares one too.
//!
//! Prints (each test): `test <name> ... ok`.

use blaze_id::{ContentId, CrateItemKey};
use blaze_ladder::front2::linked::Linked;
use blaze_ladder::front2::{Front2, FrontError};
use blaze_scope::graph::Sysroot;
use blaze_scope::resolve::{CrateInput, Names, SourceTexts, resolve_crates};
use blaze_scope::task::Serial;
use blaze_surface::Edition;
use blaze_unit_graph::{Implicit, Prelude, StdIs};

const BIN_A: &str = "#![no_core]
struct Mine;
#[global_allocator]
static A: Mine = Mine;
pub fn main() {}
";

const BIN_B: &str = "#![no_core]
struct Mine;
#[global_allocator]
static B: Mine = Mine;
pub fn main() {}
";

const SHARED: &str = "#![no_core]
pub fn helper() {}
";

const BIN_C: &str = "#![no_core]
struct Mine;
#[global_allocator]
static C: Mine = Mine;
pub fn main() {}
";

const ALLOC_LIB: &str = "#![no_core]
pub struct Theirs;
#[global_allocator]
static L: Theirs = Theirs;
";

struct Texts;

impl SourceTexts<'static> for Texts {
    fn read(&self, krate: usize, path: &str) -> Option<&'static str> {
        match (krate, path) {
            (0, "/bin_a/main.rs") => Some(BIN_A),
            (1, "/bin_b/main.rs") => Some(BIN_B),
            (2, "/shared/lib.rs") => Some(SHARED),
            (3, "/bin_c/main.rs") => Some(BIN_C),
            (4, "/alloc_lib/lib.rs") => Some(ALLOC_LIB),
            _ => None,
        }
    }
}

fn input(name: &'static str, root: &'static str, deps: &'static [(&'static str, usize)]) -> CrateInput<'static> {
    CrateInput {
        name,
        key: CrateItemKey(ContentId::of("blaze-ladder lang rows per program test crate", name.as_bytes())),
        edition: Edition::E2021,
        implicit: Implicit { std: StdIs::Rust, prelude: Prelude::Rust(edition::Edition::Edition2021), proc_macro: false, entry: false },
        root,
        shown_from: "/",
        features: &[],
        deps,
        cfg: &[],
        script: None,
    }
}

fn names() -> &'static Names<'static> {
    let crates = [
        input("bin_a", "/bin_a/main.rs", &[("shared", 2)]),
        input("bin_b", "/bin_b/main.rs", &[("shared", 2)]),
        input("shared", "/shared/lib.rs", &[]),
        input("bin_c", "/bin_c/main.rs", &[("alloc_lib", 4)]),
        input("alloc_lib", "/alloc_lib/lib.rs", &[]),
    ];
    let names = resolve_crates(&crates, Sysroot::default(), &Texts, &Serial, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    Box::leak(Box::new(names))
}

/// The front end for the program of `roots`: the crate its `#[global_allocator]` row is of
/// (`None`: no row), or the refusal's text.
fn allocator_of(roots: &[usize]) -> Result<Option<usize>, String> {
    let names = names();
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame");
    let linked = Linked::of(names, roots, blaze_own::testing::heap().alloc()).unwrap_or_else(|e| panic!("the roots {roots:?}: {e}"));
    match Front2::for_program(names, &frame, &[], &linked) {
        Ok(front) => Ok(front.index.lang("global_allocator").map(|key| names.crate_of(key).unwrap_or_else(|| panic!("the row {key:?} is of no crate")))),
        Err(e) => Err(format!("{e}")),
    }
}

/// Each binary's front starts, and its allocator is its own: `bin_a`'s row is crate 0's,
/// `bin_b`'s crate 1's; neither sees the other, and `shared` adds none.
#[test]
fn each_binary_has_its_own_allocator() {
    assert_eq!(allocator_of(&[0]), Ok(Some(0)), "bin_a's program");
    assert_eq!(allocator_of(&[1]), Ok(Some(1)), "bin_b's program");
}

/// The closure is the roots and what they link: `bin_c` links `alloc_lib`, `bin_a` links
/// `shared`, and no binary links another.
#[test]
fn the_closure_is_the_link_graph() {
    let names = names();
    let alloc = blaze_own::testing::heap().alloc();
    let crates = |roots: &[usize]| Linked::of(names, roots, alloc).unwrap_or_else(|e| panic!("{e}")).crates().collect::<Vec<usize>>();
    assert_eq!(crates(&[0]), [0, 2]);
    assert_eq!(crates(&[3]), [3, 4]);
    assert_eq!(crates(&[2]), [2]);
    assert!(Linked::of(names, &[5], alloc).is_err(), "a root past the crates is refused by name");
}

/// One program whose dependency also declares a global allocator: the named duplicate,
/// naming both crates (`bin_c` and `alloc_lib`).
#[test]
fn a_dependency_s_allocator_beside_the_binary_s_is_declared_twice() {
    let names = names();
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame");
    let linked = Linked::of(names, &[3], blaze_own::testing::heap().alloc()).unwrap_or_else(|e| panic!("{e}"));
    match Front2::for_program(names, &frame, &[], &linked) {
        Ok(_) => panic!("bin_c's program has two `#[global_allocator]` and started"),
        Err(FrontError::Lang(twice)) => {
            assert_eq!(twice.name, "global_allocator");
            let mut both = [twice.crates[0].0.as_str(), twice.crates[1].0.as_str()];
            both.sort_unstable();
            assert_eq!(both, ["alloc_lib", "bin_c"], "{twice}");
        }
        Err(e) => panic!("not the duplicate: {e}"),
    }
}

/// A library-only survey (the roots: the library `shared`) files no allocator row.
#[test]
fn a_library_only_survey_has_no_allocator_row() {
    assert_eq!(allocator_of(&[2]), Ok(None));
}

/// The macro side is a separate program: a workspace whose binary `app` uses a member proc
/// macro `derive_it`, built on its OWN core (`wasm_core`, the macro side's: wasm32-wasip1),
/// beside the target's core (`target_core`). Both cores declare `#[lang = "sized"]` (as the
/// two real cores declare `AsyncGenFinished`: pm-small, 2026-11-16 19:51). `app`'s program
/// links `target_core` and never the proc macro or what only it links, so its table has ONE
/// `sized`, the target core's. Control: both sides' crates as one table clash.
#[test]
fn the_macro_side_is_not_joined_into_the_target_program() {
    const APP: &str = "#![no_core]\npub fn main() {}\n";
    const TARGET_CORE: &str = "#![no_core]\n#![feature(lang_items)]\n#[lang = \"sized\"]\npub trait Sized {}\npub struct Word;\nimpl Word {\n    pub fn get() {}\n}\n";
    const WASM_CORE: &str = "#![no_core]\n#![feature(lang_items)]\n#[lang = \"sized\"]\npub trait Sized {}\npub struct Word;\nimpl Word {\n    pub fn get() {}\n}\n";
    const DERIVE_IT: &str = "#![no_core]\npub fn expand() {}\n";
    struct Sides;
    impl SourceTexts<'static> for Sides {
        fn read(&self, krate: usize, path: &str) -> Option<&'static str> {
            match (krate, path) {
                (0, "/app/main.rs") => Some(APP),
                (1, "/target_core/lib.rs") => Some(TARGET_CORE),
                (2, "/derive_it/lib.rs") => Some(DERIVE_IT),
                (3, "/wasm_core/lib.rs") => Some(WASM_CORE),
                _ => None,
            }
        }
    }
    let mut derive_it = input("derive_it", "/derive_it/lib.rs", &[("core", 3)]);
    derive_it.implicit.proc_macro = true;
    let crates = [
        input("app", "/app/main.rs", &[("core", 1), ("derive_it", 2)]),
        input("target_core", "/target_core/lib.rs", &[]),
        derive_it,
        input("wasm_core", "/wasm_core/lib.rs", &[]),
    ];
    let names: &'static Names<'static> = Box::leak(Box::new(
        resolve_crates(&crates, Sysroot::default(), &Sides, &Serial, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}")),
    ));
    let alloc = blaze_own::testing::heap().alloc();
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame");
    let app = Linked::of(names, &[0], alloc).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(app.crates().collect::<Vec<usize>>(), [0, 1], "app links the target core, never the proc macro or its core");
    match Front2::for_program(names, &frame, &[], &app) {
        Ok(front) => {
            assert_eq!(front.index.lang("sized").and_then(|k| names.crate_of(k)), Some(1), "app's `sized` is the target core's");
            // The impls likewise: the target core's `impl Word` is filed, the macro side's is not.
            let of = |krate: usize| front.index.impls.iter().filter(|i| i.krate == krate).count();
            assert_eq!((of(1), of(2), of(3)), (1, 0, 0), "impls by crate: the target core's only");
        }
        Err(e) => panic!("app's program did not start: {e}"),
    }
    // CONTROL: both sides' cores in one table (what joining every crate did).
    let both = Linked::of(names, &[1, 3], alloc).unwrap_or_else(|e| panic!("{e}"));
    match Front2::for_program(names, &frame, &[], &both) {
        Ok(_) => panic!("both cores in one table started"),
        Err(e) => assert!(format!("{e}").contains("the lang item `sized` is declared twice"), "{e}"),
    }
}

/// CONTROL: the two binaries' rows filed as one table (what filing every crate of the
/// workspace did before: tempo's "the lang item `global_allocator` is declared twice"):
/// `bin_a` and `bin_b` clash.
#[test]
fn control_the_two_binaries_as_one_table_clash() {
    let got = allocator_of(&[0, 1]).expect_err("the two binaries' allocators in one table started");
    assert!(got.contains("the lang item `global_allocator` is declared twice"), "{got}");
    assert!(got.contains("crate `bin_a`") && got.contains("crate `bin_b`"), "{got}");
}
