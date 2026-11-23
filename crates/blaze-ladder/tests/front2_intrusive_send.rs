//! `Send`/`Sync` through an INTRUSIVE LIST: a type that is `Send` only through a cycle that
//! passes a written `unsafe impl`, and an impl whose where-clause is a projection.
//!
//! tempo's seven false E0277 "`impl Future` cannot be sent between threads safely" (blaze-int
//! notes reports/tempo-correct-1120/c2135wave69 errors.txt) all spawn a future that awaits
//! tokio's own: `watch::Receiver::changed` (crates/zones/l1/src/subscriber.rs `wait_for`,
//! `wait_for_capacity`), `time::Interval::tick` (crates/zones/sequencer/src/prover_config.rs
//! `monitor_upgrade_readiness_with_gauge`). The reductions of front2_interior_liveness.rs (a
//! guard, a macro temporary, a moved value) passed, so the cause is not in the async body's
//! liveness. What those tokio futures hold, reduced from tokio 1.53.3 (the version tempo's
//! lock pins): `changed_impl(shared: &Shared<T>, ..)` holds `&Shared`, so needs `Shared: Sync`,
//! so `Notify: Sync`, whose `waiters: Mutex<LinkedList<Waiter>>` (src/sync/notify.rs) is `Sync`
//! when `LinkedList<Waiter>: Send`, which is
//! `unsafe impl<L: Link> Send for LinkedList<L> where L::Target: Send {}`
//! (src/util/linked_list.rs:34): a where-clause on a PROJECTION, `<Waiter as Link>::Target`,
//! normalized to `Waiter`. And `Waiter: Send` is structural over `pointers: Pointers<Waiter>`,
//! which is `Send` by `unsafe impl<T: Send> Send for Pointers<T> {}` (linked_list.rs:106): it
//! asks `Waiter: Send` again. That cycle is of the auto trait `Send` alone, so it holds
//! coinductively (rustc: the new solver's and the old one's auto-trait coinduction, "Auto
//! traits": "an auto trait is implemented for a type if all its fields implement it" with
//! cycles accepted). An inductive reading (a cycle is a failure, or an overflow taken as
//! "no") refuses it. (The time driver's `TimerShared` is NOT this shape: tokio 1.53.3
//! runtime/time/entry.rs:355 writes `unsafe impl Send for TimerShared {}`, so `Interval::tick`'s
//! future, prover_config.rs:284, meets no cycle here: its false E0277 is another cause.)
//!
//! Oracle: `notify_is_sync`, `waiter_is_send` and `spawn_changed` (a future holding `&Shared`
//! across an `.await`, asked `Send`) are clean.
//!
//! NEGATIVE TWIN (rule 23): `Bare` is the same intrusive node with no `unsafe impl` on its
//! pointers: its raw pointers make it `!Send`, so `bare_is_send` is one E0277 naming `Send`. A
//! clean answer above is then the impls' and the coinduction's, not a check that decides nothing.

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::refusal::SourceFile;

const SOURCE: &str = r#"
use std::cell::UnsafeCell;
use std::future::Future;
use std::marker::{PhantomData, PhantomPinned};
use std::ptr::NonNull;
use std::sync::Mutex;

fn needs_send<T: Send>() {}
fn needs_sync<T: Sync>() {}
fn spawn<F: Future + Send>(f: F) { drop(f); }
async fn tick() {}

pub unsafe trait Link { type Target; }

pub struct LinkedList<L: Link> {
    head: Option<NonNull<L::Target>>,
    tail: Option<NonNull<L::Target>>,
}
unsafe impl<L: Link> Send for LinkedList<L> where L::Target: Send {}
unsafe impl<L: Link> Sync for LinkedList<L> where L::Target: Sync {}

struct PointersInner<T> { prev: Option<NonNull<T>>, next: Option<NonNull<T>>, _pin: PhantomPinned }
pub struct Pointers<T> { inner: UnsafeCell<PointersInner<T>> }
unsafe impl<T: Send> Send for Pointers<T> {}
unsafe impl<T: Sync> Sync for Pointers<T> {}

pub struct Waiter { pointers: Pointers<Waiter>, waker: UnsafeCell<Option<u8>>, _p: PhantomPinned }
unsafe impl Link for Waiter { type Target = Waiter; }

pub struct Notify { state: std::sync::atomic::AtomicUsize, waiters: Mutex<LinkedList<Waiter>> }
pub struct Shared { value: std::sync::RwLock<u8>, notify: Notify }

async fn changed_impl(shared: &Shared) -> u8 {
    tick().await;
    *shared.value.read().unwrap()
}

fn notify_is_sync() { needs_sync::<Notify>(); }
fn waiter_is_send() { needs_send::<Waiter>(); }
fn spawn_changed(shared: &Shared) { spawn(changed_impl(shared)); }

// TWIN: the same node, its pointers with no `unsafe impl`: `!Send` by its raw pointers.
pub struct BarePointers<T> { inner: UnsafeCell<PointersInner<T>> }
pub struct Bare { pointers: BarePointers<Bare>, _m: PhantomData<u8> }
fn bare_is_send() { needs_send::<Bare>(); }

// TWIN 2: the same cycle's shape through an ORDINARY trait: `Node: Walk` needs
// `Option<Box<Node>>: Walk`, which needs `Box<Node>: Walk`, which needs `Node: Walk`. No step is
// an auto trait's, so the cycle is not coinductive: rustc refuses it (E0275, "overflow
// evaluating the requirement `Node: Walk`"; the new solver: every step an impl where-bound of
// a non-coinductive trait, an ambiguous cycle, an error).
pub trait Walk {}
pub struct Node { next: Option<Box<Node>> }
impl Walk for Node where Option<Box<Node>>: Walk {}
impl<T: Walk> Walk for Option<T> {}
impl<T: Walk> Walk for Box<T> {}
fn needs_walk<T: Walk>() {}
fn node_walks() { needs_walk::<Node>(); }

fn main() {}
"#;

#[test]
fn an_intrusive_list_is_send_through_its_unsafe_impls() {
    let alloc = blaze_own::testing::heap().alloc();
    let home = std::env::var("HOME").expect("HOME");
    let tc = Toolchain::open(&mut DirShelf::new(format!("{home}/.blaze/v2/toolchain"))).expect("installed toolchain");
    let root = "/list/src/main.rs";
    let files = [(root.to_owned(), SOURCE.to_owned())];
    let user = UserCrate {
        name: "list", dir: "/list/", root, files: &files,
        edition: edition::PackageEdition::of_manifest("2024").expect("Rust 2024"), panic: blaze_build::Panic::Unwind,
    };
    let inputs = Inputs::new(&user, &tc).expect("fixture crate");
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).expect("fixture names");
    let sources = [SourceFile { path: "src/main.rs", text: SOURCE }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("checker frame");
    let mut world = ProjectWorld::new(&names, &frame, &sources, alloc).expect("body checker");
    let key_of = |name: &str| names.items(0).find_map(|(key, _, _)| (names.item_text(key)?.name == Some(name)).then_some(key)).expect(name);
    for name in ["notify_is_sync", "waiter_is_send", "spawn_changed"] {
        let v = world.check(BodyRef::Free(key_of(name)), alloc);
        assert!(v.is_clean() && v.reads.is_some(), "ORACLE: `{name}` checks with no finding (rustc accepts it: the cycle through `Pointers<Waiter>` is of `Send` alone, coinductive): {v:?}");
        println!("{name}: clean");
    }
    let v = world.check(BodyRef::Free(key_of("bare_is_send")), alloc);
    assert!(v.gaps.is_empty() && v.refused.is_none(), "CONTROL: `bare_is_send` is decided: {v:?}");
    let [d] = v.diagnostics.as_slice() else { panic!("CONTROL: `bare_is_send` must be one Send error (raw pointers, no impl): {v:?}") };
    assert_eq!(d.code, "E0277", "CONTROL: {d:?}");
    assert!(d.text.contains("Send"), "CONTROL: {d:?}");
    println!("bare_is_send: E0277 (Send)");
    // TWIN 2: a cycle of an ordinary trait is inductive: refused (rustc E0275), never a
    // coinductive success, and never left unchecked.
    let v = world.check(BodyRef::Free(key_of("node_walks")), alloc);
    assert!(v.gaps.is_empty() && v.refused.is_none(), "TWIN 2: `node_walks` is decided: {v:?}");
    assert!(!v.diagnostics.is_empty(), "TWIN 2: `node_walks` must be refused (a cycle of the non-auto trait `Walk` does not hold; rustc E0275): {v:?}");
    println!("node_walks: refused ({:?})", v.diagnostics.iter().map(|d| &d.code).collect::<Vec<_>>());
}
