//@ expect: reject E0277
//@ edition: 2021
// The same impl, asked for an argument where `Vec<T>: Clone` fails.
struct Q<T> {
    ptr: *const T,
}

unsafe impl<T> Send for Q<T> where Vec<T>: Clone {}

struct NoClone;

fn is_send<T: Send>() {}

fn main() {
    is_send::<Q<NoClone>>();
}
