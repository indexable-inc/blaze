//@ edition: 2021
pub fn sorted_desc(mut xs: Vec<i32>) -> Vec<i32> { xs.sort_by(|a, b| b.cmp(a)); xs }
pub fn find(xs: &[i32], x: i32) -> Result<usize, usize> { xs.binary_search(&x) }
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Clone)]
pub struct Person { pub age: u32, pub name: String }
pub fn oldest(mut ps: Vec<Person>) -> Person { ps.sort(); ps.pop().unwrap() }
