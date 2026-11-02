//! `unexpected_cfgs`: rustc's `tests/ui/check-cfg/*` UI tests, each `.stderr` as it is. A test
//! of revisions has the revision's `--cfg` as compiletest gives it, set as the fixture's cfg.

use super::{AUTO, flags, ui_cfg};

const DIAGNOTICS: &str = r##"//@ check-pass
//@ no-auto-check-cfg
//@ revisions: cargo rustc
//@ [rustc]unset-rustc-env:CARGO_CRATE_NAME
//@ [cargo]rustc-env:CARGO_CRATE_NAME=foo
//@ compile-flags: --check-cfg=cfg(feature,values("foo")) --check-cfg=cfg(no_values)
//@ compile-flags: --check-cfg=cfg(quote,values("quote"))

#[cfg(featur)]
//~^ WARNING unexpected `cfg` condition name
fn feature() {}

#[cfg(featur = "foo")]
//~^ WARNING unexpected `cfg` condition name
fn feature() {}

#[cfg(featur = "fo")]
//~^ WARNING unexpected `cfg` condition name
fn feature() {}

#[cfg(feature = "foo")]
fn feature() {}

#[cfg(no_value)]
//~^ WARNING unexpected `cfg` condition name
fn no_values() {}

#[cfg(no_value = "foo")]
//~^ WARNING unexpected `cfg` condition name
fn no_values() {}

#[cfg(no_values = "bar")]
//~^ WARNING unexpected `cfg` condition value
fn no_values() {}

#[cfg(quote = "quote\"")]
//~^ WARNING unexpected `cfg` condition value
fn no_values() {}

fn main() {}
"##;

const DIAGNOTICS_FLAGS: &[&str] = &[r#"cfg(feature,values("foo"))"#, "cfg(no_values)", r#"cfg(quote,values("quote"))"#];

#[test]
fn diagnotics_rustc() {
    let expected = r##"warning: unexpected `cfg` condition name: `featur`
  --> $DIR/diagnotics.rs:9:7
   |
LL | #[cfg(featur)]
   |       ^^^^^^ help: there is a config with a similar name: `feature`
   |
   = help: expected values for `feature` are: `foo`
   = help: to expect this configuration use `--check-cfg=cfg(featur)`
   = note: see <https://doc.rust-lang.org/nightly/rustc/check-cfg.html> for more information about checking conditional configuration
   = note: `#[warn(unexpected_cfgs)]` on by default

warning: unexpected `cfg` condition name: `featur`
  --> $DIR/diagnotics.rs:13:7
   |
LL | #[cfg(featur = "foo")]
   |       ^^^^^^^^^^^^^^
   |
   = help: to expect this configuration use `--check-cfg=cfg(featur, values("foo"))`
   = note: see <https://doc.rust-lang.org/nightly/rustc/check-cfg.html> for more information about checking conditional configuration
help: there is a config with a similar name and value
   |
LL | #[cfg(feature = "foo")]
   |             +

warning: unexpected `cfg` condition name: `featur`
  --> $DIR/diagnotics.rs:17:7
   |
LL | #[cfg(featur = "fo")]
   |       ^^^^^^^^^^^^^
   |
   = help: expected values for `feature` are: `foo`
   = help: to expect this configuration use `--check-cfg=cfg(featur, values("fo"))`
   = note: see <https://doc.rust-lang.org/nightly/rustc/check-cfg.html> for more information about checking conditional configuration
help: there is a config with a similar name and different values
   |
LL - #[cfg(featur = "fo")]
LL + #[cfg(feature = "foo")]
   |

warning: unexpected `cfg` condition name: `no_value`
  --> $DIR/diagnotics.rs:24:7
   |
LL | #[cfg(no_value)]
   |       ^^^^^^^^ help: there is a config with a similar name: `no_values`
   |
   = help: to expect this configuration use `--check-cfg=cfg(no_value)`
   = note: see <https://doc.rust-lang.org/nightly/rustc/check-cfg.html> for more information about checking conditional configuration

warning: unexpected `cfg` condition name: `no_value`
  --> $DIR/diagnotics.rs:28:7
   |
LL | #[cfg(no_value = "foo")]
   |       ^^^^^^^^^^^^^^^^
   |
   = help: to expect this configuration use `--check-cfg=cfg(no_value, values("foo"))`
   = note: see <https://doc.rust-lang.org/nightly/rustc/check-cfg.html> for more information about checking conditional configuration
help: there is a config with a similar name and no value
   |
LL - #[cfg(no_value = "foo")]
LL + #[cfg(no_values)]
   |

warning: unexpected `cfg` condition value: `bar`
  --> $DIR/diagnotics.rs:32:7
   |
LL | #[cfg(no_values = "bar")]
   |       ^^^^^^^^^--------
   |                |
   |                help: remove the value
   |
   = note: no expected value for `no_values`
   = help: to expect this configuration use `--check-cfg=cfg(no_values, values("bar"))`
   = note: see <https://doc.rust-lang.org/nightly/rustc/check-cfg.html> for more information about checking conditional configuration

warning: unexpected `cfg` condition value: `quote"`
  --> $DIR/diagnotics.rs:36:7
   |
LL | #[cfg(quote = "quote\"")]
   |       ^^^^^^^^---------
   |               |
   |               help: there is a expected value with a similar name: `"quote"`
   |
   = note: expected values for `quote` are: `quote`
   = help: to expect this configuration use `--check-cfg=cfg(quote, values("quote\""))`
   = note: see <https://doc.rust-lang.org/nightly/rustc/check-cfg.html> for more information about checking conditional configuration

warning: 7 warnings emitted

"##;
    let options = flags(DIAGNOTICS_FLAGS, &[], false);
    assert_eq!(ui_cfg("", "diagnotics.rs", "edition:2015 cfg:rustc", DIAGNOTICS, &options), expected);
}

#[test]
fn diagnotics_cargo() {
    let expected = r##"warning: unexpected `cfg` condition name: `featur`
  --> $DIR/diagnotics.rs:9:7
   |
LL | #[cfg(featur)]
   |       ^^^^^^ help: there is a config with a similar name: `feature`
   |
   = help: expected values for `feature` are: `foo`
   = note: see <https://doc.rust-lang.org/nightly/rustc/check-cfg/cargo-specifics.html> for more information about checking conditional configuration
   = note: `#[warn(unexpected_cfgs)]` on by default

warning: unexpected `cfg` condition name: `featur`
  --> $DIR/diagnotics.rs:13:7
   |
LL | #[cfg(featur = "foo")]
   |       ^^^^^^^^^^^^^^
   |
   = note: see <https://doc.rust-lang.org/nightly/rustc/check-cfg/cargo-specifics.html> for more information about checking conditional configuration
help: there is a config with a similar name and value
   |
LL | #[cfg(feature = "foo")]
   |             +

warning: unexpected `cfg` condition name: `featur`
  --> $DIR/diagnotics.rs:17:7
   |
LL | #[cfg(featur = "fo")]
   |       ^^^^^^^^^^^^^
   |
   = help: expected values for `feature` are: `foo`
   = note: see <https://doc.rust-lang.org/nightly/rustc/check-cfg/cargo-specifics.html> for more information about checking conditional configuration
help: there is a config with a similar name and different values
   |
LL - #[cfg(featur = "fo")]
LL + #[cfg(feature = "foo")]
   |

warning: unexpected `cfg` condition name: `no_value`
  --> $DIR/diagnotics.rs:24:7
   |
LL | #[cfg(no_value)]
   |       ^^^^^^^^ help: there is a config with a similar name: `no_values`
   |
   = help: consider using a Cargo feature instead
   = help: or consider adding in `Cargo.toml` the `check-cfg` lint config for the lint:
            [lints.rust]
            unexpected_cfgs = { level = "warn", check-cfg = ['cfg(no_value)'] }
   = help: or consider adding `println!("cargo::rustc-check-cfg=cfg(no_value)");` to the top of the `build.rs`
   = note: see <https://doc.rust-lang.org/nightly/rustc/check-cfg/cargo-specifics.html> for more information about checking conditional configuration

warning: unexpected `cfg` condition name: `no_value`
  --> $DIR/diagnotics.rs:28:7
   |
LL | #[cfg(no_value = "foo")]
   |       ^^^^^^^^^^^^^^^^
   |
   = help: consider using a Cargo feature instead
   = help: or consider adding in `Cargo.toml` the `check-cfg` lint config for the lint:
            [lints.rust]
            unexpected_cfgs = { level = "warn", check-cfg = ['cfg(no_value, values("foo"))'] }
   = help: or consider adding `println!("cargo::rustc-check-cfg=cfg(no_value, values(\"foo\"))");` to the top of the `build.rs`
   = note: see <https://doc.rust-lang.org/nightly/rustc/check-cfg/cargo-specifics.html> for more information about checking conditional configuration
help: there is a config with a similar name and no value
   |
LL - #[cfg(no_value = "foo")]
LL + #[cfg(no_values)]
   |

warning: unexpected `cfg` condition value: `bar`
  --> $DIR/diagnotics.rs:32:7
   |
LL | #[cfg(no_values = "bar")]
   |       ^^^^^^^^^--------
   |                |
   |                help: remove the value
   |
   = note: no expected value for `no_values`
   = help: consider using a Cargo feature instead
   = help: or consider adding in `Cargo.toml` the `check-cfg` lint config for the lint:
            [lints.rust]
            unexpected_cfgs = { level = "warn", check-cfg = ['cfg(no_values, values("bar"))'] }
   = help: or consider adding `println!("cargo::rustc-check-cfg=cfg(no_values, values(\"bar\"))");` to the top of the `build.rs`
   = note: see <https://doc.rust-lang.org/nightly/rustc/check-cfg/cargo-specifics.html> for more information about checking conditional configuration

warning: unexpected `cfg` condition value: `quote"`
  --> $DIR/diagnotics.rs:36:7
   |
LL | #[cfg(quote = "quote\"")]
   |       ^^^^^^^^---------
   |               |
   |               help: there is a expected value with a similar name: `"quote"`
   |
   = note: expected values for `quote` are: `quote`
   = help: consider using a Cargo feature instead
   = help: or consider adding in `Cargo.toml` the `check-cfg` lint config for the lint:
            [lints.rust]
            unexpected_cfgs = { level = "warn", check-cfg = ['cfg(quote, values("quote\""))'] }
   = help: or consider adding `println!("cargo::rustc-check-cfg=cfg(quote, values(\"quote\\\"\"))");` to the top of the `build.rs`
   = note: see <https://doc.rust-lang.org/nightly/rustc/check-cfg/cargo-specifics.html> for more information about checking conditional configuration

warning: 7 warnings emitted

"##;
    let options = flags(DIAGNOTICS_FLAGS, &[], true);
    assert_eq!(ui_cfg("", "diagnotics.rs", "edition:2015 cfg:cargo", DIAGNOTICS, &options), expected);
}

#[test]
fn unexpected_cfg_name() {
    let source = r#"// Check warning for unexpected configuration name
//
//@ check-pass
//@ no-auto-check-cfg
//@ compile-flags: --check-cfg=cfg()

#[cfg(widnows)]
//~^ WARNING unexpected `cfg` condition name
pub fn f() {}

#[cfg(test)]
//~^ WARNING unexpected `cfg` condition name

#[cfg(windows)]
pub fn g() {}

pub fn main() {}
"#;
    let expected = r#"warning: unexpected `cfg` condition name: `widnows`
  --> $DIR/unexpected-cfg-name.rs:7:7
   |
LL | #[cfg(widnows)]
   |       ^^^^^^^ help: there is a config with a similar name: `windows`
   |
   = help: to expect this configuration use `--check-cfg=cfg(widnows)`
   = note: see <https://doc.rust-lang.org/nightly/rustc/check-cfg.html> for more information about checking conditional configuration
   = note: `#[warn(unexpected_cfgs)]` on by default

warning: unexpected `cfg` condition name: `test`
  --> $DIR/unexpected-cfg-name.rs:11:7
   |
LL | #[cfg(test)]
   |       ^^^^
   |
   = help: to expect this configuration use `--check-cfg=cfg(test)`
   = note: see <https://doc.rust-lang.org/nightly/rustc/check-cfg.html> for more information about checking conditional configuration

warning: 2 warnings emitted

"#;
    let options = flags(&["cfg()"], &[], false);
    assert_eq!(ui_cfg("", "unexpected-cfg-name.rs", "edition:2015", source, &options), expected);
}

#[test]
fn unexpected_cfg_value() {
    let source = r#"// Check for unexpected configuration value in the code.
//
//@ check-pass
//@ no-auto-check-cfg
//@ compile-flags: --cfg=feature="rand"
//@ compile-flags: --check-cfg=cfg(feature,values("serde","full"))

#[cfg(feature = "sedre")]
//~^ WARNING unexpected `cfg` condition value
pub fn f() {}

#[cfg(feature = "serde")]
pub fn g() {}

#[cfg(feature = "rand")]
//~^ WARNING unexpected `cfg` condition value
pub fn h() {}

pub fn main() {}
"#;
    let expected = r#"warning: unexpected `cfg` condition value: `sedre`
  --> $DIR/unexpected-cfg-value.rs:8:7
   |
LL | #[cfg(feature = "sedre")]
   |       ^^^^^^^^^^-------
   |                 |
   |                 help: there is a expected value with a similar name: `"serde"`
   |
   = note: expected values for `feature` are: `full` and `serde`
   = help: to expect this configuration use `--check-cfg=cfg(feature, values("sedre"))`
   = note: see <https://doc.rust-lang.org/nightly/rustc/check-cfg.html> for more information about checking conditional configuration
   = note: `#[warn(unexpected_cfgs)]` on by default

warning: unexpected `cfg` condition value: `rand`
  --> $DIR/unexpected-cfg-value.rs:15:7
   |
LL | #[cfg(feature = "rand")]
   |       ^^^^^^^^^^^^^^^^
   |
   = note: expected values for `feature` are: `full` and `serde`
   = help: to expect this configuration use `--check-cfg=cfg(feature, values("rand"))`
   = note: see <https://doc.rust-lang.org/nightly/rustc/check-cfg.html> for more information about checking conditional configuration

warning: 2 warnings emitted

"#;
    let options = flags(&[r#"cfg(feature,values("serde","full"))"#], &[r#"feature="rand""#], false);
    assert_eq!(ui_cfg("", "unexpected-cfg-value.rs", "edition:2015 cfg:feature=rand", source, &options), expected);
}

#[test]
fn allow_same_level() {
    let source = r#"// This test check that #[allow(unexpected_cfgs)] **doesn't work**
// when put on the same level as the #[cfg] attribute.
//
// It should work, but due to interactions between how #[cfg]s are
// expanded, the lint machinery and the check-cfg impl, we
// miss the #[allow], althrough we probably shouldn't.
//
// cf. https://github.com/rust-lang/rust/issues/124735
//
//@ check-pass
//@ no-auto-check-cfg
//@ compile-flags: --check-cfg=cfg() --cfg=unknown_but_active_cfg

#[allow(unexpected_cfgs)]
#[cfg(unknown_and_inactive_cfg)]
//~^ WARNING unexpected `cfg` condition name
fn bar() {}

#[allow(unexpected_cfgs)]
#[cfg(unknown_but_active_cfg)]
//~^ WARNING unexpected `cfg` condition name
fn bar() {}

fn main() {}
"#;
    let expected = r#"warning: unexpected `cfg` condition name: `unknown_and_inactive_cfg`
  --> $DIR/allow-same-level.rs:15:7
   |
LL | #[cfg(unknown_and_inactive_cfg)]
   |       ^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = help: to expect this configuration use `--check-cfg=cfg(unknown_and_inactive_cfg)`
   = note: see <https://doc.rust-lang.org/nightly/rustc/check-cfg.html> for more information about checking conditional configuration
   = note: `#[warn(unexpected_cfgs)]` on by default

warning: unexpected `cfg` condition name: `unknown_but_active_cfg`
  --> $DIR/allow-same-level.rs:20:7
   |
LL | #[cfg(unknown_but_active_cfg)]
   |       ^^^^^^^^^^^^^^^^^^^^^^
   |
   = help: to expect this configuration use `--check-cfg=cfg(unknown_but_active_cfg)`
   = note: see <https://doc.rust-lang.org/nightly/rustc/check-cfg.html> for more information about checking conditional configuration

warning: 2 warnings emitted

"#;
    let options = flags(&["cfg()"], &["unknown_but_active_cfg"], false);
    assert_eq!(ui_cfg("", "allow-same-level.rs", "edition:2015 cfg:unknown_but_active_cfg", source, &options), expected);
}

#[test]
fn exhaustive_names() {
    let source = r#"// Check warning for unexpected cfg
//
//@ check-pass
//@ no-auto-check-cfg
//@ compile-flags: --check-cfg=cfg()

#[cfg(unknown_key = "value")]
//~^ WARNING unexpected `cfg` condition name
pub fn f() {}

fn main() {}
"#;
    let expected = r#"warning: unexpected `cfg` condition name: `unknown_key`
  --> $DIR/exhaustive-names.rs:7:7
   |
LL | #[cfg(unknown_key = "value")]
   |       ^^^^^^^^^^^^^^^^^^^^^
   |
   = help: to expect this configuration use `--check-cfg=cfg(unknown_key, values("value"))`
   = note: see <https://doc.rust-lang.org/nightly/rustc/check-cfg.html> for more information about checking conditional configuration
   = note: `#[warn(unexpected_cfgs)]` on by default

warning: 1 warning emitted

"#;
    let options = flags(&["cfg()"], &[], false);
    assert_eq!(ui_cfg("", "exhaustive-names.rs", "edition:2015", source, &options), expected);
}

#[test]
fn cfg_value_for_cfg_name() {
    let source = r#"// #120427
// This test checks that when a single cfg has a value for user's specified name
// suggest to use `#[cfg(target_os = "linux")]` instead of `#[cfg(linux)]`
//
//@ check-pass
//@ no-auto-check-cfg
//@ compile-flags: --check-cfg=cfg()

#[cfg(linux)]
//~^ WARNING unexpected `cfg` condition name: `linux`
fn x() {}

// will not suggest if the cfg has a value
#[cfg(linux = "os-name")]
//~^ WARNING unexpected `cfg` condition name: `linux`
fn y() {}

fn main() {}
"#;
    let expected = r#"warning: unexpected `cfg` condition name: `linux`
  --> $DIR/cfg-value-for-cfg-name.rs:9:7
   |
LL | #[cfg(linux)]
   |       ^^^^^ help: found config with similar value: `target_os = "linux"`
   |
   = help: to expect this configuration use `--check-cfg=cfg(linux)`
   = note: see <https://doc.rust-lang.org/nightly/rustc/check-cfg.html> for more information about checking conditional configuration
   = note: `#[warn(unexpected_cfgs)]` on by default

warning: unexpected `cfg` condition name: `linux`
  --> $DIR/cfg-value-for-cfg-name.rs:14:7
   |
LL | #[cfg(linux = "os-name")]
   |       ^^^^^^^^^^^^^^^^^
   |
   = help: to expect this configuration use `--check-cfg=cfg(linux, values("os-name"))`
   = note: see <https://doc.rust-lang.org/nightly/rustc/check-cfg.html> for more information about checking conditional configuration

warning: 2 warnings emitted

"#;
    let options = flags(&["cfg()"], &[], false);
    assert_eq!(ui_cfg("", "cfg-value-for-cfg-name.rs", "edition:2015", source, &options), expected);
}

#[test]
fn wrong_version_syntax() {
    let source = r#"// Check warning for wrong `cfg(version("1.27"))` syntax
//
//@ check-pass
//@ no-auto-check-cfg
//@ compile-flags: --check-cfg=cfg()
//@ run-rustfix

#![feature(cfg_version)]

#[cfg(not(version = "1.48.0"))]
//~^ WARNING unexpected `cfg` condition name: `version`
pub fn g() {}

pub fn main() {}
"#;
    let expected = r#"warning: unexpected `cfg` condition name: `version`
  --> $DIR/wrong-version-syntax.rs:10:11
   |
LL | #[cfg(not(version = "1.48.0"))]
   |           ^^^^^^^^^^^^^^^^^^
   |
   = help: to expect this configuration use `--check-cfg=cfg(version, values("1.48.0"))`
   = note: see <https://doc.rust-lang.org/nightly/rustc/check-cfg.html> for more information about checking conditional configuration
   = note: `#[warn(unexpected_cfgs)]` on by default
help: there is a similar config predicate: `version("..")`
   |
LL - #[cfg(not(version = "1.48.0"))]
LL + #[cfg(not(version("1.48.0")))]
   |

warning: 1 warning emitted

"#;
    let options = flags(&["cfg()"], &[], false);
    assert_eq!(ui_cfg("", "wrong-version-syntax.rs", "edition:2015", source, &options), expected);
}

#[test]
fn no_expected_values_simple() {
    let source = r#"// Check that we detect unexpected value when none are allowed
//
//@ check-pass
//@ no-auto-check-cfg
//@ revisions: simple mixed empty
//@ compile-flags: --check-cfg=cfg(values,simple,mixed,empty)
//@ [simple]compile-flags: --check-cfg=cfg(test) --check-cfg=cfg(feature)
//@ [mixed]compile-flags: --check-cfg=cfg(test,feature)
//@ [empty]compile-flags: --check-cfg=cfg(test,feature,values(none()))

#[cfg(feature = "foo")]
//~^ WARNING unexpected `cfg` condition value
fn do_foo() {}

#[cfg(test = "foo")]
//~^ WARNING unexpected `cfg` condition value
fn do_foo() {}

fn main() {}
"#;
    let expected = r#"warning: unexpected `cfg` condition value: `foo`
  --> $DIR/no-expected-values.rs:11:7
   |
LL | #[cfg(feature = "foo")]
   |       ^^^^^^^--------
   |              |
   |              help: remove the value
   |
   = note: no expected value for `feature`
   = help: to expect this configuration use `--check-cfg=cfg(feature, values("foo"))`
   = note: see <https://doc.rust-lang.org/nightly/rustc/check-cfg.html> for more information about checking conditional configuration
   = note: `#[warn(unexpected_cfgs)]` on by default

warning: unexpected `cfg` condition value: `foo`
  --> $DIR/no-expected-values.rs:15:7
   |
LL | #[cfg(test = "foo")]
   |       ^^^^--------
   |           |
   |           help: remove the value
   |
   = note: no expected value for `test`
   = help: to expect this configuration use `--check-cfg=cfg(test, values("foo"))`
   = note: see <https://doc.rust-lang.org/nightly/rustc/check-cfg.html> for more information about checking conditional configuration

warning: 2 warnings emitted

"#;
    let options = flags(&["cfg(values,simple,mixed,empty)", "cfg(test)", "cfg(feature)"], &[], false);
    assert_eq!(ui_cfg("", "no-expected-values.rs", "edition:2015 cfg:simple", source, &options), expected);
}

#[test]
fn cargo_feature_none() {
    let source = r#"// This test checks that when no features are passed by Cargo we
// suggest adding some in the Cargo.toml instead of vomitting a
// list of all the expected names
//
//@ check-pass
//@ no-auto-check-cfg
//@ revisions: some none
//@ rustc-env:CARGO_CRATE_NAME=foo
//@ compile-flags: --check-cfg=cfg(docsrs,test)
//@ [none]compile-flags: --check-cfg=cfg(feature,values())
//@ [some]compile-flags: --check-cfg=cfg(feature,values("bitcode"))
//@ [some]compile-flags: --check-cfg=cfg(CONFIG_NVME,values("y"))
//@ dont-require-annotations: HELP

#[cfg(feature = "serde")]
//~^ WARNING unexpected `cfg` condition value
fn ser() {}

#[cfg(feature)]
//~^ WARNING unexpected `cfg` condition value
fn feat() {}

#[cfg(tokio_unstable)]
//~^ WARNING unexpected `cfg` condition name
fn tokio() {}

#[cfg(CONFIG_NVME = "m")]
//[none]~^ WARNING unexpected `cfg` condition name
//[some]~^^ WARNING unexpected `cfg` condition value
//[none]~| HELP Cargo.toml
fn tokio() {}

fn main() {}
"#;
    let expected = r#"warning: unexpected `cfg` condition value: `serde`
  --> $DIR/cargo-feature.rs:15:7
   |
LL | #[cfg(feature = "serde")]
   |       ^^^^^^^^^^^^^^^^^ help: remove the condition
   |
   = note: no expected values for `feature`
   = help: consider adding `serde` as a feature in `Cargo.toml`
   = note: see <https://doc.rust-lang.org/nightly/rustc/check-cfg/cargo-specifics.html> for more information about checking conditional configuration
   = note: `#[warn(unexpected_cfgs)]` on by default

warning: unexpected `cfg` condition value: (none)
  --> $DIR/cargo-feature.rs:19:7
   |
LL | #[cfg(feature)]
   |       ^^^^^^^ help: remove the condition
   |
   = note: no expected values for `feature`
   = help: consider defining some features in `Cargo.toml`
   = note: see <https://doc.rust-lang.org/nightly/rustc/check-cfg/cargo-specifics.html> for more information about checking conditional configuration

warning: unexpected `cfg` condition name: `tokio_unstable`
  --> $DIR/cargo-feature.rs:23:7
   |
LL | #[cfg(tokio_unstable)]
   |       ^^^^^^^^^^^^^^
   |
   = help: expected names are: `docsrs`, `feature`, and `test` and 33 more
   = help: consider using a Cargo feature instead
   = help: or consider adding in `Cargo.toml` the `check-cfg` lint config for the lint:
            [lints.rust]
            unexpected_cfgs = { level = "warn", check-cfg = ['cfg(tokio_unstable)'] }
   = help: or consider adding `println!("cargo::rustc-check-cfg=cfg(tokio_unstable)");` to the top of the `build.rs`
   = note: see <https://doc.rust-lang.org/nightly/rustc/check-cfg/cargo-specifics.html> for more information about checking conditional configuration

warning: unexpected `cfg` condition name: `CONFIG_NVME`
  --> $DIR/cargo-feature.rs:27:7
   |
LL | #[cfg(CONFIG_NVME = "m")]
   |       ^^^^^^^^^^^^^^^^^
   |
   = help: consider using a Cargo feature instead
   = help: or consider adding in `Cargo.toml` the `check-cfg` lint config for the lint:
            [lints.rust]
            unexpected_cfgs = { level = "warn", check-cfg = ['cfg(CONFIG_NVME, values("m"))'] }
   = help: or consider adding `println!("cargo::rustc-check-cfg=cfg(CONFIG_NVME, values(\"m\"))");` to the top of the `build.rs`
   = note: see <https://doc.rust-lang.org/nightly/rustc/check-cfg/cargo-specifics.html> for more information about checking conditional configuration

warning: 4 warnings emitted

"#;
    let options = flags(&["cfg(docsrs,test)", "cfg(feature,values())"], &[], true);
    assert_eq!(ui_cfg("", "cargo-feature.rs", "edition:2015 cfg:none", source, &options), expected);
}

/// `auxiliary/cfg_macro.rs`, another crate's macros that read cfgs.
const CFG_MACRO: &str = r#"//- /auxiliary/cfg_macro.rs crate:cfg_macro edition:2015 library
// Inspired by https://github.com/rust-lang/cargo/issues/14775

pub fn my_lib_func() {}

#[macro_export]
macro_rules! my_lib_macro {
    () => {
        #[cfg(my_lib_cfg)]
        $crate::my_lib_func()
    };
}

#[macro_export]
macro_rules! my_lib_macro_value {
    () => {
        #[cfg(panic = "UNEXPECTED_VALUE")]
        $crate::my_lib_func()
    };
}

#[macro_export]
macro_rules! my_lib_macro_feature {
    () => {
        #[cfg(feature = "UNEXPECTED_FEATURE")]
        $crate::my_lib_func()
    };
}
"#;

const REPORT_IN_EXTERNAL_MACROS: &str = r#"// This test checks that we emit the `unexpected_cfgs` lint even in code
// coming from an external macro.

//@ check-pass
//@ no-auto-check-cfg
//@ revisions: cargo rustc
//@ [rustc]unset-rustc-env:CARGO_CRATE_NAME
//@ [cargo]rustc-env:CARGO_CRATE_NAME=foo
//@ aux-crate: cfg_macro=cfg_macro.rs
//@ compile-flags: --check-cfg=cfg(feature,values())

fn main() {
    cfg_macro::my_lib_macro!();
    //~^ WARNING unexpected `cfg` condition name

    cfg_macro::my_lib_macro_value!();
    //~^ WARNING unexpected `cfg` condition value

    cfg_macro::my_lib_macro_feature!();
    //~^ WARNING unexpected `cfg` condition value
}
"#;

#[test]
fn report_in_external_macros_rustc() {
    let expected = r#"warning: unexpected `cfg` condition name: `my_lib_cfg`
  --> $DIR/report-in-external-macros.rs:13:5
   |
LL |     cfg_macro::my_lib_macro!();
   |     ^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = help: expected names are: `feature` and 33 more
   = note: using a cfg inside a macro will use the cfgs from the destination crate and not the ones from the defining crate
   = help: try referring to `cfg_macro::my_lib_macro` crate for guidance on how handle this unexpected cfg
   = help: to expect this configuration use `--check-cfg=cfg(my_lib_cfg)`
   = note: see <https://doc.rust-lang.org/nightly/rustc/check-cfg.html> for more information about checking conditional configuration
   = note: `#[warn(unexpected_cfgs)]` on by default
   = note: this warning originates in the macro `cfg_macro::my_lib_macro` (in Nightly builds, run with -Z macro-backtrace for more info)

warning: unexpected `cfg` condition value: `UNEXPECTED_VALUE`
  --> $DIR/report-in-external-macros.rs:16:5
   |
LL |     cfg_macro::my_lib_macro_value!();
   |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = note: expected values for `panic` are: `abort`, `immediate-abort`, and `unwind`
   = note: using a cfg inside a macro will use the cfgs from the destination crate and not the ones from the defining crate
   = help: try referring to `cfg_macro::my_lib_macro_value` crate for guidance on how handle this unexpected cfg
   = note: see <https://doc.rust-lang.org/nightly/rustc/check-cfg.html> for more information about checking conditional configuration
   = note: this warning originates in the macro `cfg_macro::my_lib_macro_value` (in Nightly builds, run with -Z macro-backtrace for more info)

warning: unexpected `cfg` condition value: `UNEXPECTED_FEATURE`
  --> $DIR/report-in-external-macros.rs:19:5
   |
LL |     cfg_macro::my_lib_macro_feature!();
   |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = note: no expected values for `feature`
   = help: to expect this configuration use `--check-cfg=cfg(feature, values("UNEXPECTED_FEATURE"))`
   = note: using a cfg inside a macro will use the cfgs from the destination crate and not the ones from the defining crate
   = help: try referring to `cfg_macro::my_lib_macro_feature` crate for guidance on how handle this unexpected cfg
   = note: see <https://doc.rust-lang.org/nightly/rustc/check-cfg.html> for more information about checking conditional configuration
   = note: this warning originates in the macro `cfg_macro::my_lib_macro_feature` (in Nightly builds, run with -Z macro-backtrace for more info)

warning: 3 warnings emitted

"#;
    let options = flags(&["cfg(feature,values())"], &[], false);
    let header = "edition:2015 deps:cfg_macro cfg:rustc";
    assert_eq!(ui_cfg(CFG_MACRO, "report-in-external-macros.rs", header, REPORT_IN_EXTERNAL_MACROS, &options), expected);
}

#[test]
fn report_in_external_macros_cargo() {
    let expected = r#"warning: unexpected `cfg` condition name: `my_lib_cfg`
  --> $DIR/report-in-external-macros.rs:13:5
   |
LL |     cfg_macro::my_lib_macro!();
   |     ^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = help: expected names are: `feature` and 33 more
   = note: using a cfg inside a macro will use the cfgs from the destination crate and not the ones from the defining crate
   = help: try referring to `cfg_macro::my_lib_macro` crate for guidance on how handle this unexpected cfg
   = note: see <https://doc.rust-lang.org/nightly/rustc/check-cfg/cargo-specifics.html> for more information about checking conditional configuration
   = note: `#[warn(unexpected_cfgs)]` on by default
   = note: this warning originates in the macro `cfg_macro::my_lib_macro` (in Nightly builds, run with -Z macro-backtrace for more info)

warning: unexpected `cfg` condition value: `UNEXPECTED_VALUE`
  --> $DIR/report-in-external-macros.rs:16:5
   |
LL |     cfg_macro::my_lib_macro_value!();
   |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = note: expected values for `panic` are: `abort`, `immediate-abort`, and `unwind`
   = note: using a cfg inside a macro will use the cfgs from the destination crate and not the ones from the defining crate
   = help: try referring to `cfg_macro::my_lib_macro_value` crate for guidance on how handle this unexpected cfg
   = note: see <https://doc.rust-lang.org/nightly/rustc/check-cfg/cargo-specifics.html> for more information about checking conditional configuration
   = note: this warning originates in the macro `cfg_macro::my_lib_macro_value` (in Nightly builds, run with -Z macro-backtrace for more info)

warning: unexpected `cfg` condition value: `UNEXPECTED_FEATURE`
  --> $DIR/report-in-external-macros.rs:19:5
   |
LL |     cfg_macro::my_lib_macro_feature!();
   |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = note: no expected values for `feature`
   = note: using a cfg inside a macro will use the cfgs from the destination crate and not the ones from the defining crate
   = help: try referring to `cfg_macro::my_lib_macro_feature` crate for guidance on how handle this unexpected cfg
   = note: see <https://doc.rust-lang.org/nightly/rustc/check-cfg/cargo-specifics.html> for more information about checking conditional configuration
   = note: this warning originates in the macro `cfg_macro::my_lib_macro_feature` (in Nightly builds, run with -Z macro-backtrace for more info)

warning: 3 warnings emitted

"#;
    let options = flags(&["cfg(feature,values())"], &[], true);
    let header = "edition:2015 deps:cfg_macro cfg:cargo";
    assert_eq!(ui_cfg(CFG_MACRO, "report-in-external-macros.rs", header, REPORT_IN_EXTERNAL_MACROS, &options), expected);
}

/// What `cfg-select.rs` has of core: `cfg_select!` in the prelude.
const STD: &str = r#"//- /std.rs crate:std edition:2015 library
pub mod prelude {
    pub mod v1 {
        #[rustc_builtin_macro]
        pub macro cfg_select($($tt:tt)*) {}
    }
    pub mod rust_2015 { pub use super::v1::*; }
    pub mod rust_2018 { pub use super::v1::*; }
    pub mod rust_2021 { pub use super::v1::*; }
    pub mod rust_2024 { pub use super::v1::*; }
}
"#;

#[test]
fn cfg_select() {
    let source = r#"//@ check-pass

#![crate_type = "lib"]

cfg_select! {
    false => {}
    invalid_cfg1 => {}
    //~^ WARN unexpected `cfg` condition name
    _ => {}
}

cfg_select! {
    invalid_cfg2 => {}
    //~^ WARN unexpected `cfg` condition name
    false => {}
    _ => {}
}
"#;
    let expected = r#"warning: unexpected `cfg` condition name: `invalid_cfg1`
  --> $DIR/cfg-select.rs:7:5
   |
LL |     invalid_cfg1 => {}
   |     ^^^^^^^^^^^^
   |
   = help: expected names are: `FALSE` and `test` and 33 more
   = help: to expect this configuration use `--check-cfg=cfg(invalid_cfg1)`
   = note: see <https://doc.rust-lang.org/nightly/rustc/check-cfg.html> for more information about checking conditional configuration
   = note: `#[warn(unexpected_cfgs)]` on by default

warning: unexpected `cfg` condition name: `invalid_cfg2`
  --> $DIR/cfg-select.rs:13:5
   |
LL |     invalid_cfg2 => {}
   |     ^^^^^^^^^^^^
   |
   = help: to expect this configuration use `--check-cfg=cfg(invalid_cfg2)`
   = note: see <https://doc.rust-lang.org/nightly/rustc/check-cfg.html> for more information about checking conditional configuration

warning: 2 warnings emitted

"#;
    let options = flags(&[AUTO], &[], false);
    assert_eq!(ui_cfg(STD, "cfg-select.rs", "edition:2015 deps:std", source, &options), expected);
}

#[test]
fn nothing_is_checked_without_a_check_cfg() {
    let source = "#[cfg(widnows)]\nfn f() {}\n\nfn main() {}\n";
    assert_eq!(ui_cfg("", "none.rs", "edition:2015", source, &flags(&[], &[], false)), "");
}

#[test]
fn every_name_of_a_predicate_is_checked_and_a_false_cfg_stops_the_rest() {
    let source = r#"#[cfg(any(all(not(a)), b, target(os = "nope")))]
#[cfg(c)]
#[cfg(d)]
fn f() {}

#[cfg_attr(e, cfg(f))]
#[cfg_attr(all(), cfg(g))]
fn h() {}

#[cfg(not(x, y))]
fn i() {}

fn main() {
    let _ = cfg!(z);
}
"#;
    let options = flags(&["cfg()"], &[], false);
    let fixture = format!("//- minicore: cfg\n//- /lib.rs crate:main edition:2021\n{source}");
    let options = crate::tests::compiletest(crate::tests::Mode::Other, &options);
    let checked = crate::tests::checked_with(&fixture, "", &options);
    let lints: Vec<String> = checked
        .diags
        .iter()
        .filter(|it| it.lint_name() == Some("unexpected_cfgs"))
        .map(|it| it.message.clone())
        .collect();
    assert_eq!(
        lints,
        [
            "unexpected `cfg` condition name: `a`",
            "unexpected `cfg` condition name: `b`",
            "unexpected `cfg` condition value: `nope`",
            "unexpected `cfg` condition name: `c`",
            "unexpected `cfg` condition name: `e`",
            "unexpected `cfg` condition name: `g`",
            "unexpected `cfg` condition name: `z`",
        ]
    );
}

#[test]
fn raw_names_are_shown_as_their_edition_writes_them() {
    let source = "#[cfg(tru)]\nfn f() {}\n\n#[cfg(r#false)]\nfn g() {}\n\n#[cfg(r#raw)]\nfn h() {}\n\nfn main() {}\n";
    let options = flags(&["cfg(r#true,r#async,edition2015,edition2021)"], &[], false);
    let fixture = format!("//- /lib.rs crate:main edition:2021\n{source}");
    let options = crate::tests::compiletest(crate::tests::Mode::Other, &options);
    let checked = crate::tests::checked_with(&fixture, "", &options);
    let lints: Vec<String> = checked
        .diags
        .iter()
        .filter(|it| it.lint_name() == Some("unexpected_cfgs"))
        .map(|it| it.message.clone())
        .collect();
    assert_eq!(
        lints,
        [
            "unexpected `cfg` condition name: `tru`",
            "unexpected `cfg` condition name: `r#false`",
            "unexpected `cfg` condition name: `raw`",
        ]
    );
    let helps: Vec<&str> =
        checked.diags[1].children.iter().map(|it| it.message.as_str()).collect();
    assert_eq!(helps[0], "expected names are: `r#async`, `edition2015`, `edition2021`, and `r#true` and 33 more");
    assert_eq!(checked.diags[0].suggestions[0].substitutions[0][0].1, "r#true");
}
