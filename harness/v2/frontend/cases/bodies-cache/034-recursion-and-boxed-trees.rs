//@ expect: accept
//@ edition: 2021
//@ aux-crate: recursion=recursion.rs
use recursion::Tree;
fn main() {
    let t = Tree::Node(Box::new(Tree::Leaf(2)), Box::new(Tree::Node(Box::new(Tree::Leaf(3)), Box::new(Tree::Leaf(4)))));
    println!("{} {} {} {}", recursion::fact(10), recursion::is_even(10), recursion::is_odd(7), recursion::sum(&t));
}
