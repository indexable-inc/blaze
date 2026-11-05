//@ expect: accept
//@ edition: 2021
//@ aux-crate: iters=iters.rs
fn main() {
    println!("{:?}", iters::Countdown(4).collect::<Vec<_>>());
    println!("{:?} {:?} {:?}", iters::evens_squared(6), iters::zipped(), iters::windows_max(&[1, 5, 2, 8]));
}
