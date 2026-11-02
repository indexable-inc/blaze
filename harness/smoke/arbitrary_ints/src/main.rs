//! Integers of any width (`#![blaze::arbitrary_ints]`, docs/const.md#integers-of-any-width):
//! arithmetic, overflow, shifts and casts of `u3` and `i7`, compiled and in const eval. Each
//! line says `ok` when the compiled result is the one the profile gives (dev has overflow
//! checks, so an overflow panics naming the operation and the type; release wraps at the
//! width) and const eval of the same call gives the same value, so the output is the same in
//! both. A line that is not `ok` says what each gave.
#![blaze::arbitrary_ints]

use std::hint::black_box;
use std::mem::size_of;
use std::panic::{self, AssertUnwindSafe};

/// Whether this build has overflow checks: the smoke corpus builds dev, which has them, and
/// release, which doesn't, and neither changes `debug-assertions` apart from them.
const CHECKS: bool = cfg!(debug_assertions);

const fn add3(a: u3, b: u3) -> u3 {
    a + b
}
const fn sub3(a: u3, b: u3) -> u3 {
    a - b
}
const fn mul3(a: u3, b: u3) -> u3 {
    a * b
}
const fn div3(a: u3, b: u3) -> u3 {
    a / b
}
const fn rem3(a: u3, b: u3) -> u3 {
    a % b
}
const fn shl3(a: u3, b: u32) -> u3 {
    a << b
}
const fn shr3(a: u3, b: u32) -> u3 {
    a >> b
}
const fn not3(a: u3) -> u3 {
    !a
}
const fn add7(a: i7, b: i7) -> i7 {
    a + b
}
const fn sub7(a: i7, b: i7) -> i7 {
    a - b
}
const fn mul7(a: i7, b: i7) -> i7 {
    a * b
}
const fn div7(a: i7, b: i7) -> i7 {
    a / b
}
const fn rem7(a: i7, b: i7) -> i7 {
    a % b
}
const fn neg7(a: i7) -> i7 {
    -a
}
const fn shl7(a: i7, b: u32) -> i7 {
    a << b
}
const fn shr7(a: i7, b: u32) -> i7 {
    a >> b
}
const fn not7(a: i7) -> i7 {
    !a
}
const fn u8_to_u3(a: u8) -> u3 {
    a as u3
}
const fn u8_to_i7(a: u8) -> i7 {
    a as i7
}
const fn i7_to_u8(a: i7) -> u8 {
    a as u8
}
const fn i7_to_u3(a: i7) -> u3 {
    a as u3
}
const fn u3_to_i7(a: u3) -> i7 {
    a as i7
}
const fn i7_to_i16(a: i7) -> i16 {
    a as i16
}
const fn f64_to_u3(a: f64) -> u3 {
    a as u3
}
const fn f32_to_i7(a: f32) -> i7 {
    a as i7
}

/// What running `f` gave: its value, or its panic's message.
fn outcome(f: impl FnOnce() -> i16) -> Result<i16, String> {
    panic::catch_unwind(AssertUnwindSafe(f)).map_err(|payload| match payload.downcast::<&'static str>() {
        Ok(text) => (*text).to_owned(),
        Err(payload) => match payload.downcast::<String>() {
            Ok(text) => *text,
            Err(_) => "a panic with no message".to_owned(),
        },
    })
}

/// The line of `name`: compiled it gave `got`, it should give `want`, and const eval gave
/// `konst`, which must be `want`'s value when there is one.
fn report(name: &str, got: Result<i16, String>, want: Result<i16, String>, konst: Option<i16>) {
    let konst_agrees = match konst {
        Some(value) => want == Ok(value),
        None => true,
    };
    if got == want && konst_agrees {
        println!("{name}: ok");
    } else {
        println!("{name}: compiled {got:?}, want {want:?}, const eval {konst:?}");
    }
}

/// A call whose value is the same in both profiles, compiled and in const eval.
macro_rules! same {
    ($name:literal, $f:ident($($arg:expr),*), $value:expr) => {{
        const KONST: i16 = $f($($arg),*) as i16;
        let got = outcome(|| $f($(black_box($arg)),*) as i16);
        report($name, got, Ok($value), Some(KONST));
    }};
}

/// A call that overflows: it panics with `message` under overflow checks, and without them it
/// wraps to `wrapped`, which const eval of it (only then a constant) must give too.
macro_rules! wraps {
    ($name:literal, $f:ident($($arg:expr),*), $wrapped:expr, $message:literal) => {{
        #[cfg(not(debug_assertions))]
        const KONST: Option<i16> = Some($f($($arg),*) as i16);
        #[cfg(debug_assertions)]
        const KONST: Option<i16> = None;
        let got = outcome(|| $f($(black_box($arg)),*) as i16);
        let want = if CHECKS { Err($message.to_owned()) } else { Ok($wrapped) };
        report($name, got, want, KONST);
    }};
}

/// A call that panics with `message` whatever the setting (division by zero, `MIN / -1`).
macro_rules! panics {
    ($name:literal, $f:ident($($arg:expr),*), $message:literal) => {{
        let got = outcome(|| $f($(black_box($arg)),*) as i16);
        report($name, got, Err($message.to_owned()), None);
    }};
}

fn main() {
    panic::set_hook(Box::new(|_| {}));

    same!("u3 3 + 4", add3(3, 4), 7);
    wraps!("u3 7 + 1", add3(7, 1), 0, "attempt to add with overflow at type `u3`");
    wraps!("u3 0 - 1", sub3(0, 1), 7, "attempt to subtract with overflow at type `u3`");
    wraps!("u3 3 * 3", mul3(3, 3), 1, "attempt to multiply with overflow at type `u3`");
    same!("u3 7 / 2", div3(7, 2), 3);
    panics!("u3 7 / 0", div3(7, 0), "attempt to divide by zero at type `u3`");
    panics!("u3 7 % 0", rem3(7, 0), "attempt to calculate the remainder with a divisor of zero at type `u3`");
    same!("u3 5 << 1", shl3(5, 1), 2);
    wraps!("u3 3 << 4", shl3(3, 4), 6, "attempt to shift left with overflow at type `u3`");
    wraps!("u3 6 >> 3", shr3(6, 3), 6, "attempt to shift right with overflow at type `u3`");
    same!("u3 !5", not3(5), 2);

    same!("i7 -64 + 63", add7(-64, 63), -1);
    wraps!("i7 63 + 1", add7(63, 1), -64, "attempt to add with overflow at type `i7`");
    wraps!("i7 -64 - 1", sub7(-64, 1), 63, "attempt to subtract with overflow at type `i7`");
    wraps!("i7 9 * 8", mul7(9, 8), -56, "attempt to multiply with overflow at type `i7`");
    wraps!("i7 -64 * -1", mul7(-64, -1), -64, "attempt to multiply with overflow at type `i7`");
    wraps!("i7 -(-64)", neg7(-64), -64, "attempt to negate with overflow at type `i7`");
    same!("i7 -63 / 2", div7(-63, 2), -31);
    panics!("i7 -64 / -1", div7(-64, -1), "attempt to divide with overflow at type `i7`");
    panics!("i7 -64 % -1", rem7(-64, -1), "attempt to calculate the remainder with overflow at type `i7`");
    panics!("i7 1 % 0", rem7(1, 0), "attempt to calculate the remainder with a divisor of zero at type `i7`");
    same!("i7 -64 >> 6", shr7(-64, 6), -1);
    wraps!("i7 1 << 13", shl7(1, 13), -64, "attempt to shift left with overflow at type `i7`");
    same!("i7 !0", not7(0), -1);

    same!("13u8 as u3", u8_to_u3(13), 5);
    same!("200u8 as i7", u8_to_i7(200), -56);
    same!("-1i7 as u8", i7_to_u8(-1), 255);
    same!("-3i7 as u3", i7_to_u3(-3), 5);
    same!("7u3 as i7", u3_to_i7(7), 7);
    same!("-5i7 as i16", i7_to_i16(-5), -5);
    same!("9.5f64 as u3", f64_to_u3(9.5), 7);
    same!("NaN as u3", f64_to_u3(f64::NAN), 0);
    same!("-1000f32 as i7", f32_to_i7(-1000.0), -64);
    same!("63.9f32 as i7", f32_to_i7(63.9), 63);

    println!(
        "size_of u3 {}, i7 {}, u24 {}, u65 {}, Option<u7> {}",
        size_of::<u3>(),
        size_of::<i7>(),
        size_of::<u24>(),
        size_of::<u65>(),
        size_of::<Option<u7>>()
    );
}
