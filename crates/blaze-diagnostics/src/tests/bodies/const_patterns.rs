//! rustc's UI tests of the errors of building patterns (`rustc_mir_build`'s `thir/pattern`: E0030,
//! E0579 and the constants `const_to_pat` refuses) and of the range lints of match checking
//! (`overlapping_range_endpoints`, `non_contiguous_range_endpoints`).

use crate::tests::{
    items::ui_core,
    lints::bodies::{reachable::PRINTLN, types::ui_std},
    ui,
};
use crate::tests::{assert_reports, error, warning};

#[test]
fn a_range_with_its_lower_bound_above_its_upper_is_e0030() {
    // tests/ui/error-codes/E0030.rs
    let source = r##"fn main() {
    match 5u32 {
        1000 ..= 5 => {}
        //~^ ERROR lower bound for range pattern must be less than or equal to upper bound
    }
}
"##;
    let stderr = r##"error[E0030]: lower bound for range pattern must be less than or equal to upper bound
  --> $DIR/E0030.rs:3:9
   |
LL |         1000 ..= 5 => {}
   |         ^^^^^^^^^^ lower bound larger than upper bound

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0030`.
"##;
    assert_eq!(ui("E0030.rs", "2015", source), stderr);
}

#[test]
fn a_union_constant_is_not_a_pattern() {
    // tests/ui/union/union-const-pat.rs
    let source = r##"union U {
    a: usize,
    b: usize,
}

const C: U = U { a: 10 };

fn main() {
    match C {
        C => {} //~ ERROR cannot use unions in constant patterns
        _ => {}
    }
}
"##;
    let stderr = r##"error: cannot use unions in constant patterns
  --> $DIR/union-const-pat.rs:10:9
   |
LL | const C: U = U { a: 10 };
   | ---------- constant defined here
...
LL |         C => {}
   |         ^ can't use a `union` here

error: aborting due to 1 previous error

"##;
    assert_eq!(ui("union-const-pat.rs", "2015", source), stderr);
}

#[test]
fn a_constant_of_a_type_without_partial_eq_is_not_a_pattern() {
    // tests/ui/consts/const_in_pattern/no-eq-branch-fail.rs
    let source = r##"struct NoEq;

enum Foo {
    Bar,
    Baz,
    Qux(NoEq),
}

// Even though any of these values can be compared structurally, we still disallow it in a pattern
// because `Foo` does not impl `PartialEq`.
const BAR_BAZ: Foo = if 42 == 42 {
    Foo::Baz
} else {
    Foo::Bar
};

fn main() {
    match Foo::Qux(NoEq) {
        BAR_BAZ => panic!(), //~ ERROR constant of non-structural type `Foo` in a pattern
        _ => {}
    }
}
"##;
    assert_reports(&ui_core("no-eq-branch-fail.rs", "2015", "panic", source), &[error(19, &["`Foo`"])]);
}

#[test]
fn each_kind_of_pattern_gets_its_equality_suggestion() {
    // tests/ui/consts/const_in_pattern/suggest_equality_comparison_instead_of_pattern_matching.rs, its
    // `Vec` lines left empty (the fixture has no alloc) and their four errors left out
    let source = r##"//@ edition:2024
// #42753
mod partial_eq {
    struct S;
    impl PartialEq<S> for S {
        fn eq(&self, _: &S) -> bool {
            true
        }
    }

    const C: S = S;


    fn foo() {
        match Some(S) {
            Some(C) => {} //~ ERROR: constant of non-structural type
            Some(C) if true => {} //~ ERROR: constant of non-structural type
            None => {}
        }
        if let Some(C) = Some(S) {} //~ ERROR: constant of non-structural type
        if let Some(C) = Some(S) && let Some(1) = Some(2) {} //~ ERROR: constant of non-structural type
        let Some(C) = Some(S) else { return; }; //~ ERROR: constant of non-structural type







    }
}

mod not_partial_eq {
    struct S;

    const C: S = S;

    fn foo() {
        match Some(S) {
            Some(C) => {} //~ ERROR: constant of non-structural type
            Some(C) if true => {} //~ ERROR: constant of non-structural type
            None => {}
        }
        if let Some(C) = Some(S) {} //~ ERROR: constant of non-structural type
        if let Some(C) = Some(S) && let Some(1) = Some(2) {} //~ ERROR: constant of non-structural type
        let Some(C) = Some(S) else { return; }; //~ ERROR: constant of non-structural type
    }
}

fn main() {}
"##;
    let stderr = r##"error: constant of non-structural type `partial_eq::S` in a pattern
  --> $DIR/suggest_equality_comparison_instead_of_pattern_matching.rs:16:18
   |
LL |     struct S;
   |     -------- `partial_eq::S` must be annotated with `#[derive(PartialEq)]` to be usable in patterns
...
LL |     const C: S = S;
   |     ---------- constant defined here
...
LL |             Some(C) => {}
   |                  ^ constant of non-structural type
   |
note: the `PartialEq` trait must be derived, manual `impl`s are not sufficient; see https://doc.rust-lang.org/stable/std/marker/trait.StructuralPartialEq.html for details
  --> $DIR/suggest_equality_comparison_instead_of_pattern_matching.rs:5:5
   |
LL |     impl PartialEq<S> for S {
   |     ^^^^^^^^^^^^^^^^^^^^^^^
help: add a condition to the match arm checking for equality
   |
LL -             Some(C) => {}
LL +             Some(binding) if binding == C => {}
   |

error: constant of non-structural type `partial_eq::S` in a pattern
  --> $DIR/suggest_equality_comparison_instead_of_pattern_matching.rs:17:18
   |
LL |     struct S;
   |     -------- `partial_eq::S` must be annotated with `#[derive(PartialEq)]` to be usable in patterns
...
LL |     const C: S = S;
   |     ---------- constant defined here
...
LL |             Some(C) if true => {}
   |                  ^ constant of non-structural type
   |
note: the `PartialEq` trait must be derived, manual `impl`s are not sufficient; see https://doc.rust-lang.org/stable/std/marker/trait.StructuralPartialEq.html for details
  --> $DIR/suggest_equality_comparison_instead_of_pattern_matching.rs:5:5
   |
LL |     impl PartialEq<S> for S {
   |     ^^^^^^^^^^^^^^^^^^^^^^^
help: add a check for equality to the condition of the match arm
   |
LL -             Some(C) if true => {}
LL +             Some(binding) if true && binding == C => {}
   |

error: constant of non-structural type `partial_eq::S` in a pattern
  --> $DIR/suggest_equality_comparison_instead_of_pattern_matching.rs:20:21
   |
LL |     struct S;
   |     -------- `partial_eq::S` must be annotated with `#[derive(PartialEq)]` to be usable in patterns
...
LL |     const C: S = S;
   |     ---------- constant defined here
...
LL |         if let Some(C) = Some(S) {}
   |                     ^ constant of non-structural type
   |
note: the `PartialEq` trait must be derived, manual `impl`s are not sufficient; see https://doc.rust-lang.org/stable/std/marker/trait.StructuralPartialEq.html for details
  --> $DIR/suggest_equality_comparison_instead_of_pattern_matching.rs:5:5
   |
LL |     impl PartialEq<S> for S {
   |     ^^^^^^^^^^^^^^^^^^^^^^^
help: check for equality instead of pattern matching
   |
LL -         if let Some(C) = Some(S) {}
LL +         if let Some(binding) = Some(S) && binding == C {}
   |

error: constant of non-structural type `partial_eq::S` in a pattern
  --> $DIR/suggest_equality_comparison_instead_of_pattern_matching.rs:21:21
   |
LL |     struct S;
   |     -------- `partial_eq::S` must be annotated with `#[derive(PartialEq)]` to be usable in patterns
...
LL |     const C: S = S;
   |     ---------- constant defined here
...
LL |         if let Some(C) = Some(S) && let Some(1) = Some(2) {}
   |                     ^ constant of non-structural type
   |
note: the `PartialEq` trait must be derived, manual `impl`s are not sufficient; see https://doc.rust-lang.org/stable/std/marker/trait.StructuralPartialEq.html for details
  --> $DIR/suggest_equality_comparison_instead_of_pattern_matching.rs:5:5
   |
LL |     impl PartialEq<S> for S {
   |     ^^^^^^^^^^^^^^^^^^^^^^^
help: check for equality instead of pattern matching
   |
LL -         if let Some(C) = Some(S) && let Some(1) = Some(2) {}
LL +         if let Some(binding) = Some(S) && binding == C && let Some(1) = Some(2) {}
   |

error: constant of non-structural type `partial_eq::S` in a pattern
  --> $DIR/suggest_equality_comparison_instead_of_pattern_matching.rs:22:18
   |
LL |     struct S;
   |     -------- `partial_eq::S` must be annotated with `#[derive(PartialEq)]` to be usable in patterns
...
LL |     const C: S = S;
   |     ---------- constant defined here
...
LL |         let Some(C) = Some(S) else { return; };
   |                  ^ constant of non-structural type
   |
note: the `PartialEq` trait must be derived, manual `impl`s are not sufficient; see https://doc.rust-lang.org/stable/std/marker/trait.StructuralPartialEq.html for details
  --> $DIR/suggest_equality_comparison_instead_of_pattern_matching.rs:5:5
   |
LL |     impl PartialEq<S> for S {
   |     ^^^^^^^^^^^^^^^^^^^^^^^
help: check for equality instead of pattern matching
   |
LL -         let Some(C) = Some(S) else { return; };
LL +         if Some(C) == Some(S) { return; };
   |

error: constant of non-structural type `not_partial_eq::S` in a pattern
  --> $DIR/suggest_equality_comparison_instead_of_pattern_matching.rs:40:18
   |
LL |     struct S;
   |     -------- `not_partial_eq::S` must be annotated with `#[derive(PartialEq)]` to be usable in patterns
LL |
LL |     const C: S = S;
   |     ---------- constant defined here
...
LL |             Some(C) => {}
   |                  ^ constant of non-structural type
   |
   = note: see https://doc.rust-lang.org/stable/std/marker/trait.StructuralPartialEq.html for details
help: add a condition to the match arm checking for equality
   |
LL -             Some(C) => {}
LL +             Some(binding) if binding == C => {}
   |

error: constant of non-structural type `not_partial_eq::S` in a pattern
  --> $DIR/suggest_equality_comparison_instead_of_pattern_matching.rs:41:18
   |
LL |     struct S;
   |     -------- `not_partial_eq::S` must be annotated with `#[derive(PartialEq)]` to be usable in patterns
LL |
LL |     const C: S = S;
   |     ---------- constant defined here
...
LL |             Some(C) if true => {}
   |                  ^ constant of non-structural type
   |
   = note: see https://doc.rust-lang.org/stable/std/marker/trait.StructuralPartialEq.html for details
help: add a check for equality to the condition of the match arm
   |
LL -             Some(C) if true => {}
LL +             Some(binding) if true && binding == C => {}
   |

error: constant of non-structural type `not_partial_eq::S` in a pattern
  --> $DIR/suggest_equality_comparison_instead_of_pattern_matching.rs:44:21
   |
LL |     struct S;
   |     -------- `not_partial_eq::S` must be annotated with `#[derive(PartialEq)]` to be usable in patterns
LL |
LL |     const C: S = S;
   |     ---------- constant defined here
...
LL |         if let Some(C) = Some(S) {}
   |                     ^ constant of non-structural type
   |
   = note: see https://doc.rust-lang.org/stable/std/marker/trait.StructuralPartialEq.html for details
help: check for equality instead of pattern matching
   |
LL -         if let Some(C) = Some(S) {}
LL +         if let Some(binding) = Some(S) && binding == C {}
   |

error: constant of non-structural type `not_partial_eq::S` in a pattern
  --> $DIR/suggest_equality_comparison_instead_of_pattern_matching.rs:45:21
   |
LL |     struct S;
   |     -------- `not_partial_eq::S` must be annotated with `#[derive(PartialEq)]` to be usable in patterns
LL |
LL |     const C: S = S;
   |     ---------- constant defined here
...
LL |         if let Some(C) = Some(S) && let Some(1) = Some(2) {}
   |                     ^ constant of non-structural type
   |
   = note: see https://doc.rust-lang.org/stable/std/marker/trait.StructuralPartialEq.html for details
help: check for equality instead of pattern matching
   |
LL -         if let Some(C) = Some(S) && let Some(1) = Some(2) {}
LL +         if let Some(binding) = Some(S) && binding == C && let Some(1) = Some(2) {}
   |

error: constant of non-structural type `not_partial_eq::S` in a pattern
  --> $DIR/suggest_equality_comparison_instead_of_pattern_matching.rs:46:18
   |
LL |     struct S;
   |     -------- `not_partial_eq::S` must be annotated with `#[derive(PartialEq)]` to be usable in patterns
LL |
LL |     const C: S = S;
   |     ---------- constant defined here
...
LL |         let Some(C) = Some(S) else { return; };
   |                  ^ constant of non-structural type
   |
   = note: see https://doc.rust-lang.org/stable/std/marker/trait.StructuralPartialEq.html for details
help: check for equality instead of pattern matching
   |
LL -         let Some(C) = Some(S) else { return; };
LL +         if Some(C) == Some(S) { return; };
   |

error: aborting due to 10 previous errors

"##;
    assert_eq!(ui_core("suggest_equality_comparison_instead_of_pattern_matching.rs", "2024", "option, eq", source), stderr);
}

#[test]
fn exclusive_ranges_one_apart_and_overlapping_endpoints_are_linted() {
    // tests/ui/pattern/usefulness/integer-ranges/gap_between_ranges.rs
    let source = r##"#![deny(non_contiguous_range_endpoints)]

macro_rules! m {
    ($s:expr, $t1:pat, $t2:pat) => {
        match $s {
            $t1 => {}
            $t2 => {}
            _ => {}
        }
    };
}

fn main() {
    match 0u8 {
        20..30 => {} //~ ERROR multiple ranges are one apart
        31..=40 => {}
        _ => {}
    }
    match 0u8 {
        20..30 => {} //~ ERROR multiple ranges are one apart
        31 => {}
        _ => {}
    }

    m!(0u8, 20..30, 31..=40); //~ ERROR multiple ranges are one apart
    m!(0u8, 31..=40, 20..30); //~ ERROR multiple ranges are one apart
    m!(0u8, 20..30, 29..=40); //~ WARN multiple patterns overlap on their endpoints
    m!(0u8, 20..30, 30..=40);
    m!(0u8, 20..30, 31..=40); //~ ERROR multiple ranges are one apart
    m!(0u8, 20..30, 32..=40);
    m!(0u8, 20..30, 31..=32); //~ ERROR multiple ranges are one apart
    // Don't lint two singletons.
    m!(0u8, 30, 32);
    // Don't lint on the equivalent inclusive range
    m!(0u8, 20..=29, 31..=40);
    // Don't lint if the lower one is a singleton.
    m!(0u8, 30, 32..=40);

    // Catch the `lo..MAX` case.
    match 0u8 {
        0..255 => {} //~ ERROR exclusive range missing `u8::MAX`
        _ => {}
    }
    // Except for `usize`, since `0..=usize::MAX` isn't exhaustive either.
    match 0usize {
        0..usize::MAX => {}
        _ => {}
    }

    // Don't lint if the gap is caught by another range.
    match 0u8 {
        0..10 => {}
        11..20 => {}
        10 => {}
        _ => {}
    }
    match 0u8 {
        0..10 => {}
        11..20 => {}
        5..15 => {}
        _ => {}
    }
    match 0u8 {
        0..255 => {}
        255 => {}
    }

    // Test multiple at once
    match 0u8 {
        0..10 => {} //~ ERROR multiple ranges are one apart
        11..20 => {}
        11..30 => {}
        _ => {}
    }
    match 0u8 {
        0..10 => {}  //~ ERROR multiple ranges are one apart
        11..20 => {} //~ ERROR multiple ranges are one apart
        21..30 => {}
        _ => {}
    }
    match 0u8 {
        00..20 => {} //~ ERROR multiple ranges are one apart
        10..20 => {} //~ ERROR multiple ranges are one apart
        21..30 => {}
        21..40 => {}
        _ => {}
    }

    // Test nested
    match (0u8, true) {
        (0..10, true) => {} //~ ERROR multiple ranges are one apart
        (11..20, true) => {}
        _ => {}
    }
    match (true, 0u8) {
        (true, 0..10) => {} //~ ERROR multiple ranges are one apart
        (true, 11..20) => {}
        _ => {}
    }
    // Asymmetry due to how exhaustiveness is computed.
    match (0u8, true) {
        (0..10, true) => {} //~ ERROR multiple ranges are one apart
        (11..20, false) => {}
        _ => {}
    }
    match (true, 0u8) {
        (true, 0..10) => {}
        (false, 11..20) => {}
        _ => {}
    }
    match Some(0u8) {
        Some(0..10) => {} //~ ERROR multiple ranges are one apart
        Some(11..20) => {}
        _ => {}
    }
}
"##;
    assert_reports(
        &ui_core("gap_between_ranges.rs", "2015", "option, int_bounds", source),
        &[
            error(15, &[]),
            error(20, &[]),
            error(25, &[]),
            error(26, &[]),
            warning(27, &[]),
            error(29, &[]),
            error(31, &[]),
            error(41, &["`u8::MAX`"]),
            error(70, &[]),
            error(76, &[]),
            error(77, &[]),
            error(82, &[]),
            error(83, &[]),
            error(91, &[]),
            error(96, &[]),
            error(102, &[]),
            error(112, &[]),
        ],
    );
}

#[test]
fn a_derive_whose_structural_impl_does_not_apply_is_not_structural() {
    // tests/ui/consts/const_in_pattern/derive-and-manual-partialeq-issue-147714.rs
    let source = r##"// `derive(PartialEq)`, which also implements `StructuralPartialEq`, must not allow the latter impl
// to be used with other non-derived implementations of `PartialEq`.
//
// Test case by theemathas from <https://github.com/rust-lang/rust/issues/147714>.

#[allow(dead_code)]
#[derive(PartialEq)]
enum Thing<T> {
    A(T),
    B,
}

struct Incomparable;

// This impl does not obey StructuralPartialEq's rules.
impl PartialEq for Thing<Incomparable> {
    fn eq(&self, _: &Self) -> bool {
        panic!()
    }
}

// This constant does not obey StructuralPartialEq's rules, so it should not
// implement StructuralPartialEq.
const X: Thing<Incomparable> = Thing::B;

fn main() {
    if let X = X {
        //~^ ERROR constant of non-structural type `Thing<Incomparable>` in a pattern
        println!("equal");
    }
}
"##;
    let name = "derive-and-manual-partialeq-issue-147714.rs";
    assert_reports(
        &ui_std(name, "2015", "sized, fmt, derive, eq, panic", PRINTLN, source),
        &[
            error(27, &["`Thing<Incomparable>`"]),
        ],
    );
}

#[test]
fn a_type_without_partial_eq_inside_the_constant_is_labelled() {
    // tests/ui/consts/const_in_pattern/reject_non_partial_eq.rs
    let source = r##"// This test is illustrating the difference between how failing to derive
// `PartialEq` is handled compared to failing to implement it at all.

// See also RFC 1445

#[derive(PartialEq, Eq)]
struct Structural(u32);

struct NoPartialEq(u32);

struct NoDerive(u32);

// This impl makes NoDerive irreflexive.
impl PartialEq for NoDerive { fn eq(&self, _: &Self) -> bool { false } }

impl Eq for NoDerive { }

const NO_DERIVE_NONE: Option<NoDerive> = None;
const NO_PARTIAL_EQ_NONE: Option<NoPartialEq> = None;

fn main() {
    match None {
        NO_DERIVE_NONE => println!("NO_DERIVE_NONE"),
        _ => panic!("whoops"),
    }

    match None {
        NO_PARTIAL_EQ_NONE => println!("NO_PARTIAL_EQ_NONE"),
        //~^ ERROR constant of non-structural type `Option<NoPartialEq>` in a pattern
        _ => panic!("whoops"),
    }
}
"##;
    let stderr = r##"error: constant of non-structural type `Option<NoPartialEq>` in a pattern
  --> $DIR/reject_non_partial_eq.rs:28:9
   |
LL | struct NoPartialEq(u32);
   | ------------------ must be annotated with `#[derive(PartialEq)]` to be usable in patterns
...
LL | const NO_PARTIAL_EQ_NONE: Option<NoPartialEq> = None;
   | --------------------------------------------- constant defined here
...
LL |         NO_PARTIAL_EQ_NONE => println!("NO_PARTIAL_EQ_NONE"),
   |         ^^^^^^^^^^^^^^^^^^ constant of non-structural type
   |
   = note: see https://doc.rust-lang.org/stable/std/marker/trait.StructuralPartialEq.html for details

error: aborting due to 1 previous error

"##;
    let name = "reject_non_partial_eq.rs";
    assert_eq!(ui_std(name, "2015", "sized, fmt, derive, eq, option, panic", PRINTLN, source), stderr);
}
