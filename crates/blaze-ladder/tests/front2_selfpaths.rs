//! Paths through `Self`, a type alias and a qualified projection, on front2 with the real core:
//! tempo's three classes of the checker's own (tempo-front-4: "916 diag E0034", "611 diag
//! E0599", "151 diag E0277"), all in code alloy's `sol!` writes for an enum of calls (`enum
//! CreateXCalls { .. }` with an inherent `impl CreateXCalls { pub const SELECTORS: .. }` and
//! `impl alloy_sol_types::SolInterface for CreateXCalls { .. }`), reduced. tempo compiles under
//! the pinned compiler, so each body here must check clean.
//!
//! The Reference, "Paths", "Paths in expressions": `Self` in an implementation names the
//! implementing type; "Enumerations": an enum's variants are reached by a path through the
//! enum; "Type aliases": an alias of an enum reaches its variants the same way. "Method call
//! expressions" states the lookup order: "Then, for each candidate type `T`, search for a
//! visible method with a receiver of that type in the following places: 1. `T`'s inherent
//! methods (methods implemented directly on `T`). 2. Any of the methods provided by a visible
//! trait implemented by `T`." An inherent associated item shadows a trait's of the same name.
//! "Path patterns": "Path patterns are patterns that refer either to constant values or to
//! structs or enum variants that have no fields"; "Tuple struct patterns" name a tuple variant.
//! "Associated Items", "Associated types": an implementation's `type R = ..` is the value of
//! `<T as Trait>::R` for that type.
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
struct T;
impl T {
    const K: u8 = 1;
    fn a1() -> u8 { Self::K }
}
trait Tr {
    const K: u8;
}
struct T2;
impl Tr for T2 {
    const K: u8 = 2;
}
impl T2 {
    const K: u8 = 1;
    fn a2() -> u8 { Self::K }
}
struct T3;
impl T3 {
    const K: u8 = 1;
    fn a3(x: u8) -> u8 { match x { Self::K => 1, _ => 0 } }
}
enum E {
    A(u8),
    B,
}
impl E {
    fn b1(&self) -> u8 { match self { Self::A(_) => 1, Self::B => 2 } }
    fn b2() -> E { let _x = Self::B; Self::A(3) }
}
trait Tb {
    fn g() -> u8;
    fn h() -> u8 { 0 }
    fn b3(&self) -> u8;
    fn b4() -> u8;
}
impl Tb for E {
    fn g() -> u8 { 1 }
    fn b3(&self) -> u8 { let _y = Self::A(4); match self { Self::A(_) => Self::g(), Self::B => 2 } }
    fn b4() -> u8 { Self::g() + Self::h() }
}
type X = E;
fn b5() -> E { X::A(1) }
fn b5p(e: &E) -> u8 { match e { X::A(v) => *v, X::B => 0 } }
trait St {
    type R;
}
struct K;
impl St for K {
    type R = K;
}
fn c1(k: <K as St>::R) -> K { k }
macro_rules! st {
    ($t:ident) => {
        struct $t;
        impl St for $t {
            type R = $t;
        }
    };
}
st!(K2);
fn c2(k: <K2 as St>::R) -> K2 { k }
struct It;
impl Iterator for It {
    type Item = u8;
    fn next(&mut self) -> Option<u8> { None }
}
fn c3(x: <It as Iterator>::Item) -> u8 { x }
fn needr<V: St<R = V>>(v: V) -> V { v }
fn c4() -> K { needr(K) }
fn needi<I: Iterator<Item = u8>>(i: I) -> I { i }
fn c5() -> It { needi(It) }
struct K3;
impl St for K3 {
    type R = Self;
}
fn c6() -> K3 { needr(K3) }
trait Sv {
    type R;
    type Tok<'a>;
}
struct K4;
impl Sv for K4 {
    type R = Self;
    type Tok<'a> = u8;
}
fn needs<V: Sv<R = V>>(v: V) -> V { v }
fn c7() -> K4 { needs(K4) }
fn c8(x: <K4 as Sv>::R) -> K4 { x }
impl Sv for u8 {
    type R = u8;
    type Tok<'a> = &'a u8;
}
struct K5;
impl Sv for K5 {
    type R = Self;
    type Tok<'a> = <u8 as Sv>::Tok<'a>;
}
fn c9() -> K5 { needs(K5) }
struct NotSelf;
impl NotSelf { fn d1() -> Self { Self } }
impl From<()> for NotSelf {
    fn from(_tuple: ()) -> Self { Self }
}
struct Pair(u8, u8);
impl Pair {
    fn d2() -> Self { Self(1, 2) }
    fn d3() -> fn(u8, u8) -> Pair { Self }
}
struct Wrap<V>(V);
impl<V> Wrap<V> { fn d4(v: V) -> Self { Self(v) } }
";

/// The free bodies that must check clean, each with what it proves.
const FREE: [(&str, &str); 11] = [
    ("c9", "(C) alloy's exact form: `type Token<'a> = <U as SolType>::Token<'a>;` beside `type RustType = Self;`"),
    ("c7", "(C) `type R = Self;` beside a generic associated type (alloy's `type Token<'a>`)"),
    ("c8", "(C) `<K4 as Sv>::R` beside a generic associated type, as a parameter's type"),
    ("b5", "(B5) `X::A(1)`: a variant through a type alias, as an expression"),
    ("b5p", "(B5) `X::A(v)`, `X::B`: variants through a type alias, as patterns"),
    ("c1", "(C1) `<K as St>::R` fixed by an impl in the crate"),
    ("c2", "(C2) the same where a `macro_rules!` wrote the impl (as `sol!` does)"),
    ("c3", "(C2) `<It as Iterator>::Item`: another crate's trait, a local impl"),
    ("c4", "(C) `<K as St>::R == K` asked by a callee's bound"),
    ("c5", "(C) `<It as Iterator>::Item == u8` asked by a callee's bound"),
    ("c6", "(C) `type R = Self;` (alloy's `type RustType = Self;`) asked by a bound"),
];

/// The impl members that must check clean.
const MEMBERS: [(&str, &str); 12] = [
    // tempo-front-7 "347 diag E0423": `Self` as a value (the Reference, "Paths", "`Self`":
    // "When implementing a tuple or unit struct, it also refers to the constructor in the value
    // namespace."); alloy's `sol!` writes `fn from(tuple: UnderlyingRustTuple<'_>) -> Self { Self }`.
    ("d1", "(D1) `Self` as a unit struct's value"),
    ("from", "(D1) alloy's form: `Self` as the unit value in `impl From<_> for NotSelf`"),
    ("d2", "(D2) `Self(1, 2)`: a tuple struct's ctor called"),
    ("d3", "(D2) bare `Self`: the ctor fn, coerced to `fn(u8, u8) -> Pair`"),
    ("d4", "(D3) `Self(v)` in `impl<V> Wrap<V>`: the ctor at the impl's arguments"),
    ("a1", "(A1) `Self::K`: an inherent constant"),
    ("a2", "(A2) `Self::K`: the inherent constant shadows `Tr::K`, in scope and implemented"),
    ("a3", "(A3) `Self::K` as a path pattern"),
    ("b1", "(B1) `Self::A(_)`, `Self::B` as patterns in an inherent impl"),
    ("b2", "(B2) `Self::B`, `Self::A(3)` as expressions"),
    ("b3", "(B3) variants through `Self` in an impl of a trait, and `Self::g()`"),
    ("b4", "(B4) `Self::g()` (the impl's) and `Self::h()` (the trait's provided method)"),
];

#[test]
fn self_paths_through_front2() {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/slf/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest("blaze-2026.11").expect("a known edition");
    let user = UserCrate { name: "slf", dir: "/slf/", root: "/slf/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: MAIN }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
    let mut front = Front2::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global());
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut buckets = None;
    let mut failed: Vec<String> = Vec::new();
    let mut clean: Vec<String> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    let mut bodies = Vec::new();
    for (name, proves) in FREE {
        let key = names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: &[name] }, Alloc::global())
            .ok()
            .and_then(|f| f.whole(1))
            .map(|d| d.key)
            .unwrap_or_else(|| panic!("`{name}` from the crate's root"));
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
    let mut members = 0;
    for (k, _, _) in names.items(0) {
        let Some(header) = names.item_text(k).and_then(|t| t.impl_header) else { continue };
        if let Err(e) = store.ensure(&names, &mut front.keys, Want::Impl(k)) {
            notes.push(format!("impl `{header}`: no head: {e:?}"));
            failed.push(header.to_string());
            continue;
        }
        for (member, proves) in MEMBERS {
            let Some(mk) = front.keys.filed_member(checker_of(k), MemberKind::Fn, member) else { continue };
            members += 1;
            match lower_member(&names, prepared, &mut front.keys, &mut store, mk) {
                Ok(b) => bodies.push((format!("<{header}>::{member}"), b, proves)),
                Err(e) => {
                    notes.push(format!("{header}::{member}: does not lower: {e}"));
                    failed.push(member.to_string());
                }
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
    assert_eq!(members, MEMBERS.len(), "every impl member was found\n{}", notes.join("\n"));
    assert!(failed.is_empty(), "bodies that must check clean did not: {failed:?}\n{}", notes.join("\n"));
}
