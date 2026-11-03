#![feature(linkage)]

use std::cell::RefCell;
use std::fmt::Display;

unsafe extern "C" {
    /// libc's `getpid`, weakly: defined on every target this runs on, so the address is set.
    #[link_name = "getpid"]
    #[linkage = "extern_weak"]
    static WEAK_GETPID: Option<unsafe extern "C" fn() -> i32>;
}

thread_local! {
    /// A thread-local with a destructor: on Linux std registers it through its own
    /// `extern_weak` statics (`__cxa_thread_atexit_impl`, `__dso_handle`).
    static SEEN: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

fn main() {
    // SAFETY: reading a weak static's address, which is all it holds.
    let found = unsafe { WEAK_GETPID }.is_some();
    // A call through a `dyn` in a body of this crate at MIR level 2 (`opt-level = 3`), so the
    // program's dyn facts are made.
    let shown: &dyn Display = &found;
    SEEN.with(|it| it.borrow_mut().push(shown.to_string()));
    println!("{}", SEEN.with(|it| it.borrow().join(",")));
}
