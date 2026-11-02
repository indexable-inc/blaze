//! `type_name::scalar_name` against the arms each spelling of a type had for the scalars.

use hir_ty::{
    next_solver::{DbInterner, Ty, TyKind},
    primitive::{FloatTy, IntTy, UintTy},
};
use hir_ty_mir::mir::type_name::{rustc_symbol_type, rustc_type_name, scalar_name};
use rustc_type_ir::inherent::{IntoKind, Ty as _};
use hir_expand::fixture;


/// The arms `write_ty`, `spell_ty` and `Spelling::symbol_type` each had.
fn reference(kind: TyKind<'_>) -> Option<&'static str> {
    Some(match kind {
        TyKind::Bool => "bool",
        TyKind::Char => "char",
        TyKind::Str => "str",
        TyKind::Never => "!",
        TyKind::Int(it) => it.name_str(),
        TyKind::Uint(it) => it.name_str(),
        TyKind::Float(it) => it.name_str(),
        _ => return None,
    })
}

#[test]
fn scalar_names_are_the_arms_they_replace() {
    let fx = fixture::db("fn main() {}");
    let _ = fx.only_file();
    let db = fx.into_db();
    hir_ty::attach_db(&db, || {
        let interner = DbInterner::new_no_crate(&db);
        let ints = [IntTy::Isize, IntTy::I8, IntTy::I16, IntTy::I32, IntTy::I64, IntTy::I128];
        let uints =
            [UintTy::Usize, UintTy::U8, UintTy::U16, UintTy::U32, UintTy::U64, UintTy::U128];
        let floats = [FloatTy::F16, FloatTy::F32, FloatTy::F64, FloatTy::F128];
        let scalars = [TyKind::Bool, TyKind::Char, TyKind::Str, TyKind::Never]
            .into_iter()
            .chain(ints.map(TyKind::Int))
            .chain(uints.map(TyKind::Uint))
            .chain(floats.map(TyKind::Float));
        for kind in scalars {
            let ty = Ty::new(interner, kind);
            let name = scalar_name(ty).expect("a scalar has a name");
            assert_eq!(Some(name), reference(ty.kind()));
            assert_eq!(rustc_type_name(&db, ty).as_deref(), Some(name));
            assert_eq!(rustc_symbol_type(&db, ty).as_deref(), Some(name));
        }
        let unit = Ty::new_tup(interner, &[]);
        let slice = Ty::new_slice(interner, Ty::new(interner, TyKind::Bool));
        for ty in [unit, slice] {
            assert_eq!(scalar_name(ty), None);
            assert_eq!(reference(ty.kind()), None);
        }
    });
}
