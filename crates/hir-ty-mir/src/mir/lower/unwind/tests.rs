//! The cached folds of `Unwinds` against the fold over everything at every ask that lowering did
//! before (copied below as it was, over plain lists), on generated sequences of what lowering
//! does to its scopes: nested scopes, temporaries, operands moved last, unscheduled temporaries,
//! tails before 2024, scopes handed outward, drops at the end of scopes, drops of scopes still
//! on the stack (`drop_until_scope`, `guard_failed`) and ways out (`exit_to`), with and without
//! storage ending on the way out. Both must give the same block at every ask, allocate the same
//! blocks in the same order and ask whether a local is `Copy` in the same order.

use blaze_hash::{HashMap, HashSet};
use blaze_mem::{Frame, Tag, frame};
use la_arena::{Idx, RawIdx};

use super::*;
use crate::mir::framed::block_id;

fn local(index: usize) -> LocalId {
    Idx::from_raw(RawIdx::from_u32(index as u32))
}

/// A block lowering allocated for the unwind drop tree.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Made {
    Resume,
    Node(BasicBlockId, LocalId, bool),
}

/// A drop tree that numbers its blocks in the order `unwind_node` and `resume_block` allocate
/// them, logs each, and logs the first time each local's `Copy` is asked.
struct Model<'f> {
    nodes: HashMap<(BasicBlockId, LocalId, bool), BasicBlockId>,
    resume: Option<BasicBlockId>,
    made: Vec<Made>,
    copy: Vec<bool>,
    asked: Vec<LocalId>,
    asked_set: HashSet<LocalId>,
    unscheduled: LocalSet<'f>,
    /// Every `node` call, made or found.
    probes: usize,
}

impl<'f> Model<'f> {
    fn new(frame: &'f Frame) -> Self {
        Model {
            nodes: HashMap::default(),
            resume: None,
            made: Vec::new(),
            copy: Vec::new(),
            asked: Vec::new(),
            asked_set: HashSet::default(),
            unscheduled: LocalSet::new_in(frame),
            probes: 0,
        }
    }

    /// The next block, numbered as lowering numbers them.
    fn block(&self) -> BasicBlockId {
        block_id(self.made.len())
    }
}

impl DropTree for Model<'_> {
    fn node(&mut self, parent: Option<BasicBlockId>, local: LocalId, value: bool) -> BasicBlockId {
        self.probes += 1;
        let parent = match (parent, self.resume) {
            (Some(it), _) | (None, Some(it)) => it,
            (None, None) => {
                let resume = self.block();
                self.made.push(Made::Resume);
                self.resume = Some(resume);
                resume
            }
        };
        if let Some(&node) = self.nodes.get(&(parent, local, value)) {
            return node;
        }
        let node = self.block();
        self.made.push(Made::Node(parent, local, value));
        self.nodes.insert((parent, local, value), node);
        node
    }

    fn local_is_copy(&mut self, local: LocalId) -> bool {
        if self.asked_set.insert(local) {
            self.asked.push(local);
        }
        self.copy[local_index(local)]
    }

    fn unscheduled(&self, local: LocalId) -> bool {
        self.unscheduled.contains(&local)
    }
}

// The fold as lowering did it before `Unwinds`, over plain lists.

struct OldScope {
    locals: Vec<LocalId>,
    tail: Option<usize>,
}

fn old_in_order(scope: &OldScope, operands: &HashSet<LocalId>) -> Vec<LocalId> {
    match scope.tail {
        Some(declared) if declared < scope.locals.len() => {
            let (operands, temporaries): (Vec<LocalId>, Vec<LocalId>) =
                scope.locals[declared..].iter().partition(|it| operands.contains(*it));
            [temporaries, scope.locals[..declared].to_vec(), operands].concat()
        }
        _ => scope.locals.clone(),
    }
}

fn old_leaving_order(scope: &OldScope, operands: &HashSet<LocalId>) -> Vec<LocalId> {
    let (pending, mut rest): (Vec<LocalId>, Vec<LocalId>) =
        old_in_order(scope, operands).iter().partition(|it| operands.contains(*it));
    rest.extend(pending);
    rest
}

fn old_in_scope(scopes: &[OldScope], operands: &HashSet<LocalId>) -> Vec<LocalId> {
    scopes.iter().flat_map(|it| old_in_order(it, operands)).collect()
}

fn old_unwind_through(
    tree: &mut Model<'_>,
    storage: bool,
    locals: &[LocalId],
    storage_of: Option<LocalId>,
) -> Option<BasicBlockId> {
    let mut at = None;
    for &local in locals {
        if storage {
            at = Some(tree.node(at, local, false));
        }
        if !tree.local_is_copy(local) && !tree.unscheduled(local) {
            at = Some(tree.node(at, local, true));
        }
    }
    if let (true, Some(local)) = (storage, storage_of) {
        at = Some(tree.node(at, local, false));
    }
    at
}

/// The old world: lists, a hash set of operands, and the fold over everything at every ask.
struct Old<'f> {
    scopes: Vec<OldScope>,
    operands: HashSet<LocalId>,
    tree: Model<'f>,
    storage: bool,
}

impl Old<'_> {
    fn here(&mut self) -> Option<BasicBlockId> {
        let in_scope: Vec<LocalId> =
            self.scopes.iter().flat_map(|it| old_leaving_order(it, &self.operands)).collect();
        old_unwind_through(&mut self.tree, self.storage, &in_scope, None)
    }

    fn from_drop(&mut self, locals: &[LocalId], at: usize) -> Option<BasicBlockId> {
        let local = locals[at];
        let mut in_scope = old_in_scope(&self.scopes, &self.operands);
        match in_scope.iter().position(|&it| it == local) {
            Some(position) => in_scope.truncate(position),
            None => in_scope.extend_from_slice(&locals[..at]),
        }
        old_unwind_through(&mut self.tree, self.storage, &in_scope, Some(local))
    }

    fn before(&mut self, index: usize, locals: &[LocalId], position: usize) -> Option<BasicBlockId> {
        let mut before: Vec<LocalId> =
            (0..index).flat_map(|outer| old_leaving_order(&self.scopes[outer], &self.operands)).collect();
        before.extend_from_slice(&locals[..position]);
        old_unwind_through(&mut self.tree, self.storage, &before, Some(locals[position]))
    }
}

/// The new world: `ScopeLocals`, a `LocalSet` of operands, and `Unwinds`.
struct New<'f> {
    frame: &'f Frame,
    scopes: Vec<ScopeLocals<'f>>,
    operands: LocalSet<'f>,
    tree: Model<'f>,
    unwinds: Unwinds<'f>,
    storage: bool,
}

impl<'f> New<'f> {
    fn listed(&mut self) {
        self.unwinds.sync_storage(self.storage);
        self.unwinds.list(&mut self.scopes[..], &self.operands);
    }

    fn here(&mut self) -> Option<BasicBlockId> {
        self.listed();
        self.unwinds.here(&mut self.tree)
    }

    fn from_drop(&mut self, locals: &[LocalId], at: usize) -> Option<BasicBlockId> {
        self.listed();
        self.unwinds.from_drop(&mut self.tree, locals, at)
    }

    fn before(&mut self, index: usize, locals: &[LocalId], position: usize) -> Option<BasicBlockId> {
        self.listed();
        self.unwinds.before(&mut self.tree, index, locals, position)
    }

    fn declared(&self, depth: usize) -> Vec<LocalId, &'f Frame> {
        let mut out = Vec::new_in(self.frame);
        self.scopes[depth].declared_into(&self.operands, &mut out);
        out
    }

    fn leaving(&self, depth: usize) -> Vec<LocalId, &'f Frame> {
        let mut out = Vec::new_in(self.frame);
        self.scopes[depth].leaving_into(&self.operands, &mut out);
        out
    }
}

/// xorshift64*: the same sequence on every machine.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }
}

/// Both worlds, driven by the same operations; every answer and every log is compared.
struct Both<'f> {
    old: Old<'f>,
    new: New<'f>,
    locals: usize,
    /// The temporaries being written to, innermost last.
    writing: Vec<LocalId>,
}

impl<'f> Both<'f> {
    fn new(frame: &'f Frame, storage: bool) -> Self {
        let mut both = Both {
            old: Old {
                scopes: Vec::new(),
                operands: HashSet::default(),
                tree: Model::new(frame),
                storage,
            },
            new: New {
                frame,
                scopes: Vec::new(),
                operands: LocalSet::new_in(frame),
                tree: Model::new(frame),
                unwinds: Unwinds::new_in(frame),
                storage,
            },
            locals: 0,
            writing: Vec::new(),
        };
        both.push_scope();
        both
    }

    fn check(&self, what: &str, old: Option<BasicBlockId>, new: Option<BasicBlockId>) {
        assert_eq!(old, new, "{what}: the block");
        assert_eq!(self.old.tree.made, self.new.tree.made, "{what}: the blocks made");
        assert_eq!(self.old.tree.asked, self.new.tree.asked, "{what}: the `Copy` asks");
    }

    fn depth(&self) -> usize {
        self.old.scopes.len()
    }

    fn push_scope(&mut self) {
        self.old.scopes.push(OldScope { locals: Vec::new(), tail: None });
        self.new.scopes.push(ScopeLocals::new_in(self.new.frame));
    }

    /// A new local, or one declared before, scheduled in the scope at `depth`.
    fn schedule(&mut self, rng: &mut Rng, depth: usize) {
        let local = if self.locals > 0 && rng.chance(10) {
            local(rng.below(self.locals))
        } else {
            let copy = rng.chance(40);
            self.old.tree.copy.push(copy);
            self.new.tree.copy.push(copy);
            self.locals += 1;
            local(self.locals - 1)
        };
        let old = &mut self.old.scopes[depth].locals;
        if !old.contains(&local) {
            old.push(local);
        }
        let new = &mut self.new.scopes[depth];
        if !new.holds(local) {
            new.push_new(local);
        }
        assert_eq!(self.old.scopes[depth].locals[..], self.new.scopes[depth][..], "schedule");
    }

    /// The scope at the top is left at its end: popped, then each value dropped, the last
    /// declared first, each drop with its way out.
    fn pop_scope(&mut self) {
        let old = self.old.scopes.pop().unwrap();
        let new = self.new.scopes.pop().unwrap();
        let old_locals = old_in_order(&old, &self.old.operands);
        let mut new_locals = Vec::new_in(self.new.frame);
        new.declared_into(&self.new.operands, &mut new_locals);
        assert_eq!(old_locals[..], new_locals[..], "the order a scope's end drops in");
        for at in (0..old_locals.len()).rev() {
            let old_copy = self.old.tree.local_is_copy(old_locals[at]);
            let new_copy = self.new.tree.local_is_copy(new_locals[at]);
            assert_eq!(old_copy, new_copy);
            if old_copy {
                continue;
            }
            let old = self.old.from_drop(&old_locals, at);
            let new = self.new.from_drop(&new_locals, at);
            self.check("a drop at a scope's end", old, new);
        }
    }

    /// The scopes from `index` in are dropped while still on the stack, innermost first, as
    /// `drop_until_scope` does for `break` and `return` without an exit tree.
    fn drop_until(&mut self, index: usize) {
        for depth in (index..self.depth()).rev() {
            let old_locals = old_in_order(&self.old.scopes[depth], &self.old.operands);
            let new_locals = self.new.declared(depth);
            assert_eq!(old_locals[..], new_locals[..], "the order `drop_until_scope` drops in");
            for at in (0..old_locals.len()).rev() {
                if self.old.tree.local_is_copy(old_locals[at]) {
                    self.new.tree.local_is_copy(new_locals[at]);
                    continue;
                }
                self.new.tree.local_is_copy(new_locals[at]);
                let old = self.old.from_drop(&old_locals, at);
                let new = self.new.from_drop(&new_locals, at);
                self.check("a drop of a scope on the stack", old, new);
            }
        }
    }

    /// What a guard bound after `bound`, in the innermost scope, is dropped where the guard
    /// failed, as `guard_failed` does.
    fn guard_failed(&mut self, rng: &mut Rng) {
        let depth = self.depth() - 1;
        let len = self.old.scopes[depth].locals.len();
        let bound = rng.below(len + 1);
        let old_locals = self.old.scopes[depth].locals[bound..].to_vec();
        let by_guard = ScopeLocals::from_slice_in(&self.new.scopes[depth][bound..], self.new.frame);
        let mut new_locals = Vec::new_in(self.new.frame);
        by_guard.declared_into(&self.new.operands, &mut new_locals);
        assert_eq!(old_locals[..], new_locals[..]);
        for at in (0..old_locals.len()).rev() {
            if self.old.tree.local_is_copy(old_locals[at]) {
                self.new.tree.local_is_copy(new_locals[at]);
                continue;
            }
            self.new.tree.local_is_copy(new_locals[at]);
            let old = self.old.from_drop(&old_locals, at);
            let new = self.new.from_drop(&new_locals, at);
            self.check("a drop where a guard failed", old, new);
        }
    }

    /// The ways out of each drop of leaving the scopes from `index` in early, as `exit_to`
    /// asks for them.
    fn exit(&mut self, index: usize) {
        for depth in index..self.depth() {
            let old_locals = old_leaving_order(&self.old.scopes[depth], &self.old.operands);
            let new_locals = self.new.leaving(depth);
            assert_eq!(old_locals[..], new_locals[..], "the order `exit_to` drops in");
            for position in 0..old_locals.len() {
                if self.old.tree.local_is_copy(old_locals[position]) {
                    self.new.tree.local_is_copy(new_locals[position]);
                    continue;
                }
                self.new.tree.local_is_copy(new_locals[position]);
                let old = self.old.before(depth, &old_locals, position);
                let new = self.new.before(depth, &new_locals, position);
                self.check("a drop on a way out", old, new);
            }
        }
    }

    fn step(&mut self, rng: &mut Rng) {
        let depth = self.depth();
        match rng.below(20) {
            0..=3 => self.schedule(rng, depth - 1),
            4 => {
                let to = rng.below(depth);
                self.schedule(rng, to)
            }
            5 if depth < 8 => self.push_scope(),
            6 if depth > 1 => self.pop_scope(),
            7 if self.locals > 0 => {
                let local = local(rng.below(self.locals));
                self.old.operands.insert(local);
                self.new.unwinds.mark_operand(&mut self.new.scopes[..], &mut self.new.operands, local);
            }
            8 if self.locals > 0 => {
                let local = local(rng.below(self.locals));
                if !self.writing.contains(&local) {
                    self.writing.push(local);
                    self.old.tree.unscheduled.insert(local);
                    self.new.unwinds.unschedule(&mut self.new.tree.unscheduled, local);
                }
            }
            9 => {
                if let Some(local) = self.writing.pop() {
                    self.old.tree.unscheduled.remove(local);
                    self.new.unwinds.reschedule(&mut self.new.tree.unscheduled, local);
                }
            }
            10 => {
                let old = self.old.scopes.last_mut().unwrap();
                old.tail = match old.tail {
                    None => Some(old.locals.len()),
                    Some(_) => None,
                };
                let tail = old.tail;
                self.new.scopes.last_mut().unwrap().set_tail(tail);
            }
            11 => {
                // `declare_last`: a temporary is dropped before those made while computing it.
                let old = &mut self.old.scopes[depth - 1].locals;
                if !old.is_empty() {
                    let local = old[rng.below(old.len())];
                    old.retain(|&it| it != local);
                    old.push(local);
                    let new = self.new.scopes[depth - 1].edit();
                    new.retain(|&it| it != local);
                    new.push(local);
                }
            }
            12 if depth > 1 => {
                // `outlive_innermost_scope`.
                let inner = std::mem::take(&mut self.old.scopes[depth - 1].locals);
                self.old.scopes[depth - 2].locals.extend(inner);
                let (outer, inner) = self.new.scopes.split_at_mut(depth - 1);
                outer[depth - 2].edit().append(inner[0].edit());
            }
            13 => {
                // `declare_operands_last`: the operands since `declared` go last.
                let len = self.old.scopes[depth - 1].locals.len();
                let declared = rng.below(len + 1);
                let operands = &self.old.operands;
                let old = &mut self.old.scopes[depth - 1].locals;
                let (moved, kept): (Vec<LocalId>, Vec<LocalId>) =
                    old[declared..].iter().partition(|it| operands.contains(*it));
                old.truncate(declared);
                old.extend(kept);
                old.extend(moved);
                let operands = &self.new.operands;
                let new = self.new.scopes[depth - 1].edit();
                let since: Vec<LocalId> = new[declared..].to_vec();
                new.truncate(declared);
                new.extend(since.iter().filter(|it| !operands.contains(it)));
                new.extend(since.iter().filter(|it| operands.contains(it)));
            }
            14..=16 => {
                let old = self.old.here();
                let new = self.new.here();
                self.check("a statement that may panic", old, new);
            }
            17 => self.drop_until(rng.below(depth)),
            18 => self.exit(rng.below(depth)),
            19 => self.guard_failed(rng),
            _ => {
                let old = self.old.here();
                let new = self.new.here();
                self.check("a call", old, new);
            }
        }
        assert_eq!(self.old.scopes.len(), self.new.scopes.len());
        for (old, new) in self.old.scopes.iter().zip(&self.new.scopes) {
            assert_eq!(old.locals[..], new[..]);
            assert_eq!(old.tail, new.tail());
        }
    }
}

#[test]
fn the_cached_folds_are_the_folds_over_everything() {
    for seed in 1..=400u64 {
        frame::task(Tag::Mir, |frame| {
            let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
            let storage = rng.chance(30);
            let mut both = Both::new(frame, storage);
            for _ in 0..300 {
                both.step(&mut rng);
            }
            while both.depth() > 1 {
                both.pop_scope();
            }
            both.pop_scope();
        });
    }
}

#[test]
fn storage_changing_midway_folds_everything_again() {
    frame::task(Tag::Mir, |frame| {
        let mut rng = Rng(7);
        let mut both = Both::new(frame, false);
        for round in 0..400 {
            if round % 100 == 50 {
                both.old.storage = !both.old.storage;
                both.new.storage = !both.new.storage;
            }
            both.step(&mut rng);
        }
    });
}

#[test]
fn asking_again_with_nothing_changed_probes_nothing() {
    frame::task(Tag::Mir, |frame| {
        let mut new = New {
            frame,
            scopes: Vec::new(),
            operands: LocalSet::new_in(frame),
            tree: Model::new(frame),
            unwinds: Unwinds::new_in(frame),
            storage: false,
        };
        for depth in 0..6 {
            new.scopes.push(ScopeLocals::new_in(frame));
            for _ in 0..5 {
                new.tree.copy.push(false);
                let at = new.tree.copy.len() - 1;
                new.scopes[depth].push_new(local(at));
            }
        }
        let first = new.here();
        let probes = new.tree.probes;
        assert_eq!(new.here(), first);
        assert_eq!(new.tree.probes, probes);
    });
}

/// A block of `n` `let`s with a call after each: the old fold probes n² / 2 times.
#[test]
fn a_long_block_probed_quadratically_before() {
    const N: usize = 200;
    frame::task(Tag::Mir, |frame| {
        let mut old = Old {
            scopes: vec![
                OldScope { locals: Vec::new(), tail: None },
                OldScope { locals: Vec::new(), tail: None },
            ],
            operands: HashSet::default(),
            tree: Model::new(frame),
            storage: false,
        };
        for at in 0..N {
            old.tree.copy.push(false);
            old.scopes[1].locals.push(local(at));
            old.here();
        }
        assert_eq!(old.tree.probes, N * (N + 1) / 2);
    });
}

/// A binding moved into a call after each `let`, as `let b = f(a);` does. Operands go last in a
/// scope's leaving order, so each new `let` goes before every moved binding and the way out
/// through them is new blocks each time: lowering's output is quadratic there, and the cached
/// fold probes only for the blocks it makes, plus at most one a call.
#[test]
fn a_moved_binding_probes_only_for_new_blocks() {
    const N: usize = 500;
    frame::task(Tag::Mir, |frame| {
        let mut new = New {
            frame,
            scopes: Vec::new(),
            operands: LocalSet::new_in(frame),
            tree: Model::new(frame),
            unwinds: Unwinds::new_in(frame),
            storage: false,
        };
        new.scopes.push(ScopeLocals::new_in(frame));
        for at in 0..N {
            new.tree.copy.push(false);
            new.scopes[0].push_new(local(at));
            if at > 0 {
                new.unwinds.mark_operand(&mut new.scopes[..], &mut new.operands, local(at - 1));
            }
            new.here();
        }
        let made = new.tree.made.len();
        assert!(new.tree.probes <= made + N, "{} probes, {made} blocks", new.tree.probes);
    });
}

/// The same block as `a_long_block_probed_quadratically_before`, cached: one probe a local.
#[test]
fn a_long_block_probes_once_a_local() {
    const N: usize = 2000;
    frame::task(Tag::Mir, |frame| {
        let mut new = New {
            frame,
            scopes: Vec::new(),
            operands: LocalSet::new_in(frame),
            tree: Model::new(frame),
            unwinds: Unwinds::new_in(frame),
            storage: false,
        };
        new.scopes.push(ScopeLocals::new_in(frame));
        new.scopes.push(ScopeLocals::new_in(frame));
        for at in 0..N {
            new.tree.copy.push(false);
            new.scopes[1].push_new(local(at));
            // The statement's temporaries' scope, made and left empty.
            new.scopes.push(ScopeLocals::new_in(frame));
            new.here();
            new.scopes.pop();
        }
        assert!(new.tree.probes <= N, "{} probes for {N} locals", new.tree.probes);
    });
}

#[test]
fn a_new_local_is_held_without_a_scan() {
    frame::task(Tag::Mir, |frame| {
        let mut scope = ScopeLocals::new_in(frame);
        for at in 0..100 {
            assert!(!scope.holds(local(at)));
            scope.push_new(local(at));
            assert!(scope.holds(local(at)));
        }
        scope.edit().retain(|&it| local_index(it) % 2 == 0);
        for at in 0..100 {
            assert_eq!(scope.holds(local(at)), at % 2 == 0);
        }
        assert!(!scope.holds(local(1000)));
    });
}

#[test]
fn local_sets() {
    frame::task(Tag::Mir, |frame| {
        let mut set = LocalSet::new_in(frame);
        assert!(!set.contains(&local(5)));
        assert!(set.insert(local(5)));
        assert!(!set.insert(local(5)));
        assert!(set.insert(local(200)));
        assert!(set.contains(&local(5)) && set.contains(&local(200)));
        assert!(!set.contains(&local(6)) && !set.contains(&local(10_000)));
        assert!(set.remove(local(5)));
        assert!(!set.remove(local(5)));
        assert!(!set.contains(&local(5)));
        assert!(!set.remove(local(10_000)));
    });
}
