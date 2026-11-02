//! The unwind paths lowering writes, each step folded once per change of what it drops, after
//! rustc's cached unwind blocks (`Scope::cached_unwind_block`, `rustc_mir_build`'s
//! `builder/scope.rs`).
//!
//! The way out of a point that may panic drops what every scope holds there, innermost first. It
//! is a fold over the scopes' locals, outermost first, through `unwind_node`, whose memo makes
//! each step's block depend only on the block before it, the local and the kind of step. So the
//! block after each position of the scopes' order is kept, and when the order changes only the
//! positions after the first one that differs are folded again. A fold over a prefix that has
//! not changed allocates no block, then or when it was first folded, so blocks come out in the
//! order the fold over everything at every ask gives them.
//!
//! rustc folds a scope's drops again whenever one is scheduled in it, which is quadratic in a
//! block of many `let`s with a call after each; here a `let` is one step.
//!
//! What a fold depends on: each scope's locals and tail mark (`ScopeLocals`, which marks itself
//! stale on every change), which locals are pending operands (`operands`: it changes the order)
//! and which are unscheduled temporaries (`unscheduled`: it changes whether a step drops). Both
//! sets change only through `Unwinds`.

use std::{cell::Cell, ops::Deref};

use blaze_mem::Frame;

use crate::mir::{
    BasicBlockId, LocalId,
    framed::local_index,
};

use super::{DropScope, MirLowerCtx};

/// No position: the local is in no listed scope.
const NONE: u32 = u32::MAX;

/// A `ScopeLocals::bound` that has to be counted again.
const UNKNOWN: u32 = u32::MAX;

/// A set of locals: a bit per local index, in the frame, as long as the largest local put in.
pub(super) struct LocalSet<'f> {
    words: Vec<u64, &'f Frame>,
}

impl<'f> LocalSet<'f> {
    pub(super) fn new_in(frame: &'f Frame) -> Self {
        LocalSet { words: Vec::new_in(frame) }
    }

    pub(super) fn contains(&self, local: &LocalId) -> bool {
        let at = local_index(*local);
        self.words.get(at / 64).is_some_and(|&word| (word >> (at % 64)) & 1 == 1)
    }

    /// Whether `local` was not in the set.
    fn insert(&mut self, local: LocalId) -> bool {
        let at = local_index(local);
        if self.words.len() <= at / 64 {
            self.words.resize(at / 64 + 1, 0);
        }
        let word = &mut self.words[at / 64];
        let bit = 1u64 << (at % 64);
        let new = *word & bit == 0;
        *word |= bit;
        new
    }

    /// Whether `local` was in the set.
    fn remove(&mut self, local: LocalId) -> bool {
        let at = local_index(local);
        let Some(word) = self.words.get_mut(at / 64) else { return false };
        let bit = 1u64 << (at % 64);
        let had = *word & bit != 0;
        *word &= !bit;
        had
    }
}

/// The locals of a drop scope in the order they were declared, which is the reverse of the order
/// they are dropped in, and its tail mark. Reads go through `Deref`; every change goes through
/// `edit`, `push_new` or `set_tail`, which mark the scope for `Unwinds` to list again.
#[derive(Debug)]
pub(super) struct ScopeLocals<'f> {
    list: Vec<LocalId, &'f Frame>,
    /// While the tail of a block before the 2024 edition is lowered: how many locals the block
    /// had declared before it. What the tail declares that is not an operand outlives the
    /// block's locals (`outlive_block`), also when the tail panics or leaves early.
    tail: Option<usize>,
    /// Changed since `Unwinds` last listed it.
    stale: bool,
    /// Whether `list[..listed]` is as `Unwinds` last listed it, with only `push_new` since.
    kept: bool,
    listed: u32,
    /// One more than the largest local index in `list`, or `UNKNOWN` after an `edit`: a local
    /// at or above it is not in the scope.
    bound: Cell<u32>,
}

impl<'f> ScopeLocals<'f> {
    pub(super) fn new_in(frame: &'f Frame) -> Self {
        ScopeLocals {
            list: Vec::new_in(frame),
            tail: None,
            stale: true,
            kept: false,
            listed: 0,
            bound: Cell::new(0),
        }
    }

    pub(super) fn from_slice_in(locals: &[LocalId], frame: &'f Frame) -> Self {
        let mut list = Vec::with_capacity_in(locals.len(), frame);
        list.extend_from_slice(locals);
        ScopeLocals {
            list,
            tail: None,
            stale: true,
            kept: false,
            listed: 0,
            bound: Cell::new(UNKNOWN),
        }
    }

    /// The list, to change.
    pub(super) fn edit(&mut self) -> &mut Vec<LocalId, &'f Frame> {
        self.stale = true;
        self.kept = false;
        self.bound.set(UNKNOWN);
        &mut self.list
    }

    /// Moves the list out, with no tail, leaving this scope empty with its tail. Both are
    /// scopes `Unwinds` has not listed.
    pub(super) fn take_list(&mut self) -> Self {
        let frame = *self.list.allocator();
        let list = std::mem::replace(&mut self.list, Vec::new_in(frame));
        self.stale = true;
        self.kept = false;
        self.bound.set(0);
        ScopeLocals {
            list,
            tail: None,
            stale: true,
            kept: false,
            listed: 0,
            bound: Cell::new(UNKNOWN),
        }
    }

    /// Declares `local` last.
    pub(super) fn push_new(&mut self, local: LocalId) {
        self.stale = true;
        let bound = self.bound.get();
        if bound != UNKNOWN {
            self.bound.set(bound.max(local_index(local) as u32 + 1));
        }
        self.list.push(local);
    }

    /// Whether `local` is in the scope. A local declared after everything in it, which a new
    /// temporary always is, is answered without a scan.
    pub(super) fn holds(&self, local: LocalId) -> bool {
        let mut bound = self.bound.get();
        if bound == UNKNOWN {
            bound = self.list.iter().map(|&it| local_index(it) as u32 + 1).max().unwrap_or(0);
            self.bound.set(bound);
        }
        local_index(local) < bound as usize && self.list.contains(&local)
    }

    pub(super) fn tail(&self) -> Option<usize> {
        self.tail
    }

    pub(super) fn set_tail(&mut self, tail: Option<usize>) {
        self.stale = true;
        self.kept = false;
        self.tail = tail;
    }

    /// The block's own locals and what its tail declared, when a tail is being lowered.
    fn parts(&self) -> (&[LocalId], &[LocalId]) {
        match self.tail {
            Some(declared) if declared < self.list.len() => self.list.split_at(declared),
            _ => (&self.list, &[]),
        }
    }

    /// Appends the locals in the order they were declared. The temporaries of a tail before the
    /// 2024 edition come before the block's locals, as `outlive_block` leaves them once the tail
    /// is lowered; the tail's operands stay last.
    pub(super) fn declared_into(&self, operands: &LocalSet<'_>, out: &mut Vec<LocalId, &Frame>) {
        let (head, tail) = self.parts();
        if tail.is_empty() {
            out.extend_from_slice(head);
            return;
        }
        out.extend(tail.iter().filter(|it| !operands.contains(it)));
        out.extend_from_slice(head);
        out.extend(tail.iter().filter(|it| operands.contains(it)));
    }

    /// Appends the locals in the reverse of the order leaving the scope early drops them in:
    /// `declared_into`'s order with the pending operands moved to the end, each part in its
    /// order. rustc keeps the operands of an expression being lowered in that expression's own
    /// scope, inside the temporaries that live to the end of the statement.
    pub(super) fn leaving_into(&self, operands: &LocalSet<'_>, out: &mut Vec<LocalId, &Frame>) {
        let (head, tail) = self.parts();
        out.extend(tail.iter().filter(|it| !operands.contains(it)));
        out.extend(head.iter().filter(|it| !operands.contains(it)));
        out.extend(head.iter().filter(|it| operands.contains(it)));
        out.extend(tail.iter().filter(|it| operands.contains(it)));
    }

    /// Whether listing it again only appends what `push_new` added to both orders: nothing but
    /// pushes since it was listed, no tail being lowered, and no operand in it then or now.
    fn appends(&self, had_operands: bool, operands: &LocalSet<'_>) -> bool {
        self.kept
            && !had_operands
            && self.parts().1.is_empty()
            && self.list[self.listed as usize..].iter().all(|it| !operands.contains(it))
    }
}

impl Clone for ScopeLocals<'_> {
    /// A copy is a scope `Unwinds` has not listed.
    fn clone(&self) -> Self {
        let mut list = Vec::with_capacity_in(self.list.len(), *self.list.allocator());
        list.extend_from_slice(&self.list);
        ScopeLocals {
            list,
            tail: self.tail,
            stale: true,
            kept: false,
            listed: 0,
            bound: Cell::new(self.bound.get()),
        }
    }
}

impl Deref for ScopeLocals<'_> {
    type Target = [LocalId];

    fn deref(&self) -> &[LocalId] {
        &self.list
    }
}

/// A scope on the stack, as `Unwinds` reads it.
pub(super) trait HasLocals<'f> {
    fn locals_mut(&mut self) -> &mut ScopeLocals<'f>;
}

impl<'f> HasLocals<'f> for DropScope<'f> {
    fn locals_mut(&mut self) -> &mut ScopeLocals<'f> {
        &mut self.locals
    }
}

impl<'f> HasLocals<'f> for ScopeLocals<'f> {
    fn locals_mut(&mut self) -> &mut ScopeLocals<'f> {
        self
    }
}

/// What a fold needs of lowering: the unwind drop tree's nodes, and whether a local's value is
/// dropped on the way out.
pub(super) trait DropTree {
    /// The node that drops the value of `local` (`value`) or ends its storage, then goes on to
    /// `parent`, or to resuming the unwind when there is none.
    fn node(&mut self, parent: Option<BasicBlockId>, local: LocalId, value: bool) -> BasicBlockId;
    /// Whether the type of `local` is `Copy`, which nothing drops. Asked once a local.
    fn local_is_copy(&mut self, local: LocalId) -> bool;
    /// Whether `local` is a temporary its expression is being written to, which unwinding out
    /// of that expression does not drop.
    fn unscheduled(&self, local: LocalId) -> bool;
}

/// Which of the two orders a fold goes through.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Chain {
    /// `ScopeLocals::leaving_into`: unwinding from a statement or a call, or out of a drop on
    /// the way out of `break`, `return` and the rest.
    Leaving,
    /// `ScopeLocals::declared_into`: out of a drop at the end of a scope.
    Declared,
}

/// One order of every listed scope's locals, scope after scope, outermost first, and the block
/// after folding each prefix of it.
struct Order<'f> {
    locals: Vec<LocalId, &'f Frame>,
    /// `at[position]`: the block after folding `locals[..=position]`, for positions below
    /// `folded`.
    at: Vec<Option<BasicBlockId>, &'f Frame>,
    folded: usize,
    /// The next listing, before it replaces what changed.
    scratch: Vec<LocalId, &'f Frame>,
}

impl<'f> Order<'f> {
    fn new_in(frame: &'f Frame) -> Self {
        Order {
            locals: Vec::new_in(frame),
            at: Vec::new_in(frame),
            folded: 0,
            scratch: Vec::new_in(frame),
        }
    }

    /// Replaces `locals[keep..]` with `scratch`, keeping the folds of the positions it leaves
    /// as they were.
    fn replace_from(&mut self, keep: usize) {
        let common = common_prefix(&self.locals[keep.min(self.locals.len())..], &self.scratch);
        self.folded = self.folded.min(keep + common);
        self.locals.truncate(keep);
        self.locals.extend_from_slice(&self.scratch);
        self.at.resize(self.locals.len(), None);
    }

    fn before(&self, position: usize) -> Option<BasicBlockId> {
        match position.checked_sub(1) {
            Some(last) => self.at[last],
            None => None,
        }
    }
}

/// How many locals `a` and `b` start with in common.
fn common_prefix(a: &[LocalId], b: &[LocalId]) -> usize {
    let len = a.len().min(b.len());
    if a[..len] == b[..len] {
        return len;
    }
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

/// The fold past the end of the listed scopes through the locals of a scope that was popped:
/// from `base`, with `at[i]` the block after the first `i` of `seq`. It holds while no local's
/// dropping changed (`epoch`).
struct Memo<'f> {
    epoch: u32,
    base: Option<BasicBlockId>,
    seq: Vec<LocalId, &'f Frame>,
    at: Vec<Option<BasicBlockId>, &'f Frame>,
}

/// The scopes as last listed, in both orders, and how far each order is folded.
pub(super) struct Unwinds<'f> {
    declared: Order<'f>,
    leaving: Order<'f>,
    /// By listed scope: where its locals start in both orders.
    starts: Vec<u32, &'f Frame>,
    /// By listed scope: whether a pending operand was among its locals when it was listed.
    had_operands: Vec<bool, &'f Frame>,
    /// By local index: its first position in `declared`, or `NONE`.
    first: Vec<u32, &'f Frame>,
    /// By local index: 0 not asked yet, 1 `Copy`, 2 dropped.
    copy: Vec<u8, &'f Frame>,
    /// Whether a fold ends storage on the way out, as in a coroutine.
    storage: bool,
    /// Counts the changes of which locals a fold drops (`unscheduled`, `storage`).
    epoch: u32,
    memo: Memo<'f>,
}

impl<'f> Unwinds<'f> {
    pub(super) fn new_in(frame: &'f Frame) -> Self {
        Unwinds {
            declared: Order::new_in(frame),
            leaving: Order::new_in(frame),
            starts: Vec::new_in(frame),
            had_operands: Vec::new_in(frame),
            first: Vec::new_in(frame),
            copy: Vec::new_in(frame),
            storage: false,
            epoch: 0,
            memo: Memo {
                epoch: 0,
                base: None,
                seq: Vec::new_in(frame),
                at: Vec::new_in(frame),
            },
        }
    }

    /// Whether folds end storage on the way out. Everything is folded again when it changes.
    pub(super) fn sync_storage(&mut self, storage: bool) {
        if self.storage != storage {
            self.storage = storage;
            self.declared.folded = 0;
            self.leaving.folded = 0;
            self.epoch = self.epoch.wrapping_add(1);
        }
    }

    /// Lists the scopes again from the outermost one that changed, or the first one past those
    /// listed. Allocates no block: folding waits until a way out is asked for.
    pub(super) fn list<S: HasLocals<'f>>(&mut self, scopes: &mut [S], operands: &LocalSet<'_>) {
        let listed = self.starts.len();
        let mut depth = 0;
        while depth < listed && depth < scopes.len() && !scopes[depth].locals_mut().stale {
            depth += 1;
        }
        if depth == listed && depth == scopes.len() {
            return;
        }
        let appends = depth < listed
            && depth < scopes.len()
            && scopes[depth].locals_mut().appends(self.had_operands[depth], operands);
        let keep = match (appends, self.starts.get(depth)) {
            (true, _) => self.end_of(depth),
            (false, Some(&start)) => start as usize,
            (false, None) => self.declared.locals.len(),
        };
        self.declared.scratch.clear();
        self.leaving.scratch.clear();
        self.starts.truncate(depth + usize::from(appends));
        self.had_operands.truncate(depth + usize::from(appends));
        let mut rest = &mut scopes[depth..];
        if appends {
            let (scope, inner) = rest.split_first_mut().expect("the scope that was appended to");
            let scope = scope.locals_mut();
            self.declared.scratch.extend_from_slice(&scope.list[scope.listed as usize..]);
            self.leaving.scratch.extend_from_slice(&scope.list[scope.listed as usize..]);
            scope.listed = scope.list.len() as u32;
            scope.stale = false;
            rest = inner;
        }
        for scope in rest {
            let scope = scope.locals_mut();
            self.starts.push((keep + self.declared.scratch.len()) as u32);
            self.had_operands.push(scope.iter().any(|it| operands.contains(it)));
            scope.declared_into(operands, &mut self.declared.scratch);
            scope.leaving_into(operands, &mut self.leaving.scratch);
            scope.listed = scope.list.len() as u32;
            scope.kept = true;
            scope.stale = false;
        }
        for &local in &self.declared.locals[keep.min(self.declared.locals.len())..] {
            let slot = &mut self.first[local_index(local)];
            if *slot as usize >= keep {
                *slot = NONE;
            }
        }
        self.declared.replace_from(keep);
        self.leaving.replace_from(keep);
        for position in keep..self.declared.locals.len() {
            let at = local_index(self.declared.locals[position]);
            if self.first.len() <= at {
                self.first.resize(at + 1, NONE);
            }
            if self.first[at] == NONE {
                self.first[at] = position as u32;
            }
        }
    }

    /// Puts `local` in `operands`: what holds it is in another order from then.
    pub(super) fn mark_operand<S: HasLocals<'f>>(
        &mut self,
        scopes: &mut [S],
        operands: &mut LocalSet<'_>,
        local: LocalId,
    ) {
        if operands.insert(local)
            && let Some(position) = self.first_of(local)
            && let Some(scope) = scopes.get_mut(self.depth_of(position))
        {
            // The outermost listed scope holding it; the ones inside it are listed again with
            // it. One that holds it and has not changed since it was listed holds it at or
            // after its first position.
            let scope = scope.locals_mut();
            scope.stale = true;
            scope.kept = false;
        }
    }

    /// Puts `local` in `unscheduled`: unwinding no longer drops it.
    pub(super) fn unschedule(&mut self, unscheduled: &mut LocalSet<'_>, local: LocalId) {
        if unscheduled.insert(local) {
            self.dropping_changed(local);
        }
    }

    /// Takes `local` out of `unscheduled`: unwinding drops it again.
    pub(super) fn reschedule(&mut self, unscheduled: &mut LocalSet<'_>, local: LocalId) {
        if unscheduled.remove(local) {
            self.dropping_changed(local);
        }
    }

    /// Whether `local` is dropped changed: the folds from the start of the outermost listed
    /// scope holding it go. A scope that gets it after it was listed changes its order there,
    /// so listing it again folds from there.
    fn dropping_changed(&mut self, local: LocalId) {
        self.epoch = self.epoch.wrapping_add(1);
        if let Some(position) = self.first_of(local) {
            let start = self.starts[self.depth_of(position)] as usize;
            self.declared.folded = self.declared.folded.min(start);
            self.leaving.folded = self.leaving.folded.min(start);
        }
    }

    fn first_of(&self, local: LocalId) -> Option<usize> {
        match self.first.get(local_index(local)) {
            Some(&position) if position != NONE => Some(position as usize),
            _ => None,
        }
    }

    /// The listed scope `declared.locals[position]` is in.
    fn depth_of(&self, position: usize) -> usize {
        self.starts.partition_point(|&it| it as usize <= position) - 1
    }

    /// Where the listed scope at `depth` ends in both orders.
    fn end_of(&self, depth: usize) -> usize {
        self.starts.get(depth + 1).map_or(self.declared.locals.len(), |&it| it as usize)
    }

    fn order(&mut self, chain: Chain) -> &mut Order<'f> {
        match chain {
            Chain::Leaving => &mut self.leaving,
            Chain::Declared => &mut self.declared,
        }
    }

    /// The way out from inside every listed scope, through their leaving orders. Call `list`
    /// first.
    pub(super) fn here<T: DropTree>(&mut self, tree: &mut T) -> Option<BasicBlockId> {
        self.fold_to(tree, Chain::Leaving, self.leaving.locals.len())
    }

    /// The way out of the drop of `locals[at]` at the end of a scope that drops `locals`: what
    /// is declared before it, then, in a coroutine, its own storage. A local still in a listed
    /// scope (the scopes `drop_until_scope` and `guard_failed` drop) goes after what is declared
    /// before it there; one of a scope that was popped goes after every listed scope and
    /// `locals[..at]`. Call `list` first.
    pub(super) fn from_drop<T: DropTree>(
        &mut self,
        tree: &mut T,
        locals: &[LocalId],
        at: usize,
    ) -> Option<BasicBlockId> {
        let local = locals[at];
        let unwind = match self.first_of(local) {
            Some(position) => self.fold_to(tree, Chain::Declared, position),
            None => {
                let base = self.fold_to(tree, Chain::Declared, self.declared.locals.len());
                self.fold_popped(tree, base, &locals[..at])
            }
        };
        self.storage_step(tree, unwind, local)
    }

    /// The way out of the drop of `locals[position]` when leaving early through the scope at
    /// `index`, whose leaving order `locals` is: every scope outside it, then
    /// `locals[..position]`, then, in a coroutine, its own storage. Call `list` first.
    pub(super) fn before<T: DropTree>(
        &mut self,
        tree: &mut T,
        index: usize,
        locals: &[LocalId],
        position: usize,
    ) -> Option<BasicBlockId> {
        let start = self.starts[index] as usize;
        debug_assert_eq!(locals, &self.leaving.locals[start..self.end_of(index)]);
        let unwind = self.fold_to(tree, Chain::Leaving, start + position);
        self.storage_step(tree, unwind, locals[position])
    }

    fn storage_step<T: DropTree>(
        &mut self,
        tree: &mut T,
        at: Option<BasicBlockId>,
        local: LocalId,
    ) -> Option<BasicBlockId> {
        match self.storage {
            true => Some(tree.node(at, local, false)),
            false => at,
        }
    }

    /// Folds `chain`'s order up to `upto`; the block after its first `upto` locals.
    fn fold_to<T: DropTree>(
        &mut self,
        tree: &mut T,
        chain: Chain,
        upto: usize,
    ) -> Option<BasicBlockId> {
        let mut position = self.order(chain).folded;
        while position < upto {
            let order = self.order(chain);
            let (at, local) = (order.before(position), order.locals[position]);
            let at = self.step(tree, at, local);
            self.order(chain).at[position] = at;
            position += 1;
        }
        let order = self.order(chain);
        order.folded = order.folded.max(upto);
        order.before(upto)
    }

    /// Folds `seq` from `base`, past what the last such fold shared with it.
    fn fold_popped<T: DropTree>(
        &mut self,
        tree: &mut T,
        base: Option<BasicBlockId>,
        seq: &[LocalId],
    ) -> Option<BasicBlockId> {
        let memo = &mut self.memo;
        if memo.at.is_empty() || memo.base != base || memo.epoch != self.epoch {
            memo.seq.clear();
            memo.at.clear();
            memo.at.push(base);
            memo.base = base;
            memo.epoch = self.epoch;
        }
        let same = common_prefix(&memo.seq, seq);
        memo.seq.truncate(same);
        memo.at.truncate(same + 1);
        let mut at = memo.at[same];
        for &local in &seq[same..] {
            at = self.step(tree, at, local);
            self.memo.seq.push(local);
            self.memo.at.push(at);
        }
        at
    }

    /// One local of a fold: its storage in a coroutine, then its value unless nothing drops it.
    fn step<T: DropTree>(
        &mut self,
        tree: &mut T,
        mut at: Option<BasicBlockId>,
        local: LocalId,
    ) -> Option<BasicBlockId> {
        if self.storage {
            at = Some(tree.node(at, local, false));
        }
        if !self.is_copy(tree, local) && !tree.unscheduled(local) {
            at = Some(tree.node(at, local, true));
        }
        at
    }

    fn is_copy<T: DropTree>(&mut self, tree: &mut T, local: LocalId) -> bool {
        let at = local_index(local);
        if self.copy.len() <= at {
            self.copy.resize(at + 1, 0);
        }
        match self.copy[at] {
            0 => {
                let copy = tree.local_is_copy(local);
                self.copy[at] = if copy { 1 } else { 2 };
                copy
            }
            known => known == 1,
        }
    }
}

impl DropTree for MirLowerCtx<'_, '_, '_> {
    fn node(&mut self, parent: Option<BasicBlockId>, local: LocalId, value: bool) -> BasicBlockId {
        self.unwind_node(parent, local, value)
    }

    fn local_is_copy(&mut self, local: LocalId) -> bool {
        let ty = self.local(local).ty.as_ref();
        self.is_copy(ty)
    }

    fn unscheduled(&self, local: LocalId) -> bool {
        self.unscheduled.contains(&local)
    }
}

impl<'f> MirLowerCtx<'_, 'f, '_> {
    /// The scopes listed, for a fold. `put_unwinds` gives them back.
    fn take_unwinds(&mut self) -> Box<Unwinds<'f>, &'f Frame> {
        let mut unwinds = self.unwinds.take().expect("one unwind path is asked at a time");
        unwinds.sync_storage(self.result.coroutine.is_some());
        unwinds.list(&mut self.drop_scopes[..], &self.operand_locals);
        unwinds
    }

    fn put_unwinds(&mut self, unwinds: Box<Unwinds<'f>, &'f Frame>) {
        self.unwinds = Some(unwinds);
    }

    /// The unwind drop tree's way out from where lowering is: every scope's drops.
    pub(super) fn unwind_here(&mut self) -> Option<BasicBlockId> {
        let mut unwinds = self.take_unwinds();
        let unwind = unwinds.here(self);
        self.put_unwinds(unwinds);
        unwind
    }

    /// The way out of a drop of `locals[at]` of a scope that drops `locals`: what is declared
    /// before it, then, in a coroutine, its own storage.
    pub(super) fn unwind_from_drop(&mut self, locals: &[LocalId], at: usize) -> Option<BasicBlockId> {
        let mut unwinds = self.take_unwinds();
        let unwind = unwinds.from_drop(self, locals, at);
        self.put_unwinds(unwinds);
        unwind
    }

    /// The way out of a drop of `locals[position]` when leaving the scope at `index` early,
    /// `locals` being that scope's leaving order: what every scope outside it declared and
    /// `locals[..position]`, then, in a coroutine, its own storage.
    pub(super) fn unwind_before(
        &mut self,
        index: usize,
        locals: &[LocalId],
        position: usize,
    ) -> Option<BasicBlockId> {
        let mut unwinds = self.take_unwinds();
        let unwind = unwinds.before(self, index, locals, position);
        self.put_unwinds(unwinds);
        unwind
    }

    /// Records that `local` holds an operand of an aggregate or a call being lowered.
    pub(super) fn mark_operand(&mut self, local: LocalId) {
        let unwinds = self.unwinds.as_mut().expect("not inside a fold");
        unwinds.mark_operand(&mut self.drop_scopes[..], &mut self.operand_locals, local);
    }

    /// While an expression is written to the temporary `local`: what unwinds out of it does not
    /// drop `local`'s value (rustc's `as_temp` schedules the drop once the expression is done).
    pub(super) fn unschedule(&mut self, local: LocalId) {
        let unwinds = self.unwinds.as_mut().expect("not inside a fold");
        unwinds.unschedule(&mut self.unscheduled, local);
    }

    /// Once the expression written to `local` is done: unwinding drops it.
    pub(super) fn reschedule(&mut self, local: LocalId) {
        let unwinds = self.unwinds.as_mut().expect("not inside a fold");
        unwinds.reschedule(&mut self.unscheduled, local);
    }
}

#[cfg(test)]
mod tests;
