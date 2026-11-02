//! What rustc's lexer lints and what it gathers for the identifier lints:
//! `text_direction_codepoint_in_comment`, `text_direction_codepoint_in_literal`,
//! `rust_2021_prefixes_incompatible_syntax` and `rust_2024_guarded_string_incompatible_syntax`,
//! buffered at the crate root as each file is lexed; `keyword_idents_2018` and
//! `keyword_idents_2024` of the pre-expansion pass (`keywords`); and the identifier lints of
//! `check_crate` over the symbol gallery (`idents`).
//!
//! rustc lexes a file whole before `cfg` removes anything, so nothing here skips what `cfg`
//! removes (`Tree::skipped`): a comment or identifier of a removed item is lexed, and linted,
//! as any other. The files lexed are those of the
//! crate's modules and those `include!` reads; what a macro expands to is not lexed again.
//!
//! Follows rustc's `compiler/rustc_parse/src/lexer/mod.rs` (`lint_unicode_text_flow`,
//! `report_text_direction_codepoint`, `report_unknown_prefix`, `maybe_report_guarded_str`, the
//! `RawLifetime` arm of `next_token_from_cursor`), `compiler/rustc_parse/src/diagnostics.rs`
//! (`UnicodeTextFlow`, `UnicodeCharNoteSub`, `UnicodeTextFlowSuggestion`,
//! `HiddenUnicodeCodepointsDiag`, `HiddenUnicodeCodepointsDiagLabels`,
//! `HiddenUnicodeCodepointsDiagSub`, `ReservedPrefix`, `RawPrefix`, `ReservedStringLint`,
//! `ReservedMultihashLint`), `compiler/rustc_session/src/parse.rs` (`SymbolGallery`) and
//! `compiler/rustc_expand/src/proc_macro_server.rs` (the identifiers a proc macro makes, in the
//! gallery) (MIT or Apache-2.0).

mod idents;
mod keywords;
mod lex;

use std::{borrow::Cow, collections::hash_map};

use blaze_hash::{HashMap, HashSet};
use blaze_mem::Frame;
use hir::{HirFileId, InFile};
use hir_expand::MacroDefKind;
use span::{Edition, FileId};
use syntax::{SyntaxKind, SyntaxNodePtr, TextRange};

use crate::{
    cx::Cx,
    lints::{
        registry::{LintId, LintSet},
        syntax::{Emitted, Pass, Stage, Walk, source_fact},
    },
    lower::Phase,
    model::{Applicability, Diag, Span, SuggestionStyle},
};

use self::lex::{Lexed, LexedFile};


pub(crate) fn lints<'db>(cx: &Cx<'_, 'db>, walk: &Walk<'db>) -> Vec<Emitted> {
    // alloc: empty, allocates nothing
    let Some(root) = cx.krate.root_module(cx.db).as_source_file_id(cx.db) else { return Vec::new() };
    // alloc: empty, allocates nothing
    let Some(root) = walk.tree(HirFileId::from(root)) else { return Vec::new() };
    let krate = root.ptr(root.root);
    // alloc: empty until a lint fires; holds only what fires
    let mut emitted = Vec::new();
    for tree in walk.files() {
        keywords::lints(cx, tree, &mut emitted);
    }
    let sources = sources(cx, walk);
    let mut ascii = Vec::with_capacity_in(sources.len(), cx.frame());
    ascii.extend(sources.iter().map(|it| cx.file_text(it.file).is_ascii()));
    let gathered = gathers_idents(cx, walk, &ascii);
    let prefixes = cx.lint_may_fire(LintId::named("rust_2021_prefixes_incompatible_syntax"));
    let guarded = cx.lint_may_fire(LintId::named("rust_2024_guarded_string_incompatible_syntax"));
    let mut gallery = Gallery::default();
    let mut lexed = LexedFile::default();
    for (index, source) in sources.iter().enumerate() {
        // A codepoint that changes the direction of text is not ASCII, and the migration lints
        // are of editions before theirs: an ASCII file gives nothing else.
        let lexes = gathered
            || !ascii[index]
            || (prefixes && !source.edition.at_least_2021())
            || (guarded && !source.edition.at_least_2024());
        if !lexes {
            continue;
        }
        let text = cx.file_text(source.file);
        lex::lex_into(text, source.edition, &mut lexed);
        for found in lexed.lexed.drain(..) {
            emitted.push(lexer_lint(cx, source.file, text, found, krate));
        }
        if !gathered {
            continue;
        }
        for &ident in &lexed.idents {
            let symbol = lex::nfc(&text[ident.symbol_range()]);
            let order = (index as u32, u32::from(ident.range.start()), u32::from(ident.range.end()));
            gallery.symbols.entry(symbol).or_insert(Entry { order, place: Place::File(source.file, ident.range) });
        }
    }
    if gathered {
        gallery.add_proc_macro_idents(cx, walk, &sources);
        emitted.extend(idents::lints(cx, gallery.sorted(), krate));
    }
    emitted
}

/// The lints of the symbol gallery ([`idents`]).
const IDENT_LINTS: LintSet = LintSet::of(&[
    LintId::named("non_ascii_idents"),
    LintId::named("uncommon_codepoints"),
    LintId::named("confusable_idents"),
    LintId::named("mixed_script_confusables"),
]);

/// Whether the symbol gallery is gathered: some of its lints may fire, and some identifier may
/// not be ASCII. Every lint of it starts from an identifier that is not (rustc's
/// `NonAsciiIdents::check_crate` returns when none is), so a crate whose files are ASCII
/// throughout and whose proc macros make only ASCII text gives none of them: its identifiers
/// are not hashed. The test is a SIMD-wide ASCII scan of each file (`ascii`, by source) and of
/// each proc macro's expansion.
fn gathers_idents(cx: &Cx<'_, '_>, walk: &Walk<'_>, ascii: &[bool]) -> bool {
    if !cx.any_may_fire(&IDENT_LINTS) {
        return false;
    }
    if ascii.iter().any(|&it| !it) {
        return true;
    }
    walk.trees.iter().any(|tree| {
        tree.call.is_some_and(|call| matches!(call.loc(cx.db).def.kind, MacroDefKind::ProcMacro(..)))
            && !tree.root.text().is_ascii()
    })
}

/// A file rustc's lexer reads, and the edition it is lexed in.
struct Source {
    file: FileId,
    edition: Edition,
}

/// The files rustc lexes, in the order it loads them: the crate's modules, then what
/// `include!` reads.
fn sources<'f>(cx: &Cx<'f, '_>, walk: &Walk<'_>) -> Vec<Source, &'f Frame> {
    let mut seen = HashSet::default();
    let mut sources = Vec::new_in(cx.frame());
    for tree in walk.files() {
        let Some(file) = tree.file.file_id() else { continue };
        let id = file.file_id(cx.db);
        if seen.insert(id) {
            sources.push(Source { file: id, edition: tree.file.edition(cx.db) });
        }
    }
    for tree in &walk.trees {
        let Some(call) = tree.call else { continue };
        let Some(file) = call.loc(cx.db).include_file_id(cx.db, call) else { continue };
        let id = file.file_id(cx.db);
        // An included file's spans are of no expansion: it is lexed in the crate's edition.
        if seen.insert(id) {
            sources.push(Source { file: id, edition: cx.edition });
        }
    }
    sources
}

/// Where a symbol of the gallery was first seen: a token of a file, or one a proc macro made.
#[derive(Clone, Copy, Debug)]
pub(super) enum Place {
    File(FileId, TextRange),
    Expansion(InFile<TextRange>),
}

impl Place {
    pub(super) fn span(self, cx: &Cx<'_, '_>) -> Span {
        match self {
            Place::File(file, range) => cx.span_in_file(file, range),
            Place::Expansion(range) => cx.span(range),
        }
    }
}

/// Where rustc's spans sort a place: by the file, in the order the source map loaded it, then
/// where in it.
pub(super) type Order = (u32, u32, u32);

#[derive(Clone, Copy, Debug)]
struct Entry {
    order: Order,
    place: Place,
}

/// rustc's `SymbolGallery`: each identifier's symbol and where it was first seen.
#[derive(Default)]
struct Gallery<'db> {
    symbols: HashMap<Cow<'db, str>, Entry>,
}

/// A symbol of the gallery, as `check_crate` takes them: sorted by where they were seen.
pub(super) struct Symbol<'db> {
    pub(super) name: Cow<'db, str>,
    pub(super) order: Order,
    pub(super) place: Place,
}

impl<'db> Gallery<'db> {
    /// The identifiers of what proc macros expand to (`proc_macro_server.rs`: each `Ident` a
    /// proc macro gives back, its lifetimes' too, which `proc_macro` gives as a `'` and an
    /// `Ident`), where the gallery has no such symbol yet. They are expanded after the files
    /// they are called from are lexed. A symbol borrows the expansion's text, which the
    /// database keeps, unless NFC changes it.
    fn add_proc_macro_idents(&mut self, cx: &Cx<'_, 'db>, walk: &Walk<'db>, sources: &[Source]) {
        let mut index: Option<HashMap<FileId, u32>> = None;
        for tree in &walk.trees {
            let Some(call) = tree.call else { continue };
            if !matches!(call.loc(cx.db).def.kind, MacroDefKind::ProcMacro(..)) {
                continue;
            }
            for token in tree.root.descendants_with_tokens().filter_map(|it| it.into_token()) {
                let text = token.text();
                let name = match token.kind() {
                    SyntaxKind::LIFETIME_IDENT => text.strip_prefix('\''),
                    kind if kind.is_any_identifier() => Some(text),
                    _ => None,
                };
                let Some(name) = name else { continue };
                let symbol = lex::nfc(name.strip_prefix("r#").unwrap_or(name));
                let hash_map::Entry::Vacant(slot) = self.symbols.entry(symbol) else { continue };
                let range = InFile::new(tree.file, token.text_range());
                let (file, at) = cx.place(range);
                let index = index.get_or_insert_with(|| {
                    // alloc: setup, once per crate whose proc macros make a new identifier
                    sources.iter().enumerate().map(|(at, it)| (it.file, at as u32)).collect()
                });
                let file = index.get(&file).copied().unwrap_or(u32::MAX);
                let order = (file, at.start().into(), at.end().into());
                slot.insert(Entry { order, place: Place::Expansion(range) });
            }
        }
    }

    fn sorted(self) -> Vec<Symbol<'db>> {
        let mut symbols: Vec<Symbol<'db>> = self
            .symbols
            .into_iter()
            .map(|(name, Entry { order, place })| Symbol { name, order, place })
            // alloc: once per crate whose identifiers are gathered
            .collect();
        // Symbols a proc macro made share its call's span; rustc leaves those in hash order.
        symbols.sort_unstable_by(|a, b| a.order.cmp(&b.order).then_with(|| a.name.cmp(&b.name)));
        symbols
    }
}

const TEXT_FLOW_NOTE: &str = "these kind of unicode codepoints change the way text flows on applications that support them, but can cause confusion because they change the order of characters on the screen";
const REMOVE_THEM: &str = "if their presence wasn't intentional, you can remove them";

/// `count` codepoints, as `this comment contains ..` and `this literal contains ..` say it.
fn invisible(count: usize) -> &'static str {
    match count {
        1 => "an invisible unicode text flow control codepoint",
        _ => "invisible unicode text flow control codepoints",
    }
}

/// A character as rustc's `{c:?}` writes it, without the quotes.
fn escaped(c: char) -> String {
    // alloc: the lint's text, for a lint found
    let debug = format!("{c:?}");
    // alloc: the lint's text, for a lint found
    debug[1..debug.len() - 1].to_owned()
}

/// The lint rustc's lexer buffers for `lexed`, of `file` (whose text is `text`), at the crate
/// root `krate`.
fn lexer_lint(cx: &Cx<'_, '_>, file: FileId, text: &str, lexed: Lexed, krate: InFile<SyntaxNodePtr>) -> Emitted {
    let span = |range: TextRange| cx.span_in_file(file, range);
    let mut diag = match lexed {
        Lexed::Comment { range, chars } => {
            let at = span(range);
            let mut diag = Diag::lint(
                "text_direction_codepoint_in_comment",
                "unicode codepoint changing visible direction of text present in comment",
                // clone: a lint's span, for a lint found
                at.clone(),
            );
            diag.note(TEXT_FLOW_NOTE);
            // alloc: the lint's text, for a lint found
            diag.span_label(at, format!("this comment contains {}", invisible(chars.len())));
            for &(c, range) in &chars {
                // alloc: the lint's text, for a lint found
                diag.span_label(span(range), format!("{c:?}"));
            }
            // alloc: the lint's text, for a lint found
            let parts = chars.iter().map(|&(_, range)| (span(range), String::new())).collect();
            diag.suggestion_with_style(parts, REMOVE_THEM, Applicability::MachineApplicable, SuggestionStyle::HideCodeAlways);
            diag
        }
        Lexed::Literal { range, label, chars, inner, doc } => {
            let at = span(range);
            let mut diag = Diag::lint(
                "text_direction_codepoint_in_literal",
                // alloc: the lint's text, for a lint found
                format!("unicode codepoint changing visible direction of text present in {label}"),
                // clone: a lint's span, for a lint found
                at.clone(),
            );
            diag.note(TEXT_FLOW_NOTE);
            // alloc: the lint's text, for a lint found
            diag.span_label(at, format!("this {label} contains {}", invisible(chars.len())));
            if inner {
                for &(c, range) in &chars {
                    // alloc: the lint's text, for a lint found
                    diag.span_label(span(range), format!("{c:?}"));
                }
            }
            if inner && !chars.is_empty() {
                // alloc: the lint's text, for a lint found
                let removed = chars.iter().map(|&(_, range)| (span(range), String::new())).collect();
                diag.suggestion_with_style(
                    removed,
                    REMOVE_THEM,
                    Applicability::MachineApplicable,
                    SuggestionStyle::HideCodeAlways,
                );
                // alloc: the lint's text, for a lint found
                let escapes = chars.iter().map(|&(c, range)| (span(range), escaped(c))).collect();
                diag.multipart_suggestion(
                    "if you want to keep them but make them visible in your source code, you can escape them",
                    escapes,
                    Applicability::MachineApplicable,
                );
            } else {
                // alloc: the lint's text, for a lint found
                let escaped: Vec<String> = chars.iter().map(|(c, _)| format!("{c:?}")).collect();
                let escaped = escaped.join(", ");
                diag.note(REMOVE_THEM);
                match doc {
                    // alloc: the lint's text, for a lint found
                    true => diag.note(format!(
                        "if you need to keep them and make them explicit in source, rewrite this doc comment as a `#[doc = \"...\"]` attribute and use Unicode escapes such as {escaped}"
                    )),
                    // alloc: the lint's text, for a lint found
                    false => diag.note(format!(
                        "if you want to keep them but make them visible in your source code, you can escape them: {escaped}"
                    )),
                };
            }
            diag
        }
        Lexed::UnknownPrefix { range } => {
            let at = span(range);
            let mut diag = Diag::lint(
                "rust_2021_prefixes_incompatible_syntax",
                // alloc: the lint's text, for a lint found
                format!("prefix `{}` is unknown", &text[range]),
                // clone: a lint's span, for a lint found
                at.clone(),
            );
            diag.span_label(at, "unknown prefix");
            diag.suggestion(
                span(TextRange::empty(range.end())),
                "insert whitespace here to avoid this being parsed as a prefix in Rust 2021",
                " ",
                Applicability::MachineApplicable,
            );
            diag
        }
        Lexed::RawPrefix { range } => {
            let at = span(range);
            // clone: a lint's span, for a lint found
            let mut diag = Diag::lint("rust_2021_prefixes_incompatible_syntax", "prefix `'r` is reserved", at.clone());
            diag.span_label(at, "reserved prefix");
            diag.suggestion(
                span(TextRange::empty(range.end())),
                "insert whitespace here to avoid this being parsed as a prefix in Rust 2021",
                " ",
                Applicability::MachineApplicable,
            );
            diag
        }
        Lexed::Guarded { range, string, space } => {
            let (message, help) = match string {
                true => (
                    "will be parsed as a guarded string in Rust 2024",
                    "insert whitespace here to avoid this being parsed as a guarded string in Rust 2024",
                ),
                false => (
                    "reserved token in Rust 2024",
                    "insert whitespace here to avoid this being parsed as a forbidden token in Rust 2024",
                ),
            };
            let mut diag = Diag::lint("rust_2024_guarded_string_incompatible_syntax", message, span(range));
            diag.suggestion(span(TextRange::empty(space)), help, " ", Applicability::MachineApplicable);
            diag
        }
    };
    source_fact(cx, &mut diag, krate);
    Emitted::new(diag, Phase::Resolve, Pass::Buffered, krate).buffered(Stage::Parsed)
}

#[cfg(test)]
mod tests {
    use syntax::TextSize;

    use super::*;

    #[test]
    fn codepoints_are_escaped_as_rust_writes_them() {
        assert_eq!(escaped('\u{202e}'), "\\u{202e}");
        assert_eq!(format!("{:?}", '\u{2066}'), "'\\u{2066}'");
        assert_eq!(invisible(1), "an invisible unicode text flow control codepoint");
        assert_eq!(invisible(4), "invisible unicode text flow control codepoints");
    }

    /// A crate of edition 2021 whose files are ASCII, under rustc's defaults, is not lexed and
    /// its identifiers are not gathered: the pass makes the same few allocations (the list of
    /// files) whatever the crate's size.
    #[test]
    fn an_ascii_crate_is_not_lexed() {
        let used = |fns: usize| {
            let mut used = None;
            crate::lints::with_cx(&crate::lints::quiet_crate(fns), &[], |cx| {
                let walk = cx.walk();
                assert!(lints(cx, walk).is_empty());
                let (found, it) = blaze_alloc_count::measure(|| lints(cx, walk));
                assert!(found.is_empty());
                used = Some(it.allocations);
            });
            used.unwrap()
        };
        let (small, large) = (used(4), used(400));
        assert_eq!(small, large, "the lexer's allocations do not grow with the crate");
        assert!(small <= 4, "{small} allocations");
    }

    #[test]
    fn an_empty_range_is_where_whitespace_goes() {
        let range = TextRange::new(TextSize::from(3), TextSize::from(5));
        assert_eq!(TextRange::empty(range.end()), TextRange::new(TextSize::from(5), TextSize::from(5)));
    }
}
