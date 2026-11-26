//! A trait fn's `impl Trait` (and an `async fn`'s future) RUN at the impl the call selects,
//! BUILT AND RUN on the new front end. They pin a run-time bug, the code generator's half of
//! front2 rpitit.rs; the fix below is written (front2/coroutines.rs `opaque_hidden` and
//! `opaque_at_impl`, rpitit.rs `place_in`), not yet run.
//!
//! The rules (RFC 3425, "return position `impl Trait` in traits"; the Reference,
//! "Associated items"): a trait fn's `impl Trait` is an associated type of the trait, and its
//! value at a type is that type's impl's. An impl that writes the fn gives its own body's
//! type. One that does not, of a PROVIDED fn, takes the default body's type. A generic caller
//! `call<T: Tr>(t: &T)` holds `<T as Tr>::{f's type}`, which at `T = B` is B's impl's type,
//! never the default body's.
//!
//! The bug: front2/coroutines.rs `opaque_hidden` reveals an opaque from its DEFINING fn's
//! check record (`program.opaque_of`: program.rs:1764, program/lazy/front_end.rs:504). For a
//! trait fn's opaque, the defining fn is the trait fn. So:
//! - a provided fn's default body's type is taken at EVERY self type. At `T = B`, B's override
//!   is called, but its result is laid out as the default body's (expected 1 then 2; today 1
//!   then wrong: a miscompile or a crash, by reading, not run);
//! - a required fn has no body, so it is refused ("an `impl Trait` type whose defining fn has
//!   no checked body").
//!
//! The fix (written): `opaque_hidden` selects the impl fn at the ground arguments by the seam's
//! `resolve`, the call's own selection, so the layout and the call agree. A selected impl fn
//! gives the type at the opaque's place of its return type (rpitit.rs `place_in`, the walk of
//! the impl's associated-type row), grounded at the impl fn's arguments: its own opaque is then
//! revealed from its own record. A fn of no trait, and a provided fn the impl does not write,
//! resolve to the owner: its own record, as before (the default, rpitit.rs `Twin`).
//!
//! The library is the async twin's (crates/blaze-cli/tests/ladder/front2/x0-async-01), as in
//! tests/front2_closure_shapes_run.rs, whose harness this follows: `#![no_core]`, built and run
//! in this process with no toolchain. Each test prints its status.
//!
//! Run 2026-11-21 05:56: the selection fix made the provided `impl Next` test and the negative
//! twin pass. The provided async test then stopped at the BUILD, not the reveal: `t.f().await`
//! awaits a METHOD call's future, and blaze-build walk.rs `is_fresh` counts a call as building
//! its value in a slot of its own but not a method call, so the await copies the result and
//! leaves the call's own slot unended across the suspension (async-P8-4's refusal). That gap is
//! its own pair below (`a_method_calls_future_awaited_in_place_is_no_unnamed_slot`); the reveal
//! tests call the trait fn by path (`Tr::f(t)`, a call), which is the same reveal of `<T as
//! Tr>::{f's future}` at `T = A` and `T = B`. The required test stopped at the reveal itself:
//! coroutines.rs asked the selection in the world of the trait fn's own row, a declared head
//! with no body, whose world is the ctor world's refusal `Missing(FnSig(Req::f))`; the selection
//! is now asked in a body's world (NOT YET RUN).

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

/// Polls `fut` until it is ready; 200 when it is not ready after 64 polls.
fn block_on<F: Future<Output = u32>>(fut: F) -> u32 {
    let mut fut = fut;
    let mut wakes = 0u32;
    let mut cx = Context { wakes: &mut wakes };
    let mut polls = 0u32;
    loop {
        if polls == 64 {
            return 200;
        }
        polls = polls + 1;
        let pinned = Pin { pointer: &mut fut };
        match <F as Future>::poll(pinned, &mut cx) {
            Poll::Ready(v) => return v,
            Poll::Pending => {}
        }
    }
}

/// An iterator's one method, the program's own (the library has no `Iterator`).
trait Next {
    fn next(&mut self) -> Option<u32>;
}

/// Yields `left` once.
struct Once {
    left: Option<u32>,
}

impl Next for Once {
    fn next(&mut self) -> Option<u32> {
        match self.left {
            Some(v) => {
                self.left = None;
                Some(v)
            }
            None => None,
        }
    }
}

/// Yields `v` forever, laid out unlike `Once` (a `u64` beside it): a layout taken from the
/// wrong type reads the wrong bytes.
struct Repeat {
    pad: u64,
    v: u32,
}

impl Next for Repeat {
    fn next(&mut self) -> Option<u32> {
        self.pad = self.pad + 1;
        Some(self.v)
    }
}

/// The first item, 100 when there is none.
fn first<N: Next>(n: N) -> u32 {
    let mut n = n;
    match n.next() {
        Some(v) => v,
        None => 100,
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

/// A PROVIDED `async fn`: `A` keeps the default body (ready with 1); `B` overrides it with a
/// body that awaits `Later` (a different future, laid out unlike the default's) and gives 2.
/// Through the generic `call`, `call(&A)` is 1 and `call(&B)` is 2: status 1 + 10 * 2 = 21.
/// Before the selection fix, `call(&B)`'s future was laid out as the default body's. `call`
/// awaits `Tr::f(t)` (a call by path, the same reveal as `t.f()`): the method-call form's own
/// build gap is the pair below.
#[test]
fn a_provided_async_fn_runs_the_overriding_impls_future_through_a_generic() {
    let got = run("
trait Tr {
    async fn f(&self) -> u32 {
        1
    }
}

struct A;
impl Tr for A {}

struct B;
impl Tr for B {
    async fn f(&self) -> u32 {
        let v = Later { left: 1, value: 2 }.await;
        v
    }
}

async fn call<T: Tr>(t: &T) -> u32 {
    Tr::f(t).await
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let a = block_on(call(&A));
    let b = block_on(call(&B));
    (a + 10 * b) as i32
}
");
    println!("PROVIDED ASYNC FN: {got:?}");
    assert_eq!(got, Ok(21));
}

/// A PROVIDED fn returning `impl Next`: the default body gives `Once` yielding 1; `B`
/// overrides it with `Repeat` yielding 2 (a different type and layout). Through the generic
/// `call`, 1 and 2: status 21. Today (by reading): `call(&B)`'s value is laid out as `Once`.
/// FAILS until the cause is fixed: coroutines.rs `opaque_hidden` (program.rs:1764 /
/// program/lazy/front_end.rs:504 `opaque_of`).
#[test]
fn a_provided_fns_impl_trait_is_the_overriding_impls_through_a_generic() {
    let got = run("
trait Tr {
    fn g(&self) -> impl Next {
        Once { left: Some(1) }
    }
}

struct A;
impl Tr for A {}

struct B;
impl Tr for B {
    fn g(&self) -> impl Next {
        Repeat { pad: 0, v: 2 }
    }
}

fn call<T: Tr>(t: &T) -> u32 {
    first(t.g())
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let a = call(&A);
    let b = call(&B);
    (a + 10 * b) as i32
}
");
    println!("PROVIDED IMPL TRAIT: {got:?}");
    assert_eq!(got, Ok(21));
}

/// The REQUIRED twin (no default body anywhere): each impl's own `async fn` future and `impl
/// Next`. `A`: ready 3, and `Once` yielding 3: 6. `B`: `Later` giving 4, and `Repeat` yielding
/// 4: 8. Status 6 + 10 * 8 = 86. Run 2026-11-21 05:56: "selecting a `Drop` impl: resolving an
/// instance: the world has no answer to FnSig(..)": coroutines.rs `opaque_hidden` asked the
/// selection in the world of `Req::f`'s own row, which has no body (fixed there, not yet run).
/// `call_f` awaits `Req::f(t)` (a call by path): the method-call form's build gap is its own
/// pair below.
#[test]
fn a_required_trait_fns_impl_trait_and_future_run_at_the_impl() {
    let got = run("
trait Req {
    async fn f(&self) -> u32;
    fn g(&self) -> impl Next;
}

struct A;
impl Req for A {
    async fn f(&self) -> u32 {
        3
    }
    fn g(&self) -> impl Next {
        Once { left: Some(3) }
    }
}

struct B;
impl Req for B {
    async fn f(&self) -> u32 {
        let v = Later { left: 1, value: 4 }.await;
        v
    }
    fn g(&self) -> impl Next {
        Repeat { pad: 0, v: 4 }
    }
}

async fn call_f<T: Req>(t: &T) -> u32 {
    Req::f(t).await
}

fn call<T: Req>(t: &T) -> u32 {
    block_on(call_f(t)) + first(t.g())
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let a = call(&A);
    let b = call(&B);
    (a + 10 * b) as i32
}
");
    println!("REQUIRED: {got:?}");
    assert_eq!(got, Ok(86));
}

/// The NEGATIVE twin: opaques of NO trait (a free `async fn`, a free fn's and an inherent
/// method's `impl Next`) reached through generics are revealed from their own fn's record, as
/// before the fix: the selection resolves each owner to itself. `Once` 5 + free future 7 +
/// inherent `Repeat` 9: status 21.
#[test]
fn opaques_of_no_trait_reveal_their_own_fns_body_through_a_generic() {
    let got = run("
async fn seven() -> u32 {
    let v = Later { left: 1, value: 7 }.await;
    v
}

fn five() -> impl Next {
    Once { left: Some(5) }
}

struct C;
impl C {
    fn nine(&self) -> impl Next {
        Repeat { pad: 0, v: 9 }
    }
}

fn take<F: Future<Output = u32>>(f: F) -> u32 {
    block_on(f)
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    (first(five()) + take(seven()) + first(C.nine())) as i32
}
");
    println!("NO TRAIT: {got:?}");
    assert_eq!(got, Ok(21));
}

/// The awaited future of `.await` is the operand's own value: the Reference, "Place
/// Expressions and Value Expressions" (a method call is a value expression, as a call is: only
/// paths to locals and statics, dereferences, indexing, fields and parentheses of those are
/// place expressions) and "Await expressions" ("Create a future by calling
/// `IntoFuture::into_future` on the future operand", which moves it). So `c.f().await` holds
/// one future across its suspension, the one the call made; nothing else of the call lives on.
/// No trait, no reveal: an inherent `async fn` awaited by METHOD call. 3 (pending once) + 10 *
/// 4 = 43.
/// FAILS until async-P8-4's first gap is closed: blaze-build walk.rs `is_fresh` counts
/// `Expr::Call` and not `Expr::MethodCall`, so `await_poll` (coroutine.rs ~691) copies the
/// method call's result and ends only the copy's slot; the call's own slot, its address passed
/// to the call, is kept across the suspension unnamed: "`call`: expression 1: a slot this
/// expression made is read after a suspension, and the flow saves no local it names
/// (.. async-P8-4)". The edit: the scratch file checker-e-walk-is-fresh-method-call.md.
#[test]
fn a_method_calls_future_awaited_in_place_is_no_unnamed_slot() {
    let got = run("
struct C {
    v: u32,
}

impl C {
    async fn f(&self) -> u32 {
        let v = Later { left: 1, value: self.v }.await;
        v
    }
}

async fn call(c: &C, d: &C) -> u32 {
    let a = c.f().await;
    let b = d.f().await;
    a + 10 * b
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    block_on(call(&C { v: 3 }, &C { v: 4 })) as i32
}
");
    println!("METHOD CALL AWAITED: {got:?}");
    assert_eq!(got, Ok(43));
}

/// The twin: the same program awaiting the same inherent fn by PATH (`C::f(c)`, an
/// `Expr::Call`, which the walk already takes in place): passes today, so the pair above
/// fails by the method-call form alone.
#[test]
fn a_path_calls_future_awaited_in_place_is_no_unnamed_slot() {
    let got = run("
struct C {
    v: u32,
}

impl C {
    async fn f(&self) -> u32 {
        let v = Later { left: 1, value: self.v }.await;
        v
    }
}

async fn call(c: &C, d: &C) -> u32 {
    let a = C::f(c).await;
    let b = C::f(d).await;
    a + 10 * b
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    block_on(call(&C { v: 3 }, &C { v: 4 })) as i32
}
");
    println!("PATH CALL AWAITED: {got:?}");
    assert_eq!(got, Ok(43));
}

#[test]
fn ipc_start_inner_loop_match_await_let_else_and_pin() {
    let got = run("
enum Opt<T> {
    Some(T),
    None,
}
struct Stream(u32);
impl Drop for Stream {
    fn drop(&mut self) {}
}
struct Reader(u32);
impl Drop for Reader {
    fn drop(&mut self) {}
}
struct Writer(u32);
impl Drop for Writer {
    fn drop(&mut self) {}
}
impl Stream {
    fn split(self) -> (Reader, Writer) {
        (Reader(self.0), Writer(self.0))
    }
}
struct PinMacroHelper<T> {
    value: T,
}
fn pin_new_unchecked<T>(p: &mut T) -> Pin<&mut T> {
    Pin { pointer: p }
}
fn my_drop<T>(_x: T) {}
struct StopFut(u32);
impl Drop for StopFut {
    fn drop(&mut self) {}
}
impl Future for StopFut {
    type Output = u32;
    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<u32> {
        Poll::Ready(self.pointer.0)
    }
}
struct WriteAll<'a> {
    w: &'a mut Writer,
    buf: &'a [u8],
    left: u32,
}
enum Res<T, E> {
    Ok(T),
    Err(E),
}
impl<T, E> Res<T, E> {
    fn ok(self) -> Opt<T> {
        match self {
            Res::Ok(v) => Opt::Some(v),
            Res::Err(_) => Opt::None,
        }
    }
}
struct OnReady(u32);
impl Drop for OnReady {
    fn drop(&mut self) {}
}
impl OnReady {
    fn send(self, _v: Res<(), ErrObj>) -> Res<(), ()> {
        Res::Ok(())
    }
}
struct Server {
    endpoint: u32,
    cfg: ErrObj,
}
impl<'a> Future for WriteAll<'a> {
    type Output = Res<(), ErrObj>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Res<(), ErrObj>> {
        let this = self.pointer;
        if this.left == 0 {
            Poll::Ready(Res::Ok(()))
        } else {
            this.left = this.left - 1;
            *cx.wakes = *cx.wakes + 1;
            Poll::Pending
        }
    }
}
impl Writer {
    fn write_all<'a>(&'a mut self, buf: &'a [u8]) -> WriteAll<'a> {
        WriteAll { w: self, buf, left: 1 }
    }
}
struct Tx(u32);
impl Drop for Tx {
    fn drop(&mut self) {}
}
struct Rx(u32);
impl Drop for Rx {
    fn drop(&mut self) {}
}
impl Rx {
    async fn recv(&mut self) -> Opt<u32> {
        let v = Later { left: 1, value: self.0 }.await;
        if v > 0 {
            self.0 = v - 1;
            Opt::Some(v)
        } else {
            Opt::None
        }
    }
}
impl<T> Opt<T> {
    fn is_some(&self) -> bool {
        match *self {
            Opt::Some(_) => true,
            Opt::None => false,
        }
    }
}
fn channel() -> (Tx, Rx) {
    (Tx(1), Rx(1))
}
struct Permit(u32);
impl Drop for Permit {
    fn drop(&mut self) {}
}
struct Guard {
    avail: u32,
}
impl Guard {
    fn try_acquire(&mut self) -> Opt<Permit> {
        if self.avail > 0 {
            self.avail = self.avail - 1;
            Opt::Some(Permit(self.avail + 1))
        } else {
            Opt::None
        }
    }
}
struct ErrObj(u32);
impl Drop for ErrObj {
    fn drop(&mut self) {}
}
enum Accept<S> {
    Shutdown,
    Established { stream: Stream, stop: S },
    Err((ErrObj, S)),
}
async fn try_accept<S>(n: &mut u32, stop: S) -> Accept<S> {
    let k = Later { left: 1, value: *n }.await;
    *n = k + 1;
    match k {
        0 => Accept::Established { stream: Stream(10), stop },
        1 => Accept::Established { stream: Stream(20), stop },
        2 => Accept::Err((ErrObj(7), stop)),
        _ => Accept::Shutdown,
    }
}
async fn backoff<S>(err: &ErrObj, stop: S) -> Opt<S> {
    let _ = Later { left: 1, value: err.0 }.await;
    Opt::Some(stop)
}
async fn start_inner(srv: Server, on_ready: OnReady) -> u32 {
    let mut listener = match if srv.endpoint == 0 { Res::Ok(0u32) } else { Res::Err(ErrObj(1)) } {
        Res::Ok(l) => l,
        Res::Err(err) => {
            on_ready.send(Res::Err(err)).ok();
            return 0;
        }
    };
    on_ready.send(Res::Ok(())).ok();
    let mut guard = Guard { avail: 1 };
    let stopped = StopFut(0u32);
    let mut stopped = {
        super let mut pinned = PinMacroHelper { value: stopped };
        pin_new_unchecked(&mut pinned.value)
    };
    let (drop_on_completion, mut awaiter) = channel();
    let mut sum = 0u32;
    loop {
        match try_accept(&mut listener, stopped).await {
            Accept::Established { stream, stop } => {
                let Opt::Some(permit) = guard.try_acquire() else {
                    let (_r, mut w) = stream.split();
                    let _ = w.write_all(b\"too many\").await;
                    stopped = stop;
                    continue;
                };
                sum = sum + stream.0 + permit.0;
                stopped = stop;
            }
            Accept::Shutdown => break,
            Accept::Err((err, stop)) => {
                let Opt::Some(stop) = backoff(&err, stop).await else { break };
                stopped = stop;
            }
        }
    }
    my_drop(drop_on_completion);
    while awaiter.recv().await.is_some() {}
    let _ = srv.cfg.0;
    sum
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    block_on(start_inner(Server { endpoint: 0, cfg: ErrObj(2) }, OnReady(1))) as i32
}
");
    println!("IPC START INNER: {got:?}");
    assert_eq!(got, Ok(11));
}

#[test]
fn while_let_await_match_with_guard_and_dropped_arm_binding() {
    let got = run("
struct Val(u32);
impl Drop for Val {
    fn drop(&mut self) {}
}

enum Outcome {
    Valid { tx: Val, extra: Val },
    Invalid(Val),
}

enum Out {
    Ok(Val),
    Err(Val),
}

async fn recv(n: &mut u32) -> Option<(u32, u32, Outcome)> {
    let k = Later { left: 1, value: *n }.await;
    if k == 0 {
        None
    } else {
        *n = k - 1;
        Some((k, k % 2, Outcome::Valid { tx: Val(k * 10), extra: Val(1) }))
    }
}

fn handle(o: Outcome) -> Out {
    match o {
        Outcome::Valid { tx, .. } => Out::Ok(tx),
        Outcome::Invalid(v) => Out::Err(v),
    }
}

async fn run_guarded() -> u32 {
    let mut sum = 0u32;
    let mut n = 3u32;
    let mut slot = Out::Err(Val(0));
    while let Some((_idx, epoch, outcome)) = recv(&mut n).await {
        slot = match outcome {
            Outcome::Valid { tx, .. } if epoch != 0 => {
                let copy = Val(tx.0 + 1);
                Out::Err(copy)
            }
            outcome => handle(outcome),
        };
        match &slot {
            Out::Ok(v) | Out::Err(v) => sum = sum + v.0,
        }
    }
    sum
}

async fn run_unguarded() -> u32 {
    let mut sum = 0u32;
    let mut n = 2u32;
    let mut slot = Out::Err(Val(0));
    while let Some((_idx, _epoch, outcome)) = recv(&mut n).await {
        slot = match outcome {
            Outcome::Valid { tx, .. } => {
                let copy = Val(tx.0 + 2);
                Out::Err(copy)
            }
            Outcome::Invalid(v) => Out::Err(v),
        };
        match &slot {
            Out::Ok(v) | Out::Err(v) => sum = sum + v.0,
        }
    }
    sum
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    // guarded: k=3 (epoch=1 -> 31), k=2 (epoch=0 -> 20), k=1 (epoch=1 -> 11) => 62
    // unguarded: k=2 (-> 22), k=1 (-> 12) => 34
    (block_on(run_guarded()) + block_on(run_unguarded())) as i32
}
");
    assert_eq!(got, Ok(96));
}

/// Regression for `reth_rpc_eth_types::cache::EthStateCacheService::spawn_fetch`: a `Drop` local
/// captured by value by `async move` blocks in multiple sibling `match` arms, where every use of
/// the local inside each `async move` body only borrows it (`&self`), must receive a drop flag in
/// `scan_env_moves` so sibling arms do not hit E0382 ("already moved") and drops run exactly once.
#[test]
fn match_arms_async_move_captures_borrowed_drop_local() {
    let got = run("
struct Provider {
    val: u32,
    drops: *mut u32,
}
impl Drop for Provider {
    fn drop(&mut self) {
        unsafe { *self.drops = *self.drops + 1; }
    }
}
impl Provider {
    fn get_block(&self, k: u32) -> u32 { self.val + k }
    fn get_receipts(&self, k: u32) -> u32 { self.val + k * 2 }
    fn get_bal(&self, k: u32) -> u32 { self.val + k * 3 }
}

enum Fetch {
    Block(u32),
    Receipts(u32),
    Bal(u32),
}

fn spawn_fetch(fetch: Fetch, base: u32, drops: *mut u32) -> u32 {
    let provider = Provider { val: base, drops };
    match fetch {
        Fetch::Block(k) => block_on(async move {
            provider.get_block(k)
        }),
        Fetch::Receipts(k) => block_on(async move {
            provider.get_receipts(k)
        }),
        Fetch::Bal(k) => block_on(async move {
            provider.get_bal(k)
        }),
    }
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let mut drops = 0u32;
    let a = spawn_fetch(Fetch::Block(1), 10, &mut drops as *mut u32);
    let b = spawn_fetch(Fetch::Receipts(2), 10, &mut drops as *mut u32);
    let c = spawn_fetch(Fetch::Bal(3), 10, &mut drops as *mut u32);
    if drops == 3 { (a + b + c) as i32 } else { -1 }
}
");
    // 11 + 14 + 19 = 44, drops == 3 (each Provider dropped once inside its async block)
    assert_eq!(got, Ok(44));
}
