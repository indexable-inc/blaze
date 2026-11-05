//@ edition: 2021
pub fn largest<T: PartialOrd + Copy>(items: &[T]) -> T {
    let mut best = items[0];
    for &it in items { if it > best { best = it; } }
    best
}
#[inline]
pub fn swap_pair<A, B>(pair: (A, B)) -> (B, A) { (pair.1, pair.0) }
pub struct Stack<T> { items: Vec<T> }
impl<T: Clone> Stack<T> {
    pub fn new() -> Self { Stack { items: Vec::new() } }
    pub fn push(&mut self, it: T) { self.items.push(it) }
    pub fn pop(&mut self) -> Option<T> { self.items.pop() }
    pub fn len(&self) -> usize { self.items.len() }
}
pub fn sum_array<const N: usize>(xs: [u32; N]) -> u32 { xs.iter().sum() }
