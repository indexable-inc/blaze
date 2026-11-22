//! The columns a lowering writes, the view every reader takes (of the columns in memory or of
//! a record), and the record: one blaze-format buffer whose root holds a slice per column and
//! the head, so a record is read in place.

use core::mem::offset_of;

use blaze_format_core::{FormatError, Own, Record, Slice, Writer, WriterOpts, open_as};
use blaze_ids::ContentId;
use blaze_own::{AVec, Alloc, Vec};
use blaze_surface::Range;

use crate::form::*;

/// The number of `u32` words in a [`Head`].
pub const HEAD_WORDS: usize = head::FN_TEXT_PREFIX + 1;

/// What a lowering is about, beyond its columns: the root of a body, or the parts of a
/// signature. Words by [`Head`]'s constants; lists are (start, len) pairs.
pub type Head = [u32; HEAD_WORDS];

/// The head's words.
pub mod head {
    /// [`crate::Mode`] as 0 (signature), 1 (body), 2 (anonymous constant), 3 (a lone type).
    pub const MODE: usize = 0;
    pub const EDITION: usize = 1;
    /// The item's parser kind, as its number; `u32::MAX` for an anonymous constant.
    pub const SYNTAX: usize = 2;
    /// [`crate::machine::sig`]'s flags.
    pub const FLAGS: usize = 3;
    /// Rows of `GenericParam`.
    pub const GENERICS: usize = 4;
    /// Rows of `WherePred`.
    pub const WHERE: usize = 6;
    /// Rows of `Attr`.
    pub const ATTRS: usize = 8;
    /// Rows of `Param` (a signature); in a body, the parameters' patterns in `ids`.
    pub const PARAMS: usize = 10;
    /// The return type, a type alias's or const's or static's type; a lone type's root.
    pub const TY: usize = 12;
    /// Rows of `Field` (struct, union) or `Variant` (enum).
    pub const FIELDS: usize = 13;
    /// `Shape` of a struct.
    pub const SHAPE: usize = 15;
    /// Rows of `Bound`: supertraits, an associated type's or a trait alias's bounds.
    pub const BOUNDS: usize = 16;
    /// An impl's trait, a path type.
    pub const TRAIT: usize = 18;
    /// An impl's self type.
    pub const SELF_TY: usize = 19;
    /// The item's name.
    pub const NAME: usize = 20;
    /// The ABI string's range (an `extern` with none: start `u32::MAX - 1`).
    pub const ABI: usize = 21;
    /// `Vis` code and path.
    pub const VIS: usize = 23;
    /// Rows of `LocalItem` (assoc entries of a trait or impl).
    pub const ITEMS: usize = 25;
    /// A body's root expression.
    pub const ROOT: usize = 27;
    /// The row of the item's `#[repr(..)]`, if it has one.
    pub const REPR: usize = 28;
    /// 1 when the item's own `cfg` is false: nothing else is lowered.
    pub const CFG_OUT: usize = 29;
    /// The length of the item's text the record was lowered from: a view is made only with
    /// a text of this length ([`super::open`]).
    pub const ITEM_LEN: usize = 30;
    /// Item-relative offset of a fn body's opening brace. Absent for declarations.
    pub const FN_TEXT_PREFIX: usize = 31;
}

blaze_format_core::record! {
    /// A lowering's record: a slice per column, the lang items read and the head.
    /// Format 2: a block scope's row holds its `{` offset (`d`) and the expansion that wrote
    /// it (`e`, NONE for the item's own text) ([`crate::scope_origin`]).
    /// Format 3: the marks of the expansions' words (`mark_starts`, `mark_bytes`).
    /// Format 4 assigns the reserved last head word to the fn text prefix.
    #[derive(Clone, Copy, Debug)]
    pub struct LoweredRoot: 4 {
        pub langs: u64,
        pub exprs: Slice<Node>,
        pub expr_ranges: Slice<Range>,
        pub pats: Slice<Node>,
        pub pat_ranges: Slice<Range>,
        pub stmts: Slice<Node>,
        pub types: Slice<Node>,
        pub type_ranges: Slice<Range>,
        pub paths: Slice<Node>,
        pub path_ranges: Slice<Range>,
        pub rows: Slice<Node>,
        pub row_ranges: Slice<Range>,
        pub ids: Slice<u32>,
        pub names: Slice<u64>,
        pub name_ranges: Slice<Range>,
        pub scopes: Slice<Node>,
        pub consts: Slice<Range>,
        pub unhandled: Slice<Node>,
        pub diags: Slice<Node>,
        /// The read list of identifier patterns: each name asked of the world (its name
        /// index) and the [`crate::ValueName`] answered, in pattern order.
        pub value_reads: Slice<Node>,
        pub cfg: Slice<Node>,
        /// The text of the expansions lowered in place, after the item's own: a range from
        /// the item's length on is of this, from there.
        pub expanded: Slice<u8>,
        /// The read list of the expansions: each body macro call's range ...
        pub expansion_calls: Slice<Range>,
        /// ... and the key of its expansion record (blaze-expand's call key) ...
        pub expansion_keys: Slice<ContentId>,
        /// ... and where its words start in `expanded`: expansions are appended whole, in
        /// order, so the one holding an offset is the last starting at or before it.
        pub expansion_starts: Slice<u32>,
        /// The marks of the expansions' words (blaze-expand `BodyWord::mark`, drive.md "A token
        /// keeps its mark"): where each marked word starts in the body's text (past the item's
        /// length; ascending, as the words are appended) ...
        pub mark_starts: Slice<u32>,
        /// ... and its mark, `origin_column::MARK` bytes a row (`TextMark::to_bytes`).
        pub mark_bytes: Slice<u8>,
        // Written as the array itself: the record's compile-time field check (blaze-format-core
        // declared.rs) refuses the alias `Head` here ("a field type that is not its struct's
        // field's", first compile of lane CX17).
        pub head: [u32; 32],
    }
}

/// The columns while they are written: plain vectors in the lowering's allocator.
pub struct Columns<'a> {
    pub(crate) exprs: AVec<'a, Node>,
    pub(crate) expr_ranges: AVec<'a, Range>,
    pub(crate) pats: AVec<'a, Node>,
    pub(crate) pat_ranges: AVec<'a, Range>,
    pub(crate) stmts: AVec<'a, Node>,
    pub(crate) types: AVec<'a, Node>,
    pub(crate) type_ranges: AVec<'a, Range>,
    pub(crate) paths: AVec<'a, Node>,
    pub(crate) path_ranges: AVec<'a, Range>,
    pub(crate) rows: AVec<'a, Node>,
    pub(crate) row_ranges: AVec<'a, Range>,
    pub(crate) ids: AVec<'a, u32>,
    pub(crate) names: AVec<'a, u64>,
    pub(crate) name_ranges: AVec<'a, Range>,
    pub(crate) scopes: AVec<'a, Node>,
    pub(crate) consts: AVec<'a, Range>,
    pub(crate) unhandled: AVec<'a, Node>,
    pub(crate) diags: AVec<'a, Node>,
    pub(crate) value_reads: AVec<'a, Node>,
    pub(crate) cfg: AVec<'a, Node>,
    pub(crate) expanded: AVec<'a, u8>,
    pub(crate) expansion_calls: AVec<'a, Range>,
    pub(crate) expansion_keys: AVec<'a, ContentId>,
    pub(crate) expansion_starts: AVec<'a, u32>,
    pub(crate) mark_starts: AVec<'a, u32>,
    pub(crate) mark_bytes: AVec<'a, u8>,
    pub(crate) langs: u64,
    pub(crate) head: Head,
}

impl<'a> Columns<'a> {
    pub(crate) fn new_in(alloc: Alloc<'a>) -> Columns<'a> {
        Columns {
            exprs: Vec::new_in(alloc),
            expr_ranges: Vec::new_in(alloc),
            pats: Vec::new_in(alloc),
            pat_ranges: Vec::new_in(alloc),
            stmts: Vec::new_in(alloc),
            types: Vec::new_in(alloc),
            type_ranges: Vec::new_in(alloc),
            paths: Vec::new_in(alloc),
            path_ranges: Vec::new_in(alloc),
            rows: Vec::new_in(alloc),
            row_ranges: Vec::new_in(alloc),
            ids: Vec::new_in(alloc),
            names: Vec::new_in(alloc),
            name_ranges: Vec::new_in(alloc),
            scopes: Vec::new_in(alloc),
            consts: Vec::new_in(alloc),
            unhandled: Vec::new_in(alloc),
            diags: Vec::new_in(alloc),
            value_reads: Vec::new_in(alloc),
            cfg: Vec::new_in(alloc),
            expanded: Vec::new_in(alloc),
            expansion_calls: Vec::new_in(alloc),
            expansion_keys: Vec::new_in(alloc),
            expansion_starts: Vec::new_in(alloc),
            mark_starts: Vec::new_in(alloc),
            mark_bytes: Vec::new_in(alloc),
            langs: 0,
            head: [NONE; HEAD_WORDS],
        }
    }

    /// The view of these columns, lowered from `item`: only [`crate::Lowered::view`] makes
    /// one, with the text it was lowered from.
    pub(crate) fn view<'s>(&'s self, item: &'s str) -> View<'s> {
        View {
            item,
            lens: Lens::of(item.len(), self.expanded.len()),
            expansion_starts: &self.expansion_starts,
            exprs: &self.exprs,
            expr_ranges: &self.expr_ranges,
            pats: &self.pats,
            pat_ranges: &self.pat_ranges,
            stmts: &self.stmts,
            types: &self.types,
            type_ranges: &self.type_ranges,
            paths: &self.paths,
            path_ranges: &self.path_ranges,
            rows: &self.rows,
            row_ranges: &self.row_ranges,
            ids: &self.ids,
            names: &self.names,
            name_ranges: &self.name_ranges,
            scopes: &self.scopes,
            consts: &self.consts,
            unhandled: &self.unhandled,
            diags: &self.diags,
            value_reads: &self.value_reads,
            cfg: &self.cfg,
            expanded: &self.expanded,
            expansion_calls: &self.expansion_calls,
            expansion_keys: &self.expansion_keys,
            mark_starts: &self.mark_starts,
            mark_bytes: &self.mark_bytes,
            langs: self.langs,
            head: self.head,
        }
    }

    pub(crate) fn to_record<'b>(&self, alloc: Alloc<'b>) -> blaze_own::Vec<u8, Alloc<'b>> {
        let mut out = Writer::new(WriterOpts { schema: <LoweredRoot as Record>::SCHEMA, alloc: Own(alloc), capacity: None });
        let exprs = Column::push(&mut out, &self.exprs);
        let expr_ranges = Column::push(&mut out, &self.expr_ranges);
        let pats = Column::push(&mut out, &self.pats);
        let pat_ranges = Column::push(&mut out, &self.pat_ranges);
        let stmts = Column::push(&mut out, &self.stmts);
        let types = Column::push(&mut out, &self.types);
        let type_ranges = Column::push(&mut out, &self.type_ranges);
        let paths = Column::push(&mut out, &self.paths);
        let path_ranges = Column::push(&mut out, &self.path_ranges);
        let rows = Column::push(&mut out, &self.rows);
        let row_ranges = Column::push(&mut out, &self.row_ranges);
        let ids = Column::push(&mut out, &self.ids);
        let names = Column::push(&mut out, &self.names);
        let name_ranges = Column::push(&mut out, &self.name_ranges);
        let scopes = Column::push(&mut out, &self.scopes);
        let consts = Column::push(&mut out, &self.consts);
        let unhandled = Column::push(&mut out, &self.unhandled);
        let diags = Column::push(&mut out, &self.diags);
        let value_reads = Column::push(&mut out, &self.value_reads);
        let cfg = Column::push(&mut out, &self.cfg);
        let expanded = Column::push(&mut out, &self.expanded);
        let expansion_calls = Column::push(&mut out, &self.expansion_calls);
        let expansion_keys = Column::push(&mut out, &self.expansion_keys);
        let expansion_starts = Column::push(&mut out, &self.expansion_starts);
        let mark_starts = Column::push(&mut out, &self.mark_starts);
        let mark_bytes = Column::push(&mut out, &self.mark_bytes);
        let at = out.next::<LoweredRoot>();
        let root = LoweredRoot {
            langs: self.langs,
            exprs: exprs.slice(at, offset_of!(LoweredRoot, exprs)),
            expr_ranges: expr_ranges.slice(at, offset_of!(LoweredRoot, expr_ranges)),
            pats: pats.slice(at, offset_of!(LoweredRoot, pats)),
            pat_ranges: pat_ranges.slice(at, offset_of!(LoweredRoot, pat_ranges)),
            stmts: stmts.slice(at, offset_of!(LoweredRoot, stmts)),
            types: types.slice(at, offset_of!(LoweredRoot, types)),
            type_ranges: type_ranges.slice(at, offset_of!(LoweredRoot, type_ranges)),
            paths: paths.slice(at, offset_of!(LoweredRoot, paths)),
            path_ranges: path_ranges.slice(at, offset_of!(LoweredRoot, path_ranges)),
            rows: rows.slice(at, offset_of!(LoweredRoot, rows)),
            row_ranges: row_ranges.slice(at, offset_of!(LoweredRoot, row_ranges)),
            ids: ids.slice(at, offset_of!(LoweredRoot, ids)),
            names: names.slice(at, offset_of!(LoweredRoot, names)),
            name_ranges: name_ranges.slice(at, offset_of!(LoweredRoot, name_ranges)),
            scopes: scopes.slice(at, offset_of!(LoweredRoot, scopes)),
            consts: consts.slice(at, offset_of!(LoweredRoot, consts)),
            unhandled: unhandled.slice(at, offset_of!(LoweredRoot, unhandled)),
            diags: diags.slice(at, offset_of!(LoweredRoot, diags)),
            value_reads: value_reads.slice(at, offset_of!(LoweredRoot, value_reads)),
            cfg: cfg.slice(at, offset_of!(LoweredRoot, cfg)),
            expanded: expanded.slice(at, offset_of!(LoweredRoot, expanded)),
            expansion_calls: expansion_calls.slice(at, offset_of!(LoweredRoot, expansion_calls)),
            expansion_keys: expansion_keys.slice(at, offset_of!(LoweredRoot, expansion_keys)),
            expansion_starts: expansion_starts.slice(at, offset_of!(LoweredRoot, expansion_starts)),
            mark_starts: mark_starts.slice(at, offset_of!(LoweredRoot, mark_starts)),
            mark_bytes: mark_bytes.slice(at, offset_of!(LoweredRoot, mark_bytes)),
            head: self.head,
        };
        let written = out.push(&root);
        debug_assert_eq!(written, at);
        out.finish(at)
    }
}

/// Where a column was written: nowhere when it is empty.
#[derive(Clone, Copy)]
struct Column {
    at: u64,
    len: usize,
}

impl Column {
    fn push<T: Record>(out: &mut Writer<Own<Alloc<'_>>>, values: &[T]) -> Column {
        match values.is_empty() {
            true => Column { at: 0, len: 0 },
            false => Column { at: out.push_slice(values), len: values.len() },
        }
    }

    fn slice<T>(self, at: u64, offset: usize) -> Slice<T> {
        match self.len {
            0 => Slice::EMPTY,
            len => Slice::between(at + offset as u64, self.at, len),
        }
    }
}

/// Why a record could not be opened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Open {
    /// The buffer is no lowering's record.
    Format(FormatError),
    /// The item text given is not the one the record was lowered from: its length is not the
    /// recorded one. Refused here, once, so no read of the view can cut a wrong text.
    ItemText { recorded: u32, given: usize },
}

impl From<FormatError> for Open {
    fn from(e: FormatError) -> Open {
        Open::Format(e)
    }
}

/// Opens a lowering's record of the item whose text is `item`: its root verified once, each
/// column a slice of the buffer. The view holds `item`, so every read of text through it is
/// of the text the record was lowered from.
pub fn open<'b>(buffer: &'b [u8], item: &'b str) -> Result<View<'b>, Open> {
    let root = open_as::<LoweredRoot>(buffer, <LoweredRoot as Record>::SCHEMA)?;
    let recorded = root.head[head::ITEM_LEN];
    if usize::try_from(recorded).ok() != Some(item.len()) {
        return Err(Open::ItemText { recorded, given: item.len() });
    }
    let expanded: &[u8] = root.expanded.follow(buffer)?;
    Ok(View {
        item,
        lens: Lens::of(item.len(), expanded.len()),
        expansion_starts: root.expansion_starts.follow(buffer)?,
        exprs: root.exprs.follow(buffer)?,
        expr_ranges: root.expr_ranges.follow(buffer)?,
        pats: root.pats.follow(buffer)?,
        pat_ranges: root.pat_ranges.follow(buffer)?,
        stmts: root.stmts.follow(buffer)?,
        types: root.types.follow(buffer)?,
        type_ranges: root.type_ranges.follow(buffer)?,
        paths: root.paths.follow(buffer)?,
        path_ranges: root.path_ranges.follow(buffer)?,
        rows: root.rows.follow(buffer)?,
        row_ranges: root.row_ranges.follow(buffer)?,
        ids: root.ids.follow(buffer)?,
        names: root.names.follow(buffer)?,
        name_ranges: root.name_ranges.follow(buffer)?,
        scopes: root.scopes.follow(buffer)?,
        consts: root.consts.follow(buffer)?,
        unhandled: root.unhandled.follow(buffer)?,
        diags: root.diags.follow(buffer)?,
        value_reads: root.value_reads.follow(buffer)?,
        cfg: root.cfg.follow(buffer)?,
        expanded,
        expansion_calls: root.expansion_calls.follow(buffer)?,
        expansion_keys: root.expansion_keys.follow(buffer)?,
        mark_starts: root.mark_starts.follow(buffer)?,
        mark_bytes: root.mark_bytes.follow(buffer)?,
        langs: root.langs,
        head: root.head,
    })
}

/// The two lengths that make a record's ranges its own: its item's text and its expansions'.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Lens {
    item_len: u32,
    expanded_len: u32,
}

impl Lens {
    /// While a lowering builds its columns: a range made then is only encoded (its bounds),
    /// never read through a view.
    pub(crate) const BUILDING: Lens = Lens { item_len: u32::MAX, expanded_len: u32::MAX };

    fn of(item_len: usize, expanded_len: usize) -> Lens {
        // An item's text and its expansions are each under 4 GiB: the lowering's ranges are
        // `u32` pairs (asserted where it starts, `feed::run`).
        Lens { item_len: idx(item_len), expanded_len: idx(expanded_len) }
    }
}

fn idx(n: usize) -> u32 {
    u32::try_from(n).expect("an item and its expansions are under 4 GiB")
}

/// A range of a lowering's record: of the item's own text, or past its length, of the text
/// of the macro expansions lowered in place after it.
///
/// It has no way to cut text: no `of`, `get`, `Index`, bounds or conversion to a plain
/// [`Range`]. The one door is [`View::text_at`], which reads the item's text or the
/// expansions' as the range says. So a reader that slices the item's text with a range past
/// its end (the bug class this type closes: a panic, or the wrong bytes) cannot be written;
/// there is nothing to test, because the method does not exist. A position for a diagnostic
/// is [`View::site`] (item-relative, rule 71) or [`View::item_relative`].
///
/// It carries the two lengths of the view that made it, so a view recognises a range of
/// another record ([`TextAt::OtherView`]) rather than reading it as its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BodyRange {
    start: u32,
    end: u32,
    lens: Lens,
}

impl BodyRange {
    pub(crate) const fn new(r: Range, lens: Lens) -> BodyRange {
        BodyRange { start: r.start, end: r.end, lens }
    }

    /// A range the lowering makes while it builds: encoded, never read.
    pub(crate) const fn building(r: Range) -> BodyRange {
        BodyRange::new(r, Lens::BUILDING)
    }

    /// Its bounds, for the lowering that builds the text (inside this crate only).
    pub(crate) const fn raw(self) -> Range {
        Range::new(self.start, self.end)
    }

    pub const fn is_empty(self) -> bool {
        self.start >= self.end
    }

    /// Its length in bytes.
    pub const fn len(self) -> u32 {
        self.end.saturating_sub(self.start)
    }

    /// The range from the first's start to the later end of both; `None` for ranges of two
    /// records, or of the item and of an expansion (no text spans both).
    pub fn cover(self, other: BodyRange) -> Option<BodyRange> {
        let item = self.lens.item_len;
        let same_side = (self.end <= item) == (other.end <= item);
        (self.lens == other.lens && same_side).then(|| BodyRange {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
            lens: self.lens,
        })
    }

    /// Whether it is of the expansions' text, not the item's.
    pub const fn in_expansion(self) -> bool {
        self.end > self.lens.item_len
    }
}

/// Why [`View::text_at`] gave no text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextAt {
    /// The range was made by another record's view.
    OtherView(BodyRange),
    /// The range is not a text of this record (past its end, across the item's end, or off a
    /// character's boundary): a record whose bytes are wrong.
    Malformed(BodyRange),
    /// The row asked for is not in the record.
    Row(BadRow),
}

impl From<BadRow> for TextAt {
    fn from(b: BadRow) -> TextAt {
        TextAt::Row(b)
    }
}

/// A lowering read: of the columns in memory, or of a record in place, with the item's text
/// it was lowered from. Rows decode when asked; a row that is no form of its column is a
/// [`BadRow`]. Every range it gives is a [`BodyRange`], read through [`View::text_at`].
#[derive(Clone, Copy)]
pub struct View<'b> {
    item: &'b str,
    lens: Lens,
    pub exprs: &'b [Node],
    expr_ranges: &'b [Range],
    pub pats: &'b [Node],
    pat_ranges: &'b [Range],
    pub stmts: &'b [Node],
    pub types: &'b [Node],
    type_ranges: &'b [Range],
    pub paths: &'b [Node],
    path_ranges: &'b [Range],
    pub rows: &'b [Node],
    row_ranges: &'b [Range],
    pub ids: &'b [u32],
    pub names: &'b [u64],
    name_ranges: &'b [Range],
    pub scopes: &'b [Node],
    consts: &'b [Range],
    pub unhandled: &'b [Node],
    pub diags: &'b [Node],
    pub value_reads: &'b [Node],
    pub cfg: &'b [Node],
    expanded: &'b [u8],
    expansion_calls: &'b [Range],
    pub expansion_keys: &'b [ContentId],
    expansion_starts: &'b [u32],
    mark_starts: &'b [u32],
    mark_bytes: &'b [u8],
    pub langs: u64,
    pub head: Head,
}

impl<'b> View<'b> {
    /// The item's text the record was lowered from.
    pub fn item(&self) -> &'b str {
        self.item
    }

    /// The text checked by a frozen function definition ends at its body's opening brace.
    /// Declarations and const/static initializers keep their whole-text check.
    pub fn fn_text_prefix(&self, detail: blaze_surface::ItemDetail) -> Result<Option<u32>, TextAt> {
        let (blaze_surface::ItemDetail::Fn { .. }, Some(end)) = (detail, self.head_opt(head::FN_TEXT_PREFIX)) else {
            return Ok(None);
        };
        if self.item.as_bytes().get(end as usize) != Some(&b'{') {
            return Err(TextAt::Malformed(self.range(Range::new(0, end))));
        }
        Ok(Some(end))
    }

    pub(crate) fn lens(&self) -> Lens {
        self.lens
    }

    fn range(&self, r: Range) -> BodyRange {
        BodyRange::new(r, self.lens)
    }

    /// The text at `r`: the item's own, or of an expansion lowered in place. Total: a range
    /// of another record, or one this record cannot hold, is refused, never a panic.
    pub fn text_at(&self, r: BodyRange) -> Result<&'b str, TextAt> {
        if r.lens != self.lens {
            return Err(TextAt::OtherView(r));
        }
        let malformed = TextAt::Malformed(r);
        let item = self.lens.item_len;
        if r.start > r.end {
            return Err(malformed);
        }
        if r.end <= item {
            return self.item.get(r.start as usize..r.end as usize).ok_or(malformed);
        }
        let start = r.start.checked_sub(item).ok_or(malformed)? as usize;
        let bytes = self.expanded.get(start..(r.end - item) as usize).ok_or(malformed)?;
        core::str::from_utf8(bytes).map_err(|_| malformed)
    }

    /// `r` as a range of the item's text; `None` inside an expansion, which has no position
    /// of its own in the item (rule 71: see [`View::site`]).
    pub fn item_relative(&self, r: BodyRange) -> Option<Range> {
        (r.lens == self.lens && !r.in_expansion()).then(|| r.raw())
    }

    /// Where `r` is in the item, for a diagnostic: `r` itself, or the range of the outermost
    /// macro call whose expansion holds it. Item-relative (rule 71): a file position exists
    /// only when printed.
    pub fn site(&self, r: BodyRange) -> Result<Range, TextAt> {
        if r.lens != self.lens {
            return Err(TextAt::OtherView(r));
        }
        let item = self.lens.item_len;
        let at = r.raw();
        // A node of the item's text that ends with a macro call (`x.map_err(|e| format!(..))`)
        // ends where the call's expansion ends: its site runs to the outermost call's end.
        if at.start < item && at.end > item {
            let tail = self.outermost(Range::new(at.end - 1, at.end), r)?;
            return Ok(Range::new(at.start, tail.end.max(at.start)));
        }
        self.outermost(at, r)
    }

    /// [`View::site`] of a body macro call's finding range (blaze-lower by_name.rs
    /// `MacroFinding::at`: a range of this record's text, past the item's length when the call is
    /// in an expansion lowered after it). The one door from a finding's plain range to its place.
    pub fn call_site(&self, at: Range) -> Result<Range, TextAt> {
        self.site(BodyRange::new(at, self.lens))
    }

    /// `at`, or the range of the outermost macro call whose expansion holds it.
    fn outermost(&self, mut at: Range, r: BodyRange) -> Result<Range, TextAt> {
        let item = self.lens.item_len;
        // Each step goes to a call of an earlier expansion (a call is in the text before the
        // expansion it makes), so this ends within one step per expansion.
        for _ in 0..=self.expansion_calls.len() {
            if at.end <= item {
                return Ok(at);
            }
            let offset = at.start.checked_sub(item).ok_or(TextAt::Malformed(r))?;
            let held = self.expansion_starts.partition_point(|s| *s <= offset);
            let call = held.checked_sub(1).and_then(|j| self.expansion_calls.get(j)).ok_or(TextAt::Malformed(r))?;
            at = *call;
        }
        Err(TextAt::Malformed(r))
    }

    /// The text of the expansions lowered in place, after the item's own.
    pub(crate) fn expanded(&self) -> &'b [u8] {
        self.expanded
    }

    /// The read list of the expansions: each body macro call's range, beside
    /// [`View::expansion_keys`].
    pub fn expansion_calls(&self) -> impl Iterator<Item = BodyRange> + 'b {
        let lens = self.lens;
        self.expansion_calls.iter().map(move |r| BodyRange::new(*r, lens))
    }

    /// The anonymous constants' ranges, in order.
    pub fn consts(&self) -> impl Iterator<Item = BodyRange> + 'b {
        let lens = self.lens;
        self.consts.iter().map(move |r| BodyRange::new(*r, lens))
    }

    pub(crate) fn raw_consts(&self) -> &'b [Range] {
        self.consts
    }

    /// The ABI string's range of an `extern "abi"` item; `None` with no `extern` or `extern`
    /// alone.
    pub fn abi(&self) -> Option<BodyRange> {
        match self.head[head::ABI] {
            NONE => None,
            start if start == NONE - 1 => None,
            start => Some(self.range(Range::new(start, self.head[head::ABI + 1]))),
        }
    }

    fn ranged(&self, column: &'static str, all: &[Range], i: u32) -> Result<BodyRange, BadRow> {
        Ok(self.range(at(column, all, i)?))
    }

    pub fn expr_range(&self, e: ExprId) -> Result<BodyRange, BadRow> {
        self.ranged("expr_ranges", self.expr_ranges, e.0)
    }

    pub fn pat_range(&self, p: PatId) -> Result<BodyRange, BadRow> {
        self.ranged("pat_ranges", self.pat_ranges, p.0)
    }

    pub fn type_range(&self, t: TypeId) -> Result<BodyRange, BadRow> {
        self.ranged("type_ranges", self.type_ranges, t.0)
    }

    pub fn path_range(&self, p: PathIdx) -> Result<BodyRange, BadRow> {
        self.ranged("path_ranges", self.path_ranges, p.0)
    }

    /// The mark of the word the path `p` starts at (blaze-expand `BodyWord::mark`; drive.md,
    /// "A token keeps its mark"): `None` for a path of the item's own text or one whose first
    /// word no expansion marked. Where the path resolves is `crate::mark_site::site_of` of
    /// it. A mark row whose bytes are no mark is a [`BadRow`], never `None`.
    pub fn mark_of_path(&self, p: PathIdx) -> Result<Option<crate::mark_site::TextMark>, BadRow> {
        use blaze_expand::origin_column::MARK;
        let start = at("path_ranges", self.path_ranges, p.0)?.start;
        let Ok(i) = self.mark_starts.binary_search(&start) else { return Ok(None) };
        let bad = BadRow { column: "mark_bytes", tag: u16::MAX };
        let bytes = self.mark_bytes.get(i * MARK..(i + 1) * MARK).ok_or(bad)?;
        crate::mark_site::TextMark::from_bytes(bytes).map(Some).ok_or(bad)
    }

    /// The mark of the word the expression `e` starts at, as [`View::mark_of_path`] for a
    /// path: `None` for an expression of the item's own text or one whose first word no
    /// expansion marked (a token as written at the call). A block's is the mark of its `{` (or
    /// of the `unsafe`, `async`, `const` or label before it, written by the same macro), so the
    /// edition its tail's temporaries follow (rustc region.rs `resolve_block`:
    /// `blk.span.edition()`).
    pub fn mark_of_expr(&self, e: ExprId) -> Result<Option<crate::mark_site::TextMark>, BadRow> {
        self.mark_at(at("expr_ranges", self.expr_ranges, e.0)?.start)
    }

    /// The mark of the word the pattern `p` starts at, as [`View::mark_of_expr`]: the edition
    /// its binding modes follow (rustc's pattern migration errors by the span of the `mut`,
    /// `ref` or `&` they name).
    pub fn mark_of_pat(&self, p: PatId) -> Result<Option<crate::mark_site::TextMark>, BadRow> {
        self.mark_at(at("pat_ranges", self.pat_ranges, p.0)?.start)
    }

    /// The mark of the word starting at `start`, if a word starts there and an expansion
    /// marked it.
    fn mark_at(&self, start: u32) -> Result<Option<crate::mark_site::TextMark>, BadRow> {
        use blaze_expand::origin_column::MARK;
        let Ok(i) = self.mark_starts.binary_search(&start) else { return Ok(None) };
        let bad = BadRow { column: "mark_bytes", tag: u16::MAX };
        let bytes = self.mark_bytes.get(i * MARK..(i + 1) * MARK).ok_or(bad)?;
        crate::mark_site::TextMark::from_bytes(bytes).map(Some).ok_or(bad)
    }

    /// Each marked word that starts inside `r` (a range of this view): its start relative to
    /// `r`'s start, so a place in [`View::text_at`] of `r`, and its mark, ascending. What a copy
    /// of an expansion's text out of the record keeps its marks by (blaze-ladder front2
    /// locals.rs: a local item a call in the body wrote; [`crate::mark_site::column_of_copy`]).
    /// A range of another view is `TextAt::OtherView`; a mark row whose bytes are no mark a
    /// `TextAt::Row`.
    pub fn marks_in<'x>(&self, r: BodyRange, alloc: Alloc<'x>) -> Result<AVec<'x, (u32, crate::mark_site::TextMark)>, TextAt> {
        use blaze_expand::origin_column::MARK;
        if r.lens != self.lens {
            return Err(TextAt::OtherView(r));
        }
        let raw = r.raw();
        let first = self.mark_starts.partition_point(|&s| s < raw.start);
        let past = self.mark_starts.partition_point(|&s| s < raw.end);
        let mut out = AVec::with_capacity_in(past.saturating_sub(first), alloc);
        for i in first..past {
            let bad = TextAt::Row(BadRow { column: "mark_bytes", tag: u16::MAX });
            let bytes = self.mark_bytes.get(i * MARK..(i + 1) * MARK).ok_or(bad)?;
            let mark = crate::mark_site::TextMark::from_bytes(bytes).ok_or(bad)?;
            out.push((self.mark_starts[i] - raw.start, mark));
        }
        Ok(out)
    }

    /// The range of the row at `i` of the `rows` column (a list's `start + k`).
    pub fn row_range(&self, i: u32) -> Result<BodyRange, BadRow> {
        self.ranged("row_ranges", self.row_ranges, i)
    }

    pub fn name_range(&self, n: NameIdx) -> Result<BodyRange, BadRow> {
        self.ranged("name_ranges", self.name_ranges, n.0)
    }

    /// A name as written (`r#` kept).
    pub fn name_text(&self, n: NameIdx) -> Result<&'b str, TextAt> {
        self.text_at(self.name_range(n)?)
    }

    pub fn attr(&self, n: Node) -> Result<Attr, BadRow> {
        Attr::decode(n, self.lens)
    }

    pub fn macro_call(&self, n: Node) -> Result<MacroCall, BadRow> {
        MacroCall::decode(n, self.lens)
    }

    pub fn format_args(&self, n: Node) -> Result<FormatArgs, BadRow> {
        FormatArgs::decode(n, self.lens)
    }

    pub fn fmt_piece(&self, n: Node) -> Result<FmtPiece, BadRow> {
        FmtPiece::decode(n, self.lens)
    }

    pub fn local_item(&self, n: Node) -> Result<LocalItem, BadRow> {
        LocalItem::decode(n, self.lens)
    }
}

fn at<T: Copy>(column: &'static str, all: &[T], i: u32) -> Result<T, BadRow> {
    all.get(i as usize).copied().ok_or(BadRow { column, tag: u16::MAX })
}

fn slice<'b, T>(column: &'static str, all: &'b [T], l: List) -> Result<&'b [T], BadRow> {
    let start = l.start as usize;
    all.get(start..start + l.len as usize).ok_or(BadRow { column, tag: u16::MAX })
}

impl<'b> View<'b> {
    pub fn expr(&self, e: ExprId) -> Result<Expr, BadRow> {
        Expr::decode(at("exprs", self.exprs, e.0)?)
    }

    pub fn pat(&self, p: PatId) -> Result<Pat, BadRow> {
        Pat::decode(at("pats", self.pats, p.0)?)
    }

    pub fn stmt(&self, s: StmtId) -> Result<Stmt, BadRow> {
        Stmt::decode(at("stmts", self.stmts, s.0)?)
    }

    pub fn ty(&self, t: TypeId) -> Result<TypeRef, BadRow> {
        TypeRef::decode(at("types", self.types, t.0)?, self.lens)
    }

    pub fn path(&self, p: PathIdx) -> Result<PathRef, BadRow> {
        PathRef::decode(at("paths", self.paths, p.0)?)
    }

    pub fn row(&self, r: RowIdx) -> Result<Node, BadRow> {
        at("rows", self.rows, r.0)
    }

    pub fn rows(&self, l: List) -> Result<&'b [Node], BadRow> {
        slice("rows", self.rows, l)
    }

    pub fn ids(&self, l: List) -> Result<&'b [u32], BadRow> {
        slice("ids", self.ids, l)
    }

    pub fn segments(&self, p: PathRef) -> Result<&'b [Node], BadRow> {
        self.rows(p.segments)
    }

    /// A name's hash, blaze-typeck's `Name`.
    pub fn name(&self, n: NameIdx) -> Result<u64, BadRow> {
        at("names", self.names, n.0)
    }

    pub fn const_range(&self, c: ConstIdx) -> Result<BodyRange, BadRow> {
        self.ranged("consts", self.consts, c.0)
    }

    pub fn scope(&self, s: ScopeId) -> Result<Scope, BadRow> {
        Scope::decode(at("scopes", self.scopes, s.0)?)
    }

    /// Which text wrote the block scope `s`, and where its `{` is in it
    /// ([`crate::scope_origin`]).
    pub fn scope_origin(&self, s: ScopeId) -> Result<crate::scope_origin::ScopeOrigin, BadRow> {
        Ok(crate::scope_origin::ScopeOrigin::read(&at("scopes", self.scopes, s.0)?))
    }

    /// The text of expansion `j` alone: its words as appended, with the spaces between them.
    /// An offset of a [`crate::scope_origin::ScopeOrigin::Expansion`] or a
    /// [`crate::scope_origin::CallAt::Expansion`] is of this text.
    pub fn expansion_text(&self, j: u32) -> Result<&'b str, TextAt> {
        let bad = TextAt::Row(BadRow { column: "expansion_starts", tag: u16::MAX });
        let start = *self.expansion_starts.get(j as usize).ok_or(bad)? as usize;
        let end = self.expansion_starts.get(j as usize + 1).map_or(self.expanded.len(), |e| *e as usize);
        let bytes = self.expanded.get(start..end).ok_or(bad)?;
        core::str::from_utf8(bytes).map_err(|_| bad)
    }

    /// Where the call that made expansion `j` stands: in the item's own text, or in the text of
    /// the earlier expansion that wrote it. Expansions are appended whole and in order, so the
    /// text holding a byte past the item's is the last expansion starting at or before it:
    /// that is which appended text the call's bytes ARE, not a guess among calls. A call is
    /// read before the expansion it makes, so its writer's index is below `j`.
    pub fn expansion_call_at(&self, j: u32) -> Result<crate::scope_origin::CallAt, TextAt> {
        use crate::scope_origin::CallAt;
        let bad = TextAt::Row(BadRow { column: "expansion_calls", tag: u16::MAX });
        let call = *self.expansion_calls.get(j as usize).ok_or(bad)?;
        let item = self.lens.item_len;
        if call.end <= item {
            return Ok(CallAt::Item { start: call.start });
        }
        let offset = call.start.checked_sub(item).ok_or(bad)?;
        let held = self.expansion_starts.partition_point(|s| *s <= offset);
        let writer = held.checked_sub(1).filter(|w| *w < j as usize).ok_or(bad)?;
        Ok(CallAt::Expansion { expansion: writer as u32, start: offset - self.expansion_starts[writer] })
    }

    pub fn unhandled(&self) -> impl Iterator<Item = Unhandled> + 'b {
        let lens = self.lens;
        self.unhandled.iter().map(move |n| Unhandled::decode(*n, lens))
    }

    pub fn diags(&self) -> impl Iterator<Item = Result<Diag, BadRow>> + 'b {
        let lens = self.lens;
        self.diags.iter().map(move |n| Diag::decode(*n, lens))
    }

    /// The identifier patterns' read list: each name asked of the world and its answer
    /// ([`crate::value_reads_hold`] asks them again).
    pub fn value_reads(&self) -> impl Iterator<Item = Result<(NameIdx, crate::ValueName), BadRow>> + 'b {
        // A row flagged `block_uses::BY_USES` is a question asked through a block's `use`s,
        // not the module's answer: `block_uses::reads_hold` reads those.
        self.value_reads.iter().filter(|n| n.flags & crate::block_uses::BY_USES == 0).map(|n| match crate::ValueName::of_code(n.tag) {
            Some(answer) => Ok((NameIdx(n.a), answer)),
            None => Err(BadRow { column: "value_reads", tag: n.tag }),
        })
    }

    pub fn cfg_reads(&self) -> impl Iterator<Item = CfgRead> + 'b {
        self.cfg.iter().map(|n| CfgRead::decode(*n))
    }

    /// The lang items the body's desugarings name.
    pub fn langs(&self) -> impl Iterator<Item = LangItem> + 'b {
        let set = self.langs;
        LangItem::ALL.into_iter().filter(move |l| set & l.bit() != 0)
    }

    pub fn list(&self, word: usize) -> List {
        List { start: self.head[word], len: self.head[word + 1] }
    }

    pub fn head_opt(&self, word: usize) -> Option<u32> {
        unopt(self.head[word])
    }

    /// A body's root expression and its parameters' patterns.
    pub fn body(&self) -> Option<(ExprId, &'b [u32])> {
        let root = self.head_opt(head::ROOT)?;
        let params = self.ids(self.list(head::PARAMS)).unwrap_or(&[]);
        Some((ExprId(root), params))
    }

    /// A lone type's root ([`crate::lower_type_text`]); `None` for any other record.
    pub fn type_root(&self) -> Option<TypeId> {
        if self.head[head::MODE] != 3 {
            return None;
        }
        self.head_opt(head::TY).map(TypeId)
    }

    /// The item's visibility as written (`head::VIS`: a signature's; `Private` when none is
    /// written, and for a record with no item).
    pub fn vis(&self) -> Vis {
        Vis::decode(self.head[head::VIS] as u16, self.head[head::VIS + 1])
    }

    /// Whether the item's own `cfg` turned it off.
    pub fn cfg_out(&self) -> bool {
        self.head[head::CFG_OUT] == 1
    }
}
