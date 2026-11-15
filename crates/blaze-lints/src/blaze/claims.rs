//! The claim lints (docs/v2/plan/design/claims.md): a promise written in a doc comment is
//! derived by the compiler, checked by an attribute, or points at what proves it; never just
//! asserted. A reader, an agent above all, takes a doc's word; these make the word one the tree
//! holds.
//!
//! - `claim_contradicts_facts`: the doc says an effect the effect walk refuses (`never panics`
//!   on a function with a reachable panic, named with its path), or has a `# Panics` section on
//!   a function proved never to panic.
//! - `claim_is_derivable`: the doc says an effect the walk proves: the attribute states it where
//!   the compiler holds it (the fix writes it and deletes the sentence).
//! - `claim_without_evidence`: a claim neither derived, nor an attribute, nor evidenced, nor
//!   declared `Unverified:` with a reason.
//! - `evidence_is_stale`: an `Evidence:` pointer that resolves to nothing, to what is not a
//!   test, or to a test that does not reach the item.
//! - `safety_without_invariant` (`claims/safety.rs`) and `comment_claims`
//!   (`claims/comments.rs`).
//!
//! Where a doc is read: a function's in its body's pass, where the effect walk's answer for it
//! is; every other item's (a type's `# Invariants`) at its doc comment. How a doc is read is
//! `claims/prose.rs`, pure text in and places out. An evidence pointer names a declaration
//! ([`declared`]): walked module by module from the item's, without asking visibility, through
//! every configuration of the crate, so a private test in a `#[cfg(test)]` module is found from
//! the lib; `doc_names_nothing` leaves the text after `Evidence:` to this module.

mod comments;
mod prose;
mod safety;
#[cfg(test)]
mod tests;

use std::alloc::Global;

use hir_def::{DefWithBodyId, FunctionId, HasModule, attrs::PureAttr};
use hir_ty_mir::mir::overflow::crate_checks_overflow;
use hir_ty_pure::{
    EffectDef, Refusal, Rule,
    effect_attrs::{NOALLOC, NONBLOCKING, nopanic_for},
};
use syntax::{
    SyntaxKind, TextRange, TextSize,
    view::{self as ast, AstNode as _},
};

use self::prose::{Effect, Held, PointerKind, Stated};
use super::{
    Lint, effects,
    std_mistakes::{Fix, report},
    suspicious::doc_names_nothing::{self as names, Owner},
    test_code, unmarked_effects,
    weight::rows::Subject,
};
use crate::{
    check::Cx,
    diag::{self, Applicability},
};

/// The lints of this module.
const LINTS: &[Lint] = &[
    Lint::ClaimContradictsFacts,
    Lint::ClaimIsDerivable,
    Lint::ClaimWithoutEvidence,
    Lint::EvidenceIsStale,
    Lint::SafetyWithoutInvariant,
    Lint::CommentClaims,
];

/// How deep a test's calls are followed into the crate's own code to find the item it is the
/// evidence of: the test, a helper it calls, and one the helper calls.
const REACH_DEPTH: u32 = 2;

/// Every node of a file and of each body: a doc comment that is not a function's with a body,
/// and an `unsafe` block or `unsafe impl`.
pub(super) fn check_node<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if !LINTS.iter().any(|it| cx.on_blaze(*it)) || cx.expansion().is_some() || !cx.is_written(node)
    {
        return;
    }
    safety::check(cx, node);
    if node.kind() != SyntaxKind::DOC_COMMENT {
        return;
    }
    let Some(doc) = ast::DocComment::cast(node) else { return };
    let inner = doc.inner_doc_comment_token().is_some();
    let Some(parent) = node.parent() else { return };
    if first_doc(parent, inner) != Some(node) {
        return;
    }
    // A function with a body is read in its body's pass, where the effect walk's answer is.
    if !inner && ast::Fn::cast(parent).is_some_and(|f| f.body().is_some()) {
        return;
    }
    if test_code::is_test_code(cx, parent) {
        return;
    }
    read_doc(cx, parent, inner, None);
}

/// Once per body, from `blaze.rs`'s body pass: the function's own doc, then its comments.
pub(super) fn check_body(cx: &mut Cx<'_, '_>) {
    if !LINTS.iter().any(|it| cx.on_blaze(*it)) || !cx.in_file() {
        return;
    }
    // The file's own pass (`check::lint_items`) reaches here too, its root the file and no
    // body analysed: its comments are each body's, read in that body's pass, so it reads none.
    let Some(analysis) = cx.analysis() else { return };
    let func = match DefWithBodyId::try_from(analysis.def()) {
        Ok(DefWithBodyId::FunctionId(it)) => Some(it),
        _ => None,
    };
    let root = cx.root();
    if test_code::is_test_code(cx, root) {
        return;
    }
    if ast::Fn::cast(root).is_some()
        && let Some(func) = func
    {
        read_doc(cx, root, false, Some(func));
    }
    comments::check(cx);
}

/// The first doc comment of `parent` of the kind `inner` says, which its doc is read from.
fn first_doc(parent: ast::Node<'_>, inner: bool) -> Option<ast::Node<'_>> {
    parent.children().find(|it| {
        ast::DocComment::cast(*it).is_some_and(|it| it.inner_doc_comment_token().is_some() == inner)
    })
}

/// One line of a doc: its text past `///` and one space, where that text starts in
/// [`Cx::text`], and its comment.
#[derive(Clone, Copy)]
struct Line<'q, 't> {
    text: &'q str,
    base: TextSize,
    comment: ast::Node<'t>,
}

/// The lines of `parent`'s doc of the kind `inner` says.
fn lines_of<'q, 't>(cx: &Cx<'q, 't>, parent: ast::Node<'t>, inner: bool) -> Vec<Line<'q, 't>, Global> {
    let mut lines = Vec::new_in(Global);
    for comment in parent.children() {
        let Some(doc) = ast::DocComment::cast(comment) else { continue };
        if doc.inner_doc_comment_token().is_some() != inner {
            continue;
        }
        let Some(range) = cx.range(comment) else { continue };
        let text = cx.snippet(range);
        let Some(body) = text.strip_prefix("///").or_else(|| text.strip_prefix("//!")) else {
            continue;
        };
        let line = body.strip_prefix(' ').unwrap_or(body);
        let base = range.start() + TextSize::of(&text[..text.len() - line.len()]);
        lines.push(Line { text: line, base, comment });
    }
    lines
}

/// Where `len` bytes from `at` of `lines` are in [`Cx::text`].
fn range_at(lines: &[Line<'_, '_>], at: prose::At, len: usize) -> TextRange {
    let line = lines[at.line];
    let start = at.byte.min(line.text.len());
    let end = (start + len).min(line.text.len());
    TextRange::at(line.base + TextSize::of(&line.text[..start]), TextSize::of(&line.text[start..end]))
}

/// Reads the doc of `parent` (of `func`, when it is a function read in its body's pass) and
/// reports what it claims that the tree does not hold.
fn read_doc<'t>(cx: &mut Cx<'_, 't>, parent: ast::Node<'t>, inner: bool, func: Option<FunctionId>) {
    let Some(first) = first_doc(parent, inner) else { return };
    let Some(owner) = names::owner_of(cx, first, parent, inner) else { return };
    let lines = lines_of(cx, parent, inner);
    let mut texts = Vec::with_capacity_in(lines.len(), Global);
    texts.extend(lines.iter().map(|it| it.text));
    let doc = prose::read(&texts);
    let what = names::owner_name(cx, owner);
    let is_inner = if inner { hir::IsInnerDoc::Yes } else { hir::IsInnerDoc::No };
    if let Some(func) = func {
        for stated in &doc.stated {
            stated_effects(cx, parent, &lines, func, &what, stated);
        }
        if let Some(line) = doc.panics {
            panics_section(cx, &lines, func, &what, line);
        }
    }
    for item in &doc.items {
        // An effect the item states is held by the attribute when the walk proves it: the
        // item's evidence is then the attribute's, said by `claim_is_derivable`.
        let mut derived = !item.effects.is_empty();
        if let Some(func) = func {
            for effect in &item.effects {
                derived &= item_effect(cx, parent, &lines, func, &what, *effect, item.at);
            }
        } else {
            derived = false;
        }
        held(cx, owner, is_inner, &lines, &what, item, derived);
    }
}

/// What the effect walk says of `effect` for `func`.
enum Verdict {
    /// Proved through every call, for every profile the crate builds with.
    Proved,
    /// Refused by what the code does: a reachable panic, an allocation, a blocking call.
    Refused(Refusal),
    /// Not decided: what the walk can't follow, or a profile it can't know.
    Undecided(&'static str),
}

/// The effect definition the walk checks `effect` against, for `func`'s crate.
fn effect_def(db: &base_db::Db, func: FunctionId, effect: Effect) -> (&'static EffectDef, Option<bool>) {
    let overflow = crate_checks_overflow(db, HasModule::krate(&func, db));
    let def = match effect {
        Effect::Panic => nopanic_for(overflow == Some(true)),
        Effect::Alloc => &NOALLOC,
        Effect::Block => &NONBLOCKING,
        Effect::Pure => &hir_ty_pure::PURE,
    };
    (def, overflow)
}

fn verdict(db: &base_db::Db, func: FunctionId, effect: Effect) -> Verdict {
    let (def, overflow) = effect_def(db, func, effect);
    match hir_ty_pure::check_generic(db, func, def) {
        Ok(_) if effect == Effect::Panic && overflow != Some(true) => Verdict::Undecided(
            "it is proved only without overflow checks, and this crate's profile keeps none or is not known",
        ),
        Ok(_) => Verdict::Proved,
        // What the code itself does: each of these is an effect found, not one unfollowed.
        Err(refusal)
            if matches!(
                refusal.rule,
                Rule::Panic
                    | Rule::Overflow
                    | Rule::Alloc
                    | Rule::AllocIn
                    | Rule::Block
                    | Rule::Io
                    | Rule::Env
                    | Rule::Clock
                    | Rule::Thread
                    | Rule::MutableState
                    | Rule::Address
                    | Rule::Random
            ) =>
        {
            Verdict::Refused(refusal)
        }
        Err(_) => Verdict::Undecided("the effect walk can't follow all of it"),
    }
}

/// Whether `func` is marked with the attribute of `effect`, and how.
fn marking(db: &base_db::Db, func: FunctionId, effect: Effect) -> Option<PureAttr> {
    effects::marked(db, func, effect_def(db, func, effect).0)
}

/// The edit that writes `#[blaze::<attr>]` on the function `parent`, where `unmarked_*` writes it.
fn attribute_fix(cx: &Cx<'_, '_>, parent: ast::Node<'_>, effect: Effect) -> Option<(TextRange, String)> {
    let f = ast::Fn::cast(parent)?;
    let whole = cx.range(parent)?;
    let in_text = |at: TextSize| whole.start() + (at - parent.text_range().start());
    let signature = unmarked_effects::signature_start(f)?;
    let (at, text) = unmarked_effects::before(cx.text(), in_text(signature), whole.end(), effect.attribute());
    Some((TextRange::empty(at), text))
}

/// The edit that deletes the sentence of `stated` when it says nothing else: its whole line,
/// with the next line's indentation (the deleted line's stays), when it is the line.
fn deletion(cx: &Cx<'_, '_>, lines: &[Line<'_, '_>], stated: &Stated) -> Option<(TextRange, String)> {
    let (at, from, to) = stated.alone?;
    let line = lines[at];
    if line.text[..from].trim().is_empty() && to == line.text.len() {
        let comment = cx.range(line.comment)?;
        let text = cx.text();
        let end = usize::from(comment.end());
        let rest = &text[end..];
        let newline = rest.strip_prefix('\n')?;
        let indent = newline.len() - newline.trim_start_matches([' ', '\t']).len();
        let stop = comment.end() + TextSize::of(&rest[..1 + indent]);
        return Some((TextRange::new(comment.start(), stop), String::new()));
    }
    // A sentence among others: it and the space before it.
    let start = if line.text[..from].ends_with(' ') { from - 1 } else { from };
    let range = TextRange::new(
        line.base + TextSize::of(&line.text[..start]),
        line.base + TextSize::of(&line.text[..to]),
    );
    Some((range, String::new()))
}

/// The effects one sentence of a function's prose states.
fn stated_effects<'t>(
    cx: &mut Cx<'_, 't>,
    parent: ast::Node<'t>,
    lines: &[Line<'_, 't>],
    func: FunctionId,
    what: &str,
    stated: &Stated,
) {
    let db = cx.db();
    let at = range_at(lines, stated.at, stated.len);
    let comment = lines[stated.at.line].comment;
    let mut proved = Vec::new_in(Global);
    let mut undecided = Vec::new_in(Global);
    for &effect in &stated.effects {
        let marked = marking(db, func, effect);
        match verdict(db, func, effect) {
            Verdict::Proved => proved.push((effect, marked.is_some())),
            // A marked function's refusal is its attribute's lint's to say.
            Verdict::Refused(_) if marked.is_some() => {}
            Verdict::Refused(refusal) => contradicts(cx, comment, at, func, what, effect, &refusal),
            // A trusted or checked mark holds what the walk can't: the ledger lists it.
            Verdict::Undecided(_) if marked.is_some() => {}
            Verdict::Undecided(why) => undecided.push((effect, why)),
        }
    }
    if !proved.is_empty() {
        let mut parts = Vec::new_in(Global);
        for (effect, marked) in &proved {
            if !marked && let Some(part) = attribute_fix(cx, parent, *effect) {
                parts.push(part);
            }
        }
        // The sentence goes only when the attributes say all of it.
        if proved.len() == stated.effects.len()
            && let Some(part) = deletion(cx, lines, stated)
        {
            parts.push(part);
        }
        let attrs = attributes(proved.iter().map(|(effect, _)| *effect));
        let all_marked = proved.iter().all(|(_, marked)| *marked);
        let message = if all_marked {
            format!(
                "the doc of {what} says it {}, which {attrs} on it already holds: the sentence repeats the attribute",
                said(proved.iter().map(|(effect, _)| *effect)),
            )
        } else {
            format!(
                "the doc of {what} says it {}, and the effect walk proves it through every call: {attrs} states it where the compiler holds it",
                said(proved.iter().map(|(effect, _)| *effect)),
            )
        };
        let note = "an attribute is checked on every build; a sentence is believed until the code changes under it";
        let fix = (!parts.is_empty()).then(|| Fix {
            message: if all_marked { "delete the sentence" } else { "write the attribute and delete the sentence" },
            parts: parts.into_iter().collect(),
            applicability: Applicability::MachineApplicable,
        });
        report(cx, Lint::ClaimIsDerivable, comment, at, message, note, fix);
    }
    if let Some((_, why)) = undecided.first() {
        let message = format!(
            "the doc of {what} says it {}, which the effect walk can't decide ({why}), and the doc points at nothing that proves it",
            said(undecided.iter().map(|(effect, _)| *effect)),
        );
        let note = "list it under `# Guarantees` ending in `Evidence: [`a_test`]` or `Evidence: Lean `Theorem``, or `Unverified: why`, so a reader can tell a held promise from a hope";
        report(cx, Lint::ClaimWithoutEvidence, comment, at, message, note, None);
    }
}

/// An effect an item of `# Guarantees` states: whether the attribute holds it (proved).
fn item_effect<'t>(
    cx: &mut Cx<'_, 't>,
    parent: ast::Node<'t>,
    lines: &[Line<'_, 't>],
    func: FunctionId,
    what: &str,
    effect: Effect,
    at: prose::At,
) -> bool {
    let db = cx.db();
    let marked = marking(db, func, effect).is_some();
    let range = range_at(lines, at, lines[at.line].text.len().saturating_sub(at.byte));
    let comment = lines[at.line].comment;
    match verdict(db, func, effect) {
        Verdict::Proved => {
            let message = if marked {
                format!("{what} is `#[blaze::{}]`, which holds this guarantee: the item repeats it", effect.attribute())
            } else {
                format!(
                    "this guarantee of {what}, that it {}, is proved by the effect walk: `#[blaze::{}]` holds it on every build",
                    effect.said(),
                    effect.attribute()
                )
            };
            let note = "delete the item once the attribute states it";
            let fix = (!marked).then(|| attribute_fix(cx, parent, effect)).flatten().map(|part| {
                let mut parts = Vec::new_in(Global);
                parts.push(part);
                Fix {
                    message: "write the attribute",
                    parts: parts.into_iter().collect(),
                    applicability: Applicability::MachineApplicable,
                }
            });
            report(cx, Lint::ClaimIsDerivable, comment, range, message, note, fix);
            true
        }
        Verdict::Refused(_) if marked => false,
        Verdict::Refused(refusal) => {
            contradicts(cx, comment, range, func, what, effect, &refusal);
            false
        }
        Verdict::Undecided(_) => marked,
    }
}

/// `claim_contradicts_facts` of an effect the doc says and the walk refuses, with the chain.
fn contradicts<'t>(
    cx: &mut Cx<'_, 't>,
    comment: ast::Node<'t>,
    at: TextRange,
    func: FunctionId,
    what: &str,
    effect: Effect,
    refusal: &Refusal,
) {
    let lint = Lint::ClaimContradictsFacts;
    let db = cx.db();
    let notes = effects::step_notes(db, HasModule::krate(&func, db), refusal);
    let message = format!("the doc of {what} says it {}, but {}", effect.said(), refusal.said());
    let span = cx.span(at);
    let diagnostic = cx.blaze(lint, comment, span, message);
    diagnostic.note(format!(
        "the doc is wrong or the code is: make the code hold it and mark it `#[blaze::{}]`, or say what it does",
        effect.attribute()
    ));
    for (file, range, text) in notes {
        diagnostic.span_note(diag::of_file(file, range), text);
    }
    diagnostic.help(lint.help()).help(format!("for further information visit {}", lint.docs_url()));
}

/// A `# Panics` section on a function proved never to panic, in every profile its crate builds.
fn panics_section<'t>(
    cx: &mut Cx<'_, 't>,
    lines: &[Line<'_, 't>],
    func: FunctionId,
    what: &str,
    line: usize,
) {
    if !matches!(verdict(cx.db(), func, Effect::Panic), Verdict::Proved) {
        return;
    }
    let at = range_at(lines, prose::At { line, byte: 0 }, lines[line].text.len());
    let message = format!(
        "{what} has a `# Panics` section, but the effect walk proves it never panics (with overflow checks, so in every profile)"
    );
    let note = "say what the section means another way, or delete it and mark the function `#[blaze::nopanic]`";
    report(cx, Lint::ClaimContradictsFacts, lines[line].comment, at, message, note, None);
}

/// How an item of `# Guarantees` or `# Invariants` says it is held, and whether that holds.
fn held<'t>(
    cx: &mut Cx<'_, 't>,
    owner: Owner,
    inner: hir::IsInnerDoc,
    lines: &[Line<'_, 't>],
    what: &str,
    item: &prose::Item<'_>,
    derived: bool,
) {
    let comment = lines[item.at.line].comment;
    let whole = range_at(lines, item.at, lines[item.at.line].text.len().saturating_sub(item.at.byte));
    let note = "end the claim with `Evidence: [`a_test`]` (a test or oracle that calls the item), `Evidence: Lean `Theorem``, or `Unverified: why`";
    let lint = Lint::ClaimWithoutEvidence;
    match &item.held {
        Held::Nothing if derived => {}
        Held::Nothing => {
            let message =
                format!("this claim of {what} in `# {}` says nothing holds it: \"{}\"", item.section.heading(), item.claim);
            report(cx, lint, comment, whole, message, note, None);
        }
        Held::Misspelled(spelled, at) => {
            let marker = if spelled.to_ascii_lowercase().starts_with("unverified") { "Unverified:" } else { "Evidence:" };
            let range = range_at(lines, *at, spelled.len());
            let mut parts = Vec::new_in(Global);
            parts.push((range, marker.to_owned()));
            let fix = Fix {
                message: "write the one marker",
                parts: parts.into_iter().collect(),
                applicability: Applicability::MaybeIncorrect,
            };
            let message = format!(
                "this claim of {what} writes `{spelled}` where the one marker is `{marker}`, which the ledger and this lint read"
            );
            report(cx, lint, comment, whole, message, note, Some(fix));
        }
        Held::Empty(marker, at) => {
            let lack = if *marker == "Unverified:" { "gives no reason" } else { "names nothing" };
            let message = format!("this claim of {what} has `{marker}` that {lack}");
            let range = range_at(lines, *at, marker.len());
            report(cx, lint, comment, range, message, note, None);
        }
        Held::Unverified => {}
        Held::Evidence(pointers) => {
            for pointer in pointers {
                let range = range_at(lines, pointer.at, pointer.text.len());
                if let Some(problem) = stale(cx, owner, inner, pointer.kind, pointer.text) {
                    let lint = Lint::EvidenceIsStale;
                    let message = format!("the evidence `{}` of this claim of {what} {problem}", pointer.text);
                    let note = "point at a test or oracle that calls the item (by its path), or at a Lean theorem by its name";
                    report(cx, lint, comment, range, message, note, None);
                }
            }
        }
    }
}

/// What is wrong with an evidence pointer, if anything.
fn stale(cx: &Cx<'_, '_>, owner: Owner, inner: hir::IsInnerDoc, kind: PointerKind, text: &str) -> Option<String> {
    match kind {
        // A theorem's name is checked against `lean/theorems.json` by the ledger, which reads it.
        PointerKind::Lean => return None,
        PointerKind::Bad => {
            return Some(
                "is not a pointer: write `` [`path::to::test`] `` or ``Lean `Name` ``".to_owned(),
            );
        }
        PointerKind::Rust => {}
    }
    let db = cx.db();
    let Some(target) = names::declared(db, owner, inner, text) else {
        return Some(format!("resolves to nothing from {}", names::owner_name(cx, owner)));
    };
    let hir::ModuleDef::Function(test) = target else {
        return Some("is not a function: evidence is a test, an oracle, or a theorem".to_owned());
    };
    if !is_test(db, test) {
        return Some("is not a test: a function the item's claim rests on is not evidence of it".to_owned());
    }
    if !test.has_body(db) {
        return None;
    }
    let analysis = hir::BodyAnalysis::new(db, hir::DefWithBody::from(test));
    match reaches(cx, &analysis, owner, REACH_DEPTH) {
        Some(false) => Some(format!(
            "does not reach {}: no call it makes, {REACH_DEPTH} calls deep into the crate, calls it or makes a value of it",
            names::owner_name(cx, owner)
        )),
        _ => None,
    }
}

/// Whether `test` is test code: a `#[test]`, or a function under `#[cfg(test)]`.
fn is_test(db: &base_db::Db, test: hir::Function) -> bool {
    use hir::HasAttrs as _;
    test.is_test(db)
        || hir_def::FunctionId::try_from(test)
            .is_ok_and(|id| test_code::is_test_def(db, DefWithBodyId::FunctionId(id)))
        || test.attrs(db).cfgs(db).is_some_and(test_code::names_test)
}

/// Whether the body `analysis` reaches `owner` by its static calls, `depth` levels into the
/// crate's own code: a call of the function, or a value of the type. `None` for an owner whose
/// reach is not checked (a trait, a module, an impl, a constant).
///
/// The test is of the crate's test configuration and the item may be of its lib, whose
/// definition ids differ: the item is matched by where it is written ([`Subject`]), the same
/// in every configuration.
fn reaches(cx: &Cx<'_, '_>, analysis: &hir::BodyAnalysis<'_>, owner: Owner, depth: u32) -> Option<bool> {
    let db = cx.db();
    let (target, adt) = match owner {
        Owner::Def(hir::ModuleDef::Function(it)) => (Subject::of_fn(db, FunctionId::try_from(it).ok()?)?, false),
        Owner::Def(hir::ModuleDef::Adt(it)) => (Subject::of_adt(db, hir_def::AdtId::from(it))?, true),
        _ => return None,
    };
    let root = cx.krate().root_file(db);
    Some(reaches_subject(db, analysis, target, adt, root, depth))
}

/// [`reaches`] of the item written at `target` (a type when `adt`), following calls into the
/// crates whose root file is `root`.
fn reaches_subject(
    db: &base_db::Db,
    analysis: &hir::BodyAnalysis<'_>,
    target: Subject,
    adt: bool,
    root: span::FileId,
    depth: u32,
) -> bool {
    let body = analysis.body();
    let infer = analysis.infer();
    for (id, expr) in body.exprs() {
        if adt
            && analysis
                .ty(infer.expr_ty(id))
                .strip_references()
                .as_adt()
                .and_then(|it| Subject::of_adt(db, hir_def::AdtId::from(it)))
                == Some(target)
        {
            return true;
        }
        if !matches!(expr, hir_def::hir::Expr::Call { .. } | hir_def::hir::Expr::MethodCall { .. }) {
            continue;
        }
        let Some((callee, _)) = analysis.callee(id) else { continue };
        if !adt && Subject::of_fn(db, callee) == Some(target) {
            return true;
        }
        let callee = hir::Function::from(callee);
        if depth > 0 && callee.module(db).krate(db).root_file(db) == root && callee.has_body(db) {
            let inner = hir::BodyAnalysis::new(db, hir::DefWithBody::from(callee));
            if reaches_subject(db, &inner, target, adt, root, depth - 1) {
                return true;
            }
        }
    }
    false
}

/// `#[blaze::a]` and `#[blaze::b]`, of `effects`.
fn attributes(effects: impl Iterator<Item = Effect>) -> String {
    let mut out = String::new();
    for (at, effect) in effects.enumerate() {
        if at > 0 {
            out.push_str(" and ");
        }
        out.push_str("`#[blaze::");
        out.push_str(effect.attribute());
        out.push_str("]`");
    }
    out
}

/// `never panics and does not allocate`, of `effects`.
fn said(effects: impl Iterator<Item = Effect>) -> String {
    let mut out = String::new();
    for (at, effect) in effects.enumerate() {
        if at > 0 {
            out.push_str(" and ");
        }
        out.push_str(effect.said());
    }
    out
}
