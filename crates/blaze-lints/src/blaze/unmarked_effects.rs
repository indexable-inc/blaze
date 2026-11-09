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
//! One finding per function, at its name, under the first of the four lints (in the order
//! pure, nopanic, noalloc, nonblocking) that is on and proved, naming every one that is: two
//! fixes inserting at one place clash in `blaze check --fix` (`fix::overlaps`), so the
//! function has one fix, machine-applicable, that writes each attribute on its own line above
//! the signature, under the doc comments and the attributes already there, at the item's
//! indentation and in that order.

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
    view::{self as ast, AstNode as _, HasName as _, HasVisibility as _},
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

/// The finding of the function `cx` walks the body of.
struct Found {
    /// The first lint on and proved: the one the finding is reported under.
    lint: Lint,
    name: TextRange,
    message: String,
    notes: Vec<String>,
    /// Where the signature starts: the fix is inserted there.
    insert: TextSize,
    fix: String,
    label: String,
}

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>) {
    if !EFFECTS.iter().any(|(lint, _)| cx.on_blaze(*lint)) {
        return;
    }
    let Some(found) = find(cx) else { return };
    let root = cx.root();
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
    diagnostic.help(lint.help()).help(format!("for further information visit {}", lint.docs_url()));
}

fn find<'t>(cx: &Cx<'_, 't>) -> Option<Found> {
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
    // Each effect proved, by its attribute's word, with whether its proof leaves a call open.
    let mut proved: Vec<(Lint, &'static str, bool)> = Vec::new(); // alloc: at most four, once per finding
    for (lint, effect) in EFFECTS {
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
        proved.push((lint, effect.name, proof.open));
    }
    let &(lint, ..) = proved.first()?;
    let name = f.name()?;
    let name_range = cx.range(name.syntax())?;
    let whole = cx.range(root)?;
    let insert = whole.start() + (signature_start(f)? - root.text_range().start());

    let words: Vec<&str> = proved.iter().map(|&(_, word, _)| word).collect(); // alloc: a message's parts
    let attrs: Vec<String> = words.iter().map(|word| format!("`#[blaze::{word}]`")).collect(); // alloc: as above
    let attrs: Vec<&str> = attrs.iter().map(String::as_str).collect(); // alloc: as above
    let relies =
        if lint == Lint::UnmarkedPure { "callers and the cache" } else { "callers" };
    let message = format!(
        "`fn {}` is proved {} through every call it makes, and is not marked {}: marking it lets {relies} rely on it",
        name.text(),
        listed(&words),
        listed(&attrs),
    );
    let mut notes = Vec::new(); // alloc: a finding's notes, at most three, once per finding
    if proved.iter().any(|&(.., open)| open) {
        notes.push(
            "what only an instance decides (a type parameter's method, its drop, a parameter of a type parameter's type) is left to each instance, as it is for a marked generic function"
                .to_owned(), // alloc: a note
        );
    }
    if proved.iter().any(|&(it, ..)| it == Lint::UnmarkedNopanic) {
        notes.push(
            "`nopanic` holds with this crate's overflow checks on, so the mark holds without them too"
                .to_owned(), // alloc: a note
        );
    }
    if proved.len() > 1 {
        let others: Vec<String> =
            proved[1..].iter().map(|&(it, ..)| format!("`blaze::{}`", it.name())).collect(); // alloc: a note's parts
        let others: Vec<&str> = others.iter().map(String::as_str).collect(); // alloc: as above
        notes.push(format!(
            "this one finding also stands for {}: the fix writes every attribute, one per line, in the order pure, nopanic, noalloc, nonblocking",
            listed(&others)
        ));
    }

    // The attributes go where the signature starts: on lines of their own at its indentation
    // when it starts its line, before it on its line otherwise.
    let text = cx.text();
    let at = usize::from(insert).min(text.len());
    let line = &text[text[..at].rfind('\n').map_or(0, |it| it + 1)..at];
    let own_line = line.trim().is_empty();
    let mut fix = String::new(); // alloc: the fix's text
    for word in &words {
        fix.push_str("#[blaze::");
        fix.push_str(word);
        fix.push(']');
        if own_line {
            fix.push('\n');
            fix.push_str(line);
        } else {
            fix.push(' ');
        }
    }
    let label = format!("mark it {}", listed(&attrs));
    Some(Found { lint, name: name_range, message, notes, insert, fix, label })
}

/// `items` as a sentence lists them: "a", "a and b", "a, b and c".
fn listed(items: &[&str]) -> String {
    match items {
        [] => String::new(),
        [one] => (*one).to_owned(), // alloc: a message's part
        [head @ .., last] => format!("{} and {last}", head.join(", ")),
    }
}

/// Where `f`'s signature starts in its tree: past its doc comments and attributes, at its
/// visibility, qualifiers or `fn`.
fn signature_start(f: ast::Fn<'_>) -> Option<TextSize> {
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
