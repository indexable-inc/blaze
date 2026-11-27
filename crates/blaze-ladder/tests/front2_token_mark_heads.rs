//! A path in an item's HEAD resolves by the mark of its first token (piece token-mark-F1;
//! drive.md, "A token keeps its mark"; front2 item_site.rs `resolve_written`), as a body's path
//! does (tests/front2_token_mark.rs, piece token-mark-1).
//!
//! The Reference, "Macros By Example", Hygiene: "By default, all identifiers referred to in a
//! macro expansion are looked up as if they appeared at the invocation site"; `macro_rules!`
//! has mixed-site hygiene (`$crate` names the defining crate); a `macro` item is def-site: its
//! names resolve where it is defined. Two `#![no_core]` crates written as files, no toolchain:
//! `dep` defines the trait `Tr`, a `macro_rules! impl_tr` writing `impl $crate::Tr for $t {}`,
//! and in its module `m` a private `struct Hidden`, a `pub macro mk_fn` writing `pub fn
//! by_macro(_: Hidden) {}` and an exported `macro_rules! mk_rules` writing `pub fn by_rules(_:
//! Hidden) {}`. The program calls all three at its root, which has no `Hidden`.
//!
//! Rows: `impl_tr` (the impl the call writes has `dep::Tr` as its trait), `by_macro` (its head
//! is made, `Hidden` found in `dep::m`); THE CONTROL `by_rules` (its head is refused
//! `Why::Resolve`: a `macro_rules!` item's names resolve at the call). Without the piece
//! `by_macro` is refused `Resolve` like the control; a resolver that read every expansion at its
//! definition makes the control's head and fails.
//!
//! What it prints when it passes:
//! ```text
//! front2_token_mark_heads: impl_tr: dep::Tr; by_macro: made; by_rules: Resolve (the control)
//! test token_mark_heads ... ok
//! ```

use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_ladder::crates::{Toolchain, syntax_of};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::heads::{HeadError, Store, Want};
use blaze_ladder::front2::keys::checker_of;
use blaze_ladder::front2::source::NamesSource;
use blaze_ladder::front2::types::Why;
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::resolve::{Ns, Paths};
use blaze_scope::graph::Sysroot;
use blaze_scope::resolve::{CrateInput, Names, SourceTexts, resolve_crates};
use blaze_scope::task::Serial;
use blaze_surface::ItemDetail;
use blaze_unit_graph::implicit;
use edition::PackageEdition;

const DEP: &str = "#![feature(no_core, lang_items, decl_macro)]
#![no_core]

#[lang = \"sized\"]
pub trait Sized {}

pub trait Tr {}

#[macro_export]
macro_rules! impl_tr { ($t:ident) => { impl $crate::Tr for $t {} } }

pub mod m {
    struct Hidden;
    pub macro mk_fn() { pub fn by_macro(_: Hidden) {} }
    #[macro_export]
    macro_rules! mk_rules { () => { pub fn by_rules(_: Hidden) {} } }
}
";

const ROOT: &str = "#![feature(no_core, decl_macro)]
#![no_core]
#![no_main]

pub struct Mine;
dep::impl_tr!(Mine);
dep::m::mk_fn!();
dep::mk_rules!();
";

const ROOT_PATH: &str = "/marks/src/main.rs";
const DEP_PATH: &str = "/dep/src/lib.rs";

struct Files;

impl<'t> SourceTexts<'t> for Files {
    fn read(&self, krate: usize, path: &str) -> Option<&'t str> {
        match (krate, path) {
            (0, ROOT_PATH) => Some(ROOT),
            (1, DEP_PATH) => Some(DEP),
            _ => None,
        }
    }
}

/// The program's items with text whose detail `is` accepts and whose name is `name` (`None`:
/// any name).
fn items_of(names: &Names<'_>, name: Option<&str>, is: fn(ItemDetail) -> bool) -> Vec<ItemKey> {
    names
        .items(0)
        .map(|(key, _, _)| key)
        .filter(|&key| NamesSource::detail_of(names, key).is_some_and(is))
        .filter(|&key| name.is_none() || NamesSource::name_of(names, key) == name)
        .collect()
}

/// `impl_tr!`'s impl names `dep::Tr`; `mk_fn!`'s fn head is made with `dep::m::Hidden`; the
/// control `mk_rules!`'s is refused `Why::Resolve` at the program's root.
///
/// Prints: `front2_token_mark_heads: impl_tr: dep::Tr; by_macro: made; by_rules: Resolve (the
/// control)` and `test token_mark_heads ... ok`
#[test]
fn token_mark_heads() {
    let alloc = blaze_own::testing::heap().alloc();
    let tc = Toolchain::none(alloc);
    let cfg = tc.program_cfg(blaze_build::Panic::Unwind);
    let rust = PackageEdition { syntax: edition::Edition::Edition2024, blaze: None };
    let lib = implicit(rust, &blaze_unit_graph::UnitKind::Lib);
    let deps: [(&str, usize); 1] = [("dep", 1)];
    let crates = [
        CrateInput {
            name: "marks",
            key: CrateItemKey(ContentId::of("front2_token_mark_heads program", b"marks")),
            edition: syntax_of(rust),
            implicit: lib,
            root: ROOT_PATH,
            shown_from: "/marks/",
            features: &[],
            deps: &deps,
            cfg: &cfg,
            script: None,
        },
        CrateInput {
            name: "dep",
            key: CrateItemKey(ContentId::of("front2_token_mark_heads test crate", b"dep")),
            edition: syntax_of(rust),
            implicit: lib,
            root: DEP_PATH,
            shown_from: "/dep/",
            features: &[],
            deps: &[],
            cfg: &cfg,
            script: None,
        },
    ];
    let names = resolve_crates(&crates, Sysroot::default(), &Files, &Serial, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: ROOT }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame");
    let mut front = Front2::new(&names, &frame, &sources, alloc).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, alloc);
    let mut failed: Vec<String> = Vec::new();
    let mut said: Vec<String> = Vec::new();

    // (a) The impl a `macro_rules!` of `dep` writes with `$crate::Tr`.
    let tr = Paths { names: &names }.path(names.root(1), false, &["Tr"], Ns::Type).unwrap_or_else(|e| panic!("dep::Tr: {e:?}")).key;
    let impls = items_of(&names, None, |d| matches!(d, ItemDetail::Impl));
    match impls.as_slice() {
        [one] => match store.ensure(&names, &mut front.keys, Want::Impl(*one)) {
            Ok(()) => {
                let got = store.impls.get(&checker_of(*one)).map(|h| h.trait_);
                if got == Some(Some(checker_of(tr))) {
                    said.push("impl_tr: dep::Tr".to_string());
                } else {
                    failed.push(format!("impl_tr: its trait is {got:?}, not dep::Tr ({:?})", checker_of(tr)));
                }
            }
            Err(e) => failed.push(format!("impl_tr: no head: {e:?}")),
        },
        other => failed.push(format!("impl_tr: the program holds {} impls, not one: {other:?}", other.len())),
    }

    // (b) A `macro`'s fn names its module's private `Hidden`; (c) the control.
    let is_fn = |d: ItemDetail| matches!(d, ItemDetail::Fn { .. });
    for (name, must_make) in [("by_macro", true), ("by_rules", false)] {
        let keys = items_of(&names, Some(name), is_fn);
        let [key] = keys.as_slice() else {
            failed.push(format!("{name}: the program holds {} fns of that name", keys.len()));
            continue;
        };
        let res = store.ensure(&names, &mut front.keys, Want::Fn(*key));
        let misses = store.type_misses.get(&checker_of(*key)).map_or(&[][..], |m| &m[..]);
        match (res, must_make, misses.is_empty()) {
            (Ok(()), true, true) => said.push(format!("{name}: made")),
            (Ok(()), true, false) => failed.push(format!("{name}: head had unresolved type misses: {misses:?}")),
            (Err(HeadError::Item(_, Why::Resolve(..))), false, _) => said.push(format!("{name}: Resolve (the control)")),
            (Ok(()), false, false) => said.push(format!("{name}: type miss (the control)")),
            (Ok(()), false, true) => failed.push(format!("{name}: THE CONTROL's head was made cleanly: a `macro_rules!` item's `Hidden` was found at the macro's module")),
            (Err(e), true, _) => failed.push(format!("{name}: no head: {e:?}")),
            (Err(e), false, _) => failed.push(format!("{name}: refused, but not as a path not found: {e:?}")),
        }
    }
    assert!(failed.is_empty(), "heads that did not say what they must: {failed:?} (said: {said:?})");
    println!("front2_token_mark_heads: {}", said.join("; "));
}

/// End-to-end head freezing for Fix 1B (`keep_item` preserving parent `OriginColumn` `TextMark`s):
/// - Positive 1 (`NodeId`): cross-crate `macro_rules!` emitting `#[derive(macros::Show)]
///   #[show(rename = "n")] pub struct NodeId($crate::nonmax::NonMaxU32);` freezes cleanly with zero
///   type misses (the exact `oxc_syntax::node::NodeId` / `oxc_index::define_index_type!` shape).
/// - Positive 2 (`NestedId`): multi-stage `KEEP_ITEM` (`#[derive(macros::Show)] #[show(a)]
///   #[derive(macros::Tag)] #[tag(b)] pub struct NestedId($crate::nonmax::NonMaxU32);`) freezes
///   cleanly with zero type misses.
/// - Negative control (`PlainId`): cross-crate `macro_rules!` emitting plain
///   `crate::nonmax::NonMaxU32` (NOT `$crate`) resolves `crate` in the caller crate (which has no
///   `nonmax`), so its head fails with unresolved type misses.
#[test]
fn kept_item_head_preserves_dollar_crate_and_plain_crate_control() {
    use blaze_expand::{HostAsk, HostError, HostWhy, MacroDefinition, ProcCall, ProcKind, Produced, Reads, TokenOrigin, Transparency, lex};
    use blaze_own::{AVec, Alloc};
    use blaze_surface::Edition;

    const MACROS_PATH: &str = "/macros/src/lib.rs";
    const MACROS_SRC: &str = "#![feature(no_core)]
#![no_core]
#[proc_macro_derive(Show, attributes(show))]
pub fn show(item: TokenStream) -> TokenStream { item }
#[proc_macro_derive(Tag, attributes(tag))]
pub fn tag(item: TokenStream) -> TokenStream { item }
";
    const DEP_KEPT_SRC: &str = "#![feature(no_core, lang_items)]
#![no_core]
#[lang = \"sized\"]
pub trait Sized {}
pub mod nonmax {
    pub struct NonMaxU32(pub u32);
}
#[macro_export]
macro_rules! define_index {
    ($name:ident) => {
        #[derive(macros::Show)]
        #[show(rename = \"idx\")]
        pub struct $name(#[show(skip)] $crate::nonmax::NonMaxU32);
    };
}
#[macro_export]
macro_rules! define_nested_index {
    ($name:ident) => {
        #[derive(macros::Show)]
        #[show(a)]
        #[derive(macros::Tag)]
        #[tag(b)]
        pub struct $name($crate::nonmax::NonMaxU32);
    };
}
#[macro_export]
macro_rules! define_plain_crate {
    ($name:ident) => {
        #[derive(macros::Show)]
        #[show(rename = \"plain\")]
        pub struct $name(crate::nonmax::NonMaxU32);
    };
}
";
    const ROOT_KEPT_SRC: &str = "#![feature(no_core)]
#![no_core]
#![no_main]
dep::define_index!(NodeId);
dep::define_nested_index!(NestedId);
dep::define_plain_crate!(PlainId);
";

    struct KeptFiles;
    impl<'t> SourceTexts<'t> for KeptFiles {
        fn read(&self, krate: usize, path: &str) -> Option<&'t str> {
            match (krate, path) {
                (0, ROOT_PATH) => Some(ROOT_KEPT_SRC),
                (1, DEP_PATH) => Some(DEP_KEPT_SRC),
                (2, MACROS_PATH) => Some(MACROS_SRC),
                _ => None,
            }
        }
        fn proc_macro_key(&self, krate: usize) -> Option<CrateItemKey> {
            (krate == 2).then_some(CrateItemKey(ContentId([31; 32])))
        }
        fn run_proc_macro<'b>(&self, _: usize, def: &MacroDefinition<'_>, call: ProcCall<'_>, alloc: Alloc<'b>) -> Result<Produced<'b>, HostError> {
            let refused = HostError { ask: HostAsk::ProcMacro, why: HostWhy::Refused };
            let text = match (call.kind, def.proc_name()) {
                (ProcKind::Derive, Some("Show" | "Tag")) => "",
                _ => return Err(refused),
            };
            let tokens = lex(text, Edition::E2024, alloc).map_err(|_| refused)?;
            let mut origins = AVec::new_in(alloc);
            origins.extend(tokens.tokens().iter().map(|_| TokenOrigin::Made(Transparency::Transparent)));
            Ok(Produced { tokens, origins, reads: Reads::new_in(alloc), diagnostics: AVec::new_in(alloc), text: AVec::new_in(alloc) })
        }
    }

    let alloc = blaze_own::testing::heap().alloc();
    let tc = Toolchain::none(alloc);
    let cfg = tc.program_cfg(blaze_build::Panic::Unwind);
    let rust = PackageEdition { syntax: edition::Edition::Edition2024, blaze: None };
    let lib = implicit(rust, &blaze_unit_graph::UnitKind::Lib);
    let pm = implicit(rust, &blaze_unit_graph::UnitKind::ProcMacro);
    let root_deps: [(&str, usize); 2] = [("dep", 1), ("macros", 2)];
    let dep_deps: [(&str, usize); 1] = [("macros", 2)];
    let crates = [
        CrateInput {
            name: "marks",
            key: CrateItemKey(ContentId::of("front2_token_mark_heads kept program", b"marks")),
            edition: syntax_of(rust),
            implicit: lib,
            root: ROOT_PATH,
            shown_from: "/marks/",
            features: &[],
            deps: &root_deps,
            cfg: &cfg,
            script: None,
        },
        CrateInput {
            name: "dep",
            key: CrateItemKey(ContentId::of("front2_token_mark_heads kept dep", b"dep")),
            edition: syntax_of(rust),
            implicit: lib,
            root: DEP_PATH,
            shown_from: "/dep/",
            features: &[],
            deps: &dep_deps,
            cfg: &cfg,
            script: None,
        },
        CrateInput {
            name: "macros",
            key: CrateItemKey(ContentId::of("front2_token_mark_heads kept macros", b"macros")),
            edition: syntax_of(rust),
            implicit: pm,
            root: MACROS_PATH,
            shown_from: "/macros/",
            features: &[],
            deps: &[],
            cfg: &cfg,
            script: None,
        },
    ];
    let names = resolve_crates(&crates, Sysroot::default(), &KeptFiles, &Serial, blaze_own::testing::bump())
        .unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: ROOT_KEPT_SRC }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame");
    let mut front = Front2::new(&names, &frame, &sources, alloc).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, alloc);

    let is_struct = |d: ItemDetail| matches!(d, ItemDetail::Struct);
    for (name, must_resolve) in [("NodeId", true), ("NestedId", true), ("PlainId", false)] {
        let keys = items_of(&names, Some(name), is_struct);
        let [key] = keys.as_slice() else {
            panic!("{name}: expected 1 struct, got {keys:?}");
        };
        let res = store.ensure(&names, &mut front.keys, Want::Adt(*key));
        let misses = store.type_misses.get(&checker_of(*key)).map_or(&[][..], |m| &m[..]);
        if must_resolve {
            assert!(res.is_ok(), "{name}: expected clean ADT head, got {res:?}");
            assert!(misses.is_empty(), "{name}: expected zero type misses, got {misses:?}");
        } else {
            let failed_resolve = matches!(res, Err(HeadError::Item(_, Why::Resolve(..)))) || !misses.is_empty();
            assert!(
                failed_resolve,
                "{name} (negative control): plain `crate::nonmax::NonMaxU32` must NOT resolve in `dep`, got res={res:?}, misses={misses:?}"
            );
        }
    }
}
