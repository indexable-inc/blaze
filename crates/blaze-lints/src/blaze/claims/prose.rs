//! How `claims` reads a doc comment: its sections, each claim listed in `# Guarantees` or
//! `# Invariants` with how it says it is held, a `# Panics` heading, and the sentences of its
//! prose that state an effect the compiler decides. Plain text in, places out (a line of the
//! doc and a byte in it): the lints judge and place what this finds.
//!
//! The one form of a claim that is not derived or an attribute (docs/v2/plan/design/claims.md):
//!
//! ```text
//! /// # Guarantees
//! ///
//! /// - The result is sorted by key. Evidence: [`tests::sorted_by_key`]
//! /// - A lookup is O(log n). Unverified: no test counts the probes yet.
//! /// - The model's step is deterministic. Evidence: Lean `Blaze.Mir.step_deterministic`
//! ```
//!
//! An effect sentence of the prose is found only in the narrow form measured on the tree's
//! docs: a clause that opens (after `It`, `This` or `This function`) with the phrase, `Never
//! panics.`, `It cannot panic or allocate.`, `Allocates nothing.`, or a later phrase of a
//! clause that opens with a verb of the item (`Allocates nothing, takes no lock and never
//! panics`), each followed by the clause's end, never by a word that narrows it (`allocates
//! nothing once warm`, `never blocks without end`, `a warm read allocates nothing`).

use std::alloc::Global;

/// An effect the effect walk decides for a function, with the attribute that states it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Effect {
    Panic,
    Alloc,
    Block,
    Pure,
}

impl Effect {
    /// The attribute that states it, after `blaze::`.
    pub(super) const fn attribute(self) -> &'static str {
        match self {
            Effect::Panic => "nopanic",
            Effect::Alloc => "noalloc",
            Effect::Block => "nonblocking",
            Effect::Pure => "pure",
        }
    }

    /// What the doc says, in a message.
    pub(super) const fn said(self) -> &'static str {
        match self {
            Effect::Panic => "never panics",
            Effect::Alloc => "does not allocate",
            Effect::Block => "never blocks",
            Effect::Pure => "is pure",
        }
    }
}

/// A place in a doc: a line (an index into the lines read) and a byte in that line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct At {
    pub(super) line: usize,
    pub(super) byte: usize,
}

/// A section whose every item is a claim.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Section {
    Guarantees,
    Invariants,
}

impl Section {
    /// Its heading's word.
    pub(super) const fn heading(self) -> &'static str {
        match self {
            Section::Guarantees => "Guarantees",
            Section::Invariants => "Invariants",
        }
    }
}

/// What an evidence pointer names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PointerKind {
    /// `` [`path`] ``: a Rust item, by name resolution.
    Rust,
    /// ``Lean `Name` ``: a theorem of `lean/theorems.json`, which the ledger checks.
    Lean,
    /// Anything else written after `Evidence:`.
    Bad,
}

/// One pointer after `Evidence:`: what it names and where its text is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Pointer<'d> {
    pub(super) kind: PointerKind,
    pub(super) text: &'d str,
    pub(super) at: At,
}

/// How a claim says it is held.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Held<'d> {
    /// `Evidence: <pointers>`, at least one.
    Evidence(Vec<Pointer<'d>, Global>),
    /// `Unverified: <reason>`, the reason not empty.
    Unverified,
    /// Neither marker.
    Nothing,
    /// The marker written another way (`Tested by`, `Proof:`), at its place: one spelling.
    Misspelled(&'static str, At),
    /// `Evidence:` with no pointer, or `Unverified:` with no reason, at the marker.
    Empty(&'static str, At),
}

/// A claim of a section: an item of its list, or a paragraph.
#[derive(Debug)]
pub(super) struct Item<'d> {
    pub(super) section: Section,
    /// Where its text starts.
    pub(super) at: At,
    /// Its words before the marker, the lines joined.
    pub(super) claim: String,
    pub(super) held: Held<'d>,
    /// The effects its words state, in the narrow form.
    pub(super) effects: Vec<Effect, Global>,
}

/// A sentence of the prose that states effects.
#[derive(Debug)]
pub(super) struct Stated {
    pub(super) effects: Vec<Effect, Global>,
    /// Where the phrase starts.
    pub(super) at: At,
    /// The phrase's length in its line, cut at the line's end.
    pub(super) len: usize,
    /// The sentence's bytes in its line, when it is on one line and says nothing but the
    /// phrase: what a fix deletes.
    pub(super) alone: Option<(usize, usize, usize)>,
}

/// What a doc comment says.
#[derive(Debug)]
pub(super) struct Doc<'d> {
    pub(super) items: Vec<Item<'d>, Global>,
    pub(super) stated: Vec<Stated, Global>,
    /// The line of a `# Panics` heading.
    pub(super) panics: Option<usize>,
}

/// The two markers, the one way a claim says how it is held.
const EVIDENCE: &str = "Evidence:";
const UNVERIFIED: &str = "Unverified:";

/// What a claim says it is held by when it says it another way: each is refused, so a reader
/// and the ledger find one spelling.
const MISSPELLED: &[&str] = &[
    "Tested by",
    "Proved by",
    "Proven by",
    "Checked by",
    "Verified by",
    "Proof:",
    "Evidence -",
    "evidence:",
    "unverified:",
    "Unverified -",
];

/// The words that negate an effect verb.
const NEGATIONS: &[&str] =
    &["can never ", "never ", "does not ", "doesn't ", "cannot ", "can't ", "will not ", "won't "];

/// The subjects a clause may open with and still be about the item.
const SUBJECTS: &[&str] = &["This function ", "this function ", "The function ", "It ", "it ", "This "];

/// What, before a later phrase of a clause, makes the phrase about something else than the
/// item, or narrows it.
const SUBORDINATE: &[&str] = &[
    " which ", " that ", " when ", " if ", " once ", " while ", " so ", " as ", " because ",
    " until ", " unless ", " after ", " before ", "(",
];

/// The words that, after a comma that ends a phrase, narrow it (`allocates nothing, once warm`).
const NARROWING: &[&str] = &[
    "once", "when", "until", "unless", "after", "before", "while", "if", "except", "but", "for",
    "on", "in", "per", "past", "without", "beyond", "from", "of",
];

/// Reads the doc whose lines (each past its `///` and one space) are `lines`.
pub(super) fn read<'d>(lines: &[&'d str]) -> Doc<'d> {
    let mut doc = Doc { items: Vec::new_in(Global), stated: Vec::new_in(Global), panics: None };
    // `None` in prose; `Some(None)` in a section that holds no claims.
    let mut section: Option<Option<Section>> = None;
    let mut in_code = false;
    // The lines of the prose paragraph or of the section's item being read: (line, byte, text).
    let mut open: Vec<(usize, usize, &'d str), Global> = Vec::new_in(Global);
    for (at, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        let lead = line.len() - trimmed.len();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            close(&mut doc, section, &mut open);
            in_code = !in_code;
            continue;
        }
        // An indented code block of the prose; a section's continued item is indented too.
        if in_code || (line.starts_with("    ") && section.is_none()) {
            continue;
        }
        if trimmed.starts_with('#') {
            close(&mut doc, section, &mut open);
            let heading = trimmed.trim_start_matches('#').trim();
            section = Some(match heading {
                "Guarantees" => Some(Section::Guarantees),
                "Invariants" => Some(Section::Invariants),
                _ => None,
            });
            if heading == "Panics" {
                doc.panics = Some(at);
            }
            continue;
        }
        if trimmed.is_empty() {
            close(&mut doc, section, &mut open);
            continue;
        }
        match section {
            Some(None) => {}
            None => open.push((at, lead, trimmed)),
            Some(Some(_)) => {
                let bullet = trimmed.strip_prefix("- ").or_else(|| trimmed.strip_prefix("* "));
                match bullet {
                    Some(text) => {
                        close(&mut doc, section, &mut open);
                        open.push((at, lead + 2, text));
                    }
                    None => open.push((at, lead, trimmed)),
                }
            }
        }
    }
    close(&mut doc, section, &mut open);
    doc
}

/// Ends the paragraph or item `open` holds, in `section`.
fn close<'d>(
    doc: &mut Doc<'d>,
    section: Option<Option<Section>>,
    open: &mut Vec<(usize, usize, &'d str), Global>,
) {
    if open.is_empty() {
        return;
    }
    match section {
        None => stated(doc, open),
        Some(Some(section)) => doc.items.push(item(section, open)),
        Some(None) => {}
    }
    open.clear();
}

/// The claim the lines `open` of a section make.
fn item<'d>(section: Section, open: &[(usize, usize, &'d str)]) -> Item<'d> {
    let at = At { line: open[0].0, byte: open[0].1 };
    // Where the marker is: its line among `open` and its byte in that line's text.
    let marker = [EVIDENCE, UNVERIFIED].into_iter().find_map(|marker| {
        open.iter().enumerate().find_map(|(index, (_, _, text))| {
            text.find(marker).map(|byte| (marker, index, byte))
        })
    });
    let mut claim = String::new();
    let upto = marker.map_or(open.len(), |(_, index, _)| index + 1);
    for (index, (_, _, text)) in open[..upto].iter().enumerate() {
        let text = match marker {
            Some((_, at, byte)) if at == index => &text[..byte],
            _ => text,
        };
        if !claim.is_empty() {
            claim.push(' ');
        }
        claim.push_str(text.trim());
    }
    let held = match marker {
        None => match misspelled(open) {
            Some((spelled, at)) => Held::Misspelled(spelled, at),
            None => Held::Nothing,
        },
        Some((marker, index, byte)) => {
            let place = At { line: open[index].0, byte: open[index].1 + byte };
            // The marker's rest of its line, then the item's later lines.
            let first = (open[index].0, open[index].1 + byte + marker.len(), &open[index].2[byte + marker.len()..]);
            let rest = std::iter::once(first).chain(open[index + 1..].iter().copied());
            if marker == UNVERIFIED {
                let said = rest
                    .clone()
                    .any(|(_, _, text)| !text.trim_matches(|it: char| it == '.' || it.is_whitespace()).is_empty());
                if said { Held::Unverified } else { Held::Empty(UNVERIFIED, place) }
            } else {
                let mut pointers = Vec::new_in(Global);
                for (line, byte, text) in rest {
                    pointers_of(line, byte, text, &mut pointers);
                }
                if pointers.is_empty() { Held::Empty(EVIDENCE, place) } else { Held::Evidence(pointers) }
            }
        }
    };
    let effects = effects_in(&claim).into_iter().flat_map(|(effects, _, _)| effects).collect();
    Item { section, at, claim, held, effects }
}

/// The first other spelling of a marker in `open`, with its place.
fn misspelled(open: &[(usize, usize, &str)]) -> Option<(&'static str, At)> {
    MISSPELLED.iter().find_map(|spelled| {
        open.iter().find_map(|(line, byte, text)| {
            text.find(spelled).map(|at| (*spelled, At { line: *line, byte: byte + at }))
        })
    })
}

/// The pointers of `text`, the part of line `line` from byte `byte` on after `Evidence:`:
/// `` [`path`] `` and ``Lean `Name` ``, between commas, `and`, `;` and a closing `.`.
fn pointers_of<'d>(line: usize, byte: usize, text: &'d str, out: &mut Vec<Pointer<'d>, Global>) {
    let mut at = 0;
    let bytes = text.as_bytes();
    while at < bytes.len() {
        let rest = &text[at..];
        let skip = rest.len() - rest.trim_start_matches(|it: char| it.is_whitespace() || matches!(it, ',' | ';' | '.')).len();
        if skip > 0 {
            at += skip;
            continue;
        }
        if let Some(after) = rest.strip_prefix("and ") {
            at += rest.len() - after.len();
            continue;
        }
        let place = |from: usize| At { line, byte: byte + from };
        if let Some(inner) = rest.strip_prefix("[`")
            && let Some(end) = inner.find("`]")
        {
            out.push(Pointer { kind: PointerKind::Rust, text: &inner[..end], at: place(at + 2) });
            at += 2 + end + 2;
            continue;
        }
        if let Some(inner) = rest.strip_prefix("Lean `")
            && let Some(end) = inner.find('`')
        {
            let name = &inner[..end];
            let kind = if lean_name(name) { PointerKind::Lean } else { PointerKind::Bad };
            out.push(Pointer { kind, text: name, at: place(at + 6) });
            at += 6 + end + 1;
            continue;
        }
        let end = rest.find([',', ';']).unwrap_or(rest.len());
        let word = rest[..end].trim_end_matches(|it: char| it == '.' || it.is_whitespace());
        out.push(Pointer { kind: PointerKind::Bad, text: word, at: place(at) });
        at += end.max(1);
    }
}

/// Whether `name` is a Lean name: dotted identifiers.
fn lean_name(name: &str) -> bool {
    !name.is_empty()
        && name.split('.').all(|part| {
            !part.is_empty() && part.chars().all(|it| it.is_alphanumeric() || matches!(it, '_' | '\''))
        })
}

/// The effect sentences of the prose paragraph `open`, pushed to `doc`.
fn stated(doc: &mut Doc<'_>, open: &[(usize, usize, &str)]) {
    // The paragraph's lines joined by a space, with where each starts in the joined text.
    let mut joined = String::new();
    let mut starts: Vec<(usize, usize, usize, usize), Global> = Vec::new_in(Global);
    for (line, byte, text) in open {
        if !joined.is_empty() {
            joined.push(' ');
        }
        starts.push((joined.len(), *line, *byte, text.len()));
        joined.push_str(text);
    }
    let place = |offset: usize| {
        let (from, line, byte, len) =
            starts.iter().rev().find(|(from, ..)| *from <= offset).copied().unwrap_or((0, 0, 0, 0));
        (At { line, byte: byte + (offset - from).min(len) }, len - (offset - from).min(len))
    };
    for (effects, start, end) in effects_in(&joined) {
        let (at, left) = place(start);
        let len = (end - start).min(left);
        // The sentence the phrase is in, and whether it says nothing else.
        let from = sentence_start(&joined, start);
        let to = sentence_end(&joined, end);
        let sentence = &joined[from..to];
        let bare = strip_subject(sentence.trim_end_matches('.').trim());
        let only = phrase(bare).is_some_and(|(_, length)| length == bare.len());
        let (first, _) = place(from);
        let (last, _) = place(to.saturating_sub(1));
        let alone = (only && first.line == last.line)
            .then(|| (first.line, first.byte, last.byte + 1));
        doc.stated.push(Stated { effects, at, len, alone });
    }
}

/// Where the sentence holding byte `at` of `text` starts.
fn sentence_start(text: &str, at: usize) -> usize {
    let before = &text[..at];
    [". ", "! ", "? "].iter().filter_map(|it| before.rfind(it).map(|found| found + it.len())).max().unwrap_or(0)
}

/// Where the sentence holding byte `at` of `text` ends, its full stop included.
fn sentence_end(text: &str, at: usize) -> usize {
    let after = &text[at..];
    [". ", "! ", "? "]
        .iter()
        .filter_map(|it| after.find(it).map(|found| at + found + 1))
        .min()
        .unwrap_or(text.len())
}

/// `text` without the subject it opens with, when it opens with one of [`SUBJECTS`].
fn strip_subject(text: &str) -> &str {
    SUBJECTS.iter().find_map(|it| text.strip_prefix(it)).unwrap_or(text)
}

/// The effect phrases of `text` in the narrow form, each with its effects and its bytes.
pub(super) fn effects_in(text: &str) -> Vec<(Vec<Effect, Global>, usize, usize), Global> {
    let mut found = Vec::new_in(Global);
    // Clauses: from the text's start and after each `. `, `! `, `? `, `: ` and `; `.
    let mut starts: Vec<usize, Global> = Vec::new_in(Global);
    starts.push(0);
    for (at, _) in text.match_indices([':', ';', '.', '!', '?']) {
        if text[at + 1..].starts_with(' ') {
            starts.push(at + 2);
        }
    }
    for (index, &start) in starts.iter().enumerate() {
        let end = starts.get(index + 1).map_or(text.len(), |next| next.saturating_sub(2).max(start));
        let clause = &text[start..end];
        let body = strip_subject(clause);
        let offset = start + (clause.len() - body.len());
        if let Some((effects, len)) = phrase(body)
            && ends(&body[len..])
        {
            found.push((effects, offset, offset + len));
        }
        // A later phrase of a clause that opens with a verb of the item.
        if !opens_with_verb(body) {
            continue;
        }
        for (at, sep) in body.match_indices([',', ' ']).filter(|(at, sep)| {
            *sep == "," || body[*at..].starts_with(" and ")
        }) {
            let next = if sep == "," { at + 2 } else { at + 5 };
            if SUBORDINATE.iter().any(|it| body[..at].contains(it)) {
                continue;
            }
            let Some(candidate) = body.get(next..) else { continue };
            if let Some((effects, len)) = phrase(candidate)
                && ends(&candidate[len..])
            {
                found.push((effects, offset + next, offset + next + len));
            }
        }
    }
    found
}

/// Whether `text` opens with what reads as a verb in the third person (`Allocates`, `Takes`,
/// `Returns`): a word of letters ending in one `s`.
fn opens_with_verb(text: &str) -> bool {
    let word: &str = text.split(|it: char| !it.is_alphabetic()).next().unwrap_or("");
    word.len() >= 4 && word.ends_with('s') && !word.ends_with("ss") && word.chars().all(|it| it.is_ascii_alphabetic())
}

/// Whether what follows a phrase ends it: the clause's end, punctuation, or another verb
/// joined by `and`, `or`, `nor`; never a word that narrows it.
fn ends(rest: &str) -> bool {
    if rest.is_empty() || rest.starts_with(['.', ';', ':', ')', '!', '?']) {
        return true;
    }
    if let Some(after) = rest.strip_prefix(',') {
        let word = after.trim_start().split(|it: char| !it.is_alphabetic()).next().unwrap_or("");
        return !NARROWING.contains(&word);
    }
    [" and ", " or ", " nor ", " (", " either", " at all", " ever"].iter().any(|it| rest.starts_with(it))
}

/// The effects the phrase `text` opens with, and its length: `never panics`, `cannot panic
/// or allocate`, `allocates nothing`, `is pure`, `has no side effects`.
fn phrase(text: &str) -> Option<(Vec<Effect, Global>, usize)> {
    let one = |effect: Effect| {
        let mut it = Vec::new_in(Global);
        it.push(effect);
        it
    };
    for (fixed, effect) in [
        ("allocates nothing", Effect::Alloc),
        ("has no side effects", Effect::Pure),
        ("has no side effect", Effect::Pure),
        ("is panic-free", Effect::Panic),
        ("is pure", Effect::Pure),
    ] {
        if strip_ci(text, fixed).is_some() {
            return Some((one(effect), fixed.len()));
        }
    }
    let (negation, mut rest) = NEGATIONS.iter().find_map(|it| strip_ci(text, it).map(|rest| (*it, rest)))?;
    let mut effects = Vec::new_in(Global);
    let mut len = negation.len();
    loop {
        let word: &str = rest.split(|it: char| !it.is_ascii_alphabetic()).next().unwrap_or("");
        let effect = match word {
            "panic" | "panics" => Effect::Panic,
            "allocate" | "allocates" => Effect::Alloc,
            "block" | "blocks" => Effect::Block,
            _ => break,
        };
        effects.push(effect);
        len += word.len();
        rest = &rest[word.len()..];
        // Another verb of the same negation: `panic or allocate`, `panics, blocks or allocates`.
        let joined = [" or ", ", ", " nor "].iter().find_map(|it| {
            let after = rest.strip_prefix(it)?;
            let next: &str = after.split(|it: char| !it.is_ascii_alphabetic()).next().unwrap_or("");
            matches!(next, "panic" | "panics" | "allocate" | "allocates" | "block" | "blocks")
                .then_some((it.len(), after))
        });
        match joined {
            Some((sep, after)) => {
                len += sep;
                rest = after;
            }
            None => break,
        }
    }
    (!effects.is_empty()).then_some((effects, len))
}

/// `text` past `prefix`, compared without ASCII case.
fn strip_ci<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix).then(|| &text[prefix.len()..])
}

#[cfg(test)]
mod tests {
    use super::{Effect, Held, PointerKind, Section, effects_in, read};

    fn effects(text: &str) -> Vec<Vec<Effect>> {
        effects_in(text).into_iter().map(|(it, _, _)| it.into_iter().collect()).collect()
    }

    /// The narrow form: a phrase that opens a clause, or a later one of a clause that opens
    /// with the item's verb, followed by the clause's end. The tree's own qualified sentences
    /// (docs read 2026-11-13) are not claims of the item.
    #[test]
    fn only_the_narrow_form_is_an_effect_claim() {
        use Effect::{Alloc, Block, Panic};
        assert_eq!(effects("Never blocks."), [vec![Block]]);
        assert_eq!(effects("It cannot panic or allocate, as [`Id::new`]."), [vec![Panic, Alloc]]);
        assert_eq!(effects("Allocates nothing, takes no lock and never panics, so an allocator may call it."), [vec![Alloc], vec![Panic]]);
        assert_eq!(effects("A stable sort: it never allocates."), [vec![Alloc]]);
        let none: [Vec<Effect>; 0] = [];
        for qualified in [
            "A warm read allocates nothing.",
            "Allocates nothing when there is none.",
            "It never blocks without end, and never pins past the bound.",
            "Bytes appended to a fixed buffer (never allocates).",
            "`lhs op rhs`, which never panics.",
            "Allocates nothing, once warm.",
            "Otherwise it never panics, since formatting must not.",
            "A probe, never blocks it.",
        ] {
            assert_eq!(effects(qualified), none, "{qualified}");
        }
    }

    #[test]
    fn a_sentence_that_is_only_the_claim_is_alone() {
        let doc = read(&["The first byte.", "Never panics."]);
        assert_eq!(doc.stated.len(), 1);
        assert_eq!(doc.stated[0].alone, Some((1, 0, 13)));
        let doc = read(&["The first byte. Never panics, so a caller may index."]);
        assert_eq!(doc.stated[0].alone, None);
    }

    #[test]
    fn a_section_s_items_say_how_they_are_held() {
        let doc = read(&[
            "Words.",
            "",
            "# Guarantees",
            "",
            "- Sorted. Evidence: [`tests::sorted`], Lean `Blaze.Mir.run_mono`",
            "- Unique.",
            "- Logarithmic. Unverified: nothing counts the probes.",
            "- In order. Tested by [`tests::sorted`]",
            "- Fast. Evidence: soon",
            "- Small. Unverified:",
            "",
            "# Examples",
            "",
            "- not a claim",
        ]);
        let held: Vec<&Held<'_>> = doc.items.iter().map(|it| &it.held).collect();
        assert_eq!(doc.items.len(), 6, "{doc:?}");
        assert!(doc.items.iter().all(|it| it.section == Section::Guarantees));
        let Held::Evidence(pointers) = held[0] else { panic!("{held:?}") };
        let kinds: Vec<(PointerKind, &str)> = pointers.iter().map(|it| (it.kind, it.text)).collect();
        assert_eq!(kinds, [(PointerKind::Rust, "tests::sorted"), (PointerKind::Lean, "Blaze.Mir.run_mono")]);
        assert_eq!(doc.items[0].claim, "Sorted.");
        assert_eq!(held[1], &Held::Nothing);
        assert_eq!(held[2], &Held::Unverified);
        assert!(matches!(held[3], Held::Misspelled("Tested by", _)), "{held:?}");
        let Held::Evidence(bad) = held[4] else { panic!("{held:?}") };
        assert_eq!(bad[0].kind, PointerKind::Bad);
        assert!(matches!(held[5], Held::Empty(_, _)), "{held:?}");
    }

    #[test]
    fn a_panics_heading_is_found() {
        assert_eq!(read(&["Mixes.", "", "# Panics", "", "When equal."]).panics, Some(2));
    }
}
