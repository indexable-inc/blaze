//! Closure captures as PLACES (W4), by `Send`: the Reference, "Closure types", "Capture
//! precision" and "Capture modes"; RFC 2229. A closure's environment holds each captured place,
//! so its auto traits are those of the places' stored types (`T`, `&T`, `&mut T`), not of the
//! whole bindings (blaze-typeck capture_places.rs, `places_tuple`). Every answer is rustc's.
//! Each rule has its twin (rule 23).
//!
//! 1. REFUTATION 1, a mixed capture. A non-`move` closure that moves `s.a` and reads
//!    `s.b: Cell<u8>` captures `s.a` by value and `&s.b`; `&Cell<u8>` is not `Send` (`Cell` is
//!    not `Sync`), so the closure is not, though `S1` is `Send`.
//!    - `mixed`: E0277 naming `Send`.
//!    - TWIN `mixed_move`: `move` captures both by value (`String`, `Cell<u8>`): clean.
//! 2. REFUTATION 2, a by-value field. With `unsafe impl Send for S2`, moving `s.rc` captures the
//!    `Rc<u8>` alone, not `S2`, so the closure is not `Send`.
//!    - `field_rc`: E0277 naming `Send`.
//!    - TWIN `whole_rc`: moving `s` captures `S2`, which is `Send` by its impl: clean.
//! 3. Precision against edition 2018. A closure reading `s.n` (a `Copy` field, by-value use, so
//!    an immutable borrow: "Capture modes") beside an `Rc` field captures `&s.n: &u8`.
//!    - `field_ref` (2021): clean.
//!    - TWIN `field_ref` in a 2018 crate: whole `s` is captured, `&S3`, and `S3` is not `Sync`:
//!      E0277 (the Reference, "Closure types", edition differences: before 2021 a closure
//!      captures whole variables).
//! 4. `move` truncates at the first deref (RFC 2229, "move closures": a place through a
//!    reference captures the reference).
//!    - `move_through_mut` (`r: &mut S3`, `move || r.n`): captures `r: &mut S3` by value;
//!      `&mut S3: Send` needs `S3: Send`, not held: E0277.
//!    - TWIN `ref_through_mut`: the same without `move` captures `&(*r).n: &u8` (the last deref
//!      is of a `&mut`, so the optimization truncation does not apply): clean.
//! 5. An async block holding a closure across an `.await` (the Reference, "Async blocks":
//!    captures as closures do).
//!    - `holds_closure`: `async move { let f = || s.len(); tick().await; f() }` with
//!      `s: String`: clean.
//!    - TWIN `holds_closure_rc`: the same over `Rc<u8>`: E0277 naming `Send`.
//!
//! 6. `repr(packed)`: a borrow of a packed struct's field (a `Copy` read is one) captures the
//!    STRUCT (rustc upvar.rs `restrict_repr_packed_field_ref_capture`, any alignment).
//!    - `packed_ref` (`|| p.n` over `#[repr(packed)] PkRc { n: u32, rc: Rc<u8> }`): captures
//!      `&PkRc`, and `PkRc` is not `Sync`: E0277 naming `Send`.
//!    - TWIN `unpacked_ref` (the same struct without `repr(packed)`): `&u32`: clean.
//!    - TWIN `packed_by_value` (`move || p.n`: a `Copy` read, so a borrow truncated to `p`,
//!      then `move` takes `p` by value: `PkRc` holds an `Rc`): E0277.
//!    - `packed_moved_field` (`|| drop(p.s)` over `PkS { s: String, rc: Rc<u8> }`: a by-value
//!      use keeps the field `p.s`): clean, stage 2 (below).
//!
//! 7. An async block borrowing one field of a local, sent. `async { let n = &s.n; tick().await;
//!    n.len() }` over `S4 { n: String, rc: Rc<u8> }` captures `&s.n: &String`; the `Rc` field
//!    it does not use is not captured (the Reference, "Async blocks": captures as closures do).
//!    The exact interior takes the places (blaze-flow-lower coroutine.rs `parts_with`, front2
//!    stalled_goals.rs `storage_tuple`) and the flow's saved locals.
//!    - `field_borrow_async`: clean, `Send` by the field's type.
//!    - TWIN `field_borrow_async_rc`: borrowing `s.rc`: E0277.
//!    - TWIN `field_borrow_async` in a 2018 crate: whole `s` by reference, `&S4`: E0277.
//!
//! STAGE 2 (red until the layout holds one field per place, `ActiveEnv = Places`: before it a
//! by-value row whose places are not the whole binding is laid as the whole binding moved, and
//! the witness counts it, blaze-typeck env_fields.rs `moved_whole_beside_places`):
//! 8. `field_move_async` (`async move { let n = s.n; tick().await; n.len() }`): moves `s.n`
//!    alone: clean. TWIN `field_move_async_rc` (moves `s.rc`): E0277.
//!
//! NOT YET RUN (written 2026-11-21 by a write-only lane).

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::refusal::SourceFile;

const SOURCE: &str = r#"
use std::cell::Cell;
use std::rc::Rc;

fn send<T: Send>(_: T) {}
async fn tick() {}

pub struct S1 { pub a: String, pub b: Cell<u8> }
fn mixed(s: S1) { send(|| { drop(s.a); s.b.get(); }); }
fn mixed_move(s: S1) { send(move || { drop(s.a); s.b.get(); }); }

pub struct S2 { pub rc: Rc<u8>, pub n: u8 }
unsafe impl Send for S2 {}
fn field_rc(s: S2) { send(move || { let _r = s.rc; }); }
fn whole_rc(s: S2) { send(move || { let _s = s; }); }

pub struct S3 { pub n: u8, pub rc: Rc<u8> }
fn field_ref(s: S3) { send(|| s.n + 1); }
fn move_through_mut(r: &mut S3) { send(move || r.n); }
fn ref_through_mut(r: &mut S3) { send(|| r.n); }

fn holds_closure(s: String) { send(async move { let f = || s.len(); tick().await; f() }); }
fn holds_closure_rc(s: Rc<u8>) { send(async move { let f = || *s; tick().await; f() }); }

pub struct S4 { pub n: String, pub rc: Rc<u8> }
fn field_borrow_async(s: S4) { send(async { let n = &s.n; tick().await; n.len() }); }
fn field_borrow_async_rc(s: S4) { send(async { let r = &s.rc; tick().await; **r }); }
fn field_move_async(s: S4) { send(async move { let n = s.n; tick().await; n.len() }); }
fn field_move_async_rc(s: S4) { send(async move { let r = s.rc; tick().await; *r }); }

#[repr(packed)]
pub struct PkRc { pub n: u32, pub rc: Rc<u8> }
pub struct UnpkRc { pub n: u32, pub rc: Rc<u8> }
#[repr(packed)]
pub struct PkS { pub s: String, pub rc: Rc<u8> }
fn packed_ref(p: PkRc) { send(|| p.n); }
fn unpacked_ref(p: UnpkRc) { send(|| p.n); }
fn packed_by_value(p: PkRc) { send(move || p.n); }
fn packed_moved_field(p: PkS) { send(|| drop(p.s)); }

fn main() {}
"#;

/// What one body's check said, kept past the checker.
#[derive(Debug)]
struct Said {
    name: String,
    clean: bool,
    codes: Vec<&'static str>,
    texts: Vec<String>,
    stopped: bool,
}

fn verdicts(edition: &str, wanted: &[&str]) -> Vec<Said> {
    let alloc = blaze_own::testing::heap().alloc();
    let home = std::env::var("HOME").expect("HOME");
    let tc = Toolchain::open(&mut DirShelf::new(format!("{home}/.blaze/v2/toolchain"))).expect("installed toolchain");
    let root = "/caps/src/main.rs";
    let files = [(root.to_owned(), SOURCE.to_owned())];
    let user = UserCrate {
        name: "caps", dir: "/caps/", root, files: &files,
        edition: edition::PackageEdition::of_manifest(edition).expect("edition"), panic: blaze_build::Panic::Unwind,
    };
    let inputs = Inputs::new(&user, &tc).expect("fixture crate");
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).expect("fixture names");
    let sources = [SourceFile { path: "src/main.rs", text: SOURCE }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("checker frame");
    let mut world = ProjectWorld::new(&names, &frame, &sources, alloc).expect("body checker");
    let mut out = Vec::new();
    for &name in wanted {
        let key = names.items(0).find_map(|(key, _, _)| (names.item_text(key)?.name == Some(name)).then_some(key)).expect(name);
        let v = world.check(BodyRef::Free(key), alloc);
        let said = Said {
            name: format!("{name} ({edition})"),
            clean: v.is_clean() && v.reads.is_some(),
            codes: v.diagnostics.iter().map(|d| d.code).collect(),
            texts: v.diagnostics.iter().map(|d| d.text.to_string()).collect(),
            stopped: !(v.gaps.is_empty() && v.undecided.is_empty() && v.refused.is_none()),
        };
        // The verdicts first, every one, before any assertion.
        println!("{}: clean {} codes {:?} stopped {} {:?}", said.name, said.clean, said.codes, said.stopped, said.texts);
        if said.stopped {
            println!("  stopped by: gaps {:?} undecided {:?} refused {:?}", v.gaps, v.undecided, v.refused);
        }
        out.push(said);
    }
    out
}

fn clean(rows: &[Said], name: &str) {
    let s = rows.iter().find(|s| s.name.starts_with(name) && s.name[name.len()..].starts_with(' ')).expect("checked above");
    assert!(s.clean && !s.stopped, "`{}`: rustc accepts it: {s:?}", s.name);
}

fn not_send(rows: &[Said], name: &str) {
    let s = rows.iter().find(|s| s.name.starts_with(name) && s.name[name.len()..].starts_with(' ')).expect("checked above");
    assert!(!s.stopped, "`{}`: a gap where rustc decides E0277: {s:?}", s.name);
    assert_eq!(s.codes.as_slice(), ["E0277"], "`{}`: rustc reports one E0277: {s:?}", s.name);
    assert!(s.texts[0].contains("Send"), "`{}`: the E0277 names `Send`: {s:?}", s.name);
}

#[test]
fn captured_places_decide_send() {
    let rows = verdicts("2021", &[
        "mixed", "mixed_move", "field_rc", "whole_rc", "field_ref", "move_through_mut", "ref_through_mut",
        "holds_closure", "holds_closure_rc",
    ]);
    for n in ["mixed_move", "whole_rc", "field_ref", "ref_through_mut", "holds_closure"] {
        clean(&rows, n);
    }
    for n in ["mixed", "field_rc", "move_through_mut", "holds_closure_rc"] {
        not_send(&rows, n);
    }
}

#[test]
fn edition_2018_captures_whole_bindings() {
    let rows = verdicts("2018", &["field_ref", "whole_rc"]);
    not_send(&rows, "field_ref");
    // Control: the 2018 crate checks at all (a whole-binding capture of a `Send` type).
    clean(&rows, "whole_rc");
}

#[test]
fn a_borrow_of_a_packed_field_captures_the_struct() {
    let rows = verdicts("2021", &["packed_ref", "unpacked_ref", "packed_by_value"]);
    clean(&rows, "unpacked_ref");
    not_send(&rows, "packed_ref");
    not_send(&rows, "packed_by_value");
}

/// Stage 2: before the switch the closure's witness counts `p` moved whole beside `p.s`
/// (capture_places.rs `places_tuple`), which holds the `Rc`.
#[test]
#[ignore = "stage 2: needs blaze-typeck `ActiveEnv = Places` (the switch removes this)"]
fn a_move_of_a_packed_field_keeps_the_field_needs_stage2() {
    let rows = verdicts("2021", &["packed_moved_field"]);
    clean(&rows, "packed_moved_field");
}

#[test]
fn an_async_block_borrowing_one_field_is_send_by_that_field() {
    let rows = verdicts("2021", &["field_borrow_async", "field_borrow_async_rc"]);
    clean(&rows, "field_borrow_async");
    not_send(&rows, "field_borrow_async_rc");
    let old = verdicts("2018", &["field_borrow_async"]);
    not_send(&old, "field_borrow_async");
}

#[test]
#[ignore = "stage 2: needs blaze-typeck `ActiveEnv = Places` (the switch removes this)"]
fn async_block_moving_one_field_needs_stage2() {
    let rows = verdicts("2021", &["field_move_async", "field_move_async_rc"]);
    clean(&rows, "field_move_async");
    not_send(&rows, "field_move_async_rc");
}
