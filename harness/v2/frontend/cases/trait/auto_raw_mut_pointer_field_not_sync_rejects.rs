//@ expect: reject E0277
//@ edition: 2021
// A struct holding a `*mut u8` is not Sync: the field's negative impl decides it.
struct Raw {
    len: usize,
    ptr: *mut u8,
}

fn is_sync<T: Sync>() {}

fn main() {
    is_sync::<Raw>();
}
