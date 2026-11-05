//@ expect: accept
//@ edition: 2021
// A generic ADT whose fields do not reach its parameter's answer: Send and Sync hold for any
// argument, `Rc`, raw pointers and `Cell` included.
use std::cell::Cell;
use std::marker::PhantomData;
use std::rc::Rc;

struct Key<T> {
    index: u32,
    generation: u32,
    _t: PhantomData<fn(T) -> T>,
}

struct Len<T> {
    n: usize,
    _t: PhantomData<fn() -> T>,
}

fn is_send<T: Send>() -> u32 {
    1
}
fn is_sync<T: Sync>() -> u32 {
    1
}

fn main() {
    let k: Key<Rc<u8>> = Key { index: 1, generation: 2, _t: PhantomData };
    let l: Len<*const u8> = Len { n: 3, _t: PhantomData };
    let n = is_send::<Key<Rc<u8>>>()
        + is_sync::<Key<Cell<u8>>>()
        + is_send::<Len<*const u8>>()
        + is_sync::<Len<Rc<String>>>();
    println!("{} {} {} {}", n, k.index, k.generation, l.n);
}
