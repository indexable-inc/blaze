//! The origin column of an expansion's TEXT (drive.md, "A finding inside an expansion is
//! reported where its tokens came from", ruled 20:57).
//!
//! An expansion's rendered text keeps, for each token it wrote, the byte range the token has in
//! that text and where the token came from: the n-th token of the call's text as written, a
//! token of the macro's definition, or made by the macro ([`TextOrigin`]). Input tokens are kept
//! by INDEX, never by a position in the calling text (rule 71): the reader lexes the call's
//! input as it is today ([`input_places`]) and maps an index to its bytes there, so a call whose
//! input moved, or whose whitespace changed under one key, maps right (rule 21: no position of
//! the calling text is held under the call's key).
//!
//! The task that MAKES a finding in expanded text maps its range with [`map_range`] before
//! filing it: when every token of the range is an input token, the finding is filed at those
//! tokens' bytes in the user's text; otherwise at the call, naming the macro as written. A nested
//! expansion maps through each level in turn (the driver is the caller's loop: front2
//! expansion_origin.rs).
//!
//! The same column carries each token's MARK (drive.md, "A token keeps its mark", ruled 21:27:
//! one column for both): the innermost mark of the token's context ([`TextMark`]), `None` for a
//! token as written at the call. The const-intrinsics piece resolves a path by its first
//! segment's mark through it.
//!
//! Form (little-endian, read in place, checked once by [`OriginsRef::open`]): a row count u32, a
//! mark count u32, then the marks ([`MARK`] bytes each, sorted by their bytes and each once: the
//! output is a function of the marks, not of their order), then the rows ([`ROW`] bytes each,
//! sorted by start and not overlapping): the token's start and end in the expansion's text (u32
//! each), its origin's tag (u32) and value (u32), and its mark's index in the table (u32,
//! [`NO_MARK`] for none). Tags: 0 input, 1 attribute argument, 2 definition, 3 a
//! `macro m(params)`'s parameter, 4 made, 5 an input token that is not as written (an item the
//! attribute walk rewrote, `cfg_attr`). A mark: the definition's crate key (32), a module flag
//! u32 (0 none, 1 one) and the module's key (32, zero for none), the transparency's byte as a u32
//! and the edition's byte as a u32. Sans-IO, no recursion; everything allocated is in the
//! caller's `alloc`.

use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_own::{AVec, Alloc};

use crate::error::LexError;
use crate::{DefSite, Edition, TokenOrigin, TokenStream, Transparency};

/// One row's bytes.
pub const ROW: usize = 20;
/// One mark's bytes.
pub const MARK: usize = 32 + 4 + 32 + 4 + 4;
/// A row's mark index for a token with no mark.
pub const NO_MARK: u32 = u32::MAX;
/// The column's head: its row count and mark count.
const HEAD: usize = 8;

/// A token's mark as the column keeps it: the innermost mark of its context (blaze-expand
/// `TokenStream::hygiene`): where the macro is defined, how it resolves names, and the edition
/// the token is read in.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct TextMark {
    pub site: DefSite,
    pub transparency: Transparency,
    pub edition: Edition,
}

impl TextMark {
    /// The column's form of an expansion's mark.
    pub fn of(mark: crate::tokens::Mark) -> TextMark {
        TextMark { site: mark.site, transparency: mark.transparency, edition: mark.edition }
    }

    /// The stream mark a token of a text keeps when the text is lexed again
    /// ([`crate::tokens::lex_marked`]): this mark's site and edition, of an earlier expansion
    /// ([`crate::MarkOf::Outer`], never `This`: a call's input holds no mark of the call's own,
    /// `ExpandError::Unsealed`), named by a key hashed from the mark's bytes (the column does
    /// not keep which expansion wrote the token), and TRANSPARENT.
    ///
    /// Why transparent: a word keeps through the round trip the syntax context the text's own
    /// lowering gives it. That lowering keys a local or a label of the item's text by its name
    /// alone (blaze-lower machine.rs `hygiene_at` is 0 for every word of the item's text), so a
    /// binder the expansion wrote there (`let mut config = ..;`, proptest's `proptest!`) and its
    /// use inside an inner call's input (`$crate::proptest_helper!(.. config ..)`) must have equal
    /// [`crate::local_hygiene`]: 0, which only a transparent mark gives (the Reference, "Macros
    /// By Example", "Hygiene": locals resolve at the definition site's context, and both tokens
    /// are of one expansion). A semi-opaque mark here made the use a different local and every
    /// such body E0425 (tempo a1630: 590 errors in `proptest!`, `hardfork!`, `paste!`,
    /// `bitflags!` bodies). What the mark still gives is all a non-opaque mark ever gave a path:
    /// `$crate` (its site's crate, blaze-scope crate_roots.rs), the edition, and a path read at
    /// the call. A token the INNER macro writes gets that call's own mark over it, so the inner
    /// macro's locals stay hidden from the outer's tokens.
    ///
    /// GAP (named): an opaque (`macro`) mark is made transparent too, so a path of an item text a
    /// `macro` wrote, handed through an inner call's input, is read at the call and not at the
    /// `macro`'s module (as before marks were kept on inputs). Keeping it needs the item text's
    /// own locals keyed by their marks (`hygiene_at` from the mark column), which is the end
    /// state of item-text hygiene: one rule for both sides.
    pub fn as_outer_mark(self) -> crate::tokens::Mark {
        let mut hasher = blaze_ids::Hasher::new("blaze v2 text mark expansion 1");
        hasher.part(&self.to_bytes());
        crate::tokens::Mark {
            of: crate::tokens::MarkOf::Outer(hasher.finish()),
            transparency: crate::Transparency::Transparent,
            edition: self.edition,
            site: self.site,
        }
    }

    /// The mark's [`MARK`] bytes, little-endian: the crate key (32), the module flag u32 (0 none,
    /// 1 some), the module key (32, zero for none), the transparency byte as u32, the edition
    /// byte as u32. One mark has one form: the column's table and the lowered body's marks
    /// (blaze-lower) both keep these bytes.
    pub fn to_bytes(self) -> [u8; MARK] {
        let mut out = [0u8; MARK];
        out[..32].copy_from_slice(&self.site.krate.0.0);
        if let Some(module) = self.site.module {
            out[32..36].copy_from_slice(&1u32.to_le_bytes());
            out[36..68].copy_from_slice(&module.0.0);
        }
        out[68..72].copy_from_slice(&u32::from(self.transparency.byte()).to_le_bytes());
        out[72..76].copy_from_slice(&u32::from(self.edition.byte()).to_le_bytes());
        out
    }

    /// The mark [`to_bytes`](TextMark::to_bytes) wrote, or `None` for bytes no mark writes: a
    /// length other than [`MARK`], a flag other than 0 or 1, a nonzero module key under flag 0,
    /// or a transparency or edition no byte names.
    pub fn from_bytes(bytes: &[u8]) -> Option<TextMark> {
        if bytes.len() != MARK {
            return None;
        }
        let key = |at: usize| {
            let mut k = [0u8; 32];
            k.copy_from_slice(&bytes[at..at + 32]);
            ContentId(k)
        };
        let module = match u32_at(bytes, 32) {
            0 if bytes[36..68].iter().all(|&b| b == 0) => None,
            1 => Some(ItemKey(key(36))),
            _ => return None,
        };
        let transparency = Transparency::from_byte(u8::try_from(u32_at(bytes, 68)).ok()?)?;
        let edition = Edition::from_byte(u8::try_from(u32_at(bytes, 72)).ok()?)?;
        Some(TextMark { site: DefSite { krate: CrateItemKey(key(0)), module }, transparency, edition })
    }
}

/// Where a token of an expansion's text came from, in the call's text as written.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum TextOrigin {
    /// The token at this index of the call's input as written, lexed by [`input_places`] in the
    /// call's [`InputShape`].
    Input(u32),
    /// Of the attribute's arguments.
    AttrArgs(u32),
    /// Of the macro's definition.
    Def(u32),
    /// Of a `macro m(params)` definition's parameters.
    Params(u32),
    /// Made by the macro.
    Made,
    /// A token of the macro's input that is not a token of the call as written: the attribute
    /// walk rewrote it (a `cfg_attr` applied, an attribute taken off).
    InputRewritten,
}

impl TextOrigin {
    fn tag(self) -> (u32, u32) {
        match self {
            TextOrigin::Input(at) => (0, at),
            TextOrigin::AttrArgs(at) => (1, at),
            TextOrigin::Def(at) => (2, at),
            TextOrigin::Params(at) => (3, at),
            TextOrigin::Made => (4, 0),
            TextOrigin::InputRewritten => (5, 0),
        }
    }

    fn of_tag(tag: u32, value: u32) -> Option<TextOrigin> {
        Some(match tag {
            0 => TextOrigin::Input(value),
            1 => TextOrigin::AttrArgs(value),
            2 => TextOrigin::Def(value),
            3 => TextOrigin::Params(value),
            4 => TextOrigin::Made,
            5 => TextOrigin::InputRewritten,
            _ => return None,
        })
    }
}

/// How the macro's input indexes relate to the call's text as written.
#[derive(Clone, Copy, Debug)]
pub enum InputIndex<'m> {
    /// The macro's input IS the call's input as written (a function-like call:
    /// [`crate::lex_inside`] of the text between its delimiters).
    AsWritten,
    /// The macro's input is the item the attribute walk made of the call's text: entry `i` is
    /// the index, in the call's text as written, of the macro's input token `i` ([`align_suffix`]),
    /// `None` for one the walk rewrote.
    Aligned(&'m [Option<u32>]),
}

/// A token's origin as the column keeps it: the expander's [`TokenOrigin`] with an input index
/// carried into the call's text as written.
pub fn text_origin(origin: TokenOrigin, input: InputIndex<'_>) -> TextOrigin {
    match origin {
        TokenOrigin::Input(at) => match input {
            InputIndex::AsWritten => TextOrigin::Input(at),
            InputIndex::Aligned(map) => match map.get(at as usize).copied().flatten() {
                Some(written) => TextOrigin::Input(written),
                None => TextOrigin::InputRewritten,
            },
        },
        TokenOrigin::AttrArgs(at) => TextOrigin::AttrArgs(at),
        TokenOrigin::Def(at) => TextOrigin::Def(at),
        TokenOrigin::Params(at) => TextOrigin::Params(at),
        TokenOrigin::Made(_) => TextOrigin::Made,
    }
}

/// The macro's input tokens matched to the call's text as written, from the END: an attribute
/// walk only takes off or rewrites outer attributes, which stand before the item, so the
/// item's tokens from its last one back are the written text's until the first that differs
/// (kind and text). Entry `i` is `Some(j)`: the macro's input token `i` is the written token
/// `j`; `None`: rewritten. A loop, no recursion.
pub fn align_suffix<'a>(item: &TokenStream<'_>, written: &TokenStream<'_>, alloc: Alloc<'a>) -> AVec<'a, Option<u32>> {
    let (a, b) = (item.tokens(), written.tokens());
    let mut out: AVec<'a, Option<u32>> = AVec::new_in(alloc);
    out.resize(a.len(), None);
    let (mut i, mut j) = (a.len(), b.len());
    while i > 0 && j > 0 {
        let (x, y) = (&a[i - 1], &b[j - 1]);
        if x.kind != y.kind || item.text_of(x) != written.text_of(y) {
            break;
        }
        // Fits: a stream's length is a u32 (its tables are u32-indexed, `StreamError::TooLarge`).
        out[i - 1] = Some((j - 1) as u32);
        i -= 1;
        j -= 1;
    }
    out
}

/// One row of the column.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct OriginRow {
    /// The token's bytes in the expansion's text.
    pub start: u32,
    pub end: u32,
    pub origin: TextOrigin,
    /// The innermost mark of the token's context; `None`: as written at the call.
    pub mark: Option<TextMark>,
}

/// Why a column could not be made or read (rule: blaze-expand origin_column.rs).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ColumnError {
    /// Row `row` (`start..end`) starts before row `row - 1` (`previous_start..previous_end`)
    /// ends, or ends before it starts: the writer's bug, named at the write (and at
    /// [`OriginsRef::open`] for bytes no builder made). Tokens that share one place of a text
    /// as written are one row ([`OriginColumn::input_identity`]).
    Unsorted { row: u32, start: u32, end: u32, previous_start: u32, previous_end: u32 },
    /// A place past what a u32 holds.
    TooLarge { at: usize },
    /// The bytes are shorter than the count says.
    Short { len: usize, want: usize },
    /// A row's tag is no origin.
    Tag { row: u32, tag: u32 },
    /// A mark of the table does not read (a module flag, transparency or edition of no value),
    /// or the table is not sorted and each once.
    Mark { mark: u32 },
    /// A row names a mark past the table.
    MarkIndex { row: u32, index: u32 },
}

impl core::fmt::Display for ColumnError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ColumnError::Unsorted { row, start, end, previous_start, previous_end } => write!(
                f,
                "an expansion's origin row {row} ({start}..{end}) is out of order after row {} ({previous_start}..{previous_end}): rows are sorted, apart and each place once (rule: blaze-expand origin_column.rs)",
                row.saturating_sub(1)
            ),
            ColumnError::TooLarge { at } => write!(f, "an expansion's text place {at} does not fit a u32 (rule: blaze-expand origin_column.rs)"),
            ColumnError::Short { len, want } => {
                write!(f, "an expansion's origin column of {len} bytes is shorter than the {want} its count says (rule: blaze-expand origin_column.rs)")
            }
            ColumnError::Tag { row, tag } => write!(f, "an expansion's origin row {row} has tag {tag}, no origin (rule: blaze-expand origin_column.rs)"),
            ColumnError::Mark { mark } => write!(f, "an expansion's mark {mark} does not read, or the marks are not sorted each once (rule: blaze-expand origin_column.rs)"),
            ColumnError::MarkIndex { row, index } => write!(f, "an expansion's origin row {row} names mark {index}, past the table (rule: blaze-expand origin_column.rs)"),
        }
    }
}

fn place32(at: usize) -> Result<u32, ColumnError> {
    u32::try_from(at).map_err(|_| ColumnError::TooLarge { at })
}

/// The column, as its writer builds it.
#[derive(Clone, Debug)]
pub struct OriginColumn<'a> {
    rows: AVec<'a, OriginRow>,
}

impl<'a> OriginColumn<'a> {
    pub fn new_in(alloc: Alloc<'a>) -> OriginColumn<'a> {
        OriginColumn { rows: AVec::new_in(alloc) }
    }

    /// Adds the next token's row: `start..end` in the text, at or after the last row's end (so
    /// a nonempty place is never kept twice).
    pub fn push(&mut self, start: usize, end: usize, origin: TextOrigin, mark: Option<TextMark>) -> Result<(), ColumnError> {
        let (start, end) = (place32(start)?, place32(end)?);
        let (previous_start, previous_end) = self.rows.last().map_or((0, 0), |r| (r.start, r.end));
        if start < previous_end || end < start {
            return Err(ColumnError::Unsorted { row: place32(self.rows.len())?, start, end, previous_start, previous_end });
        }
        self.rows.push(OriginRow { start, end, origin, mark });
        Ok(())
    }

    /// The identity column of a text that IS a call's input as written: `places` are its tokens'
    /// bytes ([`input_places`], token `i` at entry `i`), every row `TextOrigin::Input` of its
    /// token with no mark. The lexer gives several tokens ONE place (blaze-expand tokens.rs
    /// `place_new`): a doc comment's `# [ doc = ".." ]` stand at the comment's bytes, a
    /// lifetime's `'` and name at the lifetime's, the groups one closer closes at that closer,
    /// the groups a text leaves open at its end. Such a run is one row, the run's FIRST token's
    /// (a range mapped through it, [`map_range`], names that token, whose place is the run's:
    /// the same bytes). So the column is sorted and each place once by construction, for any
    /// text the lexer reads (tempo census g051231: 70 rows "row 1 (0..39) is out of order after
    /// one ending at 39", a kept item `/// doc` + `#[derive(..)] #[serde(..)] enum CallKind`).
    pub fn input_identity(places: &[(u32, u32)], alloc: Alloc<'a>) -> Result<OriginColumn<'a>, ColumnError> {
        let mut column = OriginColumn::new_in(alloc);
        let mut previous: Option<(u32, u32)> = None;
        for (i, &(start, end)) in places.iter().enumerate() {
            if previous == Some((start, end)) {
                continue;
            }
            previous = Some((start, end));
            column.push(start as usize, end as usize, TextOrigin::Input(place32(i)?), None)?;
        }
        Ok(column)
    }

    pub fn rows(&self) -> &[OriginRow] {
        &self.rows
    }

    /// The column's bytes ([`OriginsRef::open`] reads them), appended to `out`. The mark table is
    /// built in `out`'s allocator, sorted by bytes, each once.
    pub fn write(&self, out: &mut AVec<'_, u8>) -> Result<(), ColumnError> {
        let alloc = self.rows.allocator().clone();
        let mut marks: AVec<'_, [u8; MARK]> = AVec::new_in(alloc);
        marks.extend(self.rows.iter().filter_map(|r| r.mark.map(TextMark::to_bytes)));
        marks.sort_unstable();
        marks.dedup();
        out.extend_from_slice(&place32(self.rows.len())?.to_le_bytes());
        out.extend_from_slice(&place32(marks.len())?.to_le_bytes());
        for mark in marks.iter() {
            out.extend_from_slice(mark);
        }
        for row in self.rows.iter() {
            let (tag, value) = row.origin.tag();
            let index = match row.mark {
                // Every row's mark was put in the table above: a miss is this writer's bug, named.
                Some(mark) => place32(marks.binary_search(&mark.to_bytes()).map_err(|_| ColumnError::Mark { mark: NO_MARK })?)?,
                None => NO_MARK,
            };
            for word in [row.start, row.end, tag, value, index] {
                out.extend_from_slice(&word.to_le_bytes());
            }
        }
        Ok(())
    }
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    let mut word = [0u8; 4];
    word.copy_from_slice(&bytes[at..at + 4]);
    u32::from_le_bytes(word)
}

/// A column read in place, its length checked once.
#[derive(Clone, Copy, Debug)]
pub struct OriginsRef<'s> {
    marks: &'s [u8],
    rows: &'s [u8],
}

impl<'s> OriginsRef<'s> {
    /// The column at the start of `bytes`, and the bytes after it.
    pub fn open(bytes: &'s [u8]) -> Result<(OriginsRef<'s>, &'s [u8]), ColumnError> {
        if bytes.len() < HEAD {
            return Err(ColumnError::Short { len: bytes.len(), want: HEAD });
        }
        let (count, mark_count) = (u32_at(bytes, 0) as usize, u32_at(bytes, 4) as usize);
        let short = ColumnError::Short { len: bytes.len(), want: usize::MAX };
        let marks_end = mark_count.checked_mul(MARK).and_then(|n| n.checked_add(HEAD)).ok_or(short)?;
        let want = count.checked_mul(ROW).and_then(|n| n.checked_add(marks_end)).ok_or(short)?;
        if bytes.len() < want {
            return Err(ColumnError::Short { len: bytes.len(), want });
        }
        let column = OriginsRef { marks: &bytes[HEAD..marks_end], rows: &bytes[marks_end..want] };
        // Every mark read once here, and the table sorted, each once (rule 25).
        for m in 0..mark_count {
            let at = m * MARK;
            let sorted = m == 0 || column.marks[at - MARK..at] < column.marks[at..at + MARK];
            if !sorted || TextMark::from_bytes(&column.marks[at..at + MARK]).is_none() {
                return Err(ColumnError::Mark { mark: place32(m)? });
            }
        }
        // Every tag and mark index read once here, so `row` never meets a bad one; and the rows
        // sorted and apart, as [`OriginColumn::push`] made them ([`map_range`] and
        // `crate_roots::remap` search them by place).
        let (mut previous_start, mut previous_end) = (0u32, 0u32);
        for i in 0..column.len() {
            let (start, end) = (u32_at(column.rows, i * ROW), u32_at(column.rows, i * ROW + 4));
            if start < previous_end || end < start {
                return Err(ColumnError::Unsorted { row: place32(i)?, start, end, previous_start, previous_end });
            }
            (previous_start, previous_end) = (start, end);
            let tag = u32_at(column.rows, i * ROW + 8);
            if TextOrigin::of_tag(tag, 0).is_none() {
                return Err(ColumnError::Tag { row: place32(i)?, tag });
            }
            let index = u32_at(column.rows, i * ROW + 16);
            if index != NO_MARK && index as usize >= mark_count {
                return Err(ColumnError::MarkIndex { row: place32(i)?, index });
            }
        }
        Ok((column, &bytes[want..]))
    }

    /// A column with no row: what a text with no origins kept maps through (every range to the
    /// call): the control of the origin tests.
    pub const EMPTY: OriginsRef<'static> = OriginsRef { marks: &[], rows: &[] };

    pub fn len(&self) -> usize {
        self.rows.len() / ROW
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn row(&self, i: usize) -> OriginRow {
        let at = i * ROW;
        let word = |k: usize| u32_at(self.rows, at + 4 * k);
        // Checked by `open`: every tag names an origin.
        let origin = TextOrigin::of_tag(word(2), word(3)).unwrap_or(TextOrigin::Made);
        OriginRow { start: word(0), end: word(1), origin, mark: self.mark(i) }
    }

    /// Row `i`'s mark: what a path whose first segment is this token resolves by (drive.md, "A
    /// token keeps its mark"). `None`: as written at the call.
    pub fn mark(&self, i: usize) -> Option<TextMark> {
        let index = u32_at(self.rows, i * ROW + 16);
        if index == NO_MARK {
            return None;
        }
        // Checked by `open`: the index is in the table and its mark reads.
        let at = index as usize * MARK;
        TextMark::from_bytes(&self.marks[at..at + MARK])
    }
}

/// What a range of an expansion's text maps to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mapped {
    /// Every token of the range is an input token: the lowest and highest of their indexes in
    /// the call's text as written.
    Input { first: u32, last: u32 },
    /// Not wholly input tokens: filed at the call. Says the first token that is not one.
    Call(NotInput),
}

/// Why a range maps to the call.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NotInput {
    /// The range holds no token of the column (only spaces, or the column was not kept).
    NoToken,
    AttrArgs,
    Def,
    Params,
    Made,
    InputRewritten,
}

/// Maps `start..end` of the expansion's text (an empty range: the token holding `start`)
/// through `column`. A binary search for the first token, then a walk over the range's.
pub fn map_range(column: OriginsRef<'_>, start: u32, end: u32) -> Mapped {
    let n = column.len();
    let (mut low, mut high) = (0usize, n);
    while low < high {
        let mid = low + (high - low) / 2;
        if column.row(mid).end <= start {
            low = mid + 1;
        } else {
            high = mid;
        }
    }
    let mut span: Option<(u32, u32)> = None;
    let mut i = low;
    while i < n {
        let row = column.row(i);
        let inside = if start == end { row.start <= start && start < row.end } else { row.start < end };
        if !inside {
            break;
        }
        let at = match row.origin {
            TextOrigin::Input(at) => at,
            TextOrigin::AttrArgs(_) => return Mapped::Call(NotInput::AttrArgs),
            TextOrigin::Def(_) => return Mapped::Call(NotInput::Def),
            TextOrigin::Params(_) => return Mapped::Call(NotInput::Params),
            TextOrigin::Made => return Mapped::Call(NotInput::Made),
            TextOrigin::InputRewritten => return Mapped::Call(NotInput::InputRewritten),
        };
        span = Some(match span {
            None => (at, at),
            Some((a, b)) => (a.min(at), b.max(at)),
        });
        if start == end {
            break;
        }
        i += 1;
    }
    match span {
        Some((first, last)) => Mapped::Input { first, last },
        None => Mapped::Call(NotInput::NoToken),
    }
}

/// How a call's input is cut from its text.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InputShape {
    /// The text between a function-like call's delimiters ([`crate::lex_inside`]).
    Inside,
    /// The whole item an attribute or derive call takes ([`crate::lex`]).
    Whole,
}

impl InputShape {
    pub fn word(self) -> u32 {
        match self {
            InputShape::Inside => 0,
            InputShape::Whole => 1,
        }
    }

    pub fn of_word(word: u32) -> Option<InputShape> {
        match word {
            0 => Some(InputShape::Inside),
            1 => Some(InputShape::Whole),
            _ => None,
        }
    }
}

/// Each token's bytes in `input` (the call's input as it is today), in the index space the
/// column's [`TextOrigin::Input`] counts in: lexed exactly as the expander lexed it, so the n-th
/// place is the n-th token's.
pub fn input_places<'a>(input: &str, shape: InputShape, edition: Edition, alloc: Alloc<'a>) -> Result<AVec<'a, (u32, u32)>, LexError> {
    let mut places: AVec<'a, (u32, u32)> = AVec::new_in(alloc);
    match shape {
        InputShape::Whole => {
            crate::tokens::lex_placed(input, edition, alloc, Some(&mut places))?;
        }
        InputShape::Inside => {
            // As `lex_inside`: the text in parentheses, the two of them dropped.
            let mut group = blaze_own::AString::new_in(alloc);
            group.push('(');
            group.push_str(input);
            group.push(')');
            let mut all: AVec<'a, (u32, u32)> = AVec::new_in(alloc);
            crate::tokens::lex_placed(group.as_str(), edition, alloc, Some(&mut all))?;
            // The two parentheses are always tokens: fewer is a text the lexer refused.
            if all.len() < 2 {
                return Err(LexError::Refused { edition });
            }
            places.extend(all[1..all.len() - 1].iter().map(|&(s, e)| (s.saturating_sub(1), e.saturating_sub(1))));
        }
    }
    Ok(places)
}
