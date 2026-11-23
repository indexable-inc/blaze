//! Walks over terms with an explicit stack (no recursion on a term's depth): rebuilding a term of
//! the arena with some subterms replaced, and importing a term of a portable answer into the
//! arena with its generic parameters instantiated.

use blaze_hash::{FixedState, Map, MapOpts};
use blaze_own::{Alloc, Vec};

use crate::error::{Limit, Malformed, SolveError};
use crate::facts::Portable;
use crate::term::{Children, List, Ty, TyKind, children};
use crate::tys::Tys;

/// The most nodes one walk visits (rule 18).
pub const MAX_WALK: usize = 1 << 22;

#[derive(Clone, Copy)]
struct MemoSlot {
    key: u64,
    val: Ty,
    generation: u32,
}

impl MemoSlot {
    const EMPTY: MemoSlot = MemoSlot { key: 0, val: Ty::NONE, generation: 0 };
}

#[inline(always)]
fn memo_key(t: Ty, depth: u32) -> u64 {
    (u64::from(depth) << 32) | u64::from(t.0)
}

#[inline(always)]
fn hash_u64(mut x: u64) -> usize {
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
    x ^= x >> 33;
    x as usize
}

/// An O(1)-clear generation-stamped flat open-addressed hash table mapping `(Ty, u32)` to `Ty`.
/// Each slot is 16 bytes so a probe touches one cache line, and clearing between imports only
/// bumps `gen_now` without zeroing memory.
struct ImportMemo<'a> {
    slots: Vec<MemoSlot, Alloc<'a>>,
    gen_now: u32,
    len: usize,
}

impl<'a> ImportMemo<'a> {
    fn new_in(alloc: Alloc<'a>) -> ImportMemo<'a> {
        ImportMemo { slots: Vec::new_in(alloc), gen_now: 1, len: 0 }
    }

    #[inline]
    fn clear(&mut self) {
        if self.len == 0 {
            return;
        }
        self.len = 0;
        self.gen_now = self.gen_now.wrapping_add(1);
        if self.gen_now == 0 {
            for s in self.slots.iter_mut() {
                s.generation = 0;
            }
            self.gen_now = 1;
        }
    }

    #[inline]
    fn get(&self, t: Ty, depth: u32) -> Option<Ty> {
        if self.len == 0 {
            return None;
        }
        let key = memo_key(t, depth);
        let mask = self.slots.len() - 1;
        let mut at = hash_u64(key) & mask;
        loop {
            let s = self.slots[at];
            if s.generation != self.gen_now {
                return None;
            }
            if s.key == key {
                return Some(s.val);
            }
            at = (at + 1) & mask;
        }
    }

    #[inline]
    fn insert(&mut self, t: Ty, depth: u32, val: Ty) {
        if (self.len + 1) * 2 > self.slots.len() {
            self.grow();
        }
        let key = memo_key(t, depth);
        let mask = self.slots.len() - 1;
        let mut at = hash_u64(key) & mask;
        loop {
            let s = &mut self.slots[at];
            if s.generation != self.gen_now {
                *s = MemoSlot { key, val, generation: self.gen_now };
                self.len += 1;
                return;
            }
            if s.key == key {
                s.val = val;
                return;
            }
            at = (at + 1) & mask;
        }
    }

    #[cold]
    fn grow(&mut self) {
        let cap = (self.slots.len() * 2).max(64);
        let alloc = *self.slots.allocator();
        let mut slots = Vec::new_in(alloc);
        slots.resize(cap, MemoSlot::EMPTY);
        let old = core::mem::replace(&mut self.slots, slots);
        let was = self.gen_now;
        self.gen_now = 1;
        self.len = 0;
        let mask = cap - 1;
        for s in old.iter() {
            if s.generation != was {
                continue;
            }
            let mut at = hash_u64(s.key) & mask;
            while self.slots[at].generation == self.gen_now {
                at = (at + 1) & mask;
            }
            self.slots[at] = MemoSlot { key: s.key, val: s.val, generation: self.gen_now };
            self.len += 1;
        }
    }
}

/// Scratch the walks reuse: term work and result stacks, plus portable-parameter buffers.
pub struct Scratch<'a> {
    work: Vec<(Ty, u32, u32), Alloc<'a>>,
    out: Vec<Ty, Alloc<'a>>,
    /// Reusable children buffer for nodes with more than 8 children (avoids per-walk allocation).
    kids: Vec<Ty, Alloc<'a>>,
    /// The max-ordered worklist for portable-table reachability walks. It is reused by
    /// `builtin::params_reached`, whose callers may inspect only a few nodes of a large table.
    params_work: Vec<u32, Alloc<'a>>,
    /// Roots collected from an impl header before its portable terms are walked.
    params_roots: Vec<Ty, Alloc<'a>>,
    /// The import's memo: a portable node and binder depth to its arena handle, for the nodes one import
    /// reaches (a record's table may hold many terms; only the root's are walked).
    memo: ImportMemo<'a>,
    /// Which generics of an impl are const parameters, one bit each, by the impl and its
    /// header's digest (coherence.rs `fresh_for_impl`): found once per solver, not once per
    /// candidate.
    masks: Map<(crate::term::Def, u64), u64, FixedState, Alloc<'a>>,
    /// The solver's counters (`eval::Stats`), kept here because every walk and rule is handed
    /// this table.
    pub(crate) stats: crate::eval::Stats,
    /// The auto-trait summaries per (ADT, trait) of this solver (auto_summary.rs).
    pub(crate) auto: crate::auto_summary::Summaries<'a>,
    /// The shortcuts turned off (none but in tests).
    pub(crate) off: crate::eval::Off,
    /// On only while `Solver::why_no` runs, so a rule notes what only its chain reads (a
    /// negative impl naming the goal's type: trait_goals.rs `step_impl`). Not a shortcut.
    pub(crate) why_no: bool,
}

impl<'a> Scratch<'a> {
    pub fn new_in(alloc: Alloc<'a>) -> Scratch<'a> {
        Scratch {
            work: Vec::new_in(alloc),
            out: Vec::new_in(alloc),
            kids: Vec::new_in(alloc),
            params_work: Vec::new_in(alloc),
            params_roots: Vec::new_in(alloc),
            memo: ImportMemo::new_in(alloc),
            masks: Map::new(MapOpts { hasher: FixedState, alloc: alloc, capacity: None }),
            stats: crate::eval::Stats::default(),
            auto: crate::auto_summary::Summaries::new_in(alloc),
            off: crate::eval::Off { summaries: false, prematch: false, memo: false },
            why_no: false,
        }
    }

    pub(crate) fn params_work(&mut self) -> &mut Vec<u32, Alloc<'a>> {
        &mut self.params_work
    }

    pub(crate) fn params_buffers(&mut self) -> (&mut Vec<Ty, Alloc<'a>>, &mut Vec<u32, Alloc<'a>>) {
        (&mut self.params_roots, &mut self.params_work)
    }

    /// The const-parameter mask kept for `impl_` with the header digest `digest`.
    pub(crate) fn const_mask(&self, impl_: crate::term::Def, digest: u64) -> Option<u64> {
        self.masks.get(&(impl_, digest)).copied()
    }

    /// Keeps `mask` for `impl_` with the header digest `digest`.
    pub(crate) fn keep_const_mask(&mut self, impl_: crate::term::Def, digest: u64, mask: u64) {
        self.masks.insert((impl_, digest), mask);
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

/// A signature and each existential predicate introduce a binder. The region on a
/// `Dynamic` node is outside those binders (blaze-ty's kind descriptors).
fn child_depth(k: TyKind, depth: u32) -> u32 {
    depth + u32::from(matches!(k, TyKind::FnPtr(..) | TyKind::ExTrait(..) | TyKind::ExProj(..)))
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
        TyKind::Record(s, _) => TyKind::Record(s, tys.mk_list(kids)?),
        TyKind::Union(_) => TyKind::Union(tys.mk_list(kids)?),
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
    fold_at_depth(tys, s, root, |tys, _, t, _| replace(tys, t))
}

pub(crate) fn fold_at_depth<T: Tys>(
    tys: &mut T,
    s: &mut Scratch<'_>,
    root: Ty,
    mut replace: impl FnMut(&mut T, &mut Scratch<'_>, Ty, u32) -> Result<Option<Ty>, SolveError>,
) -> Result<Ty, SolveError> {
    let base_work = s.work.len();
    let base_out = s.out.len();
    let result = fold_walk(tys, s, root, base_work, base_out, &mut replace);
    s.work.truncate(base_work);
    s.out.truncate(base_out);
    result
}

fn fold_walk<T: Tys>(
    tys: &mut T,
    s: &mut Scratch<'_>,
    root: Ty,
    base_work: usize,
    base_out: usize,
    replace: &mut impl FnMut(&mut T, &mut Scratch<'_>, Ty, u32) -> Result<Option<Ty>, SolveError>,
) -> Result<Ty, SolveError> {
    s.work.push((root, 0, u32::MAX));
    let mut visited = 0usize;
    while s.work.len() > base_work {
        let (t, depth, state) = s.work[s.work.len() - 1];
        if state == u32::MAX {
            visited += 1;
            if visited > MAX_WALK {
                return Err(SolveError::Limit(Limit::Walk));
            }
            if let Some(r) = replace(tys, s, t, depth)? {
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
            s.work[last].2 = s.out.len() as u32;
            for i in (0..n).rev() {
                let ch = child(c, i, |l, j| tys.list_get(l, j));
                s.work.push((ch, child_depth(k, depth), u32::MAX));
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
            } else if (n as usize) <= 8 {
                let mut kids: [Ty; 8] = [Ty::NONE; 8];
                kids[..n as usize].copy_from_slice(&s.out[start..start + n as usize]);
                rebuild(tys, k, &kids[..n as usize])?
            } else {
                s.kids.clear();
                s.kids.extend_from_slice(&s.out[start..start + n as usize]);
                rebuild(tys, k, &s.kids)?
            };
            s.out.truncate(start);
            s.out.push(r);
        }
    }
    Ok(s.out[base_out])
}

/// How a portable term's generic parameters are instantiated.
#[derive(Clone, Copy)]
pub struct Inst<'i> {
    /// `Param(i)` and `ConstParam(i)` become `args[i]`.
    pub args: &'i [Ty],
    /// A variable of the enclosing binder becomes `bound[i]` at any depth (empty: kept as is).
    pub bound: &'i [Ty],
}

/// The portable term `root` of `p` in the arena, its parameters instantiated by `inst`. A region
/// parameter becomes `ReErased` (regions are not related: solve.md, "Not ported"). A parameter
/// past `inst` is `Malformed::Param` naming the caller's line.
#[track_caller]
pub fn import<T: Tys>(tys: &mut T, s: &mut Scratch<'_>, p: Portable<'_>, root: Ty, inst: Inst<'_>) -> Result<Ty, SolveError> {
    import_at(tys, s, p, root, inst, core::panic::Location::caller())
}

/// Evaluates a 0-child portable node `k` at binder depth `depth`.
#[inline]
fn import_zero_arity<T: Tys>(
    tys: &mut T,
    s: &mut Scratch<'_>,
    k: TyKind,
    depth: u32,
    inst: Inst<'_>,
    at: &'static core::panic::Location<'static>,
) -> Result<Ty, SolveError> {
    match k {
        TyKind::Param(x) | TyKind::ConstParam(x) => match inst.args.get(x as usize) {
            Some(&a) => land(tys, s, a, depth),
            None => Err(SolveError::Malformed(Malformed::Param {
                index: x,
                args: inst.args.len() as u32,
                at,
                goal: None,
                head: None,
            })),
        },
        TyKind::ReParam(_) => tys.mk(TyKind::ReErased),
        TyKind::Bound(d, x) if d == depth && !inst.bound.is_empty() => match inst.bound.get(x as usize) {
            Some(&b) => land(tys, s, b, depth),
            None => Err(SolveError::Malformed(Malformed::Param {
                index: x,
                args: inst.bound.len() as u32,
                at,
                goal: None,
                head: None,
            })),
        },
        // Removing the enclosing binder moves variables bound farther out one level closer.
        TyKind::Bound(d, x) if d > depth && !inst.bound.is_empty() => tys.mk(TyKind::Bound(d - 1, x)),
        _ => tys.mk(k),
    }
}

/// Fast-path for a 0-child root at binder depth 0: resolves without touching `memo` or `work`.
#[inline]
fn import_leaf<T: Tys>(
    tys: &mut T,
    s: &mut Scratch<'_>,
    k: TyKind,
    inst: Inst<'_>,
    at: &'static core::panic::Location<'static>,
) -> Option<Result<Ty, SolveError>> {
    if arity(children(k)) != 0 {
        return None;
    }
    let r = import_zero_arity(tys, s, k, 0, inst, at);
    if r.is_ok() {
        s.stats.terms_imported += 1;
    }
    Some(r)
}

/// [`import`] with the line a `Malformed::Param` names given: for a helper that imports from a
/// closure (which cannot forward its caller's location) and takes `at` once, `#[track_caller]`.
pub fn import_at<T: Tys>(
    tys: &mut T,
    s: &mut Scratch<'_>,
    p: Portable<'_>,
    root: Ty,
    inst: Inst<'_>,
    at: &'static core::panic::Location<'static>,
) -> Result<Ty, SolveError> {
    let len = p.nodes.len() as u32;
    if root.0 >= len {
        return Err(SolveError::Malformed(Malformed::PortableIndex { node: root.0, len }));
    }
    if let Some(r) = import_leaf(tys, s, p.nodes[root.0 as usize], inst, at) {
        return r;
    }
    // A post-order walk of the nodes `root` reaches (a record's table may hold other terms,
    // over other parameters, that this import must not touch). A child's index is below its
    // parent's (children come first in a table), which bounds the walk on a malformed table.
    s.memo.clear();
    let wb = s.work.len();
    let r = import_walk(tys, s, p, root, inst, at);
    s.work.truncate(wb);
    s.memo.clear();
    r
}

/// Imports every portable term in `roots` into `out` sharing one `memo` generation across all
/// elements (and without touching `memo` at all when every element is a 0-child leaf).
pub fn import_slice<T: Tys>(
    tys: &mut T,
    s: &mut Scratch<'_>,
    p: Portable<'_>,
    roots: &[Ty],
    inst: Inst<'_>,
    out: &mut [Ty],
    at: &'static core::panic::Location<'static>,
) -> Result<(), SolveError> {
    debug_assert_eq!(roots.len(), out.len());
    let wb = s.work.len();
    let mut memo_started = false;
    let r = import_slice_inner(tys, s, p, roots, inst, out, at, &mut memo_started);
    s.work.truncate(wb);
    if memo_started {
        s.memo.clear();
    }
    r
}

fn import_slice_inner<T: Tys>(
    tys: &mut T,
    s: &mut Scratch<'_>,
    p: Portable<'_>,
    roots: &[Ty],
    inst: Inst<'_>,
    out: &mut [Ty],
    at: &'static core::panic::Location<'static>,
    memo_started: &mut bool,
) -> Result<(), SolveError> {
    let len = p.nodes.len() as u32;
    for (dst, &root) in out.iter_mut().zip(roots.iter()) {
        if root.0 >= len {
            return Err(SolveError::Malformed(Malformed::PortableIndex { node: root.0, len }));
        }
        let k = p.nodes[root.0 as usize];
        if let Some(res) = import_leaf(tys, s, k, inst, at) {
            *dst = res?;
            continue;
        }
        if !*memo_started {
            s.memo.clear();
            *memo_started = true;
        } else if let Some(cached) = s.memo.get(root, 0) {
            *dst = cached;
            continue;
        }
        *dst = import_walk(tys, s, p, root, inst, at)?;
    }
    Ok(())
}

/// Imports the portable list `l` of `p` into an arena `List`, sharing one `memo` generation
/// across its elements and avoiding heap allocation for lists of up to 8 elements.
pub fn import_list_at<T: Tys>(
    tys: &mut T,
    s: &mut Scratch<'_>,
    p: Portable<'_>,
    l: List,
    inst: Inst<'_>,
    at: &'static core::panic::Location<'static>,
) -> Result<List, SolveError> {
    if l.len == 0 {
        return tys.mk_list(&[]);
    }
    let start = l.start as usize;
    let len = l.len as usize;
    let end = match start.checked_add(len) {
        Some(e) if e <= p.lists.len() => e,
        _ => {
            let bad = if start < p.lists.len() { p.lists.len() as u32 } else { l.start };
            return Err(SolveError::Malformed(Malformed::PortableIndex {
                node: bad,
                len: p.lists.len() as u32,
            }));
        }
    };
    let roots = &p.lists[start..end];
    if len <= 8 {
        let mut buf = [Ty::NONE; 8];
        import_slice(tys, s, p, roots, inst, &mut buf[..len], at)?;
        tys.mk_list(&buf[..len])
    } else {
        let mut out: Vec<Ty, Alloc<'_>> = Vec::with_capacity_in(len, *s.work.allocator());
        out.resize(len, Ty::NONE);
        import_slice(tys, s, p, roots, inst, &mut out, at)?;
        tys.mk_list(&out)
    }
}

fn import_walk<T: Tys>(
    tys: &mut T,
    s: &mut Scratch<'_>,
    p: Portable<'_>,
    root: Ty,
    inst: Inst<'_>,
    at: &'static core::panic::Location<'static>,
) -> Result<Ty, SolveError> {
    let get = |l: List, x: u32| {
        let at = (l.start + x) as usize;
        if at < p.lists.len() { p.lists[at] } else { Ty::NONE }
    };
    let base = s.work.len();
    s.work.push((root, 0, 0));
    let mut steps = 0usize;
    while s.work.len() > base {
        let Some((t, depth, exit)) = s.work.pop() else { break };
        steps += 1;
        if steps > MAX_WALK {
            s.work.truncate(base);
            return Err(SolveError::Limit(Limit::Walk));
        }
        if exit == 0 && s.memo.get(t, depth).is_some() {
            continue;
        }
        let k = p.nodes[t.0 as usize];
        let c = children(k);
        let n = arity(c);
        let next_depth = child_depth(k, depth);
        if exit == 0 {
            if n == 0 {
                let made = import_zero_arity(tys, s, k, depth, inst, at)?;
                s.stats.terms_imported += 1;
                s.memo.insert(t, depth, made);
                continue;
            }
            s.work.push((t, depth, 1));
            for j in (0..n).rev() {
                let ch = child(c, j, get);
                if ch.0 >= t.0 {
                    s.work.truncate(base);
                    return Err(SolveError::Malformed(Malformed::PortableIndex { node: ch.0, len: t.0 }));
                }
                if s.memo.get(ch, next_depth).is_none() {
                    s.work.push((ch, next_depth, 0));
                }
            }
            continue;
        }
        let made = if (n as usize) <= 8 {
            let mut kids = [Ty::NONE; 8];
            for j in 0..n {
                let ch = child(c, j, get);
                kids[j as usize] = s.memo.get(ch, next_depth).unwrap_or(Ty::NONE);
            }
            rebuild(tys, k, &kids[..n as usize])?
        } else {
            s.kids.clear();
            for j in 0..n {
                let ch = child(c, j, get);
                s.kids.push(s.memo.get(ch, next_depth).unwrap_or(Ty::NONE));
            }
            rebuild(tys, k, &s.kids)?
        };
        s.stats.terms_imported += 1;
        s.memo.insert(t, depth, made);
    }
    Ok(s.memo.get(root, 0).unwrap_or(Ty::NONE))
}

/// An argument entering a binder must retain references to its own surrounding binders.
fn land<T: Tys>(tys: &mut T, s: &mut Scratch<'_>, arg: Ty, depth: u32) -> Result<Ty, SolveError> {
    if depth == 0 || !tys.flags(arg).has(crate::term::Flags::HAS_BOUND) {
        return Ok(arg);
    }
    tys.shift_in(s, arg, depth)
}

/// Instantiates the binder enclosing an arena term, including its uses under other binders.
#[track_caller]
pub(crate) fn instantiate_bound<T: Tys>(tys: &mut T, s: &mut Scratch<'_>, root: Ty, with: List) -> Result<Ty, SolveError> {
    if !tys.flags(root).has(crate::term::Flags::HAS_BOUND) {
        return Ok(root);
    }
    let at = core::panic::Location::caller();
    fold_at_depth(tys, s, root, |tys, s, t, depth| {
        if !tys.flags(t).has(crate::term::Flags::HAS_BOUND) {
            return Ok(Some(t));
        }
        match tys.kind(t) {
            TyKind::Bound(d, i) if d == depth => {
                if i >= with.len {
                    return Err(SolveError::Malformed(Malformed::Param {
                        index: i, args: with.len, at, goal: None, head: None,
                    }));
                }
                let arg = tys.list_get(with, i);
                Ok(Some(land(tys, s, arg, depth)?))
            }
            TyKind::Bound(d, i) if d > depth => Ok(Some(tys.reindex_bound(t, d - 1, i)?)),
            _ => Ok(None),
        }
    })
}

/// Visits every node of `root` (resolving variables at each node) before its children, until
/// `visit` answers `false` for a node (its children are then skipped). Returns `Err(Walk)`
/// past `MAX_WALK` nodes.
pub fn visit<T: Tys>(
    tys: &T,
    work: &mut Vec<(Ty, u32), Alloc<'_>>,
    root: Ty,
    mut f: impl FnMut(Ty, TyKind) -> bool,
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
        if !f(t, k) {
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

#[cfg(test)]
mod tests;
