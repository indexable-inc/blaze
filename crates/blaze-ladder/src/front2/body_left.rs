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

use core::cell::LazyCell;

use blaze_expand::Reads;
use blaze_id::ItemKey as LibKey;
use blaze_lower::call_reads::CallReads;
use blaze_own::{AString, Alloc};
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

/// Decision D111 for a body's includes: each `include!`, `include_str!` or `include_bytes!`
/// read in `calls`' spans of `reads` whose file, placed from `calling` (the body's file,
/// absolute under the package, as the body host places it: body_host.rs `BodyHost::include`)
/// by the one `blaze_scope::include_place::include_place`, lies outside the member's own
/// package, is told to `sources` at the call, as the item host tells its own (blaze-scope
/// resolve.rs `Host::include`). The subject of such a read is the path as written (blaze-expand
/// cx.rs `Recorder::include`: the body host answers `Included::Bytes` or `Missing`); a path the
/// placing refuses was the expansion's `Outside` error, no read to tell. Returns how many.
///
/// `calling` is asked at the first include read alone, as the body host asks its own file
/// (body_host.rs `BodyHost::file`, a `LazyCell`): over records the file's row is read through
/// the crate's items section WHOLE (body_host.rs `body_file`, tables view.rs `file_paths`),
/// which any item added to the crate moves. Asked of every body, it entered every body of a
/// crate on any item edit (blaze-cli engine_edit_rows_tests R6, run t82 2026-11-21: Bodies
/// "read moved" CrateScope 8 + 6 after a private fn was added). So a body with no include
/// reads here nothing its host did not; one with an include reads what its host already read.
/// `None`: the body has no file, and its host refused every include (`HostAsk::Include`):
/// nothing to tell.
pub fn file_body_left_includes<'c>(
    sources: &dyn BodySources, krate: usize, body: Option<BodyPlace<'_>>, calling: impl FnOnce() -> Option<AString<'c>>, calls: &[CallReads<'_>], reads: &Reads<'_>,
    alloc: Alloc<'_>,
) -> u32 {
    use blaze_expand::ReadKind;
    let Some(package) = sources.folder(krate) else { return 0 };
    let seen = sources.seen(krate);
    let calling = LazyCell::new(calling);
    let list = reads.list();
    let mut filed = 0u32;
    for c in calls {
        let (from, at) = match body {
            Some(b) => match c.at {
                Some(range) if range.start < b.len => (b.from, b.start + range.start),
                _ => (b.from, b.start),
            },
            None => ("", 0),
        };
        let span = c.reads.start.min(list.len())..c.reads.end.min(list.len());
        for read in &list[span] {
            match read.kind {
                ReadKind::IncludeStr | ReadKind::IncludeBytes => {}
                ReadKind::Env | ReadKind::Cfg | ReadKind::Line | ReadKind::Column | ReadKind::File | ReadKind::ModulePath | ReadKind::ProcMacro => continue,
            }
            let Some(calling) = calling.as_deref() else { return filed };
            let Some(place) = blaze_scope::include_place::include_place(package, seen, calling, reads.subject(read), alloc) else { continue };
            if place.outside_package {
                sources.left_package(krate, blaze_scope::include_place::LeftPackage { from, at, call: c.path.as_str(), shape: c.shape, path: &place.path });
                filed += 1;
            }
        }
    }
    filed
}
