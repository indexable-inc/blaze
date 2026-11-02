//@ expect: reject E0204
//@ edition: 2021
// Copy cannot be implemented for a struct with a `String` field.
#[derive(Clone, Copy)]
struct Named {
    id: u32,
    name: String,
}

fn main() {
    let n = Named { id: 1, name: String::new() };
    let _m = n;
}
