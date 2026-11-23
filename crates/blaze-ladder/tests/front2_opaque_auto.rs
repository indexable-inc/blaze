//! Census 14, "Unported auto trait of an opaque" (20 tempo bodies): an auto trait of ANOTHER
//! fn's `impl Trait` is that of its hidden type, read from the defining body's check
//! (front2/opaque_auto.rs).
//!
//! The Reference, "Impl trait", "Auto trait leakage": "Auto traits ... of the hidden type are
//! leaked through the opaque type": `small() -> impl Sized { 7u8 }` is `Send` (`u8: Send`);
//! `shared() -> impl Sized { Rc::new(7u8) }` is not (library/alloc/src/rc.rs: "impl<T: ?Sized,
//! A: Allocator> !Send for Rc<T, A> {}"), so `is_send(shared())` is E0277. "Abstract return
//! types": the defining body chooses the hidden type, so asking an auto trait of a fn's own
//! opaque inside its own body has no answer (rustc's E0391 class): refused by name. A fn that
//! only names another opaque-returning fn whose body names it back (`ping`/`pong`) needs no
//! hidden type and is clean.
//!
//! Before the fix every line but `ping`'s is "not checked: ... its hidden type was not given by
//! the world" (blaze-typeck's `CheckError::OpaqueHiddenUnknown`: front2 answered
//! `opaque_hidden` with `None` always).
//!
//! Expected lines:
//!   "opaque: send_small: clean"
//!   "opaque: send_shared: diags E0277 ..." (one diagnostic, E0277)
//!   "opaque: send_ping: clean"
//!   "opaque: me: not checked: ... cycle (E0391) ..."
//!
//! Run: `LANECHECK_WAIT=540 bash /Volumes/Projects/blaze-int-1112-notes/lanecheck.sh blaze-ladder
//! test '--test front2_opaque_auto -- --nocapture'`. Needs the installed toolchain under
//! `$HOME/.blaze/v2/toolchain`. NEVER COMPILED by its writer (a write-only lane).

#![feature(allocator_api)]

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::body::lower_fn;
use blaze_ladder::front2::check::{BuildOrder, CheckFail, Records, check_body};
use blaze_ladder::front2::consts::Consts;
use blaze_ladder::front2::heads::Store;
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::refusal::SourceFile;
use blaze_lower::scoped::Prepares;
use blaze_own::Alloc;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};

const RUST_EDITION: &str = "2024";

const PROGRAM: &str = "use std::rc::Rc;
fn is_send<T: Send>(_: T) {}
fn small() -> impl Sized { 7u8 }
fn shared() -> impl Sized { Rc::new(7u8) }
fn send_small() { is_send(small()); }
fn send_shared() { is_send(shared()); }
fn ping(n: u8) -> impl Sized { if n > 0 { let _ = pong(n - 1); } 1u8 }
fn pong(n: u8) -> impl Sized { if n > 0 { let _ = ping(n - 1); } 2u16 }
fn send_ping() { is_send(ping(3)); }
fn me(n: u8) -> impl Sized { if n > 0 { is_send(me(n - 1)); } 0u8 }
fn main() {}
";

/// Each fn of `fns`, lowered and checked in order over one store (tests/front2_auto_shape.rs
/// `check_fns`, Rust edition, no array-length constants).
fn check_fns(text: &str, fns: &[&str]) -> Vec<(String, String)> {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/hello-world/src/main.rs".to_string(), text.to_string())];
    let edition = edition::PackageEdition::of_manifest(RUST_EDITION).expect("a known edition");
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
        let outcome = match lower_fn(&names, prepared, &mut front.keys, &mut store, k) {
            Err(e) => format!("not lowered: {e}"),
            Ok(body) => {
                let view = body.lowered.view();
                let spell = Spelling::of(&view).unwrap_or_else(|e| panic!("spelling `{n}`: {e:?}"));
                let scope = body.scope(&spell);
                let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: Some((&mut consts, prepared)) };
                match check_body(&mut records, &body, &scope) {
                    Ok(done) if done.diags.is_empty() => "clean".to_string(),
                    Ok(done) => {
                        let d: Vec<String> = done.diags.iter().map(|d| d.code.code().to_string()).collect();
                        format!("diags {}", d.join(" "))
                    }
                    Err(CheckFail::Diags { diags, .. }) => {
                        let d: Vec<String> = diags.iter().map(|(c, _, about)| format!("{} {about}", c.code())).collect();
                        format!("diags {}", d.join("; "))
                    }
                    Err(e) => format!("not checked: {e}"),
                }
            }
        };
        got.push((n.to_string(), outcome));
    }
    got
}

/// The four lines above; each fails before the fix (but `send_ping`, the control: no hidden
/// type is read, so none is defined).
#[test]
fn auto_traits_leak_from_the_hidden_type() {
    let got = check_fns(PROGRAM, &["send_small", "send_shared", "send_ping", "me"]);
    for (n, o) in &got {
        println!("opaque: {n}: {o}");
    }
    let of = |n: &str| got.iter().find(|g| g.0 == n).map(|g| g.1.clone()).unwrap_or_default();
    assert_eq!(of("send_small"), "clean", "`u8` is `Send`, so `small()`'s opaque is");
    let shared = of("send_shared");
    assert!(shared.starts_with("diags E0277") && !shared.contains(';'), "`Rc<u8>` is `!Send`: one E0277, got: {shared}");
    assert_eq!(of("send_ping"), "clean", "no hidden type is read for `ping`'s opaque except its own, `u8`");
    let me = of("me");
    assert!(me.contains("cycle (E0391)"), "`me` asks `Send` of its own opaque: the cycle by name, got: {me}");
    assert!(!me.contains("was not given by the world"), "the cycle is named, not the checker's bare gap: {me}");
}
