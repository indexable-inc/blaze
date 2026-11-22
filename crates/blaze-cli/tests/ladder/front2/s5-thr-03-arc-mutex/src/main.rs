//! s5-thr-03: shared state across threads: `Arc<Mutex<_>>` counting to 8000 from 8 threads, an
//! `AtomicUsize`, `RwLock` with two readers at once, a `Condvar` handshake, a `Mutex` poisoned by
//! a panicking holder ("a mutex is considered poisoned whenever a thread panics while holding
//! the mutex": std/src/sync/poison/mutex.rs) and read through `PoisonError::into_inner`, and
//! `compare_exchange` (`Ok(previous)` on success, `Err(current)` on failure).

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::thread;

fn main() {
    let counter = Arc::new(Mutex::new(0u64));
    let hs: Vec<_> = (0..8)
        .map(|_| {
            let c = Arc::clone(&counter);
            thread::spawn(move || {
                for _ in 0..1000 {
                    *c.lock().unwrap() += 1;
                }
            })
        })
        .collect();
    for h in hs {
        h.join().unwrap();
    }
    println!("{}", *counter.lock().unwrap());
    let hits = Arc::new(AtomicUsize::new(0));
    let hs: Vec<_> = (0..4)
        .map(|_| {
            let h = hits.clone();
            thread::spawn(move || {
                for _ in 0..250 {
                    h.fetch_add(1, Ordering::Relaxed);
                }
            })
        })
        .collect();
    for h in hs {
        h.join().unwrap();
    }
    println!("{}", hits.load(Ordering::SeqCst));
    let cfg = Arc::new(RwLock::new(vec![1]));
    {
        let r1 = cfg.read().unwrap();
        let r2 = cfg.read().unwrap();
        println!("{} {}", r1.len(), r2.len());
    }
    cfg.write().unwrap().push(2);
    println!("{:?}", *cfg.read().unwrap());
    let pair = Arc::new((Mutex::new(false), Condvar::new()));
    let p2 = Arc::clone(&pair);
    let h = thread::spawn(move || {
        let (m, cv) = &*p2;
        *m.lock().unwrap() = true;
        cv.notify_one();
    });
    {
        let (m, cv) = &*pair;
        let mut ready = m.lock().unwrap();
        while !*ready {
            ready = cv.wait(ready).unwrap();
        }
        println!("ready {}", *ready);
    }
    h.join().unwrap();
    let poisoned = Arc::new(Mutex::new(1));
    let p = poisoned.clone();
    let _ = thread::spawn(move || {
        let _g = p.lock().unwrap();
        panic!("poison");
    })
    .join();
    println!("{}", poisoned.is_poisoned());
    println!("{}", *poisoned.lock().unwrap_or_else(|e| e.into_inner()));
    let flag = AtomicBool::new(false);
    println!("{:?}", flag.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire));
    println!("{:?}", flag.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire));
    let m = Mutex::new(vec![1]);
    m.lock().unwrap().push(2);
    println!("{:?}", m.into_inner().unwrap());
}
