//! s4-err-04: `panic::catch_unwind` with the real unwinder: a live local's destructor runs during
//! the unwind ("as the panic traverses Rust frames, live objects in those frames that implement
//! Drop will have their drop methods called", the Reference, [panic.unwind.destruction]); the
//! payload of `panic!("literal")` is a `&'static str` and of a formatted panic a `String`
//! (std/src/panicking.rs `begin_panic_handler`: `msg.as_str()` gives `StaticStrPayload`, else
//! `FormatStringPayload`); `panic_any` keeps its value; an index out of bounds panics; a nested
//! catch; `set_hook` with a silent hook (nothing is written to standard error).

use std::panic;

struct Guard(&'static str);

impl Drop for Guard {
    fn drop(&mut self) {
        println!("guard {} dropped", self.0);
    }
}

fn main() {
    panic::set_hook(Box::new(|_| {}));
    let r = panic::catch_unwind(|| {
        let _g = Guard("inner");
        panic!("boom");
    });
    match r {
        Err(p) => println!("caught: {:?}", p.downcast_ref::<&str>()),
        Ok(()) => println!("no panic"),
    }
    let r2 = panic::catch_unwind(|| -> () {
        let n = 7;
        panic!("bad value {n}");
    });
    let p2 = r2.unwrap_err();
    println!("{:?} {:?}", p2.downcast_ref::<String>(), p2.downcast_ref::<&str>());
    let ok = panic::catch_unwind(|| 40 + 2);
    println!("{:?}", ok);
    let r3 = panic::catch_unwind(|| {
        let v: Vec<i32> = Vec::new();
        v[3]
    });
    println!("{}", r3.is_err());
    let p4 = panic::catch_unwind(|| -> u8 { panic::panic_any(17u32) }).unwrap_err();
    println!("{:?}", p4.downcast_ref::<u32>());
    let nested = panic::catch_unwind(|| {
        let inner = panic::catch_unwind(|| -> () { panic!("inner") });
        println!("inner caught: {}", inner.is_err());
        5
    });
    println!("{:?}", nested);
    let _ = panic::take_hook();
    println!("done");
}
