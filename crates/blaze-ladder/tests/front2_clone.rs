//! `Clone` through an `Arc` and a derive, on front2 with the real core and alloc: the census's
//! "diag E0277: Clone" (59 bstd bodies; `Arc<Attempt<..>, A>: Clone` in bstd::sync::flight and
//! `Allowed<DeviceName>: Clone` in bstd::device), reduced. bstd compiles under the pinned
//! compiler, so each body here must check clean.
//!
//! The pinned library: `pub struct Arc<T: ?Sized, A: Allocator = Global>` and
//! `impl<T: ?Sized, A: AllocatorClone> Clone for Arc<T, A>` (alloc/src/rcs/arc.rs);
//! `pub unsafe trait AllocatorClone: Allocator + Clone {}` (core/src/alloc/mod.rs);
//! `unsafe impl core::alloc::AllocatorClone for Global {}` (alloc/src/alloc.rs). The Reference,
//! "Generic parameters": a type parameter's default fills an omitted argument; "Traits",
//! "Supertraits": "Supertraits are traits that are required to be implemented for a type to
//! implement a specific trait. Furthermore, anywhere a generic or trait object is bounded by a
//! trait, it has access to the associated items of its supertraits." `#[derive(Clone)]` on a
//! generic type writes `impl<T: Clone> Clone for Ty<T>` (core/src/clone.rs, "Derivable").
//! tempo's "347 diag E0308" (`found &MCall, expected MCall`): alloy's `sol!` writes a function
//! with no parameters as `#[derive(Clone)] pub struct MCall;` (alloy-sol-macro-expander
//! expand/function.rs), and the derive writes `fn clone(&self) -> Self { match self { MCall =>
//! MCall, } }`. The Reference, "Identifier patterns": "Path patterns take precedence over
//! identifier patterns", and "Path patterns": a path to a unit struct is a pattern; so `MCall`
//! there matches the struct (by default binding mode through `&MCall`), binds nothing, and the
//! arm's `MCall` is the struct's value.
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`, as tests/front2_panics.rs.

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::body::{lower_fn, lower_member};
use blaze_ladder::front2::check::{BuildOrder, CheckFail, Records, check_body};
use blaze_ladder::front2::heads::{Store, Want};
use blaze_ladder::front2::keys::{MemberKind, checker_of};
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::refusal::SourceFile;
use blaze_lower::scoped::Prepares;
use blaze_own::Alloc;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};

const MAIN: &str = "fn main(cx: &Cx, io: Io) -> Result<(), Error> {
    Ok(())
}
extern crate alloc;
use alloc::sync::Arc;
fn f1<T>(a: &Arc<T>) -> Arc<T> { a.clone() }
fn f1b<T>(a: &Arc<T>) -> Arc<T> { Clone::clone(a) }
fn f2<T, A: core::alloc::AllocatorClone>(a: &Arc<T, A>) -> Arc<T, A> { Clone::clone(a) }
fn f3<A: core::alloc::AllocatorClone>(a: &A) -> A { Clone::clone(a) }
fn f4(a: &Arc<[u8]>) -> Arc<[u8]> { Clone::clone(a) }
fn f5(a: &Al<N>) -> Al<N> { Clone::clone(a) }
fn need<V: Clone>(v: V) -> V { v }
fn f6<T, A: core::alloc::AllocatorClone>(a: Arc<T, A>) -> Arc<T, A> { need(a) }
#[derive(Clone)]
struct N(Arc<str>);
#[derive(Clone)]
enum Al<T> {
    Any,
    Only(Arc<[T]>),
}
#[derive(Clone)]
struct Cam {
    allowed: Al<N>,
}
macro_rules! typed {
    ($name:ident) => {
        #[derive(Clone)]
        pub struct $name(Arc<str>);
    };
}
typed!(Dn);
fn f7(a: &Al<Dn>) -> Al<Dn> { Clone::clone(a) }
struct Tb<K, H, A: core::alloc::Allocator> {
    k: Option<K>,
    h: Option<H>,
    a: A,
}
impl<K: Eq, H: Clone, A: core::alloc::Allocator> Tb<K, H, A> {
    fn new_in(a: A) -> Tb<K, H, A> { Tb { k: None, h: None, a } }
}
struct Fl<K, V, A: core::alloc::AllocatorClone> {
    t: Tb<K, Arc<Option<V>, A>, A>,
    a: A,
}
impl<K: Eq + Clone, V: Clone, A: core::alloc::AllocatorClone> Fl<K, V, A> {
    fn planted(a: A) -> Fl<K, V, A> {
        let t = Tb::new_in(a.clone());
        Fl { t, a }
    }
}
";

/// The free bodies that must check clean, each with what it proves.
const CLEAN: [(&str, &str); 9] = [
    ("f7", "a derive written by a `macro_rules!` expansion (bstd's `typed_name!`)"),
    ("f1", "`Arc<T>` (allocator defaulted to `Global`) is `Clone` by method"),
    ("f1b", "`Arc<T>: Clone` by path: `Global: AllocatorClone`"),
    ("f2", "`Arc<T, A>: Clone` from `A: AllocatorClone` in the where clauses"),
    ("f3", "`A: Clone` from `A: AllocatorClone`'s supertrait"),
    ("f4", "`Arc<[u8]>: Clone`: an unsized `T`"),
    ("f5", "a derived generic enum at a concrete type"),
    ("f6", "`Arc<T, A>: Clone` as a callee's bound (flight's `Table::with_capacity_in`)"),
    ("need", "a plain bound"),
];

#[test]
fn clones_through_front2() {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/cln/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest("blaze-2026.11").expect("a known edition");
    let user = UserCrate { name: "cln", dir: "/cln/", root: "/cln/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: MAIN }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
        let mut front = Front2::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global());
    let find = |krate: usize, segments: &[&str]| {
        names::path(&names, names.root(krate), Namespace::Values, Written { kind: PathKind::Plain, segments }, Alloc::global())
            .ok()
            .and_then(|f| f.whole(1))
            .map(|d| d.key)
            .unwrap_or_else(|| panic!("`{segments:?}` from crate {krate}'s root"))
    };
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut buckets = None;
    let mut failed: Vec<String> = Vec::new();
    let mut clean: Vec<String> = Vec::new();
    // What happened to each body that failed, carried into the assertion's message.
    let mut notes: Vec<String> = Vec::new();
    let mut bodies = Vec::new();
    for (name, proves) in CLEAN {
        let key = find(0, &[name]);
        if let Err(e) = store.ensure(&names, &mut front.keys, Want::Fn(key)) {
            notes.push(format!("{name}: no head: {e:?}"));
            failed.push(name.to_string());
            continue;
        }
        match lower_fn(&names, prepared, &mut front.keys, &mut store, key) {
            Ok(b) => bodies.push((name.to_string(), b, proves)),
            Err(e) => {
                notes.push(format!("{name}: does not lower: {e}"));
                failed.push(name.to_string());
            }
        }
    }
    // The derived `clone` members.
    let mut derived = 0;
    for (k, _, _) in names.items(0) {
        let Some(header) = names.item_text(k).and_then(|t| t.impl_header) else { continue };
        if let Err(e) = store.ensure(&names, &mut front.keys, Want::Impl(k)) {
            notes.push(format!("impl `{header}`: no head: {e:?}"));
            failed.push(header.to_string());
            continue;
        }
        // `planted`: bstd::sync::flight's shape: the callee's `H: Clone` is asked while `H` is
        // a variable the struct literal fixes later.
        let Some((member, mk)) = ["clone", "planted"]
            .into_iter()
            .find_map(|m| front.keys.filed_member(checker_of(k), MemberKind::Fn, m).map(|mk| (m, mk)))
        else {
            continue;
        };
        derived += usize::from(member == "clone");
        match lower_member(&names, prepared, &mut front.keys, &mut store, mk) {
            Ok(b) => bodies.push((format!("<{header}>::{member}"), b, "a derived `clone`, or flight's `planted`")),
            Err(e) => {
                notes.push(format!("{header}: clone does not lower: {e}"));
                failed.push(header.to_string());
            }
        }
    }
    for (name, body, proves) in &bodies {
        let view = body.lowered.view();
        let spell = Spelling::of(&view).expect("spelling");
        let scope = body.scope(&spell);
        let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: None };
        match check_body(&mut records, body, &scope) {
            Ok(done) if done.diags.is_empty() => clean.push(name.clone()),
            Ok(done) => {
                notes.push(format!("{name}: Ok with {} diagnostics ({proves})", done.diags.len()));
                failed.push(name.clone());
            }
            Err(CheckFail::Diags { diags, .. }) => {
                notes.push(format!("{name}: diagnostics {diags:?} ({proves})"));
                failed.push(name.clone());
            }
            Err(other) => {
                notes.push(format!("{name}: {other:?} ({proves})"));
                failed.push(name.clone());
            }
        }
    }
    notes.push(format!("clean: {clean:?}"));
    assert_eq!(derived, 4, "the four derived `clone` members were found");
    assert!(bodies.iter().any(|b| b.0.ends_with("::planted")), "`Fl::planted` was taken");
    assert!(failed.is_empty(), "bodies that must check clean did not: {failed:?}\n{}", notes.join("\n"));
}

/// tempo's "347 diag E0308" reduced: a function of a `sol!` interface with no parameters is
/// `#[derive(Clone)] pub struct MCall;` (alloy-sol-macro-expander 1.7.3, expand/function.rs:
/// `pub struct #call_name;`), and the derive writes `match self { MCall => MCall, }`.
const UNIT: &str = "fn main(cx: &Cx, io: Io) -> Result<(), Error> {
    Ok(())
}
#[derive(Clone)]
pub struct MCall;
#[derive(Clone)]
pub struct NCall;
#[derive(Clone)]
pub enum Calls {
    M(MCall),
    N(NCall),
}
fn by_hand(a: &MCall) -> MCall { match a { MCall => MCall } }
pub enum E { A, B }
use E::*;
const LIMIT: u8 = 3;
static ST: u8 = 1;
pub struct Tup(pub u8);
fn prelude(o: Option<u8>) -> u8 { match o { None => 0, Some(v) => v } }
fn glob(e: E) -> u8 { match e { A => 1, B => 2 } }
fn module_const(n: u8) -> u8 { match n { LIMIT => 1, _ => 0 } }
fn local_const(n: u8) -> u8 { const LOCAL: u8 = 4; match n { LOCAL => 1, _ => 0 } }
fn let_unit(x: &MCall) -> MCall { let MCall = x; MCall }
fn param_unit(MCall: MCall) -> MCall { MCall }
fn ordinary(n: u8) -> u8 { match n { other => other } }
fn ref_unit(x: MCall) -> u8 { match x { ref MCall => 0 } }
fn static_pat(n: u8) -> u8 { match n { ST => 0, _ => 1 } }
fn tuple_pat(t: Tup) -> u8 { match t { Tup => 0 } }
";

/// Each case of the Reference's "Identifier patterns" rule, by the fn that writes it, with what
/// its body must give: `Ok(binds)` (checks clean; `binds` whether a binding of the case's name is
/// left in the record) or `Err(())` (a lowering error, E0530).
const CASES: [(&str, &str, Result<bool, ()>); 11] = [
    ("by_hand", "MCall", Ok(false)),
    ("prelude", "None", Ok(false)),
    ("glob", "A", Ok(false)),
    ("module_const", "LIMIT", Ok(false)),
    ("local_const", "LOCAL", Ok(false)),
    ("let_unit", "MCall", Ok(false)),
    ("param_unit", "MCall", Ok(false)),
    ("ordinary", "other", Ok(true)),
    ("ref_unit", "MCall", Err(())),
    ("static_pat", "ST", Err(())),
    ("tuple_pat", "Tup", Err(())),
];

/// The Reference, "Identifier patterns": "Path patterns take precedence over identifier
/// patterns"; "Path patterns": a path to a unit struct is a pattern, and it matches through a
/// reference by default binding mode ("Binding modes"). So `MCall` in `match self { MCall =>
/// MCall }` binds nothing, and the arm's `MCall` is the unit struct's value: each body is clean.
/// Today the lowering takes the pattern's `MCall` for a binding (blaze-lower's machine.rs makes
/// it `Pat::Bind { candidate: true }`, and no step reads `candidate`), and the arm's `MCall`
/// resolves to that binding: "found Some(\"&MCall\"), expected Some(\"MCall\")" for `by_hand`
/// and both derived `clone`s. Owner: blaze-lower's scoped resolution ("Items inside bodies").
#[test]
fn unit_struct_patterns_through_front2() {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/unt/src/main.rs".to_string(), UNIT.to_string())];
    let edition = edition::PackageEdition::of_manifest("blaze-2026.11").expect("a known edition");
    let user = UserCrate { name: "unt", dir: "/unt/", root: "/unt/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: UNIT }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
        let mut front = Front2::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global());
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut buckets = None;
    let mut notes: Vec<String> = Vec::new();
    let mut bodies = Vec::new();
    let mut outcomes: Vec<String> = Vec::new();
    for (f, bound, want) in CASES {
        let key = names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: &[f] }, Alloc::global())
            .ok()
            .and_then(|found| found.whole(1))
            .map(|d| d.key)
            .unwrap_or_else(|| panic!("`{f}` from the crate's root"));
        store.ensure(&names, &mut front.keys, Want::Fn(key)).unwrap_or_else(|e| panic!("{f}: no head: {e:?}"));
        let got: Result<bool, String> = match lower_fn(&names, prepared, &mut front.keys, &mut store, key) {
            Err(e) => Err(format!("{e:?}")),
            Ok(body) => {
                let view = body.lowered.view();
                let binds = (0..view.pats.len()).any(|i| {
                    matches!(view.pat(blaze_lower::PatId(i as u32)), Ok(blaze_lower::Pat::Bind { name, .. }) if view.name_text(name) == Ok(bound))
                });
                let spell = Spelling::of(&view).expect("spelling");
                let scope = body.scope(&spell);
                let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: None };
                match check_body(&mut records, &body, &scope) {
                    Ok(done) if done.diags.is_empty() => Ok(binds),
                    Ok(done) => Err(format!("binds {bound}: {binds}; Ok with {} diagnostics", done.diags.len())),
                    Err(CheckFail::Diags { diags, .. }) => Err(format!("binds {bound}: {binds}; diagnostics {diags:?}")),
                    Err(other) => Err(format!("binds {bound}: {binds}; {other:?}")),
                }
            }
        };
        let line = format!("{f}: {got:?}");
        eprintln!("CASE {line}");
        let right = match (&got, want) {
            (Ok(b), Ok(w)) => *b == w,
            (Err(e), Err(())) => e.contains("PatternShadows"),
            _ => false,
        };
        if !right {
            outcomes.push(line);
        }
    }
    assert!(outcomes.is_empty(), "identifier patterns that are wrong:\n{}", outcomes.join("\n"));
    for (k, _, _) in names.items(0) {
        let Some(header) = names.item_text(k).and_then(|t| t.impl_header) else { continue };
        store.ensure(&names, &mut front.keys, Want::Impl(k)).unwrap_or_else(|e| panic!("impl `{header}`: no head: {e:?}"));
        let Some(mk) = front.keys.filed_member(checker_of(k), MemberKind::Fn, "clone") else { continue };
        let b = lower_member(&names, prepared, &mut front.keys, &mut store, mk).unwrap_or_else(|e| panic!("{header}: {e}"));
        bodies.push((format!("<{header}>::clone"), b));
    }
    assert_eq!(bodies.len(), 3, "the three derived `clone`s were taken");
    for (name, body) in &bodies {
        let view = body.lowered.view();
        let spell = Spelling::of(&view).expect("spelling");
        let scope = body.scope(&spell);
        let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: None };
        match check_body(&mut records, body, &scope) {
            Ok(done) if done.diags.is_empty() => {}
            Ok(done) => notes.push(format!("{name}: Ok with {} diagnostics", done.diags.len())),
            Err(CheckFail::Diags { diags, .. }) => notes.push(format!("{name}: diagnostics {diags:?}")),
            Err(other) => notes.push(format!("{name}: {other:?}")),
        }
    }
    assert!(notes.is_empty(), "bodies that must check clean did not:\n{}", notes.join("\n"));
}
