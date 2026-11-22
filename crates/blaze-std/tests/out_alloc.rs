//! What a line costs in allocations: none. A test binary of its own, because the counting
//! allocator must be this binary's global one (`bstd::test::installed` says so before the
//! count is read, so a zero is never a missing counter).

use bstd::{
    entry::{MAIN_BUDGET, MAIN_SCRATCH},
    io::{Io, Limits, SimWorld},
};

#[global_allocator]
static ALLOC: bstd::test::Counting = bstd::test::Counting;

/// 1,000 `line(cx, format_args!("{} {}", n, s))` calls make no allocation: the line is
/// formatted on the stack (`out::Held`) and appended to the buffer inline in the world, and
/// `core::fmt` of an integer and a `&str` allocates nothing. The world is made first, with a
/// trace that keeps no records and a host whose standard output was reserved, so neither the
/// recorder nor the simulated host's own `Vec` is what is counted. Prints
/// `1000 lines: 0 allocations`.
#[test]
fn a_thousand_formatted_lines_allocate_nothing() {
    assert!(bstd::test::installed(), "the counting allocator is this binary's");
    let mut world = SimWorld::new(1, Limits::USUAL);
    world.limits = Limits { trace_most: 0, ..Limits::USUAL };
    let io = Io::simulated(world);
    io.script(|host| host.out.reserve(1 << 16)).expect("a simulated world");
    let cx = io.cx(MAIN_BUDGET, MAIN_SCRATCH).expect("a root context");
    let out = io.out().expect("Io::sim offers out");
    let s = "items";
    let ((), used) = bstd::test::measure(|| {
        for n in 0..1000u64 {
            out.line(&cx, format_args!("{} {}", n, s)).unwrap();
        }
    });
    println!("1000 lines: {} allocations", used.allocations);
    assert_eq!(used.allocations, 0, "{used:?}");
    let said = io.script(|host| host.out.clone()).expect("a simulated world");
    let expected: String = (0..1000u64).map(|n| format!("{n} {s}\n")).collect();
    assert_eq!(said, expected.as_bytes());
}
