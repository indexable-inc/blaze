//! Implementation of incremental re-parsing.
//!
//! The smallest element holding the edit that can be parsed again alone is, and its new tree
//! is spliced into the old one ([`FlatTree::replace_subtree`]), the first of:
//!   - the token, when the edit keeps it one token of its kind (an identifier's letter);
//!   - in an outline, a `fn` body, lazy or not, which is a lazy node again;
//!   - the nearest `{ ... }` block or list [`Reparser::for_node`] parses;
//!   - an item at the top level of the file.
//!
//! Each is checked to parse alone as it does in the file: its new text lexes alone as it does
//! between its neighbours, its tokens balance, the tokens the parser may look at from outside
//! it are unchanged, and its parse is one node of its kind taking all of its text. When none
//! is, [`incremental_reparse`] is `None` and the caller parses the whole text.
//!
//! Several edits made at once ([`reparse_all`]) are each parsed again so, and spliced in one
//! copy of the tree ([`FlatTree::splice_all`]); edits whose checks read each other's text are
//! parsed again as one.

use std::cell::Cell;

use parser::{Edition, LexedStr, OutlineStep, Reparser, Step};
use syntax_tree::{FlatTree, Language, Splice, Text};

use crate::{
    AstNode, SyntaxError,
    SyntaxKind::{self, *},
    SyntaxTreeBuilder, T, TextRange, TextSize, ast,
    parsing::{build_node, lexer_errors},
    raw::joined,
    syntax_node::RustLanguage,
};

/// How many tokens into a node the parser may look from outside it: the node's first ones
/// must not change for the node to be parsed again alone.
const LOOKAHEAD: usize = 4;

/// How many tokens after a top-level item its parse is given, to stop where it stops in the
/// file.
const AFTER_ITEM: usize = 16;

/// The tree of `tree`'s text with `delete` replaced by `insert`, its errors (`errors`, those
/// of `tree`, merged with those of what was parsed again) and the range of the old text parsed
/// again. `outline` is the edition of an outline.
pub(crate) fn incremental_reparse(
    tree: &FlatTree,
    delete: TextRange,
    insert: &str,
    errors: impl IntoIterator<Item = SyntaxError>,
    edition: Edition,
    outline: Option<Edition>,
) -> Option<(FlatTree, Vec<SyntaxError>, TextRange)> {
    // An outline's lazy nodes are parsed in its edition: another one is a new parse.
    if outline.is_some_and(|it| it != edition) {
        return None;
    }
    let source = tree.source();
    let new_source = joined(&[
        &source[..usize::from(delete.start())],
        insert,
        &source[usize::from(delete.end())..],
    ]);
    reparse_onto(tree, delete, insert, &new_source, errors, edition, outline)
}

/// [`incremental_reparse`] of the edit that makes `tree`'s text `new_source`, whose tree shares
/// `new_source`: `delete` of the old text replaced by what is at `delete.start()` in the new.
pub(crate) fn incremental_reparse_onto(
    tree: &FlatTree,
    delete: TextRange,
    new_source: &Text,
    errors: impl IntoIterator<Item = SyntaxError>,
    edition: Edition,
    outline: Option<Edition>,
) -> Option<(FlatTree, Vec<SyntaxError>, TextRange)> {
    if outline.is_some_and(|it| it != edition) {
        return None;
    }
    let start = usize::from(delete.start());
    let insert_len = (new_source.len() + usize::from(delete.len()))
        .checked_sub(tree.source().len())
        .expect("the edit deletes no more than the old text has");
    let insert = &new_source[start..start + insert_len];
    reparse_onto(tree, delete, insert, new_source, errors, edition, outline)
}

fn reparse_onto(
    tree: &FlatTree,
    delete: TextRange,
    insert: &str,
    new_source: &Text,
    errors: impl IntoIterator<Item = SyntaxError>,
    edition: Edition,
    outline: Option<Edition>,
) -> Option<(FlatTree, Vec<SyntaxError>, TextRange)> {
    let edit = Edit::new(tree, delete, insert, new_source, 0, edition, outline.is_some());
    let mut unit = edit.unit()?;
    let merged = merge_errors(errors, std::slice::from_mut(&mut unit))?;
    let tree = tree.splice_all(&[unit.splice(new_source)], new_source.clone());
    Some((tree, merged, unit.range))
}

/// The tree of `tree`'s text with each of `edits` made, which is `new_source` (whose tree
/// shares it), and its errors (`errors`, those of `tree`, merged with those of what was parsed
/// again). The edits are in order and none overlaps another, `delete` of the old text replaced
/// by `insert`. Each is parsed again as [`incremental_reparse`] parses one, and the old tree is
/// copied once with every new piece in it. Edits whose checks read another's text (a token
/// next to it, the item it is in) are one edit from the first's start to the last's end. `None`
/// when one of them can not be parsed again alone: the caller parses the whole text.
pub(crate) fn reparse_all(
    tree: &FlatTree,
    edits: &[(TextRange, &str)],
    new_source: &Text,
    errors: impl IntoIterator<Item = SyntaxError>,
    edition: Edition,
    outline: Option<Edition>,
) -> Option<(FlatTree, Vec<SyntaxError>)> {
    if outline.is_some_and(|it| it != edition) {
        return None;
    }
    let grown = |&(delete, insert): &(TextRange, &str)| {
        insert.len() as i64 - u32::from(delete.len()) as i64
    };
    // alloc: one of each an edit, once a call
    let (mut units, mut groups): (Vec<Unit>, Vec<Group>) = (Vec::new(), Vec::new());
    let (mut next, mut shift) = (0, 0);
    while let Some(first) = edits.get(next) {
        let mut group = Group { delete: first.0, shift, grown: grown(first) };
        next += 1;
        let unit = loop {
            let after = groups.last().map_or(TextSize::new(0), |it: &Group| it.delete.end());
            let before = edits.get(next).map_or(TextSize::of(&**tree.source()), |it| it.0.start());
            let bounds = TextRange::new(after, before);
            let attempt = group.edit(tree, new_source, bounds, edition, outline.is_some());
            let unit = attempt.unit();
            let reach = attempt.reach.get();
            if units.last().is_some_and(|it: &Unit| it.reach.end() >= reach.start()) {
                units.pop();
                group = groups.pop().expect("a group for every unit").joined(group);
                continue;
            }
            if let Some(edit) = edits.get(next).filter(|it| it.0.start() <= reach.end()) {
                group = group.joined(Group { delete: edit.0, shift: 0, grown: grown(edit) });
                next += 1;
                continue;
            }
            break unit?;
        };
        shift = group.shift + group.grown;
        units.push(unit);
        groups.push(group);
    }
    let merged = merge_errors(errors, &mut units)?;
    // alloc: one an edit, as `units`
    let splices: Vec<(u32, Splice<'_>)> = units.iter().map(|it| it.splice(new_source)).collect();
    let tree = tree.splice_all(&splices, new_source.clone());
    Some((tree, merged))
}

/// Edits of [`reparse_all`] parsed again as one: the old text `delete` from the first's start
/// to the last's end, how many bytes longer the text before it is in the new, and how many
/// longer it is itself.
#[derive(Clone, Copy)]
struct Group {
    delete: TextRange,
    shift: i64,
    grown: i64,
}

impl Group {
    /// This group and the one just after it, as one.
    fn joined(self, after: Group) -> Group {
        let delete = self.delete.cover(after.delete);
        Group { delete, shift: self.shift, grown: self.grown + after.grown }
    }

    /// The group's edit, `bounds` the old text around it that holds no other edit.
    fn edit<'a>(
        &self,
        tree: &'a FlatTree,
        new_source: &'a Text,
        bounds: TextRange,
        edition: Edition,
        outline: bool,
    ) -> Edit<'a> {
        let start = u32::from(self.delete.start()) as i64 + self.shift;
        let end = u32::from(self.delete.end()) as i64 + self.shift + self.grown;
        let insert = &new_source[start as usize..end as usize];
        let edit = Edit::new(tree, self.delete, insert, new_source, self.shift, edition, outline);
        Edit { bounds, ..edit }
    }
}

/// What an edit parses again: element `at` of the old tree, over `range` of the old text and
/// `new_range` of the new, with its new tree (`None` for a token, whose new text is all it
/// needs) and the errors in it from its new start; `reach` is the old text the checks read.
struct Unit {
    at: u32,
    with: Option<FlatTree>,
    errors: Vec<SyntaxError>,
    range: TextRange,
    new_range: TextRange,
    reach: TextRange,
}

impl Unit {
    fn splice<'a>(&'a self, new_source: &'a str) -> (u32, Splice<'a>) {
        match &self.with {
            Some(with) => (self.at, Splice::Subtree(with)),
            None => (self.at, Splice::Token(&new_source[self.new_range])),
        }
    }
}

/// The edit that makes `old` into `new`: the range of `old` between their longest common
/// prefix and suffix (at character boundaries), which the bytes of `new` at the same start and
/// `insert` long replace. Compares 64 bytes at a time (`memcmp`) until the texts differ.
pub(crate) fn edit_between(old: &str, new: &str) -> (TextRange, usize) {
    // Each compare is one `memcmp`, whose call costs about what comparing 64 bytes does: the
    // big blocks run at memory bandwidth, the small ones find the difference within one.
    const BIG: usize = 4096;
    const BLOCK: usize = 64;
    let (a, b) = (old.as_bytes(), new.as_bytes());
    let len = a.len().min(b.len());
    let mut prefix = 0;
    while prefix + BIG <= len && a[prefix..prefix + BIG] == b[prefix..prefix + BIG] {
        prefix += BIG;
    }
    while prefix + BLOCK <= len && a[prefix..prefix + BLOCK] == b[prefix..prefix + BLOCK] {
        prefix += BLOCK;
    }
    while prefix < len && a[prefix] == b[prefix] {
        prefix += 1;
    }
    // A char that ends at `prefix` in one text ends there in the other: their bytes to it are
    // the same, and its first byte says how long it is.
    while !old.is_char_boundary(prefix) {
        prefix -= 1;
    }
    let most = len - prefix;
    let (a_end, b_end) = (a.len(), b.len());
    let mut suffix = 0;
    while suffix + BIG <= most
        && a[a_end - suffix - BIG..a_end - suffix] == b[b_end - suffix - BIG..b_end - suffix]
    {
        suffix += BIG;
    }
    while suffix + BLOCK <= most
        && a[a_end - suffix - BLOCK..a_end - suffix] == b[b_end - suffix - BLOCK..b_end - suffix]
    {
        suffix += BLOCK;
    }
    while suffix < most && a[a_end - suffix - 1] == b[b_end - suffix - 1] {
        suffix += 1;
    }
    while !old.is_char_boundary(a_end - suffix) {
        suffix -= 1;
    }
    let to_offset = |at: usize| TextSize::try_from(at).expect("a text under 4 GiB");
    let delete = TextRange::new(to_offset(prefix), to_offset(a_end - suffix));
    (delete, b_end - suffix - prefix)
}

struct Edit<'a> {
    tree: &'a FlatTree,
    delete: TextRange,
    insert: &'a str,
    new_source: &'a Text,
    /// How many bytes longer the text before `delete` is in `new_source`: the other edits'
    /// made with it.
    shift: i64,
    /// The old text holding no other edit made with it: the new text of an element outside
    /// it is not known from this edit alone.
    bounds: TextRange,
    edition: Edition,
    outline: bool,
    /// The old text the checks have read so far.
    reach: Cell<TextRange>,
}

impl<'a> Edit<'a> {
    fn new(
        tree: &'a FlatTree,
        delete: TextRange,
        insert: &'a str,
        new_source: &'a Text,
        shift: i64,
        edition: Edition,
        outline: bool,
    ) -> Edit<'a> {
        let reach = Cell::new(delete);
        let bounds = TextRange::up_to(TextSize::of(&**tree.source()));
        Edit { tree, delete, insert, new_source, shift, bounds, edition, outline, reach }
    }
}

impl Edit<'_> {
    fn kind(&self, at: u32) -> SyntaxKind {
        RustLanguage::kind_from_raw(self.tree.kind(at))
    }

    /// The element parsed again for the edit: the token, else the smallest node.
    fn unit(&self) -> Option<Unit> {
        self.token().or_else(|| self.node())
    }

    /// The checks read `range` of the old text too.
    fn reads(&self, range: TextRange) {
        self.reach.set(self.reach.get().cover(range));
    }

    /// Element `at` parsed again: `with` its new tree, `errors` from its new start.
    fn parsed(&self, at: u32, with: Option<FlatTree>, errors: Vec<SyntaxError>) -> Option<Unit> {
        let range = self.tree.range(at);
        self.reads(range);
        let (new_range, reach) = (self.new_range(range)?, self.reach.get());
        Some(Unit { at, with, errors, range, new_range, reach })
    }

    /// Whether `at` is a leaf: a token or a lazy node.
    fn is_leaf(&self, at: u32) -> bool {
        self.tree.is_token(at) || self.tree.is_lazy(at)
    }

    /// Where the old `range`, which holds the edit, is in the new text: `None` when it is not
    /// in [`Edit::bounds`], and the checks read it, the edit it holds too being one with this.
    fn new_range(&self, range: TextRange) -> Option<TextRange> {
        if !self.bounds.contains_range(range) {
            self.reads(range);
            return None;
        }
        let start = TextSize::new((u32::from(range.start()) as i64 + self.shift) as u32);
        Some(TextRange::at(start, range.len() + TextSize::of(self.insert) - self.delete.len()))
    }

    /// The text of the old `range`, which holds the edit, after it.
    fn new_text(&self, range: TextRange) -> Option<&str> {
        Some(&self.new_source[self.new_range(range)?])
    }

    fn token(&self) -> Option<Unit> {
        let tree = self.tree;
        let at = covering(tree, self.delete);
        if !tree.is_token(at) {
            return None;
        }
        let kind = self.kind(at);
        if !matches!(kind, WHITESPACE | COMMENT | IDENT | STRING | BYTE_STRING | C_STRING) {
            return None;
        }
        let range = tree.range(at);
        let old_text = tree.token_text(at);
        if matches!(kind, WHITESPACE | COMMENT)
            // removing a new line may extend the previous token
            && old_text[self.delete - range.start()].contains('\n')
        {
            return None;
        }
        let new_text = self.new_text(range)?;
        let (new_kind, new_err) = LexedStr::single_token(self.edition, new_text)?;
        let contextual =
            |text: &str| SyntaxKind::from_contextual_keyword(text, self.edition).is_some();
        if new_kind != kind || (kind == IDENT && (contextual(old_text) || contextual(new_text))) {
            return None;
        }
        // Which comments an item takes (`n_attached_trivias`) depends on their style and on the
        // blank lines between them.
        if kind == COMMENT && comment_style(old_text) != comment_style(new_text) {
            return None;
        }
        let prev = (0..at).rev().find(|&it| self.is_leaf(it));
        let next = (at + 1..tree.len()).find(|&it| self.is_leaf(it));
        [prev, next].into_iter().flatten().for_each(|it| self.reads(tree.range(it)));
        if kind == WHITESPACE
            && old_text.contains("\n\n") != new_text.contains("\n\n")
            && [prev, next].into_iter().flatten().any(|it| self.kind(it) == COMMENT)
        {
            return None;
        }

        // The edited token must not take the start of the next one, nor the previous one its
        // start: for source code `bruh"str"`, removing `ruh` makes `b"str"` one byte string.
        if let Some(next_char) = self.tree.source()[usize::from(range.end())..].chars().next() {
            self.reads(TextRange::at(range.end(), TextSize::of(next_char)));
            let takes_next = blaze_mem::scratch::with_string(|with_next| {
                with_next.push_str(new_text);
                with_next.push(next_char);
                LexedStr::single_token(self.edition, with_next).is_some()
            });
            if takes_next {
                return None;
            }
        }
        if let Some(prev) = prev.filter(|&it| tree.is_token(it)) {
            let prev_text = tree.token_text(prev);
            let alone = LexedStr::new(self.edition, prev_text).len();
            let joins = blaze_mem::scratch::with_string(|joint| {
                joint.push_str(prev_text);
                joint.push_str(new_text);
                let joint = LexedStr::new(self.edition, joint);
                joint.len() == alone + 1 && joint.text(joint.len() - 1) == new_text
            });
            if !joins {
                return None;
            }
        }

        let token_range = TextRange::up_to(TextSize::of(new_text));
        let errors = new_err.into_iter().map(|msg| SyntaxError::new(msg, token_range)).collect();
        self.parsed(at, None, errors)
    }

    /// The smallest node holding the edit that parses again alone.
    fn node(&self) -> Option<Unit> {
        let tree = self.tree;
        let covering = covering(tree, self.delete);
        let start = match tree.is_token(covering) {
            true => tree.parent(covering)?,
            false => covering,
        };
        let ancestors = || std::iter::successors(Some(start), |&it| tree.parent(it));
        // In an outline, what is in a `fn` body that is not lazy is parsed in full: the
        // ancestors below the outermost body.
        let outermost = ancestors().enumerate().filter(|&(_, it)| self.is_fn_body(it)).last();
        // An ancestor holding another edit is parsed again with it, as is every one above it.
        let in_bounds = |&(_, at): &(usize, u32)| self.new_range(tree.range(at)).is_some();
        ancestors().enumerate().take_while(in_bounds).find_map(|(i, at)| {
            let in_full = !self.outline || outermost.is_some_and(|(body, _)| i < body);
            let kind = self.kind(at);
            if !in_full && self.is_fn_body(at) {
                return self.body(at);
            }
            let first_child = tree.children_with_tokens(at).next().map(|it| self.kind(it));
            let parent = tree.parent(at).map(|it| self.kind(it));
            if let Some(reparser) = Reparser::for_node(kind, first_child, parent) {
                return self.block(at, reparser, in_full);
            }
            if parent == Some(SOURCE_FILE) && ast::Item::can_cast(kind) {
                return self.item(at, in_full);
            }
            None
        })
    }

    fn is_fn_body(&self, at: u32) -> bool {
        self.kind(at) == BLOCK_EXPR && self.tree.parent(at).is_some_and(|it| self.kind(it) == FN)
    }

    /// A `fn` body of an outline, as a lazy node: one exactly when its tokens let the outline
    /// leave it unparsed (`braced`), which it did or the full parse of the body would have
    /// to be checked too.
    fn body(&self, at: u32) -> Option<Unit> {
        let range = self.new_range(self.tree.range(at))?;
        let text = &self.new_source[range];
        let lexed = LexedStr::new(self.edition, text);
        if !braced(&lexed, self.edition)
            || !self.same_start(at, &lexed)
            || !self.lexes_alone(at, &lexed)
        {
            return None;
        }
        // clone: a count of the new text, which the node's tree shares
        let mut builder = SyntaxTreeBuilder::sourced_range(self.new_source.clone(), range);
        builder.lazy(BLOCK_EXPR, TextSize::of(text));
        let (with, _no_errors) = builder.finish_raw(self.edition);
        self.parsed(at, Some(with), lexer_errors(&lexed, TextSize::new(0)).collect())
    }

    fn block(&self, at: u32, reparser: Reparser, in_full: bool) -> Option<Unit> {
        let kind = self.kind(at);
        let range = self.new_range(self.tree.range(at))?;
        let lexed = LexedStr::new(self.edition, &self.new_source[range]);
        // An expression's parse looks into the block or list after it (a `{` after a path is
        // a struct literal's when an identifier and a `:` follow); a list of items, fields or
        // variants is its parent's last part.
        let looked_into = matches!(kind, BLOCK_EXPR | RECORD_EXPR_FIELD_LIST | MATCH_ARM_LIST);
        if !braced(&lexed, self.edition)
            || (looked_into && !self.same_start(at, &lexed))
            || !self.lexes_alone(at, &lexed)
        {
            return None;
        }
        let input = lexed.to_input(self.edition);
        let output = match in_full {
            true => reparser.parse(&input),
            false => reparser.parse_outline(&input),
        };
        // clone: a count of the new text, which the node's tree shares
        let source = self.new_source.clone();
        let (with, errors) = build_node(source, range, &lexed, &output, self.edition, kind)?;
        self.parsed(at, Some(with), errors)
    }

    /// An item of the source file, parsed as the file's top level parses it: with the tokens
    /// after it, to stop where it stops in the file.
    fn item(&self, at: u32, in_full: bool) -> Option<Unit> {
        let tree = self.tree;
        let kind = self.kind(at);
        let range = tree.range(at);
        let new = self.new_range(range)?;
        let lexed = LexedStr::new(self.edition, &self.new_source[new]);
        // Trivia after an item are its parent's; a parse of the item alone would take them.
        if lexed.is_empty()
            || lexed.kind(lexed.len() - 1).is_trivia()
            || !self.same_leading_trivia(at, &lexed)
            || !self.same_start(at, &lexed)
            || !self.lexes_alone(at, &lexed)
        {
            return None;
        }
        let mut seen = 0;
        let ahead = (tree.end(at)..tree.len())
            .filter(|&it| self.is_leaf(it))
            .find(|&it| {
                let kind = self.kind(it);
                seen += usize::from(!kind.is_trivia());
                seen >= AFTER_ITEM && !kind.is_trivia() && !matches!(kind, INT_NUMBER | T![.])
            })
            .map_or(tree.source().len(), |it| usize::from(tree.range(it).end()));
        // A `{ ... }` list's parse takes the tokens up to the `}` closing it: a `{` left open
        // in the item may be closed past the tokens it is given, and a `}` it does not open
        // may close a list of an item before it.
        let (unclosed, stray) = unpaired_curlies(&lexed);
        if stray || (unclosed && ahead < tree.source().len()) {
            return None;
        }
        self.reads(TextRange::new(range.end(), TextSize::try_from(ahead).unwrap()));
        // `lexes_alone` showed the lexer ends a token where the item ends.
        let ahead = LexedStr::new(self.edition, &tree.source()[usize::from(range.end())..ahead]);
        let input = lexed.to_input_followed_by(&ahead, self.edition);
        let output = match in_full {
            true => Reparser::top_level_item().parse(&input),
            false => Reparser::top_level_item().parse_outline(&input),
        };
        let (output, first, n_tokens) = output.into_first_node()?;
        let tokens = (0..lexed.len()).filter(|&it| !lexed.kind(it).is_trivia()).count();
        if first != kind || n_tokens != tokens {
            return None;
        }
        // A node entered after the item's last token (an empty `ERROR` at the end of the
        // text) is entered past the trivia after the item in the file, and takes them.
        let enters_after_tokens = output.steps().fold(false, |entered, step| match step {
            OutlineStep::Step(Step::Enter { .. }) => true,
            OutlineStep::Step(Step::Token { .. } | Step::FloatSplit { .. })
            | OutlineStep::Lazy { .. } => false,
            OutlineStep::Step(Step::Exit | Step::Error { .. }) => entered,
        });
        if enters_after_tokens {
            return None;
        }
        // clone: a count of the new text, which the node's tree shares
        let source = self.new_source.clone();
        let (with, errors) = build_node(source, new, &lexed, &output, self.edition, kind)?;
        self.parsed(at, Some(with), errors)
    }

    /// Whether item `at`'s new text `new` starts with the trivia its old text does: the same
    /// kinds, and whitespace of the same text. Whitespace before an item is its parent's, and
    /// a comment before it is the item's or its parent's by the new lines after it, so a parse
    /// of the item alone places them as the whole parse does only when these are unchanged; the
    /// text of a comment may change.
    fn same_leading_trivia(&self, at: u32, new: &LexedStr<'_>) -> bool {
        if !new.kind(0).is_trivia() {
            return true;
        }
        let old = LexedStr::new(self.edition, &self.tree.source()[self.tree.range(at)]);
        leading_trivia(&old).eq(leading_trivia(new))
    }

    /// Whether node `at`'s first [`LOOKAHEAD`] tokens are those of `new`, its new text. When the
    /// edit starts past them and the two tokens after them, the old text is the same up to there
    /// and lexes to the same tokens (the lexer reads at most a character past a token to end
    /// it), so it is not lexed.
    fn same_start(&self, at: u32, new: &LexedStr<'_>) -> bool {
        let range = self.tree.range(at);
        let edit = usize::from(self.delete.start() - range.start());
        let last = (0..new.len()).filter(|&it| !new.kind(it).is_trivia()).nth(LOOKAHEAD - 1);
        if let Some(last) = last
            && last + 2 < new.len()
            && new.text_range(last + 2).end <= edit
        {
            return true;
        }
        let old = LexedStr::new(self.edition, &self.tree.source()[range]);
        first_tokens(&old).eq(first_tokens(new))
    }

    /// Whether `new`, the new text of node `at`, lexes alone as it does between the tokens
    /// around it: the lexing of the text from a token before it to a token after it is that of
    /// the three parts alone. The tokens bounding it are where the lexer's are: not pieces of
    /// a float literal the parser split (`x.0.1`), nor where a lexing starting there reads a
    /// shebang or front matter.
    fn lexes_alone(&self, at: u32, new: &LexedStr<'_>) -> bool {
        let (tree, source) = (self.tree, self.tree.source());
        let range = tree.range(at);
        let bound =
            |it: &u32| !matches!(self.kind(*it), INT_NUMBER | T![.] | T![#] | T![-] | WHITESPACE);
        // A lazy node bounds at its last token, a `}`, or its first, a `{`: its body is not
        // lexed again.
        let before = (0..at).rev().filter(|&it| self.is_leaf(it)).find(bound).map_or(
            TextSize::new(0),
            |it| match tree.is_lazy(it) {
                true => tree.range(it).end() - TextSize::of('}'),
                false => tree.range(it).start(),
            },
        );
        let after = (tree.end(at)..tree.len()).filter(|&it| self.is_leaf(it)).find(bound).map_or(
            TextSize::of(&**source),
            |it| match tree.is_lazy(it) {
                true => tree.range(it).start() + TextSize::of('{'),
                false => tree.range(it).end(),
            },
        );
        self.reads(TextRange::new(before, after));
        let before = &source[TextRange::new(before, range.start())];
        let after = &source[TextRange::new(range.end(), after)];
        blaze_mem::scratch::with_string(|joint| {
            joint.push_str(before);
            joint.push_str(new.as_str());
            joint.push_str(after);
            // Locals drop in reverse, so each lexing's columns go back to the shelf in the
            // order the next reparse takes them.
            let joint = LexedStr::new(self.edition, joint);
            let before = LexedStr::new(self.edition, before);
            let after = LexedStr::new(self.edition, after);
            let alone = [&before, new, &after].into_iter().flat_map(token_lengths);
            alone.eq(token_lengths(&joint))
        })
    }
}

/// Whether a comment reads as outer and as inner to `parser`'s attaching of comments to items.
fn comment_style(text: &str) -> (bool, bool) {
    let outer = !(text.starts_with("////") || text.starts_with("/***"))
        && (text.starts_with("///") || text.starts_with("/**"));
    (outer, text.starts_with("//!") || text.starts_with("/*!"))
}

fn token_lengths<'a>(lexed: &'a LexedStr<'_>) -> impl Iterator<Item = (SyntaxKind, usize)> + 'a {
    (0..lexed.len()).map(|it| (lexed.kind(it), lexed.text_len(it)))
}

/// The first [`LOOKAHEAD`] tokens of `lexed` that are not trivia.
fn first_tokens<'a>(lexed: &'a LexedStr<'_>) -> impl Iterator<Item = (SyntaxKind, &'a str)> + 'a {
    (0..lexed.len())
        .filter(|&it| !lexed.kind(it).is_trivia())
        .take(LOOKAHEAD)
        .map(|it| (lexed.kind(it), lexed.text(it)))
}

/// The trivia `lexed` starts with: each one's kind, and a whitespace's text.
fn leading_trivia<'a>(
    lexed: &'a LexedStr<'_>,
) -> impl Iterator<Item = (SyntaxKind, Option<&'a str>)> + 'a {
    (0..lexed.len())
        .take_while(|&it| lexed.kind(it).is_trivia())
        .map(|it| (lexed.kind(it), (lexed.kind(it) == WHITESPACE).then(|| lexed.text(it))))
}

/// Whether `lexed` is a `{ ... }` that parses to its last `}`: its braces balance and it holds
/// none of the tokens that make a block's parse take a brace as a plain token
/// ([`parser::lazy_block_len`]).
fn braced(lexed: &LexedStr<'_>, edition: Edition) -> bool {
    if lexed.is_empty() || lexed.kind(0) != T!['{'] || lexed.kind(lexed.len() - 1) != T!['}'] {
        return false;
    }
    let input = lexed.to_input(edition);
    parser::lazy_block_len(&input, 0) == u32::try_from(input.len()).ok()
}

/// Whether a `{` of `lexed` has no `}` of `lexed` closing it, and whether a `}` of it closes
/// no `{` of it, each `}` closing the nearest `{` before it not yet closed.
fn unpaired_curlies(lexed: &LexedStr<'_>) -> (bool, bool) {
    let (mut open, mut stray) = (0usize, false);
    for at in 0..lexed.len() {
        match lexed.kind(at) {
            T!['{'] => open += 1,
            T!['}'] if open == 0 => stray = true,
            T!['}'] => open -= 1,
            _ => (),
        }
    }
    (open > 0, stray)
}

/// The smallest element of `tree` whose range holds `range`, never in a lazy node's part:
/// for an empty range between two elements, the one before it.
fn covering(tree: &FlatTree, range: TextRange) -> u32 {
    let mut at = 0;
    loop {
        if tree.is_token(at) || tree.is_lazy(at) {
            return at;
        }
        let (mut overlapping, mut before, mut first) = (None, None, None);
        for child in tree.children_with_tokens(at) {
            first = first.or(Some(child));
            let child_range = tree.range(child);
            if child_range.end() <= range.start() {
                before = Some(child);
            } else if range.end() <= child_range.start() {
                break;
            } else {
                overlapping = Some(child);
                break;
            }
        }
        match overlapping.or(before).or(first) {
            Some(child) if tree.range(child).contains_range(range) => at = child,
            _ => return at,
        }
    }
}

/// The errors of the tree `units` make: `old_errors`, those of the old tree, that are outside
/// every unit, moved by what the units before them grew, then each unit's own, from its new
/// start. `None` when an old error is empty at the start or end of a unit parsed again as a
/// node: it may be the unit's own (an unclosed block's "expected `}`" at the end of the text a
/// unit runs to), which its new errors have again, or its neighbour's, and its place can't tell
/// which. A token's unit makes no empty error (a lexer error spans its token), so one at its
/// edge is the parse around it (`let x = foo + bar` missing its `;` after `bar`), and kept.
fn merge_errors(
    old_errors: impl IntoIterator<Item = SyntaxError>,
    units: &mut [Unit],
) -> Option<Vec<SyntaxError>> {
    let mut res = Vec::new();
    for old_err in old_errors {
        let range = old_err.range();
        let before = units.partition_point(|it| it.range.end() <= range.start());
        let at_edge = |unit: Option<&Unit>| {
            unit.is_some_and(|it| {
                it.with.is_some()
                    && (it.range.start() == range.start() || it.range.end() == range.start())
            })
        };
        if range.is_empty()
            && (at_edge(units.get(before)) || at_edge(before.checked_sub(1).map(|it| &units[it])))
        {
            return None;
        }
        if units.get(before).is_some_and(|it| it.range.start() < range.end()) {
            continue;
        }
        match before.checked_sub(1).map(|it| &units[it]) {
            None => res.push(old_err),
            // The old end is subtracted first: the error starts past it.
            Some(it) => res.push(old_err.with_range(range - it.range.end() + it.new_range.end())),
        }
    }
    for unit in units.iter_mut() {
        let start = unit.new_range.start();
        res.extend(unit.errors.drain(..).map(|it| {
            let range = it.range() + start;
            it.with_range(range)
        }));
    }
    Some(res)
}

#[cfg(test)]
mod tests {
    use std::ops::Range;

    use blaze_ids::rng::SplitMix64;
    use parser::Edition;
    use test_utils::{assert_eq_text, extract_range};

    use super::*;
    use crate::{Parse, SourceFile, Tree};

    fn do_check(before: &str, replace_with: &str, reparsed_len: u32) {
        let (range, before) = extract_range(before);
        let after = {
            let mut after = before.clone();
            after.replace_range(Range::<usize>::from(range), replace_with);
            after
        };

        let fully_reparsed = SourceFile::parse(&after, Edition::CURRENT);
        let incrementally_reparsed: Parse<SourceFile> = {
            let before = SourceFile::parse(&before, Edition::CURRENT);
            let (tree, new_errors, range) = incremental_reparse(
                before.tree.as_ref().unwrap().flat(),
                range,
                replace_with,
                before.errors.as_deref().unwrap_or_default().iter().cloned(),
                Edition::CURRENT,
                None,
            )
            .unwrap();
            assert_eq!(range.len(), reparsed_len.into(), "reparsed fragment has wrong length");
            Parse::new(Tree::new(std::sync::Arc::new(tree)), new_errors)
        };

        assert_eq_text!(
            &format!("{:#?}", fully_reparsed.tree().syntax()),
            &format!("{:#?}", incrementally_reparsed.tree().syntax()),
        );
        assert_eq!(fully_reparsed.errors(), incrementally_reparsed.errors());

        let outline = SourceFile::parse_outline(Text::from(before), Edition::CURRENT);
        check_same(
            &outline.reparse(range, replace_with, Edition::CURRENT),
            &SourceFile::parse_outline(Text::from(after), Edition::CURRENT),
        );
    }

    /// `reparsed` and `parsed` are the same parse: their trees, which of their nodes are lazy,
    /// and their errors (in another order, see `merge_errors`).
    fn check_same(reparsed: &Parse<SourceFile>, parsed: &Parse<SourceFile>) {
        let lazies = |parse: &Parse<SourceFile>| {
            let tree = parse.tree.as_ref().unwrap().flat();
            tree.lazies().map(|it| tree.range(it)).collect::<Vec<_>>()
        };
        assert_eq!(lazies(reparsed), lazies(parsed), "different lazy nodes");
        assert_eq_text!(
            &format!("{:#?}", parsed.tree().syntax()),
            &format!("{:#?}", reparsed.tree().syntax()),
        );
        let errors = |parse: &Parse<SourceFile>| {
            let mut errors: Vec<_> = parse
                .errors_with_bodies()
                .into_iter()
                .map(|it| (it.range().start(), it.range().end(), it.to_string()))
                .collect();
            errors.sort();
            errors
        };
        assert_eq!(errors(reparsed), errors(parsed));
        same_hashes(reparsed, parsed);
    }

    /// The nodes of `parse` large enough that they may keep their subtree hashes: all those
    /// that do, and more.
    fn large(parse: &Parse<SourceFile>) -> impl Iterator<Item = u32> + '_ {
        let tree = parse.tree.as_ref().unwrap().flat();
        (0..tree.len()).filter(move |&it| {
            !tree.is_token(it)
                && (tree.end(it) - it >= 64 || tree.range(it).len() >= TextSize::new(1024))
        })
    }

    /// Asks the large nodes of `parse` for their hashes, so the edits of it carry them.
    fn hash_large(parse: &Parse<SourceFile>) {
        let tree = parse.tree.as_ref().unwrap().flat();
        for at in large(parse) {
            let _ = tree.subtree_hash(at);
        }
    }

    /// The large nodes of `reparsed`, the same tree as `parsed`, have the hashes they have in
    /// `parsed`: those an edit carried, those it made again, and those hashed when asked.
    fn same_hashes(reparsed: &Parse<SourceFile>, parsed: &Parse<SourceFile>) {
        let (of, again) =
            (reparsed.tree.as_ref().unwrap().flat(), parsed.tree.as_ref().unwrap().flat());
        assert_eq!(of.len(), again.len(), "different trees");
        for at in large(parsed) {
            assert_eq!(of.subtree_hash(at), again.subtree_hash(at), "the hash of element {at}");
        }
    }

    #[test] // FIXME: some test here actually test token reparsing
    fn reparse_block_tests() {
        do_check(
            r"
fn foo() {
    let x = foo + $0bar$0
}
",
            "baz",
            3,
        );
        do_check(
            r"
fn foo() {
    let x = foo$0 + bar$0
}
",
            "baz",
            25,
        );
        do_check(
            r"
struct Foo {
    f: foo$0$0
}
",
            ",\n    g: (),",
            14,
        );
        do_check(
            r"
fn foo {
    let;
    1 + 1;
    $092$0;
}
",
            "62",
            31, // FIXME: reparse only int literal here
        );
        do_check(
            r"
mod foo {
    fn $0$0
}
",
            "bar",
            11,
        );

        do_check(
            r"
trait Foo {
    type $0Foo$0;
}
",
            "Output",
            3,
        );
        do_check(
            r"
impl IntoIterator<Item=i32> for Foo {
    f$0$0
}
",
            "n next(",
            9,
        );
        do_check(r"use a::b::{foo,$0,bar$0};", "baz", 10);
        do_check(
            r"
pub enum A {
    Foo$0$0
}
",
            "\nBar;\n",
            11,
        );
        do_check(
            r"
foo!{a, b$0$0 d}
",
            ", c[3]",
            8,
        );
        do_check(
            r"
fn foo() {
    vec![$0$0]
}
",
            "123",
            14,
        );
        do_check(
            r"
extern {
    fn$0;$0
}
",
            " exit(code: c_int)",
            11,
        );
    }

    #[test]
    fn reparse_token_tests() {
        do_check(
            r"$0$0
fn foo() -> i32 { 1 }
",
            "\n\n\n   \n",
            1,
        );
        do_check(
            r"
fn foo() -> $0$0 {}
",
            "  \n",
            2,
        );
        do_check(
            r"
fn $0foo$0() -> i32 { 1 }
",
            "bar",
            3,
        );
        do_check(
            r"
fn foo$0$0foo() {  }
",
            "bar",
            6,
        );
        do_check(
            r"
fn foo /* $0$0 */ () {}
",
            "some comment",
            6,
        );
        do_check(
            r"
fn baz $0$0 () {}
",
            "    \t\t\n\n",
            2,
        );
        do_check(
            r"
fn baz $0$0 () {}
",
            "    \t\t\n\n",
            2,
        );
        do_check(
            r#"
fn -> &str { "Hello$0$0" }
"#,
            ", world",
            7,
        );
        do_check(
            r#"
fn -> &str { // "Hello$0$0"
"#,
            ", world",
            10,
        );
        do_check(
            r##"
fn -> &str { r#"Hello$0$0"#
"##,
            ", world",
            10,
        );
        do_check(
            r"
#[derive($0Copy$0)]
enum Foo {

}
",
            "Clone",
            4,
        );
    }

    #[test]
    fn reparse_str_token_with_error_unchanged() {
        do_check(r#""$0Unclosed$0 string literal"#, "Still unclosed", 24);
    }

    #[test]
    fn reparse_str_token_with_error_fixed() {
        do_check(r#""unterminated$0$0"#, "\"", 13);
    }

    #[test]
    fn reparse_block_with_error_in_middle_unchanged() {
        do_check(
            r#"fn main() {
                if {}
                32 + 4$0$0
                return
                if {}
            }"#,
            "23",
            105,
        )
    }

    #[test]
    fn reparse_block_with_error_in_middle_fixed() {
        do_check(
            r#"fn main() {
                if {}
                32 + 4$0$0
                return
                if {}
            }"#,
            ";",
            105,
        )
    }

    #[test]
    fn reparse_lazy_body() {
        let before = "fn f() {\n    let x = 1;\n}\nfn g() {}\n";
        let after = "fn f() {\n    let x = 2 + 3;\n}\nfn g() {}\n";
        let outline = SourceFile::parse_outline(Text::from(before), Edition::CURRENT);
        let at = TextRange::at(TextSize::new(21), TextSize::new(1));
        assert_eq!(&before[at], "1");
        let (tree, errors, range) = incremental_reparse(
            outline.tree.as_ref().unwrap().flat(),
            at,
            "2 + 3",
            [],
            Edition::CURRENT,
            Some(Edition::CURRENT),
        )
        .unwrap();
        assert_eq!(range, TextRange::new(TextSize::new(7), TextSize::new(25)), "f's body");
        let reparsed = Parse::with_outline(
            Tree::new(std::sync::Arc::new(tree)),
            errors,
            Some(Edition::CURRENT),
        );
        let parsed = SourceFile::parse_outline(Text::from(after), Edition::CURRENT);
        assert_eq!(parsed.tree.as_ref().unwrap().flat().lazies().count(), 2);
        check_same(&reparsed, &parsed);
    }

    const INSERTS: &[&str] = &[
        "",
        " ",
        "\n",
        "\n\n",
        "{",
        "}",
        "(",
        ")",
        "[",
        "]",
        ";",
        ",",
        "x",
        "_",
        "#",
        "#[a]",
        "<",
        ">",
        "1.0",
        ".0",
        "0",
        "\"",
        "'",
        "r#",
        "//",
        "/// d\n",
        "/*",
        "*/",
        "let x = 1;",
        "fn f() {}",
        "fn g() { 1 }",
        "impl S {}",
        "struct S { a: u8 }",
        "mod m {}",
        "=>",
        "::",
        "unsafe",
        "async",
        "union",
        "macro_rules! m {}",
    ];

    /// Random edits of real files, each reparse the parse of the edited text: in full and as an
    /// outline.
    #[test]
    fn reparse_random_edits() {
        let root = test_utils::project_root().into_std_path_buf();
        let mut files = Vec::new();
        for dir in ["crates/syntax/src", "crates/parser/src"] {
            collect_rs(&root.join(dir), &mut files);
        }
        for dir in ["ok", "inline/ok"] {
            collect_rs(&root.join("crates/parser/test_data/parser").join(dir), &mut files);
        }
        files.sort();
        assert!(files.len() > 100, "the sources and test data of syntax and parser");
        let mut rng = SplitMix64::new(0x2545_f491_4f6c_dd1d);
        for file in &files {
            let text = std::fs::read_to_string(file).unwrap();
            let edits = if text.len() > 20_000 { 2 } else { 6 };
            let full = SourceFile::parse(&text, Edition::CURRENT);
            let outline = SourceFile::parse_outline(Text::from(text.as_str()), Edition::CURRENT);
            hash_large(&full);
            hash_large(&outline);
            for _ in 0..edits {
                let (delete, insert) = random_edit(&mut rng, &text);
                let mut after = text.clone();
                after.replace_range(Range::<usize>::from(delete), &insert);
                let check = || {
                    let reparsed = full.reparse(delete, &insert, Edition::CURRENT);
                    let parsed = SourceFile::parse(&after, Edition::CURRENT);
                    assert_eq_text!(
                        &format!("{:#?}", parsed.tree().syntax()),
                        &format!("{:#?}", reparsed.tree().syntax()),
                    );
                    same_hashes(&reparsed, &parsed);
                    let reparsed = outline.reparse(delete, &insert, Edition::CURRENT);
                    let parsed =
                        SourceFile::parse_outline(Text::from(after.as_str()), Edition::CURRENT);
                    check_same(&reparsed, &parsed);
                };
                if std::panic::catch_unwind(std::panic::AssertUnwindSafe(check)).is_err() {
                    panic!("{}: {delete:?} replaced by {insert:?}", file.display());
                }
            }
        }
    }

    /// Several random edits of real files at once, made into one parse by `reparse_to` from the
    /// parse before them: the parse of the text after them, in full and as an outline, sharing
    /// that text.
    #[test]
    fn reparse_to_random_edits() {
        let root = test_utils::project_root().into_std_path_buf();
        let mut files = Vec::new();
        collect_rs(&root.join("crates/syntax/src"), &mut files);
        files.sort();
        let mut rng = SplitMix64::new(0x9e37_79b9_7f4a_7c15);
        for file in &files {
            let text = std::fs::read_to_string(file).unwrap();
            let full = SourceFile::parse(&text, Edition::CURRENT);
            let outline = SourceFile::parse_outline(Text::from(text.as_str()), Edition::CURRENT);
            hash_large(&full);
            hash_large(&outline);
            for edits in 1..4 {
                let mut after = text.clone();
                for _ in 0..edits {
                    let (delete, insert) = random_edit(&mut rng, &after);
                    after.replace_range(Range::<usize>::from(delete), &insert);
                }
                let shared = Text::from(after.as_str());
                let check = || {
                    let reparsed = full.reparse_to(shared.clone(), Edition::CURRENT);
                    let parsed = SourceFile::parse(&after, Edition::CURRENT);
                    assert_eq_text!(
                        &format!("{:#?}", parsed.tree().syntax()),
                        &format!("{:#?}", reparsed.tree().syntax()),
                    );
                    assert_eq!(reparsed.errors(), parsed.errors());
                    same_hashes(&reparsed, &parsed);
                    assert!(reparsed.tree.as_ref().unwrap().flat().source().ptr_eq(&shared));
                    let reparsed = outline.reparse_to(shared.clone(), Edition::CURRENT);
                    let parsed = SourceFile::parse_outline(shared.clone(), Edition::CURRENT);
                    check_same(&reparsed, &parsed);
                };
                if std::panic::catch_unwind(std::panic::AssertUnwindSafe(check)).is_err() {
                    panic!("{}: {edits} edits", file.display());
                }
            }
        }
    }

    /// Random disjoint edits of real files made at once by `reparse_all`: the parse of the text
    /// after them, and the one `reparse` makes of them edit by edit, in full and as an outline.
    #[test]
    fn reparse_all_random_edits() {
        let root = test_utils::project_root().into_std_path_buf();
        let mut files = Vec::new();
        collect_rs(&root.join("crates/syntax/src"), &mut files);
        files.sort();
        let mut rng = SplitMix64::new(0x6a09_e667_f3bc_c908);
        for file in &files {
            let text = std::fs::read_to_string(file).unwrap();
            let full = SourceFile::parse(&text, Edition::CURRENT);
            let outline = SourceFile::parse_outline(Text::from(text.as_str()), Edition::CURRENT);
            hash_large(&full);
            hash_large(&outline);
            for count in 1..8 {
                let mut owned: Vec<(TextRange, String)> =
                    (0..count).map(|_| random_edit(&mut rng, &text)).collect();
                owned.sort_by_key(|it| (it.0.start(), it.0.end()));
                let mut end = TextSize::new(0);
                owned.retain(|it| {
                    let disjoint = it.0.start() >= end;
                    if disjoint {
                        end = it.0.end();
                    }
                    disjoint
                });
                let edits: Vec<(TextRange, &str)> =
                    owned.iter().map(|(delete, insert)| (*delete, insert.as_str())).collect();
                let mut after = text.clone();
                for &(delete, insert) in edits.iter().rev() {
                    after.replace_range(Range::<usize>::from(delete), insert);
                }
                let check = || {
                    for parse in [&full, &outline] {
                        let at_once = parse.reparse_all(&edits, Edition::CURRENT);
                        let mut one_by_one = parse.clone();
                        for &(delete, insert) in edits.iter().rev() {
                            one_by_one = one_by_one.reparse(delete, insert, Edition::CURRENT);
                        }
                        let after = Text::from(after.as_str());
                        let parsed = match parse.is_outline() {
                            true => SourceFile::parse_outline(after, Edition::CURRENT),
                            false => SourceFile::parse_shared(after, Edition::CURRENT),
                        };
                        check_same(&at_once, &parsed);
                        check_same(&at_once, &one_by_one);
                        let flat = |it: &Parse<SourceFile>| it.tree.as_ref().unwrap().flat().clone();
                        assert!(*flat(&at_once) == *flat(&one_by_one), "the same tree");
                    }
                };
                if std::panic::catch_unwind(std::panic::AssertUnwindSafe(check)).is_err() {
                    panic!("{}: {edits:?}", file.display());
                }
            }
        }
    }

    /// The edit between two texts is the one that makes the first the second, at character
    /// boundaries, and covers no byte the two share at either end.
    #[test]
    fn edit_between_cuts_the_common_ends() {
        let long = "x".repeat(200);
        let cases = [
            ("abc", "abc"),
            ("abc", "abXc"),
            ("abc", "c"),
            ("", "fn f() {}"),
            ("fn f() {}", ""),
            ("aaaa", "aaaaaa"),
            ("let s = \"é\";", "let s = \"è\";"),
            ("é", "è"),
            ("ü", "u"),
        ];
        let mut all: Vec<(String, String)> =
            cases.iter().map(|&(a, b)| (a.to_owned(), b.to_owned())).collect();
        all.push((format!("{long}a{long}"), format!("{long}bb{long}")));
        let huge = "y".repeat(9000);
        all.push((format!("{huge}a{huge}"), format!("{huge}bb{huge}")));
        all.push((format!("{huge}é{long}"), format!("{huge}è{long}")));
        for (old, new) in &all {
            let (old, new) = (old.as_str(), new.as_str());
            let (delete, insert) =
                blaze_alloc_count::assert_at_most(0, "finding an edit", || edit_between(old, new));
            let start = usize::from(delete.start());
            let made = format!(
                "{}{}{}",
                &old[..start],
                &new[start..start + insert],
                &old[usize::from(delete.end())..]
            );
            assert_eq!(made, new, "{old:?} to {new:?}");
            let suffix = old.len() - usize::from(delete.end());
            assert_eq!(new.len() - suffix - start, insert);
            // No character both the deleted and the inserted text start (or end) with.
            let (deleted, inserted) = (&old[delete], &new[start..start + insert]);
            if let (Some(a), Some(b)) = (deleted.chars().next(), inserted.chars().next()) {
                assert_ne!(a, b, "{old:?} to {new:?}: a shared start");
            }
            if let (Some(a), Some(b)) = (deleted.chars().next_back(), inserted.chars().next_back())
            {
                assert_ne!(a, b, "{old:?} to {new:?}: a shared end");
            }
        }
        assert_eq!(edit_between(&format!("{long}a{long}"), &format!("{long}bb{long}")).0, {
            TextRange::at(TextSize::new(200), TextSize::new(1))
        });
        assert_eq!(edit_between(&format!("{huge}a{huge}"), &format!("{huge}bb{huge}")).0, {
            TextRange::at(TextSize::new(9000), TextSize::new(1))
        });
    }

    fn random_edit(rng: &mut SplitMix64, text: &str) -> (TextRange, String) {
        let boundary = |mut at: usize| {
            while !text.is_char_boundary(at) {
                at -= 1;
            }
            at
        };
        let start = boundary(rng.index(text.len() + 1));
        let end = boundary((start + rng.index(12)).min(text.len())).max(start);
        let insert = match rng.index(4) {
            0 => {
                let from = boundary(rng.index(text.len() + 1));
                let to = boundary((from + rng.index(24)).min(text.len())).max(from);
                text[from..to].to_owned()
            }
            _ => INSERTS[rng.index(INSERTS.len())].to_owned(),
        };
        let delete = TextRange::new(start.try_into().unwrap(), end.try_into().unwrap());
        (delete, insert)
    }

    fn collect_rs(dir: &std::path::Path, files: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                collect_rs(&path, files);
            } else if path.extension().is_some_and(|it| it == "rs") {
                files.push(path);
            }
        }
    }

    /// A warm edit allocates the trees it makes alone: for a block parsed again, the block's
    /// tree, the spliced tree and the parse's `Arc`; for a token, the last two. No buffer of
    /// the reparse itself (the lexer's and parser input's columns are their shelves'), and the
    /// same parse as the whole text's.
    #[test]
    fn a_warm_reparse_allocates_its_trees_alone() {
        let before = "fn f() {\n    let x = 1 + 2;\n}\nfn foo() {}\n";
        let edits = [
            ("fn f() {\n    let x = 1 + 3;\n}\nfn foo() {}\n", 3, "a block parsed again"),
            ("fn f() {\n    let x = 1 + 2;\n}\nfn fob() {}\n", 2, "a token replaced"),
        ];
        let parse = SourceFile::parse(before, Edition::CURRENT);
        for (after, budget, what) in edits {
            let after = Text::from(after);
            let cold = parse.reparse_to(after.clone(), Edition::CURRENT);
            let warm = blaze_alloc_count::assert_at_most(budget, what, || {
                parse.reparse_to(after.clone(), Edition::CURRENT)
            });
            let whole = SourceFile::parse(&after, Edition::CURRENT);
            let shown = format!("{:#?}", warm.tree().syntax());
            assert_eq!(shown, format!("{:#?}", cold.tree().syntax()));
            assert_eq!(shown, format!("{:#?}", whole.tree().syntax()));
        }
    }
}
