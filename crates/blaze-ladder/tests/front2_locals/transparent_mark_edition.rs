//! Transparent (`Span::call_site()`) `TextMark` edition preservation across item-level,
//! body-level, and nested (`macro_rules!` -> proc-macro) expansions.
//!
//! Reduced from `clap_derive 4.5.55` (`edition = "2021"`) invoked from `execpolicy`
//! (`edition = "2024"`): `clap_derive` emits `match self { Self::Check(ref mut __clap_arg) => ... }`
//! with `Span::call_site()` (`TokenOrigin::Made(Transparency::Transparent)`). Dropping the
//! transparent mark when projecting `TextMark` onto `OriginColumn` / `BodyWord` caused
//! `View::mark_of_pat` to return `None`, falling back to the calling crate's `Edition::E2024`
//! and firing `DiagCode::BindingModifier2024`.
//!
//! Positive cases (edition-2021 proc-macro invoked from edition-2024 crate):
//! - Item-level `#[derive(macros_2021::Subcommand)]` emitting `match self { Self::Check(ref mut arg) => *arg }`
//! - Body-level `macros_2021::match_ref_mut!(x)` emitting `match x { Cmd::Check(ref mut arg) => *arg }`
//! - Nested: edition-2021 transparent proc-macro invoked INSIDE an edition-2024 `macro_rules!`
//!   (both item-level `#[derive]` and body-level fn-like, while preserving `macro_rules!` local hygiene!)
//! All check cleanly with 0 diagnostics.
//!
//! Negative controls (same code from edition-2024 proc-macro or written directly in edition 2024):
//! - Item-level `#[derive(macros_2024::Subcommand)]` -> fires `BindingModifier2024`
//! - Body-level `macros_2024::match_ref_mut!(x)` -> fires `BindingModifier2024`
//! - Nested: edition-2024 transparent proc-macro invoked INSIDE an edition-2021 `macro_rules!`
//!   (both item-level `#[derive]` and body-level fn-like) -> fires `BindingModifier2024`
//! - Direct edition-2024 method `Cmd::direct_2024` -> fires `BindingModifier2024`
//!
//! Self-contained `#![no_core]`: needs no toolchain.

use blaze_expand::{HostAsk, HostError, HostWhy, MacroDefinition, ProcCall, ProcKind, Produced, Reads, TokenOrigin, Transparency, lex};
use blaze_id::{ContentId, CrateItemKey};
use blaze_ladder::front2::body_host::{BodySources, Sourced};
use blaze_ladder::front2::program::Front2Program;
use blaze_own::{AString, AVec, Alloc};
use blaze_scope::graph::Sysroot;
use blaze_scope::resolve::{CrateInput, Names, SourceTexts, resolve_crates};
use blaze_scope::task::Serial;
use blaze_surface::{Edition, ItemDetail};
use blaze_unit_graph::{Implicit, Prelude, StdIs};

const USER: &str = "#![feature(no_core, lang_items)]
#![no_core]

#[lang = \"sized\"]
trait Sized {}

#[lang = \"copy\"]
trait Copy {}

impl Copy for u32 {}

#[derive(macros_2021::Subcommand, macros_2024::Subcommand)]
pub enum Cmd {
    Check(u32),
}

rules_2024::emit_derive_2021!();
rules_2021::emit_derive_2024!();

impl Cmd {
    pub fn direct_2024(&mut self) -> u32 {
        match self {
            Self::Check(ref mut arg) => *arg,
        }
    }
}

fn body_2021(x: &mut Cmd) -> u32 {
    macros_2021::match_ref_mut!(x)
}

fn body_2024(x: &mut Cmd) -> u32 {
    macros_2024::match_ref_mut!(x)
}

fn nested_body_2021_in_2024(x: &mut Cmd) -> u32 {
    // Caller's `helper` has type `bool`; `rules_2024::call_2021_in_2024!` binds its own
    // `SemiOpaque` `helper: u32` and invokes `macros_2021::match_ref_mut!`, verifying both
    // E2021 transparent edition preservation and `SemiOpaque` local hygiene isolation.
    let helper = true;
    let _ = helper;
    rules_2024::call_2021_in_2024!(x)
}

fn nested_body_2024_in_2021(x: &mut Cmd) -> u32 {
    rules_2021::call_2024_in_2021!(x)
}

fn body_2015_async_ident() -> u32 {
    macros_2015::async_try_ident!()
}

fn body_2024_async_ident() -> u32 {
    macros_2024::async_try_ident!()
}

pub fn main_ok() -> u32 {
    let mut c = Cmd::Check(1);
    let mut n = NestedCmd2021::Check(2);
    let a = c.update_2021();
    let _b = body_2021(&mut c);
    let _c = n.nested_update_2021();
    let _d = nested_body_2021_in_2024(&mut c);
    let _e = body_2015_async_ident();
    a
}

pub fn main_body_2024_async_ident() -> u32 {
    body_2024_async_ident()
}

pub fn main_derive_2024() -> u32 {
    let mut c = Cmd::Check(1);
    c.update_2024()
}

pub fn main_body_2024() -> u32 {
    let mut c = Cmd::Check(1);
    body_2024(&mut c)
}

pub fn main_nested_derive_2024_in_2021() -> u32 {
    let mut n = NestedCmd2024::Check(1);
    n.nested_update_2024()
}

pub fn main_nested_body_2024_in_2021() -> u32 {
    let mut c = Cmd::Check(1);
    nested_body_2024_in_2021(&mut c)
}

pub fn main_direct_2024() -> u32 {
    let mut c = Cmd::Check(1);
    c.direct_2024()
}
";

const MACROS_2021: &str = "#![no_core]
#[proc_macro_derive(Subcommand)]
pub fn derive_subcommand(input: TokenStream) -> TokenStream { input }

#[proc_macro_derive(SubcommandNested2021)]
pub fn derive_subcommand_nested_2021(input: TokenStream) -> TokenStream { input }

#[proc_macro]
pub fn match_ref_mut(input: TokenStream) -> TokenStream { input }
";

const MACROS_2015: &str = "#![no_core]
#[proc_macro]
pub fn async_try_ident(input: TokenStream) -> TokenStream { input }
";

const MACROS_2024: &str = "#![no_core]
#[proc_macro_derive(Subcommand)]
pub fn derive_subcommand(input: TokenStream) -> TokenStream { input }

#[proc_macro_derive(SubcommandNested2024)]
pub fn derive_subcommand_nested_2024(input: TokenStream) -> TokenStream { input }

#[proc_macro]
pub fn match_ref_mut(input: TokenStream) -> TokenStream { input }

#[proc_macro]
pub fn async_try_ident(input: TokenStream) -> TokenStream { input }
";

const RULES_2024: &str = "#![no_core]
#[macro_export]
macro_rules! emit_derive_2021 {
    () => {
        #[derive(macros_2021::SubcommandNested2021)]
        pub enum NestedCmd2021 {
            Check(u32),
        }
    };
}

#[macro_export]
macro_rules! call_2021_in_2024 {
    ($x:expr) => {{
        let helper: u32 = 42;
        let _: u32 = helper;
        macros_2021::match_ref_mut!($x)
    }};
}
";

const RULES_2021: &str = "#![no_core]
#[macro_export]
macro_rules! emit_derive_2024 {
    () => {
        #[derive(macros_2024::SubcommandNested2024)]
        pub enum NestedCmd2024 {
            Check(u32),
        }
    };
}

#[macro_export]
macro_rules! call_2024_in_2021 {
    ($x:expr) => {
        macros_2024::match_ref_mut!($x)
    };
}
";

const RUNNER_2015: CrateItemKey = CrateItemKey(ContentId([15; 32]));
const RUNNER_2021: CrateItemKey = CrateItemKey(ContentId([21; 32]));
const RUNNER_2024: CrateItemKey = CrateItemKey(ContentId([24; 32]));

struct TransparentEditionRunner;

impl SourceTexts<'static> for TransparentEditionRunner {
    fn read(&self, krate: usize, path: &str) -> Option<&'static str> {
        [
            ("/user/lib.rs", USER),
            ("/macros_2021/lib.rs", MACROS_2021),
            ("/macros_2024/lib.rs", MACROS_2024),
            ("/rules_2024/lib.rs", RULES_2024),
            ("/rules_2021/lib.rs", RULES_2021),
            ("/macros_2015/lib.rs", MACROS_2015),
        ]
        .get(krate)
        .filter(|(p, _)| *p == path)
        .map(|(_, t)| *t)
    }

    fn proc_macro_key(&self, krate: usize) -> Option<CrateItemKey> {
        match krate {
            1 => Some(RUNNER_2021),
            2 => Some(RUNNER_2024),
            5 => Some(RUNNER_2015),
            _ => None,
        }
    }

    fn run_proc_macro<'b>(
        &self,
        _: usize,
        def: &MacroDefinition<'_>,
        call: ProcCall<'_>,
        alloc: Alloc<'b>,
    ) -> Result<Produced<'b>, HostError> {
        let refused = HostError { ask: HostAsk::ProcMacro, why: HostWhy::Refused };
        let (edition, text) = match (def.site().krate, call.kind, def.proc_name()) {
            (k, ProcKind::FnLike, Some("async_try_ident")) if k == RUNNER_2015 => (
                Edition::E2015,
                "{ let async: u32 = 7; let try: u32 = async; try }",
            ),
            (k, ProcKind::FnLike, Some("async_try_ident")) if k == RUNNER_2024 => (
                Edition::E2024,
                "{ let async: u32 = 7; let try: u32 = async; try }",
            ),
            (k, ProcKind::Derive, Some("Subcommand")) if k == RUNNER_2021 => (
                Edition::E2021,
                "impl Cmd { pub fn update_2021(&mut self) -> u32 { match self { Self::Check(ref mut arg) => *arg } } }",
            ),
            (k, ProcKind::Derive, Some("SubcommandNested2021")) if k == RUNNER_2021 => (
                Edition::E2021,
                "impl NestedCmd2021 { pub fn nested_update_2021(&mut self) -> u32 { match self { Self::Check(ref mut arg) => *arg } } }",
            ),
            (k, ProcKind::FnLike, Some("match_ref_mut")) if k == RUNNER_2021 => (
                Edition::E2021,
                "match x { Cmd::Check(ref mut arg) => *arg }",
            ),
            (k, ProcKind::Derive, Some("Subcommand")) if k == RUNNER_2024 => (
                Edition::E2024,
                "impl Cmd { pub fn update_2024(&mut self) -> u32 { match self { Self::Check(ref mut arg) => *arg } } }",
            ),
            (k, ProcKind::Derive, Some("SubcommandNested2024")) if k == RUNNER_2024 => (
                Edition::E2024,
                "impl NestedCmd2024 { pub fn nested_update_2024(&mut self) -> u32 { match self { Self::Check(ref mut arg) => *arg } } }",
            ),
            (k, ProcKind::FnLike, Some("match_ref_mut")) if k == RUNNER_2024 => (
                Edition::E2024,
                "match x { Cmd::Check(ref mut arg) => *arg }",
            ),
            _ => return Err(refused),
        };
        let tokens = lex(text, edition, alloc).map_err(|_| refused)?;
        let mut origins = AVec::new_in(alloc);
        origins.extend(tokens.tokens().iter().map(|_| TokenOrigin::Made(Transparency::Transparent)));
        // `match_ref_mut!($x)` forwards the input token `x` with its input span (like `quote!(match #input { ... })`),
        // so calling it from inside a `macro_rules!` preserves the caller's hygiene for `x` after #2840.
        if def.proc_name() == Some("match_ref_mut") {
            if let Some(in_idx) = call.input.tokens().iter().position(|t| call.input.text_of(t) == "x") {
                if let Some(out_idx) = tokens.tokens().iter().position(|t| tokens.text_of(t) == "x") {
                    if let Ok(in_u32) = u32::try_from(in_idx) {
                        origins[out_idx] = TokenOrigin::Input(in_u32);
                    }
                }
            }
        }
        Ok(Produced { tokens, origins, reads: Reads::new_in(alloc), diagnostics: AVec::new_in(alloc), text: AVec::new_in(alloc) })
    }
}

static FILES: TransparentEditionRunner = TransparentEditionRunner;

fn crate_input(
    name: &'static str,
    root: &'static str,
    edition: Edition,
    prelude_ed: edition::Edition,
    deps: &'static [(&'static str, usize)],
    proc_macro: bool,
) -> CrateInput<'static> {
    CrateInput {
        name,
        key: CrateItemKey(ContentId::of("front2 transparent mark edition test crate", name.as_bytes())),
        edition,
        implicit: Implicit { std: StdIs::Rust, prelude: Prelude::Rust(prelude_ed), proc_macro, entry: false },
        root,
        shown_from: "/",
        features: &[],
        deps,
        cfg: &[],
        script: None,
    }
}

fn fn_named(names: &Names<'_>, name: &str) -> blaze_id::ItemKey {
    for (key, _, place) in names.items(0) {
        let Some((text, outline)) = names.outline_at(0, place.at) else { continue };
        let item = outline.item(place.item);
        if !matches!(item.detail, ItemDetail::Fn { .. }) {
            continue;
        }
        let at = item.range.start as usize;
        if item.name.is_some_and(|r| &text[at + r.start as usize..at + r.end as usize] == name) {
            return key;
        }
    }
    panic!("crate 0 has no fn `{name}`")
}

#[test]
fn transparent_proc_macro_preserves_defining_edition_for_binding_modifiers() {
    let g = blaze_own::testing::heap().alloc();
    let crates = [
        crate_input(
            "user",
            "/user/lib.rs",
            Edition::E2024,
            edition::Edition::Edition2024,
            &[("macros_2021", 1), ("macros_2024", 2), ("rules_2024", 3), ("rules_2021", 4), ("macros_2015", 5)],
            false,
        ),
        crate_input("macros_2021", "/macros_2021/lib.rs", Edition::E2021, edition::Edition::Edition2021, &[], true),
        crate_input("macros_2024", "/macros_2024/lib.rs", Edition::E2024, edition::Edition::Edition2024, &[], true),
        crate_input("rules_2024", "/rules_2024/lib.rs", Edition::E2024, edition::Edition::Edition2024, &[("macros_2021", 1)], false),
        crate_input("rules_2021", "/rules_2021/lib.rs", Edition::E2021, edition::Edition::Edition2021, &[("macros_2024", 2)], false),
        crate_input("macros_2015", "/macros_2015/lib.rs", Edition::E2015, edition::Edition::Edition2015, &[], true),
    ];
    let names = resolve_crates(&crates, Sysroot::default(), &FILES, &Serial, blaze_own::testing::bump())
        .unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the survey's frame");
    let sources_val = Sourced(&FILES);
    let sources: &dyn BodySources = &sources_val;

    // Positive: edition-2021 transparent derive + body proc-macro (both direct AND nested inside
    // edition-2024 `macro_rules!`) check cleanly with 0 diagnostics (`BindingModifier2024` NOT
    // fired) while preserving `macro_rules!` local hygiene (`helper: u32`).
    let mut left_out = Vec::new();
    let _ok = Front2Program::survey_from(&names, &frame, &[], &[fn_named(&names, "main_ok")], &mut left_out, Some(sources), g)
        .unwrap_or_else(|e| panic!("edition-2021 transparent proc-macros must succeed: {e}"));
    let mut ok_rows: AVec<'_, AString<'_>> = AVec::new_in(g);
    for (f, row) in left_out.iter() {
        ok_rows.push(blaze_own::format_in!(g, "{f}: {}", row.class));
    }
    assert!(ok_rows.is_empty(), "edition-2021 transparent proc-macros (direct + nested in E2024 rules) must have 0 errors: {ok_rows:?}");

    // Negative controls: edition-2024 transparent derive, edition-2024 transparent body proc-macro,
    // nested edition-2024 transparent derive + body proc-macro inside edition-2021 `macro_rules!`,
    // and direct edition-2024 code must each fire `E0000` (`BindingModifier2024`).
    for (root_name, target_fn) in [
        ("main_derive_2024", "update_2024"),
        ("main_body_2024", "body_2024"),
        ("main_nested_derive_2024_in_2021", "nested_update_2024"),
        ("main_nested_body_2024_in_2021", "nested_body_2024_in_2021"),
        ("main_direct_2024", "direct_2024"),
    ] {
        left_out.clear();
        let res = Front2Program::survey_from(&names, &frame, &[], &[fn_named(&names, root_name)], &mut left_out, Some(sources), g);
        let mut rows: AVec<'_, AString<'_>> = AVec::new_in(g);
        for (f, row) in left_out.iter() {
            rows.push(blaze_own::format_in!(g, "{f}: {}", row.class));
        }
        let err_text = res.as_ref().err().map(|e| blaze_own::format_in!(g, "{e}")).unwrap_or_else(|| AString::new_in(g));
        assert!(
            rows.iter().any(|r| r.contains(target_fn) && r.contains("check: Diags: E0000")),
            "negative control `{root_name}` (`{target_fn}`) must fire E0000 (BindingModifier2024); left_out={rows:?}; err={err_text}"
        );
    }

    // Negative control for E2015 vs E2024 keyword parsing: `async`/`try` emitted by an E2024
    // transparent proc-macro must be rejected as keywords (failing to parse as identifiers).
    left_out.clear();
    let res = Front2Program::survey_from(&names, &frame, &[], &[fn_named(&names, "main_body_2024_async_ident")], &mut left_out, Some(sources), g);
    let mut rows: AVec<'_, AString<'_>> = AVec::new_in(g);
    for (f, row) in left_out.iter() {
        rows.push(blaze_own::format_in!(g, "{f}: {}", row.class));
    }
    assert!(
        rows.iter().any(|r| r.contains("body_2024_async_ident")) || res.is_err(),
        "negative control `main_body_2024_async_ident` must fail parsing `async`/`try` as identifiers in E2024; left_out={rows:?}"
    );
}
