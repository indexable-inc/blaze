//@ expect: reject E0277
//@ edition: 2021
// A closure that captures a raw pointer is not Send, so `thread::spawn` rejects it.
use std::thread;

static VALUE: i32 = 5;

fn main() {
    let p = &VALUE as *const i32;
    let h = thread::spawn(move || unsafe { *p });
    let _ = h.join();
}
