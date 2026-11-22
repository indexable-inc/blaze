//! Hands the compiler that builds bstd to its refusal test (`tests/refusals.rs`), which compiles
//! each fixture in `refusals/` with that same compiler against the bstd it just built.

fn main() {
    // asks blaze: forwards build-time RUSTC to the compile-fail refusal test harness
    #[expect(clippy::disallowed_methods, reason = "cargo hands a build script its compiler only as `RUSTC`")]
    let rustc = std::env::var("RUSTC").expect("cargo sets RUSTC for every build script");
    println!("cargo::rustc-env=BSTD_RUSTC={rustc}");
    println!("cargo::rerun-if-env-changed=RUSTC");
    println!("cargo::rerun-if-changed=build.rs");
}
