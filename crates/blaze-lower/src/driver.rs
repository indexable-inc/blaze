//! A file to its records: the outline (blaze-file-items), then one lowering per item, each
//! with its key computed from text before the work (rule 65) and handed to a [`Sink`] with
//! its record. Item macros are expanded by the caller's [`Expand`] (blaze-expand's, plugged
//! in by the caller so this stays sans-IO) and their output lowered as a text of its own;
//! the items a body declares are lowered the same way, from the body record's rows.
//!
//! Nothing recurses: the texts to lower (the file, each expansion, each body's items) wait on
//! one work list, and each one's anonymous constants on another.

use blaze_expand::TokenStream;
use blaze_ids::ContentId;
use blaze_own::{AString, AVec, Alloc, String, Vec};
use blaze_surface::{Attr, AttrKind, Edition, Item, ItemDetail, ItemIndex, ItemKind, Range};

use crate::columns::View;
use crate::form::ROW_LOCAL_ITEM;
use crate::{
    Cfg, CfgAnswer, ItemWords, Lowered, Need, anon_const_key, body_key, lower_anon_const_expanding, lower_body_expanding,
    lower_body_words_expanding, lower_signature, lower_signature_words, signature_key, ValueNames,
};

/// How deep item macros may expand into item macros (rustc's `recursion_limit` default).
pub const EXPANSION_DEPTH: u32 = 128;

/// How many macro expansions one item of the file may make, every descendant counted, and
/// how many one body's lowering may make: the depth bound alone lets a macro whose output
/// holds two calls of itself make 2^128. Tempo's most in one file is 1,569 (one `sol!`); its
/// deepest item chain 3.
pub const EXPANSION_BUDGET: u32 = 1 << 16;

/// The bound a macro call was refused by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bound {
    /// It would be past [`EXPANSION_DEPTH`] expansions deep.
    Depth,
    /// Its item has made [`EXPANSION_BUDGET`] expansions already.
    Budget,
}

impl core::fmt::Display for Bound {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Bound::Depth => write!(f, "past {EXPANSION_DEPTH} nested expansions (rule 18)"),
            Bound::Budget => write!(f, "past {EXPANSION_BUDGET} expansions from one item (rule 18)"),
        }
    }
}

/// An item macro call a bound refused: the macro's path, written on the item the sink is
/// given, and the bound it passed. Nothing of the call is lowered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Refusal<'r> {
    pub path: &'r str,
    pub bound: Bound,
}

/// What a macro call became.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Expansion {
    /// It expanded: an item call's items' text is in the output string, a body call's words
    /// in its [`BodyOut`].
    Expanded,
    /// The expander has no way to expand it (no definition found, a proc macro not built, a
    /// builtin not taken).
    Unsupported,
    /// It expanded to an error, which the expander reports.
    Failed,
}

/// Where a body macro call's input is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallInput<'s> {
    /// In the item's own text: the token tree's text, delimiters included.
    Text(&'s str),
    /// In an expansion this body's lowering asked for earlier: its tokens `start..end`
    /// (delimiters included), `expansion` counting the [`Expand::expand_body`] calls that
    /// expanded, from 0. The expander keeps those expansions while the body is lowered.
    Expanded { expansion: u32, start: u32, end: u32 },
}

/// A macro call in a body.
#[derive(Clone, Copy, Debug)]
pub struct BodyCall<'s> {
    /// The path as written (in an expansion, its words with a space between each two).
    pub path: &'s str,
    /// The mark of the path's first word: of an expansion lowered here, or of the item's own
    /// text when a macro wrote the item ([`crate::lower_body_expanding_marked`]); `None` for a
    /// word as written in a file. Where the name is looked up is `mark_site::site_of` of it.
    pub path_mark: Option<crate::mark_site::TextMark>,
    pub input: CallInput<'s>,
    /// The marks of the input's words when the input is text ([`CallInput::Text`]): the words
    /// of an item a macro wrote keep theirs ([`crate::lower_body_expanding_marked`]), and the
    /// call's input is lexed with them (blaze-expand `lex_marked`), so a `$crate` the expansion
    /// wrote is one identifier of the defining crate (tracing's `event!` writes `static
    /// __CALLSITE: .. = $crate::callsite2! { .., level: $crate::Level::DEBUG, .. }` in a body).
    /// Empty for a text as written in a file. Not read for [`CallInput::Expanded`], whose
    /// tokens carry their contexts.
    pub input_marks: InputMarks<'s>,
    /// The whole call's range in the body's text (past the item's length: in an expansion
    /// appended to it), for a finding about it.
    pub range: Range,
    /// The call written in the item's own text that this call is in (item-relative, rule 71):
    /// `range` for a call written there; for a call an expansion wrote, the written call it
    /// came from, followed out through every expansion. A path read at the call site is read
    /// in the innermost block of the text around it (tempo's `use serde_json::json;` in a test
    /// fn, then `assert_eq!(.., json!(..))`: `json!` is a call in `assert_eq!`'s expansion,
    /// and `json` is bound in the fn's block, not its module).
    pub written: Range,
    /// What its position takes; the expansion is parsed as this.
    pub fragment: blaze_expand::Fragment,
    pub edition: Edition,
    /// The `macro_rules!` and `macro` the body defines that are in scope at the call, in the
    /// order written (the last of a name wins): tried before any path-based resolution.
    pub locals: &'s [LocalMacro<'s>],
    /// The expansions appended to the body's text before this call, which hold the call when
    /// an expansion wrote it. A path read at the call site is read in the innermost block
    /// around it, and that block may be one an expansion wrote (the Reference, "Statements":
    /// "Declaring an item within a statement block restricts its scope to the block containing
    /// the statement"; proptest 1.11.0's `prop_assert!` writes `if !$cond { extern crate alloc;
    /// let message = alloc::format!(..); .. }`).
    pub expansions: Expansions<'s>,
}

/// The marks of a call's input text ([`BodyCall::input_marks`]): the body's mark rows (each
/// marked word's start in the body's text, ascending, and its [`crate::mark_site::TextMark`]
/// bytes, one [`blaze_expand::origin_column::MARK`] row per start) that fall in the input, and
/// where the input starts, so a mark is asked by its byte offset in the input's text.
#[derive(Clone, Copy, Debug)]
pub struct InputMarks<'s> {
    at: u32,
    starts: &'s [u32],
    bytes: &'s [u8],
}

impl<'s> InputMarks<'s> {
    /// No marks: an input as written in a file.
    pub const NONE: InputMarks<'static> = InputMarks { at: 0, starts: &[], bytes: &[] };

    /// The rows `starts` (with their `bytes`) of an input starting at byte `at` of the text
    /// the rows are in (a body's grown text, or an item's).
    pub(crate) fn new(at: u32, starts: &'s [u32], bytes: &'s [u8]) -> InputMarks<'s> {
        debug_assert_eq!(starts.len() * blaze_expand::origin_column::MARK, bytes.len(), "one mark's bytes per start");
        InputMarks { at, starts, bytes }
    }

    /// Whether no word of the input has a mark.
    pub fn is_empty(&self) -> bool {
        self.starts.is_empty()
    }

    /// The mark of the word starting at byte `offset` of the input's text.
    pub fn mark_at(&self, offset: u32) -> Option<crate::mark_site::TextMark> {
        use blaze_expand::origin_column::MARK;
        let i = self.starts.binary_search(&self.at.checked_add(offset)?).ok()?;
        // Written with `TextMark::to_bytes`, by the machine (machine.rs `mark_item`,
        // `append_expansion`) or by `ExpansionWords::mark_rows`, and by nothing else.
        Some(crate::mark_site::TextMark::from_bytes(&self.bytes[i * MARK..(i + 1) * MARK]).expect("a mark the machine wrote reads back"))
    }
}

/// The expansions a body's lowering has appended to the item's text so far, as a reader of
/// their blocks needs them: each one's text and where its call stands. These are the same
/// columns, read the same way, as the finished record's [`View::expansion_text`] and
/// [`View::expansion_call_at`]. An expansion is appended whole, after every earlier one, so
/// the text holding an offset past the item is the last expansion starting at or before it.
#[derive(Clone, Copy, Debug)]
pub struct Expansions<'s> {
    /// The body's text: the item's own, then each expansion's words with the spaces between
    /// them.
    pub text: &'s str,
    /// The item's own text's length: offsets from here on are in the expansions.
    pub item_len: u32,
    /// Where each expansion's text starts, counted from `item_len`.
    pub starts: &'s [u32],
    /// Each expansion's call, as a range of `text`.
    pub calls: &'s [Range],
}

impl<'s> Expansions<'s> {
    /// How many expansions there are.
    pub fn len(&self) -> u32 {
        u32::try_from(self.starts.len()).unwrap_or(u32::MAX)
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.starts.is_empty()
    }

    /// The text of expansion `j` alone. `None` when there is no expansion `j`, or its range is
    /// not on a character boundary of the body's text.
    pub fn text(&self, j: u32) -> Option<&'s str> {
        let start = self.item_len as usize + *self.starts.get(j as usize)? as usize;
        let end = match self.starts.get(j as usize + 1) {
            Some(next) => self.item_len as usize + *next as usize,
            None => self.text.len(),
        };
        self.text.get(start..end)
    }

    /// The expansion holding the body offset `at`, and `at`'s offset in that expansion's
    /// text. `None` for an offset in the item's own text.
    pub fn holding(&self, at: u32) -> Option<(u32, u32)> {
        let offset = at.checked_sub(self.item_len)?;
        let j = self.starts.partition_point(|s| *s <= offset).checked_sub(1)?;
        Some((u32::try_from(j).ok()?, offset - self.starts[j]))
    }

    /// Where the call that made expansion `j` stands: in the item's own text, or in the text of
    /// an earlier expansion. `None` when there is no expansion `j`, or its call's writer is not
    /// an earlier one (a call is read before the expansion it makes).
    pub fn call_at(&self, j: u32) -> Option<crate::scope_origin::CallAt> {
        use crate::scope_origin::CallAt;
        let call = *self.calls.get(j as usize)?;
        if call.end <= self.item_len {
            return Some(CallAt::Item { start: call.start });
        }
        let (writer, start) = self.holding(call.start)?;
        (writer < j).then_some(CallAt::Expansion { expansion: writer, start })
    }
}

/// A `macro_rules!` or `macro` written in a body: its name and its definition's text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalMacro<'s> {
    pub name: &'s str,
    pub form: LocalForm<'s>,
}

/// The text of a macro a body defines, by its kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalForm<'s> {
    /// `macro_rules! m { rules }`: the rules, inside the delimiters.
    Rules(&'s str),
    /// `macro m(params) { body }` or `macro m { arms }`: the trees after the name, their
    /// delimiters kept (blaze-lower by_name.rs `macro2_of` splits them).
    Macro2(&'s str),
}

/// What a body macro call expanded to, as the expander writes it: the expansion's words
/// ([`blaze_expand::body_words`]) and the key of the call it expanded, which joins the body
/// record's read list.
pub struct BodyOut<'a> {
    pub key: ContentId,
    pub words: blaze_expand::BodyWords<'a>,
}

impl<'a> BodyOut<'a> {
    pub fn new_in(alloc: Alloc<'a>) -> BodyOut<'a> {
        BodyOut { key: ContentId([0; 32]), words: blaze_expand::BodyWords::new_in(alloc) }
    }
}

/// Macro expansion: blaze-expand's, through the caller, who resolves each call's path.
///
/// Every call form takes the mark of its path's first word (`path_mark`, [`BodyCall::path_mark`]
/// for a body call): `None` for a word as written in a file, the word's mark when a macro wrote
/// it. A `$crate` a macro wrote names the macro's crate only through that mark; where the name is
/// looked up is `mark_site::site_of` of it, never the text alone.
pub trait Expand {
    /// Expands the item call of `path` (as written) on `input` (its token tree's text,
    /// delimiters included) into `out`: its words, which the driver lowers in place of a text
    /// lexed again. `cfg` is the set the file is lowered under: a `cfg!` or `cfg_select!` of
    /// the call is answered by it, as the items' `#[cfg]`s are ([`crate::cfg_outcome`]), the
    /// one home of that answer in item and body position alike, never the expander's host.
    /// `input_marks` are the marks of `input`'s words when a macro wrote the text the call is
    /// in ([`BodyCall::input_marks`], the same seam): [`InputMarks::NONE`] for a file's call.
    fn expand(
        &mut self,
        path: &str,
        path_mark: Option<crate::mark_site::TextMark>,
        input: &str,
        input_marks: InputMarks<'_>,
        edition: Edition,
        cfg: &mut dyn crate::Cfg,
        out: &mut BodyOut<'_>,
    ) -> Expansion;

    /// Expands a macro call in a body into `out`. A new body's calls start when
    /// [`Expand::begin_body`] is called. `cfg` is the set the body is lowered under: a `cfg!`
    /// of the call (or of its eager input) is answered by it, as the body's `#[cfg]`s are
    /// ([`crate::cfg_outcome`]), never by the expander's host.
    fn expand_body(&mut self, call: BodyCall<'_>, cfg: &mut dyn crate::Cfg, out: &mut BodyOut<'_>) -> Expansion;
    /// Expands the derive of `path` (as written in `#[derive(..)]`) on `item`, the struct, enum
    /// or union as a derive sees it (blaze-expand's `Derives::input`), into `out`: items, an
    /// impl for a builtin derive.
    fn derive(
        &mut self,
        path: &str,
        path_mark: Option<crate::mark_site::TextMark>,
        item: &TokenStream<'_>,
        facts: blaze_expand::DeriveFacts,
        edition: Edition,
        out: &mut BodyOut<'_>,
    ) -> Expansion;
    /// Expands the attribute macro of `path` (as written) with `args` (inside the attribute's
    /// delimiters, `None` for `#[path]`) on `item`, the item without that attribute
    /// (blaze-expand's `ItemAttrs::attr_call`), into `out`: what replaces the item.
    /// [`Expansion::Unsupported`] when `path` names no attribute macro (`#[inline]`, a tool
    /// attribute): the walk goes on to the next attribute.
    fn attr(
        &mut self,
        path: &str,
        path_mark: Option<crate::mark_site::TextMark>,
        args: Option<&TokenStream<'_>>,
        item: &TokenStream<'_>,
        edition: Edition,
        out: &mut BodyOut<'_>,
    ) -> Expansion;

    /// A new body's lowering starts: the expansions of the last are no longer asked for.
    fn begin_body(&mut self) {}
}

/// Where a text being lowered came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    File,
    /// The output of an item macro call, `depth` expansions deep.
    Expansion { depth: u32 },
    /// An item declared in a body, by the text of the item holding it.
    BodyItem,
}

/// The item a record is of: its text and its outline entry (ranges relative to that text).
#[derive(Clone, Copy, Debug)]
pub struct ItemRef<'s> {
    pub source: Source,
    /// The text the item's outline is of: the file, an expansion, a body item.
    pub text: &'s str,
    pub index: ItemIndex,
    pub item: &'s Item,
}

impl<'s> ItemRef<'s> {
    /// The item's own text, which its records' ranges are of.
    pub fn item_text(&self) -> &'s str {
        self.item.range.of(self.text)
    }
}

/// Which record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum What {
    Signature,
    Body,
    /// An anonymous constant at this range of the item.
    AnonConst(Range),
}

/// What a lowering gave.
pub enum Outcome<'r, 'a> {
    /// The record's bytes and its view.
    Record { bytes: &'r [u8], view: View<'r> },
    /// It met `cfg` atoms not known yet.
    Need(&'r Need<'a>),
}

/// Where records go.
pub trait Sink {
    fn record(&mut self, item: &ItemRef<'_>, what: What, key: ContentId, outcome: Outcome<'_, '_>);
    /// A macro call on `item` (its attribute, derive or the call itself) refused by a bound.
    fn refused(&mut self, item: &ItemRef<'_>, refusal: Refusal<'_>);
}

/// The counts of a file's lowering, by what was done.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FileStats {
    /// Outline entries seen (fields and variants are their parent's signature, not counted).
    pub items: u64,
    pub signatures: u64,
    pub bodies: u64,
    pub anon_consts: u64,
    /// Lowerings that met a `cfg` atom not known yet.
    pub needs: u64,
    /// Items whose own `cfg` is false.
    pub cfg_out: u64,
    /// `use`, `extern crate`, `macro_rules!` and `macro`: blaze-scope's and blaze-expand's,
    /// not lowered here, by design.
    pub not_lowered: u64,
    /// Outline entries the parser could not read, or of a kind the outline does not take apart.
    pub errors: u64,
    pub macro_calls_expanded: u64,
    pub macro_calls_unsupported: u64,
    pub macro_calls_failed: u64,
    /// Item macro calls refused past [`EXPANSION_DEPTH`].
    pub macro_calls_too_deep: u64,
    /// Item macro calls refused past [`EXPANSION_BUDGET`].
    pub macro_calls_over_budget: u64,
    pub body_items: u64,
    /// Macro calls in bodies expanded and lowered in place.
    pub body_calls_expanded: u64,
    /// Unhandled rows across every record.
    pub unhandled: u64,
    pub record_bytes: u64,
}

/// A text waiting to be outlined and lowered: the file (`None`) or a text of its own.
struct Job<'a> {
    text: Option<AString<'a>>,
    /// An item macro's expansion: the words it was written as, read in place of its text.
    words: Option<ExpansionWords<'a>>,
    source: Source,
    /// The file's item this text descends from, an index into [`Driver::spent`]; `None` for
    /// the file itself.
    root: Option<u32>,
}

/// An item macro's expansion as words: the parser's words (no `EOF` padding), each one's
/// range in the expansion's text, and each one's mark ([`blaze_expand::BodyWord::mark`]): the
/// text alone says `$crate` but not whose, so a call's path in it is resolved by its mark.
struct ExpansionWords<'a> {
    words: AVec<'a, u32>,
    ranges: AVec<'a, Range>,
    marks: AVec<'a, Option<crate::mark_site::TextMark>>,
}

impl<'a> ExpansionWords<'a> {
    /// `out`'s words, which [`Expand::expand`] or [`Expand::derive`] wrote, and their text:
    /// each word's, a space before it where one may go, so the outline reads `fn clone`, not
    /// `fnclone`.
    fn of(out: &BodyOut<'_>, alloc: Alloc<'a>) -> (AString<'a>, ExpansionWords<'a>) {
        let n = out.words.words.len();
        let mut words = Vec::with_capacity_in(n, alloc);
        words.extend_from_slice(&out.words.parser.words[..n]);
        let mut ranges = Vec::with_capacity_in(n, alloc);
        let mut marks = Vec::with_capacity_in(n, alloc);
        let mut text = String::with_capacity_in(out.words.text.len() + n, alloc);
        for w in out.words.words.iter() {
            if w.space_before && !text.is_empty() {
                text.push(' ');
            }
            let start = u32::try_from(text.len()).expect("an expansion under 4 GiB");
            text.push_str(&out.words.text[w.text.0 as usize..w.text.1 as usize]);
            ranges.push(Range::new(start, u32::try_from(text.len()).expect("an expansion under 4 GiB")));
            marks.push(w.mark);
        }
        (text, ExpansionWords { words, ranges, marks })
    }

    /// The words inside `range` of the text, their ranges in `range`'s own text.
    fn slice(&self, range: Range, alloc: Alloc<'a>) -> ExpansionWords<'a> {
        let first = self.ranges.partition_point(|r| r.start < range.start);
        let past = self.ranges.partition_point(|r| r.end <= range.end);
        let mut words = Vec::with_capacity_in(past.saturating_sub(first) + parser::Tokens::PAD, alloc);
        let mut ranges = Vec::with_capacity_in(past.saturating_sub(first), alloc);
        let mut marks = Vec::with_capacity_in(past.saturating_sub(first), alloc);
        if first < past {
            words.extend_from_slice(&self.words[first..past]);
            ranges.extend(self.ranges[first..past].iter().map(|r| Range::new(r.start - range.start, r.end - range.start)));
            marks.extend_from_slice(&self.marks[first..past]);
        }
        ExpansionWords { words, ranges, marks }
    }

    /// The mark of the word holding byte `at` of the text; `None` for a byte between words.
    /// The outline reads `$crate` as ` crate` (same bytes, [`ExpansionWords::outline_text`]),
    /// so a path's range may start one byte into its first word.
    fn mark_at(&self, at: u32) -> Option<crate::mark_site::TextMark> {
        let i = self.ranges.partition_point(|r| r.end <= at);
        match self.ranges.get(i) {
            Some(r) if r.start <= at => self.marks[i],
            _ => None,
        }
    }

    /// The marks of the words inside `range` as [`InputMarks`] rows: each marked word's start
    /// (in this words' text, ascending: the words are in order) and its mark's bytes. An item
    /// call's input in a text a macro wrote is lexed with them ([`Expand::expand`]).
    fn mark_rows(&self, range: Range, alloc: Alloc<'a>) -> (AVec<'a, u32>, AVec<'a, u8>) {
        let mut starts = Vec::new_in(alloc);
        let mut bytes = Vec::new_in(alloc);
        let first = self.ranges.partition_point(|r| r.start < range.start);
        for (r, mark) in self.ranges[first..].iter().zip(&self.marks[first..]).take_while(|(r, _)| r.start < range.end) {
            if let Some(mark) = mark {
                starts.push(r.start);
                bytes.extend_from_slice(&mark.to_bytes());
            }
        }
        (starts, bytes)
    }

    /// Whether any word carries a mark: when none does, every path in the text is as written
    /// in a file and the driver reads no marks.
    fn any_marked(&self) -> bool {
        self.marks.iter().any(Option::is_some)
    }

    /// The tokens of `text` (this words' text) as blaze-expand lexes them, each in a context
    /// carrying its word's mark: a probe walked beside the item's own tokens (same attributes,
    /// same `cfg_attr`s applied, so the same token indices) to read the mark of an attribute's
    /// or a derive's path. The probe is never a macro's input: a [`crate::mark_site::TextMark`]
    /// has no expansion key, so the contexts here say only where each token resolves.
    fn probe(&self, text: &str, edition: Edition, alloc: Alloc<'a>) -> Option<TokenStream<'a>> {
        let lexed = blaze_expand::lex(text, edition, alloc).ok()?;
        let places = blaze_expand::origin_column::input_places(text, blaze_expand::origin_column::InputShape::Whole, edition, alloc).ok()?;
        if places.len() != lexed.len() {
            return None;
        }
        let mut probe = TokenStream::new_in(alloc);
        for (t, &(start, _)) in lexed.tokens().iter().zip(places.iter()) {
            let ctx = match self.mark_at(start) {
                None => t.ctx,
                Some(m) => {
                    let mark = blaze_expand::Mark { of: blaze_expand::MarkOf::This, transparency: m.transparency, edition: m.edition, site: m.site };
                    probe.context(blaze_expand::CtxNode { parent: t.ctx, mark }).ok()?
                }
            };
            probe.push(t.kind, lexed.text_of(t), ctx).ok()?;
        }
        Some(probe)
    }

    /// The words, padded for the parser.
    fn padded(&self, alloc: Alloc<'a>) -> AVec<'a, u32> {
        let mut words = Vec::with_capacity_in(self.words.len() + parser::Tokens::PAD, alloc);
        words.extend_from_slice(&self.words);
        words.extend(core::iter::repeat_n(parser::Tokens::EOF_WORD, parser::Tokens::PAD));
        words
    }

    /// `text` with each `$crate` word's `$` a space: what the outline reads, which finds
    /// items and their ranges (same bytes, same places) and lowers nothing.
    fn outline_text(&self, text: &str, alloc: Alloc<'a>) -> AString<'a> {
        let mut out = String::with_capacity_in(text.len(), alloc);
        let mut at = 0usize;
        for r in self.ranges.iter().filter(|r| r.of(text) == "$crate") {
            out.push_str(&text[at..r.start as usize]);
            out.push(' ');
            at = r.start as usize + 1;
        }
        out.push_str(&text[at..]);
        out
    }
}

/// The mark of the first identifier at or after token `from` of `stream` (a path's first
/// segment, past a leading `::`).
fn first_word_mark(stream: &TokenStream<'_>, from: usize) -> Option<crate::mark_site::TextMark> {
    let tokens = stream.tokens();
    let at = (from..tokens.len()).find(|&i| matches!(tokens[i].kind, blaze_expand::TokenKind::Ident { .. }))?;
    stream.hygiene(tokens[at].ctx).mark.map(crate::mark_site::TextMark::of)
}

/// The marks of the item's derive paths, in the order blaze-expand's `ItemAttrs::derives` lists
/// them: `probe` walked as `derives` walks (each `cfg_attr` and true `cfg` applied), then each
/// `#[derive(..)]`/`#[derive_const(..)]` read in order. `None` when the item's `cfg` is false or
/// an attribute is refused (the derives are not run then either).
fn derive_marks<'a>(probe: TokenStream<'a>, cfg: &mut dyn Cfg, alloc: Alloc<'a>) -> Option<AVec<'a, Option<crate::mark_site::TextMark>>> {
    let mut walk = blaze_expand::ItemAttrs::new(probe);
    loop {
        let next = walk.next(&mut |name, value| match cfg.atom(name, value) {
            CfgAnswer::Yes => Ok(true),
            CfgAnswer::No => Ok(false),
            CfgAnswer::Need => Err(()),
        });
        match next {
            Ok(blaze_expand::NextAttr::End) => break,
            Ok(blaze_expand::NextAttr::Call { .. }) => {}
            Ok(blaze_expand::NextAttr::Removed) | Err(_) => return None,
        }
    }
    let item = walk.item();
    let tokens = item.tokens();
    let mut marks = Vec::new_in(alloc);
    let mut at = 0usize;
    loop {
        let hash = matches!(tokens.get(at).map(|t| t.kind), Some(blaze_expand::TokenKind::Punct { char: '#', .. }));
        let bracket = matches!(tokens.get(at + 1).map(|t| t.kind), Some(blaze_expand::TokenKind::Open(blaze_expand::Delim::Bracket)));
        let Some(close) = (hash && bracket).then(|| item.close_of(at + 1)).flatten() else { break };
        let attr = item.copy_in(at..close + 1, alloc).ok()?;
        if let Some((_, paths)) = blaze_expand::derive_attr(&attr, alloc) {
            marks.extend(paths.iter().map(|p| first_word_mark(&attr, p.start as usize)));
        }
        at = close + 1;
    }
    Some(marks)
}

struct Driver<'d, 'a> {
    edition: Edition,
    cfg: &'d mut dyn Cfg,
    values: &'d mut dyn ValueNames,
    sink: &'d mut dyn Sink,
    alloc: Alloc<'a>,
    stats: FileStats,
    /// The current item's text with its body's expansions after it, when it has any: what
    /// the ranges of its anonymous constants and local items are of.
    grown: AString<'a>,
    /// The expansions each of the file's items has made, its descendants' counted.
    spent: AVec<'a, u32>,
    /// The file's item the current item descends from, an index into `spent`.
    root: u32,
}

impl<'a> Driver<'_, 'a> {
    /// Hands a lowering to the sink with its record; queues its anonymous constants and, for
    /// a body, the items it declares.
    fn emit(
        &mut self,
        at: &ItemRef<'_>,
        what: What,
        key: ContentId,
        result: &Result<Lowered<'a, '_>, Need<'a>>,
        consts: &mut AVec<'a, Range>,
        jobs: Option<(&mut AVec<'a, Job<'a>>, Option<&ExpansionWords<'a>>)>,
    ) -> bool {
        match result {
            Ok(l) => {
                let bytes = l.to_record(self.alloc);
                self.stats.record_bytes += bytes.len() as u64;
                let view = l.view();
                self.stats.unhandled += view.unhandled.len() as u64;
                if view.cfg_out() {
                    self.stats.cfg_out += 1;
                }
                // An anonymous constant's range is of the item's text with the expansions
                // after it, which its own lowering takes whole.
                consts.extend(view.raw_consts().iter().copied());
                self.stats.body_calls_expanded += view.expansion_calls().count() as u64;
                if !view.expanded().is_empty() {
                    let expanded = core::str::from_utf8(view.expanded()).expect("an expansion's text is UTF-8");
                    self.grown.clear();
                    self.grown.push_str(at.item_text());
                    self.grown.push_str(expanded);
                }
                if let Some((jobs, item_words)) = jobs {
                    for n in view.rows {
                        if n.tag != ROW_LOCAL_ITEM {
                            continue;
                        }
                        // A row that does not read back is a record this lowering wrote wrong:
                        // counted as an error, never lowered from wrong bytes.
                        let Ok(item) = view.local_item(*n) else {
                            self.stats.errors += 1;
                            continue;
                        };
                        if item.range.is_empty() {
                            continue;
                        }
                        let Ok(item_text) = view.text_at(item.range) else {
                            self.stats.errors += 1;
                            continue;
                        };
                        let mut s = String::new_in(self.alloc);
                        s.push_str(item_text);
                        // An item of a body written by a macro is read from its words too,
                        // when it is in the item's own text (not a call's expansion in it).
                        let words = item_words
                            .filter(|_| !item.range.in_expansion())
                            .map(|w| w.slice(item.range.raw(), self.alloc));
                        jobs.push(Job { text: Some(s), words, source: Source::BodyItem, root: Some(self.root) });
                        self.stats.body_items += 1;
                    }
                }
                let cfg_out = view.cfg_out();
                self.sink.record(at, what, key, Outcome::Record { bytes: &bytes, view });
                !cfg_out
            }
            Err(need) => {
                self.stats.needs += 1;
                self.sink.record(at, what, key, Outcome::Need(need));
                false
            }
        }
    }

    /// Lowers the anonymous constants queued in `consts`, and theirs.
    fn anon_consts(&mut self, at: &ItemRef<'_>, consts: &mut AVec<'a, Range>, expand: &mut dyn Expand) {
        // Taken out while the constants lower: one in an expansion is of the grown text.
        let grown = core::mem::replace(&mut self.grown, String::new_in(self.alloc));
        let text = if grown.is_empty() { at.item_text() } else { grown.as_str() };
        while let Some(range) = consts.pop() {
            let key = anon_const_key(text, range, self.edition);
            let result = lower_anon_const_expanding(text, range, self.edition, self.cfg, self.values, expand, self.alloc);
            self.stats.anon_consts += 1;
            self.emit(at, What::AnonConst(range), key, &result, consts, None);
        }
    }

    /// One text: its outline, then each entry in order. An entry whose signature is off by
    /// `cfg`, or not known yet, takes its descendants with it.
    fn text(
        &mut self,
        text: &str,
        words: Option<&ExpansionWords<'a>>,
        source: Source,
        root: Option<u32>,
        expand: &mut dyn Expand,
        jobs: &mut AVec<'a, Job<'a>>,
    ) {
        if let Some(root) = root {
            self.root = root;
        }
        let outline = match words {
            None => blaze_file_items::outline(text, self.edition, self.alloc),
            Some(w) => blaze_file_items::outline(&w.outline_text(text, self.alloc), self.edition, self.alloc),
        };
        let items = outline.items();
        let mut consts: AVec<'a, Range> = Vec::new_in(self.alloc);
        let mut i = 0usize;
        while i < items.len() {
            let item = &items[i];
            let index = ItemIndex(u32::try_from(i).expect("an outline's entries are counted in u32"));
            // Each of the file's own items is a root with a budget of its own.
            if root.is_none() {
                self.root = u32::try_from(self.spent.len()).expect("a file's items are counted in u32");
                self.spent.push(0);
            }
            let at = ItemRef { source, text, index, item };
            let item_text = at.item_text();
            let next = i + 1;
            let past = item.end.at().max(next);
            match item.detail {
                ItemDetail::Field { .. } | ItemDetail::Variant { .. } => {
                    i = next;
                    continue;
                }
                _ => self.stats.items += 1,
            }
            match item.kind() {
                ItemKind::Use | ItemKind::ExternCrate | ItemKind::MacroRules | ItemKind::Macro2 => {
                    self.stats.not_lowered += 1;
                    i = past;
                    continue;
                }
                ItemKind::Error | ItemKind::Unhandled => {
                    self.stats.errors += 1;
                    i = past;
                    continue;
                }
                ItemKind::MacroCall => {
                    let own = words.map(|w| w.slice(item.range, self.alloc));
                    self.macro_call(&at, own.as_ref(), source, expand, jobs);
                    i = past;
                    continue;
                }
                _ => {}
            }
            // The item's own words when a macro wrote it: their marks say where each path a
            // call, an attribute or a derive names is read.
            let own = words.map(|w| w.slice(item.range, self.alloc));
            // An attribute macro takes the item: its expansion is lowered in its place.
            if self.attr_macro(&at, own.as_ref(), outline.attrs(item), source, expand, jobs) {
                i = past;
                continue;
            }
            let has_body = matches!(item.detail, ItemDetail::Fn { .. } | ItemDetail::Const { .. } | ItemDetail::Static { .. })
                && item.body.is_some();
            // A fn's, const's or static's signature is keyed by its text before the body; every
            // other item's signature holds what is written inside it (fields, variants, the
            // associated entries' names), so its key is its whole text.
            let key_text = match item.detail {
                ItemDetail::Fn { .. } | ItemDetail::Const { .. } | ItemDetail::Static { .. } => {
                    item.signature.get(item_text).unwrap_or(item_text)
                }
                _ => item_text,
            };
            let key = signature_key(key_text, self.edition, has_body);
            let padded = own.as_ref().map(|w| w.padded(self.alloc));
            let item_words = own.as_ref().zip(padded.as_ref()).map(|(o, p)| ItemWords { words: p, ranges: &o.ranges });
            let result = match item_words {
                None => lower_signature(item_text, self.edition, self.cfg, self.alloc),
                Some(w) => lower_signature_words(item_text, w, self.edition, self.cfg, self.alloc),
            };
            self.stats.signatures += 1;
            let live = self.emit(&at, What::Signature, key, &result, &mut consts, None);
            drop(result);
            if !live {
                consts.clear();
                i = past;
                continue;
            }
            if matches!(item.kind(), ItemKind::Struct | ItemKind::Enum | ItemKind::Union) {
                self.derives(&at, own.as_ref(), outline.attrs(item), source, expand, jobs);
            }
            if has_body {
                let key = body_key(item_text, self.edition);
                let result = match item_words {
                    None => lower_body_expanding(item_text, self.edition, self.cfg, self.values, expand, self.alloc),
                    Some(w) => lower_body_words_expanding(item_text, w, self.edition, self.cfg, self.values, expand, self.alloc),
                };
                self.stats.bodies += 1;
                self.emit(&at, What::Body, key, &result, &mut consts, Some((jobs, own.as_ref())));
            }
            self.anon_consts(&at, &mut consts, expand);
            i = next;
        }
    }

    /// The derives written on `at` (`attrs`, its attributes): each path of each `#[derive(..)]`
    /// in order, its expansion queued as a text of its own, as an item macro call's is. A
    /// `#[cfg_attr(pred, ..)]` is applied under the unit's cfg set first, its derives read in
    /// its place; one whose predicate meets a cfg atom not known yet is counted unsupported.
    /// The item's `cfg`s are the signature's, already true here.
    fn derives(
        &mut self,
        at: &ItemRef<'_>,
        own: Option<&ExpansionWords<'a>>,
        attrs: &[Attr],
        source: Source,
        expand: &mut dyn Expand,
        jobs: &mut AVec<'a, Job<'a>>,
    ) {
        let depth = match source {
            Source::Expansion { depth } => depth + 1,
            Source::File | Source::BodyItem => 1,
        };
        let text = at.item_text();
        if !attrs.iter().any(|a| !a.inner && matches!(a.kind, AttrKind::Other | AttrKind::CfgAttr)) {
            return;
        }
        let Ok(tokens) = blaze_expand::lex(text, self.edition, self.alloc) else { return };
        // Each derive path's mark, when a macro wrote the item: walked over a probe of the same
        // tokens, so the n-th mark is the n-th use's. A probe that cannot be made, or that
        // disagrees in count, stops the item's derives (counted failed): a derive is never
        // resolved without the mark its text was written with.
        let marks = match own.filter(|w| w.any_marked()) {
            None => None,
            Some(w) => match w.probe(text, self.edition, self.alloc).and_then(|p| derive_marks(p, &mut *self.cfg, self.alloc)) {
                Some(marks) => Some(marks),
                None => {
                    self.stats.macro_calls_failed += 1;
                    return;
                }
            },
        };
        let cfg = &mut *self.cfg;
        let derives = blaze_expand::ItemAttrs::new(tokens).derives(&mut |name, value| match cfg.atom(name, value) {
            CfgAnswer::Yes => Ok(true),
            CfgAnswer::No => Ok(false),
            CfgAnswer::Need => Err(()),
        });
        let derives = match derives {
            Ok(Some(derives)) => derives,
            Ok(None) => return,
            // A cfg atom not known yet, or a malformed attribute: its derives, if any, are not run.
            Err(_) => {
                if text.contains("derive") {
                    self.stats.macro_calls_unsupported += 1;
                }
                return;
            }
        };
        if marks.as_ref().is_some_and(|m| m.len() != derives.uses.len()) {
            self.stats.macro_calls_failed += 1;
            return;
        }
        for (n, u) in derives.uses.iter().enumerate() {
            if let Some(bound) = self.bound(depth) {
                self.refuse(at, &u.path, bound);
                continue;
            }
            let path_mark = marks.as_ref().and_then(|m| m[n]);
            let mut out = BodyOut::new_in(self.alloc);
            let expansion = expand.derive(&u.path, path_mark, &derives.input, derives.facts(u), self.edition, &mut out);
            self.queue_expansion(expansion, &out, depth, jobs);
        }
    }

    /// The first outer attribute of `at` that is an attribute macro, expanded on the item
    /// without it and queued: whether one took the item (expanded, or refused, which leaves
    /// nothing of the item to lower). The attributes after it are the expansion's to meet.
    fn attr_macro(
        &mut self,
        at: &ItemRef<'_>,
        own: Option<&ExpansionWords<'a>>,
        attrs: &[Attr],
        source: Source,
        expand: &mut dyn Expand,
        jobs: &mut AVec<'a, Job<'a>>,
    ) -> bool {
        if !attrs.iter().any(|a| !a.inner && matches!(a.kind, AttrKind::Other | AttrKind::CfgAttr)) {
            return false;
        }
        let Ok(tokens) = blaze_expand::lex(at.item_text(), self.edition, self.alloc) else { return false };
        let mut walk = blaze_expand::ItemAttrs::new(tokens);
        // When a macro wrote the item, a probe of the same tokens carrying their marks, walked in
        // step with `walk` (the same `cfg_attr`s applied, so the same indices): an attribute's
        // path is read by its first word's mark. A probe that cannot be made or that leaves step
        // stops the item (counted failed), never resolves the path without its mark.
        let mut probe = match own.filter(|w| w.any_marked()) {
            None => None,
            Some(w) => match w.probe(at.item_text(), self.edition, self.alloc) {
                Some(p) => Some(blaze_expand::ItemAttrs::new(p)),
                None => {
                    self.stats.macro_calls_failed += 1;
                    return true;
                }
            },
        };
        loop {
            let cfg = &mut *self.cfg;
            let next = walk.next(&mut |name, value| match cfg.atom(name, value) {
                CfgAnswer::Yes => Ok(true),
                CfgAnswer::No => Ok(false),
                CfgAnswer::Need => Err(()),
            });
            // The item's own `cfg`s and a cfg atom not known yet are the signature's to report.
            let (path, attr) = match next {
                Ok(blaze_expand::NextAttr::Call { path, attr }) => (path, attr),
                Ok(blaze_expand::NextAttr::End | blaze_expand::NextAttr::Removed) | Err(_) => return false,
            };
            let path_mark = match probe.as_mut() {
                None => None,
                Some(p) => {
                    let cfg = &mut *self.cfg;
                    let stepped = p.next(&mut |name, value| match cfg.atom(name, value) {
                        CfgAnswer::Yes => Ok(true),
                        CfgAnswer::No => Ok(false),
                        CfgAnswer::Need => Err(()),
                    });
                    match stepped {
                        // `#`, `[`, then the path.
                        Ok(blaze_expand::NextAttr::Call { attr: same, .. }) if same == attr => first_word_mark(p.item(), attr.start as usize + 2),
                        _ => {
                            self.stats.macro_calls_failed += 1;
                            return true;
                        }
                    }
                }
            };
            let depth = match source {
                Source::Expansion { depth } => depth + 1,
                Source::File | Source::BodyItem => 1,
            };
            let Ok(call) = walk.attr_call(attr) else {
                self.stats.macro_calls_unsupported += 1;
                return true;
            };
            // Checked before the run: the attribute may be inert (`#[inline]`), but no item
            // reaches this deep but by a chain of expansions that is already past the bound.
            if let Some(bound) = self.bound(depth) {
                self.refuse(at, &path, bound);
                return true;
            }
            let mut out = BodyOut::new_in(self.alloc);
            match expand.attr(&path, path_mark, call.args.as_ref(), &call.item, self.edition, &mut out) {
                Expansion::Unsupported => continue,
                expansion => {
                    // The derives written before the macro ran first and are not in its input
                    // (blaze-expand `ItemAttrs::attr_call`): the first `derives_taken` of the
                    // item's derives, each on the item as a derive sees it.
                    if call.derives_taken > 0 {
                        let marks = match probe.as_ref() {
                            None => None,
                            Some(p) => match derive_marks(p.item().clone(), &mut *self.cfg, self.alloc) {
                                Some(marks) => Some(marks),
                                None => {
                                    self.stats.macro_calls_failed += 1;
                                    self.queue_expansion(expansion, &out, depth, jobs);
                                    return true;
                                }
                            },
                        };
                        let cfg = &mut *self.cfg;
                        let derives = blaze_expand::ItemAttrs::new(walk.item().clone()).derives(&mut |name, value| match cfg.atom(name, value) {
                            CfgAnswer::Yes => Ok(true),
                            CfgAnswer::No => Ok(false),
                            CfgAnswer::Need => Err(()),
                        });
                        match derives {
                            Ok(Some(derives)) if marks.as_ref().is_some_and(|m| m.len() != derives.uses.len()) => {
                                self.stats.macro_calls_failed += 1;
                            }
                            Ok(Some(derives)) => {
                                for (n, u) in derives.uses.iter().enumerate().take(call.derives_taken) {
                                    let path_mark = marks.as_ref().and_then(|m| m[n]);
                                    let mut derived = BodyOut::new_in(self.alloc);
                                    let made = expand.derive(&u.path, path_mark, &derives.input, derives.facts(u), self.edition, &mut derived);
                                    self.queue_expansion(made, &derived, depth, jobs);
                                }
                            }
                            // A later `#[cfg]` false, a cfg atom not known yet or a malformed
                            // attribute: the derives are not run, and counted.
                            Ok(None) | Err(_) => self.stats.macro_calls_unsupported += 1,
                        }
                    }
                    self.queue_expansion(expansion, &out, depth, jobs);
                    return true;
                }
            }
        }
    }

    /// The bound a call `depth` expansions deep, from the current item, would pass.
    fn bound(&self, depth: u32) -> Option<Bound> {
        if depth > EXPANSION_DEPTH {
            Some(Bound::Depth)
        } else if self.spent[self.root as usize] >= EXPANSION_BUDGET {
            Some(Bound::Budget)
        } else {
            None
        }
    }

    /// The call of `path` on `at`, refused by `bound`: counted and handed to the sink.
    fn refuse(&mut self, at: &ItemRef<'_>, path: &str, bound: Bound) {
        match bound {
            Bound::Depth => self.stats.macro_calls_too_deep += 1,
            Bound::Budget => self.stats.macro_calls_over_budget += 1,
        }
        self.sink.refused(at, Refusal { path, bound });
    }

    /// Queues an item macro's expansion, `out`, as a text of its own read from its words.
    fn queue_expansion(&mut self, expansion: Expansion, out: &BodyOut<'_>, depth: u32, jobs: &mut AVec<'a, Job<'a>>) {
        match expansion {
            Expansion::Expanded => {
                self.stats.macro_calls_expanded += 1;
                self.spent[self.root as usize] += 1;
                let (text, words) = ExpansionWords::of(out, self.alloc);
                jobs.push(Job { text: Some(text), words: Some(words), source: Source::Expansion { depth }, root: Some(self.root) });
            }
            Expansion::Unsupported => self.stats.macro_calls_unsupported += 1,
            Expansion::Failed => self.stats.macro_calls_failed += 1,
        }
    }

    /// The item macro call `at`, expanded and queued. `own`, the item's words when a macro wrote
    /// it, gives the path's first word its mark.
    fn macro_call(&mut self, at: &ItemRef<'_>, own: Option<&ExpansionWords<'a>>, source: Source, expand: &mut dyn Expand, jobs: &mut AVec<'a, Job<'a>>) {
        let ItemDetail::MacroCall { path: Some(path), input: Some(input) } = at.item.detail else {
            self.stats.macro_calls_failed += 1;
            return;
        };
        let depth = match source {
            Source::Expansion { depth } => depth + 1,
            Source::File | Source::BodyItem => 1,
        };
        let text = at.item_text();
        if let Some(bound) = self.bound(depth) {
            self.refuse(at, path.of(text), bound);
            return;
        }
        let mut out = BodyOut::new_in(self.alloc);
        let path_mark = own.and_then(|w| w.mark_at(path.start));
        let (starts, bytes) = match own {
            Some(w) => w.mark_rows(input, self.alloc),
            None => (Vec::new_in(self.alloc), Vec::new_in(self.alloc)),
        };
        let input_marks = InputMarks::new(input.start, &starts, &bytes);
        let expansion = expand.expand(path.of(text), path_mark, input.of(text), input_marks, self.edition, &mut *self.cfg, &mut out);
        self.queue_expansion(expansion, &out, depth, jobs);
    }
}

/// Lowers every item of `text`, a source file of `edition`, with `cfg` the build's cfg set and
/// `expand` the item macro expander, handing each record to `sink`; every allocation in
/// `alloc`.
pub fn lower_file(
    text: &str,
    edition: Edition,
    cfg: &mut dyn Cfg,
    values: &mut dyn ValueNames,
    expand: &mut dyn Expand,
    sink: &mut dyn Sink,
    alloc: Alloc<'_>,
) -> FileStats {
    let mut d = Driver { edition, cfg, values, sink, alloc, stats: FileStats::default(), grown: String::new_in(alloc), spent: Vec::new_in(alloc), root: 0 };
    let mut jobs: AVec<'_, Job<'_>> = Vec::new_in(alloc);
    jobs.push(Job { text: None, words: None, source: Source::File, root: None });
    while let Some(job) = jobs.pop() {
        match &job.text {
            None => d.text(text, None, job.source, job.root, expand, &mut jobs),
            Some(owned) => d.text(owned.as_str(), job.words.as_ref(), job.source, job.root, expand, &mut jobs),
        }
    }
    d.stats
}
