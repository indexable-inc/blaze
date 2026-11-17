//! D104: a blaze-edition binary's root `fn main` is the input of `std::main` with no attribute
//! written (blaze-unit-graph `Implicit::entry`, task/lower.rs `IMPLICIT_MAIN`). Here `std` is a
//! small `#![no_core]` crate whose `main` is an attribute `macro_rules!` that writes one marker
//! fn, so what the root declares says whether the call was made: the marker and no `main` when
//! it was, `main` and no marker when it was not.

use blaze_own::Alloc;
use blaze_surface::{Namespace, PathKind};
use blaze_unit_graph::Implicit;

use super::entry::{Texts, input, resolved};
use crate::names::{Why, Written};
use crate::resolve::CrateInput;

/// The edition's `std` as far as the call needs it: `main` an attribute macro (bstd's shape,
/// `attr()` arm) that replaces the item with `granted_entry`.
const STD: &str = "#![no_core]
#[macro_export]
macro_rules! main {
    attr() ($(#[$meta:meta])* $vis:vis fn main($($params:tt)*) $body:block) => {
        pub fn granted_entry() {}
    };
}
";

/// The owner's hello world's shape (no attribute written).
const GRANTED: &str = "#![no_core]
fn main(cx: &Cx, out: Out) {}
";

/// The same, with the file writing its own start (as D59's interim module does).
const OWN_START: &str = "#![no_core]
fn main(cx: &Cx, io: Io) {}
mod __blaze_entry {
    #[unsafe(no_mangle)]
    pub extern \"C\" fn bstd_hosted_start() {}
}
";

static TEXTS: Texts = Texts(&[&[("/user/main.rs", GRANTED)], &[("/std/lib.rs", STD)], &[("/own/main.rs", OWN_START)]]);

const DEPS: &[(&str, usize)] = &[("std", 1)];

fn crate_of(name: &'static str, root: &'static str, entry: bool) -> CrateInput<'static> {
    let mut it = input(name, root, DEPS);
    it.implicit = Implicit { entry, ..it.implicit };
    it
}

/// Whether the root of crate `krate` declares the value `name`.
fn declares(crates: &[CrateInput<'static>], krate: usize, name: &'static str) -> Result<(), Why> {
    let names = resolved(crates, &TEXTS);
    let root = names.root(krate);
    let segments: &'static [&'static str] = match name {
        "main" => &["main"],
        _ => &["granted_entry"],
    };
    names.item_at(root, Namespace::Values, Written { kind: PathKind::Plain, segments }, Alloc::global()).map(|_| ()).map_err(|e| e.why)
}

/// The owner's form, entry set: `std::main` took the item (the marker is declared, the written
/// `main` is not). Without the call (a planted `entry: false`) the second assertion fails.
#[test]
fn a_blaze_binary_root_main_is_std_main_input() {
    let crates = [crate_of("user", "/user/main.rs", true), input("std", "/std/lib.rs", &[]), input("own", "/own/main.rs", DEPS)];
    assert_eq!(declares(&crates, 0, "granted_entry"), Ok(()), "the implicit `std::main` call wrote the marker");
    assert_eq!(declares(&crates, 0, "main"), Err(Why::NotFound), "the written `main` was the call's input");
    let rust = [crate_of("user", "/user/main.rs", false), input("std", "/std/lib.rs", &[]), input("own", "/own/main.rs", DEPS)];
    assert_eq!(declares(&rust, 0, "main"), Ok(()), "with no entry (a library, a Rust edition) `main` is as written");
    assert_eq!(declares(&rust, 0, "granted_entry"), Err(Why::NotFound));
}

/// A root that exports `bstd_hosted_start` itself writes its own start: no implicit call, even
/// with entry set.
#[test]
fn a_root_that_exports_its_entry_has_no_implicit_call() {
    let crates = [input("user", "/user/main.rs", DEPS), input("std", "/std/lib.rs", &[]), crate_of("own", "/own/main.rs", true)];
    assert_eq!(declares(&crates, 2, "main"), Ok(()), "`main` is as written");
    assert_eq!(declares(&crates, 2, "granted_entry"), Err(Why::NotFound), "no call was made");
}
