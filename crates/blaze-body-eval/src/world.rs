//! The facts seam: what the evaluator asks of the world beyond the old evaluator's
//! [`ConstWorld`]. Calls ask the SAME questions the old evaluator asks
//! ([`ConstWorld::resolve`] for the instance a call at ground arguments reaches,
//! [`ConstWorld::fn_kind`] for what kind of fn it is), so the two agree call by call until the
//! old walk is deleted (P7). Only two questions are new: the verified body of an item, and the
//! drop glue of a ground type.

use blaze_body::Verified;
use blaze_consteval::{ConstWorld, Refusal};
use blaze_own::Alloc;
use blaze_typeck::{Fetch, ItemKey, List, PTy, PTys, PortableTys};

/// An item's body as the one lowering wrote it, verified, with the type table its `PTy`s are
/// handles into (the checked record's table: a body does not carry its types).
#[derive(Clone, Copy)]
pub struct Bodied<'b> {
    pub item: ItemKey,
    pub body: Verified<'b>,
    pub types: PTys<'b>,
}

/// What dropping a value of a ground type runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropGlue {
    /// The type needs no drop: nothing runs.
    Nothing,
    /// The glue body `item` at `args` (a list of the ground table), called with one argument,
    /// `&mut T` to the dropped place, returning `()`.
    Call { item: ItemKey, args: List },
}

/// The world the body evaluator runs in.
pub trait BodyWorld: ConstWorld {
    /// The verified body of `item`; `Ok(None)`: the item has no body (it is no fn with code);
    /// `Err(Missing)`: not lowered yet (the run stops with `Answer::Need` of kind `Body`).
    fn verified(&self, item: ItemKey) -> Fetch<Option<Bodied<'_>>>;

    /// The drop glue of `ty`, a type of `ground`. A type the world cannot judge is a refusal
    /// naming it (`Refusal::NeedsDrop`), never `Nothing` (rule 3).
    fn drop_glue(&self, ty: PTy, ground: &mut PortableTys<'_>, scratch: Alloc<'_>) -> Result<DropGlue, Refusal<'static>>;
}
