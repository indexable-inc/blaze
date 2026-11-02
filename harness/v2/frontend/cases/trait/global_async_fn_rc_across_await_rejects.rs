//@ expect: reject E0277
//@ edition: 2021
// An `Rc` held across an await makes the `async fn` future not Send (auto-trait leakage
// through the opaque): spawning it where Send is required is E0277.
use std::future::{ready, Future};
use std::rc::Rc;

fn spawn<F: Future + Send + 'static>(_f: F) {}

async fn holds_rc() -> u32 {
    let rc = Rc::new(1u32);
    ready(()).await;
    *rc
}

fn main() {
    spawn(holds_rc());
}
