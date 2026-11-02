//@ expect: reject E0277
//@ edition: 2021
// `PhantomData<T>` is Send exactly when `T` is: `PhantomData<Rc<u8>>` is not.
use std::marker::PhantomData;
use std::rc::Rc;

struct Tagged {
    id: u32,
    _owner: PhantomData<Rc<u8>>,
}

fn is_send<T: Send>() {}

fn main() {
    is_send::<Tagged>();
}
