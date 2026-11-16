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
    let user = UserCrate { name: "cln", dir: "/cln/", root: "/cln/src/main.rs", files: &files, edition };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: MAIN }];
    let mut front = Front2::new(&names, &sources).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"));
    let find = |krate: usize, segments: &[&str]| {
        names::path(&names, names.root(krate), Namespace::Values, Written { kind: PathKind::Plain, segments }, Alloc::global())
            .ok()
            .and_then(|f| f.whole(1))
            .map(|d| d.key)
            .unwrap_or_else(|| panic!("`{segments:?}` from crate {krate}'s root"))
    };
    let prepared = Prepares::new_in(Alloc::global());
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
