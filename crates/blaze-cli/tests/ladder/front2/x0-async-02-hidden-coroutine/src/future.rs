//! The futures part of the library, for the one twin that is async (x0-async-01): the `no_core`
//! declarations an `async fn` and an `.await` need, each modeled on the pinned library's own
//! (library/core/src, the toolchain shelf, nightly-2026-10-27): the same lang item, the same
//! trait signature.
//!
//! - future/future.rs: `#[lang = "future_trait"] pub trait Future { #[lang = "future_output"]
//!   type Output; #[lang = "poll"] fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) ->
//!   Poll<Self::Output>; }`
//! - future/into_future.rs: `pub trait IntoFuture { type Output; type IntoFuture: Future<Output
//!   = Self::Output>; #[lang = "into_future"] fn into_future(self) -> Self::IntoFuture; }` and
//!   `impl<F: Future> IntoFuture for F { type Output = F::Output; type IntoFuture = F; fn
//!   into_future(self) -> Self::IntoFuture { self } }`. The trait has no lang item: the front
//!   end finds it by its path, `future::IntoFuture`, in the crate that declares `Sized`, which
//!   is why this file is the module `future` of the crate root.
//! - task/poll.rs: `#[lang = "Poll"] pub enum Poll<T> { #[lang = "Ready"] Ready(T), #[lang =
//!   "Pending"] Pending }`
//! - pin.rs: `#[lang = "pin"] pub struct Pin<Ptr> { pointer: Ptr }`. The field is `pub` here:
//!   this program makes and reads a `Pin` by its field, where core's callers go through
//!   `Pin::new_unchecked` and `Pin::get_unchecked_mut`.
//! - task/wake.rs: `#[lang = "Context"] pub struct Context<'a> { waker: &'a Waker, .. }`. Here
//!   the context holds this program's own wake counter in the waker's place: no waker is
//!   cloned or run, a pending future counts its wake.
//! - ops/coroutine.rs: `#[lang = "coroutine_state"] pub enum CoroutineState<Y, R> { Yielded(Y),
//!   Complete(R) }`, `#[lang = "coroutine"] pub trait Coroutine<R = ()> { type Yield; type
//!   Return; #[lang = "coroutine_resume"] fn resume(self: Pin<&mut Self>, arg: R) ->
//!   CoroutineState<Self::Yield, Self::Return>; }`: the item a coroutine's resume fn is named
//!   by.
//!
//! Left out of core's: stability and diagnostic attributes, `Deref for Pin` and the receiver
//! impls (no call here derefs a `Pin`), `Waker` and `RawWakerVTable`.

use crate::prelude::*;

#[lang = "pin"]
pub struct Pin<Ptr> {
    pub pointer: Ptr,
}

#[lang = "Context"]
pub struct Context<'a> {
    /// How often a pending future asked to be polled again.
    pub wakes: &'a mut u32,
}

#[lang = "Poll"]
pub enum Poll<T> {
    #[lang = "Ready"]
    Ready(T),
    #[lang = "Pending"]
    Pending,
}

#[lang = "future_trait"]
pub trait Future {
    #[lang = "future_output"]
    type Output;

    #[lang = "poll"]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output>;
}

pub trait IntoFuture {
    type Output;
    type IntoFuture: Future<Output = Self::Output>;

    #[lang = "into_future"]
    fn into_future(self) -> Self::IntoFuture;
}

impl<F: Future> IntoFuture for F {
    type Output = F::Output;
    type IntoFuture = F;

    fn into_future(self) -> Self::IntoFuture {
        self
    }
}

#[lang = "coroutine_state"]
pub enum CoroutineState<Y, R> {
    Yielded(Y),
    Complete(R),
}

#[lang = "coroutine"]
pub trait Coroutine<R = ()> {
    type Yield;
    type Return;

    #[lang = "coroutine_resume"]
    fn resume(self: Pin<&mut Self>, arg: R) -> CoroutineState<Self::Yield, Self::Return>;
}
