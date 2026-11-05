//@ expect: reject E0277
//@ edition: 2021
// A union with one raw pointer field is not Send.
union Slot {
    ptr: *const u8,
    addr: usize,
}

fn is_send<T: Send>() {}

fn main() {
    is_send::<Slot>();
}
