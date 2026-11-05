//@ expect: reject E0277
//@ edition: 2021
// `Clone::clone(&v)` names the trait, so there is no method probe: `Vec<NoClone>: Clone` fails
// on `NoClone: Clone` with E0277.
struct NoClone;

fn main() {
    let v = vec![NoClone];
    let _w = Clone::clone(&v);
}
