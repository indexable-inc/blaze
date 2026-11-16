//! Places of unsized types (walk.rs, "Unsized places"): a dereference of a wide pointer is a
//! place with the pointer's metadata, and a wide pointer read out of memory is the whole pair.
//! Each built from a body the checker types, then run natively and its result compared.

use super::*;

/// The Reference, "Dereference operator": "On non-pointer types `*x` is equivalent to
/// `*std::ops::Deref::deref(&x)`"; on a reference it denotes the place it points to.
/// "Dynamically Sized Types": "Pointer types to DSTs are sized but have twice the size of the
/// size of pointers to sized types: pointers to slices also store the number of elements of the
/// slice". So `&*s` of `s: &[u8]` is `s` again, address and length, and `*r` of `r: &&[u8]`
/// reads both words.
/// ```text
/// fn main() -> usize {
///     let a = [7u8, 2, 3];
///     let s: &[u8] = &a;
///     let t: &[u8] = &*s;
///     let r: &&[u8] = &t;
///     let u: &[u8] = *r;
///     let p: (*const u8, usize) = transmute(u);
///     let v: &[u8] = r;      // a deref coercion
///     p.1 * 100 + *p.0 as usize + u[1] as usize * 1000 + v[2] as usize * 10_000   // 32307
/// }
/// ```
#[test]
fn a_wide_pointer_reborrows_and_reads_back_whole() {
    let mut w = TestWorld::new();
    w.func(1, "main", &[], USIZE);
    w.bounds_fn();
    w.generic_func(6, "transmute", 2, &[PNode::Param(0)], PNode::Param(1));
    w.intrinsics.push((ItemKey(6), Intrinsic::Table(crate::intrinsics::Which::Transmute)));
    let mut b = BodyBuf::new();
    let u8_ty = b.types.intern(U8).expect("a type interns");
    let usize_ty = b.types.intern(USIZE).expect("a type interns");
    let slice = b.types.intern(PNode::Slice(u8_ty)).expect("a type interns");
    let ref_slice = b.types.intern(PNode::Ref(Mutbl::Not, slice)).expect("a type interns");
    let ref_ref = b.types.intern(PNode::Ref(Mutbl::Not, ref_slice)).expect("a type interns");
    let p = b.types.intern(PNode::Ptr(Mutbl::Not, u8_ty)).expect("a type interns");
    let l = b.types.intern_list(&[p, usize_ty]).expect("a list interns");
    let pair = b.types.intern(PNode::Tuple(l)).expect("a type interns");
    let (a, s, t, r, u, pp) = (b.bind("a"), b.bind("s"), b.bind("t"), b.bind("r"), b.bind("u"), b.bind("p"));
    let e7 = b.int(7, None);
    let e2 = b.int(2, None);
    let e3 = b.int(3, None);
    let arr = b.array(&[e7, e2, e3]);
    let a_e = b.local(a);
    let s_init = b.e(Expr::Ref { mutbl: Mutbl::Not, operand: a_e });
    let s_e = b.local(s);
    let star_s = b.e(Expr::Unary { op: blaze_typeck::UnOp::Deref, operand: s_e });
    let t_init = b.e(Expr::Ref { mutbl: Mutbl::Not, operand: star_s });
    let t_e = b.local(t);
    let r_init = b.e(Expr::Ref { mutbl: Mutbl::Not, operand: t_e });
    let r_e = b.local(r);
    let u_init = b.e(Expr::Unary { op: blaze_typeck::UnOp::Deref, operand: r_e });
    let u_e = b.local(u);
    let p_init = b.call("transmute", &[u_e]);
    let p_e = b.local(pp);
    let len = b.tfield(p_e, 1);
    let k = b.int(100, None);
    let len_k = b.bin(BinOp::Mul, len, k);
    let p_e = b.local(pp);
    let addr = b.tfield(p_e, 0);
    let first = b.e(Expr::Unary { op: blaze_typeck::UnOp::Deref, operand: addr });
    let first = b.e(Expr::Cast { operand: first, ty: usize_ty });
    let u_e = b.local(u);
    let one = b.int(1, None);
    let second = b.index(u_e, one);
    let second = b.e(Expr::Cast { operand: second, ty: usize_ty });
    let k = b.int(1000, None);
    let second_k = b.bin(BinOp::Mul, second, k);
    let sum = b.bin(BinOp::Add, len_k, first);
    let sum = b.bin(BinOp::Add, sum, second_k);
    // `let v: &[u8] = r;`: a deref coercion (Deref, Deref, Borrow): the inner pointer is loaded
    // whole, then `v[2] * 10_000`.
    let vv = b.bind("v");
    let r_e = b.local(r);
    let v_e = b.local(vv);
    let two = b.int(2, None);
    let third = b.index(v_e, two);
    let third = b.e(Expr::Cast { operand: third, ty: usize_ty });
    let k = b.int(10_000, None);
    let third_k = b.bin(BinOp::Mul, third, k);
    let sum = b.bin(BinOp::Add, sum, third_k);
    let root = b.block(
        &[
            Stmt::Let { pat: a, ty: None, init: Some(arr), else_: None },
            Stmt::Let { pat: s, ty: Some(ref_slice), init: Some(s_init), else_: None },
            Stmt::Let { pat: t, ty: Some(ref_slice), init: Some(t_init), else_: None },
            Stmt::Let { pat: r, ty: Some(ref_ref), init: Some(r_init), else_: None },
            Stmt::Let { pat: u, ty: Some(ref_slice), init: Some(u_init), else_: None },
            Stmt::Let { pat: vv, ty: Some(ref_slice), init: Some(r_e), else_: None },
            Stmt::Let { pat: pp, ty: Some(pair), init: Some(p_init), else_: None },
        ],
        Some(sum),
    );
    let built = build(&w, &b, root).unwrap_or_else(|e| panic!("{e}"));
    #[cfg(target_arch = "aarch64")]
    {
        let host = [(ItemKey(5), host_bounds as usize as u64)];
        assert_eq!(run_native_with(&host, &[(MAIN, None, &built)]), 32307);
    }
    let _ = built;
}
