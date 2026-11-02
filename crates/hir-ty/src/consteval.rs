//! Constant evaluation details

use std::mem::offset_of;

use base_db::{Db, Crate};
use blaze_format::{FormatError, Placed, Slice, Writer};
use hir_def::{
    ConstId, EnumVariantId, ExpressionStoreOwnerId, ItemContainerId, StaticId,
    expr_store::{ExpressionStore, HygieneId, path::Path},
    hir::{Expr, ExprId, Literal},
    resolver::{Resolver, ValueNs},
    signatures::ImplSignature,
};
use hir_expand::Lookup;
use rustc_abi::Size;
use rustc_apfloat::Float;
use rustc_ast_ir::Mutability;
use rustc_type_ir::inherent::{GenericArgs as _, IntoKind, Ty as _};

use crate::{
    ComplexMemoryMap, MemoryMap, ParamEnvAndCrate, Span, StaticMemory,
    db::{AnonConstId, AnonConstLoc, HirDatabase},
    generics::Generics,
    lower::LoweringMode,
    mir::{IsSigned, MirEvalError, MirLowerError, pad16},
    next_solver::{
        AliasConst, AliasConstKind, Allocation, AllocationData, AnonConstIdWrapper, Const, ConstExt,
        ConstKind, Consts, DbInterner, DefaultAny, FreeConstAliasId, GenericArgs,
        InherentAssocConstId, IsRigid, ParamConst, ScalarInt, StoredAllocation, StoredEarlyBinder,
        StoredGenericArgs, StoredTy, TraitAssocConstId, Ty, TyKind, ValTreeKind, default_types,
        persist::{TyTableReader, TyTableWriter},
    },
    traits::StoredParamEnvAndCrate,
};

pub mod deps;
pub mod discriminants;
pub mod hir_eval;

#[derive(Debug, Clone, PartialEq, Eq, Hash, blaze_query::Value)]
pub enum ConstEvalError<'db> {
    MirLowerError(MirLowerError<'db>),
    MirEvalError(MirEvalError<'db>),
}

impl<'db> From<MirLowerError<'db>> for ConstEvalError<'db> {
    fn from(value: MirLowerError<'db>) -> Self {
        match value {
            MirLowerError::ConstEvalError(_, e) => *e,
            _ => ConstEvalError::MirLowerError(value),
        }
    }
}

impl<'db> From<MirEvalError<'db>> for ConstEvalError<'db> {
    fn from(value: MirEvalError<'db>) -> Self {
        ConstEvalError::MirEvalError(value)
    }
}

/// Interns a constant scalar with the given type
fn intern_const_ref<'db>(
    interner: DbInterner<'db>,
    value: &Literal,
    ty: Ty<'db>,
) -> Option<Result<Const<'db>, CreateConstError>> {
    let Ok(data_layout) = interner.db.target_data_layout(interner.expect_crate()) else {
        return Some(Err(CreateConstError::TargetLayoutNotAvailable));
    };
    // As rustc's `lit_to_const`: an integer literal is truncated to its type's width (the
    // `overflowing_literals` lint is what refuses one too large).
    let trunc = |size: Size, bits: u128| {
        ScalarInt::try_from_uint(size.truncate(bits), size).expect("a truncated value fits its size")
    };
    let valtree = match (ty.kind(), value) {
        (TyKind::Uint(uint), Literal::Uint(value, _)) => {
            let size = uint.bit_width().map(Size::from_bits).unwrap_or(data_layout.pointer_size());
            ValTreeKind::Leaf(trunc(size, *value))
        }
        (TyKind::Uint(uint), Literal::Int(value, _)) => {
            // `Literal::Int` is the default, so we also need to account for the type being uint.
            let size = uint.bit_width().map(Size::from_bits).unwrap_or(data_layout.pointer_size());
            ValTreeKind::Leaf(trunc(size, *value as u128))
        }
        (TyKind::Int(int), Literal::Int(value, _)) => {
            let size = int.bit_width().map(Size::from_bits).unwrap_or(data_layout.pointer_size());
            ValTreeKind::Leaf(trunc(size, *value as u128))
        }
        (TyKind::Bool, Literal::Bool(value)) => ValTreeKind::Leaf(ScalarInt::from(*value)),
        (TyKind::Char, Literal::Char(value)) => ValTreeKind::Leaf(ScalarInt::from(*value)),
        (TyKind::Float(float), Literal::Float(value, _)) => {
            let size = Size::from_bits(float.bit_width());
            let value = match float {
                rustc_ast_ir::FloatTy::F16 => value.to_f16().to_bits(),
                rustc_ast_ir::FloatTy::F32 => value.to_f32().to_bits(),
                rustc_ast_ir::FloatTy::F64 => value.to_f64().to_bits(),
                rustc_ast_ir::FloatTy::F128 => value.to_f128().to_bits(),
            };
            let scalar = ScalarInt::try_from_uint(value, size).unwrap();
            ValTreeKind::Leaf(scalar)
        }
        (TyKind::Ref(_, inner_ty, _), Literal::String(value))
            if matches!(inner_ty.kind(), TyKind::Str) =>
        {
            let u8_values = &interner.default_types().consts.u8_values;
            ValTreeKind::Branch(Consts::new_from_iter(
                value.as_str().as_bytes().iter().map(|&byte| u8_values[usize::from(byte)]),
            ))
        }
        (_, Literal::ByteString(_) | Literal::CString(_)) => {
            // This literals are complicated to construct and/or are possible to coerce.
            // So we just allocate an anon const for them, they should be rare so it's not a problem.
            return None;
        }
        _ => {
            let actual = literal_ty(
                interner,
                value,
                |types| types.types.i32,
                |types| types.types.u32,
                |types| types.types.f64,
            );
            return Some(Err(CreateConstError::TypeMismatch { actual: actual.store() }));
        }
    };
    Some(Ok(Const::new_valtree(interner, ty, valtree)))
}

pub(crate) fn literal_ty<'db>(
    interner: DbInterner<'db>,
    value: &Literal,
    default_int: impl FnOnce(&DefaultAny<'db>) -> Ty<'db>,
    default_uint: impl FnOnce(&DefaultAny<'db>) -> Ty<'db>,
    default_float: impl FnOnce(&DefaultAny<'db>) -> Ty<'db>,
) -> Ty<'db> {
    let types = interner.default_types();
    match value {
        Literal::Bool(..) => types.types.bool,
        Literal::String(..) => types.types.static_str_ref,
        Literal::ByteString(bs) => {
            let byte_type = types.types.u8;
            let array_type = Ty::new_array(interner, byte_type, bs.len() as u64);
            Ty::new_ref(interner, types.regions.statik, array_type, Mutability::Not)
        }
        Literal::CString(..) => Ty::new_ref(
            interner,
            types.regions.statik,
            interner.lang_items().CStr.map_or(types.types.error, |strukt| {
                Ty::new_adt(interner, strukt.into(), types.empty.generic_args)
            }),
            Mutability::Not,
        ),
        Literal::Char(..) => types.types.char,
        Literal::Int(_v, ty) => match ty {
            Some(int_ty) => match int_ty {
                hir_def::builtin_type::BuiltinInt::Isize => types.types.isize,
                hir_def::builtin_type::BuiltinInt::I8 => types.types.i8,
                hir_def::builtin_type::BuiltinInt::I16 => types.types.i16,
                hir_def::builtin_type::BuiltinInt::I32 => types.types.i32,
                hir_def::builtin_type::BuiltinInt::I64 => types.types.i64,
                hir_def::builtin_type::BuiltinInt::I128 => types.types.i128,
            },
            None => default_int(types),
        },
        Literal::Uint(_v, ty) => match ty {
            Some(int_ty) => match int_ty {
                hir_def::builtin_type::BuiltinUint::Usize => types.types.usize,
                hir_def::builtin_type::BuiltinUint::U8 => types.types.u8,
                hir_def::builtin_type::BuiltinUint::U16 => types.types.u16,
                hir_def::builtin_type::BuiltinUint::U32 => types.types.u32,
                hir_def::builtin_type::BuiltinUint::U64 => types.types.u64,
                hir_def::builtin_type::BuiltinUint::U128 => types.types.u128,
            },
            None => default_uint(types),
        },
        Literal::Float(_v, ty) => match ty {
            Some(float_ty) => match float_ty {
                hir_def::builtin_type::BuiltinFloat::F16 => types.types.f16,
                hir_def::builtin_type::BuiltinFloat::F32 => types.types.f32,
                hir_def::builtin_type::BuiltinFloat::F64 => types.types.f64,
                hir_def::builtin_type::BuiltinFloat::F128 => types.types.f128,
            },
            None => default_float(types),
        },
    }
}

/// Interns a possibly-unknown target usize
pub fn usize_const<'db>(db: &'db Db, value: Option<u128>, krate: Crate) -> Const<'db> {
    let interner = DbInterner::new_no_crate(db);
    let value = match value {
        Some(value) => value,
        None => {
            return Const::error(interner);
        }
    };
    let Ok(data_layout) = db.target_data_layout(krate) else {
        return Const::error(interner);
    };
    let usize_ty = interner.default_types().types.usize;
    let Some(scalar) = ScalarInt::try_from_uint(value, data_layout.pointer_size()) else {
        return Const::error(interner);
    };
    Const::new_valtree(interner, usize_ty, ValTreeKind::Leaf(scalar))
}

pub fn allocation_as_usize(ec: Allocation<'_>) -> u128 {
    u128::from_le_bytes(pad16(&ec.memory, IsSigned::No))
}

/// The alias kind of a MIR constant naming `konst` with its own (impl-form) args. A const in a
/// trait impl has none: MIR names the trait's const (a projection) and `lookup_impl_const`
/// resolves it at eval time, so meeting one here is a bug.
pub fn mir_const_alias_kind<'db>(
    db: &'db Db,
    konst: ConstId,
) -> AliasConstKind<'db> {
    match konst.lookup(db).container {
        ItemContainerId::TraitId(_) => {
            AliasConstKind::Projection { def_id: TraitAssocConstId(konst) }
        }
        ItemContainerId::ImplId(impl_) if ImplSignature::of(db, impl_).target_trait.is_none() => {
            AliasConstKind::InherentImpl { def_id: InherentAssocConstId(konst) }
        }
        ItemContainerId::ImplId(_) => panic!("{konst:?} is a trait impl's const: name the trait's"),
        ItemContainerId::ModuleId(_) | ItemContainerId::ExternBlockId(_) => {
            AliasConstKind::Free { def_id: FreeConstAliasId(konst) }
        }
    }
}

/// The value of `unevaluated`, read from the cached result by reference: an array length asks
/// this on every check, and one that fails to evaluate clones no error.
fn evaluated<'db>(
    db: &'db Db,
    unevaluated: AliasConst<'db>,
) -> Option<Allocation<'db>> {
    let result = match unevaluated.kind {
        AliasConstKind::Projection { def_id: TraitAssocConstId(id) }
        | AliasConstKind::InherentImpl { def_id: InherentAssocConstId(id) }
        | AliasConstKind::Free { def_id: FreeConstAliasId(id) } => {
            let _request = trace_const(db, id, false);
            const_eval_query(db, id, unevaluated.args.store(), None)
        }
        AliasConstKind::Anon { def_id } => {
            anon_const_eval_query(db, def_id.0, unevaluated.args.store(), None)
        }
        AliasConstKind::InherentSelf { .. } => {
            panic!("the inherent const {unevaluated:?} is in Self form after type check")
        }
    };
    result.as_ref().ok().map(|it| it.as_ref())
}

pub fn try_const_usize<'db>(db: &'db Db, c: Const<'db>) -> Option<u128> {
    match c.kind() {
        ConstKind::Param(_) => None,
        ConstKind::Infer(_) => None,
        ConstKind::Bound(_, _) => None,
        ConstKind::Placeholder(_) => None,
        ConstKind::Alias(_, unevaluated) => evaluated(db, unevaluated).map(allocation_as_usize),
        ConstKind::Value(val) => {
            if val.ty == default_types().types.usize {
                Some(val.value.inner().to_leaf().to_uint_unchecked())
            } else {
                None
            }
        }
        ConstKind::Error(_) => None,
        ConstKind::Expr(_) => None,
    }
}

pub fn allocation_as_isize(ec: Allocation<'_>) -> i128 {
    i128::from_le_bytes(pad16(&ec.memory, IsSigned::Yes))
}

pub fn try_const_isize<'db>(db: &'db Db, c: Const<'db>) -> Option<i128> {
    match c.kind() {
        ConstKind::Param(_) => None,
        ConstKind::Infer(_) => None,
        ConstKind::Bound(_, _) => None,
        ConstKind::Placeholder(_) => None,
        ConstKind::Alias(_, unevaluated) => evaluated(db, unevaluated).map(allocation_as_isize),
        ConstKind::Value(val) => {
            if val.ty == default_types().types.isize {
                Some(val.value.inner().to_leaf().to_int_unchecked())
            } else {
                None
            }
        }
        ConstKind::Error(_) => None,
        ConstKind::Expr(_) => None,
    }
}

/// Why a const argument is not a constant: the reason of
/// [`crate::InferenceDiagnostic::InvalidConstArg`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CreateConstError {
    /// A generic parameter where none may be used: rustc's "generic parameters may not be used
    /// in const operations".
    UsedForbiddenParam,
    /// A local binding: E0435.
    LocalBinding,
    /// A static: E0013.
    Static,
    /// The `Self` constructor of an impl.
    ImplSelf,
    /// A function or constructor named by a bare path, which only an anon const holds.
    NeedsAnonConst,
    /// A path that names no value: E0425.
    DoesNotResolve,
    /// A bare path to a constant with generic parameters of its own.
    ConstHasGenerics,
    /// `_` where no inference variable may stand.
    UnderscoreExpr,
    /// An expression that needs an anon const where the lowering mode makes none.
    AnonConstInterningDisabled,
    /// A literal of `actual`, another type than the parameter's: E0308.
    TypeMismatch { actual: StoredTy },
    /// No target data layout to size an integer literal by.
    TargetLayoutNotAvailable,
}

pub(crate) fn path_to_const<'a, 'db: 'a>(
    db: &'db Db,
    resolver: &Resolver<'db>,
    generics: &impl Fn() -> &'a Generics<'db>,
    forbid_params_after: Option<u32>,
    path: Path<'_>,
) -> Result<Const<'db>, CreateConstError> {
    let interner = DbInterner::new_no_crate(db);
    let resolution = resolver
        .resolve_path_in_value_ns_fully(db, path, HygieneId::ROOT)
        .ok_or(CreateConstError::DoesNotResolve)?;
    let no_generics = |def| crate::generics::generics(db, def).has_no_params();
    let konst = match resolution {
        ValueNs::ConstId(id) if no_generics(id.into()) => id,
        ValueNs::ConstId(_) => return Err(CreateConstError::ConstHasGenerics),
        ValueNs::GenericParam(param) => {
            let index = generics().type_or_const_param_idx(param.into());
            if forbid_params_after.is_some_and(|forbid_after| index >= forbid_after) {
                return Err(CreateConstError::UsedForbiddenParam);
            }
            return Ok(Const::new_param(interner, ParamConst { id: param, index }));
        }
        ValueNs::LocalBinding(_) => return Err(CreateConstError::LocalBinding),
        ValueNs::StaticId(_) => return Err(CreateConstError::Static),
        ValueNs::ImplSelf(_) => return Err(CreateConstError::ImplSelf),
        // A function or a constructor is a value, but only an anon const can hold it.
        ValueNs::FunctionId(_) | ValueNs::StructId(_) | ValueNs::EnumVariantId(_) => {
            return Err(CreateConstError::NeedsAnonConst);
        }
    };
    let args = GenericArgs::empty();
    let alias = AliasConst::new(interner, mir_const_alias_kind(db, konst), args);
    Ok(Const::new_alias(interner, IsRigid::No, alias))
}

/// `forbid_params_after` and `allow_anon_const_using_generic_params` fill a similar role, but are not the same.
///
/// `forbid_params_after` is only respected if an anon const is *not* created, that is, we directly resolve to a const param.
/// `allow_anon_const_using_generic_params` is only respected if an anon const *is* created.
///
/// For example, inside types, you are allowed to refer to const params in bare paths (but might be forbidden from referring
/// to *some*, in param defaults), but not allowed to do so in anon consts (e.g. `{ {}; N }` is forbidden, because it creates
/// an anon const). Inside field defaults, you are allowed to refer to them even inside anon consts.
///
/// There's also `generic_const_exprs`, `min_generic_const_args`, and `generic_const_args`, all of which we don't implement yet.
pub(crate) fn create_anon_const<'a, 'db: 'a>(
    interner: DbInterner<'db>,
    owner: ExpressionStoreOwnerId,
    store: &ExpressionStore,
    expr_id: ExprId,
    resolver: &Resolver<'db>,
    expected_ty: Ty<'db>,
    generics: &impl Fn() -> &'a Generics<'db>,
    create_var: Option<&mut impl FnMut(Span) -> Const<'db>>,
    lowering_mode: LoweringMode,
    forbid_params_after: Option<u32>,
    allow_anon_const_using_generic_params: bool,
    mismatched_literal_as_anon_const: bool,
) -> Result<Const<'db>, CreateConstError> {
    let mut expr = store.expr(expr_id);
    if let Expr::Block { statements, tail: Some(tail), .. } = expr
        && statements.is_empty()
    {
        // rustc unwraps *one* layer of blocks, so we do too (this impacts whether the const can use generic parameters.
        // Anon consts sometimes cannot while bare paths can). mGCA allows arbitrarily many blocks, but we don't implement
        // it yet.
        expr = store.expr(tail);
    }
    match expr {
        // A literal of another type is, as in rustc, an anon const whose own type check gives
        // E0308, when `mismatched_literal_as_anon_const`.
        Expr::Literal(literal)
            if let Some(literal) = intern_const_ref(interner, literal, expected_ty)
                && !(mismatched_literal_as_anon_const
                    && matches!(literal, Err(CreateConstError::TypeMismatch { .. }))) =>
        {
            literal
        }
        Expr::Underscore => match create_var {
            Some(create_var) => Ok(create_var(expr_id.into())),
            None => Err(CreateConstError::UnderscoreExpr),
        },
        Expr::Path(path)
            if let konst =
                path_to_const(interner.db, resolver, generics, forbid_params_after, path)
                && !matches!(
                    konst,
                    Err(CreateConstError::DoesNotResolve
                        | CreateConstError::ConstHasGenerics
                        | CreateConstError::NeedsAnonConst)
                ) =>
        {
            konst
        }
        _ => {
            let Some(token) = lowering_mode.allow_tracked_structs() else {
                return Err(CreateConstError::AnonConstInterningDisabled);
            };

            let konst = AnonConstId::new(
                interner.db,
                AnonConstLoc {
                    owner,
                    expr: expr_id,
                    ty: StoredEarlyBinder::bind(interner, expected_ty.store()),
                    allow_using_generic_params: allow_anon_const_using_generic_params,
                },
                token,
            );
            let args = if allow_anon_const_using_generic_params {
                GenericArgs::identity_for_item(interner, owner.generic_def(interner.db).into())
            } else {
                GenericArgs::empty()
            };
            let kind = AliasConstKind::Anon { def_id: AnonConstIdWrapper(konst) };
            Ok(Const::new_alias(interner, IsRigid::No, AliasConst::new(interner, kind, args)))
        }
    }
}

/// One variant's discriminant, kept per variant so that what reads it is invalidated only when
/// its own value changes. Every variant's at once is [`discriminants::enum_discriminants`].
#[blaze_query::tracked(cycle_result = const_eval_discriminant_cycle_result, returns(clone), mem_tag = Infer)]
pub(crate) fn const_eval_discriminant_variant<'db>(
    db: &'db Db,
    variant_id: EnumVariantId,
) -> Result<i128, ConstEvalError<'db>> {
    discriminants::variant_discriminant(db, variant_id)
}

fn const_eval_discriminant_cycle_result<'db>(
    _: &'db Db,
    _: blaze_query::db::Id,
    _: EnumVariantId,
) -> Result<i128, ConstEvalError<'db>> {
    Err(ConstEvalError::MirLowerError(MirLowerError::Loop))
}

type TraceConst = fn(&Db, ConstId, bool) -> Option<fn(std::time::Instant)>;
static TRACE_CONST: std::sync::OnceLock<TraceConst> = std::sync::OnceLock::new();

/// Has `observer` see each request for a named constant (`false`) and each run of its query
/// (`true`): for one it follows, it gives what to call at the end with when it began.
pub fn trace_consts_with(observer: TraceConst) {
    _ = TRACE_CONST.set(observer);
}

fn trace_const(db: &Db, def: ConstId, executing: bool) -> Option<impl Drop> {
    let finish = TRACE_CONST.get()?(db, def, executing)?;
    let start = std::time::Instant::now();
    Some(stdx::defer(move || finish(start)))
}

pub(crate) fn const_eval<'db>(
    db: &'db Db,
    def: ConstId,
    subst: GenericArgs<'db>,
    trait_env: Option<ParamEnvAndCrate<'db>>,
) -> Result<Allocation<'db>, ConstEvalError<'db>> {
    let _request = trace_const(db, def, false);
    match const_eval_query(db, def, subst.store(), trait_env.map(|env| env.store())) {
        Ok(konst) => Ok(konst.as_ref()),
        Err(err) => Err(err.clone()), // clone: error path
    }
}

#[blaze_query::tracked(returns(ref), cycle_result = const_eval_cycle_result, lru = 0, mem_tag = Infer)]
pub fn const_eval_query<'db>(
    db: &'db Db,
    def: ConstId,
    subst: StoredGenericArgs,
    trait_env: Option<StoredParamEnvAndCrate>,
) -> Result<StoredAllocation, ConstEvalError<'db>> {
    let _execution = trace_const(db, def, true);
    let subst = subst.as_ref();
    if let Some(kept) = deps::cached(db, def, subst)? {
        return Ok(kept.store());
    }
    let trait_env = trait_env.as_ref().map(|env| env.as_ref(db));
    let value = hir_eval::value(db, def.into(), subst, trait_env).map(|it| it.store());
    deps::record(db, def, subst, &value);
    value
}

fn const_eval_cycle_result<'db>(
    _: &'db Db,
    _: blaze_query::db::Id,
    _: ConstId,
    _: StoredGenericArgs,
    _: Option<StoredParamEnvAndCrate>,
) -> Result<StoredAllocation, ConstEvalError<'db>> {
    Err(ConstEvalError::MirLowerError(MirLowerError::Loop))
}

pub(crate) fn anon_const_eval<'db>(
    db: &'db Db,
    def: AnonConstId<'db>,
    subst: GenericArgs<'db>,
    trait_env: Option<ParamEnvAndCrate<'db>>,
) -> Result<Allocation<'db>, ConstEvalError<'db>> {
    match anon_const_eval_query(db, def, subst.store(), trait_env.map(|env| env.store())) {
        Ok(konst) => Ok(konst.as_ref()),
        Err(err) => Err(err.clone()), // clone: error path
    }
}

#[blaze_query::tracked(returns(ref), cycle_result = anon_const_eval_cycle_result, lru = 0, mem_tag = Infer)]
pub(crate) fn anon_const_eval_query<'db>(
    db: &'db Db,
    def: AnonConstId<'db>,
    subst: StoredGenericArgs,
    trait_env: Option<StoredParamEnvAndCrate>,
) -> Result<StoredAllocation, ConstEvalError<'db>> {
    let trait_env = trait_env.as_ref().map(|env| env.as_ref(db));
    Ok(hir_eval::value(db, def.into(), subst.as_ref(), trait_env)?.store())
}

fn anon_const_eval_cycle_result<'db>(
    _: &'db Db,
    _: blaze_query::db::Id,
    _: AnonConstId<'db>,
    _: StoredGenericArgs,
    _: Option<StoredParamEnvAndCrate>,
) -> Result<StoredAllocation, ConstEvalError<'db>> {
    Err(ConstEvalError::MirLowerError(MirLowerError::Loop))
}

pub(crate) fn const_eval_static<'db>(
    db: &'db Db,
    def: StaticId,
) -> Result<Allocation<'db>, ConstEvalError<'db>> {
    match const_eval_static_query(db, def) {
        Ok(konst) => Ok(konst.as_ref()),
        Err(err) => Err(err.clone()), // clone: error path
    }
}

#[blaze_query::tracked(returns(ref), cycle_result = const_eval_static_cycle_result, lru = 0, mem_tag = Infer)]
pub(crate) fn const_eval_static_query<'db>(
    db: &'db Db,
    def: StaticId,
) -> Result<StoredAllocation, ConstEvalError<'db>> {
    Ok(hir_eval::value(db, def.into(), GenericArgs::empty(), None)?.store())
}

fn const_eval_static_cycle_result<'db>(
    _: &'db Db,
    _: blaze_query::db::Id,
    _: StaticId,
) -> Result<StoredAllocation, ConstEvalError<'db>> {
    Err(ConstEvalError::MirLowerError(MirLowerError::Loop))
}

blaze_format::record! {
    /// A piece of a constant's memory: its address, and where its bytes are in the bytes of
    /// its [`MemoryMapRec`].
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub struct PieceRec: 1 {
        pub address: u64,
        pub start: u64,
        pub len: u64,
    }
}

blaze_format::record! {
    /// What the piece of memory at `address` was aligned to.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub struct AlignRec: 1 {
        pub address: u64,
        pub align: u64,
    }
}

blaze_format::record! {
    /// The memory of a static a constant points into: where it was and the static, as its id
    /// (the database that wrote it is the one reading it).
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub struct StaticRec: 1 {
        pub start: u64,
        pub size: u64,
        pub id: u64,
    }
}

blaze_format::record! {
    /// A [`MemoryMap`]: `kind` 0 for [`MemoryMap::Empty`], 1 for [`MemoryMap::Simple`] (its
    /// bytes in `bytes`, every other column empty), 2 for [`MemoryMap::Complex`] (its pieces'
    /// bytes in `bytes` in the order of their addresses). Types are indices in the buffer's
    /// type table (`next_solver::persist`): `vtable` in the order of their ids, `type_ids` one
    /// more than the index, 0 for a `TypeId` of no type. `value_pointers` and
    /// `memory_pointers` are where its words with provenance are, sorted.
    #[derive(Clone, Copy)]
    pub struct MemoryMapRec: 2 {
        pub kind: u32,
        pub bytes: Slice<u8>,
        pub pieces: Slice<PieceRec>,
        pub aligns: Slice<AlignRec>,
        pub statics: Slice<StaticRec>,
        pub vtable: Slice<u32>,
        pub type_ids: Slice<u32>,
        pub value_pointers: Slice<u64>,
        pub memory_pointers: Slice<u64>,
    }
}

blaze_format::record! {
    /// An [`Allocation`], the value of a constant or a static: its type as an index in the
    /// buffer's type table, its bytes and what they point to.
    #[derive(Clone, Copy)]
    pub struct AllocationRec: 1 {
        pub ty: u32,
        pub memory: Slice<u8>,
        pub map: MemoryMapRec,
    }
}

const _: () = assert!(size_of::<MemoryMapRec>() == 4 + 8 * 8);
const _: () = assert!(size_of::<AllocationRec>() == 4 + 8 + size_of::<MemoryMapRec>());

/// Writes `allocation` into `out`, its types into `tys`; returns where its record is, for the
/// [`blaze_format::Rel`] of the record that holds it.
pub fn write_allocation<'db>(
    out: &mut Writer,
    tys: &mut TyTableWriter<'db>,
    allocation: Allocation<'db>,
) -> u64 {
    let memory = Placed::push(out, &allocation.memory);
    let map = write_memory_map(out, &allocation.memory_map, &mut |ty| tys.ty(ty));
    let ty = tys.ty(allocation.ty);
    let at = out.next::<AllocationRec>();
    let record = AllocationRec {
        ty,
        memory: memory.slice(at + offset_of!(AllocationRec, memory) as u64),
        map: map.record(at + offset_of!(AllocationRec, map) as u64),
    };
    let written = out.push(&record);
    debug_assert_eq!(written, at);
    at
}

/// The allocation `record`, a record of `buffer`, is of, its types read from `tys`.
pub fn read_allocation<'db>(
    record: &AllocationRec,
    buffer: &[u8],
    tys: &TyTableReader<'_, 'db>,
) -> Result<Allocation<'db>, FormatError> {
    let ty = tys.ty(record.ty)?;
    let memory = Box::from(record.memory.follow(buffer)?);
    let memory_map = read_memory_map(&record.map, buffer, &|at| tys.ty(at))?;
    Ok(Allocation::new(AllocationData { ty, memory, memory_map }))
}

/// The columns of a [`MemoryMapRec`], written.
struct MapColumns {
    kind: u32,
    bytes: Placed,
    pieces: Placed,
    aligns: Placed,
    statics: Placed,
    vtable: Placed,
    type_ids: Placed,
    value_pointers: Placed,
    memory_pointers: Placed,
}

impl MapColumns {
    /// The record of the columns, to be written at `at`.
    fn record(&self, at: u64) -> MemoryMapRec {
        let field = |offset: usize| at + offset as u64;
        MemoryMapRec {
            kind: self.kind,
            bytes: self.bytes.slice(field(offset_of!(MemoryMapRec, bytes))),
            pieces: self.pieces.slice(field(offset_of!(MemoryMapRec, pieces))),
            aligns: self.aligns.slice(field(offset_of!(MemoryMapRec, aligns))),
            statics: self.statics.slice(field(offset_of!(MemoryMapRec, statics))),
            vtable: self.vtable.slice(field(offset_of!(MemoryMapRec, vtable))),
            type_ids: self.type_ids.slice(field(offset_of!(MemoryMapRec, type_ids))),
            value_pointers: self
                .value_pointers
                .slice(field(offset_of!(MemoryMapRec, value_pointers))),
            memory_pointers: self
                .memory_pointers
                .slice(field(offset_of!(MemoryMapRec, memory_pointers))),
        }
    }
}

/// Writes the columns of `map`, each type as the index `ty` gives it.
fn write_memory_map<'db>(
    out: &mut Writer,
    map: &MemoryMap<'db>,
    ty: &mut impl FnMut(Ty<'db>) -> u32,
) -> MapColumns {
    let none = Placed { at: 0, len: 0 };
    let mut columns = MapColumns {
        kind: 0,
        bytes: none,
        pieces: none,
        aligns: none,
        statics: none,
        vtable: none,
        type_ids: none,
        value_pointers: none,
        memory_pointers: none,
    };
    match map {
        MemoryMap::Empty => {}
        MemoryMap::Simple(bytes) => {
            columns.kind = 1;
            columns.bytes = Placed::push(out, bytes);
        }
        MemoryMap::Complex(map) => {
            columns.kind = 2;
            let memory = &map.memory;
            columns.bytes = match memory.compact_bytes() {
                Some(bytes) => Placed::push(out, bytes),
                None => {
                    let len = memory.iter().map(|(_, it)| it.len()).sum();
                    out.bytes.reserve(len);
                    let at = out.next::<u8>();
                    memory.iter().for_each(|(_, it)| _ = out.push_slice(it));
                    Placed { at, len }
                }
            };
            let mut start = 0;
            columns.pieces = Placed::push_each(
                out,
                memory.len(),
                memory.iter().map(|(&address, it)| {
                    let piece = PieceRec { address: address as u64, start, len: it.len() as u64 };
                    start += it.len() as u64;
                    piece
                }),
            );
            columns.aligns = Placed::push_each(
                out,
                map.aligns.len(),
                map.aligns.iter().map(|&(address, align)| AlignRec {
                    address: address as u64,
                    align: align as u64,
                }),
            );
            columns.statics = Placed::push_each(
                out,
                map.statics.len(),
                map.statics.iter().map(|it| StaticRec {
                    start: it.start as u64,
                    size: it.size as u64,
                    id: crate::check_cache::id_bits(it.id),
                }),
            );
            columns.vtable = Placed::push_each(
                out,
                map.vtable.tys().len(),
                map.vtable.tys().map(&mut *ty),
            );
            columns.type_ids = Placed::push_each(
                out,
                map.type_ids.len(),
                map.type_ids.iter().map(|it| it.map_or(0, |it| ty(it) + 1)),
            );
            columns.value_pointers = Placed::push_each(
                out,
                map.value_pointers.len(),
                map.value_pointers.iter().map(|&it| it as u64),
            );
            columns.memory_pointers = Placed::push_each(
                out,
                map.memory_pointers.len(),
                map.memory_pointers.iter().map(|&it| it as u64),
            );
        }
    }
    columns
}

/// The memory map `record`, a record of `buffer`, is of, each type read as `ty` reads its index.
fn read_memory_map<'db>(
    record: &MemoryMapRec,
    buffer: &[u8],
    ty: &impl Fn(u32) -> Result<Ty<'db>, FormatError>,
) -> Result<MemoryMap<'db>, FormatError> {
    let at = (record as *const MemoryMapRec as usize).wrapping_sub(buffer.as_ptr() as usize) as u64;
    let invalid = |what| FormatError::Invalid { at, what };
    let word = |value: u64| usize::try_from(value).map_err(|_| invalid("an address past usize"));
    let bytes = record.bytes.follow(buffer)?;
    let pieces = record.pieces.follow(buffer)?;
    let aligns = record.aligns.follow(buffer)?;
    let statics = record.statics.follow(buffer)?;
    let vtable = record.vtable.follow(buffer)?;
    let type_ids = record.type_ids.follow(buffer)?;
    let value_pointers = record.value_pointers.follow(buffer)?;
    let memory_pointers = record.memory_pointers.follow(buffer)?;
    let only_bytes = pieces.is_empty()
        && aligns.is_empty()
        && statics.is_empty()
        && vtable.is_empty()
        && type_ids.is_empty()
        && value_pointers.is_empty()
        && memory_pointers.is_empty();
    match record.kind {
        0 if only_bytes && bytes.is_empty() => return Ok(MemoryMap::Empty),
        1 if only_bytes => return Ok(MemoryMap::Simple(Box::from(bytes))),
        0 | 1 => return Err(invalid("columns a memory map of its kind does not have")),
        2 => {}
        _ => return Err(invalid("a memory map of no kind")),
    }
    let mut addresses = Vec::with_capacity(pieces.len());
    let mut extents = Vec::with_capacity(pieces.len());
    for piece in pieces {
        let address = word(piece.address)?;
        if addresses.last().is_some_and(|&last| last >= address) {
            return Err(invalid("pieces of memory out of the order of their addresses"));
        }
        let (start, len) = (word(piece.start)?, word(piece.len)?);
        if start.checked_add(len).is_none_or(|end| end > bytes.len()) {
            return Err(invalid("a piece of memory past the bytes of its map"));
        }
        addresses.push(address);
        extents.push(crate::Extent { start, len });
    }
    let mut map = ComplexMemoryMap {
        // alloc: the owned allocation read back from the image, once, at its size
        memory: crate::MemoryPieces::from_columns(bytes.to_vec(), addresses, extents),
        aligns: Vec::with_capacity(aligns.len()),
        statics: Vec::with_capacity(statics.len()),
        type_ids: Vec::with_capacity(type_ids.len()),
        ..ComplexMemoryMap::default()
    };
    for align in aligns {
        if map.aligns.last().is_some_and(|&(last, _)| last as u64 >= align.address) {
            return Err(invalid("alignments out of the order of their addresses"));
        }
        map.aligns.push((word(align.address)?, word(align.align)?));
    }
    for it in statics {
        let id = crate::check_cache::id_of::<StaticId>(it.id)
            .ok_or_else(|| invalid("a static of id index 0 or not in the table"))?;
        map.statics.push(StaticMemory { start: word(it.start)?, size: word(it.size)?, id });
    }
    let mut last = None;
    for &at in vtable {
        let id = map.vtable.id(ty(at)?);
        if last.is_some_and(|last: usize| id != last + 1) {
            return Err(invalid("a type twice in a vtable map"));
        }
        last = Some(id);
    }
    for &at in type_ids {
        map.type_ids.push(match at {
            0 => None,
            at => Some(ty(at - 1)?),
        });
    }
    for (column, places) in
        [(&mut map.value_pointers, value_pointers), (&mut map.memory_pointers, memory_pointers)]
    {
        if places.windows(2).any(|it| it[0] >= it[1]) {
            return Err(invalid("pointers with provenance out of the order of their places"));
        }
        // alloc: the owned allocation read back, once, at its size
        column.reserve_exact(places.len());
        for &place in places {
            column.push(word(place)?);
        }
    }
    Ok(MemoryMap::Complex(Box::new(map))) // alloc: the owned allocation read back, once
}

#[cfg(test)]
mod tests {
    use blaze_format::{Aligned, Record};

    use super::*;

    fn write(map: &MemoryMap<'static>) -> Aligned {
        let mut out = Writer::new(<MemoryMapRec as Record>::SCHEMA);
        let columns = write_memory_map(&mut out, map, &mut |_| unreachable!("a map without types"));
        let at = out.next::<MemoryMapRec>();
        let root = out.push(&columns.record(at));
        Aligned::copy_from(&out.finish(root))
    }

    fn read(buffer: &[u8]) -> Result<MemoryMap<'static>, FormatError> {
        let record = blaze_format::open::<MemoryMapRec>(buffer)?;
        read_memory_map(record, buffer, &|_| unreachable!("a map without types"))
    }

    fn complex() -> ComplexMemoryMap<'static> {
        let mut map = ComplexMemoryMap::default();
        map.insert(0x2000, Box::new([1u8, 2, 3, 4, 5, 6, 7, 8]));
        map.insert(0x1000, Box::new([9u8; 3]));
        map.insert(0x1000, Box::new([10u8; 5]));
        map.insert(0x2004, Box::new([5u8, 6]));
        map.insert(0x3000, Box::new([0u8; 0]));
        map.note_align(0x2000, 8);
        map.note_align(0x1000, 1);
        map.statics.push(StaticMemory {
            start: 0x9000,
            size: 16,
            id: blaze_query::db::FromId::from_id(blaze_query::db::Id::from_u32(7 + (2 << 20))),
        });
        map.note_pointer(crate::PointerAt::Memory(0x2000));
        map.note_pointer(crate::PointerAt::Value(8));
        map.note_pointer(crate::PointerAt::Value(0));
        map
    }

    #[test]
    fn memory_maps_read_back_as_written() {
        for map in [
            MemoryMap::Empty,
            MemoryMap::simple(Box::new([0u8; 0])),
            MemoryMap::simple(Box::new([1u8, 2, 3])),
            MemoryMap::Complex(Box::default()),
            MemoryMap::Complex(Box::new(complex())),
        ] {
            assert_eq!(read(&write(&map)).unwrap(), map);
        }
    }

    #[test]
    fn equal_maps_are_equal_bytes() {
        let mut other = ComplexMemoryMap::default();
        other.note_align(0x1000, 1);
        other.insert(0x3000, Box::new([0u8; 0]));
        other.insert(0x2004, Box::new([5u8, 6]));
        other.insert(0x1000, Box::new([10u8; 5]));
        other.note_align(0x2000, 8);
        other.insert(0x2000, Box::new([1u8, 2, 3, 4, 5, 6, 7, 8]));
        other.statics = complex().statics;
        other.note_pointer(crate::PointerAt::Value(0));
        other.note_pointer(crate::PointerAt::Memory(0x2000));
        other.note_pointer(crate::PointerAt::Value(8));
        let (a, b) = (MemoryMap::Complex(Box::new(complex())), MemoryMap::Complex(Box::new(other)));
        assert_eq!(a, b);
        assert_eq!(&*write(&a), &*write(&b));
        // A piece that grew leaves nothing of its old bytes in the buffer.
        let buffer = write(&a);
        let record = blaze_format::open::<MemoryMapRec>(&buffer).unwrap();
        assert_eq!(record.bytes.get(&buffer).len(), 5 + 8 + 2);
    }

    /// A buffer of a map of `kind` with `pieces` over `bytes`, as no writer of a map writes it.
    fn forged(kind: u32, pieces: &[PieceRec], bytes: &[u8]) -> Aligned {
        forged_with_pointers(kind, pieces, bytes, &[])
    }

    /// [`forged`], with `value_pointers` as that column.
    fn forged_with_pointers(
        kind: u32,
        pieces: &[PieceRec],
        bytes: &[u8],
        value_pointers: &[u64],
    ) -> Aligned {
        let mut out = Writer::new(<MemoryMapRec as Record>::SCHEMA);
        let pieces_at = Placed::push(&mut out, pieces);
        let bytes_at = Placed::push(&mut out, bytes);
        let value_pointers = Placed::push(&mut out, value_pointers);
        let none = Placed { at: 0, len: 0 };
        let columns = MapColumns {
            kind,
            bytes: bytes_at,
            pieces: pieces_at,
            aligns: none,
            statics: none,
            vtable: none,
            type_ids: none,
            value_pointers,
            memory_pointers: none,
        };
        let at = out.next::<MemoryMapRec>();
        let root = out.push(&columns.record(at));
        Aligned::copy_from(&out.finish(root))
    }

    #[test]
    fn maps_no_writer_writes_are_refused() {
        let piece = |address, start, len| PieceRec { address, start, len };
        assert!(read(&forged(3, &[], &[])).is_err());
        assert!(read(&forged(0, &[], &[1])).is_err());
        assert!(read(&forged(1, &[piece(0, 0, 1)], &[1])).is_err());
        assert!(read(&forged(2, &[piece(8, 0, 2)], &[1])).is_err());
        assert!(read(&forged(2, &[piece(8, u64::MAX, 2)], &[1, 2])).is_err());
        assert!(read(&forged(2, &[piece(8, 0, 1), piece(8, 1, 1)], &[1, 2])).is_err());
        assert!(read(&forged(2, &[piece(9, 0, 1), piece(8, 1, 1)], &[1, 2])).is_err());
        let read_back = read(&forged(2, &[piece(8, 0, 1), piece(9, 0, 2)], &[1, 2])).unwrap();
        let MemoryMap::Complex(map) = read_back else { panic!("a complex map read as another") };
        assert_eq!(map.memory.get(&9), Some(&[1, 2][..]));
        assert!(read(&forged_with_pointers(1, &[], &[1], &[0])).is_err());
        assert!(read(&forged_with_pointers(2, &[], &[], &[8, 8])).is_err());
        assert!(read(&forged_with_pointers(2, &[], &[], &[16, 8])).is_err());
        let read_back = read(&forged_with_pointers(2, &[], &[], &[8, 16])).unwrap();
        assert!(read_back.has_provenance(crate::PointerAt::Value(16)));
        assert!(!read_back.has_provenance(crate::PointerAt::Memory(16)));
    }
}
