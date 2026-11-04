//! A report for a person: findings ranked, each with its fix and evidence, then blind spots.

use std::fmt::Write as _;

use super::{Died, DiedThread, Estimate, Report, s};

/// `report` as text, the first `top` findings and blind spots of each.
pub fn text(report: &Report, top: usize) -> String {
    // alloc: the report, once a report
    let mut out = String::new();
    let (b, sum) = (&report.build, &report.summary);
    // Writing to a `String` does not fail.
    _ = write!(out, "build {}", b.id);
    if let Some(command) = &b.command {
        _ = write!(out, " ({command})");
    }
    let outcome = match &b.died {
        Some(_) => "ended by the daemon's death",
        None => b.outcome.as_deref().unwrap_or("unfinished"),
    };
    _ = writeln!(
        out,
        ": {outcome} {}, {} workers {:.0}% busy",
        s(sum.wall_ns),
        sum.workers,
        100.0 * sum.busy_ratio
    );
    if let (Some(tree), Some(blaze)) = (&b.tree, &b.blaze) {
        _ = writeln!(out, "  tree {tree}, blaze {blaze}");
    }
    if let Some(died) = &b.died {
        died_text(died, top, &mut out);
    }
    // alloc: a line
    let links: Vec<String> =
        sum.path_by_link.iter().map(|it| format!("{} {}", it.name, s(it.ns))).collect();
    _ = writeln!(
        out,
        "  critical path {} back to {}: {}",
        s(sum.path_ns),
        sum.path_reached,
        links.join(", ")
    );
    if !sum.path_by_phase.is_empty() {
        // alloc: a line
        let phases: Vec<String> =
            sum.path_by_phase.iter().map(|it| format!("{} {}", it.name, s(it.ns))).collect();
        _ = writeln!(out, "  its running time by phase: {}", phases.join(", "));
    }
    _ = writeln!(out, "  {}", recorder(&sum.recorder));
    _ = writeln!(out);
    if report.findings.is_empty() {
        _ = writeln!(out, "no finding saves {} or more", s(sum.cut_ns));
    }
    for (i, it) in report.findings.iter().take(top).enumerate() {
        // alloc: a cell
        let saves = match it.estimate {
            Estimate::Unknown => "saves ?".to_owned(),
            Estimate::CriticalPath => format!("saves {} (critical path)", s(it.saves_ns)),
            Estimate::IdleCores => format!("saves up to {} (idle cores)", s(it.saves_ns)),
            Estimate::ThreadTime => {
                format!("saves ~{} (thread time / parallelism)", s(it.saves_ns))
            }
        };
        _ = writeln!(out, "{:>2}. {saves}  {}", i + 1, it.title);
        if let Some(owner) = &it.owner {
            _ = writeln!(out, "    in {owner}");
        }
        for line in &it.evidence {
            _ = writeln!(out, "    - {line}");
        }
        _ = writeln!(out, "    fix: {}", it.fix.says());
    }
    let hidden = report.findings.len().saturating_sub(top);
    if hidden > 0 || sum.smaller > 0 {
        _ = writeln!(
            out,
            "    ({hidden} more past --top, {} saving under {} left out)",
            sum.smaller,
            s(sum.cut_ns)
        );
    }
    if !report.blind_spots.is_empty() {
        _ = writeln!(out, "\nwhat the profile could not see:");
    }
    for it in report.blind_spots.iter().take(top) {
        _ = writeln!(out, " - {}", it.title);
        for line in &it.evidence {
            _ = writeln!(out, "     {line}");
        }
        _ = writeln!(out, "     fix: {}", it.fix);
    }
    _ = writeln!(
        out,
        "\n`blaze slow --json` for an agent; `blaze debug time --build {}` for the raw tables",
        b.id
    );
    out
}

/// The death of the build's daemon, and what its threads were inside at its last reading, the
/// first `top` threads of them.
fn died_text(died: &Died, top: usize, out: &mut String) {
    _ = writeln!(
        out,
        "  the daemon (pid {}) died after its last row, {} in; nothing ended the build",
        died.process,
        s(died.last_row_ns)
    );
    let Some(read) = died.read_ns else {
        _ = writeln!(
            out,
            "  what it was running: no reading of its threads was written in the build"
        );
        return;
    };
    _ = writeln!(
        out,
        "  what it was running at its last reading, {} in ({} threads inside spans):",
        s(read),
        died.threads.len()
    );
    for thread in died.threads.iter().take(top) {
        _ = writeln!(out, "    {}", thread_line(thread));
    }
    let hidden = died.threads.len().saturating_sub(top);
    if hidden > 0 {
        _ = writeln!(out, "    ({hidden} more threads past --top)");
    }
}

/// A thread of a daemon that died and the spans it was inside, outermost first, in the stall
/// watchdog's form ([`crate::watchdog::thread_said`]): `worker (tid 7): unit bodies (3.0 s) >
/// wait, awaiting key, run by task 7 (2.9 s)`. Said by `blaze slow` and by the CLI a dead daemon
/// hung up on.
pub fn thread_line(thread: &DiedThread) -> String {
    let spans = thread.spans.iter().map(|it| crate::watchdog::SpanSaid {
        name: &it.name,
        function: it.function.as_deref(),
        krate: None,
        unit: None,
        awaits: it.awaits.as_deref(),
        owner: it.owner,
        for_ns: it.for_ns,
    });
    // alloc: a line
    let mut line = String::new();
    _ = crate::watchdog::thread_said(
        &mut line,
        thread.name.as_deref().unwrap_or("?"),
        u64::from(thread.thread),
        spans,
    );
    line
}

/// One line on what the flight recorder dropped and summed, said even when it dropped nothing.
fn recorder(r: &super::Recorder) -> String {
    let lost = if r.rows_skipped.is_none() && r.spans_lost.is_none() {
        "recorder: this record has no loss counters".to_owned()
    } else if r.lost() == 0 {
        "recorder: lost no rows".to_owned()
    } else {
        let behind = r.rows_behind.unwrap_or(0);
        format!(
            "recorder: LOST {} rows ({} while the history's writer was behind, {} a ring wrote over between drains, {} spans with no slot)",
            r.lost(),
            behind,
            r.rows_skipped.unwrap_or(0) - behind,
            r.spans_lost.unwrap_or(0)
        )
    };
    match (r.folded_spans, r.folded_ns) {
        (Some(n), Some(ns)) if n > 0 => format!(
            "{lost}; summed {n} spans under {:?} into `folded` ({})",
            crate::flight::FOLD_BELOW,
            super::s(ns)
        ),
        _ => lost,
    }
}
