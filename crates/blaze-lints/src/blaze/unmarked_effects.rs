//! `unmarked_pure`, `unmarked_nopanic`, `unmarked_noalloc` and `unmarked_nonblocking` (in
//! `blaze::edition`, allowed by default): a function the effects walk proves pure, nopanic,
//! noalloc or nonblocking that lacks the attribute saying so. The blaze edition infers effects
//! inside a crate and writes them at its border (docs/v2/plan/design/edition.md, "infer inside,
//! declare at the border"): a mark the code already earns lets callers, and for `pure` the
//! cache, rely on it, and holds the function to it from then on.
//!
//! What it reads: the answer the attribute's own check gives, `hir_ty_pure::check_generic` of
//! the definition for the attribute's `EffectDef` (docs/v2/plan/design/pure-fns.md,
//! effects.md), the query `blaze::pure`, `nopanic`, `noalloc` and `nonblocking` ask of a marked
//! function. So a function is found only where the attribute, once written, passes its check:
//! nothing is trusted but what the check trusts, the root's parameters are scanned as the
//! check scans them, and what the walk can't follow (a body that does not lower, a call it
//! can't resolve) is `Unchecked`, which every one of the four refuses, so nothing is found.
//!
//! - A generic function is proved as a marked one is: in its own where-clauses, a call only an
//!   instance decides (a type parameter's method, its drop, a parameter of a type parameter's
//!   type) left to each instance. The finding says so.
//! - `nopanic` is found only where the crate is checked with overflow checks
//!   (`crate_checks_overflow` is `Some(true)`: a dev profile, or `overflow-checks = true`):
//!   the bodies then have every optional overflow `Assert`, so what is proved there is proved
//!   without them too, and the mark holds in dev and in release. Checked without them, a
//!   function whose `x + 1` release drops would be marked and then refused in dev, so nothing
//!   is found; nor in a crate whose setting is not known (a lang crate), which the check of a
//!   marked function can't check.
//! - No attribute's kinds are a subset of another's (`pure` refuses no panic, allocation or
//!   blocking; `noalloc` and `nonblocking` each refuse a kind the other allows), so none
//!   implies another and each is found on its own.
//! - In a crate with `#![blaze::capabilities]` the global heap is out of reach, so `noalloc`
//!   is found only where the signature could allocate: a parameter (`self` included) whose
//!   type is or holds, behind references too, a type implementing `core::alloc::Allocator`,
//!   an ADT with an allocator parameter (`Vec<T, A>`), or bstd's `Cx`. A signature with none
//!   already says noalloc.
//!
//! Which functions: one with a body and none of the attribute's forms (the bare word,
//! `trusted = ".."`, a malformed one, or the attribute on the trait it is a method of or
//! implements), that is not a test, a bench, the crate's `main`, in test code or written by a
//! macro. By default only one at the border: a function with a visibility written (`pub`,
//! `pub(crate)`, `pub(in ..)`), or a method of a trait, or of an impl of one, whose trait has
//! one (a trait's methods take no visibility of their own). `unmarked-effects-private = true`
//! in `clippy.toml` extends it to every function. A method of an impl may be marked whether or
//! not its trait is: the check reads the function's own attribute first.
//!
//! One finding per lint that is on and proved, each at the function's name, so a level set
//! on the function (`#[expect(blaze::unmarked_noalloc)]`, `#[allow(..)]`) works per lint as
//! for any other. Each finding has its own machine-applicable fix, of its one attribute: the
//! level of a lint at an item is decided after this query (`check::Driver::finish`, from the
//! command line and every attribute around the item, which the query is not keyed by), so no
//! finding here can know which of the others is allowed or expected, and one fix for all of
//! them would write an attribute its lint was told not to ask for.
//!
//! Where a fix writes its attribute keeps the function's effect attributes in the order
//! pure, nopanic, noalloc, nonblocking, moving none it has: on a line of its own before the
//! first effect attribute written that comes later in that order; else after the last one
//! written; else directly above the signature, under the doc comments and the other
//! attributes, at the item's indentation. Two of them can land at one place (a function with
//! no effect attribute has all of them above its signature), which `blaze check --fix` takes
//! as a clash (`fix::overlaps`) and makes one per pass, in the order of the findings, which is
//! that order: each pass places the next against what the last one wrote, so four attributes
//! take `fix::PASSES` passes and come out in order.

use base_db::Db;
use hir::HasVisibility as _;
use hir_def::{
    AdtId, DefWithBodyId, FunctionId, HasModule, ItemContainerId, Lookup,
    signatures::FunctionSignature,
};
use hir_ty::db::HirDatabase as _;
use hir_ty_mir::mir::overflow::crate_checks_overflow;
use hir_ty_pure::{
    effect_attrs::{NOALLOC, NONBLOCKING, NOPANIC, capabilities_asked},
    effects::EffectDef,
};
use syntax::{
    NodeOrToken, SyntaxKind, TextRange, TextSize,
    view::{self as ast, AstNode as _, HasAttrs as _, HasName as _, HasVisibility as _},
};

use super::{Lint, effects, facts, implicit_allocator::Allocators};
use crate::{check::Cx, diag::Applicability};

/// The `clippy.toml` flag that extends the lints from the border to every function.
const PRIVATE_KEY: &str = "unmarked-effects-private";

/// Each lint with the effect whose attribute it asks for, in the order the fix writes them.
/// `nopanic` is the one of a crate with overflow checks, the only crates it is found in.
const EFFECTS: [(Lint, &EffectDef); 4] = [
    (Lint::UnmarkedPure, &hir_ty_pure::PURE),
    (Lint::UnmarkedNopanic, &NOPANIC),
    (Lint::UnmarkedNoalloc, &NOALLOC),
    (Lint::UnmarkedNonblocking, &NONBLOCKING),
];

/// One lint's finding of the function `cx` walks the body of.
struct Found {
    lint: Lint,
    name: TextRange,
    message: String,
    notes: Vec<String>,
    /// Where the fix writes the attribute.
    insert: TextSize,
    fix: String,
    label: String,
}

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>) {
    if !EFFECTS.iter().any(|(lint, _)| cx.on_blaze(*lint)) {
        return;
    }
    let root = cx.root();
    for found in find(cx).unwrap_or_default() {
        let span = cx.span(found.name);
        let insert = cx.span(TextRange::empty(found.insert));
        let lint = found.lint;
        let diagnostic = cx.blaze(lint, root, span, found.message);
        for note in found.notes {
            diagnostic.note(note);
        }
        diagnostic.multipart_suggestion(
            found.label,
            // alloc: the fix's one part
            vec![(insert, found.fix)],
            Applicability::MachineApplicable,
        );
        diagnostic
            .help(lint.help())
            .help(format!("for further information visit {}", lint.docs_url()));
    }
}

/// The findings of the function, one per lint on and proved, in the order of [`EFFECTS`].
fn find<'t>(cx: &Cx<'_, 't>) -> Option<Vec<Found>> {
    let root = cx.root();
    let f = ast::Fn::cast(root)?;
    if cx.is_in_test(root) {
        return None;
    }
    let analysis = cx.analysis()?;
    let Ok(DefWithBodyId::FunctionId(func)) = DefWithBodyId::try_from(analysis.def()) else {
        return None;
    };
    let db = cx.db();
    // A trait's method with no body is a declaration: there is no code of it to prove.
    if !FunctionSignature::of(db, func).has_body() {
        return None;
    }
    let function = hir::Function::from(func);
    if function.is_main(db) || function.is_test(db) || function.is_bench(db) {
        return None;
    }
    if !cx.conf().bool(PRIVATE_KEY, false) && !at_border(db, f, func) {
        return None;
    }
    let krate = HasModule::krate(&func, db);
    let name = f.name()?;
    let name_range = cx.range(name.syntax())?;
    let whole = cx.range(root)?;
    let text = cx.text();
    // A place of the function's tree in the text the fix is made in.
    let in_text = |at: TextSize| whole.start() + (at - root.text_range().start());
    let signature = signature_start(f)?;
    let written = written_effects(f);

    let mut found = Vec::new(); // alloc: at most four findings, once per function that has one
    for (place, (lint, effect)) in EFFECTS.into_iter().enumerate() {
        if !cx.on_blaze(lint) || effects::marked(db, func, effect).is_some() {
            continue;
        }
        // Only bodies built with overflow checks have every `Assert` a profile can keep: a
        // proof without them does not hold in dev, and a crate whose setting is not known
        // can't be checked at all.
        if lint == Lint::UnmarkedNopanic && crate_checks_overflow(db, krate) != Some(true) {
            continue;
        }
        let Ok(proof) = hir_ty_pure::check_generic(db, func, effect) else { continue };
        if lint == Lint::UnmarkedNoalloc
            && capabilities_asked(db, krate)
            && !may_allocate(cx, function)
        {
            continue;
        }
        let word = effect.name;
        let relies =
            if lint == Lint::UnmarkedPure { "callers and the cache" } else { "callers" };
        let message = format!(
            "`fn {}` is proved {word} through every call it makes, and is not marked `#[blaze::{word}]`: marking it lets {relies} rely on it",
            name.text(),
        );
        let mut notes = Vec::new(); // alloc: a finding's notes, at most two
        if proof.open {
            notes.push(
                "what only an instance decides (a type parameter's method, its drop, a parameter of a type parameter's type) is left to each instance, as it is for a marked generic function"
                    .to_owned(), // alloc: a note
            );
        }
        if lint == Lint::UnmarkedNopanic {
            notes.push(
                "`nopanic` holds with this crate's overflow checks on, so the mark holds without them too"
                    .to_owned(), // alloc: a note
            );
        }
        // Before the first effect attribute written that comes later in the order, else after
        // the last one written, else where the signature starts.
        let later = written
            .iter()
            .filter(|&&(at, _)| at > place)
            .map(|&(_, range)| range)
            .min_by_key(|range| range.start());
        let last = written.iter().map(|&(_, range)| range).max_by_key(|range| range.end());
        let (insert, fix) = match (later, last) {
            (Some(next), _) => before(text, in_text(next.start()), whole.end(), word),
            (None, Some(last)) => {
                after(text, in_text(last.start()), in_text(last.end()), whole.end(), word)
            }
            (None, None) => before(text, in_text(signature), whole.end(), word),
        };
        let label = format!("mark it `#[blaze::{word}]`");
        found.push(Found { lint, name: name_range, message, notes, insert, fix, label });
    }
    Some(found)
}

/// What of `text` is on its line before `at`.
fn line_before(text: &str, at: TextSize) -> &str {
    let at = usize::from(at).min(text.len());
    &text[text[..at].rfind('\n').map_or(0, |it| it + 1)..at]
}

/// The indentation of the line `at` starts, or `None` when something is written before it on
/// its line.
///
/// `text` is a body's anchor alone when the function is linted by its body (`Cx::text`): it
/// starts at the function, so a signature or attribute with nothing above it is at offset 0
/// and the text does not have its line's indentation. It is then read from the line of the
/// function's closing brace (`end`), which is at the function's own indentation in formatted
/// code; a function written on one line has none to read. Without this the fix wrote
/// `    #[blaze::nopanic]\npub fn len(..)` in every `impl` (bstd, 2026-11-12).
// when `hir_def::expr_store::body::BodySyntax` says its anchor's column: read it here.
fn indent_of(text: &str, at: TextSize, end: TextSize) -> Option<&str> {
    if usize::from(at) != 0 {
        let lead = line_before(text, at);
        return lead.trim().is_empty().then_some(lead);
    }
    let last = line_before(text, end);
    let brace = last.trim_start();
    (brace == "}" && usize::from(end) > last.len()).then(|| &last[..last.len() - brace.len()])
}

/// The edit that writes `#[blaze::<word>]` before what starts at `at`: on a line of its own at
/// that line's indentation when `at` starts its line, before it on its line otherwise. `end`
/// is where the function ends. `claims` writes the attribute a doc's claim is with it.
pub(super) fn before(text: &str, at: TextSize, end: TextSize, word: &str) -> (TextSize, String) {
    match indent_of(text, at, end) {
        Some(indent) => (at, format!("#[blaze::{word}]\n{indent}")), // alloc: the fix's text
        None => (at, format!("#[blaze::{word}] ")),                  // alloc: the fix's text
    }
}

/// The edit that writes `#[blaze::<word>]` after the attribute from `start` to `after`: on the
/// next line at its indentation when the attribute starts its line, after it otherwise. `end`
/// is where the function ends.
fn after(text: &str, start: TextSize, after: TextSize, end: TextSize, word: &str) -> (TextSize, String) {
    match indent_of(text, start, end) {
        Some(indent) => (after, format!("\n{indent}#[blaze::{word}]")), // alloc: the fix's text
        None => (after, format!(" #[blaze::{word}]")),                  // alloc: the fix's text
    }
}

/// The effect attributes written on `f`, a `cfg_attr`'s included: each one's place in
/// [`EFFECTS`] and where its attribute is in `f`'s tree. Read as the front end reads a tool
/// attribute, by its path's two segments (`hir_def::attrs`); whether the function is marked
/// is the front end's answer ([`effects::marked`]), this only places the fix.
fn written_effects(f: ast::Fn<'_>) -> Vec<(usize, TextRange)> {
    let mut found = Vec::new(); // alloc: the few effect attributes of one function
    for attr in f.attrs() {
        for meta in attr.skip_cfg_attrs() {
            let Some(path) = meta.path() else { continue };
            let path: String =
                path.syntax().text().chars().filter(|it| !it.is_whitespace()).collect(); // alloc: one attribute's path
            let Some(word) = path.strip_prefix("blaze::") else { continue };
            if let Some(place) = EFFECTS.iter().position(|(_, effect)| effect.name == word) {
                found.push((place, attr.syntax().text_range()));
            }
        }
    }
    found
}

/// Where `f`'s signature starts in its tree: past its doc comments and attributes, at its
/// visibility, qualifiers or `fn`.
pub(super) fn signature_start(f: ast::Fn<'_>) -> Option<TextSize> {
    f.syntax().children_with_tokens().find_map(|child| match child {
        NodeOrToken::Node(node)
            if matches!(node.kind(), SyntaxKind::ATTR | SyntaxKind::DOC_COMMENT) =>
        {
            None
        }
        NodeOrToken::Node(node) => Some(node.text_range().start()),
        NodeOrToken::Token(token)
            if token.kind().is_trivia() || token.kind() == SyntaxKind::NEWLINE =>
        {
            None
        }
        NodeOrToken::Token(token) => Some(token.text_range().start()),
    })
}

/// Whether `func` is at its crate's border as written: it has a visibility of its own, or it
/// is a method of a trait, or of an impl of one, and the trait has one.
fn at_border(db: &Db, f: ast::Fn<'_>, func: FunctionId) -> bool {
    let of_trait = match func.lookup(db).container {
        ItemContainerId::TraitId(it) => Some(it),
        ItemContainerId::ImplId(imp) => db.impl_trait(imp).map(|it| it.skip_binder().def_id.0),
        _ => None,
    };
    match of_trait {
        Some(trait_) => match hir::Trait::from(trait_).visibility(db) {
            hir::Visibility::Public | hir::Visibility::PubCrate(_) => true,
            hir::Visibility::Module(_, explicit) => explicit.is_explicit(),
        },
        None => f.visibility().is_some(),
    }
}

/// Whether `function`'s signature hands it a way to allocate: a parameter, `self` included,
/// whose type is or holds (behind references too) an allocator, an ADT with an allocator
/// parameter, or bstd's `Cx`.
fn may_allocate(cx: &Cx<'_, '_>, function: hir::Function) -> bool {
    let db = cx.db();
    let allocators = Allocators::new(cx);
    let mut found = false;
    for param in function.assoc_fn_params(db) {
        param.ty().walk(db, |ty| {
            found = found || hands_allocator(db, &allocators, &ty);
        });
        if found {
            break;
        }
    }
    found
}

/// Whether `ty` itself is a way to allocate.
fn hands_allocator<'db>(db: &'db Db, allocators: &Allocators, ty: &hir::Type<'db>) -> bool {
    if ty.contains_unknown() {
        return false;
    }
    if let Some(adt) = ty.as_adt()
        && (allocators.allocator_param(db, AdtId::from(adt)).is_some() || is_bstd_cx(db, adt))
    {
        return true;
    }
    allocators.allocator_trait().is_some_and(|allocator| ty.impls_trait(db, allocator, &[]))
}

/// Whether `adt` is bstd's task context, `Cx` of the module `cx` at the root of the crate
/// `bstd` (the package `blaze-std`): found by what the type resolved to, so a crate that does
/// not depend on bstd has none.
fn is_bstd_cx(db: &Db, adt: hir::Adt) -> bool {
    let module = adt.module(db);
    let krate = module.krate(db);
    adt.name(db).as_str() == "Cx"
        && module.name(db).is_some_and(|it| it.as_str() == "cx")
        && module.parent(db).is_some_and(|it| it.is_crate_root(db))
        && (facts::crate_is(db, krate, "bstd") || facts::crate_is(db, krate, "blaze_std"))
}

#[cfg(test)]
mod tests {
    use syntax::TextSize;

    use super::{after, before};

    fn at(offset: usize) -> TextSize {
        TextSize::new(offset as u32)
    }

    /// A method linted by its body is the whole text: its first line's indentation is not
    /// in it, and is its closing brace's.
    #[test]
    fn a_mark_at_the_start_of_a_body_s_anchor_takes_the_closing_brace_s_indentation() {
        let method = "pub fn len(&self) -> usize {\n        self.0\n    }";
        let end = at(method.len());
        assert_eq!(before(method, at(0), end, "nopanic"), (at(0), "#[blaze::nopanic]\n    ".to_owned()));
        let marked = "#[blaze::pure]\n    pub fn len(&self) -> usize {\n        self.0\n    }";
        let end = at(marked.len());
        assert_eq!(
            after(marked, at(0), at(14), end, "noalloc"),
            (at(14), "\n    #[blaze::noalloc]".to_owned())
        );
        // On one line there is no brace line to read: the mark goes on the line.
        let one = "fn id(x: u8) -> u8 { x }";
        assert_eq!(before(one, at(0), at(one.len()), "pure"), (at(0), "#[blaze::pure] ".to_owned()));
    }

    /// With the function's own line in the text (a doc comment above it, or a file's text)
    /// the indentation is that line's, as before.
    #[test]
    fn a_mark_inside_the_text_takes_its_line_s_indentation() {
        let text = "/// Doc.\n    pub fn f() {\n    }";
        assert_eq!(
            before(text, at(13), at(text.len()), "pure"),
            (at(13), "#[blaze::pure]\n    ".to_owned())
        );
        let inline = "    #[inline] pub fn f() {\n    }";
        assert_eq!(
            before(inline, at(14), at(inline.len()), "pure"),
            (at(14), "#[blaze::pure] ".to_owned())
        );
    }
}
