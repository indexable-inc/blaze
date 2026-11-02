//! `normalize_or_keep` against the copies it replaced: the code generator's `normalize`,
//! lowering's `place_ty` and the interpreter's `patch_addresses` (an alias only), and debug
//! info's `normalize` and `tail` (every type, through the solver).

use hir_def::ModuleDefId;
use hir_ty::{
    InferBodyId, TyDefId,
    db::HirDatabase,
    next_solver::{
        DbInterner, ParamEnv, Ty, TyKind, TypingMode,
        infer::{DbInternerInferExt, InferCtxt, traits::ObligationCause},
        obligation_ctxt::ObligationCtxt,
    },
};
use hir_ty_mir::mir::normalize_or_keep;
use rustc_type_ir::inherent::IntoKind;
use hir_expand::fixture;


const TYPES: &str = r#"
//- minicore: sized, copy
pub trait Tr { type Out; }
impl Tr for u32 { type Out = (u8, bool); }
impl Tr for u8 { type Out = <u32 as Tr>::Out; }
pub trait Unimplemented { type Out; }
pub struct W<T>(pub T);
pub type A = <u32 as Tr>::Out;
pub type B = <u8 as Tr>::Out;
pub type C = <i64 as Unimplemented>::Out;
pub type D = W<<u32 as Tr>::Out>;
pub type E = (u8, [char; 2]);
pub type F = &'static <u32 as Tr>::Out;
pub fn f(x: <u32 as Tr>::Out, w: W<<u8 as Tr>::Out>) -> <u8 as Tr>::Out { let _ = w; x }
"#;

/// Debug info's copy: every type through the solver, the type itself where it fails.
fn through_the_solver<'db>(infcx: &InferCtxt<'db>, env: ParamEnv<'db>, ty: Ty<'db>) -> Ty<'db> {
    ObligationCtxt::new(infcx).structurally_normalize_ty(&ObligationCause::dummy(), env, ty).unwrap_or(ty)
}

/// The code generator's, lowering's and the interpreter's: an alias only.
fn alias_only<'db>(infcx: &InferCtxt<'db>, env: ParamEnv<'db>, ty: Ty<'db>) -> Ty<'db> {
    if !matches!(ty.kind(), TyKind::Alias(..)) {
        return ty;
    }
    through_the_solver(infcx, env, ty)
}

#[test]
fn normalize_or_keep_is_each_copy_it_replaced() {
    let fx = fixture::db(TYPES);
    let _ = fx.only_file();
    let db = fx.into_db();
    hir_ty::attach_db(&db, || {
        let krate = fixture::test_crate(&db);
        let interner = DbInterner::new_with(&db, krate);
        let infcx = interner.infer_ctxt().build(TypingMode::Codegen);
        let env = ParamEnv::empty();
        let mut tys = Vec::new();
        for def in hir_def::test_items::declarations(&db, krate) {
            match def {
                ModuleDefId::TypeAliasId(it) => tys.push(db.ty(TyDefId::TypeAliasId(it)).skip_binder()),
                ModuleDefId::FunctionId(it) => {
                    let body = db
                        .mir_body(InferBodyId::DefWithBodyId(it.into()))
                        .unwrap_or_else(|e| panic!("the MIR of {it:?}: {e:?}"));
                    tys.extend(body.locals.iter().map(|(_, local)| local.ty.as_ref()));
                }
                _ => {}
            }
        }
        let mut aliases = 0;
        let mut normalized = 0;
        for ty in tys {
            let one = normalize_or_keep(&infcx, env, ty);
            assert_eq!(one, alias_only(&infcx, env, ty), "`{ty:?}`");
            assert_eq!(one, through_the_solver(&infcx, env, ty), "`{ty:?}`");
            aliases += usize::from(matches!(ty.kind(), TyKind::Alias(..)));
            normalized += usize::from(one != ty);
        }
        assert!(aliases > 0, "the fixture has aliases");
        assert!(normalized > 0, "the fixture has aliases that normalize");
    });
}
