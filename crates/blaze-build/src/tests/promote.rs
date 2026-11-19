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
/// same path carries `&RawWakerVTable::new(..)`'s four words; its fn relocations need the
/// front's `promoted_fn_key`, which the test world does not answer.)
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
/// `seven`'s instance key. The operand names no fn item by path, so the walk's own candidates
/// are none and the instance is the one `Adts::const_fn_instance` gives: `seven` is the
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

/// A record of `allocs`: each its bytes, log2 alignment and relocations (offset, target).
fn rows(allocs: &[(&[u8], u8, &[(u32, crate::data::DataTarget)])]) -> DataRecord<Global> {
    let mut out = Vec::new_in(Global);
    for &(bytes, align, relocs) in allocs {
        let mut b = Vec::new_in(Global);
        b.extend_from_slice(bytes);
        let mut r = Vec::new_in(Global);
        for &(offset, target) in relocs {
            r.push(crate::data::DataReloc { offset, target });
        }
        out.push(DataAlloc { bytes: b, align, mutable: false, relocs: r });
    }
    DataRecord { allocs: out }
}

/// [`build`], with borrow `unlisted` taken out of `Checked::promoted` after the check: a walk
/// that meets a borrow the checker did not promote inside a promoted operand.
fn build_unlisting(world: &TestWorld, body: &BodyBuf, root: ExprId, unlisted: ExprId) -> Result<Built<Global>, String> {
    let view = body.view(MAIN, root);
    let mut ty = InferCtx::new_in(Alloc::global());
    let mut checked = match blaze_typeck::check(&view, world, &mut ty, &Needs::new_in(Alloc::global()), Alloc::global()) {
        Ok(Outcome::Done(c)) => c,
        other => return Err(format!("the check did not finish: {}", other.is_ok())),
    };
    checked.promoted.retain(|&e| e != unlisted);
    let sig = world.sig(MAIN).ok_or_else(|| String::from("no signature for main"))?;
    let mut ground = PortableTys::new_in(Alloc::global());
    let mut subst = Subst { args: &[], ground: &mut ground, scratch: Alloc::global() };
    crate::build(&view, &checked, &sig, &mut subst, &TestAdts { world, body }, Global).map_err(|error| format!("the build failed: {error}"))
}

/// The lines of the printed function that make a data row.
fn data_rows(text: &str) -> usize {
    text.lines().filter(|l| l.contains("bytes \"")).count()
}

/// `fn main() -> u32 { **&&7 }`: the checker promotes both borrows (blaze-typeck
/// rules/promote.rs: `&7`'s type `&u32` is `Freeze` with no drop glue), and the evaluator's
/// record of the outer operand `&7` is a pointer word relocated to its second allocation, the
/// four bytes of 7 (blaze-consteval: a reference's target travels with it). The built function
/// takes the outer borrow as the address of a data row holding that word, relocated to a second
/// row holding 7, and reads 7 through both: two rows, no frame temporary. Before the arm in
/// walk/promote.rs `promoted_fns` the build was refused ("a promoted operand of a form the
/// checker does not promote"), which is what the Mac run of `fn f() -> &'static &'static i32 {
/// &&7 }` printed. An inner borrow the checker did not list is refused by name, never built as
/// a temporary: without the arm's `is_promoted` guard the second half builds.
#[test]
fn a_promoted_borrow_of_a_promoted_borrow_is_built() {
    use crate::data::DataTarget::Local;
    let mut w = TestWorld::new();
    w.func(1, "main", &[], U32);
    let mut b = BodyBuf::new();
    let seven = b.int(7, None);
    let inner = b.e(Expr::Ref { mutbl: Mutbl::Not, operand: seven });
    let outer = b.e(Expr::Ref { mutbl: Mutbl::Not, operand: inner });
    let once = b.e(Expr::Unary { op: blaze_typeck::UnOp::Deref, operand: outer });
    let root = b.e(Expr::Unary { op: blaze_typeck::UnOp::Deref, operand: once });
    w.promoted.push((inner, Some(record(&[7, 0, 0, 0], 2))));
    w.promoted.push((outer, Some(rows(&[(&[0; 8], 3, &[(0, Local(1))]), (&[7, 0, 0, 0], 2, &[])]))));
    let built = build(&w, &b, root).unwrap_or_else(|e| panic!("{e}"));
    let text = blaze_lir::print(built.func.view());
    assert_eq!(data_rows(&text), 2, "the outer borrow's word and the 7 it points at, both rows:\n{text}");
    assert!(text.contains("bytes \"07000000\"") && text.contains("data_addr"), "{text}");
    assert_eq!(one(&built, &[]), 7);

    let refused = build_unlisting(&w, &b, root, inner).err().unwrap_or_default();
    assert!(refused.contains("a borrow inside a promoted operand that the checker did not promote"), "{refused}");
}

/// `fn main() -> u32 { (*(*&[(&[1u32], &[2u32, 3])])[0].1)[1] }`, the shape of ignore's
/// `DEFAULT_TYPES: &[(&[&str], &[&str])]` (arrays here, so no unsizing): the checker promotes
/// the outer borrow and both inner ones; the outer record is one 16-byte row of two words
/// relocated to its allocations 1 (`[1]`) and 2 (`[2, 3]`). The built function makes the three
/// rows and reads 3 through the second word. Before the arm in `promoted_fns` it was refused.
#[test]
fn a_promoted_table_of_promoted_borrows_is_built() {
    use crate::data::DataTarget::Local;
    let mut w = TestWorld::new();
    w.func(1, "main", &[], U32);
    w.bounds_fn();
    let mut b = BodyBuf::new();
    let one_ = b.int(1, SU32);
    let first = b.array(&[one_]);
    let r1 = b.e(Expr::Ref { mutbl: Mutbl::Not, operand: first });
    let two = b.int(2, SU32);
    let three = b.int(3, SU32);
    let second = b.array(&[two, three]);
    let r2 = b.e(Expr::Ref { mutbl: Mutbl::Not, operand: second });
    let entry = b.tuple(&[r1, r2]);
    let table = b.array(&[entry]);
    let outer = b.e(Expr::Ref { mutbl: Mutbl::Not, operand: table });
    let all = b.e(Expr::Unary { op: blaze_typeck::UnOp::Deref, operand: outer });
    let zero = b.int(0, None);
    let row = b.index(all, zero);
    let field = b.tfield(row, 1);
    let pair = b.e(Expr::Unary { op: blaze_typeck::UnOp::Deref, operand: field });
    let k = b.int(1, None);
    let root = b.index(pair, k);
    w.promoted.push((r1, Some(record(&[1, 0, 0, 0], 2))));
    w.promoted.push((r2, Some(record(&[2, 0, 0, 0, 3, 0, 0, 0], 2))));
    w.promoted.push((
        outer,
        Some(rows(&[(&[0; 16], 3, &[(0, Local(1)), (8, Local(2))]), (&[1, 0, 0, 0], 2, &[]), (&[2, 0, 0, 0, 3, 0, 0, 0], 2, &[])])),
    ));
    let built = build(&w, &b, root).unwrap_or_else(|e| panic!("{e}"));
    let text = blaze_lir::print(built.func.view());
    assert_eq!(data_rows(&text), 3, "the table's row and its two arrays:\n{text}");
    assert_eq!(one(&built, &[]), 3);
}

/// `fn main() -> &'static &'static [fn() -> u32; 1] { &&[seven] }`: `seven` is coerced to a fn
/// pointer inside the inner borrow, so the outer record's second allocation is a word relocated
/// to `seven`'s instance key. The walk goes on into the inner borrow's operand, finds `seven`
/// by path and keys it (`Adts::promoted_fn_key`), so `seven` is the function's one reference.
/// A walk that stopped at the inner borrow would not know the key and would refuse the build
/// (`NoConstFnInstance`, "holds a fn pointer to instance").
#[test]
fn a_fn_named_inside_a_promoted_inner_borrow_is_a_reference() {
    use crate::data::DataTarget::{Fn, Local};
    let mut w = TestWorld::new();
    let u32_ = w.ty(U32);
    let io = w.types.intern_list(&[u32_]).expect("a list interns");
    let fn_ptr = PNode::FnPtr(io, blaze_typeck::FnPtrHeader::RUST);
    let table = w.array(fn_ptr, 1);
    let inner_ty = w.ref_(Mutbl::Not, table);
    let outer_ty = w.ref_(Mutbl::Not, inner_ty);
    w.func(1, "main", &[], outer_ty);
    w.func(40, "seven", &[], U32);
    let key = crate::InstanceKey(blaze_ids::ContentId::of("test fn", b"seven"));
    let seven = Instance { item: ItemKey(40), args: List::EMPTY };
    w.promoted_fns.push((seven, key));
    let mut b = BodyBuf::new();
    let f = b.path("seven");
    let f = b.e(Expr::Path(f));
    let arr = b.array(&[f]);
    let inner = b.e(Expr::Ref { mutbl: Mutbl::Not, operand: arr });
    let root = b.e(Expr::Ref { mutbl: Mutbl::Not, operand: inner });
    w.promoted.push((inner, Some(rows(&[(&[0; 8], 3, &[(0, Fn(key))])]))));
    w.promoted.push((root, Some(rows(&[(&[0; 8], 3, &[(0, Local(1))]), (&[0; 8], 3, &[(0, Fn(key))])]))));
    let built = build(&w, &b, root).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(built.refs.as_slice(), &[seven], "`seven` is the function's one reference");
    let text = blaze_lir::print(built.func.view());
    assert_eq!(data_rows(&text), 2, "{text}");
}
