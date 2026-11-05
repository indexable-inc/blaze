//@ expect: reject E0277
//@ edition: 2021
// `Copy: Clone`: a Copy impl on a type with no Clone impl fails its supertrait, E0277.
#[derive(Copy)]
struct Unit;

fn main() {
    let u = Unit;
    let _v = u;
}
