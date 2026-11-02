//@ expect: reject E0277
//@ edition: 2021
// A Clone bound on a spawn helper, failed by a closure that captures a non-Clone value. The
// closure moves `n` whole: `move || n.0` would capture only the `u8` field (edition 2021's
// precise captures) and be Clone.
use std::thread;

struct NoClone(u8);

fn spawn_twice<F>(f: F) -> (u8, u8)
where
    F: FnOnce() -> u8 + Clone + Send + 'static,
{
    let g = f.clone();
    (thread::spawn(f).join().unwrap(), thread::spawn(g).join().unwrap())
}

fn main() {
    let n = NoClone(4);
    let _r = spawn_twice(move || {
        let m = n;
        m.0
    });
}
