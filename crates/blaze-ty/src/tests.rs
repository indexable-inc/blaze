//! Properties of the arena, the tables, relating, folding, canonicalization and export, each
//! against a slow model over plain trees, and a planted-bug control for snapshots (rule 53):
//! an int table whose writes are not logged, which the rollback oracle must catch.

use std::{format, string::String, vec, vec::Vec};

use blaze_id::{ContentId, ItemKey};
use blaze_sim::{Broke, Config, Tape};

use crate::effect_ty::{
    EffectPolyError, EffectRow, EffectRowSet, EffectSet, EffectVar, LoweringMode, MAX_ROW_VARS, check_dyn_fn_effects,
    check_higher_order_call, specialize_call,
};
use crate::infer::Plant;
use crate::{
    AllInvariant, CanonBuf, DefaultNames, IntTy, Mode, Mutability, Node, Out, PTys, Region, Ty, TyCx, Tys, UintTy,
    VarKind, decimal, print,
};

/// A model type: a plain tree over a vector.
#[derive(Clone, Debug, PartialEq, Eq)]
enum M {
    Bool,
    I32,
    U8,
    Param(u32),
    Var(usize),
    Tuple(Vec<usize>),
    Ref(usize),
    Slice(usize),
    Adt(u8, Vec<usize>),
}

#[derive(Default)]
struct Model {
    nodes: Vec<M>,
}

impl Model {
    fn push(&mut self, m: M) -> usize {
        self.nodes.push(m);
        self.nodes.len() - 1
    }

    /// A random term of depth at most `depth`; `vars` variables and `params` parameters may
    /// appear.
    fn draw(&mut self, tape: &Tape, depth: u32, vars: usize, params: u32) -> usize {
        let leaf = depth == 0 || tape.one_in(3, "leaf");
        if leaf {
            let n = tape.index(3 + usize::from(vars > 0) + usize::from(params > 0), "leaf kind");
            let m = match n {
                0 => M::Bool,
                1 => M::I32,
                2 => M::U8,
                3 if vars > 0 => M::Var(tape.index(vars, "var")),
                _ => M::Param(tape.int_in(0..=params.max(1) - 1, "param")),
            };
            return self.push(m);
        }
        match tape.index(4, "node kind") {
            0 => {
                let len = tape.index(4, "tuple len");
                let kids = (0..len).map(|_| self.draw(tape, depth - 1, vars, params)).collect();
                self.push(M::Tuple(kids))
            }
            1 => {
                let k = self.draw(tape, depth - 1, vars, params);
                self.push(M::Ref(k))
            }
            2 => {
                let k = self.draw(tape, depth - 1, vars, params);
                self.push(M::Slice(k))
            }
            _ => {
                let d = tape.int_in(0u8..=1, "adt");
                let kids = (0..usize::from(d) + 1).map(|_| self.draw(tape, depth - 1, vars, params)).collect();
                self.push(M::Adt(d, kids))
            }
        }
    }

    /// Structural equality of two model terms (variables by number).
    fn eq(&self, a: usize, b: usize) -> bool {
        match (&self.nodes[a], &self.nodes[b]) {
            (M::Tuple(x), M::Tuple(y)) | (M::Adt(_, x), M::Adt(_, y)) => {
                let same_head = match (&self.nodes[a], &self.nodes[b]) {
                    (M::Adt(d, _), M::Adt(e, _)) => d == e,
                    _ => true,
                };
                same_head && x.len() == y.len() && x.iter().zip(y).all(|(p, q)| self.eq(*p, *q))
            }
            (M::Ref(x), M::Ref(y)) | (M::Slice(x), M::Slice(y)) => self.eq(*x, *y),
            (x, y) => x == y,
        }
    }

    /// The term with its variables renamed in pre-order first-visit order.
    fn renamed(&self, a: usize, names: &mut Vec<usize>, out: &mut String) {
        match &self.nodes[a] {
            M::Var(v) => {
                let i = match names.iter().position(|x| x == v) {
                    Some(i) => i,
                    None => {
                        names.push(*v);
                        names.len() - 1
                    }
                };
                out.push_str(&format!("v{i} "));
            }
            M::Tuple(k) | M::Adt(_, k) => {
                out.push_str(&format!("{:?}[", core::mem::discriminant(&self.nodes[a])));
                if let M::Adt(d, _) = &self.nodes[a] {
                    out.push_str(&format!("{d}"));
                }
                for c in k {
                    self.renamed(*c, names, out);
                }
                out.push_str("] ");
            }
            M::Ref(k) => {
                out.push_str("& ");
                self.renamed(*k, names, out);
            }
            M::Slice(k) => {
                out.push_str("[] ");
                self.renamed(*k, names, out);
            }
            m => out.push_str(&format!("{m:?} ")),
        }
    }
}

fn key(d: u8) -> ItemKey {
    ItemKey(ContentId([d.wrapping_add(1); 32]))
}

/// Builds a model term in an arena; `vars[i]` is the arena type of variable `i`, `params`
/// stand for parameters if given.
fn build(tys: &mut Tys<'_>, m: &Model, at: usize, vars: &[Ty], params: Option<&[Ty]>) -> Ty {
    match &m.nodes[at] {
        M::Bool => Ty::BOOL,
        M::I32 => Ty::int(IntTy::I32),
        M::U8 => Ty::uint(UintTy::U8),
        M::Param(i) => match params {
            Some(p) => p[usize::try_from(*i).unwrap_or(0)],
            None => tys.param(*i),
        },
        M::Var(v) => vars[*v],
        M::Tuple(k) => {
            let kids: Vec<Ty> = k.iter().map(|c| build(tys, m, *c, vars, params)).collect();
            tys.tuple_of(&kids)
        }
        M::Ref(k) => {
            let t = build(tys, m, *k, vars, params);
            tys.reference(Region::STATIC, t, Mutability::Not)
        }
        M::Slice(k) => {
            let t = build(tys, m, *k, vars, params);
            tys.slice(t)
        }
        M::Adt(d, k) => {
            let kids: Vec<Ty> = k.iter().map(|c| build(tys, m, *c, vars, params)).collect();
            let l = tys.mk_ty_list(&kids);
            let def = tys.def(key(*d));
            tys.adt(def, l)
        }
    }
}

fn oracle(holds: bool, why: impl FnOnce() -> String) -> Result<(), Broke> {
    if holds { Ok(()) } else { Err(Broke::Oracle(why())) }
}

#[test]
fn interning_is_structural() {
    blaze_sim::check(Config::cases(400), |tape| {
        let mut m = Model::default();
        let a = m.draw(tape, 4, 0, 3);
        let b = if tape.coin("same") { a } else { m.draw(tape, 4, 0, 3) };
        let mut tys = Tys::new_in(blaze_own::testing::heap().alloc());
        let ta = build(&mut tys, &m, a, &[], None);
        let tb = build(&mut tys, &m, b, &[], None);
        let again = build(&mut tys, &m, a, &[], None);
        oracle(ta == again, || format!("one term interned twice gave {ta:?} and {again:?}"))?;
        oracle((ta == tb) == m.eq(a, b), || format!("ids equal: {}, model equal: {}", ta == tb, m.eq(a, b)))
    });
}

/// Robinson unification over the model: the slow reference.
fn model_unify(m: &Model, a: usize, b: usize, sub: &mut Vec<Option<usize>>) -> bool {
    let mut work = vec![(a, b)];
    while let Some((x, y)) = work.pop() {
        let x = walk(m, x, sub);
        let y = walk(m, y, sub);
        match (&m.nodes[x], &m.nodes[y]) {
            (M::Var(v), M::Var(w)) if v == w => {}
            (M::Var(v), _) => {
                if occurs(m, *v, y, sub) {
                    return false;
                }
                sub[*v] = Some(y);
            }
            (_, M::Var(w)) => {
                if occurs(m, *w, x, sub) {
                    return false;
                }
                sub[*w] = Some(x);
            }
            (M::Tuple(p), M::Tuple(q)) => {
                if p.len() != q.len() {
                    return false;
                }
                work.extend(p.iter().copied().zip(q.iter().copied()));
            }
            (M::Adt(d, p), M::Adt(e, q)) => {
                if d != e || p.len() != q.len() {
                    return false;
                }
                work.extend(p.iter().copied().zip(q.iter().copied()));
            }
            (M::Ref(p), M::Ref(q)) | (M::Slice(p), M::Slice(q)) => work.push((*p, *q)),
            (p, q) => {
                if p != q {
                    return false;
                }
            }
        }
    }
    true
}

fn walk(m: &Model, mut x: usize, sub: &[Option<usize>]) -> usize {
    while let M::Var(v) = m.nodes[x] {
        match sub[v] {
            Some(y) => x = y,
            None => break,
        }
    }
    x
}

fn occurs(m: &Model, v: usize, t: usize, sub: &[Option<usize>]) -> bool {
    let mut work = vec![t];
    while let Some(x) = work.pop() {
        let x = walk(m, x, sub);
        match &m.nodes[x] {
            M::Var(w) if *w == v => return true,
            M::Tuple(k) | M::Adt(_, k) => work.extend(k.iter().copied()),
            M::Ref(k) | M::Slice(k) => work.push(*k),
            _ => {}
        }
    }
    false
}

#[test]
fn unify_matches_robinson() {
    blaze_sim::check(Config::cases(600), |tape| -> Result<(), Broke> {
        let nvars = 1 + tape.index(4, "vars");
        let mut m = Model::default();
        let a = m.draw(tape, 3, nvars, 0);
        let b = m.draw(tape, 3, nvars, 0);
        let mut sub = vec![None; nvars];
        let model_ok = model_unify(&m, a, b, &mut sub);

        let mut cx = TyCx::new_in(blaze_own::testing::heap().alloc());
        let vars: Vec<Ty> = (0..nvars).map(|_| cx.next_ty_var()).collect();
        let ta = build(&mut cx.tys, &m, a, &vars, None);
        let tb = build(&mut cx.tys, &m, b, &vars, None);
        let got = cx.eq(ta.arg(), tb.arg(), &mut AllInvariant);
        oracle(got.is_ok() == model_ok, || format!("blaze: {got:?}, model unifies: {model_ok}"))?;
        if got.is_ok() {
            let (ra, rb) = (cx.resolve_ty(ta), cx.resolve_ty(tb));
            oracle(ra == rb, || format!("after unifying, the sides resolve to {ra:?} and {rb:?}"))?;
            oracle(cx.goals().is_empty(), || "equating plain types made goals".into())?;
        } else {
            // A failed relation leaves nothing bound.
            for (i, v) in vars.iter().enumerate() {
                let r = cx.resolve_ty(*v);
                oracle(r == *v, || format!("a failed relation left variable {i} bound to {r:?}"))?;
            }
        }
        Ok(())
    });
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SnapPlant {
    Clean,
    ForgetIntUndo,
}

fn snapshots(tape: &Tape, p: SnapPlant) -> Result<(), Broke> {
    let mut cx = TyCx::new_in(blaze_own::testing::heap().alloc());
    cx.vars.plant = match p {
        SnapPlant::Clean => Plant::Clean,
        SnapPlant::ForgetIntUndo => Plant::ForgetIntUndo,
    };
    let n = 1 + tape.index(5, "vars");
    let mut vars: Vec<Ty> = Vec::new();
    for _ in 0..n {
        vars.push(match tape.index(3, "var kind") {
            0 => cx.next_ty_var(),
            1 => cx.next_int_var(),
            _ => cx.next_float_var(),
        });
    }
    // Some bindings before the snapshot, kept.
    let before_ops = tape.index(3, "ops before");
    let mut kept = 0usize;
    for _ in 0..before_ops {
        kept += usize::from(random_op(tape, &mut cx, &vars));
    }
    let before: Vec<Ty> = vars.iter().map(|v| cx.resolve_ty(*v)).collect();
    let s = cx.snapshot();
    let ops = 1 + tape.index(6, "ops");
    for _ in 0..ops {
        if tape.one_in(4, "nested") {
            let inner = cx.snapshot();
            random_op(tape, &mut cx, &vars);
            if tape.coin("commit inner") {
                cx.commit(inner);
            } else {
                cx.rollback_to(inner);
            }
        } else {
            random_op(tape, &mut cx, &vars);
        }
    }
    cx.rollback_to(s);
    for (i, v) in vars.iter().enumerate() {
        let r = cx.resolve_ty(*v);
        if r != before[i] {
            return Err(Broke::Oracle(format!(
                "after the rollback, variable {i} resolves to {r:?}, before it was {:?} ({kept} kept ops)",
                before[i]
            )));
        }
    }
    Ok(())
}

/// One random binding or union; whether it bound anything.
fn random_op(tape: &Tape, cx: &mut TyCx<'_>, vars: &[Ty]) -> bool {
    let a = vars[tape.index(vars.len(), "a")];
    let b = match tape.index(4, "b kind") {
        0 => vars[tape.index(vars.len(), "b var")],
        1 => Ty::int(IntTy::I32),
        2 => Ty::uint(UintTy::U8),
        _ => Ty::BOOL,
    };
    matches!(cx.eq(a.arg(), b.arg(), &mut AllInvariant), Ok(true))
}

#[test]
fn snapshots_roll_back_every_table() {
    blaze_sim::check(Config::cases(500), |tape| snapshots(tape, SnapPlant::Clean));
}

#[test]
fn snapshot_control_catches_a_forgotten_int_undo() {
    blaze_sim::control(Config::cases(500), SnapPlant::Clean, SnapPlant::ForgetIntUndo, "after the rollback", snapshots);
}

#[test]
fn substitution_matches_the_model() {
    blaze_sim::check(Config::cases(400), |tape| {
        let mut m = Model::default();
        let t = m.draw(tape, 4, 0, 3);
        let args: Vec<usize> = (0..3).map(|_| m.draw(tape, 2, 0, 0)).collect();
        let mut cx = TyCx::new_in(blaze_own::testing::heap().alloc());
        let arg_tys: Vec<Ty> = args.iter().map(|a| build(&mut cx.tys, &m, *a, &[], None)).collect();
        let list = cx.tys.mk_ty_list(&arg_tys);
        let term = build(&mut cx.tys, &m, t, &[], None);
        let got = cx.subst_ty(term, list).map_err(|e| Broke::Oracle(format!("substitution refused: {e}")))?;
        let want = build(&mut cx.tys, &m, t, &[], Some(&arg_tys));
        oracle(got == want, || format!("substituted {got:?}, model says {want:?}"))?;
        // Too few arguments is a typed refusal, never a quiet keep.
        let short = cx.tys.mk_ty_list(&arg_tys[..1]);
        let has_high_param = m.nodes.iter().enumerate().any(|(i, n)| matches!(n, M::Param(p) if *p > 0) && reaches(&m, t, i));
        let r = cx.subst_ty(term, short);
        oracle(r.is_err() == has_high_param, || format!("substituting one argument: {r:?}, a parameter past it: {has_high_param}"))
    });
}

fn reaches(m: &Model, from: usize, to: usize) -> bool {
    let mut work = vec![from];
    while let Some(x) = work.pop() {
        if x == to {
            return true;
        }
        match &m.nodes[x] {
            M::Tuple(k) | M::Adt(_, k) => work.extend(k.iter().copied()),
            M::Ref(k) | M::Slice(k) => work.push(*k),
            _ => {}
        }
    }
    false
}

/// Canonicalizes a model term in a fresh context whose variables are made in the order
/// `order` (with `junk` unrelated variables first), returning the key.
fn canon_key(m: &Model, t: usize, nvars: usize, order: &[usize], junk: usize) -> Vec<u32> {
    let mut cx = TyCx::new_in(blaze_own::testing::heap().alloc());
    for _ in 0..junk {
        cx.next_ty_var();
    }
    let mut vars = vec![Ty::BOOL; nvars];
    for &i in order {
        vars[i] = cx.next_ty_var();
    }
    let term = build(&mut cx.tys, m, t, &vars, None);
    let mut buf = CanonBuf::new_in(blaze_own::testing::heap().alloc());
    let c = cx.canonicalize(term.node(), Mode::Input { param_env: false }, &mut buf);
    let Ok(c) = c else { panic!("canonicalizing a plain term refused: {c:?}") };
    let key: Vec<u32> = buf.key.iter().copied().collect();
    // The one walk wrote the term's words from the canonical nodes as it made them: they are
    // the words of the finished canonical term written alone (canonicalizing it changes
    // nothing, so its walk only writes).
    let mut again = CanonBuf::new_in(blaze_own::testing::heap().alloc());
    let c2 = cx.canonicalize(c.value, Mode::Input { param_env: false }, &mut again);
    assert!(matches!(c2, Ok(x) if x.value == c.value && x.vars == 0), "a canonical term canonicalized changed: {c2:?}");
    let words = key.len() - 2 * buf.vars.len() - 2;
    assert_eq!(key[..words], again.key[..again.key.len() - 2], "the one walk's key is not the canonical term's");
    key
}

#[test]
fn canonical_keys_are_equal_exactly_up_to_renaming() {
    blaze_sim::check(Config::cases(400), |tape| {
        let nvars = 1 + tape.index(4, "vars");
        let mut m = Model::default();
        let a = m.draw(tape, 3, nvars, 2);
        let b = if tape.coin("rename") {
            // `a` with its variables permuted.
            let perm: Vec<usize> = {
                let mut p: Vec<usize> = (0..nvars).collect();
                tape.shuffle(&mut p, "perm");
                p
            };
            rename(&mut m, a, &perm)
        } else {
            m.draw(tape, 3, nvars, 2)
        };
        let mut order: Vec<usize> = (0..nvars).collect();
        tape.shuffle(&mut order, "order");
        let ka = canon_key(&m, a, nvars, &(0..nvars).collect::<Vec<_>>(), 0);
        let kb = canon_key(&m, b, nvars, &order, tape.index(3, "junk"));
        let (mut sa, mut sb) = (String::new(), String::new());
        m.renamed(a, &mut Vec::new(), &mut sa);
        m.renamed(b, &mut Vec::new(), &mut sb);
        let same = sa == sb;
        oracle((ka == kb) == same, || format!("keys equal: {}, equal up to renaming: {same} ({sa} / {sb})", ka == kb))
    });
}

fn rename(m: &mut Model, t: usize, perm: &[usize]) -> usize {
    let n = m.nodes[t].clone();
    let r = match n {
        M::Var(v) => M::Var(perm[v]),
        M::Tuple(k) => M::Tuple(k.iter().map(|c| rename(m, *c, perm)).collect()),
        M::Adt(d, k) => M::Adt(d, k.iter().map(|c| rename(m, *c, perm)).collect()),
        M::Ref(k) => M::Ref(rename(m, k, perm)),
        M::Slice(k) => M::Slice(rename(m, k, perm)),
        other => other,
    };
    m.push(r)
}

fn export_bytes(tys: &Tys<'_>, n: Node) -> Vec<u8> {
    let mut p = PTys::new_in(blaze_own::testing::heap().alloc());
    p.export(tys, n);
    let mut out = blaze_own::Vec::new_in(blaze_own::testing::heap().alloc());
    p.to_bytes(&mut out);
    out.iter().copied().collect()
}

#[test]
fn export_import_round_trips() {
    blaze_sim::check(Config::cases(400), |tape| {
        let mut m = Model::default();
        let t = m.draw(tape, 4, 0, 3);
        let mut a = Tys::new_in(blaze_own::testing::heap().alloc());
        let ta = build(&mut a, &m, t, &[], None);
        // Another arena, with unrelated rows made first: other ids, the same bytes.
        let mut b = Tys::new_in(blaze_own::testing::heap().alloc());
        let junk = m.draw(tape, 3, 0, 3);
        build(&mut b, &m, junk, &[], None);
        let tb = build(&mut b, &m, t, &[], None);
        let (ba, bb) = (export_bytes(&a, ta.node()), export_bytes(&b, tb.node()));
        oracle(ba == bb, || "one type exported from two arenas gave different bytes".into())?;

        let p = PTys::from_bytes(&ba, blaze_own::testing::heap().alloc()).map_err(|e| Broke::Oracle(format!("re-reading refused: {e}")))?;
        let mut c = Tys::new_in(blaze_own::testing::heap().alloc());
        let mut map = blaze_own::Vec::new_in(blaze_own::testing::heap().alloc());
        let mut hole = |_: &mut Tys<'_>, _: VarKind| -> Node { Ty::ERROR.node() };
        p.import(&mut c, &mut hole, &mut map).map_err(|e| Broke::Oracle(format!("import refused: {e}")))?;
        let last = crate::PTy(u32::try_from(p.len().saturating_sub(1)).unwrap_or(0));
        let n = PTys::node_of(&map, last).map_err(|e| Broke::Oracle(format!("{e}")))?;
        let bc = export_bytes(&c, n);
        oracle(bc == ba, || "export, import, export changed the bytes".into())?;
        // The imported type is the one built directly.
        let direct = build(&mut c, &m, t, &[], None);
        oracle(direct.node() == n, || "the imported type is not the type built in that arena".into())
    });
}

#[test]
fn malformed_bytes_are_refused() {
    blaze_sim::check(Config::cases(300), |tape| -> Result<(), Broke> {
        let mut m = Model::default();
        let t = m.draw(tape, 3, 0, 2);
        let mut a = Tys::new_in(blaze_own::testing::heap().alloc());
        let ta = build(&mut a, &m, t, &[], None);
        let mut bytes = export_bytes(&a, ta.node());
        let i = tape.index(bytes.len(), "byte");
        bytes[i] ^= 1 << tape.index(8, "bit");
        // Reading may refuse or accept; an accepted table either imports or refuses, never
        // panics.
        if let Ok(p) = PTys::from_bytes(&bytes, blaze_own::testing::heap().alloc()) {
            let mut c = Tys::new_in(blaze_own::testing::heap().alloc());
            let mut map = blaze_own::Vec::new_in(blaze_own::testing::heap().alloc());
            let mut hole = |_: &mut Tys<'_>, _: VarKind| -> Node { Ty::ERROR.node() };
            let _ = p.import(&mut c, &mut hole, &mut map);
        }
        Ok(())
    });
}

struct Count(usize);

impl Out for Count {
    fn str(&mut self, s: &str) {
        self.0 += s.len();
    }
}

struct Text(String);

impl Out for Text {
    fn str(&mut self, s: &str) {
        self.0.push_str(s);
    }
}

#[test]
fn prints_rust_forms() {
    let mut cx = TyCx::new_in(blaze_own::testing::heap().alloc());
    let i32_ = Ty::int(IntTy::I32);
    let r = cx.tys.reference(Region::STATIC, i32_, Mutability::Mut);
    let s = cx.tys.slice(r);
    let t = cx.tys.tuple_of(&[s, Ty::BOOL]);
    let one = cx.tys.tuple_of(&[Ty::CHAR]);
    let pair = cx.tys.tuple_of(&[t, one]);
    let mut out = Text(String::new());
    print(&cx.tys, pair.node(), &mut DefaultNames, &mut out);
    assert_eq!(out.0, "(([&'static mut i32], bool), (char,))");
    let mut d = Text(String::new());
    decimal(u128::MAX, &mut d);
    assert_eq!(d.0, "340282366920938463463374607431768211455");
}

/// A non-rigid alias relates through a projection goal on a fresh variable; structurally, it
/// is a constructor and nothing is deferred.
#[test]
fn aliases_defer_unless_structural() {
    use crate::{AliasKind, PredKind, TyKind};
    let mut cx = TyCx::new_in(blaze_own::testing::heap().alloc());
    let item = cx.tys.def(key(7));
    let v = cx.next_ty_var();
    let args = cx.tys.mk_ty_list(&[v]);
    let alias = cx.tys.alias(AliasKind::Projection, item, args);
    let i32_ = Ty::int(IntTy::I32);
    assert_eq!(cx.eq(alias.arg(), i32_.arg(), &mut AllInvariant), Ok(true));
    let goals: Vec<_> = cx.goals().to_vec();
    assert_eq!(goals.len(), 1, "one projection goal");
    match cx.tys.pred_kind(goals[0]) {
        PredKind::Projection(a, t) => {
            assert_eq!(a, alias.arg());
            let t = t.ty().map(|t| cx.resolve_ty(t));
            assert_eq!(t, Some(i32_), "the goal's fresh variable is related with the other side");
        }
        other => panic!("expected a projection goal, got {other:?}"),
    }
    assert_eq!(cx.resolve_ty(v), v, "the alias's argument is not touched");

    // Structurally: the arguments relate, no goal is made.
    let mut cx = TyCx::new_in(blaze_own::testing::heap().alloc());
    let item = cx.tys.def(key(7));
    let v = cx.next_ty_var();
    let with_var = cx.tys.mk_ty_list(&[v]);
    let with_bool = cx.tys.mk_ty_list(&[Ty::BOOL]);
    let a = cx.tys.alias(AliasKind::Projection, item, with_var);
    let b = cx.tys.alias(AliasKind::Projection, item, with_bool);
    assert_eq!(cx.unify_structurally(a.arg(), b.arg(), &mut AllInvariant), Ok(true));
    assert_eq!(cx.resolve_ty(v), Ty::BOOL);
    assert!(cx.goals().is_empty());
    assert!(cx.unify_structurally(b.arg(), Ty::int(IntTy::I32).arg(), &mut AllInvariant).is_err());
    // A variable against an alias, structurally, takes the alias as it is.
    let w = cx.next_ty_var();
    assert_eq!(cx.unify_structurally(w.arg(), b.arg(), &mut AllInvariant), Ok(true));
    assert_eq!(cx.resolve_ty(w), b);
    assert!(matches!(cx.tys.ty_kind(b), TyKind::Alias { rigid: false, .. }));
    assert!(cx.goals().is_empty());
}

/// A million nested references: every walk runs on its own stack, so none grows the thread's.
#[test]
fn a_shared_dag_is_walked_once_per_node() {
    // `T(k) = (T(k-1), T(k-1))`: 64 levels, 2^64 paths and 129 distinct nodes. Substitution,
    // canonicalization and resolution must each visit a node a bounded number of times: a
    // fold that lost the memo, or a deferred intern that kept a sibling's result from it,
    // would not finish.
    const LEVELS: usize = 64;
    let mut cx = TyCx::new_in(blaze_own::testing::heap().alloc());
    let v = cx.next_ty_var();
    let (mut with_param, mut with_var, mut plain) = (cx.tys.param(0), v, Ty::BOOL);
    for _ in 0..LEVELS {
        with_param = cx.tys.tuple_of(&[with_param, with_param]);
        with_var = cx.tys.tuple_of(&[with_var, with_var]);
        plain = cx.tys.tuple_of(&[plain, plain]);
    }
    let args = cx.tys.mk_ty_list(&[Ty::BOOL]);
    assert_eq!(cx.subst_ty(with_param, args), Ok(plain));
    let mut buf = CanonBuf::new_in(blaze_own::testing::heap().alloc());
    let c = cx.canonicalize(with_var.node(), Mode::Input { param_env: false }, &mut buf);
    assert!(matches!(c, Ok(c) if c.vars == 1), "{c:?}");
    assert_eq!(cx.eq(v.arg(), Ty::BOOL.arg(), &mut AllInvariant), Ok(true));
    assert_eq!(cx.resolve_ty(with_var), plain);
}

#[test]
fn a_million_deep_chain_walks_without_stack_growth() {
    const DEPTH: usize = 1_000_000;
    let mut cx = TyCx::new_in(blaze_own::testing::heap().alloc());
    let p = cx.tys.param(0);
    let v = cx.next_ty_var();
    let (mut with_param, mut with_var, mut plain) = (p, v, Ty::BOOL);
    for _ in 0..DEPTH {
        with_param = cx.tys.reference(Region::STATIC, with_param, Mutability::Not);
        with_var = cx.tys.reference(Region::STATIC, with_var, Mutability::Not);
        plain = cx.tys.reference(Region::STATIC, plain, Mutability::Not);
    }
    let args = cx.tys.mk_ty_list(&[Ty::BOOL]);
    let substituted = cx.subst_ty(with_param, args);
    assert_eq!(substituted, Ok(plain), "substituting at the bottom of the chain");
    let bound = cx.eq(with_var.arg(), plain.arg(), &mut AllInvariant);
    assert_eq!(bound, Ok(true), "equating the chains binds the variable at the bottom");
    assert_eq!(cx.resolve_ty(with_var), plain);
    // The occurs check, a million levels down.
    let w = cx.next_ty_var();
    let mut deep_w = w;
    for _ in 0..DEPTH {
        deep_w = cx.tys.reference(Region::STATIC, deep_w, Mutability::Not);
    }
    assert!(cx.eq(w.arg(), deep_w.arg(), &mut AllInvariant).is_err(), "a variable equated with a type containing it");
    let mut buf = CanonBuf::new_in(blaze_own::testing::heap().alloc());
    let c = cx.canonicalize(deep_w.node(), Mode::Input { param_env: false }, &mut buf);
    assert!(c.is_ok());
    let bytes = export_bytes(&cx.tys, plain.node());
    let pt = PTys::from_bytes(&bytes, blaze_own::testing::heap().alloc());
    assert!(pt.is_ok());
    let mut count = Count(0);
    print(&cx.tys, plain.node(), &mut DefaultNames, &mut count);
    assert_eq!(count.0, DEPTH * "&'static ".len() + "bool".len());
}

// ---- what the solver needs (lib.rs, "What blaze-solve reads") ------------------------------

/// Every node of an arena built from random terms comes back from its bits; bits past a
/// class's rows do not; a node of one class is not a handle of another.
#[test]
fn node_at_round_trips_every_node_and_refuses_the_rest() {
    blaze_sim::check(Config::cases(300), |tape| {
        let mut m = Model::default();
        let mut cx = TyCx::new_in(blaze_own::testing::heap().alloc());
        let vars: Vec<Ty> = (0..3).map(|_| cx.next_ty_var()).collect();
        let n = 1 + tape.index(6, "terms");
        for _ in 0..n {
            let at = m.draw(tape, 3, vars.len(), 2);
            build(&mut cx.tys, &m, at, &vars, None);
        }
        let _ = cx.tys.const_value(Ty::uint(UintTy::Usize), u128::from(tape.index(9, "len") as u32));
        let classes = [
            (crate::ids::TAG_TY, cx.tys.ty.len()),
            (crate::ids::TAG_REGION, cx.tys.re.len()),
            (crate::ids::TAG_CONST, cx.tys.ct.len()),
            (crate::ids::TAG_LIST, cx.tys.list_start.len()),
        ];
        for (tag, rows) in classes {
            for i in 0..rows {
                let bits = (u32::try_from(i).unwrap_or(u32::MAX) << 2) | tag;
                let got = cx.tys.node_at(bits);
                oracle(got.map(Node::bits) == Some(bits), || format!("node_at({bits:#x}) of row {i} gave {got:?}"))?;
                let Some(node) = got else { continue };
                let one_class = [node.as_ty().is_some(), node.as_region().is_some(), node.as_const().is_some(), node.as_list().is_some()];
                oracle(one_class.iter().filter(|x| **x).count() == 1, || format!("{node:?} is a handle of {one_class:?}"))?;
                if let Some(t) = node.as_ty() {
                    oracle(t.node() == node, || format!("{node:?} as a type is {t:?}"))?;
                }
                if let Some(l) = node.as_list() {
                    oracle(l.node() == node, || format!("{node:?} as a list is {l:?}"))?;
                }
            }
            let past = (u32::try_from(rows).unwrap_or(u32::MAX) << 2) | tag;
            oracle(cx.tys.node_at(past).is_none(), || format!("node_at({past:#x}) past {rows} rows of class {tag}"))?;
        }
        oracle(Ty::BOOL.node().as_region().is_none() && Region::STATIC.node().as_ty().is_none(), || {
            String::from("a node of one class read as another")
        })
    });
}

/// The variables a tape makes and binds: types, ints, floats and consts.
struct Tape4 {
    tys: Vec<Ty>,
    consts: Vec<crate::Const>,
}

fn tape_vars(tape: &Tape, cx: &mut TyCx<'_>) -> Tape4 {
    let n = 2 + tape.index(5, "vars");
    let mut tys = Vec::new();
    for _ in 0..n {
        tys.push(match tape.index(3, "var kind") {
            0 => cx.next_ty_var(),
            1 => cx.next_int_var(),
            _ => cx.next_float_var(),
        });
    }
    let consts = (0..1 + tape.index(3, "const vars")).map(|_| cx.next_const_var()).collect();
    Tape4 { tys, consts }
}

/// One random relation: types (variables, scalars, tuples of variables) or consts.
fn tape_op(tape: &Tape, cx: &mut TyCx<'_>, v: &Tape4) {
    if tape.one_in(4, "const op") {
        let a = v.consts[tape.index(v.consts.len(), "const a")];
        let b = if tape.coin("const b var") {
            v.consts[tape.index(v.consts.len(), "const b")]
        } else {
            cx.tys.const_value(Ty::uint(UintTy::Usize), u128::from(tape.index(3, "value") as u32))
        };
        let _ = cx.eq(a.arg(), b.arg(), &mut AllInvariant);
        return;
    }
    let a = v.tys[tape.index(v.tys.len(), "a")];
    let b = match tape.index(5, "b kind") {
        0 => v.tys[tape.index(v.tys.len(), "b var")],
        1 => Ty::int(IntTy::I32),
        2 => Ty::BOOL,
        3 => {
            let x = v.tys[tape.index(v.tys.len(), "tuple x")];
            let y = v.tys[tape.index(v.tys.len(), "tuple y")];
            cx.tys.tuple_of(&[x, y])
        }
        _ => cx.tys.slice(Ty::BOOL),
    };
    let _ = cx.eq(a.arg(), b.arg(), &mut AllInvariant);
}

/// Every variable of every table, by its row, with the kind the binding log names it by.
fn every_var(cx: &TyCx<'_>) -> Vec<(crate::VarId, Node)> {
    use crate::kind::{ct, ty};
    let mut out = Vec::new();
    for (kind, tag, k) in [
        (VarKind::Ty, crate::ids::TAG_TY, ty::TY_VAR),
        (VarKind::Int, crate::ids::TAG_TY, ty::INT_VAR),
        (VarKind::Float, crate::ids::TAG_TY, ty::FLOAT_VAR),
        (VarKind::Const, crate::ids::TAG_CONST, ct::VAR),
    ] {
        for i in 0..cx.vars.var_count(kind) {
            let index = u32::try_from(i).unwrap_or(u32::MAX);
            let row = cx.var_row(tag, k, index);
            out.push((crate::VarId { kind, index }, Node((row << 2) | tag)));
        }
    }
    out
}

/// What `probe` (or `probe_const`) gives for a variable's node.
fn probe_node(cx: &TyCx<'_>, n: Node) -> Node {
    match n.as_ty() {
        Some(t) => cx.probe(t).node(),
        None => cx.probe_const(n.konst()).node(),
    }
}

/// The unbound roots, whose `probe` is themselves.
fn unbound_roots(cx: &TyCx<'_>) -> Vec<crate::VarId> {
    every_var(cx).into_iter().filter(|(_, n)| probe_node(cx, *n) == *n).map(|(v, _)| v).collect()
}

fn binding_log(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let mut cx = TyCx::new_in(blaze_own::testing::heap().alloc());
    cx.vars.plant = plant;
    let v = tape_vars(tape, &mut cx);
    let ops = 1 + tape.index(8, "ops");
    let mut snaps = Vec::new();
    for step in 0..ops {
        if tape.one_in(4, "open") {
            snaps.push((cx.snapshot(), cx.vars.bindings().len()));
        }
        // (2): probe equals shallow_resolve and writes nothing.
        let undo = cx.vars.undo_len();
        let probed: Vec<(Node, Node)> = every_var(&cx).iter().map(|(_, n)| (*n, probe_node(&cx, *n))).collect();
        oracle(cx.vars.undo_len() == undo, || format!("step {step}: probing wrote {} undo entries", cx.vars.undo_len() - undo))?;
        for (n, p) in probed {
            let s = match n.as_ty() {
                Some(t) => cx.shallow_resolve(t).node(),
                None => cx.shallow_resolve_const(n.konst()).node(),
            };
            oracle(p == s, || format!("step {step}: probe({n:?}) = {p:?}, shallow_resolve = {s:?}"))?;
        }
        // (3): the log grows by exactly the roots that stopped being unbound roots.
        let roots = unbound_roots(&cx);
        let before = cx.vars.bindings().len();
        tape_op(tape, &mut cx, &v);
        let after = unbound_roots(&cx);
        let mut changed: Vec<crate::VarId> = roots.iter().filter(|r| !after.contains(r)).copied().collect();
        let mut logged: Vec<crate::VarId> = cx.vars.bindings()[before..].to_vec();
        changed.sort_unstable();
        logged.sort_unstable();
        logged.dedup();
        oracle(changed == logged, || format!("step {step}: the log grew by {logged:?}, the roots that changed are {changed:?}"))?;
        if tape.one_in(3, "close") {
            if let Some((s, len)) = snaps.pop() {
                if tape.coin("commit") {
                    cx.commit(s);
                } else {
                    cx.rollback_to(s);
                    oracle(cx.vars.bindings().len() == len, || {
                        format!("step {step}: after a rollback the log has {} entries, at the snapshot {len}", cx.vars.bindings().len())
                    })?;
                }
            }
        }
    }
    Ok(())
}

#[test]
fn probe_equals_shallow_resolve_and_the_log_names_each_change() {
    blaze_sim::check(Config::cases(500), |tape| binding_log(tape, Plant::Clean));
}

/// The control: a log that misses the int table's bindings is caught.
#[test]
fn binding_log_control_catches_an_unlogged_int_binding() {
    blaze_sim::control(Config::cases(500), Plant::Clean, Plant::ForgetIntBinding, "the log grew by", binding_log);
}

/// (4) The counts: one per variable made, rolled back with the snapshot that made them.
#[test]
fn var_counts_follow_creation_and_rollback() {
    let mut cx = TyCx::new_in(blaze_own::testing::heap().alloc());
    let _ = (cx.next_ty_var(), cx.next_ty_var(), cx.next_int_var(), cx.next_const_var(), cx.next_region_var());
    let counts = |cx: &TyCx<'_>| {
        [VarKind::Ty, VarKind::Int, VarKind::Float, VarKind::Const, VarKind::Region].map(|k| cx.vars.var_count(k))
    };
    assert_eq!(counts(&cx), [2, 1, 0, 1, 1]);
    let s = cx.snapshot();
    let _ = (cx.next_float_var(), cx.next_ty_var(), cx.next_region_var());
    assert_eq!(counts(&cx), [3, 1, 1, 1, 2]);
    cx.rollback_to(s);
    assert_eq!(counts(&cx), [2, 1, 0, 1, 1]);
}

/// Rolling back an outer snapshot undoes the inner one's writes too and closes it, so the
/// outer can be opened again; a new snapshot after it nests from the outer's depth.
#[test]
fn rollback_to_an_outer_snapshot_closes_the_inner() {
    use crate::fold::AllInvariant;
    let mut cx = TyCx::new_in(blaze_own::testing::heap().alloc());
    let v = cx.next_ty_var();
    let w = cx.next_ty_var();
    let outer = cx.snapshot();
    assert_eq!(cx.eq(v.arg(), Ty::BOOL.arg(), &mut AllInvariant), Ok(true));
    let _inner = cx.snapshot();
    assert_eq!(cx.eq(w.arg(), Ty::CHAR.arg(), &mut AllInvariant), Ok(true));
    cx.rollback_to(outer);
    assert!(!cx.vars.in_snapshot(), "the inner snapshot was closed with the outer");
    assert_eq!(cx.shallow_resolve(v), v);
    assert_eq!(cx.shallow_resolve(w), w);
    let again = cx.snapshot();
    assert_eq!(cx.eq(v.arg(), Ty::CHAR.arg(), &mut AllInvariant), Ok(true));
    cx.commit(again);
    assert_eq!(cx.shallow_resolve(v), Ty::CHAR);
}

#[test]
fn var_node_names_each_variable_and_nothing_past_the_count() {
    use crate::VarId;
    let mut cx = TyCx::new_in(blaze_own::testing::heap().alloc());
    let made = [
        (VarId { kind: VarKind::Ty, index: 0 }, cx.next_ty_var().node()),
        (VarId { kind: VarKind::Ty, index: 1 }, cx.next_ty_var().node()),
        (VarId { kind: VarKind::Int, index: 0 }, cx.next_int_var().node()),
        (VarId { kind: VarKind::Float, index: 0 }, cx.next_float_var().node()),
        (VarId { kind: VarKind::Const, index: 0 }, cx.next_const_var().node()),
        (VarId { kind: VarKind::Region, index: 0 }, cx.next_region_var().node()),
    ];
    for (v, n) in made {
        assert_eq!(cx.var_node(v), Some(n), "{v:?}");
    }
    for kind in [VarKind::Ty, VarKind::Int, VarKind::Float, VarKind::Const, VarKind::Region] {
        let past = cx.vars.var_count(kind) as u32;
        assert_eq!(cx.var_node(VarId { kind, index: past }), None, "{kind:?}");
    }
}

// ---- effect polymorphism & tail-resumptive specialization ----------------------------------

/// `fn map<T, U, / E>(f: fn(T) -> U / E) -> .. / E` compiles as a plain direct function with zero
/// coroutine state-machine overhead when instantiated with a pure (or non-suspending) closure, and
/// lowers to `MachineCoroutine` when instantiated with `SUSPEND`.
#[test]
fn effect_poly_map_pure_vs_suspend_lowering_mode() {
    let e = EffectVar(0);
    let map_row = EffectRow::poly(EffectSet::PURE, e);
    assert_eq!(map_row.lowering_mode(), None, "uninstantiated polymorphic row has no fixed lowering mode");

    // Pure closure -> zero state-machine overhead (`PlainFn`).
    let pure_inst = map_row.instantiate(e, EffectSet::PURE);
    assert_eq!(pure_inst, EffectRow::Concrete(EffectSet::PURE));
    assert_eq!(pure_inst.lowering_mode(), Some(LoweringMode::PlainFn));
    assert_eq!(
        check_higher_order_call(map_row, EffectSet::PURE, EffectSet::PURE),
        Ok((EffectSet::PURE, LoweringMode::PlainFn))
    );
    assert_eq!(
        specialize_call(EffectRowSet::from_row(map_row), &[(e, EffectSet::PURE)], EffectSet::PURE),
        Ok((EffectSet::PURE, LoweringMode::PlainFn))
    );

    // Non-suspending effects (e.g. `ALLOC | PANIC`) also stay `PlainFn`.
    let alloc_panic = EffectSet::ALLOC.union(EffectSet::PANIC);
    assert_eq!(
        check_higher_order_call(map_row, alloc_panic, alloc_panic),
        Ok((alloc_panic, LoweringMode::PlainFn))
    );

    // Suspending closure -> lowers through `blaze-machine` (`MachineCoroutine`).
    let suspend_inst = map_row.instantiate(e, EffectSet::SUSPEND);
    assert_eq!(suspend_inst, EffectRow::Concrete(EffectSet::SUSPEND));
    assert_eq!(suspend_inst.lowering_mode(), Some(LoweringMode::MachineCoroutine));
    assert_eq!(
        check_higher_order_call(map_row, EffectSet::SUSPEND, EffectSet::SUSPEND),
        Ok((EffectSet::SUSPEND, LoweringMode::MachineCoroutine))
    );
    assert_eq!(
        specialize_call(EffectRowSet::from_row(map_row), &[(e, EffectSet::SUSPEND)], EffectSet::SUSPEND),
        Ok((EffectSet::SUSPEND, LoweringMode::MachineCoroutine))
    );

    // A polymorphic row that already carries `SUSPEND` in its base is immediately `MachineCoroutine`.
    let always_suspend = EffectRow::poly(EffectSet::SUSPEND, e);
    assert_eq!(always_suspend.lowering_mode(), Some(LoweringMode::MachineCoroutine));
    assert_eq!(EffectRowSet::from_row(always_suspend).lowering_mode(), Some(LoweringMode::MachineCoroutine));
}

/// Multi-variable `zip_with(f / e1, g / e2) -> / (alloc | e1 | e2)` combines rows with
/// `EffectRowSet::union`, deduplicates variables, specializes to `PlainFn` for `(PURE, PURE)` and
/// `MachineCoroutine` for `(PURE, SUSPEND)`, and enforces the inline variable bound.
#[test]
fn effect_poly_zip_with_multi_var_union_and_specialization() {
    let e1 = EffectVar(1);
    let e2 = EffectVar(2);
    let f_row = EffectRowSet::poly(EffectSet::ALLOC, e1);
    let g_row = EffectRowSet::poly(EffectSet::PURE, e2);

    let zip_row = f_row.union(g_row).expect("two vars fit in inline row set");
    let zip_rev = g_row.union(f_row).expect("union is commutative");
    assert_eq!(zip_row, zip_rev, "canonical variable order makes union commutative");
    assert_eq!(zip_row.base(), EffectSet::ALLOC);
    assert_eq!(zip_row.vars(), &[e1, e2]);
    assert_eq!(zip_row.var_count(), 2);
    assert_eq!(zip_row.as_concrete(), None);
    assert!(zip_row.may_suspend());
    assert_eq!(zip_row.lowering_mode(), None);

    // Union deduplicates shared variables cleanly.
    let deduped = zip_row.union(EffectRowSet::poly(EffectSet::PURE, e1)).expect("dedup keeps var count at 2");
    assert_eq!(deduped, zip_row);

    // `(PURE, PURE)` -> `ALLOC`, `PlainFn` (no coroutine overhead).
    let pure_call = specialize_call(zip_row, &[(e1, EffectSet::PURE), (e2, EffectSet::PURE)], EffectSet::ALLOC);
    assert_eq!(pure_call, Ok((EffectSet::ALLOC, LoweringMode::PlainFn)));
    let pure_row = zip_row.instantiate_one(e1, EffectSet::PURE).instantiate_one(e2, EffectSet::PURE);
    assert_eq!(pure_row.as_concrete(), Some(EffectSet::ALLOC));
    assert!(!pure_row.may_suspend());
    assert_eq!(pure_row.lowering_mode(), Some(LoweringMode::PlainFn));

    // `(PURE, SUSPEND)` -> `ALLOC | SUSPEND`, `MachineCoroutine`.
    let alloc_suspend = EffectSet::ALLOC.union(EffectSet::SUSPEND);
    let suspend_call =
        specialize_call(zip_row, &[(e1, EffectSet::PURE), (e2, EffectSet::SUSPEND)], alloc_suspend);
    assert_eq!(suspend_call, Ok((alloc_suspend, LoweringMode::MachineCoroutine)));

    // Partial instantiation: binding `e2` to `SUSPEND` fixes `MachineCoroutine` even before `e1` is bound.
    let partial_suspend = zip_row.instantiate_one(e2, EffectSet::SUSPEND);
    assert_eq!(partial_suspend.vars(), &[e1]);
    assert_eq!(partial_suspend.as_concrete(), None);
    assert!(partial_suspend.may_suspend());
    assert_eq!(partial_suspend.lowering_mode(), Some(LoweringMode::MachineCoroutine));

    // Partial instantiation with `PURE` leaves lowering undetermined until `e2` is known.
    let partial_pure = zip_row.instantiate_one(e1, EffectSet::PURE);
    assert_eq!(partial_pure.vars(), &[e2]);
    assert!(partial_pure.may_suspend());
    assert_eq!(partial_pure.lowering_mode(), None);

    // Exceeding `MAX_ROW_VARS` fails with `TooManyEffectVars`.
    let mut row = EffectRowSet::PURE;
    for i in 0..u32::try_from(MAX_ROW_VARS).unwrap_or(4) {
        row = row.union(EffectRowSet::poly(EffectSet::PURE, EffectVar(i))).expect("within MAX_ROW_VARS");
    }
    assert_eq!(row.var_count(), MAX_ROW_VARS);
    let overflow = row.union(EffectRowSet::poly(EffectSet::PURE, EffectVar(99)));
    assert_eq!(overflow, Err(EffectPolyError::TooManyEffectVars { max: MAX_ROW_VARS }));
}

/// `dyn Fn` object-safety: non-suspending callbacks (`ALLOC | PANIC`, `PURE`) coerce when within
/// the allowed effect set, while any `SUSPEND` callback is rejected with `SuspendBehindDyn`.
#[test]
fn effect_poly_dyn_fn_object_safety_and_subtyping() {
    let alloc_panic = EffectSet::ALLOC.union(EffectSet::PANIC);
    assert_eq!(check_dyn_fn_effects(alloc_panic, alloc_panic), Ok(()));
    assert_eq!(check_dyn_fn_effects(alloc_panic, EffectSet::ALL), Ok(()));
    assert_eq!(check_dyn_fn_effects(EffectSet::PURE, alloc_panic), Ok(()));

    // Exceeding `dyn Fn`'s allowed effects fails with `EffectExceedsAllowed`.
    assert_eq!(
        check_dyn_fn_effects(alloc_panic, EffectSet::ALLOC),
        Err(EffectPolyError::EffectExceedsAllowed { required: alloc_panic, allowed: EffectSet::ALLOC })
    );

    // `SUSPEND` is refused behind `dyn Fn` even when `allowed` includes `SUSPEND` or `ALL`.
    assert_eq!(
        check_dyn_fn_effects(EffectSet::SUSPEND, EffectSet::SUSPEND),
        Err(EffectPolyError::SuspendBehindDyn { callback_effects: EffectSet::SUSPEND })
    );
    let alloc_suspend = EffectSet::ALLOC.union(EffectSet::SUSPEND);
    assert_eq!(
        check_dyn_fn_effects(alloc_suspend, EffectSet::ALL),
        Err(EffectPolyError::SuspendBehindDyn { callback_effects: alloc_suspend })
    );
}

/// `specialize_call` enforces caller effect allowances (`EffectExceedsAllowed`) and rejects
/// partially bound polymorphic rows (`UnboundEffectVar`).
#[test]
fn effect_poly_specialize_call_rejects_disallowed_and_unbound() {
    let e1 = EffectVar(10);
    let e2 = EffectVar(20);

    // Pure `expect()` caller refuses a `SUSPEND` instantiation of `map`.
    let map_row = EffectRowSet::poly(EffectSet::PURE, e1);
    assert_eq!(
        specialize_call(map_row, &[(e1, EffectSet::SUSPEND)], EffectSet::PURE),
        Err(EffectPolyError::EffectExceedsAllowed { required: EffectSet::SUSPEND, allowed: EffectSet::PURE })
    );

    // `expect(alloc)` caller refuses `ALLOC | SUSPEND`.
    let alloc_map = EffectRowSet::poly(EffectSet::ALLOC, e1);
    let alloc_suspend = EffectSet::ALLOC.union(EffectSet::SUSPEND);
    assert_eq!(
        specialize_call(alloc_map, &[(e1, EffectSet::SUSPEND)], EffectSet::ALLOC),
        Err(EffectPolyError::EffectExceedsAllowed { required: alloc_suspend, allowed: EffectSet::ALLOC })
    );

    // Missing binding for `e2` fails with `UnboundEffectVar(e2)`.
    let zip_row = EffectRowSet::poly(EffectSet::ALLOC, e1)
        .union(EffectRowSet::poly(EffectSet::PURE, e2))
        .expect("two vars fit");
    assert_eq!(
        specialize_call(zip_row, &[(e1, EffectSet::PURE)], EffectSet::ALL),
        Err(EffectPolyError::UnboundEffectVar(e2))
    );
    // When both are unbound, the smallest canonical var (`e1`) is reported deterministically.
    assert_eq!(
        specialize_call(zip_row, &[], EffectSet::ALL),
        Err(EffectPolyError::UnboundEffectVar(e1))
    );
}

/// Algebraic laws of `EffectRowSet`: commutativity, idempotence, instantiation order-independence,
/// and exact agreement among `as_concrete`, `may_suspend`, and `lowering_mode`.
#[test]
fn effect_row_set_algebra_properties() {
    fn draw_set(tape: &Tape, name: &'static str) -> EffectSet {
        EffectSet::from_bits(tape.int_in(0u8..=31, name))
    }
    fn draw_row(tape: &Tape) -> EffectRowSet {
        let mut row = EffectRowSet::concrete(draw_set(tape, "base"));
        let n = tape.index(3, "nvars");
        for _ in 0..n {
            let v = EffectVar(tape.int_in(0u32..=5, "var"));
            if let Ok(next) = row.union(EffectRowSet::poly(EffectSet::PURE, v)) {
                row = next;
            }
        }
        row
    }
    blaze_sim::check(Config::cases(400), |tape| {

        let a = draw_row(tape);
        let b = draw_row(tape);

        // Idempotence and commutativity of `union`.
        oracle(a.union(a) == Ok(a), || format!("union not idempotent for {a:?}"))?;
        oracle(a.union(b) == b.union(a), || format!("union not commutative for {a:?} and {b:?}"))?;

        // `instantiate_one` commutes across distinct variables.
        let v1 = EffectVar(tape.int_in(0u32..=5, "v1"));
        let v2 = EffectVar(tape.int_in(0u32..=5, "v2"));
        let s1 = draw_set(tape, "s1");
        let s2 = draw_set(tape, "s2");
        if v1 != v2 {
            let ab = a.instantiate_one(v1, s1).instantiate_one(v2, s2);
            let ba = a.instantiate_one(v2, s2).instantiate_one(v1, s1);
            oracle(ab == ba, || format!("instantiate_one order changed result: {ab:?} vs {ba:?}"))?;
        }

        // Consistency of `as_concrete`, `may_suspend`, and `lowering_mode`.
        oracle(a.as_concrete().is_some() == a.vars().is_empty(), || format!("as_concrete disagrees with vars: {a:?}"))?;
        let expected_may_suspend = a.base().contains(EffectSet::SUSPEND) || !a.vars().is_empty();
        oracle(a.may_suspend() == expected_may_suspend, || format!("may_suspend wrong for {a:?}"))?;
        let expected_mode = if a.base().contains(EffectSet::SUSPEND) {
            Some(LoweringMode::MachineCoroutine)
        } else if let Some(c) = a.as_concrete() {
            Some(LoweringMode::for_effects(c))
        } else {
            None
        };
        oracle(a.lowering_mode() == expected_mode, || {
            format!("lowering_mode {:?} != expected {:?} for {a:?}", a.lowering_mode(), expected_mode)
        })
    });
}

// ---- syscall authority sets & sans-io / sandbox effect algebra -----------------------------

#[test]
fn syscall_set_bits_words_display_and_lowering_mode() {
    use crate::syscall_ty::SyscallSet;

    // Exact partition of all 8 authority bits into tail-resumptive (0..=3) and suspending (4..=7).
    assert_eq!(
        SyscallSet::CLOCK.union(SyscallSet::RNG).union(SyscallSet::VM).union(SyscallSet::STAT),
        SyscallSet::TAIL_RESUMPTIVE
    );
    assert_eq!(
        SyscallSet::FS.union(SyscallSet::NET).union(SyscallSet::PROC).union(SyscallSet::SYNC),
        SyscallSet::SUSPENDING
    );
    assert_eq!(SyscallSet::TAIL_RESUMPTIVE.intersection(SyscallSet::SUSPENDING), SyscallSet::EMPTY);
    assert_eq!(SyscallSet::TAIL_RESUMPTIVE.union(SyscallSet::SUSPENDING), SyscallSet::ALL);
    assert_eq!(SyscallSet::EMPTY.bits(), 0);
    assert_eq!(SyscallSet::ALL.bits(), 0xFF);
    assert!(SyscallSet::EMPTY.is_empty());
    assert!(!SyscallSet::CLOCK.is_empty());

    // Word parsing accepts both bare and `sys::` prefixed forms.
    let words = [
        ("clock", "sys::clock", SyscallSet::CLOCK),
        ("rng", "sys::rng", SyscallSet::RNG),
        ("vm", "sys::vm", SyscallSet::VM),
        ("stat", "sys::stat", SyscallSet::STAT),
        ("fs", "sys::fs", SyscallSet::FS),
        ("net", "sys::net", SyscallSet::NET),
        ("proc", "sys::proc", SyscallSet::PROC),
        ("sync", "sys::sync", SyscallSet::SYNC),
    ];
    for (bare, prefixed, expected) in words {
        assert_eq!(SyscallSet::from_word(bare), Some(expected));
        assert_eq!(SyscallSet::from_word(prefixed), Some(expected));
    }
    assert_eq!(SyscallSet::from_word("sys::unknown"), None);
    assert_eq!(SyscallSet::from_word(""), None);

    // Canonical Display formatting.
    assert_eq!(format!("{}", SyscallSet::EMPTY), "sys()");
    assert_eq!(format!("{}", SyscallSet::CLOCK), "sys(clock)");
    assert_eq!(format!("{}", SyscallSet::FS.union(SyscallSet::CLOCK)), "sys(clock, fs)");
    assert_eq!(format!("{}", SyscallSet::ALL), "sys(clock, rng, vm, stat, fs, net, proc, sync)");

    // Tail-resumptive domains never require suspension and lower to PlainFn.
    assert!(!SyscallSet::EMPTY.requires_suspend());
    assert_eq!(SyscallSet::EMPTY.implied_effects(), EffectSet::PURE);
    assert_eq!(SyscallSet::EMPTY.lowering_mode(), LoweringMode::PlainFn);

    assert!(!SyscallSet::TAIL_RESUMPTIVE.requires_suspend());
    assert_eq!(SyscallSet::TAIL_RESUMPTIVE.implied_effects(), EffectSet::PURE);
    assert_eq!(SyscallSet::TAIL_RESUMPTIVE.lowering_mode(), LoweringMode::PlainFn);

    // Any suspending domain implies EffectSet::SUSPEND and lowers to MachineCoroutine.
    for domain in [SyscallSet::FS, SyscallSet::NET, SyscallSet::PROC, SyscallSet::SYNC] {
        assert!(domain.requires_suspend());
        assert_eq!(domain.implied_effects(), EffectSet::SUSPEND);
        assert_eq!(domain.lowering_mode(), LoweringMode::MachineCoroutine);
        let mixed = domain.union(SyscallSet::TAIL_RESUMPTIVE);
        assert!(mixed.requires_suspend());
        assert_eq!(mixed.implied_effects(), EffectSet::SUSPEND);
        assert_eq!(mixed.lowering_mode(), LoweringMode::MachineCoroutine);
    }
}

#[test]
fn syscall_policy_and_sans_io_checks() {
    use crate::syscall_ty::{
        SyscallPolicyError, SyscallSet, check_sans_io, check_syscall_policy, is_sans_io_clean,
    };

    let fs_clock = SyscallSet::FS.union(SyscallSet::CLOCK);
    assert_eq!(check_syscall_policy(SyscallSet::EMPTY, SyscallSet::EMPTY), Ok(()));
    assert_eq!(check_syscall_policy(SyscallSet::CLOCK, fs_clock), Ok(()));
    assert_eq!(check_syscall_policy(fs_clock, fs_clock), Ok(()));

    let err = check_syscall_policy(SyscallSet::NET.union(SyscallSet::CLOCK), fs_clock).unwrap_err();
    assert_eq!(
        err,
        SyscallPolicyError::SyscallExceedsPolicy {
            required: SyscallSet::NET.union(SyscallSet::CLOCK),
            allowed: fs_clock,
        }
    );
    let err_msg = format!("{err}");
    assert!(err_msg.contains("net"), "error message must name disallowed domain: {err_msg}");

    // Pure, allocating, panicking, or diverging code with zero syscalls is sans-IO clean.
    let pure_compute = EffectSet::ALLOC.union(EffectSet::PANIC).union(EffectSet::DIVERGE);
    assert!(SyscallSet::is_sans_io_clean(EffectSet::PURE, SyscallSet::EMPTY));
    assert!(is_sans_io_clean(pure_compute, SyscallSet::EMPTY));
    assert_eq!(check_sans_io(pure_compute, SyscallSet::EMPTY), Ok(()));

    // Any syscall (even tail-resumptive CLOCK), BLOCK, or SUSPEND violates sans-IO.
    assert!(!is_sans_io_clean(EffectSet::PURE, SyscallSet::CLOCK));
    assert_eq!(
        check_sans_io(EffectSet::PURE, SyscallSet::CLOCK),
        Err(SyscallPolicyError::SansIoViolation { effects: EffectSet::PURE, syscalls: SyscallSet::CLOCK })
    );
    assert_eq!(
        check_sans_io(EffectSet::BLOCK, SyscallSet::EMPTY),
        Err(SyscallPolicyError::SansIoViolation { effects: EffectSet::BLOCK, syscalls: SyscallSet::EMPTY })
    );
    assert_eq!(
        check_sans_io(EffectSet::SUSPEND, SyscallSet::EMPTY),
        Err(SyscallPolicyError::SansIoViolation { effects: EffectSet::SUSPEND, syscalls: SyscallSet::EMPTY })
    );
    let sans_io_msg = format!(
        "{}",
        SyscallPolicyError::SansIoViolation { effects: EffectSet::BLOCK, syscalls: SyscallSet::FS }
    );
    assert!(sans_io_msg.contains("sans-io"), "violation display should mention sans-io: {sans_io_msg}");
}

#[test]
fn syscall_set_algebra_and_soundness_properties() {
    use crate::syscall_ty::{SyscallSet, check_sans_io, check_syscall_policy};

    fn draw_syscalls(tape: &Tape, name: &'static str) -> SyscallSet {
        SyscallSet::from_bits(tape.int_in(0u8..=u8::MAX, name))
    }
    fn draw_effects(tape: &Tape, name: &'static str) -> EffectSet {
        EffectSet::from_bits(tape.int_in(0u8..=31, name))
    }

    blaze_sim::check(Config::cases(500), |tape| {
        let a = draw_syscalls(tape, "a");
        let b = draw_syscalls(tape, "b");
        let c = draw_syscalls(tape, "c");
        let eff = draw_effects(tape, "eff");

        // Boolean lattice laws.
        oracle(a.union(b) == b.union(a), || format!("union not commutative for {a}, {b}"))?;
        oracle(a.intersection(b) == b.intersection(a), || format!("intersection not commutative for {a}, {b}"))?;
        oracle(a.union(b).union(c) == a.union(b.union(c)), || format!("union not associative for {a}, {b}, {c}"))?;
        oracle(a.union(a) == a && a.intersection(a) == a, || format!("not idempotent for {a}"))?;
        oracle(a.union(SyscallSet::EMPTY) == a, || format!("EMPTY not union identity for {a}"))?;
        oracle(a.intersection(SyscallSet::ALL) == a, || format!("ALL not intersection identity for {a}"))?;
        oracle(a.difference(a).is_empty(), || format!("self difference non-empty for {a}"))?;
        oracle(a.is_subset_of(b) == a.difference(b).is_empty(), || format!("subset/difference mismatch for {a}, {b}"))?;
        oracle(a.is_subset_of(b) == b.contains(a), || format!("subset/contains mismatch for {a}, {b}"))?;

        // Suspension & lowering soundness.
        let has_susp = !a.intersection(SyscallSet::SUSPENDING).is_empty();
        oracle(a.requires_suspend() == has_susp, || format!("requires_suspend wrong for {a}"))?;
        let expected_eff = if has_susp { EffectSet::SUSPEND } else { EffectSet::PURE };
        oracle(a.implied_effects() == expected_eff, || format!("implied_effects wrong for {a}"))?;
        oracle(a.lowering_mode() == LoweringMode::for_effects(expected_eff), || {
            format!("lowering_mode wrong for {a}")
        })?;
        if a.is_subset_of(b) && a.requires_suspend() {
            oracle(b.requires_suspend(), || format!("requires_suspend not monotone: {a} <= {b}"))?;
        }

        // Policy & sans-IO agreement.
        oracle(check_syscall_policy(a, b).is_ok() == a.is_subset_of(b), || {
            format!("check_syscall_policy disagrees with is_subset_of for {a}, {b}")
        })?;
        let expected_clean =
            a.is_empty() && !eff.contains(EffectSet::BLOCK) && !eff.contains(EffectSet::SUSPEND);
        oracle(SyscallSet::is_sans_io_clean(eff, a) == expected_clean, || {
            format!("is_sans_io_clean wrong for {eff}, {a}")
        })?;
        oracle(check_sans_io(eff, a).is_ok() == expected_clean, || {
            format!("check_sans_io disagrees with is_sans_io_clean for {eff}, {a}")
        })?;
        if expected_clean {
            oracle(
                a.lowering_mode() == LoweringMode::PlainFn && LoweringMode::for_effects(eff) == LoweringMode::PlainFn,
                || format!("sans-io clean item must lower as PlainFn: {eff}, {a}"),
            )?;
        }
        Ok::<(), Broke>(())
    });
}
