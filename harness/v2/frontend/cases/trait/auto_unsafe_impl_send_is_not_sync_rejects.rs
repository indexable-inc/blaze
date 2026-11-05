//@ expect: reject E0277
//@ edition: 2021
// An `unsafe impl Send` says nothing of Sync: the raw pointer field still decides it.
struct Handle {
    ptr: *mut u8,
    len: usize,
}

unsafe impl Send for Handle {}

fn is_sync<T: Sync>() {}

fn main() {
    is_sync::<Handle>();
}
