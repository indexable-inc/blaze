//! `seqcst_unexplained`: a use of `Ordering::SeqCst` (`core::sync::atomic`'s) with no
//! `// seqcst: <why>` comment on its line or the line above. `SeqCst` puts the operation in one
//! total order with every other `SeqCst` operation of the program, which costs a full fence on
//! most targets; it is needed only where some thread reasons about that total order (Dekker's
//! store-then-load on two flags, an IRIW reader). Most uses want acquire/release: a load that
//! sees a store sees what came before it.
//!
//! The path is resolved, so `use Ordering::SeqCst as S` and `use Ordering::*` are found, and a
//! `SeqCst` a macro wrote is placed at the call. Only expressions count: a pattern
//! `Ordering::SeqCst =>` in a `match` over an ordering names it, it does not order anything.
//! There is no fix: only a person knows whether a total order is needed. The note says what
//! each operation takes instead: a `load` `Acquire`, a `store` `Release`, a read-modify-write
//! (`swap`, `fetch_*`, the success ordering of `compare_exchange`) `AcqRel`, the failure
//! ordering of `compare_exchange` and the fetch ordering of `fetch_update` `Acquire`, a `fence`
//! `AcqRel`.

use base_db::Db;
use hir_def::{
    DefWithBodyId, EnumVariantId, Lookup as _, VariantId,
    expr_store::{Body, scope::ExprScopes},
    hir::{Expr, ExprId},
    resolver::{ValueNs, resolver_for_scope},
    signatures::FunctionSignature,
};

use super::{
    Finding, Lint,
    alloc::written_callee,
    exempt,
    facts::{self, Parents},
};
use crate::check::Cx;

const LINT: Lint = Lint::SeqcstUnexplained;

pub(super) fn check(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    let found = find(cx);
    facts::report(cx, found);
}

fn find(cx: &Cx<'_, '_>) -> Vec<Finding> {
    // alloc: the findings of one body, reported at the edge
    let mut found = Vec::new();
    let Some(analysis) = cx.analysis() else { return found };
    let Ok(def) = DefWithBodyId::try_from(analysis.def()) else { return found };
    let db = cx.db();
    let body = analysis.body();
    let infer = analysis.infer();
    let mut parents = None;
    for (id, expr) in body.exprs() {
        if !matches!(expr, Expr::Path(_)) {
            continue;
        }
        let variant = match infer.variant_resolution_for_expr(id) {
            Some(VariantId::EnumVariantId(it)) => Some(it),
            _ => variant_of(db, def, body, id),
        };
        let Some(variant) = variant else { continue };
        if !is_seq_cst(db, variant) {
            continue;
        }
        let Some(range) = analysis.expr_range(id) else { continue };
        if exempt::exempt(cx.text(), range, LINT) {
            continue;
        }
        let parents = parents.get_or_insert_with(|| Parents::of(body));
        let (operation, instead) = match used_by(db, body, infer, parents, id) {
            Some((name, at)) => {
                let instead = instead(&name, at);
                (Some(name), instead)
            }
            None => (None, None),
        };
        let message = match &operation {
            Some(name) => format!(
                "this `{name}` orders by `Ordering::SeqCst` with no `// seqcst: <why>` saying why it needs a total order"
            ),
            None => {
                "`Ordering::SeqCst` with no `// seqcst: <why>` saying why it needs a total order"
                    .to_owned()
            } // alloc: a message's part
        };
        let note = match (&operation, instead) {
            (Some(name), Some(instead)) => format!(
                "where no thread reasons about one order of every `SeqCst` operation, `{name}` takes `Ordering::{instead}`; \
                 `SeqCst` costs a full fence on most targets"
            ),
            _ => "where no thread reasons about one order of every `SeqCst` operation, a load takes `Acquire`, a store \
                  `Release` and a read-modify-write `AcqRel`; `SeqCst` costs a full fence on most targets"
                .to_owned(), // alloc: a message's part
        };
        facts::push(&mut found, LINT, range, message, Some(note));
    }
    found
}

/// The enum variant the path expression `expr` names, through the scope it is in (a `use`
/// rename or glob included).
fn variant_of(db: &Db, def: DefWithBodyId, body: &Body, expr: ExprId) -> Option<EnumVariantId> {
    let Expr::Path(path) = body.expr(expr) else { return None };
    let scope = ExprScopes::of(db, def).scope_for(expr);
    match resolver_for_scope(db, def, scope).resolve_path_in_value_ns_fully(
        db,
        path,
        body.expr_path_hygiene(expr),
    ) {
        Some(ValueNs::EnumVariantId(it)) => Some(it),
        _ => None,
    }
}

/// Whether `variant` is `SeqCst` of `core::sync::atomic::Ordering` (std's re-export is the
/// same item).
fn is_seq_cst(db: &Db, variant: EnumVariantId) -> bool {
    if hir::EnumVariant::from(variant).name(db).as_str() != "SeqCst" {
        return false;
    }
    let enum_ = hir::Enum::from(variant.lookup(db).parent);
    let module = enum_.module(db);
    enum_.name(db).as_str() == "Ordering"
        && module.name(db).is_some_and(|it| it.as_str() == "atomic")
        && ["core", "std"].iter().any(|it| facts::crate_is(db, module.krate(db), it))
}

/// The function `expr` is an argument of, by name, and the argument's place among the
/// arguments (the receiver not counted).
fn used_by<'db>(
    db: &'db Db,
    body: &Body,
    infer: &hir_ty::InferenceResult<'db>,
    parents: &Parents,
    expr: ExprId,
) -> Option<(String, usize)> {
    let parent = parents.of_expr(expr)?;
    let at = match body.expr(parent) {
        Expr::MethodCall { args, .. } | Expr::Call { args, .. } => {
            args.iter().position(|&it| it == expr)?
        }
        _ => return None,
    };
    let (f, _) = written_callee(infer, body, parent)?;
    // alloc: a message's part
    Some((FunctionSignature::of(db, f).name.as_str().to_owned(), at))
}

/// The ordering the `at`th argument of `operation` takes where no total order is needed.
fn instead(operation: &str, at: usize) -> Option<&'static str> {
    Some(match operation {
        "load" => "Acquire",
        "store" => "Release",
        "fence" | "compiler_fence" => "AcqRel",
        // `(current, new, success, failure)`: the failure ordering reads only.
        "compare_exchange" | "compare_exchange_weak" => match at {
            3 => "Acquire",
            _ => "AcqRel",
        },
        // `(set_order, fetch_order, f)`: the fetch ordering reads only.
        "fetch_update" => match at {
            1 => "Acquire",
            _ => "AcqRel",
        },
        "swap" | "compare_and_swap" => "AcqRel",
        _ if operation.starts_with("fetch_") => "AcqRel",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::super::perf_tests::perf_only;

    const LINT: &str = "seqcst_unexplained";
    const FOUND: &str = "with no `// seqcst: <why>`";

    #[test]
    fn seqcst_with_no_reason_is_found_through_renames_with_the_ordering_to_take() {
        let found = perf_only(
            LINT,
            r#"
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::atomic::Ordering::SeqCst as Total;
pub fn bump(a: &AtomicU64) -> u64 {
    a.store(1, Ordering::SeqCst);
    let seen = a.load(Total);
    a.fetch_add(1, Ordering::SeqCst);
    seen
}
"#,
        );
        assert_eq!(found.matches(FOUND).count(), 3, "{found}");
        assert!(found.contains("this `store` orders by `Ordering::SeqCst`"), "{found}");
        assert!(found.contains("`store` takes `Ordering::Release`"), "{found}");
        assert!(
            found.contains("`load` takes `Ordering::Acquire`"),
            "the renamed `SeqCst` is resolved:\n{found}"
        );
        assert!(found.contains("`fetch_add` takes `Ordering::AcqRel`"), "{found}");
    }

    #[test]
    fn a_reason_on_the_line_or_above_keeps_it_and_other_orderings_are_left_alone() {
        let found = perf_only(
            LINT,
            r#"
use std::sync::atomic::{AtomicU64, Ordering};
pub fn kept(a: &AtomicU64, b: &AtomicU64) -> u64 {
    // seqcst: Dekker: each thread stores its flag, then loads the other's
    a.store(1, Ordering::SeqCst);
    let seen = b.load(Ordering::SeqCst); // seqcst: the other half of the pair above
    a.store(2, Ordering::Release);
    seen + b.load(Ordering::Acquire)
}
pub fn named(o: Ordering) -> u32 {
    match o {
        Ordering::SeqCst => 1,
        _ => 0,
    }
}
"#,
        );
        assert_eq!(found.matches(FOUND).count(), 0, "{found}");
    }

    #[test]
    fn compare_exchange_takes_acqrel_on_success_and_acquire_on_failure() {
        let found = perf_only(
            LINT,
            r#"
use std::sync::atomic::{AtomicU64, Ordering::SeqCst};
pub fn swap_in(a: &AtomicU64) -> bool {
    a.compare_exchange(0, 1, SeqCst, SeqCst);
    true
}
"#,
        );
        assert_eq!(found.matches(FOUND).count(), 2, "{found}");
        assert!(found.contains("`compare_exchange` takes `Ordering::AcqRel`"), "{found}");
        assert!(found.contains("`compare_exchange` takes `Ordering::Acquire`"), "{found}");
    }
}
