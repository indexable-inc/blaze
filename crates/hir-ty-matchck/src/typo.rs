//! rustc's `find_fallback_pattern_typo`: a catch-all binding that makes a later pattern
//! unreachable may have been meant as a constant, mistyped or not in scope.

use hir_ty::db::HirDatabase;
use hir_def::{
    AssocItemId, ConstId, HasModule, ModuleDefId, ModuleIdLt,
    expr_store::Body,
    hir::{Expr, ExprId, Pat as HirPat, PatId, Statement},
    item_scope::{ImportId, ImportOrGlob},
    nameres::{block_def_map, crate_def_map},
    signatures::ConstSignature,
};
use hir_expand::name::Name;
use hir_ty::{InferenceResult, ValueTyDefId, next_solver::Ty};
use stdx::edit_distance::find_best_match_for_name_by;

use crate::{PatternTypo, cx::Cx};

/// The constants of the binding's type in the crate's modules.
#[derive(Default)]
struct Candidates {
    accessible: Vec<Name>,
    accessible_path: Vec<Box<str>>,
    inaccessible: Vec<ConstId>,
    imported: Vec<Name>,
    imported_at: Vec<ImportId>,
}

/// `pat` is the catch-all; `blocks` are the blocks around it, outermost first.
pub(crate) fn find_fallback_pattern_typo<'db>(
    cx: &Cx<'_, 'db>,
    body: &Body,
    infer: &InferenceResult<'db>,
    blocks: &[ExprId],
    pat: PatId,
) -> Option<PatternTypo> {
    let HirPat::Bind { id, subpat: None } = body.pat(pat) else { return None };
    let name = &body[id].name;
    let found = candidates(cx, name, infer.binding_ty(id), body, blocks);

    if let Some(i) = found.accessible.iter().position(|c| c == name) {
        return Some(PatternTypo::WantedConstant {
            is_typo: false,
            const_name: found.accessible[i].as_str().into(),
            const_path: found.accessible_path[i].clone(), // clone: one path, into the lint.
        });
    }
    if let Some(typo) = find_best_match_for_name_by(&found.accessible, Name::as_str, name.as_str(), None) {
        return Some(PatternTypo::WantedConstant {
            is_typo: true,
            const_name: typo.as_str().into(),
            const_path: typo.as_str().into(),
        });
    }
    if let Some(i) = found.imported.iter().position(|c| c == name) {
        return Some(PatternTypo::AccessibleConstant(found.imported_at[i]));
    }
    if let Some(typo) = find_best_match_for_name_by(&found.imported, Name::as_str, name.as_str(), None) {
        return Some(PatternTypo::WantedConstant {
            is_typo: true,
            const_name: typo.as_str().into(),
            const_path: typo.as_str().into(),
        });
    }
    if let Some(&konst) = found.inaccessible.last() {
        return Some(PatternTypo::InaccessibleConstant(konst));
    }

    // rustc walks the HIR parents from the pattern out, each block's `let`s overwriting what
    // an inner one found: the outermost block's last `let` of the name wins.
    blocks
        .iter()
        .find_map(|&block| {
            let Expr::Block { statements, .. } = body.expr(block) else { return None };
            statements
                .iter()
                .filter_map(|stmt| match stmt {
                    Statement::Let { pat: let_pat, .. } => match body.pat(let_pat) {
                        HirPat::Bind { id, .. } if body[id].name == *name => Some(let_pat),
                        _ => None,
                    },
                    _ => None,
                })
                .last()
        })
        .map(PatternTypo::PatternLetBinding)
}

/// `free_items`' constants of `ty`, and its `use`s whose value is a constant of `ty`: those of
/// the crate's modules and of the `blocks` around the pattern (rustc's `free_items` has the
/// items of every body; those of other bodies can't be named from the pattern).
fn candidates<'db>(
    cx: &Cx<'_, 'db>,
    name: &Name,
    ty: Ty<'db>,
    body: &Body,
    blocks: &[ExprId],
) -> Candidates {
    let db = cx.db;
    let mut found = Candidates::default();
    let same_ty = |konst: ConstId| {
        db.value_ty(ValueTyDefId::ConstId(konst))
            .is_some_and(|const_ty| cx.infcx.can_eq(cx.env, ty, const_ty.skip_binder()))
    };
    let visible = |konst: ConstId| {
        AssocItemId::ConstId(konst).assoc_visibility(db).is_visible_from(db, cx.module)
    };
    let block_maps = blocks.iter().filter_map(|&block| match body.expr(block) {
        Expr::Block { id: Some(id), .. } => Some(block_def_map(db, id.to_db(db))),
        _ => None,
    });
    let def_maps = std::iter::once(crate_def_map(db, cx.krate)).chain(block_maps);
    for (module, data) in def_maps.flat_map(|it| it.modules(db)) {
        for (item_name, item) in data.scope(db).values() {
            let ModuleDefId::ConstId(konst) = item.def else { continue };
            match item.import {
                None if konst.module(db) == module && same_ty(konst) => {
                    if visible(konst) {
                        found.accessible.push(item_name.clone()); // clone: an interned name.
                        found.accessible_path.push(def_path(cx, konst, item_name));
                    } else if item_name == name {
                        found.inaccessible.push(konst);
                    }
                }
                Some(ImportOrGlob::Import(import)) if same_ty(konst) => {
                    if visible(konst) {
                        let Some(const_name) = &ConstSignature::of(db, konst).name else {
                            continue;
                        };
                        found.accessible_path.push(def_path(cx, konst, const_name));
                        found.accessible.push(const_name.clone()); // clone: an interned name.
                    } else if item.vis.is_visible_from(db, cx.module) {
                        found.imported.push(item_name.clone()); // clone: an interned name.
                        found.imported_at.push(import);
                    }
                }
                _ => {}
            }
        }
    }
    found
}

/// `def_path_str` without trimming: the modules from the crate root, then the name.
fn def_path(cx: &Cx<'_, '_>, konst: ConstId, name: &Name) -> Box<str> {
    let db = cx.db;
    // alloc: one path, for a lint.
    let mut modules = Vec::new();
    let mut module: ModuleIdLt<'_> = konst.module(db);
    while let Some(module_name) = module.name(db) {
        modules.push(module_name);
        match module.containing_module(db) {
            Some(parent) => module = parent,
            None => break,
        }
    }
    let mut path = String::new(); // alloc: as above.
    for module_name in modules.iter().rev() {
        path.push_str(module_name.as_str());
        path.push_str("::");
    }
    path.push_str(name.as_str());
    path.into_boxed_str()
}
