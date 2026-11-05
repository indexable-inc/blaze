//@ expect: accept
//@ edition: 2021
//@ aux-crate: derives=derives.rs
use derives::Rank;
fn main() {
    let c = derives::default_config();
    println!("{:?} {}", c, c == c.clone());
    let mut ranks = vec![Rank::High, Rank::Low, Rank::Mid];
    ranks.sort();
    println!("{:?} {}", ranks, Rank::Mid > Rank::Low);
}
