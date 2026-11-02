//! The renderer as it was before the region was diffed and the build indexed: linear scans,
//! a `String` per row, the whole region cleared and drawn again for every record. Kept as
//! the oracle the differential tests compare the fast one with; nothing else uses it.

use blaze_protocol::events::{
    Cache, CrateState, Event, Fns, Level, NodeBatch, NodeKind, NodeState, Order, Record, SLOW_NS,
    Slow, Step, Took,
};

use crate::{
    CYAN, Crated, DIM, Finished, GREEN, NEWS_NS, Progress, RED, RESET, Stepped, Strip, Unnamed,
    YELLOW, done_word, label, last_lines, plural,
};

pub fn took(ns: u64) -> String {
    let ms = ns / 1_000_000;
    match ms {
        0..=9_999 => format!("{}.{:02}s", ms / 1000, ms % 1000 / 10),
        10_000..=59_999 => format!("{}.{}s", ms / 1000, ms % 1000 / 100),
        _ => format!("{}m{:02}s", ms / 60_000, ms % 60_000 / 1000),
    }
}

fn paint(color: &str, text: &str, colored: bool) -> String {
    if colored { format!("{color}{text}{RESET}") } else { text.to_owned() }
}

fn said_line(level: Level, text: &str, color: bool) -> String {
    let (word, color_of) = match level {
        Level::Note => ("note", GREEN),
        Level::Warning => ("warning", YELLOW),
        Level::Error => ("error", RED),
    };
    format!("{}: {text}", paint(color_of, word, color))
}

#[derive(Debug, Clone)]
pub struct Noded {
    pub id: u32,
    pub kind: NodeKind,
    pub row: usize,
    pub deps: Vec<usize>,
}

#[derive(Default, Debug)]
pub struct Build {
    pub command: Option<String>,
    pub steps: Vec<Stepped>,
    pub graph: Vec<Noded>,
    pub crates: Vec<Crated>,
    pub cache: Option<Cache>,
    pub rebuilt: Vec<(String, String, blaze_protocol::events::Cause)>,
    pub unlisted: u64,
    pub refused: Vec<(String, String)>,
    pub finished: Option<Finished>,
    pub last_ns: u64,
    pub unnamed: Option<Unnamed>,
    pub critical: Vec<usize>,
    pub cores: Vec<(u64, u32)>,
    pub cores_of: u32,
    pub compiling: Vec<(Slow, u64)>,
    pub slowest: Vec<Slow>,
    pub idle: Option<(u64, Step)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Nothing,
    Began(usize),
    Ended(usize),
    Crate { at: usize, moved: bool },
    Said(Level, String),
    Refused(usize),
    Rebuilt(usize),
    Finished,
}

fn row_of(kind: NodeKind, name: &str) -> (String, Option<&'static str>) {
    let own = |what: &str, word| (format!("{name} {what}"), Some(word));
    match kind {
        NodeKind::Names | NodeKind::Bodies | NodeKind::Codegen | NodeKind::Std => {
            (name.to_owned(), None)
        }
        NodeKind::Script => own("build script", "compiling"),
        NodeKind::ScriptRun => own("build script", "running"),
        NodeKind::Macro => own("proc macro", "linking"),
        NodeKind::Fetch => own("source", "fetching"),
        NodeKind::Manifest => (name.to_owned(), Some("reading")),
        NodeKind::Store => own("to the store", "writing"),
        NodeKind::Link => own("link", "linking"),
        NodeKind::Test => own("tests", "running"),
    }
}

fn crate_state(kind: NodeKind, state: &NodeState) -> CrateState {
    match (kind, state) {
        (_, NodeState::Waiting | NodeState::Ready) => CrateState::Waiting,
        (NodeKind::Names, NodeState::Running) => CrateState::Resolving,
        (NodeKind::Bodies, NodeState::Running) => CrateState::Checking,
        (_, NodeState::Running) => CrateState::Running,
        (NodeKind::Names | NodeKind::Bodies, NodeState::Built | NodeState::Cached { .. }) => {
            CrateState::Resolved
        }
        (_, NodeState::Built) => CrateState::Built,
        (_, NodeState::Cached { .. }) => CrateState::UpToDate,
        (_, NodeState::Failed { .. } | NodeState::Skipped { .. } | NodeState::Cancelled) => {
            CrateState::Failed
        }
    }
}

fn waiting(name: &str, at: u64) -> Crated {
    let (fns, state) = (Fns::default(), CrateState::Waiting);
    let name = name.to_owned();
    Crated { name, state, doing: None, fns, began_ns: at, ended_ns: None, since_ns: at, busy_ns: 0 }
}

fn count(stepped: &Stepped) -> Option<String> {
    let progress = stepped.progress.as_ref()?;
    let counts = stepped.step.counts();
    Some(match progress.total {
        0 => format!("{} {counts}", progress.done),
        total => format!("{}/{total} {counts}", progress.done),
    })
}

impl Build {
    fn noded(&mut self, id: u32, kind: NodeKind, name: &str, deps: &[u32], at: u64) {
        let row = self.row(&row_of(kind, name).0, at);
        let deps = deps.iter().filter_map(|dep| self.node(*dep)).collect();
        self.graph.push(Noded { id, kind, row, deps });
    }

    pub fn apply(&mut self, record: &Record) -> Change {
        let at = record.at_ns;
        self.last_ns = self.last_ns.max(at);
        match &record.event {
            Event::Began { command, .. } => {
                self.command = Some(command.clone());
                Change::Nothing
            }
            Event::Started { step, of } => {
                self.named(at);
                let of = of.clone();
                let (began_ns, ended_ns, progress) = (at, None, None);
                self.steps.push(Stepped { step: *step, of, began_ns, ended_ns, progress });
                Change::Began(self.steps.len() - 1)
            }
            Event::Progress { step, done, total, longest } => {
                let open =
                    self.steps.iter().rposition(|it| it.step == *step && it.ended_ns.is_none());
                if let Some(index) = open {
                    let longest = longest.clone();
                    self.steps[index].progress =
                        Some(Progress { done: *done, total: *total, longest });
                }
                Change::Nothing
            }
            Event::Node { id, kind, name, deps } => {
                self.noded(*id, *kind, name, deps, at);
                Change::Nothing
            }
            Event::Nodes { first, kinds, names, name_at, deps, deps_at } => {
                let mut order = Order::default();
                match NodeBatch::new(*first, kinds, names, name_at, deps, deps_at, &mut order) {
                    // Each after those of it it waits for, the first that can go first, by a
                    // search from the start each time.
                    Ok(batch) => {
                        let len = batch.len();
                        let of_it = |dep: u32| dep.checked_sub(*first).filter(|&it| it < len as u32);
                        let mut placed = vec![false; len];
                        while let Some(at_node) = (0..len).find(|&it| {
                            !placed[it]
                                && batch.node(it).deps.iter().all(|&dep| {
                                    of_it(dep).is_none_or(|dep| placed[dep as usize])
                                })
                        }) {
                            placed[at_node] = true;
                            let node = batch.node(at_node);
                            self.noded(node.id, node.kind, node.name, node.deps, at);
                        }
                        Change::Nothing
                    }
                    Err(why) => Change::Said(Level::Error, why.to_string()),
                }
            }
            Event::Moved { id, state } => match self.node(*id) {
                Some(node) => {
                    let Noded { kind, row, .. } = self.graph[node];
                    let krate = &mut self.crates[row];
                    krate.doing = row_of(kind, "").1;
                    let moved = krate.moved(crate_state(kind, state), at);
                    Change::Crate { at: row, moved }
                }
                None => Change::Nothing,
            },
            Event::Cache { cache } => {
                self.cache = Some(cache.clone());
                Change::Nothing
            }
            Event::Ended { step, of } => {
                let open = self
                    .steps
                    .iter()
                    .rposition(|it| it.step == *step && it.of == *of && it.ended_ns.is_none());
                match open {
                    Some(index) => {
                        self.steps[index].ended_ns = Some(at);
                        if self.running().next().is_none() {
                            self.idle = Some((at, *step));
                        }
                        Change::Ended(index)
                    }
                    None => Change::Nothing,
                }
            }
            Event::Crate { name, state, fns } => {
                let rows = self.crates.len();
                let index = self.row(name, at);
                let krate = &mut self.crates[index];
                let moved = krate.moved(*state, at) || index == rows;
                krate.fns = *fns;
                Change::Crate { at: index, moved }
            }
            Event::Rebuilt { name, krate, cause } => {
                self.rebuilt.push((name.clone(), krate.clone(), cause.clone()));
                Change::Rebuilt(self.rebuilt.len() - 1)
            }
            Event::Unlisted { count } => {
                self.unlisted += count;
                Change::Nothing
            }
            Event::Refused { name, why } => {
                self.refused.push((name.clone(), why.clone()));
                Change::Refused(self.refused.len() - 1)
            }
            Event::Said { level, text } => Change::Said(*level, text.clone()),
            Event::Finished { ok, summary } => {
                self.named(at);
                let (chain, chain_ns) = self.critical_path(at);
                let chain = chain.iter().rev().map(|&it| self.crates[it].name.clone()).collect();
                let (ok, summary, unnamed) = (*ok, summary.clone(), self.unnamed);
                let cores = summary.slowest.iter().map(|it| self.busy_in(it, at)).collect();
                let cores_of = self.cores_of;
                self.compiling.clear();
                let slowest = self.slowest.clone();
                self.finished = Some(Finished {
                    ok,
                    summary,
                    at_ns: at,
                    chain,
                    chain_ns,
                    unnamed,
                    cores,
                    cores_of,
                    slowest,
                });
                Change::Finished
            }
            Event::Critical { path } => {
                self.critical = path.iter().filter_map(|&it| self.node(it)).collect();
                Change::Nothing
            }
            Event::Cores { busy, of } => {
                self.cores.push((at, *busy));
                self.cores_of = *of;
                Change::Nothing
            }
            Event::Compiling { functions } => {
                let since = |it: &Slow| at.saturating_sub(it.ns);
                self.compiling = functions.iter().map(|it| (it.clone(), since(it))).collect();
                Change::Nothing
            }
            Event::Slowest { functions } => {
                self.slowest.clone_from(functions);
                Change::Nothing
            }
            Event::Functions { .. } => Change::Nothing,
        }
    }

    pub fn compiling_of(&self, krate: &str, now_ns: u64) -> Vec<(&str, u64)> {
        let of = self.compiling.iter().filter(|(it, _)| it.krate == krate);
        let of = of.map(|(it, since)| (it.name.as_str(), now_ns.saturating_sub(*since)));
        let mut of: Vec<_> = of.filter(|it| it.1 >= SLOW_NS).collect();
        of.sort_by_key(|it| std::cmp::Reverse(it.1));
        of
    }

    fn named(&mut self, at: u64) {
        let Some((since, after)) = self.idle.take() else { return };
        let took_ns = at.saturating_sub(since);
        if self.unnamed.is_none_or(|it| took_ns > it.took_ns) {
            self.unnamed = Some(Unnamed { took_ns, after });
        }
    }

    pub fn strip(&self, now_ns: u64) -> Vec<Strip> {
        let mut strip: Vec<Strip> = Vec::new();
        for stepped in &self.steps {
            let took_ns = stepped.ended_ns.unwrap_or(now_ns).saturating_sub(stepped.began_ns);
            let running = stepped.ended_ns.is_none();
            let count = count(stepped);
            match strip.iter_mut().find(|it| it.step == stepped.step) {
                Some(it) => {
                    it.took_ns += took_ns;
                    it.running |= running;
                    it.count = count.or(it.count.take());
                }
                None => strip.push(Strip { step: stepped.step, running, took_ns, count }),
            }
        }
        strip.retain(|it| it.running || it.took_ns >= 50_000_000);
        strip
    }

    pub fn busy_at(&self, at: u64) -> Option<u32> {
        let after = self.cores.partition_point(|it| it.0 <= at);
        after.checked_sub(1).map(|it| self.cores[it].1)
    }

    fn busy_between(&self, from: u64, to: u64) -> Option<(f64, u64)> {
        let first = self.busy_at(from).or_else(|| self.cores.first().map(|it| it.1))?;
        let (mut sum, mut at, mut level) = (0.0, from, first);
        for &(when, busy) in self.cores.iter().filter(|it| it.0 > from && it.0 < to) {
            sum += f64::from(level) * (when - at) as f64;
            (at, level) = (when, busy);
        }
        sum += f64::from(level) * to.saturating_sub(at) as f64;
        let spent = to.saturating_sub(from);
        (spent > 0).then(|| (sum / spent as f64 / 100.0, spent))
    }

    fn busy_in(&self, took: &Took, now_ns: u64) -> Option<f64> {
        let (mut sum, mut spent) = (0.0, 0);
        for stepped in self
            .steps
            .iter()
            .filter(|it| it.step == took.step && (it.step == Step::Codegen || it.of == took.of))
        {
            let ended = stepped.ended_ns.unwrap_or(now_ns);
            if let Some((busy, ns)) = self.busy_between(stepped.began_ns, ended) {
                sum += busy * ns as f64;
                spent += ns;
            }
        }
        (spent > 0).then(|| sum / spent as f64)
    }

    pub fn row(&mut self, name: &str, at: u64) -> usize {
        match self.crates.iter().position(|it| it.name == name) {
            Some(row) => row,
            None => {
                self.crates.push(waiting(name, at));
                self.crates.len() - 1
            }
        }
    }

    pub fn node(&self, id: u32) -> Option<usize> {
        self.graph.iter().rposition(|it| it.id == id)
    }

    pub fn running(&self) -> impl Iterator<Item = &Stepped> {
        self.steps.iter().filter(|it| it.ended_ns.is_none())
    }

    pub fn critical_path(&self, now_ns: u64) -> (Vec<usize>, u64) {
        let mut chain: Vec<usize> = Vec::new();
        for &node in self.critical.iter().rev() {
            let row = self.graph[node].row;
            if !chain.contains(&row) {
                chain.push(row);
            }
        }
        let (Some(&last), Some(&first)) = (chain.first(), chain.last()) else {
            return (Vec::new(), 0);
        };
        let last = &self.crates[last];
        let ended = match (last.ended_ns, last.is_busy()) {
            (Some(ended), _) => ended,
            (None, true) => now_ns,
            (None, false) => last.since_ns,
        };
        (chain, ended.saturating_sub(self.crates[first].began_ns))
    }

    pub fn fns(&self) -> Fns {
        self.crates.iter().fold(Fns::default(), |all, it| Fns {
            total: all.total + it.fns.total,
            done: all.done + it.fns.done,
            kept: all.kept + it.fns.kept,
        })
    }
}

fn staying(
    build: &Build,
    change: &Change,
    color: bool,
    live: bool,
    errors_said: bool,
) -> Vec<String> {
    let status =
        |word: &str, rest: &str| format!("{} {rest}", paint(GREEN, &format!("{word:>12}"), color));
    match change {
        Change::Ended(at) => {
            let stepped = &build.steps[*at];
            let took_ns = stepped.ended_ns.unwrap_or(stepped.began_ns) - stepped.began_ns;
            let said_by_crates = stepped.step == Step::Codegen && !build.crates.is_empty();
            match took_ns >= NEWS_NS && !said_by_crates {
                true => {
                    let rest = format!("{} in {}", stepped.what(), took(took_ns));
                    vec![status(done_word(stepped.step), &rest)]
                }
                false => Vec::new(),
            }
        }
        Change::Crate { at, moved: true } => {
            let krate = &build.crates[*at];
            let took_ns = krate.busy_ns(krate.ended_ns.unwrap_or(krate.since_ns));
            match krate.state {
                CrateState::Running
                    if !live
                        && !build.running().any(|it| {
                            it.step == Step::Codegen && it.of.as_deref() == Some(&krate.name)
                        }) =>
                {
                    vec![status("Compiling", &krate.name)]
                }
                CrateState::Built if build.graph.is_empty() && (live || took_ns >= NEWS_NS) => {
                    let compiled = krate.fns.compiled();
                    let fns = match krate.fns.total {
                        0 => String::new(),
                        _ => format!(
                            ": {compiled} {} compiled, {} kept",
                            plural(compiled, "fn"),
                            krate.fns.kept
                        ),
                    };
                    vec![status("Compiled", &format!("{}{fns} in {}", krate.name, took(took_ns)))]
                }
                _ => Vec::new(),
            }
        }
        Change::Said(level, text) => vec![said_line(*level, text, color)],
        Change::Refused(at) => {
            let (name, why) = &build.refused[*at];
            vec![said_line(Level::Error, &format!("blaze does not build `{name}`: {why}"), color)]
        }
        Change::Finished if !live => {
            build.finished.iter().flat_map(|it| last_lines(it, color, errors_said)).collect()
        }
        _ => Vec::new(),
    }
}

const SHOWN: usize = 6;
const TREE: usize = 10;
const FUNCTIONS: usize = 3;
const SPARK: usize = 20;

pub struct Live {
    pub build: Build,
    drawn: usize,
    width: usize,
    color: bool,
    errors_said: bool,
}

impl Live {
    pub fn new(width: usize, color: bool) -> Live {
        Live { build: Build::default(), drawn: 0, width: width.max(20), color, errors_said: false }
    }

    pub fn errors_said(self) -> Live {
        Live { errors_said: true, ..self }
    }

    pub fn resize(&mut self, width: usize) {
        self.width = width.max(20);
    }

    pub fn record(&mut self, record: &Record, now_ns: u64) -> String {
        let change = self.build.apply(record);
        let lines = staying(&self.build, &change, self.color, true, self.errors_said);
        match change {
            Change::Finished => self.finish_with(&lines),
            _ => self.redraw(&lines, now_ns),
        }
    }

    pub fn tick(&mut self, now_ns: u64) -> String {
        self.redraw(&[], now_ns)
    }

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

    pub fn snapshot(&self, now_ns: u64) -> Vec<String> {
        let Some(finished) = &self.build.finished else { return self.region(now_ns) };
        let mut lines: Vec<String> = self.strip(finished.at_ns).into_iter().collect();
        lines.extend(last_lines(finished, self.color, false));
        lines
    }

    pub fn region(&self, now_ns: u64) -> Vec<String> {
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

    fn strip(&self, now_ns: u64) -> Option<String> {
        let strip = self.build.strip(now_ns);
        if strip.is_empty() {
            return None;
        }
        let mark = |it: &Strip| if it.running { "\u{25cf}" } else { "\u{2713}" };
        let plain = |it: &Strip| {
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

    fn under_way(&self, now_ns: u64) -> Vec<String> {
        let build = &self.build;
        let since = |began: u64| took(now_ns.saturating_sub(began));
        let mut totals = Vec::new();
        let crates = build.crates.len();
        if crates > 0 && build.graph.is_empty() {
            let done = build.crates.iter().filter(|it| it.ended_ns.is_some()).count();
            totals.push(format!("{done}/{crates} crates"));
        }
        let fns = build.fns();
        let unknown = build.crates.iter().any(|it| it.fns.total == 0 && it.fns.done > 0);
        if fns.total > 0 && !unknown {
            totals.push(format!("{}/{} fns", fns.done, fns.total));
        } else if fns.done > 0 {
            totals.push(format!("{} fns", fns.done));
        }
        let step = build.running().last().or_else(|| build.steps.last());
        let (word, what) = match step {
            Some(step) => (step.step.word(), format!("{} {}", step.what(), since(step.began_ns))),
            None => ("Waiting", format!("for the daemon {}", since(0))),
        };
        let mut what = what;
        if let Some(count) = step.and_then(count) {
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
        running.sort_by_key(|it| it.began_ns);
        let shown = running.len().min(SHOWN);
        for (at, krate) in running.iter().take(shown).enumerate() {
            let branch = match at + 1 == shown && running.len() == shown {
                true => "\u{2514}\u{2500}",
                false => "\u{251c}\u{2500}",
            };
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

    fn tree(&self, now_ns: u64) -> Vec<String> {
        let build = &self.build;
        let graph = &build.graph;
        let krate = |node: usize| &build.crates[graph[node].row];
        let busy = |node: usize| krate(node).is_busy();
        let mut under_way = vec![false; graph.len()];
        let mut waited_for = vec![false; graph.len()];
        for (at, node) in graph.iter().enumerate() {
            under_way[at] = busy(at) || node.deps.iter().any(|&dep| under_way[dep]);
            for &dep in &node.deps {
                waited_for[dep] = true;
            }
        }
        let mut lines = Vec::new();
        let mut shown = vec![false; build.crates.len()];
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
            let word = match last {
                None => format!("{:12}", ""),
                Some(_) => format!("{:13}{indent}{glyph}", ""),
            };
            lines.push(self.fit(CYAN, &word, &rest));
            if it.is_busy() {
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

    fn functions(&self, krate: &str, now_ns: u64, under: &str) -> Vec<String> {
        let of = self.build.compiling_of(krate, now_ns);
        let of = of.into_iter().take(FUNCTIONS);
        of.map(|(name, ns)| self.fit(DIM, under, &format!("  {name} {}", took(ns)))).collect()
    }

    fn fit(&self, color: &str, word: &str, rest: &str) -> String {
        let room = self.width.saturating_sub(word.chars().count() + 2);
        let rest: String = match rest.chars().count() > room {
            true => rest.chars().take(room.saturating_sub(1)).chain(['\u{2026}']).collect(),
            false => rest.to_owned(),
        };
        format!("{} {rest}", paint(color, word, self.color))
    }
}

pub struct Plain {
    pub build: Build,
    said_ns: u64,
    begun: usize,
    color: bool,
    errors_said: bool,
}

impl Plain {
    pub fn new(color: bool) -> Plain {
        Plain { build: Build::default(), said_ns: 0, begun: 0, color, errors_said: false }
    }

    pub fn errors_said(self) -> Plain {
        Plain { errors_said: true, ..self }
    }

    pub fn record(&mut self, record: &Record) -> String {
        let mut said = self.begun_by(record.at_ns);
        let change = self.build.apply(record);
        let lines = match change {
            Change::Began(_) => Vec::new(),
            change => staying(&self.build, &change, self.color, false, self.errors_said),
        };
        if !lines.is_empty() {
            self.said_ns = record.at_ns;
        }
        said.extend(lines.into_iter().map(|it| it + "\n"));
        said
    }

    fn begun_by(&mut self, now_ns: u64) -> String {
        let mut said = String::new();
        while let Some(stepped) = self.build.steps.get(self.begun) {
            let ended = stepped.ended_ns.is_some();
            if !ended && now_ns.saturating_sub(stepped.began_ns) < NEWS_NS {
                break;
            }
            if !ended {
                let word = paint(GREEN, &format!("{:>12}", stepped.step.word()), self.color);
                said.push_str(&format!("{word} {}\n", stepped.what()));
                self.said_ns = now_ns;
            }
            self.begun += 1;
        }
        said
    }

    pub fn tick(&mut self, now_ns: u64) -> String {
        if self.build.finished.is_some() {
            return String::new();
        }
        let begun = self.begun_by(now_ns);
        if !begun.is_empty() || now_ns.saturating_sub(self.said_ns) < crate::HEARTBEAT_NS {
            return begun;
        }
        let Some(step) = self.build.running().last() else { return String::new() };
        self.said_ns = now_ns;
        let word = paint(GREEN, &format!("{:>12}", step.step.word()), self.color);
        let count = count(step).map(|it| format!(": {it}")).unwrap_or_default();
        let took = took(now_ns.saturating_sub(step.began_ns));
        format!("{word} {}{count} ({took})\n", step.what())
    }
}

// The last lines and `blaze why`'s steps as they were before each was written in place: a
// `String` per piece, joined.

pub fn tail(text: &str, most: usize) -> String {
    let count = text.chars().count();
    match count > most {
        true => ['\u{2026}'].into_iter().chain(text.chars().skip(count + 1 - most)).collect(),
        false => text.to_owned(),
    }
}

fn short(step: Step, of: Option<&str>) -> String {
    match of {
        None => label(step).to_owned(),
        Some(of) => match of.strip_prefix("the standard library ") {
            Some(rest) => format!("{} {rest}", label(step)),
            None => format!("{} {of}", label(step)),
        },
    }
}

fn causes(rebuilt: &[(String, blaze_protocol::events::Cause)]) -> String {
    let mut said = String::new();
    for (at, (name, cause)) in rebuilt.iter().enumerate() {
        if at > 0 {
            said.push_str(", ");
        }
        said.push_str(name);
        if rebuilt.get(at + 1).is_none_or(|next| next.1 != *cause) {
            said.push_str(&format!(" ({})", cause.short()));
        }
    }
    said
}

pub fn why_steps(build: &crate::Build, now_ns: u64) -> Vec<String> {
    const GAP_NS: u64 = 50_000_000;
    const STEP_NS: u64 = 10_000_000;
    let cores = |from: u64, to: u64| match build.busy_between(from, to) {
        Some((busy, _)) => format!("  {busy:.1}/{} cores", build.cores_of),
        None => String::new(),
    };
    let mut rows: Vec<(String, u64, String)> = Vec::new();
    let mut free = 0;
    for stepped in &build.steps {
        if stepped.began_ns.saturating_sub(free) >= GAP_NS {
            let gap = stepped.began_ns - free;
            rows.push(("at no step".into(), gap, cores(free, stepped.began_ns)));
        }
        let ended = stepped.ended_ns.unwrap_or(now_ns);
        free = free.max(ended);
        let took_ns = ended.saturating_sub(stepped.began_ns);
        if took_ns < STEP_NS {
            continue;
        }
        let mut what = short(stepped.step, stepped.of.as_deref());
        let of = stepped.of.as_deref().and_then(|it| it.rsplit(' ').next());
        if let Some(count) = stepped.count().filter(|it| it.rsplit(' ').next() != of) {
            what = format!("{what} {count}");
        }
        rows.push((what, took_ns, cores(stepped.began_ns, ended)));
    }
    if !rows.is_empty() && now_ns.saturating_sub(free) >= GAP_NS {
        rows.push(("at no step".into(), now_ns - free, cores(free, now_ns)));
    }
    let width = rows.iter().map(|it| it.0.chars().count()).max().unwrap_or(0);
    rows.iter()
        .map(|(what, ns, cores)| format!("{what:<width$}  {:>6}{cores}", took(*ns)))
        .collect()
}

pub fn why_lines(finished: &Finished, color: bool) -> Vec<String> {
    let summary = &finished.summary;
    let took_ns = crate::took_ns(finished);
    if took_ns < crate::WHY_NS || !finished.ok {
        return Vec::new();
    }
    let status =
        |word: &str, rest: &str| format!("{} {rest}", paint(GREEN, &format!("{word:>12}"), color));
    let mut lines = Vec::new();
    if !summary.slowest.is_empty() {
        let steps: Vec<String> = summary
            .slowest
            .iter()
            .enumerate()
            .map(|(at, it)| {
                let cores = match finished.cores.get(at).copied().flatten() {
                    Some(busy) if at == 0 => format!(" ({busy:.1}/{} cores)", finished.cores_of),
                    Some(busy) => format!(" ({busy:.1}/{})", finished.cores_of),
                    None => String::new(),
                };
                format!("{} {}{cores}", short(it.step, it.of.as_deref()), took(it.took_ns))
            })
            .collect();
        lines.push(status("Took", &format!("{}: {}", took(took_ns), steps.join(", "))));
    }
    if !finished.slowest.is_empty() {
        const LISTED: usize = 5;
        let listed = finished.slowest.iter().take(LISTED);
        let listed: Vec<String> =
            listed.map(|it| format!("{} {}", tail(&it.name, 48), took(it.ns))).collect();
        let more = match finished.slowest.len().saturating_sub(LISTED) {
            0 => String::new(),
            more => format!(" (and {more} more)"),
        };
        lines.push(status("Slowest", &format!("fns: {}{more}", listed.join(", "))));
    }
    if let Some(cache) = &summary.cache {
        let rest = match &cache.cold {
            Some(why) => Some(format!("cold: {why}")),
            None if cache.fetched > 0 => Some(format!(
                "{} found here, {} fetched from the remote store",
                cache.found, cache.fetched
            )),
            None => None,
        };
        lines.extend(rest.map(|rest| status("Cache", &rest)));
    }
    if finished.chain.len() > 1 && finished.chain_ns > 0 {
        let chain = finished.chain.join(" \u{2192} ");
        lines.push(status("Chain", &format!("{chain} {}", took(finished.chain_ns))));
    }
    if let Some(unnamed) = finished.unnamed.filter(|it| it.took_ns >= crate::UNNAMED_NS) {
        let after = unnamed.after.whole();
        let rest = format!("{} with no step named, after {after}", took(unnamed.took_ns));
        lines.push(said_line(Level::Warning, &rest, color));
    }
    if !lines.is_empty() {
        lines.push(paint(DIM, &format!("{:>12} `blaze why` says more", ""), color));
    }
    lines
}

pub fn summary_line(finished: &Finished, color: bool) -> String {
    let summary = &finished.summary;
    let took = took(crate::took_ns(finished));
    if !finished.ok {
        let why = summary.error.as_deref().unwrap_or("the build failed");
        return format!("{}: {why} (after {took})", paint(RED, "error", color));
    }
    let compiled = summary.fns.compiled();
    let (word, what) = if !summary.rebuilt.is_empty() {
        let listed = summary.rebuilt.len() as u64 + summary.more;
        let mut said = format!("{listed} {}: {}", plural(listed, "fn"), causes(&summary.rebuilt));
        if summary.more > 0 {
            said.push_str(&format!(" and {} more", summary.more));
        }
        ("Rebuilt", said)
    } else if compiled > 0 {
        let kept = summary.fns.kept;
        let crates = summary.crates;
        let said = format!(
            "{compiled} {} in {crates} {}, {kept} kept",
            plural(compiled, "fn"),
            plural(crates, "crate")
        );
        ("Compiled", said)
    } else if let Some(cache) = summary.cache.as_ref().filter(|it| it.found + it.fetched > 0) {
        let found = cache.found + cache.fetched;
        let said = format!(
            "from the store: {found} {} found, {} compiled",
            plural(found, "fn"),
            cache.compiled
        );
        ("Built", said)
    } else {
        ("Fresh", "nothing to rebuild".to_owned())
    };
    format!("{} {what}; {took}", paint(GREEN, &format!("{word:>12}"), color))
}
