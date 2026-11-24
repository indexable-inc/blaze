//! Struct update expressions, `S { a: x, ..base }`. The Reference, "Struct expressions",
//! "Functional update syntax": "A struct expression that constructs a value of a struct type
//! can terminate with the syntax `..` followed by an expression to denote a functional update.
//! The expression following `..` (the base) must have the same struct type as the new struct
//! type being formed. The entire expression uses the given values for the fields that were
//! specified and moves or copies the remaining fields from the base expression."
//! "Place Expressions and Value Expressions" names "The base of a functional update struct
//! expression" among the place expression contexts, so the base is evaluated as a place and
//! only its untaken parts stay where they are.
//!
//! Built in the order Rust gives: the written fields in written order, each stored into the
//! new value's temporary; then the base, as a place; then each field the literal does not
//! name, copied out of the base's place (`Task::StructBase`). A taken field whose type needs
//! drop moves out of the base as `let x = base.f;` does (walk/moves.rs):
//! - a base that is a local's field path (`..p`, `..self.inner`): the part's bit in the
//!   local's word is cleared, so its scope's end drops only what is left (`drop_moved`; the
//!   local has field flags from `scan_update_moves`);
//! - a temporary base (`..make()`, `..S { .. }`): its `hold_temp` entry becomes a
//!   `Held::Moved` with a word of its own, as `move_temp_field` makes it, so its statement's
//!   end drops what is left.
//! Refused by name: a taken field that needs drop out of a base whose type implements `Drop`
//! (E0509) or behind a pointer or in a static (E0507), out of a temporary not held to its
//! statement's end, of a zero-sized part, and more than 64 parts of one value.

use core::alloc::Allocator;

use blaze_lir::builder::MemAttrs;
use blaze_lir::func::SlotKind;
use blaze_lir::{Flags, Ty, Value};
use blaze_typeck::{Expr, ExprId, FieldInit};

use super::super::{Held, Repr, Shape, Task, V, Walk, malformed, unsupported};
use super::Step;
use crate::BuildError;

impl<'x, A: Allocator + Copy> Walk<'x, '_, '_, A> {
    /// The written fields of struct update `e` (the body's, not borrowed from the walk), and
    /// its base.
    fn update_parts(&self, e: ExprId) -> Result<(&'x [FieldInit], ExprId), BuildError> {
        let body: &'x blaze_typeck::BodyView<'x> = self.body;
        let (fields, base) = match self.expr(e)? {
            Expr::Struct { fields, base: blaze_typeck::StructBase::Expr(base), .. }
            | Expr::Record { fields, base: blaze_typeck::StructBase::Expr(base) } => (fields, base),
            _ => return Err(malformed("a struct update that is not a struct expression with a base")),
        };
        let inits = body.field_inits.get(fields.range()).ok_or(malformed("a struct update's fields past `field_inits`"))?;
        Ok((inits, base))
    }

    /// Pushes the tasks of struct update `e`: its written fields stored into a temporary of
    /// its own (as a struct literal's), then its base as a place, then `Task::StructBase`.
    pub(in crate::walk) fn struct_update(&mut self, e: ExprId) -> Result<(), BuildError> {
        let (inits, base) = self.update_parts(e)?;
        let repr = self.expr_repr(e)?;
        match repr {
            // A zero-sized struct: its fields are evaluated in order, then the base, for their
            // effects. A base that needs drop would leave parts to move out of nothing.
            Repr::None => {
                let t = self.adjusted_ty(base)?;
                let g = self.ground_ty(t).map_err(|error| Self::ty_err(base, error))?;
                if self.needs_drop(g).map_err(|error| Self::ty_err(base, error))? {
                    return Err(unsupported(e, "a struct update of a zero-sized struct whose base needs drop"));
                }
                self.work.push(Task::PushNone);
                self.work.push(Task::Discard);
                self.work.push(Task::Eval(base));
                for init in inits.iter().rev() {
                    self.work.push(Task::Discard);
                    self.work.push(Task::Eval(init.expr));
                }
            }
            Repr::Agg(l) => {
                if self.layouts[l as usize].is_union {
                    return Err(malformed("a struct update of a union (only a struct takes a base)"));
                }
                let variant = if matches!(self.expr(e)?, Expr::Record { .. }) {
                    0
                } else {
                    Self::def_variant(self.expr_def(e)).ok_or(malformed("a struct update the check did not resolve to a struct"))?
                };
                let (first, len, tag) = self.variant_rows(l, variant)?;
                let at = self.temp(repr);
                self.write_tag(tag, variant, at);
                self.work.push(Task::StructBase(e));
                self.work.push(Task::Place(base));
                for init in inits.iter().rev() {
                    let row = (first..first + len)
                        .find(|&i| self.layout_fields[i as usize].0 == init.name)
                        .ok_or(malformed("a struct update naming a field its struct lacks"))?;
                    let (_, off, frepr) = self.layout_fields[row as usize];
                    self.work.push(Task::InitAt { off, repr: frepr });
                    self.work.push(Task::Eval(init.expr));
                }
                // Partly built while its written fields and its base run, owned field by field
                // (walk/val_stack.rs); one value once `StructBase` has taken the rest.
                self.push_literal(at);
            }
            _ => return Err(unsupported(e, "a struct update of other than a struct")),
        }
        Ok(())
    }

    /// `Task::StructBase(e)`: the base's place on top, the new value's place under it. Each
    /// field the update does not name is copied from the base into the new value, in the
    /// struct's declaration order; one that needs drop moves out of the base (`take_field`).
    pub(in crate::walk) fn struct_update_base(&mut self, e: ExprId) -> Result<(), BuildError> {
        let (inits, base) = self.update_parts(e)?;
        let v = self.pop()?;
        let src = self.place_any(v)?;
        let Some(&V::Place(p, at)) = self.vals.last() else {
            return Err(malformed("a struct update without its place"));
        };
        let Repr::Agg(l) = self.expr_repr(e)? else {
            return Err(malformed("a struct update built as other than an aggregate"));
        };
        let variant = if matches!(self.expr(e)?, Expr::Record { .. }) {
            0
        } else {
            Self::def_variant(self.expr_def(e)).ok_or(malformed("a struct update the check did not resolve to a struct"))?
        };
        let (first, len, _) = self.variant_rows(l, variant)?;
        let Repr::Agg(bl) = self.adjusted_repr(base)? else {
            return Err(malformed("a struct update's base held as other than an aggregate"));
        };
        if self.layouts[bl as usize].is_union {
            return Err(malformed("a struct update whose base is a union"));
        }
        let Shape::Fields { first: bfirst, len: blen } = self.layouts[bl as usize].shape else {
            return Err(malformed("a struct update whose base is not a struct"));
        };
        for row in bfirst..bfirst + blen {
            let (name, soff, srepr) = self.layout_fields[row as usize];
            if inits.iter().any(|f| f.name == name) {
                continue;
            }
            let to = (first..first + len)
                .find(|&i| self.layout_fields[i as usize].0 == name)
                .ok_or(malformed("a struct update's base with a field its result lacks"))?;
            let (_, doff, drepr) = self.layout_fields[to as usize];
            if srepr != drepr {
                return Err(malformed("a struct update taking a field held differently in its base and its result"));
            }
            let g = self.layout_field_tys[row as usize];
            if self.needs_drop(g).map_err(|error| Self::ty_err(base, error))? {
                if srepr == Repr::None {
                    return Err(unsupported(e, "a struct update taking a zero-sized field that needs drop"));
                }
                self.take_field(e, base, bl, row, src)?;
            }
            let v = self.load(srepr, (src.0, src.1 + soff as i32));
            self.store(v, drepr, (p, at + doff as i32))?;
        }
        // Whole now: owned as one value (walk/val_stack.rs).
        self.literal_built(l)
    }

    /// Field `row` of base `base` (of layout `bl`, its place `src`), which needs drop, moves
    /// out into struct update `e`: its bit in the owner's word is cleared.
    fn take_field(&mut self, e: ExprId, base: ExprId, bl: u32, row: u32, src: (Value, i32)) -> Result<(), BuildError> {
        if self.layouts[bl as usize].drop_fn.is_some() {
            return Err(unsupported(e, "a struct update moving a field out of a base whose type implements `Drop` (E0509)"));
        }
        if !self.adjust_rows(base).is_empty() {
            return Err(unsupported(e, "a struct update moving a field that needs drop out of a base with an adjustment"));
        }
        if let Some((_, mask, start, len, drops)) = self.place_path(base, 0)? {
            if drops {
                self.moves.steps.truncate(start as usize);
                return Err(unsupported(e, "a struct update moving a field out of a type that implements `Drop` (E0509)"));
            }
            self.moves.steps.push(Step::Field(row));
            let Some(bit) = self.part_bit(mask, start, len + 1, 0, Some(e))? else {
                return Err(unsupported(e, "a struct update moving out more than 64 parts of one local (its word)"));
            };
            self.clear_bit(mask, bit);
            return Ok(());
        }
        if self.is_place_expr(base)? {
            return Err(unsupported(
                e,
                "a struct update moving a field that needs drop out of a place that is not a local's field path (behind a reference or a pointer, or a static: E0507)",
            ));
        }
        let found = self
            .scope_pats
            .iter()
            .rposition(|h| matches!(*h, Held::Rest { at: a, variant: None, .. } | Held::Moved { at: a, .. } if a == src));
        let Some(i) = found else {
            if !self.live {
                // Unreachable code: nothing runs, nothing is held (`Task::Spill` held nothing).
                return Ok(());
            }
            let t = self.adjusted_ty(base)?;
            let g = self.ground_ty(t).map_err(|error| Self::ty_err(base, error))?;
            if !self.needs_drop(g).map_err(|error| Self::ty_err(base, error))? {
                // A temporary whose own type needs no drop (e.g. `ManuallyDrop<T>`) was never held.
                return Ok(());
            }
            return Err(unsupported(e, "a struct update moving a field out of a temporary base not held to its statement's end (an extended temporary)"));
        };
        let mask = match self.scope_pats[i] {
            // A second field of the same base: its word is made.
            Held::Moved { mask, .. } => mask,
            _ => {
                let t = self.adjusted_ty(base)?;
                let g = self.ground_ty(t).map_err(|error| Self::ty_err(base, error))?;
                let slot = self.b.slot(8, 3, SlotKind::Named);
                let fa = self.b.slot_addr(slot);
                let all = self.b.iconst(Ty::I64, u128::from(u64::MAX));
                self.b.store(Flags::NONE, all, fa, 0, MemAttrs::aligned(3));
                self.scope_pats[i] = Held::Moved { at: src, g, mask: slot };
                slot
            }
        };
        let start = self.moves.steps.len() as u32;
        self.moves.steps.push(Step::Field(row));
        let Some(bit) = self.part_bit(mask, start, 1, 0, Some(e))? else {
            return Err(unsupported(e, "a struct update moving out more than 64 parts of one temporary (its word)"));
        };
        self.clear_bit(mask, bit);
        Ok(())
    }

    /// `scan_flags`: a local that is (or whose field path is) the base of a struct update
    /// taking a field that needs drop gets field flags (`partial`): the word `take_field`
    /// clears and its drop reads.
    pub(in crate::walk) fn scan_update_moves(&mut self) -> Result<(), BuildError> {
        for i in 0..self.body.exprs.len() {
            let e = ExprId(i as u32);
            let base = match self.expr(e)? {
                Expr::Struct { base: blaze_typeck::StructBase::Expr(base), .. }
                | Expr::Record { base: blaze_typeck::StructBase::Expr(base), .. } => base,
                _ => continue,
            };
            if !self.adjust_rows(base).is_empty() {
                continue;
            }
            let (inits, _) = self.update_parts(e)?;
            let Repr::Agg(bl) = self.adjusted_repr(base)? else { continue };
            if self.layouts[bl as usize].is_union {
                continue;
            }
            let Shape::Fields { first, len } = self.layouts[bl as usize].shape else { continue };
            let mut takes = false;
            for row in first..first + len {
                let name = self.layout_fields[row as usize].0;
                if inits.iter().any(|f| f.name == name) {
                    continue;
                }
                let g = self.layout_field_tys[row as usize];
                if self.needs_drop(g).map_err(|error| Self::ty_err(base, error))? {
                    takes = true;
                    break;
                }
            }
            if !takes {
                continue;
            }
            let mark = self.moves.steps.len();
            let root = self.path_steps(base, 0)?;
            self.moves.steps.truncate(mark);
            if let Some((x, _)) = root {
                *self.partial.get_mut(x.ix()).ok_or(malformed("a local past `pats`"))? = true;
            }
        }
        Ok(())
    }
}
