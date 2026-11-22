//! Match shapes tempo's build met refused (the census of 2026-11-17 04:35): a zero-sized
//! enum matched through references (a derived `Debug` of `enum IterError { Stalled }` is
//! `match self { Stalled => .. }`), and byte string literal patterns (`Components::
//! parse_single_component`'s `match comp { b"." => .., b".." => .., b"" => .., _ => .. }`).
//! Built from a body the checker types, run natively, the result compared.

use super::*;

/// A byte string literal pattern of `text`'s bytes (tests.rs `str_pat` reads `pat_strs`).
fn bytes_pat(b: &mut BodyBuf, text: &'static str) -> PatId {
    let p = b.pat(Pat::Lit(Lit::ByteStr(text.len() as u64)));
    b.pat_strs.push((p, text));
    p
}

fn arms(b: &mut BodyBuf, list: &[(PatId, ExprId)]) -> List {
    let start = b.arms.len() as u32;
    for &(pat, body) in list {
        b.arms.push(Arm { pat, guard: None, body });
    }
    List { start, len: list.len() as u32 }
}

/// `[bytes..]` of `u8`s.
fn byte_array(b: &mut BodyBuf, text: &str) -> ExprId {
    let mut elems: Vec<ExprId, Global> = Vec::new_in(Global);
    for &x in text.as_bytes() {
        elems.push(b.int(u128::from(x), Some(IntSuffix::Uint(UintTy::U8))));
    }
    b.array(&elems)
}

/// The Reference, "Literal patterns": "Literal patterns match exactly the same value as what
/// is created by the literal"; a byte string literal pattern is `&[u8; N]`, or `&[u8]` over a
/// slice (rustc's `check_pat_lit`). "Binding modes": a non-reference pattern over a reference
/// dereferences it, so a path pattern of a one-variant, zero-sized enum matches through `&`
/// and `&&` (nothing is read: the enum has no bytes).
/// ```text
/// enum Z { Only }
/// fn main() -> u32 {
///     let arr = [b'.', b'.'];
///     let s: &[u8] = &arr;
///     let a: u32 = match s { b"." => 1, b".." => 2, b"" => 3, _ => 4 };                   // 2
///     let arr2 = [b'a', b'b', b'c'];
///     let c: u32 = match &arr2 { b"abd" => 1, b"abc" => 2, _ => 3 };                     // 2
///     let arr3 = *b"0123456789";
///     let s3: &[u8] = &arr3;
///     let d: u32 = match s3 { b"0123456788" => 1, b"012345678" => 2, b"0123456789" => 3, _ => 4 }; // 3
///     let z = Z::Only;
///     let r = &z;
///     let rr = &r;
///     let e1: u32 = match r { Z::Only => 7 };
///     let e2: u32 = match rr { Z::Only => 8 };
///     a * 10000 + c * 1000 + d * 100 + e1 * 10 + e2                                       // 22_378
/// }
/// ```
/// Each wrong reading gives another number: a slice compared without its length (`a` 1, a
/// prefix: 12_378); the last word skipped (`d` 1: 22_178); a wrong byte matched (`c` 1:
/// 21_378). Before the fix, `b".."` was refused ("a literal or range pattern over other than
/// an integer, a `char` or a `bool`") and `match r` ("a match on other than an integer, a bool
/// or an enum").
#[test]
fn byte_strings_and_zero_sized_enums_through_references_match() {
    let mut w = TestWorld::new();
    w.func(1, "main", &[], U32);
    w.enum_(60, &[("Only", CtorKind::Unit, &[])]);
    let mut b = BodyBuf::new();
    let u32_ty = b.types.intern(U32).expect("a type interns");
    let u8_ty = b.types.intern(U8).expect("a type interns");
    let slice = b.types.intern(PNode::Slice(u8_ty)).expect("a type interns");
    let ref_slice = b.types.intern(PNode::Ref(Mutbl::Not, slice)).expect("a type interns");
    let mut stmts: Vec<Stmt, Global> = Vec::new_in(Global);
    let let_u32 = |b: &mut BodyBuf, stmts: &mut Vec<Stmt, Global>, name: &str, init: ExprId| -> PatId {
        let p = b.bind(name);
        stmts.push(Stmt::Let { pat: p, ty: Some(u32_ty), init: Some(init), else_: None });
        p
    };
    // let <name>: &[u8] = &<array of text>;
    let slice_of = |b: &mut BodyBuf, stmts: &mut Vec<Stmt, Global>, name: &str, text: &str| -> PatId {
        let arr = b.bind("arr");
        let init = byte_array(b, text);
        stmts.push(Stmt::Let { pat: arr, ty: None, init: Some(init), else_: None });
        let s = b.bind(name);
        let arr_e = b.local(arr);
        let s_init = b.e(Expr::Ref { mutbl: Mutbl::Not, operand: arr_e });
        stmts.push(Stmt::Let { pat: s, ty: Some(ref_slice), init: Some(s_init), else_: None });
        s
    };

    // let a: u32 = match s { b"." => 1, b".." => 2, b"" => 3, _ => 4 };
    let s = slice_of(&mut b, &mut stmts, "s", "..");
    let (q1, q2, q3, q4) = (bytes_pat(&mut b, "."), bytes_pat(&mut b, ".."), bytes_pat(&mut b, ""), b.pat(Pat::Wild));
    let (r1, r2, r3, r4) = (b.int(1, None), b.int(2, None), b.int(3, None), b.int(4, None));
    let list = arms(&mut b, &[(q1, r1), (q2, r2), (q3, r3), (q4, r4)]);
    let s_e = b.local(s);
    let ma = b.e(Expr::Match { scrutinee: s_e, arms: list });
    let a = let_u32(&mut b, &mut stmts, "a", ma);

    // let arr2 = [b'a', b'b', b'c']; let c: u32 = match &arr2 { b"abd" => 1, b"abc" => 2, _ => 3 };
    let arr2 = b.bind("arr2");
    let init = byte_array(&mut b, "abc");
    stmts.push(Stmt::Let { pat: arr2, ty: None, init: Some(init), else_: None });
    let (q1, q2, q3) = (bytes_pat(&mut b, "abd"), bytes_pat(&mut b, "abc"), b.pat(Pat::Wild));
    let (r1, r2, r3) = (b.int(1, None), b.int(2, None), b.int(3, None));
    let list = arms(&mut b, &[(q1, r1), (q2, r2), (q3, r3)]);
    let arr2_e = b.local(arr2);
    let r_arr2 = b.e(Expr::Ref { mutbl: Mutbl::Not, operand: arr2_e });
    let mc = b.e(Expr::Match { scrutinee: r_arr2, arms: list });
    let c = let_u32(&mut b, &mut stmts, "c", mc);

    // let d: u32 = match s3 { b"0123456788" => 1, b"012345678" => 2, b"0123456789" => 3, _ => 4 };
    let s3 = slice_of(&mut b, &mut stmts, "s3", "0123456789");
    let (q1, q2, q3, q4) = (bytes_pat(&mut b, "0123456788"), bytes_pat(&mut b, "012345678"), bytes_pat(&mut b, "0123456789"), b.pat(Pat::Wild));
    let (r1, r2, r3, r4) = (b.int(1, None), b.int(2, None), b.int(3, None), b.int(4, None));
    let list = arms(&mut b, &[(q1, r1), (q2, r2), (q3, r3), (q4, r4)]);
    let s3_e = b.local(s3);
    let md = b.e(Expr::Match { scrutinee: s3_e, arms: list });
    let d = let_u32(&mut b, &mut stmts, "d", md);

    // let z = Z::Only; let r = &z; let rr = &r;
    let z = b.bind("z");
    let only = b.path("Only");
    let z_init = b.e(Expr::Path(only));
    stmts.push(Stmt::Let { pat: z, ty: None, init: Some(z_init), else_: None });
    let r = b.bind("r");
    let z_e = b.local(z);
    let r_init = b.e(Expr::Ref { mutbl: Mutbl::Not, operand: z_e });
    stmts.push(Stmt::Let { pat: r, ty: None, init: Some(r_init), else_: None });
    let rr = b.bind("rr");
    let r_e = b.local(r);
    let rr_init = b.e(Expr::Ref { mutbl: Mutbl::Not, operand: r_e });
    stmts.push(Stmt::Let { pat: rr, ty: None, init: Some(rr_init), else_: None });
    // match r { Z::Only => 7 } and match rr { Z::Only => 8 }
    let through = |b: &mut BodyBuf, scrutinee: PatId, value: u128| -> ExprId {
        let path = b.path("Only");
        let pat = b.pat(Pat::Path(path));
        let body = b.int(value, None);
        let list = arms(b, &[(pat, body)]);
        let s = b.local(scrutinee);
        b.e(Expr::Match { scrutinee: s, arms: list })
    };
    let m1 = through(&mut b, r, 7);
    let e1 = let_u32(&mut b, &mut stmts, "e1", m1);
    let m2 = through(&mut b, rr, 8);
    let e2 = let_u32(&mut b, &mut stmts, "e2", m2);

    // a * 10000 + c * 1000 + d * 100 + e1 * 10 + e2
    let times = |b: &mut BodyBuf, x: PatId, k: u128| {
        let x = b.local(x);
        let k = b.int(k, None);
        b.bin(BinOp::Mul, x, k)
    };
    let (ak, ck, dk, e1k) = (times(&mut b, a, 10000), times(&mut b, c, 1000), times(&mut b, d, 100), times(&mut b, e1, 10));
    let e2_e = b.local(e2);
    let sum = b.bin(BinOp::Add, ak, ck);
    let sum = b.bin(BinOp::Add, sum, dk);
    let sum = b.bin(BinOp::Add, sum, e1k);
    let sum = b.bin(BinOp::Add, sum, e2_e);
    let root = b.block(&stmts, Some(sum));
    let built = build(&w, &b, root).unwrap_or_else(|e| panic!("{e}"));
    #[cfg(target_arch = "aarch64")]
    assert_eq!(run_native(&[(MAIN, &built)]) as u32, 22_378);
    let _ = built;
}

/// The Reference, "Path patterns" and `[patterns.path.const]`: a constant pattern matches a
/// value equal to the constant, compared structurally, leaf by leaf (rustc's `const_to_pat`):
/// an ADT's fields, an array's elements, an enum's variant then that variant's fields, a
/// float by `==`. tempo's `match y_parity.0 { U256::ZERO => .., U256::ONE => .., _ => .. }`.
/// ```text
/// struct U { limbs: [u64; 4] }
/// enum E { A(u32), B }
/// const ZERO: U = U { limbs: [0, 0, 0, 0] };   const ONE: U = U { limbs: [1, 0, 0, 0] };
/// const Y: U = ONE;                             const Y2: U = U { limbs: [1, 0, 0, 5] };
/// const EA7: E = A(7);  const EA8: E = A(8);  const EB: E = B;
/// const EBV: E = B;     // its record holds 9 where `A`'s field would be: bytes no field reads
/// const NAN: f64 = f64::NAN;  const ZEROF: f64 = 0.0;  const NEGZ: f64 = -0.0;
/// const PAIR: (u8, u32) = (3, 9);  const PAIR_B: (u8, u32) = (3, 8);
/// fn main() -> u32 {
///     let a = match Y { ZERO => 1, ONE => 2, _ => 3 };              // 2
///     let b = match Y2 { ONE => 1, _ => 2 };                        // 2
///     let c = match EA8 { EA7 => 1, EB => 2, _ => 3 };              // 3
///     let d = match EBV { EA7 => 1, EB => 2, _ => 3 };              // 2
///     let f = match NAN { ZEROF => 1, NEGZ => 2, _ => 3 };          // 3
///     let g = match NEGZ { ZEROF => 1, _ => 2 };                    // 1
///     let t = match PAIR { PAIR_B => 1, PAIR => 2, _ => 3 };        // 2
///     a * 1000000 + b * 100000 + c * 10000 + d * 1000 + f * 100 + g * 10 + t   // 2_232_312
/// }
/// ```
/// Each wrong reading gives another number: one limb compared (`b` 1: 2_132_312), the tag
/// alone (`c` 1: 2_212_312), the bytes of a variant's fields not its own (`d` 3: 2_233_312),
/// floats by their bits (`g` 2: 2_232_322). A NaN constant as a pattern is refused, as rustc
/// refuses it ("cannot use NaN in patterns"): `match NAN { NAN => .. }` is the control.
/// Before the fix every arm was refused ("a structural match of an ADT constant", "a
/// constant of a float type as a pattern", "a constant of other than an integer, a `char` or
/// a `bool` type as a pattern").
#[test]
fn constant_patterns_compare_structurally() {
    let mut w = TestWorld::new();
    w.func(1, "main", &[], U32);
    let limbs = w.array(PNode::Uint(UintTy::U64), 4);
    w.strukt(70, "U", &[("limbs", limbs)]);
    w.enum_(90, &[("A", CtorKind::Tuple, &[("0", U32)]), ("B", CtorKind::Unit, &[])]);
    let u = PNode::Adt(ItemKey(70), List::EMPTY);
    let e = PNode::Adt(ItemKey(90), List::EMPTY);
    let f64_ = PNode::Float(blaze_typeck::FloatTy::F64);
    let pair = w.tuple(&[U8, U32]);
    let limbs_bytes = |l: [u64; 4]| -> std::vec::Vec<u8> { l.iter().flat_map(|x| x.to_le_bytes()).collect() };
    // (key, name, type, bytes, log2 alignment)
    let consts: [(u128, &str, PNode, std::vec::Vec<u8>, u8); 13] = [
        (100, "ZERO", u, limbs_bytes([0, 0, 0, 0]), 3),
        (101, "ONE", u, limbs_bytes([1, 0, 0, 0]), 3),
        (102, "Y", u, limbs_bytes([1, 0, 0, 0]), 3),
        (103, "Y2", u, limbs_bytes([1, 0, 0, 5]), 3),
        // The tag first, `A`'s `u32` at 4 (a `u8` or a `u32` tag reads the same bytes).
        (104, "EA7", e, std::vec![0, 0, 0, 0, 7, 0, 0, 0], 2),
        (105, "EA8", e, std::vec![0, 0, 0, 0, 8, 0, 0, 0], 2),
        (106, "EB", e, std::vec![1, 0, 0, 0, 0, 0, 0, 0], 2),
        (107, "EBV", e, std::vec![1, 0, 0, 0, 9, 0, 0, 0], 2),
        (108, "NAN", f64_, f64::NAN.to_le_bytes().to_vec(), 3),
        (109, "ZEROF", f64_, 0.0f64.to_le_bytes().to_vec(), 3),
        (110, "NEGZ", f64_, (-0.0f64).to_le_bytes().to_vec(), 3),
        // repr(Rust) orders by alignment: the `u32` (.1) at 0, the `u8` (.0) at 4.
        (111, "PAIR", pair, std::vec![9, 0, 0, 0, 3, 0, 0, 0], 2),
        (112, "PAIR_B", pair, std::vec![8, 0, 0, 0, 3, 0, 0, 0], 2),
    ];
    for (key, name, ty, bytes, align) in consts.iter() {
        w.func(*key, name, &[], *ty);
        w.consts.push((ItemKey(*key), Some(record(bytes, *align))));
    }
    let mut b = BodyBuf::new();
    let mut stmts: Vec<Stmt, Global> = Vec::new_in(Global);
    // let <name>: u32 = match <scrutinee> { <consts> => 1, 2, .., _ => n + 1 };
    let matched = |b: &mut BodyBuf, stmts: &mut Vec<Stmt, Global>, name: &str, scrutinee: &str, pats: &[&str]| -> PatId {
        let u32_ty = b.types.intern(U32).expect("a type interns");
        let mut list: Vec<(PatId, ExprId), Global> = Vec::new_in(Global);
        for (i, &c) in pats.iter().enumerate() {
            let path = b.path(c);
            let p = b.pat(Pat::Path(path));
            let r = b.int(i as u128 + 1, None);
            list.push((p, r));
        }
        let wild = b.pat(Pat::Wild);
        let r = b.int(pats.len() as u128 + 1, None);
        list.push((wild, r));
        let arms = arms(b, &list);
        let s = b.path(scrutinee);
        let s = b.e(Expr::Path(s));
        let m = b.e(Expr::Match { scrutinee: s, arms });
        let p = b.bind(name);
        stmts.push(Stmt::Let { pat: p, ty: Some(u32_ty), init: Some(m), else_: None });
        p
    };
    let a = matched(&mut b, &mut stmts, "a", "Y", &["ZERO", "ONE"]);
    let bb = matched(&mut b, &mut stmts, "b", "Y2", &["ONE"]);
    let c = matched(&mut b, &mut stmts, "c", "EA8", &["EA7", "EB"]);
    let d = matched(&mut b, &mut stmts, "d", "EBV", &["EA7", "EB"]);
    let f = matched(&mut b, &mut stmts, "f", "NAN", &["ZEROF", "NEGZ"]);
    let g = matched(&mut b, &mut stmts, "g", "NEGZ", &["ZEROF"]);
    let t = matched(&mut b, &mut stmts, "t", "PAIR", &["PAIR_B", "PAIR"]);
    let mut sum = b.local(t);
    for (x, k) in [(a, 1_000_000u128), (bb, 100_000), (c, 10_000), (d, 1000), (f, 100), (g, 10)] {
        let x = b.local(x);
        let k = b.int(k, None);
        let term = b.bin(BinOp::Mul, x, k);
        sum = b.bin(BinOp::Add, sum, term);
    }
    let root = b.block(&stmts, Some(sum));
    let built = build(&w, &b, root).unwrap_or_else(|e| panic!("{e}"));
    #[cfg(target_arch = "aarch64")]
    assert_eq!(run_native(&[(MAIN, &built)]) as u32, 2_232_312);
    let _ = built;

    // The control: a NaN constant as a pattern is refused by name.
    let mut b = BodyBuf::new();
    let mut stmts: Vec<Stmt, Global> = Vec::new_in(Global);
    let n = matched(&mut b, &mut stmts, "n", "ZEROF", &["NAN"]);
    let n = b.local(n);
    let root = b.block(&stmts, Some(n));
    let refused = build(&w, &b, root).err().unwrap_or_default();
    assert!(refused.contains("a NaN constant as a pattern"), "{refused}");
}

/// A by-value binding of a part that needs drop inside an or-pattern (tests.rs `Arms::Or`):
/// `match x { A(_, s) | B(s) => {}, _ => {} }; let k = G { x: 9 };` in tests.rs `match_world`
/// (`enum E { A(G, G), B(G), C, W(N, G) }`, `G` logs its `x` when dropped). Before: refused
/// with "a binding by value of a part that needs drop, in a match by tests (a move out)": the
/// alternative binds on its own pass, inside the tests, where no arm was set
/// (walk/matches.rs `arm_tested` now sets it first; walk/moves.rs `move_path` follows the
/// alternatives).
///
/// The Reference, "Or-patterns": the alternatives bind the same names, the one that matched
/// gives the value; "Destructors": the binding drops at its arm's end, the scrutinee's rest
/// with its owner (a local at its scope's end after `k`, a temporary at its statement's end),
/// never the moved part again:
/// - a local `A(G1, G2)`: `s` = G2 at the arm's end, then `k`, then `x`'s G1: 2, 9, 1;
///   `B(G3)`: 3, 9; `C`: 9; `W(P(G5), G6)` (the `_` arm): 9, 5, 6.
/// - a temporary: `A`: 2, then its G1 at the statement's end, then `k`: 2, 1, 9; `W`: 5, 6, 9.
///
/// The control: a guarded arm runs its guard once per alternative (matches/alts.rs), so the
/// same pattern under `if 1 == 1` stays refused by name.
///
/// Prints: `test tests::match_shapes::or_pattern_bindings_move_their_part_out ... ok`
#[test]
fn or_pattern_bindings_move_their_part_out() {
    let cases: &[(Scrut, bool, &[i32])] = &[
        (Scrut::A, false, &[2, 9, 1]),
        (Scrut::B, false, &[3, 9]),
        (Scrut::C, false, &[9]),
        (Scrut::W, false, &[9, 5, 6]),
        (Scrut::A, true, &[2, 1, 9]),
        (Scrut::B, true, &[3, 9]),
        (Scrut::W, true, &[5, 6, 9]),
    ];
    let mut failed = String::new();
    for &(s, temp, want) in cases {
        match match_drops(s, Arms::Or, temp) {
            // Off aarch64 built only (`g_drops`), nothing seen.
            Ok(seen) if cfg!(target_arch = "aarch64") && seen.as_slice() != want => failed.push_str(&format!("{s:?} temp={temp}: dropped {seen:?}, want {want:?}\n")),
            Ok(_) => {}
            Err(e) => failed.push_str(&format!("{s:?} temp={temp}: {e}\n")),
        }
    }
    assert!(failed.is_empty(), "{failed}");
    let refused = match_drops(Scrut::A, Arms::OrGuard, false).err().unwrap_or_default();
    assert!(refused.contains("in an or-pattern of a guarded arm"), "{refused}");
}
