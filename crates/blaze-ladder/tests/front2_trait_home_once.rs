//! A trait question's home is the one its trait's head gives, whatever was made before it
//! (front2/check.rs `make_once`, `Fill::Impls`; rule 21).
//!
//! `trait_home` narrows an operator question by the trait's head (`Add<Rhs = Self>`: an impl
//! that writes no argument is written in core or in the self type's crate) and reads only the
//! text's parameter count when the head is not made yet. `a + b` over the user's `P` asks
//! `P: Add` through its lang item, before anything has made `Add`'s head; its candidates'
//! headers make that head. Before the fix the first answer was filed with the wide home and a
//! later answer of the same question (or the same body checked over a store that held the head)
//! with the narrow one: two answers under one key, as k256's `<impl ProjectivePoint>::GENERATOR`
//! met in tempo's census 7 ("checking .. again read other answers than its first check").
//!
//! So: `adds` is checked over a fresh store, and over a second fresh store whose `Add` head is
//! made first. Both must file the home the head gives, and the same read-list digest.
//!
//! Expected line: `trait home once: P: Add homed by its head in both orders, one digest`.
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`, as tests/front2_main.rs does.

#![feature(allocator_api)]

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::body::lower_fn;
use blaze_ladder::front2::buckets::trait_home;
use blaze_ladder::front2::check::{BuildOrder, Records, check_body};
use blaze_ladder::front2::consts::Consts;
use blaze_ladder::front2::heads::{Store, Want};
use blaze_ladder::front2::keys::checker_of;
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::refusal::SourceFile;
use blaze_lower::scoped::Prepares;
use blaze_own::Alloc;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};
use blaze_typeck::Shape;

const EDITION: &str = "blaze-2026.11";

const BODY: &str = "use core::ops::Add;\nstruct P(u32);\nimpl Add for P {\n    type Output = P;\n    fn add(self, o: P) -> P {\n        P(self.0.wrapping_add(o.0))\n    }\n}\nfn adds(a: P, b: P) -> P {\n    a + b\n}\nfn main() {}\n";

#[test]
fn a_trait_question_is_homed_by_its_head_in_any_order() {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/hello-world/src/main.rs".to_string(), BODY.to_string())];
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
    let user = UserCrate { name: "hello_world", dir: "/hello-world/", root: "/hello-world/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: BODY }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
    let mut front = Front2::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let key = |ns: Namespace, path: &[&str]| {
        let found = names::path(&names, names.root(0), ns, Written { kind: PathKind::Plain, segments: path }, Alloc::global())
            .unwrap_or_else(|e| panic!("`{path:?}`: {e}"));
        found.whole(path.len()).unwrap_or_else(|| panic!("`{path:?}` not named")).key
    };
    let adds = key(Namespace::Values, &["adds"]);
    let add = key(Namespace::Types, &["core", "ops", "Add"]);
    let p = key(Namespace::Types, &["P"]);
    let shape = Shape::Adt(checker_of(p));
    let prepared = Prepares::new_in(Alloc::global());
    let sized = front.index.lang("sized");

    let mut digests = Vec::new();
    for head_first in [false, true] {
        let (mut store, mut buckets, mut consts) = (Store::new(sized, front.frame, blaze_own::Alloc::global()), None, Consts::default());
        if head_first {
            store.ensure(&names, &mut front.keys, Want::Head(add)).unwrap_or_else(|e| panic!("`Add`'s head: {e:?}"));
        }
        let body = lower_fn(&names, prepared, &mut front.keys, &mut store, adds).unwrap_or_else(|e| panic!("lowering `adds`: {e}"));
        let view = body.lowered.view();
        let spell = Spelling::of(&view).unwrap_or_else(|e| panic!("spelling `adds`: {e:?}"));
        let scope = body.scope(&spell);
        let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: Some((&mut consts, prepared)) };
        check_body(&mut records, &body, &scope).unwrap_or_else(|e| panic!("checking `adds` (head first: {head_first}): {e}"));
        let b = buckets.as_ref().expect("`a + b` asked a trait question");
        assert!(b.impls_of(checker_of(add), shape).is_some(), "head first: {head_first}: `P: Add` was not answered");
        let by_head = trait_home(&names, &front.keys, &store, checker_of(add), shape);
        assert_eq!(b.home_of(checker_of(add), shape), by_head, "head first: {head_first}: `P: Add` was answered with another home than its trait's head gives");
        digests.push(store.reads.get(body.owner).expect("`adds` checked clean filed a digest"));
    }
    assert_eq!(digests[0], digests[1], "`adds` read other answers over a store that made `Add`'s head first");
    println!("trait home once: P: Add homed by its head in both orders, one digest");
}
