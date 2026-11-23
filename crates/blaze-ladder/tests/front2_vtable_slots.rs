//! Every slot of a vtable the program makes is reached, whether or not a call names it
//! (front2/reach/instances/slots.rs; rung x0-dyn-03-uncalled-method).
//!
//! The Reference, "Trait objects": each pointer to a trait object holds "a virtual method
//! table ... which contains, for each method of SomeTrait and its supertraits that T
//! implements, a pointer to T's implementation". The Reference, "Dyn compatibility": a method
//! with a `where Self: Sized` bound is "explicitly non-dispatchable": it has no slot.
//!
//! The program, walked from `run`: `trait Tr: Base` with `req` (required), `m` and `k`
//! (provided) and `sized_only` (`where Self: Sized`); `Base` with `base` and a provided
//! `base_sized` (`where Self: Sized`); `A`, `B` and `C` implement both (`B` overrides `m`);
//! `A` also implements `Other`. `call(t: &dyn Tr)` calls only `t.m()`; `run` coerces `&A` and
//! `&B` to `&dyn Tr` and writes a `C` it never coerces.
//!
//! So the tables are `(A, Tr)` and `(B, Tr)`, each holding `base`, `req`, `m` and `k`: the impl
//! fns reached are `A::base A::req B::base B::m B::req` (`A`'s `m` and both `k`s are the trait's
//! provided bodies: `Tr::k Tr::m`). Not reached: `sized_only` and `base_sized` (no slot), any of
//! `C`'s (never coerced), `A::other` (`Other` is not in `dyn Tr`'s table).
//!
//! Line it prints (and asserts):
//! `slots: reached A::base A::req B::base B::m B::req; provided Tr::k Tr::m`.
//! Without the slots (the walk reaching only the methods called on `dyn`, the cause removed) it
//! prints `slots: reached B::m; provided Tr::m`.
//!
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`, as tests/front2_clone.rs
//! does. NOT RUN by the writer (a write-only lane).

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::body::lower_fn;
use blaze_ladder::front2::check::{BuildOrder, Records, check_body};
use blaze_ladder::front2::heads::{Store, Want};
use blaze_ladder::front2::keys::Origin;
use blaze_ladder::front2::reach::{First, Reached, reach};
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::refusal::SourceFile;
use blaze_lower::scoped::Prepares;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};
use blaze_typeck::{ItemKey, PNode};

const SLOTS: &str = "fn main(cx: &Cx) -> Result<(), Error> {
    Ok(())
}

trait Base {
    fn base(&self) -> i32;
    fn base_sized(&self) -> i32 where Self: Sized { 9 }
}

trait Tr: Base {
    fn req(&self) -> i32;
    fn m(&self) -> i32 { 1 }
    fn k(&self) -> i32 { 77 }
    fn sized_only(&self) -> i32 where Self: Sized;
}

trait Other {
    fn other(&self) -> i32;
}

struct A;
struct B;
struct C;

impl Base for A {
    fn base(&self) -> i32 { 10 }
}

impl Base for B {
    fn base(&self) -> i32 { 20 }
}

impl Base for C {
    fn base(&self) -> i32 { 30 }
}

impl Tr for A {
    fn req(&self) -> i32 { 5 }
    fn sized_only(&self) -> i32 { 6 }
}

impl Tr for B {
    fn req(&self) -> i32 { 7 }
    fn m(&self) -> i32 { 2 }
    fn sized_only(&self) -> i32 { 8 }
}

impl Tr for C {
    fn req(&self) -> i32 { 3 }
    fn sized_only(&self) -> i32 { 4 }
}

impl Other for A {
    fn other(&self) -> i32 { 11 }
}

fn call(t: &dyn Tr) -> i32 {
    t.m()
}

fn run() -> i32 {
    let _c = C;
    call(&A) + call(&B)
}
";

#[test]
fn every_slot_of_a_made_table_is_reached() {
    let heap = blaze_own::testing::heap();
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let (dir, root) = ("/slots/", "/slots/src/main.rs");
    let files = [(String::from(root), String::from(SLOTS))];
    let edition = edition::PackageEdition::of_manifest("blaze-2026.11").expect("a known edition");
    let user = UserCrate { name: "slots", dir, root, files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: SLOTS }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
    let mut front = Front2::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global());
    let lib = |ns: Namespace, name: &'static str| {
        names::path(&names, names.root(0), ns, Written { kind: PathKind::Plain, segments: &[name] }, heap.alloc())
            .ok()
            .and_then(|f| f.whole(1))
            .map(|d| d.key)
            .unwrap_or_else(|| panic!("`{name}` from the crate root"))
    };
    let run = lib(Namespace::Values, "run");
    let key = |ns: Namespace, n: &'static str| -> (ItemKey, &'static str) { (front.keys.item(lib(ns, n)).unwrap_or_else(|e| panic!("{n}'s key: {e:?}")), n) };
    let types: Vec<(ItemKey, &'static str)> = ["A", "B", "C"].iter().map(|&t| key(Namespace::Types, t)).collect();
    let traits: Vec<(ItemKey, &'static str)> = ["Base", "Tr", "Other"].iter().map(|&t| key(Namespace::Types, t)).collect();
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut buckets = None;
    store.ensure(&names, &mut front.keys, Want::Fn(run)).unwrap_or_else(|e| panic!("run's head: {e:?}"));
    let body = lower_fn(&names, prepared, &mut front.keys, &mut store, run).unwrap_or_else(|e| panic!("run lowers: {e}"));
    let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: None };
    let view = body.lowered.view();
    let spell = Spelling::of(&view).expect("run's spelling");
    let scope = body.scope(&spell);
    let mut checked = check_body(&mut records, &body, &scope).unwrap_or_else(|e| panic!("run checks: {e:?}"));
    let census = reach(&mut records, prepared, body.owner, First::CheckedBody(&body, &mut checked), &scope);
    let shown = |ks: &[(ItemKey, &'static str)], k: ItemKey| ks.iter().find(|(t, _)| *t == k).map(|(_, n)| *n);
    let mut impl_fns: Vec<String> = Vec::new();
    let mut provided: Vec<String> = Vec::new();
    for (k, outcome) in census.fns.iter() {
        let Some(Origin::Member { container, name, .. }) = records.keys.origin(*k) else { continue };
        if let Some(t) = shown(&traits, *container) {
            assert!(matches!(outcome, Reached::Checked | Reached::AtObject), "{t}::{name}: {outcome:?}");
            if matches!(outcome, Reached::Checked) {
                provided.push(format!("{t}::{name}"));
            }
            continue;
        }
        let Some(h) = records.store.impls.get(container) else { continue };
        if !h.trait_.is_some_and(|tr| shown(&traits, tr).is_some()) {
            continue;
        }
        let Some(PNode::Adt(a, _)) = records.store.tys.view().get(h.self_ty) else { continue };
        let Some(ty) = shown(&types, *a) else { continue };
        assert!(matches!(outcome, Reached::Checked), "{ty}::{name}: {outcome:?}");
        impl_fns.push(format!("{ty}::{name}"));
    }
    impl_fns.sort();
    impl_fns.dedup();
    provided.sort();
    provided.dedup();
    let line = format!("slots: reached {}; provided {}", impl_fns.join(" "), provided.join(" "));
    eprintln!("{line}");
    assert_eq!(line, "slots: reached A::base A::req B::base B::m B::req; provided Tr::k Tr::m");
}
