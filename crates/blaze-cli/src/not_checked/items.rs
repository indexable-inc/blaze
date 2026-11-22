//! ITEMS `blaze check` could not check, named by cause: the item half of not_checked.rs (one
//! mechanism, rule 30: the same "not checked because <cause>: .." line, the same cap
//! `CRATES_SHOWN` a cause with the exact count past it).
//!
//! The ruling (main, via the front-end coordinator, 2026-11-16; rule 3 and the owner's "`blaze
//! check` must really check"): an item the check could not check FAILS the command, and is not
//! an error in the user's code:
//! - its own exit status, main.rs `UNCHECKED_ITEMS` (an error found stays 1, and wins when
//!   both are there);
//! - its own summary line, [`summary`]: "the check is incomplete: blaze could not check N
//!   items", printed beside the closing line, never "0 errors" beside it;
//! - each item named with its cause, grouped by cause:
//!   "not checked because front2 lists no body for it: `outer` (src/main.rs), .. (and N more)".
//!
//! THE FORM a finding carries (body_types.rs `TYPES_NOT_CHECKED`, three arguments between 0x1F
//! bytes, blaze-report words.rs Cli 18): the item's path in its file, its
//! CAUSE (a fixed phrase: what groups items), and a detail (`""`, or `"; "` and the words that
//! name the one case: the refusal's debug, the record's error). [`keep`] reads it back where
//! print.rs counts it.

use blaze_own::Vec;
use bstd::mem::Frame;

use crate::CliError;

/// One item the check could not check, as print.rs kept it from its finding.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Unchecked<'f> {
    /// What groups it: sorted first, so a cause's items are together.
    pub(crate) cause: &'f str,
    /// Its file, as the command prints it.
    pub(crate) file: &'f str,
    /// Its path in the file.
    pub(crate) item: &'f str,
}

/// The item and cause of a `TYPES_NOT_CHECKED` finding's words, borrowed from
/// the listed finding (its words and path live as long as the list, `'f`) and pushed onto
/// `into` with the finding's file: nothing is copied. A finding not of that form is a typed
/// refusal naming the rule (rule 3): it is never counted as some other item.
pub(crate) fn keep<'f>(into: &mut Vec<Unchecked<'f>, &'f Frame>, file: Option<&'f str>, words: &'f [u8]) -> Result<(), CliError> {
    let refused = CliError::Text { what: "a types-not-checked finding without its item and cause (blaze-cli not_checked/items.rs `keep`)" };
    let mut parts = words.split(|b| *b == 0x1f);
    let (Some(item), Some(cause), Some(_detail), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else { return Err(refused) };
    let (Ok(item), Ok(cause)) = (core::str::from_utf8(item), core::str::from_utf8(cause)) else { return Err(refused) };
    let file = file.unwrap_or("<no file>");
    into.try_reserve(1).map_err(|_| CliError::Text { what: "the command's frame has no room for one more item not checked (blaze-cli not_checked/items.rs `keep`)" })?;
    into.push(Unchecked { cause, file, item });
    Ok(())
}

/// The summary line of a check that could not check some items: `own` for the causes named
/// above it (the causes named by tasks not run), `behind` because a task failed (the
/// lines under the failures). Every item not checked is in its count, so it never reads better
/// than the truth: tempo's said 1,061 while 28,678 more were not checked under one failure.
/// `None` when every item was checked.
pub(crate) fn summary(own: u32, behind: super::Behind, frame: &Frame) -> Option<bstd::mem::String<&Frame>> {
    let items = |n: u32| if n == 1 { "item" } else { "items" };
    let all = own.saturating_add(behind.items);
    if all == 0 {
        return None;
    }
    let mut words = bstd::mem::format_in!(frame, "the check is incomplete: blaze could not check {all} {}", items(all));
    if behind.items > 0 {
        let failures = if behind.failures == 1 { "failure" } else { "failures" };
        let more = bstd::mem::format_in!(frame, ": {} behind {} {failures}", behind.items, behind.failures);
        words.push_str(more.as_str());
        if own > 0 {
            let more = bstd::mem::format_in!(frame, ", {own} for the reasons above");
            words.push_str(more.as_str());
        }
    }
    Some(words)
}
