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

fn test_alloc() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

fn open_toolchain() -> Toolchain {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"))
}

/// Each fn of `fns` in the crate `text`, lowered and checked in order over one store, as
/// tests/front2_const_open.rs `check_fns` does: its name and its outcome ("clean", the
/// diagnostics, or the failure as printed). The test is the entry: the allocator it names
/// here is handed down to every call.
fn check_fns(text: &str, fns: &[&str]) -> Vec<(String, String)> {
    let alloc = test_alloc();
    let tc = open_toolchain();
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
    let mut lines = blaze_own::AVec::new_in(test_alloc());
    for (n, o) in got {
        lines.push(format!("{what}: {n}: {o}"));
    }
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

/// A direct unbraced macro call at a `const` initializer's top level expanding to a block with a
/// local enum (`const VAL: usize = make_expr_enum!();`) is scanned via `head_calls` into
/// `__blaze_block_head` and resolves `MacroConstEnum` cleanly.
#[test]
fn top_level_const_initializer_macro_call_resolves_expanded_enum() {
    const TEXT: &str = "
macro_rules! make_expr_enum {
    () => {
        {
            #[repr(u16)]
            enum MacroConstEnum { A = 0, B = 1 }
            core::mem::size_of::<MacroConstEnum>()
        }
    };
}
const VAL: usize = make_expr_enum!();
fn ok_len() -> [u8; VAL] { [0u8; 2] }
fn bad_len() -> [u8; VAL] { [0u8; 3] }
fn main() {}
";
    let got = check_fns(TEXT, &["ok_len", "bad_len"]);
    expect("top-level const macro call", &got, &[true, false]);
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
        let alloc = test_alloc();
        let tc = open_toolchain();
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

/// Round 3 P1 #1: moving an unchanged module across `#[path = "..."]` files keeps the enclosing
/// `impl` item key AND its `(line, col)` identical, so `HeadSite::file` MUST be hashed into the
/// anonymous constant key so `file!().len()` produces distinct keys, bits, and frozen meanings.
#[test]
fn moving_module_across_path_files_invalidates_file_macro_head_const_key() {
    use blaze_ladder::front2::body_host::BodySources;
    use blaze_ladder::front2::consts::ConstFail;
    use blaze_ladder::front2::heads::Want;
    use blaze_ladder::front2::keys::Keys;
    use blaze_ladder::front2::source::NamesSource;
    use blaze_ladder::front2::tables::head::{ConstValues, HeadCells, SeededHeads, SeededHeadsOpts, StoreConsts};

    struct ShownFileSources;
    impl BodySources for ShownFileSources {
        fn read(&self, _: usize, _: &str) -> Option<&str> { None }
        fn read_data(&self, _: usize, _: &str, _: &str) -> Option<&[u8]> { None }
        fn env(&self, _: usize, _: &str) -> Result<Option<&str>, blaze_expand::HostError> { Ok(None) }
        fn folder(&self, _: usize) -> Option<&str> { None }
        fn shown_file<'b>(&self, _: usize, path: &str, alloc: Alloc<'b>) -> Option<blaze_own::AString<'b>> {
            Some(blaze_own::AString::from_str_in(path, alloc))
        }
        fn script_file(&self, _: usize, _: &str) -> Option<Result<Option<&[u8]>, blaze_expand::HostError>> { None }
        fn run_proc_macro<'b>(&self, _: usize, _: &blaze_expand::MacroDefinition<'_>, _: blaze_expand::ProcCall<'_>, _: Alloc<'b>) -> Result<blaze_expand::Produced<'b>, blaze_expand::HostError> {
            Err(blaze_expand::HostError { ask: blaze_expand::HostAsk::ProcMacro, why: blaze_expand::HostWhy::Refused })
        }
        fn file_left_reads(&self, _: usize, _: blaze_scope::left_reads::CallAt<'_>, _: &blaze_expand::Reads<'_>, _: core::ops::Range<usize>, _: Alloc<'_>) -> u32 { 0 }
        fn left_package(&self, _: usize, _: blaze_scope::include_place::LeftPackage<'_>) {}
    }
    struct SourcedEval<'r, 'n, S> {
        inner: StoreConsts<'r, 'n, S>,
        sources: &'n dyn BodySources,
    }
    impl<'n, S: NamesSource<'n>> ConstValues<'n> for SourcedEval<'_, 'n, S> {
        fn eval(&mut self, store: &mut Store<'n>, keys: &mut Keys, c: blaze_id::ItemKey) -> Result<(), ConstFail> {
            let prev = store.sources;
            store.sources = Some(self.sources);
            let res = self.inner.eval(store, keys, c);
            store.sources = prev;
            res
        }
    }
    static SHOWN_SOURCES: ShownFileSources = ShownFileSources;

    const MOD_TEXT: &str = "pub trait Check<const N: usize> {}
pub struct S;
impl Check<{ file!().len() }> for S {}
";

    let freeze_in_file = |mod_file: &str| {
        let alloc = test_alloc();
        let tc = open_toolchain();
        let root_text = format!("#[path = \"{mod_file}\"]\nmod m;\nfn main() {{}}\n");
        let files = [
            ("/hello-world/src/main.rs".to_string(), root_text.clone()),
            (format!("/hello-world/src/{mod_file}"), MOD_TEXT.to_string()),
        ];
        let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
        let user = UserCrate { name: "hello_world", dir: "/hello-world/", root: "/hello-world/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
        let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
        let crates = inputs.crates();
        let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
        let mod_path = format!("src/{mod_file}");
        let sources = [
            SourceFile { path: "src/main.rs", text: root_text.as_str() },
            SourceFile { path: mod_path.as_str(), text: MOD_TEXT },
        ];
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("frame");
        let front = Front2::new(&names, &frame, &sources, alloc).unwrap_or_else(|e| panic!("front2: {e}"));
        let impl_key = names
            .krate(0)
            .tables
            .impls
            .iter()
            .find(|i| i.header.contains("file!().len()"))
            .expect("impl Check<{ file!().len() }> for S")
            .key;
        let prepared = Prepares::new_in(blaze_own::testing::bump());
        let mut evaluator = SourcedEval {
            inner: StoreConsts { names: &names, index: &front.index, prepared },
            sources: &SHOWN_SOURCES,
        };
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

    let (key_short, meaning_short, consts_short) = freeze_in_file("a.rs");
    let (key_long, meaning_long, consts_long) = freeze_in_file("much_longer_file.rs");
    assert_eq!(key_short, key_long, "enclosing impl key is identical across #[path] rename");
    assert_eq!(consts_short.len(), 1, "short: {consts_short:?}");
    assert_eq!(consts_long.len(), 1, "long: {consts_long:?}");
    assert_ne!(consts_short[0].0, consts_long[0].0, "different shown filenames must produce distinct head anon const keys");
    assert_eq!(
        consts_long[0].1 - consts_short[0].1,
        ("much_longer_file.rs".len() - "a.rs".len()) as u128,
        "evaluated file!().len() must reflect the exact filename length delta"
    );
    assert_ne!(meaning_short, meaning_long, "frozen impl head meaning must reflect the new file!().len()");
}

/// Round 3 P1 #2: associated `fn`, `const`, and `type` members inside both `trait` and `impl`
/// containers keep their member `LibKey` as `Builder::owner`, so item-declaring const blocks in
/// member signatures and associated types resolve to the member's `blaze-scope` block modules.
#[test]
fn associated_fn_const_and_type_heads_map_const_blocks_to_member_block_modules() {
    const TEXT: &str = "
trait Check<const N: usize> {}
impl Check<2> for () {}

trait MemberTrait {
    type AssocTy;
    const TRAIT_CONST: [u8; {
        #[repr(u16)]
        enum TraitConstEnum { A = 0, B = 1 }
        core::mem::size_of::<TraitConstEnum>()
    }];
    fn trait_fn()
    where
        (): Check<{
            #[repr(u16)]
            enum TraitFnEnum { A = 0, B = 1 }
            core::mem::size_of::<TraitFnEnum>()
        }>;
}

struct S;
impl MemberTrait for S {
    type AssocTy = [u8; {
        #[repr(u16)]
        enum ImplTyEnum { A = 0, B = 1 }
        core::mem::size_of::<ImplTyEnum>()
    }];
    const TRAIT_CONST: [u8; {
        #[repr(u16)]
        enum ImplConstEnum { A = 0, B = 1 }
        core::mem::size_of::<ImplConstEnum>()
    }] = [0u8; 2];
    fn trait_fn()
    where
        (): Check<{
            #[repr(u16)]
            enum ImplFnEnum { A = 0, B = 1 }
            core::mem::size_of::<ImplFnEnum>()
        }>,
    {}
}

impl S {
    const INHERENT_C: [u8; {
        #[repr(u16)]
        enum InherentConstEnum { A = 0, B = 1 }
        core::mem::size_of::<InherentConstEnum>()
    }] = [0u8; 2];
    fn inherent_fn()
    where
        (): Check<{
            #[repr(u16)]
            enum InherentFnEnum { A = 0, B = 1 }
            core::mem::size_of::<InherentFnEnum>()
        }>,
    {}
}

fn check_assoc_ty_ok(x: <S as MemberTrait>::AssocTy) -> [u8; 2] { x }
fn check_assoc_ty_bad(x: <S as MemberTrait>::AssocTy) -> [u8; 3] { x }
fn check_trait_const_ok() -> [u8; 2] { <S as MemberTrait>::TRAIT_CONST }
fn check_trait_const_bad() -> [u8; 3] { <S as MemberTrait>::TRAIT_CONST }
fn check_trait_fn_ok() { <S as MemberTrait>::trait_fn() }
fn check_inherent_const_ok() -> [u8; 2] { S::INHERENT_C }
fn check_inherent_const_bad() -> [u8; 3] { S::INHERENT_C }
fn check_inherent_fn_ok() { S::inherent_fn() }
fn main() {}
";
    let got = check_fns(
        TEXT,
        &[
            "check_assoc_ty_ok",
            "check_assoc_ty_bad",
            "check_trait_const_ok",
            "check_trait_const_bad",
            "check_trait_fn_ok",
            "check_inherent_const_ok",
            "check_inherent_const_bad",
            "check_inherent_fn_ok",
        ],
    );
    expect("associated member const blocks", &got, &[true, false, true, false, true, true, false, true]);
}

/// Round 3 P1 #3: head-level item-declaring const blocks in `type` aliases, tuple/record
/// `struct`s, `enum` discriminants, and `trait` `where` clauses all emit `blaze-scope` block
/// modules and evaluate cleanly in `front2`.
#[test]
fn type_alias_struct_enum_and_trait_heads_with_const_blocks_evaluate_cleanly() {
    const TEXT: &str = "
trait Check<const N: usize> {}
impl Check<2> for () {}

type AliasBuf = [u8; {
    #[repr(u16)]
    enum AliasE { A = 0, B = 1 }
    core::mem::size_of::<AliasE>()
}];

struct TupleBuf([u8; {
    #[repr(u16)]
    enum TupleE { A = 0, B = 1 }
    core::mem::size_of::<TupleE>()
}]);

struct RecordBuf {
    data: [u8; {
        #[repr(u16)]
        enum RecordE { A = 0, B = 1 }
        core::mem::size_of::<RecordE>()
    }],
}

#[repr(u16)]
enum DiscEnum {
    V = {
        #[repr(u16)]
        enum DiscTag { A = 0, B = 1 }
        core::mem::size_of::<DiscTag>() as u16
    },
}

trait BoundedTrait
where
    (): Check<{
        #[repr(u16)]
        enum TraitE { A = 0, B = 1 }
        core::mem::size_of::<TraitE>()
    }>,
{
    fn ping(&self) {}
}

struct S;
impl BoundedTrait for S {}

fn alias_ok(x: AliasBuf) -> [u8; 2] { x }
fn alias_bad(x: AliasBuf) -> [u8; 3] { x }
fn tuple_ok(x: TupleBuf) -> [u8; 2] { x.0 }
fn tuple_bad(x: TupleBuf) -> [u8; 3] { x.0 }
fn record_ok(x: RecordBuf) -> [u8; 2] { x.data }
fn record_bad(x: RecordBuf) -> [u8; 3] { x.data }
fn disc_ok() -> [u8; DiscEnum::V as usize] { [0u8; 2] }
fn disc_bad() -> [u8; DiscEnum::V as usize] { [0u8; 3] }
fn trait_ok(s: &S) { s.ping() }
fn main() {}
";
    let got = check_fns(
        TEXT,
        &[
            "alias_ok",
            "alias_bad",
            "tuple_ok",
            "tuple_bad",
            "record_ok",
            "record_bad",
            "disc_ok",
            "disc_bad",
            "trait_ok",
        ],
    );
    expect("type/struct/enum/trait head const blocks", &got, &[true, false, true, false, true, false, true, false, true]);
}

/// Round 4 P1 #1: nominal field default containing an item-declaring const block (`struct S {
/// n: usize = { enum E {} core::mem::size_of::<E>() } }`) registers its synthetic `FnHead`
/// with `HeadAnon` under the ADT so lowering and evaluating the default resolves `E` in BOTH a
/// fresh store AND a frozen/seeded store (`Store::ensure_field_default_head_anon`).
#[test]
fn nominal_field_default_const_block_with_enum_evaluates_cleanly() {
    use blaze_ladder::front2::consts::int_of;
    use blaze_ladder::front2::heads::Want;
    use blaze_ladder::front2::keys::{Keys, checker_of};
    use blaze_ladder::front2::tables::head::{FrozenHead, Plant, export, file_frozen};

    const TEXT: &str = "#![feature(default_field_values)]
pub struct Opts {
    pub n: usize = {
        #[repr(u16)]
        enum DefaultEnum { A = 0, B = 1 }
        core::mem::size_of::<DefaultEnum>()
    },
}
const DEF_N: usize = Opts { .. }.n;
fn ok_len() -> [u8; DEF_N] { [0u8; 2] }
fn bad_len() -> [u8; DEF_N] { [0u8; 3] }
fn main() {}
";
    let got = check_fns(TEXT, &["ok_len", "bad_len"]);
    expect("nominal field default const block", &got, &[true, false]);

    // Also verify the frozen/seeded store path (`ensure_field_default_head_anon`).
    let alloc = test_alloc();
    let tc = open_toolchain();
    let files = [("/hello-world/src/main.rs".to_string(), TEXT.to_string())];
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
    let user = UserCrate { name: "hello_world", dir: "/hello-world/", root: "/hello-world/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: TEXT }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("frame");
    let front = Front2::new(&names, &frame, &sources, alloc).unwrap_or_else(|e| panic!("front2: {e}"));
    let opts = names::path(&names, names.root(0), Namespace::Types, Written { kind: PathKind::Plain, segments: &["Opts"] }, alloc)
        .unwrap_or_else(|e| panic!("Opts: {e}"))
        .whole(1)
        .expect("Opts key")
        .key;
    let sized = front.index.lang("sized");
    let mut maker = Store::new(sized, &frame, alloc);
    let mut maker_keys = Keys::new();
    maker.ensure(&names, &mut maker_keys, Want::Adt(opts)).unwrap_or_else(|e| panic!("ensure Opts: {e}"));
    let bytes = export(&names, &maker_keys, &mut maker, Want::Adt(opts), Plant::None, alloc).unwrap_or_else(|e| panic!("export Opts: {e}"));
    let frozen = FrozenHead::read(&bytes).unwrap_or_else(|e| panic!("read frozen Opts: {e}"));
    let mut seeded = Store::new(sized, &frame, alloc);
    let mut seeded_keys = Keys::new();
    file_frozen(&names, Want::Adt(opts), frozen, &seeded, &seeded_keys, alloc).unwrap_or_else(|e| panic!("file_frozen Opts: {e}"));
    let dk = seeded.field_defaults.get(&checker_of(opts)).expect("field_defaults")[0].expect("first default key");
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let (mut buckets, mut consts) = (None, Consts::default());
    let mut r = Records { names: &names, index: &front.index, store: &mut seeded, keys: &mut seeded_keys, buckets: &mut buckets, consts: None, order: BuildOrder::AsAsked };
    let val = consts.value(&mut r, prepared, dk).unwrap_or_else(|e| panic!("eval frozen field default: {e}"));
    assert_eq!(int_of(val), Some(2), "frozen field default must evaluate size_of::<DefaultEnum>() == 2");
}

/// Round 4 P2 #2: a `where const` condition declaring a local item (`impl Tr for S where const {
/// enum E {} core::mem::size_of::<E>() == 2 } {}`) attaches its synthetic `const _: bool = ...;`
/// `FnHead` to the enclosing item's `blaze-scope` block module so `E` resolves and the clause
/// evaluates cleanly (true impl selected, false impl rejected).
#[test]
fn where_const_condition_with_local_enum_evaluates_cleanly() {
    const TEXT: &str = "
trait OkTr {}
trait FalseTr {}
struct S;

impl OkTr for S
where
    const {
        #[repr(u16)]
        enum WhereEnum { A = 0, B = 1 }
        core::mem::size_of::<WhereEnum>() == 2
    }
{}

impl FalseTr for S
where
    const {
        #[repr(u8)]
        enum WhereEnumBad { A = 0, B = 1 }
        core::mem::size_of::<WhereEnumBad>() == 2
    }
{}

fn need_ok<T: OkTr>() {}
fn need_false<T: FalseTr>() {}
fn ok_holds() { need_ok::<S>() }
fn false_rejected() { need_false::<S>() }
fn main() {}
";
    let got = check_fns(TEXT, &["ok_holds", "false_rejected"]);
    expect("where const local enum", &got, &[true, false]);
}

/// Round 4 P1 #3: replacing a long qualified associated constant (`<A as Value>::N`) with the
/// identical literal produced by substitution (`10usize`) leaves the substituted text, owner,
/// offset, and start site identical while shifting a later `column!()` call in the original
/// source. Hashing `owner.len` and `edits` into the head anon const key ensures the two builds
/// get distinct keys and exact distinct `column!()` values.
#[test]
fn replacing_substituted_qpath_with_literal_invalidates_column_macro_head_const_key() {
    use blaze_ladder::front2::heads::Want;
    use blaze_ladder::front2::tables::head::{HeadCells, SeededHeads, SeededHeadsOpts, StoreConsts};

    // Line 5 in WITH_QPATH: `impl Check<{ <A as Value>::N + column!() }> for S {}`
    //   `column!()` starts at 1-based column 32 -> 10 + 32 = 42.
    // Line 5 in WITH_LITERAL: `impl Check<{ 10usize + column!() }> for S {}`
    //   `column!()` starts at 1-based column 24 -> 10 + 24 = 34.
    // Both substitute to the exact same text `10usize + column!()` at the exact same
    // (owner.item, owner.offset, line=5, col=14, file="src/main.rs").
    const WITH_QPATH: &str = "trait Value { const N: usize; }
struct A;
impl Value for A { const N: usize = 10; }
trait Check<const N: u32> {}
struct S;
impl Check<{ <A as Value>::N as u32 + column!() }> for S {}
fn main() {}
";
    const WITH_LITERAL: &str = "trait Value { const N: usize; }
struct A;
impl Value for A { const N: usize = 10; }
trait Check<const N: u32> {}
struct S;
impl Check<{ 10usize as u32 + column!() }> for S {}
fn main() {}
";

    let freeze_impl = |text: &str| {
        let alloc = test_alloc();
        let tc = open_toolchain();
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
            .find(|i| i.header.contains("column!()"))
            .expect("impl Check<{ ... column!() }> for S")
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

    let (key_qpath, meaning_qpath, consts_qpath) = freeze_impl(WITH_QPATH);
    let (key_lit, meaning_lit, consts_lit) = freeze_impl(WITH_LITERAL);
    assert_eq!(key_qpath, key_lit, "enclosing impl key is identical");
    // `consts_qpath` has both the `<A as Value>::N` leaf (10) and the head anon const (10 + 39 = 49);
    // `consts_lit` has only the head anon const (10 + 31 = 41).
    let &(anon_qpath_key, anon_qpath_val) = consts_qpath.iter().find(|&&(_, v)| v > 10).expect("qpath head anon const");
    let &(anon_lit_key, anon_lit_val) = consts_lit.iter().find(|&&(_, v)| v > 10).expect("literal head anon const");
    assert_eq!(anon_qpath_val, 10 + 39, "qpath column!() must report original column 39");
    assert_eq!(anon_lit_val, 10 + 31, "literal column!() must report column 31");
    assert_ne!(anon_qpath_key, anon_lit_key, "replacing substituted leaf with its literal must invalidate the head anon const key");
    assert_ne!(meaning_qpath, meaning_lit, "frozen impl head meaning must reflect the new column");
}

/// Round 5 P1 #1: moving an otherwise unchanged `where const` condition containing `line!()` to
/// another line changes both its persisted `PClause::Const` key and its evaluated result (both
/// directly and after freezing/reconstructing via `ensure_where_const_head`).
#[test]
fn where_const_condition_key_tracks_source_line_and_evaluates_accurately() {
    use blaze_ladder::front2::consts::int_of;
    use blaze_ladder::front2::heads::Want;
    use blaze_ladder::front2::keys::{Keys, checker_of};
    use blaze_ladder::front2::tables::head::{FrozenHead, Plant, export, file_frozen};
    use blaze_typeck::PClause;

    const ON_LINE_5: &str = "trait OkTr {}
struct S;
impl OkTr for S
where
    const { #[repr(u16)] enum LineEnum { A = 0, B = 1 } line!() == 5 && core::mem::size_of::<LineEnum>() == 2 }
{}
fn need_ok<T: OkTr>() {}
fn ok_holds() { need_ok::<S>() }
fn main() {}
";
    const ON_LINE_6: &str = "
trait OkTr {}
struct S;
impl OkTr for S
where
    const { #[repr(u16)] enum LineEnum { A = 0, B = 1 } line!() == 5 && core::mem::size_of::<LineEnum>() == 2 }
{}
fn need_ok<T: OkTr>() {}
fn ok_holds() { need_ok::<S>() }
fn main() {}
";

    let got_5 = check_fns(ON_LINE_5, &["ok_holds"]);
    expect("where const line 5", &got_5, &[true]);
    let got_6 = check_fns(ON_LINE_6, &["ok_holds"]);
    expect("where const line 6", &got_6, &[false]);

    // Also verify persisted `PClause::Const` keys differ and frozen/seeded reconstruction matches.
    let inspect_frozen = |text: &str| {
        let alloc = test_alloc();
        let tc = open_toolchain();
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
            .find(|i| i.header.contains("LineEnum"))
            .expect("impl OkTr for S")
            .key;
        let sized = front.index.lang("sized");
        let mut maker = Store::new(sized, &frame, alloc);
        let mut maker_keys = Keys::new();
        maker.ensure(&names, &mut maker_keys, Want::Impl(impl_key)).unwrap_or_else(|e| panic!("ensure impl: {e}"));
        let bytes = export(&names, &maker_keys, &mut maker, Want::Impl(impl_key), Plant::None, alloc).unwrap_or_else(|e| panic!("export impl: {e}"));
        let frozen = FrozenHead::read(&bytes).unwrap_or_else(|e| panic!("read frozen impl: {e}"));
        let mut seeded = Store::new(sized, &frame, alloc);
        let mut seeded_keys = Keys::new();
        file_frozen(&names, Want::Impl(impl_key), frozen, &seeded, &seeded_keys, alloc).unwrap_or_else(|e| panic!("file_frozen impl: {e}"));
        let cond_key = seeded
            .preds
            .get(&checker_of(impl_key))
            .expect("seeded impl preds")
            .clauses
            .iter()
            .find_map(|c| match *c {
                PClause::Const { cond, .. } => Some(cond),
                _ => None,
            })
            .expect("PClause::Const");
        let prepared = Prepares::new_in(blaze_own::testing::bump());
        let (mut buckets, mut consts) = (None, Consts::default());
        let mut r = Records { names: &names, index: &front.index, store: &mut seeded, keys: &mut seeded_keys, buckets: &mut buckets, consts: None, order: BuildOrder::AsAsked };
        let val = consts.value(&mut r, prepared, cond_key).unwrap_or_else(|e| panic!("eval frozen where const: {e}"));
        (cond_key, int_of(val))
    };

    let (cond_5, val_5) = inspect_frozen(ON_LINE_5);
    let (cond_6, val_6) = inspect_frozen(ON_LINE_6);
    assert_ne!(cond_5, cond_6, "moving `where const` containing `line!()` must change its persisted key");
    assert_eq!(val_5, Some(1), "frozen `where const` on line 5 must evaluate true");
    assert_eq!(val_6, Some(0), "frozen `where const` on line 6 must evaluate false");
}

/// Round 5 P1 #2: structural record field defaults (`{ n: usize = { enum E {} ... } }`) are stored
/// under `RecordSchema::defaults_key()`, not `checker_of(item)`. Importing/seeding them into a
/// fresh store with empty `head_anons` must recover ownership through `ensure_field_default_site`.
#[test]
fn structural_record_field_default_with_enum_recovers_ownership_in_fresh_store() {
    use blaze_ladder::front2::consts::int_of;
    use blaze_ladder::front2::head_record::{self, HeadAt};
    use blaze_ladder::front2::heads::Want;
    use blaze_ladder::front2::keys::Keys;
    use blaze_typeck::{Name, RecordSchema};

    const TEXT: &str = "#![blaze::records]
pub type Config = {
    n: usize = {
        #[repr(u16)]
        enum RecDefaultEnum { A = 0, B = 1 }
        core::mem::size_of::<RecDefaultEnum>()
    },
};
fn main() {}
";
    let alloc = test_alloc();
    let tc = open_toolchain();
    let files = [("/hello-world/src/main.rs".to_string(), TEXT.to_string())];
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
    let user = UserCrate { name: "hello_world", dir: "/hello-world/", root: "/hello-world/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: TEXT }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("frame");
    let front = Front2::new(&names, &frame, &sources, alloc).unwrap_or_else(|e| panic!("front2: {e}"));
    let cfg = names::path(&names, names.root(0), Namespace::Types, Written { kind: PathKind::Plain, segments: &["Config"] }, alloc)
        .unwrap_or_else(|e| panic!("Config: {e}"))
        .whole(1)
        .expect("Config key")
        .key;
    let sized = front.index.lang("sized");

    // Fresh store: registers structural record defaults under `schema.defaults_key()`.
    let mut maker = Store::new(sized, &frame, alloc);
    let mut maker_keys = Keys::new();
    maker.ensure(&names, &mut maker_keys, Want::Head(cfg)).unwrap_or_else(|e| panic!("ensure Config: {e}"));
    let schema = RecordSchema::of_sorted_names(&[Name::of("n")]);
    let rk = schema.defaults_key();
    let dk = maker.field_defaults.get(&rk).expect("record field_defaults")[0].expect("first record default key");
    let encoded_dk = {
        let head_ref = head_record::from_store(&maker, HeadAt::Fn(dk)).unwrap_or_else(|e| panic!("from_store dk: {e}"));
        head_record::encode(head_ref, maker.tys.view(), alloc).unwrap_or_else(|e| panic!("encode dk: {e}"))
    };
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    {
        let (mut buckets, mut consts) = (None, Consts::default());
        let mut r = Records { names: &names, index: &front.index, store: &mut maker, keys: &mut maker_keys, buckets: &mut buckets, consts: None, order: BuildOrder::AsAsked };
        let val = consts.value(&mut r, prepared, dk).unwrap_or_else(|e| panic!("eval fresh record default: {e}"));
        assert_eq!(int_of(val), Some(2), "fresh structural record default must evaluate size_of::<RecDefaultEnum>() == 2");
    }

    // Seeded/frozen store: empty `locals.head_anons` and empty `anon_marks`, carrying only the
    // decoded frozen `Head::Fn(dk)` and the structural record's `field_defaults` row (under `rk`,
    // NOT `checker_of(cfg)`).
    let mut seeded = Store::new(sized, &frame, alloc);
    let mut seeded_keys = Keys::new();
    let mut keep_text = |s: &str| -> &str { frame.alloc_str(s) };
    let decoded = head_record::decode_with_origins_boxed_inline(&encoded_dk, None, &seeded.tys, alloc, &mut keep_text, &mut |_, _| Ok(()))
        .unwrap_or_else(|e| panic!("decode dk: {e}"));
    seeded
        .file_heads(Want::Head(cfg), [(HeadAt::Fn(dk), blaze_own::Box::into_inner(decoded))])
        .unwrap_or_else(|e| panic!("file_heads dk: {e}"));
    let mut seeded_defaults = blaze_own::AVec::new_in(alloc);
    seeded_defaults.push(Some(dk));
    seeded.field_defaults.insert(rk, seeded_defaults);
    let (mut buckets, mut consts) = (None, Consts::default());
    let mut r = Records { names: &names, index: &front.index, store: &mut seeded, keys: &mut seeded_keys, buckets: &mut buckets, consts: None, order: BuildOrder::AsAsked };
    let val = consts.value(&mut r, prepared, dk).unwrap_or_else(|e| panic!("eval seeded structural record default: {e}"));
    assert_eq!(int_of(val), Some(2), "seeded structural record default must recover HeadAnon and evaluate size_of::<RecDefaultEnum>() == 2");
}

/// Round 5 P1 #3: an exported macro from a dependency crate writing an item-declaring nominal
/// field default and `where const` condition referencing `$crate::N` preserves def-site hygiene
/// marks into `anon_marks` in BOTH fresh stores and frozen/seeded stores.
#[test]
fn exported_macro_field_default_and_where_const_preserve_dollar_crate_hygiene() {
    use blaze_id::CrateItemKey;
    use blaze_ids::ContentId;
    use blaze_ladder::front2::consts::int_of;
    use blaze_ladder::front2::heads::Want;
    use blaze_ladder::front2::keys::{Keys, checker_of};
    use blaze_ladder::front2::tables::head::{FrozenHead, Plant, export, file_frozen};
    use blaze_scope::graph::{CrateId, Sysroot};
    use blaze_scope::resolve::{CrateInput, SourceTexts, resolve_crates};
    use blaze_typeck::PClause;

    struct WithDep<'t> {
        base: &'t blaze_ladder::crates::Texts<'t>,
        dep: usize,
        files: &'t [(&'t str, &'t str)],
    }

    impl<'t> SourceTexts<'t> for WithDep<'t> {
        fn read(&self, krate: usize, path: &str) -> Option<&'t str> {
            if krate == self.dep {
                return self.files.iter().find(|(p, _)| *p == path).map(|(_, t)| *t);
            }
            self.base.read(krate, path)
        }
    }

    const DEP: &str = "
pub const N: usize = 5;
pub trait CheckTr {}
#[macro_export]
macro_rules! define_with_default {
    () => {
        pub struct MacroOpts {
            pub n: usize = {
                #[repr(u16)]
                enum MacroDefEnum { A = 0, B = 1 }
                $crate::N + core::mem::size_of::<MacroDefEnum>()
            },
        }
        pub struct MacroS;
        impl $crate::CheckTr for MacroS
        where
            const {
                #[repr(u16)]
                enum MacroWhereEnum { A = 0, B = 1 }
                $crate::N + core::mem::size_of::<MacroWhereEnum>() == 7
            }
        {}
    };
}
";
    const APP: &str = "#![feature(default_field_values)]
// Local `N` has a different value to prove `$crate::N` resolves to `dep::N` (5), not `crate::N` (99).
pub const N: usize = 99;
dep::define_with_default!();
const DEF_VAL: usize = MacroOpts { .. }.n;
fn need_tr<T: dep::CheckTr>() {}
fn ok_all() -> [u8; DEF_VAL] {
    need_tr::<MacroS>();
    [0u8; 7]
}
fn bad_len() -> [u8; DEF_VAL] {
    [0u8; 8]
}
fn main() {}
";

    let alloc = test_alloc();
    let tc = open_toolchain();
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
    let user_files = [("/user/src/main.rs".to_string(), APP.to_string())];
    let dep_files = [("/dep/src/lib.rs", DEP)];
    let user = UserCrate { name: "user", dir: "/user/", root: "/user/src/main.rs", files: &user_files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let mut crates = inputs.crates();
    let at = |name: &str| crates.iter().position(|c| c.name == name).map(|i| CrateId(u32::try_from(i).expect("few crates")));
    let sysroot = Sysroot { core: at("core"), alloc: at("alloc"), std: at("std"), bstd: at("bstd"), proc_macro: at("proc_macro") };
    let dep_at = crates.len();
    let user_deps: [(&str, usize); 1] = [("dep", dep_at)];
    let (cfg, crate_edition, implicit) = (crates[0].cfg, crates[0].edition, crates[0].implicit);
    crates[0].deps = &user_deps;
    crates.push(CrateInput {
        name: "dep",
        key: CrateItemKey(ContentId([0x42; 32])),
        edition: crate_edition,
        implicit,
        root: "/dep/src/lib.rs",
        shown_from: "/dep/",
        features: &[],
        deps: &[],
        cfg,
        script: None,
    });
    let texts = WithDep { base: &inputs.texts, dep: dep_at, files: &dep_files };
    let names = resolve_crates(&crates, sysroot, &texts, &blaze_scope::task::Serial, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: APP }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("frame");
    let mut front = Front2::new(&names, &frame, &sources, alloc).unwrap_or_else(|e| panic!("front2: {e}"));
    let prepared = Prepares::new_in(blaze_own::testing::bump());

    // 1. Fresh store check (`ok_all` clean with `DEF_VAL == 5 + 2 == 7`, `bad_len` reports E0308).
    let sized = front.index.lang("sized");
    let mut store = Store::new(sized, &frame, alloc);
    let (mut buckets, mut consts) = (None, Consts::default());
    let mut got = blaze_own::AVec::new_in(alloc);
    for &n in &["ok_all", "bad_len"] {
        let found = names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: &[n] }, alloc).unwrap_or_else(|e| panic!("`{n}`: {e}"));
        let k = found.whole(1).unwrap_or_else(|| panic!("`{n}` not named")).key;
        let mut lens = 0;
        let outcome = loop {
            match lower_fn(&names, prepared, &mut front.keys, &mut store, k) {
                Err(BodyError::Convert { why: Some(Why::NeedConst(c, _)), .. }) | Err(BodyError::Head { error: HeadError::Item(_, Why::NeedConst(c, _)), .. }) if lens < MOST_LENS => {
                    lens += 1;
                    let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: Some((&mut consts, prepared)) };
                    if let Err(e) = eval_len(&mut records, c) {
                        break format!("not lowered: {e}");
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
                        Ok(done) => format!("diags {:?}", done.diags),
                        Err(CheckFail::Diags { diags, .. }) => format!("diags {diags:?}"),
                        Err(e) => format!("not checked: {e}"),
                    };
                }
            }
        };
        got.push((n.to_string(), outcome));
    }
    expect("exported macro $crate field default + where const", &got, &[true, false]);

    // 2. Frozen/seeded store check for BOTH the macro-emitted field default AND the `where const`.
    let opts = names::path(&names, names.root(0), Namespace::Types, Written { kind: PathKind::Plain, segments: &["MacroOpts"] }, alloc)
        .unwrap_or_else(|e| panic!("MacroOpts: {e}"))
        .whole(1)
        .expect("MacroOpts key")
        .key;
    let impl_key = names
        .krate(0)
        .tables
        .impls
        .iter()
        .find(|i| i.header.contains("MacroWhereEnum"))
        .expect("impl CheckTr for MacroS")
        .key;
    let mut maker = Store::new(sized, &frame, alloc);
    let mut maker_keys = Keys::new();
    maker.ensure(&names, &mut maker_keys, Want::Adt(opts)).unwrap_or_else(|e| panic!("ensure MacroOpts: {e}"));
    maker.ensure(&names, &mut maker_keys, Want::Impl(impl_key)).unwrap_or_else(|e| panic!("ensure MacroS impl: {e}"));
    let opts_bytes = export(&names, &maker_keys, &mut maker, Want::Adt(opts), Plant::None, alloc).unwrap_or_else(|e| panic!("export MacroOpts: {e}"));
    let impl_bytes = export(&names, &maker_keys, &mut maker, Want::Impl(impl_key), Plant::None, alloc).unwrap_or_else(|e| panic!("export MacroS impl: {e}"));

    let mut seeded = Store::new(sized, &frame, alloc);
    let mut seeded_keys = Keys::new();
    file_frozen(&names, Want::Adt(opts), FrozenHead::read(&opts_bytes).expect("read MacroOpts"), &seeded, &seeded_keys, alloc).unwrap_or_else(|e| panic!("file_frozen MacroOpts: {e}"));
    file_frozen(&names, Want::Impl(impl_key), FrozenHead::read(&impl_bytes).expect("read MacroS impl"), &seeded, &seeded_keys, alloc).unwrap_or_else(|e| panic!("file_frozen MacroS impl: {e}"));
    let dk = seeded.field_defaults.get(&checker_of(opts)).expect("MacroOpts defaults")[0].expect("MacroOpts.n default");
    let cond_key = seeded
        .preds
        .get(&checker_of(impl_key))
        .expect("MacroS impl preds")
        .clauses
        .iter()
        .find_map(|c| match *c {
            PClause::Const { cond, .. } => Some(cond),
            _ => None,
        })
        .expect("MacroS where const");
    let (mut buckets, mut consts) = (None, Consts::default());
    let mut r = Records { names: &names, index: &front.index, store: &mut seeded, keys: &mut seeded_keys, buckets: &mut buckets, consts: None, order: BuildOrder::AsAsked };
    let def_val = consts.value(&mut r, prepared, dk).unwrap_or_else(|e| panic!("eval frozen MacroOpts.n: {e}"));
    assert_eq!(int_of(def_val), Some(7), "frozen macro field default must resolve `$crate::N` (5) + 2 == 7");
    let where_val = consts.value(&mut r, prepared, cond_key).unwrap_or_else(|e| panic!("eval frozen MacroS where const: {e}"));
    assert_eq!(int_of(where_val), Some(1), "frozen macro `where const` must resolve `$crate::N` (5) + 2 == 7");
}

/// Round 6 P1 #1: a macro-written head constant combining `$crate::N`, an inherent constant leaf
/// (`Self::C` or `Pair::C`), and an item-declaring block (`enum E {}`) must remap def-site hygiene
/// marks through `inherent_edits` on BOTH the `self_value` path (`Self` remains in the expression)
/// and the `const_value_bits_owned` path (no `Self` remains).
#[test]
fn macro_head_const_with_inherent_const_and_dollar_crate_preserves_hygiene() {
    use blaze_id::CrateItemKey;
    use blaze_ids::ContentId;
    use blaze_scope::graph::{CrateId, Sysroot};
    use blaze_scope::resolve::{CrateInput, SourceTexts, resolve_crates};

    struct WithDep<'t> {
        base: &'t blaze_ladder::crates::Texts<'t>,
        dep: usize,
        files: &'t [(&'t str, &'t str)],
    }

    impl<'t> SourceTexts<'t> for WithDep<'t> {
        fn read(&self, krate: usize, path: &str) -> Option<&'t str> {
            if krate == self.dep {
                return self.files.iter().find(|(p, _)| *p == path).map(|(_, t)| *t);
            }
            self.base.read(krate, path)
        }
    }

    const DEP: &str = "
pub const N: usize = 5;
pub trait Tag<const K: usize> {}
#[macro_export]
macro_rules! define_inherent_head {
    () => {
        pub struct Pair(pub u16);
        impl Pair {
            pub const C: usize = 3;
            pub fn take_self(_: [u8; {
                #[repr(u16)]
                enum SelfInherentEnum { A = 0, B = 1 }
                $crate::N + Self::C + core::mem::size_of::<Self>() + core::mem::size_of::<SelfInherentEnum>()
            }]) {}
        }
        impl $crate::Tag<{
            #[repr(u16)]
            enum NonSelfInherentEnum { A = 0, B = 1 }
            $crate::N + Pair::C + core::mem::size_of::<NonSelfInherentEnum>()
        }> for Pair {}
    };
}
";
    const APP: &str = "
pub const N: usize = 99;
dep::define_inherent_head!();
fn need_tag<const K: usize, T: dep::Tag<K>>() {}
fn self_ok() { Pair::take_self([0u8; 12]); }
fn self_bad() { Pair::take_self([0u8; 13]); }
fn tag_ok() { need_tag::<10, Pair>(); }
fn tag_bad() { need_tag::<11, Pair>(); }
fn main() {}
";

    let alloc = test_alloc();
    let tc = open_toolchain();
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
    let user_files = [("/user/src/main.rs".to_string(), APP.to_string())];
    let dep_files = [("/dep/src/lib.rs", DEP)];
    let user = UserCrate { name: "user", dir: "/user/", root: "/user/src/main.rs", files: &user_files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let mut crates = inputs.crates();
    let at = |name: &str| crates.iter().position(|c| c.name == name).map(|i| CrateId(u32::try_from(i).expect("few crates")));
    let sysroot = Sysroot { core: at("core"), alloc: at("alloc"), std: at("std"), bstd: at("bstd"), proc_macro: at("proc_macro") };
    let dep_at = crates.len();
    let user_deps: [(&str, usize); 1] = [("dep", dep_at)];
    let (cfg, crate_edition, implicit) = (crates[0].cfg, crates[0].edition, crates[0].implicit);
    crates[0].deps = &user_deps;
    crates.push(CrateInput {
        name: "dep",
        key: CrateItemKey(ContentId([0x43; 32])),
        edition: crate_edition,
        implicit,
        root: "/dep/src/lib.rs",
        shown_from: "/dep/",
        features: &[],
        deps: &[],
        cfg,
        script: None,
    });
    let texts = WithDep { base: &inputs.texts, dep: dep_at, files: &dep_files };
    let names = resolve_crates(&crates, sysroot, &texts, &blaze_scope::task::Serial, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: APP }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("frame");
    let mut front = Front2::new(&names, &frame, &sources, alloc).unwrap_or_else(|e| panic!("front2: {e}"));
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut store = Store::new(front.index.lang("sized"), &frame, alloc);
    let (mut buckets, mut consts) = (None, Consts::default());
    let mut got = blaze_own::AVec::new_in(alloc);
    for &n in &["self_ok", "self_bad", "tag_ok", "tag_bad"] {
        let found = names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: &[n] }, alloc).unwrap_or_else(|e| panic!("`{n}`: {e}"));
        let k = found.whole(1).unwrap_or_else(|| panic!("`{n}` not named")).key;
        let mut lens = 0;
        let outcome = loop {
            match lower_fn(&names, prepared, &mut front.keys, &mut store, k) {
                Err(BodyError::Convert { why: Some(Why::NeedConst(c, _)), .. }) | Err(BodyError::Head { error: HeadError::Item(_, Why::NeedConst(c, _)), .. }) if lens < MOST_LENS => {
                    lens += 1;
                    let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: Some((&mut consts, prepared)) };
                    if let Err(e) = eval_len(&mut records, c) {
                        break format!("not lowered: {e}");
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
                        Ok(done) => format!("diags {:?}", done.diags),
                        Err(CheckFail::Diags { diags, .. }) => format!("diags {diags:?}"),
                        Err(e) => format!("not checked: {e}"),
                    };
                }
            }
        };
        got.push((n.to_string(), outcome));
    }
    expect("inherent const + $crate hygiene", &got, &[true, false, true, false]);
}

/// Round 6 P1 #2: local `fn`, `const`, and `static` items declared inside a function body whose
/// heads contain item-declaring const blocks (`fn outer() { fn inner(_: [u8; { enum E {} ... }]) {} }`)
/// retain their library-twin head owner so `E` maps to the local item's `blaze-scope` block module.
#[test]
fn local_fn_const_and_static_heads_with_const_blocks_resolve_local_enums() {
    const TEXT: &str = "
fn ok_outer() -> u8 {
    /// Leading doc comment exercises trivia alignment between parser and nested scanner.
    fn inner(a: [u8; {
        #[repr(u16)]
        enum LocalFnEnum { A = 0, B = 1 }
        core::mem::size_of::<LocalFnEnum>()
    }]) -> u8 {
        a[1]
    }
    const LOCAL_C: [u8; {
        #[repr(u16)]
        enum LocalConstEnum { A = 0, B = 1 }
        core::mem::size_of::<LocalConstEnum>()
    }] = [1, 2];
    static LOCAL_S: [u8; {
        #[repr(u16)]
        enum LocalStaticEnum { A = 0, B = 1 }
        core::mem::size_of::<LocalStaticEnum>()
    }] = [3, 4];
    inner([0u8; 2]) + LOCAL_C[1] + LOCAL_S[1]
}
fn bad_local_fn() {
    fn inner(_: [u8; {
        #[repr(u16)]
        enum BadFnEnum { A = 0, B = 1 }
        core::mem::size_of::<BadFnEnum>()
    }]) {}
    inner([0u8; 3]);
}
fn bad_local_const() -> [u8; 3] {
    const LOCAL_C: [u8; {
        #[repr(u16)]
        enum BadConstEnum { A = 0, B = 1 }
        core::mem::size_of::<BadConstEnum>()
    }] = [1, 2];
    LOCAL_C
}
fn bad_local_static() -> &'static [u8; 3] {
    static LOCAL_S: [u8; {
        #[repr(u16)]
        enum BadStaticEnum { A = 0, B = 1 }
        core::mem::size_of::<BadStaticEnum>()
    }] = [3, 4];
    &LOCAL_S
}
fn main() {}
";
    let got = check_fns(TEXT, &["ok_outer", "bad_local_fn", "bad_local_const", "bad_local_static"]);
    expect("local fn/const/static head const blocks", &got, &[true, false, false, false]);
}

/// Round 7 P1 #1: freezing an owner head whose `where const` or field default observes `line!()`
/// through `Tables` + `Recorder` must record `TableRead::File` (mapped by `blaze-cli` onto
/// `Kind::FileItems` `WHOLE`) so inserting a blank line above the item invalidates the cached
/// owner head instead of reusing a stale `PClause::Const` key. Normal heads without `!` must NOT
/// record `TableRead::File`.
#[test]
fn line_sensitive_where_const_and_field_default_record_file_read_for_invalidation() {
    use blaze_ladder::front2::heads::Want;
    use blaze_ladder::front2::source::{Load, Log, Records as SourceRecords};
    use blaze_ladder::front2::tables::head::{HeadCells, SeededHeads, SeededHeadsOpts, StoreConsts};
    use blaze_ladder::front2::tables::recorder::{Recorder, RecorderOpts, TableRead};
    use blaze_ladder::front2::tables::view::{Tables, TablesOpts};
    use blaze_scope::items_record::encode as encode_items;
    use blaze_scope::names_record::encode as encode_names;
    use blaze_scope::resolve::aligned;

    const TEXT: &str = "#![feature(default_field_values)]
trait OkTr {}
trait PlainTr {}
struct S;
impl OkTr for S
where
    const { #[repr(u16)] enum LineEnum { A = 0, B = 1 } line!() == 7 && core::mem::size_of::<LineEnum>() == 2 }
{}
impl PlainTr for S
where
    const { 1 + 1 == 2 }
{}
pub struct LineOpts {
    pub n: u32 = line!(),
}
fn main() {}
";

    struct SolvedLoad<'n> {
        solved: &'n blaze_scope::resolve::Names<'n>,
        records: &'n [(&'n [u8], &'n [u8])],
    }

    impl<'n> Load<'n> for SolvedLoad<'n> {
        fn names(&self, krate: usize) -> Option<&'n [u8]> {
            self.records.get(krate).map(|c| c.0)
        }
        fn items(&self, krate: usize) -> Option<&'n [u8]> {
            self.records.get(krate).map(|c| c.1)
        }
        fn file(&self, krate: usize, path: blaze_scope::package_path::PackagePath<'_>) -> Option<&'n str> {
            let k = self.solved.crates().get(krate)?;
            let want = path.as_str();
            k.files.iter().find(|f| f.path == want || f.path.ends_with(want)).map(|f| f.text)
        }
        fn expansion(&self, _krate: usize, _call: blaze_id::ItemKey) -> Option<blaze_ladder::front2::source::LoadedExpansion<'n>> {
            None
        }
    }

    fn leak<T>(v: T, alloc: Alloc<'static>) -> &'static mut T {
        blaze_own::Box::leak_static(blaze_own::Box::new_in(v, alloc))
    }
    let alloc = test_alloc();
    let bump = blaze_own::testing::bump();
    let tc: &'static Toolchain = leak(open_toolchain(), alloc);
    let files: &'static [(String, String)] = leak([("/hello-world/src/main.rs".to_string(), TEXT.to_string())], alloc).as_slice();
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
    let user: &'static UserCrate<'static> = leak(UserCrate { name: "hello_world", dir: "/hello-world/", root: "/hello-world/src/main.rs", files, edition, panic: blaze_build::Panic::Unwind }, alloc);
    let inputs: &'static Inputs<'static> = leak(Inputs::new(user, tc).unwrap_or_else(|e| panic!("the crates: {e}")), alloc);
    let mut crates = blaze_own::AVec::new_in(alloc);
    for c in inputs.crates() {
        crates.push(c);
    }
    let crates: &'static [blaze_scope::resolve::CrateInput<'static>] = crates.leak_static();
    let solved: &'static blaze_scope::resolve::Names<'static> = leak(inputs.resolve(crates, bump).unwrap_or_else(|e| panic!("resolve_crates: {e}")), alloc);

    let mut loaded = blaze_own::AVec::new_in(alloc);
    let mut crate_keys = blaze_own::AVec::new_in(alloc);
    let mut crate_names = blaze_own::AVec::new_in(alloc);
    for i in 0..solved.crates().len() {
        let k = solved.krate(i);
        let nb = aligned(&encode_names(k, alloc).expect("encode names"), bump);
        let ib = aligned(&encode_items(k, alloc).expect("encode items"), bump);
        loaded.push((nb, ib));
        crate_keys.push(k.key);
        crate_names.push(k.name);
    }
    let load: &'static SolvedLoad<'static> = leak(SolvedLoad { solved, records: loaded.leak_static() }, alloc);
    let rec_frame: &'static blaze_mem::Frame = leak(blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("rec frame"), alloc);
    let log: &'static Log<'static> = leak(Log::new(crate_names.leak_static(), alloc), alloc);
    let records: &'static SourceRecords<'static> = leak(SourceRecords::new(crate_keys.leak_static(), load, log, rec_frame, alloc), alloc);

    let line_impl = solved.krate(0).tables.impls.iter().find(|i| i.header.contains("LineEnum")).expect("line impl").key;
    let plain_impl = solved.krate(0).tables.impls.iter().find(|i| i.header.contains("PlainTr")).expect("plain impl").key;
    let line_opts = names::path(solved, solved.root(0), Namespace::Types, Written { kind: PathKind::Plain, segments: &["LineOpts"] }, alloc)
        .unwrap_or_else(|e| panic!("LineOpts: {e}"))
        .whole(1)
        .expect("LineOpts key")
        .key;
    let prefix = solved.krate(0).key.prefix();

    let sources = [SourceFile { path: "src/main.rs", text: TEXT }];
    let frame: &'static blaze_mem::Frame = leak(blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("frame"), alloc);
    let front = Front2::new(solved, frame, &sources, alloc).unwrap_or_else(|e| panic!("front2: {e}"));
    let sized = front.index.lang("sized");

    let mut results = blaze_own::AVec::new_in(alloc);
    for want in [Want::Impl(line_impl), Want::Adt(line_opts), Want::Impl(plain_impl)] {
        let rec = Recorder::new(RecorderOpts { alloc });
        {
            let view = Tables::new(TablesOpts { records, impls: None, lang_crates: &[], reads: &rec });
            let prepared = Prepares::new_in(blaze_own::testing::bump());
            let mut evaluator = StoreConsts { names: view, index: &front.index, prepared };
            let mut source = SeededHeads::new(SeededHeadsOpts {
                names: view,
                sized,
                pointers: blaze_ladder::front2::receiver::Pointers::NOT_LOOKED_UP,
                frame,
                scratch: alloc,
                alloc,
                consts: Some(&mut evaluator),
                known: None,
                owners: None,
            });
            let _ = source.cell(want).unwrap_or_else(|e| panic!("freeze {want:?}: {e}"));
        }
        results.push(rec.finish().expect("recorder finish"));
    }

    let line_impl_reads = &results[0];
    assert!(
        line_impl_reads.scan_for(&TableRead::File { krate: prefix, file: 0 }).is_some(),
        "impl with `where const {{ line!() }}` must record TableRead::File for invalidation: {line_impl_reads:?}"
    );
    let line_opts_reads = &results[1];
    assert!(
        line_opts_reads.scan_for(&TableRead::File { krate: prefix, file: 0 }).is_some(),
        "ADT with `line!()` field default must record TableRead::File for invalidation: {line_opts_reads:?}"
    );
    let plain_impl_reads = &results[2];
    assert!(
        !plain_impl_reads.iter().any(|r| matches!(r, TableRead::File { .. })),
        "normal `where const` without `!` must NOT record TableRead::File: {plain_impl_reads:?}"
    );
}

/// Round 7 P1 #2: structural record field defaults declaring local items inside an `impl` method
/// body or a `trait` default method body must attach their `HeadAnon` owner to the method's
/// member body (not the enclosing `impl`/`trait` container) so local items resolve cleanly.
#[test]
fn structural_record_default_in_impl_and_trait_method_bodies_resolves_local_enum() {
    const TEXT: &str = "#![blaze::records]
pub struct S;
impl S {
    pub fn impl_ok() -> [u8; 2] {
        let r: {
            n: usize = {
                #[repr(u16)]
                enum ImplMethodRecEnum { A = 0, B = 1 }
                core::mem::size_of::<ImplMethodRecEnum>()
            },
        } = { .. };
        [r.n as u8; 2]
    }
    pub fn impl_bad() -> [u8; 3] {
        let r: {
            n: usize = {
                #[repr(u16)]
                enum ImplMethodRecBad { A = 0, B = 1 }
                core::mem::size_of::<ImplMethodRecBad>()
            },
        } = { .. };
        let _ = r.n;
        [0u8; 2]
    }
}
pub trait Tr {
    fn trait_ok() -> usize {
        let r: {
            n: usize = {
                #[repr(u16)]
                enum TraitMethodRecEnum { A = 0, B = 1 }
                core::mem::size_of::<TraitMethodRecEnum>()
            },
        } = { .. };
        r.n
    }
}
impl Tr for S {}
fn call_impl_ok() -> [u8; 2] { S::impl_ok() }
fn call_impl_bad() -> [u8; 3] { S::impl_bad() }
fn call_trait_ok() -> usize { <S as Tr>::trait_ok() }
fn main() {}
";

    let alloc = test_alloc();
    let tc = open_toolchain();
    let files = [("/hello-world/src/main.rs".to_string(), TEXT.to_string())];
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
    let user = UserCrate { name: "hello_world", dir: "/hello-world/", root: "/hello-world/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: TEXT }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("frame");
    let mut front = Front2::new(&names, &frame, &sources, alloc).unwrap_or_else(|e| panic!("front2: {e}"));
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut store = Store::new(front.index.lang("sized"), &frame, alloc);
    let (mut buckets, mut consts) = (None, Consts::default());

    let impl_key = names.krate(0).tables.impls.iter().find(|i| i.header.contains("impl S")).expect("impl S").key;
    let trait_key = names::path(&names, names.root(0), Namespace::Types, Written { kind: PathKind::Plain, segments: &["Tr"] }, alloc)
        .unwrap_or_else(|e| panic!("Tr: {e}"))
        .whole(1)
        .expect("Tr key")
        .key;
    store.ensure(&names, &mut front.keys, blaze_ladder::front2::heads::Want::Impl(impl_key)).expect("ensure impl S");
    store.ensure(&names, &mut front.keys, blaze_ladder::front2::heads::Want::Members(trait_key)).expect("ensure trait Tr");

    let impl_ok_k = front.keys.member(blaze_ladder::front2::keys::checker_of(impl_key), blaze_ladder::front2::keys::MemberKind::Fn, "impl_ok").expect("impl_ok key");
    let impl_bad_k = front.keys.member(blaze_ladder::front2::keys::checker_of(impl_key), blaze_ladder::front2::keys::MemberKind::Fn, "impl_bad").expect("impl_bad key");
    let trait_ok_k = front.keys.member(blaze_ladder::front2::keys::checker_of(trait_key), blaze_ladder::front2::keys::MemberKind::Fn, "trait_ok").expect("trait_ok key");

    let mut got = blaze_own::AVec::new_in(alloc);
    for &(name, k) in &[("S::impl_ok", impl_ok_k), ("S::impl_bad", impl_bad_k), ("Tr::trait_ok", trait_ok_k)] {
        let body = blaze_ladder::front2::body::lower_member(&names, prepared, &mut front.keys, &mut store, k).unwrap_or_else(|e| panic!("lower {name}: {e}"));
        let view = body.lowered.view();
        let spell = Spelling::of(&view).unwrap_or_else(|e| panic!("spelling {name}: {e:?}"));
        let scope = body.scope(&spell);
        let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: Some((&mut consts, prepared)) };
        let outcome = match check_body(&mut records, &body, &scope) {
            Ok(done) if done.diags.is_empty() => "clean".to_string(),
            Ok(done) => format!("diags {:?}", done.diags),
            Err(CheckFail::Diags { diags, .. }) => format!("diags {diags:?}"),
            Err(e) => format!("not checked: {e}"),
        };
        got.push((name.to_string(), outcome));
    }
    expect("record defaults in impl/trait method bodies", &got, &[true, false, true]);

    // Also evaluate the registered default constants directly to confirm `size_of::<Enum>() == 2`
    // (not just type-checked without running the constant evaluator).
    let schema = blaze_typeck::RecordSchema::of_sorted_names(&[blaze_typeck::Name::of("n")]);
    let dk = store.field_defaults.get(&schema.defaults_key()).expect("record field_defaults")[0].expect("n default key");
    let mut r = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: None };
    let val = consts.value(&mut r, prepared, dk).unwrap_or_else(|e| panic!("eval method record default: {e}"));
    assert_eq!(blaze_ladder::front2::consts::int_of(val), Some(2), "method structural record default must evaluate size_of::<Enum>() == 2");
}

/// Round 8 P1: a direct unbraced macro call in an item head (`type A = [u8; make_len!()];`,
/// tuple struct, fn signature) or nominal field default (`n: usize = make_len!()`) where the
/// macro expands to an item-declaring block (`{ enum E {} size_of::<E>() }`) is scanned by
/// `blaze-scope`, expanded into a block module, and mapped back when evaluating the constant.
#[test]
fn direct_macro_call_in_head_and_field_default_resolves_expanded_enum() {
    const TEXT: &str = "
macro_rules! make_len {
    () => {
        {
            #[repr(u16)]
            enum MacroHeadEnum { A = 0, B = 1 }
            core::mem::size_of::<MacroHeadEnum>()
        }
    };
}
macro_rules! one {
    () => { 1usize };
}

pub type AliasArr = [u8; make_len!()];
pub struct TupleArr(pub [u8; make_len!()]);
pub fn take_arr(_: [u8; make_len!()]) {}
pub struct Opts {
    pub a: usize = one!(),
    pub n: usize = make_len!(),
}

const DEF_N: usize = Opts { .. }.n;

fn alias_ok() -> AliasArr { [0u8; 2] }
fn alias_bad() -> AliasArr { [0u8; 3] }
fn tuple_ok() -> TupleArr { TupleArr([0u8; 2]) }
fn tuple_bad() -> TupleArr { TupleArr([0u8; 3]) }
fn fn_ok() { take_arr([0u8; 2]); }
fn fn_bad() { take_arr([0u8; 3]); }
fn default_ok() -> [u8; DEF_N] { [0u8; 2] }
fn default_bad() -> [u8; DEF_N] { [0u8; 3] }
fn main() {}
";
    let got = check_fns(
        TEXT,
        &["alias_ok", "alias_bad", "tuple_ok", "tuple_bad", "fn_ok", "fn_bad", "default_ok", "default_bad"],
    );
    expect(
        "direct macro call in heads & field defaults",
        &got,
        &[true, false, true, false, true, false, true, false],
    );
}

/// Round 9 P1: `#[cfg(any())]` on a fn parameter, record struct field, or tuple struct field
/// prevents expanding `missing!()` while a sibling `#[cfg(all())]` macro call still expands and
/// resolves its local enum cleanly.
#[test]
fn cfg_disabled_head_macro_calls_are_skipped_while_enabled_ones_resolve() {
    const TEXT: &str = "
macro_rules! make_len {
    () => {
        {
            #[repr(u16)]
            enum CfgHeadEnum { A = 0, B = 1 }
            core::mem::size_of::<CfgHeadEnum>()
        }
    };
}

pub fn f(
    #[cfg(any())] _gone: [u8; missing!()],
    #[cfg(all())] _ok: [u8; make_len!()],
) {}

pub struct S {
    #[cfg(any())]
    pub gone: [u8; missing!()],
    #[cfg(all())]
    pub ok: [u8; make_len!()],
}

pub struct T(
    #[cfg(any())] pub [u8; missing!()],
    #[cfg(all())] pub [u8; make_len!()],
);

pub enum NonExh {
    #[cfg_attr(all(), non_exhaustive)]
    V([u8; make_len!()]),
}

pub fn g(
    #[cfg_attr(all(), cfg(any()))] _gone: [u8; missing!()],
    #[cfg_attr(any(), cfg(any()))] _kept: [u8; make_len!()],
    #[cfg_attr(all(), cfg(all()))] _ok: [u8; make_len!()],
) {}

fn fn_ok() { f([0u8; 2]); }
fn fn_bad() { f([0u8; 3]); }
fn struct_ok() -> S { S { ok: [0u8; 2] } }
fn struct_bad() -> S { S { ok: [0u8; 3] } }
fn tuple_ok() -> T { T([0u8; 2]) }
fn tuple_bad() -> T { T([0u8; 3]) }
fn enum_ok() -> NonExh { NonExh::V([0u8; 2]) }
fn enum_bad() -> NonExh { NonExh::V([0u8; 3]) }
fn cfg_attr_ok() { g([0u8; 2], [0u8; 2]); }
fn cfg_attr_bad() { g([0u8; 2], [0u8; 3]); }
fn main() {}
";
    let got = check_fns(
        TEXT,
        &[
            "fn_ok", "fn_bad", "struct_ok", "struct_bad", "tuple_ok", "tuple_bad",
            "enum_ok", "enum_bad", "cfg_attr_ok", "cfg_attr_bad",
        ],
    );
    expect(
        "cfg-gated head macro calls",
        &got,
        &[true, false, true, false, true, false, true, false, true, false],
    );
}

/// Round 11 P1 #1: when an inherent associated constant (`A::N`) is nested inside a qualified
/// associated constant (`<[u8; A::N] as Tr>::C`) in a macro-written head constant that also
/// references `$crate::EXTRA` and declares a local enum, `compose_edits` must supersede the
/// contained inner edit so both `HeadOwner` block mapping and `$crate` hygiene stay intact.
#[test]
fn nested_inherent_const_inside_qpath_const_preserves_head_owner_and_hygiene() {
    use blaze_id::CrateItemKey;
    use blaze_ids::ContentId;
    use blaze_scope::graph::{CrateId, Sysroot};
    use blaze_scope::resolve::{SourceTexts, resolve_crates};

    struct WithDep<'t> {
        base: &'t blaze_ladder::crates::Texts<'t>,
        dep: usize,
        files: &'t [(&'t str, &'t str)],
    }

    impl<'t> SourceTexts<'t> for WithDep<'t> {
        fn read(&self, krate: usize, path: &str) -> Option<&'t str> {
            if krate == self.dep {
                return self.files.iter().find(|(p, _)| *p == path).map(|(_, t)| *t);
            }
            self.base.read(krate, path)
        }
    }

    const DEP: &str = "
pub const EXTRA: usize = 4;
#[macro_export]
macro_rules! define_nested_subst_head {
    () => {
        pub struct A;
        impl A {
            pub const N: usize = 3;
        }
        pub trait Tr {
            const C: usize;
        }
        impl Tr for [u8; 3] {
            const C: usize = 5;
        }
        pub fn take_nested(_: [u8; {
            #[repr(u16)]
            enum NestedSubstEnum { A = 0, B = 1 }
            <[u8; A::N] as Tr>::C + $crate::EXTRA + core::mem::size_of::<NestedSubstEnum>()
        }]) {}
    };
}
";
    const APP: &str = "
pub const EXTRA: usize = 99;
dep::define_nested_subst_head!();
fn ok_len() { take_nested([0u8; 11]); }
fn bad_len() { take_nested([0u8; 12]); }
fn main() {}
";

    let alloc = test_alloc();
    let tc = open_toolchain();
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
    let user_files = [("/user/src/main.rs".to_string(), APP.to_string())];
    let dep_files = [("/dep/src/lib.rs", DEP)];
    let user = UserCrate { name: "user", dir: "/user/", root: "/user/src/main.rs", files: &user_files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let mut crates = inputs.crates();
    let at = |name: &str| crates.iter().position(|c| c.name == name).map(|i| CrateId(u32::try_from(i).expect("few crates")));
    let sysroot = Sysroot { core: at("core"), alloc: at("alloc"), std: at("std"), bstd: at("bstd"), proc_macro: at("proc_macro") };
    let dep_at = crates.len();
    let user_deps: [(&str, usize); 1] = [("dep", dep_at)];
    let (cfg, crate_edition, implicit) = (crates[0].cfg, crates[0].edition, crates[0].implicit);
    crates[0].deps = &user_deps;
    crates.push(blaze_scope::resolve::CrateInput {
        name: "dep",
        key: CrateItemKey(ContentId([0x44; 32])),
        edition: crate_edition,
        implicit,
        root: "/dep/src/lib.rs",
        shown_from: "/dep/",
        features: &[],
        deps: &[],
        cfg,
        script: None,
    });
    let texts = WithDep { base: &inputs.texts, dep: dep_at, files: &dep_files };
    let names = resolve_crates(&crates, sysroot, &texts, &blaze_scope::task::Serial, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: APP }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("frame");
    let mut front = Front2::new(&names, &frame, &sources, alloc).unwrap_or_else(|e| panic!("front2: {e}"));
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut store = Store::new(front.index.lang("sized"), &frame, alloc);
    let (mut buckets, mut consts) = (None, Consts::default());
    let mut got = blaze_own::AVec::new_in(alloc);
    for &n in &["ok_len", "bad_len"] {
        let found = names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: &[n] }, alloc).unwrap_or_else(|e| panic!("`{n}`: {e}"));
        let k = found.whole(1).unwrap_or_else(|| panic!("`{n}` not named")).key;
        let mut lens = 0;
        let outcome = loop {
            match lower_fn(&names, prepared, &mut front.keys, &mut store, k) {
                Err(BodyError::Convert { why: Some(Why::NeedConst(c, _)), .. }) | Err(BodyError::Head { error: HeadError::Item(_, Why::NeedConst(c, _)), .. }) if lens < MOST_LENS => {
                    lens += 1;
                    let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: Some((&mut consts, prepared)) };
                    if let Err(e) = eval_len(&mut records, c) {
                        break format!("not lowered: {e}");
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
                        Ok(done) => format!("diags {:?}", done.diags),
                        Err(CheckFail::Diags { diags, .. }) => format!("diags {diags:?}"),
                        Err(e) => format!("not checked: {e}"),
                    };
                }
            }
        };
        got.push((n.to_string(), outcome));
    }
    expect("nested inherent const inside qpath const", &got, &[true, false]);
}

/// Round 12 P1 #1: type macros in item heads (`type A = ty!();`, `fn ret_ty() -> ty!()`,
/// `struct Tup(ty!());`) carry enclosing head ownership through expansion so item-declaring
/// const blocks inside the expanded type resolve in `blaze-scope`'s expansion block modules.
#[test]
fn type_macro_in_head_with_item_declaring_const_block_resolves() {
    let text = "
macro_rules! ty {
    () => {
        [u8; {
            #[repr(u16)]
            enum TyMacroEnum { A = 0, B = 1 }
            core::mem::size_of::<TyMacroEnum>()
        }]
    };
}
type A = ty!();
fn ret_ty() -> ty!() { [0u8; 2] }
struct Tup(ty!());

fn alias_ok(x: A) -> [u8; 2] { x }
fn alias_bad(x: A) -> [u8; 3] { x }
fn ret_ok() -> [u8; 2] { ret_ty() }
fn ret_bad() -> [u8; 3] { ret_ty() }
fn tup_ok(t: Tup) -> [u8; 2] { t.0 }
fn tup_bad(t: Tup) -> [u8; 3] { t.0 }
fn main() {}
";
    let got = check_fns(text, &["alias_ok", "alias_bad", "ret_ok", "ret_bad", "tup_ok", "tup_bad"]);
    expect("type macro in head with item-declaring const block", &got, &[true, false, true, false, true, false]);
}

/// Round 12 P1 #3: generic type defaults that contain an owned lifetime (`&'static ...`) take
/// `lay_default_arg`'s general re-lowering path and must preserve the declaring item's head owner
/// so item-declaring const blocks inside the default resolve.
#[test]
fn generic_default_with_owned_lifetime_and_item_declaring_const_block_resolves() {
    let text = "
struct S<T = &'static [u8; {
    #[repr(u16)]
    enum DefLtEnum { A = 0, B = 1 }
    core::mem::size_of::<DefLtEnum>()
}]>(T);

fn ok_def(s: S) -> &'static [u8; 2] { s.0 }
fn bad_def(s: S) -> &'static [u8; 3] { s.0 }
fn main() {}
";
    let got = check_fns(text, &["ok_def", "bad_def"]);
    expect("generic default with owned lifetime and item-declaring const block", &got, &[true, false]);
}

/// Round 12 P1 #4 + Round 13 P1 #1: identical structural-record default text in two different
/// modules/owners must receive distinct anonymous default keys AND stay owner-scoped in
/// `World::field_defaults` so checking bodies for `a::Rec` and `b::Rec` AFTER both are loaded
/// evaluates `a::Rec`'s default in `mod a` (`N == 1`) and `b::Rec`'s default in `mod b` (`N == 2`).
#[test]
fn structural_record_defaults_with_identical_text_in_distinct_modules_do_not_collide() {
    use blaze_ladder::front2::consts::int_of;
    use blaze_ladder::front2::heads::Want;
    use blaze_ladder::front2::keys::Keys;
    use blaze_typeck::{Name, RecordSchema};

    const TEXT: &str = "#![blaze::records]
mod a {
    pub const N: isize = 1;
    pub type Rec = { n: isize = { enum E { V = N } E::V as isize } };
    pub fn mk() -> Rec { { .. } }
}
mod b {
    pub const N: isize = 2;
    pub type Rec = { n: isize = { enum E { V = N } E::V as isize } };
    pub fn mk() -> Rec { { .. } }
}
fn mk_a_root() -> a::Rec { { .. } }
fn mk_b_root() -> b::Rec { { .. } }
fn main() {}
";
    let alloc = test_alloc();
    let tc = open_toolchain();
    let files = [("/hello-world/src/main.rs".to_string(), TEXT.to_string())];
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
    let user = UserCrate { name: "hello_world", dir: "/hello-world/", root: "/hello-world/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: TEXT }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("frame");
    let front = Front2::new(&names, &frame, &sources, alloc).unwrap_or_else(|e| panic!("front2: {e}"));
    let lookup = |ns, segs: &[&str]| {
        names::path(&names, names.root(0), ns, Written { kind: PathKind::Plain, segments: segs }, alloc)
            .unwrap_or_else(|e| panic!("{segs:?}: {e}"))
            .whole(segs.len())
            .expect("key")
            .key
    };
    let rec_a = lookup(Namespace::Types, &["a", "Rec"]);
    let rec_b = lookup(Namespace::Types, &["b", "Rec"]);

    let mut store = Store::new(front.index.lang("sized"), &frame, alloc);
    let mut keys = Keys::new();
    let schema = RecordSchema::of_sorted_names(&[Name::of("n")]);
    let rk = schema.defaults_key();

    store.ensure(&names, &mut keys, Want::Head(rec_a)).unwrap_or_else(|e| panic!("ensure a::Rec: {e}"));
    let dk_a = store.record_defaults_by_owner.get(&(rec_a, rk)).expect("a::Rec record_defaults")[0].expect("a::Rec default key");

    store.ensure(&names, &mut keys, Want::Head(rec_b)).unwrap_or_else(|e| panic!("ensure b::Rec: {e}"));
    let dk_b = store.record_defaults_by_owner.get(&(rec_b, rk)).expect("b::Rec record_defaults")[0].expect("b::Rec default key");

    assert_ne!(dk_a, dk_b, "identical structural-record default text in distinct modules must produce distinct keys");

    // Check bodies for BOTH `a::Rec` and `b::Rec` AFTER `b::Rec` has been registered:
    // `World::field_defaults` must resolve `dk_a` for `a::mk` / `mk_a_root` and `dk_b` for
    // `b::mk` / `mk_b_root` (NOT overwritten by `b::Rec`!).
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let (mut buckets, mut consts) = (None, Consts::default());
    let cases: [(&[&str], u128, &str); 4] = [
        (&["a", "mk"], 1, "a::mk must pick a::Rec default (N == 1)"),
        (&["b", "mk"], 2, "b::mk must pick b::Rec default (N == 2)"),
        (&["mk_a_root"], 1, "mk_a_root must pick a::Rec default (N == 1)"),
        (&["mk_b_root"], 2, "mk_b_root must pick b::Rec default (N == 2)"),
    ];
    for (segs, expected, msg) in cases {
        let fn_key = lookup(Namespace::Values, segs);
        let body = lower_fn(&names, prepared, &mut keys, &mut store, fn_key).unwrap_or_else(|e| panic!("lower {segs:?}: {e}"));
        let view = body.lowered.view();
        let spell = Spelling::of(&view).unwrap_or_else(|e| panic!("spell {segs:?}: {e:?}"));
        let scope = body.scope(&spell);
        let picked_dk = {
            let mut r = Records { names: &names, index: &front.index, store: &mut store, keys: &mut keys, buckets: &mut buckets, consts: Some((&mut consts, prepared)), order: BuildOrder::AsAsked };
            let done = check_body(&mut r, &body, &scope).unwrap_or_else(|e| panic!("check {segs:?}: {e}"));
            assert!(done.diags.is_empty(), "{segs:?} diags: {:?}", done.diags);
            *done.default_fields.column::<2>().first().unwrap_or_else(|| panic!("{segs:?} must record default_fields"))
        };
        let mut r2 = Records { names: &names, index: &front.index, store: &mut store, keys: &mut keys, buckets: &mut buckets, consts: None, order: BuildOrder::AsAsked };
        let got = int_of(consts.value(&mut r2, prepared, picked_dk).unwrap_or_else(|e| panic!("eval {segs:?} default: {e}")));
        assert_eq!(got, Some(expected), "{msg}");
    }
}

/// Round 13 P1 #2: inserting a blank line above a frozen nominal ADT whose field default uses
/// `line!()` + a local enum invalidates the frozen ADT cell (`file_frozen` rejects the stale key),
/// `ensure_field_default_site` refuses to attach moved coordinates to the stale key, and fresh
/// lowering produces a new key evaluating to the shifted line.
#[test]
fn frozen_nominal_field_default_with_line_macro_rejects_stale_key_after_line_shift() {
    use blaze_ladder::front2::consts::int_of;
    use blaze_ladder::front2::head_record::{self, HeadAt};
    use blaze_ladder::front2::heads::Want;
    use blaze_ladder::front2::keys::{Keys, checker_of};
    use blaze_ladder::front2::tables::head::{FrozenHead, Plant, export, file_frozen};

    const BEFORE: &str = "#![feature(default_field_values)]
pub struct Opts {
    pub n: usize = { #[repr(u16)] enum ShiftEnum { A = 0, B = 1 } line!() as usize + core::mem::size_of::<ShiftEnum>() },
}
fn main() {}
";
    const AFTER: &str = "#![feature(default_field_values)]

pub struct Opts {
    pub n: usize = { #[repr(u16)] enum ShiftEnum { A = 0, B = 1 } line!() as usize + core::mem::size_of::<ShiftEnum>() },
}
fn main() {}
";

    let alloc = test_alloc();
    let tc = open_toolchain();
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
    let files_before = [("/hello-world/src/main.rs".to_string(), BEFORE.to_string())];
    let user_before = UserCrate { name: "hello_world", dir: "/hello-world/", root: "/hello-world/src/main.rs", files: &files_before, edition, panic: blaze_build::Panic::Unwind };
    let inputs_before = Inputs::new(&user_before, &tc).unwrap_or_else(|e| panic!("crates before: {e}"));
    let crates_before = inputs_before.crates();
    let names_before = inputs_before.resolve(&crates_before, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve before: {e}"));

    let files_after = [("/hello-world/src/main.rs".to_string(), AFTER.to_string())];
    let user_after = UserCrate { name: "hello_world", dir: "/hello-world/", root: "/hello-world/src/main.rs", files: &files_after, edition, panic: blaze_build::Panic::Unwind };
    let inputs_after = Inputs::new(&user_after, &tc).unwrap_or_else(|e| panic!("crates after: {e}"));
    let crates_after = inputs_after.crates();
    let names_after = inputs_after.resolve(&crates_after, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve after: {e}"));
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("frame");
    let front_before = Front2::new(&names_before, &frame, &[SourceFile { path: "src/main.rs", text: BEFORE }], alloc).unwrap_or_else(|e| panic!("front before: {e}"));
    let front_after = Front2::new(&names_after, &frame, &[SourceFile { path: "src/main.rs", text: AFTER }], alloc).unwrap_or_else(|e| panic!("front after: {e}"));
    let find_opts = |names: &blaze_scope::resolve::Names<'_>| {
        names::path(names, names.root(0), Namespace::Types, Written { kind: PathKind::Plain, segments: &["Opts"] }, alloc)
            .unwrap_or_else(|e| panic!("Opts: {e}"))
            .whole(1)
            .expect("Opts key")
            .key
    };
    let opts_before = find_opts(&names_before);
    let opts_after = find_opts(&names_after);
    let sized = front_before.index.lang("sized");

    // Lower and freeze `Opts` at BEFORE (line 3 -> 3 + 2 = 5).
    let mut maker = Store::new(sized, &frame, alloc);
    let mut maker_keys = Keys::new();
    maker.ensure(&names_before, &mut maker_keys, Want::Adt(opts_before)).unwrap_or_else(|e| panic!("ensure before: {e}"));
    let dk_before = maker.field_defaults.get(&checker_of(opts_before)).expect("defaults before")[0].expect("dk_before");
    let frozen_bytes = export(&names_before, &maker_keys, &mut maker, Want::Adt(opts_before), Plant::None, alloc).unwrap_or_else(|e| panic!("export before: {e}"));
    let encoded_dk = {
        let head_ref = head_record::from_store(&maker, HeadAt::Fn(dk_before)).unwrap_or_else(|e| panic!("from_store dk_before: {e}"));
        head_record::encode(head_ref, maker.tys.view(), alloc).unwrap_or_else(|e| panic!("encode dk_before: {e}"))
    };
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let val_before = {
        let (mut buckets, mut consts) = (None, Consts::default());
        let mut r = Records { names: &names_before, index: &front_before.index, store: &mut maker, keys: &mut maker_keys, buckets: &mut buckets, consts: None, order: BuildOrder::AsAsked };
        int_of(consts.value(&mut r, prepared, dk_before).unwrap_or_else(|e| panic!("eval before: {e}")))
    };
    assert_eq!(val_before, Some(5), "BEFORE line 3 + size_of::<ShiftEnum>() (2) == 5");

    // 1) Importing the stale frozen cell into AFTER must fail validation (line shifted 3 -> 4).
    let frozen = FrozenHead::read(&frozen_bytes).unwrap_or_else(|e| panic!("read frozen: {e}"));
    let seeded_check = Store::new(front_after.index.lang("sized"), &frame, alloc);
    let seeded_keys = Keys::new();
    assert!(
        file_frozen(&names_after, Want::Adt(opts_after), frozen, &seeded_check, &seeded_keys, alloc).is_err(),
        "file_frozen must reject stale frozen ADT field default after blank-line shift"
    );

    // 2) Even if seeded directly via `file_heads`, `ensure_field_default_site` must refuse the stale key.
    let mut direct = Store::new(front_after.index.lang("sized"), &frame, alloc);
    let mut direct_keys = Keys::new();
    let mut keep_text = |s: &str| -> &str { frame.alloc_str(s) };
    let decoded = head_record::decode_with_origins_boxed_inline(&encoded_dk, None, &direct.tys, alloc, &mut keep_text, &mut |_, _| Ok(()))
        .unwrap_or_else(|e| panic!("decode dk_before: {e}"));
    direct
        .file_heads(Want::Adt(opts_after), [(HeadAt::Fn(dk_before), blaze_own::Box::into_inner(decoded))])
        .unwrap_or_else(|e| panic!("file_heads: {e}"));
    let mut stale_defaults = blaze_own::AVec::new_in(alloc);
    stale_defaults.push(Some(dk_before));
    direct.field_defaults.insert(checker_of(opts_after), stale_defaults);
    assert!(
        blaze_ladder::front2::body::lower_member(&names_after, prepared, &mut direct_keys, &mut direct, dk_before).is_err(),
        "lower_member must refuse stale field default key after blank-line shift"
    );

    // 3) Freshly lowering `Opts` on AFTER produces a new key evaluating to line 4 + 2 = 6.
    let mut fresh = Store::new(front_after.index.lang("sized"), &frame, alloc);
    let mut fresh_keys = Keys::new();
    fresh.ensure(&names_after, &mut fresh_keys, Want::Adt(opts_after)).unwrap_or_else(|e| panic!("ensure after: {e}"));
    let dk_after = fresh.field_defaults.get(&checker_of(opts_after)).expect("defaults after")[0].expect("dk_after");
    assert_ne!(dk_before, dk_after, "shifted line must produce a distinct field default key");
    let (mut buckets, mut consts) = (None, Consts::default());
    let mut r = Records { names: &names_after, index: &front_after.index, store: &mut fresh, keys: &mut fresh_keys, buckets: &mut buckets, consts: None, order: BuildOrder::AsAsked };
    let val_after = int_of(consts.value(&mut r, prepared, dk_after).unwrap_or_else(|e| panic!("eval after: {e}")));
    assert_eq!(val_after, Some(6), "AFTER line 4 + size_of::<ShiftEnum>() (2) == 6");
}

/// Round 13 P1 #3: a cross-crate `#[macro_export]` type macro expanding to a type with an
/// item-declaring const block AND `$crate::` paths preserves expansion hygiene marks in BOTH
/// `pattern_ty.rs::macro_ty` (`type A = dep::ty!();`) and `heads.rs::fn_head` (`fn ret() -> dep::ty!()`).
#[test]
fn cross_crate_type_macro_in_head_preserves_dollar_crate_hygiene_and_block_owner() {
    use blaze_id::CrateItemKey;
    use blaze_ids::ContentId;
    use blaze_scope::graph::{CrateId, Sysroot};
    use blaze_scope::resolve::{SourceTexts, resolve_crates};

    struct WithDep<'t> {
        base: &'t blaze_ladder::crates::Texts<'t>,
        dep: usize,
        files: &'t [(&'t str, &'t str)],
    }

    impl<'t> SourceTexts<'t> for WithDep<'t> {
        fn read(&self, krate: usize, path: &str) -> Option<&'t str> {
            if krate == self.dep {
                return self.files.iter().find(|(p, _)| *p == path).map(|(_, t)| *t);
            }
            self.base.read(krate, path)
        }
    }

    const DEP: &str = "
pub const N: isize = 5;
pub const EXTRA: usize = 3;
#[macro_export]
macro_rules! ty_enum_hygiene {
    () => {
        [u8; {
            enum TyHygEnum { V = $crate::N }
            TyHygEnum::V as usize + $crate::EXTRA
        }]
    };
}
";
    const APP: &str = "
pub const N: isize = 99;
pub const EXTRA: usize = 77;
type A = dep::ty_enum_hygiene!();
fn ret_ty() -> dep::ty_enum_hygiene!() { [0u8; 8] }

fn alias_ok(x: A) -> [u8; 8] { x }
fn alias_bad(x: A) -> [u8; 9] { x }
fn ret_ok() -> [u8; 8] { ret_ty() }
fn ret_bad() -> [u8; 9] { ret_ty() }
fn main() {}
";

    let alloc = test_alloc();
    let tc = open_toolchain();
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
    let user_files = [("/user/src/main.rs".to_string(), APP.to_string())];
    let dep_files = [("/dep/src/lib.rs", DEP)];
    let user = UserCrate { name: "user", dir: "/user/", root: "/user/src/main.rs", files: &user_files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let mut crates = inputs.crates();
    let at = |name: &str| crates.iter().position(|c| c.name == name).map(|i| CrateId(u32::try_from(i).expect("few crates")));
    let sysroot = Sysroot { core: at("core"), alloc: at("alloc"), std: at("std"), bstd: at("bstd"), proc_macro: at("proc_macro") };
    let dep_at = crates.len();
    let user_deps: [(&str, usize); 1] = [("dep", dep_at)];
    let (cfg, crate_edition, implicit) = (crates[0].cfg, crates[0].edition, crates[0].implicit);
    crates[0].deps = &user_deps;
    crates.push(blaze_scope::resolve::CrateInput {
        name: "dep",
        key: CrateItemKey(ContentId([0x55; 32])),
        edition: crate_edition,
        implicit,
        root: "/dep/src/lib.rs",
        shown_from: "/dep/",
        features: &[],
        deps: &[],
        cfg,
        script: None,
    });
    let texts = WithDep { base: &inputs.texts, dep: dep_at, files: &dep_files };
    let names = resolve_crates(&crates, sysroot, &texts, &blaze_scope::task::Serial, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: APP }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("frame");
    let mut front = Front2::new(&names, &frame, &sources, alloc).unwrap_or_else(|e| panic!("front2: {e}"));
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut store = Store::new(front.index.lang("sized"), &frame, alloc);
    let (mut buckets, mut consts) = (None, Consts::default());
    let mut got = blaze_own::AVec::new_in(alloc);
    for &n in &["alias_ok", "alias_bad", "ret_ok", "ret_bad"] {
        let found = names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: &[n] }, alloc).unwrap_or_else(|e| panic!("`{n}`: {e}"));
        let k = found.whole(1).unwrap_or_else(|| panic!("`{n}` not named")).key;
        let mut lens = 0;
        let outcome = loop {
            match lower_fn(&names, prepared, &mut front.keys, &mut store, k) {
                Err(BodyError::Convert { why: Some(Why::NeedConst(c, _)), .. }) | Err(BodyError::Head { error: HeadError::Item(_, Why::NeedConst(c, _)), .. }) if lens < MOST_LENS => {
                    lens += 1;
                    let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: Some((&mut consts, prepared)) };
                    if let Err(e) = eval_len(&mut records, c) {
                        break format!("not lowered: {e}");
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
                        Ok(done) => format!("diags {:?}", done.diags),
                        Err(CheckFail::Diags { diags, .. }) => format!("diags {diags:?}"),
                        Err(e) => format!("not checked: {e}"),
                    };
                }
            }
        };
        got.push((n.to_string(), outcome));
    }
    expect("cross-crate type macro with $crate hygiene and local enum", &got, &[true, false, true, false]);
}
