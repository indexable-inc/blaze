//! A promoted borrow is a constant row, not a frame temporary (walk/promote.rs; blaze-typeck
//! rules/promote.rs lists it).
//!
//! The Reference, "Destructors", "Constant promotion": "Promotion of a value expression to a
//! `'static` slot occurs when the expression could be written in a constant and borrowed, and
//! that borrow could be dereferenced where the expression was originally written, without
//! changing the runtime behavior. That is, the promoted expression can be evaluated at
//! compile-time and the resulting value does not contain interior mutability or destructors".

use super::*;

/// `fn main() -> u32 { *&7 }`: the checker lists `&7` (a literal of `u32`, `Freeze` with no
/// drop glue); the evaluator's record of it is the four bytes `07 00 00 00`. The built function
/// takes the borrow as the address of a data row holding those bytes (`data_addr`), which
/// outlives the frame, and reads 7 through it. Before walk/promote.rs the borrow was the address
/// of a stack temporary the literal was stored into: no data row of those bytes, so the first
/// assertion fails with "a promoted borrow must not point into the frame". With no record for
/// the borrow the build is refused by name, never built as a temporary. (The prompt's name: the
/// same path carries `&RawWakerVTable::new(..)`'s four words; its fn relocations are read back
/// by `const_fn_instance`, as a constant path's are: the next test.)
#[test]
fn a_promoted_vtable_outlives_its_fn() {
    let mut w = TestWorld::new();
    w.func(1, "main", &[], U32);
    let mut b = BodyBuf::new();
    let seven = b.int(7, None);
    let r = b.e(Expr::Ref { mutbl: Mutbl::Not, operand: seven });
    let root = b.e(Expr::Unary { op: blaze_typeck::UnOp::Deref, operand: r });
    w.promoted.push((r, Some(record(&[7, 0, 0, 0], 2))));
    let built = build(&w, &b, root).unwrap_or_else(|e| panic!("{e}"));
    let text = blaze_lir::print(built.func.view());
    assert!(text.contains("bytes \"07000000\"") && text.contains("data_addr"), "a promoted borrow must not point into the frame:\n{text}");
    assert_eq!(one(&built, &[]), 7);

    w.promoted[0].1 = None;
    let refused = build(&w, &b, root).err().unwrap_or_default();
    assert!(refused.contains("has no promoted value"), "{refused}");
}

/// `const F: fn() -> u32 = seven; fn main() -> fn() -> u32 { *&F }`: the checker promotes `&F`
/// (a constant path), and the evaluator's record of the operand is one word relocated to
/// `seven`'s instance key. The instance is the one `Adts::const_fn_instance` gives (the one
/// answer for every promoted, constant and const block fn word): `seven` is the
/// function's one reference (`Built::refs`, which the run's reach follows). With no answer the
/// build is refused by name (`NoConstFnInstance`). Before `data_rows` asked
/// `const_fn_instance`, it refused every such borrow ("a constant holding a fn pointer to an
/// instance its own expression does not name") and the first assertion failed.
///
/// Prints: `test tests::promote::a_promoted_constants_fn_pointer_is_a_reference_of_the_function ... ok`
#[test]
fn a_promoted_constants_fn_pointer_is_a_reference_of_the_function() {
    use crate::data::{DataReloc, DataTarget};
    let mut w = TestWorld::new();
    let u32_ = w.ty(U32);
    let io = w.types.intern_list(&[u32_]).expect("a list interns");
    let fn_ptr = PNode::FnPtr(io, blaze_typeck::FnPtrHeader::RUST);
    w.func(1, "main", &[], fn_ptr);
    w.func(40, "seven", &[], U32);
    w.func(41, "F", &[], fn_ptr);
    let key = crate::InstanceKey(blaze_ids::ContentId::of("test fn", b"seven"));
    let word = || {
        let mut bytes = Vec::new_in(Global);
        bytes.extend_from_slice(&[0u8; 8]);
        let mut relocs = Vec::new_in(Global);
        relocs.push(DataReloc { offset: 0, target: DataTarget::Fn(key) });
        let mut allocs = Vec::new_in(Global);
        allocs.push(DataAlloc { bytes, align: 3, mutable: false, relocs });
        DataRecord { allocs }
    };
    w.consts.push((ItemKey(41), Some(word())));
    let mut b = BodyBuf::new();
    let f = b.path("F");
    let f = b.e(Expr::Path(f));
    let r = b.e(Expr::Ref { mutbl: Mutbl::Not, operand: f });
    let root = b.e(Expr::Unary { op: blaze_typeck::UnOp::Deref, operand: r });
    w.promoted.push((r, Some(word())));
    let seven = Instance { item: ItemKey(40), args: List::EMPTY };
    w.const_fns.push((key, seven));
    let built = build(&w, &b, root).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(built.refs.as_slice(), &[seven], "`seven` is the function's one reference");
    let text = blaze_lir::print(built.func.view());
    assert!(text.contains("data_addr"), "`&F` is the address of its data row:\n{text}");

    w.const_fns.clear();
    let refused = build(&w, &b, root).err().unwrap_or_default();
    assert!(refused.contains("holds a fn pointer to instance"), "{refused}");
}

/// `impl E { fn id(&self) -> &E }` called as `B.id()` (shared autoref on a unit variant):
/// the receiver `B` is promoted to a constant row (`data_addr`), never spilled to a stack
/// temporary. Also verifies `Expr::Ref` with a further shared autoref (`(&7).read()`) keeps
/// two levels of indirection instead of confusing the `Expr::Ref` promotion with an autoref
/// promotion.
#[test]
fn a_promoted_autoref_uses_constant_row_not_frame_temporary() {
    let mut w = TestWorld::new();
    let u32_t = w.ty(U32);
    let u32_ref = PNode::Ref(Mutbl::Not, u32_t);
    let u32_ref_t = w.ty(u32_ref);
    let u32_ref_ref = PNode::Ref(Mutbl::Not, u32_ref_t);
    w.func(1, "main", &[], U32);
    w.func(20, "read_ref", &[u32_ref], U32);
    w.func(21, "read_ref_ref", &[u32_ref_ref], U32);
    let imp_u32 = ItemKey(200);
    let imp_ref = ItemKey(201);
    let u32_ref_ty = w.ty(u32_ref);
    w.methods.push((
        blaze_typeck::Shape::Uint(blaze_typeck::UintTy::U32),
        blaze_typeck::Name::of("read_ref"),
        blaze_typeck::AssocItem { container: imp_u32, item: ItemKey(20), kind: blaze_typeck::AssocKind::Fn { self_param: true } },
        u32_t,
    ));
    w.methods.push((
        blaze_typeck::Shape::Ref(Mutbl::Not),
        blaze_typeck::Name::of("read_ref_ref"),
        blaze_typeck::AssocItem { container: imp_ref, item: ItemKey(21), kind: blaze_typeck::AssocKind::Fn { self_param: true } },
        u32_ref_ty,
    ));

    // Case 1: `7u32.read_ref()` -> `7u32` is autoref-promoted to a constant row.
    let mut b = BodyBuf::new();
    let seven = b.int(7, Some(blaze_typeck::IntSuffix::Uint(blaze_typeck::UintTy::U32)));
    let root = b.e(Expr::MethodCall {
        receiver: seven,
        name: blaze_typeck::Name::of("read_ref"),
        generic_args: List::EMPTY,
        args: List::EMPTY,
        scope: blaze_typeck::ScopeId(0),
    });
    w.promoted.push((seven, Some(record(&[7, 0, 0, 0], 2))));
    let built = build(&w, &b, root).unwrap_or_else(|e| panic!("{e}"));
    let text = blaze_lir::print(built.func.view());
    assert!(
        text.contains("bytes \"07000000\"") && text.contains("data_addr"),
        "a promoted shared autoref must emit a constant data row:\n{text}"
    );

    w.promoted[0].1 = None;
    let refused = build(&w, &b, root).err().unwrap_or_default();
    assert!(refused.contains("has no promoted value"), "{refused}");

    // Case 2: `(&7u32).read_ref_ref()` -> `&7u32` is an `Expr::Ref` (promoted) with a
    // further shared autoref adjustment (`&&u32`); it must pass a stack slot holding the
    // `data_addr` (two indirections), not the `data_addr` directly.
    let mut b2 = BodyBuf::new();
    let seven2 = b2.int(7, Some(blaze_typeck::IntSuffix::Uint(blaze_typeck::UintTy::U32)));
    let r2 = b2.e(Expr::Ref { mutbl: Mutbl::Not, operand: seven2 });
    let root2 = b2.e(Expr::MethodCall {
        receiver: r2,
        name: blaze_typeck::Name::of("read_ref_ref"),
        generic_args: List::EMPTY,
        args: List::EMPTY,
        scope: blaze_typeck::ScopeId(0),
    });
    w.promoted.clear();
    w.promoted.push((r2, Some(record(&[7, 0, 0, 0], 2))));
    let built2 = build(&w, &b2, root2).unwrap_or_else(|e| panic!("{e}"));
    let text2 = blaze_lir::print(built2.func.view());
    assert!(
        text2.contains("data_addr") && text2.contains("slot_addr"),
        "an autoref of a promoted `Expr::Ref` must keep the second indirection:\n{text2}"
    );
}
