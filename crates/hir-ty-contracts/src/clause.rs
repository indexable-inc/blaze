//! What a callee's native clauses say, read from its HIR clause groups
//! (`hir_def::expr_store::lower::spec`): whether it has a `requires`, and the tightest literal
//! bound an `ensures` puts on its return.

use base_db::Db;
use hir_def::{
    DefWithBodyId, FunctionId,
    expr_store::{Body, ExpressionStore, SpecClauseKind, pretty::print_expr_hir},
    hir::{BinaryOp, BindingId, CmpOp, Expr, ExprId, Literal, Ordering, Pat},
    resolver::{HasResolver as _, Resolver, ValueNs},
};

use crate::ClauseHash;

/// A function's native contract, as far as the optimizer reads it.
pub(crate) struct Contract {
    /// It has a `requires` clause.
    pub(crate) has_requires: bool,
    /// The tightest bound an `ensures` item puts on the return, if one does.
    pub(crate) bound: Option<ReturnBound>,
}

/// `r < below`, where `r` is the function's named return: what an `ensures` item `r < K`
/// (`below = K`), `r <= K` (`below = K + 1`), `K > r` or `K >= r` says, with `K` an integer
/// literal. In a clause, `<` compares mathematical integers, so for an unsigned return this is
/// the machine value's bound.
pub(crate) struct ReturnBound {
    pub(crate) below: u128,
    pub(crate) clause: ClauseHash,
    /// The item, printed from HIR.
    pub(crate) text: String,
}

pub(crate) fn contract(db: &Db, func: FunctionId) -> Contract {
    let def = DefWithBodyId::FunctionId(func);
    let body = Body::of(db, def);
    let store = &body.store;
    let mut reader = Reader { db, def, store, resolver: def.resolver(db) };
    let mut contract = Contract { has_requires: false, bound: None };
    // A group's index among all the function's clause groups, in source order.
    for (group, &(expr, kind)) in store.spec_clauses().iter().enumerate() {
        match kind {
            SpecClauseKind::Requires => contract.has_requires = true,
            SpecClauseKind::Ensures => {
                // `|r: T| (e1, e2, ..)` for a named return; without a name no item can bound it.
                let Expr::Closure { args: &[ret], body: items, .. } = store.expr(expr) else {
                    continue;
                };
                let Pat::Bind { id: ret, subpat: None } = store.pat(ret) else { continue };
                let Expr::Tuple { exprs } = store.expr(items) else { continue };
                for (item, &e) in exprs.iter().enumerate() {
                    let Some(below) = reader.bound(e, ret) else { continue };
                    // The tightest; of equal ones, the first in source order.
                    if contract.bound.as_ref().is_some_and(|it| it.below <= below) {
                        continue;
                    }
                    let edition = syntax::Edition::CURRENT;
                    let text = print_expr_hir(db, store, def.into(), e, edition); // alloc: the clause's text, for its fact's span
                    let clause = ClauseHash::of(kind.keyword(), group as u32, item as u32, &text);
                    contract.bound = Some(ReturnBound { below, clause, text });
                }
            }
            SpecClauseKind::Recommends
            | SpecClauseKind::Decreases
            | SpecClauseKind::Invariant
            | SpecClauseKind::LoopEnsures
            | SpecClauseKind::LoopDecreases => {}
        }
    }
    contract
}

struct Reader<'db> {
    db: &'db Db,
    def: DefWithBodyId,
    store: &'db ExpressionStore,
    resolver: Resolver<'db>,
}

impl Reader<'_> {
    /// The exclusive upper bound `e` puts on the binding `ret`, if `e` is one of the four
    /// forms [`ReturnBound`] lists.
    fn bound(&mut self, e: ExprId, ret: BindingId) -> Option<u128> {
        let Expr::BinaryOp { lhs, rhs, op: Some(BinaryOp::CmpOp(CmpOp::Ord { ordering, strict })) } =
            self.store.expr(e)
        else {
            return None;
        };
        let (value, limit) = match ordering {
            Ordering::Less => (lhs, rhs),
            Ordering::Greater => (rhs, lhs),
        };
        if !self.names(value, ret) {
            return None;
        }
        let limit = self.literal(limit)?;
        match strict {
            true => Some(limit),
            false => limit.checked_add(1),
        }
    }

    /// Whether `e` is a path to the binding `ret`.
    fn names(&mut self, e: ExprId, ret: BindingId) -> bool {
        let Expr::Path(path) = self.store.expr(e) else { return false };
        let guard = self.resolver.update_to_inner_scope(self.db, self.def, e);
        let hygiene = self.store.expr_path_hygiene(e);
        let found = self.resolver.resolve_path_in_value_ns_fully(self.db, path, hygiene);
        self.resolver.reset_to_guard(guard);
        matches!(found, Some(ValueNs::LocalBinding(binding)) if binding == ret)
    }

    /// The value of a non-negative integer literal.
    fn literal(&self, e: ExprId) -> Option<u128> {
        let Expr::Literal(literal) = self.store.expr(e) else { return None };
        match literal {
            Literal::Uint(value, _) => Some(*value),
            Literal::Int(value, _) => u128::try_from(*value).ok(),
            Literal::String(_)
            | Literal::ByteString(_)
            | Literal::CString(_)
            | Literal::Char(_)
            | Literal::Bool(_)
            | Literal::Float(..) => None,
        }
    }
}
