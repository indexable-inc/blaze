//! s2-gen-02: generic structs and fns, derived `PartialOrd`/`Ord` (lexicographic by field in
//! declaration order: the `PartialOrd` docs, "Derivable"), associated consts with a provided
//! method under a `where Self: Debug` clause, const generics, and a chained `&mut Self` API.

use std::fmt::Debug;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Version {
    major: u32,
    minor: u32,
    patch: u32,
}

struct Stack<T> {
    items: Vec<T>,
}

impl<T: Debug> Stack<T> {
    fn new() -> Self {
        Stack { items: Vec::new() }
    }
    fn push(&mut self, t: T) -> &mut Self {
        self.items.push(t);
        self
    }
    fn pop(&mut self) -> Option<T> {
        self.items.pop()
    }
    fn peek(&self) -> Option<&T> {
        self.items.last()
    }
}

trait Describe {
    const KIND: &'static str;
    fn describe(&self) -> String
    where
        Self: Debug,
    {
        format!("{}: {:?}", Self::KIND, self)
    }
}

impl Describe for Version {
    const KIND: &'static str = "version";
}

fn largest<T: PartialOrd + Copy>(xs: &[T]) -> T {
    let mut m = xs[0];
    for &x in xs {
        if x > m {
            m = x;
        }
    }
    m
}

fn pair_up<A, B>(a: Vec<A>, b: Vec<B>) -> Vec<(A, B)> {
    a.into_iter().zip(b).collect()
}

struct Wrapper<const N: usize> {
    data: [u8; N],
}

impl<const N: usize> Wrapper<N> {
    fn sum(&self) -> u32 {
        self.data.iter().map(|&b| b as u32).sum()
    }
}

fn main() {
    let mut vs = vec![
        Version { major: 1, minor: 2, patch: 3 },
        Version { major: 1, minor: 10, patch: 0 },
        Version { major: 0, minor: 9, patch: 9 },
    ];
    vs.sort();
    println!("{:?}", vs.iter().map(|v| format!("{}.{}.{}", v.major, v.minor, v.patch)).collect::<Vec<_>>());
    println!("{}", vs[0] < vs[1]);
    println!("{}", vs.iter().max().unwrap().describe());
    let mut s = Stack::new();
    s.push(1).push(2).push(3);
    let popped = s.pop();
    println!("{:?} {:?} {:?}", popped, s.peek(), s.items);
    println!("{} {} {}", largest(&[3, 9, 2]), largest(&[0.5, -1.0]), largest(&['x', 'b']));
    println!("{:?}", pair_up(vec![1, 2, 3], vec!["a", "b"]));
    println!("{} {}", Wrapper { data: [1, 2, 3] }.sum(), Wrapper::<0> { data: [] }.sum());
    println!("{}", <Version as Describe>::KIND);
    let v2 = vs[1].clone();
    println!("{}", v2 == vs[1]);
}
