//@ expect: reject "cannot find macro"
//@ edition: 2021
fn main() {
    later!();
}

macro_rules! later {
    () => {};
}
