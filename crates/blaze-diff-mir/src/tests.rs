//! Lifting, lowering and the derivative body over fixtures: the lifted graph and the lowered
//! body against the MIR interpreter, the refusals by name, and
//! `#[blaze::differentiable(symbolic)]` end to end up to the body.

use base_db::Db;
use blaze_diff::{
    DiffError as RuleError, Op,
    eval::{Scalar, eval, same},
};
use hegel::generators as gs;
use hir_def::FunctionId;
use hir_expand::fixture;
use hir_ty_mir::mir::{
    MirBody, MirSpan, Operand, Place, Rvalue, Statement, StatementKind, interpret_mir,
};

use crate::{
    DiffError, NotLifted, NotLowered, derivative_body, derived, explain, lift, lower,
    surface::body_of,
};

/// The float intrinsics as `core::intrinsics` declares them, in the fixture's own crate
/// (minicore has none), and the functions the tests lift.
const FIXTURE: &str = r#"
//- minicore: sized, copy
#[rustc_intrinsic]
pub fn sin<T>(x: T) -> T;
#[rustc_intrinsic]
pub fn cos<T>(x: T) -> T;
#[rustc_intrinsic]
pub fn exp<T>(x: T) -> T;
#[rustc_intrinsic]
pub fn log<T>(x: T) -> T;
#[rustc_intrinsic]
pub fn sqrt<T>(x: T) -> T;
#[rustc_intrinsic]
pub fn fabs<T>(x: T) -> T;
#[rustc_intrinsic]
pub fn powf<T>(x: T, y: T) -> T;
#[rustc_intrinsic]
pub fn powi<T>(x: T, n: i32) -> T;
#[rustc_intrinsic]
pub fn fma<T>(a: T, b: T, c: T) -> T;

pub fn plan(x: f64, y: f64) -> f64 {
    sin(x * y) / (1.0 + x * x)
}

pub fn straight(x: f64, y: f64) -> f64 {
    let t = x * y - 3.0;
    let u = t * t;
    fma(u, y, sqrt(fabs(t) + 1.0)) / (exp(-x) + powi(y, 3)) + powf(u, 0.5) * log(u)
}

pub fn branchy(x: f64, y: f64) -> f64 {
    let big = if x > y { x * 2.0 } else { y - x };
    if big <= 1.5 { big * big } else { cos(big) }
}

pub fn looped(x: f64, y: f64) -> f64 {
    let mut s = y;
    let mut i = 0;
    while i < 4 {
        s = s * x + 1.0;
        i = i + 1;
    }
    s
}

pub fn pair(x: f64, y: f64) -> (f64, f64) {
    (x * y, x + y)
}

pub fn narrow(x: f32, y: f32) -> f32 {
    let t = x * 1.5 - y;
    if t < 0.25 { sqrt(t * t + y) } else { t / y }
}

pub fn spins(x: f64) -> f64 {
    let mut s = x;
    while s < 100.0 {
        s = s * 2.0;
    }
    s
}

pub fn long_loop(x: f64) -> f64 {
    let mut s = x;
    let mut i = 0;
    while i < 1000 {
        s = s + x;
        i = i + 1;
    }
    s
}

fn helper(x: f64) -> f64 {
    x + 1.0
}

pub fn calls_helper(x: f64) -> f64 {
    helper(x) * 2.0
}

pub fn through_a_reference(x: f64) -> f64 {
    let r = &x;
    *r + 1.0
}

pub fn of_an_integer(n: u32) -> f64 {
    1.0
}

#[blaze::pure]
#[blaze::differentiable(symbolic)]
pub fn smooth(x: f64, y: f64) -> f64 {
    sin(x * y) * cos(x) / (1.0 + x * x)
}

#[blaze::pure]
#[blaze::differentiable(symbolic, fast_math)]
pub fn smooth_fast(x: f64, y: f64) -> f64 {
    sin(x * y) * cos(x) / (1.0 + x * x)
}

#[blaze::pure]
#[blaze::differentiable(symbolic)]
pub fn marked_plan(x: f64, y: f64) -> f64 {
    sin(x * y) / (1.0 + x * x)
}

#[blaze::pure]
#[blaze::differentiable(symbolic)]
pub fn marked_spins(x: f64) -> f64 {
    let mut s = x;
    while s < 100.0 {
        s = s * 2.0;
    }
    s
}

#[blaze::pure]
#[blaze::differentiable(symbolic)]
pub fn marked_abs(x: f64) -> f64 {
    fabs(x) * x
}

#[blaze::differentiable(symbolic)]
pub fn not_marked_pure(x: f64) -> f64 {
    x * x
}

#[blaze::pure]
#[blaze::differentiable]
pub fn not_symbolic(x: f64) -> f64 {
    x * x
}

#[blaze::pure]
#[blaze::differentiable(reverse)]
pub fn malformed(x: f64) -> f64 {
    x * x
}

#[blaze::pure]
pub fn not_differentiable(x: f64) -> f64 {
    x * x
}
"#;

/// Runs `check` on the fixture's database.
#[track_caller]
fn with_db<T>(check: impl FnOnce(&Db) -> T) -> T {
    let db = fixture::db(FIXTURE).into_db();
    blaze_query::db::attach(&db, || check(&db))
}

#[track_caller]
fn function(db: &Db, name: &str) -> FunctionId {
    hir_def::test_items::function(db, fixture::test_crate(db), name)
}

#[track_caller]
fn body<'db>(db: &'db Db, name: &str) -> &'db MirBody<'db> {
    body_of(db, function(db, name)).unwrap_or_else(|error| panic!("{error}"))
}

/// What `body` returns given `args`, as the MIR interpreter runs it: its parameters assigned
/// the arguments at the start, as a body of no parameters.
#[track_caller]
fn interpreted<'db, F: Scalar>(db: &'db Db, body: &MirBody<'db>, args: &[F]) -> Vec<F> {
    let mut body = body.clone();
    let size = std::mem::size_of::<F>();
    let assigned: Vec<Statement> = body
        .param_locals
        .iter()
        .zip(args)
        .map(|(&local, arg)| {
            let ty = body.locals[local].ty.as_ref();
            let bytes = arg.to_payload().to_le_bytes()[..size].to_vec().into_boxed_slice();
            Statement {
                kind: StatementKind::Assign(
                    Place::from(local).store(),
                    Rvalue::Use(Operand::from_bytes(bytes, ty)),
                ),
                span: MirSpan::Unknown,
            }
        })
        .collect();
    let start = body.start_block;
    for (at, statement) in assigned.into_iter().enumerate() {
        body.basic_blocks[start].statements.insert(at, statement);
    }
    body.param_locals.clear();
    let body: &'db MirBody<'db> = Box::leak(Box::new(body));
    let (value, _) = interpret_mir(db, body, None).expect("the body runs");
    let memory = value.expect("the body returns").memory.clone();
    memory
        .chunks_exact(size)
        .map(|chunk| {
            let mut bytes = [0u8; 8];
            bytes[..size].copy_from_slice(chunk);
            F::from_payload(u64::from_le_bytes(bytes))
        })
        .collect()
}

/// The oracle: the lifted graph of the function `name` evaluates at `point` to what the
/// interpreter computes from its body, and so does the body lowered from the graph: the same
/// bits, any NaN being any NaN. With `planted`, the graph is first changed by the bug it names.
fn refines<F: Scalar>(db: &Db, name: &str, point: &[F], planted: Planted) -> Result<(), String> {
    let body = body(db, name);
    let mut lifted = lift(db, body).map_err(|why| format!("`{name}` did not lift: {why}"))?;
    if planted == Planted::SwapsSelectArms {
        // The arms of the result's `select` the wrong way round.
        let node = lifted.graph.node(lifted.roots[0]);
        if node.op != Op::Select {
            return Err(format!("the result of `{name}` is no `select` to plant the bug in"));
        }
        lifted.roots[0] = lifted.graph.select(node.args[0], node.args[2], node.args[1]);
    }
    let expected = interpreted(db, body, point);
    let found = eval::<F>(&lifted.graph, &lifted.roots, point).map_err(|it| it.to_string())?;
    let lowered = lower(db, &lifted.graph, &lifted.roots, &lifted.signature, &lifted.callees)
        .map_err(|why| format!("`{name}` did not lower: {why}"))?;
    let rerun = interpreted(db, &lowered, point);
    for (what, values) in [("the lifted graph", &found), ("the lowered body", &rerun)] {
        let equal = values.len() == expected.len()
            && values.iter().zip(&expected).all(|(&found, &expected)| same(found, expected));
        if !equal {
            return Err(format!(
                "`{name}` at {point:?}: the body returns {expected:?}, and {what} {values:?}"
            ));
        }
    }
    Ok(())
}

/// A bug planted in a lifted graph, for the control of [`refines`].
#[derive(Clone, Copy, PartialEq, Eq)]
enum Planted {
    None,
    SwapsSelectArms,
}

const SPECIAL: &[f64] =
    &[0.0, -0.0, 1.0, -1.5, 2.5, f64::INFINITY, f64::NEG_INFINITY, f64::NAN, 5e-324, 1e300];
const LIFTABLE: &[&str] = &["plan", "straight", "branchy", "looped", "pair"];

/// The refinement: for a body that lifts, the lifted expression graph evaluates to the body's
/// own result, bit for bit, at every input, and the body lowered from the graph does too:
/// here at every pair of special values (zeros of both signs, infinities, NaN, a subnormal),
/// for straight-line code, calls of intrinsics, branches, an unrolled loop and a tuple result.
#[test]
fn lifting_and_lowering_keep_what_the_body_computes_on_special_values() {
    with_db(|db| {
        for name in LIFTABLE {
            for &x in SPECIAL {
                for &y in SPECIAL {
                    if let Err(message) = refines(db, name, &[x, y], Planted::None) {
                        panic!("{message}");
                    }
                }
            }
        }
        for (x, y) in
            [(0.5f32, 2.0f32), (-1.0, 0.0), (f32::NAN, 1.0), (1e-45, -0.0), (3.0, f32::INFINITY)]
        {
            if let Err(message) = refines(db, "narrow", &[x, y], Planted::None) {
                panic!("{message}");
            }
        }
    });
}

/// The refinement: the same at drawn inputs of any bits.
#[hegel::test(test_cases = 12)]
fn lifting_and_lowering_keep_what_the_body_computes_on_any_bits(tc: hegel::TestCase) {
    let name =
        LIFTABLE[tc.draw(gs::integers::<u32>().max_value(LIFTABLE.len() as u32 - 1)) as usize];
    let mut points = Vec::new();
    for _ in 0..6 {
        let x = f64::from_bits(tc.draw(gs::integers::<u64>()));
        let y = f64::from_bits(tc.draw(gs::integers::<u64>()));
        points.push([x, y]);
    }
    with_db(|db| {
        for point in &points {
            if let Err(message) = refines(db, name, point, Planted::None) {
                panic!("{message}");
            }
        }
    });
}

#[test]
fn the_oracle_catches_a_select_with_its_arms_swapped() {
    with_db(|db| {
        // x > y picks the first arm, and the two arms differ at (3, 1).
        assert_eq!(refines(db, "branchy", &[3.0f64, 1.0], Planted::None), Ok(()));
        let verdict = refines(db, "branchy", &[3.0f64, 1.0], Planted::SwapsSelectArms);
        assert!(verdict.is_err(), "the oracle let swapped arms through");
    });
}

#[test]
fn what_does_not_lift_is_named() {
    with_db(|db| {
        let refused = |name: &str| match lift(db, body(db, name)) {
            Err(why) => why,
            Ok(lifted) => panic!("`{name}` lifted: {:?}", lifted.graph),
        };
        // A loop whose trip count depends on a float, and one too long to unroll.
        assert!(
            matches!(refused("spins"), NotLifted::Loop { turns: None, .. }),
            "{:?}",
            refused("spins")
        );
        assert!(
            matches!(
                refused("long_loop"),
                NotLifted::Loop { turns: Some(crate::lift::MAX_UNROLL), .. }
            ),
            "{:?}",
            refused("long_loop")
        );
        // A call of a function the rule table does not know, by its name.
        match refused("calls_helper") {
            NotLifted::Call { callee, .. } => assert_eq!(callee, "`helper`"),
            other => panic!("{other:?}"),
        }
        assert!(matches!(refused("through_a_reference"), NotLifted::Memory { .. }));
        assert!(matches!(refused("of_an_integer"), NotLifted::Type { .. }));
        // Each says what layer 1 would do.
        for name in ["spins", "calls_helper", "through_a_reference", "of_an_integer"] {
            assert!(!refused(name).layer_1().is_empty());
        }
    });
}

#[test]
fn the_unrolled_loop_is_a_closed_form() {
    with_db(|db| {
        let lifted = lift(db, body(db, "looped")).unwrap_or_else(|why| panic!("{why}"));
        // (((y * x + 1) * x + 1) * x + 1) * x + 1: four turns, no condition left.
        let reached = lifted.graph.reached(&lifted.roots);
        let count = |op: Op| {
            lifted
                .graph
                .nodes()
                .iter()
                .enumerate()
                .filter(|(at, it)| reached[*at] && it.op == op)
                .count()
        };
        assert_eq!(
            (count(Op::Mul), count(Op::Add), count(Op::Select)),
            (4, 4, 0),
            "{:?}",
            lifted.graph
        );
        assert_eq!(
            blaze_diff::print::math(&lifted.graph, lifted.roots[0], &["x", "y"]),
            "(((y * x + 1) * x + 1) * x + 1) * x + 1"
        );
    });
}

/// d/dx of `smooth`, by hand.
fn smooth_dx(x: f64, y: f64) -> f64 {
    let (s, c, d) = ((x * y).sin(), x.cos(), 1.0 + x * x);
    ((y * (x * y).cos() * c - s * x.sin()) * d - s * c * 2.0 * x) / (d * d)
}

#[test]
fn a_symbolic_function_has_a_derivative_body() {
    with_db(|db| {
        for (name, tolerance) in [("smooth", 1e-12), ("smooth_fast", 1e-9)] {
            let function = function(db, name);
            let derivative =
                derivative_body(db, function, 0).unwrap_or_else(|error| panic!("{error}"));
            assert_eq!(derivative.param_locals.len(), 2);
            let derived = derived(db, function, 0).unwrap_or_else(|error| panic!("{error}"));
            assert_eq!(derived.fast_math, name == "smooth_fast");
            for (x, y) in [(0.5, 0.75), (1.25, -0.5), (-2.0, 1.5), (0.1, 3.0)] {
                let ran = interpreted::<f64>(db, &derivative, &[x, y]);
                let expected = smooth_dx(x, y);
                assert!(
                    (ran[0] - expected).abs() <= tolerance * expected.abs().max(1.0),
                    "`{name}` at ({x}, {y}): the body returns {:e}, the derivative is {expected:e}",
                    ran[0]
                );
                // The body is the derived graph, lowered.
                let graph = &derived.derivative;
                let evaluated = eval::<f64>(&graph.graph, &graph.roots, &[x, y]).unwrap();
                assert!(same(ran[0], evaluated[0]), "{} and {}", ran[0], evaluated[0]);
            }
        }
        // By the second parameter too, and not by a third.
        let function = function(db, "smooth");
        assert!(derivative_body(db, function, 1).is_ok());
        assert!(matches!(
            derivative_body(db, function, 2),
            Err(DiffError::NoSuchParam { wrt: 2, params: 2, .. })
        ));
    });
}

#[test]
fn explain_prints_the_derivative_and_how_it_was_simplified() {
    with_db(|db| {
        let function = function(db, "marked_plan");
        let text = explain(db, function, 0, &["x", "y"]).unwrap_or_else(|error| panic!("{error}"));
        assert!(text.starts_with("d/dx marked_plan\n  "), "{text}");
        assert!(text.contains("cos("), "{text}");
        assert!(text.contains("simplification (exact rewrites only)"), "{text}");
        // The same text twice.
        assert_eq!(text, explain(db, function, 0, &["x", "y"]).unwrap());
        // The derivative calls `cos`, which the function does not, and this fixture's `core`
        // has no `core::intrinsics::cos` to call: a typed refusal naming the operation.
        assert_eq!(
            derivative_body(db, function, 0).map(|_| ()),
            Err(DiffError::NotLowered {
                function: "marked_plan".to_owned(),
                why: NotLowered::NoCallee { op: Op::Cos },
            })
        );
    });
}

#[test]
fn a_function_without_a_symbolic_derivative_says_why() {
    with_db(|db| {
        let error = |name: &str| match derivative_body(db, function(db, name), 0) {
            Err(error) => error,
            Ok(_) => panic!("`{name}` has a derivative body"),
        };
        assert!(matches!(error("not_differentiable"), DiffError::NotDifferentiable { .. }));
        assert!(matches!(error("not_symbolic"), DiffError::NotSymbolic { .. }));
        assert!(
            matches!(error("malformed"), DiffError::Malformed { .. }),
            "{:?}",
            error("malformed")
        );
        assert!(matches!(error("not_marked_pure"), DiffError::NotPure { .. }));

        // Not liftable: the loop is named, and so is what layer 1 would do; nothing switches.
        let spins = error("marked_spins");
        assert!(
            matches!(&spins, DiffError::NotLifted { why: NotLifted::Loop { turns: None, .. }, .. }),
            "{spins:?}"
        );
        let said = spins.to_string();
        assert!(said.contains("`marked_spins` has no symbolic derivative"), "{said}");
        assert!(said.contains("a loop whose number of turns depends on a value"), "{said}");
        assert!(said.contains("blaze does not switch to it on its own"), "{said}");

        // A kink on the variable's way: the rule's typed error, by default.
        match error("marked_abs") {
            DiffError::Rule { error: RuleError::NonDifferentiable { op, point, .. }, .. } => {
                assert_eq!((op, point), (Op::Abs, "0"));
            }
            other => panic!("{other:?}"),
        }
    });
}
