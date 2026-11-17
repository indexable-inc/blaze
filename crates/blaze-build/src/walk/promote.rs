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
//! its [`InstanceKey`]. The walk can reference only an instance it names as an [`Instance`], so
//! the candidates are the fn items the operand itself names (`clone_arc_raw::<W>` at this
//! instance's arguments), each keyed by [`crate::Adts::promoted_fn_key`] exactly as the program
//! keys instances; a relocation to any other instance is refused by name.

use core::alloc::Allocator;

use blaze_lir::Value;
use blaze_lir::builder::DataReloc;
use blaze_lir::func::{RefId, RefKind};
use blaze_own::Vec;
use blaze_typeck::{ExprId, PNode};

use super::{V, Walk, malformed, unsupported};
use crate::data::{DataRecord, DataTarget};
use crate::{BuildError, Instance, InstanceKey};

/// The most expressions one promoted operand's fn-item search visits.
const MOST_OPERAND_STEPS: usize = 1 << 14;

impl<A: Allocator + Clone> Walk<'_, '_, '_, A> {
    /// Whether the checker listed borrow `e` as promoted (`Checked::promoted`, sorted).
    pub(super) fn is_promoted(&self, e: ExprId) -> bool {
        self.checked.promoted.binary_search_by_key(&e.0, |x| x.0).is_ok()
    }

    /// `&operand` promoted (the module doc): pushes the address of the value's constant row.
    pub(super) fn promoted_ref(&mut self, e: ExprId, operand: ExprId) -> Result<(), BuildError> {
        let adts = self.adts;
        let owner = self.body.owner;
        let record = adts.promoted_value(owner, e, self.args).ok_or(BuildError::NoPromotedValue { expr: e, owner })?;
        record.check().map_err(|_| malformed("a promoted borrow's record that breaks the data form"))?;
        let fns = self.promoted_fns(operand)?;
        let at = self.data_rows(e, record, &fns)?;
        self.push_val(V::Scalar(at), None);
        Ok(())
    }

    /// The fn instances operand `operand` names by path, each with its key: the only fn
    /// targets a promoted record may hold here. By a work list over the promotable forms.
    fn promoted_fns(&mut self, operand: ExprId) -> Result<Vec<(InstanceKey, Instance), A>, BuildError> {
        use blaze_typeck::Expr;
        let mut out: Vec<(InstanceKey, Instance), A> = Vec::new_in(self.alloc.clone());
        let mut work: Vec<ExprId, A> = Vec::new_in(self.alloc.clone());
        work.push(operand);
        let mut steps = 0usize;
        while let Some(x) = work.pop() {
            steps += 1;
            if steps > MOST_OPERAND_STEPS {
                return Err(unsupported(operand, "a promoted operand past the walk's bound on its expressions"));
            }
            match self.expr(x)? {
                Expr::Lit(_) => {}
                Expr::Paren(i) => work.push(i),
                Expr::Tuple { elems } | Expr::Array { elems } => {
                    let l = self.body.expr_lists.get(elems.range()).ok_or(malformed("a promoted literal's elements past `expr_lists`"))?;
                    work.extend_from_slice(l);
                }
                Expr::Struct { fields, base: None, .. } => {
                    let inits = self.body.field_inits.get(fields.range()).ok_or(malformed("a promoted struct literal's fields past `field_inits`"))?;
                    for f in inits {
                        work.push(f.expr);
                    }
                }
                Expr::Call { callee, args } => {
                    let l = self.body.expr_lists.get(args.range()).ok_or(malformed("a promoted call's arguments past `expr_lists`"))?;
                    work.extend_from_slice(l);
                    work.push(callee);
                }
                Expr::Path(_) => {
                    let t = self.expr_ty(x)?;
                    if let PNode::FnDef(item, args) = self.node_at(x, t)? {
                        let instance = Instance { item, args };
                        if out.iter().any(|&(_, i)| i == instance) {
                            continue;
                        }
                        let adts = self.adts;
                        let key = adts.promoted_fn_key(instance, self.ground).ok_or(BuildError::NoPromotedFnKey { expr: x, item })?;
                        out.push((key, instance));
                    }
                }
                _ => return Err(unsupported(x, "a promoted operand of a form the checker does not promote")),
            }
        }
        Ok(out)
    }

    /// Each allocation of `record` as a data row of the function, its relocations resolved;
    /// returns the root's address. A fn target is one of `fns` (a promoted operand's), each a
    /// `Func` reference of this function; a constant path passes none. Rows are made targets
    /// first, by an explicit stack; a cycle among the allocations is refused.
    pub(super) fn data_rows(&mut self, e: ExprId, record: &DataRecord<std::alloc::Global>, fns: &[(InstanceKey, Instance)]) -> Result<Value, BuildError> {
        const TODO: u8 = 0;
        const OPEN: u8 = 1;
        let n = record.allocs.len();
        let mut state: Vec<u8, A> = Vec::with_capacity_in(n, self.alloc.clone());
        state.resize(n, TODO);
        let mut rows: Vec<Option<RefId>, A> = Vec::with_capacity_in(n, self.alloc.clone());
        rows.resize(n, None);
        let mut stack: Vec<usize, A> = Vec::new_in(self.alloc.clone());
        stack.push(0usize);
        let mut relocs: Vec<DataReloc, A> = Vec::new_in(self.alloc.clone());
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
                    DataTarget::Fn(key) => match fns.iter().find(|&&(k, _)| k == key) {
                        Some(&(_, instance)) => {
                            let place = self.ref_place(instance);
                            self.b.reference(RefKind::Func, place)
                        }
                        None => return Err(unsupported(e, "a constant holding a fn pointer to an instance its own expression does not name")),
                    },
                    DataTarget::Vtable(_) => return Err(unsupported(e, "a const holding a `&dyn Trait` (its vtable is not a reference of this function)")),
                };
                relocs.push(DataReloc { offset: r.offset, target, addend });
            }
            if a.mutable {
                return Err(unsupported(e, "a const with a mutable allocation"));
            }
            let mut bytes: Vec<u8, A> = Vec::with_capacity_in(a.bytes.len(), self.alloc.clone());
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
