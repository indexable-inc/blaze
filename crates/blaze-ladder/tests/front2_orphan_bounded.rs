//! A refused impl header is a candidate (so a gap) only of the questions the coherence rules
//! let it answer (front2/buckets.rs `PathSelf`, `Homes::local`, `Buckets::bounded`).
//!
//! Tempo's census 7 (`LEFT 6963 check gap: RefusedImpl`): seven impls whose self path was not
//! found (`impl $crate::fmt::Debug for StorageEntry`, `impl<T: EvmTypes> PoolResults<T>`, ...)
//! were filed under their trait's "any" list, so every question of `Debug` (at `Param`, at
//! `&_`, ...) and every inherent question at `Param` had them as candidates and a gap. By the
//! Reference ("Orphan rules"; "Inherent implementations") such an impl in crate C answers only
//! a self type local to C when its trait is another crate's and written with no argument, and
//! an inherent one only a type C defines.
//!
//! The crate `c` (the program) holds `pub struct Local;`, a local trait `Mine`, and four
//! impls whose self path `Missing` names nothing: `core::fmt::Debug` (foreign, bare: bounded),
//! `core::cmp::PartialEq<Local>` (foreign, with an argument that is local: wide), `Mine`
//! (local trait: wide) and an inherent `impl Missing { fn f() {} }` (bounded). Over the solved
//! names (`Buckets::new`, each impl filed from its header's text), each question's candidates
//! are read with the home `trait_home`/`inherent_home` gives.
//!
//! A wrong version fails: before `Homes::local`, `Debug` at `Param` and the inherent question
//! at `Param` had the refused impls as candidates (their homes do not narrow by crate).
//! Never dropped: `Debug` at `c::Local`, at `&_` (`&Local`) and at `Pin<_>` (fundamental).
//!
//! Expected line: `orphan bounded: Debug for Missing: candidate at Local, Ref, Pin; not at u8,
//! Ordering, Param, Closure; PartialEq<Local> and Mine wide; impl Missing at Local only`.
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`, as tests/front2_main.rs does.

#![feature(allocator_api)]

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::buckets::{Ask, Buckets, inherent_home, prepare_home, trait_home};
use blaze_ladder::front2::heads::Store;
use blaze_ladder::front2::keys::Keys;
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::resolve::{Ns, Paths};
use blaze_typeck::{Mutbl, Shape, UintTy};

const EDITION: &str = "blaze-2026.11";

const C: &str = "pub struct Local;
pub trait Mine {
    fn m(&self) {}
}
impl core::fmt::Debug for Missing {}
impl core::cmp::PartialEq<Local> for Missing {}
impl Mine for Missing {}
impl Missing {
    fn f() {}
}
fn main() {}
";

#[test]
fn a_refused_header_is_a_candidate_only_where_coherence_allows() {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/hello-world/src/main.rs".to_string(), C.to_string())];
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
    let user = UserCrate { name: "hello_world", dir: "/hello-world/", root: "/hello-world/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: C }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
    let front = Front2::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let root = names.root(0);
    let mut keys = Keys::new();
    let checker = |path: &[&str]| {
        let lib = Paths { names: &names }.path(root, false, path, Ns::Type).unwrap_or_else(|e| panic!("`{path:?}` from the crate root: {e}")).key;
        keys.item(lib).unwrap_or_else(|e| panic!("filing `{path:?}`: {e}"))
    };
    let debug = checker(&["core", "fmt", "Debug"]);
    let partial_eq = checker(&["core", "cmp", "PartialEq"]);
    let mine = checker(&["Mine"]);
    let local = Shape::Adt(checker(&["Local"]));
    let ordering = Shape::Adt(checker(&["core", "cmp", "Ordering"]));
    let pin = Shape::Adt(checker(&["core", "pin", "Pin"]));
    let u8_ = Shape::Uint(UintTy::U8);

    // The program's four refused impls, by their header's text.
    let refused = |of: &str| {
        let found: Vec<_> = front.index.impls.iter().filter(|at| at.krate == 0 && at.header.contains("Missing") && at.header.contains(of)).map(|at| at.key).collect();
        assert_eq!(found.len(), 1, "one impl of the program has `{of}` and `Missing` in its header: {found:?}");
        found[0]
    };
    let debug_impl = refused("Debug");
    let eq_impl = refused("PartialEq");
    let mine_impl = refused("Mine");
    let inherent_impl = {
        let found: Vec<_> = front.index.impls.iter().filter(|at| at.krate == 0 && at.header.contains("Missing") && !at.header.contains(" for ")).map(|at| at.key).collect();
        assert_eq!(found.len(), 1, "one inherent impl of `Missing`: {found:?}");
        found[0]
    };

    let mut store = Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global());
    let b = Buckets::new(&names, &front.index);
    let mut candidates = |ask| {
        prepare_home(&names, &mut keys, &mut store, ask).unwrap_or_else(|e| panic!("{ask:?}: prepare coherence: {e}"));
        match ask {
            Ask::Trait(trait_, shape) => {
                let home = trait_home(&names, &keys, &store, trait_, shape).unwrap_or_else(|e| panic!("{ask:?}: trait home: {e}"));
                b.trait_candidates(trait_, shape, home)
            }
            Ask::Inherent(shape) => {
                let home = inherent_home(&names, &keys, &store, shape).unwrap_or_else(|e| panic!("{ask:?}: inherent home: {e}"));
                b.inherent_candidates(shape, home)
            }
        }
    };

    // `Debug` (core's, written bare) for a self path that names nothing: only where the self
    // type can be `c`'s local type.
    for (shape, may) in [(local, true), (Shape::Ref(Mutbl::Not), true), (pin, true), (u8_, false), (ordering, false), (Shape::Param, false), (Shape::Closure, false)] {
        let got = candidates(Ask::Trait(debug, shape)).contains(&debug_impl);
        assert_eq!(got, may, "`impl Debug for Missing` a candidate of `Debug` at {shape:?}: {got}, the rules say {may}");
    }
    // A foreign trait written with an argument (which may be the local type) and a local trait:
    // a candidate wherever its trait's crates allow, as before.
    for shape in [u8_, Shape::Param, local] {
        assert!(candidates(Ask::Trait(partial_eq, shape)).contains(&eq_impl), "`impl PartialEq<Local> for Missing` dropped at {shape:?}");
        assert!(candidates(Ask::Trait(mine, shape)).contains(&mine_impl), "`impl Mine for Missing` dropped at {shape:?}");
    }
    // An inherent impl: only of a type `c` defines.
    for (shape, may) in [(local, true), (Shape::Param, false), (ordering, false), (u8_, false), (Shape::Ref(Mutbl::Not), false)] {
        let got = candidates(Ask::Inherent(shape)).contains(&inherent_impl);
        assert_eq!(got, may, "`impl Missing` a candidate of the inherent question at {shape:?}: {got}, the rules say {may}");
    }
    println!("orphan bounded: Debug for Missing: candidate at Local, Ref, Pin; not at u8, Ordering, Param, Closure; PartialEq<Local> and Mine wide; impl Missing at Local only");
}
