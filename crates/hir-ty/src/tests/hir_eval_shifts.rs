//! A shift of two integers is of the HIR evaluator's shape, as an addition is (queue 01397:
//! `by_hir` refused exactly the shifts of `hir-ty-mir`'s `consteval::hir_eval` tests). On a
//! refusal the message has what `Program::of` reads of the body: inference's error flags and
//! diagnostics, and each node's type and adjustments.

use hir_def::{
    AdtId, DefWithBodyId, ModuleDefId, nameres::crate_def_map, signatures::ConstSignature,
};
use hir_expand::fixture;

use crate::{InferBodyId, InferenceResult, consteval::hir_eval};

const FIXTURE: &str = r#"
const ADD: u8 = 1 + 4;
const SHIFT: u8 = 1 << 4;
const SHIFT_TYPED: u8 = 1u8 << 4u32;
const A: u8 = 7;
const C: i32 = -(A as i32) * 3 + (1 << 4);
const SHL: u8 = 1 << 8;
const SHR: i128 = 1 >> 128;
const SHL_NEG: i32 = 1 << -1;
enum E { X = 1 << 64 }
"#;

#[test]
fn a_shift_of_two_integers_is_of_the_shape() {
    let db = fixture::db(FIXTURE).into_db();
    crate::next_solver::attach_db(&db, || {
        let mut refused = String::new();
        // As `hir-ty-mir`'s `check_all`: every constant and explicit discriminant.
        let mut owners = Vec::new();
        for (_, module) in crate_def_map(&db, fixture::test_crate(&db)).modules(&db) {
            for def in module.scope(&db).declarations() {
                match def {
                    ModuleDefId::ConstId(konst) => {
                        let name = ConstSignature::of(&db, konst).name.as_ref();
                        let name = name.map_or_else(String::new, |it| it.as_str().to_owned());
                        let owner = InferBodyId::DefWithBodyId(DefWithBodyId::ConstId(konst));
                        owners.push((name, owner));
                    }
                    ModuleDefId::AdtId(AdtId::EnumId(enum_)) => owners.extend(
                        enum_
                            .enum_variants(&db)
                            .variants
                            .values()
                            .map(|(it, _)| ("E::X".to_owned(), InferBodyId::from(*it))),
                    ),
                    _ => {}
                }
            }
        }
        assert_eq!(owners.len(), 9);
        for (name, owner) in owners {
            if hir_eval::by_hir(&db, owner, None).is_some() {
                continue;
            }
            let infer = InferenceResult::of(&db, owner);
            let (store, root) = owner.store_and_root_expr(&db);
            refused.push_str(&format!(
                "`{name}` refused: has_errors {}, mismatches {}, diagnostics {:?}, root {root:?}\n",
                infer.has_errors,
                infer.has_type_mismatches(),
                infer.diagnostics(),
            ));
            for (id, expr) in store.exprs() {
                refused.push_str(&format!(
                    "  {id:?} {expr:?}: {:?}, adjustments {:?}, method {:?}\n",
                    infer.expr_ty(id),
                    infer.expr_adjustment(id),
                    infer.method_resolution(id),
                ));
            }
        }
        assert!(refused.is_empty(), "{refused}");
    });
}
