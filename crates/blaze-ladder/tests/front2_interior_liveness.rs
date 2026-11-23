//! An async fn's auto traits hold over what its state HOLDS at an `.await`: the liveness rule.
//! The Reference, "Async blocks": the future is "roughly equivalent to an enum with one variant
//! per await point, where each variant stores the data needed to resume from its corresponding
//! point"; rustc decides it from MIR (`mir_coroutine_witnesses`: a local held at a suspension is
//! storage-live there and live by use or maybe borrowed). So:
//! - a value dropped before the `.await` is out (a guard whose block ends first);
//! - a temporary's scope ends at its statement's end, so one made in a statement BEFORE the
//!   `.await`'s statement is out (a logging macro's temporaries in a match arm);
//! - a value moved out before the `.await`, and never borrowed, is out.
//!
//! tempo's seven false E0277 "`impl Future` cannot be sent between threads safely" (blaze-int
//! notes reports/tempo-correct-1120 errors.txt, errors 1 to 7: the six `tokio::test!` and
//! `test_traced!` bodies and crates/zones/node/src/deposit_allowlist/relay.rs:611) are these
//! three shapes, reduced:
//! - `inner_guard`: crates/zones/l1/src/subscriber.rs `wait_for_observation`, a parking_lot read
//!   guard in a `{ .. }` block inside a `loop`, its `match` returning from some arms, the
//!   `.await` after the block;
//! - `macro_arm`: relay.rs `refresh_mirror`, `debug!(..)` in a match arm that returns, an
//!   `.await` in a later statement; and `macro_tail`, the same temporaries in a block's tail as
//!   tracing's `valueset!` makes them;
//! - `moved_first`: a `!Send` value moved into a call before the `.await`.
//!
//! Each is asked `Send` by another fn (`spawn`), through its coroutine row, as tempo's tests
//! were. Each must check clean.
//!
//! NEGATIVE TWINS (rule 23): the same shapes with the value really held across the `.await` must
//! each be one E0277 naming `Send`: a guard held over the `.await` (`held_guard`), the temporary
//! in the `.await`'s own statement (`temp_in_await`), a value borrowed before the `.await`
//! whose storage lasts past it (`borrowed_first`). A clean answer above is then not a check that
//! decides nothing, and an exact row that drops a held value is caught.
//!
//! Where the row's interior cannot be made exactly (the flow refused), the row is a superset
//! (front2 opaque_auto.rs `FiledInterior::Superset`), and a `No` on a superset is a gap, never an
//! E0277: these tests then fail as gaps, by name, not as the false error.

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::refusal::SourceFile;

const SOURCE: &str = r#"
use std::future::Future;
use std::marker::PhantomData;

fn spawn<F: Future + Send>(f: F) { drop(f); }
async fn tick() {}

/// Not `Send`, as `parking_lot::RwLockReadGuard` and `fmt::Arguments` are not.
pub struct Unsend(u8, PhantomData<*const ()>);
impl Unsend { pub fn new(n: u8) -> Unsend { Unsend(n, PhantomData) } }
impl Drop for Unsend { fn drop(&mut self) {} }

pub struct Lock(u8);
pub struct Guard<'a>(&'a Lock, PhantomData<*const ()>);
impl Drop for Guard<'_> { fn drop(&mut self) {} }
impl Lock {
    pub fn read(&self) -> Guard<'_> { Guard(self, PhantomData) }
}
impl Guard<'_> {
    pub fn get(&self) -> Option<u8> { if self.0.0 > 0 { Some(self.0.0) } else { None } }
}

fn observe(_: &Unsend) {}
fn consume(_: Unsend) {}
fn probe(u: &Unsend) -> u8 { u.0 }
async fn after(_: &Unsend) {}

macro_rules! note { ($e:expr) => { observe(&Unsend::new($e)) } }

// wait_for_observation: the guard's block ends before the `.await`.
async fn inner_guard(l: &Lock) -> u8 {
    loop {
        {
            let g = l.read();
            match g.get() {
                Some(v) if v > 9 => return v,
                Some(v) => return v + 1,
                None => {}
            }
        }
        tick().await;
    }
}
fn spawn_inner_guard(l: &Lock) { spawn(inner_guard(l)); }

// refresh_mirror: the macro's temporaries end with their statement, in an arm that returns.
async fn macro_arm(n: u8) -> u8 {
    let m = match n {
        0 => {
            note!(n);
            return 0;
        }
        k => k,
    };
    note!(m);
    tick().await;
    m
}
fn spawn_macro_arm() { spawn(macro_arm(2)); }

// tracing's `valueset!`: the temporary is in a block's tail, the block in a statement.
async fn macro_tail(n: u8) -> u8 {
    let k = { probe(&Unsend::new(n)) };
    tick().await;
    k
}
fn spawn_macro_tail() { spawn(macro_tail(2)); }

// A value moved out before the `.await`, never borrowed.
async fn moved_first(n: u8) -> u8 {
    let u = Unsend::new(n);
    consume(u);
    tick().await;
    n
}
fn spawn_moved_first() { spawn(moved_first(2)); }

// TWIN: the guard held across the `.await`.
async fn held_guard(l: &Lock) -> u8 {
    let g = l.read();
    tick().await;
    g.get().unwrap_or(0)
}
fn spawn_held_guard(l: &Lock) { spawn(held_guard(l)); }

// TWIN: the temporary in the `.await`'s own statement lives to its end.
async fn temp_in_await(n: u8) -> u8 {
    after(&Unsend::new(n)).await;
    n
}
fn spawn_temp_in_await() { spawn(temp_in_await(2)); }

// TWIN: borrowed before the `.await`, its storage lasting past it.
async fn borrowed_first(n: u8) -> u8 {
    let u = Unsend::new(n);
    observe(&u);
    tick().await;
    n
}
fn spawn_borrowed_first() { spawn(borrowed_first(2)); }

fn main() {}
"#;

#[test]
fn an_async_fns_state_holds_only_what_is_live_at_its_awaits() {
    let alloc = blaze_own::testing::heap().alloc();
    let home = std::env::var("HOME").expect("HOME");
    let tc = Toolchain::open(&mut DirShelf::new(format!("{home}/.blaze/v2/toolchain"))).expect("installed toolchain");
    let root = "/live/src/main.rs";
    let files = [(root.to_owned(), SOURCE.to_owned())];
    let user = UserCrate {
        name: "live", dir: "/live/", root, files: &files,
        edition: edition::PackageEdition::of_manifest("2024").expect("Rust 2024"), panic: blaze_build::Panic::Unwind,
    };
    let inputs = Inputs::new(&user, &tc).expect("fixture crate");
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).expect("fixture names");
    let sources = [SourceFile { path: "src/main.rs", text: SOURCE }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("checker frame");
    let mut world = ProjectWorld::new(&names, &frame, &sources, alloc).expect("body checker");
    let key_of = |name: &str| names.items(0).find_map(|(key, _, _)| (names.item_text(key)?.name == Some(name)).then_some(key)).expect(name);
    for name in ["spawn_inner_guard", "spawn_macro_arm", "spawn_macro_tail", "spawn_moved_first"] {
        let v = world.check(BodyRef::Free(key_of(name)), alloc);
        assert!(v.is_clean() && v.reads.is_some(), "ORACLE: `{name}` checks with no finding (rustc accepts it: nothing `!Send` is held at the `.await`): {v:?}");
        println!("{name}: clean");
    }
    for name in ["spawn_held_guard", "spawn_temp_in_await", "spawn_borrowed_first"] {
        let v = world.check(BodyRef::Free(key_of(name)), alloc);
        assert!(v.gaps.is_empty() && v.refused.is_none(), "CONTROL: `{name}` is decided: {v:?}");
        let [d] = v.diagnostics.as_slice() else { panic!("CONTROL: `{name}` must be one Send error (a `!Send` value is held at the `.await`): {v:?}") };
        assert_eq!(d.code, "E0277", "CONTROL `{name}`: {d:?}");
        assert!(d.text.contains("Send"), "CONTROL `{name}`: {d:?}");
        println!("{name}: E0277 (Send), the value held at the `.await`");
    }
}
