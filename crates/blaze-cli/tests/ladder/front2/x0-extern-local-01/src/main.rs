// Own program x0-extern-local-01 (front2): a foreign fn declared by an `extern` block INSIDE a
// body is called by its symbol, exactly as one a module-level block declares (support/
// s3-extern-c is that twin). The pinned core's `panic_fmt` (library/core/src/panicking.rs)
// writes its panic entry this way: `unsafe extern "Rust" { #[lang = "panic_impl"] fn
// panic_impl(pi: &PanicInfo<'_>) -> !; }` in its body, then `unsafe { panic_impl(&pi) }`.
//
// The Reference, "External blocks": [items.extern.namespace] "The external block defines its
// functions and statics in the value namespace of the module or block where it is located."
// [items.extern.abi.intro] "The extern keyword can be followed by an optional ABI string. The
// ABI specifies the calling convention of the functions in the block."
// [items.extern.attributes.link_name.intro] "The link_name attribute may be applied to
// declarations inside an extern block to specify the symbol to import for the given function
// or static." (The pinned nightly's copy, share/doc/rust/html/reference/items/
// external-blocks.html, read 2026-11-16.)
//
// C's `int abs(int)` (the process's libc holds it): `abs(-42)` is 42. `magnitude` is no
// symbol of any library: its `#[link_name = "abs"]` is what is imported, so
// `magnitude(-42)` is 42 too. `main` returns 42 only when BOTH calls bound:
// - a body-local declaration refused or built as a Rust fn with no body: no status at all
//   (the rung prints the refusal);
// - `magnitude` imported by its own name: the import fails by that name;
// - either call bound to another fn: `main` returns 1 or 2, the arm that saw the wrong value.
#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = "sized"]
trait Sized {}

#[lang = "copy"]
trait Copy {}

impl Copy for i32 {}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    unsafe extern "C" {
        fn abs(x: i32) -> i32;
        #[link_name = "abs"]
        fn magnitude(x: i32) -> i32;
    }
    let by_name = unsafe { abs(-42) };
    let by_link_name = unsafe { magnitude(-42) };
    match by_name {
        42 => match by_link_name {
            42 => by_name,
            _ => 2,
        },
        _ => 1,
    }
}
