//@ expect: reject E0599
//@ edition: 2021
// `#[derive(Clone)]` on `W<T>` gives `impl<T: Clone> Clone for W<T>`. A method call `.clone()`
// on `W<NoClone>` finds that impl with an unmet bound, and no other candidate (no Deref, and
// autoref stops at `&W<NoClone>` as `Self = W<NoClone>`): E0599, "the method `clone` exists
// for struct `W<NoClone>`, but its trait bounds were not satisfied".
#[derive(Clone)]
struct W<T>(T);

struct NoClone;

fn main() {
    let w = W(NoClone);
    let _v = w.clone();
}
