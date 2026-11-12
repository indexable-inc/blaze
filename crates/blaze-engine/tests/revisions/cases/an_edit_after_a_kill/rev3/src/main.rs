fn filled(n: usize) -> Vec<u8> {
    vec![0u8; n]
}

fn f(x: u32) -> u32 {
    x + 10
}

fn main() {
    println!("{} {}", filled(3).len(), f(1));
}
