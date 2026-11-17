//! Where a path in expanded text is resolved, by the mark of its first segment: the ONE
//! function for it (drive.md, "A token keeps its mark", ruled 21:27; rule 30). Its three uses
//! are `$crate` (the crate the mark's definition is in), a name a `macro` item's body writes
//! (resolved where the macro is defined), and a body macro call's own name; every resolver of
//! a body's paths (blaze-lower `scoped::Scoped`, blaze-ladder front2 `ScopedBy` and
//! `BodyWorld::path`, the scoped tests' `Ask`) asks it and nothing else.
//!
//! The rules (the Reference, "Macros By Example", Hygiene: "By default, all identifiers
//! referred to in a macro expansion are looked up as if they appeared at the invocation
//! site", with `$crate` the exception, "the crate where the macro was defined"; a `macro`
//! item is def-site, rustc's opaque transparency):
//! - `$crate` names the crate of its token's mark's definition; a `$crate` with no mark is
//!   refused by name ([`SiteRefused::UnmarkedDollarCrate`]): it is in no expansion.
//! - a path whose first segment's mark is opaque (a `macro` item's, or a token a macro made
//!   def-site) is resolved in the definition's module;
//! - every other path (no mark: the call's own text; a `macro_rules!`'s semi-opaque mark; a
//!   transparent one) is resolved at the CALL, the module the body is in. An ITEM name a
//!   `macro_rules!` writes resolves at the call (only locals, labels and `$crate` are
//!   def-site there, and locals and labels are the lowering's, never a module's).
//!
//! Sans-IO; the one allocation ([`column_of_copy`]) is in the caller's `alloc`.

use blaze_expand::Transparency;
use blaze_surface::{CrateItemKey, ItemKey};

/// A token's mark as an expansion's text keeps it: its one home is blaze-expand's origin
/// column.
pub use blaze_expand::origin_column::TextMark;

/// The marks of an item's own text when a macro wrote the item: `text` is a slice of the
/// expansion text `marks` are of, checked once by [`ItemMarks::of`] (rule 74), so a lowering
/// that takes this type reads each word's mark at its own place and no other text can be
/// lowered with these marks. The renderer writes another crate's `$crate` in an item text as
/// the word `crate` and keeps its crate in that word's mark (blaze-scope crate_roots
/// `root_mark`): a reader of the item's text alone would name the expanding crate.
#[derive(Clone, Copy, Debug)]
pub struct ItemMarks<'t> {
    text: &'t str,
    marks: blaze_scope::crate_roots::TextMarks<'t>,
    /// Where `text` starts in `marks.text`.
    offset: u32,
}

impl<'t> ItemMarks<'t> {
    /// `text`'s marks, or why `text` is not a slice of the text `marks` are of.
    pub fn of(text: &'t str, marks: blaze_scope::crate_roots::TextMarks<'t>) -> Result<ItemMarks<'t>, blaze_scope::crate_roots::MarkMiss> {
        let len = u32::try_from(text.len()).unwrap_or(u32::MAX);
        let offset = blaze_scope::expansion_origins::offset_in(marks.text, text).ok_or(blaze_scope::crate_roots::MarkMiss::NotInText { len })?;
        Ok(ItemMarks { text, marks, offset })
    }

    /// The item's text.
    pub fn text(&self) -> &'t str {
        self.text
    }

    /// Each marked token wholly inside the item's text: its start in the item's text and its
    /// mark, in ascending start (the column's rows are sorted and do not overlap).
    pub fn marked(&self) -> impl Iterator<Item = (u32, TextMark)> + '_ {
        let (from, len) = (self.offset, u32::try_from(self.text.len()).unwrap_or(u32::MAX));
        let end = from.saturating_add(len);
        let column = self.marks.column;
        // The first row at or after the item's start.
        let (mut lo, mut hi) = (0usize, column.len());
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if column.row(mid).start < from {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        (lo..column.len()).map(move |i| column.row(i)).take_while(move |row| row.end <= end).filter_map(move |row| row.mark.map(|mark| (row.start - from, mark)))
    }
}

/// The marks' column of `text`, a copy of an expansion's text out of a lowered body (blaze-ladder
/// front2 locals.rs: a local item a macro call inside the body wrote, whose text has no column
/// of its own), from `marked` ([`crate::View::marks_in`]: each marked word's start in `text` and
/// its mark, ascending), written in the column's bytes (blaze-expand `OriginColumn::write`) for
/// `OriginsRef::open`. Only marks are read from it ([`ItemMarks::marked`], blaze-scope
/// `TextMarks::mark_of`); a copy has no place in its call's text, so each row's origin is
/// `TextOrigin::Made`. A row ends where its word does: `$crate`, `r#name`, a lifetime or a name
/// (identifier characters), else one character; never past the next row's start.
pub fn column_of_copy<'a>(
    text: &str,
    marked: &[(u32, TextMark)],
    alloc: blaze_own::Alloc<'a>,
) -> Result<blaze_own::AVec<'a, u8>, blaze_expand::origin_column::ColumnError> {
    let mut column = blaze_expand::origin_column::OriginColumn::new_in(alloc);
    for (i, &(start, mark)) in marked.iter().enumerate() {
        let rest = text.get(start as usize..).unwrap_or("");
        let lead = ["$", "r#", "'"].iter().find(|p| rest.starts_with(**p)).map_or(0, |p| p.len());
        let word = lead + rest[lead..].bytes().take_while(|b| b.is_ascii_alphanumeric() || *b == b'_').count();
        let word = if word == 0 { rest.chars().next().map_or(0, char::len_utf8) } else { word };
        let next = marked.get(i + 1).map_or(text.len(), |n| n.0 as usize);
        let end = (start as usize + word).min(next).min(text.len());
        column.push(start as usize, end, blaze_expand::origin_column::TextOrigin::Made, Some(mark))?;
    }
    let mut bytes = blaze_own::AVec::new_in(alloc);
    column.write(&mut bytes)?;
    Ok(bytes)
}

/// Where a path is read: the module its first segment is looked up in, and the crate its
/// `$crate` names when its first segment is `$crate`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PathSite {
    pub module: ItemKey,
    /// `Some` when the root is a definition's crate: `$crate`, or an opaque mark's `crate`
    /// (blaze-scope `crate_roots::root_by_mark`).
    pub dollar_crate: Option<CrateItemKey>,
}

/// A path whose mark gives it no site.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SiteRefused {
    /// `$crate` on a token no expansion wrote: the body's own text, or a call's input.
    UnmarkedDollarCrate,
    /// A relative path under an opaque mark whose definition records no module (a mark no
    /// `macro` item made): where its names are is not known, and the call's module is not
    /// taken in its place.
    OpaqueWithoutModule { krate: CrateItemKey },
}

impl core::fmt::Display for SiteRefused {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            SiteRefused::UnmarkedDollarCrate => f.write_str("`$crate` outside an expansion (no mark names its crate)"),
            SiteRefused::OpaqueWithoutModule { krate } => {
                write!(f, "a relative path under a def-site mark of crate {krate:?} whose definition names no module (drive.md, \"A token keeps its mark\")")
            }
        }
    }
}

/// Where a path is resolved: `mark` is its FIRST segment's token's mark (`None` for a token
/// of the text as written), `first` that segment's text, `absolute` whether it is written
/// with a leading `::` (or the lowering's `{{root}}`), `call` the module the body is in.
pub fn site_of(mark: Option<TextMark>, first: &str, absolute: bool, call: ItemKey) -> Result<PathSite, SiteRefused> {
    // A body has no unmarked `$crate` (blaze-expand marks every token it writes); a crate root
    // is read by blaze-scope's one resolver (token-mark-2): `$crate`, and a `crate` an
    // expansion's item text wrote for another crate's `$crate` (opaque, `root_mark`).
    if !absolute && first == "$crate" && mark.is_none() {
        return Err(SiteRefused::UnmarkedDollarCrate);
    }
    let dollar_crate = match absolute {
        true => None,
        false => match blaze_scope::crate_roots::root_by_mark(first, mark) {
            blaze_scope::crate_roots::Root::Crate(k) => Some(k),
            blaze_scope::crate_roots::Root::Here | blaze_scope::crate_roots::Root::NotARoot => None,
        },
    };
    let dollar = dollar_crate.is_some();
    let module = match mark {
        Some(m) if m.transparency == Transparency::Opaque => match m.site.module {
            Some(module) => module,
            // A global path and `$crate` start at a crate, never at a module's names: the
            // module is read only for the crate it is in, the call's (the edition's extern
            // prelude), which is the body's.
            None if absolute || dollar => call,
            None => return Err(SiteRefused::OpaqueWithoutModule { krate: m.site.krate }),
        },
        _ => call,
    };
    Ok(PathSite { module, dollar_crate })
}

#[cfg(test)]
mod tests {
    use blaze_expand::{DefSite, Edition};
    use blaze_surface::ContentId;

    use super::*;

    fn key(b: u8) -> ItemKey {
        ItemKey(ContentId([b; 32]))
    }

    fn mark(transparency: Transparency, module: Option<ItemKey>) -> TextMark {
        TextMark { site: DefSite { krate: CrateItemKey(ContentId([9; 32])), module }, transparency, edition: Edition::E2021 }
    }

    /// The Reference, "Macros By Example", Hygiene: a `macro_rules!` (semi-opaque) writes an
    /// item name resolved at the call, a `macro` (opaque) one resolved at its definition, and
    /// `$crate` names the definition's crate under either. Expected: `test
    /// mark_site::tests::the_site_by_the_mark ... ok`.
    #[test]
    fn the_site_by_the_mark() {
        let (call, def) = (key(1), key(2));
        let rules = mark(Transparency::SemiOpaque, None);
        let item = mark(Transparency::Opaque, Some(def));
        assert_eq!(site_of(Some(rules), "only_dep", false, call), Ok(PathSite { module: call, dollar_crate: None }));
        assert_eq!(site_of(Some(item), "sel", false, call), Ok(PathSite { module: def, dollar_crate: None }));
        assert_eq!(site_of(None, "f", false, call), Ok(PathSite { module: call, dollar_crate: None }));
        let krate = Some(CrateItemKey(ContentId([9; 32])));
        assert_eq!(site_of(Some(rules), "$crate", false, call), Ok(PathSite { module: call, dollar_crate: krate }));
        assert_eq!(site_of(Some(item), "$crate", false, call), Ok(PathSite { module: def, dollar_crate: krate }));
        assert_eq!(site_of(None, "$crate", false, call), Err(SiteRefused::UnmarkedDollarCrate));
        let made = mark(Transparency::Opaque, None);
        assert_eq!(site_of(Some(made), "f", false, call), Err(SiteRefused::OpaqueWithoutModule { krate: CrateItemKey(ContentId([9; 32])) }));
        assert_eq!(site_of(Some(made), "core", true, call), Ok(PathSite { module: call, dollar_crate: None }));
    }
}
