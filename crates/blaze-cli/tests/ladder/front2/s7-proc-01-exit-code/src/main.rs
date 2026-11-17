//! s7-proc-01: `std::process::exit(3)` ends the process with status 3.
//!
//! std/src/process.rs: `pub fn exit(code: i32) -> ! { crate::rt::cleanup(); crate::sys::exit::exit(code) }`;
//! `rt::cleanup` "Flush stdout and disable buffering" (std/src/rt.rs), so the unterminated
//! `print!` reaches standard output; the docs: "no destructors on the current stack or any other
//! thread's stack will be run", so `Loud`'s line never appears. The process is the program's own:
//! `blaze run` reports the status it ended with.

struct Loud;

impl Drop for Loud {
    fn drop(&mut self) {
        println!("destructor ran");
    }
}

fn main() {
    let _loud = Loud;
    println!("before exit");
    print!("unflushed tail");
    std::process::exit(3);
}
