//! Writeback of [`Checked::selected`](crate::Checked): each projection a callee's signature
//! named at a call and each of its trait bounds whose instance may relate a region, their
//! arguments resolved and exported, and the impl that gives the projection its value or proves
//! the bound when the arguments are ground (crate::selected's module doc); then each selected
//! impl's own where-clauses at its arguments, as further bound rows at the same call.

use blaze_hash::HashMap;
use blaze_own::{Alloc, Vec};

use super::Export;
use crate::columns::Closures;
use crate::diag::At;
use crate::error::{CheckError, Limit, Malformed, ParamSite, Stop};
use crate::ids::{ExprId, ItemKey, LangItem, List, Name};
use crate::infer::InferCtx;
use crate::instance::{OwnClosures, ResolveError};
use crate::machine::{Machine, children};
use crate::portable::{PNode, PTy, PTys, PortableTys};
use crate::selected::{MOST_NODES, MOST_PROJECTED, MOST_ROW_NESTING, Projected, Selected, holds_alias, may_relate};
use crate::ty::TypeCtx;
use crate::world::{PClause, World};

/// The most traits the caller's environment check holds (its clauses' traits and their
/// supertraits; rule 18): past it the check stops with `Limit::Solver`.
const MOST_ENV_TRAITS: usize = 1 << 12;

/// The traits every proof of which relates nothing, by lang item: `Sized`, `MetaSized`,
/// `PointeeSized`, `Destruct` (the same four as blaze-ladder front2 borrows.rs
/// `BorrowLangs::relates_nothing`). No bound row is kept for them.
#[derive(Clone, Copy)]
struct Inert([Option<ItemKey>; 4]);

impl Inert {
    fn has(self, trait_: ItemKey) -> bool {
        self.0.contains(&Some(trait_))
    }
}

/// What the caller's own where-clauses can answer at a ground row. rustc's selection
/// (rustc_trait_selection select/mod.rs `winnow_candidates`): "The next highest priority is
/// for non-global where-bounds. However, while we don't prefer global where-clauses here, we
/// do bail with ambiguity when encountering both a global and a non-global where-clause";
/// global where-bounds "get ignored" in favor of an impl. A where-clause is a candidate only
/// where it unifies with the goal (`match_where_clause_trait_ref`). So at a ground goal (no
/// type parameter, no inference variable) a clause naming a type parameter outside a
/// projection is never a candidate (the parameter is a placeholder); a clause naming none
/// (`&'a u8: Marker`, `Vec<&'a u8>: Tr`) is one only where its types equal the goal's up to
/// regions, and with regions erased here such an equal clause cannot be told from the impl's
/// lifetimes (`'a` against `'static`). A clause whose types differ from the goal's past
/// regions (`&'static u16: Marker` at `&'x u8: Marker`) is no candidate: the impl applies.
struct CallEnv<'a> {
    /// The caller assumes only `Sized` ([`Projected::global_env`]).
    global: bool,
    /// The caller's clauses were not read: any trait may be answered by one.
    all: bool,
    /// The traits a clause of the caller may answer at ANY ground row: a clause naming a
    /// projection (it normalizes to what this does not compute), and every supertrait the
    /// caller's parameter-free clauses elaborate to (the Reference, "Supertraits"; their
    /// arguments are not substituted here).
    traits: Vec<ItemKey, Alloc<'a>>,
    /// The traits of the caller's ground clauses (no type parameter, no projection): such a
    /// clause answers a ground row only where its erased types may equal the row's
    /// ([`may_equal`]).
    exact: Vec<ItemKey, Alloc<'a>>,
}

/// What every row's selection at one writeback shares.
struct Rows<'s, 'a> {
    /// The body's own closures, for a row whose arguments hold one.
    own: OwnClosures<'s>,
    env: CallEnv<'a>,
    /// Each `(trait, arguments, name)` already decided: one selection per instance and body
    /// (a `Vec<_, Global>`'s `Global: Allocator` at each of thirty calls is selected once).
    memo: HashMap<(ItemKey, List, Option<Name>), Selected, Alloc<'a>>,
}

impl<'a, W: World, C: TypeCtx> Machine<'_, '_, '_, 'a, W, C> {
    /// The rows of `Checked::selected`, sorted by expression, each call's rows in the order its
    /// signature was lowered (numbered by `at`), its bound rows after its projections, an
    /// impl's where-clause rows after the call's own. A row whose arguments did not resolve
    /// adds no finding here (the body's own diagnostics say why) and is not kept: the borrow
    /// check finds fewer rows than hidden positions and refuses that call by name.
    /// `closures` is the record's `Checked::closures` (row `i` is closure `i`, its signature a
    /// type of `ex.out`): a closure of this body among a row's arguments (`iter.filter(|b| ..)
    /// .count()`) is selected with its facts from there, since the world holds no record of a
    /// body still being checked.
    ///
    /// A bound row (`name: None`) is kept only where its trait relates something ([`Inert`])
    /// and its instance may hold a region or a caller's type parameter
    /// ([`may_relate`](crate::selected::may_relate)). The filter is here, not where the call
    /// notes the clause: there the instance still holds inference variables (`need(&n)`'s `T`
    /// is a variable until the argument is typed), here it is resolved, and the notes are a
    /// tuple each while the export and the selection are the cost.
    pub(super) fn selected_rows(&mut self, ex: &mut Export<'a>, closures: &Closures<'a>) -> Result<Vec<Projected, Alloc<'a>>, CheckError> {
        let alloc = self.alloc;
        if self.projected.len() > MOST_PROJECTED {
            return Err(CheckError::Limit(Limit::Solver));
        }
        let mut out: Vec<Projected, Alloc<'a>> = Vec::with_capacity_in(self.projected.len(), alloc);
        if self.projected.is_empty() {
            return Ok(out);
        }
        let own = OwnClosures {
            owner: self.body.owner,
            body: self.body,
            params: self.own_params,
            consts: self.own_consts,
            kinds: closures.column::<1>(),
            sigs: closures.column::<2>(),
            upvars: closures.column::<3>(),
        };
        let inert = self.inert()?;
        let env = self.call_env(inert)?;
        // In a world that stops at holes (`World::stops_at_holes`), a stop met here (each
        // `Err(Stop::Need)` arm below that would go on) ends the rows: nothing runs past a
        // missing fact, and the caller (writeback.rs) returns the run's need, as it does for any
        // holed run.
        if self.reads.stopped() {
            return Ok(out);
        }
        let mut rows = Rows { own, env, memo: HashMap::new_in(alloc) };
        // Each row once per call: the call's own, then an impl's where-clauses and nested aliases.
        let mut seen: HashMap<(ExprId, ItemKey, Option<Name>, List), (), Alloc<'a>> = HashMap::new_in(alloc);
        let mut ps: Vec<PTy, Alloc<'a>> = Vec::new_in(alloc);
        for i in 0..self.projected.len() {
            let (e, trait_, name, args) = self.projected[i];
            if name.is_none() && inert.has(trait_) {
                continue;
            }
            ps.clear();
            let n = self.diags.len();
            for j in 0..self.ty.list_len(args) {
                let t = self.ty.list_get(args, j);
                let t = self.ty.resolve(t)?;
                ps.push(self.export(ex, t, At::Expr(e))?);
            }
            if self.diags.len() != n {
                self.diags.truncate(n);
                continue;
            }
            if name.is_none() && !may_relate(ex.out.view(), &ps, alloc)? {
                continue;
            }
            let list = ex.out.intern_list(&ps)?;
            if seen.insert((e, trait_, name, list), ()).is_some() && name.is_none() {
                continue;
            }
            let selected = self.select_row(ex, &mut rows, inert, &ps, (trait_, list, name), e)?;
            if self.reads.stopped() {
                return Ok(out);
            }
            out.push(Projected { expr: e, at: 0, trait_, name, args: list, global_env: rows.env.global, selected });
        }
        self.impl_where_rows(ex, &mut rows, inert, &mut seen, &mut out)?;
        // Stable: each call's rows keep their lowering order, an impl's where-clause rows
        // (pushed last) after the call's own.
        out.sort_by_key(|r| (r.expr, r.name.is_none()));
        let mut at = 0u32;
        for i in 0..out.len() {
            at = if i > 0 && out[i - 1].expr == out[i].expr { at + 1 } else { 0 };
            out[i].at = at;
        }
        Ok(out)
    }

    /// The selected impls' own where-clauses as bound rows at the same call, breadth first:
    /// the Reference, "Implementations": an impl applies only where its where-clauses hold,
    /// so `impl<T: Clone> Clone for Vec<T>` selected at `Vec<&'x u8>` proves `&'x u8: Clone`
    /// at that call too, through the impl that applies there. Each clause is the impl's
    /// written `PClause::Trait` at the impl's arguments (`Selected::Impl::args`), kept by the
    /// call's filter ([`Inert`], [`holds_alias`], [`may_relate`]) and selected the same way;
    /// one selected `Impl` brings its own impl's clauses in turn, to [`MOST_ROW_NESTING`]
    /// (past it no row: a gap the borrow check refuses by name, never a decided answer).
    fn impl_where_rows(
        &mut self,
        ex: &mut Export<'a>,
        rows: &mut Rows<'_, 'a>,
        inert: Inert,
        seen: &mut HashMap<(ExprId, ItemKey, Option<Name>, List), (), Alloc<'a>>,
        out: &mut Vec<Projected, Alloc<'a>>,
    ) -> Result<(), CheckError> {
        let alloc = self.alloc;
        // `(row, depth)`: a row selected `Impl` whose impl's clauses and nested assoc aliases are still to file.
        let mut nest: Vec<(usize, u32), Alloc<'a>> = Vec::new_in(alloc);
        for (i, r) in out.iter().enumerate() {
            if matches!(r.selected, Selected::Impl { .. }) {
                nest.push((i, 0));
            }
        }
        let mut iargs: Vec<PTy, Alloc<'a>> = Vec::new_in(alloc);
        let mut ps: Vec<PTy, Alloc<'a>> = Vec::new_in(alloc);
        let mut next = 0usize;
        while next < nest.len() {
            let (row, depth) = nest[next];
            next += 1;
            let Projected { expr: e, name: row_name, selected: Selected::Impl { imp, args }, .. } = out[row] else { continue };
            if depth >= MOST_ROW_NESTING {
                continue;
            }
            iargs.clear();
            iargs.extend_from_slice(ex.out.view().list(args)?);
            // If this row selected an impl for a projection `type Assoc = V;` and `V` itself
            // contains a projection `<X as Tr>::N` (e.g. `impl<T, U: TryFrom<T>> TryInto<U> for T {
            // type Error = U::Error; }`), select those nested projections too so `blaze-borrow`
            // can normalize through chained associated types.
            if let Some(assoc_name) = row_name {
                let hdr = match self.reads.impl_header(imp) {
                    Ok(h) => h,
                    Err(Stop::Need(_)) if self.reads.stopped() => return Ok(()),
                    Err(Stop::Need(_)) => None,
                    Err(Stop::Error(err)) => return Err(err),
                };
                if let Some(h) = hdr
                    && let Some(&(_, val_ty)) = h.assoc_types.iter().find(|&&(n, _)| n == assoc_name)
                {
                    let mut aliases: Vec<(ItemKey, Name, List), Alloc<'a>> = Vec::new_in(alloc);
                    let mut astack: Vec<PTy, Alloc<'a>> = Vec::new_in(alloc);
                    let mut akids: Vec<PTy, Alloc<'a>> = Vec::new_in(alloc);
                    astack.push(val_ty);
                    let mut asteps = 0u32;
                    while let Some(t) = astack.pop() {
                        asteps += 1;
                        if asteps > MOST_NODES {
                            break;
                        }
                        let Ok(node) = h.types.node(t) else { break };
                        if let PNode::Alias { trait_: nt, name: nn, args: na } = node {
                            aliases.push((nt, nn, na));
                        }
                        akids.clear();
                        if h.types.children_of(node, &mut akids).is_ok() {
                            astack.extend_from_slice(&akids);
                        }
                    }
                    for (nt, nn, na) in aliases {
                        let Ok(formal_list) = h.types.list(na) else { continue };
                        if !params_in_bounds(h.types, formal_list, iargs.len() as u32, alloc)? {
                            continue;
                        }
                        ps.clear();
                        for &t in formal_list {
                            ps.push(subst_into(h.types, t, &iargs, &mut ex.out, alloc)?);
                        }
                        let list = ex.out.intern_list(&ps)?;
                        if seen.insert((e, nt, Some(nn), list), ()).is_some() {
                            continue;
                        }
                        if out.len() >= MOST_PROJECTED {
                            return Err(CheckError::Limit(Limit::Solver));
                        }
                        let selected = self.select_row(ex, rows, inert, &ps, (nt, list, Some(nn)), e)?;
                        if self.reads.stopped() {
                            return Ok(());
                        }
                        if matches!(selected, Selected::Impl { .. }) {
                            out.push(Projected { expr: e, at: 0, trait_: nt, name: Some(nn), args: list, global_env: rows.env.global, selected });
                            nest.push((out.len() - 1, depth + 1));
                        }
                    }
                }
            }
            // The impl's predicates were read by the solver when it held the impl (instance.rs
            // `try_impl`); one not answered here files no row (never a wait at writeback).
            let wc = match self.reads.predicates(imp) {
                Ok(wc) => wc,
                // A world that stops at holes stops here ([`Self::selected_rows`]).
                Err(Stop::Need(_)) if self.reads.stopped() => return Ok(()),
                Err(Stop::Need(_)) => continue,
                Err(Stop::Error(err)) => return Err(err),
            };
            for &clause in wc.clauses {
                let (trait_, cname, formal) = match clause {
                    PClause::Trait { trait_, args: formal } => (trait_, None, formal),
                    PClause::Projection { trait_, args: formal, name, .. } => (trait_, Some(name), formal),
                    _ => continue,
                };
                if cname.is_none() && inert.has(trait_) {
                    continue;
                }
                let Ok(formal_list) = wc.types.list(formal) else { continue };
                if !params_in_bounds(wc.types, formal_list, iargs.len() as u32, alloc)? {
                    continue;
                }
                ps.clear();
                for &t in formal_list {
                    let sub = subst_into(wc.types, t, &iargs, &mut ex.out, alloc)?;
                    let norm = self.normalize_portable(ex, rows, inert, seen, out, &mut nest, e, depth, sub, 0)?;
                    if self.reads.stopped() {
                        return Ok(());
                    }
                    ps.push(norm);
                }
                if cname.is_none() && !may_relate(ex.out.view(), &ps, alloc)? {
                    continue;
                }
                let list = ex.out.intern_list(&ps)?;
                if seen.insert((e, trait_, cname, list), ()).is_some() {
                    continue;
                }
                if out.len() >= MOST_PROJECTED {
                    return Err(CheckError::Limit(Limit::Solver));
                }
                let selected = self.select_row(ex, rows, inert, &ps, (trait_, list, cname), e)?;
                if self.reads.stopped() {
                    return Ok(());
                }
                if cname.is_some() && !matches!(selected, Selected::Impl { .. }) {
                    continue;
                }
                out.push(Projected { expr: e, at: 0, trait_, name: cname, args: list, global_env: rows.env.global, selected });
                if matches!(selected, Selected::Impl { .. }) {
                    nest.push((out.len() - 1, depth + 1));
                }
            }
        }
        Ok(())
    }

    /// Normalize any `PNode::Alias` inside `ty` (in `ex.out`) through `select_row`, filing each
    /// normalized alias row into `out` and enqueueing its selected impl into `nest` so chained
    /// associated-type bounds like `I::Item: IntoIterator` and
    /// `<I::Item as IntoIterator>::IntoIter: DoubleEndedIterator` in `Flatten<I>` are answered.
    #[allow(clippy::too_many_arguments)]
    fn normalize_portable(
        &mut self,
        ex: &mut Export<'a>,
        rows: &mut Rows<'_, 'a>,
        inert: Inert,
        seen: &mut HashMap<(ExprId, ItemKey, Option<Name>, List), (), Alloc<'a>>,
        out: &mut Vec<Projected, Alloc<'a>>,
        nest: &mut Vec<(usize, u32), Alloc<'a>>,
        e: ExprId,
        depth: u32,
        ty: PTy,
        norm_depth: u32,
    ) -> Result<PTy, CheckError> {
        if norm_depth >= 8 {
            return Ok(ty);
        }
        let alloc = self.alloc;
        let node = ex.out.view().node(ty)?;
        let mut kids: Vec<PTy, Alloc<'a>> = Vec::new_in(alloc);
        ex.out.view().children_of(node, &mut kids)?;
        let mut changed = false;
        for kid in kids.iter_mut() {
            let n = self.normalize_portable(ex, rows, inert, seen, out, nest, e, depth, *kid, norm_depth + 1)?;
            if self.reads.stopped() {
                return Ok(ty);
            }
            if n != *kid {
                *kid = n;
                changed = true;
            }
        }
        let cur = if changed { rebuild_in(&mut ex.out, node, &kids)? } else { ty };
        let PNode::Alias { trait_: at, name: an, args: al } = ex.out.view().node(cur)? else {
            return Ok(cur);
        };
        let mut aps: Vec<PTy, Alloc<'a>> = Vec::new_in(alloc);
        aps.extend_from_slice(ex.out.view().list(al)?);
        let selected = self.select_row(ex, rows, inert, &aps, (at, al, Some(an)), e)?;
        if self.reads.stopped() {
            return Ok(cur);
        }
        if seen.insert((e, at, Some(an), al), ()).is_none() {
            if out.len() >= MOST_PROJECTED {
                return Err(CheckError::Limit(Limit::Solver));
            }
            out.push(Projected { expr: e, at: 0, trait_: at, name: Some(an), args: al, global_env: rows.env.global, selected });
            if matches!(selected, Selected::Impl { .. }) && depth < MOST_ROW_NESTING {
                nest.push((out.len() - 1, depth + 1));
            }
        }
        let Selected::Impl { imp, args: imp_args } = selected else {
            return Ok(cur);
        };
        let hdr = match self.reads.impl_header(imp) {
            Ok(h) => h,
            Err(Stop::Need(_)) if self.reads.stopped() => return Ok(cur),
            Err(Stop::Need(_)) => None,
            Err(Stop::Error(err)) => return Err(err),
        };
        let Some(h) = hdr else { return Ok(cur) };
        let Some(&(_, val_ty)) = h.assoc_types.iter().find(|&&(n, _)| n == an) else {
            return Ok(cur);
        };
        let mut ia: Vec<PTy, Alloc<'a>> = Vec::new_in(alloc);
        ia.extend_from_slice(ex.out.view().list(imp_args)?);
        if !params_in_bounds(h.types, &[val_ty], ia.len() as u32, alloc)? {
            return Ok(cur);
        }
        let sub = subst_into(h.types, val_ty, &ia, &mut ex.out, alloc)?;
        self.normalize_portable(ex, rows, inert, seen, out, nest, e, depth, sub, norm_depth + 1)
    }

    /// How the row `key` (`(trait, [Self, trait args..], name)`, its arguments `ps` in
    /// `ex.out`) at `e` is decided: by its source where its arguments are not ground, else by
    /// the one selection home for ground arguments, over the body's own reads.
    fn select_row(&mut self, ex: &mut Export<'a>, rows: &mut Rows<'_, 'a>, inert: Inert, ps: &[PTy], key: (ItemKey, List, Option<Name>), e: ExprId) -> Result<Selected, CheckError> {
        if let Some(&s) = rows.memo.get(&key) {
            return Ok(s);
        }
        let (trait_, _, name) = key;
        let alloc = self.alloc;
        let selected = if !ground(ex.out.view(), ps, name.is_none(), alloc)? {
            // Even when `ps` contains the body's own type parameters (e.g. `RChunksMut<'a, T>:
            // ExactSizeIterator`, `(A, B): Default`, `Foo<E>: DiscriminantKind`, `E: ToString`
            // via blanket impl), select the owning impl or builtin rule whenever no caller
            // where-clause can match `ps`.
            let self_node = ps.first().and_then(|&s| ex.out.view().node(s).ok());
            let try_impl = self_node.is_some_and(|n| !matches!(n, PNode::Infer | PNode::Error));
            let self_is_abstract = matches!(
                self_node,
                Some(PNode::Param(_) | PNode::ConstParam(_) | PNode::Opaque(..) | PNode::Dynamic(_) | PNode::Alias { .. })
            );
            let mut decided = None;
            if try_impl && !self.any_clause_may_match(ex.out.view(), ps, trait_, inert)? {
                let mut cx = InferCtx::analysis_in(alloc);
                match crate::instance::select_owning_owner(&mut cx, &mut self.reads, self.body.owner, Some(rows.own), trait_, ps, &mut ex.out) {
                    Ok(Some((imp, iargs))) => decided = Some(Selected::Impl { imp, args: iargs }),
                    Ok(None) | Err(ResolveError::NoImpl { .. }) if !self_is_abstract => {
                        decided = Some(Selected::Builtin);
                    }
                    _ => {}
                }
            }
            match decided {
                Some(s) => s,
                None => self.source_not_ground(ex.out.view(), ps, trait_, name)?,
            }
        } else {
            // A where-clause of the caller may be the candidate that applies (`CallEnv`).
            let env = rows.env.all
                || rows.env.traits.contains(&trait_)
                || (rows.env.exact.contains(&trait_) && self.ground_clause_may_equal(ex.out.view(), ps, trait_)?);
            // Part of the body's own check, recorded for its borrow check: analysis.
            let mut cx = InferCtx::analysis_in(alloc);
            match crate::instance::select_owning(&mut cx, &mut self.reads, Some(rows.own), trait_, ps, &mut ex.out) {
                Ok(Some((imp, iargs))) if !env => Selected::Impl { imp, args: iargs },
                // An impl holds, and so may the caller's clause, which rustc prefers: which
                // one applies is not decided here (its lifetimes are erased).
                Ok(Some(_)) => Selected::Generic,
                // No written impl and no clause of the caller can answer: a builtin rule.
                Ok(None) | Err(ResolveError::NoImpl { .. }) if !env => Selected::Builtin,
                // No written impl: a builtin rule or the caller's where-clause.
                Ok(None) | Err(ResolveError::NoImpl { .. }) => Selected::NotImpl,
                // A fact not read, or two impls (coherence refuses that elsewhere): not
                // decided here; the borrow check refuses the call by name.
                // Undecided at GROUND arguments (`ground` above: no parameter, hole, alias
                // or opaque inside): an impl's where-clause still ambiguous there (instance.rs
                // `try_impl`), or impl arguments that did not export. Not `Generic` ("decided
                // per instance": these ARE the instance's arguments) and never `NotImpl` or
                // `Builtin` (a guessed no): a choice not made, refused by name downstream.
                Err(ResolveError::Missing(_) | ResolveError::Overlap { .. } | ResolveError::NotGround { .. }) => Selected::Unanswered,
                Err(ResolveError::Check(err)) => return Err(err.at_coroutine_import(self.body.owner, self.body.expr_range(e))),
            }
        };
        rows.memo.insert(key, selected);
        Ok(selected)
    }

    /// The four relates-nothing traits of this world ([`Inert`]).
    fn inert(&mut self) -> Result<Inert, CheckError> {
        let mut items = [None; 4];
        let langs = [LangItem::Sized, LangItem::MetaSized, LangItem::PointeeSized, LangItem::Destruct];
        for (slot, l) in items.iter_mut().zip(langs) {
            *slot = match self.lang(l) {
                Ok(k) => k,
                // Not answered at writeback (never a wait here): that trait's rows are kept
                // and selected, which decides nothing they would not.
                Err(Stop::Need(_)) => None,
                Err(Stop::Error(err)) => return Err(err),
            };
        }
        Ok(Inert(items))
    }

    /// The caller's environment for ground rows ([`CallEnv`]): whether it assumes only `Sized`,
    /// and the traits a clause of it naming no type parameter (outside a projection) may
    /// answer, with their supertraits (a bounded work list).
    fn call_env(&mut self, inert: Inert) -> Result<CallEnv<'a>, CheckError> {
        let alloc = self.alloc;
        let mut env = CallEnv { global: false, all: false, traits: Vec::new_in(alloc), exact: Vec::new_in(alloc) };
        let own = match self.reads.predicates(self.body.owner) {
            Ok(own) => own,
            Err(Stop::Need(_)) => {
                env.all = true;
                return Ok(env);
            }
            Err(Stop::Error(err)) => return Err(err),
        };
        let sized = inert.0[0];
        let global = own.clauses.iter().all(|c| matches!(c, PClause::Trait { trait_, .. } if Some(*trait_) == sized));
        env.global = global;
        for c in own.clauses {
            let (trait_, args) = match *c {
                PClause::Trait { trait_, args } | PClause::Projection { trait_, args, .. } => (trait_, args),
                _ => continue,
            };
            if inert.has(trait_) || env.traits.contains(&trait_) || names_param(own.types, args, alloc)? {
                continue;
            }
            if holds_alias(own.types, args, alloc)? {
                env.traits.push(trait_);
            } else if !env.exact.contains(&trait_) {
                env.exact.push(trait_);
            }
        }
        // The supertraits of every parameter-free clause's trait, each answerable at any row.
        let mut work: Vec<ItemKey, Alloc<'a>> = Vec::new_in(alloc);
        work.extend_from_slice(&env.traits);
        work.extend(env.exact.iter().copied().filter(|t| !env.traits.contains(t)));
        let mut next = 0usize;
        while next < work.len() {
            if work.len() > MOST_ENV_TRAITS {
                return Err(CheckError::Limit(Limit::Solver));
            }
            let t = work[next];
            next += 1;
            let def = match self.reads.trait_def(t) {
                Ok(Some(def)) => def,
                Ok(None) => continue,
                Err(Stop::Need(_)) => {
                    env.all = true;
                    return Ok(env);
                }
                Err(Stop::Error(err)) => return Err(err),
            };
            for s in def.supertraits {
                let (PClause::Trait { trait_: st, .. } | PClause::Projection { trait_: st, .. }) = *s else { continue };
                if inert.has(st) {
                    continue;
                }
                if !env.traits.contains(&st) {
                    env.traits.push(st);
                }
                if !work.contains(&st) {
                    work.push(st);
                }
            }
        }
        Ok(env)
    }

    /// Whether a ground clause of the caller of `trait_` ([`CallEnv::exact`]) may equal the
    /// ground row `ps` (types of `tys`) up to regions, so may be the candidate rustc prefers.
    /// The caller's predicates were read by `call_env`; one not answered now may answer
    /// (never a wait at writeback).
    fn ground_clause_may_equal(&mut self, tys: PTys<'_>, ps: &[PTy], trait_: ItemKey) -> Result<bool, CheckError> {
        let alloc = self.alloc;
        let own = match self.reads.predicates(self.body.owner) {
            Ok(own) => own,
            Err(Stop::Need(_)) => return Ok(true),
            Err(Stop::Error(err)) => return Err(err),
        };
        for c in own.clauses {
            let (t, args) = match *c {
                PClause::Trait { trait_, args } | PClause::Projection { trait_, args, .. } => (trait_, args),
                _ => continue,
            };
            if t != trait_ || names_param(own.types, args, alloc)? || holds_alias(own.types, args, alloc)? {
                continue;
            }
            let l = own.types.list(args)?;
            if l.len() != ps.len() {
                continue;
            }
            let mut equal = true;
            for (&a, &b) in l.iter().zip(ps) {
                if !may_equal(own.types, a, tys, b, alloc)? {
                    equal = false;
                    break;
                }
            }
            if equal {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Whether any clause of the caller (including parametric ones and their transitive
    /// supertraits) could match `ps` for `trait_`. Used before selecting an impl or builtin for
    /// non-ground rows so a caller where-clause is never shadowed by a blanket impl or builtin.
    fn any_clause_may_match(&mut self, tys: PTys<'_>, ps: &[PTy], trait_: ItemKey, inert: Inert) -> Result<bool, CheckError> {
        let alloc = self.alloc;
        let own = match self.reads.predicates(self.body.owner) {
            Ok(own) => own,
            Err(Stop::Need(_)) => return Ok(true),
            Err(Stop::Error(err)) => return Err(err),
        };
        let Some(&ps0) = ps.first() else { return Ok(true) };
        let ps0_node = tys.node(ps0)?;
        for c in own.clauses {
            let (ct, args) = match *c {
                PClause::Trait { trait_: ct, args } | PClause::Projection { trait_: ct, args, .. } => (ct, args),
                _ => continue,
            };
            if inert.has(ct) {
                continue;
            }
            let same = ct == trait_;
            if !same && !self.trait_has_super(ct, trait_, inert)? {
                continue;
            }
            let l = own.types.list(args)?;
            let Some(&a0) = l.first() else { return Ok(true) };
            let a0_node = own.types.node(a0)?;
            let self_matches = match (a0_node, ps0_node) {
                (PNode::Param(q), PNode::Param(p)) => q == p,
                (PNode::Param(_), PNode::Alias { .. } | PNode::Opaque(..) | PNode::Infer | PNode::Error) => true,
                (PNode::Param(_), _) => false,
                (PNode::Alias { args: a_args, .. }, PNode::Adt(..)) => {
                    // A rigid alias `<P as Tr>::Assoc` on a type parameter cannot shadow a
                    // nominal ADT's own impl (e.g. `P::Searcher<'a>: Clone` vs `MatchIndicesInternal<'a, P>: Clone`).
                    let rigid_param = own.types.list(a_args).ok().and_then(|al| al.first()).and_then(|&s| own.types.node(s).ok()).is_some_and(|n| matches!(n, PNode::Param(_)));
                    !rigid_param
                }
                (PNode::Alias { .. } | PNode::Opaque(..) | PNode::Infer | PNode::Error, _) => true,
                _ => may_equal(own.types, a0, tys, ps0, alloc)?,
            };
            if !self_matches {
                continue;
            }
            if same && l.len() == ps.len() {
                let mut all_ok = true;
                for (&a, &b) in l.iter().skip(1).zip(ps.iter().skip(1)) {
                    if !may_equal(own.types, a, tys, b, alloc)? {
                        all_ok = false;
                        break;
                    }
                }
                if all_ok {
                    return Ok(true);
                }
            } else if !same {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Whether `target` is a transitive supertrait of `start` (bounded BFS).
    fn trait_has_super(&mut self, start: ItemKey, target: ItemKey, inert: Inert) -> Result<bool, CheckError> {
        let alloc = self.alloc;
        let mut work: Vec<ItemKey, Alloc<'a>> = Vec::new_in(alloc);
        work.push(start);
        let mut next = 0usize;
        while next < work.len() {
            if work.len() > MOST_ENV_TRAITS {
                return Err(CheckError::Limit(Limit::Solver));
            }
            let t = work[next];
            next += 1;
            let def = match self.reads.trait_def(t) {
                Ok(Some(def)) => def,
                Ok(None) => continue,
                Err(Stop::Need(_)) => return Ok(true),
                Err(Stop::Error(err)) => return Err(err),
            };
            for s in def.supertraits {
                let (PClause::Trait { trait_: st, .. } | PClause::Projection { trait_: st, .. }) = *s else { continue };
                if inert.has(st) {
                    continue;
                }
                if st == target {
                    return Ok(true);
                }
                if !work.contains(&st) {
                    work.push(st);
                }
            }
        }
        Ok(false)
    }

    /// The source of a row whose arguments are not ground, by the head of its `Self` (`ps[0]`):
    /// an opaque type's bounds, a trait object's predicates, or the body's own where-clause on
    /// its parameter where exactly one applies ([`Selected::ParamEnv`]); else
    /// [`Selected::Generic`] (a generic impl, or a choice this does not make).
    fn source_not_ground(&mut self, tys: PTys<'_>, ps: &[PTy], trait_: ItemKey, name: Option<Name>) -> Result<Selected, CheckError> {
        let Some(&self_ty) = ps.first() else { return Ok(Selected::Generic) };
        let p = match tys.node(self_ty)? {
            PNode::Opaque(..) => return Ok(Selected::OpaqueBound),
            PNode::Dynamic(_) => return Ok(Selected::ObjectBound),
            PNode::Param(p) => p,
            _ => return Ok(Selected::Generic),
        };
        // The owner's predicates were read for its param env before the walk; a question not
        // answered here names no source (never a wait at writeback).
        let own = match self.reads.predicates(self.body.owner) {
            Ok(own) => own,
            Err(Stop::Need(_)) => return Ok(Selected::Generic),
            Err(Stop::Error(e)) => return Err(e),
        };
        // A clause on `Param(p)` of `trait_` with as many arguments as the row.
        let on_p = |args: List| -> Result<bool, CheckError> {
            let l = own.types.list(args)?;
            Ok(l.len() == ps.len() && matches!(l.first().map(|&s| own.types.node(s)).transpose()?, Some(PNode::Param(q)) if q == p))
        };
        let mut proj: Option<u32> = None;
        let mut proj_n = 0u32;
        let mut tr: Option<u32> = None;
        let mut tr_n = 0u32;
        for (i, c) in own.clauses.iter().enumerate() {
            let nth = u32::try_from(i).map_err(|_| CheckError::Limit(Limit::Solver))?;
            match *c {
                PClause::Projection { trait_: t, args, name: n, .. } if t == trait_ && Some(n) == name && on_p(args)? => {
                    proj = Some(nth);
                    proj_n += 1;
                }
                PClause::Trait { trait_: t, args } if t == trait_ && on_p(args)? => {
                    tr = Some(nth);
                    tr_n += 1;
                }
                _ => {}
            }
        }
        Ok(match (proj, proj_n, tr, tr_n) {
            (Some(clause), 1, _, _) => Selected::ParamEnv { clause },
            (None, 0, Some(clause), 1) => Selected::ParamEnv { clause },
            _ => Selected::Generic,
        })
    }
}

/// Whether every `Param`/`ConstParam` inside `roots` has index `< len`.
fn params_in_bounds(tys: PTys<'_>, roots: &[PTy], len: u32, alloc: Alloc<'_>) -> Result<bool, CheckError> {
    let mut stack: Vec<PTy, Alloc<'_>> = Vec::new_in(alloc);
    stack.extend_from_slice(roots);
    let mut steps = 0u32;
    while let Some(t) = stack.pop() {
        steps += 1;
        if steps > MOST_NODES {
            return Err(CheckError::Limit(Limit::Solver));
        }
        let node = tys.node(t)?;
        match node {
            PNode::Param(i) | PNode::ConstParam(i) if i >= len => return Ok(false),
            _ => children(node, tys, &mut stack)?,
        }
    }
    Ok(true)
}

/// Whether `roots` name nothing the call does not fix: no type parameter, inference variable,
/// projection or opaque type (a bounded work list, no recursion).
fn ground(tys: PTys<'_>, roots: &[PTy], bound: bool, alloc: Alloc<'_>) -> Result<bool, CheckError> {
    let mut stack: Vec<PTy, Alloc<'_>> = Vec::new_in(alloc);
    stack.extend_from_slice(roots);
    let mut steps = 0u32;
    while let Some(t) = stack.pop() {
        steps += 1;
        if steps > MOST_NODES {
            return Err(CheckError::Limit(Limit::Solver));
        }
        let node = tys.node(t)?;
        match node {
            // A bound-only row is evidence about a written impl. A closure, coroutine or
            // dynamic type can instead depend on a builtin witness or the caller's environment.
            PNode::Closure { .. } | PNode::Future(..) | PNode::Coroutine { .. } | PNode::Dynamic(_) if bound => return Ok(false),
            PNode::Param(_) | PNode::ConstParam(_) | PNode::Infer | PNode::Alias { .. } | PNode::Opaque(..) => return Ok(false),
            _ => children(node, tys, &mut stack)?,
        }
    }
    Ok(true)
}

/// Whether the list `l` of `tys` (a clause of the caller) names a type or const parameter
/// outside a projection: such a clause names a placeholder no ground goal unifies with. A
/// parameter inside a projection does not count (the param env normalizes
/// `<T as Tr>::Out: Marker` under `T: Tr<Out = &'a u8>` to `&'a u8: Marker`).
fn names_param(tys: PTys<'_>, l: List, alloc: Alloc<'_>) -> Result<bool, CheckError> {
    let mut stack: Vec<PTy, Alloc<'_>> = Vec::new_in(alloc);
    stack.extend_from_slice(tys.list(l)?);
    let mut steps = 0u32;
    while let Some(t) = stack.pop() {
        steps += 1;
        if steps > MOST_NODES {
            return Err(CheckError::Limit(Limit::Solver));
        }
        let node = tys.node(t)?;
        match node {
            PNode::Param(_) | PNode::ConstParam(_) => return Ok(true),
            PNode::Alias { .. } => {}
            _ => children(node, tys, &mut stack)?,
        }
    }
    Ok(false)
}

/// Whether `a` (of `at`, a caller's ground clause) and `b` (of `bt`, a ground row) may be the
/// same type up to regions (both tables erase them): `false` only where a node tells them
/// apart (another constructor, ADT, scalar, mutability, length, value or arity); a node whose
/// identity this does not compare (a closure, a coroutine or future, a trait object, an alias
/// or opaque type, a parameter, a hole, the error type) may equal anything, so the clause is
/// kept as a candidate. A bounded work list of pairs, no recursion (rule 18).
fn may_equal(at: PTys<'_>, a: PTy, bt: PTys<'_>, b: PTy, alloc: Alloc<'_>) -> Result<bool, CheckError> {
    let mut stack: Vec<(PTy, PTy), Alloc<'_>> = Vec::new_in(alloc);
    let mut ka: Vec<PTy, Alloc<'_>> = Vec::new_in(alloc);
    let mut kb: Vec<PTy, Alloc<'_>> = Vec::new_in(alloc);
    stack.push((a, b));
    let mut steps = 0u32;
    while let Some((x, y)) = stack.pop() {
        steps += 1;
        if steps > MOST_NODES {
            return Err(CheckError::Limit(Limit::Solver));
        }
        let (nx, ny) = (at.node(x)?, bt.node(y)?);
        match (head(nx), head(ny)) {
            (Some(hx), Some(hy)) if hx == hy => {}
            (Some(_), Some(_)) => return Ok(false),
            _ => return Ok(true),
        }
        ka.clear();
        kb.clear();
        at.children_of(nx, &mut ka)?;
        bt.children_of(ny, &mut kb)?;
        if ka.len() != kb.len() {
            return Ok(false);
        }
        stack.extend(ka.iter().copied().zip(kb.iter().copied()));
    }
    Ok(true)
}

/// `node` with its children's ids (table-relative) cleared, for comparing nodes of two
/// tables; `None` for a node whose identity [`may_equal`] does not compare.
fn head(node: PNode) -> Option<PNode> {
    let p = PTy(0);
    let l = List::EMPTY;
    Some(match node {
        PNode::Bool | PNode::Char | PNode::Int(_) | PNode::Uint(_) | PNode::Float(_) | PNode::Str | PNode::Never
        | PNode::Foreign(_) => node,
        PNode::Tuple(_) => PNode::Tuple(l),
        PNode::Record(s, _) => PNode::Record(s, l),
        PNode::Union(_) => PNode::Union(l),
        PNode::Ref(m, _) => PNode::Ref(m, p),
        PNode::Ptr(m, _) => PNode::Ptr(m, p),
        PNode::Array(..) => PNode::Array(p, p),
        PNode::Slice(_) => PNode::Slice(p),
        PNode::Pat(..) => PNode::Pat(p, l),
        PNode::Adt(k, _) => PNode::Adt(k, l),
        PNode::FnDef(k, _) => PNode::FnDef(k, l),
        PNode::FnPtr(_, h) => PNode::FnPtr(l, h),
        PNode::ConstValue { bits, .. } => PNode::ConstValue { ty: p, bits },
        PNode::Closure { .. }
        | PNode::Future(..)
        | PNode::Coroutine { .. }
        | PNode::Param(_)
        | PNode::ConstParam(_)
        | PNode::Alias { .. }
        | PNode::Opaque(..)
        | PNode::Infer
        | PNode::Error
        | PNode::Dynamic(_)
        | PNode::ExTrait(..)
        | PNode::ExProj { .. }
        | PNode::ExAuto(_) => return None,
    })
}

/// `root`, a type of the world table `src` (an impl's where-clause), instantiated in `out`:
/// `Param(i)` and `ConstParam(i)` become `args[i]` (types of `out`: the impl's arguments as
/// `select_owning` exported them, a closure of this body among them included), every other
/// node rebuilt in `out`. `PortableTys::subst` across two tables: a post-order work list
/// bounded by `MOST_NODES` (rule 18), children before their parent as the table orders them.
fn subst_into(src: PTys<'_>, root: PTy, args: &[PTy], out: &mut PortableTys<'_>, alloc: Alloc<'_>) -> Result<PTy, CheckError> {
    let order = |node: PTy, child: PTy| CheckError::Malformed(Malformed::TypeOrder { node: node.0, child: child.0 });
    let mut mapped: HashMap<PTy, PTy, Alloc<'_>> = HashMap::new_in(alloc);
    let mut stack: Vec<(PTy, bool), Alloc<'_>> = Vec::new_in(alloc);
    let mut kids: Vec<PTy, Alloc<'_>> = Vec::new_in(alloc);
    let mut steps = 0u32;
    stack.push((root, false));
    while let Some((at, expanded)) = stack.pop() {
        if mapped.contains_key(&at) {
            continue;
        }
        steps += 1;
        if steps > MOST_NODES {
            return Err(CheckError::Limit(Limit::Solver));
        }
        let node = src.node(at)?;
        if let PNode::Param(index) | PNode::ConstParam(index) = node {
            let site = ParamSite::PortableSubstitution { root, node: at };
            let &arg = args.get(index as usize).ok_or(CheckError::Malformed(Malformed::Param { index, args: args.len() as u32, site }))?;
            mapped.insert(at, arg);
            continue;
        }
        kids.clear();
        src.children_of(node, &mut kids)?;
        if !expanded && !kids.is_empty() {
            stack.push((at, true));
            for &child in kids.iter().rev() {
                if child.0 >= at.0 {
                    return Err(order(at, child));
                }
                stack.push((child, false));
            }
            continue;
        }
        for child in kids.iter_mut() {
            *child = *mapped.get(&*child).ok_or(order(at, *child))?;
        }
        let made = rebuild_in(out, node, &kids)?;
        mapped.insert(at, made);
    }
    mapped.get(&root).copied().ok_or(order(root, root))
}

/// `node` of another table reassembled in `out` from its children there, given in
/// `PTys::children_of`'s order.
fn rebuild_in(out: &mut PortableTys<'_>, node: PNode, kids: &[PTy]) -> Result<PTy, CheckError> {
    let node = match node {
        PNode::ConstValue { bits, .. } => PNode::ConstValue { ty: kids[0], bits },
        PNode::Tuple(_) => PNode::Tuple(out.intern_list(kids)?),
        PNode::Record(s, _) => PNode::Record(s, out.intern_list(kids)?),
        PNode::Union(_) => PNode::Union(out.intern_list(kids)?),
        PNode::Ref(m, _) => PNode::Ref(m, kids[0]),
        PNode::Ptr(m, _) => PNode::Ptr(m, kids[0]),
        PNode::Array(..) => PNode::Array(kids[0], kids[1]),
        PNode::Slice(_) => PNode::Slice(kids[0]),
        PNode::Pat(..) => PNode::Pat(kids[0], out.intern_list(&kids[1..])?),
        PNode::Adt(k, _) => PNode::Adt(k, out.intern_list(kids)?),
        PNode::FnDef(k, _) => PNode::FnDef(k, out.intern_list(kids)?),
        PNode::FnPtr(_, h) => PNode::FnPtr(out.intern_list(kids)?, h),
        PNode::Closure { owner, ordinal, .. } => PNode::Closure { owner, ordinal, args: out.intern_list(kids)? },
        PNode::Future(e, ..) => PNode::Future(e, kids[0], kids[1]),
        PNode::Coroutine { owner, ordinal, .. } => PNode::Coroutine { owner, ordinal, args: out.intern_list(kids)? },
        PNode::Alias { trait_, name, .. } => PNode::Alias { trait_, name, args: out.intern_list(kids)? },
        PNode::Opaque(k, _) => PNode::Opaque(k, out.intern_list(kids)?),
        // A trait object's rows keep their order: substitution changes no sort key.
        PNode::Dynamic(_) => PNode::Dynamic(out.intern_list(kids)?),
        PNode::ExTrait(k, _) => PNode::ExTrait(k, out.intern_list(kids)?),
        PNode::ExProj { trait_, name, args, .. } => {
            let n = args.len as usize;
            PNode::ExProj { trait_, name, args: out.intern_list(&kids[..n])?, term: kids[n] }
        }
        PNode::Bool | PNode::Char | PNode::Int(_) | PNode::Uint(_) | PNode::Float(_) | PNode::Str
        | PNode::Never | PNode::Param(_) | PNode::ConstParam(_) | PNode::Infer | PNode::Error | PNode::ExAuto(_)
        | PNode::Foreign(_) => node,
    };
    out.intern(node)
}
