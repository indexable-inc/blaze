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
}

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
    for block in &blocks {
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
            return Reach::refused("it has inline assembly, a `yield` or a `for const` loop");
        }
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
            variants: Vec::new(), // alloc: the variants the body names, few
            models: HashMap::new_in(Global),
            exposed: Vec::new(),
            cyclic,
            predecessors,
            joined: vec![Vec::new(); count], // alloc: one list per block, once per proof
            word,
            at: (0, 0),
            made: 0,
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
            let had = self.ranges[id as usize];
            self.ranges[id as usize] = (had.0.max(range.0), had.1.min(range.1));
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
        self.term(Term::Discr(of), range)
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
        let atom = Atom::new(rel, a, b);
        self.term(Term::Cmp(atom.rel, atom.a, atom.b), (0, 1))
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
        if self.entails(state, atom) {
            return true;
        }
        state.put(atom);
        for term in [atom.a, atom.b] {
            let (low, high) = self.bounds(state, term);
            if low > high {
                return false;
            }
        }
        // What the new fact is the premise of: each implication fires at most once.
        let mut budget = state.imps.len();
        loop {
            let next = state
                .imps
                .iter()
                .find(|&&(premise, then)| {
                    self.entails(state, premise) && !self.entails(state, then)
                })
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

    fn rvalue(&mut self, state: &State, to: &StoredPlace, rvalue: &Rvalue) -> TermId {
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
                // A cast that cannot change the value is the value.
                let value = self.operand(state, operand);
                let (low, high) = self.bounds(state, value);
                match int_range(ty.as_ref(), self.word) {
                    Some((least, most)) if least <= low && high <= most => value,
                    _ => self.fresh(Some(ty.as_ref())),
                }
            }
            Rvalue::UnaryOp(UnOp::Not, operand)
                if self.operand_ty(operand).is_some_and(|it| it.is_bool()) =>
            {
                let value = self.operand(state, operand);
                self.not(value)
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
            // `(a op b, overflowed)`: when the exact result fits the type whatever the operands
            // are within their bounds, the flag is false and the check after it holds.
            Rvalue::BinaryOp(
                op @ (BinOp::AddWithOverflow | BinOp::SubWithOverflow | BinOp::MulWithOverflow),
                left,
                right,
            ) => {
                let range = self.operand_ty(left).and_then(|it| int_range(it, self.word));
                let (a, b) = (self.operand(state, left), self.operand(state, right));
                let ((low_a, high_a), (low_b, high_b)) = (self.bounds(state, a), self.bounds(state, b));
                let ends = match op {
                    BinOp::AddWithOverflow => {
                        low_a.checked_add(low_b).zip(high_a.checked_add(high_b))
                    }
                    BinOp::SubWithOverflow => {
                        low_a.checked_sub(high_b).zip(high_a.checked_sub(low_b))
                    }
                    _ => {
                        let products = [
                            low_a.checked_mul(low_b),
                            low_a.checked_mul(high_b),
                            high_a.checked_mul(low_b),
                            high_a.checked_mul(high_b),
                        ];
                        match products {
                            [Some(a), Some(b), Some(c), Some(d)] => {
                                Some((a.min(b).min(c).min(d), a.max(b).max(c).max(d)))
                            }
                            _ => None,
                        }
                    }
                };
                match (ends, range) {
                    (Some((low, high)), Some((least, most))) if least <= low && high <= most => {
                        // The result is left unknown: a term's range is its type's, never a
                        // state's.
                        let ty = self.operand_ty(left);
                        let value = self.fresh(ty);
                        let flag = self.constant(0);
                        self.term(Term::Agg(NO_VARIANT, Box::new([value, flag])), ANY)
                    }
                    _ => self.fresh(None),
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
                        out.safe = call.requires.is_none_or(|goal| self.entails(state, goal));
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
        // What this join makes again is forgotten in each state first.
        let joined = self.joined[block as usize].clone(); // clone: read while states change
        let made = |key: Key| matches!(key, Key::Join(at, of) if at == block && joined.contains(&of));
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
