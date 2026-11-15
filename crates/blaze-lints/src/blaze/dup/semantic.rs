//! `same_behaviour`: two functions that compile to one behaviour though they are written
//! differently: a helper an agent wrote again in another style under another name. The syntax
//! lints ([`super::index`]) see copies up to the names of their locals; this one sees through
//! the style, by comparing a canonical form of each function's runtime MIR.
//!
//! The canonical form ([`canonical`]), the contract `docs/lints.md#same_behaviour` states:
//!
//! - lowering ([`lower`]) drops storage markers, nops, fake reads, spans and names; reads by
//!   copy and by move are one read; a constant is its value and type, a callee its stable path
//!   with its generic arguments (never an id), a type its printed form with the path of every
//!   ADT in it;
//! - [`thread`] sends every jump past blocks that only jump, and a jump to a block that only
//!   ends the function ends it there;
//! - [`number`] drops unreachable blocks, numbers blocks in reverse post-order with successors
//!   in operand order (a switch's targets by value, the otherwise target last), and locals by
//!   first appearance (the return place 0, parameters at their positions);
//! - [`transform`] folds comparisons of a `bool` with a constant, copy-propagates temporaries
//!   used once, drops stores nothing reads, folds a switch on a negation into a switch with its
//!   targets swapped, simplifies switches (one normal form for `bool`, a switch with one live
//!   target a jump), and sorts the operands of commutative operations;
//! - the steps repeat to a fixpoint within [`CANON_ROUNDS`], and the form is hashed with a
//!   domain string and [`CANON_VERSION`] ([`fingerprint`]).
//!
//! The pass ([`semantic_index`]) groups a crate's functions and its local dependencies' by that
//! hash, and a function is reported when an earlier one (a dependency's first) has its hash and
//! a different syntax shape: an equal shape is `same_fn`'s finding. Where the form cannot be
//! made (inline assembly, closures, coroutines, a constant it cannot read), nothing is said.

use std::{
    alloc::Global,
    fmt::{self, Write as _},
    hash::{Hash, Hasher},
};

use base_db::{Db, EditionedFileId};
use blaze_hash::{FixedHasher, HashMap};
use blaze_items::ItemId;
use hir::{HasVisibility as _, HirDisplay as _};
use hir_def::{
    AdtId, CallableDefId, DefWithBodyId, HasModule as _, ItemContainerId, Lookup as _, VariantId,
    expr_store::body::body_syntax,
};
use hir_ty::{
    InferBodyId, MemoryMap,
    db::HirDatabase as _,
    display::DisplayTarget,
    mir::{
        AggregateKind, AssertKind, BasicBlockId, BinOp, BorrowKind, LocalId, MirBody,
        Operand as MirOperand, OperandKind, PlaceTy, ProjectionElem, Rvalue as MirRvalue,
        StatementKind, StoredPlace, TerminatorKind, UnOp,
    },
    next_solver::{
        ConstKind, DbInterner, GenericArgs, ParamEnv, Ty, TyKind, TypingMode,
        infer::{DbInternerInferExt as _, InferCtxt},
    },
};
use rustc_type_ir::inherent::{AdtDef as _, IntoKind as _};
use syntax::{SyntaxKind, TextRange, view as ast};

use super::index::{self, Role, Site};
use crate::{
    blaze::{Lint, test_code},
    check::Cx,
    diag::Span,
    query,
};

/// The version of the canonical form, hashed into every fingerprint: a change to any step
/// bumps it, so a hash of one form is never compared with a hash of another.
pub(crate) const CANON_VERSION: u32 = 1;

/// The domain of the fingerprint, so no other hash of a MIR body equals one by construction.
const DOMAIN: &str = "blaze.dup.same_behaviour";

/// The seed of every hash this module makes; the other duplicate hashes use 1 to 6.
const SEED: u64 = 7;

/// A byte hashed after printed text, so two texts written one after the other never hash as
/// one longer text.
const SEPARATOR: u8 = 0xff;

/// The fewest statements and terminators a canonical body has to be compared: a getter, a
/// constructor and up to three arithmetic operations are 2 to 4 (the MIR has no overflow
/// asserts, only plain operations), and a function that small is one a shared helper would
/// not make shorter.
pub(crate) const SEMANTIC_FLOOR: u32 = 6;

/// The most statements and terminators a canonical body has to be compared: past it a body is
/// rarely rewritten whole, and the pass stays linear in what it reads.
pub(crate) const SEMANTIC_MOST: u32 = 256;

/// The most statements and blocks of the MIR a body is lowered from: lowering and the steps
/// shrink a body by a few times at most, so a bigger one cannot come under [`SEMANTIC_MOST`].
const RAW_MOST: usize = 4 * SEMANTIC_MOST as usize;

/// The most rounds of the steps to their fixpoint: each round's steps run to their own
/// fixpoints, so a real body settles in two; one that does not settle in this many is refused.
pub(crate) const CANON_ROUNDS: u32 = 8;

/// The most steps any walk of a body takes (a search, a repeat of a step to its fixpoint): a
/// body has at most [`RAW_MOST`] blocks and statements, and a walk visits each edge or removes
/// one statement per step, so the bound stops only a walk over a malformed graph.
const WALK_MOST: usize = 64 * RAW_MOST;

/// A symbol: the hash of what a leaf names (a type, a constant, a callee, an operation).
pub(crate) type Sym = u64;

/// A projection of a place.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Proj {
    Deref,
    Field(u32),
    /// Indexed by the local.
    Index(u32),
    ConstIndex { offset: u64, min_length: u64, from_end: bool },
    Subslice { from: u64, to: u64 },
    /// To the variant the symbol names.
    Downcast(Sym),
}

/// A place: a local and its projections.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Place {
    pub(crate) local: u32,
    pub(crate) proj: Vec<Proj, Global>,
}

impl Place {
    /// The whole of `local`.
    pub(crate) fn whole(local: u32) -> Place {
        Place { local, proj: Vec::new_in(Global) }
    }

    fn is_whole(&self) -> bool {
        self.proj.is_empty()
    }

    fn has_deref(&self) -> bool {
        self.proj.contains(&Proj::Deref)
    }

    fn locals(&self, f: &mut impl FnMut(u32)) {
        f(self.local);
        for proj in &self.proj {
            if let Proj::Index(local) = proj {
                f(*local);
            }
        }
    }

    fn locals_mut(&mut self, f: &mut impl FnMut(&mut u32)) {
        f(&mut self.local);
        for proj in &mut self.proj {
            if let Proj::Index(local) = proj {
                f(local);
            }
        }
    }
}

/// An operand: a read of a place (by copy or by move, one to the form), or a constant.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Operand {
    Read(Place),
    Const(Sym),
    /// A `bool` constant, which the steps fold.
    Bool(bool),
}

impl Operand {
    fn locals(&self, f: &mut impl FnMut(u32)) {
        if let Operand::Read(place) = self {
            place.locals(f);
        }
    }

    fn locals_mut(&mut self, f: &mut impl FnMut(&mut u32)) {
        if let Operand::Read(place) = self {
            place.locals_mut(f);
        }
    }
}

/// What a statement assigns.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Rvalue {
    Use(Operand),
    /// `!` of a `bool`.
    Not(Operand),
    /// `==` (`!=` when `negated`) of two `bool`s.
    BoolEq { negated: bool, a: Operand, b: Operand },
    /// Any other operation of operands, by what it is (`head`): its operands are sorted when
    /// it `commutes`.
    Op { head: Sym, args: Vec<Operand, Global>, commutes: bool },
    /// An operation of a place itself (a borrow, a length, a discriminant).
    At { head: Sym, place: Place },
}

impl Rvalue {
    fn operands(&self, f: &mut impl FnMut(&Operand)) {
        match self {
            Rvalue::Use(a) | Rvalue::Not(a) => f(a),
            Rvalue::BoolEq { a, b, .. } => {
                f(a);
                f(b);
            }
            Rvalue::Op { args, .. } => {
                for arg in args {
                    f(arg);
                }
            }
            Rvalue::At { .. } => {}
        }
    }

    fn operands_mut(&mut self, f: &mut impl FnMut(&mut Operand)) {
        match self {
            Rvalue::Use(a) | Rvalue::Not(a) => f(a),
            Rvalue::BoolEq { a, b, .. } => {
                f(a);
                f(b);
            }
            Rvalue::Op { args, .. } => {
                for arg in args {
                    f(arg);
                }
            }
            Rvalue::At { .. } => {}
        }
    }

    fn locals(&self, f: &mut impl FnMut(u32)) {
        match self {
            Rvalue::At { place, .. } => place.locals(f),
            _ => self.operands(&mut |op: &Operand| op.locals(&mut *f)),
        }
    }

    fn locals_mut(&mut self, f: &mut impl FnMut(&mut u32)) {
        match self {
            Rvalue::At { place, .. } => place.locals_mut(f),
            _ => self.operands_mut(&mut |op: &mut Operand| op.locals_mut(&mut *f)),
        }
    }
}

/// A statement.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Stmt {
    Assign(Place, Rvalue),
    Deinit(Place),
}

impl Stmt {
    fn operands_mut(&mut self, f: &mut impl FnMut(&mut Operand)) {
        match self {
            Stmt::Assign(_, rvalue) => rvalue.operands_mut(f),
            Stmt::Deinit(_) => {}
        }
    }

    fn operands(&self, f: &mut impl FnMut(&Operand)) {
        match self {
            Stmt::Assign(_, rvalue) => rvalue.operands(f),
            Stmt::Deinit(_) => {}
        }
    }

    fn locals_mut(&mut self, f: &mut impl FnMut(&mut u32)) {
        match self {
            Stmt::Assign(place, rvalue) => {
                place.locals_mut(&mut *f);
                rvalue.locals_mut(f);
            }
            Stmt::Deinit(place) => place.locals_mut(f),
        }
    }

    /// The place it writes.
    fn dest(&self) -> &Place {
        match self {
            Stmt::Assign(place, _) | Stmt::Deinit(place) => place,
        }
    }
}

/// How a block ends.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Term {
    Goto(u32),
    /// `arms` by value, then `otherwise`; `on_bool` when the operand is a `bool`.
    Switch { discr: Operand, on_bool: bool, arms: Vec<(u128, u32), Global>, otherwise: u32 },
    Return,
    Unreachable,
    Resume,
    Abort,
    Drop { place: Place, target: u32, unwind: Option<u32> },
    Call {
        func: Operand,
        args: Vec<Operand, Global>,
        dest: Place,
        target: Option<u32>,
        cleanup: Option<u32>,
    },
    TailCall { func: Operand, args: Vec<Operand, Global> },
    /// A check that panics with the message `msg` of `msg_args` (sorted when it `commutes`).
    Assert {
        cond: Operand,
        expected: bool,
        msg: Sym,
        msg_args: Vec<Operand, Global>,
        commutes: bool,
        target: u32,
        cleanup: Option<u32>,
    },
}

impl Term {
    /// Its successors, in operand order.
    fn targets(&self, f: &mut impl FnMut(u32)) {
        match self {
            Term::Goto(target) => f(*target),
            Term::Switch { arms, otherwise, .. } => {
                for (_, target) in arms {
                    f(*target);
                }
                f(*otherwise);
            }
            Term::Drop { target, unwind, .. } => {
                f(*target);
                if let Some(unwind) = unwind {
                    f(*unwind);
                }
            }
            Term::Call { target, cleanup, .. } => {
                if let Some(target) = target {
                    f(*target);
                }
                if let Some(cleanup) = cleanup {
                    f(*cleanup);
                }
            }
            Term::Assert { target, cleanup, .. } => {
                f(*target);
                if let Some(cleanup) = cleanup {
                    f(*cleanup);
                }
            }
            Term::Return | Term::Unreachable | Term::Resume | Term::Abort | Term::TailCall { .. } => {}
        }
    }

    fn targets_mut(&mut self, f: &mut impl FnMut(&mut u32)) {
        match self {
            Term::Goto(target) => f(target),
            Term::Switch { arms, otherwise, .. } => {
                for (_, target) in arms.iter_mut() {
                    f(target);
                }
                f(otherwise);
            }
            Term::Drop { target, unwind, .. } => {
                f(target);
                if let Some(unwind) = unwind {
                    f(unwind);
                }
            }
            Term::Call { target, cleanup, .. } => {
                if let Some(target) = target {
                    f(target);
                }
                if let Some(cleanup) = cleanup {
                    f(cleanup);
                }
            }
            Term::Assert { target, cleanup, .. } => {
                f(target);
                if let Some(cleanup) = cleanup {
                    f(cleanup);
                }
            }
            Term::Return | Term::Unreachable | Term::Resume | Term::Abort | Term::TailCall { .. } => {}
        }
    }

    fn operands(&self, f: &mut impl FnMut(&Operand)) {
        match self {
            Term::Switch { discr, .. } => f(discr),
            Term::Call { func, args, .. } | Term::TailCall { func, args } => {
                f(func);
                for arg in args {
                    f(arg);
                }
            }
            Term::Assert { cond, msg_args, .. } => {
                f(cond);
                for arg in msg_args {
                    f(arg);
                }
            }
            _ => {}
        }
    }

    fn operands_mut(&mut self, f: &mut impl FnMut(&mut Operand)) {
        match self {
            Term::Switch { discr, .. } => f(discr),
            Term::Call { func, args, .. } | Term::TailCall { func, args } => {
                f(func);
                for arg in args.iter_mut() {
                    f(arg);
                }
            }
            Term::Assert { cond, msg_args, .. } => {
                f(cond);
                for arg in msg_args.iter_mut() {
                    f(arg);
                }
            }
            _ => {}
        }
    }

    /// Its locals in walk order: a call's callee, arguments, then destination.
    fn locals(&self, f: &mut impl FnMut(u32)) {
        match self {
            Term::Drop { place, .. } => place.locals(f),
            Term::Call { func, args, dest, .. } => {
                func.locals(&mut *f);
                for arg in args {
                    arg.locals(&mut *f);
                }
                dest.locals(f);
            }
            _ => self.operands(&mut |op: &Operand| op.locals(&mut *f)),
        }
    }

    fn locals_mut(&mut self, f: &mut impl FnMut(&mut u32)) {
        match self {
            Term::Drop { place, .. } => place.locals_mut(f),
            Term::Call { func, args, dest, .. } => {
                func.locals_mut(&mut *f);
                for arg in args.iter_mut() {
                    arg.locals_mut(&mut *f);
                }
                dest.locals_mut(f);
            }
            _ => self.operands_mut(&mut |op: &mut Operand| op.locals_mut(&mut *f)),
        }
    }

    /// Whether it ends the function with no successor and nothing read.
    fn is_end(&self) -> bool {
        matches!(self, Term::Return | Term::Unreachable | Term::Resume | Term::Abort)
    }
}

/// A basic block.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Block {
    pub(crate) stmts: Vec<Stmt, Global>,
    pub(crate) term: Term,
}

/// A body: the type of each local (the return place 0, then `params` parameters), the entry
/// block and the blocks.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Body {
    pub(crate) params: u32,
    pub(crate) locals: Vec<Sym, Global>,
    pub(crate) entry: u32,
    pub(crate) blocks: Vec<Block, Global>,
}

/// A vector of `n` copies of `value`.
fn filled<T: Clone>(value: T, n: usize) -> Vec<T, Global> {
    let mut out = Vec::with_capacity_in(n, Global);
    out.resize(n, value);
    out
}

/// The canonical form of `body`: the steps repeated to their fixpoint, or `None` when they do
/// not settle within [`CANON_ROUNDS`] or the graph is malformed. Equal for bodies that differ
/// in the order of their blocks or the numbers of their temporaries, and its own canonical
/// form.
pub(crate) fn canonical(body: &Body) -> Option<Body> {
    let mut current = body.clone();
    for _ in 0..CANON_ROUNDS {
        let numbered = number(&thread(&current))?;
        let transformed = transform(&numbered)?;
        if transformed == numbered {
            return Some(numbered);
        }
        current = transformed;
    }
    None
}

/// The statements and terminators of `body`: the size its floors judge.
pub(crate) fn size(body: &Body) -> u32 {
    let count: usize = body.blocks.iter().map(|it| it.stmts.len() + 1).sum();
    u32::try_from(count).unwrap_or(u32::MAX)
}

/// The fingerprint of a canonical body.
pub(crate) fn fingerprint(body: &Body) -> u64 {
    let mut hasher = FixedHasher::with_seed(SEED);
    DOMAIN.hash(&mut hasher);
    CANON_VERSION.hash(&mut hasher);
    body.hash(&mut hasher);
    hasher.finish()
}

/// Whether a block only jumps.
fn only_jumps(block: &Block) -> Option<u32> {
    match block.term {
        Term::Goto(target) if block.stmts.is_empty() => Some(target),
        _ => None,
    }
}

/// `body` with every jump sent past the blocks that only jump (a cycle of them is left as it
/// is), and a jump to a block that only ends the function made that end.
fn thread(body: &Body) -> Body {
    let n = body.blocks.len();
    let resolve = |start: u32| -> u32 {
        let mut at = start;
        // A chain longer than the blocks is a cycle.
        for _ in 0..=n {
            match body.blocks.get(at as usize).and_then(only_jumps) {
                Some(next) => at = next,
                None => return at,
            }
        }
        start
    };
    let mut blocks = body.blocks.clone();
    for block in &mut blocks {
        block.term.targets_mut(&mut |target: &mut u32| *target = resolve(*target));
    }
    for at in 0..blocks.len() {
        let Term::Goto(target) = blocks[at].term else { continue };
        let end = blocks
            .get(target as usize)
            .filter(|it| it.stmts.is_empty() && it.term.is_end())
            .map(|it| it.term.clone());
        if let Some(end) = end {
            blocks[at].term = end;
        }
    }
    Body { params: body.params, locals: body.locals.clone(), entry: resolve(body.entry), blocks }
}

/// `body` with its reachable blocks in reverse post-order from the entry (successors in
/// operand order) and its locals by first appearance in that order; `None` for a target or
/// local out of range.
fn number(body: &Body) -> Option<Body> {
    let n = body.blocks.len();
    if body.entry as usize >= n {
        return None;
    }
    let mut succ: Vec<Vec<u32, Global>, Global> = Vec::with_capacity_in(n, Global);
    for block in &body.blocks {
        let mut out = Vec::new_in(Global);
        block.term.targets(&mut |target: u32| out.push(target));
        if out.iter().any(|&it| it as usize >= n) {
            return None;
        }
        succ.push(out);
    }
    let mut seen = filled(false, n);
    let mut post: Vec<u32, Global> = Vec::with_capacity_in(n, Global);
    let mut stack: Vec<(u32, usize), Global> = Vec::new_in(Global);
    stack.push((body.entry, 0));
    seen[body.entry as usize] = true;
    let mut steps = 0;
    while let Some(top) = stack.last_mut() {
        steps += 1;
        if steps > WALK_MOST {
            return None;
        }
        let (block, next) = *top;
        match succ[block as usize].get(next) {
            Some(&to) => {
                top.1 += 1;
                if !seen[to as usize] {
                    seen[to as usize] = true;
                    stack.push((to, 0));
                }
            }
            None => {
                post.push(block);
                stack.pop();
            }
        }
    }
    let mut order: Vec<Option<u32>, Global> = filled(None, n);
    for (new, &old) in post.iter().rev().enumerate() {
        order[old as usize] = Some(u32::try_from(new).ok()?);
    }
    let mut blocks: Vec<Block, Global> = Vec::with_capacity_in(post.len(), Global);
    for &old in post.iter().rev() {
        let mut block = body.blocks[old as usize].clone();
        let mut lost = false;
        block.term.targets_mut(&mut |target: &mut u32| match order[*target as usize] {
            Some(new) => *target = new,
            None => lost = true,
        });
        if lost {
            return None;
        }
        blocks.push(block);
    }
    let len = body.locals.len();
    let params = body.params as usize;
    if params >= len {
        return None;
    }
    let mut map: Vec<Option<u32>, Global> = filled(None, len);
    for (at, slot) in map.iter_mut().enumerate().take(params + 1) {
        *slot = u32::try_from(at).ok();
    }
    let mut next = body.params + 1;
    let mut lost = false;
    let mut renumber = |local: &mut u32| {
        let Some(slot) = map.get_mut(*local as usize) else {
            lost = true;
            return;
        };
        let new = *slot.get_or_insert_with(|| {
            let it = next;
            next += 1;
            it
        });
        *local = new;
    };
    for block in &mut blocks {
        for stmt in &mut block.stmts {
            stmt.locals_mut(&mut renumber);
        }
        block.term.locals_mut(&mut renumber);
    }
    if lost {
        return None;
    }
    let mut locals: Vec<Sym, Global> = filled(0, next as usize);
    for (old, slot) in map.iter().enumerate() {
        if let Some(new) = slot {
            locals[*new as usize] = body.locals[old];
        }
    }
    Some(Body { params: body.params, locals, entry: 0, blocks })
}

/// One round of the steps that change what the body says, each to its own fixpoint.
fn transform(body: &Body) -> Option<Body> {
    let mut out = body.clone();
    fold_bools(&mut out);
    copy_propagate(&mut out)?;
    dead_stores(&mut out)?;
    flip_switches(&mut out);
    simplify_switches(&mut out);
    sort_commutative(&mut out);
    Some(out)
}

/// How often a local is named, and how.
#[derive(Clone, Copy, Debug, Default)]
struct Mentions {
    /// As the whole destination of an assignment.
    defs: u32,
    /// As a whole operand.
    reads: u32,
    /// In any other way: projected, borrowed, dropped, a call's destination, an index.
    other: u32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mention {
    Def,
    Read,
    Other,
}

fn note(out: &mut [Mentions], local: u32, mention: Mention) {
    if let Some(it) = out.get_mut(local as usize) {
        match mention {
            Mention::Def => it.defs += 1,
            Mention::Read => it.reads += 1,
            Mention::Other => it.other += 1,
        }
    }
}

fn note_place(out: &mut [Mentions], place: &Place, whole: Mention) {
    note(out, place.local, if place.is_whole() { whole } else { Mention::Other });
    for proj in &place.proj {
        if let Proj::Index(local) = proj {
            note(out, *local, Mention::Other);
        }
    }
}

fn note_operand(out: &mut [Mentions], operand: &Operand) {
    if let Operand::Read(place) = operand {
        note_place(out, place, Mention::Read);
    }
}

/// The [`Mentions`] of each local of `body`.
fn mentions(body: &Body) -> Vec<Mentions, Global> {
    let mut out = filled(Mentions::default(), body.locals.len());
    for block in &body.blocks {
        for stmt in &block.stmts {
            match stmt {
                Stmt::Assign(dest, rvalue) => {
                    note_place(&mut out, dest, Mention::Def);
                    match rvalue {
                        Rvalue::At { place, .. } => note_place(&mut out, place, Mention::Other),
                        _ => rvalue.operands(&mut |op: &Operand| note_operand(&mut out, op)),
                    }
                }
                Stmt::Deinit(place) => note_place(&mut out, place, Mention::Other),
            }
        }
        match &block.term {
            Term::Drop { place, .. } => note_place(&mut out, place, Mention::Other),
            Term::Call { dest, .. } => {
                block.term.operands(&mut |op: &Operand| note_operand(&mut out, op));
                note_place(&mut out, dest, Mention::Other);
            }
            term => term.operands(&mut |op: &Operand| note_operand(&mut out, op)),
        }
    }
    out
}

/// `b == true` is `b`, `b == false` is `!b`, `!true` is `false`; a comparison of two `bool`s
/// takes its operands in order.
fn fold_bools(body: &mut Body) {
    for block in &mut body.blocks {
        for stmt in &mut block.stmts {
            if let Stmt::Assign(_, rvalue) = stmt
                && let Some(folded) = fold_bool(rvalue)
            {
                *rvalue = folded;
            }
        }
    }
}

fn fold_bool(rvalue: &Rvalue) -> Option<Rvalue> {
    match rvalue {
        Rvalue::Not(Operand::Bool(value)) => Some(Rvalue::Use(Operand::Bool(!value))),
        Rvalue::BoolEq { negated, a, b } => match (a, b) {
            (Operand::Bool(x), Operand::Bool(y)) => Some(Rvalue::Use(Operand::Bool((x == y) != *negated))),
            (Operand::Bool(value), other) | (other, Operand::Bool(value)) => Some(if *value != *negated {
                Rvalue::Use(other.clone())
            } else {
                Rvalue::Not(other.clone())
            }),
            _ if a > b => Some(Rvalue::BoolEq { negated: *negated, a: b.clone(), b: a.clone() }),
            _ => None,
        },
        _ => None,
    }
}

/// Where the one whole read of `local` is: its block and its statement, or the block's
/// statement count for the terminator.
fn read_of(body: &Body, local: u32) -> Option<(usize, usize)> {
    let wanted = Operand::Read(Place::whole(local));
    for (at, block) in body.blocks.iter().enumerate() {
        for (index, stmt) in block.stmts.iter().enumerate() {
            let mut found = false;
            stmt.operands(&mut |op: &Operand| found |= *op == wanted);
            if found {
                return Some((at, index));
            }
        }
        let mut found = false;
        block.term.operands(&mut |op: &Operand| found |= *op == wanted);
        if found {
            return Some((at, block.stmts.len()));
        }
    }
    None
}

/// Where the one whole definition of `local` is, as a plain copy, and what it copies.
fn copy_of(body: &Body, local: u32) -> Option<(usize, usize, Operand)> {
    for (at, block) in body.blocks.iter().enumerate() {
        for (index, stmt) in block.stmts.iter().enumerate() {
            if let Stmt::Assign(dest, rvalue) = stmt
                && dest.local == local
                && dest.is_whole()
            {
                return match rvalue {
                    Rvalue::Use(source) => Some((at, index, source.clone())),
                    _ => None,
                };
            }
        }
    }
    None
}

/// Whether a statement of `block` in `from..to` may change what `source` reads: it writes
/// its local or a local it is indexed by, or writes through a reference.
fn written_between(block: &Block, source: &Place, from: usize, to: usize) -> bool {
    let mut named = Vec::new_in(Global);
    source.locals(&mut |local: u32| named.push(local));
    block.stmts.get(from..to.min(block.stmts.len())).unwrap_or_default().iter().any(|stmt| {
        let dest = stmt.dest();
        dest.has_deref() || named.contains(&dest.local)
    })
}

/// Copy propagation, one temporary at a time to the fixpoint: a temporary defined once as a
/// copy and read once, whole, reads what it copies. A constant may be read anywhere; a place
/// only in the same block, after the copy, with nothing between that may change it. And a
/// temporary defined once by any other operation and read once as the whole of a later
/// assignment of the same block (`_t = a * k; y = _t`) is that operation assigned there, so a
/// `let` lowered through a temporary and one lowered into its local are one form.
fn copy_propagate(body: &mut Body) -> Option<()> {
    for _ in 0..WALK_MOST {
        if !propagate_one(body) && !forward_one(body) {
            return Some(());
        }
    }
    None
}

/// Moves one operation into the assignment that only copies its temporary: `_t = R; ..;
/// D = _t` becomes `..; D = R`, when `_t` is defined and read once, in one block, and nothing
/// between writes a local `R` reads or writes through a reference.
fn forward_one(body: &mut Body) -> bool {
    let counts = mentions(body);
    let first_temp = body.params as usize + 1;
    for (temp, count) in counts.iter().enumerate().skip(first_temp) {
        if count.defs != 1 || count.reads != 1 || count.other != 0 {
            continue;
        }
        let Ok(temp) = u32::try_from(temp) else { continue };
        let Some((def_block, def_at)) = def_of(body, temp) else { continue };
        let Some((use_block, use_at)) = read_of(body, temp) else { continue };
        if use_block != def_block || use_at <= def_at {
            continue;
        }
        let block = &body.blocks[def_block];
        let wanted = Rvalue::Use(Operand::Read(Place::whole(temp)));
        let Some(Stmt::Assign(_, used)) = block.stmts.get(use_at) else { continue };
        if *used != wanted {
            continue;
        }
        let Stmt::Assign(_, rvalue) = &block.stmts[def_at] else { continue };
        let mut named = Vec::new_in(Global);
        rvalue.locals(&mut |local: u32| named.push(local));
        let changed = block.stmts[def_at + 1..use_at].iter().any(|stmt| {
            let dest = stmt.dest();
            dest.has_deref() || named.contains(&dest.local)
        });
        if changed {
            continue;
        }
        let block = &mut body.blocks[def_block];
        let Stmt::Assign(_, rvalue) = block.stmts.remove(def_at) else { continue };
        if let Some(Stmt::Assign(_, used)) = block.stmts.get_mut(use_at - 1) {
            *used = rvalue;
        }
        return true;
    }
    false
}

/// Where the one whole assignment of `local` is: its block and its statement.
fn def_of(body: &Body, local: u32) -> Option<(usize, usize)> {
    for (at, block) in body.blocks.iter().enumerate() {
        for (index, stmt) in block.stmts.iter().enumerate() {
            if let Stmt::Assign(dest, _) = stmt
                && dest.local == local
                && dest.is_whole()
            {
                return Some((at, index));
            }
        }
    }
    None
}

fn propagate_one(body: &mut Body) -> bool {
    let counts = mentions(body);
    let first_temp = body.params as usize + 1;
    for (temp, count) in counts.iter().enumerate().skip(first_temp) {
        if count.defs != 1 || count.reads != 1 || count.other != 0 {
            continue;
        }
        let Ok(temp) = u32::try_from(temp) else { continue };
        let Some((def_block, def_at, source)) = copy_of(body, temp) else { continue };
        let Some((use_block, use_at)) = read_of(body, temp) else { continue };
        let movable = match &source {
            Operand::Const(_) | Operand::Bool(_) => true,
            Operand::Read(place) => {
                use_block == def_block
                    && use_at > def_at
                    && !place.has_deref()
                    && !written_between(&body.blocks[def_block], place, def_at + 1, use_at)
            }
        };
        if !movable {
            continue;
        }
        let wanted = Operand::Read(Place::whole(temp));
        let mut replace = |op: &mut Operand| {
            if *op == wanted {
                *op = source.clone();
            }
        };
        let block = &mut body.blocks[use_block];
        match block.stmts.get_mut(use_at) {
            Some(stmt) => stmt.operands_mut(&mut replace),
            None => block.term.operands_mut(&mut replace),
        }
        body.blocks[def_block].stmts.remove(def_at);
        return true;
    }
    false
}

/// Which locals are read: named anywhere but as the base of a destination written whole or
/// by projection without a dereference.
fn live_locals(body: &Body) -> Vec<bool, Global> {
    let mut live = filled(false, body.locals.len());
    let mut mark = |local: u32| {
        if let Some(it) = live.get_mut(local as usize) {
            *it = true;
        }
    };
    for block in &body.blocks {
        for stmt in &block.stmts {
            let dest = stmt.dest();
            if dest.has_deref() {
                mark(dest.local);
            }
            for proj in &dest.proj {
                if let Proj::Index(local) = proj {
                    mark(*local);
                }
            }
            if let Stmt::Assign(_, rvalue) = stmt {
                rvalue.locals(&mut mark);
            }
        }
        block.term.locals(&mut mark);
    }
    live
}

/// Drops assignments to locals nothing reads (not the return place, not through a
/// reference), to the fixpoint: an rvalue has no effect but its value.
fn dead_stores(body: &mut Body) -> Option<()> {
    for _ in 0..WALK_MOST {
        let live = live_locals(body);
        let mut changed = false;
        for block in &mut body.blocks {
            let before = block.stmts.len();
            block.stmts.retain(|stmt| {
                let dest = stmt.dest();
                dest.local == 0 || dest.has_deref() || live.get(dest.local as usize).copied().unwrap_or(true)
            });
            changed |= block.stmts.len() != before;
        }
        if !changed {
            return Some(());
        }
    }
    None
}

/// The target a switch takes for `value`.
fn target_for(arms: &[(u128, u32)], otherwise: u32, value: u128) -> u32 {
    arms.iter().find(|it| it.0 == value).map_or(otherwise, |it| it.1)
}

fn one_arm(value: u128, target: u32) -> Vec<(u128, u32), Global> {
    let mut out = Vec::with_capacity_in(1, Global);
    out.push((value, target));
    out
}

/// A switch on a temporary that is the negation, last in the block, of a `bool` switches on
/// the `bool` with its targets swapped.
fn flip_switches(body: &mut Body) {
    let counts = mentions(body);
    let params = body.params;
    for block in &mut body.blocks {
        let Term::Switch { discr: Operand::Read(place), on_bool: true, arms, otherwise } = &block.term else {
            continue;
        };
        if !place.is_whole() || place.local <= params {
            continue;
        }
        let temp = place.local;
        let Some(count) = counts.get(temp as usize) else { continue };
        if count.defs != 1 || count.reads != 1 || count.other != 0 {
            continue;
        }
        let Some(Stmt::Assign(dest, Rvalue::Not(inner))) = block.stmts.last() else { continue };
        if dest.local != temp || !dest.is_whole() {
            continue;
        }
        let (on_false, on_true) = (target_for(arms, *otherwise, 0), target_for(arms, *otherwise, 1));
        let inner = inner.clone();
        block.stmts.pop();
        block.term = Term::Switch { discr: inner, on_bool: true, arms: one_arm(0, on_true), otherwise: on_false };
    }
}

/// One form per switch: a constant `bool` operand is a jump; a `bool` switch is
/// `[0 => else] otherwise then`; arms are by value, once each, and none goes where
/// `otherwise` goes or to a block that is only `unreachable`; a switch left with one live
/// target is a jump there.
fn simplify_switches(body: &mut Body) {
    let mut dead = Vec::with_capacity_in(body.blocks.len(), Global);
    dead.extend(body.blocks.iter().map(|it| it.stmts.is_empty() && it.term == Term::Unreachable));
    for block in &mut body.blocks {
        if let Some(term) = simplified(&block.term, &dead) {
            block.term = term;
        }
    }
}

fn simplified(term: &Term, dead: &[bool]) -> Option<Term> {
    let Term::Switch { discr, on_bool, arms, otherwise } = term else { return None };
    if let Operand::Bool(value) = discr {
        return Some(Term::Goto(target_for(arms, *otherwise, u128::from(*value))));
    }
    let is_dead = |target: u32| dead.get(target as usize).copied().unwrap_or(false);
    let (mut kept, otherwise) = if *on_bool {
        (one_arm(0, target_for(arms, *otherwise, 0)), target_for(arms, *otherwise, 1))
    } else {
        let mut kept = arms.clone();
        kept.sort_by_key(|it| it.0);
        kept.dedup_by_key(|it| it.0);
        (kept, *otherwise)
    };
    kept.retain(|it| it.1 != otherwise && !is_dead(it.1));
    let first = kept.first().map(|it| it.1);
    let next = match first {
        None => Term::Goto(otherwise),
        Some(first) if is_dead(otherwise) && kept.iter().all(|it| it.1 == first) => Term::Goto(first),
        Some(_) => Term::Switch { discr: discr.clone(), on_bool: *on_bool, arms: kept, otherwise },
    };
    (next != *term).then_some(next)
}

/// Sorts the operands of commutative operations and of their overflow checks' messages.
fn sort_commutative(body: &mut Body) {
    for block in &mut body.blocks {
        for stmt in &mut block.stmts {
            if let Stmt::Assign(_, Rvalue::Op { args, commutes: true, .. }) = stmt {
                args.sort();
            }
        }
        if let Term::Assert { msg_args, commutes: true, .. } = &mut block.term {
            msg_args.sort();
        }
    }
}

/// Whether `op` on operands of `ty` gives the same for its operands swapped: integers and
/// `bool` for `+`, `*`, the bitwise operations, `==`, `!=` and the overflowing forms of `+` and
/// `*`; floats for `+` and `*` alone.
fn commutes(op: &BinOp, ty: Option<Ty<'_>>) -> bool {
    let Some(ty) = ty else { return false };
    match ty.kind() {
        TyKind::Int(_) | TyKind::Uint(_) | TyKind::Bool => matches!(
            op,
            BinOp::Add
                | BinOp::Mul
                | BinOp::BitAnd
                | BinOp::BitOr
                | BinOp::BitXor
                | BinOp::Eq
                | BinOp::Ne
                | BinOp::AddWithOverflow
                | BinOp::MulWithOverflow
        ),
        TyKind::Float(_) => matches!(op, BinOp::Add | BinOp::Mul),
        _ => false,
    }
}

/// The symbol of `parts`.
fn sym(parts: impl Hash) -> Sym {
    let mut hasher = FixedHasher::with_seed(SEED);
    parts.hash(&mut hasher);
    hasher.finish()
}

/// Printed text written into a hasher, never into a string.
struct HashWrite<'h>(&'h mut FixedHasher);

impl fmt::Write for HashWrite<'_> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        self.0.write(text.as_bytes());
        Ok(())
    }
}

fn block_of(id: BasicBlockId) -> u32 {
    id.into_raw().into_u32()
}

/// The lowering of one runtime MIR body to the form.
struct Lower<'db> {
    db: &'db Db,
    mir: &'db MirBody<'db>,
    infcx: InferCtxt<'db>,
    env: ParamEnv<'db>,
    target: DisplayTarget,
    params: u32,
    /// The position, counted from 1, of each local that is a parameter, by its MIR index.
    param_of: Vec<Option<u32>, Global>,
    types: HashMap<Ty<'db>, Sym, Global>,
}

/// The form of `def`'s runtime MIR, before the steps; `None` where it has what the form does
/// not say (inline assembly, a closure or coroutine, a union, a `for const` loop, a constant
/// or type it cannot read) or is past [`RAW_MOST`].
pub(crate) fn lower<'db>(db: &'db Db, def: DefWithBodyId, mir: &'db MirBody<'db>) -> Option<Body> {
    if !mir.closures.is_empty() || mir.coroutine.is_some() {
        return None;
    }
    let raw: usize = mir.basic_blocks.iter().map(|(_, block)| block.statements.len() + 1).sum();
    if raw > RAW_MOST {
        return None;
    }
    let krate = def.krate(db);
    let params = u32::try_from(mir.param_locals.len()).ok()?;
    let mut param_of = filled(None, mir.locals.len());
    for (at, local) in mir.param_locals.iter().enumerate() {
        *param_of.get_mut(local.into_raw().into_u32() as usize)? = Some(u32::try_from(at + 1).ok()?);
    }
    let mut lower = Lower {
        db,
        mir,
        infcx: DbInterner::new_with(db, krate).with_regions().infer_ctxt().build(TypingMode::non_body_analysis()),
        env: db.trait_environment(def.generic_def(db)),
        target: DisplayTarget::from_crate(db, krate),
        params,
        param_of,
        types: HashMap::new_in(Global),
    };
    let mut locals = filled(0, params as usize + 1 + mir.locals.len());
    for (id, local) in mir.locals.iter() {
        let at = lower.local(id) as usize;
        *locals.get_mut(at)? = lower.ty(local.ty.as_ref())?;
    }
    let mut blocks = Vec::with_capacity_in(mir.basic_blocks.len(), Global);
    for (_, block) in mir.basic_blocks.iter() {
        let mut stmts = Vec::new_in(Global);
        for statement in &block.statements {
            if let Some(stmt) = lower.stmt(&statement.kind)? {
                stmts.push(stmt);
            }
        }
        let term = lower.term(&block.terminator.as_ref()?.kind)?;
        blocks.push(Block { stmts, term });
    }
    Some(Body { params, locals, entry: block_of(mir.start_block), blocks })
}

impl<'db> Lower<'db> {
    /// The number of a MIR local: the return place 0, a parameter its position, any other
    /// past the parameters.
    fn local(&self, id: LocalId) -> u32 {
        let raw = id.into_raw().into_u32();
        if raw == 0 {
            return 0;
        }
        match self.param_of.get(raw as usize).copied().flatten() {
            Some(at) => at,
            None => self.params + raw,
        }
    }

    /// The symbol of a type: its printed form and the path of every ADT in it; `None` for a
    /// closure, coroutine, foreign or unknown type.
    fn ty(&mut self, ty: Ty<'db>) -> Option<Sym> {
        if let Some(&known) = self.types.get(&ty) {
            return Some(known);
        }
        let db = self.db;
        let mut hasher = FixedHasher::with_seed(SEED);
        "ty".hash(&mut hasher);
        for arg in ty.walk() {
            let Some(inner) = arg.ty() else { continue };
            match inner.kind() {
                TyKind::Closure(..)
                | TyKind::Coroutine(..)
                | TyKind::CoroutineClosure(..)
                | TyKind::CoroutineWitness(..)
                | TyKind::Foreign(_)
                | TyKind::Error(_)
                | TyKind::Infer(_) => return None,
                TyKind::Adt(adt, _) => blaze_items::key::path(db, ItemId::Adt(adt.def_id())).hash(&mut hasher),
                _ => {}
            }
        }
        write!(HashWrite(&mut hasher), "{}", ty.display(db, self.target)).ok()?;
        hasher.write_u8(SEPARATOR);
        let out = hasher.finish();
        self.types.insert(ty, out);
        Some(out)
    }

    /// The symbol of a variant (a struct's one, an enum's by its enum's path and index).
    fn variant(&self, variant: VariantId) -> Sym {
        let db = self.db;
        match variant {
            VariantId::EnumVariantId(it) => sym((
                "variant",
                blaze_items::key::path(db, ItemId::Adt(AdtId::EnumId(it.lookup(db).parent))),
                it.index(db),
            )),
            VariantId::StructId(it) => sym(("struct", blaze_items::key::path(db, ItemId::Adt(AdtId::StructId(it))))),
            VariantId::UnionId(it) => sym(("union", blaze_items::key::path(db, ItemId::Adt(AdtId::UnionId(it))))),
        }
    }

    /// Hashes generic arguments: types by [`Lower::ty`], constants printed, lifetimes not.
    fn args_into(&mut self, args: GenericArgs<'db>, hasher: &mut FixedHasher) -> Option<()> {
        for arg in args.iter() {
            if let Some(ty) = arg.ty() {
                self.ty(ty)?.hash(hasher);
            } else if let Some(konst) = arg.konst() {
                write!(HashWrite(&mut *hasher), "{}", konst.display(self.db, self.target)).ok()?;
                hasher.write_u8(SEPARATOR);
            }
        }
        Some(())
    }

    /// The symbol of a callee: its stable path (a method's impl by its self type and trait
    /// too, since the path writes every impl as `<impl>`) and its generic arguments.
    fn callee(&mut self, callable: CallableDefId, args: GenericArgs<'db>) -> Option<Sym> {
        let db = self.db;
        let mut hasher = FixedHasher::with_seed(SEED);
        match callable {
            CallableDefId::FunctionId(f) => {
                ("fn", blaze_items::key::path(db, ItemId::Function(f))).hash(&mut hasher);
                if let ItemContainerId::ImplId(imp) = f.lookup(db).container {
                    let imp = hir::Impl::from(imp);
                    let self_ty = imp.self_ty(db);
                    write!(HashWrite(&mut hasher), "{}", self_ty.display(db, self.target)).ok()?;
                    hasher.write_u8(SEPARATOR);
                    if let Some(adt) = self_ty.as_adt() {
                        blaze_items::key::path(db, ItemId::Adt(AdtId::from(adt))).hash(&mut hasher);
                    }
                    if let Some(of) = imp.trait_(db) {
                        blaze_items::key::path(db, ItemId::Trait(of.into())).hash(&mut hasher);
                    }
                }
            }
            CallableDefId::StructId(it) => {
                ("ctor", blaze_items::key::path(db, ItemId::Adt(AdtId::StructId(it)))).hash(&mut hasher);
            }
            CallableDefId::EnumVariantId(it) => {
                ("ctor", self.variant(VariantId::EnumVariantId(it))).hash(&mut hasher);
            }
        }
        self.args_into(args, &mut hasher)?;
        Some(hasher.finish())
    }

    fn place(&self, place: &StoredPlace) -> Option<Place> {
        let mut proj = Vec::new_in(Global);
        for elem in place.projection.as_slice() {
            proj.push(match *elem {
                ProjectionElem::Deref => Proj::Deref,
                ProjectionElem::Field(field) => Proj::Field(field.0),
                ProjectionElem::Index(local) => Proj::Index(self.local(local)),
                ProjectionElem::ConstantIndex { offset, min_length, from_end } => {
                    Proj::ConstIndex { offset, min_length, from_end }
                }
                ProjectionElem::Subslice { from, to } => Proj::Subslice { from, to },
                ProjectionElem::Downcast(variant) => Proj::Downcast(self.variant(variant)),
                ProjectionElem::ReflectedField(_) => return None,
            });
        }
        Some(Place { local: self.local(place.local), proj })
    }

    fn operand_ty(&self, operand: &MirOperand) -> Option<Ty<'db>> {
        match &operand.kind {
            OperandKind::Copy(place) | OperandKind::Move(place) => Some(
                PlaceTy::from_ty(self.mir.locals[place.local].ty.as_ref())
                    .multi_projection_ty(&self.infcx, self.env, place.projection.as_slice())
                    .ty,
            ),
            OperandKind::Constant { ty, .. } => Some(ty.as_ref()),
            OperandKind::Allocation { allocation } => Some(allocation.as_ref().ty),
            OperandKind::Static(_) => None,
        }
    }

    fn is_bool(&self, operand: &MirOperand) -> bool {
        self.operand_ty(operand).is_some_and(|it| matches!(it.kind(), TyKind::Bool))
    }

    fn operand(&mut self, operand: &MirOperand) -> Option<Operand> {
        if let Some((callable, args)) = operand.fn_def() {
            return Some(Operand::Const(self.callee(callable, args)?));
        }
        match &operand.kind {
            OperandKind::Copy(place) | OperandKind::Move(place) => Some(Operand::Read(self.place(place)?)),
            OperandKind::Constant { konst, ty } => {
                let ty = ty.as_ref();
                let of = self.ty(ty)?;
                let bits = match konst.as_ref().kind() {
                    ConstKind::Value(value) => value.try_to_leaf().map(|leaf| leaf.to_bits_unchecked()),
                    _ => None,
                };
                if let Some(bits) = bits {
                    if matches!(ty.kind(), TyKind::Bool) && bits <= 1 {
                        return Some(Operand::Bool(bits == 1));
                    }
                    return Some(Operand::Const(sym(("bits", of, bits))));
                }
                let mut hasher = FixedHasher::with_seed(SEED);
                ("const", of).hash(&mut hasher);
                write!(HashWrite(&mut hasher), "{}", konst.as_ref().display(self.db, self.target)).ok()?;
                hasher.write_u8(SEPARATOR);
                Some(Operand::Const(hasher.finish()))
            }
            OperandKind::Allocation { allocation } => {
                let data = allocation.as_ref();
                let of = self.ty(data.ty)?;
                let map: &[u8] = match &data.memory_map {
                    MemoryMap::Empty => &[],
                    MemoryMap::Simple(bytes) => &bytes[..],
                    MemoryMap::Complex(_) => return None,
                };
                if matches!(data.ty.kind(), TyKind::Bool) && map.is_empty() {
                    match &data.memory[..] {
                        [0] => return Some(Operand::Bool(false)),
                        [1] => return Some(Operand::Bool(true)),
                        _ => {}
                    }
                }
                Some(Operand::Const(sym(("bytes", of, &data.memory[..], map))))
            }
            OperandKind::Static(it) => {
                Some(Operand::Const(sym(("static", blaze_items::key::path(self.db, ItemId::Static(*it))))))
            }
        }
    }

    fn operands(&mut self, operands: &[MirOperand]) -> Option<Vec<Operand, Global>> {
        let mut out = Vec::with_capacity_in(operands.len(), Global);
        for operand in operands {
            out.push(self.operand(operand)?);
        }
        Some(out)
    }

    fn binary(&mut self, op: &BinOp, a: &MirOperand, b: &MirOperand, checked: bool) -> Option<Rvalue> {
        let ty = self.operand_ty(a);
        let on_bool = ty.is_some_and(|it| matches!(it.kind(), TyKind::Bool));
        let commutes = commutes(op, ty);
        let (a, b) = (self.operand(a)?, self.operand(b)?);
        if !checked && on_bool && matches!(op, BinOp::Eq | BinOp::Ne) {
            return Some(Rvalue::BoolEq { negated: *op == BinOp::Ne, a, b });
        }
        let mut args = Vec::with_capacity_in(2, Global);
        args.push(a);
        args.push(b);
        Some(Rvalue::Op { head: sym(("binary", checked, op)), args, commutes })
    }

    fn rvalue(&mut self, rvalue: &MirRvalue) -> Option<Rvalue> {
        let one = |operand: Operand| {
            let mut out = Vec::with_capacity_in(1, Global);
            out.push(operand);
            out
        };
        Some(match rvalue {
            MirRvalue::Use(operand) => Rvalue::Use(self.operand(operand)?),
            MirRvalue::CopyForDeref(place) => Rvalue::Use(Operand::Read(self.place(place)?)),
            MirRvalue::Repeat(operand, count) => {
                let mut hasher = FixedHasher::with_seed(SEED);
                "repeat".hash(&mut hasher);
                write!(HashWrite(&mut hasher), "{}", count.as_ref().display(self.db, self.target)).ok()?;
                hasher.write_u8(SEPARATOR);
                Rvalue::Op { head: hasher.finish(), args: one(self.operand(operand)?), commutes: false }
            }
            MirRvalue::Ref(kind, place) => Rvalue::At {
                // A two-phase borrow of auto-ref and a written `&mut` are one borrow.
                head: sym(("ref", matches!(kind, BorrowKind::Mut { .. }))),
                place: self.place(place)?,
            },
            MirRvalue::Len(place) => Rvalue::At { head: sym("len"), place: self.place(place)? },
            MirRvalue::Discriminant(place) => Rvalue::At { head: sym("discriminant"), place: self.place(place)? },
            MirRvalue::Cast(kind, operand, ty) => {
                let to = self.ty(ty.as_ref())?;
                Rvalue::Op { head: sym(("cast", kind, to)), args: one(self.operand(operand)?), commutes: false }
            }
            MirRvalue::BinaryOp(op, a, b) => self.binary(op, a, b, false)?,
            MirRvalue::CheckedBinaryOp(op, a, b) => self.binary(op, a, b, true)?,
            MirRvalue::UnaryOp(op, operand) => {
                let on_bool = self.is_bool(operand);
                let operand = self.operand(operand)?;
                if *op == UnOp::Not && on_bool {
                    Rvalue::Not(operand)
                } else {
                    Rvalue::Op { head: sym(("unary", op)), args: one(operand), commutes: false }
                }
            }
            MirRvalue::Aggregate(kind, operands) => {
                let head = match kind {
                    AggregateKind::Array(ty) => sym(("array", self.ty(ty.as_ref())?)),
                    AggregateKind::Tuple(ty) => sym(("tuple", self.ty(ty.as_ref())?)),
                    AggregateKind::Adt(variant, args) => {
                        let mut hasher = FixedHasher::with_seed(SEED);
                        ("adt", self.variant(*variant)).hash(&mut hasher);
                        self.args_into(args.as_ref(), &mut hasher)?;
                        hasher.finish()
                    }
                    AggregateKind::Union(..) | AggregateKind::Closure(_) => return None,
                };
                Rvalue::Op { head, args: self.operands(operands)?, commutes: false }
            }
            MirRvalue::ThreadLocalRef(never) | MirRvalue::AddressOf(never) | MirRvalue::NullaryOp(never) => {
                match *never {}
            }
        })
    }

    /// A statement, `Some(None)` for one the form drops, `None` for one it cannot say.
    fn stmt(&mut self, kind: &StatementKind) -> Option<Option<Stmt>> {
        Some(match kind {
            StatementKind::Assign(place, rvalue) => Some(Stmt::Assign(self.place(place)?, self.rvalue(rvalue)?)),
            StatementKind::Deinit(place) => Some(Stmt::Deinit(self.place(place)?)),
            StatementKind::FakeRead(_)
            | StatementKind::StorageLive(_)
            | StatementKind::StorageDead(_)
            | StatementKind::Nop => None,
            StatementKind::InlineAsm(_) => return None,
        })
    }

    /// An assertion's message, and whether its operands commute.
    fn message(&self, msg: &AssertKind) -> (Sym, bool) {
        match msg {
            AssertKind::BoundsCheck { .. } => (sym("bounds"), false),
            AssertKind::Overflow(op, a, _) => (sym(("overflow", op)), commutes(op, self.operand_ty(a))),
            AssertKind::OverflowNeg(_) => (sym("overflow_neg"), false),
            AssertKind::DivisionByZero(_) => (sym("division_by_zero"), false),
            AssertKind::RemainderByZero(_) => (sym("remainder_by_zero"), false),
            AssertKind::MisalignedPointerDereference { .. } => (sym("misaligned"), false),
            AssertKind::NullPointerDereference => (sym("null_deref"), false),
            AssertKind::NullReferenceConstructed => (sym("null_ref"), false),
            AssertKind::InvalidEnumConstruction(_) => (sym("invalid_enum"), false),
        }
    }

    fn term(&mut self, kind: &TerminatorKind) -> Option<Term> {
        Some(match kind {
            TerminatorKind::Goto { target } => Term::Goto(block_of(*target)),
            TerminatorKind::FalseEdge { real_target, .. } | TerminatorKind::FalseUnwind { real_target, .. } => {
                Term::Goto(block_of(*real_target))
            }
            TerminatorKind::SwitchInt { discr, targets } => {
                let on_bool = self.is_bool(discr);
                let mut arms = Vec::with_capacity_in(targets.values.len(), Global);
                arms.extend(targets.iter().map(|(value, to)| (value, block_of(to))));
                arms.sort_unstable();
                Term::Switch { discr: self.operand(discr)?, on_bool, arms, otherwise: block_of(targets.otherwise()) }
            }
            TerminatorKind::UnwindResume => Term::Resume,
            TerminatorKind::Abort => Term::Abort,
            TerminatorKind::Return => Term::Return,
            TerminatorKind::Unreachable => Term::Unreachable,
            TerminatorKind::Drop { place, target, unwind } => {
                Term::Drop { place: self.place(place)?, target: block_of(*target), unwind: unwind.map(block_of) }
            }
            TerminatorKind::Call { func, args, destination, target, cleanup, .. } => Term::Call {
                func: self.operand(func)?,
                args: self.operands(args)?,
                dest: self.place(destination)?,
                target: target.map(block_of),
                cleanup: cleanup.map(block_of),
            },
            TerminatorKind::TailCall { func, args, .. } => {
                Term::TailCall { func: self.operand(func)?, args: self.operands(args)? }
            }
            TerminatorKind::Assert { cond, expected, msg, target, cleanup } => {
                let (message, commutes) = self.message(msg);
                let mut msg_args = Vec::new_in(Global);
                let mut unread = false;
                msg.for_each_operand(&mut |operand: &MirOperand| match self.operand(operand) {
                    Some(it) => msg_args.push(it),
                    None => unread = true,
                });
                if unread {
                    return None;
                }
                Term::Assert {
                    cond: self.operand(cond)?,
                    expected: *expected,
                    msg: message,
                    msg_args,
                    commutes,
                    target: block_of(*target),
                    cleanup: cleanup.map(block_of),
                }
            }
            TerminatorKind::DropAndReplace { .. }
            | TerminatorKind::Yield { .. }
            | TerminatorKind::CoroutineDrop
            | TerminatorKind::ComptimeFor { .. }
            | TerminatorKind::ComptimeNext { .. } => return None,
        })
    }
}

/// Whether a canonical body only returns one constant.
fn is_single_constant(body: &Body) -> bool {
    match body.blocks.as_slice() {
        [Block { stmts, term: Term::Return }] => matches!(
            stmts.as_slice(),
            [Stmt::Assign(dest, Rvalue::Use(Operand::Const(_) | Operand::Bool(_)))] if dest.local == 0 && dest.is_whole()
        ),
        _ => false,
    }
}

/// A function's canonical behaviour: its [`fingerprint`] and [`size`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SemanticRow {
    pub(crate) hash: u64,
    pub(crate) size: u32,
}

/// The canonical behaviour of `def`'s body, or nothing for what is not compared: not a
/// function of a module, impl or trait, test code, a body a macro writes (a derive's
/// methods among them), a `const fn` that is one constant, a body outside
/// [`SEMANTIC_FLOOR`]..=[`SEMANTIC_MOST`], and one whose MIR or form cannot be made.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn body_semantic(db: &Db, def: DefWithBodyId) -> Option<SemanticRow> {
    let DefWithBodyId::FunctionId(f) = def else { return None };
    let role = index::body_shape(db, def).role;
    if !matches!(role, Role::Free | Role::Inherent | Role::TraitImpl | Role::TraitDefault) {
        return None;
    }
    if test_code::is_test_def(db, def) || body_syntax(db, def).is_none() {
        return None;
    }
    let mir = db.runtime_mir_body(InferBodyId::DefWithBodyId(def)).ok()?;
    let canon = canonical(&lower(db, def, mir)?)?;
    let size = size(&canon);
    if !(SEMANTIC_FLOOR..=SEMANTIC_MOST).contains(&size) {
        return None;
    }
    if hir::Function::from(f).is_const(db) && is_single_constant(&canon) {
        return None;
    }
    Some(SemanticRow { hash: fingerprint(&canon), size })
}

/// One row of [`crate_semantic`]: a function's fingerprint, and the function.
pub(crate) type SemanticWhole = (u64, DefWithBodyId);

/// The compared functions of a crate by fingerprint, sorted: what a dependent crate searches
/// for its own. Empty for a test, bench or example target.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn crate_semantic(db: &Db, krate: base_db::Crate) -> Box<[SemanticWhole]> {
    let mut out: Vec<SemanticWhole, Global> = Vec::new_in(Global);
    if !index::is_test_target(db, krate) {
        for body in query::lint_plan(db, krate).bodies.iter() {
            if let Some(row) = *body_semantic(db, body.def) {
                out.push((row.hash, body.def));
            }
        }
    }
    out.sort_unstable();
    out.into_boxed_slice()
}

/// A finding of `same_behaviour` in a body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Finding {
    /// The function's name, in the body's anchor.
    pub(crate) at: TextRange,
    /// The original first, every function with this behaviour and another shape, and this one.
    pub(crate) sites: Box<[Site]>,
    /// What to make visible for this body to call the original, when it cannot.
    pub(crate) expose: Option<String>,
}

/// Every finding of a crate's bodies, by body.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct SemanticIndex {
    findings: Box<[(DefWithBodyId, Finding)]>,
}

/// [`Member::krate`] of the crate checked: after every dependency.
const OWN: u32 = u32::MAX;

/// One function of a group.
struct Member {
    /// A dependency's place in the walk, or [`OWN`].
    krate: u32,
    /// The file's place among the crate's module files, and the body's offset in it.
    file: u32,
    at: u32,
    /// A dependency's function's path.
    path: Option<String>,
    def: DefWithBodyId,
}

/// The name of `def`'s function in its anchor, and the label a message gives it.
fn named(db: &Db, def: DefWithBodyId) -> Option<(TextRange, String)> {
    let syntax = body_syntax(db, def).as_ref()?;
    let tree = syntax.tree(db);
    let anchor = ast::Node::root(&tree);
    let owner = ast::try_to_node(syntax.item, anchor)?;
    let name = owner.children().find(|it| it.kind() == SyntaxKind::NAME)?;
    Some((name.text_range(), format!("`{}`", name.text())))
}

/// Whether two syntax shapes are known to differ: an equal shape is `same_fn`'s, and two
/// bodies too short for a shape cannot be told apart.
fn shapes_differ(a: Option<u64>, b: Option<u64>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a != b,
        (None, None) => false,
        _ => true,
    }
}

/// Every finding of `krate`'s functions: one pass over their fingerprints, and its local
/// dependencies' searched per fingerprint.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn semantic_index(db: &Db, krate: base_db::Crate) -> SemanticIndex {
    if index::is_test_target(db, krate) {
        return SemanticIndex::default();
    }
    let plan = query::lint_plan(db, krate);
    let mut files: HashMap<EditionedFileId, u32, Global> = HashMap::new_in(Global);
    for (at, file) in plan.files.iter().enumerate() {
        files.entry(file.file).or_insert(u32::try_from(at).unwrap_or(u32::MAX));
    }
    let mut own: Vec<(u64, u32, u32, DefWithBodyId), Global> = Vec::new_in(Global);
    for body in plan.bodies.iter() {
        if let Some(row) = *body_semantic(db, body.def) {
            let file = files.get(&body.file).copied().unwrap_or(u32::MAX);
            own.push((row.hash, file, u32::from(body.offset), body.def));
        }
    }
    own.sort_unstable();
    let deps = index::local_deps(db, krate);
    let mut out: Vec<(DefWithBodyId, Finding), Global> = Vec::new_in(Global);
    let mut start = 0;
    while start < own.len() {
        let hash = own[start].0;
        let end = start + own[start..].iter().take_while(|it| it.0 == hash).count();
        let mut members: Vec<Member, Global> = Vec::new_in(Global);
        members.extend(own[start..end].iter().map(|&(_, file, at, def)| Member { krate: OWN, file, at, path: None, def }));
        start = end;
        for (walk, &dep) in deps.iter().enumerate() {
            let theirs = crate_semantic(db, dep);
            let from = theirs.partition_point(|it| it.0 < hash);
            for &(_, def) in theirs[from..].iter().take_while(|it| it.0 == hash) {
                let path = blaze_items::key::path(db, ItemId::of_body(db, def));
                let krate = u32::try_from(walk).unwrap_or(OWN - 1);
                members.push(Member { krate, file: 0, at: 0, path: Some(path), def });
            }
        }
        if members.len() < 2 {
            continue;
        }
        members.sort_by(|a, b| (a.krate, a.file, a.at, &a.path, a.def).cmp(&(b.krate, b.file, b.at, &b.path, b.def)));
        group(db, &members, &mut out);
    }
    out.sort_by(|(a, x), (b, y)| (a, x.at.start(), x.at.end()).cmp(&(b, y.at.start(), y.at.end())));
    SemanticIndex { findings: out.into_boxed_slice() }
}

/// The findings of one group of functions with one fingerprint, in order: each of the
/// crate's own that an earlier one of another shape does what it does.
fn group(db: &Db, members: &[Member], out: &mut Vec<(DefWithBodyId, Finding), Global>) {
    let mut shapes = Vec::with_capacity_in(members.len(), Global);
    shapes.extend(members.iter().map(|it| index::body_shape(db, it.def).whole));
    for (at, member) in members.iter().enumerate() {
        if member.krate != OWN {
            continue;
        }
        let Some(first) = (0..at).find(|&it| shapes_differ(shapes[it], shapes[at])) else { continue };
        let Some((name, _)) = named(db, member.def) else { continue };
        let mut sites: Vec<Site, Global> = Vec::new_in(Global);
        for (other, them) in members.iter().enumerate() {
            if other != at && !shapes_differ(shapes[other], shapes[at]) {
                continue;
            }
            let site = match &them.path {
                Some(path) => Site { place: index::Place::Crate, label: format!("`{path}`"), here: false },
                None => {
                    let Some((range, label)) = named(db, them.def) else { continue };
                    Site { place: index::Place::Body { def: them.def, range }, label, here: other == at }
                }
            };
            sites.push(site);
        }
        let expose = expose(db, &members[first], member.def, sites.first());
        out.push((member.def, Finding { at: name, sites: sites.into_boxed_slice(), expose }));
    }
}

/// What to make visible for `here` to call `original`, when it cannot now.
fn expose(db: &Db, original: &Member, here: DefWithBodyId, label: Option<&Site>) -> Option<String> {
    let DefWithBodyId::FunctionId(f) = original.def else { return None };
    let module = hir::DefWithBody::from(here).module(db);
    if hir::Function::from(f).is_visible_from(db, module) {
        return None;
    }
    let label = label.map_or("the original", |it| it.label.as_str());
    Some(format!("make {label} visible here (`pub`, and `pub` on every module on its path), then call it"))
}

/// The findings of `def`'s body: its slice of its crate's [`semantic_index`].
#[blaze_query::tracked(returns(ref))]
pub(crate) fn semantic_findings(db: &Db, def: DefWithBodyId) -> Box<[Finding]> {
    let all = semantic_index(db, def.krate(db));
    let start = all.findings.partition_point(|(it, _)| *it < def);
    let end = all.findings.partition_point(|(it, _)| *it <= def);
    let mut out = Vec::new_in(Global);
    out.extend(all.findings.get(start..end).unwrap_or_default().iter().map(|(_, it)| it.clone()));
    out.into_boxed_slice()
}

/// `same_behaviour` in the body linted.
pub(super) fn check(cx: &mut Cx<'_, '_>, def: DefWithBodyId) {
    if !cx.on_blaze(Lint::SameBehaviour) {
        return;
    }
    let db = cx.db();
    let root = cx.root();
    for found in semantic_findings(db, def).iter() {
        if root.text_range().contains_range(found.at) {
            report(cx, found);
        }
    }
}

fn report(cx: &mut Cx<'_, '_>, found: &Finding) {
    let lint = Lint::SameBehaviour;
    let here = found.sites.iter().find(|it| it.here).map_or("", |it| it.label.as_str());
    let Some(original) = found.sites.iter().find(|it| !it.here) else { return };
    let message =
        format!("{here} does what {} does (same compiled behaviour, written differently)", original.label);
    let call = format!(
        "call the original: delete {here} and call {} where it is called, or make its body a call of {}",
        original.label, original.label
    );
    let mut notes: Vec<(Option<Span>, String), Global> = Vec::new_in(Global);
    for site in found.sites.iter().filter(|it| !it.here) {
        match &site.place {
            index::Place::Body { def, range } => {
                if let Some(span) = super::span_of(cx, *def, *range) {
                    notes.push((Some(span), format!("{} is here", site.label)));
                }
            }
            index::Place::Crate => {
                notes.push((None, format!("{} is in a crate this one depends on", site.label)));
            }
        }
    }
    let node = super::node_at(cx.root(), found.at);
    let span = cx.span(found.at);
    let diagnostic = cx.blaze(lint, node, span, message);
    for (span, note) in notes {
        match span {
            Some(span) => diagnostic.span_note(span, note),
            None => diagnostic.note(note),
        };
    }
    diagnostic.help(call);
    if let Some(expose) = &found.expose {
        diagnostic.help(expose.clone());
    }
    diagnostic.help(lint.help()).help(format!("for further information visit {}", lint.docs_url()));
}

#[cfg(test)]
#[path = "semantic_tests.rs"]
mod tests;
