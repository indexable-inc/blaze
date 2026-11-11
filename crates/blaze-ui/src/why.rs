//! `blaze why`'s text ([`blaze_api::Rebuilt`]): without an item, what the last build computed
//! again, grouped by reason as the daemon grouped it, most first; with one, its chain of
//! reasons in order.

use blaze_api::{EvictedBy, PartChanged, Reason, Rebuild, RebuildCause, Rebuilt};
use blaze_ids::ContentId;

/// What is said when the daemon has no build to answer of: why, and the command that reads
/// the reasons of the builds before it started.
const NO_BUILD: &str = "no build of a workspace holding this directory has ended since the daemon started, and the daemon keeps a build's reasons only while it runs: earlier builds' are in the history, `blaze debug sql \"SELECT build_id, step, item, cause, what FROM misses ORDER BY build_id DESC LIMIT 50\"`";

/// `rebuilt` as `blaze why` prints it.
pub fn lines(out: &mut String, rebuilt: &Rebuilt) {
    if rebuilt.restoring {
        put!(
            out,
            "note: the daemon is still restoring blaze why's ledger, so whether it restored whole is not known yet\n"
        );
    }
    if let Some(torn) = &rebuilt.torn {
        put!(
            out,
            "note: blaze why's ledger restored short at the daemon's start ({torn}): an item whose records it lost reads as computed first, or as not explained where an older record of it was read\n"
        );
    }
    if let Some(dropped) = rebuilt.dropped
        && (rebuilt.groups.iter().any(|it| it.cause == RebuildCause::First)
            || rebuilt.rebuilt.iter().any(|it| it.cause == RebuildCause::First))
    {
        put!(
            out,
            "note: to stay under its cap, blaze why's ledger dropped the records of what was last computed in build {dropped} or before: an item read as computed first may be one of them\n"
        );
    }
    let Some(build) = rebuilt.build else {
        put!(
            out,
            "{NO_BUILD}\n"
        );
        return;
    };
    if let (Some(workspace), Some(command)) = (&rebuilt.workspace, &rebuilt.command) {
        put!(out, "build {build} is `{command}` in {workspace}\n");
    }
    if rebuilt.chain {
        return chain(out, build, &rebuilt.rebuilt);
    }
    if rebuilt.count == 0 {
        put!(
            out,
            "build {build} computed nothing again that was asked of: everything was reused\n"
        );
        return;
    }
    put!(out, "build {build} computed {} things again:\n", rebuilt.count);
    for group in &rebuilt.groups {
        put!(out, "\n  {} {}: {}\n", group.count, group.step, reason(&group.cause, false, build));
        for item in &group.items {
            put!(out, "    {item}\n");
        }
        let listed = u32::try_from(group.items.len()).unwrap_or(u32::MAX);
        if group.count > listed {
            put!(out, "    and {} more (`--all` lists every one)\n", group.count - listed);
        }
    }
    put!(out, "\n`blaze why ITEM` gives an item's chain of reasons.\n");
}

/// `rebuilt`, the answer to `blaze why --unexplained`: each step the build recomputed for no
/// recorded reason, with what is missing.
pub fn unexplained(out: &mut String, rebuilt: &Rebuilt) {
    let Some(build) = rebuilt.build else {
        put!(
            out,
            "{NO_BUILD}\n"
        );
        return;
    };
    if rebuilt.rebuilt.is_empty() {
        put!(out, "build {build}: every step it recomputed has a recorded reason\n");
        return;
    }
    put!(
        out,
        "build {build} recomputed {} steps for no recorded reason, each a bug in blaze:\n",
        rebuilt.count
    );
    for it in &rebuilt.rebuilt {
        put!(out, "\n  {} ({})\n", it.item, it.step);
        for end in &it.ends {
            if let Reason::Unexplained { what } = end {
                put!(out, "    {what}\n");
            }
        }
    }
}

/// An item's reasons, then each item they name, in order, then what the chain ends in.
fn chain(out: &mut String, build: u64, rebuilt: &[Rebuild]) {
    let Some((first, rest)) = rebuilt.split_first() else {
        put!(out, "build {build}: nothing of that name was computed again\n");
        return;
    };
    put!(out, "build {build}: {}\n", one(first, build));
    for it in rest {
        put!(out, "  because {}\n", one(it, build));
    }
    for it in &first.ends {
        put!(out, "  in the end: {}\n", end(it));
    }
}

/// Where a chain ends, in words.
fn end(it: &Reason) -> String {
    // alloc: a reason's text, once an end of an item asked
    match it {
        Reason::InputChanged { input, trivia: false, .. } => format!("`{input}` changed"),
        Reason::InputChanged { input, trivia: true, .. } => {
            format!("`{input}` changed, though no item in it did (comments only)")
        }
        Reason::FirstTime => "it was never computed before".to_owned(),
        Reason::Evicted { by, when } => format!(
            "it was last computed in build {when}, and {} since",
            match by {
                EvictedBy::Memory => "the store dropped its result to free memory",
                EvictedBy::Age => "the store dropped its result as unused",
                EvictedBy::Cap => "blaze why's ledger dropped its record to stay under its cap",
            }
        ),
        // What is recorded: the build did not end well and nothing was kept. Whether the
        // step's own compute failed is that build's `errors` rows, named, not guessed here.
        Reason::PreviousFailed { since } => format!(
            "its last compute was in build {since}, which did not end well (it failed, was cancelled or its daemon stopped), and nothing of it was kept; whether its own compute failed is in that build's errors: `blaze debug sql \"SELECT unit, message FROM errors WHERE build_id = {since}\"`"
        ),
        Reason::Forced { how } => format!("it was asked for: {how}"),
        Reason::KeyUnstable { part } => format!(
            "`{part}` came out different from the same inputs (a determinism bug in blaze)"
        ),
        Reason::Unexplained { what } => format!("no recorded reason (a bug in blaze): {what}"),
    }
}

fn one(it: &Rebuild, this: u64) -> String {
    // alloc: a line, once an item asked
    format!("{} ({}) was computed again: {}", it.item, it.step, reason(&it.cause, true, this))
}

/// Why, in words; `exact` adds what tells one item from another (the build it is compared
/// with, the hashes), which a group leaves out. `this` is the build asked of.
///
/// Each text says what the daemon recorded and nothing it inferred: where the record does not
/// say which of two things happened, the text says that it does not, and where the record is
/// gone, it says where what is left of it can be read.
fn reason(cause: &RebuildCause, exact: bool, this: u64) -> String {
    // alloc: a reason's text, once an item
    let since = |build: &u64| match exact {
        true => format!(" since it was last computed, in build {build}"),
        false => " since it was last computed".to_owned(),
    };
    // alloc: a reason's text, once an item
    match cause {
        RebuildCause::First => "nothing was kept under its key, and blaze has no record of computing it before".to_owned(),
        RebuildCause::Parts { parts, since: build, changed } => {
            // alloc: a reason's text
            let named: Vec<String> = changed.iter().map(|it| part(it, exact)).collect();
            let of = if exact { format!(" (of {parts} in its key)") } else { String::new() };
            format!("{} changed{}{of}", and(&named), since(build))
        }
        RebuildCause::Lost { since: build } => format!(
            "its key is the one it had when last computed{}, yet nothing is kept under it now: either that compute did not keep it or it was collected since, and which is not recorded",
            match exact {
                true => format!(", in build {build}"),
                false => String::new(),
            }
        ),
        RebuildCause::Unfinished { since: build } => format!(
            "its key is the one it had when last computed{}, in a build that did not end well, and nothing is kept under it; whether its own compute failed or the build stopped before keeping it is not recorded here: that build's errors say (`blaze debug sql \"SELECT unit, message FROM errors WHERE build_id = {build}\"`)",
            match exact {
                true => format!(", in build {build}"),
                false => String::new(),
            }
        ),
        RebuildCause::Dropped { since: build } => format!(
            "it was last computed{}, and blaze why's ledger dropped what its key was made of then to stay under its cap, so the daemon cannot say what changed since; while the history keeps that build, its `key_parts` rows can: `blaze explain key_part_diff after={this} before={build}`; its compute in this build is recorded whole",
            match exact {
                true => format!(" in build {build}"),
                false => String::new(),
            }
        ),
        RebuildCause::Beyond { what, since: build } => format!(
            "its key is the one it had when last computed{}, so {what} differ, or that compute did not keep it: which, is not explained yet",
            match exact {
                true => format!(", in build {build}"),
                false => String::new(),
            }
        ),
        RebuildCause::Unnamed { since: build } => format!(
            "its key changed{} though every part it names is the same: the key has a part it does not name",
            since(build)
        ),
        RebuildCause::Read { now, was, read, reads, since: build, changed, .. } => match (exact, build) {
            (true, Some(build)) => format!(
                "{read} changed ({} -> {}) since the entry kept in build {build}, {changed} of its {reads} reads in all",
                hash(was),
                hash(now)
            ),
            (true, None) => format!("{read} changed ({} -> {}), {changed} of its {reads} reads in all", hash(was), hash(now)),
            (false, _) => format!("{read} changed"),
        },
        RebuildCause::Unrecorded => {
            "shapes of its reads are kept, but blaze has no record of the entry it kept last: either another daemon kept it or it was kept before this ledger began, and which is not recorded; its compute in this build is recorded".to_owned()
        }
        RebuildCause::Collected => "the entry blaze recorded as kept last is not in the store now: either that compute did not keep it or it was collected since, and which is not recorded".to_owned(),
        RebuildCause::Unshaped => {
            "every read of the entry kept last hashes as it did, yet no shape of reads names it; what left it so is not recorded (a keep that stops between its entry and its shape leaves this state)".to_owned()
        }
        RebuildCause::Twice { count } => format!(
            "this build computed it {count} times under different keys, so none is compared with its last compute (a bug in blaze: it told them apart by less than their keys)"
        ),
        RebuildCause::Always { why } => format!("it runs every build: {why}"),
        RebuildCause::NotExplained { what } => format!("not explained yet: {what}"),
        RebuildCause::Failed { why } => format!("the reason could not be worked out: {why}"),
    }
}

fn part(it: &PartChanged, exact: bool) -> String {
    let name = match (it.was, it.now) {
        (None, Some(_)) => format!("`{}` (added)", it.part),
        (Some(_), None) => format!("`{}` (removed)", it.part),
        _ => format!("`{}`", it.part),
    };
    match (exact, it.was, it.now) {
        (true, Some(was), Some(now)) => format!("{name} ({} -> {})", short(&was), short(&now)),
        _ => name,
    }
}

fn hash(it: &Option<ContentId>) -> String {
    match it {
        Some(id) => short(id),
        // alloc: a reason's text
        None => "nothing".to_owned(),
    }
}

/// A hash's first eight bytes in hex: enough to tell two apart in a line.
fn short(id: &ContentId) -> String {
    // alloc: a reason's text
    id.0[..8].iter().map(|it| format!("{it:02x}")).collect()
}

/// `a`, `a and b`, `a, b and c`.
fn and(names: &[String]) -> String {
    match names {
        [] => "nothing it names".to_owned(), // alloc: a reason's text
        [one] => one.clone(),                // clone: a reason's text
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn every_cause() -> Vec<RebuildCause> {
        vec![
            RebuildCause::First,
            RebuildCause::Lost { since: 3 },
            RebuildCause::Unfinished { since: 3 },
            RebuildCause::Dropped { since: 3 },
            RebuildCause::Beyond { what: "its process variables".to_owned(), since: 3 },
            RebuildCause::Unnamed { since: 3 },
            RebuildCause::Unrecorded,
            RebuildCause::Collected,
            RebuildCause::Unshaped,
            RebuildCause::Twice { count: 2 },
            RebuildCause::Always { why: "it is unsandboxed".to_owned() },
            RebuildCause::NotExplained { what: "its key names no parts".to_owned() },
            RebuildCause::Failed { why: "the ledger was not read".to_owned() },
        ]
    }

    /// No reason `blaze why` prints is a guess: none says what "likely" or "probably"
    /// happened. Until 2026-11-11 a step last computed in a build that did not end well was
    /// said to have "likely failed or was stopped".
    #[test]
    fn no_reason_is_a_guess() {
        for cause in every_cause() {
            for exact in [false, true] {
                let said = reason(&cause, exact, 9);
                for guess in ["likely", "probably", "perhaps", "presumably", "may have"] {
                    assert!(!said.contains(guess), "{cause:?} guesses (`{guess}`): {said}");
                }
            }
        }
        let ends = [
            Reason::FirstTime,
            Reason::PreviousFailed { since: 3 },
            Reason::Evicted { by: EvictedBy::Cap, when: 3 },
        ];
        for it in &ends {
            let said = end(it);
            assert!(!said.contains("likely") && !said.contains("probably"), "{said}");
        }
    }

    /// Where the record that would explain is gone or was never made, the reason says so and
    /// names the command that reads what is left.
    #[test]
    fn an_explanation_that_is_unavailable_says_why_and_where_to_read_it() {
        let dropped = reason(&RebuildCause::Dropped { since: 3 }, true, 9);
        assert!(dropped.contains("in build 3"), "{dropped}");
        assert!(dropped.contains("to stay under its cap"), "{dropped}");
        assert!(dropped.contains("`blaze explain key_part_diff after=9 before=3`"), "{dropped}");
        let unfinished = reason(&RebuildCause::Unfinished { since: 3 }, true, 9);
        assert!(unfinished.contains("is not recorded here"), "{unfinished}");
        assert!(unfinished.contains("FROM errors WHERE build_id = 3"), "{unfinished}");
        let failed = end(&Reason::PreviousFailed { since: 3 });
        assert!(failed.contains("build 3, which did not end well"), "{failed}");
        assert!(failed.contains("FROM errors WHERE build_id = 3"), "{failed}");
        for cause in [RebuildCause::Lost { since: 3 }, RebuildCause::Collected, RebuildCause::Unrecorded] {
            let said = reason(&cause, true, 9);
            assert!(said.contains("which is not recorded"), "{cause:?}: {said}");
        }
        // No build to answer of: why, and the query over the history.
        let mut out = String::new();
        let none = Rebuilt {
            torn: None,
            build: None,
            chain: false,
            count: 0,
            groups: Vec::new(),
            command: None,
            dropped: None,
            rebuilt: Vec::new(),
            summary: Vec::new(),
            restoring: false,
            workspace: None,
        };
        lines(&mut out, &none);
        assert!(out.contains("only while it runs") && out.contains("FROM misses ORDER BY build_id DESC"), "{out}");
    }
}
