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
//! order ([`coroutines_in`]), and the owner's generic arguments. The checker's type is still
//! `PNode::Future(expression, output, interior)`, by an expression id local to one body, which
//! no key and no ground type may hold. Until blaze-typeck spells the identity itself (wanted:
//! `PNode::Coroutine { owner, ordinal, args }`, made where `open_future` makes `Future`), the
//! GROUND form is the same node read as `Future(ExprId(ordinal), output, FnDef(owner,
//! owner_args))`: [`identity`] is the one place that makes it, [`coroutine_of`] the one that
//! reads it. A future still named by its expression is refused by name wherever it is grounded
//! (instance.rs `ground` through [`ground_node`]), never guessed.
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
use blaze_typeck::{BodyView, CaptureKind, Checked, Expr, ExprId, ItemKey, List, Mutbl, PNode, PTy, PTys, PortableTys};

use crate::BuildError;
use crate::layout::NoFacts;

/// The async blocks of `body` with their ORDINALS: a block's position among its owner body's
/// `async` blocks in expression order. The one place an ordinal is derived (closure.rs
/// `closures_in` is its sibling); the expression id never leaves one body's walk.
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
    /// Its output (`Future::Output`), ground.
    pub output: PTy,
}

/// The ground type of coroutine `ordinal` of `owner` at `owner_args`, with `output`, interned
/// in `out` (the module doc's ground form). The one constructor: the walk makes its own
/// body's blocks with it, and a seam that answers a hidden type (an `async fn`'s) makes that
/// with it over the owner's parameters.
pub fn identity(out: &mut PortableTys<'_>, owner: ItemKey, ordinal: u32, owner_args: List, output: PTy) -> Result<PTy, BuildError> {
    let full = |_| BuildError::Malformed { what: "the ground table is full" };
    let named = out.intern(PNode::FnDef(owner, owner_args)).map_err(full)?;
    out.intern(PNode::Future(ExprId(ordinal), output, named)).map_err(full)
}

/// Ground type `ty` as a coroutine, or `None`: another type, or a future still named by its
/// expression (the checker's own form, which has no identity outside its body).
pub fn coroutine_of(ground: PTys<'_>, ty: PTy) -> Option<CoroutineOf> {
    let PNode::Future(ordinal, output, named) = ground.node(ty).ok()? else {
        return None;
    };
    match ground.node(named).ok()? {
        PNode::FnDef(owner, owner_args) => Some(CoroutineOf { owner, owner_args, ordinal: ordinal.0, output }),
        _ => None,
    }
}

/// instance.rs `ground`'s arm for a `Future` node of table `from` whose children are already
/// mapped (`output` and `named`): the identity form goes on as it is, its owner's arguments
/// substituted with the rest; the checker's by-expression form is refused by name.
pub(crate) fn ground_node(from: PTys<'_>, ordinal: ExprId, interior: PTy, output: PTy, named: PTy) -> Result<PNode, BuildError> {
    match from.node(interior) {
        Ok(PNode::FnDef(..)) => Ok(PNode::Future(ordinal, output, named)),
        Ok(_) => Err(BuildError::UnsupportedSig {
            what: "an async block's future named by its expression (only its identity form, coroutine.rs, crosses a body)",
        }),
        Err(_) => Err(BuildError::Malformed { what: "a future's third part past its table" }),
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
    let output = crate::ground(from, output, &params, out, scratch, &NoFacts)?;
    let mut fields: Vec<PTy, Alloc<'_>> = Vec::new_in(scratch);
    let caps = &checked.captures;
    for c in 0..caps.len() {
        if caps.column::<0>()[c] != e {
            continue;
        }
        let (p, by) = (caps.column::<1>()[c], caps.column::<2>()[c]);
        let lt = *checked.local_ty.get(p.ix()).ok_or(BuildError::Malformed { what: "a captured binding without a type" })?;
        let t = crate::ground(from, lt, &params, out, scratch, &NoFacts)?;
        fields.push(match by {
            CaptureKind::ByRef => out.intern(PNode::Ref(Mutbl::Not, t)).map_err(full)?,
            CaptureKind::ByMutRef => out.intern(PNode::Ref(Mutbl::Mut, t)).map_err(full)?,
            CaptureKind::ByValue => t,
        });
    }
    let l = out.intern_list(&fields).map_err(full)?;
    let env = out.intern(PNode::Tuple(l)).map_err(full)?;
    Ok((env, output))
}
