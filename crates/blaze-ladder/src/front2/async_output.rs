//! An `async fn`'s body's output type, read from the fn's HEAD, never lowered again from the
//! body's view.
//!
//! The rule. The Reference, "Async functions": "async fn example(x: &str) -> usize { x.len() }"
//! is "roughly equivalent to" "fn example<'a>(x: &'a str) -> impl Future<Output = usize> + 'a",
//! and the body is an async block whose value is the written return type. So the written return
//! type `T` is in RETURN POSITION: an `impl Trait` inside it (`-> Result<impl Clone, ()>`) is one
//! of the fn's opaques (the Reference, "Impl trait", "Abstract return types"), the one `fn_head`
//! numbered after the future's 0 and filed as the `Output` of opaque 0's `Future` bound
//! (heads.rs, `fn_head`, "An `async fn f(..) -> T` is `fn f(..) -> impl Future<Output = T>`").
//!
//! `to_typeck` reads the async body's coroutine output by lowering the view's written return
//! type through the body's [`blaze_lower::typeck::Types`]; the body's type env holds no return
//! opaques (a body's own `impl Trait` is in no return position), so `Result<impl Clone, ()>` was
//! refused "an `impl Trait` type outside a fn's parameters and return type". The head is the one
//! home where a return type is lowered with its opaques (and with its type macros expanded:
//! front2/type_expand.rs): the body takes `T` from there, the same `PTy` the callers see through
//! `Future::Output`.

use blaze_lower::machine::sig;
use blaze_lower::{View, form as lf, head};
use blaze_typeck::{ItemKey, Name, PClause, PTy};

use super::heads::Store;
use super::keys::opaque_key;

/// The written return type of the `async fn` whose body `view` is, as its id in `view`; `None`
/// for a body of no `async fn`, or one with no return type written (`to_typeck` makes its `()`
/// itself: no written type is read).
pub(super) fn written(view: &View<'_>) -> Option<lf::TypeId> {
    if view.head[head::FLAGS] & sig::ASYNC == 0 {
        return None;
    }
    view.head_opt(head::TY).map(lf::TypeId)
}

/// The `T` of `owner`'s `impl Future<Output = T>`: the `Output` projection among the bounds of
/// `owner`'s opaque 0, as `fn_head` filed them (and a head record restores them). `None` while
/// `owner`'s head is not made, or where its opaque 0 is not an `async fn`'s future (no
/// `Output` projection in its bounds): the caller refuses by the fn's key.
pub(super) fn of(store: &Store<'_>, owner: ItemKey) -> Option<PTy> {
    let k = opaque_key(owner, 0);
    if store.opaques.get(&k) != Some(&(owner, 0)) {
        return None;
    }
    let output = Name::of("Output");
    store.preds.get(&k)?.clauses.iter().find_map(|c| match *c {
        PClause::Projection { name, term, .. } if name == output => Some(term),
        _ => None,
    })
}
