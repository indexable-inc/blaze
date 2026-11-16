//! Which locals are held across each suspension (coroutines.md, "Which locals are saved").
//!
//! A local is held across a `Suspend` when one of two things is true there:
//! - it is live by use: some path from the suspension's resume block or cancel path reads it
//!   (a use, a move, a drop, or taking its address) before writing all of it;
//! - it may have been borrowed on some path to the suspension and its storage has not ended
//!   since (`StorageDead`). A reference to it may be held across too, so it must stay where it
//!   is. This is the rule that makes the state self-referential, and `Pin` its licence.
//!
//! Both are bitset dataflows over the blocks, run to a fixed point with a counted bound.
#![blaze::sans_io]

use blaze_flow::Bound;
use blaze_flow::bits::{self, Bits};
use blaze_own::Vec;
use core::alloc::Allocator;

use crate::Plant;
use crate::ir::{Access, BlockId, Body, Term};

/// Per suspension point, the locals held across it.
pub struct Across<A: Allocator> {
    /// Row `point`, column `local`: held across that point.
    pub at: Bits<A>,
    /// Row `point`, column `local`: maybe borrowed there (a subset of `at`).
    pub borrowed: Bits<A>,
}

/// The locals held across each suspension of a validated body.
///
/// Bounds: a liveness pass but the last sets a bit of `blocks * locals`, and so does a borrow
/// pass, so each runs at most `blocks * locals + 2` times.
pub fn across<A: Allocator, B: Allocator + Clone>(body: &Body<A>, plant: Plant, alloc: B) -> Result<Across<B>, Bound> {
    let (n, m) = (body.blocks.len(), body.locals.len());
    let limit = (n * m + 2) as u64;

    // Live by use, backward: `live_in[b]`.
    let mut live_in = Bits::new_in(n, m, alloc.clone());
    let w = live_in.words();
    let mut cur = Vec::with_capacity_in(w, alloc.clone());
    cur.resize(w, 0u64);
    let mut passes = 0u64;
    loop {
        passes += 1;
        if passes > limit {
            return Err(Bound { what: "coroutine liveness passes", limit });
        }
        let mut changed = false;
        for b in (0..n).rev() {
            let id = BlockId(b as u32);
            for x in cur.iter_mut() {
                *x = 0;
            }
            body.for_each_succ(id, |t| {
                let _ = bits::union_into(&mut cur, live_in.row(t.index()));
            });
            body.term_accesses(body.term(id), |_, l| bits::put(&mut cur, l.index(), true));
            for &s in body.stmts_of(id).iter().rev() {
                // A statement reads before it writes: going backward, the write is undone first.
                body.stmt_accesses(s, |a, l| {
                    if matches!(a, Access::Def | Access::Dead) {
                        bits::put(&mut cur, l.index(), false);
                    }
                });
                body.stmt_accesses(s, |a, l| {
                    if matches!(a, Access::Use | Access::Borrow) {
                        bits::put(&mut cur, l.index(), true);
                    }
                });
            }
            if bits::union_into(live_in.row_mut(b), &cur) {
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    // Maybe borrowed, forward: `borrowed_in[b]`.
    let mut borrowed_in = Bits::new_in(n, m, alloc.clone());
    let mut passes = 0u64;
    loop {
        passes += 1;
        if passes > limit {
            return Err(Bound { what: "coroutine borrow passes", limit });
        }
        let mut changed = false;
        for b in 0..n {
            let id = BlockId(b as u32);
            cur.copy_from_slice(borrowed_in.row(b));
            borrowed_through(body, id, &mut cur);
            body.for_each_succ(id, |t| {
                if bits::union_into(borrowed_in.row_mut(t.index()), &cur) {
                    changed = true;
                }
            });
        }
        if !changed {
            break;
        }
    }

    let points = body.points as usize;
    let mut at = Bits::new_in(points, m, alloc.clone());
    let mut borrowed = Bits::new_in(points, m, alloc);
    for b in 0..n {
        let id = BlockId(b as u32);
        let Term::Suspend { point, resume, drop } = body.term(id) else { continue };
        let p = point.index();
        cur.copy_from_slice(borrowed_in.row(b));
        borrowed_through(body, id, &mut cur);
        // Planted (test builds only): borrows are not counted.
        if !(cfg!(test) && plant == Plant::BorrowsNotCounted) {
            let _ = bits::union_into(borrowed.row_mut(p), &cur);
            let _ = bits::union_into(at.row_mut(p), &cur);
        }
        let _ = bits::union_into(at.row_mut(p), live_in.row(resume.index()));
        let _ = bits::union_into(at.row_mut(p), live_in.row(drop.index()));
    }
    Ok(Across { at, borrowed })
}

/// Applies a block's statements to a maybe-borrowed set.
fn borrowed_through<A: Allocator>(body: &Body<A>, b: BlockId, cur: &mut [u64]) {
    for &s in body.stmts_of(b) {
        body.stmt_accesses(s, |a, l| match a {
            Access::Borrow => bits::put(cur, l.index(), true),
            Access::Dead => bits::put(cur, l.index(), false),
            Access::Use | Access::Def => {}
        });
    }
}
