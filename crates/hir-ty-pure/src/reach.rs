//! Which sites of a body are proved never to be reached, or never to fail
//! (docs/v2/plan/design/proved-unreachable.md): what `blaze::unreachable!()` is held to, and
//! what takes a dead panic site off `#[blaze::nopanic]`'s walk and the panic lints' count.
//!
//! One forward abstract interpretation of a body's runtime MIR ([`prove`]). The state at a
//! point is a symbolic value per local (a hash-consed [`Term`]) and a set of [`Fact`]s over
//! terms. Facts come from the edges taken (`SwitchInt`, an `Assert`'s success), from types
//! (an unsigned integer is not negative, `Option`'s discriminant is 0 or 1) and from the
//! models of a few std functions ([`models`]). A site is discharged when no feasible edge
//! reaches its block ([`Reach::dead`]) or the state before it entails its condition
//! ([`Reach::safe`]): an `Assert`'s, or a modeled callee's condition for not panicking.
//!
//! It reads the one body and the identity of what it calls, never a callee's body and never
//! optimized MIR, so the answer does not depend on the profile.
//!
//! The memory model, and why it is sound for safe code: a term is a value, not a place. A
//! shared borrow is a reference to the value its place has then ([`Term::RefTo`]): nothing
//! changes the place while the reference lives, and what is tracked (lengths, discriminants,
//! integers) is not behind an `UnsafeCell` a place expression reaches. What a `&mut`, a `Box`
//! or a raw pointer leads to is read at the state's epoch ([`Term::DerefAt`]); a write through
//! a pointer, a call no model names and a `Drop` start a new epoch and forget every local
//! whose address a `&mut` or a raw pointer took ([`Solver::bump`]). An unknown made at a
//! program point is forgotten, with what mentions it, when the point runs again
//! ([`Solver::kill_point`]); where paths meet, a local with two values becomes one unknown per
//! block and local and only facts every path holds survive ([`Solver::join`]).
//!
//! The invariant: a site [`Reach::discharged`] says is discharged is not reached, or its
//! condition holds, in every run of the body's MIR.

pub mod models;

use std::alloc::Global;

use base_db::Db;
use blaze_hash::HashMap;
use hir_def::{DefWithBodyId, EnumVariantId, VariantId, hir::ExprId};
use hir_ty::{
    InferBodyId,
    db::HirDatabase as _,
    next_solver::{
        DbInterner, ParamEnv, Ty, TyKind, TypingMode,
        infer::{DbInternerInferExt as _, InferCtxt},
    },
};
use hir_ty_mir::mir::{
    AggregateKind, BasicBlock, BasicBlockId, BinOp, BorrowKind, CastKind, LocalId, MirBody,
    MirSpan, Operand, OperandKind, PlaceTy, ProjectionElem, Rvalue, StatementKind, StoredPlace,
    TerminatorKind, UnOp,
};
use rustc_type_ir::{IntTy, UintTy, inherent::IntoKind as _};

use crate::effect_attrs::constant_bits;
use models::Model;

/// A term's number in its body's table.
pub(crate) type TermId = u32;

/// How two terms compare, as integers. `>` and `>=` are written with the sides swapped.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub(crate) enum Rel {
    Lt,
    Le,
    Eq,
    Ne,
}

/// Where an unknown value was made: the same key is the same unknown while its point does not
/// run again.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Key {
    /// What a local holds when the body starts (a parameter); the last slot is the epoch.
    Entry(u32),
    /// Made by statement `stmt` of `block` (the terminator is the number of statements).
    At(u32, u32, u32),
    /// What `slot` holds on entry to `block`, where paths that disagree meet.
    Join(u32, u32),
}

/// The slots of [`Key::At`] that are not a statement's own counter.
const EPOCH: u32 = u32::MAX;
const HAVOC: u32 = 0x8000_0000;
const STALE: u32 = 0x4000_0000;
/// A [`Key::Join`] slot of a field of a local's aggregate: `FIELD + local * 64 + field`.
const FIELD: u32 = 0x8000_0000;
/// An aggregate that is not an enum variant's.
const NO_VARIANT: u32 = u32::MAX;

/// A symbolic value.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
enum Term {
    Const(i128),
    Unknown(Key),
    /// The length of the slice, array, `str`, `Vec` or `String` value.
    Len(TermId),
    /// The discriminant of the enum value.
    Discr(TermId),
    Field(TermId, u32),
    /// Field `.2` of the variant `.1` ([`Solver::variants`]) of the enum value.
    Payload(TermId, u32, u32),
    /// 1 when the comparison holds, else 0.
    Cmp(Rel, TermId, TermId),
    /// `!` of a `bool`.
    Not(TermId),
    /// A shared reference to a place that held this value when it was borrowed.
    RefTo(TermId),
    /// A `&mut` to the whole local `.0`, which held `.1` at epoch `.2`.
    MutRef(u32, TermId, TermId),
    /// What the shared reference (or the slice pointer) leads to.
    Deref(TermId),
    /// What the `&mut` or `Box` leads to at the epoch.
    DerefAt(TermId, TermId),
    /// A struct, tuple or enum variant ([`NO_VARIANT`] when not a variant) of these fields.
    Agg(u32, Box<[TermId]>),
    /// A linear form, the exact integer `sum of coefficient * term, plus constant`: the terms
    /// in order, none a constant or a form itself, no coefficient 0, and not a lone term with
    /// coefficient 1 and constant 0 ([`Solver::lin`]). It is a number, not a machine integer:
    /// a local holds one only where the sum is shown not to leave its type.
    Lin(Box<[(TermId, i128)]>, i128),
    /// The exact value `.0` wrapped into an integer type (`.1`: its bits, 128 more when
    /// signed): what `a + b` is where nothing shows it fits. The same operands give the same
    /// term, so a guard and a use that compute one expression agree.
    Wrap(TermId, u32),
}

/// A linear form as it is worked with: `(term, coefficient)` in term order, and the constant.
type Form = (Vec<(TermId, i128)>, i128);

/// `a + scale * b`; none when a coefficient or the constant leaves `i128`.
fn form_add(a: &Form, b: &Form, scale: i128) -> Option<Form> {
    let mut parts = Vec::with_capacity(a.0.len() + b.0.len()); // alloc: a form's terms, a few
    let (mut i, mut j) = (0, 0);
    while i < a.0.len() || j < b.0.len() {
        let left = a.0.get(i).copied();
        let right = b.0.get(j).copied();
        let (term, coefficient) = match (left, right) {
            (Some((t, c)), Some((u, d))) if t == u => {
                (i, j) = (i + 1, j + 1);
                (t, c.checked_add(d.checked_mul(scale)?)?)
            }
            (Some((t, c)), Some((u, _))) if t < u => {
                i += 1;
                (t, c)
            }
            (Some((t, c)), None) => {
                i += 1;
                (t, c)
            }
            (_, Some((u, d))) => {
                j += 1;
                (u, d.checked_mul(scale)?)
            }
            (None, None) => break,
        };
        if coefficient != 0 {
            parts.push((term, coefficient));
        }
    }
    Some((parts, a.1.checked_add(b.1.checked_mul(scale)?)?))
}

/// How many facts one linear proof may add up, and how many it may try: the budget of
/// [`Solver::lin_le`]. A proof that needs more is not found, and the site stays reported.
const LINEAR_DEPTH: u8 = 4;
const LINEAR_FUEL: u32 = 256;

/// `a rel b`, or for [`Fact::If`] an implication.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub(crate) struct Atom {
    rel: Rel,
    a: TermId,
    b: TermId,
}

impl Atom {
    /// `a rel b` with `==` and `!=` in one order.
    fn new(rel: Rel, a: TermId, b: TermId) -> Atom {
        match rel {
            Rel::Eq | Rel::Ne if a > b => Atom { rel, a: b, b: a },
            _ => Atom { rel, a, b },
        }
    }

    /// The atom that holds exactly when this one does not.
    fn not(self) -> Atom {
        match self.rel {
            Rel::Lt => Atom::new(Rel::Le, self.b, self.a),
            Rel::Le => Atom::new(Rel::Lt, self.b, self.a),
            Rel::Eq => Atom::new(Rel::Ne, self.a, self.b),
            Rel::Ne => Atom::new(Rel::Eq, self.a, self.b),
        }
    }
}

/// What is known at a point: each slot's value (the locals, then the epoch), the atoms that
/// hold and the implications between atoms. Sorted, so equal states are equal values.
#[derive(Clone, PartialEq, Eq, Debug)]
struct State {
    vals: Vec<TermId>,
    facts: Vec<Atom>,
    imps: Vec<(Atom, Atom)>,
}

impl State {
    fn has(&self, atom: Atom) -> bool {
        self.facts.binary_search(&atom).is_ok()
    }

    fn put(&mut self, atom: Atom) {
        if let Err(at) = self.facts.binary_search(&atom) {
            self.facts.insert(at, atom);
        }
    }

    fn put_imp(&mut self, imp: (Atom, Atom)) {
        if let Err(at) = self.imps.binary_search(&imp) {
            self.imps.insert(at, imp);
        }
    }
}

/// What [`prove`] found of one body, by block.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Reach {
    /// No feasible path reaches the block. Empty when the body was not analysed.
    dead: Box<[bool]>,
    /// The block's `Assert` cannot fail, or its call of a modeled function cannot panic.
    safe: Box<[bool]>,
    /// Each block that ends in an `Assert` or a call, with the expression it is written at.
    sites: Box<[(ExprId, u32)]>,
    /// Asked for with `explain`: why the block is reached, or why its site is not safe.
    pub explained: Box<[(u32, String)]>,
    /// Why nothing is proved, when the body was not analysed.
    pub refused: Option<&'static str>,
}

impl Reach {
    fn refused(why: &'static str) -> Reach {
        Reach { refused: Some(why), ..Reach::default() }
    }

    /// Whether no feasible path reaches `block`.
    pub fn dead(&self, block: BasicBlockId) -> bool {
        self.dead_at(index(block))
    }

    /// Whether the site that ends `block` cannot fail here: it is never reached, its `Assert`
    /// holds, or the modeled function it calls cannot panic with these arguments.
    pub fn discharged_block(&self, block: BasicBlockId) -> bool {
        self.discharged_at(index(block))
    }

    fn dead_at(&self, block: usize) -> bool {
        self.dead.get(block).copied().unwrap_or(false)
    }

    fn discharged_at(&self, block: usize) -> bool {
        self.dead_at(block) || self.safe.get(block).copied().unwrap_or(false)
    }

    /// Whether every `Assert` and call the MIR has at the expression `expr` is discharged, and
    /// there is one. An expression with no site (not lowered, or lowered to no check) is not
    /// discharged by this: [`Reach::unreached`] answers for a site that must not be reached.
    pub fn discharged(&self, expr: ExprId) -> bool {
        let mut any = false;
        for &(at, block) in self.sites.iter() {
            if at == expr {
                any = true;
                if !self.discharged_at(block as usize) {
                    return false;
                }
            }
        }
        any
    }

    /// Whether no feasible path reaches a site written at `expr`: every block that ends at it
    /// is dead, or lowering made none (the code after a diverging expression is not lowered).
    /// False when the body was not analysed.
    pub fn unreached(&self, expr: ExprId) -> bool {
        self.refused.is_none()
            && self.sites.iter().all(|&(at, block)| at != expr || self.dead_at(block as usize))
    }

    /// The blocks whose site is written at `expr`.
    pub fn blocks_at(&self, expr: ExprId) -> impl Iterator<Item = u32> + '_ {
        self.sites.iter().filter(move |&&(at, _)| at == expr).map(|&(_, block)| block)
    }
}

/// What kind of operation an [`Overflow`] site is: the lint asks of an expression the kind its
/// syntax is, so a check lowering adds at the same expression (the cast of a signed shift
/// amount) is not taken for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum OpKind {
    /// `+`, `-` or `*` of integers, compound or not.
    Arith,
    /// `<<` or `>>`: whether the amount is below the bits.
    Shift,
    /// `-` of a signed integer.
    Neg,
    /// An `as` between integers: whether the value survives, `usize` and `isize` taken as
    /// narrow as 32 bits on the target side.
    Cast,
}

/// What the prover could not show of one operation: the fact that is missing, for the
/// message (docs/v2/plan/design/explicit-overflow.md).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Missing {
    /// Nothing bounds the left operand, the right one, or both, on the side the result needs.
    Unbounded { left: bool, right: bool },
    /// `a - b` of an unsigned type: nothing shows `b <= a`.
    Order,
    /// The operands are bounded, but what bounds them lets the result reach this value,
    /// outside its type.
    Reaches(i128),
    /// A shift: nothing shows the amount is below this many bits (or not negative).
    Amount(u32),
    /// `-a`: nothing shows `a` is not the type's least value.
    NotMin,
    /// A cast: nothing shows the value is at least this (`0 <= x` for a signed to unsigned).
    Below(i128),
    /// A cast: nothing shows the value is at most this.
    Above(i128),
    /// A `u128`: its values past `i128::MAX` are not held, so nothing of it is shown.
    Wide,
}

/// What [`overflow_of_body`] found of a body and its closures: each integer operation the
/// MIR has (`+`, `-`, `*`, a shift, a negation, an `as` between integers), at the expression
/// it is written at, with what was not shown (none where it fits). An expression lowered
/// more than once has one entry per lowering.
///
/// The semantics are the profile's with overflow checks, whichever the crate has: an
/// operation not shown to fit is reported and taken to fit after it (as a run that got past
/// it did), so a body with no site reported overflows in no profile, and the answer is the
/// same in both.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Overflow {
    pub sites: Box<[(ExprId, OpKind, Option<Missing>)]>,
    /// Why a body was not analysed: none for the function's own, else the closure's
    /// expression it is of; with the reason.
    pub refused: Box<[(Option<ExprId>, &'static str)]>,
}

impl Overflow {
    /// What was not shown of the operation of kind `kind` written at `expr`: `None` when every
    /// lowering of it fits, or none was made (code no path reaches is not lowered).
    pub fn missing(&self, expr: ExprId, kind: OpKind) -> Option<Missing> {
        self.sites.iter().find_map(|&(at, of, missing)| (at == expr && of == kind).then_some(missing)?)
    }

    /// Whether an operation of kind `kind` at `expr` was lowered and analysed.
    pub fn analysed(&self, expr: ExprId, kind: OpKind) -> bool {
        self.sites.iter().any(|&(at, of, _)| at == expr && of == kind)
    }
}

#[cfg(test)]
thread_local! {
    /// A bug planted for the control of the overflow oracle (`tests/reach_oracle.rs`): every
    /// `+` is taken to fit.
    pub(crate) static PLANT_ADD_ALWAYS_FITS: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

/// The integer operations of the body of `def` and of its closures that the prover can't
/// show keep their value ([`Overflow`]), once per definition: what `blaze::implicit_overflow`
/// reads.
#[blaze_query::tracked(returns(ref))]
pub fn overflow_of_body(db: &Db, def: DefWithBodyId) -> Overflow {
    let Ok(root) = db.runtime_mir_body(InferBodyId::DefWithBodyId(def)) else {
        return Overflow {
            sites: Box::default(),
            refused: Box::new([(None, "its body does not lower to MIR")]),
        };
    };
    let krate = hir_def::HasModule::krate(&def, db);
    let infcx = DbInterner::new_with(db, krate).infer_ctxt().build(TypingMode::PostAnalysis);
    let env = db.trait_environment(def.generic_def(db));
    // alloc: the body's MIR and its closures', a few
    let mut bodies: Vec<(Option<ExprId>, &MirBody<'_>)> = vec![(None, root)];
    let mut refused = Vec::new(); // alloc: the bodies not analysed, few
    let mut at = 0;
    while at < bodies.len() {
        for &closure in &bodies[at].1.closures {
            let expr = closure.loc(db).expr;
            match db.runtime_mir_body_for_closure(closure) {
                Ok(body) => bodies.push((Some(expr), body)),
                Err(_) => refused.push((Some(expr), "its body does not lower to MIR")),
            }
        }
        at += 1;
    }
    let mut sites = Vec::new(); // alloc: the sites of the bodies, the result
    for (of, body) in bodies {
        match prove_overflow(db, &infcx, env, body) {
            Ok(found) => sites.extend(found),
            Err(why) => refused.push((of, why)),
        }
    }
    sites.sort();
    Overflow { sites: sites.into_boxed_slice(), refused: refused.into_boxed_slice() }
}

/// The integer operations of `body` with what was not shown of each, or why it was not
/// analysed.
fn prove_overflow<'db>(
    db: &'db Db,
    infcx: &InferCtxt<'db>,
    env: ParamEnv<'db>,
    body: &MirBody<'db>,
) -> Result<Vec<(ExprId, OpKind, Option<Missing>)>, &'static str> {
    if body.coroutine.is_some() {
        return Err("it is a coroutine");
    }
    // alloc: the body's blocks by index, once per proof
    let blocks: Vec<&BasicBlock> = body.basic_blocks.iter().map(|(_, block)| block).collect();
    analysable(&blocks)?;
    let Ok(layout) = db.target_data_layout(hir_def::HasModule::krate(&body.owner, db)) else {
        return Err("its target's layout is not known");
    };
    let word = layout.pointer_size().bits() as u32;
    let mut solver = Solver::new(db, infcx, env, body, &blocks, word);
    solver.mode = Mode::Overflow;
    let Some(entries) = solver.solve() else {
        return Err("its proof did not settle within the budget");
    };
    // The verdicts, from the settled entry states: each block walked once more, recording.
    solver.recording = true;
    for (at, entry) in entries.iter().enumerate() {
        let Some(entry) = entry else { continue };
        let mut state = entry.clone(); // clone: the block's entry state, walked to its end
        let _ = solver.transfer(at as u32, &mut state);
    }
    Ok(std::mem::take(&mut solver.verdicts))
}

/// Refuses a body with inline assembly, a `yield` or a `for const` loop.
fn analysable(blocks: &[&BasicBlock]) -> Result<(), &'static str> {
    for block in blocks {
        let asm =
            block.statements.iter().any(|it| matches!(it.kind, StatementKind::InlineAsm(_)));
        let odd = matches!(
            block.terminator.as_ref().map(|it| &it.kind),
            Some(
                TerminatorKind::Yield { .. }
                    | TerminatorKind::ComptimeFor { .. }
                    | TerminatorKind::ComptimeNext { .. }
                    | TerminatorKind::CoroutineDrop
            )
        );
        if asm || odd {
            return Err("it has inline assembly, a `yield` or a `for const` loop");
        }
    }
    Ok(())
}

/// What a proof is for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    /// Which panic sites are dead or cannot fail: the MIR's own semantics, its profile's.
    Panics,
    /// Which integer operations keep their value ([`overflow_of_body`]): an operation not
    /// shown to fit is taken to fit after it, in either profile, and an overflow check's
    /// `Assert` adds nothing, so both profiles give one answer.
    Overflow,
}

fn index(block: BasicBlockId) -> usize {
    block.into_raw().into_u32() as usize
}

fn local_index(local: LocalId) -> usize {
    local.into_raw().into_u32() as usize
}

/// Whether a proof of `body` could discharge anything: it has a branch or a check. A body
/// with neither costs this one scan.
pub fn worth(body: &MirBody<'_>) -> bool {
    body.basic_blocks.iter().any(|(_, block)| {
        matches!(
            block.terminator.as_ref().map(|it| &it.kind),
            Some(TerminatorKind::SwitchInt { .. } | TerminatorKind::Assert { .. })
        )
    })
}

/// The proof of the body of `def` as it is written (its generic runtime MIR), once per
/// definition. A body that does not lower is refused.
#[blaze_query::tracked(returns(ref))]
pub fn of_body(db: &Db, def: DefWithBodyId) -> Reach {
    let Ok(body) = db.runtime_mir_body(InferBodyId::DefWithBodyId(def)) else {
        return Reach::refused("its body does not lower to MIR");
    };
    let krate = hir_def::HasModule::krate(&def, db);
    let infcx = DbInterner::new_with(db, krate).infer_ctxt().build(TypingMode::PostAnalysis);
    let env = db.trait_environment(def.generic_def(db));
    prove(db, &infcx, env, body, &[])
}

/// [`of_body`] with why each block of `explain` is reached. Not kept: asked only to say why a
/// site is refused.
pub fn explain(db: &Db, def: DefWithBodyId, explain: &[u32]) -> Reach {
    let Ok(body) = db.runtime_mir_body(InferBodyId::DefWithBodyId(def)) else {
        return Reach::refused("its body does not lower to MIR");
    };
    let krate = hir_def::HasModule::krate(&def, db);
    let infcx = DbInterner::new_with(db, krate).infer_ctxt().build(TypingMode::PostAnalysis);
    let env = db.trait_environment(def.generic_def(db));
    prove(db, &infcx, env, body, explain)
}

/// What is proved of `body`'s sites. `explain` names the blocks to say why they are reached.
pub fn prove<'db>(
    db: &'db Db,
    infcx: &InferCtxt<'db>,
    env: ParamEnv<'db>,
    body: &MirBody<'db>,
    explain: &[u32],
) -> Reach {
    if body.coroutine.is_some() {
        return Reach::refused("it is a coroutine");
    }
    let count = body.basic_blocks.len();
    // alloc: the body's blocks by index, once per proof
    let blocks: Vec<&BasicBlock> = body.basic_blocks.iter().map(|(_, block)| block).collect();
    if let Err(why) = analysable(&blocks) {
        return Reach::refused(why);
    }
    // A `usize`'s range is the target's: a wider guess would prove an overflow check that can
    // fail, a narrower one a bound that does not hold.
    let Ok(layout) = db.target_data_layout(hir_def::HasModule::krate(&body.owner, db)) else {
        return Reach::refused("its target's layout is not known");
    };
    let word = layout.pointer_size().bits() as u32;
    let mut solver = Solver::new(db, infcx, env, body, &blocks, word);
    let Some(entries) = solver.solve() else {
        return Reach::refused("its proof did not settle within the budget");
    };
    // The verdicts, from the settled entry states.
    let mut dead = vec![false; count]; // alloc: one flag per block, the proof's result
    let mut safe = vec![false; count]; // alloc: as above
    let mut sites = Vec::new(); // alloc: the body's sites, the proof's result
    let mut explained = Vec::new(); // alloc: only what `explain` asked
    for (at, block) in blocks.iter().enumerate() {
        let Some(terminator) = &block.terminator else { continue };
        let site = matches!(
            terminator.kind,
            TerminatorKind::Assert { .. } | TerminatorKind::Call { .. }
        );
        if site && let MirSpan::ExprId(expr) = terminator.span {
            sites.push((expr, at as u32));
        }
        let Some(entry) = &entries[at] else {
            dead[at] = true;
            continue;
        };
        if !site {
            continue;
        }
        let mut state = entry.clone(); // clone: the block's entry state, walked to its end
        let out = solver.transfer(at as u32, &mut state);
        safe[at] = out.safe;
        if explain.contains(&(at as u32)) && !out.safe {
            explained.push((at as u32, solver.why_reached(at as u32, &entries)));
        }
    }
    sites.sort();
    Reach {
        dead: dead.into_boxed_slice(),
        safe: safe.into_boxed_slice(),
        sites: sites.into_boxed_slice(),
        explained: explained.into_boxed_slice(),
        refused: None,
    }
}

/// What walking a block to its end gives: the states along its feasible edges, and whether
/// its site is safe.
struct Out {
    edges: Vec<(u32, State)>,
    safe: bool,
    /// What the block's site needs and the state did not entail.
    goal: Option<Atom>,
}

struct Solver<'a, 'db> {
    db: &'db Db,
    infcx: &'a InferCtxt<'db>,
    env: ParamEnv<'db>,
    body: &'a MirBody<'db>,
    blocks: &'a [&'a BasicBlock],
    terms: Vec<Term>,
    ids: HashMap<Term, TermId, Global>,
    /// What each term's type allows, by its number.
    ranges: Vec<(i128, i128)>,
    /// The first named local each term was put in, for messages.
    names: HashMap<TermId, u32, Global>,
    /// The locals a binding of the source names.
    named: Vec<bool>,
    /// The enum each discriminant term is of, for messages.
    enums: HashMap<TermId, hir_def::EnumId, Global>,
    variants: Vec<EnumVariantId>,
    models: HashMap<hir_def::FunctionId, Option<Model>, Global>,
    /// The locals whose address a `&mut` or a raw pointer took.
    exposed: Vec<bool>,
    /// The blocks on a cycle of the graph: only there can a point run again.
    cyclic: Vec<bool>,
    predecessors: Vec<Vec<u32>>,
    /// Per block, the slots that are a join's unknown there: only grows.
    joined: Vec<Vec<u32>>,
    /// How many bits a `usize` has on the target.
    word: u32,
    /// The statement being walked, and how many unknowns it has made.
    at: (u32, u32),
    made: u32,
    /// What the proof is for.
    mode: Mode,
    /// Where the statement being walked is written.
    span: MirSpan,
    /// Whether [`Self::note`] keeps what it is told: on for the last walk of a settled
    /// [`Mode::Overflow`] proof.
    recording: bool,
    /// What [`Self::note`] kept.
    verdicts: Vec<(ExprId, OpKind, Option<Missing>)>,
}

/// The widest range a term's value is kept in.
const ANY: (i128, i128) = (i128::MIN, i128::MAX);

impl<'a, 'db> Solver<'a, 'db> {
    fn new(
        db: &'db Db,
        infcx: &'a InferCtxt<'db>,
        env: ParamEnv<'db>,
        body: &'a MirBody<'db>,
        blocks: &'a [&'a BasicBlock],
        word: u32,
    ) -> Solver<'a, 'db> {
        let count = blocks.len();
        let mut graph = crate::loops::Graph::new(count);
        let mut predecessors = vec![Vec::new(); count]; // alloc: one list per block, once per proof
        for (from, block) in blocks.iter().enumerate() {
            let Some(terminator) = &block.terminator else { continue };
            for target in terminator.kind.all_successors() {
                let to = index(target);
                graph.edge(from as u32, to as u32);
                if !predecessors[to].contains(&(from as u32)) {
                    predecessors[to].push(from as u32);
                }
            }
        }
        let all = vec![true; count]; // alloc: once per proof
        let mut cyclic = vec![false; count]; // alloc: once per proof
        for component in graph.components(&all) {
            for block in component {
                cyclic[block as usize] = true;
            }
        }
        let mut solver = Solver {
            db,
            infcx,
            env,
            body,
            blocks,
            terms: Vec::new(), // alloc: the body's terms, once per proof
            ids: HashMap::new_in(Global),
            ranges: Vec::new(), // alloc: one range per term
            names: HashMap::new_in(Global),
            named: Vec::new(),
            enums: HashMap::new_in(Global),
            variants: Vec::new(), // alloc: the variants the body names, few
            models: HashMap::new_in(Global),
            exposed: Vec::new(),
            cyclic,
            predecessors,
            joined: vec![Vec::new(); count], // alloc: one list per block, once per proof
            word,
            at: (0, 0),
            made: 0,
            mode: Mode::Panics,
            span: MirSpan::Unknown,
            recording: false,
            verdicts: Vec::new(), // alloc: only an overflow proof's, its result
        };
        solver.exposed = solver.exposed_locals();
        let mut named = vec![false; body.locals.len()]; // alloc: one flag per local, once per proof
        for (_, local) in body.binding_locals.iter() {
            named[local_index(*local)] = true;
        }
        solver.named = named;
        solver
    }

    // Terms.

    fn term(&mut self, term: Term, range: (i128, i128)) -> TermId {
        if let Some(&id) = self.ids.get(&term) {
            // The same term asked for with another range keeps what both allow at least: the
            // wider of the two, which is true whichever asked.
            let had = self.ranges[id as usize];
            self.ranges[id as usize] = (had.0.min(range.0), had.1.max(range.1));
            return id;
        }
        let id = self.terms.len() as TermId;
        self.terms.push(term.clone()); // clone: the table's copy and the map's key
        self.ranges.push(range);
        self.ids.insert(term, id);
        id
    }

    fn constant(&mut self, value: i128) -> TermId {
        self.term(Term::Const(value), (value, value))
    }

    fn unknown(&mut self, key: Key, ty: Option<Ty<'db>>) -> TermId {
        let range = ty.and_then(|ty| int_range(ty, self.word)).unwrap_or(ANY);
        self.term(Term::Unknown(key), range)
    }

    /// An unknown of the statement being walked.
    fn fresh(&mut self, ty: Option<Ty<'db>>) -> TermId {
        let slot = self.made;
        self.made += 1;
        self.unknown(Key::At(self.at.0, self.at.1, slot), ty)
    }

    fn const_of(&self, term: TermId) -> Option<i128> {
        match self.terms[term as usize] {
            Term::Const(value) => Some(value),
            _ => None,
        }
    }

    fn len(&mut self, of: TermId) -> TermId {
        let most = (1i128 << self.word.min(64)) - 1;
        self.term(Term::Len(of), (0, most))
    }

    /// The discriminant of the enum value `of`, a value of `ty` when it is known.
    fn discr(&mut self, of: TermId, ty: Option<Ty<'db>>) -> TermId {
        if let Term::Agg(variant, _) = self.terms[of as usize]
            && variant != NO_VARIANT
            && let Ok(value) = self.db.const_eval_discriminant(self.variants[variant as usize])
        {
            return self.constant(value);
        }
        let range = ty.and_then(|ty| models::discriminants(self.db, ty)).unwrap_or(ANY);
        let term = self.term(Term::Discr(of), range);
        if let Some(ty) = ty
            && let TyKind::Adt(def, _) = ty.kind()
            && let hir_def::AdtId::EnumId(it) = def.def_id()
        {
            self.enums.insert(term, it);
        }
        term
    }

    fn field(&mut self, of: TermId, field: u32) -> TermId {
        if let Term::Agg(_, fields) = &self.terms[of as usize]
            && let Some(&it) = fields.get(field as usize)
        {
            return it;
        }
        self.term(Term::Field(of, field), ANY)
    }

    fn payload(&mut self, of: TermId, variant: u32, field: u32) -> TermId {
        if let Term::Agg(has, fields) = &self.terms[of as usize]
            && *has == variant
            && let Some(&it) = fields.get(field as usize)
        {
            return it;
        }
        self.term(Term::Payload(of, variant, field), ANY)
    }

    fn variant(&mut self, variant: EnumVariantId) -> u32 {
        match self.variants.iter().position(|&it| it == variant) {
            Some(at) => at as u32,
            None => {
                self.variants.push(variant);
                self.variants.len() as u32 - 1
            }
        }
    }

    /// `a rel b` as a value: a constant when both are, else the comparison.
    fn cmp(&mut self, rel: Rel, a: TermId, b: TermId) -> TermId {
        if let (Some(x), Some(y)) = (self.const_of(a), self.const_of(b)) {
            let holds = match rel {
                Rel::Lt => x < y,
                Rel::Le => x <= y,
                Rel::Eq => x == y,
                Rel::Ne => x != y,
            };
            return self.constant(i128::from(holds));
        }
        // Two forms that differ by a constant compare as that constant does with 0
        // (`len - 1 < len`).
        if let Some((parts, by)) = form_add(&self.form(a), &self.form(b), -1)
            && parts.is_empty()
        {
            let holds = match rel {
                Rel::Lt => by < 0,
                Rel::Le => by <= 0,
                Rel::Eq => by == 0,
                Rel::Ne => by != 0,
            };
            return self.constant(i128::from(holds));
        }
        let atom = Atom::new(rel, a, b);
        self.term(Term::Cmp(atom.rel, atom.a, atom.b), (0, 1))
    }

    /// `term` as a linear form: a constant, a form, or the term itself once.
    fn form(&self, term: TermId) -> Form {
        match &self.terms[term as usize] {
            Term::Const(value) => (Vec::new(), *value),
            Term::Lin(parts, constant) => (parts.to_vec(), *constant), // alloc: a form's terms, a few
            _ => (vec![(term, 1)], 0), // alloc: one term
        }
    }

    /// The term of a linear form: a constant, a lone term, or the form.
    fn lin(&mut self, form: Form) -> TermId {
        match (&form.0[..], form.1) {
            ([], constant) => self.constant(constant),
            ([(term, 1)], 0) => *term,
            _ => self.term(Term::Lin(form.0.into_boxed_slice(), form.1), ANY),
        }
    }

    /// The greatest value `form` can have by the bounds of each of its terms alone; none
    /// when a term has no bound on that side.
    fn form_high(&self, state: &State, form: &Form) -> Option<i128> {
        let mut high = form.1;
        for &(term, coefficient) in &form.0 {
            let (least, most) = self.bounds(state, term);
            let end = if coefficient > 0 { most } else { least };
            if end == i128::MAX || end == i128::MIN {
                return None;
            }
            high = high.checked_add(coefficient.checked_mul(end)?)?;
        }
        Some(high)
    }

    /// What the state's facts say as linear bounds: each `a < b`, `a <= b` and `a == b` as
    /// `form <= bound`, the form with no constant.
    fn known(&self, state: &State) -> Vec<(Vec<(TermId, i128)>, i128)> {
        let mut known = Vec::with_capacity(state.facts.len()); // alloc: one bound per fact, per proof
        let mut add = |left: &Form, right: &Form, slack: i128| {
            if let Some((parts, constant)) = form_add(left, right, -1)
                && !parts.is_empty()
                && let Some(bound) = slack.checked_sub(constant)
            {
                known.push((parts, bound));
            }
        };
        for fact in &state.facts {
            let (a, b) = (self.form(fact.a), self.form(fact.b));
            match fact.rel {
                Rel::Lt => add(&a, &b, -1),
                Rel::Le => add(&a, &b, 0),
                Rel::Eq => {
                    add(&a, &b, 0);
                    add(&b, &a, 0);
                }
                Rel::Ne => {}
            }
        }
        known
    }

    /// Whether the state entails `form <= limit`, by adding up facts: the bounds of the
    /// form's terms give it, or some fact `f <= b` that cancels a term of the form leaves
    /// `form - f <= limit - b` to show the same way. Shortest paths over difference
    /// constraints (`x - y <= c`) are the case of two-term facts; this adds any linear
    /// facts, at most `depth` of them, trying at most `fuel`.
    fn lin_le(
        &self,
        state: &State,
        form: &Form,
        limit: i128,
        known: &[(Vec<(TermId, i128)>, i128)],
        depth: u8,
        fuel: &mut u32,
    ) -> bool {
        let Some(limit) = limit.checked_sub(form.1) else { return false };
        if form.0.is_empty() {
            return 0 <= limit;
        }
        let bare = (form.0.clone(), 0); // clone: the form without its constant
        if self.form_high(state, &bare).is_some_and(|high| high <= limit) {
            return true;
        }
        if depth == 0 {
            return false;
        }
        for (parts, bound) in known {
            let cancels = parts.iter().any(|&(term, coefficient)| {
                bare.0.iter().any(|&(of, has)| of == term && (has > 0) == (coefficient > 0))
            });
            if !cancels {
                continue;
            }
            if *fuel == 0 {
                return false;
            }
            *fuel -= 1;
            let fact = (parts.clone(), 0); // clone: subtracted from the form
            let (Some(rest), Some(left)) = (form_add(&bare, &fact, -1), limit.checked_sub(*bound))
            else {
                continue;
            };
            if self.lin_le(state, &rest, left, known, depth - 1, fuel) {
                return true;
            }
        }
        false
    }

    /// Whether the state entails `atom` by linear reasoning ([`Self::lin_le`]).
    fn linear(&self, state: &State, atom: Atom) -> bool {
        let (a, b) = (self.form(atom.a), self.form(atom.b));
        let known = self.known(state);
        let mut fuel = LINEAR_FUEL;
        let mut le = |left: &Form, right: &Form, limit: i128| {
            form_add(left, right, -1).is_some_and(|form| {
                self.lin_le(state, &form, limit, &known, LINEAR_DEPTH, &mut fuel)
            })
        };
        match atom.rel {
            Rel::Lt => le(&a, &b, -1),
            Rel::Le => le(&a, &b, 0),
            Rel::Eq => le(&a, &b, 0) && le(&b, &a, 0),
            Rel::Ne => le(&a, &b, -1) || le(&b, &a, -1),
        }
    }

    /// Whether the state shows the exact value `form` is within `least..=most`.
    fn form_within(&self, state: &State, form: &Form, least: i128, most: i128) -> bool {
        let known = self.known(state);
        let mut fuel = LINEAR_FUEL;
        let zero: Form = (Vec::new(), 0);
        let Some(negated) = form_add(&zero, form, -1) else { return false };
        let Some(floor) = least.checked_neg() else { return false };
        self.lin_le(state, form, most, &known, LINEAR_DEPTH, &mut fuel)
            && self.lin_le(state, &negated, floor, &known, LINEAR_DEPTH, &mut fuel)
    }

    /// Whether every product of the bounds of `a` and `b` is inside `range`.
    fn product_within(&self, state: &State, a: TermId, b: TermId, range: Option<(i128, i128)>) -> bool {
        let Some((least, most)) = range else { return false };
        let ((low_a, high_a), (low_b, high_b)) = (self.bounds(state, a), self.bounds(state, b));
        let ends = [
            low_a.checked_mul(low_b),
            low_a.checked_mul(high_b),
            high_a.checked_mul(low_b),
            high_a.checked_mul(high_b),
        ];
        match ends {
            [Some(a), Some(b), Some(c), Some(d)] => {
                least <= a.min(b).min(c).min(d) && a.max(b).max(c).max(d) <= most
            }
            _ => false,
        }
    }

    /// Keeps the verdict of the operation being walked, when recording.
    fn note(&mut self, kind: OpKind, missing: Option<Missing>) {
        if !self.recording {
            return;
        }
        if let MirSpan::ExprId(expr) = self.span {
            self.verdicts.push((expr, kind, missing));
        }
    }

    /// The verdict of a shift of a `ty` by `amount`: it is below the bits and not negative.
    /// Taken to hold after it, as every operation an overflow proof reports.
    fn note_shift(&mut self, state: &mut State, ty: Ty<'db>, amount: TermId) {
        let bits = type_bits(ty, self.word);
        let (zero, limit) = (self.constant(0), self.constant(i128::from(bits)));
        let (natural, below) = (Atom::new(Rel::Le, zero, amount), Atom::new(Rel::Lt, amount, limit));
        let shown = self.entails(state, natural) && self.entails(state, below);
        self.note(OpKind::Shift, (!shown).then_some(Missing::Amount(bits)));
        let _ = self.assume(state, natural) && self.assume(state, below);
    }

    /// The verdict of a cast of `value`, a `from`, to `to`: every value it can have here is
    /// one `to` holds, on every pointer width (`usize` and `isize` as narrow as 32 bits).
    fn note_cast(&mut self, state: &State, value: TermId, from: Ty<'db>, to: Ty<'db>) {
        if !self.recording {
            return;
        }
        let Some((least, most)) = int_range(to, self.word.min(32)) else { return };
        if wide(from) || wide(to) {
            return self.note(OpKind::Cast, Some(Missing::Wide));
        }
        let (low, high) = (self.constant(least), self.constant(most));
        let missing = if !self.entails(state, Atom::new(Rel::Le, low, value)) {
            Some(Missing::Below(least))
        } else if !self.entails(state, Atom::new(Rel::Le, value, high)) {
            Some(Missing::Above(most))
        } else {
            None
        };
        self.note(OpKind::Cast, missing);
    }

    /// What is missing for `a op b`, a `ty` in `range`, to fit: an operand nothing bounds on
    /// the side the result needs, `b <= a` for an unsigned difference, or how far what bounds
    /// them lets it reach.
    fn missing_arith(
        &self,
        state: &State,
        op: &BinOp,
        (a, b): (TermId, TermId),
        exact: Option<&Form>,
        ty: Ty<'db>,
        (least, most): (i128, i128),
    ) -> Missing {
        if wide(ty) {
            return Missing::Wide;
        }
        let known = self.known(state);
        let mut fuel = LINEAR_FUEL;
        let zero: Form = (Vec::new(), 0);
        let (high_ok, low_ok) = match exact {
            Some(form) => {
                let high = self.lin_le(state, form, most, &known, LINEAR_DEPTH, &mut fuel);
                let low = match (form_add(&zero, form, -1), least.checked_neg()) {
                    (Some(negated), Some(floor)) => {
                        self.lin_le(state, &negated, floor, &known, LINEAR_DEPTH, &mut fuel)
                    }
                    _ => false,
                };
                (high, low)
            }
            None => (false, false),
        };
        let sub = matches!(op, BinOp::Sub | BinOp::SubWithOverflow);
        let mul = matches!(op, BinOp::Mul | BinOp::MulWithOverflow);
        if sub && least == 0 && high_ok && !low_ok {
            return Missing::Order;
        }
        let ((low_a, high_a), (low_b, high_b)) = (self.bounds(state, a), self.bounds(state, b));
        let above = |high: i128| high >= most;
        let below = |low: i128| least < 0 && low <= least;
        let (left, right) = match (mul, sub) {
            (true, _) => (above(high_a) || below(low_a), above(high_b) || below(low_b)),
            (false, true) => (
                (!high_ok && above(high_a)) || (!low_ok && below(low_a)),
                (!high_ok && below(low_b)) || (!low_ok && above(high_b)),
            ),
            (false, false) => (
                (!high_ok && above(high_a)) || (!low_ok && below(low_a)),
                (!high_ok && above(high_b)) || (!low_ok && below(low_b)),
            ),
        };
        if left || right {
            return Missing::Unbounded { left, right };
        }
        // Both are bounded: how far the bounds let it go, on the side that fails.
        let reach = match (exact, mul) {
            (Some(form), false) if !high_ok => self.form_high(state, form),
            (Some(form), false) => form_add(&zero, form, -1)
                .and_then(|negated| self.form_high(state, &negated))
                .and_then(i128::checked_neg),
            _ => {
                let ends = [
                    low_a.checked_mul(low_b),
                    low_a.checked_mul(high_b),
                    high_a.checked_mul(low_b),
                    high_a.checked_mul(high_b),
                ];
                ends.into_iter()
                    .flatten()
                    .filter(|&it| it < least || most < it)
                    .max_by_key(|it| it.unsigned_abs())
            }
        };
        match reach {
            Some(value) => Missing::Reaches(value),
            None => Missing::Unbounded { left: true, right: true },
        }
    }

    fn not(&mut self, of: TermId) -> TermId {
        match self.terms[of as usize] {
            Term::Const(value) => self.constant(i128::from(value == 0)),
            Term::Not(inner) => inner,
            Term::Cmp(rel, a, b) => {
                let atom = Atom { rel, a, b }.not();
                self.term(Term::Cmp(atom.rel, atom.a, atom.b), (0, 1))
            }
            _ => self.term(Term::Not(of), (0, 1)),
        }
    }

    /// Narrows `term`'s range to what `ty` holds.
    fn typed(&mut self, term: TermId, ty: Ty<'db>) -> TermId {
        // A linear form is a number whose bounds are its terms': a place it sits in says
        // nothing of it in a state where it is not that place's value.
        if matches!(self.terms[term as usize], Term::Lin(..) | Term::Const(_)) {
            return term;
        }
        if let Some(range) = int_range(ty, self.word) {
            let had = self.ranges[term as usize];
            self.ranges[term as usize] = (had.0.max(range.0), had.1.min(range.1));
        }
        term
    }

    /// Whether `term` is, or is made of, an unknown `of` says yes to.
    fn mentions(&self, term: TermId, of: &impl Fn(Key) -> bool) -> bool {
        match &self.terms[term as usize] {
            Term::Const(_) => false,
            Term::Unknown(key) => of(*key),
            Term::Len(a)
            | Term::Discr(a)
            | Term::Field(a, _)
            | Term::Payload(a, _, _)
            | Term::Not(a)
            | Term::RefTo(a)
            | Term::Deref(a) => self.mentions(*a, of),
            Term::Cmp(_, a, b) | Term::DerefAt(a, b) | Term::MutRef(_, a, b) => {
                self.mentions(*a, of) || self.mentions(*b, of)
            }
            Term::Agg(_, fields) => fields.iter().any(|&it| self.mentions(it, of)),
            Term::Lin(parts, _) => parts.iter().any(|&(it, _)| self.mentions(it, of)),
            Term::Wrap(a, _) => self.mentions(*a, of),
        }
    }

    // Entailment.

    /// The least and greatest value `term` can have in `state`: [`Self::bounds_by_constants`],
    /// narrowed once more by the facts that compare it with another term (`i < n` and
    /// `n <= MAX` give `i <= MAX - 1`).
    fn bounds(&self, state: &State, term: TermId) -> (i128, i128) {
        let (mut low, mut high) = self.bounds_by_constants(state, term);
        if self.const_of(term).is_some() {
            return (low, high);
        }
        for fact in &state.facts {
            let (left, right) = (fact.a == term, fact.b == term);
            if left == right {
                continue;
            }
            let other = if left { fact.b } else { fact.a };
            if self.const_of(other).is_some() {
                continue;
            }
            let (least, most) = self.bounds_by_constants(state, other);
            match (fact.rel, left) {
                (Rel::Lt, true) => high = high.min(most.saturating_sub(1)),
                (Rel::Le, true) => high = high.min(most),
                (Rel::Lt, false) => low = low.max(least.saturating_add(1)),
                (Rel::Le, false) => low = low.max(least),
                (Rel::Eq, _) => {
                    low = low.max(least);
                    high = high.min(most);
                }
                (Rel::Ne, _) => {}
            }
        }
        (low, high)
    }

    /// The least and greatest value `term` can have in `state`: its type's, narrowed by the
    /// facts that compare it with a constant.
    fn bounds_by_constants(&self, state: &State, term: TermId) -> (i128, i128) {
        if let Some(value) = self.const_of(term) {
            return (value, value);
        }
        // A form's bounds are its terms', added up; an end a term leaves open is open.
        if let Term::Lin(parts, constant) = &self.terms[term as usize] {
            let (mut low, mut high) = (Some(*constant), Some(*constant));
            for &(part, coefficient) in parts.iter() {
                let (least, most) = self.bounds_by_constants(state, part);
                let (down, up) = if coefficient > 0 { (least, most) } else { (most, least) };
                let end = |sum: Option<i128>, end: i128| {
                    let open = end == i128::MAX || end == i128::MIN;
                    sum.filter(|_| !open)?.checked_add(coefficient.checked_mul(end)?)
                };
                (low, high) = (end(low, down), end(high, up));
            }
            return (low.unwrap_or(i128::MIN), high.unwrap_or(i128::MAX));
        }
        let (mut low, mut high) = self.ranges[term as usize];
        for fact in &state.facts {
            let (left, right) = (fact.a == term, fact.b == term);
            if !left && !right {
                continue;
            }
            let Some(value) = self.const_of(if left { fact.b } else { fact.a }) else {
                continue;
            };
            match (fact.rel, left) {
                (Rel::Lt, true) => high = high.min(value.saturating_sub(1)),
                (Rel::Le, true) => high = high.min(value),
                (Rel::Lt, false) => low = low.max(value.saturating_add(1)),
                (Rel::Le, false) => low = low.max(value),
                (Rel::Eq, _) => {
                    low = low.max(value);
                    high = high.min(value);
                }
                (Rel::Ne, _) => {}
            }
        }
        // A value ruled out at an end moves the end: at most as many turns as such facts.
        let mut turns = state.facts.len();
        loop {
            let mut moved = false;
            for fact in &state.facts {
                if fact.rel != Rel::Ne || (fact.a != term && fact.b != term) {
                    continue;
                }
                let other = if fact.a == term { fact.b } else { fact.a };
                let Some(value) = self.const_of(other) else { continue };
                if value == low && low <= high {
                    low = low.saturating_add(1);
                    moved = true;
                }
                if value == high && low <= high {
                    high = high.saturating_sub(1);
                    moved = true;
                }
            }
            if !moved || turns == 0 {
                break;
            }
            turns -= 1;
        }
        (low, high)
    }

    /// Whether `state` entails `atom`, following at most `depth` facts or implications.
    fn holds(&self, state: &State, atom: Atom, depth: u8) -> bool {
        let Atom { rel, a, b } = atom;
        if a == b {
            return matches!(rel, Rel::Le | Rel::Eq);
        }
        if state.has(atom) {
            return true;
        }
        let known = |rel, a, b| state.has(Atom::new(rel, a, b));
        let stated = match rel {
            Rel::Le => known(Rel::Lt, a, b) || known(Rel::Eq, a, b),
            Rel::Ne => known(Rel::Lt, a, b) || known(Rel::Lt, b, a),
            Rel::Lt | Rel::Eq => false,
        };
        if stated {
            return true;
        }
        let ((low_a, high_a), (low_b, high_b)) = (self.bounds(state, a), self.bounds(state, b));
        let bounded = match rel {
            Rel::Lt => high_a < low_b,
            Rel::Le => high_a <= low_b,
            Rel::Eq => low_a == high_a && low_b == high_b && low_a == low_b,
            Rel::Ne => high_a < low_b || high_b < low_a,
        };
        if bounded {
            return true;
        }
        // A comparison's value is 1 exactly when the comparison holds.
        if matches!(rel, Rel::Eq | Rel::Ne) {
            for (term, other) in [(a, b), (b, a)] {
                let Some(value) = self.const_of(other) else { continue };
                let truth = match (rel, value) {
                    (Rel::Eq, 1) | (Rel::Ne, 0) => true,
                    (Rel::Eq, 0) | (Rel::Ne, 1) => false,
                    _ => continue,
                };
                match self.terms[term as usize] {
                    Term::Cmp(rel, x, y) => {
                        let inner = Atom { rel, a: x, b: y };
                        if self.holds(state, if truth { inner } else { inner.not() }, depth) {
                            return true;
                        }
                    }
                    Term::Not(inner) if depth > 0 => {
                        let want = self.ids.get(&Term::Const(i128::from(!truth)));
                        if let Some(&want) = want
                            && self.holds(state, Atom::new(Rel::Eq, inner, want), depth - 1)
                        {
                            return true;
                        }
                    }
                    _ => {}
                }
            }
        }
        // Adding facts up: `i + 1 < n` gives `i < n`, `k <= n` gives `0 <= n - k`. Once, at
        // the top: the steps below ask about single facts.
        if depth >= 2 && self.linear(state, atom) {
            return true;
        }
        if depth == 0 {
            return false;
        }
        // One step through a fact: `a <= m` and `m < b`, and the like; `a == m` rewrites.
        for fact in &state.facts {
            let through = |mid: TermId, strict: bool| -> bool {
                match rel {
                    Rel::Lt if strict => self.holds(state, Atom::new(Rel::Le, mid, b), depth - 1),
                    Rel::Lt => self.holds(state, Atom::new(Rel::Lt, mid, b), depth - 1),
                    Rel::Le => self.holds(state, Atom::new(Rel::Le, mid, b), depth - 1),
                    Rel::Eq | Rel::Ne => false,
                }
            };
            let stepped = match fact.rel {
                Rel::Lt if fact.a == a && fact.b != b => through(fact.b, true),
                Rel::Le if fact.a == a && fact.b != b => through(fact.b, false),
                Rel::Eq if fact.a == a && fact.b != b => {
                    self.holds(state, Atom::new(rel, fact.b, b), depth - 1)
                }
                Rel::Eq if fact.b == a && fact.a != b => {
                    self.holds(state, Atom::new(rel, fact.a, b), depth - 1)
                }
                Rel::Eq if fact.a == b && fact.b != a => {
                    self.holds(state, Atom::new(rel, a, fact.b), depth - 1)
                }
                Rel::Eq if fact.b == b && fact.a != a => {
                    self.holds(state, Atom::new(rel, a, fact.a), depth - 1)
                }
                _ => false,
            };
            if stepped {
                return true;
            }
        }
        state
            .imps
            .iter()
            .any(|&(premise, then)| then == atom && self.holds(state, premise, depth - 1))
    }

    fn entails(&self, state: &State, atom: Atom) -> bool {
        self.holds(state, atom, 2)
    }

    /// Adds `atom` to `state`: false when the state then has no run (the atom contradicts it).
    fn assume(&mut self, state: &mut State, atom: Atom) -> bool {
        if self.entails(state, atom.not()) {
            return false;
        }
        // A comparison's value being 0 or 1 is the comparison, or its negation.
        if matches!(atom.rel, Rel::Eq | Rel::Ne) {
            for (term, other) in [(atom.a, atom.b), (atom.b, atom.a)] {
                let Some(value) = self.const_of(other) else { continue };
                let truth = match (atom.rel, value) {
                    (Rel::Eq, 1) | (Rel::Ne, 0) => true,
                    (Rel::Eq, 0) | (Rel::Ne, 1) => false,
                    _ => continue,
                };
                match self.terms[term as usize] {
                    Term::Cmp(rel, a, b) => {
                        let inner = Atom { rel, a, b };
                        return self.assume(state, if truth { inner } else { inner.not() });
                    }
                    Term::Not(inner) => {
                        let want = self.constant(i128::from(!truth));
                        return self.assume(state, Atom::new(Rel::Eq, inner, want));
                    }
                    _ => {}
                }
            }
        }
        // The fact is written down even when the state already entails it (through an
        // implication, or by adding others up): the linear proofs read the facts as written.
        let trivial = atom.a == atom.b
            || (self.const_of(atom.a).is_some() && self.const_of(atom.b).is_some());
        if !trivial && !state.has(atom) {
            state.put(atom);
            for term in [atom.a, atom.b] {
                let (low, high) = self.bounds(state, term);
                if low > high {
                    return false;
                }
            }
        }
        self.saturate(state)
    }

    /// Writes down what the state's implications give: the conclusion of each whose premise
    /// holds. False when one contradicts the state. Each implication fires at most once.
    fn saturate(&self, state: &mut State) -> bool {
        let mut budget = state.imps.len();
        loop {
            let next = state
                .imps
                .iter()
                .find(|&&(premise, then)| !state.has(then) && self.entails(state, premise))
                .copied();
            let Some((_, then)) = next else { return true };
            if budget == 0 {
                return true;
            }
            budget -= 1;
            if self.entails(state, then.not()) {
                return false;
            }
            state.put(then);
        }
    }

    // The state.

    fn epoch_slot(&self) -> usize {
        self.body.locals.len()
    }

    fn local_ty(&self, slot: usize) -> Option<Ty<'db>> {
        self.body.locals.iter().nth(slot).map(|(_, local)| local.ty.as_ref())
    }

    fn entry(&mut self) -> State {
        let slots = self.epoch_slot() + 1;
        let mut vals = Vec::with_capacity(slots); // alloc: one value per slot, the first state
        for slot in 0..slots {
            let ty = self.local_ty(slot);
            vals.push(self.unknown(Key::Entry(slot as u32), ty));
        }
        State { vals, facts: Vec::new(), imps: Vec::new() }
    }

    /// Forgets what statement `at` made the last time it ran: the facts that mention it, and
    /// the slots that hold it, which become unknowns of this statement.
    fn kill_point(&mut self, state: &mut State, at: (u32, u32)) {
        if !self.cyclic[at.0 as usize] {
            return;
        }
        let of = |key: Key| matches!(key, Key::At(block, stmt, _) if (block, stmt) == at);
        self.kill(state, &of, |slot| Key::At(at.0, at.1, STALE + slot));
    }

    /// Forgets every unknown `of` says yes to: the facts that mention one go, and a slot that
    /// holds one gets the unknown `stale` names for it.
    fn kill(&mut self, state: &mut State, of: &impl Fn(Key) -> bool, stale: impl Fn(u32) -> Key) {
        state.facts.retain(|fact| !self.mentions(fact.a, of) && !self.mentions(fact.b, of));
        state.imps.retain(|(premise, then)| {
            ![premise.a, premise.b, then.a, then.b].iter().any(|&it| self.mentions(it, of))
        });
        for slot in 0..state.vals.len() {
            if self.mentions(state.vals[slot], of) {
                let ty = self.local_ty(slot);
                state.vals[slot] = self.unknown(stale(slot as u32), ty);
            }
        }
    }

    /// A new epoch: what is behind a `&mut`, a `Box` or a raw pointer may have changed, and so
    /// may every local whose address one of them took.
    fn bump(&mut self, state: &mut State) {
        let (block, stmt) = self.at;
        let epoch = self.epoch_slot();
        state.vals[epoch] = self.unknown(Key::At(block, stmt, EPOCH), None);
        for slot in 0..epoch {
            if self.exposed[slot] {
                let ty = self.local_ty(slot);
                state.vals[slot] = self.unknown(Key::At(block, stmt, HAVOC + slot as u32), ty);
            }
        }
    }

    /// The locals a `&mut` borrow or a raw pointer was made of: what a write through a pointer
    /// or an unknown call may change. A shared borrow counts too when the body casts a
    /// reference to a raw pointer anywhere.
    fn exposed_locals(&self) -> Vec<bool> {
        let mut exposed = vec![false; self.body.locals.len()]; // alloc: one flag per local, once per proof
        let mut shared = vec![false; self.body.locals.len()]; // alloc: as above
        let mut cast_to_raw = false;
        for block in self.blocks {
            for statement in &block.statements {
                let StatementKind::Assign(to, rvalue) = &statement.kind else { continue };
                match rvalue {
                    Rvalue::Ref(kind, place) => {
                        let direct = !place
                            .projection
                            .as_slice()
                            .iter()
                            .any(|it| matches!(it, ProjectionElem::Deref));
                        if !direct {
                            continue;
                        }
                        let raw = matches!(self.place_ty(to).kind(), TyKind::RawPtr(..));
                        let slot = local_index(place.local);
                        if raw || matches!(kind, BorrowKind::Mut { .. }) {
                            exposed[slot] = true;
                        } else {
                            shared[slot] = true;
                        }
                    }
                    Rvalue::Cast(_, operand, ty) => {
                        let from_ref = self
                            .operand_ty(operand)
                            .is_none_or(|it| !matches!(it.kind(), TyKind::RawPtr(..)));
                        if matches!(ty.as_ref().kind(), TyKind::RawPtr(..)) && from_ref {
                            cast_to_raw = true;
                        }
                    }
                    _ => {}
                }
            }
        }
        if cast_to_raw {
            for (slot, shared) in shared.into_iter().enumerate() {
                exposed[slot] |= shared;
            }
        }
        exposed
    }

    fn place_ty(&self, place: &StoredPlace) -> Ty<'db> {
        place.as_ref().ty(self.body, self.infcx, self.env).ty
    }

    fn operand_ty(&self, operand: &Operand) -> Option<Ty<'db>> {
        match &operand.kind {
            OperandKind::Copy(place) | OperandKind::Move(place) => Some(self.place_ty(place)),
            OperandKind::Constant { ty, .. } => Some(ty.as_ref()),
            OperandKind::Allocation { allocation } => Some(allocation.as_ref().ty),
            OperandKind::Static(_) => None,
        }
    }

    /// What the pointer `pointer` of type `ty` leads to.
    fn deref(&mut self, state: &State, pointer: TermId, ty: Ty<'db>) -> TermId {
        match self.terms[pointer as usize] {
            Term::RefTo(value) => return value,
            Term::MutRef(_, value, epoch) if epoch == state.vals[self.epoch_slot()] => {
                return value;
            }
            _ => {}
        }
        match ty.kind() {
            TyKind::Ref(_, pointee, mutability) => {
                // A slice's length is the reference's own, whatever is written behind it.
                let fixed = matches!(pointee.kind(), TyKind::Slice(_) | TyKind::Str);
                if fixed || mutability == rustc_type_ir::Mutability::Not {
                    self.term(Term::Deref(pointer), ANY)
                } else {
                    let epoch = state.vals[self.epoch_slot()];
                    self.term(Term::DerefAt(pointer, epoch), ANY)
                }
            }
            TyKind::Adt(def, _) if def.is_box() => {
                let epoch = state.vals[self.epoch_slot()];
                self.term(Term::DerefAt(pointer, epoch), ANY)
            }
            _ => self.fresh(None),
        }
    }

    /// The value `place` holds in `state`.
    fn read(&mut self, state: &State, place: &StoredPlace) -> TermId {
        let mut term = state.vals[local_index(place.local)];
        let mut ty = PlaceTy::from_ty(self.body.locals[place.local].ty.as_ref());
        let mut variant = None;
        for elem in place.projection.as_slice() {
            term = match elem {
                ProjectionElem::Deref => self.deref(state, term, ty.ty),
                ProjectionElem::Field(field) => match variant.take() {
                    Some(variant) => self.payload(term, variant, field.0),
                    None => self.field(term, field.0),
                },
                ProjectionElem::Downcast(VariantId::EnumVariantId(it)) => {
                    variant = Some(self.variant(*it));
                    term
                }
                // An element, a subslice or a reflected field: not followed.
                _ => self.fresh(None),
            };
            ty = ty.projection_ty(self.infcx, elem, self.env);
        }
        self.typed(term, ty.ty)
    }

    fn operand(&mut self, state: &State, operand: &Operand) -> TermId {
        match &operand.kind {
            OperandKind::Copy(place) | OperandKind::Move(place) => self.read(state, place),
            OperandKind::Constant { .. } | OperandKind::Allocation { .. } => {
                let ty = self.operand_ty(operand);
                match ty.and_then(|ty| constant_value(operand, ty, self.word)) {
                    Some(value) => self.constant(value),
                    None => self.fresh(ty),
                }
            }
            OperandKind::Static(_) => self.fresh(None),
        }
    }

    /// Puts `value` in `place`.
    fn write(&mut self, state: &mut State, place: &StoredPlace, value: TermId) {
        let slot = local_index(place.local);
        let projection = place.projection.as_slice();
        if projection.is_empty() {
            state.vals[slot] = value;
            if self.named[slot] && !self.names.contains_key(&value) {
                self.names.insert(value, slot as u32);
            }
            return;
        }
        if projection.iter().any(|it| matches!(it, ProjectionElem::Deref)) {
            self.bump(state);
            return;
        }
        // A part of the local: one field of a known aggregate is replaced, anything else
        // leaves the local unknown.
        let (block, stmt) = self.at;
        let replaced = match (projection, &self.terms[state.vals[slot] as usize]) {
            ([ProjectionElem::Field(field)], Term::Agg(variant, fields))
                if *variant == NO_VARIANT && (field.0 as usize) < fields.len() =>
            {
                let mut fields = fields.clone(); // clone: the aggregate with one field replaced
                fields[field.0 as usize] = value;
                Some(Term::Agg(NO_VARIANT, fields))
            }
            _ => None,
        };
        state.vals[slot] = match replaced {
            Some(term) => self.term(term, ANY),
            None => {
                let ty = self.local_ty(slot);
                self.unknown(Key::At(block, stmt, HAVOC + slot as u32), ty)
            }
        };
    }

    fn rvalue(&mut self, state: &mut State, to: &StoredPlace, rvalue: &Rvalue) -> TermId {
        match rvalue {
            Rvalue::Use(operand) => self.operand(state, operand),
            Rvalue::CopyForDeref(place) => self.read(state, place),
            Rvalue::Ref(kind, place) => {
                let raw = matches!(self.place_ty(to).kind(), TyKind::RawPtr(..));
                let value = self.read(state, place);
                match kind {
                    _ if raw => self.fresh(None),
                    BorrowKind::Shared | BorrowKind::Shallow => self.term(Term::RefTo(value), ANY),
                    BorrowKind::Mut { .. } if place.projection.as_slice().is_empty() => {
                        let epoch = state.vals[self.epoch_slot()];
                        let local = local_index(place.local) as u32;
                        self.term(Term::MutRef(local, value, epoch), ANY)
                    }
                    // A reborrow of all a `&mut` leads to is that `&mut`.
                    BorrowKind::Mut { .. }
                        if matches!(place.projection.as_slice(), [ProjectionElem::Deref]) =>
                    {
                        state.vals[local_index(place.local)]
                    }
                    BorrowKind::Mut { .. } => self.fresh(None),
                }
            }
            Rvalue::Len(place) => {
                let value = self.read(state, place);
                self.len(value)
            }
            Rvalue::Discriminant(place) => {
                let value = self.read(state, place);
                let ty = self.place_ty(place);
                self.discr(value, Some(ty))
            }
            Rvalue::Cast(CastKind::IntToInt, operand, ty) => {
                // A cast to a type that holds every value of the operand's type is the value.
                // Decided by the two types, never by what this state knows of the value: a
                // term's range is its type's in every state it appears in.
                let value = self.operand(state, operand);
                if self.mode == Mode::Overflow
                    && let Some(from) = self.operand_ty(operand)
                {
                    self.note_cast(state, value, from, ty.as_ref());
                }
                let from = self.operand_ty(operand).and_then(|it| int_range(it, self.word));
                match (from, int_range(ty.as_ref(), self.word)) {
                    (Some((low, high)), Some((least, most))) if least <= low && high <= most => {
                        value
                    }
                    _ => self.fresh(Some(ty.as_ref())),
                }
            }
            Rvalue::UnaryOp(UnOp::Not, operand)
                if self.operand_ty(operand).is_some_and(|it| it.is_bool()) =>
            {
                let value = self.operand(state, operand);
                self.not(value)
            }
            Rvalue::UnaryOp(UnOp::Neg, operand)
                if self.mode == Mode::Overflow
                    && self.operand_ty(operand).is_some_and(|it| matches!(it.kind(), TyKind::Int(_))) =>
            {
                let ty = self.operand_ty(operand);
                let value = self.operand(state, operand);
                let least = ty.and_then(|it| int_range(it, self.word)).map_or(i128::MIN, |it| it.0);
                let min = self.constant(least);
                let not_min = Atom::new(Rel::Ne, value, min);
                let shown = self.entails(state, not_min);
                self.note(OpKind::Neg, (!shown).then_some(Missing::NotMin));
                // Taken to fit after it, as every operation an overflow proof reports.
                let _ = self.assume(state, not_min);
                let zero: Form = (Vec::new(), 0);
                match form_add(&zero, &self.form(value), -1) {
                    Some(negated) => self.lin(negated),
                    None => self.fresh(ty),
                }
            }
            // Arithmetic that is linear: the exact sum, difference or multiple, as a form. A
            // local holds it only where it fits the type: shown by the state, or, for a
            // checked operation, on the far side of its overflow check (the flag being false
            // implies the value is the form). A wrapping operation not shown to fit is the
            // form wrapped, an unknown that the same operands give again.
            Rvalue::BinaryOp(
                op @ (BinOp::Add
                | BinOp::Sub
                | BinOp::Mul
                | BinOp::Shl
                | BinOp::AddWithOverflow
                | BinOp::SubWithOverflow
                | BinOp::MulWithOverflow),
                left,
                right,
            ) => {
                let ty = self.operand_ty(left);
                let range = ty.and_then(|it| int_range(it, self.word));
                let (a, b) = (self.operand(state, left), self.operand(state, right));
                let (form_a, form_b) = (self.form(a), self.form(b));
                let exact = match op {
                    BinOp::Add | BinOp::AddWithOverflow => form_add(&form_a, &form_b, 1),
                    BinOp::Sub | BinOp::SubWithOverflow => form_add(&form_a, &form_b, -1),
                    BinOp::Mul | BinOp::MulWithOverflow => {
                        let zero: Form = (Vec::new(), 0);
                        match (form_a.0.is_empty(), form_b.0.is_empty()) {
                            (true, _) => form_add(&zero, &form_b, form_a.1),
                            (_, true) => form_add(&zero, &form_a, form_b.1),
                            // A product of two unknowns is not linear.
                            _ => None,
                        }
                    }
                    _ => {
                        let zero: Form = (Vec::new(), 0);
                        match form_b {
                            (parts, by) if parts.is_empty() && (0..=100).contains(&by) => {
                                form_add(&zero, &form_a, 1i128 << by)
                            }
                            _ => None,
                        }
                    }
                };
                let checked = matches!(
                    op,
                    BinOp::AddWithOverflow | BinOp::SubWithOverflow | BinOp::MulWithOverflow
                );
                let fits = match (&exact, range) {
                    (Some(form), Some((least, most))) => self.form_within(state, form, least, most),
                    _ => false,
                };
                // A product of two unknowns fits when every product of their bounds does.
                let product = matches!(op, BinOp::Mul | BinOp::MulWithOverflow) && exact.is_none();
                let inside = product && self.product_within(state, a, b, range);
                let integral = ty.is_some_and(|it| matches!(it.kind(), TyKind::Int(_) | TyKind::Uint(_)));
                if self.mode == Mode::Overflow
                    && integral
                    && let (Some(ty), Some(range)) = (ty, range)
                {
                    match op {
                        BinOp::Shl => self.note_shift(state, ty, b),
                        _ => {
                            #[cfg(test)]
                            let planted = matches!(op, BinOp::Add | BinOp::AddWithOverflow)
                                && PLANT_ADD_ALWAYS_FITS.get();
                            #[cfg(not(test))]
                            let planted = false;
                            let shown = (fits || inside || planted) && !wide(ty);
                            let missing = (!shown)
                                .then(|| self.missing_arith(state, op, (a, b), exact.as_ref(), ty, range));
                            self.note(OpKind::Arith, missing);
                        }
                    }
                }
                let pair = |this: &mut Self, value, flag| {
                    this.term(Term::Agg(NO_VARIANT, Box::new([value, flag])), ANY)
                };
                let shift = matches!(op, BinOp::Shl);
                match (exact, range) {
                    (Some(form), Some(_)) if fits && !checked => self.lin(form),
                    (Some(form), Some(_)) if fits => {
                        let (value, flag) = (self.lin(form), self.constant(0));
                        pair(self, value, flag)
                    }
                    // An overflow proof takes an operation it could not show to fit to fit:
                    // it is reported, and a run past it with overflow checks had it fit.
                    // `<<` drops its high bits in every profile, so it is not one.
                    (Some(form), Some(_)) if self.mode == Mode::Overflow && !shift => {
                        let value = self.fresh(ty);
                        let exact = self.lin(form);
                        state.put(Atom::new(Rel::Eq, value, exact));
                        match checked {
                            true => {
                                let flag = self.constant(0);
                                pair(self, value, flag)
                            }
                            false => value,
                        }
                    }
                    (Some(form), Some(_)) if checked => {
                        let (value, flag) = (self.fresh(ty), self.fresh(None));
                        self.ranges[flag as usize] = (0, 1);
                        let (exact, zero) = (self.lin(form), self.constant(0));
                        state.put_imp((
                            Atom::new(Rel::Eq, flag, zero),
                            Atom::new(Rel::Eq, value, exact),
                        ));
                        pair(self, value, flag)
                    }
                    (Some(form), Some((least, _))) => {
                        let exact = self.lin(form);
                        let bits = ty.map_or(0, |it| type_bits(it, self.word));
                        let key = bits + if least < 0 { 128 } else { 0 };
                        let wrapped = self.term(Term::Wrap(exact, key), ANY);
                        match ty {
                            Some(ty) => self.typed(wrapped, ty),
                            None => wrapped,
                        }
                    }
                    _ if checked => {
                        let value = self.fresh(ty);
                        let flag = match inside || self.mode == Mode::Overflow {
                            true => self.constant(0),
                            false => {
                                let flag = self.fresh(None);
                                self.ranges[flag as usize] = (0, 1);
                                flag
                            }
                        };
                        pair(self, value, flag)
                    }
                    _ => {
                        let ty = self.place_ty(to);
                        self.fresh(Some(ty))
                    }
                }
            }
            // `x / d` and `x >> c` of a value known not negative, `d` a positive constant: a
            // quotient `q` with `d * q <= x <= d * q + d - 1`, and so `q <= x`, and `q < x`
            // when `d >= 2` and `x >= 1`.
            Rvalue::BinaryOp(op @ (BinOp::Div | BinOp::Shr), left, right) => {
                let ty = self.operand_ty(left);
                let integral = ty.is_some_and(|it| int_range(it, self.word).is_some());
                let (a, b) = (self.operand(state, left), self.operand(state, right));
                if self.mode == Mode::Overflow
                    && *op == BinOp::Shr
                    && let Some(ty) = ty
                    && matches!(ty.kind(), TyKind::Int(_) | TyKind::Uint(_))
                {
                    self.note_shift(state, ty, b);
                }
                let by = match (op, self.const_of(b)) {
                    (BinOp::Div, Some(by)) if by > 0 => Some(by),
                    (BinOp::Shr, Some(by)) if (0..=100).contains(&by) => Some(1i128 << by),
                    _ => None,
                };
                let zero = self.constant(0);
                let positive = integral && self.entails(state, Atom::new(Rel::Le, zero, a));
                match by {
                    Some(by) if positive => {
                        let quotient = self.fresh(ty);
                        let single: Form = (vec![(quotient, 1)], 0); // alloc: one term
                        let none: Form = (Vec::new(), 0);
                        state.put(Atom::new(Rel::Le, zero, quotient));
                        state.put(Atom::new(Rel::Le, quotient, a));
                        if let Some(low) = form_add(&none, &single, by) {
                            let top = form_add(&low, &(Vec::new(), by - 1), 1);
                            let low = self.lin(low);
                            state.put(Atom::new(Rel::Le, low, a));
                            if let Some(top) = top {
                                let top = self.lin(top);
                                state.put(Atom::new(Rel::Le, a, top));
                            }
                        }
                        if by >= 2 && self.entails(state, Atom::new(Rel::Lt, zero, a)) {
                            state.put(Atom::new(Rel::Lt, quotient, a));
                        }
                        quotient
                    }
                    _ => {
                        let ty = self.place_ty(to);
                        self.fresh(Some(ty))
                    }
                }
            }
            // `x & m` of a constant `m >= 0` is in `0..=m`, and at most `x` when `x >= 0`;
            // `x % d` of `x >= 0` and `d > 0` is in `0..d`, and at most `x`: the masks and
            // remainders that say which bits or which slot is meant.
            Rvalue::BinaryOp(op @ (BinOp::BitAnd | BinOp::Rem), left, right)
                if self.operand_ty(left).is_some_and(|it| {
                    matches!(it.kind(), TyKind::Int(_) | TyKind::Uint(_))
                }) =>
            {
                let ty = self.operand_ty(left);
                let (a, b) = (self.operand(state, left), self.operand(state, right));
                let zero = self.constant(0);
                let natural =
                    |this: &Self, state: &State, it| this.entails(state, Atom::new(Rel::Le, zero, it));
                let result = self.fresh(ty);
                match op {
                    BinOp::BitAnd => {
                        let mask = [(a, b), (b, a)].into_iter().find_map(|(value, mask)| {
                            self.const_of(mask).filter(|&it| it >= 0).map(|it| (value, it))
                        });
                        if let Some((value, mask)) = mask {
                            let mask = self.constant(mask);
                            state.put(Atom::new(Rel::Le, zero, result));
                            state.put(Atom::new(Rel::Le, result, mask));
                            if natural(self, state, value) {
                                state.put(Atom::new(Rel::Le, result, value));
                            }
                        }
                    }
                    _ => {
                        let positive = self.entails(state, Atom::new(Rel::Lt, zero, b));
                        if positive && natural(self, state, a) {
                            state.put(Atom::new(Rel::Le, zero, result));
                            state.put(Atom::new(Rel::Lt, result, b));
                            state.put(Atom::new(Rel::Le, result, a));
                        }
                    }
                }
                result
            }
            Rvalue::BinaryOp(op, left, right) => {
                let rel = match op {
                    BinOp::Lt | BinOp::Gt => Some(Rel::Lt),
                    BinOp::Le | BinOp::Ge => Some(Rel::Le),
                    BinOp::Eq => Some(Rel::Eq),
                    BinOp::Ne => Some(Rel::Ne),
                    _ => None,
                };
                // Integers only: a float's `!(a < b)` is not `b <= a`.
                let integral =
                    self.operand_ty(left).is_some_and(|it| int_range(it, self.word).is_some());
                match rel {
                    Some(rel) if integral => {
                        let (a, b) = (self.operand(state, left), self.operand(state, right));
                        match op {
                            BinOp::Gt | BinOp::Ge => self.cmp(rel, b, a),
                            _ => self.cmp(rel, a, b),
                        }
                    }
                    _ => {
                        let ty = self.place_ty(to);
                        self.fresh(Some(ty))
                    }
                }
            }
            Rvalue::Aggregate(kind, operands) => {
                let variant = match kind {
                    AggregateKind::Adt(VariantId::EnumVariantId(it), _) => Some(self.variant(*it)),
                    AggregateKind::Adt(VariantId::StructId(_), _) | AggregateKind::Tuple(_) => {
                        Some(NO_VARIANT)
                    }
                    _ => None,
                };
                match variant {
                    Some(variant) => {
                        // alloc: the aggregate's fields, once per term
                        let fields: Box<[TermId]> =
                            operands.iter().map(|it| self.operand(state, it)).collect();
                        self.term(Term::Agg(variant, fields), ANY)
                    }
                    None => self.fresh(None),
                }
            }
            _ => {
                let ty = self.place_ty(to);
                self.fresh(Some(ty))
            }
        }
    }

    /// Walks block `block` from `state`, its entry state, to its end.
    fn transfer(&mut self, block: u32, state: &mut State) -> Out {
        let data = self.blocks[block as usize];
        for (stmt, statement) in data.statements.iter().enumerate() {
            self.at = (block, stmt as u32);
            self.made = 0;
            self.kill_point(state, self.at);
            self.span = statement.span;
            if let StatementKind::Assign(to, rvalue) = &statement.kind {
                let value = self.rvalue(state, to, rvalue);
                self.write(state, to, value);
            }
        }
        self.at = (block, data.statements.len() as u32);
        self.made = 0;
        self.kill_point(state, self.at);
        let mut out = Out { edges: Vec::new(), safe: false, goal: None }; // alloc: the block's edges, few
        let Some(terminator) = &data.terminator else { return out };
        // clone: each edge leaves with its own state
        match &terminator.kind {
            TerminatorKind::Goto { target } => out.edges.push((index(*target) as u32, state.clone())),
            TerminatorKind::FalseEdge { real_target, imaginary_target } => {
                out.edges.push((index(*real_target) as u32, state.clone()));
                out.edges.push((index(*imaginary_target) as u32, state.clone()));
            }
            TerminatorKind::FalseUnwind { real_target, unwind } => {
                out.edges.push((index(*real_target) as u32, state.clone()));
                if let Some(unwind) = unwind {
                    out.edges.push((index(*unwind) as u32, state.clone()));
                }
            }
            TerminatorKind::SwitchInt { discr, targets } => {
                let value = self.operand(state, discr);
                let ty = self.operand_ty(discr);
                let mut rest = state.clone();
                let mut rest_runs = true;
                for (bits, target) in targets.iter() {
                    let case = ty.and_then(|ty| value_of_bits(bits, ty, self.word)).map(|it| self.constant(it));
                    let Some(case) = case else {
                        // A value this cannot read: the edge is taken as it is.
                        out.edges.push((index(target) as u32, state.clone()));
                        continue;
                    };
                    let mut taken = state.clone();
                    if self.assume(&mut taken, Atom::new(Rel::Eq, value, case)) {
                        out.edges.push((index(target) as u32, taken));
                    }
                    rest_runs = rest_runs && self.assume(&mut rest, Atom::new(Rel::Ne, value, case));
                }
                if rest_runs {
                    out.edges.push((index(targets.otherwise()) as u32, rest));
                }
            }
            // An overflow check proves nothing of an overflow proof: the operation it guards
            // made its own verdict and took itself to fit, which a crate built without the
            // check must agree with.
            TerminatorKind::Assert { msg, target, .. }
                if self.mode == Mode::Overflow && msg.is_optional_overflow_check() =>
            {
                out.safe = true;
                out.edges.push((index(*target) as u32, state.clone()));
            }
            TerminatorKind::Assert { cond, expected, target, cleanup, .. } => {
                let value = self.operand(state, cond);
                let want = self.constant(i128::from(*expected));
                let goal = Atom::new(Rel::Eq, value, want);
                out.safe = self.entails(state, goal);
                if !out.safe {
                    out.goal = Some(goal);
                }
                let mut passed = state.clone();
                if self.assume(&mut passed, goal) {
                    out.edges.push((index(*target) as u32, passed));
                }
                if let Some(cleanup) = cleanup
                    && !out.safe
                {
                    out.edges.push((index(*cleanup) as u32, state.clone()));
                }
            }
            TerminatorKind::Call { func, args, destination, target, cleanup, .. } => {
                let model = self.model(func);
                let applied = model.and_then(|model| self.apply(state, model, args, destination));
                match applied {
                    Some(call) => {
                        // Safe means its stated condition is shown: a model that states
                        // none (`len`) says nothing of whether its function can panic.
                        out.safe = call.requires.is_some_and(|goal| self.entails(state, goal));
                        if !out.safe {
                            out.goal = call.requires;
                        }
                        if let Some(cleanup) = cleanup
                            && !out.safe
                        {
                            out.edges.push((index(*cleanup) as u32, state.clone()));
                        }
                        // Having returned, the call did not panic: its condition held.
                        let mut returned = state.clone();
                        let mut runs = true;
                        if let Some(goal) = call.requires {
                            runs = self.assume(&mut returned, goal);
                        }
                        if let Some((local, value)) = call.sets {
                            returned.vals[local as usize] = value;
                        }
                        self.write(&mut returned, destination, call.result);
                        for (premise, then) in call.implies {
                            returned.put_imp((premise, then));
                        }
                        runs = runs && self.saturate(&mut returned);
                        for fact in call.facts {
                            runs = runs && self.assume(&mut returned, fact);
                        }
                        if runs && let Some(target) = target {
                            out.edges.push((index(*target) as u32, returned));
                        }
                    }
                    None => {
                        self.bump(state);
                        if let Some(cleanup) = cleanup {
                            out.edges.push((index(*cleanup) as u32, state.clone()));
                        }
                        if let Some(target) = target {
                            let ty = self.place_ty(destination);
                            let result = self.fresh(Some(ty));
                            let mut returned = state.clone();
                            self.write(&mut returned, destination, result);
                            out.edges.push((index(*target) as u32, returned));
                        }
                    }
                }
            }
            TerminatorKind::Drop { target, unwind, .. } => {
                self.bump(state);
                out.edges.push((index(*target) as u32, state.clone()));
                if let Some(unwind) = unwind {
                    out.edges.push((index(*unwind) as u32, state.clone()));
                }
            }
            TerminatorKind::DropAndReplace { place, value, target, unwind } => {
                let value = self.operand(state, value);
                self.bump(state);
                if let Some(unwind) = unwind {
                    out.edges.push((index(*unwind) as u32, state.clone()));
                }
                self.write(state, place, value);
                out.edges.push((index(*target) as u32, state.clone()));
            }
            TerminatorKind::Return
            | TerminatorKind::Unreachable
            | TerminatorKind::UnwindResume
            | TerminatorKind::Abort
            | TerminatorKind::TailCall { .. }
            | TerminatorKind::Yield { .. }
            | TerminatorKind::CoroutineDrop
            | TerminatorKind::ComptimeFor { .. }
            | TerminatorKind::ComptimeNext { .. } => {}
        }
        out
    }

    fn model(&mut self, func: &Operand) -> Option<Model> {
        let (func, args) = func.fn_item()?;
        // A trait's method is modeled by its `Self`, which only the call's arguments name.
        if matches!(
            hir_def::Lookup::lookup(&func, self.db).container,
            hir_def::ItemContainerId::TraitId(_)
        ) {
            return models::model_of(self.db, func, args);
        }
        if let Some(&known) = self.models.get(&func) {
            return known;
        }
        let model = models::model_of(self.db, func, args);
        self.models.insert(func, model);
        model
    }

    // Where paths meet.

    /// The entry state of `block` from the states along the edges into it, or none when no
    /// edge is feasible. `prior` is its entry state the last time, which the joined slots'
    /// bounds are widened against.
    fn join(&mut self, block: u32, incoming: &mut [State], prior: Option<&State>) -> Option<State> {
        let first = incoming.first()?;
        let slots = first.vals.len();
        let before = self.joined[block as usize].len();
        // A slot whose values differ is joined; so is one that holds an unknown this join
        // makes again, until no more are found (each turn adds a slot).
        loop {
            let mut grew = false;
            for slot in 0..slots {
                let whole = slot as u32;
                if self.joined[block as usize].contains(&whole) {
                    continue;
                }
                let value = incoming[0].vals[slot];
                let differ = incoming.iter().any(|it| it.vals[slot] != value);
                let joined = &self.joined[block as usize];
                let stale = |key: Key| {
                    matches!(key, Key::Join(at, of) if at == block && joined.contains(&of))
                };
                let holds_stale = incoming.iter().any(|it| self.mentions(it.vals[slot], &stale));
                if !differ && !holds_stale {
                    continue;
                }
                // Aggregates of one shape are joined field by field, so a field every path
                // agrees on (a range's end) is kept.
                let fields = self.same_shape(incoming, slot).filter(|_| slot < FIELD_LOCALS);
                match fields {
                    Some(count) if !holds_stale => {
                        for field in 0..count {
                            let key = FIELD + whole * 64 + field as u32;
                            if self.joined[block as usize].contains(&key) {
                                continue;
                            }
                            let of = |state: &State| match &self.terms[state.vals[slot] as usize] {
                                Term::Agg(_, fields) => fields[field],
                                _ => unreachable!("`same_shape` saw an aggregate"),
                            };
                            let value = of(&incoming[0]);
                            if incoming.iter().any(|it| of(it) != value) {
                                self.joined[block as usize].push(key);
                                grew = true;
                            }
                        }
                    }
                    _ => {
                        self.joined[block as usize].push(whole);
                        grew = true;
                    }
                }
            }
            if !grew {
                break;
            }
        }
        let joined = self.joined[block as usize].clone(); // clone: read while states change
        let made = |key: Key| matches!(key, Key::Join(at, of) if at == block && joined.contains(&of));
        // What every path holds of a joined integer against the values the paths bring for
        // it (a loop's `hi <= len`: `len` on the way in, `mid` on the way round): `j <= y`
        // and `y <= j` for each such value `y` that this join does not make again, kept when
        // every path's own value satisfies it in that path's state. Asked before the states
        // forget what this join makes again: a path round a loop proves it from what held of
        // the last turn's values.
        let mut invariants = Vec::new(); // alloc: the joined slots' relations, few
        for slot in 0..slots {
            let whole = slot as u32;
            let integer = self
                .local_ty(slot)
                .is_some_and(|ty| !ty.is_bool() && int_range(ty, self.word).is_some());
            if !integer || !joined.contains(&whole) {
                continue;
            }
            for at in 0..incoming.len().min(4) {
                let anchor = incoming[at].vals[slot];
                let repeated = incoming[..at].iter().any(|it| it.vals[slot] == anchor);
                if repeated || self.mentions(anchor, &made) || self.const_of(anchor).is_some() {
                    continue;
                }
                for below in [true, false] {
                    let holds = incoming.iter().all(|it| {
                        let value = it.vals[slot];
                        let atom = match below {
                            true => Atom::new(Rel::Le, value, anchor),
                            false => Atom::new(Rel::Le, anchor, value),
                        };
                        self.entails(it, atom)
                    });
                    if holds {
                        invariants.push((whole, anchor, below));
                    }
                }
            }
        }
        // What this join makes again is forgotten in each state first.
        for state in incoming.iter_mut() {
            state.facts.retain(|fact| !self.mentions(fact.a, &made) && !self.mentions(fact.b, &made));
            state.imps.retain(|(premise, then)| {
                ![premise.a, premise.b, then.a, then.b].iter().any(|&it| self.mentions(it, &made))
            });
        }
        let mut state = State {
            vals: incoming[0].vals.clone(), // clone: the joined state starts from the first
            facts: incoming[0].facts.clone(),
            imps: incoming[0].imps.clone(),
        };
        for other in incoming.iter().skip(1) {
            state.facts.retain(|fact| other.has(*fact));
            state.imps.retain(|imp| other.imps.binary_search(imp).is_ok());
        }
        // The joined slots' values, with the bounds every path gives them.
        let mut bounds = Vec::new(); // alloc: the joined slots' bounds, few
        for slot in 0..slots {
            let whole = slot as u32;
            let ty = self.local_ty(slot);
            if joined.contains(&whole) {
                let value = self.unknown(Key::Join(block, whole), ty);
                let (mut low, mut high) = (i128::MAX, i128::MIN);
                for it in incoming.iter() {
                    let (least, most) = self.bounds(it, it.vals[slot]);
                    (low, high) = (low.min(least), high.max(most));
                }
                bounds.push((value, low, high));
                state.vals[slot] = value;
                continue;
            }
            let low_key = FIELD.wrapping_add(whole.wrapping_mul(64));
            let any = slot < FIELD_LOCALS
                && joined.iter().any(|&it| it >= low_key && it - low_key < 64);
            if !any {
                continue;
            }
            let (variant, mut fields) = match &self.terms[state.vals[slot] as usize] {
                Term::Agg(variant, fields) => (*variant, fields.clone()), // clone: rebuilt with its joined fields
                _ => continue,
            };
            let mut changed = false;
            for field in 0..fields.len().min(64) {
                let key = FIELD + whole * 64 + field as u32;
                if slot < FIELD_LOCALS && joined.contains(&key) {
                    fields[field] = self.unknown(Key::Join(block, key), None);
                    changed = true;
                }
            }
            if changed {
                state.vals[slot] = self.term(Term::Agg(variant, fields), ANY);
            }
        }
        for (slot, anchor, below) in invariants {
            let value = state.vals[slot as usize];
            state.put(match below {
                true => Atom::new(Rel::Le, value, anchor),
                false => Atom::new(Rel::Le, anchor, value),
            });
        }
        // Widening: once the joined slots stop growing, a bound is kept only if the entry
        // state had it the last time, so bounds that move each turn (a counter's) are dropped
        // and the walk settles. Every other fact only shrinks from turn to turn by itself: the
        // first turn is the most hopeful and each step is monotone.
        let widen = prior.filter(|_| self.joined[block as usize].len() == before);
        for (value, low, high) in bounds {
            let (least, most) = self.ranges[value as usize];
            if low > least {
                let low = self.constant(low);
                let fact = Atom::new(Rel::Le, low, value);
                if widen.is_none_or(|it| it.has(fact)) {
                    state.put(fact);
                }
            }
            if high < most {
                let high = self.constant(high);
                let fact = Atom::new(Rel::Le, value, high);
                if widen.is_none_or(|it| it.has(fact)) {
                    state.put(fact);
                }
            }
        }
        // A joined `bool` keeps what each of its values meant on the paths that gave it (`a &&
        // b` is lowered to a temporary both arms write): what every path that can give the
        // value holds, once its own value is taken to be that, is implied by the value.
        for slot in 0..slots {
            let is_bool = self.local_ty(slot).is_some_and(|ty| ty.is_bool());
            if !is_bool || !joined.contains(&(slot as u32)) {
                continue;
            }
            let value = state.vals[slot];
            for case in [0, 1] {
                let case = self.constant(case);
                let mut common: Option<Vec<Atom>> = None;
                for it in incoming.iter() {
                    let mut given = it.clone(); // clone: the path with the value assumed
                    if !self.assume(&mut given, Atom::new(Rel::Eq, it.vals[slot], case)) {
                        continue;
                    }
                    common = Some(match common {
                        None => given.facts,
                        Some(mut have) => {
                            have.retain(|fact| given.has(*fact));
                            have
                        }
                    });
                }
                for fact in common.unwrap_or_default() {
                    let stale = self.mentions(fact.a, &made) || self.mentions(fact.b, &made);
                    if !stale && !state.has(fact) {
                        state.put_imp((Atom::new(Rel::Eq, value, case), fact));
                    }
                }
            }
        }
        Some(state)
    }

    /// How many fields the aggregates all of `incoming` hold in `slot` have, when they are
    /// aggregates of one variant and length.
    fn same_shape(&self, incoming: &[State], slot: usize) -> Option<usize> {
        let Term::Agg(variant, fields) = &self.terms[incoming[0].vals[slot] as usize] else {
            return None;
        };
        let same = incoming.iter().all(|it| {
            matches!(&self.terms[it.vals[slot] as usize],
                Term::Agg(other, others) if other == variant && others.len() == fields.len())
        });
        (same && fields.len() <= 64).then_some(fields.len())
    }

    /// The entry state of every block at the fixed point, none for a block no feasible path
    /// reaches; or none when the walk did not settle within its budget.
    fn solve(&mut self) -> Option<Vec<Option<State>>> {
        let count = self.blocks.len();
        let start = index(self.body.start_block);
        let mut entries: Vec<Option<State>> = vec![None; count]; // alloc: one state per block, the proof
        // alloc: the feasible edges out of each block, once per proof
        let mut edges: Vec<Vec<(u32, State)>> = vec![Vec::new(); count];
        let mut grown = vec![0usize; count]; // alloc: once per proof
        let mut queued = vec![false; count]; // alloc: once per proof
        let mut queue = std::collections::VecDeque::new(); // alloc: the worklist, once per proof
        let entry = self.entry();
        queue.push_back(start);
        queued[start] = true;
        let mut budget = count * 8 + 64;
        while let Some(block) = queue.pop_front() {
            queued[block] = false;
            if budget == 0 {
                return None;
            }
            budget -= 1;
            // alloc: the states into this block, a few
            let mut incoming: Vec<State> = Vec::new();
            if block == start {
                incoming.push(entry.clone()); // clone: the entry state joins the back edges
            }
            for &from in &self.predecessors[block] {
                for (to, state) in &edges[from as usize] {
                    if *to as usize == block {
                        incoming.push(state.clone()); // clone: joined in place
                    }
                }
            }
            let prior = entries[block].take();
            let settled = grown[block] == self.joined[block].len();
            let joined = self.join(block as u32, &mut incoming, prior.as_ref().filter(|_| settled));
            grown[block] = self.joined[block].len();
            if joined == prior && (prior.is_some() || !edges[block].is_empty()) {
                entries[block] = prior;
                continue;
            }
            entries[block] = joined.clone(); // clone: kept as the block's entry, walked below
            let out = match joined {
                Some(mut state) => self.transfer(block as u32, &mut state).edges,
                None => Vec::new(),
            };
            if self.terms.len() > 1 << 16 {
                return None;
            }
            let changed = out != edges[block];
            // alloc: the blocks this one now goes to and went to before
            let mut targets: Vec<u32> = out.iter().map(|it| it.0).collect();
            targets.extend(edges[block].iter().map(|it| it.0));
            edges[block] = out;
            if changed {
                for target in targets {
                    if !queued[target as usize] {
                        queued[target as usize] = true;
                        queue.push_back(target as usize);
                    }
                }
            }
        }
        Some(entries)
    }

    // Messages.

    fn local_name(&self, slot: u32) -> Option<String> {
        let (binding, _) = self
            .body
            .binding_locals
            .iter()
            .find(|(_, local)| local_index(**local) as u32 == slot)?;
        let body = hir_def::expr_store::Body::of(self.db, self.owner()?);
        Some(body[binding].name.as_str().to_owned()) // alloc: a message's part
    }

    fn owner(&self) -> Option<DefWithBodyId> {
        match self.body.owner {
            InferBodyId::DefWithBodyId(def) => Some(def),
            _ => None,
        }
    }

    /// `term` as source would say it, when it has a name.
    fn said(&self, term: TermId) -> Option<String> {
        if let Some(&slot) = self.names.get(&term)
            && let Some(name) = self.local_name(slot)
        {
            return Some(name);
        }
        Some(match &self.terms[term as usize] {
            Term::Const(value) => value.to_string(), // alloc: a message's part
            Term::Unknown(Key::Entry(slot) | Key::Join(_, slot)) => self.local_name(*slot)?,
            Term::Unknown(Key::At(..)) => return None,
            Term::Len(of) => format!("{}.len()", self.said(*of)?), // alloc: a message's part
            Term::Deref(of) | Term::DerefAt(of, _) | Term::RefTo(of) | Term::MutRef(_, of, _) => {
                self.said(*of)?
            }
            Term::Field(of, field) => format!("{}.{field}", self.said(*of)?), // alloc: a message's part
            Term::Discr(of) => format!("the variant of {}", self.said(*of)?), // alloc: a message's part
            Term::Cmp(rel, a, b) => self.said_atom(Atom { rel: *rel, a: *a, b: *b })?,
            Term::Not(of) => format!("!{}", self.said(*of)?), // alloc: a message's part
            Term::Lin(parts, constant) => {
                let mut text = String::new(); // alloc: a message's part
                for &(part, coefficient) in parts.iter() {
                    let name = self.said(part)?;
                    let sign = if coefficient < 0 { "-" } else { "+" };
                    if !text.is_empty() || coefficient < 0 {
                        text.push_str(if text.is_empty() { "-" } else { sign });
                        if text.len() > 1 {
                            text.insert(text.len() - 1, ' ');
                            text.push(' ');
                        }
                    }
                    if coefficient.unsigned_abs() != 1 {
                        text.push_str(&format!("{} * ", coefficient.unsigned_abs())); // alloc: a message's part
                    }
                    text.push_str(&name);
                }
                if *constant != 0 {
                    let sign = if *constant < 0 { "-" } else { "+" };
                    text.push_str(&format!(" {sign} {}", constant.unsigned_abs())); // alloc: a message's part
                }
                text
            }
            Term::Wrap(of, _) => self.said(*of)?,
            Term::Payload(..) | Term::Agg(..) => return None,
        })
    }

    /// `atom` as source would say it: "`i < v.len()`" without the quotes.
    fn said_atom(&self, atom: Atom) -> Option<String> {
        // A comparison's value compared with 0 or 1 is the comparison.
        for (term, other) in [(atom.a, atom.b), (atom.b, atom.a)] {
            if let (Term::Cmp(rel, a, b), Some(value)) =
                (&self.terms[term as usize], self.const_of(other))
                && matches!(atom.rel, Rel::Eq | Rel::Ne)
            {
                let inner = Atom { rel: *rel, a: *a, b: *b };
                let truth = (atom.rel == Rel::Eq) == (value == 1);
                return self.said_atom(if truth { inner } else { inner.not() });
            }
        }
        // A discriminant compared with a variant's value is the value being that variant.
        for (term, other) in [(atom.a, atom.b), (atom.b, atom.a)] {
            if let (Term::Discr(of), Some(value)) =
                (&self.terms[term as usize], self.const_of(other))
                && matches!(atom.rel, Rel::Eq | Rel::Ne)
                && let Some(enum_) = self.enums.get(&term)
            {
                let variant = enum_.enum_variants(self.db).variants.iter().find(|(_, it)| {
                    self.db.const_eval_discriminant(it.0).ok() == Some(value)
                });
                let (name, _) = variant?;
                let is = if atom.rel == Rel::Eq { "is" } else { "is not" };
                // alloc: a message's part
                return Some(format!("{} {is} {}", self.said(*of)?, name.as_str()));
            }
        }
        let sign = match atom.rel {
            Rel::Lt => "<",
            Rel::Le => "<=",
            Rel::Eq => "==",
            Rel::Ne => "!=",
        };
        // alloc: a message's part
        Some(format!("{} {sign} {}", self.said(atom.a)?, self.said(atom.b)?))
    }

    /// Why block `block` is reached, or why its site is not safe: what would have to be shown,
    /// and that nothing before it shows it.
    fn why_reached(&mut self, block: u32, entries: &[Option<State>]) -> String {
        // The site's own condition, when it has one.
        if let Some(entry) = &entries[block as usize] {
            let mut state = entry.clone(); // clone: walked to the block's end for its goal
            let out = self.transfer(block, &mut state);
            if let Some(goal) = out.goal
                && let Some(said) = self.said_atom(goal)
            {
                // alloc: a message
                return format!("can't show `{said}` here: no check dominates this site");
            }
        }
        // Else the last branch on the way here: back through blocks with one way in.
        let mut at = block;
        for _ in 0..self.blocks.len() {
            let [from] = self.predecessors[at as usize][..] else { break };
            let Some(entry) = &entries[from as usize] else { break };
            let kind = self.blocks[from as usize].terminator.as_ref().map(|it| &it.kind);
            if let Some(TerminatorKind::SwitchInt { discr, targets }) = kind {
                let mut state = entry.clone(); // clone: walked to the branch
                let _ = self.transfer(from, &mut state);
                let value = self.operand(&state, discr);
                let ty = self.operand_ty(discr);
                let taken: Vec<i128> = targets
                    .iter()
                    .filter(|&(_, target)| index(target) as u32 == at)
                    .filter_map(|(bits, _)| ty.and_then(|ty| value_of_bits(bits, ty, self.word)))
                    .collect(); // alloc: the values that lead here, one or two
                let otherwise = index(targets.otherwise()) as u32 == at;
                let listed: Vec<i128> = targets
                    .iter()
                    .filter_map(|(bits, _)| ty.and_then(|ty| value_of_bits(bits, ty, self.word)))
                    .collect(); // alloc: as above
                let atom = match (&taken[..], otherwise, &listed[..]) {
                    ([case], false, _) => {
                        let case = self.constant(*case);
                        Some(Atom::new(Rel::Eq, value, case))
                    }
                    ([], true, [case]) => {
                        let case = self.constant(*case);
                        Some(Atom::new(Rel::Ne, value, case))
                    }
                    _ => None,
                };
                if let Some(said) = atom.and_then(|it| self.said_atom(it.not())) {
                    // alloc: a message
                    return format!(
                        "can't show `{said}` here: no check before this branch rules the path out"
                    );
                }
                break;
            }
            at = from;
        }
        "nothing before it rules out the path to it".to_owned() // alloc: a message
    }
}

/// The locals whose aggregates are joined field by field: a join's slot number has room for
/// these.
const FIELD_LOCALS: usize = (FIELD / 64) as usize;

/// The values an integer, `bool` or `char` type holds; none for any other type. `usize` and
/// `isize` are `word` bits wide, the target's.
fn int_range(ty: Ty<'_>, word: u32) -> Option<(i128, i128)> {
    let bits = |bits: u32, signed: bool| -> (i128, i128) {
        match (bits, signed) {
            (128, false) => (0, i128::MAX),
            (128, true) => (i128::MIN, i128::MAX),
            (bits, false) => (0, (1i128 << bits) - 1),
            (bits, true) => (-(1i128 << (bits - 1)), (1i128 << (bits - 1)) - 1),
        }
    };
    Some(match ty.kind() {
        TyKind::Bool => (0, 1),
        TyKind::Char => (0, 0x10_FFFF),
        TyKind::Uint(it) => bits(uint_bits(it, word), false),
        TyKind::Int(it) => bits(int_bits(it, word), true),
        _ => return None,
    })
}

fn uint_bits(ty: UintTy, word: u32) -> u32 {
    ty.bit_width().map_or(word, |it| it as u32)
}

fn int_bits(ty: IntTy, word: u32) -> u32 {
    ty.bit_width().map_or(word, |it| it as u32)
}

/// Whether `ty` is `u128`, whose values past `i128::MAX` the prover's integers do not hold.
fn wide(ty: Ty<'_>) -> bool {
    matches!(ty.kind(), TyKind::Uint(UintTy::U128))
}

/// How many bits an integer type has; 0 for any other type.
fn type_bits(ty: Ty<'_>, word: u32) -> u32 {
    match ty.kind() {
        TyKind::Uint(it) => uint_bits(it, word),
        TyKind::Int(it) => int_bits(it, word),
        _ => 0,
    }
}

/// The value of `bits` read as a `ty`: sign-extended for a signed integer. None for a type
/// that is not an integer, `bool` or `char`, and for a `u128` past `i128::MAX`.
fn value_of_bits(bits: u128, ty: Ty<'_>, word: u32) -> Option<i128> {
    match ty.kind() {
        TyKind::Bool | TyKind::Char | TyKind::Uint(_) => i128::try_from(bits).ok(),
        TyKind::Int(it) => {
            let width = int_bits(it, word);
            if width == 128 {
                return Some(bits as i128);
            }
            let shift = 128 - width;
            Some(((bits << shift) as i128) >> shift)
        }
        _ => None,
    }
}

/// The value of the constant `operand` of type `ty`.
fn constant_value(operand: &Operand, ty: Ty<'_>, word: u32) -> Option<i128> {
    value_of_bits(constant_bits(operand)?, ty, word)
}

/// What a modeled call does to the state.
pub(crate) struct Applied {
    pub(crate) result: TermId,
    /// What must hold for the call not to panic.
    pub(crate) requires: Option<Atom>,
    /// What holds once it has returned.
    pub(crate) facts: Vec<Atom>,
    pub(crate) implies: Vec<(Atom, Atom)>,
    /// A local the call writes, and its value after.
    pub(crate) sets: Option<(u32, TermId)>,
}

impl<'a, 'db> Solver<'a, 'db> {
    /// What the call of the modeled function does, when its arguments have the model's shape.
    fn apply(
        &mut self,
        state: &State,
        model: Model,
        args: &[Operand],
        destination: &StoredPlace,
    ) -> Option<Applied> {
        let first = args.first()?;
        let first_ty = self.operand_ty(first)?;
        let receiver = self.operand(state, first);
        let result_ty = self.place_ty(destination);
        let mut applied = Applied {
            result: receiver,
            requires: None,
            facts: Vec::new(),   // alloc: a model's facts, at most two
            implies: Vec::new(), // alloc: a model's implications, at most three
            sets: None,
        };
        let zero = self.constant(0);
        let one = self.constant(1);
        // What the receiver leads to, for the methods that take `&self` or `&mut self`.
        let pointee = |this: &mut Self| match first_ty.kind() {
            TyKind::Ref(..) => Some(this.deref(state, receiver, first_ty)),
            _ => None,
        };
        let usize_arg = |this: &mut Self| -> Option<TermId> {
            let index = args.get(1)?;
            let ty = this.operand_ty(index)?;
            matches!(ty.kind(), TyKind::Uint(UintTy::Usize)).then(|| this.operand(state, index))
        };
        match model {
            Model::Len => {
                let of = pointee(self)?;
                applied.result = self.len(of);
            }
            Model::IsEmpty => {
                let of = pointee(self)?;
                let len = self.len(of);
                applied.result = self.cmp(Rel::Eq, len, zero);
            }
            Model::IsVariant { discr, is } => {
                let of = pointee(self)?;
                let inner = first_ty.builtin_deref(true);
                let tag = self.discr(of, inner);
                let want = self.constant(discr);
                applied.result = self.cmp(if is { Rel::Eq } else { Rel::Ne }, tag, want);
            }
            Model::Unwrap { discr } => {
                let tag = self.discr(receiver, Some(first_ty));
                let want = self.constant(discr);
                applied.requires = Some(Atom::new(Rel::Eq, tag, want));
                applied.result = match models::variant_at(self.db, first_ty, discr) {
                    Some(variant) => {
                        let variant = self.variant(variant);
                        self.payload(receiver, variant, 0)
                    }
                    None => self.fresh(Some(result_ty)),
                };
            }
            Model::Index => {
                let of = pointee(self)?;
                let at = usize_arg(self)?;
                let len = self.len(of);
                applied.requires = Some(Atom::new(Rel::Lt, at, len));
                applied.result = self.fresh(None);
            }
            Model::First => {
                let of = pointee(self)?;
                let len = self.len(of);
                applied.result = self.fresh(None);
                let tag = self.discr(applied.result, Some(result_ty));
                let some = Atom::new(Rel::Eq, tag, one);
                let none = Atom::new(Rel::Eq, tag, zero);
                let filled = Atom::new(Rel::Lt, zero, len);
                applied.implies.push((filled, some));
                applied.implies.push((some, filled));
                applied.implies.push((Atom::new(Rel::Eq, len, zero), none));
                #[cfg(test)]
                if models::PLANT_FIRST_ALWAYS_SOME.get() {
                    applied.facts.push(some);
                }
            }
            Model::Get => {
                let of = pointee(self)?;
                let at = usize_arg(self)?;
                let len = self.len(of);
                applied.result = self.fresh(None);
                let tag = self.discr(applied.result, Some(result_ty));
                let some = Atom::new(Rel::Eq, tag, one);
                let none = Atom::new(Rel::Eq, tag, zero);
                let inside = Atom::new(Rel::Lt, at, len);
                applied.implies.push((inside, some));
                applied.implies.push((some, inside));
                applied.implies.push((inside.not(), none));
            }
            Model::NonZeroGet => {
                applied.result = self.field(receiver, 0);
                self.typed(applied.result, result_ty);
                applied.facts.push(Atom::new(Rel::Ne, applied.result, zero));
            }
            Model::RangeNext => {
                // `next(&mut range)`: the range is a local this state knows, borrowed whole.
                let Term::MutRef(local, value, epoch) = self.terms[receiver as usize] else {
                    return None;
                };
                if epoch != state.vals[self.epoch_slot()] || state.vals[local as usize] != value {
                    return None;
                }
                let (start, end) = (self.field(value, 0), self.field(value, 1));
                applied.result = self.fresh(None);
                let tag = self.discr(applied.result, Some(result_ty));
                let some = Atom::new(Rel::Eq, tag, one);
                let variant = models::variant_at(self.db, result_ty, 1)?;
                let variant = self.variant(variant);
                let item = self.payload(applied.result, variant, 0);
                applied.implies.push((some, Atom::new(Rel::Lt, item, end)));
                applied.implies.push((some, Atom::new(Rel::Le, start, item)));
                // The range after: a start this does not follow, the same end.
                let next = self.fresh(None);
                let after = self.term(Term::Agg(NO_VARIANT, Box::new([next, end])), ANY);
                applied.sets = Some((local, after));
            }
            Model::Itself => {}
            Model::View => {
                // `Vec`'s slice, `String`'s `str`: the same length, so the same value here.
                if !matches!(
                    self.terms[receiver as usize],
                    Term::RefTo(_) | Term::MutRef(..) | Term::Unknown(_)
                ) {
                    return None;
                }
                let of = pointee(self)?;
                applied.result = match self.terms[receiver as usize] {
                    Term::MutRef(..) => receiver,
                    _ => self.term(Term::RefTo(of), ANY),
                };
            }
        }
        Some(applied)
    }
}
