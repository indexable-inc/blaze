//! Nested indexing through slices, arrays and references infers with no error: `grid[i][j] += 1`
//! through `IndexMut`, `rows[1][i]` through two `Index`es (queue 01396: hir-ty-borrowck's
//! `CLEAN` fixture left `indexes` not checked for its type errors). On a failure the message has
//! inference's diagnostics and each node whose type has an error.

use hir_def::{DefWithBodyId, ModuleDefId, nameres::crate_def_map, signatures::FunctionSignature};
use hir_expand::fixture;
use rustc_type_ir::TypeVisitableExt as _;

use crate::{InferBodyId, InferenceResult};

const FIXTURE: &str = r#"
//- minicore: sized, copy, clone, fn, index, slice, add
fn assign(grid: &mut [[u32; 4]; 4], i: usize, j: usize) {
    grid[i][j] += 1;
}
fn read(rows: &[&[u32]], i: usize) -> u32 {
    rows[1][i]
}
fn read_array(grid: &mut [[u32; 4]; 4], i: usize, j: usize) -> u32 {
    grid[j][i]
}
fn indexes(rows: &[&[u32]], grid: &mut [[u32; 4]; 4], i: usize, j: usize) -> u32 {
    grid[i][j] += 1;
    rows[1][i] + grid[j][i]
}
"#;

#[test]
fn nested_indexing_infers_with_no_error() {
    let db = fixture::db(FIXTURE).into_db();
    crate::next_solver::attach_db(&db, || {
        let mut failed = String::new();
        let fns = crate_def_map(&db, fixture::test_crate(&db))
            .modules(&db)
            .flat_map(|(_, module)| module.scope(&db).declarations().collect::<Vec<_>>())
            .filter_map(|def| match def {
                ModuleDefId::FunctionId(it) => Some(it),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(fns.len(), 4);
        for func in fns {
            let owner = InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(func));
            let infer = InferenceResult::of(&db, owner);
            if !infer.has_errors && !infer.has_type_mismatches() && infer.diagnostics().is_empty() {
                continue;
            }
            let name = FunctionSignature::of(&db, func).name.as_str().to_owned();
            failed.push_str(&format!(
                "`{name}`: has_errors {}, mismatches {}, diagnostics {:?}\n",
                infer.has_errors,
                infer.has_type_mismatches(),
                infer.diagnostics(),
            ));
            let (store, _) = owner.store_and_root_expr(&db);
            for (id, expr) in store.exprs() {
                let ty = infer.expr_ty(id);
                if ty.references_error() {
                    failed.push_str(&format!(
                        "  {id:?} {expr:?}: {ty:?}, adjustments {:?}, method {:?}\n",
                        infer.expr_adjustment(id),
                        infer.method_resolution(id),
                    ));
                }
            }
        }
        assert!(failed.is_empty(), "{failed}");
    });
}
