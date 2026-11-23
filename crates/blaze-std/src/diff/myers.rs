//! Iterative linear-space Myers O(ND) diff algorithm (Myers 1986, Section 4b) with common
//! prefix/suffix stripping and an explicit step budget (`bounded-progress`).
//!
//! Scratch space (`vf`, `vb`) is allocated once up front (`O(N + M)`) and reused across every
//! divide step via an explicit worklist stack (zero recursion, zero stack overflow risk, zero
//! panicking indexes).

use core::alloc::Allocator;

use blaze_own::{AllocFailure, Vec};

use super::{DiffError, Op};

/// Maximum number of steps before [`DiffError::BudgetExceeded`] is returned by default.
pub const DEFAULT_BUDGET: usize = 4_000_000;

/// Compute a canonical, coalesced Myers edit script transforming `old` into `new`.
pub(crate) fn diff_myers<T: Eq, A: Allocator + Copy>(
    old: &[T],
    new: &[T],
    budget: usize,
    alloc: A,
) -> Result<Vec<Op, A>, DiffError> {
    let mut ops = Vec::new_in(alloc);
    let mut remaining = budget;

    let prefix = common_prefix_len(old, new);
    if prefix > 0 {
        push_op(&mut ops, Op::Equal { old_start: 0, new_start: 0, len: prefix })?;
    }
    let old_mid = old.get(prefix..).unwrap_or(&[]);
    let new_mid = new.get(prefix..).unwrap_or(&[]);
    let suffix = common_suffix_len(old_mid, new_mid);
    let old_inner_len = old_mid.len().saturating_sub(suffix);
    let new_inner_len = new_mid.len().saturating_sub(suffix);

    if old_inner_len > 0 || new_inner_len > 0 {
        if old_inner_len == 0 {
            push_op(&mut ops, Op::Insert { new_start: prefix, len: new_inner_len })?;
        } else if new_inner_len == 0 {
            push_op(&mut ops, Op::Delete { old_start: prefix, len: old_inner_len })?;
        } else {
            let max_d = (old_inner_len.saturating_add(new_inner_len)).div_ceil(2);
            let scratch_len = max_d
                .checked_mul(2)
                .and_then(|v| v.checked_add(3))
                .ok_or(DiffError::BudgetExceeded { steps: budget })?;
            let mut vf = Vec::new_in(alloc);
            vf.try_reserve_exact(scratch_len).map_err(DiffError::Alloc)?;
            for _ in 0..scratch_len {
                vf.try_push(0isize).map_err(|r| DiffError::Alloc(r.why))?;
            }
            let mut vb = Vec::new_in(alloc);
            vb.try_reserve_exact(scratch_len).map_err(DiffError::Alloc)?;
            for _ in 0..scratch_len {
                vb.try_push(0isize).map_err(|r| DiffError::Alloc(r.why))?;
            }

            let mut ctx = MyersCtx {
                old,
                new,
                vf: vf.as_mut_slice(),
                vb: vb.as_mut_slice(),
                remaining: &mut remaining,
                budget,
                ops: &mut ops,
            };
            ctx.conquer_iter(
                prefix,
                prefix + old_inner_len,
                prefix,
                prefix + new_inner_len,
                alloc,
            )?;
        }
    }

    if suffix > 0 {
        push_op(
            &mut ops,
            Op::Equal {
                old_start: old.len().saturating_sub(suffix),
                new_start: new.len().saturating_sub(suffix),
                len: suffix,
            },
        )?;
    }

    Ok(ops)
}

/// Compute the exact Myers edit distance (number of insertions + deletions) between `old` and `new`.
pub(crate) fn edit_distance_myers<T: Eq, A: Allocator + Copy>(
    old: &[T],
    new: &[T],
    budget: usize,
    alloc: A,
) -> Result<usize, DiffError> {
    let prefix = common_prefix_len(old, new);
    let old = old.get(prefix..).unwrap_or(&[]);
    let new = new.get(prefix..).unwrap_or(&[]);
    let suffix = common_suffix_len(old, new);
    let old = old.get(..old.len().saturating_sub(suffix)).unwrap_or(&[]);
    let new = new.get(..new.len().saturating_sub(suffix)).unwrap_or(&[]);

    let n = old.len();
    let m = new.len();
    if n == 0 {
        return Ok(m);
    }
    if m == 0 {
        return Ok(n);
    }

    let max_d = n.checked_add(m).ok_or(DiffError::BudgetExceeded { steps: budget })?;
    let v_len = max_d
        .checked_mul(2)
        .and_then(|v| v.checked_add(3))
        .ok_or(DiffError::BudgetExceeded { steps: budget })?;
    let offset = isize::try_from(max_d.saturating_add(1))
        .map_err(|_| DiffError::BudgetExceeded { steps: budget })?;
    let mut v = Vec::new_in(alloc);
    v.try_reserve_exact(v_len).map_err(DiffError::Alloc)?;
    for _ in 0..v_len {
        v.try_push(0isize).map_err(|r| DiffError::Alloc(r.why))?;
    }

    let n_i = isize::try_from(n).map_err(|_| DiffError::BudgetExceeded { steps: budget })?;
    let m_i = isize::try_from(m).map_err(|_| DiffError::BudgetExceeded { steps: budget })?;
    let max_d_i =
        isize::try_from(max_d).map_err(|_| DiffError::BudgetExceeded { steps: budget })?;
    let mut steps = 0usize;

    for d in 0..=max_d_i {
        let mut k = -d;
        while k <= d {
            steps = steps.saturating_add(1);
            if steps > budget {
                return Err(DiffError::BudgetExceeded { steps: budget });
            }
            let ki = usize::try_from(k + offset)
                .map_err(|_| DiffError::BudgetExceeded { steps: budget })?;
            let prev_v = v.get(ki.saturating_sub(1)).copied().unwrap_or(0);
            let next_v = v.get(ki.saturating_add(1)).copied().unwrap_or(0);
            let mut x = if k == -d || (k != d && prev_v < next_v) { next_v } else { prev_v + 1 };
            let mut y = x - k;
            while x < n_i && y < m_i {
                let Ok(xu) = usize::try_from(x) else { break };
                let Ok(yu) = usize::try_from(y) else { break };
                if old.get(xu) == new.get(yu) {
                    x += 1;
                    y += 1;
                } else {
                    break;
                }
            }
            if let Some(slot) = v.get_mut(ki) {
                *slot = x;
            }
            if x >= n_i && y >= m_i {
                return usize::try_from(d).map_err(|_| DiffError::BudgetExceeded { steps: budget });
            }
            k += 2;
        }
    }

    Ok(max_d)
}

pub(crate) fn common_prefix_len<T: Eq>(a: &[T], b: &[T]) -> usize {
    a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count()
}

pub(crate) fn common_suffix_len<T: Eq>(a: &[T], b: &[T]) -> usize {
    a.iter().rev().zip(b.iter().rev()).take_while(|(x, y)| x == y).count()
}

/// Push `op` into `ops`, coalescing adjacent runs of the same kind and keeping `Delete` before
/// `Insert` within an adjacent change block for canonical deterministic output. Non-recursive.
pub(crate) fn push_op<A: Allocator>(ops: &mut Vec<Op, A>, op: Op) -> Result<(), DiffError> {
    match op {
        Op::Equal { len: 0, .. } | Op::Delete { len: 0, .. } | Op::Insert { len: 0, .. } => {
            return Ok(());
        }
        _ => {}
    }

    if let Some(last) = ops.last_mut() {
        match (*last, op) {
            (
                Op::Equal { old_start: os1, new_start: ns1, len: l1 },
                Op::Equal { old_start: os2, new_start: ns2, len: l2 },
            ) if os1 + l1 == os2 && ns1 + l1 == ns2 => {
                *last = Op::Equal { old_start: os1, new_start: ns1, len: l1 + l2 };
                return Ok(());
            }
            (
                Op::Delete { old_start: os1, len: l1 },
                Op::Delete { old_start: os2, len: l2 },
            ) if os1 + l1 == os2 => {
                *last = Op::Delete { old_start: os1, len: l1 + l2 };
                return Ok(());
            }
            (
                Op::Insert { new_start: ns1, len: l1 },
                Op::Insert { new_start: ns2, len: l2 },
            ) if ns1 + l1 == ns2 => {
                *last = Op::Insert { new_start: ns1, len: l1 + l2 };
                return Ok(());
            }
            // Canonicalize adjacent Insert followed by Delete into Delete before Insert.
            (Op::Insert { .. }, Op::Delete { old_start: os2, len: l2 }) => {
                if let Some(prev_insert) = ops.pop() {
                    // Merge `op` into any Delete now at the back, else push it.
                    let merged = if let Some(Op::Delete { old_start: os1, len: l1 }) = ops.last_mut()
                        && *os1 + *l1 == os2
                    {
                        *l1 += l2;
                        true
                    } else {
                        false
                    };
                    if !merged {
                        ops.try_push(op).map_err(|r| DiffError::Alloc(r.why))?;
                    }
                    ops.try_push(prev_insert).map_err(|r| DiffError::Alloc(r.why))?;
                }
                return Ok(());
            }
            _ => {}
        }
    }

    ops.try_push(op).map_err(|r| DiffError::Alloc(r.why))
}

struct MiddleSnake {
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
}

enum WorkItem {
    Range { old_start: usize, old_end: usize, new_start: usize, new_end: usize },
    Emit(Op),
}

struct MyersCtx<'a, T, A: Allocator> {
    old: &'a [T],
    new: &'a [T],
    vf: &'a mut [isize],
    vb: &'a mut [isize],
    remaining: &'a mut usize,
    budget: usize,
    ops: &'a mut Vec<Op, A>,
}

impl<T: Eq, A: Allocator + Copy> MyersCtx<'_, T, A> {
    fn conquer_iter(
        &mut self,
        old_start: usize,
        old_end: usize,
        new_start: usize,
        new_end: usize,
        alloc: A,
    ) -> Result<(), DiffError> {
        let mut stack: Vec<WorkItem, A> = Vec::new_in(alloc);
        stack
            .try_push(WorkItem::Range { old_start, old_end, new_start, new_end })
            .map_err(|r| DiffError::Alloc(r.why))?;

        while let Some(item) = stack.pop() {
            match item {
                WorkItem::Emit(op) => push_op(self.ops, op)?,
                WorkItem::Range { mut old_start, mut old_end, mut new_start, mut new_end } => {
                    let old_slice = self.old.get(old_start..old_end).unwrap_or(&[]);
                    let new_slice = self.new.get(new_start..new_end).unwrap_or(&[]);
                    let prefix = common_prefix_len(old_slice, new_slice);
                    if prefix > 0 {
                        push_op(self.ops, Op::Equal { old_start, new_start, len: prefix })?;
                        old_start += prefix;
                        new_start += prefix;
                    }
                    let old_slice = self.old.get(old_start..old_end).unwrap_or(&[]);
                    let new_slice = self.new.get(new_start..new_end).unwrap_or(&[]);
                    let suffix = common_suffix_len(old_slice, new_slice);
                    let suffix_old_start = old_end.saturating_sub(suffix);
                    let suffix_new_start = new_end.saturating_sub(suffix);
                    old_end = suffix_old_start;
                    new_end = suffix_new_start;

                    if suffix > 0 {
                        stack
                            .try_push(WorkItem::Emit(Op::Equal {
                                old_start: suffix_old_start,
                                new_start: suffix_new_start,
                                len: suffix,
                            }))
                            .map_err(|r| DiffError::Alloc(r.why))?;
                    }

                    let n = old_end.saturating_sub(old_start);
                    let m = new_end.saturating_sub(new_start);

                    if n == 0 {
                        if m > 0 {
                            push_op(self.ops, Op::Insert { new_start, len: m })?;
                        }
                    } else if m == 0 {
                        push_op(self.ops, Op::Delete { old_start, len: n })?;
                    } else {
                        let snake =
                            self.find_middle_snake(old_start, old_end, new_start, new_end)?;
                        let mid_old_0 = old_start + snake.x0;
                        let mid_new_0 = new_start + snake.y0;
                        let mid_old_1 = old_start + snake.x1;
                        let mid_new_1 = new_start + snake.y1;

                        // LIFO push order: right half, middle snake, left half (so left runs first).
                        stack
                            .try_push(WorkItem::Range {
                                old_start: mid_old_1,
                                old_end,
                                new_start: mid_new_1,
                                new_end,
                            })
                            .map_err(|r| DiffError::Alloc(r.why))?;
                        if snake.x1 > snake.x0 {
                            stack
                                .try_push(WorkItem::Emit(Op::Equal {
                                    old_start: mid_old_0,
                                    new_start: mid_new_0,
                                    len: snake.x1 - snake.x0,
                                }))
                                .map_err(|r| DiffError::Alloc(r.why))?;
                        }
                        stack
                            .try_push(WorkItem::Range {
                                old_start,
                                old_end: mid_old_0,
                                new_start,
                                new_end: mid_new_0,
                            })
                            .map_err(|r| DiffError::Alloc(r.why))?;
                    }
                }
            }
        }
        Ok(())
    }

    fn find_middle_snake(
        &mut self,
        old_start: usize,
        old_end: usize,
        new_start: usize,
        new_end: usize,
    ) -> Result<MiddleSnake, DiffError> {
        let old = self.old.get(old_start..old_end).unwrap_or(&[]);
        let new = self.new.get(new_start..new_end).unwrap_or(&[]);
        let n = isize::try_from(old.len())
            .map_err(|_| DiffError::Alloc(AllocFailure::CapacityOverflow { what: "myers", len: old.len(), additional: 0 }))?;
        let m = isize::try_from(new.len())
            .map_err(|_| DiffError::Alloc(AllocFailure::CapacityOverflow { what: "myers", len: new.len(), additional: 0 }))?;
        let delta = n - m;
        let odd = (delta & 1) != 0;
        let max_d = (n + m + 1) / 2;
        let offset = max_d + 1;

        if let Ok(init_idx) = usize::try_from(offset + 1) {
            if let Some(slot) = self.vf.get_mut(init_idx) {
                *slot = 0;
            }
            if let Some(slot) = self.vb.get_mut(init_idx) {
                *slot = 0;
            }
        }

        for d in 0..=max_d {
            // Forward search: k in [-d, d] step 2.
            let mut k = -d;
            while k <= d {
                if *self.remaining == 0 {
                    return Err(DiffError::BudgetExceeded { steps: self.budget });
                }
                *self.remaining -= 1;

                let ki = usize::try_from(k + offset)
                    .map_err(|_| DiffError::BudgetExceeded { steps: self.budget })?;
                let prev_f = self.vf.get(ki.saturating_sub(1)).copied().unwrap_or(0);
                let next_f = self.vf.get(ki.saturating_add(1)).copied().unwrap_or(0);
                let mut x =
                    if k == -d || (k != d && prev_f < next_f) { next_f } else { prev_f + 1 };
                let mut y = x - k;
                let x0 = x;
                let y0 = y;
                while x < n && y < m {
                    let Ok(xu) = usize::try_from(x) else { break };
                    let Ok(yu) = usize::try_from(y) else { break };
                    if old.get(xu) == new.get(yu) {
                        x += 1;
                        y += 1;
                    } else {
                        break;
                    }
                }
                if let Some(slot) = self.vf.get_mut(ki) {
                    *slot = x;
                }

                if odd {
                    let rk = delta - k;
                    if rk >= -(d - 1) && rk <= d - 1 {
                        if let Ok(rki) = usize::try_from(rk + offset) {
                            let rx = self.vb.get(rki).copied().unwrap_or(0);
                            if x + rx >= n {
                                return Ok(MiddleSnake {
                                    x0: usize::try_from(x0).unwrap_or(0),
                                    y0: usize::try_from(y0).unwrap_or(0),
                                    x1: usize::try_from(x).unwrap_or(0),
                                    y1: usize::try_from(y).unwrap_or(0),
                                });
                            }
                        }
                    }
                }
                k += 2;
            }

            // Backward search: rk in [-d, d] step 2.
            let mut rk = -d;
            while rk <= d {
                if *self.remaining == 0 {
                    return Err(DiffError::BudgetExceeded { steps: self.budget });
                }
                *self.remaining -= 1;

                let rki = usize::try_from(rk + offset)
                    .map_err(|_| DiffError::BudgetExceeded { steps: self.budget })?;
                let prev_b = self.vb.get(rki.saturating_sub(1)).copied().unwrap_or(0);
                let next_b = self.vb.get(rki.saturating_add(1)).copied().unwrap_or(0);
                let mut rx =
                    if rk == -d || (rk != d && prev_b < next_b) { next_b } else { prev_b + 1 };
                let mut ry = rx - rk;
                let rx0 = rx;
                let ry0 = ry;
                while rx < n && ry < m {
                    let Ok(ox) = usize::try_from(n - 1 - rx) else { break };
                    let Ok(ny) = usize::try_from(m - 1 - ry) else { break };
                    if old.get(ox) == new.get(ny) {
                        rx += 1;
                        ry += 1;
                    } else {
                        break;
                    }
                }
                if let Some(slot) = self.vb.get_mut(rki) {
                    *slot = rx;
                }

                if !odd {
                    let k = delta - rk;
                    if k >= -d && k <= d {
                        if let Ok(ki) = usize::try_from(k + offset) {
                            let fx = self.vf.get(ki).copied().unwrap_or(0);
                            if fx + rx >= n {
                                return Ok(MiddleSnake {
                                    x0: usize::try_from(n - rx).unwrap_or(0),
                                    y0: usize::try_from(m - ry).unwrap_or(0),
                                    x1: usize::try_from(n - rx0).unwrap_or(0),
                                    y1: usize::try_from(m - ry0).unwrap_or(0),
                                });
                            }
                        }
                    }
                }
                rk += 2;
            }
        }

        Ok(MiddleSnake { x0: old.len(), y0: 0, x1: old.len(), y1: 0 })
    }
}
