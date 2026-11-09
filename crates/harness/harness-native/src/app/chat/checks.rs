//! A pull request's checks as its hover card says them (chat/peek.rs, scene/widgets/peek.flecs `hn.PeekState`): the
//! checks line's words, the rows the list under it shows (one a check not passed: failed first, then running, then
//! queued) and the row the passed and skipped ones fold into, and how long each took or has taken. Pure: the answer and
//! the time (Unix seconds) in, words out.
use harness_proto::peek::{GhCheck, GhCheckState};

/// The checks line: `2 of 9 running · 4m` (the longest running one's time) while any runs, `3 of 9 queued` while some
/// only wait, else `1 failed` or `All checks passed`; a failure is said first while others still run.
pub(super) fn summary(checks: &[GhCheck], total: u64, now: i64) -> String {
    let count = |s: GhCheckState| checks.iter().filter(|c| c.state == s).count();
    let (failed, running, queued) = (count(GhCheckState::Failed), count(GhCheckState::Running), count(GhCheckState::Queued));
    let n = total.max(checks.len() as u64);
    let lead = if failed > 0 { format!("{failed} failed · ") } else { String::new() };
    if running > 0 {
        let longest = checks.iter().filter(|c| c.state == GhCheckState::Running).filter_map(|c| elapsed(c, now)).max();
        let since = longest.map(|s| format!(" · {}", span(s))).unwrap_or_default();
        return format!("{lead}{running} of {n} running{since}");
    }
    if queued > 0 {
        return format!("{lead}{queued} of {n} queued");
    }
    if failed > 0 {
        return format!("{failed} failed");
    }
    if checks.is_empty() {
        return "No checks".to_owned();
    }
    if n > checks.len() as u64 {
        return format!("{} of {n} passed", count(GhCheckState::Passed));
    }
    "All checks passed".to_owned()
}

/// The checks the list shows, one row each: every one that failed or runs, failed first; within each the one started
/// first first (an unknown start last), then by name. What only waits has not run: it is counted in the folded row.
pub(super) fn listed(checks: &[GhCheck]) -> Vec<(usize, &GhCheck)> {
    let mut rows: Vec<(usize, &GhCheck)> = checks
        .iter()
        .enumerate()
        .filter(|(_, c)| matches!(c.state, GhCheckState::Failed | GhCheckState::Running))
        .collect();
    rows.sort_by(|(_, a), (_, b)| (a.state, a.started == 0, a.started, &a.name).cmp(&(b.state, b.started == 0, b.started, &b.name)));
    rows
}

/// The row the rest fold into: `5 queued · 7 passed, 2 skipped`, and how many GitHub has past what was read; None when
/// there are none.
pub(super) fn folded(checks: &[GhCheck], total: u64) -> Option<String> {
    let passed = checks.iter().filter(|c| c.state == GhCheckState::Passed).count();
    let skipped = checks.iter().filter(|c| c.state == GhCheckState::Skipped).count();
    let queued = checks.iter().filter(|c| c.state == GhCheckState::Queued).count();
    let more = total.saturating_sub(checks.len() as u64);
    let mut parts = Vec::new(); // alloc: a row's words, while the card is up
    if passed > 0 {
        parts.push(format!("{passed} passed"));
    }
    if skipped > 0 {
        parts.push(format!("{skipped} skipped"));
    }
    if more > 0 {
        parts.push(format!("{more} more not shown"));
    }
    let done = (!parts.is_empty()).then(|| parts.join(", "));
    match (queued, done) {
        (0, done) => done,
        (q, None) => Some(format!("{q} queued")),
        (q, Some(d)) => Some(format!("{q} queued \u{b7} {d}")),
    }
}

/// How long `c` has run or waited (until `now`), or took (a finished one); None when GitHub gave no start.
pub(super) fn elapsed(c: &GhCheck, now: i64) -> Option<i64> {
    if c.started == 0 {
        return None;
    }
    let end = match c.state {
        GhCheckState::Running | GhCheckState::Queued => now,
        _ if c.completed > 0 => c.completed,
        _ => return None,
    };
    Some((end - c.started).max(0))
}

/// A short span for the checks line: `35s`, `4m`, `1h 4m`.
pub(super) fn span(secs: i64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h {}m", secs / 3600, secs % 3600 / 60)
    }
}

/// A row's time, a clock that ticks each second: `0:35`, `4:12`, `1:04:12`.
pub(super) fn clock(secs: i64) -> String {
    let (h, m, s) = (secs / 3600, secs % 3600 / 60, secs % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(name: &str, state: GhCheckState, started: i64) -> GhCheck {
        GhCheck { name: name.into(), state, workflow: String::new(), step: String::new(), started, completed: 0, url: String::new() }
    }

    /// Running says how many of all and the longest one's time; waiting, failing and passing each their own words.
    #[test]
    fn summary_says_what_is_left() {
        use GhCheckState::*;
        let now = 1_000;
        let mut cs = vec![check("a", Running, 760), check("b", Running, 900), check("c", Queued, 990)];
        cs.extend((0..6).map(|i| check(&format!("p{i}"), Passed, 100)));
        assert_eq!(summary(&cs, 9, now), "2 of 9 running · 4m");
        cs[2].state = Failed;
        assert_eq!(summary(&cs, 9, now), "1 failed · 2 of 9 running · 4m");
        cs[0].state = Queued;
        cs[1].state = Queued;
        assert_eq!(summary(&cs, 9, now), "1 failed · 2 of 9 queued");
        cs[0].state = Passed;
        cs[1].state = Skipped;
        assert_eq!(summary(&cs, 9, now), "1 failed");
        cs[2].state = Passed;
        assert_eq!(summary(&cs, 9, now), "All checks passed");
        assert_eq!(summary(&cs, 12, now), "8 of 12 passed");
        assert_eq!(summary(&[], 0, now), "No checks");
        assert_eq!((span(35), span(240), span(3_840)), ("35s".into(), "4m".into(), "1h 4m".into()));
        assert_eq!((clock(35), clock(252), clock(3_852)), ("0:35".into(), "4:12".into(), "1:04:12".into()));
    }

    /// Failed, then running (the longest first), a row each; what only waits has not run, so it folds with the passed
    /// and skipped into one row.
    #[test]
    fn rows_failed_then_running_the_rest_folded() {
        use GhCheckState::*;
        let cs = [
            check("queued", Queued, 50),
            check("ok", Passed, 10),
            check("late", Running, 300),
            check("broke", Failed, 200),
            check("skip", Skipped, 0),
            check("early", Running, 100),
            check("nostart", Running, 0),
        ];
        let names: Vec<&str> = listed(&cs).into_iter().map(|(_, c)| c.name.as_str()).collect();
        assert_eq!(names, ["broke", "early", "late", "nostart"]);
        assert_eq!(listed(&cs)[0].0, 3, "a row keeps its place in the answer, its key");
        assert_eq!(folded(&cs, 7).as_deref(), Some("1 queued \u{b7} 1 passed, 1 skipped"));
        assert_eq!(folded(&cs, 9).as_deref(), Some("1 queued \u{b7} 1 passed, 1 skipped, 2 more not shown"));
        assert_eq!(folded(&cs[..1], 1).as_deref(), Some("1 queued"));
        assert_eq!(folded(&cs[3..4], 1), None, "a failure alone folds nothing");
        assert_eq!(elapsed(&cs[2], 360), Some(60));
        assert_eq!(elapsed(&GhCheck { completed: 260, ..cs[3].clone() }, 9_999), Some(60));
        assert_eq!(elapsed(&cs[3], 9_999), None);
    }
}
