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
    Block, BlockKind, Builder, Flags, Full, Func, Op, Ty,
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
