//! The C half of the program, linked as an object, as bun links its C and C++.

use std::{env, path::PathBuf, process::Command};

fn main() {
    let object = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("native.o");
    let cc = env::var("CC").unwrap_or_else(|_| "cc".into());
    let built = Command::new(&cc).args(["-c", "-O2", "native.c", "-o"]).arg(&object).status();
    assert!(built.expect("a C compiler (CC)").success());
    println!("cargo:rerun-if-changed=native.c");
    println!("cargo:rustc-link-arg-bins={}", object.display());
}
