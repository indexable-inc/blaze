//! Built-in attributes named by a path, as macro expansions write them.
//!
//! The Reference, "Attributes", "Built-in attributes index", and "Preludes": the built-in
//! attributes are items of `core::prelude::v1` (and each edition's prelude module beside it), so
//! `#[::core::prelude::v1::derive(Clone)]` is the derive attribute and its item is declared as
//! with `#[derive(Clone)]`. libc's `s! { .. }` writes every struct that way. An expansion's text
//! keeps spaces between tokens (`:: core :: prelude`), and the path reads the same.

use blaze_surface::{Namespace, PathKind};

use super::entry::{Texts, input, resolved};
use crate::names::Written;
use crate::task::head;

const DEP: &str = "#![no_core]
macro_rules! s { ($($i:item)*) => { $( #[::core::prelude::v1::derive(::core::clone::Clone)] $i )* }; }
s! { pub struct Made; }
#[::core::prelude::v1::derive(Clone)]
pub struct Written;
#[std::prelude::rust_2021::derive(Clone)]
pub struct Edition;
";

const USER: &str = "#![no_core]
use dep::{Edition, Made, Written};
";

static TEXTS: Texts = Texts(&[&[("/user/lib.rs", USER)], &[("/dep/lib.rs", DEP)]]);

#[test]
fn a_struct_under_a_prelude_path_derive_is_declared() {
    let crates = [input("user", "/user/lib.rs", &[("dep", 1)]), input("dep", "/dep/lib.rs", &[])];
    let names = resolved(&crates, &TEXTS);
    let g = blaze_own::testing::heap().alloc();
    for name in ["Made", "Written", "Edition"] {
        let segments: &'static [&'static str] = match name {
            "Made" => &["Made"],
            "Written" => &["Written"],
            _ => &["Edition"],
        };
        names
            .item_at(names.root(1), Namespace::Types, Written { kind: PathKind::Plain, segments }, g)
            .unwrap_or_else(|e| panic!("dep declares `{name}`: {e}; notes {:?}", names.notes(1)));
    }
    let errors = &names.krate(0).done.get(&names.root(0)).expect("the root is done").scope.errors;
    assert!(errors.is_empty(), "the imports resolve: {errors:?}");
}

#[test]
fn a_path_with_spaces_between_its_tokens_is_one_head() {
    assert_eq!(head(":: core :: prelude :: v1 :: derive (:: core :: clone :: Clone)"), ":: core :: prelude :: v1 :: derive");
    assert_eq!(head("cfg (unix)"), "cfg");
    assert_eq!(head("doc = \"x\""), "doc");
    assert_eq!(crate::task::prelude_attr(":: core :: prelude :: v1 :: derive"), "derive");
    assert_eq!(crate::task::prelude_attr("serde::prelude::v1::derive"), "serde::prelude::v1::derive");
}

/// Reduced from tower-http 0.6.11 (`src/lib.rs`: `#[cfg(any(feature = "compression-br", ...,
/// feature = "fs" // Used for serving precompressed static files as well\n))] mod
/// content_encoding;`): the module was off, its 5 + 4 dependents' imports unresolved in tempo.
/// The Reference, "Comments": "Non-doc comments are interpreted as a form of whitespace."
/// Block comments nest ("Nested block comments are supported").
#[test]
fn a_comment_inside_a_cfg_predicate_is_whitespace() {
    let g = blaze_own::testing::heap().alloc();
    let cfg = crate::task::Cfg::from_atoms(&[("feature", Some("fs"))], g);
    assert_eq!(cfg.eval("any(feature = \"gzip\", feature = \"fs\" // served as well\n)"), Some(true));
    assert_eq!(cfg.eval("all(/* a /* nested */ note */ feature = \"fs\")"), Some(true));
    assert_eq!(cfg.eval("not(feature = \"fs\") // off"), Some(false));
    assert_eq!(cfg.eval("all(feature = \"fs\" /* open"), None);
}

#[test]
fn cfg_version_predicate_evaluates_against_pinned_release() {
    let g = blaze_own::testing::heap().alloc();
    // Gate OFF by default: `version(..)` is unread (`None`) whether it would hold or not.
    let gated = crate::task::Cfg::from_atoms(&[("rust_nightly", None)], g);
    assert_eq!(gated.eval("version(\"1.76\")"), None);
    assert_eq!(gated.eval("version(\"99.0\")"), None);
    assert_eq!(gated.eval("not(version(\"99.0\"))"), None);
    assert_eq!(gated.eval("all(false, version(\"99.0\"))"), None);

    // Gate ON: decodes cooked escapes, line continuations, and raw strings before version comparison.
    let cfg = gated.with_cfg_version(true);
    assert_eq!(cfg.eval("version(\"1.76\")"), Some(true));
    assert_eq!(cfg.eval("version(\"1.76.0\")"), Some(true));
    assert_eq!(cfg.eval("version(\"1.76\",)"), Some(true));
    assert_eq!(cfg.eval("version(\"\\x31.76\")"), Some(true));
    assert_eq!(cfg.eval("version(\"1.\\\n  76\")"), Some(true));
    assert_eq!(cfg.eval("version(r\"1.76\")"), Some(true));
    assert_eq!(cfg.eval("version(r#\"1.76\"#)"), Some(true));
    assert_eq!(cfg.eval("version(\"99.0\")"), Some(false));
    assert_eq!(cfg.eval("not(version(\"99.0\"))"), Some(true));
    assert_eq!(cfg.eval("not(version(\"1.76\"))"), Some(false));
    assert_eq!(cfg.eval("version(\"..\")"), Some(false));
    assert_eq!(cfg.eval("all(rust_nightly, version(\"1.76\"))"), Some(true));
    // Negative controls: malformed `version(..)`, bad escapes, and unknown calls remain unread (`None`).
    assert_eq!(cfg.eval("version()"), None);
    assert_eq!(cfg.eval("version(123)"), None);
    assert_eq!(cfg.eval("version(\"\\q\")"), None);
    assert_eq!(cfg.eval("version(\"1.76\", \"1.77\")"), None);
    assert_eq!(cfg.eval("version(rust_nightly)"), None);
    assert_eq!(cfg.eval("unknown_call(\"foo\")"), None);
}

#[test]
fn cfg_version_feature_gate_enforced_in_crate_scope() {
    use crate::resolve::NamesNote;
    use crate::task::LowerNoteKind;

    const GATED_OFF: &str = "#![no_core]
#[cfg(version(\"1.76\"))]
pub struct Kept;
#[cfg(version(\"99.0\"))]
pub struct Dropped;
";
    const GATED_ON: &str = "#![no_core]
#![cfg_attr(rust_nightly, feature(cfg_version))]
#[cfg(version(\"1.76\"))]
pub struct Kept;
#[cfg(version(\"\\x31.76\"))]
pub struct Escaped;
#[cfg(version(\"99.0\"))]
pub struct Dropped;
";
    const DISABLED_OUTER: &str = "#![no_core]
#![cfg_attr(rust_nightly, feature(cfg_version))]
#[cfg(rust_nightly)]
mod nightly_only {
    #[cfg(version(\"1.76\"))]
    pub struct Inner;
}
#[cfg(rust_nightly)]
#[cfg(version(\"1.76\"))]
pub struct StackedSkipped;
";

    // 1. Without `#![feature(cfg_version)]`: both true and false `version(..)` emit UnstableCfgVersion.
    static T_OFF: Texts = Texts(&[&[("/off/lib.rs", GATED_OFF)]]);
    let crates_off = [input("off", "/off/lib.rs", &[])];
    let names_off = resolved(&crates_off, &T_OFF);
    let gated_notes = names_off
        .notes(0)
        .iter()
        .filter(|n| matches!(n, NamesNote::Lower(l) if matches!(l.kind, LowerNoteKind::UnstableCfgVersion)))
        .count();
    assert_eq!(gated_notes, 2, "expected 2 UnstableCfgVersion notes (both true and false predicates): {:?}", names_off.notes(0));

    // 2. With `#![cfg_attr(rust_nightly, feature(cfg_version))]` active: zero gate notes, Kept + Escaped declared, Dropped stripped.
    static T_ON: Texts = Texts(&[&[("/on/lib.rs", GATED_ON)]]);
    let mut c_on = input("on", "/on/lib.rs", &[]);
    c_on.cfg = &[("rust_nightly", None)];
    let names_on = resolved(&[c_on], &T_ON);
    assert!(names_on.notes(0).is_empty(), "expected no notes with feature(cfg_version): {:?}", names_on.notes(0));
    let g = blaze_own::testing::heap().alloc();
    assert!(names_on.item_at(names_on.root(0), Namespace::Types, Written { kind: PathKind::Plain, segments: &["Kept"] }, g).is_ok());
    assert!(names_on.item_at(names_on.root(0), Namespace::Types, Written { kind: PathKind::Plain, segments: &["Escaped"] }, g).is_ok());
    assert!(names_on.item_at(names_on.root(0), Namespace::Types, Written { kind: PathKind::Plain, segments: &["Dropped"] }, g).is_err());

    // 3. Inactive outer `#[cfg(rust_nightly)]` strips the inner `#[cfg(version("1.76"))]` before gate checking.
    static T_DIS: Texts = Texts(&[&[("/dis/lib.rs", DISABLED_OUTER)]]);
    let crates_dis = [input("dis", "/dis/lib.rs", &[])];
    let names_dis = resolved(&crates_dis, &T_DIS);
    assert!(names_dis.notes(0).is_empty(), "disabled outer cfg must not trigger cfg_version gate: {:?}", names_dis.notes(0));

    // 4. `#![cfg(version("1.76"))]` placed BEFORE `#![feature(cfg_version)]` (or before an active
    //    `#![cfg_attr(rust_nightly, feature(cfg_version))]`) is accepted and keeps the crate's items.
    const CFG_BEFORE_FEATURE: &str = "#![no_core]
#![cfg(version(\"1.76\"))]
#![feature(cfg_version)]
pub struct AfterOrder;
";
    static T_ORD: Texts = Texts(&[&[("/ord/lib.rs", CFG_BEFORE_FEATURE)]]);
    let crates_ord = [input("ord", "/ord/lib.rs", &[])];
    let names_ord = resolved(&crates_ord, &T_ORD);
    assert!(names_ord.notes(0).is_empty(), "cfg(version) before feature(cfg_version) must be accepted: {:?}", names_ord.notes(0));
    assert!(names_ord.krate(0).tree.notes.is_empty(), "expected no walk notes: {:?}", names_ord.krate(0).tree.notes);
    assert!(names_ord.item_at(names_ord.root(0), Namespace::Types, Written { kind: PathKind::Plain, segments: &["AfterOrder"] }, g).is_ok());

    // 5. `#![cfg(version("1.76"))]` before `#![cfg_attr(rust_nightly, feature(cfg_version))]`:
    //    accepted when `rust_nightly` is active, rejected with UnstableCfgVersion when inactive.
    const CFG_BEFORE_CFG_ATTR_FEATURE: &str = "#![no_core]
#![cfg(version(\"1.76\"))]
#![cfg_attr(rust_nightly, feature(cfg_version))]
pub struct NightlyOrder;
";
    static T_NORD: Texts = Texts(&[&[("/nord/lib.rs", CFG_BEFORE_CFG_ATTR_FEATURE)]]);
    let mut c_nord_on = input("nord_on", "/nord/lib.rs", &[]);
    c_nord_on.cfg = &[("rust_nightly", None)];
    let names_nord_on = resolved(&[c_nord_on], &T_NORD);
    assert!(names_nord_on.notes(0).is_empty(), "active cfg_attr(feature(cfg_version)) after cfg(version) must succeed: {:?}", names_nord_on.notes(0));
    assert!(names_nord_on.item_at(names_nord_on.root(0), Namespace::Types, Written { kind: PathKind::Plain, segments: &["NightlyOrder"] }, g).is_ok());

    let crates_nord_off = [input("nord_off", "/nord/lib.rs", &[])];
    let names_nord_off = resolved(&crates_nord_off, &T_NORD);
    assert!(
        names_nord_off.notes(0).iter().any(|n| matches!(n, NamesNote::Lower(l) if matches!(l.kind, LowerNoteKind::UnstableCfgVersion)))
            || names_nord_off.krate(0).tree.notes.iter().any(|n| matches!(n, crate::task::WalkNote::UnstableCfgVersion { .. })),
        "inactive cfg_attr(feature(cfg_version)) must still gate cfg(version)"
    );

    // 6. False `#![cfg(version("99.0"))]` before `#![feature(cfg_version)]`: gate enabled (no E0658),
    //    crate contents cleanly stripped.
    const FALSE_CFG_BEFORE_FEATURE: &str = "#![no_core]
#![cfg(version(\"99.0\"))]
#![feature(cfg_version)]
pub struct Stripped;
";
    static T_F_ORD: Texts = Texts(&[&[("/ford/lib.rs", FALSE_CFG_BEFORE_FEATURE)]]);
    let crates_f_ord = [input("ford", "/ford/lib.rs", &[])];
    let names_f_ord = resolved(&crates_f_ord, &T_F_ORD);
    assert!(names_f_ord.notes(0).is_empty(), "false cfg(version) with feature(cfg_version) must not emit gate error: {:?}", names_f_ord.notes(0));
    assert!(names_f_ord.krate(0).tree.notes.is_empty(), "expected no walk notes: {:?}", names_f_ord.krate(0).tree.notes);
    assert!(names_f_ord.item_at(names_f_ord.root(0), Namespace::Types, Written { kind: PathKind::Plain, segments: &["Stripped"] }, g).is_err());
}
