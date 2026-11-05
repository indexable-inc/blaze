//@ expect: reject E0277
//@ edition: 2021
// An auto-trait goal on a parameter with no where clause naming it.
fn is_send<T: Send>() {}

fn check<S>(_s: S) {
    is_send::<S>();
}

fn main() {
    check(1u8);
}
