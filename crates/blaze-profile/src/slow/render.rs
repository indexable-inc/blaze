//! A report for a person: findings ranked, each with its fix and evidence, then blind spots.

use std::fmt::Write as _;

use super::{Estimate, Report, s};

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
    _ = writeln!(
        out,
        ": {} {}, {} workers {:.0}% busy",
        b.outcome.as_deref().unwrap_or("unfinished"),
        s(sum.wall_ns),
        sum.workers,
        100.0 * sum.busy_ratio
    );
    if let (Some(tree), Some(blaze)) = (&b.tree, &b.blaze) {
        _ = writeln!(out, "  tree {tree}, blaze {blaze}");
    }
    // alloc: a line
    let links: Vec<String> = sum.path_by_link.iter().map(|it| format!("{} {}", it.name, s(it.ns))).collect();
    _ = writeln!(out, "  critical path {} back to {}: {}", s(sum.path_ns), sum.path_reached, links.join(", "));
    if !sum.path_by_phase.is_empty() {
        // alloc: a line
        let phases: Vec<String> = sum.path_by_phase.iter().map(|it| format!("{} {}", it.name, s(it.ns))).collect();
        _ = writeln!(out, "  its running time by phase: {}", phases.join(", "));
    }
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
            Estimate::ThreadTime => format!("saves ~{} (thread time / parallelism)", s(it.saves_ns)),
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
        _ = writeln!(out, "    ({hidden} more past --top, {} saving under {} left out)", sum.smaller, s(sum.cut_ns));
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
    _ = writeln!(out, "\n`blaze slow --json` for an agent; `blaze debug time --build {}` for the raw tables", b.id);
    out
}
