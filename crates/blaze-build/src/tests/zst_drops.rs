//! Zero-sized values whose types need drop (walk.rs `Layout::placed`): whether a value needs
//! drop is a fact of its type, never of its size or of how it is held. Each test runs the built
//! code with host `Drop` fns that record the order they ran in.
//!
//! The Reference, "Destructors":
//! - `[destructors.intro]`: "When an initialized variable or temporary goes out of scope, its
//!   destructor is run or it is dropped."
//! - `[destructors.scope.intro]`: "When control flow leaves a drop scope all variables
//!   associated to that scope are dropped in reverse order of declaration (for variables) or
//!   creation (for temporaries)."
//! - `[destructors.operation]`: "The destructor of a type `T` consists of: 1. If `T: Drop`,
//!   calling `<T as core::ops::Drop>::drop` 2. Recursively running the destructor of all of its
//!   fields. .. The elements of an array or owned slice are dropped from the first element to
//!   the last."
//! Nothing there reads the type's size: `struct Z1;` has no bytes and has a destructor.

use super::*;

const Z1: ItemKey = ItemKey(91);
const Z2: ItemKey = ItemKey(92);
const Z3: ItemKey = ItemKey(93);
const Z1_DROP: ItemKey = ItemKey(61);
const Z2_DROP: ItemKey = ItemKey(62);
const Z3_DROP: ItemKey = ItemKey(63);
const TOUCH1: ItemKey = ItemKey(31);
const TOUCH2: ItemKey = ItemKey(32);

extern "C" fn host_z1_drop(_: *mut u8) {
    DROPPED.with(|d| d.borrow_mut().push(1));
}

extern "C" fn host_z2_drop(_: *mut u8) {
    DROPPED.with(|d| d.borrow_mut().push(2));
}

extern "C" fn host_z3_drop(_: *mut u8) {
    DROPPED.with(|d| d.borrow_mut().push(3));
}

/// `touch1(&Z1) -> i32` and `touch2(&Z2) -> i32` on the host: logs that the call ran.
extern "C" fn host_touch(_: *const u8) -> i32 {
    DROPPED.with(|d| d.borrow_mut().push(100));
    0
}

/// `struct Z1; struct Z2; struct Z3;`, each with a `Drop` impl, and the drop glue's lang fn.
fn z_world(w: &mut TestWorld) -> [PNode; 3] {
    w.strukt(91, "Z1", &[]);
    w.strukt(92, "Z2", &[]);
    w.strukt(93, "Z3", &[]);
    w.drops.push((Z1, Z1_DROP));
    w.drops.push((Z2, Z2_DROP));
    w.drops.push((Z3, Z3_DROP));
    let to = w.ty(T);
    w.generic_func(20, "drop_in_place", 1, &[PNode::Ptr(Mutbl::Mut, to)], UNIT);
    w.drop_in_place = Some(DROP_IN_PLACE);
    [PNode::Adt(Z1, List::EMPTY), PNode::Adt(Z2, List::EMPTY), PNode::Adt(Z3, List::EMPTY)]
}

/// `Name {}`.
fn z_lit(b: &mut BodyBuf, name: &str) -> ExprId {
    let path = b.path(name);
    b.e(Expr::Struct { path, fields: List::EMPTY, base: blaze_typeck::StructBase::None })
}

/// `drop_in_place::<t>`, built.
fn glue_of(w: &TestWorld, ground: &mut PortableTys<'_>, t: PNode) -> (List, Built<Global>) {
    let mut gb = BodyBuf::new();
    let p = gb.bind("to_drop");
    gb.params.push(p);
    let groot = gb.block(&[], None);
    let view = gb.view(DROP_IN_PLACE, groot);
    let mut cx = InferCtx::new_in(Alloc::global());
    let checked = match blaze_typeck::check(&view, w, &mut cx, &Needs::new_in(Alloc::global()), Alloc::global()) {
        Ok(Outcome::Done(c)) => c,
        other => panic!("the check did not finish: {:?}", other.err()),
    };
    let sig = w.sig(DROP_IN_PLACE).expect("a signature");
    let ty = ground.intern(t).expect("intern");
    let args = ground.intern_list(&[ty]).expect("intern");
    let mut subst = Subst { args: &[ty], ground: &mut *ground, scratch: Alloc::global() };
    let built = crate::build_drop_glue(&view, &checked, &sig, &mut subst, &TestAdts { world: w, body: &gb }, Global)
        .unwrap_or_else(|e| panic!("{e}"));
    (args, built)
}

const HOST: [(ItemKey, extern "C" fn(*mut u8)); 3] = [(Z1_DROP, host_z1_drop), (Z2_DROP, host_z2_drop), (Z3_DROP, host_z3_drop)];

fn host() -> Vec<(ItemKey, u64), Global> {
    let mut all = Vec::new_in(Global);
    all.extend(HOST.iter().map(|&(k, f)| (k, f as usize as u64)));
    all.push((TOUCH1, host_touch as usize as u64));
    all.push((TOUCH2, host_touch as usize as u64));
    all
}

/// Zero-sized TEMPORARIES, the three positions the ladder's x0-drop-11 printed "WRONG got
/// status 1" for (none was ever dropped: the walk asked "does it need drop" of how the value
/// was held, and a zero-sized value was held as nothing):
/// ```text
/// fn main() {
///     Z1 {};                             // an expression statement's value: dropped at its end
///     let _ = Z2 {};                     // `_` binds nothing: dropped at the statement's end
///     touch1(&Z1 {}) + touch2(&Z2 {});   // both calls run, then Z2, then Z1 (reverse of creation)
/// }
/// ```
/// Drops and calls, in order: 1, 2, 100, 100, 2, 1.
#[test]
fn zero_sized_temporaries_drop_at_their_statements_end() {
    let mut w = TestWorld::new();
    w.func(1, "main", &[], UNIT);
    let [z1, z2, _] = z_world(&mut w);
    let (r1, r2) = (w.ref_(Mutbl::Not, z1), w.ref_(Mutbl::Not, z2));
    w.func(31, "touch1", &[r1], I32);
    w.func(32, "touch2", &[r2], I32);
    let mut b = BodyBuf::new();
    let first = z_lit(&mut b, "Z1");
    let wild = b.pat(Pat::Wild);
    let second = z_lit(&mut b, "Z2");
    let (t1, t2) = (z_lit(&mut b, "Z1"), z_lit(&mut b, "Z2"));
    let (b1, b2) = (b.e(Expr::Ref { mutbl: Mutbl::Not, operand: t1 }), b.e(Expr::Ref { mutbl: Mutbl::Not, operand: t2 }));
    let (c1, c2) = (b.call("touch1", &[b1]), b.call("touch2", &[b2]));
    let sum = b.bin(BinOp::Add, c1, c2);
    let root = b.block(
        &[
            Stmt::Expr { expr: first, semi: true },
            Stmt::Let { pat: wild, ty: None, init: Some(second), else_: None },
            Stmt::Expr { expr: sum, semi: true },
        ],
        None,
    );
    let mut ground = PortableTys::new_in(Alloc::global());
    let main = build_instance(&w, &b, MAIN, root, &[], &mut ground).unwrap_or_else(|e| panic!("{e}"));
    let (a1, g1) = glue_of(&w, &mut ground, z1);
    let (a2, g2) = glue_of(&w, &mut ground, z2);
    DROPPED.with(|d| d.borrow_mut().clear());
    run_native_with(&host(), &[(DROP_IN_PLACE, Some(a1), &g1), (DROP_IN_PLACE, Some(a2), &g2), (MAIN, None, &main)]);
    let seen = DROPPED.with(|d| d.borrow().clone());
    assert_eq!(seen.as_slice(), &[1, 2, 100, 100, 2, 1], "{}", blaze_lir::print(main.func.view()));
}

/// Zero-sized LOCALS, a zero-sized FIELD, zero-sized ARRAY ELEMENTS and a MOVED zero-sized
/// local (each was refused by name before; a move must drop once, at the new owner):
/// ```text
/// struct W { n: i32, z: Z3 }
/// fn main() {
///     let a = Z1 {};
///     let w = W { n: 7, z: Z3 {} };
///     let c = Z2 {};
///     let d = c;                    // moved: dropped once, as `d`
///     let arr = [Z3 {}, Z3 {}];
/// }   // reverse order of declaration: arr (3, 3), d (2), w's z (3), a (1)
/// ```
#[test]
fn zero_sized_locals_fields_and_elements_drop_in_reverse_order() {
    let mut w = TestWorld::new();
    w.func(1, "main", &[], UNIT);
    let [z1, z2, z3] = z_world(&mut w);
    w.strukt(94, "W", &[("n", I32), ("z", z3)]);
    let wt = PNode::Adt(ItemKey(94), List::EMPTY);
    let mut b = BodyBuf::new();
    let (a, wv, c, d, arr) = (b.bind("a"), b.bind("w"), b.bind("c"), b.bind("d"), b.bind("arr"));
    let a_init = z_lit(&mut b, "Z1");
    let seven = b.int(7, None);
    let z = z_lit(&mut b, "Z3");
    let w_path = b.path("W");
    let start = b.field_inits.len() as u32;
    b.field_inits.push(FieldInit { name: Name::of("n"), expr: seven });
    b.field_inits.push(FieldInit { name: Name::of("z"), expr: z });
    let w_init = b.e(Expr::Struct { path: w_path, fields: List { start, len: 2 }, base: blaze_typeck::StructBase::None });
    let c_init = z_lit(&mut b, "Z2");
    let d_init = b.local(c);
    let (e0, e1) = (z_lit(&mut b, "Z3"), z_lit(&mut b, "Z3"));
    let arr_init = b.array(&[e0, e1]);
    let root = b.block(
        &[
            Stmt::Let { pat: a, ty: None, init: Some(a_init), else_: None },
            Stmt::Let { pat: wv, ty: None, init: Some(w_init), else_: None },
            Stmt::Let { pat: c, ty: None, init: Some(c_init), else_: None },
            Stmt::Let { pat: d, ty: None, init: Some(d_init), else_: None },
            Stmt::Let { pat: arr, ty: None, init: Some(arr_init), else_: None },
        ],
        None,
    );
    let mut ground = PortableTys::new_in(Alloc::global());
    let main = build_instance(&w, &b, MAIN, root, &[], &mut ground).unwrap_or_else(|e| panic!("{e}"));
    let (a1, g1) = glue_of(&w, &mut ground, z1);
    let (a2, g2) = glue_of(&w, &mut ground, z2);
    let (a3, g3) = glue_of(&w, &mut ground, z3);
    let (aw, gw) = glue_of(&w, &mut ground, wt);
    // `[Z3; 2]` as a node of the ground table (its element and length are that table's).
    let z3_ground = ground.intern(z3).expect("intern");
    let arr_t = PNode::Array(z3_ground, ground.usize_value(2).expect("a length interns"));
    let (aa, ga) = glue_of(&w, &mut ground, arr_t);
    DROPPED.with(|d| d.borrow_mut().clear());
    run_native_with(
        &host(),
        &[
            (DROP_IN_PLACE, Some(a1), &g1),
            (DROP_IN_PLACE, Some(a2), &g2),
            (DROP_IN_PLACE, Some(a3), &g3),
            (DROP_IN_PLACE, Some(aw), &gw),
            (DROP_IN_PLACE, Some(aa), &ga),
            (MAIN, None, &main),
        ],
    );
    let seen = DROPPED.with(|d| d.borrow().clone());
    assert_eq!(seen.as_slice(), &[3, 3, 2, 3, 1], "{}", blaze_lir::print(main.func.view()));
}
