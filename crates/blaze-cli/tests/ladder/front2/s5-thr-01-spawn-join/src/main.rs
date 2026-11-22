//! s5-thr-01: `thread::spawn` and `join` on the real std (pthread on macOS and Linux): results
//! returned in spawn order by joining in that order; a panicking thread's `join` is `Err` (its
//! default hook writes to standard error only); a thread named by `Builder`; the main thread's
//! name is "main" (std's runtime init names it: std/src/rt.rs `init`, thread/main_thread.rs);
//! thread ids differ; `sleep` returns.

use std::thread;
use std::time::Duration;

fn main() {
    let handles: Vec<thread::JoinHandle<u64>> =
        (0..4u64).map(|i| thread::spawn(move || (i * 1000..(i + 1) * 1000).sum())).collect();
    let sums: Vec<u64> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    println!("{:?}", sums);
    println!("{}", sums.iter().sum::<u64>());
    let r = thread::spawn(|| -> i32 { panic!("thread failed") }).join();
    println!("{}", r.is_err());
    let data = vec![1, 2, 3];
    let h = thread::spawn(move || data.iter().map(|x| x * 2).collect::<Vec<i32>>());
    println!("{:?}", h.join().unwrap());
    let b = thread::Builder::new().name("named".into()).spawn(|| thread::current().name().map(str::to_owned)).unwrap();
    println!("{:?} {:?}", b.join().unwrap(), thread::current().name());
    let h = thread::spawn(|| thread::current().id());
    let other = h.join().unwrap();
    println!("{}", other != thread::current().id());
    thread::sleep(Duration::from_millis(1));
    println!("slept");
}
