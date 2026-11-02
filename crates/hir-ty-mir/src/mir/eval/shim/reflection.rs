//! Reflection (`core::intrinsics::reflection` behind `core::mem::type_info`), which only const
//! eval answers. Each intrinsic is answered as rustc's const eval answers it at the pinned
//! nightly (`rustc_const_eval`'s `const_eval/machine.rs`, `const_eval/type_info.rs`,
//! `const_eval/type_info/adt.rs` and `const_eval/dyn_trait.rs`): a value of one of core's
//! `type_info` types is written field by field, by the names rustc writes, and a field or variant
//! rustc does not know is refused rather than guessed. A field's id stands for the field, as
//! rustc's field representing type does.

use hir_ty::db::HirDatabase;
use hir_def::{AttrDefId, EnumVariantId};

use crate::mir::eval::word_bytes;
use crate::next_solver::{
    Binder, BoundExistentialPredicates, ExistentialPredicate, TypingMode,
    infer::{
        DbInternerInferExt,
        traits::{Obligation, ObligationCause},
    },
    obligation_ctxt::ObligationCtxt,
};

use super::*;

/// A value being made: its bytes, and which of them carry the provenance of a pointer to
/// memory the evaluator made for it.
type Made<S> = (Vec<u8>, S);

/// Gives a field of a value being made, from its name and type.
// dyn: once per reflected field (`type_info`, once per constant), which allocates; ~20 closures would each copy the walkers
type FieldBytes<'a, 'db, M> = &'a mut dyn FnMut(
    &mut Evaluator<'db, M>,
    &str,
    Ty<'db>,
) -> Result<'db, Made<<M as Machine<'db>>::Shadow>>;

impl<'db, M: Machine<'db>> Evaluator<'db, M> {
    /// Answers the reflection intrinsic `intrinsic`, or says it is not one.
    pub(super) fn exec_reflection_intrinsic(
        &mut self,
        intrinsic: Intrinsic,
        args: &[IntervalAndTy<'db>],
        destination: Interval,
        locals: &Locals<'db>,
    ) -> Result<'db, bool> {
        match intrinsic {
            Intrinsic::TypeIdTypeOf => {
                let [id] = args else { not_supported!("type_id_type_of args") };
                let ty = self.type_id_of(id)?;
                let (value, shadow) = self.reflected_type(ty, locals)?;
                self.write_shadowed(destination.addr, &value, &shadow)?;
            }
            Intrinsic::TypeIdVtable => {
                let [id, trait_] = args else { not_supported!("type_id_vtable args") };
                let ty = self.type_id_of(id)?;
                let trait_ty = self.type_id_of(trait_)?;
                self.reflected_vtable(ty, trait_ty, destination, locals)?;
            }
            Intrinsic::TypeIdIsSigned => {
                let [id] = args else { not_supported!("type_id_is_signed args") };
                let ty = self.type_id_of(id)?;
                destination.write_from_bytes(self, &[matches!(ty.kind(), TyKind::Int(_)) as u8])?;
            }
            Intrinsic::TypeIdArrayLen => {
                let [id] = args else { not_supported!("type_id_array_len args") };
                let len = match self.type_id_of(id)?.kind() {
                    TyKind::Array(_, len) => match try_const_usize(self.db, len) {
                        Some(len) => len as usize,
                        None => not_supported!("the length of an array that is not known"),
                    },
                    _ => 0,
                };
                destination.write_from_bytes(self, &word_bytes(len))?;
            }
            Intrinsic::TypeIdElementTy | Intrinsic::TypeIdPointsTo => {
                let [id] = args else { not_supported!("{intrinsic} args") };
                let inner = match (intrinsic, self.type_id_of(id)?.kind()) {
                    (Intrinsic::TypeIdElementTy, TyKind::Array(it, _) | TyKind::Slice(it)) => {
                        Some(it)
                    }
                    (Intrinsic::TypeIdPointsTo, TyKind::RawPtr(it, _) | TyKind::Ref(_, it, _)) => {
                        Some(it)
                    }
                    _ => None,
                };
                let inner = match inner {
                    Some(it) => Some(self.type_id_bytes(it)?),
                    None => None,
                };
                let type_id = self.type_id_adt()?;
                self.write_option(destination, type_id, inner, locals)?;
            }
            Intrinsic::TypeIdPointsMutably => {
                let [id] = args else { not_supported!("type_id_points_mutably args") };
                let mutably = matches!(
                    self.type_id_of(id)?.kind(),
                    TyKind::RawPtr(_, Mutability::Mut) | TyKind::Ref(_, _, Mutability::Mut)
                );
                destination.write_from_bytes(self, &[mutably as u8])?;
            }
            Intrinsic::TypeIdSizeOf => {
                let [id] = args else { not_supported!("type_id_size_of args") };
                let ty = self.type_id_of(id)?;
                let size = self.size_align_of(ty, locals)?.map(|(size, _)| word_bytes(size));
                let usize = Ty::new_uint(self.interner(), rustc_type_ir::UintTy::Usize);
                self.write_option(destination, usize, size, locals)?;
            }
            Intrinsic::TypeIdVariants => {
                let [id] = args else { not_supported!("type_id_variants args") };
                let ty = self.type_id_of(id)?;
                let variants = self.reflected_variants(ty)?;
                destination.write_from_bytes(self, &word_bytes(variants))?;
            }
            Intrinsic::TypeIdVariantName => {
                let [id, variant] = args else { not_supported!("type_id_variant_name args") };
                let ty = self.type_id_of(id)?;
                let variant = from_bytes!(usize, self.read_scalar(variant.interval)?);
                let TyKind::Adt(adt, _) = ty.kind() else {
                    // rustc's `span_bug!`.
                    not_supported!("type_id_variant_name of {ty:?}, which is not an ADT")
                };
                let name = match self.adt_variant(adt.def_id(), variant)? {
                    VariantId::EnumVariantId(it) => {
                        let variants = &it.lookup(self.db).parent.enum_variants(self.db).variants;
                        variants.get_index(variant).map(|(name, _)| name.clone()) // clone: an interned `Name`; reflection shim, only for a `type_info` intrinsic
                    }
                    VariantId::StructId(it) => Some(StructSignature::of(self.db, it).name.clone()), // clone: an interned `Name`; reflection shim, only for a `type_info` intrinsic
                    VariantId::UnionId(it) => Some(UnionSignature::of(self.db, it).name.clone()), // clone: an interned `Name`; reflection shim, only for a `type_info` intrinsic
                };
                let name = name.map(|it| it.as_str().to_owned()).unwrap_or_default(); // alloc: reflection shim, only for a `type_info` intrinsic
                self.write_str(destination, &name)?;
            }
            Intrinsic::TypeIdVariantNonExhaustive => {
                let [id, variant] = args else {
                    not_supported!("type_id_variant_non_exhaustive args")
                };
                let ty = self.type_id_of(id)?;
                let variant = from_bytes!(usize, self.read_scalar(variant.interval)?);
                let TyKind::Adt(adt, _) = ty.kind() else {
                    // rustc's `span_bug!`.
                    not_supported!("type_id_variant_non_exhaustive of {ty:?}, which is not an ADT")
                };
                // rustc's `is_field_list_non_exhaustive`: the variant's own attribute, or for a
                // struct or union the item's, wherever it is defined.
                let attrs_of: AttrDefId = match self.adt_variant(adt.def_id(), variant)? {
                    VariantId::EnumVariantId(it) => it.into(),
                    VariantId::StructId(it) => AdtId::StructId(it).into(),
                    VariantId::UnionId(it) => AdtId::UnionId(it).into(),
                };
                let non_exhaustive = self.is_non_exhaustive(attrs_of);
                destination.write_from_bytes(self, &[non_exhaustive as u8])?;
            }
            Intrinsic::TypeIdNonExhaustive => {
                let [id] = args else { not_supported!("type_id_non_exhaustive args") };
                // rustc's `is_variant_list_non_exhaustive` of an enum and
                // `is_field_list_non_exhaustive` of a struct's or union's one variant: the
                // item's attribute either way; anything but an ADT is not.
                let non_exhaustive = match self.type_id_of(id)?.kind() {
                    TyKind::Adt(adt, _) => self.is_non_exhaustive(adt.def_id().into()),
                    _ => false,
                };
                destination.write_from_bytes(self, &[non_exhaustive as u8])?;
            }
            Intrinsic::TypeIdFields => {
                let [id, variant] = args else { not_supported!("type_id_fields args") };
                let ty = self.type_id_of(id)?;
                let variant = from_bytes!(usize, self.read_scalar(variant.interval)?);
                let fields = self.reflected_fields(ty, variant)?;
                destination.write_from_bytes(self, &word_bytes(fields.len()))?;
            }
            Intrinsic::TypeIdFieldRepresentingType => {
                let [id, variant, field] = args else {
                    not_supported!("type_id_field_representing_type args")
                };
                let base = self.type_id_of(id)?;
                let variant = from_bytes!(usize, self.read_scalar(variant.interval)?);
                let field = from_bytes!(usize, self.read_scalar(field.interval)?);
                let fields = self.reflected_fields(base, variant)?.len();
                if field >= fields {
                    // alloc: error path
                    return Err(MirEvalError::UndefinedBehavior(format!(
                        "field {field} of a variant with {fields}"
                    )));
                }
                let id = self.type_id(TypeIdOf::Field { base, variant, field });
                destination.write_from_bytes(self, &id)?;
            }
            Intrinsic::FieldRepresentingTypeActualTypeId
            | Intrinsic::FieldRepresentingTypeName
            | Intrinsic::FieldRepresentingTypeOffset => {
                let [id] = args else { not_supported!("{intrinsic} args") };
                let Some(&TypeIdOf::Field { base, variant, field }) = self.type_id_entry(id)?
                else {
                    not_supported!("{intrinsic} of a `TypeId` that is not a field's")
                };
                let (field_name, field_ty) =
                    self.reflected_fields(base, variant)?.swap_remove(field);
                match intrinsic {
                    Intrinsic::FieldRepresentingTypeActualTypeId => {
                        let id = self.type_id_bytes(field_ty)?;
                        destination.write_from_bytes(self, &id)?;
                    }
                    Intrinsic::FieldRepresentingTypeName => {
                        self.write_str(destination, &field_name)?
                    }
                    _ => {
                        let offset = self.reflected_offset(base, variant, field, locals)?;
                        destination.write_from_bytes(self, &word_bytes(offset))?;
                    }
                }
            }
            Intrinsic::TypeIdGenerics => {
                let [id] = args else { not_supported!("type_id_generics args") };
                let ty = self.type_id_of(id)?;
                let (value, shadow) = self.reflected_generics(ty, locals)?;
                self.write_shadowed(destination.addr, &value, &shadow)?;
            }
            Intrinsic::TypeIdFunctionPtr => {
                let [id] = args else { not_supported!("type_id_function_ptr args") };
                let ty = self.type_id_of(id)?;
                let Some(fn_ptr) = self.lang_items().TypeInfoFnPtr else {
                    not_supported!("no `FnPtr` (lang item `FnPtr`) in core")
                };
                let fn_ptr = Ty::new_adt(self.interner(), fn_ptr.into(), GenericArgs::empty());
                let TyKind::FnPtr(sig, header) = ty.kind() else {
                    self.write_option(destination, fn_ptr, None::<Vec<u8>>, locals)?;
                    return Ok(true);
                };
                // rustc skips the binder too (its "FIXME: handle lifetime bounds").
                let sig = sig.skip_binder();
                let inputs: Vec<Ty<'db>> = sig.inputs().iter().copied().collect(); // alloc: reflection shim, only for a `type_info` intrinsic
                let value = self.reflected_fn_ptr(
                    fn_ptr,
                    &inputs,
                    sig.output(),
                    header.fn_sig_kind,
                    locals,
                )?;
                let (value, shadow) = self.reflected_some(fn_ptr, value, locals)?;
                self.write_shadowed(destination.addr, &value, &shadow)?;
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// The bytes of the `TypeId` of `ty`, as a `TypeId` is of it.
    fn type_id_bytes(&mut self, ty: Ty<'db>) -> Result<'db, Vec<u8>> {
        let ty = self.type_id_ty(ty)?;
        Ok(self.type_id(TypeIdOf::Ty(ty)).to_vec()) // alloc: reflection shim, only for a `type_info` intrinsic
    }

    /// `bytes` the evaluator computed, which carry no pointer.
    fn made(bytes: Vec<u8>) -> Made<M::Shadow> {
        let shadow = M::Shadow::init(bytes.len());
        (bytes, shadow)
    }

    /// The `TypeId` of `ty`, as a field of a value being made.
    fn reflected_type_id(&mut self, ty: Ty<'db>) -> Result<'db, Made<M::Shadow>> {
        self.type_id_bytes(ty).map(Self::made)
    }

    /// A value of `size` bytes with `fields` where `layout` puts them, and `tag`, with what
    /// each field carries.
    fn reflected_construct(
        &mut self,
        size: usize,
        layout: &Layout,
        tag: Option<(usize, usize, i128)>,
        fields: Vec<Made<M::Shadow>>,
    ) -> Result<'db, Made<M::Shadow>> {
        let mut shadow = M::Shadow::init(size);
        if M::CHECKS {
            for (i, (_, it)) in fields.iter().enumerate() {
                shadow.put(layout.fields.offset(i).bytes_usize(), it);
            }
        }
        let fields = fields
            .into_iter()
            .map(|(bytes, _)| IntervalOrOwned::Owned(OwnedBytes::from_vec(bytes)));
        let bytes = self.construct_with_layout(size, layout, tag, fields)?;
        Ok((bytes.into_vec(), shadow))
    }

    /// `Some(value)` of an `Option<inner>`, as [`Self::write_option`] makes it, with what
    /// `value` carries.
    fn reflected_some(
        &mut self,
        inner: Ty<'db>,
        value: Made<M::Shadow>,
        locals: &Locals<'db>,
    ) -> Result<'db, Made<M::Shadow>> {
        let Some(some) = self.lang_items().OptionSome else {
            not_supported!("no `Option` in core")
        };
        let args = GenericArgs::new_from_slice(&[inner.into()]);
        let (size, layout, tag) = self.layout_of_variant(some.into(), args, locals)?;
        self.reflected_construct(size, &layout, tag, vec![value]) // alloc: reflection shim, only for a `type_info` intrinsic
    }

    /// A `&'static [_]` or `&'static str` of `len` elements at `addr`, memory the evaluator
    /// made, carrying the provenance of that allocation.
    fn reflected_ref(&self, addr: Address, len: usize) -> Made<M::Shadow> {
        let mut bytes = addr.to_bytes().to_vec(); // alloc: reflection shim, only for a `type_info` intrinsic
        let mut shadow = M::Shadow::pointer(bytes.len(), self.ptr(addr).prov);
        let at = bytes.len();
        bytes.extend(word_bytes(len));
        if M::CHECKS {
            shadow.put(at, &M::Shadow::init(bytes.len() - at));
        }
        (bytes, shadow)
    }

    /// `core::any::TypeId`.
    fn type_id_adt(&self) -> Result<'db, Ty<'db>> {
        let Some(type_id) = self.lang_items().TypeId else { not_supported!("no `TypeId` in core") };
        Ok(Ty::new_adt(self.interner(), type_id.into(), GenericArgs::empty()))
    }

    /// Whether `def` is marked `#[non_exhaustive]`, which rustc's flags say wherever the item
    /// is defined (its `..._has_applicable_non_exhaustive` is what looks at the crate).
    fn is_non_exhaustive(&self, def: AttrDefId) -> bool {
        AttrFlags::query(self.db, def).contains(AttrFlags::NON_EXHAUSTIVE)
    }

    /// How many variants reflection says `ty` has: an enum's, and one for anything else.
    fn reflected_variants(&self, ty: Ty<'db>) -> Result<'db, usize> {
        Ok(match ty.kind() {
            TyKind::Adt(adt, _) => match adt.def_id() {
                AdtId::EnumId(it) => it.enum_variants(self.db).variants.len(),
                AdtId::StructId(_) | AdtId::UnionId(_) => 1,
            },
            _ => 1,
        })
    }

    /// The variant `index` of an ADT, in the order they are written.
    fn adt_variant(&self, adt: AdtId, index: usize) -> Result<'db, VariantId> {
        match adt {
            AdtId::StructId(it) if index == 0 => Ok(it.into()),
            AdtId::UnionId(it) if index == 0 => Ok(it.into()),
            AdtId::EnumId(it) => match it.enum_variants(self.db).variants.get_index(index) {
                Some((_, &(variant, _))) => Ok(variant.into()),
                // alloc: error path
                None => Err(MirEvalError::UndefinedBehavior(format!("variant {index} of {adt:?}"))),
            },
            // alloc: error path
            _ => Err(MirEvalError::UndefinedBehavior(format!("variant {index} of {adt:?}"))),
        }
    }

    /// The names and types of the fields of `variant` with the generic args `args`, in the
    /// order they are written.
    fn variant_fields(&self, variant: VariantId, args: GenericArgs<'db>) -> Vec<(String, Ty<'db>)> {
        let names = variant.fields(self.db).fields();
        self.db
            .field_types(variant)
            .iter()
            .map(|(id, field)| {
                let ty = field.ty().instantiate(self.interner(), args).skip_norm_wip();
                (names[id].name.as_str().to_owned(), ty) // alloc: reflection shim, only for a `type_info` intrinsic
            })
            .collect() // alloc: reflection shim, only for a `type_info` intrinsic
    }

    /// The names and types of the fields of the variant `variant` of `ty`, in the order they
    /// are written: an ADT's or a tuple's, none of anything else.
    fn reflected_fields(&self, ty: Ty<'db>, variant: usize) -> Result<'db, Vec<(String, Ty<'db>)>> {
        let variants = self.reflected_variants(ty)?;
        if variant >= variants {
            // alloc: error path
            return Err(MirEvalError::UndefinedBehavior(format!(
                "variant {variant} of a type with {variants}"
            )));
        }
        Ok(match ty.kind() {
            TyKind::Adt(adt, args) => {
                let variant = self.adt_variant(adt.def_id(), variant)?;
                self.variant_fields(variant, args)
            }
            TyKind::Tuple(fields) => {
                fields.iter().enumerate().map(|(i, ty)| (i.to_string(), ty)).collect() // alloc: reflection shim, only for a `type_info` intrinsic
            }
            _ => Vec::new(), // alloc: empty, allocates nothing
        })
    }

    /// Where the field `field` of the variant `variant` of `base` is in a value of it.
    fn reflected_offset(
        &mut self,
        base: Ty<'db>,
        variant: usize,
        field: usize,
        locals: &Locals<'db>,
    ) -> Result<'db, usize> {
        let offset = match base.kind() {
            TyKind::Adt(adt, args) => {
                let variant = self.adt_variant(adt.def_id(), variant)?;
                self.layout_of_variant(variant, args, locals)?.1.fields.offset(field)
            }
            _ => self.layout(base)?.fields.offset(field),
        };
        Ok(offset.bytes_usize())
    }

    /// A value of `variant` of an ADT with the generic args `args`, made field by field: `field`
    /// gives the bytes of each from its name and type, as rustc writes each field it knows by
    /// its name.
    fn reflected_value(
        &mut self,
        variant: VariantId,
        args: GenericArgs<'db>,
        locals: &Locals<'db>,
        field: FieldBytes<'_, 'db, M>,
    ) -> Result<'db, Made<M::Shadow>> {
        let mut values = Vec::new(); // alloc: reflection shim, only for a `type_info` intrinsic
        for (name, ty) in self.variant_fields(variant, args) {
            values.push(field(self, &name, ty)?);
        }
        let (size, layout, tag) = self.layout_of_variant(variant, args, locals)?;
        self.reflected_construct(size, &layout, tag, values)
    }

    /// A value of the struct `ty`, made field by field as [`Self::reflected_value`] does.
    fn reflected_struct(
        &mut self,
        ty: Ty<'db>,
        locals: &Locals<'db>,
        field: FieldBytes<'_, 'db, M>,
    ) -> Result<'db, Made<M::Shadow>> {
        let TyKind::Adt(adt, args) = ty.kind() else {
            not_supported!("{ty:?} in core's `type_info`, which is not a struct there")
        };
        let AdtId::StructId(it) = adt.def_id() else {
            not_supported!("{ty:?} in core's `type_info`, which is not a struct there")
        };
        self.reflected_value(it.into(), args, locals, field)
    }

    /// A value of the variant called `name` of the enum `ty`, as rustc's
    /// `project_downcast_named` and `write_discriminant` make it, its fields made as
    /// [`Self::reflected_value`] does.
    fn reflected_variant(
        &mut self,
        ty: Ty<'db>,
        name: &str,
        locals: &Locals<'db>,
        field: FieldBytes<'_, 'db, M>,
    ) -> Result<'db, Made<M::Shadow>> {
        let TyKind::Adt(adt, args) = ty.kind() else {
            not_supported!("{ty:?} in core's `type_info`, which is not an enum there")
        };
        let AdtId::EnumId(it) = adt.def_id() else {
            not_supported!("{ty:?} in core's `type_info`, which is not an enum there")
        };
        let variant: Option<EnumVariantId> = it
            .enum_variants(self.db)
            .variants
            .iter()
            .find_map(|(it, &(variant, _))| (it.as_str() == name).then_some(variant));
        let Some(variant) = variant else {
            not_supported!("no variant `{name}` in core's `{ty:?}`, which rustc writes")
        };
        self.reflected_value(variant.into(), args, locals, field)
    }

    /// The bytes of a field rustc leaves as it is: none, of a field of no size, for a variant
    /// whose payload carries nothing (`TypeKind::Str(Str)`).
    fn reflected_nothing(
        &self,
        name: &str,
        ty: Ty<'db>,
        locals: &Locals<'db>,
    ) -> Result<'db, Made<M::Shadow>> {
        match self.size_align_of(ty, locals)? {
            Some((0, _)) => Ok(Self::made(Vec::new())), // alloc: reflection shim, only for a `type_info` intrinsic
            _ => not_supported!("the field `{name}` of {ty:?}, which rustc does not write"),
        }
    }

    /// A `&'static [_]` in a field of type `slice_ref` of `len` elements, each made by
    /// `element` from its index, as rustc's `allocate_fill_and_write_slice_ptr` makes it.
    fn reflected_slice(
        &mut self,
        slice_ref: Ty<'db>,
        len: usize,
        // dyn: once per element of a reflected slice, which allocates; as `FieldBytes`
        element: &mut dyn FnMut(&mut Self, Ty<'db>, usize) -> Result<'db, Made<M::Shadow>>,
    ) -> Result<'db, Made<M::Shadow>> {
        let elem = match slice_ref.kind() {
            TyKind::Ref(_, slice, _) => match slice.kind() {
                TyKind::Slice(elem) => elem,
                _ => not_supported!("{slice_ref:?} in core's `type_info`, not a slice there"),
            },
            _ => not_supported!("{slice_ref:?} in core's `type_info`, not a slice there"),
        };
        let layout = self.layout(elem)?;
        let size = layout.size.bytes_usize();
        let align = layout.align.abi.bytes() as usize;
        let addr = self.heap_allocate(size * len, align)?;
        for i in 0..len {
            let (bytes, shadow) = element(self, elem, i)?;
            self.write_shadowed(addr.offset(i * size).aligned(align), &bytes, &shadow)?;
        }
        Ok(self.reflected_ref(addr, len))
    }

    /// A `&'static str` of `text`.
    fn reflected_str(&mut self, text: &str) -> Result<'db, Made<M::Shadow>> {
        let addr = self.heap_allocate(text.len(), 1)?;
        self.write_memory(addr, text.as_bytes())?;
        Ok(self.reflected_ref(addr, text.len()))
    }

    /// The `core::mem::type_info::Type` of `ty`, as rustc's `write_type_info` makes it.
    fn reflected_type(
        &mut self,
        ty: Ty<'db>,
        locals: &Locals<'db>,
    ) -> Result<'db, Made<M::Shadow>> {
        let Some(type_info) = self.lang_items().TypeInfo else {
            not_supported!("no `Type` (lang item `type_info`) in core")
        };
        let type_info = Ty::new_adt(self.interner(), type_info.into(), GenericArgs::empty());
        self.reflected_struct(type_info, locals, &mut |this, name, kind| match name {
            "kind" => this.reflected_type_kind(ty, kind, locals),
            other => not_supported!("the field `{other}` of `Type`, which rustc does not write"),
        })
    }

    /// The `TypeKind` of `ty`, a value of `kind`.
    fn reflected_type_kind(
        &mut self,
        ty: Ty<'db>,
        kind: Ty<'db>,
        locals: &Locals<'db>,
    ) -> Result<'db, Made<M::Shadow>> {
        let variant = match ty.kind() {
            // rustc writes a tuple's fields into the field of `TypeKind::Tuple`, which at the
            // pinned nightly has none, and so ICEs.
            TyKind::Tuple(_) => not_supported!(
                "the `Type` of the tuple {ty:?}: rustc at the pinned nightly ICEs on it, \
                 writing its fields into `TypeKind::Tuple`, which has none"
            ),
            TyKind::Array(..) => "Array",
            TyKind::Slice(_) => "Slice",
            TyKind::Adt(adt, _) => match adt.def_id() {
                AdtId::StructId(_) => "Struct",
                AdtId::UnionId(_) => "Union",
                AdtId::EnumId(_) => "Enum",
            },
            TyKind::Bool => "Bool",
            TyKind::Char => "Char",
            TyKind::Int(_) | TyKind::Uint(_) => "Int",
            TyKind::Float(_) => "Float",
            TyKind::Str => "Str",
            TyKind::Ref(..) => "Reference",
            TyKind::RawPtr(..) => "Pointer",
            TyKind::FnPtr(..) => "FnPtr",
            TyKind::Dynamic(predicates, region) => {
                return self.reflected_variant(kind, "DynTrait", locals, &mut |this, _, dyn_| {
                    this.reflected_dyn_trait(predicates, region, dyn_, locals)
                });
            }
            _ => "Other",
        };
        self.reflected_variant(kind, variant, locals, &mut |this, name, ty| {
            this.reflected_nothing(name, ty, locals)
        })
    }

    /// The `DynTrait` of `dyn` with `predicates`, as rustc's `write_dyn_trait_type_info`
    /// makes it: its principal (with the projections, without the auto traits) first, if it
    /// has one, then each auto trait as a `dyn` of that trait alone.
    fn reflected_dyn_trait(
        &mut self,
        predicates: BoundExistentialPredicates<'db>,
        region: Region<'db>,
        dyn_trait: Ty<'db>,
        locals: &Locals<'db>,
    ) -> Result<'db, Made<M::Shadow>> {
        let interner = self.interner();
        let is_auto = |it: &Binder<'db, ExistentialPredicate<'db>>| {
            matches!(it.skip_binder(), ExistentialPredicate::AutoTrait(_))
        };
        let mut traits = Vec::new(); // alloc: reflection shim, only for a `type_info` intrinsic
        if predicates.iter().any(|it| matches!(it.skip_binder(), ExistentialPredicate::Trait(_))) {
            let principal = BoundExistentialPredicates::new_from_iter(
                predicates.iter().filter(|it| !is_auto(it)),
            );
            traits.push((Ty::new_dynamic(interner, principal, region), false));
        }
        for it in predicates.iter().filter(is_auto) {
            let auto = BoundExistentialPredicates::new_from_iter([it]);
            traits.push((Ty::new_dynamic(interner, auto, region), true));
        }
        self.reflected_struct(dyn_trait, locals, &mut |this, name, field| match name {
            "predicates" => this.reflected_slice(field, traits.len(), &mut |this, predicate, i| {
                let (trait_ty, is_auto) = traits[i];
                this.reflected_struct(predicate, locals, &mut |this, name, trait_| match name {
                    "trait_ty" => {
                        this.reflected_struct(trait_, locals, &mut |this, name, _| match name {
                            "ty" => this.reflected_type_id(trait_ty),
                            "is_auto" => Ok(Self::made(vec![is_auto as u8])), // alloc: reflection shim, only for a `type_info` intrinsic
                            other => {
                                not_supported!("the field `{other}` of `Trait`, not rustc's")
                            }
                        })
                    }
                    other => {
                        not_supported!("the field `{other}` of `DynTraitPredicate`, not rustc's")
                    }
                })
            }),
            other => {
                not_supported!("the field `{other}` of `DynTrait`, which rustc does not write")
            }
        })
    }

    /// The `&'static [Generic]` of `ty`, as rustc's `write_type_id_generics` makes it: an ADT's
    /// generic args, none of a type that has none.
    fn reflected_generics(
        &mut self,
        ty: Ty<'db>,
        locals: &Locals<'db>,
    ) -> Result<'db, Made<M::Shadow>> {
        let args: Vec<_> = match ty.kind() {
            TyKind::Adt(_, args) => args.iter().collect(), // alloc: reflection shim, only for a `type_info` intrinsic
            // rustc's are the item's args and, of a closure or coroutine, the synthetic ones
            // after them (its kind, signature and upvars), which this front end's are not
            // checked against.
            TyKind::FnDef(..)
            | TyKind::Closure(..)
            | TyKind::CoroutineClosure(..)
            | TyKind::Coroutine(..) => {
                not_supported!("the generic args of {ty:?} as rustc's reflection gives them")
            }
            _ => Vec::new(), // alloc: empty, allocates nothing
        };
        let Some(generic) = self.lang_items().TypeInfoGeneric else {
            not_supported!("no `Generic` (lang item `type_info_generic`) in core")
        };
        let generic = Ty::new_adt(self.interner(), generic.into(), GenericArgs::empty());
        let slice = Ty::new_ref(
            self.interner(),
            Region::new_static(self.interner()),
            Ty::new_slice(self.interner(), generic),
            Mutability::Not,
        );
        self.reflected_slice(slice, args.len(), &mut |this, generic, i| {
            let arg = args[i];
            let (variant, of) = if let Some(ty) = arg.ty() {
                ("Type", Some(ty))
            } else if let Some(konst) = arg.konst() {
                let Some(value) = konst.try_to_value() else {
                    // rustc's `bug!`.
                    not_supported!("the const generic arg {konst:?}, which is not a value")
                };
                ("Const", Some(value.ty))
            } else {
                ("Lifetime", None)
            };
            this.reflected_variant(generic, variant, locals, &mut |this, name, payload| match of {
                // `GenericType { ty }` and `Const { ty }`: the type, and the const's type.
                Some(of) => {
                    this.reflected_struct(payload, locals, &mut |this, name, _| match name {
                        "ty" => this.reflected_type_id(of),
                        other => not_supported!("the field `{other}` of {payload:?}, not rustc's"),
                    })
                }
                None => this.reflected_nothing(name, payload, locals),
            })
        })
    }

    /// The `FnPtr` of a function pointer with `inputs`, `output` and `kind`, as rustc's
    /// `write_fn_ptr_type_info` makes it. This front end has no `#[rustc_splat]`, so no
    /// argument is splatted, which rustc encodes as `u8::MAX`
    /// (`FnSigKind::NO_SPLATTED_ARG_INDEX`).
    fn reflected_fn_ptr(
        &mut self,
        fn_ptr: Ty<'db>,
        inputs: &[Ty<'db>],
        output: Ty<'db>,
        kind: rustc_type_ir::FnSigKind<crate::next_solver::DbInterner<'db>>,
        locals: &Locals<'db>,
    ) -> Result<'db, Made<M::Shadow>> {
        self.reflected_struct(fn_ptr, locals, &mut |this, name, field| match name {
            "is_unsafe" => Ok(Self::made(vec![!kind.is_safe() as u8])), // alloc: reflection shim, only for a `type_info` intrinsic
            "abi" => match kind.abi() {
                ExternAbi::C { .. } => {
                    this.reflected_variant(field, "ExternC", locals, &mut |this, name, ty| {
                        this.reflected_nothing(name, ty, locals)
                    })
                }
                ExternAbi::Rust => {
                    this.reflected_variant(field, "ExternRust", locals, &mut |this, name, ty| {
                        this.reflected_nothing(name, ty, locals)
                    })
                }
                other => this.reflected_variant(field, "Named", locals, &mut |this, _, _| {
                    this.reflected_str(other.as_str())
                }),
            },
            "inputs" => this.reflected_slice(field, inputs.len(), &mut |this, _, i| {
                this.reflected_type_id(inputs[i])
            }),
            "output" => this.reflected_type_id(output),
            "variadic" => Ok(Self::made(vec![kind.c_variadic() as u8])), // alloc: reflection shim, only for a `type_info` intrinsic
            "is_splatted" => Ok(Self::made(vec![0])), // alloc: reflection shim, only for a `type_info` intrinsic
            "splatted_index" => Ok(Self::made(vec![u8::MAX])), // alloc: reflection shim, only for a `type_info` intrinsic
            other => not_supported!("the field `{other}` of `FnPtr`, which rustc does not write"),
        })
    }

    /// `type_id_vtable` of `ty` and the `dyn` type `trait_ty`, as rustc's
    /// `type_implements_dyn_trait`: every one of the `dyn`'s predicates of `ty`, its erased
    /// regions `'static`, holds in `TypingMode::Reflection` with no ambiguity, and then the
    /// answer is `ty`'s vtable, else `None`. rustc also resolves the regions the proof
    /// constrains; with every region `'static` and the mode's impls constraining no lifetime
    /// of their own, none is left free to fail.
    fn reflected_vtable(
        &mut self,
        ty: Ty<'db>,
        trait_ty: Ty<'db>,
        destination: Interval,
        locals: &Locals<'db>,
    ) -> Result<'db, ()> {
        let TyKind::Dynamic(predicates, _) = trait_ty.kind() else {
            // rustc's `span_bug!`.
            // dyn: error path
            not_supported!("type_id_vtable for {trait_ty:?}, which is not a `dyn Trait`")
        };
        let interner = self.interner();
        let infcx = interner.infer_ctxt().build(TypingMode::Reflection);
        let mut ocx = ObligationCtxt::new(&infcx);
        let param_env = self.param_env.param_env;
        ocx.register_obligations(predicates.iter().map(|predicate| {
            let clause = predicate.with_self_ty(interner, ty);
            let clause = rustc_type_ir::fold_regions(interner, clause, |region, _| {
                match region.kind() {
                    rustc_type_ir::RegionKind::ReErased => Region::new_static(interner),
                    _ => region,
                }
            });
            Obligation::new(interner, ObligationCause::dummy(), param_env, clause)
        }));
        let holds = ocx.evaluate_obligations_error_on_ambiguity().is_empty();
        let vtable = holds.then(|| word_bytes(self.vtable_map.id(ty)));
        let Some(dyn_metadata) = self.lang_items().DynMetadata else {
            not_supported!("no `DynMetadata` in core")
        };
        let unit_ptr = Ty::new_ptr(interner, Ty::new_unit(interner), Mutability::Not);
        let dyn_metadata = Ty::new_adt(
            interner,
            dyn_metadata.into(),
            GenericArgs::new_from_slice(&[unit_ptr.into()]),
        );
        self.write_option(destination, dyn_metadata, vtable, locals)
    }
}
