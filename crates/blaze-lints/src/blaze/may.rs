//! `undeclared_effect`, `unneeded_effect` and `negative_effect`: the lints of
//! `#[blaze::may(..)]`, which says what a function may do (docs/v2/plan/design/effects.md):
//! `panic`, `alloc`, `block`, `diverge`. A word a list leaves out is an effect the function
//! does not have, checked through every call it makes.
//!
//! - `undeclared_effect` (`deny`; it fires only where something is declared): a function with
//!   an effect its allowance leaves out, one finding at its name that says each such effect
//!   with its witness (the call chain down to what does it, or the loop's line), and one fix
//!   that writes the list with the words added. The allowance is the function's own list, else
//!   its trait method's (or trait's), else the `#![blaze::may(..)]` of the nearest module
//!   around it, else nothing in a crate with `#![blaze::effects]`
//!   (`hir_ty_pure::may::declared`); a function none of these reach is not looked at, so a
//!   crate that asks for nothing is linted as before. Also here: an impl's method whose list
//!   has a word its trait's method does not allow, and a list that is not one.
//! - `unneeded_effect` (`warn`): a word of a private function's own list for an effect it is
//!   proved not to have. A list is a permission, so this is advice, and only where nobody else
//!   can rely on the right: a function with a visibility written, a trait's method and an
//!   impl's method of one may list more than they do today.
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
    DefWithBodyId, FunctionId, HasModule, ItemContainerId, Lookup,
    attrs::{EffectWord, MayWord, PureAttr},
    signatures::FunctionSignature,
};
use hir_ty::db::HirDatabase as _;
use hir_ty_mir::mir::overflow::crate_checks_overflow;
use hir_ty_pure::{
    Refusal,
    effect_attrs::of_word,
    may::{self, BoundFrom, Declared, DefaultFrom, Verdict, Words},
};
use syntax::{
    NodeOrToken, SyntaxKind, TextRange, TextSize,
    view::{self as ast, AstNode as _, HasAttrs as _, HasName as _, HasVisibility as _},
};

use super::{Lint, effects};
use crate::{
    check::Cx,
    diag::{self, Applicability},
};

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
    let lints = [Lint::UndeclaredEffect, Lint::UnneededEffect, Lint::NegativeEffect];
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
    /// Where its own `#[blaze::may(..)]` is in the text.
    may_at: Option<TextRange>,
    /// Its old spellings, each where it is in the text.
    negatives_at: Vec<(EffectWord, TextRange)>,
    /// Where its signature starts in the text.
    signature: TextSize,
    text: &'a str,
    /// It has no visibility written and is no method of a trait or of a trait's impl.
    private: bool,
    in_test: bool,
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
    let declared = may::declared(db, func);
    // Nothing declared on it or around it: it may do anything, and nothing is asked.
    if declared.marks == may::Marks::default() && declared.may.is_none() {
        return None;
    }
    let name = f.name()?;
    let name_at = cx.range(name.syntax())?;
    let whole = cx.range(root)?;
    // A place of the function's tree in the text the fix is made in.
    let at = |it: TextSize| whole.start() + (it - root.text_range().start());
    let in_text = |range: TextRange| TextRange::new(at(range.start()), at(range.end()));
    let of_trait = match func.lookup(db).container {
        ItemContainerId::TraitId(_) => true,
        ItemContainerId::ImplId(imp) => db.impl_trait(imp).is_some(),
        _ => false,
    };
    let site = Site {
        db,
        func,
        krate: HasModule::krate(&func, db),
        name: name.text().to_string(), // alloc: a message's part
        may_at: written(f, "may").map(in_text),
        negatives_at: EffectWord::ALL
            .into_iter()
            .filter_map(|word| Some((word, in_text(written(f, word.text())?))))
            .collect(), // alloc: at most three
        signature: at(signature_start(f)?),
        text: cx.text(),
        private: f.visibility().is_none() && !of_trait,
        in_test: cx.is_in_test(root),
        declared,
    };
    let mut found = Vec::new(); // alloc: at most three findings
    if cx.on_blaze(Lint::UndeclaredEffect) {
        found.extend(undeclared(&site));
    }
    if cx.on_blaze(Lint::UnneededEffect) {
        found.extend(unneeded(&site));
    }
    if cx.on_blaze(Lint::NegativeEffect) {
        found.extend(negative(&site));
    }
    Some((name_at, found))
}

/// What a word allows, as a message says it after "may".
fn may_do(word: MayWord) -> &'static str {
    match word {
        MayWord::Panic => "panic",
        MayWord::Alloc => "allocate from the global heap",
        MayWord::Block => "block the thread",
        MayWord::Diverge => "not return (`diverge`)",
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
    fn has(&self, word: MayWord, overflow_checks: bool) -> Option<Refusal> {
        hir_ty_pure::check_generic(self.db, self.func, of_word(word, overflow_checks)).err()
    }

    /// Whether the function is proved free of what `word` allows, with every overflow check
    /// on and no call left to an instance: the word can go, in any profile.
    fn proved_free_of(&self, word: MayWord) -> bool {
        if word == MayWord::Panic && crate_checks_overflow(self.db, self.krate) != Some(true) {
            return false;
        }
        hir_ty_pure::check_generic(self.db, self.func, of_word(word, true))
            .is_ok_and(|proof| !proof.open)
    }

    /// What the allowance is written as, for a message.
    fn allowance(&self) -> String {
        let marks = &self.declared.marks;
        let list = |words: Option<Words>| words.unwrap_or_default().list();
        if marks.own.is_some() {
            return format!("its `#[blaze::may({})]`", list(marks.own)); // alloc: a message's part
        }
        match (self.declared.bound_from, self.declared.default_from) {
            (Some(BoundFrom::Method(_)), _) => format!(
                "the `#[blaze::may({})]` of the trait's method it implements",
                list(marks.bound)
            ), // alloc: as above
            (Some(BoundFrom::Trait(_)), _) => {
                format!("the `#[blaze::may({})]` of its trait", list(marks.bound)) // alloc: as above
            }
            (None, Some(DefaultFrom::Module(_))) => format!(
                "the default of a module around it, `#![blaze::may({})]`",
                list(marks.default)
            ), // alloc: as above
            _ => "`#![blaze::effects]` (a function with no `#[blaze::may(..)]` may have no effect)"
                .to_owned(), // alloc: as above
        }
    }

    /// The fix that makes the function's list `words`: its own list rewritten, or a new one
    /// above its signature.
    fn write(&self, words: Words) -> Vec<(TextRange, String)> {
        match self.may_at {
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
    if let Some(text) = declared.may.as_ref().and_then(|it| it.malformed.as_ref()) {
        return Some(finding(
            Lint::UndeclaredEffect,
            format!(
                "`#[blaze::may{}]` on `{name}`: the list is words of `panic`, `alloc`, `block` and `diverge` separated by commas, each once, then at most `trusted = \"why\"`",
                text.as_str()
            ),
        ));
    }
    if declared.may.is_some() && declared.has_negative() {
        return Some(finding(
            Lint::UndeclaredEffect,
            format!(
                "`fn {name}` says what it may do (`#[blaze::may(..)]`) and what it may not ({}): write the list alone, without the words the old spelling takes off",
                ticked_negatives(declared)
            ),
        ));
    }
    // An old spelling alone is its own lint's (`blaze::nopanic`, ...), as before.
    if declared.may.is_none() && declared.has_negative() {
        return None;
    }
    let marks = &declared.marks;
    if let Verdict::BeyondBound(beyond) = may::verdict(marks, Words::NONE) {
        let mut found = finding(
            Lint::UndeclaredEffect,
            format!(
                "`fn {name}` lists {} in its `#[blaze::may(..)]`, which the trait's method it implements does not allow (`#[blaze::may({})]`): an impl may declare fewer effects than its trait's method, never more",
                ticked(beyond),
                marks.bound.unwrap_or_default().list()
            ),
        );
        found.fix = site.write(marks.own.unwrap_or_default().without(beyond));
        found.label = "list only what the trait's method allows";
        return Some(found);
    }
    let allowed = may::allowed(marks)?;
    // A list taken at its word is not checked; a default does not reach test code.
    let trusted = declared.may.as_ref().is_some_and(|it| it.trusted.is_some());
    let by_default = marks.own.is_none() && marks.bound.is_none();
    if trusted || (by_default && site.in_test) {
        return None;
    }
    let overflow_checks = site.overflow_checks();
    let refused: Vec<(MayWord, Refusal)> = MayWord::ALL
        .into_iter()
        .filter(|&word| !allowed.has(word))
        .filter_map(|word| Some((word, site.has(word, overflow_checks)?)))
        .collect(); // alloc: at most four refusals, once per function that has one
    let has = refused.iter().fold(Words::NONE, |has, (word, _)| has.union(Words::one(*word)));
    let Verdict::Undeclared(missing) = may::verdict(marks, allowed.union(has)) else {
        return None;
    };
    let (first, refusal) = refused.first()?;
    let does: Vec<&str> = missing.iter().map(may_do).collect(); // alloc: a message's parts
    let mut found = finding(
        Lint::UndeclaredEffect,
        format!(
            "`fn {name}` may {}, which {} does not allow: it {}: {}; add `{}` to its `#[blaze::may(..)]`, or handle it",
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
    if matches!(first, MayWord::Panic) {
        let profile = if overflow_checks { "with" } else { "without" };
        found.notes.push(format!("`panic` is checked as this crate builds: {profile} overflow checks")); // alloc: a note
    }
    found.steps = effects::step_notes(site.db, site.krate, refusal);
    // The list with the words added, where the trait's method allows them.
    let wanted = allowed.union(missing);
    if marks.bound.is_none_or(|bound| wanted.within(bound)) {
        found.fix = site.write(wanted);
        found.label = "list what it may do";
    }
    Some(found)
}

/// `unneeded_effect` of the function.
fn unneeded(site: &Site<'_>) -> Option<Found> {
    let may = site.declared.may.as_ref()?;
    if may.malformed.is_some() || may.trusted.is_some() || !site.private || site.may_at.is_none() {
        return None;
    }
    let own = Words(may.words);
    let unneeded = own
        .iter()
        .filter(|&word| site.proved_free_of(word))
        .fold(Words::NONE, |words, word| words.union(Words::one(word)));
    if unneeded.is_empty() {
        return None;
    }
    let mut found = finding(
        Lint::UnneededEffect,
        format!(
            "`fn {}` lists {} in its `#[blaze::may(..)]` and is proved not to, through every call it makes: a list says what a function may do, and a private function keeps the right for nobody",
            site.name,
            ticked(unneeded)
        ),
    );
    found.fix = site.write(own.without(unneeded));
    found.label = "list only what it may do";
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
    let words = MayWord::ALL
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
    if declared.may.is_some() {
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
/// is the front end's answer (`may::declared`), this only places a fix.
fn written(f: ast::Fn<'_>, word: &str) -> Option<TextRange> {
    for attr in f.attrs() {
        for meta in attr.skip_cfg_attrs() {
            let Some(path) = meta.path() else { continue };
            let path: String =
                path.syntax().text().chars().filter(|it| !it.is_whitespace()).collect(); // alloc: one attribute's path
            if path.strip_prefix("blaze::") == Some(word) {
                return Some(attr.syntax().text_range());
            }
        }
    }
    None
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
