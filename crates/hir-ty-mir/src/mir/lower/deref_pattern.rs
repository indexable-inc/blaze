//! MIR lowering for deref patterns: `deref!(p)` and the smart pointers inference peels off a
//! scrutinee by itself (`PatAdjust::OverloadedDeref`), as `rustc_mir_build` lowers them.
//!
//! A deref pattern on a `Box` is a builtin deref of the box's place (rustc's
//! `DerefPattern { borrow: ByRef::No }`), so a binding by value moves out of the box. On any
//! other `DerefPure` type it is a test of its own (`TestableCase::Deref`): the place is
//! borrowed into a temporary, `Deref::deref` (or `DerefMut::deref_mut` when the pattern
//! inside binds by `ref mut`) is called on it into a second temporary, and the inner pattern
//! matches what that points to. Each deref pattern gets its own temporaries; binding after
//! the pattern has matched reads through the ones the test wrote.

use hir_def::hir::PatId;
use rustc_type_ir::inherent::Ty as _;

use crate::{
    CallableDefId,
    mir::{
        BasicBlockId, BorrowKind, GenericArgs, MirSpan, MutBorrowKind, Operand, OperandKind, Place,
        ProjectionElem, Rvalue, Ty,
        lower::{MirLowerCtx, MirLowerError, Result},
    },
    next_solver::{
        IsRigid, Region, RegionExt, infer::traits::ObligationCause, obligation_ctxt::ObligationCtxt,
    },
};

/// Which deref of a pattern a temporary is for: the index among the pattern's implicit
/// adjustments, or one past them for the pattern's own `deref!`.
pub(super) type DerefKey = (PatId, u32);

impl<'db> MirLowerCtx<'_, '_, 'db> {
    /// The place a deref pattern at `key` matches its inner pattern against, given the place
    /// of the pointer, `place` of type `pointer`. `test` is set when the pattern is being
    /// tested, where the call is made; binding after reads through the temporary it wrote.
    pub(super) fn deref_pattern_place(
        &mut self,
        current: BasicBlockId,
        place: Place<'db>,
        pointer: Ty<'db>,
        key: DerefKey,
        test: bool,
    ) -> Result<'db, (Place<'db>, BasicBlockId)> {
        if pointer.is_box() {
            return Ok((place.project(ProjectionElem::Deref), current));
        }
        if !test && let Some(&temp) = self.deref_temps.get(&key) {
            return Ok((Place::from(temp).project(ProjectionElem::Deref), current));
        }
        let (pattern, _) = key;
        let span = MirSpan::from(pattern);
        let mutable = self.has_ref_mut_binding(pattern);
        let interner = self.interner();
        let lang_items = self.lang_items();
        let (mutability, method, borrow) = if mutable {
            (
                rustc_ast_ir::Mutability::Mut,
                lang_items.DerefMut_deref_mut,
                BorrowKind::Mut { kind: MutBorrowKind::Default },
            )
        } else {
            (rustc_ast_ir::Mutability::Not, lang_items.Deref_deref, BorrowKind::Shared)
        };
        let method = method.ok_or(MirLowerError::LangItemNotFound)?;
        let target = self.deref_target(pointer)?;
        let erased = Region::new_erased(interner);
        // `let temp: &Target;` is the place the pattern inside matches through.
        let temp = self.temp(Ty::new_ref(interner, erased, target, mutability), current, span)?;
        // `let ref_src = &place;` (or `&mut place`)
        let source =
            self.temp(Ty::new_ref(interner, erased, pointer, mutability), current, span)?;
        self.push_assignment(current, source.into(), Rvalue::Ref(borrow, place.store()), span);
        // `temp = <Pointer as Deref>::deref(move ref_src)` (or `DerefMut::deref_mut`)
        let func = Operand::const_zst(Ty::new_fn_def(
            interner,
            CallableDefId::FunctionId(method).into(),
            GenericArgs::new_from_slice(&[pointer.into()]),
        ));
        // alloc: the call's arguments, kept in the body
        let args = Box::new([Operand {
            kind: OperandKind::Move(Place::from(source).store()),
            span: None,
        }]);
        let current = self.lower_call(func, args, temp.into(), current, false, span)?.ok_or(
            MirLowerError::ImplementationError("a deref pattern's call does not return".into()),
        )?;
        self.deref_temps.insert(key, temp);
        Ok((Place::from(temp).project(ProjectionElem::Deref), current))
    }

    /// `<pointer as Deref>::Target`, normalized.
    fn deref_target(&self, pointer: Ty<'db>) -> Result<'db, Ty<'db>> {
        let target = self.lang_items().DerefTarget.ok_or(MirLowerError::LangItemNotFound)?;
        let target = Ty::new_projection(self.interner(), IsRigid::No, target.into(), [pointer]);
        let mut ocx = ObligationCtxt::new(&self.infcx);
        Ok(ocx
            .structurally_normalize_ty(&ObligationCause::dummy(), self.env, target)
            .unwrap_or(target))
    }

    /// Whether `pattern` binds anything by `ref mut`, which makes a deref pattern around it
    /// call `DerefMut::deref_mut` (rustc's `deref_pat_borrow_mode`).
    fn has_ref_mut_binding(&self, pattern: PatId) -> bool {
        hir_ty::utils::pat_has_ref_mut_binding(self.store, pattern, |pat| self.infer.binding_mode(pat))
    }
}
