// x-native-01's build script: compiles src/forty.c into a static library and asks for it to be
// linked (`cc::Build::compile` prints `cargo:rustc-link-lib=static=forty` and the search path
// under OUT_DIR). Under `blaze run` this is the D115 scripts lane's run, native by D112 (the
// shelf's clang); the rung only uses that path.

fn main() {
    println!("cargo::rerun-if-changed=src/forty.c");
    cc::Build::new().file("src/forty.c").compile("forty");
}
