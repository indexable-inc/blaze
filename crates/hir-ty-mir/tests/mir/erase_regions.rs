//! `erase_and_anonymize_regions` against the two folders it replaced: codegen's vtable
//! `LifetimeEraser` and the evaluator's `TypeId` erasing followed by `Anonymized`.

use hir_ty::{
    db::HirDatabase,
    next_solver::{Binder, DbInterner, Region, RegionExt as _, RegionKind, Ty},
};
use hir_ty_mir::mir::erase_and_anonymize_regions;
use rustc_type_ir::{Interner, TypeFoldable, TypeFolder, TypeSuperFoldable, inherent::IntoKind};
use hir_expand::fixture;

use crate::function;

const TYPES: &str = r#"
//- minicore: sized, fn
pub fn types<'x>(
    a: for<'a> fn(&'a u8, &'static u8, &'x u8),
    b: &'x dyn for<'b> Fn(&'b u8) -> &'b u8,
    c: (&'x u8, for<'c, 'd> fn(&'d u8, &'c u8)),
    d: u8,
) {}
"#;

struct LifetimeEraser<'db>(DbInterner<'db>);

impl<'db> TypeFolder<DbInterner<'db>> for LifetimeEraser<'db> {
    fn cx(&self) -> DbInterner<'db> {
        self.0
    }

    fn fold_binder<T: TypeFoldable<DbInterner<'db>>>(
        &mut self,
        binder: Binder<'db, T>,
    ) -> Binder<'db, T> {
        self.0.anonymize_bound_vars(binder).super_fold_with(self)
    }

    fn fold_region(&mut self, region: Region<'db>) -> Region<'db> {
        match region.kind() {
            RegionKind::ReBound(..) => region,
            _ => Region::new_erased(self.0),
        }
    }
}

struct Anonymized<'db>(DbInterner<'db>);

impl<'db> TypeFolder<DbInterner<'db>> for Anonymized<'db> {
    fn cx(&self) -> DbInterner<'db> {
        self.0
    }

    fn fold_binder<T: TypeFoldable<DbInterner<'db>>>(
        &mut self,
        binder: Binder<'db, T>,
    ) -> Binder<'db, T> {
        self.0.anonymize_bound_vars(binder.super_fold_with(self))
    }
}

fn type_id_reference<'db>(interner: DbInterner<'db>, ty: Ty<'db>) -> Ty<'db> {
    let erased = Region::new_erased(interner);
    let ty = rustc_type_ir::fold_regions(interner, ty, |region, _| match region.kind() {
        RegionKind::ReBound(..) => region,
        _ => erased,
    });
    ty.fold_with(&mut Anonymized(interner))
}

#[test]
fn erasing_and_anonymizing_is_what_both_folders_did() {
    let fx = fixture::db(TYPES);
    let _ = fx.only_file();
    let db = fx.into_db();
    hir_ty::attach_db(&db, || {
        let interner = DbInterner::new_no_crate(&db);
        let func = function(&db, "types");
        let body = db.mir_body(func.into()).expect("the body");
        let params: Vec<Ty<'_>> =
            body.param_locals.iter().map(|&it| body.locals[it].ty.as_ref()).collect();
        assert_eq!(params.len(), 4);
        for &ty in &params {
            let shared = erase_and_anonymize_regions(interner, ty);
            assert_eq!(shared, ty.fold_with(&mut LifetimeEraser(interner)), "vtable's, {ty:?}");
            assert_eq!(shared, type_id_reference(interner, ty), "`TypeId`'s, {ty:?}");
        }
        assert_eq!(erase_and_anonymize_regions(interner, params[3]), params[3]);
    });
}
