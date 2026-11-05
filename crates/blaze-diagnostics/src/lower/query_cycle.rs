//! E0391 "cycle detected when computing the super clauses of `Tr`": computing a query of an
//! item needed itself. Each item in the cycle reports one, at itself.
//!
//! Follows rustc's cycle error (`compiler/rustc_query_system/src/query/job.rs`:
//! `report_cycle`) and the descriptions of its queries (`compiler/rustc_middle/src/query/mod.rs`)
//! (MIT or Apache-2.0).

use hir::CycleQuery;

use crate::{
    cx::Cx,
    lower::{Found, Phase},
    model::Diag,
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::QueryCycle) -> Vec<Found> {
    let span = cx.span_of(d.node);
    let item = &d.item;
    let message = match d.query {
        // alloc: the error's text, for an error hir found
        CycleQuery::Predicates => format!(
            "cycle detected when computing predicates of `{item}`: its where clauses need themselves"
        ),
        // alloc: as above
        CycleQuery::SuperClauses | CycleQuery::Supertraits => format!(
            "cycle detected when computing the super clauses of `{item}`: its supertraits need themselves"
        ),
        // alloc: as above
        CycleQuery::ImpliedClauses => format!(
            "cycle detected when computing the implied clauses of `{item}`: its `Self` and associated type bounds need themselves"
        ),
        CycleQuery::ImplSelfTy => {
            "cycle detected when computing type of `impl`: its self type needs itself".to_owned() // alloc: as above
        }
        CycleQuery::ImplTrait => {
            "cycle detected when computing trait implemented by `impl`: its trait needs itself"
                .to_owned() // alloc: as above
        }
    };
    let diag = Diag::error(Some("E0391"), message, span);
    // alloc: one error, for an error hir found
    vec![Found::new(diag, Phase::Items)]
}
