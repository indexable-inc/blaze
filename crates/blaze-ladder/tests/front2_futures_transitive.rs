//! W3: an auto trait of a future that holds ANOTHER fn's future is decided through that fn's
//! future, at any depth. The Reference, "Auto traits": an auto trait is implemented for a type
//! whose "constituent types" all implement it, and "Auto trait leakage" (impl Trait): a caller
//! sees the auto traits of the hidden type behind `impl Trait`, an `async fn`'s future
//! included. So `spawn(a())` is `Send` exactly when `a`'s future, `b`'s future it holds across
//! its await, and `c`'s future that one holds, store only `Send` values.
//!
//! The mechanism (blaze-typeck machine.rs `hidden_stall`, stalled.rs `decide`; front2
//! stalled_goals.rs `decide_stalled_goals`): an undecided goal's own cause is the opaque its
//! ordinary evaluation stalled on (blaze-solve `Ask::first_hidden_unknown` over the root's read
//! list: the auto-trait rule reads that opaque's fact the moment it stalls, and a cached answer
//! carries its read list whole). The check stops with the typed gap `OpaqueHiddenUnknown`, the
//! loop files the opaque's defining body and leaks it (opaque_auto.rs `after_auto_gap`), and
//! the goal is asked again; a reveal that adds no opaque ends the loop with the gap. A cycle of
//! futures is answered coinductively by the solver (`PathKind::Coinductive`).
//!
//! The worlds differ ONLY in the value `c` (or the dependency's `leaf`) holds across its await:
//! `u8`, `u16` (both `Send`) or `Rc<u8>` (`impl !Send for Rc<T>`, alloc/src/rc.rs).
//!
//! - Case 1, two levels (`two_levels`): clean on `u8`; E0277 on `Rc<u8>`, its chain down to
//!   `Rc<u8>: Send`.
//! - Case 2, through a closure's capture (`closure_capture`) and a closure returning an async
//!   block that holds the boxed future (`closure_block`, tempo
//!   crates/consensus/src/executor/actor/tests/scheduling.rs:55).
//! - Case 3, through a blanket impl's where-clause (`server`, axum's `Handler` impl's `Fut:
//!   Future + Send`, tempo crates/zones/rpc/src/server.rs:63).
//! - Case 4, a cycle (`recursive`: `f` awaits its own boxed future): clean in every world.
//! - Case 6, a cycle of two (`go`: `ping` and `pong` await each other's boxed futures; `pong`
//!   holds `HELD`): decided coinductively from both witnesses, its read list holding both.
//! - Case 5, across crates: the futures are the dependency's.
//!
//! Rule 23 (remove the cause, watch the effect go): the `Rc` world turns each asker's verdict to
//! E0277; the `u8` and `u16` worlds are both clean with read-list digests that differ for each
//! asker that reaches `c` (its future's record is in the asker's read list: an edit to `c`'s
//! body that changes what its future stores rechecks the asker, rule 21), and are equal for
//! `recursive`, which never reads `c`.
//!
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`.

#![feature(allocator_api)]

use blaze_id::{ContentId, CrateItemKey};
use blaze_ladder::crates::{DirShelf, Inputs, Texts, Toolchain, UserCrate};
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::refusal::SourceFile;
use blaze_scope::graph::{CrateId, Sysroot};
use blaze_scope::resolve::{CrateInput, SourceTexts, resolve_crates};
use blaze_scope::task::Serial;

/// The single-crate program; `HELD` is the value `c` holds across its await.
const ONE_CRATE: &str = r#"
use std::future::Future;

fn spawn<F: Future + Send + 'static>(f: F) { drop(f); }
fn needs_send<T: Send>(_: T) {}
fn start<F: FnOnce(u8) -> Fut, Fut: Future + Send>(f: F) { drop(f); }
async fn tick() {}

async fn c() { let held = HELD; tick().await; drop(held); }
async fn b() { c().await }
async fn a() { b().await }
fn two_levels() { spawn(a()); }

fn closure_capture() { let verify = b(); needs_send(move |_: u8| verify); }
fn closure_block() { start(|_: u8| async move { let v = Box::pin(b()); v.await }); }

trait Handler<T> {}
impl<F, Fut> Handler<()> for F where F: FnOnce() -> Fut, Fut: Future<Output = ()> + Send {}
fn route<H: Handler<()>>(_: H) {}
async fn handle() { b().await }
fn server() { route(handle); }

async fn f(n: u32) { if n > 0 { Box::pin(f(n - 1)).await } }
fn recursive() { spawn(f(3)); }

async fn ping(n: u32) { if n > 0 { Box::pin(pong(n - 1)).await } }
async fn pong(n: u32) { let held = HELD; if n > 0 { Box::pin(ping(n - 1)).await } drop(held); }
fn go() { spawn(ping(3)); }

fn main() {}
"#;

/// The askers of [`ONE_CRATE`] that reach `c`'s future.
const REACH_C: [&str; 4] = ["two_levels", "closure_capture", "closure_block", "server"];

/// Case 5's dependency: the futures are its own; `HELD` as in [`ONE_CRATE`].
const DEP: &str = r#"
async fn tick() {}
pub async fn leaf() { let held = HELD; tick().await; drop(held); }
pub async fn mid() { leaf().await }
"#;

/// Case 5's program: it asks `Send` of the dependency's `mid` future.
const USER: &str = r#"
use std::future::Future;
fn spawn<F: Future + Send + 'static>(f: F) { drop(f); }
fn cross() { spawn(w3dep::mid()); }
fn main() {}
"#;

const HELD_U8: &str = "1u8";
const HELD_U16: &str = "1u16";
const HELD_RC: &str = "std::rc::Rc::new(1u8)";

/// One body's verdict, owned: whether it is clean, its read list's digest, its diagnostics
/// (code, text), and the whole verdict as printed.
struct Got {
    clean: bool,
    reads: Option<[u8; 32]>,
    diags: Vec<(&'static str, String)>,
    shown: String,
}

fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

/// The texts of the two-crate layout: the dependency's file for crate `dep`, the rest as
/// [`Inputs`] reads them.
struct WithDep<'t> {
    base: &'t Texts<'t>,
    dep: usize,
    files: &'t [(String, String)],
}

impl<'t> SourceTexts<'t> for WithDep<'t> {
    fn read(&self, krate: usize, path: &str) -> Option<&'t str> {
        if krate == self.dep {
            return self.files.iter().find(|(p, _)| p == path).map(|(_, t)| t.as_str());
        }
        self.base.read(krate, path)
    }
}

/// Checks each of `bodies` (fns of the program crate) in one world: the program `user`, with
/// the library crate `w3dep` of text `dep` when given.
fn check_bodies(user: &'static str, dep: Option<&'static str>, bodies: &[&str]) -> Vec<Got> {
    let alloc = blaze_own::testing::heap().alloc();
    let home = std::env::var("HOME").expect("HOME");
    let tc = Toolchain::open(&mut DirShelf::new(format!("{home}/.blaze/v2/toolchain"))).expect("installed toolchain");
    let root = "/w3/src/main.rs";
    let files = [(root.to_owned(), user.to_owned())];
    let program = UserCrate {
        name: "w3", dir: "/w3/", root, files: &files,
        edition: edition::PackageEdition::of_manifest("2024").expect("Rust 2024"), panic: blaze_build::Panic::Unwind,
    };
    let inputs = Inputs::new(&program, &tc).expect("fixture crate");
    let mut crates: Vec<CrateInput<'_>> = inputs.crates();
    let dep_files = [("/w3dep/src/lib.rs".to_owned(), dep.unwrap_or("").to_owned())];
    let names = match dep {
        None => inputs.resolve(&crates, blaze_own::testing::bump()).expect("fixture names"),
        Some(_) => {
            let at = |name: &str| crates.iter().position(|c| c.name == name).map(|i| CrateId(i as u32));
            let sysroot = Sysroot { core: at("core"), alloc: at("alloc"), std: at("std"), bstd: at("bstd") };
            let dep_at = crates.len();
            let user_deps: &'static [(&'static str, usize)] = Box::leak(vec![("w3dep", dep_at)].into_boxed_slice());
            let (cfg, crate_edition, implicit) = (crates[0].cfg, crates[0].edition, crates[0].implicit);
            crates[0].deps = user_deps;
            crates.push(CrateInput {
                name: "w3dep",
                key: CrateItemKey(ContentId::of("blaze-ladder W3 cross-crate test crate", b"w3dep")),
                edition: crate_edition,
                implicit,
                root: "/w3dep/src/lib.rs",
                shown_from: "/w3dep/",
                features: &[],
                deps: &[],
                cfg,
                script: None,
            });
            let texts = WithDep { base: &inputs.texts, dep: dep_at, files: &dep_files };
            resolve_crates(&crates, sysroot, &texts, &Serial, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates (two crates): {e}"))
        }
    };
    let sources = [SourceFile { path: "src/main.rs", text: user }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("checker frame");
    let mut world = ProjectWorld::new(&names, &frame, &sources, alloc).expect("body checker");
    let key_of = |name: &str| names.items(0).find_map(|(key, _, _)| (names.item_text(key)?.name == Some(name)).then_some(key)).expect(name);
    let mut out = Vec::new();
    for &name in bodies {
        let v = world.check(BodyRef::Free(key_of(name)), alloc);
        let shown = format!("{v:?}");
        let diags = v.diagnostics.iter().map(|d| (d.code, d.text.as_str().to_owned())).collect();
        out.push(Got { clean: v.is_clean(), reads: v.reads.map(|c| c.0), diags, shown });
    }
    out
}

fn one_crate(held: &str) -> &'static str {
    leak(ONE_CRATE.replace("HELD", held))
}

/// Cases 1 to 4 and the rule-23 control, in one program.
#[test]
fn another_fns_future_is_decided_through_its_own() {
    let mut askers: Vec<&str> = REACH_C.to_vec();
    askers.push("recursive");
    let small = check_bodies(one_crate(HELD_U8), None, &askers);
    let wide = check_bodies(one_crate(HELD_U16), None, &askers);
    let rc = check_bodies(one_crate(HELD_RC), None, &askers);

    // ORACLE (rustc accepts each): every asker is clean on a `Send` value, with a digest.
    for (name, got) in askers.iter().zip(small.iter()) {
        assert!(got.clean && got.reads.is_some(), "`{name}` (u8): clean with a read list (rustc accepts it): {}", got.shown);
    }
    for (name, got) in askers.iter().zip(wide.iter()) {
        assert!(got.clean && got.reads.is_some(), "`{name}` (u16): clean with a read list: {}", got.shown);
    }
    println!("u8 and u16 worlds: {} askers clean", askers.len());

    // TWIN: `c` holds an `Rc<u8>` across its await: each asker that reaches `c` is one E0277
    // about `Send`, its chain down to the value.
    for (name, got) in askers.iter().zip(rc.iter()) {
        if *name == "recursive" {
            assert!(got.clean, "CONTROL: `recursive` never reaches `c`: clean in the Rc world too: {}", got.shown);
            continue;
        }
        let [(code, text)] = got.diags.as_slice() else { panic!("TWIN: `{name}` must be one Send error: {}", got.shown) };
        assert_eq!(*code, "E0277", "TWIN: `{name}`: {}", got.shown);
        assert!(text.contains("Send"), "TWIN: `{name}` names `Send`: {text}");
        if *name == "two_levels" {
            assert!(text.contains("Rc<u8"), "TWIN: `two_levels`' chain reaches `Rc<u8>: Send` in `c`: {text}");
            // Each step through a future names it by its fn (check.rs `opaque_shown`). CONTROL:
            // printed by the opaque's own key, it read "the hidden type of an item the names
            // source holds no path for (key ..)", an opaque key being no item of the names.
            assert!(text.contains("the future of `"), "`two_levels`' chain names each future by its fn: {text}");
            assert!(!text.contains("holds no path for"), "no future is said by its bare key: {text}");
        }
        println!("{name} (Rc): E0277: {text}");
    }

    // Rule 23 on the read list: `c`'s future is read by each asker that reaches it, so its
    // digest moves with what `c` stores; `recursive`'s does not.
    for ((name, s), w) in askers.iter().zip(small.iter()).zip(wide.iter()) {
        if *name == "recursive" {
            assert_eq!(s.reads, w.reads, "`recursive` reads nothing of `c`: one digest in both worlds");
        } else {
            assert_ne!(s.reads, w.reads, "`{name}` reads `c`'s future: its digest moves with what `c` holds ({} / {})", s.shown, w.shown);
        }
    }
    println!("read digests: moved for {REACH_C:?}, equal for `recursive`");
}

/// Case 5: the futures are another crate's.
#[test]
fn another_crates_future_is_decided_through_its_own() {
    let dep = |held: &str| leak(DEP.replace("HELD", held));
    let small = check_bodies(USER, Some(dep(HELD_U8)), &["cross"]);
    let wide = check_bodies(USER, Some(dep(HELD_U16)), &["cross"]);
    let rc = check_bodies(USER, Some(dep(HELD_RC)), &["cross"]);
    assert!(small[0].clean && small[0].reads.is_some(), "`cross` (u8): clean (rustc accepts it): {}", small[0].shown);
    assert!(wide[0].clean && wide[0].reads.is_some(), "`cross` (u16): clean: {}", wide[0].shown);
    assert_ne!(small[0].reads, wide[0].reads, "`cross` reads the dependency's `leaf` future: its digest moves with what `leaf` holds");
    let [(code, text)] = rc[0].diags.as_slice() else { panic!("TWIN: `cross` must be one Send error: {}", rc[0].shown) };
    assert_eq!(*code, "E0277", "TWIN: {}", rc[0].shown);
    assert!(text.contains("Send"), "TWIN: names `Send`: {text}");
    println!("cross (Rc): E0277: {text}");
}

/// Case 4's twin, on the cycle itself: `f` holds an `Rc<u8>` across its own boxed await, so
/// its future is not `Send` (rustc: E0277), and the coinductive cycle (the auto rule sees the
/// hidden type, blaze-solve structural.rs; normalization answers the opaque inside it rigid,
/// alias.rs `normalize_opaque_type`) must not hide it: one E0277 naming
/// the `Rc`. Rule 23: the same program holding a `u8` is clean (the main test's `recursive`).
/// NOT YET RUN (written 2026-11-21 by a write-only lane).
#[test]
fn a_recursive_future_holding_an_rc_across_its_await_is_not_send() {
    let held_rc = leak(one_crate(HELD_U8).replace(
        "async fn f(n: u32) { if n > 0 { Box::pin(f(n - 1)).await } }",
        "async fn f(n: u32) { let held = std::rc::Rc::new(1u8); if n > 0 { Box::pin(f(n - 1)).await } drop(held); }",
    ));
    assert!(held_rc.contains("let held = std::rc::Rc::new(1u8); if n > 0"), "the fixture's `f` was replaced");
    let rc = check_bodies(held_rc, None, &["recursive"]);
    let [(code, text)] = rc[0].diags.as_slice() else { panic!("TWIN: `recursive` must be one Send error: {}", rc[0].shown) };
    assert_eq!(*code, "E0277", "TWIN: {}", rc[0].shown);
    assert!(text.contains("Send") && text.contains("Rc<u8"), "TWIN: names `Send` and the `Rc<u8>` held on the cycle: {text}");
    println!("recursive (Rc on the cycle): E0277: {text}");
    let small = check_bodies(one_crate(HELD_U8), None, &["recursive"]);
    assert!(small[0].clean && small[0].reads.is_some(), "CONTROL: `recursive` holding nothing on the cycle is clean: {}", small[0].shown);
}

/// An opaque type outside its defining body is RIGID to the check: the Reference, "Abstract
/// return types": "a caller of the function is not permitted to know the concrete type"; only
/// the bounds written in `impl Trait` (and its auto traits, leaked: the tests above) are
/// known. blaze-solve alias.rs `normalize_opaque_type` answers an opaque itself in
/// `TypingMode::Analysis`; it revealed the hidden type before, for an opaque reaching the
/// solver non-rigid (blaze-typeck infer/facts.rs imports a record's `PNode::Opaque` with
/// `Rigid::No`).
///
/// - NEGATIVE, rustc rejects both: `let x: u32 = make();` (E0308: the opaque is not `u32`) and
///   `make().count_ones()` (E0599: `impl Sized` has no such method), `make` returning `0u32`.
/// - POSITIVE, through the bounds alone: `a().await` is `u32` by `Future<Output = u32>`; `for s
///   in it()` yields `&'static str` by `Iterator<Item = &'static str>`; `adder()(1)` is `u32`
///   by `Fn(u32) -> u32`.
/// - TWIN of the negatives: the same uses of a fn returning `u32` itself are clean.
/// NOT YET RUN (written 2026-11-21 by a write-only lane).
#[test]
fn an_opaque_type_is_rigid_to_its_callers() {
    const RIGID: &str = r#"
fn make() -> impl Sized { 0u32 }
fn use_eq() { let x: u32 = make(); let _ = x; }
fn use_method() { let _ = make().count_ones(); }

fn plain() -> u32 { 0u32 }
fn use_plain() { let x: u32 = plain(); let _ = x.count_ones(); }

async fn a() -> u32 { 7 }
async fn use_await() { let n: u32 = a().await; let _ = n; }
fn it() -> impl Iterator<Item = &'static str> { ["a", "b"].into_iter() }
fn use_iter() { for s in it() { let _: &str = s; } }
fn adder() -> impl Fn(u32) -> u32 { |x| x + 1 }
fn use_fn() { let n: u32 = adder()(1); let _ = n; }

fn main() {}
"#;
    let got = check_bodies(RIGID, None, &["use_eq", "use_method", "use_plain", "use_await", "use_iter", "use_fn"]);
    for (name, g) in ["use_eq", "use_method", "use_plain", "use_await", "use_iter", "use_fn"].iter().zip(got.iter()) {
        println!("opaque rigid: {name}: clean {} {:?}", g.clean, g.diags);
    }
    let codes = |i: usize| got[i].diags.iter().map(|(c, _)| *c).collect::<Vec<_>>();
    assert_eq!(codes(0), ["E0308"], "`let x: u32 = make();`: the opaque is not `u32` (rustc E0308): {}", got[0].shown);
    assert_eq!(codes(1), ["E0599"], "`make().count_ones()`: `impl Sized` has no such method (rustc E0599): {}", got[1].shown);
    assert!(got[2].clean, "TWIN: the same uses of a fn returning `u32` are clean: {}", got[2].shown);
    for (i, what) in [(3, "`a().await` is `u32` by its future's bound"), (4, "`it()` yields `&'static str` by its bound"), (5, "`adder()(1)` is `u32` by its bound")] {
        assert!(got[i].clean, "POSITIVE: {what}: {}", got[i].shown);
    }
}

/// A callee whose output is `Self::Value` in an impl of a trait WITH a lifetime (serde's
/// visitors; head record 29, types.rs `projection_slots`, `Through::SelfImpl`): its regions are
/// laid, so a caller's borrows are checked through it instead of refused (`NotLaid/
/// ImplTraitLifetimes/Output`). The borrow it returns must still be found when it escapes:
/// `V.visit(&n)` is `&'de u8` with `'de` the borrow of the local `n`, returned as `&'static`
/// (rustc: E0515, "cannot return value referencing local variable `n`").
/// TWIN: the same call on a `&'static u8` is clean.
/// UNSURE: whether the caller reads the impl member's head or the trait method's (both are
/// laid now); the assertion is on the answer either way. NOT YET RUN.
#[test]
fn a_borrow_through_self_name_of_an_impl_with_a_lifetime_is_checked() {
    const VISIT: &str = r#"
pub trait Visitor<'de> { type Value; fn visit(self, x: &'de u8) -> Self::Value; }
pub struct V;
impl<'de> Visitor<'de> for V { type Value = &'de u8; fn visit(self, x: &'de u8) -> Self::Value { x } }
fn escapes() -> &'static u8 { let n = 1u8; V.visit(&n) }
fn stays(x: &'static u8) -> &'static u8 { V.visit(x) }
fn main() {}
"#;
    let got = check_bodies(VISIT, None, &["escapes", "stays"]);
    println!("self name through an impl with a lifetime: escapes {:?}; stays clean {}", got[0].diags, got[1].clean);
    // `check_bodies` is the TYPE check (and the witness decision); it never runs the move and
    // borrow check, so E0515 cannot be asked here (it was, and read as a soundness miss on
    // 2026-11-21). The borrow check's answer for these bodies is asked in
    // tests/front2_borrow_self_name.rs (`escapes` E0515; twins `stays`, `passes` clean).
    assert!(got[0].clean && got[1].clean, "`V.visit(..)` through `Self::Value` type-checks: {} {}", got[0].shown, got[1].shown);
    assert!(got[1].reads.is_some(), "TWIN: a `&'static u8` through the same call is checked: {}", got[1].shown);
}

/// Case 6, W3 (a): `ping` and `pong` await each other's boxed futures and only `go` asks
/// `Send` (no body asks an auto trait of a future it is checked to define). The check loop
/// files `ping`'s witness, then `pong`'s (front2 opaque_auto.rs `after_auto_gap`, one opaque
/// a round), and the solver answers the cycle `ping -> pong -> ping` coinductively (the
/// Reference, "Auto traits"; rustc accepts it).
///
/// - Clean on `u8` and `u16`, with read lists that differ (`pong`'s witness is read), and that
///   differ again when only `ping` holds another value (`ping`'s witness is read): both bodies
///   are in `go`'s read list (rule 21).
/// - TWIN: `pong` holds an `Rc<u8>` across its await: one E0277 naming `Send` and the `Rc`.
///
/// The case where each body itself asks `Send` of the other's future is the typed cycle gap
/// (front2_witness_records.rs, `ping`/`pong`). NOT YET RUN.
#[test]
fn a_cycle_of_two_futures_is_decided_through_both_witnesses() {
    let small = check_bodies(one_crate(HELD_U8), None, &["go"]);
    let wide = check_bodies(one_crate(HELD_U16), None, &["go"]);
    let rc = check_bodies(one_crate(HELD_RC), None, &["go"]);
    let ping_text = "async fn ping(n: u32) { if n > 0 { Box::pin(pong(n - 1)).await } }";
    let ping_holds = leak(one_crate(HELD_U8).replace(ping_text, "async fn ping(n: u32) { let h = 1u16; if n > 0 { Box::pin(pong(n - 1)).await } drop(h); }"));
    assert!(ping_holds.contains("let h = 1u16;"), "the fixture's `ping` was replaced");
    let other_ping = check_bodies(ping_holds, None, &["go"]);
    for (world, got) in [("u8", &small[0]), ("u16", &wide[0]), ("ping holds u16", &other_ping[0])] {
        assert!(got.clean && got.reads.is_some(), "`go` ({world}): clean with a read list (rustc accepts it): {}", got.shown);
    }
    assert_ne!(small[0].reads, wide[0].reads, "`go` reads `pong`'s witness: its digest moves with what `pong` holds");
    assert_ne!(small[0].reads, other_ping[0].reads, "`go` reads `ping`'s witness: its digest moves with what `ping` holds");
    let [(code, text)] = rc[0].diags.as_slice() else { panic!("TWIN: `go` must be one Send error: {}", rc[0].shown) };
    assert_eq!(*code, "E0277", "TWIN: {}", rc[0].shown);
    assert!(text.contains("Send") && text.contains("Rc<u8"), "TWIN: names `Send` and the `Rc<u8>` `pong` holds: {text}");
    println!("go: clean (u8, u16), digests move with both bodies; Rc: E0277: {text}");
}
