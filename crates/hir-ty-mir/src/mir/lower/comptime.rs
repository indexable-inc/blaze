//! `for const` loops and `base.[f]` places (`#![feature(blaze_comptime)]`,
//! docs/v2/plan/design/comptime.md, "MIR: one generic body, unrolled at monomorphization"):
//! lowered once, generic in the loop's binder `F`, as a [`TerminatorKind::ComptimeFor`] loop
//! whose iterations end in [`TerminatorKind::ComptimeNext`] and a
//! [`ProjectionElem::ReflectedField`] place. Monomorphization expands both.

use super::*;
use crate::mir::ComptimeBinder;

/// `env` with what the body's `for const` loops assume of their binders, `infcx` given the
/// universes inference made them in: the environment the body's goals about a binder hold in
/// (`F: Copy`, `F::Type: Hash`). `env` itself, and `infcx` untouched, in a body without one.
pub(super) fn lowering_env<'db>(
    infcx: &InferCtxt<'db>,
    infer: &InferenceResult<'db>,
    env: ParamEnv<'db>,
) -> ParamEnv<'db> {
    hir_ty::comptime_env::body_env(infcx, infer, env)
}

impl<'f, 'db> MirLowerCtx<'_, 'f, 'db> {
    /// `'label: for const pat in iterable body`: the iterable for its effects, then
    ///
    /// ```text
    /// head: ComptimeFor { binder: F, domain, body: first, after: done }
    /// first: pat = <the F value, a ZST>; body; goto next
    /// next: ComptimeNext { head }        // `continue` goes here
    /// done: place = (); goto end          // `break` goes to end
    /// ```
    ///
    /// so every block of the iteration (`next` too) is dominated by `first`, which is what the
    /// expansion copies per field.
    #[expect(
        clippy::too_many_arguments,
        reason = "the expression's parts, as `lower_loop` has them"
    )]
    pub(super) fn lower_comptime_for(
        &mut self,
        current: BasicBlockId,
        place: Place<'db>,
        expr_id: ExprId,
        pat: PatId,
        iterable: ExprId,
        body: ExprId,
        label: Option<LabelId>,
    ) -> Result<'db, Option<BasicBlockId>> {
        let Some(checked) = self.infer.comptime_loop(expr_id) else {
            // Inference reported why (a filtered domain, an iterable that is no domain).
            not_supported!(
                "`for const` (expression {expr_id:?}) that inference did not check: a filtered \
                 domain (`Fields<T, P>` with `P != ()`) or an iterable that is no domain"
            );
        };
        if let Some(other) = hir_ty::mir::comptime_universe_clash(self.infer, expr_id) {
            // Not handled: an anonymous constant's loop merged into this table shares the
            // universe; telling them apart needs per-body universes from inference.
            not_supported!(
                "`for const` (expression {expr_id:?}) whose binder shares its universe with the \
                 `for const` at {other:?}, from an anonymous constant inferred apart from its \
                 parent"
            );
        }
        let binder_ty = checked.binder.as_ref();
        let Some(binder) = ComptimeBinder::of(binder_ty) else {
            implementation_error!(
                "the binder of `for const` (expression {expr_id:?}) is no placeholder: {binder_ty:?}"
            );
        };
        let domain = checked.domain.clone(); // clone: an interned type handle, into the terminator
        let span: MirSpan = expr_id.into();
        // The domain value is a ZST that names the fields; evaluated once, as an iterable is.
        let Some((_, current)) = self.lower_expr_to_some_operand(iterable, current)? else {
            return Ok(None);
        };

        let head = self.new_basic_block();
        let next = self.new_basic_block();
        let prev = self.current_loop_blocks.replace(LoopBlocks {
            begin: next,
            end: None,
            place: place.store(),
            drop_scope_index: self.drop_scopes.len(),
        });
        let end = self.current_loop_end()?;
        let prev_label = label.map(|label| {
            // clone: the loop's blocks and place, kept while its label is in scope
            let blocks = self.current_loop_blocks.as_ref().expect("set above").clone();
            (label, self.labeled_loop_blocks.insert(label, blocks))
        });
        self.set_goto(current, head, span);

        let first = self.new_basic_block();
        let done = self.new_basic_block();
        self.set_terminator(
            head,
            TerminatorKind::ComptimeFor { binder, domain, body: first, after: done },
            span,
        );
        self.push_assign_unit(done, place, span);
        self.set_goto(done, end, span);
        self.set_terminator(next, TerminatorKind::ComptimeNext { head }, span);

        let lowered = self.lower_comptime_iteration(first, next, pat, body, binder_ty);

        let my = mem::replace(&mut self.current_loop_blocks, prev).ok_or(
            // alloc: error path
            MirLowerError::ImplementationError("current_loop_blocks is corrupt".to_owned()),
        )?;
        if let Some((label, Some(prev))) = prev_label {
            self.labeled_loop_blocks.insert(label, prev);
        }
        lowered?;
        Ok(my.end)
    }

    /// One iteration from `first`: `pat` bound to the binder's value, `body`, then `next`.
    fn lower_comptime_iteration(
        &mut self,
        first: BasicBlockId,
        next: BasicBlockId,
        pat: PatId,
        body: ExprId,
        binder_ty: Ty<'db>,
    ) -> Result<'db, ()> {
        let scope = self.push_drop_scope();
        self.declare_bindings(pat);
        // `F` is a ZST (a field's or variant's representing type): its one value has no bytes.
        let value = self.temp(binder_ty, first, pat.into())?;
        self.push_assignment(
            first,
            value.into(),
            Operand::from_bytes(Box::default(), binder_ty).into(),
            pat.into(),
        );
        let (current, refuted) = self.pattern_match(first, None, value.into(), pat)?;
        if refuted.is_some() {
            // Inference checks the pattern as irrefutable.
            implementation_error!("the pattern of a `for const` ({pat:?}) is refutable");
        }
        match self.lower_expr_as_place(current, body, true)? {
            Some((_, current)) => {
                let current = scope.pop_and_drop(self, current, body.into());
                self.set_goto(current, next, body.into());
            }
            None => scope.pop_assume_dropped(self),
        }
        Ok(())
    }

    /// The generic arguments of the inline constant at `expr`: the item's own, then the
    /// binder `F` of each `for const` loop around it, outermost first
    /// ([`hir_ty::mir::comptime_binders_around`]). The expansion puts each copy's member in
    /// place of its binder, so the constant is evaluated, and keyed, per member; its body has
    /// them as the arguments past the item's (`monomorphized`). The item's alone outside a
    /// loop, as before.
    pub(super) fn inline_const_args(&self, expr: ExprId) -> GenericArgs<'db> {
        let interner = self.interner();
        let identity =
            GenericArgs::identity_for_item(interner, self.owner.generic_def(self.db).into());
        let binders = hir_ty::mir::comptime_binders_around(self.store, self.infer, expr);
        if binders.is_empty() {
            return identity;
        }
        GenericArgs::new_from_iter(
            identity.iter().chain(binders.iter().map(|it| it.ty(interner).into())),
        )
    }

    /// In the body of a `const { .. }` inside a `for const` loop, the loop's own binding `f`,
    /// which the block names from outside it: a value of the binder `F`, a zero-sized type (a
    /// field's or variant's representing type), so it is that type's one value, made where it
    /// is used. `None` for any other binding, which is the body's own.
    pub(super) fn comptime_binding_value(
        &mut self,
        binding: BindingId,
        current: BasicBlockId,
        expr: ExprId,
    ) -> Result<'db, Option<Place<'db>>> {
        if !matches!(self.owner, InferBodyId::AnonConstId(_))
            || self.local_of_binding(binding).is_some()
        {
            return Ok(None);
        }
        let ty = self.infer.binding_ty(binding);
        let Some(binder) = ComptimeBinder::of(ty) else { return Ok(None) };
        let of_its_loop = self.infer.comptime_loops().any(|(loop_, it)| {
            if it.universe().as_u32() != binder.0 {
                return false;
            }
            let Expr::ComptimeFor { pat, .. } = self.store.expr(loop_) else { return false };
            let mut binds = false;
            self.store.walk_bindings_in_pat(pat, |it| binds |= it == binding);
            binds
        });
        if !of_its_loop {
            return Ok(None);
        }
        let temp = self.temp(ty, current, expr.into())?;
        self.push_assignment(
            current,
            temp.into(),
            Operand::from_bytes(Box::default(), ty).into(),
            expr.into(),
        );
        Ok(Some(temp.into()))
    }

    /// `base.[field]` as a place: `base` (auto-deref'd as inference adjusted it) projected to
    /// the field the enclosing `for const` binder names in the iteration.
    pub(super) fn lower_reflected_field_as_place(
        &mut self,
        current: BasicBlockId,
        expr_id: ExprId,
        base: ExprId,
        field: ExprId,
    ) -> Result<'db, Option<(Place<'db>, BasicBlockId)>> {
        let field_ty = self.infer.expr_ty(field);
        let binder = ComptimeBinder::of(field_ty).filter(|binder| {
            self.infer.comptime_loops().any(|(_, it)| it.universe().as_u32() == binder.0)
        });
        let Some(binder) = binder else {
            // A generic `F: Field<B>` parameter, or any other type: monomorphization has no
            // loop to expand it in.
            not_supported!(
                "a reflected field `base.[f]` (expression {expr_id:?}) whose `f` is not the \
                 binder of an enclosing `for const`: its type is {field_ty:?}"
            );
        };
        let Some((base_place, current)) = self.lower_expr_as_place(current, base, true)? else {
            return Ok(None);
        };
        // The field token is a ZST; evaluated for its effects, after the base, as an index.
        let Some((_, current)) = self.lower_expr_to_some_operand(field, current)? else {
            return Ok(None);
        };
        Ok(Some((base_place.project(ProjectionElem::ReflectedField(binder)), current)))
    }
}
