use rustc_ast_ir::try_visit;
use rustc_next_trait_solver::{
    canonical::instantiate_canonical_state,
    solve::{SolverDelegateEvalExt, inspect},
};
use rustc_type_ir::{
    InferCtxtLike, UniverseIndex, VisitorResult,
    inherent::IntoKind,
    search_graph::RequiredDepth,
    solve::{Certainty, GoalSource, MaybeCause, MaybeInfo, NoSolution},
};
use thin_vec::ThinVec;

use crate::{
    Span,
    next_solver::{
        DbInterner, GenericArg, GenericArgs, Goal, NormalizesTo, Predicate, PredicateKind,
        QueryResult, SolverContext, infer::InferCtxt,
    },
};

pub(crate) use rustc_next_trait_solver::solve::inspect::*;

pub(crate) struct InspectConfig {
    pub(crate) max_depth: usize,
}

pub(crate) struct InspectGoal<'a, 'db> {
    infcx: &'a SolverContext<'db>,
    /// How deep this goal is below the root goal.
    depth: usize,
    /// The depth evaluating this goal needed.
    required_depth: RequiredDepth,
    orig_values: ThinVec<GenericArg<'db>>,
    prev_universe: UniverseIndex,
    goal: Goal<'db, Predicate<'db>>,
    result: Result<Certainty, NoSolution>,
    final_revision: inspect::Probe<DbInterner<'db>>,
    source: GoalSource,
}

impl<'a, 'db> std::fmt::Debug for InspectGoal<'a, 'db> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InspectGoal")
            .field("depth", &self.depth)
            .field("required_depth", &self.required_depth)
            .field("orig_values", &self.orig_values)
            .field("prev_universe", &self.prev_universe)
            .field("goal", &self.goal)
            .field("result", &self.result)
            .field("final_revision", &self.final_revision)
            .field("source", &self.source)
            .finish()
    }
}

impl<'a, 'db> std::fmt::Debug for InspectCandidate<'a, 'db> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InspectCandidate")
            .field("kind", &self.kind)
            .field("steps", &self.steps)
            .field("final_state", &self.final_state)
            .field("result", &self.result)
            .field("shallow_certainty", &self.shallow_certainty)
            .finish()
    }
}

pub(crate) struct InspectCandidate<'a, 'db> {
    goal: &'a InspectGoal<'a, 'db>,
    kind: inspect::ProbeKind<DbInterner<'db>>,
    steps: Vec<&'a inspect::ProbeStep<DbInterner<'db>>>,
    final_state: inspect::CanonicalState<DbInterner<'db>, ()>,
    result: QueryResult<'db>,
    shallow_certainty: Certainty,
}

impl<'a, 'db> InspectCandidate<'a, 'db> {
    pub(crate) fn kind(&self) -> inspect::ProbeKind<DbInterner<'db>> {
        self.kind
    }

    pub(crate) fn result(&self) -> Result<Certainty, NoSolution> {
        self.result.map(|c| c.value.certainty)
    }

    pub(crate) fn goal(&self) -> &'a InspectGoal<'a, 'db> {
        self.goal
    }

    /// Certainty passed into `evaluate_added_goals_and_make_canonical_response`.
    ///
    /// If this certainty is `Yes`, then we must be confident that the candidate
    /// must hold iff its nested goals hold. This is not true if the certainty is
    /// `Maybe(..)`, which suggests we forced ambiguity instead.
    ///
    /// This is *not* the certainty of the candidate's full nested evaluation, which
    /// can be accessed with [`Self::result`] instead.
    pub(crate) fn shallow_certainty(&self) -> Certainty {
        self.shallow_certainty
    }

    /// Visit all nested goals of this candidate without rolling
    /// back their inference constraints. This function modifies
    /// the state of the `infcx`.
    pub(crate) fn visit_nested_no_probe<V: ProofTreeVisitor<'db>>(
        &self,
        visitor: &mut V,
    ) -> V::Result {
        for goal in self.instantiate_nested_goals(visitor.span()) {
            try_visit!(goal.visit_with(visitor));
        }

        V::Result::output()
    }

    /// Instantiate the nested goals for the candidate without rolling back their
    /// inference constraints. This function modifies the state of the `infcx`.
    ///
    /// See [`Self::instantiate_impl_args`] if you need the impl args too.
    pub(crate) fn instantiate_nested_goals(&self, span: Span) -> Vec<InspectGoal<'a, 'db>> {
        self.instantiate_nested_goals_from(span, |_| true)
    }

    /// [`Self::instantiate_nested_goals`], building a proof tree only for the nested goals
    /// whose source `keep` accepts. Every step is still instantiated in order, as each one
    /// extends `orig_values` for the steps after it.
    pub(crate) fn instantiate_nested_goals_from(
        &self,
        span: Span,
        keep: impl Fn(GoalSource) -> bool,
    ) -> Vec<InspectGoal<'a, 'db>> {
        let infcx = self.goal.infcx;
        let prev_universe = self.goal.prev_universe;
        let mut orig_values = self.goal.orig_values.clone(); // clone: instantiating advances it

        let mut instantiated_goals = Vec::with_capacity(self.steps.len());
        for step in &self.steps {
            match **step {
                inspect::ProbeStep::AddGoal(source, goal) => {
                    let goal = instantiate_canonical_state(
                        infcx,
                        span,
                        prev_universe,
                        &mut orig_values,
                        goal,
                    );
                    if keep(source) {
                        instantiated_goals.push((source, goal));
                    }
                }
                inspect::ProbeStep::RecordImplArgs { .. } => {}
                inspect::ProbeStep::MakeCanonicalResponse { .. }
                | inspect::ProbeStep::NestedProbe(_) => unreachable!(),
            }
        }

        let () = instantiate_canonical_state(
            infcx,
            span,
            prev_universe,
            &mut orig_values,
            self.final_state,
        );

        instantiated_goals
            .into_iter()
            .map(|(source, goal)| self.instantiate_proof_tree_for_nested_goal(source, goal, span))
            .collect() // alloc: proof trees are built for diagnostics and overflow reports only
    }

    /// Instantiate the args of an impl if this candidate came from a
    /// `CandidateSource::Impl`. This function modifies the state of the
    /// `infcx`.
    pub(crate) fn instantiate_impl_args(&self, span: Span) -> GenericArgs<'db> {
        let infcx = self.goal.infcx;
        let prev_universe = self.goal.prev_universe;
        let mut orig_values = self.goal.orig_values.clone(); // clone: instantiating advances it

        for step in &self.steps {
            match **step {
                inspect::ProbeStep::RecordImplArgs { impl_args } => {
                    let impl_args = instantiate_canonical_state(
                        infcx,
                        span,
                        prev_universe,
                        &mut orig_values,
                        impl_args,
                    );

                    let () = instantiate_canonical_state(
                        infcx,
                        span,
                        prev_universe,
                        &mut orig_values,
                        self.final_state,
                    );

                    return infcx.deeply_resolve_via_unification_table(impl_args);
                }
                inspect::ProbeStep::AddGoal(..) => {}
                inspect::ProbeStep::MakeCanonicalResponse { .. }
                | inspect::ProbeStep::NestedProbe(_) => unreachable!(),
            }
        }

        panic!("expected impl args probe step for `instantiate_impl_args`");
    }

    pub(crate) fn instantiate_proof_tree_for_nested_goal(
        &self,
        source: GoalSource,
        goal: Goal<'db, Predicate<'db>>,
        span: Span,
    ) -> InspectGoal<'a, 'db> {
        let infcx = self.goal.infcx;
        match goal.predicate.kind().no_bound_vars() {
            Some(PredicateKind::NormalizesTo(NormalizesTo { .. })) => {
                unreachable!("the solver never adds a `NormalizesTo` goal as a nested goal")
            }
            _ => {
                // We're using a probe here as evaluating a goal could constrain
                // inference variables by choosing one candidate. If we then recurse
                // into another candidate who ends up with different inference
                // constraints, we get an ICE if we already applied the constraints
                // from the chosen candidate.
                let proof_tree =
                    infcx.probe(|_| infcx.evaluate_root_goal_for_proof_tree(goal, span).1);
                InspectGoal::new(infcx, self.goal.depth + 1, proof_tree, source)
            }
        }
    }

    /// Visit all nested goals of this candidate, rolling back
    /// all inference constraints.
    #[expect(dead_code, reason = "used in rustc")]
    fn visit_nested_in_probe<V: ProofTreeVisitor<'db>>(&self, visitor: &mut V) -> V::Result {
        self.goal.infcx.probe(|_| self.visit_nested_no_probe(visitor))
    }
}

impl<'a, 'db> InspectGoal<'a, 'db> {
    pub(crate) fn infcx(&self) -> &'a InferCtxt<'db> {
        self.infcx
    }

    pub(crate) fn goal(&self) -> Goal<'db, Predicate<'db>> {
        self.goal
    }

    pub(crate) fn result(&self) -> Result<Certainty, NoSolution> {
        self.result
    }

    pub(crate) fn source(&self) -> GoalSource {
        self.source
    }

    pub(crate) fn depth(&self) -> usize {
        self.depth
    }

    pub(crate) fn required_depth(&self) -> RequiredDepth {
        self.required_depth
    }

    fn candidates_recur(
        &'a self,
        candidates: &mut Vec<InspectCandidate<'a, 'db>>,
        steps: &mut Vec<&'a inspect::ProbeStep<DbInterner<'db>>>,
        probe: &'a inspect::Probe<DbInterner<'db>>,
    ) {
        let mut shallow_certainty = None;
        for step in &probe.steps {
            match *step {
                inspect::ProbeStep::AddGoal(..) | inspect::ProbeStep::RecordImplArgs { .. } => {
                    steps.push(step)
                }
                inspect::ProbeStep::MakeCanonicalResponse { shallow_certainty: c } => {
                    assert!(matches!(
                        shallow_certainty.replace(c),
                        None | Some(Certainty::Maybe(MaybeInfo {
                            cause: MaybeCause::Ambiguity,
                            opaque_types_jank: _,
                            stalled_on_coroutines: _,
                        }))
                    ));
                }
                inspect::ProbeStep::NestedProbe(ref probe) => {
                    match probe.kind {
                        // These never assemble candidates for the goal we're trying to solve.
                        inspect::ProbeKind::ProjectionCompatibility
                        | inspect::ProbeKind::ShadowedEnvProbing => continue,

                        inspect::ProbeKind::NormalizedSelfTyAssembly
                        | inspect::ProbeKind::UnsizeAssembly
                        | inspect::ProbeKind::Root { .. }
                        | inspect::ProbeKind::TraitCandidate { .. }
                        | inspect::ProbeKind::OpaqueTypeStorageLookup { .. }
                        | inspect::ProbeKind::RigidAlias { .. } => {
                            // Nested probes have to prove goals added in their parent
                            // but do not leak them, so we truncate the added goals
                            // afterwards.
                            let num_steps = steps.len();
                            self.candidates_recur(candidates, steps, probe);
                            steps.truncate(num_steps);
                        }
                    }
                }
            }
        }

        match probe.kind {
            inspect::ProbeKind::ProjectionCompatibility
            | inspect::ProbeKind::ShadowedEnvProbing => {
                panic!("{:?} never assembles a candidate", probe.kind)
            }

            inspect::ProbeKind::NormalizedSelfTyAssembly | inspect::ProbeKind::UnsizeAssembly => {}

            // We add a candidate even for the root evaluation if there
            // is only one way to prove a given goal, e.g. for `WellFormed`.
            inspect::ProbeKind::Root { result }
            | inspect::ProbeKind::TraitCandidate { source: _, result }
            | inspect::ProbeKind::OpaqueTypeStorageLookup { result }
            | inspect::ProbeKind::RigidAlias { result } => {
                // We only add a candidate if `shallow_certainty` was set, which means
                // that we ended up calling `evaluate_added_goals_and_make_canonical_response`.
                if let Some(shallow_certainty) = shallow_certainty {
                    candidates.push(InspectCandidate {
                        goal: self,
                        kind: probe.kind,
                        steps: steps.clone(), // clone: each candidate owns its step prefix
                        final_state: probe.final_state,
                        shallow_certainty,
                        result,
                    });
                }
            }
        }
    }

    pub(crate) fn candidates(&'a self) -> Vec<InspectCandidate<'a, 'db>> {
        let mut candidates = Vec::new(); // alloc: proof-tree inspection, diagnostics only
        let mut nested_goals = Vec::new(); // alloc: proof-tree inspection, diagnostics only
        self.candidates_recur(&mut candidates, &mut nested_goals, &self.final_revision);
        candidates
    }

    /// Returns the single candidate applicable for the current goal, if it exists.
    ///
    /// Returns `None` if there are either no or multiple applicable candidates.
    pub(crate) fn unique_applicable_candidate(&'a self) -> Option<InspectCandidate<'a, 'db>> {
        // FIXME(-Znext-solver): This does not handle impl candidates
        // hidden by env candidates.
        let mut candidates = self.candidates();
        candidates.retain(|c| c.result().is_ok());
        candidates.pop().filter(|_| candidates.is_empty())
    }

    pub(crate) fn new(
        infcx: &'a InferCtxt<'db>,
        depth: usize,
        root: inspect::GoalEvaluation<DbInterner<'db>>,
        source: GoalSource,
    ) -> Self {
        let infcx = <&SolverContext<'db>>::from(infcx);
        let prev_universe = infcx.universe();

        let inspect::GoalEvaluation {
            uncanonicalized_goal,
            orig_values,
            final_revision,
            result,
            required_depth,
        } = root;

        InspectGoal {
            infcx,
            depth,
            required_depth,
            orig_values,
            prev_universe,
            goal: infcx.deeply_resolve_via_unification_table(uncanonicalized_goal),
            result: result.map(|ok| ok.value.certainty),
            final_revision,
            source,
        }
    }

    pub(crate) fn visit_with<V: ProofTreeVisitor<'db>>(&self, visitor: &mut V) -> V::Result {
        if self.depth < visitor.config().max_depth {
            try_visit!(visitor.visit_goal(self));
            V::Result::output()
        } else {
            visitor.on_recursion_limit()
        }
    }
}

/// The public API to interact with proof trees.
pub(crate) trait ProofTreeVisitor<'db> {
    type Result: VisitorResult;

    fn span(&self) -> Span;

    fn config(&self) -> InspectConfig {
        InspectConfig { max_depth: 10 }
    }

    fn visit_goal(&mut self, goal: &InspectGoal<'_, 'db>) -> Self::Result;

    fn on_recursion_limit(&mut self) -> Self::Result {
        Self::Result::output()
    }
}

impl<'db> InferCtxt<'db> {
    pub(crate) fn visit_proof_tree<V: ProofTreeVisitor<'db>>(
        &self,
        goal: Goal<'db, Predicate<'db>>,
        visitor: &mut V,
    ) -> V::Result {
        self.visit_proof_tree_at_depth(goal, 0, visitor)
    }

    pub(crate) fn visit_proof_tree_at_depth<V: ProofTreeVisitor<'db>>(
        &self,
        goal: Goal<'db, Predicate<'db>>,
        depth: usize,
        visitor: &mut V,
    ) -> V::Result {
        let (_, proof_tree) = <&SolverContext<'db>>::from(self)
            .evaluate_root_goal_for_proof_tree(goal, visitor.span());
        visitor.visit_goal(&InspectGoal::new(self, depth, proof_tree, GoalSource::Misc))
    }
}
