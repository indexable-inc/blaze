//! A report for a person: findings ranked, each with its fix and evidence, then blind spots.

use std::fmt::Write as _;

use super::{Died, DiedThread, Estimate, PanicFacts, Report, StackSeen, s};

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
    panics_text(&b.panics, top, &mut out);
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
        for (side, stacks) in [("on the CPU", &it.stacks.on_cpu), ("blocked", &it.stacks.off_cpu)] {
            for stack in stacks {
                _ = writeln!(
                    out,
                    "    - sampled {side} inside it {}x: {}",
                    stack.samples,
                    innermost(stack)
                );
            }
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

/// The frames a stack line shows, innermost first.
const STACK_FRAMES: usize = 8;

/// A sampled stack's innermost [`STACK_FRAMES`] frames, innermost first, `<` between: where
/// the time went, then what called it; `< ...` when there are more.
fn innermost(stack: &StackSeen) -> String {
    if stack.frames.is_empty() {
        // Linux's blocked samples: the kernel gives the time of a block, not its stack
        // (`sampler/linux.rs`), so the span it was inside is what says where.
        // alloc: a line
        return "no stack (Linux samples where a thread blocked by its span alone)".to_owned();
    }
    // alloc: a line
    let mut line = String::new();
    for (i, frame) in stack.frames.iter().rev().take(STACK_FRAMES).enumerate() {
        if i > 0 {
            line.push_str(" < ");
        }
        line.push_str(frame);
    }
    if stack.frames.len() > STACK_FRAMES {
        line.push_str(" < ...");
    }
    line
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

/// The build's panics and its daemon's fatal signal, the first `top`, each with its stack's top
/// frames: `panics: 2 (1 caught by its job)`, then `panicked at src/a.rs:3:5 on thread w: oops`
/// and `at <frame>` lines.
fn panics_text(panics: &[PanicFacts], top: usize, out: &mut String) {
    if panics.is_empty() {
        return;
    }
    let caught = panics.iter().filter(|it| it.caught).count();
    _ = writeln!(out, "  panics: {} ({caught} caught by its job)", panics.len());
    for it in panics.iter().take(top) {
        let what = match (it.kind.as_str(), &it.signal) {
            ("signal", Some(signal)) => format!("the daemon died of {signal}"), // alloc: a line
            ("signal", None) => "the daemon died of a fatal signal".to_owned(), // alloc: a line
            (_, _) if it.caught => "panicked, failing its job".to_owned(),      // alloc: a line
            (_, _) => "panicked, caught by no job".to_owned(),                  // alloc: a line
        };
        _ = write!(out, "    {what}");
        if let Some(at) = &it.location {
            _ = write!(out, " at {at}");
        }
        if let Some(thread) = &it.thread {
            _ = write!(out, " on thread {thread}");
        }
        _ = writeln!(out, ": {}", it.message.as_deref().unwrap_or("(no message)"));
        for frame in &it.top {
            _ = writeln!(out, "      at {frame}");
        }
    }
    let hidden = panics.len().saturating_sub(top);
    if hidden > 0 {
        _ = writeln!(out, "    ({hidden} more panics past --top)");
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
    let said = match (r.folded_spans, r.folded_ns) {
        (Some(n), Some(ns)) if n > 0 => format!(
            "{lost}; summed {n} spans under {:?} into `folded` ({})",
            crate::flight::FOLD_BELOW,
            super::s(ns)
        ),
        _ => lost,
    };
    // alloc: a line
    format!("{said}; {}", recorder_cost(r))
}

/// What the recorder itself cost over the window, from its own measure ([`crate::flight::cost`]).
fn recorder_cost(r: &super::Recorder) -> String {
    match (r.cost_ns, r.spans_made, r.cpu_reads) {
        (Some(ns), Some(spans), reads) if spans > 0 => format!(
            "cost {} ({} ns a span over {spans} spans, {} thread-CPU reads)",
            super::s(ns),
            ns / spans,
            reads.unwrap_or(0)
        ),
        (Some(ns), _, reads) => {
            format!("cost {} over no spans ({} thread-CPU reads)", super::s(ns), reads.unwrap_or(0))
        }
        (None, ..) => "this record has no cost counters".to_owned(),
    }
}
