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
