fn main() {
    println!("cargo::rerun-if-changed=revision");
    println!("cargo::rustc-check-cfg=cfg(first)");
    println!("cargo::rustc-check-cfg=cfg(second)");
    let revision = std::fs::read_to_string("revision").expect("the revision file");
    println!("cargo::rustc-cfg={}", revision.trim());
}
