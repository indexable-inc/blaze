//! `min_specialization` as alloc uses it, BUILT AND RUN on the new front end: the shape of
//! `SpecFromIterNested::from_iter` calling `vector.spec_extend(iterator)` under
//! `I: TrustedLen<Item = T>` (library/alloc/src/vec/spec_from_iter_nested.rs), with the two
//! `SpecExtend` impls of library/alloc/src/vec/spec_extend.rs: `impl<T, I, A: Allocator>
//! SpecExtend<T, I> for Vec<T, A> where I: Iterator<Item = T>` (`default fn spec_extend`) and
//! the one `where I: TrustedLen<Item = T>` that specializes it. tempo t0850 and rt-loop a0855:
//! 3 tests NOT RUN, `Vec<Param(0)>: SpecExtend<{unknown}, Param(1)>` (E0283) "2 candidates may
//! each hold and none is chosen".
//!
//! The rules:
//! - RFC 1210 ("Specialization"): impl A specializes impl B when A applies to a strict subset
//!   of what B applies to; "the most specific impl that applies is used"; an item B marks
//!   `default` may be overridden by A.
//! - In a generic body, the pinned rustc_next_trait_solver's `filter_specialized_impls`
//!   (solve/assembly/mod.rs) drops the specialized impl candidate when the specializing one
//!   "has the same response" ("dropping victim doesn't change the result. We need to do so
//!   here as we don't merge distinct impl candidates"); the old solver the pinned nightly's
//!   typeck runs (`NextSolverConfig::Coherence` by default) drops it when the specializing
//!   candidate must apply (select/mod.rs `prefer_lhs_over_victim`). Both answer
//!   `Bag<T>: SpecExtend<?X, I>` with `?X := T`, by the TrustedLen impl.
//! - At an instance (ground arguments), the most specific impl that holds is the one called
//!   (blaze-typeck specialize.rs `most_specific`): `via_plain` at a TrustedLen iterator calls
//!   the specializing impl's `spec_extend`, at a plain one the general impl's.
//!
//! The twin: two overlapping impls neither of which specializes the other are E0119 at the
//! impls (rustc refuses the program); blaze has no coherence check of them, so the call
//! that both answer must stay undecided and the program is refused, never run through
//! either impl.

use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{FrontChoice, NewFront, run_with};

const NEW: NewFront<'static> = NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::On };

const PRELUDE: &str = include_str!("../../blaze-cli/tests/ladder/front2/x0-async-01/src/prelude.rs");

const HEAD: &str = "#![feature(no_core, lang_items, intrinsics, rustc_attrs, min_specialization)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod prelude;
use prelude::*;

trait Iter {
    type Item;
    fn next(&mut self) -> Option<Self::Item>;
}

#[rustc_specialization_trait]
unsafe trait TrustedLen: Iter {}

/// A TrustedLen iterator.
struct Counted {
    left: u32,
}

impl Iter for Counted {
    type Item = u32;
    fn next(&mut self) -> Option<u32> {
        if self.left == 0 {
            None
        } else {
            self.left = self.left - 1;
            Some(self.left)
        }
    }
}

unsafe impl TrustedLen for Counted {}

/// A plain iterator.
struct Plain {
    left: u32,
}

impl Iter for Plain {
    type Item = u32;
    fn next(&mut self) -> Option<u32> {
        if self.left == 0 {
            None
        } else {
            self.left = self.left - 1;
            Some(self.left)
        }
    }
}

/// `Vec<T>`'s stand-in.
struct Bag<T> {
    first: Option<T>,
}
";

/// Builds and runs `HEAD` + `rest`: the status, or the refusal on one line.
fn run(rest: &str) -> Result<u8, String> {
    let text = [HEAD, rest].concat();
    let files = [SourceFile { path: "src/main.rs", text: &text }, SourceFile { path: "src/prelude.rs", text: PRELUDE }];
    run_with(&files, "src/main.rs", None, FrontChoice::New(NEW), blaze_own::testing::heap().shared()).map_err(|e| format!("{e}").replace('\n', " "))
}

/// The fix and the run: `via_trusted`'s body is alloc's shape (the method call
/// `bag.spec_extend(iter)` under `I: TrustedLen<Item = T>`, both impls answering `?X := T`):
/// it checks clean. At the instances: `via_trusted(Counted)` and `via_plain(Counted)` call the
/// TrustedLen impl (2), `via_plain(Plain)` the general one (1): 2 * 100 + 2 * 10 + 1 = 221.
/// Before the fix the run is refused with the E0283 of `via_trusted`'s body.
#[test]
fn the_specializing_impl_serves_a_trusted_len_iterator() {
    let got = run("
trait SpecExtend<T, I> {
    fn spec_extend(&mut self, iter: I) -> u32;
}

impl<T, I> SpecExtend<T, I> for Bag<T>
where
    I: Iter<Item = T>,
{
    default fn spec_extend(&mut self, iter: I) -> u32 {
        let mut it = iter;
        self.first = it.next();
        1
    }
}

impl<T, I> SpecExtend<T, I> for Bag<T>
where
    I: TrustedLen<Item = T>,
{
    fn spec_extend(&mut self, iter: I) -> u32 {
        let mut it = iter;
        self.first = it.next();
        2
    }
}

fn via_trusted<T, I: TrustedLen<Item = T>>(bag: &mut Bag<T>, iter: I) -> u32 {
    bag.spec_extend(iter)
}

fn via_plain<T, I: Iter<Item = T>>(bag: &mut Bag<T>, iter: I) -> u32 {
    bag.spec_extend(iter)
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let mut bag: Bag<u32> = Bag { first: None };
    let a = via_trusted(&mut bag, Counted { left: 3 });
    let b = via_plain(&mut bag, Counted { left: 3 });
    let c = via_plain(&mut bag, Plain { left: 3 });
    (a * 100 + b * 10 + c) as i32
}
");
    println!("SPECIALIZATION RUN: {got:?}");
    assert_eq!(got, Ok(221), "TrustedLen impl at a TrustedLen iterator (generic and ground callers), the general impl at a plain one");
}

/// The twin (and the control: the same call with the specialization relation removed): two
/// overlapping impls, `I: A<Item = T>` and `I: B<Item = T>`, neither trait implying the
/// other, so neither impl specializes the other (rustc: E0119 at the impls). The call under
/// `I: A<Item = T> + B<Item = T>` is answered `?X := T` by both: it must stay undecided, and
/// the program is refused rather than run through either impl.
#[test]
fn twin_two_unrelated_overlapping_impls_are_refused() {
    let got = run("
#[rustc_specialization_trait]
trait A: Iter {}
#[rustc_specialization_trait]
trait B: Iter {}
impl A for Counted {}
impl B for Counted {}

trait Pick<T, I> {
    fn pick(&mut self, iter: I) -> u32;
}

impl<T, I> Pick<T, I> for Bag<T>
where
    I: A<Item = T>,
{
    default fn pick(&mut self, iter: I) -> u32 {
        let mut it = iter;
        self.first = it.next();
        1
    }
}

impl<T, I> Pick<T, I> for Bag<T>
where
    I: B<Item = T>,
{
    default fn pick(&mut self, iter: I) -> u32 {
        let mut it = iter;
        self.first = it.next();
        2
    }
}

fn via_both<T, I: A<Item = T> + B<Item = T>>(bag: &mut Bag<T>, iter: I) -> u32 {
    bag.pick(iter)
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let mut bag: Bag<u32> = Bag { first: None };
    via_both(&mut bag, Counted { left: 3 }) as i32
}
");
    println!("OVERLAP TWIN: {got:?}");
    assert!(got.is_err(), "two impls neither more specific (E0119): never run through one of them, got {got:?}");
}
