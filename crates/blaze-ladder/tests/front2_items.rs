//! Item heads through front2 (front2/heads.rs, types.rs's argument and length lowering,
//! world_items.rs): each fixture is a small crate whose fns are lowered and checked one by one
//! over one store; every fn of a fixture must check clean, or the test prints each fn's outcome.
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`, as tests/front2_main.rs does.

#![feature(allocator_api)]

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::check::{BuildOrder, Records, check_body};
use blaze_ladder::front2::consts::Consts;
use blaze_ladder::front2::heads::Store;
use blaze_ladder::front2::lower_made::{LowerTarget, lower_made};
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::refusal::SourceFile;
use blaze_lower::scoped::Prepares;
use blaze_own::Alloc;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};

const EDITION: &str = "blaze-2026.11";

fn open_toolchain() -> Toolchain {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"))
}

/// Each fn of `fns` in the crate `text`, lowered and checked in order over one store: its
/// name and its outcome ("clean", the diagnostics, or the failure as printed).
fn check_fns(text: &str, fns: &[&str]) -> Vec<(String, String)> {
    let tc = open_toolchain();
    let files = [("/hello-world/src/main.rs".to_string(), text.to_string())];
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
    let user = UserCrate { name: "hello_world", dir: "/hello-world/", root: "/hello-world/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
        let mut front = Front2::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut store = Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global());
    let (mut buckets, mut consts) = (None, Consts::default());
    let mut got = Vec::new();
    for &n in fns {
        let found = names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: &[n] }, Alloc::global())
            .unwrap_or_else(|e| panic!("`{n}`: {e}"));
        let k = found.whole(1).unwrap_or_else(|| panic!("`{n}` not named")).key;
        let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: Some((&mut consts, prepared)) };
        let outcome = match lower_made(&mut records, prepared, LowerTarget::Free(k)) {
            Err(e) => format!("not lowered: {e}"),
            Ok(body) => {
                let view = body.lowered.view();
                let spell = Spelling::of(&view).unwrap_or_else(|e| panic!("spelling `{n}`: {e:?}"));
                let scope = body.scope(&spell);
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
    for (n, o) in got {
        println!("{what}: {n}: {o}");
    }
    assert!(got.iter().all(|(_, o)| o == "clean"), "{what}: {got:?}");
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

/// A bound's generic argument is what the TRAIT's parameter in that position is, not what its
/// tokens look like. The Reference, "Generic parameters", "Const generics": "A const argument in
/// a path specifies the const value to use for that item."; "The const expression must be a
/// block expression (surrounded with braces) unless it is a single path segment (an IDENTIFIER)
/// or a literal (with a possibly leading - token)."; "When there is ambiguity if a generic
/// argument could be resolved as either a type or const argument, it is always resolved as a
/// type." (so a one-segment path naming a const parameter is written without braces, is parsed
/// as a type, and is a constant because the parameter it fills is one).
///
/// - `impl .. where Uint<LO>: Concat<HI>` (crypto_bigint's form, which stopped 57 bodies of
///   tempo_contracts): `HI` fills `Concat`'s const parameter, so it is the const parameter's
///   term; `needs` calls a method only that impl gives, so its where clause is proved with
///   `Concat<2>`.
/// - `T: Tr<N>` written on a parameter (`inline`), and a constant item and a braced const
///   parameter in the same position (`item`, `braced`).
/// - `H: Holds<T, N>` (`typed`): `T` is a one-segment path too, in a position `Holds` declares
///   a TYPE, so it stays a type: a fix that reads every one-segment path as a constant refuses
///   it ("`T` is a type parameter; a constant is expected here").
///
/// `sized` is checked before `item` so that `CAP` has its value: this harness lowers a fn's
/// generic parameters with no constant evaluator behind them (`lower_fn`'s first stage has no
/// round for `Why::NeedConst`; a driver makes the fn's head, and with it the constant, first).
///
/// Expected line: `const arguments in bounds: needs: clean` (and the same for `inline`, `sized`,
/// `item`, `braced`, `typed`). Before the fix each was "not lowered"/"not checked" with
/// `Unsupported("a const parameter in a type position (a bound's const argument `Tr<N>`)")`.
#[test]
fn a_bound_argument_is_const_where_the_trait_declares_const() {
    const TEXT: &str = "struct Uint<const L: usize> { limbs: [u64; L] }
trait Concat<const HI: usize> { fn low(&self) -> u64; }
impl Concat<2> for Uint<2> { fn low(&self) -> u64 { self.limbs[0] } }
struct Pair<const LO: usize, const HI: usize> { lo: Uint<LO> }
trait Low { fn first(&self) -> u64; }
impl<const LO: usize, const HI: usize> Low for Pair<LO, HI> where Uint<LO>: Concat<HI> {
    fn first(&self) -> u64 { self.lo.low() }
}
trait Tr<const K: usize> { fn k(&self) -> usize; }
trait Holds<T, const K: usize> { fn held(&self) -> T; }
const CAP: usize = 4;
fn needs(p: &Pair<2, 2>) -> u64 { p.first() }
fn inline<const N: usize, T: Tr<N>>(t: &T) -> usize { t.k() }
fn sized(a: [u8; CAP]) -> u8 { a[0] }
fn item<T: Tr<CAP>>(t: &T) -> usize { t.k() }
fn braced<const N: usize, T>(t: &T) -> usize where T: Tr<{ N }> { t.k() }
fn typed<const N: usize, T, H: Holds<T, N>>(h: &H) -> T { h.held() }
fn main() {}
";
    let got = check_fns(TEXT, &["needs", "inline", "sized", "item", "braced", "typed"]);
    all_clean("const arguments in bounds", &got);
}

/// A member fn's const parameter is the member's own: it never becomes a const parameter of
/// its trait (the Reference, "Generic parameters": "Generic parameters are in scope within the
/// item definition where they are declared."). `chunk<const N: usize>` declares index 1 of its
/// own list; `total<S>` declares index 1 of ITS own list, a type. `other<L: Later>` makes the
/// trait's members be made a second time when `Later`'s head is not there yet, which is when
/// the first round's leftovers were read: core's `Iterator::next_chunk<const N: usize>` made
/// `Iterator::sum<S>`'s `S` "a const parameter in a type position" (8 signatures of
/// tempo_primitives). Expected line: `a member's const parameter: uses: clean`.
#[test]
fn a_members_const_parameter_is_not_its_traits() {
    const TEXT: &str = "trait Later { fn l(&self) -> u8; }
trait Walk {
    fn chunk<const N: usize>(&self) -> [u8; N];
    fn other<L: Later>(&self, l: &L) -> u8;
    fn total<S>(&self, s: S) -> S;
}
struct W;
impl Later for W { fn l(&self) -> u8 { 1 } }
impl Walk for W {
    fn chunk<const N: usize>(&self) -> [u8; N] { [0u8; N] }
    fn other<L: Later>(&self, l: &L) -> u8 { l.l() }
    fn total<S>(&self, s: S) -> S { s }
}
fn uses(w: &W) -> u8 { w.total(7u8) }
fn main() {}
";
    let got = check_fns(TEXT, &["uses"]);
    all_clean("a member's const parameter", &got);
}

/// What a bound's const argument still cannot be: an expression over a generic parameter
/// (`{ N + 1 }`; the Reference, "Const generics": "const parameters may only appear as a
/// standalone argument inside of a type or array repeat expression. In those contexts, they may
/// only be used as a single segment path expression, possibly inside a block (such as N or {N}).
/// That is, they cannot be combined with other expressions."). It is a typed refusal with its
/// own name, never a guessed value. `warm` is checked first: it calls `Tr`'s method, so `Tr`'s
/// head is made (this harness lowers a fn's generic parameters with no round for a head not
/// made yet, and what says `{ N + 1 }` fills a const parameter is that head). Expected lines:
/// `warm` is `clean`; the outcome of `plus` contains
/// `a constant expression over a generic parameter`.
#[test]
fn a_bound_argument_that_computes_over_a_parameter_is_refused_by_name() {
    const TEXT: &str = "trait Tr<const K: usize> { fn k(&self) -> usize; }
struct Unit;
impl Tr<3> for Unit { fn k(&self) -> usize { 3 } }
fn warm(u: &Unit) -> usize { u.k() }
fn plus<const N: usize, T: Tr<{ N + 1 }>>(t: &T) -> usize { t.k() }
fn main() {}
";
    let got = check_fns(TEXT, &["warm", "plus"]);
    println!("{got:?}");
    assert_eq!(got[0].1, "clean", "{got:?}");
    assert!(got[1].1.contains("a constant expression over a generic parameter"), "{got:?}");
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
/// CHANGED 2026-11-16 14:3x (front-end integrator, leg 4): the constants pieces (front2
/// consts.rs / const_open.rs) now evaluate `size_of::<u8>()` in a header, so that impl is no
/// longer refused: it is `impl Tr for W<1>`. The right answers are then the language's own:
/// `proven` (`W<3>: Tr`) and `depends` (`W<1>: Tr`, by the evaluated impl) are clean and
/// `other` (`u16: Tr`) is E0277. The run of 14:31 printed exactly that. This fixture no longer
/// reaches the refused-header gap; a header the evaluator still refuses is owed by the ITEMS
/// writer (a5eefcbfe224f7f51) to cover it again.
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
        assert_eq!(got[i].1, "clean", "{got:?}");
    }
}

/// A type alias in a body path (the Reference, "Type aliases": "A type alias defines a new name
/// for an existing type"; "A type alias to a tuple-struct or unit-struct cannot be used to
/// qualify that type's constructor"): `Same::<u8>::new()`, `Plain { x }`, `Int::MAX` resolve to
/// the aliased type; so does `Fixed::new()` (`type Fixed = P<u16>`: the body's conversion writes
/// the aliased type's arguments on the alias's segment, world_paths.rs `AliasPaths::expand`),
/// as `P<u16>` and never a looser `P<_>`: `loose`, which wants a `P<u8>` from it, is E0308.
/// An alias of a type that is no ADT with a path after it (`type Tup = (u8, u8)`,
/// `Tup::default()`) is `Def::Alias`: the checker reads the aliased type and finds `default` on
/// `(u8, u8)` as on the type written, so it is clean too. Before the fixes each was "gap
/// TypeAlias" ("aliases a type with fixed arguments").
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
fn loose() -> P<u8> { Fixed::new(2) }
type Tup = (u8, u8);
fn tup() -> (u8, u8) { Tup::default() }
fn main() {}
";
    let got = check_fns(TEXT, &["same", "plain", "int", "fixed", "loose", "tup"]);
    println!("alias paths: {got:?}");
    all_clean("alias paths", &got[..4]);
    assert!(got[4].1.contains("diagnostics") && got[4].1.contains("E0308"), "{got:?}");
    assert_eq!(got[5].1, "clean", "`Tup::default()` through `type Tup = (u8, u8)`: {got:?}");
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

/// A refused impl blocks only the goals whose crate it could be written in (the Reference,
/// "Implementations", "Inherent implementations": "An implementing type must be defined within
/// the same crate as the original type definition."; "Orphan rules": "Given `impl<P1..=Pn>
/// Trait<T1..=Tn> for T0`, an `impl` is valid only if at least one of the following is true:
/// `Trait` is a local trait; At least one of the types `T0..=Tn` must be a local type."). The
/// program's `impl dyn Tr<W<{ size_of::<u8>() }>>` and `impl Display for dyn Tr<..>` are refused
/// (the evaluator runs no call yet) and their self type has no constructor read as written, so
/// each is a candidate of every inherent question and of every `Display` question. `Option`'s
/// inherent `is_some` and `u16: Display` are answered (core's type, core's trait: no impl of
/// this crate can answer them); so are `u8`'s inherent `count_ones` (a built-in type's inherent
/// impls are written only in a crate with `#![feature(rustc_attrs)]`: rustc's E0390) and
/// `u16: PartialEq` against a refused `impl PartialEq for ..` written with no arguments (`Rhs`
/// defaults to `Self`, so only the self type can be this crate's); `S`'s inherent `s` and
/// `S: Display` (S is this crate's) stay the gap naming the refused impl. Before the fix all
/// six were the gap.
#[test]
fn a_refused_impl_blocks_only_its_crates_goals() {
    const TEXT: &str = "trait Tr<T> {}
struct W<const N: usize>;
struct S;
impl S { fn s(&self) -> u8 { 1 } }
impl core::fmt::Display for S { fn fmt(&self, _f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result { Ok(()) } }
impl dyn Tr<W<{ core::mem::size_of::<u8>() }>> { fn d(&self) -> u8 { 0 } }
impl core::fmt::Display for dyn Tr<W<{ core::mem::size_of::<u16>() }>> { fn fmt(&self, _f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result { Ok(()) } }
impl PartialEq for dyn Tr<W<{ core::mem::size_of::<u32>() }>> { fn eq(&self, _o: &Self) -> bool { true } }
fn need<T: core::fmt::Display>(_t: T) {}
fn need_eq<T: PartialEq>(_t: T) {}
fn foreign_inherent() -> bool { Some(1u8).is_some() }
fn foreign_trait() { need(7u16) }
fn builtin_inherent() -> u32 { 7u8.count_ones() }
fn bare_trait() { need_eq(7u16) }
fn local_inherent() -> u8 { S.s() }
fn local_trait() { need(S) }
fn main() {}
";
    let got = check_fns(TEXT, &["foreign_inherent", "foreign_trait", "builtin_inherent", "bare_trait", "local_inherent", "local_trait"]);
    println!("by crate: {got:?}");
    // CHANGED 2026-11-16 14:3x (front-end integrator, leg 4): the constants pieces (front2
    // consts.rs / const_open.rs) now evaluate `size_of::<..>()` in a header, so none of the three
    // `dyn Tr<W<..>>` impls is refused any more and `S`'s two goals are answered by its own
    // impls: all six are clean, as the language says (the 14:31 run). The refused-header gap
    // this test was written for needs a header the evaluator still refuses: owed by the ITEMS
    // writer (a5eefcbfe224f7f51).
    all_clean("by crate", &got);
}

/// An impl's header is built from its own text and names: a projection of its own parameter in
/// its trait's arguments (`impl<T: Core> Api<Req<T::N>> for E<T>`, reth_rpc_eth_api core.rs:458's
/// `impl<T> EthApiServer<RpcTxReq<T::NetworkTypes>, ..>`) is resolved against the parameter's
/// bounds as written in that header (the Reference, "Paths", "Qualified paths": `T::N` is
/// `<T as Core>::N` for the one bound of `T` that has an `N`), never through the finished header
/// of the impl itself. Before the fix tempo's impl was "these items need each other's heads in
/// a cycle: [e97737e3dcaa, e97737e3dcaa]".
#[test]
fn an_impl_header_needs_not_itself() {
    const TEXT: &str = "trait Net { type Ty; }
trait Core { type N: Net; }
trait Full: Core {}
trait Api<A> { fn a(&self) -> u8 { 0 } }
type Req<N> = <N as Net>::Ty;
impl<T> Api<Req<T::N>> for T where T: Full, u8: From<<T::N as Net>::Ty> {}
struct C;
struct Nn;
impl Net for Nn { type Ty = u8; }
impl Core for C { type N = Nn; }
impl Full for C {}
fn need<A, X: Api<A>>(x: X) -> u8 { x.a() }
fn uses() -> u8 { need::<u8, C>(C) }
fn main() {}
";
    let got = check_fns(TEXT, &["uses"]);
    all_clean("self need", &got);
}

/// A type-relative path is read through the bounds AS WRITTEN while those bounds are lowered
/// (front2/written.rs), and a blanket impl is a candidate only of its own crate's traits.
/// `BUILDS`: the Reference, "Traits", "Supertraits": "anywhere a generic or trait object is
/// bounded by a trait, it has access to the associated items of its supertraits"; "Associated
/// Items", "Associated Types": "Associated types are type aliases associated with another
/// type"; two bounds declaring one name are the ambiguity error (rustc's E0221: "An attempt
/// was made to retrieve an associated type, but the type was ambiguous"), none here. `trait
/// Full: Types + Wrap<Self::Net>` names `Self::Net` inside its OWN supertrait list (reth's
/// rpc-eth-api: `trait Call: LoadState<Error: FromEvmError<Self::Evm>> + ..`, `trait
/// FullEthApiServer: EthApiServer<RpcTxReq<Self::NetworkTypes>, ..> + FullEthApi`): `Net` is
/// found by NAME among the written bounds (`Types` declares it) and `Wrap<..>`, which does not
/// declare it, is not read to find it. So the header of `impl<T> Server<Req<T::Net>> for T
/// where T: Full` (reth_rpc_eth_api core.rs:458's shape) builds, `K.s()` checks clean, a body
/// about the unrelated `U: Other` is clean, and `early`'s `T: Wrap<T::Net> + Types` (the name
/// used in a bound written BEFORE the bound that declares it) builds too.
/// `UNREAD`: the Reference, "Orphan rules": "Given `impl<P1..=Pn> Trait<T1..=Tn> for T0`, an
/// `impl` is valid only if at least one of the following is true: `Trait` is a local trait; At
/// least one of the types `T0..=Tn` must be a local type. Let `Ti` be the first such type. No
/// uncovered type parameters `P1..=Pn` may appear in `T0..Ti` (excluding `Ti`)": in `impl<T>
/// Nope<T> for T {}` (`Nope` names nothing: an unread header) `T0` is an uncovered parameter,
/// so the impl can only be of a trait of this crate: `u16: From<u8>` (core's trait) is clean,
/// `U: Local` (this crate's) stays the gap naming the unread impl.
/// Without the fix the expected lines are "written bounds: builds: calls: not checked: ..
/// these items need each other's heads in a cycle: [..]" (the members of `Full` asking for the
/// supertraits of `Full`), "written bounds: builds: early: not lowered: .. `T::X` in a bound
/// written before the bound of `T` that declares `X`" and "written bounds: unread: foreign:
/// not checked: .. a candidate filed without its trait or self type (unread as written), has
/// no header" (NOT RUN by the writer: a writer runs nothing). With it: "written bounds: builds:
/// calls: clean", ".. unrelated: clean", ".. early: clean", "written bounds: unread: foreign:
/// clean" and "written bounds: unread: local: not checked: .. unread as written ..".
#[test]
fn a_projection_in_a_header_reads_the_written_bounds() {
    const BUILDS: &str = "trait Types { type Net; }
trait Wrap<A> {}
trait Full: Types + Wrap<Self::Net> {}
trait Server<A> { fn s(&self) -> u8; }
struct Req<N>(N);
impl<T> Server<Req<T::Net>> for T where T: Full { fn s(&self) -> u8 { 1 } }
struct K;
impl Types for K { type Net = u8; }
impl Wrap<u8> for K {}
impl Full for K {}
trait Other { fn o(&self) -> u8; }
struct U;
impl Other for U { fn o(&self) -> u8 { 2 } }
fn calls() -> u8 { K.s() }
fn unrelated() -> u8 { U.o() }
fn early<T: Wrap<T::Net> + Types>(_t: &T) -> u8 { 0 }
fn main() {}
";
    const UNREAD: &str = "trait Local { fn l(&self) -> u8 { 0 } }
struct U;
impl Local for U {}
impl<T> Nope<T> for T {}
fn need_from<T: From<u8>>(_t: T) {}
fn need_local<T: Local>(_t: T) {}
fn foreign() { need_from(7u16) }
fn local() { need_local(U) }
fn main() {}
";
    let builds = check_fns(BUILDS, &["calls", "unrelated", "early"]);
    let unread = check_fns(UNREAD, &["foreign", "local"]);
    for (what, got) in [("builds", &builds), ("unread", &unread)] {
        for (n, o) in got.iter() {
            println!("written bounds: {what}: {n}: {o}");
        }
    }
    all_clean("written bounds: builds", &builds);
    assert_eq!(unread[0].1, "clean", "{unread:?}");
    assert!(unread[1].1.contains("unread as written"), "{unread:?}");
}

/// `Self` in an impl's where clauses is the impl's self type (the Reference, "Paths", "`Self`":
/// "`Self`, with a capital \"S\", is used to refer to the current type being implemented or
/// defined"; "In an implementation, it refers to the type being implemented."): `impl<T> Tr<T>
/// for S where T: From<Self>` and ruint's from.rs:397 shape, `where T: for<'a> From<&'a Self>`
/// (tempo census 13: "57 gap RefusedImpl [UintTryTo]: Unsupported \"`Self` here\""). The header
/// builds because heads.rs `impl_` lowers the self type before the bounds; `by_value` and
/// `by_ref` check clean. `REFUSED`: `impl Tr for W<Self>` names `Self` in the self type itself,
/// where there is no type being implemented yet to refer to (rustc: "`Self` is not valid in the
/// self type of an impl block"): the header stays refused, by its own name. A fix that gives
/// `Self` some type everywhere fails the second.
/// Without the fix the expected lines are "self in where: by_value: not checked: .. has a
/// refused header: .. Unsupported(\"`Self` here\" .." and the same for `by_ref` (NOT RUN by the
/// writer). With it: "self in where: by_value: clean", "self in where: by_ref: clean" and
/// "self in where: refused: asks: not checked: .. has a refused header: .. an impl's self type
/// naming `Self` ..".
#[test]
fn self_in_an_impls_where_clause_is_its_self_type() {
    const TEXT: &str = "struct S;
trait Tr<T> { fn g(self) -> u8; }
trait Tr2<T> { fn h(self) -> u8; }
impl<T> Tr<T> for S where T: From<Self> { fn g(self) -> u8 { 7 } }
impl<T> Tr2<T> for S where T: for<'a> From<&'a Self> { fn h(self) -> u8 { 8 } }
impl From<S> for u8 { fn from(_s: S) -> u8 { 1 } }
impl<'a> From<&'a S> for u16 { fn from(_s: &'a S) -> u16 { 2 } }
fn by_value(s: S) -> u8 { <S as Tr<u8>>::g(s) }
fn by_ref(s: S) -> u8 { <S as Tr2<u16>>::h(s) }
fn main() {}
";
    const REFUSED: &str = "struct W<T>(T);
trait Tr { fn g(&self) -> u8 { 0 } }
impl Tr for W<Self> {}
fn need<T: Tr>(_t: T) {}
fn asks() { need(W(1u8)) }
fn main() {}
";
    let got = check_fns(TEXT, &["by_value", "by_ref"]);
    let refused = check_fns(REFUSED, &["asks"]);
    for (n, o) in &refused {
        println!("self in where: refused: {n}: {o}");
    }
    all_clean("self in where", &got);
    assert!(refused[0].1.contains("has a refused header") && refused[0].1.contains("self type naming"), "{refused:?}");
}

/// A type alias's defaulted parameter left out of a path takes its default (the Reference,
/// "Type aliases": "A type alias defines a new name for an existing type"; "Generic parameters":
/// a type parameter with a default may be left out, the arguments before it put in): `Res<u8>`
/// for `type Res<T, E = Er> = Result<T, E>` is `Result<u8, Er>` (alloy_sol_types's
/// `Result<Self>`). Before the fix: "`Res < u8 >` is written with 1 type arguments; it takes 2".
#[test]
fn a_type_alias_default_fills_a_left_out_argument() {
    const TEXT: &str = "struct Er;
type Res<T, E = Er> = core::result::Result<T, E>;
fn ok() -> Res<u8> { Ok(1) }
fn err() -> Res<u8> { Err(Er) }
fn both() -> Res<u8, u16> { Err(2u16) }
fn main() {}
";
    let got = check_fns(TEXT, &["ok", "err", "both"]);
    all_clean("alias default", &got);
}

/// Reduction probe for the census's `Malformed/Sort` (bstd::blobs `from_key`).
/// A trait impl is never a candidate of an inherent question (the Reference, "Implementations":
/// "Inherent implementations": "The nominal type is called the implementing type and the
/// associable items are the associated items to the implementing type"; a "Trait
/// implementation" is "defined like an inherent implementation except that the optional
/// generic type declarations are followed by a trait, followed by the keyword `for`"): the
/// inherent items named `m` of `X` come from `impl X` blocks only, so a trait impl whose header
/// is refused cannot stand in the way of `X.m()`. An impl's kind is read from its header's
/// tokens (`blaze_file_items::impl_header`) before anything of it is lowered.
/// Two crates. `plain`: the refused trait impl is at module level (`impl Display for dyn
/// Tr<W<{ size_of::<u16>() }>>`, the form of `a_refused_impl_blocks_only_its_crates_goals`).
/// `block`: zerocopy 0.8.63's src/impls.rs:751..800 reduced: `const _: () = { pub enum value {}
/// unsafe impl<Client, T: ?Sized> HasField<Client, value, { crate::STRUCT_VARIANT_ID }, {
/// crate::ident_id!(value) }> for ManuallyDrop<T> { .. } };`: a trait impl in a block naming a
/// type of that block, with a macro call as a const argument (refused: the evaluator runs no
/// macro call in a head). The walk of 12:12 had this impl as "88 x check gap: RefusedImpl ..
/// the impl b6e41207260879b0.. at (no file place), a candidate of this question, has a refused
/// header: .. Program("argument 2 of `HasField` is a constant; that parameter is not a const
/// parameter", Range { start: 56, end: 61 })" in bodies that ask nothing of `HasField`.
/// Without the fix the expected line of `block` is "trait impls: block: calls: not checked: ..
/// check gap: RefusedImpl .. a candidate of this question, has a refused header" (NOT RUN by
/// the writer: a writer runs nothing; if `block` is clean without the fix too, the walk's 88
/// came by another road and the gap's new "[asked: ..]" text names it).
#[test]
fn a_trait_impl_is_no_candidate_of_an_inherent_question() {
    const PLAIN: &str = "trait Tr<T> {}
struct W<const N: usize>;
struct X;
impl X { fn m(&self) -> u8 { 1 } }
impl core::fmt::Display for dyn Tr<W<{ core::mem::size_of::<u16>() }>> { fn fmt(&self, _f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result { Ok(()) } }
fn calls() -> u8 { X.m() }
fn main() {}
";
    const BLOCK: &str = "trait HasF<Client, Field, const V: i128, const F: i128> { fn only(&self) -> u8 { 0 } }
const SV: i128 = 2;
macro_rules! id { ($f:ident) => { 7 } }
struct M<T>(T);
struct X;
impl X { fn m(&self) -> u8 { 1 } }
const _: () = {
    pub enum value {}
    impl<Client, T> HasF<Client, value, { crate::SV }, { crate::id!(value) }> for M<T> {}
};
fn calls() -> u8 { X.m() }
fn by_ref(x: &X) -> u8 { x.m() }
fn main() {}
";
    let plain = check_fns(PLAIN, &["calls"]);
    let block = check_fns(BLOCK, &["calls", "by_ref"]);
    for (what, got) in [("plain", &plain), ("block", &block)] {
        for (n, o) in got.iter() {
            println!("trait impls: {what}: {n}: {o}");
        }
    }
    all_clean("trait impls: plain", &plain);
    all_clean("trait impls: block", &block);
}

/// A trait's const parameters keep their places in its head, and an impl's const arguments
/// are read against them (the Reference, "Generic parameters", "Const generics": "A const
/// argument in a path specifies the const value to use for that item"; a const argument is "a
/// literal", "a single segment path" or "a block expression"): zerocopy 0.8.63 src/lib.rs:1310
/// is `pub unsafe trait HasField<Client, Field, const VARIANT_ID: i128, const FIELD_ID: i128>`
/// (`Self` is parameter 0, so the two const parameters are 3 and 4), and its impls give them
/// `{ crate::STRUCT_VARIANT_ID }` and a literal. Here the trait has its first const parameter
/// in third place (after `Self` and `Field`): `impl HasF<Val, 3, { SV }> for M` (a literal, a
/// braced constant item) and `impl HasF<Val, { 4 }, -1> for Z` (a braced literal, a negative
/// literal) build, so `M.f()` and `Z.f()` check clean. The walk's refusal "argument 2 of
/// `HasField` is a constant; that parameter is not a const parameter" was NOT a lost const
/// parameter: argument 2 there is `value`, a type of the impl's block that the impl's module
/// does not name; `unresolved` has that shape at module level (`Nope` names nothing) and must
/// say so by the name's resolution, never as "is a constant".
#[test]
fn a_trait_const_parameter_takes_a_literal_and_a_braced_constant() {
    const TEXT: &str = "trait HasF<Field, const V: i128, const F: i128> { fn f(&self) -> u8 { 0 } }
const SV: i128 = 2;
enum Val {}
struct M;
struct Z;
impl HasF<Val, 3, { SV }> for M {}
impl HasF<Val, { 4 }, -1> for Z {}
fn m() -> u8 { M.f() }
fn z() -> u8 { Z.f() }
fn main() {}
";
    let got = check_fns(TEXT, &["m", "z"]);
    all_clean("trait const parameters", &got);
    const UNRESOLVED: &str = "trait HasF<Field, const V: i128, const F: i128> { fn f(&self) -> u8 { 0 } }
struct M;
impl HasF<Nope, 3, 4> for M {}
fn m() -> u8 { M.f() }
fn main() {}
";
    let got = check_fns(UNRESOLVED, &["m"]);
    println!("trait const parameters: unresolved: {got:?}");
    assert!(got[0].1.contains("has a refused header") && !got[0].1.contains("is a constant"), "{got:?}");
}

/// An UNREAD trait impl written with no type argument on its trait is judged by the bare home
/// as a read one is (the Reference, "Orphan rules": "at least one of the types `T0..=Tn` must be
/// a local type"; with the trait's parameters at their default `Self` only `T0` can be). `impl
/// Nope for Missing {}` resolves neither its trait nor its self type (serde's `impl<'de>
/// _serde::Deserialize<'de> for __Field` in a block is filed so today: tempo-front-6b's "7443
/// NEW gap UnreadImpl"), so it is a candidate of every trait question at every shape; `u16:
/// PartialEq` (core's trait, `Rhs = Self`, a built-in self type) cannot be answered by an impl
/// of this crate written without arguments: clean. `u16: From<u8>` (a parameter with no
/// default: no narrowing without the goal's arguments) stays the gap naming the unread impl.
/// Without the fix the expected line is "unread bare: bare: not checked: .. a candidate filed
/// without its trait or self type (unread as written), has no header" (NOT RUN by the writer).
#[test]
fn an_unread_bare_trait_impl_is_judged_by_the_bare_home() {
    const TEXT: &str = "impl Nope for Missing {}
fn need_eq<T: PartialEq>(_t: T) {}
fn need_from<T: From<u8>>(_t: T) {}
fn bare() { need_eq(7u16) }
fn with_args() { need_from(7u16) }
fn main() {}
";
    let got = check_fns(TEXT, &["bare", "with_args"]);
    for (n, o) in &got {
        println!("unread bare: {n}: {o}");
    }
    assert_eq!(got[0].1, "clean", "{got:?}");
    assert!(got[1].1.contains("unread as written"), "{got:?}");
}

/// The "has a refused header" gap and its narrowing by crate, on headers NO evaluator can ever
/// build: each names a type that does not exist (`Missing`: rustc's E0412), so the refusal is
/// the program's own and stays one whatever constants become evaluable (the two older
/// fixtures used `W<{ size_of::<u8>() }>`, which the constants pieces of 14:31 now evaluate).
/// `impl Tr for G<Missing>` is a candidate of `Tr` at `G`: `G<u8>: Tr` is the gap naming it,
/// never E0277 (the Reference, "Trait and lifetime bounds": a bound holds when an impl
/// applies, and an impl that is not read could be the one); `u16: Tr` is at another shape:
/// E0277. `impl core::fmt::Display for dyn Dy<Missing>` has no self constructor read as
/// written, so it is a candidate of `Display` at every shape this crate may implement it for:
/// `S: Display` (S is this crate's) is the gap; `u16: Display` (core's trait, a built-in type:
/// "Orphan rules": "at least one of the types `T0..=Tn` must be a local type") is clean; and
/// `S.s()`, an inherent question, is clean: a trait impl is never its candidate.
#[test]
fn an_unbuildable_header_is_a_named_gap_of_its_own_questions_only() {
    const TEXT: &str = "struct G<T>(T);
struct S;
trait Tr { fn t(&self) -> u8 { 0 } }
trait Dy<T> {}
impl Tr for G<Missing> {}
impl S { fn s(&self) -> u8 { 1 } }
impl core::fmt::Display for dyn Dy<Missing> { fn fmt(&self, _f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result { Ok(()) } }
fn need<T: Tr>(t: T) -> u8 { t.t() }
fn show<T: core::fmt::Display>(_t: T) {}
fn same_shape() -> u8 { need(G(1u8)) }
fn other_shape() -> u8 { need(7u16) }
fn local_display() { show(S) }
fn foreign_display() { show(7u16) }
fn inherent() -> u8 { S.s() }
fn main() {}
";
    let got = check_fns(TEXT, &["same_shape", "other_shape", "local_display", "foreign_display", "inherent"]);
    for (n, o) in &got {
        println!("unbuildable header: {n}: {o}");
    }
    assert!(got[0].1.contains("has a refused header") && got[0].1.contains("src/main.rs:5"), "{got:?}");
    assert!(got[1].1.contains("E0277"), "{got:?}");
    assert!(got[2].1.contains("has a refused header") && got[2].1.contains("src/main.rs:7"), "{got:?}");
    all_clean("unbuildable header", &got[3..]);
}

/// The row of each ADT named `adts` in the crate `text`, made as front2/program.rs's ADT-row
/// loop makes it (`Store::ensure(Want::Adt)`; a constant its head names is evaluated by
/// `eval_len` and the head made again, at most four times): per ADT its name and either
/// "fields [..]" with each field's type node printed (a pattern type as `base is [lo..=hi, ..]`
/// with the bits the constant evaluator gave) and "discrs [..]", or the refusal as printed.
fn adt_rows(text: &str, adts: &[&str]) -> Vec<(String, String)> {
    use blaze_ladder::front2::check::eval_len;
    use blaze_ladder::front2::heads::{HeadError, Want};
    use blaze_ladder::front2::keys::checker_of;
    use blaze_ladder::front2::types::Why;
    use blaze_typeck::PNode;
    let tc = open_toolchain();
    let files = [("/hello-world/src/main.rs".to_string(), text.to_string())];
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
    let user = UserCrate { name: "hello_world", dir: "/hello-world/", root: "/hello-world/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
    let mut front = Front2::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut store = Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global());
    let (mut buckets, mut consts) = (None, Consts::default());
    let mut got = Vec::new();
    for &n in adts {
        let found = names::path(&names, names.root(0), Namespace::Types, Written { kind: PathKind::Plain, segments: &[n] }, Alloc::global())
            .unwrap_or_else(|e| panic!("`{n}`: {e}"));
        let lib = found.whole(1).unwrap_or_else(|| panic!("`{n}` not named")).key;
        let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: Some((&mut consts, prepared)) };
        let mut lens = 0;
        let refused = loop {
            match records.store.ensure(records.names, records.keys, Want::Adt(lib)) {
                Err(HeadError::Item(_, Why::NeedConst(c, _))) if lens < 4 => {
                    lens += 1;
                    if let Err(e) = eval_len(&mut records, c) {
                        break Some(format!("the constant {c:x} its head names: {e:?}"));
                    }
                }
                Err(e) => break Some(format!("{e:?}")),
                Ok(()) => break None,
            }
        };
        let outcome = match (refused, store.adts.get(&checker_of(lib))) {
            (Some(why), _) => format!("refused: {why}"),
            (None, None) => "made, but no row filed under its key".to_string(),
            (None, Some(row)) => {
                let view = store.tys.view();
                let mut fields = Vec::new();
                for f in row.fields.iter() {
                    fields.push(match view.node(f.ty) {
                        Ok(PNode::Pat(base, ends)) => {
                            let ends = view.list(ends).expect("the pattern's ends");
                            let mut runs = Vec::new();
                            for pair in ends.chunks(2) {
                                let bits = |t| match view.node(t) {
                                    Ok(PNode::ConstValue { bits, .. }) => format!("{bits:#x}"),
                                    other => format!("{other:?}"),
                                };
                                runs.push(format!("{}..={}", bits(pair[0]), bits(pair[1])));
                            }
                            format!("{:?} is [{}]", view.node(base).expect("the base"), runs.join(", "))
                        }
                        // An array: its element's node and its length's value (`array of
                        // Uint(..) len 2`), the length a node only when it is not a value.
                        Ok(PNode::Array(elem, len)) => match view.value_u64(len) {
                            Some(n) => format!("array of {:?} len {n}", view.node(elem).expect("the element")),
                            None => format!("array of {:?} len {:?}", view.node(elem).expect("the element"), view.node(len)),
                        },
                        other => format!("{other:?}"),
                    });
                }
                format!("fields [{}] discrs {:?}", fields.join("; "), row.discrs)
            }
        };
        got.push((n.to_string(), outcome));
    }
    got
}

/// A pattern type whose range end is a constant item (the pinned core,
/// library/core/src/num/niche_types.rs:113 "const HALF_USIZE: usize = usize::MAX >> 1;" and
/// :129 "    pub struct UsizeNoHighBit(usize is 0..=HALF_USIZE);", inside
/// `define_valid_range_type! { .. }` whose arm writes "$vis struct $name(pattern_type!($int is
/// $pat));"; :114 "const HALF_USIZE_MINUS_ONE: usize = HALF_USIZE - 1;" is a constant that
/// names a constant). The struct's row is made with ONE field whose type is the pattern type
/// over `usize` with the one range `0..=0x7fffffffffffffff` (`usize::MAX >> 1` on a 64-bit
/// target, the value the constant evaluator gave for `HALF_USIZE`), and the second's with
/// `0..=0x7ffffffffffffffe`. The walk's "111 x NoAdt: no row for the ADT
/// `core::num::niche_types::UsizeNoHighBit`; the first: `main`" is the builder finding no row:
/// this fixture says whether the HEAD is the reason (a refusal printed here) or the seam
/// (the row is made here and still not found there).
#[test]
fn a_pattern_type_range_end_is_a_constant_items_value() {
    const TEXT: &str = "#![feature(pattern_types, pattern_type_macro)]
use core::pattern_type;
macro_rules! define_valid_range_type {
    ($(
        $(#[$m:meta])*
        $vis:vis struct $name:ident($int:ident is $pat:pat);
    )+) => {$(
        #[repr(transparent)]
        $(#[$m])*
        $vis struct $name(pattern_type!($int is $pat));
    )+};
}
const HALF_USIZE: usize = usize::MAX >> 1;
const HALF_USIZE_MINUS_ONE: usize = HALF_USIZE - 1;
define_valid_range_type! {
    pub struct UsizeNoHighBit(usize is 0..=HALF_USIZE);
    pub struct UsizeNoHighBitMinusOne(usize is 0..=HALF_USIZE_MINUS_ONE);
}
fn main() {}
";
    let got = adt_rows(TEXT, &["UsizeNoHighBit", "UsizeNoHighBitMinusOne"]);
    for (n, o) in &got {
        println!("pattern type head: {n}: {o}");
    }
    assert_eq!(got[0].1, "fields [Uint(Usize) is [0x0..=0x7fffffffffffffff]] discrs []", "{got:?}");
    assert_eq!(got[1].1, "fields [Uint(Usize) is [0x0..=0x7ffffffffffffffe]] discrs []", "{got:?}");
}

/// An enum's written discriminant is a constant expression (the Reference, "Enumerations",
/// "Discriminants": "an enumeration can explicitly state its discriminants by following the
/// variant name with `=` and a constant expression"), not only a literal or a constant item: the
/// pinned core's library/core/src/mem/alignment.rs:377 has "#[repr(usize)]" / "enum
/// AlignmentEnum {" / "    _Align1Shl0 = 1 << 0," / "    _Align1Shl1 = 1 << 1," .. (the walk's
/// "14 x NoAdt" on `AlignmentEnum`). The row holds each value: 1, 2, 4, and `(1 << 3) | 1` = 9.
/// Each such expression is an ANONYMOUS constant (front2/anon.rs): the item `const _: usize =
/// 1 << 0;` of the enum's module, keyed by (module, type, text) with `MemberKind::Anon`, its
/// head in `Store::fns`, needed as `Why::NeedConst` under its need key and evaluated by
/// `check::eval_len` through the same machine as a constant item's body; all of one enum's are
/// asked in one pass and evaluated in one round (`adt_rows` allows four). `Signed`: the
/// Reference, "Negation operators" and "Arithmetic and Logical Binary Operators": `-(1 << 3)`
/// at `i8` is -8 (bits 0xf8, sign-extended by the head), `5 - 2` is 3, and the implicit `C` is
/// the previous plus one, 4. `Top`: the pinned core's `u8::MAX` (library/core/src/num/
/// uint_macros.rs, in `impl u8`: "pub const MAX: Self = !0;") is an associated constant of an
/// inherent impl, read through the anonymous constant `const _: u8 = u8::MAX;`: `!0` at `u8` is
/// 255 (tempo's census class "ConstPath \"u8 :: MAX\""). Before the fix each printed "refused:
/// Item(.., ConstPath(\"1 << 0\", ..))". Expected lines: "discriminant expressions:
/// AlignmentEnum: fields [] discrs [1, 2, 4, 9]", "discriminant expressions: Signed: fields []
/// discrs [-8, 3, 4]", "discriminant expressions: Top: fields [] discrs [255, 0]".
#[test]
fn a_discriminant_is_a_constant_expression() {
    const TEXT: &str = "#[repr(usize)]
enum AlignmentEnum {
    _Align1Shl0 = 1 << 0,
    _Align1Shl1 = 1 << 1,
    _Align1Shl2 = 1 << 2,
    Odd = (1 << 3) | 1,
}
#[repr(i8)]
enum Signed { A = -(1 << 3), B = 5 - 2, C }
#[repr(u8)]
enum Top { A = u8::MAX, B = 0 }
fn main() {}
";
    let got = adt_rows(TEXT, &["AlignmentEnum", "Signed", "Top"]);
    for (n, o) in &got {
        println!("discriminant expressions: {n}: {o}");
    }
    assert_eq!(got[0].1, "fields [] discrs [1, 2, 4, 9]", "{got:?}");
    assert_eq!(got[1].1, "fields [] discrs [-8, 3, 4]", "{got:?}");
    assert_eq!(got[2].1, "fields [] discrs [255, 0]", "{got:?}");
}

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

/// A trait's associated constant read through a qualified path in an array length (tempo's
/// primitives: `[u8; (((<B4 as ::modular_bitfield::Specifier>::BITS + <B1 as ..>::BITS) ..`,
/// about 68 refused signatures). The Reference, "Associated Items", "Associated Constants":
/// "An associated constant declaration declares a signature for associated constant
/// definitions"; "An associated constant definition defines a constant associated with a
/// type"; its example "Using default values" has `trait ConstantIdDefault { const ID: i32 = 1;
/// }`, `impl ConstantIdDefault for Struct {}` and `assert_eq!(1, <Struct as
/// ConstantIdDefault>::ID)`: an impl that writes no definition has the trait's default.
/// `Packed`: `<B4 as Spec>::BITS` is 4 and `<B1 as Spec>::BITS` is 1 (each impl's own
/// definition), so the length is (4 + 1 + 3) / 8 + 1 = 8 / 8 + 1 = 2. `Padded`: `B4` writes no
/// `PAD`, so its value is the trait's default 3; `B1` writes 5; the length is 3 + 5 = 8.
/// Before the change a trait's constant had no head (heads.rs `trait_members` filed no row,
/// world_items.rs `assoc_item` answered `None`), so each row printed "refused: ..". Expected
/// lines: "trait constants: Packed: fields [array of Uint(U8) len 2] discrs []" and "trait
/// constants: Padded: fields [array of Uint(U8) len 8] discrs []" (the element's node as the
/// checker prints `u8`; the assertions read only the length).
#[test]
fn a_trait_constant_in_an_array_length_is_its_impls_value() {
    const TEXT: &str = "trait Spec { const BITS: usize; const PAD: usize = 3; }
struct B4;
impl Spec for B4 { const BITS: usize = 4; }
struct B1;
impl Spec for B1 { const BITS: usize = 1; const PAD: usize = 5; }
struct Packed([u8; (<B4 as Spec>::BITS + <B1 as Spec>::BITS + 3) / 8 + 1]);
struct Padded([u8; <B4 as Spec>::PAD + <B1 as Spec>::PAD]);
fn main() {}
";
    let got = adt_rows(TEXT, &["Packed", "Padded"]);
    for (n, o) in &got {
        println!("trait constants: {n}: {o}");
    }
    // A qualified leaf of a HEAD's expression is answered by selection before the expression
    // is evaluated (front2/const_qpath.rs), so these two no longer wait on
    // [`CONST_PATH_ARGS_OWED`] (which still holds for a path inside a constant's body: the
    // test below). Never the trait's default taken for an impl that overrides it: the first
    // run of this test printed "Padded: .. len 6", 3 + 3.
    assert!(got[0].1.starts_with("fields [array of ") && got[0].1.ends_with(" len 2] discrs []"), "{got:?}");
    assert!(got[1].1.starts_with("fields [array of ") && got[1].1.ends_with(" len 8] discrs []"), "{got:?}");
}

/// What `modular_bitfield` writes for a bitfield struct's bytes (tempo's primitives, about 115
/// bodies stopped at "Item/ConstPath \"( ( ( < B4 as :: modular_bitfield :: Specifier > ::
/// BITS + ..\""): an array length that is arithmetic over qualified paths to a trait's
/// associated constant on concrete types. The Reference, "Associated Items", "Associated
/// Constants": "An associated constant definition defines a constant associated with a type";
/// its example "Using default values" (`trait ConstantIdDefault { const ID: i32 = 1; }`, `impl
/// ConstantIdDefault for Struct {}`, `assert_eq!(1, <Struct as ConstantIdDefault>::ID)`): the
/// value is the implementing type's definition, or the trait's default where the impl writes
/// none. So each leaf is answered by SELECTING the impl of the trait for the written type
/// (front2/const_qpath.rs; `ConstsWorld::impl_const`).
/// `Packed`: `<B4 as Spec>::BITS` is 4 and `<B1 as Spec>::BITS` is 1, so the length is ((4 +
/// 1 - 1) / 8) + 1 = (4 / 8) + 1 = 0 + 1 = 1 (integer division: the Reference, "Arithmetic
/// and Logical Binary Operators": "Integer division rounds towards zero"). `first` reads the
/// field: its body is clean. `Missing`: `B9` has no impl of `Spec`, so there is no definition
/// to take: a typed refusal (`ConstFail::NoImplConst`) that names the leaf as written (the
/// trait and the type) and never a guessed value. `Inner`: the path is inside a constant
/// item's BODY, where the checker's record holds no `Self` yet: still refused by name
/// (`ConstFail::TraitConstNoSelf`) while [`CONST_PATH_ARGS_OWED`].
/// Before the change `Packed` printed "refused: .. TraitConstNoSelf ..". Expected lines:
/// "qualified constants: Packed: fields [array of Uint(U8) len 1] discrs []", "qualified
/// constants: Missing: refused: Item(.., ConstPath(\"<B9 as Spec>::BITS: no impl of trait ..
/// for the type Adt(..) gives its constant ..\", ..))" and "qualified constants: first:
/// clean".
#[test]
fn a_qualified_trait_constant_in_an_array_length_is_selected_by_its_impl() {
    const TEXT: &str = "trait Spec { const BITS: usize; }
struct B4;
impl Spec for B4 { const BITS: usize = 4; }
struct B1;
impl Spec for B1 { const BITS: usize = 1; }
struct B9;
struct Packed { bytes: [u8; ((<B4 as Spec>::BITS + <B1 as Spec>::BITS - 1) / 8) + 1] }
struct Missing([u8; <B9 as Spec>::BITS + 1]);
const INNER: usize = <B4 as Spec>::BITS;
struct Inner([u8; INNER]);
fn first(p: Packed) -> u8 { p.bytes[0] }
fn main() {}
";
    let got = adt_rows(TEXT, &["Packed", "Missing", "Inner"]);
    for (n, o) in &got {
        println!("qualified constants: {n}: {o}");
    }
    assert!(got[0].1.starts_with("fields [array of ") && got[0].1.ends_with(" len 1] discrs []"), "{got:?}");
    let missing = &got[1].1;
    assert!(missing.starts_with("refused: ") && missing.contains("ConstPath") && missing.contains("<B9 as Spec>::BITS") && missing.contains("no impl of trait"), "{got:?}");
    if CONST_PATH_ARGS_OWED {
        let inner = &got[2].1;
        assert!(inner.starts_with("refused: ") && (inner.contains("TraitConstNoSelf") || inner.contains("with no `Self`")), "{got:?}");
    } else {
        assert!(got[2].1.ends_with(" len 4] discrs []"), "{got:?}");
    }
    all_clean("qualified constants", &check_fns(TEXT, &["first"]));
}

/// OWED BY THE CHECKER AND THE EVALUATOR (2026-11-16): a checked body records a constant path
/// as `Def::Const(key)` with no arguments (blaze-typeck rules/lit_path.rs `assoc_item_ty`
/// pushes `Def::Const(item)` and drops `parent_args`), and blaze-consteval's machine/code.rs
/// `Expr::Path` instances it at `List::EMPTY` without asking `ConstWorld::resolve`. So
/// `<B4 as Spec>::BITS` reaches front2 with no `Self`, and front2 refuses it by name
/// (`ConstFail::TraitConstNoSelf`). While `true`, the trait-constant test asserts exactly that
/// refusal; set it to `false` when the arguments arrive, and the values above are asserted.
/// Set to `false` 2026-11-16 14:3x (front-end integrator, leg 4): the 14:31 run printed
/// "qualified constants: Inner: fields [array of Uint(U8) len 4] discrs []" (`INNER` =
/// `<B4 as Spec>::BITS` = 4: right), after the constants pieces (front2 consts.rs /
/// const_open.rs) landed.
const CONST_PATH_ARGS_OWED: bool = false;

/// A struct's size in a constant: `size_of::<S>()` reaches the constant evaluator's layout,
/// which reads the store's row of `S` (front2/consts.rs `LayoutFacts::adt`, the row made on
/// demand by `adt_row` through `Store::ensure(Want::Adt)`, the run path's maker). The pinned
/// core, library/core/src/mem/mod.rs: `size_of::<T>()` is "<T as SizedTypeProperties>::SIZE",
/// the trait's default "const SIZE: usize = intrinsics::size_of::<Self>();" through "impl<T>
/// SizedTypeProperties for T {}": so this also runs a trait default selected through a
/// blanket impl. The value: the Reference, "Type layout", "The Rust representation": the only
/// guarantees are that "The fields are properly aligned", "The fields do not overlap" and
/// "The alignment of the type is at least the maximum alignment of its fields"; "Size and
/// Alignment": "The size of a value is always a multiple of its alignment". `S { a: u8, b:
/// u32 }` holds 1 + 4 bytes that do not overlap (N >= 5) and is aligned to at least `u32`'s 4
/// (N % 4 == 0, on every target blaze builds for: `u32` is 4-aligned on arm64 and x86-64).
/// Nothing more is guaranteed, so nothing more is asserted: 8 is what a layout with no waste
/// gives, and any N >= 5 that is a multiple of 4 is accepted. Before the change `adt` answered
/// `None` and the row printed "refused: the constant .. the world has no ADT definition for
/// ..". Expected line: "struct size: T: fields [array of Uint(U8) len 8] discrs []".
#[test]
fn a_structs_size_in_a_constant_is_its_layouts() {
    // The intrinsic itself (the pinned core's `SizedTypeProperties::SIZE` is
    // "intrinsics::size_of::<Self>()"): `core::mem::size_of::<S>()` reads that trait constant
    // through a path, which is owed ([`CONST_PATH_ARGS_OWED`]).
    const TEXT: &str = "#![feature(core_intrinsics)]
struct S { a: u8, b: u32 }
const N: usize = core::intrinsics::size_of::<S>();
struct T([u8; N]);
fn main() {}
";
    let got = adt_rows(TEXT, &["T"]);
    for (n, o) in &got {
        println!("struct size: {n}: {o}");
    }
    let row = &got[0].1;
    let len: Option<u64> = row.strip_suffix("] discrs []").and_then(|r| r.rsplit_once(" len ")).and_then(|(_, n)| n.parse().ok());
    let n = len.unwrap_or_else(|| panic!("the row has no array length that is a value: {got:?}"));
    assert!(n >= 5 && n % 4 == 0, "size_of::<S>() = {n}: {got:?}");
}

/// An inherent item is found by its name before any impl's self type is read (the Reference,
/// "Paths", "Qualified paths" and "Method call expressions": the candidates are the type's
/// inherent items OF THAT NAME): crypto-bigint's macro-made `impl Uint<{ <U1792>::LIMBS / 2 }>
/// { fn concat .. }` (uint/macros.rs `impl_uint_concat_split_even`). The header's anonymous
/// constant reads the inherent constant `LIMBS`, which only `impl<const L: usize> Uint<L>`
/// declares, so the impl whose header is being evaluated is no candidate of `LIMBS` and is
/// never needed to answer it. Before the fix every inherent question on `Uint` made every
/// inherent impl's header, that one included, and `lo` and `half` were "not checked" with
/// "an array length names: Nested" (tempo census 7, 169 fns). Expected lines:
/// `inherent by name: lo: clean` and `inherent by name: half: clean`.
#[test]
fn an_inherent_item_is_found_by_name_before_headers() {
    const TEXT: &str = "struct Uint<const L: usize> { limbs: [u64; L] }
impl<const L: usize> Uint<L> {
    const LIMBS: usize = L;
    fn lo(&self) -> u64 { self.limbs[0] }
}
type U4 = Uint<4>;
impl Uint<{ <U4>::LIMBS / 2 }> {
    fn concat(&self) -> u64 { self.limbs[1] }
}
fn lo(u: &Uint<4>) -> u64 { u.lo() }
fn half(u: &Uint<2>) -> u64 { u.concat() }
fn main() {}
";
    let got = check_fns(TEXT, &["lo", "half"]);
    all_clean("inherent by name", &got);
}

/// A trait impl whose associated type's array length evaluates a constant that proves the
/// impl's own trait (either as a supertrait, like `itoa`'s `trait Integer: private::Sealed`
/// where `impl private::Sealed for i128` defines `type Buffer = [MaybeUninit<u8>;
/// Self::MAX_STR_LEN]`, or on the same trait, like `impl ArrayLen for u32 { const N: usize =
/// 4; type Buf = [u8; Self::N]; }`). Proving `T: Trait` needs only the impl's header and
/// where-clauses, never its associated type values, so `Store::impl_` publishes a provisional
/// trait-impl header while the associated type's constant evaluates, then completes and
/// replaces it with the final `ImplHead`.
///
/// Also tests: exact evaluated lengths (`40` for `i128`, `20` for `u64`, `4` for `ArrayLen`),
/// a named constant shared between an impl's associated type and a body (no `ReadsDiffer`),
/// missing supertrait (`Bad::MAX_STR_LEN` still reports E0277), and a real self-referential
/// associated type cycle (`type A = [u8; size_of::<<u8 as Cyc>::A>()]`, still refused).
#[test]
fn trait_const_array_len_with_supertrait_assoc_array() {
    const TEXT: &str = "use core::mem::{MaybeUninit, size_of};
pub struct Buffer {
    bytes: [MaybeUninit<u8>; i128::MAX_STR_LEN],
}
impl Buffer {
    pub fn new() -> Buffer {
        let bytes = [MaybeUninit::<u8>::uninit(); i128::MAX_STR_LEN];
        Buffer { bytes }
    }
    pub fn format<I: Integer>(&mut self, i: I) -> usize {
        let buf_ptr = self.bytes.as_mut_ptr().cast::<I::Buffer>();
        i.write(unsafe { &mut *buf_ptr })
    }
}
pub trait Integer: private::Sealed {
    const MAX_STR_LEN: usize;
}
mod private {
    pub trait Sealed: Copy {
        type Buffer: 'static;
        fn write(self, buf: &mut Self::Buffer) -> usize;
    }
}
impl Integer for i128 {
    const MAX_STR_LEN: usize = i128::MAX.ilog10() as usize + 2;
}
impl private::Sealed for i128 {
    type Buffer = [MaybeUninit<u8>; Self::MAX_STR_LEN];
    fn write(self, buf: &mut Self::Buffer) -> usize { buf.len() }
}
impl Integer for u64 {
    const MAX_STR_LEN: usize = u64::MAX.ilog10() as usize + 1;
}
impl private::Sealed for u64 {
    type Buffer = [MaybeUninit<u8>; Self::MAX_STR_LEN];
    fn write(self, buf: &mut Self::Buffer) -> usize { buf.len() }
}
trait ArrayLen {
    const N: usize;
    type Buf;
}
impl ArrayLen for u32 {
    const N: usize = 4;
    type Buf = [u8; Self::N];
}
const SHARED_LEN: usize = i128::MAX_STR_LEN;
trait SharedTrait {
    type Arr;
}
impl SharedTrait for i128 {
    type Arr = [u8; SHARED_LEN];
}
#[derive(Copy, Clone)]
struct MissingSuper;
impl Integer for MissingSuper {
    const MAX_STR_LEN: usize = 4;
}
trait Cyc {
    type A;
}
impl Cyc for u8 {
    type A = [u8; size_of::<<u8 as Cyc>::A>()];
}
fn use_buf() -> usize {
    let mut b = Buffer::new();
    b.format(42u64)
}
fn i128_ok(x: <i128 as private::Sealed>::Buffer) -> [MaybeUninit<u8>; 40] { x }
fn i128_bad(x: <i128 as private::Sealed>::Buffer) -> [MaybeUninit<u8>; 41] { x }
fn u64_ok(x: <u64 as private::Sealed>::Buffer) -> [MaybeUninit<u8>; 20] { x }
fn u64_bad(x: <u64 as private::Sealed>::Buffer) -> [MaybeUninit<u8>; 21] { x }
fn same_trait_ok(x: <u32 as ArrayLen>::Buf) -> [u8; 4] { x }
fn same_trait_bad(x: <u32 as ArrayLen>::Buf) -> [u8; 5] { x }
fn named_const_ok(x: <i128 as SharedTrait>::Arr) -> ([u8; 40], usize) { (x, SHARED_LEN) }
fn missing_supertrait_bad() -> usize { MissingSuper::MAX_STR_LEN }
fn real_cycle_bad(x: <u8 as Cyc>::A) -> <u8 as Cyc>::A { x }
fn main() {}
";
    let got = check_fns(
        TEXT,
        &[
            "use_buf",
            "i128_ok",
            "i128_bad",
            "u64_ok",
            "u64_bad",
            "same_trait_ok",
            "same_trait_bad",
            "named_const_ok",
            "missing_supertrait_bad",
            "real_cycle_bad",
        ],
    );
    let outcome = |n: &str| got.iter().find(|(m, _)| m == n).map(|(_, o)| o.as_str()).unwrap_or("");
    for ok in ["use_buf", "i128_ok", "u64_ok", "same_trait_ok", "named_const_ok"] {
        assert_eq!(outcome(ok), "clean", "{ok}: {got:?}");
    }
    for mismatch in ["i128_bad", "u64_bad", "same_trait_bad"] {
        assert!(outcome(mismatch).contains("E0308"), "{mismatch}: {got:?}");
    }
    assert!(outcome("missing_supertrait_bad").contains("E0277"), "missing_supertrait_bad: {got:?}");
    assert_ne!(outcome("real_cycle_bad"), "clean", "real_cycle_bad: {got:?}");
}
