//! An expansion text's origins (drive.md, "A finding inside an expansion is reported where its
//! tokens came from", ruled 20:57): kept beside the text in its record, written once when the
//! text is made, read in place by the task that makes a finding in the text.
//!
//! Two parts. The CALL: the text it is written in ([`At`]: a file, or the expansion of another
//! call, so a nested expansion maps level by level), its outline entry there (the fn-like call's,
//! or the item an attribute or derive takes), where its input starts in that entry (item-relative,
//! rule 71), how its input is cut ([`InputShape`]), and the macro's path as written
//! ([`IMPLICIT_MAIN`](crate::task::IMPLICIT_MAIN) for D104's implicit call). The COLUMN: each
//! rendered token's bytes in the text and its origin, input tokens by index in the call's text as
//! written (blaze-expand origin_column.rs).
//!
//! Form, little-endian, read by [`OriginsView::open`]: the text's tag u32 (0 file, 1 expansion),
//! the file index u32 (0 for an expansion), the calling expansion's call key (32 bytes, zero for a
//! file), the outline entry u32, the input's start in the entry u32, its length u32, the shape
//! word u32, implicit u32 (0 or 1), the path's length u32, the path's bytes padded to 4, then the
//! column (blaze-expand `OriginColumn::write`). A function of the call's text and its expansion's
//! tokens only: equal for any builder (rule 25).

use blaze_expand::origin_column::{ColumnError, InputShape, OriginColumn, OriginsRef, input_places};
use blaze_id::{ContentId, ItemKey};
use blaze_own::{AVec, Alloc, Bump};
use blaze_surface::{Edition, ItemIndex};

use crate::resolve::At;

/// Where a call stands in the text it is written in, as the lowering saw it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CallInText {
    /// The calling text: set by `Tables::add`, the one door a lowered call enters the tables by
    /// (the lowering knows its fragment, the tables its `At`).
    pub text: Option<At>,
    /// The fn-like call's outline entry, or the item an attribute or derive call takes.
    pub item: ItemIndex,
    /// The input's bytes, from the entry's first byte.
    pub input_start: u32,
    pub input_len: u32,
}

/// Where `part` (a slice of `text`) starts in `text`; `None` when it is not a slice of it. Safe
/// address arithmetic only: both are borrowed `str`s, compared as addresses, never read through.
pub fn offset_in(text: &str, part: &str) -> Option<u32> {
    let (base, at) = (text.as_ptr() as usize, part.as_ptr() as usize);
    if at < base || at + part.len() > base + text.len() {
        return None;
    }
    u32::try_from(at - base).ok()
}

/// Why an expansion's origins could not be written or read (rule: blaze-scope expansion_origins.rs).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OriginsError {
    /// The lowering placed no calling text for the call (a call the tables did not take through
    /// `Tables::add`).
    Unplaced { call: ItemKey },
    /// The text is not the call's rendering as the column places it: `what` names the text
    /// (a call's statements read as items, a block's module text; a kept item's text has its
    /// origins, [`kept`]). A finding in it is filed at
    /// its own place with this reason, never mapped by a guess.
    Rewritten { what: &'static str },
    /// The call's input is not a slice of the text it was lowered from.
    InputNotInText { call: ItemKey },
    /// The column could not be made or read.
    Column { call: Option<ItemKey>, error: ColumnError },
    /// A token's index has no rendered place, or the renderer placed one twice.
    Places { call: ItemKey, tokens: u32, places: u32 },
    /// The bytes are shorter than their head says.
    Short { len: usize, want: usize },
    /// A tag or word of the head is no value it may hold.
    Head { what: &'static str, word: u32 },
    /// A kept item's text ([`kept`]) does not lex, so its tokens have no places.
    KeptLex { call: ItemKey },
}

impl core::fmt::Display for OriginsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            OriginsError::Unplaced { call } => write!(f, "the macro call {call:?} has no calling text placed (rule: blaze-scope expansion_origins.rs)"),
            OriginsError::Rewritten { what } => write!(f, "{what} is not a rendering the origin column places (rule: blaze-scope expansion_origins.rs)"),
            OriginsError::InputNotInText { call } => {
                write!(f, "the input of macro call {call:?} is not a slice of the text it was lowered from (rule: blaze-scope expansion_origins.rs)")
            }
            OriginsError::Column { call: Some(call), error } => write!(f, "the origins of macro call {call:?}: {error}"),
            OriginsError::Column { call: None, error } => write!(f, "an expansion's origins: {error}"),
            OriginsError::Places { call, tokens, places } => write!(
                f,
                "the expansion of macro call {call:?} rendered {places} token places for {tokens} tokens (rule: blaze-scope expansion_origins.rs)"
            ),
            OriginsError::Short { len, want } => {
                write!(f, "an expansion's origins of {len} bytes are shorter than the {want} their head says (rule: blaze-scope expansion_origins.rs)")
            }
            OriginsError::Head { what, word } => write!(f, "an expansion's origins hold {word} as its {what} (rule: blaze-scope expansion_origins.rs)"),
            OriginsError::KeptLex { call } => {
                write!(f, "the kept item's text of call {call:?} does not lex, so its tokens have no places (rule: blaze-scope expansion_origins.rs `kept`)")
            }
        }
    }
}

const FIXED: usize = 4 + 4 + 32 + 4 * 6;

fn pad4(n: usize) -> usize {
    (n + 3) & !3
}

/// The origins' bytes for `call`'s expansion, in `alloc`.
pub fn write_bytes<'a>(
    call: ItemKey,
    place: Option<CallInText>,
    shape: InputShape,
    macro_name: &str,
    implicit: bool,
    column: &OriginColumn<'_>,
    alloc: Alloc<'a>,
) -> Result<AVec<'a, u8>, OriginsError> {
    // `None`: a check call (never expanded) or an input that is no slice of its text.
    let place = place.ok_or(OriginsError::InputNotInText { call })?;
    let text = place.text.ok_or(OriginsError::Unplaced { call })?;
    let (tag, file, parent) = match text {
        At::File(file) => (0u32, file, [0u8; 32]),
        At::Expansion(parent) => (1, 0, parent.0.0),
    };
    let name_len = u32::try_from(macro_name.len()).map_err(|_| OriginsError::Column { call: Some(call), error: ColumnError::TooLarge { at: macro_name.len() } })?;
    let mut out: AVec<'a, u8> = AVec::new_in(alloc);
    out.reserve(FIXED + pad4(macro_name.len()) + 4 + column.rows().len() * blaze_expand::origin_column::ROW);
    out.extend_from_slice(&tag.to_le_bytes());
    out.extend_from_slice(&file.to_le_bytes());
    out.extend_from_slice(&parent);
    for word in [place.item.0, place.input_start, place.input_len, shape.word(), u32::from(implicit), name_len] {
        out.extend_from_slice(&word.to_le_bytes());
    }
    out.extend_from_slice(macro_name.as_bytes());
    out.resize(FIXED + pad4(macro_name.len()), 0);
    column.write(&mut out).map_err(|error| OriginsError::Column { call: Some(call), error })?;
    Ok(out)
}

/// The origins' bytes for `call`'s expansion, in `alloc`, frozen into the bump arena as the text beside them is.
pub fn write<'a>(
    call: ItemKey,
    place: Option<CallInText>,
    shape: InputShape,
    macro_name: &str,
    implicit: bool,
    column: &OriginColumn<'_>,
    alloc: Bump<'a>,
) -> Result<&'a [u8], OriginsError> {
    let out = write_bytes(call, place, shape, macro_name, implicit, column, alloc.alloc())?;
    Ok(alloc.alloc_slice_copy(&out))
}

/// The origins of keep call `call`'s text (resolve.rs `keep_item`: the item its derives' helper
/// attributes keep, `#[derive(..)] #[helper(..)] struct S { .. }`), in `alloc`. The text IS the
/// call's input as written (a slice of the calling text), so its column is the identity: token
/// `i`, lexed as every reader lexes a call's input (blaze-expand `input_places`, the whole
/// item), is input token `i` (tokens sharing one place: one row, the first's), with no mark (the lowering reads the kept text with none:
/// resolve.rs `keep_item`, `Fragment::marks`). Its call part names the entry the call is at,
/// the item itself: a reader of a head the kept item declares walks to that file entry and its
/// `Signature` (blaze-cli body_types/covered.rs `callers`), and a finding in the text maps to
/// the user's own tokens. A function of the text alone (rules 25, 71).
pub fn kept<'a>(call: ItemKey, place: Option<CallInText>, text: &str, macro_name: &str, edition: Edition, alloc: Bump<'a>) -> Result<&'a [u8], OriginsError> {
    let a = alloc.alloc();
    let places = input_places(text, InputShape::Whole, edition, a).map_err(|_| OriginsError::KeptLex { call })?;
    // One row a PLACE, not a token: a doc comment's tokens, a lifetime's two and the groups one
    // closer closes share one (blaze-expand `OriginColumn::input_identity`).
    let column = OriginColumn::input_identity(&places, a).map_err(|error| OriginsError::Column { call: Some(call), error })?;
    write(call, place, InputShape::Whole, macro_name, false, &column, alloc)
}

/// The column of `call`'s expansion from the renderer's places (`(start, end, token)` in token
/// order: blaze-scope resolve.rs `render_with`) and its `roots` (`(place, crate)`, sorted by
/// place: the `crate` written for that crate's `$crate`, whose row carries
/// [`crate::crate_roots::root_mark`]) and the expansion's origins, input indexes carried into
/// the call's text as written by `input`.
pub fn column_of<'a>(
    call: ItemKey,
    places: &[(u32, u32, u32)],
    roots: &[(u32, blaze_id::CrateItemKey)],
    origins: &[blaze_expand::TokenOrigin],
    input: blaze_expand::origin_column::InputIndex<'_>,
    // Each token's innermost mark (drive.md, "A token keeps its mark": the same column):
    // `|at| expansion.hygiene(at).mark.map(TextMark::of)`.
    marks: &dyn Fn(usize) -> Option<blaze_expand::origin_column::TextMark>,
    alloc: Alloc<'a>,
) -> Result<OriginColumn<'a>, OriginsError> {
    let mut column = OriginColumn::new_in(alloc);
    let count = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
    let mut roots = roots.iter().peekable();
    for (place, &(start, end, token)) in places.iter().enumerate() {
        let (token, start, end) = (token as usize, start as usize, end as usize);
        let root = roots.next_if(|(at, _)| *at as usize == place).map(|(_, krate)| *krate);
        let origin = origins.get(token).copied().ok_or(OriginsError::Places { call, tokens: count(origins.len()), places: count(places.len()) })?;
        // A root row is only placed for a marked `$crate` (the renderer's `dollar` is that mark's
        // crate): its edition is that mark's.
        let mark = match root {
            Some(krate) => marks(token).map(|m| crate::crate_roots::root_mark(krate, m.edition)),
            None => marks(token),
        };
        column
            .push(start, end, blaze_expand::origin_column::text_origin(origin, input), mark)
            .map_err(|error| OriginsError::Column { call: Some(call), error })?;
    }
    Ok(column)
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    let mut word = [0u8; 4];
    word.copy_from_slice(&bytes[at..at + 4]);
    u32::from_le_bytes(word)
}

/// An expansion's origins read in place, checked once.
#[derive(Clone, Copy, Debug)]
pub struct OriginsView<'s> {
    /// The text the call is written in.
    pub text: At,
    pub item: ItemIndex,
    pub input_start: u32,
    pub input_len: u32,
    pub shape: InputShape,
    pub implicit: bool,
    /// The macro's path as written (`::std::main` for D104's implicit call).
    pub macro_name: &'s str,
    pub column: OriginsRef<'s>,
}

/// Origins `bytes` cut where the column starts: the CALL part before it (where the call stands
/// in its calling text: a function of the call's place, so two calls of equal input have two),
/// the COLUMN after it (a function of the expansion alone: its tokens' places in the text and
/// their origins, input tokens by index, rule 71; so two calls of equal input have one). The
/// expansion records keep the column with the text and the call part in the crate's directory
/// (expansion_records.rs); [`OriginsView::open_parts`] reads the two where they lie.
pub fn split(bytes: &[u8]) -> Result<(&[u8], &[u8]), OriginsError> {
    let at = call_len(bytes)?;
    Ok(bytes.split_at(at))
}

/// The length of the call part that begins `bytes`: the fixed words and the macro's path,
/// padded to 4.
fn call_len(bytes: &[u8]) -> Result<usize, OriginsError> {
    if bytes.len() < FIXED {
        return Err(OriginsError::Short { len: bytes.len(), want: FIXED });
    }
    // The path's length is the sixth word after the 40 bytes of tag, file and calling call.
    let column_at = FIXED + pad4(u32_at(bytes, 40 + 4 * 5) as usize);
    if bytes.len() < column_at {
        return Err(OriginsError::Short { len: bytes.len(), want: column_at });
    }
    Ok(column_at)
}

impl<'s> OriginsView<'s> {
    pub fn open(bytes: &'s [u8]) -> Result<OriginsView<'s>, OriginsError> {
        let (call, column) = split(bytes)?;
        OriginsView::open_parts(call, column)
    }

    /// The origins whose call part is `call` and whose column is `column`, each read where it
    /// lies (an expansion record's column, its crate directory's call part): equal to
    /// [`OriginsView::open`] of the two joined.
    pub fn open_parts(call: &'s [u8], column: &'s [u8]) -> Result<OriginsView<'s>, OriginsError> {
        let CallView { text, item, input_start, input_len, shape, implicit, macro_name } = CallView::open(call)?;
        let (column, _) = OriginsRef::open(column).map_err(|error| OriginsError::Column { call: None, error })?;
        Ok(OriginsView { text, item, input_start, input_len, shape, implicit, macro_name, column })
    }
}

/// The CALL part of an expansion's origins alone ([`split`]'s first half), read where it lies:
/// where the call stands and the macro's path, without the column. What a walk up a call's
/// callers reads (blaze-cli body_types/covered.rs `callers`): it is in the crate's directory
/// (expansion_records.rs), so the walk reads no expansion record.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CallView<'s> {
    /// The text the call is written in.
    pub text: At,
    pub item: ItemIndex,
    pub input_start: u32,
    pub input_len: u32,
    pub shape: InputShape,
    pub implicit: bool,
    /// The macro's path as written.
    pub macro_name: &'s str,
}

impl<'s> CallView<'s> {
    /// The call part `call`: exactly its fixed words and its padded path, no column.
    pub fn open(call: &'s [u8]) -> Result<CallView<'s>, OriginsError> {
        let at = call_len(call)?;
        if at != call.len() {
            return Err(OriginsError::Head { what: "call part's length (not its path's padded end)", word: u32::try_from(call.len()).unwrap_or(u32::MAX) });
        }
        let bytes = call;
        let mut key = [0u8; 32];
        key.copy_from_slice(&bytes[8..40]);
        let text = match u32_at(bytes, 0) {
            0 => At::File(u32_at(bytes, 4)),
            1 => At::Expansion(ItemKey(ContentId(key))),
            word => return Err(OriginsError::Head { what: "text tag", word }),
        };
        let word = |k: usize| u32_at(bytes, 40 + 4 * k);
        let shape = InputShape::of_word(word(3)).ok_or(OriginsError::Head { what: "input shape", word: word(3) })?;
        let implicit = match word(4) {
            0 => false,
            1 => true,
            w => return Err(OriginsError::Head { what: "implicit flag", word: w }),
        };
        let name_end = FIXED + word(5) as usize;
        let macro_name = core::str::from_utf8(&bytes[FIXED..name_end]).map_err(|_| OriginsError::Head { what: "macro path (not UTF-8)", word: word(5) })?;
        Ok(CallView { text, item: ItemIndex(word(0)), input_start: word(1), input_len: word(2), shape, implicit, macro_name })
    }
}
