//! The viewer's core in simulation: a queue for each ring, a virtual clock, viewers that come
//! and go. The five cases the floor rests on, one test each (no viewer: no timer; a viewer:
//! the timer armed; the ring advances: frames sent; the viewer leaves: the timer disarmed; a
//! directory event: the runs list read once), then the timer's rule as a property over random
//! histories.

use core::time::Duration;

use blaze_own::{AVec, Alloc};
use blaze_sim::{Config as SimConfig, Tape};

use super::*;

/// A ring as a count of events waiting: each popped as [`EVENT_BYTES`] bytes of its number.
struct Queue {
    waiting: u64,
    pushed: u64,
}

impl Ring for Queue {
    fn drain_into(&mut self, most: usize, into: &mut AVec<'_, u8>) -> usize {
        let took = usize::try_from(self.waiting).unwrap_or(usize::MAX).min(most);
        for at in 0..took {
            let number = self.pushed - self.waiting + u64::try_from(at).unwrap_or(0);
            into.extend_from_slice(&number.to_le_bytes());
            into.resize(into.len() + EVENT_BYTES - 8, 0);
        }
        self.waiting -= u64::try_from(took).unwrap_or(0);
        took
    }

    fn stats(&self) -> RingStats {
        RingStats { cap: 1024, pushed: self.pushed, popped: self.pushed - self.waiting, ..RingStats::default() }
    }
}

impl Queue {
    fn push(&mut self, events: u64) {
        self.waiting += events;
        self.pushed += events;
    }
}

const MS: Duration = Duration::from_millis(1);

fn alloc() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

fn config() -> Config {
    Config::new(Duration::from_secs(60))
}

/// A pump that has listed `run-a.telem` and attached it (process 7, `waiting` events).
fn with_run(waiting: u64) -> Pump<'static, Queue> {
    let mut pump = Pump::new(config(), Duration::ZERO, alloc());
    pump.take_actions();
    pump.listed(&[Listed { name: "run-a.telem", modified: 1 }, Listed { name: "run-a.meta", modified: 1 }]);
    let asked = pump.take_actions();
    assert!(matches!(asked.as_slice(), [Action::Attach { stem }] if stem.as_str() == "run-a"));
    let meta = RunMeta::parse(Some("pid=7\npackage=demo\nstarted_ms=5\nslot=2:b\nslot=1:a\n"), "run-a", 0, alloc());
    pump.attached("run-a", Queue { waiting, pushed: waiting }, meta);
    pump
}

fn frames(actions: &[Action<'_>]) -> usize {
    actions.iter().filter(|action| matches!(action, Action::BroadcastBinary { .. })).count()
}

fn listings(actions: &[Action<'_>]) -> usize {
    actions.iter().filter(|action| matches!(action, Action::ListDir)).count()
}

#[test]
fn with_no_viewer_there_is_no_timer() {
    let mut pump = with_run(10);
    let asked = pump.take_actions();
    assert!(asked.iter().any(|action| matches!(action, Action::WatchExit { pid: 7 })));
    assert!(!pump.ticking());
    // A live run and nobody watching: not idle, so no expiry either. No deadline at all.
    assert_eq!(pump.next_deadline(), None);
    for at in 1..100 {
        pump.advance(MS * at);
    }
    assert_eq!(frames(&pump.take_actions()), 0, "nobody watches: nothing is drained or sent");
}

#[test]
fn a_viewer_arms_the_timer_and_is_sent_the_snapshots() {
    let mut pump = with_run(10);
    pump.take_actions();
    pump.advance(MS * 3);
    pump.viewer_joined(ConnId(1));
    assert!(pump.ticking());
    assert_eq!(pump.next_deadline(), Some(MS * 3), "armed at once: the first drain is now");
    let asked = pump.take_actions();
    let texts: AVec<'_, &str> = {
        let mut texts = AVec::new_in(alloc());
        for action in asked.iter() {
            if let Action::SendText { to: ConnId(1), text } = action {
                texts.push(text.as_str());
            }
        }
        texts
    };
    assert_eq!(
        texts.as_slice(),
        [
            r#"{"type":"runs","runs":[{"id":"run-a","pid":7,"package":"demo","alive":true,"started_ms":5,"stats":{"cap":1024,"pushed":10,"popped":10,"dropped":0,"shadow_overflows":0,"unwound_spans":0},"slots":{"1":"a","2":"b"}}]}"#,
            r#"{"type":"builds","builds":[]}"#,
        ]
    );
}

#[test]
fn the_ring_advancing_sends_one_frame_per_tick() {
    let mut pump = with_run(10);
    pump.viewer_joined(ConnId(1));
    pump.take_actions();
    // The backlog went when the first viewer came: the view starts live.
    pump.advance(Duration::ZERO);
    assert_eq!(frames(&pump.take_actions()), 0);
    assert_eq!(pump.next_deadline(), Some(TICK));
    let Some(run) = pump.runs.get_mut(0) else { panic!("run-a is tracked") };
    run.ring.push(3);
    pump.advance(TICK);
    let asked = pump.take_actions();
    let [Action::BroadcastBinary { bytes }] = asked.as_slice() else { panic!("one frame") };
    // `B`, version 1, id length 5, "run-a", padded to 16, then three events.
    assert_eq!(bytes.get(..16), Some(&b"B\x01\x05\x00run-a\x00\x00\x00\x00\x00\x00\x00"[..]));
    assert_eq!(bytes.len(), 16 + 3 * EVENT_BYTES);
    assert_eq!(bytes.get(16..24), Some(&10u64.to_le_bytes()[..]), "the first event after the backlog");
    assert_eq!(pump.next_deadline(), Some(TICK * 2));
    // A full drain ticks again at once.
    if let Some(run) = pump.runs.get_mut(0) {
        run.ring.push(300);
    }
    pump.advance(TICK * 2);
    assert_eq!(frames(&pump.take_actions()), 1);
    assert_eq!(pump.next_deadline(), Some(TICK * 2));
    pump.advance(TICK * 2);
    assert_eq!(frames(&pump.take_actions()), 1);
    assert_eq!(pump.next_deadline(), Some(TICK * 3));
}

#[test]
fn the_last_viewer_leaving_disarms_the_timer() {
    let mut pump = with_run(0);
    pump.viewer_joined(ConnId(1));
    pump.viewer_joined(ConnId(2));
    pump.viewer_left(ConnId(1));
    assert!(pump.ticking(), "one viewer is still there");
    pump.viewer_left(ConnId(2));
    assert!(!pump.ticking());
    assert_eq!(pump.next_deadline(), None);
    pump.take_actions();
    if let Some(run) = pump.runs.get_mut(0) {
        run.ring.push(5);
    }
    pump.advance(TICK * 10);
    assert_eq!(frames(&pump.take_actions()), 0);
}

#[test]
fn directory_events_read_the_runs_list_once() {
    let mut pump: Pump<'_, Queue> = Pump::new(config(), Duration::ZERO, alloc());
    assert_eq!(listings(&pump.take_actions()), 1, "the first listing");
    // Three changes while that listing is out: one more listing after it, not three.
    pump.dir_changed();
    pump.dir_changed();
    pump.dir_changed();
    assert_eq!(listings(&pump.take_actions()), 0);
    pump.listed(&[]);
    assert_eq!(listings(&pump.take_actions()), 1);
    pump.listed(&[Listed { name: "b.build.json", modified: 9 }]);
    let asked = pump.take_actions();
    assert_eq!(listings(&asked), 0, "nothing changed since: no listing");
    assert!(matches!(asked.as_slice(), [Action::ReadBuild { name, modified: 9 }] if name.as_str() == "b.build.json"));
    pump.build_read("b.build.json", 9, br#"{"id":"b"}"#);
    // The same listing again reads nothing; a newer build is read.
    pump.dir_changed();
    pump.take_actions();
    pump.listed(&[Listed { name: "b.build.json", modified: 9 }]);
    assert_eq!(pump.take_actions().len(), 0);
    assert_eq!(pump.builds_json().as_str(), r#"{"type":"builds","builds":[{"id":"b"}]}"#);
}

#[test]
fn an_exit_ends_the_run_and_idle_stops_the_viewer() {
    let mut pump = with_run(0);
    pump.viewer_joined(ConnId(1));
    pump.exited(7);
    pump.take_actions();
    pump.advance(MS);
    let asked = pump.take_actions();
    assert!(asked.iter().any(|action| matches!(action, Action::BroadcastText { text } if text.as_str() == r#"{"type":"run_ended","id":"run-a","status":0}"#)));
    assert!(!pump.ticking(), "an ended run holds no events");
    pump.viewer_left(ConnId(1));
    // Idle: the collection 15 s after the exit, the expiry 60 s after the viewer left, nothing
    // between.
    assert_eq!(pump.next_deadline(), Some(Duration::from_secs(15)));
    pump.advance(Duration::from_secs(15));
    assert!(pump.take_actions().iter().any(|action| matches!(action, Action::Remove { stem } if stem.as_str() == "run-a")));
    assert_eq!(pump.next_deadline(), Some(MS + Duration::from_secs(60)));
    pump.advance(MS + Duration::from_secs(60));
    assert!(pump.take_actions().iter().any(|action| matches!(action, Action::Stop)));
}

#[test]
fn a_ring_that_does_not_attach_is_retried_then_reported() {
    let mut pump: Pump<'_, Queue> = Pump::new(config(), Duration::ZERO, alloc());
    pump.take_actions();
    pump.listed(&[Listed { name: "x.telem", modified: 1 }]);
    pump.take_actions();
    let mut now = Duration::ZERO;
    for _ in 0..24 {
        pump.attach_failed("x", "too small");
        now = pump.next_deadline().unwrap_or(now);
        pump.advance(now);
        assert!(matches!(pump.take_actions().as_slice(), [Action::Attach { .. }]));
    }
    pump.attach_failed("x", "too small");
    assert!(matches!(
        pump.take_actions().as_slice(),
        [Action::Report { error: PumpError::AttachRefused { tries: 25, .. } }]
    ));
}

/// The rule of the one timer over random histories: armed exactly while a viewer is connected
/// and a run is not ended; nothing sent to viewers while there are none; no deadline before
/// the clock.
fn history(tape: &Tape) {
    let mut pump: Pump<'_, Queue> = Pump::new(config(), Duration::ZERO, alloc());
    let mut now = Duration::ZERO;
    let mut next_conn = 0u64;
    let mut conns: AVec<'_, ConnId> = AVec::new_in(alloc());
    let mut pids = 0u32;
    for _ in 0..tape.below(64, "steps") {
        match tape.below(7, "event") {
            0 => {
                next_conn += 1;
                pump.viewer_joined(ConnId(next_conn));
                conns.push(ConnId(next_conn));
            }
            1 => {
                if !conns.is_empty() {
                    let at = usize::try_from(tape.below(u64::try_from(conns.len()).unwrap_or(1), "who")).unwrap_or(0);
                    let conn = conns.remove_swap(at.min(conns.len() - 1));
                    pump.viewer_left(conn);
                }
            }
            2 => {
                pids += 1;
                let mut name = blaze_own::AString::new_in(alloc());
                let _ = core::fmt::Write::write_fmt(&mut name, format_args!("r{pids}"));
                let mut text = blaze_own::AString::new_in(alloc());
                let _ = core::fmt::Write::write_fmt(&mut text, format_args!("pid={pids}\n"));
                let meta = RunMeta::parse(Some(text.as_str()), name.as_str(), 0, alloc());
                pump.attached(name.as_str(), Queue { waiting: 0, pushed: 0 }, meta);
            }
            3 => {
                if pids > 0 {
                    pump.exited(u32::try_from(tape.below(u64::from(pids), "pid")).unwrap_or(0) + 1);
                }
            }
            4 => {
                let events = tape.below(600, "events");
                for run in pump.runs.iter_mut() {
                    run.ring.push(events);
                }
            }
            5 => pump.dir_changed(),
            _ => {
                now += MS * u32::try_from(tape.below(40, "ms")).unwrap_or(0);
                pump.advance(now);
            }
        }
        let watched = pump.viewers() > 0 && pump.runs.iter().any(|run| !run.ended);
        assert_eq!(pump.ticking(), watched, "the timer is armed exactly while a viewer watches a run");
        if let Some(at) = pump.next_deadline() {
            assert!(at >= now, "no deadline is before the clock");
        }
        let asked = pump.take_actions();
        if pump.viewers() == 0 {
            assert_eq!(frames(&asked), 0, "no frame with no viewer");
        }
    }
}

#[test]
fn the_timer_is_armed_exactly_while_a_viewer_watches() {
    blaze_sim::check(SimConfig::cases(512), history);
}
