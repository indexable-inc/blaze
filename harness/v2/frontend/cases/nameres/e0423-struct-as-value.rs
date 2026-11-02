//@ expect: reject E0423
//@ edition: 2021
struct S {
    x: u8,
}

fn main() {
    let ok = S { x: 1 };
    let _ = ok.x;
    let _ = S;
}
