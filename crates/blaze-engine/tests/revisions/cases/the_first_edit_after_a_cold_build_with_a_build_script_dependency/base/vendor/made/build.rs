// A package outside the workspace (a library to the front end) whose build script leaves a
// file in `OUT_DIR` that its library includes: serde, thiserror and bindgen's shape.
fn main() {
    let out = std::env::var("OUT_DIR").expect("OUT_DIR");
    std::fs::write(format!("{out}/made.rs"), "pub const MADE: u32 = 40;\n").expect("writing made.rs");
    println!("cargo::rerun-if-changed=build.rs");
}
