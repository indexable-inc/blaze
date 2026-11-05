//@ expect: accept
//@ edition: 2021
// `Arc<Mutex<T>>` is Send and Sync for `T: Send`; `Mutex<Cell<_>>` is Sync though `Cell` is not.
use std::cell::Cell;
use std::sync::{Arc, Mutex, RwLock};

fn is_send<T: Send>() -> u32 {
    1
}
fn is_sync<T: Sync>() -> u32 {
    1
}

fn main() {
    let shared = Arc::new(Mutex::new(vec![String::from("a")]));
    shared.lock().unwrap().push(String::from("b"));
    let n = is_send::<Arc<Mutex<Vec<String>>>>()
        + is_sync::<Arc<Mutex<Vec<String>>>>()
        + is_sync::<Mutex<Cell<i32>>>()
        + is_send::<Arc<RwLock<u64>>>()
        + is_sync::<RwLock<Vec<u8>>>();
    println!("{} {}", n, shared.lock().unwrap().len());
}
