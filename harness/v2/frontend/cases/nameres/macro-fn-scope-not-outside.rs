//@ expect: reject "cannot find macro"
//@ edition: 2021
fn f() {
    macro_rules! local {
        () => {};
    }
    local!();
}

fn main() {
    f();
    local!();
}
