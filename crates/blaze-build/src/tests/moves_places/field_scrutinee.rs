//! A match on a FIELD OF AN OWNED LOCAL (walk/moves/field_scrutinee.rs): tested in place, a
//! by-value binding moving the part out by the local's field flags, the rest dropped at the
//! local's scope end. Found on the hello-world path: bstd's `Held::end` (`fn end(mut self)`,
//! `match self.failed { Some(error) => Err(error), None => Ok(()) }`) was refused as "a move
//! out of a field of other than a temporary or a local with field flags (behind a pointer ..)".

use super::*;

/// The Reference, "Match expressions": "If the scrutinee expression is a place expression, the
/// match does not allocate a temporary location; however, a by-value binding may copy or move
/// from the memory location"; [expr.move.movable-place]: "Only the following place expressions
/// may be moved out of: Variables which are not currently borrowed. ... Fields of a place
/// expression which can be moved out of and don't implement `Drop`"; "Destructors": "If a
/// variable has been partially initialized, only its initialized fields are dropped", a
/// tuple's fields in order, a scope's variables in reverse order of declaration, and a match
/// arm's bindings when the arm ends.
/// ```text
/// fn main() {                                                     // sel = 1     sel = 0
///     let t = (sel, G { x: 1 }, G { x: 2 });
///     let y = match t.1 { ref a => a.x };                           // 1, nothing moves
///     let x = match t.0 { 0 => 7, _ => match t.2 { g => g.x } };   // g drops: 2  (no move)
///     let k = G { x: x * 10 + y };                                  // 21          71
/// }   // k, then what t still holds:                                 21, 1       71, 1, 2
/// ```
/// sel = 1: `[2, 21, 1]`; sel = 0: `[71, 1, 2]`. PLANTED BUGS this catches: the moved part's
/// bit not cleared gives `[2, 21, 1, 2]` (a double drop); the move taken as static (the field
/// skipped at the scope end without testing its bit) gives `[71, 1]` for sel = 0; `t.1`
/// matched by value (a copy held as a temporary) adds a drop of `1` after its statement; the
/// scrutinee evaluated as a value (the old path) fails the build with "a move out of a field
/// of other than a temporary or a local with field flags". Prints nothing; passes as
/// "test tests::moves_places::field_scrutinee::a_match_on_an_owned_locals_field_moves_by_its_field_flags ... ok".
#[test]
fn a_match_on_an_owned_locals_field_moves_by_its_field_flags() {
    let mut w = TestWorld::new();
    w.func(1, "main", &[], UNIT);
    let g = g_world(&mut w);
    let cases: [(u128, &[i32]); 2] = [(1, &[2, 21, 1]), (0, &[71, 1, 2])];
    for (sel, want) in cases {
        let mut b = BodyBuf::new();
        let mut stmts: Vec<Stmt, Global> = Vec::new_in(Global);
        // let t = (sel, G1, G2);
        let s = b.int(sel, None);
        let (g1, g2) = (g_lit(&mut b, 1), g_lit(&mut b, 2));
        let value = b.tuple(&[s, g1, g2]);
        let t = b.bind("t");
        stmts.push(Stmt::Let { pat: t, ty: None, init: Some(value), else_: None });
        // let y = match t.1 { ref a => a.x };
        let t1 = b.local(t);
        let t_1 = b.tfield(t1, 1);
        let a = ref_bind(&mut b, "a");
        let a_e = b.local(a);
        let a_x = b.field(a_e, "x");
        let m1 = match_on(&mut b, t_1, &[(a, a_x)]);
        let y = b.bind("y");
        stmts.push(Stmt::Let { pat: y, ty: None, init: Some(m1), else_: None });
        // let x = match t.0 { 0 => 7, _ => match t.2 { g => g.x } };
        let t2 = b.local(t);
        let t_2 = b.tfield(t2, 2);
        let gp = plain_bind(&mut b, "g");
        let g_e = b.local(gp);
        let g_x = b.field(g_e, "x");
        let inner = match_on(&mut b, t_2, &[(gp, g_x)]);
        let t0 = b.local(t);
        let t_0 = b.tfield(t0, 0);
        let zero = b.pat_int(0);
        let seven = b.int(7, None);
        let rest = b.pat(Pat::Wild);
        let m2 = match_on(&mut b, t_0, &[(zero, seven), (rest, inner)]);
        let x = b.bind("x");
        stmts.push(Stmt::Let { pat: x, ty: None, init: Some(m2), else_: None });
        // let k = G { x: x * 10 + y };
        let (xe, ye) = (b.local(x), b.local(y));
        let ten = b.int(10, None);
        let x10 = b.bin(BinOp::Mul, xe, ten);
        let sum = b.bin(BinOp::Add, x10, ye);
        let path = b.path("G");
        let start = b.field_inits.len() as u32;
        b.field_inits.push(FieldInit { name: Name::of("x"), expr: sum });
        let k_init = b.e(Expr::Struct { path, fields: List { start, len: 1 }, base: blaze_typeck::StructBase::None });
        let k = b.bind("k");
        stmts.push(Stmt::Let { pat: k, ty: None, init: Some(k_init), else_: None });
        let root = b.block(&stmts, None);
        let seen = drops_of(&w, &b, root, g, None).unwrap_or_else(|e| panic!("sel={sel}: {e}"));
        #[cfg(target_arch = "aarch64")]
        assert_eq!(seen.as_slice(), want, "sel={sel}");
        let _ = (seen, want);
    }
}
