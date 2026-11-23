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
