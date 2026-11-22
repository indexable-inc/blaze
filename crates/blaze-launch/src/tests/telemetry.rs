//! Deterministic simulation of [`crate::telemetry::ShadowStack`] against a sequential
//! reference model (rules 43 & 53): nested calls, tail calls at equal `sp`, panics unwinding
//! to arbitrary ancestor frames, deep recursion past [`SHADOW_DEPTH`], and planted-bug
//! controls (`HealOnEqualSp`, `OverwriteOnFull`, `ReuseSpanId`).

use std::collections::HashSet;

use blaze_sim::Tape;

use crate::Slot;
use crate::telemetry::{
    EnterOutcome, SHADOW_DEPTH, ShadowError, ShadowPlant, ShadowStack, SpanId,
};

#[derive(Clone, Copy, Debug)]
struct ModelFrame {
    slot: Slot,
    real_lr: u64,
    caller_sp: u64,
    start_ns: u64,
    span: SpanId,
    parent: SpanId,
    /// `true` when this call pushed onto the shadow stack; `false` when it overflowed and ran
    /// passthrough without modifying `x30`.
    hooked: bool,
}

#[derive(Clone, Copy, Debug)]
enum Op {
    /// Normal nested call (`sp` decreases by `delta` >= 16).
    Call { slot: u32, delta: u64, dt: u64 },
    /// Tail call between hooked functions (`sp` stays equal to the top frame's `caller_sp`).
    TailCall { slot: u32, dt: u64 },
    /// Normal return from the top call.
    Return { dt: u64 },
    /// Unwind (panic caught `drop_frames` frames higher up the stack).
    Unwind { drop_frames: usize, dt: u64 },
    /// Full thread unwind (`_Unwind_RaiseException`).
    UnwindAll { dt: u64 },
}

fn draw_op(tape: &Tape) -> Op {
    let slot = tape.int_in(0..=15, "slot") as u32;
    let dt = tape.int_in(0..=50, "dt") as u64;
    match tape.int_in(0..=9, "op") {
        0..=3 => Op::Call { slot, delta: (tape.int_in(1..=4, "frame") as u64) * 16, dt },
        4 | 5 => Op::TailCall { slot, dt },
        6 | 7 => Op::Return { dt },
        8 => Op::Unwind { drop_frames: tape.int_in(1..=4, "drop"), dt },
        _ => Op::UnwindAll { dt },
    }
}

fn simulate(ops: &[Op], plant: ShadowPlant) -> Result<(), String> {
    let mut stack = ShadowStack::planted(plant);
    let mut model: Vec<ModelFrame> = Vec::new();
    let mut seen_spans: HashSet<SpanId> = HashSet::new();
    let mut sp: u64 = 0x0001_0000_0000;
    let mut ts: u64 = 1000;
    let mut next_lr: u64 = 0x2000_0000;

    for (step, &op) in ops.iter().enumerate() {
        match op {
            Op::Call { slot, delta, dt } => {
                ts += dt;
                sp = sp.saturating_sub(delta);
                let real_lr = next_lr;
                next_lr += 4;
                let mut unwound = Vec::new();
                let outcome = stack.enter(Slot(slot), real_lr, sp, ts, |u| unwound.push(u));
                if !unwound.is_empty() {
                    return Err(format!("step {step}: Call at lower sp unexpectedly unwound {unwound:?}"));
                }
                let hooked_depth = model.iter().filter(|f| f.hooked).count();
                let expected_parent = model.iter().rev().find(|f| f.hooked).map_or(SpanId::NONE, |f| f.span);
                match outcome {
                    EnterOutcome::Hooked { span, parent } => {
                        if hooked_depth >= SHADOW_DEPTH {
                            return Err(format!("step {step}: accepted Hooked past SHADOW_DEPTH"));
                        }
                        if parent != expected_parent {
                            return Err(format!("step {step}: parent {parent:?} != expected {expected_parent:?}"));
                        }
                        if !seen_spans.insert(span) {
                            return Err(format!("step {step}: duplicate SpanId {span:?}"));
                        }
                        model.push(ModelFrame {
                            slot: Slot(slot),
                            real_lr,
                            caller_sp: sp,
                            start_ns: ts,
                            span,
                            parent,
                            hooked: true,
                        });
                    }
                    EnterOutcome::Overflow => {
                        if hooked_depth < SHADOW_DEPTH {
                            return Err(format!("step {step}: overflowed early at depth {hooked_depth}"));
                        }
                        model.push(ModelFrame {
                            slot: Slot(slot),
                            real_lr,
                            caller_sp: sp,
                            start_ns: ts,
                            span: SpanId::NONE,
                            parent: SpanId::NONE,
                            hooked: false,
                        });
                    }
                }
            }
            Op::TailCall { slot, dt } => {
                ts += dt;
                // Equal sp to current frame (tail call!).
                let real_lr = next_lr;
                next_lr += 4;
                let mut unwound = Vec::new();
                let outcome = stack.enter(Slot(slot), real_lr, sp, ts, |u| unwound.push(u));
                if !unwound.is_empty() {
                    return Err(format!("step {step}: TailCall at equal sp {sp:#x} wrongly unwound {unwound:?}"));
                }
                let hooked_depth = model.iter().filter(|f| f.hooked).count();
                let expected_parent = model.iter().rev().find(|f| f.hooked).map_or(SpanId::NONE, |f| f.span);
                match outcome {
                    EnterOutcome::Hooked { span, parent } => {
                        if hooked_depth >= SHADOW_DEPTH {
                            return Err(format!("step {step}: TailCall accepted past SHADOW_DEPTH"));
                        }
                        if parent != expected_parent {
                            return Err(format!("step {step}: TailCall parent {parent:?} != {expected_parent:?}"));
                        }
                        if !seen_spans.insert(span) {
                            return Err(format!("step {step}: duplicate SpanId {span:?}"));
                        }
                        model.push(ModelFrame {
                            slot: Slot(slot),
                            real_lr,
                            caller_sp: sp,
                            start_ns: ts,
                            span,
                            parent,
                            hooked: true,
                        });
                    }
                    EnterOutcome::Overflow => {
                        if hooked_depth < SHADOW_DEPTH {
                            return Err(format!("step {step}: TailCall overflowed early at {hooked_depth}"));
                        }
                        model.push(ModelFrame {
                            slot: Slot(slot),
                            real_lr,
                            caller_sp: sp,
                            start_ns: ts,
                            span: SpanId::NONE,
                            parent: SpanId::NONE,
                            hooked: false,
                        });
                    }
                }
            }
            Op::Return { dt } => {
                ts += dt;
                let Some(top) = model.pop() else {
                    if stack.depth() != 0 {
                        return Err(format!("step {step}: model empty but stack depth {}", stack.depth()));
                    }
                    continue;
                };
                if top.hooked {
                    let mut unwound = Vec::new();
                    let left = stack
                        .leave(top.caller_sp, ts, |u| unwound.push(u))
                        .map_err(|e| format!("step {step}: unexpected leave error: {e}"))?;
                    if !unwound.is_empty() {
                        return Err(format!("step {step}: normal Return unwound {unwound:?}"));
                    }
                    if left.real_lr != top.real_lr || left.slot != top.slot || left.span != top.span || left.parent != top.parent {
                        return Err(format!("step {step}: left {left:?} != model {top:?}"));
                    }
                    if left.duration_ns != ts - top.start_ns {
                        return Err(format!("step {step}: duration {} != {}", left.duration_ns, ts - top.start_ns));
                    }
                }
                sp = model.last().map_or(0x0001_0000_0000, |f| f.caller_sp);
            }
            Op::Unwind { drop_frames, dt } => {
                ts += dt;
                if model.is_empty() {
                    continue;
                }
                // Simulate a panic caught at an ancestor frame with a strictly larger sp:
                // drop `drop_frames` plus any tail-call siblings sharing the last dropped sp so
                // the catch site's sp is strictly above the unwound frames.
                let keep = model.len().saturating_sub(drop_frames);
                let catch_sp = if keep == 0 { 0x0001_0000_0000 } else { model[keep - 1].caller_sp };
                let mut expected_unwound = Vec::new();
                while let Some(&top) = model.last() {
                    if top.caller_sp < catch_sp {
                        model.pop();
                        if top.hooked {
                            expected_unwound.push(top);
                        }
                    } else {
                        break;
                    }
                }
                sp = catch_sp;
                // Next call or return at `catch_sp` heals the unwound frames.
                let mut got_unwound = Vec::new();
                if let Some(top) = model.pop() {
                    if top.hooked {
                        let left = stack
                            .leave(top.caller_sp, ts, |u| got_unwound.push(u))
                            .map_err(|e| format!("step {step}: leave after unwind failed: {e}"))?;
                        if left.real_lr != top.real_lr || left.span != top.span {
                            return Err(format!("step {step}: post-unwind leave {left:?} != {top:?}"));
                        }
                    } else {
                        let mut dummy = Vec::new();
                        let _ = stack.enter(Slot(0), 0x9999, sp, ts, |u| got_unwound.push(u));
                        let _ = stack.leave(sp, ts, |u| dummy.push(u));
                    }
                    sp = model.last().map_or(0x0001_0000_0000, |f| f.caller_sp);
                } else {
                    // Nothing left on model; unwind_all drains whatever was unwound.
                    stack.unwind_all(ts, |u| got_unwound.push(u));
                }
                if got_unwound.len() != expected_unwound.len() {
                    return Err(format!(
                        "step {step}: unwound {} frames, expected {}",
                        got_unwound.len(),
                        expected_unwound.len()
                    ));
                }
                for (got, exp) in got_unwound.iter().zip(expected_unwound.iter()) {
                    if got.real_lr != exp.real_lr || got.span != exp.span {
                        return Err(format!("step {step}: unwound {got:?} != expected {exp:?}"));
                    }
                }
            }
            Op::UnwindAll { dt } => {
                ts += dt;
                let mut got = Vec::new();
                stack.unwind_all(ts, |u| got.push(u));
                let expected: Vec<ModelFrame> = model.drain(..).rev().filter(|f| f.hooked).collect();
                if got.len() != expected.len() {
                    return Err(format!("step {step}: unwind_all got {}, expected {}", got.len(), expected.len()));
                }
                for (g, e) in got.iter().zip(expected.iter()) {
                    if g.real_lr != e.real_lr || g.span != e.span {
                        return Err(format!("step {step}: unwind_all {g:?} != {e:?}"));
                    }
                }
                sp = 0x0001_0000_0000;
            }
        }
    }
    let expected_depth = model.iter().filter(|f| f.hooked).count();
    if stack.depth() != expected_depth {
        return Err(format!("end: depth {} != expected {expected_depth}", stack.depth()));
    }
    Ok(())
}

/// The refinement: `ShadowStack` refines the sequential reference model across nested calls, tail calls at equal stack pointer, unwinds, and unique span ids.
#[test]
fn shadow_stack_refines_model_under_calls_tail_calls_and_unwinds() {
    blaze_sim::check(blaze_sim::Config::cases(300), |tape| {
        let ops = tape.vec(1..=80, "ops", draw_op);
        simulate(&ops, ShadowPlant::None)
    });
}

#[test]
fn shadow_stack_deep_recursion_overflows_cleanly_without_corrupting_lr() {
    let mut ops = Vec::new();
    for i in 0..(SHADOW_DEPTH + 16) {
        ops.push(Op::Call { slot: (i % 8) as u32, delta: 16, dt: 1 });
    }
    for _ in 0..(SHADOW_DEPTH + 16) {
        ops.push(Op::Return { dt: 1 });
    }
    assert_eq!(simulate(&ops, ShadowPlant::None), Ok(()));
    assert!(
        simulate(&ops, ShadowPlant::OverwriteOnFull).is_err(),
        "the oracle must catch OverwriteOnFull"
    );
}

#[test]
fn the_oracle_catches_heal_on_equal_sp_breaking_tail_calls() {
    let ops = [
        Op::Call { slot: 1, delta: 16, dt: 1 },
        Op::TailCall { slot: 2, dt: 1 },
        Op::Return { dt: 1 },
        Op::Return { dt: 1 },
    ];
    assert_eq!(simulate(&ops, ShadowPlant::None), Ok(()));
    assert!(simulate(&ops, ShadowPlant::HealOnEqualSp).is_err());
}

#[test]
fn the_oracle_catches_reused_span_ids() {
    let ops = [
        Op::Call { slot: 1, delta: 16, dt: 1 },
        Op::Call { slot: 2, delta: 16, dt: 1 },
    ];
    assert_eq!(simulate(&ops, ShadowPlant::None), Ok(()));
    assert!(simulate(&ops, ShadowPlant::ReuseSpanId).is_err());
}

#[test]
fn leaving_an_empty_shadow_stack_is_a_typed_error() {
    let mut stack = ShadowStack::new();
    assert_eq!(
        stack.leave(0x1000, 10, |_| {}),
        Err(ShadowError::Empty { current_sp: 0x1000 })
    );
}
