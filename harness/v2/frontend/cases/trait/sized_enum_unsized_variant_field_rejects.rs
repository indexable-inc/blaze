//@ expect: reject E0277
//@ edition: 2021
// Every field of an enum variant must be Sized, the last one too.
enum Text {
    Empty,
    Inline(str),
}

fn main() {}
