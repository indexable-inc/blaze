fn shown() -> u32 {
    up::sum(&[1, 2, 3]) + made::MADE
}

fn main() {
    println!("{} {}", shown(), up::longest(&["a", "bcd"]));
}
