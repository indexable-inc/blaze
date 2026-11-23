//! A path's crate root in an expansion's ITEM text, by its first segment's MARK (drive.md, "A
//! token keeps its mark", ruled 21:27; piece token-mark-2). No spelled token: the text writes the
//! keyword `crate` where the expansion had another crate's `$crate`, and that token's row in the
//! text's origin column (blaze-expand origin_column.rs) carries the mark it resolves by.
//!
//! The rule is Rust's, one function ([`root_by_mark`]): the Reference, "Paths", "`$crate`":
//! "`$crate` is only used within macro transcribers, and can only be used as the first segment,
//! without a preceding `::`. `$crate` will expand to a path to access items from the top level
//! of the crate where the macro is defined, regardless of which crate the macro is invoked."
//! "Paths", "`crate`": "`crate` resolves the path relative to the current crate"; under a
//! macro's mark the current crate is where the mark says (the Reference, "Macros By Example",
//! "Hygiene": a `macro_rules!` is mixed site, so a plain `crate` it writes is the CALL's crate; a
//! `macro` item is definition site, so its `crate` is the definition's). So a `crate` the
//! renderer wrote for `$crate` of crate `k` carries [`root_mark`]: crate `k`'s root, opaque, and
//! the same one rule reads a `$crate`, a `crate` from a `macro` item and a plain `crate`.
//!
//! [`TextMarks`] finds a segment's mark (the segment is a slice of the text: its offset by
//! address, [`crate::expansion_origins::offset_in`]); [`remap`] carries a column into a text made
//! of copied slices of the expansion's (blaze-scope expanded_blocks.rs: a statement call's items,
//! a block's module), so a rewritten text keeps its marks. Sans-IO, no recursion; everything
//! allocated is in the caller's `alloc`.

use blaze_expand::origin_column::{ColumnError, OriginColumn, OriginsRef, TextMark};
use blaze_expand::{DefSite, Edition, Transparency};
use blaze_id::CrateItemKey;
use blaze_own::{Alloc, Bump};

#[cfg(test)]
mod tests {
    use super::{Root, root_by_mark, root_mark};
    use blaze_expand::{DefSite, Edition, Transparency};
    use blaze_expand::origin_column::TextMark;
    use blaze_id::{ContentId, CrateItemKey};

    /// The Reference, "Paths", "`$crate`" and "Macros By Example", "Hygiene": `$crate` names the
    /// defining crate; a `macro_rules!`'s plain `crate` the call's; the renderer's `crate` for
    /// another crate's `$crate` carries [`root_mark`] and reads as that crate. CONTROL: with a
    /// semi-opaque mark (the old meaning of a written `crate`) the third assertion fails.
    /// Expected: `test crate_roots::tests::a_root_by_its_mark ... ok`.
    #[test]
    fn a_root_by_its_mark() {
        let dep = CrateItemKey(ContentId([7; 32]));
        let rules = TextMark { site: DefSite { krate: dep, module: None }, transparency: Transparency::SemiOpaque, edition: Edition::E2021 };
        assert_eq!(root_by_mark("$crate", Some(rules)), Root::Crate(dep));
        assert_eq!(root_by_mark("crate", Some(rules)), Root::Here);
        assert_eq!(root_by_mark("crate", Some(root_mark(dep, Edition::E2021))), Root::Crate(dep));
        assert_eq!(root_by_mark("$crate", None), Root::Here);
        assert_eq!(root_by_mark("inner", Some(rules)), Root::NotARoot);
    }
}

/// The mark of a `crate` keyword written for the `$crate` of `krate` (read in `edition`): the
/// definition crate's root, opaque, so [`root_by_mark`] reads it as `krate`.
pub fn root_mark(krate: CrateItemKey, edition: Edition) -> TextMark {
    TextMark { site: DefSite { krate, module: None }, transparency: Transparency::Opaque, edition }
}

/// What a path's first segment says of its crate root, by its mark.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Root {
    /// The root of this crate (a definition's, by the mark).
    Crate(CrateItemKey),
    /// The root of the crate whose text holds the path.
    Here,
    /// The segment is no crate root (`self`, `super`, a name): resolved as before.
    NotARoot,
}

/// The crate root a path whose first segment is `first`, with `mark`, names. THE one
/// resolver-by-mark of a crate root: blaze-scope's [`to_path_marked`](crate::task::to_path_marked)
/// and the front end's `blaze_lower::mark_site::site_of` (token-mark-1) both ask it.
///
/// - `$crate`: the mark's definition crate; with no mark, the crate whose text holds it (a
///   `$crate` no expansion wrote: tokio 1.53.2's `doc! { macro_rules! try_join { .. $crate .. } }`,
///   in a definition's own tokens). A body has no unmarked `$crate`: `site_of` refuses it first.
/// - `crate`: an opaque mark's crate (a `macro` item's, and the renderer's `$crate`, [`root_mark`]);
///   a semi-opaque or transparent mark, or none: [`Root::Here`].
pub fn root_by_mark(first: &str, mark: Option<TextMark>) -> Root {
    match (first, mark) {
        ("$crate", Some(m)) => Root::Crate(m.site.krate),
        ("$crate", None) => Root::Here,
        ("crate", Some(m)) if m.transparency == Transparency::Opaque => Root::Crate(m.site.krate),
        ("crate", _) => Root::Here,
        _ => Root::NotARoot,
    }
}

/// A macro definition an expansion writes holds the `$crate` of ANOTHER crate than the one
/// expanding (crate `k`'s macro, called in crate `here`, writes `macro_rules! b { .. $crate .. }`):
/// the definition is lowered from text and defined in `here`, so its `$crate` would name `here`.
/// Refused by name, never written another way: carrying the mark into the written definition
/// (its `MacroSrc` keeping the crate its `$crate` names, in the two names records and the asked
/// digest) is the named next piece (token-mark-3). The Reference, "Macros By Example",
/// "Hygiene": `$crate` names the crate that defines the macro the token came from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ForeignDollarCrate {
    /// The crate the `$crate` names.
    pub krate: CrateItemKey,
}

/// An expansion text and its origin column: the mark of a token by its place.
#[derive(Clone, Copy, Debug)]
pub struct TextMarks<'t> {
    pub text: &'t str,
    pub column: OriginsRef<'t>,
}

/// Why a segment has no answer from [`TextMarks::mark_of`] (rule: blaze-scope crate_roots.rs).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MarkMiss {
    /// The segment is not a slice of the text the marks are of: a path the lowering made from
    /// other text; asking it would read another token's mark.
    NotInText { len: u32 },
}

impl core::fmt::Display for MarkMiss {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            MarkMiss::NotInText { len } => write!(f, "a path segment of {len} bytes is not in the expansion text its marks are of (blaze-scope crate_roots.rs)"),
        }
    }
}

impl<'t> TextMarks<'t> {
    /// The mark of the token that starts where `segment` (a slice of the text) starts; `None`
    /// when no row starts there (a token as written at the call keeps none).
    pub fn mark_of(&self, segment: &str) -> Result<Option<TextMark>, MarkMiss> {
        let len = u32::try_from(segment.len()).unwrap_or(u32::MAX);
        let start = crate::expansion_origins::offset_in(self.text, segment).ok_or(MarkMiss::NotInText { len })?;
        // Rows are sorted by start and do not overlap (blaze-expand `OriginsRef::open` checked).
        let (mut lo, mut hi) = (0usize, self.column.len());
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let row = self.column.row(mid);
            if row.start == start {
                return Ok(row.mark);
            }
            if row.start < start {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        Ok(None)
    }
}

/// The column of an expansion text's origins (`ExpansionText::origins`), or why there is none:
/// a text whose renderer wrote a `crate` for another crate's `$crate` and keeps no column cannot
/// say which `crate` is whose, so its caller refuses it by name ([`RootsLost`]).
pub fn column_in<'a>(origins: &Result<&'a [u8], crate::expansion_origins::OriginsError>) -> Result<OriginsRef<'a>, RootsLost> {
    let bytes = origins.as_ref().map_err(|error| RootsLost::NoOrigins { error: *error })?;
    crate::expansion_origins::OriginsView::open(bytes).map(|view| view.column).map_err(|error| RootsLost::NoOrigins { error })
}

/// `column` carried into a rewritten text ([`remap`]), written and read back in place: its bytes
/// live in `alloc`, as the texts they mark do.
pub fn remapped<'a>(column: OriginsRef<'_>, copies: &[Copied], alloc: Bump<'a>) -> Result<OriginsRef<'a>, RootsLost> {
    let a = alloc.alloc();
    let moved = remap(column, copies, a).map_err(|error| RootsLost::Column { error })?;
    let mut bytes: blaze_own::AVec<'a, u8> = blaze_own::AVec::new_in(a);
    moved.write(&mut bytes).map_err(|error| RootsLost::Column { error })?;
    let bytes: &'a [u8] = alloc.alloc_slice_copy(&bytes);
    OriginsRef::open(bytes).map(|(column, _)| column).map_err(|error| RootsLost::Column { error })
}

/// Why an expansion text that writes another crate's root keeps no marks (rule: blaze-scope
/// crate_roots.rs): its `crate` tokens cannot be told apart, so the call is refused, never read
/// as the expanding crate's.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RootsLost {
    NoOrigins { error: crate::expansion_origins::OriginsError },
    Column { error: ColumnError },
}

impl core::fmt::Display for RootsLost {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            RootsLost::NoOrigins { error } => write!(f, "the expansion writes another crate's `$crate` and keeps no origin column: {error}"),
            RootsLost::Column { error } => write!(f, "the expansion writes another crate's `$crate` and its origin column does not carry over: {error}"),
        }
    }
}

/// One slice a rewritten text copied: `len` bytes from `from` in the expansion's text to `to`
/// in the new one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Copied {
    pub from: u32,
    pub to: u32,
    pub len: u32,
}

/// `column` carried into a text made of `copies` (sorted by `to`, as written): each row wholly
/// inside a copied slice, moved with it; a row of no copied slice is not in the new text. The
/// rows stay sorted because the copies are written in order and do not overlap in the new text.
pub fn remap<'a>(column: OriginsRef<'_>, copies: &[Copied], alloc: Alloc<'a>) -> Result<OriginColumn<'a>, ColumnError> {
    let mut out = OriginColumn::new_in(alloc);
    for copy in copies {
        let end = copy.from.saturating_add(copy.len);
        // The first row at or after the slice's start.
        let (mut lo, mut hi) = (0usize, column.len());
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if column.row(mid).start < copy.from {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        let mut i = lo;
        while i < column.len() {
            let row = column.row(i);
            if row.end > end {
                break;
            }
            let shift = |at: u32| (at - copy.from + copy.to) as usize;
            out.push(shift(row.start), shift(row.end), row.origin, row.mark)?;
            i += 1;
        }
    }
    Ok(out)
}
