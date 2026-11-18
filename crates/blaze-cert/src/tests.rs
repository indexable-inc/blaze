//! The differential test (certificates.md, "The differential test"): for each goal the solver
//! answers `yes` over blaze-solve's test world, a hand-built derivation of it checks; each
//! planted wrong derivation is refused with the error that names its row; truncated or flipped
//! bytes are a typed error or a refusal, never a panic; and the questions half: an impl that
//! could match moves a question, an unrelated one moves none and the derivation still checks.
//!
//! A case's first goal is the root, the solver's; the rest are the derivation's step goals, in
//! the order the fixture's rows name them (each `=> yes` as well, so the solver agrees on every
//! step).

use std::collections::HashMap;
use std::println;
use std::string::String;
use std::vec::Vec as StdVec;

use blaze_own::{Alloc, Vec};
use blaze_solve::assembly::{Clause, EnvId, Goal};
use blaze_solve::eval::{Answer, Solver, Stats};
use blaze_solve::facts::Read;
use blaze_solve::term::{Def, Pred, Ty, TyKind, TypingMode, UintTy};
use blaze_solve::testing::parse::{CaseA, CheckA, DeclA, parse_file};
use blaze_solve::testing::run::PRELUDE;
use blaze_solve::testing::tys::TestTys;
use blaze_solve::testing::world::{Arena, Scope, World, def_of};
use blaze_solve::tys::Tys;
use blaze_solve::witness::{Emit, NoDerivation};

use crate::check::{CheckError, Checker};
use crate::derivation::{Builder, Builtin, By, FormatError, StepId, View};
use crate::questions::Questions;
use crate::{Today, today};

pub(crate) fn alloc() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

/// One case's world, arena, solver and environment.
pub(crate) struct Fx<E = NoDerivation> {
    pub(crate) world: World,
    pub(crate) case: CaseA,
    pub(crate) tys: TestTys,
    vars: HashMap<String, Ty>,
    pub(crate) solver: Solver<'static, E>,
    pub(crate) env: EnvId,
}

/// The world of `text` (one case) on the prelude, `extra` declarations added to the case's.
pub(crate) fn fixture(text: &str, extra: &str) -> Fx {
    fixture_with(text, extra, NoDerivation)
}

/// [`fixture`] with a solver recording into `emit` (emit_tests.rs).
pub(crate) fn fixture_with<E: Emit>(text: &str, extra: &str, emit: E) -> Fx<E> {
    let prelude = parse_file(PRELUDE).unwrap_or_else(|e| panic!("the prelude: {e}"));
    let f = parse_file(text).unwrap_or_else(|e| panic!("the fixture: {e}"));
    let x = parse_file(extra).unwrap_or_else(|e| panic!("the extra decls: {e}"));
    let [case] = f.cases.as_slice() else { panic!("{} cases, want one", f.cases.len()) };
    let mut decls: StdVec<(u32, &DeclA)> = StdVec::new();
    decls.extend(prelude.decls.iter().map(|(l, d)| (*l, d)));
    decls.extend(f.decls.iter().map(|(l, d)| (*l, d)));
    decls.extend(case.decls.iter().map(|(l, d)| (*l, d)));
    // Last, so every declaration before keeps its place.
    decls.extend(x.decls.iter().map(|(l, d)| (*l, d)));
    let world = World::build(&decls).unwrap_or_else(|e| panic!("the world: {e}"));
    let mut tys = TestTys::new();
    let mut vars = HashMap::new();
    let mut solver = Solver::with_emit_in(alloc(), emit);
    let sc = Scope { params: &case.params, bound: &[] };
    let mut cl: StdVec<Clause> = StdVec::new();
    {
        let mut a = Arena { tys: &mut tys, vars: &mut vars };
        for p in &case.env {
            world.names.pred(&mut a, sc, case.line, p, &mut cl).unwrap_or_else(|e| panic!("the env: {e}"));
        }
        let sized: StdVec<bool> = case.params.iter().map(|_| false).collect();
        world.names.case_sized(&mut a, case.line, &sized, &mut cl).unwrap_or_else(|e| panic!("the env: {e}"));
    }
    let mut reads: Vec<Read, Alloc<'static>> = Vec::new_in(alloc());
    let env = solver.env(&mut tys, &world, &cl, &mut reads).unwrap_or_else(|s| panic!("the env: {s:?}"));
    Fx { world, case: case.clone(), tys, vars, solver, env }
}

impl<E: Emit> Fx<E> {
    /// Goal line `i` of the case, in the arena.
    pub(crate) fn goal(&mut self, i: usize) -> Pred {
        let g = &self.case.goals[i];
        let CheckA::Goal(p) = &g.check else { panic!("line {}: not a goal", g.line) };
        let sc = Scope { params: &self.case.params, bound: &[] };
        let mut cl: StdVec<Clause> = StdVec::new();
        let mut a = Arena { tys: &mut self.tys, vars: &mut self.vars };
        self.world.names.pred(&mut a, sc, g.line, p, &mut cl).unwrap_or_else(|e| panic!("line {}: {e}", g.line));
        let [c] = cl.as_slice() else { panic!("line {}: a goal is one predicate", g.line) };
        assert_eq!(c.bound, 0, "line {}: the fixtures' goals are not higher-ranked", g.line);
        c.pred
    }

    /// Goal `i` solved: its answer and the reads of the solve.
    pub(crate) fn solve(&mut self, i: usize) -> (Answer, Vec<Read, Alloc<'static>>) {
        let pred = self.goal(i);
        let mut reads: Vec<Read, Alloc<'static>> = Vec::new_in(alloc());
        let snap = self.tys.snapshot();
        let a = self
            .solver
            .evaluate(&mut self.tys, &self.world, TypingMode::Analysis, Goal { env: self.env, pred, bound: 0 }, &mut reads)
            .unwrap_or_else(|s| panic!("goal {i}: the solver stopped: {s:?}"));
        self.tys.rollback_to(snap);
        (a, reads)
    }

    /// Every goal of the case answers `yes` (the solver agrees with each step).
    pub(crate) fn all_yes(&mut self) {
        for i in 0..self.case.goals.len() {
            let (a, _) = self.solve(i);
            assert_eq!(a, Answer::Yes, "goal {i} (line {})", self.case.goals[i].line);
        }
    }

    pub(crate) fn check(&mut self, bytes: &[u8]) -> Result<crate::Cost, CheckError> {
        let view = View::read(bytes).unwrap_or_else(|e| panic!("{e}"));
        let mut checker = Checker::new_in(alloc());
        let env = self.solver.env_clauses(self.env);
        checker.check(view, &mut self.tys, &self.world, env).map(|c| c.cost())
    }

    /// The index of the environment clause of `trait_`'s trait (the first).
    fn env_index(&self, name: &str) -> u32 {
        let d = def_of("trait", name);
        let i = self
            .solver
            .env_clauses(self.env)
            .iter()
            .position(|c| matches!(c.pred, Pred::Trait { trait_, .. } if trait_ == d))
            .unwrap_or_else(|| panic!("no env clause of {name}"));
        u32::try_from(i).unwrap_or_else(|_| panic!("env index {i}"))
    }

    fn uint(&mut self, u: UintTy) -> Ty {
        self.tys.mk(TyKind::Uint(u)).unwrap_or_else(|e| panic!("{e:?}"))
    }
}

pub(crate) fn imp(label: &str) -> Def {
    def_of("impl", label)
}

/// A derivation's bytes from `rows` (goal index, by, nested rows) and back-edges closed after.
pub(crate) fn bytes<E: Emit>(fx: &mut Fx<E>, rows: &[(usize, Row)], closes: &[(u32, u32)]) -> StdVec<u8> {
    let mut b = Builder::new_in(alloc());
    for (g, row) in rows {
        let goal = fx.goal(*g);
        let id = match row {
            Row::By(by, nested) => {
                let nested: StdVec<StepId> = nested.iter().map(|&n| StepId(n)).collect();
                let args: StdVec<Ty> = by.args.iter().map(|&u| fx.uint(u)).collect();
                let by = match by.kind {
                    K::Impl(l) => By::Impl { impl_: imp(l), args: &args },
                    K::Normalize(l) => By::Normalize { impl_: imp(l), args: &args },
                    K::Env(c) => By::Env { clause: c },
                    K::Builtin(r) => By::Builtin(r),
                    K::Auto => By::Auto,
                    K::Object(i) => By::Object { bound: i },
                };
                b.push(&fx.tys, goal, by, &nested)
            }
            Row::Back => b.back_edge(&fx.tys, goal),
        };
        id.unwrap_or_else(|e| panic!("{e}"));
    }
    for &(s, t) in closes {
        b.close_cycle(StepId(s), StepId(t)).unwrap_or_else(|e| panic!("{e}"));
    }
    let mut out: Vec<u8, Alloc<'static>> = Vec::new_in(alloc());
    b.finish(&mut out).unwrap_or_else(|e| panic!("{e}"));
    out.iter().copied().collect()
}

#[derive(Clone, Copy)]
pub(crate) enum K {
    Impl(&'static str),
    Normalize(&'static str),
    Env(u32),
    Builtin(Builtin),
    Auto,
    Object(u32),
}

#[derive(Clone, Copy)]
pub(crate) struct ByA {
    kind: K,
    args: &'static [UintTy],
}

#[derive(Clone, Copy)]
pub(crate) enum Row {
    By(ByA, &'static [u32]),
    Back,
}

pub(crate) const fn by(kind: K, args: &'static [UintTy], nested: &'static [u32]) -> Row {
    Row::By(ByA { kind, args }, nested)
}

pub(crate) const SIZED: K = K::Builtin(Builtin::Sized);

pub(crate) const CHAIN: &str = "
case chain = cert three-deep impl chain
trait A
trait B
trait C
struct W<T>(T)
impl@ia<T: B> A for W<T>
impl@ib<T: C> B for T
impl@ic C for u32
goal W<u32>: A => yes
goal u32: Sized => yes
goal u32: C => yes
goal u32: B => yes
";

/// Rows 0 `u32: Sized`, 1 `u32: C`, 2 `u32: B`, 3 `W<u32>: A` (the root, last).
pub(crate) fn chain_rows(ia_args: &'static [UintTy], ia_nested: &'static [u32]) -> [(usize, Row); 4] {
    [
        (1, by(SIZED, &[], &[])),
        (2, by(K::Impl("ic"), &[], &[])),
        (3, by(K::Impl("ib"), &[UintTy::U32], &[0, 1])),
        (0, by(K::Impl("ia"), ia_args, ia_nested)),
    ]
}

#[test]
fn chain_checks_and_costs_a_line() {
    let mut fx = fixture(CHAIN, "");
    fx.all_yes();
    let mut solver = Solver::new_in(alloc());
    // The root alone in a fresh solver, for the cost line.
    let mut reads: Vec<Read, Alloc<'static>> = Vec::new_in(alloc());
    let env = solver.env(&mut fx.tys, &fx.world, fx.solver.env_clauses(fx.env), &mut reads).unwrap_or_else(|s| panic!("{s:?}"));
    let root = fx.goal(0);
    let a = solver
        .evaluate(&mut fx.tys, &fx.world, TypingMode::Analysis, Goal { env, pred: root, bound: 0 }, &mut reads)
        .unwrap_or_else(|s| panic!("{s:?}"));
    assert_eq!(a, Answer::Yes);
    let st: Stats = solver.stats();
    let b = bytes(&mut fx, &chain_rows(&[UintTy::U32], &[0, 2]), &[]);
    let cost = fx.check(&b).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(cost.rows, 4);
    assert_eq!(cost.headers, 3, "one header per impl step");
    println!(
        "blaze-cert cost, W<u32>: A three deep: check rows={} headers={} nodes_compared={} words_decoded={} bytes={} | solver frames={} candidates={} impl_candidates={}",
        cost.rows,
        cost.headers,
        cost.nodes_compared,
        cost.words_decoded,
        b.len(),
        st.frames,
        st.candidates,
        st.impl_candidates
    );
}

#[test]
fn chain_bytes_are_deterministic() {
    let mut fx1 = fixture(CHAIN, "");
    let mut fx2 = fixture(CHAIN, "");
    // A second arena whose handles differ: a term made first shifts every later one.
    fx2.uint(UintTy::U128);
    let rows = chain_rows(&[UintTy::U32], &[0, 2]);
    assert_eq!(bytes(&mut fx1, &rows, &[]), bytes(&mut fx2, &rows, &[]));
}

#[test]
fn control_wrong_impl_argument() {
    let mut fx = fixture(CHAIN, "");
    let b = bytes(&mut fx, &chain_rows(&[UintTy::U8], &[0, 2]), &[]);
    assert_eq!(fx.check(&b), Err(CheckError::HeaderMismatch { step: StepId(3), impl_: imp("ia") }));
}

#[test]
fn control_nested_for_wrong_clause() {
    let mut fx = fixture(CHAIN, "");
    // `T: Sized` comes first in `ia`'s clauses; row 2 proves `u32: B`.
    let b = bytes(&mut fx, &chain_rows(&[UintTy::U32], &[2, 0]), &[]);
    assert_eq!(fx.check(&b), Err(CheckError::NestedMismatch { step: StepId(3), clause: 0 }));
}

#[test]
fn control_missing_nested_step() {
    let mut fx = fixture(CHAIN, "");
    let b = bytes(&mut fx, &chain_rows(&[UintTy::U32], &[0]), &[]);
    assert_eq!(fx.check(&b), Err(CheckError::NestedCount { step: StepId(3), want: 2, have: 1 }));
}

#[test]
fn truncated_and_flipped_bytes_never_panic() {
    let mut fx = fixture(CHAIN, "");
    let b = bytes(&mut fx, &chain_rows(&[UintTy::U32], &[0, 2]), &[]);
    for n in 0..b.len() {
        assert!(View::read(&b[..n]).is_err(), "a prefix of {n} of {} bytes read as a derivation", b.len());
    }
    assert!(matches!(View::read(&[]), Err(FormatError::Short { have: 0 })));
    // Every byte flipped: a format error, a refusal or (a flip the check cannot see, as in a
    // reserved bit of a def key it then finds missing) a check result; never a panic.
    for i in 0..b.len() {
        let mut m = b.clone();
        m[i] ^= 0xff;
        if let Ok(view) = View::read(&m) {
            let mut tys = TestTys::new();
            let mut checker = Checker::new_in(alloc());
            let _ = checker.check(view, &mut tys, &fx.world, fx.solver.env_clauses(fx.env));
        }
    }
}

const ENV: &str = "
case env = cert env clause
trait Foo
params T
env T: Foo
goal T: Foo => yes
";

#[test]
fn env_clause_checks() {
    let mut fx = fixture(ENV, "");
    fx.all_yes();
    let i = fx.env_index("Foo");
    let b = bytes(&mut fx, &[(0, by(K::Env(i), &[], &[]))], &[]);
    fx.check(&b).unwrap_or_else(|e| panic!("{e}"));
}

#[test]
fn control_env_clause_not_the_goal() {
    let mut fx = fixture(ENV, "");
    let i = fx.env_index("Sized");
    let b = bytes(&mut fx, &[(0, by(K::Env(i), &[], &[]))], &[]);
    assert_eq!(fx.check(&b), Err(CheckError::EnvMismatch { step: StepId(0), index: i }));
}

#[test]
fn coinductive_send_for_recursive_type() {
    let mut fx = fixture(
        "
case node = cert coinductive auto cycle
struct Node(Box<Node>)
goal Node: Send => yes
goal Box<Node>: Send => yes
",
        "",
    );
    fx.all_yes();
    // 0 the back-edge to 2, 1 `Box<Node>: Send` by its field, 2 `Node: Send` by its field.
    let rows = [(0, Row::Back), (1, by(K::Auto, &[], &[0])), (0, by(K::Auto, &[], &[1]))];
    let b = bytes(&mut fx, &rows, &[(0, 2)]);
    fx.check(&b).unwrap_or_else(|e| panic!("{e}"));
}

#[test]
fn control_inductive_cycle() {
    let mut fx = fixture(
        "
case ind = cert inductive cycle
trait Ind
impl@loop Ind for u32 where u32: Ind
goal u32: Ind => no
",
        "",
    );
    let rows = [(0, Row::Back), (0, by(K::Impl("loop"), &[], &[0]))];
    let b = bytes(&mut fx, &rows, &[(0, 1)]);
    assert_eq!(fx.check(&b), Err(CheckError::InductiveCycle { step: StepId(0) }));
}

#[test]
fn projection_by_impl_value() {
    let mut fx = fixture(
        "
case proj = cert projection
trait Get { type Out; }
struct W<T>(T)
impl@ig<T> Get for W<T> { type Out = T; }
goal <W<u32> as Get>::Out == u32 => yes
goal u32: Sized => yes
",
        "",
    );
    fx.all_yes();
    let rows = [(1, by(SIZED, &[], &[])), (0, by(K::Normalize("ig"), &[UintTy::U32], &[0]))];
    let b = bytes(&mut fx, &rows, &[]);
    fx.check(&b).unwrap_or_else(|e| panic!("{e}"));
}

#[test]
fn builtin_structural_rules() {
    // Clone of a tuple: one nested step per element, the shared `K: Clone` row once.
    let mut fx = fixture(
        "
case clone = cert builtin copy clone
struct K
impl@kc Clone for K
goal (K, K): Clone => yes
goal K: Clone => yes
",
        "",
    );
    fx.all_yes();
    let rows = [(1, by(K::Impl("kc"), &[], &[])), (0, by(K::Builtin(Builtin::CopyClone), &[], &[0, 0]))];
    let b = bytes(&mut fx, &rows, &[]);
    fx.check(&b).unwrap_or_else(|e| panic!("{e}"));

    // Sized through a tuple's last element and a struct's last field.
    let mut fx = fixture(
        "
case sized = cert builtin sized
struct W<T>(T)
goal (u32, W<u32>): Sized => yes
goal W<u32>: Sized => yes
goal u32: Sized => yes
",
        "",
    );
    fx.all_yes();
    let rows = [(2, by(SIZED, &[], &[])), (1, by(SIZED, &[], &[0])), (0, by(SIZED, &[], &[1]))];
    let b = bytes(&mut fx, &rows, &[]);
    fx.check(&b).unwrap_or_else(|e| panic!("{e}"));
}

#[test]
fn object_bounds() {
    let mut fx = fixture(
        "
case object = cert object bound
goal dyn Debug + Send: Send => yes
goal dyn Debug: Debug => yes
",
        "",
    );
    fx.all_yes();
    // The `+ Send` element's index in the `dyn`'s list.
    let g = fx.goal(0);
    let Pred::Trait { args, .. } = g else { panic!("not a trait goal") };
    let TyKind::Dynamic(preds, _) = fx.tys.kind(fx.tys.list_get(args, 0)) else { panic!("not a dyn") };
    let send = (0..preds.len)
        .find(|&i| matches!(fx.tys.kind(fx.tys.list_get(preds, i)), TyKind::ExAuto(d) if d == def_of("trait", "Send")))
        .unwrap_or_else(|| panic!("no + Send"));
    let b = bytes(&mut fx, &[(0, by(K::Object(send), &[], &[]))], &[]);
    fx.check(&b).unwrap_or_else(|e| panic!("{e}"));
    let b = bytes(&mut fx, &[(1, by(K::Object(0), &[], &[]))], &[]);
    fx.check(&b).unwrap_or_else(|e| panic!("{e}"));
    // The principal is not the auto trait's bound.
    if send != 0 {
        let b = bytes(&mut fx, &[(0, by(K::Object(0), &[], &[]))], &[]);
        assert_eq!(fx.check(&b), Err(CheckError::ObjectBound { step: StepId(0), index: 0 }));
    }
}

const Q: &str = "
case q = cert questions
trait Q
trait R
struct W<T>(T)
impl@q1 Q for W<u32>
goal W<u32>: Q => yes
";

/// The root's certificate in world `Q`: its bytes and its questions.
fn q_certificate() -> (StdVec<u8>, StdVec<Read>) {
    let mut fx = fixture(Q, "");
    let (a, solve_reads) = fx.solve(0);
    assert_eq!(a, Answer::Yes);
    let b = bytes(&mut fx, &[(0, by(K::Impl("q1"), &[], &[]))], &[]);
    let view = View::read(&b).unwrap_or_else(|e| panic!("{e}"));
    let mut checker = Checker::new_in(alloc());
    let checked = checker.check(view, &mut fx.tys, &fx.world, fx.solver.env_clauses(fx.env)).unwrap_or_else(|e| panic!("{e}"));
    let qs = Questions::from_solve(&solve_reads, &checked, alloc()).unwrap_or_else(|e| panic!("{e}"));
    assert!(!qs.reads().is_empty(), "a solve rests on at least its impl bucket");
    assert!(qs.stands(&fx.world).unwrap_or_else(|e| panic!("{e}")), "the questions stand in the world they were read in");
    let reads = qs.reads().to_vec();
    (b, reads)
}

fn questions(reads: &[Read]) -> Questions<'static> {
    let mut v: Vec<Read, Alloc<'static>> = Vec::new_in(alloc());
    v.extend_from_slice(reads);
    Questions::from_reads(v).unwrap_or_else(|e| panic!("{e}"))
}

#[test]
fn an_impl_that_could_match_moves_a_question() {
    let (_, reads) = q_certificate();
    let fx = fixture(Q, "impl@q2<T: R> Q for W<T>");
    let qs = questions(&reads);
    assert!(!qs.stands(&fx.world).unwrap_or_else(|e| panic!("{e}")));
}

#[test]
fn an_unrelated_impl_moves_none_and_still_checks() {
    let (b, reads) = q_certificate();
    let mut fx = fixture(Q, "impl@r1 R for u8");
    let qs = questions(&reads);
    assert!(qs.stands(&fx.world).unwrap_or_else(|e| panic!("{e}")));
    // In a fresh arena, by the rule a consumer applies.
    let view = View::read(&b).unwrap_or_else(|e| panic!("{e}"));
    let mut checker = Checker::new_in(alloc());
    let mut tys = TestTys::new();
    let t = today(&mut checker, &qs, view, &mut tys, &fx.world, fx.solver.env_clauses(fx.env)).unwrap_or_else(|e| panic!("{e:?}"));
    assert!(matches!(t, Today::Valid(_)), "{t:?}");
    // And the solver, run today, agrees.
    let (a, _) = fx.solve(0);
    assert_eq!(a, Answer::Yes);
}
