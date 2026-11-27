//! Opt-in checked complexity cost contracts (`#[blaze::cost(...)]`): one pass over the body
//! after the end sequence, when every callee is settled, checking declared asymptotic time,
//! space, and synchronization bounds in the same record and read list as typing.
//!
//! Zero-cost fast path: when the body's owner carries no `#[blaze::cost(..)]` attribute
//! (`CostContract::NONE`), the pass checks only for malformed loop marks (Rule 3) and returns
//! without walking the body or querying any callee.
//!
//! For an annotated body without `trusted = "..."`, an explicit-stack DFS (Rule 18: never
//! recursive on AST depth) tracks both `time_depth` and `space_depth`:
//! - unmarked loop (`LoopCost::Normal`): `time_depth += 1`, `space_depth += 1`
//! - `#[blaze::cost(bounded)]`: `time_depth += 0`, `space_depth += 0`
//! - `#[blaze::cost(amortized)]`: `time_depth = max(1)`, `space_depth = max(1)`
//! - `#[blaze::cost(reset)]`: `time_depth += 1`, `space_depth += 0`
//!
//! Closures inherit the enclosing `(time_depth, space_depth)` so calls inside iterator adaptors
//! (`xs.iter().any(|x| scan_for(x))`) see their enclosing loop depth. Compile-time `const { .. }`
//! blocks are skipped (O(1) at runtime).

use blaze_own::{Alloc, Vec};

use crate::body::{Expr, LoopCost};
use crate::diag::At;
use crate::error::{CheckError, Limit, Stop};
use crate::ids::{ExprId, ItemKey};
use crate::machine::Machine;
use crate::rules::args_inline::TypedFinding;
use crate::ty::TypeCtx;
use crate::world::{CostClass, CostContract, SyncCost, World};

/// Which complexity contract invariant was violated.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum CostViolation {
    /// Malformed or duplicate `#[blaze::cost(..)]` attribute on the function itself.
    MalformedContract = 0,
    /// Malformed or duplicate `#[blaze::cost(..)]` attribute on a loop expression.
    MalformedLoopMark = 1,
    /// A loop's nesting depth exceeds the function's declared time complexity bound.
    LoopExceedsBudget = 2,
    /// A callee's effective time cost (compounded by enclosing loops) exceeds the declared time bound.
    CallExceedsBudget = 3,
    /// Direct self-recursion in a function declaring `O(1)`, `amortized O(1)`, or `O(log n)`.
    DirectRecursion = 4,
    /// A callee's effective space cost (compounded by non-reset loops) exceeds the declared space bound.
    SpaceExceedsBudget = 5,
    /// A callee's synchronization class (`atomic` or `locks`) exceeds the declared sync class.
    SyncExceedsBudget = 6,
    /// A callee marked `locks` called inside an unmarked loop (`time_depth >= 1`) of a cost-checked fn.
    LockInLoop = 7,
}

/// Computed asymptotic cost of a loop or call after compounding with enclosing loop depth.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum EffectiveCost {
    O1 = 1,
    AmortizedO1 = 2,
    OLogN = 3,
    ON = 4,
    ONLogN = 5,
    ON2 = 6,
    OverON2 = 7,
}

impl EffectiveCost {
    /// Human-readable spelling of this computed cost.
    pub const fn name(self) -> &'static str {
        match self {
            EffectiveCost::O1 => "O(1)",
            EffectiveCost::AmortizedO1 => "amortized O(1)",
            EffectiveCost::OLogN => "O(log n)",
            EffectiveCost::ON => "O(n)",
            EffectiveCost::ONLogN => "O(n log n)",
            EffectiveCost::ON2 => "O(n^2)",
            EffectiveCost::OverON2 => "> O(n^2)",
        }
    }

    /// Whether this effective cost strictly exceeds `budget` (must be a valid declared class).
    pub const fn exceeds(self, budget: CostClass) -> bool {
        (self as u8) > (budget as u8)
    }
}

/// Inherent asymptotic time cost of a loop at nesting depth `depth`.
pub const fn loop_effective_cost(depth: u32) -> EffectiveCost {
    match depth {
        0 => EffectiveCost::O1,
        1 => EffectiveCost::ON,
        2 => EffectiveCost::ON2,
        _ => EffectiveCost::OverON2,
    }
}

/// Effective cost of invoking a callee of class `callee` inside `depth` compounding loops.
pub const fn effective_cost(callee: CostClass, depth: u32) -> EffectiveCost {
    match (callee, depth) {
        (CostClass::O1, 0) => EffectiveCost::O1,
        (CostClass::O1, 1) => EffectiveCost::ON,
        (CostClass::O1, 2) => EffectiveCost::ON2,
        (CostClass::O1, _) => EffectiveCost::OverON2,

        (CostClass::AmortizedO1, 0) => EffectiveCost::AmortizedO1,
        (CostClass::AmortizedO1, 1) => EffectiveCost::ON,
        (CostClass::AmortizedO1, 2) => EffectiveCost::ON2,
        (CostClass::AmortizedO1, _) => EffectiveCost::OverON2,

        (CostClass::OLogN, 0) => EffectiveCost::OLogN,
        (CostClass::OLogN, 1) => EffectiveCost::ONLogN,
        (CostClass::OLogN, _) => EffectiveCost::OverON2,

        (CostClass::ON, 0) => EffectiveCost::ON,
        (CostClass::ON, 1) => EffectiveCost::ON2,
        (CostClass::ON, _) => EffectiveCost::OverON2,

        (CostClass::ONLogN, 0) => EffectiveCost::ONLogN,
        (CostClass::ONLogN, _) => EffectiveCost::OverON2,

        (CostClass::ON2, 0) => EffectiveCost::ON2,
        (CostClass::ON2, _) => EffectiveCost::OverON2,

        (CostClass::None | CostClass::Invalid, _) => EffectiveCost::O1,
    }
}

#[derive(Clone, Copy)]
struct Visit {
    e: ExprId,
    time_depth: u32,
    space_depth: u32,
}

fn clamp_u8(v: u32) -> u8 {
    if v > u32::from(u8::MAX) { u8::MAX } else { v as u8 }
}

impl<'a, W: World, C: TypeCtx> Machine<'_, '_, '_, 'a, W, C> {
    /// Checked complexity cost-contract pass (`#[blaze::cost(..)]`), after the end sequence.
    pub(crate) fn cost_contract(&mut self) -> Result<(), Stop> {
        if self.errors > 0 {
            return Ok(());
        }
        let body = self.body;
        let owner_cost = self.reads.attrs(body.owner)?.cost();
        if owner_cost.is_invalid() {
            self.file_cost(
                CostViolation::MalformedContract,
                CostClass::Invalid,
                EffectiveCost::O1,
                SyncCost::None,
                SyncCost::None,
                0,
                None,
                At::Expr(body.root),
            );
            return Ok(());
        }
        if owner_cost.is_none() || owner_cost.trusted() {
            // Fast path: unannotated or trusted body skips DFS and callee queries,
            // while still reporting any malformed loop marks (Rule 3).
            let declared = owner_cost.time();
            for &(e, mark) in body.loop_marks {
                if mark == LoopCost::Invalid {
                    self.file_cost(
                        CostViolation::MalformedLoopMark,
                        declared,
                        EffectiveCost::O1,
                        owner_cost.sync(),
                        SyncCost::None,
                        0,
                        None,
                        At::Expr(e),
                    );
                }
            }
            return Ok(());
        }

        let time_budget = owner_cost.time();
        let space_budget = owner_cost.space();
        let sync_budget = owner_cost.sync();

        let alloc = self.alloc;
        let mut callees: Vec<(ExprId, ItemKey), Alloc<'a>> = Vec::with_capacity_in(self.callees.len(), alloc);
        callees.extend(self.callees.iter().map(|&(e, k, _)| (e, k)));
        callees.sort_unstable_by_key(|c| c.0);

        let mut cache: Vec<(ItemKey, CostContract), Alloc<'a>> = Vec::new_in(alloc);
        let mut stack: Vec<Visit, Alloc<'a>> = Vec::with_capacity_in(64, alloc);
        let mut kids: Vec<ExprId, Alloc<'a>> = Vec::new_in(alloc);

        stack.push(Visit { e: body.root, time_depth: 0, space_depth: 0 });
        let mut steps: usize = 0;
        while let Some(Visit { e, time_depth, space_depth }) = stack.pop() {
            steps += 1;
            if steps > 4 * body.exprs.len() + 64 {
                return Err(CheckError::Limit(Limit::Steps).into());
            }
            let x = body.expr(e)?;
            if matches!(x, Expr::ConstBlock { .. }) {
                // Compile-time constant blocks execute at compile time, not runtime.
                continue;
            }
            let (next_time, next_space) = if matches!(x, Expr::Loop { .. }) {
                let mark = body.loop_cost(e);
                let (nt, ns) = match mark {
                    LoopCost::Invalid => {
                        self.file_cost(
                            CostViolation::MalformedLoopMark,
                            time_budget,
                            EffectiveCost::O1,
                            sync_budget,
                            SyncCost::None,
                            clamp_u8(time_depth),
                            None,
                            At::Expr(e),
                        );
                        (time_depth.saturating_add(1), space_depth.saturating_add(1))
                    }
                    LoopCost::Normal => (time_depth.saturating_add(1), space_depth.saturating_add(1)),
                    LoopCost::Bounded => (time_depth, space_depth),
                    LoopCost::Amortized => (time_depth.max(1), space_depth.max(1)),
                    LoopCost::Reset => (time_depth.saturating_add(1), space_depth),
                };
                if mark != LoopCost::Invalid && time_budget != CostClass::None {
                    let loop_actual = loop_effective_cost(nt);
                    if loop_actual.exceeds(time_budget) {
                        self.file_cost(
                            CostViolation::LoopExceedsBudget,
                            time_budget,
                            loop_actual,
                            sync_budget,
                            SyncCost::None,
                            clamp_u8(nt),
                            None,
                            At::Expr(e),
                        );
                    }
                }
                (nt, ns)
            } else {
                (time_depth, space_depth)
            };

            // Check every resolved callee attached to expression `e`.
            let start = callees.partition_point(|c| c.0 < e);
            for &(ce, callee) in &callees[start..] {
                if ce != e {
                    break;
                }
                if callee == body.owner && time_budget != CostClass::None && time_budget <= CostClass::OLogN {
                    self.file_cost(
                        CostViolation::DirectRecursion,
                        time_budget,
                        EffectiveCost::ON,
                        sync_budget,
                        SyncCost::None,
                        clamp_u8(time_depth),
                        Some(callee),
                        At::Expr(e),
                    );
                    continue;
                }
                let callee_cost = self.cached_cost(callee, &mut cache)?;
                if callee_cost.is_none() || callee_cost.is_invalid() {
                    continue;
                }
                let callee_time = callee_cost.time();
                if time_budget != CostClass::None && callee_time != CostClass::None {
                    let eff_time = effective_cost(callee_time, time_depth);
                    if eff_time.exceeds(time_budget) {
                        self.file_cost(
                            CostViolation::CallExceedsBudget,
                            time_budget,
                            eff_time,
                            sync_budget,
                            SyncCost::None,
                            clamp_u8(time_depth),
                            Some(callee),
                            At::Expr(e),
                        );
                    }
                }
                let callee_space = callee_cost.space();
                if space_budget != CostClass::None && callee_space != CostClass::None {
                    let eff_space = effective_cost(callee_space, space_depth);
                    if eff_space.exceeds(space_budget) {
                        self.file_cost(
                            CostViolation::SpaceExceedsBudget,
                            space_budget,
                            eff_space,
                            sync_budget,
                            SyncCost::None,
                            clamp_u8(space_depth),
                            Some(callee),
                            At::Expr(e),
                        );
                    }
                }
                let callee_sync = callee_cost.sync();
                if callee_sync != SyncCost::None {
                    let sync_exceeds = (sync_budget == SyncCost::Local
                        && matches!(callee_sync, SyncCost::Atomic | SyncCost::Lock))
                        || (sync_budget == SyncCost::Atomic && callee_sync == SyncCost::Lock);
                    if sync_exceeds {
                        self.file_cost(
                            CostViolation::SyncExceedsBudget,
                            time_budget,
                            EffectiveCost::O1,
                            sync_budget,
                            callee_sync,
                            clamp_u8(time_depth),
                            Some(callee),
                            At::Expr(e),
                        );
                    } else if sync_budget == SyncCost::None && callee_sync == SyncCost::Lock && time_depth >= 1 {
                        self.file_cost(
                            CostViolation::LockInLoop,
                            time_budget,
                            EffectiveCost::O1,
                            sync_budget,
                            callee_sync,
                            clamp_u8(time_depth),
                            Some(callee),
                            At::Expr(e),
                        );
                    }
                }
            }

            kids.clear();
            body.children(x, &mut kids)?;
            for &k in kids.iter().rev() {
                stack.push(Visit { e: k, time_depth: next_time, space_depth: next_space });
            }
        }
        Ok(())
    }

    fn cached_cost(
        &mut self,
        callee: ItemKey,
        cache: &mut Vec<(ItemKey, CostContract), Alloc<'a>>,
    ) -> Result<CostContract, Stop> {
        if let Some(&(_, c)) = cache.iter().find(|&&(k, _)| k == callee) {
            return Ok(c);
        }
        let c = self.reads.attrs(callee)?.cost();
        cache.push((callee, c));
        Ok(c)
    }

    #[expect(clippy::too_many_arguments, reason = "packs one TypedFinding::Cost record cleanly")]
    fn file_cost(
        &mut self,
        violation: CostViolation,
        declared: CostClass,
        actual: EffectiveCost,
        declared_sync: SyncCost,
        actual_sync: SyncCost,
        loop_depth: u8,
        callee: Option<ItemKey>,
        at: At,
    ) {
        let at = self.range_of(at);
        self.typed_findings.push(TypedFinding::Cost {
            violation,
            declared,
            actual,
            declared_sync,
            actual_sync,
            loop_depth,
            callee,
            at,
        });
    }
}
