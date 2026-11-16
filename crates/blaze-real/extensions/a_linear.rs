//! linear: `#[blaze::linear]` on a type whose value is dropped implicitly at the end of `main`. A working layer refuses it.
//! expect: refuse

#[blaze::linear]
struct Txn {
    id: i32,
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let txn = Txn { id: 42 };
    txn.id
}
