//! The hosted hello with both sides in one process, for measuring what it executes
//! (crates/blaze-cli/tests/ladder/README.md, "What the hosted hello executes"): the program is
//! the owner's hello under `#[bstd::main]`, exactly as `blaze run` compiles it, and the host is
//! a [`Hosting`] over a simulated world, standing in for the driver inside `blaze`. The host
//! calls the program's `bstd_hosted_start` with its table, as `blaze run` does; a debugger
//! counts every function between that call and its return.

#![feature(macro_attr)]

use bstd::{Io, hosting::Hosting, wire::HostTable};

mod program {
    use bstd::{Cx, Io, entry::Error};

    #[bstd::main]
    pub fn main(cx: &Cx, io: Io) -> Result<(), Error> {
        io.need_out()?.line(cx, "Hello, world!")?;
        Ok(())
    }
}

unsafe extern "C" {
    /// The program's hosted start, made by `#[bstd::main]` above.
    fn bstd_hosted_start(table: *const HostTable) -> i32;
}

/// The host's side, outside the measured stretch: a world, the host over it, the call.
#[inline(never)]
fn host() -> (i32, Option<Vec<u8>>) {
    let io = Io::sim(1);
    let hosting = Hosting::new(io.clone(), vec![String::from("hello")], vec![(String::from("everything"), String::from("io"))], 4096);
    let table = hosting.table();
    #[expect(unsafe_code, reason = "the host's call of the program's start, as `blaze run` makes it")]
    // SAFETY: `table` is `hosting`'s, which lives across the call: the start's contract.
    let status = unsafe { bstd_hosted_start(&raw const table) };
    (status, io.script(|host| host.out.clone()))
}

fn main() {
    let (status, out) = host();
    let out = out.unwrap_or_default();
    assert_eq!(status, 0, "the hosted hello ends 0");
    assert_eq!(out, b"Hello, world!\n", "the hosted hello writes its line to the host's standard output");
    println!("hosted hello: status {status}, the host's standard output holds {:?}", String::from_utf8_lossy(&out));
}
