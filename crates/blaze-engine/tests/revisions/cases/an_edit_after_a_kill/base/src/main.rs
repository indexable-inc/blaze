fn filled(n: usize) -> Vec<u8> {
    vec![0u8; n]
}

fn main() {
    println!("{} {}", filled(3).len(), app::f(1));
}
