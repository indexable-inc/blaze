//! The needs core under blaze-core's deterministic simulation: launchers attach, call and
//! detach, a request wants the front end, and the driver's channel hands the core what it
//! heard in batches of any size, in the order it was sent.

use std::collections::VecDeque;
use std::time::Duration;

use blaze_core::Driver as _;
use blaze_core::sim::{Faults, Sim};
use blaze_ids::ContentId;
use blaze_launch_host::Conn;

use blaze_sim::{Broke, Config, Tape};

use super::{ATTACH, BUILD, Front, FrontPlant, Heard, Need, Plant, Refuse, SERVED_WITHIN, Serve, Served, Step, Want};

/// What became of one run of the task.
struct Ran {
    /// How many times each need (by its number) was made, given back at `Wanted`, or
    /// refused by the driver after the task ended.
    made: Vec<u32>,
    left: Vec<u32>,
    refused: Vec<u32>,
    /// A wanting wake whose next step was not `Done(Wanted)`.
    unheeded: bool,
    done: Option<Served>,
}

fn need(n: u32) -> Need {
    let mut identity = [0u8; 32];
    identity[..4].copy_from_slice(&n.to_le_bytes());
    Need { program: [7; 32], conn: Conn(u64::from(n % 3)), slot: n, identity: ContentId(identity) }
}

/// One run of the task under `seed`: the sim's own events (launchers attaching and
/// detaching, their first calls, a wanting wake at a random moment or none), the driver's
/// channel delivering them in batches, and the driver making each need it is told to.
fn run(seed: u64, plant: Plant) -> Ran {
    let mut sim = Sim::new(seed, Faults::default());
    let launchers = sim.rng().below(3) as u32;
    let calls = sim.rng().below(12) as u32;
    let wants = sim.rng().one_in(2);
    // The events, in the order the readers send them: each launcher attaches, calls, and
    // detaches; the wake falls anywhere.
    // alloc: a test's events
    let mut events: Vec<Heard> = Vec::new();
    let mut attached = 0;
    let mut sent = 0;
    for _ in 0..launchers {
        attached += 1;
        events.push(Heard::Attached(attached));
        for _ in 0..calls {
            events.push(Heard::Need(need(sent)));
            sent += 1;
        }
    }
    for _ in 0..launchers {
        let at = events.len() - sim.rng().index(events.len().min(4));
        attached -= 1;
        events.insert(at.max(1), Heard::Attached(attached));
    }
    // Detaches are counts: whatever order they landed in, the last one says zero.
    if launchers > 0 {
        events.push(Heard::Attached(0));
    }
    if wants {
        let at = sim.rng().index(events.len() + 1);
        events.insert(at, Heard::Woken { wanted: true });
    }
    let mut channel: VecDeque<Heard> = events.into();

    let mut serve = Serve::new(sim.now(), 0);
    serve.plant = plant;
    let mut ran = Ran {
        made: vec![0; sent as usize],
        left: vec![0; sent as usize],
        refused: vec![0; sent as usize],
        unheeded: false,
        done: None,
    };
    let mut step = serve.step(Heard::Deadline(sim.now()));
    // Bounded: every round hears at least one thing or moves the clock to a deadline.
    for _ in 0..10_000 {
        let mut make = None;
        if let Step::Done(done) = step {
            ran.done = Some(done);
            break;
        }
        if let Step::Make(need) = step {
            make = Some(need);
        }
        if channel.is_empty() && make.is_none() {
            // Nothing to hear: the wait ends at its deadline.
            if let Step::Wait { until } = step {
                sim.advance(until.left(sim.now()) + Duration::from_millis(1));
            }
            step = serve.step(Heard::Deadline(sim.now()));
            continue;
        }
        // A batch of what the channel holds, each heard in turn.
        let batch = 1 + sim.rng().index(channel.len().max(1));
        for _ in 0..batch {
            let Some(heard) = channel.pop_front() else { break };
            step = serve.step(heard);
            if heard == (Heard::Woken { wanted: true }) && step != Step::Done(Served::Wanted) {
                ran.unheeded = true;
            }
            match step {
                Step::Make(need) => make = Some(need),
                Step::Done(_) => break,
                Step::Wait { .. } => {}
            }
        }
        if let Step::Done(done) = step {
            ran.done = Some(done);
            break;
        }
        if let Some(need) = make {
            ran.made[need.slot as usize] += 1;
            let wait = Duration::from_micros(sim.rng().below(3_000));
            sim.advance(wait);
            step = serve.step(Heard::Made);
        }
    }
    // The driver after the task: what it left goes back to the channel for the next task
    // at `Wanted`; otherwise every need still in the channel is refused by name.
    if ran.done == Some(Served::Wanted) {
        for need in serve.left() {
            ran.left[need.slot as usize] += 1;
        }
    }
    for heard in channel {
        if let Heard::Need(need) = heard {
            match ran.done {
                Some(Served::Wanted) => ran.left[need.slot as usize] += 1,
                _ => ran.refused[need.slot as usize] += 1,
            }
        }
    }
    ran
}

/// What the oracle finds wrong with a run, if anything.
fn broken(ran: &Ran) -> Option<String> {
    if ran.done.is_none() {
        return Some("the task never ended".to_owned());
    }
    if ran.unheeded {
        return Some("a wanting wake's next step was not Done(Wanted)".to_owned());
    }
    for n in 0..ran.made.len() {
        let answers = ran.made[n] + ran.left[n] + ran.refused[n];
        if answers != 1 {
            return Some(format!(
                "need {n} was made {} times, left {} and refused {}: not once",
                ran.made[n], ran.left[n], ran.refused[n]
            ));
        }
    }
    None
}

/// The invariant: over every interleaving of needs, launchers attaching and detaching, a
/// request wanting the front end and the deadline, under many seeds, each need gets exactly
/// one `Make` or is still queued at `Wanted` (or, after the task let the front end go
/// otherwise, is refused by the driver), the step after `Woken { wanted: true }` is
/// `Done(Wanted)`, and `Done` is reached.
#[test]
fn a_need_is_made_by_the_front_end_or_refused_and_never_dropped() {
    for seed in 0..2_000 {
        if let Some(why) = broken(&run(seed, Plant::None)) {
            panic!("seed {seed}: {why}");
        }
    }
    // A task no launcher attaches to ends once `ATTACH` passed, and not before.
    let mut serve = Serve::new(Duration::ZERO, 0);
    assert_eq!(serve.step(Heard::Deadline(ATTACH / 2)), Step::Wait { until: blaze_core::wait::Deadline::at(ATTACH) });
    assert_eq!(serve.step(Heard::Deadline(ATTACH)), Step::Done(Served::NeverAttached));
    // Controls: each planted bug is caught under some seed.
    for plant in [Plant::ServesWhenWanted, Plant::EndsWithNeeds] {
        let caught = (0..2_000).any(|seed| broken(&run(seed, plant)).is_some());
        assert!(caught, "{plant:?} is not caught");
    }
}

/// One run of a program's [`Front`] under `seed`: the sim's events in any order (first calls
/// no record answers, a build's link, the daemon's request ending well or not, the serving
/// task ending, the clock moving), with the driver's half done as `launch` does it: a `Build`
/// sends one request, a refusal refuses every need queued, a served task takes them.
/// What the oracle finds wrong, if anything.
fn front_run(seed: u64, plant: FrontPlant) -> Option<String> {
    let mut sim = Sim::new(seed, Faults::default());
    let mut front = Front::new();
    front.plant = plant;
    // The needs queued and not yet served or refused, by number, and how each ended.
    let (mut queued, mut ended): (Vec<u32>, Vec<u32>) = (Vec::new(), Vec::new());
    // Requests the daemon sent itself that have not ended; whether a task serves now.
    let (mut requests, mut serving) = (0u32, false);
    let mut next = 0u32;
    let refuse = |queued: &mut Vec<u32>, ended: &mut Vec<u32>| ended.extend(queued.drain(..));
    for _ in 0..sim.rng().below(40) + 1 {
        let now = sim.now();
        match sim.rng().below(6) {
            0 | 1 => {
                queued.push(next);
                next += 1;
                if front.want(now) == Want::Build {
                    if requests > 0 || front.is_held() {
                        return Some(format!("a second build was asked while one was asked or held, at need {next}"));
                    }
                    requests += 1;
                }
                if serving {
                    // The serving task takes what is queued.
                    ended.extend(queued.drain(..));
                }
            }
            // The build's link, only of a request under way, and the task after it at once
            // (`held_front_run` takes them apart).
            2 if requests > 0 && !front.is_held() => {
                front.held(now, !queued.is_empty());
                front.serving();
                serving = true;
                ended.extend(queued.drain(..));
            }
            // The daemon's request ends, well or not.
            3 if requests > 0 => {
                requests -= 1;
                if front.built(sim.rng().one_in(2)).is_some() {
                    refuse(&mut queued, &mut ended);
                }
            }
            // The serving task ends Detached: what is left asks again (`launch::serve_needs`).
            4 if serving => {
                serving = false;
                front.let_go();
                if !queued.is_empty() && front.want(now) == Want::Build {
                    requests += 1;
                }
            }
            _ => {
                let wait = Duration::from_secs(sim.rng().below(90));
                sim.advance(wait);
                if front.tick(sim.now()).is_some() {
                    // The late request still ends later; it holds nothing.
                    refuse(&mut queued, &mut ended);
                }
            }
        }
        if let Some(due) = front.due()
            && due.passed(sim.now())
            && front.tick(sim.now()).is_none()
        {
            return Some("a build asked past its bound is not refused".to_owned());
        }
    }
    // Every request ends, and the clock runs out past the bound.
    while requests > 0 {
        requests -= 1;
        if front.built(true).is_some() {
            refuse(&mut queued, &mut ended);
        }
    }
    sim.advance(BUILD * 2);
    if front.tick(sim.now()).is_some() {
        refuse(&mut queued, &mut ended);
    }
    if !serving && !queued.is_empty() {
        return Some(format!("{} needs were left queued with no task and no build to serve them", queued.len()));
    }
    ended.sort_unstable();
    if ended.windows(2).any(|it| it[0] == it[1]) {
        return Some("a need was served or refused twice".to_owned());
    }
    None
}

/// The invariant: over every order of wants, links, the request's end, the task's end and
/// ticks, at most one build is asked while one is asked or held, and from every state a
/// queued need is served by a task or refused once. Controls: `FrontPlant::AsksTwice` (a
/// second build is asked), `FrontPlant::NeverLate` (a build that never holds leaves its needs
/// queued forever).
#[test]
fn a_need_with_no_front_end_asks_one_build_and_ends_made_or_refused() {
    for seed in 0..2_000 {
        if let Some(why) = front_run(seed, FrontPlant::None) {
            panic!("seed {seed}: {why}");
        }
    }
    let mut front = Front::new();
    assert_eq!(front.want(Duration::ZERO), Want::Build);
    assert_eq!(front.want(Duration::from_secs(1)), Want::Queue, "asked: queued");
    assert_eq!(front.tick(BUILD - Duration::from_secs(1)), None);
    assert_eq!(front.tick(BUILD), Some(Refuse::Late));
    assert_eq!(front.built(true), None, "a late request's end refuses nothing again");
    assert_eq!(front.want(BUILD), Want::Build, "a refusal leaves it none");
    assert_eq!(front.built(false), Some(Refuse::Failed));
    assert_eq!(front.want(BUILD), Want::Build);
    assert_eq!(front.built(true), Some(Refuse::NoStart));
    for plant in [FrontPlant::AsksTwice, FrontPlant::NeverLate] {
        let caught = (0..2_000).any(|seed| front_run(seed, plant).is_some());
        assert!(caught, "{plant:?} is not caught");
    }
}

/// What the model of [`held_front_run`] knows of the program's front end, apart from the core.
struct Model {
    /// Each need queued and not yet served or refused: its number, and when its wait began
    /// (its queueing, or the last link or hand-back after it).
    queued: Vec<(u32, Duration)>,
    /// Each need served or refused, by number.
    ended: Vec<u32>,
    /// A build's link holds the front end for the program, and has not let it go.
    linked: bool,
    /// The task after that build serves from it now.
    serving: bool,
    /// The front end the last link held was lost (the store after its answer failed).
    lost: bool,
    /// Requests the daemon sent itself that have not ended.
    requests: u32,
}

impl Model {
    fn end_all(&mut self) {
        self.ended.extend(self.queued.drain(..).map(|(need, _)| need));
    }

    /// The oracle, after every event: once the front end is lost no need waits, and no need
    /// waits with no task serving past its bound (`SERVED_WITHIN` while a link holds the front
    /// end, `BUILD` while a build is asked).
    fn broken(&self, now: Duration) -> Result<(), Broke> {
        if self.lost && !self.queued.is_empty() {
            return Err(Broke::Oracle(format!(
                "{} needs wait after the front end its link held was lost, for a task that will not come",
                self.queued.len()
            )));
        }
        if self.serving {
            return Ok(());
        }
        let bound = if self.linked { SERVED_WITHIN } else { BUILD };
        match self.queued.iter().find(|(_, from)| now.saturating_sub(*from) >= bound) {
            Some((need, from)) => Err(Broke::Oracle(format!(
                "need {need} waited {} s with no task serving, past its bound of {} s",
                now.saturating_sub(*from).as_secs(),
                bound.as_secs()
            ))),
            None => Ok(()),
        }
    }
}

/// One run of a program's [`Front`] from the tape, with the build's link and the task after
/// its answer apart, as they are: first calls no record answers, links, the serving task
/// starting and ending (`Wanted` or `Detached`), the store after an answer failing
/// ([`Front::lost`], as `launch::lose_needs` drives it), the daemon's own request ending, and
/// the clock moving with the loop's tick. The driver's half is done as `launch` does it.
fn held_front_run(tape: &Tape, plant: FrontPlant) -> Result<(), Broke> {
    let mut front = Front::new();
    front.plant = plant;
    let mut now = Duration::ZERO;
    let mut model =
        Model { queued: Vec::new(), ended: Vec::new(), linked: false, serving: false, lost: false, requests: 0 };
    let mut next = 0u32;
    let events: usize = tape.int_in(1..=40, "events");
    for _ in 0..events {
        match tape.int_in(0u8..=7, "event") {
            // A launcher's first call no record answers.
            0 | 1 => {
                let need = next;
                next += 1;
                match front.want(now) {
                    Want::Build => {
                        model.requests += 1;
                        model.queued.push((need, now));
                    }
                    Want::Queue => model.queued.push((need, now)),
                    Want::Refuse if model.lost => model.ended.push(need),
                    Want::Refuse => return Err(Broke::Oracle(format!("need {need} was refused with no loss"))),
                }
                if model.serving {
                    model.end_all();
                }
            }
            // A build's link: the daemon's own or a person's `blaze run`.
            2 if !model.serving => {
                front.held(now, !model.queued.is_empty());
                model.linked = true;
                model.lost = false;
                for (_, from) in &mut model.queued {
                    *from = now;
                }
            }
            // The task after the linked build's answer starts serving, and takes the queue; or,
            // once lost, the task after a later build of the checkout.
            3 if (model.linked || model.lost) && !model.serving => {
                front.serving();
                model.linked = true;
                model.lost = false;
                model.serving = true;
                model.end_all();
            }
            // The task hands the front end to a request that wants it (`Wanted`).
            4 if model.serving => {
                model.serving = false;
                front.handed_back(now, !model.queued.is_empty());
            }
            // The task ends with no launcher left (`Detached`): no one holds the front end.
            5 if model.serving => {
                model.serving = false;
                model.linked = false;
                front.let_go();
            }
            // The store after a build's answer failed (or panicked, or found the front end
            // taken): its task serves no more.
            6 => {
                tape.reached("a loss");
                if front.lost("the store refused a record") {
                    model.end_all();
                }
                if model.linked {
                    model.lost = true;
                    model.linked = false;
                    model.serving = false;
                }
            }
            // The daemon's own request ends, well or not.
            7 if model.requests > 0 => {
                model.requests -= 1;
                if front.built(tape.bool("built well")).is_some() {
                    model.end_all();
                }
            }
            // The clock moves, and the loop ticks.
            _ => {
                now += Duration::from_secs(tape.int_in(0u64..=90, "seconds"));
                if let Some(refused) = front.tick(now) {
                    model.end_all();
                    if refused == Refuse::Unserved {
                        model.linked = false;
                    }
                }
            }
        }
        model.broken(now)?;
    }
    // Every request ends, and the clock runs past every bound, with the loop's tick.
    while model.requests > 0 {
        model.requests -= 1;
        if front.built(true).is_some() {
            model.end_all();
        }
    }
    now += BUILD.max(SERVED_WITHIN) * 2;
    if front.tick(now).is_some() {
        model.end_all();
    }
    model.broken(now)?;
    model.ended.sort_unstable();
    if model.ended.windows(2).any(|it| it[0] == it[1]) {
        return Err(Broke::Oracle("a need was served or refused twice".to_owned()));
    }
    Ok(())
}

/// The invariant: a run either starts, or ends with a named failure, within its bound of its
/// build's answer. Over every order of first calls, links, the serving task's start and end,
/// the store after an answer failing, the daemon's own request ending and the clock, a need
/// queued while no task serves is refused at once once the front end its link held is lost,
/// and otherwise served or refused within `SERVED_WITHIN` of its wait's start while a link
/// holds the front end (within `BUILD` while a build is asked); each need ends once.
/// Controls: `FrontPlant::LeftOpen` (the held run is left open: a loss is not heard, the
/// ripgrep hang) and `FrontPlant::Unbounded` (the wait has no bound).
#[test]
fn a_need_held_for_a_task_that_never_comes_is_refused_by_name_within_its_bound() {
    let config = || Config::cases(1_024).must_reach(&["a loss"]);
    blaze_sim::control(config(), FrontPlant::None, FrontPlant::LeftOpen, "was lost", held_front_run);
    blaze_sim::control(config(), FrontPlant::None, FrontPlant::Unbounded, "past its bound", held_front_run);
    // The ripgrep case by hand: link, answer, a first call, the store fails.
    let mut front = Front::new();
    front.held(Duration::ZERO, false);
    assert_eq!(front.want(Duration::from_secs(1)), Want::Queue, "held: queued for the task");
    assert!(front.lost("the store holds other bytes under 7f98e0838b31"));
    assert_eq!(front.want(Duration::from_secs(2)), Want::Refuse, "lost: refused at once");
    assert_eq!(front.lost_why(), Some("the store holds other bytes under 7f98e0838b31"));
    front.held(Duration::from_secs(3), false);
    assert_eq!(front.want(Duration::from_secs(3)), Want::Queue, "a link holds one again");
    // With no loss said, the bound ends it.
    assert_eq!(front.tick(Duration::from_secs(3) + SERVED_WITHIN - Duration::from_secs(1)), None);
    assert_eq!(front.due(), Some(blaze_core::wait::Deadline::after(Duration::from_secs(3), SERVED_WITHIN)));
    assert_eq!(front.tick(Duration::from_secs(3) + SERVED_WITHIN), Some(Refuse::Unserved));
}
