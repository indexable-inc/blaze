//! Where lowering a type written in a body gets its inference variables
//! (`lower::InferVars`): each variant lowers the same `let` type, whose `_`, `'_` and array
//! length `_` are the three kinds of variable.

use std::cell::OnceCell;

use base_db::{Db, all_crates};
use blaze_mem::{Frame, Tag, frame};
use hir_def::{
    DefWithBodyId, ExpressionStoreOwnerId, FunctionId, ModuleDefId,
    expr_store::ExpressionStore,
    nameres::crate_def_map,
    resolver::{HasResolver, Resolver},
    type_ref::{TypeRef, TypeRefId},
};
use rustc_type_ir::{InferConst, InferTy, inherent::IntoKind as _};
use hir_expand::fixture;
use thin_vec::ThinVec;

use crate::{
    InferenceResult, LifetimeElisionKind, LifetimeLoweringMode, TyLoweringContext,
    TyLoweringDiagnostic,
    generics::Generics,
    next_solver::{
        ConstKind, DbInterner, RegionKind, Ty, TyKind, TypingMode, default_types,
        infer::{DbInternerInferExt, InferCtxt},
    },
};

const FIXTURE: &str = r#"
//- minicore: sized, copy
fn f() {
    let _a: (_, &'_ u8, [u8; _]) = (1u32, &2, [0u8; 3]);
}
"#;

struct Fixture {
    db: Db,
    function: FunctionId,
}

impl Fixture {
    fn new() -> Fixture {
        let db = fixture::db(FIXTURE).into_db();
        let mut function = None;
        for &krate in all_crates(&db).iter() {
            if !krate.data(&db).origin.is_local() {
                continue;
            }
            let def_map = crate_def_map(&db, krate);
            for (_, module) in def_map.modules(&db) {
                for def in module.scope(&db).declarations() {
                    if let ModuleDefId::FunctionId(it) = def {
                        function = Some(it);
                    }
                }
            }
        }
        let function = function.expect("the fixture has `f`");
        Fixture { db, function }
    }

    fn resolver(&self) -> Resolver<'_> {
        self.body().resolver(&self.db)
    }

    fn body(&self) -> DefWithBodyId {
        self.function.into()
    }

    fn store(&self) -> &ExpressionStore {
        ExpressionStore::of(&self.db, ExpressionStoreOwnerId::Body(self.body()))
    }

    /// The `let`'s type, `(_, &'_ u8, [u8; _])`, and its `_`.
    fn annotation(&self) -> (TypeRefId, TypeRefId) {
        let store = self.store();
        let (tuple, fields) = store
            .type_refs()
            .find_map(|(id, it)| match it {
                TypeRef::Tuple(fields) => Some((id, fields)),
                _ => None,
            })
            .expect("the `let` writes a tuple type");
        assert!(matches!(store.type_ref(fields[0]), TypeRef::Placeholder));
        (tuple, fields[0])
    }

    /// The `let`'s type lowered with the variables `vars` gives, with lifetimes left out made
    /// variables, and the diagnostics the lowering pushed.
    fn lower<'db, 'a>(
        &'db self,
        resolver: &'a Resolver<'db>,
        generics: &'a OnceCell<Generics<'db>>,
        frame: &'a Frame,
        vars: impl FnOnce(TyLoweringContext<'db, 'a>) -> TyLoweringContext<'db, 'a>,
    ) -> (Ty<'db>, ThinVec<TyLoweringDiagnostic>) {
        let ctx = TyLoweringContext::new(
            &self.db,
            resolver,
            self.store(),
            ExpressionStoreOwnerId::Body(self.body()),
            self.function.into(),
            generics,
            LifetimeElisionKind::Infer,
            LifetimeLoweringMode::LateParam,
            frame,
        )
        .with_elided_region_vars();
        let mut ctx = vars(ctx);
        let ty = ctx.lower_ty(self.annotation().0);
        (ty, ctx.diagnostics.drain(..).collect())
    }
}

/// The three fields of a lowered `(_, &'_ u8, [u8; _])`: the `_`, the reference and the array.
fn fields(ty: Ty<'_>) -> [Ty<'_>; 3] {
    let TyKind::Tuple(tys) = ty.kind() else { panic!("{ty:?} is not a tuple") };
    <[Ty<'_>; 3]>::try_from(tys.as_slice()).expect("three fields")
}

fn infers_vars_not_allowed(diagnostics: &[TyLoweringDiagnostic]) -> bool {
    diagnostics.iter().any(|it| matches!(it, TyLoweringDiagnostic::InferVarsNotAllowed { .. }))
}

#[test]
fn inference_records_the_type_it_gives_a_placeholder() {
    let fixture = Fixture::new();
    let (_, placeholder) = fixture.annotation();
    let infer = InferenceResult::of(&fixture.db, fixture.body());
    assert_eq!(infer.type_of_type_placeholder(placeholder), Some(default_types().types.u32));
}

#[test]
fn an_inference_context_gives_a_variable_of_each_kind() {
    let fixture = Fixture::new();
    let krate = hir_def::HasModule::module(&fixture.function, &fixture.db).krate(&fixture.db);
    let interner = DbInterner::new_with(&fixture.db, krate);
    let infcx: InferCtxt<'_> = interner.infer_ctxt().build(TypingMode::non_body_analysis());
    let (resolver, generics) = (fixture.resolver(), OnceCell::new());
    let (ty, diagnostics) = frame::task(Tag::Infer, |frame| {
        fixture.lower(&resolver, &generics, frame, |ctx| ctx.with_infer_ctxt_vars(Some(&infcx)))
    });
    let [placeholder, reference, array] = fields(ty);
    assert!(matches!(placeholder.kind(), TyKind::Infer(InferTy::TyVar(_))), "{placeholder:?}");
    let TyKind::Ref(region, _, _) = reference.kind() else { panic!("{reference:?}") };
    assert!(matches!(region.kind(), RegionKind::ReVar(_)), "{region:?}");
    let TyKind::Array(_, len) = array.kind() else { panic!("{array:?}") };
    assert!(matches!(len.kind(), ConstKind::Infer(InferConst::Var(_))), "{len:?}");
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
}

#[test]
fn without_an_inference_context_a_placeholder_is_refused() {
    let fixture = Fixture::new();
    let (resolver, generics) = (fixture.resolver(), OnceCell::new());
    let (ty, diagnostics) = frame::task(Tag::Infer, |frame| {
        fixture.lower(&resolver, &generics, frame, |ctx| ctx.with_infer_ctxt_vars(None))
    });
    let types = default_types();
    let [placeholder, _, array] = fields(ty);
    assert_eq!(placeholder, types.types.error);
    let TyKind::Array(_, len) = array.kind() else { panic!("{array:?}") };
    assert_eq!(len, types.consts.error);
    assert!(infers_vars_not_allowed(&diagnostics), "{diagnostics:?}");
}

#[test]
fn a_finished_inference_answers_a_placeholder_and_errs_the_rest() {
    let fixture = Fixture::new();
    let infer = InferenceResult::of(&fixture.db, fixture.body());
    let (resolver, generics) = (fixture.resolver(), OnceCell::new());
    let (ty, diagnostics) = frame::task(Tag::Infer, |frame| {
        fixture.lower(&resolver, &generics, frame, |ctx| ctx.with_answered_infer_vars(Some(infer)))
    });
    let types = default_types();
    let [placeholder, reference, array] = fields(ty);
    assert_eq!(placeholder, types.types.u32);
    let TyKind::Ref(region, _, _) = reference.kind() else { panic!("{reference:?}") };
    assert_eq!(region, types.regions.error);
    let TyKind::Array(_, len) = array.kind() else { panic!("{array:?}") };
    assert_eq!(len, types.consts.error);
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
}

#[test]
fn without_a_finished_inference_a_placeholder_is_an_error_not_a_diagnostic() {
    let fixture = Fixture::new();
    let (resolver, generics) = (fixture.resolver(), OnceCell::new());
    let (ty, diagnostics) = frame::task(Tag::Infer, |frame| {
        fixture.lower(&resolver, &generics, frame, |ctx| ctx.with_answered_infer_vars(None))
    });
    let [placeholder, _, _] = fields(ty);
    assert_eq!(placeholder, default_types().types.error);
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
}
