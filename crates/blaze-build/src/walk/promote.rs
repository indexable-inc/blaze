//! A promoted borrow (ASYNC.md D-A10, P10): `&expr` that the checker listed in
//! `Checked::promoted` (blaze-typeck rules/promote.rs) is the address of a read-only constant
//! row, never of a frame temporary.
//!
//! The Reference, "Destructors", "Constant promotion": "Promotion of a value expression to a
//! `'static` slot occurs when the expression could be written in a constant and borrowed, and
//! that borrow could be dereferenced where the expression was originally written, without
//! changing the runtime behavior." So the borrow outlives the fn that made it: tokio's
//! `waker_vtable::<W>()` returns `&RawWakerVTable::new(clone_arc_raw::<W>, ..)` as
//! `&'static RawWakerVTable`, and its callers read the four words long after the frame is gone.
//!
//! The value is the constant evaluator's, as a `const { .. }` block's is: one record per
//! (instance, borrow), evaluated at the instance's ground arguments and answered by
//! [`crate::Adts::promoted_value`]; no answer is refused by name
//! ([`BuildError::NoPromotedValue`]), and the operand's code is never built in its place (no
//! fallback to a temporary: that is the wrong code this replaces). The record's allocations are
//! the function's data rows (the walk's `data_rows`, one home with a constant path's), so two
//! promotions of equal bytes in one function share nothing promised: Rust promises no address
//! equality between promotions.
//!
//! A fn pointer in the value (the vtable's four entries) is a relocation to a fn instance by
//! its key, read back to an `Instance` as a constant path's or a const block's is: the
//! evaluator's record names the instance of each of its fn words (blaze-consteval
//! `ConstRecord::fns`), and [`crate::Adts::const_fn_instance`] answers from it. A key no record
//! names is refused by name ([`BuildError::NoConstFnInstance`]). The instance is a `Func`
//! reference of the function, so the run reaches it by `Built::refs` as a fn coerced in a body:
//! a lazy front end with no row for it yet tables it there (run/make_one.rs `Step::Table`).
//! Before (tempo tests t0850, 2026-11-21): the fn items the operand names were keyed first by a
//! second answer (`Adts::promoted_fn_key`, the program's row of the fn), and a fn the body
//! names only as a value has no row on the lazy path before the build (eyre's
//! `&ErrorVTable { object_drop: object_drop::<E>, .. }`), so the build was refused.

use core::alloc::Allocator;

use blaze_lir::Value;
use blaze_lir::builder::DataReloc;
use blaze_lir::func::{RefId, RefKind};
use blaze_own::Vec;
use blaze_typeck::ExprId;

use super::{V, Walk, malformed, unsupported};
use crate::BuildError;
use crate::data::{DataRecord, DataTarget};

impl<A: Allocator + Copy> Walk<'_, '_, '_, A> {
    /// Whether the checker listed borrow `e` as promoted (`Checked::promoted`, sorted).
    pub(super) fn is_promoted(&self, e: ExprId) -> bool {
        self.checked.promoted.binary_search_by_key(&e.0, |x| x.0).is_ok()
    }

    /// `&operand` promoted (the module doc): pushes the address of the value's constant row.
    pub(super) fn promoted_ref(&mut self, e: ExprId) -> Result<(), BuildError> {
        let adts = self.adts;
        let owner = self.body.owner;
        let record = adts.promoted_value(owner, e, self.args).ok_or(BuildError::NoPromotedValue { expr: e, owner })?;
        record.check().map_err(|_| malformed("a promoted borrow's record that breaks the data form"))?;
        let at = self.data_rows(e, record)?;
        self.push_val(V::Scalar(at), None);
        Ok(())
    }

    /// Each allocation of `record` as a data row of the function, its relocations resolved;
    /// returns the root's address: the one home of a promoted borrow's rows and of a constant
    /// path's or a const block's (walk.rs `const_record_value`). A fn target is a `Func`
    /// reference of this function to the instance [`crate::Adts::const_fn_instance`] says the
    /// evaluator keyed (a fn the operand or the constant's own body names, or one a `const fn`
    /// it called made), else refused with [`BuildError::NoConstFnInstance`]. Rows are made
    /// targets first, by an explicit stack; a cycle among the allocations is refused.
    pub(super) fn data_rows(&mut self, e: ExprId, record: &DataRecord<std::alloc::Global>) -> Result<Value, BuildError> {
        const TODO: u8 = 0;
        const OPEN: u8 = 1;
        let n = record.allocs.len();
        let mut state: Vec<u8, A> = Vec::with_capacity_in(n, self.alloc);
        state.resize(n, TODO);
        let mut rows: Vec<Option<RefId>, A> = Vec::with_capacity_in(n, self.alloc);
        rows.resize(n, None);
        let mut stack: Vec<usize, A> = Vec::new_in(self.alloc);
        stack.push(0usize);
        let mut relocs: Vec<DataReloc, A> = Vec::new_in(self.alloc);
        while let Some(&k) = stack.last() {
            if rows[k].is_some() {
                stack.pop();
                continue;
            }
            let a = &record.allocs[k];
            if state[k] == TODO {
                state[k] = OPEN;
                for r in a.relocs.iter() {
                    if let DataTarget::Local(t) = r.target {
                        let t = t as usize;
                        match (rows.get(t), state.get(t)) {
                            (Some(Some(_)), _) => {}
                            (Some(None), Some(&OPEN)) => {
                                return Err(unsupported(e, "a const whose allocations point at each other in a cycle"));
                            }
                            (Some(None), _) => stack.push(t),
                            (None, _) => return Err(malformed("a const's relocation to an allocation past its record")),
                        }
                    }
                }
                continue;
            }
            // Every local target is made: this allocation's row.
            relocs.clear();
            for r in a.relocs.iter() {
                let off = r.offset as usize;
                let word = a.bytes.get(off..off + 8).ok_or(malformed("a const's relocation past its bytes"))?;
                let addend = i64::from_le_bytes(word.try_into().map_err(|_| malformed("a relocation word not of 8 bytes"))?);
                let target = match r.target {
                    DataTarget::Local(t) => rows[t as usize].ok_or(malformed("a const's row made before its target"))?,
                    DataTarget::Static(s) => {
                        let place = self.static_place(s, false)?;
                        self.b.reference(RefKind::Static, place)
                    }
                    // It becomes a `Func` reference of this function exactly as a fn item
                    // coerced to a fn pointer here does, so the run reaches it by `Built::refs`.
                    DataTarget::Fn(key) => {
                        let adts = self.adts;
                        let instance = adts.const_fn_instance(key, self.ground).ok_or(BuildError::NoConstFnInstance { expr: e, key })?;
                        let target = self.reify_target(instance)?;
                        let place = self.ref_place(target);
                        self.b.reference(RefKind::Func, place)
                    }
                    DataTarget::Vtable(_) => return Err(unsupported(e, "a const holding a `&dyn Trait` (its vtable is not a reference of this function)")),
                };
                relocs.push(DataReloc { offset: r.offset, target, addend });
            }
            if a.mutable {
                return Err(unsupported(e, "a const with a mutable allocation"));
            }
            let mut bytes: Vec<u8, A> = Vec::with_capacity_in(a.bytes.len(), self.alloc);
            bytes.extend_from_slice(&a.bytes);
            // The word's addend is in the relocation: the row's bytes hold zero there.
            for r in relocs.iter() {
                bytes[r.offset as usize..r.offset as usize + 8].fill(0);
            }
            rows[k] = Some(self.b.data(&bytes, u32::from(a.align), &relocs));
            stack.pop();
        }
        let root = rows[0].ok_or(malformed("a const's root row not made"))?;
        Ok(self.b.data_addr(root))
    }
}
