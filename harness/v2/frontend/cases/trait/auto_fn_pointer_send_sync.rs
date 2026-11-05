//@ expect: accept
//@ edition: 2021
// A fn pointer has no constituent types: Send and Sync hold whatever its signature names.
use std::rc::Rc;

fn is_send<T: Send>(_: &T) -> &'static str {
    "send"
}
fn is_sync<T: Sync>(_: &T) -> &'static str {
    "sync"
}

fn make(p: *const u8) -> Rc<u8> {
    Rc::new(p.is_null() as u8)
}

struct Table {
    f: fn(*const u8) -> Rc<u8>,
    g: fn(u32) -> u32,
}

fn double(x: u32) -> u32 {
    x * 2
}

fn main() {
    let t = Table { f: make, g: double };
    println!("{} {}", is_send(&t), is_sync(&t));
    println!("{} {}", (t.f)(std::ptr::null()), (t.g)(21));
}
