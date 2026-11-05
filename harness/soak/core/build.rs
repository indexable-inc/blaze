//! Writes `table.rs` from `data.txt`: a constant for every line, and their number.
use std::{env, fmt::Write as _, fs, path::Path};

/// What every constant is scaled by. The soak edits it.
const SCALE: u64 = 1;

fn main() {
    println!("cargo:rerun-if-changed=data.txt");
    println!("cargo:rerun-if-changed=build.rs");
    let data = fs::read_to_string("data.txt").expect("data.txt is next to the build script");
    let mut code = String::new();
    let mut count = 0;
    for line in data.lines() {
        let mut words = line.split_whitespace();
        let (Some(name), Some(value)) = (words.next(), words.next()) else { continue };
        let value: u64 = value.parse().expect("the second word of a line is a number");
        writeln!(code, "pub const {}: u64 = {};", name.to_uppercase(), value * SCALE).unwrap();
        count += 1;
    }
    writeln!(code, "pub const COUNT: usize = {count};").unwrap();
    let out = env::var("OUT_DIR").expect("cargo sets OUT_DIR");
    fs::write(Path::new(&out).join("table.rs"), code).expect("OUT_DIR can be written to");
    println!("cargo:rustc-cfg=soak_built");
    println!("cargo:rustc-check-cfg=cfg(soak_built)");
    println!("cargo:rustc-env=SOAK_SCALE={SCALE}");
}
