//! A closure value's drop glue asks for no `Drop` impl of the closure itself (walkAJ-P3).
//!
//! The fault this guards (2026-11-16, port27/front2/walk-AJ-briefs.md "P3", 15 refusals in the
//! walk of the owner's hello world: `call_once_force`, `each`, `drop`): run.rs `drop_impl`
//! asked the checker's solver `selects(drop, ty)` for every aggregate laid out, a closure type
//! included; selection imports its arguments, and importing a closure type needs its owner's
//! checked record, which the run path's world does not hold: "selecting a `Drop` impl:
//! resolving an instance: blaze-typeck: malformed input: BodyTypeInRecord(Closure(..))".
//!
//! The rule: only a struct, enum or union can implement `Drop` (E0120: "the Drop trait may
//! only be implemented for structs, enums, and unions"); the Reference, "Destructors": "If T
//! implements Drop ... then ... the destructor of T", and a closure drops only its captures;
//! a `Drop` impl covers every instance of its ADT (E0366/E0367). So `drop_impl` answers by the
//! ground type's head: a closure has none, without the solver; an ADT's is found in its own
//! bucket (blaze-typeck drop_impl.rs `adt_drop_impl`), its arguments never imported.
//! Needs no toolchain: the programs are `#![no_core]` (setup as tests/front2_survey.rs).
//!
//! WHICH TEST IS THE CONTROL (walkAJ-P3-3, rule 23). blaze-build asks `Adts::drop_impl` only
//! of an ADT (walk.rs and drop_facts.rs, inside their `PNode::Adt` arms), so [`PROGRAM`]'s bare
//! closure never reaches it; and a closure inside an ADT is imported since the front2 world
//! answers any checked body's closures (front2/foreign_closures.rs). What the head-first
//! answer alone decides is an ADT over an async future: selection imports every argument, and
//! import refuses a future (blaze-typeck instance.rs: `Asked` inherits `future_of`, which returns
//! `Malformed::InstanceImportFuture { block }`; a `Coroutine` is `BodyTypeInRecord`). [`ASYNC_MAIN`] holds one; its test
//! fails with "selecting a `Drop` impl:" when run.rs `drop_impl` selects instead.

#![feature(allocator_api)]

use std::alloc::Global;

use blaze_ladder::crates::{Inputs, Toolchain};
use blaze_ladder::front2::program::Front2Program;
use blaze_ladder::program_texts::ProgramTexts;
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{FrontChoice, NewFront, build_survey, run_with};

/// `main` makes a `move` closure capturing a `G`, which has a `Drop` impl, and never calls it:
/// at `main`'s end the closure is dropped, so its drop glue is built, and that glue drops the
/// captured `G` through `G`'s impl. The lang items are core's (library/core/src/ops/drop.rs
/// `#[lang = "drop"]`, ptr/mod.rs `#[lang = "drop_in_place"]`, ops/function.rs `fn_once`,
/// `fn_mut`, `fn`), as in the ladder's no_core closure rungs (x0-clo-09's prelude).
const PROGRAM: &str = "#![feature(no_core, lang_items, unboxed_closures, rustc_attrs)]
#![no_core]
#![no_main]
#![allow(internal_features)]

#[lang = \"pointee_sized\"]
trait PointeeSized {}

#[lang = \"meta_sized\"]
trait MetaSized: PointeeSized {}

#[lang = \"sized\"]
trait Sized: MetaSized {}

#[lang = \"copy\"]
trait Copy {}

#[lang = \"drop\"]
trait Drop {
    fn drop(&mut self);
}

#[lang = \"drop_in_place\"]
unsafe fn drop_in_place<T: PointeeSized>(to_drop: *mut T) {
    unsafe { drop_in_place(to_drop) }
}

#[lang = \"tuple_trait\"]
trait Tuple {}

#[lang = \"fn_once\"]
#[rustc_paren_sugar]
trait FnOnce<Args: Tuple> {
    #[lang = \"fn_once_output\"]
    type Output;
    extern \"rust-call\" fn call_once(self, args: Args) -> Self::Output;
}

#[lang = \"fn_mut\"]
#[rustc_paren_sugar]
trait FnMut<Args: Tuple>: FnOnce<Args> {
    extern \"rust-call\" fn call_mut(&mut self, args: Args) -> Self::Output;
}

#[lang = \"fn\"]
#[rustc_paren_sugar]
trait Fn<Args: Tuple>: FnMut<Args> {
    extern \"rust-call\" fn call(&self, args: Args) -> Self::Output;
}

struct G(i32);

impl Drop for G {
    fn drop(&mut self) {}
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let g = G(1);
    let _c = move || {
        let _x = &g;
    };
    0
}
";

/// `main` holds an unpolled `async fn` future in `Held`, a generic struct with no `Drop` impl,
/// and drops it at its end: `Held<{future of ready}>` is laid out, so its `Drop` impl is asked
/// (walk.rs `drop_fn`) and whether it needs drop (drop_facts.rs `needs_drop`, whose ADT arm asks
/// the same before reading the fields). The Reference, "Destructors": a value is dropped at
/// the end of its scope by its type's destructor; "Async blocks": an async block's future that
/// was never polled drops what it captured when it is dropped. The async lang items are the
/// rung x0-async-01's (its prelude.rs and future.rs: core's `future_trait`, `Poll`, `Context`,
/// `pin`, `coroutine` items), included as written.
const ASYNC_MAIN: &str = "#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures, arbitrary_self_types)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod future;
mod prelude;
use future::*;
use prelude::*;

struct G(u32);

impl Drop for G {
    fn drop(&mut self) {}
}

/// Holds its value; no `Drop` impl of its own.
struct Held<F>(F);

async fn ready(g: G) -> u32 {
    g.0
}

/// Holds `f` in `Held` and drops it at its end: `Held<F>` is laid out at `F` = the future.
fn keep<F>(f: F) -> i32 {
    let _h = Held(f);
    0
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    keep(ready(G(1)))
}
";

/// The rung x0-async-01's no_core prelude and future items (crates/blaze-cli/tests/ladder).
const ASYNC_PRELUDE: &str = include_str!("../../blaze-cli/tests/ladder/front2/x0-async-01/src/prelude.rs");
const ASYNC_FUTURE: &str = include_str!("../../blaze-cli/tests/ladder/front2/x0-async-01/src/future.rs");

/// Surveys the no_core program `files` (its root `src/main.rs`): the refused rows and the
/// counts, after asserting front2 tabled every fn.
fn survey(files: &[SourceFile<'_>]) -> (Vec<String>, blaze_ladder::run::SurveyCounts) {
    let tc = Toolchain::none(blaze_own::Alloc::global());
    let new = NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::On };
    let texts = ProgramTexts::new(files, "src/main.rs", new, &tc, blaze_build::Panic::Unwind).unwrap_or_else(|e| panic!("the program's texts: {e}"));
    let user = texts.user();
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the survey's frame");
    let mut left_out = Vec::new_in(Global);
    let mut program = Front2Program::survey(&names, &frame, files, &mut left_out, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("BUILDS: no survey: {e}"));
    let left: Vec<String> = left_out.iter().map(|(f, class)| format!("{f}: {class}")).collect();
    assert!(left.is_empty(), "front2 tables every fn of the program; left out: {left:?}");
    let mut refused = Vec::new_in(Global);
    let counts = build_survey(&mut program, &mut refused, blaze_build::OverflowChecks::On, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("BUILDS: the survey stopped: {e}"));
    let rows: Vec<String> = refused.iter().map(|(f, class)| format!("{f}: {class}")).collect();
    println!("BUILDS: {} of {} instances ({} refused: {})", counts.built, counts.tried, rows.len(), rows.join("; "));
    (rows, counts)
}

/// The survey of [`PROGRAM`] refuses nothing: the closure's drop glue is built with no `Drop`
/// lookup for the closure type, and `G`'s impl is found by `G`'s head. NOT edit 5's control
/// (see the module doc): it passes with run.rs `drop_impl` selecting too.
///
/// Prints: `BUILDS: n of n instances (0 refused: )` and
/// `test a_closure_value_has_no_drop_impl_lookup ... ok`.
#[test]
fn a_closure_value_has_no_drop_impl_lookup() {
    let files = [SourceFile { path: "src/main.rs", text: PROGRAM }];
    let (rows, counts) = survey(&files);
    assert!(rows.is_empty(), "a closure's drop glue asks no `Drop` impl of the closure: {rows:?}");
    assert!(counts.tried > 0, "the survey built `main` at least");
    assert_eq!(counts.built, counts.tried, "every instance built: {rows:?}");
}

/// The survey of [`ASYNC_MAIN`] refuses nothing: `Held<{future}>`'s `Drop` impl is looked up
/// in `Held`'s own bucket (none), the future never imported; the future's own drop is its
/// coroutine's (drop_facts.rs: `PNode::Future` needs drop, no impl asked).
///
/// Prints: `BUILDS: n of n instances (0 refused: )` and
/// `test an_adt_over_a_future_asks_its_drop_impl_by_its_head ... ok`.
/// Without run.rs `drop_impl`'s head-first answer (selecting `Drop` at `Held<{future}>`): a
/// refusal "selecting a `Drop` impl: .." from importing the future (blaze-typeck instance.rs
/// `import`: `Malformed::InstanceImportFuture { block }` for `PNode::Future`, `BodyTypeInRecord(Coroutine(` for
/// `PNode::Coroutine`).
#[test]
fn an_adt_over_a_future_asks_its_drop_impl_by_its_head() {
    let files = [
        SourceFile { path: "src/main.rs", text: ASYNC_MAIN },
        SourceFile { path: "src/prelude.rs", text: ASYNC_PRELUDE },
        SourceFile { path: "src/future.rs", text: ASYNC_FUTURE },
    ];
    let (rows, counts) = survey(&files);
    assert!(rows.is_empty(), "an ADT over a future asks its `Drop` impl by its head, importing nothing: {rows:?}");
    assert!(counts.tried > 0, "the survey built `main` at least");
    assert_eq!(counts.built, counts.tried, "every instance built: {rows:?}");
}

/// Shared `#![no_core]` async harness for the P8 coroutine suspension-slot regression tests:
/// `Later` suspends `left` polls before returning `value`, `block_on` drives a future to
/// completion, and `DROPS` counts destructor runs.
const P8_HEAD: &str = "#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures, arbitrary_self_types)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod future;
mod prelude;
use future::*;
use prelude::*;

static mut DROPS: u32 = 0;

fn drops() -> u32 {
    unsafe { DROPS }
}

fn bump_drop() {
    unsafe {
        DROPS = DROPS + 1;
    }
}

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

fn block_on<F: Future<Output = u32>>(mut fut: F) -> u32 {
    let mut wakes = 0u32;
    let mut cx = Context { wakes: &mut wakes };
    let mut polls = 0u32;
    loop {
        if polls == 64 {
            return 250;
        }
        polls = polls + 1;
        let pinned = Pin { pointer: &mut fut };
        match Future::poll(pinned, &mut cx) {
            Poll::Ready(v) => return v,
            Poll::Pending => {}
        }
    }
}
";

fn run_p8(body: &str) -> u8 {
    let main_src = format!("{P8_HEAD}{body}");
    let files = [
        SourceFile { path: "src/main.rs", text: &main_src },
        SourceFile { path: "src/prelude.rs", text: ASYNC_PRELUDE },
        SourceFile { path: "src/future.rs", text: ASYNC_FUTURE },
    ];
    let new = NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::On };
    // First survey under `Panic::Unwind` (every call in an async body is an `invoke` with cleanup).
    let (rows, counts) = survey(&files);
    assert!(rows.is_empty(), "P8 survey refused: {rows:?}");
    assert_eq!(counts.built, counts.tried, "P8 survey missed instances: {rows:?}");
    run_with(&files, "src/main.rs", None, FrontChoice::New(new), blaze_own::testing::heap().shared())
        .unwrap_or_else(|e| panic!("P8 run refused: {e}"))
}

/// Blocker #1A (`zone-node` `settlement_rejects_nonconsecutive_zone_batch_indices`):
/// passing a temporary `[u64; 2]` array (Pass::Ref) and `&str` to an `async fn` before `.await`,
/// destructuring a 4-tuple with a dropping `_` field, binding a fresh aggregate via `let`, and
/// borrowing both copy and dropping temporaries before `.await`.
#[test]
fn tempo_p8_array_arg_and_fresh_let_across_await() {
    let body = "
struct Guard { tag: u32, extra: u64 }
impl Drop for Guard {
    fn drop(&mut self) { bump_drop(); }
}

struct BigCopy { a: u64, b: u64, c: u64 }

#[inline(never)]
fn make_copy(x: u64) -> BigCopy { BigCopy { a: x, b: x + 1, c: x + 2 } }

#[inline(never)]
fn read_copy(v: &BigCopy) -> u32 { (v.a + v.b + v.c) as u32 }

#[inline(never)]
fn read_guard(g: &Guard) -> u32 { g.tag }

#[inline(never)]
fn fixture(indices: [u64; 2]) -> (Guard, Guard, Guard, BigCopy) {
    (
        Guard { tag: indices[0] as u32, extra: 1 },
        Guard { tag: indices[1] as u32, extra: 2 },
        Guard { tag: 99, extra: 3 },
        BigCopy { a: indices[0], b: indices[1], c: 4 },
    )
}

#[inline(never)]
fn fresh_agg(n: u64) -> Guard { Guard { tag: n as u32, extra: n } }

#[inline(never)]
fn touch_str(_s: &str) -> u32 { 3 }

async fn settlement(indices: [u64; 2], number: u64, expected: &str) -> u32 {
    let (provider, context, _, header) = fixture(indices);
    let fresh = fresh_agg(number);
    let csum = read_copy(&make_copy(2));
    let gsum = read_guard(&Guard { tag: 5, extra: 0 });
    let waited = Later { left: 2, value: 10 }.await;
    provider.tag + context.tag + fresh.tag + header.c as u32 + csum + gsum + waited + touch_str(expected)
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let sum = block_on(async {
        let res = settlement([2u64, 3u64], 1u64, \"err\").await;
        res
    });
    // provider(2) + context(3) + fresh(1) + header.c(4) + csum(9) + gsum(5) + waited(10) + len(3) = 37
    // Guards dropped: `_` (1) + `&Guard` temp (1) + provider (1) + context (1) + fresh (1) = 5.
    if sum == 37 && drops() == 5 { 0 } else { (sum * 10 + drops()) as i32 }
}
";
    assert_eq!(run_p8(body), 0);
}

/// Blocker #1B (`zone-sequencer` x2 `for (before, after) in TRANSITIONS { ... .await ... }`):
/// single-arm whole-binding `match` over a `Drop` iterator (the desugaring of `for`), plus an
/// inner `match` over a >16-byte `Copy` `Option<(&str, u64)>` (`sret`) with `.await` inside.
#[test]
fn tempo_p8_for_loop_over_drop_iter_and_copy_option_across_await() {
    let body = "
const TRANSITIONS: [(u64, u64); 2] = [(1, 2), (2, 3)];

struct PairIter {
    items: [(u64, u64); 2],
    idx: usize,
}
impl Drop for PairIter {
    fn drop(&mut self) { bump_drop(); }
}
impl PairIter {
    #[inline(never)]
    fn from_arr(items: [(u64, u64); 2]) -> PairIter { PairIter { items, idx: 0 } }
    #[inline(never)]
    fn next(&mut self) -> Option<(u64, u64)> {
        if self.idx < 2 {
            let v = self.items[self.idx];
            self.idx = self.idx + 1;
            Option::Some(v)
        } else {
            Option::None
        }
    }
}

struct StrPairIter {
    items: [(&'static str, u64); 2],
    idx: usize,
}
impl Drop for StrPairIter {
    fn drop(&mut self) { bump_drop(); }
}
impl StrPairIter {
    #[inline(never)]
    fn from_arr(items: [(&'static str, u64); 2]) -> StrPairIter { StrPairIter { items, idx: 0 } }
    #[inline(never)]
    fn next(&mut self) -> Option<(&'static str, u64)> {
        if self.idx < 2 {
            let v = self.items[self.idx];
            self.idx = self.idx + 1;
            Option::Some(v)
        } else {
            Option::None
        }
    }
}

#[inline(never)]
fn touch_addr(_s: &str) -> u32 { 3 }

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let total = block_on(async {
        let mut acc = 0u32;
        match PairIter::from_arr(TRANSITIONS) {
            mut iter => loop {
                match iter.next() {
                    Option::Some((before, after)) => {
                        match StrPairIter::from_arr([(\"old\", before), (\"new!\", after)]) {
                            mut inner => loop {
                                match inner.next() {
                                    Option::Some((addr, fork)) => {
                                        let w = Later { left: 1, value: fork as u32 }.await;
                                        acc = acc + w + touch_addr(addr);
                                    }
                                    Option::None => break,
                                }
                            },
                        }
                    }
                    Option::None => break,
                }
            },
        }
        acc
    });
    // Iter 1: (1+3) + (2+3) = 9; Iter 2: (2+3) + (3+3) = 11 => total = 20.
    // Drops: 1 outer PairIter + 2 inner StrPairIter = 3.
    if total == 20 && drops() == 3 { 0 } else { (total * 10 + drops()) as i32 }
}
";
    assert_eq!(run_p8(body), 0);
}

/// Blocker #2 (`zone-rpc` `handle_call` / `handle_estimate_gas`):
/// a by-value `Drop` parameter (upvar of the `async fn` root block) conditionally moved before
/// `.await` needs its 1-byte drop flag laid in the coroutine state; verifies early move, live
/// use after `.await`, cancel while suspended, non-returning conditional move, and unresumed drop.
#[test]
fn tempo_p8_by_value_drop_upvar_conditionally_moved_before_await() {
    let body = "
struct Val { n: u32, pad: u64 }
impl Drop for Val {
    fn drop(&mut self) { bump_drop(); }
}

#[inline(never)]
fn consume(v: Val) -> u32 { v.n }

async fn handle(id: Val, early: bool) -> u32 {
    if early {
        return consume(id);
    }
    let r = Later { left: 2, value: 10 }.await;
    consume(id) + r
}

async fn maybe_drop_before_await(id: Val, drop_early: bool) -> u32 {
    if drop_early {
        drop(id);
    }
    Later { left: 1, value: 7 }.await
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let a = block_on(handle(Val { n: 5, pad: 0 }, true));
    let d1 = drops();
    let b = block_on(handle(Val { n: 6, pad: 0 }, false));
    let d2 = drops();
    // Poll once (Pending) then drop the suspended future: cancel path drops `id` via its upvar flag.
    {
        let mut fut = handle(Val { n: 7, pad: 0 }, false);
        let mut wakes = 0u32;
        let mut cx = Context { wakes: &mut wakes };
        let pinned = Pin { pointer: &mut fut };
        match Future::poll(pinned, &mut cx) {
            Poll::Pending => {}
            Poll::Ready(_) => return 91,
        }
    }
    let d3 = drops();
    // Non-returning conditional move before `.await`: dropped once in the `if`, not again after `.await`.
    let c = block_on(maybe_drop_before_await(Val { n: 8, pad: 0 }, true));
    let d4 = drops();
    let d = block_on(maybe_drop_before_await(Val { n: 9, pad: 0 }, false));
    let d5 = drops();
    if a == 5 && d1 == 1 && b == 16 && d2 == 2 && d3 == 3 && c == 7 && d4 == 4 && d == 7 && d5 == 5 {
        0
    } else {
        1
    }
}
";
    assert_eq!(run_p8(body), 0);
}

/// Blocker #3 (`tempo-consensus` `verifications_queue_per_round_and_builds_keep_their_own_slot`):
/// a 0-suspend `async move` block capturing a `Drop` value and holding local `Drop` structs across
/// calls under `Panic::Unwind` (every call is an `invoke` with a `Cleanup` landing block).
#[test]
fn tempo_p8_zero_suspend_async_move_with_unwind_cleanup() {
    let body = "
struct Ctx { seed: u32, pad: u64 }
impl Drop for Ctx {
    fn drop(&mut self) { bump_drop(); }
}

struct Actor { total: u32, pad: u64 }
impl Drop for Actor {
    fn drop(&mut self) { bump_drop(); }
}

struct Item { v: u32, pad: u64 }
impl Drop for Item {
    fn drop(&mut self) { bump_drop(); }
}

#[inline(never)]
fn init_actor(ctx: Ctx) -> Actor { Actor { total: ctx.seed, pad: 0 } }

#[inline(never)]
fn make_item(v: u32) -> Item { Item { v, pad: 0 } }

#[inline(never)]
fn push_item(actor: &mut Actor, item: Item) { actor.total = actor.total + item.v; }

fn start<F: Future<Output = u32>>(f: F) -> u32 { block_on(f) }

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let ctx = Ctx { seed: 10, pad: 0 };
    let res = start(async move {
        let mut actor = init_actor(ctx);
        push_item(&mut actor, make_item(2));
        push_item(&mut actor, make_item(3));
        actor.total
    });
    // Drops: ctx(1) + item(2) + item(3) + actor(1) = 4.
    if res == 15 && drops() == 4 { 0 } else { (res * 10 + drops()) as i32 }
}
";
    assert_eq!(run_p8(body), 0);
}
