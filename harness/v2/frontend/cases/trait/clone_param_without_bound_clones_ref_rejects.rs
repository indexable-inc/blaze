//@ expect: reject E0308
//@ edition: 2021
// With no `T: Clone`, `t.clone()` on `&T` resolves to `<&T as Clone>::clone` and returns `&T`:
// the error is the mismatched return type, E0308, not a failed Clone goal.
fn dup<T>(t: &T) -> T {
    t.clone()
}

fn main() {
    let _x = dup(&1u8);
}
