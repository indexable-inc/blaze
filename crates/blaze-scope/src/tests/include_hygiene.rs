//! `include!` hygiene and `Transparency::Transparent` `$crate` attribution across crates.
//!
//! Reduced from `target-lexicon 0.13.5/build.rs`:
//! ```ignore
//! mod triple { include!("src/triple.rs"); }
//! use self::triple::Triple;
//! ```
//! where `src/triple.rs` defines `pub struct Triple;` AND `#[macro_export] macro_rules! triple {
//! ($s:tt) => { $crate::Triple }; }`.
//!
//! Because `include!` is defined in `core`, marking its tokens with `core::include`'s crate caused
//! `render_with` to see `$crate` inside `macro_rules! triple` as `core != app` and drop the entire
//! `include!` expansion with `ExpandNote::ForeignDollarCrate { krate: core }`. The same bug hit
//! proc-macros emitting `macro_rules! { ... $crate ... }` with `Span::call_site()`
//! (`Transparency::Transparent`).
//!
//! Conversely, a `SemiOpaque` (`macro_rules!`) macro defined in `dep` MUST still attribute `$crate`
//! to `dep` (both for paths it expands directly and for refusing foreign `$crate` inside a
//! `macro_rules!` definition it emits).

use std::format;
use std::vec::Vec;

use blaze_expand::{HostAsk, HostError, HostWhy, MacroDefinition, ProcCall, ProcKind, Produced, Reads, TokenOrigin, Transparency, lex};
use blaze_id::{ContentId, CrateItemKey};
use blaze_own::{AVec, Alloc};
use blaze_surface::{Edition, Namespace, PathKind};
use blaze_unit_graph::{Implicit, Prelude, StdIs};

use super::entry::input;
use crate::graph::Sysroot;
use crate::names::Written;
use crate::resolve::{CrateInput, ExpandNote, Names, NamesNote, SourceTexts, resolve_crates};
use crate::task::Serial;

const CORE: &str = "#![no_core]
#[macro_export]
#[rustc_builtin_macro]
macro_rules! include { () => {} }
";

const DEP: &str = "#![no_core]
pub struct DepType;
#[macro_export]
macro_rules! make_dep_alias {
    () => {
        pub type FromDep = $crate::DepType;
    };
}
#[macro_export]
macro_rules! emit_foreign_macro {
    () => {
        macro_rules! foreign_m {
            () => {
                pub type Bad = $crate::DepType;
            };
        }
    };
}
";

const PROC_MACROS: &str = "#![no_core]
#[proc_macro]
pub fn gen_macro(input: TokenStream) -> TokenStream { input }
";

const TRIPLE_RS: &str = "pub struct Triple;
#[macro_export]
macro_rules! triple {
    ($s:tt) => {
        pub type Checked = $crate::Triple;
    };
}
";

const APP: &str = "#![no_core]
mod triple {
    core::include!(\"src/triple.rs\");
}
pub use self::triple::Triple;

pub mod check {
    crate::triple!(\"x86_64\");
}

dep::make_dep_alias!();
macros::gen_macro!();

pub mod check_proc {
    crate::from_proc!();
}
";

const CONSUMER: &str = "#![no_core]
app::triple!(\"aarch64\");
";

const BAD_APP: &str = "#![no_core]
dep::emit_foreign_macro!();
foreign_m!();
";

const RUNNER: CrateItemKey = CrateItemKey(ContentId([8; 32]));

struct WorldSources(&'static [&'static [(&'static str, &'static str)]], usize);

impl SourceTexts<'static> for WorldSources {
    fn read(&self, krate: usize, path: &str) -> Option<&'static str> {
        self.0.get(krate)?.iter().find(|(p, _)| *p == path).map(|(_, t)| *t)
    }

    fn proc_macro_key(&self, krate: usize) -> Option<CrateItemKey> {
        (krate == self.1).then_some(RUNNER)
    }

    fn run_proc_macro<'b>(
        &self,
        _: usize,
        def: &MacroDefinition<'_>,
        call: ProcCall<'_>,
        alloc: Alloc<'b>,
    ) -> Result<Produced<'b>, HostError> {
        let refused = HostError { ask: HostAsk::ProcMacro, why: HostWhy::Refused };
        let text = match (call.kind, def.proc_name()) {
            (ProcKind::FnLike, Some("gen_macro")) => {
                "#[macro_export]\nmacro_rules! from_proc { () => { pub type ViaProc = $crate::Triple; }; }"
            }
            _ => return Err(refused),
        };
        let tokens = lex(text, Edition::E2021, alloc).map_err(|_| refused)?;
        let mut origins = AVec::new_in(alloc);
        origins.extend(tokens.tokens().iter().map(|_| TokenOrigin::Made(Transparency::Transparent)));
        Ok(Produced { tokens, origins, reads: Reads::new_in(alloc), diagnostics: AVec::new_in(alloc), text: AVec::new_in(alloc) })
    }
}

fn errors(names: &Names<'static>, krate: usize) -> Vec<std::string::String> {
    names.krate(krate).done.values().flat_map(|done| done.scope.errors.iter().map(|e| format!("{e:?}"))).collect()
}

#[test]
fn include_from_core_and_call_site_proc_macro_keep_caller_dollar_crate() {
    static FILES: WorldSources = WorldSources(
        &[
            &[("/core/src/lib.rs", CORE)],
            &[("/dep/src/lib.rs", DEP)],
            &[("/macros/src/lib.rs", PROC_MACROS)],
            &[("/app/build.rs", APP), ("/app/src/triple.rs", TRIPLE_RS)],
            &[("/consumer/src/lib.rs", CONSUMER)],
        ],
        2,
    );
    let core_in = input("core", "/core/src/lib.rs", &[]);
    let dep_in = input("dep", "/dep/src/lib.rs", &[("core", 0)]);
    let macros_base = input("macros", "/macros/src/lib.rs", &[]);
    let macros_in = CrateInput {
        implicit: Implicit { std: StdIs::Rust, prelude: Prelude::Rust(edition::Edition::Edition2021), proc_macro: true, entry: false },
        ..macros_base
    };
    let mut app_in = input("app", "/app/build.rs", &[("core", 0), ("dep", 1), ("macros", 2)]);
    app_in.shown_from = "/app/";
    let consumer_in = input("consumer", "/consumer/src/lib.rs", &[("app", 3)]);
    let crates = [core_in, dep_in, macros_in, app_in, consumer_in];

    let names = resolve_crates(&crates, Sysroot::default(), &FILES, &Serial, super::SyncFrame::leaked())
        .unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let g = blaze_own::testing::heap().alloc();

    // 1. `app`: `mod triple { core::include!("src/triple.rs"); } pub use self::triple::Triple;`
    //    succeeds with no `ForeignDollarCrate` error and resolves `Triple` and `check::Checked`.
    let app_errors = errors(&names, 3);
    assert!(app_errors.is_empty(), "app errors: {app_errors:?}\nnotes: {:?}", names.notes(3));
    let app_notes = format!("{:?}", names.notes(3));
    assert!(!app_notes.contains("ForeignDollarCrate"), "app must not hit ForeignDollarCrate: {app_notes}");

    let triple_in_app = names
        .item_at(names.root(3), Namespace::Types, Written { kind: PathKind::Plain, segments: &["Triple"] }, g)
        .unwrap_or_else(|e| panic!("app::Triple must resolve: {e:?}; notes: {app_notes}"));
    assert_eq!(triple_in_app.krate, 3);

    let checked_in_app = names
        .item_at(names.root(3), Namespace::Types, Written { kind: PathKind::Plain, segments: &["check", "Checked"] }, g)
        .unwrap_or_else(|e| panic!("app::check::Checked must resolve: {e:?}; notes: {app_notes}"));
    assert_eq!(checked_in_app.krate, 3);

    // 2. Downstream `consumer` calling `app::triple!("aarch64")`: `$crate` in the included
    //    `macro_rules! triple` resolves to `app` (NOT `core` and NOT `consumer`).
    let consumer_errors = errors(&names, 4);
    assert!(consumer_errors.is_empty(), "consumer errors: {consumer_errors:?}\nnotes: {:?}", names.notes(4));
    let checked_in_consumer = names
        .item_at(names.root(4), Namespace::Types, Written { kind: PathKind::Plain, segments: &["Checked"] }, g)
        .unwrap_or_else(|e| panic!("consumer::Checked must resolve: {e:?}"));
    assert_eq!(checked_in_consumer.krate, 4);
    // Verify the `crate` root written for `$crate` in `consumer`'s expansion of `app::triple!`
    // points to `app`'s crate key (NOT `core`'s).
    let app_key = names.krate(3).key;
    let core_key = names.krate(0).key;
    let consumer_exp_roots: Vec<CrateItemKey> = names
        .krate(4)
        .expansions
        .values()
        .filter_map(|e| e.made.as_ref())
        .filter_map(|made| crate::crate_roots::column_in(&made.origins).ok())
        .flat_map(|col| (0..col.len()).filter_map(move |i| col.row(i).mark.map(|m| m.site.krate)))
        .collect();
    assert!(consumer_exp_roots.contains(&app_key), "consumer's expansion of `app::triple!` must attribute `$crate` to `app`: {consumer_exp_roots:?}");
    assert!(!consumer_exp_roots.contains(&core_key), "consumer's expansion of `app::triple!` must NOT attribute `$crate` to `core`: {consumer_exp_roots:?}");

    // 3. Proc-macro emitting `macro_rules! from_proc { () => { pub type ViaProc = $crate::Triple; }; }`
    //    with `Span::call_site()` (`Transparency::Transparent`): resolves cleanly in `app`.
    let via_proc = names
        .item_at(names.root(3), Namespace::Types, Written { kind: PathKind::Plain, segments: &["check_proc", "ViaProc"] }, g)
        .unwrap_or_else(|e| panic!("app::check_proc::ViaProc must resolve: {e:?}; notes: {app_notes}"));
    assert_eq!(via_proc.krate, 3);

    // 4. Control: `SemiOpaque` macro `dep::make_dep_alias!()` STILL attributes `$crate` to `dep`.
    let dep_key = names.krate(1).key;
    let from_dep = names
        .item_at(names.root(3), Namespace::Types, Written { kind: PathKind::Plain, segments: &["FromDep"] }, g)
        .unwrap_or_else(|e| panic!("app::FromDep must resolve: {e:?}"));
    assert_eq!(from_dep.krate, 3);
    let app_exp_roots: Vec<CrateItemKey> = names
        .krate(3)
        .expansions
        .values()
        .filter_map(|e| e.made.as_ref())
        .filter_map(|made| crate::crate_roots::column_in(&made.origins).ok())
        .flat_map(|col| (0..col.len()).filter_map(move |i| col.row(i).mark.map(|m| m.site.krate)))
        .collect();
    assert!(app_exp_roots.contains(&dep_key), "`dep::make_dep_alias!()` must still attribute `$crate` to `dep`: {app_exp_roots:?}");
}

#[test]
fn semi_opaque_macro_emitting_macro_rules_with_uniform_dollar_crate_preserves_foreign_crate() {
    static FILES: WorldSources = WorldSources(
        &[
            &[("/dep/src/lib.rs", DEP)],
            &[("/bad_app/src/lib.rs", BAD_APP)],
        ],
        usize::MAX,
    );
    let dep_in = input("dep", "/dep/src/lib.rs", &[]);
    let bad_app_in = input("bad_app", "/bad_app/src/lib.rs", &[("dep", 0)]);
    let crates = [dep_in, bad_app_in];

    let names = resolve_crates(&crates, Sysroot::default(), &FILES, &Serial, super::SyncFrame::leaked())
        .unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let dep_key = names.krate(0).key;
    assert!(
        names.notes(1).is_empty(),
        "token-mark-3: uniform foreign `$crate` in emitted `macro_rules!` must succeed cleanly; notes: {:?}",
        names.notes(1)
    );
    let foreign_m = names
        .krate(1)
        .tables
        .macros
        .values()
        .find(|m| m.name == "foreign_m")
        .expect("foreign_m macro lowered");
    assert_eq!(
        foreign_m.dollar_crate,
        Some(dep_key),
        "emitted `foreign_m` must preserve `dollar_crate = Some(dep)`"
    );
    let g = blaze_own::testing::heap().alloc();
    let bad_ty = names
        .item_at(names.root(1), Namespace::Types, Written { kind: PathKind::Plain, segments: &["Bad"] }, g)
        .unwrap_or_else(|e| panic!("bad_app::Bad must resolve: {e:?}"));
    assert_eq!(bad_ty.krate, 1);
    let app_exp_roots: Vec<CrateItemKey> = names
        .krate(1)
        .expansions
        .values()
        .filter_map(|e| e.made.as_ref())
        .filter_map(|made| crate::crate_roots::column_in(&made.origins).ok())
        .flat_map(|col| (0..col.len()).filter_map(move |i| col.row(i).mark.map(|m| m.site.krate)))
        .collect();
    assert!(
        app_exp_roots.contains(&dep_key),
        "expanding `foreign_m!()` must attribute `$crate::DepType` to `dep`: {app_exp_roots:?}"
    );
}
