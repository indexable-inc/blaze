//! Compute the binary representation of a type

use std::fmt;

use hir_def::{
    AdtId, LocalFieldId, StructId,
    attrs::AttrFlags,
    layout::{LayoutCalculatorError, LayoutData},
};
use la_arena::{Idx, RawIdx};

use rustc_abi::{
    AddressSpace, BackendRepr, FieldsShape, Float, Integer, LayoutCalculator, Niche, Primitive,
    ReprOptions, Scalar, Size, StructKind, TargetDataLayout, WrappingRange,
};
use rustc_index::{IndexVec, bit_set::BitMatrix};
use rustc_type_ir::{
    FloatTy, IntTy, TypeVisitableExt as _, UintTy,
    inherent::{GenericArgs as _, IntoKind},
};
use triomphe::Arc;

use crate::{
    ParamEnvAndCrate,
    consteval::try_const_usize,
    db::HirDatabase,
    next_solver::{
        Const, ConstKind, DbInterner, GenericArgs, PatternKind, StoredTy, Ty, TyKind, TypingMode,
        ValueConst,
        infer::{DbInternerInferExt, traits::ObligationCause},
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

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum LayoutError {
    // FIXME: Remove more variants once they get added to LayoutCalculatorError
    BadCalc(LayoutCalculatorError<()>),
    HasErrorConst,
    HasErrorType,
    HasPlaceholder,
    InvalidSimdType,
    NotImplemented,
    RecursiveTypeWithoutIndirection,
    TargetLayoutNotAvailable,
    Unknown,
    UserReprTooSmall,
}

impl std::error::Error for LayoutError {}
impl fmt::Display for LayoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LayoutError::BadCalc(err) => err.fallback_fmt(f),
            LayoutError::HasErrorConst => write!(f, "type contains an unevaluatable const"),
            LayoutError::HasErrorType => write!(f, "type contains an error"),
            LayoutError::HasPlaceholder => write!(f, "type contains placeholders"),
            LayoutError::InvalidSimdType => write!(f, "invalid simd type definition"),
            LayoutError::NotImplemented => write!(f, "not implemented"),
            LayoutError::RecursiveTypeWithoutIndirection => {
                write!(f, "recursive type without indirection")
            }
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
    db: &'db dyn HirDatabase,
    id: StructId,
    repr_packed: bool,
    args: &GenericArgs<'db>,
    env: ParamEnvAndCrate<'db>,
    dl: &TargetDataLayout,
) -> Result<Arc<Layout>, LayoutError> {
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
    Ok(Arc::new(cx.calc.layout_of_simd_type(e_ly, e_len, repr_packed)?))
}

#[salsa::tracked(cycle_result = layout_of_ty_cycle_result, returns(clone))]
pub fn layout_of_ty_query(
    db: &dyn HirDatabase,
    ty: StoredTy,
    trait_env: StoredParamEnvAndCrate,
) -> Result<Arc<Layout>, LayoutError> {
    let krate = trait_env.krate;
    let interner = DbInterner::new_with(db, krate);
    let Ok(target) = db.target_data_layout(krate) else {
        return Err(LayoutError::TargetLayoutNotAvailable);
    };
    let dl = target;
    let cx = LayoutCx::new(dl);
    let infer_ctxt = interner.infer_ctxt().build(TypingMode::PostAnalysis);
    let cause = ObligationCause::dummy();
    let ty = infer_ctxt
        .at(&cause, trait_env.param_env(db))
        .deeply_normalize(ty.as_ref())
        .unwrap_or(ty.as_ref());
    let result = match ty.kind() {
        TyKind::Adt(def, args) => {
            match def.def_id() {
                hir_def::AdtId::StructId(s) => {
                    let repr = AttrFlags::repr(db, s.into()).unwrap_or_default();
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

            let fields = tys
                .iter()
                .map(|k| db.layout_of_ty(k.store(), trait_env.clone()))
                .collect::<Result<Vec<_>, _>>()?;
            let fields = fields.iter().map(|it| &**it).collect::<Vec<_>>();
            let fields = fields.iter().collect::<IndexVec<_, _>>();
            cx.calc.layout_of_univariant(&fields, &ReprOptions::default(), kind)?
        }
        TyKind::Array(element, count) => {
            let count = try_const_usize(db, count).ok_or(LayoutError::HasErrorConst)? as u64;
            let element = db.layout_of_ty(element.store(), trait_env)?;
            cx.calc.layout_of_array_like::<_, _, ()>(&element, Some(count))?
        }
        TyKind::Slice(element) => {
            let element = db.layout_of_ty(element.store(), trait_env)?;
            cx.calc.layout_of_array_like::<_, _, ()>(&element, None)?
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
            let normalize = |ty| {
                infer_ctxt.at(&cause, trait_env.param_env(db)).deeply_normalize(ty).unwrap_or(ty)
            };
            let unsized_part = struct_tail_erasing_lifetimes(db, pointee, &normalize);
            // FIXME(next-solver)
            /*
            if let TyKind::AssociatedType(id, subst) = unsized_part.kind(Interner) {
                unsized_part = TyKind::Alias(chalk_ir::AliasTy::Projection(ProjectionTy {
                    associated_ty_id: *id,
                    substitution: subst.clone(),
                }))
                .intern(Interner);
            }
            unsized_part = normalize(db, trait_env, unsized_part);
            */
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
                    return Ok(Arc::new(Layout::scalar(dl, data_ptr)));
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
            let fields = rustc_type_ir::inherent::Ty::tuple_fields(upvars)
                .iter()
                .map(|k| db.layout_of_ty(k.store(), trait_env.clone()))
                .collect::<Result<Vec<_>, _>>()?;
            let fields = fields.iter().map(|it| &**it).collect::<Vec<_>>();
            let fields = fields.iter().collect::<IndexVec<_, _>>();
            cx.calc.layout_of_univariant(
                &fields,
                &ReprOptions::default(),
                StructKind::AlwaysSized,
            )?
        }
        TyKind::Coroutine(id, args) => {
            // Laid out as rustc's `coroutine_layout` lays it out: what was captured and where
            // the body left off, then for each place it yields what is kept there, one such
            // set over another, from the runtime body as rustc's coroutine transform has it.
            // `CoroutineFrame` says which field is which.
            let args = args.as_coroutine();
            let parent_args = GenericArgs::new_from_slice(args.parent_args());
            let body = if crate::mir::is_by_move_coroutine(db, id.0, args) {
                db.monomorphized_runtime_mir_body_for_coroutine_by_move(
                    id.0,
                    parent_args.store(),
                    trait_env.clone(),
                )
            } else {
                db.monomorphized_runtime_mir_body_for_coroutine(
                    id.0,
                    parent_args.store(),
                    trait_env.clone(),
                )
            }
            .map_err(|e| {
                eprintln!("D1DEBUG coroutine {id:?} mir error: {e:?}");
                LayoutError::Unknown
            })?;
            let held = body.locals_held_across_yields();
            let upvars = rustc_type_ir::inherent::Ty::tuple_fields(args.tupled_upvars_ty())
                .iter()
                .map(|ty| db.layout_of_ty(ty.store(), trait_env.clone()).inspect_err(|e| eprintln!("D1DEBUG coroutine {id:?} upvar {ty:?}: {e:?}")))
                .collect::<Result<Vec<_>, _>>()?;
            // Each local is a `MaybeUninit` of its type, as rustc has it: nothing of it is a
            // niche.
            let locals = held
                .locals
                .iter()
                .map(|&local| {
                    let of = db.layout_of_ty(body.locals[local].ty.clone(), trait_env.clone()).inspect_err(|e| eprintln!("D1DEBUG coroutine {id:?} local {:?} layout: {e:?}", body.locals[local].ty.as_ref()))?;
                    let mut layout = LayoutData::unit(dl, true);
                    layout.size = of.size;
                    layout.align = of.align;
                    Ok(layout)
                })
                .collect::<Result<Vec<_>, LayoutError>>()?;
            let prefix: Vec<&Layout> = upvars.iter().map(|it| &**it).collect();
            let prefix = prefix.iter().collect::<IndexVec<RustcFieldIdx, _>>();
            let locals = locals.iter().collect::<Vec<_>>();
            let locals = locals.iter().collect::<IndexVec<usize, _>>();
            // The body has not started, has returned or has panicked, and holds nothing then.
            let mut kept: IndexVec<RustcEnumVariantIdx, IndexVec<RustcFieldIdx, usize>> =
                IndexVec::from_elem_n(IndexVec::new(), CoroutineFrame::SUSPENDED);
            kept.extend(held.yields.iter().map(|it| it.kept.iter().copied().collect()));
            let mut conflicts = BitMatrix::new(locals.len(), locals.len());
            for a in 0..locals.len() {
                for b in 0..locals.len() {
                    if held.conflict(a, b) {
                        conflicts.insert(a, b);
                    }
                }
            }
            let state = Layout::scalar(dl, CoroutineFrame::state_scalar(held.yields.len()));
            let state = &state;
            cx.calc.layout_of_coroutine(&locals, prefix, &kept, &conflicts, |_| &state)?
        }
        TyKind::CoroutineWitness(_, _) => {
            return Err(LayoutError::NotImplemented);
        }

        TyKind::Pat(ty, pat) => {
            let mut layout = (*db.layout_of_ty(ty.store(), trait_env.clone())?).clone();
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
                            let variants: Result<Vec<_>, _> = variants
                                .iter()
                                .map(|pat| match pat.kind() {
                                    PatternKind::Range { start, end } => Ok::<_, LayoutError>((
                                        extract_const_value(start)?
                                            .try_to_bits(db, trait_env.as_ref(db))
                                            .ok_or(LayoutError::Unknown)?,
                                        extract_const_value(end)?
                                            .try_to_bits(db, trait_env.as_ref(db))
                                            .ok_or(LayoutError::Unknown)?,
                                    )),
                                    PatternKind::NotNull | PatternKind::Or(_) => {
                                        Err(LayoutError::Unknown)
                                    }
                                })
                                .collect();
                            let mut variants = variants?;
                            if !scalar.is_signed() {
                                return Err(LayoutError::HasErrorType);
                            }
                            variants.sort();
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
                offsets: [Size::ZERO].into_iter().collect(),
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
    Ok(Arc::new(result))
}

fn layout_of_ty_cycle_result(
    _: &dyn HirDatabase,
    _: salsa::Id,
    _: StoredTy,
    _: StoredParamEnvAndCrate,
) -> Result<Arc<Layout>, LayoutError> {
    Err(LayoutError::RecursiveTypeWithoutIndirection)
}

fn extract_const_value<'db>(ct: Const<'db>) -> Result<ValueConst<'db>, LayoutError> {
    match ct.kind() {
        ConstKind::Value(cv) => Ok(cv),
        ConstKind::Param(_)
        | ConstKind::Expr(_)
        | ConstKind::Unevaluated(_)
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

pub fn struct_tail_erasing_lifetimes<'a>(
    db: &'a dyn HirDatabase,
    pointee: Ty<'a>,
    normalize: &dyn Fn(Ty<'a>) -> Ty<'a>,
) -> Ty<'a> {
    match pointee.kind() {
        TyKind::Adt(def, args) => {
            let struct_id = match def.def_id() {
                AdtId::StructId(id) => id,
                _ => return pointee,
            };
            if representability(db, def.def_id()) == Representability::Infinite {
                return pointee;
            }
            let data = struct_id.fields(db);
            let mut it = data.fields().iter().rev();
            match it.next() {
                Some((f, _)) => {
                    let last_field_ty = normalize(field_ty(db, struct_id.into(), f, args));
                    struct_tail_erasing_lifetimes(db, last_field_ty, normalize)
                }
                None => pointee,
            }
        }
        TyKind::Tuple(tys) => {
            if let Some(last_field_ty) = tys.iter().next_back() {
                struct_tail_erasing_lifetimes(db, last_field_ty, normalize)
            } else {
                pointee
            }
        }
        _ => pointee,
    }
}

fn field_ty<'a>(
    db: &'a dyn HirDatabase,
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
