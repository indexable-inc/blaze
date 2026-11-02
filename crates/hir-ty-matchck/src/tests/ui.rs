//! More of rustc's UI tests, each cut down to what the fixture's `minicore` has, with what its
//! `.stderr` gives. Lints a UI test allows are still found here (compiletest passes `-A unused`,
//! which holds `unreachable_patterns`): match checking reports every lint, the lint levels are
//! applied after it.

use super::check;

// tests/ui/resolve/const-with-typo-in-pattern-binding.rs, with `std::env::consts::ARCH`
// replaced by a constant of the crate re-exported the same way.
#[test]
fn const_with_typo_in_pattern_binding() {
    check(
        r#"
mod y {
    pub const ARCH: &str = "a";
}
mod x {
    pub use crate::y::ARCH;
    const X: i32 = 0;
}
fn main() {
    let input: i32 = 42;

    const god: i32 = 1;
    const GOOD: i32 = 1;
    const BAD: i32 = 2;

    let name: i32 = 42;

    match input {
        X => {}
        _ => {}
    }
    match input {
        GOD => {}
        _ => {}
    }
    match input {
        GOOOD => {}
        _ => {}
    }
    match input {
        name => {}
        _ => {}
    }
    match "" {
        ARCH => {}
        _ => {}
    }
}
"#,
        &[
            ("_", "unreachable_patterns: catchall, inaccessible"),
            ("_", "unreachable_patterns: catchall, wanted god typo=true"),
            ("_", "unreachable_patterns: catchall, wanted GOOD typo=true"),
            ("_", "unreachable_patterns: catchall, let binding"),
            ("_", "unreachable_patterns: catchall, wanted y::ARCH typo=false"),
        ],
    );
}

// tests/ui/rfcs/rfc-2294-if-let-guard/exhaustive.rs (which allows `irrefutable_let_patterns`),
// and the plainest E0004 of an integer.
#[test]
fn guards_do_not_count_towards_exhaustivity() {
    check(
        r#"
//- minicore: option
fn match_option(x: Option<u32>) {
    match x {
        Some(_) => {}
        None if let y = x => {}
    }
}

fn main() {
    let x = ();
    match x {
        y if let z = y => {}
    }
}

fn integer() {
    match 0u8 {
        0..=100 => {}
    }
}
"#,
        &[
            ("let y = x", "irrefutable_let_patterns: IfLetGuard"),
            ("x", "E0004: None"),
            ("let z = y", "irrefutable_let_patterns: IfLetGuard"),
            ("x", "E0004: ()"),
            ("0u8", "E0004: 101_u8..=u8::MAX"),
        ],
    );
}

// tests/ui/pattern/usefulness/integer-ranges/overlapping_range_endpoints.rs: a range overlapping
// two others on its two ends gets a lint for each; a singleton or a range only touching another
// gets none.
#[test]
fn overlapping_range_endpoints() {
    check(
        r#"
//- minicore: option
fn main() {
    match 0u8 { 20..=30 => {} 31..=40 => {} _ => {} }
    match 0u8 { 20..30 => {} 30..=40 => {} _ => {} }
    match 0u8 { 20..=30 => {} 30..=30 => {} _ => {} }
    match 0u8 { 20..=30 => {} 20..=21 => {} _ => {} }
    match 0u8 { 20..=30 => {} 19..=20 => {} _ => {} }
    match 0u8 {
        0..=10 => {}
        20..=30 => {}
        10..=20 => {}
        _ => {}
    }
    match (0u8, true) {
        (0..=10, true) => {}
        (10..20, true) => {}
        (10..20, false) => {}
        _ => {}
    }
    match Some(0u8) {
        Some(0..=10) => {}
        Some(10..20) => {}
        _ => {}
    }
}
"#,
        // rustc's `.stderr` has no `unreachable_patterns`: compiletest runs ui tests with
        // `-A unused`, and the lint is in that group.
        &[
            ("30..=30", "unreachable_patterns"),
            ("20..=21", "unreachable_patterns"),
            ("19..=20", "overlapping_range_endpoints: 20_u8"),
            ("10..=20", "overlapping_range_endpoints: 10_u8"),
            ("10..=20", "overlapping_range_endpoints: 20_u8"),
            ("10..20", "overlapping_range_endpoints: 10_u8"),
            ("10..20", "overlapping_range_endpoints: 10_u8"),
        ],
    );
}

// tests/ui/pattern/usefulness/integer-ranges/gap_between_ranges.rs: `lo..hi` where `hi` is
// matched by nothing but `hi + 1` is, the other pattern before or after it.
#[test]
fn gap_between_ranges() {
    check(
        r#"
fn main() {
    match 0u8 {
        20..30 => {}
        31..=40 => {}
        _ => {}
    }
    match 0u8 {
        20..30 => {}
        31 => {}
        _ => {}
    }
    match 0u8 { 31..=40 => {} 20..30 => {} _ => {} }
    match 0u8 { 20..30 => {} 29..=40 => {} _ => {} }
    match 0u8 { 20..30 => {} 30..=40 => {} _ => {} }
    match 0u8 { 20..30 => {} 32..=40 => {} _ => {} }
}
"#,
        &[
            ("20..30", "non_contiguous_range_endpoints: 30_u8 (20_u8..=30_u8, 1 after)"),
            ("20..30", "non_contiguous_range_endpoints: 30_u8 (20_u8..=30_u8, 1 after)"),
            ("20..30", "non_contiguous_range_endpoints: 30_u8 (20_u8..=30_u8, 1 after)"),
            ("29..=40", "overlapping_range_endpoints: 29_u8"),
        ],
    );
}

// tests/ui/pattern/usefulness/refutable-pattern-errors.rs: an `isize` has no fixed bounds, an
// `i32` does.
#[test]
fn refutable_pattern_errors() {
    check(
        r#"
//- minicore: option
fn func((1, (Some(1), 2..=3)): (isize, (Option<isize>, isize))) {}

fn main() {
    let (1, (Some(1), 2..=3)) = (1, (None, 2));
}
"#,
        &[
            (
                "(1, (Some(1), 2..=3))",
                "E0005 function argument: (..=0_isize, _) | (2_isize.., _)",
            ),
            (
                "(1, (Some(1), 2..=3))",
                "E0005 local binding: (i32::MIN..=0_i32, _) | (2_i32..=i32::MAX, _)",
            ),
        ],
    );
}

// tests/ui/lint/lint-match-arms-2.rs: a one-variant enum's binding gets the suggestion in an
// irrefutable match too.
#[test]
fn lint_match_arms_2() {
    check(
        r#"
enum E {
    A,
}

fn foo() {
    match E::A {
        A => {}
    }

    match &E::A {
        a if let b = a => {}
        _ => {}
    }
}
"#,
        &[
            ("A", "bindings_with_variant_name: suggest"),
            ("let b = a", "irrefutable_let_patterns: IfLetGuard"),
        ],
    );
}

// tests/ui/pattern/issue-67776-match-same-name-enum-variant-refs.rs: the binding's type is a
// reference to the enum.
#[test]
fn binding_named_like_a_variant_through_references() {
    check(
        r#"
enum Foo {
    Bar,
    Baz,
}

fn fn2(e: &Foo) {
    match e {
        Bar => {},
        Baz => {},
    }
}

fn fn3(e: &mut &&mut Foo) {
    match e {
        Bar => {},
        Baz => {},
    }
}
"#,
        &[
            ("Bar", "bindings_with_variant_name: suggest"),
            ("Baz", "bindings_with_variant_name: suggest"),
            ("Baz", "unreachable_patterns: catchall"),
            ("Bar", "bindings_with_variant_name: suggest"),
            ("Baz", "bindings_with_variant_name: suggest"),
            ("Baz", "unreachable_patterns: catchall"),
        ],
    );
}
