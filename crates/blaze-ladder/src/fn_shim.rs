//! A FUNCTION ITEM or a FUNCTION POINTER used as an `Fn*` value (`apply(double, 3)` with
//! `F: Fn(u8) -> u8`): the instance of `Fn::call` / `FnMut::call_mut` / `FnOnce::call_once` at
//! `[callee, (A..)]` is a shim the builder makes ([`blaze_build::build_fn_shim`]), never a
//! selected impl (the Reference, "Function item types": "All function items implement `Fn`,
//! `FnMut`, `FnOnce`, ..."; core's function.rs: `Fn` "is implemented automatically by closures
//! ... as well as (safe) function pointers").
//!
//! Tabled (run.rs's instance loop indexes every instance by a tabled fn): a fn item's shim at
//! the callee's own index, whose signature is the shim's `sig`; a fn pointer's at the index of
//! the fn whose body called it (no fn stands for a pointer; the builder reads no body for it,
//! and the key below tells it apart from that fn's own instances). A self of any other type is
//! no shim: it stays the typed refusal it is.

use std::alloc::Global;
use std::vec::Vec;

use blaze_build::{Adts, BuildError, FnTrait, Instance};
use blaze_typeck::{ItemKey, List, PNode, PTy, PortableTys};

use crate::seam::FrontEnd;

/// An `Fn*` method instance whose self is a fn item or a fn pointer.
#[derive(Clone, Copy, Debug)]
pub(crate) struct FnShim {
    pub which: FnTrait,
    /// The fn item or fn pointer type.
    pub callee: PTy,
    /// The argument tuple.
    pub tuple: PTy,
    /// For a fn item: the item and its generic arguments (`FnDef(item, args)`); `None` for a
    /// fn pointer.
    pub item: Option<(ItemKey, List)>,
}

const METHODS: [FnTrait; 3] = [FnTrait::Fn, FnTrait::FnMut, FnTrait::FnOnce];

/// `called` as a fn item's or fn pointer's `Fn*` method instance; `None` for any other call.
pub(crate) fn fn_shim_of(adts: &dyn Adts, called: Instance, ground: &PortableTys<'_>) -> Option<FnShim> {
    let view = ground.view();
    let &[callee, tuple] = view.list(called.args).ok()? else { return None };
    let which = METHODS.into_iter().find(|&w| adts.fn_trait_method(w) == Some(called.item))?;
    let item = match view.node(callee).ok()? {
        PNode::FnDef(item, args) => Some((item, args)),
        PNode::FnPtr(..) => None,
        _ => return None,
    };
    Some(FnShim { which, callee, tuple, item })
}

/// The tabled index of `called` as a shim (module doc): a fn item's own index (`None` when the
/// program tables no such fn: the call stays refused by name), a fn pointer's `caller`.
pub(crate) fn fn_shim_owner<P: FrontEnd>(program: &P, adts: &dyn Adts, called: Instance, ground: &PortableTys<'_>, caller: usize) -> Option<usize> {
    let shim = fn_shim_of(adts, called, ground)?;
    match shim.item {
        Some((item, _)) => program.fn_index(item),
        None => Some(caller),
    }
}

/// Whether `instance`'s first argument (an `Fn*` method's self) is a fn item or a fn pointer.
pub(crate) fn self_is_fn(instance: Instance, ground: &PortableTys<'_>) -> bool {
    let view = ground.view();
    let Some(&first) = view.list(instance.args).ok().and_then(|l| l.first()) else { return false };
    matches!(view.node(first), Ok(PNode::FnDef(..) | PNode::FnPtr(..)))
}

/// The list a shim instance is keyed by: the method as a `FnDef`, then the instance's own
/// arguments (the callee type and the tuple). Never equal to the tabled fn's own list: its
/// first is the `Fn*` method's `FnDef`, and the count is hashed.
pub(crate) fn fn_shim_key_args(instance: Instance, ground: &mut PortableTys<'_>) -> Result<List, BuildError> {
    let malformed = |_| BuildError::Malformed { what: "a fn shim instance's arguments past the ground table" };
    let full = |_| BuildError::Malformed { what: "the ground table is full" };
    let mut all: Vec<PTy, Global> = Vec::new_in(Global);
    all.push(ground.intern(PNode::FnDef(instance.item, List::EMPTY)).map_err(full)?);
    all.extend_from_slice(ground.view().list(instance.args).map_err(malformed)?);
    ground.intern_list(&all).map_err(full)
}
