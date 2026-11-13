//! The daemon that exits once no connection holds it ([`crate::serve::unheld`]), the whole
//! daemon on the deterministic simulation: clients connect at its socket, send a request or
//! half of one, and are cut off abruptly (the peer's end closed, as the kernel closes it for a
//! client killed with `kill -9` or by nextest's time cap), at random moments around its grace.
//!
//! The invariant (checked by [`broken`] at every turn, under many seeds): the daemon never
//! stops while a connection is open; once the last one closes it stops no sooner than its grace
//! and within one turn of it; an open inside the grace cancels the stop; and the stop is its own
//! (`Daemon::ended_unheld`), a whole one (the loop ends).

use std::{sync::Arc, time::Duration};

use blaze_api::{Envelope, Request, SCHEMA, frames};
use blaze_core::{
    Address, Conn, Driver as _,
    sim::{Faults, Sim},
};

use super::{Fake, SOCKET, SimDaemon, TURNS, name, options};
use crate::serve::{
    Daemon, Options,
    plane::{Inline, Runner},
    unheld::Plant,
};

/// The daemon's grace in the simulation: short, so a run is a few hundred turns.
const GRACE: Duration = Duration::from_secs(2);

/// How long past its grace a stop may come: the loop's idle tick (it also wakes at the grace's
/// own deadline), and the simulation's latency of the close that started it.
const TURN: Duration = Duration::from_millis(51);

/// Around the grace's end, a dial is not made: a connection the kernel queued in the backlog
/// while the daemon decides to stop is hung up on, which no daemon can tell from one it never
/// had. Wider than the simulation's latency of an accept.
const BACKLOG: Duration = Duration::from_millis(2);

/// How far the clock moves in a turn that did not move it: the simulation's poll returns at
/// once when nothing is pending (a daemon idle between connections waits on no completion),
/// where a real driver's wait blocks to its deadline, so the test lets that time pass itself.
/// Far below [`TURN`], so a stop is still seen within one turn of the grace's end.
const STEP: Duration = Duration::from_millis(1);

/// One turn of the daemon, the clock moved [`STEP`] when the turn did not move it: `false`
/// once its stop ended (`Daemon::turn`).
fn turn(daemon: &mut SimDaemon) -> bool {
    let before = daemon.driver().now();
    let more = daemon.turn();
    if daemon.driver().now() == before {
        daemon.driver().advance(STEP);
    }
    more
}

/// What the peers did, as the oracle keeps it.
struct Model {
    /// The clients connected, not yet cut off.
    open: Vec<Conn>,
    /// Since when no client is connected: the start, or the last close.
    quiet_since: Option<Duration>,
    next_id: u64,
}

/// What the oracle finds wrong at `now`, if anything: `stopped` whether the daemon began its
/// stop by this turn.
fn broken(model: &Model, now: Duration, stopped: bool) -> Option<String> {
    match (stopped, model.quiet_since) {
        (true, None) => Some(format!("it stopped at {now:?} with {} connections open", model.open.len())),
        (true, Some(since)) if now < since + GRACE => Some(format!(
            "it stopped at {now:?}, {:?} after the last close at {since:?}: before its grace of {GRACE:?}",
            now - since
        )),
        (false, Some(since)) if now > since + GRACE + TURN => Some(format!(
            "it had not stopped at {now:?}, {:?} after the last close at {since:?}: past its grace of {GRACE:?} and a turn",
            now - since
        )),
        _ => None,
    }
}

fn start(seed: u64, plant: Plant) -> SimDaemon {
    crate::release::for_tests();
    let options = Options { unheld: Some(GRACE), ..options() };
    let steps = Arc::new(Fake::default());
    let mut daemon =
        Daemon::start(Sim::new(seed, Faults::default()), options, steps, Runner::Inline(Inline::new(seed)))
            .unwrap_or_else(|why| panic!("{why}"));
    daemon.plant_unheld(plant);
    daemon
}

/// Turns the daemon until the clock reaches `until` or it begins its stop, checking the
/// oracle after each turn. `Err` with what broke; `Ok(true)` once it stops.
fn turn_to(daemon: &mut SimDaemon, model: &Model, until: Duration) -> Result<bool, String> {
    for _ in 0..TURNS {
        let now = daemon.driver().now();
        let stopped = daemon.stopping();
        if let Some(why) = broken(model, now, stopped) {
            return Err(why);
        }
        if stopped {
            return Ok(true);
        }
        if now >= until {
            return Ok(false);
        }
        turn(daemon);
    }
    Err(format!("the clock never reached {until:?} in {TURNS} turns"))
}

/// One run under `seed`: what broke, if anything.
fn run(seed: u64, plant: Plant) -> Option<String> {
    let mut daemon = start(seed, plant);
    let now = daemon.driver().now();
    let mut model = Model { open: Vec::new(), quiet_since: Some(now), next_id: 1 };
    let gaps = [Duration::from_millis(10), GRACE.mul_f64(0.45), GRACE.mul_f64(0.9), GRACE.mul_f64(1.5)];
    let events = 1 + daemon.driver().rng().below(24);
    let mut stopped = false;
    for _ in 0..events {
        let now = daemon.driver().now();
        let near_the_end = model.quiet_since.is_some_and(|since| {
            let end = since + GRACE;
            now + BACKLOG >= end && now <= end + TURN + BACKLOG
        });
        let open = model.open.len();
        match daemon.driver().rng().below(4) {
            // A client connects.
            0 | 1 if open < 4 && !near_the_end => {
                match daemon.driver().dial(&Address::Unix(name(SOCKET))) {
                    Ok(conn) => {
                        model.open.push(conn);
                        model.quiet_since = None;
                    }
                    Err(error) => return Some(format!("a client could not connect at {now:?}: {error:?}")),
                }
            }
            // One asks for the daemon's status, whole, or half of it before it is killed.
            2 if open > 0 => {
                let at = daemon.driver().rng().index(open);
                let conn = model.open[at];
                let id = model.next_id;
                model.next_id += 1;
                let bytes = frames::encode(&Envelope { schema: SCHEMA, id, body: Request::Status }).unwrap();
                let half = daemon.driver().rng().one_in(2);
                let sent = if half { &bytes[..bytes.len() / 2] } else { &bytes[..] };
                daemon.driver().peer_send(conn, sent);
                if half {
                    daemon.driver().peer_hangup(conn);
                    model.open.swap_remove(at);
                }
            }
            // One is cut off: its end closed, as the kernel closes a killed client's.
            3 if open > 0 => {
                let at = daemon.driver().rng().index(open);
                daemon.driver().peer_hangup(model.open.swap_remove(at));
            }
            _ => {}
        }
        if open > 0 && model.open.is_empty() {
            model.quiet_since = Some(now);
        }
        let gap = gaps[daemon.driver().rng().index(gaps.len())];
        match turn_to(&mut daemon, &model, now + gap) {
            Ok(true) => {
                stopped = true;
                break;
            }
            Ok(false) => {}
            Err(why) => return Some(why),
        }
    }
    if !stopped {
        // The run's end: every client cut off, and the daemon left to its grace.
        let now = daemon.driver().now();
        for conn in model.open.drain(..) {
            daemon.driver().peer_hangup(conn);
        }
        if model.quiet_since.is_none() {
            model.quiet_since = Some(now);
        }
        match turn_to(&mut daemon, &model, now + GRACE + TURN * 2) {
            Ok(true) => {}
            Ok(false) => return Some(format!("it never stopped, its last client gone at {now:?}")),
            Err(why) => return Some(why),
        }
    }
    if !daemon.ended_unheld() {
        return Some("it stopped, and not because nothing held it".to_owned());
    }
    for _ in 0..TURNS {
        if !turn(&mut daemon) {
            return None;
        }
    }
    Some(format!("its stop did not end in {TURNS} turns"))
}

/// The invariant (the module's), under 64 seeds. Controls: a daemon that stops at the first
/// look with nothing open, with no grace, and one whose countdown an open does not end, are
/// each caught under some seed.
#[test]
fn a_daemon_nothing_holds_stops_after_its_grace_and_never_while_a_connection_is_open() {
    for seed in 0..64 {
        if let Some(why) = run(seed, Plant::None) {
            panic!("seed {seed}: {why}");
        }
    }
    for plant in [Plant::ExitsOnClose, Plant::OpenKeepsCountdown] {
        let caught = (0..64).any(|seed| run(seed, plant).is_some());
        assert!(caught, "the control {plant:?} is not caught");
    }
}
