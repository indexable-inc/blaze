/* x-native-01: C calls back into Rust. `rust_six` is the Rust program's `#[no_mangle]`
   `extern "C"` function (src/main.rs); this library names it and is linked into the program. */

extern int rust_six(void);

int forty_two(void) { return 36 + rust_six(); }
