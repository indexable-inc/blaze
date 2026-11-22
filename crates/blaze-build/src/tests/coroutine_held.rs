//! An async block's future held in a struct, as a caller's record writes it: by IDENTITY,
//! `PNode::Coroutine { owner, ordinal, args }` (blaze-typeck rules/writeback.rs; coroutine_id.rs:
//! the block's ordinal among its owner's async blocks). Since coroutine-identity-1 that node is
//! the builder's one ground form of a coroutine (coroutine.rs, the module doc): `ground` maps
//! its arguments as a closure's, and its layout and drop facts are read from the world by the
//! identity (`LayoutFacts::coroutine`, `coroutine_state`).
//!
//! The Reference, "Async blocks": "An async block is a variant of a block expression which
//! evaluates to a future"; "Destructors": "When an initialized variable or temporary goes out of
//! scope, its destructor is run": `Held(fut)` dropped drops its field, and an unpolled future
//! drops what its state holds (core's `needs_drop` "may return `true` for types that don't
//! actually need to be dropped": a coroutine always answers `true`, coroutine.rs). The program
//! of walkAJ-P3-3's ladder test is this shape: `let _h = Held(ready(G(1)));`.

use std::alloc::Global;
use std::format;
use std::println;

use blaze_lir::Ty;
use blaze_own::Alloc;
use blaze_typeck::{Adt, AdtKind, CtorKind, ExprId, Field, ItemKey, List, Name, PNode, PTy, PTys, PortableTys, UintTy, Variant};

use crate::drop_facts::needs_drop;
use crate::layout::{LayoutCx, LayoutFacts, Layouts, Missing, ReprAttrs, ValidRange};
use crate::{CoroutineFacts, StateShape, coroutine_of};

/// The owner fn of the one coroutine (an `async fn ready<T>(t: T) -> T`'s root block).
const OWNER: ItemKey = ItemKey(0xc1);
/// `struct Held<F>(F);`, with no `Drop` impl.
const HELD: ItemKey = ItemKey(0x4e);

/// A world with `Held` and one coroutine, ordinal 0 of [`OWNER`] at one argument: its upvars
/// `(T,)` and output `T` (in the owner's parameter `Param(0)`), and the state the pass would lay
/// out at `T = u64`: 8 bytes of upvars, an `i8` tag at 8, 16 bytes aligned to 8.
/// `answers`: whether it answers the coroutine's facts at all (the control).
struct HeldWorld {
    types: PortableTys<'static>,
    env: PTy,
    output: PTy,
    state: StateShape,
    answers: bool,
    held_variants: [Variant; 1],
    held_fields: [Field; 1],
}

impl HeldWorld {
    fn new(answers: bool) -> HeldWorld {
        let mut types = PortableTys::new_in(Alloc::global());
        let param = types.intern(PNode::Param(0)).unwrap();
        let fields = types.intern_list(&[param]).unwrap();
        let env = types.intern(PNode::Tuple(fields)).unwrap();
        let layout = blaze_lir::coroutine::StateLayout { size: 16, align: 3, tag_offset: 8, tag_ty: Ty::I8, points: 1, fields: std::vec::Vec::new_in(Global) };
        let held_fields = [Field { name: Name(0), ty: param }];
        let held = Variant { key: ItemKey(HELD.0 * 64), name: Name(0), ctor: CtorKind::Tuple, fields: List { start: 0, len: 1 } };
        HeldWorld { types, env, output: param, state: StateShape::of(&layout), answers, held_variants: [held], held_fields }
    }
}

impl LayoutFacts for HeldWorld {
    fn adt(&self, key: ItemKey) -> Option<Adt<'_>> {
        (key == HELD).then(|| Adt { kind: AdtKind::Struct, generics: 1, variants: &self.held_variants, fields: &self.held_fields, types: self.types.view() })
    }

    fn repr(&self, _: ItemKey) -> Result<ReprAttrs, Missing> {
        Ok(ReprAttrs::RUST)
    }

    fn discriminant(&self, _: ItemKey, _: u32) -> Result<Option<i128>, Missing> {
        Ok(None)
    }

    fn valid_range(&self, _: ItemKey) -> Result<ValidRange, Missing> {
        Ok(ValidRange::default())
    }

    fn coroutine(&self, owner: ItemKey, ordinal: u32) -> Option<CoroutineFacts<'_>> {
        (self.answers && owner == OWNER && ordinal == 0).then(|| CoroutineFacts { types: self.types.view(), env: self.env, output: self.output })
    }

    fn coroutine_state(&self, owner: ItemKey, ordinal: u32, args: &[PTy], ground: PTys<'_>) -> Result<Option<StateShape>, Missing> {
        let at_u64 = matches!(args, [t] if matches!(ground.node(*t), Ok(PNode::Uint(UintTy::U64))));
        Ok((self.answers && owner == OWNER && ordinal == 0 && at_u64).then_some(self.state))
    }
}

/// `Held<{ready::<u64>'s future}>` as the caller's record writes it, in `from`: the struct over
/// the coroutine by identity at `[u64]`.
fn held_record(from: &mut PortableTys<'_>) -> PTy {
    let wide = from.intern(PNode::Uint(UintTy::U64)).unwrap();
    let args = from.intern_list(&[wide]).unwrap();
    let fut = from.intern(PNode::Coroutine { owner: OWNER, ordinal: 0, args }).unwrap();
    let held_args = from.intern_list(&[fut]).unwrap();
    from.intern(PNode::Adt(HELD, held_args)).unwrap()
}

/// `Held<F>` over an async future grounds, lays out and needs drop by the coroutine's identity:
/// the ground type keeps `PNode::Coroutine { owner, 0, [u64] }` (read back by `coroutine_of`),
/// `Held`'s layout is the state's (16 bytes, align 8: one field at offset 0), and `needs_drop`
/// is `true` with `Held`'s `Drop` impl asked once, by its own type (no impl: the field's glue
/// decides).
///
/// Controls: a world that does not answer the coroutine's facts refuses the layout by name
/// ("did not answer"), never a guessed size; the checker's in-body form, `Future(expression,
/// output, interior)`, is refused by `ground` by name ("named by its expression"): it never
/// crosses a body. WITHOUT THE FIX (the tree before coroutine-identity-1) the first step fails:
/// "the identity form was refused: .. an async block's future by identity (`PNode::Coroutine`):
/// the builder grounds the `Future(ordinal, output, FnDef)` form (coroutine.rs) until it reads
/// `World::coroutine`".
///
/// Prints: `held coroutine: Held<coroutine#0 of 0xc1<u64>> 16 bytes align 3, needs drop, Drop asked 1 time; unanswered facts refused; a future named by its expression refused`
#[test]
fn an_async_future_held_in_a_struct_lays_out_and_drops_by_its_identity() {
    let world = HeldWorld::new(true);
    let mut from = PortableTys::new_in(Alloc::global());
    let held = held_record(&mut from);
    let mut ground = PortableTys::new_in(Alloc::global());
    let g = crate::ground(from.view(), held, &[], &mut ground, Alloc::global(), &world).unwrap_or_else(|error| panic!("the identity form was refused: {error}"));

    // The ground type: `Held<Coroutine { OWNER, 0, [u64] }>`.
    let Ok(PNode::Adt(HELD, l)) = ground.view().node(g) else { panic!("`Held<..>` did not ground to `Held`") };
    let fut = ground.view().list(l).unwrap()[0];
    let of = coroutine_of(ground.view(), fut).expect("the field is not a coroutine by identity");
    assert_eq!((of.owner, of.ordinal), (OWNER, 0));
    let owner_args = ground.view().list(of.owner_args).unwrap();
    assert!(matches!(owner_args, [t] if matches!(ground.view().node(*t), Ok(PNode::Uint(UintTy::U64)))), "the owner's arguments are not `[u64]`");

    // Layout: the struct of one field, the 16-byte state.
    let mut ls = Layouts::new_in(Global);
    let id = ls.of(g, &mut LayoutCx { ground: &mut ground, facts: &world, scratch: Alloc::global() }).unwrap_or_else(|error| panic!("`Held<future>` was refused: {error}"));
    let row = *ls.get(id);
    assert_eq!((row.size, row.align), (16, 3), "`Held<future>` is not the state's size and alignment");

    // Drop: the coroutine needs drop; `Held`'s own `Drop` impl is asked by its type, once.
    let mut asked = 0u32;
    let mut drop_impl = |t: PTy, tys: &mut PortableTys<'_>| -> Result<Option<ItemKey>, core::convert::Infallible> {
        assert!(matches!(tys.view().node(t), Ok(PNode::Adt(HELD, _))), "a `Drop` impl asked of other than `Held`");
        asked += 1;
        Ok(None)
    };
    let needs = needs_drop(g, &mut ground, &world, None, &mut drop_impl, Alloc::global()).unwrap_or_else(|error| panic!("needs_drop refused `Held<future>`: {error}"));
    assert!(needs, "`Held<future>` must need drop: its future's state is dropped by the coroutine's `drop`");
    assert_eq!(asked, 1, "`Held`'s `Drop` impl is asked once, by its own type");

    // Control 1: no facts for the identity, no layout: refused by name.
    let silent = HeldWorld::new(false);
    let mut ground = PortableTys::new_in(Alloc::global());
    let g = crate::ground(from.view(), held, &[], &mut ground, Alloc::global(), &silent).unwrap_or_else(|error| panic!("grounding needs no facts: {error}"));
    let mut ls = Layouts::new_in(Global);
    let refused = match ls.of(g, &mut LayoutCx { ground: &mut ground, facts: &silent, scratch: Alloc::global() }) {
        Ok(_) => panic!("a coroutine the world does not answer was laid out"),
        Err(error) => format!("{error}"),
    };
    assert!(refused.contains("did not answer"), "the refusal does not name the missing answer: {refused}");

    // Control 2: the checker's in-body form never enters a ground table.
    let wide = from.intern(PNode::Uint(UintTy::U64)).unwrap();
    let none = from.intern_list(&[]).unwrap();
    let interior = from.intern(PNode::Tuple(none)).unwrap();
    let raw = from.intern(PNode::Future(ExprId(7), wide, interior)).unwrap();
    let raw_args = from.intern_list(&[raw]).unwrap();
    let raw_held = from.intern(PNode::Adt(HELD, raw_args)).unwrap();
    let mut ground = PortableTys::new_in(Alloc::global());
    let refused = match crate::ground(from.view(), raw_held, &[], &mut ground, Alloc::global(), &world) {
        Ok(_) => panic!("a future named by its expression entered a ground table"),
        Err(error) => format!("{error}"),
    };
    assert!(refused.contains("named by its expression"), "the refusal does not name the form: {refused}");
    println!(
        "held coroutine: Held<coroutine#0 of {:#x}<u64>> {} bytes align {}, needs drop, Drop asked {asked} time; unanswered facts refused; a future named by its expression refused",
        OWNER.0, row.size, row.align
    );
}
