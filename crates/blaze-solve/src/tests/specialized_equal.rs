//! `filter_specialized_impls` (assembly.rs) under `InferenceConstraints::NotAllowed`, the trait
//! goal's: a specializing impl candidate drops the impl it specializes when its response binds
//! no variable, or when its response EQUALS the victim's.
//!
//! The rule, the pinned rustc_next_trait_solver's `filter_specialized_impls`
//! (solve/assembly/mod.rs): "If both impls have the same response, dropping victim doesn't
//! change the result. We need to do so here as we don't merge distinct impl candidates."
//! (`|| c.result == candidates[i].result`); `try_merge_impl_candidates` (trait_goals.rs)
//! returns `None` for two user-written impls of a non-marker trait. The real site: alloc's
//! `SpecFromIterNested::from_iter` under `I: TrustedLen<Item = T>` calls
//! `vector.spec_extend(iterator)`, the goal `Vec<T>: SpecExtend<?X, I>`, which
//! `impl<T, I: TrustedLen<Item = T>, A> SpecExtend<T, I> for Vec<T, A>` and the
//! `I: Iterator<Item = T>` impl it specializes both answer `?X := T` (tempo t0850: 2 tests NOT
//! RUN with "2 candidates may each hold and none is chosen").
//!
//! - the fix: two equal binding responses, the first impl specializing the second: the second
//!   is dropped, one candidate is left (merged by `try_merge_impl` as itself);
//! - the twin: two DIFFERENT binding responses (the specializing impl would constrain the
//!   goal's variable otherwise than the general one): neither is dropped, so the goal is not
//!   decided by guessing the specializing impl (the merge bails: E0283);
//! - the control (rule 23, the cause removed): the same equal responses with the relation off
//!   (`specializes` false, two impls neither more specific: E0119 at the impls): both stay, as
//!   before the fix.

use blaze_own::Vec;

use crate::assembly::{Cand, Done, InferenceConstraints, Merger, Rule, Source, Via, filter_specialized_impls};
use crate::canon::{RespId, Responses};
use crate::error::Stop;
use crate::term::{Certainty, Def, TypingMode};

/// The specializing impl and the general one it specializes (`Def`s of no world: the filter
/// asks only `specializes`).
const TRUSTED_LEN_IMPL: Def = Def(0x7e11);
const ITERATOR_IMPL: Def = Def(0x17e2);

/// A response binding the goal's one variable to the word `to` (`[nvals = 1, len = 1, to,
/// ngoals = 0]`): never the identity, so `vars_identity` is false.
fn binding(resps: &mut Responses<'_>, to: u32) -> RespId {
    resps.push_cached(&[1, 1, to, 0], Certainty::Yes, Via::Misc).expect("a response row")
}

/// The impls left of `[TRUSTED_LEN_IMPL with resp_a, ITERATOR_IMPL with resp_b]` after the
/// filter, with `spec` answering whether TRUSTED_LEN_IMPL specializes ITERATOR_IMPL.
fn left(a_to: u32, b_to: u32, spec: bool) -> std::vec::Vec<Def> {
    let alloc = blaze_own::testing::heap().alloc();
    let mut resps = Responses::new_in(alloc);
    let ra = binding(&mut resps, a_to);
    let rb = binding(&mut resps, b_to);
    let m = Merger { resps: &mut resps, vars: 1, bail: None };
    let mut cands: Vec<Done, _> = Vec::new_in(alloc);
    cands.push(Done { cand: Cand::new(Source::Impl(TRUSTED_LEN_IMPL), Rule::Certain(Certainty::Yes)), resp: ra });
    cands.push(Done { cand: Cand::new(Source::Impl(ITERATOR_IMPL), Rule::Certain(Certainty::Yes)), resp: rb });
    let mut specializes = |a: Def, b: Def| -> Result<bool, Stop> { Ok(spec && a == TRUSTED_LEN_IMPL && b == ITERATOR_IMPL) };
    filter_specialized_impls(&m, TypingMode::Analysis, InferenceConstraints::NotAllowed, &mut cands, &mut specializes)
        .unwrap_or_else(|e| panic!("the filter stopped: {e:?}"));
    cands
        .iter()
        .map(|d| match d.cand.source {
            Source::Impl(i) => i,
            other => panic!("only impl candidates were given, got {other:?}"),
        })
        .collect()
}

/// The fix: equal binding responses (`?X := T` both), the TrustedLen impl specializing the
/// Iterator impl: the Iterator impl is dropped. Without the equal-response clause both stay
/// and the trait goal bails (the tempo E0283).
#[test]
fn an_equal_response_lets_the_specializing_impl_drop_the_general() {
    assert_eq!(left(7, 7, true), std::vec![TRUSTED_LEN_IMPL], "equal responses: the specialized impl is dropped");
}

/// The twin: the two responses bind the variable differently; under `NotAllowed` the
/// specializing impl may not drop the general one (it would decide the goal's variable by
/// the guess "the specializing impl applies"): both stay, and the merge bails.
#[test]
fn a_different_binding_response_keeps_both() {
    assert_eq!(left(7, 9, true), std::vec![TRUSTED_LEN_IMPL, ITERATOR_IMPL], "different responses: nothing dropped under NotAllowed");
}

/// The control: equal responses, but neither impl specializes the other: both stay (the
/// relation, not the equality, is what drops one).
#[test]
fn control_equal_responses_without_the_relation_keep_both() {
    assert_eq!(left(7, 7, false), std::vec![TRUSTED_LEN_IMPL, ITERATOR_IMPL], "no specialization: nothing dropped");
}
