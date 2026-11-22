//! Deterministic, Fuel- & Seed-Controlled SOTA Auto-Prover for Blaze's Lean 4 Kernel
//! ([`crate::lean_kernel`]).
//!
//! # LCF Architecture (Zero TCB Expansion)
//! Every proof synthesized by this module is a concrete Lean 4 Calculus of Inductive
//! Constructions term (`ExprId`) that is **independently type-checked by [`KernelEnv`]** before
//! being returned in [`AutoOutcome::Proved`]. No heuristic or search bug can ever forge an
//! unsound proof.
//!
//! # Strict Determinism (`fuel` + `seed`)
//! - **Fuel (`AutoConfig::fuel`)**: every search, unification, congruence-closure, rewrite, and
//!   branching step ticks an explicit deterministic counter by `1` (never wall-clock time).
//! - **Seed (`AutoConfig::seed`)**: feeds a deterministic SplitMix64 PRNG for tie-breaking among
//!   equally scored candidate proof steps and for [`auto_prove_portfolio`] strategy diversification.
//! - Identical `(env, goal, config)` always produces the bit-for-bit identical outcome, proof
//!   term, `fuel_used`, and [`ProofStep`] trace.
//!
//! # Proof Engines
//! 1. **Reflexivity & definitional computation** (`rfl`, `True.intro`, `whnf`, `is_def_eq`).
//! 2. **Context intro, decomposition & bounded forward modus ponens** (`∀`, `And`, `Iff`,
//!    `Exists`, `Prod`, `Or`, `False` / `Not` contradiction).
//! 3. **Proof-producing Congruence Closure (E-graph)** over local equalities and `whnf` bridges,
//!    synthesizing pure CIC equality proofs from `Eq.refl` and `Eq.rec` alone (symmetry,
//!    transitivity, multi-arg congruence, predicate transport, and constructor discrimination
//!    `ctor_i ... = ctor_j ... ⊢ False`).
//! 4. **First-order pattern unification & equational rewriting (`rw`)** using local hypotheses
//!    (including induction hypotheses) and kernel-checked environment theorems (strictly skipping
//!    `sorryAx`).
//! 5. **Seeded best-first backward chaining** over local hypotheses, constructors, and theorems.
//! 6. **Finite inductive case-splitting (`cases`) & generalized structural induction (`induction`)**
//!    via `T.rec` (e.g. `Bool.rec`, `Nat.rec`).
//! 7. **Deterministic seeded portfolio** ([`auto_prove_portfolio`]) partitioning fuel across
//!    multiple strategies and seeds.

use blaze_int::idx;
use blaze_own::{ABTreeMap, ABTreeSet, AVec, Alloc};

use crate::lean_kernel::{
    Decl, DeclKind, Expr, ExprId, KernelEnv, KernelError, LevelId, NameId, VerifiedTheorem,
};

/// Default deterministic fuel budget for [`AutoConfig::default`].
pub const DEFAULT_AUTO_FUEL: u32 = 4_000;

/// Default maximum proof-search tree depth.
pub const DEFAULT_AUTO_MAX_DEPTH: u32 = 14;

/// Default maximum equational rewrite steps along one proof branch.
pub const DEFAULT_MAX_REWRITE_STEPS: u32 = 6;

/// Search heuristic profile.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Strategy {
    /// Canonical order: fast close → CC → logical rules → rewriting → backward chain → induction.
    Balanced,
    /// Prioritize equational rewriting and congruence closure before logical splitting/induction.
    EqRewriteFirst,
    /// Prioritize structural case-splitting and induction early on inductive goals.
    InductionFirst,
    /// Prioritize backward chaining over environment theorems & constructors.
    BackwardWide,
}

/// Deterministic configuration for [`auto_prove`] and [`auto_prove_and_register`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AutoConfig {
    /// Strict deterministic step budget. Every search/tactic/CC/rewrite/unify step consumes 1.
    pub fuel: u32,
    /// Deterministic PRNG seed (`SplitMix64`) for tie-breaking equally-scored candidates.
    /// `0` selects canonical priority order; non-zero seeds deterministically explore variants.
    pub seed: u64,
    /// Maximum proof-search depth.
    pub max_depth: u32,
    /// Maximum equational rewrite steps along a single branch.
    pub max_rewrite_steps: u32,
    /// Enable proof-producing Congruence Closure (E-graph).
    pub enable_cc: bool,
    /// Enable finite inductive case-splitting and `Nat` structural induction.
    pub enable_induction: bool,
    /// Enable environment theorem rewriting & backward chaining.
    pub enable_lemmas: bool,
    /// Strategy ordering profile.
    pub strategy: Strategy,
}

impl AutoConfig {
    /// Canonical deterministic configuration (`fuel = 4_000`, `seed = 0`, `Strategy::Balanced`).
    #[must_use]
    pub const fn new(fuel: u32, seed: u64) -> Self {
        Self {
            fuel,
            seed,
            max_depth: DEFAULT_AUTO_MAX_DEPTH,
            max_rewrite_steps: DEFAULT_MAX_REWRITE_STEPS,
            enable_cc: true,
            enable_induction: true,
            enable_lemmas: true,
            strategy: Strategy::Balanced,
        }
    }

    /// Return a copy with a different [`Strategy`].
    #[must_use]
    pub const fn with_strategy(mut self, strategy: Strategy) -> Self {
        self.strategy = strategy;
        self
    }

    /// Return a copy with a different maximum search depth.
    #[must_use]
    pub const fn with_max_depth(mut self, max_depth: u32) -> Self {
        self.max_depth = max_depth;
        self
    }
}

impl Default for AutoConfig {
    fn default() -> Self {
        Self::new(DEFAULT_AUTO_FUEL, 0)
    }
}

/// Deterministic trace event recorded during proof synthesis.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProofStep {
    TrueIntro,
    Refl,
    Assumption,
    ExFalso,
    Intro,
    AndIntro,
    AndElim,
    OrLeft,
    OrRight,
    OrElim,
    IffIntro,
    ExistsIntro,
    ExistsElim,
    CongruenceClosure,
    CtorDiscrimination,
    PredicateTransport,
    RewriteLocal,
    RewriteLemma(NameId),
    ApplyLocal,
    ApplyConst(NameId),
    Cases(NameId),
    Induction(NameId),
}

/// Reason why [`auto_prove`] did not find a kernel-verified proof.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureReason {
    /// Deterministic fuel counter reached zero before search completed.
    OutOfFuel,
    /// Maximum recursion depth was reached along all open branches.
    DepthExceeded,
    /// Exhausted all enabled strategies without finding a proof.
    Unsolvable,
    /// The underlying kernel rejected a goal or encountered a kernel error.
    Kernel(KernelError),
}

/// Kernel-verified proof produced by [`auto_prove`].
#[derive(Debug)]
pub struct AutoProof<'a> {
    /// Closed proof term verified by `KernelEnv` (`infer_type(proof) ≡ goal_ty`).
    pub proof: ExprId,
    /// Exact deterministic fuel units consumed.
    pub fuel_used: u32,
    /// Fuel budget configured for the attempt.
    pub fuel_limit: u32,
    /// Seed used for the winning attempt.
    pub seed: u64,
    /// Strategy profile used for the winning attempt.
    pub strategy: Strategy,
    /// Number of proof steps applied.
    pub tactic_steps: u32,
    /// Deterministic trace of proof steps that closed the goal.
    pub trace: AVec<'a, ProofStep>,
}

/// Deterministic failure diagnostics returned by [`auto_prove`].
#[derive(Debug)]
pub struct AutoFailure<'a> {
    pub reason: FailureReason,
    pub fuel_used: u32,
    pub fuel_limit: u32,
    pub seed: u64,
    pub strategy: Strategy,
    /// Deepest / most specific open subgoal where proof search stalled.
    pub open_goal: Option<ExprId>,
    /// Partial trace leading up to failure.
    pub trace: AVec<'a, ProofStep>,
}

/// Outcome of [`auto_prove`] or [`auto_prove_portfolio`].
#[derive(Debug)]
pub enum AutoOutcome<'a> {
    Proved(AutoProof<'a>),
    Failed(AutoFailure<'a>),
}

impl<'a> AutoOutcome<'a> {
    #[inline]
    pub fn is_proved(&self) -> bool {
        matches!(self, Self::Proved(_))
    }

    #[inline]
    pub fn proof(&self) -> Option<&AutoProof<'a>> {
        match self {
            Self::Proved(p) => Some(p),
            Self::Failed(_) => None,
        }
    }

    #[inline]
    pub fn failure(&self) -> Option<&AutoFailure<'a>> {
        match self {
            Self::Proved(_) => None,
            Self::Failed(f) => Some(f),
        }
    }
}

// =================================================================================================
// Deterministic SplitMix64 PRNG
// =================================================================================================

#[derive(Clone, Copy, Debug)]
struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    #[inline]
    const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    #[inline]
    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Deterministic hash-based tie-breaker combining `seed` and `tag`.
    /// When `seed == 0`, returns `0` so canonical heuristic order is preserved exactly.
    #[inline]
    fn tie_break(seed: u64, tag: u64) -> u64 {
        if seed == 0 {
            return 0;
        }
        let mut s = Self::new(seed ^ tag.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        s.next_u64()
    }
}

// =================================================================================================
// Top-Level Entry Points
// =================================================================================================

/// Attempt to automatically prove `goal_ty` in `env` under deterministic `(fuel, seed)` control.
///
/// Every returned [`AutoOutcome::Proved`] has been independently verified by [`KernelEnv`]:
/// `env.infer_type(proof)` succeeds and `env.is_def_eq(inferred, goal_ty)` is `true`.
pub fn auto_prove<'a>(
    env: &mut KernelEnv<'a>,
    goal_ty: ExprId,
    config: AutoConfig,
) -> AutoOutcome<'a> {
    let alloc = env.alloc();
    let mut state = SearchState::new(alloc, config);
    let mut ctx = LocalCtx::new(alloc);
    let env_index = EnvIndex::build(env);

    match prove_goal(env, &env_index, &mut ctx, goal_ty, &mut state, 0, 0) {
        Ok(proof) => {
            // LCF kernel gate: NEVER trust the prover; verify every proof term with KernelEnv!
            match verify_proof_term(env, proof, goal_ty) {
                Ok(true) => AutoOutcome::Proved(AutoProof {
                    proof,
                    fuel_used: state.fuel_used(),
                    fuel_limit: config.fuel,
                    seed: config.seed,
                    strategy: config.strategy,
                    tactic_steps: state.tactic_steps,
                    trace: state.trace,
                }),
                Ok(false) => AutoOutcome::Failed(AutoFailure {
                    reason: FailureReason::Unsolvable,
                    fuel_used: state.fuel_used(),
                    fuel_limit: config.fuel,
                    seed: config.seed,
                    strategy: config.strategy,
                    open_goal: Some(goal_ty),
                    trace: state.trace,
                }),
                Err(kerr) => AutoOutcome::Failed(AutoFailure {
                    reason: FailureReason::Kernel(kerr),
                    fuel_used: state.fuel_used(),
                    fuel_limit: config.fuel,
                    seed: config.seed,
                    strategy: config.strategy,
                    open_goal: Some(goal_ty),
                    trace: state.trace,
                }),
            }
        }
        Err(abort) => {
            let reason = match abort {
                StepAbort::OutOfFuel => FailureReason::OutOfFuel,
                StepAbort::Depth => FailureReason::DepthExceeded,
                StepAbort::Kernel(e) => FailureReason::Kernel(e),
                StepAbort::NoProof => {
                    if state.hit_out_of_fuel {
                        FailureReason::OutOfFuel
                    } else if state.hit_depth {
                        FailureReason::DepthExceeded
                    } else {
                        FailureReason::Unsolvable
                    }
                }
            };
            AutoOutcome::Failed(AutoFailure {
                reason,
                fuel_used: state.fuel_used(),
                fuel_limit: config.fuel,
                seed: config.seed,
                strategy: config.strategy,
                open_goal: state.best_open_goal.or(Some(goal_ty)),
                trace: state.trace,
            })
        }
    }
}

/// Auto-prove `goal_ty`, register it as a checked `Theorem` named `name` in `env`, and
/// axiom-audit it via [`KernelEnv::verify_theorem`] against `allowed_axioms`.
///
/// Once registered, subsequent calls to [`auto_prove`] automatically see this theorem as a
/// reusable lemma for rewriting and backward chaining.
pub fn auto_prove_and_register<'a>(
    env: &mut KernelEnv<'a>,
    name: NameId,
    level_params: &[NameId],
    goal_ty: ExprId,
    config: AutoConfig,
    allowed_axioms: &[&str],
) -> Result<(AutoProof<'a>, VerifiedTheorem<'a>), AutoFailure<'a>> {
    match auto_prove(env, goal_ty, config) {
        AutoOutcome::Failed(f) => Err(f),
        AutoOutcome::Proved(ap) => {
            let alloc = env.alloc();
            let mut lp = AVec::new_in(alloc);
            for &p in level_params {
                lp.push(p);
            }
            if let Err(e) = env.check_decl(Decl::Def {
                name,
                level_params: lp,
                ty: goal_ty,
                value: ap.proof,
                is_theorem: true,
            }) {
                return Err(AutoFailure {
                    reason: FailureReason::Kernel(e),
                    fuel_used: ap.fuel_used,
                    fuel_limit: ap.fuel_limit,
                    seed: ap.seed,
                    strategy: ap.strategy,
                    open_goal: Some(goal_ty),
                    trace: ap.trace,
                });
            }
            match env.verify_theorem(name, allowed_axioms) {
                Ok(vt) => Ok((ap, vt)),
                Err(e) => Err(AutoFailure {
                    reason: FailureReason::Kernel(e),
                    fuel_used: ap.fuel_used,
                    fuel_limit: ap.fuel_limit,
                    seed: ap.seed,
                    strategy: ap.strategy,
                    open_goal: Some(goal_ty),
                    trace: ap.trace,
                }),
            }
        }
    }
}

/// Run a deterministic seeded portfolio across multiple strategies and seeds, partitioning
/// `total_fuel` across `attempts.max(1)` slices.
///
/// The first attempt always uses `(Strategy::Balanced, base_seed)`. Subsequent attempts rotate
/// through `EqRewriteFirst`, `InductionFirst`, `BackwardWide`, and SplitMix64-derived seeds.
/// Returns the first kernel-verified proof, or a deterministic failure reporting cumulative fuel.
pub fn auto_prove_portfolio<'a>(
    env: &mut KernelEnv<'a>,
    goal_ty: ExprId,
    total_fuel: u32,
    base_seed: u64,
    attempts: u32,
) -> AutoOutcome<'a> {
    let alloc = env.alloc();
    let count = attempts.max(1);
    let strategies = [
        Strategy::Balanced,
        Strategy::EqRewriteFirst,
        Strategy::InductionFirst,
        Strategy::BackwardWide,
    ];

    let mut prng = SplitMix64::new(base_seed ^ 0x517C_C1B7_2722_0A95);
    let mut remaining = total_fuel;
    let mut cumulative_used: u32 = 0;
    let mut last_failure: Option<AutoFailure<'a>> = None;

    for i in 0..count {
        if remaining == 0 {
            break;
        }
        let slices_left = count - i;
        let slice_fuel = if slices_left == 1 {
            remaining
        } else {
            (remaining / slices_left).max(1)
        };
        let strategy = strategies[idx(i) % strategies.len()];
        let seed = if i == 0 { base_seed } else { prng.next_u64() | 1 };
        let cfg = AutoConfig::new(slice_fuel, seed).with_strategy(strategy);

        match auto_prove(env, goal_ty, cfg) {
            AutoOutcome::Proved(mut proof) => {
                proof.fuel_used = cumulative_used.saturating_add(proof.fuel_used);
                proof.fuel_limit = total_fuel;
                return AutoOutcome::Proved(proof);
            }
            AutoOutcome::Failed(f) => {
                cumulative_used = cumulative_used.saturating_add(f.fuel_used);
                remaining = remaining.saturating_sub(f.fuel_used.max(1));
                last_failure = Some(f);
            }
        }
    }

    match last_failure {
        Some(mut f) => {
            f.fuel_used = cumulative_used.min(total_fuel);
            f.fuel_limit = total_fuel;
            AutoOutcome::Failed(f)
        }
        None => AutoOutcome::Failed(AutoFailure {
            reason: FailureReason::OutOfFuel,
            fuel_used: 0,
            fuel_limit: total_fuel,
            seed: base_seed,
            strategy: Strategy::Balanced,
            open_goal: Some(goal_ty),
            trace: AVec::new_in(alloc),
        }),
    }
}

fn verify_proof_term(
    env: &mut KernelEnv<'_>,
    proof: ExprId,
    expected_ty: ExprId,
) -> Result<bool, KernelError> {
    let inferred = env.infer_type(proof)?;
    env.is_def_eq(inferred, expected_ty)
}

// =================================================================================================
// Internal Search State & Local Context
// =================================================================================================

#[derive(Clone, Copy, Debug)]
enum StepAbort {
    NoProof,
    OutOfFuel,
    Depth,
    Kernel(KernelError),
}

struct SearchState<'a> {
    config: AutoConfig,
    fuel_left: u32,
    tactic_steps: u32,
    trace: AVec<'a, ProofStep>,
    active_goals: AVec<'a, (ExprId, usize)>,
    best_open_goal: Option<ExprId>,
    best_open_depth: u32,
    hit_out_of_fuel: bool,
    hit_depth: bool,
}

impl<'a> SearchState<'a> {
    fn new(alloc: Alloc<'a>, config: AutoConfig) -> Self {
        Self {
            config,
            fuel_left: config.fuel,
            tactic_steps: 0,
            trace: AVec::new_in(alloc),
            active_goals: AVec::new_in(alloc),
            best_open_goal: None,
            best_open_depth: 0,
            hit_out_of_fuel: false,
            hit_depth: false,
        }
    }

    #[inline]
    fn fuel_used(&self) -> u32 {
        self.config.fuel.saturating_sub(self.fuel_left)
    }

    #[inline]
    fn tick(&mut self) -> Result<(), StepAbort> {
        if self.fuel_left == 0 {
            self.hit_out_of_fuel = true;
            return Err(StepAbort::OutOfFuel);
        }
        self.fuel_left -= 1;
        Ok(())
    }

    #[inline]
    fn record(&mut self, step: ProofStep) {
        self.tactic_steps = self.tactic_steps.saturating_add(1);
        if self.trace.len() < 256 {
            self.trace.push(step);
        }
    }

    #[inline]
    fn note_open(&mut self, goal: ExprId, depth: u32) {
        if self.best_open_goal.is_none() || depth >= self.best_open_depth {
            self.best_open_goal = Some(goal);
            self.best_open_depth = depth;
        }
    }
}

/// One local assumption or variable in proof context.
#[derive(Clone, Copy, Debug)]
struct Hyp {
    /// Proof term / variable expression (e.g. `FVar` or derived `And.left h`).
    expr: ExprId,
    /// Type of `expr`.
    ty: ExprId,
    /// If `expr` is a direct `FVar`, its unique ID (eligible for `cases`/`induction`).
    fvar_id: Option<u32>,
}

struct LocalCtx<'a> {
    hyps: AVec<'a, Hyp>,
}

impl<'a> LocalCtx<'a> {
    fn new(alloc: Alloc<'a>) -> Self {
        Self { hyps: AVec::new_in(alloc) }
    }

    fn snapshot(&self) -> usize {
        self.hyps.len()
    }

    fn restore(&mut self, mark: usize) {
        self.hyps.truncate(mark);
    }

    fn push_fvar(&mut self, fvar_id: u32, expr: ExprId, ty: ExprId) {
        self.hyps.push(Hyp { expr, ty, fvar_id: Some(fvar_id) });
    }

    fn push_derived(&mut self, expr: ExprId, ty: ExprId) {
        // Avoid exact duplicate derived types so forward saturation stays compact.
        for h in self.hyps.as_slice() {
            if h.ty == ty {
                return;
            }
        }
        self.hyps.push(Hyp { expr, ty, fvar_id: None });
    }
}

// =================================================================================================
// Environment Index (Cached Names & Usable Lemmas)
// =================================================================================================

struct EnvIndex<'a> {
    true_n: Option<NameId>,
    true_intro_n: Option<NameId>,
    false_n: Option<NameId>,
    false_rec_n: Option<NameId>,
    and_n: Option<NameId>,
    and_intro_n: Option<NameId>,
    and_left_n: Option<NameId>,
    and_right_n: Option<NameId>,
    and_rec_n: Option<NameId>,
    or_n: Option<NameId>,
    or_inl_n: Option<NameId>,
    or_inr_n: Option<NameId>,
    or_rec_n: Option<NameId>,
    iff_n: Option<NameId>,
    iff_intro_n: Option<NameId>,
    iff_mp_n: Option<NameId>,
    iff_mpr_n: Option<NameId>,
    exists_n: Option<NameId>,
    exists_intro_n: Option<NameId>,
    exists_rec_n: Option<NameId>,
    eq_n: Option<NameId>,
    eq_refl_n: Option<NameId>,
    eq_rec_n: Option<NameId>,
    nat_n: Option<NameId>,
    nat_zero_n: Option<NameId>,
    nat_succ_n: Option<NameId>,
    nat_rec_n: Option<NameId>,
    /// Reusable theorems & constructors in deterministic `NameId` order (never `sorryAx`).
    lemmas: AVec<'a, NameId>,
    /// Map `inductive_name -> recursor_name`.
    recursors: ABTreeMap<'a, NameId, NameId>,
}

impl<'a> EnvIndex<'a> {
    fn build(env: &KernelEnv<'a>) -> Self {
        let alloc = env.alloc();
        let all = env.decl_names();
        let mut lemmas = AVec::new_in(alloc);
        let mut recursors = ABTreeMap::new_in(alloc);

        for &name in all.as_slice() {
            let s = env.name_str(name);
            // NEVER use sorryAx or unsafe axioms!
            if s == "sorryAx" || s == "sorry" || s.ends_with(".sorryAx") {
                continue;
            }
            match env.decl_kind(name) {
                Some(DeclKind::Def { is_theorem: true }) => {
                    lemmas.push(name);
                }
                Some(DeclKind::Recursor { ind_name, .. }) => {
                    recursors.entry(ind_name).or_insert(name);
                }
                _ => {}
            }
        }

        Self {
            true_n: env.lookup_name("True"),
            true_intro_n: env.lookup_name("True.intro"),
            false_n: env.lookup_name("False"),
            false_rec_n: env.lookup_name("False.rec"),
            and_n: env.lookup_name("And"),
            and_intro_n: env.lookup_name("And.intro"),
            and_left_n: env.lookup_name("And.left"),
            and_right_n: env.lookup_name("And.right"),
            and_rec_n: env.lookup_name("And.rec"),
            or_n: env.lookup_name("Or"),
            or_inl_n: env.lookup_name("Or.inl"),
            or_inr_n: env.lookup_name("Or.inr"),
            or_rec_n: env.lookup_name("Or.rec"),
            iff_n: env.lookup_name("Iff"),
            iff_intro_n: env.lookup_name("Iff.intro"),
            iff_mp_n: env.lookup_name("Iff.mp"),
            iff_mpr_n: env.lookup_name("Iff.mpr"),
            exists_n: env.lookup_name("Exists"),
            exists_intro_n: env.lookup_name("Exists.intro"),
            exists_rec_n: env.lookup_name("Exists.rec"),
            eq_n: env.lookup_name("Eq"),
            eq_refl_n: env.lookup_name("Eq.refl"),
            eq_rec_n: env.lookup_name("Eq.rec"),
            nat_n: env.lookup_name("Nat"),
            nat_zero_n: env.lookup_name("Nat.zero"),
            nat_succ_n: env.lookup_name("Nat.succ"),
            nat_rec_n: env.lookup_name("Nat.rec"),
            lemmas,
            recursors,
        }
    }
}

// =================================================================================================
// Core Expression Helpers & Pure-CIC Proof Combinators
// =================================================================================================

fn decompose_app<'a>(env: &KernelEnv<'a>, mut e: ExprId) -> (ExprId, AVec<'a, ExprId>) {
    let mut rev = AVec::new_in(env.alloc());
    while let Expr::App(f, a) = env.expr(e) {
        rev.push(a);
        e = f;
    }
    let mut args = AVec::new_in(env.alloc());
    while let Some(a) = rev.pop() {
        args.push(a);
    }
    (e, args)
}

fn decompose_eq(
    env: &mut KernelEnv<'_>,
    idx_env: &EnvIndex<'_>,
    ty: ExprId,
) -> Option<(LevelId, ExprId, ExprId, ExprId)> {
    let eq_n = idx_env.eq_n?;
    let try_one = |env: &KernelEnv<'_>, e: ExprId| -> Option<(LevelId, ExprId, ExprId, ExprId)> {
        let (head, args) = decompose_app(env, e);
        if args.len() == 3
            && let Expr::Const { name, levels } = env.expr(head)
            && name == eq_n
        {
            let lvls = env.const_levels(levels);
            if lvls.len() == 1 {
                return Some((lvls[0], args[0], args[1], args[2]));
            }
        }
        None
    };
    if let Some(res) = try_one(env, ty) {
        return Some(res);
    }
    let w = env.whnf(ty).ok()?;
    if w != ty { try_one(env, w) } else { None }
}

fn decompose_binary(
    env: &mut KernelEnv<'_>,
    expected_name: Option<NameId>,
    ty: ExprId,
) -> Option<(ExprId, ExprId)> {
    let target = expected_name?;
    let try_one = |env: &KernelEnv<'_>, e: ExprId| -> Option<(ExprId, ExprId)> {
        let (head, args) = decompose_app(env, e);
        if args.len() == 2
            && let Expr::Const { name, .. } = env.expr(head)
            && name == target
        {
            return Some((args[0], args[1]));
        }
        None
    };
    if let Some(res) = try_one(env, ty) {
        return Some(res);
    }
    let w = env.whnf(ty).ok()?;
    if w != ty { try_one(env, w) } else { None }
}

fn decompose_exists(
    env: &mut KernelEnv<'_>,
    idx_env: &EnvIndex<'_>,
    ty: ExprId,
) -> Option<(LevelId, ExprId, ExprId)> {
    let ex_n = idx_env.exists_n?;
    let try_one = |env: &KernelEnv<'_>, e: ExprId| -> Option<(LevelId, ExprId, ExprId)> {
        let (head, args) = decompose_app(env, e);
        if args.len() == 2
            && let Expr::Const { name, levels } = env.expr(head)
            && name == ex_n
        {
            let lvls = env.const_levels(levels);
            if lvls.len() == 1 {
                return Some((lvls[0], args[0], args[1]));
            }
        }
        None
    };
    if let Some(res) = try_one(env, ty) {
        return Some(res);
    }
    let w = env.whnf(ty).ok()?;
    if w != ty { try_one(env, w) } else { None }
}

fn is_const_named(env: &KernelEnv<'_>, e: ExprId, target: Option<NameId>) -> bool {
    let Some(t) = target else { return false };
    matches!(env.expr(e), Expr::Const { name, .. } if name == t)
}

/// Build `@Eq.refl.{u} α a`.
fn mk_eq_refl(
    env: &mut KernelEnv<'_>,
    idx_env: &EnvIndex<'_>,
    u: LevelId,
    alpha: ExprId,
    a: ExprId,
) -> Option<ExprId> {
    let refl_n = idx_env.eq_refl_n?;
    let c = env.mk_const(refl_n, &[u]);
    Some(env.mk_apps(c, &[alpha, a]))
}

/// Build `@Eq.rec.{v, u} α a motive minor b h`, where `motive_fn : α → Sort v` does not depend on
/// the equality proof argument.
fn mk_eq_subst(
    env: &mut KernelEnv<'_>,
    idx_env: &EnvIndex<'_>,
    u: LevelId,
    v: LevelId,
    alpha: ExprId,
    a: ExprId,
    b: ExprId,
    h_ab: ExprId,
    motive_fn: ExprId,
    minor: ExprId,
) -> Option<ExprId> {
    let eq_n = idx_env.eq_n?;
    let eq_rec_n = idx_env.eq_rec_n?;

    // Construct `motive = fun (x : α) (_ : Eq.{u} α a x) => motive_fn x`.
    let (x_id, x_fv) = env.mk_fresh_fvar(alpha);
    let eq_c = env.mk_const(eq_n, &[u]);
    let eq_a_x = env.mk_apps(eq_c, &[alpha, a, x_fv]);
    let m_x = env.mk_app(motive_fn, x_fv);
    let m_x_whnf = env.whnf(m_x).unwrap_or(m_x);
    // Keep body closed under the dummy `_ : Eq α a x` binder.
    let (_h_id, _h_fv) = env.mk_fresh_fvar(eq_a_x);
    let body_under_h = env.abstract_fvar(m_x_whnf, _h_id).ok()?;
    let inner_lam = env.mk_lam(eq_a_x, body_under_h);
    let body_under_x = env.abstract_fvar(inner_lam, x_id).ok()?;
    let motive = env.mk_lam(alpha, body_under_x);

    let rec_c = env.mk_const(eq_rec_n, &[v, u]);
    Some(env.mk_apps(rec_c, &[alpha, a, motive, minor, b, h_ab]))
}

/// Build a pure-CIC proof of `Eq.{u} α b a` from `h : Eq.{u} α a b` using only `Eq.refl` + `Eq.rec`.
fn mk_eq_symm(
    env: &mut KernelEnv<'_>,
    idx_env: &EnvIndex<'_>,
    u: LevelId,
    alpha: ExprId,
    a: ExprId,
    b: ExprId,
    h_ab: ExprId,
) -> Option<ExprId> {
    let eq_n = idx_env.eq_n?;
    let prop_lvl = env.level_zero();
    let (x_id, x_fv) = env.mk_fresh_fvar(alpha);
    let eq_c = env.mk_const(eq_n, &[u]);
    let eq_x_a = env.mk_apps(eq_c, &[alpha, x_fv, a]);
    let abs = env.abstract_fvar(eq_x_a, x_id).ok()?;
    let motive_fn = env.mk_lam(alpha, abs);
    let refl_a = mk_eq_refl(env, idx_env, u, alpha, a)?;
    mk_eq_subst(env, idx_env, u, prop_lvl, alpha, a, b, h_ab, motive_fn, refl_a)
}

/// Build a pure-CIC proof of `Eq.{u} α a c` from `h_ab : Eq.{u} α a b` and `h_bc : Eq.{u} α b c`.
fn mk_eq_trans(
    env: &mut KernelEnv<'_>,
    idx_env: &EnvIndex<'_>,
    u: LevelId,
    alpha: ExprId,
    a: ExprId,
    b: ExprId,
    c: ExprId,
    h_ab: ExprId,
    h_bc: ExprId,
) -> Option<ExprId> {
    let eq_n = idx_env.eq_n?;
    let prop_lvl = env.level_zero();
    let (x_id, x_fv) = env.mk_fresh_fvar(alpha);
    let eq_c = env.mk_const(eq_n, &[u]);
    let eq_a_x = env.mk_apps(eq_c, &[alpha, a, x_fv]);
    let abs = env.abstract_fvar(eq_a_x, x_id).ok()?;
    let motive_fn = env.mk_lam(alpha, abs);
    mk_eq_subst(env, idx_env, u, prop_lvl, alpha, b, c, h_bc, motive_fn, h_ab)
}

/// Build a pure-CIC proof of `Eq.{v} β (f a) (f b)` from `h_ab : Eq.{u} α a b`.
fn mk_congr_arg(
    env: &mut KernelEnv<'_>,
    idx_env: &EnvIndex<'_>,
    u: LevelId,
    alpha: ExprId,
    f: ExprId,
    a: ExprId,
    b: ExprId,
    h_ab: ExprId,
) -> Option<ExprId> {
    let eq_n = idx_env.eq_n?;
    let fa = env.mk_app(f, a);
    let beta = env.infer_type(fa).ok()?;
    let v = env.infer_sort(beta).ok()?;
    let prop_lvl = env.level_zero();

    let (x_id, x_fv) = env.mk_fresh_fvar(alpha);
    let fx = env.mk_app(f, x_fv);
    let eq_c = env.mk_const(eq_n, &[v]);
    let eq_fa_fx = env.mk_apps(eq_c, &[beta, fa, fx]);
    let abs = env.abstract_fvar(eq_fa_fx, x_id).ok()?;
    let motive_fn = env.mk_lam(alpha, abs);
    let refl_fa = mk_eq_refl(env, idx_env, v, beta, fa)?;
    mk_eq_subst(env, idx_env, u, prop_lvl, alpha, a, b, h_ab, motive_fn, refl_fa)
}

/// Build a pure-CIC proof of `Eq.{v} β (f a) (g a)` from `h_fg : Eq.{u_fn} (α → β) f g`.
fn mk_congr_fun(
    env: &mut KernelEnv<'_>,
    idx_env: &EnvIndex<'_>,
    u_fn: LevelId,
    fn_ty: ExprId,
    f: ExprId,
    g: ExprId,
    h_fg: ExprId,
    a: ExprId,
) -> Option<ExprId> {
    let eq_n = idx_env.eq_n?;
    let fa = env.mk_app(f, a);
    let beta = env.infer_type(fa).ok()?;
    let v = env.infer_sort(beta).ok()?;
    let prop_lvl = env.level_zero();

    let (fn_id, fn_fv) = env.mk_fresh_fvar(fn_ty);
    let fn_a = env.mk_app(fn_fv, a);
    let eq_c = env.mk_const(eq_n, &[v]);
    let eq_fa_fna = env.mk_apps(eq_c, &[beta, fa, fn_a]);
    let abs = env.abstract_fvar(eq_fa_fna, fn_id).ok()?;
    let motive_fn = env.mk_lam(fn_ty, abs);
    let refl_fa = mk_eq_refl(env, idx_env, v, beta, fa)?;
    mk_eq_subst(env, idx_env, u_fn, prop_lvl, fn_ty, f, g, h_fg, motive_fn, refl_fa)
}

/// Build `@False.rec.{v} (fun _ : False => target_ty) false_proof`.
fn mk_false_elim(
    env: &mut KernelEnv<'_>,
    idx_env: &EnvIndex<'_>,
    false_proof: ExprId,
    target_ty: ExprId,
) -> Option<ExprId> {
    let false_n = idx_env.false_n?;
    let false_rec_n = idx_env.false_rec_n?;
    let v = env.infer_sort(target_ty).ok()?;
    let false_c = env.mk_const(false_n, &[]);
    let (fv_id, _fv) = env.mk_fresh_fvar(false_c);
    let body = env.abstract_fvar(target_ty, fv_id).ok()?;
    let motive = env.mk_lam(false_c, body);
    let rec_c = env.mk_const(false_rec_n, &[v]);
    Some(env.mk_apps(rec_c, &[motive, false_proof]))
}

/// Pure-CIC constructor discrimination (no-confusion):
/// Given `h_eq : Eq.{u} T (ctor_i ...) (ctor_j ...)` with `i != j` on a 0-param, 0-index inductive
/// `T` (such as `Nat`, `Bool`, or any finite enum), constructs a closed proof of `False` using
/// only `T.rec.{1}`, `True`, `True.intro`, `False`, and `Eq.rec` — zero axioms!
fn mk_ctor_ne_false(
    env: &mut KernelEnv<'_>,
    idx_env: &EnvIndex<'_>,
    u: LevelId,
    ind_ty: ExprId,
    lhs: ExprId,
    rhs: ExprId,
    h_eq: ExprId,
) -> Option<ExprId> {
    let true_n = idx_env.true_n?;
    let true_intro_n = idx_env.true_intro_n?;
    let false_n = idx_env.false_n?;

    let (l_head, _) = decompose_app(env, lhs);
    let (r_head, _) = decompose_app(env, rhs);
    let Expr::Const { name: l_ctor, .. } = env.expr(l_head) else { return None };
    let Expr::Const { name: r_ctor, .. } = env.expr(r_head) else { return None };

    let Some(DeclKind::Ctor { ind_name: l_ind, ctor_idx: ci, num_params: 0, .. }) =
        env.decl_kind(l_ctor)
    else {
        return None;
    };
    let Some(DeclKind::Ctor { ind_name: r_ind, ctor_idx: cj, num_params: 0, .. }) =
        env.decl_kind(r_ctor)
    else {
        return None;
    };
    if l_ind != r_ind || ci == cj {
        return None;
    }

    let &rec_n = idx_env.recursors.get(&l_ind)?;
    let Some(DeclKind::Recursor {
        num_params: 0,
        num_indices: 0,
        num_motives: 1,
        num_minors,
        ..
    }) = env.decl_kind(rec_n)
    else {
        return None;
    };
    let mut lps = AVec::new_in(env.alloc());
    for &p in env.decl_level_params(rec_n) {
        lps.push(p);
    }
    if lps.len() != 1 {
        return None;
    }
    let rec_ty = env.decl_ty(rec_n)?;
    let type0_lvl = env.mk_level_nat(1); // Prop : Sort 1
    let mut rec_ty_inst = env.instantiate_levels(rec_ty, lps.as_slice(), &[type0_lvl]).ok()?;

    // Instantiate motive binder: `fun (_ : T) => Prop`.
    let prop = env.mk_prop();
    let true_c = env.mk_const(true_n, &[]);
    let false_c = env.mk_const(false_n, &[]);

    let Expr::ForallE { body: after_motive, .. } = env.expr(rec_ty_inst) else { return None };
    let (m_fv_id, _) = env.mk_fresh_fvar(ind_ty);
    let const_prop_body = env.abstract_fvar(prop, m_fv_id).ok()?;
    let const_prop = env.mk_lam(ind_ty, const_prop_body);
    rec_ty_inst = env.instantiate(after_motive, const_prop).ok()?;

    // Build each minor premise: wrap `True` (for `ci`) or `False` (for others) in lambdas matching
    // that minor's binder telescope!
    let mut rec_args = AVec::new_in(env.alloc());
    rec_args.push(const_prop);

    for m_idx in 0..num_minors {
        let Expr::ForallE { binder_ty: minor_ty, body: rest } = env.expr(rec_ty_inst) else {
            return None;
        };
        let target_prop = if m_idx == ci { true_c } else { false_c };
        let minor_val = wrap_const_for_telescope(env, minor_ty, target_prop)?;
        rec_args.push(minor_val);
        rec_ty_inst = env.instantiate(rest, minor_val).ok()?;
    }

    // Build `M = fun (x : ind_ty) => @T.rec.{1} (fun _ => Prop) minor_0 ... minor_{k-1} x`.
    let rec_c = env.mk_const(rec_n, &[type0_lvl]);
    let (x_id, x_fv) = env.mk_fresh_fvar(ind_ty);
    rec_args.push(x_fv);
    let app = env.mk_apps(rec_c, rec_args.as_slice());
    let abs = env.abstract_fvar(app, x_id).ok()?;
    let disc_motive = env.mk_lam(ind_ty, abs);

    let true_intro = env.mk_const(true_intro_n, &[]);
    let prop_lvl = env.level_zero();
    mk_eq_subst(env, idx_env, u, prop_lvl, ind_ty, lhs, rhs, h_eq, disc_motive, true_intro)
}

/// Given a telescope type `∀ (a1 : A1) ... (ak : Ak), Prop` and a closed `result : Prop`,
/// returns `fun (a1 : A1) ... (ak : Ak) => result`.
fn wrap_const_for_telescope(
    env: &mut KernelEnv<'_>,
    mut ty: ExprId,
    result: ExprId,
) -> Option<ExprId> {
    let mut binders = AVec::new_in(env.alloc());
    loop {
        let w = env.whnf(ty).ok()?;
        match env.expr(w) {
            Expr::ForallE { binder_ty, body } => {
                let (fv_id, fv) = env.mk_fresh_fvar(binder_ty);
                binders.push((fv_id, binder_ty));
                ty = env.instantiate(body, fv).ok()?;
            }
            _ => break,
        }
    }
    let mut term = result;
    while let Some((fv_id, bty)) = binders.pop() {
        let abs = env.abstract_fvar(term, fv_id).ok()?;
        term = env.mk_lam(bty, abs);
    }
    Some(term)
}

/// Replace a single first-found syntactic occurrence of `needle` in `haystack` with `replacement`.
fn replace_first_subterm(
    env: &mut KernelEnv<'_>,
    haystack: ExprId,
    needle: ExprId,
    replacement: ExprId,
    depth: u32,
) -> Option<(ExprId, bool)> {
    if depth > 64 {
        return None;
    }
    if haystack == needle {
        return Some((replacement, true));
    }
    match env.expr(haystack) {
        Expr::App(f, a) => {
            let (f2, cf) = replace_first_subterm(env, f, needle, replacement, depth + 1)?;
            if cf {
                return Some((env.mk_app(f2, a), true));
            }
            let (a2, ca) = replace_first_subterm(env, a, needle, replacement, depth + 1)?;
            if ca {
                return Some((env.mk_app(f, a2), true));
            }
            Some((haystack, false))
        }
        Expr::Proj { struct_name, field_idx, expr } => {
            let (e2, c) = replace_first_subterm(env, expr, needle, replacement, depth + 1)?;
            if c {
                Some((env.mk_proj(struct_name, field_idx, e2), true))
            } else {
                Some((haystack, false))
            }
        }
        _ => Some((haystack, false)),
    }
}

/// Check if `target_fvar` appears anywhere inside `expr`.
fn expr_contains_fvar(env: &KernelEnv<'_>, expr: ExprId, target_fvar: u32, depth: u32) -> bool {
    if depth > 64 {
        return true;
    }
    match env.expr(expr) {
        Expr::FVar { id, .. } => id == target_fvar,
        Expr::BVar(_) | Expr::Sort(_) | Expr::Const { .. } | Expr::Lit(_) => false,
        Expr::App(f, a) => {
            expr_contains_fvar(env, f, target_fvar, depth + 1)
                || expr_contains_fvar(env, a, target_fvar, depth + 1)
        }
        Expr::Lam { binder_ty, body } | Expr::ForallE { binder_ty, body } => {
            expr_contains_fvar(env, binder_ty, target_fvar, depth + 1)
                || expr_contains_fvar(env, body, target_fvar, depth + 1)
        }
        Expr::LetE { ty, val, body } => {
            expr_contains_fvar(env, ty, target_fvar, depth + 1)
                || expr_contains_fvar(env, val, target_fvar, depth + 1)
                || expr_contains_fvar(env, body, target_fvar, depth + 1)
        }
        Expr::Proj { expr, .. } => expr_contains_fvar(env, expr, target_fvar, depth + 1),
    }
}

// =================================================================================================
// Proof-Producing Congruence Closure (E-Graph with Equality Proof Forest)
// =================================================================================================

#[derive(Clone, Copy, Debug)]
enum CcJustify {
    /// Local equality hypothesis `proof : Eq.{u} ty a b`.
    Hyp { u: LevelId, ty: ExprId, proof: ExprId },
    /// Congruence of application nodes `f1 a1` and `f2 a2`.
    Congr { f1: u32, f2: u32, a1: u32, a2: u32 },
    /// Definitional equality (`is_def_eq`), justified by `Eq.refl`.
    DefEq { u: LevelId, ty: ExprId },
}

#[derive(Clone, Copy, Debug)]
struct CcEdge {
    /// Target node in the proof spanning forest.
    to: u32,
    /// `true` if `justify` proves `self = to`; `false` if `justify` proves `to = self`.
    forward: bool,
    justify: CcJustify,
}

#[derive(Clone, Copy, Debug)]
struct CcNode {
    expr: ExprId,
    /// Optional application decomposition `(fn_node, arg_node)`.
    app: Option<(u32, u32)>,
    /// Union-find parent.
    uf_parent: u32,
    uf_rank: u8,
    /// Proof spanning forest parent edge.
    proof_parent: Option<CcEdge>,
}

struct CcGraph<'a> {
    nodes: AVec<'a, CcNode>,
    expr_to_node: ABTreeMap<'a, ExprId, u32>,
    /// Signature table `(find(f), find(a)) -> app_node` for congruence propagation.
    sig_table: ABTreeMap<'a, (u32, u32), u32>,
    /// Pending equality merges `(n1, n2, justify)`.
    pending: AVec<'a, (u32, u32, CcJustify)>,
}

impl<'a> CcGraph<'a> {
    fn new(alloc: Alloc<'a>) -> Self {
        Self {
            nodes: AVec::new_in(alloc),
            expr_to_node: ABTreeMap::new_in(alloc),
            sig_table: ABTreeMap::new_in(alloc),
            pending: AVec::new_in(alloc),
        }
    }

    fn intern(&mut self, env: &KernelEnv<'a>, e: ExprId, depth: u32) -> u32 {
        if let Some(&id) = self.expr_to_node.get(&e) {
            return id;
        }
        let app = if depth < 24
            && let Expr::App(f, a) = env.expr(e)
        {
            let fn_id = self.intern(env, f, depth + 1);
            let arg_id = self.intern(env, a, depth + 1);
            Some((fn_id, arg_id))
        } else {
            None
        };
        let id = self.nodes.len() as u32;
        self.nodes.push(CcNode {
            expr: e,
            app,
            uf_parent: id,
            uf_rank: 0,
            proof_parent: None,
        });
        self.expr_to_node.insert(e, id);
        id
    }

    fn find(&mut self, mut x: u32) -> u32 {
        while self.nodes[idx(x)].uf_parent != x {
            let p = self.nodes[idx(x)].uf_parent;
            let gp = self.nodes[idx(p)].uf_parent;
            self.nodes[idx(x)].uf_parent = gp;
            x = gp;
        }
        x
    }

    fn enqueue(&mut self, u: u32, v: u32, justify: CcJustify) {
        if u != v {
            self.pending.push((u, v, justify));
        }
    }

    /// Re-root the proof tree containing `u` so that `u` becomes its root (`proof_parent = None`).
    fn reroot_proof_tree(&mut self, alloc: Alloc<'a>, u: u32) {
        let mut chain: AVec<'a, (u32, CcEdge)> = AVec::new_in(alloc);
        let mut cur = u;
        while let Some(edge) = self.nodes[idx(cur)].proof_parent {
            chain.push((cur, edge));
            cur = edge.to;
        }
        self.nodes[idx(u)].proof_parent = None;
        for (child, edge) in chain.as_slice() {
            // Reverse `child -> edge.to` into `edge.to -> child` with flipped direction.
            self.nodes[idx(edge.to)].proof_parent = Some(CcEdge {
                to: *child,
                forward: !edge.forward,
                justify: edge.justify,
            });
        }
    }

    fn saturate(
        &mut self,
        env: &mut KernelEnv<'a>,
        state: &mut SearchState<'a>,
    ) -> Result<(), StepAbort> {
        let alloc = env.alloc();
        // Add whnf definitional bridges for all interned nodes.
        let initial_len = self.nodes.len();
        for i in 0..initial_len {
            state.tick()?;
            let e = self.nodes[i].expr;
            if let Ok(w) = env.whnf(e)
                && w != e
                && let Ok(ty) = env.infer_type(e)
                && let Ok(u) = env.infer_sort(ty)
            {
                let wn = self.intern(env, w, 0);
                self.enqueue(i as u32, wn, CcJustify::DefEq { u, ty });
            }
        }

        // Fixed-point congruence saturation bounded by fuel.
        let mut changed = true;
        while changed {
            changed = false;
            while let Some((u, v, justify)) = self.pending.pop() {
                state.tick()?;
                let ru = self.find(u);
                let rv = self.find(v);
                if ru == rv {
                    continue;
                }
                // Connect proof spanning trees (always acyclic because `ru != rv`).
                self.reroot_proof_tree(alloc, u);
                self.nodes[idx(u)].proof_parent = Some(CcEdge {
                    to: v,
                    forward: true,
                    justify,
                });
                // Union by rank.
                let rank_u = self.nodes[idx(ru)].uf_rank;
                let rank_v = self.nodes[idx(rv)].uf_rank;
                if rank_u < rank_v {
                    self.nodes[idx(ru)].uf_parent = rv;
                } else if rank_u > rank_v {
                    self.nodes[idx(rv)].uf_parent = ru;
                } else {
                    self.nodes[idx(rv)].uf_parent = ru;
                    self.nodes[idx(ru)].uf_rank = rank_u.saturating_add(1);
                }
                changed = true;
            }

            // Rebuild signature table and discover new congruences.
            self.sig_table.clear();
            let n = self.nodes.len();
            for i in 0..n {
                state.tick()?;
                if let Some((f, a)) = self.nodes[i].app {
                    let rf = self.find(f);
                    let ra = self.find(a);
                    let key = (rf, ra);
                    if let Some(&prev) = self.sig_table.get(&key) {
                        if self.find(prev) != self.find(i as u32)
                            && let Some((pf, pa)) = self.nodes[idx(prev)].app
                        {
                            self.pending.push((
                                prev,
                                i as u32,
                                CcJustify::Congr { f1: pf, f2: f, a1: pa, a2: a },
                            ));
                            changed = true;
                        }
                    } else {
                        self.sig_table.insert(key, i as u32);
                    }
                }
            }
        }
        Ok(())
    }

    /// Reconstruct a real CIC equality proof of `Eq.{u} α nodes[src].expr nodes[dst].expr`
    /// from the proof spanning forest (if `src` and `dst` are in the same E-class).
    fn explain(
        &mut self,
        env: &mut KernelEnv<'a>,
        idx_env: &EnvIndex<'a>,
        src: u32,
        dst: u32,
        depth: u32,
    ) -> Option<ExprId> {
        if depth > 32 {
            return None;
        }
        if self.find(src) != self.find(dst) {
            return None;
        }
        let src_expr = self.nodes[idx(src)].expr;
        let dst_expr = self.nodes[idx(dst)].expr;
        let alpha = env.infer_type(src_expr).ok()?;
        let u = env.infer_sort(alpha).ok()?;

        if src == dst || env.is_def_eq(src_expr, dst_expr).unwrap_or(false) {
            return mk_eq_refl(env, idx_env, u, alpha, src_expr);
        }

        let alloc = env.alloc();
        // Collect ancestor paths from `src` and `dst` to their shared proof-tree root.
        let mut src_anc: AVec<'a, (u32, Option<CcEdge>)> = AVec::new_in(alloc);
        let mut src_pos: ABTreeMap<'a, u32, usize> = ABTreeMap::new_in(alloc);
        let mut cur = src;
        loop {
            let pos = src_anc.len();
            src_pos.insert(cur, pos);
            let edge = self.nodes[idx(cur)].proof_parent;
            src_anc.push((cur, edge));
            match edge {
                Some(e) => cur = e.to,
                None => break,
            }
        }

        let mut dst_path: AVec<'a, (u32, CcEdge)> = AVec::new_in(alloc);
        let mut cur = dst;
        let lca_idx_in_src;
        loop {
            if let Some(&pos) = src_pos.get(&cur) {
                lca_idx_in_src = pos;
                break;
            }
            let edge = self.nodes[idx(cur)].proof_parent?;
            dst_path.push((cur, edge));
            cur = edge.to;
        }

        // Build proof from `src` up to `lca`, then down from `lca` to `dst`.
        let mut current_node = src;
        let mut acc_proof: Option<ExprId> = None;

        for i in 0..lca_idx_in_src {
            let (from_n, edge_opt) = src_anc[i];
            let edge = edge_opt?;
            let step_pf =
                self.explain_edge(env, idx_env, from_n, edge.to, edge.forward, edge.justify, depth + 1)?;
            acc_proof = Some(match acc_proof {
                None => step_pf,
                Some(prev) => mk_eq_trans(
                    env,
                    idx_env,
                    u,
                    alpha,
                    self.nodes[idx(src)].expr,
                    self.nodes[idx(from_n)].expr,
                    self.nodes[idx(edge.to)].expr,
                    prev,
                    step_pf,
                )?,
            });
            current_node = edge.to;
        }

        while let Some((child_n, edge)) = dst_path.pop() {
            // Traversing `edge.to -> child_n` (opposite direction of `edge`).
            let step_pf = self.explain_edge(
                env,
                idx_env,
                edge.to,
                child_n,
                !edge.forward,
                edge.justify,
                depth + 1,
            )?;
            acc_proof = Some(match acc_proof {
                None => step_pf,
                Some(prev) => mk_eq_trans(
                    env,
                    idx_env,
                    u,
                    alpha,
                    self.nodes[idx(src)].expr,
                    self.nodes[idx(current_node)].expr,
                    self.nodes[idx(child_n)].expr,
                    prev,
                    step_pf,
                )?,
            });
            current_node = child_n;
        }

        acc_proof.or_else(|| mk_eq_refl(env, idx_env, u, alpha, src_expr))
    }

    /// Explain a single proof-forest edge from `from` to `to`.
    #[allow(clippy::too_many_arguments)]
    fn explain_edge(
        &mut self,
        env: &mut KernelEnv<'a>,
        idx_env: &EnvIndex<'a>,
        from: u32,
        to: u32,
        forward: bool,
        justify: CcJustify,
        depth: u32,
    ) -> Option<ExprId> {
        let from_e = self.nodes[idx(from)].expr;
        let to_e = self.nodes[idx(to)].expr;
        match justify {
            CcJustify::Hyp { u, ty, proof } => {
                if forward {
                    Some(proof)
                } else {
                    mk_eq_symm(env, idx_env, u, ty, to_e, from_e, proof)
                }
            }
            CcJustify::DefEq { u, ty } => mk_eq_refl(env, idx_env, u, ty, from_e),
            CcJustify::Congr { f1, f2, a1, a2 } => {
                let (fa_f, fa_a, gb_f, gb_a) = if forward {
                    (f1, a1, f2, a2)
                } else {
                    (f2, a2, f1, a1)
                };
                let f_expr = self.nodes[idx(fa_f)].expr;
                let g_expr = self.nodes[idx(gb_f)].expr;
                let a_expr = self.nodes[idx(fa_a)].expr;
                let b_expr = self.nodes[idx(gb_a)].expr;

                let mut steps: Option<(ExprId, ExprId)> = None; // (mid_expr, proof_from_fa_to_mid)
                // Step 1: `f a = g a` (if `f != g`)
                if !env.is_def_eq(f_expr, g_expr).unwrap_or(false) {
                    let h_fg = self.explain(env, idx_env, fa_f, gb_f, depth + 1)?;
                    let fn_ty = env.infer_type(f_expr).ok()?;
                    let u_fn = env.infer_sort(fn_ty).ok()?;
                    let pf1 = mk_congr_fun(env, idx_env, u_fn, fn_ty, f_expr, g_expr, h_fg, a_expr)?;
                    let ga = env.mk_app(g_expr, a_expr);
                    steps = Some((ga, pf1));
                }
                // Step 2: `g a = g b` (if `a != b`)
                if !env.is_def_eq(a_expr, b_expr).unwrap_or(false) {
                    let h_ab = self.explain(env, idx_env, fa_a, gb_a, depth + 1)?;
                    let a_ty = env.infer_type(a_expr).ok()?;
                    let u_a = env.infer_sort(a_ty).ok()?;
                    let pf2 = mk_congr_arg(env, idx_env, u_a, a_ty, g_expr, a_expr, b_expr, h_ab)?;
                    steps = Some(match steps {
                        None => (to_e, pf2),
                        Some((ga, pf1)) => {
                            let res_ty = env.infer_type(from_e).ok()?;
                            let u_res = env.infer_sort(res_ty).ok()?;
                            let chained = mk_eq_trans(
                                env, idx_env, u_res, res_ty, from_e, ga, to_e, pf1, pf2,
                            )?;
                            (to_e, chained)
                        }
                    });
                }
                match steps {
                    Some((_, pf)) => Some(pf),
                    None => {
                        let res_ty = env.infer_type(from_e).ok()?;
                        let u_res = env.infer_sort(res_ty).ok()?;
                        mk_eq_refl(env, idx_env, u_res, res_ty, from_e)
                    }
                }
            }
        }
    }
}

// =================================================================================================
// First-Order Pattern Unification (for Rewriting & Backward Chaining)
// =================================================================================================

/// Match pattern `pat` (which may reference pattern variables `mvars` as `FVar`s with IDs in
/// `mvar_ids`) against concrete expression `target`. Fills `assign` (`mvar_id -> ExprId`).
fn match_pattern(
    env: &mut KernelEnv<'_>,
    pat: ExprId,
    target: ExprId,
    mvar_ids: &[u32],
    assign: &mut ABTreeMap<'_, u32, ExprId>,
    depth: u32,
) -> bool {
    if depth > 48 {
        return false;
    }
    if let Expr::FVar { id, .. } = env.expr(pat)
        && mvar_ids.contains(&id)
    {
        if let Some(&existing) = assign.get(&id) {
            return existing == target || env.is_def_eq(existing, target).unwrap_or(false);
        }
        assign.insert(id, target);
        return true;
    }
    if pat == target {
        return true;
    }
    match (env.expr(pat), env.expr(target)) {
        (Expr::App(pf, pa), Expr::App(tf, ta)) => {
            match_pattern(env, pf, tf, mvar_ids, assign, depth + 1)
                && match_pattern(env, pa, ta, mvar_ids, assign, depth + 1)
        }
        (
            Expr::Const { name: pn, levels: pl },
            Expr::Const { name: tn, levels: tl },
        ) => {
            if pn != tn {
                return false;
            }
            let pl_s = env.const_levels(pl);
            let tl_s = env.const_levels(tl);
            pl_s.len() == tl_s.len()
        }
        (Expr::Lit(a), Expr::Lit(b)) => a == b,
        (Expr::Sort(a), Expr::Sort(b)) => env.level_eq(a, b),
        (
            Expr::Proj { struct_name: ps, field_idx: pi, expr: pe },
            Expr::Proj { struct_name: ts, field_idx: ti, expr: te },
        ) => ps == ts && pi == ti && match_pattern(env, pe, te, mvar_ids, assign, depth + 1),
        _ => {
            // Try whnf on target (e.g. matching `Nat.add x (Nat.succ y)` against a reduced/unreduced term).
            if let Ok(tw) = env.whnf(target)
                && tw != target
            {
                return match_pattern(env, pat, tw, mvar_ids, assign, depth + 1);
            }
            false
        }
    }
}

/// Replace pattern FVars in `expr` using `assign`.
fn subst_mvars(
    env: &mut KernelEnv<'_>,
    expr: ExprId,
    assign: &ABTreeMap<'_, u32, ExprId>,
    depth: u32,
) -> Option<ExprId> {
    if depth > 64 {
        return None;
    }
    match env.expr(expr) {
        Expr::FVar { id, .. } => Some(assign.get(&id).copied().unwrap_or(expr)),
        Expr::BVar(_) | Expr::Sort(_) | Expr::Const { .. } | Expr::Lit(_) => Some(expr),
        Expr::App(f, a) => {
            let f2 = subst_mvars(env, f, assign, depth + 1)?;
            let a2 = subst_mvars(env, a, assign, depth + 1)?;
            Some(if f2 == f && a2 == a { expr } else { env.mk_app(f2, a2) })
        }
        Expr::Lam { binder_ty, body } => {
            let t2 = subst_mvars(env, binder_ty, assign, depth + 1)?;
            let b2 = subst_mvars(env, body, assign, depth + 1)?;
            Some(if t2 == binder_ty && b2 == body { expr } else { env.mk_lam(t2, b2) })
        }
        Expr::ForallE { binder_ty, body } => {
            let t2 = subst_mvars(env, binder_ty, assign, depth + 1)?;
            let b2 = subst_mvars(env, body, assign, depth + 1)?;
            Some(if t2 == binder_ty && b2 == body { expr } else { env.mk_forall(t2, b2) })
        }
        Expr::LetE { ty, val, body } => {
            let t2 = subst_mvars(env, ty, assign, depth + 1)?;
            let v2 = subst_mvars(env, val, assign, depth + 1)?;
            let b2 = subst_mvars(env, body, assign, depth + 1)?;
            Some(if t2 == ty && v2 == val && b2 == body { expr } else { env.mk_let(t2, v2, b2) })
        }
        Expr::Proj { struct_name, field_idx, expr: inner } => {
            let i2 = subst_mvars(env, inner, assign, depth + 1)?;
            Some(if i2 == inner { expr } else { env.mk_proj(struct_name, field_idx, i2) })
        }
    }
}

/// Collect all subterms of `e` (excluding binder bodies) in deterministic pre-order.
fn collect_subterms<'a>(env: &KernelEnv<'a>, e: ExprId, out: &mut AVec<'a, ExprId>, depth: u32) {
    if depth > 24 || out.len() >= 128 {
        return;
    }
    if !out.as_slice().contains(&e) {
        out.push(e);
    }
    match env.expr(e) {
        Expr::App(f, a) => {
            collect_subterms(env, f, out, depth + 1);
            collect_subterms(env, a, out, depth + 1);
        }
        Expr::Proj { expr, .. } => collect_subterms(env, expr, out, depth + 1),
        _ => {}
    }
}

// =================================================================================================
// Main Deterministic Recursive Prover
// =================================================================================================

fn prove_goal<'a>(
    env: &mut KernelEnv<'a>,
    idx_env: &EnvIndex<'a>,
    ctx: &mut LocalCtx<'a>,
    goal: ExprId,
    state: &mut SearchState<'a>,
    depth: u32,
    rw_steps: u32,
) -> Result<ExprId, StepAbort> {
    state.tick()?;
    if depth > state.config.max_depth {
        state.hit_depth = true;
        state.note_open(goal, depth);
        return Err(StepAbort::Depth);
    }
    let goal_key = (goal, ctx.hyps.len());
    if state.active_goals.as_slice().contains(&goal_key) {
        return Err(StepAbort::NoProof);
    }

    state.active_goals.push(goal_key);
    let result = prove_goal_inner(env, idx_env, ctx, goal, state, depth, rw_steps);
    state.active_goals.pop();
    if result.is_err() {
        state.note_open(goal, depth);
    }
    result
}

fn prove_goal_inner<'a>(
    env: &mut KernelEnv<'a>,
    idx_env: &EnvIndex<'a>,
    ctx: &mut LocalCtx<'a>,
    goal: ExprId,
    state: &mut SearchState<'a>,
    depth: u32,
    rw_steps: u32,
) -> Result<ExprId, StepAbort> {
    // -------------------------------------------------------------------------
    // Stage 0: Fast Direct Closures (`True`, `rfl`, direct assumption, `False`)
    // -------------------------------------------------------------------------
    if let Some(pf) = try_direct_close(env, idx_env, ctx, goal, state)? {
        return Ok(pf);
    }

    // -------------------------------------------------------------------------
    // Stage 1: Intros (`∀ x : A, B` or `Not P` = `P → False`)
    // -------------------------------------------------------------------------
    let goal_whnf = env.whnf(goal).map_err(StepAbort::Kernel)?;
    if let Expr::ForallE { binder_ty, body } = env.expr(goal_whnf) {
        state.tick()?;
        let (fv_id, fv) = env.mk_fresh_fvar(binder_ty);
        let inst_body = env.instantiate(body, fv).map_err(StepAbort::Kernel)?;
        let mark = ctx.snapshot();
        ctx.push_fvar(fv_id, fv, binder_ty);
        let sub_pf = prove_goal(env, idx_env, ctx, inst_body, state, depth + 1, rw_steps);
        ctx.restore(mark);
        let body_pf = sub_pf?;
        let abs = env.abstract_fvar(body_pf, fv_id).map_err(StepAbort::Kernel)?;
        state.record(ProofStep::Intro);
        return Ok(env.mk_lam(binder_ty, abs));
    }

    // -------------------------------------------------------------------------
    // Stage 2: Safe Context Saturation (`And`/`Iff` unpacking + bounded Modus Ponens)
    // -------------------------------------------------------------------------
    let sat_mark = ctx.snapshot();
    saturate_context(env, idx_env, ctx, state)?;

    // Re-check direct closures after context saturation!
    if ctx.snapshot() > sat_mark
        && let Some(pf) = try_direct_close(env, idx_env, ctx, goal, state)?
    {
        ctx.restore(sat_mark);
        return Ok(pf);
    }

    let res = prove_after_saturation(env, idx_env, ctx, goal, goal_whnf, state, depth, rw_steps);
    ctx.restore(sat_mark);
    res
}

fn prove_after_saturation<'a>(
    env: &mut KernelEnv<'a>,
    idx_env: &EnvIndex<'a>,
    ctx: &mut LocalCtx<'a>,
    goal: ExprId,
    goal_whnf: ExprId,
    state: &mut SearchState<'a>,
    depth: u32,
    rw_steps: u32,
) -> Result<ExprId, StepAbort> {
    // -------------------------------------------------------------------------
    // Stage 3: Proof-Producing Congruence Closure (E-graph)
    // -------------------------------------------------------------------------
    if state.config.enable_cc
        && let Some(pf) = try_congruence_closure(env, idx_env, ctx, goal, state)?
    {
        return Ok(pf);
    }

    // -------------------------------------------------------------------------
    // Stage 4: Safe Invertible Goal Connectives (`And`, `Iff`) & Context Splits (`Or`, `Exists`)
    // -------------------------------------------------------------------------
    // 4a. Goal `And A B` -> prove `A` and `B`, build `And.intro A B pa pb`.
    if let Some((a, b)) = decompose_binary(env, idx_env.and_n, goal_whnf)
        && let Some(and_intro_n) = idx_env.and_intro_n
    {
        state.tick()?;
        let pa = prove_goal(env, idx_env, ctx, a, state, depth + 1, rw_steps)?;
        let pb = prove_goal(env, idx_env, ctx, b, state, depth + 1, rw_steps)?;
        let intro = env.mk_const(and_intro_n, &[]);
        state.record(ProofStep::AndIntro);
        return Ok(env.mk_apps(intro, &[a, b, pa, pb]));
    }

    // 4b. Goal `Iff A B` -> prove `A → B` and `B → A`, build `Iff.intro`.
    if let Some((a, b)) = decompose_binary(env, idx_env.iff_n, goal_whnf)
        && let Some(iff_intro_n) = idx_env.iff_intro_n
    {
        state.tick()?;
        let ab = env.mk_arrow(a, b).map_err(StepAbort::Kernel)?;
        let ba = env.mk_arrow(b, a).map_err(StepAbort::Kernel)?;
        let p_ab = prove_goal(env, idx_env, ctx, ab, state, depth + 1, rw_steps)?;
        let p_ba = prove_goal(env, idx_env, ctx, ba, state, depth + 1, rw_steps)?;
        let intro = env.mk_const(iff_intro_n, &[]);
        state.record(ProofStep::IffIntro);
        return Ok(env.mk_apps(intro, &[a, b, p_ab, p_ba]));
    }

    // 4c. Context `Exists α P` -> eliminate via `Exists.rec` (non-branching!).
    if let Some(pf) = try_exists_elim(env, idx_env, ctx, goal, state, depth, rw_steps)? {
        return Ok(pf);
    }

    // 4d. Context `Or A B` -> eliminate via `Or.rec` (splits into 2 branches!).
    if let Some(pf) = try_or_elim(env, idx_env, ctx, goal, state, depth, rw_steps)? {
        return Ok(pf);
    }

    // -------------------------------------------------------------------------
    // Stage 5: Strategy-Ordered Non-Invertible Search
    //   - Disjunction / Existential intro (`Or`, `Exists`)
    //   - Equational rewriting (`rw` with local `ih` & env lemmas)
    //   - Backward chaining (`apply` local hyps & env theorems)
    //   - Structural `cases` (Bool/finite enums) & `induction` (`Nat.rec`)
    // -------------------------------------------------------------------------
    let phase_order: [u8; 4] = match state.config.strategy {
        Strategy::Balanced => [0, 1, 2, 3],       // logical -> rw -> backward -> induction
        Strategy::EqRewriteFirst => [1, 0, 3, 2], // rw -> logical -> induction -> backward
        Strategy::InductionFirst => [3, 1, 0, 2], // induction -> rw -> logical -> backward
        Strategy::BackwardWide => [2, 0, 1, 3],   // backward -> logical -> rw -> induction
    };

    for phase in phase_order {
        match phase {
            0 => {
                if let Some(pf) =
                    try_logical_non_invertible(env, idx_env, ctx, goal_whnf, state, depth, rw_steps)?
                {
                    return Ok(pf);
                }
            }
            1 => {
                if rw_steps < state.config.max_rewrite_steps
                    && let Some(pf) =
                        try_equational_rewrite(env, idx_env, ctx, goal_whnf, state, depth, rw_steps)?
                {
                    return Ok(pf);
                }
            }
            2 => {
                if let Some(pf) =
                    try_backward_chain(env, idx_env, ctx, goal_whnf, state, depth, rw_steps)?
                {
                    return Ok(pf);
                }
            }
            3 => {
                if state.config.enable_induction
                    && let Some(pf) =
                        try_cases_and_induction(env, idx_env, ctx, goal, state, depth, rw_steps)?
                {
                    return Ok(pf);
                }
            }
            _ => {}
        }
    }

    Err(StepAbort::NoProof)
}

// =================================================================================================
// Stage 0: Direct Closures
// =================================================================================================

fn try_direct_close<'a>(
    env: &mut KernelEnv<'a>,
    idx_env: &EnvIndex<'a>,
    ctx: &LocalCtx<'a>,
    goal: ExprId,
    state: &mut SearchState<'a>,
) -> Result<Option<ExprId>, StepAbort> {
    let goal_w = env.whnf(goal).map_err(StepAbort::Kernel)?;

    // 1. `True` -> `True.intro`
    if is_const_named(env, goal_w, idx_env.true_n)
        && let Some(ti) = idx_env.true_intro_n
    {
        state.record(ProofStep::TrueIntro);
        return Ok(Some(env.mk_const(ti, &[])));
    }

    // 2. `Eq α a b` with `is_def_eq(a, b)` -> `Eq.refl α a`
    if let Some((u, alpha, a, b)) = decompose_eq(env, idx_env, goal_w)
        && env.is_def_eq(a, b).unwrap_or(false)
        && let Some(pf) = mk_eq_refl(env, idx_env, u, alpha, a)
    {
        state.record(ProofStep::Refl);
        return Ok(Some(pf));
    }

    // 3. Direct hypothesis match (`is_def_eq(h.ty, goal)`), or `False` hypothesis, or `P` + `P → False`
    let mut false_proof: Option<ExprId> = None;
    for h in ctx.hyps.as_slice() {
        state.tick()?;
        if env.is_def_eq(h.ty, goal).unwrap_or(false) {
            state.record(ProofStep::Assumption);
            return Ok(Some(h.expr));
        }
        if false_proof.is_none()
            && let Ok(hw) = env.whnf(h.ty)
            && is_const_named(env, hw, idx_env.false_n)
        {
            false_proof = Some(h.expr);
        }
    }

    // Check direct `h_p : P` and `h_np : P → False` (or `Not P`).
    if false_proof.is_none() && idx_env.false_n.is_some() {
        'outer: for h_neg in ctx.hyps.as_slice() {
            let Ok(hw) = env.whnf(h_neg.ty) else { continue };
            if let Expr::ForallE { binder_ty, body } = env.expr(hw)
                && is_const_named(env, body, idx_env.false_n)
            {
                for h_pos in ctx.hyps.as_slice() {
                    if env.is_def_eq(h_pos.ty, binder_ty).unwrap_or(false) {
                        false_proof = Some(env.mk_app(h_neg.expr, h_pos.expr));
                        break 'outer;
                    }
                }
            }
        }
    }

    // Check direct absurd constructor equality `h : ctor_i ... = ctor_j ...` in context.
    if false_proof.is_none() {
        for h in ctx.hyps.as_slice() {
            if let Some((u, alpha, lhs, rhs)) = decompose_eq(env, idx_env, h.ty) {
                let lw = env.whnf(lhs).unwrap_or(lhs);
                let rw = env.whnf(rhs).unwrap_or(rhs);
                if let Some(fp) = mk_ctor_ne_false(env, idx_env, u, alpha, lw, rw, h.expr) {
                    false_proof = Some(fp);
                    state.record(ProofStep::CtorDiscrimination);
                    break;
                }
            }
        }
    }

    if let Some(fp) = false_proof
        && let Some(pf) = mk_false_elim(env, idx_env, fp, goal)
    {
        state.record(ProofStep::ExFalso);
        return Ok(Some(pf));
    }

    Ok(None)
}

// =================================================================================================
// Stage 2: Safe Context Decomposition & Bounded Forward Modus Ponens
// =================================================================================================

fn saturate_context<'a>(
    env: &mut KernelEnv<'a>,
    idx_env: &EnvIndex<'a>,
    ctx: &mut LocalCtx<'a>,
    state: &mut SearchState<'a>,
) -> Result<(), StepAbort> {
    let mut round = 0u32;
    while round < 4 && ctx.hyps.len() < 48 {
        let before = ctx.hyps.len();
        let snapshot_len = ctx.hyps.len();
        for i in 0..snapshot_len {
            state.tick()?;
            let h = ctx.hyps[i];
            // Unpack `And A B`
            if let Some((a, b)) = decompose_binary(env, idx_env.and_n, h.ty) {
                if let Some(left_n) = idx_env.and_left_n {
                    let c = env.mk_const(left_n, &[]);
                    let e = env.mk_apps(c, &[a, b, h.expr]);
                    ctx.push_derived(e, a);
                } else if let Some(and_rec_n) = idx_env.and_rec_n {
                    if let Some(e) = mk_and_proj_via_rec(env, and_rec_n, a, b, h.expr, true) {
                        ctx.push_derived(e, a);
                    }
                }
                if let Some(right_n) = idx_env.and_right_n {
                    let c = env.mk_const(right_n, &[]);
                    let e = env.mk_apps(c, &[a, b, h.expr]);
                    ctx.push_derived(e, b);
                } else if let Some(and_rec_n) = idx_env.and_rec_n {
                    if let Some(e) = mk_and_proj_via_rec(env, and_rec_n, a, b, h.expr, false) {
                        ctx.push_derived(e, b);
                    }
                }
                state.record(ProofStep::AndElim);
            }
            // Unpack `Iff A B` into `A → B` and `B → A`
            if let Some((a, b)) = decompose_binary(env, idx_env.iff_n, h.ty) {
                if let Some(mp_n) = idx_env.iff_mp_n
                    && let Ok(ab) = env.mk_arrow(a, b)
                {
                    let c = env.mk_const(mp_n, &[]);
                    ctx.push_derived(env.mk_apps(c, &[a, b, h.expr]), ab);
                }
                if let Some(mpr_n) = idx_env.iff_mpr_n
                    && let Ok(ba) = env.mk_arrow(b, a)
                {
                    let c = env.mk_const(mpr_n, &[]);
                    ctx.push_derived(env.mk_apps(c, &[a, b, h.expr]), ba);
                }
            }
            // Forward Modus Ponens: non-dependent `h : A → B` with `arg : A` already in `ctx`.
            if let Ok(hw) = env.whnf(h.ty)
                && let Expr::ForallE { binder_ty, body } = env.expr(hw)
                && let Ok(dom_sort) = env.infer_sort(binder_ty)
                && env.level_eq(dom_sort, env.level_zero())
            {
                for j in 0..snapshot_len {
                    if ctx.hyps.len() >= 48 {
                        break;
                    }
                    let arg = ctx.hyps[j];
                    if env.is_def_eq(arg.ty, binder_ty).unwrap_or(false)
                        && let Ok(res_ty) = env.instantiate(body, arg.expr)
                    {
                        let app = env.mk_app(h.expr, arg.expr);
                        ctx.push_derived(app, res_ty);
                    }
                }
            }
        }
        if ctx.hyps.len() == before {
            break;
        }
        round += 1;
    }
    Ok(())
}

/// Project `And.left` / `And.right` purely from `And.rec` if `And.left`/`And.right` aren't declared.
fn mk_and_proj_via_rec(
    env: &mut KernelEnv<'_>,
    and_rec_n: NameId,
    a: ExprId,
    b: ExprId,
    h: ExprId,
    pick_left: bool,
) -> Option<ExprId> {
    let target = if pick_left { a } else { b };
    let v = env.infer_sort(target).ok()?;
    let (fa_id, fa) = env.mk_fresh_fvar(a);
    let (fb_id, fb) = env.mk_fresh_fvar(b);
    let chosen = if pick_left { fa } else { fb };
    let abs_b = env.abstract_fvar(chosen, fb_id).ok()?;
    let lam_b = env.mk_lam(b, abs_b);
    let abs_a = env.abstract_fvar(lam_b, fa_id).ok()?;
    let minor = env.mk_lam(a, abs_a);
    // And.rec.{u} : ∀ {a b : Prop} {motive : And a b → Sort u} (intro : a → b → motive ...) (t : And a b), motive t
    let and_ty = env.infer_type(h).ok()?;
    let (ft_id, _) = env.mk_fresh_fvar(and_ty);
    let m_body = env.abstract_fvar(target, ft_id).ok()?;
    let motive = env.mk_lam(and_ty, m_body);
    let rec_c = env.mk_const(and_rec_n, &[v]);
    Some(env.mk_apps(rec_c, &[a, b, motive, minor, h]))
}

// =================================================================================================
// Stage 3: Proof-Producing Congruence Closure Driver
// =================================================================================================

fn try_congruence_closure<'a>(
    env: &mut KernelEnv<'a>,
    idx_env: &EnvIndex<'a>,
    ctx: &LocalCtx<'a>,
    goal: ExprId,
    state: &mut SearchState<'a>,
) -> Result<Option<ExprId>, StepAbort> {
    if idx_env.eq_n.is_none() || idx_env.eq_refl_n.is_none() || idx_env.eq_rec_n.is_none() {
        return Ok(None);
    }

    // Collect equality hypotheses in `ctx`.
    let mut eq_hyps: AVec<'a, (LevelId, ExprId, ExprId, ExprId, ExprId)> =
        AVec::new_in(env.alloc());
    for h in ctx.hyps.as_slice() {
        if let Some((u, alpha, a, b)) = decompose_eq(env, idx_env, h.ty) {
            eq_hyps.push((u, alpha, a, b, h.expr));
        }
    }
    if eq_hyps.is_empty() {
        return Ok(None);
    }

    let mut cc = CcGraph::new(env.alloc());
    for &(u, alpha, a, b, pf) in eq_hyps.as_slice() {
        let na = cc.intern(env, a, 0);
        let nb = cc.intern(env, b, 0);
        cc.enqueue(na, nb, CcJustify::Hyp { u, ty: alpha, proof: pf });
    }

    // Intern goal sides (if equality) and predicate hypotheses for transport.
    let goal_eq = decompose_eq(env, idx_env, goal);
    let goal_nodes = if let Some((_, _, gl, gr)) = goal_eq {
        let nl = cc.intern(env, gl, 0);
        let nr = cc.intern(env, gr, 0);
        Some((nl, nr))
    } else {
        let _ = cc.intern(env, goal, 0);
        for h in ctx.hyps.as_slice() {
            let _ = cc.intern(env, h.ty, 0);
        }
        None
    };

    cc.saturate(env, state)?;

    // 1. Direct equality closure: `goal = (lhs = rhs)` and `find(lhs) == find(rhs)`!
    if let Some((nl, nr)) = goal_nodes
        && cc.find(nl) == cc.find(nr)
        && let Some(pf) = cc.explain(env, idx_env, nl, nr, 0)
    {
        state.record(ProofStep::CongruenceClosure);
        return Ok(Some(pf));
    }

    // 2. Constructor discrimination inside E-classes (e.g. `a = 0, a = succ b ⊢ False`).
    if idx_env.false_rec_n.is_some() {
        let mut rep_to_ctor: ABTreeMap<'a, u32, (u32, NameId, u32)> =
            ABTreeMap::new_in(env.alloc());
        let n = cc.nodes.len();
        for i in 0..n {
            let e = cc.nodes[i].expr;
            let (head, _) = decompose_app(env, e);
            if let Expr::Const { name: c_name, .. } = env.expr(head)
                && let Some(DeclKind::Ctor { ind_name, ctor_idx, num_params: 0, .. }) =
                    env.decl_kind(c_name)
            {
                let r = cc.find(i as u32);
                if let Some(&(other_node, other_ind, other_idx)) = rep_to_ctor.get(&r) {
                    if other_ind == ind_name
                        && other_idx != ctor_idx
                        && let Some(eq_pf) = cc.explain(env, idx_env, other_node, i as u32, 0)
                    {
                        let lhs = cc.nodes[idx(other_node)].expr;
                        let rhs = e;
                        if let Ok(ind_ty) = env.infer_type(lhs)
                            && let Ok(u) = env.infer_sort(ind_ty)
                            && let Some(false_pf) =
                                mk_ctor_ne_false(env, idx_env, u, ind_ty, lhs, rhs, eq_pf)
                            && let Some(goal_pf) = mk_false_elim(env, idx_env, false_pf, goal)
                        {
                            state.record(ProofStep::CtorDiscrimination);
                            return Ok(Some(goal_pf));
                        }
                    }
                } else {
                    rep_to_ctor.insert(r, (i as u32, ind_name, ctor_idx));
                }
            }
        }
    }

    // 3. Predicate transport: `h_p : P a`, goal is `P b`, and `find(a) == find(b)`!
    if goal_eq.is_none()
        && let Expr::App(g_pred, g_arg) = env.expr(goal)
        && let Some(&g_arg_node) = cc.expr_to_node.get(&g_arg)
    {
        for h in ctx.hyps.as_slice() {
            if let Expr::App(h_pred, h_arg) = env.expr(h.ty)
                && env.is_def_eq(h_pred, g_pred).unwrap_or(false)
                && let Some(&h_arg_node) = cc.expr_to_node.get(&h_arg)
                && cc.find(h_arg_node) == cc.find(g_arg_node)
                && let Some(eq_pf) = cc.explain(env, idx_env, h_arg_node, g_arg_node, 0)
                && let Ok(arg_ty) = env.infer_type(h_arg)
                && let Ok(u) = env.infer_sort(arg_ty)
                && let Ok(v) = env.infer_sort(goal)
                && let Some(pf) =
                    mk_eq_subst(env, idx_env, u, v, arg_ty, h_arg, g_arg, eq_pf, g_pred, h.expr)
            {
                state.record(ProofStep::PredicateTransport);
                return Ok(Some(pf));
            }
        }
    }

    Ok(None)
}

// =================================================================================================
// Stage 4c/4d: Context `Exists` & `Or` Elimination
// =================================================================================================

fn try_exists_elim<'a>(
    env: &mut KernelEnv<'a>,
    idx_env: &EnvIndex<'a>,
    ctx: &mut LocalCtx<'a>,
    goal: ExprId,
    state: &mut SearchState<'a>,
    depth: u32,
    rw_steps: u32,
) -> Result<Option<ExprId>, StepAbort> {
    let Some(ex_rec_n) = idx_env.exists_rec_n else { return Ok(None) };
    // Only eliminate into `Prop` (`Exists.rec` eliminates into `Prop` in Lean 4!).
    let Ok(g_sort) = env.infer_sort(goal) else { return Ok(None) };
    if !env.level_eq(g_sort, env.level_zero()) {
        return Ok(None);
    }

    let len = ctx.hyps.len();
    for i in 0..len {
        let h = ctx.hyps[i];
        let Some((u, alpha, pred)) = decompose_exists(env, idx_env, h.ty) else { continue };
        // Check if we already unpacked this `Exists` (avoid infinite re-elimination).
        let already = ctx.hyps.as_slice().iter().any(|other| {
            matches!(env.expr(other.ty), Expr::App(p, _) if p == pred)
        });
        if already {
            continue;
        }
        state.tick()?;
        let (w_id, w_fv) = env.mk_fresh_fvar(alpha);
        let pw_raw = env.mk_app(pred, w_fv);
        let pw = env.whnf(pw_raw).unwrap_or(pw_raw);
        let (hw_id, hw_fv) = env.mk_fresh_fvar(pw);

        let mark = ctx.snapshot();
        ctx.push_fvar(w_id, w_fv, alpha);
        ctx.push_fvar(hw_id, hw_fv, pw);
        let sub = prove_goal(env, idx_env, ctx, goal, state, depth + 1, rw_steps);
        ctx.restore(mark);
        if let Ok(sub_pf) = sub {
            let abs_hw = env.abstract_fvar(sub_pf, hw_id).map_err(StepAbort::Kernel)?;
            let lam_hw = env.mk_lam(pw, abs_hw);
            let abs_w = env.abstract_fvar(lam_hw, w_id).map_err(StepAbort::Kernel)?;
            let minor = env.mk_lam(alpha, abs_w);

            let (t_id, _) = env.mk_fresh_fvar(h.ty);
            let m_body = env.abstract_fvar(goal, t_id).map_err(StepAbort::Kernel)?;
            let motive = env.mk_lam(h.ty, m_body);
            let rec_c = env.mk_const(ex_rec_n, &[u]);
            state.record(ProofStep::ExistsElim);
            return Ok(Some(env.mk_apps(rec_c, &[alpha, pred, motive, minor, h.expr])));
        }
    }
    Ok(None)
}

fn try_or_elim<'a>(
    env: &mut KernelEnv<'a>,
    idx_env: &EnvIndex<'a>,
    ctx: &mut LocalCtx<'a>,
    goal: ExprId,
    state: &mut SearchState<'a>,
    depth: u32,
    rw_steps: u32,
) -> Result<Option<ExprId>, StepAbort> {
    let Some(or_rec_n) = idx_env.or_rec_n else { return Ok(None) };
    let Ok(g_sort) = env.infer_sort(goal) else { return Ok(None) };
    if !env.level_eq(g_sort, env.level_zero()) {
        return Ok(None);
    }

    let len = ctx.hyps.len();
    for i in 0..len {
        let h = ctx.hyps[i];
        let Some((a, b)) = decompose_binary(env, idx_env.or_n, h.ty) else { continue };
        // Skip if either disjunct is already in context (prevents infinite splitting).
        let already = ctx
            .hyps
            .as_slice()
            .iter()
            .any(|o| o.ty == a || o.ty == b);
        if already {
            continue;
        }
        state.tick()?;

        let (ha_id, ha_fv) = env.mk_fresh_fvar(a);
        let mark_a = ctx.snapshot();
        ctx.push_fvar(ha_id, ha_fv, a);
        let res_a = prove_goal(env, idx_env, ctx, goal, state, depth + 1, rw_steps);
        ctx.restore(mark_a);
        let Ok(pf_a) = res_a else { continue };

        let (hb_id, hb_fv) = env.mk_fresh_fvar(b);
        let mark_b = ctx.snapshot();
        ctx.push_fvar(hb_id, hb_fv, b);
        let res_b = prove_goal(env, idx_env, ctx, goal, state, depth + 1, rw_steps);
        ctx.restore(mark_b);
        let Ok(pf_b) = res_b else { continue };

        let abs_a = env.abstract_fvar(pf_a, ha_id).map_err(StepAbort::Kernel)?;
        let lam_a = env.mk_lam(a, abs_a);
        let abs_b = env.abstract_fvar(pf_b, hb_id).map_err(StepAbort::Kernel)?;
        let lam_b = env.mk_lam(b, abs_b);

        let (t_id, _) = env.mk_fresh_fvar(h.ty);
        let m_body = env.abstract_fvar(goal, t_id).map_err(StepAbort::Kernel)?;
        let motive = env.mk_lam(h.ty, m_body);
        let rec_c = env.mk_const(or_rec_n, &[]);
        state.record(ProofStep::OrElim);
        return Ok(Some(env.mk_apps(rec_c, &[a, b, motive, lam_a, lam_b, h.expr])));
    }
    Ok(None)
}

// =================================================================================================
// Stage 5a: Non-Invertible Logical Rules (`Or.inl`/`Or.inr`, `Exists.intro`)
// =================================================================================================

fn try_logical_non_invertible<'a>(
    env: &mut KernelEnv<'a>,
    idx_env: &EnvIndex<'a>,
    ctx: &mut LocalCtx<'a>,
    goal_whnf: ExprId,
    state: &mut SearchState<'a>,
    depth: u32,
    rw_steps: u32,
) -> Result<Option<ExprId>, StepAbort> {
    // Goal `Or A B`: seeded order between `inl` and `inr`, backtracking cleanly.
    if let Some((a, b)) = decompose_binary(env, idx_env.or_n, goal_whnf) {
        let tag = (u64::from(depth) << 32) ^ u64::from(state.tactic_steps) ^ 0x0B0B_1001;
        let try_right_first = SplitMix64::tie_break(state.config.seed, tag) & 1 == 1;
        let sides = if try_right_first { [false, true] } else { [true, false] };
        for is_left in sides {
            if is_left {
                if let Some(inl_n) = idx_env.or_inl_n
                    && let Ok(pa) = prove_goal(env, idx_env, ctx, a, state, depth + 1, rw_steps)
                {
                    let c = env.mk_const(inl_n, &[]);
                    state.record(ProofStep::OrLeft);
                    return Ok(Some(env.mk_apps(c, &[a, b, pa])));
                }
            } else if let Some(inr_n) = idx_env.or_inr_n
                && let Ok(pb) = prove_goal(env, idx_env, ctx, b, state, depth + 1, rw_steps)
            {
                let c = env.mk_const(inr_n, &[]);
                state.record(ProofStep::OrRight);
                return Ok(Some(env.mk_apps(c, &[a, b, pb])));
            }
        }
    }

    // Goal `Exists α P`: synthesize candidate witnesses from context + nullary constructors!
    if let Some((u, alpha, pred)) = decompose_exists(env, idx_env, goal_whnf)
        && let Some(ex_intro_n) = idx_env.exists_intro_n
    {
        let mut witnesses: AVec<'a, ExprId> = AVec::new_in(env.alloc());
        for h in ctx.hyps.as_slice() {
            if env.is_def_eq(h.ty, alpha).unwrap_or(false) && !witnesses.as_slice().contains(&h.expr)
            {
                witnesses.push(h.expr);
            }
        }
        // Add nullary constructors of `alpha` (e.g. `Nat.zero`, `Bool.true`, `Bool.false`, `True.intro`).
        let (alpha_head, alpha_args) = decompose_app(env, alpha);
        if alpha_args.is_empty()
            && let Expr::Const { name: ind_n, .. } = env.expr(alpha_head)
            && let Some(ctors) = env.inductive_ctors(ind_n)
        {
            let mut ctor_copy = AVec::new_in(env.alloc());
            for &c in ctors {
                ctor_copy.push(c);
            }
            for &c_name in ctor_copy.as_slice() {
                if let Some(DeclKind::Ctor { num_params: 0, num_fields: 0, .. }) =
                    env.decl_kind(c_name)
                {
                    let ce = env.mk_const(c_name, &[]);
                    if !witnesses.as_slice().contains(&ce) {
                        witnesses.push(ce);
                    }
                }
            }
        }
        // Deterministic seeded ordering of witnesses (arena-ID independent).
        if state.config.seed != 0 && witnesses.len() > 1 {
            let mut rng = SplitMix64::new(
                state.config.seed ^ (u64::from(depth) << 32) ^ u64::from(state.tactic_steps) ^ 0xE715_7500,
            );
            let slice = witnesses.as_mut_slice();
            for i in (1..slice.len()).rev() {
                let j = (rng.next_u64() as usize) % (i + 1);
                slice.swap(i, j);
            }
        }

        for &w in witnesses.as_slice() {
            state.tick()?;
            let pw_raw = env.mk_app(pred, w);
            let pw = env.whnf(pw_raw).unwrap_or(pw_raw);
            if let Ok(pf_pw) = prove_goal(env, idx_env, ctx, pw, state, depth + 1, rw_steps) {
                let c = env.mk_const(ex_intro_n, &[u]);
                state.record(ProofStep::ExistsIntro);
                return Ok(Some(env.mk_apps(c, &[alpha, pred, w, pf_pw])));
            }
        }
    }

    Ok(None)
}

// =================================================================================================
// Stage 5b: Equational Rewriting (`rw` with Local `∀` Hypotheses & Environment Theorems)
// =================================================================================================

#[derive(Clone, Copy)]
struct RewriteRule {
    source_name: Option<NameId>,
    expr: ExprId,
    ty: ExprId,
    is_local: bool,
}

fn try_equational_rewrite<'a>(
    env: &mut KernelEnv<'a>,
    idx_env: &EnvIndex<'a>,
    ctx: &mut LocalCtx<'a>,
    goal_whnf: ExprId,
    state: &mut SearchState<'a>,
    depth: u32,
    rw_steps: u32,
) -> Result<Option<ExprId>, StepAbort> {
    let Some((eq_u, eq_alpha, g_lhs, g_rhs)) = decompose_eq(env, idx_env, goal_whnf) else {
        return Ok(None);
    };

    // Collect candidate rewrite rules: local hypotheses first (including `ih`!), then env theorems.
    let mut rules: AVec<'a, RewriteRule> = AVec::new_in(env.alloc());
    for h in ctx.hyps.as_slice() {
        rules.push(RewriteRule {
            source_name: None,
            expr: h.expr,
            ty: h.ty,
            is_local: true,
        });
    }
    if state.config.enable_lemmas {
        for &lem in idx_env.lemmas.as_slice() {
            if !env.decl_level_params(lem).is_empty() {
                continue;
            }
            if let Some(ty) = env.decl_ty(lem) {
                rules.push(RewriteRule {
                    source_name: Some(lem),
                    expr: env.mk_const(lem, &[]),
                    ty,
                    is_local: false,
                });
            }
        }
    }

    // Score & deterministically sort rules (local rules first, tie-broken by seed).
    if state.config.seed != 0 && rules.len() > 1 {
        // Keep local hypotheses ahead of global lemmas, tie-breaking within each group.
        let mut local_end = 0usize;
        while local_end < rules.len() && rules[local_end].is_local {
            local_end += 1;
        }
        let mut rng = SplitMix64::new(
            state.config.seed ^ (u64::from(depth) << 32) ^ u64::from(rw_steps) ^ 0x5257_0000,
        );
        if local_end > 1 {
            let s = &mut rules.as_mut_slice()[..local_end];
            for i in (1..s.len()).rev() {
                let j = (rng.next_u64() as usize) % (i + 1);
                s.swap(i, j);
            }
        }
        if rules.len() > local_end + 1 {
            let s = &mut rules.as_mut_slice()[local_end..];
            for i in (1..s.len()).rev() {
                let j = (rng.next_u64() as usize) % (i + 1);
                s.swap(i, j);
            }
        }
    }

    // Collect all subterms of `g_lhs` and `g_rhs` (prefer original un-unfolded terms first so
    // theorems like `Nat.succ_add` match `Nat.add (succ k) a` before delta-unfolding!).
    let mut lhs_subs = AVec::new_in(env.alloc());
    let g_lhs_w = env.whnf(g_lhs).unwrap_or(g_lhs);
    collect_subterms(env, g_lhs, &mut lhs_subs, 0);
    if g_lhs != g_lhs_w {
        collect_subterms(env, g_lhs_w, &mut lhs_subs, 0);
    }

    let mut rhs_subs = AVec::new_in(env.alloc());
    let g_rhs_w = env.whnf(g_rhs).unwrap_or(g_rhs);
    collect_subterms(env, g_rhs, &mut rhs_subs, 0);
    if g_rhs != g_rhs_w {
        collect_subterms(env, g_rhs_w, &mut rhs_subs, 0);
    }

    let mut tried_targets: ABTreeSet<'a, ExprId> = ABTreeSet::new_in(env.alloc());

    // Two-pass ordered rewriting (Knuth-Bendix style):
    // Pass 0: strictly simplifying / forward rewrites across all rules (`lhs -> rhs`), with
    //         permutative rules (like `add x y = add y x`) restricted to strictly decreasing
    //         canonical order (`new_sub < sub`) to eliminate commutative ping-pong!
    // Pass 1: reverse rewrites (`rhs -> lhs`) only for non-permutative rules when pass 0 didn't close.
    for pass in 0u8..2 {
        let forward = pass == 0;
        for &rule in rules.as_slice() {
            state.tick()?;
            // Open quantified binders `∀ x1 ... xk, Eq α pat_l pat_r`.
            let Some(opened) = open_eq_rule(env, idx_env, rule.ty) else { continue };
            let permutative = is_permutative_rule(env, opened.lhs, opened.rhs, opened.mvar_ids.as_slice());
            if permutative && !forward {
                // Commutative/symmetric rules are already covered in the canonical forward pass.
                continue;
            }
            // In `Balanced` mode, skip permutative global lemmas before trying structural induction
            // if no local `ih` is in context yet (avoids spinning on `add_comm` before inducting!).
            if permutative && !rule.is_local && state.config.strategy != Strategy::EqRewriteFirst && rw_steps > 0 {
                continue;
            }

            for rewrite_on_lhs in [true, false] {
                let (orig_side, whnf_side, subs) = if rewrite_on_lhs {
                    (g_lhs, g_lhs_w, lhs_subs.as_slice())
                } else {
                    (g_rhs, g_rhs_w, rhs_subs.as_slice())
                };
                let (pat, repl_pat) = if forward {
                    (opened.lhs, opened.rhs)
                } else {
                    (opened.rhs, opened.lhs)
                };
                // Avoid matching bare variable pattern against everything (prevents pointless loops!).
                if let Expr::FVar { id, .. } = env.expr(pat)
                    && opened.mvar_ids.as_slice().contains(&id)
                {
                    continue;
                }

                for &sub in subs {
                    state.tick()?;
                    let mut assign = ABTreeMap::new_in(env.alloc());
                    if !match_pattern(env, pat, sub, opened.mvar_ids.as_slice(), &mut assign, 0) {
                        continue;
                    }
                    // Every non-Prop binder must be assigned (Prop premises can be proved as subgoals!).
                    let mut inst_args: AVec<'a, ExprId> = AVec::new_in(env.alloc());
                    let mut all_ok = true;
                    for &(mv_id, mv_ty) in opened.binders.as_slice() {
                        if let Some(&val) = assign.get(&mv_id) {
                            inst_args.push(val);
                        } else if let Some(pty) = subst_mvars(env, mv_ty, &assign, 0)
                            && let Ok(s) = env.infer_sort(pty)
                            && env.level_eq(s, env.level_zero())
                            && let Ok(prem_pf) =
                                prove_goal(env, idx_env, ctx, pty, state, depth + 1, rw_steps + 1)
                        {
                            assign.insert(mv_id, prem_pf);
                            inst_args.push(prem_pf);
                        } else {
                            all_ok = false;
                            break;
                        }
                    }
                    if !all_ok {
                        continue;
                    }

                    let Some(new_sub) = subst_mvars(env, repl_pat, &assign, 0) else { continue };
                    if env.is_def_eq(sub, new_sub).unwrap_or(false) {
                        continue;
                    }
                    // Ordered rewriting guard for permutative rules (e.g. commutativity `a + b = b + a`):
                    // only orient strictly downward by canonical structural comparison so cycles are impossible!
                    if permutative && cmp_expr_struct(env, new_sub, sub, 0) != core::cmp::Ordering::Less {
                        continue;
                    }

                    // Construct `sub_eq : sub = new_sub` using the instantiated rule.
                    let raw_rule_pf = env.mk_apps(rule.expr, inst_args.as_slice());
                    let sub_eq = if forward {
                        raw_rule_pf
                    } else {
                        let Some(sym) = mk_eq_symm(
                            env,
                            idx_env,
                            opened.eq_u,
                            opened.eq_alpha,
                            new_sub,
                            sub,
                            raw_rule_pf,
                        ) else {
                            continue;
                        };
                        sym
                    };

                    // Build context function `ctx_fn = fun (x : opened.eq_alpha) => base_side[x / sub]`
                    // and lift `sub_eq` to `side_eq : base_side = new_side`!
                    let (hole_id, hole_fv) = env.mk_fresh_fvar(opened.eq_alpha);
                    let (base_side, side_with_hole) =
                        if let Some((h, true)) = replace_first_subterm(env, orig_side, sub, hole_fv, 0)
                        {
                            (orig_side, h)
                        } else if let Some((h, true)) =
                            replace_first_subterm(env, whnf_side, sub, hole_fv, 0)
                        {
                            (whnf_side, h)
                        } else {
                            continue;
                        };
                    let Ok(abs_hole) = env.abstract_fvar(side_with_hole, hole_id) else {
                        continue;
                    };
                    let ctx_fn = env.mk_lam(opened.eq_alpha, abs_hole);
                    let Some((new_side, _)) = replace_first_subterm(env, base_side, sub, new_sub, 0)
                    else {
                        continue;
                    };

                    let Some(side_eq) = mk_congr_arg(
                        env,
                        idx_env,
                        opened.eq_u,
                        opened.eq_alpha,
                        ctx_fn,
                        sub,
                        new_sub,
                        sub_eq,
                    ) else {
                        continue;
                    };

                    // Build rewritten goal `new_goal` and recurse!
                    let Some(eq_n) = idx_env.eq_n else { continue };
                    let eq_c = env.mk_const(eq_n, &[eq_u]);
                    let new_goal = if rewrite_on_lhs {
                        env.mk_apps(eq_c, &[eq_alpha, new_side, g_rhs])
                    } else {
                        env.mk_apps(eq_c, &[eq_alpha, g_lhs, new_side])
                    };
                    if !tried_targets.insert(new_goal) {
                        continue;
                    }

                    if let Ok(rest_pf) =
                        prove_goal(env, idx_env, ctx, new_goal, state, depth + 1, rw_steps + 1)
                    {
                        let combined = if rewrite_on_lhs {
                            // `base_side = new_side` (side_eq) + `new_side = g_rhs` (rest_pf)
                            mk_eq_trans(
                                env, idx_env, eq_u, eq_alpha, base_side, new_side, g_rhs, side_eq,
                                rest_pf,
                            )
                        } else {
                            // `g_lhs = new_side` (rest_pf) + `new_side = base_side` (symm side_eq)
                            let Some(rev_side) = mk_eq_symm(
                                env, idx_env, eq_u, eq_alpha, base_side, new_side, side_eq,
                            ) else {
                                continue;
                            };
                            mk_eq_trans(
                                env, idx_env, eq_u, eq_alpha, g_lhs, new_side, base_side, rest_pf,
                                rev_side,
                            )
                        };
                        if let Some(pf) = combined {
                            state.record(match rule.source_name {
                                Some(n) => ProofStep::RewriteLemma(n),
                                None => ProofStep::RewriteLocal,
                            });
                            return Ok(Some(pf));
                        }
                    }
                }
            }
        }
    }

    Ok(None)
}

/// Detect permutative / commutative rules such as `add a b = add b a` (same head constant and
/// identical arity of pattern variables, just reordered).
fn is_permutative_rule(env: &KernelEnv<'_>, lhs: ExprId, rhs: ExprId, mvars: &[u32]) -> bool {
    let (lh, la) = decompose_app(env, lhs);
    let (rh, ra) = decompose_app(env, rhs);
    if lh != rh || la.len() != ra.len() || la.is_empty() {
        return false;
    }
    let all_mvars = |args: &[ExprId]| {
        args.iter().all(|&a| matches!(env.expr(a), Expr::FVar { id, .. } if mvars.contains(&id)))
    };
    all_mvars(la.as_slice()) && all_mvars(ra.as_slice())
}

/// Deterministic structural term ordering for Knuth-Bendix ordered rewriting (prevents cycles).
fn cmp_expr_struct(env: &KernelEnv<'_>, a: ExprId, b: ExprId, depth: u32) -> core::cmp::Ordering {
    use core::cmp::Ordering;
    if a == b || depth > 32 {
        return Ordering::Equal;
    }
    match (env.expr(a), env.expr(b)) {
        (Expr::FVar { id: x, .. }, Expr::FVar { id: y, .. }) => x.cmp(&y),
        (Expr::Const { name: x, .. }, Expr::Const { name: y, .. }) => x.cmp(&y),
        (Expr::App(f1, a1), Expr::App(f2, a2)) => {
            cmp_expr_struct(env, f1, f2, depth + 1).then_with(|| cmp_expr_struct(env, a1, a2, depth + 1))
        }
        (Expr::Lit(x), Expr::Lit(y)) => x.cmp(&y),
        _ => a.cmp(&b),
    }
}

struct OpenedEqRule<'a> {
    binders: AVec<'a, (u32, ExprId)>,
    mvar_ids: AVec<'a, u32>,
    eq_u: LevelId,
    eq_alpha: ExprId,
    lhs: ExprId,
    rhs: ExprId,
}

fn open_eq_rule<'a>(
    env: &mut KernelEnv<'a>,
    idx_env: &EnvIndex<'a>,
    mut ty: ExprId,
) -> Option<OpenedEqRule<'a>> {
    let mut binders = AVec::new_in(env.alloc());
    let mut mvar_ids = AVec::new_in(env.alloc());
    while binders.len() < 12 {
        let w = env.whnf(ty).ok()?;
        match env.expr(w) {
            Expr::ForallE { binder_ty, body } => {
                let (fv_id, fv) = env.mk_fresh_fvar(binder_ty);
                binders.push((fv_id, binder_ty));
                mvar_ids.push(fv_id);
                ty = env.instantiate(body, fv).ok()?;
            }
            _ => break,
        }
    }
    let (eq_u, eq_alpha, lhs, rhs) = decompose_eq(env, idx_env, ty)?;
    Some(OpenedEqRule { binders, mvar_ids, eq_u, eq_alpha, lhs, rhs })
}

// =================================================================================================
// Stage 5c: Seeded Best-First Backward Chaining (`apply`)
// =================================================================================================

struct BackwardCandidate<'a> {
    source_name: Option<NameId>,
    expr: ExprId,
    binders: AVec<'a, (u32, ExprId)>,
    assign: ABTreeMap<'a, u32, ExprId>,
    unassigned_premises: u32,
    tie_score: u64,
}

fn try_backward_chain<'a>(
    env: &mut KernelEnv<'a>,
    idx_env: &EnvIndex<'a>,
    ctx: &mut LocalCtx<'a>,
    goal_whnf: ExprId,
    state: &mut SearchState<'a>,
    depth: u32,
    rw_steps: u32,
) -> Result<Option<ExprId>, StepAbort> {
    let mut candidates: AVec<'a, BackwardCandidate<'a>> = AVec::new_in(env.alloc());

    // 1. Local quantified hypotheses (`h : ∀ ..., conclusion`).
    for (hyp_idx, h) in ctx.hyps.as_slice().iter().enumerate() {
        state.tick()?;
        collect_backward_candidates(
            env,
            ctx,
            None,
            hyp_idx as u64,
            h.expr,
            h.ty,
            goal_whnf,
            state.config.seed,
            &mut candidates,
        );
    }

    // 2. Environment theorems (and nullary/simple constructors of goal's inductive head).
    if state.config.enable_lemmas {
        for &lem in idx_env.lemmas.as_slice() {
            state.tick()?;
            if !env.decl_level_params(lem).is_empty() {
                continue;
            }
            if let Some(ty) = env.decl_ty(lem) {
                let ce = env.mk_const(lem, &[]);
                collect_backward_candidates(
                    env,
                    ctx,
                    Some(lem),
                    u64::from(lem.0) ^ 0xA5A5_5A5A,
                    ce,
                    ty,
                    goal_whnf,
                    state.config.seed,
                    &mut candidates,
                );
            }
        }
    }

    // Sort best-first by `(unassigned_premises, is_global, tie_score)`!
    candidates.as_mut_slice().sort_by_key(|c| {
        (c.unassigned_premises, c.source_name.is_some(), c.tie_score)
    });

    for cand in candidates.as_slice() {
        state.tick()?;
        let mut assign = ABTreeMap::new_in(env.alloc());
        for (&k, &v) in cand.assign.iter() {
            assign.insert(k, v);
        }
        let mut args = AVec::new_in(env.alloc());
        let mut ok = true;
        for &(mv_id, mv_ty) in cand.binders.as_slice() {
            if let Some(&val) = assign.get(&mv_id) {
                args.push(val);
            } else if let Some(prem_ty) = subst_mvars(env, mv_ty, &assign, 0) {
                if let Ok(prem_pf) =
                    prove_goal(env, idx_env, ctx, prem_ty, state, depth + 1, rw_steps)
                {
                    assign.insert(mv_id, prem_pf);
                    args.push(prem_pf);
                } else {
                    ok = false;
                    break;
                }
            } else {
                ok = false;
                break;
            }
        }
        if ok {
            let pf = env.mk_apps(cand.expr, args.as_slice());
            if verify_proof_term(env, pf, goal_whnf).unwrap_or(false) {
                state.record(match cand.source_name {
                    Some(n) => ProofStep::ApplyConst(n),
                    None => ProofStep::ApplyLocal,
                });
                return Ok(Some(pf));
            }
        }
    }

    Ok(None)
}

#[allow(clippy::too_many_arguments)]
fn collect_backward_candidates<'a>(
    env: &mut KernelEnv<'a>,
    ctx: &LocalCtx<'a>,
    source_name: Option<NameId>,
    tag: u64,
    expr: ExprId,
    mut ty: ExprId,
    goal_whnf: ExprId,
    seed: u64,
    out: &mut AVec<'a, BackwardCandidate<'a>>,
) {
    let mut binders = AVec::new_in(env.alloc());
    let mut mvar_ids = AVec::new_in(env.alloc());
    while binders.len() < 10 {
        let Ok(w) = env.whnf(ty) else { return };
        match env.expr(w) {
            Expr::ForallE { binder_ty, body } => {
                let (fv_id, fv) = env.mk_fresh_fvar(binder_ty);
                binders.push((fv_id, binder_ty));
                mvar_ids.push(fv_id);
                let Ok(next) = env.instantiate(body, fv) else { return };
                ty = next;
            }
            _ => break,
        }
    }
    if binders.is_empty() {
        return;
    }
    let mut base_assign = ABTreeMap::new_in(env.alloc());
    if !match_pattern(env, ty, goal_whnf, mvar_ids.as_slice(), &mut base_assign, 0) {
        return;
    }

    // Check whether any non-Prop pivot binder (like `y : Nat` in transitivity `R x y → R y z → R x z`)
    // remains unassigned after matching the conclusion.
    let mut has_unassigned_non_prop = false;
    let mut unassigned_props = 0u32;
    for &(mv_id, mv_ty) in binders.as_slice() {
        if !base_assign.contains_key(&mv_id) {
            let Some(pty) = subst_mvars(env, mv_ty, &base_assign, 0) else { return };
            let Ok(sort) = env.infer_sort(pty) else { return };
            if env.level_eq(sort, env.level_zero()) {
                unassigned_props += 1;
            } else {
                has_unassigned_non_prop = true;
            }
        }
    }

    if !has_unassigned_non_prop {
        out.push(BackwardCandidate {
            source_name,
            expr,
            binders,
            assign: base_assign,
            unassigned_premises: unassigned_props,
            tie_score: SplitMix64::tie_break(seed, tag),
        });
        return;
    }

    // Premise-guided pivot resolution: match any Prop premise against local hypotheses `ctx.hyps`
    // to bind existential pivot variables without blind guessing!
    for &(prem_mv_id, prem_ty) in binders.as_slice() {
        if base_assign.contains_key(&prem_mv_id) {
            continue;
        }
        let Some(prem_pat) = subst_mvars(env, prem_ty, &base_assign, 0) else { continue };
        let Ok(sort) = env.infer_sort(prem_pat) else { continue };
        if !env.level_eq(sort, env.level_zero()) {
            continue;
        }
        for (h_idx, h) in ctx.hyps.as_slice().iter().enumerate() {
            let mut assign = ABTreeMap::new_in(env.alloc());
            for (&k, &v) in base_assign.iter() {
                assign.insert(k, v);
            }
            if !match_pattern(env, prem_pat, h.ty, mvar_ids.as_slice(), &mut assign, 0) {
                continue;
            }
            assign.insert(prem_mv_id, h.expr);
            // Verify all remaining unassigned binders are Props.
            let mut still_non_prop = false;
            let mut rem_props = 0u32;
            for &(mv_id, mv_ty) in binders.as_slice() {
                if !assign.contains_key(&mv_id) {
                    let Some(pty) = subst_mvars(env, mv_ty, &assign, 0) else {
                        still_non_prop = true;
                        break;
                    };
                    let Ok(s) = env.infer_sort(pty) else {
                        still_non_prop = true;
                        break;
                    };
                    if env.level_eq(s, env.level_zero()) {
                        rem_props += 1;
                    } else {
                        still_non_prop = true;
                        break;
                    }
                }
            }
            if !still_non_prop {
                let mut binders_copy = AVec::new_in(env.alloc());
                for &b in binders.as_slice() {
                    binders_copy.push(b);
                }
                out.push(BackwardCandidate {
                    source_name,
                    expr,
                    binders: binders_copy,
                    assign,
                    unassigned_premises: rem_props,
                    tie_score: SplitMix64::tie_break(seed, tag ^ ((h_idx as u64) << 16)),
                });
            }
        }
    }
}

// =================================================================================================
// Stage 5d: Structural Case-Split (`Bool` / Finite Enums) & Induction (`Nat.rec`)
// =================================================================================================

fn try_cases_and_induction<'a>(
    env: &mut KernelEnv<'a>,
    idx_env: &EnvIndex<'a>,
    ctx: &mut LocalCtx<'a>,
    goal: ExprId,
    state: &mut SearchState<'a>,
    depth: u32,
    rw_steps: u32,
) -> Result<Option<ExprId>, StepAbort> {
    let Ok(g_sort) = env.infer_sort(goal) else { return Ok(None) };

    // Collect candidate inductive `FVar`s in `ctx` that actually occur in `goal`.
    let mut finite_vars: AVec<'a, (u32, ExprId, NameId, ExprId)> = AVec::new_in(env.alloc());
    let mut nat_vars: AVec<'a, (u32, ExprId, ExprId)> = AVec::new_in(env.alloc());

    for h in ctx.hyps.as_slice() {
        let Some(fv_id) = h.fvar_id else { continue };
        if !expr_contains_fvar(env, goal, fv_id, 0) {
            continue;
        }
        let Ok(ty_w) = env.whnf(h.ty) else { continue };
        let Expr::Const { name: ind_n, .. } = env.expr(ty_w) else { continue };
        if Some(ind_n) == idx_env.nat_n {
            nat_vars.push((fv_id, h.expr, ty_w));
        } else if let Some(DeclKind::Inductive { num_params: 0, num_ctors }) = env.decl_kind(ind_n)
            && (2..=8).contains(&num_ctors)
            && idx_env.recursors.contains_key(&ind_n)
        {
            // Check every constructor is nullary (pure finite enum like `Bool`).
            let all_nullary = env.inductive_ctors(ind_n).is_some_and(|ctors| {
                ctors.iter().all(|&c| {
                    matches!(
                        env.decl_kind(c),
                        Some(DeclKind::Ctor { num_params: 0, num_fields: 0, .. })
                    )
                })
            });
            if all_nullary {
                finite_vars.push((fv_id, h.expr, ind_n, ty_w));
            }
        }
    }

    // 1. Finite inductive case-splitting (`Bool`, enums) — always terminating!
    for &(fv_id, fv_expr, ind_n, ind_ty) in finite_vars.as_slice() {
        state.tick()?;
        if let Some(pf) = try_finite_enum_cases(
            env, idx_env, ctx, goal, g_sort, fv_id, fv_expr, ind_n, ind_ty, state, depth, rw_steps,
        )? {
            return Ok(Some(pf));
        }
    }

    // 2. Structural induction on `Nat` (`Nat.rec`) — try candidates in heuristic / seeded order!
    if !nat_vars.is_empty()
        && idx_env.nat_rec_n.is_some()
        && idx_env.nat_zero_n.is_some()
        && idx_env.nat_succ_n.is_some()
    {
        // Order candidates: prefer variables appearing in recursive (first-arg) position of `Nat.*`
        // applications, with deterministic seeded tie-breaking.
        let mut scored: AVec<'a, (u32, u64, usize, u32, ExprId, ExprId)> = AVec::new_in(env.alloc());
        for (pos, &(fv_id, fv_expr, ind_ty)) in nat_vars.as_slice().iter().enumerate() {
            let rec_score = score_nat_induction_var(env, goal, fv_id);
            let tie = SplitMix64::tie_break(state.config.seed, (pos as u64) ^ 0x1D0C_7104);
            scored.push((rec_score, tie, pos, fv_id, fv_expr, ind_ty));
        }
        scored.as_mut_slice().sort_by_key(|&(score, tie, pos, _, _, _)| (score, tie, pos));

        for &(_, _, _, fv_id, fv_expr, nat_ty) in scored.as_slice() {
            state.tick()?;
            // First try direct induction on `fv_id`.
            if let Some(pf) = try_nat_induction(
                env, idx_env, ctx, goal, g_sort, fv_id, fv_expr, nat_ty, state, depth, rw_steps,
            )? {
                return Ok(Some(pf));
            }
            // If direct induction failed and there are other `Nat` variables in `ctx`, try
            // generalizing one other `Nat` variable so the IH is universally quantified!
            for &(_, _, _, other_id, other_expr, other_ty) in scored.as_slice() {
                if other_id == fv_id {
                    continue;
                }
                state.tick()?;
                if let Some(pf) = try_generalized_nat_induction(
                    env,
                    idx_env,
                    ctx,
                    goal,
                    g_sort,
                    fv_id,
                    fv_expr,
                    nat_ty,
                    other_id,
                    other_expr,
                    other_ty,
                    state,
                    depth,
                    rw_steps,
                )? {
                    return Ok(Some(pf));
                }
            }
        }
    }

    Ok(None)
}

/// Lower score = better induction variable (prefers variables in the recursive argument position
/// of function calls, weighting top-level / repeated recursive occurrences highest!).
fn score_nat_induction_var(env: &KernelEnv<'_>, goal: ExprId, fv_id: u32) -> u32 {
    let mut subs = AVec::new_in(env.alloc());
    collect_subterms(env, goal, &mut subs, 0);
    let mut last_arg_hits = 0u32;
    let mut first_arg_hits = 0u32;
    for &s in subs.as_slice() {
        let (head, args) = decompose_app(env, s);
        if let Expr::Const { name, .. } = env.expr(head)
            && matches!(env.decl_kind(name), Some(DeclKind::Def { is_theorem: false }))
            && !args.is_empty()
        {
            if matches!(env.expr(args[args.len() - 1]), Expr::FVar { id, .. } if id == fv_id) {
                last_arg_hits = last_arg_hits.saturating_add(1);
            } else if matches!(env.expr(args[0]), Expr::FVar { id, .. } if id == fv_id) {
                first_arg_hits = first_arg_hits.saturating_add(1);
            }
        }
    }
    if last_arg_hits > 0 {
        10u32.saturating_sub(last_arg_hits.min(10))
    } else if first_arg_hits > 0 {
        20u32.saturating_sub(first_arg_hits.min(10))
    } else {
        30
    }
}

#[allow(clippy::too_many_arguments)]
fn try_finite_enum_cases<'a>(
    env: &mut KernelEnv<'a>,
    idx_env: &EnvIndex<'a>,
    ctx: &mut LocalCtx<'a>,
    goal: ExprId,
    g_sort: LevelId,
    fv_id: u32,
    fv_expr: ExprId,
    ind_n: NameId,
    ind_ty: ExprId,
    state: &mut SearchState<'a>,
    depth: u32,
    rw_steps: u32,
) -> Result<Option<ExprId>, StepAbort> {
    let Some(&rec_n) = idx_env.recursors.get(&ind_n) else { return Ok(None) };
    let ctors_copy = {
        let Some(ctors) = env.inductive_ctors(ind_n) else { return Ok(None) };
        let mut v = AVec::new_in(env.alloc());
        for &c in ctors {
            v.push(c);
        }
        v
    };

    // Build `motive = fun (x : ind_ty) => goal[x / fv_id]`.
    let abs = env.abstract_fvar(goal, fv_id).map_err(StepAbort::Kernel)?;
    let motive = env.mk_lam(ind_ty, abs);

    let mut rec_args = AVec::new_in(env.alloc());
    rec_args.push(motive);

    for &ctor_n in ctors_copy.as_slice() {
        let ctor_e = env.mk_const(ctor_n, &[]);
        let case_goal_raw = env.instantiate(abs, ctor_e).map_err(StepAbort::Kernel)?;
        let case_goal = env.whnf(case_goal_raw).unwrap_or(case_goal_raw);

        // Also substitute `fv_id -> ctor_e` in any context hypotheses mentioning `fv_id`?
        // Simpler & sound: add equality `fv = ctor`? No, `fv` is already substituted out of `case_goal`!
        let Ok(case_pf) = prove_goal(env, idx_env, ctx, case_goal, state, depth + 1, rw_steps)
        else {
            return Ok(None);
        };
        rec_args.push(case_pf);
    }

    rec_args.push(fv_expr);
    let rec_lps = env.decl_level_params(rec_n);
    let rec_c = if rec_lps.len() == 1 {
        env.mk_const(rec_n, &[g_sort])
    } else if rec_lps.is_empty() {
        env.mk_const(rec_n, &[])
    } else {
        return Ok(None);
    };
    let pf = env.mk_apps(rec_c, rec_args.as_slice());
    if verify_proof_term(env, pf, goal).unwrap_or(false) {
        state.record(ProofStep::Cases(ind_n));
        return Ok(Some(pf));
    }
    Ok(None)
}

#[allow(clippy::too_many_arguments)]
fn try_nat_induction<'a>(
    env: &mut KernelEnv<'a>,
    idx_env: &EnvIndex<'a>,
    ctx: &mut LocalCtx<'a>,
    goal: ExprId,
    g_sort: LevelId,
    fv_id: u32,
    fv_expr: ExprId,
    nat_ty: ExprId,
    state: &mut SearchState<'a>,
    depth: u32,
    rw_steps: u32,
) -> Result<Option<ExprId>, StepAbort> {
    let Some(nat_rec_n) = idx_env.nat_rec_n else { return Ok(None) };
    let Some(zero_n) = idx_env.nat_zero_n else { return Ok(None) };
    let Some(succ_n) = idx_env.nat_succ_n else { return Ok(None) };

    let zero_c = env.mk_const(zero_n, &[]);
    let succ_c = env.mk_const(succ_n, &[]);

    // `motive = fun (n : Nat) => goal[n / fv_id]`
    let abs = env.abstract_fvar(goal, fv_id).map_err(StepAbort::Kernel)?;
    let motive = env.mk_lam(nat_ty, abs);

    // Base case: `motive Nat.zero`
    let base_goal = env.instantiate(abs, zero_c).map_err(StepAbort::Kernel)?;
    let Ok(base_pf) = prove_goal(env, idx_env, ctx, base_goal, state, depth + 1, rw_steps) else {
        return Ok(None);
    };

    // Inductive step: `∀ (k : Nat) (ih : motive k), motive (Nat.succ k)`
    let (k_id, k_fv) = env.mk_fresh_fvar(nat_ty);
    let ih_ty = env.instantiate(abs, k_fv).map_err(StepAbort::Kernel)?;
    let (ih_id, ih_fv) = env.mk_fresh_fvar(ih_ty);
    let succ_k = env.mk_app(succ_c, k_fv);
    let step_goal = env.instantiate(abs, succ_k).map_err(StepAbort::Kernel)?;

    let mark = ctx.snapshot();
    ctx.push_fvar(k_id, k_fv, nat_ty);
    ctx.push_fvar(ih_id, ih_fv, ih_ty);
    let step_res = prove_goal(env, idx_env, ctx, step_goal, state, depth + 1, rw_steps);
    ctx.restore(mark);
    let Ok(step_body) = step_res else {
        return Ok(None);
    };

    let abs_ih = env.abstract_fvar(step_body, ih_id).map_err(StepAbort::Kernel)?;
    let lam_ih = env.mk_lam(ih_ty, abs_ih);
    let abs_k = env.abstract_fvar(lam_ih, k_id).map_err(StepAbort::Kernel)?;
    let step_pf = env.mk_lam(nat_ty, abs_k);

    let rec_c = env.mk_const(nat_rec_n, &[g_sort]);
    let pf = env.mk_apps(rec_c, &[motive, base_pf, step_pf, fv_expr]);
    if verify_proof_term(env, pf, goal).unwrap_or(false) {
        state.record(ProofStep::Induction(idx_env.nat_n.unwrap_or(nat_rec_n)));
        return Ok(Some(pf));
    }
    Ok(None)
}

/// Generalized induction: abstracts `other_id` into `∀ other, goal` before inducting on `fv_id`,
/// then applies the resulting proof to `other_expr`!
#[allow(clippy::too_many_arguments)]
fn try_generalized_nat_induction<'a>(
    env: &mut KernelEnv<'a>,
    idx_env: &EnvIndex<'a>,
    ctx: &mut LocalCtx<'a>,
    goal: ExprId,
    g_sort: LevelId,
    fv_id: u32,
    fv_expr: ExprId,
    nat_ty: ExprId,
    other_id: u32,
    other_expr: ExprId,
    other_ty: ExprId,
    state: &mut SearchState<'a>,
    depth: u32,
    rw_steps: u32,
) -> Result<Option<ExprId>, StepAbort> {
    let abs_other = env.abstract_fvar(goal, other_id).map_err(StepAbort::Kernel)?;
    let gen_goal = env.mk_forall(other_ty, abs_other);
    let Some(gen_pf) = try_nat_induction(
        env, idx_env, ctx, gen_goal, g_sort, fv_id, fv_expr, nat_ty, state, depth + 1, rw_steps,
    )?
    else {
        return Ok(None);
    };
    let pf = env.mk_app(gen_pf, other_expr);
    if verify_proof_term(env, pf, goal).unwrap_or(false) {
        return Ok(Some(pf));
    }
    Ok(None)
}

// =================================================================================================
// Comprehensive Test Suite
// =================================================================================================

#[cfg(test)]
mod tests {
    use blaze_own::testing::heap;

    use super::*;
    use crate::lean_kernel::{DEFAULT_ALLOWED_AXIOMS, RecRule};

    fn alloc() -> Alloc<'static> {
        heap().alloc()
    }

    fn avec<T: Copy>(items: &[T]) -> AVec<'static, T> {
        let mut v = AVec::new_in(alloc());
        for &x in items {
            v.push(x);
        }
        v
    }

    /// Build a rich Lean 4 standard logic + Bool + Nat environment (ZERO axioms!).
    fn mk_std_env() -> KernelEnv<'static> {
        let mut env = KernelEnv::new_in(alloc());
        let u_name = env.intern_name("u");
        let v_name = env.intern_name("v");
        let u_lvl = env.mk_level_param(u_name);
        let v_lvl = env.mk_level_param(v_name);
        let prop = env.mk_prop();
        let type0 = env.mk_type0();
        let sort_u = env.mk_sort(u_lvl);
        let sort_v = env.mk_sort(v_lvl);

        // --- True, True.intro ---
        let true_n = env.intern_name("True");
        let true_intro_n = env.intern_name("True.intro");
        let true_c = env.mk_const(true_n, &[]);
        env.check_decl(Decl::Inductive {
            name: true_n,
            level_params: avec(&[]),
            num_params: 0,
            ty: prop,
            ctors: avec(&[true_intro_n]),
        })
        .unwrap();
        env.check_decl(Decl::Ctor {
            name: true_intro_n,
            ind_name: true_n,
            ctor_idx: 0,
            num_params: 0,
            num_fields: 0,
            level_params: avec(&[]),
            ty: true_c,
        })
        .unwrap();

        // --- False, False.rec ---
        let false_n = env.intern_name("False");
        let false_rec_n = env.intern_name("False.rec");
        let false_c = env.mk_const(false_n, &[]);
        env.check_decl(Decl::Inductive {
            name: false_n,
            level_params: avec(&[]),
            num_params: 0,
            ty: prop,
            ctors: avec(&[]),
        })
        .unwrap();
        let false_motive_ty = env.mk_arrow(false_c, sort_u).unwrap();
        let b0 = env.mk_bvar(0);
        let b1 = env.mk_bvar(1);
        let b2 = env.mk_bvar(2);
        let b3 = env.mk_bvar(3);
        let b4 = env.mk_bvar(4);
        let m_t = env.mk_app(b1, b0);
        let t_to_mt = env.mk_forall(false_c, m_t);
        let false_rec_ty = env.mk_forall(false_motive_ty, t_to_mt);
        env.check_decl(Decl::Recursor {
            name: false_rec_n,
            ind_name: false_n,
            num_params: 0,
            num_indices: 0,
            num_motives: 1,
            num_minors: 0,
            level_params: avec(&[u_name]),
            ty: false_rec_ty,
            rules: avec(&[]),
        })
        .unwrap();

        // --- Not : Prop → Prop := fun p => p → False ---
        let not_n = env.intern_name("Not");
        let not_ty = env.mk_arrow(prop, prop).unwrap();
        let p_to_false = env.mk_arrow(b0, false_c).unwrap();
        let not_val = env.mk_lam(prop, p_to_false);
        env.check_decl(Decl::Def {
            name: not_n,
            level_params: avec(&[]),
            ty: not_ty,
            value: not_val,
            is_theorem: false,
        })
        .unwrap();

        // --- And : Prop → Prop → Prop ---
        let and_n = env.intern_name("And");
        let and_intro_n = env.intern_name("And.intro");
        let and_left_n = env.intern_name("And.left");
        let and_right_n = env.intern_name("And.right");
        let and_rec_n = env.intern_name("And.rec");
        let and_c = env.mk_const(and_n, &[]);
        let p_p_p = {
            let p_p = env.mk_arrow(prop, prop).unwrap();
            env.mk_arrow(prop, p_p).unwrap()
        };
        env.check_decl(Decl::Inductive {
            name: and_n,
            level_params: avec(&[]),
            num_params: 2,
            ty: p_p_p,
            ctors: avec(&[and_intro_n]),
        })
        .unwrap();
        // And.intro : ∀ (a b : Prop), a → b → And a b
        let and_intro_ty = {
            let and_ab = env.mk_apps(and_c, &[b3, b2]);
            let t3 = env.mk_forall(b1, and_ab);
            let t2 = env.mk_forall(b1, t3);
            let t1 = env.mk_forall(prop, t2);
            env.mk_forall(prop, t1)
        };
        env.check_decl(Decl::Ctor {
            name: and_intro_n,
            ind_name: and_n,
            ctor_idx: 0,
            num_params: 2,
            num_fields: 2,
            level_params: avec(&[]),
            ty: and_intro_ty,
        })
        .unwrap();
        // And.rec.{u} : ∀ (a b : Prop) (motive : And a b → Sort u)
        //   (intro : ∀ (ha : a) (hb : b), motive (And.intro a b ha hb)) (t : And a b), motive t
        let and_intro_c = env.mk_const(and_intro_n, &[]);
        let and_rec_ty = {
            // Under (a=b1, b=b0): motive_ty = And a b → Sort u
            let and_ab_m = env.mk_apps(and_c, &[b1, b0]);
            let motive_ty = env.mk_arrow(and_ab_m, sort_u).unwrap();
            // Under (a=b2, b=b1, motive=b0):
            //   intro_ty = ∀ (ha : a [b2]) (hb : b [b2]), motive [b2] (And.intro a [b4] b [b3] ha [b1] hb [b0])
            let app_intro = env.mk_apps(and_intro_c, &[b4, b3, b1, b0]);
            let m_intro = env.mk_app(b2, app_intro);
            let intro_inner = env.mk_forall(b2, m_intro);
            let intro_ty = env.mk_forall(b2, intro_inner);
            // Under (a=b3, b=b2, motive=b1, intro=b0): ∀ (t : And a b), motive [b2] t [b0]
            let and_ab_t = env.mk_apps(and_c, &[b3, b2]);
            let m_t = env.mk_app(b2, b0);
            let t_part = env.mk_forall(and_ab_t, m_t);
            let s3 = env.mk_forall(intro_ty, t_part);
            let s2 = env.mk_forall(motive_ty, s3);
            let s1 = env.mk_forall(prop, s2);
            env.mk_forall(prop, s1)
        };
        // Rule for And.intro: λ a b motive intro ha hb => intro ha hb
        let and_rec_rhs = {
            let and_ab_m = env.mk_apps(and_c, &[b1, b0]);
            let motive_ty = env.mk_arrow(and_ab_m, sort_u).unwrap();
            let app_intro = env.mk_apps(and_intro_c, &[b4, b3, b1, b0]);
            let m_intro = env.mk_app(b2, app_intro);
            let intro_inner = env.mk_forall(b2, m_intro);
            let intro_ty = env.mk_forall(b2, intro_inner);
            // Under (a=b5, b=b4, motive=b3, intro=b2, ha=b1, hb=b0): intro ha hb
            let body = env.mk_apps(b2, &[b1, b0]);
            let l5 = env.mk_lam(b3, body); // hb : b (a=b4, b=b3, motive=b2, intro=b1, ha=b0)
            let l4 = env.mk_lam(b3, l5); // ha : a (a=b3, b=b2, motive=b1, intro=b0)
            let l3 = env.mk_lam(intro_ty, l4);
            let l2 = env.mk_lam(motive_ty, l3);
            let l1 = env.mk_lam(prop, l2);
            env.mk_lam(prop, l1)
        };
        env.check_decl(Decl::Recursor {
            name: and_rec_n,
            ind_name: and_n,
            num_params: 2,
            num_indices: 0,
            num_motives: 1,
            num_minors: 1,
            level_params: avec(&[u_name]),
            ty: and_rec_ty,
            rules: avec(&[RecRule { ctor_name: and_intro_n, nfields: 2, rhs: and_rec_rhs }]),
        })
        .unwrap();

        // And.left / And.right via Proj!
        let and_left_ty = {
            let and_ab = env.mk_apps(and_c, &[b1, b0]);
            let r = env.mk_forall(and_ab, b2);
            let s1 = env.mk_forall(prop, r);
            env.mk_forall(prop, s1)
        };
        let and_left_val = {
            let and_ab = env.mk_apps(and_c, &[b1, b0]);
            let p = env.mk_proj(and_n, 0, b0);
            let l2 = env.mk_lam(and_ab, p);
            let l1 = env.mk_lam(prop, l2);
            env.mk_lam(prop, l1)
        };
        env.check_decl(Decl::Def {
            name: and_left_n,
            level_params: avec(&[]),
            ty: and_left_ty,
            value: and_left_val,
            is_theorem: false,
        })
        .unwrap();

        let and_right_ty = {
            let and_ab = env.mk_apps(and_c, &[b1, b0]);
            let r = env.mk_forall(and_ab, b1);
            let s1 = env.mk_forall(prop, r);
            env.mk_forall(prop, s1)
        };
        let and_right_val = {
            let and_ab = env.mk_apps(and_c, &[b1, b0]);
            let p = env.mk_proj(and_n, 1, b0);
            let l2 = env.mk_lam(and_ab, p);
            let l1 = env.mk_lam(prop, l2);
            env.mk_lam(prop, l1)
        };
        env.check_decl(Decl::Def {
            name: and_right_n,
            level_params: avec(&[]),
            ty: and_right_ty,
            value: and_right_val,
            is_theorem: false,
        })
        .unwrap();

        // --- Or : Prop → Prop → Prop ---
        let or_n = env.intern_name("Or");
        let or_inl_n = env.intern_name("Or.inl");
        let or_inr_n = env.intern_name("Or.inr");
        let or_rec_n = env.intern_name("Or.rec");
        let or_c = env.mk_const(or_n, &[]);
        env.check_decl(Decl::Inductive {
            name: or_n,
            level_params: avec(&[]),
            num_params: 2,
            ty: p_p_p,
            ctors: avec(&[or_inl_n, or_inr_n]),
        })
        .unwrap();
        let or_inl_ty = {
            let or_ab = env.mk_apps(or_c, &[b2, b1]);
            let t2 = env.mk_forall(b1, or_ab);
            let t1 = env.mk_forall(prop, t2);
            env.mk_forall(prop, t1)
        };
        let or_inr_ty = {
            let or_ab = env.mk_apps(or_c, &[b2, b1]);
            let t2 = env.mk_forall(b0, or_ab);
            let t1 = env.mk_forall(prop, t2);
            env.mk_forall(prop, t1)
        };
        env.check_decl(Decl::Ctor {
            name: or_inl_n,
            ind_name: or_n,
            ctor_idx: 0,
            num_params: 2,
            num_fields: 1,
            level_params: avec(&[]),
            ty: or_inl_ty,
        })
        .unwrap();
        env.check_decl(Decl::Ctor {
            name: or_inr_n,
            ind_name: or_n,
            ctor_idx: 1,
            num_params: 2,
            num_fields: 1,
            level_params: avec(&[]),
            ty: or_inr_ty,
        })
        .unwrap();
        // Or.rec : ∀ (a b : Prop) (motive : Or a b → Prop)
        //   (inl : ∀ h : a, motive (Or.inl a b h)) (inr : ∀ h : b, motive (Or.inr a b h))
        //   (t : Or a b), motive t
        let or_inl_c = env.mk_const(or_inl_n, &[]);
        let or_inr_c = env.mk_const(or_inr_n, &[]);
        let or_rec_ty = {
            let or_ab_m = env.mk_apps(or_c, &[b1, b0]);
            let motive_ty = env.mk_arrow(or_ab_m, prop).unwrap();
            // inl under (a=b2, b=b1, motive=b0): ∀ (h : a [b2]), motive [b1] (Or.inl a [b3] b [b2] h [b0])
            let inl_app = env.mk_apps(or_inl_c, &[b3, b2, b0]);
            let m_inl = env.mk_app(b1, inl_app);
            let inl_ty = env.mk_forall(b2, m_inl);
            // inr under (a=b3, b=b2, motive=b1, inl=b0): ∀ (h : b [b2]), motive [b2] (Or.inr a [b4] b [b3] h [b0])
            let inr_app = env.mk_apps(or_inr_c, &[b4, b3, b0]);
            let m_inr = env.mk_app(b2, inr_app);
            let inr_ty = env.mk_forall(b2, m_inr);
            // t under (a=b4, b=b3, motive=b2, inl=b1, inr=b0): ∀ (t : Or a b), motive [b3] t [b0]
            let or_ab_t = env.mk_apps(or_c, &[b4, b3]);
            let m_t = env.mk_app(b3, b0);
            let t_ty = env.mk_forall(or_ab_t, m_t);
            let s4 = env.mk_forall(inr_ty, t_ty);
            let s3 = env.mk_forall(inl_ty, s4);
            let s2 = env.mk_forall(motive_ty, s3);
            let s1 = env.mk_forall(prop, s2);
            env.mk_forall(prop, s1)
        };
        env.check_decl(Decl::Recursor {
            name: or_rec_n,
            ind_name: or_n,
            num_params: 2,
            num_indices: 0,
            num_motives: 1,
            num_minors: 2,
            level_params: avec(&[]),
            ty: or_rec_ty,
            rules: avec(&[]),
        })
        .unwrap();

        // --- Exists.{u} : ∀ (α : Sort u), (α → Prop) → Prop ---
        let ex_n = env.intern_name("Exists");
        let ex_intro_n = env.intern_name("Exists.intro");
        let ex_rec_n = env.intern_name("Exists.rec");
        let ex_u = env.mk_const(ex_n, &[u_lvl]);
        let ex_ty = {
            let pred_ty = env.mk_arrow(b0, prop).unwrap();
            let inner = env.mk_forall(pred_ty, prop);
            env.mk_forall(sort_u, inner)
        };
        env.check_decl(Decl::Inductive {
            name: ex_n,
            level_params: avec(&[u_name]),
            num_params: 2,
            ty: ex_ty,
            ctors: avec(&[ex_intro_n]),
        })
        .unwrap();
        // Exists.intro.{u} : ∀ (α : Sort u) (p : α → Prop) (w : α) (h : p w), Exists.{u} α p
        let ex_intro_ty = {
            let pred_ty = env.mk_arrow(b0, prop).unwrap();
            // Under (α=b2, p=b1, w=b0): h_ty = p w
            let pw = env.mk_app(b1, b0);
            let ex_ap = env.mk_apps(ex_u, &[b3, b2]);
            let t3 = env.mk_forall(pw, ex_ap);
            let t2 = env.mk_forall(b1, t3); // w : α
            let t1 = env.mk_forall(pred_ty, t2);
            env.mk_forall(sort_u, t1)
        };
        env.check_decl(Decl::Ctor {
            name: ex_intro_n,
            ind_name: ex_n,
            ctor_idx: 0,
            num_params: 2,
            num_fields: 2,
            level_params: avec(&[u_name]),
            ty: ex_intro_ty,
        })
        .unwrap();
        // Exists.rec.{u} : ∀ (α : Sort u) (p : α → Prop) (motive : Exists α p → Prop)
        //   (intro : ∀ (w : α) (h : p w), motive (Exists.intro α p w h)) (t : Exists α p), motive t
        let ex_intro_u = env.mk_const(ex_intro_n, &[u_lvl]);
        let ex_rec_ty = {
            let pred_ty = env.mk_arrow(b0, prop).unwrap();
            let ex_ap_m = env.mk_apps(ex_u, &[b1, b0]);
            let motive_ty = env.mk_arrow(ex_ap_m, prop).unwrap();
            // Under (α=b2, p=b1, motive=b0):
            //   ∀ (w : α [b2]) (h : p [b2] w [b0]), motive [b2] (Exists.intro α [b4] p [b3] w [b1] h [b0])
            let pw = env.mk_app(b2, b0);
            let intro_app = env.mk_apps(ex_intro_u, &[b4, b3, b1, b0]);
            let m_intro = env.mk_app(b2, intro_app);
            let intro_inner = env.mk_forall(pw, m_intro);
            let intro_ty = env.mk_forall(b2, intro_inner);
            let ex_ap_t = env.mk_apps(ex_u, &[b3, b2]);
            let m_t = env.mk_app(b2, b0);
            let t_ty = env.mk_forall(ex_ap_t, m_t);
            let s3 = env.mk_forall(intro_ty, t_ty);
            let s2 = env.mk_forall(motive_ty, s3);
            let s1 = env.mk_forall(pred_ty, s2);
            env.mk_forall(sort_u, s1)
        };
        env.check_decl(Decl::Recursor {
            name: ex_rec_n,
            ind_name: ex_n,
            num_params: 2,
            num_indices: 0,
            num_motives: 1,
            num_minors: 1,
            level_params: avec(&[u_name]),
            ty: ex_rec_ty,
            rules: avec(&[]),
        })
        .unwrap();

        // --- Eq.{u}, Eq.refl.{u}, Eq.rec.{v, u} ---
        let eq_n = env.intern_name("Eq");
        let eq_refl_n = env.intern_name("Eq.refl");
        let eq_rec_n = env.intern_name("Eq.rec");
        let eq_ty = {
            let t2 = env.mk_forall(b1, prop);
            let t1 = env.mk_forall(b0, t2);
            env.mk_forall(sort_u, t1)
        };
        env.check_decl(Decl::Inductive {
            name: eq_n,
            level_params: avec(&[u_name]),
            num_params: 2,
            ty: eq_ty,
            ctors: avec(&[eq_refl_n]),
        })
        .unwrap();
        let eq_u = env.mk_const(eq_n, &[u_lvl]);
        let eq_refl_ty = {
            let eq_aa = env.mk_apps(eq_u, &[b1, b0, b0]);
            let t1 = env.mk_forall(b0, eq_aa);
            env.mk_forall(sort_u, t1)
        };
        env.check_decl(Decl::Ctor {
            name: eq_refl_n,
            ind_name: eq_n,
            ctor_idx: 0,
            num_params: 2,
            num_fields: 0,
            level_params: avec(&[u_name]),
            ty: eq_refl_ty,
        })
        .unwrap();
        let eq_refl_u = env.mk_const(eq_refl_n, &[u_lvl]);
        let eq_a_x = env.mk_apps(eq_u, &[b2, b1, b0]);
        let eq_motive_inner = env.mk_forall(eq_a_x, sort_v);
        let eq_motive_ty = env.mk_forall(b1, eq_motive_inner);
        let refl_a = env.mk_apps(eq_refl_u, &[b2, b1]);
        let eq_minor_ty = env.mk_apps(b0, &[b1, refl_a]);
        let eq_h_ty = env.mk_apps(eq_u, &[b4, b3, b0]);
        let eq_ret_ty = env.mk_apps(b3, &[b1, b0]);
        let eq_rec_ty = {
            let t5 = env.mk_forall(eq_h_ty, eq_ret_ty);
            let t4 = env.mk_forall(b3, t5);
            let t3 = env.mk_forall(eq_minor_ty, t4);
            let t2 = env.mk_forall(eq_motive_ty, t3);
            let t1 = env.mk_forall(b0, t2);
            env.mk_forall(sort_u, t1)
        };
        let eq_rec_rhs = {
            let l3 = env.mk_lam(eq_minor_ty, b0);
            let l2 = env.mk_lam(eq_motive_ty, l3);
            let l1 = env.mk_lam(b0, l2);
            env.mk_lam(sort_u, l1)
        };
        env.check_decl(Decl::Recursor {
            name: eq_rec_n,
            ind_name: eq_n,
            num_params: 2,
            num_indices: 1,
            num_motives: 1,
            num_minors: 1,
            level_params: avec(&[v_name, u_name]),
            ty: eq_rec_ty,
            rules: avec(&[RecRule { ctor_name: eq_refl_n, nfields: 0, rhs: eq_rec_rhs }]),
        })
        .unwrap();

        // --- Bool, Bool.false, Bool.true, Bool.rec, Bool.not, Bool.and, Bool.or ---
        let bool_n = env.intern_name("Bool");
        let bf_n = env.intern_name("Bool.false");
        let bt_n = env.intern_name("Bool.true");
        let bool_rec_n = env.intern_name("Bool.rec");
        let bool_c = env.mk_const(bool_n, &[]);
        let bf_c = env.mk_const(bf_n, &[]);
        let bt_c = env.mk_const(bt_n, &[]);
        env.check_decl(Decl::Inductive {
            name: bool_n,
            level_params: avec(&[]),
            num_params: 0,
            ty: type0,
            ctors: avec(&[bf_n, bt_n]),
        })
        .unwrap();
        env.check_decl(Decl::Ctor {
            name: bf_n,
            ind_name: bool_n,
            ctor_idx: 0,
            num_params: 0,
            num_fields: 0,
            level_params: avec(&[]),
            ty: bool_c,
        })
        .unwrap();
        env.check_decl(Decl::Ctor {
            name: bt_n,
            ind_name: bool_n,
            ctor_idx: 1,
            num_params: 0,
            num_fields: 0,
            level_params: avec(&[]),
            ty: bool_c,
        })
        .unwrap();
        let bm_ty = env.mk_arrow(bool_c, sort_u).unwrap();
        let bf_case = env.mk_app(b0, bf_c);
        let bt_case = env.mk_app(b1, bt_c);
        let b_ret = env.mk_app(b3, b0);
        let b_rec_ty = {
            let t3 = env.mk_forall(bool_c, b_ret);
            let t2 = env.mk_forall(bt_case, t3);
            let t1 = env.mk_forall(bf_case, t2);
            env.mk_forall(bm_ty, t1)
        };
        let bf_rhs = {
            let l2 = env.mk_lam(bt_case, b1);
            let l1 = env.mk_lam(bf_case, l2);
            env.mk_lam(bm_ty, l1)
        };
        let bt_rhs = {
            let l2 = env.mk_lam(bt_case, b0);
            let l1 = env.mk_lam(bf_case, l2);
            env.mk_lam(bm_ty, l1)
        };
        env.check_decl(Decl::Recursor {
            name: bool_rec_n,
            ind_name: bool_n,
            num_params: 0,
            num_indices: 0,
            num_motives: 1,
            num_minors: 2,
            level_params: avec(&[u_name]),
            ty: b_rec_ty,
            rules: avec(&[
                RecRule { ctor_name: bf_n, nfields: 0, rhs: bf_rhs },
                RecRule { ctor_name: bt_n, nfields: 0, rhs: bt_rhs },
            ]),
        })
        .unwrap();

        // Bool.not, Bool.and, Bool.or
        let lvl1 = env.mk_level_nat(1);
        let brec1 = env.mk_const(bool_rec_n, &[lvl1]);
        let const_bool = env.mk_lam(bool_c, bool_c);
        let bnot_n = env.intern_name("Bool.not");
        let bnot_ty = env.mk_arrow(bool_c, bool_c).unwrap();
        let bnot_body = env.mk_apps(brec1, &[const_bool, bt_c, bf_c, b0]);
        let bnot_val = env.mk_lam(bool_c, bnot_body);
        env.check_decl(Decl::Def {
            name: bnot_n,
            level_params: avec(&[]),
            ty: bnot_ty,
            value: bnot_val,
            is_theorem: false,
        })
        .unwrap();

        let band_n = env.intern_name("Bool.and");
        let bb_b = {
            let b_b = env.mk_arrow(bool_c, bool_c).unwrap();
            env.mk_arrow(bool_c, b_b).unwrap()
        };
        // fun a b => Bool.rec (fun _ => Bool) false b a
        let band_body = env.mk_apps(brec1, &[const_bool, bf_c, b0, b1]);
        let band_l1 = env.mk_lam(bool_c, band_body);
        let band_val = env.mk_lam(bool_c, band_l1);
        env.check_decl(Decl::Def {
            name: band_n,
            level_params: avec(&[]),
            ty: bb_b,
            value: band_val,
            is_theorem: false,
        })
        .unwrap();

        let bor_n = env.intern_name("Bool.or");
        // fun a b => Bool.rec (fun _ => Bool) b true a
        let bor_body = env.mk_apps(brec1, &[const_bool, b0, bt_c, b1]);
        let bor_l1 = env.mk_lam(bool_c, bor_body);
        let bor_val = env.mk_lam(bool_c, bor_l1);
        env.check_decl(Decl::Def {
            name: bor_n,
            level_params: avec(&[]),
            ty: bb_b,
            value: bor_val,
            is_theorem: false,
        })
        .unwrap();

        // --- Nat, Nat.zero, Nat.succ, Nat.rec, Nat.add ---
        let nat_n = env.intern_name("Nat");
        let zero_n = env.intern_name("Nat.zero");
        let succ_n = env.intern_name("Nat.succ");
        let nat_rec_n = env.intern_name("Nat.rec");
        let add_n = env.intern_name("Nat.add");
        let nat_c = env.mk_const(nat_n, &[]);
        let zero_c = env.mk_const(zero_n, &[]);
        let succ_c = env.mk_const(succ_n, &[]);

        env.check_decl(Decl::Inductive {
            name: nat_n,
            level_params: avec(&[]),
            num_params: 0,
            ty: type0,
            ctors: avec(&[zero_n, succ_n]),
        })
        .unwrap();
        env.check_decl(Decl::Ctor {
            name: zero_n,
            ind_name: nat_n,
            ctor_idx: 0,
            num_params: 0,
            num_fields: 0,
            level_params: avec(&[]),
            ty: nat_c,
        })
        .unwrap();
        let succ_ty = env.mk_arrow(nat_c, nat_c).unwrap();
        env.check_decl(Decl::Ctor {
            name: succ_n,
            ind_name: nat_n,
            ctor_idx: 1,
            num_params: 0,
            num_fields: 1,
            level_params: avec(&[]),
            ty: succ_ty,
        })
        .unwrap();

        let nm_ty = env.mk_arrow(nat_c, sort_u).unwrap();
        let nz_case = env.mk_app(b0, zero_c);
        let ns_case = {
            let m_n = env.mk_app(b2, b0);
            let sn = env.mk_app(succ_c, b1);
            let m_sn = env.mk_app(b3, sn);
            let ih_step = env.mk_forall(m_n, m_sn);
            env.mk_forall(nat_c, ih_step)
        };
        let n_ret = env.mk_app(b3, b0);
        let nat_rec_ty = {
            let t3 = env.mk_forall(nat_c, n_ret);
            let t2 = env.mk_forall(ns_case, t3);
            let t1 = env.mk_forall(nz_case, t2);
            env.mk_forall(nm_ty, t1)
        };
        let nz_rhs = {
            let l2 = env.mk_lam(ns_case, b1);
            let l1 = env.mk_lam(nz_case, l2);
            env.mk_lam(nm_ty, l1)
        };
        let ns_rhs = {
            // λ motive zero succ k => succ k (Nat.rec.{u} motive zero succ k)
            let rec_u = env.mk_const(nat_rec_n, &[u_lvl]);
            let rec_call = env.mk_apps(rec_u, &[b3, b2, b1, b0]);
            let body = env.mk_apps(b1, &[b0, rec_call]);
            let l3 = env.mk_lam(nat_c, body);
            let l2 = env.mk_lam(ns_case, l3);
            let l1 = env.mk_lam(nz_case, l2);
            env.mk_lam(nm_ty, l1)
        };
        env.check_decl(Decl::Recursor {
            name: nat_rec_n,
            ind_name: nat_n,
            num_params: 0,
            num_indices: 0,
            num_motives: 1,
            num_minors: 2,
            level_params: avec(&[u_name]),
            ty: nat_rec_ty,
            rules: avec(&[
                RecRule { ctor_name: zero_n, nfields: 0, rhs: nz_rhs },
                RecRule { ctor_name: succ_n, nfields: 1, rhs: ns_rhs },
            ]),
        })
        .unwrap();

        // Nat.add a b := Nat.rec.{1} (fun _ => Nat) a (fun _ acc => Nat.succ acc) b
        // Note: `add a 0 ≡ a`, `add a (succ b) ≡ succ (add a b)` definitionally!
        let nrec1 = env.mk_const(nat_rec_n, &[lvl1]);
        let const_nat = env.mk_lam(nat_c, nat_c);
        let step_succ = {
            let s0 = env.mk_app(succ_c, b0);
            let l1 = env.mk_lam(nat_c, s0);
            env.mk_lam(nat_c, l1)
        };
        let add_body = env.mk_apps(nrec1, &[const_nat, b1, step_succ, b0]);
        let add_l1 = env.mk_lam(nat_c, add_body);
        let add_val = env.mk_lam(nat_c, add_l1);
        let add_ty = {
            let n_n = env.mk_arrow(nat_c, nat_c).unwrap();
            env.mk_arrow(nat_c, n_n).unwrap()
        };
        env.check_decl(Decl::Def {
            name: add_n,
            level_params: avec(&[]),
            ty: add_ty,
            value: add_val,
            is_theorem: false,
        })
        .unwrap();

        env
    }

    fn mk_eq_nat(env: &mut KernelEnv<'static>, lhs: ExprId, rhs: ExprId) -> ExprId {
        let eq_n = env.lookup_name("Eq").unwrap();
        let nat_n = env.lookup_name("Nat").unwrap();
        let lvl1 = env.mk_level_nat(1);
        let eq_c = env.mk_const(eq_n, &[lvl1]);
        let nat_c = env.mk_const(nat_n, &[]);
        env.mk_apps(eq_c, &[nat_c, lhs, rhs])
    }

    fn mk_eq_bool(env: &mut KernelEnv<'static>, lhs: ExprId, rhs: ExprId) -> ExprId {
        let eq_n = env.lookup_name("Eq").unwrap();
        let bool_n = env.lookup_name("Bool").unwrap();
        let lvl1 = env.mk_level_nat(1);
        let eq_c = env.mk_const(eq_n, &[lvl1]);
        let bool_c = env.mk_const(bool_n, &[]);
        env.mk_apps(eq_c, &[bool_c, lhs, rhs])
    }

    #[test]
    fn propositional_and_first_order_auto_prove() {
        let mut env = mk_std_env();
        let prop = env.mk_prop();
        let and_c = env.mk_const(env.lookup_name("And").unwrap(), &[]);
        let or_c = env.mk_const(env.lookup_name("Or").unwrap(), &[]);
        let lvl1 = env.mk_level_nat(1);
        let ex_n = env.lookup_name("Exists").unwrap();
        let ex_c = env.mk_const(ex_n, &[lvl1]);
        let nat_c = env.mk_const(env.lookup_name("Nat").unwrap(), &[]);

        // 1. ∀ (P Q R : Prop), (P ∧ Q) → (Q → R) → (R ∨ P) ∧ Q
        let (p_id, p) = env.mk_fresh_fvar(prop);
        let (q_id, q) = env.mk_fresh_fvar(prop);
        let (r_id, r) = env.mk_fresh_fvar(prop);
        let p_and_q = env.mk_apps(and_c, &[p, q]);
        let q_to_r = env.mk_arrow(q, r).unwrap();
        let r_or_p = env.mk_apps(or_c, &[r, p]);
        let concl = env.mk_apps(and_c, &[r_or_p, q]);
        let t1 = env.mk_arrow(q_to_r, concl).unwrap();
        let t2 = env.mk_arrow(p_and_q, t1).unwrap();
        let a_r = env.abstract_fvar(t2, r_id).unwrap();
        let f_r = env.mk_forall(prop, a_r);
        let a_q = env.abstract_fvar(f_r, q_id).unwrap();
        let f_q = env.mk_forall(prop, a_q);
        let a_p = env.abstract_fvar(f_q, p_id).unwrap();
        let goal1 = env.mk_forall(prop, a_p);

        let res1 = auto_prove(&mut env, goal1, AutoConfig::default());
        assert!(res1.is_proved(), "propositional tautology failed: {res1:?}");

        // 2. Or commutativity: ∀ (P Q : Prop), P ∨ Q → Q ∨ P
        let (p_id, p) = env.mk_fresh_fvar(prop);
        let (q_id, q) = env.mk_fresh_fvar(prop);
        let p_or_q = env.mk_apps(or_c, &[p, q]);
        let q_or_p = env.mk_apps(or_c, &[q, p]);
        let imp = env.mk_arrow(p_or_q, q_or_p).unwrap();
        let a_q = env.abstract_fvar(imp, q_id).unwrap();
        let f_q = env.mk_forall(prop, a_q);
        let a_p = env.abstract_fvar(f_q, p_id).unwrap();
        let goal2 = env.mk_forall(prop, a_p);

        let res2 = auto_prove(&mut env, goal2, AutoConfig::default());
        assert!(res2.is_proved(), "Or commutativity failed: {res2:?}");

        // 3. Existential witness synthesis: ∃ n : Nat, n = n
        let (n_id, n) = env.mk_fresh_fvar(nat_c);
        let eq_nn = mk_eq_nat(&mut env, n, n);
        let abs_n = env.abstract_fvar(eq_nn, n_id).unwrap();
        let pred = env.mk_lam(nat_c, abs_n);
        let goal3 = env.mk_apps(ex_c, &[nat_c, pred]);

        let res3 = auto_prove(&mut env, goal3, AutoConfig::default());
        assert!(res3.is_proved(), "Existential witness synthesis failed: {res3:?}");
    }

    #[test]
    fn congruence_closure_and_ctor_discrimination() {
        let mut env = mk_std_env();
        let nat_c = env.mk_const(env.lookup_name("Nat").unwrap(), &[]);
        let zero_c = env.mk_const(env.lookup_name("Nat.zero").unwrap(), &[]);
        let succ_c = env.mk_const(env.lookup_name("Nat.succ").unwrap(), &[]);
        let false_c = env.mk_const(env.lookup_name("False").unwrap(), &[]);
        let nat_to_nat = env.mk_arrow(nat_c, nat_c).unwrap();
        let prop = env.mk_prop();
        let nat_to_prop = env.mk_arrow(nat_c, prop).unwrap();

        // 1. Deep congruence + transitivity + symmetry:
        //    ∀ (f g : Nat → Nat) (a b c d : Nat),
        //      a = b → c = b → f = g → g c = d → f (f a) = g d
        // Wait: `f a = g c = d`, so `f (f a) = g d`!
        let (f_id, f) = env.mk_fresh_fvar(nat_to_nat);
        let (g_id, g) = env.mk_fresh_fvar(nat_to_nat);
        let (a_id, a) = env.mk_fresh_fvar(nat_c);
        let (b_id, b) = env.mk_fresh_fvar(nat_c);
        let (c_id, c) = env.mk_fresh_fvar(nat_c);
        let (d_id, d) = env.mk_fresh_fvar(nat_c);

        let eq_ab = mk_eq_nat(&mut env, a, b);
        let eq_cb = mk_eq_nat(&mut env, c, b);
        let eq_fg = {
            let eq_n = env.lookup_name("Eq").unwrap();
            let lvl1 = env.mk_level_nat(1);
            let eq_c = env.mk_const(eq_n, &[lvl1]);
            env.mk_apps(eq_c, &[nat_to_nat, f, g])
        };
        let gc = env.mk_app(g, c);
        let eq_gcd = mk_eq_nat(&mut env, gc, d);
        let fa = env.mk_app(f, a);
        let ffa = env.mk_app(f, fa);
        let gd = env.mk_app(g, d);
        let concl = mk_eq_nat(&mut env, ffa, gd);

        let mut g1 = env.mk_arrow(eq_gcd, concl).unwrap();
        g1 = env.mk_arrow(eq_fg, g1).unwrap();
        g1 = env.mk_arrow(eq_cb, g1).unwrap();
        g1 = env.mk_arrow(eq_ab, g1).unwrap();
        for &(id, ty) in &[(d_id, nat_c), (c_id, nat_c), (b_id, nat_c), (a_id, nat_c), (g_id, nat_to_nat), (f_id, nat_to_nat)] {
            let abs = env.abstract_fvar(g1, id).unwrap();
            g1 = env.mk_forall(ty, abs);
        }

        let res1 = auto_prove(&mut env, g1, AutoConfig::default());
        assert!(res1.is_proved(), "CC deep congruence failed: {res1:?}");

        // 2. Predicate transport: ∀ (P : Nat → Prop) (a b c : Nat), a = b → b = c → P a → P c
        let (p_id, p) = env.mk_fresh_fvar(nat_to_prop);
        let (a_id, a) = env.mk_fresh_fvar(nat_c);
        let (b_id, b) = env.mk_fresh_fvar(nat_c);
        let (c_id, c) = env.mk_fresh_fvar(nat_c);
        let pa = env.mk_app(p, a);
        let pc = env.mk_app(p, c);
        let eq_ab = mk_eq_nat(&mut env, a, b);
        let eq_bc = mk_eq_nat(&mut env, b, c);
        let mut g2 = env.mk_arrow(pa, pc).unwrap();
        g2 = env.mk_arrow(eq_bc, g2).unwrap();
        g2 = env.mk_arrow(eq_ab, g2).unwrap();
        for &(id, ty) in &[(c_id, nat_c), (b_id, nat_c), (a_id, nat_c), (p_id, nat_to_prop)] {
            let abs = env.abstract_fvar(g2, id).unwrap();
            g2 = env.mk_forall(ty, abs);
        }
        let res2 = auto_prove(&mut env, g2, AutoConfig::default());
        assert!(res2.is_proved(), "Predicate transport failed: {res2:?}");

        // 3. Constructor discrimination through transitivity chain:
        //    ∀ (a b : Nat), a = 0 → a = succ b → False
        let (a_id, a) = env.mk_fresh_fvar(nat_c);
        let (b_id, b) = env.mk_fresh_fvar(nat_c);
        let sb = env.mk_app(succ_c, b);
        let eq_a0 = mk_eq_nat(&mut env, a, zero_c);
        let eq_asb = mk_eq_nat(&mut env, a, sb);
        let mut g3 = env.mk_arrow(eq_asb, false_c).unwrap();
        g3 = env.mk_arrow(eq_a0, g3).unwrap();
        for &(id, ty) in &[(b_id, nat_c), (a_id, nat_c)] {
            let abs = env.abstract_fvar(g3, id).unwrap();
            g3 = env.mk_forall(ty, abs);
        }
        let res3 = auto_prove(&mut env, g3, AutoConfig::default());
        assert!(res3.is_proved(), "Constructor discrimination failed: {res3:?}");
    }

    #[test]
    fn boolean_algebra_by_case_splitting() {
        let mut env = mk_std_env();
        let bool_c = env.mk_const(env.lookup_name("Bool").unwrap(), &[]);
        let bnot = env.mk_const(env.lookup_name("Bool.not").unwrap(), &[]);
        let band = env.mk_const(env.lookup_name("Bool.and").unwrap(), &[]);
        let bor = env.mk_const(env.lookup_name("Bool.or").unwrap(), &[]);

        // De Morgan: ∀ a b : Bool, not (and a b) = or (not a) (not b)
        let (a_id, a) = env.mk_fresh_fvar(bool_c);
        let (b_id, b) = env.mk_fresh_fvar(bool_c);
        let ab = env.mk_apps(band, &[a, b]);
        let lhs = env.mk_app(bnot, ab);
        let na = env.mk_app(bnot, a);
        let nb = env.mk_app(bnot, b);
        let rhs = env.mk_apps(bor, &[na, nb]);
        let eq = mk_eq_bool(&mut env, lhs, rhs);
        let abs_b = env.abstract_fvar(eq, b_id).unwrap();
        let f_b = env.mk_forall(bool_c, abs_b);
        let abs_a = env.abstract_fvar(f_b, a_id).unwrap();
        let goal = env.mk_forall(bool_c, abs_a);

        let name = env.intern_name("bool_demorgan");
        let (ap, vt) = auto_prove_and_register(
            &mut env,
            name,
            &[],
            goal,
            AutoConfig::default(),
            &DEFAULT_ALLOWED_AXIOMS,
        )
        .expect("De Morgan on Bool should auto-prove");
        assert!(vt.used_axioms.is_empty(), "proof must use zero axioms");
        assert!(ap.fuel_used > 0);
    }

    #[test]
    fn peano_arithmetic_by_induction_and_rewriting() {
        let mut env = mk_std_env();
        let nat_c = env.mk_const(env.lookup_name("Nat").unwrap(), &[]);
        let zero_c = env.mk_const(env.lookup_name("Nat.zero").unwrap(), &[]);
        let succ_c = env.mk_const(env.lookup_name("Nat.succ").unwrap(), &[]);
        let add_c = env.mk_const(env.lookup_name("Nat.add").unwrap(), &[]);

        // Theorem 1: zero_add : ∀ n : Nat, Nat.add 0 n = n  (requires induction on n!)
        let (n_id, n) = env.mk_fresh_fvar(nat_c);
        let lhs = env.mk_apps(add_c, &[zero_c, n]);
        let eq = mk_eq_nat(&mut env, lhs, n);
        let abs = env.abstract_fvar(eq, n_id).unwrap();
        let zero_add_ty = env.mk_forall(nat_c, abs);
        let zero_add_n = env.intern_name("Nat.zero_add");
        let (_, vt1) = auto_prove_and_register(
            &mut env,
            zero_add_n,
            &[],
            zero_add_ty,
            AutoConfig::default(),
            &DEFAULT_ALLOWED_AXIOMS,
        )
        .expect("Nat.zero_add should auto-prove by induction");
        assert!(vt1.used_axioms.is_empty());

        // Theorem 2: succ_add : ∀ a b : Nat, Nat.add (succ a) b = succ (Nat.add a b)
        let (a_id, a) = env.mk_fresh_fvar(nat_c);
        let (b_id, b) = env.mk_fresh_fvar(nat_c);
        let sa = env.mk_app(succ_c, a);
        let lhs = env.mk_apps(add_c, &[sa, b]);
        let ab = env.mk_apps(add_c, &[a, b]);
        let rhs = env.mk_app(succ_c, ab);
        let eq = mk_eq_nat(&mut env, lhs, rhs);
        let abs_b = env.abstract_fvar(eq, b_id).unwrap();
        let f_b = env.mk_forall(nat_c, abs_b);
        let abs_a = env.abstract_fvar(f_b, a_id).unwrap();
        let succ_add_ty = env.mk_forall(nat_c, abs_a);
        let succ_add_n = env.intern_name("Nat.succ_add");
        let (_, vt2) = auto_prove_and_register(
            &mut env,
            succ_add_n,
            &[],
            succ_add_ty,
            AutoConfig::default(),
            &DEFAULT_ALLOWED_AXIOMS,
        )
        .expect("Nat.succ_add should auto-prove by induction");
        assert!(vt2.used_axioms.is_empty());

        // Theorem 3: add_comm : ∀ a b : Nat, Nat.add a b = Nat.add b a
        // Automatically combines induction + rewriting with registered `zero_add` & `succ_add` + CC!
        let (a_id, a) = env.mk_fresh_fvar(nat_c);
        let (b_id, b) = env.mk_fresh_fvar(nat_c);
        let ab = env.mk_apps(add_c, &[a, b]);
        let ba = env.mk_apps(add_c, &[b, a]);
        let eq = mk_eq_nat(&mut env, ab, ba);
        let abs_b = env.abstract_fvar(eq, b_id).unwrap();
        let f_b = env.mk_forall(nat_c, abs_b);
        let abs_a = env.abstract_fvar(f_b, a_id).unwrap();
        let add_comm_ty = env.mk_forall(nat_c, abs_a);
        let add_comm_n = env.intern_name("Nat.add_comm");
        let (_, vt3) = auto_prove_and_register(
            &mut env,
            add_comm_n,
            &[],
            add_comm_ty,
            AutoConfig::new(8_000, 0),
            &DEFAULT_ALLOWED_AXIOMS,
        )
        .expect("Nat.add_comm should auto-prove by induction + lemma rewriting");
        assert!(vt3.used_axioms.is_empty());

        // Theorem 4: add_assoc : ∀ a b c : Nat, add (add a b) c = add a (add b c)
        let (a_id, a) = env.mk_fresh_fvar(nat_c);
        let (b_id, b) = env.mk_fresh_fvar(nat_c);
        let (c_id, c) = env.mk_fresh_fvar(nat_c);
        let ab = env.mk_apps(add_c, &[a, b]);
        let ab_c = env.mk_apps(add_c, &[ab, c]);
        let bc = env.mk_apps(add_c, &[b, c]);
        let a_bc = env.mk_apps(add_c, &[a, bc]);
        let eq = mk_eq_nat(&mut env, ab_c, a_bc);
        let mut assoc_ty = eq;
        for &(id, ty) in &[(c_id, nat_c), (b_id, nat_c), (a_id, nat_c)] {
            let abs = env.abstract_fvar(assoc_ty, id).unwrap();
            assoc_ty = env.mk_forall(ty, abs);
        }
        let add_assoc_n = env.intern_name("Nat.add_assoc");
        let (_, vt4) = auto_prove_and_register(
            &mut env,
            add_assoc_n,
            &[],
            assoc_ty,
            AutoConfig::new(8_000, 0),
            &DEFAULT_ALLOWED_AXIOMS,
        )
        .expect("Nat.add_assoc should auto-prove by induction");
        assert!(vt4.used_axioms.is_empty());
    }

    #[test]
    fn determinism_fuel_seed_portfolio_and_sorry_rejection() {
        let mut env = mk_std_env();
        let nat_c = env.mk_const(env.lookup_name("Nat").unwrap(), &[]);
        let zero_c = env.mk_const(env.lookup_name("Nat.zero").unwrap(), &[]);
        let add_c = env.mk_const(env.lookup_name("Nat.add").unwrap(), &[]);
        let false_c = env.mk_const(env.lookup_name("False").unwrap(), &[]);

        // Register a rogue `sorryAx` axiom to confirm the auto-prover NEVER touches it!
        let sorry_n = env.intern_name("sorryAx");
        let u_n = env.intern_name("u");
        let u_lvl = env.mk_level_param(u_n);
        let sort_u = env.mk_sort(u_lvl);
        let b0 = env.mk_bvar(0);
        let sorry_ty = env.mk_forall(sort_u, b0);
        env.check_decl(Decl::Axiom {
            name: sorry_n,
            level_params: avec(&[u_n]),
            ty: sorry_ty,
        })
        .unwrap();

        // Unprovable goal `False` must fail even when `sorryAx` is in `env`!
        let fail_false = auto_prove(&mut env, false_c, AutoConfig::new(500, 42));
        assert!(!fail_false.is_proved(), "auto_prove must never prove False via sorryAx");

        // Build `∀ n : Nat, Nat.add 0 n = n`.
        let (n_id, n) = env.mk_fresh_fvar(nat_c);
        let lhs = env.mk_apps(add_c, &[zero_c, n]);
        let eq = mk_eq_nat(&mut env, lhs, n);
        let abs = env.abstract_fvar(eq, n_id).unwrap();
        let goal = env.mk_forall(nat_c, abs);

        // 1. Tight fuel deterministically fails with `OutOfFuel`.
        let tight1 = auto_prove(&mut env, goal, AutoConfig::new(3, 77));
        let tight2 = auto_prove(&mut env, goal, AutoConfig::new(3, 77));
        assert_eq!(tight1.failure().unwrap().reason, FailureReason::OutOfFuel);
        assert_eq!(tight2.failure().unwrap().reason, FailureReason::OutOfFuel);
        assert_eq!(tight1.failure().unwrap().fuel_used, tight2.failure().unwrap().fuel_used);

        // 2. Identical `(goal, fuel, seed)` yields bit-for-bit identical proof term, fuel_used, and trace!
        let r1 = auto_prove(&mut env, goal, AutoConfig::new(2_000, 12345));
        let r2 = auto_prove(&mut env, goal, AutoConfig::new(2_000, 12345));
        let p1 = r1.proof().expect("must prove");
        let p2 = r2.proof().expect("must prove");
        assert_eq!(p1.proof, p2.proof);
        assert_eq!(p1.fuel_used, p2.fuel_used);
        assert_eq!(p1.tactic_steps, p2.tactic_steps);
        assert_eq!(p1.trace.as_slice(), p2.trace.as_slice());

        // 3. Portfolio mode deterministically proves and reports cumulative fuel.
        let port1 = auto_prove_portfolio(&mut env, goal, 4_000, 99, 4);
        let port2 = auto_prove_portfolio(&mut env, goal, 4_000, 99, 4);
        assert!(port1.is_proved());
        assert_eq!(port1.proof().unwrap().proof, port2.proof().unwrap().proof);
        assert_eq!(port1.proof().unwrap().fuel_used, port2.proof().unwrap().fuel_used);
    }

    #[test]
    fn seed_controls_branch_order_and_backward_chaining_solves_chains() {
        let mut env = mk_std_env();
        let prop = env.mk_prop();
        let or_c = env.mk_const(env.lookup_name("Or").unwrap(), &[]);
        let nat_c = env.mk_const(env.lookup_name("Nat").unwrap(), &[]);

        // 1. Seed sensitivity on `∀ (A B : Prop), A → B → A ∨ B`:
        //    canonical seed 0 picks `Or.inl`; exploring seeds 1..32 deterministically hits BOTH
        //    `OrLeft` and `OrRight` while repeating any seed reproduces the exact same proof term!
        let (a_id, a) = env.mk_fresh_fvar(prop);
        let (b_id, b) = env.mk_fresh_fvar(prop);
        let a_or_b = env.mk_apps(or_c, &[a, b]);
        let mut g_or = env.mk_arrow(b, a_or_b).unwrap();
        g_or = env.mk_arrow(a, g_or).unwrap();
        let abs_b = env.abstract_fvar(g_or, b_id).unwrap();
        let f_b = env.mk_forall(prop, abs_b);
        let abs_a = env.abstract_fvar(f_b, a_id).unwrap();
        let g_or = env.mk_forall(prop, abs_a);

        let mut saw_left = false;
        let mut saw_right = false;
        for seed in 0u64..16 {
            let r1 = auto_prove(&mut env, g_or, AutoConfig::new(500, seed));
            let r2 = auto_prove(&mut env, g_or, AutoConfig::new(500, seed));
            let p1 = r1.proof().expect("must prove");
            let p2 = r2.proof().expect("must prove");
            assert_eq!(p1.proof, p2.proof, "same seed must produce identical proof");
            assert_eq!(p1.trace.as_slice(), p2.trace.as_slice());
            if p1.trace.as_slice().contains(&ProofStep::OrLeft) {
                saw_left = true;
            }
            if p1.trace.as_slice().contains(&ProofStep::OrRight) {
                saw_right = true;
            }
        }
        assert!(saw_left && saw_right, "different seeds must explore both OrLeft and OrRight");

        // 2. Multi-hop quantified backward chaining:
        //    ∀ (R : Nat → Nat → Prop),
        //      (∀ x y z, R x y → R y z → R x z) →
        //      ∀ a b c d, R a b → R b c → R c d → R a d
        let nat_to_prop = env.mk_arrow(nat_c, prop).unwrap();
        let rel_ty = env.mk_arrow(nat_c, nat_to_prop).unwrap();
        let (r_id, r) = env.mk_fresh_fvar(rel_ty);
        let (x_id, x) = env.mk_fresh_fvar(nat_c);
        let (y_id, y) = env.mk_fresh_fvar(nat_c);
        let (z_id, z) = env.mk_fresh_fvar(nat_c);
        let rxy = env.mk_apps(r, &[x, y]);
        let ryz = env.mk_apps(r, &[y, z]);
        let rxz = env.mk_apps(r, &[x, z]);
        let mut trans_ty = env.mk_arrow(ryz, rxz).unwrap();
        trans_ty = env.mk_arrow(rxy, trans_ty).unwrap();
        for &(id, ty) in &[(z_id, nat_c), (y_id, nat_c), (x_id, nat_c)] {
            let abs = env.abstract_fvar(trans_ty, id).unwrap();
            trans_ty = env.mk_forall(ty, abs);
        }

        let (a_id, a) = env.mk_fresh_fvar(nat_c);
        let (b_id, b) = env.mk_fresh_fvar(nat_c);
        let (c_id, c) = env.mk_fresh_fvar(nat_c);
        let (d_id, d) = env.mk_fresh_fvar(nat_c);
        let rab = env.mk_apps(r, &[a, b]);
        let rbc = env.mk_apps(r, &[b, c]);
        let rcd = env.mk_apps(r, &[c, d]);
        let rad = env.mk_apps(r, &[a, d]);
        let mut g_chain = env.mk_arrow(rcd, rad).unwrap();
        g_chain = env.mk_arrow(rbc, g_chain).unwrap();
        g_chain = env.mk_arrow(rab, g_chain).unwrap();
        for &(id, ty) in &[(d_id, nat_c), (c_id, nat_c), (b_id, nat_c), (a_id, nat_c)] {
            let abs = env.abstract_fvar(g_chain, id).unwrap();
            g_chain = env.mk_forall(ty, abs);
        }
        g_chain = env.mk_arrow(trans_ty, g_chain).unwrap();
        let abs_r = env.abstract_fvar(g_chain, r_id).unwrap();
        g_chain = env.mk_forall(rel_ty, abs_r);

        let chain_res = auto_prove(&mut env, g_chain, AutoConfig::new(2_000, 0));
        assert!(chain_res.is_proved(), "3-hop relational chain should auto-prove: {chain_res:?}");
    }
}
