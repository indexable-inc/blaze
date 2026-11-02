//! rustc's UI tests of the feature gates of syntax (`tests/ui/feature-gates/feature-gate-*.rs`,
//! `soft-feature-gate-*.rs`, `soft-syntax-gates-*.rs`): the parser's gated spans before expansion,
//! the soft gates `feature_warn` gives, and `PostExpansionVisitor`'s gates.

use super::{ui, ui_core};
use crate::tests::{assert_reports, error, lints::bodies::types::ui_std};

#[test]
fn become_is_gated_before_expansion() {
    // tests/ui/feature-gates/feature-gate-explicit_tail_calls.rs: a `gate_all!` gate, its span recorded after the whole expression
    let source = r#"pub fn you<T>() -> T {
    become bottom(); //~ error: `become` expression is experimental
}

pub fn bottom<T>() -> T {
    become you(); //~ error: `become` expression is experimental
}

fn main() {}
"#;
    let stderr = r#"error[E0658]: `become` expression is experimental
  --> $DIR/feature-gate-explicit_tail_calls.rs:2:5
   |
LL |     become bottom();
   |     ^^^^^^^^^^^^^^^
   |
   = note: see issue #112788 <https://github.com/rust-lang/rust/issues/112788> for more information
   = help: add `#![feature(explicit_tail_calls)]` to the crate attributes to enable
   = note: this compiler was built on YYYY-MM-DD; consider upgrading it if it is out of date

error[E0658]: `become` expression is experimental
  --> $DIR/feature-gate-explicit_tail_calls.rs:6:5
   |
LL |     become you();
   |     ^^^^^^^^^^^^
   |
   = note: see issue #112788 <https://github.com/rust-lang/rust/issues/112788> for more information
   = help: add `#![feature(explicit_tail_calls)]` to the crate attributes to enable
   = note: this compiler was built on YYYY-MM-DD; consider upgrading it if it is out of date

error: aborting due to 2 previous errors

For more information about this error, try `rustc --explain E0658`.
"#;
    assert_eq!(ui("feature-gate-explicit_tail_calls.rs", "2015", source), stderr);
}

#[test]
fn builtin_hash_is_gated_at_the_builtin_name() {
    // tests/ui/feature-gates/feature-gate-builtin_syntax.rs: a `gate_all!` gate whose span is the name after `builtin #`
    let source = r#"struct Foo {
    v: u8,
    w: u8,
}
fn main() {
    builtin # offset_of(Foo, v); //~ ERROR `builtin #` syntax is unstable
}
"#;
    let stderr = r#"error[E0658]: `builtin #` syntax is unstable
  --> $DIR/feature-gate-builtin_syntax.rs:6:15
   |
LL |     builtin # offset_of(Foo, v);
   |               ^^^^^^^^^
   |
   = note: see issue #110680 <https://github.com/rust-lang/rust/issues/110680> for more information
   = help: add `#![feature(builtin_syntax)]` to the crate attributes to enable
   = note: this compiler was built on YYYY-MM-DD; consider upgrading it if it is out of date

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0658`.
"#;
    assert_eq!(ui("feature-gate-builtin_syntax.rs", "2015", source), stderr);
}

#[test]
fn super_let_is_gated_in_cfg_d_out_code() {
    // tests/ui/feature-gates/feature-gate-super-let.rs: a pre-expansion gate walks code `cfg` removes
    let source = r#"fn main() {
    super let a = 1;
    //~^ ERROR `super let` is experimental
}

// Check that it also isn't accepted in cfg'd out code.
#[cfg(false)]
fn a() {
    super let a = 1;
    //~^ ERROR `super let` is experimental
}
"#;
    let stderr = r#"error[E0658]: `super let` is experimental
  --> $DIR/feature-gate-super-let.rs:2:5
   |
LL |     super let a = 1;
   |     ^^^^^
   |
   = note: see issue #139076 <https://github.com/rust-lang/rust/issues/139076> for more information
   = help: add `#![feature(super_let)]` to the crate attributes to enable
   = note: this compiler was built on YYYY-MM-DD; consider upgrading it if it is out of date

error[E0658]: `super let` is experimental
  --> $DIR/feature-gate-super-let.rs:9:5
   |
LL |     super let a = 1;
   |     ^^^^^
   |
   = note: see issue #139076 <https://github.com/rust-lang/rust/issues/139076> for more information
   = help: add `#![feature(super_let)]` to the crate attributes to enable
   = note: this compiler was built on YYYY-MM-DD; consider upgrading it if it is out of date

error: aborting due to 2 previous errors

For more information about this error, try `rustc --explain E0658`.
"#;
    assert_eq!(ui("feature-gate-super-let.rs", "2015", source), stderr);
}

#[test]
fn a_closure_binder_is_gated_with_its_help() {
    // tests/ui/feature-gates/feature-gate-closure_lifetime_binder.rs: `gate_all!`'s help: `closure_lifetime_binder`
    let source = r#"fn main() {
    for<> || -> () {};
    //~^ ERROR `for<...>` binders for closures are experimental
    for<'a> || -> () {};
    //~^ ERROR `for<...>` binders for closures are experimental
    for<'a, 'b> |_: &'a ()| -> () {};
    //~^ ERROR `for<...>` binders for closures are experimental
}
"#;
    let stderr = r#"error[E0658]: `for<...>` binders for closures are experimental
  --> $DIR/feature-gate-closure_lifetime_binder.rs:2:5
   |
LL |     for<> || -> () {};
   |     ^^^^^
   |
   = note: see issue #97362 <https://github.com/rust-lang/rust/issues/97362> for more information
   = help: add `#![feature(closure_lifetime_binder)]` to the crate attributes to enable
   = note: this compiler was built on YYYY-MM-DD; consider upgrading it if it is out of date
   = help: consider removing `for<...>`

error[E0658]: `for<...>` binders for closures are experimental
  --> $DIR/feature-gate-closure_lifetime_binder.rs:4:5
   |
LL |     for<'a> || -> () {};
   |     ^^^^^^^
   |
   = note: see issue #97362 <https://github.com/rust-lang/rust/issues/97362> for more information
   = help: add `#![feature(closure_lifetime_binder)]` to the crate attributes to enable
   = note: this compiler was built on YYYY-MM-DD; consider upgrading it if it is out of date
   = help: consider removing `for<...>`

error[E0658]: `for<...>` binders for closures are experimental
  --> $DIR/feature-gate-closure_lifetime_binder.rs:6:5
   |
LL |     for<'a, 'b> |_: &'a ()| -> () {};
   |     ^^^^^^^^^^^
   |
   = note: see issue #97362 <https://github.com/rust-lang/rust/issues/97362> for more information
   = help: add `#![feature(closure_lifetime_binder)]` to the crate attributes to enable
   = note: this compiler was built on YYYY-MM-DD; consider upgrading it if it is out of date
   = help: consider removing `for<...>`

error: aborting due to 3 previous errors

For more information about this error, try `rustc --explain E0658`.
"#;
    assert_eq!(ui("feature-gate-closure_lifetime_binder.rs", "2015", source), stderr);
}

#[test]
fn associated_const_equality_names_gca_min_const_items() {
    // tests/ui/feature-gates/feature-gate-associated_const_equality.rs: a gate after `gate_all!`: `associated_const_equality`, part of `gca_min_const_items`
    let source = r#"pub trait TraitWAssocConst {
  const A: usize;
}
pub struct Demo {}

impl TraitWAssocConst for Demo {
  const A: usize = 32;
}

fn foo<A: TraitWAssocConst<A=32>>() {}
//~^ ERROR associated const equality is incomplete

fn main() {
  foo::<Demo>();
}
"#;
    assert_reports(
        &ui("feature-gate-associated_const_equality.rs", "2015", source),
        &[error(10, &["E0658", "associated const equality is incomplete", "`#![feature(gca_min_const_items)]`"])],
    );
}

#[test]
fn a_frontmatter_is_gated() {
    // tests/ui/feature-gates/feature-gate-frontmatter.rs: the lexer's gate: `frontmatter`
    let source = r#"---cargo
//~^ ERROR: frontmatters are experimental
---

fn main() {}
"#;
    let stderr = r#"error[E0658]: frontmatters are experimental
  --> $DIR/feature-gate-frontmatter.rs:1:1
   |
LL | / ---cargo
LL | |
LL | | ---
   | |___^
   |
   = note: see issue #136889 <https://github.com/rust-lang/rust/issues/136889> for more information
   = help: add `#![feature(frontmatter)]` to the crate attributes to enable
   = note: this compiler was built on YYYY-MM-DD; consider upgrading it if it is out of date

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0658`.
"#;
    assert_eq!(ui("feature-gate-frontmatter.rs", "2015", source), stderr);
}

#[test]
fn a_macro_item_is_gated_after_expansion() {
    // tests/ui/feature-gates/feature-gate-decl_macro.rs: `PostExpansionVisitor::visit_item`: `decl_macro`
    let source = r#"#![allow(unused_macros)]

macro m() {} //~ ERROR `macro` is experimental

fn main() {}
"#;
    let stderr = r#"error[E0658]: `macro` is experimental
  --> $DIR/feature-gate-decl_macro.rs:3:1
   |
LL | macro m() {}
   | ^^^^^^^^^^^^
   |
   = note: see issue #39412 <https://github.com/rust-lang/rust/issues/39412> for more information
   = help: add `#![feature(decl_macro)]` to the crate attributes to enable
   = note: this compiler was built on YYYY-MM-DD; consider upgrading it if it is out of date

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0658`.
"#;
    assert_eq!(ui("feature-gate-decl_macro.rs", "2015", source), stderr);
}

#[test]
fn auto_traits_are_gated_after_expansion() {
    // tests/ui/feature-gates/feature-gate-auto-traits.rs: `PostExpansionVisitor::visit_item`: `auto_traits`
    let source = r#"auto trait DummyAutoTrait {} //~ ERROR auto traits are experimental and possibly buggy

pub unsafe auto trait AnotherAutoTrait {} //~ ERROR auto traits are experimental and possibly buggy

fn main() {}
"#;
    let stderr = r#"error[E0658]: auto traits are experimental and possibly buggy
  --> $DIR/feature-gate-auto-traits.rs:1:1
   |
LL | auto trait DummyAutoTrait {}
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = note: see issue #13231 <https://github.com/rust-lang/rust/issues/13231> for more information
   = help: add `#![feature(auto_traits)]` to the crate attributes to enable
   = note: this compiler was built on YYYY-MM-DD; consider upgrading it if it is out of date

error[E0658]: auto traits are experimental and possibly buggy
  --> $DIR/feature-gate-auto-traits.rs:3:1
   |
LL | pub unsafe auto trait AnotherAutoTrait {}
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = note: see issue #13231 <https://github.com/rust-lang/rust/issues/13231> for more information
   = help: add `#![feature(auto_traits)]` to the crate attributes to enable
   = note: this compiler was built on YYYY-MM-DD; consider upgrading it if it is out of date

error: aborting due to 2 previous errors

For more information about this error, try `rustc --explain E0658`.
"#;
    assert_eq!(ui("feature-gate-auto-traits.rs", "2015", source), stderr);
}

#[test]
fn a_trait_alias_is_gated_after_expansion() {
    // tests/ui/feature-gates/feature-gate-trait-alias.rs: `PostExpansionVisitor::visit_item`: `trait_alias`
    let source = r#"trait Foo = Default;
//~^ ERROR trait aliases are experimental

fn main() {}
"#;
    let stderr = r#"error[E0658]: trait aliases are experimental
  --> $DIR/feature-gate-trait-alias.rs:1:1
   |
LL | trait Foo = Default;
   | ^^^^^^^^^^^^^^^^^^^^
   |
   = note: see issue #41517 <https://github.com/rust-lang/rust/issues/41517> for more information
   = help: add `#![feature(trait_alias)]` to the crate attributes to enable
   = note: this compiler was built on YYYY-MM-DD; consider upgrading it if it is out of date

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0658`.
"#;
    assert_eq!(ui_core("feature-gate-trait-alias.rs", "2015", "default", source), stderr);
}

#[test]
fn an_extern_type_is_gated() {
    // tests/ui/feature-gates/feature-gate-extern_types.rs: `PostExpansionVisitor::visit_foreign_item`: `extern_types`
    let source = r#"extern "C" {
    type T; //~ ERROR extern types are experimental
}

fn main() {}
"#;
    let stderr = r#"error[E0658]: extern types are experimental
  --> $DIR/feature-gate-extern_types.rs:2:5
   |
LL |     type T;
   |     ^^^^^^^
   |
   = note: see issue #43467 <https://github.com/rust-lang/rust/issues/43467> for more information
   = help: add `#![feature(extern_types)]` to the crate attributes to enable
   = note: this compiler was built on YYYY-MM-DD; consider upgrading it if it is out of date

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0658`.
"#;
    assert_eq!(ui("feature-gate-extern_types.rs", "2015", source), stderr);
}

#[test]
fn linking_an_llvm_intrinsic_is_gated() {
    // tests/ui/feature-gates/feature-gate-link_llvm_intrinsics.rs: `PostExpansionVisitor::visit_foreign_item`: `link_llvm_intrinsics`
    let source = r#"extern "C" {
    #[link_name = "llvm.sqrt.f32"]
    fn sqrt(x: f32) -> f32;
//~^ ERROR linking to LLVM intrinsics is experimental
}

fn main() {}
"#;
    let stderr = r#"error[E0658]: linking to LLVM intrinsics is experimental
  --> $DIR/feature-gate-link_llvm_intrinsics.rs:3:5
   |
LL |     fn sqrt(x: f32) -> f32;
   |     ^^^^^^^^^^^^^^^^^^^^^^^
   |
   = note: see issue #29602 <https://github.com/rust-lang/rust/issues/29602> for more information
   = help: add `#![feature(link_llvm_intrinsics)]` to the crate attributes to enable
   = note: this compiler was built on YYYY-MM-DD; consider upgrading it if it is out of date

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0658`.
"#;
    assert_eq!(ui("feature-gate-link_llvm_intrinsics.rs", "2015", source), stderr);
}

#[test]
fn a_type_parameter_in_a_binder_is_gated() {
    // tests/ui/feature-gates/feature-gate-non_lifetime_binders.rs: `check_late_bound_lifetime_defs`: `non_lifetime_binders`
    let source = r#"fn foo() where for<T> T:, {}
//~^ ERROR only lifetime parameters can be used in this context

fn main() {}
"#;
    assert_reports(&ui("feature-gate-non_lifetime_binders.rs", "2015", source), &[error(1, &[])]);
}

#[test]
fn doc_cfg_is_gated() {
    // tests/ui/feature-gates/feature-gate-doc_cfg.rs: `PostExpansionVisitor::visit_attribute`'s `doc` gates: `doc_cfg`
    let source = r#"#[doc(cfg(unix))] //~ ERROR
fn main() {}
"#;
    let stderr = r#"error[E0658]: `#[doc(cfg)]` is experimental
  --> $DIR/feature-gate-doc_cfg.rs:1:1
   |
LL | #[doc(cfg(unix))]
   | ^^^^^^^^^^^^^^^^^
   |
   = note: see issue #43781 <https://github.com/rust-lang/rust/issues/43781> for more information
   = help: add `#![feature(doc_cfg)]` to the crate attributes to enable
   = note: this compiler was built on YYYY-MM-DD; consider upgrading it if it is out of date

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0658`.
"#;
    assert_eq!(ui("feature-gate-doc_cfg.rs", "2015", source), stderr);
}

#[test]
fn cfg_d_out_auto_trait_warns() {
    // tests/ui/feature-gates/soft-feature-gate-auto_traits.rs: a soft gate: `auto_traits`
    let source = r#"// For historical reasons, auto traits don't have an erroring pre-expansion feature gate.
// We're now at least issuing a warning for those that only exist before macro expansion.
// FIXME(#154045): Turn this pre-expansion warning into an error and remove the post-expansion gate.
//                 As part of this, move these test cases into `feature-gate-auto-traits.rs`.
//@ check-pass

#[cfg(false)]
auto trait Foo {}
//~^ WARN `auto` traits are unstable
//~| WARN unstable syntax can change at any point in the future

fn main() {}
"#;
    let stderr = r#"warning: `auto` traits are unstable
  --> $DIR/soft-feature-gate-auto_traits.rs:8:1
   |
LL | auto trait Foo {}
   | ^^^^
   |
   = note: see issue #13231 <https://github.com/rust-lang/rust/issues/13231> for more information
   = help: add `#![feature(auto_traits)]` to the crate attributes to enable
   = note: this compiler was built on YYYY-MM-DD; consider upgrading it if it is out of date
   = warning: unstable syntax can change at any point in the future, causing a hard error!
   = note: for more information, see issue #154045 <https://github.com/rust-lang/rust/issues/154045>

warning: 1 warning emitted

"#;
    assert_eq!(ui("soft-feature-gate-auto_traits.rs", "2015", source), stderr);
}

/// std's `Box`, with `Box::new`, as the test uses it, and std's prelude of it.
const BOX_STD: (&str, &str) = (
    "pub use crate::boxed::Box;",
    "pub mod boxed {
    #[lang = \"owned_box\"]
    pub struct Box<T: ?Sized>(*mut T);
    impl<T> Box<T> {
        pub fn new(x: T) -> Box<T> {
            loop {}
        }
    }
}
",
);

#[test]
fn a_box_pattern_is_removed_syntax() {
    // tests/ui/parser/removed-syntax/removed-syntax-box-patterns.rs: `box_patterns` is removed, and the parser errors at each `box` of a pattern, a field shorthand's too
    let source = r#"fn main() {
    let box x = Box::new('c'); //~ ERROR `box` patterns have been removed
    let _: char = x;

    struct Packet { x: Box<i32> }

    let Packet { box x } = Packet { x: Box::new(0) }; //~ ERROR `box` patterns have been removed
    let _: i32 = x;

    let Packet { box ref mut x }; //~ ERROR `box` patterns have been removed
}
"#;
    let removed = ["`box` patterns have been removed (feature `box_patterns`)", "deref!("];
    assert_reports(
        &ui_std("removed-syntax-box-patterns.rs", "2015", "sized", BOX_STD, source),
        &[error(2, &removed), error(7, &removed), error(10, &removed)],
    );
}

#[test]
fn cfg_d_out_macro_items_warn() {
    // tests/ui/feature-gates/soft-feature-gate-decl_macro.rs: a soft gate: `decl_macro`
    let source = r#"// For historical reasons, decl macros 2.0 don't have an erroring pre-expansion feature gate.
// We're now at least issuing a warning for those that only exist before macro expansion.
// FIXME(#154045): Turn this pre-expansion warning into an error and remove the post-expansion gate.
//                 As part of this, move these test cases into `feature-gate-decl_macro.rs`.
//@ check-pass

#[cfg(false)]
macro make() {}
//~^ WARN `macro` is experimental
//~| WARN unstable syntax can change at any point in the future

#[cfg(false)]
macro create { () => {} }
//~^ WARN `macro` is experimental
//~| WARN unstable syntax can change at any point in the future

fn main() {}
"#;
    let stderr = r#"warning: `macro` is experimental
  --> $DIR/soft-feature-gate-decl_macro.rs:8:1
   |
LL | macro make() {}
   | ^^^^^^^^^^^^^^^
   |
   = note: see issue #39412 <https://github.com/rust-lang/rust/issues/39412> for more information
   = help: add `#![feature(decl_macro)]` to the crate attributes to enable
   = note: this compiler was built on YYYY-MM-DD; consider upgrading it if it is out of date
   = warning: unstable syntax can change at any point in the future, causing a hard error!
   = note: for more information, see issue #154045 <https://github.com/rust-lang/rust/issues/154045>

warning: `macro` is experimental
  --> $DIR/soft-feature-gate-decl_macro.rs:13:1
   |
LL | macro create { () => {} }
   | ^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = note: see issue #39412 <https://github.com/rust-lang/rust/issues/39412> for more information
   = help: add `#![feature(decl_macro)]` to the crate attributes to enable
   = note: this compiler was built on YYYY-MM-DD; consider upgrading it if it is out of date
   = warning: unstable syntax can change at any point in the future, causing a hard error!
   = note: for more information, see issue #154045 <https://github.com/rust-lang/rust/issues/154045>

warning: 2 warnings emitted

"#;
    assert_eq!(ui("soft-feature-gate-decl_macro.rs", "2015", source), stderr);
}

#[test]
fn a_cfg_d_out_negative_impl_warns() {
    // tests/ui/feature-gates/soft-feature-gate-negative_impls.rs: a soft gate: `negative_impls`, at the `!`
    let source = r#"// For historical reasons, negative impls don't have an erroring pre-expansion feature gate.
// We're now at least issuing a warning for those that only exist before macro expansion.
// FIXME(#154045): Turn this pre-expansion warning into an error and remove the post-expansion gate.
//                 As part of this, move these test cases into `feature-gate-negative_impls.rs`.
//@ check-pass

#[cfg(false)]
impl !Trait for () {}
//~^ WARN negative impls are experimental
//~| WARN unstable syntax can change at any point in the future

fn main() {}
"#;
    let stderr = r#"warning: negative impls are experimental
  --> $DIR/soft-feature-gate-negative_impls.rs:8:6
   |
LL | impl !Trait for () {}
   |      ^
   |
   = note: see issue #68318 <https://github.com/rust-lang/rust/issues/68318> for more information
   = help: add `#![feature(negative_impls)]` to the crate attributes to enable
   = note: this compiler was built on YYYY-MM-DD; consider upgrading it if it is out of date
   = warning: unstable syntax can change at any point in the future, causing a hard error!
   = note: for more information, see issue #154045 <https://github.com/rust-lang/rust/issues/154045>

warning: 1 warning emitted

"#;
    assert_eq!(ui("soft-feature-gate-negative_impls.rs", "2015", source), stderr);
}

#[test]
fn cfg_d_out_trait_aliases_warn() {
    // tests/ui/feature-gates/soft-feature-gate-trait_alias.rs: a soft gate: `trait_alias`
    let source = r#"// For historical reasons, trait aliases don't have an erroring pre-expansion feature gate.
// We're now at least issuing a warning for those that only exist before macro expansion.
// FIXME(#154045): Turn this pre-expansion warning into an error and remove the post-expansion gate.
//                 As part of this, move these test cases into `feature-gate-trait-alias.rs`.
//@ check-pass

#[cfg(false)]
trait Trait =;
//~^ WARN trait aliases are experimental
//~| WARN unstable syntax can change at any point in the future

#[cfg(false)]
trait Trait<T> = Bound where T: Bound;
//~^ WARN trait aliases are experimental
//~| WARN unstable syntax can change at any point in the future

fn main() {}
"#;
    let stderr = r#"warning: trait aliases are experimental
  --> $DIR/soft-feature-gate-trait_alias.rs:8:1
   |
LL | trait Trait =;
   | ^^^^^^^^^^^^^^
   |
   = note: see issue #41517 <https://github.com/rust-lang/rust/issues/41517> for more information
   = help: add `#![feature(trait_alias)]` to the crate attributes to enable
   = note: this compiler was built on YYYY-MM-DD; consider upgrading it if it is out of date
   = warning: unstable syntax can change at any point in the future, causing a hard error!
   = note: for more information, see issue #154045 <https://github.com/rust-lang/rust/issues/154045>

warning: trait aliases are experimental
  --> $DIR/soft-feature-gate-trait_alias.rs:13:1
   |
LL | trait Trait<T> = Bound where T: Bound;
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = note: see issue #41517 <https://github.com/rust-lang/rust/issues/41517> for more information
   = help: add `#![feature(trait_alias)]` to the crate attributes to enable
   = note: this compiler was built on YYYY-MM-DD; consider upgrading it if it is out of date
   = warning: unstable syntax can change at any point in the future, causing a hard error!
   = note: for more information, see issue #154045 <https://github.com/rust-lang/rust/issues/154045>

warning: 2 warnings emitted

"#;
    assert_eq!(ui("soft-feature-gate-trait_alias.rs", "2015", source), stderr);
}

#[test]
fn a_cfg_d_out_try_block_warns() {
    // tests/ui/feature-gates/soft-feature-gate-try_blocks.rs: a soft gate: `try_blocks`
    let source = r#"// For historical reasons, try blocks don't have an erroring pre-expansion feature gate.
// We're now at least issuing a warning for those that only exist before macro expansion.
// FIXME(#154045): Turn this pre-expansion warning into an error and remove the post-expansion gate.
//                 As part of this, move these test cases into `feature-gate-try_blocks.rs`.
//@ edition: 2018
//@ check-pass

fn main() {
    #[cfg(false)]
    try {}
    //~^ WARN `try` blocks are unstable
    //~| WARN unstable syntax can change at any point
}
"#;
    let stderr = r#"warning: `try` blocks are unstable
  --> $DIR/soft-feature-gate-try_blocks.rs:10:5
   |
LL |     try {}
   |     ^^^^^^
   |
   = note: see issue #154391 <https://github.com/rust-lang/rust/issues/154391> for more information
   = help: add `#![feature(try_blocks)]` to the crate attributes to enable
   = note: this compiler was built on YYYY-MM-DD; consider upgrading it if it is out of date
   = warning: unstable syntax can change at any point in the future, causing a hard error!
   = note: for more information, see issue #154045 <https://github.com/rust-lang/rust/issues/154045>

warning: 1 warning emitted

"#;
    assert_eq!(ui("soft-feature-gate-try_blocks.rs", "2018", source), stderr);
}

#[test]
fn a_post_expansion_error_cancels_the_soft_gate_at_its_span() {
    // tests/ui/feature-gates/soft-syntax-gates-with-errors.rs: soft gates under errors: an error at a span steals its warning, and none is left once any error is
    let source = r#"//@ check-fail
// This file is used to test the behavior of the early-pass syntax warnings.
// If macro syntax is stabilized, replace with a different unstable syntax.

macro a() {}
//~^ ERROR: `macro` is experimental

#[cfg(false)]
macro b() {}

macro_rules! identity {
    ($($x:tt)*) => ($($x)*);
}

identity! {
    macro c() {}
    //~^ ERROR: `macro` is experimental
}

#[cfg(false)]
identity! {
    macro d() {} // No error
}

identity! {
    #[cfg(false)]
    macro e() {}
}

fn main() {}
"#;
    let stderr = r#"error[E0658]: `macro` is experimental
  --> $DIR/soft-syntax-gates-with-errors.rs:5:1
   |
LL | macro a() {}
   | ^^^^^^^^^^^^
   |
   = note: see issue #39412 <https://github.com/rust-lang/rust/issues/39412> for more information
   = help: add `#![feature(decl_macro)]` to the crate attributes to enable
   = note: this compiler was built on YYYY-MM-DD; consider upgrading it if it is out of date

error[E0658]: `macro` is experimental
  --> $DIR/soft-syntax-gates-with-errors.rs:16:5
   |
LL |     macro c() {}
   |     ^^^^^^^^^^^^
   |
   = note: see issue #39412 <https://github.com/rust-lang/rust/issues/39412> for more information
   = help: add `#![feature(decl_macro)]` to the crate attributes to enable
   = note: this compiler was built on YYYY-MM-DD; consider upgrading it if it is out of date

error: aborting due to 2 previous errors

For more information about this error, try `rustc --explain E0658`.
"#;
    assert_eq!(ui("soft-syntax-gates-with-errors.rs", "2015", source), stderr);
}

#[test]
fn soft_gates_warn_where_expansion_removed_the_item() {
    // tests/ui/feature-gates/soft-syntax-gates-without-errors.rs: soft gates without errors: a warning for each gated span expansion dropped
    let source = r#"//@ check-pass
// This file is used to test the behavior of the early-pass syntax warnings.
// If macro syntax is stabilized, replace with a different unstable syntax.

#[cfg(false)]
macro b() {}
//~^ WARN: `macro` is experimental
//~| WARN: unstable syntax

macro_rules! identity {
    ($($x:tt)*) => ($($x)*);
}

#[cfg(false)]
identity! {
    macro d() {} // No error
}

identity! {
    #[cfg(false)]
    macro e() {}
    //~^ WARN: `macro` is experimental
    //~| WARN: unstable syntax
}

fn main() {}
"#;
    let stderr = r#"warning: `macro` is experimental
  --> $DIR/soft-syntax-gates-without-errors.rs:6:1
   |
LL | macro b() {}
   | ^^^^^^^^^^^^
   |
   = note: see issue #39412 <https://github.com/rust-lang/rust/issues/39412> for more information
   = help: add `#![feature(decl_macro)]` to the crate attributes to enable
   = note: this compiler was built on YYYY-MM-DD; consider upgrading it if it is out of date
   = warning: unstable syntax can change at any point in the future, causing a hard error!
   = note: for more information, see issue #154045 <https://github.com/rust-lang/rust/issues/154045>

warning: `macro` is experimental
  --> $DIR/soft-syntax-gates-without-errors.rs:21:5
   |
LL |     macro e() {}
   |     ^^^^^^^^^^^^
   |
   = note: see issue #39412 <https://github.com/rust-lang/rust/issues/39412> for more information
   = help: add `#![feature(decl_macro)]` to the crate attributes to enable
   = note: this compiler was built on YYYY-MM-DD; consider upgrading it if it is out of date
   = warning: unstable syntax can change at any point in the future, causing a hard error!
   = note: for more information, see issue #154045 <https://github.com/rust-lang/rust/issues/154045>

warning: 2 warnings emitted

"#;
    assert_eq!(ui("soft-syntax-gates-without-errors.rs", "2015", source), stderr);
}
