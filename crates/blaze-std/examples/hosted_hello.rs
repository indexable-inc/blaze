//! The hosted hello with both sides in one process, for measuring what it executes
//! (crates/blaze-cli/tests/ladder/README.md, "What the hosted hello executes"): the program is
//! the owner's hello under `#[bstd::main]`, exactly as `blaze run` compiles it, and the host is
//! a [`Hosting`] over a simulated world, standing in for the driver inside `blaze`. The host
//! calls the program's `bstd_hosted_start` with its table, as `blaze run` does; a debugger
//! counts every function between that call and its return.

#![feature(macro_attr)]

use bstd::{Io, hosting::Hosting, wire::HostTable};

mod program {
    use bstd::{Cx, entry::Error, out::Out};

    #[bstd::main]
    pub fn main(cx: &Cx, out: Out) -> Result<(), Error> {
        out.line(cx, "Hello, world!")?;
        Ok(())
    }
}

unsafe extern "C" {
    /// The program's hosted start, made by `#[bstd::main]` above.
    fn bstd_hosted_start(table: *const HostTable) -> i32;
}

/// The host's side, outside the measured stretch: a world, the host over it, the call.
#[inline(never)]
fn host(heap: bstd::mem::Heap) -> (i32, Option<Vec<u8>>) {
    let io = Io::sim(1);
    // No grant: `out` fills itself (D104; `Kind::fills_itself`), so the host answers it unasked.
    let hosting = Hosting::new(io.clone(), vec![String::from("hello")], Vec::new(), 4096, heap.shared());
    let table = hosting.table();
    #[expect(unsafe_code, reason = "the host's call of the program's start, as `blaze run` makes it")]
    // SAFETY: `table` is `hosting`'s, which lives across the call: the start's contract.
    let status = unsafe { bstd_hosted_start(&raw const table) };
    (status, io.script(|host| host.out.clone()))
}

fn main() -> bstd::entry::Exit {
    // The host's heap is a grant bstd's entry hands its `main` (D120), as the program's is.
    bstd::entry::run_with_heap(bstd::entry::Os::process(), |_cx, _io, heap| {
        let (status, out) = host(heap);
        let out = out.unwrap_or_default();
        assert_eq!(status, 0, "the hosted hello ends 0");
        assert_eq!(out, b"Hello, world!\n", "the hosted hello writes its line to the host's standard output");
        println!("hosted hello: status {status}, the host's standard output holds {:?}", String::from_utf8_lossy(&out));
    })
}
