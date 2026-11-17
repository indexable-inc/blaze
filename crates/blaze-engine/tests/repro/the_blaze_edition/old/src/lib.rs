//! A library of Rust 2024, with Rust's `std`: what a program of the blaze edition depends on
//! as on any crate.

/// Built with Rust std's own collections and formatting, inside the old crate.
pub fn answer() -> u64 {
    let parts: Vec<String> = vec!["4".to_string(), "2".to_string()];
    parts.concat().parse().expect("a number")
}
