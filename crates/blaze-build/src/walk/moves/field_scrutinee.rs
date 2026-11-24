//! A `match`, `if let` or `let`-`else` whose scrutinee is a FIELD OF AN OWNED LOCAL
//! (`match self.failed { Some(error) => Err(error), None => Ok(()) }` in bstd's `Held::end`,
//! `self: Held` taken by value): tested IN PLACE, owned by the local, a by-value binding moving
//! its part out by the local's field flags.
//!
//! The Reference, "Match expressions": "If the scrutinee expression is a place expression, the
//! match does not allocate a temporary location; however, a by-value binding may copy or move
//! from the memory location"; [expr.move.movable-place]: "Only the following place expressions
//! may be moved out of: Variables which are not currently borrowed. Temporary values. Fields of
//! a place expression which can be moved out of and don't implement `Drop`"; "Destructors":
//! "If a variable has been partially initialized, only its initialized fields are dropped".
//!
//! Before this, such a scrutinee was EVALUATED as a value (walk.rs `Expr::Match` pushed
//! `Task::Eval`), which moved the whole field (walk/moves.rs `move_field`); the local had no
//! field flags (`scan_field_moves` counts a scrutinee as used in place), so `move_temp_field`
//! refused it as "a move out of a field of other than a temporary or a local with field flags
//! (behind a pointer ..)": a misleading reason, as nothing there is behind a pointer.
//!
//! A `Box`'s contents on the path are the same (`match *s.bx`, `if let Type::Path(mut ty) =
//! *group.elem` in syn's `ambig_ty`): [expr.move.movable-place] lets them be moved out of, so
//! the part `[Field elem, Deref, Variant Path, ..]` moves by the local's word and the local's
//! drops free the box alone where its contents moved (walk/moves/boxes.rs `box_node`). Before
//! this, walk/moves.rs `scrutinee_in_place` took such a scrutinee as behind a pointer, owned by
//! no one, and `move_out` refused the binding ("pattern 9 ... a move out").
//!
//! The pieces, each called from the place named:
//! - `field_chain_local`: the local a scrutinee is a place path of (fields, box contents);
//! - `owned_field_scrutinee`: that local when it is owned here and the scrutinee needs drop
//!   (walk.rs `Expr::Match` and `match_dispatch`, walk/lets.rs `push_scrutinee`,
//!   walk/matches.rs `tested_place`, walk/moves.rs `own_scrutinee`);
//! - `scan_field_scrutinee`: field flags for the local when an arm may move a part out
//!   (walk.rs `scan_flags`, for a `match` and a `let`-`else`);
//! - `field_scrutinee_prefix`: the steps from the local to the scrutinee, the start of every
//!   moved part's path (walk/moves.rs `move_out`).

use core::alloc::Allocator;

use blaze_lir::func::SlotId;
use blaze_typeck::{ExprId, Pat, PatId};

use super::super::{Walk, malformed};
use crate::BuildError;

impl<A: Allocator + Copy> Walk<'_, '_, '_, A> {
    /// The local `e` is a part of: `e` a place path of one or more steps from a local, each a
    /// struct or tuple field or a `Box`'s contents (an overloaded `*b` of a box, a base's
    /// autoderef through boxes), through parentheses, with no adjustment on `e` itself: the
    /// paths walk/moves/boxes.rs `path_steps` follows, so the local found here is the one
    /// `field_scrutinee_prefix` reaches (one definition of a place a local owns). The
    /// Reference, [expr.move.movable-place]: "Fields of a place expression which can be moved
    /// out of and don't implement `Drop`. The result of dereferencing an expression with type
    /// `Box<T>` and that can also be moved out of": `*group.elem` with `group: TypeGroup` by
    /// value (syn's `ambig_ty`) is such a place. Any other dereference on the way (a
    /// reference's, a pointer's, an overloaded `Deref` other than a box's) ends the path: the
    /// place is behind a pointer (walk/moves.rs `scrutinee_in_place`). `None` for anything
    /// else, a whole local included (walk.rs handles that as its own owner). Reads the
    /// expressions, their adjustment rows and the layouts of the bases, never a local's
    /// ownership, so `scan_flags` may ask before any local is owned.
    pub(in crate::walk) fn field_chain_local(&mut self, e: ExprId) -> Result<Option<PatId>, BuildError> {
        if !self.adjust_rows(e).is_empty() {
            return Ok(None);
        }
        let mark = self.moves.steps.len();
        let root = self.path_steps(e, 0)?;
        let steps = self.moves.steps.len() - mark;
        self.moves.steps.truncate(mark);
        Ok(match root {
            Some((x, _)) if steps > 0 => Some(x),
            _ => None,
        })
    }

    /// Whether scrutinee `scrutinee` is a field of a local this body owns, of a type that needs
    /// drop: its local, else `None`. Such a scrutinee is a place expression the local owns; it
    /// is pushed as a place (`Task::Place`), tested by a match by tests where it is, and its
    /// parts move by the local's field flags. A scrutinee that needs no drop is left as it was
    /// (a copy moves nothing, and its build does not change).
    pub(in crate::walk) fn owned_field_scrutinee(&mut self, scrutinee: ExprId) -> Result<Option<PatId>, BuildError> {
        let Some(x) = self.field_chain_local(scrutinee)? else { return Ok(None) };
        if !self.local_is_owned(x) {
            return Ok(None);
        }
        let t = self.adjusted_ty(scrutinee)?;
        let g = self.ground_ty(t).map_err(|error| Self::ty_err(scrutinee, error))?;
        if !self.needs_drop(g).map_err(|error| Self::ty_err(scrutinee, error))? {
            return Ok(None);
        }
        Ok(Some(x))
    }

    /// `scan_flags`, for an arm (or a `let`-`else`'s pattern) `pat` over `scrutinee`: when the
    /// scrutinee is a field of a local (`field_chain_local`) of a type that needs drop and
    /// `pat` binds some part by value (itself, or some binding inside it: `moves_deep`), the
    /// local gets field flags (`partial`), the word `move_out` clears the moved part's bit in
    /// and the local's scope end drops the rest by (`drop_moved`).
    pub(in crate::walk) fn scan_field_scrutinee(&mut self, scrutinee: ExprId, pat: PatId) -> Result<(), BuildError> {
        let Some(x) = self.field_chain_local(scrutinee)? else { return Ok(()) };
        let t = self.adjusted_ty(scrutinee)?;
        let g = self.ground_ty(t).map_err(|error| Self::ty_err(scrutinee, error))?;
        if !self.needs_drop(g).map_err(|error| Self::ty_err(scrutinee, error))? {
            return Ok(());
        }
        let top = matches!(self.body.pats.get(pat.ix()), Some(Pat::Bind { ann, .. }) if ann.by_ref.is_none());
        if top || self.moves_deep(pat)? {
            *self.partial.get_mut(x.ix()).ok_or(malformed("a local past `pats`"))? = true;
        }
        Ok(())
    }

    /// `move_out`, for a match whose scrutinee `scrutinee` is a field of its owning local: the
    /// steps from the local to the scrutinee appended to `steps` root first (`field_path`), so
    /// the pattern's path (`move_path`, appended next) continues it into one part's path. The
    /// local's word, the steps' start and their count. Refused by name (`p`, the binding): a
    /// container on the way that implements `Drop` (E0509: [expr.move.movable-place], "Fields
    /// of a place expression which can be moved out of and don't implement `Drop`"), and a path
    /// `field_path` does not follow (a union's field, or a local `scan_field_scrutinee` gave no
    /// field flags).
    pub(super) fn field_scrutinee_prefix(&mut self, scrutinee: ExprId, p: PatId) -> Result<(SlotId, u32, u32), BuildError> {
        let Some((_, mask, start, len, drops)) = self.field_path(scrutinee)? else {
            return Err(BuildError::UnsupportedPat {
                pat: p,
                what: "a move out of a field scrutinee of a local with no field flags or through a union's field, in a match by tests",
            });
        };
        if drops {
            self.moves.steps.truncate(start as usize);
            return Err(BuildError::UnsupportedPat { pat: p, what: "a move out of a field of a type that implements `Drop` (E0509)" });
        }
        Ok((mask, start, len))
    }
}
