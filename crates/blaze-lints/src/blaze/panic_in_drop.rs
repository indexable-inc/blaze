//! `panic_in_drop`: an `impl Drop` whose `drop` can panic: `unwrap` or `expect` (of a `Result`
//! a call returned, or of an `Option`), `panic!` and the macros that expand to it, an index, or
//! an integer `/` or `%` by a divisor that may be zero ([`general_facts::panic_sites`]). `drop`
//! runs while a panic unwinds too; a second panic then aborts the process, and the first
//! panic's message is the only trace left. Outside unwinding, a panicking `drop` leaks what the
//! rest of it would have freed.
//!
//! A body that calls `std::thread::panicking()` anywhere is taken as handling the unwinding
//! case and is not checked. Narrowed as `unwind_across_ffi` is: from what the body writes, not
//! from the bodies it calls, and arithmetic overflow is not counted.

use hir::AsAssocItem as _;
use hir_def::DefWithBodyId;

use super::{Finding, Lint, facts, general_facts};
use crate::check::Cx;

const LINT: Lint = Lint::PanicInDrop;

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
    let Ok(DefWithBodyId::FunctionId(f)) = DefWithBodyId::try_from(analysis.def()) else {
        return found;
    };
    let db = cx.db();
    let Some(drop) = cx.lang_trait(hir::LangItem::Drop) else { return found };
    let Some(hir::AssocItemContainer::Impl(imp)) =
        hir::Function::from(f).as_assoc_item(db).map(|it| it.container(db))
    else {
        return found;
    };
    if imp.trait_(db) != Some(drop) || hir::Function::from(f).name(db).as_str() != "drop" {
        return found;
    }
    if general_facts::calls_std(db, analysis, "thread", "panicking") {
        return found;
    }
    let target = cx.krate().to_display_target(db);
    let of = {
        use hir::HirDisplay as _;
        imp.self_ty(db).display(db, target).to_string()
    };
    for site in general_facts::panic_sites(db, analysis) {
        let Some(range) = analysis.expr_range(site.expr) else { continue };
        let message = format!("{} can panic in `Drop::drop` of `{of}`", site.what);
        let note = "`drop` also runs while a panic unwinds, and a second panic then aborts the process; \
                    handle the error (log it, or keep it for an explicit `close` method that returns it), or \
                    skip the work when `std::thread::panicking()` is true";
        facts::push(&mut found, LINT, range, message, Some(note.to_owned())); // alloc: a finding's note
    }
    found
}
