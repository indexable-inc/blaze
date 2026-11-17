//! Closure shapes that real crates use, BUILT AND RUN on the new front end (lane CL, census
//! g033201 classes 91 and 66):
//! - core's `Parser::read_separator` (library/core/src/net/parser.rs): `self.read_atomically(move
//!   |p| { if index > 0 { p.read_given_char(sep)?; } inner(p) })`, a `move` closure holding a
//!   `char`, a `usize` and a generic `F: FnOnce`, made in a method of `impl<'a> Parser<'a>`
//!   (class 66: "a capture past its environment's fields");
//! - a closure that captures an `impl Trait` value (gimli's `Context::find_frames` captures an
//!   `impl Iterator`), and one whose signature returns an `async` block (class 91: "a closure
//!   whose facts the world did not answer"; the facts were copied with no layout facts, which
//!   refuse an opaque and a future: blaze-build instance.rs `copy_written` now copies them as
//!   written);
//! - a closure kept in a struct made by another fn, holding or returning an `async` block,
//!   polled by an executor written here;
//! - disjoint fields of one binding used by one closure, one moved and one read, with the
//!   drops observed;
//! - a closure capturing a closure.
//!
//! The rules the programs rest on (the Rust Reference):
//! - "Closure types", "Capture modes": a closure captures each place it uses by the least mode
//!   the use needs; a `move` closure by value. "Capture precision": a closure captures the
//!   place it uses (`a.b`), not the whole binding, so a closure that moves `a.b` out leaves
//!   `a.c` usable after it, and `a.b` is not dropped again with `a`.
//! - "Destructors": a value moved into a closure is dropped with the closure (or, moved out in
//!   the body, where the body's binding goes out of scope); a variable is dropped at the end of
//!   its scope in reverse declaration order.
//! - "Async blocks": an `async` block's value is a future that captures as a closure does.
//!
//! The library is the async twin's (crates/blaze-cli/tests/ladder/front2/x0-async-01: the
//! lang traits, operators, `Option`, `Drop`, and `Future`/`Pin`/`Context`/`Poll` modeled on the
//! pinned library's). The programs are `#![no_core]`, built and run in this process with no
//! toolchain (`NewFront::shelf` is `None`). Each test prints its status; the expected one is
//! worked out in its doc.

#![feature(allocator_api)]

use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{FrontChoice, NewFront, run_with};

const NEW: NewFront<'static> = NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::On };

const PRELUDE: &str = include_str!("../../blaze-cli/tests/ladder/front2/x0-async-01/src/prelude.rs");
const FUTURE: &str = include_str!("../../blaze-cli/tests/ladder/front2/x0-async-01/src/future.rs");

const HEAD: &str = "#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures, arbitrary_self_types)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod future;
mod prelude;
use future::*;
use prelude::*;

/// Pending `left` more times, then ready with `value`.
struct Later {
    left: u32,
    value: u32,
}

impl Future for Later {
    type Output = u32;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<u32> {
        let this = self.pointer;
        if this.left == 0 {
            Poll::Ready(this.value)
        } else {
            this.left = this.left - 1;
            *cx.wakes = *cx.wakes + 1;
            Poll::Pending
        }
    }
}

/// Polls `fut` until it is ready; 1000 when it is not ready after 64 polls.
fn block_on<F: Future<Output = u32>>(fut: F) -> u32 {
    let mut fut = fut;
    let mut wakes = 0u32;
    let mut cx = Context { wakes: &mut wakes };
    let mut polls = 0u32;
    loop {
        if polls == 64 {
            return 1000;
        }
        polls = polls + 1;
        let pinned = Pin { pointer: &mut fut };
        match <F as Future>::poll(pinned, &mut cx) {
            Poll::Ready(v) => return v,
            Poll::Pending => {}
        }
    }
}
";

/// Builds and runs `HEAD` + `main` with the async twin's library: the status, or the refusal
/// on one line.
fn run(main: &str) -> Result<u8, String> {
    let text = [HEAD, main].concat();
    let files = [
        SourceFile { path: "src/main.rs", text: &text },
        SourceFile { path: "src/prelude.rs", text: PRELUDE },
        SourceFile { path: "src/future.rs", text: FUTURE },
    ];
    run_with(&files, "src/main.rs", None, FrontChoice::New(NEW), blaze_own::testing::heap().shared()).map_err(|e| format!("{e}").replace('\n', " "))
}

/// core's `read_separator` shape: in `impl<'a> Parser<'a>`, a `move` closure holding `sep:
/// char`, `index: usize` and `inner: F` (`F: FnOnce(&mut Parser<'_>) -> Option<T>`), passed to
/// `read_atomically`, at `T = u32` and `F` a closure of `main`. `index` 1 reads the separator
/// (`pos` 0 to 1), then `inner` reads `*pos + 40` = 41; `index` 0 skips it: `*pos + 40` = 41
/// again (pos stays 1). 41 + 41 = 82.
#[test]
fn read_separator_shape_runs() {
    let got = run("
struct Parser<'a> {
    pos: &'a mut u32,
    text: &'a [char; 2],
}

impl<'a> Parser<'a> {
    fn read_atomically<T, F: FnOnce(&mut Parser<'_>) -> Option<T>>(&mut self, inner: F) -> Option<T> {
        inner(self)
    }

    fn read_given_char(&mut self, target: char) -> Option<()> {
        let at = *self.pos;
        if at < 2 && self.text[at as usize] == target {
            *self.pos = at + 1;
            Some(())
        } else {
            None
        }
    }

    fn read_separator<T, F>(&mut self, sep: char, index: usize, inner: F) -> Option<T>
    where
        F: FnOnce(&mut Parser<'_>) -> Option<T>,
    {
        self.read_atomically(move |p| {
            if index > 0 {
                match p.read_given_char(sep) {
                    Some(()) => {}
                    None => return None,
                }
            }
            inner(p)
        })
    }
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let text = [',', 'x'];
    let mut pos = 0u32;
    let mut p = Parser { pos: &mut pos, text: &text };
    let first = match p.read_separator(',', 1, |q: &mut Parser<'_>| Some(*q.pos + 40)) {
        Some(v) => v,
        None => 1,
    };
    let second = match p.read_separator(',', 0, |q: &mut Parser<'_>| Some(*q.pos + 40)) {
        Some(v) => v,
        None => 2,
    };
    (first + second) as i32
}
");
    println!("READ SEPARATOR: {got:?}");
    assert_eq!(got, Ok(82));
}

/// A closure capturing an `impl Trait` value, in a plain fn and in a generic one (the opaque
/// is the closure's environment's field: its facts copy it as written). `twice(|| c.count() +
/// 1)` = 2 * (3 + 1) = 8; `wrap(Three)` = 2 * (3 + 3) = 12. 8 + 12 = 20.
#[test]
fn a_closure_capturing_an_opaque_runs() {
    let got = run("
trait Count {
    fn count(&self) -> u32;
}

struct Three;

impl Count for Three {
    fn count(&self) -> u32 {
        3
    }
}

fn make() -> impl Count {
    Three
}

fn twice<F: Fn() -> u32>(f: F) -> u32 {
    f() + f()
}

fn wrap<C: Count>(c: C) -> u32 {
    let it = make();
    twice(move || it.count() + c.count())
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let c = make();
    (twice(|| c.count() + 1) + wrap(Three)) as i32
}
");
    println!("OPAQUE CAPTURE: {got:?}");
    assert_eq!(got, Ok(20));
}

/// Closures and `async` blocks kept in a struct made by another fn:
/// - `build(move |x| async move { Later { left: 1, value: x }.await + base })` (the closure's
///   signature returns the block's future), driven by `run_holder`: (20 + 5) + extra 1 = 26;
/// - `holder(7)` returns a `Holder<impl Fn(u32) -> u32>` whose closure holds an `async` block
///   and drives it itself: (30 + 7) + extra 2 = 39.
/// 26 + 39 = 65.
#[test]
fn a_kept_closure_with_an_async_block_runs() {
    let got = run("
struct Holder<F> {
    f: F,
    extra: u32,
}

fn build<F, Fut>(f: F) -> Holder<F>
where
    F: Fn(u32) -> Fut,
    Fut: Future<Output = u32>,
{
    Holder { f, extra: 1 }
}

fn run_holder<F, Fut>(h: Holder<F>) -> u32
where
    F: Fn(u32) -> Fut,
    Fut: Future<Output = u32>,
{
    block_on((h.f)(20)) + h.extra
}

fn holder(base: u32) -> Holder<impl Fn(u32) -> u32> {
    Holder { f: move |x: u32| block_on(async move { Later { left: 2, value: x }.await + base }), extra: 2 }
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let base = 5u32;
    let first = run_holder(build(move |x: u32| async move { Later { left: 1, value: x }.await + base }));
    let h = holder(7);
    let second = (h.f)(30) + h.extra;
    (first + second) as i32
}
");
    println!("KEPT ASYNC: {got:?}");
    assert_eq!(got, Ok(65));
}

/// Disjoint fields of one binding in one closure. `D` appends its id to a log (`log * 10 +
/// id`) when dropped.
/// - `|| a.b.id + a.c.y` borrows both fields: 1 + 5 = 6, nothing dropped by the call.
/// - `move || { let taken = a.b; taken.id + 10 }` moves `a.b` in (edition 2021 and later
///   capture the place `a.b`; a whole-binding capture of `a` holds the same `D`, and `a.c`
///   holds none, so the drops are the same), called once: 11, and `taken` (D 1) is dropped
///   when the body ends: log 1. At the end of `scene` `other` (D 2) is dropped: log 12, and D 1
///   is never dropped a second time.
/// Status: 6 + 11 = 17 from the reads, times 100, plus the log 12 = 1712 (mod 256: 176).
#[test]
fn disjoint_field_captures_drop_once() {
    let got = run("
struct D {
    log: *mut u32,
    id: u32,
}

impl Drop for D {
    fn drop(&mut self) {
        unsafe {
            *self.log = *self.log * 10 + self.id;
        }
    }
}

struct Inner {
    y: u32,
}

struct Pair {
    b: D,
    c: Inner,
}

fn scene(log: *mut u32) -> u32 {
    let other = D { log, id: 2 };
    let a = Pair { b: D { log, id: 1 }, c: Inner { y: 5 } };
    let read = || a.b.id + a.c.y;
    let both = read();
    let take = move || {
        let taken = a.b;
        taken.id + 10
    };
    let moved = take();
    let _keep = &other;
    both + moved
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let mut log = 0u32;
    let reads = scene(&mut log);
    (reads * 100 + log) as i32
}
");
    println!("DISJOINT CAPTURE: {got:?}");
    assert_eq!(got, Ok((1712u32 % 256) as u8));
}

/// An `Fn` closure called directly in a program where no bound names `Fn`: the builder calls
/// it as `Fn::call(&read, ())` (the Reference, "Call expressions"), so the trait's fn is asked
/// for by the walk itself (front2 back_needs.rs), not only by a bound. `k + 1` = 4. Before,
/// "needs the `call` lang item fn, which the program does not declare".
#[test]
fn a_closure_called_directly_runs() {
    let got = run("
#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let k = 3u32;
    let read = || k + 1;
    read() as i32
}
");
    println!("DIRECT CALL: {got:?}");
    assert_eq!(got, Ok(4));
}

/// A closure capturing a closure: `outer` borrows `inner` (which borrows `k`), `again` moves
/// `inner` in. `outer(4)` = (4 + 3) * 2 = 14; `again(1)` = (1 + 3) + 1 = 5; and an `FnOnce`
/// chain: `last` moves `once`, which moves a `D`-free `Holder` value out: `once()` = 9, called
/// through `last()` + 1 = 10. 14 + 5 + 10 = 29.
#[test]
fn a_closure_capturing_a_closure_runs() {
    let got = run("
struct Boxed {
    v: u32,
}

fn call_once<F: FnOnce() -> u32>(f: F) -> u32 {
    f()
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let k = 3u32;
    let inner = |x: u32| x + k;
    let outer = |y: u32| inner(y) * 2;
    let first = outer(4);
    let again = move |y: u32| inner(y) + 1;
    let second = again(1);
    let held = Boxed { v: 9 };
    let once = move || {
        let b = held;
        b.v
    };
    let last = move || call_once(once) + 1;
    let third = call_once(last);
    (first + second + third) as i32
}
");
    println!("CLOSURE OF A CLOSURE: {got:?}");
    assert_eq!(got, Ok(29));
}
