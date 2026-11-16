//! Overloaded dereference and `?` through front2 with the real library: each fixture is a fn whose
//! BODY is checked by the real front end against the installed toolchain, and must check clean.
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`, as tests/front2_main.rs does.
//!
//! The Reference, "Method call expressions": the candidate receiver types are got by repeatedly
//! dereferencing the receiver's type, then one unsized coercion at the end, and for each candidate
//! `T` also `&T` and `&mut T`. "Field access expressions": automatic dereferencing. "Type
//! coercions": `&T` to `&U` when `T: Deref<Target = U>`. "Operator expressions", "The dereference
//! operator": on a non-pointer type `*x` is `*core::ops::Deref::deref(&x)` (`DerefMut` in a mutable
//! place). "The question mark operator": `?` is `Try::branch` and, on a residual,
//! `FromResidual::from_residual`, which for `Result` converts the error by `From`.

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::body::lower_fn;
use blaze_ladder::front2::check::{BuildOrder, CheckFail, Records, check_body};
use blaze_ladder::front2::heads::{Store, Want};
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::refusal::SourceFile;
use blaze_lower::scoped::Prepares;
use blaze_own::Alloc;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};

const MAIN: &str = "extern crate alloc;
use alloc::sync::Arc;
use alloc::rc::Rc;
use core::ops::{Deref, DerefMut};
fn main(cx: &Cx, io: Io) -> Result<(), Error> {
    Ok(())
}
fn vec_len(v: Vec<u8>) -> usize { v.len() }
fn vec_iter(v: Vec<u8>) -> Option<u8> { v.iter().next().copied() }
fn arc_slice(a: &Arc<[u8]>) -> Option<&u8> { a.first() }
fn takes(s: &[u8]) -> usize { s.len() }
fn box_coerce(b: &Box<[u8]>) -> usize { takes(b) }
fn rc_coerce(r: &Rc<Cancel>) -> bool { let c: &Cancel = r; c.set }
struct Cancel { set: bool }
fn arc_field(c: Arc<Cancel>) -> bool { c.set }
struct Guard<'a>(&'a mut u8);
impl<'a> Deref for Guard<'a> { type Target = u8; fn deref(&self) -> &u8 { self.0 } }
impl<'a> DerefMut for Guard<'a> { fn deref_mut(&mut self) -> &mut u8 { self.0 } }
fn read_guard(g: Guard<'_>) -> u8 { *g }
fn write_guard(mut g: Guard<'_>) { *g = 1; }
struct E;
struct E2;
impl From<E> for E2 { fn from(_: E) -> E2 { E2 } }
fn try_same(r: Result<(), E>) -> Result<(), E> { r?; Ok(()) }
fn try_from(r: Result<u8, E>) -> Result<u8, E2> { let x = r?; Ok(x) }
fn try_option(o: Option<u8>) -> Option<u8> { let x = o?; Some(x) }
fn two_levels(a: &Arc<Vec<u8>>) -> usize { a.len() }
fn two_levels_slice(a: &Arc<Vec<u8>>) -> Option<&u8> { a.first() }
fn mut_through(v: &mut Vec<u8>) { v.sort(); }
fn coerce_arg_two(a: &Arc<Vec<u8>>) -> usize { takes(a) }
struct Two<A, B>(A, B);
impl<A, B> Deref for Two<A, B> { type Target = A; fn deref(&self) -> &A { &self.0 } }
fn local_two(t: Two<u8, bool>) -> u8 { *t }
struct Al;
trait Alloc2 {}
impl Alloc2 for Al {}
struct Bx<T: ?Sized, A: Alloc2 = Al>(A, Box<T>);
impl<T: ?Sized, A: Alloc2> Deref for Bx<T, A> { type Target = T; fn deref(&self) -> &T { &self.1 } }
fn local_bounded(b: Bx<u8>) -> u8 { *b }
fn need_alloc<A: core::alloc::Allocator>(_: A) {}
fn global_allocates() { need_alloc(alloc::alloc::Global) }
fn need_from<A, B: From<A>>(_: A) {}
fn from_self() { need_from::<E, E>(E) }
fn need_try<T: core::ops::Try>(_: T) {}
fn result_tries(r: Result<(), E>) { need_try(r) }
fn arc_star(a: Arc<u8>) -> u8 { *a }
fn deref_bound<T: Deref<Target = [u8]>>(_: &T) {}
fn vec_deref_bound(v: Vec<u8>) { deref_bound(&v) }
fn need_deref<T: Deref>(_: &T) {}
fn vec_derefs(v: Vec<u8>) { need_deref(&v) }
fn try_output<T: core::ops::Try<Output = ()>>(_: T) {}
fn result_output(r: Result<(), E>) { try_output(r) }
";

/// The bodies that must check clean, each with what it proves.
const CLEAN: [(&str, &str); 26] = [
    ("arc_star", "`*a` on `Arc<u8>`"),
    ("vec_deref_bound", "`Vec<u8>: Deref<Target = [u8]>` as a bound"),
    ("vec_derefs", "`Vec<u8>: Deref` as a bound"),
    ("result_output", "`Result<(), E>: Try<Output = ()>` as a bound"),
    ("global_allocates", "`Global: Allocator` by `const unsafe impl Allocator for Global`"),
    ("from_self", "`E: From<E>` by `const impl<T> From<T> for T`"),
    ("result_tries", "`Result<(), E>: Try` as a bound"),
    ("local_two", "`*t` by a local `Deref` impl with two type parameters"),
    ("local_bounded", "`*b` by a local `Deref` impl like `Box`'s: `T: ?Sized, A: Alloc2` and a default"),
    ("vec_len", "`Vec::len`: an inherent method by value"),
    ("vec_iter", "`[u8]::iter` on `Vec<u8>` through `Deref<Target = [u8]>`"),
    ("arc_slice", "`[u8]::first` on `&Arc<[u8]>`: two dereference steps"),
    ("box_coerce", "`&Box<[u8]>` coerced to `&[u8]` at an argument"),
    ("rc_coerce", "`&Rc<Cancel>` coerced to `&Cancel` at a `let` with a written type"),
    ("arc_field", "a field of `Cancel` through `Arc<Cancel>`"),
    ("read_guard", "`*g` on a type with a `Deref` impl"),
    ("write_guard", "`*g = 1`: `DerefMut` in a mutable place"),
    ("try_same", "`?` on `Result<(), E>` in a fn returning `Result<(), E>`"),
    ("try_from", "`?` converting the error by `From<E> for E2`"),
    ("try_option", "`?` on `Option` in a fn returning `Option`"),
    ("two_levels", "`Vec::len` on `&Arc<Vec<u8>>`"),
    ("two_levels_slice", "`[u8]::first` on `&Arc<Vec<u8>>`: three dereference steps"),
    ("mut_through", "`[u8]::sort` on `&mut Vec<u8>`: `DerefMut` for a `&mut self` method"),
    ("coerce_arg_two", "`&Arc<Vec<u8>>` coerced to `&[u8]`: two deref steps in a coercion"),
    ("takes", "`<[u8]>::len`"),
    ("main", "the entry"),
];

#[test]
fn deref_and_try_through_front2() {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/chks/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest("blaze-2026.11").expect("a known edition");
    let user = UserCrate { name: "chks", dir: "/chks/", root: "/chks/src/main.rs", files: &files, edition };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: MAIN }];
    let mut front = Front2::new(&names, &sources).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"));
    let root = names.root(0);
    let lib = |name: &'static str| {
        names::path(&names, root, Namespace::Values, Written { kind: PathKind::Plain, segments: &[name] }, Alloc::global())
            .ok()
            .and_then(|f| f.whole(1))
            .map(|d| d.key)
            .unwrap_or_else(|| panic!("`{name}` from the crate root"))
    };
    let prepared = Prepares::new_in(Alloc::global());
    let mut buckets = None;
    let mut failed = Vec::new();
    let mut report: Vec<String> = Vec::new();
    for (name, proves) in CLEAN {
        if let Err(e) = store.ensure(&names, &mut front.keys, Want::Fn(lib(name))) {
            report.push(format!("{name}: no head: {e:?}"));
            failed.push(name);
            continue;
        }
        let body = match lower_fn(&names, prepared, &mut front.keys, &mut store, lib(name)) {
            Ok(b) => b,
            Err(e) => {
                report.push(format!("{name}: does not lower: {e}"));
                failed.push(name);
                continue;
            }
        };
        let view = body.lowered.view();
        let spell = Spelling::of(&view).expect("spelling");
        let scope = body.scope(&spell);
        let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: None };
        match check_body(&mut records, &body, &scope) {
            Ok(_) => report.push(format!("{name}: clean")),
            Err(CheckFail::Diags { diags, .. }) => {
                report.push(format!("{name}: diagnostics {diags:?} ({proves})"));
                failed.push(name);
            }
            Err(other) => {
                report.push(format!("{name}: {other:?} ({proves})"));
                failed.push(name);
            }
        }
    }
    let mut probes = String::new();
    if let Some(b) = buckets.as_ref() {
        for lang in ["deref", "Try"] {
            let t = front.index.lang(lang).map(blaze_ladder::front2::keys::checker_of).expect("lang");
            for (tr, _, found) in b.answered() {
                if tr != t {
                    continue;
                }
                for k in found {
                    let Some(h) = store.impls.get(k) else { println!("probe {lang}: {k:?} no head"); continue };
                    let preds = store.preds.get(k).map(|p| format!("{:?}", p.clauses)).unwrap_or_default();
                    let tys = store.tys.view();
                    probes += &format!("\nprobe {lang}: g={} self={:?} args={:?} assoc={:?} preds={}", h.generics, tys.node(h.self_ty), h.trait_args, h.assoc.iter().map(|(_, t)| tys.node(*t)).collect::<Vec<_>>(), &preds[..preds.len().min(200)]);
                    let _ = &mut probes;
                }
            }
        }
    }
    // Failures first: the harness's runner shows the first lines only.
    report.sort_by_key(|l| l.ends_with(": clean"));
    for line in &report {
        println!("{line}");
    }
    let _ = probes;
    let why: Vec<String> = report.iter().filter(|l| !l.ends_with(": clean")).map(|l| l.chars().take(260).collect()).collect();
    assert!(failed.is_empty(), "bodies that must check clean did not: {failed:?}\n{}", why.join("\n"));
}
