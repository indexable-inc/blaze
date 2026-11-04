//! A build's critical path: from the run that ended last, back in time through each lane's
//! intervals, crossing from a wait to the job that woke it, and from a job's start to the job
//! that gave it to the pool (or, given by a thread on no fiber, the lane of the span it was
//! given inside), when the record names it. The links are contiguous, so their
//! durations add up to the path's length.

use std::alloc::Global;

use blaze_hash::HashMap;

use super::{Model, State};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkKind {
    /// Running on a core.
    Ran,
    /// Runnable, waiting for a core.
    Queued,
    /// Parked on a wait whose waker the record does not name, or one the walk could not cross.
    Waited,
    /// From the waker's last run to the resume: the wake's latency, queueing included.
    Wake,
    /// From the spawner's end (or the end of the span it was given inside) to the job's
    /// start: queued for a core after what gave it to the pool ended.
    Spawned,
    Unrecorded,
}

impl LinkKind {
    pub const ALL: [LinkKind; 6] = [
        LinkKind::Ran,
        LinkKind::Waited,
        LinkKind::Wake,
        LinkKind::Queued,
        LinkKind::Spawned,
        LinkKind::Unrecorded,
    ];

    pub fn name(self) -> &'static str {
        match self {
            LinkKind::Ran => "ran",
            LinkKind::Queued => "queued",
            LinkKind::Waited => "waited",
            LinkKind::Wake => "wake",
            LinkKind::Spawned => "spawned",
            LinkKind::Unrecorded => "unrecorded",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Link {
    pub kind: LinkKind,
    /// The lane and the interval of it ([`Model::lanes`]): for [`LinkKind::Spawned`], the job's
    /// first interval, the gap being before it.
    pub lane: usize,
    pub interval: usize,
    pub from_ns: i64,
    pub to_ns: i64,
    /// The wait ([`Model::waits`]) this link's end woke: the path crossed from it to here.
    pub wakes: Option<usize>,
}

impl Link {
    pub fn dur_ns(&self) -> i64 {
        self.to_ns - self.from_ns
    }
}

/// Where the walk stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reached {
    BuildStart,
    /// The start of a lane the record names no spawner of: a job given to the pool by a thread
    /// on no fiber inside no span the model has, or one whose spawner has no lane.
    LaneStart {
        lane: usize,
    },
    /// Its bound of steps, [`fuel`] of the model.
    Fuel {
        steps: usize,
    },
    /// No lane ran at all.
    NoRun,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Path {
    /// First link first.
    pub links: Vec<Link>,
    pub reached: Reached,
}

impl Path {
    pub fn start_ns(&self) -> Option<i64> {
        self.links.first().map(|it| it.from_ns)
    }

    pub fn end_ns(&self) -> Option<i64> {
        self.links.last().map(|it| it.to_ns)
    }

    pub fn len_ns(&self) -> i64 {
        self.links.iter().map(Link::dur_ns).sum()
    }
}

/// The walk's bound: each step either moves back in time or crosses to a waker once at that
/// instant, and a lane's wait can be met again only at an earlier instant.
pub fn fuel(model: &Model) -> usize {
    4 * model.lanes.iter().map(|it| it.intervals.len()).sum::<usize>() + 16
}

/// The critical path of `model`.
pub fn critical(model: &Model) -> Path {
    // alloc: the path, once a report
    let mut links = Vec::new();
    let last = model
        .lanes
        .iter()
        .enumerate()
        .flat_map(|(l, lane)| lane.intervals.iter().map(move |it| (l, it)))
        .filter(|(_, it)| matches!(it.state, State::Running { .. }))
        .max_by_key(|(l, it)| (it.to_ns, std::cmp::Reverse(*l)));
    let Some((mut lane, last)) = last else { return Path { links, reached: Reached::NoRun } };
    let mut t = last.to_ns;
    let start = model.bounds.start_ns;
    let (mut wakes, mut crossed_at) = (None, None);
    let fuel = fuel(model);
    let mut reached = Reached::Fuel { steps: fuel };
    for _ in 0..fuel {
        if t <= start {
            reached = Reached::BuildStart;
            break;
        }
        let Some(i) = model.lanes[lane].at_or_before(t) else {
            // A job's start stands on its spawner's work up to it; from the spawner's end (or
            // the end of the span it was given inside), if it ended first, the job waited for a
            // core.
            let spawner = model.lanes[lane].spawner.filter(|&it| {
                let it = &model.lanes[it];
                !it.intervals.is_empty() && it.begin_ns() < t
            });
            match spawner {
                Some(spawner) if crossed_at != Some(t) => {
                    let gave_until = match model.lanes[lane].creator {
                        Some(span) => model.spans[span].end_ns(),
                        None => model.lanes[spawner].end_ns(),
                    };
                    let spawner_end = gave_until.max(start);
                    if spawner_end < t {
                        links.push(Link {
                            kind: LinkKind::Spawned,
                            lane,
                            interval: 0,
                            from_ns: spawner_end,
                            to_ns: t,
                            wakes: wakes.take(),
                        });
                        t = spawner_end;
                    }
                    crossed_at = Some(t);
                    lane = spawner;
                    continue;
                }
                _ => {
                    reached = Reached::LaneStart { lane };
                    break;
                }
            }
        };
        let interval = &model.lanes[lane].intervals[i];
        let from = interval.from_ns.max(start);
        let mut link = |kind: LinkKind, from: i64, to: i64| {
            links.push(Link {
                kind,
                lane,
                interval: i,
                from_ns: from,
                to_ns: to,
                wakes: wakes.take(),
            });
        };
        let kind = match interval.state {
            State::Running { .. } => LinkKind::Ran,
            State::Queued { .. } => LinkKind::Queued,
            State::Unrecorded { .. } => LinkKind::Unrecorded,
            State::Parked { wait } | State::ParkedOrQueued { wait } => {
                let waker = model.waits[wait].waker.lane().filter(|&it| {
                    model.lanes[it].begin_ns() < t && model.lanes[it].end_ns() > from
                });
                match waker {
                    Some(waker) if crossed_at != Some(t) => {
                        // The waker's work up to the wake stands on the path in place of the wait;
                        // from its last run to the resume is the wake's latency.
                        let waker_end = model.lanes[waker].end_ns().max(from);
                        if waker_end < t {
                            link(LinkKind::Wake, waker_end, t);
                            t = waker_end;
                        }
                        wakes = Some(wait);
                        crossed_at = Some(t);
                        lane = waker;
                        continue;
                    }
                    _ => LinkKind::Waited,
                }
            }
        };
        link(kind, from, t);
        t = from;
    }
    links.reverse();
    Path { links, reached }
}

/// The critical path's running links cut by the innermost span open on their lane: (from, to,
/// span, lane), in path order; `None` where no span of the lane is open.
pub fn segments(model: &Model, path: &Path) -> Vec<(i64, i64, Option<usize>, usize)> {
    // alloc: once a report
    let mut cuts: HashMap<usize, Vec<(i64, i64, usize)>, Global> = HashMap::new_in(Global);
    // alloc: once a report
    let mut out = Vec::new();
    for link in path.links.iter().filter(|it| it.kind == LinkKind::Ran) {
        // alloc: a lane's cut, once a lane on the path
        let cut = cuts.entry(link.lane).or_insert_with(|| innermost(model, link.lane));
        let mut t = link.from_ns;
        let at = cut.partition_point(|it| it.1 <= link.from_ns);
        for &(from, to, span) in &cut[at..] {
            if from >= link.to_ns {
                break;
            }
            let (from, to) = (from.max(link.from_ns), to.min(link.to_ns));
            if from > t {
                out.push((t, from, None, link.lane));
            }
            if to > from {
                out.push((from, to, Some(span), link.lane));
            }
            t = t.max(to);
        }
        if t < link.to_ns {
            out.push((t, link.to_ns, None, link.lane));
        }
    }
    out
}

/// A lane's time cut by the innermost of its spans open, end to end where spans are: (from, to,
/// span), sorted, none overlapping.
pub fn innermost(model: &Model, lane: usize) -> Vec<(i64, i64, usize)> {
    let spans = &model.spans;
    // alloc: a lane's cut, once a lane on the path
    let mut out: Vec<(i64, i64, usize)> = Vec::new();
    // alloc: a lane's open spans, once a lane on the path
    let mut open: Vec<usize> = Vec::new();
    let mut t = i64::MIN;
    let emit = |out: &mut Vec<(i64, i64, usize)>, from: i64, to: i64, span: usize| {
        if to > from {
            out.push((from, to, span));
        }
    };
    for &i in &model.lanes[lane].spans {
        let start = spans[i].start_ns;
        while let Some(&top) = open.last()
            && spans[top].end_ns() <= start
        {
            emit(&mut out, t, spans[top].end_ns(), top);
            t = t.max(spans[top].end_ns());
            open.pop();
        }
        if let Some(&top) = open.last() {
            emit(&mut out, t, start, top);
        }
        t = t.max(start);
        open.push(i);
    }
    while let Some(top) = open.pop() {
        emit(&mut out, t, spans[top].end_ns(), top);
        t = t.max(spans[top].end_ns());
    }
    out
}

/// What the critical path ran, by query: its running time under each innermost span, summed by
/// the span's name and what it was about (a query span's `function`, a unit phase's unit), so
/// the queries the build's length stood on are named, not the jobs they ran in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OnPath {
    /// The span's name; `None` for running time under no span of its lane.
    pub name: Option<String>,
    pub subject: Option<String>,
    pub ns: i64,
    /// The path's stretches under it.
    pub stretches: u64,
}

/// [`OnPath`] of each innermost span on `path`, the most time first (by name and subject on a
/// tie), and the path's running time under none.
pub fn by_query(model: &Model, path: &Path) -> Vec<OnPath> {
    // alloc: once a report
    let mut sums: HashMap<(Option<&str>, Option<&str>), (i64, u64), Global> =
        HashMap::new_in(Global);
    for (from, to, span, _) in segments(model, path) {
        let key = span.map_or((None, None), |it| {
            (Some(model.spans[it].name.as_str()), model.spans[it].subject())
        });
        let sum = sums.entry(key).or_insert((0, 0));
        sum.0 += to - from;
        sum.1 += 1;
    }
    let mut out: Vec<OnPath> = sums
        .into_iter()
        // alloc: a row's labels, once a row a report
        .map(|((name, subject), (ns, stretches))| OnPath {
            name: name.map(str::to_owned),
            subject: subject.map(str::to_owned),
            ns,
            stretches,
        })
        // alloc: once a report
        .collect();
    out.sort_by(|a, b| {
        b.ns.cmp(&a.ns).then_with(|| (&a.name, &a.subject).cmp(&(&b.name, &b.subject)))
    });
    out
}
