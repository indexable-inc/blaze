//! Coroutines, the builder's side (docs/v2/plan/design/coroutines.md, "The builder's two emit
//! points"; decision D73): a child module of walk.rs, the sibling of closure.rs.
//!
//! The Reference, "Async blocks": "An async block is a variant of a block expression which
//! evaluates to a future." "The future type that rustc generates is roughly equivalent to an
//! enum with one variant per await point, where each variant stores the data needed to resume
//! from its corresponding point." Here that type is the STATE the LIR pass lays out
//! (`blaze_lir::coroutine::StateLayout`): the upvars first (closure.rs's environment tuple),
//! then what the pass keeps across a suspend, then the tag.
//!
//! IDENTITY, as a closure's (typeck.md, "A closure's identity"): the owner item whose body
//! holds the `async` block, the block's ORDINAL among that body's async blocks in expression
//! order ([`coroutines_in`]), and the owner's generic arguments. The checker spells it
//! `PNode::Coroutine { owner, ordinal, args }` (blaze-typeck coroutine_id.rs) for a future of
//! another body, and that node is the ONE ground form (coroutine-identity-1): [`identity`] is
//! the one place the builder makes it, [`coroutine_of`] the one that reads it, and `ground`
//! maps its arguments as a closure's. Its output, upvars and state are facts of the identity
//! (`LayoutFacts::coroutine`, `coroutine_state`), never parts of the type. In its OWN body the
//! checker still names a block by its expression, `PNode::Future(expression, output,
//! interior)`, which no key and no ground type may hold: the walk turns it into the identity
//! ([`own_block`]), and anywhere else it is refused by name (instance.rs `ground`), never guessed.
//!
//! FACTS of that identity, as a closure's: [`CoroutineFacts`] (the upvars' tuple and the
//! output, in the owner's generic parameters: `LayoutFacts::coroutine`), and one fact a closure
//! has no like of, [`StateShape`]: the state's size, alignment and tag, which the pass decides
//! from the BUILT body of one instance (`LayoutFacts::coroutine_state`, at the owner's ground
//! arguments). So a coroutine's body is built and split before any body that holds its state by
//! value is laid out; a state not made yet is `Missing`, which caches nothing, and the build is
//! asked again once the record exists (as an enum's explicit discriminant is).

use blaze_lir::Ty;
use blaze_lir::coroutine::StateLayout;
use blaze_own::Alloc;
use blaze_flow_lower::identity::{Identity, Role};
use blaze_typeck::{BodyView, CaptureKind, Checked, EnvField, Expr, ExprId, ItemKey, List, Mutbl, PNode, PTy, PTys, PortableTys};

use crate::{BuildError, FnTrait};

// The walk's own names (`Walk`, `Repr`, `V`, `Task`, `TyErr`...), as closure.rs takes them.
use super::*;

/// The async blocks of `body` with their ORDINALS: a block's position among its owner body's
/// `async` blocks in expression order (closure.rs `closures_in` is its sibling); the
/// expression id never leaves one body's walk. The checker names a hidden coroutine by the
/// SAME rule (`blaze_typeck::coroutine_id::async_ordinal`, `PNode::Coroutine`); this loop is
/// kept, and tests/coroutine_id_agree.rs asserts the two agree.
pub fn coroutines_in<'b>(body: &'b BodyView<'_>) -> impl Iterator<Item = (u32, ExprId)> + 'b {
    // An `ExprId` is a `u32`, so every index of `exprs` fits one.
    let ids = body.exprs.iter().enumerate().filter(|(_, x)| matches!(x, Expr::AsyncBlock { .. })).map(|(i, _)| ExprId(i as u32));
    (0u32..).zip(ids)
}

/// A coroutine's identity, read from its ground type: what names its state, its `Future::poll`
/// instance and its drop across bodies.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CoroutineOf {
    /// The owner fn item whose body the `async` block is written in.
    pub owner: ItemKey,
    /// The owner's instance: its ground generic arguments.
    pub owner_args: List,
    /// Its position among the owner body's async blocks ([`coroutines_in`]).
    pub ordinal: u32,
}

/// The ground type of coroutine `ordinal` of `owner` at `owner_args`, interned in `out`:
/// `PNode::Coroutine`, the checker's own identity node (the module doc). The one constructor:
/// the walk makes its own body's blocks with it. Its output is a fact of the identity
/// (`LayoutFacts::coroutine`), not a part of the type.
pub fn identity(out: &mut PortableTys<'_>, owner: ItemKey, ordinal: u32, owner_args: List) -> Result<PTy, BuildError> {
    out.intern(PNode::Coroutine { owner, ordinal, args: owner_args }).map_err(|_| BuildError::Malformed { what: "the ground table is full" })
}

/// Ground type `ty` as a coroutine, or `None`: another type. (A future still named by its
/// expression never reaches a ground table: `ground` refuses it.)
pub fn coroutine_of(ground: PTys<'_>, ty: PTy) -> Option<CoroutineOf> {
    match ground.node(ty).ok()? {
        PNode::Coroutine { owner, ordinal, args } => Some(CoroutineOf { owner, owner_args: args, ordinal }),
        _ => None,
    }
}

/// A coroutine's facts, of its owner's generic parameters (`Param(i)`) in `types`: the answer
/// of `LayoutFacts::coroutine`.
#[derive(Clone, Copy)]
pub struct CoroutineFacts<'a> {
    pub types: PTys<'a>,
    /// Its UPVARS: a tuple with one field per capture in the checker's order, as a closure's
    /// environment is (closure.rs): `&T` for `ByRef`, `&mut T` for `ByMutRef`, `T` for
    /// `ByValue`. The first bytes of the state.
    pub env: PTy,
    /// Its output (`Future::Output`).
    pub output: PTy,
}

/// The state of one coroutine INSTANCE, as the pass laid it out: the answer of
/// `LayoutFacts::coroutine_state`, kept by whoever ran `blaze_lir::coroutine::split` on the
/// built body. Machine-independent: sizes of the target, no address.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StateShape {
    pub size: u32,
    /// The log2 of its alignment.
    pub align: u8,
    /// Where the tag is; a state never polled holds 0 there
    /// (`blaze_lir::coroutine::UNRESUMED`).
    pub tag_offset: u32,
    /// The tag's type: `i8`, `i16` or `i32`.
    pub tag_ty: Ty,
}

impl StateShape {
    /// The shape of the layout the pass returned.
    pub fn of<A: core::alloc::Allocator>(layout: &StateLayout<A>) -> StateShape {
        StateShape { size: layout.size, align: layout.align, tag_offset: layout.tag_offset, tag_ty: layout.tag_ty }
    }
}

/// The facts of coroutine `ordinal` of `body`, whose record is `checked`, written into `out`
/// (the seam's own table; `generics` the owner's parameter count, parents first): its upvars'
/// tuple and its output, `(env, output)`. The answer of `LayoutFacts::coroutine`; it holds no
/// expression id of the body.
///
/// The upvars are the rows of `Checked::captures` whose first column is the block (the table a
/// closure's captures are in). The Reference, "Async blocks": "Async blocks capture variables
/// from their environment using the same capture modes as closures."
pub fn coroutine_facts(
    body: &BodyView<'_>,
    checked: &Checked<'_>,
    ordinal: u32,
    generics: u32,
    out: &mut PortableTys<'_>,
    scratch: Alloc<'_>,
) -> Result<(PTy, PTy), BuildError> {
    let full = |_| BuildError::Malformed { what: "a coroutine's facts table is full" };
    let e = coroutines_in(body).find(|c| c.0 == ordinal).map(|c| c.1).ok_or(BuildError::Malformed { what: "a coroutine ordinal past its owner body's async blocks" })?;
    let mut params: Vec<PTy, Alloc<'_>> = Vec::with_capacity_in(generics as usize, scratch);
    for i in 0..generics {
        params.push(out.intern(PNode::Param(i)).map_err(full)?);
    }
    let from = checked.types.view();
    let t = *checked.expr_ty.get(e.ix()).ok_or(BuildError::Malformed { what: "an async block without a type" })?;
    let Ok(PNode::Future(_, output, _)) = from.node(t) else {
        return Err(BuildError::Malformed { what: "an async block whose type is not a future" });
    };
    // As written (`copy_written`): an output or an upvar of `impl Trait`, projection or
    // another block's future type is settled where an instance grounds it.
    let output = crate::copy_written(from, output, &params, body, out, scratch)?;
    let env = crate::layout::environment(body, checked, e, &params, out, scratch)?;
    Ok((env, output))
}

// THE EMIT POINTS (coroutines.md, "The builder's two emit points").
//
// Three functions per coroutine instance, as a closure has its body and its entries:
// - RESUME, instance `(AsyncItems::resume, [coroutine])`: the body, split by the pass into
//   its `poll` form; parameters `(result: ptr, state: ptr, cx: ptr)`. The pass needs the state
//   pointer as an entry parameter (`blaze_lir::coroutine::Shape::state_param`).
// - the ENTRY, instance `(Future::poll, [coroutine])`: `poll(self: Pin<&mut Self>, cx)` as
//   every `Future::poll` crosses. It adapts the public receiver and result to RESUME.
// - DROP, instance `(drop_in_place, [coroutine])`: the pass's `drop`, taking the state pointer.
// The state is made where the `async` block is evaluated (`coroutine_value`), which for an
// `async fn` is its fn's whole body (the Reference, "Async functions": "roughly equivalent to
// a function that returns an async block").

/// What the walk of a coroutine's BODY holds beside a fn's (`Walk::co`; `None` in every other
/// build): the task context every poll passes, the next suspend point, and how the body's own
/// `Poll<Output>` is made. (The state pointer, the other entry parameter, is in the upvars'
/// homes: each capture is bound at its offset from it.)
#[derive(Clone, Copy)]
pub(super) struct Co {
    /// "The current task context" of this poll, an entry parameter (never kept: each poll
    /// passes its own).
    cx: Value,
    /// The number the next `.await`'s `suspend` takes: points count up in emit order.
    points: u32,
    /// `Poll<Output>`'s walk row and its layout in `ls`.
    poll_row: u32,
    poll_id: LayoutId,
    items: crate::AsyncItems,
    /// The one slot every `.await` of the body makes its `Pin` in: written before each poll,
    /// read by nothing after the call returns.
    pin: Option<SlotId>,
}

/// A ground coroutine type read: its identity, and its upvars' tuple grounded at the owner's
/// arguments (the seam's `LayoutFacts::coroutine`), as closure.rs `Parts` is of a closure.
#[derive(Clone, Copy)]
pub(super) struct CoParts {
    pub(super) of: CoroutineOf,
    pub(super) env: PTy,
    /// Its output (`Future::Output`), ground: the world's fact grounded at the owner's arguments.
    pub(super) output: PTy,
}

/// Record type `t` as an `async` block in the checker's own form (`Future(expression, output,
/// interior)`): the block's expression. `None` for every other type, the identity
/// (`PNode::Coroutine`) included. `Walk::ground_ty` asks it: a block's type in its OWN body is
/// the one place the by-expression form is read, and there it has an ordinal.
pub(super) fn own_block(types: PTys<'_>, t: PTy) -> Option<ExprId> {
    match types.node(t).ok()? {
        PNode::Future(e, ..) => Some(e),
        _ => None,
    }
}

use super::signature_error as co_err;

impl<A: Allocator + Copy> Walk<'_, '_, '_, A> {
    /// The ground type of `async` block `e` of this body: its identity at this instance's
    /// arguments ([`identity`]). Its output is the world's fact of that identity
    /// ([`Self::coroutine_parts`]).
    pub(super) fn own_coroutine_ty(&mut self, e: ExprId) -> Result<PTy, TyErr> {
        let ordinal = coroutines_in(self.body)
            .find(|c| c.1 == e)
            .map(|c| c.0)
            .ok_or(TyErr::Malformed("a future's expression that is not an async block of its body"))?;
        let own = self.args;
        let args = self.ground.intern_list(own).map_err(|_| TyErr::Malformed("the ground table is full"))?;
        identity(self.ground, self.body.owner, ordinal, args).map_err(TyErr::Other)
    }

    /// Ground coroutine type `g` read: its identity from the type, its upvars and output from
    /// the seam, grounded at the owner's arguments (closure.rs `closure_parts` is its sibling).
    pub(super) fn coroutine_parts(&mut self, g: PTy) -> Result<CoParts, TyErr> {
        let of = coroutine_of(self.ground.view(), g).ok_or(TyErr::Malformed("a coroutine's parts asked of another type"))?;
        let facts = self.adts.facts();
        let c = facts
            .coroutine(of.owner, of.ordinal)
            .ok_or(TyErr::Unsupported("a coroutine whose facts the world did not answer (`LayoutFacts::coroutine`)"))?;
        let mut a: Vec<PTy, A> = Vec::new_in(self.alloc);
        a.extend_from_slice(self.ground.view().list(of.owner_args).map_err(|_| TyErr::Malformed("a list past the ground table"))?);
        let env = crate::instance::ground(c.types, c.env, &a, self.ground, self.scratch, facts).map_err(TyErr::Other)?;
        let output = crate::instance::ground(c.types, c.output, &a, self.ground, self.scratch, facts).map_err(TyErr::Other)?;
        Ok(CoParts { of, env, output })
    }

    /// `Walk::copy`'s step for walk row `l` when it is a coroutine's: the state's bytes moved
    /// whole, `true`; `false` for every other row. A state's row has one field, its upvars,
    /// but what the pass keeps past them and the tag are the state too. Only a state not
    /// polled yet is ever moved (after its first poll it is pinned: the Reference, "Await
    /// expressions", step 3), so the bytes hold no pointer into themselves.
    pub(super) fn copy_state(&mut self, l: u32, from: (Value, i32), to: (Value, i32)) -> bool {
        let row = &self.layouts[l as usize];
        let (ty, size, align) = (row.ty, row.size, row.align);
        if coroutine_of(self.ground.view(), ty).is_none() {
            return false;
        }
        let (src, dst) = (self.address(from), self.address(to));
        let len = self.b.iconst(Ty::I64, u128::from(size));
        let mut attrs = MemAttrs::aligned(align);
        attrs.src_align = align;
        self.b.bulk(Op::Memcpy, Flags::NONE, dst, src, len, attrs);
        true
    }

    /// The library items `.await` and a coroutine's entries need, asked at `expr`.
    fn async_items(&self, expr: Option<ExprId>) -> Result<crate::AsyncItems, BuildError> {
        self.adts.async_items().ok_or(BuildError::NoLangFn { expr, name: "the `Poll`, `Pin` and `IntoFuture` items (`Adts::async_items`)" })
    }

    /// The state of coroutine `of` as the pass laid it out, asked at `e`. A state not made yet
    /// is the missing record of its owner: that body is built and split first.
    fn state_shape(&self, e: ExprId, of: CoroutineOf) -> Result<StateShape, BuildError> {
        let view = self.ground.view();
        let args = view.list(of.owner_args).map_err(|_| malformed("a list past the ground table"))?;
        match self.adts.facts().coroutine_state(of.owner, of.ordinal, args, view) {
            Ok(Some(shape)) => Ok(shape),
            Ok(None) => Err(unsupported(e, "an async block whose state the world did not answer (`LayoutFacts::coroutine_state`)")),
            Err(missing) => Err(BuildError::MissingRecord { item: missing.item }),
        }
    }

    /// Ground `Poll<out>`.
    fn poll_ty(&mut self, items: crate::AsyncItems, out: PTy) -> Result<PTy, BuildError> {
        let full = |_| malformed("the ground table is full");
        let l = self.ground.intern_list(&[out]).map_err(full)?;
        self.ground.intern(PNode::Adt(items.poll, l)).map_err(full)
    }

    /// The walk row and `ls` layout of a `Poll` held as `repr`.
    fn poll_row(&self, repr: Repr) -> Result<(u32, LayoutId), TyErr> {
        let Repr::Agg(row) = repr else {
            return Err(TyErr::Unsupported("a `Poll` of no size (a coroutine or an `.await` whose output is uninhabited)"));
        };
        match self.layouts[row as usize].shape {
            Shape::Enum { id, .. } => Ok((row, id)),
            _ => Err(TyErr::Malformed("`Poll` not laid out as an enum")),
        }
    }

    /// How ground `Pin<P>` (`pin`, `P` a thin pointer) is held, and where its one field, the
    /// pointer, is (core's pin.rs: `pub struct Pin<Ptr> { pointer: Ptr }`).
    fn pin_repr(&mut self, pin: PTy) -> Result<(Repr, u32), TyErr> {
        const NOT_ONE: &str = "a `Pin` that is not held as a struct of one thin pointer";
        let repr = self.repr(pin)?;
        let Repr::Agg(l) = repr else {
            return Err(TyErr::Unsupported(NOT_ONE));
        };
        match self.layouts[l as usize].shape {
            Shape::Fields { first, len: 1 } => match self.layout_fields[first as usize] {
                (_, off, Repr::Ptr) => Ok((repr, off)),
                _ => Err(TyErr::Unsupported(NOT_ONE)),
            },
            _ => Err(TyErr::Unsupported(NOT_ONE)),
        }
    }

    /// `into_future` and `poll` are called as Rust fns: another ABI is refused by name.
    fn rust_abi(&self, e: ExprId, item: ItemKey) -> Result<(), BuildError> {
        match self.adts.abi(item) {
            Some(Abi::Rust) => Ok(()),
            Some(_) => Err(unsupported(e, "`.await` through an `into_future` or a `poll` that is not of the Rust ABI")),
            None => Err(BuildError::NoAbi { item }),
        }
    }

    /// Refuses `async` block `e` when its body names a binding of the enclosing body that the
    /// record has no capture row for (`caps` are `e`'s rows of `Checked::captures`). The
    /// Reference, "Async blocks": "Async blocks capture variables from their environment using
    /// the same capture modes as closures": which mode is the checker's answer, never guessed
    /// here. A binding is of the enclosing body exactly when it has a home where the block is
    /// evaluated: the block's own bindings get theirs when its body is built.
    fn uncaptured(&self, e: ExprId, caps: &[(PatId, CaptureKind)]) -> Result<(), BuildError> {
        let n = self.body.exprs.len();
        // Per expression: 0 not known yet, 1 inside the block, 2 outside it. Each expression's
        // side is found once, walking up its parents to one whose side is known.
        let mut side: Vec<u8, A> = Vec::with_capacity_in(n, self.alloc);
        side.resize(n, 0);
        let mut path: Vec<u32, A> = Vec::new_in(self.alloc);
        for (i, x) in self.body.exprs.iter().enumerate() {
            let Expr::Local(p) = *x else { continue };
            path.clear();
            let mut cur = i as u32;
            let mut found = 0u8;
            for _ in 0..=n {
                if cur == e.0 {
                    found = 1;
                    break;
                }
                if cur == NO_EXPR {
                    found = 2;
                    break;
                }
                let known = *side.get(cur as usize).ok_or(malformed("an expression's parent past the body"))?;
                if known != 0 {
                    found = known;
                    break;
                }
                path.push(cur);
                cur = self.expr_parent.get(cur as usize).ok_or(malformed("an expression without a row of parents (`scan_flags` did not run)"))?.0;
            }
            if found == 0 {
                return Err(malformed("a cycle among a body's expressions"));
            }
            for &q in path.iter() {
                side[q as usize] = found;
            }
            if found != 1 {
                continue;
            }
            let outer = self.slots.get(p.ix()).copied().flatten().is_some();
            let in_record = caps.iter().any(|c| c.0 == p)
                || self
                    .checked
                    .captures
                    .column::<0>()
                    .iter()
                    .zip(self.checked.captures.column::<1>().iter())
                    .any(|(&owner, &pat)| owner == e && pat == p);
            if outer && !in_record {
                return Err(unsupported(e, "an async block that names a binding of the enclosing body with no capture row in the record (`Checked::captures`)"));
            }
        }
        Ok(())
    }

    /// `async { .. }` where it is evaluated: the STATE, in a slot of the size and alignment the
    /// pass gave it. Its upvars are written as a closure's environment is (closure.rs
    /// `closure_value`: the address of the binding's home for a capture by reference, the
    /// value moved in for one by value), then the tag is `Unresumed`. The Reference, "Async
    /// blocks": "An async block is a variant of a block expression which evaluates to a
    /// future": nothing of its body runs here.
    pub(super) fn coroutine_value(&mut self, e: ExprId) -> Result<(), BuildError> {
        let err = |error| Self::ty_err(e, error);
        let t = self.expr_ty(e)?;
        let g = self.ground_ty(t).map_err(err)?;
        let of = coroutine_of(self.ground.view(), g).ok_or(malformed("an async block whose ground type is not a coroutine"))?;
        // Asked before the layout is: a state not made yet is its owner's missing record.
        let shape = self.state_shape(e, of)?;
        let repr = self.repr(g).map_err(err)?;
        let Repr::Agg(l) = repr else {
            return Err(malformed("a coroutine's state of no size"));
        };
        // The environment's fields (blaze-typeck env_fields.rs, the active strategy): a field
        // holding a place below its binding (`x.a`, `(*r).b`: RFC 2229, "Async blocks capture
        // variables from their environment using the same capture modes as closures") is the
        // place's address or value (walk/moves/env_places.rs `store_env_place`), as a closure's.
        let mut fields: Vec<EnvField, A> = Vec::new_in(self.alloc);
        let mut caps = Vec::new_in(self.alloc);
        for f in self.checked.env_fields_by::<crate::layout::BuildEnv>(e) {
            fields.push(f);
            caps.push((f.base, f.how));
        }
        self.uncaptured(e, &caps)?;
        let at = self.temp(repr);
        if !caps.is_empty() {
            let parts = self.coroutine_parts(g).map_err(err)?;
            let le = self.layout(parts.env).map_err(err)?;
            self.env_holds(e, parts.env, caps.len())?;
            let base = self.field_offset(l, 0)?;
            for (j, &(p, by)) in caps.iter().enumerate() {
                if !fields[j].is_whole() {
                    let dst = (at.0, at.1 + (base + self.field_offset(le, j)?) as i32);
                    self.store_env_place(e, fields[j], dst)?;
                    continue;
                }
                let (home, prepr) = self.slots.get(p.ix()).copied().flatten().ok_or(malformed("a binding captured before its binding"))?;
                let src = self.home(home);
                let dst = (at.0, at.1 + (base + self.field_offset(le, j)?) as i32);
                match by {
                    CaptureKind::ByRef | CaptureKind::ByMutRef => {
                        let a = self.address(src);
                        self.store(V::Scalar(a), Repr::Ptr, dst)?;
                    }
                    CaptureKind::ByValue => {
                        self.moved(e, p)?;
                        let v = self.load(prepr, src);
                        self.store(v, prepr, dst)?;
                    }
                }
            }
        }
        let tag = self.b.iconst(shape.tag_ty, u128::from(blaze_lir::coroutine::UNRESUMED));
        // The tag's own alignment is the pass's; a store claims none.
        self.b.store(Flags::NONE, tag, at.0, at.1 + shape.tag_offset as i32, MemAttrs::aligned(0));
        self.push_val(V::Place(at.0, at.1), None);
        Ok(())
    }

    /// The result type of this body's fn when the body is an `async fn`'s: the ground type of
    /// its root block, the coroutine, whatever the signature writes (the written type is the
    /// future's OUTPUT). `None`: not an `async fn`. The Reference, "Async functions": "An
    /// async function is roughly equivalent to a function that returns `impl Future` and with
    /// an `async move` block as its body"; every parameter is moved into that block, so a
    /// parameter with no capture row is refused by name.
    pub(super) fn async_fn_output(&mut self) -> Result<Option<PTy>, BuildError> {
        if self.body.coroutine.is_none() {
            return Ok(None);
        }
        let root = self.body.root;
        let Expr::AsyncBlock { .. } = self.expr(root)? else {
            return Err(unsupported(root, "an `async fn` whose body is not its `async move` block (the Reference, \"Async functions\")"));
        };
        // The root block's fields: each a parameter moved in whole (the place analysis files
        // an `async fn`'s root so, capture_places.rs `places_of`); one holding a place below
        // its binding is a record this build cannot read.
        let mut caps = 0usize;
        for f in self.checked.env_fields_by::<crate::layout::BuildEnv>(root) {
            if !f.is_whole() {
                return Err(malformed("an `async fn`'s root block with a field holding a place below its binding (every parameter is moved in whole)"));
            }
            caps += 1;
        }
        if caps < self.body.params.len() {
            return Err(unsupported(root, "an `async fn` parameter with no capture row in the record (every parameter is moved into the future)"));
        }
        let t = self.expr_ty(root)?;
        self.ground_ty(t).map(Some).map_err(|error| Self::ty_err(root, error))
    }

    /// What both of a coroutine's built functions start from: the block's ground type and
    /// parts, the library's items, and `Poll<Output>` as the result (`self.ret`), which
    /// crosses as every Rust result does.
    fn co_entry(&mut self, block: ExprId) -> Result<(PTy, CoParts, crate::AsyncItems, u32, LayoutId), BuildError> {
        let t = self.expr_ty(block)?;
        let g = self.ground_ty(t).map_err(co_err)?;
        let parts = self.coroutine_parts(g).map_err(co_err)?;
        let items = self.async_items(Some(block))?;
        let polled = self.poll_ty(items, parts.output)?;
        self.ret = self.repr(polled).map_err(co_err)?;
        let (row, id) = self.poll_row(self.ret).map_err(co_err)?;
        Ok((g, parts, items, row, id))
    }

    /// Bind the receiver and context and locate the storage RESUME writes its Poll into.
    fn co_params(&mut self, first: Pass) -> Result<(Value, Value, Value), BuildError> {
        let passes = [first, self.rust_pass(Repr::Ptr)?];
        let (params, rets) = self.sig_of(&passes, self.ret_pass);
        let own = self.b.sig(Conv::C, Variadic::No, &params, &rets);
        self.b.set_signature(own);
        let entry = self.create();
        self.reached[entry.index()] = true;
        self.begin(entry);
        let mut values = Vec::with_capacity_in(params.len(), self.alloc);
        for p in params.iter() {
            values.push(self.b.param(p.ty));
        }
        match (self.ret_pass, values.as_slice()) {
            (Pass::Ref, &[result, a, cx]) => {
                self.sret = Some(result);
                Ok((result, a, cx))
            }
            (Pass::Ignore | Pass::Scalar { .. } | Pass::Wide { .. }, &[a, cx]) => {
                let at = self.temp(self.ret);
                Ok((self.address(at), a, cx))
            }
            _ => Err(malformed("a coroutine entry without its receiver, context or Poll result")),
        }
    }

    /// The BODY of `async` block `block` (the view's root is the block's body), as the pass
    /// takes it: parameters `(result, state, cx)`, each capture bound at its offset in the
    /// state as an `FnOnce` closure's is in its environment (closure.rs `run_closure`: a
    /// capture by value is the body's to drop), the body walked to its value, returned as
    /// `Poll::Ready` (`coroutine_return`), then the UNRESUMED block: the drop of the upvars of
    /// a state never polled, ending in `cancelled`. Returns what the pass needs beside the LIR.
    ///
    /// The Reference, "Destructors": "The variables that a closure captures by move are
    /// dropped in an unspecified order": here the upvars' tuple's glue, in capture order.
    pub(super) fn run_coroutine(&mut self, block: ExprId) -> Result<blaze_lir::coroutine::Shape, BuildError> {
        let (_, parts, items, row, id) = self.co_entry(block)?;
        // RESUME is private to the split pass. Its state parameter is always number one.
        self.ret_pass = Pass::Ref;
        let (_, state, cx) = self.co_params(self.rust_pass(Repr::Ptr)?)?;
        self.co = Some(Co { cx, points: 0, poll_row: row, poll_id: id, items, pin: None });
        // The upvars' drop scope, the outermost (a fn's parameters' scope).
        self.scope_marks.push(0);
        // The environment's fields, as `coroutine_value` stores them. A field holding a place
        // below its binding is that place for every use in the body (walk/moves/env_places.rs
        // `bind_env_place`), held as an `FnOnce` closure's: a by-value one whose type needs drop
        // is the body's, dropped at its end, at a `return` and on the cancel path of a
        // suspension unless moved out (`Held::EnvPlace`); a whole binding is noted beside them
        // for `cover`. A drop flag (a move under a branch) is no saved local: read after a
        // suspension it is refused by name (`UNNAMED_ACROSS`), as a whole binding's field word.
        let mut fields: Vec<EnvField, A> = Vec::new_in(self.alloc);
        for f in self.checked.env_fields_by::<crate::layout::BuildEnv>(block) {
            fields.push(f);
        }
        if !fields.is_empty() {
            let le = self.layout(parts.env).map_err(co_err)?;
            self.env_holds(block, parts.env, fields.len())?;
            for j in 0..fields.len() {
                let f = fields[j];
                let at = (state, self.field_offset(le, j)? as i32);
                if !f.is_whole() {
                    self.bind_env_place(block, f, Some(at), FnTrait::FnOnce)?;
                    continue;
                }
                self.note_env_whole(f);
                let (p, by) = (f.base, f.how);
                match by {
                    CaptureKind::ByRef | CaptureKind::ByMutRef => {
                        let V::Scalar(ptr) = self.load(Repr::Ptr, at) else {
                            return Err(malformed("a capture's pointer not a scalar"));
                        };
                        self.bind_capture_at(p, (ptr, 0))?;
                    }
                    CaptureKind::ByValue => {
                        // The variable alone (not its subpattern's bindings: each is its own
                        // capture).
                        self.bind_capture_at(p, at)?;
                        self.own(p, true)?;
                    }
                }
            }
            self.settle_env_places(block)?;
        }
        self.walk_root()?;
        // No edge of the body leads here: the state's drop does, when the tag is `Unresumed`.
        let unresumed = self.create();
        self.reached[unresumed.index()] = true;
        self.begin(unresumed);
        let env_repr = self.repr(parts.env).map_err(co_err)?;
        let (prefix_size, prefix_align) = self.size_align(env_repr);
        if self.needs_drop(parts.env).map_err(co_err)? {
            self.drop_at(parts.env, (state, 0))?;
        }
        self.b.cancelled();
        if let Some(error) = self.b.error() {
            return Err(BuildError::Lir(error.clone()));
        }
        Ok(blaze_lir::coroutine::Shape { state_param: 1, prefix_size, prefix_align, unresumed })
    }

    /// The ENTRY of `async` block `block`: `Future::poll` at `[coroutine]`, `(result, self:
    /// Pin<&mut Self>, cx)`. Core's future/future.rs: `fn poll(self: Pin<&mut Self>, cx: &mut
    /// Context<'_>) -> Poll<Self::Output>`. It reads the state pointer out of the `Pin` and
    /// calls RESUME with the same result pointer and context.
    ///
    /// The `Pin`'s field is found on `Pin<&mut ()>`: a reference to a sized type is one thin
    /// pointer whatever its referent (the Reference, "Pointer types"), and the state's own
    /// layout is not a record until this build returns.
    pub(super) fn run_poll_entry(&mut self, block: ExprId) -> Result<(), BuildError> {
        let full = |_| malformed("the ground table is full");
        let (g, _, items, _, _) = self.co_entry(block)?;
        let none = self.ground.intern_list(&[]).map_err(full)?;
        let unit = self.ground.intern(PNode::Tuple(none)).map_err(full)?;
        let r = self.ground.intern(PNode::Ref(Mutbl::Mut, unit)).map_err(full)?;
        let l = self.ground.intern_list(&[r]).map_err(full)?;
        let pin_ty = self.ground.intern(PNode::Adt(items.pin, l)).map_err(full)?;
        let (prepr, pin_off) = self.pin_repr(pin_ty).map_err(co_err)?;
        let pass = self.rust_pass(prepr)?;
        self.ret_pass = self.rust_pass(self.ret)?;
        let (result, pin, cx) = self.co_params(pass)?;
        let state = match pass {
            Pass::Scalar { ty: Ty::PTR, agg: true, .. } if pin_off == 0 => pin,
            Pass::Ref => self.b.load(Ty::PTR, Flags::NONE, pin, pin_off as i32, MemAttrs::aligned(3)),
            _ => return Err(malformed("a poll receiver without its state pointer")),
        };
        let args = self.ground.intern_list(&[g]).map_err(full)?;
        let place = self.ref_place(Instance { item: items.resume, args });
        let reference = self.b.reference(RefKind::Func, place);
        let resume = self.b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR); 3], &[]);
        self.b.call(Flags::NONE, reference, resume, &[result, state, cx]);
        let mut returned = Vec::with_capacity_in(2, self.alloc);
        if self.ret_pass != Pass::Ref {
            self.pass_arg(V::Place(result, 0), self.ret, self.ret_pass, &mut returned)?;
        }
        self.b.ret(&returned);
        if let Some(error) = self.b.error() {
            return Err(BuildError::Lir(error.clone()));
        }
        Ok(())
    }

    /// A coroutine body's `return v` and its tail value: `Poll::Ready(v)` through the result
    /// pointer by the walk's own enum construction, then every open scope dropped, the upvars
    /// last, as a fn's `return` (`ret_value`). The pass stores `Returned` in the tag.
    pub(super) fn coroutine_return(&mut self, v: V) -> Result<(), BuildError> {
        let co = self.co.ok_or(malformed("a coroutine's return outside a coroutine's body"))?;
        let to = (self.sret.ok_or(malformed("a coroutine's `Poll` returned without its pointer"))?, 0);
        let (ff, fl, id) = self.variant_rows(co.poll_row, co.items.ready)?;
        match fl {
            0 => {}
            1 => {
                let (_, off, frepr) = self.layout_fields[ff as usize];
                self.store(v, frepr, (to.0, off as i32))?;
            }
            _ => return Err(BuildError::UnsupportedSig { what: "a `Poll` whose `Ready` variant is not one field" }),
        }
        self.write_tag(id, co.items.ready, to);
        self.drop_from(0)?;
        self.b.ret(&[]);
        Ok(())
    }

    /// `operand.await`: the operand first, then the poll loop (`await_poll`). Outside a
    /// coroutine's body it is refused by name: an async closure's body and a generator's are
    /// not built.
    pub(super) fn await_operand(&mut self, e: ExprId, operand: ExprId) -> Result<(), BuildError> {
        if self.co.is_none() {
            return Err(unsupported(e, "`.await` outside an async block's body (an async closure's body is not built)"));
        }
        self.work.push(Task::Await(e));
        self.work.push(Task::Eval(operand));
        Ok(())
    }

    /// `operand.await`, the operand's value on top. The Reference, "Await expressions":
    ///
    /// 1. "Create a future by calling `IntoFuture::into_future` on the future operand": the
    ///    callee the check recorded for `e`.
    /// 2. "Evaluate the future to a future `tmp`": a slot of its own; its address is taken, so
    ///    the pass keeps it in the state, where it never moves.
    /// 3. "Pin `tmp` using `Pin::new_unchecked`": a `Pin` whose one field is `tmp`'s address.
    /// 4. "This pinned future is then polled by calling the `Future::poll` method and passing
    ///    it the current task context": the instance at the held type, `cx` this poll's.
    /// 5. "If the call to `poll` returns `Poll::Pending`, then the future returns
    ///    `Poll::Pending`, suspending its state so that, when the surrounding async context is
    ///    re-polled, execution returns to step 3": `Pending` is written through the result
    ///    pointer and the block ends in `suspend k, L, C`: `L` the loop's head, `C` the cancel
    ///    block, which drops `tmp` and then what a `return` here would drop (`drop_from(0)`,
    ///    the upvars last) and ends in `cancelled`.
    /// 6. "Otherwise the call to `poll` must have returned `Poll::Ready`, in which case the
    ///    value contained in the `Poll::Ready` variant is used as the result of the `await`
    ///    expression itself": read from the returned `Poll`; `tmp` is dropped.
    ///
    /// A value in flight in an enclosing expression (an operand already evaluated) is dropped
    /// by neither path, as the walk's `return` does not drop one.
    pub(super) fn await_poll(&mut self, e: ExprId) -> Result<(), BuildError> {
        let Expr::Await(operand) = self.expr(e)? else {
            return Err(malformed("an await's continuation of another expression"));
        };
        let mut co = self.co.ok_or(malformed("an await's continuation outside a coroutine's body"))?;
        let items = co.items;
        let poll_item = self.adts.future_poll().ok_or(BuildError::NoLangFn { expr: Some(e), name: "poll" })?;
        let full = |_| malformed("the ground table is full");
        let err = |error| Self::ty_err(e, error);

        // Step 1: `into_future(operand)`. An aggregate operand that is a place of its own (a
        // local) is copied first: the callee keeps the copy it is passed.
        let t = self.adjusted_ty(operand)?;
        let gt = self.ground_ty(t).map_err(err)?;
        let trepr = self.repr(gt).map_err(err)?;
        let v = self.pop()?;
        // `moved`: the operand's own slot when the operand is a whole one (a copy, or a fresh
        // temporary): `into_future` takes it by value, so its storage ends after the call and
        // the slot, whose address the call took, is not kept across the suspend.
        let (v, moved) = match (v, trepr) {
            (V::Place(..), Repr::Agg(_)) if !self.is_fresh(operand)? => {
                let at = self.temp(trepr);
                self.store(v, trepr, at)?;
                (V::Place(at.0, at.1), self.whole_slot(at))
            }
            (V::Place(p, off), Repr::Agg(_)) => (v, self.whole_slot((p, off))),
            _ => (v, None),
        };
        if !self.checked.callees.column::<0>().iter().any(|&x| x == e) {
            return Err(unsupported(e, "`.await` with no `IntoFuture::into_future` callee in the record"));
        }
        let into = self.method_callee(e)?;
        self.rust_abi(e, into.item)?;
        self.rust_abi(e, poll_item)?;
        // `tmp`'s type, `<T as IntoFuture>::IntoFuture`, is the check's own answer (its
        // `awaited` row, as the flow lowering reads it: rule 74), grounded at this instance: an
        // opaque future (an `async fn`'s) is then its hidden coroutine, which no solver is asked
        // to prove a `Future` (a ground coroutine is a body's type, not one a record imports).
        let held_record = self.checked.awaited_of(e).ok_or_else(|| unsupported(e, "`.await` whose held future the check did not record (`Checked::awaited`)"))?;
        let held = self.ground_ty(held_record).map_err(err)?;
        let hrepr = self.repr(held).map_err(err)?;
        let tpass = self.rust_pass(trepr)?;
        let hpass = self.rust_pass(hrepr)?;
        self.push_val(v, None);
        let place = self.ref_place(into);
        let reference = self.b.reference(RefKind::Func, place);
        self.emit_call(Callee::Direct(reference), &[trepr], &[tpass], hrepr, hpass)?;
        let hv = self.pop()?;
        if let Some(slot) = moved {
            self.b.slot_dead(slot);
        }

        // Step 2: `tmp`.
        let at = match (hv, hrepr) {
            (_, Repr::None) => return Err(unsupported(e, "`.await` of a zero-sized future")),
            (V::Place(p, off), Repr::Agg(_)) => (p, off),
            (hv, hrepr) => {
                let at = self.temp(hrepr);
                self.store(hv, hrepr, at)?;
                at
            }
        };
        // `tmp`'s storage ends on the Ready edge (ASYNC.md D-A6), so the state's bytes for it
        // are free for what a later await keeps.
        let fut_slot = self.await_slot(e, at, "`.await` of a future not in a slot of its own: its storage cannot end on the Ready edge")?;
        self.name_slot(fut_slot, Identity::Temp(e, Role::Awaitee));

        // Step 3's type: `Pin<&mut Held>`; the loop's result type: `Poll<Output>`.
        let fut_ref = self.ground.intern(PNode::Ref(Mutbl::Mut, held)).map_err(full)?;
        let pin_args = self.ground.intern_list(&[fut_ref]).map_err(full)?;
        let pin_ty = self.ground.intern(PNode::Adt(items.pin, pin_args)).map_err(full)?;
        let (prepr, pin_off) = self.pin_repr(pin_ty).map_err(err)?;
        let pin_slot = match co.pin {
            Some(slot) => slot,
            None => {
                let (size, align) = self.size_align(prepr);
                let slot = self.b.slot(size, align, SlotKind::Temp);
                co.pin = Some(slot);
                slot
            }
        };
        let out_t = self.expr_ty(e)?;
        let out = self.ground_ty(out_t).map_err(err)?;
        let orepr = self.repr(out).map_err(err)?;
        let polled = self.poll_ty(items, out)?;
        let rrepr = self.repr(polled).map_err(err)?;
        let (row, id) = self.poll_row(rrepr).map_err(err)?;
        let rpass = self.rust_pass(rrepr)?;
        let ppass = self.rust_pass(prepr)?;
        let cpass = self.rust_pass(Repr::Ptr)?;
        let args = self.ground.intern_list(&[held]).map_err(full)?;
        let poll = Instance { item: poll_item, args };
        let held_drops = self.needs_drop(held).map_err(err)?;

        // Every value waiting on the stack (an operand evaluated before this `.await`) is
        // written into the slot of the flow local it is, before the loop, and read back on the
        // Ready edge: the walk invents no slot across a suspend (walk/await_spill.rs).
        let spilled = self.spill_waiting(e)?;
        let planted = self.plant_before();

        // Steps 3 and 4, the loop's head: where a poll after a suspend goes on.
        let head = self.create();
        self.jump(head, &[]);
        self.begin(head);
        let fut = self.address(at);
        let pa = self.b.slot_addr(pin_slot);
        self.store(V::Scalar(fut), Repr::Ptr, (pa, pin_off as i32))?;
        self.push_val(V::Place(pa, 0), None);
        self.push_val(V::Scalar(co.cx), None);
        let place = self.ref_place(poll);
        let reference = self.b.reference(RefKind::Func, place);
        self.emit_call(Callee::Direct(reference), &[prepr, Repr::Ptr], &[ppass, cpass], rrepr, rpass)?;
        let r = self.pop()?;
        self.b.slot_dead(pin_slot);
        let rat = self.place(r, rrepr)?;
        let poll_slot = self.await_slot(e, rat, "`.await` whose `Poll` is not in a slot of its own: its storage cannot end on the Ready edge")?;
        let d = self.read_discriminant(id, rat)?;
        let ready_case = self.discriminant_case(id, items.ready)?;
        let ready = self.create();
        let pending = self.create();
        self.switch(d, pending, &[(ready_case, ready)]);

        // Step 5: this poll returns `Poll::Pending`; the next goes on at the head.
        self.begin(pending);
        self.b.slot_dead(poll_slot);
        let to = self.sret.ok_or(malformed("a coroutine's `Poll` returned without its pointer"))?;
        self.write_tag(Some(co.poll_id), items.pending, (to, 0));
        let cancel = self.create();
        let point = co.points;
        co.points = point.checked_add(1).ok_or(malformed("a coroutine of more suspend points than a `u32` counts"))?;
        self.co = Some(co);
        self.reach(head);
        self.reach(cancel);
        self.b.suspend(point, head, cancel, &[]);

        // The cancel block: the state dropped while suspended here. `tmp` is the innermost
        // temporary, so it goes first; then every open scope, as a `return`'s exit.
        self.begin(cancel);
        if held_drops {
            self.drop_at(held, at)?;
        }
        self.drop_from(0)?;
        self.b.cancelled();

        // Step 6: the value, then `tmp` is dropped.
        self.begin(ready);
        self.reload_waiting(&spilled);
        self.plant_after(planted);
        let (ff, fl, _) = self.variant_rows(row, items.ready)?;
        let value = match (fl, orepr) {
            (_, Repr::None) => V::None,
            (1, _) => {
                let (_, off, frepr) = self.layout_fields[ff as usize];
                if frepr != orepr {
                    return Err(malformed("`Poll::Ready`'s field not held as the awaited output"));
                }
                self.load(frepr, (rat.0, rat.1 + off as i32))
            }
            _ => return Err(unsupported(e, "a `Poll` whose `Ready` variant is not one field")),
        };
        // The payload is moved out of the `Poll` before its storage ends: an aggregate (held
        // as a place in the `Poll` slot) is copied to a temporary of its own; a scalar is
        // already loaded.
        let value = match value {
            V::Place(..) => {
                let to = self.temp(orepr);
                self.name_at(to, Identity::Temp(e, Role::Result));
                self.store(value, orepr, to)?;
                V::Place(to.0, to.1)
            }
            other => other,
        };
        self.b.slot_dead(poll_slot);
        if held_drops {
            self.drop_at(held, at)?;
        }
        self.b.slot_dead(fut_slot);
        self.push_val(value, None);
        Ok(())
    }

    /// The slot whose whole storage the place `at` is: an `.await`'s future or its `Poll`,
    /// each a fresh temporary of this walk (`temp`, or a call's result slot), whose
    /// `slot_dead` the Ready edge writes. Refused by name when it is not one: a `slot_dead`
    /// of a slot that also holds something else would end that too.
    fn await_slot(&self, e: ExprId, at: (Value, i32), refused: &'static str) -> Result<SlotId, BuildError> {
        self.whole_slot(at).ok_or_else(|| unsupported(e, refused))
    }

    /// The slot whose whole storage the place `at` is, if it is one (its `slot_addr`, offset 0).
    fn whole_slot(&self, at: (Value, i32)) -> Option<SlotId> {
        let view = self.b.func().view();
        let ix = at.0.index();
        (at.1 == 0 && ix < view.len() && view.op(ix) == Op::SlotAddr).then(|| SlotId(view.data[ix]))
    }
}

/// Builds coroutine `ordinal` of `body` (the owner's, checked once as `checked`) at the
/// owner's instance `subst` names: its body walked and split by the pass into RESUME and DROP,
/// its ENTRY, and the state's shape. With `plan` (the run path, always: coroutine_state.rs
/// `lay_state` from the flow's saved locals' types, ASYNC.md P8) the walk names each slot that
/// is a saved flow local (walk/state_names.rs), the split places those at their offsets and
/// refuses any other it keeps across a suspend (`blaze_lir::coroutine::split_in`), and the
/// shape and niche are the laid ones; with `None` (this crate's unit tests, which have no
/// recipe) the split lays the state itself. The split functions name their callees by the
/// body's reference places, so each carries the body's tables.
pub(crate) fn build<A: Allocator + Copy>(
    body: &BodyView<'_>,
    checked: &Checked<'_>,
    ordinal: u32,
    subst: &mut Subst<'_, '_>,
    adts: &dyn Adts,
    plan: Option<crate::coroutine_state::StatePlan<'_>>,
    alloc: A,
) -> Result<crate::BuiltCoroutine<A>, BuildError> {
    let block = coroutines_in(body).find(|c| c.0 == ordinal).map(|c| c.1).ok_or(malformed("a coroutine ordinal past its owner body's async blocks"))?;
    let Some(Expr::AsyncBlock { body: root, .. }) = body.exprs.get(block.ix()).copied() else {
        return Err(malformed("a coroutine body asked of an expression that is not an async block"));
    };
    // The block's body is the root; it has no parameters of its own (its upvars are bound
    // from the state).
    let view = BodyView { params: &[], root, ..*body };
    let lir = |error| match error {
        blaze_lir::BuildError::Verify(v) => BuildError::Verify(v),
        other => BuildError::Lir(other),
    };
    let (func, shape, refs, statics, thread_locals, vtables, names, copy_insts) = {
        let mut w = Walk::new(&view, checked, &mut *subst, adts, alloc);
        w.names = plan.map(|p| super::state_names::Names::new(p.names, p.plant, root, alloc));
        w.scan_flags()?;
        let shape = w.run_coroutine(block)?;
        // `poll` on a finished state: the library's two panics, when it declares them.
        let panics = w.coroutine_panics()?;
        let Walk { b, refs, statics, thread_locals, vtables, names, copy_insts, .. } = w;
        (b.finish().map_err(lir)?, (shape, panics), refs, statics, thread_locals, vtables, names, copy_insts)
    };
    let (shape, panics) = shape;
    let split = match (plan, &names) {
        (Some(p), Some(n)) => {
            let l = p.laid;
            let region = blaze_lir::coroutine::Region {
                start: l.region.start,
                end: l.region.end,
                tag_offset: l.shape.tag_offset,
                tag_ty: l.shape.tag_ty,
                size: l.shape.size,
                align: l.shape.align,
            };
            let slot_names = n.slot_names(alloc);
            blaze_lir::coroutine::split_in(func.view(), shape, panics, region, &slot_names, alloc).map_err(|error| n.split_err(error))?
        }
        (None, None) => blaze_lir::coroutine::split_with(func.view(), shape, panics, alloc).map_err(BuildError::Coroutine)?,
        _ => return Err(malformed("a coroutine's state plan and its walk's names out of step")),
    };
    let (state, niche) = match plan {
        Some(p) => (p.laid.shape, p.laid.niche),
        None => (StateShape::of(&split.layout), crate::niche_of(&split.layout)),
    };
    let poll = {
        let mut w = Walk::new(&view, checked, &mut *subst, adts, alloc);
        w.run_poll_entry(block)?;
        w.into_built()?
    };
    // The split's functions are blaze-lir's rewrite of the walk's: its `unreachable`s are not
    // the walk's one for one, so they carry no trap table (a fault there names no construct).
    // The split's two functions verified here, once (D163): blaze-lir's split checks its output
    // in debug builds only, and the emitter takes nothing unproven.
    fn copy_slice<T: Copy, A: Allocator>(slice: &[T], alloc: A) -> Vec<T, A> {
        let mut out = Vec::with_capacity_in(slice.len(), alloc);
        out.extend_from_slice(slice);
        out
    }
    let resume = Built {
        func: blaze_lir::VerifiedFunc::new(split.poll).map_err(BuildError::Verify)?,
        refs: copy_slice(&refs, alloc),
        statics: copy_slice(&statics, alloc),
        thread_locals: copy_slice(&thread_locals, alloc),
        vtables: copy_slice(&vtables, alloc),
        traps: Vec::new_in(alloc),
        copy_insts,
    };
    let drop = Built { func: blaze_lir::VerifiedFunc::new(split.drop).map_err(BuildError::Verify)?, refs, statics, thread_locals, vtables, traps: Vec::new_in(alloc), copy_insts };
    Ok(crate::BuiltCoroutine { poll, resume, drop, state, niche })
}
