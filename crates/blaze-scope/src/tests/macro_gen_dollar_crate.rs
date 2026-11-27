//! token-mark-3: macro-generating-macro `$crate` preservation across crates (`cfg_aliases!`,
//! `nix`/`quinn`/`quinn-udp` `build.rs` pattern) and refusal when a single generated macro
//! definition mixes `$crate` from two different crates.

use std::format;
use std::string::String;
use std::vec::Vec;

use blaze_id::ItemKey;
use blaze_surface::{Namespace, PathKind};

use super::entry::input;
use crate::crate_scope::{Form, Ready, resolve_by_records_in};
use crate::graph::Sysroot;
use crate::items_record::{self, ItemsRecord};
use crate::names::Written;
use crate::names_record::{self, NamesRecord};
use crate::resolve::{ExpandNote, NamesNote, SourceTexts, aligned, resolve_crates};
use crate::scope_record::{DepRecord, Verified};
use crate::task::Serial;

/// Minimal `core` stub providing `stringify!` and a `println!`-like macro.
const CORE: &str = "#![no_core]
#[macro_export]
#[rustc_builtin_macro]
macro_rules! stringify { () => {} }

#[macro_export]
macro_rules! emit_line {
    ($fmt:expr, $arg:expr) => {{
        let _ = ($fmt, $arg);
    }};
}
";

/// Mirrors `cfg_aliases::cfg_aliases!` (`nix`/`quinn` `build.rs`):
/// - `@with_dollar[$d:tt]` emits a local `macro_rules! __cfg_aliases_matcher__` whose arms call
///   `$crate::cfg_aliases!(@parser ...)` and `$crate::cfg_aliases!(@cfg_is_set $e)`.
/// - `@cfg_is_set` expands to a block using `stringify!`, and top-level calls emit `emit_line!`
///   inside an `if` block (creating `__blaze_block_x...` modules in `blaze-scope`).
const CFG_ALIASES: &str = "#![no_core]
#[macro_export]
macro_rules! cfg_aliases {
    (@with_dollar[$d:tt] $($alias:ident : { $($config:tt)* }),* $(,)?) => {
        macro_rules! __cfg_aliases_matcher__ {
            $(
                ($alias) => { $crate::cfg_aliases!(@parser $($config)*) };
            )*
            ($e:ident) => { $crate::cfg_aliases!(@cfg_is_set $e) };
        }
        $(
            core::emit_line!(\"cargo:rustc-check-cfg=cfg({})\", core::stringify!($alias));
            if $crate::cfg_aliases!(@parser $($config)*) {
                core::emit_line!(\"cargo:rustc-cfg={}\", core::stringify!($alias));
            }
        )*
    };
    (@parser target_os = $v:literal) => { true };
    (@parser $alias:ident) => { __cfg_aliases_matcher__!($alias) };
    (@cfg_is_set $e:ident) => {{
        let _name = core::stringify!($e);
        true
    }};
    ($($tt:tt)*) => {
        $crate::cfg_aliases!(@with_dollar[$] $($tt)*);
    };
}
";

const APP: &str = "#![no_core]
fn main() {
    cfg_aliases::cfg_aliases! {
        linux: { target_os = \"linux\" },
        my_alias: { linux },
    }
}
";

const DEP_A: &str = "#![no_core]
pub struct TypeA;

#[macro_export]
macro_rules! gen_exported {
    () => {
        #[macro_export]
        macro_rules! from_dep_a {
            () => {
                pub type Reexported = $crate::TypeA;
            };
        }
    };
}

#[macro_export]
macro_rules! mix_with_arg {
    ($($body:tt)*) => {
        macro_rules! mixed_m {
            () => {
                pub type A = $crate::TypeA;
                $($body)*
            };
        }
    };
}
";

const DEP_B: &str = "#![no_core]
pub struct TypeB;

#[macro_export]
macro_rules! pass_to_dep_a {
    () => {
        dep_a::mix_with_arg! {
            pub type B = $crate::TypeB;
        }
    };
}
";

const MID: &str = "#![no_core]
dep_a::gen_exported!();
";

const CONSUMER: &str = "#![no_core]
mid::from_dep_a!();
";

const MIX_TWO_DEPS: &str = "#![no_core]
dep_b::pass_to_dep_a!();
";

const MIX_DEP_AND_HERE: &str = "#![no_core]
pub struct LocalType;
dep_a::mix_with_arg! {
    pub type Local = $crate::LocalType;
}
";

struct WorldSources(&'static [&'static [(&'static str, &'static str)]]);

impl SourceTexts<'static> for WorldSources {
    fn read(&self, krate: usize, path: &str) -> Option<&'static str> {
        self.0.get(krate)?.iter().find(|(p, _)| *p == path).map(|(_, t)| *t)
    }
}

#[test]
fn cfg_aliases_pattern_preserves_dollar_crate_and_block_modules() {
    static FILES: WorldSources = WorldSources(&[
        &[("/core/src/lib.rs", CORE)],
        &[("/cfg_aliases/src/lib.rs", CFG_ALIASES)],
        &[("/app/src/main.rs", APP)],
    ]);
    let crates = [
        input("core", "/core/src/lib.rs", &[]),
        input("cfg_aliases", "/cfg_aliases/src/lib.rs", &[("core", 0)]),
        input("app", "/app/src/main.rs", &[("core", 0), ("cfg_aliases", 1)]),
    ];
    let bump = super::SyncFrame::leaked();
    let g = blaze_own::testing::heap().alloc();
    let names = resolve_crates(&crates, Sysroot::default(), &FILES, &Serial, bump)
        .unwrap_or_else(|e| panic!("resolve_crates: {e}"));

    let app_notes: Vec<String> = names.notes(2).iter().map(|n| format!("{n:?}")).collect();
    assert!(app_notes.is_empty(), "app must resolve with zero notes (no ForeignDollarCrate): {app_notes:?}");

    let cfg_aliases_key = names.krate(1).key;
    let app = names.krate(2);

    // `__cfg_aliases_matcher__` must be lowered with `dollar_crate == Some(cfg_aliases_key)`.
    let (matcher_key, matcher_src) = app
        .tables
        .macros
        .iter()
        .find(|(_, m)| m.name == "__cfg_aliases_matcher__")
        .map(|(&k, &m)| (k, m))
        .expect("__cfg_aliases_matcher__ must be lowered in app");
    assert_eq!(
        matcher_src.dollar_crate,
        Some(cfg_aliases_key),
        "__cfg_aliases_matcher__ must carry dollar_crate = Some(cfg_aliases)"
    );

    // Expansion block modules (`__blaze_block_x...`) must be filed into `names_record`.
    let names_bytes = aligned(&names_record::encode(app, g).expect("encode names_record"), bump);
    let nrec = NamesRecord::open(names_record::Verified::check(names_bytes).expect("verify names_record"));
    let has_exp_block = app.done.keys().any(|&m| {
        app.path_of(m).is_some_and(|p| p.contains(crate::expanded_blocks::EXPANSION_BLOCK))
    });
    assert!(has_exp_block, "app must create expansion block modules (__blaze_block_x...)");
    for &m in app.done.keys() {
        if app.path_of(m).is_some_and(|p| p.contains(crate::expanded_blocks::EXPANSION_BLOCK)) {
            assert!(nrec.scope(m).is_some(), "expansion block module {m:?} must be present in names_record");
        }
    }

    // `items_record` round-trips `dollar_crate`.
    let items_bytes = aligned(&items_record::encode_with(app, &names, g).expect("encode items_record"), bump);
    let irec = ItemsRecord::open(items_bytes, g).expect("open items_record");
    let row = irec.macro_row(matcher_key).expect("macro_row").expect("matcher row present");
    assert_eq!(row.dollar_crate, Some(cfg_aliases_key), "items_record must round-trip dollar_crate");
    let round_src = irec
        .macro_src(matcher_key, &|what| match what {
            items_record::TextOf::Expansion { call } => app.expansions.get(&call).and_then(|e| e.made.map(|m| m.text)),
            items_record::TextOf::File { index, .. } => app.files.get(usize::try_from(index).ok()?).map(|f| f.text),
        })
        .expect("macro_src")
        .expect("matcher macro_src present");
    assert_eq!(round_src.dollar_crate, Some(cfg_aliases_key), "items_record::macro_src must round-trip dollar_crate");
}

#[test]
fn exported_generated_macro_preserves_dollar_crate_across_records_and_reaches() {
    static FILES: WorldSources = WorldSources(&[
        &[("/dep_a/src/lib.rs", DEP_A)],
        &[("/mid/src/lib.rs", MID)],
        &[("/consumer/src/lib.rs", CONSUMER)],
    ]);
    // Note: `consumer` depends ONLY on `mid` (NOT directly on `dep_a`!).
    let crates = [
        input("dep_a", "/dep_a/src/lib.rs", &[]),
        input("mid", "/mid/src/lib.rs", &[("dep_a", 0)]),
        input("consumer", "/consumer/src/lib.rs", &[("mid", 1)]),
    ];
    let bump = super::SyncFrame::leaked();
    let g = blaze_own::testing::heap().alloc();
    let wait = core::time::Duration::from_secs(30);
    let (solved, _) = resolve_by_records_in(&crates, Sysroot::default(), &FILES, &Serial, &Ready::serial(wait), false, Form::InPlace, bump)
        .unwrap_or_else(|e| panic!("resolve_by_records_in: {e:?}"));

    let dep_a_key = crates[0].key;

    // Inspect `mid`'s serialized `scope_record` directly: must carry `dollar_crate == Some(dep_a)`
    // AND include `dep_a` in `reaches` so `consumer` can resolve `dep_a::TypeA`.
    let mid_bytes = aligned(&solved[1].record, bump);
    let mid_rec = DepRecord::open(Verified::check(mid_bytes).expect("verify mid scope_record"));
    let reaches: Vec<_> = mid_rec.reaches().iter().collect();
    assert!(reaches.contains(&dep_a_key), "mid's scope_record reaches must contain dep_a: {reaches:?}");
    let from_dep_a = mid_rec
        .macros()
        .filter_map(|k| mid_rec.macro_of(k))
        .find(|m| m.name == "from_dep_a")
        .expect("from_dep_a exported in mid's scope_record");
    assert_eq!(from_dep_a.dollar_crate, Some(dep_a_key), "scope_record must round-trip dollar_crate");

    // Consumer must resolve `Reexported` cleanly with zero notes and attribute `$crate` to `dep_a`.
    assert!(solved[2].names.done.values().all(|d| d.scope.errors.is_empty()), "consumer record-solve scope errors");
    let names = resolve_crates(&crates, Sysroot::default(), &FILES, &Serial, bump)
        .unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    assert!(names.notes(2).is_empty(), "consumer notes: {:?}", names.notes(2));
    let reexported = names
        .item_at(names.root(2), Namespace::Types, Written { kind: PathKind::Plain, segments: &["Reexported"] }, g)
        .unwrap_or_else(|e| panic!("consumer::Reexported must resolve: {e:?}"));
    assert_eq!(reexported.krate, 2);
    let consumer_roots: Vec<_> = names
        .krate(2)
        .expansions
        .values()
        .filter_map(|e| e.made.as_ref())
        .filter_map(|made| crate::crate_roots::column_in(&made.origins).ok())
        .flat_map(|col| (0..col.len()).filter_map(move |i| col.row(i).mark.map(|m| m.site.krate)))
        .collect();
    assert!(consumer_roots.contains(&dep_a_key), "consumer expansion of mid::from_dep_a! must attribute $crate to dep_a: {consumer_roots:?}");
}

#[test]
fn mixed_dollar_crate_in_single_generated_macro_refuses_cleanly() {
    // Case 1: Single generated `macro_rules!` mixing `$crate` from `dep_a` AND `dep_b`.
    static FILES_TWO_DEPS: WorldSources = WorldSources(&[
        &[("/dep_a/src/lib.rs", DEP_A)],
        &[("/dep_b/src/lib.rs", DEP_B)],
        &[("/app/src/lib.rs", MIX_TWO_DEPS)],
    ]);
    let crates_two = [
        input("dep_a", "/dep_a/src/lib.rs", &[]),
        input("dep_b", "/dep_b/src/lib.rs", &[("dep_a", 0)]),
        input("app", "/app/src/lib.rs", &[("dep_a", 0), ("dep_b", 1)]),
    ];
    let names_two = resolve_crates(&crates_two, Sysroot::default(), &FILES_TWO_DEPS, &Serial, super::SyncFrame::leaked())
        .unwrap_or_else(|e| panic!("resolve_crates two deps: {e}"));
    let dep_b_key = names_two.krate(1).key;
    let saw_foreign_two = names_two.notes(2).iter().any(|note| {
        matches!(note, NamesNote::Expand { note: ExpandNote::ForeignDollarCrate { krate, .. }, .. } if *krate == dep_b_key)
    });
    assert!(
        saw_foreign_two,
        "mixing `$crate` from dep_a and dep_b in one generated macro_rules! must refuse with ForeignDollarCrate(dep_b); notes: {:?}",
        names_two.notes(2)
    );
    let has_mixed_two = names_two.krate(2).tables.macros.values().any(|m| m.name == "mixed_m");
    assert!(!has_mixed_two, "refused mixed_m must not be lowered into app's macros");

    // Case 2: Single generated `macro_rules!` mixing `$crate` from `dep_a` AND unmarked `$crate` from `here`.
    static FILES_HERE: WorldSources = WorldSources(&[
        &[("/dep_a/src/lib.rs", DEP_A)],
        &[("/app/src/lib.rs", MIX_DEP_AND_HERE)],
    ]);
    let crates_here = [
        input("dep_a", "/dep_a/src/lib.rs", &[]),
        input("app", "/app/src/lib.rs", &[("dep_a", 0)]),
    ];
    let names_here = resolve_crates(&crates_here, Sysroot::default(), &FILES_HERE, &Serial, super::SyncFrame::leaked())
        .unwrap_or_else(|e| panic!("resolve_crates dep+here: {e}"));
    let dep_a_key = names_here.krate(0).key;
    let saw_foreign_here = names_here.notes(1).iter().any(|note| {
        matches!(note, NamesNote::Expand { note: ExpandNote::ForeignDollarCrate { krate, .. }, .. } if *krate == dep_a_key)
    });
    assert!(
        saw_foreign_here,
        "mixing `$crate` from dep_a and here in one generated macro_rules! must refuse with ForeignDollarCrate(dep_a); notes: {:?}",
        names_here.notes(1)
    );
    let has_mixed_here: Option<ItemKey> = names_here.krate(1).tables.macros.iter().find(|(_, m)| m.name == "mixed_m").map(|(&k, _)| k);
    assert!(has_mixed_here.is_none(), "refused mixed_m (dep+here) must not be lowered into app's macros");
}

#[test]
fn local_inner_macros_resolves_in_defining_crate_while_dollar_crate_resolves_in_foreign_crate() {
    // Regression for Codex P1: a generated `#[macro_export(local_inner_macros)] macro_rules! m`
    // in `mid` (emitted by `dep_a`) invokes a bare `helper!()` AND references `$crate::TypeA`.
    // `helper!()` MUST resolve to `mid::helper!` (emits `FromMidHelper`), NOT `dep_a::helper!`
    // (emits `FromDepAHelper`), while `$crate::TypeA` MUST resolve to `dep_a::TypeA`.
    const DEP_A_LOCAL_INNER: &str = "#![no_core]
pub struct TypeA;

#[macro_export]
macro_rules! helper {
    () => {
        pub struct FromDepAHelper;
    };
}

#[macro_export]
macro_rules! gen_local_inner {
    () => {
        #[macro_export(local_inner_macros)]
        macro_rules! m {
            () => {
                helper!();
                pub type FromA = $crate::TypeA;
            };
        }
    };
}
";
    const MID_LOCAL_INNER: &str = "#![no_core]
#[macro_export]
macro_rules! helper {
    () => {
        pub struct FromMidHelper;
    };
}

dep_a::gen_local_inner!();
";
    const CONSUMER_LOCAL_INNER: &str = "#![no_core]
mid::m!();
";
    static FILES: WorldSources = WorldSources(&[
        &[("/dep_a/src/lib.rs", DEP_A_LOCAL_INNER)],
        &[("/mid/src/lib.rs", MID_LOCAL_INNER)],
        &[("/consumer/src/lib.rs", CONSUMER_LOCAL_INNER)],
    ]);
    let crates = [
        input("dep_a", "/dep_a/src/lib.rs", &[]),
        input("mid", "/mid/src/lib.rs", &[("dep_a", 0)]),
        input("consumer", "/consumer/src/lib.rs", &[("mid", 1)]),
    ];
    let bump = super::SyncFrame::leaked();
    let g = blaze_own::testing::heap().alloc();
    let names = resolve_crates(&crates, Sysroot::default(), &FILES, &Serial, bump)
        .unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    assert!(names.notes(1).is_empty(), "mid notes: {:?}", names.notes(1));
    assert!(names.notes(2).is_empty(), "consumer notes: {:?}", names.notes(2));

    let dep_a_key = names.krate(0).key;
    let mid_key = names.krate(1).key;

    // `mid::helper!()` must have expanded (declaring `FromMidHelper`), NOT `dep_a::helper!()`.
    let from_mid = names
        .item_at(names.root(2), Namespace::Types, Written { kind: PathKind::Plain, segments: &["FromMidHelper"] }, g)
        .unwrap_or_else(|e| panic!("consumer::FromMidHelper must exist (mid::helper! ran): {e:?}"));
    assert_eq!(from_mid.krate, 2);
    let from_dep_a = names.item_at(names.root(2), Namespace::Types, Written { kind: PathKind::Plain, segments: &["FromDepAHelper"] }, g);
    assert!(from_dep_a.is_err(), "dep_a::helper! must NOT have been invoked: {from_dep_a:?}");

    // `FromA` must exist and the `$crate` root written for `$crate::TypeA` must attribute to `dep_a`
    // while the `local_inner_macros` prefix for `helper!()` must attribute to `mid`.
    let from_a = names
        .item_at(names.root(2), Namespace::Types, Written { kind: PathKind::Plain, segments: &["FromA"] }, g)
        .unwrap_or_else(|e| panic!("consumer::FromA must exist: {e:?}"));
    assert_eq!(from_a.krate, 2);

    let consumer_roots: Vec<_> = names
        .krate(2)
        .expansions
        .values()
        .filter_map(|e| e.made.as_ref())
        .filter_map(|made| crate::crate_roots::column_in(&made.origins).ok())
        .flat_map(|col| (0..col.len()).filter_map(move |i| col.row(i).mark.map(|m| m.site.krate)))
        .collect();
    assert!(consumer_roots.contains(&dep_a_key), "consumer expansion must attribute `$crate::TypeA` to dep_a: {consumer_roots:?}");
    assert!(consumer_roots.contains(&mid_key), "consumer expansion must attribute `local_inner_macros` `helper!()` to mid: {consumer_roots:?}");
}
