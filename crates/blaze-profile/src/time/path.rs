//! A build's critical path: from the run that ended last, back in time through each lane's
//! intervals, crossing from a wait to the job that woke it, and from a job's start to the job
//! that gave it to the pool, when the record names that job. The links are contiguous, so their
//! durations add up to the path's length.

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
    /// From the spawner's end to the job's start: queued for a core after the job that gave it
    /// to the pool ended.
    Spawned,
    Unrecorded,
}

impl LinkKind {
    pub const ALL: [LinkKind; 6] =
        [LinkKind::Ran, LinkKind::Waited, LinkKind::Wake, LinkKind::Queued, LinkKind::Spawned, LinkKind::Unrecorded];

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
    /// on no fiber, or one whose spawner has no lane.
    LaneStart { lane: usize },
    /// Its bound of steps, [`fuel`] of the model.
    Fuel { steps: usize },
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
            // A job's start stands on its spawner's work up to it; from the spawner's end, if
            // it ended first, the job waited for a core.
            let spawner = model.lanes[lane].spawner.filter(|&it| {
                let it = &model.lanes[it];
                !it.intervals.is_empty() && it.begin_ns() < t
            });
            match spawner {
                Some(spawner) if crossed_at != Some(t) => {
                    let spawner_end = model.lanes[spawner].end_ns().max(start);
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
            links.push(Link { kind, lane, interval: i, from_ns: from, to_ns: to, wakes: wakes.take() });
        };
        let kind = match interval.state {
            State::Running { .. } => LinkKind::Ran,
            State::Queued { .. } => LinkKind::Queued,
            State::Unrecorded { .. } => LinkKind::Unrecorded,
            State::Parked { wait } | State::ParkedOrQueued { wait } => {
                let waker = model.waits[wait]
                    .waker
                    .lane()
                    .filter(|&it| model.lanes[it].begin_ns() < t && model.lanes[it].end_ns() > from);
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
