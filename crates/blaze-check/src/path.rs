//! An execution's path: its branches in order, which the next execution replays up to the last
//! one with an alternative left. A port of loom's `rt/path.rs` (loom 0.7.2), with loads branching
//! on a candidate's position (the candidates are a function of the path before them) and with a
//! replay and a random mode over the same branches.

use crate::{
    clock::{MAX_THREADS, Tid},
    rng::Rng,
};

/// What a thread is at a schedule branch (loom's `path::Thread`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Mark {
    /// It cannot run: blocked or ended.
    Disabled,
    /// It could run; nothing asks to explore it here.
    Skip,
    /// It could run but yielded.
    Yield,
    /// To be explored here.
    Pending,
    /// The one this execution runs.
    Active,
    /// Explored here already.
    Visited,
}

impl Mark {
    fn explore(&mut self) {
        if *self == Mark::Skip {
            *self = Mark::Pending;
        }
    }

    fn enabled(self) -> bool {
        self != Mark::Disabled
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Schedule {
    /// Preemptions before this branch.
    preemptions: u8,
    /// The thread that runs here without a preemption, when one does.
    initial: Option<u8>,
    threads: [Mark; MAX_THREADS],
    /// The schedule branch before this one.
    prev: Option<u32>,
    /// The first schedule branch of the run this one is in: the latest at or before it, along
    /// `prev`, that is the first or runs another thread than the one before it. Bounded DPOR
    /// backtracks there; kept per branch so that is one load, not a walk back.
    start: u32,
}

/// [`Schedule::start`] of a branch at `at` running `active`, after the schedule `prev`.
fn run_start(branches: &[Branch], at: u32, active: Option<usize>, prev: Option<u32>) -> u32 {
    let Some(prev) = prev else { return at };
    match &branches[prev as usize] {
        Branch::Schedule(before) if before.active() == active => before.start,
        _ => at,
    }
}

impl Schedule {
    fn active(&self) -> Option<usize> {
        self.threads.iter().position(|&it| it == Mark::Active)
    }

    /// Preemptions up to and including this branch.
    fn preemptions(&self) -> u8 {
        match self.initial {
            Some(initial) if Some(initial as usize) != self.active() => self.preemptions + 1,
            _ => self.preemptions,
        }
    }

    fn backtrack(&mut self, thread: Tid, bound: Option<u8>) {
        if bound.is_some_and(|bound| self.preemptions >= bound) {
            return;
        }
        if thread >= MAX_THREADS {
            return;
        }
        if self.threads[thread].enabled() {
            self.threads[thread].explore();
        } else {
            for mark in &mut self.threads {
                mark.explore();
            }
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) enum Branch {
    Schedule(Schedule),
    /// The store a load reads: `pos` of `len` candidates, newest first. `len` 0: not yet known
    /// (a replayed choice).
    Load { pos: u16, len: u16 },
    /// Whether a timed wait times out.
    Spurious(bool),
}

/// How the next branch is chosen.
pub(crate) enum Mode {
    /// Every alternative, depth first, as DPOR asks.
    Explore,
    /// The branches given, then stop.
    Replay,
    /// Shuttle's PCT: threads by priority, `depth - 1` priority drops at random steps.
    Random(Pct),
}

pub(crate) struct Pct {
    rng: Rng,
    /// Each thread's priority; higher runs first.
    priority: [u32; MAX_THREADS],
    /// Schedule branches at which the running thread's priority drops below every other's.
    drops: Vec<u32>,
    steps: u32,
    lowest: u32,
}

impl Pct {
    /// A run's priorities and drops from `seed`, its drops kept in `drops` (a buffer reused
    /// across runs).
    pub(crate) fn new(seed: u64, depth: usize, steps: u32, mut drops: Vec<u32>) -> Pct {
        let mut rng = Rng::new(seed);
        let mut priority = [0; MAX_THREADS];
        for (at, it) in priority.iter_mut().enumerate() {
            *it = MAX_THREADS as u32 + at as u32;
        }
        rng.shuffle(&mut priority);
        drops.clear();
        drops.extend((1..depth.max(1)).map(|_| rng.below(u64::from(steps.max(1))) as u32));
        Pct { rng, priority, drops, steps: 0, lowest: MAX_THREADS as u32 }
    }
}

/// Why a path cannot go on.
pub(crate) enum Stuck {
    /// More branches than the bound: a loop that waits without yielding, or a model too long.
    TooLong(usize),
    /// The execution differs from the one before it at the same point.
    Nondeterministic(usize, &'static str),
    /// A replayed choice that cannot be taken.
    BadReplay(usize, String),
}

pub(crate) struct Path {
    pub(crate) mode: Mode,
    bound: Option<u8>,
    max: usize,
    pos: usize,
    branches: Vec<Branch>,
}

impl Path {
    pub(crate) fn new(mode: Mode, bound: Option<u8>, max: usize) -> Path {
        Path { mode, bound, max, pos: 0, branches: Vec::with_capacity(max.min(1 << 16)) }
    }

    /// A path that replays `schedule` (as [`Path::trail`] wrote it).
    pub(crate) fn replay(schedule: &str, max: usize) -> Result<Path, String> {
        // alloc: a replayed path, once per model
        let mut branches = Vec::new();
        let mut chars = schedule.chars().peekable();
        let mut prev = None;
        while let Some(char) = chars.next() {
            let branch = match char {
                '0'..='7' | '.' => {
                    let mut threads = [Mark::Skip; MAX_THREADS];
                    if char != '.' {
                        threads[char as usize - '0' as usize] = Mark::Active;
                    }
                    let at = branches.len() as u32;
                    let active = threads.iter().position(|&it| it == Mark::Active);
                    let start = run_start(&branches, at, active, prev);
                    let schedule = Schedule { preemptions: 0, initial: None, threads, prev, start };
                    prev = Some(at);
                    Branch::Schedule(schedule)
                }
                'a'..='z' => Branch::Load { pos: char as u16 - 'a' as u16, len: 0 },
                '{' => {
                    // alloc: parsing a replayed path, once per model
                    let mut digits = String::new();
                    for char in chars.by_ref() {
                        if char == '}' {
                            break;
                        }
                        digits.push(char);
                    }
                    let pos = digits
                        .parse()
                        // alloc: a finding's or an error's text, made once, when the model stops
                        .map_err(|_| format!("`{{{digits}}}` in a schedule is not a load choice"))?;
                    Branch::Load { pos, len: 0 }
                }
                '+' => Branch::Spurious(true),
                '-' => Branch::Spurious(false),
                // alloc: a finding's or an error's text, made once, when the model stops
                other => return Err(format!("`{other}` is not a branch of a schedule")),
            };
            branches.push(branch);
        }
        Ok(Path { mode: Mode::Replay, bound: None, max: max.max(branches.len()), pos: 0, branches })
    }

    /// Readies the path for the random run `seed`, keeping its buffers: the same run as a new
    /// `Path::new(Mode::Random(Pct::new(seed, depth, steps, ..)), None, max)`.
    pub(crate) fn random(&mut self, seed: u64, depth: usize, steps: u32) {
        let drops = match &mut self.mode {
            Mode::Random(pct) => std::mem::take(&mut pct.drops),
            // alloc: the first random run's priority drops; later runs reuse them
            _ => Vec::new(),
        };
        self.mode = Mode::Random(Pct::new(seed, depth, steps, drops));
        self.bound = None;
        self.pos = 0;
        self.branches.clear();
    }

    pub(crate) fn pos(&self) -> usize {
        self.pos
    }

    pub(crate) fn len(&self) -> usize {
        self.branches.len()
    }

    fn traversed(&self) -> bool {
        self.pos == self.branches.len()
    }

    fn push(&mut self, branch: Branch) -> Result<(), Stuck> {
        if self.branches.len() >= self.max {
            return Err(Stuck::TooLong(self.max));
        }
        self.branches.push(branch);
        Ok(())
    }

    fn last_schedule(&self) -> Option<u32> {
        self.branches.iter().rposition(|it| matches!(it, Branch::Schedule(_))).map(|at| at as u32)
    }

    fn schedule(&self, at: u32) -> &Schedule {
        match &self.branches[at as usize] {
            Branch::Schedule(it) => it,
            _ => unreachable!("a schedule's `prev` names a schedule"),
        }
    }

    fn schedule_mut(&mut self, at: u32) -> &mut Schedule {
        match &mut self.branches[at as usize] {
            Branch::Schedule(it) => it,
            _ => unreachable!("a schedule's `prev` names a schedule"),
        }
    }

    /// The thread to run next, from each thread's mark: the current one `Active` if it can
    /// run, else the first that can. `None`: no thread can run.
    pub(crate) fn branch_thread(
        &mut self,
        marks: [Mark; MAX_THREADS],
    ) -> Result<Option<Tid>, Stuck> {
        if self.traversed() {
            let prev = self.last_schedule();
            let mut threads = marks;
            let mut active = threads.iter().position(|&it| it == Mark::Active);
            if active.is_none()
                && let Some(at) = threads.iter().position(|&it| it == Mark::Yield)
            {
                threads[at] = Mark::Active;
                active = Some(at);
            }
            if let Mode::Random(pct) = &mut self.mode {
                let chosen = pct_choose(pct, &threads);
                if let Some(at) = active {
                    threads[at] = Mark::Skip;
                }
                if let Some(chosen) = chosen {
                    threads[chosen] = Mark::Active;
                }
                active = chosen;
            }
            let mut initial = active.map(|it| it as u8);
            if let Some(prev) = prev
                && initial.map(usize::from) != self.schedule(prev).active()
            {
                initial = None;
            }
            let preemptions = prev.map_or(0, |prev| self.schedule(prev).preemptions());
            let start = run_start(&self.branches, self.branches.len() as u32, active, prev);
            self.push(Branch::Schedule(Schedule { preemptions, initial, threads, prev, start }))?;
        }
        let at = self.pos;
        self.pos += 1;
        let replaying = matches!(self.mode, Mode::Replay);
        let Branch::Schedule(schedule) = &mut self.branches[at] else {
            return Err(Stuck::Nondeterministic(at, "a thread switch where the path has a load"));
        };
        let active = schedule.active();
        match active {
            Some(thread) if !marks[thread].enabled() => {
                if replaying {
                    // alloc: a finding's or an error's text, made once, when the model stops
                    return Err(Stuck::BadReplay(at, format!("thread {thread} cannot run here")));
                }
                Err(Stuck::Nondeterministic(at, "the thread the path runs here cannot run"))
            }
            None if marks.iter().any(|it| it.enabled()) => {
                Err(Stuck::Nondeterministic(at, "no thread runs where one could"))
            }
            _ => {
                if replaying {
                    // A replayed branch knows only its choice: the marks are this run's, but
                    // only the replayed thread may stay `Active`.
                    for (mark, &now) in schedule.threads.iter_mut().zip(&marks) {
                        if *mark != Mark::Active {
                            *mark = if now == Mark::Active { Mark::Skip } else { now };
                        }
                    }
                }
                Ok(active)
            }
        }
    }

    /// Which of `len` candidate stores a load reads.
    pub(crate) fn branch_load(&mut self, len: usize) -> Result<usize, Stuck> {
        if len <= 1 {
            return Ok(0);
        }
        let len = u16::try_from(len).expect("store histories are bounded below u16::MAX");
        if self.traversed() {
            let pos = match &mut self.mode {
                Mode::Random(pct) => pct.rng.below(u64::from(len)) as u16,
                _ => 0,
            };
            self.push(Branch::Load { pos, len })?;
        }
        let at = self.pos;
        self.pos += 1;
        match &mut self.branches[at] {
            Branch::Load { pos, len: known } if *known == 0 => {
                if *pos >= len {
                    // alloc: a finding's or an error's text, made once, when the model stops
                    let why = format!("load choice {pos} of {len} stores");
                    return Err(Stuck::BadReplay(at, why));
                }
                *known = len;
                Ok(usize::from(*pos))
            }
            Branch::Load { pos, len: known } if *known == len => Ok(usize::from(*pos)),
            Branch::Load { .. } => Err(Stuck::Nondeterministic(at, "a load with other stores")),
            _ => Err(Stuck::Nondeterministic(at, "a load where the path switches threads")),
        }
    }

    /// Whether a timed wait times out here.
    pub(crate) fn branch_spurious(&mut self) -> Result<bool, Stuck> {
        if self.traversed() {
            let spur = match &mut self.mode {
                Mode::Random(pct) => pct.rng.one_in(2),
                _ => false,
            };
            self.push(Branch::Spurious(spur))?;
        }
        let at = self.pos;
        self.pos += 1;
        match self.branches[at] {
            Branch::Spurious(spur) => Ok(spur),
            _ => Err(Stuck::Nondeterministic(at, "a timed wait where the path has another branch")),
        }
    }

    /// Asks to explore `thread` at the schedule branch `point` (loom's `Path::backtrack`), and,
    /// with a preemption bound, at the last branch before it where the running thread changed:
    /// bounded DPOR must add that point to cover what the bound cuts.
    pub(crate) fn backtrack(&mut self, point: usize, thread: Tid) {
        if !matches!(self.mode, Mode::Explore) {
            return;
        }
        let bound = self.bound;
        let Some(point) =
            self.branches[..=point].iter().rposition(|it| matches!(it, Branch::Schedule(_)))
        else {
            return;
        };
        let point = point as u32;
        self.schedule_mut(point).backtrack(thread, bound);
        if bound.is_none() {
            return;
        }
        // Loom's walk back from `point`'s previous schedule to the first that is the first or
        // runs another thread than the one before it: that branch's `start`.
        let Some(prev) = self.schedule(point).prev else { return };
        let start = self.schedule(prev).start;
        self.schedule_mut(start).backtrack(thread, bound);
    }

    /// Readies the next execution: the deepest branch with an alternative left takes it and
    /// the branches after it are dropped. `false`: every execution has been explored.
    pub(crate) fn step(&mut self) -> bool {
        self.pos = 0;
        match &mut self.mode {
            Mode::Explore => {}
            Mode::Replay => return false,
            Mode::Random(_) => {
                self.branches.clear();
                return true;
            }
        }
        while let Some(last) = self.branches.last_mut() {
            match last {
                Branch::Schedule(schedule) => {
                    if let Some(active) = schedule.active() {
                        schedule.threads[active] = Mark::Visited;
                    }
                    if let Some(next) = schedule.threads.iter().position(|&it| it == Mark::Pending)
                    {
                        schedule.threads[next] = Mark::Active;
                        let prev = schedule.prev;
                        let at = self.branches.len() as u32 - 1;
                        let start = run_start(&self.branches, at, Some(next), prev);
                        self.schedule_mut(at).start = start;
                        return true;
                    }
                }
                Branch::Load { pos, len } => {
                    *pos += 1;
                    if *pos < *len {
                        return true;
                    }
                }
                Branch::Spurious(spur) => {
                    if !*spur {
                        *spur = true;
                        return true;
                    }
                }
            }
            self.branches.pop();
        }
        false
    }

    /// The choices of this execution so far, as [`Path::replay`] reads them: a digit per
    /// thread switch, a letter per load (`a` the newest store; `{n}` past `z`), `+`/`-` per timed
    /// wait.
    pub(crate) fn trail(&self) -> String {
        let mut out = String::with_capacity(self.pos);
        for branch in &self.branches[..self.pos] {
            match branch {
                Branch::Schedule(schedule) => match schedule.active() {
                    Some(at) => out.push((b'0' + at as u8) as char),
                    None => out.push('.'),
                },
                Branch::Load { pos, .. } if *pos < 26 => out.push((b'a' + *pos as u8) as char),
                // alloc: a finding's replayable trail, made once, when the model stops
                Branch::Load { pos, .. } => out.push_str(&format!("{{{pos}}}")),
                Branch::Spurious(spur) => out.push(if *spur { '+' } else { '-' }),
            }
        }
        out
    }
}

/// The enabled thread of highest priority, after this step's priority drop. A thread that
/// yielded runs only when no other can, so a spin loop lets the others on.
fn pct_choose(pct: &mut Pct, threads: &[Mark; MAX_THREADS]) -> Option<Tid> {
    pct.steps += 1;
    let best = |pct: &Pct| {
        let pick = |yielded: bool| {
            (0..MAX_THREADS)
                .filter(|&at| threads[at].enabled() && (yielded || threads[at] != Mark::Yield))
                .max_by_key(|&at| pct.priority[at])
        };
        pick(false).or_else(|| pick(true))
    };
    let chosen = best(pct)?;
    if pct.drops.contains(&pct.steps) {
        pct.lowest = pct.lowest.saturating_sub(1);
        pct.priority[chosen] = pct.lowest;
        return best(pct);
    }
    Some(chosen)
}

#[cfg(test)]
mod tests {
    use super::{Branch, Mark, Mode, Path};
    use crate::{clock::MAX_THREADS, rng::Rng};

    impl Path {
        /// Loom's walk from schedule `at` back to the first schedule of its run: what
        /// `Schedule::start` keeps.
        fn walk_start(&self, at: u32) -> u32 {
            let mut curr = at;
            loop {
                match self.schedule(curr).prev {
                    Some(prev) if self.schedule(curr).active() == self.schedule(prev).active() => curr = prev,
                    _ => return curr,
                }
            }
        }

        fn check_starts(&self) {
            for (at, branch) in self.branches.iter().enumerate() {
                if let Branch::Schedule(schedule) = branch {
                    assert_eq!(schedule.start, self.walk_start(at as u32), "the start of schedule {at}");
                }
            }
        }
    }

    /// The threads at branch `pos` of every execution: a function of the position, as a
    /// deterministic model's are of the path before it.
    fn marks(seed: u64, pos: usize) -> [Mark; MAX_THREADS] {
        let mut rng = Rng::new(blaze_rng::mix(seed ^ (pos as u64) << 20));
        let mut marks = [Mark::Disabled; MAX_THREADS];
        for mark in &mut marks[..3] {
            *mark = match rng.below(5) {
                0 => Mark::Disabled,
                1 => Mark::Yield,
                _ => Mark::Skip,
            };
        }
        let first = rng.below(3) as usize;
        if let Some(at) = (0..3).map(|it| (first + it) % 3).find(|&it| marks[it] == Mark::Skip) {
            marks[at] = Mark::Active;
        }
        marks
    }

    /// Every schedule's `start` is loom's walk, through branches, loads, backtracks and steps.
    #[test]
    fn a_schedule_keeps_the_start_of_its_run() {
        for seed in 0..64 {
            let mut path = Path::new(Mode::Explore, Some(2), 1000);
            let mut rng = Rng::new(seed);
            for _ in 0..200 {
                for _ in 0..24 {
                    let pos = path.pos();
                    if path.branch_thread(marks(seed, pos)).is_err() {
                        panic!("the path does not fit its marks at {pos}");
                    }
                    if blaze_rng::mix(seed ^ pos as u64) % 3 == 0 {
                        assert!(path.branch_load(2).is_ok());
                    }
                    let point = rng.index(path.pos());
                    if matches!(path.branches[point], Branch::Schedule(_)) {
                        path.backtrack(point, rng.index(3));
                    }
                    path.check_starts();
                }
                if !path.step() {
                    break;
                }
                path.check_starts();
            }
        }
    }
}
