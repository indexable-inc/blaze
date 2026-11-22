//! A `mod x;` of the project's own code whose file is at neither of its places (`x.rs`,
//! `x/mod.rs`), or whose `#[path]` names no file of the checkout: an ERROR finding of the
//! crate's names record at the `mod` item (rustc's E0583; rules 19 and 81). The module is empty
//! for everything else, and the finding goes when the file appears (the walk's look for it is
//! the record's absent read, names/scope.rs `absent`).
//!
//! The crate's walk decides it (blaze-scope task/lower.rs `walk`): a cfg'd-out `mod` is skipped
//! before its file is looked for, so `#[cfg(test)] mod t;` without `t.rs` is no finding of the
//! library, only of its test build. Placed item-relative like a failed call
//! ([`super::failed_call`], rule 71): the file, the `mod` item's chain, the edition, then the
//! rule's arguments (blaze-report's Drive words 0xC0F7 "this module's file is at neither of its
//! places: {0} or {1}", 0xC0F8 "this module's `#[path]` names {0}"), checkout-relative. The
//! range is `mod x` in the item, as rustc marks it.
//!
//! The library and its test build are two names records; equal findings of both are one printed
//! line (engine_lines.rs, the `seen` fold).

use blaze_own::SharedAlloc;
use blaze_scope::resolve::CrateNames;
use blaze_scope::task::WalkNote;

/// One module file the walk did not find, as its record's finding: the rule
/// ([`blaze_drive::task::MODULE_FILE_MISSING_RULE`] or
/// [`blaze_drive::task::PATH_FILE_MISSING_RULE`]), the range from the `mod` item's start and
/// the placed message ([`super::place_pieces`]).
pub(crate) struct ModuleMissing {
    pub(crate) rule: u16,
    pub(crate) at: (u32, u32),
    pub(crate) message: blaze_own::Vec<u8, SharedAlloc<'static>>,
}

/// The findings of crate `c`'s module files the walk did not find, in its own files (a
/// dependency's are not the project's to fix, as with failed calls), held on `kept` (they leave
/// the worker with the crate's record), sorted by (rule, message, range) (rule 25).
pub(crate) fn findings(c: &CrateNames<'_>, kept: SharedAlloc<'static>) -> blaze_own::Vec<ModuleMissing, SharedAlloc<'static>> {
    let mut own = blaze_own::Vec::new_in(kept);
    for note in c.tree.notes.iter() {
        let (rule, at, places): (u16, _, [&str; 2]) = match note {
            WalkNote::NeitherFile { at, rs, mod_rs, .. } => (blaze_drive::task::MODULE_FILE_MISSING_RULE, *at, [rs.as_str(), mod_rs.as_str()]),
            WalkNote::PathFileNotFound { at, path } => (blaze_drive::task::PATH_FILE_MISSING_RULE, *at, [path.as_str(), ""]),
            // Not a missing file: the walk's own limits and choices, each its own matter.
            WalkNote::BothFiles { .. } | WalkNote::TooManyModules { .. } | WalkNote::PathValueRefused { .. } => continue,
        };
        let Some(file) = c.files.get(at.file as usize).copied() else { continue };
        if !file.path.starts_with("/0/") {
            continue;
        }
        let mut message = blaze_own::Vec::new_in(kept);
        let mut range = (0u32, 0u32);
        match crate::work::item_holding(&file.outline, at.offset) {
            Some(item) => {
                let from = at.offset.saturating_sub(item.range.start);
                // `mod x`: from the keyword before the name to the name's end (the name's range
                // is entry-relative); an item with no name is marked at its start.
                let entry = item.range.of(file.text);
                range = match item.name {
                    Some(name) => {
                        let head = entry.get(..name.start as usize).map_or("", str::trim_end);
                        let start = match head.strip_suffix("mod") {
                            Some(before) => before.len() as u32,
                            None => name.start,
                        };
                        (start, name.end)
                    }
                    None => (from, from),
                };
                message.extend_from_slice(file.path.trim_start_matches("/0/").as_bytes());
                message.push(0x1F);
                let chain = crate::work::item_chain(&file.outline, item, file.text);
                for shift in (0..16).rev() {
                    message.push(b"0123456789abcdef"[((chain >> (shift * 4)) & 0xF) as usize]);
                }
            }
            None => message.push(0x1F),
        }
        message.push(0x1F);
        message.push(b'0' + c.edition.byte());
        for place in places.iter().filter(|p| !p.is_empty()) {
            message.push(0x1F);
            message.extend_from_slice(place.trim_start_matches("/0/").as_bytes());
        }
        own.push(ModuleMissing { rule, at: range, message });
    }
    own.sort_unstable_by(|a, b| (a.rule, &a.message[..], a.at).cmp(&(b.rule, &b.message[..], b.at)));
    own
}
