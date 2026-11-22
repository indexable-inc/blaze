//! Associated types at an instance (normalize.rs): every type lowered at an instance is
//! grounded and then each projection normalized through `LayoutFacts::normalize`, here the
//! fixture solver `TestWorld::projs` (`<Self as Deref>::Target`).
//!
//! The Reference, "Associated Items", "Associated types": "Associated types are type aliases
//! associated with another type"; "Paths", "Qualified paths": `<T as Trait>::Name` names the
//! associated item of `T`'s implementation of `Trait`. At an instance `T` is concrete, so the
//! projection is that impl's definition.

use super::*;

const WRAP: ItemKey = ItemKey(10);
const MID: ItemKey = ItemKey(11);
const HOLDER: ItemKey = ItemKey(20);
const FIRST: ItemKey = ItemKey(3);
const U16: PNode = PNode::Uint(UintTy::U16);

/// `<of as Deref>::Target`, a node of `w`'s table.
fn target_of(w: &mut TestWorld, of: PNode) -> PNode {
    let t = w.ty(of);
    let args = w.types.intern_list(&[t]).expect("a list interns");
    PNode::Alias { trait_: DEREF, name: Name::of("Target"), args }
}

/// `fn <item><T>(..) where <each of `deref`>: Deref`.
fn deref_bounds(w: &mut TestWorld, item: ItemKey, deref: &[PNode]) {
    let mut clauses = Vec::new_in(Global);
    for &of in deref {
        let t = w.ty(of);
        let args = w.types.intern_list(&[t]).expect("a list interns");
        clauses.push(PClause::Trait { trait_: DEREF, args });
    }
    w.where_clauses.push((item, clauses));
}

/// `struct Wrap { n: u8 }`, `impl Deref for Wrap { type Target = <target>; }` (its `deref` fn
/// declared, never called), and the fixture solver's row for it.
fn wrap_world(target: PNode) -> TestWorld {
    let mut w = TestWorld::new();
    w.strukt(10, "Wrap", &[("n", U8)]);
    let wrap = PNode::Adt(WRAP, List::EMPTY);
    let r_wrap = w.ref_(Mutbl::Not, wrap);
    let r_target = w.ref_(Mutbl::Not, target);
    w.func(12, "wrap_deref", &[r_wrap], r_target);
    w.deref_impl(wrap, target, ItemKey(12));
    let (s, t) = (w.ty(wrap), w.ty(target));
    w.projs.push((s, t));
    w
}

/// ```text
/// struct Holder<T: Deref> { v: <T as Deref>::Target, k: T }
/// fn first<T: Deref>(w: T, x: <T as Deref>::Target) -> <T as Deref>::Target {
///     let h = Holder { v: x, k: w };   // `k` fixes Holder's `T`: a projection is not injective
///     h.v
/// }
/// fn main() -> u16 { first(Wrap { n: 1 }, 700u16) }
/// ```
/// At `T = Wrap` (`Target = u16`) the signature is `(Wrap, u16) -> u16`, the local `h` is a
/// `Holder` of a `u16` and a `u8` (4 bytes: "Type layout", "The `Rust` representation"
/// rounds the size up to the alignment), and the call returns 700.
#[test]
fn a_projection_in_a_signature_a_local_and_a_field_is_normalized() {
    let mut w = wrap_world(U16);
    let proj = target_of(&mut w, T);
    w.generic_func(3, "first", 1, &[T, proj], proj);
    deref_bounds(&mut w, FIRST, &[T]);
    w.generic_strukt(20, "Holder", 1, &[("v", proj), ("k", T)]);
    deref_bounds(&mut w, HOLDER, &[T]);
    w.func(1, "main", &[], U16);

    let mut b = BodyBuf::new();
    let (wp, x, h) = (b.bind("w"), b.bind("x"), b.bind("h"));
    b.params.extend_from_slice(&[wp, x]);
    let x_e = b.local(x);
    let one_ = b.local(wp);
    let path = b.path("Holder");
    let start = b.field_inits.len() as u32;
    b.field_inits.push(FieldInit { name: Name::of("v"), expr: x_e });
    b.field_inits.push(FieldInit { name: Name::of("k"), expr: one_ });
    let lit = b.e(Expr::Struct { path, fields: List { start, len: 2 }, base: blaze_typeck::StructBase::None });
    let h_e = b.local(h);
    let hv = b.e(Expr::Field { base: h_e, name: Name::of("v") });
    let root = b.block(&[Stmt::Let { pat: h, ty: None, init: Some(lit), else_: None }], Some(hv));

    let mut g = PortableTys::new_in(Alloc::global());
    let wrap_g = g.intern(PNode::Adt(WRAP, List::EMPTY)).expect("a type interns");
    let first = build_instance(&w, &b, FIRST, root, &[wrap_g], &mut g).unwrap_or_else(|e| panic!("{e}"));
    let largest = first.func.view().slots.iter().map(|s| s.size).max();
    assert_eq!(largest, Some(4), "Holder<Wrap> is a u16 and a u8\n{}", blaze_lir::print(first.func.view()));

    let mut m = BodyBuf::new();
    let path = m.path("Wrap");
    let n = m.int(1, Some(IntSuffix::Uint(UintTy::U8)));
    let start = m.field_inits.len() as u32;
    m.field_inits.push(FieldInit { name: Name::of("n"), expr: n });
    let wrap_lit = m.e(Expr::Struct { path, fields: List { start, len: 1 }, base: blaze_typeck::StructBase::None });
    let v = m.int(700, Some(IntSuffix::Uint(UintTy::U16)));
    let path = m.path("first");
    let callee = m.e(Expr::Path(path));
    let args = m.exprs_list(&[wrap_lit, v]);
    let root = m.e(Expr::Call { callee, args });
    let main = build_instance(&w, &m, MAIN, root, &[], &mut g).unwrap_or_else(|e| panic!("{e}"));
    let at = g.intern_list(&[wrap_g]).expect("a list interns");
    assert_eq!(main.refs.as_slice(), &[Instance { item: FIRST, args: at }]);
    #[cfg(target_arch = "aarch64")]
    assert_eq!(run_native_with(&[], &[(FIRST, Some(at), &first), (MAIN, None, &main)]), 700);
}

/// `fn nested<T>(x: <<T as Deref>::Target as Deref>::Target) -> <<T as Deref>::Target as
/// Deref>::Target { x }` at `T = Wrap`, with `Wrap: Deref<Target = Mid>` and `Mid: Deref<Target
/// = u8>`: the inner projection is answered first, then the outer one at `Mid`, giving `u8`.
#[test]
fn a_nested_projection_is_normalized_inside_out() {
    let mut w = wrap_world(PNode::Adt(MID, List::EMPTY));
    w.strukt(11, "Mid", &[("m", U16)]);
    let (mid, u8_) = (w.ty(PNode::Adt(MID, List::EMPTY)), w.ty(U8));
    w.projs.push((mid, u8_));
    let inner = target_of(&mut w, T);
    let outer = target_of(&mut w, inner);
    w.generic_func(3, "nested", 1, &[outer], outer);
    deref_bounds(&mut w, FIRST, &[T, inner]);
    let mut b = BodyBuf::new();
    let x = b.bind("x");
    b.params.push(x);
    let x_e = b.local(x);
    let root = b.block(&[], Some(x_e));
    let mut g = PortableTys::new_in(Alloc::global());
    let wrap_g = g.intern(PNode::Adt(WRAP, List::EMPTY)).expect("a type interns");
    let built = build_instance(&w, &b, FIRST, root, &[wrap_g], &mut g).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(one(&built, &[Val::Int(201)]), 201);
    let u8_g = g.intern(U8).expect("a type interns");
    let s = w.ty(outer);
    assert_eq!(crate::ground(w.types.view(), s, &[wrap_g], &mut g, Alloc::global(), &w).expect("normalizes"), u8_g);
}

/// The same `nested` at `T = u32`, which has no `Deref` impl: refused naming the projection.
#[test]
fn a_projection_the_solver_does_not_answer_is_refused_by_name() {
    let mut w = wrap_world(U8);
    let proj = target_of(&mut w, T);
    w.generic_func(3, "nested", 1, &[proj], proj);
    deref_bounds(&mut w, FIRST, &[T]);
    let mut b = BodyBuf::new();
    let x = b.bind("x");
    b.params.push(x);
    let x_e = b.local(x);
    let root = b.block(&[], Some(x_e));
    let mut g = PortableTys::new_in(Alloc::global());
    let u32_g = g.intern(U32).expect("a type interns");
    let error = build_instance(&w, &b, FIRST, root, &[u32_g], &mut g).err().expect("refused");
    assert!(error.contains("of trait 0x5a at this instance was not normalized"), "{error}");
}

/// Two programs that differ only in what `<Wrap as Deref>::Target` is (`u8`, `u16`): the
/// instance `first::<Wrap>` has equal arguments in both, so `instance_key` alone is equal;
/// `instance_key_normalized` folds the normalized projection in, and the keys differ. A fn with
/// no projection keys as `instance_key` keys it.
#[test]
fn an_instance_key_covers_what_its_projections_normalize_to() {
    let code = blaze_ids::ContentId::of("test code", b"fn first");
    let key = |target: PNode| {
        let mut w = wrap_world(target);
        let proj = target_of(&mut w, T);
        let _ = w.ty(proj);
        let mut g = PortableTys::new_in(Alloc::global());
        let wrap_g = g.intern(PNode::Adt(WRAP, List::EMPTY)).expect("a type interns");
        let args = g.intern_list(&[wrap_g]).expect("a list interns");
        let plain = crate::instance_key(code, args, g.view(), Alloc::global()).expect("a key");
        let normalized =
            crate::instance_key_normalized(code, args, w.types.view(), &mut g, &w, Alloc::global()).expect("a key");
        let none = crate::instance_key_normalized(code, args, PTys::EMPTY, &mut g, &w, Alloc::global()).expect("a key");
        assert_eq!(none, plain, "no projection: the plain key");
        (plain, normalized)
    };
    let (plain8, norm8) = key(U8);
    let (plain16, norm16) = key(U16);
    assert_eq!(plain8, plain16, "the arguments alone do not tell the programs apart");
    assert_ne!(norm8, norm16, "the normalized projection is in the key");
    assert_eq!(key(U8).1, norm8, "deterministic");
}
