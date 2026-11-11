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
use hegel::{TestCase, generators as gs};
use hir_def::{FunctionId, ModuleDefId, nameres::crate_def_map, signatures::FunctionSignature};
use hir_expand::fixture;
// Links the crate that provides the MIR queries.
use hir_ty_mir as _;

use hir_def::{DefWithBodyId, HasModule as _};
use rustc_type_ir::inherent::GenericArgs as _;
use hir_ty::db::HirDatabase as _;
use hir_ty_mir::mir::budget::{self, Pass};

use crate::{
    Emitted, body::*, driver, emit, embed_mir, embed_mir_body, lean,
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

/// A source of draws: Hegel's, or a seed's.
trait Draw {
    /// In `lo..=hi`.
    fn int(&mut self, lo: u64, hi: u64) -> u64;
}

struct Drawn<'a>(&'a TestCase);

impl Draw for Drawn<'_> {
    fn int(&mut self, lo: u64, hi: u64) -> u64 {
        self.0.draw(gs::integers::<u64>().min_value(lo).max_value(hi))
    }
}

/// SplitMix64, for the controls' seeded programs.
struct Seeded(u64);

impl Draw for Seeded {
    fn int(&mut self, lo: u64, hi: u64) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^= z >> 31;
        lo + z % (hi - lo + 1)
    }
}

/// A structured program whose every jump has a label of its kind: `labels` are those in
/// scope, `true` for a loop.
fn program(d: &mut impl Draw, depth: u32, labels: &mut Vec<bool>) -> Program {
    let mut out = Vec::new();
    for _ in 0..d.int(0, 4) {
        let var = d.int(0, 2) as usize;
        let kind = if depth == 0 { d.int(0, 4) } else { d.int(0, 11) };
        match kind {
            0 => out.push(Node::Do(Op::Add(var, d.int(1, 3)))),
            1 => out.push(Node::Do(Op::Set(var, d.int(0, 5)))),
            2 | 3 => out.push(Node::Do(Op::Trace(d.int(0, 9)))),
            4 => out.push(Node::Do(Op::PanicIf(var, d.int(0, 40)))),
            5 | 6 => {
                let on = if d.int(0, 1) == 0 { Test::Low(var) } else { Test::Less(var, d.int(1, 6)) };
                let arms = (0..d.int(1, 2))
                    .map(|value| (u128::from(value), program(d, depth - 1, labels)))
                    .collect();
                out.push(Node::Branch { on, arms, otherwise: program(d, depth - 1, labels) });
            }
            7 => {
                labels.push(false);
                out.push(Node::Block(program(d, depth - 1, labels)));
                labels.pop();
            }
            8 => {
                // The loop is in a block, which a `Break` in its body leaves.
                labels.push(false);
                labels.push(true);
                let mut body = program(d, depth - 1, labels);
                labels.pop();
                labels.pop();
                // A turn moves a variable, so most loops end.
                body.push(Node::Do(Op::Add(var, 1)));
                out.push(Node::Block(vec![Node::Branch {
                    on: Test::Less(var, d.int(2, 8)),
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
                out.push(if d.int(0, 3) == 0 { Node::End(End::Panic) } else { Node::Return });
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
#[hegel::test(test_cases = 300)]
fn structured_code_runs_as_its_graph(tc: TestCase) {
    let written = program(&mut Drawn(&tc), 3, &mut Vec::new());
    if let Err(broke) = agrees(&written, Plant::None) {
        panic!("{broke}\nwritten {written:?}");
    }
}

const CONTROL_SEEDS: u64 = 400;

/// Whether the oracle catches `plant` in one of the seeded programs.
fn caught(plant: Plant) -> bool {
    (0..CONTROL_SEEDS).any(|seed| {
        let written = program(&mut Seeded(seed), 3, &mut Vec::new());
        agrees(&written, plant).is_err()
    })
}

#[test]
fn the_seeded_programs_agree_unplanted() {
    for seed in 0..CONTROL_SEEDS {
        let written = program(&mut Seeded(seed), 3, &mut Vec::new());
        if let Err(broke) = agrees(&written, Plant::None) {
            panic!("seed {seed}: {broke}\nwritten {written:?}");
        }
    }
}

#[test]
fn the_oracle_catches_a_continue_made_a_break() {
    assert!(caught(Plant::ContinueAsBreak));
}

#[test]
fn the_oracle_catches_an_else_branch_dropped_at_a_join() {
    assert!(caught(Plant::ElseDropped));
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

/// `fnv1a_from` as crates/blaze-graph/src/ledger.rs has it, in a crate and a module of the
/// same names, with the clauses blaze's own crates cannot carry yet (`blaze_verify` is not on
/// for them): the signature's return is named and an `ensures` follows it. The body is the
/// tree's, byte for byte.
fn fnv1a_fixture() -> String {
    let ledger = include_str!("../../blaze-graph/src/ledger.rs");
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
/// proof is of the model this test holds to the tree's source: the function's text is read
/// from crates/blaze-graph/src/ledger.rs, its emitted Lean must be the checked-in module the
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

// ---- The driver. ----

#[test]
fn the_axioms_lean_prints_are_read_and_held_to_the_allowed_ones() {
    let listed = "'m.f.spec' depends on axioms: [propext,\n Classical.choice,\n Quot.sound]\n";
    let axioms = driver::printed_axioms(listed, "m.f.spec").expect("listed");
    assert_eq!(axioms, ["propext", "Classical.choice", "Quot.sound"]);
    assert_eq!(driver::judge("m::f", "m.f.spec", &axioms), Ok(()));
    let none = "'m.f.spec' does not depend on any axioms\n";
    assert_eq!(driver::printed_axioms(none, "m.f.spec"), Some(Vec::new()));
    assert_eq!(driver::printed_axioms("warning: unused\n", "m.f.spec"), None);
    let sorry = driver::printed_axioms("'m.f.spec' depends on axioms: [sorryAx]", "m.f.spec");
    assert!(matches!(
        driver::judge("m::f", "m.f.spec", &sorry.expect("listed")),
        Err(crate::LeanError::Sorry { .. })
    ));
    let native = ["propext".to_owned(), "Lean.ofReduceBool".to_owned()];
    match driver::judge("m::f", "m.f.spec", &native) {
        Err(crate::LeanError::Axiom { axiom, .. }) => assert_eq!(axiom, "Lean.ofReduceBool"),
        other => panic!("native evaluation is refused: {other:?}"),
    }
}

#[test]
fn a_proof_key_covers_the_model_the_proof_and_the_check() {
    let db = db(&fnv1a_fixture());
    let out = emit(&db, fnv1a(&db)).expect("translated");
    let key = crate::proof_key(&out, "check", "proof");
    assert_eq!(key, crate::proof_key(&out, "check", "proof"));
    assert_eq!(key.how, crate::Prover::Lean);
    assert_ne!(key, crate::proof_key(&out, "check", "another proof"));
    assert_ne!(key, crate::proof_key(&out, "another check", "proof"));
    let mut changed = out.clone();
    changed.text.push_str("-- changed\n");
    assert_ne!(key, crate::proof_key(&changed, "check", "proof"));
}

/// End to end, with the pinned Lean from `BLAZE_LEAN_PROVER`; without it the test fails,
/// never skips.
#[test]
fn lean_checks_the_first_proof_and_its_axioms() {
    let db = db(&fnv1a_fixture());
    let out = std::env::temp_dir().join(format!("blaze-hir-ty-lean-{}", std::process::id()));
    let config = crate::Config::from_env(out.clone()).unwrap_or_else(|err| panic!("{err}"));
    let proof = crate::ProofFile {
        root: std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../blaze-graph/lean"),
        module: "BlazeGraph.Ledger".to_owned(),
    };
    let proved = crate::check(&db, fnv1a(&db), &proof, &config);
    let _ = std::fs::remove_dir_all(&out);
    let proved = proved.unwrap_or_else(|err| panic!("{err}"));
    assert_eq!(proved.theorem, "blaze_graph.ledger.fnv1a_from.spec");
    assert!(proved.axioms.iter().all(|it| crate::ALLOWED_AXIOMS.contains(&it.as_str())));
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
    let before = embed_mir_body(db, func, &before).unwrap_or_else(|err| panic!("{err}"));
    let after = embed_mir_body(db, func, &after).unwrap_or_else(|err| panic!("{err}"));
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
