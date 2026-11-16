//! Walks over terms with an explicit stack (no recursion on a term's depth): rebuilding a term of
//! the arena with some subterms replaced, and importing a term of a portable answer into the
//! arena with its generic parameters instantiated.

use blaze_hash::HashMap;
use blaze_own::{Alloc, Vec};

use crate::error::{Limit, Malformed, SolveError};
use crate::facts::Portable;
use crate::term::{Children, List, Ty, TyKind, children};
use crate::tys::Tys;

/// The most nodes one walk visits (rule 18).
pub const MAX_WALK: usize = 1 << 22;

/// Scratch the walks reuse: a work stack and a result stack.
pub struct Scratch<'a> {
    work: Vec<(Ty, u32), Alloc<'a>>,
    out: Vec<Ty, Alloc<'a>>,
    /// The import's memo: a portable node to its arena handle, for the nodes one import
    /// reaches (a record's table may hold many terms; only the root's are walked).
    memo: HashMap<u32, Ty, Alloc<'a>>,
}

impl<'a> Scratch<'a> {
    pub fn new_in(alloc: Alloc<'a>) -> Scratch<'a> {
        Scratch { work: Vec::new_in(alloc), out: Vec::new_in(alloc), memo: HashMap::new_in(alloc) }
    }
}

/// How many children `c` has.
fn arity(c: Children) -> u32 {
    match c {
        Children::None => 0,
        Children::One(_) => 1,
        Children::Two(..) => 2,
        Children::List(l) => l.len,
        Children::ListThen(l, _) => l.len + 1,
    }
}

/// The `i`th child of `c`, reading lists with `get`.
fn child(c: Children, i: u32, get: impl Fn(List, u32) -> Ty) -> Ty {
    match c {
        Children::None => Ty::NONE,
        Children::One(a) => a,
        Children::Two(a, b) => {
            if i == 0 {
                a
            } else {
                b
            }
        }
        Children::List(l) => get(l, i),
        Children::ListThen(l, t) => {
            if i < l.len {
                get(l, i)
            } else {
                t
            }
        }
    }
}

/// `k` with its children replaced by `kids` (in `children` order), made in the arena.
pub fn rebuild<T: Tys>(tys: &mut T, k: TyKind, kids: &[Ty]) -> Result<Ty, SolveError> {
    let n = kids.len();
    let k2 = match k {
        TyKind::Tuple(_) => TyKind::Tuple(tys.mk_list(kids)?),
        TyKind::Adt(d, _) => TyKind::Adt(d, tys.mk_list(kids)?),
        TyKind::FnDef(d, _) => TyKind::FnDef(d, tys.mk_list(kids)?),
        TyKind::FnPtr(_, h) => TyKind::FnPtr(tys.mk_list(kids)?, h),
        TyKind::Closure(d, _) => TyKind::Closure(d, tys.mk_list(kids)?),
        TyKind::Coroutine(d, _) => TyKind::Coroutine(d, tys.mk_list(kids)?),
        TyKind::Alias(r, a, d, _) => TyKind::Alias(r, a, d, tys.mk_list(kids)?),
        TyKind::ExTrait(d, _) => TyKind::ExTrait(d, tys.mk_list(kids)?),
        TyKind::Ref(_, _, m) => TyKind::Ref(kids[0], kids[1], m),
        TyKind::Array(..) => TyKind::Array(kids[0], kids[1]),
        TyKind::RawPtr(_, m) => TyKind::RawPtr(kids[0], m),
        TyKind::Slice(_) => TyKind::Slice(kids[0]),
        TyKind::Pat(..) => TyKind::Pat(kids[n - 1], tys.mk_list(&kids[..n - 1])?),
        TyKind::Dynamic(..) => TyKind::Dynamic(tys.mk_list(&kids[..n - 1])?, kids[n - 1]),
        TyKind::ExProj(d, ..) => TyKind::ExProj(d, tys.mk_list(&kids[..n - 1])?, kids[n - 1]),
        other => other,
    };
    tys.mk(k2)
}

/// `root` with every subterm `t` for which `replace` answers `Some(r)` replaced by `r` (not
/// walked into). `replace` sees each node before its children. Unchanged subterms keep their
/// handles.
pub fn fold<T: Tys>(
    tys: &mut T,
    s: &mut Scratch<'_>,
    root: Ty,
    mut replace: impl FnMut(&mut T, Ty) -> Result<Option<Ty>, SolveError>,
) -> Result<Ty, SolveError> {
    let base_work = s.work.len();
    let base_out = s.out.len();
    s.work.push((root, u32::MAX));
    let mut visited = 0usize;
    while s.work.len() > base_work {
        let (t, state) = s.work[s.work.len() - 1];
        if state == u32::MAX {
            visited += 1;
            if visited > MAX_WALK {
                s.work.truncate(base_work);
                s.out.truncate(base_out);
                return Err(SolveError::Limit(Limit::Walk));
            }
            if let Some(r) = replace(tys, t)? {
                s.work.pop();
                s.out.push(r);
                continue;
            }
            let k = tys.kind(t);
            let c = children(k);
            let n = arity(c);
            if n == 0 {
                s.work.pop();
                s.out.push(t);
                continue;
            }
            let last = s.work.len() - 1;
            s.work[last].1 = s.out.len() as u32;
            for i in (0..n).rev() {
                let ch = child(c, i, |l, j| tys.list_get(l, j));
                s.work.push((ch, u32::MAX));
            }
        } else {
            s.work.pop();
            let start = state as usize;
            let k = tys.kind(t);
            let c = children(k);
            let n = arity(c);
            let mut same = true;
            for i in 0..n {
                if s.out[start + i as usize] != child(c, i, |l, j| tys.list_get(l, j)) {
                    same = false;
                    break;
                }
            }
            let r = if same {
                t
            } else {
                let mut kids: [Ty; 8] = [Ty::NONE; 8];
                if (n as usize) <= kids.len() {
                    kids[..n as usize].copy_from_slice(&s.out[start..start + n as usize]);
                    rebuild(tys, k, &kids[..n as usize])?
                } else {
                    let mut big = Vec::with_capacity_in(n as usize, *s.out.allocator());
                    big.extend_from_slice(&s.out[start..start + n as usize]);
                    rebuild(tys, k, &big)?
                }
            };
            s.out.truncate(start);
            s.out.push(r);
        }
    }
    let r = s.out[base_out];
    s.out.truncate(base_out);
    Ok(r)
}

/// How a portable term's generic parameters are instantiated.
#[derive(Clone, Copy)]
pub struct Inst<'i> {
    /// `Param(i)` and `ConstParam(i)` become `args[i]`.
    pub args: &'i [Ty],
    /// `Bound(0, i)` at the top binder becomes `bound[i]` (empty: kept as is).
    pub bound: &'i [Ty],
}

/// The portable term `root` of `p` in the arena, its parameters instantiated by `inst`. A region
/// parameter becomes `ReErased` (regions are not related: solve.md, "Not ported").
pub fn import<T: Tys>(tys: &mut T, s: &mut Scratch<'_>, p: Portable<'_>, root: Ty, inst: Inst<'_>) -> Result<Ty, SolveError> {
    let len = p.nodes.len() as u32;
    if root.0 >= len {
        return Err(SolveError::Malformed(Malformed::PortableIndex { node: root.0, len }));
    }
    // A post-order walk of the nodes `root` reaches (a record's table may hold other terms,
    // over other parameters, that this import must not touch). A child's index is below its
    // parent's (children come first in a table), which bounds the walk on a malformed table.
    s.memo.clear();
    let wb = s.work.len();
    let r = import_walk(tys, s, p, root, inst);
    s.work.truncate(wb);
    s.memo.clear();
    r
}

fn import_walk<T: Tys>(tys: &mut T, s: &mut Scratch<'_>, p: Portable<'_>, root: Ty, inst: Inst<'_>) -> Result<Ty, SolveError> {
    let get = |l: List, x: u32| {
        let at = (l.start + x) as usize;
        if at < p.lists.len() { p.lists[at] } else { Ty::NONE }
    };
    let base = s.work.len();
    let mut kids: Vec<Ty, Alloc<'_>> = Vec::new_in(*s.work.allocator());
    s.work.push((root, 0));
    let mut steps = 0usize;
    while s.work.len() > base {
        let Some((t, exit)) = s.work.pop() else { break };
        steps += 1;
        if steps > MAX_WALK {
            s.work.truncate(base);
            return Err(SolveError::Limit(Limit::Walk));
        }
        if exit == 0 && s.memo.contains_key(&t.0) {
            continue;
        }
        let k = p.nodes[t.0 as usize];
        let c = children(k);
        let n = arity(c);
        if exit == 0 {
            s.work.push((t, 1));
            for j in (0..n).rev() {
                let ch = child(c, j, get);
                if ch.0 >= t.0 {
                    s.work.truncate(base);
                    return Err(SolveError::Malformed(Malformed::PortableIndex { node: ch.0, len: t.0 }));
                }
                if !s.memo.contains_key(&ch.0) {
                    s.work.push((ch, 0));
                }
            }
            continue;
        }
        kids.clear();
        for j in 0..n {
            let ch = child(c, j, get);
            kids.push(s.memo[&ch.0]);
        }
        let made = match k {
            TyKind::Param(x) | TyKind::ConstParam(x) => match inst.args.get(x as usize) {
                Some(&a) => a,
                None => {
                    s.work.truncate(base);
                    return Err(SolveError::Malformed(Malformed::Param { index: x, args: inst.args.len() as u32 }));
                }
            },
            TyKind::ReParam(_) => tys.mk(TyKind::ReErased)?,
            TyKind::Bound(0, x) if !inst.bound.is_empty() => match inst.bound.get(x as usize) {
                Some(&b) => b,
                None => {
                    s.work.truncate(base);
                    return Err(SolveError::Malformed(Malformed::Param { index: x, args: inst.bound.len() as u32 }));
                }
            },
            _ => {
                if n == 0 {
                    tys.mk(k)?
                } else {
                    rebuild(tys, k, &kids)?
                }
            }
        };
        s.memo.insert(t.0, made);
    }
    Ok(s.memo[&root.0])
}

/// Visits every node of `root` (resolving variables at each node) before its children, until
/// `visit` answers `false` for a node (its children are then skipped). Returns `Err(Walk)`
/// past `MAX_WALK` nodes.
pub fn visit<T: Tys>(
    tys: &T,
    work: &mut Vec<(Ty, u32), Alloc<'_>>,
    root: Ty,
    mut visit: impl FnMut(Ty, TyKind) -> bool,
) -> Result<(), SolveError> {
    let base = work.len();
    work.push((root, 0));
    let mut n = 0usize;
    while work.len() > base {
        let (t, _) = work.pop().unwrap_or((Ty::NONE, 0));
        n += 1;
        if n > MAX_WALK {
            work.truncate(base);
            return Err(SolveError::Limit(Limit::Walk));
        }
        let t = tys.shallow_resolve(t);
        let k = tys.kind(t);
        if !visit(t, k) {
            continue;
        }
        let c = children(k);
        for i in (0..arity(c)).rev() {
            work.push((child(c, i, |l, j| tys.list_get(l, j)), 0));
        }
    }
    Ok(())
}

/// `root` with every bound variable resolved, deeply.
pub fn resolve<T: Tys>(tys: &mut T, s: &mut Scratch<'_>, root: Ty) -> Result<Ty, SolveError> {
    // Each pass replaces every bound variable it meets by its value without walking into the
    // value; passes repeat until nothing changes. The occurs check keeps bindings acyclic, so
    // the passes are at most the depth of the binding chain; 64 bounds it (rule 18).
    let mut cur = root;
    for _ in 0..64 {
        if tys.flags(cur).no_infer() {
            return Ok(cur);
        }
        let next = fold(tys, s, cur, |tys, t| {
            let r = tys.shallow_resolve(t);
            if r != t {
                return Ok(Some(r));
            }
            if tys.flags(t).no_infer() { Ok(Some(t)) } else { Ok(None) }
        })?;
        if next == cur {
            return Ok(cur);
        }
        cur = next;
    }
    Err(SolveError::Limit(Limit::Walk))
}
