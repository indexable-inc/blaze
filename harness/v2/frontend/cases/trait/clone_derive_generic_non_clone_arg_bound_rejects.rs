//@ expect: reject E0277
//@ edition: 2021
// The same derive, asked through a function bound: `W<NoClone>: Clone` fails with E0277.
#[derive(Clone)]
struct W<T>(T);

struct NoClone;

fn dup<T: Clone>(t: &T) -> (T, T) {
    (t.clone(), t.clone())
}

fn main() {
    let w = W(NoClone);
    let _pair = dup(&w);
}
