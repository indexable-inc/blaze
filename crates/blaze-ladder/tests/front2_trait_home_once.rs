//! A trait question's home is the one its trait's head gives, whatever was made before it
//! (front2/check.rs `make_once`, `Fill::Impls`; rule 21).
//!
//! `trait_home` narrows an operator question by the trait's head (`Add<Rhs = Self>`: an impl
//! that writes no argument is written in core or in the self type's crate). `a + b` over the
//! user's `P` asks `P: Add` through its lang item, before anything has made `Add`'s head; its
//! candidates' headers make that head. A checked body's answer is filed under its crate's sight,
//! while `impls_of` is the whole-program answer used after checking, so this test reads the body
//! sight explicitly. Before the fix the first answer was filed with the wide home and a later
//! answer of the same question (or the same body checked over a store that held the head) with
//! the narrow one: two answers under one key, as k256's `<impl ProjectivePoint>::GENERATOR` met
//! in tempo's census 7 ("checking .. again read other answers than its first check").
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
use blaze_ladder::front2::buckets::{Seen, trait_home};
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

fn with_front(text: &str, check: impl FnOnce(Front2<'_>)) {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/hello-world/src/main.rs".to_string(), text.to_string())];
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
    let user = UserCrate { name: "hello_world", dir: "/hello-world/", root: "/hello-world/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
    let front = Front2::new(&names, &frame, &sources, Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    check(front);
}

fn key<'n>(front: &Front2<'n>, ns: Namespace, path: &[&'n str]) -> blaze_id::ItemKey {
    let names = front.names;
    let found = names::path(names, names.root(0), ns, Written { kind: PathKind::Plain, segments: path }, Alloc::global())
        .unwrap_or_else(|e| panic!("`{path:?}`: {e}"));
    found.whole(path.len()).unwrap_or_else(|| panic!("`{path:?}` not named")).key
}

#[test]
fn a_trait_question_is_homed_by_its_head_in_any_order() {
    with_front(BODY, check_add_orders);
}

fn check_add_orders(mut front: Front2<'_>) {
    let names = front.names;
    let adds = key(&front, Namespace::Values, &["adds"]);
    let add = key(&front, Namespace::Types, &["core", "ops", "Add"]);
    let p = key(&front, Namespace::Types, &["P"]);
    let shape = Shape::Adt(checker_of(p));
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let sized = front.index.lang("sized");

    let mut digests = Vec::new();
    for head_first in [false, true] {
        let (mut store, mut buckets, mut consts) = (Store::new(sized, front.frame, blaze_own::Alloc::global()), None, Consts::default());
        if head_first {
            store.ensure(names, &mut front.keys, Want::Head(add)).unwrap_or_else(|e| panic!("`Add`'s head: {e:?}"));
        }
        let body = lower_fn(names, prepared, &mut front.keys, &mut store, adds).unwrap_or_else(|e| panic!("lowering `adds`: {e}"));
        let view = body.lowered.view();
        let spell = Spelling::of(&view).unwrap_or_else(|e| panic!("spelling `adds`: {e:?}"));
        let scope = body.scope(&spell);
        let seen = Seen::of_checked_body(names, scope.module).unwrap_or_else(|e| panic!("the body's impl sight: {e}"));
        let mut records = Records { names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: Some((&mut consts, prepared)) };
        check_body(&mut records, &body, &scope).unwrap_or_else(|e| panic!("checking `adds` (head first: {head_first}): {e}"));
        let b = buckets.as_ref().expect("`a + b` asked a trait question");
        assert!(b.impls_seen(checker_of(add), shape, seen).is_some(), "head first: {head_first}: `P: Add` was not answered");
        let by_head = trait_home(names, &front.keys, &store, checker_of(add), shape).unwrap_or_else(|e| panic!("trait home: {e}"));
        assert_eq!(b.home_of(checker_of(add), shape), by_head, "head first: {head_first}: `P: Add` was answered with another home than its trait's head gives");
        digests.push(store.reads.get(body.owner).expect("`adds` checked clean filed a digest"));
    }
    assert_eq!(digests[0], digests[1], "`adds` read other answers over a store that made `Add`'s head first");
    println!("trait home once: P: Add homed by its head in both orders, one digest");
}

#[test]
fn question_mark_propagates_a_dependency_adt_error() {
    // Result's FromResidual impl converts the error through From<Io> for Io.
    // Neither argument needs a constructor or a field access to preload Io's row.
    let text = "use std::Io;
fn propagate(first: Result<(), Io>, next: Result<(), Io>) -> Result<(), Io> {
    first?;
    next
}
fn main() {}
";
    with_front(text, |mut front| {
        let propagate = key(&front, Namespace::Values, &["propagate"]);
        let io = key(&front, Namespace::Types, &["std", "Io"]);
        let alloc = Alloc::global();
        let mut store = Store::new(front.index.lang("sized"), front.frame, alloc);
        let prepared = Prepares::new_in(blaze_own::testing::bump());
        let body = lower_fn(front.names, prepared, &mut front.keys, &mut store, propagate).expect("lower propagate");
        assert!(!store.made(Want::Adt(io)), "the question must start with Io's ADT row absent");
        let view = body.lowered.view();
        let spell = Spelling::of(&view).expect("spell propagate");
        let scope = body.scope(&spell);
        let mut buckets = None;
        let mut consts = Consts::default();
        let mut records = Records {
            names: front.names, keys: &mut front.keys, index: &front.index, store: &mut store,
            buckets: &mut buckets, order: BuildOrder::AsAsked, consts: Some((&mut consts, prepared)),
        };
        check_body(&mut records, &body, &scope).unwrap_or_else(|e| panic!("checking propagate: {e}"));
        assert!(store.made(Want::Adt(io)), "the body must have obtained Io's coherence marks");
        println!("question mark: Result<(), Io> propagates its dependency error");
    });
}

#[test]
fn a_field_length_can_read_its_own_types_inherent_constant() {
    use blaze_ladder::front2::receiver::Pointers;
    use blaze_ladder::front2::tables::head::{HeadCells, SeededHeads, SeededHeadsOpts, StoreConsts, file_frozen};
    use blaze_typeck::{PNode, UintTy};

    let text = "struct S([u8; S::MAX as usize]);
impl S { const MAX: u8 = 3; }
fn f() -> [u8; S::MAX as usize] { [0; 3] }
fn main() {}
";
    with_front(text, |front| {
        let s = key(&front, Namespace::Types, &["S"]);
        let f = key(&front, Namespace::Values, &["f"]);
        let alloc = Alloc::global();
        let sized = front.index.lang("sized");
        let prepared = Prepares::new_in(blaze_own::testing::bump());
        let mut evaluator = StoreConsts { names: front.names, index: &front.index, prepared };
        let mut heads = SeededHeads::new(SeededHeadsOpts {
            names: front.names, sized, pointers: Pointers::NOT_LOOKED_UP, frame: front.frame,
            scratch: alloc, alloc, consts: Some(&mut evaluator), known: None, owners: None,
        });
        let store = Store::new(sized, front.frame, alloc);
        for want in [Want::Adt(s), Want::Fn(f)] {
            let (frozen, _) = heads.cell(want).unwrap_or_else(|e| panic!("freezing {want:?}: {e}"));
            file_frozen(front.names, want, frozen, &store, &front.keys, alloc).expect("import the completed head");
        }
        let field = store.adts.get(&checker_of(s)).expect("S").fields[0].ty;
        let output = store.fns.get(&checker_of(f)).expect("f").output;
        let tys = store.tys.view();
        for ty in [field, output] {
            let PNode::Array(elem, len) = tys.node(ty).expect("the frozen type") else { panic!("expected an array") };
            assert_eq!(tys.node(elem).expect("the element"), PNode::Uint(UintTy::U8));
            assert_eq!(tys.value_u64(len), Some(3));
        }
        println!("S's field and f's result both freeze as [u8; 3]");
    });
}
