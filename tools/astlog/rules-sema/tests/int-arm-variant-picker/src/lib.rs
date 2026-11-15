//! The shape of `crates/blaze-api/src/frames/identity.rs`'s `fn request`: a variant picked by a
//! hand-kept integer per arm.

pub enum Kind {
    Lib,
    Bin,
    Test,
    Bench,
    Example,
}

pub enum Goal {
    Check,
    Build,
}

// Found: four integer arms that each make a `Kind`.
pub fn kind(n: u64) -> Kind {
    match n % 5 {
        0 => Kind::Lib,
        1 => Kind::Bin,
        2 => Kind::Test,
        3 => Kind::Bench,
        _ => Kind::Example,
    }
}

// Not found: two integer arms are a choice, not a table.
pub fn goal(n: u64) -> Goal {
    match n % 2 {
        0 => Goal::Check,
        _ => Goal::Build,
    }
}

// Not found: four integer arms that make numbers, not variants.
pub fn width(n: u64) -> u32 {
    match n % 4 {
        0 => 8,
        1 => 16,
        2 => 32,
        _ => 64,
    }
}

// Not found: four arms keyed by variants, mapping to integers (the index direction).
pub fn index(kind: &Kind) -> u64 {
    match kind {
        Kind::Lib => 0,
        Kind::Bin => 1,
        Kind::Test => 2,
        Kind::Bench => 3,
        Kind::Example => 4,
    }
}
