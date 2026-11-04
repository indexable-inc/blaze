//! BUILD_ID: hash of `rustc -vV`. Host and Rust modules must come from the same compiler (DESIGN 2.5 d); the module
//! export macro refuses to start on a mismatch.
fn main() {
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let out = std::process::Command::new(rustc).arg("-vV").output().expect("rustc -vV");
    let mut h: u64 = 0xcbf29ce484222325;
    for b in &out.stdout {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    println!("cargo:rustc-env=HYPERION_BUILD_ID={h:016x}");
    println!("cargo:rerun-if-env-changed=RUSTC");
    println!("cargo:rerun-if-changed=build.rs");
}
