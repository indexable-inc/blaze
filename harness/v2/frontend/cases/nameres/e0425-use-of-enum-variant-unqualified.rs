//@ expect: reject E0425
//@ edition: 2021
enum E {
    First,
    Second,
}

fn main() {
    let _ = E::First;
    let _ = Second;
}
