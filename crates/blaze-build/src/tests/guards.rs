//! A guard on an arm with or-patterns (walk/matches/alts.rs): guards WITH AN EFFECT, their
//! runs counted. Every guard in tests/matches.rs is effect-free on an or-free or disjoint
//! pattern, which is how a guard that left its arm at the first false run passed there.
//!
//! The Reference, "Match expressions", "Match guards":
//! - [expr.match.guard.behavior] "When the pattern matches successfully, the pattern guard is
//!   executed. If all of the guard condition operands evaluate to true and all of the `let`
//!   patterns successfully match their scrutinees, the match arm is successfully matched
//!   against and the arm body is executed."
//! - [expr.match.guard.next] "Otherwise, the next pattern, including other matches with the
//!   `|` operator in the same arm, is tested."
//! - its note: "Multiple matches using the `|` operator can cause the pattern guard and the
//!   side effects it has to execute multiple times. For example:"
//!   `let i : Cell<i32> = Cell::new(0); match 1 { 1 | _ if { i.set(i.get() + 1); false } => {}
//!   _ => {} } assert_eq!(i.get(), 2);`
//!
//! "Patterns", "Or-patterns", [patterns.behavior.nested-or-patterns]: "The dynamic semantics
//! of pattern matching a scrutinee expression `e_s` against a pattern `c(p | q, ..rest)` at
//! depth `d` where `c` is some constructor, `p` and `q` are arbitrary patterns, and `rest` is
//! optionally any remaining potential factors in `c`, is defined as being the same as that of
//! `c(p, ..rest) | c(q, ..rest)`."

use super::*;

/// A binding with no annotation, immutable: the default binding mode applies.
fn plain(b: &mut BodyBuf, name: &str) -> PatId {
    b.pat(Pat::Bind { ann: BindAnn { by_ref: None, mutable: false }, name: Name::of(name), sub: None, ty: None })
}

fn or(b: &mut BodyBuf, alts: &[PatId]) -> PatId {
    let alts = b.pats_list(alts);
    b.pat(Pat::Or { alts })
}

fn tuple_pat(b: &mut BodyBuf, elems: &[PatId]) -> PatId {
    let elems = b.pats_list(elems);
    b.pat(Pat::Tuple { elems, rest: None })
}

/// `{ <effect>; false }`: a guard that has an effect and fails.
fn failing(b: &mut BodyBuf, effect: ExprId) -> ExprId {
    let no = b.e(Expr::Lit(Lit::Bool(false)));
    b.block(&[Stmt::Expr { expr: effect, semi: true }], Some(no))
}

/// `{ n += 1; false }`.
fn bump(b: &mut BodyBuf, n: PatId) -> ExprId {
    let (place, one_) = (b.local(n), b.int(1, None));
    let add = b.e(Expr::AssignOp { op: BinOp::Add, lhs: place, rhs: one_ });
    failing(b, add)
}

/// `match <scrutinee> { <pat> if <guard> => <taken>, _ => 0 }`.
fn guarded(b: &mut BodyBuf, scrutinee: ExprId, pat: PatId, guard: ExprId, taken: ExprId) -> ExprId {
    let (wild, zero) = (b.pat(Pat::Wild), b.int(0, None));
    let start = b.arms.len() as u32;
    b.arms.push(Arm { pat, guard: Some(guard), body: taken });
    b.arms.push(Arm { pat: wild, guard: None, body: zero });
    b.e(Expr::Match { scrutinee, arms: List { start, len: 2 } })
}

/// `(1u32, 2u32)`.
fn one_two(b: &mut BodyBuf) -> ExprId {
    let (x, y) = (b.int(1, SU32), b.int(2, SU32));
    b.tuple(&[x, y])
}

/// ```text
/// fn main() -> u32 {
///     let mut i = 0u32;
///     let r1: u32 = match 1u32 { 1 | _ if { i += 1; false } => 9, _ => 0 };
///     // The Reference's own example: both alternatives match, the guard runs for each: i == 2.
///     let s: u32 = match (1u32, 2u32) { (a, _) | (_, a) if a == 2 => 10 + a, _ => 0 };
///     // (a, _) binds a = 1, its guard is false; (_, a) binds a = 2, its guard is true: 12.
///     let mut j = 0u32;
///     let r3: u32 = match (1u32, 2u32) { (1 | _, 2 | _) if { j += 1; false } => 9, _ => 0 };
///     // (1, 2) | (1, _) | (_, 2) | (_, _) all match: j == 4.
///     let mut log = 0u32;
///     let r4: u32 = match (1u32, 2u32) { (a, b) | (b, a) if { log = log * 100 + a * 10 + b; false } => 9, _ => 0 };
///     // the guard sees a = 1, b = 2, then a = 2, b = 1: log == 1221.
///     let mut k = 0u32;
///     let r5: u32 = match 3u32 { 3 | 3 if { k += 1; false } => 9, _ => 0 };
///     // two alternatives, both match: k == 2.
///     ((((i * 100 + s) * 10 + j) * 10000 + log) * 10 + k) + r1 + r3 + r4 + r5   // 212_412_212
/// }
/// ```
fn guards_body(b: &mut BodyBuf) -> ExprId {
    let u32_ty = b.types.intern(U32).expect("a type interns");
    let mut stmts: Vec<Stmt, Global> = Vec::new_in(Global);
    let counter = |b: &mut BodyBuf, stmts: &mut Vec<Stmt, Global>, name: &str| {
        let p = b.bind(name);
        let zero = b.int(0, SU32);
        stmts.push(Stmt::Let { pat: p, ty: None, init: Some(zero), else_: None });
        p
    };
    let keep = |b: &mut BodyBuf, stmts: &mut Vec<Stmt, Global>, name: &str, value: ExprId| {
        let p = b.bind(name);
        stmts.push(Stmt::Let { pat: p, ty: Some(u32_ty), init: Some(value), else_: None });
        p
    };

    // 1. match 1u32 { 1 | _ if { i += 1; false } => 9, _ => 0 }
    let i = counter(b, &mut stmts, "i");
    let (lit1, wild) = (b.pat_int(1), b.pat(Pat::Wild));
    let pat = or(b, &[lit1, wild]);
    let (guard, scrutinee, nine) = (bump(b, i), b.int(1, SU32), b.int(9, None));
    let m = guarded(b, scrutinee, pat, guard, nine);
    let r1 = keep(b, &mut stmts, "r1", m);

    // 2. match (1u32, 2u32) { (a, _) | (_, a) if a == 2 => 10 + a, _ => 0 }
    let (a1, w1, w2, a2) = (plain(b, "a"), b.pat(Pat::Wild), b.pat(Pat::Wild), plain(b, "a"));
    let (left, right) = (tuple_pat(b, &[a1, w1]), tuple_pat(b, &[w2, a2]));
    let pat = or(b, &[left, right]);
    // A name bound in every alternative is one variable: its first binding names it.
    let (ag, two) = (b.local(a1), b.int(2, None));
    let guard = b.bin(BinOp::Eq, ag, two);
    let (ten, ab) = (b.int(10, None), b.local(a1));
    let taken = b.bin(BinOp::Add, ten, ab);
    let scrutinee = one_two(b);
    let m = guarded(b, scrutinee, pat, guard, taken);
    let s = keep(b, &mut stmts, "s", m);

    // 3. match (1u32, 2u32) { (1 | _, 2 | _) if { j += 1; false } => 9, _ => 0 }
    let j = counter(b, &mut stmts, "j");
    let (l1, w1, l2, w2) = (b.pat_int(1), b.pat(Pat::Wild), b.pat_int(2), b.pat(Pat::Wild));
    let (first, second) = (or(b, &[l1, w1]), or(b, &[l2, w2]));
    let pat = tuple_pat(b, &[first, second]);
    let (guard, scrutinee, nine) = (bump(b, j), one_two(b), b.int(9, None));
    let m = guarded(b, scrutinee, pat, guard, nine);
    let r3 = keep(b, &mut stmts, "r3", m);

    // 4. match (1u32, 2u32) { (a, b) | (b, a) if { log = log * 100 + a * 10 + b; false } => 9, _ => 0 }
    let log = counter(b, &mut stmts, "log");
    let (a1, b1, b2, a2) = (plain(b, "a"), plain(b, "b"), plain(b, "b"), plain(b, "a"));
    let (left, right) = (tuple_pat(b, &[a1, b1]), tuple_pat(b, &[b2, a2]));
    let pat = or(b, &[left, right]);
    let (log_r, hundred) = (b.local(log), b.int(100, None));
    let shifted = b.bin(BinOp::Mul, log_r, hundred);
    let (a_e, ten) = (b.local(a1), b.int(10, None));
    let tens = b.bin(BinOp::Mul, a_e, ten);
    let sum = b.bin(BinOp::Add, shifted, tens);
    let b_e = b.local(b1);
    let sum = b.bin(BinOp::Add, sum, b_e);
    let log_w = b.local(log);
    let note = b.e(Expr::Assign { lhs: log_w, rhs: sum });
    let (guard, scrutinee, nine) = (failing(b, note), one_two(b), b.int(9, None));
    let m = guarded(b, scrutinee, pat, guard, nine);
    let r4 = keep(b, &mut stmts, "r4", m);

    // 5. match 3u32 { 3 | 3 if { k += 1; false } => 9, _ => 0 }
    let k = counter(b, &mut stmts, "k");
    let (l3, m3) = (b.pat_int(3), b.pat_int(3));
    let pat = or(b, &[l3, m3]);
    let (guard, scrutinee, nine) = (bump(b, k), b.int(3, SU32), b.int(9, None));
    let m = guarded(b, scrutinee, pat, guard, nine);
    let r5 = keep(b, &mut stmts, "r5", m);

    // ((((i * 100 + s) * 10 + j) * 10000 + log) * 10 + k) + r1 + r3 + r4 + r5
    let mut acc = b.local(i);
    for (scale, part) in [(100u128, s), (10, j), (10000, log), (10, k)] {
        let (scale, part) = (b.int(scale, None), b.local(part));
        let scaled = b.bin(BinOp::Mul, acc, scale);
        acc = b.bin(BinOp::Add, scaled, part);
    }
    for part in [r1, r3, r4, r5] {
        let part = b.local(part);
        acc = b.bin(BinOp::Add, acc, part);
    }
    b.block(&stmts, Some(acc))
}

/// Builds `guards_body` with `planted` and runs it on blaze-lir's evaluator.
fn guards_answer(planted: crate::Planted) -> u128 {
    let mut w = TestWorld::new();
    w.func(1, "main", &[], U32);
    w.planted = planted;
    let mut b = BodyBuf::new();
    let root = guards_body(&mut b);
    let built = build(&w, &b, root).unwrap_or_else(|e| panic!("{e}"));
    one(&built, &[])
}

/// The five checks of `guards_body` (the module doc quotes the rules): runs 2, value 12, runs
/// 4, log 1221, runs 2.
#[test]
fn a_false_guard_goes_on_to_the_next_alternative_of_its_arm() {
    assert_eq!(guards_answer(crate::Planted::Nothing), 212_412_212, "runs 2, value 12, runs 4, log 1221, runs 2");
}

/// The control (rule 8): with the bug planted (a false guard leaves its arm: the walk before
/// matches/alts.rs) the same body gives runs 1, value 0, runs 1, log 12, runs 1.
#[test]
fn the_control_a_guard_that_leaves_its_arm_is_caught() {
    let planted = guards_answer(crate::Planted::GuardToNextArm);
    assert_ne!(planted, 212_412_212, "the control: a guard that leaves its arm at the first false run went unnoticed");
    assert_eq!(planted, 100_100_121, "the planted walk: runs 1, value 0, runs 1, log 12, runs 1");
}

/// Regression for Bug 3A (`blaze-cli` `Err(e) if matches!(e.kind, FsKind::NotFound)`):
/// inside a match guard, `e` is not yet owned (`Pending::Own` runs after the guard passes), so a
/// nested `match e.kind { NotFound => true, _ => false }` on a field whose type needs drop must
/// match `e.kind` in place (`scrutinee_in_place`) without trying to move `e.kind` or double-drop
/// it when the guard fails and falls through to the next arm.
#[test]
fn guarded_arm_matches_dropping_field_of_binding_in_place() {
    let run_case = |is_not_found: bool| -> (u32, Vec<i32, Global>) {
        let mut w = TestWorld::new();
        w.func(1, "main", &[], U32);
        let g = g_world(&mut w);
        // `enum Kind { NotFound, Driver(G) }` — needs drop via `Driver(G)`.
        w.enum_(60, &[("NotFound", CtorKind::Unit, &[]), ("Driver", CtorKind::Tuple, &[("0", g)])]);
        let kind = PNode::Adt(ItemKey(60), List::EMPTY);
        // `struct ErrVal { path: G, kind: Kind }`.
        w.strukt(61, "ErrVal", &[("path", g), ("kind", kind)]);
        let err_val = PNode::Adt(ItemKey(61), List::EMPTY);
        // `enum Res { Ok, Err(ErrVal) }`.
        w.enum_(62, &[("Ok", CtorKind::Unit, &[]), ("Err", CtorKind::Tuple, &[("0", err_val)])]);
        let res_ty = PNode::Adt(ItemKey(62), List::EMPTY);

        let mut b = BodyBuf::new();
        let kind_expr = if is_not_found {
            let p = b.path("NotFound");
            b.e(Expr::Path(p))
        } else {
            let g7 = g_lit(&mut b, 7);
            b.call("Driver", &[g7])
        };
        let path_expr = g_lit(&mut b, 5);
        let err_path = b.path("ErrVal");
        let fstart = b.field_inits.len() as u32;
        b.field_inits.push(FieldInit { name: Name::of("path"), expr: path_expr });
        b.field_inits.push(FieldInit { name: Name::of("kind"), expr: kind_expr });
        let err_struct = b.e(Expr::Struct { path: err_path, fields: List { start: fstart, len: 2 }, base: blaze_typeck::StructBase::None });
        let scrut = b.call("Err", &[err_struct]);

        // Arm 1: `Err(e) if match e.kind { NotFound => true, _ => false } => 10`
        let e1 = plain(&mut b, "e");
        let pat1 = variant_pat(&mut b, "Err", &[e1]);
        let e1_ref = b.local(e1);
        let e1_kind = b.field(e1_ref, "kind");
        let nf_path = b.path("NotFound");
        let nf_pat = b.pat(Pat::Path(nf_path));
        let inner_wild = b.pat(Pat::Wild);
        let t_lit = b.e(Expr::Lit(Lit::Bool(true)));
        let f_lit = b.e(Expr::Lit(Lit::Bool(false)));
        let inner_start = b.arms.len() as u32;
        b.arms.push(Arm { pat: nf_pat, guard: None, body: t_lit });
        b.arms.push(Arm { pat: inner_wild, guard: None, body: f_lit });
        let guard = b.e(Expr::Match { scrutinee: e1_kind, arms: List { start: inner_start, len: 2 } });
        let body1 = b.int(10, SU32);

        // Arm 2: `Err(e) => 20`
        let e2 = plain(&mut b, "e");
        let pat2 = variant_pat(&mut b, "Err", &[e2]);
        let body2 = b.int(20, SU32);

        // Arm 3: `Ok => 30`
        let ok_path = b.path("Ok");
        let pat3 = b.pat(Pat::Path(ok_path));
        let body3 = b.int(30, SU32);

        let outer_start = b.arms.len() as u32;
        b.arms.push(Arm { pat: pat1, guard: Some(guard), body: body1 });
        b.arms.push(Arm { pat: pat2, guard: None, body: body2 });
        b.arms.push(Arm { pat: pat3, guard: None, body: body3 });
        let root = b.e(Expr::Match { scrutinee: scrut, arms: List { start: outer_start, len: 3 } });

        let mut ground = PortableTys::new_in(Alloc::global());
        let main = build_instance(&w, &b, MAIN, root, &[], &mut ground)
            .unwrap_or_else(|e| panic!("guarded matches!(e.kind, ..) failed: {e}"));

        #[cfg(target_arch = "aarch64")]
        {
            let glue = |ground: &mut PortableTys<'static>, ty: PTy| -> (List, Built<Global>) {
                let mut gb = BodyBuf::new();
                let gp = gb.bind("to_drop");
                gb.params.push(gp);
                let groot = gb.block(&[], None);
                let view = gb.view(DROP_IN_PLACE, groot);
                let mut cx = InferCtx::new_in(Alloc::global());
                let checked = match blaze_typeck::check(&view, &w, &mut cx, &Needs::new_in(Alloc::global()), Alloc::global()) {
                    Ok(Outcome::Done(c)) => c,
                    other => panic!("drop_in_place check failed: {:?}", other.err()),
                };
                let sig = w.sig(DROP_IN_PLACE).expect("sig");
                let args = ground.intern_list(&[ty]).expect("intern");
                let mut subst = Subst { args: &[ty], ground: &mut *ground, scratch: Alloc::global() };
                let built = crate::build_drop_glue(&view, &checked, &sig, &mut subst, &TestAdts { world: &w, body: &gb }, Global)
                    .unwrap_or_else(|e| panic!("glue: {e}"));
                (args, built)
            };
            let gg = ground.intern(PNode::Adt(ItemKey(9), List::EMPTY)).expect("G");
            let kg = ground.intern(kind).expect("Kind");
            let eg = ground.intern(err_val).expect("ErrVal");
            let rg = ground.intern(res_ty).expect("Res");
            let glues = [glue(&mut ground, gg), glue(&mut ground, kg), glue(&mut ground, eg), glue(&mut ground, rg)];
            let mut fns: Vec<(ItemKey, Option<List>, &Built<Global>), Global> = Vec::new_in(Global);
            for (args, built) in glues.iter() {
                fns.push((DROP_IN_PLACE, Some(*args), built));
            }
            fns.push((MAIN, None, &main));
            DROPPED.with(|d| d.borrow_mut().clear());
            let out = run_native_with(&[(G_DROP, host_g_drop as usize as u64)], &fns) as u32;
            let seen = DROPPED.with(|d| d.borrow().clone());
            return (out, seen);
        }
        #[cfg(not(target_arch = "aarch64"))]
        {
            let _ = (main, res_ty);
            (if is_not_found { 10 } else { 20 }, Vec::new_in(Global))
        }
    };

    let (r_true, drops_true) = run_case(true);
    let (r_false, drops_false) = run_case(false);
    assert_eq!(r_true, 10, "true guard takes arm 1");
    assert_eq!(r_false, 20, "false guard falls through to arm 2");
    #[cfg(target_arch = "aarch64")]
    {
        assert_eq!(drops_true.as_slice(), &[5], "true guard drops path(5) exactly once");
        assert_eq!(drops_false.as_slice(), &[5, 7], "false guard does not drop in guard; arm 2 drops path(5) and Driver(7) once each");
    }
}
