//! Moves out of a `match` by tests' scrutinee (walk/matches.rs): `match w { W(P(s), _) => s, .. }`
//! where `s` needs drop. Rules from the Reference:
//! - "Patterns", "Identifier patterns": "By default, identifier patterns bind a variable to a
//!   copy of or move from the matched value depending on whether the matched value implements
//!   `Copy`." A by-value binding of a part that needs drop (never `Copy`) moves that part.
//! - "Destructors": "When an initialized variable or temporary goes out of scope, its
//!   destructor is run"; "If a variable has been partially initialized, only its initialized
//!   fields are dropped"; drop glue: "The fields of a struct are dropped in declaration order.
//!   The fields of the active enum variant are dropped in declaration order. The fields of a
//!   tuple are dropped in order. The elements of an array ... are dropped from the first
//!   element to the last."
//! - "Destructors", "Temporary scopes": the temporary scope of an expression is "the smallest
//!   scope that contains the expression and is one of" a list that does not name a `match`
//!   scrutinee, so a temporary scrutinee lives to the end of its statement
//!   (walk/temps.rs `hold_temp` and `Task::TempEnd`), dropped then, minus what an arm moved.
//!
//! Built: each moved part is a path of steps from the scrutinee (a struct or tuple field, an
//! enum variant's field, an array element) with a bit in a 64-bit word of the scrutinee's
//! owner, set while the part is held. A local's word is its field flags (walk.rs `Owned::rest`,
//! set at its `let` and on every assignment over it; its own switch's `field_bit`s come first),
//! and it drops at its scope's end; a temporary's word is a slot set at the match's start, and
//! it drops at its statement's end (`Held::Moved`), or with the scope on a `return` or `break`.
//! An arm clears its parts' bits; the drop walks the value's type and drops every part that
//! needs drop and holds no moved part whole, tests the bit of each moved part, and descends
//! only into parts holding a moved one (an enum by its discriminant, read again: the value does
//! not change between the arm's test and its drop). Refused by name: a move out of a type that
//! implements `Drop` (E0509), from behind a reference or inside a slice, a zero-sized part,
//! and more than 64 parts of one value. A guarded arm's moves are held until its guard passed
//! (`Pending`). An or-pattern's alternative moves on its own pass (its bit cleared there); the
//! name's first binding, which every alternative stores into, owns the part at the arm's pass.
//!
//! The same bits serve a move by a place expression (`let a = t.0;`, `move_field`: the local
//! gets its word in `scan_field_moves`) and an assignment over a local's part (`assign_over`:
//! [expr.assign.drop-target]: "It then has the effect of first dropping the value at the
//! assigned place, unless the place is an uninitialized local variable or an uninitialized
//! field of a local variable": the old
//! value drops by its bits, then every bit at or under the place is set again). An assignment
//! is walked once, so a part first moved later inside the same loop is refused (`part_bit`).

use core::alloc::Allocator;

use blaze_lir::builder::MemAttrs;
use blaze_lir::func::{SlotId, SlotKind};
use blaze_lir::{Block, Flags, IntCond, Op, Ty, Value};
use blaze_own::Vec;
use blaze_typeck::{AdjustKind, Expr, ExprId, PNode, PTy, Pat, PatId, UnOp};

use super::{Held, NO_EXPR, Owner, Repr, Shape, V, Walk, malformed, unsupported};
use crate::BuildError;

mod boxes;
pub(super) mod env_places;
mod field_scrutinee;
mod struct_update;

/// One step down a value: a struct's or tuple's field (its row of `layout_fields`), a field of
/// an enum's variant `v` (its row), an array's element `k`, a `Box`'s contents (at its data
/// pointer: walk/moves/boxes.rs).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Step {
    Field(u32),
    Variant { v: u32, row: u32 },
    Elem(u32),
    Deref,
}

/// A part moved out of the value whose word is slot `mask`: bit `bit`, its path
/// `steps[start..start + len]`.
#[derive(Clone, Copy)]
pub(super) struct Part {
    mask: SlotId,
    bit: u32,
    start: u32,
    len: u32,
}

/// A match by tests whose scrutinee needs drop and is owned: by a local (its field flags) or
/// as a temporary (`mask`, the match's own word).
#[derive(Clone, Copy)]
pub(super) struct Root {
    m: u32,
    expr: ExprId,
    owner: Owner,
    mask: Option<SlotId>,
    g: PTy,
    /// A scrutinee that is a field of its owning local (`Owner::Local`; `match self.failed`):
    /// the steps to it start every moved part's path (field_scrutinee.rs).
    field: Option<ExprId>,
}

/// An effect of a guarded arm's bindings held until its guard passed. The Reference, "Match
/// guards": a by-value binding is moved only after the guard is true; while the guard runs it
/// is a shared reference to its part. So the bindings are stored (the guard reads those
/// bytes), but nothing is owned or taken from the scrutinee yet: a false guard leaves the
/// scrutinee whole for the next arm, and its copies are dropped by no one.
#[derive(Clone, Copy)]
pub(super) enum Pending {
    /// Binding `p` owns its part (`own`).
    Own(PatId),
    /// Every binding of pattern `p` owns its part (`own_pat`, the switch's).
    OwnPat(PatId),
    /// Bits `bits` of word `mask` cleared: their parts moved out.
    Clear { mask: SlotId, bits: u64 },
    /// Local `x` moved whole by `expr` (`moved`).
    Moved { expr: ExprId, x: PatId },
}

/// The arm being bound: its pattern, its match's root, and whether it has a guard (its moves
/// are held: `Pending`).
#[derive(Clone, Copy)]
pub(super) struct Cx {
    pat: PatId,
    root: Root,
    guarded: bool,
}

/// An assignment over a local's part (path `steps[start..start + len]`, `len` 0: the whole
/// local) of word `mask`, made by expression `by`: walked before any part moved under it later.
#[derive(Clone, Copy)]
struct Assigned {
    mask: SlotId,
    start: u32,
    len: u32,
    by: ExprId,
}

/// What the moves of a body's matches by tests and field moves keep (one `Walk` field).
pub(super) struct Moves<A: Allocator> {
    steps: Vec<Step, A>,
    parts: Vec<Part, A>,
    roots: Vec<Root, A>,
    assigned: Vec<Assigned, A>,
    cx: Option<Cx>,
    /// The effects guarded arms hold until their guards pass, innermost last.
    pending: Vec<Pending, A>,
    /// The first bindings an or-pattern's alternatives moved a part into (each alternative
    /// cleared its own bits on its pass), owned once at the arm's pass (walk/matches.rs
    /// `bind_with`).
    alt_owned: Vec<PatId, A>,
    /// The last `Box` whose first adjustment `box_deref_builtin` built: its expression and the
    /// pointer to it. A move out of a temporary box's contents (walk/moves/boxes.rs
    /// `move_temp_box_contents`) finds the box's held entry by this pointer: the contents'
    /// place it is given is the data pointer, which says nothing of where the box is.
    box_self: Option<(ExprId, Value)>,
}

impl<A: Allocator + Copy> Moves<A> {
    pub(super) fn new_in(alloc: A) -> Self {
        Moves {
            steps: Vec::new_in(alloc),
            parts: Vec::new_in(alloc),
            roots: Vec::new_in(alloc),
            assigned: Vec::new_in(alloc),
            cx: None,
            pending: Vec::new_in(alloc),
            alt_owned: Vec::new_in(alloc),
            box_self: None,
        }
    }
}

/// A node of a value being dropped: its type, its offset from the value, the length of its
/// path, a part whose path starts with its path (`None`: no part under it), and the switch's
/// field flag that holds it (a local enum's direct field).
#[derive(Clone, Copy)]
struct Node {
    g: PTy,
    off: i32,
    /// The pointer `off` is from, under a `Box`'s contents (`None`: the dropped value's own
    /// place, `at`).
    base: Option<Value>,
    depth: u32,
    rep: Option<u32>,
    field_bit: Option<u32>,
}

#[derive(Clone, Copy)]
enum Do {
    Visit(Node),
    Begin(Block),
    Jump(Block),
    /// The `Box` of type `g` at `place`, its contents visited: its allocation freed alone.
    BoxFree { g: PTy, place: (Value, i32) },
}

/// A pattern reached while finding a binding's path: the pattern, its type, its parent's
/// index (`u32::MAX` for the root), the step from the parent, and whether its type implements
/// `Drop`.
type Seen = (PatId, PTy, u32, Option<Step>, bool);

const MOVE_OUT: &str = "a binding by value of a part that needs drop, in a match by tests (a move out)";

impl<A: Allocator + Copy> Walk<'_, '_, '_, A> {
    /// Before match `m` by tests (`e`, on `scrutinee`, its value `s` held as `srepr`) tests its
    /// arms: a scrutinee that needs drop is owned. A local (walk/matches.rs `tested_place`
    /// takes its place) moves by its field flags; a temporary joins its statement's
    /// temporaries (`hold_temp`) as `Held::Moved` with a word of its own, all held, and its
    /// place is returned for the arms to test.
    pub(super) fn own_scrutinee(&mut self, m: u32, e: ExprId, scrutinee: ExprId, s: V, srepr: Repr) -> Result<Option<(Value, i32, PTy)>, BuildError> {
        self.moves.roots.retain(|r| r.m != m);
        let t = self.adjusted_ty(scrutinee)?;
        let g = self.ground_ty(t).map_err(|error| Self::ty_err(scrutinee, error))?;
        if !self.needs_drop(g).map_err(|error| Self::ty_err(scrutinee, error))? {
            return Ok(None);
        }
        if self.adjust_rows(scrutinee).is_empty()
            && let Some(x) = self.local_of(scrutinee)?
        {
            self.moves.roots.push(Root { m, expr: e, owner: Owner::Local(x), mask: None, g, field: None });
            return Ok(None);
        }
        // A part of an owned local by fields and `Box` contents (`match self.failed`, `self:
        // Held`; `if let Type::Path(mut ty) = *group.elem`): matched in place, its parts moved
        // by the local's field flags (field_scrutinee.rs). Asked before `scrutinee_in_place`,
        // which takes every dereference as behind a pointer: a box's contents are the box
        // owner's ([expr.move.movable-place]).
        if let Some(x) = self.owned_field_scrutinee(scrutinee)? {
            self.moves.roots.push(Root { m, expr: e, owner: Owner::Local(x), mask: None, g, field: Some(scrutinee) });
            return Ok(None);
        }
        // A place behind a pointer, matched in place: no one here owns it, nothing is held,
        // and a by-value binding of a part that needs drop is refused (`move_out`).
        if self.scrutinee_in_place(scrutinee)? {
            return Ok(None);
        }
        let (V::Place(p, off), Repr::Agg(_)) = (s, srepr) else {
            return Err(unsupported(scrutinee, "a temporary scrutinee that needs drop held as other than an aggregate (a `Box`), in a match by tests"));
        };
        self.hold_temp(scrutinee, (p, off), srepr)?;
        let slot = self.b.slot(8, 3, SlotKind::Named);
        let fa = self.b.slot_addr(slot);
        let all = self.b.iconst(Ty::I64, u128::from(u64::MAX));
        self.b.store(Flags::NONE, all, fa, 0, MemAttrs::aligned(3));
        // `hold_temp`'s entry for it (a whole `Held::Rest` at its place) takes the word.
        let held = self.scope_pats.iter().rposition(|h| matches!(*h, Held::Rest { at, variant: None, .. } if at == (p, off)));
        let held = held.ok_or(malformed("a held temporary scrutinee missing from its scope"))?;
        self.scope_pats[held] = Held::Moved { at: (p, off), g, mask: slot };
        self.moves.roots.push(Root { m, expr: e, owner: Owner::Temp, mask: Some(slot), g, field: None });
        Ok(Some((p, off, g)))
    }

    /// A parameter whose pattern leaves a part that needs drop unbound (walk/matches/irrefutable.rs
    /// `bind_param`): the argument at `at` (the callee's own copy), of ground type `g`, is owned
    /// as a temporary scrutinee is, the root of `m` (the next match's number, as `bind_let_owned`
    /// takes it): a word of its own, all held, and `Held::Moved` in the innermost drop scope (the
    /// parameters'), pushed before the parameter's bindings so it drops after them. No expression
    /// moves it: its root names the body's root, which no assignment is under before the
    /// parameters are bound (`part_bit`'s loop check never applies).
    pub(super) fn own_param(&mut self, m: u32, at: (Value, i32), g: PTy) {
        self.moves.roots.retain(|r| r.m != m);
        let slot = self.b.slot(8, 3, SlotKind::Named);
        let fa = self.b.slot_addr(slot);
        let all = self.b.iconst(Ty::I64, u128::from(u64::MAX));
        self.b.store(Flags::NONE, all, fa, 0, MemAttrs::aligned(3));
        self.scope_pats.push(Held::Moved { at, g, mask: slot });
        self.moves.roots.push(Root { m, expr: self.body.root, owner: Owner::Temp, mask: Some(slot), g, field: None });
    }

    /// Arm `pat` of match `m` is about to bind (`guarded`: it has a guard): its moves are of
    /// its match's root, if the match owns its scrutinee.
    pub(super) fn move_arm(&mut self, m: u32, pat: PatId, guarded: bool) {
        let root = self.moves.roots.iter().rev().find(|r| r.m == m).copied();
        self.moves.cx = root.map(|root| Cx { pat, root, guarded });
    }

    /// The arm's bindings are bound.
    pub(super) fn move_arm_end(&mut self) {
        self.moves.cx = None;
    }

    /// Or-alternative binding `first` (a name's first binding) took a part by value: owned at
    /// the arm's pass (`take_alt_moved`).
    pub(super) fn alt_moved(&mut self, first: PatId) {
        if !self.moves.alt_owned.contains(&first) {
            self.moves.alt_owned.push(first);
        }
    }

    /// Whether an alternative moved a part into `first`; forgets it.
    pub(super) fn take_alt_moved(&mut self, first: PatId) -> bool {
        match self.moves.alt_owned.iter().position(|&q| q == first) {
            Some(i) => {
                self.moves.alt_owned.swap_remove(i);
                true
            }
            None => false,
        }
    }

    /// Binding `p` (of type `g`, needing drop) binds its part by value: the part moves out of
    /// the scrutinee, its bit cleared. Outside a match that owns its scrutinee (an `if let`,
    /// a `let`-`else`) refused by name. `alt`: `p` is an or-alternative's, bound on that
    /// alternative's own pass, so its effect is a store on that path (a bit cleared), never a
    /// fact of the walk (a whole local's move, `moved`, is path-blind: refused there).
    pub(super) fn move_out(&mut self, p: PatId, g: PTy, alt: bool) -> Result<(), BuildError> {
        let Some(cx) = self.moves.cx else { return Err(BuildError::UnsupportedPat { pat: p, what: MOVE_OUT }) };
        // A guarded arm's move is held until its guard passed (`Pending`).
        if self.repr(g).map_err(|error| Self::pat_ty_err(p, error))? == Repr::None {
            return Err(BuildError::UnsupportedPat { pat: p, what: "a move out of a zero-sized part that needs drop" });
        }
        // A field scrutinee of its owning local (`match self.failed`): the steps from the local
        // to it first, the pattern's path after them, one part's path in the local's word.
        let prefix = match cx.root.field {
            Some(s) => Some(self.field_scrutinee_prefix(s, p)?),
            None => None,
        };
        let (start, len) = match self.move_path(cx.pat, cx.root.g, p) {
            Ok(path) => path,
            Err(error) => {
                if let Some((_, pstart, _)) = prefix {
                    self.moves.steps.truncate(pstart as usize);
                }
                return Err(error);
            }
        };
        let (start, len) = match prefix {
            Some((_, pstart, plen)) => (pstart, plen + len),
            None => (start, len),
        };
        let mask = match (cx.root.owner, prefix) {
            // The local's word (`scan_field_scrutinee` gave it): `field_path` found it.
            (_, Some((mask, _, _))) => mask,
            (Owner::Local(x), None) if len == 0 => {
                // The whole local moves (its flag, set by `scan_flags`).
                self.moves.steps.truncate(start as usize);
                if alt {
                    return Err(BuildError::UnsupportedPat { pat: p, what: "a by-value binding of a whole local scrutinee inside an or-pattern" });
                }
                return self.arm_effect(Pending::Moved { expr: cx.root.expr, x }, cx.guarded);
            }
            (Owner::Local(x), None) => self
                .field_mask(x)
                .ok_or(malformed("a match by tests moving out of a local without field flags (`scan_flags`)"))?,
            (Owner::Temp, None) => cx.root.mask.ok_or(malformed("a temporary scrutinee without its word"))?,
            (Owner::None, None) => return Err(malformed("a part moved out of a scrutinee no one owns")),
        };
        // A local enum's switch owns its first `field_bit`s (a whole local's only: a local with
        // a field scrutinee is a struct or tuple, `field_path`'s `Shape::Fields`).
        let base = match (cx.root.owner, prefix, self.layout(cx.root.g).map(|l| self.layouts[l as usize].shape)) {
            (Owner::Local(_), None, Ok(Shape::Enum { first, len, .. })) => (0..len).map(|v| self.layout_variants[(first + v) as usize].1).sum(),
            _ => 0u32,
        };
        let Some(bit) = self.part_bit(mask, start, len, base, Some(cx.root.expr))? else {
            return Err(BuildError::UnsupportedPat { pat: p, what: "a match moving out more than 64 parts of one value (its word)" });
        };
        self.arm_effect(Pending::Clear { mask, bits: 1u64 << bit }, cx.guarded)
    }

    /// Whether the arm being bound holds its moves until its guard passed (`Pending`).
    pub(super) fn holding_moves(&self) -> bool {
        self.moves.cx.is_some_and(|cx| cx.guarded)
    }

    /// Where the effects held for the next guarded arm start: its `Task::ArmMoves`.
    pub(super) fn pending_mark(&self) -> u32 {
        self.moves.pending.len() as u32
    }

    /// Applies effect `e` of an arm's bindings now, or with `hold` after its guard passed.
    pub(super) fn arm_effect(&mut self, e: Pending, hold: bool) -> Result<(), BuildError> {
        if hold {
            self.moves.pending.push(e);
            return Ok(());
        }
        match e {
            Pending::Own(p) => self.own(p, true),
            Pending::OwnPat(p) => self.own_pat(p),
            Pending::Clear { mask, bits } => {
                self.clear_bits(mask, bits);
                Ok(())
            }
            Pending::Moved { expr, x } => self.moved(expr, x),
        }
    }

    /// An arm's guard passed (`Task::ArmMoves`): the effects its bindings held from `from` on
    /// are applied in the order they were bound, at the start of its body. A guard's own
    /// guarded arms held theirs after `from` and applied them first (the work list is a stack).
    pub(super) fn arm_moves(&mut self, from: u32) -> Result<(), BuildError> {
        let from = from as usize;
        if from > self.moves.pending.len() {
            return Err(malformed("an arm's held moves past the list"));
        }
        for i in from..self.moves.pending.len() {
            let e = self.moves.pending[i];
            self.arm_effect(e, false)?;
        }
        self.moves.pending.truncate(from);
        Ok(())
    }

    /// The bit of the part at path `steps[start..start + len]` of the value whose word is
    /// `mask` (`base`: the bits before the parts, a local enum's switch's): the bit of an equal
    /// part already known (the path's steps given back), else a new one; `None` past 64.
    /// `moved_by`: the expression that moves the part out (`None`: an assignment names it). A
    /// NEW part moved under a path an earlier assignment wrote (`assigned`), both inside one
    /// loop, is refused: that assignment, walked first, did not test this part's bit, yet in
    /// the loop's next turn it runs after this move.
    fn part_bit(&mut self, mask: SlotId, start: u32, len: u32, base: u32, moved_by: Option<ExprId>) -> Result<Option<u32>, BuildError> {
        if let Some(q) = self.moves.parts.iter().find(|q| q.mask == mask && self.same_path(q.start, q.len, start, len)).copied() {
            self.moves.steps.truncate(start as usize);
            return Ok(Some(q.bit));
        }
        let Some(by) = moved_by else { return self.new_part(mask, start, len, base) };
        for i in 0..self.moves.assigned.len() {
            let a = self.moves.assigned[i];
            let under = a.mask == mask
                && a.len <= len
                && self.moves.steps[a.start as usize..(a.start + a.len) as usize] == self.moves.steps[start as usize..(start + a.len) as usize];
            if under && self.in_one_loop(a.by, by)? {
                self.moves.steps.truncate(start as usize);
                return Err(unsupported(
                    by,
                    "a move out of a part of a local that an assignment earlier in the same loop writes over (the assignment would not test the part's bit)",
                ));
            }
        }
        self.new_part(mask, start, len, base)
    }

    /// A new part at path `steps[start..start + len]` of word `mask`: its bit, `None` past 64.
    fn new_part(&mut self, mask: SlotId, start: u32, len: u32, base: u32) -> Result<Option<u32>, BuildError> {
        let used = self.moves.parts.iter().filter(|q| q.mask == mask).count() as u32;
        let bit = base + used;
        if bit >= 64 {
            self.moves.steps.truncate(start as usize);
            return Ok(None);
        }
        self.moves.parts.push(Part { mask, bit, start, len });
        Ok(Some(bit))
    }

    /// Clears bit `bit` of word `mask`: its part moved out.
    fn clear_bit(&mut self, mask: SlotId, bit: u32) {
        self.clear_bits(mask, 1u64 << bit);
    }

    /// Clears the bits `bits` of word `mask`: their parts moved out.
    pub(super) fn clear_bits(&mut self, mask: SlotId, bits: u64) {
        let fa = self.b.slot_addr(mask);
        let word = self.b.load(Ty::I64, Flags::NONE, fa, 0, MemAttrs::aligned(3));
        let keep = self.b.iconst(Ty::I64, u128::from(!bits));
        let left = self.b.binary(Op::And, Flags::NONE, word, keep);
        self.b.store(Flags::NONE, left, fa, 0, MemAttrs::aligned(3));
    }

    /// Whether expressions `a` and `b` are inside one `loop` (by `scan_flags`'s parents).
    fn in_one_loop(&self, a: ExprId, b: ExprId) -> Result<bool, BuildError> {
        let mut x = a.0;
        while let Some(&(up, _)) = self.expr_parent.get(x as usize)
            && up != NO_EXPR
        {
            x = up;
            if !matches!(self.expr(ExprId(x))?, Expr::Loop { .. }) {
                continue;
            }
            let mut y = b.0;
            while let Some(&(up_b, _)) = self.expr_parent.get(y as usize)
                && up_b != NO_EXPR
            {
                y = up_b;
                if y == x {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    /// Whether paths `steps[a..a + al]` and `steps[b..b + bl]` are equal.
    fn same_path(&self, a: u32, al: u32, b: u32, bl: u32) -> bool {
        al == bl && self.moves.steps[a as usize..(a + al) as usize] == self.moves.steps[b as usize..(b + bl) as usize]
    }

    /// The path from pattern `root` (of type `g`) to binding `target`, appended to `steps`
    /// root first: its start and length. A search by a work list; a reference or a slice is
    /// not followed (no move goes through one), so a binding under one is refused by name, and
    /// so is one under a type that implements `Drop`. An or-pattern's alternatives are.
    fn move_path(&mut self, root: PatId, g: PTy, target: PatId) -> Result<(u32, u32), BuildError> {
        let mut seen: Vec<Seen, A> = Vec::new_in(self.alloc);
        let mut todo: Vec<(PatId, PTy, u32, Option<Step>), A> = Vec::new_in(self.alloc);
        todo.push((root, g, u32::MAX, None));
        let mut found = None;
        while let Some((q, g, parent, step)) = todo.pop() {
            let me = seen.len() as u32;
            if q.ix() == target.ix() {
                seen.push((q, g, parent, step, false));
                found = Some(me);
                break;
            }
            let pat = self.pat(q)?;
            if let Pat::Bind { sub: Some(s), .. } = pat {
                seen.push((q, g, parent, step, false));
                todo.push((s, g, me, None));
                continue;
            }
            // An or-pattern: each alternative is over the same value (no step); the binding is
            // in one of them, moved on that alternative's own pass (walk/matches.rs `bind_with`).
            if let Pat::Or { alts } = pat {
                seen.push((q, g, parent, step, false));
                let subs = self.body.pat_lists.get(alts.range()).ok_or(malformed("an or-pattern's alternatives past `pat_lists`"))?;
                for &sub in subs {
                    todo.push((sub, g, me, None));
                }
                continue;
            }
            if !matches!(pat, Pat::Tuple { .. } | Pat::TupleStruct { .. } | Pat::Struct { .. } | Pat::Record { .. } | Pat::Slice { .. }) {
                continue;
            }
            match self.ground_node(g).map_err(|error| Self::pat_ty_err(q, error))? {
                // The default binding mode becomes `ref`: nothing below moves.
                PNode::Ref(..) => continue,
                PNode::Array(elem, n) => {
                    let Pat::Slice { elems, rest } = pat else { return Err(malformed("an array matched by other than a slice pattern")) };
                    seen.push((q, g, parent, step, false));
                    let n = self.ground_len(n).map_err(|error| Self::pat_ty_err(q, error))?;
                    let subs = self.body.pat_lists.get(elems.range()).ok_or(malformed("a pattern's elements past `pat_lists`"))?;
                    let count = subs.len() as u64;
                    for (j, &sub) in subs.iter().enumerate() {
                        let j = j as u64;
                        let k = match rest {
                            // `x @ ..`: a subarray, not followed.
                            Some(r) if j == u64::from(r) => continue,
                            Some(r) if j > u64::from(r) => n - (count - j),
                            _ => j,
                        };
                        let k = u32::try_from(k).map_err(|_| BuildError::UnsupportedPat { pat: q, what: "a move out of an array of 2^32 elements or more" })?;
                        todo.push((sub, elem, me, Some(Step::Elem(k))));
                    }
                    continue;
                }
                _ => {}
            }
            let l = self.layout(g).map_err(|error| Self::pat_ty_err(q, error))?;
            let is_enum = matches!(self.layouts[l as usize].shape, Shape::Enum { .. });
            let drops = self.layouts[l as usize].drop_fn.is_some();
            seen.push((q, g, parent, step, drops));
            let variant = match pat {
                Pat::TupleStruct { .. } | Pat::Struct { .. } => {
                    Self::def_variant(self.pat_def(q)).ok_or(malformed("a variant pattern the check did not resolve"))?
                }
                _ => 0,
            };
            let (first, len, _) = self.variant_rows(l, variant)?;
            let step_of = |row: u32| if is_enum { Step::Variant { v: variant, row } } else { Step::Field(row) };
            match pat {
                Pat::Tuple { elems, rest } | Pat::TupleStruct { elems, rest, .. } => {
                    let subs = self.body.pat_lists.get(elems.range()).ok_or(malformed("a pattern's elements past `pat_lists`"))?;
                    let n = len as usize;
                    for (j, &sub) in subs.iter().enumerate() {
                        let k = match rest {
                            Some(r) if j >= r as usize => n.checked_sub(subs.len() - j).ok_or(malformed("a tuple pattern longer than its type"))?,
                            _ => j,
                        };
                        let row = first + k as u32;
                        todo.push((sub, self.layout_field_tys[row as usize], me, Some(step_of(row))));
                    }
                }
                Pat::Struct { fields, .. } | Pat::Record { fields, .. } => {
                    let fps = self.body.field_pats.get(fields.range()).ok_or(malformed("a struct pattern's fields past `field_pats`"))?;
                    for fp in fps {
                        let row = (first..first + len)
                            .find(|&i| self.layout_fields[i as usize].0 == fp.name)
                            .ok_or(malformed("a struct pattern naming a field its variant lacks"))?;
                        todo.push((fp.pat, self.layout_field_tys[row as usize], me, Some(step_of(row))));
                    }
                }
                // A slice pattern over other than an array: no move out of a slice.
                _ => {}
            }
        }
        let Some(found) = found else {
            return Err(BuildError::UnsupportedPat {
                pat: target,
                what: "a move out of a part behind a reference or inside a slice, in a match by tests",
            });
        };
        // Root first: the chain from `found` up, reversed into `steps`.
        // Every pattern above the binding is a container the part leaves: none may implement
        // `Drop`.
        let start = self.moves.steps.len() as u32;
        let mut at = found;
        loop {
            let (_, _, parent, step, _) = seen[at as usize];
            if let Some(s) = step {
                self.moves.steps.push(s);
            }
            if parent == u32::MAX {
                break;
            }
            if seen[parent as usize].4 {
                self.moves.steps.truncate(start as usize);
                return Err(BuildError::UnsupportedPat { pat: target, what: "a move out of a type that implements `Drop` (E0509)" });
            }
            at = parent;
        }
        let len = self.moves.steps.len() as u32 - start;
        self.moves.steps[start as usize..].reverse();
        Ok((start, len))
    }

    /// Whether binding drop `drop_owned` of a value of type `g` with field flags `slot` is a
    /// drop of what moves left (`drop_moved`): a struct, tuple or array always (the switch's
    /// flags are an enum's), an enum when a match by tests moved out of it.
    pub(super) fn moves_take(&mut self, g: PTy, slot: SlotId) -> Result<bool, BuildError> {
        let l = self.layout(g).map_err(|_| malformed("a binding with field flags and no layout"))?;
        Ok(!matches!(self.layouts[l as usize].shape, Shape::Enum { .. }) || self.moves.parts.iter().any(|q| q.mask == slot))
    }

    /// Drops the value of type `g` at `at` minus the parts moved out of it (word `mask`, a bit
    /// set while its part is held). `switch_bits`: a local's word, whose first bits are its
    /// enum switch's `field_bit`s. In the Reference's order: fields in declaration order, an
    /// enum's live variant's fields, an array's elements first to last.
    pub(super) fn drop_moved(&mut self, g: PTy, at: (Value, i32), mask: SlotId, switch_bits: bool) -> Result<(), BuildError> {
        let mut ps: Vec<u32, A> = Vec::new_in(self.alloc);
        for (i, q) in self.moves.parts.iter().enumerate() {
            if q.mask == mask {
                ps.push(i as u32);
            }
        }
        // The switch's flags are an enum's only (a struct's word holds parts alone).
        let l = self.layout(g).map_err(|_| malformed("a value with a moves word and no layout"))?;
        let switch_bits = switch_bits && matches!(self.layouts[l as usize].shape, Shape::Enum { .. });
        self.drop_tree(Node { g, off: 0, base: None, depth: 0, rep: ps.first().copied(), field_bit: None }, at, mask, switch_bits, &ps)
    }

    /// Drops node `root` of a value at `at` (word `mask`, parts `ps`), part by part.
    fn drop_tree(&mut self, root: Node, at: (Value, i32), mask: SlotId, switch_bits: bool, ps: &[u32]) -> Result<(), BuildError> {
        let mut todo: Vec<Do, A> = Vec::new_in(self.alloc);
        todo.push(Do::Visit(root));
        while let Some(d) = todo.pop() {
            match d {
                Do::Begin(b) => self.begin(b),
                Do::Jump(b) => self.jump(b, &[]),
                Do::Visit(n) => self.drop_node(n, at, mask, switch_bits && n.depth == 0, ps, &mut todo)?,
                Do::BoxFree { g, place } => {
                    let p = self.address(place);
                    self.drop_box_free(g, p)?;
                }
            }
        }
        Ok(())
    }

    /// Whether part `q`'s path starts with path `steps[start..start + len]`.
    fn part_under(&self, q: Part, start: u32, len: u32) -> bool {
        q.len >= len && self.moves.steps[q.start as usize..(q.start + len) as usize] == self.moves.steps[start as usize..(start + len) as usize]
    }

    /// The local and the path (appended to `steps`, root first: its start and length) of
    /// place expression `e` when it is a chain of struct or tuple fields and `Box` contents
    /// (an overloaded `*b` of a box, a base's autoderef through boxes) of a local with field
    /// flags (`Owned::rest`), with no other adjustment on the way; `None` otherwise, `steps`
    /// as it was. `drops`: some container on the path implements `Drop` (E0509 for a move
    /// out; a box's contents are no such field). walk/moves/boxes.rs `place_path`.
    fn field_path(&mut self, e: ExprId) -> Result<Option<(PatId, SlotId, u32, u32, bool)>, BuildError> {
        self.place_path(e, 0)
    }

    /// `let a = t.f.g;`: a move out of a local's field by a place expression `e` (its value
    /// at `at`, held as `repr`, which needs drop). The Reference, [expr.move.movable-place]:
    /// "Only the following place expressions may be moved out of: Variables which are not
    /// currently borrowed. Temporary values. Fields of a place expression which can be moved
    /// out of and don't implement `Drop`. ..."; [expr.move.deinitialization]: "After moving out
    /// of a place expression that evaluates to a local variable, the location is deinitialized
    /// and cannot be read from again until it is reinitialized"; "Destructors": "If a variable has been partially initialized, only its
    /// initialized fields are dropped". The part's bit in the local's word is cleared; the
    /// local's scope end drops the rest (`drop_moved`). The value is copied out first: a later
    /// assignment over the field in the same expression must not change it. Refused by name: a
    /// field of a temporary or behind a pointer, and a move out of a type that implements
    /// `Drop` (E0509).
    pub(super) fn move_field(&mut self, e: ExprId, at: (Value, i32), repr: Repr) -> Result<V, BuildError> {
        let Some((_, mask, start, len, drops)) = self.field_path(e)? else {
            // Not a local's: a temporary's field, or a field behind a pointer.
            return self.move_temp_field(e, at, repr);
        };
        if drops {
            self.moves.steps.truncate(start as usize);
            return Err(unsupported(e, "a move out of a field of a type that implements `Drop` (E0509)"));
        }
        let Some(bit) = self.part_bit(mask, start, len, 0, Some(e))? else {
            return Err(unsupported(e, "a move out of more than 64 parts of one local (its word)"));
        };
        self.clear_bit(mask, bit);
        let tmp = self.temp(repr);
        self.store(V::Place(at.0, at.1), repr, tmp)?;
        Ok(V::Place(tmp.0, tmp.1))
    }

    /// `let a = make().f;`: a move by place expression `e` (its value at `at`, held as `repr`,
    /// which needs drop) out of a field of a TEMPORARY. The Reference, [expr.move.movable-place]:
    /// "Only the following place expressions may be moved out of: ... Temporary values. Fields
    /// of a place expression which can be moved out of and don't implement `Drop`."; "Destructors",
    /// "Temporary scopes": the temporary lives to the end of its temporary scope (its statement),
    /// and "If a variable has been partially initialized, only its initialized fields are
    /// dropped" (a temporary alike). Built as a match by tests' temporary scrutinee is
    /// (`own_scrutinee`): the temporary's `hold_temp` entry (a whole `Held::Rest`) becomes a
    /// `Held::Moved` with a word of its own, all held, and the moved part's bit is cleared, so
    /// the statement's end drops the rest and the moved field is dropped by its new owner only.
    /// Refused by name: a field reached through a pointer or a smart pointer (Rust's E0507,
    /// unless the type is `Copy`, which never needs drop; a `Box`'s contents moved out by a
    /// field path is not built), a container that implements `Drop` (E0509), more than 64
    /// parts of one temporary, and a temporary not held to its statement's end.
    fn move_temp_field(&mut self, e: ExprId, at: (Value, i32), repr: Repr) -> Result<V, BuildError> {
        let Some((base, start, len, drops, off)) = self.temp_field_path(e)? else {
            return Err(unsupported(
                e,
                "a move out of a field of other than a temporary or a local with field flags (behind a pointer or a smart pointer: E0507 unless a `Box`'s, whose contents moved out by a field are not built to LIR yet)",
            ));
        };
        if drops {
            self.moves.steps.truncate(start as usize);
            return Err(unsupported(e, "a move out of a field of a type that implements `Drop` (E0509)"));
        }
        let held_at = (at.0, at.1 - off);
        let found = self
            .scope_pats
            .iter()
            .rposition(|h| matches!(*h, Held::Rest { at: a, variant: None, .. } | Held::Moved { at: a, .. } if a == held_at));
        let Some(i) = found else {
            self.moves.steps.truncate(start as usize);
            let t = self.expr_ty(base)?;
            let g = self.ground_ty(t).map_err(|error| Self::ty_err(base, error))?;
            if !self.live || !self.needs_drop(g).map_err(|error| Self::ty_err(base, error))? {
                // Unreachable code, or a temporary whose own type needs no drop (e.g.
                // `ManuallyDrop::new(x).value`): `Task::Spill` held nothing because nothing in
                // the temporary drops at statement end.
                let tmp = self.temp(repr);
                self.store(V::Place(at.0, at.1), repr, tmp)?;
                return Ok(V::Place(tmp.0, tmp.1));
            }
            return Err(unsupported(e, "a move out of a field of a temporary not held to its statement's end (an extended temporary)"));
        };
        let mask = match self.scope_pats[i] {
            // A second part of the same temporary: its word is made.
            Held::Moved { mask, .. } => mask,
            _ => {
                let t = self.expr_ty(base)?;
                let g = self.ground_ty(t).map_err(|error| Self::ty_err(base, error))?;
                let slot = self.b.slot(8, 3, SlotKind::Named);
                let fa = self.b.slot_addr(slot);
                let all = self.b.iconst(Ty::I64, u128::from(u64::MAX));
                self.b.store(Flags::NONE, all, fa, 0, MemAttrs::aligned(3));
                self.scope_pats[i] = Held::Moved { at: held_at, g, mask: slot };
                slot
            }
        };
        let Some(bit) = self.part_bit(mask, start, len, 0, Some(e))? else {
            return Err(unsupported(e, "a move out of more than 64 parts of one temporary (its word)"));
        };
        self.clear_bit(mask, bit);
        // Copied out first, as `move_field` does.
        let tmp = self.temp(repr);
        self.store(V::Place(at.0, at.1), repr, tmp)?;
        Ok(V::Place(tmp.0, tmp.1))
    }

    /// The path of fields from a temporary to place expression `e` (a chain of struct or tuple
    /// fields with no adjustment on the way, over a value expression that is not a place and
    /// has no adjustment: `make().a.b`), appended to `steps` root first: the temporary's
    /// expression, the path's start and length, whether some container on it implements `Drop`,
    /// and the field's offset in the temporary. `None` (`steps` as it was) for anything else:
    /// a local's field, a union's, or a field behind a dereference.
    fn temp_field_path(&mut self, e: ExprId) -> Result<Option<(ExprId, u32, u32, bool, i32)>, BuildError> {
        let start = self.moves.steps.len() as u32;
        let mut drops = false;
        let mut off = 0i32;
        let mut cur = e;
        let base = loop {
            match self.expr(cur)? {
                Expr::Paren(inner) if self.adjust_rows(inner).is_empty() => cur = inner,
                Expr::Field { base, .. } | Expr::TupleField { base, .. } if self.adjust_rows(base).is_empty() => {
                    let col = self.checked.fields.column::<0>();
                    let row = col.binary_search(&cur).map_err(|_| malformed("a field access without its record row"))?;
                    let k = self.checked.fields.column::<2>()[row];
                    let Repr::Agg(l) = self.adjusted_repr(base)? else { break None };
                    if self.layouts[l as usize].is_union {
                        break None;
                    }
                    let Shape::Fields { first, len } = self.layouts[l as usize].shape else { break None };
                    if k >= len {
                        return Err(malformed("a field index past its aggregate's fields"));
                    }
                    drops |= self.layouts[l as usize].drop_fn.is_some();
                    off += self.layout_fields[(first + k) as usize].1 as i32;
                    self.moves.steps.push(Step::Field(first + k));
                    cur = base;
                }
                // The chain's root: a temporary (a call, a literal, a block) when it is no place.
                _ if cur != e && !self.is_place_expr(cur)? => break Some(cur),
                _ => break None,
            }
        };
        let Some(base) = base else {
            self.moves.steps.truncate(start as usize);
            return Ok(None);
        };
        self.moves.steps[start as usize..].reverse();
        let len = self.moves.steps.len() as u32 - start;
        Ok(Some((base, start, len, drops, off)))
    }

    /// Whether `match`/`if let`/`let`-`else` scrutinee `scrutinee` is matched IN PLACE as a
    /// place no one here owns: a place expression reached through a dereference (`*r`,
    /// `self.f` with `self: &S`, `(*b).f`), an index, or a static, of ANY type: whether a place
    /// is copied is never decided by its type or how it is held (a `ref mut` binding of `*r`
    /// with `r: &mut u32` writes `*r`; held as a scalar copy it wrote the copy). The
    /// Reference, "Match expressions": "If the scrutinee expression is a place expression, the
    /// match does not allocate a temporary location; however, a by-value binding may copy or
    /// move from the memory location"; so nothing moves but what a by-value binding binds,
    /// and such a binding of a part that needs drop is refused by name (`move_out` finds no
    /// owner: E0507 behind a reference). A local and a temporary are not this (their own
    /// owners: field flags, `own_scrutinee`); neither is a scrutinee with an adjustment (a
    /// coercion makes a new value). A `Box`'s contents reached from an owned local answer
    /// `true` here too, but are owned by that local (field_scrutinee.rs `field_chain_local`,
    /// which `own_scrutinee` and walk/matches.rs `tested_place` ask first).
    pub(super) fn scrutinee_in_place(&mut self, scrutinee: ExprId) -> Result<bool, BuildError> {
        if !self.adjust_rows(scrutinee).is_empty() {
            return Ok(false);
        }
        let mut cur = scrutinee;
        let borrowed = loop {
            match self.expr(cur)? {
                Expr::Paren(inner) if self.adjust_rows(inner).is_empty() => cur = inner,
                Expr::Field { base, .. } | Expr::TupleField { base, .. } => {
                    let rows = self.adjust_rows(base);
                    if rows.is_empty() {
                        cur = base;
                        continue;
                    }
                    // An autoderef of the base (`self.f`, `self: &S`): behind a pointer.
                    let kinds = self.checked.adjusts.column::<1>().get(rows).ok_or(malformed("adjustment rows past their column"))?;
                    break kinds.iter().all(|k| matches!(k, AdjustKind::Deref { .. }));
                }
                Expr::Unary { op: UnOp::Deref, .. } | Expr::Index { .. } => break true,
                // A static's place.
                Expr::Path(_) => break self.is_place_expr(cur)?,
                _ => break false,
            }
        };
        Ok(borrowed)
    }

    /// `lhs = v` over a value of type `g` at `at` (held as `repr`, needing drop), not a whole
    /// local. The Reference, [expr.assign.drop-target]: "It then has the effect of first
    /// dropping the value at the assigned place, unless the place is an uninitialized local
    /// variable or an uninitialized field of a local variable." For a field of a local with field flags:
    /// the old value drops minus its moved parts (each part's bit tested: a statically moved
    /// field is not dropped, one moved under a branch is dropped by its bit), then the store,
    /// then the bits of the field and of every part under it are set: it is initialized
    /// again, dropped at the scope's end. The assigned field gets a bit of its own, so a move
    /// of it walked later shares it. Anything else: the old value drops whole.
    pub(super) fn assign_over(&mut self, lhs: ExprId, g: PTy, at: (Value, i32), v: V, repr: Repr) -> Result<(), BuildError> {
        let path = self.field_path(lhs)?;
        // Under a type that implements `Drop` nothing moves (E0509), and its glue must run
        // whole: no part there.
        let Some((_, mask, start, len, false)) = path else {
            if let Some((.., start, _, true)) = path {
                self.moves.steps.truncate(start as usize);
            }
            self.drop_at(g, at)?;
            return self.store(v, repr, at);
        };
        let Some(bit) = self.part_bit(mask, start, len, 0, None)? else {
            return Err(unsupported(lhs, "an assignment naming more than 64 parts of one local (its word)"));
        };
        // The part `part_bit` found or made (an equal one found gives the steps back).
        let me = *self.moves.parts.iter().find(|q| q.mask == mask && q.bit == bit).ok_or(malformed("an assigned part missing from its word"))?;
        self.moves.assigned.push(Assigned { mask, start: me.start, len: me.len, by: lhs });
        let mut ps: Vec<u32, A> = Vec::new_in(self.alloc);
        let mut bits = 0u64;
        for (i, q) in self.moves.parts.iter().enumerate() {
            if q.mask == mask && self.part_under(*q, me.start, me.len) {
                ps.push(i as u32);
                bits |= 1u64 << q.bit;
            }
        }
        self.drop_tree(Node { g, off: 0, base: None, depth: me.len, rep: ps.first().copied(), field_bit: None }, at, mask, false, &ps)?;
        self.store(v, repr, at)?;
        let fa = self.b.slot_addr(mask);
        let word = self.b.load(Ty::I64, Flags::NONE, fa, 0, MemAttrs::aligned(3));
        let set = self.b.iconst(Ty::I64, u128::from(bits));
        let held = self.b.binary(Op::Or, Flags::NONE, word, set);
        self.b.store(Flags::NONE, held, fa, 0, MemAttrs::aligned(3));
        Ok(())
    }

    /// `p = v` over a whole local `p` with field flags, by expression `e` (walk.rs drops what
    /// it holds by `drop_owned` and sets every bit by `hold_all`): kept so a part moved later
    /// in the same loop is refused (`part_bit`), as this drop, walked first, does not test it.
    pub(super) fn note_whole_assign(&mut self, e: ExprId, p: PatId) {
        if let Some(mask) = self.field_mask(p) {
            // The empty path: every part is under it.
            self.moves.assigned.push(Assigned { mask, start: 0, len: 0, by: e });
        }
    }

    /// `lhs = v` over a placed field whose own type needs no drop (e.g. `w.m = ManuallyDrop::new(x)`),
    /// after the store: sets every bit moved under `lhs` back to held, and records the assignment
    /// for `part_bit`'s loop check.
    pub(super) fn reinit_field_bits(&mut self, lhs: ExprId) -> Result<(), BuildError> {
        let path = self.field_path(lhs)?;
        let Some((_, mask, start, len, false)) = path else {
            if let Some((.., start, _, true)) = path
                && let Ok(n) = usize::try_from(start)
            {
                self.moves.steps.truncate(n);
            }
            return Ok(());
        };
        let mut bits = 0u64;
        for q in self.moves.parts.iter() {
            if q.mask == mask && self.part_under(*q, start, len) {
                bits |= 1u64 << q.bit;
            }
        }
        if bits != 0 {
            let fa = self.b.slot_addr(mask);
            let word = self.b.load(Ty::I64, Flags::NONE, fa, 0, MemAttrs::aligned(3));
            let set = self.b.iconst(Ty::I64, u128::from(bits));
            let held = self.b.binary(Op::Or, Flags::NONE, word, set);
            self.b.store(Flags::NONE, held, fa, 0, MemAttrs::aligned(3));
        }
        self.moves.assigned.push(Assigned { mask, start, len, by: lhs });
        Ok(())
    }

    /// Whether part `q`'s path is at least `depth + 1` long and starts with part `rep`'s first
    /// `depth` steps then `next` (any step when `None`).
    fn under(&self, q: u32, rep: u32, depth: u32, next: Option<Step>) -> bool {
        let (q, r) = (self.moves.parts[q as usize], self.moves.parts[rep as usize]);
        if q.len <= depth || r.len < depth {
            return false;
        }
        let (qs, rs) = (q.start as usize, r.start as usize);
        let d = depth as usize;
        self.moves.steps[qs..qs + d] == self.moves.steps[rs..rs + d] && next.is_none_or(|s| self.moves.steps[qs + d] == s)
    }

    /// Node `n` of the value at `at`: dropped whole, skipped, or descended into (`todo`).
    fn drop_node(&mut self, n: Node, at: (Value, i32), mask: SlotId, root_switch: bool, ps: &[u32], todo: &mut Vec<Do, A>) -> Result<(), BuildError> {
        if !self.needs_drop(n.g).map_err(|_| malformed("a moved-from part's type has no drop answer"))? {
            return Ok(());
        }
        let place = match n.base {
            Some(base) => (base, n.off),
            None => (at.0, at.1 + n.off),
        };
        let exact = n.rep.and_then(|r| {
            ps.iter().copied().find(|&q| {
                let (qp, rp) = (self.moves.parts[q as usize], self.moves.parts[r as usize]);
                qp.len == n.depth && rp.len >= n.depth && self.moves.steps[qp.start as usize..(qp.start + n.depth) as usize] == self.moves.steps[rp.start as usize..(rp.start + n.depth) as usize]
            })
        });
        let exact_bit = exact.map(|q| self.moves.parts[q as usize].bit);
        let deeper = root_switch || n.rep.is_some_and(|r| ps.iter().any(|&q| self.under(q, r, n.depth, None)));
        let skip = if n.field_bit.is_some() || exact_bit.is_some() { Some(self.create()) } else { None };
        if let Some(skip) = skip {
            for bit in [n.field_bit, exact_bit].into_iter().flatten() {
                let fa = self.b.slot_addr(mask);
                let word = self.b.load(Ty::I64, Flags::NONE, fa, 0, MemAttrs::aligned(3));
                let one = self.b.iconst(Ty::I64, 1u128 << bit);
                let held = self.b.binary(Op::And, Flags::NONE, word, one);
                let zero = self.b.iconst(Ty::I64, 0);
                let set = self.b.icmp(IntCond::Ne, held, zero);
                let yes = self.create();
                self.br(set, (yes, &[]), (skip, &[]));
                self.begin(yes);
            }
        }
        if !deeper {
            self.drop_at(n.g, place)?;
            if let Some(skip) = skip {
                self.jump(skip, &[]);
                self.begin(skip);
            }
            return Ok(());
        }
        if let Some(skip) = skip {
            todo.push(Do::Begin(skip));
            todo.push(Do::Jump(skip));
        }
        let l = self.layout(n.g).map_err(|_| malformed("a moved-from part with no layout"))?;
        // A `Box` with a moved part under it: its contents at the data pointer, then its
        // allocation freed alone (walk/moves/boxes.rs).
        if let Some(t) = self.box_arg(n.g).map_err(boxes::from_ty)? {
            return self.box_node(n, t, place, ps, todo);
        }
        let child = |this: &Self, step: Step, g: PTy, off: i32, field_bit: Option<u32>| {
            let rep = n.rep.and_then(|r| ps.iter().copied().find(|&q| this.under(q, r, n.depth, Some(step))));
            Node { g, off: n.off + off, base: n.base, depth: n.depth + 1, rep, field_bit }
        };
        match self.layouts[l as usize].shape {
            Shape::Fields { .. } if self.layouts[l as usize].is_union => return Err(malformed("a move out of a union's field")),
            Shape::Fields { first, len } => {
                for row in (first..first + len).rev() {
                    let off = self.layout_fields[row as usize].1 as i32;
                    let node = child(&*self,Step::Field(row), self.layout_field_tys[row as usize], off, None);
                    todo.push(Do::Visit(node));
                }
            }
            Shape::Array { len, stride, .. } => {
                let PNode::Array(elem, _) = self.ground_node(n.g).map_err(|_| malformed("an array layout of other than an array"))? else {
                    return Err(malformed("an array layout of other than an array"));
                };
                if len > 64 {
                    return Err(malformed("a move out of an array of more than 64 elements"));
                }
                for k in (0..len).rev() {
                    let node = child(&*self,Step::Elem(k), elem, (k * stride) as i32, None);
                    todo.push(Do::Visit(node));
                }
            }
            Shape::Enum { id, first, len } => {
                // The variants a part moved out of (every one, for a local's switch flags);
                // the rest drop whole by the glue, which reads the discriminant again.
                let join = self.create();
                let default = self.create();
                let mut cases = Vec::with_capacity_in(len as usize, self.alloc);
                let mut bodies = Vec::with_capacity_in(len as usize, self.alloc);
                for v in 0..len {
                    let (ff, fl) = self.layout_variants[(first + v) as usize];
                    if !self.fields_need_drop(ff, fl) {
                        continue;
                    }
                    let moved = root_switch
                        || n.rep.is_some_and(|r| {
                            (ff..ff + fl).any(|row| ps.iter().any(|&q| self.under(q, r, n.depth, Some(Step::Variant { v, row }))))
                        });
                    if moved {
                        let block = self.create();
                        cases.push((self.discriminant_case(id, v)?, block));
                        bodies.push((block, v, ff, fl));
                    }
                }
                let d = self.read_discriminant(id, place)?;
                self.switch(d, default, &cases);
                self.begin(default);
                self.drop_at(n.g, place)?;
                self.jump(join, &[]);
                todo.push(Do::Begin(join));
                for &(block, v, ff, fl) in bodies.iter().rev() {
                    todo.push(Do::Jump(join));
                    for j in (0..fl).rev() {
                        let row = ff + j;
                        let off = self.layout_fields[row as usize].1 as i32;
                        let field_bit = if root_switch { self.field_bit(l, v, j)? } else { None };
                        let node = child(&*self,Step::Variant { v, row }, self.layout_field_tys[row as usize], off, field_bit);
                        todo.push(Do::Visit(node));
                    }
                    todo.push(Do::Begin(block));
                }
            }
        }
        Ok(())
    }

    /// Whether some binding strictly inside pattern `p` binds by value (`scan_flags`: a
    /// `match` on a local with such an arm may move a part out, so the local gets field
    /// flags).
    pub(super) fn moves_deep(&self, p: PatId) -> Result<bool, BuildError> {
        let mut todo: Vec<(PatId, bool), A> = Vec::new_in(self.alloc);
        todo.push((p, true));
        while let Some((q, root)) = todo.pop() {
            let pat = self.pat(q)?;
            if let Pat::Bind { ann, sub, .. } = pat {
                if !root && ann.by_ref.is_none() {
                    return Ok(true);
                }
                todo.extend(sub.map(|s| (s, false)));
                continue;
            }
            match pat {
                Pat::Ref { inner, .. } => todo.push((inner, false)),
                Pat::Tuple { elems, .. } | Pat::TupleStruct { elems, .. } | Pat::Slice { elems, .. } | Pat::Or { alts: elems } => {
                    let subs = self.body.pat_lists.get(elems.range()).ok_or(malformed("a pattern's elements past `pat_lists`"))?;
                    todo.extend(subs.iter().map(|&s| (s, false)));
                }
                Pat::Struct { fields, .. } | Pat::Record { fields, .. } => {
                    let fps = self.body.field_pats.get(fields.range()).ok_or(malformed("a struct pattern's fields past `field_pats`"))?;
                    todo.extend(fps.iter().map(|fp| (fp.pat, false)));
                }
                _ => {}
            }
        }
        Ok(false)
    }

    /// `scan_flags`, for an `if let`, `while let` or `let`-`else` pattern `pat`, or a `let`
    /// that owns its initializer's parts (matches/irrefutable.rs `let_owns_parts`), over
    /// `scrutinee`: when the scrutinee is a whole local (through parentheses, no adjustment),
    /// the local gets what a `match` arm over it gives it (walk.rs `scan_flags`'s
    /// `Expr::Match`): a drop flag when `pat` binds all of it by value, field flags
    /// (`partial`) when a binding inside `pat` binds a part by value (`moves_deep`), the word
    /// `move_out` clears that part's bit in.
    pub(in crate::walk) fn scan_local_scrutinee(&mut self, scrutinee: ExprId, pat: PatId) -> Result<(), BuildError> {
        if !self.adjust_rows(scrutinee).is_empty() {
            return Ok(());
        }
        let Some(x) = self.local_of(scrutinee)? else { return Ok(()) };
        let whole = matches!(self.body.pats.get(pat.ix()), Some(Pat::Bind { ann, .. }) if ann.by_ref.is_none());
        if whole {
            *self.needs_flag.get_mut(x.ix()).ok_or(malformed("a local past `pats`"))? = true;
        } else if self.moves_deep(pat)? {
            *self.partial.get_mut(x.ix()).ok_or(malformed("a local past `pats`"))? = true;
        }
        Ok(())
    }
}
