//! The seam's per-fn rules that both programs answer by (rule 30: one home each):
//! [`crate::front2::program::Front2Program`], whose rows are made by a census, and
//! [`super::LazyProgram`], whose rows are made one body at a time. Each rule takes what a
//! program holds (its keys, its head store, its names source, the body being built) and a
//! question about a row as a closure, so the two cannot answer differently. Kept under `lazy`
//! (visible to all of `program`) so the lazy program's files hold it whole.
//!
//! - [`fn_abi`]: a tabled fn's ABI is its row's; any other fn's is its head's (head_abi.rs).
//! - [`track_caller`]: the fn's own `#[track_caller]`, else, for a method of a trait impl, its
//!   trait's declaration's (the Reference's `track_caller` attribute: on a trait's method
//!   declaration it applies to every implementation of the method).
//! - [`caller_site`]: an expression's place as a code record keeps it (rule 71): the item and
//!   the expression's first byte from the item's first, never a line.
//! - [`source_position`]: the line and column of such a place, made only when the program is
//!   placed, from the item's first byte TODAY in today's text of its file
//!   (`NamesSource::item_start`, `NamesSource::file_text`), so an edit above the item moves
//!   the answer and no record.
//!
//! An item is found by `Keys::library_in` (never `Keys::library`): over records the keys map
//! files no item (keys.rs), the names source finds it; over solved names both answer alike.

use super::*;

use crate::front2::keys::Keys;
use crate::front2::source::NamesSource;
use crate::seam::PositionGap;

/// A trait declaration supplies the call signature until dispatch selects an implementation.
/// For other Rust rows the builder needs a body or an intrinsic. The lazy row can count a
/// body declared in the head before checking it. Foreign symbols and shims have their own rows.
pub(in crate::front2::program) fn rust_row_kind(of_trait: bool, body_or_intrinsic: bool, path: &str) -> Result<FnKind, Front2Refusal> {
    if of_trait {
        Ok(FnKind::Trait(0))
    } else if body_or_intrinsic {
        Ok(FnKind::Free)
    } else {
        Err(Front2Refusal::NoBody { function: path.into() })
    }
}

/// THE ABI RULE: `tabled` is the row's ABI when `key` has a row; else the head's (head_abi.rs
/// `head_abi`). `None`: no head, or an ABI the builder spreads no arguments of (the builder's
/// `NoAbi` names the fn).
pub(in crate::front2::program) fn fn_abi<'n, S: NamesSource<'n>>(tabled: Option<Abi>, names: S, store: &Store<'n>, key: ItemKey) -> Option<Abi> {
    if let Some(abi) = tabled {
        return Some(abi);
    }
    crate::front2::program::head_abi::head_abi(names, store, key, || Box::from("")).ok().map(|read| read.abi)
}

/// THE TRACK-CALLER RULE: `own(k)` is fn `k`'s own `#[track_caller]` (`None`: the program
/// cannot answer for `k`). A fn that says it is tracked; else a method of a trait impl answers
/// with its trait's declaration of the method of its name; any other fn (a free fn, a trait's
/// own method, an inherent method) answers with its own attribute alone. `None` when the
/// declaration is not filed or `own` cannot answer for it: never guessed `false`.
pub(in crate::front2::program) fn track_caller(keys: &Keys, store: &Store<'_>, key: ItemKey, own: impl Fn(ItemKey) -> Option<bool>) -> Option<bool> {
    if own(key)? {
        return Some(true);
    }
    let Some(crate::front2::keys::Origin::Member { container, kind: MemberKind::Fn, name }) = keys.origin(key) else {
        return Some(false);
    };
    // A trait's own member, or an inherent impl's: its own attribute is the whole answer.
    let Some(trait_) = store.impls.get(container).and_then(|imp| imp.trait_) else {
        return Some(false);
    };
    let declared = keys.filed_member(trait_, MemberKind::Fn, name)?;
    own(declared)
}

/// THE CALLER-SITE RULE: expression `expr` of `owner`'s lowered `body` as a code record keeps
/// it: `owner`, the expression's first byte from the item's (`View::site`: or the outermost
/// macro call's range when the call is in an expansion), and the shown path of the file the
/// item is written in (`NamesSource::file_shown`). `None`: the expression has no range, or the
/// item is not written in a file of its crate (an expansion's item), or the source does not
/// answer.
pub(in crate::front2::program) fn caller_site<'n, S: NamesSource<'n>>(names: S, keys: &Keys, owner: ItemKey, body: &Body<'_>, expr: ExprId) -> Option<blaze_build::CallerSite<'n>> {
    let offset = expr_offset(body, expr)?;
    let lib = keys.library_in(names, owner)?;
    let place = names.place_of(lib)?;
    let blaze_scope::resolve::At::File(i) = place.at else { return None };
    let file = names.file_shown(place.krate, i)?;
    Some(blaze_build::CallerSite { file, item: owner, offset })
}

/// THE TRAP-SITE RULE: [`caller_site`]'s offset alone, `expr`'s first byte from its item's
/// (`View::site`), with no file: what a trap row keeps (blaze-build `Adts::expr_site`). It
/// needs no shown path, so a toolchain crate's body (core's `AlignmentEnum` copy) keeps its
/// trap's site; the report places it by the item. `None`: the expression has no range.
pub(in crate::front2::program) fn expr_offset(body: &Body<'_>, expr: ExprId) -> Option<u32> {
    let view = body.lowered.view();
    Some(view.site(view.expr_range(expr).ok()?).ok()?.start)
}

/// The file item `item`'s text is in, as the program observes it (`file!()`, as
/// [`caller_site`] gives it): what a fault's report names a trap's construct by, with
/// [`source_position`]'s line and column. `None` for an item written by a macro's expansion.
pub(in crate::front2::program) fn item_file<'n, S: NamesSource<'n>>(names: S, keys: &Keys, item: ItemKey) -> Option<&'n str> {
    let lib = keys.library_in(names, item)?;
    let place = names.place_of(lib)?;
    let blaze_scope::resolve::At::File(i) = place.at else { return None };
    names.file_shown(place.krate, i)
}

/// THE POSITION RULE: the line and column (each from 1; the column in chars) of byte `site` of
/// item `item`'s text: the item's first byte today (`NamesSource::item_start`) plus `site`, in
/// today's text of its file (`NamesSource::file_text`), counted there. What a `#[track_caller]`
/// `Location`'s line and column are when the program is placed: the code record holds only
/// `item` and `site` (rule 71).
pub(in crate::front2::program) fn source_position<'n, S: NamesSource<'n>>(names: S, keys: &Keys, item: ItemKey, site: u32) -> Result<(u32, u32), PositionGap> {
    let lib = keys.library_in(names, item).ok_or(PositionGap::NoItem)?;
    let (at, start) = names.item_start(lib).ok_or(PositionGap::NoItem)?;
    let blaze_scope::resolve::At::File(i) = at else { return Err(PositionGap::NoFile) };
    let krate = names.crate_of(lib).ok_or(PositionGap::NoItem)?;
    let text = names.file_text(krate, i).ok_or(PositionGap::NoFile)?;
    line_and_column(text, start, site)
}

/// Byte `start + site` of `text` as a line and a column, each from 1, the column in chars.
/// [`PositionGap::PastText`]: past the end, or not on a char's first byte.
fn line_and_column(text: &str, start: u32, site: u32) -> Result<(u32, u32), PositionGap> {
    let byte = start.checked_add(site).ok_or(PositionGap::PastText)? as usize;
    let before = text.get(..byte).ok_or(PositionGap::PastText)?;
    let line_start = before.rfind('\n').map_or(0, |n| n + 1);
    let lines = before.bytes().filter(|&b| b == b'\n').count();
    let chars = before[line_start..].chars().count();
    let line = u32::try_from(lines + 1).map_err(|_| PositionGap::PastText)?;
    let col = u32::try_from(chars + 1).map_err(|_| PositionGap::PastText)?;
    Ok((line, col))
}

#[cfg(test)]
mod tests {
    use super::line_and_column;
    use crate::seam::PositionGap;

    /// Lines and columns from 1, the column in chars: a 'é' (two bytes) before the site counts
    /// once; a site inside a char, or past the text, is `PastText`.
    #[test]
    fn a_site_is_a_line_and_a_column_in_chars() {
        let text = "fn a() {}\nfn é() { f() }\n";
        // The item `fn é` starts at byte 10; `f()` at byte 10 + 10 (two bytes of 'é').
        assert_eq!(line_and_column(text, 10, 0), Ok((2, 1)));
        assert_eq!(line_and_column(text, 10, 10), Ok((2, 10)));
        assert_eq!(line_and_column(text, 10, 4), Err(PositionGap::PastText));
        assert_eq!(line_and_column(text, 10, 100), Err(PositionGap::PastText));
        std::println!("positions: line 2, column 10 for `f()` after a two-byte char");
    }
}
