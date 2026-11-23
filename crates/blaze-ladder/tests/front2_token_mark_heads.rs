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
    let tc = Toolchain::none(blaze_own::Alloc::global());
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
    let mut front = Front2::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global());
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
