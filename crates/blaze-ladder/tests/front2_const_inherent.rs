//! Inherent constant leaves in heads (front2/const_inherent.rs): an array length or a const
//! argument written as a path to an inherent impl's associated constant, `T::NAME`,
//! `Self::NAME`, `<T>::NAME`, is the value of that constant item, evaluated by its own key.
//!
//! The Reference, "Array types": "The size is a constant expression of type `usize`";
//! "Constant evaluation", "Const context": "Array type length expressions" are const contexts;
//! "Paths", "`Self`": "In an implementation, it refers to the type being implemented" (and in
//! a struct's definition, to the struct); "Associated Items", "Associated Constants": an
//! inherent impl's constant is "a constant associated with a type". The forms are tempo's
//! census 14 refusals: http's `struct InlineExtension([u8; InlineExtension::MAX], u8)`,
//! const-oid's `bytes: [u8; Self::MAX_SIZE]`, zone_p2p's `fn encode(&self) -> [u8;
//! Self::ENCODED_LEN]`, and a leaf inside arithmetic (`<U32>::USIZE * 8`'s shape).
//!
//! CONTROL: with the substitution in types.rs `Builder::const_term` taken out (the block that
//! calls `const_inherent::substitute`), `ext` and `oid` print "not checked: .." naming
//! `ConstPath("InlineExtension::MAX: constant ..: a body it needs: checking ..: .. an array
//! length names: Nested ..")` (the anonymous constant's check reads the struct's row, whose
//! field holds this very length), and the test fails.
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`, as tests/front2_items.rs.

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

/// Each fn of `fns` in the crate `text`, lowered and checked in order over one store: its
/// name and its outcome ("clean", the diagnostics, or the failure as printed). The harness of
/// tests/front2_items.rs.
fn check_fns(text: &str, fns: &[&str]) -> Vec<(String, String)> {
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

/// The four forms each check clean, and each length IS the constant's value: the returned
/// array type is written with the number (`[u8; 15]`), so a wrong value is E0308.
/// Expected lines: "inherent leaves: ext: clean", "inherent leaves: oid: clean",
/// "inherent leaves: tip: clean", "inherent leaves: wide: clean".
#[test]
fn inherent_constant_leaves_in_heads_check_clean() {
    const TEXT: &str = "struct InlineExtension([u8; InlineExtension::MAX], u8);
impl InlineExtension {
    const MAX: usize = 15;
}
struct ObjectIdentifier { length: u8, bytes: [u8; Self::MAX_SIZE] }
impl ObjectIdentifier {
    const MAX_SIZE: usize = 39;
}
struct PeerTip { at: u64 }
impl PeerTip {
    const ENCODED_LEN: usize = 8 + 32 + 8 + 32;
    fn encode(&self) -> [u8; Self::ENCODED_LEN] { [0u8; 80] }
}
struct Wide { bits: [u8; <Wide>::BYTES * 8] }
impl Wide {
    const BYTES: usize = 4;
}
fn ext(e: InlineExtension) -> [u8; 15] { e.0 }
fn oid(o: ObjectIdentifier) -> [u8; 39] { o.bytes }
fn tip(p: &PeerTip) -> [u8; 80] { p.encode() }
fn wide(w: Wide) -> [u8; 32] { w.bits }
fn main() {}
";
    let got = check_fns(TEXT, &["ext", "oid", "tip", "wide"]);
    let lines: Vec<String> = got.iter().map(|(n, o)| format!("inherent leaves: {n}: {o}")).collect();
    for l in &lines {
        println!("{l}");
    }
    assert!(got.iter().all(|(_, o)| o == "clean"), "{}", lines.join("\n"));
}

/// `[u8; Self::N]` with `N = 4` is `[u8; 4]`, never `[u8; 3]`: returning it where `[u8; 3]` is
/// written is a type mismatch (the Reference, "Array types": arrays of different sizes are
/// different types; E0308). A leaf answered with a wrong value, or not compared at all,
/// fails this. Expected line: "inherent mismatch: bad: diags [\"E0308 at ..\"]" (a line
/// holding E0308), and "inherent mismatch: good: clean".
#[test]
fn a_length_of_another_value_is_a_mismatch() {
    const TEXT: &str = "struct S { a: [u8; Self::N] }
impl S {
    const N: usize = 4;
}
fn bad(s: S) -> [u8; 3] { s.a }
fn good(s: S) -> [u8; 4] { s.a }
fn main() {}
";
    let got = check_fns(TEXT, &["bad", "good"]);
    let lines: Vec<String> = got.iter().map(|(n, o)| format!("inherent mismatch: {n}: {o}")).collect();
    for l in &lines {
        println!("{l}");
    }
    let outcome = |n: &str| got.iter().find(|(m, _)| m == n).map(|(_, o)| o.as_str()).unwrap_or("");
    // The check reports a body's diagnostics as its failure ("not checked: .. 1 diagnostics /
    // E0308 ..") or as `diags [..]`: either way the outcome names E0308.
    assert!(outcome("bad").contains("E0308"), "{}", lines.join("\n"));
    assert_eq!(outcome("good"), "clean", "{}", lines.join("\n"));
}

/// An inherent constant of a type that is not an integer, read inside a head's constant
/// expression: time's `type JulianDay = ri32<{ Date::MIN.to_julian_day() }, ..>` (the Reference,
/// "Constant evaluation": a constant expression may read a constant of any type and call a
/// `const fn` on it). Its value is the whole expression's, so `R<{ Date::MIN.day() }>` is `R<7>`
/// and never `R<8>`. Before the fix both heads were refused with `ConstPath("Date::MIN: the
/// inherent constant ..: a path in a head has a value only at `bool`, `char` or an integer
/// type ..")` (tempo census 7, 31 functions). Expected lines: "struct constant in head: low:
/// clean" and "struct constant in head: high: .." holding E0308.
#[test]
fn a_struct_constant_read_in_a_head() {
    const TEXT: &str = "struct Date { v: i32 }
impl Date {
    const MIN: Date = Date { v: 7 };
    const fn day(self) -> i32 { self.v }
}
struct R<const A: i32>;
type Low = R<{ Date::MIN.day() }>;
fn low(r: Low) -> R<7> { r }
fn high(r: Low) -> R<8> { r }
fn main() {}
";
    let got = check_fns(TEXT, &["low", "high"]);
    let lines: Vec<String> = got.iter().map(|(n, o)| format!("struct constant in head: {n}: {o}")).collect();
    for l in &lines {
        println!("{l}");
    }
    let outcome = |n: &str| got.iter().find(|(m, _)| m == n).map(|(_, o)| o.as_str()).unwrap_or("");
    assert_eq!(outcome("low"), "clean", "{}", lines.join("\n"));
    assert!(outcome("high").contains("E0308"), "{}", lines.join("\n"));
}

/// `Self::NAME` in a head of a trait impl for a built-in type, where `NAME` is another trait's
/// constant: itoa's `impl private::Sealed for i128 { type Buffer = [MaybeUninit<u8>;
/// Self::MAX_STR_LEN]; .. }` (the Reference, "Paths", "`Self`": it "refers to the type being
/// implemented", so the path is `u16::MAX_STR_LEN`, found through the trait in scope). The
/// length is 5, so `[u8; 6]` is E0308. Before the fix the anonymous constant was refused with
/// "E0425: `Self` (segment 0) not found in this scope" and every function reaching the impl
/// was left out (tempo census 7, 19 + 1). Expected lines: "self in prim head: five: clean" and
/// "self in prim head: six: .." holding E0308.
#[test]
fn self_of_a_builtin_type_in_an_impl_head() {
    const TEXT: &str = "trait Integer { const MAX_STR_LEN: usize; }
impl Integer for u16 { const MAX_STR_LEN: usize = 5; }
trait Sealed { type Buffer; fn first(b: Self::Buffer) -> Self::Buffer; }
impl Sealed for u16 {
    type Buffer = [u8; Self::MAX_STR_LEN];
    fn first(b: Self::Buffer) -> Self::Buffer { b }
}
fn five(b: [u8; 5]) -> [u8; 5] { <u16 as Sealed>::first(b) }
fn six(b: [u8; 6]) -> [u8; 6] { <u16 as Sealed>::first(b) }
fn main() {}
";
    let got = check_fns(TEXT, &["five", "six"]);
    let lines: Vec<String> = got.iter().map(|(n, o)| format!("self in prim head: {n}: {o}")).collect();
    for l in &lines {
        println!("{l}");
    }
    let outcome = |n: &str| got.iter().find(|(m, _)| m == n).map(|(_, o)| o.as_str()).unwrap_or("");
    assert_eq!(outcome("five"), "clean", "{}", lines.join("\n"));
    assert!(outcome("six").contains("E0308"), "{}", lines.join("\n"));
}

/// A constant whose type names a sibling constant of its own impl: turso's divan,
/// `impl KnownCounterKind { pub const COUNT: usize = 4; pub const ALL: [Self; Self::COUNT] =
/// [..]; }` with `struct CounterSet { counts: [Option<u64>; KnownCounterKind::COUNT] }` (the
/// Reference, "Associated Items": the path to an associated item is the type's path and the
/// item's name, so finding `COUNT` needs the impls' names, never `ALL`'s type; "Constant
/// evaluation" refuses only a constant that needs itself). Finding `COUNT` makes the impl's
/// header, and `ALL`'s row waits for `COUNT`'s value (heads.rs `ConstWait`), then is made with
/// it, so `ALL` is `[Kind; 4]` too. The negative twins: `[_; 5]` is E0308 for both.
/// Expected lines: "sibling constant: four: clean", "sibling constant: five: .." holding E0308,
/// "sibling constant: all4: clean", "sibling constant: all5: .." holding E0308.
///
/// CONTROL: with the wait in heads.rs `Store::impl_` taken out (its `Err(why) => match
/// self.waits_on_running(..)` arm made `Err(why) => return Err(why)`), `four` prints "not
/// checked: .. the inherent impl .. is refused, and the constant may be declared there: ..
/// Self::COUNT: the inherent constant is needed while it is being evaluated .." (turso c1851p,
/// 137 notes and 268 heads), and the test fails.
#[test]
fn a_constant_typed_by_a_sibling_constant() {
    const TEXT: &str = "enum Kind { A, B, C, D }
impl Kind {
    const COUNT: usize = 4;
    const ALL: [Self; Self::COUNT] = [Self::A, Self::B, Self::C, Self::D];
}
struct Set { counts: [Option<u64>; Kind::COUNT] }
fn four(s: Set) -> [Option<u64>; 4] { s.counts }
fn five(s: Set) -> [Option<u64>; 5] { s.counts }
fn all4() -> [Kind; 4] { Kind::ALL }
fn all5() -> [Kind; 5] { Kind::ALL }
fn main() {}
";
    let got = check_fns(TEXT, &["four", "five", "all4", "all5"]);
    let lines: Vec<String> = got.iter().map(|(n, o)| format!("sibling constant: {n}: {o}")).collect();
    for l in &lines {
        println!("{l}");
    }
    let outcome = |n: &str| got.iter().find(|(m, _)| m == n).map(|(_, o)| o.as_str()).unwrap_or("");
    assert_eq!(outcome("four"), "clean", "{}", lines.join("\n"));
    assert!(outcome("five").contains("E0308"), "{}", lines.join("\n"));
    assert_eq!(outcome("all4"), "clean", "{}", lines.join("\n"));
    assert!(outcome("all5").contains("E0308"), "{}", lines.join("\n"));
}

/// The twin rustc refuses (E0391, a cycle): `B`'s value reads `A`, whose type needs `B`. The
/// wait above must not make this a value: `t` is not clean, and its refusal names the cycle's
/// constant, never a panic or a silent pass. Expected line: "constant cycle: t: not checked:
/// .." (anything but "clean").
#[test]
fn a_constant_whose_type_needs_itself_is_refused() {
    const TEXT: &str = "struct S;
impl S {
    const A: [u8; Self::B] = [0; 2];
    const B: usize = Self::A.len();
}
struct T { x: [u8; S::B] }
fn t(v: T) -> T { v }
fn main() {}
";
    let got = check_fns(TEXT, &["t"]);
    let lines: Vec<String> = got.iter().map(|(n, o)| format!("constant cycle: {n}: {o}")).collect();
    for l in &lines {
        println!("{l}");
    }
    let outcome = |n: &str| got.iter().find(|(m, _)| m == n).map(|(_, o)| o.as_str()).unwrap_or("");
    assert_ne!(outcome("t"), "clean", "{}", lines.join("\n"));
}
