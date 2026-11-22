//! x-clo-07: closures returned from functions carry their captures out with them.
//!
//! The Reference, "Impl trait", "Abstract return types": a function returning `impl Fn(u32) ->
//! u32` returns its closure's value (with the captured `k` moved in); `Box<dyn Fn>` returns it
//! behind a vtable.
//! 1. adder(10)(5) = 15; adder(1)(5) = 6 (two closures of one type, each its own k).
//! 2. compose(adder(2), adder(3)) applied to 1: 1 + 2 + 3 = 6.
//! 3. boxed(4, frame)(5) through `Box<dyn Fn, &Frame>`, made in `main`'s root frame and called
//!    through `Deref`: 5 * 4 = 20; boxed(0, frame)(5) = 0.
//! 4. a counter returned as `impl FnMut() -> u32` starting at 100: 101, 102.
//! A wrong builder that leaves the capture on the dead stack frame exits 1 (garbage).

fn adder(k: u32) -> impl Fn(u32) -> u32 {
    move |x| x + k
}

fn compose<F: Fn(u32) -> u32, G: Fn(u32) -> u32>(f: F, g: G) -> impl Fn(u32) -> u32 {
    move |x| g(f(x))
}

/// The closure boxed in the allocator its caller hands down (blaze-own's `Box`, D120): the box
/// outlives this function's frame, its capture with it.
fn boxed(k: u32, frame: &Frame) -> Box<dyn Fn(u32) -> u32, &Frame> {
    Box::new_in(move |x| x * k, frame)
}

fn counter(start: u32) -> impl FnMut() -> u32 {
    let mut n = start;
    move || {
        n += 1;
        n
    }
}

fn main(cx: &Cx) -> std::entry::Status {
    let frame = cx.frame();
    let a10 = adder(10);
    let a1 = adder(1);
    let both = compose(adder(2), adder(3));
    let mut c = counter(100);
    let c1 = c();
    let c2 = c();
    let status = if a10(5) != 15 || a1(5) != 6 {
        1
    } else if both(1) != 6 {
        2
    } else if boxed(4, frame)(5) != 20 || boxed(0, frame)(5) != 0 {
        3
    } else if c1 != 101 || c2 != 102 {
        4
    } else {
        0
    };
    std::entry::Status(status)
}
