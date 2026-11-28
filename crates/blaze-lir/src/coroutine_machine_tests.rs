//! Unit tests and planted-bug controls for [`crate::coroutine_machine`], the zero-`Pin` sans-IO
//! Machine ABI lowering and verification pass.

use std::alloc::Global;

use crate::{
    Block, BlockKind, Builder, Flags, Full, Func, IntCond, Op, Ty, View,
    builder::{MemAttrs, Param, Variadic},
    coroutine::{FIRST_POINT, RETURNED, Saved},
    coroutine_machine::{
        MachineEdge, MachineEdgeKind, MachineError, MachinePlant, MachineShape, split_machine,
        split_machine_with_plant,
    },
    func::{Conv, RefKind, SlotId, SlotKind},
    verify,
};

const PREFIX: u32 = 16;
const STATE: u32 = 0;
const PENDING: u64 = 0;
const READY: u64 = 1;

/// `(state: ptr, cx: i64) -> (i64, i64)`: matches the standard coroutine poll/step signature.
fn set_machine_sig(b: &mut Builder<Global, Full>) {
    let sig = b.sig(
        Conv::C,
        Variadic::No,
        &[Param::of(Ty::PTR), Param::of(Ty::I64)],
        &[Param::of(Ty::I64), Param::of(Ty::I64)],
    );
    b.set_signature(sig);
}

/// Builds a valid two-suspend `.do` body with an SSA value live across both suspends and zero
/// stack-slot addresses escaping across any suspend.
fn build_two_suspend_body() -> (Func<Global>, MachineShape) {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    set_machine_sig(&mut b);
    let observe_sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[]);
    let observe = b.reference(RefKind::Func, 0);

    let [entry, resume0, resume1, cancel0, cancel1, unresumed] =
        [(); 6].map(|_| b.create_block(BlockKind::Normal));

    b.begin(entry);
    let (state, cx) = (b.param(Ty::PTR), b.param(Ty::I64));
    let upvar = b.load(Ty::I64, Flags::NONE, state, 0, MemAttrs::default());
    let live_across = b.binary(Op::Add, Flags::NONE, upvar, cx);
    let pending = b.iconst(Ty::I64, u128::from(PENDING));
    b.suspend(0, resume0, cancel0, &[pending, pending]);

    b.begin(resume0);
    let step1 = b.binary(Op::Add, Flags::NONE, live_across, cx);
    let pending1 = b.iconst(Ty::I64, u128::from(PENDING));
    b.suspend(1, resume1, cancel1, &[pending1, pending1]);

    b.begin(resume1);
    let total = b.binary(Op::Add, Flags::NONE, step1, live_across);
    let ready = b.iconst(Ty::I64, u128::from(READY));
    b.ret(&[ready, total]);

    b.begin(cancel0);
    b.call(Flags::NONE, observe, observe_sig, &[live_across]);
    b.cancelled();

    b.begin(cancel1);
    b.call(Flags::NONE, observe, observe_sig, &[step1]);
    b.cancelled();

    b.begin(unresumed);
    b.cancelled();

    let func = b.finish_checked().expect("two-suspend body is valid LIR");
    let shape = MachineShape {
        state_param: STATE,
        prefix_size: PREFIX,
        prefix_align: 3,
        unresumed: Block(5),
        grace_max_steps: 2,
    };
    (func, shape)
}

/// Builds a body where a local stack slot's address IS live across `Op::Suspend` (violating
/// zero-`Pin`).
fn build_escaping_slot_body() -> (Func<Global>, MachineShape, SlotId) {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    set_machine_sig(&mut b);
    let [entry, resume, cancel, unresumed] = [(); 4].map(|_| b.create_block(BlockKind::Normal));
    let slot = b.slot(8, 3, SlotKind::Named);

    b.begin(entry);
    let (state, cx) = (b.param(Ty::PTR), b.param(Ty::I64));
    let upvar = b.load(Ty::I64, Flags::NONE, state, 0, MemAttrs::default());
    let addr = b.slot_addr(slot);
    b.store(Flags::NONE, upvar, addr, 0, MemAttrs::default());
    let pending = b.iconst(Ty::I64, u128::from(PENDING));
    b.suspend(0, resume, cancel, &[pending, pending]);

    b.begin(resume);
    let back = b.load(Ty::I64, Flags::NONE, addr, 0, MemAttrs::default());
    let out = b.binary(Op::Add, Flags::NONE, back, cx);
    let ready = b.iconst(Ty::I64, u128::from(READY));
    b.ret(&[ready, out]);

    b.begin(cancel);
    b.cancelled();

    b.begin(unresumed);
    b.cancelled();

    let func = b.finish_checked().expect("escaping-slot body is valid LIR");
    let shape = MachineShape {
        state_param: STATE,
        prefix_size: PREFIX,
        prefix_align: 3,
        unresumed: Block(3),
        grace_max_steps: 1,
    };
    (func, shape, slot)
}

/// Collects every constant tag value stored to `tag_offset` in `view`.
fn tag_stores(view: View<'_>, tag_offset: u32) -> Vec<u32> {
    let mut stored = Vec::new();
    for inst in 0..view.len() {
        if view.op(inst) != Op::Store {
            continue;
        }
        let mut o = view.operands(inst);
        let value = o.value();
        let _ptr = o.value();
        let offset = o.offset();
        if offset == tag_offset as i32 && view.op(value.index()) == Op::Iconst {
            let mut val_o = view.operands(value.index());
            stored.push(val_o.imm());
        }
    }
    stored
}

/// Extracts `(tag, target_block)` pairs from the entry `Op::Switch` of `step`.
fn entry_switch_cases(view: View<'_>) -> Vec<(u32, Block)> {
    let term = view.terminator(Block(0));
    assert_eq!(view.op(term), Op::Switch, "step entry must dispatch on state tag");
    let mut o = view.operands(term);
    let scrutinee = o.value();
    let _default = o.target();
    let mut cases = Vec::new();
    for (words, target) in o.cases(scrutinee) {
        cases.push((words[0], target.block));
    }
    cases
}

#[test]
fn valid_multi_suspend_machine_lowers_with_zero_pin_and_exact_graph() {
    let (func, shape) = build_two_suspend_body();
    let split = split_machine(func.view(), shape, Global).expect("valid two-suspend machine lowers");

    assert!(split.graph.zero_pin_proven);
    assert_eq!(split.graph.points, 2);
    assert_eq!(split.graph.state_size, split.layout.size);
    assert_eq!(split.graph.state_align, split.layout.align);

    // Both lowered functions must pass full LIR verification.
    verify::verify(split.step.view()).expect("lowered step function is valid LIR");
    verify::verify(split.on_cancel.view()).expect("lowered on_cancel function is valid LIR");

    // Zero stack slots should be saved in state (only SSA values cross suspends).
    assert!(
        split.layout.fields.iter().all(|f| matches!(f.what, Saved::Value(_))),
        "zero-Pin machine must not pin stack slots in state: {:?}",
        split.layout.fields
    );

    // Graph must capture entry -> point 0 -> point 1 -> Return plus all Cancel edges.
    let expected = [
        MachineEdge { from_point: None, to_point: None, kind: MachineEdgeKind::Cancel },
        MachineEdge { from_point: None, to_point: Some(0), kind: MachineEdgeKind::Suspend },
        MachineEdge { from_point: Some(0), to_point: None, kind: MachineEdgeKind::Cancel },
        MachineEdge { from_point: Some(0), to_point: Some(1), kind: MachineEdgeKind::Suspend },
        MachineEdge { from_point: Some(1), to_point: None, kind: MachineEdgeKind::Return },
        MachineEdge { from_point: Some(1), to_point: None, kind: MachineEdgeKind::Cancel },
    ];
    assert_eq!(&split.graph.edges[..], &expected[..]);
}

#[test]
fn slot_killed_before_suspend_is_allowed_and_zero_pin_proven() {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    set_machine_sig(&mut b);
    let [entry, resume, cancel, unresumed] = [(); 4].map(|_| b.create_block(BlockKind::Normal));
    let tmp = b.slot(8, 3, SlotKind::Temp);

    b.begin(entry);
    let (state, cx) = (b.param(Ty::PTR), b.param(Ty::I64));
    let upvar = b.load(Ty::I64, Flags::NONE, state, 0, MemAttrs::default());
    b.slot_live(tmp);
    let addr = b.slot_addr(tmp);
    b.store(Flags::NONE, upvar, addr, 0, MemAttrs::default());
    let loaded = b.load(Ty::I64, Flags::NONE, addr, 0, MemAttrs::default());
    b.slot_dead(tmp);
    let pending = b.iconst(Ty::I64, u128::from(PENDING));
    b.suspend(0, resume, cancel, &[pending, pending]);

    b.begin(resume);
    let out = b.binary(Op::Add, Flags::NONE, loaded, cx);
    let ready = b.iconst(Ty::I64, u128::from(READY));
    b.ret(&[ready, out]);

    b.begin(cancel);
    b.cancelled();

    b.begin(unresumed);
    b.cancelled();

    let func = b.finish_checked().expect("temp-slot body is valid LIR");
    let shape = MachineShape {
        state_param: STATE,
        prefix_size: PREFIX,
        prefix_align: 3,
        unresumed: Block(3),
        grace_max_steps: 0,
    };

    let split = split_machine(func.view(), shape, Global)
        .expect("slot killed with SlotDead before Suspend must be allowed");
    assert!(split.graph.zero_pin_proven);
    assert_eq!(split.graph.points, 1);
    verify::verify(split.step.view()).expect("step is valid LIR");
    verify::verify(split.on_cancel.view()).expect("on_cancel is valid LIR");
    assert!(
        split.layout.fields.iter().all(|f| f.what != Saved::Slot(tmp)),
        "dead temp slot must not be kept in state layout"
    );
}

#[test]
fn live_slot_addr_across_suspend_is_rejected() {
    let (func, shape, slot) = build_escaping_slot_body();
    let err = split_machine(func.view(), shape, Global)
        .expect_err("live SlotAddr across Suspend must be rejected");
    match err {
        MachineError::LocalSlotAddrAcrossSuspend { slot: found, point, inst } => {
            assert_eq!(found, slot);
            assert_eq!(point, 0);
            assert_eq!(func.view().op(inst as usize), Op::Suspend);
        }
        other => panic!("expected LocalSlotAddrAcrossSuspend, got {other:?}"),
    }
}

#[test]
fn partial_path_slot_addr_across_suspend_is_rejected() {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    set_machine_sig(&mut b);
    let [entry, killed_branch, live_branch, merge, resume, cancel, unresumed] =
        [(); 7].map(|_| b.create_block(BlockKind::Normal));
    let slot = b.slot(8, 3, SlotKind::Named);

    b.begin(entry);
    let (state, cx) = (b.param(Ty::PTR), b.param(Ty::I64));
    let zero = b.iconst(Ty::I64, 0);
    let cond = b.icmp(IntCond::Eq, cx, zero);
    b.slot_live(slot);
    let addr = b.slot_addr(slot);
    let upvar = b.load(Ty::I64, Flags::NONE, state, 0, MemAttrs::default());
    b.store(Flags::NONE, upvar, addr, 0, MemAttrs::default());
    b.br(Flags::NONE, cond, (killed_branch, &[]), (live_branch, &[]));

    b.begin(killed_branch);
    b.slot_dead(slot);
    b.jump((merge, &[]));

    b.begin(live_branch);
    // Intentionally omits `slot_dead(slot)` on this branch.
    b.jump((merge, &[]));

    b.begin(merge);
    let pending = b.iconst(Ty::I64, u128::from(PENDING));
    b.suspend(0, resume, cancel, &[pending, pending]);

    b.begin(resume);
    let ready = b.iconst(Ty::I64, u128::from(READY));
    b.ret(&[ready, cx]);

    b.begin(cancel);
    b.cancelled();

    b.begin(unresumed);
    b.cancelled();

    let func = b.finish_checked().expect("diamond body is valid LIR");
    let shape = MachineShape {
        state_param: STATE,
        prefix_size: PREFIX,
        prefix_align: 3,
        unresumed: Block(6),
        grace_max_steps: 0,
    };

    let err = split_machine(func.view(), shape, Global)
        .expect_err("slot live on one path into Suspend must be rejected");
    assert!(
        matches!(err, MachineError::LocalSlotAddrAcrossSuspend { slot: found, point: 0, .. } if found == slot),
        "unexpected error: {err:?}"
    );
}

#[test]
fn control_allow_escaped_slot_addr_plant_bypasses_zero_pin_check() {
    let (func, shape, slot) = build_escaping_slot_body();
    let split = split_machine_with_plant(
        func.view(),
        shape,
        MachinePlant::AllowEscapedSlotAddr,
        Global,
    )
    .expect("AllowEscapedSlotAddr plant must let escaping-slot body pass");

    assert!(!split.graph.zero_pin_proven, "planted bypass must report zero_pin_proven == false");
    assert!(
        split.layout.fields.iter().any(|f| f.what == Saved::Slot(slot)),
        "without the zero-Pin check, the escaping slot is pinned into StateLayout"
    );
}

#[test]
fn control_skip_tag_store_and_wrong_resume_target_diverge_in_emitted_lir() {
    let (func, shape) = build_two_suspend_body();
    let clean = split_machine_with_plant(func.view(), shape, MachinePlant::None, Global)
        .expect("clean two-suspend machine lowers");

    // Control 1: SkipTagStore omits the FIRST_POINT + k tag stores before returning on Suspend.
    let skip_tag = split_machine_with_plant(func.view(), shape, MachinePlant::SkipTagStore, Global)
        .expect("SkipTagStore plant lowers");
    verify::verify(skip_tag.step.view()).expect("SkipTagStore step is still structurally valid LIR");

    let clean_tags = tag_stores(clean.step.view(), clean.layout.tag_offset);
    let skipped_tags = tag_stores(skip_tag.step.view(), skip_tag.layout.tag_offset);
    assert!(clean_tags.contains(&FIRST_POINT));
    assert!(clean_tags.contains(&(FIRST_POINT + 1)));
    assert!(clean_tags.contains(&RETURNED));
    assert!(
        !skipped_tags.contains(&FIRST_POINT) && !skipped_tags.contains(&(FIRST_POINT + 1)),
        "SkipTagStore must omit suspend tag stores; got {skipped_tags:?}"
    );
    assert!(skipped_tags.contains(&RETURNED));

    // Control 2: WrongResumeTarget swaps the entry Switch targets for tags FIRST_POINT and
    // FIRST_POINT + 1.
    let wrong_resume =
        split_machine_with_plant(func.view(), shape, MachinePlant::WrongResumeTarget, Global)
            .expect("WrongResumeTarget plant lowers");
    verify::verify(wrong_resume.step.view())
        .expect("WrongResumeTarget step is still structurally valid LIR");

    let clean_cases = entry_switch_cases(clean.step.view());
    let wrong_cases = entry_switch_cases(wrong_resume.step.view());
    let find_target = |cases: &[(u32, Block)], tag: u32| -> Block {
        cases
            .iter()
            .find_map(|&(t, blk)| (t == tag).then_some(blk))
            .expect("switch has case for suspend point tag")
    };
    let clean_p0 = find_target(&clean_cases, FIRST_POINT);
    let clean_p1 = find_target(&clean_cases, FIRST_POINT + 1);
    let wrong_p0 = find_target(&wrong_cases, FIRST_POINT);
    let wrong_p1 = find_target(&wrong_cases, FIRST_POINT + 1);
    assert_ne!(clean_p0, clean_p1);
    assert_eq!(wrong_p0, clean_p1, "WrongResumeTarget must misroute point 0 to point 1's resume");
    assert_eq!(wrong_p1, clean_p0, "WrongResumeTarget must misroute point 1 to point 0's resume");
}

#[test]
fn unreachable_suspend_block_reports_unreachable_resume() {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    set_machine_sig(&mut b);
    let [entry, dead_suspend, dead_resume, dead_cancel, unresumed] =
        [(); 5].map(|_| b.create_block(BlockKind::Normal));

    b.begin(entry);
    let (_state, cx) = (b.param(Ty::PTR), b.param(Ty::I64));
    let ready = b.iconst(Ty::I64, u128::from(READY));
    b.ret(&[ready, cx]);

    b.begin(dead_suspend);
    let pending = b.iconst(Ty::I64, u128::from(PENDING));
    b.suspend(9, dead_resume, dead_cancel, &[pending, pending]);

    b.begin(dead_resume);
    let ready2 = b.iconst(Ty::I64, u128::from(READY));
    b.ret(&[ready2, ready2]);

    b.begin(dead_cancel);
    b.cancelled();

    b.begin(unresumed);
    b.cancelled();

    let func = b.finish_checked().expect("dead-suspend body is valid LIR");
    let shape = MachineShape {
        state_param: STATE,
        prefix_size: PREFIX,
        prefix_align: 3,
        unresumed: Block(4),
        grace_max_steps: 0,
    };

    let err = split_machine(func.view(), shape, Global)
        .expect_err("unreachable suspend must be rejected");
    assert_eq!(err, MachineError::UnreachableResume { point: 9 });
}
