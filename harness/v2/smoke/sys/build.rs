fn main() {
    println!("cargo::rerun-if-changed=two.c");
    cc::Build::new().file("two.c").compile("smoketwo");
}
