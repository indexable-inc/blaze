//! The value stack's one push/pop seam (docs: port27/tempo-run/UNWIND.md, 3.5, piece P5): beside
//! each waiting value, `val_tys` keeps the ground type of the value when the walk OWNS it and its
//! type needs drop, else `None`, so a call that may unwind while an operand waits can drop that
//! operand on its cleanup path.
//!
//! The rule, from the Reference, "Destructors" (temporaries): "Temporaries are also created to
//! hold the result of operands to an expression while the other operands are evaluated. The
//! temporaries are associated to the scope of the expression with that operand. Since the
//! temporaries are moved from once the expression is evaluated, dropping them has no effect
//! unless one of the operands to an expression breaks out of the expression, returns, or panics."
//! Its example: in `(PrintOnDrop("first operand"), PrintOnDrop("second operand"), loop { break; })`
//! "the first two operands are dropped in reverse order", so the waiting operands drop top of the
//! stack first.
//!
//! What is `Some(g)`: an operand the walk holds in a place of its own, by value, whose type `g`
//! needs drop: the caller's copy of an aggregate argument (`Task::ArgCopy`: every argument of a
//! call, method call, closure call or overloaded operator, and an assignment's right side read
//! late). A value moved into a call is popped with the call's arguments before the call is made,
//! so it is never both waiting and passed. What is `None`: a scalar (no type that needs drop is
//! held as one: `Repr` holds those as aggregates), a reference, an unsized place (only ever
//! borrowed), a place of a binding or a static (owned by its binding's scope, which `drop_from`
//! drops), a temporary held by its statement's scope (`hold_temp`: dropped by that scope).
//!
//! What is refused ([`UNGROUNDED`] as the type, [`BuildError::NoCleanup`] when a call that may
//! unwind meets it): a waiting place of which the walk cannot say what is owned: an aggregate
//! literal being built whose fields need drop (`S { a: String::new(), b: f() }`: `a` is stored,
//! `b` not yet, and dropping the whole literal would drop `b`'s uninitialized bytes; dropping
//! nothing would skip `a`'s destructor).

use core::alloc::Allocator;

use blaze_typeck::PTy;

use super::{Repr, UNGROUNDED, V, Walk};
use crate::BuildError;

/// The refusal for a waiting value the walk cannot ground or cannot say what of it is owned.
const NOT_GROUNDED: BuildError = BuildError::NoCleanup {
    what: "an operand evaluated before this call waits on the value stack and the walk could not ground what of it is owned (an aggregate literal being built whose fields need drop, or a type with no layout), so the unwind path cannot drop it exactly",
};

impl<A: Allocator + Clone> Walk<'_, '_, '_, A> {
    /// Pushes `v`; `ty` is `Some(g)` only for a value the walk owns whose ground type `g` needs
    /// drop (or [`UNGROUNDED`] for one it cannot say what of is owned), else `None`.
    pub(super) fn push_val(&mut self, v: V, ty: Option<PTy>) {
        self.vals.push(v);
        self.val_tys.push(ty);
        self.note_push();
        debug_assert_eq!(self.vals.len(), self.val_tys.len(), "the value stack and its types out of step");
    }

    /// Pops the top value and its type. An empty stack gives `V::None`: the callers that pop
    /// unchecked are the ones that discard what they pop (as they discarded `Vec::pop`'s
    /// `None`); a pop whose value is used goes through `Walk::pop`, which refuses an empty
    /// stack by name first.
    pub(super) fn pop_val(&mut self) -> V {
        self.val_tys.pop();
        let v = self.vals.pop().unwrap_or(V::None);
        self.note_stack(self.vals.len(), None);
        debug_assert_eq!(self.vals.len(), self.val_tys.len(), "the value stack and its types out of step");
        v
    }

    /// Drops the values from `len` on (a call's arguments, taken).
    pub(super) fn truncate_vals(&mut self, len: usize) {
        self.vals.truncate(len);
        self.val_tys.truncate(len);
        self.note_stack(self.vals.len(), None);
        debug_assert_eq!(self.vals.len(), self.val_tys.len(), "the value stack and its types out of step");
    }

    /// Removes the value at `at` (a callee pointer under its arguments), its type with it.
    pub(super) fn remove_val(&mut self, at: usize) -> V {
        self.val_tys.remove(at);
        let v = self.vals.remove(at);
        self.note_stack(self.vals.len(), Some(at));
        debug_assert_eq!(self.vals.len(), self.val_tys.len(), "the value stack and its types out of step");
        v
    }

    /// The type row of the caller's copy of an argument held as `repr`: its ground type when it
    /// needs drop (the copy is the walk's own until the call takes it), else `None`.
    pub(super) fn owned_arg(&self, repr: Repr) -> Option<PTy> {
        match repr {
            Repr::Agg(l) if self.layouts[l as usize].needs_drop => Some(self.layouts[l as usize].ty),
            _ => None,
        }
    }

    /// The type row of an aggregate literal's place while its parts are built: [`UNGROUNDED`]
    /// (refused if a call that may unwind meets it) when a part stored in it may need drop,
    /// else `None` (nothing in it to drop).
    pub(super) fn literal_place(parts_need_drop: bool) -> Option<PTy> {
        if parts_need_drop { Some(UNGROUNDED) } else { None }
    }

    /// Whether a value waiting on the stack must be dropped if a call made now unwinds.
    /// Refuses by name a waiting value whose ownership the walk could not ground, and an owned
    /// one that is not held in memory (drop glue takes its address).
    pub(super) fn waiting_owned(&self) -> Result<bool, BuildError> {
        let mut any = false;
        for i in 0..self.vals.len() {
            let Some(g) = self.val_tys[i] else { continue };
            if g == UNGROUNDED {
                return Err(NOT_GROUNDED);
            }
            let Some(l) = self.find_layout(g) else { return Err(NOT_GROUNDED) };
            if !self.layouts[l as usize].needs_drop {
                continue;
            }
            if !matches!(self.vals[i], V::Place(..)) {
                return Err(BuildError::NoCleanup {
                    what: "an owned operand whose type needs drop waits on the value stack not as a place, so its drop glue has no address to take",
                });
            }
            any = true;
        }
        Ok(any)
    }

    /// The cleanup's first part: every owned waiting value whose type needs drop, top of the
    /// stack first (the later operand first: the Reference's tuple example), each by its drop
    /// glue. [`Walk::waiting_owned`] has refused every other form before the `invoke` is made.
    pub(super) fn drop_waiting(&mut self) -> Result<(), BuildError> {
        for i in (0..self.vals.len()).rev() {
            let Some(g) = self.val_tys[i] else { continue };
            let Some(l) = self.find_layout(g) else { return Err(NOT_GROUNDED) };
            if !self.layouts[l as usize].needs_drop {
                continue;
            }
            let V::Place(p, off) = self.vals[i] else {
                return Err(BuildError::NoCleanup { what: "an owned operand whose type needs drop waits on the value stack not as a place" });
            };
            self.drop_at(g, (p, off))?;
        }
        Ok(())
    }
}
