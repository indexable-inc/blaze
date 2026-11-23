//! A caller's borrow check through a trait method whose `-> impl Trait` captures by `use<..>`
//! (RFC 3617; in traits since Rust 1.87): tempo's 485 bodies refused "a region position the
//! signature's walk does not lay" (blaze-int notes reports/tempo-correct-1120/c2135wave69
//! refused-by-cause.txt) all call commonware-runtime 2026.10.0's `Clock::sleep(&self, duration:
//! Duration) -> impl Future<Output = ()> + Send + 'static + use<Self>` (its impls write
//! `+ use<>`). The head is now laid (front2 types.rs `Captures`; front2_region_rules.rs
//! `a_use_bound_decides_an_opaques_captures` states the columns); this file states what the
//! caller's borrow check must then decide, rustc's behaviour for each of three opaques:
//!
//! - `sleep -> impl Sized + use<>`: captures no lifetime, so the returned value holds no borrow
//!   of `c`: `uses_sleep` (`let f = c.sleep(1); c.bump(); drop(f);`) is accepted.
//! - `keep -> impl Sized + 'static`: captures `&self`'s lifetime (a trait impl's RPIT captures
//!   every lifetime in scope), but its bound says the opaque outlives `'static`, so a live value
//!   of it keeps no region live (rustc_borrowck liveness `FreeRegionsVisitor`: an alias whose
//!   item bounds give `'static` makes no region live; rust-lang/rust#116733): `uses_keep` is
//!   accepted. `+ 'static` does NOT remove the capture; it decides liveness.
//! - TWIN (rule 23) `hold -> impl Sized`: captures `&self`'s lifetime and says nothing more, so
//!   `f` keeps `c` borrowed and `c.bump()` is E0502: `uses_hold` must be refused. A clean answer
//!   above is then the captures' and the bound's, not a check that relates nothing.
//!
//! The first two depend on the flow lowering relating an opaque output by its captures (and
//! its `'static` bound) rather than tying every input into it (blaze-flow-lower lib.rs, the
//! elision arm's `positions_all_opaque`: "a superset ... for one that captures fewer").

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::refusal::SourceFile;

const SOURCE: &str = r#"
pub trait Clock {
    fn sleep(&self, d: u32) -> impl Sized + use<Self>;
    fn keep(&self, d: u32) -> impl Sized + 'static;
    fn hold(&self, d: u32) -> impl Sized;
}
pub struct Ctx { n: u32 }
impl Ctx {
    fn bump(&mut self) { self.n += 1; }
    fn tag(&self, y: &u32) -> impl Sized + '_ { let _ = y; self.n }
}
impl Clock for Ctx {
    fn sleep(&self, d: u32) -> impl Sized + use<> { d }
    fn keep(&self, d: u32) -> impl Sized + 'static { d }
    fn hold(&self, d: u32) -> impl Sized { d }
}

fn uses_sleep(mut c: Ctx) { let f = c.sleep(1); c.bump(); drop(f); }
fn uses_keep(mut c: Ctx) { let f = c.keep(1); c.bump(); drop(f); }
// TWIN: the opaque holds `c`'s borrow while `c.bump()` borrows it mutably (rustc: E0502).
fn uses_hold(mut c: Ctx) { let f = c.hold(1); c.bump(); drop(f); }
// `+ '_` (here `&self`'s lifetime): the opaque captures `y`'s lifetime too (Rust 2024), but
// only the bound's region is live, so `y` may change while `f` lives.
fn uses_tag(c: Ctx, mut y: u32) { let f = c.tag(&y); y += 1; drop(f); drop(y); }
// TWIN: `f` keeps `c` (the bound's region) borrowed across `c.bump()` (rustc: E0502).
fn uses_tag_self(mut c: Ctx, y: u32) { let f = c.tag(&y); c.bump(); drop(f); }

fn main() {}
"#;

#[test]
fn a_callers_borrows_follow_the_opaques_captures() {
    let alloc = blaze_own::testing::heap().alloc();
    let home = std::env::var("HOME").expect("HOME");
    let tc = Toolchain::open(&mut DirShelf::new(format!("{home}/.blaze/v2/toolchain"))).expect("installed toolchain");
    let root = "/cap/src/main.rs";
    let files = [(root.to_owned(), SOURCE.to_owned())];
    let user = UserCrate {
        name: "cap", dir: "/cap/", root, files: &files,
        edition: edition::PackageEdition::of_manifest("2024").expect("Rust 2024"), panic: blaze_build::Panic::Unwind,
    };
    let inputs = Inputs::new(&user, &tc).expect("fixture crate");
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).expect("fixture names");
    let sources = [SourceFile { path: "src/main.rs", text: SOURCE }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("checker frame");
    let mut world = ProjectWorld::new(&names, &frame, &sources, alloc).expect("body checker");
    let key_of = |name: &str| names.items(0).find_map(|(key, _, _)| (names.item_text(key)?.name == Some(name)).then_some(key)).expect(name);
    for name in ["uses_sleep", "uses_keep"] {
        let (types, v) = world.check_borrows(BodyRef::Free(key_of(name)), alloc);
        assert!(types.is_clean(), "`{name}`'s types: {types:?}");
        assert!(v.refused.is_none(), "ORACLE: `{name}`'s borrows are checked (the callee's head regions are laid): {v:?}");
        assert!(v.findings.is_empty(), "ORACLE: `{name}` has no borrow finding (rustc accepts it): {v:?}");
        println!("{name}: borrows clean");
    }
    let (types, v) = world.check_borrows(BodyRef::Free(key_of("uses_hold")), alloc);
    assert!(types.is_clean(), "`uses_hold`'s types: {types:?}");
    assert!(v.refused.is_none(), "TWIN: `uses_hold`'s borrows are checked: {v:?}");
    assert!(v.findings.iter().any(|d| d.code == "E0502"), "TWIN: `uses_hold` must be E0502 (`f` keeps `c` borrowed across `c.bump()`): {v:?}");
    println!("uses_hold: E0502");
}

/// `+ 'a` on an opaque (here `+ '_`, `&self`'s): only `'a` is kept live by the result
/// (rust-lang/rust#116733: an alias whose item bounds are all one region `'r` makes only `'r`
/// live), though the opaque captures every lifetime in scope. `uses_tag` (changes `y`, which
/// `f` captures but does not keep live) is accepted; the TWIN `uses_tag_self` (changes `c`,
/// the bound's region) is E0502. Every verdict is printed before any assert.
#[test]
fn an_opaques_region_bound_keeps_only_that_region_live() {
    let alloc = blaze_own::testing::heap().alloc();
    let home = std::env::var("HOME").expect("HOME");
    let tc = Toolchain::open(&mut DirShelf::new(format!("{home}/.blaze/v2/toolchain"))).expect("installed toolchain");
    let root = "/cap/src/main.rs";
    let files = [(root.to_owned(), SOURCE.to_owned())];
    let user = UserCrate {
        name: "cap", dir: "/cap/", root, files: &files,
        edition: edition::PackageEdition::of_manifest("2024").expect("Rust 2024"), panic: blaze_build::Panic::Unwind,
    };
    let inputs = Inputs::new(&user, &tc).expect("fixture crate");
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).expect("fixture names");
    let sources = [SourceFile { path: "src/main.rs", text: SOURCE }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("checker frame");
    let mut world = ProjectWorld::new(&names, &frame, &sources, alloc).expect("body checker");
    let key_of = |name: &str| names.items(0).find_map(|(key, _, _)| (names.item_text(key)?.name == Some(name)).then_some(key)).expect(name);
    let (tag_types, tag) = world.check_borrows(BodyRef::Free(key_of("uses_tag")), alloc);
    println!("uses_tag: types clean {}, {tag:?}", tag_types.is_clean());
    let (twin_types, twin) = world.check_borrows(BodyRef::Free(key_of("uses_tag_self")), alloc);
    println!("uses_tag_self: types clean {}, {twin:?}", twin_types.is_clean());
    assert!(tag_types.is_clean() && twin_types.is_clean(), "the fixture's types");
    assert!(tag.refused.is_none() && tag.findings.is_empty(), "ORACLE: `uses_tag` is clean (only `&self`'s region is live): {tag:?}");
    assert!(twin.refused.is_none(), "TWIN: `uses_tag_self`'s borrows are checked: {twin:?}");
    assert!(twin.findings.iter().any(|d| d.code == "E0502"), "TWIN: `uses_tag_self` must be E0502: {twin:?}");
}
