//! What an impl candidate's refused header names, as written (rule 19: an error names the
//! thing). A candidate impl whose header did not lower is filed with its error's text
//! ([`super::buckets::Buckets::fail`]); a path that did not resolve is reported by the names
//! library as a segment index and a range of the impl's text, which names nothing a reader can
//! find (census 14: "impl 4bc8979360898bd0 .. Resolve(Path(PathError { .. at: 0, why:
//! NotFound }), Range { start: 30, end: 57 })" was libc's `impl ::core::default::Default for
//! stat`). So the text at that range is appended after [`WRITTEN`], when the range lies in the
//! impl's header: the header is lowered over the impl's whole text (heads.rs `impl_`), so a
//! header range is a range of that text; a member's (an associated type's) range is of the
//! member's own text and is said to be outside the header, never read from the impl's text.

use std::boxed::Box;
use std::format;

use blaze_id::ItemKey as LibKey;

use super::heads::HeadError;
use super::item_shown::{ItemShown, head_error_shown};
use super::source::NamesSource;
use super::types::Why;

/// What comes before the written text in a failed header's line: the census
/// (blaze-real examples/tempo_front.rs `gap_class`) splits the class at it.
pub const WRITTEN: &str = "; the header's path as written: ";

/// What comes before the range when the range is not in the impl's header.
pub const NOT_IN_HEADER: &str = "; the path's range is not in the impl's header: ";

/// The line a candidate impl `k` whose header failed with `e` is filed with, each item by its
/// path ([`ItemShown`], rule 19): `item <k>: <why>` when `k`'s own header failed; `the head of
/// <item>, needed by the header of <k>: <why>` when a head `k`'s header needs failed (both
/// lines end their naming part at the first `"): "`, where the census cuts). Then, for a path
/// that did not resolve in an impl's header (`k`'s or the needed impl's), the path as that
/// header writes it ([`WRITTEN`]) or why it cannot be shown ([`NOT_IN_HEADER`]); in a needed
/// item that is no impl, the range of its text.
pub fn head_failed<'n, S: NamesSource<'n>>(names: S, k: LibKey, e: &HeadError) -> Box<str> {
    let HeadError::Item(item, why) = e else { return head_error_shown(names, e).into_boxed_str() };
    let mut out = if *item == k {
        format!("item {}: {why:?}", ItemShown::of_lib(names, k))
    } else {
        format!("the head of {}, needed by the header of {}: {why:?}", ItemShown::of_lib(names, *item), ItemShown::of_lib(names, k))
    };
    let Why::Resolve(_, r) = why else { return out.into_boxed_str() };
    if *item != k && names.signature_text_of(*item).is_none_or(|t| t.impl_header.is_none()) {
        out.push_str(&format!("; at {}..{} of its text", r.start, r.end));
        return out.into_boxed_str();
    }
    match written_in_header(names, *item, r.start, r.end) {
        Ok(path) => {
            out.push_str(WRITTEN);
            out.push('`');
            out.push_str(path);
            out.push('`');
        }
        Err(why) => {
            out.push_str(NOT_IN_HEADER);
            out.push_str(&format!("{}..{}: {why}", r.start, r.end));
        }
    }
    out.into_boxed_str()
}

/// The text at `start..end` of impl `k`'s text, when that range is inside its header.
fn written_in_header<'n, S: NamesSource<'n>>(names: S, k: LibKey, start: u32, end: u32) -> Result<&'n str, &'static str> {
    let it = names.signature_text_of(k).ok_or("the impl has no text")?;
    let header = it.impl_header.ok_or("the impl's row has no header text")?;
    let at = header_at(it.text, header).ok_or("the header's text is not in the impl's text")?;
    let (start, end) = (start as usize, end as usize);
    if start >= end || start < at || end > at + header.len() {
        return Err("a member's range, or one past the header");
    }
    it.text.get(start..end).ok_or("the range does not cut the impl's text at characters")
}

/// Where `header` starts in `text`: where it lies in the same buffer (the record's header span
/// is inside the item's span), else its first occurrence; `None` when it is not there at all.
fn header_at(text: &str, header: &str) -> Option<usize> {
    let same = (header.as_ptr() as usize).checked_sub(text.as_ptr() as usize);
    if let Some(off) = same {
        if text.get(off..off + header.len()) == Some(header) {
            return Some(off);
        }
    }
    text.find(header)
}

/// The text of a census class: `why` cut at [`WRITTEN`] into the reason and the written path.
pub fn split_written(why: &str) -> (&str, Option<&str>) {
    match why.rsplit_once(WRITTEN) {
        Some((reason, path)) => (reason, Some(path)),
        None => (why, None),
    }
}
