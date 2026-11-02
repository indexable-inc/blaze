//@ expect: accept
//@ edition: 2021
// A union takes every field's answer.
use std::mem::ManuallyDrop;

union Bits {
    int: u32,
    float: f32,
    text: ManuallyDrop<String>,
}

fn is_send<T: Send>() -> &'static str {
    "send"
}
fn is_sync<T: Sync>() -> &'static str {
    "sync"
}

fn main() {
    let b = Bits { int: 0x3f80_0000 };
    let f = unsafe { b.float };
    println!("{} {} {}", is_send::<Bits>(), is_sync::<Bits>(), f);
}
