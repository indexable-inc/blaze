use std::fmt::Display;

pub fn line(name: &str, value: impl Display) {
    println!("{name}: {value}");
}
