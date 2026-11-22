//! The planted bugs of the controls (rule 43, testing skill): each turns one rule wrong while
//! set on this thread, so a control can watch its oracle catch it.

use std::cell::Cell;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Plant {
    None,
    /// A coinductive cycle's initial result taken as an inductive one (no solution).
    CoinductiveAsInductive,
    /// The prefilter treats an impl's wildcard byte as a mismatch.
    PrefilterDropsWildcard,
    /// A trait object is `Sized` (builtin.rs `step_sizedness`).
    ObjectSized,
    /// A trait object that lists any auto trait has every one (builtin.rs object candidates).
    ObjectAnyAuto,
    /// An object's projection is its binding's whatever the binding's term (alias.rs `step_object`).
    ObjectProjIgnoresBinding,
    /// A trait whose dyn compatibility the world did not decide is taken as not compatible
    /// (builtin.rs `dyn_compatible`): the state before the gap stopped the goal.
    DynUndecidedAsNo,
    /// The last argument of a declaration is lost before its type or bound is instantiated.
    MissingHeadArgument,
    /// A portable reachability walk drops every child instead of visiting it.
    ParamsReachedDropsChild,
}

std::thread_local! {
    static PLANT: Cell<Plant> = const { Cell::new(Plant::None) };
}

pub fn on(p: Plant) -> bool {
    PLANT.with(|c| c.get() == p)
}

pub fn head_args(mut args: crate::term::List) -> crate::term::List {
    if on(Plant::MissingHeadArgument) {
        args.len = args.len.saturating_sub(1);
    }
    args
}

/// Runs `f` with `p` planted.
pub fn with<R>(p: Plant, f: impl FnOnce() -> R) -> R {
    let old = PLANT.with(|c| c.replace(p));
    let r = f();
    PLANT.with(|c| c.set(old));
    r
}
