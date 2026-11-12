use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, PartialEq)]
struct Point {
    x: i32,
    y: i32,
}

fn answer() -> i32 {
    1
}

fn main() {
    let text = serde_json::to_string(&Point { x: answer(), y: smoke_sys::two() }).unwrap();
    let back: Point = serde_json::from_str(&text).unwrap();
    let lock = parking_lot::Mutex::new(crossbeam_utils::CachePadded::new(back));
    let mut bytes = [0u8; 4];
    getrandom::fill(&mut bytes).unwrap();
    let queue = crossbeam_deque::Injector::new();
    queue.push(bytes.len() as i32);
    println!("smoke {text} {:?} {}", *lock.lock(), queue.len());
}
