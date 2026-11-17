//! ITEMS `blaze check` could not check, named by cause: the item half of not_checked.rs (one
//! mechanism, rule 30: the same "not checked because <cause>: .." line, the same cap
//! [`CRATES_SHOWN`] a cause with the exact count past it).
//!
//! The ruling (main, via the front-end coordinator, 2026-11-16; rule 3 and the owner's "`blaze
//! check` must really check"): an item the check could not check FAILS the command, and is not
//! an error in the user's code:
//! - its own exit status, main.rs `UNCHECKED_ITEMS` (an error found stays 1, and wins when
//!   both are there);
//! - its own summary line, [`summary`]: "the check is incomplete: blaze could not check N
//!   items", printed beside the closing line, never "0 errors" beside it;
//! - each item named with its cause, grouped by cause ([`lines`]):
//!   "not checked because front2 lists no body for it: `outer` (src/main.rs), .. (and N more)".
//!
//! THE FORM a finding carries (body_types.rs `TYPES_NOT_CHECKED`, three arguments between 0x1F
//! bytes, blaze-report words.rs Cli 18): the item's path in its file ([`item_path`]), its
//! CAUSE (a fixed phrase: what groups items), and a detail (`""`, or `"; "` and the words that
//! name the one case: the refusal's debug, the record's error). [`file`] writes it; [`keep`]
//! reads it back where print.rs counts it. Nothing else writes or reads that form.

use core::alloc::Allocator;

use blaze_file_items::Outline;
use blaze_own::Vec;
use blaze_surface::Item;
use bstd::mem::Frame;

use super::CRATES_SHOWN;
use crate::CliError;
use crate::work::Record;

/// One item the check could not check, as print.rs kept it from its finding.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Unchecked<'f> {
    /// What groups it: sorted first, so a cause's items are together.
    pub(crate) cause: &'f str,
    /// Its file, as the command prints it.
    pub(crate) file: &'f str,
    /// Its path in the file ([`item_path`]).
    pub(crate) item: &'f str,
}

/// Files that the item `item` was not checked because of `cause` (a fixed phrase), with
/// `detail` naming the one case (empty for none), into `made` at `range`.
pub(crate) fn file<A: Allocator + Copy>(made: &mut Record<A>, range: (u32, u32), item: &str, cause: &'static str, detail: &str) {
    let sep = if detail.is_empty() { "" } else { "; " };
    made.find(crate::body_types::TYPES_NOT_CHECKED, range, &[item, "\x1f", cause, "\x1f", sep, detail]);
}

/// The item and cause of a `TYPES_NOT_CHECKED` finding's words ([`file`]'s form), copied into
/// `frame` and pushed onto `into` with the finding's file. A finding not of that form is a
/// typed refusal naming the rule (rule 3): it is never counted as some other item.
pub(crate) fn keep<'f>(into: &mut Vec<Unchecked<'f>, &'f Frame>, frame: &'f Frame, file: Option<&str>, words: &[u8]) -> Result<(), CliError> {
    let refused = CliError::Text { what: "a types-not-checked finding without its item and cause (blaze-cli not_checked/items.rs `file`)" };
    let mut parts = words.split(|b| *b == 0x1f);
    let (Some(item), Some(cause), Some(_detail), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else { return Err(refused) };
    let (Ok(item), Ok(cause)) = (core::str::from_utf8(item), core::str::from_utf8(cause)) else { return Err(refused) };
    let file = file.unwrap_or("<no file>");
    into.push(Unchecked { cause: crate::work::held_str(cause, frame), file: crate::work::held_str(file, frame), item: crate::work::held_str(item, frame) });
    Ok(())
}

/// Each cause's line over `items`, in cause order, handed to `each` (the caller prints it as a
/// note): "not checked because <cause>: `a` (file), `b` (file), .. (and N more)", at most
/// [`CRATES_SHOWN`] items named a cause. Returns how many distinct items were not checked: the
/// [`summary`]'s count. An item two owners of its file (a library and its test unit) both
/// could not check is one item.
pub(crate) fn lines(items: &[Unchecked<'_>], frame: &Frame, each: &mut dyn FnMut(&str) -> Result<(), CliError>) -> Result<u32, CliError> {
    let mut sorted: Vec<Unchecked<'_>, &Frame> = Vec::with_capacity_in(items.len(), frame);
    sorted.extend(items.iter().copied());
    sorted.sort_unstable();
    sorted.dedup();
    // Distinct items, whatever their causes: (file, item) once.
    let mut distinct: Vec<(&str, &str), &Frame> = Vec::with_capacity_in(sorted.len(), frame);
    distinct.extend(sorted.iter().map(|u| (u.file, u.item)));
    distinct.sort_unstable();
    distinct.dedup();
    let text = |_: core::fmt::Error| CliError::Text { what: "a not-checked line's words" };
    let mut at = 0usize;
    while at < sorted.len() {
        let cause = sorted[at].cause;
        let end = sorted[at..].iter().position(|u| u.cause != cause).map_or(sorted.len(), |n| at + n);
        let mut words = bstd::mem::String::new_in(frame);
        core::fmt::write(&mut words, format_args!("not checked because {cause}: ")).map_err(text)?;
        for (i, u) in sorted[at..end].iter().take(CRATES_SHOWN).enumerate() {
            if i > 0 {
                words.push_str(", ");
            }
            core::fmt::write(&mut words, format_args!("`{}` ({})", u.item, u.file)).map_err(text)?;
        }
        if end - at > CRATES_SHOWN {
            core::fmt::write(&mut words, format_args!(" (and {} more)", end - at - CRATES_SHOWN)).map_err(text)?;
        }
        each(words.as_str())?;
        at = end;
    }
    u32::try_from(distinct.len()).map_err(|_| CliError::Text { what: "more than 2^32 items not checked" })
}

/// The summary line of a check that could not check `n` items (`n` > 0).
pub(crate) fn summary(n: u32, frame: &Frame) -> bstd::mem::String<&Frame> {
    bstd::mem::format_in!(frame, "the check is incomplete: blaze could not check {n} {}", if n == 1 { "item" } else { "items" })
}

/// The path of the outline entry `item` in its file: each enclosing entry's name, outermost
/// first, then its own (`outer`, `Shape::{impl}::area`); an entry with no name (an impl, a
/// block) is `{impl}`. A work list up the parents, no recursion.
pub(crate) fn item_path<'f>(outline: &Outline<'_>, item: &Item, text: &str, frame: &'f Frame) -> bstd::mem::String<&'f Frame> {
    let all = outline.items();
    let mut chain: Vec<&Item, &Frame> = Vec::new_in(frame);
    chain.push(item);
    let mut up = item.parent;
    // An outline's parents point outward and end: at most its entries' count of steps.
    let mut steps = 0usize;
    while let Some(parent) = up {
        let Some(entry) = all.get(parent.0 as usize) else { break };
        steps += 1;
        if steps > all.len() {
            break;
        }
        chain.push(entry);
        up = entry.parent;
    }
    let mut out = bstd::mem::String::new_in(frame);
    for (i, entry) in chain.iter().rev().enumerate() {
        if i > 0 {
            out.push_str("::");
        }
        match entry.name {
            Some(_) => out.push_str(crate::item_name::shown_name(entry, text)),
            None => out.push_str("{impl}"),
        }
    }
    out
}
