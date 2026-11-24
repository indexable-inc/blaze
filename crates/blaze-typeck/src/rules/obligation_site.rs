//! Where an unmet obligation is filed: at the narrowest piece of text that introduced it
//! (piece obligation-range; NEVER COMPILED when written, 2026-11-16).
//!
//! The rule, one for every obligation an item's where-clauses owe at a use of the item (a value
//! path or a method call, `register_predicates`):
//!
//! 1. the clause's subject is the first generic parameter it mentions, its self type first
//!    (`T` of `T: Tr`, of `u8: From<T>` too; a type or a const parameter, both indexed in the
//!    item's generics, parents first);
//! 2. a parameter written as a generic argument is filed at THAT ARGUMENT (`Io` of
//!    `grant::<Io>(..)`, of `x.check::<Io>(..)`, of `Param::of::<Io>(..)`): the item's own
//!    parameter `parent_count + k` is the k-th type of the last segment's (or the method call's)
//!    turbofish, as `explicit_fn_args` substitutes them;
//! 3. a trait item's `Self` written as a qualified path's anchor is filed at the anchor's type
//!    (`Io` of `<Io as Grant>::KIND`);
//! 4. a parameter an input of the callee mentions is filed at the argument expression passed
//!    for that input (the first such input): `need(&b)` for `fn need<T: Show>(t: &T)` at `&b`;
//!    a method call's input 0 is its receiver, so a bound on the receiver's type parameter is
//!    filed at the receiver;
//! 5. otherwise (a where-clause on an impl whose parameter no input and no written argument
//!    names) at the segment that names the item: the path's last segment, the method call's
//!    name;
//! 6. the whole expression only when none of these pieces of text is known (a body lowered
//!    without the range columns, an expansion's own text, a desugaring's lang-item path, which
//!    the user did not write: its expression is the operator's).
//!
//! The place is decided when the obligation is reported unmet, never when it is registered: the
//! cause is `Cause::Clause { expr, nth }` (8 bytes, Copy), the common path registers exactly what
//! it did, and only a failed obligation reads the item's clauses and signature again (both
//! already on the body's read list: `register_predicates` read them). An AMBIGUOUS obligation
//! keeps the whole expression: what is missing there is an annotation, not a bound.
//!
//! The ranges of written types and segments are the body's columns `type_list_ranges`,
//! `segment_ranges`, `method_name_ranges` and `Anchor::ty_range` (body.rs); an empty range, or
//! an index past a column, means "no range known", and the rule goes on to the next piece.

use blaze_own::{Alloc, Vec};

use crate::body::Expr;
use crate::diag::At;
use crate::error::{CheckError, Malformed, Stop};
use crate::ids::{ExprId, ItemKey, List, Range};
use crate::machine::{Machine, children};
use crate::portable::{PNode, PTy, PTys};
use crate::ty::{Cause, TypeCtx};
use crate::world::{Def, PClause, World};

/// The most type nodes (counted with repeats) the search for a clause's subject, or for the
/// input that mentions it, visits. Where-clauses are a few nodes; past this bound the search
/// stops and the clause is placed by the next piece of the rule (deterministically: the same
/// clause always stops at the same node).
const MAX_SEARCH_NODES: u32 = 1024;

/// The arguments of the call an item was named by.
#[derive(Clone, Copy, Debug)]
struct CallArgs {
    /// A method call's receiver, which is input 0.
    receiver: Option<ExprId>,
    /// The other arguments, in `BodyView::expr_lists`.
    rest: List,
}

/// The pieces of text one use of an item (a value path or a method call) wrote.
#[derive(Clone, Copy, Debug)]
struct ClauseUse {
    item: ItemKey,
    /// The turbofish naming the item's own declared parameters (in `BodyView::type_lists`).
    written: Option<List>,
    /// A qualified path's anchor type, when the anchor names a trait (`<T as Tr>::x`).
    anchor: Option<Range>,
    args: Option<CallArgs>,
    /// The segment, or the method name, that names the item.
    naming: Option<Range>,
}

/// `r` when it is a range of text (not empty).
fn known(r: Range) -> Option<Range> {
    (r.end > r.start).then_some(r)
}

/// The first generic parameter (type or const) a clause mentions: its arguments in order (the
/// self type first), then a projection's term; `None` when it mentions none, or past
/// [`MAX_SEARCH_NODES`].
fn subject(clause: PClause, types: PTys<'_>, alloc: Alloc<'_>) -> Result<Option<u32>, CheckError> {
    let (first, list, last) = match clause {
        PClause::Trait { args, .. } | PClause::Const { args, .. } => (None, args, None),
        PClause::Projection { args, term, .. } => (None, args, Some(term)),
        PClause::ForFields { container, args, .. } => (Some(container), args, None),
        PClause::TypeOutlives { ty, .. } => (None, List::EMPTY, Some(ty)),
        PClause::RegionOutlives { .. } => (None, List::EMPTY, None),
    };
    let mut stack: Vec<PTy, Alloc<'_>> = Vec::new_in(alloc);
    let mut budget = MAX_SEARCH_NODES;
    for &top in first.iter().chain(types.list(list)?.iter()).chain(last.iter()) {
        stack.clear();
        stack.push(top);
        while let Some(t) = stack.pop() {
            let Some(left) = budget.checked_sub(1) else { return Ok(None) };
            budget = left;
            match types.node(t)? {
                PNode::Param(i) | PNode::ConstParam(i) => return Ok(Some(i)),
                node => children(node, types, &mut stack)?,
            }
        }
    }
    Ok(None)
}

/// Whether `t` mentions generic parameter `param` (within [`MAX_SEARCH_NODES`]).
fn mentions(t: PTy, param: u32, types: PTys<'_>, alloc: Alloc<'_>) -> Result<bool, CheckError> {
    let mut stack: Vec<PTy, Alloc<'_>> = Vec::new_in(alloc);
    stack.push(t);
    let mut budget = MAX_SEARCH_NODES;
    while let Some(t) = stack.pop() {
        let Some(left) = budget.checked_sub(1) else { return Ok(false) };
        budget = left;
        match types.node(t)? {
            PNode::Param(i) | PNode::ConstParam(i) if i == param => return Ok(true),
            node => children(node, types, &mut stack)?,
        }
    }
    Ok(false)
}

impl<W: World, C: TypeCtx> Machine<'_, '_, '_, '_, W, C> {
    /// Whether the control `suite::obligation_range::control_whole_expression_fails_all_four`
    /// planted its bug: every obligation placed at the whole expression.
    fn whole_planted(&self) -> bool {
        #[cfg(test)]
        let planted = self.plant == crate::machine::Plant::ClauseAtWholeExpr;
        #[cfg(not(test))]
        let planted = false;
        planted
    }

    /// The cause of an obligation that the written text `r` introduced (a qualified path's
    /// anchor type), or `cause` when `r` is no range of text.
    pub(crate) fn written_cause(&self, r: Range, cause: Cause) -> Cause {
        match known(r) {
            Some(r) if !self.whole_planted() => Cause::Written(r),
            _ => cause,
        }
    }

    /// Where the unmet `nth` where-clause of the item `expr` names is filed (the module's rule).
    pub(crate) fn clause_at(&mut self, expr: ExprId, nth: u32) -> Result<At, Stop> {
        let whole = At::Expr(expr);
        if self.whole_planted() {
            return Ok(whole);
        }
        let Some(u) = self.clause_use(expr)? else { return Ok(whole) };
        let preds = self.reads.predicates(u.item)?;
        let Some(&clause) = preds.clauses.get(nth as usize) else {
            let len = u32::try_from(preds.clauses.len()).unwrap_or(u32::MAX);
            return Err(CheckError::Malformed(Malformed::List { column: "predicates", start: nth, len }).into());
        };
        let Some(param) = subject(clause, preds.types, self.alloc)? else {
            return Ok(u.naming.map_or(whole, At::Written));
        };
        let sig = self.reads.fn_sig(u.item)?;
        // 2. A generic argument written for the parameter.
        if let (Some(sig), Some(list)) = (&sig, u.written) {
            if let Some(k) = param.checked_sub(sig.parent_count) {
                if list.len == sig.declared_count && k < list.len {
                    if let Some(r) = self.body.type_arg_range(list.start + k) {
                        return Ok(At::Written(r));
                    }
                }
            }
        }
        // 3. A trait item's `Self` written as the anchor.
        if param == 0 {
            if let Some(r) = u.anchor {
                return Ok(At::Written(r));
            }
        }
        // 4. The argument passed for the first input that mentions the parameter.
        if let (Some(sig), Some(args)) = (&sig, u.args) {
            for (j, &input) in sig.inputs.iter().enumerate() {
                if mentions(input, param, sig.types, self.alloc)? {
                    if let Some(arg) = self.arg_for(args, j)? {
                        return Ok(At::Expr(arg));
                    }
                    break;
                }
            }
        }
        // 5. The segment, or the method name, that names the item.
        Ok(u.naming.map_or(whole, At::Written))
    }

    /// The pieces of text the use `expr` wrote, or `None` for an expression that names no item
    /// whose where-clauses were registered at it (and for a desugaring's lang-item path).
    fn clause_use(&self, expr: ExprId) -> Result<Option<ClauseUse>, Stop> {
        match self.body.expr(expr)? {
            Expr::Path(p) => {
                let bp = self.body.path(p)?;
                if bp.lang.is_some() {
                    return Ok(None);
                }
                let item = self.defs.iter().rev().find_map(|&(at, d)| match (at, d) {
                    (At::Expr(x), Def::Fn(k) | Def::Const(k) | Def::Static(k, _)) if x == expr => Some(k),
                    _ => None,
                });
                let Some(item) = item else { return Ok(None) };
                let segs = self.body.segment_list(bp.segments)?;
                let last = segs.last().copied().map(|s| (bp.segments.start + bp.segments.len - 1, s));
                let written = last.and_then(|(_, s)| s.has_args.then_some(s.generic_args));
                let naming = last.and_then(|(ix, _)| self.body.segment_range(ix));
                let anchor = bp.anchor.filter(|a| a.trait_.is_some()).and_then(|a| known(a.ty_range));
                let args = self.call_args_of(expr).map(|rest| CallArgs { receiver: None, rest });
                Ok(Some(ClauseUse { item, written, anchor, args, naming }))
            }
            Expr::MethodCall { receiver, generic_args, args, .. } => {
                let Some(item) = self.callees.iter().rev().find(|c| c.0 == expr).map(|c| c.1) else { return Ok(None) };
                Ok(Some(ClauseUse {
                    item,
                    written: (generic_args.len > 0).then_some(generic_args),
                    anchor: None,
                    args: Some(CallArgs { receiver: Some(receiver), rest: args }),
                    naming: self.body.method_name_range(expr),
                }))
            }
            _ => Ok(None),
        }
    }

    /// The arguments of the call whose callee is `callee` itself (`f(a, b)` for the path `f`).
    /// A scan of the body's expressions: it runs only for an unmet obligation.
    fn call_args_of(&self, callee: ExprId) -> Option<List> {
        self.body.exprs.iter().find_map(|x| match *x {
            Expr::Call { callee: c, args } if c == callee => Some(args),
            _ => None,
        })
    }

    /// The expression passed for input `j` of the callee.
    fn arg_for(&self, args: CallArgs, j: usize) -> Result<Option<ExprId>, Stop> {
        let rest = self.body.expr_list(args.rest)?;
        Ok(match (args.receiver, j) {
            (Some(r), 0) => Some(r),
            (Some(_), j) => rest.get(j - 1).copied(),
            (None, j) => rest.get(j).copied(),
        })
    }
}
