//! Decision D111 for a body's proc-macro calls (the gap the D96 lane named: the body host
//! recorded a member's read outside its package and gave no finding). A body's host is not
//! told which call runs a macro (blaze-expand's `ExpandCx`), so the finding is made after the
//! body is lowered, from what the expander kept: each call's range, its path as written, its
//! shape, and the span of the body's read list its expansion joined (blaze-lower
//! `call_reads`). Each call is filed through [`BodySources::file_left_reads`], whose one
//! implementation ([`super::body_host::Sourced`]) is blaze-scope's one filing,
//! `blaze_scope::left_reads::file_left_reads`, the item host's own (rule 30): a body call and
//! the same call at item level give the same finding, byte for byte.
//!
//! The place is the call's byte in its file (the body's item's first byte plus the call's
//! offset in the body's text), which the sink makes item-relative (rule 71; blaze-cli
//! names/include_outside.rs). A call inside an expansion appended to the body's text, or a
//! body's attribute or derive (the lowering gives no range), is placed at the body's item; a
//! body with no file (its text is a macro's expansion) is placed at the crate: never dropped.

use blaze_expand::Reads;
use blaze_id::ItemKey as LibKey;
use blaze_lower::call_reads::CallReads;
use blaze_own::Alloc;
use blaze_scope::left_reads::CallAt;
use blaze_scope::resolve::At;

use super::body_host::BodySources;
use super::source::NamesSource;

/// Where a body's text lies in its file: the file's path (the names' path space), the
/// body's item's first byte there, and the text's length.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BodyPlace<'p> {
    pub from: &'p str,
    pub start: u32,
    pub len: u32,
}

/// The place of the body of `item` (crate `krate`) whose text is `len` bytes long. `None`:
/// the item's text is a macro's expansion, or the names give it no file.
pub fn body_place<'n, S: NamesSource<'n>>(names: S, krate: usize, item: LibKey, len: usize) -> Option<BodyPlace<'n>> {
    let solved = names.solved()?;
    let place = names.place_of(item)?;
    if place.krate != krate {
        return None;
    }
    let At::File(file) = place.at else { return None };
    let (at, start) = names.item_start(item)?;
    if at != place.at {
        return None;
    }
    let from = solved.krate(krate).files.get(file as usize)?.path;
    Some(BodyPlace { from, start, len: u32::try_from(len).ok()? })
}

/// Files decision D111's findings of a body's calls (`calls`, over the body's read list
/// `reads`), each at its place in `body`: see the module's doc. Returns how many were filed.
pub fn file_body_left_reads(sources: &dyn BodySources, krate: usize, body: Option<BodyPlace<'_>>, calls: &[CallReads<'_>], reads: &Reads<'_>, alloc: Alloc<'_>) -> u32 {
    let mut filed = 0u32;
    for c in calls {
        let (from, at) = match body {
            Some(b) => match c.at {
                Some(range) if range.start < b.len => (b.from, b.start + range.start),
                _ => (b.from, b.start),
            },
            None => ("", 0),
        };
        let call = CallAt { from, at, call: c.path.as_str(), shape: c.shape };
        filed += sources.file_left_reads(krate, call, reads, c.reads.clone(), alloc);
    }
    filed
}
