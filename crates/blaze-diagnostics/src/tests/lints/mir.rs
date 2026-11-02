//! What rustc's checks of bodies after type checking give: borrow checking, const checking,
//! match checking, unsafety checking, and the scope checks of `await`, `break`, `return`,
//! labels and `yield`.

mod borrowck;
mod consts;
mod matches;
mod scopes;
mod unsafety;
