//@ expect: reject E0277
//@ edition: 2021
// `thread::spawn` needs a Send closure: one that captures an `Rc` by move is not.
use std::rc::Rc;
use std::thread;

fn main() {
    let rc = Rc::new(1);
    let h = thread::spawn(move || *rc + 1);
    let _ = h.join();
}
