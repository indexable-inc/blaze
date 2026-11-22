//! TASKS that failed only because a task they need failed (work.rs `WAITED`): no error of their
//! own. The tempo check of 2026-11-17 04:51 printed one `error[task-failed] .. needs X, which
//! failed` line for each of 29,283 such tasks under 8 failures, and counted them as 30,017
//! errors; placing them was most of a no-op check's 601 ms of printing.
//!
//! Now a waited task is filed with one `WAITED` finding whose words are its cause's name and its
//! crate ([`words`]), never placed or drawn (but under `--each`). The command follows each chain
//! of waits to the task that failed on its own ([`groups`]), prints that ROOT once as the
//! failure it is, and under it one line counting what hangs from it: the crates (the first
//! [`super::CRATES_SHOWN`] named), the files and the items. Causes in name order, crates by
//! name: the same bytes from the records whatever order the tasks ran in (rule 25).

use blaze_drive::{Kind, Name};
use blaze_ids::ContentId;
use blaze_own::Vec;
use bstd::mem::Frame;

use super::CRATES_SHOWN;
use crate::CliError;

const HEX: &[u8; 16] = b"0123456789abcdef";

/// The hex digits of a cause's name: its kind's byte, then its subject's 32.
const NAME_DIGITS: usize = 66;

/// A `WAITED` finding's words, in `held`: the name of the task it needs that failed, as
/// [`NAME_DIGITS`] hex digits, then 0x1F, then its crate as printed (`a`, or `a (macro side)`;
/// empty for a task that is no crate's scope).
pub(crate) fn words<'b>(on: Name, label: Option<(&str, bool)>, held: &'b Frame) -> bstd::mem::String<&'b Frame> {
    let mut digits = [0u8; NAME_DIGITS];
    for (at, byte) in core::iter::once(on.kind as u8).chain(on.subject.0.iter().copied()).enumerate() {
        digits[2 * at] = HEX[usize::from(byte >> 4)];
        digits[2 * at + 1] = HEX[usize::from(byte & 0xf)];
    }
    // Only ASCII hex digits were written.
    let digits = core::str::from_utf8(&digits).unwrap_or("");
    match label {
        Some((name, true)) => bstd::mem::format_in!(held, "{digits}\x1f{name} (macro side)"),
        Some((name, false)) => bstd::mem::format_in!(held, "{digits}\x1f{name}"),
        None => bstd::mem::format_in!(held, "{digits}\x1f"),
    }
}

/// The cause and crate of a `WAITED` finding's words ([`words`]' form). Words of another form
/// are a typed refusal naming the rule (rule 3): never counted under some other cause.
pub(crate) fn read(words: &[u8]) -> Result<(Name, &str), CliError> {
    let refused = CliError::Text { what: "a waited finding whose words are not its cause's name and its crate (blaze-cli not_checked/waited.rs `words`)" };
    let (Some(digits), Some(b'\x1f')) = (words.get(..NAME_DIGITS), words.get(NAME_DIGITS)) else { return Err(refused) };
    // By range, never a search of `HEX`: 66 digits a finding, and tempo's no-op reads 31,000.
    let nibble = |digit: u8| match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        _ => None,
    };
    let mut bytes = [0u8; 33];
    for (at, pair) in digits.chunks_exact(2).enumerate() {
        let (Some(high), Some(low)) = (nibble(pair[0]), nibble(pair[1])) else { return Err(refused) };
        bytes[at] = high << 4 | low;
    }
    let Some(kind) = Kind::from_u8(bytes[0]) else { return Err(refused) };
    let mut subject = [0u8; 32];
    subject.copy_from_slice(&bytes[1..]);
    let Ok(label) = core::str::from_utf8(&words[NAME_DIGITS + 1..]) else { return Err(refused) };
    Ok((Name::new(kind, ContentId(subject)), label))
}

/// One waited task, as draw found its finding.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Wait<'f> {
    /// The waiting task.
    pub(crate) name: Name,
    /// What it needs that failed.
    pub(crate) on: Name,
    /// Its crate as printed, empty for a task that is no crate's scope.
    pub(crate) label: &'f str,
    /// Its file's index in the request, for an item's task.
    pub(crate) file: Option<u32>,
}

/// What hangs from one cause: its tasks counted by crate, file and item.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Group<'f> {
    /// The task whose chain of waits ends here: it failed on its own, or was not filed.
    pub(crate) cause: Name,
    /// The waiting crates, by name, each once.
    pub(crate) crates: Vec<&'f str, &'f Frame>,
    /// The files of the waiting items, each once, in index order.
    pub(crate) files: Vec<u32, &'f Frame>,
    /// The waiting tasks of an item in a file.
    pub(crate) items: u32,
    /// Every waiting task.
    pub(crate) tasks: u32,
}

/// The waits grouped by the cause their chain ends at, in cause order. A task waiting twice (two
/// configurations of one name) counts once. A ring of waits is a typed refusal (rule 3): no
/// cause can be named.
pub(crate) fn groups<'f>(waits: &mut Vec<Wait<'f>, &'f Frame>, frame: &'f Frame) -> Result<Vec<Group<'f>, &'f Frame>, CliError> {
    waits.sort_unstable();
    waits.dedup_by(|a, b| a.name == b.name);
    let waits: &Vec<Wait<'f>, &'f Frame> = waits;
    let on_of = |name: Name| waits.binary_search_by(|w| w.name.cmp(&name)).ok().map(|at| waits[at].on);
    // (cause, the wait's index): each chain followed to its end, at most a step a wait.
    let mut caused: Vec<(Name, usize), &Frame> = Vec::with_capacity_in(waits.len(), frame);
    for (at, wait) in waits.iter().enumerate() {
        let mut cause = wait.on;
        let mut steps = 0usize;
        while let Some(next) = on_of(cause) {
            steps += 1;
            if steps > waits.len() {
                return Err(CliError::Text { what: "the waited tasks wait on each other in a ring: no cause can be named (blaze-cli not_checked/waited.rs, rule 3)" });
            }
            cause = next;
        }
        caused.push((cause, at));
    }
    caused.sort_unstable();
    let mut out: Vec<Group<'f>, &Frame> = Vec::new_in(frame);
    let mut at = 0usize;
    while at < caused.len() {
        let cause = caused[at].0;
        let end = caused[at..].iter().position(|(c, _)| *c != cause).map_or(caused.len(), |n| at + n);
        let mut crates: Vec<&'f str, &'f Frame> = Vec::new_in(frame);
        let mut files: Vec<u32, &'f Frame> = Vec::new_in(frame);
        let mut items = 0u32;
        for &(_, i) in caused[at..end].iter() {
            let wait = waits[i];
            if !wait.label.is_empty() {
                crates.push(wait.label);
            }
            if let Some(file) = wait.file {
                files.push(file);
                items += 1;
            }
        }
        crates.sort_unstable();
        crates.dedup();
        files.sort_unstable();
        files.dedup();
        let tasks = u32::try_from(end - at).map_err(|_| CliError::Text { what: "more than 2^32 tasks waited on one cause" })?;
        out.push(Group { cause, crates, files, items, tasks });
        at = end;
    }
    Ok(out)
}

/// The line under a cause: "3 crates, 2 files, 9 items not checked because of it (12 tasks in
/// all): `a`, `b`, `c`", the first [`CRATES_SHOWN`] crates named and "and N more" past them.
/// Counts of none are left out.
pub(crate) fn line<'f>(group: &Group<'_>, frame: &'f Frame) -> Result<bstd::mem::String<&'f Frame>, CliError> {
    let text = |_: core::fmt::Error| CliError::Text { what: "a waited line's words" };
    let plural = |n: usize, one: &'static str, many: &'static str| if n == 1 { one } else { many };
    let mut words = bstd::mem::String::new_in(frame);
    let n = group.crates.len();
    for (count, one, many) in [(n, "crate", "crates"), (group.files.len(), "file", "files"), (group.items as usize, "item", "items")] {
        if count == 0 {
            continue;
        }
        if !words.is_empty() {
            words.push_str(", ");
        }
        core::fmt::write(&mut words, format_args!("{count} {}", plural(count, one, many))).map_err(text)?;
    }
    let tasks = plural(group.tasks as usize, "task", "tasks");
    if words.is_empty() {
        // No crate or item among them: the tasks alone.
        core::fmt::write(&mut words, format_args!("{} {tasks} not checked because of it", group.tasks)).map_err(text)?;
    } else {
        core::fmt::write(&mut words, format_args!(" not checked because of it ({} {tasks} in all)", group.tasks)).map_err(text)?;
    }
    for (i, name) in group.crates.iter().take(CRATES_SHOWN).enumerate() {
        words.push_str(if i == 0 { ": " } else { ", " });
        core::fmt::write(&mut words, format_args!("`{name}`")).map_err(text)?;
    }
    if n > CRATES_SHOWN {
        core::fmt::write(&mut words, format_args!(" and {} more", n - CRATES_SHOWN)).map_err(text)?;
    }
    Ok(words)
}

#[cfg(test)]
mod tests {
    use super::{Wait, groups, line, read, words};
    use blaze_drive::{Kind, Name};
    use blaze_ids::ContentId;

    /// A cause's name and crate come back from its words; another form is refused. A chain of
    /// two waits ends at its root: one group, its crates by name, its files and items counted.
    /// Prints "test ... ok". Control: stop the chain at the first step (`cause = wait.on`, no
    /// loop) and the bodies count under the crate's scope, two groups, and the assertion fails.
    #[test]
    fn a_chain_of_waits_counts_under_its_root() {
        let frame = bstd::mem::Frame::with_reserve(1 << 20, bstd::mem::Tag::Other).expect("a test frame");
        let root = Name::new(Kind::Script, ContentId([7; 32]));
        let said = words(root, Some(("a", true)), &frame);
        assert_eq!(read(said.as_str().as_bytes()).expect("its own form"), (root, "a (macro side)"));
        assert!(read(b"not a name\x1fa").is_err());
        let scope = |n: u8| Name::new(Kind::CrateScope, ContentId([n; 32]));
        let body = |n: u8| Name::new(Kind::BodyCheck, ContentId([n; 32]));
        let mut waits = blaze_own::Vec::new_in(&frame);
        for wait in [
            Wait { name: scope(1), on: root, label: "b", file: None },
            Wait { name: scope(2), on: root, label: "a", file: None },
            Wait { name: body(3), on: scope(1), label: "", file: Some(4) },
            Wait { name: body(4), on: scope(1), label: "", file: Some(4) },
            Wait { name: body(4), on: scope(1), label: "", file: Some(4) },
            Wait { name: body(5), on: scope(2), label: "", file: Some(5) },
        ] {
            waits.push(wait);
        }
        let all = groups(&mut waits, &frame).expect("no ring");
        assert_eq!(all.len(), 1, "{all:?}");
        assert_eq!((all[0].cause, all[0].crates.as_slice(), all[0].files.as_slice(), all[0].items, all[0].tasks), (root, &["a", "b"][..], &[4, 5][..], 3, 5));
        assert_eq!(line(&all[0], &frame).expect("words").as_str(), "2 crates, 2 files, 3 items not checked because of it (5 tasks in all): `a`, `b`");
    }
}
