//! The imports a crate's code uses, its type inference's included: rustc's
//! `used_trait_imports` on top of the uses of its paths.

use base_db::{Crate, Db};
use blaze_hash::HashMap;
use hir_def::{
    DefWithBodyId, HasModule,
    expr_store::{ExpressionStore, StoreVisitor, scope::ExprScopes},
    hir::{Expr, ExprId, ExprOrPatId, PatId},
    nameres::{
        demand::crate_body_owners,
        uses::{Use, UsedImports, every_trait_use, used_imports_with},
    },
    resolver::resolver_for_scope,
};
use std::alloc::Global;

use crate::{InferBodyId, InferenceResult};

#[cfg(test)]
mod tests;

/// [`hir_def::nameres::uses::used_imports`] with the imports of each trait a method call or an
/// associated item path of a body resolved through ([`body_trait_uses`]). The fold over the
/// crate: it reads each body's record, never a body, so a body edit that changes none of the
/// traits it picks leaves every record equal and this does not run again.
#[blaze_query::tracked(returns(ref), mem_tag = Infer)]
pub fn used_imports_with_traits(db: &Db, krate: Crate) -> UsedImports {
    // alloc: the query's result's input, once per execution.
    let mut uses = Vec::new();
    for owner in crate_body_owners(db, krate) {
        uses.extend_from_slice(body_trait_uses(db, owner));
    }
    used_imports_with(db, krate, &uses)
}

/// The imports of each trait a method call or an associated item path of the body of `owner`
/// resolved through ([`InferenceResult::traits_in_scope_used`]), found from the scope of the
/// expression or pattern that picked it, as rustc's `lookup_method` and
/// `resolve_fully_qualified_call` mark `TraitCandidate::import_ids`; and for a method call
/// type checking resolved to nothing, those of every binding of every trait in scope with a
/// value item of its name ([`every_trait_use`]), since rustc may pick any of them (no import a
/// failed call may read is linted). One record per body.
#[blaze_query::tracked(returns(deref), mem_tag = Infer)]
pub(crate) fn body_trait_uses(db: &Db, owner: DefWithBodyId) -> Box<[Use]> {
    let infer = InferenceResult::of(db, InferBodyId::DefWithBodyId(owner));
    let picks = infer.traits_in_scope_used();
    let store = ExpressionStore::of(db, owner.into());
    let failed = || {
        store.exprs().filter_map(|(expr, it)| match it {
            Expr::MethodCall { method_name, .. } if infer.method_resolution(expr).is_none() => {
                Some((expr, *method_name))
            }
            _ => None,
        })
    };
    if picks.is_empty() && failed().next().is_none() {
        // alloc: an empty box allocates nothing
        return Box::default();
    }
    // alloc: the record, once per execution
    let mut uses = Vec::new();
    let scopes = ExprScopes::of(db, owner);
    let mut pattern_owners = None;
    for &(at, trait_) in picks {
        let expr = match at {
            ExprOrPatId::ExprId(expr) => Some(expr),
            ExprOrPatId::PatId(pat) => pattern_owners
                .get_or_insert_with(|| owners_of_patterns(store))
                .get(&pat)
                .copied(),
        };
        let scope = expr.and_then(|expr| scopes.scope_for(expr));
        resolver_for_scope(db, owner, scope).record_trait_uses(db, trait_, &mut uses);
    }
    let krate = owner.krate(db);
    for (expr, name) in failed() {
        let module = resolver_for_scope(db, owner, scopes.scope_for(expr)).module();
        every_trait_use(db, krate, module, &name, &mut uses);
    }
    uses.into_boxed_slice()
}

/// The expression each pattern of `store` is written in (a `let`'s block, a `match`, a closure,
/// a `let` expression), whose scope holds the items the pattern sees. A parameter's pattern has
/// none: it sees the body's own scope. Built only for a body with a pattern that picked a trait.
fn owners_of_patterns(store: &ExpressionStore) -> HashMap<PatId, ExprId, Global> {
    struct Patterns<'a> {
        store: &'a ExpressionStore,
        owner: ExprId,
        owners: &'a mut HashMap<PatId, ExprId, Global>,
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
    let mut owners = HashMap::new_in(Global);
    for (expr, _) in store.exprs() {
        store.visit_expr_children(expr, Patterns { store, owner: expr, owners: &mut owners });
    }
    owners
}
