//! x-native-02 (NATIVE.md W11): a C `__thread` variable set by a C constructor, read from Rust
//! on the first thread. The build script compiles src/tls.c into a static library linked into
//! the program; its constructor `init` sets the loading thread's `n` to 5 before `main`, and
//! `main` exits with what `read_n` returns: 5.
//!
//! The rules: the Reference, "External blocks" (a foreign function is declared without a body
//! and called in `unsafe`) and "Application binary interface (ABI)" (`extern "C"`: the C ABI
//! by the symbol's own name). `std::process::exit` (library/std/src/process.rs): "Terminates
//! the current process with the specified exit code". The C side's rules are the C toolchain's:
//! a `constructor` function runs when its image is loaded, before `main`; a `__thread` object
//! has one instance per thread, zero-initialized, so only the thread that ran `init` (the first
//! one, which runs `main`) sees 5. A launcher that ran the constructor on another thread, or
//! never, makes this exit 0; one that gave no thread-local block reads elsewhere: both WRONG.
//!
//! What it takes of `blaze run`, beyond x-native-01: the native library's TLV descriptors
//! (Mach-O `__thread_vars`, ELF TLS) given a block for the first thread, and its initializers
//! (`__mod_init_func` / `.init_array`) run on that thread before `main`. Expected stop until
//! W8 GROUP D is in: as x-native-01's, at the outside name `read_n` (`RunModeError::Outside`,
//! "no image of this process defines it (`dlsym`)").

unsafe extern "C" {
    fn read_n() -> i32;
}

fn main() {
    // SAFETY: `read_n` is defined in src/tls.c with this signature; it takes nothing and reads
    // the calling thread's `n`.
    std::process::exit(unsafe { read_n() });
}
