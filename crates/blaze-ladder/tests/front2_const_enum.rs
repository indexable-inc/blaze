//! A const parameter whose type is a fieldless enum, on front2 with the real core
//! (front2/const_enum.rs). The evidence: the walk of hello world left out 139 functions
//! ("128 x check: Head; the first: core::atomic_load ..: Unsupported(\"a const parameter whose
//! type is not an integer, `char` or `bool` (`adt_const_params`)\" ..)" and "11 x lower:
//! Generics" with the same text).
//!
//! That the pinned nightly accepts the form: the pinned core declares and calls it
//! (library/core/src/intrinsics/mod.rs):
//!
//! ```text
//! #[derive(Debug, ConstParamTy, PartialEq, Eq)]
//! pub enum AtomicOrdering {
//!     Relaxed = 0,
//!     Release = 1,
//!     Acquire = 2,
//!     AcqRel = 3,
//!     SeqCst = 4,
//! }
//! pub const unsafe fn atomic_load<T: Copy, const ORD: AtomicOrdering, const VOLATILE: bool>(
//!     src: *const T,
//! ) -> T;
//! ```
//!
//! and library/core/src/sync/atomic.rs: `use crate::intrinsics::AtomicOrdering as AO;` ..
//! `SeqCst => intrinsics::atomic_load::<T, { AO::SeqCst }, VOLATILE>(dst),`.
//!
//! What is right, and what each case here must say:
//!
//! ```text
//! head load: built            fn load<T: Copy, const O: Ord>(p: *const T) -> T
//! head W: built               struct W<const O: Ord>;
//! la: clean                   load::<u8, { Ord::A }>(p)
//! lb: clean                   load::<u8, { Ord::B }>(p)
//! same: clean                 W<{ Ord::A }> given where W<{ Ord::A }> is expected
//! differ: Mismatch            W<{ Ord::A }> given where W<{ Ord::B }> is expected: two
//!                             variants are two values (a fix that erases the value fails here)
//! other: Mismatch             load::<u8, { Other::X }>(p): `Other::X` has `Ord::A`'s index and
//!                             discriminant and is still not an `Ord`
//! bad: no head, "mismatched types"    fn bad(w: W<{ Other::X }>)
//! ```
//!
//! A const parameter of a struct or data enum: tests/front2_const_param_adt.rs.
//!
//! and the check script's line: `test blaze-ladder: exit 0; 1 passed, 0 failed`.
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`, as tests/front2_params.rs.

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::body::lower_fn;
use blaze_ladder::front2::check::{BuildOrder, CheckFail, Records, check_body};
use blaze_ladder::front2::heads::{Store, Want};
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::refusal::SourceFile;
use blaze_lower::scoped::Prepares;
use blaze_own::Alloc;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};

const MAIN: &str = "fn main(cx: &Cx, io: Io) -> Result<(), Error> {
    Ok(())
}
enum Ord { A = 0, B = 1 }
enum Other { X = 0 }
struct W<const O: Ord>;
fn load<T: Copy, const O: Ord>(p: *const T) -> T { loop {} }
fn la(p: *const u8) -> u8 { load::<u8, { Ord::A }>(p) }
fn lb(p: *const u8) -> u8 { load::<u8, { Ord::B }>(p) }
fn same(w: W<{ Ord::A }>) -> W<{ Ord::A }> { w }
fn differ(w: W<{ Ord::A }>) -> W<{ Ord::B }> { w }
fn other(p: *const u8) -> u8 { load::<u8, { Other::X }>(p) }
fn bad(w: W<{ Other::X }>) {}
";

/// What a body must say.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Says {
    Clean,
    Mismatch,
}

/// The bodies, each with what it proves.
const BODIES: [(&str, Says, &str); 5] = [
    ("la", Says::Clean, "`load::<u8, { Ord::A }>(p)`: a unit variant is a value of the parameter's enum type"),
    ("lb", Says::Clean, "`load::<u8, { Ord::B }>(p)`: so is the other variant"),
    ("same", Says::Clean, "`W<{ Ord::A }>` is `W<{ Ord::A }>`: one variant is one value"),
    ("differ", Says::Mismatch, "`W<{ Ord::A }>` is not `W<{ Ord::B }>`: two variants are two values"),
    ("other", Says::Mismatch, "`{ Other::X }` where an `Ord` is expected: the value carries its enum"),
];

/// The fns whose head must be refused, each with a text its refusal must hold.
const NO_HEAD: [(&str, &str, &str); 1] = [
    ("bad", "mismatched types", "`W<{ Other::X }>` in a signature: a variant of another enum than the parameter's"),
];

#[test]
fn enum_const_params_through_front2() {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/cen/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest("blaze-2026.11").expect("a known edition");
    let user = UserCrate { name: "cen", dir: "/cen/", root: "/cen/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: MAIN }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
    let mut front = Front2::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global());
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut buckets = None;
    let mut failed: Vec<String> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    let key_of = |name: &str| {
        names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: &[name] }, Alloc::global())
            .ok()
            .and_then(|f| f.whole(1))
            .map(|d| d.key)
            .unwrap_or_else(|| panic!("`{name}` from the crate's root"))
    };

    // The head of `load` builds: its const parameter's type is a fieldless enum.
    match store.ensure(&names, &mut front.keys, Want::Fn(key_of("load"))) {
        Ok(_) => notes.push("head load: built".to_string()),
        Err(e) => {
            notes.push(format!("head load: {e:?}"));
            failed.push("head load".to_string());
        }
    }

    // The heads that must be refused, each naming why.
    for (name, holds, proves) in NO_HEAD {
        let said = match store.ensure(&names, &mut front.keys, Want::Fn(key_of(name))) {
            Ok(_) => "built".to_string(),
            Err(e) => format!("{e:?}"),
        };
        let _ = proves;
        if said.contains(holds) {
            notes.push(format!("{name}: no head, {holds:?}"));
        } else {
            let brief: String = said.chars().take(240).collect();
            notes.push(format!("{name}: wanted a refusal holding {holds:?}: {brief}"));
            failed.push(name.to_string());
        }
    }

    let mut bodies = Vec::new();
    for (name, want, proves) in BODIES {
        let key = key_of(name);
        if let Err(e) = store.ensure(&names, &mut front.keys, Want::Fn(key)) {
            let brief: String = format!("{e:?}").chars().take(240).collect();
            notes.push(format!("{name}: no head: {brief}"));
            failed.push(name.to_string());
            continue;
        }
        match lower_fn(&names, prepared, &mut front.keys, &mut store, key) {
            Ok(b) => bodies.push((name, b, want, proves)),
            Err(e) => {
                let brief: String = format!("{e}").chars().take(240).collect();
                notes.push(format!("{name}: does not lower: {brief}"));
                failed.push(name.to_string());
            }
        }
    }
    for (name, body, want, proves) in &bodies {
        let view = body.lowered.view();
        let spell = Spelling::of(&view).expect("spelling");
        let scope = body.scope(&spell);
        let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: None };
        let said = match check_body(&mut records, body, &scope) {
            Ok(done) if done.diags.is_empty() => "clean".to_string(),
            Ok(done) => {
                let codes: Vec<String> = done.diags.iter().map(|d| format!("{:?}", d.code)).collect();
                format!("diagnostics {codes:?}")
            }
            Err(CheckFail::Diags { diags, .. }) => {
                let codes: Vec<String> = diags.iter().map(|d| format!("{:?}", d.0)).collect();
                format!("diagnostics {codes:?}")
            }
            Err(other) => format!("{other:?}"),
        };
        let ok = match want {
            Says::Clean => said == "clean",
            // Exactly the one mismatch: nothing else is wrong with these bodies.
            Says::Mismatch => said == "diagnostics [\"Mismatch\"]",
        };
        let _ = proves;
        let brief: String = said.chars().take(240).collect();
        notes.push(format!("{name}: {brief} (wanted {want:?})"));
        if !ok {
            failed.push(name.to_string());
        }
    }
    assert_eq!(bodies.len(), BODIES.len(), "every body lowered\n{}", notes.join("\n"));
    assert!(failed.is_empty(), "cases that did not say what is stated: {failed:?}\n{}", notes.join("\n"));
}
