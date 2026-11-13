//! What this blaze has of the features some of blaze's lints tell code to move to. A lint that
//! says "write it with X" warns by default only once X is in the tree: until then it is
//! registered, documented and `allow` by default, and its message says X is planned. Each
//! switch is one `const`, read by this crate's table (the lint's default level) and by
//! blaze-lints (the same default, and which text its findings carry), so the two cannot
//! disagree. docs/lints.md's table of defaults is held to these by blaze-lints' tests. A
//! workspace can wait for a switch too: a line of `[workspace.metadata.blaze.lints-when-ready]`
//! sets its lint's level once the lint's switch is `true` and nothing until then (blaze-units'
//! `workspace_lints`).

use super::Level;

/// Whether compile-time reflection can replace a derive or an attribute proc macro: phase 2 of
/// docs/v2/plan/design/comptime.md (`for const f in fields_of::<T>()`, `value.[f]`,
/// `for<F: Field<T>>` bounds, `fn_info`, a blanket impl over reflected structs) built and its
/// tests passing under `blaze test`. Today phase 1 (`core::mem::type_info` in blaze's const
/// evaluator) is in the tree and phase 2 is behind the opt-in `#![feature(blaze_comptime)]`,
/// not complete. Turning this to `true` makes `blaze::proc_macro_derive` and
/// `blaze::proc_macro_attribute` warn by default and their help name the replacement as
/// something to do now.
pub const REFLECTION_REPLACES_MACROS: bool = false;

/// Whether a build script can take capabilities in place of std's ambient IO: blaze's
/// capability standard library (docs/v2/plan/design/std.md: `Fs`, `Env`, `Process`, `Net`,
/// `Clock`) in the tree, with an entry point a build script is handed them through. Today
/// neither is. Turning this to `true` makes `blaze::build_script_ambient` warn by default and
/// its help name the capability as something to take now.
pub const CAPABILITIES_FOR_BUILD_SCRIPTS: bool = false;

/// Whether blaze's capability standard library (bstd, `crates/blaze-std`,
/// docs/v2/plan/design/std.md) is in the tree, compiles on a stage-1 blaze and has an API the
/// workspace can port to: steps S1 and S2 of that plan. Today `crates/blaze-std` is in the tree
/// and builds, and its tests run, under a blaze built from this tree (`blaze build -p blaze-std`,
/// `blaze test -p blaze-std`; arm64 Linux, 2026-11-12), but it is not an API the workspace can
/// port to: cargo still builds blaze's crates, and rustc refuses bstd's blaze-only syntax
/// (decision D25 of docs/v2/plan/decisions.json). Turning this to `true` makes `blaze::std_use` warn in every crate of a
/// workspace whose `lints-when-ready` table has `std_use = "warn"`, and its findings name what to
/// write as something to do now. The lint's own default stays `allow`: another workspace's
/// crates ask for it with `#![blaze::capabilities]`. blaze's own workspace does not wait: its
/// `lints-ratchet` denies the lint ahead of this switch (Andrew's decision of 2026-11-11), each
/// crate not yet ported held at `warn` by its line in the lint's baseline.
pub const BSTD_IN_TREE: bool = false;

/// Whether a package of the latest blaze edition builds wherever this blaze runs: bstd ships
/// with blaze and is given to every crate of the edition (docs/v2/plan/design/std.md, row
/// S5d), so `edition = "blaze-2026.11"` is all a package writes. Today a package of the
/// edition names bstd as a dependency by path, which only a checkout of blaze has. Turning
/// this to `true` makes `blaze::not_latest_edition` warn by default, and is turned with
/// `blaze_new::BSTD_SHIPS`, which makes the edition what `blaze new` writes: telling every
/// package to move to an edition it cannot build on would be a nag, not help.
pub const EDITION_SHIPS: bool = false;

/// Each lint that has a switch here, by its name after `blaze::`, with the switch: what the
/// table `[workspace.metadata.blaze.lints-when-ready]` reads ([`switch_of`]).
pub const SWITCHES: &[(&str, bool)] = &[
    ("proc_macro_derive", REFLECTION_REPLACES_MACROS),
    ("proc_macro_attribute", REFLECTION_REPLACES_MACROS),
    ("build_script_ambient", CAPABILITIES_FOR_BUILD_SCRIPTS),
    ("std_use", BSTD_IN_TREE),
    ("not_latest_edition", EDITION_SHIPS),
];

/// Whether what `blaze::{lint}` tells code to move to is in this blaze; `None` for a lint with
/// no switch, whose advice needs nothing new.
pub fn switch_of(lint: &str) -> Option<bool> {
    SWITCHES.iter().find(|(name, _)| *name == lint).map(|(_, ready)| *ready)
}

/// The default level of a lint whose advice is `ready` or not: `warn` once what it names is in
/// the tree, `allow` until then.
pub const fn level(ready: bool) -> Level {
    if ready { Level::Warn } else { Level::Allow }
}
