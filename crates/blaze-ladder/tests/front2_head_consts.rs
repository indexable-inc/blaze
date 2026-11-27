//! Constants in HEADS through front2 with the real core: the classes of the tempo build census
//! r16 (port27/tempo-run/build-census-r16.txt) where a constant written in a head was refused.
//!
//! (1) An anonymous constant of a head (front2/anon.rs: `[u8; 1 << 2]`, `Buf<{ limbs(64) }>`,
//! `[u8; size_of::<usize>()]`, `[T; (4 - 1) - 1]`) is a member of its MODULE whose text
//! `const _: TY = EXPR;` front2 makes. Its body's lowering asked that module's marks
//! (front2/body.rs `lower_member_into` -> item_site.rs `member_marks`), and a module has no
//! place over the crates' records ("item .. has a text and no place, so its head's paths have
//! no marks"), nor, written by a macro, a text the made constant is a slice of. Every such
//! head was refused, and every fn whose signature or impl it is (census r16: `nlimbs( 64 )` 81,
//! `nlimbs! ( N )` 62, `( mem:: size_of::< isize> ( ) )` 55, `size_of::< Self> ( )` 119 under
//! `check gap: FnHead`, `N << N` 26, `(((32 - 1) - 1) ..)` 23 + 6, `check gap: BodyConst` 26).
//! The fixtures here write each shape inside a module a `macro_rules!` writes (crypto-bigint's
//! `impl_uint_aliases!`, zerocopy's `define_type!` and core's `uint_impl!` do the same), so
//! the module has an expansion's place: the made text is no slice of that expansion.
//!
//! (2) A const argument of a trait path in an impl header that a macro wrote: the parameter's
//! type was read from the trait resolved again at the impl's MODULE (front2/types.rs
//! `const_want`), not where the path's first token's mark says. alloy-rlp's
//! `impl_max_encoded_len!` writes `unsafe impl $crate::MaxEncodedLen<{ $len }> for $t {}` and
//! alloy-primitives calls it with `$len` = `{ 20 + $crate::private::alloy_rlp::length_of_length(20) }`:
//! the impl was "a candidate filed without its trait or self type (unread as written)" with
//! `NotFound` (census r16 `check gap: UnreadImpl`, 41 of 53). The fixture's def-site `macro`
//! names a trait of its own module from another module, and its constant is a block calling
//! another crate's `const fn` through a re-export path (`crate::private::mem::size_of`).
//!
//! Every fixture has a fn that must check clean AND one with the wrong length that must be a
//! diagnostic of the program: a fix that made a constant a hole passes the second clean and
//! fails here. Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`, as
//! tests/front2_const_open.rs does. NOT RUN by the writer (a write-only lane, uncompiled).

#![feature(allocator_api)]

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::body::{BodyError, lower_fn};
use blaze_ladder::front2::check::{BuildOrder, CheckFail, Records, check_body, eval_len};
use blaze_ladder::front2::consts::Consts;
use blaze_ladder::front2::heads::{HeadError, Store};
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::front2::types::Why;
use blaze_ladder::refusal::SourceFile;
use blaze_lower::scoped::Prepares;
use blaze_own::Alloc;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};

const EDITION: &str = "blaze-2026.11";

/// The most constants one fn's lowering may need evaluated before it converts.
const MOST_LENS: u32 = 8;

/// Each fn of `fns` in the crate `text`, lowered and checked in order over one store, as
/// tests/front2_const_open.rs `check_fns` does: its name and its outcome ("clean", the
/// diagnostics, or the failure as printed). The test is the entry: the allocator it names
/// here is handed down to every call.
fn check_fns(text: &str, fns: &[&str]) -> Vec<(String, String)> {
    let alloc = Alloc::global();
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
    let mut front = Front2::new(&names, &frame, &sources, alloc).unwrap_or_else(|e| panic!("front2: {e}"));
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut store = Store::new(front.index.lang("sized"), front.frame, alloc);
    let (mut buckets, mut consts) = (None, Consts::default());
    let mut got = Vec::new();
    for &n in fns {
        let found = names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: &[n] }, alloc).unwrap_or_else(|e| panic!("`{n}`: {e}"));
        let k = found.whole(1).unwrap_or_else(|| panic!("`{n}` not named")).key;
        let mut lens = 0;
        let outcome = loop {
            match lower_fn(&names, prepared, &mut front.keys, &mut store, k) {
                Err(BodyError::Convert { why: Some(Why::NeedConst(c, _)), .. }) | Err(BodyError::Head { error: HeadError::Item(_, Why::NeedConst(c, _)), .. }) if lens < MOST_LENS => {
                    lens += 1;
                    let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: Some((&mut consts, prepared)) };
                    if let Err(e) = eval_len(&mut records, c) {
                        break format!("not lowered: the constant {c:x} its body names: {e}");
                    }
                }
                Err(e) => break format!("not lowered: {e}"),
                Ok(body) => {
                    let view = body.lowered.view();
                    let spell = Spelling::of(&view).unwrap_or_else(|e| panic!("spelling `{n}`: {e:?}"));
                    let scope = body.scope(&spell);
                    let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: Some((&mut consts, prepared)) };
                    break match check_body(&mut records, &body, &scope) {
                        Ok(done) if done.diags.is_empty() => "clean".to_string(),
                        Ok(done) => {
                            let d: Vec<String> = done.diags.iter().map(|d| format!("{} at {:?}: `{}`", d.code.code(), d.at, text.get(d.at.start as usize..d.at.end as usize).unwrap_or("?"))).collect();
                            format!("diags {d:?}")
                        }
                        Err(CheckFail::Diags { diags, .. }) => format!("diags {diags:?}"),
                        Err(e) => format!("not checked: {e}"),
                    };
                }
            }
        };
        got.push((n.to_string(), outcome));
    }
    got
}

/// Prints each outcome as `<what>: <fn>: <outcome>` and asserts, per fn, `clean` (`true`) or
/// a diagnostic of the program (`false`); a gap or a refusal ("not checked", "not lowered")
/// fails both. No outcome may name the lost marks (the refusal of class (1)).
fn expect(what: &str, got: &[(String, String)], clean: &[bool]) {
    let lines: Vec<String> = got.iter().map(|(n, o)| format!("{what}: {n}: {o}")).collect();
    for l in &lines {
        println!("{l}");
    }
    assert_eq!(got.len(), clean.len(), "{}", lines.join("\n"));
    for ((n, o), &want) in got.iter().zip(clean) {
        assert!(!o.contains("has a text and no place") && !o.contains("is not a slice of the expansion"), "`{n}`: an anonymous constant asked its module's marks\n{}", lines.join("\n"));
        if want {
            assert_eq!(o, "clean", "`{n}` must check clean\n{}", lines.join("\n"));
        } else {
            assert!(o.starts_with("diags "), "`{n}` must be an error of the program, not clean and not a gap\n{}", lines.join("\n"));
        }
    }
}

/// Class (1), crypto-bigint's `pub type U64 = Uint<{nlimbs!(64)}>;` (0.5.5, a macro that
/// expands to `64 / $crate::Limb::BITS`) and `Uint<{ nlimbs(64) }>` (0.7, a `const fn`): a
/// `const fn` call, and a macro call that expands to one, as the const argument of an alias.
/// `limbs(64)` is 2. Expected lines: "alias const fn: fn_two: clean", "alias const fn:
/// fn_three: diags [..]" (a `Buf<3>` where `Buf<2>` is expected), "alias const fn: mac_two:
/// clean", "alias const fn: mac_three: diags [..]".
#[test]
fn a_const_fn_in_an_aliass_argument_has_its_value() {
    const TEXT: &str = "macro_rules! limbs_of { ($bits:expr) => { $crate::m::limbs($bits) } }
macro_rules! mk {
    () => {
        pub mod m {
            pub const fn limbs(bits: usize) -> usize { bits / 32 }
            pub struct Buf<const N: usize>(pub [u8; N]);
            pub type ByFn = Buf<{ limbs(64) }>;
            pub type ByMacro = Buf<{ limbs_of!(64) }>;
            pub fn by_fn(b: ByFn) -> u8 { b.0[1] }
            pub fn by_macro(b: ByMacro) -> u8 { b.0[1] }
        }
    };
}
mk!();
fn fn_two(b: m::Buf<2>) -> u8 { m::by_fn(b) }
fn fn_three(b: m::Buf<3>) -> u8 { m::by_fn(b) }
fn mac_two(b: m::Buf<2>) -> u8 { m::by_macro(b) }
fn mac_three(b: m::Buf<3>) -> u8 { m::by_macro(b) }
fn main() {}
";
    let got = check_fns(TEXT, &["fn_two", "fn_three", "mac_two", "mac_three"]);
    expect("alias const fn", &got, &[true, false, true, false]);
}

/// Class (1), zerocopy's `impl<O> PartialEq<Usize<O>> for [u8; (mem::size_of::<usize>())]`
/// (src/byteorder.rs, written by `define_type!`) and core's `pub const fn from_le_bytes(bytes:
/// [u8; size_of::<Self>()]) -> Self` (written by `uint_impl!` inside `impl u32`): `size_of` as
/// an array length in an impl header and in a signature, in a module a macro wrote. `usize` is
/// 8 bytes on every host blaze targets. Expected lines: "size_of head: header_eight: clean",
/// "size_of head: header_seven: diags [..]", "size_of head: sig_two: clean", "size_of head:
/// sig_three: diags [..]" (a `[u8; 3]` where `W`'s `[u8; size_of::<Self>()]`, 2, is expected).
#[test]
fn size_of_in_a_header_and_a_signature_has_its_value() {
    const TEXT: &str = "macro_rules! mk {
    () => {
        pub mod m {
            use core::mem;
            pub struct B;
            impl From<[u8; (mem::size_of::<usize>())]> for B {
                fn from(_: [u8; (mem::size_of::<usize>())]) -> B { B }
            }
            pub struct W(pub u16);
            impl W {
                pub fn pick(bytes: [u8; mem::size_of::<Self>()]) -> u8 { bytes[1] }
            }
        }
    };
}
mk!();
fn header_eight(a: [u8; 8]) -> m::B { m::B::from(a) }
fn header_seven(a: [u8; 7]) -> m::B { m::B::from(a) }
fn sig_two(b: [u8; 2]) -> u8 { m::W::pick(b) }
fn sig_three(b: [u8; 3]) -> u8 { m::W::pick(b) }
fn main() {}
";
    let got = check_fns(TEXT, &["header_eight", "header_seven", "sig_two", "sig_three"]);
    expect("size_of head", &got, &[true, false, true, false]);
}

/// Class (1), core's `impl<T> Default for [T; ((32 - 1) - 1) ..]` (library/core/src/array/
/// mod.rs, `array_impl_default!`), rand's `Distribution<[T; (32 - 1) - 1 ..]>`, and core's
/// `AlignmentEnum` discriminants `_Align1Shl0 = 1 << 0`: nested arithmetic and a shift as an
/// array length in an impl header. `((4 - 1) - 1)` is 2 and `1 << 2` is 4. Expected lines:
/// "nested length: nested_two: clean", "nested length: nested_three: diags [..]" (no impl of
/// `D` for `[u8; 3]`), "nested length: shl_four: clean", "nested length: shl_five: diags [..]".
#[test]
fn nested_arithmetic_in_a_header_has_its_value() {
    const TEXT: &str = "macro_rules! mk {
    () => {
        pub mod m {
            pub trait D { fn d() -> u8; }
            impl D for [u8; ((4 - 1) - 1)] { fn d() -> u8 { 2 } }
            pub fn shl(a: [u8; 1 << 2]) -> u8 { a[3] }
        }
    };
}
mk!();
fn nested_two() -> u8 { <[u8; 2] as m::D>::d() }
fn nested_three() -> u8 { <[u8; 3] as m::D>::d() }
fn shl_four(a: [u8; 4]) -> u8 { m::shl(a) }
fn shl_five(a: [u8; 5]) -> u8 { m::shl(a) }
fn main() {}
";
    let got = check_fns(TEXT, &["nested_two", "nested_three", "shl_four", "shl_five"]);
    expect("nested length", &got, &[true, false, true, false]);
}

/// Class (1) in a body (`check gap: BodyConst`, `RepeatLen: AnonInBody` 20, `Arg: AnonInBody`
/// 6): core's `repeat_u8`, "usize::from_ne_bytes([x; size_of::<usize>()])", in a module a macro
/// wrote (`impl usize` is in core::num, its fns written by `uint_impl!`). The repeat length and
/// the turbofish's const argument are anonymous constants filed under that module. Expected
/// lines: "body anon: rep_eight: clean", "body anon: rep_seven: diags [..]", "body anon:
/// arg_six: clean", "body anon: arg_seven: diags [..]".
#[test]
fn an_anonymous_constant_in_a_macro_written_body_has_its_value() {
    const TEXT: &str = "macro_rules! mk {
    () => {
        pub mod m {
            pub const BASE: usize = 7;
            pub fn take<const N: usize>(a: [u8; N]) -> usize { a.len() }
            pub fn rep_eight(x: u8) -> [u8; 8] { [x; core::mem::size_of::<usize>()] }
            pub fn rep_seven(x: u8) -> [u8; 7] { [x; core::mem::size_of::<usize>()] }
            pub fn arg_six() -> usize { take::<{ BASE - 1 }>([0u8; 6]) }
            pub fn arg_seven() -> usize { take::<{ BASE - 1 }>([0u8; 7]) }
        }
    };
}
mk!();
use m::{arg_seven, arg_six, rep_eight, rep_seven};
fn main() {}
";
    let got = check_fns(TEXT, &["rep_eight", "rep_seven", "arg_six", "arg_seven"]);
    expect("body anon", &got, &[true, false, true, false]);
}

/// Class (2), alloy-primitives' `unsafe impl $crate::MaxEncodedLen<{ { 20 +
/// $crate::private::alloy_rlp::length_of_length(20) } }> for Address {}`: a const block argument
/// of a trait path a macro wrote, calling another crate's `const fn` through a re-export path.
/// The def-site `macro` names `Tr` of its own module `a` (the Reference, "Macros By Example",
/// Hygiene; a `macro` item is def-site), called in `b`, where `Tr` names nothing: the trait
/// resolves by its token's mark, and the const parameter's type must be read from the SAME
/// trait. `20 + size_of::<u16>()` is 22. Expected lines: "block arg: twenty_two: clean",
/// "block arg: twenty_three: diags [..]" (`S: Tr<23>` does not hold).
#[test]
fn a_const_block_argument_of_a_macro_written_trait_path_has_its_value() {
    const TEXT: &str = "#![feature(decl_macro)]
mod private { pub use core::mem; }
mod a {
    pub trait Tr<const N: usize> {}
    pub struct S;
    pub macro mk() { impl Tr<{ { 20 + crate::private::mem::size_of::<u16>() } }> for S {} }
}
mod b { crate::a::mk!(); }
fn need<T: a::Tr<22>>() {}
fn need23<T: a::Tr<23>>() {}
fn twenty_two() { need::<a::S>() }
fn twenty_three() { need23::<a::S>() }
fn main() {}
";
    let got = check_fns(TEXT, &["twenty_two", "twenty_three"]);
    for (n, o) in &got {
        assert!(!o.contains("unread as written"), "`{n}`: the impl's header must read: {o}");
    }
    expect("block arg", &got, &[true, false]);
}

/// Class (3), tempo census r20's largest (`Nested`, 1,368 + 34 fns of p256): primefield's
/// `monty_field_params!` writes, for the caller's `$name`,
/// `impl $crate::MontyFieldParams<..> for $name { type ByteSize = U<{ <$name as
/// $crate::bigint::modular::ConstMontyParams<..>>::PARAMS.modulus().as_ref().bits().div_ceil(8)
/// as usize }>; .. }` beside the `ConstMontyParams` impl. The anonymous constant K of
/// `ByteSize` reads `<P as ConstParams>::PARAMS`, so its check asks `P: ConstParams`. Filed with
/// its trait path read at the impl's module (not where the macro's tokens' marks say), the
/// `FieldParams` impl was unread: a candidate of EVERY trait question at `P`, so that question
/// made its whole head, `ByteSize`'s value included, which needs K while K is evaluated. The
/// def-site `macro` here names the traits of its impl headers from its own module `lib` while
/// called in `p`, as `$crate::` does across crates. The constant is written as tempo's
/// primefield writes it (`FieldParams :: PARAMS. modulus( ) ..`: a type-relative path, its trait
/// in scope where the macro is called), because an anonymous constant's body is read at its
/// module with no marks (owed: a def-site name in it is the open case). 256 bits are 32 bytes. Expected lines: "self length: thirty_two: clean",
/// "self length: thirty_three: diags [..]" (`ByteSize` is `U<32>`, not `U<33>`).
#[test]
fn an_associated_types_constant_reading_its_own_types_trait_has_its_value() {
    const TEXT: &str = "#![feature(decl_macro)]
mod lib {
    #[derive(Clone, Copy)]
    pub struct Params { pub bits: usize }
    impl Params { pub const fn bits(&self) -> usize { self.bits } }
    pub trait ConstParams { const PARAMS: Params; }
    pub trait FieldParams { type ByteSize; }
    pub struct U<const N: usize>;
    pub macro field_params($name:ident) {
        pub struct $name;
        impl ConstParams for $name { const PARAMS: Params = Params { bits: 256 }; }
        impl FieldParams for $name {
            type ByteSize = U<{ $name::PARAMS.bits().div_ceil(8) as usize }>;
        }
    }
}
mod p { use crate::lib::ConstParams; crate::lib::field_params!(P); }
fn need<T: lib::FieldParams<ByteSize = lib::U<32>>>() {}
fn need33<T: lib::FieldParams<ByteSize = lib::U<33>>>() {}
fn thirty_two() { need::<p::P>() }
fn thirty_three() { need33::<p::P>() }
fn main() {}
";
    let got = check_fns(TEXT, &["thirty_two", "thirty_three"]);
    for (n, o) in &got {
        assert!(!o.contains("Nested"), "`{n}`: the constant needed itself: {o}");
    }
    expect("self length", &got, &[true, false]);
}

/// A multi-statement block const generic argument (`unsafe impl MaxEncodedLen<{ let _sz = ...; _sz + f(_sz) }> for S`,
/// as expanded by `alloy_rlp_derive::RlpMaxEncodedLen` in `reth-ecies`): `const_term` strips the outer `{ ... }`,
/// so `anon::file_asked` must wrap the synthetic declaration in `{ ... }` lest `let _sz = ...` parse as a
/// Rust 2024 `Expr::Let` (`bool`) instead of a statement inside a block.
#[test]
fn a_multi_statement_block_const_arg_preserves_its_braces() {
    const TEXT: &str = "
const fn extra(n: usize) -> usize { n + 2 }
trait MaxLen<const N: usize> {}
trait AssocLen { const LEN: usize; }
struct A;
struct B;
impl AssocLen for A { const LEN: usize = 3; }
impl AssocLen for B { const LEN: usize = 4; }
struct S;
impl MaxLen<{ let _sz = 0usize + <A as AssocLen>::LEN + <B as AssocLen>::LEN; _sz + extra(_sz) }> for S {}
fn need16<T: MaxLen<16>>() {}
fn need17<T: MaxLen<17>>() {}
fn sixteen() { need16::<S>() }
fn seventeen() { need17::<S>() }
fn main() {}
";
    let got = check_fns(TEXT, &["sixteen", "seventeen"]);
    expect("block stmt arg", &got, &[true, false]);
}

/// Positive 1: a hand-written `impl` with a head-level anonymous const block in its `where` clause
/// declaring both an `enum` and a trait `impl` for it (both without `Self` via `const_value_bits`
/// and with ground `Self` via `self_value`, matching `zerocopy::IntoBytes`). The enclosing `impl`'s
/// `blaze-scope` block module binds `Tag` and registers `impl Marker for Tag`, so the synthetic
/// anonymous constant maps its blocks back to that module, evaluates cleanly, and produces the
/// true size (not a hole).
#[test]
fn a_head_anon_const_block_with_enum_and_impl_maps_to_enclosing_impl_block_module() {
    const TEXT: &str = "
trait Marker {}
trait PaddingFree<T, const N: usize> {}
trait IntoBytes {}
trait CheckSize<const N: usize> {}

#[repr(u8)]
enum AddressSpace {
    SystemMemory = 0,
    SystemIo = 1,
}

#[repr(u16)]
enum WideSpace {
    SystemMemory = 0,
    SystemIo = 1,
}

impl<T> PaddingFree<T, 1> for () {}
impl CheckSize<1> for () {}

impl IntoBytes for AddressSpace
where
    (): PaddingFree<
        Self,
        {
            #[repr(u8)]
            pub enum Tag {
                A = 0,
                B = 1,
            }
            impl Marker for Tag {}
            const fn require_marker<T: Marker>() -> usize {
                core::mem::size_of::<T>()
            }
            let _ = core::mem::size_of::<Self>();
            require_marker::<Tag>()
        },
    >,
{
}

// Same shape, but `Self` has size 2 while `Tag` has size 1 -> evaluates to 2, so `PaddingFree<Self, 1>` does NOT hold.
impl IntoBytes for WideSpace
where
    (): PaddingFree<
        Self,
        {
            #[repr(u8)]
            pub enum Tag {
                A = 0,
                B = 1,
            }
            impl Marker for Tag {}
            core::mem::size_of::<Self>()
        },
    >,
{
}

// Without `Self` (`Builder::const_value_bits` path): two distinct impls in the same module with
// identical const-block text must not collide on block module keys.
struct S1;
struct S2;
trait Ok1 {}
trait Ok2 {}
impl Ok1 for S1
where
    (): CheckSize<{
        #[repr(u8)]
        pub enum LocalTag {
            X = 0,
        }
        impl Marker for LocalTag {}
        core::mem::size_of::<LocalTag>()
    }>,
{
}
impl Ok2 for S2
where
    (): CheckSize<{
        #[repr(u8)]
        pub enum LocalTag {
            X = 0,
        }
        impl Marker for LocalTag {}
        core::mem::size_of::<LocalTag>()
    }>,
{
}

fn need_into_bytes<T: IntoBytes>() {}
fn need_ok1<T: Ok1>() {}
fn need_ok2<T: Ok2>() {}

fn addr_ok() { need_into_bytes::<AddressSpace>() }
fn wide_fails() { need_into_bytes::<WideSpace>() }
fn s1_ok() { need_ok1::<S1>() }
fn s2_ok() { need_ok2::<S2>() }
fn main() {}
";
    let got = check_fns(TEXT, &["addr_ok", "wide_fails", "s1_ok", "s2_ok"]);
    expect("head anon block module", &got, &[true, false, true, true]);
}

/// Positive 2: `zerocopy::IntoBytes` macro-expansion shape: a macro expands to
/// `const _: () = { unsafe impl IntoBytes for AddressSpace where (): PaddingFree<Self, {
///     #[repr(u8)] #[derive(Copy, Clone)] pub enum ___ZerocopyTag { ... }
///     unsafe impl Immutable for ___ZerocopyTag {}
///     enum_padding!(Self, ___ZerocopyTag)
/// }> {} };`.
/// Verifies block module keys match `blaze-scope` for macro-expanded impls nested inside `const _`.
#[test]
fn a_macro_expanded_impl_with_where_const_block_enum_evaluates_cleanly() {
    const TEXT: &str = "
pub trait Immutable {}
pub trait PaddingFree<T, const PADDING: usize> {}
pub trait IntoBytes {}

impl<T> PaddingFree<T, 0> for () {}

macro_rules! enum_padding {
    ($t:ty, $disc:ty) => {{
        const _: [(); 1] = [(); 1];
        core::mem::size_of::<$t>() - core::mem::size_of::<$disc>()
    }};
}

macro_rules! derive_into_bytes_enum {
    ($name:ident, $tag_repr:ident, $($v:ident),+ $(,)?) => {
        const _: () = {
            unsafe impl IntoBytes for $name
            where
                (): PaddingFree<
                    Self,
                    {
                        #[repr($tag_repr)]
                        #[derive(Copy, Clone)]
                        pub enum ___ZerocopyTag {
                            $($v,)+
                        }
                        unsafe impl Immutable for ___ZerocopyTag {}
                        const fn check_imm<T: Immutable>() -> usize { 0 }
                        check_imm::<___ZerocopyTag>() + enum_padding!(Self, ___ZerocopyTag)
                    },
                >,
            {
            }
        };
    };
}

#[repr(u8)]
pub enum AddressSpace {
    SystemMemory = 0,
    SystemIo = 1,
}
derive_into_bytes_enum!(AddressSpace, u8, SystemMemory, SystemIo);

// Mismatched tag repr (`u8` tag for `u16` enum) => padding = 2 - 1 = 1 != 0 => IntoBytes does not hold.
#[repr(u16)]
pub enum PaddedEnum {
    A = 0,
    B = 1,
}
derive_into_bytes_enum!(PaddedEnum, u8, A, B);

fn need_into_bytes<T: IntoBytes>() {}
fn addr_ok() { need_into_bytes::<AddressSpace>() }
fn padded_fails() { need_into_bytes::<PaddedEnum>() }
fn main() {}
";
    let got = check_fns(TEXT, &["addr_ok", "padded_fails"]);
    expect("macro expanded head anon block", &got, &[true, false]);
}

/// Negative control: an enum declared inside a block that a body-level macro expansion wrote in an
/// expression position (outside any filed `blaze-scope` block module, here an expression call at a
/// `const` initializer's top level) must still be refused explicitly with `LocalKind::Enum.unplaced()`,
/// never silently dropped or mis-keyed.
#[test]
fn an_unplaced_enum_in_an_expression_macro_expansion_stays_refused() {
    const TEXT: &str = "
macro_rules! make_expr_enum {
    () => {
        {
            enum Unplaced { A }
            core::mem::size_of::<Unplaced>()
        }
    };
}
const BAD: usize = make_expr_enum!();
fn use_bad() -> [u8; BAD] { [0u8; BAD] }
fn main() {}
";
    let got = check_fns(TEXT, &["use_bad"]);
    assert_eq!(got.len(), 1);
    let (name, outcome) = &got[0];
    assert!(
        outcome.contains("an enum in a block that a macro call wrote or that is in a local item's body"),
        "`{name}` must refuse the genuinely unplaced macro enum explicitly, got: {outcome}"
    );
}

/// A multi-line head-level anonymous constant calling `line!()` on two different lines and
/// `column!()` at two different columns (including inside a local `const` declared in the same
/// block) must subtract the synthetic declaration prefix rather than fixing every call at the
/// expression's start, so each call reports its exact 1-based source line and column.
#[test]
fn a_multiline_head_anon_const_preserves_distinct_line_and_column_offsets() {
    // Line 1: empty (after opening quote newline)
    // Line 2: trait Check<const N: u32> {}
    // Line 3: trait LocalCheck<const N: u32> {}
    // Line 4: struct S;
    // Line 5: impl Check<{
    // Line 6:     let l1 = line!();            -> line 6, col 14
    // Line 7:     let l2 = line!();            -> line 7, col 14
    // Line 8:     let c1 = column!();          -> line 8, col 14
    // Line 9:         let c2 = column!();      -> line 9, col 18
    // Line 10:    l1 * 1_000_000 + l2 * 10_000 + c1 * 100 + c2
    // Line 11: }> for S {}
    // Exact: 6_071_418; if fixed at expression start (line 6 col 5): 6_060_505.
    // Line 12: impl LocalCheck<{
    // Line 13:     const A: u32 = line!();     -> line 13, col 20
    // Line 14:         const B: u32 = column!(); -> line 14, col 24
    // Line 15:     A * 100 + B
    // Line 16: }> for S {}
    // Exact: 1324; if fixed at expression start (line 13 col 5): 1305.
    const TEXT: &str = "
trait Check<const N: u32> {}
trait LocalCheck<const N: u32> {}
struct S;
impl Check<{
    let l1 = line!();
    let l2 = line!();
    let c1 = column!();
        let c2 = column!();
    l1 * 1_000_000 + l2 * 10_000 + c1 * 100 + c2
}> for S {}
impl LocalCheck<{
    const A: u32 = line!();
        const B: u32 = column!();
    A * 100 + B
}> for S {}
fn need_exact<T: Check<6_071_418>>() {}
fn need_fixed<T: Check<6_060_505>>() {}
fn need_local_exact<T: LocalCheck<1324>>() {}
fn need_local_fixed<T: LocalCheck<1305>>() {}
fn exact_ok() { need_exact::<S>() }
fn fixed_wrong() { need_fixed::<S>() }
fn local_exact_ok() { need_local_exact::<S>() }
fn local_fixed_wrong() { need_local_fixed::<S>() }
fn main() {}
";
    let got = check_fns(TEXT, &["exact_ok", "fixed_wrong", "local_exact_ok", "local_fixed_wrong"]);
    expect("multiline head anon line/column", &got, &[true, false, true, false]);
}

/// P1 #1: a body-free `fn` (`{}` so `body_holds_items == false`) declaring an item inside a
/// `where`-clause const block must emit a `blaze-scope` block module and wire `Builder::owner`
/// so the signature freezes and evaluates cleanly.
#[test]
fn a_body_free_fn_where_const_block_with_enum_evaluates_cleanly() {
    const TEXT: &str = "
trait Tr<const N: usize> {}
impl Tr<2> for () {}
fn ok_fn()
where
    (): Tr<{
        #[repr(u16)]
        enum E { A = 0, B = 1 }
        core::mem::size_of::<E>()
    }>,
{}
fn bad_fn()
where
    (): Tr<{
        #[repr(u8)]
        enum E { A = 0, B = 1 }
        core::mem::size_of::<E>()
    }>,
{}
fn call_ok() { ok_fn() }
fn call_bad() { bad_fn() }
fn main() {}
";
    let got = check_fns(TEXT, &["call_ok", "call_bad"]);
    expect("body-free fn where const block", &got, &[true, false]);
}

/// P1 #2: when a head-level item-declaring const block ALSO contains a qualified associated
/// constant (`<A as Value>::N`) or an inherent associated constant (`A::M`), leaf substitution
/// must preserve `HeadOwner` and remap block byte offsets (including inner blocks appearing
/// after the substituted leaf) back to the enclosing item's `blaze-scope` block modules.
#[test]
fn a_head_anon_const_block_preserves_owner_across_qpath_and_inherent_substitution() {
    const TEXT: &str = "
trait Value { const N: usize; }
struct A;
impl Value for A { const N: usize = 10; }
impl A { const M: usize = 20; }
trait Check<const N: usize> {}
impl Check<12> for () {}
impl Check<22> for () {}
trait QPathOk {}
trait QPathBad {}
trait InherentOk {}
trait InherentBad {}
struct S;
impl QPathOk for S
where
    (): Check<{
        let base = <A as Value>::N;
        {
            #[repr(u16)]
            enum E { X = 0, Y = 1 }
            base + core::mem::size_of::<E>()
        }
    }>,
{}
impl QPathBad for S
where
    (): Check<{
        #[repr(u8)]
        enum E { X = 0, Y = 1 }
        <A as Value>::N + core::mem::size_of::<E>()
    }>,
{}
impl InherentOk for S
where
    (): Check<{
        let base = A::M;
        {
            #[repr(u16)]
            enum Inner { X = 0, Y = 1 }
            base + core::mem::size_of::<Inner>()
        }
    }>,
{}
impl InherentBad for S
where
    (): Check<{
        let base = A::M;
        {
            #[repr(u8)]
            enum Inner { X = 0, Y = 1 }
            base + core::mem::size_of::<Inner>()
        }
    }>,
{}
fn need_qpath_ok<T: QPathOk>() {}
fn need_qpath_bad<T: QPathBad>() {}
fn need_inherent_ok<T: InherentOk>() {}
fn need_inherent_bad<T: InherentBad>() {}
fn qpath_ok() { need_qpath_ok::<S>() }
fn qpath_bad() { need_qpath_bad::<S>() }
fn inherent_ok() { need_inherent_ok::<S>() }
fn inherent_bad() { need_inherent_bad::<S>() }
fn main() {}
";
    let got = check_fns(TEXT, &["qpath_ok", "qpath_bad", "inherent_ok", "inherent_bad"]);
    expect("head anon const across substitution", &got, &[true, false, true, false]);
}

/// P1 #3: inserting a blank line above an unchanged `impl` whose head anonymous constant calls
/// `line!()` keeps the enclosing `impl`'s item-relative `LibKey` identical, but MUST change the
/// exported anonymous constant's key and evaluated value (and the frozen head's meaning) so
/// cached heads / `const_bits` never serve stale line numbers.
#[test]
fn inserting_a_blank_line_above_a_line_macro_head_const_invalidates_its_key_and_read() {
    use blaze_ladder::front2::heads::Want;
    use blaze_ladder::front2::tables::head::{HeadCells, SeededHeads, SeededHeadsOpts, StoreConsts};

    const BEFORE: &str = "trait Check<const N: u32> {}
struct S;
impl Check<{ line!() }> for S {}
fn need3<T: Check<3>>() {}
fn need4<T: Check<4>>() {}
fn at_3() { need3::<S>() }
fn at_4() { need4::<S>() }
fn main() {}
";
    const AFTER: &str = "
trait Check<const N: u32> {}
struct S;
impl Check<{ line!() }> for S {}
fn need3<T: Check<3>>() {}
fn need4<T: Check<4>>() {}
fn at_3() { need3::<S>() }
fn at_4() { need4::<S>() }
fn main() {}
";

    let freeze_impl = |text: &str| {
        let alloc = Alloc::global();
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
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("frame");
        let front = Front2::new(&names, &frame, &sources, alloc).unwrap_or_else(|e| panic!("front2: {e}"));
        let impl_key = names
            .krate(0)
            .tables
            .impls
            .iter()
            .find(|i| i.header.contains("line!()"))
            .expect("impl Check<{ line!() }> for S")
            .key;
        let prepared = Prepares::new_in(blaze_own::testing::bump());
        let mut evaluator = StoreConsts { names: &names, index: &front.index, prepared };
        let sized = front.index.lang("sized");
        let mut source = SeededHeads::new(SeededHeadsOpts {
            names: &names,
            sized,
            pointers: blaze_ladder::front2::receiver::Pointers::NOT_LOOKED_UP,
            frame: &frame,
            scratch: alloc,
            alloc,
            consts: Some(&mut evaluator),
            known: None,
            owners: None,
        });
        let (frozen, read) = source.cell(Want::Impl(impl_key)).unwrap_or_else(|e| panic!("freeze impl: {e}"));
        (impl_key, frozen.meaning(), read.consts.to_vec())
    };

    let (key_before, meaning_before, consts_before) = freeze_impl(BEFORE);
    let (key_after, meaning_after, consts_after) = freeze_impl(AFTER);
    assert_eq!(key_before, key_after, "enclosing impl key is item-relative (unchanged by blank line)");
    assert_eq!(consts_before.len(), 1, "before: one head anon const evaluated: {consts_before:?}");
    assert_eq!(consts_after.len(), 1, "after: one head anon const evaluated: {consts_after:?}");
    assert_eq!(consts_before[0].1, 3, "before: line!() is 3");
    assert_eq!(consts_after[0].1, 4, "after: line!() is 4");
    assert_ne!(consts_before[0].0, consts_after[0].0, "inserting a blank line must change the cached head anon const key");
    assert_ne!(meaning_before, meaning_after, "frozen impl head meaning must reflect the new line number");

    expect("before blank line", &check_fns(BEFORE, &["at_3", "at_4"]), &[true, false]);
    expect("after blank line", &check_fns(AFTER, &["at_3", "at_4"]), &[false, true]);
}
