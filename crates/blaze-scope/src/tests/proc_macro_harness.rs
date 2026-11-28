//! D175 (docs/v2/plan/design/comptime-native.md, "The proc_macro route", blocker 2): a
//! proc-macro crate's harness static. rustc's proc_macro_harness.rs writes, for a crate whose
//! manifest says `proc-macro = true`, one `#[rustc_proc_macro_decls] static _DECLS:
//! &[proc_macro::bridge::client::Client]` listing each `#[proc_macro*]` fn of the root in
//! declaration order (the pinned library/proc_macro/src/bridge/client.rs: `Client::expand1` for
//! a derive and a function-like macro, `Client::expand2` for an attribute macro). blaze makes it
//! as the expansion of the root's harness call (task/lower.rs `Lower::proc_macro_harness`),
//! keyed by `proc_macro_decls_key`, declaring no name.

use std::format;
use std::vec::Vec;

use blaze_own::Alloc;
use blaze_surface::{Namespace, PathKind};
use blaze_unit_graph::{Implicit, Prelude, StdIs};

use super::entry::{Texts, input};
use crate::graph::{CrateId, Sysroot};
use crate::names::Written;
use crate::resolve::{CrateInput, Names, resolve_crates};
use crate::task::{
    PROC_MACRO_DECLS, PROC_MACRO_DECLS_FN, PROC_MACRO_DECLS_LEN_FN, ProcAttr, Serial,
    proc_macro_decls_fn_key, proc_macro_decls_key, proc_macro_decls_len_fn_key,
};

/// One macro of each kind, the attribute's fn named by a raw identifier, an ordinary fn between
/// them that is in no list.
const MACROS: &str = "#![no_core]
#[proc_macro_derive(Hello, attributes(hi))]
pub fn derive_hello() {}
pub fn helper() {}
#[proc_macro_attribute]
pub fn r#trace() {}
#[proc_macro]
pub fn make() {}
";

/// An ordinary crate with the same fns' text: no harness.
const PLAIN: &str = "#![no_core]
pub fn derive_hello() {}
";

static TEXTS: Texts = Texts(&[&[("/plain/lib.rs", PLAIN)], &[("/macros/lib.rs", MACROS)]]);

fn resolved(proc_macro: bool) -> (Names<'static>, [CrateInput<'static>; 2]) {
    let macros = input("macros", "/macros/lib.rs", &[]);
    let implicit = Implicit { std: StdIs::Rust, prelude: Prelude::Rust(edition::Edition::Edition2021), proc_macro, entry: false };
    let crates = [input("plain", "/plain/lib.rs", &[]), CrateInput { implicit, ..macros }];
    let names = resolve_crates(&crates, Sysroot::default(), &TEXTS, &Serial, super::SyncFrame::leaked()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    (names, crates)
}

/// The text of the item keyed `key` in crate `krate`, if it has one.
fn text_of(names: &Names<'static>, krate: usize, key: blaze_id::ItemKey) -> Option<(Option<&'static str>, &'static str)> {
    names.krate(krate).tables.texts.get(&key).map(|(t, _)| (t.name, t.text))
}

/// Rule: a proc-macro crate has the `_DECLS` static under `proc_macro_decls_key`, its list the
/// root's three macros in declaration order (`expand1` derive, `expand2` attribute, `expand1`
/// function-like; the raw name kept raw; the plain fn left out), bound to no name; an ordinary
/// crate has none.
///
/// Prints `harness of macros: <the static's text>` and
/// `test tests::proc_macro_harness::a_proc_macro_crate_lists_its_macros_in_one_unnamed_static ... ok`.
/// CONTROL: the same crate resolved with `proc_macro: false` (the cause, `Implicit::proc_macro`,
/// removed) has no such item: the test asserts it, so a harness made for every crate fails here.
#[test]
fn a_proc_macro_crate_lists_its_macros_in_one_unnamed_static() {
    let (names, crates) = resolved(true);
    let notes = || format!("notes {:?}", names.notes(1));
    let key = proc_macro_decls_key(crates[1].key);
    let (name, text) = text_of(&names, 1, key).unwrap_or_else(|| panic!("the macro crate has no `_DECLS` under its key: {}", notes()));
    assert_eq!(name, Some(PROC_MACRO_DECLS));
    assert!(text.starts_with("#[rustc_proc_macro_decls]"), "the static keeps rustc's attribute: {text}");
    assert!(text.contains("static _DECLS: &[::proc_macro::bridge::client::Client] = &["), "{text}");
    let client = "::proc_macro::bridge::client::Client::";
    let wanted = [format!("{client}expand1(crate::derive_hello)"), format!("{client}expand2(crate::r#trace)"), format!("{client}expand1(crate::make)")];
    let at: Vec<usize> = wanted.iter().map(|w| text.find(w.as_str()).unwrap_or_else(|| panic!("`{w}` in {text}"))).collect();
    assert!(at.windows(2).all(|w| w[0] < w[1]), "declaration order: {at:?} in {text}");
    assert_eq!(text.matches(client).count(), 3, "three macros, the plain fn in no list: {text}");
    // Bound to no name, as rustc's def-site `_DECLS`.
    let g = Alloc::global();
    let bound = names.item_at(names.root(1), Namespace::Values, Written { kind: PathKind::Plain, segments: &["_DECLS"] }, g);
    assert!(bound.is_err(), "`_DECLS` must not be nameable from the crate");
    // An ordinary crate has none.
    assert!(text_of(&names, 0, proc_macro_decls_key(crates[0].key)).is_none(), "an ordinary crate made a harness");
    assert!(names.krate(0).tables.texts.values().all(|(t, _)| t.name != Some(PROC_MACRO_DECLS)));
    std::println!("harness of macros: {text}");

    // CONTROL: the cause removed, the item goes.
    let (plain, crates) = resolved(false);
    assert!(text_of(&plain, 1, proc_macro_decls_key(crates[1].key)).is_none(), "a crate that is no proc-macro crate made a harness");
}

/// Macro-generated `#[proc_macro*]` exports (like `derive_more-impl`'s `create_derive!`)
/// must appear in BOTH `_DECLS` AND `CrateNames::proc_exports` in identical order.
#[test]
fn macro_expanded_proc_macro_exports_appear_in_decls_and_proc_exports() {
    const EXPANDED_MACROS: &str = r#"#![no_core]
macro_rules! create_derive {
    ($trait:ident, $fn_name:ident, $attr:ident) => {
        #[proc_macro_derive($trait, attributes($attr))]
        pub fn $fn_name() {}
    };
}
#[proc_macro]
pub fn direct_fn() {}
create_derive!(Deref, deref_derive, deref);
create_derive!(DerefMut, deref_mut_derive, deref_mut);
"#;
    static EXP_TEXTS: Texts = Texts(&[&[("/macros/lib.rs", EXPANDED_MACROS)]]);
    let macros = input("macros", "/macros/lib.rs", &[]);
    let implicit = Implicit { std: StdIs::Rust, prelude: Prelude::Rust(edition::Edition::Edition2021), proc_macro: true, entry: false };
    let crates = [CrateInput { implicit, ..macros }];
    let names = resolve_crates(&crates, Sysroot::default(), &EXP_TEXTS, &Serial, super::SyncFrame::leaked())
        .unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let key = proc_macro_decls_key(crates[0].key);
    let (_, text) = text_of(&names, 0, key).expect("_DECLS must exist for macro-expanded proc-macro crate");
    let exports = &names.krate(0).proc_exports;
    assert_eq!(exports.len(), 3, "expected 1 direct + 2 macro-expanded exports, got {exports:?}");
    assert_eq!(exports[0].macro_name, "direct_fn");
    assert_eq!(exports[0].fn_name, "direct_fn");
    let mut expanded_names: Vec<&str> = exports[1..].iter().map(|e| e.macro_name).collect();
    expanded_names.sort_unstable();
    assert_eq!(expanded_names.as_slice(), &["Deref", "DerefMut"]);
    // Every `proc_exports` entry must match its exact slot in `_DECLS`.
    let client = "::proc_macro::bridge::client::Client::";
    assert_eq!(text.matches(client).count(), 3, "_DECLS must contain all 3 entries: {text}");
    let mut prev = 0usize;
    for entry in exports.iter() {
        let needle = format!("{client}expand1(crate::{})", entry.fn_name);
        let pos = text.find(&needle).unwrap_or_else(|| panic!("`{needle}` missing from `_DECLS`: {text}"));
        assert!(pos >= prev, "`_DECLS` order must match `proc_exports` order: {text}");
        prev = pos;
    }
}

/// Real-world `zerocopy-derive 0.8.63` pattern: a proc-macro crate defines `mod derive;` and
/// `macro_rules! derive!` (shadowing the Rust prelude's `#[derive]` builtin macro) that emits
/// `#[proc_macro_derive($trait, attributes(zerocopy))]` functions, alongside direct exports and
/// cfg-gated exports. Every active export must appear in BOTH `_DECLS` and `proc_exports` in
/// identical order with its helper attributes preserved.
#[test]
fn zerocopy_derive_macro_rules_derive_shadowing_prelude_emits_proc_exports_and_decls() {
    const CORE: &str = r#"#![no_core]
pub mod macros {
    #[rustc_builtin_macro]
    pub macro derive($item:item) {}
}
pub mod prelude {
    pub mod v1 {
        pub use crate::macros::derive;
    }
    pub mod rust_2021 {
        pub use super::v1::*;
    }
}
"#;
    const ZEROCOPY_DERIVE: &str = r#"#![no_std]
mod derive {
    pub mod known_layout { pub fn derive() {} }
    pub fn derive_immutable() {}
    pub mod project { pub fn derive() {} }
}
macro_rules! derive {
    ($(#[$attr:meta])* $trait:ident => $outer:ident => $inner:path) => {
        $(#[$attr])*
        #[proc_macro_derive($trait, attributes(zerocopy))]
        pub fn $outer() {
            let _ = $inner;
        }
    };
}
derive!(KnownLayout => derive_known_layout => crate::derive::known_layout::derive);
derive!(Immutable => derive_immutable => crate::derive::derive_immutable);
derive!(#[doc(hidden)] Project => derive_project => crate::derive::project::derive);

#[cfg(__ZEROCOPY_INTERNAL_USE_ONLY_NIGHTLY_FEATURES_IN_TESTS)]
#[proc_macro]
pub fn __test_hygienically_mixed_into_bytes() {}

#[proc_macro_derive(most_traits, attributes(zerocopy))]
pub fn most_traits() {}
"#;
    static ZC_TEXTS: Texts = Texts(&[&[("/core/lib.rs", CORE)], &[("/zerocopy_derive/lib.rs", ZEROCOPY_DERIVE)]]);
    let core_in = input("core", "/core/lib.rs", &[]);
    let zc_base = input("zerocopy_derive", "/zerocopy_derive/lib.rs", &[("core", 0)]);
    let implicit = Implicit { std: StdIs::Rust, prelude: Prelude::Rust(edition::Edition::Edition2021), proc_macro: true, entry: false };
    let crates = [core_in, CrateInput { implicit, ..zc_base }];
    let sysroot = Sysroot { core: Some(CrateId(0)), ..Sysroot::default() };
    let names = resolve_crates(&crates, sysroot, &ZC_TEXTS, &Serial, super::SyncFrame::leaked())
        .unwrap_or_else(|e| panic!("resolve_crates: {e}"));

    let key = proc_macro_decls_key(crates[1].key);
    let (_, text) = text_of(&names, 1, key).expect("_DECLS must exist for zerocopy-derive pattern");
    let exports = &names.krate(1).proc_exports;
    assert_eq!(exports.len(), 4, "expected 1 direct + 3 macro-expanded derives (no cfg-disabled export), got {exports:?}; notes {:?}", names.notes(1));
    assert_eq!(exports[0].macro_name, "most_traits");
    assert_eq!(exports[0].fn_name, "most_traits");
    assert_eq!(ProcAttr::helper_names(exports[0].helpers).collect::<Vec<_>>(), std::vec!["zerocopy"]);

    let mut expanded: Vec<(&str, &str, Vec<&str>)> = exports[1..]
        .iter()
        .map(|e| (e.macro_name, e.fn_name, ProcAttr::helper_names(e.helpers).collect()))
        .collect();
    expanded.sort_unstable();
    assert_eq!(
        expanded.as_slice(),
        &[
            ("Immutable", "derive_immutable", std::vec!["zerocopy"]),
            ("KnownLayout", "derive_known_layout", std::vec!["zerocopy"]),
            ("Project", "derive_project", std::vec!["zerocopy"]),
        ]
    );
    let client = "::proc_macro::bridge::client::Client::";
    assert_eq!(text.matches(client).count(), 4, "_DECLS must contain all 4 entries: {text}");
    let mut prev = 0usize;
    for entry in exports.iter() {
        let needle = format!("{client}expand1(crate::{})", entry.fn_name);
        let pos = text.find(&needle).unwrap_or_else(|| panic!("`{needle}` missing from `_DECLS`: {text}"));
        assert!(pos >= prev, "`_DECLS` order must match `proc_exports` order: {text}");
        prev = pos;
    }
}

/// Real-world `derive_more-impl 2.1.1` pattern: parenthesized `macro_rules! create_derive(...)`
/// emitting `#[cfg(feature = $feature)]` + `#[proc_macro_derive($trait_, attributes($($attribute),*))]`
/// (with both non-empty and empty helper attribute lists). Only enabled features must appear in
/// `_DECLS` and `proc_exports`, in identical order.
#[test]
fn derive_more_impl_cfg_gated_create_derive_emits_enabled_proc_exports_and_decls() {
    const DERIVE_MORE_IMPL: &str = r#"#![no_core]
macro_rules! create_derive(
    ($feature:literal, $mod_:ident $(:: $mod_rest:ident)*, $trait_:ident, $fn_name: ident $(,$attribute:ident)* $(,)?) => {
        #[cfg(feature = $feature)]
        #[proc_macro_derive($trait_, attributes($($attribute),*))]
        pub fn $fn_name() {}
    }
);

create_derive!("add", ops::add, Add, add_derive, add);
create_derive!("constructor", constructor, Constructor, constructor_derive);
create_derive!("debug", fmt::debug, Debug, debug_derive, debug);
create_derive!("deref", deref, Deref, deref_derive, deref);
create_derive!("display", fmt::display, Display, display_derive, display);
"#;
    static DM_TEXTS: Texts = Texts(&[&[("/derive_more_impl/lib.rs", DERIVE_MORE_IMPL)]]);
    let base = input("derive_more_impl", "/derive_more_impl/lib.rs", &[]);
    let implicit = Implicit { std: StdIs::Rust, prelude: Prelude::Rust(edition::Edition::Edition2021), proc_macro: true, entry: false };
    let cfg: &[(&str, Option<&str>)] = &[
        ("feature", Some("debug")),
        ("feature", Some("constructor")),
        ("feature", Some("deref")),
    ];
    let crates = [CrateInput { implicit, features: &["debug", "constructor", "deref"], cfg, ..base }];
    let names = resolve_crates(&crates, Sysroot::default(), &DM_TEXTS, &Serial, super::SyncFrame::leaked())
        .unwrap_or_else(|e| panic!("resolve_crates: {e}"));

    let key = proc_macro_decls_key(crates[0].key);
    let (_, text) = text_of(&names, 0, key).expect("_DECLS must exist for derive_more-impl pattern");
    let exports = &names.krate(0).proc_exports;
    assert_eq!(exports.len(), 3, "expected only enabled features (Constructor, Debug, Deref), got {exports:?}; notes {:?}", names.notes(0));
    let mut got: Vec<(&str, &str, Vec<&str>)> = exports
        .iter()
        .map(|e| (e.macro_name, e.fn_name, ProcAttr::helper_names(e.helpers).collect()))
        .collect();
    got.sort_unstable();
    assert_eq!(
        got.as_slice(),
        &[
            ("Constructor", "constructor_derive", std::vec![]),
            ("Debug", "debug_derive", std::vec!["debug"]),
            ("Deref", "deref_derive", std::vec!["deref"]),
        ]
    );
    let client = "::proc_macro::bridge::client::Client::";
    assert_eq!(text.matches(client).count(), 3, "_DECLS must contain all 3 enabled entries: {text}");
    let mut prev = 0usize;
    for entry in exports.iter() {
        let needle = format!("{client}expand1(crate::{})", entry.fn_name);
        let pos = text.find(&needle).unwrap_or_else(|| panic!("`{needle}` missing from `_DECLS`: {text}"));
        assert!(pos >= prev, "`_DECLS` order must match `proc_exports` order: {text}");
        prev = pos;
    }
}

/// Native targets emit the link-time C-ABI accessors `__blaze_pm_decls` and `__blaze_pm_decls_len`
/// alongside `_DECLS` (never bound in the crate's scope), while `wasm64` and `wasm32` targets
/// suppress them so wasm proc-macro modules have zero extra items or exports.
#[test]
fn link_time_decls_accessors_emit_on_native_and_stay_gated_off_on_wasm() {
    let g = Alloc::global();
    let implicit = Implicit {
        std: StdIs::Rust,
        prelude: Prelude::Rust(edition::Edition::Edition2021),
        proc_macro: true,
        entry: false,
    };

    // 1. Native target (`aarch64` / `linux`): emits `_DECLS`, `__blaze_pm_decls`, `__blaze_pm_decls_len`.
    let native_cfg: &[(&str, Option<&str>)] = &[
        ("target_arch", Some("aarch64")),
        ("target_os", Some("linux")),
        ("target_family", Some("unix")),
        ("unix", None),
    ];
    let macros_native = CrateInput {
        implicit,
        cfg: native_cfg,
        ..input("macros", "/macros/lib.rs", &[])
    };
    let native_crates = [input("plain", "/plain/lib.rs", &[]), macros_native];
    let native_names = resolve_crates(
        &native_crates,
        Sysroot::default(),
        &TEXTS,
        &Serial,
        super::SyncFrame::leaked(),
    )
    .unwrap_or_else(|e| panic!("resolve_crates native: {e}"));

    let pm_key = native_crates[1].key;
    assert!(text_of(&native_names, 1, proc_macro_decls_key(pm_key)).is_some());

    let (fn_name, fn_text) = text_of(&native_names, 1, proc_macro_decls_fn_key(pm_key))
        .expect("native target must emit `__blaze_pm_decls`");
    assert_eq!(fn_name, Some(PROC_MACRO_DECLS_FN));
    assert!(fn_text.contains("#[unsafe(export_name = \"__blaze_pm_decls\")]"), "{fn_text}");
    assert!(
        fn_text.contains("pub extern \"C\" fn __blaze_pm_decls() -> *const ::proc_macro::bridge::client::Client"),
        "{fn_text}"
    );
    let client = "::proc_macro::bridge::client::Client::";
    let wanted = [
        format!("{client}expand1(crate::derive_hello)"),
        format!("{client}expand2(crate::r#trace)"),
        format!("{client}expand1(crate::make)"),
    ];
    let at: Vec<usize> = wanted
        .iter()
        .map(|w| fn_text.find(w.as_str()).unwrap_or_else(|| panic!("`{w}` in {fn_text}")))
        .collect();
    assert!(at.windows(2).all(|w| w[0] < w[1]), "declaration order: {at:?} in {fn_text}");
    assert_eq!(fn_text.matches(client).count(), 3, "{fn_text}");

    let (len_name, len_text) = text_of(&native_names, 1, proc_macro_decls_len_fn_key(pm_key))
        .expect("native target must emit `__blaze_pm_decls_len`");
    assert_eq!(len_name, Some(PROC_MACRO_DECLS_LEN_FN));
    assert!(len_text.contains("#[unsafe(export_name = \"__blaze_pm_decls_len\")]"), "{len_text}");
    assert!(len_text.contains("3usize"), "{len_text}");

    // Neither accessor may be bound into the crate's scope.
    for sym in [PROC_MACRO_DECLS_FN, PROC_MACRO_DECLS_LEN_FN] {
        let bound = native_names.item_at(
            native_names.root(1),
            Namespace::Values,
            Written { kind: PathKind::Plain, segments: &[sym] },
            g,
        );
        assert!(bound.is_err(), "`{sym}` must not be nameable from the crate");
    }

    // 2. Wasm64 and wasm32 targets: `_DECLS` present, BOTH native accessors cfg'd out!
    const WASM64_CFG: &[(&str, Option<&str>)] = &[
        ("target_arch", Some("wasm64")),
        ("target_os", Some("wasi")),
        ("target_family", Some("wasm")),
    ];
    const WASM32_CFG: &[(&str, Option<&str>)] = &[
        ("target_arch", Some("wasm32")),
        ("target_os", Some("wasi")),
        ("target_family", Some("wasm")),
    ];
    for (arch, wasm_cfg) in [("wasm64", WASM64_CFG), ("wasm32", WASM32_CFG)] {
        let macros_wasm = CrateInput {
            implicit,
            cfg: wasm_cfg,
            ..input("macros", "/macros/lib.rs", &[])
        };
        let wasm_crates = [input("plain", "/plain/lib.rs", &[]), macros_wasm];
        let wasm_names = resolve_crates(
            &wasm_crates,
            Sysroot::default(),
            &TEXTS,
            &Serial,
            super::SyncFrame::leaked(),
        )
        .unwrap_or_else(|e| panic!("resolve_crates {arch}: {e}"));
        let k = wasm_crates[1].key;
        assert!(text_of(&wasm_names, 1, proc_macro_decls_key(k)).is_some(), "{arch} keeps `_DECLS`");
        assert!(
            text_of(&wasm_names, 1, proc_macro_decls_fn_key(k)).is_none(),
            "{arch} must NOT emit `__blaze_pm_decls`"
        );
        assert!(
            text_of(&wasm_names, 1, proc_macro_decls_len_fn_key(k)).is_none(),
            "{arch} must NOT emit `__blaze_pm_decls_len`"
        );
    }

    // 3. Empty proc-macro crate (0 macros): valid empty slice and `0usize`.
    const EMPTY_PM: &str = "#![no_core]\npub fn helper() {}\n";
    static EMPTY_TEXTS: Texts = Texts(&[&[("/empty/lib.rs", EMPTY_PM)]]);
    let empty_crates = [CrateInput {
        implicit,
        cfg: native_cfg,
        ..input("empty_pm", "/empty/lib.rs", &[])
    }];
    let empty_names = resolve_crates(
        &empty_crates,
        Sysroot::default(),
        &EMPTY_TEXTS,
        &Serial,
        super::SyncFrame::leaked(),
    )
    .unwrap_or_else(|e| panic!("resolve_crates empty: {e}"));
    let ek = empty_crates[0].key;
    let (_, empty_fn) = text_of(&empty_names, 0, proc_macro_decls_fn_key(ek)).expect("empty decls fn");
    let (_, empty_len) = text_of(&empty_names, 0, proc_macro_decls_len_fn_key(ek)).expect("empty decls len fn");
    assert!(empty_fn.contains("static DECLS: &[::proc_macro::bridge::client::Client] = &[];"), "{empty_fn}");
    assert!(empty_len.contains("0usize"), "{empty_len}");
}
