//! A macro call of the project's own code whose expansion FAILED (no `macro_rules!` rule took
//! its input, `compile_error!`, a proc macro's error; D104's implicit `#[::std::main]` over a
//! root `fn main` no arm of `std::main` takes): an ERROR finding of the crate's names record at
//! the call, with the matcher's own reason (implicit-main-reason; rules 19 and 81). Before it,
//! `blaze check` said nothing: the call was noted and counted, never shown, and a `main` it
//! did not write was silence on `check` (only `run` failed, later, with "no `main`").
//!
//! The finding is placed item-relative (rule 71) like an unresolved path (names.rs
//! [`super::UNRESOLVED`]'s message form: file, item chain, edition, then the words), so a body
//! edit elsewhere moves no byte of it and the printer turns it into `file:line` only when it
//! prints. The range is the word the call is about inside its item: `main` for the implicit
//! call (its input is the whole item), else the macro's last path segment from the call's
//! place. The words are two pieces: the call (`the implicit `#[::std::main]` over `fn main``,
//! or `the call of `make``) and the reason, [`blaze_expand::Failed`]'s own words through
//! blaze-scope's note ("no rule of the macro's 1 matched; the closest, rule 1 of 1, expected ..
//! and found .. (token N of the call's input)").
//!
//! A dependency's failed calls are not the project's to fix and are not listed (as with
//! unresolved paths); the crate's names record still carries every one (blaze-scope
//! names_record.rs, the failed-calls section) for a reader that looks a name up behind it.

use std::alloc::Global;

use blaze_scope::resolve::{CrateNames, ExpandNote, NamesNote};

use super::{Unresolved, find_word};

/// The rule of a macro call in the project's own code whose expansion failed. An error:
/// blaze-report's words, number 22, "macro-call-failed".
pub(crate) const FAILED_CALL: u16 = 22;

/// The findings of crate `c`'s failed calls in its own files, sorted by (message, range). A call
/// whose file or item is not found is placed at the crate (an empty file piece), never dropped.
pub(super) fn findings(c: &CrateNames<'_>) -> blaze_own::Vec<Unresolved, Global> {
    let mut own = blaze_own::Vec::new_in(Global);
    if !c.files.iter().any(|file| file.path.starts_with("/0/")) {
        return own;
    }
    for note in c.notes.iter() {
        let NamesNote::Expand { at, note: ExpandNote::Failed { path, implicit, failed }, .. } = note else { continue };
        let (path, implicit) = (*path, *implicit);
        let mut message = blaze_own::Vec::new_in(Global);
        let mut range = (0u32, 0u32);
        let file = at.and_then(|loc| c.files.get(loc.file as usize).map(|file| (loc, *file)));
        match file.and_then(|(loc, file)| Some((loc, file, crate::work::item_holding(&file.outline, loc.offset)?))) {
            Some((loc, file, item)) => {
                let text = file.text;
                let entry = item.range.of(text);
                let from = loc.offset.saturating_sub(item.range.start) as usize;
                let (word, search_from) = match implicit {
                    true => ("main", 0),
                    false => (path.rsplit("::").next().unwrap_or(path), from),
                };
                range = match find_word(entry, word, search_from) {
                    Some(start) => (start as u32, (start + word.len()) as u32),
                    None => (from as u32, from as u32),
                };
                message.extend_from_slice(file.path.trim_start_matches("/0/").as_bytes());
                message.push(0x1F);
                let chain = crate::work::item_chain(&file.outline, item, text);
                for shift in (0..16).rev() {
                    message.push(b"0123456789abcdef"[((chain >> (shift * 4)) & 0xF) as usize]);
                }
            }
            None => message.push(0x1F),
        }
        message.push(0x1F);
        message.push(b'0' + c.edition.byte());
        message.push(0x1F);
        let call = match implicit {
            true => std::format!("the implicit `#[{path}]` over `fn main` (D104: a blaze-edition binary's root `fn main` is its input, the whole item)"),
            false => std::format!("the call of `{path}`"),
        };
        message.extend_from_slice(call.as_bytes());
        message.push(0x1F);
        message.extend_from_slice(std::format!("{failed}").as_bytes());
        own.push(Unresolved { at: range, message });
    }
    // The notes are in the solve's order; the record holds them sorted (rule 25).
    own.sort_unstable_by(|a, b| (&a.message[..], a.at).cmp(&(&b.message[..], b.at)));
    own
}
