//@ expect: reject E0425
//@ edition: 2021
fn defined() {}

fn main() {
    defined();
    undefined();
}
