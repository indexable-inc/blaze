//! Default field values BUILD AND RUN (RFC 3681, `default_field_values`; D126, rule 83
//! `options-struct`): `S { a: 1, .. }` is built as `S { a: 1, b: <default of b> }` would be.
//! The pieces: front2/const_at.rs `needs_of` pulls each default (`Checked::default_fields`) as
//! a constant at the struct type's ground arguments; blaze-build walk.rs `init_defaults` stores
//! each into the literal; blaze-consteval machine/code.rs builds a `Defaults` base inside a
//! constant the same way. The checker side is tests/front2_field_defaults.rs.
//!
//! The programs are `#![no_core]` on the blaze edition (the feature on unasked), built and run
//! in this process with no toolchain (`NewFront::shelf` is `None`). `&'static str`'s length is
//! read through a union of the wide pointer and its two words (the layout `str`'s references
//! have: data pointer, then length), as `core` has no `len` here.
//!
//! The values are arithmetic on the defaults as declared (rustc nightly gives the same):
//! - `show(Opts { name: "x", .. })` = 80 + 0 + 1 = 81, `show(Opts { width: 3, name: "yy", .. })`
//!   = 3 + 0 + 2 = 5: status 86;
//! - `const O: Opts = Opts { name: "x", .. };` read by `show(O)`: 81 (consteval's build);
//! - `Buf<const N: usize> { len: usize = N, data: [u8; N] = [0; N] }` at N = 4 and N = 8: the
//!   defaults at each instance's arguments, 4 + 8 = 12.

#![feature(allocator_api)]

use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{FrontChoice, NewFront, run_with};

/// The blaze edition: field defaults are on unasked. Overflow wraps (the `release` profile's):
/// a `no_core` program declares no `panic_const_add_overflow`, and no sum here overflows.
const NEW: NewFront<'static> = NewFront { package: "ladder", edition: "blaze-2026.11", shelf: None, overflow: blaze_build::OverflowChecks::Off };

/// The lang items, `Copy` for what the union holds, and `len` of a `&'static str`.
const HEAD: &str = "#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = \"sized\"]
trait Sized {}

#[lang = \"panic_bounds_check\"]
fn panic_bounds_check(_index: usize, _len: usize) -> ! {
    loop {}
}

#[lang = \"copy\"]
trait Copy {}

impl Copy for usize {}

#[repr(C)]
struct Words {
    data: usize,
    len: usize,
}

impl Copy for Words {}

union Wide {
    s: &'static str,
    words: Words,
}

fn len(s: &'static str) -> u32 {
    unsafe { Wide { s }.words.len as u32 }
}

pub struct Opts {
    pub width: u32 = 80,
    pub wrap: bool = false,
    pub name: &'static str,
}

fn show(o: Opts) -> u32 {
    o.width + o.wrap as u32 + len(o.name)
}
";

/// Builds and runs `head` + `main`: the status, or the refusal on one line.
fn run_head(head: &str, main: &str) -> Result<u8, String> {
    let text = [head, main].concat();
    let files = [SourceFile { path: "src/main.rs", text: &text }];
    run_with(&files, "src/main.rs", None, FrontChoice::New(NEW), blaze_own::testing::heap().shared()).map_err(|e| format!("{e}").replace('\n', " "))
}

/// Two base-less literals of a three-field options struct, each unwritten field its default.
/// Prints `DEFAULTS RUN: status 86`.
#[test]
fn a_defaulted_literal_builds_and_runs() {
    let got = run_head(HEAD, "
#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    (show(Opts { name: \"x\", .. }) + show(Opts { width: 3, name: \"yy\", .. })) as i32
}
");
    println!("DEFAULTS RUN: {got:?}");
    assert_eq!(got, Ok(86));
}

/// A base-less literal inside a constant: evaluated by consteval, read by path.
/// Prints `DEFAULTS IN A CONST: status 81`.
#[test]
fn a_defaulted_literal_in_a_constant() {
    let got = run_head(HEAD, "
const O: Opts = Opts { name: \"x\", .. };

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    show(O) as i32
}
");
    println!("DEFAULTS IN A CONST: {got:?}");
    assert_eq!(got, Ok(81));
}

/// Defaults naming the ADT's const parameter, at two arguments: each its own value.
/// Prints `GENERIC DEFAULTS: status 12`.
#[test]
fn generic_defaults_at_two_arguments() {
    let got = run_head(HEAD, "
pub struct Buf<const N: usize> {
    pub len: usize = N,
    pub data: [u8; N] = [0; N],
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let four: Buf<4> = Buf { .. };
    let eight: Buf<8> = Buf { .. };
    (four.len + eight.len) as i32
}
");
    println!("GENERIC DEFAULTS: {got:?}");
    assert_eq!(got, Ok(12));
}

const RECORDS_HEAD: &str = "#![blaze::records]
#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = \"sized\"]
trait Sized {}

#[lang = \"panic_bounds_check\"]
fn panic_bounds_check(_index: usize, _len: usize) -> ! {
    loop {}
}

#[lang = \"copy\"]
trait Copy {}

impl Copy for u8 {}
impl Copy for u32 {}
impl Copy for i32 {}
impl Copy for bool {}
";

const UNIONS_HEAD: &str = "#![blaze::error_unions]
#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = \"sized\"]
trait Sized {}

#[lang = \"panic_bounds_check\"]
fn panic_bounds_check(_index: usize, _len: usize) -> ! {
    loop {}
}

#[lang = \"copy\"]
trait Copy {}

impl Copy for u8 {}
impl Copy for u32 {}
impl Copy for i32 {}
impl Copy for bool {}
";

/// Anonymous record literal `{ b: 10u32, a: 32u32 }` passed to `fn sum(r: { a: u32, b: u32 }) -> u32`.
#[test]
fn record_literal_and_field_access() {
    let got = run_head(RECORDS_HEAD, "
fn sum(r: { a: u32, b: u32 }) -> u32 {
    r.a + r.b
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    sum({ b: 10u32, a: 32u32 }) as i32
}
");
    assert_eq!(got, Ok(42));
}

/// Record functional update `let r2 = { a: 50, ..r };` -> `r2.a + r2.b` -> exit 60.
#[test]
fn record_functional_update() {
    let got = run_head(RECORDS_HEAD, "
#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let r: { a: u32, b: u32 } = { a: 5u32, b: 10u32 };
    let r2 = { a: 50u32, ..r };
    (r2.a + r2.b) as i32
}
");
    assert_eq!(got, Ok(60));
}

/// Record destructuring `let { a, b } = r;`, `let { a, .. } = r;`, and fn param `{ a, b }` -> exit 42.
#[test]
fn record_destructuring() {
    let got = run_head(RECORDS_HEAD, "
fn unpack({ a, b }: { a: u32, b: u32 }) -> u32 {
    let r2: { a: u32, b: u32 } = { a: a, b: b };
    let { a: x, b: y } = r2;
    let { a: z, .. } = { a: 2u32, b: 99u32 };
    x + y + z
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    unpack({ b: 10u32, a: 30u32 }) as i32
}
");
    assert_eq!(got, Ok(42));
}

/// Autoderef field access on `&{ a: u32, b: u32 }` and `match &r { { a, b } => *a + *b }` -> exit 42.
#[test]
fn record_autoderef_and_ref_match() {
    let got = run_head(RECORDS_HEAD, "
fn get_a(r: &{ a: u32, b: u32 }) -> u32 {
    r.a
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let r: { a: u32, b: u32 } = { a: 10u32, b: 22u32 };
    let x = get_a(&r);
    let y = match &r {
        { a, b } => *a + *b,
    };
    (x + y) as i32
}
");
    assert_eq!(got, Ok(42));
}

/// Nested records `{ inner: { x: 20u32, y: 22u32 }, tag: 1u8 }` -> exit 43.
#[test]
fn record_nested() {
    let got = run_head(RECORDS_HEAD, "
fn total(r: { inner: { x: u32, y: u32 }, tag: u8 }) -> u32 {
    r.inner.x + r.inner.y + r.tag as u32
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    total({ inner: { x: 20u32, y: 22u32 }, tag: 1u8 }) as i32
}
");
    assert_eq!(got, Ok(43));
}

/// Union injection `A -> A | B`, typed pattern match, and single-member residual narrowing.
#[test]
fn union_inject_and_single_residual() {
    let body = "
fn eval(x: u32 | bool) -> u32 {
    match x {
        n: u32 => n + 2,
        b => if b { 99 } else { 0 },
    }
}
";
    let got_u32 = run_head(UNIONS_HEAD, &format!(
        "{body}
#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {{
    eval(40u32) as i32
}}
"
    ));
    assert_eq!(got_u32, Ok(42));

    let got_bool = run_head(UNIONS_HEAD, &format!(
        "{body}
#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {{
    eval(true) as i32
}}
"
    ));
    assert_eq!(got_bool, Ok(99));
}

/// Union widening `A | B -> A | B | C`, 3-way typed matching, and multi-member residual narrowing.
#[test]
fn union_widen_and_multi_residual() {
    let defs = "
struct ErrA(u32);
struct ErrB(u32);
struct ErrC(u32);

fn make_ab(which: u32) -> ErrA | ErrB {
    if which == 0 {
        ErrA(11)
    } else {
        ErrB(22)
    }
}

fn widen_abc(which: u32) -> ErrA | ErrB | ErrC {
    if which == 2 {
        ErrC(33)
    } else {
        make_ab(which)
    }
}

fn handle(x: ErrA | ErrB | ErrC) -> u32 {
    match x {
        a: ErrA => a.0 + 1,
        rest => match rest {
            b: ErrB => b.0 + 2,
            c => c.0 + 3,
        },
    }
}
";
    for (which, expected) in [(0u32, 12u8), (1, 24), (2, 36)] {
        let got = run_head(UNIONS_HEAD, &format!(
            "{defs}
#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {{
    handle(widen_abc({which})) as i32
}}
"
        ));
        assert_eq!(got, Ok(expected), "branch {which}");
    }
}

/// Niche-optimized union `&u32 | ()` (non-null reference vs unit).
#[test]
fn union_niche_ref_or_unit() {
    let defs = "
fn unwrap_or_seven(opt: &u32 | ()) -> u32 {
    match opt {
        r: &u32 => *r,
        _ => 7,
    }
}
";
    let got_ref = run_head(UNIONS_HEAD, &format!(
        "{defs}
#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {{
    let x: u32 = 55;
    let opt: &u32 | () = &x;
    unwrap_or_seven(opt) as i32
}}
"
    ));
    assert_eq!(got_ref, Ok(55));

    let got_unit = run_head(UNIONS_HEAD, &format!(
        "{defs}
#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {{
    let opt: &u32 | () = ();
    unwrap_or_seven(opt) as i32
}}
"
    ));
    assert_eq!(got_unit, Ok(7));
}

/// `Result<T, ErrA | ErrB>` with automatic error injection `ErrA -> ErrA | ErrB`.
#[test]
fn union_result_auto_injection() {
    let defs = "
enum Result<T, E> {
    Ok(T),
    Err(E),
}

struct ErrA(u32);
struct ErrB(u32);

fn check(x: u32) -> Result<u32, ErrA | ErrB> {
    if x == 0 {
        Result::Err(ErrA(10))
    } else if x == 1 {
        Result::Err(ErrB(20))
    } else {
        Result::Ok(x)
    }
}

fn dispatch(x: u32) -> u32 {
    match check(x) {
        Result::Ok(v) => v + 1,
        Result::Err(e) => match e {
            a: ErrA => a.0 + 3,
            b => b.0 + 5,
        },
    }
}
";
    for (input, expected) in [(0u32, 13u8), (1, 25), (41, 42)] {
        let got = run_head(UNIONS_HEAD, &format!(
            "{defs}
#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {{
    dispatch({input}) as i32
}}
"
        ));
        assert_eq!(got, Ok(expected), "input {input}");
    }
}

/// Real `?` propagation across error unions (`From<ErrA> for ErrA | ErrB`,
/// `From<ErrB> for ErrA | ErrB`, and `From<ErrA | ErrB> for ErrA | ErrB | ErrC`).
#[test]
fn union_try_question_mark_propagation() {
    let defs = "
enum Infallible {}

enum ControlFlow<B, C> {
    #[lang = \"Continue\"]
    Continue(C),
    #[lang = \"Break\"]
    Break(B),
}

#[lang = \"from\"]
trait From<T> {
    fn from(value: T) -> Self;
}

impl<T> From<T> for T {
    fn from(value: T) -> Self {
        value
    }
}

trait FromResidual<R = <Self as Try>::Residual> {
    #[lang = \"from_residual\"]
    fn from_residual(residual: R) -> Self;
}

trait Try: FromResidual {
    type Output;
    type Residual;
    #[lang = \"branch\"]
    fn branch(self) -> ControlFlow<Self::Residual, Self::Output>;
}

enum Result<T, E> {
    Ok(T),
    Err(E),
}

impl<T, E> Try for Result<T, E> {
    type Output = T;
    type Residual = Result<Infallible, E>;
    fn branch(self) -> ControlFlow<Self::Residual, Self::Output> {
        match self {
            Result::Ok(v) => ControlFlow::Continue(v),
            Result::Err(e) => ControlFlow::Break(Result::Err(e)),
        }
    }
}

impl<T, E, F: From<E>> FromResidual<Result<Infallible, E>> for Result<T, F> {
    fn from_residual(residual: Result<Infallible, E>) -> Self {
        match residual {
            Result::Err(e) => Result::Err(From::from(e)),
            Result::Ok(x) => match x {},
        }
    }
}

struct ErrA(u32);
struct ErrB(u32);
struct ErrC(u32);

fn read(mode: u32) -> Result<u32, ErrA> {
    if mode == 1 {
        Result::Err(ErrA(10))
    } else {
        Result::Ok(mode)
    }
}

fn parse(x: u32) -> Result<u32, ErrB> {
    if x == 2 {
        Result::Err(ErrB(20))
    } else {
        Result::Ok(x + 42)
    }
}

fn load(mode: u32) -> Result<u32, ErrA | ErrB> {
    let a = read(mode)?;
    let b = parse(a)?;
    Result::Ok(b)
}

fn outer(mode: u32) -> Result<u32, ErrA | ErrB | ErrC> {
    if mode == 3 {
        return Result::Err(ErrC(30));
    }
    let v = load(mode)?;
    Result::Ok(v)
}

fn dispatch(mode: u32) -> u32 {
    match outer(mode) {
        Result::Ok(v) => v,
        Result::Err(e) => match e {
            a: ErrA => a.0 + 1,
            b: ErrB => b.0 + 2,
            c: ErrC => c.0 + 3,
        },
    }
}
";
    for (mode, expected) in [(0u32, 42u8), (1, 11), (2, 22), (3, 33)] {
        let got = run_head(UNIONS_HEAD, &format!(
            "{defs}
#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {{
    dispatch({mode}) as i32
}}
"
        ));
        assert_eq!(got, Ok(expected), "mode {mode}");
    }
}

/// Structural record width-subtyping coercion `{ host, port, tls } -> { host }` -> exit 42.
#[test]
fn record_width_subtyping_coercion() {
    let got = run_head(RECORDS_HEAD, "
fn read_host(r: { host: u32 }) -> u32 {
    r.host
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let wide = { host: 20u32, port: 8080u32, tls: true };
    let a = read_host(wide);
    let b = read_host({ host: 22u32, port: 443u32, tls: false });
    (a + b) as i32
}
");
    assert_eq!(got, Ok(42));
}

/// Structural record type-level defaults `{ a: u32, b: u32 = 32 }` with `{ a: 10u32, .. }` -> exit 42.
#[test]
fn record_type_level_defaults() {
    let got = run_head(RECORDS_HEAD, "
fn sum(r: { a: u32, b: u32 = 32 }) -> u32 {
    r.a + r.b
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    sum({ a: 10u32, .. }) as i32
}
");
    assert_eq!(got, Ok(42));
}

/// Anonymous record default elision without `, ..` (`{ a: 10u32 }`) and all-defaults `{ .. }`.
#[test]
fn record_default_elision_without_dots_and_all_defaults() {
    let elided = run_head(RECORDS_HEAD, "
fn sum(r: { a: u32, b: u32 = 32 }) -> u32 {
    r.a + r.b
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    sum({ a: 10u32 }) as i32
}
");
    assert_eq!(elided, Ok(42));

    let all_def = run_head(RECORDS_HEAD, "
fn all_def(r: { a: u32 = 10, b: u32 = 32 }) -> u32 {
    r.a + r.b
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    all_def({ .. }) as i32
}
");
    assert_eq!(all_def, Ok(42));
}

/// Inferred `Result<T, _>` error unions across private and `pub fn` with `?` call chains.
#[test]
fn inferred_result_error_union_and_try_chain() {
    let defs = "
enum Infallible {}

enum ControlFlow<B, C> {
    #[lang = \"Continue\"]
    Continue(C),
    #[lang = \"Break\"]
    Break(B),
}

#[lang = \"from\"]
trait From<T> {
    fn from(value: T) -> Self;
}

impl<T> From<T> for T {
    fn from(value: T) -> Self {
        value
    }
}

trait FromResidual<R = <Self as Try>::Residual> {
    #[lang = \"from_residual\"]
    fn from_residual(residual: R) -> Self;
}

trait Try: FromResidual {
    type Output;
    type Residual;
    #[lang = \"branch\"]
    fn branch(self) -> ControlFlow<Self::Residual, Self::Output>;
}

enum Result<T, E> {
    Ok(T),
    Err(E),
}

impl<T, E> Try for Result<T, E> {
    type Output = T;
    type Residual = Result<Infallible, E>;
    fn branch(self) -> ControlFlow<Self::Residual, Self::Output> {
        match self {
            Result::Ok(v) => ControlFlow::Continue(v),
            Result::Err(e) => ControlFlow::Break(Result::Err(e)),
        }
    }
}

impl<T, E, F: From<E>> FromResidual<Result<Infallible, E>> for Result<T, F> {
    fn from_residual(residual: Result<Infallible, E>) -> Self {
        match residual {
            Result::Err(e) => Result::Err(From::from(e)),
            Result::Ok(x) => match x {},
        }
    }
}

struct ErrA(u32);
struct ErrB(u32);
struct ErrC(u32);

fn read(mode: u32) -> Result<u32, _> {
    if mode == 1 {
        Result::Err(ErrA(10))
    } else {
        Result::Ok(mode)
    }
}

fn parse(x: u32) -> Result<u32, _> {
    if x == 2 {
        Result::Err(ErrB(20))
    } else {
        Result::Ok(x + 42)
    }
}

pub fn load(mode: u32) -> Result<u32, _> {
    if mode == 3 {
        return Result::Err(ErrC(30));
    }
    let a = read(mode)?;
    let b = parse(a)?;
    Result::Ok(b)
}

fn dispatch(mode: u32) -> u32 {
    match load(mode) {
        Result::Ok(v) => v,
        Result::Err(e) => match e {
            c: ErrC => c.0 + 3,
            a: ErrA => a.0 + 1,
            b => b.0 + 2,
        },
    }
}
";
    for (mode, expected) in [(0u32, 42u8), (1, 11), (2, 22), (3, 33)] {
        let got = run_head(UNIONS_HEAD, &format!(
            "{defs}
#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {{
    dispatch({mode}) as i32
}}
"
        ));
        assert_eq!(got, Ok(expected), "mode {mode}");
    }
}

/// General return-type `_` holes: `-> _`, `-> Option<_>`, and `-> (_, u32)` on private and `pub fn`.
#[test]
fn inferred_general_return_type_holes() {
    let got = run_head(RECORDS_HEAD, "
enum Option<T> {
    Some(T),
    None,
}

fn make_rec() -> _ {
    { a: 10u32, b: 20u32 }
}

pub fn wrap() -> Option<_> {
    Option::Some(5u32)
}

fn pair() -> (_, u32) {
    (true, 7u32)
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let r = make_rec();
    let w = match wrap() {
        Option::Some(v) => v,
        Option::None => 0,
    };
    let (flag, p) = pair();
    let extra = if flag { p } else { 0 };
    (r.a + r.b + w + extra) as i32
}
");
    assert_eq!(got, Ok(42));
}

/// Mutually recursive `_`-returning functions are rejected cleanly with a cycle error.
#[test]
fn inferred_return_type_cycle_rejected() {
    let got = run_head(UNIONS_HEAD, "
enum Result<T, E> {
    Ok(T),
    Err(E),
}

fn ping(n: u32) -> Result<u32, _> {
    pong(n)
}

fn pong(n: u32) -> Result<u32, _> {
    ping(n)
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    match ping(0) {
        Result::Ok(v) => v as i32,
        Result::Err(_) => 1,
    }
}
");
    assert!(
        got.as_ref().is_err_and(|msg| msg.contains("Cycle") || msg.contains("cycle")),
        "expected cycle error, got: {got:?}"
    );
}

/// Compile-time `const` structural records and error unions evaluated via `blaze-consteval`
/// and linked into native machine code.
#[test]
fn const_record_and_union_eval() {
    let got_rec = run_head(RECORDS_HEAD, "
const REC: { a: u32, b: u32 } = { b: 10u32, a: 32u32 };

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    (REC.a + REC.b) as i32
}
");
    assert_eq!(got_rec, Ok(42));

    let got_uni = run_head(UNIONS_HEAD, "
const UNI: u32 | bool = 40u32;

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    match UNI {
        n: u32 => (n + 2u32) as i32,
        _: bool => 0,
    }
}
");
    assert_eq!(got_uni, Ok(42));
}

/// Full native Flecs-without-Flecs-Script Proof of Concept combining:
/// - Composable prefab types via record type spread (`{ ..Transform, ..Velocity, ..Combat }`)
/// - Inherited & overridden type-level defaults + elided literals (`{ hp: 30u32, vx: 2i32 }`)
/// - Const-evaluated prefabs + partial overrides (`{ x: 3i32, ..GOBLIN }`)
/// - Compile-time reflection intrinsics (`blaze_field_count`, `blaze_records_disjoint`,
///   `blaze_record_schema_id`) proving canonical archetype identity and disjoint system queries
/// - Width-subtyping systems (`RecordProject`) over both `UnitPrefab` and `BossPrefab`
/// - SoA world + `ChildOf` cascade + `Result<u32, _>` inferred error unions (`MissingEntity | DeadEntity`)
/// - Live `front2::inspect` LLM inlay-hint rendering over the PoC itself.
#[test]
fn flecs_without_flecs_script_native_poc_and_inspect() {
    let poc_source = "#![blaze::records]
#![blaze::error_unions]
#![feature(no_core, lang_items, intrinsics)]
#![no_core]
#![no_main]

#[lang = \"sized\"]
trait Sized {}

#[lang = \"panic_bounds_check\"]
fn panic_bounds_check(_index: usize, _len: usize) -> ! {
    loop {}
}

#[lang = \"copy\"]
trait Copy {}

impl Copy for u8 {}
impl Copy for u32 {}
impl Copy for u64 {}
impl Copy for i32 {}
impl Copy for usize {}
impl Copy for bool {}

#[rustc_intrinsic]
const fn blaze_field_count<T>() -> usize;

#[rustc_intrinsic]
const fn blaze_records_disjoint<A, B>() -> bool;

#[rustc_intrinsic]
const fn blaze_record_schema_id<T>() -> u64;

enum Result<T, E> {
    Ok(T),
    Err(E),
}

enum ControlFlow<B, C> {
    #[lang = \"Continue\"]
    Continue(C),
    #[lang = \"Break\"]
    Break(B),
}

#[lang = \"from\"]
trait From<T> {
    fn from(value: T) -> Self;
}

impl<T> From<T> for T {
    fn from(value: T) -> Self {
        value
    }
}

#[lang = \"Try\"]
trait Try {
    type Output;
    type Residual;
    #[lang = \"from_output\"]
    fn from_output(output: Self::Output) -> Self;
    #[lang = \"branch\"]
    fn branch(self) -> ControlFlow<Self::Residual, Self::Output>;
}

#[lang = \"FromResidual\"]
trait FromResidual<R = <Self as Try>::Residual> {
    #[lang = \"from_residual\"]
    fn from_residual(residual: R) -> Self;
}

impl<T, E> Try for Result<T, E> {
    type Output = T;
    type Residual = Result<!, E>;
    fn from_output(output: T) -> Self {
        Result::Ok(output)
    }
    fn branch(self) -> ControlFlow<Result<!, E>, T> {
        match self {
            Result::Ok(v) => ControlFlow::Continue(v),
            Result::Err(e) => ControlFlow::Break(Result::Err(e)),
        }
    }
}

impl<T, E, F: From<E>> FromResidual<Result<!, E>> for Result<T, F> {
    fn from_residual(residual: Result<!, E>) -> Self {
        match residual {
            Result::Err(e) => Result::Err(F::from(e)),
            Result::Ok(never) => match never {},
        }
    }
}

// 1. Declarative Flecs-style prefabs using composable record spread & inherited defaults.
type Transform = { x: i32 = 0, y: i32 = 0, scale: i32 = 1 };
type Velocity = { vx: i32 = 0, vy: i32 = 0 };
type Combat = { hp: u32 = 100, armor: u32 = 0 };
type UnitPrefab = { ..Transform, ..Velocity, ..Combat, team: u32 = 0 };
type BossPrefab = { ..UnitPrefab, hp: u32 = 500, shield: u32 = 50, phase: u32 = 1 };
type FlatUnit = { armor: u32, hp: u32, scale: i32, team: u32, vx: i32, vy: i32, x: i32, y: i32 };

// 2. Compile-time reflection proofs (zero macros, zero runtime cost).
const BOSS_FIELDS: usize = blaze_field_count::<BossPrefab>();
const DISJOINT_MOVE: bool = blaze_records_disjoint::<{ x: i32, y: i32 }, { vx: i32, vy: i32 }>();
const OVERLAP_COMBAT: bool = blaze_records_disjoint::<UnitPrefab, BossPrefab>();
const SCHEMA_MATCH: bool = blaze_record_schema_id::<UnitPrefab>() == blaze_record_schema_id::<FlatUnit>();

// 3. Const-evaluated prefabs with elided inherited defaults.
const GOBLIN: UnitPrefab = { hp: 30u32, vx: 2i32 };
const DRAGON: BossPrefab = { x: 10i32, shield: 80u32, vx: 5i32 };

// 4. Width-subtyping physics system: accepts ANY record containing x, y, vx, vy.
fn step_physics(e: { x: i32, y: i32, vx: i32, vy: i32 }) -> { x: i32, y: i32 } {
    { x: e.x + e.vx, y: e.y + e.vy }
}

// 5. Minimal SoA World with ChildOf hierarchy + fallible inferred-error-set queries.
struct MissingEntity(u32);
struct DeadEntity(u32);

struct World {
    alive: [bool; 4],
    parent: [usize; 4], // 4 = root (no parent)
    local_x: [i32; 4],
    world_x: [i32; 4],
    hp: [u32; 4],
    shield: [u32; 4],
}

fn spawn_unit(w: &mut World, id: usize, parent: usize, u: UnitPrefab) {
    let moved = step_physics(u);
    w.alive[id] = true;
    w.parent[id] = parent;
    w.local_x[id] = moved.x;
    w.world_x[id] = moved.x;
    w.hp[id] = u.hp;
    w.shield[id] = 0;
}

fn spawn_boss(w: &mut World, id: usize, parent: usize, b: BossPrefab) {
    let moved = step_physics(b);
    w.alive[id] = true;
    w.parent[id] = parent;
    w.local_x[id] = moved.x;
    w.world_x[id] = moved.x;
    w.hp[id] = b.hp;
    w.shield[id] = b.shield;
}

fn cascade_transforms(w: &mut World) {
    let mut i: usize = 0;
    while i < 4 {
        if w.alive[i] {
            let p = w.parent[i];
            if p < 4 {
                w.world_x[i] = w.world_x[p] + w.local_x[i];
            } else {
                w.world_x[i] = w.local_x[i];
            }
        }
        i = i + 1;
    }
}

fn require_alive(w: &World, id: usize) -> Result<u32, MissingEntity> {
    if id >= 4 || !w.alive[id] {
        Result::Err(MissingEntity(id as u32))
    } else {
        Result::Ok(w.hp[id])
    }
}

pub fn query_effective_hp(w: &World, id: usize) -> Result<u32, _> {
    let hp = require_alive(w, id)?;
    if hp == 0 {
        return Result::Err(DeadEntity(id as u32));
    }
    Result::Ok(hp + w.shield[id])
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    if BOSS_FIELDS != 10 || !DISJOINT_MOVE || OVERLAP_COMBAT || !SCHEMA_MATCH {
        return 1;
    }
    let mut world = World {
        alive: [false; 4],
        parent: [4; 4],
        local_x: [0; 4],
        world_x: [0; 4],
        hp: [0; 4],
        shield: [0; 4],
    };
    // Entity 0: Dragon boss at root (x=10 + vx=5 = 15, hp=500, shield=80)
    spawn_boss(&mut world, 0, 4, DRAGON);
    // Entity 1: Goblin minion ChildOf(0), partial override x=3 + vx=2 = 5 => world_x = 15 + 5 = 20, hp = 30
    let minion: UnitPrefab = { x: 3i32, ..GOBLIN };
    spawn_unit(&mut world, 1, 0, minion);
    // Entity 2: Fallen goblin (hp = 0)
    let fallen: UnitPrefab = { hp: 0u32 };
    spawn_unit(&mut world, 2, 0, fallen);

    cascade_transforms(&mut world);

    // Effective HP of Dragon (580) + Minion (30) -> check world_x(1) == 20, dead(2) -> DeadEntity(2), missing(3) -> MissingEntity(3).
    let boss_hp = match query_effective_hp(&world, 0) {
        Result::Ok(v) => v,
        Result::Err(_) => 0,
    };
    let minion_hp = match query_effective_hp(&world, 1) {
        Result::Ok(v) => v,
        Result::Err(_) => 0,
    };
    let dead_code = match query_effective_hp(&world, 2) {
        Result::Ok(_) => 0,
        Result::Err(e) => match e {
            d: DeadEntity => d.0 + 10,
            m => m.0 + 100,
        },
    };
    let miss_code = match query_effective_hp(&world, 3) {
        Result::Ok(_) => 0,
        Result::Err(e) => match e {
            m: MissingEntity => m.0 + 7,
            d => d.0 + 200,
        },
    };
    // 580 + 30 == 610, world_x[1] == 20, dead_code == 12, miss_code == 10 => 20 + 12 + 10 = 42!
    if boss_hp != 580 || minion_hp != 30 {
        return 2;
    }
    (world.world_x[1] as u32 + dead_code + miss_code) as i32
}
";
    // 1. Compile to native machine code and execute in-process!
    let got = run_head("", poc_source);
    assert_eq!(got, Ok(42));

    // 2. Verify `blaze inspect` surfaces LLM inlay hints across the PoC!
    let alloc = blaze_own::testing::heap().alloc();
    let inspected = blaze_ladder::front2::inspect::inspect_source(
        poc_source,
        blaze_ladder::front2::inspect::BLAZE_EDITION,
        None,
        blaze_ladder::front2::inspect::InspectOpts::default(),
        alloc,
        alloc,
    )
    .expect("inspect PoC");
    let annotated = blaze_ladder::front2::inspect::render_annotated(&inspected, alloc);
    assert!(annotated.contains("DeadEntity") && annotated.contains("MissingEntity"), "inferred error union hint missing:\n{annotated}");
    assert!(annotated.contains("project ->"), "RecordProject width-subtyping hint missing:\n{annotated}");
    assert!(annotated.contains("..defaults:"), "elided inherited default hint missing:\n{annotated}");
    assert!(annotated.contains("? MissingEntity"), "try propagation hint missing:\n{annotated}");
    println!("=== FLECS-WITHOUT-FLECS-SCRIPT POC (BLAZE INSPECT) ===\n{annotated}");
}

/// Comptime field unrolling (`inline for`) on heterogeneous structural records, tuples,
/// `&rec` name+value destructuring without moving, and `&mut rec` in-place mutation.
#[test]
fn inline_for_record_and_tuple_heterogeneous_dispatch() {
    let got = run_head(RECORDS_HEAD, "
impl Copy for usize {}

#[repr(C)]
struct Words {
    data: usize,
    len: usize,
}

impl Copy for Words {}

union Wide {
    s: &'static str,
    words: Words,
}

fn str_len(s: &'static str) -> u32 {
    unsafe { Wide { s }.words.len as u32 }
}

trait Score {
    fn score(&self) -> u32;
    fn take_score(self) -> u32;
    fn bump(&mut self);
}

struct A(u32);
struct B(u32);
struct C(u32);

impl Score for A {
    fn score(&self) -> u32 { self.0 }
    fn take_score(self) -> u32 { self.0 }
    fn bump(&mut self) { self.0 = self.0 + 1; }
}

impl Score for B {
    fn score(&self) -> u32 { self.0 * 2 }
    fn take_score(self) -> u32 { self.0 * 2 }
    fn bump(&mut self) { self.0 = self.0 + 2; }
}

impl Score for C {
    fn score(&self) -> u32 { self.0 + 3 }
    fn take_score(self) -> u32 { self.0 + 3 }
    fn bump(&mut self) { self.0 = self.0 + 3; }
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let mut total: u32 = 0;
    let rec = { a: A(4), b: B(5), c: C(3) };
    // Borrowed (name, field) iteration without moving `rec`:
    // names: 1 + 1 + 1 = 3; scores: 4 + 10 + 6 = 20 => 23
    inline for (name, field) in &rec {
        total = total + str_len(name) + field.score();
    }
    // By-value heterogeneous record iteration (moves disjoint fields):
    // 4 + 10 + 6 = 20 => 43
    inline for field in rec {
        total = total + field.take_score();
    }
    // Heterogeneous tuple iteration:
    // 1 + 4 + 6 = 11 => 54
    inline for x in (A(1), B(2), C(3)) {
        total = total + x.take_score();
    }
    // Mutable borrowed record iteration:
    // A(2)->3, B(3)->5 => +8 => 62
    let mut mut_rec = { x: A(2), y: B(3) };
    inline for f in &mut mut_rec {
        f.bump();
    }
    total = total + mut_rec.x.0 + mut_rec.y.0;
    total as i32
}
");
    assert_eq!(got, Ok(62));
}

/// Generic `fn score_all<T>(bundle: T) -> u32` using `inline for` with zero trait bounds on `T`:
/// called with a composed defaulted record prefab, a heterogeneous tuple, a named struct, and a
/// tuple struct — compiled straight to zero-overhead monomorphized native code.
#[test]
fn inline_for_generic_spawn_prefab_bundle() {
    let got = run_head(RECORDS_HEAD, "
trait Score {
    fn score(self) -> u32;
}

struct Health(u32);
struct Armor(u32);
struct Shield(u32);

impl Score for Health {
    fn score(self) -> u32 { self.0 }
}

impl Score for Armor {
    fn score(self) -> u32 { self.0 * 2 }
}

impl Score for Shield {
    fn score(self) -> u32 { self.0 + 1 }
}

type Unit = { hp: Health = Health(10) };
type Boss = { ..Unit, shield: Shield = Shield(12) };

struct Pack {
    h: Health,
    s: Shield,
}

struct Pair(Health, Armor);

fn score_all<T>(bundle: T) -> u32 {
    let mut acc: u32 = 0;
    inline for f in bundle {
        acc = acc + f.score();
    }
    acc
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    // 1. Composed defaulted record prefab: Health(20) -> 20, Shield(12) -> 13 => 33
    let s1 = score_all::<Boss>({ hp: Health(20) });
    // 2. Heterogeneous tuple: Health(5) -> 5, Armor(5) -> 10, Shield(2) -> 3 => 18
    let s2 = score_all((Health(5), Armor(5), Shield(2)));
    // 3. Named struct: Health(4) -> 4, Shield(6) -> 7 => 11
    let s3 = score_all(Pack { h: Health(4), s: Shield(6) });
    // 4. Tuple struct: Health(3) -> 3, Armor(4) -> 8 => 11
    let s4 = score_all(Pair(Health(3), Armor(4)));
    (s1 + s2 + s3 + s4) as i32
}
");
    assert_eq!(got, Ok(73));
}

/// `inline for` with `?` propagation and `Result<u32, _>` inferred structural error union.
#[test]
fn inline_for_with_error_union_try_propagation() {
    let got = run_head("", "#![blaze::records]
#![blaze::error_unions]
#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = \"sized\"]
trait Sized {}

#[lang = \"panic_bounds_check\"]
fn panic_bounds_check(_index: usize, _len: usize) -> ! {
    loop {}
}

#[lang = \"copy\"]
trait Copy {}

impl Copy for u32 {}
impl Copy for i32 {}

#[lang = \"from\"]
trait From<T> {
    fn from(v: T) -> Self;
}

impl<T> From<T> for T {
    fn from(v: T) -> T { v }
}

enum ControlFlow<B, C = ()> {
    #[lang = \"Continue\"]
    Continue(C),
    #[lang = \"Break\"]
    Break(B),
}

enum Result<T, E> {
    Ok(T),
    Err(E),
}

#[lang = \"Try\"]
trait Try {
    type Output;
    type Residual;
    #[lang = \"from_output\"]
    fn from_output(output: Self::Output) -> Self;
    #[lang = \"branch\"]
    fn branch(self) -> ControlFlow<Self::Residual, Self::Output>;
}

#[lang = \"FromResidual\"]
trait FromResidual<R = <Self as Try>::Residual> {
    #[lang = \"from_residual\"]
    fn from_residual(residual: R) -> Self;
}

impl<T, E> Try for Result<T, E> {
    type Output = T;
    type Residual = Result<T, E>;
    fn from_output(output: T) -> Self { Result::Ok(output) }
    fn branch(self) -> ControlFlow<Result<T, E>, T> {
        match self {
            Result::Ok(v) => ControlFlow::Continue(v),
            Result::Err(e) => ControlFlow::Break(Result::Err(e)),
        }
    }
}

impl<T, E, F: From<E>> FromResidual<Result<T, E>> for Result<T, F> {
    fn from_residual(residual: Result<T, E>) -> Self {
        match residual {
            Result::Err(e) => Result::Err(F::from(e)),
            Result::Ok(_) => loop {},
        }
    }
}

struct ErrA(u32);
struct ErrB(u32);
struct CheckA(u32);
struct CheckB(u32);

impl CheckA {
    fn verify(self) -> Result<u32, ErrA> {
        if self.0 == 0 { Result::Err(ErrA(10)) } else { Result::Ok(self.0) }
    }
}

impl CheckB {
    fn verify(self) -> Result<u32, ErrB> {
        if self.0 == 0 { Result::Err(ErrB(20)) } else { Result::Ok(self.0 * 2) }
    }
}

fn check_all(r: { a: CheckA, b: CheckB }) -> Result<u32, _> {
    let mut sum: u32 = 0;
    inline for f in r {
        sum = sum + f.verify()?;
    }
    Result::Ok(sum)
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let ok_val = match check_all({ a: CheckA(4), b: CheckB(4) }) {
        Result::Ok(v) => v,
        Result::Err(_) => 0,
    };
    let err_a = match check_all({ a: CheckA(0), b: CheckB(4) }) {
        Result::Ok(_) => 0,
        Result::Err(e) => match e {
            a: ErrA => a.0,
            b => b.0 + 100,
        },
    };
    let err_b = match check_all({ a: CheckA(4), b: CheckB(0) }) {
        Result::Ok(_) => 0,
        Result::Err(e) => match e {
            b: ErrB => b.0,
            a => a.0 + 100,
        },
    };
    // 12 + 10 + 20 = 42
    (ok_val + err_a + err_b) as i32
}
");
    assert_eq!(got, Ok(42));
}

/// Combined native compile-and-run e2e test proving:
/// - All-`pub` no-`Drop` structs (`Pos`, `Vel`, `Health`) are automatically `Copy` and `Clone`
///   (with synthesized `Clone::clone` shims and zero `#[derive]`)
/// - Private-field structs (`FileHandle(u32)`) remain strictly move-only (rejected by `T: Copy`)
/// - Lockstep `inline for (a, b) in zip_fields(lhs, rhs)` powers zero-macro generic structural
///   `eq_all<T>` and `inline for (name, f) in v` powers zero-macro generic `hash_all<T>` across
///   both auto-`Copy` structs and composed `{ ..Base }` structural records.
#[test]
fn auto_copy_pub_structs_and_inline_for_zip_reflection() {
    let ok = run_head(RECORDS_HEAD, "
#[lang = \"clone\"]
trait Clone: Sized {
    fn clone(&self) -> Self;
}

impl Clone for i32 {
    fn clone(&self) -> Self { *self }
}
impl Clone for u32 {
    fn clone(&self) -> Self { *self }
}

// ZERO `impl Copy` or `impl Clone` — all fields are `pub`!
struct Pos { pub x: i32, pub y: i32 }
struct Vel { pub dx: i32, pub dy: i32 }
struct Health(pub u32);

trait EqVal {
    fn eq_val(&self, other: &Self) -> bool;
}

impl EqVal for i32 {
    fn eq_val(&self, other: &Self) -> bool { *self == *other }
}
impl EqVal for u32 {
    fn eq_val(&self, other: &Self) -> bool { *self == *other }
}
impl EqVal for Pos {
    fn eq_val(&self, other: &Self) -> bool { eq_all(self, other) }
}
impl EqVal for Vel {
    fn eq_val(&self, other: &Self) -> bool { eq_all(self, other) }
}
impl EqVal for Health {
    fn eq_val(&self, other: &Self) -> bool { eq_all(self, other) }
}

fn eq_all<T>(lhs: &T, rhs: &T) -> bool {
    inline for (a, b) in zip_fields(lhs, rhs) {
        if !a.eq_val(b) {
            return false;
        }
    }
    true
}

fn require_copy<T: Copy>(v: T) -> T {
    v
}

type Movable = { pos: Pos = Pos { x: 1, y: 2 }, vel: Vel = Vel { dx: 3, dy: 4 } };
type Unit = { ..Movable, hp: Health = Health(32) };

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let p = Pos { x: 10, y: 20 };
    let p_copy = require_copy(p);
    let p_clone = p.clone();
    if !eq_all(&p, &p_copy) || !eq_all(&p, &p_clone) {
        return 1;
    }

    let u1: Unit = { pos: p };
    let u2: Unit = u1.clone();
    let u3: Unit = { pos: Pos { x: 10, y: 99 }, ..u1 };
    if !eq_all(&u1, &u2) || eq_all(&u1, &u3) {
        return 2;
    }
    (u2.pos.x as u32 + u2.hp.0) as i32
}
");
    assert_eq!(ok, Ok(42));

    // Private-field struct must remain strictly move-only (NOT auto-Copy).
    let private_rejected = run_head(RECORDS_HEAD, "
struct FileHandle(u32);

fn require_copy<T: Copy>(v: T) -> T {
    v
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let h = FileHandle(7);
    let _ = require_copy(h);
    0
}
");
    assert!(
        private_rejected.is_err(),
        "private-field struct `FileHandle(u32)` must NOT be auto-Copy, got: {private_rejected:?}"
    );
}

/// Native compile-and-run e2e test proving zero-macro `serde`-equivalent serialization,
/// live in-place field patching by runtime field name, and snapshot/diff logging across:
/// - plain all-`pub` structs (`Pos`, `Health`) with zero `#[derive]` / zero macros (auto-`Copy`/`Clone`)
/// - composed structural records with spread & defaults (`type EntityRecord = { ..BasePos, hp: i32 = 100 }`)
/// using `inline for (name, val) in v`, `inline for (name, field) in dst` (`&mut T`), and
/// `inline for (name, a, b) in zip_fields(before, after)`.
#[test]
fn inline_for_zero_macro_serde_and_live_patch() {
    let got = run_head(RECORDS_HEAD, "
impl Copy for usize {}

#[lang = \"panic_const_div_by_zero\"]
fn panic_const_div_by_zero() -> ! {
    loop {}
}

#[lang = \"panic_const_rem_by_zero\"]
fn panic_const_rem_by_zero() -> ! {
    loop {}
}

#[lang = \"clone\"]
trait Clone: Sized {
    fn clone(&self) -> Self;
}

impl Clone for i32 {
    fn clone(&self) -> Self { *self }
}

#[repr(C)]
struct Words {
    data: usize,
    len: usize,
}
impl Copy for Words {}

union Wide {
    s: &'static str,
    bytes: &'static [u8],
    words: Words,
}

fn str_len(s: &'static str) -> usize {
    unsafe { Wide { s }.words.len }
}

fn str_bytes(s: &'static str) -> &'static [u8] {
    unsafe { Wide { s }.bytes }
}

fn str_eq(a: &'static str, b: &'static str) -> bool {
    let n = str_len(a);
    if n != str_len(b) {
        return false;
    }
    let ab = str_bytes(a);
    let bb = str_bytes(b);
    let mut i: usize = 0;
    while i < n {
        if ab[i] != bb[i] {
            return false;
        }
        i = i + 1;
    }
    true
}

struct SerBuf {
    buf: [u8; 64],
    len: usize,
}

impl SerBuf {
    fn new() -> SerBuf {
        SerBuf { buf: [0u8; 64], len: 0 }
    }

    fn push_byte(&mut self, b: u8) {
        if self.len < 64 {
            self.buf[self.len] = b;
            self.len = self.len + 1;
        }
    }

    fn push_str(&mut self, s: &'static str) {
        let n = str_len(s);
        let bytes = str_bytes(s);
        let mut i: usize = 0;
        while i < n {
            self.push_byte(bytes[i]);
            i = i + 1;
        }
    }

    fn push_u32(&mut self, mut v: u32) {
        if v == 0 {
            self.push_byte(b'0');
            return;
        }
        let mut tmp = [0u8; 12];
        let mut k: usize = 0;
        while v > 0 {
            let digit = (v % 10) as u8;
            tmp[k] = b'0' + digit;
            k = k + 1;
            v = v / 10;
        }
        while k > 0 {
            k = k - 1;
            self.push_byte(tmp[k]);
        }
    }

    fn push_i32(&mut self, v: i32) {
        if v < 0 {
            self.push_byte(b'-');
            self.push_u32((0 - v) as u32);
        } else {
            self.push_u32(v as u32);
        }
    }

    fn push_signed_i32(&mut self, v: i32) {
        if v < 0 {
            self.push_byte(b'-');
            self.push_u32((0 - v) as u32);
        } else {
            self.push_byte(b'+');
            self.push_u32(v as u32);
        }
    }

    fn eq_str(&self, expected: &'static str) -> bool {
        let n = str_len(expected);
        if self.len != n {
            return false;
        }
        let eb = str_bytes(expected);
        let mut i: usize = 0;
        while i < n {
            if self.buf[i] != eb[i] {
                return false;
            }
            i = i + 1;
        }
        true
    }
}

// Plain all-`pub` structs with ZERO `#[derive]`, ZERO `impl Copy`/`Clone`, ZERO macros.
struct Pos {
    pub x: i32,
    pub y: i32,
}

struct Health {
    pub hp: i32,
    pub max_hp: i32,
}

// Composed structural records with spread & defaults.
type BasePos = { x: i32 = 10, y: i32 = 20 };
type EntityRecord = { ..BasePos, hp: i32 = 100 };

// Zero-macro `serde`-equivalent serializer over any struct or structural record with `i32` fields.
fn serialize_i32_fields<T>(v: T, out: &mut SerBuf) {
    inline for (name, val) in v {
        out.push_str(name);
        out.push_byte(b'=');
        out.push_i32(val);
        out.push_byte(b';');
    }
}

// Zero-macro live in-place field patcher by runtime field name (`&mut T`).
fn patch_i32_field<T>(dst: &mut T, target: &'static str, new_val: i32) -> bool {
    inline for (name, field) in dst {
        if str_eq(name, target) {
            *field = new_val;
            return true;
        }
    }
    false
}

// Zero-macro live diff / undo log via lockstep `zip_fields(before, after)`.
fn diff_i32_fields<T>(before: T, after: T, out: &mut SerBuf) -> i32 {
    let mut sum_delta: i32 = 0;
    inline for (name, a, b) in zip_fields(before, after) {
        if a != b {
            let delta = b - a;
            sum_delta = sum_delta + delta;
            out.push_str(name);
            out.push_byte(b':');
            out.push_signed_i32(delta);
            out.push_byte(b';');
        }
    }
    sum_delta
}

fn require_copy<T: Copy>(v: T) -> T {
    v
}

fn runtime_field_name(sel: i32) -> &'static str {
    if sel == 0 {
        \"x\"
    } else if sel == 1 {
        \"y\"
    } else if sel == 2 {
        \"hp\"
    } else if sel == 3 {
        \"max_hp\"
    } else {
        \"nope\"
    }
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    // 1. Named all-`pub` struct `Pos`: auto-Copy, auto-Clone, serialize, live patch, snapshot diff.
    let mut pos = Pos { x: 10, y: 20 };
    let before_pos = require_copy(pos);
    let cloned_pos = pos.clone();

    let mut s_pos = SerBuf::new();
    serialize_i32_fields(pos, &mut s_pos);
    if !s_pos.eq_str(\"x=10;y=20;\") {
        return 1;
    }

    // Live-patch `pos.x` to 42 by runtime name, leave `pos.y` untouched.
    if !patch_i32_field(&mut pos, runtime_field_name(0), 42) {
        return 2;
    }
    if pos.x != 42 || pos.y != 20 {
        return 3;
    }
    // Unknown field returns false and leaves `pos` unchanged.
    if patch_i32_field(&mut pos, runtime_field_name(9), 99) {
        return 4;
    }
    if pos.x != 42 || pos.y != 20 {
        return 5;
    }

    // Snapshot diff via `zip_fields(before_pos, pos)` reports exact changed field and delta.
    let mut pos_diff = SerBuf::new();
    let pos_delta = diff_i32_fields(before_pos, pos, &mut pos_diff);
    if pos_delta != 32 || !pos_diff.eq_str(\"x:+32;\") {
        return 6;
    }
    // Cloned snapshot still matches `before_pos` with empty diff.
    let mut zero_diff = SerBuf::new();
    if diff_i32_fields(before_pos, cloned_pos, &mut zero_diff) != 0 || zero_diff.len != 0 {
        return 7;
    }

    // Concrete (non-generic) `inline for (name, field) in &mut pos` live patch also works.
    let target_y = runtime_field_name(1);
    inline for (name, field) in &mut pos {
        if str_eq(name, target_y) {
            *field = 30;
        }
    }
    if pos.x != 42 || pos.y != 30 {
        return 8;
    }
    // Concrete (non-generic) `inline for (name, a, b) in zip_fields(before_pos, pos)` live diff.
    let mut pos_diff2 = SerBuf::new();
    inline for (name, a, b) in zip_fields(before_pos, pos) {
        if a != b {
            pos_diff2.push_str(name);
            pos_diff2.push_byte(b':');
            pos_diff2.push_signed_i32(b - a);
            pos_diff2.push_byte(b';');
        }
    }
    if !pos_diff2.eq_str(\"x:+32;y:+10;\") {
        return 20;
    }

    // 2. Named all-`pub` struct `Health`: serialize & live patch `max_hp`.
    let mut health = Health { hp: 80, max_hp: 100 };
    let before_health = health;
    let mut s_hp = SerBuf::new();
    serialize_i32_fields(health, &mut s_hp);
    if !s_hp.eq_str(\"hp=80;max_hp=100;\") {
        return 9;
    }
    if !patch_i32_field(&mut health, runtime_field_name(3), 250) {
        return 10;
    }
    if health.hp != 80 || health.max_hp != 250 {
        return 11;
    }
    if patch_i32_field(&mut health, \"shield\", 50) {
        return 12;
    }
    let mut hp_diff = SerBuf::new();
    let hp_delta = diff_i32_fields(before_health, health, &mut hp_diff);
    if hp_delta != 150 || !hp_diff.eq_str(\"max_hp:+150;\") {
        return 13;
    }

    // 3. Composed structural record with spread & defaults (`EntityRecord`).
    let mut ent: EntityRecord = { x: 10 };
    let before_ent = require_copy(ent);
    let mut s_ent_before = SerBuf::new();
    serialize_i32_fields(ent, &mut s_ent_before);
    // Structural record fields are canonically ordered alphabetically: `hp`, `x`, `y`.
    if !s_ent_before.eq_str(\"hp=100;x=10;y=20;\") {
        return 14;
    }

    if !patch_i32_field(&mut ent, runtime_field_name(1), 55) {
        return 15;
    }
    if !patch_i32_field(&mut ent, runtime_field_name(2), 75) {
        return 16;
    }
    if patch_i32_field(&mut ent, runtime_field_name(9), 123) {
        return 17;
    }
    let mut s_ent_after = SerBuf::new();
    serialize_i32_fields(ent, &mut s_ent_after);
    if !s_ent_after.eq_str(\"hp=75;x=10;y=55;\") {
        return 18;
    }

    // Snapshot diff on composed record: `hp` changed by -25, `y` changed by +35, `x` untouched.
    let mut ent_diff = SerBuf::new();
    let ent_delta = diff_i32_fields(before_ent, ent, &mut ent_diff);
    if ent_delta != 10 || !ent_diff.eq_str(\"hp:-25;y:+35;\") {
        return 19;
    }

    // Return 42 = pos_delta (32) + ent_delta (10)
    pos_delta + ent_delta
}
");
    assert_eq!(got, Ok(42));
}

/// Regression test for generic + concrete `inline for` under `LazyProgram` / `make_one.rs`
/// (`blaze run`) with BOTH `SecondCheck` AND `FromRecord` (pre-checked typed records produced
/// by `ProjectWorld::check_borrows`, mirroring `blaze check` -> `blaze run`).
#[test]
fn lazy_program_generic_inline_for_second_check_and_from_record_run() {
    use blaze_build::{OverflowChecks, Panic, UbChecks};
    use blaze_emit::{Isa, Os};
    use blaze_ids::Hasher;
    use blaze_ids::producer::{CodeBuilder, Producers};
    use blaze_ladder::body_ids::{BodyId, BodyIds, Fixture, Make, Read, RowKind, Walk};
    use blaze_ladder::code_records::{Builder, CodeStore, EntryView, MemoryStore, Settings, TierName};
    use blaze_ladder::crates::{Inputs, Toolchain};
    use blaze_ladder::front2::keys::checker_of;
    use blaze_ladder::front2::program::Front2Program;
    use blaze_ladder::front2::program::lazy::LazyProgram;
    use blaze_ladder::front2::program::lazy::typed::{TypedGap, TypedRecords};
    use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
    use blaze_ladder::front2::source::NamesSource as _;
    use blaze_ladder::program_texts::ProgramTexts;
    use blaze_ladder::run::RunError;
    use blaze_ladder::run::make_one::{CheckCount, FromRecord, MakerState, SecondCheck, Typing, make_instance, walk_records};
    use blaze_ladder::seam::FrontEnd;
    use blaze_mem::Frame;
    use blaze_own::AVec;
    use blaze_toolchain::ToolchainId;
    use blaze_typeck::ItemKey;

    let body = "
impl Copy for usize {}
impl Copy for i32 {}
impl Copy for bool {}

#[lang = \"drop\"]
trait Drop {
    fn drop(&mut self);
}

#[lang = \"fn_once\"]
trait FnOnce<Args> {
    #[lang = \"fn_once_output\"]
    type Output;
    extern \"rust-call\" fn call_once(self, args: Args) -> Self::Output;
}

#[lang = \"fn_mut\"]
trait FnMut<Args>: FnOnce<Args> {
    extern \"rust-call\" fn call_mut(&mut self, args: Args) -> Self::Output;
}

#[lang = \"fn\"]
trait Fn<Args>: FnMut<Args> {
    extern \"rust-call\" fn call(&self, args: Args) -> Self::Output;
}

#[repr(C)]
struct Words {
    data: usize,
    len: usize,
}
impl Copy for Words {}

union Wide {
    s: &'static str,
    bytes: &'static [u8],
    words: Words,
}

fn str_eq(a: &'static str, b: &'static str) -> bool {
    let na = unsafe { Wide { s: a }.words.len };
    let nb = unsafe { Wide { s: b }.words.len };
    if na != nb {
        return false;
    }
    let ab = unsafe { Wide { s: a }.bytes };
    let bb = unsafe { Wide { s: b }.bytes };
    let mut i: usize = 0;
    while i < na {
        if ab[i] != bb[i] {
            return false;
        }
        i = i + 1;
    }
    true
}

struct Pos { pub x: i32, pub y: i32 }
struct Health { pub hp: i32, pub max_hp: i32 }
struct Pair { pub a: Pos, pub b: Health }

fn sum_fields<T>(v: &T) -> i32 {
    let mut total = 0;
    inline for (_name, field) in v {
        total = total + *field;
    }
    total
}

fn patch_field<T>(dst: &mut T, target: &'static str, value: i32) -> bool {
    inline for (name, field) in dst {
        if str_eq(name, target) {
            *field = value;
            return true;
        }
    }
    false
}

fn sum_via_closure<T>(v: &T) -> i32 {
    let mut total = 0;
    inline for (_name, field) in v {
        let read = || *field;
        total = total + read();
    }
    total
}

fn nested_sum<T>(v: &T) -> i32 {
    let mut total = 0;
    inline for (_name, inner) in v {
        total = total + sum_fields(inner);
    }
    total
}

fn zip_delta<T>(a: &T, b: &T) -> i32 {
    let mut total = 0;
    inline for (_name, x, y) in zip_fields(a, b) {
        total = total + (*y - *x);
    }
    total
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let mut p = Pos { x: 10, y: 20 };
    // Generic sum_fields(&Pos): 30
    if sum_fields(&p) != 30 {
        return 1;
    }
    // Generic patch_field(&mut Pos): x -> 42, returns true
    if !patch_field(&mut p, \"x\", 42) {
        return 2;
    }
    if p.x != 42 || p.y != 20 {
        return 3;
    }
    if patch_field(&mut p, \"missing\", 99) {
        return 4;
    }
    // Same generic fn instantiated at a second struct type `Health`
    let mut h = Health { hp: 5, max_hp: 15 };
    if sum_fields(&h) != 20 {
        return 5;
    }
    if !patch_field(&mut h, \"max_hp\", 25) {
        return 6;
    }
    // Generic inline for with closure inside unrolled body: 42 + 20 = 62
    if sum_via_closure(&p) != 62 {
        return 7;
    }
    // Nested generic inline for calling another generic inline for: (42+20) + (5+25) = 92
    let pair = Pair { a: p, b: h };
    if nested_sum(&pair) != 92 {
        return 8;
    }
    // Generic zip_fields: (42-10) + (20-20) = 32
    let before = Pos { x: 10, y: 20 };
    if zip_delta(&before, &p) != 32 {
        return 9;
    }
    // Concrete inline for inside main() (also tests Typer::Record expansion)
    let mut concrete = 0;
    inline for (_name, f) in &p {
        concrete = concrete + *f;
    }
    if concrete != 62 {
        return 10;
    }
    // Generic inline for on structural record: 3 + 7 = 10
    let rec = { alpha: 3i32, beta: 7i32 };
    if sum_fields(&rec) != 10 {
        return 11;
    }
    // 32 + 10 = 42
    zip_delta(&before, &p) + sum_fields(&rec)
}
";
    // 1. Whole-program `Front2Program` baseline.
    assert_eq!(run_head(RECORDS_HEAD, body), Ok(42));

    // 2. Build `Names` once for `ProjectWorld` (`blaze check`) and `LazyProgram` (`blaze run`).
    let text = [RECORDS_HEAD, body].concat();
    let files = [SourceFile { path: "src/main.rs", text: &text }];
    let heap = blaze_own::testing::heap();
    let alloc = heap.alloc();
    let frame = Frame::new(blaze_mem::tag::Tag::Infer).expect("frame");
    let tc = Toolchain::none(alloc);
    let texts = ProgramTexts::new(&files, "src/main.rs", NEW, &tc, Panic::Abort).expect("program texts");
    let user = texts.user();
    let inputs = Inputs::new(&user, &tc).expect("inputs");
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).expect("names");

    // Pre-check every free fn with `ProjectWorld::check_borrows` (the exact path `blaze check`
    // uses to write typed records for `blaze run`).
    let mut encoded_records: blaze_own::Vec<(ItemKey, blaze_own::Vec<u8, blaze_own::Alloc<'static>>), blaze_own::Alloc<'static>> =
        blaze_own::Vec::new_in(alloc);
    {
        let check_frame = Frame::new(blaze_mem::tag::Tag::Infer).expect("check frame");
        let mut world = ProjectWorld::new(&names, &check_frame, &files, alloc).expect("project world");
        let mut fn_items = blaze_own::Vec::new_in(alloc);
        (&names).each_item(0, &mut |item, _| {
            if (&names).kind_of_item(item) == Some(blaze_surface::ItemKind::Function) {
                fn_items.push(item);
            }
        });
        for item in fn_items {
            let fn_name = (&names).name_of(item).unwrap_or("");
            let (verdict, borrows) = world.check_borrows(BodyRef::Free(item), alloc);
            assert!(verdict.diagnostics.is_empty(), "{fn_name}: check diagnostics: {:?}", verdict.diagnostics);
            assert!(verdict.gaps.is_empty(), "{fn_name}: check gaps: {:?}", verdict.gaps);
            assert!(verdict.refused.is_none(), "{fn_name}: check refused: {:?}", verdict.refused);
            if fn_name != "str_eq" {
                assert!(borrows.refused.is_none(), "{fn_name}: borrow refused: {:?}", borrows.refused);
            }
            let bytes = verdict.typed.expect("typed record encoded").expect("clean typed record");
            let mut copy = blaze_own::Vec::with_capacity_in(bytes.len(), alloc);
            copy.extend_from_slice(bytes.as_slice());
            encoded_records.push((checker_of(item), copy));
        }
    }

    struct EveryItem;
    static READS: [Read; 1] = [Read { fact: [3; 32], digest: [4; 32] }];
    impl BodyIds for EveryItem {
        fn body_check(&self, item: ItemKey) -> Option<BodyId<'_>> {
            Some(BodyId { key: Hasher::new("inline-for lazy test").part(&item.0.to_le_bytes()[..]).finish(), reads: &READS })
        }
        fn today(&self, fact: &[u8; 32]) -> Option<[u8; 32]> {
            (*fact == READS[0].fact).then_some(READS[0].digest)
        }
        fn crate_fact(&self, _: ItemKey) -> Option<Read> {
            Some(Read { fact: [8; 32], digest: [9; 32] })
        }
    }

    struct MapTyped {
        records: blaze_own::Vec<(ItemKey, blaze_own::Vec<u8, blaze_own::Alloc<'static>>), blaze_own::Alloc<'static>>,
        alloc: blaze_own::Alloc<'static>,
    }
    impl TypedRecords for MapTyped {
        fn typed(&mut self, key: ItemKey) -> Result<blaze_typeck::Checked<'static>, TypedGap> {
            let Some((_, bytes)) = self.records.iter().find(|(k, _)| *k == key) else {
                return Err(TypedGap::NoRecord { key });
            };
            blaze_typeck::checked_record::decode(bytes.as_slice(), self.alloc).map_err(|why| TypedGap::Record { key, why })
        }
    }

    struct LazyWalkMaker<'p, P, T> {
        program: &'p mut P,
        typing: T,
        state: Option<MakerState>,
        checks: CheckCount,
        isa: Isa,
        overflow: OverflowChecks,
        ub: UbChecks,
        panic: Panic,
        alloc: blaze_own::Alloc<'static>,
    }
    impl<P: FrontEnd, T: Typing<P>> Make for LazyWalkMaker<'_, P, T> {
        type Error = RunError;
        type Grant = Fixture;
        fn make(&mut self, _grant: &Fixture, tabled: ItemKey, name: &[u8], entry: &mut AVec<'_, u8>) -> Result<(), Self::Error> {
            if self.state.is_none() {
                self.state = Some(MakerState::new(self.program, tabled, self.isa, self.overflow, self.ub, self.panic, self.alloc)?);
            }
            let Some(state) = self.state.as_mut() else {
                return Err(RunError::OneBody { key: tabled, why: "maker state missing" });
            };
            make_instance(self.program, &mut self.typing, state, &mut self.checks, tabled, name, entry)
        }
        fn make_static(&mut self, _grant: &Fixture, item: ItemKey, _: &mut AVec<'_, u8>) -> Result<(), Self::Error> {
            Err(RunError::OneBody { key: item, why: "unexpected static" })
        }
        fn make_thread_local(&mut self, _grant: &Fixture, item: ItemKey, _: &mut AVec<'_, u8>) -> Result<(), Self::Error> {
            Err(RunError::OneBody { key: item, why: "unexpected tls" })
        }
    }

    let whole = Front2Program::new(&names, &frame, &files, alloc).expect("whole program");
    let entry = whole.function(whole.entry().expect("entry")).key;
    let producer = Producers::for_tests(b"inline-for lazy").get::<CodeBuilder>();
    let isa = Isa { lse: true, os: if cfg!(target_os = "macos") { Os::MacOs } else { Os::Linux } };
    let builder = Builder {
        isa,
        tier: TierName::Reference,
        settings: Settings::baseline(OverflowChecks::Off, Panic::Abort),
        toolchain: ToolchainId([7; 32]),
        producer,
        edition: edition::PackageEdition::of_manifest("blaze-2026.11").expect("blaze edition"),
    };

    let run_lazy_mode = |label: &str, use_records: bool| {
        let mut lazy = LazyProgram::new(&names, &frame, &files, Some(entry), alloc).expect("lazy program");
        lazy.decide_start(blaze_ladder::run::special::MakerStart::ByRule).expect("decide_start");
        let mut store = MemoryStore::new_in(alloc);
        let walk: Walk<'_> = if use_records {
            let mut maker = LazyWalkMaker {
                program: &mut lazy,
                typing: FromRecord(MapTyped { records: encoded_records.clone(), alloc }),
                state: None,
                checks: CheckCount::default(),
                isa,
                overflow: OverflowChecks::Off,
                ub: UbChecks::Off,
                panic: Panic::Abort,
                alloc,
            };
            walk_records(entry, &EveryItem, &mut store, &builder, &producer, &mut maker, alloc)
                .unwrap_or_else(|e| panic!("{label}: walk_records failed: {e}"))
                .0
        } else {
            let mut maker = LazyWalkMaker {
                program: &mut lazy,
                typing: SecondCheck,
                state: None,
                checks: CheckCount::default(),
                isa,
                overflow: OverflowChecks::Off,
                ub: UbChecks::Off,
                panic: Panic::Abort,
                alloc,
            };
            walk_records(entry, &EveryItem, &mut store, &builder, &producer, &mut maker, alloc)
                .unwrap_or_else(|e| panic!("{label}: walk_records failed: {e}"))
                .0
        };
        #[cfg(target_arch = "aarch64")]
        {
            use blaze_code::image::{Image, Kind, Member, lay_out_positions};
            use blaze_code::record::{Space, Target};
            use blaze_launch_rt::sys::{Code, page};

            let mut members = blaze_own::Vec::new_in(alloc);
            for row in 0..walk.len() {
                assert_eq!(walk.kind(row), RowKind::Instance);
                let bytes = store.get(walk.key(row).expect("record key")).expect("store get").expect("record");
                members.push(Member { symbol: walk.symbol(row), record: EntryView::read(bytes).expect("entry").record });
            }
            let pg = page().expect("page size");
            let segment = lay_out_positions(&members, Kind::Bundle, pg as u64, alloc, |_, row| {
                Ok(lazy.source_position(ItemKey(row.item), row.site).expect("source position"))
            })
            .expect("lay out");
            let mut memory = Code::map(segment.size as usize).expect("map");
            let base = memory.base() as u64;
            let mut image = Image::new(base, pg as u64, alloc).expect("image");
            let placed = image.append(&segment).expect("append");
            assert!(placed.outside.is_empty(), "{label}: unexpected native import");
            memory.write(0, &segment.code).expect("write code");
            memory.write(segment.relro_at as usize, &segment.relro).expect("write relro");
            for write in placed.writes.iter() {
                memory.write((write.at - base) as usize, &write.value.to_le_bytes()).expect("write reloc");
            }
            let addr = image.home(Target { space: Space::Func, symbol: walk.symbol(0) }).expect("main");
            let main_fn: extern "C-unwind" fn() -> i32 = unsafe { core::mem::transmute(addr as usize) };
            assert_eq!(main_fn(), 42, "{label}: native execution returned wrong value");
        }
        #[cfg(not(target_arch = "aarch64"))]
        let _ = walk;
    };

    run_lazy_mode("LazyProgram + SecondCheck", false);
    run_lazy_mode("LazyProgram + FromRecord (blaze check -> blaze run)", true);
}

/// `ProjectWorld::check_borrows` (`blaze check`) expands concrete `inline for` loops and catches
/// type errors inside the unrolled loop body rather than silently checking only the probe block.
#[test]
fn project_world_checks_concrete_inline_for_diagnostics() {
    use blaze_build::Panic;
    use blaze_ladder::crates::{Inputs, Toolchain};
    use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
    use blaze_ladder::front2::source::NamesSource as _;
    use blaze_ladder::program_texts::ProgramTexts;
    use blaze_mem::Frame;

    let bad = [
        RECORDS_HEAD,
        "
struct Pos { pub x: i32, pub y: i32 }

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let p = Pos { x: 1, y: 2 };
    let mut bad_acc: bool = false;
    inline for (_name, field) in &p {
        bad_acc = *field;
    }
    0
}
",
    ]
    .concat();
    let files = [SourceFile { path: "src/main.rs", text: &bad }];
    let heap = blaze_own::testing::heap();
    let alloc = heap.alloc();
    let frame = Frame::new(blaze_mem::tag::Tag::Infer).expect("frame");
    let tc = Toolchain::none(alloc);
    let texts = ProgramTexts::new(&files, "src/main.rs", NEW, &tc, Panic::Abort).expect("program texts");
    let user = texts.user();
    let inputs = Inputs::new(&user, &tc).expect("inputs");
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).expect("names");
    let mut world = ProjectWorld::new(&names, &frame, &files, alloc).expect("project world");
    let mut main_item = None;
    (&names).each_item(0, &mut |it, _| {
        if (&names).name_of(it) == Some("main") {
            main_item = Some(it);
        }
    });
    let main_item = main_item.expect("main item");
    let (verdict, _) = world.check_borrows(BodyRef::Free(main_item), alloc);
    assert!(
        !verdict.diagnostics.is_empty(),
        "expected type error inside concrete inline for body, got clean verdict"
    );
}

/// First-class `where for<F in fields(T)> F: Trait` bounds on generic `inline for` functions
/// (structs, structural records with spread, tuples, generic-to-generic forwarding, `zip_fields`)
/// AND recursive blanket impls (`impl<T> FieldCodec for T where for<F in fields(T)> F: FieldCodec`)
/// alongside primitive impls on nested structs, compiled and executed natively via both
/// `Front2Program` and `LazyProgram`.
#[test]
fn for_fields_bounded_generic_inline_for_and_recursive_blanket_impl() {
    use blaze_build::{OverflowChecks, Panic, UbChecks};
    use blaze_emit::{Isa, Os};
    use blaze_ids::Hasher;
    use blaze_ids::producer::{CodeBuilder, Producers};
    use blaze_ladder::body_ids::{BodyId, BodyIds, Fixture, Make, Read, RowKind};
    use blaze_ladder::code_records::{Builder, CodeStore, EntryView, MemoryStore, Settings, TierName};
    use blaze_ladder::crates::{Inputs, Toolchain};
    use blaze_ladder::front2::keys::{checker_of, member_key_of};
    use blaze_ladder::front2::program::Front2Program;
    use blaze_ladder::front2::program::lazy::LazyProgram;
    use blaze_ladder::front2::program::lazy::typed::{TypedGap, TypedRecords};
    use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
    use blaze_ladder::program_texts::ProgramTexts;
    use blaze_ladder::run::RunError;
    use blaze_ladder::run::make_one::{CheckCount, FromRecord, MakerState, SecondCheck, Typing, make_instance, walk_records};
    use blaze_ladder::seam::FrontEnd;
    use blaze_mem::Frame;
    use blaze_own::{AString, AVec};
    use blaze_toolchain::ToolchainId;
    use blaze_typeck::ItemKey;

    let body = "
impl Copy for usize {}
impl Copy for i32 {}
impl Copy for bool {}

#[lang = \"drop\"]
trait Drop {
    fn drop(&mut self);
}

#[repr(C)]
struct Words {
    data: usize,
    len: usize,
}
impl Copy for Words {}

union Wide {
    s: &'static str,
    bytes: &'static [u8],
    words: Words,
}

fn str_len(s: &'static str) -> i32 {
    unsafe { Wide { s }.words.len as i32 }
}

fn str_eq(a: &'static str, b: &'static str) -> bool {
    let na = unsafe { Wide { s: a }.words.len };
    let nb = unsafe { Wide { s: b }.words.len };
    if na != nb {
        return false;
    }
    let ab = unsafe { Wide { s: a }.bytes };
    let bb = unsafe { Wide { s: b }.bytes };
    let mut i: usize = 0;
    while i < na {
        if ab[i] != bb[i] {
            return false;
        }
        i = i + 1;
    }
    true
}

trait FieldCodec {
    fn json_len(&self) -> i32;
    fn patch_i32_self(&mut self, val: i32) -> bool;
}

impl FieldCodec for i32 {
    fn json_len(&self) -> i32 { *self }
    fn patch_i32_self(&mut self, val: i32) -> bool {
        *self = val;
        true
    }
}

impl FieldCodec for bool {
    fn json_len(&self) -> i32 {
        if *self { 4 } else { 5 }
    }
    fn patch_i32_self(&mut self, _val: i32) -> bool { false }
}

impl<T> FieldCodec for T
where
    for<F in fields(T)> F: FieldCodec,
{
    fn json_len(&self) -> i32 {
        let mut total: i32 = 0;
        inline for (_name, field) in self {
            total = total + field.json_len();
        }
        total
    }
    fn patch_i32_self(&mut self, val: i32) -> bool {
        let mut changed = false;
        inline for (_name, field) in self {
            if field.patch_i32_self(val) {
                changed = true;
            }
        }
        changed
    }
}

struct Pos { pub x: i32, pub y: i32 }
struct Health { pub hp: i32, pub max_hp: i32 }
struct Ship { pub pos: Pos, pub hp: Health, pub active: bool }
type Base = { a: i32, b: i32 };
type Extended = { ..Base, flag: bool };

fn to_json_len<T>(v: &T) -> i32
where
    for<F in fields(T)> F: FieldCodec,
{
    let mut total: i32 = 0;
    inline for (name, field) in v {
        total = total + str_len(name) + field.json_len();
    }
    total
}

fn outer<U>(u: &U) -> i32
where
    for<F in fields(U)> F: FieldCodec,
{
    to_json_len(u)
}

fn patch_i32<T>(dst: &mut T, key: &'static str, val: i32) -> bool
where
    for<F in fields(T)> F: FieldCodec,
{
    inline for (name, field) in dst {
        if str_eq(name, key) {
            return field.patch_i32_self(val);
        }
    }
    false
}

fn sum_pair_json<T>(a: &T, b: &T) -> i32
where
    for<F in fields(T)> F: FieldCodec,
{
    let mut total: i32 = 0;
    inline for (fa, fb) in zip_fields(a, b) {
        total = total + fa.json_len() + fb.json_len();
    }
    total
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let mut p = Pos { x: 10, y: 20 };
    if to_json_len(&p) != 32 { return 1; }
    if outer(&p) != 32 { return 2; }
    if !patch_i32(&mut p, \"x\", 15) { return 3; }
    if p.x != 15 || p.y != 20 { return 4; }
    if patch_i32(&mut p, \"missing\", 99) { return 5; }

    let mut h = Health { hp: 3, max_hp: 7 };
    if to_json_len(&h) != 18 { return 6; }
    if !patch_i32(&mut h, \"hp\", 5) { return 7; }

    let rec: Extended = { a: 2, b: 4, flag: true };
    if to_json_len(&rec) != 16 { return 8; }
    if rec.json_len() != 10 { return 9; }

    let tup = (6i32, false);
    if to_json_len(&tup) != 13 { return 10; }
    if tup.json_len() != 11 { return 11; }

    let n: i32 = 7;
    let b: bool = false;
    if n.json_len() != 7 || b.json_len() != 5 { return 12; }

    let mut ship = Ship { pos: p, hp: h, active: true };
    if ship.json_len() != 51 { return 13; }
    if to_json_len(&ship) != 62 { return 14; }

    let other = Pos { x: 1, y: 2 };
    if sum_pair_json(&ship.pos, &other) != 38 { return 15; }

    if !ship.patch_i32_self(10) { return 16; }
    ship.json_len()
}
";
    // 1. Whole-program `Front2Program` baseline.
    assert_eq!(run_head(RECORDS_HEAD, body), Ok(44));

    // 2. Build `Names` once for `ProjectWorld` and `LazyProgram`.
    let heap = blaze_own::testing::heap();
    let alloc = heap.alloc();
    let mut text = AString::with_capacity_in(RECORDS_HEAD.len() + body.len(), alloc);
    text.push_str(RECORDS_HEAD);
    text.push_str(body);
    let files = [SourceFile { path: "src/main.rs", text: text.as_str() }];
    let Ok(frame) = Frame::new(blaze_mem::tag::Tag::Infer) else {
        assert!(false, "frame");
        return;
    };
    let tc = Toolchain::none(alloc);
    let Ok(texts) = ProgramTexts::new(&files, "src/main.rs", NEW, &tc, Panic::Abort) else {
        assert!(false, "program texts");
        return;
    };
    let user = texts.user();
    let Ok(inputs) = Inputs::new(&user, &tc) else {
        assert!(false, "inputs");
        return;
    };
    let crates = inputs.crates();
    let Ok(names) = inputs.resolve(&crates, blaze_own::testing::bump()) else {
        assert!(false, "names");
        return;
    };

    // Pre-check every body (both free fns and impl methods) with `ProjectWorld::check_borrows`.
    let mut encoded_records: AVec<'static, (ItemKey, AVec<'static, u8>)> = AVec::new_in(alloc);
    {
        let Ok(check_frame) = Frame::new(blaze_mem::tag::Tag::Infer) else {
            assert!(false, "check frame");
            return;
        };
        let Ok(mut world) = ProjectWorld::new(&names, &check_frame, &files, alloc) else {
            assert!(false, "project world");
            return;
        };
        let bodies = world.bodies(alloc);
        for &(body_ref, _) in bodies.iter() {
            let key = match body_ref {
                BodyRef::Free(item) => checker_of(item),
                BodyRef::Member { container, kind, name, .. } => member_key_of(checker_of(container), kind, name),
            };
            let (verdict, _) = world.check_borrows(body_ref, alloc);
            assert!(verdict.diagnostics.is_empty(), "{body_ref:?}: check diagnostics: {:?}", verdict.diagnostics);
            assert!(verdict.gaps.is_empty(), "{body_ref:?}: check gaps: {:?}", verdict.gaps);
            assert!(verdict.refused.is_none(), "{body_ref:?}: check refused: {:?}", verdict.refused);
            let Some(Ok(bytes)) = verdict.typed else {
                assert!(false, "{body_ref:?}: missing clean typed record");
                return;
            };
            let mut copy = AVec::with_capacity_in(bytes.len(), alloc);
            copy.extend_from_slice(bytes.as_slice());
            encoded_records.push((key, copy));
        }
    }

    struct EveryItem;
    static READS: [Read; 1] = [Read { fact: [5; 32], digest: [6; 32] }];
    impl BodyIds for EveryItem {
        fn body_check(&self, item: ItemKey) -> Option<BodyId<'_>> {
            Some(BodyId { key: Hasher::new("for-fields lazy test").part(&item.0.to_le_bytes()).finish(), reads: &READS })
        }
        fn today(&self, fact: &[u8; 32]) -> Option<[u8; 32]> {
            (*fact == READS[0].fact).then_some(READS[0].digest)
        }
        fn crate_fact(&self, _: ItemKey) -> Option<Read> {
            Some(Read { fact: [10; 32], digest: [11; 32] })
        }
    }

    struct RefTyped<'r> {
        records: &'r [(ItemKey, AVec<'static, u8>)],
        alloc: blaze_own::Alloc<'static>,
    }
    impl TypedRecords for RefTyped<'_> {
        fn typed(&mut self, key: ItemKey) -> Result<blaze_typeck::Checked<'static>, TypedGap> {
            let Some((_, bytes)) = self.records.iter().find(|(k, _)| *k == key) else {
                return Err(TypedGap::NoRecord { key });
            };
            blaze_typeck::checked_record::decode(bytes.as_slice(), self.alloc).map_err(|why| TypedGap::Record { key, why })
        }
    }

    struct LazyWalkMaker<'p, P, T> {
        program: &'p mut P,
        typing: T,
        state: Option<MakerState>,
        checks: CheckCount,
        isa: Isa,
        overflow: OverflowChecks,
        ub: UbChecks,
        panic: Panic,
        alloc: blaze_own::Alloc<'static>,
    }
    impl<P: FrontEnd, T: Typing<P>> Make for LazyWalkMaker<'_, P, T> {
        type Error = RunError;
        type Grant = Fixture;
        fn make(&mut self, _grant: &Fixture, tabled: ItemKey, name: &[u8], entry: &mut AVec<'_, u8>) -> Result<(), Self::Error> {
            if self.state.is_none() {
                self.state = Some(MakerState::new(self.program, tabled, self.isa, self.overflow, self.ub, self.panic, self.alloc)?);
            }
            let Some(state) = self.state.as_mut() else {
                return Err(RunError::OneBody { key: tabled, why: "maker state missing" });
            };
            make_instance(self.program, &mut self.typing, state, &mut self.checks, tabled, name, entry)
        }
        fn make_static(&mut self, _grant: &Fixture, item: ItemKey, _: &mut AVec<'_, u8>) -> Result<(), Self::Error> {
            Err(RunError::OneBody { key: item, why: "unexpected static" })
        }
        fn make_thread_local(&mut self, _grant: &Fixture, item: ItemKey, _: &mut AVec<'_, u8>) -> Result<(), Self::Error> {
            Err(RunError::OneBody { key: item, why: "unexpected tls" })
        }
    }

    let Ok(whole) = Front2Program::new(&names, &frame, &files, alloc) else {
        assert!(false, "whole program");
        return;
    };
    let Some(entry_row) = whole.entry() else {
        assert!(false, "entry");
        return;
    };
    let entry = whole.function(entry_row).key;
    let producer = Producers::for_tests(b"for-fields lazy").get::<CodeBuilder>();
    let isa = Isa { lse: true, os: if cfg!(target_os = "macos") { Os::MacOs } else { Os::Linux } };
    let Ok(pkg_ed) = edition::PackageEdition::of_manifest("blaze-2026.11") else {
        assert!(false, "blaze edition");
        return;
    };
    let builder = Builder {
        isa,
        tier: TierName::Reference,
        settings: Settings::baseline(OverflowChecks::Off, Panic::Abort),
        toolchain: ToolchainId([8; 32]),
        producer,
        edition: pkg_ed,
    };

    let run_lazy_mode = |label: &str, use_records: bool| {
        let Ok(mut lazy) = LazyProgram::new(&names, &frame, &files, Some(entry), alloc) else {
            assert!(false, "{label}: lazy program");
            return;
        };
        if let Err(e) = lazy.decide_start(blaze_ladder::run::special::MakerStart::ByRule) {
            assert!(false, "{label}: decide_start: {e:?}");
            return;
        }
        let mut store = MemoryStore::new_in(alloc);
        let walked = if use_records {
            let mut maker = LazyWalkMaker {
                program: &mut lazy,
                typing: FromRecord(RefTyped { records: encoded_records.as_slice(), alloc }),
                state: None,
                checks: CheckCount::default(),
                isa,
                overflow: OverflowChecks::Off,
                ub: UbChecks::Off,
                panic: Panic::Abort,
                alloc,
            };
            walk_records(entry, &EveryItem, &mut store, &builder, &producer, &mut maker, alloc)
        } else {
            let mut maker = LazyWalkMaker {
                program: &mut lazy,
                typing: SecondCheck,
                state: None,
                checks: CheckCount::default(),
                isa,
                overflow: OverflowChecks::Off,
                ub: UbChecks::Off,
                panic: Panic::Abort,
                alloc,
            };
            walk_records(entry, &EveryItem, &mut store, &builder, &producer, &mut maker, alloc)
        };
        let Ok((walk, _)) = walked else {
            assert!(false, "{label}: walk_records failed: {:?}", walked.err());
            return;
        };
        #[cfg(target_arch = "aarch64")]
        {
            use blaze_code::image::{Image, Kind, Member, lay_out_positions};
            use blaze_code::record::{Space, Target};
            use blaze_launch_rt::sys::{Code, page};

            let mut members = AVec::new_in(alloc);
            for row in 0..walk.len() {
                assert_eq!(walk.kind(row), RowKind::Instance);
                let Some(rk) = walk.key(row) else {
                    assert!(false, "{label}: missing record key");
                    return;
                };
                let Ok(Some(bytes)) = store.get(rk) else {
                    assert!(false, "{label}: missing record");
                    return;
                };
                let Ok(view) = EntryView::read(bytes) else {
                    assert!(false, "{label}: bad entry");
                    return;
                };
                members.push(Member { symbol: walk.symbol(row), record: view.record });
            }
            let Ok(pg) = page() else {
                assert!(false, "{label}: page size");
                return;
            };
            let Ok(pg64) = u64::try_from(pg) else {
                assert!(false, "{label}: page size u64");
                return;
            };
            let Ok(segment) = lay_out_positions(members.as_slice(), Kind::Bundle, pg64, alloc, |_, row| {
                lazy.source_position(ItemKey(row.item), row.site)
                    .map_err(|_| blaze_code::image::ImageError::Base { base: 0 })
            }) else {
                assert!(false, "{label}: lay out");
                return;
            };
            let Ok(seg_size) = usize::try_from(segment.size) else {
                assert!(false, "{label}: segment size");
                return;
            };
            let Ok(mut memory) = Code::map(seg_size) else {
                assert!(false, "{label}: map");
                return;
            };
            let Ok(base) = u64::try_from(memory.base()) else {
                assert!(false, "{label}: base");
                return;
            };
            let Ok(mut image) = Image::new(base, pg64, alloc) else {
                assert!(false, "{label}: image");
                return;
            };
            let Ok(placed) = image.append(&segment) else {
                assert!(false, "{label}: append");
                return;
            };
            assert!(placed.outside.is_empty(), "{label}: unexpected native import");
            assert!(memory.write(0, &segment.code).is_ok());
            if let Ok(relro_at) = usize::try_from(segment.relro_at) {
                assert!(memory.write(relro_at, &segment.relro).is_ok());
            }
            for write in placed.writes.iter() {
                if let Ok(off) = usize::try_from(write.at.saturating_sub(base)) {
                    assert!(memory.write(off, &write.value.to_le_bytes()).is_ok());
                }
            }
            let Some(addr) = image.home(Target { space: Space::Func, symbol: walk.symbol(0) }) else {
                assert!(false, "{label}: main symbol");
                return;
            };
            let Ok(addr_usize) = usize::try_from(addr) else {
                assert!(false, "{label}: addr usize");
                return;
            };
            let main_fn: extern "C-unwind" fn() -> i32 = unsafe { core::mem::transmute(addr_usize) };
            assert_eq!(main_fn(), 44, "{label}: native execution returned wrong value");
        }
        #[cfg(not(target_arch = "aarch64"))]
        let _ = walk;
    };

    run_lazy_mode("LazyProgram + SecondCheck (for_fields)", false);
    run_lazy_mode("LazyProgram + FromRecord (for_fields)", true);
}

/// Proves that `where for<F in fields(T)> F: Trait` rejects:
/// 1. Bad call sites passing a struct with a field that does not implement `FieldCodec`.
/// 2. Broken generic `inline for` bodies calling a method not in `FieldCodec` AT DEFINITION TIME
///    even with zero callers anywhere in the crate (both single `inline for` and `zip_fields`).
#[test]
fn for_fields_rejects_bad_call_site_and_unbounded_body_at_definition_time() {
    use blaze_build::Panic;
    use blaze_ladder::crates::{Inputs, Toolchain};
    use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
    use blaze_ladder::front2::source::NamesSource as _;
    use blaze_ladder::program_texts::ProgramTexts;
    use blaze_mem::Frame;
    use blaze_own::AString;

    let check_fn_fails = |label: &str, body_src: &str, target_fn: &str| {
        let heap = blaze_own::testing::heap();
        let alloc = heap.alloc();
        let mut src = AString::with_capacity_in(RECORDS_HEAD.len() + body_src.len(), alloc);
        src.push_str(RECORDS_HEAD);
        src.push_str(body_src);
        let files = [SourceFile { path: "src/main.rs", text: src.as_str() }];
        let Ok(frame) = Frame::new(blaze_mem::tag::Tag::Infer) else {
            assert!(false, "{label}: frame");
            return;
        };
        let tc = Toolchain::none(alloc);
        let Ok(texts) = ProgramTexts::new(&files, "src/main.rs", NEW, &tc, Panic::Abort) else {
            assert!(false, "{label}: program texts");
            return;
        };
        let user = texts.user();
        let Ok(inputs) = Inputs::new(&user, &tc) else {
            assert!(false, "{label}: inputs");
            return;
        };
        let crates = inputs.crates();
        let Ok(names) = inputs.resolve(&crates, blaze_own::testing::bump()) else {
            assert!(false, "{label}: names");
            return;
        };
        let Ok(mut world) = ProjectWorld::new(&names, &frame, &files, alloc) else {
            assert!(false, "{label}: project world");
            return;
        };
        let mut target_item = None;
        (&names).each_item(0, &mut |it, _| {
            if (&names).name_of(it) == Some(target_fn) {
                target_item = Some(it);
            }
        });
        let Some(target_item) = target_item else {
            assert!(false, "{label}: missing target fn `{target_fn}`");
            return;
        };
        let (verdict, _) = world.check_borrows(BodyRef::Free(target_item), alloc);
        assert!(
            !verdict.diagnostics.is_empty(),
            "{label}: expected diagnostics for `{target_fn}`, got clean verdict (refused: {:?})",
            verdict.refused
        );
    };

    // 1. Call-site rejection: `Opaque` does not implement `FieldCodec`.
    let bad_call = "
trait FieldCodec {
    fn json_len(&self) -> i32;
}
impl FieldCodec for i32 {
    fn json_len(&self) -> i32 { *self }
}
struct Opaque(i32);
struct Bad { pub x: i32, pub h: Opaque }

fn to_json_len<T>(v: &T) -> i32
where
    for<F in fields(T)> F: FieldCodec,
{
    let mut total: i32 = 0;
    inline for (_name, f) in v {
        total = total + f.json_len();
    }
    total
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let b = Bad { x: 1, h: Opaque(7) };
    to_json_len(&b)
}
";
    check_fn_fails("bad call site", bad_call, "main");
    assert!(run_head(RECORDS_HEAD, bad_call).is_err(), "expected bad call site to fail compilation");

    // 2. Definition-time rejection with ZERO callers: calling `f.does_not_exist()` inside a
    //    `ForFields`-bounded generic `inline for` body must fail `check_borrows` immediately.
    let bad_body = "
trait FieldCodec {
    fn json_len(&self) -> i32;
}
impl FieldCodec for i32 {
    fn json_len(&self) -> i32 { *self }
}

fn broken<T>(v: &T)
where
    for<F in fields(T)> F: FieldCodec,
{
    inline for (_, f) in v {
        f.does_not_exist();
    }
}

fn broken_zip<T>(a: &T, b: &T)
where
    for<F in fields(T)> F: FieldCodec,
{
    inline for (fa, fb) in zip_fields(a, b) {
        fa.does_not_exist(fb);
    }
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    0
}
";
    check_fn_fails("broken single inline for (zero callers)", bad_body, "broken");
    check_fn_fails("broken zip_fields inline for (zero callers)", bad_body, "broken_zip");
}
