//@ expect: reject E0277
//@ edition: 2021
// Only a struct's last field may be unsized.
struct Bad {
    head: [u8],
    tail: u8,
}

fn main() {}
