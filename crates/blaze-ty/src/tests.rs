//! Properties of the arena, the tables, relating, folding, canonicalization and export, each
//! against a slow model over plain trees, and a planted-bug control for snapshots (rule 53):
//! an int table whose writes are not logged, which the rollback oracle must catch.

use std::{format, string::String, vec, vec::Vec};

use blaze_id::{ContentId, ItemKey};
use blaze_sim::{Broke, Config, Tape};

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
