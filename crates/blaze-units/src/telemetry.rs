//! Integer telemetry: [`Nanos<K>`], [`Bytes<K>`], [`Count<K>`], each over `u64`. No `+` or `-`
//! operator: an operator on `u64` wraps silently in a release build, so every sum and difference
//! is a named method that says what happens at the edge (`checked_*` gives `None`,
//! `saturating_*` stops at the edge). No operation mixes kinds: `Nanos<Wall>` and `Nanos<Cpu>`
//! have no sum, and a ratio is of one kind over the same kind.
//!
//! # `blaze_int::time` and this module: the plan for its owner (rule 30)
//!
//! Today `blaze_int::time` has `Nanos(pub u64)` (an instant OR a length of time, untyped),
//! `Deadline(Nanos)` and `trait Clock { fn now(&self) -> Nanos; }`; blaze-text depends on
//! blaze-int and blaze-units depends on blaze-text, so blaze-units cannot be below blaze-int.
//! The one home for time is REPLACE, not wrap, in four steps, each a whole port (rule 24):
//!
//! 1. blaze-int's owner adds `Instant(u64)` beside `Nanos` in `blaze_int::time` (nanoseconds
//!    since a clock's origin), changes `Clock::now` to return `Instant` and `Deadline` to hold
//!    one, with `Instant::since(self, earlier) -> u64` and `Instant::after(self, ns: u64)`.
//!    Every caller that used `Nanos` as an instant moves to `Instant`; what is left of
//!    `blaze_int::time::Nanos` is lengths of time.
//! 2. Every remaining use of `blaze_int::time::Nanos` (a length) moves to
//!    `blaze_units::Nanos<Wall>` (or `<Cpu>` where it is processor time), and
//!    `Instant::since` returns `Nanos<Wall>`. That needs `Instant`, `Clock` and `Deadline` to
//!    move to a crate that may depend on blaze-units: they move to `blaze_units::time` (this
//!    crate), which depends on nothing that depends on blaze-int's time module.
//! 3. `blaze_int::time` is deleted, and a ban on the old path is added (rule 30); blaze-int
//!    keeps only integer conversions, so blaze-text's dependency on it stays acyclic.
//! 4. blaze-sim's `flight::Nanos` (its own virtual-clock time) moves to `Instant` in the same
//!    batch.
//!
//! Until step 2, the two types meet at one place, `Nanos::<Wall>::new(blaze_int_nanos.0)`.

use core::{marker::PhantomData, ops::Div};

use crate::kind::{ByteKind, Counted, TimeKind};

macro_rules! telemetry {
    ($(#[$doc:meta])* $name:ident<K: $bound:ident>, $field:ident, $unit:literal) => {
        $(#[$doc])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
        pub struct $name<K: $bound> {
            $field: u64,
            kind: PhantomData<K>,
        }

        impl<K: $bound> $name<K> {
            /// None.
            pub const ZERO: $name<K> = $name { $field: 0, kind: PhantomData };

            #[doc = concat!("`n` ", $unit, ".")]
            pub const fn new(n: u64) -> $name<K> {
                $name { $field: n, kind: PhantomData }
            }

            #[doc = concat!("The ", $unit, ".")]
            pub const fn get(self) -> u64 {
                self.$field
            }

            /// The sum, or `None` past `u64::MAX`.
            pub const fn checked_add(self, o: $name<K>) -> Option<$name<K>> {
                match self.$field.checked_add(o.$field) {
                    Some(v) => Some($name::new(v)),
                    None => None,
                }
            }

            /// The sum, stopping at `u64::MAX`.
            pub const fn saturating_add(self, o: $name<K>) -> $name<K> {
                $name::new(self.$field.saturating_add(o.$field))
            }

            /// The difference, or `None` below zero.
            pub const fn checked_sub(self, o: $name<K>) -> Option<$name<K>> {
                match self.$field.checked_sub(o.$field) {
                    Some(v) => Some($name::new(v)),
                    None => None,
                }
            }

            /// The difference, stopping at zero.
            pub const fn saturating_sub(self, o: $name<K>) -> $name<K> {
                $name::new(self.$field.saturating_sub(o.$field))
            }

            /// `k` times this, or `None` past `u64::MAX`.
            pub const fn checked_mul(self, k: u64) -> Option<$name<K>> {
                match self.$field.checked_mul(k) {
                    Some(v) => Some($name::new(v)),
                    None => None,
                }
            }

            /// `k` times this, stopping at `u64::MAX`.
            pub const fn saturating_mul(self, k: u64) -> $name<K> {
                $name::new(self.$field.saturating_mul(k))
            }

            /// This divided by `k`, rounded down, or `None` for `k == 0`.
            pub const fn checked_div(self, k: u64) -> Option<$name<K>> {
                match self.$field.checked_div(k) {
                    Some(v) => Some($name::new(v)),
                    None => None,
                }
            }
        }

        impl<K: $bound> Div for $name<K> {
            type Output = f64;
            /// The ratio of two of one kind, as f64 (`inf` or NaN over zero, as IEEE says).
            fn div(self, o: $name<K>) -> f64 {
                self.$field as f64 / o.$field as f64
            }
        }
    };
}

telemetry! {
    /// A length of time in nanoseconds, of a kind: `Nanos<Wall>` or `Nanos<Cpu>`.
    Nanos<K: TimeKind>, ns, "nanoseconds"
}

telemetry! {
    /// A number of bytes, of a kind (`Bytes<Resident>`, `Bytes<Read>`, ...).
    Bytes<K: ByteKind>, bytes, "bytes"
}

telemetry! {
    /// A number of things, the kind naming what (`Count<Cells>`).
    Count<K: Counted>, n, "things"
}
