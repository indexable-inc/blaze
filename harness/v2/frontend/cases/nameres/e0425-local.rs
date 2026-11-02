//@ expect: reject E0425
//@ edition: 2021
fn main() {
    let present = 1;
    let _ = present + absent;
}
