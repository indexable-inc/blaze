//! Census 14, brief A, second probe: the two things front2_auto_atomic left unclean, separated.
//!
//! (1) `std::sync::Once` / `OnceLock` did not resolve at segment 2. Under the blaze edition
//! `std` is bstd (blaze-scope graph.rs: "The crate `std` names: Rust's, or bstd under a blaze
//! edition"), and bstd's `sync` has no `Once` by design (crates/blaze-std/src/sync.rs: "`Once`,
//! `Lazy` | `OnceLock`, `LazyLock`: not here until bstd's own wait is under them"). Rust's std
//! declares them plainly (library/std/src/sync/mod.rs: "pub(crate) mod once;", "pub use
//! self::once::{Once, OnceState};", "mod once_lock;", "pub use self::once_lock::OnceLock;").
//! So the `rust:` lines run the same goals in a crate of Rust edition 2024, where `std` is Rust's.
//!
//! (2) `D: Sync` for tracing's shape was ambiguous (E0283). The claimed mechanism: a `Sized`
//! cycle. `unsafe impl<T> Prim for *mut T` carries the implicit `T: Sized` (the Reference,
//! "Generic parameters": "All type parameters have an implicit bound of `Sized` unless it is
//! relaxed"), so `At<*mut E>: Sync` needs `E: Sized`, whose tail `At<*mut E>` holds
//! `UnsafeCell<<*mut E as Prim>::Storage>`; normalizing that projection through the same impl
//! needs `E: Sized` again. core marks `Sized` coinductive (library/core/src/marker.rs:
//! "`Sized` being coinductive, despite having supertraits, is okay as there are no
//! user-written impls" above "#[rustc_coinductive] pub trait Sized: MetaSized"), so a cycle
//! through a `Sized` step holds. front2 read `#[rustc_coinductive]` nowhere (heads.rs
//! `trait_head` set COINDUCTIVE for auto traits only), so the cycle's path was forced
//! ambiguity. The fix (wiring census14-A-2-1.md) reads the attribute.
//!
//! What each line says:
//! - `cycle: sized_self_at` (`E: Sized`, E holding `At<*mut E>`) and `cycle: sync_self_at`,
//!   `cycle: tracing_shape_sync`: NOT clean before the fix (an E0283 or E0282 diag), clean after.
//!   If `sized_self_at` is clean BEFORE the fix, the mechanism is refuted.
//! - Controls, clean before and after: `cycle: sync_no_self` (the same fields without the
//!   self-reference: the cause removed), `cycle: auto_self_ref` (`G { p: &'static G }`: a
//!   cycle of `Sync` alone, coinductive already), `cycle: sized_ptr_unit` (`At<*mut ()>`).
//! - `rust: <fn>: clean` for every Once/OnceLock goal and for core's `AtomicPtr<Self>`
//!   (`rust: sized_core_self_ptr`, `rust: sync_core_self_ptr`: these two need the fix too).
//!   A `rust:` line that fails with a path error names a gap of the Rust-edition graph, not (1).
//!
//! Expected lines when fixed: "cycle: sized_self_at: clean", "cycle: sync_self_at: clean",
//! "cycle: tracing_shape_sync: clean", "rust: sync_once_lock: clean",
//! "rust: sync_core_self_ptr: clean".
//!
//! Run: `LANECHECK_WAIT=540 bash /Volumes/Projects/blaze-int-1112-notes/lanecheck.sh blaze-ladder
//! test '--test front2_auto_shape -- --nocapture'`. Needs the installed toolchain under
//! `$HOME/.blaze/v2/toolchain`. NEVER COMPILED by its writer (a write-only lane).

#![feature(allocator_api)]

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::body::{BodyError, lower_fn};
use blaze_ladder::front2::check::{BuildOrder, CheckFail, Records, check_body, eval_len};
use blaze_ladder::front2::consts::Consts;
use blaze_ladder::front2::heads::{HeadError, Store};
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::front2::types::Why;
use blaze_ladder::refusal::SourceFile;
use blaze_lower::scoped::Prepares;
use blaze_own::Alloc;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};

const BLAZE_EDITION: &str = "blaze-2026.11";
const RUST_EDITION: &str = "2024";

/// The most constants one fn's lowering may need evaluated before it converts.
const MOST_LENS: u32 = 8;

/// Each fn of `fns` in the crate `text` of `edition`, lowered and checked in order over one
/// store: tests/front2_auto_atomic.rs `check_fns` with the edition a parameter.
fn check_fns(edition_text: &str, text: &str, fns: &[&str]) -> Vec<(String, String)> {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/hello-world/src/main.rs".to_string(), text.to_string())];
    let edition = edition::PackageEdition::of_manifest(edition_text).expect("a known edition");
    let user = UserCrate { name: "hello_world", dir: "/hello-world/", root: "/hello-world/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
    let mut front = Front2::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut store = Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global());
    let (mut buckets, mut consts) = (None, Consts::default());
    let mut got = Vec::new();
    for &n in fns {
        let found = names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: &[n] }, Alloc::global())
            .unwrap_or_else(|e| panic!("`{n}`: {e}"));
        let k = found.whole(1).unwrap_or_else(|| panic!("`{n}` not named")).key;
        let mut lens = 0;
        let outcome = loop {
            match lower_fn(&names, prepared, &mut front.keys, &mut store, k) {
                Err(BodyError::Convert { why: Some(Why::NeedConst(c, _)), .. }) | Err(BodyError::Head { error: HeadError::Item(_, Why::NeedConst(c, _)), .. }) if lens < MOST_LENS => {
                    lens += 1;
                    let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: Some((&mut consts, prepared)) };
                    if let Err(e) = eval_len(&mut records, c) {
                        break format!("not lowered: the constant {c:x} its body names: {e}");
                    }
                }
                Err(e) => break format!("not lowered: {e}"),
                Ok(body) => {
                    let view = body.lowered.view();
                    let spell = Spelling::of(&view).unwrap_or_else(|e| panic!("spelling `{n}`: {e:?}"));
                    let scope = body.scope(&spell);
                    let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: Some((&mut consts, prepared)) };
                    break match check_body(&mut records, &body, &scope) {
                        Ok(done) if done.diags.is_empty() => "clean".to_string(),
                        Ok(done) => {
                            let d: Vec<String> = done.diags.iter().map(|d| format!("{} at {:?}: `{}`", d.code.code(), d.at, text.get(d.at.start as usize..d.at.end as usize).unwrap_or("?"))).collect();
                            format!("diags {d:?}")
                        }
                        Err(CheckFail::Diags { diags, .. }) => format!("diags {diags:?}"),
                        Err(e) => format!("not checked: {e}"),
                    };
                }
            }
        };
        got.push((n.to_string(), outcome));
    }
    got
}

/// Prints every outcome as `<what>: <fn>: <outcome>` first, then asserts each is `clean`.
fn expect_clean(what: &str, got: &[(String, String)]) {
    let lines: Vec<String> = got.iter().map(|(n, o)| format!("{what}: {n}: {o}")).collect();
    for l in &lines {
        println!("{l}");
    }
    let bad: Vec<&String> = lines.iter().filter(|l| !l.ends_with(": clean")).collect();
    assert!(bad.is_empty(), "{} of {} not clean (each goal holds: `Sized` is coinductive, auto traits are structural):\n{}", bad.len(), lines.len(), lines.join("\n"));
}

/// The crate's own replica of core's `Atomic<T>`, one field at a time.
const CYCLE: &str = "use core::cell::UnsafeCell;
fn is_sync<T: ?Sized + Sync>() {}
fn is_sized<T>() {}
pub unsafe trait Prim: Sized + Copy { type Storage: Sized; }
#[repr(C, align(1))]
pub struct B1<T>(T);
#[repr(C, align(8))]
pub struct B8<T>(T);
unsafe impl Prim for u8 { type Storage = B1<u8>; }
unsafe impl<T> Prim for *mut T { type Storage = B8<*mut T>; }
#[repr(C)]
pub struct At<T: Prim> { v: UnsafeCell<T::Storage> }
unsafe impl<T: Prim> Send for At<T> {}
unsafe impl<T: Prim> Sync for At<T> {}
trait C: Sync {}
struct M { c: &'static dyn C }
struct E { n: At<*mut E> }
struct D { a: At<u8>, m: &'static M, n: At<*mut D> }
struct N { a: At<u8>, m: &'static M, n: At<*mut u8> }
struct G { p: &'static G }
fn sized_self_at() { is_sized::<E>(); }
fn sync_self_at() { is_sync::<E>(); }
fn tracing_shape_sync() { is_sync::<D>(); }
fn sync_no_self() { is_sync::<N>(); }
fn auto_self_ref() { is_sync::<G>(); }
fn sized_ptr_unit() { is_sized::<At<*mut ()>>(); }
fn main() {}
";

/// Rust edition: `std` is Rust's std; core's `AtomicPtr<Self>` is tracing's `next` field.
const RUST: &str = "fn is_send<T: ?Sized + Send>() {}
fn is_sync<T: ?Sized + Sync>() {}
fn is_unpin<T: ?Sized + Unpin>() {}
fn is_sized<T>() {}
struct W { a: core::sync::atomic::AtomicU8, next: core::sync::atomic::AtomicPtr<W> }
fn unpin_once() { is_unpin::<std::sync::Once>(); }
fn unpin_once_lock() { is_unpin::<std::sync::OnceLock<u8>>(); }
fn send_once_lock() { is_send::<std::sync::OnceLock<u8>>(); }
fn sync_once_lock() { is_sync::<std::sync::OnceLock<u8>>(); }
fn sized_core_self_ptr() { is_sized::<W>(); }
fn sync_core_self_ptr() { is_sync::<W>(); }
fn main() {}
";

/// The `Sized` cycle and its controls. Expected: every line "cycle: <fn>: clean"; before the
/// fix, `sized_self_at`, `sync_self_at` and `tracing_shape_sync` are not.
#[test]
fn sized_cycle_is_coinductive() {
    let fns = ["sized_self_at", "sync_self_at", "tracing_shape_sync", "sync_no_self", "auto_self_ref", "sized_ptr_unit"];
    expect_clean("cycle", &check_fns(BLAZE_EDITION, CYCLE, &fns));
}

/// Rust's std names and core's self-referential `AtomicPtr`. Expected: every line
/// "rust: <fn>: clean".
#[test]
fn rust_edition_std() {
    let fns = ["unpin_once", "unpin_once_lock", "send_once_lock", "sync_once_lock", "sized_core_self_ptr", "sync_core_self_ptr"];
    expect_clean("rust", &check_fns(RUST_EDITION, RUST, &fns));
}
