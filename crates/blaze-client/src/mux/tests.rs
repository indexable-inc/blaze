//! The core against a scripted daemon: a plan draws the requests, what the daemon answers each
//! with, how its frames interleave, where its bytes are cut, when it hangs up, a frame of
//! another schema and deadlines that pass. The oracle keeps each request's script beside the
//! run and checks every event against it. One planted bug ([`Plant::ByArrival`]) proves the
//! oracle catches what it claims to.

use std::time::Duration;

use blaze_api::{Envelope, HotSince, Request, Response, SCHEMA, frames};
use blaze_core::wait::Deadline;
use blaze_rng::SplitMix64;
use hegel::{TestCase, generators as gs};

use super::{Mux, End, Event, Fault, Plant};

/// How many seeded plans a control may take to catch its plant.
const CONTROL_SEEDS: u64 = 256;

trait Draw {
    fn int(&mut self, lo: u64, hi: u64) -> u64;

    fn coin(&mut self) -> bool {
        self.int(0, 1) == 1
    }
}

struct Drawn<'a>(&'a TestCase);

impl Draw for Drawn<'_> {
    fn int(&mut self, lo: u64, hi: u64) -> u64 {
        self.0.draw(gs::integers::<u64>().min_value(lo).max_value(hi))
    }
}

impl Draw for SplitMix64 {
    fn int(&mut self, lo: u64, hi: u64) -> u64 {
        match (hi - lo).checked_add(1) {
            Some(n) => lo + self.below(n),
            None => self.next_u64(),
        }
    }
}

/// One request of a plan and what the daemon answers it with.
#[derive(Clone, Debug)]
struct Asked {
    /// How many responses come before its `Done`.
    responses: usize,
    ok: bool,
    /// When it must have ended, in the run's milliseconds; `None` for never.
    deadline: Option<u64>,
}

#[derive(Clone, Copy, Debug)]
enum Step {
    /// The client sends request `0`.
    Send(usize),
    /// The daemon writes the next frame of request `0`'s answer.
    Emit(usize),
    /// The clock moves on by so many milliseconds.
    Pass(u64),
}

#[derive(Clone, Debug)]
struct Plan {
    requests: Vec<Asked>,
    steps: Vec<Step>,
    /// How many bytes each delivery and each write takes, in turn, again from the first.
    chunks: Vec<usize>,
    /// The daemon hangs up once it has sent this many bytes; `None` after its last.
    cut: Option<usize>,
    /// The frame (of a request, by its place in the answer) the daemon sends in another schema.
    other_schema: Option<(usize, usize)>,
}

impl Plan {
    fn faultless(&self) -> bool {
        self.cut.is_none()
            && self.other_schema.is_none()
            && self.requests.iter().all(|it| it.deadline.is_none())
    }
}

/// A plan, its fault classes drawn as a swarm: each of a hang-up part way, a frame of another
/// schema and deadlines is on in about half the plans.
fn plan(d: &mut impl Draw) -> Plan {
    let (cuts, schemas, deadlines) = (d.coin(), d.coin(), d.coin());
    let count = d.int(1, 5) as usize;
    let requests: Vec<Asked> = (0..count)
        .map(|_| Asked {
            responses: d.int(0, 3) as usize,
            ok: d.coin(),
            deadline: (deadlines && d.coin()).then(|| d.int(1, 40)),
        })
        .collect();
    // Each request's tokens in its own order (its send, then each frame of its answer), the
    // requests' interleaved as drawn.
    let mut left: Vec<usize> = requests.iter().map(|it| it.responses + 2).collect();
    let mut steps = Vec::new();
    while left.iter().any(|&it| it > 0) {
        let live: Vec<usize> = (0..count).filter(|&it| left[it] > 0).collect();
        let request = live[d.int(0, live.len() as u64 - 1) as usize];
        let first = left[request] == requests[request].responses + 2;
        steps.push(if first { Step::Send(request) } else { Step::Emit(request) });
        left[request] -= 1;
        if d.int(0, 3) == 0 {
            steps.push(Step::Pass(d.int(1, 20)));
        }
    }
    let chunks = (0..d.int(1, 4)).map(|_| d.int(1, 9) as usize).collect();
    let cut = cuts.then(|| d.int(0, 200) as usize);
    let other_schema = schemas.then(|| {
        let request = d.int(0, count as u64 - 1) as usize;
        (request, d.int(0, requests[request].responses as u64) as usize)
    });
    Plan { requests, steps, chunks, cut, other_schema }
}

/// What a response of request `request` carries, so that it is told from every other's.
fn tag(request: usize, at: usize) -> u64 {
    (request as u64) * 100 + at as u64
}

fn asked(request: usize) -> Request {
    Request::Hot { top: request as u32, since: HotSince::Start }
}

/// The oracle's record of one request.
#[derive(Default, Debug)]
struct Told {
    id: Option<u64>,
    seen: usize,
    end: Option<End>,
}

struct World<'a> {
    plan: &'a Plan,
    mux: Mux,
    told: Vec<Told>,
    /// Every byte the daemon wrote, and how many of them the client was given.
    wire: Vec<u8>,
    delivered: usize,
    /// Every byte the client wrote.
    written: Vec<u8>,
    /// The next frame of each request's answer.
    next: Vec<usize>,
    clock: u64,
    hung_up: bool,
    turn: usize,
}

impl World<'_> {
    fn chunk(&mut self) -> usize {
        self.turn += 1;
        self.plan.chunks[self.turn % self.plan.chunks.len()]
    }

    /// The daemon's bytes not yet given that it sends before it hangs up.
    fn deliverable(&self) -> usize {
        self.plan.cut.map_or(self.wire.len(), |cut| cut.min(self.wire.len())) - self.delivered
    }

    fn deliver(&mut self) {
        let count = self.chunk().min(self.deliverable());
        self.mux.received(&self.wire[self.delivered..self.delivered + count]);
        self.delivered += count;
    }

    fn write(&mut self) {
        let count = self.chunk().min(self.mux.outgoing().len());
        self.written.extend_from_slice(&self.mux.outgoing()[..count]);
        self.mux.wrote(count);
    }

    /// The oracle, of each event as it comes: a response is its request's next, a request
    /// ends once, done only with its whole answer and failed only by a fault that happened.
    fn check(&mut self) -> Result<(), String> {
        while let Some(event) = self.mux.poll() {
            let (Event::Response { id, .. } | Event::Ended { id, .. }) = &event;
            let Some(request) = self.told.iter().position(|it| it.id == Some(*id)) else {
                return Err(format!("{event:?} is of an id no request was given"));
            };
            let script = &self.plan.requests[request];
            let told = &mut self.told[request];
            if let Some(end) = &told.end {
                return Err(format!("{event:?} came after request {request} ended as {end:?}"));
            }
            match event {
                Event::Response { response: Response::Flushed { seq }, .. }
                    if told.seen < script.responses && seq == tag(request, told.seen) =>
                {
                    told.seen += 1;
                }
                Event::Response { response, .. } => {
                    return Err(format!(
                        "request {request} was told {response:?} as its response {}, of {} that are its own",
                        told.seen, script.responses
                    ));
                }
                Event::Ended { how, .. } => {
                    let true_end = match &how {
                        End::Done { ok } => told.seen == script.responses && *ok == script.ok,
                        End::Failed(Fault::Schema { ours, theirs }) => {
                            self.plan.other_schema.is_some()
                                && (*ours, *theirs) == (SCHEMA, SCHEMA + 1)
                        }
                        End::Failed(Fault::HungUp { held }) => {
                            self.hung_up && *held <= self.delivered
                        }
                        End::Failed(Fault::Deadline { id: of, .. }) => {
                            Some(*of) == told.id
                                && script.deadline.is_some_and(|it| it <= self.clock)
                        }
                        End::Failed(_) => false,
                    };
                    if !true_end {
                        return Err(format!(
                            "request {request} ended as {how:?} after {} of its {} responses, at {} ms",
                            told.seen, script.responses, self.clock
                        ));
                    }
                    told.end = Some(how);
                }
            }
        }
        Ok(())
    }

    /// What the client wrote is its requests' frames, each whole and once, ids increasing, and
    /// a cancel only of a request that ended at its deadline.
    fn check_written(&self) -> Result<(), String> {
        let mut frames = frames::Frames::new();
        frames.push(&self.written);
        let mut last = 0;
        while let Some(item) = frames.next().map_err(|error| format!("{error:?}"))? {
            let envelope = frames::decode::<Request>(&item).map_err(|it| format!("{it:?}"))?;
            if envelope.id <= last {
                return Err(format!("request {} was written after request {last}", envelope.id));
            }
            last = envelope.id;
            let of = |id: u64| self.told.iter().position(|it| it.id == Some(id));
            let own = match &envelope.body {
                Request::Cancel { id } => of(*id).is_some_and(|it| {
                    matches!(self.told[it].end, Some(End::Failed(Fault::Deadline { .. })))
                }),
                body => of(envelope.id).is_some_and(|it| *body == asked(it)),
            };
            if !own {
                return Err(format!("the client wrote {envelope:?}, which no step of the plan asked"));
            }
        }
        Ok(())
    }
}

fn run(plan: &Plan, plant: Plant) -> Result<(), String> {
    let count = plan.requests.len();
    let mut world = World {
        plan,
        mux: Mux::planted(plant),
        told: (0..count).map(|_| Told::default()).collect(),
        wire: Vec::new(),
        delivered: 0,
        written: Vec::new(),
        next: vec![0; count],
        clock: 0,
        hung_up: false,
        turn: 0,
    };
    for &step in &plan.steps {
        match step {
            Step::Send(request) => {
                let deadline = plan.requests[request]
                    .deadline
                    .map_or(Deadline::NEVER, |it| Deadline::at(Duration::from_millis(it)));
                match world.mux.send(&asked(request), deadline) {
                    Ok(id) => world.told[request].id = Some(id),
                    Err(fault) if world.mux.broken() == Some(&fault) => {}
                    Err(fault) => {
                        return Err(format!("request {request} was refused with {fault:?} on a connection that takes requests"));
                    }
                }
            }
            Step::Emit(request) => {
                if let Some(id) = world.told[request].id {
                    let script = &plan.requests[request];
                    let at = world.next[request];
                    world.next[request] += 1;
                    let body = match at < script.responses {
                        true => Response::Flushed { seq: tag(request, at) },
                        false => Response::Done { ok: script.ok },
                    };
                    let schema = match plan.other_schema == Some((request, at)) {
                        true => SCHEMA + 1,
                        false => SCHEMA,
                    };
                    let frame = frames::encode(&Envelope { schema, id, body }).unwrap();
                    world.wire.extend_from_slice(&frame);
                }
            }
            Step::Pass(millis) => {
                world.clock += millis;
                world.mux.tick(Duration::from_millis(world.clock));
            }
        }
        world.deliver();
        world.write();
        world.check()?;
    }
    // Bounded: each turn gives at least one byte of what is left.
    while world.deliverable() > 0 {
        world.deliver();
        world.write();
        world.check()?;
    }
    // Bounded: each turn writes at least one byte of what is left.
    while !world.mux.outgoing().is_empty() {
        world.write();
    }
    world.hung_up = true;
    world.mux.hung_up();
    world.check()?;
    for (request, told) in world.told.iter().enumerate() {
        match (&told.id, &told.end) {
            (Some(_), None) => return Err(format!("request {request} never ended")),
            (Some(_), Some(End::Failed(fault))) if plan.faultless() => {
                return Err(format!("request {request} ended with {fault:?} in a plan with no fault"));
            }
            (None, _) if world.mux.broken().is_none() || plan.faultless() => {
                return Err(format!("request {request} was never sent"));
            }
            _ => {}
        }
    }
    world.check_written()
}

/// The invariant: whatever the daemon's bytes are cut into and however its answers interleave, every request gets exactly its own responses, in the order the daemon sent them, and then ends exactly once, with its `Done` after its whole answer or with one fault that happened (another schema, a hang-up, its deadline); and the client writes each request's frame whole, once.
#[hegel::test(test_cases = 300)]
fn every_request_gets_its_own_responses_in_order_then_one_end(tc: TestCase) {
    let plan = plan(&mut Drawn(&tc));
    if let Err(broke) = run(&plan, Plant::None) {
        panic!("{broke}\nplan {plan:?}");
    }
}

/// The plant the oracle must catch in one of [`CONTROL_SEEDS`] seeded plans: a response given
/// to the oldest request under way, not to the one its id names.
#[test]
fn the_oracle_catches_responses_matched_by_arrival() {
    for seed in 0..CONTROL_SEEDS {
        if let Err(why) = run(&plan(&mut SplitMix64::new(seed)), Plant::ByArrival) {
            eprintln!("Plant::ByArrival caught at seed {seed}: {why}");
            return;
        }
    }
    panic!(
        "the oracle caught Plant::ByArrival at none of seeds 0..{CONTROL_SEEDS}: it, or the draws, are too weak"
    );
}

/// The same seeded plans pass with nothing planted: what the control catches is the plant.
#[test]
fn the_seeded_plans_pass_with_nothing_planted() {
    for seed in 0..CONTROL_SEEDS {
        let plan = plan(&mut SplitMix64::new(seed));
        if let Err(broke) = run(&plan, Plant::None) {
            panic!("seed {seed}: {broke}\nplan {plan:?}");
        }
    }
}

fn frame(id: u64, body: Response) -> Vec<u8> {
    frames::encode(&Envelope::new(id, body)).unwrap()
}

fn events(mux: &mut Mux) -> Vec<Event> {
    std::iter::from_fn(|| mux.poll()).collect()
}

fn flushed(id: u64, seq: u64) -> Event {
    Event::Response { id, response: Response::Flushed { seq } }
}

fn done(id: u64, ok: bool) -> Event {
    Event::Ended { id, how: End::Done { ok } }
}

/// Two requests under way, their answers interleaved: (the core, the daemon's bytes, the
/// events they are).
fn two_interleaved() -> (Mux, Vec<u8>, Vec<Event>) {
    let mut mux = Mux::new();
    let a = mux.send(&Request::Status, Deadline::NEVER).unwrap();
    let b = mux.send(&Request::Memory, Deadline::NEVER).unwrap();
    assert_eq!((a, b), (1, 2));
    let bytes = [
        frame(a, Response::Flushed { seq: 1 }),
        frame(b, Response::Flushed { seq: 2 }),
        frame(b, Response::Done { ok: true }),
        frame(a, Response::Flushed { seq: 3 }),
        frame(a, Response::Done { ok: false }),
    ]
    .concat();
    let events = vec![flushed(a, 1), flushed(b, 2), done(b, true), flushed(a, 3), done(a, false)];
    (mux, bytes, events)
}

#[test]
fn two_interleaved_requests_each_get_their_own_responses() {
    let (mut mux, bytes, want) = two_interleaved();
    mux.received(&bytes);
    assert_eq!(events(&mut mux), want);
    assert_eq!(mux.open(), 0);
    assert_eq!(mux.broken(), None);
}

/// The daemon's bytes cut in two at every byte, and given a byte at a time, are the events of
/// the bytes given whole.
#[test]
fn frames_cut_at_every_byte_are_the_same_events() {
    let (_, bytes, want) = two_interleaved();
    for cut in 0..=bytes.len() {
        let (mut mux, _, _) = two_interleaved();
        mux.received(&bytes[..cut]);
        mux.received(&bytes[cut..]);
        assert_eq!(events(&mut mux), want, "cut at {cut}");
    }
    let (mut mux, _, _) = two_interleaved();
    for byte in &bytes {
        mux.received(std::slice::from_ref(byte));
    }
    assert_eq!(events(&mut mux), want);
}

#[test]
fn a_daemon_of_another_schema_ends_every_request_naming_both_schemas() {
    let mut mux = Mux::new();
    let a = mux.send(&Request::Status, Deadline::NEVER).unwrap();
    let b = mux.send(&Request::Memory, Deadline::NEVER).unwrap();
    let theirs = Envelope { schema: SCHEMA + 1, id: a, body: Response::Done { ok: true } };
    mux.received(&frames::encode(&theirs).unwrap());
    let fault = Fault::Schema { ours: SCHEMA, theirs: SCHEMA + 1 };
    let ended = |id| Event::Ended { id, how: End::Failed(fault.clone()) };
    assert_eq!(events(&mut mux), [ended(a), ended(b)]);
    assert_eq!(mux.send(&Request::Status, Deadline::NEVER), Err(fault.clone()));
    let said = fault.to_string();
    assert!(
        said.contains(&format!("schema {}", SCHEMA + 1)) && said.contains(&format!("schema {SCHEMA}")),
        "{said}"
    );
}

#[test]
fn a_hang_up_inside_a_frame_ends_each_request_once_naming_the_bytes_held() {
    let (mut mux, bytes, want) = two_interleaved();
    // The first frame whole, and three bytes of the second.
    let first = frame(1, Response::Flushed { seq: 1 }).len();
    mux.received(&bytes[..first + 3]);
    mux.hung_up();
    let fault = Fault::HungUp { held: 3 };
    let ended = |id| Event::Ended { id, how: End::Failed(fault.clone()) };
    assert_eq!(events(&mut mux), [want[0].clone(), ended(1), ended(2)]);
    mux.hung_up();
    mux.received(&bytes[first + 3..]);
    assert_eq!(events(&mut mux), []);
    assert_eq!(mux.broken(), Some(&fault));
    assert_eq!(fault.to_string(), "the daemon hung up 3 bytes into a frame");
}

#[test]
fn a_request_past_its_deadline_ends_once_and_is_cancelled() {
    let mut mux = Mux::new();
    let slow = mux.send(&Request::Status, Deadline::at(Duration::from_millis(10))).unwrap();
    let other = mux.send(&Request::Memory, Deadline::NEVER).unwrap();
    assert_eq!(mux.next_deadline(), Some(Deadline::at(Duration::from_millis(10))));
    let sent = mux.outgoing().len();
    mux.wrote(sent);
    mux.tick(Duration::from_millis(9));
    assert_eq!(events(&mut mux), []);
    mux.tick(Duration::from_millis(10));
    let fault = Fault::Deadline { id: slow, request: "Status" };
    assert_eq!(events(&mut mux), [Event::Ended { id: slow, how: End::Failed(fault) }]);
    assert_eq!(mux.next_deadline(), None);
    // The cancel is the one frame to write, under an id of its own.
    let mut written = frames::Frames::new();
    written.push(mux.outgoing());
    let cancel = frames::decode::<Request>(&written.next().unwrap().unwrap()).unwrap();
    assert_eq!(cancel, Envelope::new(3, Request::Cancel { id: slow }));
    // What the daemon still says of the request, and of its cancel, is told to no one; the
    // other request goes on.
    mux.received(&frame(slow, Response::Flushed { seq: 1 }));
    mux.received(&frame(slow, Response::Done { ok: false }));
    mux.received(&frame(cancel.id, Response::Done { ok: true }));
    mux.received(&frame(other, Response::Done { ok: true }));
    assert_eq!(events(&mut mux), [done(other, true)]);
    assert_eq!(mux.broken(), None);
}

#[test]
fn a_stop_ends_once_the_daemon_hangs_up_after_its_done() {
    let mut mux = Mux::new();
    let stop = mux.send(&Request::Stop, Deadline::NEVER).unwrap();
    mux.received(&frame(stop, Response::Done { ok: true }));
    assert_eq!(events(&mut mux), []);
    let step = Response::Stopping { doing: "writing its image".to_owned() };
    mux.received(&frame(stop, step.clone()));
    assert_eq!(events(&mut mux), [Event::Response { id: stop, response: step }]);
    mux.hung_up();
    assert_eq!(events(&mut mux), [done(stop, true)]);
}

#[test]
fn a_response_to_no_request_ends_the_connection() {
    let mut mux = Mux::new();
    let a = mux.send(&Request::Status, Deadline::NEVER).unwrap();
    mux.received(&frame(9, Response::Done { ok: true }));
    let fault = Fault::Unasked { id: 9 };
    assert_eq!(events(&mut mux), [Event::Ended { id: a, how: End::Failed(fault.clone()) }]);
    assert_eq!(mux.broken(), Some(&fault));
}
