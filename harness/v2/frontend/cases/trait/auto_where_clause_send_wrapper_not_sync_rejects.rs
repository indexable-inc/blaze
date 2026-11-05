//@ expect: reject E0277
//@ edition: 2021
// `S: Send` in the where clause does not give `Wrap<S>: Sync`, which needs `S: Sync`.
struct Wrap<T>(T);

fn is_sync<T: Sync>() {}

fn check<S: Send>() {
    is_sync::<Wrap<S>>();
}

fn main() {
    check::<u8>();
}
