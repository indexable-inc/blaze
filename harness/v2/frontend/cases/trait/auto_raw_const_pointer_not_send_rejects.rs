//@ expect: reject E0277
//@ edition: 2021
// Raw pointers are never Send: core's `impl !Send for *const T`.
fn is_send<T: Send>() {}

fn main() {
    is_send::<*const u8>();
}
