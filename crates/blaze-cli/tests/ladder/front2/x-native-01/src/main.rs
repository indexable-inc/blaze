//! x-native-01 (NATIVE.md W11): Rust calls C, and C calls back into Rust. The build script
//! compiles src/forty.c (`forty_two` returns `36 + rust_six()`) into a static library linked
//! into the program; `main` exits with what `forty_two` returns: 42.
//!
//! The rules: the Reference, "External blocks": "Functions within external blocks are declared
//! in the same way as other Rust functions, with the exception that they must not have a body
//! and are instead terminated by a semicolon", and calling one is unsafe; "Application binary
//! interface (ABI)", "The no_mangle attribute": it "disables name mangling" so the symbol is
//! the function's name, and "Functions" ("Extern function qualifier"): `extern "C" fn` has the
//! platform's C ABI, so the C side calls `rust_six` by that name. `std::process::exit`
//! (library/std/src/process.rs): "Terminates the current process with the specified exit code";
//! the platform keeps its low 8 bits, so 42 is the status.
//!
//! What it takes of `blaze run`: the build script run (D115, native by D112: `cc` reaches the
//! shelf's clang), the static library's object loaded beside the program with its outside name
//! `forty_two` bound to it and its reference to `rust_six` bound to the Rust function (the
//! native library, NATIVE.md W1..W8). Expected stop until W8 GROUP D (the parent's
//! `Down::Native` send and the launcher's load) is in: `forty_two` is an outside name no image
//! of the process defines, `RunModeError::Outside` "the C name of symbol ..: no image of this
//! process defines it (`dlsym`)" (crates/blaze-launch-rt/src/run_mode.rs `outside`).

unsafe extern "C" {
    fn forty_two() -> i32;
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_six() -> i32 {
    6
}

fn main() {
    // SAFETY: `forty_two` is defined in src/forty.c with this signature, takes nothing and
    // only calls `rust_six`, which is safe.
    std::process::exit(unsafe { forty_two() });
}
