//! holds: `#[blaze::holds]` on a handle over a raw pointer, so a handle of a linear value is linear and is dropped here. A working layer refuses it.
//! expect: refuse

#[blaze::linear]
struct Txn {
    id: i32,
}

#[blaze::holds]
struct Handle<T> {
    at: *const T,
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let h: Handle<Txn> = Handle { at: 0 as *const Txn };
    let _ = h.at;
    0
}
