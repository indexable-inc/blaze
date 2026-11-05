//@ expect: accept
//@ edition: 2021
// A static's Sync goal on `Mutex<Vec<u8>>` holds.
use std::sync::Mutex;

static LOG: Mutex<Vec<u8>> = Mutex::new(Vec::new());

fn main() {
    LOG.lock().unwrap().push(3);
    LOG.lock().unwrap().push(4);
    println!("{:?}", LOG.lock().unwrap());
}
