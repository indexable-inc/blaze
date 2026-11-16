//! A file to its records: the outline (blaze-file-items), then one lowering per item, each
//! with its key computed from text before the work (rule 65) and handed to a [`Sink`] with
//! its record. Item macros are expanded by the caller's [`Expand`] (blaze-expand's, plugged
//! in by the caller so this stays sans-IO) and their output lowered as a text of its own;
//! the items a body declares are lowered the same way, from the body record's rows.
//!
//! Nothing recurses: the texts to lower (the file, each expansion, each body's items) wait on
//! one work list, and each one's anonymous constants on another.

use blaze_ids::ContentId;
use blaze_own::{AString, AVec, Alloc, String, Vec};
use blaze_surface::{Edition, Item, ItemDetail, ItemIndex, ItemKind, Range};

use crate::columns::View;
use crate::form::{ROW_LOCAL_ITEM, LocalItem};
use crate::{Cfg, Lowered, Need, anon_const_key, body_key, lower_anon_const, lower_body, lower_signature, signature_key};

/// How deep item macros may expand into item macros (rustc's `recursion_limit` default).
pub const EXPANSION_DEPTH: u32 = 128;

/// What an item macro call became.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Expansion {
    /// Its items' text is in the output string.
    Items,
    /// The expander has no way to expand it (a proc macro not built, a builtin not taken).
    Unsupported,
    /// It expanded to an error, which the expander reports.
    Failed,
}

/// Item macro expansion: blaze-expand's, through the caller.
pub trait Expand {
    /// Expands the call of `path` (as written) on `input` (its token tree's text, delimiters
    /// included) into `out`.
    fn expand(&mut self, path: &str, input: &str, edition: Edition, out: &mut AString<'_>) -> Expansion;
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
    /// Expansions past [`EXPANSION_DEPTH`].
    pub macro_calls_too_deep: u64,
    pub body_items: u64,
    /// Unhandled rows across every record.
    pub unhandled: u64,
    pub record_bytes: u64,
}

/// A text waiting to be outlined and lowered: the file (`None`) or a text of its own.
struct Job<'a> {
    text: Option<AString<'a>>,
    source: Source,
}

struct Driver<'d, 'a> {
    edition: Edition,
    cfg: &'d mut dyn Cfg,
    sink: &'d mut dyn Sink,
    alloc: Alloc<'a>,
    stats: FileStats,
}

impl<'a> Driver<'_, 'a> {
    /// Hands a lowering to the sink with its record; queues its anonymous constants and, for
    /// a body, the items it declares.
    fn emit(
        &mut self,
        at: &ItemRef<'_>,
        what: What,
        key: ContentId,
        result: &Result<Lowered<'a>, Need<'a>>,
        consts: &mut AVec<'a, Range>,
        jobs: Option<&mut AVec<'a, Job<'a>>>,
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
                consts.extend(view.consts.iter().copied());
                if let Some(jobs) = jobs {
                    let text = at.item_text();
                    for n in view.rows {
                        if n.tag != ROW_LOCAL_ITEM {
                            continue;
                        }
                        let Ok(item) = LocalItem::decode(*n) else { continue };
                        if item.range.is_empty() {
                            continue;
                        }
                        let mut s = String::new_in(self.alloc);
                        s.push_str(item.range.of(text));
                        jobs.push(Job { text: Some(s), source: Source::BodyItem });
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
    fn anon_consts(&mut self, at: &ItemRef<'_>, consts: &mut AVec<'a, Range>) {
        let text = at.item_text();
        while let Some(range) = consts.pop() {
            let key = anon_const_key(text, range, self.edition);
            let result = lower_anon_const(text, range, self.edition, self.cfg, self.alloc);
            self.stats.anon_consts += 1;
            self.emit(at, What::AnonConst(range), key, &result, consts, None);
        }
    }

    /// One text: its outline, then each entry in order. An entry whose signature is off by
    /// `cfg`, or not known yet, takes its descendants with it.
    fn text(&mut self, text: &str, source: Source, expand: &mut dyn Expand, jobs: &mut AVec<'a, Job<'a>>) {
        let outline = blaze_file_items::outline(text, self.edition, self.alloc);
        let items = outline.items();
        let mut consts: AVec<'a, Range> = Vec::new_in(self.alloc);
        let mut i = 0usize;
        while i < items.len() {
            let item = &items[i];
            let index = ItemIndex(u32::try_from(i).expect("an outline's entries are counted in u32"));
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
                    self.macro_call(&at, source, expand, jobs);
                    i = past;
                    continue;
                }
                _ => {}
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
            let result = lower_signature(item_text, self.edition, self.cfg, self.alloc);
            self.stats.signatures += 1;
            let live = self.emit(&at, What::Signature, key, &result, &mut consts, None);
            drop(result);
            if !live {
                consts.clear();
                i = past;
                continue;
            }
            if has_body {
                let key = body_key(item_text, self.edition);
                let result = lower_body(item_text, self.edition, self.cfg, self.alloc);
                self.stats.bodies += 1;
                self.emit(&at, What::Body, key, &result, &mut consts, Some(jobs));
            }
            self.anon_consts(&at, &mut consts);
            i = next;
        }
    }

    fn macro_call(&mut self, at: &ItemRef<'_>, source: Source, expand: &mut dyn Expand, jobs: &mut AVec<'a, Job<'a>>) {
        let ItemDetail::MacroCall { path: Some(path), input: Some(input) } = at.item.detail else {
            self.stats.macro_calls_failed += 1;
            return;
        };
        let depth = match source {
            Source::Expansion { depth } => depth + 1,
            Source::File | Source::BodyItem => 1,
        };
        if depth > EXPANSION_DEPTH {
            self.stats.macro_calls_too_deep += 1;
            return;
        }
        let text = at.item_text();
        let mut out = String::new_in(self.alloc);
        match expand.expand(path.of(text), input.of(text), self.edition, &mut out) {
            Expansion::Items => {
                self.stats.macro_calls_expanded += 1;
                jobs.push(Job { text: Some(out), source: Source::Expansion { depth } });
            }
            Expansion::Unsupported => self.stats.macro_calls_unsupported += 1,
            Expansion::Failed => self.stats.macro_calls_failed += 1,
        }
    }
}

/// Lowers every item of `text`, a source file of `edition`, with `cfg` the build's cfg set and
/// `expand` the item macro expander, handing each record to `sink`; every allocation in
/// `alloc`.
pub fn lower_file(
    text: &str,
    edition: Edition,
    cfg: &mut dyn Cfg,
    expand: &mut dyn Expand,
    sink: &mut dyn Sink,
    alloc: Alloc<'_>,
) -> FileStats {
    let mut d = Driver { edition, cfg, sink, alloc, stats: FileStats::default() };
    let mut jobs: AVec<'_, Job<'_>> = Vec::new_in(alloc);
    jobs.push(Job { text: None, source: Source::File });
    while let Some(job) = jobs.pop() {
        match &job.text {
            None => d.text(text, job.source, expand, &mut jobs),
            Some(owned) => d.text(owned.as_str(), job.source, expand, &mut jobs),
        }
    }
    d.stats
}
