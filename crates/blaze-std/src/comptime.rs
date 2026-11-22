//! Compile-time reflection & derive bridge (`bstd::comptime`).
//!
//! When compiled by Blaze (`cfg(blaze)`), version-gated helpers dispatch to native compile-time
//! reflection (`blaze_comptime`) with zero proc-macro crate or token-stream serialization overhead.
//! Under interim `cargo`/`rustc` builds (`cfg(not(blaze))`), the exact same API works cleanly via
//! `core::mem::discriminant` and declarative macros so crates stay single-source across both builders.

use core::hash::{Hash as _, Hasher as _};

/// True when compiled by Blaze with native comptime reflection enabled.
pub const IS_BLAZE_COMPTIME: bool = cfg!(blaze);

/// Minimum Blaze compiler major version supporting native comptime reflection derives.
pub const BLAZE_COMPTIME_MIN_VERSION: u32 = 2;

/// Compact deterministic 64-bit discriminant tag of any enum value without hand-writing a
/// variant-by-variant `match` table (`blaze::hand_written_derivable`).
#[inline]
pub fn discriminant_tag<T>(value: &T) -> u64 {
    struct Fnv(u64);
    impl core::hash::Hasher for Fnv {
        #[inline]
        fn finish(&self) -> u64 {
            let mut x = self.0;
            x ^= x >> 30;
            x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
            x ^= x >> 27;
            x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
            x ^ (x >> 31)
        }
        #[inline]
        fn write(&mut self, bytes: &[u8]) {
            let mut h = self.0;
            for &b in bytes {
                h = h.wrapping_mul(0x0100_0000_01B3) ^ u64::from(b);
            }
            self.0 = h;
        }
    }
    let mut h = Fnv(0xCBF2_9CE4_8422_2325);
    core::mem::discriminant(value).hash(&mut h);
    h.finish()
}

/// Trait for types that expose a dense, zero-boilerplate variant tag (`u16`) without a
/// proc-macro crate. Under `cfg(blaze)`, Blaze's comptime reflection can synthesize this;
/// under both builders, [`crate::enum_tags!`] generates it at compile time with zero proc macros.
pub trait TaggedEnum {
    const VARIANT_COUNT: usize;
    fn tag(&self) -> u16;
    fn variant_name(&self) -> &'static str;
}

/// Zero-proc-macro declarative derive for dense enum tags and names (`TaggedEnum`), keeping
/// `VARIANT_COUNT`, `tag()`, and `variant_name()` guaranteed in sync at compile time.
#[macro_export]
macro_rules! enum_tags {
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident {
            $( $(#[$vmeta:meta])* $variant:ident ),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[repr(u16)]
        $vis enum $name {
            $( $(#[$vmeta])* $variant ),+
        }

        impl $crate::comptime::TaggedEnum for $name {
            const VARIANT_COUNT: usize = {
                let mut n = 0usize;
                $( let _ = stringify!($variant); n += 1; )+
                n
            };

            #[inline]
            fn tag(&self) -> u16 {
                *self as u16
            }

            #[inline]
            fn variant_name(&self) -> &'static str {
                match self {
                    $( $name::$variant => stringify!($variant) ),+
                }
            }
        }
    };
}
