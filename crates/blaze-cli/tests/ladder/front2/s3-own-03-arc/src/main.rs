//! s3-own-03: `Arc` and `sync::Weak`: counts, `make_mut` cloning a shared value ("If there are
//! other Arc pointers to the same allocation, then make_mut will clone the inner value"),
//! `try_unwrap` once unique, `Arc<str>`, and clones moved into threads that are dropped there
//! before `join` returns (a spawned closure and its captures are dropped when it returns, before
//! the thread's result is handed to `join`).

use std::sync::{Arc, Weak};
use std::thread;

fn main() {
    let a = Arc::new(vec![1, 2, 3]);
    let b = Arc::clone(&a);
    println!("{} {}", Arc::strong_count(&a), Arc::ptr_eq(&a, &b));
    let mut c = Arc::clone(&a);
    Arc::make_mut(&mut c).push(4);
    println!("{:?} {:?} {}", a, c, Arc::ptr_eq(&a, &c));
    drop(b);
    println!("{:?}", Arc::try_unwrap(a));
    let w: Weak<i32>;
    {
        let s = Arc::new(9);
        w = Arc::downgrade(&s);
        println!("{:?}", w.upgrade());
    }
    println!("{:?}", w.upgrade());
    let s: Arc<str> = Arc::from("shared");
    let t = s.clone();
    println!("{} {}", t, Arc::strong_count(&s));
    let handles: Vec<_> = (0..4)
        .map(|i| {
            let data = Arc::clone(&c);
            thread::spawn(move || data[i])
        })
        .collect();
    let got: Vec<i32> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    println!("{:?}", got);
    println!("{}", Arc::strong_count(&c));
}
