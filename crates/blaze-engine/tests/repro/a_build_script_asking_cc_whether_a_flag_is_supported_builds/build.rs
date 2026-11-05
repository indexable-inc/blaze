fn main() {
    let asked = cc::Build::new().is_flag_supported("-O2").map(|_| "asked").unwrap_or("asked");
    println!("cargo:rustc-env=ASKED={asked}");
}
