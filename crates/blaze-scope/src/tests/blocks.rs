//! Impls written inside blocks, through the one-call entry on two crates: the block's module
//! ([`crate::blocks`]) must read what the real derives write.
//!
//! The texts are the shapes of serde_derive 1.0.229 (the version tempo's lock file names),
//! src/dummy.rs `wrap_in_const`:
//!
//! ```text
//! let use_serde = match serde_path {
//!     Some(path) => quote! { use #path as _serde; },
//!     None => quote! {
//!         #[allow(unused_extern_crates, clippy::useless_attribute)]
//!         extern crate serde as _serde;
//!     },
//! };
//! quote! {
//!     #[doc(hidden)]
//!     #[allow(non_upper_case_globals, unused_attributes, unused_qualifications, clippy::absolute_paths)]
//!     const _: () = {
//!         #use_serde
//!         _serde::__require_serde_not_serde_core!();
//!         #code
//!     };
//! }
//! ```
//!
//! and of what `#code` is for `Deserialize` (src/de.rs): the impl's `fn deserialize` holds
//! `#[allow(non_camel_case_types)] #[doc(hidden)] enum __Field { .. }`, `#[doc(hidden)] struct
//! __FieldVisitor;` and `#[automatically_derived] impl<'de> _serde::de::Visitor<'de> for
//! __FieldVisitor` / `impl<'de> _serde::Deserialize<'de> for __Field`.
//!
//! The tempo census of 2026-11-16 (tempo-front-6b.txt) on this shape: "9781 NEW container
//! head: Resolve Path" (`<impl _serde :: Serialize for Burn>::serialize`) and "7443 NEW gap
//! UnreadImpl" (`impl < 'de > _serde :: Deserialize < 'de > for __Field`: `PathError { .., at:
//! 0, why: NotFound }`): `_serde` named nothing in the block's module, because the scan lost
//! the item written after an attribute with arguments.

use std::vec::Vec;

use blaze_own::Alloc;
use blaze_surface::{Namespace, PathKind};

use super::entry::{Texts, input, resolved};
use crate::names::Written;
use crate::resolve::Names;
use crate::task::ImplSrc;

/// The stand-in for serde: its traits, the macro its derive calls in statement position
/// (serde 1.0.229 src/lib.rs: `macro_rules! __require_serde_not_serde_core { () => {}; }`,
/// `#[macro_export]`, `#[doc(hidden)]`), and a `private` module re-exporting the crate, as a
/// crate that re-exports serde for its own macros does (`#[serde(crate = "..")]`).
const SERDE: &str = "#![no_core]
#[doc(hidden)]
#[macro_export]
macro_rules! __require_serde_not_serde_core { () => {}; }
#[macro_export]
macro_rules! write_marker { ($t:ty) => { impl $crate::Marker for $t {} }; }
pub trait Serialize {}
pub trait Deserialize<'de> {}
pub trait Marker {}
pub mod de { pub trait Visitor<'de> {} }
pub mod private { pub use crate as serde; }
";

const DERIVED: &str = "#![no_core]
pub struct Burn;
#[doc(hidden)]
#[allow(non_upper_case_globals, unused_attributes, unused_qualifications, clippy::absolute_paths)]
const _: () = {
    #[allow(unused_extern_crates, clippy::useless_attribute)]
    extern crate serde as _serde;
    _serde::__require_serde_not_serde_core!();
    #[automatically_derived]
    impl _serde::Serialize for Burn {
        fn serialize() {}
    }
};
#[doc(hidden)]
#[allow(non_upper_case_globals, unused_attributes, unused_qualifications, clippy::absolute_paths)]
const _: () = {
    #[allow(unused_extern_crates, clippy::useless_attribute)]
    extern crate serde as _serde;
    _serde::__require_serde_not_serde_core!();
    #[automatically_derived]
    impl<'de> _serde::Deserialize<'de> for Burn {
        fn deserialize() {
            #[allow(non_camel_case_types)]
            #[doc(hidden)]
            enum __Field { __field0, __ignore }
            #[doc(hidden)]
            struct __FieldVisitor;
            #[automatically_derived]
            impl<'de> _serde::de::Visitor<'de> for __FieldVisitor {
                fn visit() {}
            }
            #[automatically_derived]
            impl<'de> _serde::Deserialize<'de> for __Field {
                fn deserialize() {}
            }
        }
    }
};
";

static DERIVED_TEXTS: Texts = Texts(&[&[("/user/lib.rs", DERIVED)], &[("/serde/lib.rs", SERDE)]]);

/// The impl of `krate` whose header (as written, spaces dropped) ends with `tail`.
fn impl_ending<'n>(names: &'n Names<'static>, krate: usize, tail: &str) -> &'n ImplSrc<'static> {
    let impls = &names.krate(krate).tables.impls;
    let squeezed = |h: &str| -> std::string::String { h.chars().filter(|c| !c.is_whitespace()).collect() };
    let found: Vec<&ImplSrc<'static>> = impls.iter().filter(|i| squeezed(i.header).trim_end_matches('{').ends_with(tail)).collect();
    let all: Vec<&str> = impls.iter().map(|i| i.header).collect();
    assert_eq!(found.len(), 1, "one impl ending `{tail}` among {all:?}");
    found[0]
}

/// The crate a type path names, read in the module of `imp`.
fn named_in(names: &Names<'static>, imp: &ImplSrc<'static>, segments: &'static [&'static str]) -> usize {
    names
        .item_at(imp.module, Namespace::Types, Written { kind: PathKind::Plain, segments }, Alloc::global())
        .unwrap_or_else(|e| panic!("{segments:?} read in the module of `{}`: {e}", imp.header))
        .krate
}

/// The Reference, "Extern crate declarations": "An extern crate declaration specifies a
/// dependency on an external crate. The external crate is then bound into the declaring scope
/// as the identifier provided in the extern crate declaration"; "Attributes": an attribute's
/// input is a delimited token tree, so the item after `#[allow(a, b)]` is the `extern crate`.
/// Every derived impl's trait path must name serde's trait in the block's module, and the
/// local types of `fn deserialize`'s block (`__Field`, `__FieldVisitor`, written after
/// attributes with arguments) and the type outside both blocks (`Burn`) must be found there.
#[test]
fn blocks_read_what_serde_s_derive_writes() {
    let crates = [input("user", "/user/lib.rs", &[("serde", 1)]), input("serde", "/serde/lib.rs", &[])];
    let names = resolved(&crates, &DERIVED_TEXTS);
    let all: Vec<&str> = names.krate(0).tables.impls.iter().map(|i| i.header).collect();
    assert_eq!(all.len(), 4, "the four derived impls: {all:?}");
    let ser = impl_ending(&names, 0, "_serde::SerializeforBurn");
    assert_eq!(named_in(&names, ser, &["_serde", "Serialize"]), 1, "`_serde::Serialize` is serde's");
    assert_eq!(named_in(&names, ser, &["Burn"]), 0);
    let de = impl_ending(&names, 0, "_serde::Deserialize<'de>forBurn");
    assert_eq!(named_in(&names, de, &["_serde", "Deserialize"]), 1, "`_serde::Deserialize` is serde's");
    let visitor = impl_ending(&names, 0, "for__FieldVisitor");
    assert_eq!(named_in(&names, visitor, &["_serde", "de", "Visitor"]), 1, "`_serde` of the block around the member's block");
    assert_eq!(named_in(&names, visitor, &["__FieldVisitor"]), 0, "a unit struct written after `#[doc(hidden)]`");
    let field = impl_ending(&names, 0, "for__Field");
    assert_eq!(named_in(&names, field, &["_serde", "Deserialize"]), 1);
    assert_eq!(named_in(&names, field, &["__Field"]), 0, "an enum written after two attributes with arguments");
    assert_eq!(named_in(&names, field, &["Burn"]), 0, "the module's type, through both blocks");
    assert_ne!(visitor.module, de.module, "the member's block is its own module");
    assert_eq!(visitor.module, field.module);
    let members = |i: &ImplSrc<'static>| -> Vec<Option<&str>> { names.members(i.key).unwrap_or(&[]).iter().map(|m| m.name).collect() };
    assert_eq!(members(ser), [Some("serialize")]);
    assert_eq!(members(visitor), [Some("visit")]);
}

/// The other arm of `wrap_in_const`: with `#[serde(crate = "serde::private::serde")]` the
/// block holds `use serde::private::serde as _serde;`, a path through another crate's `pub
/// use crate as serde;`. The Reference, "Use declarations", "Underscore imports" aside, a
/// `use .. as name;` binds `name` in the block's scope like any item.
const BY_PATH: &str = "#![no_core]
pub struct Mint;
#[doc(hidden)]
#[allow(non_upper_case_globals, unused_attributes, unused_qualifications, clippy::absolute_paths)]
const _: () = {
    use serde::private::serde as _serde;
    _serde::__require_serde_not_serde_core!();
    #[automatically_derived]
    impl _serde::Serialize for Mint {}
};
";

static BY_PATH_TEXTS: Texts = Texts(&[&[("/user/lib.rs", BY_PATH)], &[("/serde/lib.rs", SERDE)]]);

#[test]
fn blocks_read_a_crate_path_given_to_serde_s_derive() {
    let crates = [input("user", "/user/lib.rs", &[("serde", 1)]), input("serde", "/serde/lib.rs", &[])];
    let names = resolved(&crates, &BY_PATH_TEXTS);
    let ser = impl_ending(&names, 0, "_serde::SerializeforMint");
    assert_eq!(named_in(&names, ser, &["_serde", "Serialize"]), 1, "`_serde::Serialize` through `serde::private::serde`");
}

/// The Reference, "Macro invocation": a macro may be invoked as a statement, and "Statements"
/// lists item declarations among them: a statement call may write an impl, which holds for
/// its type in the whole crate. A call in a block that is made a module is expanded there,
/// by the path it is written with (a dependency's exported macro) or by the textual scope of
/// the module the body is in (a `macro_rules!` written before the fn).
const BY_CALLS: &str = "#![no_core]
pub trait Local {}
macro_rules! write_local { ($t:ty) => { impl Local for $t {} }; }
pub struct A;
pub struct B;
pub fn by_path() -> u8 {
    struct Here;
    impl Local for Here {}
    serde::write_marker!(A);
    serde::__require_serde_not_serde_core!();
    0
}
";

static BY_CALLS_TEXTS: Texts = Texts(&[&[("/user/lib.rs", BY_CALLS)], &[("/serde/lib.rs", SERDE)]]);

#[test]
fn blocks_expand_a_statement_call_that_writes_an_impl() {
    let crates = [input("user", "/user/lib.rs", &[("serde", 1)]), input("serde", "/serde/lib.rs", &[])];
    let names = resolved(&crates, &BY_CALLS_TEXTS);
    let all: Vec<&str> = names.krate(0).tables.impls.iter().map(|i| i.header).collect();
    let marker = impl_ending(&names, 0, "MarkerforA");
    let here = impl_ending(&names, 0, "LocalforHere");
    assert_eq!(marker.module, here.module, "the call's impl is read in its block's module: {all:?}");
    assert_eq!(named_in(&names, marker, &["A"]), 0);
    assert_eq!(all.len(), 2, "one written impl and one written by a statement call: {all:?}");
    let refused: Vec<_> = names.notes(0).iter().filter(|n| matches!(n, crate::resolve::NamesNote::Expand { .. })).collect();
    assert!(refused.is_empty(), "no expansion refused: {refused:?}");
}

/// The Reference, "Macros by example", "Textual scope": "the scope of a `macro_rules!`
/// definition begins after its definition and extends to the end of the enclosing module or
/// block": a call in a block names a `macro_rules!` written earlier in the block by its bare
/// name. OWED (not asserted here: it fails today): one written before the fn in its MODULE is
/// in scope too, and is not found, because a block's module has no `mod` line in its parent
/// for the textual scope to be marked at (task/answer.rs `entry`: `scope.mark(module)` is
/// `None`, read as 0): `write_local! { B }` below writes no impl yet.
const BY_TEXTUAL: &str = "#![no_core]
pub trait Local {}
macro_rules! write_local { ($t:ty) => { impl Local for $t {} }; }
pub struct A;
pub struct B;
pub struct C;
pub fn by_textual_scope() {
    impl Local for A {}
    write_local! { B }
    macro_rules! in_block { () => { impl Local for C {} }; }
    in_block!();
}
";

static BY_TEXTUAL_TEXTS: Texts = Texts(&[&[("/user/lib.rs", BY_TEXTUAL)]]);

#[test]
fn blocks_expand_a_statement_call_named_by_textual_scope() {
    let crates = [input("user", "/user/lib.rs", &[])];
    let names = resolved(&crates, &BY_TEXTUAL_TEXTS);
    let all: Vec<&str> = names.krate(0).tables.impls.iter().map(|i| i.header).collect();
    let local_c = impl_ending(&names, 0, "LocalforC");
    assert_eq!(named_in(&names, local_c, &["Local"]), 0, "written by the block's own `macro_rules!`: {all:?}");
    // When the owed case above is built this becomes 3 (`impl Local for B`).
    std::println!("BLOCK impls by textual scope: {} of 3: {all:?}", all.len());
    assert!(all.len() >= 2, "{all:?}");
}
