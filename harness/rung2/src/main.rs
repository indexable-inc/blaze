//! Futures the way tower builds them: combinators whose `Future` impl is found through
//! other impls' where clauses.

use futures_core::TryFuture;
use futures_util::{future, TryFutureExt};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

fn block_on<T>(mut future: Pin<Box<dyn Future<Output = T>>>) -> T {
    fn raw() -> RawWaker {
        static VTABLE: RawWakerVTable = RawWakerVTable::new(|_| raw(), |_| {}, |_| {}, |_| {});
        RawWaker::new(std::ptr::null(), &VTABLE)
    }
    let waker = unsafe { Waker::from_raw(raw()) };
    let mut cx = Context::from_waker(&waker);
    loop {
        if let Poll::Ready(value) = future.as_mut().poll(&mut cx) {
            return value;
        }
    }
}

/// `tower::util::AndThenFuture`.
pub struct AndThenFuture<F1, F2: TryFuture, N> {
    inner: future::AndThen<future::ErrInto<F1, F2::Error>, F2, N>,
}

impl<F1, F2: TryFuture, N> Future for AndThenFuture<F1, F2, N>
where
    future::AndThen<future::ErrInto<F1, F2::Error>, F2, N>: Future,
{
    type Output = <future::AndThen<future::ErrInto<F1, F2::Error>, F2, N> as Future>::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        unsafe { self.map_unchecked_mut(|it| &mut it.inner) }.poll(cx)
    }
}

fn and_then<F1, F2, N>(first: F1, next: N) -> AndThenFuture<F1, F2, N>
where
    F1: TryFuture,
    F1::Error: Into<F2::Error>,
    F2: TryFuture,
    N: FnOnce(F1::Ok) -> F2,
{
    AndThenFuture { inner: first.err_into().and_then(next) }
}

fn map_ok(seed: u8) -> u8 {
    block_on(Box::pin(async move { Ok::<u8, ()>(seed) }.map_ok(|it| it + 1))).unwrap()
}

fn err_into(seed: u8) -> u8 {
    block_on(Box::pin(async move { Ok::<u8, u8>(seed) }.err_into::<u32>())).unwrap()
}

fn plain_and_then(seed: u8) -> u8 {
    let future = async move { Ok::<u8, ()>(seed) }.and_then(|it| async move { Ok(it + 2) });
    block_on(Box::pin(future)).unwrap()
}

fn err_into_and_then(seed: u8) -> u8 {
    let future = async move { Ok::<u8, u8>(seed) }
        .err_into::<u32>()
        .and_then(|it| async move { Ok(it + 3) });
    block_on(Box::pin(future)).unwrap()
}

fn tower_and_then(seed: u8, name: String) -> usize {
    let future = and_then(async move { Ok::<u8, u8>(seed) }, move |it| async move {
        Ok::<usize, u32>(usize::from(it) + name.len())
    });
    block_on(Box::pin(future)).unwrap()
}

fn tower_service(seed: u8, name: String) -> usize {
    use tower::{service_fn, ServiceBuilder, ServiceExt};
    let service = ServiceBuilder::new()
        .and_then(move |it: u8| async move {
            Ok::<usize, std::convert::Infallible>(usize::from(it) + name.len())
        })
        .service(service_fn(|request: u8| async move {
            Ok::<u8, std::convert::Infallible>(request + 1)
        }));
    block_on(Box::pin(service.oneshot(seed))).unwrap()
}

fn tower_inferred_error(seed: u8, name: String) -> usize {
    use tower::{service_fn, ServiceBuilder, ServiceExt};
    let service = ServiceBuilder::new()
        .and_then(move |it| async move { Ok(usize::from(it) + name.len()) })
        .service(service_fn(|request: u8| async move {
            Ok::<u8, std::convert::Infallible>(request + 1)
        }));
    let future: Pin<Box<dyn Future<Output = Result<usize, std::convert::Infallible>>>> =
        Box::pin(service.oneshot(seed));
    block_on(future).unwrap()
}

type Dyn<T> = Pin<Box<dyn Future<Output = T>>>;

fn serve_dir_plain(dir: &str) -> u16 {
    use tower::ServiceExt;
    let service = tower_http::services::ServeDir::new(dir);
    let request = http::Request::new(String::new());
    let future: Dyn<_> = Box::pin(service.oneshot(request));
    block_on(future).unwrap().status().as_u16()
}

fn serve_dir_and_then(dir: &str, extra: u16) -> u16 {
    use tower::{ServiceBuilder, ServiceExt};
    let service = ServiceBuilder::new()
        .and_then(move |response: http::Response<_>| async move {
            Ok(response.status().as_u16() + extra)
        })
        .service(tower_http::services::ServeDir::new(dir));
    let request = http::Request::new(String::new());
    let future: Dyn<Result<u16, std::convert::Infallible>> = Box::pin(service.oneshot(request));
    block_on(future).unwrap()
}

fn sd_future(dir: &str) -> impl Future<Output = Result<http::Response<tower_http::services::fs::ServeFileSystemResponseBody>, std::convert::Infallible>> {
    use tower::Service;
    tower_http::services::ServeDir::new(dir).call(http::Request::new(String::new()))
}

fn sd_err_into(dir: &str) -> u16 {
    use futures_util::TryFutureExt;
    let future: Dyn<Result<_, std::convert::Infallible>> = Box::pin(sd_future(dir).err_into());
    block_on(future).unwrap().status().as_u16()
}

fn sd_map_ok(dir: &str) -> u16 {
    use futures_util::TryFutureExt;
    let future: Dyn<_> = Box::pin(sd_future(dir).map_ok(|it| it.status().as_u16()));
    block_on(future).unwrap()
}

fn sd_and_then(dir: &str) -> u16 {
    use futures_util::TryFutureExt;
    let future: Dyn<_> = Box::pin(sd_future(dir).and_then(|it| async move { Ok(it.status().as_u16()) }));
    block_on(future).unwrap()
}

fn main() {
    println!("sd_err_into {}", sd_err_into("/nonexistent"));
    println!("sd_map_ok {}", sd_map_ok("/nonexistent"));
    println!("sd_and_then {}", sd_and_then("/nonexistent"));
    println!("serve_dir_plain {}", serve_dir_plain("/nonexistent"));
    println!("serve_dir_and_then {}", serve_dir_and_then("/nonexistent", 1));
    println!("tower_service {}", tower_service(1, "four".to_owned()));
    println!("tower_inferred_error {}", tower_inferred_error(1, "four".to_owned()));
    println!("map_ok {}", map_ok(1));
    println!("err_into {}", err_into(1));
    println!("plain_and_then {}", plain_and_then(1));
    println!("err_into_and_then {}", err_into_and_then(1));
    println!("tower_and_then {}", tower_and_then(1, "four".to_owned()));
}
