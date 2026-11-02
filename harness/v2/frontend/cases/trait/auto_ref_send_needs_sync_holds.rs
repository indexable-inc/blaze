//@ expect: accept
//@ edition: 2021
// `&T: Send` needs `T: Sync`; `&mut T: Send` needs only `T: Send`, so `&mut Cell<_>` is Send.
use std::cell::Cell;
use std::sync::Mutex;

fn is_send<T: Send>(_: &T) -> &'static str {
    "send"
}

fn main() {
    let a = 5i32;
    let m = Mutex::new(1u8);
    let mut c = Cell::new(2i32);
    let ra = &a;
    let rm = &m;
    let rc = &mut c;
    println!("{} {} {}", is_send(&ra), is_send(&rm), is_send(&rc));
    rc.set(3);
    println!("{} {} {}", a, m.lock().unwrap(), c.get());
}
