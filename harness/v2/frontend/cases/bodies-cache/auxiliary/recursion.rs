//@ edition: 2021
pub fn fact(n: u64) -> u64 { if n == 0 { 1 } else { n * fact(n - 1) } }
pub fn is_even(n: u32) -> bool { if n == 0 { true } else { is_odd(n - 1) } }
pub fn is_odd(n: u32) -> bool { if n == 0 { false } else { is_even(n - 1) } }
pub enum Tree { Leaf(i32), Node(Box<Tree>, Box<Tree>) }
pub fn sum(t: &Tree) -> i32 { match t { Tree::Leaf(v) => *v, Tree::Node(l, r) => sum(l) + sum(r) } }
