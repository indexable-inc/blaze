//! `where const { COND }` (`#![feature(blaze_comptime)]`) and generic const expressions
//! (`#![feature(blaze_const_exprs)]`), docs/v2/plan/design/ownership.md, as the type checker
//! and the evaluator decide them together: a condition is evaluated per instance, an impl
//! applies exactly when its condition is `true`, the error of the bound has the condition's own
//! reason, and a const argument that is an expression over generic parameters is one constant
//! with another by its value or its normal form.

use hir_def::{DefWithBodyId, FunctionId, ModuleDefId, nameres::crate_def_map, signatures::FunctionSignature};
use hir_expand::fixture;

use crate::{
    InferBodyId, InferenceDiagnostic, InferenceResult,
    next_solver::DbInterner,
    solver_errors::{SolverDiagnostic, SolverDiagnosticKind, StoredGoal},
    tests::consteval::reach::{BASE, STAND_INS, WALK},
    where_const::{self, Failure},
};

fn function(db: &base_db::Db, name: &str) -> FunctionId {
    crate_def_map(db, fixture::test_crate(db))
        .modules(db)
        .flat_map(|(_, module)| module.scope(db).declarations().collect::<Vec<_>>())
        .find_map(|def| match def {
            ModuleDefId::FunctionId(it) if FunctionSignature::of(db, it).name.as_str() == name => {
                Some(it)
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("the fixture's `{name}`"))
}

fn diagnostics<'db>(db: &'db base_db::Db, name: &str) -> &'db [InferenceDiagnostic] {
    let func = function(db, name);
    InferenceResult::of(db, InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(func)))
        .diagnostics()
}

fn with_db(fixture: &str, check: impl FnOnce(&base_db::Db)) {
    let db = fixture::db(fixture).into_db();
    blaze_query::db::attach(&db, || check(&db));
}

/// The body of `name` type-checks with no error.
#[track_caller]
fn accepted(db: &base_db::Db, name: &str) {
    let found = diagnostics(db, name);
    assert!(found.is_empty(), "`{name}`: {found:?}");
}

/// Why the one unmet bound in the body of `name` does not hold through an impl with a
/// condition: the trait bound's error, and the reason hir adds to it.
#[track_caller]
fn bound_refused(db: &base_db::Db, name: &str) -> Failure {
    let found = diagnostics(db, name);
    let unmet: Vec<_> = found
        .iter()
        .filter_map(|it| match it {
            InferenceDiagnostic::SolverDiagnostic(SolverDiagnostic {
                kind: SolverDiagnosticKind::TraitUnimplemented { trait_predicate, .. },
                ..
            }) => Some(trait_predicate),
            _ => None,
        })
        .collect();
    let [trait_predicate] = unmet[..] else {
        panic!("`{name}`: one unmet trait bound, not {found:?}");
    };
    let interner = DbInterner::new_with(db, fixture::test_crate(db));
    where_const::bound_refusal(interner, trait_predicate.get(interner).trait_ref)
        .unwrap_or_else(|| panic!("`{name}`: the bound's impl has a condition that fails"))
}

/// Why the one unmet condition in the body of `name`, a function's own, does not hold.
#[track_caller]
fn condition_refused(db: &base_db::Db, name: &str) -> Failure {
    let found = diagnostics(db, name);
    let unmet: Vec<_> = found
        .iter()
        .filter_map(|it| match it {
            InferenceDiagnostic::SolverDiagnostic(SolverDiagnostic {
                kind:
                    SolverDiagnosticKind::GoalUnmet { goal: StoredGoal::ConstEvaluatable { ct }, .. },
                ..
            }) => Some(ct),
            _ => None,
        })
        .collect();
    let [ct] = unmet[..] else {
        panic!("`{name}`: one unmet condition, not {found:?}");
    };
    let interner = DbInterner::new_with(db, fixture::test_crate(db));
    let ct = ct.as_ref().konst().expect("the condition is a constant");
    where_const::refusal(interner, ct)
        .unwrap_or_else(|| panic!("`{name}`: the condition fails when asked again"))
}

const SMALL: &str = r#"
//- minicore: sized, size_of, panic
#![feature(blaze_comptime, marker_trait_attr)]
const fn small<T>() -> bool { core::mem::size_of::<T>() <= 4 }
const fn fits<T>() -> bool {
    if core::mem::size_of::<T>() > 4 {
        panic!("{}", "the type is wider than 4 bytes");
    }
    true
}
#[marker] trait Small {}
impl<T> Small for T where const { small::<T>() } {}
#[marker] trait Fits {}
impl<T> Fits for T where const { fits::<T>() } {}
struct Asserted(u64);
impl Fits for Asserted {}

fn need_small<T: Small>() {}
fn need_fits<T: Fits>() {}
fn holds() { need_small::<u8>(); need_small::<u32>(); need_fits::<(u8, u8)>(); }
fn false_for_u64() { need_small::<u64>(); }
fn panics_for_u64() { need_fits::<u64>(); }
fn asserted() { need_fits::<Asserted>(); }
fn generic_unstated<T>() { need_small::<T>(); }
fn generic_stated<T: Small>() { need_small::<T>(); }
"#;

/// An impl with a condition applies to exactly the instances its condition is `true` for.
#[test]
fn an_impl_applies_exactly_when_its_condition_is_true() {
    with_db(SMALL, |db| {
        accepted(db, "holds");
        assert_eq!(bound_refused(db, "false_for_u64"), Failure::False);
    });
}

/// A condition that panics does not hold, and the panic's message is the reason the bound's
/// error shows.
#[test]
fn the_unmet_bound_shows_the_conditions_reason() {
    with_db(SMALL, |db| {
        let reason = bound_refused(db, "panics_for_u64");
        assert_eq!(reason, Failure::Refused("the type is wider than 4 bytes".to_owned()));
        assert_eq!(reason.text(), "the type is wider than 4 bytes");
    });
}

/// A `#[marker]` trait's explicit impl stands beside the derived one: the type the condition
/// refuses is asserted.
#[test]
fn an_explicit_impl_holds_where_the_condition_fails() {
    with_db(SMALL, |db| accepted(db, "asserted"));
}

/// Generic code cannot show a condition of its own parameter: it states the bound, as it
/// states any bound, and the caller with a concrete type meets it.
#[test]
fn a_generic_use_needs_the_bound_stated() {
    with_db(SMALL, |db| {
        accepted(db, "generic_stated");
        assert_eq!(bound_refused(db, "generic_unstated"), Failure::Generic);
    });
}

const CONDITIONS: &str = r#"
//- minicore: sized, int_bounds
#![feature(blaze_comptime, blaze_const_exprs)]
fn split<const A: usize, const B: usize, const N: usize>() where const { A >= 1 && B >= 1 && A + B == N } {}
fn positive<const N: usize>() where const { N >= 1 } {}
fn fits<const N: usize, const M: usize>() where const { N + M > 0 } {}

fn adds_up() { split::<1, 2, 3>(); positive::<7>(); }
fn does_not_add_up() { split::<1, 1, 3>(); }
fn stated<const N: usize>() where const { 1 <= N } { positive::<N>(); }
fn unstated<const N: usize>() { positive::<N>(); }
fn overflows() { fits::<{ usize::MAX }, 1>(); }
"#;

/// A function's own condition is asked with the arguments of each call.
#[test]
fn a_functions_condition_is_asked_per_call() {
    with_db(CONDITIONS, |db| {
        accepted(db, "adds_up");
        assert_eq!(condition_refused(db, "does_not_add_up"), Failure::False);
    });
}

/// A generic caller meets a callee's condition with its own, by the condition's normal form
/// (`N >= 1` is `1 <= N`), and not otherwise.
#[test]
fn a_callers_own_condition_meets_the_same_condition() {
    with_db(CONDITIONS, |db| {
        accepted(db, "stated");
        let found = diagnostics(db, "unstated");
        assert!(!found.is_empty(), "`positive::<N>()` with nothing known of `N`");
    });
}

/// An expression that overflows at an instantiation is that instantiation's error, with the
/// evaluator's message, never a wrapped value.
#[test]
fn an_overflow_at_an_instantiation_is_its_error() {
    with_db(CONDITIONS, |db| {
        let Failure::Refused(reason) = condition_refused(db, "overflows") else {
            panic!("the condition's evaluation panics");
        };
        assert!(reason.to_lowercase().contains("overflow"), "{reason}");
    });
}

const EXPRESSIONS: &str = r#"
//- minicore: sized
#![feature(blaze_const_exprs)]
struct Arr<const N: usize>;
fn join<const N: usize, const M: usize>(_: Arr<N>, _: Arr<M>) -> Arr<{ N + M }> { Arr }
fn make<const N: usize, const M: usize>() -> Arr<{ N + M }> { Arr }

fn concrete() { let _: Arr<3> = join(Arr::<1>, Arr::<2>); }
fn concrete_mismatch() { let _: Arr<4> = join(Arr::<1>, Arr::<2>); }
fn commuted<const N: usize, const M: usize>(a: Arr<N>, b: Arr<M>) -> Arr<{ M + N }> { join(a, b) }
fn folded<const N: usize>(a: Arr<N>, b: Arr<2>) -> Arr<{ 1 + N + 1 }> { join(a, b) }
fn generic_mismatch<const N: usize, const M: usize>(a: Arr<N>, b: Arr<M>) -> Arr<{ N * M }> {
    join(a, b)
}
fn guessed() { let _: Arr<3> = make(); }
"#;

/// With concrete arguments an expression is its value: `Arr<{ 1 + 2 }>` is `Arr<3>` and is
/// not `Arr<4>`.
#[test]
fn concrete_expressions_unify_by_evaluation() {
    with_db(EXPRESSIONS, |db| {
        accepted(db, "concrete");
        let found = diagnostics(db, "concrete_mismatch");
        assert!(!found.is_empty(), "`Arr<3>` given for `Arr<4>`");
    });
}

/// With generic arguments two expressions are one constant when they have one normal form:
/// `N + M` is `M + N`, `N + 2` is `1 + N + 1`, and `N + M` is not `N * M`.
#[test]
fn generic_expressions_unify_by_normal_form() {
    with_db(EXPRESSIONS, |db| {
        accepted(db, "commuted");
        accepted(db, "folded");
        let found = diagnostics(db, "generic_mismatch");
        assert!(!found.is_empty(), "`Arr<{{ N + M }}>` given for `Arr<{{ N * M }}>`");
    });
}

/// Nothing is guessed: `N + M == 3` names no `N` and `M`, so the call is an error asking for
/// them, never an instance picked.
#[test]
fn an_expression_is_never_solved_for_its_parameters() {
    with_db(EXPRESSIONS, |db| {
        let found = diagnostics(db, "guessed");
        assert!(!found.is_empty(), "`make()` with `N` and `M` not given");
    });
}

const RANGES: &str = r#"
//- minicore: sized, int_bounds
#![feature(blaze_comptime, blaze_const_exprs)]
struct Int<const LO: i128, const HI: i128>(i128);
fn new<const LO: i128, const HI: i128, const V: i128>() -> Int<LO, HI> where const { LO <= V && V <= HI } {
    Int(V)
}
fn add<const A: i128, const B: i128, const C: i128, const D: i128>(a: Int<A, B>, b: Int<C, D>) -> Int<{ A + C }, { B + D }> {
    Int(a.0 + b.0)
}

fn doubled() -> Int<0, 2147483646> { add(new::<0, 1073741823, 5>(), new::<0, 1073741823, 7>()) }
fn too_narrow() { let _: Int<0, 100> = add(new::<0, 50, 1>(), new::<0, 51, 1>()); }
fn too_wide() { let _ = add(new::<0, { i128::MAX }, 0>(), new::<0, 1, 0>()); }
fn out_of_range() { let _ = new::<0, 10, 11>(); }
"#;

/// `Int<LO, HI>` as a library over `where const` (docs/v2/plan/design/refined-values.md,
/// "`Int<LO, HI>`"): the sum of two `0..=i32::MAX/2` is `0..=i32::MAX - 1` by the types; a
/// range that does not contain the sum is a type error; a bound that overflows `i128` is an
/// error, never a wrapped range; a literal outside its range is refused by the condition.
#[test]
fn ranged_integers_are_checked_in_the_types() {
    with_db(RANGES, |db| {
        accepted(db, "doubled");
        assert!(!diagnostics(db, "too_narrow").is_empty(), "`Int<0, 101>` given for `Int<0, 100>`");
        assert!(!diagnostics(db, "too_wide").is_empty(), "`i128::MAX + 1` as a bound");
        assert_eq!(condition_refused(db, "out_of_range"), Failure::False);
    });
}

/// The whole of it: `Acyclic` derived by bstd's walk through a `where const` impl. A type the
/// walk clears meets the bound; one it refuses does not, and the bound's error has the path.
#[test]
fn acyclic_is_derived_and_its_refusal_names_the_path() {
    let items = r#"
unsafe impl<T: ?Sized> Acyclic for T where const { none::<T, Cycle>() } {}
struct List { tail: Option<Box<List>> }
struct Node { next: RefCell<Option<Rc<Node>>> }
struct Parent { kids: Vec<Rc<Child>> }
struct Child { parent: RefCell<Option<Rc<Parent>>> }
struct Trusted { raw: *mut u8 }
// SAFETY: the test's: the pointer owns nothing.
unsafe impl Acyclic for Trusted {}
fn need<T: Acyclic>() {}
fn list() { need::<List>(); need::<Trusted>(); }
fn node() { need::<Node>(); }
fn child() { need::<Child>(); }
"#;
    let base = BASE.replace(
        "#![feature(lang_items, marker_trait_attr)]",
        "#![feature(lang_items, marker_trait_attr, blaze_comptime)]",
    );
    with_db(&format!("{base}{WALK}{STAND_INS}{items}"), |db| {
        accepted(db, "list");
        assert_eq!(
            bound_refused(db, "node"),
            Failure::Refused(
                "`Node.next`: a `Rc<Node>` behind `RefCell<Option<Rc<Node>>>` reaches `Node`"
                    .to_owned()
            )
        );
        assert_eq!(
            bound_refused(db, "child"),
            Failure::Refused(
                "`Child.parent`: a `Rc<Parent>` behind `RefCell<Option<Rc<Parent>>>` reaches \
                 `Parent.kids: Vec<Rc<Child>>`, which reaches `Child`"
                    .to_owned()
            )
        );
    });
}

/// `Shares`' arithmetic as its signatures state it (`crates/blaze-std/src/own/shares.rs`), on
/// a type with the same parameters: parts split and join with the fractions in the types, a
/// join of parts of different wholes is a type error, and the value is taken only from the
/// handle whose parts are all of them.
#[test]
fn shares_arithmetic_is_checked_in_the_types() {
    let text = r#"
//- minicore: sized
#![feature(blaze_comptime, blaze_const_exprs)]
struct Part<const N: usize, const D: usize>;
fn split<const A: usize, const B: usize, const N: usize, const D: usize>(
    _: Part<N, D>,
) -> (Part<A, D>, Part<B, D>)
where
    const { A >= 1 && B >= 1 && A + B == N },
{
    (Part, Part)
}
fn join<const N: usize, const M: usize, const D: usize>(_: Part<N, D>, _: Part<M, D>) -> Part<{ N + M }, D> {
    Part
}
fn whole<const N: usize, const D: usize>(_: Part<N, D>) where const { N == D } {}

fn round_trip() {
    let (one, three) = split::<1, 3, 4, 4>(Part);
    let (two, last) = split::<2, 1, 3, 4>(three);
    let most: Part<3, 4> = join(one, two);
    whole(join(most, last));
}
fn uneven_split() { let _ = split::<1, 2, 4, 4>(Part); }
fn empty_part() { let _ = split::<0, 4, 4, 4>(Part); }
fn other_whole() { let _ = join(Part::<1, 2>, Part::<1, 3>); }
fn not_whole() { whole(Part::<1, 2>); }
"#;
    with_db(text, |db| {
        accepted(db, "round_trip");
        assert_eq!(condition_refused(db, "uneven_split"), Failure::False);
        assert_eq!(condition_refused(db, "empty_part"), Failure::False);
        assert_eq!(condition_refused(db, "not_whole"), Failure::False);
        let found = diagnostics(db, "other_whole");
        assert!(!found.is_empty(), "halves joined with thirds: `Part<1, 2>` and `Part<1, 3>`");
    });
}
