//! The auto traits of async futures, by their witnesses. These are the acceptance tests of
//! docs/v2/plan/design/typeck.md, "Witness records". Each program is tempo's first site of one
//! group, reduced (blaze-int notes reports/tempo-correct-1120 not-checked-by-cause.txt). Each
//! answer is rustc's: the program checks, with no finding.
//!
//! - `spawn_worker`, from bin/tempo/src/tempo_cmd.rs `execute`: 65 rows, "the auto trait of
//!   another fn's future needs its witness record". Another fn's future is asked `Send`. Its
//!   body awaits an `async fn(self)`, where the borrow lowering refuses the call
//!   (`CountUnknown { what: OpaqueCaptures }`), so the row held only a superset.
//! - `local_block`, from crates/consensus/src/executor/actor/tests/harness.rs: 45 rows, "no
//!   lowered async block supplied its captures and saved locals". The same refusal, in the
//!   body's own block.
//! - `boxed_field`, from crates/zones/node/src/rpc.rs `coinbase`: 19 rows, "the coroutine
//!   witness needs the types and borrow modes of its captured places". `Box::pin(async move {
//!   .. self.api .. })` as `Pin<Box<dyn Future + Send + '_>>`.
//! - `returned_closure`: 17 rows, `ClosureCaptureWitness`. A closure made by another fn, asked
//!   `Send`.
//! - W3's mutual recursion, in two cases (the Reference, "Auto traits": a type is `Send` when
//!   its constituent types are; "Auto trait leakage": an `async fn`'s future leaks its auto
//!   traits; its constituents are its witness, what the body holds across its awaits).
//!   (a) `ping_a` and `pong_a` await each other's boxed futures and only `go` asks `Send`: no
//!   body asks an auto trait of a future it is needed to define, so both witnesses are filed
//!   and `ping_a: Send` holds coinductively (auto traits are coinductive: the cycle `ping_a ->
//!   pong_a -> ping_a` holds). rustc accepts it. Its twin `go_rc`: `pong_rc` holds an `Rc`
//!   across its await, E0277 naming `Send` and the `Rc` through the chain.
//!   (b) `ping` and `pong` each SPAWN the other's future in their own bodies: each body's
//!   check needs the other's witness, and each witness is its body's check. rustc stops on a
//!   query cycle (E0391: `type_of` of an opaque runs its defining body's `typeck`); the
//!   Reference states no answer. So the typed gap `HiddenFail::Cycle` (front2 opaque_auto.rs),
//!   never an error and never a pass. (This test asserted it clean, "rustc accepts it", without
//!   grounds until 2026-11-21.)
//!
//! Control (rule 23): `holds_rc` keeps an `Rc` across an `.await` in the same shapes. It must be
//! E0277 naming `Send`, so a clean answer above is not a check that decides nothing.

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::refusal::SourceFile;

const SOURCE: &str = r#"
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;

fn spawn<F: Future + Send + 'static>(f: F) { drop(f); }
async fn tick() {}

pub struct Conn(u8);
impl Conn {
    pub async fn resolve(self) -> Result<u8, &'static str> { tick().await; Ok(self.0) }
}

async fn worker(c: Conn) -> u8 { tick().await; c.resolve().await.unwrap_or(0) }
fn spawn_worker() { spawn(worker(Conn(1))); }

fn local_block(c: Conn) { spawn(async move { tick().await; c.resolve().await.unwrap_or(0) }); }

pub struct Api(u8);
impl Api { pub async fn header(&self) -> u8 { tick().await; self.0 } }
pub struct Rpc { api: Api }
type BoxFut<'a> = Pin<Box<dyn Future<Output = u8> + Send + 'a>>;
// tempo's `fn coinbase(&self) -> BoxFut<'_>`, its receiver written as a parameter: the block
// captures the place `rpc.api` through a reference, as it captured `self.eth.api`.
pub fn boxed_field(rpc: &Rpc) -> BoxFut<'_> { Box::pin(async move { rpc.api.header().await }) }

fn make_closure(n: u8) -> impl Fn() -> u8 { move || n }
fn needs_send<T: Send>(_: T) {}
fn returned_closure() { needs_send(make_closure(3)); }

async fn ping(n: u8) { if n > 0 { spawn(pong(n - 1)); } tick().await; }
async fn pong(n: u8) { if n > 0 { spawn(ping(n - 1)); } tick().await; }

async fn ping_a(n: u8) { if n > 0 { Box::pin(pong_a(n - 1)).await } }
async fn pong_a(n: u8) { if n > 0 { Box::pin(ping_a(n - 1)).await } }
fn go() { spawn(ping_a(3)); }
async fn ping_rc(n: u8) { if n > 0 { Box::pin(pong_rc(n - 1)).await } }
async fn pong_rc(n: u8) { let r = Rc::new(n); if n > 0 { Box::pin(ping_rc(n - 1)).await } drop(r); }
fn go_rc() { spawn(ping_rc(3)); }

async fn rc_worker() -> u8 { let r = Rc::new(1u8); tick().await; *r }
fn holds_rc() { spawn(rc_worker()); }
fn holds_rc_local() { spawn(async { let r = Rc::new(1u8); tick().await; drop(r); }); }
fn drops_rc_local() { spawn(async { { let r = Rc::new(1u8); drop(r); } tick().await; }); }

// `local_block`'s twin: `Maker::make(self)` is refused by the borrow lowering as `resolve`
// is (its record only a superset), and holds an `Rc` across its await. rustc: E0277.
pub struct Maker(u8);
impl Maker {
    pub async fn make(self) -> u8 { let r = Rc::new(self.0); tick().await; *r }
}
fn local_block_rc(m: Maker) { spawn(async move { tick().await; m.make().await }); }

fn main() {}
"#;

#[test]
fn async_futures_are_send_by_their_witnesses() {
    let alloc = blaze_own::testing::heap().alloc();
    let home = std::env::var("HOME").expect("HOME");
    let tc = Toolchain::open(&mut DirShelf::new(format!("{home}/.blaze/v2/toolchain"))).expect("installed toolchain");
    let root = "/wit/src/main.rs";
    let files = [(root.to_owned(), SOURCE.to_owned())];
    let user = UserCrate {
        name: "wit", dir: "/wit/", root, files: &files,
        edition: edition::PackageEdition::of_manifest("2024").expect("Rust 2024"), panic: blaze_build::Panic::Unwind,
    };
    let inputs = Inputs::new(&user, &tc).expect("fixture crate");
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).expect("fixture names");
    let sources = [SourceFile { path: "src/main.rs", text: SOURCE }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("checker frame");
    let mut world = ProjectWorld::new(&names, &frame, &sources, alloc).expect("body checker");
    let key_of = |name: &str| names.items(0).find_map(|(key, _, _)| (names.item_text(key)?.name == Some(name)).then_some(key)).expect(name);
    for name in ["spawn_worker", "local_block", "boxed_field", "returned_closure", "go"] {
        let v = world.check(BodyRef::Free(key_of(name)), alloc);
        assert!(v.is_clean() && v.reads.is_some(), "ORACLE: `{name}` checks with no finding (rustc accepts it): {v:?}");
        println!("{name}: clean");
    }

    // W3 (a)'s twin: the `Rc` `pong_rc` holds across its await makes `ping_rc`'s future not
    // `Send`, through the cycle: one E0277 naming `Send` and the `Rc`.
    let v = world.check(BodyRef::Free(key_of("go_rc")), alloc);
    assert!(v.gaps.is_empty() && v.refused.is_none(), "TWIN: `go_rc` is decided: {v:?}");
    let [d] = v.diagnostics.as_slice() else { panic!("TWIN: `go_rc` must be one Send error: {v:?}") };
    assert_eq!(d.code, "E0277", "TWIN: {d:?}");
    assert!(d.text.contains("Send") && d.text.contains("Rc<u8"), "TWIN: names `Send` and the `Rc<u8>` `pong_rc` holds: {}", d.text.as_str());
    println!("go_rc: E0277 through the cycle: {}", d.text.as_str());

    // W3 (b): each body asks `Send` of the other's future: the typed cycle gap, neither an
    // error nor a pass, naming the cycle (not one defining body's refusal as text).
    for name in ["ping", "pong"] {
        let v = world.check(BodyRef::Free(key_of(name)), alloc);
        assert!(v.diagnostics.is_empty(), "(b): `{name}` is no error (rustc's E0391 is a query cycle, no rule of the language): {v:?}");
        assert!(!v.is_clean(), "(b): `{name}` is never clean: {v:?}");
        let cycle = v.gaps.iter().any(|g| matches!(g, blaze_ladder::front2::world_items::Gap::Hidden(blaze_ladder::front2::opaque_auto::HiddenFail::Cycle { .. })));
        assert!(cycle, "(b): `{name}` stops on the typed cycle gap: {v:?}");
        println!("{name}: the cycle gap");
    }
    let v = world.check(BodyRef::Free(key_of("holds_rc")), alloc);
    assert!(v.gaps.is_empty() && v.refused.is_none(), "CONTROL: `holds_rc` is decided: {v:?}");
    let [d] = v.diagnostics.as_slice() else { panic!("CONTROL: `holds_rc` must be one Send error: {v:?}") };
    assert_eq!(d.code, "E0277", "CONTROL: {d:?}");
    assert!(d.text.contains("Send"), "CONTROL: {d:?}");
    println!("holds_rc: E0277 (Send), the Rc held across the await");

    // The chain of why, decided on the body's own block's exact interior (front2
    // stalled_goals.rs `held_slots`, check.rs `why_words`): WHICH value is held, across WHICH
    // await, and why its type is not `Send` (the prelude's `impl !Send for Rc<T>`). The two
    // places it names are filed as data (`BodyDiagnostic::notes`): the binding `r` and the
    // await. Needs the decision's own why column (blaze-typeck stalled.rs, this lane's report).
    let v = world.check(BodyRef::Free(key_of("holds_rc_local")), alloc);
    assert!(v.gaps.is_empty() && v.refused.is_none(), "`holds_rc_local` is decided: {v:?}");
    let [d] = v.diagnostics.as_slice() else { panic!("`holds_rc_local` must be one Send error: {v:?}") };
    assert_eq!(d.code, "E0277", "{d:?}");
    assert!(d.text.contains("`r` (`Rc<u8>`) is held across `tick().await` by the async block"), "the held value, by name and type, and the await: {}", d.text.as_str());
    assert!(d.text.contains("(a negative impl)"), "the leaf: `Rc<u8>: Send` has a negative impl: {}", d.text.as_str());
    let item = SOURCE.lines().find(|l| l.starts_with("fn holds_rc_local")).expect("the fixture's line");
    let at = |n: &blaze_ladder::front2::project::DiagNote| item.get(n.start as usize..n.end as usize);
    let held: Vec<_> = d.notes.iter().filter(|n| n.what == blaze_ladder::front2::project::NoteKind::HeldValue).filter_map(at).collect();
    let across: Vec<_> = d.notes.iter().filter(|n| n.what == blaze_ladder::front2::project::NoteKind::HeldAcross).filter_map(at).collect();
    assert_eq!(held, ["r"], "the note of the held value is the binding `r`: {:?}", d.notes);
    assert_eq!(across, ["tick().await"], "the note of the await it is held across: {:?}", d.notes);
    println!("holds_rc_local: E0277 with its chain: {}", d.text.as_str());

    // TWIN (rule 23): `r` dropped in a scope that ends before the await is held across none.
    let v = world.check(BodyRef::Free(key_of("drops_rc_local")), alloc);
    assert!(v.is_clean() && v.reads.is_some(), "TWIN: `drops_rc_local` checks with no finding (rustc accepts it): {v:?}");
    println!("drops_rc_local: clean");

    // TWIN of `local_block` (rule 23): the body's own block holds another fn's future whose
    // record is only a superset (blaze-typeck stalled.rs `Exact::coroutine_of`), and that
    // superset holds an `Rc` across an await. A `Yes` over the superset is what made
    // `local_block` clean; here the `No` may rest on the superset, so it is the typed witness
    // gap, never clean and never an E0277 the record cannot prove. (If `make`'s flow is
    // lowered exactly, the record is exact and the answer is rustc's E0277: also not clean.)
    let v = world.check(BodyRef::Free(key_of("local_block_rc")), alloc);
    assert!(!v.is_clean(), "TWIN: `local_block_rc` is never clean (rustc: E0277): {v:?}");
    let witness_gap = format!("{:?}", v.refused).contains("needs its witness record");
    let send_error = matches!(v.diagnostics.as_slice(), [d] if d.code == "E0277");
    assert!(witness_gap || send_error, "TWIN: `local_block_rc` is the witness gap (superset record) or E0277 (exact record): {v:?}");
    println!("local_block_rc: {}", if witness_gap { "the witness gap" } else { "E0277" });
}
