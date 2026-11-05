//@ expect: accept
//@ edition: 2021
// `PhantomPinned` takes away Unpin only: Send and Sync still hold.
use std::marker::PhantomPinned;

struct SelfRef {
    data: [u8; 4],
    _pin: PhantomPinned,
}

fn is_send<T: Send>() -> &'static str {
    "send"
}
fn is_sync<T: Sync>() -> &'static str {
    "sync"
}

fn main() {
    let s = SelfRef { data: [1, 2, 3, 4], _pin: PhantomPinned };
    println!("{} {} {}", is_send::<SelfRef>(), is_sync::<SelfRef>(), s.data.len());
}
