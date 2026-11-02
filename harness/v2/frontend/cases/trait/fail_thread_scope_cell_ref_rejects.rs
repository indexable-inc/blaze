//@ expect: reject E0277
//@ edition: 2021
// A scoped thread's closure borrows a `Cell`: `&Cell<i32>: Send` needs `Cell<i32>: Sync`.
use std::cell::Cell;
use std::thread;

fn main() {
    let c = Cell::new(1);
    thread::scope(|s| {
        s.spawn(|| c.get());
    });
}
