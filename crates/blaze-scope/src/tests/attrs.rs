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
    let g = blaze_own::Alloc::global();
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
    let g = blaze_own::Alloc::global();
    let cfg = crate::task::Cfg::from_atoms(&[("feature", Some("fs"))], g);
    assert_eq!(cfg.eval("any(feature = \"gzip\", feature = \"fs\" // served as well\n)"), Some(true));
    assert_eq!(cfg.eval("all(/* a /* nested */ note */ feature = \"fs\")"), Some(true));
    assert_eq!(cfg.eval("not(feature = \"fs\") // off"), Some(false));
    assert_eq!(cfg.eval("all(feature = \"fs\" /* open"), None);
}
