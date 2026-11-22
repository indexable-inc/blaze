//! Every pair of the table, made from the same rows as the impls, at its edges: the source's
//! minimum and maximum and one inside each, 0, 1 and its wrap of -1, its middle, and the
//! target's minimum and maximum and one past each where the source holds them. Against std as
//! the oracle: `narrow` agrees with `TryFrom`, `truncate` and `widen` with `as`, a widening
//! round-trips through `TryFrom`, `saturate` is `TryFrom` clamped, `narrow_or_stop` is
//! `narrow` where it fits, and a narrowing of two
//! fixed-width types fails at some edge (else it would be a widening the table missed). The
//! table names each other type once per source, and `truncate`'s targets are `narrow`'s with
//! no more bits.

use std::{panic, string::String};

use crate::{Cast, Int, Narrowed, Value, idx, len32};

/// The 12 primitive integer types.
const TYPES: [&str; 12] = ["u8", "u16", "u32", "u64", "u128", "usize", "i8", "i16", "i32", "i64", "i128", "isize"];

/// The values a pair `$s -> $t` is tried at, as an iterator of `$s`.
macro_rules! edges {
    ($s:ident, $t:ident) => {{
        let around = |edge: Option<$s>| [edge, edge.map(|e| e.wrapping_add(1)), edge.map(|e| e.wrapping_sub(1))];
        let [t_max, t_max_up, t_max_down] = around(<$s>::try_from(<$t>::MAX).ok());
        let [t_min, t_min_up, t_min_down] = around(<$s>::try_from(<$t>::MIN).ok());
        [
            Some(<$s>::MIN),
            Some(<$s>::MIN.wrapping_add(1)),
            Some(<$s>::MAX.wrapping_sub(1)),
            Some(<$s>::MAX),
            Some(0),
            Some(1),
            Some(<$s>::wrapping_sub(0, 1)),
            Some(<$s>::MAX / 2),
            Some(<$s>::MAX / 2 + 1),
            t_max,
            t_max_up,
            t_max_down,
            t_min,
            t_min_up,
            t_min_down,
        ]
        .into_iter()
        .flatten()
    }};
}

macro_rules! check_pairs {
    ($(
        $s:ident $sign:ident: widen [$($w:ident),*] widen_ptr [$($p:ident),*]
            narrow [$($n:ident),*] truncate [$($t:ident),*];
    )*) => {
        $(
            $( check_widen!($s, $w); )*
            $( check_widen!($s, $p); )*
            $( check_narrow!($s, $n); )*
            $( check_truncate!($s, $t); )*
            let named: &[&str] = &[$(stringify!($w),)* $(stringify!($p),)* $(stringify!($n),)*];
            let narrowed: &[&str] = &[$(stringify!($n),)*];
            let truncated: &[&str] = &[$(stringify!($t),)*];
            each_other_once(stringify!($s), named);
            for it in truncated {
                assert!(narrowed.contains(it), "{} -> {it} truncates but is no narrowing", stringify!($s));
            }
        )*
    };
}

macro_rules! check_widen {
    ($s:ident, $t:ident) => {
        for v in edges!($s, $t) {
            let wide: $t = Cast::<$s, $t>::widen(v);
            assert_eq!(wide, v as $t, "{} -> {} widen of {v} is not `as`", stringify!($s), stringify!($t));
            assert_eq!(<$s>::try_from(wide).ok(), Some(v), "{} -> {} widen lost {v}", stringify!($s), stringify!($t));
            assert_eq!(v.widen_to::<$t>(), wide);
        }
    };
}

macro_rules! check_narrow {
    ($s:ident, $t:ident) => {
        let mut failed = false;
        for v in edges!($s, $t) {
            let got = Cast::<$s, $t>::narrow(v);
            let oracle = <$t>::try_from(v).ok();
            assert_eq!(got.ok(), oracle, "{} -> {} narrow of {v} is not `TryFrom`'s", stringify!($s), stringify!($t));
            assert_eq!(v.narrow::<$t>(), got);
            let clamped = oracle.unwrap_or(if v < 0 { <$t>::MIN } else { <$t>::MAX });
            assert_eq!(Cast::<$s, $t>::saturate(v), clamped, "{} -> {} saturate of {v}", stringify!($s), stringify!($t));
            assert_eq!(v.saturate::<$t>(), clamped);
            match got {
                Ok(it) => {
                    assert_eq!(v.narrow_or_stop::<$t>(), it);
                    assert_eq!(Cast::<$s, $t>::narrow_or_stop(v), it);
                }
                Err(e) => {
                    failed = true;
                    let value = if <$s>::MIN == 0 {
                        Value::Unsigned(u128::try_from(v).expect("an unsigned value"))
                    } else {
                        Value::Signed(i128::try_from(v).expect("a signed value"))
                    };
                    assert_eq!(e, Narrowed { value, from: stringify!($s), to: stringify!($t) });
                }
            }
        }
        // Only a pair with a pointer-sized side can never fail on this host (`u64 -> usize`
        // on a 64-bit one): it narrows on the other width.
        let pointer = [stringify!($s), stringify!($t)].iter().any(|it| *it == "usize" || *it == "isize");
        assert!(failed || pointer, "{} -> {} never fails at its edges: it is a widening", stringify!($s), stringify!($t));
    };
}

macro_rules! check_truncate {
    ($s:ident, $t:ident) => {
        assert!(<$t>::BITS <= <$s>::BITS, "{} -> {} truncates into more bits", stringify!($s), stringify!($t));
        for v in edges!($s, $t) {
            assert_eq!(Cast::<$s, $t>::truncate(v), v as $t, "{} -> {} truncate of {v} is not `as`", stringify!($s), stringify!($t));
            assert_eq!(v.truncate_to::<$t>(), v as $t);
        }
    };
}

/// `named`, a source's targets, are the 11 other types, each once.
fn each_other_once(source: &str, named: &[&str]) {
    for it in TYPES {
        let times = named.iter().filter(|n| **n == it).count();
        let want = usize::from(it != source);
        assert_eq!(times, want, "the table's row {source} names {it} {times} times");
    }
    assert_eq!(named.len(), TYPES.len() - 1, "the table's row {source}: {named:?}");
}

#[test]
#[allow(unused_comparisons, reason = "`v < 0` of an unsigned source, the table's one body for all")]
fn every_pair_at_its_edges() {
    table!(check_pairs);
}

#[test]
fn narrow_or_stop_names_the_value_and_both_types() {
    let caught = panic::catch_unwind(|| 5_000_000_000u64.narrow_or_stop::<u32>());
    let payload = caught.expect_err("5000000000 does not fit in u32");
    let said = payload.downcast_ref::<String>().expect("a formatted message");
    assert!(said.contains("the u64 5000000000 does not fit in u32"), "{said}");
    // The `const` form names the types; it cannot format the value.
    let caught = panic::catch_unwind(|| Cast::<i32, u8>::narrow_or_stop(-1));
    let payload = caught.expect_err("-1 does not fit in u8");
    let said = payload.downcast_ref::<&str>().expect("a literal message");
    assert!(said.contains("a i32 value does not fit in u8"), "{said}");
}

#[test]
fn conversions_are_const() {
    const NARROWED: u32 = Cast::<u64, u32>::narrow_or_stop(7);
    const WIDE: u64 = Cast::<u32, u64>::widen(u32::MAX);
    const LOW: u8 = Cast::<u32, u8>::truncate(0x1234);
    const CLAMPED: i8 = Cast::<i64, i8>::saturate(-1000);
    const INDEX: usize = idx(3);
    const TOO_LONG: Result<u32, Narrowed> = len32(usize::MAX);
    assert_eq!((NARROWED, WIDE, LOW, CLAMPED, INDEX), (7, 4_294_967_295, 0x34, -128, 3));
    let e = TOO_LONG.expect_err("usize::MAX is longer than u32::MAX");
    assert_eq!(e, Narrowed { value: Value::Unsigned(u128::from(u64::MAX)), from: "usize", to: "u32" });
    assert_eq!(std::format!("{e}"), "the usize 18446744073709551615 does not fit in u32");
    assert_eq!(len32(5), Ok(5));
    let negative = (-3i64).narrow::<u16>().expect_err("-3 is not a u16");
    assert_eq!(std::format!("{negative}"), "the i64 -3 does not fit in u16");
}

/// What `as` does in one step across a sign and a width is two named steps: sign-extend then
/// reinterpret, or reinterpret then zero-extend.
#[test]
fn a_cast_across_sign_and_width_is_two_steps() {
    for v in [i32::MIN, -1, 0, 1, i32::MAX] {
        assert_eq!(v.widen_to::<i64>().cast_unsigned(), v as u64);
        assert_eq!(v.cast_unsigned().widen_to::<u64>(), u64::from(v as u32));
    }
}

/// Time as a number: the units saturate, a duration round-trips where it fits and clamps where
/// it does not, and a deadline's edges (`PASSED`, `NEVER`, one past the clock's end) hold.
#[test]
fn nanos_and_deadlines_saturate_at_their_edges() {
    use core::time::Duration;

    use crate::time::{Clock, Deadline, Nanos};

    assert_eq!(Nanos::from_secs(2), Nanos(2_000_000_000));
    assert_eq!(Nanos::from_millis(3), Nanos(3_000_000));
    assert_eq!(Nanos::from_micros(4), Nanos(4_000));
    assert_eq!(Nanos::from_secs(u64::MAX), Nanos::MAX);
    for d in [Duration::ZERO, Duration::from_nanos(1), Duration::from_secs(7), Duration::from_nanos(u64::MAX)] {
        assert_eq!(Nanos::from_duration(d).to_duration(), d);
    }
    assert_eq!(Nanos::from_duration(Duration::MAX), Nanos::MAX);
    assert_eq!(Nanos(5).since(Nanos(7)), Nanos::ZERO);
    assert_eq!(Nanos(7).since(Nanos(5)), Nanos(2));
    assert_eq!(Nanos::MAX.checked_add(Nanos(1)), None);
    assert_eq!(Nanos::MAX.saturating_add(Nanos(1)), Nanos::MAX);

    let now = Nanos(100);
    assert!(Deadline::PASSED.passed(now));
    assert!(Deadline::PASSED.passed(Nanos::ZERO));
    assert!(!Deadline::NEVER.passed(now));
    assert_eq!(Deadline::NEVER.finite(), None);
    assert_eq!(Deadline::after(Nanos::MAX, Nanos(1)), Deadline::NEVER);
    let d = Deadline::after(now, Nanos(50));
    assert_eq!(d.get(), Nanos(150));
    assert_eq!(d.finite(), Some(d));
    assert_eq!(d.left(now), Nanos(50));
    assert_eq!(d.left(Nanos(200)), Nanos::ZERO);
    assert!(!d.passed(Nanos(149)));
    assert!(d.passed(Nanos(150)));
    assert_eq!(d.min(Deadline::NEVER), d);
    assert_eq!(Deadline::NEVER.min(d), d);
    assert_eq!(Deadline::at(Nanos(9)), Deadline::after(Nanos(4), Nanos(5)));

    /// A test's clock: each read one tick after the last.
    struct Ticks(core::cell::Cell<u64>);
    impl Clock for Ticks {
        fn now(&self) -> Nanos {
            self.0.set(self.0.get() + 1);
            Nanos(self.0.get())
        }
    }
    fn first_two(clock: &impl Clock) -> (Nanos, Nanos) {
        (clock.now(), clock.now())
    }
    let ticks = Ticks(core::cell::Cell::new(0));
    assert_eq!(first_two(&ticks), (Nanos(1), Nanos(2)));
    assert_eq!(first_two(&&ticks), (Nanos(3), Nanos(4)), "a reference to a clock is the clock");
}
