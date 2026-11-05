//! Types, and the declarations of local structs and enums.
//!
//! As charon-driver's `translate_ty`: a tuple is the builtin ADT `TypeDeclId::UNIT` applied to
//! its field types (the Aeneas preset generates no tuple declarations), a local struct or enum
//! its declaration's id. Where a region is depends on what the type is part of ([`Regions`]).

use charon_lib::ast::{
    AttrInfo, BuiltinAdt, DeBruijnVar, Field, FieldId, GenericArgs as CArgs, GenericParams,
    IntTy as CIntTy, IntegerTy, IntegerValue, PtrMetadata, RefKind, Region as CRegion, RegionId,
    ScalarTy, SeqHashMap, Span, Ty as CTy, TyKind as CTyKind, TypeDecl, TypeDeclId, TypeDeclKind,
    TypeDeclRef, TypeSource, TypeVarId, UIntTy as CUIntTy, Variant, VariantId as CVariantId,
};
use charon_lib::ids::IndexVec;
use hir_def::{AdtId, HasModule as _, VariantId, item_tree::FieldsShape};
use hir_ty::{
    db::HirDatabase as _,
    next_solver::{AdtDef, GenericArgs, Region, RegionKind, Ty, TyKind},
};
use rustc_type_ir::{
    BoundVarIndexKind, IntTy, UintTy,
    inherent::{AdtDef as _, IntoKind as _, SliceLike as _},
};

use crate::{Unsupported, items::Ctx, names, refuse, why};

/// Where the regions of a type come from.
pub(crate) enum Regions<'a> {
    /// A function's signature: a region is one of the function's late-bound lifetimes
    /// (`Region::Var` at the item binder) or `'static`.
    Signature,
    /// A body: every region but `'static` is a fresh body region, as charon-driver gives each
    /// erased region of rustc's MIR. The count is the body's `bound_body_regions`.
    Body(&'a mut usize),
    /// A type declaration: version 1 declares no type with lifetime parameters.
    Decl,
}

/// `ty` as Charon's type; `item` is the item it is part of, for errors.
pub(crate) fn ty<'db>(
    ctx: &mut Ctx<'db>,
    item: &str,
    ty_: Ty<'db>,
    regions: &mut Regions<'_>,
) -> Result<CTy, Unsupported> {
    Ok(match ty_.kind() {
        TyKind::Bool => CTy::mk_bool(),
        TyKind::Int(int) => match int_ty(int) {
            Some(int) => scalar(ScalarTy::Integer(IntegerTy::Signed(int))),
            None => return Err(refuse(item, format!("the integer type `{int:?}`"), why::TYPES)),
        },
        TyKind::Uint(uint) => match uint_ty(uint) {
            Some(uint) => scalar(ScalarTy::Integer(IntegerTy::Unsigned(uint))),
            None => return Err(refuse(item, format!("the integer type `{uint:?}`"), why::TYPES)),
        },
        TyKind::Never => CTyKind::Never.into_ty(),
        TyKind::Tuple(fields) => {
            let mut tys = IndexVec::new(); // alloc: a Charon type's arguments
            for field in fields.iter() {
                tys.push(ty(ctx, item, field, regions)?);
            }
            tuple(tys)
        }
        TyKind::Ref(region_, inner, mutability) => {
            let region_ = region(item, region_, regions)?;
            let inner = ty(ctx, item, inner, regions)?;
            let kind = if mutability.is_mut() { RefKind::Mut } else { RefKind::Shared };
            CTyKind::Ref(region_, inner, kind).into_ty()
        }
        TyKind::Adt(def, args) => adt(ctx, item, def, args)?,
        TyKind::RawPtr(..) => return Err(refuse(item, "a raw pointer type", why::UNSAFE)),
        TyKind::Char => return Err(refuse(item, "the type `char`", why::TYPES)),
        TyKind::Float(_) => return Err(refuse(item, "a floating-point type", why::TYPES)),
        TyKind::Str => return Err(refuse(item, "the type `str`", why::TYPES)),
        TyKind::Slice(_) => return Err(refuse(item, "a slice type", why::TYPES)),
        TyKind::Array(..) => return Err(refuse(item, "an array type", why::TYPES)),
        TyKind::Param(_) => return Err(refuse(item, "a type parameter", why::GENERICS)),
        TyKind::Alias(..) => {
            return Err(refuse(item, "an associated or opaque type", why::GENERICS));
        }
        TyKind::Dynamic(..) => return Err(refuse(item, "a `dyn` type", why::TYPES)),
        TyKind::FnDef(..) | TyKind::FnPtr(..) => {
            return Err(refuse(item, "a function item or pointer type", why::CONTROL));
        }
        TyKind::Closure(..) | TyKind::CoroutineClosure(..) => {
            return Err(refuse(item, "a closure", why::CONTROL));
        }
        TyKind::Coroutine(..) | TyKind::CoroutineWitness(..) => {
            return Err(refuse(item, "a coroutine (`async` or a generator)", why::CONTROL));
        }
        _ => return Err(refuse(item, format!("the type `{ty_:?}`"), why::TYPES)), // alloc: an error
    })
}

fn scalar(scalar: ScalarTy) -> CTy {
    CTyKind::Scalar(scalar).into_ty()
}

/// The tuple of `fields`: Charon's builtin tuple ADT, `()` when there are none.
pub(crate) fn tuple(fields: IndexVec<TypeVarId, CTy>) -> CTy {
    if fields.is_empty() {
        return CTy::mk_unit();
    }
    CTyKind::Adt(TypeDeclRef::new(
        TypeDeclId::UNIT,
        CArgs::new_types(fields),
        Some(BuiltinAdt::Tuple),
    ))
    .into_ty()
}

/// `None` for blaze's arbitrary-width integers (`IntTy::Bits`), which Charon has no type for.
pub(crate) fn int_ty(int: IntTy) -> Option<CIntTy> {
    Some(match int {
        IntTy::Isize => CIntTy::Isize,
        IntTy::I8 => CIntTy::I8,
        IntTy::I16 => CIntTy::I16,
        IntTy::I32 => CIntTy::I32,
        IntTy::I64 => CIntTy::I64,
        IntTy::I128 => CIntTy::I128,
        IntTy::Bits(_) => return None,
    })
}

/// `None` for blaze's arbitrary-width integers (`UintTy::Bits`), which Charon has no type for.
pub(crate) fn uint_ty(uint: UintTy) -> Option<CUIntTy> {
    Some(match uint {
        UintTy::Usize => CUIntTy::Usize,
        UintTy::U8 => CUIntTy::U8,
        UintTy::U16 => CUIntTy::U16,
        UintTy::U32 => CUIntTy::U32,
        UintTy::U64 => CUIntTy::U64,
        UintTy::U128 => CUIntTy::U128,
        UintTy::Bits(_) => return None,
    })
}

/// A region of a type, by where the type is.
fn region<'db>(
    item: &str,
    region_: Region<'db>,
    regions: &mut Regions<'_>,
) -> Result<CRegion, Unsupported> {
    match regions {
        Regions::Signature => match region_.kind() {
            // Bound at the function's own binder: its late-bound lifetime number `var`, which is
            // the index of the region parameter `signatures` declares for it.
            RegionKind::ReBound(BoundVarIndexKind::Bound(debruijn), bound)
                if debruijn.as_u32() == 0 =>
            {
                Ok(CRegion::Var(DeBruijnVar::new_at_zero(RegionId::from_usize(
                    bound.var.as_usize(),
                ))))
            }
            RegionKind::ReStatic => Ok(CRegion::Static),
            RegionKind::ReEarlyParam(_) => {
                Err(refuse(item, "an early-bound lifetime parameter", why::GENERICS))
            }
            _ => Err(refuse(
                item,
                format!("the lifetime `{region_:?}` in a signature"), // alloc: an error
                why::GENERICS,
            )),
        },
        Regions::Body(count) => match region_.kind() {
            RegionKind::ReStatic => Ok(CRegion::Static),
            _ => {
                let id = RegionId::from_usize(**count);
                **count += 1;
                Ok(CRegion::Body(id))
            }
        },
        Regions::Decl => Err(refuse(
            item,
            "a reference in a type declaration (a type with a lifetime parameter)",
            why::GENERICS,
        )),
    }
}

/// A struct or enum type: a local, non-generic one's declaration.
fn adt<'db>(
    ctx: &mut Ctx<'db>,
    item: &str,
    def: AdtDef,
    args: GenericArgs<'db>,
) -> Result<CTy, Unsupported> {
    let adt = def.def_id();
    if def.is_box() {
        return Err(refuse(item, "the type `Box`", why::TYPES));
    }
    if let Some(&id) = ctx.adts.get(&adt) {
        if !args.is_empty() {
            // A declaration of ours has no parameters, so this cannot happen; refuse rather
            // than drop the arguments.
            return Err(refuse(
                item,
                format!("generic arguments to `{}`", names::adt_name(ctx.db, adt)), // alloc: an error
                why::GENERICS,
            ));
        }
        return Ok(CTyKind::Adt(TypeDeclRef::new(id, CArgs::empty(), None)).into_ty());
    }
    let name = names::adt_path(ctx.db, adt).display;
    Err(if adt.krate(ctx.db) != ctx.krate {
        // alloc: an error
        refuse(item, format!("the type `{name}` of another crate"), why::TYPES)
    } else if def.is_union() {
        refuse(item, format!("the union `{name}`"), why::TYPES) // alloc: an error
    } else {
        refuse(item, format!("the generic type `{name}`"), why::GENERICS) // alloc: an error
    })
}

/// The declaration of the local struct or enum `adt`, whose id is `id`.
pub(crate) fn adt_decl<'db>(
    ctx: &mut Ctx<'db>,
    adt: AdtId,
    id: TypeDeclId,
    path: &names::ItemPath,
) -> Result<TypeDecl, Vec<Unsupported>> {
    let item = path.display.as_str();
    let mut errors = Vec::new(); // alloc: errors, once per declaration
    let kind = match adt {
        AdtId::StructId(it) => {
            TypeDeclKind::Struct(fields(ctx, item, VariantId::StructId(it), &mut errors))
        }
        AdtId::EnumId(it) => {
            let repr = AdtDef::new(adt, ctx.interner).repr(ctx.db);
            let discr_ty = discriminant_ty(repr.discr_type());
            let mut variants = IndexVec::new(); // alloc: an enum's variants
            for (name, &(variant, _)) in it.enum_variants(ctx.db).variants.iter() {
                let fields = fields(ctx, item, VariantId::EnumVariantId(variant), &mut errors);
                let discriminant = match ctx.db.const_eval_discriminant(variant) {
                    Ok(value) => discriminant(discr_ty, value),
                    Err(err) => {
                        errors.push(refuse(
                            item,
                            // alloc: an error
                            format!(
                                "the discriminant of `{}`, which did not evaluate: {err:?}",
                                name.as_str()
                            ),
                            why::OPERATIONS,
                        ));
                        continue;
                    }
                };
                variants.push_with(|id: CVariantId| Variant {
                    id,
                    span: Span::dummy(),
                    attr_info: AttrInfo::dummy_public(),
                    name: name.as_str().to_owned(), // alloc: a variant's name
                    fields,
                    discriminant,
                });
            }
            TypeDeclKind::Enum(variants)
        }
        AdtId::UnionId(_) => return Err(vec![refuse(item, "a union", why::TYPES)]), // alloc: an error
    };
    if !errors.is_empty() {
        return Err(errors);
    }
    Ok(TypeDecl {
        def_id: id,
        // clone: the declaration keeps its name; the crate's map too
        item_meta: crate::items::local_meta(path.name.clone()),
        generics: GenericParams::empty(),
        src: TypeSource::Normal,
        kind,
        layout: SeqHashMap::default(),
        ptr_metadata: PtrMetadata::None,
        marker_traits: None,
    })
}

/// The fields of `variant`, as charon-driver names them: a positional field is `_{index}`.
fn fields<'db>(
    ctx: &mut Ctx<'db>,
    item: &str,
    variant: VariantId,
    errors: &mut Vec<Unsupported>,
) -> IndexVec<FieldId, Field> {
    let data = variant.fields(ctx.db);
    let positional = matches!(data.shape, FieldsShape::Tuple);
    let mut out = IndexVec::new(); // alloc: a declaration's fields
    for (index, (local, field)) in ctx.db.field_types(variant).iter().enumerate() {
        // The declaration has no parameters (generic ones are refused before), so the field's
        // type is its own.
        let field_ty = field.ty().skip_binder();
        let ty_ = match ty(ctx, item, field_ty, &mut Regions::Decl) {
            Ok(it) => it,
            Err(err) => {
                errors.push(err);
                continue;
            }
        };
        let name = if positional {
            format!("_{index}") // alloc: a field's name
        } else {
            data.fields()[local].name.as_str().to_owned() // alloc: a field's name
        };
        out.push(Field {
            span: Span::dummy(),
            attr_info: AttrInfo::dummy_public(),
            name,
            is_positional: positional,
            ty: ty_,
        });
    }
    out
}

/// The type of the local enum `adt`'s discriminant.
pub(crate) fn enum_discriminant_ty(ctx: &Ctx<'_>, adt: AdtId) -> IntegerTy {
    discriminant_ty(AdtDef::new(adt, ctx.interner).repr(ctx.db).discr_type())
}

/// The type of an enum's discriminant, as its `repr` gives it (`isize` without one).
fn discriminant_ty(repr: rustc_abi::IntegerType) -> IntegerTy {
    use rustc_abi::{Integer, IntegerType};
    match repr {
        IntegerType::Pointer(true) => IntegerTy::Signed(CIntTy::Isize),
        IntegerType::Pointer(false) => IntegerTy::Unsigned(CUIntTy::Usize),
        IntegerType::Fixed(size, signed) => {
            let (int, uint) = match size {
                Integer::I8 => (CIntTy::I8, CUIntTy::U8),
                Integer::I16 => (CIntTy::I16, CUIntTy::U16),
                Integer::I32 => (CIntTy::I32, CUIntTy::U32),
                Integer::I64 => (CIntTy::I64, CUIntTy::U64),
                Integer::I128 => (CIntTy::I128, CUIntTy::U128),
            };
            if signed { IntegerTy::Signed(int) } else { IntegerTy::Unsigned(uint) }
        }
    }
}

/// A discriminant `value` of type `ty`, from its bits as rustc's `DiscriminantValue` has them.
fn discriminant(ty: IntegerTy, value: i128) -> IntegerValue {
    IntegerValue::from_bits(ty, value as u128)
}

/// Whether dropping a value of type `ty_` does nothing: no destructor anywhere in it. A drop of
/// such a value is a jump to its target.
pub(crate) fn trivially_droppable<'db>(ctx: &Ctx<'db>, ty_: Ty<'db>) -> bool {
    match ty_.kind() {
        TyKind::Bool
        | TyKind::Int(_)
        | TyKind::Uint(_)
        | TyKind::Char
        | TyKind::Float(_)
        | TyKind::Never
        | TyKind::Ref(..)
        | TyKind::RawPtr(..)
        | TyKind::FnDef(..)
        | TyKind::FnPtr(..) => true,
        TyKind::Tuple(fields) => fields.iter().all(|field| trivially_droppable(ctx, field)),
        TyKind::Adt(def, args) => {
            if def.is_box() || def.destructor(ctx.interner).is_some() {
                return false;
            }
            let adt = def.def_id();
            let variants: Vec<VariantId> = match adt {
                // alloc: an ADT's variants, for one drop
                AdtId::StructId(it) => vec![VariantId::StructId(it)],
                // A union never drops its fields.
                AdtId::UnionId(_) => return true,
                AdtId::EnumId(it) => it
                    .enum_variants(ctx.db)
                    .variants
                    .values()
                    .map(|&(variant, _)| VariantId::EnumVariantId(variant))
                    .collect(), // alloc: an ADT's variants, for one drop
            };
            variants.into_iter().all(|variant| {
                ctx.db.field_types(variant).values().all(|field| {
                    let field_ty = field.ty().instantiate(ctx.interner, args).skip_norm_wip();
                    trivially_droppable(ctx, field_ty)
                })
            })
        }
        _ => false,
    }
}
