//! `Default` for arrays as core writes it, BUILT AND RUN on the new front end: core's
//! library/core/src/array/mod.rs `array_impl_default!` writes one impl per length 0..=32, the
//! length of each a nested constant expression (`32`, `(32 - 1)`, `((32 - 1) - 1)` ..) made by
//! the macro's recursion: `impl<T> Default for [T; $n] where T: Default { fn default() -> [T;
//! $n] { [$t::default(), $($ts::default()),*] } }` and, at the end, `impl<T> Default for [T;
//! $n] { fn default() -> [T; $n] { [] } }`. tempo t0850: 5 tests NOT RUN at rand_core's
//! `SeedableRng::seed_from_u64` (`let mut seed = Self::Seed::default();` with `type Seed = [u8;
//! 32]` for ChaCha20Rng): "resolving an instance: the world has no answer to
//! ImplsOf(Default, Array)".
//!
//! The rules (the Reference, "Implementations", "Trait implementations"; "Array types": "The
//! size is a constant expression that evaluates to a usize"): `[u8; 4]: Default` holds by the
//! impl whose length evaluates to 4 (`(((..(32 - 1)..) - 1)` 28 times); a generic `T::default()`
//! at `T = [D; 2]` calls that impl's `default` at `T = D`; a projection `Self::Seed` that
//! normalizes to `[u8; 32]` calls the impl of length 32. The twin: no impl has length 33, so
//! `[u8; 33]: Default` is E0277 (rustc: "the trait bound `[u8; 33]: Default` is not
//! satisfied"), a decided refusal, never a run.

use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{FrontChoice, NewFront, run_with};

const NEW: NewFront<'static> = NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::On };

const PRELUDE: &str = include_str!("../../blaze-cli/tests/ladder/front2/x0-async-01/src/prelude.rs");

const HEAD: &str = "#![feature(no_core, lang_items, intrinsics, rustc_attrs)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod prelude;
use prelude::*;

trait Default: Sized {
    fn default() -> Self;
}

impl Default for u8 {
    fn default() -> u8 {
        0
    }
}

struct D {
    v: u8,
}

impl Default for D {
    fn default() -> D {
        D { v: 7 }
    }
}

// core/src/array/mod.rs, as written.
macro_rules! array_impl_default {
    {$n:expr, $t:ident $($ts:ident)*} => {
        impl<T> Default for [T; $n] where T: Default {
            fn default() -> [T; $n] {
                [$t::default(), $($ts::default()),*]
            }
        }
        array_impl_default!{($n - 1), $($ts)*}
    };
    {$n:expr,} => {
        impl<T> Default for [T; $n] {
            fn default() -> [T; $n] { [] }
        }
    };
}

array_impl_default! {32, T T T T T T T T T T T T T T T T T T T T T T T T T T T T T T T T}

fn make<T: Default>() -> T {
    T::default()
}

/// rand_core's `SeedableRng` shape: `Self::Seed::default()` in a provided method.
trait Seedable: Sized {
    type Seed: Default;
    fn from_seed(seed: Self::Seed) -> Self;
    fn seed_from_zero() -> Self {
        let seed = Self::Seed::default();
        Self::from_seed(seed)
    }
}

struct Rng {
    first: u8,
}

impl Seedable for Rng {
    type Seed = [u8; 32];
    fn from_seed(seed: [u8; 32]) -> Rng {
        Rng { first: seed[0] + seed[31] + 5 }
    }
}
";

/// Builds and runs `HEAD` + `rest`: the status, or the refusal on one line.
fn run(rest: &str) -> Result<u8, String> {
    let text = [HEAD, rest].concat();
    let files = [SourceFile { path: "src/main.rs", text: &text }, SourceFile { path: "src/prelude.rs", text: PRELUDE }];
    run_with(&files, "src/main.rs", None, FrontChoice::New(NEW), blaze_own::testing::heap().shared()).map_err(|e| format!("{e}").replace('\n', " "))
}

/// `<[u8; 4] as Default>::default()` (zeros), `make::<[D; 2]>()` (two `D { v: 7 }`) and
/// `Rng::seed_from_zero()` (`[u8; 32]` zeros: `first` = 0 + 0 + 5): 0 + 0 + 7 + 7 + 5 + 30 = 49.
#[test]
fn array_default_runs_by_its_length_impl() {
    let got = run("
#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let a: [u8; 4] = <[u8; 4] as Default>::default();
    let b: [D; 2] = make::<[D; 2]>();
    let r = Rng::seed_from_zero();
    (a[0] + a[3] + b[0].v + b[1].v + r.first + 30) as i32
}
");
    println!("ARRAY DEFAULT: {got:?}");
    assert_eq!(got, Ok(49), "the length-4, length-2 (at T = D) and length-32 impls of core's macro");
}

/// The twin: `make::<[u8; 33]>()`: no impl of length 33 (rustc E0277); refused, and by a
/// decided answer of the bucket, not by a question the world could not answer.
#[test]
fn twin_no_impl_above_32_is_refused() {
    let got = run("
#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let a: [u8; 33] = make::<[u8; 33]>();
    a[32] as i32
}
");
    println!("ARRAY DEFAULT 33: {got:?}");
    let Err(why) = got else { panic!("`[u8; 33]: Default` holds by no impl (E0277): never run, got {got:?}") };
    assert!(!why.contains("no answer to"), "the bucket answers (the 33 impls, none of length 33): a decided E0277, not a gap: {why}");
}
