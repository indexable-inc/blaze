//! The head-shape prefilter against unification (item "head shape", 2026-11-16).
//!
//! The solver drops an impl candidate by its fingerprint (prefilter.rs: the head of each trait
//! argument and of its first three children, a wildcard, ANY, for whatever can still become
//! anything) before it imports the impl's header and makes fresh arguments for it
//! (trait_goals.rs `assemble_impls`, then `step_impl`). The drop must be exactly conservative:
//! an impl the fingerprints drop never unifies with the goal. The Reference, "Implementations":
//! a trait implementation applies to a trait reference when its header, its generic parameters
//! instantiated, is that reference; the solver checks it as `fresh_impl_args` then `eq_lists`,
//! whose relation is the term arena's `Tys::unify`: that is the oracle here.
//!
//! (a) `head_shape_never_drops_an_impl_that_unifies`: generated goals (concrete heads, the
//!     goal's own rigid parameters, type, integer and float variables, errors, nested up to
//!     three levels, one to five trait arguments: past the four the fingerprint reads) against
//!     one to forty generated impl headers (concrete heads and the impl's own type and const
//!     parameters); every impl the columns filter drops must fail to unify. Its control plants
//!     an impl fingerprint that reads the impl's own parameters as rigid (not ANY) and watches
//!     the oracle catch it.
//! (b) `head_shape_imports_only_matching_and_any`: forty impls of one trait, a goal with a known
//!     self type and argument: the impl candidates assembled (each one header imported and
//!     fresh arguments made, `Stats::impl_candidates`) are the matching impl and the ANY one.

use std::collections::HashMap;
use std::format;
use std::string::{String, ToString};
use std::vec::Vec;

use blaze_own::Alloc;
use blaze_sim::{Broke, Config, Tape};

use crate::assembly::{Clause, Goal};
use crate::error::SolveError;
use crate::eval::{Answer, Solver};
use crate::facts::{Portable, Read};
use crate::prefilter::{self, Columns, Fingerprint};
use crate::term::{Def, FloatTy, IntTy, List, Mutbl, Ty, TyKind, TypingMode, UintTy, ValTy, VarKind};
use crate::testing::parse::{CaseA, CheckA, DeclA, parse_file};
use crate::testing::run::PRELUDE;
use crate::testing::tys::TestTys;
use crate::testing::world::{Arena, Scope, World};
use crate::tys::{Relate, Tys};

/// The concrete leaves.
const PRIMS: [TyKind; 8] = [
    TyKind::Bool,
    TyKind::Char,
    TyKind::Str,
    TyKind::Never,
    TyKind::Int(IntTy::I32),
    TyKind::Uint(UintTy::U32),
    TyKind::Uint(UintTy::U8),
    TyKind::Float(FloatTy::F64),
];

/// A term with no children.
#[derive(Clone, Copy, Debug)]
enum Leaf {
    Prim(u8),
    Unit,
    /// A struct without parameters, one of three.
    Adt0(u8),
    /// A parameter: the goal's own (rigid) on the goal side, the impl's own (ANY) on the impl's.
    Param(u8),
    Var,
    IntVar,
    FloatVar,
    Error,
}

/// An array's length.
#[derive(Clone, Copy, Debug)]
enum Len {
    Lit(u8),
    Param(u8),
    Var,
}

/// One step of a term in postfix order: a leaf, or a constructor over the terms before it.
#[derive(Clone, Copy, Debug)]
enum Op {
    Leaf(Leaf),
    Ref(Mutbl),
    Ptr(Mutbl),
    Slice,
    Array(Len),
    /// A tuple of the last `n` terms (`n` at least 1; the unit is a leaf).
    Tuple(u8),
    /// A struct with one parameter, one of two.
    Adt1(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Side {
    Goal,
    Impl,
}

/// Which impl fingerprint the case builds: the real one, or the planted bug.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Head {
    Real,
    /// The impl's own parameters read as rigid, like the goal's, not as ANY.
    ParamRigid,
}

fn mutbl(t: &Tape) -> Mutbl {
    if t.coin("mutable") { Mutbl::Mut } else { Mutbl::Not }
}

fn leaf(t: &Tape, side: Side) -> Leaf {
    let kinds = match side {
        Side::Goal => 8,
        Side::Impl => 4,
    };
    match t.below(kinds, "leaf") {
        0 => Leaf::Prim(t.below(PRIMS.len() as u64, "primitive") as u8),
        1 => Leaf::Unit,
        2 => Leaf::Adt0(t.below(3, "struct") as u8),
        3 => Leaf::Param(t.below(3, "parameter") as u8),
        4 => Leaf::Var,
        5 => Leaf::IntVar,
        6 => Leaf::FloatVar,
        _ => Leaf::Error,
    }
}

fn len(t: &Tape, side: Side) -> Len {
    match (side, t.below(3, "length")) {
        (_, 0) => Len::Lit(t.below(2, "length value") as u8),
        (Side::Goal, 1) => Len::Var,
        _ => Len::Param(t.below(2, "const parameter") as u8),
    }
}

/// A term of up to three levels, in postfix order, appended to `ops` (a loop: no recursion).
fn term(t: &Tape, side: Side, ops: &mut Vec<Op>) {
    ops.push(Op::Leaf(leaf(t, side)));
    for _ in 0..t.below(3, "depth") {
        let op = match t.below(6, "constructor") {
            0 => Op::Ref(mutbl(t)),
            1 => Op::Ptr(mutbl(t)),
            2 => Op::Slice,
            3 => Op::Array(len(t, side)),
            4 => {
                let n = 1 + t.below(3, "tuple arity") as u8;
                for _ in 1..n {
                    ops.push(Op::Leaf(leaf(t, side)));
                }
                Op::Tuple(n)
            }
            _ => Op::Adt1(t.below(2, "struct with a parameter") as u8),
        };
        ops.push(op);
    }
}

fn solve_err(e: SolveError) -> Broke {
    Broke::Harness(format!("the term arena refused a term: {e}"))
}

/// Where a term is built: an impl header's portable arena, or the term arena.
trait Mk {
    fn node(&mut self, k: TyKind) -> Result<Ty, Broke>;
    fn list(&mut self, items: &[Ty]) -> Result<List, Broke>;
    fn param(&mut self, i: u8) -> Result<Ty, Broke>;
    fn const_param(&mut self, i: u8) -> Result<Ty, Broke>;
    fn var(&mut self, k: VarKind) -> Result<Ty, Broke>;
}

/// The impls' portable arena (what `ImplHeader::portable` holds), parameters as `Param`.
#[derive(Default)]
struct PortMk {
    nodes: Vec<TyKind>,
    lists: Vec<Ty>,
}

impl Mk for PortMk {
    fn node(&mut self, k: TyKind) -> Result<Ty, Broke> {
        self.nodes.push(k);
        Ok(Ty(self.nodes.len() as u32 - 1))
    }

    fn list(&mut self, items: &[Ty]) -> Result<List, Broke> {
        let start = self.lists.len() as u32;
        self.lists.extend_from_slice(items);
        Ok(List { start, len: items.len() as u32 })
    }

    fn param(&mut self, i: u8) -> Result<Ty, Broke> {
        self.node(TyKind::Param(u32::from(i)))
    }

    fn const_param(&mut self, i: u8) -> Result<Ty, Broke> {
        self.node(TyKind::ConstParam(u32::from(i)))
    }

    fn var(&mut self, k: VarKind) -> Result<Ty, Broke> {
        Err(Broke::Harness(format!("an impl header holds no inference variable ({k:?})")))
    }
}

/// The term arena: parameters rigid (`Param`), or each of an impl's parameters one fresh
/// variable (`fresh_impl_args`).
struct TysMk<'t> {
    tys: &'t mut TestTys,
    rigid: bool,
    params: [Ty; 3],
    consts: [Ty; 3],
}

impl<'t> TysMk<'t> {
    fn new(tys: &'t mut TestTys, rigid: bool) -> TysMk<'t> {
        TysMk { tys, rigid, params: [Ty::NONE; 3], consts: [Ty::NONE; 3] }
    }
}

impl Mk for TysMk<'_> {
    fn node(&mut self, k: TyKind) -> Result<Ty, Broke> {
        self.tys.mk(k).map_err(solve_err)
    }

    fn list(&mut self, items: &[Ty]) -> Result<List, Broke> {
        self.tys.mk_list(items).map_err(solve_err)
    }

    fn param(&mut self, i: u8) -> Result<Ty, Broke> {
        if self.rigid {
            return self.node(TyKind::Param(u32::from(i)));
        }
        let slot = self.params.get_mut(i as usize).ok_or_else(|| Broke::Harness(format!("parameter {i} past three")))?;
        if *slot == Ty::NONE {
            *slot = self.tys.fresh(VarKind::Ty).map_err(solve_err)?;
        }
        Ok(*slot)
    }

    fn const_param(&mut self, i: u8) -> Result<Ty, Broke> {
        if self.rigid {
            return self.node(TyKind::ConstParam(u32::from(i)));
        }
        let slot = self.consts.get_mut(i as usize).ok_or_else(|| Broke::Harness(format!("const parameter {i} past three")))?;
        if *slot == Ty::NONE {
            *slot = self.tys.fresh(VarKind::Const).map_err(solve_err)?;
        }
        Ok(*slot)
    }

    fn var(&mut self, k: VarKind) -> Result<Ty, Broke> {
        self.tys.fresh(k).map_err(solve_err)
    }
}

fn pop(stack: &mut Vec<Ty>, ops: &[Op]) -> Result<Ty, Broke> {
    stack.pop().ok_or_else(|| Broke::Harness(format!("a constructor without its child in {ops:?}")))
}

/// Builds the postfix term `ops` in `m` (a stack: no recursion).
fn build<M: Mk>(m: &mut M, ops: &[Op]) -> Result<Ty, Broke> {
    let mut stack: Vec<Ty> = Vec::new();
    for &op in ops {
        let t = match op {
            Op::Leaf(l) => match l {
                Leaf::Prim(i) => m.node(PRIMS[i as usize])?,
                Leaf::Unit => {
                    let l = m.list(&[])?;
                    m.node(TyKind::Tuple(l))?
                }
                Leaf::Adt0(d) => {
                    let l = m.list(&[])?;
                    m.node(TyKind::Adt(Def(0x100 + u128::from(d)), l))?
                }
                Leaf::Param(i) => m.param(i)?,
                Leaf::Var => m.var(VarKind::Ty)?,
                Leaf::IntVar => m.var(VarKind::Int)?,
                Leaf::FloatVar => m.var(VarKind::Float)?,
                Leaf::Error => m.node(TyKind::Error)?,
            },
            Op::Ref(mu) => {
                let c = pop(&mut stack, ops)?;
                let r = m.node(TyKind::ReStatic)?;
                m.node(TyKind::Ref(r, c, mu))?
            }
            Op::Ptr(mu) => {
                let c = pop(&mut stack, ops)?;
                m.node(TyKind::RawPtr(c, mu))?
            }
            Op::Slice => {
                let c = pop(&mut stack, ops)?;
                m.node(TyKind::Slice(c))?
            }
            Op::Array(n) => {
                let c = pop(&mut stack, ops)?;
                let n = match n {
                    Len::Lit(v) => m.node(TyKind::ConstValue(ValTy::Uint(UintTy::Usize), u128::from(v)))?,
                    Len::Param(i) => m.const_param(i)?,
                    Len::Var => m.var(VarKind::Const)?,
                };
                m.node(TyKind::Array(c, n))?
            }
            Op::Tuple(n) => {
                let at = stack
                    .len()
                    .checked_sub(n as usize)
                    .ok_or_else(|| Broke::Harness(format!("a tuple of {n} without its children in {ops:?}")))?;
                let l = m.list(&stack[at..])?;
                stack.truncate(at);
                m.node(TyKind::Tuple(l))?
            }
            Op::Adt1(d) => {
                let c = pop(&mut stack, ops)?;
                let l = m.list(&[c])?;
                m.node(TyKind::Adt(Def(0x200 + u128::from(d)), l))?
            }
        };
        stack.push(t);
    }
    match stack.as_slice() {
        [t] => Ok(*t),
        _ => Err(Broke::Harness(format!("{ops:?} left {} terms", stack.len()))),
    }
}

/// Whether the impl `header` unifies with the goal's arguments `gargs`: its parameters made
/// fresh variables, then argument by argument (`eq_lists`). The caller rolls back.
fn unifies(
    tys: &mut TestTys,
    header: &[Vec<Op>],
    gargs: &[Ty],
    deferred: &mut blaze_own::Vec<(Ty, Ty), Alloc<'_>>,
) -> Result<bool, Broke> {
    let mut args: Vec<Ty> = Vec::new();
    {
        let mut m = TysMk::new(tys, false);
        for ops in header {
            args.push(build(&mut m, ops)?);
        }
    }
    for (&g, &a) in gargs.iter().zip(args.iter()) {
        deferred.clear();
        if !tys.unify(g, a, Relate::Lazy, deferred).map_err(solve_err)? {
            return Ok(false);
        }
        if !deferred.is_empty() {
            return Err(Broke::Harness(format!("no alias is generated, yet {} pairs were deferred", deferred.len())));
        }
    }
    Ok(true)
}

/// One case: a goal, up to forty impl headers, their columns filtered by the goal's
/// fingerprint; every dropped impl is unified with the goal.
fn head_shape_case(t: &Tape, head: Head) -> Result<(), Broke> {
    let alloc = blaze_own::testing::heap().alloc();
    // One to five arguments: the fingerprint reads four, the fifth is never filtered on.
    let arity = 1 + t.below(5, "trait arguments") as usize;
    let mut tys = TestTys::new();
    let mut goal_ops: Vec<Vec<Op>> = Vec::new();
    for _ in 0..arity {
        let mut ops = Vec::new();
        term(t, Side::Goal, &mut ops);
        goal_ops.push(ops);
    }
    let mut gargs: Vec<Ty> = Vec::new();
    {
        let mut m = TysMk::new(&mut tys, true);
        for ops in &goal_ops {
            gargs.push(build(&mut m, ops)?);
        }
    }
    let glist = tys.mk_list(&gargs).map_err(solve_err)?;
    let gfp = prefilter::fingerprint_goal(&tys, glist);

    let count = 1 + t.below(40, "impls") as usize;
    let mut impl_ops: Vec<Vec<Vec<Op>>> = Vec::new();
    let mut port = PortMk::default();
    let mut lists: Vec<List> = Vec::new();
    for _ in 0..count {
        let mut header: Vec<Vec<Op>> = Vec::new();
        for _ in 0..arity {
            let mut ops = Vec::new();
            term(t, Side::Impl, &mut ops);
            header.push(ops);
        }
        let mut args: Vec<Ty> = Vec::new();
        for ops in &header {
            args.push(build(&mut port, ops)?);
        }
        lists.push(port.list(&args)?);
        impl_ops.push(header);
    }
    let mut fps: Vec<Fingerprint> = Vec::new();
    for (header, &l) in impl_ops.iter().zip(lists.iter()) {
        let fp = match head {
            Head::Real => prefilter::fingerprint_impl(Portable { nodes: &port.nodes, lists: &port.lists }, l),
            Head::ParamRigid => {
                // The planted bug: the impl's parameters get the byte of a rigid parameter.
                let mut args: Vec<Ty> = Vec::new();
                {
                    let mut m = TysMk::new(&mut tys, true);
                    for ops in header {
                        args.push(build(&mut m, ops)?);
                    }
                }
                let rl = tys.mk_list(&args).map_err(solve_err)?;
                prefilter::fingerprint_goal(&tys, rl)
            }
        };
        fps.push(fp);
    }

    let mut cols: blaze_own::Vec<u8, Alloc<'static>> = blaze_own::Vec::new_in(alloc);
    let stride = prefilter::columns_of(&fps, &mut cols);
    let mut keep: blaze_own::Vec<u32, Alloc<'static>> = blaze_own::Vec::new_in(alloc);
    prefilter::filter(&gfp, Columns { bytes: &cols, stride, len: count as u32 }, &mut keep);
    let mut deferred: blaze_own::Vec<(Ty, Ty), Alloc<'static>> = blaze_own::Vec::new_in(alloc);
    for (i, header) in impl_ops.iter().enumerate() {
        if keep.iter().any(|&k| k as usize == i) {
            continue;
        }
        let snap = tys.snapshot();
        let got = unifies(&mut tys, header, &gargs, &mut deferred);
        tys.rollback_to(snap);
        if got? {
            return Err(Broke::Oracle(format!(
                "the head shape drops impl {i} of {count} ({header:?}) for the goal {goal_ops:?}, which it unifies with"
            )));
        }
    }
    Ok(())
}

/// (a) Prints `blaze-sim: <this file>:<line>: 500 cases, <n> choices`.
#[test]
fn head_shape_never_drops_an_impl_that_unifies() {
    blaze_sim::check(Config::cases(500), |t: &Tape| head_shape_case(t, Head::Real));
}

/// (a)'s control: the clean run passes, then prints `plant ParamRigid: caught at case <k> of
/// 300` (the oracle names "the head shape drops impl").
#[test]
fn control_head_shape_impl_param_not_any() {
    blaze_sim::control(Config::cases(300), Head::Real, Head::ParamRigid, "the head shape drops impl", |t, p| {
        head_shape_case(t, p)
    });
}

/// Forty impls of `Plus<R>`: thirty for `u32` (one per struct `S0`..`S29`), six for `i64`
/// (another self shape), one ANY at both positions (`impl<T> Plus<T> for T`) and three
/// blankets whose argument is another struct (`impl<T> Plus<Wk<T>> for T`).
fn forty_impls() -> String {
    let mut s = String::from("case plus_forty = trait_goals.rs assemble_impl_candidates\ntrait Plus<R>\n");
    for i in 0..30 {
        s.push_str(&format!("struct S{i}(u8)\n"));
    }
    for k in 0..3 {
        s.push_str(&format!("struct W{k}<T>(T)\n"));
    }
    for i in 0..30 {
        s.push_str(&format!("impl Plus<S{i}> for u32\n"));
    }
    for i in 0..6 {
        s.push_str(&format!("impl Plus<S{i}> for i64\n"));
    }
    s.push_str("impl<T> Plus<T> for T\n");
    for k in 0..3 {
        s.push_str(&format!("impl<T> Plus<W{k}<T>> for T\n"));
    }
    s.push_str("goal u32: Plus<S7> => yes\n");
    s.push_str("goal u32: Plus<?0> => maybe\n");
    s
}

/// The answer to `case`'s goal `goal` in `world` from a fresh solver, with the impl
/// candidates it assembled.
fn impl_candidates(world: &World, case: &CaseA, goal: usize) -> Result<(Answer, u64), String> {
    let alloc = blaze_own::testing::heap().alloc();
    let mut tys = TestTys::new();
    let mut vars: HashMap<String, Ty> = HashMap::new();
    let mut solver = Solver::new_in(alloc);
    let mut reads: blaze_own::Vec<Read, Alloc<'static>> = blaze_own::Vec::new_in(alloc);
    let none: Vec<Clause> = Vec::new();
    let env = solver.env(&mut tys, world, &none, &mut reads).map_err(|s| format!("the env stopped: {s:?}"))?;
    let g = case.goals.get(goal).ok_or_else(|| format!("no goal {goal}"))?;
    let CheckA::Goal(p) = &g.check else { return Err(format!("goal {goal} is not a goal")) };
    let sc = Scope { params: &case.params, bound: &[] };
    let mut cl: Vec<Clause> = Vec::new();
    world.names.pred(&mut Arena { tys: &mut tys, vars: &mut vars }, sc, g.line, p, &mut cl).map_err(|e| format!("{e}"))?;
    let [c]: [Clause; 1] = cl.try_into().map_err(|_| "a goal is one predicate".to_string())?;
    let a = solver
        .evaluate(&mut tys, world, TypingMode::Analysis, Goal { env, pred: c.pred, bound: c.bound }, &mut reads)
        .map_err(|s| format!("the solver stopped: {s:?}"))?;
    Ok((a, solver.stats().impl_candidates))
}

/// (b) Prints `head shape: u32: Plus<S7> assembles 2 of 40 impls (34 by self shape); u32:
/// Plus<?0> assembles 34`. Without the columns (every impl of the self shape's bucket and the
/// blanket bucket a candidate) the first count is 34.
#[test]
fn head_shape_imports_only_matching_and_any() {
    let text = forty_impls();
    let prelude = parse_file(PRELUDE).unwrap_or_else(|e| panic!("the prelude does not parse: {e}"));
    let file = parse_file(&text).unwrap_or_else(|e| panic!("the forty impls do not parse: {e}"));
    let case = file.cases.first().unwrap_or_else(|| panic!("the text has no case"));
    let mut decls: Vec<(u32, &DeclA)> = Vec::new();
    decls.extend(prelude.decls.iter().map(|(l, d)| (*l, d)));
    decls.extend(file.decls.iter().map(|(l, d)| (*l, d)));
    decls.extend(case.decls.iter().map(|(l, d)| (*l, d)));
    let world = World::build(&decls).unwrap_or_else(|e| panic!("the forty impls do not build: {e}"));
    let (a, known) = impl_candidates(&world, case, 0).unwrap_or_else(|e| panic!("u32: Plus<S7>: {e}"));
    assert!(a == Answer::Yes, "u32: Plus<S7> must hold by `impl Plus<S7> for u32`");
    // `impl Plus<S7> for u32` matches and `impl<T> Plus<T> for T` is ANY at both positions;
    // the other 29 u32 impls name another struct, the three `Wk<T>` blankets another head, and
    // the six i64 impls are in another self shape's bucket.
    assert_eq!(known, 2, "u32: Plus<S7> assembled {known} impl candidates, want the matching one and the ANY one");
    let (_, unknown) = impl_candidates(&world, case, 1).unwrap_or_else(|e| panic!("u32: Plus<?0>: {e}"));
    // An unknown argument filters nothing past the self shape: 30 u32 impls and 4 blankets.
    assert_eq!(unknown, 34, "u32: Plus<?0> assembled {unknown} impl candidates, want every one of the u32 and blanket buckets");
    std::println!("head shape: u32: Plus<S7> assembles {known} of 40 impls (34 by self shape); u32: Plus<?0> assembles {unknown}");
}
