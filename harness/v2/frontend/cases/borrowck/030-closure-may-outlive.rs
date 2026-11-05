//@ expect: reject E0373
use std::thread;

fn main() {
    let v = vec![1, 2, 3];
    let h = thread::spawn(|| { //~ ERROR E0373
        println!("{:?}", v);
    });
    h.join().unwrap();
}
