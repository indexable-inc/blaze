//! static-param-gap-1: a caller's own type parameter passed where the callee requires
//! `T: 'static` (or `T: 'a` for an `'a` the caller's lifetimes reach) must be bounded by the
//! caller (blaze-flow-lower param_outlives.rs). Before, it was accepted: the parameter has no
//! origin position, so the call took `call_by_signature`, which reads no bounds.
//!
//! The Reference, "Trait and lifetime bounds": "`T: 'a` means that all lifetime parameters of
//! `T` outlive `'a`"; a type parameter outlives only what its bounds and its signature's
//! well-formed types say (rustc E0310 otherwise).
//!
//! - `caller_unbounded<U>(u: U) { need(u) }`: E0310 (a finding with code "E0310"), refused None.
//! - `caller_static<U: 'static>(u: U) { need(u) }`: CLEAN.
//! - `implied<'x, U>(u: &'x U, r: &'x u8) -> &'x u8 { hold(u, r) }` with
//!   `hold<'a, T: 'a>(t: T, r: &'a u8) -> &'a u8`: `T = &'x U`, so `U: 'a`, and `'a` flows into
//!   the result `'x`: `U: 'x` is owed, and implied by the input `&'x U`: CLEAN.
//! - `caller_traited<U: Clone>(u: U) { need(u) }`: a trait bound's implied outlives are not
//!   read, so not an error but a refusal by name: `refused Some(Lower(Refusal { form:
//!   ParamOutlivesUnread { param: 0 }, .. }))`.
//!
//! CONTROLS (fail without the fix): with lib.rs `call_done`'s `owes` taken out (`owes = None`),
//! `caller_unbounded` prints `[] refused None` and its assertion fails: the gap as it was.
//! With `signature`'s `self.sig_param_bounds(..)` call taken out, `implied` prints E0310: the
//! implied bound is what makes it clean.
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`.

#![feature(allocator_api)]

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::borrows::BorrowVerdict;
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::refusal::SourceFile;

const EDITION: &str = "blaze-2026.11";

const MAIN: &str = "fn main() {}

fn need<T: 'static>(t: T) {
    let _ = t;
}

fn hold<'a, T: 'a>(t: T, r: &'a u8) -> &'a u8 {
    let _ = t;
    r
}

fn caller_unbounded<U>(u: U) {
    need(u)
}

fn caller_static<U: 'static>(u: U) {
    need(u)
}

fn implied<'x, U>(u: &'x U, r: &'x u8) -> &'x u8 {
    hold(u, r)
}

fn caller_traited<U: Clone>(u: U) {
    need(u)
}
";

fn shown(item: &str, v: &BorrowVerdict<'_>) -> String {
    let found: Vec<String> = v.findings.iter().map(|d| format!("{} at {:?}", d.code, item.get(d.start as usize..d.end as usize).unwrap_or("<out of range>"))).collect();
    format!("{found:?} refused {:?}", v.refused)
}

#[test]
fn a_callers_type_parameter_owes_the_callees_outlives_bound() {
    let heap = blaze_own::testing::heap();
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/spg/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
    let user = UserCrate { name: "spg", dir: "/spg/", root: "/spg/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));

    let sources = [SourceFile { path: "src/main.rs", text: MAIN }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command's frame");
    let mut world = ProjectWorld::new(&names, &frame, &sources, heap.alloc()).unwrap_or_else(|e| panic!("front2: {e}"));
    let bodies = world.bodies(heap.alloc());
    let name_of = |b: &BodyRef<'_>| -> String {
        match b {
            BodyRef::Free(k) => names.item_text(*k).and_then(|t| t.name).unwrap_or("?").to_string(),
            BodyRef::Member { name, .. } => name.to_string(),
        }
    };
    let find = |n: &str| bodies.iter().find(|(b, _)| name_of(b) == n).map(|(b, _)| *b).unwrap_or_else(|| panic!("`{n}` listed"));
    let text_of = |b: BodyRef<'_>| {
        let k = match b {
            BodyRef::Free(k) => k,
            BodyRef::Member { container, .. } => container,
        };
        names.item_text(k).expect("the body's item text").text
    };

    // `U` is not known to be `'static`: E0310 (accepted before).
    let b = find("caller_unbounded");
    let (types, v) = world.check_borrows(b, heap.alloc());
    let item = text_of(b);
    println!("borrows caller_unbounded: {}", shown(item, &v));
    assert!(types.is_clean(), "caller_unbounded's types: {types:?}");
    assert!(v.refused.is_none(), "caller_unbounded not checked: {}", shown(item, &v));
    assert!(v.findings.iter().any(|d| d.code == "E0310"), "caller_unbounded: no E0310: {}", shown(item, &v));

    // Declared `U: 'static`: clean.
    let b = find("caller_static");
    let (types, v) = world.check_borrows(b, heap.alloc());
    let item = text_of(b);
    println!("borrows caller_static: {}", shown(item, &v));
    assert!(types.is_clean(), "caller_static's types: {types:?}");
    assert!(v.refused.is_none(), "caller_static not checked: {}", shown(item, &v));
    assert!(v.findings.is_empty(), "caller_static: {}", shown(item, &v));

    // `U: 'x` owed through `'a` into the result, implied by the input `&'x U`: clean.
    let b = find("implied");
    let (types, v) = world.check_borrows(b, heap.alloc());
    let item = text_of(b);
    println!("borrows implied: {}", shown(item, &v));
    assert!(types.is_clean(), "implied's types: {types:?}");
    assert!(v.refused.is_none(), "implied not checked: {}", shown(item, &v));
    assert!(v.findings.is_empty(), "implied: {}", shown(item, &v));

    // A trait bound whose implied outlives are not read: refused by name, never an error.
    let b = find("caller_traited");
    let (types, v) = world.check_borrows(b, heap.alloc());
    let item = text_of(b);
    let line = shown(item, &v);
    println!("borrows caller_traited: {line}");
    assert!(types.is_clean(), "caller_traited's types: {types:?}");
    assert!(v.findings.is_empty(), "caller_traited: {line}");
    assert!(line.contains("ParamOutlivesUnread"), "caller_traited: {line}");
}
