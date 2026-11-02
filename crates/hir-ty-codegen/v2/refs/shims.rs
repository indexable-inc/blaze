//! The references of the shims `instance_code` builds without MIR: drop glue and the call
//! shims, as [`FunctionCx::drop_glue`], [`FunctionCx::call_once_shim`],
//! [`FunctionCx::vtable_shim`], [`FunctionCx::fn_pointer_shim`] and
//! [`FunctionCx::reify_shim`] build them. What a `guarded` cleanup, a `drop_ladder` pad or
//! the unwinding loop of `drop_elements` drops is what the code that unwound drops after it,
//! so each part is walked once: the cleanups ask for nothing the way without unwinding does
//! not. Resuming the unwinding is `outside`'s.

use hir_ty::db::HirDatabase;
use hir_def::{
    AdtId, CallableDefId, FunctionId, VariantId,
    layout::Variants,
    signatures::{StructFlags, StructSignature},
};
use hir_ty::{
    consteval::try_const_usize,
    drop::DropGlue,
    intrinsic::intrinsic_of,
    next_solver::{GenericArgs, Region, RegionExt, Ty, TyKind},
};
use hir_ty_mir::mir::location::track_caller;
use rustc_abi::ExternAbi;
use rustc_ast_ir::Mutability;
use rustc_type_ir::inherent::{AdtDef as _, GenericArgs as _, IntoKind, SliceLike as _, Ty as _};
use blaze_query::db::AsId;

use super::{Tys, told};
use crate::{CodegenError, FunctionCx, InstanceDef, coroutine_instance, implemented_fn_method};

impl<'cx, 'db> super::Walk<'_, 'cx, 'db> {
    /// `drop_glue`: the body of `drop_in_place::<ty>`, handed metadata when `ty` is unsized.
    pub(super) fn drop_glue(&mut self, ty: Ty<'db>) -> Result<(), String> {
        let meta = self.fx.is_unsized(ty);
        self.drop_contents(ty, meta)
    }

    /// `drop_contents`: what dropping a value of `ty` in place does, `meta` saying whether
    /// its metadata is at hand.
    fn drop_contents(&mut self, ty: Ty<'db>, meta: bool) -> Result<(), String> {
        let db = self.db();
        let ty = self.fx.normalize(ty);
        match ty.kind() {
            TyKind::Adt(adt_def, subst) => {
                let boxed = adt_def.is_box();
                if boxed {
                    // `box_pointer`, and the pointee dropped; the cleanup that frees the
                    // memory when that unwinds is the `Drop` impl and the fields below.
                    self.fx.layout(ty).map_err(told)?;
                    let pointee = subst.type_at(0);
                    let pointee_meta = self.fx.is_unsized(pointee);
                    let pointee = self.fx.normalize(pointee);
                    self.drop_normalized(pointee, pointee_meta)?;
                }
                if boxed || adt_def.destructor(self.fx.interner).is_some() {
                    self.call_drop_impl(ty)?;
                }
                self.drop_adt_fields(ty, meta)
            }
            TyKind::Tuple(tys) => self.drop_tuple_fields(ty, tys.iter()),
            TyKind::Closure(..) | TyKind::CoroutineClosure(..) => {
                let Some(TyKind::Tuple(tys)) = ty.closure_upvars_ty().map(|it| it.kind()) else {
                    // alloc: the error's message
                    return Err("drop glue for a closure without tupled upvars".to_owned());
                };
                self.drop_tuple_fields(ty, tys.iter())
            }
            TyKind::Coroutine(..) => self.drop_coroutine(ty),
            TyKind::Array(element, len) => {
                if try_const_usize(db, len).is_none() {
                    // alloc: the error's message
                    return Err("array with unevaluated length".to_owned());
                }
                self.drop_elements(element)
            }
            TyKind::Slice(element) => {
                if !meta {
                    // alloc: the error's message
                    return Err("drop of a slice without a length".to_owned());
                }
                self.drop_elements(element)
            }
            // Through the vtable's first entry.
            TyKind::Dynamic(..) => match meta {
                true => Ok(()),
                // alloc: the error's message
                false => Err("drop of `dyn` without a vtable".to_owned()),
            },
            // alloc: the error's message
            _ => Err(format!("drop glue for {ty:?}")),
        }
    }

    /// `drop_value` of a value of `ty`, which is normalized: a call of its glue, handed the
    /// metadata when `meta`.
    fn drop_normalized(&mut self, ty: Ty<'db>, meta: bool) -> Result<(), String> {
        if self.fx.glue_of(ty) == DropGlue::None {
            return Ok(());
        }
        if !meta && self.fx.is_unsized(ty) {
            // alloc: the error's message
            return Err(format!("drop of unsized {ty:?} without metadata"));
        }
        self.call(InstanceDef::DropGlue, GenericArgs::new_from_slice(&[ty.into()]))
    }

    /// `drop_tuple_fields`: each field of the tuple or closure `ty` that needs dropping.
    fn drop_tuple_fields(
        &mut self,
        ty: Ty<'db>,
        fields: impl Iterator<Item = Ty<'db>>,
    ) -> Result<(), String> {
        self.fx.layout(ty).map_err(told)?;
        for field in fields {
            let field = self.fx.normalize(field);
            self.drop_normalized(field, false)?;
        }
        Ok(())
    }

    /// `drop_elements`: the glue of `element`, which every element of an array or slice is
    /// dropped with.
    fn drop_elements(&mut self, element: Ty<'db>) -> Result<(), String> {
        self.fx.layout(element).map_err(told)?;
        let element = self.fx.normalize(element);
        self.drop_normalized(element, false)
    }

    /// `drop_coroutine`: the `async` block is dropped by polling it with no context.
    fn drop_coroutine(&mut self, ty: Ty<'db>) -> Result<(), String> {
        let Some((def, parent_args)) = coroutine_instance(self.db(), ty) else {
            // alloc: the error's message
            return Err(format!("drop glue for {ty:?}"));
        };
        let body = self.fx.body_types(def, parent_args).map_err(told)?;
        let Some(poll_ty) = &body.poll else {
            // alloc: the error's message
            return Err("async block whose body is not one".to_owned());
        };
        let poll_ty = poll_ty.as_ref();
        // The pinned reference and the context, as many of them as the body takes.
        // alloc: at most two, inline in `Tys`
        let arg_tys: Tys<'db> = body.params.iter().take(2).map(|it| it.as_ref()).collect();
        self.fx.layout(poll_ty).map_err(told)?;
        self.emit_call(def, parent_args, &arg_tys, poll_ty)
    }

    /// `call_drop_impl`: `<ty as Drop>::drop` on a `&mut ty`.
    fn call_drop_impl(&mut self, ty: Ty<'db>) -> Result<(), String> {
        let interner = self.fx.interner;
        let Some(drop_fn) = interner.lang_items().Drop_drop else {
            // alloc: the error's message
            return Err("no `Drop::drop` lang item".to_owned());
        };
        let reference = Ty::new_ref(interner, Region::new_erased(interner), ty, Mutability::Mut);
        let args = GenericArgs::new_from_slice(&[ty.into()]);
        self.call_direct(drop_fn, args, &[reference], Ty::new_unit(interner))
    }

    /// `drop_adt_fields`: the fields of a struct that is not `ManuallyDrop`, or of each
    /// variant an enum's layout has, which a switch on its discriminant picks.
    fn drop_adt_fields(&mut self, ty: Ty<'db>, meta: bool) -> Result<(), String> {
        let db = self.db();
        let TyKind::Adt(adt_def, subst) = ty.kind() else { return Ok(()) };
        let layout = self.fx.layout(ty).map_err(told)?;
        match adt_def.def_id() {
            AdtId::UnionId(_) => Ok(()),
            AdtId::StructId(id) => {
                let flags = StructSignature::of(db, id).flags;
                if flags.contains(StructFlags::IS_MANUALLY_DROP) {
                    return Ok(());
                }
                self.drop_fields(id.into(), subst, ty, meta)
            }
            AdtId::EnumId(id) => {
                let variants = &id.enum_variants(db).variants;
                match &layout.variants {
                    Variants::Empty => Ok(()),
                    Variants::Single { index } => {
                        let variant = variants
                            .values()
                            .map(|&(variant, _)| variant)
                            .find(|variant| variant.index(db) == index.0);
                        match variant {
                            Some(variant) => self.drop_fields(variant.into(), subst, ty, meta),
                            None => Ok(()),
                        }
                    }
                    Variants::Multiple { .. } => {
                        for &(variant, _) in variants.values() {
                            db.const_eval_discriminant(variant).map_err(|e| {
                                // alloc: the error's message
                                told(CodegenError::Frontend(format!("discriminant: {e:?}")))
                            })?;
                        }
                        for &(variant, _) in variants.values() {
                            self.drop_fields(variant.into(), subst, ty, meta)?;
                        }
                        Ok(())
                    }
                }
            }
        }
    }

    /// `drop_fields`: each field of `variant` of `container` that needs dropping. An unsized
    /// one is handed the container's metadata; a sized one a packed struct leaves misaligned
    /// is moved out first (`aligned_for_drop`).
    fn drop_fields(
        &mut self,
        variant: VariantId,
        subst: GenericArgs<'db>,
        container: Ty<'db>,
        meta: bool,
    ) -> Result<(), String> {
        let interner = self.fx.interner;
        let field_types = self.db().field_types(variant);
        for (_, field) in field_types.iter() {
            let field = field.ty().instantiate(interner, subst).skip_norm_wip();
            let field = self.fx.normalize(field);
            if self.fx.glue_of(field) == DropGlue::None {
                continue;
            }
            if self.fx.is_unsized(field) {
                // `unsized_field_addr` finds where the tail is.
                let mut tail = field;
                while matches!(tail.kind(), TyKind::Adt(..) | TyKind::Tuple(_))
                    && self.fx.is_unsized(tail)
                {
                    tail = self.fx.last_field(tail).map_err(told)?.1;
                }
                self.drop_normalized(field, meta)?;
            } else {
                if self.fx.pack(container).is_some() {
                    self.fx.layout(field).map_err(told)?;
                }
                self.drop_normalized(field, false)?;
            }
        }
        Ok(())
    }

    /// `call_once_shim`: the closure body, which borrows the closure, called by the shim
    /// that owns it and drops it after; or, for a body that takes the closure by value, the
    /// body handed it.
    pub(super) fn call_once_shim(&mut self, ty: Ty<'db>) -> Result<(), String> {
        let TyKind::Closure(id, generic_args) = ty.kind() else {
            // alloc: the error's message
            return Err(format!("call_once shim for {ty:?}"));
        };
        let parent_args = GenericArgs::new_from_slice(generic_args.as_closure().parent_args());
        let def = InstanceDef::Closure(id.0.as_id());
        let body = self.fx.body_types(def, parent_args).map_err(told)?;
        self.fx.body_signature(body).map_err(told)?;
        let Some(receiver) = body.params.first() else {
            // alloc: the error's message
            return Err("closure without a receiver".to_owned());
        };
        let receiver_ty = receiver.as_ref();
        let by_value = receiver_ty.as_reference().is_none();
        let layout = self.fx.layout(receiver_ty).map_err(told)?;
        self.fx.pass_mode(&layout, false).map_err(told)?;
        self.call(def, parent_args)?;
        if by_value {
            return Ok(());
        }
        self.drop_value(ty)
    }

    /// `vtable_shim`: the method, handed the receiver the shim has a pointer to; for an
    /// `Fn`-trait method written for a type (`spread_shim`), with the arguments put back in
    /// their tuple.
    pub(super) fn vtable_shim(
        &mut self,
        func: FunctionId,
        args: GenericArgs<'db>,
    ) -> Result<(), String> {
        let db = self.db();
        let def = InstanceDef::Fn(func);
        if implemented_fn_method(db, self.fx.interner.lang_items(), func).is_some() {
            if track_caller(db, func) {
                return Err(
                    // alloc: the error's message
                    "an `Fn`-trait method in a vtable that tracks its caller".to_owned()
                );
            }
            let body = self.fx.body_types(def, args).map_err(told)?;
            let Some(receiver) = body.params.first() else {
                // alloc: the error's message
                return Err("an `Fn`-trait method without a receiver".to_owned());
            };
            let receiver_ty = receiver.as_ref();
            let dst_ty = body.returned.as_ref();
            let (tuple_ty, _) = self.fx.spread_shim_abi(body).map_err(told)?;
            self.fx.layout(dst_ty).map_err(told)?;
            self.fx.layout(tuple_ty).map_err(told)?;
            return self.emit_call(def, args, &[receiver_ty, tuple_ty], dst_ty);
        }
        let body = self.fx.body_types(def, args).map_err(told)?;
        self.fx.body_signature(body).map_err(told)?;
        let Some(receiver) = body.params.first() else {
            // alloc: the error's message
            return Err("vtable method without a receiver".to_owned());
        };
        let layout = self.fx.layout(receiver.as_ref()).map_err(told)?;
        self.fx.pass_mode(&layout, false).map_err(told)?;
        // `call_by_pointer`; a method that tracks its caller is handed the place of the
        // trait's declaration, data of the shim's own.
        self.call(def, args)
    }

    /// `fn_pointer_shim`: the closure body, handed a receiver it does not read.
    pub(super) fn fn_pointer_shim(&mut self, ty: Ty<'db>) -> Result<(), String> {
        let TyKind::Closure(id, generic_args) = ty.kind() else {
            // alloc: the error's message
            return Err(format!("fn pointer shim for {ty:?}"));
        };
        let parent_args = GenericArgs::new_from_slice(generic_args.as_closure().parent_args());
        let def = InstanceDef::Closure(id.0.as_id());
        let body = self.fx.body_types(def, parent_args).map_err(told)?;
        self.fx.body_signature(body).map_err(told)?;
        self.fx.closure_pointer_abi(body).map_err(told)?;
        self.call(def, parent_args)
    }

    /// `reify_shim`: what the fn item `ty` names, called with the arguments the shim is
    /// handed (a `rust-call` tuple put back together), or the fn pointer the receiver points
    /// to, called through it.
    pub(super) fn reify_shim(&mut self, ty: Ty<'db>, receiver: bool) -> Result<(), String> {
        let (fn_sig, ..) = self.fx.reify_signature(ty, receiver).map_err(told)?;
        let dst_ty = fn_sig.output();
        self.fx.layout(dst_ty).map_err(told)?;
        let abi = if receiver { ExternAbi::Rust } else { fn_sig.abi() };
        let inputs = FunctionCx::abi_inputs(abi, fn_sig.inputs()).map_err(told)?;
        let mut arg_tys = Tys::with_capacity(inputs.len());
        for &input in inputs.iter() {
            self.fx.layout(input).map_err(told)?;
            arg_tys.push(input);
        }
        if abi == ExternAbi::RustCall {
            let Some((&tuple, prefix)) = fn_sig.inputs().split_last() else {
                // alloc: the error's message
                return Err("a rust-call signature without its tuple".to_owned());
            };
            self.fx.layout(tuple).map_err(told)?;
            arg_tys.truncate(prefix.len());
            arg_tys.push(tuple);
        }
        match ty.kind() {
            TyKind::FnDef(def, generic_args) => match def.0 {
                CallableDefId::FunctionId(func) => {
                    if let Some(intrinsic) =
                        // alloc: the error's message
                        intrinsic_of(self.db(), func).map_err(|it| it.to_string())?
                    {
                        // alloc: the error's message
                        return Err(format!("shim for the intrinsic `{intrinsic}`"));
                    }
                    // A callee that tracks its caller is handed the place of its
                    // declaration, data of the shim's own.
                    self.call_direct(func, generic_args.skip_binder(), &arg_tys, dst_ty)
                }
                // The fields are written in place.
                CallableDefId::StructId(_) | CallableDefId::EnumVariantId(_) => {
                    match dst_ty.kind() {
                        TyKind::Adt(..) => Ok(()),
                        // alloc: the error's message
                        _ => Err(format!("constructor of {dst_ty:?}")),
                    }
                }
            },
            // alloc: the error's message
            _ if !receiver => Err("a fn pointer is only reified for a vtable".to_owned()),
            _ => self.emit_pointer_call(fn_sig.abi(), None, &arg_tys, dst_ty),
        }
    }
}
