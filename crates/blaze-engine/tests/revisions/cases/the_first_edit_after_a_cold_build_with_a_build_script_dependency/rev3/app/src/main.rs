fn shown() -> u32 {
    up::sum(&[1, 2, 3]) + made::MADE + 1
}

fn main() {
    println!("{} {}", shown(), up::longest(&["a", "bcd"]));
}
