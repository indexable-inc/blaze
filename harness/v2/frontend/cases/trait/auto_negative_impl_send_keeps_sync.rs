//@ expect: accept
//@ edition: 2021
// `impl !Send` takes away Send only: Sync and Unpin of the same struct still hold.
#![feature(negative_impls)]

struct Token(u8);

impl !Send for Token {}

fn is_sync<T: Sync>() -> &'static str {
    "sync"
}
fn is_unpin<T: Unpin>() -> &'static str {
    "unpin"
}

fn main() {
    let t = Token(4);
    println!("{} {} {}", is_sync::<Token>(), is_unpin::<Token>(), t.0);
}
