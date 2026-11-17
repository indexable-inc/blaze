//! blaze: where a process gets its randomness and its wall clock
//! (docs/v2/plan/design/determinism.md, section A).
//!
//! A process has one provider, chosen once as it starts:
//!
//! - the system's (the default): everything in std is as the nightly's, and a program that
//!   knows nothing of blaze never sees a difference;
//! - a given one: whoever started the process put `__BLAZE_PROVIDER=1:<seed>:<wall>` in its
//!   environment (blaze does, for the build scripts and tests it runs in its sandbox). Then
//!   the keys of `RandomState`, `fill_bytes` and, when `<wall>` is a time, `SystemTime::now`
//!   are functions of the seed and of nothing else ([`derive`]), with no system call.
//!
//! The variable is read from `environ` itself, never through `getenv`, so blaze's interposer
//! does not take it for a variable the program read; and it is read before `main` (`rt::init`),
//! so nothing the program does to its environment changes its provider. A program whose
//! `main` is not Rust's reads it at its first use instead.
//!
//! A child started through `Command` is given a seed of its own, derived from this process's
//! and the number of the spawn ([`spawning`]), so two children never share one.
//!
//! Targets with no `environ` this reads (all but Linux, Android and Apple's) always have the
//! system's provider.

#[allow(dead_code, reason = "blaze's side uses the encoder and the streams std does not")]
pub(crate) mod derive;

cfg_select! {
    any(target_os = "linux", target_os = "android", target_vendor = "apple") => {
        mod given;
        pub use given::{fill, hashmap_keys, init, spawning, wall};
    }
    _ => {
        /// Nothing to read: the provider is the system's.
        pub fn init() {}

        /// The system's provider fills nothing.
        #[inline]
        pub fn fill(_bytes: &mut [u8]) -> bool {
            false
        }

        /// The system's provider has no keys of its own.
        #[inline]
        pub fn hashmap_keys() -> Option<(u64, u64)> {
            None
        }

        /// The system's provider has no time of its own.
        #[inline]
        pub fn wall() -> Option<i64> {
            None
        }

        /// The system's provider gives a child nothing.
        #[inline]
        pub fn spawning() -> Option<()> {
            None
        }
    }
}
