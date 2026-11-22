//! A finding inside an expansion is filed where its tokens came from (drive.md, ruled 20:57).
//!
//! The task that MAKES a finding in expanded text maps its range before filing it (here: the
//! check loop, check.rs `check_rounds_as`, for each diagnostic): through the expansion's origin
//! column (blaze-scope expansion_origins.rs, blaze-expand origin_column.rs), a range whose tokens
//! are all the call's input tokens is filed at those tokens' bytes in the calling text; any
//! other at the call, naming the macro as written. A call that is itself in an expansion maps
//! through that expansion in turn, level by level, until a file: a loop of at most
//! [`MOST_LEVELS`] (rule 18), no recursion. Mapping at the making, not the printing, so a kept
//! finding and a fresh one are equal.

use std::alloc::Global;
use std::boxed::Box;

use blaze_expand::origin_column::{InputShape, Mapped, NotInput, input_places, map_range};
use blaze_id::ItemKey as LibKey;
use blaze_own::Alloc;
use blaze_scope::expansion_origins::{OriginsError, OriginsView};
use blaze_scope::resolve::At;
use blaze_surface::Edition;
use blaze_typeck as tc;

use super::source::NamesSource;

/// The most expansion levels a finding maps through: one per nested call (rule 18).
pub const MOST_LEVELS: u32 = 64;

/// A call's origins, as a names source answers them: the column and call read in place, and
/// the calling text as it is today.
#[derive(Clone, Copy, Debug)]
pub struct CallOrigin<'n> {
    pub view: OriginsView<'n>,
    /// The text the call is written in, and the path a finding prints for it.
    pub text: &'n str,
    pub shown: &'n str,
    /// The call's outline entry's bytes in `text`: the call as a whole.
    pub entry: (u32, u32),
    /// The calling crate's edition: its input is lexed in it.
    pub edition: Edition,
}

/// Why a finding in an expansion was not mapped: each case by name, the finding filed at its
/// own place beside it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unmapped {
    /// The finding's item has no place the source answers.
    NoPlace,
    /// The expansion's text keeps no origins, or they did not read.
    Origins { call: LibKey, error: OriginsError },
    /// The source holds no text for the call, or no calling text for it.
    NoText { call: LibKey },
    /// The loader serves no origins for the call (a loader that keeps none: `Load`'s default).
    NotServed { call: LibKey },
    /// The expansion's record keeps no call part, by its writer's design (blaze-scope
    /// expansion_records.rs `write`: a text whose origins are `Err`, such as a call's statements
    /// read as items, a block's module text or a column that could not be made, keeps none): where
    /// the call stands is not recorded, so nothing in that text is placed through it. Typed,
    /// never a records error (tempo a1425, 2026-11-21: filed in the log, it voided the instance).
    NoCallPart { call: LibKey },
    /// Over the crates' records, the calling text's outline is not served yet: owed with the
    /// `Texts` table's filling over records (texts.rs `Texts::empty`).
    OverRecords { call: LibKey },
    /// The call's input does not lex as it did, or an index has no token in it.
    Input { call: LibKey },
    /// More than [`MOST_LEVELS`] nested calls.
    Levels,
    /// A member's text is not a part of its container's text as the source holds it (a member
    /// a macro call in the container's body wrote, or a text served again at another address
    /// over records): its place in the container is not known ([`file_member_range`]).
    MemberText { container: LibKey },
}

/// Where a finding is filed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Filed {
    /// Not in an expansion: the finding's own range stands.
    Own,
    /// At the user's tokens: bytes `start..end` of the text `shown`, at `line:col` (1-based).
    Text { shown: Box<str, Global>, at: At, start: u32, end: u32, line: u32, col: u32 },
    /// At the call of `macro_name` as written (the last macro the walk out met: the one the
    /// user's text names), because the range is not wholly input tokens (`why`).
    Call { shown: Box<str, Global>, at: At, start: u32, end: u32, line: u32, col: u32, macro_name: Box<str, Global>, attribute: bool, implicit: bool, why: NotInput },
    /// Not mapped, and why.
    Unmapped(Unmapped),
}

impl core::fmt::Display for Filed {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Filed::Own => Ok(()),
            Filed::Text { shown, line, col, .. } => write!(f, "filed at {shown}:{line}:{col}"),
            Filed::Call { shown, line, col, macro_name, attribute, implicit, .. } => {
                let written = match (attribute, implicit) {
                    (true, true) => std::format!("the implicit `#[{macro_name}]`"),
                    (true, false) => std::format!("`#[{macro_name}]`"),
                    (false, _) => std::format!("`{macro_name}!`"),
                };
                write!(f, "filed at {shown}:{line}:{col}, in the expansion of {written}")
            }
            Filed::Unmapped(why) => write!(f, "in an expansion, not mapped to its source: {why:?}"),
        }
    }
}

/// The 1-based line and column of byte `at` of `text` (columns in chars).
fn line_col(text: &str, at: u32) -> (u32, u32) {
    let head = text.get(..at as usize).unwrap_or(text);
    let line = head.bytes().filter(|&b| b == b'\n').count();
    let col = head.rsplit('\n').next().map_or(0, |l| l.chars().count());
    (u32::try_from(line + 1).unwrap_or(u32::MAX), u32::try_from(col + 1).unwrap_or(u32::MAX))
}

/// Files a finding of a MEMBER (of the trait or impl `container`, whose text is `member`) at
/// `range`, relative to the member's first byte: a member has no library key of its own
/// (front2 keys.rs `library_in`), so the range is moved to the container's text, where the
/// member's text is, and filed as the container's ([`file_range`]). Before this every finding of
/// an impl's or a trait's fn printed "not mapped to its source: NoPlace" (lane RF13: core's
/// `<impl ErrorKind>::from_prim`).
pub fn file_member_range<'n, S: NamesSource<'n>>(names: S, container: LibKey, member: &str, range: tc::Range, alloc: Alloc<'_>) -> Filed {
    let Some(whole) = names.text_of(container) else { return Filed::Unmapped(Unmapped::NoPlace) };
    let Some(offset) = blaze_scope::expansion_origins::offset_in(whole.text, member) else {
        return Filed::Unmapped(Unmapped::MemberText { container });
    };
    let moved = tc::Range { start: range.start.saturating_add(offset), end: range.end.saturating_add(offset) };
    file_range(names, Some(container), moved, alloc)
}

/// Files a finding of item `lib` at `range` (relative to the item's first byte, as the checker's
/// ranges are). `alloc` holds the input's places while one level is mapped.
pub fn file_range<'n, S: NamesSource<'n>>(names: S, lib: Option<LibKey>, range: tc::Range, alloc: Alloc<'_>) -> Filed {
    let Some(lib) = lib else { return Filed::Unmapped(Unmapped::NoPlace) };
    let (Some(place), Some((at, first))) = (names.place_of(lib), names.item_start(lib)) else {
        return Filed::Unmapped(Unmapped::NoPlace);
    };
    let At::Expansion(mut call) = at else { return Filed::Own };
    let krate = place.krate;
    let (mut start, mut end) = (first.saturating_add(range.start), first.saturating_add(range.end));
    // The last call the walk filed at: (macro as written, attribute, implicit, why).
    let mut via: Option<(&'n str, bool, bool, NotInput)> = None;
    for _ in 0..MOST_LEVELS {
        let origin = match names.call_origin(krate, call) {
            Ok(origin) => origin,
            Err(why) => return Filed::Unmapped(why),
        };
        let attribute = origin.view.shape == InputShape::Whole;
        (start, end) = match map_range(origin.view.column, start, end) {
            Mapped::Input { first, last } => {
                let input_at = origin.entry.0.saturating_add(origin.view.input_start);
                let input = origin.text.get(input_at as usize..input_at.saturating_add(origin.view.input_len) as usize);
                let places = input.and_then(|input| input_places(input, origin.view.shape, origin.edition, alloc).ok());
                let bytes = places.as_ref().and_then(|p| Some((p.get(first as usize)?.0, p.get(last as usize)?.1)));
                match bytes {
                    Some((a, b)) => (input_at + a, input_at + b),
                    None => return Filed::Unmapped(Unmapped::Input { call }),
                }
            }
            Mapped::Call(why) => {
                via = Some((origin.view.macro_name, attribute, origin.view.implicit, why));
                origin.entry
            }
        };
        match origin.view.text {
            At::Expansion(parent) => call = parent,
            At::File(_) => {
                let (line, col) = line_col(origin.text, start);
                let shown = Box::from(origin.shown);
                return match via {
                    None => Filed::Text { shown, at: origin.view.text, start, end, line, col },
                    Some((name, attribute, implicit, why)) => {
                        Filed::Call { shown, at: origin.view.text, start, end, line, col, macro_name: Box::from(name), attribute, implicit, why }
                    }
                };
            }
        }
    }
    Filed::Unmapped(Unmapped::Levels)
}
