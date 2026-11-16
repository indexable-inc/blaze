//! Bounds in front2's heads (front2/preds.rs): `Fn(A) -> B` outside `dyn`, `Name = T` bindings,
//! `?Sized` and the sized hierarchy, higher-ranked bounds. Each fixture is a fn whose BODY is
//! checked through front2 with the real core: a body that uses the bound checks clean only when
//! its head says what the language says. Needs the installed toolchain under
//! `$HOME/.blaze/v2/toolchain`, as tests/front2_main.rs does.
//!
//! The Reference, "Paths", "Paths in types": the parenthesized form `Fn(A, B) -> C` is sugar for
//! the trait with one argument, the tuple `(A, B)`, and the binding `Output = C` (`()` with no
//! `->`). "Trait and lifetime bounds": "`?` is only used to relax the implicit `Sized` trait bound
//! for type parameters or associated types"; RFC 3729 (the sized hierarchy, the pinned core's
//! `impl<T: PointeeSized> Clone for &T`): a bound by `MetaSized` or `PointeeSized` states the
//! parameter's sizedness too. "Higher-ranked trait bounds": `for<'a>` binds lifetimes, which this
//! checker erases.
//!
//! Associated type bounds (the Reference, "Trait and lifetime bounds", associated item
//! constraints; RFC 2289): `T: Tr<Assoc: Bounds>` is `T: Tr` plus `<T as Tr>::Assoc: Bounds`;
//! the pinned core writes `R: [const] Try<Output = T, Residual: [const] Residual<[R::Output; N]>>`
//! (core/src/array/mod.rs:116). `[const]` is the bound itself here: constness is not modelled by
//! this checker (RFC 3762's const traits are checked only in const contexts). `Self::X` in a
//! trait impl that does not declare `X` (the Reference, "Paths", "`Self`"): `<Self as Tr>::X` for
//! the trait or supertrait declaring it; core: `impl DoubleEndedIterator for CaseMappingIter {
//! fn rfold<B, F>(..) where F: FnMut(B, Self::Item) -> B }` (core/src/char/mod.rs:598).

#![feature(variant_count)]

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::langs;
use blaze_ladder::resolve::Resolver;
use blaze_typeck::LangItem;
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::body::lower_fn;
use blaze_ladder::front2::check::{BuildOrder, CheckFail, Records, check_body};
use blaze_ladder::front2::heads::{Store, Want};
use blaze_ladder::front2::keys::{MemberKind, checker_of};
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::front2::spell::Spelling;
use blaze_lower::scoped::Prepares;
use blaze_own::Alloc;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};

const MAIN: &str = "fn main(cx: &Cx, io: Io) -> Result<(), Error> {
    Ok(())
}
fn call_bound<F: Fn(u8) -> u16>(f: F) -> u16 { f(1) }
fn call_where<F>(mut f: F) where F: FnMut(u8) { f(1) }
fn call_apit(f: impl FnOnce(u8, bool) -> u16) -> u16 { f(1, true) }
fn call_unit<F: Fn()>(f: F) { let () = f(); }
trait Sup: Fn(u8) -> u16 {}
fn call_sup<S: Sup>(s: &S) -> u16 { s(1) }
fn next_bound<I: Iterator<Item = u8>>(mut i: I) -> Option<u8> { i.next() }
fn unsized_where<T>(t: &T) -> &T where T: ?Sized { t }
fn unsized_list<T: ?Sized>(t: &T) -> &T { t }
fn pointee<T: core::marker::PointeeSized>(t: &T) -> &T { t }
fn use_where(s: &str) -> &str { unsized_where(s) }
fn use_list(s: &str) -> &str { unsized_list(s) }
fn use_pointee(s: &str) -> &str { pointee(s) }
fn clone_ref<'a>(s: &&'a str) -> &'a str { s.clone() }
fn hr_where<F>(f: F) -> u8 where for<'a> F: Fn(&'a u8) -> u8 { f(&1) }
fn hr_bound<F: for<'a> Fn(&'a u8) -> u8>(f: F) -> u8 { f(&1) }
fn hr_ptr(f: for<'a> fn(&'a u8) -> u8) -> u8 { f(&1) }
fn display<T: core::fmt::Display>(t: T, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result { write!(f, \"{t}\") }
fn need_clone<T: Clone>(t: &T) -> T { t.clone() }
fn assoc_bound<I: Iterator<Item: Clone>>(mut i: I) { if let Some(x) = i.next() { let _y = need_clone(&x); } }
fn gat_bound<'a, P: core::str::pattern::Pattern<Searcher<'a>: Clone>>(s: &P::Searcher<'a>) -> P::Searcher<'a> { need_clone(s) }
struct Count(u8);
impl Iterator for Count { type Item = u8; fn next(&mut self) -> Option<u8> { None } }
impl DoubleEndedIterator for Count { fn next_back(&mut self) -> Option<Self::Item> { None } }
";

/// The bodies that must check clean, each with what it proves.
const CLEAN: [(&str, &str); 18] = [
    ("call_bound", "`F: Fn(u8) -> u16` on the parameter: `Fn<(u8,)>` and `Output = u16`"),
    ("call_where", "`where F: FnMut(u8)`: the output is `()`"),
    ("call_apit", "`impl FnOnce(u8, bool) -> u16` in argument position"),
    ("call_unit", "`Fn()`: no inputs, the output `()`"),
    ("call_sup", "`trait Sup: Fn(u8) -> u16`: the sugar in a supertrait"),
    ("next_bound", "`Iterator<Item = u8>`: a binding in the angle form"),
    ("unsized_where", "`where T: ?Sized`"),
    ("unsized_list", "`T: ?Sized` in the generics list"),
    ("pointee", "`T: PointeeSized`"),
    ("use_where", "`unsized_where::<str>`: `where T: ?Sized` takes `Sized` away"),
    ("use_list", "`unsized_list::<str>`"),
    ("use_pointee", "`pointee::<str>`: a `PointeeSized` bound takes `Sized` away"),
    ("clone_ref", "`&str: Clone` by core's `impl<T: PointeeSized> Clone for &T`"),
    ("hr_where", "`for<'a> F: Fn(&'a u8)`: a lifetime binder is erased"),
    ("hr_bound", "`F: for<'a> Fn(&'a u8)`"),
    ("hr_ptr", "`for<'a> fn(&'a u8) -> u8`"),
    ("assoc_bound", "`I: Iterator<Item: Clone>` (an associated type bound): `<I as Iterator>::Item: Clone`"),
    ("gat_bound", "`P: Pattern<Searcher<'a>: Clone>`: a lifetime argument of the associated type is erased"),
];
// `display` (`write!(f, \"{t}\")` with `T: Display`) is not in CLEAN: past the lang item it meets
// the checker's internal `UnknownInCleanBody` at the format arguments (the checker's lane). The
// lang item itself is proved below: every `LangItem` has an answer.

#[test]
fn bounds_through_front2() {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/bnds/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest("blaze-2026.11").expect("a known edition");
    let user = UserCrate { name: "bnds", dir: "/bnds/", root: "/bnds/src/main.rs", files: &files, edition };
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
    let mut clean = Vec::new();
    for (name, proves) in CLEAN {
        if let Err(e) = store.ensure(&names, &mut front.keys, Want::Fn(lib(name))) {
            println!("{name}: no head: {e:?}");
            failed.push(name);
            continue;
        }
        let body = match lower_fn(&names, prepared, &mut front.keys, &mut store, lib(name)) {
            Ok(b) => b,
            Err(e) => {
                println!("{name}: does not lower: {e}");
                failed.push(name);
                continue;
            }
        };
        let view = body.lowered.view();
        let spell = Spelling::of(&view).expect("spelling");
        let scope = body.scope(&spell);
        let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: None };
        match check_body(&mut records, &body, &scope) {
            // One line for every clean body at the end: a failure's line comes first.
            Ok(_) => clean.push(name),
            Err(CheckFail::Diags { diags, .. }) => {
                println!("{name}: diagnostics {diags:?} ({proves})");
                failed.push(name);
            }
            Err(other) => {
                println!("{name}: {other:?} ({proves})");
                failed.push(name);
            }
        }
    }
    // Every impl of the fixture has its head with every member fn's signature: `Self::Item` in
    // `impl DoubleEndedIterator for Count` is `<Count as Iterator>::Item` (a supertrait's).
    let resolver = Resolver { names: &names };
    let mut impls = 0;
    for (k, _, _) in names.items(0) {
        if !matches!(resolver.detail(k), Some(blaze_surface::ItemDetail::Impl)) {
            continue;
        }
        impls += 1;
        let header = names.item_text(k).and_then(|t| t.impl_header).unwrap_or("?");
        if let Err(e) = store.ensure(&names, &mut front.keys, Want::Impl(k)) {
            println!("impl `{header}`: no head: {e}");
            failed.push("an impl head");
            continue;
        }
        for m in names.members(k).unwrap_or(&[]) {
            let Some(mk) = m.name.and_then(|n| front.keys.filed_member(checker_of(k), MemberKind::Fn, n)) else { continue };
            if let Some(why) = store.fn_gaps.get(&mk) {
                println!("impl `{header}`: member `{}` has no signature: {why:?}", m.name.unwrap_or("?"));
                failed.push("an impl member's signature");
            }
        }
    }
    assert_eq!(impls, 2, "the fixture's two impls");
    println!("clean: {clean:?}");
    assert!(failed.is_empty(), "bodies that must check clean, or impl heads that must build, did not: {failed:?}");

    // Every lang item the checker asks for has an answer in the pinned library: by `#[lang]`, or
    // (the formatting traits, `Send`, `IntoFuture`) by its path in core ([`langs::find`]).
    assert_eq!(langs::ALL.len(), core::mem::variant_count::<LangItem>(), "langs::ALL lists every LangItem");
    assert!(langs::ALL.windows(2).all(|w| w[0] < w[1]), "langs::ALL is in declaration order, each once");
    let mut unanswered = Vec::new();
    for l in langs::ALL {
        match langs::find(&front.index, resolver, l) {
            Ok(Some(_)) => {}
            other => unanswered.push((l, langs::mark(l), other)),
        }
    }
    println!("lang items with no answer: {unanswered:?}");
    assert!(unanswered.is_empty(), "every lang item has an answer in the program's crates: {unanswered:?}");
}
