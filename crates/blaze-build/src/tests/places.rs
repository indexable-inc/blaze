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

/// The Reference, "Dynamically Sized Types": "Structs may contain a DST as the last field; this
/// makes the struct itself a DST", and "pointers to slices also store the number of elements".
/// "Type layout", "The `C` representation": each field at the next offset that is a multiple of
/// its alignment, in declaration order; a slice `[u8]`'s alignment is `u8`'s, 1. So in
/// `#[repr(C)] struct Packet { tag: u16, body: [u8] }` `tag` is at 0 and `body` at 2, and a
/// `&Packet` is the address and the body's length (walk/unsized_tail.rs).
/// ```text
/// fn sum4(s: &[u8]) -> usize { s[0] as usize + s[1] as usize + s[2] as usize + s[3] as usize }
/// fn main() -> usize {
///     let a = [7u16, 0x0302, 0x0504];          // bytes 07 00 02 03 04 05 (little-endian)
///     let q: *const u8 = transmute(&a);
///     let p: &Packet = transmute((q, 4usize));  // tag 7, body [2, 3, 4, 5]
///     let t: (*const u8, usize) = transmute(&p.body);
///     p.tag as usize + p.body[1] as usize * 10 + t.1 * 100 + sum4(&p.body) * 1000
/// }
/// ```
/// 7 + 3 * 10 + 4 * 100 + (2 + 3 + 4 + 5) * 1000 = 14437. A body at offset 0 (the tail placed
/// without the prefix) reads `p.body[1]` as 0 and sums 7 + 0 + 2 + 3: 12407 is the wrong fix's.
#[test]
fn a_struct_with_a_slice_tail_is_read_through_its_wide_pointer() {
    const PACKET: ItemKey = ItemKey(40);
    const SUM4: ItemKey = ItemKey(7);
    let u16_ = PNode::Uint(UintTy::U16);
    let mut w = TestWorld::new();
    w.func(1, "main", &[], USIZE);
    w.bounds_fn();
    w.generic_func(6, "transmute", 2, &[PNode::Param(0)], PNode::Param(1));
    w.intrinsics.push((ItemKey(6), Intrinsic::Table(crate::intrinsics::Which::Transmute)));
    let u8_w = w.ty(U8);
    w.strukt(40, "Packet", &[("tag", u16_), ("body", PNode::Slice(u8_w))]);
    w.reprs.push((PACKET, ReprAttrs { c: true, ..ReprAttrs::RUST }));
    let ref_slice_w = w.ref_(Mutbl::Not, PNode::Slice(u8_w));
    w.func(7, "sum4", &[ref_slice_w], USIZE);

    // sum4: the four elements, each `s[i] as usize`.
    let mut c = BodyBuf::new();
    let s = c.bind("s");
    c.params.push(s);
    let usize_c = c.types.intern(USIZE).expect("a type interns");
    let mut sum = None;
    for i in 0..4u128 {
        let s_e = c.local(s);
        let ix = c.int(i, None);
        let el = c.index(s_e, ix);
        let el = c.e(Expr::Cast { operand: el, ty: usize_c });
        sum = Some(match sum {
            None => el,
            Some(acc) => c.bin(BinOp::Add, acc, el),
        });
    }
    let croot = c.block(&[], sum);
    let mut g = PortableTys::new_in(Alloc::global());
    let sum4 = build_instance(&w, &c, SUM4, croot, &[], &mut g).unwrap_or_else(|e| panic!("{e}"));

    let mut b = BodyBuf::new();
    let u8_ty = b.types.intern(U8).expect("a type interns");
    let usize_ty = b.types.intern(USIZE).expect("a type interns");
    let p_u8 = b.types.intern(PNode::Ptr(Mutbl::Not, u8_ty)).expect("a type interns");
    let packet = b.types.intern(PNode::Adt(PACKET, List::EMPTY)).expect("a type interns");
    let ref_packet = b.types.intern(PNode::Ref(Mutbl::Not, packet)).expect("a type interns");
    let l = b.types.intern_list(&[p_u8, usize_ty]).expect("a list interns");
    let pair = b.types.intern(PNode::Tuple(l)).expect("a type interns");
    let su16 = Some(IntSuffix::Uint(UintTy::U16));
    let (a, q, p, t) = (b.bind("a"), b.bind("q"), b.bind("p"), b.bind("t"));
    let e0 = b.int(7, su16);
    let e1 = b.int(0x0302, su16);
    let e2 = b.int(0x0504, su16);
    let arr = b.array(&[e0, e1, e2]);
    let a_e = b.local(a);
    let ref_a = b.e(Expr::Ref { mutbl: Mutbl::Not, operand: a_e });
    let q_init = b.call("transmute", &[ref_a]);
    let q_e = b.local(q);
    let four = b.int(4, Some(IntSuffix::Uint(UintTy::Usize)));
    let qn = b.tuple(&[q_e, four]);
    let p_init = b.call("transmute", &[qn]);
    let p_e = b.local(p);
    let body = b.field(p_e, "body");
    let ref_body = b.e(Expr::Ref { mutbl: Mutbl::Not, operand: body });
    let t_init = b.call("transmute", &[ref_body]);
    // p.tag as usize
    let p_e = b.local(p);
    let tag = b.field(p_e, "tag");
    let tag = b.e(Expr::Cast { operand: tag, ty: usize_ty });
    // p.body[1] as usize * 10
    let p_e = b.local(p);
    let body = b.field(p_e, "body");
    let one = b.int(1, None);
    let second = b.index(body, one);
    let second = b.e(Expr::Cast { operand: second, ty: usize_ty });
    let k = b.int(10, None);
    let second_k = b.bin(BinOp::Mul, second, k);
    // t.1 * 100
    let t_e = b.local(t);
    let len = b.tfield(t_e, 1);
    let k = b.int(100, None);
    let len_k = b.bin(BinOp::Mul, len, k);
    // sum4(&p.body) * 1000
    let p_e = b.local(p);
    let body = b.field(p_e, "body");
    let ref_body = b.e(Expr::Ref { mutbl: Mutbl::Not, operand: body });
    let summed = b.call("sum4", &[ref_body]);
    let k = b.int(1000, None);
    let sum_k = b.bin(BinOp::Mul, summed, k);
    let total = b.bin(BinOp::Add, tag, second_k);
    let total = b.bin(BinOp::Add, total, len_k);
    let total = b.bin(BinOp::Add, total, sum_k);
    let root = b.block(
        &[
            Stmt::Let { pat: a, ty: None, init: Some(arr), else_: None },
            Stmt::Let { pat: q, ty: Some(p_u8), init: Some(q_init), else_: None },
            Stmt::Let { pat: p, ty: Some(ref_packet), init: Some(p_init), else_: None },
            Stmt::Let { pat: t, ty: Some(pair), init: Some(t_init), else_: None },
        ],
        Some(total),
    );
    let built = build(&w, &b, root).unwrap_or_else(|e| panic!("{e}"));
    #[cfg(target_arch = "aarch64")]
    {
        let host = [(ItemKey(5), host_bounds as usize as u64)];
        assert_eq!(run_native_with(&host, &[(SUM4, None, &sum4), (MAIN, None, &built)]), 14437);
    }
    let _ = (built, sum4);
}
