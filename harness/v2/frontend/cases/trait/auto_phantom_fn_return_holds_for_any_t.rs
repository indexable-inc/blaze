//@ expect: accept
//@ edition: 2021
// `PhantomData<fn() -> T>` is Send and Sync for every `T`: the fn pointer has no constituents.
use std::cell::Cell;
use std::marker::PhantomData;
use std::rc::Rc;

struct Id<T> {
    raw: u32,
    _t: PhantomData<fn() -> T>,
}

fn is_send<T: Send>() -> u32 {
    1
}
fn is_sync<T: Sync>() -> u32 {
    1
}

fn main() {
    let id: Id<Rc<u8>> = Id { raw: 7, _t: PhantomData };
    let n = is_send::<Id<Rc<u8>>>()
        + is_sync::<Id<Rc<u8>>>()
        + is_send::<Id<*const u8>>()
        + is_sync::<Id<Cell<u8>>>()
        + is_send::<PhantomData<fn() -> Rc<String>>>();
    println!("{} {}", n, id.raw);
}
