//! Compute the binary representation of a type

use std::{cell::OnceCell, fmt, ops::Deref, sync::LazyLock};

use base_db::{Db, Crate};
use blaze_hashcons::{Consed, HashCons, Interned};
use blaze_mem::Frame;
use blaze_ids::{Domain, Hasher};
use hir_def::{
    AdtId, EnumId, EnumVariantId, LocalFieldId, StructId, VariantId,
    attrs::AttrFlags,
    layout::{LayoutCalculatorError, LayoutData},
    signatures::{StructFlags, StructSignature},
};
use la_arena::{Idx, RawIdx};

use rustc_abi::{
    AddressSpace, BackendRepr, FieldsShape, Float, Integer, LayoutCalculator, Niche,
    NumScalableVectors, Primitive, ReprOptions, Scalar, Size, StructKind, TargetDataLayout,
    WrappingRange,
};
use rustc_index::{IndexSlice, IndexVec, bit_set::BitMatrix};
use rustc_type_ir::{
    FloatTy, IntTy, TypeVisitableExt as _, UintTy,
    inherent::{GenericArgs as _, IntoKind},
};

use crate::{
    ParamEnvAndCrate,
    consteval::try_const_usize,
    db::HirDatabase,
    next_solver::{
        Const, ConstKind, DbInterner, GenericArgs, ParamEnv, PatternKind, StoredTy, Ty, TyKind,
        TypingMode, ValueConst,
        infer::{DbInternerInferExt, InferCtxt, traits::ObligationCause},
    },
    representability::{Representability, representability},
    traits::StoredParamEnvAndCrate,
};

pub use self::{adt::layout_of_adt_query, target::target_data_layout_query};

pub(crate) mod adt;
pub(crate) mod target;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RustcEnumVariantIdx(pub usize);

impl rustc_index::Idx for RustcEnumVariantIdx {
    fn new(idx: usize) -> Self {
        RustcEnumVariantIdx(idx)
    }

    fn index(self) -> usize {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RustcFieldIdx(pub LocalFieldId);

impl RustcFieldIdx {
    pub fn new(idx: usize) -> Self {
        RustcFieldIdx(Idx::from_raw(RawIdx::from(idx as u32)))
    }
}

impl rustc_index::Idx for RustcFieldIdx {
    fn new(idx: usize) -> Self {
        RustcFieldIdx(Idx::from_raw(RawIdx::from(idx as u32)))
    }

    fn index(self) -> usize {
        u32::from(self.0.into_raw()) as usize
    }
}

pub type Layout = LayoutData<RustcFieldIdx, RustcEnumVariantIdx>;
pub type TagEncoding = hir_def::layout::TagEncoding<RustcEnumVariantIdx>;
pub type Variants = hir_def::layout::Variants<RustcFieldIdx, RustcEnumVariantIdx>;

/// A layout as the layout queries return it: hash-consed in [`LAYOUTS`], as rustc interns
/// `Layout<'tcx>`, so equal layouts are kept once, a read is a pointer copy, and equality (what
/// the query engine backdates by) is a pointer compare. A newtype because [`Layout`] is
/// `rustc_abi`'s.
#[derive(Debug, PartialEq, Eq)]
pub struct InternedLayout(pub Layout);

impl Deref for InternedLayout {
    type Target = Layout;

    fn deref(&self) -> &Layout {
        &self.0
    }
}

impl AsRef<Layout> for InternedLayout {
    fn as_ref(&self) -> &Layout {
        &self.0
    }
}

static LAYOUT_DOMAIN: Domain = Domain::new("blaze v2 layout 1");

impl Consed for InternedLayout {
    fn domain() -> &'static Domain {
        &LAYOUT_DOMAIN
    }

    /// `LayoutData`'s `Hash` is derived, as its `Eq` is, and writes no address.
    fn hash_content(&self, hasher: &mut Hasher) {
        blaze_hashcons::hash_std(&self.0, hasher)
    }
}

/// Every layout of the process, never freed, as `crates/intern`'s tables are: a handle stays
/// valid in every revision and every database.
static LAYOUTS: LazyLock<HashCons<InternedLayout>> = LazyLock::new(HashCons::new);

/// The one handle of `layout`.
pub fn intern_layout(layout: Layout) -> Interned<'static, InternedLayout> {
    LAYOUTS.intern(InternedLayout(layout))
}

#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub enum LayoutError {
    // FIXME: Remove more variants once they get added to LayoutCalculatorError
    BadCalc(LayoutCalculatorError<()>),
    /// `variant`'s discriminant did not evaluate; rustc reports it (E0080) and stops.
    DiscriminantFailed { variant: EnumVariantId },
    HasErrorConst,
    HasErrorType,
    HasPlaceholder,
    InvalidSimdType,
    /// `ty`'s aliases do not normalize: rustc's `LayoutError::NormalizationFailure`.
    NormalizationFailure { ty: StoredTy },
    NotImplemented,
    RecursiveTypeWithoutIndirection,
    /// A scalable vector in a struct, enum, tuple, array or closure: rustc lays it out at the
    /// least vector length, which blaze's code does not keep it at.
    ScalableVectorField,
    TargetLayoutNotAvailable,
    Unknown,
    UserReprTooSmall,
}

impl std::error::Error for LayoutError {}
impl fmt::Display for LayoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LayoutError::BadCalc(err) => err.fallback_fmt(f),
            LayoutError::DiscriminantFailed { variant } => {
                write!(f, "the discriminant of {variant:?} did not evaluate")
            }
            LayoutError::HasErrorConst => write!(f, "type contains an unevaluatable const"),
            LayoutError::HasErrorType => write!(f, "type contains an error"),
            LayoutError::HasPlaceholder => write!(f, "type contains placeholders"),
            LayoutError::InvalidSimdType => write!(f, "invalid simd type definition"),
            LayoutError::NormalizationFailure { ty } => {
                write!(f, "unable to determine layout for `{:?}`: it cannot be normalized", ty.as_ref())
            }
            LayoutError::NotImplemented => write!(f, "not implemented"),
            LayoutError::RecursiveTypeWithoutIndirection => {
                write!(f, "recursive type without indirection")
            }
            LayoutError::ScalableVectorField => write!(f, "a scalable vector in an aggregate"),
            LayoutError::TargetLayoutNotAvailable => write!(f, "target layout not available"),
            LayoutError::Unknown => write!(f, "unknown"),
            LayoutError::UserReprTooSmall => {
                write!(f, "the `#[repr]` hint is too small to hold the discriminants of the enum")
            }
        }
    }
}

impl<F> From<LayoutCalculatorError<F>> for LayoutError {
    fn from(err: LayoutCalculatorError<F>) -> Self {
        LayoutError::BadCalc(err.without_payload())
    }
}

/// What a layout reads of a struct's signature: its flags. A rename, or a change to its
/// generics or their bounds, leaves this equal.
#[blaze_query::tracked(returns(copy), mem_tag = Infer)]
pub fn struct_flags(db: &Db, id: StructId) -> StructFlags {
    StructSignature::of(db, id).flags
}

/// Whether the last field of a struct may be unsized for some generic arguments. Such a field
/// stays last for all of them, so that unsizing the struct does not move it. The answer reads
/// the struct's own declaration and the crate's `Sized`, never a layout's generic arguments, so
/// it is asked once per struct and crate rather than once per instantiation.
#[blaze_query::tracked(returns(copy), mem_tag = Infer)]
pub fn struct_tail_may_be_unsized(db: &Db, id: StructId, krate: Crate) -> bool {
    variant_field_ids(db, id.into()).last().is_some_and(|&fd| {
        let interner = DbInterner::new_with(db, krate);
        let infcx = interner.infer_ctxt().build(TypingMode::PostAnalysis);
        let ty = db.field_types(id.into())[fd].ty().instantiate_identity().skip_norm_wip();
        !infcx.type_is_sized_modulo_regions(db.trait_environment(AdtId::from(id).into()), ty)
    })
}

/// The fields of a variant, in order: what a layout reads of them besides their types. A
/// field's rename or visibility leaves this equal.
#[blaze_query::tracked(returns(deref), mem_tag = Infer)]
pub fn variant_field_ids(db: &Db, variant: VariantId) -> Box<[LocalFieldId]> {
    let fields = variant.fields(db).fields();
    let mut ids = Vec::with_capacity(fields.len());
    ids.extend(fields.iter().map(|(id, _)| id));
    ids.into_boxed_slice()
}

/// The variants of an enum, in order: what a layout reads of them besides their fields.
#[blaze_query::tracked(returns(deref), mem_tag = Infer)]
pub fn enum_variant_ids(db: &Db, id: EnumId) -> Box<[EnumVariantId]> {
    let variants = &id.enum_variants(db).variants;
    let mut ids = Vec::with_capacity(variants.len());
    ids.extend(variants.values().map(|&(variant, _)| variant));
    ids.into_boxed_slice()
}

struct LayoutCx<'a> {
    calc: LayoutCalculator<&'a TargetDataLayout>,
}

impl<'a> LayoutCx<'a> {
    fn new(target: &'a TargetDataLayout) -> Self {
        Self { calc: LayoutCalculator::new(target) }
    }
}

// FIXME: move this to the `rustc_abi`.
fn layout_of_simd_ty<'db>(
    db: &'db Db,
    id: StructId,
    repr_packed: bool,
    args: &GenericArgs<'db>,
    env: ParamEnvAndCrate<'db>,
    dl: &TargetDataLayout,
) -> Result<Interned<'static, InternedLayout>, LayoutError> {
    // Supported SIMD vectors are homogeneous ADTs with exactly one array field:
    //
    // * #[repr(simd)] struct S([T; 4])
    //
    // where T is a primitive scalar (integer/float/pointer).
    let fields = db.field_types(id.into());
    let mut fields = fields.iter();
    let Some(TyKind::Array(e_ty, e_len)) =
        fields.next().filter(|_| fields.next().is_none()).map(|f| {
            (*f.1).ty().instantiate(DbInterner::new_no_crate(db), args).skip_norm_wip().kind()
        })
    else {
        return Err(LayoutError::InvalidSimdType);
    };

    let e_len = try_const_usize(db, e_len).ok_or(LayoutError::HasErrorConst)? as u64;
    let e_ly = db.layout_of_ty(e_ty.store(), env.store())?;

    let cx = LayoutCx::new(dl);
    Ok(intern_layout(cx.calc.layout_of_simd_type(e_ly.get(), e_len, repr_packed)?))
}

/// A `#[rustc_scalable_vector(N)] struct S(T)` or a `#[rustc_scalable_vector] struct Sx2(S, S)` of
/// such: rustc's `scalable_vector_parts` and `layout_of_scalable_vector_type`. Its size is that
/// of the vector at the least vector length, `size_of::<T>() * N` times the vectors of a tuple.
fn layout_of_scalable_vector_ty<'db, 'f>(
    db: &'db Db,
    frame: &'f Frame,
    id: StructId,
    args: &GenericArgs<'db>,
    env: ParamEnvAndCrate<'db>,
    dl: &TargetDataLayout,
) -> Result<Interned<'static, InternedLayout>, LayoutError> {
    let interner = DbInterner::new_no_crate(db);
    let only_field = |id: StructId| -> Result<Vec<Ty<'db>, &'f Frame>, LayoutError> {
        let fields = db.field_types(id.into());
        let mut tys = Vec::with_capacity_in(fields.len(), frame);
        tys.extend(fields.iter().map(|f| (*f.1).ty().instantiate(interner, args).skip_norm_wip()));
        Ok(tys)
    };
    let repr = |id: StructId| AttrFlags::repr(db, id.into()).unwrap_or_default().scalable;
    let fields = only_field(id)?;
    let (vector, number_of_vectors) = match repr(id) {
        Some(rustc_abi::ScalableElt::ElementCount(_)) => (id, NumScalableVectors::for_non_tuple()),
        Some(rustc_abi::ScalableElt::Container) => {
            let Some(TyKind::Adt(def, _)) = fields.first().map(|it| it.kind()) else {
                return Err(LayoutError::InvalidSimdType);
            };
            let hir_def::AdtId::StructId(vector) = def.def_id() else {
                return Err(LayoutError::InvalidSimdType);
            };
            let Some(count) = NumScalableVectors::from_field_count(fields.len()) else {
                return Err(LayoutError::InvalidSimdType);
            };
            (vector, count)
        }
        None => return Err(LayoutError::InvalidSimdType),
    };
    let Some(rustc_abi::ScalableElt::ElementCount(count)) = repr(vector) else {
        return Err(LayoutError::InvalidSimdType);
    };
    let element = match &only_field(vector)?[..] {
        [element] => *element,
        _ => return Err(LayoutError::InvalidSimdType),
    };
    let element = db.layout_of_ty(element.store(), env.store())?;
    let cx = LayoutCx::new(dl);
    Ok(intern_layout(cx.calc.layout_of_scalable_vector_type(
        element.get(),
        u64::from(count),
        number_of_vectors,
    )?))
}

/// The layout of a field of an aggregate (a struct, enum, tuple, array, closure or coroutine),
/// which a scalable vector is not laid out as: see [`LayoutError::ScalableVectorField`].
pub(crate) fn field_layout<'db>(
    db: &'db Db,
    ty: StoredTy,
    env: StoredParamEnvAndCrate,
) -> Result<&'static Layout, LayoutError> {
    let layout = db.layout_of_ty(ty, env)?.get();
    if let BackendRepr::SimdScalableVector { .. } = layout.backend_repr {
        return Err(LayoutError::ScalableVectorField);
    }
    Ok(&layout.0)
}

/// Whether `deeply_normalize` can change `ty`: its folder gives back as it is any type without
/// an alias or a `ConstKind::Alias` in it, which is what `HAS_ALIAS` flags.
fn needs_normalizing(ty: Ty<'_>) -> bool {
    ty.has_aliases()
}

/// Normalizes the types a layout reads, as rustc's `layout_of` skips `normalize_erasing_regions`
/// for a type without aliases. The interner (which reads the crate's lang items and def map)
/// and the inference context are built for the first type that needs them and shared by the
/// ones after, as one context was for all of them.
struct Normalizer<'db> {
    db: &'db Db,
    krate: Crate,
    param_env: ParamEnv<'db>,
    cause: ObligationCause,
    infcx: OnceCell<InferCtxt<'db>>,
}

impl<'db> Normalizer<'db> {
    fn new(db: &'db Db, krate: Crate, param_env: ParamEnv<'db>) -> Self {
        Self { db, krate, param_env, cause: ObligationCause::dummy(), infcx: OnceCell::new() }
    }

    fn normalize(&self, ty: Ty<'db>) -> Result<Ty<'db>, LayoutError> {
        // `deeply_normalize` refuses these, aliases or not.
        assert!(!ty.has_escaping_bound_vars());
        if !needs_normalizing(ty) {
            return Ok(ty);
        }
        let infcx = self.infcx.get_or_init(|| {
            DbInterner::new_with(self.db, self.krate).infer_ctxt().build(TypingMode::PostAnalysis)
        });
        // An alias rigid in the typing mode the type comes from may not be in `PostAnalysis`,
        // as rustc's `layout_of` resets them when it switches.
        let unnormalized =
            rustc_type_ir::set_aliases_to_non_rigid(infcx.interner, ty).skip_normalization();
        infcx
            .at(&self.cause, self.param_env)
            .deeply_normalize(unnormalized)
            .map_err(|_| LayoutError::NormalizationFailure { ty: ty.store() })
    }
}

/// A tuple's or closure's fields laid out as one variant.
fn univariant<'db>(
    cx: &LayoutCx<'_>,
    db: &'db Db,
    frame: &Frame,
    tys: impl Iterator<Item = Ty<'db>>,
    trait_env: &StoredParamEnvAndCrate,
    kind: StructKind,
) -> Result<Layout, LayoutError> {
    let mut fields = Vec::with_capacity_in(tys.size_hint().0, frame);
    for ty in tys {
        // clone: the env is two `u32` handles
        fields.push(field_layout(db, ty.store(), trait_env.clone())?);
    }
    univariant_of(cx, frame, &fields, kind)
}

fn univariant_of(
    cx: &LayoutCx<'_>,
    frame: &Frame,
    fields: &[&Layout],
    kind: StructKind,
) -> Result<Layout, LayoutError> {
    let fields: &[&&Layout] = frame.alloc_slice_from_iter(fields.iter());
    Ok(cx.calc.layout_of_univariant(
        IndexSlice::<RustcFieldIdx, _>::from_raw(fields),
        &ReprOptions::default(),
        kind,
    )?)
}

#[blaze_query::tracked(cycle_result = layout_of_ty_cycle_result, returns(clone), mem_tag = Infer)]
pub fn layout_of_ty_query(
    db: &Db,
    ty: StoredTy,
    trait_env: StoredParamEnvAndCrate,
) -> Result<Interned<'static, InternedLayout>, LayoutError> {
    if let Some(bare) = bare_env(db, ty, trait_env) {
        return db.layout_of_ty(ty, bare);
    }
    blaze_mem::frame::current(|frame| layout_of_ty_in(db, ty, trait_env, frame))
}

/// The crate's empty environment when `ty` has no parameter, inference variable or placeholder
/// and every clause of `env` names one: no such clause can match a goal about `ty` or its
/// fields, so the layout under the empty environment is the same, and a type is laid out once
/// per crate rather than once per environment of the bodies that name it. `None` when the
/// environment is already empty or a clause might apply.
fn bare_env(db: &Db, ty: StoredTy, env: StoredParamEnvAndCrate) -> Option<StoredParamEnvAndCrate> {
    let clauses = env.param_env(db).clauses;
    if clauses.is_empty() || !ty.as_ref().is_global() || clauses.iter().any(|c| c.is_global()) {
        return None;
    }
    Some(ParamEnvAndCrate { param_env: ParamEnv::empty(), krate: env.krate }.store())
}

fn layout_of_ty_in(
    db: &Db,
    ty: StoredTy,
    trait_env: StoredParamEnvAndCrate,
    frame: &Frame,
) -> Result<Interned<'static, InternedLayout>, LayoutError> {
    let krate = trait_env.krate;
    let Ok(target) = db.target_data_layout(krate) else {
        return Err(LayoutError::TargetLayoutNotAvailable);
    };
    let dl = target;
    let cx = LayoutCx::new(dl);
    let normalizer = Normalizer::new(db, krate, trait_env.param_env(db));
    let ty = normalizer.normalize(ty.as_ref())?;
    let result = match ty.kind() {
        TyKind::Adt(def, args) => {
            match def.def_id() {
                hir_def::AdtId::StructId(s) => {
                    let repr = AttrFlags::repr(db, s.into()).unwrap_or_default();
                    if repr.scalable.is_some() {
                        return layout_of_scalable_vector_ty(
                            db,
                            frame,
                            s,
                            &args,
                            trait_env.as_ref(db),
                            target,
                        );
                    }
                    if repr.simd() {
                        return layout_of_simd_ty(
                            db,
                            s,
                            repr.packed(),
                            &args,
                            trait_env.as_ref(db),
                            target,
                        );
                    }
                }
                _ => {}
            }
            return db.layout_of_adt(def.def_id(), args.store(), trait_env);
        }
        TyKind::Bool => Layout::scalar(
            dl,
            Scalar::Initialized {
                value: Primitive::Int(Integer::I8, false),
                valid_range: WrappingRange { start: 0, end: 1 },
            },
        ),
        TyKind::Char => Layout::scalar(
            dl,
            Scalar::Initialized {
                value: Primitive::Int(Integer::I32, false),
                valid_range: WrappingRange { start: 0, end: 0x10FFFF },
            },
        ),
        TyKind::Int(i) => Layout::scalar(
            dl,
            scalar_unit(
                dl,
                Primitive::Int(
                    match i {
                        IntTy::Isize => dl.ptr_sized_integer(),
                        IntTy::I8 => Integer::I8,
                        IntTy::I16 => Integer::I16,
                        IntTy::I32 => Integer::I32,
                        IntTy::I64 => Integer::I64,
                        IntTy::I128 => Integer::I128,
                    },
                    true,
                ),
            ),
        ),
        TyKind::Uint(i) => Layout::scalar(
            dl,
            scalar_unit(
                dl,
                Primitive::Int(
                    match i {
                        UintTy::Usize => dl.ptr_sized_integer(),
                        UintTy::U8 => Integer::I8,
                        UintTy::U16 => Integer::I16,
                        UintTy::U32 => Integer::I32,
                        UintTy::U64 => Integer::I64,
                        UintTy::U128 => Integer::I128,
                    },
                    false,
                ),
            ),
        ),
        TyKind::Float(f) => Layout::scalar(
            dl,
            scalar_unit(
                dl,
                Primitive::Float(match f {
                    FloatTy::F16 => Float::F16,
                    FloatTy::F32 => Float::F32,
                    FloatTy::F64 => Float::F64,
                    FloatTy::F128 => Float::F128,
                }),
            ),
        ),
        TyKind::Tuple(tys) => {
            let kind =
                if tys.is_empty() { StructKind::AlwaysSized } else { StructKind::MaybeUnsized };
            univariant(&cx, db, frame, tys.iter(), &trait_env, kind)?
        }
        TyKind::Array(element, count) => {
            let count = try_const_usize(db, count).ok_or(LayoutError::HasErrorConst)? as u64;
            let element = field_layout(db, element.store(), trait_env)?;
            cx.calc.layout_of_array_like::<_, _, ()>(element, Some(count))?
        }
        TyKind::Slice(element) => {
            let element = field_layout(db, element.store(), trait_env)?;
            cx.calc.layout_of_array_like::<_, _, ()>(element, None)?
        }
        TyKind::Str => {
            let element = scalar_unit(dl, Primitive::Int(Integer::I8, false));
            cx.calc.layout_of_array_like::<_, _, ()>(&Layout::scalar(dl, element), None)?
        }
        // Potentially-wide pointers.
        TyKind::Ref(_, pointee, _) | TyKind::RawPtr(pointee, _) => {
            let mut data_ptr = scalar_unit(dl, Primitive::Pointer(AddressSpace::ZERO));
            if matches!(ty.kind(), TyKind::Ref(..)) {
                data_ptr.valid_range_mut().start = 1;
            }

            // FIXME(next-solver)
            // let pointee = tcx.normalize_erasing_regions(param_env, pointee);
            // if pointee.is_sized(tcx.at(DUMMY_SP), param_env) {
            //     return Ok(tcx.mk_layout(LayoutS::scalar(cx, data_ptr)));
            // }

            // The tail can be behind an associated type, such as `A::Dyn`.
            let failed = std::cell::Cell::new(None);
            let normalize = |ty| {
                normalizer.normalize(ty).unwrap_or_else(|err| {
                    failed.set(Some(err));
                    ty
                })
            };
            let unsized_part = struct_tail_erasing_lifetimes(db, pointee, &normalize);
            if let Some(err) = failed.take() {
                return Err(err);
            }
            let metadata = match unsized_part.kind() {
                TyKind::Slice(_) | TyKind::Str => {
                    scalar_unit(dl, Primitive::Int(dl.ptr_sized_integer(), false))
                }
                TyKind::Dynamic(..) => {
                    let mut vtable = scalar_unit(dl, Primitive::Pointer(AddressSpace::ZERO));
                    vtable.valid_range_mut().start = 1;
                    vtable
                }
                _ => {
                    // pointee is sized
                    return Ok(intern_layout(Layout::scalar(dl, data_ptr)));
                }
            };

            // Effectively a (ptr, meta) tuple.
            LayoutData::scalar_pair(dl, data_ptr, metadata)
        }
        TyKind::Never => LayoutData::never_type(dl),
        TyKind::FnDef(..) => LayoutData::unit(dl, true),
        TyKind::Dynamic(..) | TyKind::Foreign(_) => LayoutData::unit(dl, false),
        TyKind::FnPtr(..) => {
            let mut ptr = scalar_unit(dl, Primitive::Pointer(dl.instruction_address_space));
            ptr.valid_range_mut().start = 1;
            Layout::scalar(dl, ptr)
        }
        TyKind::Closure(..) | TyKind::CoroutineClosure(..) => {
            // Not the layout of the tuple of captures: a tuple's last field can be unsized and
            // so stays last, where a closure's fields are all free to move.
            let upvars = ty.closure_upvars_ty().expect("a closure");
            let upvars = rustc_type_ir::inherent::Ty::tuple_fields(upvars);
            univariant(&cx, db, frame, upvars.iter(), &trait_env, StructKind::AlwaysSized)?
        }
        TyKind::Coroutine(id, args) => {
            // Laid out as rustc's `coroutine_layout` lays it out: what was captured and where
            // the body left off, then for each place it yields what is kept there, one such
            // set over another, from the runtime body as rustc's coroutine transform has it.
            // `CoroutineFrame` says which field is which.
            let args = args.as_coroutine();
            let parent_args = GenericArgs::new_from_slice(args.parent_args());
            let owner = crate::mir::MirOwner::coroutine(db, id.0, args);
            let runtime = crate::mir::MirStage::Runtime;
            let body = db
                .monomorphized_mir(owner, runtime, parent_args.store(), trait_env.clone()) // clone: two `u32` handles
                .map_err(|_| LayoutError::Unknown)?;
            let held = body.held_locals_in(frame);
            let upvar_tys = rustc_type_ir::inherent::Ty::tuple_fields(args.tupled_upvars_ty());
            let mut upvars = Vec::with_capacity_in(upvar_tys.len(), frame);
            for ty in upvar_tys.iter() {
                // clone: the env is two `u32` handles
                upvars.push(field_layout(db, ty.store(), trait_env.clone())?);
            }
            // Each local is a `MaybeUninit` of its type, as rustc has it: nothing of it is a
            // niche.
            let mut locals = Vec::with_capacity_in(held.locals.len(), frame);
            for &local in held.locals.iter() {
                // clone: a stored type and the env are `u32` handles
                let of = field_layout(db, body.locals[local].ty.clone(), trait_env.clone())?;
                let mut layout = LayoutData::unit(dl, true);
                layout.size = of.size;
                layout.align = of.align;
                locals.push(layout);
            }
            // alloc: `layout_of_coroutine` takes the prefix by value and pushes the state and the
            // promoted locals onto it; this is its one block, at the most it can hold.
            let mut prefix =
                IndexVec::<RustcFieldIdx, _>::with_capacity(upvars.len() + 1 + locals.len());
            prefix.extend(upvars.iter());
            let locals: &[&Layout] = frame.alloc_slice_from_iter(locals.iter());
            let locals: &[&&Layout] = frame.alloc_slice_from_iter(locals.iter());
            let locals = IndexSlice::<usize, _>::from_raw(locals);
            // The body has not started, has returned or has panicked, and holds nothing then.
            // alloc: `layout_of_coroutine` takes `IndexVec`s (R-abi lifts this)
            let mut kept: IndexVec<RustcEnumVariantIdx, IndexVec<RustcFieldIdx, usize>> =
                IndexVec::with_capacity(CoroutineFrame::SUSPENDED + held.yields.len());
            kept.extend((0..CoroutineFrame::SUSPENDED).map(|_| IndexVec::new()));
            // alloc: one exact `IndexVec` per yield, as `layout_of_coroutine` takes them
            kept.extend(held.yields.iter().map(|it| it.kept.iter().copied().collect()));
            // alloc: `layout_of_coroutine` reads the conflicts as a `BitMatrix`
            let mut conflicts = BitMatrix::new(locals.len(), locals.len());
            for a in 0..locals.len() {
                for b in held.conflicts_of(a) {
                    conflicts.insert(a, b);
                }
            }
            let state = Layout::scalar(dl, CoroutineFrame::state_scalar(held.yields.len()));
            let state = &state;
            cx.calc.layout_of_coroutine(locals, prefix, &kept, &conflicts, |_| &state)?
        }
        TyKind::CoroutineWitness(_, _) => {
            return Err(LayoutError::NotImplemented);
        }

        TyKind::Pat(ty, pat) => {
            // clone: a pattern type's layout is its base's with a narrower valid range, made once per pattern type into its memo
            let mut layout = db.layout_of_ty(ty.store(), trait_env.clone())?.0.clone();
            match pat.kind() {
                PatternKind::Range { start, end } => {
                    if let BackendRepr::Scalar(scalar) = &mut layout.backend_repr {
                        scalar.valid_range_mut().start = extract_const_value(start)?
                            .try_to_bits(db, trait_env.as_ref(db))
                            .ok_or(LayoutError::Unknown)?;

                        scalar.valid_range_mut().end = extract_const_value(end)?
                            .try_to_bits(db, trait_env.as_ref(db))
                            .ok_or(LayoutError::Unknown)?;

                        // FIXME(pattern_types): create implied bounds from pattern types in signatures
                        // that require that the range end is >= the range start so that we can't hit
                        // this error anymore without first having hit a trait solver error.
                        // Very fuzzy on the details here, but pattern types are an internal impl detail,
                        // so we can just go with this for now
                        if scalar.is_signed() {
                            let range = scalar.valid_range_mut();
                            let start = layout.size.sign_extend(range.start);
                            let end = layout.size.sign_extend(range.end);
                            if end < start {
                                return Err(LayoutError::HasErrorType);
                            }
                        } else {
                            let range = scalar.valid_range_mut();
                            if range.end < range.start {
                                return Err(LayoutError::HasErrorType);
                            }
                        };

                        let niche = Niche {
                            offset: Size::ZERO,
                            value: scalar.primitive(),
                            valid_range: scalar.valid_range(target),
                        };

                        layout.largest_niche = Some(niche);
                    } else {
                        panic!("pattern type with range but not scalar layout: {ty:?}, {layout:?}")
                    }
                }
                PatternKind::NotNull => {
                    if let BackendRepr::Scalar(scalar)
                    | BackendRepr::ScalarPair { a: scalar, b: _, .. } = &mut layout.backend_repr
                    {
                        scalar.valid_range_mut().start = 1;
                        let niche = Niche {
                            offset: Size::ZERO,
                            value: scalar.primitive(),
                            valid_range: scalar.valid_range(target),
                        };

                        layout.largest_niche = Some(niche);
                    } else {
                        panic!(
                            "pattern type with `!null` pattern but not scalar/pair layout: {ty:?}, {layout:?}"
                        )
                    }
                }

                PatternKind::Or(variants) => match variants[0].kind() {
                    PatternKind::Range { .. } => {
                        if let BackendRepr::Scalar(scalar) = &mut layout.backend_repr {
                            let mut ranges = Vec::with_capacity_in(variants.len(), frame);
                            for pat in variants.iter() {
                                let PatternKind::Range { start, end } = pat.kind() else {
                                    return Err(LayoutError::Unknown);
                                };
                                ranges.push((
                                    extract_const_value(start)?
                                        .try_to_bits(db, trait_env.as_ref(db))
                                        .ok_or(LayoutError::Unknown)?,
                                    extract_const_value(end)?
                                        .try_to_bits(db, trait_env.as_ref(db))
                                        .ok_or(LayoutError::Unknown)?,
                                ));
                            }
                            let mut variants = ranges;
                            if !scalar.is_signed() {
                                return Err(LayoutError::HasErrorType);
                            }
                            variants.sort_unstable();
                            if variants.len() != 2 {
                                return Err(LayoutError::HasErrorType);
                            }

                            // first is the one starting at the signed in range min
                            let mut first = variants[0];
                            let mut second = variants[1];
                            if second.0
                                == layout.size.truncate(layout.size.signed_int_min() as u128)
                            {
                                (second, first) = (first, second);
                            }

                            if layout.size.sign_extend(first.1) >= layout.size.sign_extend(second.0)
                            {
                                return Err(LayoutError::HasErrorType);
                            }
                            if layout.size.signed_int_max() as u128 != second.1 {
                                return Err(LayoutError::HasErrorType);
                            }

                            // Now generate a wrapping range (which aren't allowed in surface syntax).
                            scalar.valid_range_mut().start = second.0;
                            scalar.valid_range_mut().end = first.1;

                            let niche = Niche {
                                offset: Size::ZERO,
                                value: scalar.primitive(),
                                valid_range: scalar.valid_range(target),
                            };

                            layout.largest_niche = Some(niche);
                        } else {
                            panic!(
                                "pattern type with range but not scalar layout: {ty:?}, {layout:?}"
                            )
                        }
                    }
                    PatternKind::NotNull => panic!("or patterns can't contain `!null` patterns"),
                    PatternKind::Or(..) => panic!("patterns cannot have nested or patterns"),
                },
            }
            // Pattern types contain their base as their sole field.
            // This allows the rest of the compiler to process pattern types just like
            // single field transparent Adts, and only the parts of the compiler that
            // specifically care about pattern types will have to handle it.
            layout.fields = FieldsShape::Arbitrary {
                // alloc: the one field of a pattern type's layout, kept in its memo
                offsets: [Size::ZERO].into_iter().collect(),
                // alloc: as above
                in_memory_order: [RustcFieldIdx::new(0)].into_iter().collect(),
            };
            layout
        }
        TyKind::UnsafeBinder(_) => {
            return Err(LayoutError::NotImplemented);
        }

        TyKind::Error(_) => return Err(LayoutError::HasErrorType),
        TyKind::Placeholder(_)
        | TyKind::Bound(..)
        | TyKind::Infer(..)
        | TyKind::Param(..)
        | TyKind::Alias(..) => {
            return Err(LayoutError::HasPlaceholder);
        }
    };
    Ok(intern_layout(result))
}

fn layout_of_ty_cycle_result(
    _: &Db,
    _: blaze_query::db::Id,
    _: StoredTy,
    _: StoredParamEnvAndCrate,
) -> Result<Interned<'static, InternedLayout>, LayoutError> {
    Err(LayoutError::RecursiveTypeWithoutIndirection)
}

fn extract_const_value<'db>(ct: Const<'db>) -> Result<ValueConst<'db>, LayoutError> {
    match ct.kind() {
        ConstKind::Value(cv) => Ok(cv),
        ConstKind::Param(_)
        | ConstKind::Expr(_)
        | ConstKind::Alias(..)
        | ConstKind::Infer(_)
        | ConstKind::Bound(..)
        | ConstKind::Placeholder(_) => {
            if ct.has_param() {
                Err(LayoutError::HasPlaceholder)
            } else {
                Err(LayoutError::Unknown)
            }
        }
        ConstKind::Error(_) => Err(LayoutError::HasErrorConst),
    }
}

/// rustc's `struct_tail_raw`: the type a pointer to `pointee` takes its metadata from, the last
/// field of the last field ... through structs, tuples and pattern types. Each struct's last
/// field goes through `normalize` (the identity for none); a struct whose representation is
/// infinite is its own tail.
pub fn struct_tail_erasing_lifetimes<'a>(
    db: &'a Db,
    mut pointee: Ty<'a>,
    mut normalize: impl FnMut(Ty<'a>) -> Ty<'a>,
) -> Ty<'a> {
    loop {
        pointee = match pointee.kind() {
            TyKind::Adt(def, args) => {
                let AdtId::StructId(struct_id) = def.def_id() else { return pointee };
                if representability(db, def.def_id()) == Representability::Infinite {
                    return pointee;
                }
                match variant_field_ids(db, struct_id.into()).last() {
                    Some(&f) => normalize(field_ty(db, struct_id.into(), f, args)),
                    None => return pointee,
                }
            }
            TyKind::Tuple(tys) => match tys.iter().next_back() {
                Some(last) => last,
                None => return pointee,
            },
            TyKind::Pat(base, _) => base,
            _ => return pointee,
        };
    }
}

fn field_ty<'a>(
    db: &'a Db,
    def: hir_def::VariantId,
    fd: LocalFieldId,
    args: GenericArgs<'a>,
) -> Ty<'a> {
    db.field_types(def)[fd].ty().instantiate(DbInterner::new_no_crate(db), args).skip_norm_wip()
}

fn scalar_unit(dl: &TargetDataLayout, value: Primitive) -> Scalar {
    Scalar::Initialized { value, valid_range: WrappingRange::full(value.size(dl)) }
}

/// Where an `async` block keeps what has to survive from one `poll` to the next. The fields of
/// its layout are the captures and the state: where the body left off. The held locals are
/// fields of its variants, one for each place the body yields at; the ones rustc's layout
/// can't overlap with others are in the prefix after the state, and named by each variant
/// that keeps them all the same.
#[derive(Debug, Clone, Copy)]
pub struct CoroutineFrame {
    pub upvars: usize,
}

impl CoroutineFrame {
    /// The first of the states that are a place the body yielded at; before it come not yet
    /// started, returned and panicked.
    pub const SUSPENDED: usize = 3;

    /// Where the `index`th of the held locals is in the future: among what is kept at any of
    /// the places it is kept at, which are one place.
    pub fn local_offset(
        layout: &Layout,
        held: &crate::mir::HeldLocals,
        index: usize,
    ) -> Option<Size> {
        let Variants::Multiple { variants, .. } = &layout.variants else { return None };
        held.yields.iter().zip(variants.iter().skip(Self::SUSPENDED)).find_map(|(point, kept)| {
            let field = point.kept.iter().position(|&it| it == index)?;
            Some(kept.field_offsets[RustcFieldIdx::new(field)])
        })
    }

    /// The integer the state is, as wide as rustc has it.
    pub fn state_of(layout: &Layout) -> Option<Scalar> {
        match &layout.variants {
            Variants::Multiple { tag, .. } => Some(*tag),
            _ => None,
        }
    }

    fn state_scalar(yields: usize) -> Scalar {
        let last = (Self::SUSPENDED + yields - 1) as u128;
        Scalar::Initialized {
            value: Primitive::Int(Integer::fit_unsigned(last), false),
            valid_range: WrappingRange { start: 0, end: last },
        }
    }

    pub fn state(self) -> usize {
        self.upvars
    }
}

#[cfg(test)]
mod tests {
    use std::ops::ControlFlow;

    use blaze_ids::rng::SplitMix64;
    use hir_def::{ConstId, TypeAliasId};
    use rustc_type_ir::{TypeSuperVisitable, TypeVisitable, TypeVisitor};
    use blaze_query::db::FromId;

    use super::*;
    use crate::next_solver::{
        AliasConst, AliasConstKind, AliasTyKind, ClosureIdWrapper, ConstExt, FreeConstAliasId,
        IsRigid, Mutability, Pattern, Region, RegionExt, RegionKind, ScalarInt, TraitAssocTyId,
        Tys, ValTree, ValTreeKind,
    };

    /// A deterministic stream of choices: xorshift64.
    /// Field layouts of every shape the calculator treats apart: niches (`bool`, `char`, a
    /// reference), plain integers, a zero-sized and an uninhabited field, an array, a wide
    /// pointer.
    pub(super) fn sized_fields(dl: &TargetDataLayout) -> Vec<Layout> {
        let cx = LayoutCx::new(dl);
        let int = |i: Integer| Layout::scalar(dl, scalar_unit(dl, Primitive::Int(i, false)));
        let mut ptr = scalar_unit(dl, Primitive::Pointer(AddressSpace::ZERO));
        ptr.valid_range_mut().start = 1;
        let usize = scalar_unit(dl, Primitive::Int(dl.ptr_sized_integer(), false));
        let u32 = int(Integer::I32);
        let array = cx.calc.layout_of_array_like::<_, _, ()>(&u32, Some(3)).unwrap();
        [
            Layout::scalar(
                dl,
                Scalar::Initialized {
                    value: Primitive::Int(Integer::I8, false),
                    valid_range: WrappingRange { start: 0, end: 1 },
                },
            ),
            Layout::scalar(
                dl,
                Scalar::Initialized {
                    value: Primitive::Int(Integer::I32, false),
                    valid_range: WrappingRange { start: 0, end: 0x10FFFF },
                },
            ),
            Layout::scalar(dl, ptr),
            int(Integer::I8),
            int(Integer::I16),
            u32,
            int(Integer::I64),
            int(Integer::I128),
            LayoutData::unit(dl, true),
            LayoutData::never_type(dl),
            array,
            LayoutData::scalar_pair(dl, ptr, usize),
        ]
        .into()
    }

    /// `[u8]`: a field that can only be last.
    pub(super) fn unsized_field(dl: &TargetDataLayout) -> Layout {
        let u8 = Layout::scalar(dl, scalar_unit(dl, Primitive::Int(Integer::I8, false)));
        LayoutCx::new(dl).calc.layout_of_array_like::<_, _, ()>(&u8, None).unwrap()
    }

    /// What `layout_of_ty` did before: three heap buffers.
    fn univariant_with_vecs(
        cx: &LayoutCx<'_>,
        fields: &[&Layout],
        kind: StructKind,
    ) -> Result<Layout, LayoutError> {
        let fields = fields.to_vec();
        let fields = fields.iter().collect::<IndexVec<_, _>>();
        Ok(cx.calc.layout_of_univariant(&fields, &ReprOptions::default(), kind)?)
    }

    #[test]
    fn univariant_on_the_stack_matches_the_vecs() {
        let dl = TargetDataLayout::default();
        let cx = LayoutCx::new(&dl);
        let pool = sized_fields(&dl);
        let tail = unsized_field(&dl);
        let mut seed = SplitMix64::new(0x9e37_79b9_7f4a_7c15);
        for case in 0..400 {
            // Up to twelve fields.
            let len = seed.index(13);
            let mut fields: Vec<&Layout> = (0..len).map(|_| &pool[seed.index(pool.len())]).collect();
            let kinds: &[StructKind] = match case % 3 {
                // A closure: every field free to move.
                0 => &[StructKind::AlwaysSized],
                // `()`, which rustc lays out `AlwaysSized`.
                1 if fields.is_empty() => &[StructKind::AlwaysSized],
                // A tuple: the last field stays last.
                1 => &[StructKind::MaybeUnsized, StructKind::AlwaysSized],
                // A tuple with an unsized tail.
                _ => {
                    fields.push(&tail);
                    &[StructKind::MaybeUnsized]
                }
            };
            for &kind in kinds {
                assert_eq!(
                    blaze_mem::frame::task(blaze_mem::Tag::Infer, |frame| {
                        univariant_of(&cx, frame, &fields, kind)
                    }),
                    univariant_with_vecs(&cx, &fields, kind),
                    "case {case}, {len} fields",
                );
            }
        }
    }

    #[test]
    fn equal_layouts_are_one_handle() {
        let dl = TargetDataLayout::default();
        let mut pool = sized_fields(&dl);
        pool.push(unsized_field(&dl));
        let handles: Vec<_> = pool.iter().map(|it| intern_layout(it.clone())).collect();
        for (a, layout) in pool.iter().enumerate() {
            let again = intern_layout(layout.clone());
            assert_eq!(again, handles[a], "layout {a}");
            assert_eq!(again.0, *layout, "layout {a}");
            assert_eq!(
                again.content(),
                blaze_hashcons::content_of(&InternedLayout(layout.clone())),
                "layout {a}",
            );
            for (b, other) in pool.iter().enumerate() {
                assert_eq!(handles[a] == handles[b], layout == other, "layouts {a} and {b}");
            }
        }
    }

    /// Whether folding `ty` can reach an alias or an unevaluated constant: what
    /// `deeply_normalize` rewrites.
    struct FindsAlias;

    impl<'db> TypeVisitor<DbInterner<'db>> for FindsAlias {
        type Result = ControlFlow<()>;

        fn visit_ty(&mut self, ty: Ty<'db>) -> ControlFlow<()> {
            if let TyKind::Alias(..) = ty.kind() {
                return ControlFlow::Break(());
            }
            ty.super_visit_with(self)
        }

        fn visit_const(&mut self, ct: Const<'db>) -> ControlFlow<()> {
            if let ConstKind::Alias(..) = ct.kind() {
                return ControlFlow::Break(());
            }
            ct.super_visit_with(self)
        }
    }

    fn id<T: FromId>(index: u32) -> T {
        T::from_id(blaze_query::db::Id::from_u32(index))
    }

    fn ty<'db>(kind: TyKind<'db>) -> Ty<'db> {
        Ty::new_without_interner(kind)
    }

    fn args<'db>(tys: &[Ty<'db>]) -> GenericArgs<'db> {
        GenericArgs::new_from_slice(&tys.iter().map(|&it| it.into()).collect::<Vec<_>>())
    }

    fn tuple<'db>(tys: &[Ty<'db>]) -> Ty<'db> {
        ty(TyKind::Tuple(Tys::new_from_slice(tys)))
    }

    fn closure<'db>(upvars: &[Ty<'db>]) -> Ty<'db> {
        ty(TyKind::Closure(ClosureIdWrapper(id(7)), args(upvars)))
    }

    fn reference<'db>(to: Ty<'db>) -> Ty<'db> {
        ty(TyKind::Ref(Region::new_without_interner(RegionKind::ReStatic), to, Mutability::Not))
    }

    /// `<_ as Trait>::Assoc` over `args`.
    fn projection<'db>(args: GenericArgs<'db>) -> Ty<'db> {
        let alias: TypeAliasId = id(5);
        ty(TyKind::Alias(
            IsRigid::No,
            rustc_type_ir::AliasTy::from_checked_parts(
                AliasTyKind::Projection { def_id: TraitAssocTyId(alias) },
                args,
            ),
        ))
    }

    fn unevaluated<'db>(args: GenericArgs<'db>) -> Const<'db> {
        let kind = AliasConstKind::Free { def_id: FreeConstAliasId(id::<ConstId>(6)) };
        Const::new_without_interner(ConstKind::Alias(
            IsRigid::No,
            AliasConst::from_checked_parts(kind, args),
        ))
    }

    #[test]
    fn skipping_normalization_is_exactly_the_alias_free_types() {
        let u8 = ty(TyKind::Uint(UintTy::U8));
        let seven = Const::new_without_interner(ConstKind::Value(ValueConst::new(
            u8,
            ValTree::new(ValTreeKind::Leaf(ScalarInt::from(7u8))),
        )));

        let assoc = projection(args(&[u8]));
        let uneval_array = ty(TyKind::Array(u8, unevaluated(GenericArgs::empty())));
        let alias_free = [
            u8,
            ty(TyKind::Bool),
            ty(TyKind::Str),
            ty(TyKind::Never),
            tuple(&[]),
            tuple(&[u8, ty(TyKind::Char)]),
            ty(TyKind::Array(u8, seven)),
            ty(TyKind::Slice(u8)),
            reference(ty(TyKind::Slice(u8))),
            ty(TyKind::RawPtr(tuple(&[u8, ty(TyKind::Str)]), Mutability::Mut)),
            closure(&[u8, reference(u8)]),
            tuple(&[closure(&[tuple(&[u8])]), ty(TyKind::Array(u8, seven))]),
        ];
        let with_aliases = [
            assoc,
            projection(args(&[assoc])),
            tuple(&[u8, assoc]),
            tuple(&[u8, tuple(&[ty(TyKind::Slice(assoc))])]),
            reference(assoc),
            closure(&[u8, assoc]),
            closure(&[tuple(&[reference(assoc)])]),
            uneval_array,
            tuple(&[u8, uneval_array]),
            ty(TyKind::Array(u8, unevaluated(args(&[u8])))),
            ty(TyKind::Slice(uneval_array)),
            closure(&[uneval_array]),
        ];
        for (index, &it) in alias_free.iter().chain(&with_aliases).enumerate() {
            let model = it.visit_with(&mut FindsAlias).is_break();
            assert_eq!(needs_normalizing(it), model, "type {index}");
            assert_eq!(model, index >= alias_free.len(), "type {index}");
        }
    }

    /// The tail goes through tuples and pattern types, as rustc's `struct_tail_raw`, and stops at
    /// anything else; only a struct's last field is normalized.
    #[test]
    fn a_tail_goes_through_tuples_and_pattern_types() {
        let db = Db::for_tests();
        let interner = DbInterner::new_no_crate(&db);
        let u8 = ty(TyKind::Uint(UintTy::U8));
        let slice = ty(TyKind::Slice(u8));
        let not_null = |base| ty(TyKind::Pat(base, Pattern::new(interner, PatternKind::NotNull)));
        let cases = [
            (u8, u8),
            (slice, slice),
            (tuple(&[]), tuple(&[])),
            (tuple(&[u8, slice]), slice),
            (tuple(&[u8, tuple(&[ty(TyKind::Bool), ty(TyKind::Str)])]), ty(TyKind::Str)),
            (not_null(reference(u8)), reference(u8)),
            (tuple(&[u8, not_null(reference(slice))]), reference(slice)),
            (reference(slice), reference(slice)),
        ];
        let mut normalized = 0;
        for (index, (of, tail)) in cases.into_iter().enumerate() {
            let found = struct_tail_erasing_lifetimes(&db, of, |it| {
                normalized += 1;
                it
            });
            assert_eq!(found, tail, "case {index}");
        }
        assert_eq!(normalized, 0, "no struct, nothing normalized");
    }
}
