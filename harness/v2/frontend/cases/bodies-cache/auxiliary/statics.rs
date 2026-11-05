//@ edition: 2021
use std::sync::atomic::{AtomicU32, Ordering};
pub static CALLS: AtomicU32 = AtomicU32::new(0);
pub fn call() -> u32 { CALLS.fetch_add(1, Ordering::SeqCst) + 1 }
pub static GREETING: &str = "hello";
