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
//!
//! Trait aliases (the unstable book, `trait_alias`: "Trait aliases ... `trait Foo = Bar + Baz;`
//! ... used as a bound is equivalent to" its right-hand side; the pinned core declares `pub trait
//! Thin = Pointee<Metadata = ()> + PointeeSized;` in core/src/ptr/metadata.rs and bounds by it,
//! `pub unsafe fn catch_unwind<Data: ptr::Thin>`): a bound, a where clause, `impl Alias`, a
//! supertrait and `dyn Alias` stand for its clauses with `Self` and its parameters substituted,
//! through an alias of an alias; aliases naming each other are a typed cycle.
//!
//! Opaque types (the Reference, "Impl trait", "Abstract return types": "the caller can only use
//! the functionality specified by the trait bounds"): each `impl Trait` in a return type is its
//! own opaque type, keyed by its fn and its index (preorder over the written return type), its
//! bounds its predicates with the opaque as `Self`, its hidden type told to no caller. It
//! captures every in-scope generic parameter ("Capturing", 2024 edition). A trait's method
//! (`fn make(&self) -> impl Iterator`, return-position `impl Trait` in traits) gets its opaque
//! the same way, keyed by the trait's fn; an impl's method has its own key. "Async functions":
//! "async fn example(x: &str) -> usize" is "roughly equivalent to" "fn example<'a>(x: &'a str)
//! -> impl Future<Output = usize> + 'a"; its opaque is index 0.

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
fn meta_need<T: core::marker::MetaSized>(p: *const T) -> *const T { p }
fn relaxed_meta<T: ?Sized>(p: *const T) -> *const T { meta_need(p) }
fn where_meta<T>(p: *const T) -> *const T where T: ?Sized { meta_need(p) }
fn pointee_meta<T: core::marker::PointeeSized>(p: *const T) -> *const T { meta_need(p) }
fn clone_ref<'a>(s: &&'a str) -> &'a str { s.clone() }
fn hr_where<F>(f: F) -> u8 where for<'a> F: Fn(&'a u8) -> u8 { f(&1) }
fn hr_bound<F: for<'a> Fn(&'a u8) -> u8>(f: F) -> u8 { f(&1) }
fn hr_ptr(f: for<'a> fn(&'a u8) -> u8) -> u8 { f(&1) }
fn display<T: core::fmt::Display>(t: T, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result { write!(f, \"{t}\") }
fn need_clone<T: Clone>(t: &T) -> T { t.clone() }
fn assoc_bound<I: Iterator<Item: Clone>>(mut i: I) { if let Some(x) = i.next() { let _y = need_clone(&x); } }
fn gat_bound<'a, P: core::str::pattern::Pattern<Searcher<'a>: Clone>>(s: &P::Searcher<'a>) -> P::Searcher<'a> { need_clone(s) }
trait Fam { type Out<T>; }
struct Pairs;
impl Fam for Pairs { type Out<T> = (T, u8); }
fn gat_bind<F: Fam<Out<u16> = (u16, u8)>>(x: <F as Fam>::Out<u16>) -> (u16, u8) { x }
fn gat_impl(x: <Pairs as Fam>::Out<u16>) -> (u16, u8) { x }
trait Lend { type Iter<'a> where Self: 'a; }
fn gat_where<'a, L: Lend + 'a>(x: <L as Lend>::Iter<'a>) -> <L as Lend>::Iter<'a> { x }
trait Sz { type Arr<T>; }
struct Twin;
impl Sz for Twin { type Arr<T> = (T, T); }
struct NoCl;
struct Lone;
impl Sz for Lone { type Arr<T> = NoCl; }
fn gat_args_bound<U: Sz<Arr<u8>: Clone>>(a: &U::Arr<u8>) -> U::Arr<u8> { need_clone(a) }
fn gat_args_where<U: Sz>(a: &U::Arr<u8>) -> U::Arr<u8> where U::Arr<u8>: Clone { need_clone(a) }
fn gat_args_qwhere<U: Sz>(a: &<U as Sz>::Arr<u8>) -> <U as Sz>::Arr<u8> where <U as Sz>::Arr<u8>: Clone { need_clone(a) }
fn use_gat_args() -> (u8, u8) { gat_args_bound::<Twin>(&(1, 2)) }
fn use_gat_args_bad() { let _n = gat_args_bound::<Lone>(&NoCl); }
trait LendT { type It<'a, T> where Self: 'a; }
fn gat_life_bound<'a, L: LendT<It<'a, u8>: Clone> + 'a>(x: &L::It<'a, u8>) -> L::It<'a, u8> { need_clone(x) }
trait Cap { type Buf<const N: usize>; }
struct Bytes3;
impl Cap for Bytes3 { type Buf<const N: usize> = u8; }
struct NoBuf;
impl Cap for NoBuf { type Buf<const N: usize> = NoCl; }
fn gat_const_lit<C: Cap<Buf<3>: Clone>>() {}
fn use_gat_const_lit() { gat_const_lit::<Bytes3>() }
fn use_gat_const_bad() { gat_const_lit::<NoBuf>() }
struct Count(u8);
impl Iterator for Count { type Item = u8; fn next(&mut self) -> Option<u8> { None } }
impl DoubleEndedIterator for Count { fn next_back(&mut self) -> Option<Self::Item> { None } }
trait Conv<T> = Into<T> + Copy;
trait Conv16 = Conv<u16>;
trait Bytes = Iterator<Item = u8>;
trait Sub: Bytes {}
trait CycA = CycB;
trait CycB = CycA;
fn thin<T: core::ptr::Thin>(p: *const T) -> () { core::ptr::metadata(p) }
fn thin_where<P>(p: *const P) -> () where P: core::ptr::Thin { core::ptr::metadata(p) }
fn conv<A: Conv<u16>>(a: A) -> (u16, u16) { (a.into(), a.into()) }
fn conv16<A: Conv16>(a: A) -> u16 { a.into() }
fn bytes<I: Bytes>(mut i: I) -> Option<u8> { i.next() }
fn bytes_item<I: Bytes>(mut i: I) -> Option<I::Item> { i.next() }
fn bytes_apit(mut i: impl Bytes) -> Option<u8> { i.next() }
fn bytes_dyn(i: &mut dyn Bytes) -> Option<u8> { i.next() }
fn sub_next<S: Sub>(mut s: S) -> Option<u8> { s.next() }
fn cyc<T: CycA>(t: T) -> T { t }
fn use_conv() -> (u16, u16) { conv(3u8) }
fn evens() -> impl Iterator<Item = u8> + Send { Count(0) }
fn use_evens() -> Option<u8> { let mut i = evens(); i.next() }
fn use_evens_bound() -> Option<u8> { next_bound(evens()) }
fn wrap<T: Clone>(t: T) -> impl Clone { t }
fn use_wrap() { let w = wrap(1u8); let _c = need_clone(&w); }
fn opt_iter() -> Option<impl Iterator<Item = u8>> { None::<Count> }
fn use_opt() -> bool { opt_iter().is_some() }
fn use_opt_next() -> Option<u8> { match opt_iter() { Some(mut i) => i.next(), None => None } }
fn nested() -> impl Iterator<Item = impl Clone> { Count(0) }
fn use_nested() { let mut n = nested(); if let Some(x) = n.next() { let _c = need_clone(&x); } }
trait Make { fn make(&self) -> impl Iterator<Item = u8>; }
impl Make for u8 { fn make(&self) -> impl Iterator<Item = u8> { Count(*self) } }
fn use_make<M: Make>(m: &M) -> Option<u8> { let mut i = m.make(); i.next() }
async fn af(x: u8) -> u16 { x as u16 }
fn need_fut<F: core::future::Future<Output = u16>>(f: F) -> F { f }
fn use_af() { let _f = need_fut(af(1)); }
async fn g() -> u16 { af(1).await }
async fn h() -> u8 { \"a\" }
fn two_same(b: bool) -> impl Clone { if b { return 1u8; } 2u8 }
fn two_types(b: bool) -> impl Clone { if b { return 1u8; } \"a\" }
fn lacks() -> impl Iterator<Item = u8> { 1u8 }
fn use_wrap2() { let a = wrap(1u8); let b = wrap(true); let _c = need_clone(&a); let _d = need_clone(&b); }
fn into_head(xs: impl IntoIterator<Item = impl Into<u64>>) -> u64 { 0 }
fn use_into_head() -> u64 { into_head([1u8, 2]) }
fn use_into_bad() -> u64 { into_head([1i64, 2]) }
fn first_into(xs: impl IntoIterator<Item = impl Into<u64>>) -> u64 { match xs.into_iter().next() { Some(x) => x.into(), None => 0 } }
";

/// The bodies that must check clean, each with what it proves.
const CLEAN: [(&str, &str); 60] = [
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
    // The sized hierarchy (RFC 3729; the pinned core's marker.rs: `pub trait Sized: MetaSized`,
    // `pub trait MetaSized: PointeeSized`, MetaSized's doc: "Types with a size that can be
    // determined from pointer metadata."): `?Sized` relaxes `Sized` to `MetaSized`, not to
    // nothing; only `PointeeSized` states no size. alloc's `data_offset<T: ?Sized>` calls
    // `Alignment::of_val_raw<T: MetaSized>` with its own `T` (alloc/src/rcs/arc.rs). A fix that
    // drops every sizedness clause fails these; one that gives `PointeeSized` a `MetaSized`
    // clause fails `pointee_meta` in REFUSED.
    ("meta_need", "`T: MetaSized` written: states sizedness, no implied `Sized`"),
    ("relaxed_meta", "`T: ?Sized` keeps `T: MetaSized`: `data_offset`'s call of `Alignment::of_val_raw`"),
    ("where_meta", "`where T: ?Sized` keeps `T: MetaSized` too"),
    ("clone_ref", "`&str: Clone` by core's `impl<T: PointeeSized> Clone for &T`"),
    ("hr_where", "`for<'a> F: Fn(&'a u8)`: a lifetime binder is erased"),
    ("hr_bound", "`F: for<'a> Fn(&'a u8)`"),
    ("hr_ptr", "`for<'a> fn(&'a u8) -> u8`"),
    ("assoc_bound", "`I: Iterator<Item: Clone>` (an associated type bound): `<I as Iterator>::Item: Clone`"),
    ("gat_bound", "`P: Pattern<Searcher<'a>: Clone>`: a lifetime argument of the associated type is erased"),
    // Generic associated types (the Reference, "Associated Items", "Associated Types":
    // "Associated types may include generic parameters and where clauses; these are often
    // referred to as generic associated types, or GATs"; "Paths", "Qualified paths": "Fully
    // qualified paths allow for disambiguating the path for trait implementations and for
    // specifying canonical paths"). front2/gat.rs; the checker's side is blaze-typeck
    // suite/gat.rs. Each is clean only when the projection's list carries the associated
    // type's own argument after the trait's: without it `gat_bind` has no head (the refusal
    // "a binding of a generic associated type with type or const arguments") and
    // `gat_impl`'s `T` names nothing in the impl's `type Out<T> = (T, u8);`.
    ("gat_bind", "`F: Fam<Out<u16> = (u16, u8)>`: the clause `<F as Fam>::Out<u16> == (u16, u8)`, so `x` is the pair"),
    ("gat_impl", "`<Pairs as Fam>::Out<u16>` through `impl Fam for Pairs { type Out<T> = (T, u8); }` is `(u16, u8)`"),
    ("gat_where", "`type Iter<'a> where Self: 'a;`: the outlives clause is read and kept (not checked); `Iter<'a>` has no own argument"),
    // Associated type bounds on a GAT with arguments (the Reference, "Trait and lifetime
    // bounds", associated item constraints: `T: Tr<Assoc: B>` is `T: Tr` and `<T as
    // Tr>::Assoc: B`; ecdsa 0.17's `impl<C> From<Signature<C>> for Box<[u8]>` was out of
    // tempo's census 5 on this form). front2/gat_bounds.rs. Without it the three `_bound`/`_lit`
    // heads are refused "an associated type bound with type or const arguments".
    ("gat_args_bound", "`U: Sz<Arr<u8>: Clone>`: the clause `<U as Sz>::Arr<u8>: Clone`, so `need_clone(a)` holds"),
    ("gat_args_where", "`where U::Arr<u8>: Clone`: the where-clause form of the same clause"),
    ("gat_args_qwhere", "`where <U as Sz>::Arr<u8>: Clone`: the qualified where-clause form"),
    ("use_gat_args", "`gat_args_bound::<Twin>`: `<Twin as Sz>::Arr<u8>` is `(u8, u8)`, which is `Clone`"),
    ("gat_life_bound", "`L: LendT<It<'a, u8>: Clone>`: the lifetime argument erased, `u8` the own argument"),
    ("gat_const_lit", "`C: Cap<Buf<3>: Clone>`: `3` made as a `usize` constant, the GAT's declared const parameter type"),
    ("use_gat_const_lit", "`gat_const_lit::<Bytes3>`: `<Bytes3 as Cap>::Buf<3>` is `u8`, which is `Clone`"),
    ("thin", "`T: Thin` (core: `pub trait Thin = Pointee<Metadata = ()> + PointeeSized;`): the trait clause and the projection, so `metadata(p)` is `()`"),
    ("thin_where", "`where P: Thin`"),
    ("conv", "`trait Conv<T> = Into<T> + Copy`: a generic alias, its parameter substituted"),
    ("conv16", "`trait Conv16 = Conv<u16>`: an alias of an alias"),
    ("bytes", "`trait Bytes = Iterator<Item = u8>`: an alias with an associated type binding"),
    ("bytes_item", "`I::Item` through `I: Bytes`: looked up through the traits the alias stands for"),
    ("bytes_apit", "`impl Bytes` in argument position"),
    ("bytes_dyn", "`dyn Bytes`: the object of `Iterator<Item = u8>`"),
    ("sub_next", "`trait Sub: Bytes`: an alias as a supertrait"),
    ("use_conv", "`conv(3u8)`: the alias's clauses are what the caller proves"),
    ("use_evens", "`-> impl Iterator<Item = u8> + Send`: the caller calls `next` through the opaque's bound"),
    ("use_evens_bound", "the opaque passed where `I: Iterator<Item = u8>` is expected"),
    ("use_wrap", "a generic fn's opaque `impl Clone`, its bounds instantiated with `T = u8`"),
    ("use_opt", "`-> Option<impl Iterator<Item = u8>>`: an opaque nested in a type"),
    ("use_nested", "`-> impl Iterator<Item = impl Clone>`: occurrence 1 is the `Item`, `Clone`"),
    ("use_make", "a trait method's `-> impl Iterator<Item = u8>` through `M: Make`"),
    ("use_af", "`async fn af(x: u8) -> u16` is `fn af(x: u8) -> impl Future<Output = u16>`"),
    // The DEFINING bodies. The Reference, "Impl trait", [type.impl-trait.return.constraint-body]:
    // "Each possible return value from the function must resolve to the same concrete type";
    // "the function chooses the return type, but only promises that it will implement Trait".
    // "Functions", [items.fn.async.desugar-brief]: "An async function is roughly equivalent to a
    // function that returns impl Future and with an async move block as its body";
    // [items.fn.async.param-capture]: "The async move block in the body captures all function
    // parameters, including those that are unused or bound to a _ pattern. This ensures that
    // function parameters are dropped in the same order as they would be if the function were
    // not async, except that the drop occurs when the returned future has been fully awaited."
    ("evens", "`Count(0)` is the hidden type of `impl Iterator<Item = u8> + Send`"),
    ("wrap", "a generic fn's hidden type is its parameter `T`, which has `Clone` by its bound"),
    ("opt_iter", "`None::<Count>`: the opaque nested in `Option` is `Count`"),
    ("nested", "`impl Iterator<Item = impl Clone>`: two opaques, `Count` and its `Item` `u8`"),
    ("af", "an async fn's defining body is checked against its written `u16`"),
    ("g", "\"Await expressions\": `af(1).await` is `<impl Future<Output = u16> as IntoFuture>::Output`"),
    ("two_same", "two return points of one type: one hidden type `u8`"),
    ("use_wrap2", "a generic fn's opaque at two instances (`u8`, `bool`): two different types, each `Clone`"),
    ("use_opt_next", "`Some(mut i) => i.next()`: a method through the bound of an opaque bound by a pattern inside `Option`"),
    // An `impl Trait` in an associated type binding of a parameter's `impl Trait` (the
    // Reference, "Impl trait", "Limitations": "`impl Trait` can only appear as a parameter or
    // return type of a non-extern function"; "Anonymous type parameters": it "is syntactic
    // sugar for a generic type parameter like `<T: Trait>`"): two anonymous parameters, the
    // `IntoIterator` one first, the `Into<u64>` one second, `<P0 as IntoIterator>::Item == P1`.
    // clap's `ArgGroup::args(self, ns: impl IntoIterator<Item = impl Into<Id>>)`. Without
    // front2/heads.rs `anon_params` walking the binding, each is "no head: .. an `impl Trait`
    // type outside a fn's parameters and return type".
    ("into_head", "`fn into_head(xs: impl IntoIterator<Item = impl Into<u64>>)`: the head is built"),
    ("use_into_head", "`into_head([1u8, 2])`: `P0 = [u8; 2]`, its `Item` `u8` is `P1`, `u8: Into<u64>`"),
    ("first_into", "the inner anonymous parameter's `Into<u64>` bound gives `x.into()` its `u64`"),
];

/// The bodies that must be refused, each with the diagnostic it must give.
const REFUSED: [(&str, &str, &str); 7] = [
    // The controls of gat_bounds.rs: the bound is made AND required, so a type whose
    // projection is not `Clone` is the language's E0277 at the caller, never a head refusal.
    ("use_gat_args_bad", "Unsatisfied", "`gat_args_bound::<Lone>`: `<Lone as Sz>::Arr<u8>` is `NoCl`, not `Clone`: E0277"),
    ("use_gat_const_bad", "Unsatisfied", "`gat_const_lit::<NoBuf>`: `<NoBuf as Cap>::Buf<3>` is `NoCl`, not `Clone`: E0277"),
    ("pointee_meta", "Unsatisfied", "`PointeeSized` states no size: `T: MetaSized` is E0277"),
    ("h", "Mismatch", "an async fn's body `\"a\"` against its written `u8`: E0308"),
    ("two_types", "Mismatch", "two return points of two types (`u8`, `&str`): \"one type\": E0308"),
    ("lacks", "Unsatisfied", "the hidden type `u8` is not `Iterator`: E0277 naming the bound"),
    // The inner anonymous parameter's bound is the caller's to prove: `[i64; 2]`'s `Item` is
    // `i64`, and core has no `From<i64> for u64`, so `i64: Into<u64>` is E0277. A fix that files
    // the inner `impl` with no bounds checks this clean.
    ("use_into_bad", "Unsatisfied", "`into_head([1i64, 2])`: `i64: Into<u64>` does not hold: E0277"),
];
// `use_opt_next` was out of CLEAN at 11:10 with the checker's `DiagWithoutPlace { code:
// Unsatisfied }`; reduced in blaze-typeck suite/opaque.rs (`a_method_on_an_opaque_bound_by_a_pattern`
// passes in the checker alone), it is back in CLEAN to be measured through the front end.
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
    let user = UserCrate { name: "bnds", dir: "/bnds/", root: "/bnds/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: MAIN }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
        let mut front = Front2::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global());
    let root = names.root(0);
    let lib = |name: &'static str| {
        names::path(&names, root, Namespace::Values, Written { kind: PathKind::Plain, segments: &[name] }, Alloc::global())
            .ok()
            .and_then(|f| f.whole(1))
            .map(|d| d.key)
            .unwrap_or_else(|| panic!("`{name}` from the crate root"))
    };
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut buckets = None;
    let mut failed = Vec::new();
    let mut clean = Vec::new();
        let mut why: Vec<String> = Vec::new();
    for (name, proves) in CLEAN {
        if let Err(e) = store.ensure(&names, &mut front.keys, Want::Fn(lib(name))) {
            why.push(format!("{name}: no head: {e:?}"));
            failed.push(name);
            continue;
        }
        let body = match lower_fn(&names, prepared, &mut front.keys, &mut store, lib(name)) {
            Ok(b) => b,
            Err(e) => {
                why.push(format!("{name}: does not lower: {e}"));
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
                why.push(format!("{name}: diagnostics {diags:?} ({proves})"));
                failed.push(name);
            }
            Err(other) => {
                why.push(format!("{name}: {other:?}"));
                failed.push(name);
            }
        }
    }
    for (name, code, proves) in REFUSED {
        let body = match store.ensure(&names, &mut front.keys, Want::Fn(lib(name))).map_err(|e| format!("{e:?}")).and_then(|_| {
            lower_fn(&names, prepared, &mut front.keys, &mut store, lib(name)).map_err(|e| format!("{e}"))
        }) {
            Ok(b) => b,
            Err(e) => {
                println!("{name}: no body to refuse: {e}");
                failed.push(name);
                continue;
            }
        };
        let view = body.lowered.view();
        let spell = Spelling::of(&view).expect("spelling");
        let scope = body.scope(&spell);
        let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: None };
        match check_body(&mut records, &body, &scope) {
            Err(CheckFail::Diags { diags, .. }) if format!("{diags:?}").contains(code) => println!("{name}: refused: {diags:?}"),
            Ok(_) => {
                println!("{name}: checked clean, not refused with {code} ({proves})");
                failed.push(name);
            }
            Err(other) => {
                println!("{name}: not refused with {code} ({proves}): {other:?}");
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
    assert_eq!(impls, 8, "the fixture's eight impls (`impl Make for u8`: its method's own opaque; `impl Fam for Pairs`: its `type Out<T>`; `Sz` for `Twin` and `Lone`, `Cap` for `Bytes3` and `NoBuf`: GAT values with a type and a const parameter)");
    // Two trait aliases that name each other stand for nothing: a typed cycle naming both.
    match store.ensure(&names, &mut front.keys, Want::Fn(lib("cyc"))) {
        Err(e @ blaze_ladder::front2::heads::HeadError::Cycle(_)) => println!("cyc: {e}"),
        other => {
            println!("cyc: not the cycle error: {other:?}");
            failed.push("cyc");
        }
    }
    println!("clean: {clean:?}");
    assert!(failed.is_empty(), "bodies that must check clean, or impl heads that must build, did not: {failed:?}\n{}", why.join("\n"));

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
