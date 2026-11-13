//! What is built ahead (`serve::ahead`), on the simulation's daemon: the real loop, session
//! and jobs, the fakes' front end. The core's own rules are held by its schedule test
//! (`serve/ahead/tests.rs`); here, that the loop carries them out and no answer changes.

use super::*;

fn check() -> Request {
    Request::Check { view: ViewRef::Head("/w".into()), targets: agent() }
}

fn build() -> Request {
    Request::Build { view: ViewRef::Head("/w".into()), targets: agent() }
}

/// A millisecond passes and the daemon turns, `turns` times: the simulation's clock moves
/// only when told to (a poll with nothing in flight leaves it), and a save is built ahead
/// once its head has stood still for `ahead::QUIET`.
fn pass(daemon: &mut SimDaemon, turns: u64) {
    for _ in 0..turns {
        daemon.driver().advance(std::time::Duration::from_millis(1));
        daemon.turn();
    }
}

/// Lets time pass until `done` says so: with no request sent, what moves is what the daemon
/// does of its own accord.
fn turn_until(daemon: &mut SimDaemon, what: &str, mut done: impl FnMut(&mut SimDaemon) -> bool) {
    for _ in 0..TURNS {
        if done(daemon) {
            return;
        }
        pass(daemon, 1);
    }
    panic!("the daemon, left alone, never got to: {what} ({:?})", daemon.built_ahead());
}

fn phases() -> usize {
    PHASES.with(|it| it.borrow().len())
}

/// A save with no request after it is built: the fakes' front end compiles the checkout as
/// saved before any client asks, nothing of it is sent to a client, and the request that
/// follows is answered of the save. With building ahead off (the control), the same save
/// compiles nothing until the request.
#[test]
fn a_save_is_built_before_it_is_asked_for() {
    for on in [true, false] {
        let mut sim = Sim::new(71, Faults::default());
        checkout(&mut sim, "first\n");
        let steps = Arc::new(Fake::default());
        let mut daemon = start(sim, &steps, 71);
        daemon.build_ahead(on);
        let mut client = dial(&mut daemon);
        let first = client.send(&mut daemon, check());
        assert_eq!(warnings(&client.wait(&mut daemon, first)), vec!["first\n".to_owned()]);
        // Left alone after the request: at most the walk that leaves an answer to say again.
        pass(&mut daemon, 2_000);
        let before = (phases(), daemon.built_ahead());
        daemon.driver().change(&name("w/Cargo.toml"), b"saved\n");
        if on {
            turn_until(&mut daemon, "a build ahead of the save ended", |daemon| {
                daemon.built_ahead().ended > before.1.ended
            });
            assert!(phases() > before.0, "the save was compiled with no request asking");
        } else {
            pass(&mut daemon, 2_000);
            assert_eq!(phases(), before.0, "with building ahead off, a save compiles nothing");
            assert_eq!(daemon.built_ahead(), super::super::ahead::Counts::default());
        }
        client.take(&mut daemon);
        assert!(
            client.got.iter().all(|(id, _)| *id == first),
            "what a build ahead says is sent to no client: {:#?}",
            client.got
        );
        let id = client.send(&mut daemon, check());
        let got = client.wait(&mut daemon, id);
        assert_eq!(warnings(&got), vec!["saved\n".to_owned()], "{got:#?}");
        assert_eq!(got.last(), Some(&Response::Done { ok: true }));
    }
}

/// The invariant: whatever the daemon is building ahead as a request is read, the request
/// is answered once, of the checkout as saved before it was sent, as a daemon that builds
/// nothing ahead answers it. Over seeds: completions late by up to 3 ms and shuffled, a save
/// and then a request of the same shape or another after 0 to 60 turns, so the request is
/// read before the build ahead is sent, while it runs, as it is cancelled, and after it.
#[test]
fn a_request_is_answered_of_the_save_whatever_is_built_ahead() {
    for seed in 0..40 {
        let mut rng = Rng::new(seed);
        let faults = Faults {
            max_latency: std::time::Duration::from_millis(rng.below(4)),
            ..Faults::default()
        };
        let mut sim = Sim::new(seed, faults);
        checkout(&mut sim, "first\n");
        let steps = Arc::new(Fake::default());
        let mut daemon = start(sim, &steps, seed);
        daemon.build_ahead(true);
        let mut client = dial(&mut daemon);
        let first = client.send(&mut daemon, check());
        assert_eq!(warnings(&client.wait(&mut daemon, first)), vec!["first\n".to_owned()]);
        let mut ids = vec![first];
        for round in 0..10 {
            let saved = format!("seed {seed} round {round}\n");
            daemon.driver().change(&name("w/Cargo.toml"), saved.as_bytes());
            pass(&mut daemon, rng.below(61));
            let request = if rng.below(4) == 0 { build() } else { check() };
            let id = client.send(&mut daemon, request);
            ids.push(id);
            let got = client.wait(&mut daemon, id);
            assert_eq!(got.last(), Some(&Response::Done { ok: true }), "seed {seed}: {got:#?}");
            assert_eq!(warnings(&got), vec![saved], "seed {seed} round {round}: {got:#?}");
        }
        client.take(&mut daemon);
        keeps_the_contract(&client.got, &ids);
    }
}

/// A request for what is being built ahead, read while that build runs: how many requests
/// the core held, or what the oracle of the invariant above caught (an answer that is not
/// the save's).
fn asked_while_built_ahead(seed: u64, plant: Option<super::super::ahead::Plant>) -> Result<u64, String> {
    super::super::ahead::PLANTED.set(plant);
    let faults = Faults { max_latency: std::time::Duration::from_millis(seed % 4), ..Faults::default() };
    let mut sim = Sim::new(seed, faults);
    checkout(&mut sim, "first\n");
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, seed);
    daemon.build_ahead(true);
    let mut client = dial(&mut daemon);
    let first = client.send(&mut daemon, check());
    client.wait(&mut daemon, first);
    pass(&mut daemon, 2_000);
    let before = daemon.built_ahead();
    daemon.driver().change(&name("w/Cargo.toml"), b"saved\n");
    turn_until(&mut daemon, "a build ahead of the save sent", |daemon| {
        daemon.built_ahead().started > before.started
    });
    let id = client.send(&mut daemon, check());
    let got = client.wait(&mut daemon, id);
    let held = daemon.built_ahead().held;
    super::super::ahead::PLANTED.set(None);
    if warnings(&got) != vec!["saved\n".to_owned()] {
        return Err(format!("the request after the save was answered {got:?}"));
    }
    Ok(held)
}

/// A request for what is being built ahead waits for that build and is then answered by its
/// own job, of the save; and the control of the invariant above: a core that hands the
/// request the build ahead's end (`Plant::OldAnswer`) is caught by the same oracle, in each
/// schedule where the request was held.
#[test]
fn the_oracle_catches_a_request_answered_with_the_build_aheads_end() {
    let (mut held, mut caught) = (0, 0);
    for seed in 0..24 {
        let holds = asked_while_built_ahead(seed, None).unwrap_or_else(|why| panic!("seed {seed}: {why}"));
        if holds == 0 {
            continue;
        }
        held += 1;
        if asked_while_built_ahead(seed, Some(super::super::ahead::Plant::OldAnswer)).is_err() {
            caught += 1;
        }
    }
    assert!(held > 0, "no schedule of 24 held the request behind its build ahead");
    assert_eq!(caught, held, "the planted bug was missed where the request was held");
}
