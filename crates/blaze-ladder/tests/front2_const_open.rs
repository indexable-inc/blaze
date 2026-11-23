//! Constants in types through front2 with the real core (front2/const_open.rs; typeck.md,
//! "Constants in types: three states"): the four faces measured on the walk of the hello world
//! (port27/front2/list-AG.txt: `check gap: FnHead` 255, `lower: Convert` an array repeat
//! length 44, `check gap: BodyConst` 23, the byteorder impl's refused header 21).
//!
//! Every fixture has a fn that must check clean AND one with the wrong length that must be a
//! type mismatch of the program: a fix that made a length a hole (a const variable takes any
//! value) would pass the second clean and fail here.
//!
//! The Reference, "Generic parameters", "Const generics": "Const parameters can be used
//! anywhere a const item can be used, with the exception that when used in a type or array
//! repeat expression, it must be standalone"; "As a further restriction, const parameters may
//! only appear as a standalone argument inside of a type or array repeat expression. In those
//! contexts, they may only be used as a single segment path expression, possibly inside a
//! block (such as `N` or `{N}`). That is, they cannot be combined with other expressions."
//! The Reference says nothing more on WHEN a constant expression in a type is evaluated or
//! how two are compared: with the restriction above, every constant in a type of an accepted
//! program is a value or one parameter, so there is nothing unevaluated to compare. The
//! pinned library's own use is the evidence for what must have a value: an expression over a
//! ground `Self` (library/core/src/num/uint_macros.rs) and over a ground type argument
//! (library/core/src/num/mod.rs, `repeat_u8`).
//!
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`, as tests/front2_items.rs
//! does. NOT RUN by the writer (a write-only lane).

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
/// tests/front2_items.rs `check_fns` does, with the driver's round for a constant a body's
/// conversion needs (front2/reach.rs: `Why::NeedConst` has the constant evaluated by
/// `eval_len` and the body lowered again): its name and its outcome ("clean", the
/// diagnostics, or the failure as printed).
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
                        // The program's own errors (`CheckFail::Diags`: "The check finished
                        // with diagnostics"): an outcome of the check, not a gap.
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
/// a diagnostic of the program (`false`: the outcome starts with `diags`; a gap or a refusal
/// is "not checked" or "not lowered" and fails both).
fn expect(what: &str, got: &[(String, String)], clean: &[bool]) {
    let lines: Vec<String> = got.iter().map(|(n, o)| format!("{what}: {n}: {o}")).collect();
    for l in &lines {
        println!("{l}");
    }
    assert_eq!(got.len(), clean.len(), "{}", lines.join("\n"));
    for ((n, o), &want) in got.iter().zip(clean) {
        if want {
            assert_eq!(o, "clean", "`{n}` must check clean\n{}", lines.join("\n"));
        } else {
            assert!(o.starts_with("diags "), "`{n}` must be an error of the program, not clean and not a gap\n{}", lines.join("\n"));
        }
    }
}

/// Face (a): a signature's array length over a GROUND `Self` has a value at the head. The
/// pinned core, library/core/src/num/uint_macros.rs (inside `impl u16 { uint_impl! { .. } }`):
/// "pub const fn from_le_bytes(bytes: [u8; size_of::<Self>()]) -> Self": `Self` is `u16`, the
/// length is 2. It was refused "a constant expression over a generic parameter" (`Self` was
/// taken for a parameter), and `core::fmt::Arguments::estimated_capacity`, which calls
/// `u16::from_le_bytes`, was a `check gap: FnHead` with 254 more. `core_two` is that call;
/// `own_two` is the same form on a struct of the crate (`size_of::<W>()` is 2: the ADT's row
/// and the impls of `SizedTypeProperties` for it are made inside the evaluation).
/// Expected lines: "ground Self: core_two: clean", "ground Self: core_three: diags [..]" (a
/// `[u8; 3]` where `[u8; 2]` is expected), "ground Self: own_two: clean", "ground Self:
/// own_three: diags [..]".
#[test]
fn a_length_over_a_ground_self_has_its_value() {
    const TEXT: &str = "struct W(u16);
impl W {
    fn pick(bytes: [u8; core::mem::size_of::<Self>()]) -> u8 { bytes[1] }
}
fn core_two(b: [u8; 2]) -> u16 { u16::from_le_bytes(b) }
fn core_three(b: [u8; 3]) -> u16 { u16::from_le_bytes(b) }
fn own_two(b: [u8; 2]) -> u8 { W::pick(b) }
fn own_three(b: [u8; 3]) -> u8 { W::pick(b) }
fn main() {}
";
    let got = check_fns(TEXT, &["core_two", "core_three", "own_two", "own_three"]);
    expect("ground Self", &got, &[true, false, true, false]);
}

/// State three stays a typed refusal naming the constant, never a value and never clean: the
/// Reference's own example of what is not allowed, "fn bad_function<const N: usize>() ->
/// [u8; {N + 1}]" ("Not allowed to combine in other expressions in types, such as the
/// arithmetic expression in the return type here."), and the same with `Self` in a generic
/// impl, where `Self` is not ground. Expected lines: each outcome contains
/// "a constant expression over a generic parameter".
#[test]
fn a_length_over_an_open_parameter_is_refused_by_name() {
    const TEXT: &str = "struct G<T>(T);
impl<T> G<T> {
    fn pick(bytes: [u8; core::mem::size_of::<Self>()]) -> u8 { bytes[0] }
}
fn plus<const N: usize>() -> [u8; { N + 1 }] { loop {} }
fn open_self(b: [u8; 4]) -> u8 { G::<u32>::pick(b) }
fn main() {}
";
    let got = check_fns(TEXT, &["plus", "open_self"]);
    for (n, o) in &got {
        println!("open: {n}: {o}");
    }
    for (n, o) in &got {
        assert!(o.contains("a constant expression over a generic parameter"), "`{n}`: {got:?}");
    }
}

/// Face (b): an impl header whose array length calls `size_of` at a ground type is a
/// candidate with the length's value. Package 93 (zerocopy), src/byteorder.rs:804, as the
/// walk printed its header: "( mem:: size_of::< usize> ( ) )" in the array length of the
/// impl's trait argument. It was a refused header ("the trait's constant .. is read with no
/// `Self`"): `size_of::<T>()` is "<T as SizedTypeProperties>::SIZE" (the pinned core,
/// library/core/src/mem/mod.rs), and the evaluator read that path at no arguments. `usize`
/// is 8 bytes on every host blaze targets (arm64 and x86-64). Expected lines: "header
/// length: eight: clean", "header length: seven: diags [..]" (no `From<[u8; 7]>` for `W`).
#[test]
fn a_header_length_that_calls_size_of_has_its_value() {
    const TEXT: &str = "use core::mem;
struct W([u8; 8]);
impl From<[u8; mem::size_of::<usize>()]> for W {
    fn from(a: [u8; mem::size_of::<usize>()]) -> W { W(a) }
}
fn eight(a: [u8; 8]) -> W { W::from(a) }
fn seven(a: [u8; 7]) -> W { W::from(a) }
fn main() {}
";
    let got = check_fns(TEXT, &["eight", "seven"]);
    expect("header length", &got, &[true, false]);
}

/// Face (c): an array repeat length that is a name of the BODY. A const parameter of the fn
/// is state two (`param`). A `const` item declared in the body is found through the body's
/// own items and evaluated: blaze-core, src/spans.rs: "const MOST: usize =
/// crate::threads::RUN_MOST;" then "[Resolved { at: NonNull::dangling(), len: 0 }; MOST]"
/// (`real` mirrors it). It was "to_typeck refused Form { what: \"an array repeat length\" }
/// (Resolve .. NotFound)": the name was looked up at the module. The Reference, "Statements",
/// "Item declarations": "Declaring an item within a statement block restricts its scope to
/// the block containing the statement"; "Names", "Scopes": a block's item shadows the outer
/// names, so in `own` and `shadowed` the length is the body's `LEN` (4), not the module's
/// (9). Expected lines: "body names: param: clean", "body names: real: clean", "body names:
/// own: clean", "body names: shadowed: diags [..]" (a `[u8; 4]` where `[u8; 9]` is written).
#[test]
fn a_repeat_length_names_the_bodys_own_constant() {
    const TEXT: &str = "const LEN: usize = 9;
const RUN_MOST: usize = 8;
fn param<const N: usize>() -> [u8; N] { [0; N] }
fn real() -> u8 { const MOST: usize = crate::RUN_MOST; let taken = [0u8; MOST]; taken[7] }
fn own() -> [u8; 4] { const LEN: usize = 4; [0; LEN] }
fn shadowed() -> [u8; 9] { const LEN: usize = 4; [0; LEN] }
fn main() {}
";
    let got = check_fns(TEXT, &["param", "real", "own", "shadowed"]);
    expect("body names", &got, &[true, true, true, false]);
}

/// Face (d): an array repeat length that calls `size_of` at a ground type has its value. The
/// pinned core, library/core/src/num/mod.rs, in `impl usize`: "pub(crate) const fn
/// repeat_u8(x: u8) -> usize {" "usize::from_ne_bytes([x; size_of::<usize>()])" (`real`
/// mirrors it: `from_ne_bytes` takes "[u8; size_of::<Self>()]", face (a), so both lengths
/// must be 8 for it to check). It was a `check gap: BodyConst` ("an anonymous constant in a
/// body .. or the evaluator refused it"). Expected lines: "repeat size_of: eight: clean",
/// "repeat size_of: real: clean", "repeat size_of: seven: diags [..]" (a `[u8; 8]` where
/// `[u8; 7]` is written).
#[test]
fn a_repeat_length_that_calls_size_of_has_its_value() {
    const TEXT: &str = "fn eight(x: u8) -> [u8; 8] { [x; core::mem::size_of::<usize>()] }
fn real(x: u8) -> usize { usize::from_ne_bytes([x; core::mem::size_of::<usize>()]) }
fn seven(x: u8) -> [u8; 7] { [x; core::mem::size_of::<usize>()] }
fn main() {}
";
    let got = check_fns(TEXT, &["eight", "real", "seven"]);
    expect("repeat size_of", &got, &[true, true, false]);
}

/// Wave 2311 C04 (tempo census 7, `check gap: BodyConst`: `Arg: AnonInBody` 61, `RepeatLen:
/// AnonInBody` 22): an anonymous constant written in a body. The Reference, "Constant
/// evaluation", "Const context": "Const generic arguments" and "Array repeat length
/// expressions" are const contexts. (1) A const argument on a path to a fn takes its
/// parameter's type: the pinned core's `enc_16lsd::<{ U128_MAX_DEC_N - 16 }>(buf, mod_1e16)`
/// (library/core/src/fmt/num.rs, `impl u128`'s `_fmt_inner`) is a `usize` constant, which
/// `arg_six` mirrors; it was a hole the check reported as a gap. (2) A repeat length that
/// names a constant declared in the body is read in that block (the Reference, "Statements",
/// "Item declarations"): alloy-primitives' `FixedBytes` `Display` writes "const SEP_LEN: usize
/// = '…'.len_utf8(); let mut buf = [0; 2 + 4 + SEP_LEN + 4];", which `local_six` mirrors; read
/// at the module, `SEP` named nothing and the evaluator refused it. A version that left either
/// a gap makes the clean ones "not checked"; one that made either a hole makes `arg_seven` or
/// `local_five` clean. Expected lines: "body anon: arg_six: clean", "body anon: arg_seven:
/// diags [..]" (a `[u8; 7]` where `[u8; 6]` is the parameter's), "body anon: local_six:
/// clean", "body anon: local_five: diags [..]" (a `[u8; 6]` where `[u8; 5]` is written).
#[test]
fn an_anonymous_constant_in_a_body_has_its_value() {
    const TEXT: &str = "const BASE: usize = 7;
fn take<const N: usize>(a: [u8; N]) -> usize { a.len() }
fn arg_six() -> usize { take::<{ BASE - 1 }>([0u8; 6]) }
fn arg_seven() -> usize { take::<{ BASE - 1 }>([0u8; 7]) }
fn local_six() -> [u8; 6] { const SEP: usize = 2; [0u8; 2 + SEP + 2] }
fn local_five() -> [u8; 5] { const SEP: usize = 2; [0u8; 2 + SEP + 2] }
fn main() {}
";
    let got = check_fns(TEXT, &["arg_six", "arg_seven", "local_six", "local_five"]);
    expect("body anon", &got, &[true, false, true, false]);
}

/// Tempo census 13, brief 11 (`ConstFail::TraitConstNoSelf`: "the constant .. an array length
/// names: TraitConstNoSelf"): a constant whose BODY reads a trait's constant by a qualified
/// path. The Reference, "Paths", "Qualified paths": "Fully qualified paths allow for
/// disambiguating the path for trait implementations and for specifying canonical paths": the
/// path itself says `Self`, so the evaluator reads the trait's `N` at `[u8]` and the impl is
/// selected (blaze-consteval machine/code.rs `qualified_const_args`; the same step that gives
/// `size_of::<usize>()` its value). `four` must stay an error of the program (a `[u8; 3]`
/// where `[u8; 4]` is written). NOT covered here, and still `TraitConstNoSelf`: a path the
/// checker selects by type (`Self::N`, `T::N`, `<u8>::N`), which needs the checker's
/// `Checked::const_args` read by the evaluator. Expected lines: "trait constant: three:
/// clean", "trait constant: four: diags [..]".
#[test]
fn a_constant_whose_body_reads_a_trait_constant_has_its_value() {
    const TEXT: &str = "trait Tr { const N: usize; }
impl Tr for u8 { const N: usize = 3; }
const A: usize = <u8 as Tr>::N;
fn three() -> [u8; 3] { [0u8; A] }
fn four() -> [u8; 4] { [0u8; A] }
fn main() {}
";
    let got = check_fns(TEXT, &["three", "four"]);
    expect("trait constant", &got, &[true, false]);
}

/// Tempo census 13, brief 14 (`RefusedImpl [From]: the type alias `U64` is refused: ..
/// Nested`, 92 bodies): crypto_bigint writes `const BYTES` in one `impl Limb` and `fn
/// from_be_bytes(bytes: [u8; Self::BYTES])` in another (limb.rs, limb/encoding.rs). The
/// Reference, "Implementations": "A type can also have multiple inherent implementations";
/// "Associated Items": "The path to an associated item is any path to the implementing type,
/// followed by the associated item's identifier as the final path component": finding
/// `L::from_be` by name reads the impls' names, and evaluating `Self::BYTES` (8) needs no
/// signature of `from_be`. It was "constant ..: an array length needs it while another
/// constant is evaluated" (`ConstFail::Nested`): the impl's head made every fn's signature,
/// so the name lookup inside `Self::BYTES`'s evaluation needed `Self::BYTES` again. Planted
/// control: with the eager loop in heads.rs `Store::impl_` back, `eight` is "not checked ..
/// Nested" and this fails. Expected lines: "sibling const: eight: clean", "sibling const:
/// seven: diags [..]" (a `[u8; 7]` where `[u8; 8]` is expected).
#[test]
fn an_inherent_length_over_a_sibling_impls_const_has_its_value() {
    const TEXT: &str = "struct L(u64);
impl L { pub const BYTES: usize = 8; }
impl L { fn from_be(b: [u8; Self::BYTES]) -> u8 { b[0] } }
fn eight(b: [u8; 8]) -> u8 { L::from_be(b) }
fn seven(b: [u8; 7]) -> u8 { L::from_be(b) }
fn main() {}
";
    let got = check_fns(TEXT, &["eight", "seven"]);
    expect("sibling const", &got, &[true, false]);
}

/// The other side of brief 14: a signature that REALLY needs itself stays refused, by name.
/// `h`'s signature needs `Self::N`, and `N`'s body names `Self::h` (a value of `h`'s fn item
/// type, which is `h`'s signature): the Reference, "Constant evaluation", "Constant
/// expressions": "Paths to functions and constants. Recursively defining constants is not
/// allowed." Making signatures one at a time must not turn this into a value: the length
/// `Self::N` is met again inside its own evaluation. Expected line: "own cycle: cyc: not
/// .." naming the length's constant by its text (`Self::N`, rule 19: check.rs `key_shown`).
#[test]
fn a_signature_whose_length_needs_that_fn_is_refused_by_name() {
    const TEXT: &str = "struct L(u64);
impl L { const N: usize = { let _f = Self::h; 1 }; }
impl L { fn h(b: [u8; Self::N]) -> u8 { b[0] } }
fn cyc(b: [u8; 1]) -> u8 { L::h(b) }
fn main() {}
";
    let got = check_fns(TEXT, &["cyc"]);
    for (n, o) in &got {
        println!("own cycle: {n}: {o}");
    }
    let (_, o) = &got[0];
    assert!(o.starts_with("not "), "`cyc` must be refused, not clean and not a diagnostic: {o}");
    assert!(o.contains("Self::N"), "the refusal must name the length's constant by its text: {o}");
}

/// A trait's associated constant of a GENERIC impl is the solver's selection (front2/
/// const_select.rs through `blaze_typeck::resolve_instance`). The Reference, "Associated
/// Items", "Associated Constants": "Associated constants are constants associated with a
/// type", and "An associated constant definition defines a constant associated with a type";
/// its example "Using default values" (`impl ConstantIdDefault for Struct {}` gives
/// `<Struct as ConstantIdDefault>::ID` the trait's `1`). So `<S<S<Z>> as U>::N` is the
/// definition of the impl that holds for `S<S<Z>>` (`impl<T: U> U for S<T>`, at `T` =
/// `S<Z>`), `<S<Z> as U>::N + 1`, again that impl at `T` = `Z`, `<Z as U>::N + 1 + 1`, which
/// is `0 + 1 + 1` = 2; and `<S<Z> as D>::K` is the trait's default 5, the generic impl writing
/// none. The selector before this took only an impl with no generics or `impl<T> Tr for T`
/// and refused both with `NoImplConst`; a selector that took the first impl of the trait
/// (`Z`'s, 0) or the trait's default in place of an impl's gives a length that is not 2, so
/// `two` is not clean; one that made the length a hole makes `three` clean.
/// Expected lines: "generic impl: two: clean", "generic impl: three: diags [..]" (a `[u8; 2]`
/// returned where `[u8; 3]` is written), "generic impl: five: clean".
#[test]
fn a_constant_of_a_generic_impl_is_selected_by_the_solver() {
    const TEXT: &str = "trait U { const N: usize; }
trait D { const K: usize = 5; }
struct Z;
struct S<T>(T);
impl U for Z { const N: usize = 0; }
impl<T: U> U for S<T> { const N: usize = T::N + 1; }
impl<T> D for S<T> {}
fn two(a: [u8; <S<S<Z>> as U>::N]) -> [u8; 2] { a }
fn three(a: [u8; <S<S<Z>> as U>::N]) -> [u8; 3] { a }
fn five(a: [u8; <S<Z> as D>::K]) -> [u8; 5] { a }
fn main() {}
";
    let got = check_fns(TEXT, &["two", "three", "five"]);
    expect("generic impl", &got, &[true, false, true]);
}

/// Wave 2311 C09 (tempo census 7, "a written type" `NotFound`, form A): a turbofish argument
/// that is a `const` declared in the body. The pinned core's `swap_nonoverlapping_bytes`
/// (library/core/src/ptr/mod.rs) writes `const CHUNK_SIZE: usize = size_of::<*const ()>();`
/// then `swap_nonoverlapping_chunks::<CHUNK_SIZE>(..)`; `g` mirrors it. It was refused as a
/// written type: the name was looked up only at the module (front2/body.rs `name_arg`, now
/// asking const_open.rs `local_arg` first). The Reference, "Generic parameters", "Const
/// generics": "When there is ambiguity if a generic argument could be resolved as either a
/// type or const argument, it is always resolved as a type", so in `typed` the module's
/// `struct T` is the argument, not the body's `const T`; a version that read the local const
/// first makes `typed` an error. A version that made `C` a hole makes `g_five` clean.
/// Expected lines: "local const arg: g: clean", "local const arg: g_five: diags [..]" (a
/// `[u8; 4]` where `[u8; 5]` is written), "local const arg: typed: clean".
#[test]
fn local_const_as_turbofish_argument() {
    const TEXT: &str = "struct T;
fn f<const N: usize>() -> [u8; N] { [0; N] }
fn size<X>() -> usize { core::mem::size_of::<X>() }
fn g() -> [u8; 4] { const C: usize = 4; f::<C>() }
fn g_five() -> [u8; 5] { const C: usize = 4; f::<C>() }
fn typed() -> usize { const T: usize = 4; size::<T>() }
fn main() {}
";
    let got = check_fns(TEXT, &["g", "g_five", "typed"]);
    expect("local const arg", &got, &[true, false, true]);
}

/// Wave 2311 C09 (tempo census 7, "a written type" `NotFound`, form B): a type an expansion
/// writes that names a type declared in the body. time 0.3.55 writes `struct Digits { .. }`
/// then `core::mem::offset_of!(Digits, digit_1)` in a body (`h` mirrors it), and `type
/// JulianDay = ..;` then `ensure_ranged!(JulianDay: julian_day)` (`k` mirrors it). The
/// Reference, "Macros By Example", "Hygiene": macro_rules! "local variables are looked up at
/// the macro definition site while other symbols are looked up at the macro invocation site",
/// so the type is read at the call's block (front2/body.rs `env_at` through `View::site`);
/// read at the body's module, `S` and `T` were `NotFound`. A version that made the type a hole
/// makes `k_wide` clean. Expected lines: "expansion type: h: clean", "expansion type: k:
/// clean", "expansion type: k_wide: diags [..]" (a `u8` where `u32` is written).
#[test]
fn expansion_type_reads_at_call_block() {
    const TEXT: &str = "macro_rules! mk { ($t:ty) => { <$t>::default() } }
fn h() -> usize { struct S { a: u8, b: u32 } core::mem::offset_of!(S, b) }
fn k() -> u8 { type T = u8; mk!(T) }
fn k_wide() -> u32 { type T = u8; mk!(T) }
fn main() {}
";
    let got = check_fns(TEXT, &["h", "k", "k_wide"]);
    expect("expansion type", &got, &[true, true, false]);
}
