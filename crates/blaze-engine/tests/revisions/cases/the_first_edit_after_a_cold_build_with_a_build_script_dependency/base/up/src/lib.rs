//! A workspace crate over the edited one's dependencies: its code is instances of the
//! standard library's generics (`Vec`, the iterator adapters), which the build after the
//! cold one must still find under the ids it made them with.
pub fn sum(xs: &[u32]) -> u32 {
    let doubled: Vec<u32> = xs.iter().map(|x| x * 2).collect();
    doubled.iter().rev().sum()
}

pub fn longest(xs: &[&str]) -> usize {
    xs.iter().map(|it| it.len()).max().unwrap_or(0)
}
