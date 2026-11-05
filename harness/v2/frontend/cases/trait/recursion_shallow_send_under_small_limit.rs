//@ expect: accept
//@ edition: 2021
// Four levels under `#![recursion_limit = "8"]`: the deepest nested goal is at depth 4, half
// the limit, so it holds with no overflow and no rerun in either solver.
#![recursion_limit = "8"]

struct W<T>(T);

type Shallow = W<W<W<W<u8>>>>;

fn is_send<T: Send>() -> u32 {
    1
}
fn is_sync<T: Sync>() -> u32 {
    1
}

fn main() {
    let n = is_send::<Shallow>() + is_sync::<Shallow>();
    println!("{n}");
}
