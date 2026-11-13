struct Small<const N: usize>;

impl<const N: usize> Small<N> {
    const CHECKED: usize = {
        assert!(N < 4, "N is too large for Small");
        N
    };
}

fn checked<const N: usize>() -> usize {
    Small::<N>::CHECKED
}

fn main() {
    println!("before the call");
    println!("{}", checked::<8>());
}
