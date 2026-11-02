fn main() {
    let features = vec![String::from("default")];
    println!("cargo:rustc-env=FEATURES={features:?}");
    println!("cargo:rustc-env=PAIR={:?}", (String::from("neon"),));
}
