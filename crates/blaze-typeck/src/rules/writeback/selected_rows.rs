//! Writeback of [`Checked::selected`](crate::Checked): each projection a callee's signature
//! named at a call, its arguments resolved and exported, and the impl that gives it its value
//! when the arguments are ground (crate::selected's module doc).

use blaze_own::{Alloc, Vec};

use super::Export;
use crate::columns::Closures;
use crate::diag::At;
use crate::error::{CheckError, Limit};
use crate::infer::InferCtx;
use crate::instance::{OwnClosures, ResolveError};
use crate::machine::{Machine, children};
use crate::portable::{PNode, PTy, PTys};
use crate::selected::{MOST_NODES, MOST_PROJECTED, Projected, Selected};
use crate::ty::TypeCtx;
use crate::world::World;

impl<'a, W: World, C: TypeCtx> Machine<'_, '_, '_, 'a, W, C> {
    /// The rows of `Checked::selected`, sorted by expression, each call's rows in the order its
    /// signature was lowered (numbered by `at`). A row whose arguments did not resolve adds no
    /// finding here (the body's own diagnostics say why) and is not kept: the borrow check
    /// finds fewer rows than hidden positions and refuses that call by name.
    /// `closures` is the record's `Checked::closures` (row `i` is closure `i`, its signature a
    /// type of `ex.out`): a closure of this body among a row's arguments (`iter.filter(|b| ..)
    /// .count()`) is selected with its facts from there, since the world holds no record of a
    /// body still being checked.
    pub(super) fn selected_rows(&mut self, ex: &mut Export<'a>, closures: &Closures<'a>) -> Result<Vec<Projected, Alloc<'a>>, CheckError> {
        let alloc = self.alloc;
        let own = OwnClosures { owner: self.body.owner, kinds: closures.column::<1>(), sigs: closures.column::<2>() };
        if self.projected.len() > MOST_PROJECTED {
            return Err(CheckError::Limit(Limit::Solver));
        }
        let mut out: Vec<Projected, Alloc<'a>> = Vec::with_capacity_in(self.projected.len(), alloc);
        let mut ps: Vec<PTy, Alloc<'a>> = Vec::new_in(alloc);
        for i in 0..self.projected.len() {
            let (e, trait_, name, args) = self.projected[i];
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
            let list = ex.out.intern_list(&ps)?;
            let selected = if !ground(ex.out.view(), &ps, alloc)? {
                Selected::Generic
            } else {
                // The one selection home for ground arguments, over the body's own reads.
                let mut cx = InferCtx::new_in(alloc);
                match crate::instance::select_owning(&mut cx, &mut self.reads, Some(own), trait_, &ps, &mut ex.out) {
                    Ok(Some((imp, iargs))) => Selected::Impl { imp, args: iargs },
                    Ok(None) => Selected::NotImpl,
                    Err(ResolveError::NoImpl { .. }) => Selected::NotImpl,
                    // A fact not read, or two impls (coherence refuses that elsewhere): not
                    // decided here; the borrow check refuses the call by name.
                    Err(ResolveError::Missing(_) | ResolveError::Overlap { .. }) => Selected::Unanswered,
                    Err(ResolveError::NotGround { .. }) => Selected::Generic,
                    Err(ResolveError::Check(err)) => return Err(err),
                }
            };
            out.push(Projected { expr: e, at: 0, trait_, name, args: list, selected });
        }
        // Stable: each call's rows keep their lowering order.
        out.sort_by_key(|r| r.expr);
        let mut at = 0u32;
        for i in 0..out.len() {
            at = if i > 0 && out[i - 1].expr == out[i].expr { at + 1 } else { 0 };
            out[i].at = at;
        }
        Ok(out)
    }
}

/// Whether `roots` name nothing the call does not fix: no type parameter, inference variable,
/// projection or opaque type (a bounded work list, no recursion).
fn ground(tys: PTys<'_>, roots: &[PTy], alloc: Alloc<'_>) -> Result<bool, CheckError> {
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
            PNode::Param(_) | PNode::Infer | PNode::Alias { .. } | PNode::Opaque(..) => return Ok(false),
            _ => children(node, tys, &mut stack)?,
        }
    }
    Ok(true)
}
