//! s4-err-05: arithmetic checks that panic, caught, with the pinned core's messages
//! (library/core/src/panicking.rs `panic_const!`: "attempt to add with overflow", "attempt to
//! subtract with overflow", "attempt to multiply with overflow", "attempt to divide with
//! overflow", "attempt to negate with overflow", "attempt to shift left with overflow",
//! "attempt to divide by zero", "attempt to calculate the remainder with a divisor of zero").
//! Overflow checks are on: `blaze run` builds the dev profile, whose `overflow-checks` default is
//! on (D101); division by zero and `MIN / -1` are checked in every profile (the Reference,
//! "Arithmetic and Logical Binary Operators"). Each payload is a `&'static str`
//! (`fmt::Arguments::from_str`, so `msg.as_str()` is `Some`). Then the explicit forms: checked,
//! wrapping, saturating, overflowing, and Euclidean division, which never panic.

use std::hint::black_box;
use std::panic::{self, UnwindSafe};

fn msg<R>(f: impl FnOnce() -> R + UnwindSafe) -> String {
    match panic::catch_unwind(f) {
        Ok(_) => String::from("no panic"),
        Err(p) => p.downcast_ref::<&str>().map(|s| s.to_string()).unwrap_or_else(|| String::from("not a &str")),
    }
}

fn main() {
    panic::set_hook(Box::new(|_| {}));
    println!("{}", msg(|| black_box(255u8) + black_box(1)));
    println!("{}", msg(|| black_box(0u32) - black_box(1)));
    println!("{}", msg(|| black_box(200u8) * black_box(2)));
    println!("{}", msg(|| black_box(i32::MIN) / black_box(-1)));
    println!("{}", msg(|| -black_box(i8::MIN)));
    println!("{}", msg(|| black_box(1u32) << black_box(40)));
    println!("{}", msg(|| black_box(5) / black_box(0)));
    println!("{}", msg(|| black_box(5) % black_box(0)));
    println!("{}", msg(|| black_box(254u8) + black_box(1)));
    println!("{:?} {:?}", i32::MAX.checked_add(1), 5u8.checked_sub(2));
    println!("{} {} {:?}", 250u8.wrapping_add(10), 250u8.saturating_add(10), 5u8.overflowing_sub(6));
    println!("{} {} {} {}", -7 / 2, -7 % 2, (-7i32).div_euclid(2), (-7i32).rem_euclid(3));
    println!("{}", i64::MAX.wrapping_mul(2));
}
