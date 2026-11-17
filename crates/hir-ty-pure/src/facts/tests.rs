//! What the kinds of a summary prove, by table: the facts `fn_facts` infers from them.

use hir_ty::facts::{Fact, FactSet};

use super::{Plant, breaks, inferred_of, inferred_with};
use crate::effects::{EffectSet, Rule};

fn facts(of: &[Fact]) -> FactSet {
    of.iter().fold(FactSet::EMPTY, |set, &fact| set.with(fact))
}

#[test]
fn a_function_that_does_nothing_has_every_fact_the_kinds_decide() {
    let all = facts(&[Fact::NoUnwind, Fact::NoAlloc, Fact::NoBlock, Fact::Returns]);
    assert_eq!(inferred_of(EffectSet::EMPTY), all);
    // Output is none of their business.
    assert_eq!(inferred_of(EffectSet::one(Rule::Io)), all);
}

#[test]
fn each_kind_breaks_the_facts_it_is_evidence_against() {
    let without = |rule, lost: &[Fact]| {
        let kept = inferred_of(EffectSet::one(rule));
        for fact in [Fact::NoUnwind, Fact::NoAlloc, Fact::NoBlock, Fact::Returns] {
            assert_eq!(kept.has(fact), !lost.contains(&fact), "{rule:?} and {fact:?}");
        }
    };
    without(Rule::Panic, &[Fact::NoUnwind]);
    // Kept in the MIR of a crate built with overflow checks: there it can panic.
    without(Rule::Overflow, &[Fact::NoUnwind]);
    without(Rule::ForeignUnwind, &[Fact::NoUnwind]);
    without(Rule::Foreign, &[Fact::NoUnwind]);
    without(Rule::Alloc, &[Fact::NoAlloc]);
    without(Rule::Block, &[Fact::NoBlock]);
    without(Rule::Loop, &[Fact::Returns]);
    without(Rule::Recursion, &[Fact::Returns]);
    // What nobody saw proves nothing.
    let nothing = [Fact::NoUnwind, Fact::NoAlloc, Fact::NoBlock, Fact::Returns];
    without(Rule::Unchecked, &nothing);
    without(Rule::Open, &nothing);
    without(Rule::Indirect, &nothing);
    without(Rule::ForeignUnknown, &nothing);
}

#[test]
fn the_planted_bug_is_the_only_difference() {
    let indirect = EffectSet::one(Rule::Indirect);
    assert!(!inferred_of(indirect).has(Fact::NoUnwind));
    assert!(inferred_with(indirect, Plant::IndirectIgnored).has(Fact::NoUnwind));
    assert!(!inferred_with(indirect, Plant::IndirectIgnored).has(Fact::Returns));
    assert_eq!(breaks(Fact::Pure), None);
    assert_eq!(breaks(Fact::ProgramLifetime), None);
}
