//! `blaze why`'s text ([`blaze_api::Rebuilt`]): without an item, what the last build computed
//! again, grouped by reason as the daemon grouped it, most first; with one, its chain of
//! reasons in order.

use blaze_api::{EvictedBy, PartChanged, Reason, Rebuild, RebuildCause, Rebuilt};
use blaze_ids::ContentId;

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
            "no build of a workspace holding this directory has ended since the daemon started: earlier builds' reasons are in the history's `misses` table (`blaze debug sql`)\n"
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
        put!(out, "\n  {} {}: {}\n", group.count, group.step, reason(&group.cause, false));
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
            "no build of a workspace holding this directory has ended since the daemon started: earlier builds' reasons are in the history's `misses` table (`blaze debug sql`)\n"
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
    put!(out, "build {build}: {}\n", one(first));
    for it in rest {
        put!(out, "  because {}\n", one(it));
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
        Reason::PreviousFailed { since } => {
            format!("its last compute, in build {since}, failed or was stopped")
        }
        Reason::Forced { how } => format!("it was asked for: {how}"),
        Reason::KeyUnstable { part } => format!(
            "`{part}` came out different from the same inputs (a determinism bug in blaze)"
        ),
        Reason::Unexplained { what } => format!("no recorded reason (a bug in blaze): {what}"),
    }
}

fn one(it: &Rebuild) -> String {
    // alloc: a line, once an item asked
    format!("{} ({}) was computed again: {}", it.item, it.step, reason(&it.cause, true))
}

/// Why, in words; `exact` adds what tells one item from another (the build it is compared
/// with, the hashes), which a group leaves out.
fn reason(cause: &RebuildCause, exact: bool) -> String {
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
            "its key is the one it had when last computed{}, yet nothing is kept under it now: that compute did not keep it, or it was collected since",
            match exact {
                true => format!(", in build {build}"),
                false => String::new(),
            }
        ),
        RebuildCause::Unfinished { since: build } => format!(
            "its key is the one it had when last computed{}, in a build that did not end well, and nothing is kept under it: that compute likely failed or was stopped",
            match exact {
                true => format!(", in build {build}"),
                false => String::new(),
            }
        ),
        RebuildCause::Dropped { since: build } => format!(
            "it was last computed{}, and blaze why's ledger dropped what its key was made of then to stay under its cap, so what changed since is not known",
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
            "shapes of its reads are kept, but blaze has no record of the entry it kept last (kept by another daemon, or before its ledger began)".to_owned()
        }
        RebuildCause::Collected => "the entry blaze recorded as kept last is not in the store now: that compute did not keep it, or it was collected since".to_owned(),
        RebuildCause::Unshaped => {
            "every read of the entry kept last hashes as it did, yet no shape of reads names it: a keep that did not finish".to_owned()
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
