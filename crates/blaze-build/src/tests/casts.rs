//! Casts whose source is not a plain scalar (walk/casts.rs): wide pointers, enums, fn items;
//! built from a body the checker types, run natively, the result compared.

use super::*;

/// The Reference, "Type cast expressions": "`*T` where `T: ?Sized` -> `*V` where `V: ?Sized`"
/// keeps the metadata; to a `Sized` pointee it is the address; "Function item -> Function
/// pointer"; "Enumeration -> Integer type": "Casts an enum to its discriminant" (unit-only
/// `enum K { X, Y, Z }`: 0, 1, 2, "Enumerations": "if the discriminant of the previous variant
/// was ... the next is one more").
/// ```text
/// fn id(x: u32) -> u32 { x }
/// fn main() -> u32 {
///     let a = [5u8, 6, 9];
///     let s: &[u8] = &a;
///     let p = s as *const [u8];
///     let q = p as *const u8;
///     let f = id as fn(u32) -> u32;
///     let z = K::Z;
///     (z as u32) * 1000 + (*q as u32) * 100 + f(7)     // 2000 + 500 + 7
/// }
/// ```
#[test]
fn wide_pointers_enums_and_fn_items_cast() {
    let mut w = TestWorld::new();
    w.func(1, "main", &[], U32);
    w.func(3, "id", &[U32], U32);
    w.enum_(40, &[("X", CtorKind::Unit, &[]), ("Y", CtorKind::Unit, &[]), ("Z", CtorKind::Unit, &[])]);
    let mut b = BodyBuf::new();
    let u8_ty = b.types.intern(U8).expect("a type interns");
    let u32_ty = b.types.intern(U32).expect("a type interns");
    let slice = b.types.intern(PNode::Slice(u8_ty)).expect("a type interns");
    let ref_slice = b.types.intern(PNode::Ref(Mutbl::Not, slice)).expect("a type interns");
    let raw_slice = b.types.intern(PNode::Ptr(Mutbl::Not, slice)).expect("a type interns");
    let raw_u8 = b.types.intern(PNode::Ptr(Mutbl::Not, u8_ty)).expect("a type interns");
    let fl = b.types.intern_list(&[u32_ty, u32_ty]).expect("a list interns");
    let fn_ty = b.types.intern(PNode::FnPtr(fl, blaze_typeck::FnPtrHeader::RUST)).expect("a type interns");
    let (a, s, p, q, f, z) = (b.bind("a"), b.bind("s"), b.bind("p"), b.bind("q"), b.bind("f"), b.bind("z"));
    let e5 = b.int(5, None);
    let e6 = b.int(6, None);
    let e9 = b.int(9, None);
    let arr = b.array(&[e5, e6, e9]);
    let a_e = b.local(a);
    let s_init = b.e(Expr::Ref { mutbl: Mutbl::Not, operand: a_e });
    let s_e = b.local(s);
    let p_init = b.e(Expr::Cast { operand: s_e, ty: raw_slice });
    let p_e = b.local(p);
    let q_init = b.e(Expr::Cast { operand: p_e, ty: raw_u8 });
    let id_path = b.path("id");
    let id = b.e(Expr::Path(id_path));
    let f_init = b.e(Expr::Cast { operand: id, ty: fn_ty });
    let z_path = b.path("Z");
    let z_init = b.e(Expr::Path(z_path));
    let z_e = b.local(z);
    let z32 = b.e(Expr::Cast { operand: z_e, ty: u32_ty });
    let k = b.int(1000, None);
    let z_k = b.bin(BinOp::Mul, z32, k);
    let q_e = b.local(q);
    let first = b.e(Expr::Unary { op: blaze_typeck::UnOp::Deref, operand: q_e });
    let first = b.e(Expr::Cast { operand: first, ty: u32_ty });
    let k = b.int(100, None);
    let first_k = b.bin(BinOp::Mul, first, k);
    let f_e = b.local(f);
    let seven = b.int(7, None);
    let args = b.exprs_list(&[seven]);
    let call = b.e(Expr::Call { callee: f_e, args });
    let sum = b.bin(BinOp::Add, z_k, first_k);
    let sum = b.bin(BinOp::Add, sum, call);
    let root = b.block(
        &[
            Stmt::Let { pat: a, ty: None, init: Some(arr), else_: None },
            Stmt::Let { pat: s, ty: Some(ref_slice), init: Some(s_init), else_: None },
            Stmt::Let { pat: p, ty: None, init: Some(p_init), else_: None },
            Stmt::Let { pat: q, ty: None, init: Some(q_init), else_: None },
            Stmt::Let { pat: f, ty: None, init: Some(f_init), else_: None },
            Stmt::Let { pat: z, ty: None, init: Some(z_init), else_: None },
        ],
        Some(sum),
    );
    let built = build(&w, &b, root).unwrap_or_else(|e| panic!("{e}"));
    let mut callee = BodyBuf::new();
    let x = callee.bind("x");
    callee.params.push(x);
    let x_e = callee.local(x);
    let root = callee.block(&[], Some(x_e));
    let mut g = PortableTys::new_in(Alloc::global());
    let id = build_instance(&w, &callee, ItemKey(3), root, &[], &mut g).unwrap_or_else(|e| panic!("{e}"));
    #[cfg(target_arch = "aarch64")]
    assert_eq!(run_native(&[(ItemKey(3), &id), (MAIN, &built)]) as u32, 2507);
    let _ = (built, id);
}
