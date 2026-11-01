//! The imports a crate's code uses, its type inference's included: rustc's
//! `used_trait_imports` on top of the uses of its paths.

use base_db::{Crate, Db};
use blaze_hash::HashMap;
use hir_def::{
    expr_store::{ExpressionStore, StoreVisitor, scope::ExprScopes},
    hir::{ExprId, ExprOrPatId, PatId},
    nameres::{
        demand::crate_body_owners,
        uses::{UsedImports, used_imports_with},
    },
    resolver::resolver_for_scope,
};

use crate::{InferBodyId, InferenceResult};

#[cfg(test)]
mod tests;

/// [`hir_def::nameres::uses::used_imports`] with the imports of each trait a method call or an
/// associated item path of a body resolved through ([`InferenceResult::traits_in_scope_used`]),
/// found from the scope of the expression or pattern that picked it, as rustc's `lookup_method`
/// and `resolve_fully_qualified_call` mark `TraitCandidate::import_ids`.
#[blaze_query::tracked(returns(ref), mem_tag = Infer)]
pub fn used_imports_with_traits(db: &Db, krate: Crate) -> UsedImports {
    // alloc: the query's result's input, once per execution.
    let mut uses = Vec::new();
    for owner in crate_body_owners(db, krate) {
        let picks = InferenceResult::of(db, InferBodyId::DefWithBodyId(owner)).traits_in_scope_used();
        if picks.is_empty() {
            continue;
        }
        let scopes = ExprScopes::of(db, owner);
        let store = ExpressionStore::of(db, owner.into());
        let mut pattern_owners = None;
        for &(at, trait_) in picks {
            let expr = match at {
                ExprOrPatId::ExprId(expr) => Some(expr),
                ExprOrPatId::PatId(pat) => {
                    pattern_owners.get_or_insert_with(|| owners_of_patterns(store)).get(&pat).copied()
                }
            };
            let scope = expr.and_then(|expr| scopes.scope_for(expr));
            resolver_for_scope(db, owner, scope).record_trait_uses(db, trait_, &mut uses);
        }
    }
    used_imports_with(db, krate, &uses)
}

/// The expression each pattern of `store` is written in (a `let`'s block, a `match`, a closure,
/// a `let` expression), whose scope holds the items the pattern sees. A parameter's pattern has
/// none: it sees the body's own scope. Built only for a body with a pattern that picked a trait.
fn owners_of_patterns(store: &ExpressionStore) -> HashMap<PatId, ExprId> {
    struct Patterns<'a> {
        store: &'a ExpressionStore,
        owner: ExprId,
        owners: &'a mut HashMap<PatId, ExprId>,
    }

    impl StoreVisitor for Patterns<'_> {
        fn on_pat(&mut self, pat: PatId) {
            let (owner, owners) = (self.owner, &mut *self.owners);
            self.store.walk_pats(pat, &mut |it| {
                owners.insert(it, owner);
            });
        }
    }

    // alloc: once per body with a pattern path resolved through a trait in scope, which is rare.
    let mut owners = HashMap::default();
    for (expr, _) in store.exprs() {
        store.visit_expr_children(expr, Patterns { store, owner: expr, owners: &mut owners });
    }
    owners
}
