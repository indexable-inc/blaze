// A const argument is read by its value: `n::<1>` and `n::<2>` must not share.
fn n<const N: usize>() -> usize {
    N
}

fn filled<const N: usize>() -> [u8; N] {
    [7; N]
}

fn sum<T: Copy + Into<u64>, const N: usize>(values: [T; N]) -> u64 {
    let mut total = 0u64;
    for value in values {
        total += Into::<u64>::into(value);
    }
    total
}

fn main() {
    println!("{} {}", n::<1>(), n::<2>());
    println!("{:?} {:?}", filled::<1>(), filled::<3>());
    println!("{} {}", sum([1u8, 2, 3]), sum([1u32, 2, 3, 4]));
}
