//! A split into a state laid out BEFORE the LIR (coroutine_region.rs; ASYNC.md P8 option (b),
//! main's rulings of 2026-11-16, async-P8-4: the flow layer is the one owner of what crosses a
//! suspension; the builder names each slot that holds a saved local), on this file's
//! interpreter.
//!
//! The body keeps one 8-byte slot (a saved local) across its one suspend. Laid from types
//! (blaze-build coroutine_state.rs `lay_state((16, 3), &[(8, 3)], &[false], 1, ..)`): the upvars
//! at 0..16, the local at 16..24, an `i8` tag at 24, 32 bytes aligned to 8. The builder names
//! the slot at 16; the split places it there and takes the rest as given.

use std::alloc::Global;

use super::{PENDING, PREFIX, READY, STATE, agree, own_sig};
use crate::{
    Block, BlockKind, Builder, Flags, Full, Func, IntCond, Op, Ty,
    builder::MemAttrs,
    coroutine::{CoroutineError, Panics, Region, Saved, Shape, SlotName, StateField, split_in},
    func::{SlotId, SlotKind},
};

/// What [`body`] builds besides the saved slot.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Extra {
    None,
    /// `upvar + cx`, made before the suspend and used after it: a value of the builder crossing.
    Value,
    /// A second slot written and read before the suspend only: the builder's own scratch.
    ScratchBefore,
    /// The scratch slot's address stored into the saved slot before the suspend and read
    /// through after it: no `slot_addr` of the scratch after the resume, yet its bytes are read.
    Escaped,
}

/// `let kept = upvar; suspend; return kept`, and `extra`. Gives the body, the saved slot, the
/// value `upvar + cx`, and the scratch slot (when made).
fn body(extra: Extra) -> (Func<Global>, SlotId, crate::Value, Option<SlotId>) {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    own_sig(&mut b);
    let [entry, resume, cancel, unresumed] = [(); 4].map(|_| b.create_block(BlockKind::Normal));
    let kept = b.slot(8, 3, SlotKind::Named);
    let scratch = matches!(extra, Extra::ScratchBefore | Extra::Escaped).then(|| b.slot(8, 3, SlotKind::Temp));
    b.begin(entry);
    let (state, cx) = (b.param(Ty::PTR), b.param(Ty::I64));
    let at = b.slot_addr(kept);
    let upvar = b.load(Ty::I64, Flags::NONE, state, 0, MemAttrs::default());
    b.store(Flags::NONE, upvar, at, 0, MemAttrs::default());
    if let Some(s) = scratch {
        let sa = b.slot_addr(s);
        b.store(Flags::NONE, cx, sa, 0, MemAttrs::default());
        let _ = b.load(Ty::I64, Flags::NONE, sa, 0, MemAttrs::default());
        if extra == Extra::Escaped {
            // The scratch's address is the saved local's value.
            b.store(Flags::NONE, sa, at, 0, MemAttrs::default());
        }
    }
    let sum = b.binary(Op::Add, Flags::NONE, upvar, cx);
    let pending = b.iconst(Ty::I64, u128::from(PENDING));
    b.suspend(0, resume, cancel, &[pending, pending]);
    b.begin(resume);
    let out = if extra == Extra::Escaped {
        let p = b.load(Ty::PTR, Flags::NONE, at, 0, MemAttrs::default());
        b.load(Ty::I64, Flags::NONE, p, 0, MemAttrs::default())
    } else {
        let back = b.load(Ty::I64, Flags::NONE, at, 0, MemAttrs::default());
        if extra == Extra::Value { b.binary(Op::Add, Flags::NONE, back, sum) } else { back }
    };
    let ready = b.iconst(Ty::I64, u128::from(READY));
    b.ret(&[ready, out]);
    b.begin(cancel);
    b.cancelled();
    b.begin(unresumed);
    b.cancelled();
    (b.finish_checked().expect("the body is LIR"), kept, sum, scratch)
}

const SHAPE: Shape = Shape { state_param: STATE as u32, prefix_size: PREFIX, prefix_align: 3, unresumed: Block(3) };
const LAID: Region = Region { start: 16, end: 24, tag_offset: 24, tag_ty: Ty::I8, size: 32, align: 3 };

fn named(kept: SlotId) -> [SlotName; 1] {
    [SlotName { slot: kept, offset: 16 }]
}

/// The split keeps the laid size, alignment and tag, places the named slot at its offset, and
/// runs as the body on every schedule.
/// Prints "a split inside a region keeps its layout: 32 bytes, tag at 24, the local at 16".
#[test]
fn a_split_inside_a_region_keeps_its_layout() {
    let (func, kept, _, _) = body(Extra::None);
    let split = split_in(func.view(), SHAPE, Panics::NONE, LAID, &named(kept), Global).expect("the body splits into its region");
    let l = &split.layout;
    assert_eq!((l.size, l.align, l.tag_offset, l.tag_ty, l.points), (32, 3, 24, Ty::I8, 1));
    assert_eq!(&l.fields[..], &[StateField { what: Saved::Slot(kept), offset: 16, size: 8, align: 3 }]);
    if let Err(broke) = agree(func.view(), SHAPE, &split, [5, 9]) {
        panic!("{broke}");
    }
    std::println!("a split inside a region keeps its layout: 32 bytes, tag at 24, the local at 16");
}

/// THE PLANT, at this layer: the same slot with NO name (a slot the builder made that the flow
/// does not save) is reached after the resume, so it is refused by name, never given bytes.
/// Prints "a builder-made slot across a suspend is UnnamedAcrossSuspend".
#[test]
fn a_builder_made_slot_across_a_suspend_is_refused() {
    let (func, kept, _, _) = body(Extra::None);
    let got = split_in(func.view(), SHAPE, Panics::NONE, LAID, &[], Global).err();
    assert_eq!(got, Some(CoroutineError::UnnamedAcrossSuspend { slot: kept.0 }));
    std::println!("a builder-made slot across a suspend is UnnamedAcrossSuspend");
}

/// A builder slot used only before the suspend (its storage never ended: the pass's own rule
/// would keep it) stays on the stack: no code after the suspend reaches it; the state holds the
/// named local alone, and the split runs as the body.
/// Prints "a scratch slot used before the suspend stays on the stack".
#[test]
fn a_scratch_slot_before_the_suspend_is_not_kept() {
    let (func, kept, _, scratch) = body(Extra::ScratchBefore);
    let split = split_in(func.view(), SHAPE, Panics::NONE, LAID, &named(kept), Global).expect("the scratch slot needs no name");
    let scratch = scratch.expect("made");
    assert!(split.layout.fields.iter().all(|f| f.what != Saved::Slot(scratch)), "{:?}", split.layout.fields);
    if let Err(broke) = agree(func.view(), SHAPE, &split, [5, 9]) {
        panic!("{broke}");
    }
    std::println!("a scratch slot used before the suspend stays on the stack");
}

/// The cause of the scratch's stay on the stack removed: its address escapes into the saved
/// local and is read through after the resume, with no `slot_addr` of it there. It counts as
/// reached and, unnamed, is refused by name (on the stack it would be read after its poll ended).
/// Prints "an escaped scratch slot is UnnamedAcrossSuspend".
#[test]
fn an_escaped_scratch_slot_is_refused() {
    let (func, kept, _, scratch) = body(Extra::Escaped);
    let scratch = scratch.expect("made");
    let got = split_in(func.view(), SHAPE, Panics::NONE, LAID, &named(kept), Global).err();
    assert_eq!(got, Some(CoroutineError::UnnamedAcrossSuspend { slot: scratch.0 }));
    std::println!("an escaped scratch slot is UnnamedAcrossSuspend");
}

/// Control: the region one byte short of the named slot: refused by name, never laid past it.
/// Prints "control: a name past the region is NameOutside".
#[test]
fn control_a_name_past_the_region_is_refused() {
    let (func, kept, _, _) = body(Extra::None);
    let short = Region { end: 23, ..LAID };
    let got = split_in(func.view(), SHAPE, Panics::NONE, short, &named(kept), Global).err();
    assert_eq!(got, Some(CoroutineError::NameOutside { slot: kept.0, offset: 16 }));
    std::println!("control: a name past the region is NameOutside");
}

/// A value of the builder crossing the suspend (`upvar + cx`, in no slot): refused by name in a
/// laid state; without a region the same body keeps it as a value field (the old split).
/// Prints "a builder value across a suspend is refused in a laid state".
#[test]
fn a_builder_value_across_a_suspend_is_refused() {
    let (func, kept, sum, _) = body(Extra::Value);
    let got = split_in(func.view(), SHAPE, Panics::NONE, LAID, &named(kept), Global).err();
    assert_eq!(got, Some(CoroutineError::ValueAcrossSuspend { value: sum.index() as u32 }));
    let old = crate::coroutine::split(func.view(), SHAPE, Global).expect("without a region the value is a field");
    assert!(old.layout.fields.iter().any(|f| f.what == Saved::Value(sum)));
    std::println!("a builder value across a suspend is refused in a laid state");
}

/// A region whose tag is not the width the points need: refused by name.
/// Prints "a region with another tag width is RegionTag".
#[test]
fn a_region_with_another_tag_width_is_refused() {
    let (func, kept, _, _) = body(Extra::None);
    let wide = Region { tag_ty: Ty::I16, ..LAID };
    assert_eq!(split_in(func.view(), SHAPE, Panics::NONE, wide, &named(kept), Global).err(), Some(CoroutineError::RegionTag { points: 1 }));
    std::println!("a region with another tag width is RegionTag");
}

/// A 0-suspend laid-state coroutine whose `invoke` unwinds to a `Cleanup` block using a
/// stack-only `SlotAddr` and an SSA value made in the entry block: neither crosses a suspend,
/// so `split_in` accepts it (no false `ValueAcrossSuspend`) and runs identically on normal
/// and unwinding schedules.
#[test]
fn a_zero_suspend_invoke_cleanup_keeps_stack_slot_and_value_without_crossing() {
    use crate::builder::{Param, Variadic};
    use crate::func::{Conv, RefKind};
    let heap = blaze_own::testing::heap();
    let a = heap.alloc();
    let mut b = Builder::<_, Full>::new_in(64, a);
    own_sig(&mut b);
    let observe_sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[]);
    let observe = b.reference(RefKind::Func, 0);
    let may_unwind = b.reference(RefKind::Func, 1);
    let [entry, normal, unresumed] = [(); 3].map(|_| b.create_block(BlockKind::Normal));
    let pad = b.create_block(BlockKind::Cleanup);
    let local = b.slot(8, 3, SlotKind::Named);
    b.begin(entry);
    let (state, cx) = (b.param(Ty::PTR), b.param(Ty::I64));
    let at = b.slot_addr(local);
    let upvar = b.load(Ty::I64, Flags::NONE, state, 0, MemAttrs::default());
    let sum = b.binary(Op::Add, Flags::NONE, upvar, cx);
    b.store(Flags::NONE, sum, at, 0, MemAttrs::default());
    b.invoke(Flags::NONE, may_unwind, observe_sig, &[cx], normal, pad);
    b.begin(normal);
    let out = b.load(Ty::I64, Flags::NONE, at, 0, MemAttrs::default());
    let ready = b.iconst(Ty::I64, u128::from(READY));
    b.ret(&[ready, out]);
    b.begin(pad);
    let exception = b.param(Ty::PTR);
    // Uses both the non-kept stack slot's `slot_addr` (`at`) and the SSA value `sum` from entry.
    let saved = b.load(Ty::I64, Flags::NONE, at, 0, MemAttrs::default());
    b.call(Flags::NONE, observe, observe_sig, &[saved]);
    b.call(Flags::NONE, observe, observe_sig, &[sum]);
    b.resume(exception);
    b.begin(unresumed);
    b.cancelled();
    let func = b.finish_checked().expect("the body is LIR");
    // Begun in order: entry 0, normal 1, pad 2, unresumed 3.
    let shape = Shape { state_param: STATE as u32, prefix_size: PREFIX, prefix_align: 3, unresumed: Block(3) };
    let region = Region { start: 16, end: 16, tag_offset: 16, tag_ty: Ty::I8, size: 24, align: 3 };
    let split = split_in(func.view(), shape, Panics::NONE, region, &[], a)
        .expect("0-suspend cleanup using stack slot and entry value must split cleanly");
    if let Err(broke) = agree(func.view(), shape, &split, [5, 9]) {
        panic!("{broke}");
    }
}

/// After a suspend, a `Cleanup` landing block may use a post-suspend stack slot and value
/// directly (neither crosses the suspend), while a pre-suspend SSA value used in that same
/// post-suspend `Cleanup` block is still refused as `ValueAcrossSuspend`.
#[test]
fn a_post_suspend_cleanup_accepts_post_suspend_values_and_refuses_pre_suspend_values() {
    use crate::builder::{Param, Variadic};
    use crate::func::{Conv, RefKind};
    let heap = blaze_own::testing::heap();
    let a = heap.alloc();
    let build = |use_pre_suspend: bool| {
        let mut b = Builder::<_, Full>::new_in(64, a);
        own_sig(&mut b);
        let observe_sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[]);
        let observe = b.reference(RefKind::Func, 0);
        let may_unwind = b.reference(RefKind::Func, 1);
        let [entry, resume, normal, cancel, unresumed] = [(); 5].map(|_| b.create_block(BlockKind::Normal));
        let pad = b.create_block(BlockKind::Cleanup);
        let kept = b.slot(8, 3, SlotKind::Named);
        let post = b.slot(8, 3, SlotKind::Temp);
        b.begin(entry);
        let (state, cx) = (b.param(Ty::PTR), b.param(Ty::I64));
        let kept_at = b.slot_addr(kept);
        let upvar = b.load(Ty::I64, Flags::NONE, state, 0, MemAttrs::default());
        let pre_val = b.binary(Op::Add, Flags::NONE, upvar, cx);
        b.store(Flags::NONE, upvar, kept_at, 0, MemAttrs::default());
        let pending = b.iconst(Ty::I64, u128::from(PENDING));
        b.suspend(0, resume, cancel, &[pending, pending]);
        b.begin(resume);
        let post_at = b.slot_addr(post);
        let post_val = b.load(Ty::I64, Flags::NONE, kept_at, 0, MemAttrs::default());
        b.store(Flags::NONE, post_val, post_at, 0, MemAttrs::default());
        b.invoke(Flags::NONE, may_unwind, observe_sig, &[cx], normal, pad);
        b.begin(normal);
        let out = b.load(Ty::I64, Flags::NONE, post_at, 0, MemAttrs::default());
        let ready = b.iconst(Ty::I64, u128::from(READY));
        b.ret(&[ready, out]);
        b.begin(pad);
        let exception = b.param(Ty::PTR);
        let seen = b.load(Ty::I64, Flags::NONE, post_at, 0, MemAttrs::default());
        b.call(Flags::NONE, observe, observe_sig, &[seen]);
        if use_pre_suspend {
            b.call(Flags::NONE, observe, observe_sig, &[pre_val]);
        } else {
            b.call(Flags::NONE, observe, observe_sig, &[post_val]);
        }
        b.resume(exception);
        b.begin(cancel);
        b.cancelled();
        b.begin(unresumed);
        b.cancelled();
        let func = b.finish_checked().expect("the body is LIR");
        let shape = Shape { state_param: STATE as u32, prefix_size: PREFIX, prefix_align: 3, unresumed: Block(4) };
        (func, shape, kept, pre_val)
    };
    let (func, shape, kept, _) = build(false);
    let split = split_in(func.view(), shape, Panics::NONE, LAID, &named(kept), a)
        .expect("post-suspend cleanup using post-suspend slot and value must split");
    if let Err(broke) = agree(func.view(), shape, &split, [5, 9]) {
        panic!("{broke}");
    }
    let (func_bad, shape_bad, kept_bad, pre_val) = build(true);
    let err = split_in(func_bad.view(), shape_bad, Panics::NONE, LAID, &named(kept_bad), a).err();
    assert_eq!(err, Some(CoroutineError::ValueAcrossSuspend { value: pre_val.index() as u32 }));
}

/// Copying bytes between unnamed scratch slots via `Op::Memcpy` before a suspend does not
/// escape either slot's address: neither counts as reached after the suspend.
#[test]
fn a_memcpy_between_scratch_slots_before_suspend_is_not_escaped() {
    let heap = blaze_own::testing::heap();
    let a = heap.alloc();
    let mut b = Builder::<_, Full>::new_in(64, a);
    own_sig(&mut b);
    let [entry, resume, cancel, unresumed] = [(); 4].map(|_| b.create_block(BlockKind::Normal));
    let kept = b.slot(8, 3, SlotKind::Named);
    let src = b.slot(8, 3, SlotKind::Temp);
    let dst = b.slot(8, 3, SlotKind::Temp);
    b.begin(entry);
    let (state, cx) = (b.param(Ty::PTR), b.param(Ty::I64));
    let sa = b.slot_addr(src);
    let da = b.slot_addr(dst);
    let ka = b.slot_addr(kept);
    let upvar = b.load(Ty::I64, Flags::NONE, state, 0, MemAttrs::default());
    b.store(Flags::NONE, upvar, sa, 0, MemAttrs::default());
    let len = b.iconst(Ty::I64, 8);
    b.bulk(Op::Memcpy, Flags::NONE, da, sa, len, MemAttrs::aligned(3));
    let copied = b.load(Ty::I64, Flags::NONE, da, 0, MemAttrs::default());
    let sum = b.binary(Op::Add, Flags::NONE, copied, cx);
    b.store(Flags::NONE, sum, ka, 0, MemAttrs::default());
    let pending = b.iconst(Ty::I64, u128::from(PENDING));
    b.suspend(0, resume, cancel, &[pending, pending]);
    b.begin(resume);
    let out = b.load(Ty::I64, Flags::NONE, ka, 0, MemAttrs::default());
    let ready = b.iconst(Ty::I64, u128::from(READY));
    b.ret(&[ready, out]);
    b.begin(cancel);
    b.cancelled();
    b.begin(unresumed);
    b.cancelled();
    let func = b.finish_checked().expect("the body is LIR");
    let split = split_in(func.view(), SHAPE, Panics::NONE, LAID, &named(kept), a)
        .expect("memcpy between pre-suspend scratch slots must not escape them");
    assert_eq!(&split.layout.fields[..], &[StateField { what: Saved::Slot(kept), offset: 16, size: 8, align: 3 }]);
    crate::verify(split.poll.view()).expect("poll verifies");
    crate::verify(split.drop.view()).expect("drop verifies");
}

/// An unescaped unnamed slot inside a loop that is fully overwritten on every path from
/// `resume`/`cancel` before any read is not live across the suspend (a `Copy` loop local used
/// only before `.await`). A partial overwrite of a wider slot followed by a read of the
/// untouched bytes is still refused as `UnnamedAcrossSuspend`.
#[test]
fn a_loop_slot_overwritten_after_resume_is_not_reached_unless_partial() {
    let heap = blaze_own::testing::heap();
    let a = heap.alloc();
    let build = |partial: bool| {
        let mut b = Builder::<_, Full>::new_in(64, a);
        own_sig(&mut b);
        let [entry, header, body_blk, resume, done, cancel, unresumed] =
            [(); 7].map(|_| b.create_block(BlockKind::Normal));
        let kept = b.slot(8, 3, SlotKind::Named);
        let scratch = b.slot(if partial { 16 } else { 8 }, 3, SlotKind::Temp);
        b.begin(entry);
        let (state, _cx) = (b.param(Ty::PTR), b.param(Ty::I64));
        let ka = b.slot_addr(kept);
        let upvar = b.load(Ty::I64, Flags::NONE, state, 0, MemAttrs::default());
        b.store(Flags::NONE, upvar, ka, 0, MemAttrs::default());
        if partial {
            // Write the untouched upper half once before the loop.
            let sa_init = b.slot_addr(scratch);
            b.store(Flags::NONE, upvar, sa_init, 8, MemAttrs::default());
        }
        b.jump((header, &[]));

        b.begin(header);
        let rem = b.load(Ty::I64, Flags::NONE, ka, 0, MemAttrs::default());
        let zero = b.iconst(Ty::I64, 0);
        let more = b.icmp(IntCond::Ne, rem, zero);
        b.br(Flags::NONE, more, (body_blk, &[]), (done, &[]));

        b.begin(body_blk);
        // Full write when !partial (8 bytes into 8-byte slot); partial write when partial (8 of 16).
        let sa = b.slot_addr(scratch);
        let one = b.iconst(Ty::I64, 1);
        let next = b.binary(Op::Sub, Flags::NONE, rem, one);
        b.store(Flags::NONE, next, sa, 0, MemAttrs::default());
        let read_off = if partial { 8 } else { 0 };
        let val = b.load(Ty::I64, Flags::NONE, sa, read_off, MemAttrs::default());
        b.store(Flags::NONE, val, ka, 0, MemAttrs::default());
        let pending = b.iconst(Ty::I64, u128::from(PENDING));
        b.suspend(0, resume, cancel, &[pending, pending]);

        b.begin(resume);
        b.jump((header, &[]));

        b.begin(done);
        let out = b.load(Ty::I64, Flags::NONE, ka, 0, MemAttrs::default());
        let ready = b.iconst(Ty::I64, u128::from(READY));
        b.ret(&[ready, out]);

        b.begin(cancel);
        b.cancelled();

        b.begin(unresumed);
        b.cancelled();

        let func = b.finish_checked().expect("the body is LIR");
        let shape = Shape { state_param: STATE as u32, prefix_size: PREFIX, prefix_align: 3, unresumed: Block(6) };
        (func, shape, kept, scratch)
    };
    let (func, shape, kept, _) = build(false);
    let split = split_in(func.view(), shape, Panics::NONE, LAID, &named(kept), a)
        .expect("fully overwritten loop scratch slot must not be marked live across suspend");
    assert_eq!(&split.layout.fields[..], &[StateField { what: Saved::Slot(kept), offset: 16, size: 8, align: 3 }]);
    if let Err(broke) = agree(func.view(), shape, &split, [2, 0]) {
        panic!("{broke}");
    }
    let (func_bad, shape_bad, kept_bad, scratch_bad) = build(true);
    let err = split_in(func_bad.view(), shape_bad, Panics::NONE, LAID, &named(kept_bad), a).err();
    assert_eq!(err, Some(CoroutineError::UnnamedAcrossSuspend { slot: scratch_bad.0 }));
}

/// Pure, non-trapping SSA DAGs rooted at constants, `state_param`, and kept `slot_addr`s
/// (`iconst_wide`, `ptr_add(state, off)`, `ptr_add(slot_addr(kept), off)`, arithmetic over
/// constants) are rematerialized across a suspend in `split_in` with zero extra state bytes,
/// agreeing with direct execution on every resume and cancel schedule.
#[test]
fn pure_rematerializable_dags_cross_suspend_without_state_fields() {
    let heap = blaze_own::testing::heap();
    let a = heap.alloc();
    let mut b = Builder::<_, Full>::new_in(64, a);
    own_sig(&mut b);
    let [entry, resume, cancel, unresumed] = [(); 4].map(|_| b.create_block(BlockKind::Normal));
    let kept = b.slot(8, 3, SlotKind::Named);
    b.begin(entry);
    let (state, _cx) = (b.param(Ty::PTR), b.param(Ty::I64));
    let off8 = b.iconst(Ty::I64, 8);
    let upvar_ptr = b.ptr_add(Flags::NONE, state, off8);
    let kept_base = b.slot_addr(kept);
    let zero = b.iconst(Ty::I64, 0);
    let kept_ptr = b.ptr_add(Flags::NONE, kept_base, zero);
    let wide = b.iconst(Ty::I64, 0x1_0000_0003u128);
    let derived = b.binary(Op::Add, Flags::NONE, wide, off8);
    let upvar0 = b.load(Ty::I64, Flags::NONE, state, 0, MemAttrs::default());
    b.store(Flags::NONE, upvar0, kept_ptr, 0, MemAttrs::default());
    let pending = b.iconst(Ty::I64, u128::from(PENDING));
    b.suspend(0, resume, cancel, &[pending, pending]);

    b.begin(resume);
    let upvar1 = b.load(Ty::I64, Flags::NONE, upvar_ptr, 0, MemAttrs::default());
    let saved = b.load(Ty::I64, Flags::NONE, kept_ptr, 0, MemAttrs::default());
    let s1 = b.binary(Op::Add, Flags::NONE, saved, upvar1);
    let out = b.binary(Op::Add, Flags::NONE, s1, derived);
    let ready = b.iconst(Ty::I64, u128::from(READY));
    b.ret(&[ready, out]);

    b.begin(cancel);
    // Cancel path also uses the rematerialized `ptr_add(slot_addr(kept), 0)`!
    let _ = b.load(Ty::I64, Flags::NONE, kept_ptr, 0, MemAttrs::default());
    b.cancelled();

    b.begin(unresumed);
    b.cancelled();

    let func = b.finish_checked().expect("the body is LIR");
    let split = split_in(func.view(), SHAPE, Panics::NONE, LAID, &named(kept), a)
        .expect("pure DAGs over constants, state_param, and kept slot_addr must rematerialize");
    assert_eq!(&split.layout.fields[..], &[StateField { what: Saved::Slot(kept), offset: 16, size: 8, align: 3 }]);
    if let Err(broke) = agree(func.view(), SHAPE, &split, [5, 9]) {
        panic!("{broke}");
    }
}
