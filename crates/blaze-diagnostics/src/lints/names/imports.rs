//! What rustc's name resolution lints, each as `rustc_resolve` buffers it and its early lint
//! pass emits it at the node it is buffered for.
//!
//! - `used`: which imports, `extern crate` items, `#[macro_use]` imports and macros the crate's
//!   paths are resolved through (rustc's `record_use`), the traits in scope where a method is
//!   looked up, and which imports are re-exported.
//! - `macros`: `unused_macros`, `unused_macro_rules` (`check_unused_macros`).
//! - `globs`: what `finalize_imports` and `lint_reexports` lint: the glob import that re-exports
//!   nothing (`unused_imports`), `pub_use_of_private_extern_crate`, `ambiguous_glob_reexports`,
//!   `hidden_glob_reexports`.
//! - `unused`: `check_unused`: `unused_imports`, `macro_use_extern_crate`,
//!   `unused_extern_crates`, `redundant_imports`, and the trait imports only type checking uses
//!   (`rustc_hir_analysis`'s `check_unused_traits`).
//! - `qualifications`: `unused_qualifications`.
//!
//! Follows rustc's `compiler/rustc_resolve/src/lib.rs` (`resolve_crate`: expansion's
//! `check_unused_macros`, then `finalize_imports`, `lint_reexports`, late resolution and
//! `check_unused`, each buffering in that order), `compiler/rustc_lint/src/early.rs` (`check_id`
//! emits what is buffered for a node as the walk comes to it) and
//! `compiler/rustc_hir_analysis/src/check_unused.rs` (MIT or Apache-2.0).

mod globs;
mod macros;
mod qualifications;
mod unused;
mod used;

use blaze_hash::HashMap;
use hir::{HirFileId, InFile};
use hir_expand::MacroCallKind;
use smallvec::SmallVec;
use syntax::{SyntaxNodePtr, TextRange};

use crate::{
    cx::Cx,
    lints::registry::{LintId, LintSet},
    lower::{Found, Phase},
};

/// What name resolution lints, in the order rustc emits it: the lints buffered for one node in
/// the order they were buffered, nodes in the order the early lint pass comes to them, then
/// what type checking finds of trait imports.
pub(super) fn lints(cx: &Cx<'_, '_>) -> Vec<Found> {
    const GIVES: LintSet = LintSet::of(&[
        LintId::named("unused_imports"),
        LintId::named("redundant_imports"),
        LintId::named("unused_extern_crates"),
        LintId::named("macro_use_extern_crate"),
        LintId::named("unused_macros"),
        LintId::named("unused_macro_rules"),
        LintId::named("unused_qualifications"),
        LintId::named("ambiguous_glob_reexports"),
        LintId::named("hidden_glob_reexports"),
        LintId::named("pub_use_of_private_extern_crate"),
    ]);
    if !cx.any_may_fire(&GIVES) {
        // alloc: an empty `Vec` allocates nothing
        return Vec::new();
    }
    let walk = cx.walk();
    let places = Places::of(cx);
    let uses = used::Uses::of(cx, walk, &places);
    let mut buffered = macros::lints(cx, walk, &uses);
    buffered.extend(globs::lints(cx, &uses));
    let mut unused = unused::Unused::of(cx, &uses);
    buffered.append(&mut unused.resolve);
    if cx.lint_may_fire(LintId::named("unused_qualifications")) {
        buffered.extend(qualifications::lints(cx, walk, &uses, &unused));
    }
    let mut found = buffered;
    found.sort_by_cached_key(|found| found.lint_at.map(|at| places.place(cx, at)).unwrap_or_default());
    debug_assert!(found.iter().all(|it| it.phase == Phase::Resolve));
    found.append(&mut unused.bodies);
    found
}

/// Where rustc's early lint pass comes to a node: the places of the `mod` items and macro calls
/// it is reached through, outermost first, then its own; as the syntax lints' walk places them.
pub(super) struct Places {
    /// Each module file's place: where its `mod` item is, itself placed.
    files: HashMap<HirFileId, Place>,
}

/// A place, compared as rustc's walk order: inline up to four deep, which covers a node in a
/// macro call in a module file in a module file.
pub(super) type Place = SmallVec<[(u32, u32); 4]>;

impl Places {
    fn of(cx: &Cx<'_, '_>) -> Places {
        let mut files: HashMap<HirFileId, Place> = HashMap::default();
        for module in crate::checks::modules(cx) {
            let Some(file) = module.as_source_file_id(cx.db) else { continue };
            let file = HirFileId::from(file);
            if files.contains_key(&file) {
                continue;
            }
            let mut place = Place::new();
            if let Some(declared) = module.declaration_source_range(cx.db) {
                let declared = declared.original_node_file_range_rooted(cx.db);
                place = files.get(&HirFileId::from(declared.file_id)).cloned().unwrap_or_default();
                place.push(entered(declared.range));
            }
            files.insert(file, place);
        }
        Places { files }
    }

    /// The place of `at`.
    pub(super) fn place(&self, cx: &Cx<'_, '_>, at: InFile<SyntaxNodePtr>) -> Place {
        let mut inner: SmallVec<[(u32, u32); 2]> = SmallVec::new();
        inner.push(entered(at.value.text_range()));
        let mut file = at.file_id;
        while let Some(call) = file.macro_file() {
            let loc = call.loc(cx.db);
            let node = loc.to_node_view(cx.db);
            inner.push(match loc.kind {
                MacroCallKind::Derive { .. } => {
                    let adt = node.value.parent().map_or(node.value.text_range(), |it| it.text_range());
                    (u32::from(adt.end()), 1)
                }
                _ => entered(node.value.text_range()),
            });
            file = node.file_id;
        }
        let mut place = self.files.get(&file).cloned().unwrap_or_default();
        place.extend(inner.into_iter().rev());
        place
    }
}

/// The place of entering a node of `range`: outer nodes before the nodes in them.
fn entered(range: TextRange) -> (u32, u32) {
    (u32::from(range.start()), u32::MAX - u32::from(range.end()))
}
