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
    pub(super) fn selected_rows(&mut self, ex: &mut Export<'a>, closures: &Closures<'a>, expr_ty: &[PTy]) -> Result<Vec<Projected, Alloc<'a>>, CheckError> {
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
            expr_ty,
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
            if name.is_none() && !may_relate(ex.out.view(), &ps, alloc)? && !self.trait_has_lifetimes(trait_)? {
                if self.reads.stopped() {
                    return Ok(out);
                }
                continue;
            }
            if self.reads.stopped() {
                return Ok(out);
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
        let initial_len = out.len();
        for (i, r) in out.iter().enumerate() {
            if matches!(r.selected, Selected::Impl { .. }) {
                nest.push((i, 0));
            }
        }
        let mut iargs: Vec<PTy, Alloc<'a>> = Vec::new_in(alloc);
        let mut ps: Vec<PTy, Alloc<'a>> = Vec::new_in(alloc);
        // Normalize any concrete or caller-bound aliases inside initial call rows (e.g.
        // `<N as FullNodeTypes>::Types: NodeTypes` under `where N: FullNodeTypes<Types = TempoNode>`,
        // or `<Self::ReturnTuple<'_> as SolType>::abi_decode_sequence`) and file the normalized
        // selection row alongside the original.
        for i in 0..initial_len {
            let Some(&Projected { expr: e, trait_, name: cname, args: rargs, selected: rsel, .. }) = out.get(i) else { continue };
            let has_alias = holds_alias(ex.out.view(), rargs, alloc)?;
            let is_non_impl_proj = cname.is_some() && !matches!(rsel, Selected::Impl { .. });
            if !has_alias && !is_non_impl_proj {
                continue;
            }
            iargs.clear();
            iargs.extend_from_slice(ex.out.view().list(rargs)?);
            ps.clear();
            let mut changed = false;
            if has_alias {
                for &t in &iargs {
                    let norm = self.normalize_portable(ex, rows, inert, seen, out, &mut nest, e, 0, t, 0)?;
                    if self.reads.stopped() {
                        return Ok(());
                    }
                    if norm != t {
                        changed = true;
                    }
                    ps.push(norm);
                }
            } else {
                ps.extend_from_slice(&iargs);
            }
            let mut final_sel = rsel;
            if changed {
                let list = ex.out.intern_list(&ps)?;
                if seen.insert((e, trait_, cname, list), ()).is_none() {
                    if out.len() >= MOST_PROJECTED {
                        return Err(CheckError::Limit(Limit::Solver));
                    }
                    let selected = self.select_row(ex, rows, inert, &ps, (trait_, list, cname), e)?;
                    if self.reads.stopped() {
                        return Ok(());
                    }
                    out.push(Projected { expr: e, at: 0, trait_, name: cname, args: list, global_env: rows.env.global, selected });
                    if matches!(selected, Selected::Impl { .. }) {
                        nest.push((out.len() - 1, 0));
                    }
                    final_sel = selected;
                }
            }
            // If a projection row was not selected via an impl (e.g. `<Node::Provider as
            // ChainSpecProvider>::ChainSpec`), elaborate any projection equality from the caller's
            // environment / assoc-type bounds / supertraits and normalize its RHS so downstream
            // impl projection rows (e.g. `NodeTypes` for `NodeTypesWithDBAdapter` & `TempoNode`)
            // are filed at `e` for `blaze-borrow`.
            if let Some(an) = cname
                && !matches!(final_sel, Selected::Impl { .. })
                && let Some(rhs) = self.normalize_from_env_or_assoc(ex, inert, trait_, an, &ps)?
            {
                if self.reads.stopped() {
                    return Ok(());
                }
                let _ = self.normalize_portable(ex, rows, inert, seen, out, &mut nest, e, 0, rhs, 0)?;
                if self.reads.stopped() {
                    return Ok(());
                }
            }
        }
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
                            let sub = subst_into(h.types, t, &iargs, &mut ex.out, alloc)?;
                            let norm = self.normalize_portable(ex, rows, inert, seen, out, &mut nest, e, depth, sub, 0)?;
                            if self.reads.stopped() {
                                return Ok(());
                            }
                            ps.push(norm);
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
                if cname.is_none() && !may_relate(ex.out.view(), &ps, alloc)? && !self.trait_has_lifetimes(trait_)? {
                    if self.reads.stopped() {
                        return Ok(())
                    }
                    continue;
                }
                if self.reads.stopped() {
                    return Ok(());
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
        struct NormFrame<'a> {
            orig: PTy,
            node: PNode,
            kids: Vec<PTy, Alloc<'a>>,
            next: usize,
            changed: bool,
            norm_depth: u32,
        }
        let alloc = self.alloc;
        let mut stack: Vec<NormFrame<'a>, Alloc<'a>> = Vec::new_in(alloc);
        let mut cur_ty = ty;
        let mut cur_depth = norm_depth;
        loop {
            // Descend into `cur_ty` at `cur_depth`.
            let mut res = if cur_depth >= 16 {
                cur_ty
            } else {
                let node = ex.out.view().node(cur_ty)?;
                let mut kids: Vec<PTy, Alloc<'a>> = Vec::new_in(alloc);
                ex.out.view().children_of(node, &mut kids)?;
                if let Some(&first_kid) = kids.first() {
                    let next_depth = cur_depth + 1;
                    stack.push(NormFrame { orig: cur_ty, node, kids, next: 0, changed: false, norm_depth: cur_depth });
                    cur_ty = first_kid;
                    cur_depth = next_depth;
                    continue;
                }
                if let Some(sub) = self.normalize_portable_alias(ex, rows, inert, seen, out, nest, e, depth, cur_ty)? {
                    cur_ty = sub;
                    cur_depth += 1;
                    continue;
                }
                if self.reads.stopped() {
                    return Ok(cur_ty);
                }
                cur_ty
            };

            // Ascend with `res` until either a sibling needs descending, an alias expands, or
            // the stack empties.
            let mut descend: Option<(PTy, u32)> = None;
            while let Some(mut frame) = stack.pop() {
                if let Some(slot) = frame.kids.get_mut(frame.next)
                    && *slot != res
                {
                    *slot = res;
                    frame.changed = true;
                }
                frame.next += 1;
                if let Some(&next_kid) = frame.kids.get(frame.next) {
                    let next_depth = frame.norm_depth + 1;
                    stack.push(frame);
                    descend = Some((next_kid, next_depth));
                    break;
                }
                let rebuilt = if frame.changed { rebuild_in(&mut ex.out, frame.node, &frame.kids)? } else { frame.orig };
                if let Some(sub) = self.normalize_portable_alias(ex, rows, inert, seen, out, nest, e, depth, rebuilt)? {
                    descend = Some((sub, frame.norm_depth + 1));
                    break;
                }
                if self.reads.stopped() {
                    return Ok(rebuilt);
                }
                res = rebuilt;
            }
            if let Some((next_ty, next_depth)) = descend {
                cur_ty = next_ty;
                cur_depth = next_depth;
            } else {
                return Ok(res);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn normalize_portable_alias(
        &mut self,
        ex: &mut Export<'a>,
        rows: &mut Rows<'_, 'a>,
        inert: Inert,
        seen: &mut HashMap<(ExprId, ItemKey, Option<Name>, List), (), Alloc<'a>>,
        out: &mut Vec<Projected, Alloc<'a>>,
        nest: &mut Vec<(usize, u32), Alloc<'a>>,
        e: ExprId,
        depth: u32,
        cur: PTy,
    ) -> Result<Option<PTy>, CheckError> {
        let alloc = self.alloc;
        let PNode::Alias { trait_: at, name: an, args: al } = ex.out.view().node(cur)? else {
            return Ok(None);
        };
        let mut aps: Vec<PTy, Alloc<'a>> = Vec::new_in(alloc);
        aps.extend_from_slice(ex.out.view().list(al)?);
        let selected = self.select_row(ex, rows, inert, &aps, (at, al, Some(an)), e)?;
        if self.reads.stopped() {
            return Ok(None);
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
            // Also normalize through the caller's `where`-clauses, associated-type item bounds
            // along the `Self` alias chain, and their transitive supertraits (e.g.
            // `where Node: FullNodeTypes<Types = TempoNode>` -> `Node::Provider: FullProvider<
            // NodeTypesWithDBAdapter<Node::Types, Node::DB>>` ->
            // `<Node::Provider as ChainSpecProvider>::ChainSpec = <NodeTypesWithDBAdapter<..> as NodeTypes>::ChainSpec`),
            // so downstream impls (`NodeTypes` for `NodeTypesWithDBAdapter`, `NodeTypes` for
            // `TempoNode`, `TempoHardforks` for `TempoChainSpec`) are selected at writeback.
            return self.normalize_from_env_or_assoc(ex, inert, at, an, &aps);
        };
        let hdr = match self.reads.impl_header(imp) {
            Ok(h) => h,
            Err(Stop::Need(_)) if self.reads.stopped() => return Ok(None),
            Err(Stop::Need(_)) => None,
            Err(Stop::Error(err)) => return Err(err),
        };
        let Some(h) = hdr else { return Ok(None) };
        let Some(&(_, val_ty)) = h.assoc_types.iter().find(|&&(n, _)| n == an) else {
            return Ok(None);
        };
        let mut ia: Vec<PTy, Alloc<'a>> = Vec::new_in(alloc);
        ia.extend_from_slice(ex.out.view().list(imp_args)?);
        let Ok(ia_len) = u32::try_from(ia.len()) else {
            return Ok(None);
        };
        if !params_in_bounds(h.types, &[val_ty], ia_len, alloc)? {
            return Ok(None);
        }
        let sub = subst_into(h.types, val_ty, &ia, &mut ex.out, alloc)?;
        Ok(Some(sub))
    }

    /// Look up a projection equality `<aps[0] as at>::an = term` in the caller's `where`-clauses,
    /// the associated-type item bounds of any `PNode::Alias` prefix of `aps[0]`, and their
    /// transitive supertraits and trait predicates (bounded worklist, no recursion).
    fn normalize_from_env_or_assoc(
        &mut self,
        ex: &mut Export<'a>,
        inert: Inert,
        at: ItemKey,
        an: Name,
        aps: &[PTy],
    ) -> Result<Option<PTy>, CheckError> {
        const MOST_PROJ_WORK: usize = 256;
        let alloc = self.alloc;
        let Some(&self_ty) = aps.first() else { return Ok(None) };

        // Walk the `Self` alias chain of `aps[0]` (e.g. `[<Node as FullNodeTypes>::Provider, Node]`).
        let mut chain: Vec<PTy, Alloc<'a>> = Vec::new_in(alloc);
        let mut cur = self_ty;
        for _ in 0..8usize {
            if chain.scan_for(&cur).is_none() {
                chain.push(cur);
            }
            match ex.out.view().node(cur)? {
                PNode::Alias { args, .. } => {
                    if let Some(&first) = ex.out.view().list(args)?.first() {
                        cur = first;
                    } else {
                        break;
                    }
                }
                _ => break,
            }
        }

        let mut work: Vec<(ItemKey, List), Alloc<'a>> = Vec::new_in(alloc);
        let mut visited: HashMap<(ItemKey, List), (), Alloc<'a>> = HashMap::new_in(alloc);
        let mut scratch: Vec<PTy, Alloc<'a>> = Vec::new_in(alloc);

        // Seed 1: caller's own `where`-clauses (no substitution).
        let own = match self.reads.predicates(self.body.owner) {
            Ok(own) => Some(own),
            Err(Stop::Need(_)) if self.reads.stopped() => return Ok(None),
            Err(Stop::Need(_)) => None,
            Err(Stop::Error(err)) => return Err(err),
        };
        if let Some(own) = own
            && let Some(rhs) = scan_proj_clauses(
                own.types, own.clauses, None, at, an, aps, &chain, inert,
                &mut ex.out, &mut work, &mut visited, &mut scratch, MOST_PROJ_WORK, alloc,
            )?
        {
            return Ok(Some(rhs));
        }

        // Seed 2: associated-type item bounds and trait predicates along `chain`.
        let mut sub_args: Vec<PTy, Alloc<'a>> = Vec::new_in(alloc);
        for &anc in &chain {
            let PNode::Alias { trait_: sub_tr, name: sub_name, args: sub_list } = ex.out.view().node(anc)? else {
                continue;
            };
            sub_args.clear();
            sub_args.extend_from_slice(ex.out.view().list(sub_list)?);
            let def = match self.reads.trait_def(sub_tr) {
                Ok(def) => def,
                Err(Stop::Need(_)) if self.reads.stopped() => return Ok(None),
                Err(Stop::Need(_)) => None,
                Err(Stop::Error(err)) => return Err(err),
            };
            if let Some(def) = def {
                if let Some(rhs) = scan_proj_clauses(
                    def.types, def.supertraits, Some(&sub_args), at, an, aps, &chain, inert,
                    &mut ex.out, &mut work, &mut visited, &mut scratch, MOST_PROJ_WORK, alloc,
                )? {
                    return Ok(Some(rhs));
                }
                if let Some(assoc) = def.assoc_types.iter().find(|a| a.name == sub_name) {
                    let assoc_wc = match self.reads.predicates(assoc.item) {
                        Ok(wc) => Some(wc),
                        Err(Stop::Need(_)) if self.reads.stopped() => return Ok(None),
                        Err(Stop::Need(_)) => None,
                        Err(Stop::Error(err)) => return Err(err),
                    };
                    if let Some(wc) = assoc_wc
                        && let Some(rhs) = scan_proj_clauses(
                            wc.types, wc.clauses, Some(&sub_args), at, an, aps, &chain, inert,
                            &mut ex.out, &mut work, &mut visited, &mut scratch, MOST_PROJ_WORK, alloc,
                        )?
                    {
                        return Ok(Some(rhs));
                    }
                }
            }
            let tr_wc = match self.reads.predicates(sub_tr) {
                Ok(wc) => Some(wc),
                Err(Stop::Need(_)) if self.reads.stopped() => return Ok(None),
                Err(Stop::Need(_)) => None,
                Err(Stop::Error(err)) => return Err(err),
            };
            if let Some(wc) = tr_wc
                && let Some(rhs) = scan_proj_clauses(
                    wc.types, wc.clauses, Some(&sub_args), at, an, aps, &chain, inert,
                    &mut ex.out, &mut work, &mut visited, &mut scratch, MOST_PROJ_WORK, alloc,
                )?
            {
                return Ok(Some(rhs));
            }
        }

        // Transitive supertrait & trait-predicate elaboration.
        let mut next = 0usize;
        let mut cur_args: Vec<PTy, Alloc<'a>> = Vec::new_in(alloc);
        while let Some(&(cur_tr, cur_list)) = work.get(next) {
            next += 1;
            cur_args.clear();
            cur_args.extend_from_slice(ex.out.view().list(cur_list)?);
            let def = match self.reads.trait_def(cur_tr) {
                Ok(def) => def,
                Err(Stop::Need(_)) if self.reads.stopped() => return Ok(None),
                Err(Stop::Need(_)) => None,
                Err(Stop::Error(err)) => return Err(err),
            };
            if let Some(def) = def
                && let Some(rhs) = scan_proj_clauses(
                    def.types, def.supertraits, Some(&cur_args), at, an, aps, &chain, inert,
                    &mut ex.out, &mut work, &mut visited, &mut scratch, MOST_PROJ_WORK, alloc,
                )?
            {
                return Ok(Some(rhs));
            }
            let tr_wc = match self.reads.predicates(cur_tr) {
                Ok(wc) => Some(wc),
                Err(Stop::Need(_)) if self.reads.stopped() => return Ok(None),
                Err(Stop::Need(_)) => None,
                Err(Stop::Error(err)) => return Err(err),
            };
            if let Some(wc) = tr_wc
                && let Some(rhs) = scan_proj_clauses(
                    wc.types, wc.clauses, Some(&cur_args), at, an, aps, &chain, inert,
                    &mut ex.out, &mut work, &mut visited, &mut scratch, MOST_PROJ_WORK, alloc,
                )?
            {
                return Ok(Some(rhs));
            }
        }
        Ok(None)
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
        // Projection rows for GATs with type params or generic RPITITs carry `[Self, trait_args..,
        // assoc_or_fn_args..]`; impl selection matches only the trait's own `generics` arguments.
        let trait_arity = if name.is_some() { self.trait_generics(trait_)? } else { None };
        if self.reads.stopped() {
            return Ok(Selected::Unanswered);
        }
        let sel_ps = match trait_arity.and_then(|n| usize::try_from(n).ok()) {
            Some(n) if n > 0 && n < ps.len() => ps.get(..n).unwrap_or(ps),
            _ => ps,
        };
        let selected = if !ground(ex.out.view(), sel_ps, name.is_none(), alloc)? {
            // Even when `sel_ps` contains the body's own type parameters (e.g. `RChunksMut<'a, T>:
            // ExactSizeIterator`, `(A, B): Default`, `Foo<E>: DiscriminantKind`, `E: ToString`
            // via blanket impl), select the owning impl or builtin rule whenever no caller
            // where-clause can match `sel_ps`.
            let self_node = sel_ps.first().and_then(|&s| ex.out.view().node(s).ok());
            let try_impl = self_node.is_some_and(|n| !matches!(n, PNode::Infer | PNode::Error));
            let self_is_abstract = matches!(
                self_node,
                Some(PNode::Param(_) | PNode::ConstParam(_) | PNode::Opaque(..) | PNode::Dynamic(_) | PNode::Alias { .. })
            );
            let mut decided = None;
            if try_impl && !self.any_clause_may_match(ex.out.view(), sel_ps, trait_, inert)? {
                let mut cx = InferCtx::analysis_in(alloc);
                match crate::instance::select_owning_owner(&mut cx, &mut self.reads, self.body.owner, Some(rows.own), trait_, sel_ps, &mut ex.out) {
                    Ok(Some((imp, iargs))) => decided = Some(Selected::Impl { imp, args: iargs }),
                    Ok(None) | Err(ResolveError::NoImpl { .. }) if !self_is_abstract => {
                        decided = Some(Selected::Builtin);
                    }
                    _ => {}
                }
            }
            match decided {
                Some(s) => s,
                None => self.source_not_ground(ex.out.view(), sel_ps, trait_, name)?,
            }
        } else {
            // A where-clause of the caller may be the candidate that applies (`CallEnv`).
            let env = rows.env.all
                || rows.env.traits.scan_for(&trait_).is_some()
                || (rows.env.exact.scan_for(&trait_).is_some() && self.ground_clause_may_equal(ex.out.view(), sel_ps, trait_, inert)?);
            // Part of the body's own check, recorded for its borrow check: analysis.
            let mut cx = InferCtx::analysis_in(alloc);
            match crate::instance::select_owning(&mut cx, &mut self.reads, Some(rows.own), trait_, sel_ps, &mut ex.out) {
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

    /// How many generic arguments `trait_` declares (`Self` included), when its def is readable.
    fn trait_generics(&mut self, trait_: ItemKey) -> Result<Option<u32>, CheckError> {
        match self.reads.trait_def(trait_) {
            Ok(Some(def)) => Ok(Some(def.generics)),
            Ok(None) => Ok(None),
            Err(Stop::Need(_)) => Ok(None),
            Err(Stop::Error(err)) => Err(err),
        }
    }

    /// Whether `trait_` declares lifetime parameters (`TraitFlags::LIFETIME_PARAMS`): a trait
    /// like `Deserialize<'de>` or `Arbitrary<'a>` carries region slots even when `Self` is a
    /// primitive (`usize`, `bool`, `u64`) whose erased type holds no region.
    fn trait_has_lifetimes(&mut self, trait_: ItemKey) -> Result<bool, CheckError> {
        match self.reads.trait_def(trait_) {
            Ok(Some(def)) => Ok(def.flags.has(crate::world::TraitFlags::LIFETIME_PARAMS)),
            Ok(None) => Ok(false),
            Err(Stop::Need(_)) if self.reads.stopped() => Ok(false),
            Err(Stop::Need(_)) => Ok(true),
            Err(Stop::Error(err)) => Err(err),
        }
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
        // Supertraits of parameter-free clauses:
        // - From an alias-bearing clause (`env.traits`), or any non-`Self` supertrait predicate
        //   (`Self::Assoc: St`), the supertrait goes into `env.traits` (may match any ground row).
        // - From a plain ground clause (`env.exact`, e.g. `Signed<TxLegacy>: DeserializeOwned`)
        //   through a `Self: Super` chain (`DeserializeOwned: for<'de> Deserialize<'de>`), the
        //   supertrait stays in `env.exact` so `ground_clause_may_equal` matches it ONLY against
        //   rows whose `Self` may equal `Signed<TxLegacy>` (never shadowing `u32: Deserialize` or
        //   `Value: Deserialize`).
        let mut work: Vec<(ItemKey, bool), Alloc<'a>> = Vec::new_in(alloc);
        for &t in env.traits.iter() {
            work.push((t, false));
        }
        for &t in env.exact.iter() {
            if env.traits.scan_for(&t).is_none() {
                work.push((t, true));
            }
        }
        let mut next = 0usize;
        while let Some(&(t, exact_self)) = work.get(next) {
            if work.len() > MOST_ENV_TRAITS * 2 {
                return Err(CheckError::Limit(Limit::Solver));
            }
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
                let (PClause::Trait { trait_: st, args: sargs } | PClause::Projection { trait_: st, args: sargs, .. }) = *s else { continue };
                if inert.has(st) {
                    continue;
                }
                let self_is_param0 = def
                    .types
                    .list(sargs)
                    .ok()
                    .and_then(|l| l.first())
                    .and_then(|&a| def.types.node(a).ok())
                    .is_some_and(|n| matches!(n, PNode::Param(0)));
                let st_exact = exact_self && self_is_param0;
                if st_exact {
                    if env.traits.scan_for(&st).is_none() && env.exact.scan_for(&st).is_none() {
                        env.exact.push(st);
                    }
                } else if env.traits.scan_for(&st).is_none() {
                    env.traits.push(st);
                }
                if work.scan_for(&(st, st_exact)).is_none() {
                    work.push((st, st_exact));
                }
            }
        }
        Ok(env)
    }

    /// Whether a ground clause of the caller of `trait_` (or a subtrait whose `Self`-supertrait
    /// chain reaches `trait_`, [`CallEnv::exact`]) may equal the ground row `ps` (types of `tys`)
    /// up to regions, so may be the candidate rustc prefers.
    /// The caller's predicates were read by `call_env`; one not answered now may answer
    /// (never a wait at writeback).
    fn ground_clause_may_equal(&mut self, tys: PTys<'_>, ps: &[PTy], trait_: ItemKey, inert: Inert) -> Result<bool, CheckError> {
        let alloc = self.alloc;
        let own = match self.reads.predicates(self.body.owner) {
            Ok(own) => own,
            Err(Stop::Need(_)) => return Ok(true),
            Err(Stop::Error(err)) => return Err(err),
        };
        let Some(&ps0) = ps.first() else { return Ok(true) };
        for c in own.clauses {
            let (t, args) = match *c {
                PClause::Trait { trait_, args } | PClause::Projection { trait_, args, .. } => (trait_, args),
                _ => continue,
            };
            if names_param(own.types, args, alloc)? || holds_alias(own.types, args, alloc)? {
                continue;
            }
            let l = own.types.list(args)?;
            if t == trait_ {
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
            } else if let Some(&a0) = l.first()
                && may_equal(own.types, a0, tys, ps0, alloc)?
                && self.trait_has_self_super(t, trait_, inert)?
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Whether `target` is reachable from `start` via `Self: Super` (`Param(0)`) supertrait edges.
    fn trait_has_self_super(&mut self, start: ItemKey, target: ItemKey, inert: Inert) -> Result<bool, CheckError> {
        let alloc = self.alloc;
        let mut work: Vec<ItemKey, Alloc<'a>> = Vec::new_in(alloc);
        work.push(start);
        let mut next = 0usize;
        while let Some(&t) = work.get(next) {
            if work.len() > MOST_ENV_TRAITS {
                return Err(CheckError::Limit(Limit::Solver));
            }
            next += 1;
            let def = match self.reads.trait_def(t) {
                Ok(Some(def)) => def,
                Ok(None) => continue,
                Err(Stop::Need(_)) => return Ok(true),
                Err(Stop::Error(err)) => return Err(err),
            };
            for s in def.supertraits {
                let (PClause::Trait { trait_: st, args: sargs } | PClause::Projection { trait_: st, args: sargs, .. }) = *s else { continue };
                if inert.has(st) {
                    continue;
                }
                let self_is_param0 = def
                    .types
                    .list(sargs)
                    .ok()
                    .and_then(|l| l.first())
                    .and_then(|&a| def.types.node(a).ok())
                    .is_some_and(|n| matches!(n, PNode::Param(0)));
                if !self_is_param0 {
                    continue;
                }
                if st == target {
                    return Ok(true);
                }
                if work.scan_for(&st).is_none() {
                    work.push(st);
                }
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
        // Whether `own.clauses` carries any projection equality `<.. as at>::an = term` (which
        // could normalize a rigid alias to another type in this body's param-env).
        let has_proj_eq = |at: ItemKey, an: Name| -> bool {
            own.clauses.iter().any(|cl| matches!(*cl, PClause::Projection { trait_: pt, name: pn, .. } if pt == at && pn == an))
        };
        let ps0_is_concrete = !matches!(
            ps0_node,
            PNode::Param(_) | PNode::ConstParam(_) | PNode::Alias { .. } | PNode::Opaque(..) | PNode::Dynamic(_) | PNode::Infer | PNode::Error
        );
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
                (PNode::Param(_), PNode::Alias { trait_: at, name: an, args: p_args }) => {
                    // A clause `P: Tr` cannot match a rigid alias `<Q as Foo>::Assoc` (e.g.
                    // `I: IntoIterator` vs `I::IntoIter: IntoIterator` in `for x in iter.into_iter()`)
                    // unless a projection equality in `own.clauses` can equate that alias.
                    let rigid_param = tys.list(p_args).ok().and_then(|al| al.first()).and_then(|&s| tys.node(s).ok()).is_some_and(|n| matches!(n, PNode::Param(_)));
                    !rigid_param || has_proj_eq(at, an)
                }
                (PNode::Param(_), PNode::Opaque(..) | PNode::Infer | PNode::Error) => true,
                (PNode::Param(_), _) => false,
                (PNode::Alias { trait_: at, name: an, args: a_args }, _) if ps0_is_concrete => {
                    // A rigid alias `<P as Tr>::Assoc` on a type parameter cannot shadow a concrete
                    // type constructor's impl (e.g. `O::Len: PartialEq` vs `&O: PartialEq` or
                    // `P::Searcher<'a>: Clone` vs `MatchIndicesInternal<'a, P>: Clone`) unless a
                    // projection equality in `own.clauses` can equate that alias.
                    let rigid_param = own.types.list(a_args).ok().and_then(|al| al.first()).and_then(|&s| own.types.node(s).ok()).is_some_and(|n| matches!(n, PNode::Param(_)));
                    !rigid_param || has_proj_eq(at, an)
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
        while let Some(&t) = work.get(next) {
            if work.len() > MOST_ENV_TRAITS {
                return Err(CheckError::Limit(Limit::Solver));
            }
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

/// Exact structural equality of two erased portable types across two tables (including `Param`,
/// `ConstParam`, `Opaque`, `Alias`; never equating `Infer`/`Error`). Bounded by `MOST_NODES`.
fn exact_equal(at: PTys<'_>, a: PTy, bt: PTys<'_>, b: PTy, alloc: Alloc<'_>) -> Result<bool, CheckError> {
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
        let same_head = match (nx, ny) {
            (PNode::Infer | PNode::Error, _) | (_, PNode::Infer | PNode::Error) => false,
            (PNode::Param(p), PNode::Param(q)) => p == q,
            (PNode::ConstParam(p), PNode::ConstParam(q)) => p == q,
            (PNode::Opaque(p, _), PNode::Opaque(q, _)) => p == q,
            (PNode::Alias { trait_: t1, name: n1, .. }, PNode::Alias { trait_: t2, name: n2, .. }) => t1 == t2 && n1 == n2,
            (PNode::Dynamic(_), PNode::Dynamic(_)) => true,
            (PNode::ExTrait(t1, _), PNode::ExTrait(t2, _)) => t1 == t2,
            (PNode::ExProj { trait_: t1, name: n1, .. }, PNode::ExProj { trait_: t2, name: n2, .. }) => t1 == t2 && n1 == n2,
            (PNode::ExAuto(t1), PNode::ExAuto(t2)) => t1 == t2,
            _ => matches!((head(nx), head(ny)), (Some(hx), Some(hy)) if hx == hy),
        };
        if !same_head {
            return Ok(false);
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

/// Copy `root` from `src` into `out` preserving `Param` and `ConstParam` indices unchanged.
fn copy_into(src: PTys<'_>, root: PTy, out: &mut PortableTys<'_>, alloc: Alloc<'_>) -> Result<PTy, CheckError> {
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

/// Scan `clauses` (in `src`, optionally substituting `Param(i)` from `subst`) for either a
/// projection equality matching `(at, an, aps)` (returning its instantiated RHS in `out`), or
/// trait bounds whose `Self` argument belongs to `chain` (enqueueing them onto `work`).
#[allow(clippy::too_many_arguments)]
fn scan_proj_clauses<'a>(
    src: PTys<'_>,
    clauses: &[PClause],
    subst: Option<&[PTy]>,
    at: ItemKey,
    an: Name,
    aps: &[PTy],
    chain: &[PTy],
    inert: Inert,
    out: &mut PortableTys<'a>,
    work: &mut Vec<(ItemKey, List), Alloc<'a>>,
    visited: &mut HashMap<(ItemKey, List), (), Alloc<'a>>,
    scratch: &mut Vec<PTy, Alloc<'a>>,
    limit: usize,
    alloc: Alloc<'a>,
) -> Result<Option<PTy>, CheckError> {
    let sub_len = match subst {
        Some(s) => match u32::try_from(s.len()) {
            Ok(n) => n,
            Err(_) => return Ok(None),
        },
        None => 0,
    };
    for &clause in clauses {
        match clause {
            PClause::Projection { trait_: pt, name: pn, args: pargs, term } => {
                let Ok(plist) = src.list(pargs) else { continue };
                let Some(&p0) = plist.first() else { continue };
                if let Some(sub_args) = subst {
                    if !params_in_bounds(src, plist, sub_len, alloc)? {
                        continue;
                    }
                    let c0 = subst_into(src, p0, sub_args, out, alloc)?;
                    let want_proj = pt == at && pn == an && plist.len() == aps.len() && Some(&c0) == aps.first();
                    let want_work = !inert.has(pt) && work.len() < limit && chain.contains(&c0);
                    if !want_proj && !want_work {
                        continue;
                    }
                    scratch.clear();
                    scratch.push(c0);
                    for &p in plist.iter().skip(1) {
                        scratch.push(subst_into(src, p, sub_args, out, alloc)?);
                    }
                    if want_proj && scratch.as_slice() == aps && params_in_bounds(src, &[term], sub_len, alloc)? {
                        return Ok(Some(subst_into(src, term, sub_args, out, alloc)?));
                    }
                    if want_work {
                        let list = out.intern_list(scratch)?;
                        if visited.insert((pt, list), ()).is_none() {
                            work.push((pt, list));
                        }
                    }
                } else {
                    if pt == at && pn == an && plist.len() == aps.len() {
                        let mut all_eq = true;
                        for (&pa, &aa) in plist.iter().zip(aps.iter()) {
                            if !exact_equal(src, pa, out.view(), aa, alloc)? {
                                all_eq = false;
                                break;
                            }
                        }
                        if all_eq {
                            return Ok(Some(copy_into(src, term, out, alloc)?));
                        }
                    }
                    if !inert.has(pt) && work.len() < limit {
                        let mut in_chain = false;
                        for &ch in chain {
                            if exact_equal(src, p0, out.view(), ch, alloc)? {
                                in_chain = true;
                                break;
                            }
                        }
                        if in_chain {
                            scratch.clear();
                            for &p in plist {
                                scratch.push(copy_into(src, p, out, alloc)?);
                            }
                            let list = out.intern_list(scratch)?;
                            if visited.insert((pt, list), ()).is_none() {
                                work.push((pt, list));
                            }
                        }
                    }
                }
            }
            PClause::Trait { trait_: ct, args: cargs } => {
                if inert.has(ct) || work.len() >= limit {
                    continue;
                }
                let Ok(clist) = src.list(cargs) else { continue };
                let Some(&c0_raw) = clist.first() else { continue };
                if let Some(sub_args) = subst {
                    if !params_in_bounds(src, clist, sub_len, alloc)? {
                        continue;
                    }
                    let c0 = subst_into(src, c0_raw, sub_args, out, alloc)?;
                    if !chain.contains(&c0) {
                        continue;
                    }
                    scratch.clear();
                    scratch.push(c0);
                    for &p in clist.iter().skip(1) {
                        scratch.push(subst_into(src, p, sub_args, out, alloc)?);
                    }
                    let list = out.intern_list(scratch)?;
                    if visited.insert((ct, list), ()).is_none() {
                        work.push((ct, list));
                    }
                } else {
                    let mut in_chain = false;
                    for &ch in chain {
                        if exact_equal(src, c0_raw, out.view(), ch, alloc)? {
                            in_chain = true;
                            break;
                        }
                    }
                    if in_chain {
                        scratch.clear();
                        for &p in clist {
                            scratch.push(copy_into(src, p, out, alloc)?);
                        }
                        let list = out.intern_list(scratch)?;
                        if visited.insert((ct, list), ()).is_none() {
                            work.push((ct, list));
                        }
                    }
                }
            }
            _ => {}
        }
    }
    Ok(None)
}
