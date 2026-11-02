//@ expect: reject E0277
//@ edition: 2021
// A Copy bound on a method's type parameter, failed by a `String` argument.
struct Dup;

impl Dup {
    fn dup<T: Copy>(&self, t: T) -> (T, T) {
        (t, t)
    }
}

fn main() {
    let _p = Dup.dup(String::from("s"));
}
