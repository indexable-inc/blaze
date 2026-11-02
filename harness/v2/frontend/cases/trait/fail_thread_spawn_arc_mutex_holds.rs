//@ expect: accept
//@ edition: 2021
// The accepted side of the spawn cases: `Arc<Mutex<u32>>` moved into four threads, and a
// scoped thread borrowing a `Mutex` (Sync) and an `AtomicU32`.
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

fn main() {
    let total = Arc::new(Mutex::new(0u32));
    let handles: Vec<_> = (0..4)
        .map(|i| {
            let t = Arc::clone(&total);
            thread::spawn(move || *t.lock().unwrap() += i)
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    let hits = AtomicU32::new(0);
    let names = Mutex::new(Vec::new());
    thread::scope(|s| {
        for i in 0..3u32 {
            let (hits, names) = (&hits, &names);
            s.spawn(move || {
                hits.fetch_add(1, Ordering::SeqCst);
                names.lock().unwrap().push(i);
            });
        }
    });
    let mut names = names.into_inner().unwrap();
    names.sort();
    println!("{} {} {:?}", total.lock().unwrap(), hits.load(Ordering::SeqCst), names);
}
