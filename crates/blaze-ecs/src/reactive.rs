//! Glitch-free push-pull fine-grained reactivity (`Signal<T>`, `Memo<T>`, `EffectId`) and
//! DBSP-style $O(1)$-per-delta incremental collection helpers (`IncrementalCount`,
//! `IncrementalFilter`).
//!
//! Designed as the runtime target for both run-once Rust UI components with reactive holes
//! (Solid / Alien-Signals / Svelte 5 Runes semantics) and future Blaze compiler-tracked scopes:
//! - `#![no_std]`, `#![forbid(unsafe_code)]`, `#![blaze::sans_io]`, explicit `Allocator`
//! - 100% iterative (zero native recursion)
//! - Equality-guarded writes (`set` on equal value = $O(1)$ no-op, zero downstream propagation)
//! - Glitch-free diamond resolution with `Clean` / `Check` / `Dirty` push-pull short-circuiting

use core::alloc::Allocator;
use core::any::Any;
use core::marker::PhantomData;

use blaze_own::{Box as OwnBox, Vec as OwnVec};

use super::error::EcsError;
use super::id::{u32_to_usize, usize_to_u32};
use super::map::FlatMap;

/// Compact 32-bit arena handle for any reactive graph node.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId(pub u32);

/// Lightweight `Copy` typed handle to a reactive state cell (`$state` equivalent).
pub struct Signal<T: 'static> {
    id: NodeId,
    _marker: PhantomData<fn() -> T>,
}

impl<T: 'static> Clone for Signal<T> {
    #[inline]
    fn clone(&self) -> Self {
        *self
    }
}
impl<T: 'static> Copy for Signal<T> {}
impl<T: 'static> PartialEq for Signal<T> {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}
impl<T: 'static> Eq for Signal<T> {}

impl<T: 'static> Signal<T> {
    #[inline]
    pub const fn node_id(self) -> NodeId {
        self.id
    }
}

/// Lightweight `Copy` typed handle to a glitch-free derived memo (`$derived` equivalent).
pub struct Memo<T: 'static> {
    id: NodeId,
    _marker: PhantomData<fn() -> T>,
}

impl<T: 'static> Clone for Memo<T> {
    #[inline]
    fn clone(&self) -> Self {
        *self
    }
}
impl<T: 'static> Copy for Memo<T> {}
impl<T: 'static> PartialEq for Memo<T> {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}
impl<T: 'static> Eq for Memo<T> {}

impl<T: 'static> Memo<T> {
    #[inline]
    pub const fn node_id(self) -> NodeId {
        self.id
    }
}

/// Handle to a reactive side-effect / UI reactive hole (`$effect` equivalent).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct EffectId(pub NodeId);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NodeKind {
    Signal,
    Memo,
    Effect,
    Disposed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DirtyState {
    Clean,
    Check,
    Dirty,
}

trait AnyValueCell {
    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

struct TypedCell<T: 'static> {
    val: T,
}

impl<T: 'static> AnyValueCell for TypedCell<T> {
    #[inline]
    fn as_any(&self) -> &dyn Any {
        &self.val
    }
    #[inline]
    fn as_any_mut(&mut self) -> &mut dyn Any {
        &mut self.val
    }
}

struct TypedOptionCell<T: 'static> {
    val: Option<T>,
}

impl<T: 'static> AnyValueCell for TypedOptionCell<T> {
    #[inline]
    fn as_any(&self) -> &dyn Any {
        &self.val
    }
    #[inline]
    fn as_any_mut(&mut self) -> &mut dyn Any {
        &mut self.val
    }
}

trait AnyMemoCompute<A: Allocator + Copy> {
    /// Recompute into `cell` and return `true` iff the output value changed (`new != old`).
    fn recompute(&mut self, cx: &mut ReadCx<'_, A>, cell: &mut dyn AnyValueCell) -> bool;
}

struct TypedMemoCompute<T: PartialEq + 'static, F> {
    f: F,
    _marker: PhantomData<fn() -> T>,
}

impl<A: Allocator + Copy, T: PartialEq + 'static, F: FnMut(&mut ReadCx<'_, A>) -> T>
    AnyMemoCompute<A> for TypedMemoCompute<T, F>
{
    fn recompute(&mut self, cx: &mut ReadCx<'_, A>, cell: &mut dyn AnyValueCell) -> bool {
        let next = (self.f)(cx);
        if cx.deferred_dep.is_some() {
            return false;
        }
        if let Some(cur) = cell.as_any_mut().downcast_mut::<Option<T>>() {
            if cur.as_ref() == Some(&next) {
                return false;
            }
            *cur = Some(next);
            return true;
        }
        false
    }
}

trait AnyEffectFn<A: Allocator + Copy> {
    fn run(&mut self, cx: &mut ReadCx<'_, A>);
}

struct TypedEffectFn<F>(F);

impl<A: Allocator + Copy, F: FnMut(&mut ReadCx<'_, A>)> AnyEffectFn<A> for TypedEffectFn<F> {
    fn run(&mut self, cx: &mut ReadCx<'_, A>) {
        (self.0)(cx);
    }
}

struct ReactiveNode<A: Allocator + Copy> {
    kind: NodeKind,
    state: DirtyState,
    /// Bumped strictly when this node's observable output value actually changed.
    value_epoch: u64,
    /// Upstream `(node_id, observed_value_epoch)` dependencies recorded on last run.
    deps: OwnVec<(NodeId, u64), A>,
    /// Downstream subscribers notified when this node's output changes.
    subs: OwnVec<NodeId, A>,
    /// Stored value for `Signal` and `Memo` nodes.
    cell: Option<OwnBox<dyn AnyValueCell, A>>,
    /// Compute function for `Memo` nodes.
    memo_fn: Option<OwnBox<dyn AnyMemoCompute<A>, A>>,
    /// Callback for `Effect` nodes.
    effect_fn: Option<OwnBox<dyn AnyEffectFn<A>, A>>,
}

/// Tracked read context handed to `Memo` compute functions and `Effect` reactive holes.
pub struct ReadCx<'a, A: Allocator + Copy> {
    nodes: &'a [ReactiveNode<A>],
    recorded: &'a mut OwnVec<(NodeId, u64), A>,
    /// Set if a newly branched dynamic dependency was encountered in `Dirty` / `Check` state.
    deferred_dep: Option<NodeId>,
}

impl<'a, A: Allocator + Copy> ReadCx<'a, A> {
    /// Tracked read of a [`Signal<T>`].
    pub fn get<T: 'static>(&mut self, sig: Signal<T>) -> Option<&'a T> {
        let node = self.nodes.get(u32_to_usize(sig.id.0))?;
        if node.kind != NodeKind::Signal {
            return None;
        }
        self.record_dep(sig.id, node.value_epoch);
        node.cell.as_ref()?.as_any().downcast_ref::<T>()
    }

    /// Tracked `Copy` read of a [`Signal<T>`].
    #[inline]
    pub fn val<T: Copy + Default + 'static>(&mut self, sig: Signal<T>) -> T {
        self.get(sig).copied().unwrap_or_default()
    }

    /// Tracked read of a [`Memo<T>`].
    pub fn memo<T: 'static>(&mut self, m: Memo<T>) -> Option<&'a T> {
        let node = self.nodes.get(u32_to_usize(m.id.0))?;
        if node.kind != NodeKind::Memo {
            return None;
        }
        if node.state != DirtyState::Clean && self.deferred_dep.is_none() {
            self.deferred_dep = Some(m.id);
        }
        self.record_dep(m.id, node.value_epoch);
        node.cell.as_ref()?.as_any().downcast_ref::<Option<T>>()?.as_ref()
    }

    /// Tracked `Copy` read of a [`Memo<T>`].
    #[inline]
    pub fn memo_val<T: Copy + Default + 'static>(&mut self, m: Memo<T>) -> T {
        self.memo(m).copied().unwrap_or_default()
    }

    fn record_dep(&mut self, id: NodeId, epoch: u64) {
        for &(existing, _) in self.recorded.as_slice() {
            if existing == id {
                return;
            }
        }
        let _ = self.recorded.try_push((id, epoch));
    }
}

/// Glitch-free push-pull reactive dependency graph with explicit allocator `A`.
pub struct ReactiveGraph<A: Allocator + Copy + 'static> {
    alloc: A,
    nodes: OwnVec<ReactiveNode<A>, A>,
    epoch: u64,
    pending_effects: OwnVec<NodeId, A>,
    eval_stack: OwnVec<(NodeId, bool), A>,
    push_stack: OwnVec<(NodeId, bool), A>,
    deps_scratch: OwnVec<(NodeId, u64), A>,
    /// Cumulative count of `Memo` recomputations (for telemetry & benchmarks).
    pub memo_evals: u64,
    /// Cumulative count of `Effect` executions (for telemetry & benchmarks).
    pub effect_evals: u64,
}

impl<A: Allocator + Copy + 'static> ReactiveGraph<A> {
    pub fn new_in(alloc: A) -> Self {
        Self {
            alloc,
            nodes: OwnVec::new_in(alloc),
            epoch: 1,
            pending_effects: OwnVec::new_in(alloc),
            eval_stack: OwnVec::new_in(alloc),
            push_stack: OwnVec::new_in(alloc),
            deps_scratch: OwnVec::new_in(alloc),
            memo_evals: 0,
            effect_evals: 0,
        }
    }

    #[inline]
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Create a new fine-grained reactive [`Signal<T>`].
    pub fn signal<T: 'static>(&mut self, initial: T) -> Result<Signal<T>, EcsError> {
        let id = NodeId(usize_to_u32(self.nodes.len()).ok_or(EcsError::EntityCapacityExceeded)?);
        let boxed = OwnBox::try_new_in(TypedCell { val: initial }, self.alloc)
            .map_err(|r| EcsError::Alloc(r.why))?;
        let cell: OwnBox<dyn AnyValueCell, A> = boxed;
        self.nodes
            .try_push(ReactiveNode {
                kind: NodeKind::Signal,
                state: DirtyState::Clean,
                value_epoch: self.epoch,
                deps: OwnVec::new_in(self.alloc),
                subs: OwnVec::new_in(self.alloc),
                cell: Some(cell),
                memo_fn: None,
                effect_fn: None,
            })
            .map_err(|r| EcsError::Alloc(r.why))?;
        Ok(Signal { id, _marker: PhantomData })
    }

    /// Untracked read of a [`Signal<T>`].
    pub fn get<T: 'static>(&self, sig: Signal<T>) -> Option<&T> {
        let node = self.nodes.get(u32_to_usize(sig.id.0))?;
        if node.kind != NodeKind::Signal {
            return None;
        }
        node.cell.as_ref()?.as_any().downcast_ref::<T>()
    }

    /// Equality-guarded write to a [`Signal<T>`].
    ///
    /// Returns `Ok(false)` immediately in $O(1)$ with zero propagation when `value == old_value`.
    pub fn set<T: PartialEq + 'static>(
        &mut self,
        sig: Signal<T>,
        value: T,
    ) -> Result<bool, EcsError> {
        let idx = u32_to_usize(sig.id.0);
        let changed = {
            let Some(node) = self.nodes.get_mut(idx) else {
                return Ok(false);
            };
            if node.kind != NodeKind::Signal {
                return Ok(false);
            }
            let Some(cell) = node.cell.as_mut() else {
                return Ok(false);
            };
            let Some(cur) = cell.as_any_mut().downcast_mut::<T>() else {
                return Ok(false);
            };
            if *cur == value {
                return Ok(false);
            }
            *cur = value;
            true
        };
        if changed {
            self.epoch = self.epoch.saturating_add(1);
            if let Some(node) = self.nodes.get_mut(idx) {
                node.value_epoch = self.epoch;
            }
            self.mark_subscribers(sig.id)?;
        }
        Ok(changed)
    }

    /// Mutate a [`Signal<T>`] in place; `f` returns `true` iff the value changed.
    pub fn update<T: 'static, F: FnOnce(&mut T) -> bool>(
        &mut self,
        sig: Signal<T>,
        f: F,
    ) -> Result<bool, EcsError> {
        let idx = u32_to_usize(sig.id.0);
        let changed = {
            let Some(node) = self.nodes.get_mut(idx) else {
                return Ok(false);
            };
            if node.kind != NodeKind::Signal {
                return Ok(false);
            }
            let Some(cell) = node.cell.as_mut() else {
                return Ok(false);
            };
            let Some(cur) = cell.as_any_mut().downcast_mut::<T>() else {
                return Ok(false);
            };
            f(cur)
        };
        if changed {
            self.epoch = self.epoch.saturating_add(1);
            if let Some(node) = self.nodes.get_mut(idx) {
                node.value_epoch = self.epoch;
            }
            self.mark_subscribers(sig.id)?;
        }
        Ok(changed)
    }

    /// Create a glitch-free derived [`Memo<T>`] (`$derived` equivalent) initialized eagerly once.
    pub fn memo<T: PartialEq + 'static, F: FnMut(&mut ReadCx<'_, A>) -> T + 'static>(
        &mut self,
        compute: F,
    ) -> Result<Memo<T>, EcsError> {
        let id = NodeId(usize_to_u32(self.nodes.len()).ok_or(EcsError::EntityCapacityExceeded)?);
        let boxed_cell = OwnBox::try_new_in(TypedOptionCell::<T> { val: None }, self.alloc)
            .map_err(|r| EcsError::Alloc(r.why))?;
        let cell: OwnBox<dyn AnyValueCell, A> = boxed_cell;
        let boxed_fn = OwnBox::try_new_in(
            TypedMemoCompute::<T, F> { f: compute, _marker: PhantomData },
            self.alloc,
        )
        .map_err(|r| EcsError::Alloc(r.why))?;
        let memo_fn: OwnBox<dyn AnyMemoCompute<A>, A> = boxed_fn;

        self.nodes
            .try_push(ReactiveNode {
                kind: NodeKind::Memo,
                state: DirtyState::Dirty,
                value_epoch: 0,
                deps: OwnVec::new_in(self.alloc),
                subs: OwnVec::new_in(self.alloc),
                cell: Some(cell),
                memo_fn: Some(memo_fn),
                effect_fn: None,
            })
            .map_err(|r| EcsError::Alloc(r.why))?;

        self.ensure_fresh(id)?;
        Ok(Memo { id, _marker: PhantomData })
    }

    /// Ensure `memo` is up to date (pulling lazily if `Check` or `Dirty`) and borrow its value.
    pub fn get_memo<T: 'static>(&mut self, memo: Memo<T>) -> Result<Option<&T>, EcsError> {
        self.ensure_fresh(memo.id)?;
        let Some(node) = self.nodes.get(u32_to_usize(memo.id.0)) else {
            return Ok(None);
        };
        if node.kind != NodeKind::Memo {
            return Ok(None);
        }
        Ok(node
            .cell
            .as_ref()
            .and_then(|c| c.as_any().downcast_ref::<Option<T>>())
            .and_then(|opt| opt.as_ref()))
    }

    /// Register a reactive side-effect / UI hole (`$effect`) and run it once immediately to
    /// establish its initial tracked dependencies.
    pub fn effect<F: FnMut(&mut ReadCx<'_, A>) + 'static>(
        &mut self,
        f: F,
    ) -> Result<EffectId, EcsError> {
        let id = NodeId(usize_to_u32(self.nodes.len()).ok_or(EcsError::EntityCapacityExceeded)?);
        let boxed = OwnBox::try_new_in(TypedEffectFn(f), self.alloc)
            .map_err(|r| EcsError::Alloc(r.why))?;
        let effect_fn: OwnBox<dyn AnyEffectFn<A>, A> = boxed;
        self.nodes
            .try_push(ReactiveNode {
                kind: NodeKind::Effect,
                state: DirtyState::Dirty,
                value_epoch: 0,
                deps: OwnVec::new_in(self.alloc),
                subs: OwnVec::new_in(self.alloc),
                cell: None,
                memo_fn: None,
                effect_fn: Some(effect_fn),
            })
            .map_err(|r| EcsError::Alloc(r.why))?;
        let _ = self.run_effect_node(id)?;
        Ok(EffectId(id))
    }

    /// Flush all pending dirty effects in glitch-free order, returning how many effects actually ran.
    pub fn flush(&mut self) -> Result<usize, EcsError> {
        let mut ran: usize = 0;
        let mut guard: usize = 0;
        let limit = self.nodes.len().saturating_mul(4).saturating_add(64);
        while !self.pending_effects.is_empty() && guard < limit {
            guard += 1;
            let mut batch = OwnVec::new_in(self.alloc);
            core::mem::swap(&mut batch, &mut self.pending_effects);
            for &eid in batch.as_slice() {
                if self.run_effect_node(eid)? {
                    ran += 1;
                }
            }
        }
        Ok(ran)
    }

    /// Dispose an effect or memo, cleanly unlinking it from all upstream subscriber lists.
    pub fn dispose(&mut self, id: NodeId) {
        let idx = u32_to_usize(id.0);
        let mut old_deps = OwnVec::new_in(self.alloc);
        if let Some(node) = self.nodes.get_mut(idx) {
            core::mem::swap(&mut old_deps, &mut node.deps);
            node.subs.clear();
            node.cell = None;
            node.memo_fn = None;
            node.effect_fn = None;
            node.kind = NodeKind::Disposed;
            node.state = DirtyState::Clean;
        }
        for &(dep, _) in old_deps.as_slice() {
            if let Some(dn) = self.nodes.get_mut(u32_to_usize(dep.0)) {
                if let Some(pos) = dn.subs.iter().position(|&s| s == id) {
                    let _ = dn.subs.remove_swap(pos);
                }
            }
        }
    }

    /// Iterative push phase: direct subscribers become `Dirty`, transitive subscribers become `Check`.
    fn mark_subscribers(&mut self, root: NodeId) -> Result<(), EcsError> {
        self.push_stack.clear();
        if let Some(node) = self.nodes.get(u32_to_usize(root.0)) {
            for &sub in node.subs.as_slice() {
                self.push_stack.try_push((sub, true)).map_err(|r| EcsError::Alloc(r.why))?;
            }
        }
        while let Some((nid, is_direct)) = self.push_stack.pop() {
            let idx = u32_to_usize(nid.0);
            let Some(node) = self.nodes.get_mut(idx) else { continue };
            if node.kind == NodeKind::Disposed {
                continue;
            }
            let target = if is_direct { DirtyState::Dirty } else { DirtyState::Check };
            let upgraded = match (node.state, target) {
                (DirtyState::Clean, _) => true,
                (DirtyState::Check, DirtyState::Dirty) => true,
                _ => false,
            };
            if !upgraded {
                continue;
            }
            node.state = target;
            if node.kind == NodeKind::Effect {
                self.pending_effects.try_push(nid).map_err(|r| EcsError::Alloc(r.why))?;
            } else {
                // Collect downstream subscribers as transitive `Check`.
                let mut i = 0usize;
                while i < node.subs.len() {
                    if let Some(&sub) = node.subs.get(i) {
                        self.push_stack.try_push((sub, false)).map_err(|r| EcsError::Alloc(r.why))?;
                    }
                    i += 1;
                }
            }
        }
        Ok(())
    }

    /// Iterative pull phase: ensures `target` (and all transitive upstream `Memo`s) are `Clean`.
    fn ensure_fresh(&mut self, target: NodeId) -> Result<(), EcsError> {
        self.eval_stack.clear();
        self.eval_stack.try_push((target, false)).map_err(|r| EcsError::Alloc(r.why))?;

        let mut steps: usize = 0;
        let max_steps = self.nodes.len().saturating_mul(8).saturating_add(64);
        while let Some((nid, visited_deps)) = self.eval_stack.pop() {
            steps += 1;
            if steps > max_steps {
                return Err(EcsError::TraversalBoundExceeded { limit: max_steps });
            }
            let idx = u32_to_usize(nid.0);
            let (kind, state) = match self.nodes.get(idx) {
                Some(n) => (n.kind, n.state),
                None => continue,
            };
            if kind != NodeKind::Memo || state == DirtyState::Clean {
                continue;
            }

            if !visited_deps {
                self.eval_stack.try_push((nid, true)).map_err(|r| EcsError::Alloc(r.why))?;
                if let Some(node) = self.nodes.get(idx) {
                    for &(dep, _) in node.deps.as_slice() {
                        if let Some(dn) = self.nodes.get(u32_to_usize(dep.0)) {
                            if dn.kind == NodeKind::Memo && dn.state != DirtyState::Clean {
                                self.eval_stack
                                    .try_push((dep, false))
                                    .map_err(|r| EcsError::Alloc(r.why))?;
                            }
                        }
                    }
                }
                continue;
            }

            // All previously recorded upstream memos are now Clean; verify whether any changed.
            if state == DirtyState::Check && !self.any_dep_epoch_changed(nid) {
                if let Some(node) = self.nodes.get_mut(idx) {
                    node.state = DirtyState::Clean;
                }
                continue;
            }

            // Recompute this memo (may discover a newly branched dirty memo -> defer & retry once).
            let mut memo_fn = match self.nodes.get_mut(idx).and_then(|n| n.memo_fn.take()) {
                Some(f) => f,
                None => continue,
            };
            let mut cell = match self.nodes.get_mut(idx).and_then(|n| n.cell.take()) {
                Some(c) => c,
                None => {
                    if let Some(n) = self.nodes.get_mut(idx) {
                        n.memo_fn = Some(memo_fn);
                    }
                    continue;
                }
            };

            self.deps_scratch.clear();
            let (value_changed, deferred) = {
                let mut cx = ReadCx {
                    nodes: self.nodes.as_slice(),
                    recorded: &mut self.deps_scratch,
                    deferred_dep: None,
                };
                let ch = memo_fn.recompute(&mut cx, cell.as_mut());
                (ch, cx.deferred_dep)
            };

            if let Some(n) = self.nodes.get_mut(idx) {
                n.memo_fn = Some(memo_fn);
                n.cell = Some(cell);
            }

            if let Some(missing_dep) = deferred {
                self.eval_stack.try_push((nid, true)).map_err(|r| EcsError::Alloc(r.why))?;
                self.eval_stack
                    .try_push((missing_dep, false))
                    .map_err(|r| EcsError::Alloc(r.why))?;
                continue;
            }

            self.memo_evals = self.memo_evals.saturating_add(1);
            self.relink_deps(nid)?;

            if value_changed {
                self.epoch = self.epoch.saturating_add(1);
                if let Some(n) = self.nodes.get_mut(idx) {
                    n.value_epoch = self.epoch;
                    n.state = DirtyState::Clean;
                }
            } else if let Some(n) = self.nodes.get_mut(idx) {
                if n.value_epoch == 0 {
                    n.value_epoch = self.epoch;
                }
                n.state = DirtyState::Clean;
            }
        }
        Ok(())
    }

    fn run_effect_node(&mut self, eid: NodeId) -> Result<bool, EcsError> {
        let idx = u32_to_usize(eid.0);
        let (kind, state) = match self.nodes.get(idx) {
            Some(n) => (n.kind, n.state),
            None => return Ok(false),
        };
        if kind != NodeKind::Effect || state == DirtyState::Clean {
            return Ok(false);
        }

        // First ensure all previously recorded upstream memos are fresh.
        let mut upstream = OwnVec::new_in(self.alloc);
        if let Some(node) = self.nodes.get(idx) {
            for &(dep, _) in node.deps.as_slice() {
                upstream.try_push(dep).map_err(|r| EcsError::Alloc(r.why))?;
            }
        }
        for &dep in upstream.as_slice() {
            self.ensure_fresh(dep)?;
        }

        // Short-circuit if state was only `Check` and no upstream value_epoch changed!
        if state == DirtyState::Check && !self.any_dep_epoch_changed(eid) {
            if let Some(n) = self.nodes.get_mut(idx) {
                n.state = DirtyState::Clean;
            }
            return Ok(false);
        }

        let mut retries = 0usize;
        while retries < 8 {
            retries += 1;
            let mut effect_fn = match self.nodes.get_mut(idx).and_then(|n| n.effect_fn.take()) {
                Some(f) => f,
                None => return Ok(false),
            };
            self.deps_scratch.clear();
            let deferred = {
                let mut cx = ReadCx {
                    nodes: self.nodes.as_slice(),
                    recorded: &mut self.deps_scratch,
                    deferred_dep: None,
                };
                effect_fn.run(&mut cx);
                cx.deferred_dep
            };
            if let Some(n) = self.nodes.get_mut(idx) {
                n.effect_fn = Some(effect_fn);
            }
            if let Some(dep) = deferred {
                self.ensure_fresh(dep)?;
                continue;
            }
            self.effect_evals = self.effect_evals.saturating_add(1);
            self.relink_deps(eid)?;
            if let Some(n) = self.nodes.get_mut(idx) {
                n.state = DirtyState::Clean;
            }
            return Ok(true);
        }
        Ok(false)
    }

    fn any_dep_epoch_changed(&self, nid: NodeId) -> bool {
        let Some(node) = self.nodes.get(u32_to_usize(nid.0)) else {
            return false;
        };
        for &(dep, observed_epoch) in node.deps.as_slice() {
            if let Some(dn) = self.nodes.get(u32_to_usize(dep.0)) {
                if dn.value_epoch != observed_epoch {
                    return true;
                }
            }
        }
        false
    }

    fn relink_deps(&mut self, nid: NodeId) -> Result<(), EcsError> {
        let idx = u32_to_usize(nid.0);
        // Unlink previous dependencies.
        let mut old_deps = OwnVec::new_in(self.alloc);
        if let Some(node) = self.nodes.get_mut(idx) {
            core::mem::swap(&mut old_deps, &mut node.deps);
        }
        for &(dep, _) in old_deps.as_slice() {
            if let Some(dn) = self.nodes.get_mut(u32_to_usize(dep.0)) {
                if let Some(pos) = dn.subs.iter().position(|&s| s == nid) {
                    let _ = dn.subs.remove_swap(pos);
                }
            }
        }
        // Record & link new dependencies.
        let mut new_deps = OwnVec::try_with_capacity_in(self.deps_scratch.len(), self.alloc)?;
        for &pair in self.deps_scratch.as_slice() {
            new_deps.try_push(pair).map_err(|r| EcsError::Alloc(r.why))?;
        }
        for &(dep, _) in new_deps.as_slice() {
            if let Some(dn) = self.nodes.get_mut(u32_to_usize(dep.0)) {
                if !dn.subs.iter().any(|&s| s == nid) {
                    dn.subs.try_push(nid).map_err(|r| EcsError::Alloc(r.why))?;
                }
            }
        }
        if let Some(node) = self.nodes.get_mut(idx) {
            node.deps = new_deps;
        }
        Ok(())
    }
}

// =============================================================================
// DBSP-Style O(1)-Per-Delta Incremental Collection Aggregates
// =============================================================================

/// $O(1)$-per-delta incremental predicate counter backed by a [`Signal<usize>`].
///
/// Unlike `Memo::new(|| rows.iter().filter(|r| r.unread).count())` which scans $O(N)$ rows on
/// every write, [`IncrementalCount`] maintains the exact Z-set delta per key in $O(1)$!
pub struct IncrementalCount<K: Ord + Copy, A: Allocator + Copy + 'static> {
    flags: FlatMap<K, bool, A>,
    count: usize,
    signal: Signal<usize>,
}

impl<K: Ord + Copy, A: Allocator + Copy + 'static> IncrementalCount<K, A> {
    pub fn new_in(graph: &mut ReactiveGraph<A>, alloc: A) -> Result<Self, EcsError> {
        let signal = graph.signal(0usize)?;
        Ok(Self { flags: FlatMap::new_in(alloc), count: 0, signal })
    }

    #[inline]
    pub const fn signal(&self) -> Signal<usize> {
        self.signal
    }

    #[inline]
    pub const fn count(&self) -> usize {
        self.count
    }

    /// Return whether `key` currently matches the predicate (`false` if absent).
    #[inline]
    pub fn get(&self, key: K) -> bool {
        self.flags.get(&key).copied().unwrap_or(false)
    }

    /// Update `key`'s predicate membership in $O(\log N)$ (or $O(1)$ on unchanged value) and push
    /// the new total to [`Self::signal`] only if the count actually changed.
    pub fn upsert(
        &mut self,
        graph: &mut ReactiveGraph<A>,
        key: K,
        matches: bool,
    ) -> Result<bool, EcsError> {
        let prev_opt = self.flags.get(&key).copied();
        let prev = prev_opt.unwrap_or(false);
        if prev == matches && prev_opt.is_some() {
            return Ok(false);
        }
        let _ = self.flags.insert(key, matches)?;
        match (prev, matches) {
            (false, true) => self.count = self.count.saturating_add(1),
            (true, false) => self.count = self.count.saturating_sub(1),
            _ => return Ok(false),
        }
        graph.set(self.signal, self.count)
    }

    /// Remove `key` and decrement `count` in $O(\log N)$ if it previously matched.
    pub fn remove(&mut self, graph: &mut ReactiveGraph<A>, key: K) -> Result<bool, EcsError> {
        let Some(prev) = self.flags.remove(&key) else {
            return Ok(false);
        };
        if prev {
            self.count = self.count.saturating_sub(1);
            return graph.set(self.signal, self.count);
        }
        Ok(false)
    }
}

/// $O(1)$-per-delta incremental filtered key list (maintains a dense matched-key slice with
/// swap-remove on exit and push on enter).
pub struct IncrementalFilter<K: Ord + Copy, A: Allocator + Copy + 'static> {
    pos_by_key: FlatMap<K, usize, A>,
    matched: OwnVec<K, A>,
    version: Signal<u64>,
}

impl<K: Ord + Copy, A: Allocator + Copy + 'static> IncrementalFilter<K, A> {
    pub fn new_in(graph: &mut ReactiveGraph<A>, alloc: A) -> Result<Self, EcsError> {
        let version = graph.signal(0u64)?;
        Ok(Self {
            pos_by_key: FlatMap::new_in(alloc),
            matched: OwnVec::new_in(alloc),
            version,
        })
    }

    #[inline]
    pub const fn version_signal(&self) -> Signal<u64> {
        self.version
    }

    #[inline]
    pub fn matched(&self) -> &[K] {
        self.matched.as_slice()
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.matched.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.matched.is_empty()
    }

    /// Incrementally add or remove `key` from the filtered output in $O(\log N)$ without scanning.
    pub fn set_match(
        &mut self,
        graph: &mut ReactiveGraph<A>,
        key: K,
        matches: bool,
    ) -> Result<bool, EcsError> {
        if matches {
            if self.pos_by_key.get(&key).is_some() {
                return Ok(false);
            }
            let pos = self.matched.len();
            self.matched.try_push(key).map_err(|r| EcsError::Alloc(r.why))?;
            let _ = self.pos_by_key.insert(key, pos)?;
        } else {
            let Some(pos) = self.pos_by_key.remove(&key) else {
                return Ok(false);
            };
            let _ = self.matched.remove_swap(pos);
            if let Some(&moved_key) = self.matched.get(pos) {
                let _ = self.pos_by_key.insert(moved_key, pos)?;
            }
        }
        let next_v = graph.get(self.version).copied().unwrap_or(0).saturating_add(1);
        graph.set(self.version, next_v)
    }
}
