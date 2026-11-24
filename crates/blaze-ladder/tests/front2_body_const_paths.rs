//! Constants in type position inside bodies, as tempo's build census found them left out
//! (`check gap: BodyConst`, census g045941: 128 fns):
//! - alloy-sol-types' `<U96>::from_be_bytes::<{ U96::BYTES }>(..)`, 104 of them over the
//!   `U24`..`U256` and `I24`..`I256` aliases (`Arg: AnonInBody("U96 :: BYTES")`): the const
//!   argument's parameter is an associated fn's after an anchor, so its type is not known where
//!   the body is converted, and an anonymous constant of an unknown type was refused;
//! - der's `impl DecodeValue for u128`, 10 (`RepeatLen: OverGeneric`): "const
//!   UNSIGNED_HEADROOM: usize = 1; let mut buf = [0u8; (Self::BITS as usize / 8) +
//!   UNSIGNED_HEADROOM];": a constant of the body over a GROUND `Self` was taken for one over a
//!   generic parameter.
//!
//! The rules: the Reference, "Constant evaluation", "Const context": array type lengths,
//! array repeat lengths and const generic arguments are const contexts, evaluated as a
//! constant item is; "Generic parameters", "Const generics": a const argument is a value of
//! its parameter's type, and "As a further restriction, const parameters may only appear as
//! a standalone argument inside of a type or array repeat expression" (so an expression over a
//! generic parameter has no value: the control). "Array types": two array types are equal when
//! their lengths are, so a wrong length is a type mismatch of the program.
//!
//! Every shape has a fn that must check clean AND one with the wrong length that must be a
//! diagnostic of the program: a hole (a const variable takes any value) passes neither.
//! It needs no toolchain: the program is `#![no_core]`.

#![feature(allocator_api)]

use std::alloc::Global;

use blaze_ladder::crates::{Inputs, Toolchain};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::body::{BodyError, lower_fn, lower_member};
use blaze_ladder::front2::check::{BuildOrder, CheckFail, Records, check_body, eval_len};
use blaze_ladder::front2::consts::Consts;
use blaze_ladder::front2::heads::{HeadError, Store, Want};
use blaze_ladder::front2::keys::{MemberKind, checker_of};
use blaze_ladder::front2::program::Front2Program;
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::front2::types::Why;
use blaze_ladder::program_texts::ProgramTexts;
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{NewFront, build_survey};
use blaze_lower::scoped::Prepares;
use blaze_own::Alloc;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};

/// The lang items as tests/front2_promoted_value.rs models them, and `Uint` as
/// ruint's (alloy-primitives aliases.rs: `pub type U96 = Uint<96, 2>;`), `BYTES` its width.
const PRELUDE: &str = r#"#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = "pointee_sized"]
trait PointeeSized {}

#[lang = "meta_sized"]
trait MetaSized: PointeeSized {}

#[lang = "sized"]
trait Sized: MetaSized {}

#[lang = "copy"]
trait Copy {}

pub struct Uint<const BITS: usize, const LIMBS: usize> {
    limbs: [u64; LIMBS],
}

impl<const BITS: usize, const LIMBS: usize> Uint<BITS, LIMBS> {
    pub const BYTES: usize = BITS;
    pub const LIMBS: usize = LIMBS;
    pub fn from_be_bytes<const B: usize>(b: [u8; B]) -> [u8; B] {
        b
    }
}

pub type U12 = Uint<12, 2>;

fn pass<const B: usize>(b: [u8; B]) -> [u8; B] {
    b
}
"#;

/// The most constants one fn's lowering may need evaluated before it converts.
const MOST_LENS: u32 = 8;

/// Each fn of `fns` in the crate `PRELUDE` + `text`, lowered and checked in order over one
/// store with the driver's round for a constant a body needs (`Why::NeedConst`: `eval_len`,
/// then the body lowered again), as tests/front2_const_open.rs does with the real core: its
/// name and outcome ("clean", the diagnostics, or the failure with its gaps).
fn check_fns(text: &str, fns: &[&str]) -> Vec<(String, String)> {
    let program = format!("{PRELUDE}{text}");
    let files = [SourceFile { path: "src/main.rs", text: &program }];
    let tc = Toolchain::none(Alloc::global());
    let new = NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::On };
    let texts = ProgramTexts::new(&files, "src/main.rs", new, &tc, blaze_build::Panic::Unwind).unwrap_or_else(|e| panic!("the program's texts: {e}"));
    let user = texts.user();
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame");
    let mut front = Front2::new(&names, &frame, &files, Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut store = Store::new(front.index.lang("sized"), front.frame, Alloc::global());
    let (mut buckets, mut consts) = (None, Consts::default());
    let mut got = Vec::new();
    for &n in fns {
        let found = names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: &[n] }, Alloc::global()).unwrap_or_else(|e| panic!("`{n}`: {e}"));
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
                            let d: Vec<String> = done.diags.iter().map(|d| format!("{} at {:?}", d.code.code(), d.at)).collect();
                            format!("diags {d:?}")
                        }
                        Err(CheckFail::Diags { diags, .. }) => format!("diags {diags:?}"),
                        Err(e) => format!("not checked: {e} ({e:?})"),
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

/// An array TYPE's length `U12::BYTES` in a body (a `let`'s type): 12. `[0u8; 13]` there is
/// a mismatch.
/// Expected lines: "body array type: body_ty: clean", "body array type: body_ty_wrong: diags [..]".
#[test]
fn an_array_type_in_a_body() {
    const TEXT: &str = "
fn body_ty() -> [u8; 12] {
    let a: [u8; U12::BYTES] = [0u8; 12];
    a
}

fn body_ty_wrong() -> [u8; 12] {
    let a: [u8; U12::BYTES] = [0u8; 13];
    a
}
";
    expect("body array type", &check_fns(TEXT, &["body_ty", "body_ty_wrong"]), &[true, false]);
}

/// The census shape: a turbofish const argument `{ U12::BYTES }` of an associated fn after
/// an anchor (`<U12>::from_be_bytes::<..>`), whose parameter's type is not known where the
/// body is converted, and the same on a free fn (its parameter's type known). The value is
/// 12, so the result is `[u8; 12]`; declared `[u8; 13]` it is a mismatch.
/// Expected lines: "turbofish: anchored: clean", "turbofish: anchored_wrong: diags [..]",
/// "turbofish: free: clean", "turbofish: free_wrong: diags [..]".
#[test]
fn a_const_argument_in_a_turbofish() {
    const TEXT: &str = "
fn anchored(b: [u8; 12]) -> [u8; 12] {
    <U12>::from_be_bytes::<{ U12::BYTES }>(b)
}

fn anchored_wrong(b: [u8; 12]) -> [u8; 13] {
    <U12>::from_be_bytes::<{ U12::BYTES }>(b)
}

fn free(b: [u8; 12]) -> [u8; 12] {
    pass::<{ U12::BYTES }>(b)
}

fn free_wrong(b: [u8; 12]) -> [u8; 13] {
    pass::<{ U12::BYTES }>(b)
}
";
    expect("turbofish", &check_fns(TEXT, &["anchored", "anchored_wrong", "free", "free_wrong"]), &[true, false, true, false]);
}

/// An impl header's const argument `{ <U12>::LIMBS }` (p256's `impl MontyFieldParams<{
/// <U256>::LIMBS }> for FieldParams`): the impl is `Limbs<2>`, so a bound `Limbs<2>` holds
/// and `Limbs<3>` does not (E0277).
/// Expected lines: "impl header: header: clean", "impl header: header_wrong: diags [..]".
#[test]
fn a_const_argument_in_an_impl_header() {
    const TEXT: &str = "
trait Limbs<const N: usize> {}

struct P;

impl Limbs<{ <U12>::LIMBS }> for P {}

fn needs_two<T: Limbs<2>>() {}

fn needs_three<T: Limbs<3>>() {}

fn header() {
    needs_two::<P>()
}

fn header_wrong() {
    needs_three::<P>()
}
";
    expect("impl header", &check_fns(TEXT, &["header", "header_wrong"]), &[true, false]);
}

/// der's shape: a repeat length over a GROUND `Self` and a `const` of the body. `Self` is
/// `Word`, whose `W` is 4; `second(Self::W, HEAD)` is `HEAD`, 6 (no operator: `#![no_core]`
/// has no `Add`). Declared `[u8; 7]` it is a mismatch. A trait's `Self` is a parameter: the
/// same expression in the trait's default body has no value (the control below).
/// Expected lines: "ground Self repeat: Decode for Word::buf: clean", "ground Self repeat:
/// Decode for Word::buf_wrong: diags [..]".
#[test]
fn a_repeat_over_a_ground_self_and_a_body_const() {
    const TEXT: &str = "
const fn second(_a: usize, b: usize) -> usize {
    b
}

struct Word;

impl Word {
    const W: usize = 4;
}

trait Decode {
    fn buf() -> [u8; 6];
    fn buf_wrong() -> [u8; 7];
}

impl Decode for Word {
    fn buf() -> [u8; 6] {
        const HEAD: usize = 6;
        [0u8; second(Self::W, HEAD)]
    }
    fn buf_wrong() -> [u8; 7] {
        const HEAD: usize = 6;
        [0u8; second(Self::W, HEAD)]
    }
}
";
    let members = check_members(TEXT, "Decode for Word", &["buf", "buf_wrong"]);
    expect("ground Self repeat", &members, &[true, false]);
}

/// Each member `fns` of the first impl of the crate whose header holds `ty`, checked as
/// [`check_fns`] checks a free fn.
fn check_members(text: &str, ty: &str, fns: &[&str]) -> Vec<(String, String)> {
    let program = format!("{PRELUDE}{text}");
    let files = [SourceFile { path: "src/main.rs", text: &program }];
    let tc = Toolchain::none(Alloc::global());
    let new = NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::On };
    let texts = ProgramTexts::new(&files, "src/main.rs", new, &tc, blaze_build::Panic::Unwind).unwrap_or_else(|e| panic!("the program's texts: {e}"));
    let user = texts.user();
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame");
    let mut front = Front2::new(&names, &frame, &files, Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut store = Store::new(front.index.lang("sized"), front.frame, Alloc::global());
    let (mut buckets, mut consts) = (None, Consts::default());
    // The impl whose header names `ty` (as tests/front2_self_ty.rs finds an impl's members).
    let mut imp = None;
    for (k, _, _) in names.items(0) {
        if names.item_text(k).and_then(|t| t.impl_header).is_some_and(|h| h.contains(ty)) {
            imp = Some(k);
            break;
        }
    }
    let imp = imp.unwrap_or_else(|| panic!("an impl whose header names `{ty}`"));
    store.ensure(&names, &mut front.keys, Want::Impl(imp)).unwrap_or_else(|e| panic!("the impl for `{ty}`: {e:?}"));
    let mut got = Vec::new();
    for &n in fns {
        let Some(k) = front.keys.filed_member(checker_of(imp), MemberKind::Fn, n) else {
            got.push((format!("{ty}::{n}"), "not filed as a member of the impl".to_string()));
            continue;
        };
        let mut lens = 0;
        let outcome = loop {
            match lower_member(&names, prepared, &mut front.keys, &mut store, k) {
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
                            let d: Vec<String> = done.diags.iter().map(|d| format!("{} at {:?}", d.code.code(), d.at)).collect();
                            format!("diags {d:?}")
                        }
                        Err(CheckFail::Diags { diags, .. }) => format!("diags {diags:?}"),
                        Err(e) => format!("not checked: {e} ({e:?})"),
                    };
                }
            }
        };
        got.push((format!("{ty}::{n}"), outcome));
    }
    got
}

/// The control: a const argument over a generic TYPE parameter (`{ Wrap::<T>::N }`) has no
/// value (the Reference, "Const generics": "const parameters may only appear as a standalone
/// argument"; a generic parameter in a const argument's expression is refused by rustc,
/// "generic parameters may not be used in const operations"). It stays a gap named
/// `OverGeneric`, never a value and never a clean check.
/// Expected line: "over a generic: over: not checked: .. OverGeneric ..".
#[test]
fn a_const_argument_over_a_type_parameter_stays_refused() {
    const TEXT: &str = "
struct Wrap<T>(T);

impl<T> Wrap<T> {
    const N: usize = 12;
}

fn over<T>(b: [u8; 12]) -> [u8; 12] {
    pass::<{ Wrap::<T>::N }>(b)
}
";
    let got = check_fns(TEXT, &["over"]);
    for (n, o) in &got {
        println!("over a generic: {n}: {o}");
    }
    let (_, o) = &got[0];
    assert!(o.starts_with("not checked") && o.contains("OverGeneric"), "an expression over a type parameter stays the OverGeneric gap: {o}");
}

/// A repeat whose length IS the fn's const parameter, built at two instances: `zeros::<2>`
/// and `zeros::<3>` each build (the length is evaluated at the instance's arguments), and
/// `bytes::<..>` with `U12::BYTES` as its argument from a body builds too.
/// Prints: `BUILDS: <n> of <n> instances (0 refused: )`, with at least five built.
#[test]
fn a_repeat_of_a_const_parameter_builds_at_two_instances() {
    const TEXT: &str = "
fn zeros<const N: usize>() -> [u8; N] {
    [0u8; N]
}

fn bytes(b: [u8; 12]) -> [u8; 12] {
    <U12>::from_be_bytes::<{ U12::BYTES }>(b)
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let _a = zeros::<2>();
    let _b = zeros::<3>();
    let _c = bytes(zeros::<12>());
    0
}
";
    let program = format!("{PRELUDE}{TEXT}");
    let files = [SourceFile { path: "src/main.rs", text: &program }];
    let tc = Toolchain::none(Alloc::global());
    let new = NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::On };
    let texts = ProgramTexts::new(&files, "src/main.rs", new, &tc, blaze_build::Panic::Unwind).unwrap_or_else(|e| panic!("the program's texts: {e}"));
    let user = texts.user();
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the survey's frame");
    let mut left_out = Vec::new_in(Global);
    let mut program = Front2Program::survey(&names, &frame, &files, &mut left_out, Alloc::global()).unwrap_or_else(|e| panic!("BUILDS: no survey: {e}"));
    let left: Vec<String> = left_out.iter().map(|(f, class)| format!("{f}: {class}")).collect();
    assert!(left.is_empty(), "front2 tables every fn of the program; left out: {left:?}");
    let mut refused = Vec::new_in(Global);
    let counts = build_survey(&mut program, &mut refused, blaze_build::OverflowChecks::On, Alloc::global()).unwrap_or_else(|e| panic!("BUILDS: the survey stopped: {e}"));
    let rows: Vec<String> = refused.iter().map(|(f, class)| format!("{f}: {class}")).collect();
    println!("BUILDS: {} of {} instances ({} refused: {})", counts.built, counts.tried, rows.len(), rows.join("; "));
    assert!(rows.is_empty(), "every instance builds: {rows:?}");
    assert!(counts.built >= 5, "`main`, `zeros` at 2, 3 and 12, `bytes` and `from_be_bytes` at 12 built: {} of {}", counts.built, counts.tried);
}

/// Method calls and anchored associated fns with unsuffixed anonymous const arguments
/// (`reader.read::<{ 256 / 8 }>()`, `<U12>::from_be_bytes::<{ 24 / 2 }>(b)`, `s.tag::<{ 256 / 8 }>()`,
/// `s.neg::<{ -4 }>()`): `to_typeck` does not know the callee's const parameter type yet, so
/// `check_body` learns it from the resolved callee, evaluates the constant at that declared
/// type, and re-checks with the real `ConstValue`.
#[test]
fn a_method_call_const_arg_with_unsuffixed_expr() {
    const TEXT: &str = "
struct Reader;
impl Reader {
    pub const fn read<const N: usize>(self) -> [u8; N] {
        [0u8; N]
    }
    pub const fn tag<const N: u8>(&self) -> u8 {
        N
    }
    pub const fn neg<const N: i8>(&self) -> i8 {
        N
    }
}

fn method_ok() -> [u8; 32] {
    let r = Reader;
    r.read::<{ 256 / 8 }>()
}

fn method_wrong() -> [u8; 31] {
    let r = Reader;
    r.read::<{ 256 / 8 }>()
}

fn method_unconstrained() {
    let r = Reader;
    let _ = r.read::<{ 256 / 8 }>();
}

fn method_u8_and_neg_i8() -> (u8, i8) {
    let r = Reader;
    (r.tag::<{ 256 / 8 }>(), r.neg::<{ -4 }>())
}

fn anchored_expr(b: [u8; 12]) -> [u8; 12] {
    <U12>::from_be_bytes::<{ 24 / 2 }>(b)
}

fn anchored_expr_wrong(b: [u8; 12]) -> [u8; 13] {
    <U12>::from_be_bytes::<{ 24 / 2 }>(b)
}
";
    expect(
        "method const arg",
        &check_fns(
            TEXT,
            &["method_ok", "method_wrong", "method_unconstrained", "method_u8_and_neg_i8", "anchored_expr", "anchored_expr_wrong"],
        ),
        &[true, false, true, true, true, false],
    );
}

/// The `keccak_const` / `tempo_precompiles::tip20::ISSUER_ROLE` shape: a `const` item calls
/// a macro-expanded `const fn finalize(&self) -> [u8; { 256 / 8 }]` whose body calls
/// `reader.read::<{ 256 / 8 }>()`. Evaluating `ROLE` inside `Consts::value` checks `finalize`
/// while `r.consts` is `None` (`evaluating > 0`), so the deferred anonymous constant must
/// unwind via `const_nested_fail` to `Consts::value_listed` and evaluate cleanly.
#[test]
fn a_const_item_calling_const_fn_with_method_const_arg_evaluates() {
    const TEXT: &str = "
struct Reader;
impl Reader {
    pub const fn read<const N: usize>(self) -> (Self, [u8; N]) {
        (self, [7u8; N])
    }
}

macro_rules! mk_hasher {
    ($name:ident, $security:literal) => {
        pub struct $name;
        impl $name {
            pub const fn new() -> Self { $name }
            pub const fn finalize(&self) -> [u8; { $security / 8 }] {
                let reader = Reader;
                let (_, output) = reader.read::<{ $security / 8 }>();
                output
            }
        }
    };
}

mk_hasher!(Keccak256, 256);

pub const ROLE: [u8; 32] = Keccak256::new().finalize();

fn use_role() -> [u8; 32] {
    ROLE
}
";
    expect("const item method arg", &check_fns(TEXT, &["use_role"]), &[true]);
    // Also evaluate `ROLE` directly through `Consts::value` (the exact `ISSUER_ROLE` path).
    let heap = blaze_own::testing::heap();
    let alloc = heap.alloc();
    let program = format!("{PRELUDE}{TEXT}");
    let files = [SourceFile { path: "src/main.rs", text: &program }];
    let tc = Toolchain::none(alloc);
    let new = NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::On };
    let texts = ProgramTexts::new(&files, "src/main.rs", new, &tc, blaze_build::Panic::Unwind).unwrap();
    let user = texts.user();
    let inputs = Inputs::new(&user, &tc).unwrap();
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap();
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).unwrap();
    let mut front = Front2::new(&names, &frame, &files, alloc).unwrap();
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut store = Store::new(front.index.lang("sized"), front.frame, alloc);
    let (mut buckets, mut consts) = (None, Consts::default());
    let found = names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: &["ROLE"] }, alloc).unwrap();
    let k = found.whole(1).unwrap().key;
    store.ensure(&names, &mut front.keys, Want::Fn(k)).unwrap();
    let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: None };
    let rec = consts.value(&mut records, prepared, checker_of(k)).unwrap_or_else(|e| panic!("evaluating ROLE failed: {e}"));
    assert_eq!(rec.data.allocs[0].bytes.as_slice(), &[7u8; 32], "ROLE must evaluate to [7u8; 32]");
}

/// The `evm2::interpreter::dispatch::make_selector_tables` shape: a written type in a
/// macro-expanded generic `const fn` names a generic associated type with const arguments on
/// a type parameter (`F::Config<{ crate::SpecId::FRONTIER as u32 }, CUSTOM_SPEC_ID>`), as
/// well as qualified (`<F as Selector<T>>::Config<...>`, `<F>::Config<...>`) and `Self`
/// (`Self::Config<...>`) forms.
#[test]
fn a_gat_const_arg_on_a_type_param_self_or_qualified_type_lowers() {
    const TEXT: &str = "
#[repr(u32)]
pub enum SpecId {
    Frontier = 0,
    Homestead = 1,
}

pub trait Config<T> {
    const CODE: u32;
}

pub trait Selector<T> {
    type Config<const BASE: u32, const CUSTOM: u32>: Config<T>;
}

pub struct BaseConfig<const BASE: u32, const CUSTOM: u32>;
impl<T, const BASE: u32, const CUSTOM: u32> Config<T> for BaseConfig<BASE, CUSTOM> {
    const CODE: u32 = BASE;
}

pub struct BaseSelector;
impl<T> Selector<T> for BaseSelector {
    type Config<const BASE: u32, const CUSTOM: u32> = BaseConfig<BASE, CUSTOM>;
}

impl BaseSelector {
    pub const fn self_check<const CUSTOM: u32>() -> u32 {
        make_entry::<(), <Self as Selector<()>>::Config<{ crate::SpecId::Homestead as u32 }, CUSTOM>>()
    }
}

pub const fn make_entry<T, C: Config<T>>() -> u32 {
    C::CODE
}

pub const fn make_selector_tables<T, F, const CUSTOM: u32>() -> [u32; 2]
where
    F: Selector<T>,
{
    macro_rules! expand_specs {
        ($($spec:ident),*) => {
            [$(make_entry::<T, F::Config<{ crate::SpecId::$spec as u32 }, CUSTOM>>()),*]
        };
    }
    expand_specs!(Frontier, Homestead)
}

pub const fn qualified_forms<T, F, const CUSTOM: u32>() -> [u32; 2]
where
    F: Selector<T>,
{
    [
        make_entry::<T, <F as Selector<T>>::Config<{ crate::SpecId::Frontier as u32 }, CUSTOM>>(),
        make_entry::<T, <F>::Config<{ crate::SpecId::Homestead as u32 }, CUSTOM>>(),
    ]
}

pub const TABLES: [u32; 2] = make_selector_tables::<(), BaseSelector, 13>();

fn check_tables() -> [u32; 2] {
    TABLES
}

fn check_qualified() -> [u32; 2] {
    qualified_forms::<(), BaseSelector, 13>()
}

fn check_self_form() -> u32 {
    BaseSelector::self_check::<13>()
}
";
    expect("gat const arg", &check_fns(TEXT, &["make_selector_tables", "qualified_forms", "check_tables", "check_qualified", "check_self_form"]), &[true, true, true, true, true]);
    // Also evaluate `TABLES` via `Consts::value` to verify end-to-end consteval.
    let heap = blaze_own::testing::heap();
    let alloc = heap.alloc();
    let program = format!("{PRELUDE}{TEXT}");
    let files = [SourceFile { path: "src/main.rs", text: &program }];
    let tc = Toolchain::none(alloc);
    let new = NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::On };
    let texts = ProgramTexts::new(&files, "src/main.rs", new, &tc, blaze_build::Panic::Unwind).unwrap();
    let user = texts.user();
    let inputs = Inputs::new(&user, &tc).unwrap();
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap();
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).unwrap();
    let mut front = Front2::new(&names, &frame, &files, alloc).unwrap();
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut store = Store::new(front.index.lang("sized"), front.frame, alloc);
    let (mut buckets, mut consts) = (None, Consts::default());
    let found = names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: &["TABLES"] }, alloc).unwrap();
    let k = found.whole(1).unwrap().key;
    store.ensure(&names, &mut front.keys, Want::Fn(k)).unwrap();
    let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: None };
    let rec = consts.value(&mut records, prepared, checker_of(k)).unwrap_or_else(|e| panic!("evaluating TABLES failed: {e}"));
    let mut want = [0u8; 8];
    want[0..4].copy_from_slice(&0u32.to_ne_bytes());
    want[4..8].copy_from_slice(&1u32.to_ne_bytes());
    assert_eq!(rec.data.allocs[0].bytes.as_slice(), &want, "TABLES must evaluate to [0u32, 1u32]");
}
