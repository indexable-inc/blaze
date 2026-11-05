//! What the build script and the macros make, put together.
use soak_gen::{table, twice, Describe};

pub mod table {
    include!(concat!(env!("OUT_DIR"), "/table.rs"));
}

#[derive(Describe, Debug, Default)]
pub struct Reading {
    pub level: u64,
}

#[twice]
pub fn level() -> u64 {
    table::ALPHA + table::COUNT as u64
}

pub fn names() -> Vec<(&'static str, u64)> {
    table!(alpha, beta, gamma).to_vec()
}

pub fn summary() -> String {
    let built = if cfg!(soak_built) { "built" } else { "plain" };
    let loud = if cfg!(feature = "loud") { "LOUD" } else { "quiet" };
    let total: u64 = names().iter().map(|it| it.1).sum();
    format!(
        "{} {} {} scale {} level {} twice {} total {}",
        Reading::describe(),
        built,
        loud,
        env!("SOAK_SCALE"),
        level(),
        level_twice(),
        total
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn summary_names_the_type() {
        assert!(super::summary().starts_with("Reading/"));
    }

    #[test]
    fn twice_is_twice() {
        assert_eq!(super::level_twice(), super::level() * 2);
    }
}
