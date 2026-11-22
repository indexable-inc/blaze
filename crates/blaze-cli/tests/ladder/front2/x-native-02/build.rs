// x-native-02's build script: compiles src/tls.c into a static library and asks for it to be
// linked (`cc::Build::compile` prints `cargo:rustc-link-lib=static=tls` and the search path
// under OUT_DIR). Under `blaze run` this is the D115 scripts lane's run, native by D112 (the
// shelf's clang); the rung only uses that path.

fn main() {
    println!("cargo::rerun-if-changed=src/tls.c");
    cc::Build::new().file("src/tls.c").compile("tls");
}
