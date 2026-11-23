//! Pure iterative Thompson NFA compiler and linear-time PikeVM executor.
//!
//! Guaranteed `O(n * m)` worst-case runtime (no backtracking), zero recursion, zero `unsafe`,
//! zero panics, zero integer `as` casts, and zero `.clone()`.

use core::alloc::Allocator;
use core::cmp::Ordering;

use blaze_own::Vec;

use super::parse::{AnchorKind, Ast, CharRange, Node, NodeId};
use super::{Match, RegexError, RegexErrorKind, RegexLimits, RegexPlant};

/// Dense index into [`Nfa::states`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct StateId(u32);

/// One Thompson NFA instruction.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Inst {
    Char {
        ch: char,
        case_insensitive: bool,
        next: StateId,
    },
    AnyChar {
        dot_all: bool,
        next: StateId,
    },
    Class {
        range_start: u32,
        range_len: u32,
        negated: bool,
        next: StateId,
    },
    Anchor {
        kind: AnchorKind,
        next: StateId,
    },
    /// Epsilon branch: `primary` has strictly higher priority than `secondary`.
    Split {
        primary: StateId,
        secondary: StateId,
    },
    Save {
        slot: u32,
        next: StateId,
    },
    Match,
}

/// Compiled Thompson NFA.
#[derive(Debug)]
pub(crate) struct Nfa<A: Allocator> {
    states: Vec<Inst, A>,
    ranges: Vec<CharRange, A>,
    start: StateId,
    num_groups: usize,
}

impl<A: Allocator> Nfa<A> {
    #[inline]
    pub(crate) fn state_count(&self) -> usize {
        self.states.len()
    }

    #[inline]
    pub(crate) const fn num_groups(&self) -> usize {
        self.num_groups
    }

    #[inline]
    pub(crate) const fn num_slots(&self) -> usize {
        self.num_groups.saturating_mul(2)
    }

    /// Search `text` starting from byte offset `start_pos` (which must be a UTF-8 char boundary).
    ///
    /// Anchors observe the full `text`, not a sliced suffix.
    pub(crate) fn search_from<S: Allocator>(
        &self,
        text: &str,
        start_pos: usize,
        vm: &mut PikeVmScratch<S>,
        remaining_steps: &mut usize,
        max_steps: usize,
    ) -> Result<bool, RegexError> {
        vm.reset_search();
        let mut pos = start_pos;
        let mut matched = false;

        loop {
            charge_step(remaining_steps, max_steps, pos)?;

            // Seed a new start thread at `pos` only if no earlier match has succeeded yet.
            if !matched {
                for slot in vm.work_slots.as_mut_slice() {
                    *slot = None;
                }
                self.epsilon_closure(
                    self.start,
                    text,
                    pos,
                    &mut vm.curr,
                    &mut vm.work_slots,
                    &mut vm.eps_stack,
                    remaining_steps,
                    max_steps,
                )?;
            }

            let next_char_info = text.get(pos..).and_then(|s| s.chars().next());
            let next_pos = match next_char_info {
                Some(ch) => pos.saturating_add(ch.len_utf8()),
                None => pos,
            };

            vm.next.clear_for_step();

            let curr_count = vm.curr.states.len();
            let slots_per_thread = vm.curr.slots_per_thread;

            for thread_idx in 0..curr_count {
                charge_step(remaining_steps, max_steps, pos)?;
                let Some(state_id) = vm.curr.states.get(thread_idx).copied() else {
                    break;
                };
                let Some(inst) = self.state(state_id) else {
                    continue;
                };
                match inst {
                    Inst::Match => {
                        matched = true;
                        if slots_per_thread > 0 {
                            copy_thread_slots(
                                vm.best_slots.as_mut_slice(),
                                vm.curr.slots.as_slice(),
                                thread_idx,
                                slots_per_thread,
                            );
                        }
                        // All subsequent threads in `curr` have strictly lower priority.
                        break;
                    }
                    Inst::Char { ch: expected, case_insensitive, next } => {
                        if let Some(actual) = next_char_info
                            && char_matches(expected, actual, case_insensitive)
                        {
                            copy_thread_slots(
                                vm.work_slots.as_mut_slice(),
                                vm.curr.slots.as_slice(),
                                thread_idx,
                                slots_per_thread,
                            );
                            self.epsilon_closure(
                                next,
                                text,
                                next_pos,
                                &mut vm.next,
                                &mut vm.work_slots,
                                &mut vm.eps_stack,
                                remaining_steps,
                                max_steps,
                            )?;
                        }
                    }
                    Inst::AnyChar { dot_all, next } => {
                        if let Some(actual) = next_char_info
                            && (dot_all || actual != '\n')
                        {
                            copy_thread_slots(
                                vm.work_slots.as_mut_slice(),
                                vm.curr.slots.as_slice(),
                                thread_idx,
                                slots_per_thread,
                            );
                            self.epsilon_closure(
                                next,
                                text,
                                next_pos,
                                &mut vm.next,
                                &mut vm.work_slots,
                                &mut vm.eps_stack,
                                remaining_steps,
                                max_steps,
                            )?;
                        }
                    }
                    Inst::Class { range_start, range_len, negated, next } => {
                        if let Some(actual) = next_char_info
                            && self.class_matches(range_start, range_len, negated, actual)
                        {
                            copy_thread_slots(
                                vm.work_slots.as_mut_slice(),
                                vm.curr.slots.as_slice(),
                                thread_idx,
                                slots_per_thread,
                            );
                            self.epsilon_closure(
                                next,
                                text,
                                next_pos,
                                &mut vm.next,
                                &mut vm.work_slots,
                                &mut vm.eps_stack,
                                remaining_steps,
                                max_steps,
                            )?;
                        }
                    }
                    Inst::Anchor { .. } | Inst::Split { .. } | Inst::Save { .. } => {}
                }
            }

            // For `is_match` (`slots_per_thread == 0`), any match ends immediately.
            if matched && slots_per_thread == 0 {
                return Ok(true);
            }

            core::mem::swap(&mut vm.curr, &mut vm.next);

            // Once we have a match and no higher-priority threads remain, we are done.
            if matched && vm.curr.states.is_empty() {
                break;
            }

            if next_char_info.is_none() {
                break;
            }
            pos = next_pos;
        }

        Ok(matched)
    }

    #[inline]
    fn state(&self, id: StateId) -> Option<Inst> {
        let idx = usize::try_from(id.0).ok()?;
        self.states.get(idx).copied()
    }

    fn class_matches(&self, range_start: u32, range_len: u32, negated: bool, ch: char) -> bool {
        let Ok(start) = usize::try_from(range_start) else {
            return negated;
        };
        let Ok(len) = usize::try_from(range_len) else {
            return negated;
        };
        let Some(end) = start.checked_add(len) else {
            return negated;
        };
        let slice = self.ranges.get(start..end).unwrap_or(&[]);
        let found = if slice.len() <= 8 {
            let mut hit = false;
            for r in slice {
                if ch < r.start {
                    break;
                }
                if ch <= r.end {
                    hit = true;
                    break;
                }
            }
            hit
        } else {
            slice
                .binary_search_by(|r| {
                    if ch < r.start {
                        Ordering::Greater
                    } else if ch > r.end {
                        Ordering::Less
                    } else {
                        Ordering::Equal
                    }
                })
                .is_ok()
        };
        found ^ negated
    }

    /// Iterative priority-preserving epsilon closure with O(1) `Save` undo trail.
    fn epsilon_closure<S: Allocator>(
        &self,
        start: StateId,
        text: &str,
        at_pos: usize,
        dest: &mut ThreadList<S>,
        work_slots: &mut [Option<usize>],
        eps_stack: &mut Vec<EpsFrame, S>,
        remaining_steps: &mut usize,
        max_steps: usize,
    ) -> Result<(), RegexError> {
        eps_stack.clear();
        eps_stack.try_push(EpsFrame::Explore(start)).map_err(|r| RegexError::alloc(r.why))?;

        while let Some(frame) = eps_stack.pop() {
            match frame {
                EpsFrame::RestoreSlot { slot, prev } => {
                    if let Ok(idx) = usize::try_from(slot)
                        && let Some(cell) = work_slots.get_mut(idx)
                    {
                        *cell = prev;
                    }
                }
                EpsFrame::Explore(state_id) => {
                    charge_step(remaining_steps, max_steps, at_pos)?;
                    let Ok(idx) = usize::try_from(state_id.0) else {
                        continue;
                    };
                    let Some(mark) = dest.visited.get_mut(idx) else {
                        continue;
                    };
                    if *mark == dest.epoch {
                        continue;
                    }
                    *mark = dest.epoch;

                    let Some(inst) = self.state(state_id) else {
                        continue;
                    };
                    match inst {
                        Inst::Split { primary, secondary } => {
                            // Push secondary first so primary is popped and explored first.
                            eps_stack
                                .try_push(EpsFrame::Explore(secondary))
                                .map_err(|r| RegexError::alloc(r.why))?;
                            eps_stack
                                .try_push(EpsFrame::Explore(primary))
                                .map_err(|r| RegexError::alloc(r.why))?;
                        }
                        Inst::Save { slot, next } => {
                            if let Ok(slot_idx) = usize::try_from(slot)
                                && slot_idx < work_slots.len()
                            {
                                let prev = work_slots.get(slot_idx).copied().flatten();
                                if let Some(cell) = work_slots.get_mut(slot_idx) {
                                    *cell = Some(at_pos);
                                }
                                eps_stack
                                    .try_push(EpsFrame::RestoreSlot { slot, prev })
                                    .map_err(|r| RegexError::alloc(r.why))?;
                            }
                            eps_stack
                                .try_push(EpsFrame::Explore(next))
                                .map_err(|r| RegexError::alloc(r.why))?;
                        }
                        Inst::Anchor { kind, next } => {
                            if anchor_matches(kind, text, at_pos) {
                                eps_stack
                                    .try_push(EpsFrame::Explore(next))
                                    .map_err(|r| RegexError::alloc(r.why))?;
                            }
                        }
                        Inst::Char { .. }
                        | Inst::AnyChar { .. }
                        | Inst::Class { .. }
                        | Inst::Match => {
                            dest.states
                                .try_push(state_id)
                                .map_err(|r| RegexError::alloc(r.why))?;
                            if !work_slots.is_empty() {
                                dest.slots
                                    .try_extend_from_slice(work_slots)
                                    .map_err(RegexError::alloc)?;
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum EpsFrame {
    Explore(StateId),
    RestoreSlot { slot: u32, prev: Option<usize> },
}

struct ThreadList<S: Allocator> {
    states: Vec<StateId, S>,
    slots: Vec<Option<usize>, S>,
    visited: Vec<u32, S>,
    epoch: u32,
    slots_per_thread: usize,
}

impl<S: Allocator> ThreadList<S> {
    fn new(num_states: usize, slots_per_thread: usize, alloc: S) -> Result<Self, RegexError>
    where
        S: Copy,
    {
        let visited = try_fill_vec(num_states, 0u32, alloc)?;
        Ok(Self {
            states: Vec::new_in(alloc),
            slots: Vec::new_in(alloc),
            visited,
            epoch: 1,
            slots_per_thread,
        })
    }

    fn clear_for_step(&mut self) {
        self.states.clear();
        self.slots.clear();
        if self.epoch == u32::MAX {
            for v in self.visited.as_mut_slice() {
                *v = 0;
            }
            self.epoch = 1;
        } else {
            self.epoch = self.epoch.saturating_add(1);
        }
    }
}

/// Reusable scratch buffers for PikeVM execution (allocated once per search / `find_all` call).
pub(crate) struct PikeVmScratch<S: Allocator> {
    curr: ThreadList<S>,
    next: ThreadList<S>,
    work_slots: Vec<Option<usize>, S>,
    best_slots: Vec<Option<usize>, S>,
    eps_stack: Vec<EpsFrame, S>,
}

impl<S: Allocator> PikeVmScratch<S> {
    pub(crate) fn new(
        num_states: usize,
        slots_per_thread: usize,
        alloc: S,
    ) -> Result<Self, RegexError>
    where
        S: Copy,
    {
        let curr = ThreadList::new(num_states, slots_per_thread, alloc)?;
        let next = ThreadList::new(num_states, slots_per_thread, alloc)?;
        let work_slots = try_fill_vec(slots_per_thread, None, alloc)?;
        let best_slots = try_fill_vec(slots_per_thread, None, alloc)?;
        Ok(Self { curr, next, work_slots, best_slots, eps_stack: Vec::new_in(alloc) })
    }

    fn reset_search(&mut self) {
        self.curr.clear_for_step();
        self.next.clear_for_step();
        for slot in self.best_slots.as_mut_slice() {
            *slot = None;
        }
    }

    #[inline]
    pub(crate) fn match_zero(&self) -> Option<Match> {
        let start = self.best_slots.first().copied().flatten()?;
        let end = self.best_slots.get(1).copied().flatten()?;
        Some(Match { start, end })
    }

    #[inline]
    pub(crate) fn best_slots(&self) -> &[Option<usize>] {
        self.best_slots.as_slice()
    }

    pub(crate) fn extract_groups(
        &self,
        num_groups: usize,
        alloc: S,
    ) -> Result<Vec<Option<Match>, S>, RegexError> {
        let mut groups = Vec::try_with_capacity_in(num_groups, alloc).map_err(RegexError::alloc)?;
        for g in 0..num_groups {
            let base = g.checked_mul(2);
            let entry = match (
                base.and_then(|i| self.best_slots.get(i)).copied().flatten(),
                base.and_then(|i| i.checked_add(1))
                    .and_then(|i| self.best_slots.get(i))
                    .copied()
                    .flatten(),
            ) {
                (Some(start), Some(end)) => Some(Match { start, end }),
                _ => None,
            };
            groups.try_push(entry).map_err(|r| RegexError::alloc(r.why))?;
        }
        Ok(groups)
    }
}

#[derive(Clone, Copy)]
enum CompileFrame {
    Emit {
        node: NodeId,
        next: StateId,
    },
    CaptureStart {
        slot_start: u32,
    },
    ConcatStep {
        children_start: u32,
        remaining: u32,
    },
    AltFinish {
        count: u32,
    },
    RepeatOptWrap {
        inner: NodeId,
        remaining_opt: u32,
        exit: StateId,
        greedy: bool,
        remaining_min: u32,
    },
    RepeatReqStep {
        inner: NodeId,
        remaining_min: u32,
    },
    RepeatLoopFinish {
        split: StateId,
        exit: StateId,
        greedy: bool,
        inner: NodeId,
        remaining_min: u32,
        return_split: bool,
    },
}

/// Compile a parsed [`Ast`] into a Thompson [`Nfa`] iteratively (zero recursion).
pub(crate) fn compile_ast<A: Allocator + Copy>(
    ast: Ast<A>,
    limits: RegexLimits,
    alloc: A,
    plant: RegexPlant,
) -> Result<Nfa<A>, RegexError> {
    let mut states: Vec<Inst, A> = Vec::new_in(alloc);
    let mut frames: Vec<CompileFrame, A> = Vec::new_in(alloc);
    let mut results: Vec<StateId, A> = Vec::new_in(alloc);

    let match_state = push_state(&mut states, Inst::Match, limits.most_states)?;
    let save_end =
        push_state(&mut states, Inst::Save { slot: 1, next: match_state }, limits.most_states)?;

    frames
        .try_push(CompileFrame::Emit { node: ast.root, next: save_end })
        .map_err(|r| RegexError::alloc(r.why))?;

    while let Some(frame) = frames.pop() {
        match frame {
            CompileFrame::Emit { node, next } => {
                let Some(ast_node) = node_at(&ast.nodes, node) else {
                    results.try_push(next).map_err(|r| RegexError::alloc(r.why))?;
                    continue;
                };
                match ast_node {
                    Node::Empty => {
                        results.try_push(next).map_err(|r| RegexError::alloc(r.why))?;
                    }
                    Node::Literal { ch, case_insensitive } => {
                        let s = push_state(
                            &mut states,
                            Inst::Char { ch, case_insensitive, next },
                            limits.most_states,
                        )?;
                        results.try_push(s).map_err(|r| RegexError::alloc(r.why))?;
                    }
                    Node::AnyChar { dot_all } => {
                        let s = push_state(
                            &mut states,
                            Inst::AnyChar { dot_all, next },
                            limits.most_states,
                        )?;
                        results.try_push(s).map_err(|r| RegexError::alloc(r.why))?;
                    }
                    Node::Class { range_start, range_len, negated } => {
                        let s = push_state(
                            &mut states,
                            Inst::Class { range_start, range_len, negated, next },
                            limits.most_states,
                        )?;
                        results.try_push(s).map_err(|r| RegexError::alloc(r.why))?;
                    }
                    Node::Anchor(kind) => {
                        let s = push_state(
                            &mut states,
                            Inst::Anchor { kind, next },
                            limits.most_states,
                        )?;
                        results.try_push(s).map_err(|r| RegexError::alloc(r.why))?;
                    }
                    Node::Capture { group, inner } => {
                        let slot_start = group.checked_mul(2).ok_or_else(|| {
                            RegexError::new(
                                0,
                                RegexErrorKind::TooManyCaptures { limit: limits.most_captures },
                            )
                        })?;
                        let slot_end = slot_start.checked_add(1).ok_or_else(|| {
                            RegexError::new(
                                0,
                                RegexErrorKind::TooManyCaptures { limit: limits.most_captures },
                            )
                        })?;
                        let s_end = push_state(
                            &mut states,
                            Inst::Save { slot: slot_end, next },
                            limits.most_states,
                        )?;
                        frames
                            .try_push(CompileFrame::CaptureStart { slot_start })
                            .map_err(|r| RegexError::alloc(r.why))?;
                        frames
                            .try_push(CompileFrame::Emit { node: inner, next: s_end })
                            .map_err(|r| RegexError::alloc(r.why))?;
                    }
                    Node::Concat { children_start, children_len } => {
                        if children_len == 0 {
                            results.try_push(next).map_err(|r| RegexError::alloc(r.why))?;
                        } else {
                            let last_idx = children_len.saturating_sub(1);
                            let last_child = child_at(&ast.children, children_start, last_idx);
                            if last_idx > 0 {
                                frames
                                    .try_push(CompileFrame::ConcatStep {
                                        children_start,
                                        remaining: last_idx,
                                    })
                                    .map_err(|r| RegexError::alloc(r.why))?;
                            }
                            frames
                                .try_push(CompileFrame::Emit { node: last_child, next })
                                .map_err(|r| RegexError::alloc(r.why))?;
                        }
                    }
                    Node::Alt { children_start, children_len } => {
                        if children_len == 0 {
                            results.try_push(next).map_err(|r| RegexError::alloc(r.why))?;
                        } else if children_len == 1 {
                            let child = child_at(&ast.children, children_start, 0);
                            frames
                                .try_push(CompileFrame::Emit { node: child, next })
                                .map_err(|r| RegexError::alloc(r.why))?;
                        } else {
                            frames
                                .try_push(CompileFrame::AltFinish { count: children_len })
                                .map_err(|r| RegexError::alloc(r.why))?;
                            // Push in reverse order so branch 0 is compiled first and branch k-1 last.
                            for idx in (0..children_len).rev() {
                                let child = child_at(&ast.children, children_start, idx);
                                frames
                                    .try_push(CompileFrame::Emit { node: child, next })
                                    .map_err(|r| RegexError::alloc(r.why))?;
                            }
                        }
                    }
                    Node::Repeat { inner, min, max, greedy } => {
                        let effective_greedy =
                            if plant == RegexPlant::InvertGreediness { !greedy } else { greedy };
                        match max {
                            None => {
                                let split = push_state(
                                    &mut states,
                                    Inst::Split { primary: next, secondary: next },
                                    limits.most_states,
                                )?;
                                let (remaining_min, return_split) = if min == 0 {
                                    (0, true)
                                } else {
                                    (min.saturating_sub(1), false)
                                };
                                frames
                                    .try_push(CompileFrame::RepeatLoopFinish {
                                        split,
                                        exit: next,
                                        greedy: effective_greedy,
                                        inner,
                                        remaining_min,
                                        return_split,
                                    })
                                    .map_err(|r| RegexError::alloc(r.why))?;
                                frames
                                    .try_push(CompileFrame::Emit { node: inner, next: split })
                                    .map_err(|r| RegexError::alloc(r.why))?;
                            }
                            Some(max_val) => {
                                let opt = max_val.saturating_sub(min);
                                if opt > 0 {
                                    frames
                                        .try_push(CompileFrame::RepeatOptWrap {
                                            inner,
                                            remaining_opt: opt.saturating_sub(1),
                                            exit: next,
                                            greedy: effective_greedy,
                                            remaining_min: min,
                                        })
                                        .map_err(|r| RegexError::alloc(r.why))?;
                                    frames
                                        .try_push(CompileFrame::Emit { node: inner, next })
                                        .map_err(|r| RegexError::alloc(r.why))?;
                                } else if min > 0 {
                                    frames
                                        .try_push(CompileFrame::RepeatReqStep {
                                            inner,
                                            remaining_min: min.saturating_sub(1),
                                        })
                                        .map_err(|r| RegexError::alloc(r.why))?;
                                    frames
                                        .try_push(CompileFrame::Emit { node: inner, next })
                                        .map_err(|r| RegexError::alloc(r.why))?;
                                } else {
                                    results
                                        .try_push(next)
                                        .map_err(|r| RegexError::alloc(r.why))?;
                                }
                            }
                        }
                    }
                }
            }
            CompileFrame::CaptureStart { slot_start } => {
                let inner_entry = results.pop().unwrap_or(match_state);
                let s_start = push_state(
                    &mut states,
                    Inst::Save { slot: slot_start, next: inner_entry },
                    limits.most_states,
                )?;
                results.try_push(s_start).map_err(|r| RegexError::alloc(r.why))?;
            }
            CompileFrame::ConcatStep { children_start, remaining } => {
                let tail_entry = results.pop().unwrap_or(match_state);
                let prev_idx = remaining.saturating_sub(1);
                let prev_child = child_at(&ast.children, children_start, prev_idx);
                if prev_idx > 0 {
                    frames
                        .try_push(CompileFrame::ConcatStep { children_start, remaining: prev_idx })
                        .map_err(|r| RegexError::alloc(r.why))?;
                }
                frames
                    .try_push(CompileFrame::Emit { node: prev_child, next: tail_entry })
                    .map_err(|r| RegexError::alloc(r.why))?;
            }
            CompileFrame::AltFinish { count } => {
                let mut tail = results.pop().unwrap_or(match_state);
                for _ in 1..count {
                    let head = results.pop().unwrap_or(match_state);
                    tail = push_state(
                        &mut states,
                        Inst::Split { primary: head, secondary: tail },
                        limits.most_states,
                    )?;
                }
                results.try_push(tail).map_err(|r| RegexError::alloc(r.why))?;
            }
            CompileFrame::RepeatOptWrap {
                inner,
                remaining_opt,
                exit,
                greedy,
                remaining_min,
            } => {
                let copy_entry = results.pop().unwrap_or(match_state);
                let (primary, secondary) =
                    if greedy { (copy_entry, exit) } else { (exit, copy_entry) };
                let split = push_state(
                    &mut states,
                    Inst::Split { primary, secondary },
                    limits.most_states,
                )?;
                if remaining_opt > 0 {
                    frames
                        .try_push(CompileFrame::RepeatOptWrap {
                            inner,
                            remaining_opt: remaining_opt.saturating_sub(1),
                            exit,
                            greedy,
                            remaining_min,
                        })
                        .map_err(|r| RegexError::alloc(r.why))?;
                    frames
                        .try_push(CompileFrame::Emit { node: inner, next: split })
                        .map_err(|r| RegexError::alloc(r.why))?;
                } else if remaining_min > 0 {
                    frames
                        .try_push(CompileFrame::RepeatReqStep {
                            inner,
                            remaining_min: remaining_min.saturating_sub(1),
                        })
                        .map_err(|r| RegexError::alloc(r.why))?;
                    frames
                        .try_push(CompileFrame::Emit { node: inner, next: split })
                        .map_err(|r| RegexError::alloc(r.why))?;
                } else {
                    results.try_push(split).map_err(|r| RegexError::alloc(r.why))?;
                }
            }
            CompileFrame::RepeatReqStep { inner, remaining_min } => {
                let tail_entry = results.pop().unwrap_or(match_state);
                if remaining_min > 0 {
                    frames
                        .try_push(CompileFrame::RepeatReqStep {
                            inner,
                            remaining_min: remaining_min.saturating_sub(1),
                        })
                        .map_err(|r| RegexError::alloc(r.why))?;
                    frames
                        .try_push(CompileFrame::Emit { node: inner, next: tail_entry })
                        .map_err(|r| RegexError::alloc(r.why))?;
                } else {
                    results.try_push(tail_entry).map_err(|r| RegexError::alloc(r.why))?;
                }
            }
            CompileFrame::RepeatLoopFinish {
                split,
                exit,
                greedy,
                inner,
                remaining_min,
                return_split,
            } => {
                let inner_entry = results.pop().unwrap_or(match_state);
                let (primary, secondary) =
                    if greedy { (inner_entry, exit) } else { (exit, inner_entry) };
                if let Ok(idx) = usize::try_from(split.0)
                    && let Some(slot) = states.get_mut(idx)
                {
                    *slot = Inst::Split { primary, secondary };
                }
                let loop_entry = if return_split { split } else { inner_entry };
                if remaining_min > 0 {
                    frames
                        .try_push(CompileFrame::RepeatReqStep {
                            inner,
                            remaining_min: remaining_min.saturating_sub(1),
                        })
                        .map_err(|r| RegexError::alloc(r.why))?;
                    frames
                        .try_push(CompileFrame::Emit { node: inner, next: loop_entry })
                        .map_err(|r| RegexError::alloc(r.why))?;
                } else {
                    results.try_push(loop_entry).map_err(|r| RegexError::alloc(r.why))?;
                }
            }
        }
    }

    let root_entry = results.pop().unwrap_or(save_end);
    let start =
        push_state(&mut states, Inst::Save { slot: 0, next: root_entry }, limits.most_states)?;

    Ok(Nfa { states, ranges: ast.ranges, start, num_groups: ast.num_groups })
}

fn push_state<A: Allocator>(
    states: &mut Vec<Inst, A>,
    inst: Inst,
    limit: usize,
) -> Result<StateId, RegexError> {
    if states.len() >= limit {
        return Err(RegexError::new(0, RegexErrorKind::TooManyStates { limit }));
    }
    let id = u32::try_from(states.len())
        .map_err(|_| RegexError::new(0, RegexErrorKind::TooManyStates { limit }))?;
    states.try_push(inst).map_err(|r| RegexError::alloc(r.why))?;
    Ok(StateId(id))
}

#[inline]
fn node_at(nodes: &[Node], id: NodeId) -> Option<Node> {
    let idx = usize::try_from(id.0).ok()?;
    nodes.get(idx).copied()
}

#[inline]
fn child_at(children: &[NodeId], start: u32, offset: u32) -> NodeId {
    let Some(sum) = start.checked_add(offset) else {
        return NodeId(0);
    };
    let Ok(idx) = usize::try_from(sum) else {
        return NodeId(0);
    };
    children.get(idx).copied().unwrap_or(NodeId(0))
}

#[inline]
fn charge_step(
    remaining_steps: &mut usize,
    max_steps: usize,
    offset: usize,
) -> Result<(), RegexError> {
    if *remaining_steps == 0 {
        return Err(RegexError::new(
            offset,
            RegexErrorKind::StepLimitExceeded { steps: max_steps, limit: max_steps },
        ));
    }
    *remaining_steps = remaining_steps.saturating_sub(1);
    Ok(())
}

#[inline]
fn char_matches(expected: char, actual: char, case_insensitive: bool) -> bool {
    if case_insensitive {
        expected.eq_ignore_ascii_case(&actual)
    } else {
        expected == actual
    }
}

#[inline]
fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

#[inline]
fn anchor_matches(kind: AnchorKind, text: &str, pos: usize) -> bool {
    let bytes = text.as_bytes();
    match kind {
        AnchorKind::StartText => pos == 0,
        AnchorKind::EndText => pos == bytes.len(),
        AnchorKind::StartLine => {
            pos == 0 || pos.checked_sub(1).and_then(|i| bytes.get(i)).copied() == Some(b'\n')
        }
        AnchorKind::EndLine => pos == bytes.len() || bytes.get(pos).copied() == Some(b'\n'),
        AnchorKind::WordBoundary => {
            let prev = pos
                .checked_sub(1)
                .and_then(|i| bytes.get(i))
                .copied()
                .is_some_and(is_word_byte);
            let next = bytes.get(pos).copied().is_some_and(is_word_byte);
            prev != next
        }
        AnchorKind::NonWordBoundary => {
            let prev = pos
                .checked_sub(1)
                .and_then(|i| bytes.get(i))
                .copied()
                .is_some_and(is_word_byte);
            let next = bytes.get(pos).copied().is_some_and(is_word_byte);
            prev == next
        }
    }
}

fn copy_thread_slots(
    dst: &mut [Option<usize>],
    src_flat: &[Option<usize>],
    thread_idx: usize,
    slots_per_thread: usize,
) {
    if slots_per_thread == 0 {
        return;
    }
    let Some(start) = thread_idx.checked_mul(slots_per_thread) else {
        return;
    };
    let Some(end) = start.checked_add(slots_per_thread) else {
        return;
    };
    let Some(src) = src_flat.get(start..end) else {
        return;
    };
    for (d, s) in dst.iter_mut().zip(src.iter()) {
        *d = *s;
    }
}

fn try_fill_vec<T: Copy, A: Allocator>(
    len: usize,
    value: T,
    alloc: A,
) -> Result<Vec<T, A>, RegexError> {
    let mut out = Vec::try_with_capacity_in(len, alloc).map_err(RegexError::alloc)?;
    for _ in 0..len {
        out.try_push(value).map_err(|r| RegexError::alloc(r.why))?;
    }
    Ok(out)
}
