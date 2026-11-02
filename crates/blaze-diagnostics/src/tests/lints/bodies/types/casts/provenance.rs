//! `implicit_provenance_casts`: rustc's `tests/ui/lint/lint-strict-provenance-*.rs` and
//! `tests/ui/feature-gates/feature-gate-strict_provenance_lints.rs`.

use crate::tests::{assert_reports, error, ui};

#[test]
fn lossy_casts() {
    let source = r##"#![feature(strict_provenance_lints)]
#![deny(implicit_provenance_casts)]

fn main() {
    let x: u8 = 37;
    let addr: usize = &x as *const u8 as usize;
    //~^ ERROR cast from `*const u8` to `usize` implicitly exposes pointer provenance

    let addr_32bit = &x as *const u8 as u32;
    //~^ ERROR cast from `*const u8` to `u32` implicitly exposes pointer provenance

    // don't add unnecessary parens in the suggestion
    let ptr = &x as *const u8;
    let ptr_addr = ptr as usize;
    //~^ ERROR cast from `*const u8` to `usize` implicitly exposes pointer provenance
    let ptr_addr_32bit = ptr as u32;
    //~^ ERROR cast from `*const u8` to `u32` implicitly exposes pointer provenance
}
"##;
    let expected = r##"error: cast from `*const u8` to `usize` implicitly exposes pointer provenance
  --> $DIR/lint-strict-provenance-lossy-casts.rs:6:23
   |
LL |     let addr: usize = &x as *const u8 as usize;
   |                       ^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = help: if conforming to strict provenance is not possible, use `.expose_provenance()`
   = note: for more information, visit <https://doc.rust-lang.org/std/ptr/index.html#provenance>
note: the lint level is defined here
  --> $DIR/lint-strict-provenance-lossy-casts.rs:2:9
   |
LL | #![deny(implicit_provenance_casts)]
   |         ^^^^^^^^^^^^^^^^^^^^^^^^^
help: use `.addr()` to obtain the address of a pointer
   |
LL -     let addr: usize = &x as *const u8 as usize;
LL +     let addr: usize = (&x as *const u8).addr();
   |

error: cast from `*const u8` to `u32` implicitly exposes pointer provenance
  --> $DIR/lint-strict-provenance-lossy-casts.rs:9:22
   |
LL |     let addr_32bit = &x as *const u8 as u32;
   |                      ^^^^^^^^^^^^^^^^^^^^^^
   |
   = help: if conforming to strict provenance is not possible, use `.expose_provenance()`
   = note: for more information, visit <https://doc.rust-lang.org/std/ptr/index.html#provenance>
help: use `.addr()` to obtain the address of a pointer
   |
LL |     let addr_32bit = (&x as *const u8).addr() as u32;
   |                      +               ++++++++

error: cast from `*const u8` to `usize` implicitly exposes pointer provenance
  --> $DIR/lint-strict-provenance-lossy-casts.rs:14:20
   |
LL |     let ptr_addr = ptr as usize;
   |                    ^^^---------
   |                       |
   |                       help: use `.addr()` to obtain the address of a pointer: `.addr()`
   |
   = help: if conforming to strict provenance is not possible, use `.expose_provenance()`
   = note: for more information, visit <https://doc.rust-lang.org/std/ptr/index.html#provenance>

error: cast from `*const u8` to `u32` implicitly exposes pointer provenance
  --> $DIR/lint-strict-provenance-lossy-casts.rs:16:26
   |
LL |     let ptr_addr_32bit = ptr as u32;
   |                          ^^^-------
   |                             |
   |                             help: use `.addr()` to obtain the address of a pointer: `.addr() as u32`
   |
   = help: if conforming to strict provenance is not possible, use `.expose_provenance()`
   = note: for more information, visit <https://doc.rust-lang.org/std/ptr/index.html#provenance>

error: aborting due to 4 previous errors

"##;
    let stderr = crate::tests::items::ui_core("lint-strict-provenance-lossy-casts.rs", "2015", "copy", source);
    assert_eq!(stderr, expected);
}

#[test]
fn fuzzy_casts() {
    let source = r##"#![feature(strict_provenance_lints)]
#![deny(implicit_provenance_casts)]

fn main() {
    let dangling = 16_usize as *const u8;
    //~^ ERROR cast from `usize` to `*const u8` implicitly relies on exposed provenance
}
"##;
    let expected = r##"error: cast from `usize` to `*const u8` implicitly relies on exposed provenance
  --> $DIR/lint-strict-provenance-fuzzy-casts.rs:5:20
   |
LL |     let dangling = 16_usize as *const u8;
   |                    ^^^^^^^^^^^^^^^^^^^^^
   |
   = help: if conforming to strict provenance is not possible, use `std::ptr::with_exposed_provenance()`
   = note: for more information, visit <https://doc.rust-lang.org/std/ptr/index.html#provenance>
note: the lint level is defined here
  --> $DIR/lint-strict-provenance-fuzzy-casts.rs:2:9
   |
LL | #![deny(implicit_provenance_casts)]
   |         ^^^^^^^^^^^^^^^^^^^^^^^^^
help: use `.with_addr()` to adjust the address of a valid pointer in the same allocation
   |
LL -     let dangling = 16_usize as *const u8;
LL +     let dangling = (...).with_addr(16_usize);
   |

error: aborting due to 1 previous error

"##;
    let stderr = ui("lint-strict-provenance-fuzzy-casts.rs", "2015", source);
    assert_eq!(stderr, expected);
}

/// Casts in macros' expansions: no suggestions.
#[test]
fn macro_casts() {
    let source = r##"#![feature(strict_provenance_lints)]
#![deny(implicit_provenance_casts)]

macro_rules! cast {
    ($e:expr, $t:ty) => {
        $e as $t
        //~^ ERROR cast from `*const u8` to `usize`
        //~| ERROR cast from `usize` to `*const u8`
    };
}

macro_rules! p2i {
    ($e:expr) => { $e as usize };
    //~^ ERROR cast from `*const u8` to `usize`
}

macro_rules! i2p {
    ($e:expr) => { $e as *const () };
    //~^ ERROR cast from `usize` to `*const ()`
}

fn main() {
    let ptr = &0u8 as *const u8;
    let _addr = cast!(ptr, usize);

    let _ptr = cast!(0usize, *const u8);

    let x = 1u8;
    p2i!(&raw const x);

    i2p!(0x42);
}
"##;
    let stderr = ui("lint-strict-provenance-macro-casts.rs", "2015", source);
    assert_reports(
        &stderr,
        &[
            error(6, &["`*const u8`", "`usize`"]),
            error(6, &["`*const u8`", "`usize`"]),
            error(13, &["`*const u8`", "`usize`"]),
            error(18, &["`*const ()`", "`usize`"]),
        ],
    );
}

/// Without `#![feature(strict_provenance_lints)]` the lint is unknown and lints nothing. The
/// diagnostics are of the lint attributes (the levels child's); what this checks of the pass is
/// that its casts give none.
#[test]
fn feature_gate() {
    let source = r##"//@ check-pass

#![deny(implicit_provenance_casts)]
//~^ WARNING unknown lint: `implicit_provenance_casts`

// feature-gating also applies when the old names are helpfully replaced with the new one:
#![deny(fuzzy_provenance_casts)]
//~^ WARNING lint `fuzzy_provenance_casts` has been renamed to `implicit_provenance_casts`
//~| WARNING unknown lint: `implicit_provenance_casts`
#![deny(lossy_provenance_casts)]
//~^ WARNING lint `lossy_provenance_casts` has been renamed to `implicit_provenance_casts`
//~| WARNING unknown lint: `implicit_provenance_casts`

fn main() {
    // no warnings emitted since the lints are not activated

    let _dangling = 16_usize as *const u8;

    let x: u8 = 37;
    let _addr: usize = &x as *const u8 as usize;
}
"##;
    let expected = r##"warning: unknown lint: `implicit_provenance_casts`
  --> $DIR/feature-gate-strict_provenance_lints.rs:3:9
   |
LL | #![deny(implicit_provenance_casts)]
   |         ^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = note: the `implicit_provenance_casts` lint is unstable
   = note: see issue #130351 <https://github.com/rust-lang/rust/issues/130351> for more information
   = help: add `#![feature(strict_provenance_lints)]` to the crate attributes to enable
   = note: this compiler was built on YYYY-MM-DD; consider upgrading it if it is out of date
   = note: `#[warn(unknown_lints)]` on by default

warning: lint `fuzzy_provenance_casts` has been renamed to `implicit_provenance_casts`
  --> $DIR/feature-gate-strict_provenance_lints.rs:7:9
   |
LL | #![deny(fuzzy_provenance_casts)]
   |         ^^^^^^^^^^^^^^^^^^^^^^ help: use the new name: `implicit_provenance_casts`
   |
   = note: `#[warn(renamed_and_removed_lints)]` on by default

warning: unknown lint: `implicit_provenance_casts`
  --> $DIR/feature-gate-strict_provenance_lints.rs:7:9
   |
LL | #![deny(fuzzy_provenance_casts)]
   |         ^^^^^^^^^^^^^^^^^^^^^^
   |
   = note: the `implicit_provenance_casts` lint is unstable
   = note: see issue #130351 <https://github.com/rust-lang/rust/issues/130351> for more information
   = help: add `#![feature(strict_provenance_lints)]` to the crate attributes to enable
   = note: this compiler was built on YYYY-MM-DD; consider upgrading it if it is out of date

warning: lint `lossy_provenance_casts` has been renamed to `implicit_provenance_casts`
  --> $DIR/feature-gate-strict_provenance_lints.rs:10:9
   |
LL | #![deny(lossy_provenance_casts)]
   |         ^^^^^^^^^^^^^^^^^^^^^^ help: use the new name: `implicit_provenance_casts`

warning: unknown lint: `implicit_provenance_casts`
  --> $DIR/feature-gate-strict_provenance_lints.rs:10:9
   |
LL | #![deny(lossy_provenance_casts)]
   |         ^^^^^^^^^^^^^^^^^^^^^^
   |
   = note: the `implicit_provenance_casts` lint is unstable
   = note: see issue #130351 <https://github.com/rust-lang/rust/issues/130351> for more information
   = help: add `#![feature(strict_provenance_lints)]` to the crate attributes to enable
   = note: this compiler was built on YYYY-MM-DD; consider upgrading it if it is out of date

warning: 5 warnings emitted

"##;
    let stderr = ui("feature-gate-strict_provenance_lints.rs", "2015", source);
    assert_eq!(stderr, expected);
}
