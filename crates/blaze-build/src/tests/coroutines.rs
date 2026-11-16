//! Coroutines, the layout fact (coroutine.rs; docs/v2/plan/design/coroutines.md, "The
//! builder's two emit points", item 1): a coroutine's identity in a ground type, and its state
//! laid out from the seam's two answers. The emit points' tests (an async fn polled to its
//! value, an await pending twice, drop while suspended, nested) come with the emit points.

use core::cell::Cell;
use std::alloc::Global;
use std::format;
use std::println;

use blaze_lir::Ty;
use blaze_own::Alloc;
use blaze_typeck::{Adt, ExprId, ItemKey, PNode, PTy, PTys, PortableTys, UintTy};

use crate::layout::{LayoutCx, LayoutFacts, Layouts, Missing, ReprAttrs, ValidRange};
use crate::{CoroutineFacts, CoroutineOf, StateShape, coroutine_identity, coroutine_of};

/// The owner fn of the one coroutine these tests name.
const OWNER: ItemKey = ItemKey(0xc0);

/// A world with one coroutine, ordinal 0 of [`OWNER`]: its upvars `(u64, u8)`, its output
/// `u64`, and a state the test sets (`None`: its body is not built and split yet).
struct World {
    types: PortableTys<'static>,
    env: PTy,
    output: PTy,
    state: Cell<Option<StateShape>>,
}

impl World {
    fn new() -> World {
        let mut types = PortableTys::new_in(Alloc::global());
        let wide = types.intern(PNode::Uint(UintTy::U64)).unwrap();
        let byte = types.intern(PNode::Uint(UintTy::U8)).unwrap();
        let fields = types.intern_list(&[wide, byte]).unwrap();
        let env = types.intern(PNode::Tuple(fields)).unwrap();
        World { types, env, output: wide, state: Cell::new(None) }
    }
}

impl LayoutFacts for World {
    fn adt(&self, _: ItemKey) -> Option<Adt<'_>> {
        None
    }

    fn repr(&self, _: ItemKey) -> ReprAttrs {
        ReprAttrs::RUST
    }

    fn discriminant(&self, _: ItemKey, _: u32) -> Result<Option<i128>, Missing> {
        Ok(None)
    }

    fn valid_range(&self, _: ItemKey) -> ValidRange {
        ValidRange::default()
    }

    fn coroutine(&self, owner: ItemKey, ordinal: u32) -> Option<CoroutineFacts<'_>> {
        (owner == OWNER && ordinal == 0).then(|| CoroutineFacts { types: self.types.view(), env: self.env, output: self.output })
    }

    fn coroutine_state(&self, owner: ItemKey, ordinal: u32, args: &[PTy], _: PTys<'_>) -> Result<Option<StateShape>, Missing> {
        if owner != OWNER || ordinal != 0 || !args.is_empty() {
            return Ok(None);
        }
        self.state.get().map(Some).ok_or(Missing { item: owner })
    }
}

/// The ground type of the world's coroutine in `ground`.
fn coroutine_ty(ground: &mut PortableTys<'_>) -> PTy {
    let none = ground.intern_list(&[]).unwrap();
    let output = ground.intern(PNode::Uint(UintTy::U64)).unwrap();
    coroutine_identity(ground, OWNER, 0, none, output).unwrap()
}

/// A coroutine's state is its upvars, then what the pass keeps, then the tag
/// (coroutines.md, "The LIR pass": "The state: upvars, then kept slots and values by falling
/// alignment, then the tag"), so its size and alignment are the PASS's for the built body, not
/// the upvars' tuple's: `(u64, u8)` alone is 16 bytes; with one kept `u64`, a kept pointer and
/// an `i8` tag at 32 the state is 40 bytes, aligned to 8. Before the body is built and split
/// the layout is refused as a missing record and nothing is cached: the same question answers
/// once the state exists.
///
/// The control: a state smaller than its upvars (a seam that answered another coroutine's
/// state) is refused by name, never laid out.
///
/// Prints: `coroutine state: missing first, then size 40 align 8; a 8-byte state under 16 bytes of upvars refused`
#[test]
fn a_coroutine_is_laid_out_as_its_upvars_then_the_pass_state() {
    let world = World::new();
    let mut ground = PortableTys::new_in(Alloc::global());
    let t = coroutine_ty(&mut ground);
    let mut ls = Layouts::new_in(Global);

    let early = ls.of(t, &mut LayoutCx { ground: &mut ground, facts: &world, scratch: Alloc::global() });
    let early = match early {
        Ok(_) => panic!("a coroutine was laid out before its body was built and split"),
        Err(error) => format!("{error}"),
    };
    assert!(early.contains("is not made yet"), "the refusal does not name a missing record: {early}");

    world.state.set(Some(StateShape { size: 40, align: 3, tag_offset: 32, tag_ty: Ty::I8 }));
    let id = ls.of(t, &mut LayoutCx { ground: &mut ground, facts: &world, scratch: Alloc::global() }).unwrap_or_else(|error| panic!("the refusal was cached: {error}"));
    let row = ls.get(id);
    assert_eq!((row.size, row.align), (40, 3));
    assert!(row.scalar.is_none() && row.pair.is_none(), "a coroutine's state has no scalar form");

    // The control, in tables of its own (a layout made is cached by type).
    let small = World::new();
    small.state.set(Some(StateShape { size: 8, align: 3, tag_offset: 4, tag_ty: Ty::I8 }));
    let mut ground = PortableTys::new_in(Alloc::global());
    let t = coroutine_ty(&mut ground);
    let mut ls = Layouts::new_in(Global);
    let refused = match ls.of(t, &mut LayoutCx { ground: &mut ground, facts: &small, scratch: Alloc::global() }) {
        Ok(_) => panic!("a state smaller than its upvars was laid out"),
        Err(error) => format!("{error}"),
    };
    assert!(refused.contains("smaller than its upvars"), "the refusal does not name the rule: {refused}");
    println!("coroutine state: missing first, then size 40 align 8; a 8-byte state under 16 bytes of upvars refused");
}

/// A coroutine is named by its owner, its ordinal among the owner body's async blocks and the
/// owner's arguments, as a closure is (typeck.md, "A closure's identity": no handle local to
/// one body). The Reference, "Async blocks": "An async block is a variant of a block
/// expression which evaluates to a future": one type per block. So the identity form is read
/// back whole, is copied between tables by `ground` unchanged, and two blocks of one body are
/// two types.
///
/// The control: the checker's own form, a future named by its EXPRESSION (its third part the
/// interior tuple), has no identity: `coroutine_of` reads none and `ground` refuses it by name.
///
/// Prints: `coroutine identity: owner 0xc0 ordinal 0 read back; ordinals 0 and 1 differ; a future named by its expression refused`
#[test]
fn a_coroutine_is_named_by_owner_ordinal_and_arguments() {
    let world = World::new();
    let mut from = PortableTys::new_in(Alloc::global());
    let t = coroutine_ty(&mut from);
    let none = from.intern_list(&[]).unwrap();
    let output = from.intern(PNode::Uint(UintTy::U64)).unwrap();
    assert_eq!(coroutine_of(from.view(), t), Some(CoroutineOf { owner: OWNER, owner_args: none, ordinal: 0, output }));
    let second = coroutine_identity(&mut from, OWNER, 1, none, output).unwrap();
    assert_ne!(t, second, "two async blocks of one body share a type");

    let mut out = PortableTys::new_in(Alloc::global());
    let copied = crate::ground(from.view(), t, &[], &mut out, Alloc::global(), &world).unwrap_or_else(|error| panic!("the identity form was refused: {error}"));
    let read = coroutine_of(out.view(), copied).expect("the copy is not a coroutine");
    assert_eq!((read.owner, read.ordinal), (OWNER, 0));

    // The control: `Future(expression 7, u64, ())`, as the checker writes it.
    let interior = from.intern(PNode::Tuple(none)).unwrap();
    let raw = from.intern(PNode::Future(ExprId(7), output, interior)).unwrap();
    assert_eq!(coroutine_of(from.view(), raw), None);
    let refused = match crate::ground(from.view(), raw, &[], &mut out, Alloc::global(), &world) {
        Ok(_) => panic!("a future named by its expression entered a ground table"),
        Err(error) => format!("{error}"),
    };
    assert!(refused.contains("named by its expression"), "the refusal does not name the form: {refused}");
    println!("coroutine identity: owner {:#x} ordinal 0 read back; ordinals 0 and 1 differ; a future named by its expression refused", OWNER.0);
}
