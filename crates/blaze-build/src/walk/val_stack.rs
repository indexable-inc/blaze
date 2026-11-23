//! The value stack's one push/pop seam (docs: port27/tempo-run/UNWIND.md, 3.5, piece P5): beside
//! each waiting value, `val_tys` keeps what the walk OWNS of it that needs drop ([`Owns`]), so a
//! call that may unwind while an operand waits can drop that operand on its cleanup path.
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
//! What is [`Owns::Whole`]`(g)`: an operand the walk holds in a place of its own, by value, whose type `g`
//! needs drop: the caller's copy of an aggregate argument (`Task::ArgCopy`: every argument of a
//! call, method call, closure call or overloaded operator, and an assignment's right side read
//! late). A value moved into a call is popped with the call's arguments before the call is made,
//! so it is never both waiting and passed. What is [`Owns::Nothing`]: a scalar (no type that needs drop is
//! held as one: `Repr` holds those as aggregates), a reference, an unsized place (only ever
//! borrowed), a place of a binding or a static (owned by its binding's scope, which `drop_from`
//! drops), a temporary held by its statement's scope (`hold_temp`: dropped by that scope).
//!
//! An aggregate literal being built ([`Owns::Parts`]): its place waits under each part's
//! expression, and is owned part by part. `S { a: String::new(), b: f() }`: while `f()` runs,
//! `a` is stored and `b` not yet; dropping the whole literal would drop `b`'s uninitialized
//! bytes, dropping nothing would skip `a`'s destructor. So each part stored that needs drop is
//! recorded (`Task::InitAt`: [`Walk::stored_part`]) and a cleanup drops exactly those, the last
//! stored first: the Reference, "Destructors" (temporaries), whose rule is MIR's too: an
//! aggregate's operands are temporaries until the aggregate is made, dropped in reverse order of
//! creation when a later operand panics. Once every part is stored (`Task::LitBuilt`:
//! [`Walk::literal_built`]) the literal is one value, owned whole ([`Owns::Whole`] of its type
//! when that needs drop: its own `Drop` impl runs from then on). Literals nest (a part may be a
//! literal itself), and the inner one is built while the outer waits, so the recorded parts are
//! one LIFO list, `lit_parts`, each literal's from its `from` up to the next literal's.
//!
//! What is refused ([`BuildError::NoCleanup`] when a call that may unwind meets it): an owned
//! waiting value whose type has no layout row, and one not held in memory (drop glue takes its
//! address).

use core::alloc::Allocator;

use blaze_typeck::PTy;

use super::{Repr, V, Walk, malformed};
use crate::BuildError;

/// What the walk owns of a value waiting on the stack, beside it in `Walk::val_tys`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Owns {
    /// Nothing that needs drop (see the module's list).
    Nothing,
    /// The whole value, of this ground type, which needs drop.
    Whole(PTy),
    /// An aggregate literal being built: its parts stored so far that need drop are
    /// `Walk::lit_parts[from..]`, up to the next literal's `from` above it.
    Parts { from: u32 },
}

/// The refusal for an owned waiting value whose type the walk has no layout row of.
const NOT_GROUNDED: BuildError = BuildError::NoCleanup {
    what: "an owned operand evaluated before this call waits on the value stack and its type has no layout row, so the unwind path cannot drop it exactly",
};

impl<A: Allocator + Copy> Walk<'_, '_, '_, A> {
    /// Pushes `v`; `ty` is `Some(g)` only for a value the walk owns whose ground type `g` needs
    /// drop, else `None`.
    pub(super) fn push_val(&mut self, v: V, ty: Option<PTy>) {
        self.push_owned(v, ty.map_or(Owns::Nothing, Owns::Whole));
    }

    /// Pushes the place of an aggregate literal about to be built: owned part by part
    /// ([`Owns::Parts`]), none stored yet.
    pub(super) fn push_literal(&mut self, at: (blaze_lir::Value, i32)) {
        let from = self.lit_parts.len() as u32;
        self.push_owned(V::Place(at.0, at.1), Owns::Parts { from });
    }

    fn push_owned(&mut self, v: V, owns: Owns) {
        self.vals.push(v);
        self.val_tys.push(owns);
        self.note_push();
        debug_assert_eq!(self.vals.len(), self.val_tys.len(), "the value stack and its types out of step");
    }

    /// Pops the top value and what is owned of it. An empty stack gives `V::None`: the callers
    /// that pop unchecked are the ones that discard what they pop (as they discarded
    /// `Vec::pop`'s `None`); a pop whose value is used goes through `Walk::pop`, which refuses
    /// an empty stack by name first.
    pub(super) fn pop_val(&mut self) -> V {
        let len = self.vals.len().saturating_sub(1);
        self.forget_parts(len);
        self.val_tys.pop();
        let v = self.vals.pop().unwrap_or(V::None);
        self.note_stack(self.vals.len(), None);
        debug_assert_eq!(self.vals.len(), self.val_tys.len(), "the value stack and its types out of step");
        v
    }

    /// Drops the values from `len` on (a call's arguments, taken).
    pub(super) fn truncate_vals(&mut self, len: usize) {
        self.forget_parts(len);
        self.vals.truncate(len);
        self.val_tys.truncate(len);
        self.note_stack(self.vals.len(), None);
        debug_assert_eq!(self.vals.len(), self.val_tys.len(), "the value stack and its types out of step");
    }

    /// Removes the value at `at` (a callee pointer under its arguments), what is owned of it
    /// with it: a literal's recorded parts leave `lit_parts`, and the literals above it start
    /// that many rows lower.
    pub(super) fn remove_val(&mut self, at: usize) -> V {
        if let Some(&Owns::Parts { from }) = self.val_tys.get(at) {
            let end = self.parts_end(at);
            let len = self.lit_parts.len();
            self.lit_parts.copy_within(end..len, from as usize);
            let gone = (end - from as usize) as u32;
            self.lit_parts.truncate(len - gone as usize);
            for o in self.val_tys[at + 1..].iter_mut() {
                if let Owns::Parts { from } = o {
                    *from -= gone;
                }
            }
        }
        self.val_tys.remove(at);
        let v = self.vals.remove(at);
        self.note_stack(self.vals.len(), Some(at));
        debug_assert_eq!(self.vals.len(), self.val_tys.len(), "the value stack and its types out of step");
        v
    }

    /// The recorded parts of the values from `len` on leave with them: `lit_parts` is cut back
    /// to the lowest of their literals' `from`.
    fn forget_parts(&mut self, len: usize) {
        let lowest = self.val_tys.get(len..).and_then(|gone| {
            gone.iter().find_map(|o| match *o {
                Owns::Parts { from } => Some(from as usize),
                _ => None,
            })
        });
        if let Some(from) = lowest {
            self.lit_parts.truncate(from);
        }
    }

    /// Where the parts of the literal at stack row `i` end in `lit_parts`: the next literal's
    /// `from` above it, else the list's end.
    fn parts_end(&self, i: usize) -> usize {
        self.val_tys[i + 1..]
            .iter()
            .find_map(|o| match *o {
                Owns::Parts { from } => Some(from as usize),
                _ => None,
            })
            .unwrap_or(self.lit_parts.len())
    }

    /// `Task::InitAt` stored a part held as `repr` at `off` into the literal on top: owned by
    /// the literal from now on when its type needs drop.
    pub(super) fn stored_part(&mut self, off: u32, repr: Repr) -> Result<(), BuildError> {
        let Repr::Agg(l) = repr else { return Ok(()) };
        if !self.layouts[l as usize].needs_drop {
            return Ok(());
        }
        let Some(Owns::Parts { .. }) = self.val_tys.last() else {
            return Err(malformed("a part stored into a place that is not an aggregate literal being built"));
        };
        let g = self.layouts[l as usize].ty;
        self.lit_parts.push((off, g));
        Ok(())
    }

    /// `Task::LitBuilt`: every part of the literal on top (laid out as row `l`) is stored, so
    /// it is one value, owned whole when its type needs drop; its recorded parts go.
    pub(super) fn literal_built(&mut self, l: u32) -> Result<(), BuildError> {
        let Some(&Owns::Parts { from }) = self.val_tys.last() else {
            return Err(malformed("an aggregate literal finished whose place is not on top of the value stack"));
        };
        self.lit_parts.truncate(from as usize);
        let layout = &self.layouts[l as usize];
        let whole = if layout.needs_drop { Owns::Whole(layout.ty) } else { Owns::Nothing };
        if let Some(top) = self.val_tys.last_mut() {
            *top = whole;
        }
        Ok(())
    }

    /// The type row of the caller's copy of an argument held as `repr`: its ground type when it
    /// needs drop (the copy is the walk's own until the call takes it), else `None`.
    pub(super) fn owned_arg(&self, repr: Repr) -> Option<PTy> {
        match repr {
            Repr::Agg(l) if self.layouts[l as usize].needs_drop => Some(self.layouts[l as usize].ty),
            _ => None,
        }
    }

    /// Whether a value waiting on the stack must be dropped if a call made now unwinds: an
    /// owned whole value whose type needs drop, or a literal with a part stored that does.
    /// Refuses by name an owned value whose type has no layout row, and one that is not held in
    /// memory (drop glue takes its address).
    pub(super) fn waiting_owned(&self) -> Result<bool, BuildError> {
        let mut any = false;
        let mut end = self.lit_parts.len();
        for i in (0..self.vals.len()).rev() {
            let owes = match self.val_tys[i] {
                Owns::Nothing => false,
                Owns::Whole(g) => {
                    let Some(l) = self.find_layout(g) else { return Err(NOT_GROUNDED) };
                    self.layouts[l as usize].needs_drop
                }
                Owns::Parts { from } => {
                    let some = (from as usize) < end;
                    end = from as usize;
                    some
                }
            };
            if !owes {
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
    /// glue; of a literal being built, each part stored that needs drop, the last stored
    /// first. [`Walk::waiting_owned`] has refused every other form before the `invoke` is made.
    pub(super) fn drop_waiting(&mut self) -> Result<(), BuildError> {
        let mut end = self.lit_parts.len();
        for i in (0..self.vals.len()).rev() {
            match self.val_tys[i] {
                Owns::Nothing => {}
                Owns::Whole(g) => {
                    let Some(l) = self.find_layout(g) else { return Err(NOT_GROUNDED) };
                    if !self.layouts[l as usize].needs_drop {
                        continue;
                    }
                    let V::Place(p, off) = self.vals[i] else {
                        return Err(BuildError::NoCleanup { what: "an owned operand whose type needs drop waits on the value stack not as a place" });
                    };
                    self.drop_at(g, (p, off))?;
                }
                Owns::Parts { from } => {
                    let parts = from as usize..end;
                    end = from as usize;
                    if parts.is_empty() {
                        continue;
                    }
                    let V::Place(p, base) = self.vals[i] else {
                        return Err(BuildError::NoCleanup { what: "an aggregate literal being built waits on the value stack not as a place" });
                    };
                    for k in parts.rev() {
                        let (off, g) = self.lit_parts[k];
                        self.drop_at(g, (p, base + off as i32))?;
                    }
                }
            }
        }
        Ok(())
    }
}
