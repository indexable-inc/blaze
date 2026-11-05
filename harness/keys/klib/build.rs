use std::{env, fs, path::Path};

fn main() {
    println!("cargo:rerun-if-changed=gen.txt");
    println!("cargo:rerun-if-changed=flag");
    println!("cargo:rerun-if-env-changed=KEYS_SCRIPT");
    let out = env::var("OUT_DIR").unwrap();
    let number = fs::read_to_string("gen.txt").unwrap();
    fs::write(Path::new(&out).join("gen.rs"), format!("{}u32", number.trim())).unwrap();
    println!("cargo:rustc-env=KLIB_FROM_SCRIPT={}", env::var("KEYS_SCRIPT").unwrap_or("unset".into()));
    if Path::new("flag").exists() {
        println!("cargo:rustc-cfg=klib_flag");
    }
}
