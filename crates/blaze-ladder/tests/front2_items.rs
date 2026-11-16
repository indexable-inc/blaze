//! Item heads through front2 (front2/heads.rs, types.rs's argument and length lowering,
//! world_items.rs): each fixture is a small crate whose fns are lowered and checked one by one
//! over one store; every fn of a fixture must check clean, or the test prints each fn's outcome.
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`, as tests/front2_main.rs does.

#![feature(allocator_api)]

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::body::lower_fn;
use blaze_ladder::front2::check::{BuildOrder, Records, check_body};
use blaze_ladder::front2::consts::Consts;
use blaze_ladder::front2::heads::Store;
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::refusal::SourceFile;
use blaze_lower::scoped::Prepares;
use blaze_own::Alloc;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};

const EDITION: &str = "blaze-2026.11";

/// Each fn of `fns` in the crate `text`, lowered and checked in order over one store: its
/// name and its outcome ("clean", the diagnostics, or the failure as printed).
fn check_fns(text: &str, fns: &[&str]) -> Vec<(String, String)> {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/hello-world/src/main.rs".to_string(), text.to_string())];
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
    let user = UserCrate { name: "hello_world", dir: "/hello-world/", root: "/hello-world/src/main.rs", files: &files, edition };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text }];
    let mut front = Front2::new(&names, &sources).unwrap_or_else(|e| panic!("front2: {e}"));
    let prepared = Prepares::new_in(Alloc::global());
    let mut store = Store::new(front.index.lang("sized"));
    let (mut buckets, mut consts) = (None, Consts::default());
    let mut got = Vec::new();
    for &n in fns {
        let found = names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: &[n] }, Alloc::global())
            .unwrap_or_else(|e| panic!("`{n}`: {e}"));
        let k = found.whole(1).unwrap_or_else(|| panic!("`{n}` not named")).key;
        let outcome = match lower_fn(&names, prepared, &mut front.keys, &mut store, k) {
            Err(e) => format!("not lowered: {e}"),
            Ok(body) => {
                let view = body.lowered.view();
                let spell = Spelling::of(&view).unwrap_or_else(|e| panic!("spelling `{n}`: {e:?}"));
                let scope = body.scope(&spell);
                let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: Some((&mut consts, prepared)) };
                match check_body(&mut records, &body, &scope) {
                    Ok(done) if done.diags.is_empty() => "clean".to_string(),
                    Ok(done) => {
                        let d: Vec<String> = done.diags.iter().map(|d| format!("{} at {:?}: `{}`", d.code.code(), d.at, text.get(d.at.start as usize..d.at.end as usize).unwrap_or("?"))).collect();
                        format!("diags {d:?}")
                    }
                    Err(e) => format!("not checked: {e}"),
                }
            }
        };
        got.push((n.to_string(), outcome));
    }
    got
}

fn all_clean(what: &str, got: &[(String, String)]) {
    let lines: Vec<String> = got.iter().map(|(n, o)| format!("{what}: {n}: {o}")).collect();
    for l in &lines {
        println!("{l}");
    }
    assert!(got.iter().all(|(_, o)| o == "clean"), "{}", lines.join("\n"));
}

/// Const generics in every head (the Reference, "Generic parameters", "Const generics"): a
/// struct's, a trait's, an impl's, an alias's and a fn's const parameter, with a default; a
/// const argument that is a literal (`Buf<3>`, `Neg<-1>`, `Flag<true>`), a block (`{ 2 }`), a
/// constant item (`CAP`), a const parameter (`Buf<M>`: "an ambiguous generic argument is
/// resolved as a const if it does not resolve to a type"); array lengths `[u8; N]`,
/// `[u8; CAP]`. Before the fix every one was refused "a const generic parameter".
#[test]
fn const_generics_in_every_head() {
    const TEXT: &str = "struct Buf<const N: usize> { a: [u8; N] }
impl<const N: usize> Buf<N> {
    fn first(&self) -> u8 { self.a[0] }
}
const CAP: usize = 4;
struct Flag<const B: bool>;
struct Neg<const I: i32 = 0>;
trait Wide<const K: usize> { fn wide(&self) -> u8; }
impl Wide<3> for Buf<3> { fn wide(&self) -> u8 { self.a[2] } }
type Four = Buf<4>;
fn takes(b: Buf<3>) -> [u8; 3] { b.a }
fn cap(b: Buf<CAP>) -> [u8; CAP] { b.a }
fn generic<const M: usize>(b: Buf<M>) -> [u8; M] { b.a }
fn block(b: Buf<{ 2 }>) -> [u8; 2] { b.a }
fn neg(_n: Neg<-1>, _f: Flag<true>, _d: Neg) {}
fn alias(b: Four) -> [u8; 4] { b.a }
fn method(b: &Buf<5>) -> u8 { b.first() }
fn main() {}
";
    let got = check_fns(TEXT, &["takes", "cap", "generic", "block", "neg", "alias", "method"]);
    all_clean("const generics", &got);
}

/// Explicit discriminants (the Reference, "Enumerations", "Discriminants": "If a discriminant
/// for a variant is not specified, then it is set to one higher than the discriminant of the
/// previous variant"; "It is an error when two variants share the same discriminant"): with a
/// primitive repr (`u8`), with none (`isize`, a negative one), and one naming a constant item.
/// Before the fix each enum was refused "an explicit discriminant".
#[test]
fn explicit_discriminants() {
    const TEXT: &str = "#[repr(u8)]
enum Small { A = 1, B, C = 0x10 }
enum Plain { X = -1, Y, Z = 5 }
const BASE: isize = 7;
enum Named { P = BASE, Q }
enum Dup { A = 2, B = 1, C }
fn small() -> Small { Small::B }
fn plain(p: Plain) -> Plain { p }
fn named() -> Named { Named::Q }
fn dup() -> Dup { Dup::A }
fn main() {}
";
    let got = check_fns(TEXT, &["small", "plain", "named", "dup"]);
    for (n, o) in &got {
        println!("discriminants: {n}: {o}");
    }
    let (clean, dup) = got.split_at(3);
    all_clean("discriminants", clean);
    assert!(dup[0].1.contains("E0081"), "`Dup`'s B and C share 2: {}", dup[0].1);
}

/// A const parameter BEFORE a type parameter takes its index (the Reference, "Generic
/// parameters": parameters are listed in order): `T` of `impl<const N: usize, T: Copy>` is
/// index 1, so its bounds and its implied `Sized` are on `Param(1)`. Before the fix preds.rs
/// counted type parameters only, put `T: Copy` and `T: Sized` on index 0 (the const `N`), and
/// the solver stopped with `Solve(Malformed(Sort ..))` (the census's 658 bodies).
#[test]
fn a_const_parameter_first_keeps_the_type_parameters_indexes() {
    const TEXT: &str = "struct Grid<const N: usize, T> { a: [T; N] }
impl<const N: usize, T: Copy> Grid<N, T> {
    fn get(&self) -> T { self.a[0] }
}
fn grid(g: &Grid<2, u8>) -> u8 { g.get() }
fn zeros() -> u8 { let z = [0u8; 32]; z[0] }
fn main() {}
";
    let got = check_fns(TEXT, &["grid", "zeros"]);
    all_clean("const first", &got);
}

/// A one-name type argument that names a type (`Option<Id>`, `Option<u8>`) stays a type, and a
/// body with no const generic at all checks as before (the census's `from_key` and `play`).
#[test]
fn one_name_type_arguments_stay_types() {
    const TEXT: &str = "struct Id { a: [u8; 32] }
fn from_key(s: &str) -> Option<Id> {
    let hex = s.as_bytes();
    if hex.len() != 64 {
        return None;
    }
    None
}
fn wraps(x: u8) -> Option<u8> { Some(x) }
fn main() {}
";
    let got = check_fns(TEXT, &["from_key", "wraps"]);
    all_clean("one-name arguments", &got);
}

/// A const parameter named as a bound's argument (`T: Tr<N>`) is a const argument (the
/// Reference, "Generic parameters": "an ambiguous generic argument is resolved as a const if
/// it does not resolve to a type"). A bound takes no const argument yet, so the head is a typed
/// refusal naming it; before the fix it became the type `Param(0)` and the solver stopped with
/// `Malformed(Sort ..)`.
#[test]
fn a_const_parameter_as_a_bound_argument_is_refused_by_name() {
    const TEXT: &str = "trait Tr<const K: usize> { fn k(&self) -> usize; }
fn bound<const N: usize, T: Tr<N>>(t: &T) -> usize { t.k() }
fn main() {}
";
    let got = check_fns(TEXT, &["bound"]);
    println!("{got:?}");
    assert!(got[0].1.contains("a const parameter in a type position"), "{got:?}");
}

/// Statics as items with their type (the Reference, "Static items": "A static item is similar
/// to a constant, except that it represents a precise memory location"; "Mutable statics":
/// "Accesses to mutable statics are unsafe"): a body reads a static, takes its address, and
/// writes a `static mut` in an `unsafe` block. Before the fix every one was "gap Static".
#[test]
fn statics_are_items_with_their_type() {
    const TEXT: &str = "static LIMIT: u32 = 7;
static mut COUNT: u32 = 0;
static NAME: &str = \"hi\";
fn read() -> u32 { LIMIT + 1 }
fn addr() -> &'static u32 { &LIMIT }
fn name_len() -> usize { NAME.len() }
fn bump() -> u32 { unsafe { COUNT += 1; COUNT } }
fn main() {}
";
    let got = check_fns(TEXT, &["read", "addr", "name_len", "bump"]);
    all_clean("statics", &got);
}

/// A refused impl is a named gap of the questions it could answer (the Reference, "Trait and
/// lifetime bounds" / "Implementations": a bound holds when an impl applies, so an impl that
/// is not read could be the one that applies). `impl Tr for W<{ size_of::<u8>() }>` is refused
/// (the evaluator does not run calls yet); `impl Tr for W<3>` is built. A goal on another shape
/// (`u16: Tr`, E0277) is answered; every goal on `W` (`W<1>: Tr`, and `W<3>: Tr` too: the
/// world answers a shape's impls whole, so a built impl's proof cannot yet stand beside a
/// refused candidate; that answer is the checker's `World::impls_of`) is the gap naming the
/// refused impl, never E0277. Before the fix every body asking about `W` stopped as "check Head".
#[test]
fn a_refused_impl_is_a_named_gap() {
    const TEXT: &str = "struct W<const N: usize>;
trait Tr { fn t(&self) -> u8 { 0 } }
impl Tr for W<3> {}
impl Tr for W<{ core::mem::size_of::<u8>() }> {}
fn need<T: Tr>(t: T) -> u8 { t.t() }
fn proven() -> u8 { need(W::<3>) }
fn other() -> u8 { need(7u16) }
fn depends() -> u8 { need(W::<1>) }
fn main() {}
";
    let got = check_fns(TEXT, &["proven", "other", "depends"]);
    for (n, o) in &got {
        println!("refused impl: {n}: {o}");
    }
    assert!(got[1].1.contains("E0277"), "{got:?}");
    for i in [0, 2] {
        assert!(got[i].1.contains("a candidate of this question, has a refused header") && got[i].1.contains("src/main.rs:4"), "{got:?}");
    }
}

/// A type alias in a body path (the Reference, "Type aliases": "A type alias defines a new name
/// for an existing type"; "A type alias to a tuple-struct or unit-struct cannot be used to
/// qualify that type's constructor"): `Same::<u8>::new()`, `Plain { x }`, `Int::MAX` resolve to
/// the aliased type; `Fixed::new()` (`type Fixed = P<u16>`, arguments the path cannot carry) is
/// the gap naming the alias, never a looser `P<_>`. Before the fix each was "gap TypeAlias".
#[test]
fn a_type_alias_in_a_body_path_is_its_type() {
    const TEXT: &str = "struct P<T> { x: T }
impl<T> P<T> { fn new(x: T) -> P<T> { P { x } } }
struct Q { x: u8 }
type Same<T> = P<T>;
type Plain = Q;
type Int = u32;
type Fixed = P<u16>;
fn same() -> P<u8> { Same::<u8>::new(1) }
fn plain() -> u8 { let q = Plain { x: 3 }; q.x }
fn int() -> u32 { Int::MAX }
fn fixed() -> P<u16> { Fixed::new(2) }
fn main() {}
";
    let got = check_fns(TEXT, &["same", "plain", "int", "fixed"]);
    println!("alias paths: {got:?}");
    all_clean("alias paths", &got[..3]);
    assert!(got[3].1.contains("aliases a type with fixed arguments"), "{got:?}");
}

/// A negative impl is a head with its polarity (rustc's `negative_impls`, which core uses for
/// `impl<T: ?Sized> !Send for *const T`): `S: Send` with `impl !Send for S` does not hold
/// (E0277), `u8: Send` does. Before the fix the head was refused "a negative impl".
#[test]
fn a_negative_impl_is_a_head() {
    const TEXT: &str = "#![feature(negative_impls)]
struct S;
impl !Send for S {}
fn need<T: Send>(_t: T) {}
fn holds() { need(1u8) }
fn not_send() { need(S) }
fn main() {}
";
    let got = check_fns(TEXT, &["holds", "not_send"]);
    println!("negative: {got:?}");
    assert_eq!(got[0].1, "clean", "{got:?}");
    assert!(got[1].1.contains("diagnostics") && got[1].1.contains("E0277") && got[1].1.contains("`S: Send`"), "{got:?}");
}

/// Reduction probe for the census's `Malformed/Sort` (bstd::blobs `from_key`).
#[test]
fn sort_probe() {
    const TEXT: &str = "fn chunks(hex: &[u8]) -> usize { let mut n = 0; for (at, _p) in hex.chunks_exact(2).enumerate() { n = at; } n }
fn index_mut(at: usize) -> u8 { let mut id = [0u8; 32]; id[at] = 1; id[0] }
fn question(a: Option<u8>) -> Option<u8> { let b = a? << 4; Some(b) }
fn closure(x: u8) -> Option<u8> { let d = |byte: u8| match byte { b'0'..=b'9' => Some(byte - b'0'), _ => None }; d(x) }
fn iter_slice(hex: &[u8]) -> u8 { let mut s = 0; for b in hex { s = *b; } s }
fn array_iter(a: [u8; 4]) -> u8 { let mut s = 0; for b in a { s = b; } s }
fn q1(a: Option<u8>) -> Option<u8> { let b = a?; Some(b) }
fn shl(a: u8) -> u8 { a << 4 }
fn shl_u(a: u8, n: u32) -> u8 { a << n }
fn add(a: u8) -> u8 { a + 4 }
fn c_only(hex: &[u8]) -> usize { let _c = hex.chunks_exact(2); 0 }
fn en(a: [u8; 2]) -> usize { let mut n = 0; for (i, _) in a.iter().enumerate() { n = i; } n }
fn for_chunks(hex: &[u8]) -> usize { for _p in hex.chunks_exact(2) {} 0 }
fn main() {}
";
    // Order-dependent: `question` alone is clean; after `chunks` on one store it was
    // "Solve(Malformed(Sort { term: 6 }))" (the census's 662). The third checker lane's fixture.
    let got = check_fns(TEXT, &["chunks", "question", "index_mut", "q1"]);
    all_clean("sort probe", &got);
}
