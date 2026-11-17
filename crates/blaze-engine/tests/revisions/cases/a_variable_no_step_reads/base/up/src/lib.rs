pub fn sum(xs: &[u32]) -> u32 {
    let doubled: Vec<u32> = xs.iter().map(|x| x * 2).collect();
    doubled.iter().rev().sum()
}
