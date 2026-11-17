// bindgen's shape: the script cannot run without a variable that says where a library is.
fn main() {
    let at = std::env::var("NEEDED_BY_THE_SCRIPT").expect("NEEDED_BY_THE_SCRIPT is not set: this script needs it");
    let out = std::env::var("OUT_DIR").expect("OUT_DIR");
    std::fs::write(format!("{out}/needs.rs"), format!("pub const AT: &str = {at:?};\n")).expect("writing needs.rs");
    println!("cargo::rerun-if-env-changed=NEEDED_BY_THE_SCRIPT");
    println!("cargo::rerun-if-changed=build.rs");
}
