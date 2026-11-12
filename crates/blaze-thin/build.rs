//! `BLAZE_VERSION` and `BLAZE_VERSION_LINE`: which blaze this is, worked out by the one
//! function blaze-engine's build script works it out by, so the thin `blaze` and the engine
//! beside it say the same version and compare a pin to the same one.

include!("../blaze-engine/build_version.rs");

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../blaze-engine/build_version.rs");
    version();
}
