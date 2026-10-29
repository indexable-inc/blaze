//! The terminal's rendering: a region redrawn in place below the lines that stay.

use blaze_protocol::events::{CrateState, Record};

use crate::{Build, CYAN, Change, DIM, GREEN, label, last_lines, paint, staying, took};

/// The most crates the region lists by name; the rest under way are counted.
const SHOWN: usize = 6;

/// The most lines of the tree of crates ([`Live::tree`]).
const TREE: usize = 10;

/// The most functions compiling shown under a crate ([`Live::functions`]).
const FUNCTIONS: usize = 3;

/// How many half seconds the line of how busy the cores were shows ([`Live::cores`]).
const SPARK: usize = 20;

/// Clears the region and draws it again as records come and time passes. What it returns is
/// written to the terminal as it is.
pub struct Live {
    build: Build,
    /// How many lines of the region are on the screen.
    drawn: usize,
    width: usize,
    color: bool,
    errors_said: bool,
}

impl Live {
    pub fn new(width: usize, color: bool) -> Live {
        Live { build: Build::default(), drawn: 0, width: width.max(20), color, errors_said: false }
    }

    /// For the command that asked for the build, which says its errors in full after it: a
    /// failed build has no last line of its own ([`crate::last_line`]).
    pub fn errors_said(self) -> Live {
        Live { errors_said: true, ..self }
    }

    pub fn build(&self) -> &Build {
        &self.build
    }

    pub fn resize(&mut self, width: usize) {
        self.width = width.max(20);
    }

    /// What to write for `record` at `now_ns`: the lines it adds that stay, and the region
    /// again below them; at the build's end, the summary in the region's place.
    pub fn record(&mut self, record: &Record, now_ns: u64) -> String {
        let change = self.build.apply(record);
        let lines = staying(&self.build, &change, self.color, true, self.errors_said);
        match change {
            Change::Finished => self.finish_with(&lines),
            _ => self.redraw(&lines, now_ns),
        }
    }

    /// The region drawn again for the time that passed.
    pub fn tick(&mut self, now_ns: u64) -> String {
        self.redraw(&[], now_ns)
    }

    /// The region cleared, and the summary in its place when the build finished.
    pub fn finish(&mut self) -> String {
        self.finish_with(&[])
    }

    fn finish_with(&mut self, lines: &[String]) -> String {
        let mut out = self.cleared();
        for line in lines {
            out.push_str(line);
            out.push('\n');
        }
        let finished = self.build.finished.as_ref();
        for last in
            finished.map(|it| last_lines(it, self.color, self.errors_said)).unwrap_or_default()
        {
            out.push_str(&last);
            out.push('\n');
        }
        out
    }

    fn cleared(&mut self) -> String {
        let drawn = std::mem::take(&mut self.drawn);
        match drawn {
            0 => String::new(),
            _ => format!("\r\x1b[{drawn}A\x1b[J"),
        }
    }

    fn redraw(&mut self, lines: &[String], now_ns: u64) -> String {
        let mut out = self.cleared();
        for line in lines {
            out.push_str(line);
            out.push('\n');
        }
        let region = self.region(now_ns);
        self.drawn = region.len();
        for line in region {
            out.push_str(&line);
            out.push('\n');
        }
        out
    }

    /// What `blaze status` says of the build: the region at `now_ns` while it goes on, and
    /// once it is over every kind of step it did and its last lines.
    pub fn snapshot(&self, now_ns: u64) -> Vec<String> {
        let Some(finished) = &self.build.finished else { return self.region(now_ns) };
        let mut lines: Vec<String> = self.strip(finished.at_ns).into_iter().collect();
        lines.extend(last_lines(finished, self.color, false));
        lines
    }

    /// The region at `now_ns`: the command and how long it has taken, every kind of step so
    /// far, what is under way with the time it has taken, and a count of what is done. Each
    /// line fits the width, so the region is as many rows as lines.
    ///
    /// ```text
    /// blaze build · cold: the store is empty                                     12.4s
    /// ✓ workspace 0.31s  ✓ names 2.37s  ● build scripts 3/14 steps 2.10s
    ///     Building anyhow, compiling its build script 1.20s · 3/14 steps
    /// ```
    fn region(&self, now_ns: u64) -> Vec<String> {
        let build = &self.build;
        if build.finished.is_some() {
            return Vec::new();
        }
        let mut lines = vec![self.header(now_ns)];
        lines.extend(self.strip(now_ns));
        lines.extend(self.cores(now_ns));
        lines.extend(self.under_way(now_ns));
        lines
    }

    /// `blaze build · cold: the store is empty`, and how long it has taken at the right.
    fn header(&self, now_ns: u64) -> String {
        let build = &self.build;
        let mut left = format!("blaze {}", build.command.as_deref().unwrap_or("build"));
        if let Some(why) = build.cache.as_ref().and_then(|it| it.cold.as_deref()) {
            left.push_str(&format!(" \u{b7} cold: {why}"));
        }
        let right = took(now_ns);
        let room = self.width.saturating_sub(right.chars().count() + 2);
        let left: String = match left.chars().count() > room {
            true => left.chars().take(room.saturating_sub(1)).chain(['\u{2026}']).collect(),
            false => left,
        };
        let pad = self.width.saturating_sub(left.chars().count() + right.chars().count() + 1);
        format!("{}{:pad$}{}", paint(GREEN, &left, self.color), "", paint(DIM, &right, self.color))
    }

    /// Each kind of step so far, done or under way, with its count and time; the first done
    /// ones counted when they don't fit.
    fn strip(&self, now_ns: u64) -> Option<String> {
        let strip = self.build.strip(now_ns);
        if strip.is_empty() {
            return None;
        }
        let mark = |it: &crate::Strip| if it.running { "\u{25cf}" } else { "\u{2713}" };
        let plain = |it: &crate::Strip| {
            let count = it.count.as_ref().map(|it| format!(" {it}")).unwrap_or_default();
            format!("{} {}{count} {}", mark(it), label(it.step), took(it.took_ns))
        };
        let texts: Vec<String> = strip.iter().map(plain).collect();
        let counted =
            |from: usize| if from > 0 { format!("\u{2713} {from} more  ") } else { String::new() };
        let width = |from: usize| {
            let shown: usize = texts[from..].iter().map(|it| it.chars().count() + 2).sum();
            counted(from).chars().count() + shown - 2
        };
        let room = self.width.saturating_sub(1);
        let mut first = 0;
        while first + 1 < texts.len() && !strip[first].running && width(first) > room {
            first += 1;
        }
        // Too narrow for even the last: its text alone, cut to the width.
        if width(first) > room {
            let line = counted(first) + &texts[first..].join("  ");
            return Some(line.chars().take(room).collect());
        }
        let mut line = counted(first);
        for (at, it) in strip.iter().enumerate().skip(first) {
            if at > first {
                line.push_str("  ");
            }
            let color = if it.running { CYAN } else { GREEN };
            line.push_str(&paint(color, mark(it), self.color));
            line.push_str(&texts[at][mark(it).len()..]);
        }
        Some(line)
    }

    /// How busy the worker's cores were over the last [`SPARK`] half seconds, and now:
    /// `cores ▁▂▇█████▇▆▃▁▁▁ 1.0/16 busy`, so that a build on one core shows at once.
    fn cores(&self, now_ns: u64) -> Option<String> {
        const BLOCKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
        let build = &self.build;
        let now = build.busy_at(now_ns)?;
        let of = build.cores_of.max(1);
        let spark: String = (0..SPARK)
            .map(|at| {
                let back = (SPARK - 1 - at) as u64 * 500_000_000;
                match now_ns.checked_sub(back).and_then(|it| build.busy_at(it)) {
                    Some(busy) => {
                        let level =
                            (u64::from(busy) * 7 + u64::from(of) * 50) / (u64::from(of) * 100);
                        BLOCKS[level.min(7) as usize]
                    }
                    None => ' ',
                }
            })
            .collect();
        let rest = format!("{spark} {}.{}/{of} busy", now / 100, now % 100 / 10);
        Some(self.fit(DIM, "cores", &rest))
    }

    /// The step under way and what it does, and the crates under way.
    fn under_way(&self, now_ns: u64) -> Vec<String> {
        let build = &self.build;
        let since = |began: u64| took(now_ns.saturating_sub(began));
        let mut totals = Vec::new();
        let crates = build.crates.len();
        // With a graph, the tree's last line counts the crates.
        if crates > 0 && build.graph.is_empty() {
            let done = build.crates.iter().filter(|it| it.ended_ns.is_some()).count();
            totals.push(format!("{done}/{crates} crates"));
        }
        let fns = build.fns();
        // A crate whose total is not known yet counts in the done and not in the total.
        let unknown = build.crates.iter().any(|it| it.fns.total == 0 && it.fns.done > 0);
        if fns.total > 0 && !unknown {
            totals.push(format!("{}/{} fns", fns.done, fns.total));
        } else if fns.done > 0 {
            totals.push(format!("{} fns", fns.done));
        }
        // Between one step's end and the next one's start, the step that ended.
        let step = build.running().last().or_else(|| build.steps.last());
        let (word, what) = match step {
            Some(step) => (step.step.word(), format!("{} {}", step.what(), since(step.began_ns))),
            None => ("Waiting", format!("for the daemon {}", since(0))),
        };
        let mut what = what;
        // The step's own count says more than the totals: what it is doing now.
        if let Some(count) = step.and_then(|it| it.count()) {
            totals.insert(0, count);
        }
        if let Some(longest) = step.and_then(|it| it.progress.as_ref()?.longest.as_deref()) {
            totals.push(format!("longest {longest}"));
        }
        for total in &totals {
            what.push_str(" \u{b7} ");
            what.push_str(total);
        }
        let mut lines = vec![self.fit(GREEN, &format!("{word:>12}"), &what)];
        if !build.graph.is_empty() {
            lines.extend(self.tree(now_ns));
            return lines;
        }
        let mut running: Vec<_> =
            build.crates.iter().filter(|it| it.state == CrateState::Running).collect();
        // The longest under way first: what the build waits on.
        running.sort_by_key(|it| it.began_ns);
        let shown = running.len().min(SHOWN);
        for (at, krate) in running.iter().take(shown).enumerate() {
            let branch = match at + 1 == shown && running.len() == shown {
                true => "\u{2514}\u{2500}",
                false => "\u{251c}\u{2500}",
            };
            // A compile under way knows how many it did, and not yet of how many.
            let fns = match (krate.fns.total, krate.fns.done) {
                (0, 0) => String::new(),
                (0, done) => format!("  {done} fns"),
                (total, done) => format!("  {done}/{total} fns"),
            };
            let rest = format!("{}{fns}  {}", krate.name, since(krate.began_ns));
            lines.push(self.fit(CYAN, &format!("{:>14}", branch), &rest));
            let under = if at + 1 == shown && running.len() == shown { "" } else { "\u{2502}" };
            lines.extend(self.functions(&krate.name, now_ns, &format!("{under:>13}")));
        }
        if running.len() > shown {
            let more = format!("{} more under way", running.len() - shown);
            lines.push(self.fit(CYAN, &format!("{:>14}", "\u{2514}\u{2500}"), &more));
        }
        let up_to_date = build.crates.iter().filter(|it| it.state == CrateState::UpToDate).count();
        let built = build.crates.iter().filter(|it| it.state == CrateState::Built).count();
        if up_to_date + built > 0 {
            let done = format!("{built} built, {up_to_date} up to date");
            lines.push(self.fit(DIM, &format!("{:>12}", ""), &done));
        }
        lines
    }

    /// The crates under way as a tree, from the program down what it waits for: each crate
    /// being resolved, checked or compiled with what it did so far and for how long, under
    /// the crates waiting for it; what is done is left out, and counted in the last line with
    /// what held the build up once the graph says it ([`Build::critical_path`]).
    ///
    /// ```text
    ///             anim waiting
    ///             ├─ wgpu  compiling 1201 fns  4.10s
    ///             └─ winit  resolving  0.80s
    ///             98 done (40 up to date) · 2 under way · 21 waiting
    /// ```
    fn tree(&self, now_ns: u64) -> Vec<String> {
        let build = &self.build;
        let graph = &build.graph;
        let krate = |node: usize| &build.crates[graph[node].row];
        let busy = |node: usize| krate(node).is_busy();
        // Whether the node or something it waits for is under way; the graph lists each
        // node after what it waits for.
        let mut under_way = vec![false; graph.len()];
        let mut waited_for = vec![false; graph.len()];
        for (at, node) in graph.iter().enumerate() {
            under_way[at] = busy(at) || node.deps.iter().any(|&dep| under_way[dep]);
            for &dep in &node.deps {
                waited_for[dep] = true;
            }
        }
        let mut lines = Vec::new();
        // By row: the nodes of a crate are one line.
        let mut shown = vec![false; build.crates.len()];
        // Depth first from each node nothing waits for, the branch busy longest first.
        // A root has no glyph; `indent` is what the lines of its children start with.
        let mut stack: Vec<(usize, String, Option<bool>)> = (0..graph.len())
            .rev()
            .filter(|&at| !waited_for[at] && under_way[at])
            .map(|at| (at, String::new(), None))
            .collect();
        while let Some((node, indent, last)) = stack.pop() {
            let row = graph[node].row;
            if shown[row] {
                continue;
            }
            if lines.len() == TREE {
                break;
            }
            shown[row] = true;
            let glyph = match last {
                None => "",
                Some(true) => "\u{2514}\u{2500}",
                Some(false) => "\u{251c}\u{2500}",
            };
            let it = krate(node);
            let rest = match it.is_busy() {
                true => {
                    let fns = match (it.fns.total, it.fns.done) {
                        (_, 0) => String::new(),
                        (0, done) => format!(" {done} fns"),
                        (total, done) => format!(" {done}/{total} fns"),
                    };
                    let since = took(now_ns.saturating_sub(it.since_ns));
                    format!("{}  {}{fns}  {since}", it.name, it.word())
                }
                false => format!("{} waiting", it.name),
            };
            // A crate's branches start under its name.
            let word = match last {
                None => format!("{:12}", ""),
                Some(_) => format!("{:13}{indent}{glyph}", ""),
            };
            lines.push(self.fit(CYAN, &word, &rest));
            if it.is_busy() {
                // Under the name, beside the branches its children hang from.
                let under = match last {
                    None => format!("{:12}", ""),
                    Some(last) => {
                        format!("{:13}{indent}{}", "", if last { "  " } else { "\u{2502} " })
                    }
                };
                lines.extend(self.functions(&it.name, now_ns, &under));
            }
            let mut deps: Vec<usize> = graph[node]
                .deps
                .iter()
                .copied()
                .filter(|&it| under_way[it] && !shown[graph[it].row])
                .collect();
            deps.sort_by_key(|&it| std::cmp::Reverse(krate(it).busy_ns(now_ns)));
            // Deeper than this, a chain goes on at the same indent.
            let indent = match last {
                Some(last) if indent.chars().count() < 3 * 5 => {
                    indent + if last { "   " } else { "\u{2502}  " }
                }
                _ => indent,
            };
            for (at, &dep) in deps.iter().enumerate().rev() {
                stack.push((dep, indent.clone(), Some(at + 1 == deps.len())));
            }
        }
        let mut rows: Vec<usize> = graph.iter().map(|it| it.row).collect();
        rows.sort_unstable();
        rows.dedup();
        let hidden = rows.iter().filter(|&&it| build.crates[it].is_busy() && !shown[it]).count();
        if hidden > 0 {
            let more = format!("{hidden} more under way");
            lines.push(self.fit(CYAN, &format!("{:12}", ""), &more));
        }
        let count = |state: fn(CrateState) -> bool| {
            build.crates.iter().filter(|it| state(it.state)).count()
        };
        let done = count(CrateState::is_done);
        let up_to_date = count(|it| it == CrateState::UpToDate);
        let busy_now = count(|it| {
            matches!(it, CrateState::Resolving | CrateState::Checking | CrateState::Running)
        });
        let waiting = build.crates.len() - done - busy_now;
        let mut footer = format!("{done} done");
        if up_to_date > 0 {
            footer.push_str(&format!(" ({up_to_date} up to date)"));
        }
        footer.push_str(&format!(" \u{b7} {busy_now} under way \u{b7} {waiting} waiting"));
        let (chain, chain_ns) = build.critical_path(now_ns);
        if chain.len() > 1 && chain_ns > 0 {
            let names: Vec<&str> = chain.iter().map(|&it| build.crates[it].name.as_str()).collect();
            footer.push_str(&format!(
                " \u{b7} longest chain {}: {}",
                took(chain_ns),
                names.join(" \u{2190} ")
            ));
        }
        lines.push(self.fit(DIM, &format!("{:12}", ""), &footer));
        lines
    }

    /// The functions of crate `krate` compiling longest ([`Build::compiling_of`]), the top
    /// [`FUNCTIONS`] of them, each a line under the crate's starting with `under`.
    ///
    /// ```text
    ///             ├─ regex_automata  compiling 7318 fns  2.55s
    ///             │    regex_automata::meta::Regex::search 1.20s
    /// ```
    fn functions(&self, krate: &str, now_ns: u64, under: &str) -> Vec<String> {
        let of = self.build.compiling_of(krate, now_ns);
        let of = of.into_iter().take(FUNCTIONS);
        of.map(|(name, ns)| self.fit(DIM, under, &format!("  {name} {}", took(ns)))).collect()
    }

    /// `word` painted and `rest` after it, cut to the width: a line that wrapped would be two
    /// rows, and the region would no longer be cleared whole.
    fn fit(&self, color: &str, word: &str, rest: &str) -> String {
        let room = self.width.saturating_sub(word.chars().count() + 2);
        let rest: String = match rest.chars().count() > room {
            true => rest.chars().take(room.saturating_sub(1)).chain(['\u{2026}']).collect(),
            false => rest.to_owned(),
        };
        format!("{} {rest}", paint(color, word, self.color))
    }
}
