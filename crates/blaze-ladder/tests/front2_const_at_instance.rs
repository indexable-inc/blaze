//! A trait's associated constant read at a generic `T` has ONE VALUE PER INSTANCE, the selected
//! impl's (hello-e, behind hello Class Q). bstd's `fn grant<T: Grant>(..) { .. T::KIND.name() .. }`
//! (guest.rs) is walked at each `T` a `main` takes; the run path pulls `T::KIND` at the instance's
//! ground `T` (front2/const_at.rs `pull` -> `Consts::value_with_args` -> consts.rs `select_args`
//! -> const_select.rs `select_made`, the checker's solver). The `main!` expansion's
//! `static MANIFEST` reads `<Out as Grant>::KIND` at a written `Out` (the same selection, inside
//! the static's evaluation step: consts.rs `ConstsWorld::resolve` -> `select_once`).
//!
//! The rules. The Reference, "Associated Items", "Associated constants": "An associated constant
//! definition defines a constant associated with a type", so `<T as Tr>::K` is the definition in
//! the impl of `Tr` that holds for `T`; at `T = A` it is `impl Tr for A`'s and at `T = B` it is
//! `impl Tr for B`'s. "Static items": "The static initializer is a constant expression evaluated
//! at compile time", so `static S: u8 = <A as Tr>::K;` has a value. bstd grant.rs:
//! `impl Grant for Out { const KIND: Kind = Kind::Out; .. }` and
//! `impl Grant for crate::clock::Clock { const KIND: Kind = Kind::Clock; .. }`.
//!
//! Expected lines (eprintln, and FRONT2_WORK_LIST when set):
//!   "LOCAL: S = 3; K at A = 3; K at B = 5"
//!   "BSTD: KIND at Out = OUT_KIND's bytes; KIND at Clock = CLOCK_KIND's bytes; SELECTED, OUT_NAME, MANIFEST checked"
//! TODAY the bstd half is expected to fail with Class Q's gap (`checker key .. is in no row of the
//! front end's key map`) on SELECTED or MANIFEST; the panic line names the static and the key
//! whole. Needs `$HOME/.blaze/v2/toolchain`. NEVER RUN by its writer (hello-e, 2026-11-16).

#![feature(allocator_api)]

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::check::{BuildOrder, Records};
use blaze_ladder::front2::consts::{Consts, int_of};
use blaze_ladder::front2::heads::Store;
use blaze_ladder::front2::keys::MemberKind;
use blaze_ladder::refusal::SourceFile;
use blaze_lower::scoped::Prepares;
use blaze_own::Alloc;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};
use blaze_typeck as tc;

const MAIN: &str = "use std::clock::Clock;
use std::grant::{Grant, Kind, Manifest, Param};
use std::out::Out;

fn main(cx: &Cx, out: Out) -> Result<(), Error> {
    Ok(())
}

trait Tr { const K: u8; }
struct A;
struct B;
impl Tr for A { const K: u8 = 3; }
impl Tr for B { const K: u8 = 5; }
static S: u8 = <A as Tr>::K;

fn read_k<T: Tr>() -> u8 { T::K }
fn kind_name<T: Grant>() -> &'static str { T::KIND.name() }

static OUT_KIND: Kind = Kind::Out;
static CLOCK_KIND: Kind = Kind::Clock;
static SELECTED: Kind = <Out as Grant>::KIND;
static OUT_NAME: &str = <Out as Grant>::KIND.name();
static MANIFEST: Manifest = Manifest::new(&[Param::new(\"out\", <Out as Grant>::KIND)]);
";

fn note(s: &str) {
    eprintln!("{s}");
    if let Ok(p) = std::env::var("FRONT2_WORK_LIST") {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
            let _ = writeln!(f, "{s}");
        }
    }
}

/// The root allocation's bytes of a record.
fn root_bytes(rec: &blaze_consteval::ConstRecord<Alloc<'_>>) -> Vec<u8> {
    rec.data.allocs.first().map(|a| a.bytes.iter().copied().collect()).unwrap_or_default()
}

#[test]
fn trait_const_has_one_value_per_instance() {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc_ = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/inst/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest("blaze-2026.11").expect("a known edition");
    let user = UserCrate { name: "inst", dir: "/inst/", root: "/inst/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc_).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: MAIN }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
    let mut front = Front2::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global());
    let mut buckets = None;
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let root = names.root(0);
    let lib = |ns: Namespace, name: &str| {
        names::path(&names, root, ns, Written { kind: PathKind::Plain, segments: &[name] }, Alloc::global())
            .ok()
            .and_then(|f| f.whole(1))
            .map(|d| d.key)
            .unwrap_or_else(|| panic!("`{name}` from the crate root"))
    };
    let key = |ns: Namespace, n: &str| front.keys.item(lib(ns, n)).unwrap_or_else(|e| panic!("{n}'s key: {e:?}"));
    let [s, out_kind, clock_kind, selected, out_name, manifest] =
        ["S", "OUT_KIND", "CLOCK_KIND", "SELECTED", "OUT_NAME", "MANIFEST"].map(|n| key(Namespace::Values, n));
    let [tr, a, b, grant, out, clock] = ["Tr", "A", "B", "Grant", "Out", "Clock"].map(|n| key(Namespace::Types, n));
    let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: None };
    let mut consts = Consts::default();
    let unit = |k: tc::ItemKey| tc::PNode::Adt(k, tc::List::EMPTY);

    // The local half: one crate, two impls.
    let s_val = consts.value(&mut records, prepared, s).map(int_of).unwrap_or_else(|e| panic!("static S = <A as Tr>::K is evaluated: {e}"));
    let k = records.keys.filed_member(tr, MemberKind::Const, "K").expect("Tr's head was made by S's evaluation, and K filed");
    let at_a = consts.trait_const_value(&mut records, prepared, k, unit(a)).map(int_of).unwrap_or_else(|e| panic!("<A as Tr>::K: {e}"));
    let at_b = consts.trait_const_value(&mut records, prepared, k, unit(b)).map(int_of).unwrap_or_else(|e| panic!("<B as Tr>::K: {e}"));
    note(&format!("LOCAL: S = {}; K at A = {}; K at B = {}", s_val.unwrap_or(999), at_a.unwrap_or(999), at_b.unwrap_or(999)));
    assert_eq!((s_val, at_a, at_b), (Some(3), Some(3), Some(5)), "each instance reads its own impl's constant");

    // The bstd half: the library's trait and impls, the program's statics (hello's shape).
    let mut refused: Vec<String> = Vec::new();
    // Each static tried in order, its refusal kept whole on one line (a macro: the records'
    // borrow is not held across the calls).
    macro_rules! eval {
        ($name:literal, $k:expr) => {
            match consts.value(&mut records, prepared, $k) {
                Ok(rec) => Some(root_bytes(rec)),
                Err(e) => {
                    let why = format!("{e}").replace('\n', " ");
                    note(&format!("{}: refused: {why}", $name));
                    refused.push(format!("{}: {why}", $name));
                    None
                }
            }
        };
    }
    let want_out = eval!("OUT_KIND", out_kind);
    let want_clock = eval!("CLOCK_KIND", clock_kind);
    let got_selected = eval!("SELECTED", selected);
    let got_name = eval!("OUT_NAME", out_name);
    let got_manifest = eval!("MANIFEST", manifest);
    assert!(refused.is_empty(), "every static has a value (the Reference, \"Static items\"); refused: {refused:#?}");
    let kind = records.keys.filed_member(grant, MemberKind::Const, "KIND").expect("Grant's head was made, and KIND filed");
    let at_out = consts.trait_const_value(&mut records, prepared, kind, unit(out)).map(root_bytes).unwrap_or_else(|e| panic!("<Out as Grant>::KIND: {e}"));
    let at_clock = consts.trait_const_value(&mut records, prepared, kind, unit(clock)).map(root_bytes).unwrap_or_else(|e| panic!("<Clock as Grant>::KIND: {e}"));
    assert_eq!(Some(&at_out), want_out.as_ref(), "<Out as Grant>::KIND is impl Grant for Out's Kind::Out");
    assert_eq!(Some(&at_clock), want_clock.as_ref(), "<Clock as Grant>::KIND is impl Grant for Clock's Kind::Clock");
    assert_ne!(at_out, at_clock, "two instances, two impls, two values");
    assert_eq!(got_selected.as_ref(), want_out.as_ref(), "SELECTED is Kind::Out");
    assert!(got_name.is_some() && got_manifest.is_some());
    note("BSTD: KIND at Out = OUT_KIND's bytes; KIND at Clock = CLOCK_KIND's bytes; SELECTED, OUT_NAME, MANIFEST checked");
}
