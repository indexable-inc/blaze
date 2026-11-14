//! The structured-MIR refinement and its controls, the folds, the printer, golden tests of
//! the emitted Lean (each a module under `lean/BlazeModel`, which `lake build` type-checks),
//! the refusals, the first proof's statement, and the embedded MIR (`body::embed`): its golden
//! bodies under `lean/BlazeMir`, the passes held to their Lean specifications
//! (`lean/BlazeValidated`), and the corpus harness/v2/mir_semantics.py runs three ways.
//!
//! A golden's file is what the translator emits, byte for byte. When the translator changes
//! on purpose, `UPDATE_EXPECT=1` rewrites the files; then `lake build` in `lean/` must still
//! pass, and a proof over a changed model is checked again.

use std::collections::BTreeMap;

use base_db::{Crate, Db, all_crates};
use expect_test::expect_file;
use blaze_sim::{Broke, Config, Tape};
use hir_def::{FunctionId, ModuleDefId, nameres::crate_def_map, signatures::FunctionSignature};
use hir_expand::fixture;
// Links the crate that provides the MIR queries.
use hir_ty_mir as _;

use hir_def::{DefWithBodyId, HasModule as _};
use hir_ty::db::HirDatabase as _;
use hir_ty_mir::mir::budget::{self, Pass};

use crate::{
    Emitted, body::*, emit, embed_mir, embed_mir_body, embed_mir_body_as, lean, plan,
    structured::{
        self, Cfg, CfgBlock, End, Ended, Exit, Machine, Node, Plant, ill_formed, lower,
        reconstruct, reconstruct_planted, run_cfg, run_structured,
    },
};

// ---- The structured-MIR refinement, on a small language. ----

#[derive(Clone, Debug, PartialEq, Eq)]
enum Op {
    Add(usize, u64),
    Set(usize, u64),
    Trace(u64),
    PanicIf(usize, u64),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Test {
    /// The variable modulo 3.
    Low(usize),
    /// Whether the variable is below the number.
    Less(usize, u64),
}

type Program = Vec<Node<Op, Test>>;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Vm {
    vars: [u64; 3],
    trace: Vec<u64>,
}

impl Machine<Op, Test> for Vm {
    fn step(&mut self, statement: &Op) -> Result<(), End> {
        match *statement {
            Op::Add(var, by) => self.vars[var] = self.vars[var].wrapping_add(by),
            Op::Set(var, to) => self.vars[var] = to,
            Op::Trace(what) => self.trace.push(what.wrapping_add(self.vars[0])),
            Op::PanicIf(var, at) => {
                if self.vars[var] == at {
                    return Err(End::Panic);
                }
            }
        }
        Ok(())
    }

    fn test(&mut self, on: &Test) -> u128 {
        match *on {
            Test::Low(var) => u128::from(self.vars[var] % 3),
            Test::Less(var, than) => u128::from(self.vars[var] < than),
        }
    }
}

/// A structured program whose every jump has a label of its kind: `labels` are those in
/// scope, `true` for a loop.
fn program(tape: &Tape, depth: u32, labels: &mut Vec<bool>) -> Program {
    let mut out = Vec::new();
    for _ in 0..tape.int_in(0..=4u64, "nodes") {
        let var = tape.int_in(0..=2u64, "var") as usize;
        let kind = if depth == 0 { tape.int_in(0..=4u64, "kind") } else { tape.int_in(0..=11u64, "kind") };
        match kind {
            0 => out.push(Node::Do(Op::Add(var, tape.int_in(1..=3, "add")))),
            1 => out.push(Node::Do(Op::Set(var, tape.int_in(0..=5, "set")))),
            2 | 3 => out.push(Node::Do(Op::Trace(tape.int_in(0..=9, "trace")))),
            4 => out.push(Node::Do(Op::PanicIf(var, tape.int_in(0..=40, "panic at")))),
            5 | 6 => {
                let on = if tape.int_in(0..=1u64, "test") == 0 {
                    Test::Low(var)
                } else {
                    Test::Less(var, tape.int_in(1..=6, "less than"))
                };
                let arms = (0..tape.int_in(1..=2u64, "arms"))
                    .map(|value| (u128::from(value), program(tape, depth - 1, labels)))
                    .collect();
                out.push(Node::Branch { on, arms, otherwise: program(tape, depth - 1, labels) });
            }
            7 => {
                labels.push(false);
                out.push(Node::Block(program(tape, depth - 1, labels)));
                labels.pop();
            }
            8 => {
                // The loop is in a block, which a `Break` in its body leaves.
                labels.push(false);
                labels.push(true);
                let mut body = program(tape, depth - 1, labels);
                labels.pop();
                labels.pop();
                // A turn moves a variable, so most loops end.
                body.push(Node::Do(Op::Add(var, 1)));
                out.push(Node::Block(vec![Node::Branch {
                    on: Test::Less(var, tape.int_in(2..=8, "loop bound")),
                    arms: vec![(0, vec![Node::Break(0)])],
                    otherwise: vec![Node::Loop(body)],
                }]));
            }
            9 | 10 => {
                // A jump to a label of the kind drawn, when one is in scope; it ends the
                // sequence, as nothing after it runs.
                let wants_loop = kind == 10;
                let found = labels.iter().rev().position(|is_loop| *is_loop == wants_loop);
                if let Some(to) = found {
                    out.push(if wants_loop { Node::Continue(to as u32) } else { Node::Break(to as u32) });
                    return out;
                }
            }
            _ => {
                out.push(if tape.int_in(0..=3u64, "end") == 0 { Node::End(End::Panic) } else { Node::Return });
                return out;
            }
        }
    }
    out
}

const STEPS: u64 = 4_000;

fn run_graph(cfg: &Cfg<Op, Test>, vars: [u64; 3]) -> (Ended, Vm) {
    let mut vm = Vm { vars, trace: Vec::new() };
    let ended = run_cfg(cfg, &mut vm, STEPS);
    (ended, vm)
}

fn run_tree(nodes: &[Node<Op, Test>], vars: [u64; 3]) -> (Ended, Vm) {
    let mut vm = Vm { vars, trace: Vec::new() };
    // The tree counts a step per node, the graph per statement and jump: room for both.
    let ended = run_structured(nodes, &mut vm, STEPS * 16);
    (ended, vm)
}

/// The oracle: `written` lowered to a graph and reconstructed (with `plant`) runs, from each
/// of a few states, as the graph does and as `written` does; what comes back is well formed
/// and in normal form.
fn agrees(written: &Program, plant: Plant) -> Result<(), String> {
    let cfg = lower(written);
    let rebuilt = reconstruct_planted(&cfg, plant).map_err(|err| err.to_string())?;
    // A planted bug is for the runs below to catch, not the shape checks.
    if plant == Plant::None
        && let Some(wrong) = ill_formed(&rebuilt)
    {
        return Err(format!("ill-formed: {wrong}\n{rebuilt:?}"));
    }
    for vars in [[0, 0, 0], [1, 2, 3], [5, 0, 7], [2, 9, 4]] {
        let graph = run_graph(&cfg, vars);
        if graph.0 == Ended::OutOfSteps {
            continue;
        }
        let tree = run_tree(&rebuilt, vars);
        if tree != graph {
            return Err(format!(
                "from {vars:?} the graph gives {graph:?} and the reconstruction {tree:?}\n{rebuilt:?}"
            ));
        }
        let source = run_tree(written, vars);
        if source != graph {
            return Err(format!(
                "from {vars:?} the graph gives {graph:?} and what was written {source:?}"
            ));
        }
    }
    if plant != Plant::None {
        return Ok(());
    }
    let again = reconstruct(&lower(&rebuilt)).map_err(|err| err.to_string())?;
    if again != rebuilt {
        return Err(format!("not a normal form:\n{rebuilt:?}\nreconstructs to\n{again:?}"));
    }
    Ok(())
}

/// The refinement: structured control flow reconstructed from a control-flow graph runs as
/// the graph does, from every state, to the same end, variables and trace; and what
/// `reconstruct` returns is well formed and a fixed point of lowering and reconstructing.
#[test]
fn structured_code_runs_as_its_graph() {
    blaze_sim::check(Config::cases(300), |tape| {
        let written = program(tape, 3, &mut Vec::new());
        if let Err(broke) = agrees(&written, Plant::None) {
            panic!("{broke}\nwritten {written:?}");
        }
    })
}

const CONTROL_SEEDS: u32 = 400;

/// One drawn program held to the oracle with `plant`: any finding is the oracle's.
fn agrees_drawn(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let written = program(tape, 3, &mut Vec::new());
    agrees(&written, plant).map_err(|broke| Broke::Oracle(format!("{broke}\nwritten {written:?}")))
}

#[test]
fn the_seeded_programs_agree_unplanted() {
    for seed in 0..u64::from(CONTROL_SEEDS) {
        let written = program(&Tape::seeded(seed), 3, &mut Vec::new());
        if let Err(broke) = agrees(&written, Plant::None) {
            panic!("seed {seed}: {broke}\nwritten {written:?}");
        }
    }
}

#[test]
fn the_oracle_catches_a_continue_made_a_break() {
    // Any finding catches it, as before: `says` is empty.
    blaze_sim::control(Config::cases(CONTROL_SEEDS), Plant::None, Plant::ContinueAsBreak, "", agrees_drawn);
}

#[test]
fn the_oracle_catches_an_else_branch_dropped_at_a_join() {
    blaze_sim::control(Config::cases(CONTROL_SEEDS), Plant::None, Plant::ElseDropped, "", agrees_drawn);
}

#[test]
fn an_irreducible_graph_is_refused_naming_the_edge() {
    // 0 branches into the middle of the cycle 1 <-> 2 at both blocks.
    let goto = |to| CfgBlock { statements: Vec::<Op>::new(), exit: Exit::<Test>::Goto(to) };
    let cfg = Cfg {
        blocks: vec![
            CfgBlock {
                statements: Vec::new(),
                exit: Exit::Switch { on: Test::Low(0), targets: vec![(0, 1)], otherwise: 2 },
            },
            goto(2),
            goto(1),
        ],
        entry: 0,
    };
    let err = reconstruct(&cfg).expect_err("a loop with two entries");
    assert!(matches!(err, structured::StructureError::Irreducible { .. }), "{err}");
    assert!(err.to_string().contains("irreducible"), "{err}");
}

#[test]
fn a_while_loop_is_a_block_around_a_loop_with_what_follows_after_it() {
    // 0: goto 1; 1: if v0 < 3 then 2 else 3; 2: v0 += 1, goto 1; 3: trace, return.
    let cfg = Cfg {
        blocks: vec![
            CfgBlock { statements: vec![Op::Set(0, 0)], exit: Exit::Goto(1) },
            CfgBlock {
                statements: Vec::new(),
                exit: Exit::Switch { on: Test::Less(0, 3), targets: vec![(0, 3)], otherwise: 2 },
            },
            CfgBlock { statements: vec![Op::Add(0, 1)], exit: Exit::Goto(1) },
            CfgBlock { statements: vec![Op::Trace(7)], exit: Exit::Return },
        ],
        entry: 0,
    };
    let nodes = reconstruct(&cfg).expect("reducible");
    assert_eq!(
        nodes,
        vec![
            Node::Do(Op::Set(0, 0)),
            Node::Block(vec![Node::Loop(vec![Node::Branch {
                on: Test::Less(0, 3),
                arms: vec![(0, vec![Node::Break(1)])],
                otherwise: vec![Node::Do(Op::Add(0, 1))],
            }])]),
            Node::Do(Op::Trace(7)),
            Node::Return,
        ]
    );
}

// ---- The folds of runtime MIR's checks. ----

fn local(local: Local) -> Operand {
    Operand::Place(Place::local(local))
}

fn field(local: Local, index: u32) -> Operand {
    Operand::Place(Place { local, elems: vec![Elem::TupleField { index, arity: 2 }] })
}

fn assign(to: Local, value: Rvalue) -> Node<Stmt, Cond> {
    Node::Do(Stmt::Assign { place: Place::local(to), value, at: String::new() })
}

#[test]
fn an_overflowing_add_its_assert_and_its_result_fold_to_one_checked_add() {
    let nodes = vec![
        assign(
            3,
            Rvalue::WithOverflow { op: BinOp::Add, ty: IntTy::U32, left: local(1), right: local(2) },
        ),
        Node::Do(Stmt::Assert {
            cond: field(3, 1),
            expected: false,
            check: Check::Overflow,
            at: String::new(),
        }),
        assign(0, Rvalue::Use(field(3, 0))),
        Node::Return,
    ];
    assert_eq!(
        fold(nodes),
        vec![
            assign(
                0,
                Rvalue::Bin {
                    op: BinOp::Add,
                    ty: OpTy::Int(IntTy::U32),
                    checked: true,
                    left: local(1),
                    right: local(2),
                },
            ),
            Node::Return,
        ]
    );
}

#[test]
fn a_bounds_assert_folds_into_the_index_after_it_and_stays_without_one() {
    let slice = Place { local: 1, elems: vec![Elem::Deref] };
    let check = |next: Node<Stmt, Cond>| {
        vec![
            assign(3, Rvalue::Len(slice.clone())),
            assign(
                4,
                Rvalue::Bin {
                    op: BinOp::Lt,
                    ty: OpTy::Int(IntTy::Usize),
                    checked: false,
                    left: local(2),
                    right: local(3),
                },
            ),
            Node::Do(Stmt::Assert {
                cond: local(4),
                expected: true,
                check: Check::Bounds,
                at: String::new(),
            }),
            next,
        ]
    };
    let read = assign(
        0,
        Rvalue::Use(Operand::Place(Place { local: 1, elems: vec![Elem::Deref, Elem::Index(2)] })),
    );
    assert_eq!(fold(check(read.clone())), vec![read]);
    let other = assign(0, Rvalue::Use(local(2)));
    assert_eq!(fold(check(other.clone())).len(), 4, "the assert stays: nothing after it indexes");
}

#[test]
fn a_discriminant_read_and_the_switch_on_it_fold_to_a_match() {
    let option = Ty::Option(Box::new(Ty::Int(IntTy::U32)));
    let nodes = vec![
        assign(2, Rvalue::Discriminant(Place::local(1), option.clone())),
        Node::Branch {
            on: Cond::Operand(local(2), OpTy::Int(IntTy::Isize)),
            arms: vec![(0, vec![Node::Return])],
            otherwise: vec![Node::Return],
        },
    ];
    assert_eq!(
        fold(nodes),
        vec![Node::Branch {
            on: Cond::Discriminant(Place::local(1), option),
            arms: vec![(0, vec![Node::Return])],
            otherwise: vec![Node::Return],
        }]
    );
}

// ---- The printer. ----

#[test]
fn names_are_escaped_and_operators_keep_their_reading() {
    use lean::{Infix, Term, app, atom, infix, name};
    assert_eq!(lean::ident("main.at.x"), "main.«at».x");
    assert_eq!(lean::ident("r#type"), "«r#type»");
    let sum = infix(Infix::Add, name("a"), infix(Infix::Mul, name("b"), name("c")));
    assert_eq!(lean::inline(&sum), "a + b * c");
    let product = infix(Infix::Mul, infix(Infix::Add, name("a"), name("b")), name("c"));
    assert_eq!(lean::inline(&product), "(a + b) * c");
    let chain = infix(
        Infix::Implies,
        infix(Infix::Lt, name("a"), name("b")),
        infix(Infix::And, name("p"), infix(Infix::And, name("q"), name("r"))),
    );
    assert_eq!(lean::inline(&chain), "a < b → p ∧ q ∧ r");
    let call = app("f", vec![atom("-5"), app("g", vec![name("x")]), Term::Tuple(vec![])]);
    assert_eq!(lean::inline(&call), "f (-5) (g x) ()");
    let read = Term::Proj(Box::new(app("Slice.len", vec![name("at")])), "val".to_owned());
    assert_eq!(lean::inline(&infix(Infix::Ne, read, atom("0"))), "(Slice.len «at»).val ≠ 0");
    let block = Term::If(
        Box::new(name("c")),
        Box::new(Term::Do(vec![
            lean::DoElem::Bind(lean::Pat::Name("x".to_owned()), app("f", vec![name("y")])),
            lean::DoElem::Term(app("Result.ok", vec![name("x")])),
        ])),
        Box::new(app("Result.ok", vec![name("y")])),
    );
    assert_eq!(lean::lines(&block), "if c then do\n  let x ← f y\n  Result.ok x\nelse Result.ok y");
}

// ---- Emission, from source. ----

fn db(source: &str) -> Db {
    fixture::db(source).into_db()
}

#[track_caller]
fn function(db: &Db, krate: &str, function: &str) -> FunctionId {
    let krate: Crate = all_crates(db)
        .iter()
        .copied()
        .find(|it| {
            it.extra_data(db)
                .display_name
                .as_ref()
                .is_some_and(|name| name.crate_name().as_str() == krate)
        })
        .expect("the fixture has the crate");
    for (_, data) in crate_def_map(db, krate).modules(db) {
        for def in data.scope(db).declarations() {
            if let ModuleDefId::FunctionId(func) = def
                && FunctionSignature::of(db, func).name.as_str() == function
            {
                return func;
            }
        }
    }
    panic!("no function `{function}` in `{krate:?}`")
}

/// A fixture: minicore's flags, the crate (with overflow checks, as a dev build has), then
/// its source.
fn fixture_of(flags: &str, krate: &str, source: &str) -> String {
    format!("//- minicore: {flags}\n//- /lib.rs crate:{krate} cfg:overflow_checks\n{source}")
}

const FLAGS: &str = "sized, copy, add, ord, eq, fn, builtin_impls";

#[track_caller]
fn emitted(flags: &str, krate: &str, name: &str, source: &str) -> Emitted {
    let db = db(&fixture_of(flags, krate, source));
    let func = function(&db, krate, name);
    let first = match emit(&db, func) {
        Ok(it) => it,
        Err(err) => panic!("{err}"),
    };
    // Emitting again gives the same bytes (`deterministic-output`), from the same database
    // and from a new one.
    assert_eq!(emit(&db, func).as_ref().map(|it| &it.text), Ok(&first.text));
    let other = self::db(&fixture_of(flags, krate, source));
    let again = emit(&other, function(&other, krate, name)).expect("emitted once already");
    assert_eq!(again.text, first.text);
    first
}

#[track_caller]
fn refused(flags: &str, name: &str, source: &str) -> crate::NotTranslated {
    let db = db(&fixture_of(flags, "main", source));
    match emit(&db, function(&db, "main", name)) {
        Err(err) => err,
        Ok(it) => panic!("expected a refusal, got:\n{}", it.text),
    }
}

#[test]
fn arithmetic_fails_on_overflow() {
    let out = emitted(FLAGS, "arith", "add_one", "pub fn add_one(x: u32) -> u32 { x + 1 }\n");
    expect_file!["../../../lean/BlazeModel/arith/add_one.lean"].assert_eq(&out.text);
    assert!(out.shape.monadic && !out.shape.fuel);
}

#[test]
fn a_write_through_a_mut_reference_gives_the_struct_back() {
    let out = emitted(
        FLAGS,
        "structs",
        "bump",
        "pub struct Counter { pub hits: u32, pub limit: u32 }
pub fn bump(counter: &mut Counter) { counter.hits = counter.limit; }
",
    );
    expect_file!["../../../lean/BlazeModel/structs/bump.lean"].assert_eq(&out.text);
    // The struct is declared in a module of its own, which the function's imports.
    let [counter] = out.imports.as_slice() else { panic!("one import: {:?}", out.imports) };
    assert_eq!(counter.module, "BlazeModel.structs.Counter.type");
    expect_file!["../../../lean/BlazeModel/structs/Counter/type.lean"].assert_eq(&counter.text);
    assert_eq!(out.shape.mut_params, [0]);
    assert!(!out.shape.monadic, "nothing in it can fail: a plain function");
}

#[test]
fn a_match_on_an_enum_is_a_lean_match() {
    let out = emitted(
        FLAGS,
        "enums",
        "side",
        "pub enum Shape { Dot, Square(u32) }
pub fn side(shape: Shape) -> u32 { match shape { Shape::Dot => 0, Shape::Square(side) => side } }
",
    );
    expect_file!["../../../lean/BlazeModel/enums/side.lean"].assert_eq(&out.text);
    let [shape] = out.imports.as_slice() else { panic!("one import: {:?}", out.imports) };
    expect_file!["../../../lean/BlazeModel/enums/Shape/type.lean"].assert_eq(&shape.text);
}

#[test]
fn a_counted_loop_is_a_function_recursive_on_fuel() {
    let out = emitted(
        FLAGS,
        "loops",
        "count",
        "pub fn count(n: u32) -> u32 { let mut i = 0; while i < n { i += 1; } i }\n",
    );
    expect_file!["../../../lean/BlazeModel/loops/count.lean"].assert_eq(&out.text);
    assert!(out.shape.fuel);
}

#[test]
fn an_early_return_is_the_branch_that_returns() {
    let out = emitted(
        FLAGS,
        "early",
        "clamp",
        "pub fn clamp(x: u32, limit: u32) -> u32 { if x > limit { return limit; } x }\n",
    );
    expect_file!["../../../lean/BlazeModel/early/clamp.lean"].assert_eq(&out.text);
}

#[test]
fn a_slice_index_fails_out_of_bounds() {
    let out = emitted(
        "sized, copy, add, ord, eq, fn, builtin_impls, index, slice",
        "slices",
        "first",
        "pub fn first(xs: &[u32]) -> u32 { xs[0] }\n",
    );
    expect_file!["../../../lean/BlazeModel/slices/first.lean"].assert_eq(&out.text);
}

#[test]
fn a_call_imports_exactly_the_callee() {
    let out = emitted(
        FLAGS,
        "calls",
        "quadruple",
        "pub fn double(x: u32) -> u32 { x + x }
pub fn quadruple(x: u32) -> u32 { double(double(x)) }
",
    );
    expect_file!["../../../lean/BlazeModel/calls/quadruple.lean"].assert_eq(&out.text);
    let [double] = out.imports.as_slice() else { panic!("one import: {:?}", out.imports) };
    assert_eq!(double.module, "BlazeModel.calls.double");
    expect_file!["../../../lean/BlazeModel/calls/double.lean"].assert_eq(&double.text);
}

/// A state machine's shape: a constructor, a `&mut self` step and a `&self` read, called from
/// a function of the module. Each is a function in the namespace of its type, and the type is
/// declared once however many of them use it.
#[test]
fn a_method_is_a_function_in_its_types_namespace() {
    let out = emitted(
        FLAGS,
        "methods",
        "twice",
        "pub struct Counter { pub hits: u32, pub limit: u32 }
impl Counter {
    pub fn new(limit: u32) -> Counter { Counter { hits: 0, limit } }
    pub fn bump(&mut self) -> bool {
        if self.hits < self.limit { self.hits += 1; true } else { false }
    }
    pub fn hits(&self) -> u32 { self.hits }
}
pub fn twice(limit: u32) -> u32 {
    let mut counter = Counter::new(limit);
    counter.bump();
    counter.bump();
    counter.hits()
}
",
    );
    expect_file!["../../../lean/BlazeModel/methods/twice.lean"].assert_eq(&out.text);
    let modules: Vec<&str> = out.imports.iter().map(|it| it.module.as_str()).collect();
    // `hits` is also a field, whose projection Lean has put at `methods.Counter.hits`: the
    // method is `hits_fn`.
    assert_eq!(
        modules,
        [
            "BlazeModel.methods.Counter.type",
            "BlazeModel.methods.Counter.new",
            "BlazeModel.methods.Counter.bump",
            "BlazeModel.methods.Counter.hits_fn",
        ]
    );
    let [ty, new, bump, hits] = out.imports.as_slice() else { unreachable!() };
    expect_file!["../../../lean/BlazeModel/methods/Counter/type.lean"].assert_eq(&ty.text);
    expect_file!["../../../lean/BlazeModel/methods/Counter/new.lean"].assert_eq(&new.text);
    expect_file!["../../../lean/BlazeModel/methods/Counter/bump.lean"].assert_eq(&bump.text);
    expect_file!["../../../lean/BlazeModel/methods/Counter/hits_fn.lean"].assert_eq(&hits.text);
    assert_eq!(bump.function, "methods::Counter::bump");
    assert_eq!(bump.shape.mut_params, [0]);
}

#[test]
fn a_method_of_a_trait_impl_or_a_generic_type_is_refused_by_name() {
    let cases = [
        (
            "pub trait T { fn g(&self) -> u32; }\npub struct S;\nimpl T for S { fn g(&self) -> u32 { 0 } }\npub fn f(s: S) -> u32 { S::h(&s) }\nimpl S { pub fn h(&self) -> u32 { <S as T>::g(self) } }\n",
            "generic function",
        ),
        (
            "pub struct W<T> { pub x: T }\nimpl<T> W<T> { pub fn get(&self) -> u32 { 0 } }\npub fn f(w: W<u32>) -> u32 { w.get() }\n",
            "generic",
        ),
    ];
    for (source, what) in cases {
        let err = refused(FLAGS, "f", source);
        assert!(err.what.contains(what), "`{source}` is refused for {what}: {err}");
    }
}

#[test]
fn what_v0_does_not_translate_is_refused_by_name() {
    let cases = [
        ("pub fn f(p: *const u32) -> u32 { 0 }\n", "raw pointer"),
        ("pub unsafe fn f() -> u32 { 0 }\n", "`unsafe fn`"),
        ("pub fn f() -> u32 { let g = |x: u32| x; g(1) }\n", "closure"),
        ("pub fn f(x: f32) -> f32 { x }\n", "floating-point"),
        ("pub fn f(s: &str) -> u32 { 0 }\n", "string"),
        ("pub fn f<T>(x: T) -> T { x }\n", "generic function"),
        ("pub fn f(n: u32) -> u32 { if n == 0 { 0 } else { f(n - 1) } }\n", "recursion"),
        ("pub struct P<'a> { pub x: &'a mut u32 }\npub fn f(p: P<'_>) -> u32 { 0 }\n", "generic struct"),
        ("pub fn f(x: &mut (u32, u32)) -> &mut u32 { &mut x.0 }\n", "returns a `&mut`"),
        ("pub union U { a: u32, b: u32 }\npub fn f(u: U) -> u32 { 0 }\n", "union"),
    ];
    for (source, what) in cases {
        let err = refused(FLAGS, "f", source);
        assert!(err.what.contains(what), "`{source}` is refused for {what}: {err}");
        assert_eq!(err.function, "main::f");
        assert!(!err.at.is_empty());
    }
}

// ---- The first proof: `blaze_graph::ledger::fnv1a_from`. ----

/// `fnv1a_from` as blaze-graph's machine ledger had it (the ledger was deleted with the memory
/// control stack on 2026-11-13; the function's text is kept here, byte for byte, as the subject
/// of the first proof), in a crate and a module of the same names (`blaze_graph::ledger`, which
/// the checked-in Lean models are named by), with the clauses blaze's own crates cannot carry
/// yet (`blaze_verify` is not on for them): the signature's return is named and an `ensures`
/// follows it.
const FNV1A_FROM: &str = "const fn fnv1a_from(mut hash: u64, bytes: &[u8]) -> u64 {
    let mut at = 0;
    while at < bytes.len() {
        hash ^= bytes[at] as u64;
        hash = hash.wrapping_mul(0x0100_0000_01b3);
        at += 1;
    }
    hash
}
";

fn fnv1a_fixture() -> String {
    let ledger = FNV1A_FROM;
    let head = "const fn fnv1a_from(mut hash: u64, bytes: &[u8]) -> u64 {";
    let start = ledger.find(head).expect("the ledger has `fnv1a_from`");
    let end = start + ledger[start..].find("\n}\n").expect("the function ends") + 3;
    let function = ledger[start..end].replace(
        head,
        "pub const fn fnv1a_from(mut hash: u64, bytes: &[u8]) -> (r: u64)\n    ensures bytes.len() != 0 || r == hash\n{",
    );
    format!(
        "//- minicore: sized, copy, add, ord, eq, fn, builtin_impls, index, slice, int_wrapping\n\
         //- /lib.rs crate:blaze_graph cfg:overflow_checks\n\
         #![feature(blaze_verify)]\npub mod ledger {{\n{function}}}\n"
    )
}

/// The values of the test interpreter of structured MIR.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Value {
    Unit,
    Bool(bool),
    Int(IntTy, u128),
    List(Vec<Value>),
}

/// Runs the structured MIR of a function over integers, `bool`s and slices: enough for
/// `fnv1a_from`. What it does not run panics the test.
struct Interp {
    locals: BTreeMap<Local, Value>,
}

fn mask(ty: IntTy, value: u128) -> u128 {
    if ty.bits() == 128 { value } else { value & ((1u128 << ty.bits()) - 1) }
}

impl Interp {
    fn read(&self, place: &Place) -> Result<Value, End> {
        let mut value = self.locals.get(&place.local).cloned().expect("an initialized local");
        for elem in &place.elems {
            value = match (elem, value) {
                (Elem::Deref, value) => value,
                (Elem::Index(by), Value::List(items)) => {
                    let Some(Value::Int(_, at)) = self.locals.get(by) else { panic!("an index") };
                    items.get(*at as usize).cloned().ok_or(End::Panic)?
                }
                (elem, value) => panic!("the test interpreter does not read {elem:?} of {value:?}"),
            };
        }
        Ok(value)
    }

    fn operand(&self, operand: &Operand) -> Result<Value, End> {
        Ok(match operand {
            Operand::Place(place) => self.read(place)?,
            Operand::Int(ty, bits) => Value::Int(*ty, mask(*ty, *bits)),
            Operand::Bool(value) => Value::Bool(*value),
            Operand::Unit => Value::Unit,
        })
    }

    fn int(&self, operand: &Operand) -> Result<u128, End> {
        match self.operand(operand)? {
            Value::Int(_, bits) => Ok(bits),
            other => panic!("an integer, not {other:?}"),
        }
    }

    fn rvalue(&self, value: &Rvalue) -> Result<Value, End> {
        Ok(match value {
            Rvalue::Use(of) => self.operand(of)?,
            Rvalue::Len(place) => match self.read(place)? {
                Value::List(items) => Value::Int(IntTy::Usize, items.len() as u128),
                other => panic!("the length of {other:?}"),
            },
            Rvalue::Cast { to, of, .. } => Value::Int(*to, mask(*to, self.int(of)?)),
            Rvalue::Bin { op, ty: OpTy::Int(ty), checked, left, right } => {
                assert!(!ty.signed(), "the test interpreter runs unsigned arithmetic");
                let (left, right) = (self.int(left)?, self.int(right)?);
                let fits = |wide: Option<u128>| match wide {
                    Some(value) if mask(*ty, value) == value => Ok(value),
                    Some(value) if !*checked => Ok(mask(*ty, value)),
                    _ => Err(End::Panic),
                };
                match op {
                    BinOp::Add => Value::Int(*ty, fits(left.checked_add(right))?),
                    BinOp::BitXor => Value::Int(*ty, left ^ right),
                    BinOp::Lt => Value::Bool(left < right),
                    other => panic!("the test interpreter does not run {other:?}"),
                }
            }
            other => panic!("the test interpreter does not run {other:?}"),
        })
    }
}

impl Machine<Stmt, Cond> for Interp {
    fn step(&mut self, statement: &Stmt) -> Result<(), End> {
        let (place, value) = match statement {
            Stmt::Assign { place, value, .. } => (place, self.rvalue(value)?),
            Stmt::Call { dest, callee: Callee::Model { name, .. }, args, .. } => {
                let value = match (*name, args.as_slice()) {
                    ("Slice.len", [of]) => match self.operand(of)? {
                        Value::List(items) => Value::Int(IntTy::Usize, items.len() as u128),
                        other => panic!("the length of {other:?}"),
                    },
                    ("Scalar.wrapping_mul", [left, right]) => {
                        let Value::Int(ty, left) = self.operand(left)? else { panic!("an integer") };
                        Value::Int(ty, mask(ty, left.wrapping_mul(self.int(right)?)))
                    }
                    other => panic!("the test interpreter has no model of {other:?}"),
                };
                (dest, value)
            }
            other => panic!("the test interpreter does not run {other:?}"),
        };
        assert!(place.elems.is_empty(), "the test interpreter assigns whole locals");
        self.locals.insert(place.local, value);
        Ok(())
    }

    fn test(&mut self, on: &Cond) -> u128 {
        match on {
            Cond::Operand(operand, _) => match self.operand(operand) {
                Ok(Value::Bool(value)) => u128::from(value),
                Ok(Value::Int(_, bits)) => bits,
                other => panic!("a test of {other:?}"),
            },
            Cond::Discriminant(..) => panic!("the test interpreter runs no `match`"),
        }
    }
}

/// FNV-1a's fold: what the proof file calls `fold`.
fn fnv1a_fold(seed: u64, bytes: &[u8]) -> u64 {
    bytes.iter().fold(seed, |hash, byte| (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3))
}

/// What `body` (structured MIR of `fnv1a_from`) returns for `seed` and `bytes`.
fn run_fnv1a(body: &Body, seed: u64, bytes: &[u8]) -> Result<u64, Ended> {
    let mut interp = Interp { locals: BTreeMap::new() };
    interp.locals.insert(body.params[0], Value::Int(IntTy::U64, u128::from(seed)));
    let bytes = bytes.iter().map(|it| Value::Int(IntTy::U8, u128::from(*it))).collect();
    interp.locals.insert(body.params[1], Value::List(bytes));
    match run_structured(&body.nodes, &mut interp, 1_000_000) {
        Ended::Returned => match interp.locals.get(&0) {
            Some(Value::Int(IntTy::U64, bits)) => Ok(*bits as u64),
            other => panic!("the return slot holds {other:?}"),
        },
        ended => Err(ended),
    }
}

/// The oracle of the first proof's test: the structured MIR runs to FNV-1a's fold.
fn is_the_fold(body: &Body) -> Result<(), String> {
    let inputs: [(u64, &[u8]); 4] = [
        (0xcbf2_9ce4_8422_2325, b""),
        (0xcbf2_9ce4_8422_2325, b"a"),
        (7, b"blaze ledger"),
        (u64::MAX, &[0xff, 0x00, 0x80, 0x7f]),
    ];
    for (seed, bytes) in inputs {
        let (got, want) = (run_fnv1a(body, seed, bytes), Ok(fnv1a_fold(seed, bytes)));
        if got != want {
            return Err(format!("from {seed:#x} over {bytes:?}: {got:?}, not {want:?}"));
        }
    }
    Ok(())
}

fn fnv1a(db: &Db) -> FunctionId {
    function(db, "blaze_graph", "fnv1a_from")
}

/// The invariant: the ledger's
/// checksum loop `fnv1a_from` never panics (no index out of bounds, no counter overflow) and
/// returns FNV-1a's fold of the seed over the bytes, for every seed and every slice. The
/// proof is of the model this test holds to the function's source: its text is [`FNV1A_FROM`]
/// (the deleted ledger's, kept byte for byte), its emitted Lean must be the checked-in module the
/// proof imports, and its structured MIR runs to the same fold on the inputs here.
/// Lean 4.20.0 accepts the proof of the model (crates/blaze-graph/lean/BlazeGraph/Ledger.lean)
/// and a proof is written that the model is what the function's MIR runs to
/// (BlazeGraph/LedgerMir.lean, objective V7). This statement says `(proved: ..)` once three
/// things hold on one commit, never before: this test and
/// `fnv1a_from_is_embedded_as_the_body_its_proof_walks` pass (the model and the embedded body
/// are what blaze emits), and `lake build` of `lean/` passes in the gate with both proofs.
#[test]
fn fnv1a_from_is_the_fold_of_its_bytes() {
    let db = db(&fnv1a_fixture());
    let func = fnv1a(&db);
    let out = match emit(&db, func) {
        Ok(it) => it,
        Err(err) => panic!("{err}"),
    };
    expect_file!["../../../lean/BlazeModel/blaze_graph/ledger/fnv1a_from.lean"].assert_eq(&out.text);
    assert_eq!(out.statement.as_deref(), Some("blaze_graph.ledger.fnv1a_from.spec.statement"));
    assert_eq!(out.clauses, ["ensures bytes.len() != 0 || r == hash"]);
    let body = crate::structured(&db, func).expect("emitted above");
    if let Err(broke) = is_the_fold(&body) {
        panic!("{broke}");
    }
}

fn replace_xor(nodes: &mut [Node<Stmt, Cond>]) {
    for node in nodes {
        match node {
            Node::Do(Stmt::Assign { value: Rvalue::Bin { op, .. }, .. }) if *op == BinOp::BitXor => {
                *op = BinOp::Add;
            }
            Node::Branch { arms, otherwise, .. } => {
                arms.iter_mut().for_each(|(_, arm)| replace_xor(arm));
                replace_xor(otherwise);
            }
            Node::Block(body) | Node::Loop(body) => replace_xor(body),
            _ => {}
        }
    }
}

#[test]
fn the_oracle_catches_an_xor_made_an_add() {
    let db = db(&fnv1a_fixture());
    let mut body = crate::structured(&db, fnv1a(&db)).expect("translated");
    assert_eq!(is_the_fold(&body), Ok(()));
    replace_xor(&mut body.nodes);
    assert!(is_the_fold(&body).is_err());
}

// ---- The proof check: the plan (`plan`), run here by a test's own driver. ----

#[test]
fn the_axioms_lean_prints_are_read() {
    let listed = "'m.f.spec' depends on axioms: [propext,\n Classical.choice,\n Quot.sound]\n";
    let axioms = plan::printed_axioms(listed, "m.f.spec").expect("listed");
    assert_eq!(axioms, ["propext", "Classical.choice", "Quot.sound"]);
    let none = "'m.f.spec' does not depend on any axioms\n";
    assert_eq!(plan::printed_axioms(none, "m.f.spec"), Some(Vec::new()));
    assert_eq!(plan::printed_axioms("warning: unused\n", "m.f.spec"), None);
}

/// A plan's driver for tests: the files written under `out` (`lib` for [`plan::Root::Out`],
/// `proofs` for [`plan::Root::Proofs`]) and each step a check needs run with the Lean
/// `BLAZE_LEAN_PROVER` names, as the daemon's driver does but with no sandbox and no deadline:
/// test code, over blaze's own proofs. Without the variable the test fails, never skips.
fn run_plan(planned: &plan::Plan, out: &std::path::Path) -> Vec<plan::Ran> {
    let lean = std::env::var_os("BLAZE_LEAN_PROVER")
        .expect("BLAZE_LEAN_PROVER names the `lean` of the release lean/lean-toolchain pins");
    let version = std::process::Command::new(&lean).arg("--version").output().expect("Lean starts");
    let version = String::from_utf8_lossy(&version.stdout).into_owned();
    assert!(version.contains(&format!("version {}", crate::LEAN_VERSION)), "{version}");
    let dir = |root: plan::Root| match root {
        plan::Root::Out => out.join("lib"),
        plan::Root::Proofs => out.join("proofs"),
    };
    for write in &planned.writes {
        let path = dir(write.root).join(&write.path);
        std::fs::create_dir_all(path.parent().expect("a file is in a directory")).unwrap();
        std::fs::write(&path, &write.text).unwrap();
    }
    let search = std::env::join_paths([dir(plan::Root::Out), dir(plan::Root::Proofs)]).unwrap();
    let mut ran = vec![plan::Ran::NotRun; planned.steps.len()];
    for at in plan::wanted(planned) {
        let step = &planned.steps[at];
        if step.needs.iter().any(|need| !matches!(ran[*need], plan::Ran::Accepted(_))) {
            continue;
        }
        let root = dir(step.root);
        let mut command = std::process::Command::new(&lean);
        command.env_clear().env("LEAN_PATH", &search).current_dir(&root).arg("--root=.");
        if let Some(olean) = step.olean() {
            command.arg("-o").arg(root.join(olean));
        }
        let output = command.arg(step.file()).output().expect("Lean starts");
        let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&output.stderr));
        ran[at] = match output.status.success() {
            true => plan::Ran::Accepted(text),
            false => plan::Ran::Rejected(text),
        };
    }
    ran
}

/// End to end, with the pinned Lean: the plan of blaze-core's two theorems, whose one proof
/// file proves both and imports `Blaze.Auto`, run step by step, proves both on the allowed
/// axioms. With the proof file's last theorem cut off, the file still compiles: the theorem
/// it no longer proves is open at its check, in Lean's own words naming the proof it lacks,
/// and the other is proved as before.
#[test]
fn lean_checks_blaze_cores_proofs_by_the_plan() {
    let source = busy_fixture();
    let names = ["next_is_bounded", "next_is_the_doubling_schedule"];
    let theorems: Vec<Emitted> = names.iter().map(|name| emitted(FLAGS, "blaze_core", name, &source)).collect();
    let proof = include_str!("../../blaze-core/lean/BlazeCore/Busy.lean");
    let proofs = BTreeMap::from([("BlazeCore.Busy".to_owned(), proof.to_owned())]);
    let planned = plan::plan(plan::Ask { theorems: &theorems, proofs: &proofs }).unwrap_or_else(|err| panic!("{err}"));
    let out = blaze_core::temp::TempDir::for_test("blaze-hir-ty-lean-plan-");
    let ran = run_plan(&planned, out.path());
    let verdicts = plan::conclude(&planned, &ran).unwrap_or_else(|err| panic!("{err}"));
    for (verdict, name) in verdicts.iter().zip(names) {
        match verdict {
            plan::Verdict::Proved { theorem, axioms, key, .. } => {
                assert_eq!(theorem, &format!("blaze_core.busy.{name}.spec.checked"));
                assert!(axioms.iter().all(|it| crate::ALLOWED_AXIOMS.contains(&it.as_str())), "{axioms:?}");
                assert_eq!(key.how, crate::Prover::Lean);
            }
            plan::Verdict::Open { why, .. } => panic!("`{name}` is not proved: {why}"),
        }
    }
    // The proof file without its last theorem: the check of that theorem finds no proof.
    let cut = proof.find("theorem blaze_core.busy.next_is_the_doubling_schedule.spec.proof").expect("its last theorem");
    let proofs = BTreeMap::from([("BlazeCore.Busy".to_owned(), proof[..cut].to_owned())]);
    let planned = plan::plan(plan::Ask { theorems: &theorems, proofs: &proofs }).unwrap_or_else(|err| panic!("{err}"));
    let other = blaze_core::temp::TempDir::for_test("blaze-hir-ty-lean-plan-cut-");
    let verdicts = plan::conclude(&planned, &run_plan(&planned, other.path())).unwrap();
    assert!(matches!(&verdicts[0], plan::Verdict::Proved { .. }), "{:?}", verdicts[0]);
    match &verdicts[1] {
        plan::Verdict::Open { why: why @ plan::Why::Rejected { kind: plan::Kind::Check, .. }, .. } => {
            assert!(why.to_string().contains("next_is_the_doubling_schedule.spec.proof"), "{why}");
        }
        other => panic!("the theorem with no proof is open at its check: {other:?}"),
    }
}

// ---- MIR, embedded (`body::embed`): what `lean/Blaze/Mir` runs. ----

#[track_caller]
fn embedded(flags: &str, krate: &str, name: &str, source: &str) -> crate::EmbeddedMir {
    let db = db(&fixture_of(flags, krate, source));
    let func = function(&db, krate, name);
    let first = match embed_mir(&db, func) {
        Ok(it) => it,
        Err(err) => panic!("{err}"),
    };
    // Written again, it is the same bytes (`deterministic-output`), from the same database
    // and from a new one.
    assert_eq!(embed_mir(&db, func).as_ref().map(|it| &it.text), Ok(&first.text));
    let other = self::db(&fixture_of(flags, krate, source));
    let again = embed_mir(&other, function(&other, krate, name)).expect("embedded once already");
    assert_eq!(again.text, first.text);
    assert_eq!(first.module, format!("BlazeMir.{krate}.{name}"));
    first
}

const SLICES: &str = "sized, copy, add, ord, eq, fn, builtin_impls, index, slice";

/// The fixtures of the embedded goldens: the crate, the function, minicore's flags, the source.
/// Each is a construct of the subset; `lean/Blaze/Mir/Tests/Golden.lean` imports every one,
/// so `lake build` type-checks them, and `Tests/Run.lean` runs them.
const EMBEDDED: &[(&str, &str, &str, &str)] = &[
    ("arith", "add_one", FLAGS, "pub fn add_one(x: u32) -> u32 { x + 1 }\n"),
    (
        "structs",
        "bump",
        FLAGS,
        "pub struct Counter { pub hits: u32, pub limit: u32 }
pub fn bump(counter: &mut Counter) { counter.hits = counter.limit; }
",
    ),
    (
        "enums",
        "side",
        FLAGS,
        "pub enum Shape { Dot, Square(u32) }
pub fn side(shape: Shape) -> u32 { match shape { Shape::Dot => 0, Shape::Square(side) => side } }
",
    ),
    (
        "loops",
        "count",
        FLAGS,
        "pub fn count(n: u32) -> u32 { let mut i = 0; while i < n { i += 1; } i }\n",
    ),
    (
        "early",
        "clamp",
        FLAGS,
        "pub fn clamp(x: u32, limit: u32) -> u32 { if x > limit { return limit; } x }\n",
    ),
    ("slices", "first", SLICES, "pub fn first(xs: &[u32]) -> u32 { xs[0] }\n"),
    (
        "calls",
        "quadruple",
        FLAGS,
        "pub fn double(x: u32) -> u32 { x + x }
pub fn quadruple(x: u32) -> u32 { double(double(x)) }
",
    ),
];

#[test]
fn mir_is_embedded_as_the_golden_bodies() {
    for (krate, name, flags, source) in EMBEDDED {
        let out = embedded(flags, krate, name, source);
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../lean/BlazeMir")
            .join(krate)
            .join(format!("{name}.lean"));
        expect_file![path].assert_eq(&out.text);
    }
}

/// The body the proof `crates/blaze-graph/lean/BlazeGraph/LedgerMir.lean` walks is the body
/// blaze has for the tree's `fnv1a_from`: the first link of the chain from the MIR to FNV-1a's
/// fold.
#[test]
fn fnv1a_from_is_embedded_as_the_body_its_proof_walks() {
    let db = db(&fnv1a_fixture());
    let out = embed_mir(&db, fnv1a(&db)).unwrap_or_else(|err| panic!("{err}"));
    assert_eq!(out.name, "blaze_graph.ledger.fnv1a_from.mir");
    expect_file!["../../../lean/BlazeMir/blaze_graph/ledger/fnv1a_from.lean"].assert_eq(&out.text);
    assert_eq!(embed_mir(&db, fnv1a(&db)).map(|it| it.text), Ok(out.text));
}

#[test]
fn what_the_semantics_does_not_have_is_refused_by_name() {
    let cases = [
        ("pub fn f(p: *const u32) -> u32 { 0 }\n", "raw pointer"),
        ("pub unsafe fn f() -> u32 { 0 }\n", "`unsafe fn`"),
        ("pub fn f() -> u32 { let g = |x: u32| x; g(1) }\n", "closure"),
        ("pub fn f(x: f32) -> f32 { x }\n", "floating-point"),
        ("pub fn f(s: &str) -> u32 { 0 }\n", "string"),
        ("pub fn f<T>(x: T) -> T { x }\n", "generic function"),
        ("pub union U { a: u32, b: u32 }\npub fn f(u: U) -> u32 { 0 }\n", "union"),
        ("pub fn g<T>(x: T) -> T { x }\npub fn f() -> u32 { g(1) }\n", "generic function `g`"),
        ("pub fn f(x: u32) -> u64 { x as f32 as u64 }\n", "floating-point"),
    ];
    for (source, what) in cases {
        let db = db(&fixture_of(FLAGS, "main", source));
        match embed_mir(&db, function(&db, "main", "f")) {
            Err(err) => {
                assert!(err.what.contains(what), "`{source}` is refused for {what}: {err}");
                assert_eq!(err.function, "main::f");
                assert!(!err.at.is_empty());
            }
            Ok(it) => panic!("expected a refusal of `{source}`, got:\n{}", it.text),
        }
    }
}

// ---- The passes, held to their Lean specifications. ----

/// The passes with a specification in `lean/Blaze/Mir/Opt`, by rustc's name, and the Lean
/// function each must equal.
const SPECIFIED: &[(Pass, &str, &str)] = &[
    (Pass::RemoveStorageMarkers, "RemoveStorageMarkers", "Opt.removeStorageMarkers"),
    (Pass::SimplifyConstCondition, "SimplifyConstCondition", "Opt.simplifyConstCondition"),
];

/// `pass` run alone on `func`'s runtime MIR, as Lean: the body before, the body after, and
/// `example : after = <spec> before := rfl`, which the kernel decides by evaluating the
/// specification. Lean failing on it says the Rust pass did not do what its specification
/// does (it did more, or something else): a finding, reported by the build.
fn validation(db: &Db, krate: &str, name: &str, pass: Pass) -> String {
    let func = function(db, krate, name);
    let (_, pass_name, spec) =
        SPECIFIED.iter().find(|it| it.0 == pass).expect("a pass with a specification");
    let (before, after) = blaze_query::db::attach(db, || {
        let def = DefWithBodyId::FunctionId(func);
        let before = match db.runtime_mir_body(def.into()) {
            Ok(body) => body.clone(),
            Err(err) => panic!("the runtime MIR of `{krate}::{name}`: {err:?}"),
        };
        let mut after = before.clone();
        budget::with_passes(db, def.into(), |passes| passes.run(&mut after, pass))
            .unwrap_or_else(|err| panic!("{pass_name} on `{krate}::{name}`: {err:?}"));
        (before, after)
    });
    // `after` over `before`'s blocks: the pass is compared alone, not the pass and then the
    // encoder's dropping of a block the pass left unreachable.
    let after =
        embed_mir_body_as(db, func, &after, &before).unwrap_or_else(|err| panic!("{err}"));
    let before = embed_mir_body(db, func, &before).unwrap_or_else(|err| panic!("{err}"));
    let namespace = lean::ident(&format!("BlazeValidated.{pass_name}.{krate}.{name}"));
    format!(
        "namespace {namespace}\n\ndef before : Program :=\n{before}\n\ndef after : Program :=\n{after}\n\n\
         /-- `{pass_name}` did to `{krate}::{name}` what its specification does. -/\n\
         example : after = {spec} before := rfl\n\nend {namespace}\n"
    )
}

/// The head of a module of validations.
fn validation_head() -> String {
    format!(
        "-- Generated by blaze ({}): MIR passes run alone on runtime MIR, each held to its Lean specification.\n\
         -- Do not edit: the golden tests of crates/hir-ty-lean hold this file to what the passes and the encoder write.\n\
         import Blaze.Mir.Opt.RemoveStorageMarkers\nimport Blaze.Mir.Opt.SimplifyConstCondition\n\n\
         open Blaze Blaze.Mir\n",
        crate::TRANSLATOR
    )
}

/// A MIR pass with a Lean specification does, on each body here, exactly
/// what the specification does (`after = spec before`, decided by Lean's kernel when
/// `lake build` checks lean/BlazeValidated/passes.lean); the specification is proved to
/// preserve every run (`Blaze.Mir.Opt.removeStorageMarkers_correct`,
/// `simplifyConstCondition_correct`). This test holds the checked-in pairs to what the Rust
/// passes and the encoder produce today.
#[test]
fn the_passes_are_their_lean_specifications() {
    let loops = db(&fixture_of(
        FLAGS,
        "loops",
        "pub fn count(n: u32) -> u32 { let mut i = 0; while i < n { i += 1; } i }\n",
    ));
    let consts = db(&fixture_of(
        FLAGS,
        "consts",
        "pub fn pick(x: u32) -> u32 { if true { x } else { 0 } }\n",
    ));
    let text = [
        validation_head(),
        validation(&loops, "loops", "count", Pass::RemoveStorageMarkers),
        validation(&consts, "consts", "pick", Pass::SimplifyConstCondition),
    ]
    .join("\n");
    expect_file!["../../../lean/BlazeValidated/passes.lean"].assert_eq(&text);
}

// ---- The corpus of harness/v2/mir_semantics.py. ----

/// What blaze's MIR evaluator says of the nullary function `func`, in the fixed form of
/// `lean/Blaze/Mir/Runner.lean`: `ok <value>`, `panic <kind>`, `undefined`, or `error ..`.
fn evaluated(db: &Db, func: FunctionId, returns: &str) -> String {
    blaze_query::db::attach(db, || {
        // The body the evaluator runs: runtime MIR, as `tests/layout/field_order.rs` asks for it.
        let env = hir_ty::traits::ParamEnvAndCrate {
            param_env: db.trait_environment(func.into()),
            krate: func.krate(db),
        }
        .store();
        let body = match db.monomorphized_runtime_mir_body(
            func.into(),
            hir_ty::next_solver::GenericArgs::empty().store(),
            env,
        ) {
            Ok(body) => body,
            Err(err) => return format!("error: no MIR ({err:?})"),
        };
        let value = match hir_ty_mir::mir::interpret_mir(db, body, None) {
            Ok((Ok(value), _)) => value,
            Ok((Err(err), _)) | Err(err) => {
                let said = format!("{err:?}");
                let lower = said.to_lowercase();
                return if lower.contains("undefinedbehavior") {
                    "undefined".to_owned()
                } else if !lower.contains("panic") {
                    format!("error: {}", said.replace('\n', " "))
                } else if lower.contains("out of bounds") {
                    "panic bounds".to_owned()
                } else if lower.contains("overflow")
                    || lower.contains("by zero")
                    || lower.contains("zero divisor")
                {
                    "panic arithmetic".to_owned()
                } else {
                    "panic panic".to_owned()
                };
            }
        };
        let memory: &[u8] = &value.memory;
        let mut bytes = [0u8; 16];
        bytes[..memory.len().min(16)].copy_from_slice(&memory[..memory.len().min(16)]);
        let bits = u128::from_le_bytes(bytes);
        let int = match returns {
            "bool" => return format!("ok {}", bits != 0),
            "u8" => IntTy::U8,
            "u16" => IntTy::U16,
            "u32" => IntTy::U32,
            "u64" => IntTy::U64,
            "u128" => IntTy::U128,
            "usize" => IntTy::Usize,
            "i8" => IntTy::I8,
            "i16" => IntTy::I16,
            "i32" => IntTy::I32,
            "i64" => IntTy::I64,
            "i128" => IntTy::I128,
            "isize" => IntTy::Isize,
            other => return format!("error: a case returns `{other}`"),
        };
        format!("ok {}", int.decimal(bits))
    })
}

/// The cases of a corpus program: each `pub fn case_N() -> T {` of its source, its name and `T`.
fn cases_of(source: &str) -> Vec<(String, String)> {
    source
        .lines()
        .filter_map(|line| {
            let rest = line.strip_prefix("pub fn ")?;
            let (name, rest) = rest.split_once("() -> ")?;
            let (returns, _) = rest.split_once(" {")?;
            name.starts_with("case_").then(|| (name.to_owned(), returns.to_owned()))
        })
        .collect()
}

/// The Rust half of harness/v2/mir_semantics.py, which names a directory in
/// `BLAZE_MIR_SEMANTICS` (without it this test fails, never skips). For each program
/// `programs/<name>.rs` there (a crate `<name>` with `pub fn entry(..)` and nullary
/// `pub fn case_N() -> T { entry(..) }`), it writes:
///
/// - `Corpus.lean`: `entry`'s embedded program under the name `<name>`, and a `main` that runs
///   `Blaze.Mir.Runner` over them;
/// - `evaluator.txt`: a line `<name> <case> <answer>` per case, from blaze's MIR evaluator;
/// - `Validate.lean`: each specified pass on `entry`'s runtime MIR against its specification.
#[test]
fn mir_semantics_corpus() {
    let dir = std::env::var_os("BLAZE_MIR_SEMANTICS").map(std::path::PathBuf::from).expect(
        "BLAZE_MIR_SEMANTICS names the corpus directory harness/v2/mir_semantics.py wrote: run \
         that script, which runs this test",
    );
    let mut names: Vec<String> = std::fs::read_dir(dir.join("programs"))
        .unwrap_or_else(|err| panic!("{}/programs: {err}", dir.display()))
        .map(|entry| entry.expect("a directory entry").file_name().to_string_lossy().into_owned())
        .filter_map(|file| file.strip_suffix(".rs").map(str::to_owned))
        .collect();
    // A directory lists in any order; the output does not depend on it.
    names.sort();
    let flags = "sized, copy, add, ord, eq, fn, builtin_impls, index, slice, int_wrapping, panic";
    let mut corpus = String::from("import Blaze.Mir.Runner\n\nopen Blaze Blaze.Mir\n");
    let mut table = Vec::new();
    let mut answers = String::new();
    let mut validate = validation_head();
    for name in &names {
        let source = std::fs::read_to_string(dir.join("programs").join(format!("{name}.rs")))
            .unwrap_or_else(|err| panic!("{name}.rs: {err}"));
        let db = db(&fixture_of(flags, name, &source));
        let entry = function(&db, name, "entry");
        let embedded = embed_mir(&db, entry).unwrap_or_else(|err| panic!("{err}"));
        let constant = lean::ident(&embedded.name);
        corpus.push_str(&format!("\ndef {constant} : Program :=\n{}\n", embedded.term));
        table.push(format!("(\"{name}\", {constant})"));
        for (case, returns) in cases_of(&source) {
            let answer = evaluated(&db, function(&db, name, &case), &returns);
            answers.push_str(&format!("{name} {case} {answer}\n"));
        }
        for (pass, _, _) in SPECIFIED {
            validate.push('\n');
            validate.push_str(&validation(&db, name, "entry", *pass));
        }
    }
    corpus.push_str(&format!(
        "\ndef main : IO Unit := Blaze.Mir.runMain [\n  {}]\n",
        table.join(",\n  ")
    ));
    let write = |file: &str, text: &str| {
        std::fs::write(dir.join(file), text).unwrap_or_else(|err| panic!("{file}: {err}"));
    };
    write("Corpus.lean", &corpus);
    write("evaluator.txt", &answers);
    write("Validate.lean", &validate);
}

// ---- Theorem functions (docs/v2/plan/design/proofs-for-sans-io.md). ----

/// A state machine's per-step obligation as its author writes it: a well-formedness predicate,
/// a step, and a `#[blaze::theorem]` function saying the step keeps the predicate.
const KEPT: &str = "pub struct Counter { pub hits: u32, pub limit: u32 }
impl Counter {
    pub fn wf(&self) -> bool { self.hits <= self.limit }
    pub fn bump(&mut self) -> bool {
        if self.hits < self.limit { self.hits += 1; true } else { false }
    }
}
#[blaze::theorem]
pub fn bump_keeps_wf(counter: Counter) -> bool {
    if !counter.wf() { return true; }
    let mut counter = counter;
    counter.bump();
    counter.wf()
}
#[blaze::theorem]
pub fn counting_ends(n: u32) -> bool {
    let mut i = 0;
    while i < n { i += 1; }
    i == n
}
";

#[test]
fn a_theorem_function_states_that_it_returns_true() {
    let out = emitted(FLAGS, "theorems", "bump_keeps_wf", KEPT);
    expect_file!["../../../lean/BlazeModel/theorems/bump_keeps_wf.lean"].assert_eq(&out.text);
    // What it imports is checked in beside it, so Lean builds the statement from the tree.
    let [ty, wf, bump] = out.imports.as_slice() else { panic!("three imports: {:?}", out.imports) };
    expect_file!["../../../lean/BlazeModel/theorems/Counter/type.lean"].assert_eq(&ty.text);
    expect_file!["../../../lean/BlazeModel/theorems/Counter/wf.lean"].assert_eq(&wf.text);
    expect_file!["../../../lean/BlazeModel/theorems/Counter/bump.lean"].assert_eq(&bump.text);
    assert_eq!(out.statement.as_deref(), Some("theorems.bump_keeps_wf.spec.statement"));
    assert_eq!(out.clauses, ["#[blaze::theorem]"]);
    assert!(
        out.text.contains("theorems.bump_keeps_wf counter = Result.ok true"),
        "the statement is an equation, so a model that panics does not meet it:\n{}",
        out.text
    );
}

/// The statement of a theorem with a loop is total: some fuel gives `true`. `Result.div` for
/// every fuel (a loop that never ends) does not meet it.
#[test]
fn a_theorem_with_a_loop_states_that_it_returns() {
    let out = emitted(FLAGS, "theorems", "counting_ends", KEPT);
    expect_file!["../../../lean/BlazeModel/theorems/counting_ends.lean"].assert_eq(&out.text);
    assert!(out.shape.fuel);
    assert!(
        out.text.contains("∃ (fuel : Nat), theorems.counting_ends fuel n = Result.ok true"),
        "{}",
        out.text
    );
}

#[test]
fn a_theorem_function_that_cannot_be_one_is_refused_by_name() {
    let cases = [
        ("#[blaze::theorem]\npub fn f() -> u32 { 0 }\n", "returns `u32`, not `bool`"),
        ("#[blaze::theorem]\npub fn f(x: &mut u32) -> bool { *x = 0; true }\n", "`&mut` parameter"),
    ];
    for (source, what) in cases {
        let err = refused(FLAGS, "f", source);
        assert!(err.what.contains(what), "`{source}` is refused for {what}: {err}");
    }
}

// ---- Integer methods, conversions, and `Option`'s and `Result`'s methods. ----

/// What the `ints` fixture enables: the integer methods `ScalarOps` models, their bounds,
/// lossless `From`, `TryFrom`, and `Ord`'s and `cmp`'s `max`, `min` and `clamp`.
const INT_FLAGS: &str =
    "sized, copy, add, ord, eq, fn, builtin_impls, int_methods, int_bounds, int_cast, ord_methods";

/// What the `options` fixture enables: `Option`'s and `Result`'s closure-free methods.
const OPTION_FLAGS: &str =
    "sized, copy, add, ord, eq, fn, builtin_impls, option_methods, expect, fmt, reach";

const INTS: &str = "pub fn bits(x: u32, n: u32) -> u32 {
    x.reverse_bits() ^ x.swap_bytes() ^ x.rotate_left(n) ^ x.rotate_right(n)
        ^ x.leading_zeros() ^ x.trailing_zeros() ^ x.count_ones() ^ x.count_zeros()
}
pub fn powers(x: u64, y: u64, e: u32) -> u64 {
    let flag = if x.is_power_of_two() && x.is_multiple_of(y) { 1 } else { 0 };
    x.pow(e) + x.next_power_of_two() + x.div_ceil(y) + x.saturating_mul(y) + x.saturating_pow(e)
        + x.wrapping_neg() + x.wrapping_shl(e) + x.wrapping_shr(e) + x.wrapping_div(y)
        + x.wrapping_rem(y) + flag
}
pub fn signs(x: i32, y: i32) -> u32 {
    x.unsigned_abs() + x.abs_diff(y) + (x.signum() + x.abs() + x.wrapping_abs()) as u32
}
pub fn checked(x: i32, y: i32, s: u32) -> [Option<i32>; 9] {
    [
        x.checked_add(y), x.checked_sub(y), x.checked_mul(y), x.checked_div(y),
        x.checked_rem(y), x.checked_neg(), x.checked_shl(s), x.checked_shr(s), x.checked_pow(s),
    ]
}
pub fn overflowing(x: u8, y: u8) -> ((u8, bool), (u8, bool), (u8, bool)) {
    (x.overflowing_add(y), x.overflowing_sub(y), x.overflowing_mul(y))
}
pub fn order(x: u64, lo: u64, hi: u64) -> u64 {
    x.max(lo).min(hi) + x.clamp(lo, hi) + core::cmp::max(x, lo) + core::cmp::min(x, hi)
}
pub fn bytes(x: u32) -> u32 {
    u32::from_le_bytes(x.to_be_bytes()) ^ u32::from_be_bytes(x.to_le_bytes())
        ^ u32::from_ne_bytes(x.to_ne_bytes())
}
pub fn convert(x: u8, b: bool, w: u64) -> u64 {
    let a: u32 = x.into();
    let c = u64::from(a) + u64::from(b);
    let t: Result<u16, core::num::TryFromIntError> = w.try_into();
    let d = match t { Ok(v) => v as u64, Err(_) => 0 };
    match u8::try_from(w) { Ok(v) => c + d + v as u64, Err(_) => c + d }
}
pub fn consts() -> u64 {
    u64::MAX - u32::MAX as u64 + u8::BITS as u64 + (i32::MIN as i64 + i64::MAX) as u64
}
pub mod busy {
    // Copied from crates/blaze-core/src/busy.rs:15-32.
    /// The first wait: about what the process holding the file takes to reach its exec.
    pub const FIRST_NS: u64 = 50_000;
    /// The longest single wait.
    pub const CAP_NS: u64 = 50_000_000;
    /// All the waits of one exec, together: past them the spawn fails.
    pub const MOST_NS: u64 = 1_000_000_000;

    /// The wait before the next exec, after `tried` waits that came to `waited` nanoseconds; `None`
    /// once they have come to [`MOST_NS`].
    pub const fn next(tried: u32, waited: u64) -> Option<u64> {
        if waited >= MOST_NS {
            return None;
        }
        let doubled = FIRST_NS << if tried < 20 { tried } else { 20 };
        let wait = if doubled < CAP_NS { doubled } else { CAP_NS };
        let left = MOST_NS - waited;
        Some(if wait < left { wait } else { left })
    }

    #[blaze::theorem]
    pub fn busy_next_is_bounded_and_progresses(tried: u32, waited: u64) -> bool {
        match next(tried, waited) {
            None => waited >= MOST_NS,
            Some(w) => waited < MOST_NS && w > 0 && w <= CAP_NS && w <= MOST_NS - waited
                && (w >= FIRST_NS || waited + w == MOST_NS),
        }
    }
}
pub mod history {
    // Copied from crates/blaze-profile/src/history.rs:165, 170 and 175-177.
    pub const CAP_MOST: u64 = 256 << 20;
    pub const CAP_LEAST: u64 = 8 << 20;
    pub fn cap_for(store_bytes: u64) -> u64 {
        (store_bytes / 4).clamp(CAP_LEAST, CAP_MOST)
    }

    #[blaze::theorem]
    pub fn cap_for_is_between_least_and_most(store_bytes: u64) -> bool {
        let cap = cap_for(store_bytes);
        CAP_LEAST <= cap && cap <= CAP_MOST
    }
}
pub mod rebase {
    // Copied from crates/blaze-vfs/src/rebase.rs:69-72.
    /// Whether two ranges overlap, or meet where either is empty (two inserts at one place).
    fn clash(a0: u64, a1: u64, b0: u64, b1: u64) -> bool {
        if a0 == a1 || b0 == b1 { a0 <= b1 && b0 <= a1 } else { a0 < b1 && b0 < a1 }
    }

    #[blaze::theorem]
    pub fn a_clear_edit_lies_wholly_before_or_after(a0: u64, a1: u64, b0: u64, b1: u64) -> bool {
        a0 > a1 || b0 > b1 || clash(a0, a1, b0, b1) || (a1 <= b0 && a0 < b1) || a0 >= b1
    }
}
";

const OPTIONS: &str = "pub fn options(a: Option<u32>, b: Option<u32>) -> u32 {
    let mut n = 0;
    if a.is_some() { n += 1; }
    if b.is_none() { n += 2; }
    n + a.unwrap_or(7) + b.unwrap_or_default()
}
pub fn unwraps(a: Option<u32>, r: Result<u32, u8>, s: Result<u32, u8>) -> u32 {
    a.unwrap() + a.expect(\"a is some\") + r.unwrap() + s.expect(\"s is ok\")
}
pub fn results(r: Result<u32, u8>, s: Result<u32, u8>, t: Result<u32, u8>)
    -> (bool, bool, Option<u32>, Option<u8>, u32) {
    (r.is_ok(), r.is_err(), r.ok(), s.err(), t.unwrap_or(3))
}
pub fn ok_or(a: Option<u32>) -> Result<u32, u8> {
    a.ok_or(9)
}
pub fn takes(a: Option<u32>) -> (Option<u32>, Option<u32>, Option<u32>) {
    let mut a = a;
    let was = a.take();
    let mut b = was;
    let old = b.replace(5);
    (a, b, old)
}
";

/// `name` of `krate`, emitted from `source`, against its golden module and those of the
/// functions it calls, under `lean/BlazeModel/<krate>/`.
#[track_caller]
fn golden(flags: &str, krate: &str, name: &str, source: &str) -> Emitted {
    let out = emitted(flags, krate, name, source);
    for module in out.imports.iter().chain([&out]) {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../lean").join(module.path());
        // `UPDATE_EXPECT` writes a new golden only where its directory is.
        if std::env::var_os("UPDATE_EXPECT").is_some()
            && let Some(dir) = path.parent()
        {
            std::fs::create_dir_all(dir).expect("the golden's directory");
        }
        expect_file![path].assert_eq(&module.text);
    }
    out
}

/// Each integer method becomes its `ScalarOps` model: the total ones pure, `pow`, `abs`,
/// `next_power_of_two`, `div_ceil`, `wrapping_div`, `wrapping_rem` and `clamp` a `Result`.
#[test]
fn integer_methods_become_their_models() {
    let cases: [(&str, &[&str]); 9] = [
        ("bits", &[
            "Scalar.reverse_bits x", "Scalar.swap_bytes x", "Scalar.rotate_left x n",
            "Scalar.rotate_right x n", "Scalar.leading_zeros x", "Scalar.trailing_zeros x",
            "Scalar.count_ones x", "Scalar.count_zeros x",
        ]),
        ("powers", &[
            "← Scalar.pow x e", "← Scalar.next_power_of_two x", "← Scalar.div_ceil x y",
            "Scalar.saturating_mul x y", "Scalar.saturating_pow x e", "Scalar.wrapping_neg x",
            "Scalar.wrapping_shl x e", "Scalar.wrapping_shr x e", "← Scalar.wrapping_div x y",
            "← Scalar.wrapping_rem x y", "Scalar.is_power_of_two x", "Scalar.is_multiple_of x y",
        ]),
        ("signs", &[
            "Scalar.unsigned_abs x", "Scalar.abs_diff x y", "Scalar.signum x", "← Scalar.abs x",
            "Scalar.wrapping_abs x",
        ]),
        ("checked", &[
            "Scalar.checked_add x y", "Scalar.checked_sub x y", "Scalar.checked_mul x y",
            "Scalar.checked_div x y", "Scalar.checked_rem x y", "Scalar.checked_neg x",
            "Scalar.checked_shl x s", "Scalar.checked_shr x s", "Scalar.checked_pow x s",
        ]),
        ("overflowing", &[
            "Scalar.overflowing_add x y", "Scalar.overflowing_sub x y", "Scalar.overflowing_mul x y",
        ]),
        ("order", &["Scalar.max x lo", "Scalar.min", "← Scalar.ord_clamp x lo hi"]),
        ("bytes", &[
            "Scalar.from_le_bytes .U32", "Scalar.to_be_bytes x", "Scalar.from_be_bytes .U32",
            "Scalar.to_le_bytes x", "Scalar.from_ne_bytes .U32", "Scalar.to_ne_bytes x",
        ]),
        ("convert", &[
            "Scalar.cast .U32 x", "Scalar.cast .U64", "Scalar.ofBool .U64 b",
            "Scalar.try_from .U16 w", "Scalar.try_from .U8 w",
        ]),
        ("consts", &["18446744073709551615", "4294967295", "8", "-2147483648"]),
    ];
    for (name, models) in cases {
        let out = golden(INT_FLAGS, "ints", name, INTS);
        for model in models {
            assert!(out.text.contains(model), "`{name}` calls `{model}`:\n{}", out.text);
        }
    }
}

/// `Option`'s and `Result`'s closure-free methods become their `Std` models; `expect`'s
/// message is dropped, `unwrap_or_default` is `unwrap_or` of the zero, and `take` and
/// `replace` give back the `Option` they were lent.
#[test]
fn option_and_result_methods_become_their_models() {
    let cases: [(&str, &[&str]); 5] = [
        ("options", &[
            "core.option.Option.is_some a", "core.option.Option.is_none b",
            "core.option.Option.unwrap_or a (7 : U32)", "core.option.Option.unwrap_or b (0 : U32)",
        ]),
        ("unwraps", &[
            "← core.option.Option.unwrap a", "← core.option.Option.expect a",
            "← core.result.Result.unwrap r", "← core.result.Result.expect s",
        ]),
        ("results", &[
            "core.result.Result.is_ok r", "core.result.Result.is_err r", "core.result.Result.ok r",
            "core.result.Result.err s", "core.result.Result.unwrap_or t (3 : U32)",
        ]),
        ("ok_or", &["core.option.Option.ok_or a (9 : U8)"]),
        ("takes", &["core.option.Option.take", "core.option.Option.replace"]),
    ];
    for (name, models) in cases {
        let out = golden(OPTION_FLAGS, "options", name, OPTIONS);
        for model in models {
            assert!(out.text.contains(model), "`{name}` calls `{model}`:\n{}", out.text);
        }
        assert!(!out.text.contains("is some") && !out.text.contains("is ok"), "{}", out.text);
    }
}

/// Three functions of blaze's own, copied byte for byte, and a theorem about each: the
/// statements are what a proof in `lean/` will name.
#[test]
fn blazes_own_small_functions_translate_with_their_theorems() {
    let cases = [
        ("busy_next_is_bounded_and_progresses", "ints.busy.busy_next_is_bounded_and_progresses"),
        ("cap_for_is_between_least_and_most", "ints.history.cap_for_is_between_least_and_most"),
        (
            "a_clear_edit_lies_wholly_before_or_after",
            "ints.rebase.a_clear_edit_lies_wholly_before_or_after",
        ),
    ];
    for (name, lean) in cases {
        let out = golden(INT_FLAGS, "ints", name, INTS);
        assert_eq!(out.statement.as_deref(), Some(format!("{lean}.spec.statement").as_str()));
        assert_eq!(out.imports.len(), 1, "`{name}` imports the function it states: {}", out.text);
    }
}

/// A method that takes a closure or a function stays refused, by the construct outside the
/// subset: the closure or the function value, named at the call it is given to.
#[test]
fn a_closure_taking_method_is_refused_by_name() {
    let cases = [
        (
            "pub fn f(a: Option<u32>) -> Option<u32> { a.and_then(g) }\n\
             fn g(x: u32) -> Option<u32> { Some(x) }\n",
            "a function used as a value",
            "and_then",
        ),
        ("pub fn f(a: Option<u32>) -> u32 { a.unwrap_or_else(|| 3) }\n", "a closure", "unwrap_or_else"),
    ];
    for (source, what, call) in cases {
        let err = refused(OPTION_FLAGS, "f", source);
        assert!(err.what.contains(what), "`{call}` is refused for {what}: {err}");
        assert!(err.at.contains(call), "the refusal is at the `{call}` call: {err}");
    }
}

// ---- blaze's own crates: the models the proofs under `crates/*/lean` are about. ----

/// The pure part of `crates/blaze-core/src/busy.rs` (its constants, the schedule and its
/// theorem functions, up to the first function that sleeps), as the file has it, in a crate
/// and a module of the same names: the checked-in models are named by them.
fn busy_fixture() -> String {
    let file = include_str!("../../blaze-core/src/busy.rs");
    let from = file.find("/// The first wait").expect("busy.rs begins with its constants");
    let to = file.find("/// Waits out one wait of the schedule").expect("`wait` follows the pure part");
    format!("pub mod busy {{\n{}}}\n", &file[from..to])
}

/// The models `crates/blaze-core/lean/BlazeCore/Busy.lean` proves its theorems about are what
/// the translator emits from blaze-core's own source today. An edit of `busy.rs` that changes
/// a model changes these files (`UPDATE_EXPECT=1`), and `lake build` then checks the proofs
/// against the new ones: a proof cannot go on standing for code that has moved. When `blaze
/// prove` is the gate, the models are its outputs and this test goes.
#[test]
fn blaze_cores_busy_models_are_its_sources() {
    let source = busy_fixture();
    for name in ["next_is_bounded", "next_is_the_doubling_schedule"] {
        let out = emitted(FLAGS, "blaze_core", name, &source);
        let statement = format!("blaze_core.busy.{name}.spec.statement");
        assert_eq!(out.statement.as_deref(), Some(statement.as_str()));
        assert_eq!(out.function, format!("blaze_core::busy::{name}"));
        for module in out.imports.iter().chain([&out]) {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../lean").join(module.path());
            expect_file![path].assert_eq(&module.text);
        }
    }
    // `next` has no loop and `doubling` has one: the refinement's statement is total.
    let refines = emitted(FLAGS, "blaze_core", "next_is_the_doubling_schedule", &source);
    assert!(refines.shape.fuel && refines.text.contains("∃ (fuel : Nat)"), "{}", refines.text);
}

// ---- blaze-core's worlds (docs/v2/plan/design/proofs-for-sans-io.md, section 2). ----

/// What the worlds' sources use: `Vec` and its writes through an index, `Option`'s methods,
/// the integers' saturating and bounds.
const WORLD_FLAGS: &str = "sized, copy, add, ord, eq, fn, builtin_impls, vec, mem_take, drop, index, slice, \
    range, option, option_methods, int_methods, int_bounds, int_cast, ord_methods";

/// `crates/blaze-core/src/world/clock.rs` and `disk.rs` as the files have them, in a crate and
/// modules of the same names (the checked-in models are named by them): each without its
/// tests, its `sans_io` mark and its derives, which no model reads and minicore does not
/// have, and with `Global` named where minicore has it.
fn world_fixture() -> String {
    let code = |file: &str| -> String {
        let code = file.split("\n#[cfg(test)]\nmod tests {").next().expect("a file's code comes first");
        code.lines()
            .filter(|line| !line.starts_with("#![blaze::sans_io]") && !line.trim_start().starts_with("#[derive("))
            .map(|line| line.replace("use std::alloc::Global;", "use core::alloc::Global;"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "pub mod world {{\npub mod clock {{\n{}\n}}\npub mod disk {{\n{}\n}}\n}}\n",
        code(include_str!("../../blaze-core/src/world/clock.rs")),
        code(include_str!("../../blaze-core/src/world/disk.rs")),
    )
}

/// The models the worlds' proofs are about (`crates/blaze-core/lean/BlazeCore/World/`) are
/// what the translator emits from blaze-core's own source today, as for `busy` above: the
/// clock's theorem and the disk's three crash theorems, with every function of the worlds
/// they reach. When `blaze prove` is the gate, the models are its outputs and this test goes.
#[test]
fn blaze_cores_world_models_are_its_sources() {
    let source = world_fixture();
    for (module, name) in [
        ("clock", "a_timer_never_fires_early"),
        ("disk", "a_crash_keeps_a_synced_file"),
        ("disk", "a_crash_cuts_an_appended_file_to_a_prefix"),
        ("disk", "a_crash_keeps_what_a_barrier_ordered"),
    ] {
        let out = emitted(WORLD_FLAGS, "blaze_core", name, &source);
        let statement = format!("blaze_core.world.{module}.{name}.spec.statement");
        assert_eq!(out.statement.as_deref(), Some(statement.as_str()));
        assert_eq!(out.function, format!("blaze_core::world::{module}::{name}"));
        for module in out.imports.iter().chain([&out]) {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../lean").join(module.path());
            // A model of a type or function new to the worlds is a file in a new directory,
            // which `UPDATE_EXPECT` writes only where the directory is.
            if std::env::var_os("UPDATE_EXPECT").is_some()
                && let Some(dir) = path.parent()
            {
                std::fs::create_dir_all(dir).expect("the model's directory");
            }
            expect_file![path].assert_eq(&module.text);
        }
    }
}

// ---- `Vec<T>` (docs/v2/plan/design/proofs-for-sans-io.md, section 3 step d). ----

/// What the `vecs` and `queue` fixtures enable: `Vec`, its macro and `Default`, slices and
/// their indexing, `mem::{replace, swap, take}`, `Option` and drops.
const VEC_FLAGS: &str =
    "sized, copy, add, ord, eq, fn, builtin_impls, vec, mem_take, drop, index, slice, range, option";

/// One function per group of `Vec` operations: making, reading, writing through `&mut`,
/// writing through an element, `mem`, and `Vec` nested in other types.
const VECS: &str = "use core::alloc::Global;
pub struct Pair { pub a: u32, pub b: u32 }
pub fn making(n: usize) -> usize {
    let a: Vec<u32> = Vec::new();
    let b: Vec<u32> = Vec::with_capacity(n);
    let c: Vec<u32> = Vec::new_in(Global);
    let d: Vec<u32, Global> = Vec::with_capacity_in(n, Global);
    let e: Vec<u32> = vec![];
    let f = vec![7u8; n];
    a.len() + b.len() + c.len() + d.len() + e.len() + f.len()
}
pub fn reading(v: &Vec<usize>, i: usize) -> usize {
    let mut total = v[i];
    match v.get(i) { Some(x) => total += *x, None => {} }
    match v.first() { Some(x) => total += *x, None => {} }
    match v.last() { Some(x) => total += *x, None => {} }
    if v.is_empty() { total += 1; }
    let s: &[usize] = v;
    let t = &v[..];
    total + v.as_slice().len() + s.len() + t.len() + v.len()
}
pub fn writing(v: &mut Vec<u32>, w: &[u32], x: u32, i: usize) -> Option<u32> {
    v.push(x);
    v.insert(0, x);
    v.swap(0, i);
    let r = v.remove(i);
    let s = v.swap_remove(0);
    v.truncate(3);
    v.extend_from_slice(w);
    v.push(r + s);
    let top = v.pop();
    v.clear();
    top
}
pub fn elements(v: &mut Vec<u32>, pairs: &mut Vec<Pair>, i: usize, x: u32) {
    v[i] = x;
    v[i] += 1;
    pairs[i].a = x;
    let mut j = i;
    pairs[j].b = pairs[j].a;
    j = 0;
    pairs[j].a = 3;
}
pub fn mems(a: &mut Vec<u32>, b: &mut Vec<u32>, n: &mut u32, o: &mut Option<u32>, f: &mut bool) -> u32 {
    let old = core::mem::replace(n, 5);
    core::mem::swap(a, b);
    let taken = core::mem::take(a);
    let was = core::mem::take(o);
    let flag = core::mem::take(f);
    let k = core::mem::take(n);
    let first = match was { Some(x) => x, None => 0 };
    if flag { old + k + first } else { taken.len() as u32 }
}
pub fn nested(rows: &mut Vec<Vec<u32>>, i: usize, x: u32, spare: Option<Vec<u32>>) -> usize {
    rows[i].push(x);
    let n = rows[i].len();
    match spare { Some(v) => n + v.len(), None => n }
}
";

/// Each group of `Vec` operations becomes its `Blaze.Vec` model: `Vec` is a slice's value,
/// the reads are pure, `push`, `insert`, `remove`, `swap_remove`, `extend_from_slice` and
/// `swap` a `Result`, and a `&mut` into an element (`v[i] = x`, `v[i].a = x`) writes the
/// element in place.
#[test]
fn vec_operations_become_their_models() {
    let cases: [(&str, &[&str]); 6] = [
        ("making", &["(Vec.new : Vec U32)", "Vec.from_elem (7 : U8) n", "Slice.len"]),
        ("reading", &[
            "← Slice.index v i", "Slice.get v i", "Slice.first v", "Slice.last v",
            "Slice.is_empty v",
        ]),
        ("writing", &[
            "← Vec.push", "← Vec.insert", "← Slice.swap", "← Vec.remove", "← Vec.swap_remove",
            "Vec.truncate", "← Vec.extend_from_slice", "Vec.pop", "Vec.clear",
        ]),
        ("elements", &["Slice.update", "Slice.index"]),
        ("mems", &["(Vec.new : Vec U32)", "(0 : U32)", "false"]),
        ("nested", &["← Vec.push", "Vec (Vec U32)", "Option (Vec U32)"]),
    ];
    for (name, models) in cases {
        let out = golden(VEC_FLAGS, "vecs", name, VECS);
        for model in models {
            assert!(out.text.contains(model), "`{name}` has `{model}`:\n{}", out.text);
        }
    }
}

/// What the `Vec` subset leaves out is refused by name: its capacity, a closure, a range
/// `get`, a `Vec` in another allocator, a generic `Vec` function, `mem::take` of a type with
/// no modelled default, and a `&mut` into a `Vec` chosen at run time.
#[test]
fn what_the_vec_subset_leaves_out_is_refused_by_name() {
    let cases = [
        ("pub fn f(v: &Vec<u32>) -> usize { v.capacity() }\n", "Vec::capacity"),
        ("pub fn f(v: &mut Vec<u32>) { v.retain(|x| *x > 0) }\n", "closure"),
        ("pub fn f(v: &Vec<u32>) -> usize { match v.get(0..1) { Some(s) => s.len(), None => 0 } }\n", "`Range`"),
        ("pub fn f<T>(v: Vec<T>) -> usize { v.len() }\n", "generic function"),
        (
            "pub struct P { pub x: u32 }\n\
             impl Default for P { fn default() -> P { P { x: 0 } } }\n\
             pub fn f(p: &mut P) -> u32 { let q = core::mem::take(p); q.x }\n",
            "mem::take",
        ),
        (
            "pub fn f(v: &mut Vec<u32>, i: usize, c: bool) { let r = if c { &mut v[i] } else { &mut v[0] }; *r = 1; }\n",
            "`&mut`",
        ),
    ];
    for (source, what) in cases {
        let err = refused(VEC_FLAGS, "f", source);
        assert!(err.what.contains(what), "`{source}` is refused for {what}: {err}");
        assert_eq!(err.function, "main::f");
    }
}

/// A bounded queue over a `Vec`, the state machine V15 is for: its well-formedness and three
/// theorem functions, whose statements `lean/BlazeModel/queue/proved.lean` proves.
const QUEUE: &str = "pub struct Queue { items: Vec<u32>, limit: usize }
impl Queue {
    pub fn new(limit: usize) -> Queue { Queue { items: Vec::new(), limit } }
    pub fn wf(&self) -> bool { self.items.len() <= self.limit }
    /// Adds `item` unless the queue is full; whether it was added.
    pub fn offer(&mut self, item: u32) -> bool {
        if self.items.len() < self.limit { self.items.push(item); true } else { false }
    }
    /// Takes the newest item, if any.
    pub fn take(&mut self) -> Option<u32> { self.items.pop() }
}
#[blaze::theorem] pub fn new_is_wf(limit: usize) -> bool { Queue::new(limit).wf() }
#[blaze::theorem] pub fn offer_keeps_wf(queue: Queue, item: u32) -> bool {
    if !queue.wf() { return true; }
    let mut queue = queue; queue.offer(item); queue.wf()
}
#[blaze::theorem] pub fn take_keeps_wf(queue: Queue) -> bool {
    if !queue.wf() { return true; }
    let mut queue = queue; queue.take(); queue.wf()
}
";

#[test]
fn a_queue_over_a_vec_states_its_theorems() {
    for name in ["new_is_wf", "offer_keeps_wf", "take_keeps_wf"] {
        let out = golden(VEC_FLAGS, "queue", name, QUEUE);
        let statement = format!("queue.{name}.spec.statement");
        assert_eq!(out.statement.as_deref(), Some(statement.as_str()));
    }
}

// ---- What the worlds are written with, one construct a function. ----

/// The constructs of `crates/blaze-core/src/world/`, each alone: an or-pattern whose
/// alternatives bind the same names, `let .. else`, an early `return` in a `while` whose
/// condition is `&&`, a tuple returned and taken apart, `saturating_add`, `u64::MAX` and a
/// constant made by a shift, `Option::take`, `is_none` and `is_some` of a field of a `Vec`'s
/// element, a struct written over an element (the old one dropped), `&mut self.field` lent to a
/// free function, a `&Vec<u8>` given as a `&[u8]`, `Vec::remove(0)` in a loop, and nested
/// `while`s.
const WORLD_SHAPES: &str = "pub const BIG: usize = 1 << 40;
pub enum Change { At(usize, u32), Sectors(usize, u32), Cut(usize) }
pub struct Slot { pub undo: Option<u32>, pub bytes: Vec<u8> }
pub fn either(change: &Change) -> usize {
    match change {
        Change::At(at, more) | Change::Sectors(at, more) => *at + *more as usize,
        Change::Cut(len) => *len,
    }
}
pub fn kind(change: &Change) -> u32 {
    match change {
        Change::Cut(_) => 0,
        Change::At(_, _) | Change::Sectors(_, _) => 1,
    }
}
pub fn size(change: &Change) -> usize {
    match change {
        Change::Cut(len) => *len,
        Change::At(at, more) | Change::Sectors(at, more) => *at + *more as usize,
    }
}
pub fn first_or(v: &Vec<u32>) -> u32 {
    let Some(x) = v.first() else { return 0 };
    *x
}
pub fn position(v: &Vec<u32>, x: u32, most: usize) -> Option<usize> {
    let mut i = 0;
    while i < v.len() && i < most {
        if v[i] == x { return Some(i); }
        i += 1;
    }
    None
}
fn pair(x: u64) -> (bool, u64) { (x > 3, x.saturating_add(u64::MAX)) }
pub fn unpair(x: u64) -> u64 {
    let (big, y) = pair(x);
    if big || y == u64::MAX { y } else { BIG as u64 }
}
pub fn slots(v: &mut Vec<Slot>, i: usize, s: Slot) -> bool {
    let was = v[i].undo.take();
    let none = v[i].undo.is_none();
    v[i] = s;
    was.is_some() && none
}
fn grow(bytes: &mut Vec<u8>, more: &[u8]) { bytes.extend_from_slice(more); }
impl Slot {
    pub fn grow(&mut self, more: &Vec<u8>) { grow(&mut self.bytes, more); }
}
pub fn grown(slot: Slot, more: Vec<u8>) -> Slot {
    let mut slot = slot;
    slot.grow(&more);
    slot
}
pub fn drain(v: &mut Vec<u32>, n: usize) -> u32 {
    let mut total = 0;
    let mut done = 0;
    while done < n && !v.is_empty() {
        let x = v.remove(0);
        let mut k = 0;
        while k < x { total += 1; k += 1; }
        done += 1;
    }
    total
}
";

/// Each construct the worlds use becomes the model its golden holds: the or-pattern one
/// `match` arm per alternative, each binding its own fields, one without bindings the `_`
/// arm, and one after another arm more arms of the same match; `let .. else` a `match` whose
/// other arm returns; the early `return` the loop function's result; the constant its value.
#[test]
fn the_worlds_constructs_become_their_models() {
    let cases: [(&str, &[&str]); 9] = [
        ("size", &["| blaze_shapes.Change.Cut v0 =>", "| blaze_shapes.Change.Sectors v3 v4 =>"]),
        ("kind", &["| blaze_shapes.Change.Cut v0 => (0 : U32)", "| _ => (1 : U32)"]),
        ("either", &["blaze_shapes.Change.At", "blaze_shapes.Change.Sectors", "blaze_shapes.Change.Cut"]),
        ("first_or", &["match Slice.first v with", "| Option.none => (0 : U32)"]),
        ("position", &["blaze_shapes.position.loop_0", "Option.some"]),
        ("unpair", &["blaze_shapes.pair x", "Scalar.saturating_add x (18446744073709551615 : U64)", "(1099511627776 : Usize)"]),
        ("slots", &["core.option.Option.take", "core.option.Option.is_none", "Slice.update", "core.option.Option.is_some"]),
        ("grown", &["blaze_shapes.grow self.bytes more", "{ self with bytes := x0 }"]),
        ("drain", &["← Vec.remove v (0 : Usize)", "blaze_shapes.drain.loop_0", "blaze_shapes.drain.loop_1"]),
    ];
    for (name, models) in cases {
        let out = golden(WORLD_FLAGS, "blaze_shapes", name, WORLD_SHAPES);
        // The function's module and those of what it calls (`pair` for `unpair`).
        let text: String = out.imports.iter().chain([&out]).map(|it| it.text.as_str()).collect();
        for model in models {
            assert!(text.contains(model), "`{name}` has `{model}`:\n{text}");
        }
        // MIR's second test of an or-pattern, inside the arm that chose it, is gone: the
        // variants left are known there, and no arm for one it cannot be is written.
        if matches!(name, "kind" | "either" | "size") {
            assert!(!text.contains("unreachable"), "`{name}`:\n{text}");
        }
        // The arms after the first are the outer match's own, not a second match in its `_`.
        if name == "size" {
            assert!(!text.contains("| _ =>"), "`{name}`:\n{text}");
        }
    }
}
