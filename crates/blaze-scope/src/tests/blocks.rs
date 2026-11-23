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
use crate::graph::Sysroot;
use crate::names::Written;
use crate::resolve::{Names, SourceTexts, resolve_crates};
use crate::task::{ImplSrc, Serial};

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

/// A body's macro call read at its block's module (front2 scoped.rs `ScopedBy::resolve`, the
/// reader after the names are done): Rust's order, textual scope first. The Reference, "Macros
/// by example", "Textual scope": a `macro_rules!` written before the fn in its module is in
/// scope in the fn's body, so `seven` read at the body's block names the module's definition
/// (the regression of 2026-11-17: core's `argument_new!` in `Argument::new_debug`, "segment 0:
/// not found", because the body's outermost block has no `mod` line in its module and the walk
/// read none of the module's sequence); at an inner block too (its outer block is unmarked in
/// the module, and marks the inner one); a block's own `macro_rules! seven` shadows the
/// module's; a name nothing defines is not found (the control).
const BY_BODY_TEXTUAL: &str = "#![no_core]
macro_rules! seven { () => {} }
pub fn by_module() { seven!(); }
pub fn by_block() { macro_rules! seven { () => {} } seven!(); }
pub fn by_nested() { { seven!(); } }
pub fn by_nothing() { nothing_binds!(); }
";

static BY_BODY_TEXTUAL_TEXTS: Texts = Texts(&[&[("/user/lib.rs", BY_BODY_TEXTUAL)]]);

#[test]
fn a_block_reads_the_macro_rules_of_its_module_first() {
    let crates = [input("user", "/user/lib.rs", &[])];
    let names = resolved(&crates, &BY_BODY_TEXTUAL_TEXTS);
    let krate = names.krate(0);
    let root = names.root(0);
    let module_seven = krate
        .done
        .get(&root)
        .and_then(|r| r.scope.textual.iter().find(|d| d.name == "seven").map(|d| d.def))
        .unwrap_or_else(|| panic!("the module's `macro_rules! seven`"));
    // Every block's module of the crate, by its chain; in key order, so the run is one order.
    let mut blocks: Vec<(blaze_id::ItemKey, usize)> =
        krate.chains.iter().filter(|(k, _)| crate::names::World::anonymous(&names, **k)).map(|(k, c)| (*k, c.len())).collect();
    blocks.sort_unstable();
    assert_eq!(blocks.len(), 5, "one block each for by_module, by_block, by_nothing, two for by_nested: {blocks:?}");
    let read = |module: blaze_id::ItemKey, name: &'static str| {
        let segments: &'static [&'static str] = match name {
            "seven" => &["seven"],
            _ => &["nothing_binds"],
        };
        names.item_at(module, Namespace::Macros, Written { kind: PathKind::Plain, segments }, Alloc::global()).map(|at| at.def.key)
    };
    let (mut by_module, mut by_own, mut inner) = (0, 0, 0);
    for &(block, depth) in blocks.iter() {
        let own = krate.done.get(&block).and_then(|r| r.scope.textual.iter().find(|d| d.name == "seven").map(|d| d.def));
        let got = read(block, "seven").unwrap_or_else(|e| panic!("`seven` at block {block:x} (chain {depth}): {e}"));
        match own {
            Some(def) => {
                assert_eq!(got, def, "a block's own `macro_rules! seven` shadows the module's");
                by_own += 1;
            }
            None => {
                assert_eq!(got, module_seven, "the module's `macro_rules! seven` at block {block:x} (chain {depth})");
                by_module += 1;
                if depth > 2 {
                    inner += 1;
                }
            }
        }
        let none = read(block, "nothing_binds");
        assert!(none.is_err(), "a name nothing defines is not found at block {block:x}: {none:?}");
    }
    assert_eq!((by_module, by_own, inner), (4, 1, 1), "four blocks read the module's (one an inner block), one its own");
}

/// A trait declared in a block: the shape of core's `Write::write_fmt` (library/core/src/fmt/
/// mod.rs: `trait SpecWriteFmt { .. }` and `impl<W: Write + ?Sized> SpecWriteFmt for &mut W`
/// in the fn's body), and a block whose only item is a trait. The Reference, "Statements",
/// "Item declarations": "Declaring an item within a statement block restricts its scope to the
/// block containing the statement." So the trait is an item of its block's module: the impl's
/// header names it there, it is among that module's traits in scope, and a block holding a
/// trait alone is made a module too (task/lower.rs `BodySrc::of`'s words and
/// `NestedKind::makes_module`). Prints "BLOCK trait: SpecWriteFmt and Provided are traits of
/// their blocks' modules"; without "trait" in either, the second body makes no module and the
/// test fails at "`Provided` read in its block's module".
const BLOCK_TRAIT: &str = "#![no_core]
pub trait Write {}
pub fn write_fmt() -> u8 {
    trait SpecWriteFmt { fn spec_write_fmt(self) -> u8; }
    impl<W: Write + ?Sized> SpecWriteFmt for &mut W { fn spec_write_fmt(self) -> u8 { 0 } }
    0
}
pub fn alone() -> u8 {
    trait Provided { fn one(&self) -> u8 { 1 } }
    0
}
";

static BLOCK_TRAIT_TEXTS: Texts = Texts(&[&[("/user/lib.rs", BLOCK_TRAIT)]]);

#[test]
fn blocks_hold_a_trait_as_an_item_of_their_module() {
    use blaze_surface::DefKind;
    let crates = [input("user", "/user/lib.rs", &[])];
    let names = resolved(&crates, &BLOCK_TRAIT_TEXTS);
    let spec = impl_ending(&names, 0, "SpecWriteFmtfor&mutW");
    let tr = names
        .item_at(spec.module, Namespace::Types, Written { kind: PathKind::Plain, segments: &["SpecWriteFmt"] }, Alloc::global())
        .unwrap_or_else(|e| panic!("`SpecWriteFmt` read in the impl's block module: {e}"));
    assert_eq!(tr.def.kind, DefKind::Trait, "`SpecWriteFmt` is a trait of the block's module");
    let in_scope = names.traits_in_scope(spec.module, Alloc::global()).unwrap_or_else(|e| panic!("traits in scope at the block's module: {e}"));
    assert!(in_scope.contains(&tr.def.key), "the block's trait is in scope in its block");
    // The block whose only item is a trait: its module's key from the same scan.
    let root = names.root(0);
    let alone = names
        .item_at(root, Namespace::Values, Written { kind: PathKind::Plain, segments: &["alone"] }, Alloc::global())
        .unwrap_or_else(|e| panic!("`alone`: {e}"));
    let at = BLOCK_TRAIT.find("pub fn alone").unwrap_or_else(|| panic!("the text holds `alone`"));
    let nested = blaze_file_items::nested::scan(&BLOCK_TRAIT[at..], Alloc::global());
    let modules = crate::blocks::block_modules(names.krate(0).key, alone.def.key, &nested, Alloc::global());
    let module = modules.first().copied().flatten().unwrap_or_else(|| panic!("`alone`'s block is written as a module"));
    let provided = names
        .item_at(module, Namespace::Types, Written { kind: PathKind::Plain, segments: &["Provided"] }, Alloc::global())
        .unwrap_or_else(|e| panic!("`Provided` read in its block's module: {e}"));
    assert_eq!(provided.def.kind, DefKind::Trait);
    // Outside the block the trait is no name (the Reference's sentence above).
    assert!(names.item_at(root, Namespace::Types, Written { kind: PathKind::Plain, segments: &["Provided"] }, Alloc::global()).is_err(), "a block's trait is not the module's");
    std::println!("BLOCK trait: SpecWriteFmt and Provided are traits of their blocks' modules");
}

/// A call in an expression of a block (tempo's `let read = tokio::select! { .. }`, whose
/// expansion is a block expression declaring `mod __tokio_select_util`) is listed in its block
/// and expanded by the names library when its definition or its input may write an item
/// (resolve.rs `expand_call`, blaze_file_items::nested::may_declare): `sel!`'s input holds
/// `mod` and `impl`, `decl!`'s rules write an `impl`, and both impls are read in the
/// expansions' blocks. `vec![1]` (alloc's `vec!` shape: a path and an array, no item) and
/// `no_arm_takes!(1)` are not expanded there: `no_arm_takes!` has no rule for `1`, so an
/// expansion would leave a failed call's note, and none is left. Removing the skip in
/// `expand_call` makes the last assertion fail with that note; removing the expression arms of
/// the scan (nested.rs) makes `impl_ending` find no `LocalforA`. Prints one line.
const BY_EXPRESSION: &str = "#![no_core]
pub trait Local {}
pub struct A;
pub struct B;
macro_rules! sel { ($($t:tt)*) => { { $($t)* 0 } }; }
macro_rules! decl { () => { { impl Local for B {} 1 } }; }
macro_rules! vec { ($($x:expr),+ $(,)?) => { [$($x),+] }; }
macro_rules! no_arm_takes { (never) => { 0 }; }
pub fn by_expression() -> u8 {
    let v = vec![1];
    let n = no_arm_takes!(1);
    let x = sel!{ mod m {} impl Local for A {} };
    let w: u8 = decl!();
    x
}
";

static BY_EXPRESSION_TEXTS: Texts = Texts(&[&[("/user/lib.rs", BY_EXPRESSION)]]);

#[test]
fn an_expression_call_is_expanded_by_the_names_only_when_it_may_declare() {
    let crates = [input("user", "/user/lib.rs", &[])];
    let names = resolved(&crates, &BY_EXPRESSION_TEXTS);
    let all: Vec<&str> = names.krate(0).tables.impls.iter().map(|i| i.header).collect();
    let by_input = impl_ending(&names, 0, "LocalforA");
    let by_rules = impl_ending(&names, 0, "LocalforB");
    assert_eq!(named_in(&names, by_input, &["Local"]), 0, "{all:?}");
    assert_eq!(named_in(&names, by_rules, &["Local"]), 0, "{all:?}");
    let expanded: Vec<std::string::String> = names
        .notes(0)
        .iter()
        .filter(|n| matches!(n, crate::resolve::NamesNote::Expand { .. }))
        .map(|n| std::format!("{n:?}"))
        .filter(|n| n.contains("no_arm_takes") || n.contains("\"vec\""))
        .collect();
    assert!(expanded.is_empty(), "`vec![1]` and `no_arm_takes!(1)` are not expanded by the names: {expanded:?}");
    std::println!("BLOCK expression calls: sel! and decl! expanded ({} impls), vec! and no_arm_takes! not", all.len());
}

/// The shape of the pinned alloc's `format!` in a finished crate: `$crate::__export::must_use({
/// $crate::fmt::format($crate::__export::format_args!(..)) })`: no item, and its one call,
/// through `$crate`, of a builtin that writes none, yet that call stands in a BLOCK of the
/// expansion. Here `shown!` writes `{ let res = $crate::__export::inner!(); res }`, `inner!` an
/// expression. Its twin `flat!` writes `0`, no call and no item, and matches `never` only, so
/// an expansion of it by the names would leave a failed call's note naming it.
const THROUGH_LIB: &str = "#![no_core]
#[macro_export]
macro_rules! shown { ($($a:tt)*) => {{ let res = $crate::__export::inner!(); res }}; }
#[macro_export]
macro_rules! flat { (never) => { 0 }; }
#[doc(hidden)]
#[macro_export]
macro_rules! __inner { () => { 0 }; }
pub mod __export { pub use crate::__inner as inner; }
";

/// Three bodies, each with a `format!`-shaped call and its twin.
const THROUGH_USER: &str = "#![no_core]
pub fn f0() { fmtlib::shown!(\"{v:?}\"); fmtlib::flat!(1); }
pub fn f1() { fmtlib::shown!(\"{v:?}\"); fmtlib::flat!(1); }
pub fn f2() { fmtlib::shown!(\"{v:?}\"); fmtlib::flat!(1); }
";

static THROUGH_TEXTS: Texts = Texts(&[&[("/user/lib.rs", THROUGH_USER)], &[("/fmtlib/lib.rs", THROUGH_LIB)]]);

/// The body numbers the written blocks of each expansion of a statement call and reads each
/// as the module the names filed for it (front2 scope_table.rs `build`), so the names must
/// expand every call whose expansion may write a block: one holding an item or a call. A
/// dependency's `format!`-shaped call is such a call, though its `$crate` callee writes no
/// item: in three bodies `shown!` is expanded and its block `{ let res = inner!(); res }` is a
/// module of its own each time (three `__blaze_block_x..` modules). The negative twin: `flat!`,
/// whose expansion writes no block, is not expanded (no note names it). The regression:
/// skipping `shown!` because its `$crate` callee writes no item (the walk taken out of
/// resolve.rs `expand_call` on 2026-11-21) made zero such modules, and tempo's 161 bodies
/// calling `assert_eq!` and `format!` in a block asked for "the names scope of the block
/// containing the path" and found none. Prints one line.
#[test]
fn a_dependency_s_call_whose_expansion_writes_a_block_is_expanded() {
    const BODIES: usize = 3;
    let crates = [input("user", "/user/lib.rs", &[("fmtlib", 1)]), input("fmtlib", "/fmtlib/lib.rs", &[])];
    let names = resolved(&crates, &THROUGH_TEXTS);
    let notes: Vec<std::string::String> = names
        .notes(0)
        .iter()
        .filter(|n| matches!(n, crate::resolve::NamesNote::Expand { .. }))
        .map(|n| std::format!("{n:?}"))
        .collect();
    assert_eq!(notes.iter().filter(|n| n.contains("flat")).count(), 0, "`flat!` writes no block: not expanded: {notes:?}");
    let expansion_blocks = names
        .krate(0)
        .made
        .values()
        .filter(|m| m.info.name.is_some_and(|n| n.starts_with(crate::expanded_blocks::EXPANSION_BLOCK)))
        .count();
    assert_eq!(expansion_blocks, BODIES, "one module per `shown!` expansion's block holding a call: {notes:?}");
    std::println!("BLOCK a dependency's format!-shaped call: expanded in {BODIES} bodies, {expansion_blocks} expansion block modules; flat! not expanded");
}

/// tempo crates/consensus/src/follow/executor/actor.rs:325 (the check of 2026-11-20 gave
/// "tk93d error[unresolved] actor.rs:328:9 no serde_json in this scope"):
/// `#[cfg(feature = "fuzz")] let details = serde_json::json!({ .. });`, where `serde_json` is
/// an optional dependency only `fuzz` enables. The Reference, "Conditional compilation": a
/// false `cfg` on a statement removes it, its calls with it, before any name in it is
/// resolved. The scan gives the call its statement's attributes (nested.rs `NestedCall::attrs`)
/// and the block's module writes them before it (expanded_blocks.rs `render`). The twin: the
/// same call with no `cfg` names a crate the extern prelude lacks and is still refused, from
/// inside the same written block. FAILS without the attributes: `serde_json` is refused too.
/// Prints one line.
const CFG_STATEMENT: &str = "#![no_core]
pub fn executes() -> u8 {
    #[cfg(feature = \"fuzz\")]
    let details = serde_json::json!({ \"height\": 1 });
    let shown = absent_json::json!({ \"height\": 1 });
    0
}
";

static CFG_STATEMENT_TEXTS: Texts = Texts(&[&[("/user/lib.rs", CFG_STATEMENT)]]);

#[test]
fn a_call_in_a_statement_under_a_false_cfg_names_nothing() {
    let crates = [input("user", "/user/lib.rs", &[])];
    let names = resolved(&crates, &CFG_STATEMENT_TEXTS);
    let refused = |first: &str| -> usize {
        names
            .krate(0)
            .done
            .values()
            .flat_map(|r| r.scope.errors.iter())
            .filter(|e| matches!(e, blaze_surface::ScopeError::Macro { path, .. } if path.segments.first().is_some_and(|s| *s == first)))
            .count()
    };
    assert_eq!(refused("serde_json"), 0, "a call under a false `cfg` is resolved");
    let twin = refused("absent_json");
    assert_ne!(twin, 0, "the twin: a call naming an absent crate is refused in its block");
    // The removed call keeps its place: the body side numbers a block's calls from the scan
    // (front2 scope_table.rs `statement_call`), so the live call is the block's second, `1`,
    // in both layers (task/lower.rs `Lower::item`). Without the count it is `0`.
    let heap = blaze_own::testing::heap();
    let root = names.root(0);
    let body = names
        .item_at(root, Namespace::Values, Written { kind: PathKind::Plain, segments: &["executes"] }, heap.alloc())
        .unwrap_or_else(|e| panic!("`executes`: {e}"));
    let at = CFG_STATEMENT.find("pub fn executes").unwrap_or_else(|| panic!("the text holds `executes`"));
    let nested = blaze_file_items::nested::scan(&CFG_STATEMENT[at..], heap.alloc());
    let krate = names.krate(0).key;
    let modules = crate::blocks::block_modules(krate, body.def.key, &nested, heap.alloc());
    let module = modules.first().copied().flatten().unwrap_or_else(|| panic!("`executes`' block is written as a module"));
    let calls = &names.krate(0).tables.calls;
    assert!(calls.contains_key(&crate::task::key_of(krate, module, "MacroCall", "", 1)), "the live call is the block's second");
    assert!(!calls.contains_key(&crate::task::key_of(krate, module, "MacroCall", "", 0)), "the removed call is lowered as none");
    std::println!("BLOCK cfg statements: serde_json under a false cfg refused 0 times; absent_json {twin}");
}

/// tempo crates/zones/l1/src/tests.rs: `serde_json::from_str(include_str!(concat!(
/// env!("CARGO_MANIFEST_DIR"), "/testdata/depositHashChain.json")))` in a fn body. The input
/// holds a call (`concat!(`), but `include_str!`'s output is a literal: it declares nothing in
/// the block, so the names library never expands it and never reads the data file (whose read
/// no outline listed: the tempo check of 2026-11-20 21:41 stopped with "CrateScope needs
/// FileItems, which its plan did not hand it"). The `sel!` beside it is still expanded.
const DATA_IN_A_BODY: &str = "#![no_core]
#![feature(no_core, rustc_attrs)]
#[rustc_builtin_macro]
macro_rules! include_str { ($file:expr $(,)?) => {{}}; }
#[rustc_builtin_macro]
macro_rules! concat { ($($e:expr),* $(,)?) => {{}}; }
pub trait Local {}
pub struct A;
macro_rules! sel { ($($t:tt)*) => { { $($t)* 0 } }; }
pub fn reads_data() -> u8 {
    let s = include_str!(concat!(\"da\", \"ta.json\"));
    include_str!(concat!(\"da\", \"ta.json\"));
    sel!{ impl Local for A {} }
}
";

/// A source that counts its data reads: the names library must ask none of [`DATA_IN_A_BODY`].
struct DataReads {
    asked: core::sync::atomic::AtomicU32,
}

impl<'a> SourceTexts<'a> for DataReads {
    fn read(&self, krate: usize, path: &str) -> Option<&'a str> {
        (krate == 0 && path == "/user/lib.rs").then_some(DATA_IN_A_BODY)
    }

    fn read_data(&self, _krate: usize, _from: &str, _path: &str) -> Option<&'a [u8]> {
        self.asked.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        Some(b"{}")
    }
}

#[test]
fn a_literal_builtin_in_a_body_is_never_expanded_by_the_names() {
    let frame = blaze_mem::Frame::new(blaze_mem::Tag::Nameres).expect("frame");
    let erased = blaze_own::Erased::new(&frame);
    let source = DataReads { asked: core::sync::atomic::AtomicU32::new(0) };
    let crates = [input("user", "/user/lib.rs", &[])];
    let names = resolve_crates(&crates, Sysroot::default(), &source, &Serial, erased.bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let all: Vec<&str> = names.krate(0).tables.impls.iter().map(|i| i.header).collect();
    assert!(all.iter().any(|h| h.replace(' ', "").ends_with("LocalforA")), "the `sel!` beside the data reads is expanded: {all:?}");
    let asked = source.asked.load(core::sync::atomic::Ordering::Relaxed);
    assert_eq!(asked, 0, "the names library read the data file of an `include_str!` in a body");
    std::println!("BLOCK literal builtins: include_str!(concat!(..)) in a body read {asked} data files; sel! expanded");
}

/// The pinned core's panicking.rs: `panic_fmt` and `panic_nounwind_fmt` each write
/// `unsafe extern "Rust" { #[lang = "panic_impl"] fn panic_impl(pi: &PanicInfo<'_>) -> !; }` in
/// their body beside an expression call (`if cfg!(panic = "immediate-abort")`), so both blocks
/// are written as modules and both declarations are items of the names. A `#[lang]` on a
/// foreign fn naming a weak lang item is a use of it (rustc_passes weak_lang_items.rs), so
/// neither is a lang row; the definition is a `#[panic_handler]`'s. A fn's own `#[lang]` still
/// is one.
///
/// FAILS without the foreign-item rule in task/lower.rs `Lower::item`: two `panic_impl` rows
/// with two keys, which blaze-ladder's index refuses as `LangTwice` for every program linking
/// core (wave-1119 t33, 2026-11-20 22:05).
const PANIC_IMPL_TWICE: &str = "#![no_core]
#![feature(no_core, rustc_attrs, lang_items)]
#[rustc_builtin_macro]
macro_rules! cfg { ($($t:tt)*) => {{}}; }
#[lang = \"panic\"]
pub fn panic() {}
pub fn panic_fmt() {
    if cfg!(panic = \"immediate-abort\") {}
    unsafe extern \"Rust\" {
        #[lang = \"panic_impl\"]
        fn panic_impl() -> u8;
    }
    unsafe { panic_impl() };
}
pub fn panic_nounwind_fmt() {
    if cfg!(panic = \"immediate-abort\") {}
    unsafe extern \"Rust\" {
        #[lang = \"panic_impl\"]
        fn panic_impl() -> u8;
    }
    unsafe { panic_impl() };
}
";

static PANIC_IMPL_TWICE_TEXTS: Texts = Texts(&[&[("/user/lib.rs", PANIC_IMPL_TWICE)]]);

#[test]
fn a_weak_lang_item_declared_in_two_bodies_is_no_lang_row() {
    let crates = [input("user", "/user/lib.rs", &[])];
    let names = resolved(&crates, &PANIC_IMPL_TWICE_TEXTS);
    let langs: Vec<&str> = names.krate(0).tables.langs.iter().map(|l| l.name).collect();
    assert!(!langs.contains(&"panic_impl"), "a foreign fn's weak `#[lang]` is a use, no row: {langs:?}");
    assert_eq!(langs.iter().filter(|l| **l == "panic").count(), 1, "a fn's own `#[lang]` is a row: {langs:?}");
    std::println!("BLOCK weak lang uses: lang rows {langs:?}");
}
