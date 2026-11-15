//! The life of an engine served in a command's own process (`serve::once`, `[serve] host =
//! "process"`), on the simulation's daemon: started with no socket ([`Options::socket`]
//! `None`), its one client on a connection of its own ([`Daemon::connect_local`],
//! [`Daemon::send_local`], [`Daemon::take_local`], the in-process transport `once` drives), one
//! build, then the stop `once` asks as the client closes, turned until it is over, then the
//! process's end, which drops nothing (`serve::Running::exit`). The flush order is the
//! daemon's own stop's (settle, members' pages, image, the stores' and the journal's flushes,
//! then nothing in flight); this proves the in-process life leans on it correctly.
//!
//! `Sim::crash` keeps a seeded prefix of each file's unsynced writes, the store's, the image's
//! and the journal's apart.
//!
//! The invariant (under several seeds, a crash after every turn of the life, the stop's
//! included): a start on what the disk kept builds byte for byte what a fresh daemon builds, so
//! a crash before the flush loses only this run's work and corrupts nothing; and a crash once
//! the life is over (where the process may exit) loses nothing: the next start opens the
//! stop's image whole and the store holds all the build left.
//!
//! The control: a planted exit that ends the life as the answer comes, with no stop (what an
//! exit that skipped the flush would be), is caught by the oracle.

use super::*;

/// How a planted exit ends the life ([`Life::step`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Exit {
    /// As `once` does: the stop, turned until it is over.
    Stops,
    /// The planted bug: over as the answer comes, nothing stopped or flushed.
    AtTheAnswer,
}

/// The in-process client: its connection on the daemon's own loop, and what came back.
struct Here {
    conn: Conn,
    frames: Frames,
    got: Vec<(u64, Response)>,
}

impl Here {
    fn take(&mut self, daemon: &mut SimDaemon) {
        let mut bytes = Vec::new();
        daemon.take_local(self.conn, &mut bytes);
        self.frames.push(&bytes);
        while let Some(item) = self.frames.next().unwrap() {
            let envelope: Envelope<Response> = frames::decode(&item).unwrap();
            self.got.push((envelope.id, envelope.body));
        }
    }

    fn done(&self, id: u64) -> bool {
        self.got.iter().any(|(at, it)| *at == id && matches!(it, Response::Done { .. }))
    }
}

/// One command's life on seed `seed`, as far as it has gone.
struct Life {
    daemon: SimDaemon,
    here: Here,
    id: u64,
    exit: Exit,
    stopped: bool,
    over: bool,
}

/// The id the build is sent under.
const BUILD_ID: u64 = 1;

impl Life {
    /// The engine started with no socket, its own connection made and the build sent on it,
    /// as `once::connect` and the CLI's first send do.
    fn begin(seed: u64, steps: &Arc<Fake>, exit: Exit) -> Life {
        let mut sim = Sim::new(seed, Faults::default());
        checkout(&mut sim, "[package]\nname = \"app\"\n");
        crate::release::for_tests();
        let options = Options { socket: None, ..options() };
        let mut daemon = Daemon::start(sim, options, steps.clone(), Runner::Inline(Inline::new(seed)))
            .unwrap_or_else(|why| panic!("{why}"));
        let conn = daemon.connect_local().expect("an engine with no socket takes a connection of its own");
        let request = Request::Build { view: ViewRef::Head("/w".into()), targets: agent() };
        let bytes = frames::encode(&Envelope { schema: SCHEMA, id: BUILD_ID, body: request }).unwrap();
        daemon.send_local(conn, &bytes);
        let here = Here { conn, frames: Frames::new(), got: Vec::new() };
        Life { daemon, here, id: BUILD_ID, exit, stopped: false, over: false }
    }

    fn answered(&self) -> bool {
        self.here.done(self.id)
    }

    /// One turn of the life: the stop is asked once the answer came, as the CLI closes its
    /// client after the answer; over once the loop says so (or, planted, at the answer).
    fn step(&mut self) {
        if self.over {
            return;
        }
        if self.answered() && !self.stopped {
            match self.exit {
                Exit::Stops => {
                    self.daemon.stop();
                    self.stopped = true;
                }
                Exit::AtTheAnswer => {
                    self.over = true;
                    return;
                }
            }
        }
        if !self.daemon.turn() {
            self.over = true;
        }
        self.here.take(&mut self.daemon);
    }
}

/// What the oracle says of a crash after `turns` turns of a life on `seed` ending as `exit`
/// says, against `fresh`, a fresh daemon's build: `Err` names what broke.
fn crashed_after(
    seed: u64,
    turns: usize,
    exit: Exit,
    fresh: &(Option<Response>, Vec<u8>, Vec<Option<Vec<u8>>>),
) -> Result<bool, String> {
    let steps = Arc::new(Fake::default());
    let mut life = Life::begin(seed, &steps, exit);
    for _ in 0..turns {
        life.step();
    }
    let over = life.over;
    if !life.daemon.lost().is_empty() {
        return Err(format!("before the crash: {:#?}", life.daemon.lost()));
    }
    let mut sim = life.daemon.into_driver();
    sim.crash();
    let mut daemon = start(sim, &steps, seed + 1000 + turns as u64);
    if over {
        let started = daemon.started();
        if started.mapped.is_none() {
            return Err(format!("the life was over, and the next start did not open its image: {started}"));
        }
        if !holds_kept(daemon.store(), "/w") {
            return Err("the life was over, and the store lost what the build left".to_owned());
        }
    }
    let mut client = dial(&mut daemon);
    let id = client.send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
    let got = client.wait(&mut daemon, id);
    settle(&mut daemon, &mut client);
    if !daemon.lost().is_empty() {
        return Err(format!("after the crash: {:#?}", daemon.lost()));
    }
    let after = outcome(&mut daemon, &got);
    if &after != fresh {
        return Err(format!("the build after the crash is not a fresh daemon's: {after:?} against {fresh:?}"));
    }
    Ok(over)
}

/// The turns a life on `seed` takes until it is over.
fn turns_of(seed: u64, exit: Exit) -> usize {
    let steps = Arc::new(Fake::default());
    let mut life = Life::begin(seed, &steps, exit);
    let mut turns = 0;
    while !life.over {
        assert!(turns < TURNS, "seed {seed}: the life never ended");
        life.step();
        turns += 1;
    }
    turns
}

/// A fresh daemon's build of `/w`, settled: what every build after a crash must equal.
fn fresh() -> (Option<Response>, Vec<u8>, Vec<Option<Vec<u8>>>) {
    let mut sim = Sim::new(140, Faults::default());
    checkout(&mut sim, "[package]\nname = \"app\"\n");
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, 140);
    let mut client = dial(&mut daemon);
    let id = client.send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
    let got = client.wait(&mut daemon, id);
    settle(&mut daemon, &mut client);
    outcome(&mut daemon, &got)
}

/// The invariant (module docs): a crash at any turn of an in-process life loses at most this
/// run's work and corrupts nothing; once the life is over it loses nothing.
#[test]
fn an_in_process_life_crashed_at_any_turn_loses_only_its_own_work_and_once_over_nothing() {
    OUTPUT.set(true);
    let fresh = fresh();
    assert_eq!(fresh.0, Some(Response::Done { ok: true }));
    let (mut before, mut after) = (0, 0);
    for seed in 300..304 {
        let turns = turns_of(seed, Exit::Stops);
        for k in 0..=turns + 1 {
            match crashed_after(seed, k, Exit::Stops, &fresh) {
                Ok(true) => after += 1,
                Ok(false) => before += 1,
                Err(why) => panic!("seed {seed}, crash after turn {k} of {turns}: {why}"),
            }
        }
    }
    assert!(before > 0 && after > 0, "the crash points never split: {before} before the end, {after} after");
    OUTPUT.set(false);
}

/// The control: an exit planted to end the life as the answer comes, flushing nothing, is
/// caught by the oracle at a crash just after it, on every seed.
#[test]
fn an_exit_that_skips_the_stop_is_caught() {
    OUTPUT.set(true);
    let fresh = fresh();
    for seed in 300..304 {
        let turns = turns_of(seed, Exit::AtTheAnswer);
        let caught = crashed_after(seed, turns, Exit::AtTheAnswer, &fresh);
        assert!(caught.is_err(), "seed {seed}: an exit with no stop passed the oracle: {caught:?}");
    }
    OUTPUT.set(false);
}
