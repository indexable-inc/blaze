//! `undeclared_effect`, `unfulfilled_effect` and `negative_effect`: the lints of
//! `#[blaze::expect(..)]`, which says what a function is expected to do
//! (docs/v2/plan/design/effects.md): `panic`, `alloc`, `block`, `diverge`. A word a list leaves
//! out is an effect the function does not have, checked through every call it makes; a word a
//! function's own list has is an effect it has, as Rust's `#[expect(lint)]` is fulfilled or
//! reported.
//!
//! - `undeclared_effect` (`deny`; it fires only where something is declared): a function with
//!   an effect its allowance leaves out, one finding at its name that says each such effect
//!   with its witness (the call chain down to what does it, or the loop's line), and one fix
//!   that writes the list with the words added. The allowance is the function's own list, else
//!   its trait method's (or trait's), else the `#![blaze::expect(..)]` of the nearest module
//!   around it, else nothing in a crate with `#![blaze::effects]`
//!   (`hir_ty_pure::expect::declared`); a function none of these reach is not looked at, so a
//!   crate that asks for nothing is linted as before. Also here: an impl's method whose list
//!   has a word its trait's method does not allow, a list that is not one, and the deleted
//!   spelling `#[blaze::may(..)]`, refused wherever it is written with the fix that respells
//!   it.
//! - `unfulfilled_effect` (`deny`): a word of a function's own list, held exact
//!   (`hir_ty_pure::expect::Marks::exact`), for an effect it is proved not to have, private
//!   and public alike, with the fix that takes the word off. A trait method's list, a list
//!   inherited from the trait or a default, a `trusted` list and a word the walk can neither
//!   prove nor refute are bounds, never reported.
//! - `negative_effect` (`warn`): `#[blaze::nopanic]`, `#[blaze::noalloc]` or
//!   `#[blaze::nonblocking]` on a function effects are declared around (a module's default, or
//!   the crate's gate). The old spellings still mean what they meant; the fix writes the list
//!   they stand for: every word they do not take off that the function is not proved free of.
//!
//! What they read: `hir_ty_pure::check_generic` of the definition for each word's effect, the
//! query the old attributes' lints ask, so a refusal here is the one `#[blaze::nopanic]` would
//! get. `panic` is held to the profile the crate is built with (its overflow checks), and
//! taken off a list, or left out of a rewritten one, only where the crate keeps them, so the
//! list holds in dev and in release.

use base_db::Db;
use hir_def::{
    DefWithBodyId, FunctionId, HasModule,
    attrs::{EffectWord, ExpectWord, PureAttr},
    signatures::FunctionSignature,
};
use hir_ty_mir::mir::overflow::crate_checks_overflow;
use hir_ty_pure::{
    Refusal,
    effect_attrs::of_word,
    expect::{self, BoundFrom, Declared, DefaultFrom, Verdict, Words},
};
use syntax::{
    NodeOrToken, SyntaxKind, TextRange, TextSize,
    view::{self as ast, AstNode as _, HasAttrs as _, HasName as _},
};

use super::{Lint, effects};
use crate::{
    check::Cx,
    diag::{self, Applicability},
};

#[cfg(test)]
thread_local! {
    /// The planted bug of `unfulfilled_effect`'s control: the rule before lists were exact,
    /// which held only a private function's list (`expect_tests`).
    pub(super) static PRIVATE_ONLY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// One finding of the function `cx` walks the body of.
struct Found {
    lint: Lint,
    message: String,
    notes: Vec<String>,
    steps: Vec<effects::StepNote>,
    /// The fix's parts: what is at each range of the text becomes the string.
    fix: Vec<(TextRange, String)>,
    label: &'static str,
}

pub(super) fn check(cx: &mut Cx<'_, '_>) {
    let lints = [Lint::UndeclaredEffect, Lint::UnfulfilledEffect, Lint::NegativeEffect];
    if !lints.iter().any(|&lint| cx.on_blaze(lint)) {
        return;
    }
    let Some((name, found)) = find(cx) else { return };
    for found in found {
        let root = cx.root();
        let span = cx.span(name);
        let parts: Vec<_> =
            found.fix.into_iter().map(|(range, text)| (cx.span(range), text)).collect(); // alloc: the fix's parts, few
        let lint = found.lint;
        let diagnostic = cx.blaze(lint, root, span, found.message);
        for note in found.notes {
            diagnostic.note(note);
        }
        for (file, range, text) in found.steps {
            diagnostic.span_note(diag::of_file(file, range), text);
        }
        if !parts.is_empty() {
            diagnostic.multipart_suggestion(found.label, parts, Applicability::MachineApplicable);
        }
        diagnostic
            .help(lint.help())
            .help(format!("for further information visit {}", lint.docs_url()));
    }
}

/// The function as the three lints see it.
struct Site<'a> {
    db: &'a Db,
    func: FunctionId,
    krate: base_db::Crate,
    name: String,
    declared: Declared,
    /// Where its own `#[blaze::expect(..)]` is in the text.
    expect_at: Option<TextRange>,
    /// Its old spellings, each where it is in the text.
    negatives_at: Vec<(EffectWord, TextRange)>,
    /// Where its signature starts in the text.
    signature: TextSize,
    text: &'a str,
    in_test: bool,
    /// The words of its exact list it is proved not to have, when `unfulfilled_effect` is on:
    /// what a fix of the list leaves out.
    unfulfilled: Words,
}

/// Where the function's name is, and the findings of the lints that are on, in their order.
fn find<'q>(cx: &Cx<'q, '_>) -> Option<(TextRange, Vec<Found>)> {
    let root = cx.root();
    let f = ast::Fn::cast(root)?;
    let analysis = cx.analysis()?;
    let Ok(DefWithBodyId::FunctionId(func)) = DefWithBodyId::try_from(analysis.def()) else {
        return None;
    };
    let db = cx.db();
    // A trait's method with no body is a declaration: each impl's method is checked in its
    // place, within the list written here.
    if !FunctionSignature::of(db, func).has_body() {
        return None;
    }
    let declared = expect::declared(db, func);
    // Nothing declared on it or around it: it may do anything, and nothing is asked.
    if declared.marks == expect::Marks::default() && declared.expect.is_none() {
        return None;
    }
    let name = f.name()?;
    let name_at = cx.range(name.syntax())?;
    let whole = cx.range(root)?;
    // A place of the function's tree in the text the fix is made in.
    let at = |it: TextSize| whole.start() + (it - root.text_range().start());
    let in_text = |range: TextRange| TextRange::new(at(range.start()), at(range.end()));
    let mut site = Site {
        db,
        func,
        krate: HasModule::krate(&func, db),
        name: name.text().to_string(), // alloc: a message's part
        expect_at: written(f, "expect").map(in_text),
        negatives_at: EffectWord::ALL
            .into_iter()
            .filter_map(|word| Some((word, in_text(written(f, word.text())?))))
            .collect(), // alloc: at most three
        signature: at(signature_start(f)?),
        text: cx.text(),
        in_test: cx.is_in_test(root),
        declared,
        unfulfilled: Words::NONE,
    };
    if cx.on_blaze(Lint::UnfulfilledEffect) {
        site.unfulfilled = site.unfulfilled_words();
    }
    #[cfg(test)]
    if PRIVATE_ONLY.get() && syntax::view::HasVisibility::visibility(&f).is_some() {
        site.unfulfilled = Words::NONE;
    }
    let mut found = Vec::new(); // alloc: at most three findings
    let refused = cx.on_blaze(Lint::UndeclaredEffect).then(|| undeclared(&site)).flatten();
    // One fix per list: where both lints fire, `undeclared_effect`'s writes the list with the
    // missing words added and the unfulfilled ones taken off.
    let fixed = refused.as_ref().is_some_and(|it| !it.fix.is_empty());
    found.extend(refused);
    if cx.on_blaze(Lint::UnfulfilledEffect) {
        found.extend(unfulfilled(&site, fixed));
    }
    if cx.on_blaze(Lint::NegativeEffect) {
        found.extend(negative(&site));
    }
    Some((name_at, found))
}

/// What a word allows, as a message says it after "may".
fn may_do(word: ExpectWord) -> &'static str {
    match word {
        ExpectWord::Panic => "panic",
        ExpectWord::Alloc => "allocate from the global heap",
        ExpectWord::Block => "block the thread",
        ExpectWord::Diverge => "not return (`diverge`)",
    }
}

/// The words in backticks: "`panic`, `alloc`".
fn ticked(words: Words) -> String {
    words.iter().map(|it| format!("`{}`", it.text())).collect::<Vec<_>>().join(", ") // alloc: a message's part
}

impl Site<'_> {
    /// Whether the crate keeps overflow checks; the strict answer where that is not known.
    fn overflow_checks(&self) -> bool {
        crate_checks_overflow(self.db, self.krate).unwrap_or(true)
    }

    /// Why the function has the effect `word` allows, if it may.
    fn has(&self, word: ExpectWord, overflow_checks: bool) -> Option<Refusal> {
        hir_ty_pure::check_generic(self.db, self.func, of_word(word, overflow_checks)).err()
    }

    /// Whether the function is proved free of what `word` allows, with every overflow check
    /// on and no call left to an instance: the word can go, in any profile.
    fn proved_free_of(&self, word: ExpectWord) -> bool {
        if word == ExpectWord::Panic && crate_checks_overflow(self.db, self.krate) != Some(true) {
            return false;
        }
        hir_ty_pure::check_generic(self.db, self.func, of_word(word, true))
            .is_ok_and(|proof| !proof.open)
    }

    /// The words of its own list, held exact, it is proved not to have.
    fn unfulfilled_words(&self) -> Words {
        let marks = &self.declared.marks;
        let Some(own) = marks.own else { return Words::NONE };
        if !marks.exact || self.expect_at.is_none() {
            return Words::NONE;
        }
        let free = own
            .iter()
            .filter(|&word| self.proved_free_of(word))
            .fold(Words::NONE, |words, word| words.union(Words::one(word)));
        expect::unfulfilled(marks, free)
    }

    /// What the allowance is written as, for a message.
    fn allowance(&self) -> String {
        let marks = &self.declared.marks;
        let list = |words: Option<Words>| words.unwrap_or_default().list();
        if marks.own.is_some() {
            return format!("its `#[blaze::expect({})]`", list(marks.own)); // alloc: a message's part
        }
        match (self.declared.bound_from, self.declared.default_from) {
            (Some(BoundFrom::Method(_)), _) => format!(
                "the `#[blaze::expect({})]` of the trait's method it implements",
                list(marks.bound)
            ), // alloc: as above
            (Some(BoundFrom::Trait(_)), _) => {
                format!("the `#[blaze::expect({})]` of its trait", list(marks.bound)) // alloc: as above
            }
            (None, Some(DefaultFrom::Module(_))) => format!(
                "the default of a module around it, `#![blaze::expect({})]`",
                list(marks.default)
            ), // alloc: as above
            _ => "`#![blaze::effects]` (a function with no `#[blaze::expect(..)]` may have no effect)"
                .to_owned(), // alloc: as above
        }
    }

    /// The fix that makes the function's list `words`: its own list rewritten, or a new one
    /// above its signature.
    fn write(&self, words: Words) -> Vec<(TextRange, String)> {
        match self.expect_at {
            Some(at) => vec![(at, words.attribute())], // alloc: the fix's one part
            None => {
                let (at, text) = before(self.text, self.signature, &words.attribute());
                vec![(TextRange::empty(at), text)] // alloc: the fix's one part
            }
        }
    }

}

/// A finding of `lint` with no note and no fix yet.
fn finding(lint: Lint, message: String) -> Found {
    Found { lint, message, notes: Vec::new(), steps: Vec::new(), fix: Vec::new(), label: "" }
}

/// `undeclared_effect` of the function.
fn undeclared(site: &Site<'_>) -> Option<Found> {
    let declared = &site.declared;
    let name = &site.name;
    if let Some(text) = declared.expect.as_ref().and_then(|it| it.malformed.as_ref()) {
        return Some(finding(
            Lint::UndeclaredEffect,
            format!(
                "`#[blaze::expect{}]` on `{name}`: the list is words of `panic`, `alloc`, `block` and `diverge` separated by commas, each once, then at most `trusted = \"why\"`",
                text.as_str()
            ),
        ));
    }
    if declared.expect.is_some() && declared.has_negative() {
        return Some(finding(
            Lint::UndeclaredEffect,
            format!(
                "`fn {name}` says what it is expected to do (`#[blaze::expect(..)]`) and what it may not ({}): write the list alone, without the words the old spelling takes off",
                ticked_negatives(declared)
            ),
        ));
    }
    // An old spelling alone is its own lint's (`blaze::nopanic`, ...), as before.
    if declared.expect.is_none() && declared.has_negative() {
        return None;
    }
    let marks = &declared.marks;
    if let Verdict::BeyondBound(beyond) = expect::verdict(marks, Words::NONE) {
        let mut found = finding(
            Lint::UndeclaredEffect,
            format!(
                "`fn {name}` lists {} in its `#[blaze::expect(..)]`, which the trait's method it implements does not allow (`#[blaze::expect({})]`): an impl may declare fewer effects than its trait's method, never more",
                ticked(beyond),
                marks.bound.unwrap_or_default().list()
            ),
        );
        found.fix = site.write(marks.own.unwrap_or_default().without(beyond).without(site.unfulfilled));
        found.label = "list only what the trait's method allows";
        return Some(found);
    }
    let allowed = expect::allowed(marks)?;
    // A list taken at its word is not checked; a default does not reach test code.
    let trusted = declared.expect.as_ref().is_some_and(|it| it.trusted.is_some());
    let by_default = marks.own.is_none() && marks.bound.is_none();
    if trusted || (by_default && site.in_test) {
        return None;
    }
    let overflow_checks = site.overflow_checks();
    let refused: Vec<(ExpectWord, Refusal)> = ExpectWord::ALL
        .into_iter()
        .filter(|&word| !allowed.has(word))
        .filter_map(|word| Some((word, site.has(word, overflow_checks)?)))
        .collect(); // alloc: at most four refusals, once per function that has one
    let has = refused.iter().fold(Words::NONE, |has, (word, _)| has.union(Words::one(*word)));
    let Verdict::Undeclared(missing) = expect::verdict(marks, allowed.union(has)) else {
        return None;
    };
    let (first, refusal) = refused.first()?;
    let does: Vec<&str> = missing.iter().map(may_do).collect(); // alloc: a message's parts
    let mut found = finding(
        Lint::UndeclaredEffect,
        format!(
            "`fn {name}` may {}, which {} does not allow: it {}: {}; add `{}` to its `#[blaze::expect(..)]`, or handle it",
            does.join(", and may "),
            site.allowance(),
            refusal.says,
            refusal.witness(),
            first.text()
        ),
    );
    for (word, refusal) in &refused[1..] {
        found.notes.push(format!("`{}`: it {}: {}", word.text(), refusal.says, refusal.witness())); // alloc: a note
    }
    if matches!(first, ExpectWord::Panic) {
        let profile = if overflow_checks { "with" } else { "without" };
        found.notes.push(format!("`panic` is checked as this crate builds: {profile} overflow checks")); // alloc: a note
    }
    found.steps = effects::step_notes(site.db, site.krate, refusal);
    // The list with the words added, where the trait's method allows them, and any it is
    // proved not to have taken off.
    let wanted = allowed.union(missing).without(site.unfulfilled);
    if marks.bound.is_none_or(|bound| wanted.within(bound)) {
        found.fix = site.write(wanted);
        found.label = "list what it may do";
    }
    Some(found)
}

/// Why the function is known not to do what `word` allows, as a message says it.
fn known_not(word: ExpectWord) -> &'static str {
    match word {
        ExpectWord::Panic => "nothing in it panics or aborts, through every call it makes",
        ExpectWord::Alloc => {
            "no call or operation in it allocates from the global heap, through every call it makes"
        }
        ExpectWord::Block => "no call in it blocks the thread, through every call it makes",
        ExpectWord::Diverge => {
            "it returns: every loop in it is known to end and nothing it calls calls it again"
        }
    }
}

/// `unfulfilled_effect` of the function: each word of its exact list it is proved not to have.
/// `fixed`: `undeclared_effect` already writes the list, these words taken off.
fn unfulfilled(site: &Site<'_>, fixed: bool) -> Option<Found> {
    let unfulfilled = site.unfulfilled;
    let own = site.declared.marks.own?;
    if unfulfilled.is_empty() {
        return None;
    }
    let why: Vec<String> = unfulfilled
        .iter()
        .map(|word| format!("`{}`: {}", word.text(), known_not(word)))
        .collect(); // alloc: a message's parts, at most four
    let mut found = finding(
        Lint::UnfulfilledEffect,
        format!(
            "`fn {}` expects {} in its `{}` and is proved not to: {}; a function's own list is exact, each word an effect it has",
            site.name,
            ticked(unfulfilled),
            own.attribute(),
            why.join("; ")
        ),
    );
    if !fixed {
        found.fix = site.write(own.without(unfulfilled));
        found.label = "list only what it does";
    }
    Some(found)
}

/// The function's old spellings, as written: "`#[blaze::nopanic]`, `#[blaze::noalloc]`".
fn ticked_negatives(declared: &Declared) -> String {
    EffectWord::ALL
        .into_iter()
        .filter(|&word| declared.negatives[word as usize].is_some())
        .map(|word| format!("`#[blaze::{}]`", word.text()))
        .collect::<Vec<_>>()
        .join(", ") // alloc: a message's part
}

/// `negative_effect` of the function.
fn negative(site: &Site<'_>) -> Option<Found> {
    let declared = &site.declared;
    // Only where effects are declared by what a function may do: under a module's default or
    // the crate's gate. Elsewhere the old spelling is all there is, and says nothing new.
    if !declared.has_negative() || declared.default_from.is_none() {
        return None;
    }
    // What it may do: every word no old spelling takes off that it is not proved free of.
    let negated = declared.negated();
    let words = ExpectWord::ALL
        .into_iter()
        .filter(|&word| !negated.has(word) && !site.proved_free_of(word))
        .fold(Words::NONE, |words, word| words.union(Words::one(word)));
    let mut found = finding(
        Lint::NegativeEffect,
        format!(
            "`fn {}` says what it may not do ({}); where effects are declared, a function says what it may do: `{}`",
            site.name,
            ticked_negatives(declared),
            words.attribute()
        ),
    );
    // A spelling taken at its word (`trusted`) has no list that means the same for it alone.
    let checked = declared
        .negatives
        .iter()
        .flatten()
        .all(|it| matches!(it, PureAttr::Checked));
    if !checked {
        found.notes.push(
            "one of them is `trusted`: write the list by hand, with `trusted = \"why\"` after its words, which takes the whole list at its word"
                .to_owned(), // alloc: a note
        );
        return Some(found);
    }
    if declared.expect.is_some() {
        return Some(found);
    }
    // The first old spelling becomes the list; the others go, with the space after them.
    let mut places = site.negatives_at.iter().map(|&(_, at)| at).collect::<Vec<_>>(); // alloc: at most three
    places.sort_by_key(|it| it.start());
    let (first, rest) = places.split_first()?;
    found.fix.push((*first, words.attribute()));
    found.fix.extend(rest.iter().map(|&at| (with_gap(site.text, at), String::new())));
    found.label = "write what it may do";
    Some(found)
}

/// `range` of `text` and the whitespace after it: what goes when an attribute does.
fn with_gap(text: &str, range: TextRange) -> TextRange {
    let end = usize::from(range.end()).min(text.len());
    let gap = text[end..].len() - text[end..].trim_start().len();
    TextRange::new(range.start(), range.end() + TextSize::new(gap as u32))
}

/// Where `#[blaze::<word>..]` is on `f`, a `cfg_attr`'s included, in `f`'s tree. Read as the
/// front end reads a tool attribute, by its path's two segments; what the function declares
/// is the front end's answer (`expect::declared`), this only places a fix.
fn written(f: ast::Fn<'_>, word: &str) -> Option<TextRange> {
    f.attrs()
        .find(|attr| attr.skip_cfg_attrs().any(|meta| blaze_path(meta) == Some(word)))
        .map(|attr| attr.syntax().text_range())
}

/// The name after `blaze::` of `meta`'s path, when it is two segments `blaze::<name>`.
fn blaze_path(meta: ast::Meta<'_>) -> Option<&str> {
    let path = meta.path()?;
    if path.qualifier()?.as_single_name_ref()?.text() != "blaze" {
        return None;
    }
    Some(path.segment()?.name_ref()?.text())
}

/// The deleted spelling `#[blaze::may(..)]` (or `#![blaze::may(..)]`, on a function, a trait, a
/// module or the crate's root), refused by `undeclared_effect` wherever it is written, with the
/// fix that respells its path `blaze::expect`. Nothing reads it as a list (`AttrFlags` sets no
/// bit for it), so a crate that kept it would otherwise be checked as if it declared nothing.
pub(super) fn check_deleted<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if cx.expansion().is_some() || !cx.on_blaze(Lint::UndeclaredEffect) {
        return;
    }
    let Some(attr) = ast::Attr::cast(node) else { return };
    for meta in attr.skip_cfg_attrs() {
        if blaze_path(meta) != Some("may") {
            continue;
        }
        let Some(path) = meta.path() else { continue };
        let (Some(whole), Some(at)) = (cx.range(attr.syntax()), cx.range(path.syntax())) else {
            continue;
        };
        let span = cx.span(whole);
        let fix = cx.span(at);
        let bang = if attr.excl_token().is_some() { "!" } else { "" };
        cx.blaze(
            Lint::UndeclaredEffect,
            node,
            span,
            format!(
                "`#{bang}[blaze::may(..)]` is deleted: what a function is expected to do is `#{bang}[blaze::expect(..)]`, with the same words, `trusted = \"why\"` and defaults, and a function's own list is exact (each word an effect it has)"
            ),
        )
        .multipart_suggestion(
            "respell it `blaze::expect`",
            vec![(fix, "blaze::expect".to_owned())], // alloc: the fix's one part
            Applicability::MachineApplicable,
        )
        .help(Lint::UndeclaredEffect.help())
        .help(format!("for further information visit {}", Lint::UndeclaredEffect.docs_url()));
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

/// The edit that writes `attribute` before what starts at `at`: on a line of its own at that
/// line's indentation when `at` starts its line, before it on its line otherwise.
fn before(text: &str, at: TextSize, attribute: &str) -> (TextSize, String) {
    let upto = usize::from(at).min(text.len());
    let lead = &text[text[..upto].rfind('\n').map_or(0, |it| it + 1)..upto];
    match lead.trim().is_empty() {
        true => (at, format!("{attribute}\n{lead}")), // alloc: the fix's text
        false => (at, format!("{attribute} ")),       // alloc: the fix's text
    }
}
