//@ expect: accept
//@ edition: 2021
//@ aux-crate: patterns=patterns.rs
fn main() {
    let all: [&[i32]; 5] = [&[], &[1], &[1, 2, 1], &[1, 2, 3, 4, 5], &[1, 2]];
    for xs in all { println!("{}", patterns::classify(xs)); }
    println!("{} {} {} {}", patterns::bucket(0), patterns::bucket(5), patterns::bucket(20), patterns::bucket(57));
}
