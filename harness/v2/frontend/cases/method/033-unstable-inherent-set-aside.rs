//@ expect: accept
//@ edition: 2021
// A stable trait method is picked over an unstable inherent one (`unstable_name_collisions` warns).
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
