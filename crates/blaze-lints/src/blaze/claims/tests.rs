//! The claim lints over the effect lints' toolchain (`effects_tests`), the crate built with
//! overflow checks and `cfg(test)`: where each fires, where it does not, and the fixes made and
//! linted again clean.

use super::super::effects_tests::{MINICORE, TOOLCHAIN};
use crate::tests::{lint, offered_in};

/// The six on, each denied, so a finding of any is in the output.
const FLAGS: &str = "-A clippy::all --deny=blaze::claim_contradicts_facts --deny=blaze::claim_is_derivable \
     --deny=blaze::claim_without_evidence --deny=blaze::evidence_is_stale \
     --deny=blaze::safety_without_invariant --deny=blaze::comment_claims";

fn header() -> String {
    format!("{MINICORE}{TOOLCHAIN}//- /src/lib.rs crate:main deps:alloc,std cfg:test,overflow_checks\n")
}

/// What the six say of `main`.
fn out(main: &str) -> String {
    lint(&format!("{}{}", header(), main.trim_start_matches('\n')), FLAGS)
}

/// The lines of `out` that hold `marker`.
fn said<'o>(out: &'o str, marker: &str) -> Vec<&'o str> {
    out.lines().filter(|line| line.contains(marker)).collect()
}

const NONE: [&str; 0] = [];

// claim_contradicts_facts

/// `Never panics.` on a function whose index keeps its bounds check is refuted, with the walk's
/// own words for why.
#[test]
fn never_panics_on_a_function_that_indexes_contradicts_the_facts() {
    let out = out(
        r#"
/// The first.
///
/// Never panics.
pub fn first(v: &[u32]) -> u32 {
    v[0]
}
"#,
    );
    let found = said(&out, "says it never panics, but");
    assert_eq!(found.len(), 1, "{out}");
    assert!(found[0].contains("`first`"), "{out}");
}

/// A `# Panics` section on a function proved never to panic says what is not so.
#[test]
fn a_panics_section_on_a_function_that_cannot_panic_contradicts_the_facts() {
    let out = out(
        r#"
/// The third and first, mixed.
///
/// # Panics
///
/// When they are equal.
pub fn third(a: &[u32; 4]) -> u32 {
    a[2] ^ a[0]
}
"#,
    );
    assert_eq!(said(&out, "has a `# Panics` section").len(), 1, "{out}");
}

/// A qualified sentence is no claim of the item: nothing is said of it.
#[test]
fn a_qualified_sentence_is_not_a_claim() {
    let out = out(
        r#"
/// The first. A warm read allocates nothing, and it never panics once the length is checked.
pub fn first(v: &[u32]) -> u32 {
    v[0]
}
"#,
    );
    assert_eq!(said(&out, "the doc of"), NONE, "{out}");
}

// claim_is_derivable

/// The same doc on a function the walk proves: the attribute is written and the sentence goes,
/// and the file lints clean after.
#[test]
fn never_panics_on_a_proved_function_is_derivable_and_fixed() {
    const MAIN: &str = r#"/// Never panics.
/// The third and first, mixed.
pub fn third(a: &[u32; 4]) -> u32 {
    a[2] ^ a[0]
}
"#;
    let out = out(MAIN);
    assert_eq!(said(&out, "the effect walk proves it").len(), 1, "{out}");
    let after = offered_in(&header(), MAIN, FLAGS);
    assert_eq!(
        after,
        "/// The third and first, mixed.\n#[blaze::nopanic]\npub fn third(a: &[u32; 4]) -> u32 {\n    a[2] ^ a[0]\n}\n"
    );
}

/// A marked function's sentence repeats its attribute.
#[test]
fn a_sentence_that_repeats_the_attribute_is_derivable() {
    let out = out(
        r#"
/// The third. Never panics.
#[blaze::nopanic]
pub fn third(a: &[u32; 4]) -> u32 {
    a[2]
}
"#,
    );
    assert_eq!(said(&out, "already holds").len(), 1, "{out}");
}

// claim_without_evidence and evidence_is_stale

/// A `# Guarantees` item whose test resolves, is a test and calls the item: held, level
/// `tested` in the ledger. One naming a Lean theorem: held, level `proved` (the ledger checks
/// the name). One `Unverified:` with a reason: declared, counted by the ledger.
#[test]
fn guarantees_held_by_a_test_a_theorem_or_a_reason_are_quiet() {
    let out = out(
        r#"
/// Mixes the pair.
///
/// # Guarantees
///
/// - The mix is the same in either order. Evidence: [`tests::mixes_either_way`]
/// - The model's mix is deterministic. Evidence: Lean `Blaze.Mix.mix_deterministic`
/// - A mix takes one instruction. Unverified: no test counts the instructions yet.
pub fn mix(a: u32, b: u32) -> u32 {
    a ^ b
}

#[cfg(test)]
mod tests {
    #[test]
    fn mixes_either_way() {
        let _ = super::mix(1, 2);
    }
}
"#,
    );
    assert_eq!(said(&out, "this claim of"), NONE, "{out}");
    assert_eq!(said(&out, "the evidence"), NONE, "{out}");
}

/// An item with no marker, an empty `Unverified:`, and one spelled another way.
#[test]
fn a_claim_held_by_nothing_is_without_evidence() {
    let out = out(
        r#"
/// Mixes the pair.
///
/// # Guarantees
///
/// - The mix is the same in either order.
/// - A mix takes one instruction. Unverified:
pub fn mix(a: u32, b: u32) -> u32 {
    a ^ b
}
"#,
    );
    assert_eq!(said(&out, "says nothing holds it").len(), 1, "{out}");
    assert_eq!(said(&out, "gives no reason").len(), 1, "{out}");
}

/// `Tested by` is rewritten to the one marker, and the file lints clean after.
#[test]
fn a_misspelled_marker_is_fixed_to_the_one_marker() {
    const MAIN: &str = r#"/// Mixes the pair.
///
/// # Guarantees
///
/// - The mix is the same in either order. Tested by [`tests::mixes_either_way`]
pub fn mix(a: u32, b: u32) -> u32 {
    a ^ b
}

#[cfg(test)]
mod tests {
    #[test]
    fn mixes_either_way() {
        let _ = super::mix(1, 2);
    }
}
"#;
    let out = out(MAIN);
    assert_eq!(said(&out, "where the one marker is `Evidence:`").len(), 1, "{out}");
    let after = offered_in(&header(), MAIN, FLAGS);
    assert!(after.contains("either order. Evidence: [`tests::mixes_either_way`]"), "{after}");
}

/// A pointer that resolves to nothing, one to a test that never calls the item, and one to
/// what is not a test.
#[test]
fn stale_evidence_is_named() {
    let out = out(
        r#"
/// Mixes the pair.
///
/// # Guarantees
///
/// - Symmetric. Evidence: [`tests::gone`]
/// - Cheap. Evidence: [`tests::other_thing`]
/// - Total. Evidence: [`helper`]
pub fn mix(a: u32, b: u32) -> u32 {
    a ^ b
}

pub fn helper() -> u32 {
    1
}

#[cfg(test)]
mod tests {
    #[test]
    fn other_thing() {
        let _ = 1u32;
    }
}
"#,
    );
    assert_eq!(said(&out, "`tests::gone` of this claim of `mix` resolves to nothing").len(), 1, "{out}");
    assert_eq!(said(&out, "`tests::other_thing` of this claim of `mix` does not reach `mix`").len(), 1, "{out}");
    assert_eq!(said(&out, "`helper` of this claim of `mix` is not a test").len(), 1, "{out}");
}

/// A type's `# Invariants`, evidenced by a test that makes a value of it.
#[test]
fn a_type_invariant_is_reached_by_a_test_that_makes_one() {
    let out = out(
        r#"
/// A ring.
///
/// # Invariants
///
/// - `cap` is a power of two. Evidence: [`tests::a_new_ring_is_a_power_of_two`]
/// - `len` is at most `cap`.
pub struct Ring {
    cap: usize,
    len: usize,
}

impl Ring {
    pub fn new() -> Ring {
        Ring { cap: 8, len: 0 }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_new_ring_is_a_power_of_two() {
        let _ = super::Ring::new();
    }
}
"#,
    );
    assert_eq!(said(&out, "the evidence"), NONE, "{out}");
    assert_eq!(said(&out, "says nothing holds it").len(), 1, "{out}");
}

// safety_without_invariant

/// Each form: named and placed (quiet), placed by the caller in an `unsafe fn` (quiet), no
/// invariant named, named but not placed, a dead path; an `unsafe impl` too.
#[test]
fn a_safety_comment_names_its_invariant_and_where_it_is_upheld() {
    let out = out(
        r#"
pub struct Ring {
    cap: usize,
}

impl Ring {
    pub fn new() -> Ring {
        Ring { cap: 8 }
    }
}

pub unsafe trait Marker {}

// SAFETY: `Ring::cap` is a power of two, the invariant of [`Ring`]; upheld by `Ring::new`.
unsafe impl Marker for Ring {}

pub fn good(p: *const u32) -> u32 {
    // SAFETY: `Ring::cap` bounds the read; upheld by `Ring::new`.
    unsafe { *p }
}

/// # Safety
///
/// `p` points at a live `u32`.
pub unsafe fn by_caller(p: *const u32) -> u32 {
    // SAFETY: the `# Safety` of `crate::by_caller`; upheld by the caller.
    unsafe { *p }
}

pub fn vague(p: *const u32) -> u32 {
    // SAFETY: the pointer is valid.
    unsafe { *p }
}

pub fn unplaced(p: *const u32) -> u32 {
    // SAFETY: `Ring::cap` bounds the read.
    unsafe { *p }
}

pub fn dead(p: *const u32) -> u32 {
    // SAFETY: `crate::gone::Thing` says so; upheld above.
    unsafe { *p }
}
"#,
    );
    assert_eq!(said(&out, "SAFETY comment in `good`"), NONE, "{out}");
    assert_eq!(said(&out, "SAFETY comment in `by_caller`"), NONE, "{out}");
    assert_eq!(said(&out, "SAFETY comment in this impl"), NONE, "{out}");
    assert_eq!(said(&out, "SAFETY comment in `vague` names no invariant").len(), 1, "{out}");
    assert_eq!(said(&out, "SAFETY comment in `unplaced` names what it relies on but not where").len(), 1, "{out}");
    assert_eq!(said(&out, "SAFETY comment in `dead` names `crate::gone::Thing`").len(), 1, "{out}");
}

// comment_claims

/// A body comment claiming what nothing checks is advice; the same over an `expect` is not.
#[test]
fn a_comment_claim_names_the_checked_form() {
    let out = out(
        r#"
pub fn get(o: Option<u32>) -> u32 {
    // can't be None: the caller filled it
    match o {
        Some(x) => x,
        None => 0,
    }
}

pub fn got(o: Option<u32>) -> u32 {
    // can't be None: the caller filled it
    o.expect("filled by the caller")
}
"#,
    );
    let found = said(&out, "this comment claims that an `Option` holds a value");
    assert_eq!(found.len(), 1, "{out}");
}
