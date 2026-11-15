pub fn total(x: u32) -> u32 {
    two::Pair { a: x, b: two::deep::twice(x) }.sum()
}
