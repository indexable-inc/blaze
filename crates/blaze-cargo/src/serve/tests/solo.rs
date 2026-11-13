//! A client alone against a daemon started fresh (/tmp/plan/10-no-daemon-run.md, task B, the
//! last test; its soundness table, section 2): the real daemon on the simulation's driver writes
//! request slots (`serve::job::slot`), and a `Solo` (blaze-certify's `solo`) is driven over the
//! simulation's files as blaze-thin's `solo.rs` drives it over a real disk: the slot read whole,
//! each `Look::Stamps` an `lstat` of each path, each `Look::Head` a file's first bytes.
//!
//! The schedule, a function of its seed: requests of two keys (one with a feature, so two
//! slots name one program), from one client or two at once; edits of `main.rs` of the same size
//! and of another, a file added and removed, a variable of the request's set added and taken
//! away, all with a daemon running, during a build, or while none ran; graceful stops, crashes
//! that tear what was not synced (`Sim::crash`), a collection (the program's output and the
//! slots deleted, as a collector of build directories and a cleaner would), and a commit to the
//! checkout's head while the disk changes under it (what the watcher may see as a conflict).
//!
//! The launch host is played here ([`held_code`], `Fake::held_code`): the code it holds for
//! [`APP_AT`] is of the last build that ran a job, and its start file (`slot::start_file`) is
//! removed the turn a build gives the program another record (H6) and placed again once that
//! build has settled (H5).
//!
//! The judge of a hit is a daemon started fresh on the same files: whatever runs is stopped as
//! `restart` stops it (none running: one is started), the client looks again (the stop may have
//! moved what it looked at: then the hit is counted as moved, not judged), and the request is
//! asked of that daemon, which must answer it with no job (no plan load, [`PLANS`]) and say what
//! the hit says. The run goes on with that daemon.
//!
//! Not modelled, said: the slot's two premises are real files of this machine (the test's own
//! executable, this crate's `Cargo.toml` in place of the library's `DIGEST`), stamped by the
//! daemon with `local::stamp_now` and by the client the same way; so no premise is replaced in
//! the schedule (they are stamped by the system's clock, as the real disk's are). The daemon's
//! racy questions ask its driver's wall clock ([`blaze_core::Driver::wall_ns`]), which in the
//! simulation is the clock its disk stamps change times by, so a name looked at just after its
//! edit is racy, is saved unknown, and the writer reads it ([`the_oracle_catches_a_slot_written_before_its_look`]).
//! The daemon's own collector is not run (the simulation's daemon runs none).

use std::path::Path;

use blaze_certify::{
    certificate::Answer,
    slot::{START_MAGIC, key_of, start_file},
    solo::{Look, MOST_ROUNDS, Seen, Solo, Step, Verdict},
};
use blaze_core::Stamp;

use super::*;
use crate::serve::{
    daemon::Premises,
    job::slot::{PLANT as SLOT_PLANT, Plant as SlotPlant},
    work::{CodeHolder, HeldCode},
};

/// How many seeds the invariant runs over, and each control at most.
const SEEDS: u64 = 256;
/// The acts of a seed's schedule after its first ([`Act::Twice`]).
const ACTS: usize = 8;
/// The turns given, once a request's build has settled, to what it left behind: its slot's
/// looks, read and write.
const QUIET: usize = 48;
/// The most steps of one client's check: the machine's rounds and the first look, with a look
/// each round (blaze-thin's `MOST_STEPS`).
const MOST_STEPS: u32 = 2 * MOST_ROUNDS + 2;
/// The checkout's head and root directory, as the requests name them.
const ROOT: &str = "/w";

/// The slot's premises: files of this machine that no test writes, old enough to be steady.
fn premises() -> Premises {
    let engine = std::env::current_exe().expect("the test's own executable");
    Premises {
        engine: engine.to_str().expect("the test's executable has a UTF-8 path").to_owned(),
        library: Some(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml").to_owned()),
    }
}

/// Request `which`'s targets: 0 a build of the checkout, 1 the same with a feature (another
/// key, the same program). `extra`: a second variable is set in the request's environment.
fn targets(which: usize, extra: bool) -> Targets {
    let mut env = vec![("SOLO".to_owned(), "1".to_owned())];
    if extra {
        env.push(("SOLO_EXTRA".to_owned(), "1".to_owned()));
    }
    env.sort();
    let cargo = blaze_api::Cargo {
        cwd: ROOT.into(),
        env,
        args: Vec::new(),
        root: ROOT.into(),
        color: false,
        files: Vec::new(),
        width: None,
        command: "build".into(),
        refused: None,
        verbose: 0,
        prebuilt: true,
    };
    let features = if which == 1 { vec!["x".to_owned()] } else { Vec::new() };
    Targets { features, cargo: Some(cargo), ..agent() }
}

fn request(which: usize, extra: bool) -> Request {
    Request::Build { view: ViewRef::Head(ROOT.into()), targets: targets(which, extra) }
}

/// Where request `which`'s slot is, as the simulation names files: the daemon's
/// `<state>/slots/<key hex>` (handoff-solo.md HS1).
fn slot_of(which: usize, extra: bool) -> FileName {
    let key = key_of("build", &targets(which, extra), ROOT).expect("a cargo command line");
    name(&format!("{STATE}/slots/{key:x}"))
}

fn start_name() -> FileName {
    name(start_file(APP_AT).trim_start_matches('/'))
}

/// A start file's bytes as a reader with no decoder reads them: `"bzstart1" | [32] check |
/// [32] program | [32] record`.
fn start_bytes(record: ContentId) -> Vec<u8> {
    let mut bytes = START_MAGIC.to_vec(); // alloc: a test's file
    bytes.extend_from_slice(&[0; 32]);
    bytes.extend_from_slice(&ContentId::of("blaze serve tests solo program", APP_AT.as_bytes()).0);
    bytes.extend_from_slice(&record.0);
    bytes
}

/// The record of a build of request `which` (2: both at once) over `main.rs`'s `main`.
fn record_of(which: usize, main: &[u8]) -> ContentId {
    let mut bytes = vec![which as u8]; // alloc: a test's key
    bytes.extend_from_slice(main);
    ContentId::of("blaze serve tests solo launch record", &bytes)
}

/// The fakes' launch host: the code it holds for [`APP_AT`].
struct Host {
    /// The record of the code it holds.
    held: Option<ContentId>,
    /// The record of the request last sent, held once that request's job ran.
    next: Option<ContentId>,
    /// [`PLANS`] when it was sent: a plan load since is its job.
    sent_at: usize,
    /// Set when a build gave the program another record: its start file goes (H6).
    retired: bool,
}

thread_local! {
    static HOST: RefCell<Host> = const { RefCell::new(Host { held: None, next: None, sent_at: 0, retired: false }) };
}

/// `Fake::held_code` (handoff-solo.md HS2): what the launch host holds for `program`, asked by
/// the loop as a certificate records its outputs and as a check values them. Nothing but in
/// this file's runs.
pub(super) fn held_code(holder: CodeHolder, program: &str) -> Option<HeldCode> {
    if !matches!(holder, CodeHolder::Launcher) || program != APP_AT {
        return None;
    }
    HOST.with_borrow_mut(|host| {
        if PLANS.get() > host.sent_at
            && let Some(next) = host.next.take()
        {
            host.retired |= host.held.is_some_and(|it| it != next);
            host.held = Some(next);
        }
        host.held.map(HeldCode::Is)
    })
}

/// The client's check of request `which` over `sim`'s files, as blaze-thin's `solo.rs` makes
/// it; `daemon`: whether one runs. The answer of a hit, or why the daemon answers.
fn look_at(sim: &mut Sim, which: usize, extra: bool, daemon: bool) -> Result<Answer, String> {
    let targets = targets(which, extra);
    let key = key_of("build", &targets, ROOT).ok_or("no key")?;
    let file = slot_of(which, extra);
    if !sim.exists(&file) {
        return Err("no request slot".to_owned());
    }
    let bytes = sim.contents(&file);
    let env = &targets.cargo.as_ref().expect("a cargo command line").env;
    let premises = premises();
    let mut solo = Solo::open(&bytes, &key, env, &premises.engine, daemon).map_err(|why| why.to_string())?;
    let mut seen = None;
    for _ in 0..MOST_STEPS {
        match solo.step(seen.take()) {
            Step::Look(Look::Stamps(paths)) => {
                seen = Some(Seen::Stamps(paths.iter().map(|it| stamp_of(sim, &premises, it)).collect()));
            }
            Step::Look(Look::Head { path, len }) => seen = Some(Seen::Head(head_of(sim, &path, len))),
            Step::Done(Verdict::Hit(answer)) => return Ok(answer),
            Step::Done(Verdict::Daemon(why)) => return Err(why.to_string()),
        }
    }
    Err(format!("the client's check did not end within {MOST_STEPS} steps"))
}

/// The stamp of `path` now: a premise's of this machine's file (as the daemon took it), any
/// other's of the simulation's.
fn stamp_of(sim: &mut Sim, premises: &Premises, path: &str) -> Stamp {
    if path == premises.engine || premises.library.as_deref() == Some(path) {
        return crate::serve::local::stamp_now(Path::new(path)).unwrap_or(Stamp::GONE);
    }
    match FileName::new(path.trim_start_matches('/')) {
        Some(file) => sim.stamp(&file),
        None => Stamp::UNKNOWN,
    }
}

/// The first `len` bytes of the simulation's file at `path`, `None` where none is there.
fn head_of(sim: &Sim, path: &str, len: usize) -> Option<Vec<u8>> {
    let file = FileName::new(path.trim_start_matches('/'))?;
    if !sim.exists(&file) {
        return None;
    }
    let mut bytes = sim.contents(&file);
    bytes.truncate(len);
    Some(bytes)
}

/// Stops `daemon` as `restart` does (`blaze stop`): its disk.
fn stopped(mut daemon: SimDaemon) -> Sim {
    daemon.stop();
    let mut turns = 0;
    while daemon.turn() {
        turns += 1;
        assert!(turns < TURNS, "the daemon never stopped");
    }
    daemon.into_driver()
}

/// One act of a schedule.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Act {
    /// Request `.0` built twice with nothing changed, each settled: then the client hits (I4).
    Twice(usize),
    Build(usize),
    /// Request `which` sent, and `main.rs` edited `turns` turns after, while it builds.
    During { which: usize, turns: usize },
    /// Both requests at once, each from its own client.
    Together,
    Edit { same_size: bool },
    Add,
    Remove,
    /// The request's second variable set, or taken away.
    Env,
    /// `blaze stop`: no daemon until a request or a judge starts one.
    Stop,
    /// The power lost: what was not synced torn.
    Crash,
    /// The program's output deleted, and with `slots` every slot.
    Collect { slots: bool },
    /// A commit of `main.rs` to the checkout's head while the disk's `main.rs` changes.
    Commit,
}

/// Seed `seed`'s schedule.
fn schedule(seed: u64) -> Vec<Act> {
    let mut rng = Rng::new(seed ^ 0x5010_cafe);
    let mut acts = vec![Act::Twice(0)]; // alloc: a test's schedule
    for _ in 0..ACTS {
        let which = rng.index(2);
        acts.push(match rng.below(14) {
            0 => Act::Twice(which),
            1 | 2 => Act::Build(which),
            3 | 4 => Act::During { which, turns: rng.range(1..60) as usize },
            5 => Act::Together,
            6 => Act::Edit { same_size: rng.coin() },
            7 => Act::Add,
            8 => Act::Remove,
            9 => Act::Env,
            10 => Act::Stop,
            11 => Act::Crash,
            12 => Act::Collect { slots: rng.coin() },
            _ => Act::Commit,
        });
    }
    acts
}

/// A bug planted for a control.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Planted {
    None,
    /// `Plant::SlotBeforeLook`: the writer's stamps taken once, after its reads.
    Look,
    /// `Plant::ColumnOfAnotherRoot`: the slot takes the certificate's root whatever the column
    /// vouches for.
    Column,
    /// H6 undone: the launch host keeps a start file across a build that gave the program
    /// another record.
    KeepsStart,
}

/// What a run found.
#[derive(Debug)]
enum Finding {
    /// A hit a daemon started fresh on those files does not give: the invariant broken.
    Fresh(String),
    /// I4: after a build that passed and settled, with nothing changed, no hit.
    Promise(String),
    /// The run could not go on: what.
    Stuck(String),
}

#[derive(Default, Debug)]
struct Tally {
    looks: usize,
    hits: usize,
    judged: usize,
    moved: usize,
    promised: usize,
}

impl Tally {
    fn add(&mut self, other: &Tally) {
        self.looks += other.looks;
        self.hits += other.hits;
        self.judged += other.judged;
        self.moved += other.moved;
        self.promised += other.promised;
    }
}

enum World {
    Up(SimDaemon),
    Down(Sim),
}

struct Run {
    seed: u64,
    plant: Planted,
    steps: Arc<Fake>,
    world: Option<World>,
    /// Daemons started, each seeded apart.
    starts: u64,
    /// One more at each start: a client of an earlier daemon is gone.
    epoch: u64,
    extra: bool,
    edits: u64,
    /// Whether the client looks between turns (not inside [`Act::Twice`], whose look is I4's).
    checking: bool,
    /// The hits judged since the world last changed, by request: not judged again.
    judged: Vec<(usize, Answer)>,
    tally: Tally,
    finding: Option<Finding>,
}

impl Run {
    fn sim(&mut self) -> &mut Sim {
        match self.world.as_mut().expect("a world") {
            World::Up(daemon) => daemon.driver(),
            World::Down(sim) => sim,
        }
    }

    fn daemon(&mut self) -> &mut SimDaemon {
        match self.world.as_mut() {
            Some(World::Up(daemon)) => daemon,
            _ => panic!("seed {}: no daemon runs", self.seed),
        }
    }

    fn found(&mut self, finding: Finding) {
        if self.finding.is_none() {
            self.finding = Some(finding);
        }
    }

    /// The world changed by the test: every hit is judged again.
    fn changed(&mut self) {
        self.judged.clear();
    }

    /// A daemon started on `sim`, with this run's premises.
    fn begin(&mut self, sim: Sim) -> SimDaemon {
        self.starts += 1;
        self.epoch += 1;
        crate::release::for_tests();
        let options = Options { premises: premises(), ..options() };
        let seed = self.seed.wrapping_mul(1_000).wrapping_add(self.starts);
        // clone: the daemon's handle on the fakes
        Daemon::start(sim, options, self.steps.clone(), Runner::Inline(Inline::new(seed)))
            .unwrap_or_else(|why| panic!("seed {}: {why}", self.seed))
    }

    fn up(&mut self) {
        // A daemon that runs stays: only a world that is down is taken to start one.
        if !matches!(self.world, Some(World::Down(_))) {
            return;
        }
        if let Some(World::Down(sim)) = self.world.take() {
            let daemon = self.begin(sim);
            self.world = Some(World::Up(daemon));
        }
    }

    /// One turn of the daemon, then the launch host's H6, then the client's looks; `false`
    /// where no daemon runs.
    fn turn(&mut self) -> bool {
        let Some(World::Up(daemon)) = self.world.as_mut() else { return false };
        daemon.turn();
        self.retire();
        self.checks();
        true
    }

    /// H6: a build gave the program another record, so its start file goes (but planted).
    fn retire(&mut self) {
        if !HOST.with_borrow_mut(|it| std::mem::take(&mut it.retired)) {
            return;
        }
        self.changed();
        if self.plant == Planted::KeepsStart {
            return;
        }
        let file = start_name();
        if self.sim().exists(&file) {
            self.sim().delete(&file);
        }
    }

    /// H5: the start file of what the host holds, placed once a build has settled.
    fn place(&mut self) {
        let Some(held) = HOST.with_borrow(|it| it.held) else { return };
        let (file, bytes) = (start_name(), start_bytes(held));
        if self.sim().contents(&file) != bytes {
            self.sim().change(&file, &bytes);
        }
    }

    /// The client's look at each request; each hit not judged since the world changed is.
    fn checks(&mut self) {
        if !self.checking || self.finding.is_some() {
            return;
        }
        for which in 0..2 {
            let (daemon, extra) = (matches!(self.world, Some(World::Up(_))), self.extra);
            self.tally.looks += 1;
            let Ok(hit) = look_at(self.sim(), which, extra, daemon) else { continue };
            if self.judged.iter().any(|(it, answer)| *it == which && *answer == hit) {
                continue;
            }
            self.judged.push((which, hit.clone())); // clone: the judged list's own
            self.judge(which, hit);
            if self.finding.is_some() {
                return;
            }
        }
    }

    /// The oracle of a hit: a daemon started fresh on these files answers request `which`
    /// with no job and says what `hit` says.
    fn judge(&mut self, which: usize, hit: Answer) {
        self.tally.hits += 1;
        let sim = match self.world.take().expect("a world") {
            World::Up(daemon) => stopped(daemon),
            World::Down(sim) => sim,
        };
        let mut daemon = self.begin(sim);
        let extra = self.extra;
        if look_at(daemon.driver(), which, extra, true).as_ref() != Ok(&hit) {
            // The stop moved what the client looked at: no daemon starts on those files.
            self.tally.moved += 1;
            self.world = Some(World::Up(daemon));
            return;
        }
        let main = daemon.driver().contents(&name("w/src/main.rs"));
        PLANS.set(0);
        HOST.with_borrow_mut(|it| (it.next, it.sent_at) = (Some(record_of(which, &main)), 0));
        let mut client = dial(&mut daemon);
        let id = client.send(&mut daemon, request(which, extra));
        let got = client.wait(&mut daemon, id);
        let jobs = PLANS.get();
        self.tally.judged += 1;
        self.world = Some(World::Up(daemon));
        let said = blaze_protocol::cbor::from_slice::<Vec<Response>>(&hit.said).map(|mut said| {
            said.push(Response::Done { ok: true });
            said
        });
        match said {
            Ok(said) if jobs == 0 && got == said => {}
            Ok(said) => self.found(Finding::Fresh(format!(
                "the client alone answered request {which}, and a daemon started fresh on those files ran {jobs} plan loads \
                 and said {got:#?}, not what the hit says, {said:#?}"
            ))),
            Err(error) => self.found(Finding::Fresh(format!("the hit's answer to request {which} does not decode: {error}"))),
        }
    }

    /// Request `which` sent from a client of its own.
    fn send(&mut self, which: usize) -> (Client, u64) {
        self.up();
        let main = self.sim().contents(&name("w/src/main.rs"));
        let request = request(which, self.extra);
        let daemon = self.daemon();
        let mut client = dial(daemon);
        HOST.with_borrow_mut(|it| (it.next, it.sent_at) = (Some(record_of(which, &main)), PLANS.get()));
        let id = client.send(daemon, request);
        (client, id)
    }

    /// Turns until every request of `asked` is done: `false` where a judge's start left their
    /// daemon first.
    fn wait(&mut self, asked: &mut [(Client, u64)]) -> bool {
        let epoch = self.epoch;
        for _ in 0..TURNS {
            if !self.turn() || self.epoch != epoch {
                return false;
            }
            let daemon = self.daemon();
            for (client, _) in asked.iter_mut() {
                client.take(daemon);
            }
            if asked.iter().all(|(client, id)| client.done(*id)) {
                return true;
            }
        }
        self.found(Finding::Stuck(format!("{} requests never ended", asked.len())));
        false
    }

    /// Turns until what the answered builds left has settled, then [`QUIET`] more, then H5.
    fn quiet(&mut self) {
        let mut turns = 0;
        loop {
            let Some(World::Up(daemon)) = self.world.as_mut() else { return };
            if daemon.jobs_settling() == 0 && daemon.settled() {
                break;
            }
            turns += 1;
            if turns == TURNS {
                return self.found(Finding::Stuck("what the builds left never settled".to_owned()));
            }
            self.turn();
        }
        for _ in 0..QUIET {
            self.turn();
        }
        self.place();
        self.checks();
    }

    fn edit(&mut self, same_size: bool) {
        self.edits += 1;
        let n = self.edits % 1_000;
        let text = match same_size {
            true => format!("fn main() {{ let _ = {n:03}; }}\n"),
            false => format!("fn main() {{ let _ = {n}; }} // {}\n", "x".repeat((n % 5) as usize)),
        };
        self.sim().change(&name("w/src/main.rs"), text.as_bytes());
        self.changed();
    }

    /// Request `which` built twice with nothing changed, each passing and settled; then the
    /// client, with the daemon running, hits (I4).
    fn twice(&mut self, which: usize) {
        self.checking = false;
        for round in 0..2 {
            let mut asked = [self.send(which)];
            if !self.wait(&mut asked) {
                self.checking = true;
                return;
            }
            let (client, id) = &asked[0];
            if client.of(*id).last() != Some(&Response::Done { ok: true }) {
                let got = client.of(*id);
                self.checking = true;
                return self.found(Finding::Promise(format!("build {round} of request {which} did not pass: {got:#?}")));
            }
            self.quiet();
        }
        self.checking = true;
        let extra = self.extra;
        match look_at(self.sim(), which, extra, true) {
            Ok(_) => self.tally.promised += 1,
            Err(why) => self.found(Finding::Promise(format!(
                "after two builds of request {which} that passed and settled, with nothing changed, the client does not answer alone: {why}"
            ))),
        }
    }

    fn act(&mut self, act: Act) {
        match act {
            Act::Twice(which) => self.twice(which),
            Act::Build(which) => {
                let mut asked = [self.send(which)];
                if self.wait(&mut asked) {
                    self.quiet();
                }
            }
            Act::During { which, turns } => {
                let mut asked = [self.send(which)];
                let epoch = self.epoch;
                for _ in 0..turns {
                    if !self.turn() || self.epoch != epoch {
                        return;
                    }
                }
                self.edit(true);
                if self.wait(&mut asked) {
                    self.quiet();
                }
            }
            Act::Together => {
                let first = self.send(0);
                let second = self.send(1);
                let mut asked = [first, second];
                if self.wait(&mut asked) {
                    self.quiet();
                }
            }
            Act::Edit { same_size } => self.edit(same_size),
            Act::Add => {
                self.sim().change(&name("w/src/extra.rs"), b"// more\n");
                self.changed();
            }
            Act::Remove => {
                let file = name("w/src/extra.rs");
                if self.sim().exists(&file) {
                    self.sim().delete(&file);
                    self.changed();
                }
            }
            Act::Env => {
                self.extra = !self.extra;
                self.changed();
            }
            Act::Stop => {
                self.world = match self.world.take().expect("a world") {
                    World::Up(daemon) => Some(World::Down(stopped(daemon))),
                    down @ World::Down(_) => Some(down),
                };
            }
            Act::Crash => {
                let mut sim = match self.world.take().expect("a world") {
                    World::Up(daemon) => daemon.into_driver(),
                    World::Down(sim) => sim,
                };
                sim.crash();
                self.world = Some(World::Down(sim));
                self.changed();
            }
            Act::Collect { slots } => {
                let program = name(&APP_AT[1..]);
                if self.sim().exists(&program) {
                    self.sim().delete(&program);
                }
                if slots {
                    for which in 0..2 {
                        let file = slot_of(which, self.extra);
                        if self.sim().exists(&file) {
                            self.sim().delete(&file);
                        }
                    }
                }
                self.changed();
            }
            Act::Commit => {
                self.up();
                let Some(base) = self.daemon().vfs().head(ROOT) else { return };
                let ops = vec![blaze_vfs::Op::Write {
                    path: "src/main.rs".into(),
                    bytes: b"fn main() { committed(); }\n".to_vec(),
                    mode: blaze_vfs::Mode::Regular,
                }];
                let daemon = self.daemon();
                let mut client = dial(daemon);
                let id = client.send(daemon, Request::Commit { head: ROOT.into(), base, ops });
                // The disk's `main.rs` changes under the commit: the watcher sees both.
                self.edit(false);
                let mut asked = [(client, id)];
                if self.wait(&mut asked) {
                    self.quiet();
                }
            }
        }
        self.checks();
    }
}

/// Seed `seed`'s schedule run with `plant`: its tally, or what it found and where.
fn run(seed: u64, plant: Planted) -> Result<Tally, (Finding, String)> {
    HOST.with_borrow_mut(|it| *it = Host { held: None, next: None, sent_at: 0, retired: false });
    SLOT_PLANT.set(match plant {
        Planted::Look => SlotPlant::SlotBeforeLook,
        Planted::Column => SlotPlant::ColumnOfAnotherRoot,
        Planted::None | Planted::KeepsStart => SlotPlant::None,
    });
    OUTPUT.set(true);
    let mut sim = Sim::new(seed, Faults::default());
    checkout(&mut sim, "[package]\nname = \"app\"\n");
    let mut run = Run {
        seed,
        plant,
        steps: Arc::new(Fake::default()),
        world: Some(World::Down(sim)),
        starts: 0,
        epoch: 0,
        extra: false,
        edits: 0,
        checking: true,
        judged: Vec::new(),
        tally: Tally::default(),
        finding: None,
    };
    run.up();
    let acts = schedule(seed);
    let mut at = 0;
    while at < acts.len() && run.finding.is_none() {
        run.act(acts[at]);
        at += 1;
    }
    OUTPUT.set(false);
    SLOT_PLANT.set(SlotPlant::None);
    HOST.with_borrow_mut(|it| *it = Host { held: None, next: None, sent_at: 0, retired: false });
    match run.finding {
        Some(finding) => Err((finding, format!("seed {seed}, planted {plant:?}, act {} of {acts:?}", at.saturating_sub(1)))),
        None => Ok(run.tally),
    }
}

/// The first seed whose run with `plant` the oracle catches: where.
fn caught(plant: Planted) -> Option<String> {
    (0..SEEDS).find_map(|seed| match run(seed, plant) {
        Err((Finding::Fresh(why), at)) => Some(format!("{at}: {why}")),
        // Caught by I4 or stuck: not the oracle of a hit.
        Err(_) | Ok(_) => None,
    })
}

/// The invariant (I1 for both sources): between any two turns of the loop, after any crash, with
/// or without a daemon, across edits made while none ran, a collection, and a second client, a
/// `Solo` driven over the simulation's files answers `Hit(a)` only where a daemon started fresh
/// on those files answers the request `a` with no job; and the run after a build that passed and
/// settled, its launcher's start file placed, is a solo hit (I4).
///
/// Controls, each a hit on other bytes the oracle catches over some seed:
/// `Plant::ColumnOfAnotherRoot` (a file saved under the build: the slot pairs the build's root
/// with the column's new stamps) and H6 undone (the launch host keeps a start file across a
/// build that gave the program another record: the other key's slot still hits on it).
/// `Plant::SlotBeforeLook` is [`the_oracle_catches_a_slot_written_before_its_look`].
#[test]
fn a_client_alone_answers_what_a_fresh_daemon_answers() {
    let mut tally = Tally::default();
    for seed in 0..SEEDS {
        match run(seed, Planted::None) {
            Ok(it) => tally.add(&it),
            Err((finding, at)) => panic!("{at}: {finding:?}"),
        }
    }
    assert!(tally.promised >= SEEDS as usize, "every seed's first act checks I4: {tally:?}");
    assert!(tally.judged > 0, "no hit was judged by a fresh daemon: the oracle proves nothing: {tally:?}");
    assert!(tally.judged > tally.moved, "most hits moved under the stop and were not judged: {tally:?}");
    for plant in [Planted::Column, Planted::KeepsStart] {
        assert!(caught(plant).is_some(), "the planted {plant:?} was never caught over {SEEDS} seeds");
    }
}

/// The control of `Plant::SlotBeforeLook`: a name saved unknown read, then edited before the
/// writer's one look, gives a slot of the edit's stamp over the read's bytes, which the oracle
/// catches. It is reached because the daemon's racy questions ask the simulation's clock, the
/// one its disk stamps by: a name edited just before the column's look is saved unknown.
#[test]
fn the_oracle_catches_a_slot_written_before_its_look() {
    assert!(caught(Planted::Look).is_some(), "the planted SlotBeforeLook was never caught over {SEEDS} seeds");
}
