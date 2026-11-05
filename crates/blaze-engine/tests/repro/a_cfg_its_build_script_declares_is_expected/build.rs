fn main() {
    println!("cargo::rustc-check-cfg=cfg(from_build)");
    println!("cargo::rustc-check-cfg=cfg(from_build_value, values(\"one\"))");
    println!("cargo::rustc-cfg=from_build");
}
