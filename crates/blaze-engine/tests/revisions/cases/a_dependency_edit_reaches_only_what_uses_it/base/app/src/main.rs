fn caller(x: u32) -> u32 {
    dep::helper(x) * 3
}

fn other(x: u32) -> u32 {
    dep::beside(x)
}

fn main() {
    println!("{} {}", caller(1), other(2));
}
