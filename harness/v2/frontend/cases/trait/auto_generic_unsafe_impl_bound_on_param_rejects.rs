//@ expect: reject E0277
//@ edition: 2021
// The impl's where clause `T: Send` fails for `T = Rc<u8>`; the structural rule is not tried.
use std::rc::Rc;

struct P<T> {
    ptr: *const T,
}

unsafe impl<T: Send> Send for P<T> {}

fn is_send<T: Send>() {}

fn main() {
    is_send::<P<Rc<u8>>>();
}
