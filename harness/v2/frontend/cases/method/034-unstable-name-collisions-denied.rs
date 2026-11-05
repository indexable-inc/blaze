//@ expect: reject unstable_name_collisions
//@ edition: 2021
// The same pick, with `unstable_name_collisions` denied.
#![deny(unstable_name_collisions)]

trait FlatIter {
    fn into_flat_iter(self) -> u8;
}
impl FlatIter for Option<Vec<u8>> {
    fn into_flat_iter(self) -> u8 {
        42
    }
}

fn main() {
    let o: Option<Vec<u8>> = Some(vec![1]);
    println!("{}", o.into_flat_iter());
}
