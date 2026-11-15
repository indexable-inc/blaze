fn shown() -> u32 {
    up::sum(&[1, 2, 3]) + made::MADE
}

fn tag() -> &'static str {
    option_env!("APP_TAG").unwrap_or("none")
}

fn main() {
    println!("{} {}", shown(), tag());
}
