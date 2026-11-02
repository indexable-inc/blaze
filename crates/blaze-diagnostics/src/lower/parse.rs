//! The syntax errors of a crate's files as rustc gives them. rustc lexes a whole file into
//! token trees before it parses it: an unterminated literal or block comment, or a delimiter
//! that is not matched, stops the compilation there (`rustc_parse/src/lexer/mod.rs`,
//! `lexer/tokentrees.rs`, `lexer/diagnostics.rs`, `lib.rs` `unwrap_or_emit_fatal`). It then
//! parses the file and gives up at the first error it does not recover from
//! (`parser/diagnostics.rs` `expected_one_of_not_found`, `parser/item.rs` `parse_mod`,
//! `parser/ty.rs`): in the crate root that ends the compilation (`rustc_interface/src/
//! passes.rs` `parse`), in a module's file the module is left empty and its own modules are not
//! read (`rustc_expand/src/module.rs` `parse_external_mod`). Files are read in rustc's order:
//! the crate root, then each module's file as expansion reaches its `mod` item. Follows those
//! files of rustc and `diagnostics.rs` of `rustc_parse` (MIT or Apache-2.0).
//!
//! What blaze's parser reports is less than rustc's parser knows (the tokens it expected, where
//! it recovers): what it reports in a form rustc has is written as rustc writes it, the rest in
//! the parser's own words, always an error (`v2/requests/diagnostics.md`).

use blaze_hash::HashSet;
use blaze_ids::radix;
use hir::{HirFileId, InFile};
use syntax::{
    Edition, LexedStr, SyntaxError, SyntaxKind, TextRange, TextSize,
    view::{self as ast, AstNode as _},
};

use crate::{
    cx::Cx,
    lower::{Found, Phase},
    model::{Applicability, Diag, Span, SuggestionStyle},
};

mod crate_attr;

/// A crate's syntax errors, and whether rustc stops compiling after them: then nothing but
/// these and its closing lines is said of the crate.
pub(crate) struct Parsed {
    pub(crate) found: Vec<Found>,
    pub(crate) fatal: bool,
}

pub(crate) fn lower(cx: &Cx<'_, '_>) -> Vec<Found> {
    parsed(cx).found
}

/// A crate whose files all parse is one pass over its module table, one memoized read per
/// file; only a crate with an error walks its modules in rustc's order.
pub(crate) fn parsed(cx: &Cx<'_, '_>) -> Parsed {
    let db = cx.db;
    let def_map = hir_def::nameres::crate_def_map(db, cx.krate.base());
    let clean = def_map
        .modules(db)
        .all(|(_, data)| data.origin.file_id().is_none_or(|file| file_errors(db, file).is_empty()));
    if clean {
        // alloc: empty unless a crate attribute is refused, the edge
        let mut found = Vec::new();
        let fatal = crate_attr::lower(cx, &mut found);
        return Parsed { found, fatal };
    }
    // alloc: a crate with a syntax error, the edge
    let mut found = Vec::new();
    // alloc: the error's parts, for an error hir found
    let mut emptied: Vec<hir::Module> = Vec::new();
    let modules = crate::checks::modules(cx);
    let (&root, rest) = modules.split_first().expect("a crate has its root module");
    if module_fatal(cx, root, &mut emptied, &mut found) || crate_attr::lower(cx, &mut found) {
        return Parsed { found, fatal: true };
    }
    for &module in rest {
        if module_fatal(cx, module, &mut emptied, &mut found) {
            return Parsed { found, fatal: true };
        }
    }
    Parsed { found, fatal: false }
}

/// The syntax errors of `module`'s own file, if it has one rustc reads, and whether rustc stops
/// there; a module whose file rustc gives up on is `emptied`.
fn module_fatal(
    cx: &Cx<'_, '_>,
    module: hir::Module,
    emptied: &mut Vec<hir::Module>,
    found: &mut Vec<Found>,
) -> bool {
    let db = cx.db;
    if module.is_inline(db) || module.path_to_root(db).iter().any(|it| emptied.contains(it)) {
        return false;
    }
    let Some(file) = module.as_source_file_id(db) else { return false };
    let errors = file_errors(db, file);
    if errors.is_empty() {
        return false;
    }
    let (file_id, edition) = file.unpack(db);
    let text = cx.file_text(file_id);
    let tree = cx.root(file.into());
    match lower_file(cx, file.into(), text, edition, tree, errors, found) {
        Outcome::Parsed => false,
        Outcome::GaveUp if !module.is_crate_root(db) => {
            emptied.push(module);
            false
        }
        Outcome::GaveUp | Outcome::Fatal | Outcome::Unbalanced => true,
    }
}

/// The syntax errors of `file`, its bodies' included, memoized: a file parsed in full has them
/// in `parse_errors`, which other passes read too; a library file is an outline, whose lazy
/// bodies' errors are their own query.
fn file_errors(db: &base_db::Db, file: base_db::EditionedFileId) -> &[SyntaxError] {
    let errors = match file.is_library(db) {
        true => file.parse_errors_with_bodies(db),
        false => file.parse_errors(db),
    };
    errors.unwrap_or_default()
}

enum Outcome {
    Parsed,
    /// The parser gave up on the file.
    GaveUp,
    /// The compilation stops.
    Fatal,
    /// A delimiter is not matched: rustc parses nothing of the file, and stops after a file's
    /// (`unwrap_or_emit_fatal`) but not after a crate attribute's (`cmdline_attrs.rs`).
    Unbalanced,
}

/// A token of a file as rustc's lexer gives it to the parser: not whitespace or a comment, and
/// punctuation written together glued into one (`::`, `->`, `..=`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Tok {
    kind: SyntaxKind,
    range: TextRange,
}

/// A tree as rustc's parser reads it: a file's, or a macro expansion's (`file` its expansion,
/// `text` the tree's text).
struct File<'a, 'c, 'db> {
    cx: &'a Cx<'c, 'db>,
    file: HirFileId,
    text: &'a str,
    edition: Edition,
    tree: ast::Node<'a>,
    toks: Vec<Tok>,
}

/// A parse error of a macro's expansion `root` (of `file`, called at `site`), the parser's `msg`
/// at `pos`, as rustc's parser gives it, with what `rustc_expand`'s `failed_to_match_macro`
/// makes of an error at the expansion's end: "macro expansion ends with an incomplete
/// expression", at the end of its last token, or at the call for an empty expansion. Its tokens
/// are the tree's: an expansion has no text to lex again.
pub(crate) fn expansion_error(
    cx: &Cx<'_, '_>,
    file: HirFileId,
    root: ast::Node<'_>,
    site: Span,
    msg: &str,
    pos: TextSize,
) -> Diag {
    let toks = root
        .descendants_with_tokens()
        .filter_map(|it| it.into_token())
        .filter(|it| !is_trivia(it.kind()))
        .map(|it| Tok { kind: it.kind(), range: it.text_range() })
        // alloc: the error's parts, for an error hir found
        .collect();
    let f = File { cx, file, text: root.text(), edition: file.edition(cx.db), tree: root, toks };
    let (mut diag, _) = f.parser_error(msg, pos);
    let (prev, token) = f.around(pos);
    if token.is_none()
        && let Some(expected) = diag.message.strip_suffix(", found `<eof>`").map(str::to_owned)
    {
        // alloc: the error's text, for an error hir found
        diag.message = format!("macro expansion ends with an incomplete expression: {expected}");
        let at = prev.map_or(site, |it| f.at(it.range).shrink_to_hi());
        let old = std::mem::take(&mut diag.span.primary);
        for (span, _) in &mut diag.span.labels {
            if old.contains(span) {
                // clone: on the error path, for an error hir found
                *span = at.clone();
            }
        }
        // alloc: the error's parts, for an error hir found
        diag.span.primary = vec![at];
    }
    diag
}

fn lower_file(
    cx: &Cx<'_, '_>,
    file: HirFileId,
    text: &str,
    edition: Edition,
    tree: ast::Node<'_>,
    errors: &[SyntaxError],
    found: &mut Vec<Found>,
) -> Outcome {
    let lexed = LexedStr::new(edition, text);
    let toks = tokens(&lexed, text);
    let f = File { cx, file, text, edition, tree, toks };

    // alloc: the error's parts, for an error hir found
    let mut lexer_errors: Vec<(String, TextRange)> = Vec::new();
    let mut last_lifetime: Option<TextRange> = None;
    let mut errors_at = lexed.errors().peekable();
    for i in 0..lexed.len() {
        let range = token_range(&lexed, i);
        while let Some((_, msg)) = errors_at.next_if(|(at, _)| *at == i) {
            // alloc: the error's text, for an error hir found
            lexer_errors.push((msg.to_owned(), range));
            let (diag, fatal) = f.lexer_error(lexed.kind(i), range, msg, last_lifetime);
            found.push(Found::new(diag, Phase::Parse));
            if fatal {
                return Outcome::Fatal;
            }
        }
        if lexed.kind(i) == SyntaxKind::LIFETIME_IDENT {
            last_lifetime = Some(TextRange::at(range.start(), TextSize::of('\'')));
        }
    }

    let delimiters = f.delimiters();
    if !delimiters.is_empty() {
        found.extend(delimiters.into_iter().map(|it| Found::new(it, Phase::Parse)));
        return Outcome::Unbalanced;
    }

    let is_lexer_error = |error: &SyntaxError| {
        // alloc: the error's text, for an error hir found
        let message = error.to_string();
        lexer_errors.iter().any(|(msg, range)| {
            *msg == message && (*range == error.range() || range.contains_range(error.range()))
        })
    };
    // alloc: the error's parts, for an error hir found
    let mut parser_errors: Vec<&SyntaxError> = Vec::new();
    // alloc: the error's parts, for an error hir found
    let mut literals: Vec<&SyntaxError> = Vec::new();
    // alloc: the error's parts, for an error hir found
    let mut validation: Vec<&SyntaxError> = Vec::new();
    let mut seen: HashSet<(String, TextRange)> = HashSet::default();
    for error in errors {
        // alloc: the error's text, for an error hir found
        if is_lexer_error(error) || !seen.insert((error.to_string(), error.range())) {
            continue;
        }
        // The parser reports between tokens; the checks of the tree after it, over a node or,
        // for an escape, inside a literal, whose errors the lexer reported first.
        let pos = error.range().start();
        let in_literal = f.toks.iter().find(|it| it.range.start() < pos && pos < it.range.end());
        match in_literal {
            Some(tok) if tok.kind.is_literal() => {
                if !lexer_errors.iter().any(|(_, range)| *range == tok.range) {
                    literals.push(error);
                }
            }
            _ if error.range().is_empty() => parser_errors.push(error),
            _ => validation.push(error),
        }
    }
    for error in literals {
        // alloc: the error's text, for an error hir found
        found.push(Found::new(Diag::error(None, error.to_string(), f.at(error.range())), Phase::Parse));
    }
    radix::sort_by_key(&mut parser_errors, |it| u32::from(it.range().start()));
    radix::sort_by_key(&mut validation, |it| u32::from(it.range().start()));

    for error in parser_errors {
        // alloc: the error's text, for an error hir found
        let message = error.to_string();
        if let Some(missing) = f.missing_item_type(&message, error.range().start()) {
            found.extend(missing);
            continue;
        }
        let (diag, recovered) = f.parser_error(&message, error.range().start());
        found.push(Found::new(diag, Phase::Parse));
        if !recovered {
            return Outcome::GaveUp;
        }
    }
    found.extend(validation.into_iter().filter_map(|error| f.validation_error(error)));
    Outcome::Parsed
}

impl File<'_, '_, '_> {
    /// An error of the `syntax` crate's checks of the tree, where rustc gives it: none where
    /// rustc's AST validation gives it (`checks::ast_validation`: E0449 of a trait impl's item,
    /// an `impl` type with no trait), E0224 of a `dyn` type with no trait as
    /// `rustc_hir_analysis` gives it, the rest in the `syntax` crate's words.
    fn validation_error(&self, error: &SyntaxError) -> Option<Found> {
        // alloc: the error's text, for an error hir found
        let message = error.to_string();
        let range = error.range();
        let at = self.at(range);
        match message.as_str() {
            "Unnecessary visibility qualifier" => None,
            "At least one trait is required for an object type" => {
                let node = self.tree.covering_element(range);
                match node.kind() {
                    SyntaxKind::DYN_TRAIT_TYPE => Some(Found::new(
                        Diag::error(Some("E0224"), "at least one trait is required for an object type", at),
                        Phase::Items,
                    )),
                    _ => None,
                }
            }
            // alloc: the error's text, for an error hir found
            _ => Some(Found::new(Diag::error(None, error.to_string(), at), Phase::Resolve)),
        }
    }
}

fn token_range(lexed: &LexedStr<'_>, i: usize) -> TextRange {
    let range = lexed.text_range(i);
    TextRange::new(to_size(range.start), to_size(range.end))
}

fn to_size(offset: usize) -> TextSize {
    TextSize::try_from(offset).expect("a file shorter than 4 GiB")
}

fn is_trivia(kind: SyntaxKind) -> bool {
    kind.is_trivia() || matches!(kind, SyntaxKind::SHEBANG | SyntaxKind::FRONTMATTER)
}

/// The punctuation rustc's lexer glues out of single characters written together
/// (`Token::glue`).
const GLUED: &[&str] = &[
    "==", "=>", "<=", "<<", "<-", ">=", ">>", "!=", "+=", "-=", "->", "*=", "/=", "%=", "^=", "&&",
    "&=", "||", "|=", "<<=", ">>=", "..", "...", "..=", "::",
];

fn tokens(lexed: &LexedStr<'_>, text: &str) -> Vec<Tok> {
    // alloc: the error's parts, for an error hir found
    let mut toks: Vec<Tok> = Vec::new();
    for i in 0..lexed.len() {
        let kind = lexed.kind(i);
        if is_trivia(kind) {
            continue;
        }
        let range = token_range(lexed, i);
        if kind.is_punct()
            && let Some(last) = toks.last_mut()
            && last.kind.is_punct()
            && last.range.end() == range.start()
            && GLUED.contains(&&text[TextRange::new(last.range.start(), range.end())])
        {
            last.range = TextRange::new(last.range.start(), range.end());
            continue;
        }
        toks.push(Tok { kind, range });
    }
    toks
}

/// Whether `text` crosses a line: rustc's `SourceMap::is_multiline`.
fn is_multiline(text: &str, range: TextRange) -> bool {
    text[range].contains('\n')
}

/// The range from `a`'s start to `b`'s, whichever comes first: rustc's `Span::until`.
fn until(a: TextRange, b: TextRange) -> TextRange {
    let (lo, hi) = (a.start(), b.start());
    TextRange::new(lo.min(hi), lo.max(hi))
}

const USED_KEYWORDS: &[&str] = &[
    "as", "break", "const", "continue", "crate", "else", "enum", "extern", "false", "fn", "for",
    "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref", "return",
    "self", "Self", "static", "struct", "super", "trait", "true", "type", "unsafe", "use", "where",
    "while",
];
const USED_KEYWORDS_2018: &[&str] = &["async", "await", "dyn"];
const UNUSED_KEYWORDS: &[&str] = &[
    "abstract", "become", "box", "do", "final", "macro", "override", "priv", "typeof", "unsized",
    "virtual", "yield",
];

fn is_used_keyword(name: &str, edition: Edition) -> bool {
    USED_KEYWORDS.contains(&name) || (edition.at_least_2018() && USED_KEYWORDS_2018.contains(&name))
}

fn is_unused_keyword(name: &str, edition: Edition) -> bool {
    UNUSED_KEYWORDS.contains(&name)
        || (edition.at_least_2018() && name == "try")
        || (edition.at_least_2024() && name == "gen")
}

/// rustc's `is_reserved_ident`.
fn is_reserved(name: &str, edition: Edition) -> bool {
    name == "_" || is_used_keyword(name, edition) || is_unused_keyword(name, edition)
}

impl<'a, 'c, 'db> File<'a, 'c, 'db> {
    fn at(&self, range: TextRange) -> Span {
        self.cx.span(InFile::new(self.file, range))
    }

    fn eof(&self) -> TextRange {
        TextRange::empty(TextSize::of(self.text))
    }

    fn src(&self, tok: Tok) -> &'a str {
        &self.text[tok.range]
    }

    fn is_ident(&self, tok: Tok) -> bool {
        !tok.kind.is_literal()
            && tok.kind != SyntaxKind::LIFETIME_IDENT
            && self.src(tok).starts_with(|c: char| c.is_alphabetic() || c == '_')
    }

    fn is_ident_named(&self, tok: Option<Tok>, name: &str) -> bool {
        tok.is_some_and(|it| self.is_ident(it) && self.src(it) == name)
    }

    /// rustc's `token_descr`.
    fn describe(&self, tok: Option<Tok>) -> String {
        // alloc: the error's text, for an error hir found
        let Some(tok) = tok else { return "`<eof>`".to_owned() };
        let s = self.src(tok);
        if matches!(tok.kind, SyntaxKind::OUTER_DOC_COMMENT | SyntaxKind::INNER_DOC_COMMENT) {
            // alloc: the error's text, for an error hir found
            return format!("doc comment `{s}`");
        }
        if self.is_ident(tok) {
            if s == "_" {
                // alloc: the error's text, for an error hir found
                return format!("reserved identifier `{s}`");
            }
            if is_used_keyword(s, self.edition) {
                // alloc: the error's text, for an error hir found
                return format!("keyword `{s}`");
            }
            if is_unused_keyword(s, self.edition) {
                // alloc: the error's text, for an error hir found
                return format!("reserved keyword `{s}`");
            }
        }
        // alloc: the error's text, for an error hir found
        format!("`{s}`")
    }

    /// rustc's `Token::can_begin_expr`.
    fn can_begin_expr(&self, tok: Option<Tok>) -> bool {
        let Some(tok) = tok else { return false };
        let s = self.src(tok);
        if self.is_ident(tok) {
            return !is_reserved(s, self.edition)
                || ["super", "self", "Self", "crate"].contains(&s)
                || [
                    "async", "do", "box", "break", "const", "continue", "false", "for", "gen",
                    "if", "let", "loop", "match", "move", "return", "true", "try", "unsafe",
                    "while", "yield", "safe", "static",
                ]
                .contains(&s);
        }
        tok.kind.is_literal()
            || tok.kind == SyntaxKind::LIFETIME_IDENT
            || ["(", "{", "[", "!", "-", "*", "|", "||", "&", "&&", "..", "...", "..=", "<", "<<", "::", "#"]
                .contains(&s)
    }

    /// rustc's `Token::can_begin_item`.
    fn can_begin_item(&self, tok: Option<Tok>) -> bool {
        tok.is_some_and(|tok| {
            self.is_ident(tok)
                && [
                    "fn", "use", "struct", "enum", "pub", "trait", "extern", "impl", "unsafe",
                    "const", "safe", "static", "union", "macro", "mod", "type",
                ]
                .contains(&self.src(tok))
        })
    }

    /// The token at `pos` and the one before it, as rustc's parser has them when it reports
    /// there: `None` for the end of the file.
    fn around(&self, pos: TextSize) -> (Option<Tok>, Option<Tok>) {
        let next = self.toks.partition_point(|it| it.range.end() <= pos);
        let prev = next.checked_sub(1).map(|it| self.toks[it]);
        (prev, self.toks.get(next).copied())
    }

    fn range_of(&self, tok: Option<Tok>) -> TextRange {
        tok.map_or(self.eof(), |it| it.range)
    }

    /// An error of blaze's lexer, as rustc's lexer gives it, and whether rustc stops there.
    fn lexer_error(
        &self,
        kind: SyntaxKind,
        range: TextRange,
        msg: &str,
        last_lifetime: Option<TextRange>,
    ) -> (Diag, bool) {
        let one = TextSize::from(1);
        let after_prefix = TextRange::new(range.start() + one, range.end());
        let fatal = |code: &str, message: &str, range: TextRange| {
            (Diag::error(Some(code), message, self.at(range)), true)
        };
        match msg {
            "Missing trailing `*/` symbols to terminate the block comment" => {
                (self.unterminated_block_comment(kind, range), true)
            }
            "Missing trailing `'` symbol to terminate the character literal" => {
                let mut diag =
                    Diag::error(Some("E0762"), "unterminated character literal", self.at(range));
                if let Some(lifetime) = last_lifetime {
                    diag.multipart_suggestion(
                        "if you meant to write a string literal, use double quotes",
                        // alloc: the error's parts, for an error hir found
                        vec![
                            // alloc: the error's text, for an error hir found
                            (self.at(lifetime), "\"".to_owned()),
                            // alloc: the error's text, for an error hir found
                            (self.at(TextRange::at(range.start(), one)), "\"".to_owned()),
                        ],
                        Applicability::MaybeIncorrect,
                    );
                }
                (diag, true)
            }
            "Missing trailing `'` symbol to terminate the byte literal" => {
                fatal("E0763", "unterminated byte constant", after_prefix)
            }
            "Missing trailing `\"` symbol to terminate the string literal" => {
                fatal("E0765", "unterminated double quote string", range)
            }
            "Missing trailing `\"` symbol to terminate the byte string literal" => {
                fatal("E0766", "unterminated double quote byte string", after_prefix)
            }
            "Missing trailing `\"` symbol to terminate the C string literal" => {
                fatal("E0767", "unterminated C string", after_prefix)
            }
            "Invalid raw string literal" => (self.raw_string_error(range), true),
            "Missing digits after the integer base prefix" => {
                (Diag::error(Some("E0768"), "no valid digits found for number", self.at(range)), false)
            }
            "Missing digits after the exponent symbol" => (
                Diag::error(None, "expected at least one digit in exponent", self.at(range)),
                false,
            ),
            "Lifetime name cannot start with a number" => {
                (Diag::error(None, "lifetimes cannot start with a number", self.at(range)), false)
            }
            _ => (Diag::error(None, msg, self.at(range)), false),
        }
    }

    /// rustc's `report_unterminated_block_comment`.
    fn unterminated_block_comment(&self, kind: SyntaxKind, range: TextRange) -> Diag {
        let msg = match kind {
            SyntaxKind::OUTER_DOC_COMMENT | SyntaxKind::INNER_DOC_COMMENT => {
                "unterminated block doc-comment"
            }
            _ => "unterminated block comment",
        };
        let start = range.start();
        let mut diag = Diag::error(Some("E0758"), msg, self.at(TextRange::new(start, self.eof().end())));
        // alloc: the error's parts, for an error hir found
        let mut opens = Vec::new();
        let mut last_nested = None;
        let mut chars = self.text[usize::from(start)..].char_indices().peekable();
        while let Some((idx, c)) = chars.next() {
            match chars.peek() {
                Some((_, '*')) if c == '/' => opens.push(idx),
                Some((_, '/')) if c == '*' => last_nested = opens.pop().map(|open| (open, idx)),
                _ => {}
            }
        }
        if let Some((open, close)) = last_nested {
            let two = |at: usize| self.at(TextRange::at(start + to_size(at), TextSize::from(2)));
            diag.span_label(two(0), msg);
            diag.span_label(
                two(open),
                "...as last nested comment starts here, maybe you want to close this instead?",
            );
            diag.span_label(two(close), "...and last nested comment terminates here.");
        }
        diag
    }

    /// rustc's `report_raw_str_error`, from `rustc_lexer::validate_raw_str`'s reasons.
    fn raw_string_error(&self, range: TextRange) -> Diag {
        let token = &self.text[usize::from(range.start())..];
        let prefix = if token.starts_with('r') { 1 } else { 2 };
        let start = range.start();
        let rest = &token[prefix..];
        let hashes = rest.bytes().take_while(|&it| it == b'#').count();
        let after_hashes = &rest[hashes..];
        let starter = after_hashes.chars().next();
        if starter != Some('"') {
            let bad = escaped_char(starter.unwrap_or('\0'));
            // alloc: the error's text, for an error hir found
            let message = format!(
                "found invalid character; only `#` is allowed in raw string delimitation: {bad}"
            );
            return Diag::error(None, message, self.at(range));
        }
        if hashes > 255 {
            // alloc: the error's text, for an error hir found
            let message = format!(
                "too many `#` symbols: raw strings may be delimited by up to 255 `#` symbols, but found {hashes}"
            );
            return Diag::error(None, message, self.at(range));
        }
        // The closing quotes the string has, and the most `#`s after one of them.
        let body_at = prefix + hashes + 1;
        let mut max_hashes = 0;
        let mut possible: Option<usize> = None;
        let bytes = token.as_bytes();
        let mut at = body_at;
        while let Some(quote) = token[at..].find('"') {
            at += quote + 1;
            let found = bytes[at..].iter().take(hashes).take_while(|&&it| it == b'#').count();
            at += found;
            if found > max_hashes {
                possible = Some(at - found);
                max_hashes = found;
            }
        }
        let here = self.at(TextRange::empty(start));
        // clone: on the error path, for an error hir found
        let mut diag = Diag::error(Some("E0748"), "unterminated raw string", here.clone());
        diag.span_label(here, "unterminated raw string");
        if hashes > 0 {
            // alloc: the error's text, for an error hir found
            diag.note(format!("this raw string should be terminated with `\"{}`", "#".repeat(hashes)));
        }
        if let Some(offset) = possible {
            let lo = start + to_size(offset);
            diag.suggestion_verbose(
                self.at(TextRange::at(lo, to_size(max_hashes))),
                "consider terminating the string here",
                "#".repeat(hashes),
                Applicability::MaybeIncorrect,
            );
        }
        diag
    }

    /// rustc's token trees of the file (`lex_token_trees`): the errors of its delimiters, the
    /// mismatched closing ones first.
    fn delimiters(&self) -> Vec<Diag> {
        let mut trees = Trees { file: self, at: 0, info: DelimInfo::default() };
        let result = trees.trees(false);
        let info = &trees.info;
        let mut errors: Vec<Diag> = info
            .unmatched
            .iter()
            .filter_map(|unmatched| {
                let found = unmatched.found?;
                let delimiter = closer(found);
                let mut diag = Diag::error(
                    None,
                    // alloc: the error's text, for an error hir found
                    format!("mismatched closing delimiter: `{delimiter}`"),
                    self.at(unmatched.found_span),
                );
                if let Some(unclosed) = unmatched.unclosed {
                    diag.primary(self.at(unclosed));
                }
                diag.span_label(self.at(unmatched.found_span), "mismatched closing delimiter");
                if let Some(candidate) = unmatched.candidate {
                    diag.span_label(self.at(candidate), "closing delimiter possibly meant for this");
                }
                if let Some(unclosed) = unmatched.unclosed {
                    diag.span_label(self.at(unclosed), "unclosed delimiter");
                }
                Some(diag)
            })
            // alloc: the error's parts, for an error hir found
            .collect();
        if let Err(error) = result {
            errors.push(error);
        }
        errors
    }

    /// rustc's `same_indentation_level`: whether the lines of `a` and `b` are indented alike up
    /// to them.
    fn same_indentation(&self, a: TextRange, b: TextRange) -> bool {
        self.margin(a) == self.margin(b)
    }

    fn margin(&self, range: TextRange) -> usize {
        let before = &self.text[..usize::from(range.start())];
        let line = before.rsplit_once('\n').map_or(before, |(_, last)| last);
        line.split_once(|c: char| !c.is_whitespace()).map_or(line, |(indent, _)| indent).len()
    }

    /// A parser error of blaze's, as rustc's parser gives it, and whether rustc recovers from
    /// it and goes on parsing the file.
    fn parser_error(&self, msg: &str, pos: TextSize) -> (Diag, bool) {
        let (prev, token) = self.around(pos);
        if let Some(expected) = msg.strip_prefix("expected ").and_then(expected_token) {
            return self.expected_one(expected, prev, token);
        }
        if msg.starts_with("expected item, found `;`") {
            return self.expected_item(prev, token);
        }
        match msg {
            "expected an item" => {
                let (prev, token) = self.start_of_error_node(pos).unwrap_or((prev, token));
                self.expected_item(prev, token)
            }
            "expected expression" => {
                let span = self.at(self.range_of(token));
                // alloc: the error's text, for an error hir found
                let message = format!("expected expression, found {}", self.describe(token));
                // clone: on the error path, for an error hir found
                let mut diag = Diag::error(None, message, span.clone());
                diag.span_label(span, "expected expression");
                (diag, false)
            }
            "expected a type" | "expected type" => {
                let span = self.at(self.range_of(token));
                // alloc: the error's text, for an error hir found
                let message = format!("expected type, found {}", self.describe(token));
                // clone: on the error path, for an error hir found
                let mut diag = Diag::error(None, message, span.clone());
                diag.span_label(span, "expected type");
                (diag, false)
            }
            "expected pattern" => {
                let span = self.at(self.range_of(token));
                // alloc: the error's text, for an error hir found
                let message = format!("expected pattern, found {}", self.describe(token));
                // clone: on the error path, for an error hir found
                let mut diag = Diag::error(None, message, span.clone());
                diag.span_label(span, "expected pattern");
                (diag, false)
            }
            _ => {
                let at = token.map_or(TextRange::empty(pos), |it| it.range);
                (Diag::error(None, msg, self.at(at)), false)
            }
        }
    }

    /// rustc's `recover_missing_global_item_type`, which recovers: its error is stashed and
    /// written as item collection takes it back (`infer_placeholder_type`). An item with no
    /// body keeps the parser's suggestion, its type being an error; of one with a body rustc
    /// suggests the type it infers, which blaze does not give here, and says so.
    fn missing_item_type(&self, msg: &str, pos: TextSize) -> Option<Vec<Found>> {
        let keyword = match msg {
            "missing type for `const`" => "const",
            "missing type for `static`" => "static",
            _ => return None,
        };
        let (prev, next) = self.around(pos);
        let prev = prev.expect("the parser reports a missing type after the item's name");
        let before = &self.toks[..self.toks.partition_point(|it| it.range.end() <= pos)];
        let at = before
            .iter()
            .rposition(|&it| self.is_ident_named(Some(it), keyword))
            .expect("the parser reports a missing type in a `const` or `static` item");
        let mutable =
            keyword == "static" && self.is_ident_named(before.get(at + 1).copied(), "mut");
        let kind = if mutable { "static mut" } else { keyword };
        let span = self.at(TextRange::empty(prev.range.end()));
        // alloc: the error's text, for an error hir found
        let mut diag = Diag::error(None, format!("missing type for `{kind}` item"), span.clone());
        if next.is_some_and(|it| self.src(it) == "=") {
            // alloc: the error's text, for an error hir found
            let message = format!(
                "blaze: rustc suggests the type it infers for this `{kind}` item, which blaze does not infer here"
            );
            // alloc: the errors found
            return Some(vec![
                Found::new(diag, Phase::Items),
                Found::new(Diag::error(None, message, span), Phase::Items),
            ]);
        }
        let placeholders = Applicability::HasPlaceholders;
        diag.suggestion_verbose(span, "provide a type for the item", ": <type>", placeholders);
        // alloc: the error found
        Some(vec![Found::new(diag, Phase::Items)])
    }

    /// rustc's `expected_one_of_not_found` of one expected token, with its recovery of a
    /// missing or mistyped `;`.
    fn expected_one(&self, expected: &str, prev: Option<Tok>, token: Option<Tok>) -> (Diag, bool) {
        let found = self.describe(token);
        let token_range = self.range_of(token);
        let src = |tok: Option<Tok>| tok.map(|it| self.src(it));
        if expected == "`;`"
            && let Some(prev) = prev
            && is_multiline(self.text, until(prev.range, token_range))
            && !(matches!(src(token), Some("," | ":")) && src(Some(prev)) == Some(")"))
        {
            let next = token.and_then(|it| {
                let at = self.toks.partition_point(|t| t.range.start() <= it.range.start());
                self.toks.get(at).copied()
            });
            // alloc: the error's text, for an error hir found
            let message = format!("expected `;`, found {found}");
            if matches!(src(token), Some("," | ":"))
                && (src(next) == Some("}") || (self.can_begin_expr(next) && src(next) != Some(":")))
            {
                let span = self.at(token_range);
                // clone: on the error path, for an error hir found
                let mut diag = Diag::error(None, message, span.clone());
                diag.suggestion_with_style(
                    // alloc: the error's text, for an error hir found
                    vec![(span, ";".to_owned())],
                    "change this to `;`",
                    Applicability::MachineApplicable,
                    SuggestionStyle::HideCodeInline,
                );
                return (diag, true);
            }
            let after_prev = TextRange::new(prev.range.end(), token_range.start());
            if src(token) == Some("}")
                || ((self.can_begin_expr(token) || self.can_begin_item(token))
                    && !matches!(src(token), Some(";" | "#")))
                || (is_multiline(self.text, after_prev) && src(token) == Some("#"))
            {
                let span = self.at(TextRange::empty(prev.range.end()));
                // clone: on the error path, for an error hir found
                let mut diag = Diag::error(None, message, span.clone());
                diag.span_label(self.at(token_range), "unexpected token");
                diag.suggestion_with_style(
                    // alloc: the error's text, for an error hir found
                    vec![(span, ";".to_owned())],
                    "add `;` here",
                    Applicability::MachineApplicable,
                    SuggestionStyle::HideCodeInline,
                );
                return (diag, true);
            }
        }

        // alloc: the error's text, for an error hir found
        let message = format!("expected {expected}, found {found}");
        // alloc: the error's text, for an error hir found
        let label = format!("expected {expected}");
        let mut diag = Diag::error(None, message, self.at(token_range));
        if let Some(prev) = prev {
            let symbol = self.src(prev);
            if self.is_ident(prev) && ["def", "fun", "func", "function"].contains(&symbol) {
                diag.suggestion_with_style(
                    // alloc: the error's text, for an error hir found
                    vec![(self.at(prev.range), "fn".to_owned())],
                    // alloc: the error's text, for an error hir found
                    format!("write `fn` instead of `{symbol}` to declare a function"),
                    Applicability::MachineApplicable,
                    SuggestionStyle::HideCodeInline,
                );
            }
            if self.is_ident_named(Some(prev), "public")
                && (self.can_begin_item(token) || src(token) == Some("("))
            {
                diag.suggestion_with_style(
                    // alloc: the error's text, for an error hir found
                    vec![(self.at(prev.range), "pub".to_owned())],
                    "write `pub` instead of `public` to make the item public",
                    Applicability::MachineApplicable,
                    SuggestionStyle::HideCodeInline,
                );
            }
        }
        match prev {
            None => {
                diag.span_label(self.at(token_range), label);
            }
            Some(prev) => {
                let sp = match token {
                    None => prev.range,
                    Some(_) => TextRange::empty(prev.range.end()),
                };
                let between = TextRange::new(
                    token_range.end().min(sp.start()),
                    token_range.end().max(sp.start()),
                );
                if !is_multiline(self.text, between) {
                    diag.span_label(self.at(token_range), label);
                } else {
                    diag.span_label(self.at(sp), label);
                    diag.span_label(self.at(token_range), "unexpected token");
                }
            }
        }
        (diag, false)
    }

    /// rustc's `parse_mod` at a token that starts no item, and `maybe_consume_incorrect_semicolon`
    /// at a `;`.
    fn expected_item(&self, prev: Option<Tok>, token: Option<Tok>) -> (Diag, bool) {
        let span = self.at(self.range_of(token));
        if token.is_some_and(|it| self.src(it) == ";") {
            // clone: on the error path, for an error hir found
            let mut diag = Diag::error(None, "expected item, found `;`", span.clone());
            diag.suggestion_verbose(span, "remove this semicolon", "", Applicability::MachineApplicable);
            if let Some(name) = prev.and_then(|it| self.item_ending_at(it.range.end())) {
                // alloc: the error's text, for an error hir found
                diag.help(format!("{name} declarations are not followed by a semicolon"));
            }
            return (diag, true);
        }
        // alloc: the error's text, for an error hir found
        let message = format!("expected item, found {}", self.describe(token));
        // clone: on the error path, for an error hir found
        let mut diag = Diag::error(None, message, span.clone());
        let is_let = self.is_ident_named(token, "let");
        let next = token.and_then(|it| {
            let at = self.toks.partition_point(|t| t.range.start() <= it.range.start());
            self.toks.get(at).copied()
        });
        let is_let_mut = is_let && self.is_ident_named(next, "mut");
        let let_has_ident = is_let
            && !is_let_mut
            && next.is_some_and(|it| self.is_ident(it) && !is_reserved(self.src(it), self.edition));
        let label = match is_let {
            true => "`let` cannot be used for global variables",
            false => "expected item",
        };
        // clone: on the error path, for an error hir found
        diag.span_label(span.clone(), label);
        if is_let_mut {
            diag.help("consider using `static` and a `Mutex` instead of `let mut`");
        } else if let_has_ident {
            diag.suggestion_with_style(
                // alloc: the error's text, for an error hir found
                vec![(span, "static".to_owned())],
                "consider using `static` or `const` instead of `let`",
                Applicability::MaybeIncorrect,
                SuggestionStyle::HideCodeInline,
            );
        } else if is_let {
            diag.help("consider using `static` or `const` instead of `let`");
        }
        diag.note(
            "for a full list of items that can appear in modules, see <https://doc.rust-lang.org/reference/items.html>",
        );
        (diag, false)
    }

    /// Where an error node of blaze's parser that ends at `pos` starts (a path it read where an
    /// item was expected), as the token and the one before it: rustc reports at the path.
    fn start_of_error_node(&self, pos: TextSize) -> Option<(Option<Tok>, Option<Tok>)> {
        let token = self.tree.token_at_offset(pos).left_biased()?;
        let node = token
            .parent_ancestors()
            .take_while(|it| it.text_range().end() == pos)
            .filter(|it| it.kind() == SyntaxKind::ERROR && it.text_range().start() < pos)
            .last()?;
        Some(self.around(node.text_range().start()))
    }

    /// rustc's `ItemKind::descr` of the item that ends at `end`, "braced struct" for a struct.
    fn item_ending_at(&self, end: TextSize) -> Option<&'static str> {
        let token = self.tree.token_at_offset(end).left_biased()?;
        let item = token
            .parent_ancestors()
            .filter_map(ast::Item::cast)
            .find(|it| it.syntax().text_range().end() == end)?;
        Some(match item {
            ast::Item::Const(_) => "constant item",
            ast::Item::Enum(_) => "enum",
            ast::Item::ExternBlock(_) => "extern block",
            ast::Item::ExternCrate(_) => "extern crate",
            ast::Item::Fn(_) => "function",
            ast::Item::Impl(_) => "implementation",
            ast::Item::MacroCall(_) => "item macro invocation",
            ast::Item::MacroRules(_) | ast::Item::MacroDef(_) => "macro definition",
            ast::Item::Module(_) => "module",
            ast::Item::Static(_) => "static item",
            ast::Item::Struct(_) => "braced struct",
            ast::Item::Trait(_) => "trait",
            ast::Item::TypeAlias(_) => "type alias",
            ast::Item::Union(_) => "union",
            ast::Item::Use(_) => "`use` import",
            ast::Item::AsmExpr(_) => "global asm item",
        })
    }
}

/// rustc's `TokenType` of what blaze's parser says it expected (`expected {kind:?}`), as
/// rustc's messages name it.
fn expected_token(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "SEMICOLON" => "`;`",
        "COMMA" => "`,`",
        "COLON" => "`:`",
        "L_PAREN" => "`(`",
        "R_PAREN" => "`)`",
        "L_CURLY" => "`{`",
        "R_CURLY" => "`}`",
        "L_BRACK" => "`[`",
        "R_BRACK" => "`]`",
        "R_ANGLE" => "`>`",
        "BANG" => "`!`",
        "EQ" => "`=`",
        "FAT_ARROW" => "`=>`",
        "DOT" => "`.`",
        "DOT2" => "`..`",
        "IDENT" => "identifier",
        "CONST_KW" => "`const`",
        "IN_KW" => "`in`",
        "MACRO_KW" => "`macro`",
        _ => return None,
    })
}

/// rustc's `escaped_char`.
fn escaped_char(c: char) -> String {
    match c {
        // alloc: the error's text, for an error hir found
        '\u{20}'..='\u{7e}' => c.to_string(),
        // alloc: the error's text, for an error hir found
        _ => c.escape_default().to_string(),
    }
}

fn is_open(kind: SyntaxKind) -> bool {
    matches!(kind, SyntaxKind::L_PAREN | SyntaxKind::L_CURLY | SyntaxKind::L_BRACK)
}

fn is_close(kind: SyntaxKind) -> bool {
    matches!(kind, SyntaxKind::R_PAREN | SyntaxKind::R_CURLY | SyntaxKind::R_BRACK)
}

/// Whether `open` and `close` are the same delimiter.
fn same_delimiter(open: SyntaxKind, close: SyntaxKind) -> bool {
    matches!(
        (open, close),
        (SyntaxKind::L_PAREN | SyntaxKind::R_PAREN, SyntaxKind::R_PAREN | SyntaxKind::L_PAREN)
            | (SyntaxKind::L_CURLY | SyntaxKind::R_CURLY, SyntaxKind::R_CURLY | SyntaxKind::L_CURLY)
            | (SyntaxKind::L_BRACK | SyntaxKind::R_BRACK, SyntaxKind::R_BRACK | SyntaxKind::L_BRACK)
    )
}

fn closer(kind: SyntaxKind) -> &'static str {
    match kind {
        SyntaxKind::L_PAREN | SyntaxKind::R_PAREN => ")",
        SyntaxKind::L_BRACK | SyntaxKind::R_BRACK => "]",
        _ => "}",
    }
}

/// rustc's `UnmatchedDelim`.
struct Unmatched {
    found: Option<SyntaxKind>,
    found_span: TextRange,
    unclosed: Option<TextRange>,
    candidate: Option<TextRange>,
}

/// rustc's `TokenTreeDiagInfo`.
#[derive(Default)]
struct DelimInfo {
    open: Vec<(SyntaxKind, TextRange)>,
    unmatched: Vec<Unmatched>,
    last_unclosed_found: Option<TextRange>,
    empty_blocks: Vec<TextRange>,
    matching_blocks: Vec<(TextRange, TextRange)>,
}

/// rustc's `Lexer` as it builds token trees (`lexer/tokentrees.rs`).
struct Trees<'f, 'a, 'c, 'db> {
    file: &'f File<'a, 'c, 'db>,
    at: usize,
    info: DelimInfo,
}

impl Trees<'_, '_, '_, '_> {
    fn current(&self) -> Option<Tok> {
        self.file.toks.get(self.at).copied()
    }

    /// The trees up to a closing delimiter (in a delimited run) or the end of the file, and
    /// whether there were any.
    fn trees(&mut self, delimited: bool) -> Result<bool, Diag> {
        let mut any = false;
        loop {
            match self.current() {
                Some(tok) if is_open(tok.kind) => {
                    self.open_delim(tok)?;
                    any = true;
                }
                Some(tok) if is_close(tok.kind) => {
                    return match delimited {
                        true => Ok(any),
                        false => Err(self.close_delim_err(tok)),
                    };
                }
                None => {
                    return match delimited {
                        true => Err(self.eof_err()),
                        false => Ok(any),
                    };
                }
                Some(_) => {
                    self.at += 1;
                    any = true;
                }
            }
        }
    }

    fn open_delim(&mut self, open: Tok) -> Result<(), Diag> {
        self.info.open.push((open.kind, open.range));
        self.at += 1;
        let nonempty = self.trees(true)?;
        let close = self.current().expect("a delimited run ends at a closing delimiter");
        if same_delimiter(open.kind, close.kind) {
            self.info.open.pop();
            if open.kind == SyntaxKind::L_CURLY {
                let block = open.range.cover(close.range);
                if !nonempty && !is_multiline(self.file.text, block) {
                    self.info.empty_blocks.push(block);
                }
                self.info.matching_blocks.push((open.range, close.range));
            }
            self.at += 1;
            return Ok(());
        }
        if self.info.last_unclosed_found != Some(close.range) {
            self.info.last_unclosed_found = Some(close.range);
            let unclosed = self.info.open.last().map(|it| it.1);
            let candidate = self
                .info
                .open
                .iter()
                .filter(|(kind, range)| {
                    self.file.same_indentation(close.range, *range)
                        && same_delimiter(*kind, close.kind)
                })
                .map(|it| it.1)
                .last();
            self.info.open.pop();
            self.info.unmatched.push(Unmatched {
                found: Some(close.kind),
                found_span: close.range,
                unclosed,
                candidate,
            });
        } else {
            self.info.open.pop();
        }
        if !self.info.open.iter().any(|(kind, _)| same_delimiter(*kind, close.kind)) {
            self.at += 1;
        }
        Ok(())
    }

    fn eof_err(&mut self) -> Diag {
        const SHOWN: usize = 5;
        let file = self.file;
        let eof = file.eof();
        let mut diag = Diag::error(None, "this file contains an unclosed delimiter", file.at(eof));
        let shown = SHOWN.min(self.info.open.len());
        for &(_, span) in &self.info.open[..shown] {
            diag.span_label(file.at(span), "unclosed delimiter");
            self.info.unmatched.push(Unmatched {
                found: None,
                found_span: eof,
                unclosed: Some(span),
                candidate: None,
            });
        }
        if let Some(&(_, span)) = self.info.open.get(SHOWN)
            && self.info.open.len() >= SHOWN + 2
        {
            let more = self.info.open.len() - SHOWN;
            // alloc: the error's text, for an error hir found
            diag.span_label(file.at(span), format!("another {more} unclosed delimiters begin from here"));
        }
        if let Some(&(delim, _)) = self.info.open.last() {
            self.suspicious_mismatch_block(&mut diag, delim);
        }
        diag
    }

    fn close_delim_err(&mut self, tok: Tok) -> Diag {
        let file = self.file;
        // alloc: the error's text, for an error hir found
        let message = format!("unexpected closing delimiter: `{}`", file.src(tok));
        let mut diag = Diag::error(None, message, file.at(tok.range));
        if !self.missing_open_delim(&mut diag) {
            self.suspicious_mismatch_block(&mut diag, tok.kind);
        }
        diag.span_label(file.at(tok.range), "unexpected closing delimiter");
        diag
    }

    /// rustc's `report_missing_open_delim`.
    fn missing_open_delim(&mut self, diag: &mut Diag) -> bool {
        let file = self.file;
        let mut reported = false;
        self.info.unmatched.retain(|unmatched| {
            let open = match unmatched.found {
                Some(SyntaxKind::R_PAREN | SyntaxKind::L_PAREN) => "(",
                Some(SyntaxKind::R_BRACK | SyntaxKind::L_BRACK) => "[",
                _ => return true,
            };
            if let Some(unclosed) = unmatched.unclosed {
                diag.span_label(file.at(unclosed), "the nearest open delimiter");
            }
            diag.span_label(
                file.at(TextRange::empty(unmatched.found_span.start())),
                // alloc: the error's text, for an error hir found
                format!("missing open `{open}` for this delimiter"),
            );
            reported = true;
            false
        });
        reported
    }

    /// rustc's `report_suspicious_mismatch_block`.
    fn suspicious_mismatch_block(&self, diag: &mut Diag, delim: SyntaxKind) {
        let file = self.file;
        let info = &self.info;
        let mut matched: Vec<(TextRange, bool)> = info
            .matching_blocks
            .iter()
            .map(|&(open, close)| {
                (TextRange::new(open.start(), close.start()), file.same_indentation(open, close))
            })
            // alloc: the error's parts, for an error hir found
            .collect();
        radix::sort_copy_by_key(&mut matched, |(range, _)| u32::from(range.start()));
        for i in 0..matched.len() {
            let (block, same) = matched[i];
            if same {
                for inner in &mut matched[i + 1..] {
                    if block.contains_range(inner.0) && !inner.1 {
                        inner.1 = true;
                    }
                }
            }
        }
        let candidate = matched.into_iter().rev().find(|(_, same)| !same).map(|it| it.0);
        if let Some(block) = candidate {
            diag.span_label(
                file.at(TextRange::empty(block.start())),
                "this delimiter might not be properly closed...",
            );
            diag.span_label(
                file.at(TextRange::empty(block.end())),
                "...as it matches this but it has different indentation",
            );
            if delim == SyntaxKind::L_CURLY || delim == SyntaxKind::R_CURLY {
                if let Some(&empty) = info.empty_blocks.iter().find(|it| block.contains_range(**it)) {
                    diag.span_label(file.at(empty), "block is empty, you might have not meant to close it");
                }
            }
        } else if let Some(&(open, close)) = info.matching_blocks.last()
            && info.open.is_empty()
            && info.empty_blocks.iter().all(|it| *it != open.cover(close))
        {
            diag.span_label(file.at(open), "this opening brace...");
            diag.span_label(file.at(close), "...matches this closing brace");
        }
    }
}
