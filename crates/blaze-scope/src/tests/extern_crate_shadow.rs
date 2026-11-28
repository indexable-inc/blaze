//! Explicit crate-root `extern crate` declarations shadow `--extern` prelude entries of the same
//! name regardless of `ItemKey` sort order (Reference `names.preludes.extern`), while submodule
//! `extern crate` items stay local to their module.

use blaze_id::{ContentId, CrateItemKey};
use blaze_own::Alloc;
use blaze_surface::{Edition, Namespace, PathKind};
use blaze_unit_graph::{Implicit, Prelude, StdIs};

use super::entry::{Texts, resolved};
use crate::names::Written;
use crate::resolve::CrateInput;

const RUST: Implicit = Implicit { std: StdIs::Rust, prelude: Prelude::Rust(edition::Edition::Edition2021), proc_macro: false, entry: false };

fn crate_with_key(
    name: &'static str,
    key_byte: u8,
    root: &'static str,
    deps: &'static [(&'static str, usize)],
) -> CrateInput<'static> {
    CrateInput {
        name,
        key: CrateItemKey(ContentId([key_byte; 32])),
        edition: Edition::E2021,
        implicit: RUST,
        root,
        shown_from: "/",
        features: &[],
        deps,
        cfg: &[],
        script: None,
    }
}

const SELF_SHADOW_USER: &str = "#![no_core]
extern crate self as foo;
pub struct Marker;
pub use foo::Marker as FromRootPlain;
pub use ::foo::Marker as FromRootAbs;
pub mod sub {
    pub use foo::Marker as FromSubPlain;
    pub use ::foo::Marker as FromSubAbs;
}
";

const EXT_FOO: &str = "#![no_core]
pub struct Marker;
";

static SELF_SHADOW_TEXTS: Texts = Texts(&[
    &[("/user/lib.rs", SELF_SHADOW_USER)],
    &[("/ext_foo/lib.rs", EXT_FOO)],
]);

/// `extern crate self as foo;` at the crate root shadows a `--extern foo` dependency of the same
/// name in both `ItemKey` orders (`ext_root < self_root` and `self_root < ext_root`).
#[test]
fn root_extern_crate_self_shadows_extern_prelude_in_both_key_orders() {
    for (self_byte, ext_byte, expect_ext_less) in [(0xfe, 0x01, true), (0x01, 0xfe, false)] {
        let crates = [
            crate_with_key("user", self_byte, "/user/lib.rs", &[("foo", 1)]),
            crate_with_key("foo", ext_byte, "/ext_foo/lib.rs", &[]),
        ];
        let names = resolved(&crates, &SELF_SHADOW_TEXTS);
        let self_root = names.root(0);
        let ext_root = names.root(1);
        assert_eq!(ext_root < self_root, expect_ext_less, "must exercise both ItemKey sort orders");

        let at = |segments: &'static [&'static str]| {
            names
                .item_at(self_root, Namespace::Types, Written { kind: PathKind::Plain, segments }, Alloc::global())
                .unwrap_or_else(|e| panic!("{segments:?} (self_byte={self_byte:#x}): {e}"))
        };
        for path in [
            &["FromRootPlain"][..],
            &["FromRootAbs"][..],
            &["sub", "FromSubPlain"][..],
            &["sub", "FromSubAbs"][..],
        ] {
            let found = at(path);
            assert_eq!(
                found.krate, 0,
                "`{path:?}` must resolve to self (krate 0), not external `foo` (krate 1), when ext_root < self_root is {expect_ext_less}"
            );
        }
        let abs_direct = names
            .item_at(self_root, Namespace::Types, Written { kind: PathKind::Abs, segments: &["foo", "Marker"] }, Alloc::global())
            .unwrap_or_else(|e| panic!("`::foo::Marker`: {e}"));
        assert_eq!(abs_direct.krate, 0, "`::foo::Marker` must resolve to self");
    }
}

const OTHER_SHADOW_USER: &str = "#![no_core]
extern crate other as foo;
pub use foo::Marker as FromRootPlain;
pub use ::foo::Marker as FromRootAbs;
pub mod sub {
    pub use foo::Marker as FromSubPlain;
    pub use ::foo::Marker as FromSubAbs;
}
";

const OTHER_DEP: &str = "#![no_core]
pub struct Marker;
";

static OTHER_SHADOW_TEXTS: Texts = Texts(&[
    &[("/user/lib.rs", OTHER_SHADOW_USER)],
    &[("/ext_foo/lib.rs", EXT_FOO)],
    &[("/other/lib.rs", OTHER_DEP)],
]);

/// `extern crate other as foo;` at the crate root shadows a `--extern foo` dependency of the same
/// name in both `ItemKey` orders (`ext_root < other_root` and `other_root < ext_root`).
#[test]
fn root_extern_crate_rename_shadows_extern_prelude_in_both_key_orders() {
    for (other_byte, ext_byte, expect_ext_less) in [(0xfe, 0x01, true), (0x01, 0xfe, false)] {
        let crates = [
            crate_with_key("user", 0x80, "/user/lib.rs", &[("foo", 1), ("other", 2)]),
            crate_with_key("foo", ext_byte, "/ext_foo/lib.rs", &[]),
            crate_with_key("other", other_byte, "/other/lib.rs", &[]),
        ];
        let names = resolved(&crates, &OTHER_SHADOW_TEXTS);
        let user_root = names.root(0);
        let ext_root = names.root(1);
        let other_root = names.root(2);
        assert_eq!(ext_root < other_root, expect_ext_less, "must exercise both ItemKey sort orders");

        let at = |segments: &'static [&'static str]| {
            names
                .item_at(user_root, Namespace::Types, Written { kind: PathKind::Plain, segments }, Alloc::global())
                .unwrap_or_else(|e| panic!("{segments:?} (other_byte={other_byte:#x}): {e}"))
        };
        for path in [
            &["FromRootPlain"][..],
            &["FromRootAbs"][..],
            &["sub", "FromSubPlain"][..],
            &["sub", "FromSubAbs"][..],
        ] {
            let found = at(path);
            assert_eq!(
                found.krate, 2,
                "`{path:?}` must resolve to `other` (krate 2), not external `foo` (krate 1), when ext_root < other_root is {expect_ext_less}"
            );
        }
    }
}

const NO_SHADOW_USER: &str = "#![no_core]
pub struct Marker;
pub use foo::Marker as FromRootPlain;
pub use ::foo::Marker as FromRootAbs;
pub mod local_shadow {
    extern crate self as foo;
    pub use foo::Marker as LocalPlain;
    pub use ::foo::Marker as LocalAbs;
}
pub mod sibling {
    pub use foo::Marker as SiblingPlain;
    pub use ::foo::Marker as SiblingAbs;
}
";

static NO_SHADOW_TEXTS: Texts = Texts(&[
    &[("/user/lib.rs", NO_SHADOW_USER)],
    &[("/ext_foo/lib.rs", EXT_FOO)],
]);

/// Negative controls:
/// - Without an explicit crate-root `extern crate`, `foo` resolves to `plan.extern_prelude` (the
///   external crate).
/// - An `extern crate self as foo;` inside a submodule binds `foo` only in that module and does
///   NOT shadow the crate-root `extern_prelude` in sibling modules or `::foo`.
#[test]
fn without_root_extern_crate_or_in_submodule_extern_prelude_is_not_shadowed() {
    for (self_byte, ext_byte) in [(0xfe, 0x01), (0x01, 0xfe)] {
        let crates = [
            crate_with_key("user", self_byte, "/user/lib.rs", &[("foo", 1)]),
            crate_with_key("foo", ext_byte, "/ext_foo/lib.rs", &[]),
        ];
        let names = resolved(&crates, &NO_SHADOW_TEXTS);
        let root = names.root(0);
        let at = |segments: &'static [&'static str]| {
            names
                .item_at(root, Namespace::Types, Written { kind: PathKind::Plain, segments }, Alloc::global())
                .unwrap_or_else(|e| panic!("{segments:?}: {e}"))
        };
        assert_eq!(at(&["FromRootPlain"]).krate, 1, "root `foo` stays external dep");
        assert_eq!(at(&["FromRootAbs"]).krate, 1, "root `::foo` stays external dep");
        assert_eq!(at(&["local_shadow", "LocalPlain"]).krate, 0, "submodule's own `extern crate self as foo;` binds locally");
        assert_eq!(at(&["local_shadow", "LocalAbs"]).krate, 1, "`::foo` still uses crate-root extern prelude");
        assert_eq!(at(&["sibling", "SiblingPlain"]).krate, 1, "sibling module's `foo` is not shadowed by another submodule's `extern crate`");
        assert_eq!(at(&["sibling", "SiblingAbs"]).krate, 1, "sibling module's `::foo` is not shadowed");
    }
}

const DUP_ROOT_USER: &str = "#![no_core]
extern crate self as foo;
extern crate other as foo;
pub struct Marker;
pub mod sub {
    pub use foo::Marker as FromSubPlain;
    pub use ::foo::Marker as FromSubAbs;
}
";

static DUP_ROOT_TEXTS: Texts = Texts(&[
    &[("/user/lib.rs", DUP_ROOT_USER)],
    &[("/ext_foo/lib.rs", EXT_FOO)],
    &[("/other/lib.rs", OTHER_DEP)],
]);

/// When the crate root lists two `extern crate` declarations binding the same name (for example
/// in 2015 when an explicit `extern crate std as core;` precedes the injected `extern crate
/// core;`), the earlier declaration wins deterministically across `ItemKey` orders.
#[test]
fn duplicate_root_extern_crate_keeps_first_declaration_in_both_key_orders() {
    for (self_byte, ext_byte, other_byte) in [(0xfe, 0x01, 0x02), (0x01, 0xfe, 0xfd)] {
        let crates = [
            crate_with_key("user", self_byte, "/user/lib.rs", &[("foo", 1), ("other", 2)]),
            crate_with_key("foo", ext_byte, "/ext_foo/lib.rs", &[]),
            crate_with_key("other", other_byte, "/other/lib.rs", &[]),
        ];
        let names = resolved(&crates, &DUP_ROOT_TEXTS);
        let root = names.root(0);
        let at = |segments: &'static [&'static str]| {
            names
                .item_at(root, Namespace::Types, Written { kind: PathKind::Plain, segments }, Alloc::global())
                .unwrap_or_else(|e| panic!("{segments:?}: {e}"))
        };
        assert_eq!(at(&["sub", "FromSubPlain"]).krate, 0, "first root `extern crate` wins for plain path");
        assert_eq!(at(&["sub", "FromSubAbs"]).krate, 0, "first root `extern crate` wins for `::foo` path");
    }
}
