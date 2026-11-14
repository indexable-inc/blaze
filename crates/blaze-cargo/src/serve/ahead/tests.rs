//! The core against a model daemon, over any schedule of saves, watcher commits, requests,
//! cancels, lost connections, restarts and time. The core's own code decides ([`Ahead`]);
//! the model is what the daemon gives it and does for it: the watchers (a head lags its
//! files until a commit is delivered, and a request is let through once its head has caught
//! up with the files as they were when it was sent), the session and the jobs (a job is of
//! the root its head is at as it starts, takes [`BUILD`] ticks when it makes something and
//! [`WALK`] when everything is made, and a walk that made nothing leaves the answer the
//! no-op lane says again), and the clients.

use std::collections::BTreeSet;

use blaze_api::Targets;
use blaze_sim::{Config, Tape};

use super::*;

const HEADS: [&str; 2] = ["/a", "/b"];
/// Ticks a job that makes something takes, and one that finds everything made.
const BUILD: u32 = 3;
const WALK: u32 = 1;

fn root(head: usize, version: u32) -> ContentId {
    let mut bytes = vec![head as u8];
    bytes.extend_from_slice(&version.to_le_bytes());
    ContentId::of("blaze serve ahead test root", &bytes)
}

/// A version of a checkout's files that does not compile.
fn fails(version: u32) -> bool {
    version % 7 == 3
}

/// A build or a check of `head`, `variant` standing for everything else a request says
/// (its arguments, its environment).
fn request(head: usize, check: bool, variant: u8) -> Request {
    let view = ViewRef::Head(HEADS[head].to_owned());
    let targets = Targets {
        packages: Vec::new(),
        profile: String::new(),
        goal: None,
        target: None,
        features: vec![format!("variant-{variant}")],
        all_features: false,
        no_default_features: false,
        cargo: None,
    };
    if check { Request::Check { view, targets } } else { Request::Build { view, targets } }
}

fn head_of(request: &Request) -> Option<usize> {
    let head = built_ahead(request)?;
    HEADS.iter().position(|it| *it == head)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Who {
    Client(RequestRef),
    Ahead(u64),
}

#[derive(Clone, Debug)]
struct Job {
    who: Who,
    head: usize,
    request: Request,
    version: u32,
    left: u32,
    makes: bool,
    cancelled: bool,
}

/// A client's request the daemon has not answered.
#[derive(Clone, Debug)]
struct Open {
    from: RequestRef,
    request: Request,
    /// The version of its checkout's files as it was sent: its answer is of no older one.
    sent: u32,
    /// Where it is: waiting for its watcher, held by the core, or a job.
    waiting: bool,
    held: Option<Duration>,
}

#[derive(Clone, Copy, Debug)]
enum Op {
    /// A file of the checkout is written.
    Save(usize),
    /// A save of more files than the core builds ahead of: a branch switched.
    Switch(usize),
    /// The checkout's watcher commits what the disk holds.
    Deliver(usize),
    /// The watchers' flushes are told: each waiting request whose head has caught up goes on.
    Flush,
    /// A client sends a build or a check.
    Request { conn: u8, head: usize, check: bool, variant: u8 },
    /// A client sends a request that reads a checkout and builds nothing.
    Reads { conn: u8, head: usize },
    /// A client asks the daemon about itself.
    Status { conn: u8 },
    /// A client cancels its oldest request not answered.
    Cancel { conn: u8 },
    /// A client's connection ends.
    Gone { conn: u8 },
    /// Time passes, this many milliseconds, and every job does a tick of its work.
    Tick(u8),
    /// The daemon is killed and started again: its jobs, its kept answers and the core are
    /// gone, its clients' requests with them; the store keeps what was made.
    Restart,
}

struct Model {
    core: Ahead,
    now: Duration,
    /// The version of each checkout's files, and of its head (what its watcher committed).
    disk: [u32; 2],
    head: [u32; 2],
    /// Whether this daemon watches the checkout: from its first request of it.
    watched: [bool; 2],
    /// A move of the head is of more files than [`MASS`].
    mass: [bool; 2],
    jobs: Vec<Job>,
    open: Vec<Open>,
    /// What is made, in the store: a request's shape at a version of a checkout.
    made: BTreeSet<(usize, String, u32)>,
    /// Each checkout's kept answer: of this shape at this version.
    kept: [Option<(String, u32)>; 2],
    /// The last build or check a client's request of each checkout was let through as.
    last: [Option<Request>; 2],
    /// The version the last build ahead that ended was of.
    ahead_ended: Option<u32>,
    /// Builds ahead started of each checkout's version.
    starts: Vec<(usize, u32, u8)>,
    next: u64,
    /// Requests the lane answered, and requests answered in all.
    lane: u32,
    answered: u32,
    /// The requests whose answer the core owes itself: a held request a client cancelled,
    /// and the cancel.
    cancel_answers: Vec<RequestRef>,
    /// The oracle does not ask whose build ahead a request is held behind: what is left to
    /// catch a request held behind an older root is its answer alone.
    lenient: bool,
}

struct Seen<'a>(&'a Model);

impl World for Seen<'_> {
    fn idle(&self) -> bool {
        self.0.jobs.is_empty() && !self.0.open.iter().any(|it| it.waiting)
    }

    fn head(&self, head: &str) -> Option<ContentId> {
        let at = HEADS.iter().position(|it| *it == head)?;
        self.0.watched[at].then(|| root(at, self.0.head[at]))
    }

    fn known(&self, head: &str, asked: ContentId) -> bool {
        let Some(at) = HEADS.iter().position(|it| *it == head) else { return false };
        self.0.kept[at].as_ref().is_some_and(|(_, version)| root(at, *version) == asked)
    }
}

fn shape(request: &Request) -> String {
    format!("{request:?}")
}

impl Model {
    fn new() -> Model {
        Model {
            core: Ahead::new(true),
            now: Duration::from_millis(1),
            disk: [0, 0],
            head: [0, 0],
            watched: [false, false],
            mass: [false, false],
            jobs: Vec::new(),
            open: Vec::new(),
            made: BTreeSet::new(),
            kept: [None, None],
            last: [None, None],
            ahead_ended: None,
            starts: Vec::new(),
            next: 1,
            lane: 0,
            answered: 0,
            cancel_answers: Vec::new(),
            lenient: false,
        }
    }

    /// A client's request is answered, of `version` of its checkout's files.
    fn answer(&mut self, to: RequestRef, version: Option<u32>) -> Result<(), String> {
        let Some(at) = self.open.iter().position(|it| it.from == to) else { return Ok(()) };
        let open = self.open.remove(at);
        self.answered += 1;
        match version {
            Some(version) if version < open.sent => Err(format!(
                "request {to:?} was sent with its files at version {} and answered of version {version}: an answer from before the last save",
                open.sent
            )),
            _ => Ok(()),
        }
    }

    /// The session takes a build or a check: answered by the lane, or a job.
    fn start(&mut self, who: Who, request: Request) -> Result<(), String> {
        let Some(head) = head_of(&request) else {
            // What builds nothing is answered as it is taken.
            if let Who::Client(to) = who {
                self.answer(to, None)?;
            }
            return Ok(());
        };
        let version = self.head[head];
        let key = (shape(&request), version);
        if self.kept[head].as_ref() == Some(&key) && self.jobs.is_empty() {
            match who {
                Who::Client(to) => {
                    self.lane += 1;
                    self.answer(to, Some(version))?;
                }
                Who::Ahead(id) => {
                    self.ahead_ended = Some(version);
                    self.core.replied(id, &Response::Done { ok: true });
                }
            }
            return Ok(());
        }
        // A job that walks drops its checkout's kept answer.
        self.kept[head] = None;
        let makes = !self.made.contains(&(head, key.0, version));
        let left = if makes { BUILD } else { WALK };
        self.jobs.push(Job { who, head, request, version, left, makes, cancelled: false });
        Ok(())
    }

    /// Everything the core decided is carried out, and the core is asked again, until it
    /// has nothing more.
    fn pump(&mut self) -> Result<(), String> {
        for _ in 0..64 {
            let now = self.now;
            let mut core = std::mem::replace(&mut self.core, Ahead::new(false));
            core.tick(now, &Seen(self));
            self.core = core;
            if !self.core.pending() {
                return Ok(());
            }
            while let Some(it) = self.core.take() {
                self.carry_out(it)?;
            }
        }
        Err("the core never ran out of things to do".to_owned())
    }

    fn carry_out(&mut self, it: Do) -> Result<(), String> {
        match it {
            Do::Send { id, request: Request::Cancel { id: target } } => {
                if let Some(job) = self.jobs.iter_mut().find(|it| it.who == Who::Ahead(target)) {
                    (job.cancelled, job.left) = (true, 1);
                }
                self.core.replied(id, &Response::Done { ok: true });
                Ok(())
            }
            Do::Send { id, request } => {
                let Some(head) = head_of(&request) else {
                    return Err(format!("{request:?} was sent ahead: no build or check of a checkout"));
                };
                if self.last[head].as_ref() != Some(&request) {
                    return Err(format!(
                        "{request:?} was built ahead where the client's last request of {} was {:?}: a build ahead of another shape",
                        HEADS[head], self.last[head]
                    ));
                }
                if self.jobs.iter().any(|it| matches!(it.who, Who::Ahead(_))) {
                    return Err("two builds ahead are under way".to_owned());
                }
                if !self.jobs.is_empty() || self.open.iter().any(|it| it.waiting) {
                    return Err("a build ahead started beside a request".to_owned());
                }
                let version = self.head[head];
                match self.starts.iter_mut().find(|it| (it.0, it.1) == (head, version)) {
                    Some(count) => count.2 += 1,
                    None => self.starts.push((head, version, 1)),
                }
                self.start(Who::Ahead(id), request)
            }
            Do::Pass { from, request } => {
                let Some(open) = self.open.iter_mut().find(|it| it.from == from) else {
                    return Err(format!("{from:?} was handed on and is no request held"));
                };
                let Some(since) = open.held.take() else {
                    return Err(format!("{from:?} was handed on and was not held"));
                };
                let most = Duration::from_millis(u64::from(BUILD) * 8);
                if self.now.saturating_sub(since) > most {
                    return Err(format!("{from:?} was held {:?}, longer than its own build", self.now - since));
                }
                self.start(Who::Client(from), request)
            }
            Do::Answer(reply) => {
                // The core answers a client itself only for a cancel of a request it held:
                // the request not ok, the cancel ok, neither of any version. Its answer of
                // a request nobody cancelled is the build ahead's end handed on as the
                // request's own.
                match self.cancel_answers.iter().position(|it| *it == reply.to) {
                    Some(at) => {
                        self.cancel_answers.remove(at);
                        self.answer(reply.to, None)
                    }
                    None => self.answer(reply.to, self.ahead_ended),
                }
            }
        }
    }

    /// The invariants of a state at rest between two steps.
    fn check(&self) -> Result<(), String> {
        let aheads: Vec<&Job> =
            self.jobs.iter().filter(|it| matches!(it.who, Who::Ahead(_))).collect();
        if aheads.len() > 1 {
            return Err("two builds ahead are under way".to_owned());
        }
        let live = aheads.first().filter(|it| !it.cancelled);
        for open in &self.open {
            if open.held.is_some() {
                let own = live.is_some_and(|job| {
                    job.request == open.request && job.version == self.head[job.head]
                });
                if !own && !self.lenient {
                    return Err(format!(
                        "{:?} ({:?}) is held behind a build ahead that is not its own: {live:?}",
                        open.from, open.request
                    ));
                }
                continue;
            }
            // A request the daemon holds for no build ahead, and that a build ahead gives
            // way to: none goes on beside it, but one of the very request, while the
            // request still waits for its watcher.
            let Some(job) = live else { continue };
            if yielded_to(&open.request) && !(open.waiting && job.request == open.request) {
                return Err(format!(
                    "the build ahead {:?} goes on while {:?} ({:?}) waits: a request queued behind a build ahead",
                    job.who, open.from, open.request
                ));
            }
        }
        if let Some((head, version, count)) = self.starts.iter().find(|it| it.2 > STARTS) {
            return Err(format!("{count} builds ahead of version {version} of {}", HEADS[*head]));
        }
        Ok(())
    }

    fn step(&mut self, op: Op) -> Result<(), String> {
        match op {
            Op::Save(head) => self.disk[head] += 1,
            Op::Switch(head) => {
                self.disk[head] += 1;
                self.mass[head] = true;
            }
            Op::Deliver(head) => self.deliver(head),
            Op::Flush => {
                let ready: Vec<RequestRef> = self
                    .open
                    .iter()
                    .filter(|it| it.waiting)
                    .filter(|it| match head_of(&it.request).or(reads_of(&it.request)) {
                        Some(head) => self.head[head] >= it.sent && self.watched[head],
                        None => true,
                    })
                    .map(|it| it.from)
                    .collect();
                for from in ready {
                    self.release(from)?;
                }
            }
            Op::Request { conn, head, check, variant } => {
                self.send(conn, request(head, check, variant), Some(head))?
            }
            Op::Reads { conn, head } => {
                let view = ViewRef::Head(HEADS[head].to_owned());
                self.send(conn, Request::Open { view }, Some(head))?
            }
            Op::Status { conn } => self.send(conn, Request::Status, None)?,
            Op::Cancel { conn } => {
                let conn = Conn(u64::from(conn));
                let Some(target) = self.open.iter().find(|it| it.from.conn == conn).map(|it| it.from)
                else {
                    return Ok(());
                };
                let from = RequestRef { conn, id: self.id() };
                let cancel = Arrived::Request { from, request: Request::Cancel { id: target.id } };
                if self.core.read(&cancel) {
                    // The core held it: its answers come from the core, of no version.
                    self.cancel_answers.extend([target, from]);
                } else {
                    if let Some(job) = self.jobs.iter_mut().find(|it| it.who == Who::Client(target)) {
                        (job.cancelled, job.left) = (true, 1);
                    } else {
                        self.answer(target, None)?;
                    }
                }
            }
            Op::Gone { conn } => {
                let conn = Conn(u64::from(conn));
                self.core.read(&Arrived::Gone { conn });
                self.open.retain(|it| it.from.conn != conn);
                for job in &mut self.jobs {
                    if matches!(job.who, Who::Client(from) if from.conn == conn) {
                        (job.cancelled, job.left) = (true, 1);
                    }
                }
            }
            Op::Tick(ms) => {
                self.now += Duration::from_millis(u64::from(ms));
                for job in &mut self.jobs {
                    job.left = job.left.saturating_sub(1);
                }
                while let Some(at) = self.jobs.iter().position(|it| it.left == 0) {
                    let job = self.jobs.remove(at);
                    self.end(job)?;
                }
            }
            Op::Restart => {
                self.core = Ahead::new(true);
                self.jobs.clear();
                self.open.clear();
                self.starts.clear();
                self.cancel_answers.clear();
                (self.kept, self.last, self.watched) = ([None, None], [None, None], [false, false]);
            }
        }
        self.pump()?;
        self.check()
    }

    fn id(&mut self) -> u64 {
        self.next += 1;
        self.next
    }

    fn deliver(&mut self, head: usize) {
        if !self.watched[head] || self.head[head] == self.disk[head] {
            return;
        }
        self.head[head] = self.disk[head];
        let changed = if std::mem::take(&mut self.mass[head]) { MASS + 1 } else { 1 };
        self.core.moved(self.now, HEADS[head], root(head, self.head[head]), changed);
    }

    fn send(&mut self, conn: u8, request: Request, head: Option<usize>) -> Result<(), String> {
        let from = RequestRef { conn: Conn(u64::from(conn)), id: self.id() };
        let arrived = Arrived::Request { from, request: request.clone() };
        if self.core.read(&arrived) {
            return Err(format!("the core took {request:?}, which is no cancel"));
        }
        let sent = head.map_or(0, |head| self.disk[head]);
        // Its first request of a checkout starts the checkout's watcher, which scans it.
        if let Some(head) = head
            && !self.watched[head]
        {
            (self.watched[head], self.head[head], self.mass[head]) = (true, self.disk[head], false);
        }
        self.open.push(Open { from, request, sent, waiting: true, held: None });
        if head.is_none() {
            self.release(from)?;
        }
        Ok(())
    }

    /// A request's watchers have caught up: the core holds it, or the session takes it.
    fn release(&mut self, from: RequestRef) -> Result<(), String> {
        let Some(open) = self.open.iter_mut().find(|it| it.from == from) else { return Ok(()) };
        open.waiting = false;
        let request = open.request.clone();
        let head = head_of(&request);
        let at = head.map(|head| root(head, self.head[head]));
        if let Some(head) = head {
            self.last[head] = Some(request.clone());
            // A client built this root: the core's bound on its builds ahead starts again.
            self.starts.retain(|it| it.0 != head);
        }
        if self.core.released(self.now, from, &request, at) {
            let now = self.now;
            if let Some(open) = self.open.iter_mut().find(|it| it.from == from) {
                open.held = Some(now);
            }
            return Ok(());
        }
        self.start(Who::Client(from), request)
    }

    fn end(&mut self, job: Job) -> Result<(), String> {
        let ok = !job.cancelled && !fails(job.version);
        if ok && job.makes {
            self.made.insert((job.head, shape(&job.request), job.version));
        } else if ok && self.jobs.iter().all(|it| it.head != job.head) {
            self.kept[job.head] = Some((shape(&job.request), job.version));
        }
        match job.who {
            Who::Client(to) => self.answer(to, (!job.cancelled).then_some(job.version)),
            Who::Ahead(id) => {
                self.ahead_ended = Some(job.version);
                self.core.replied(id, &Response::Done { ok });
                Ok(())
            }
        }
    }

    /// The schedule's end: everything is delivered and time passes until nothing moves.
    /// Every request is answered by then: none is left held or waiting.
    fn settle(&mut self) -> Result<(), String> {
        for _ in 0..200 {
            for op in [Op::Deliver(0), Op::Deliver(1), Op::Flush, Op::Tick(6)] {
                self.step(op)?;
            }
            let owed = self.core.deadline(self.now).is_some() || self.core.running();
            if self.jobs.is_empty() && self.open.is_empty() && !owed {
                return Ok(());
            }
        }
        Err(format!("after 200 rounds left alone: jobs {:?}, requests {:?}", self.jobs, self.open))
    }
}

/// The checkout a request that builds nothing reads.
fn reads_of(request: &Request) -> Option<usize> {
    match request {
        Request::Open { view: ViewRef::Head(head) } => HEADS.iter().position(|it| it == head),
        _ => None,
    }
}

/// Runs `ops` and then leaves the daemon alone, `plant` planted: what the oracle caught, or
/// the model as it ended.
fn runs(ops: &[Op], plant: Option<Plant>) -> Result<Model, String> {
    runs_as(ops, plant, false, 0)
}

/// [`runs`], the oracle [`Model::lenient`] or not, and the bug planted from step `from` on.
fn runs_as(ops: &[Op], plant: Option<Plant>, lenient: bool, from: usize) -> Result<Model, String> {
    PLANTED.set(None);
    let mut model = Model::new();
    model.lenient = lenient;
    let mut ran = Ok(());
    for (at, op) in ops.iter().enumerate() {
        if at == from {
            PLANTED.set(plant);
        }
        ran = model.step(*op).map_err(|why| format!("step {at} ({op:?}): {why}"));
        if ran.is_err() {
            break;
        }
    }
    let ran = ran.and_then(|()| model.settle().map_err(|why| format!("settling: {why}")));
    PLANTED.set(None);
    ran.map(|()| model)
}

fn op(tape: &Tape) -> Op {
    let head = usize::from(tape.bool("head"));
    let conn = tape.int_in(0..=2u8, "conn");
    match tape.int_in(0..=23u8, "op") {
        0..=3 => Op::Save(head),
        4 => Op::Switch(head),
        5..=7 => Op::Deliver(head),
        8..=10 => Op::Flush,
        11..=14 => Op::Request {
            conn,
            head,
            check: tape.int_in(0..=4u8, "check") == 0,
            variant: u8::from(tape.int_in(0..=5u8, "variant") == 0),
        },
        15 => Op::Reads { conn, head },
        16 => Op::Status { conn },
        17 => Op::Cancel { conn },
        18 => Op::Gone { conn },
        19 => Op::Restart,
        _ => Op::Tick(tape.int_in(1..=8u8, "ticks")),
    }
}

/// The invariant: over any schedule of saves, branch switches, watcher commits, flushes,
/// builds and checks of two checkouts from three connections, other requests, cancels, lost
/// connections, restarts and time: a request is answered of its checkout's files as they
/// were when it was sent or later, never earlier; at most one build ahead is under way, and
/// none starts beside a request's job or a request waiting for its watcher; a request is held
/// only behind the build ahead of its own request and root, for no longer than that build,
/// and a build ahead goes on beside no other request that reads a checkout or builds; what
/// is built ahead is the checkout's last build or check, as its client sent it; a root is
/// built ahead at most [`STARTS`] times; and left alone, the daemon answers every request.
/// `BLAZE_SIM_SEEDS=<n>` runs it longer.
#[test]
fn a_build_ahead_changes_no_answer_and_delays_no_request() {
    blaze_sim::check(Config::cases(512), |tape| {
        let ops = tape.vec(1..=60, "ops", op);
        if let Err(why) = runs(&ops, None) {
            panic!("{ops:?}: {why}");
        }
    });
}

const BUILD_A: Op = Op::Request { conn: 0, head: 0, check: false, variant: 0 };
const TICK: Op = Op::Tick(6);

/// `/a` built by a client and its answer kept, then `ops`.
fn built_then(ops: &[Op]) -> Vec<Op> {
    let built = [BUILD_A, Op::Flush, TICK, TICK, TICK, TICK, TICK, TICK];
    built.into_iter().chain(ops.iter().copied()).collect()
}

/// The directed case of a control: passes with nothing planted, and with `plant` the oracle
/// says what it caught.
fn caught(ops: &[Op], plant: Plant) -> String {
    if let Err(why) = runs(ops, None) {
        panic!("the core as it is must pass {ops:?}: {why}");
    }
    match runs(ops, Some(plant)) {
        Ok(_) => panic!("the oracle missed {plant:?} over {ops:?}"),
        Err(why) => why,
    }
}

/// The core is of use: a save left alone is built ahead, twice (the build that makes it, the
/// walk whose answer is kept), and the request after it is answered by the lane, with no
/// job.
#[test]
fn a_save_left_alone_is_answered_by_the_lane() {
    let ops = built_then(&[
        Op::Save(0),
        Op::Deliver(0),
        TICK,
        TICK,
        TICK,
        TICK,
        TICK,
        TICK,
        TICK,
        BUILD_A,
        Op::Flush,
    ]);
    let model = runs(&ops, None).unwrap();
    assert_eq!(model.core.counts.started, 3, "{:?}", model.core.counts);
    assert_eq!(model.core.counts.ended, 3);
    assert_eq!(model.lane, 1, "the request after the save is the lane's");
    assert_eq!(model.answered, 2);
}

/// A request for what is being built ahead waits for that build and then walks; any other
/// cancels it and is taken at once.
#[test]
fn a_request_waits_for_its_own_build_and_cancels_any_other() {
    let save = [Op::Save(0), Op::Deliver(0), TICK];
    let own: Vec<Op> = save.into_iter().chain([BUILD_A, Op::Flush]).collect();
    let model = runs(&built_then(&own), None).unwrap();
    assert_eq!((model.core.counts.held, model.core.counts.cancelled), (1, 0));
    let other = Op::Request { conn: 0, head: 0, check: true, variant: 0 };
    let other: Vec<Op> = save.into_iter().chain([other, Op::Flush]).collect();
    let model = runs(&built_then(&other), None).unwrap();
    assert_eq!((model.core.counts.held, model.core.counts.cancelled), (0, 1));
    let elsewhere = Op::Request { conn: 1, head: 1, check: false, variant: 0 };
    let elsewhere: Vec<Op> = save.into_iter().chain([elsewhere, Op::Flush]).collect();
    let model = runs(&built_then(&elsewhere), None).unwrap();
    assert_eq!((model.core.counts.held, model.core.counts.cancelled), (0, 1));
}

/// A branch switch is not built ahead, and a save after it is not either, until a client
/// has built the checkout; a save that does not compile is built ahead once.
#[test]
fn a_branch_switch_and_a_failure_are_not_built_again() {
    let ops = built_then(&[Op::Switch(0), Op::Deliver(0), TICK, TICK, Op::Save(0), Op::Deliver(0), TICK, TICK]);
    let model = runs(&ops, None).unwrap();
    assert_eq!(model.core.counts.started, 1, "the walk after the client's build alone");
    assert_eq!(model.core.off(HEADS[0]), Some(Off::Mass));
    // Three saves in one commit: version 3, which does not compile.
    let ops = built_then(&[Op::Save(0), Op::Save(0), Op::Save(0), Op::Deliver(0), TICK]);
    let model = runs(&ops, None).unwrap();
    assert_eq!(model.core.counts.started, 2);
    assert_eq!(model.core.off(HEADS[0]), Some(Off::Failed));
}

#[test]
fn the_oracle_catches_an_answer_from_before_the_last_save() {
    // A build ahead of the first save is under way as the second is made; the request sent
    // after the second is held behind it and answered as it ends.
    let ops = built_then(&[
        Op::Save(0),
        Op::Deliver(0),
        TICK,
        Op::Save(0),
        BUILD_A,
        Op::Deliver(0),
        Op::Flush,
    ]);
    let why = caught(&ops, Plant::OldAnswer);
    assert!(why.contains("not its own"), "{why}");
    // And by its answer alone, the hold not looked at.
    let why = runs_as(&ops, Some(Plant::OldAnswer), true, 0).err().expect("the answer is old");
    assert!(why.contains("an answer from before the last save"), "{why}");
}

#[test]
fn the_oracle_catches_two_builds_ahead() {
    let build_b = Op::Request { conn: 1, head: 1, check: false, variant: 0 };
    let mut ops = built_then(&[build_b, Op::Flush, TICK, TICK, TICK, TICK, TICK, TICK]);
    // Planted once both checkouts are built: before, the same bug starts a build ahead
    // beside the client's build, which the control below is of.
    let from = ops.len();
    ops.extend([Op::Save(0), Op::Deliver(0), TICK, Op::Save(1), Op::Deliver(1), TICK]);
    assert!(runs(&ops, None).is_ok());
    let why = runs_as(&ops, Some(Plant::TwoJobs), false, from).err().expect("two at once");
    assert!(why.contains("two builds ahead"), "{why}");
}

#[test]
fn the_oracle_catches_a_build_ahead_started_beside_a_request() {
    // A save, and a request of another shape read before the save's quiet is over: its job
    // is under way as the build ahead is due.
    let check = Op::Request { conn: 0, head: 0, check: true, variant: 0 };
    let ops = built_then(&[Op::Save(0), Op::Deliver(0), check, Op::Flush, TICK]);
    let why = caught(&ops, Plant::TwoJobs);
    assert!(why.contains("beside a request"), "{why}");
}

#[test]
fn the_oracle_catches_a_request_queued_behind_a_build_ahead() {
    // Of the same checkout, another request: held where it must be taken at once.
    let check = Op::Request { conn: 0, head: 0, check: true, variant: 0 };
    let ops = built_then(&[Op::Save(0), Op::Deliver(0), TICK, check, Op::Flush]);
    let why = caught(&ops, Plant::Queues);
    assert!(why.contains("queued behind a build ahead"), "{why}");
    // Of another checkout: the build ahead goes on beside it.
    let build_b = Op::Request { conn: 1, head: 1, check: false, variant: 0 };
    let ops = built_then(&[Op::Save(0), Op::Deliver(0), TICK, build_b]);
    let why = caught(&ops, Plant::Queues);
    assert!(why.contains("queued behind a build ahead"), "{why}");
}

#[test]
fn the_oracle_catches_a_build_ahead_of_another_shape() {
    let ops = built_then(&[Op::Save(0), Op::Deliver(0), TICK]);
    let why = caught(&ops, Plant::OtherShape);
    assert!(why.contains("another shape"), "{why}");
}
