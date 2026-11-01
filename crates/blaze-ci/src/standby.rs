//! The standby (docs/v2/plan/lanes/ci.md section 10, "a standby resumes from the newest snapshot"): on
//! another machine, `blaze-ci standby` asks the primary's daemon for its status
//! ([`blaze_api::Request::Status`] over HTTP) every [`StandbyConfig::every_secs`]. After
//! [`StandbyConfig::misses`] failures in a row it finds the newest snapshot (`latest`),
//! restores it (`restore`) and makes it the primary (`promote`), then exits 0; the restored VM
//! starts its own `blaze-ci serve` at boot. A failure of any of the three exits 1 naming it.
//!
//! [`Standby`] decides, with no IO and no clock; [`run`] does what it says.

use std::{
    net::SocketAddr,
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use blaze_api::{Request, Response, client::Client};

use crate::{
    config::{ConfigError, Secret, StandbyConfig},
    snapshot::{self, SnapshotError},
};

/// What happened, for [`Standby::step`].
#[derive(PartialEq, Eq, Debug)]
pub enum Input {
    Started,
    /// A poll ended: `Ok` when the primary answered, else why not.
    Polled(Result<(), String>),
    Latest(Result<String, String>),
    Restored(Result<(), String>),
    Promoted(Result<(), String>),
}

/// What to do next.
#[derive(PartialEq, Eq, Debug)]
pub enum Action {
    /// Ask the primary once the clock reads `at`.
    Poll { at: u64 },
    Latest,
    Restore { id: String },
    Promote,
    /// Stop with `code`, saying `why`.
    Exit { code: i32, why: String },
}

#[derive(PartialEq, Eq, Debug)]
pub struct Standby {
    every_secs: u64,
    misses_allowed: u32,
    misses: u32,
    last_miss: Option<String>,
    over: bool,
}

impl Standby {
    pub fn new(every_secs: u64, misses_allowed: u32) -> Standby {
        Standby { every_secs, misses_allowed, misses: 0, last_miss: None, over: false }
    }

    /// The failed polls in a row so far.
    pub fn misses(&self) -> u32 {
        self.misses
    }

    /// What `input`, at `now`, calls for.
    pub fn step(&mut self, now: u64, input: Input) -> Action {
        if self.over {
            // alloc: error path
            return Action::Exit { code: 1, why: "the standby already stopped".to_owned() };
        }
        let next = Action::Poll { at: now.saturating_add(self.every_secs) };
        match input {
            Input::Started => Action::Poll { at: now },
            Input::Polled(Ok(())) => {
                self.misses = 0;
                self.last_miss = None;
                next
            }
            Input::Polled(Err(why)) => {
                self.misses += 1;
                self.last_miss = Some(why);
                if self.misses >= self.misses_allowed { Action::Latest } else { next }
            }
            Input::Latest(Ok(id)) => Action::Restore { id },
            // alloc: error path
            Input::Latest(Err(why)) => self.stop(1, format!("finding the newest snapshot failed: {why}")),
            Input::Restored(Ok(())) => Action::Promote,
            // alloc: error path
            Input::Restored(Err(why)) => self.stop(1, format!("restoring the snapshot failed: {why}")),
            Input::Promoted(Ok(())) => {
                // alloc: once, at the takeover
                let last = self.last_miss.clone().unwrap_or_default();
                self.stop(
                    0,
                    format!( // alloc: once, at the takeover
                        "the primary missed {} polls in a row (last: {last}); the newest snapshot \
                         is restored and promoted",
                        self.misses
                    ),
                )
            }
            // alloc: error path
            Input::Promoted(Err(why)) => self.stop(1, format!("promoting the restored VM failed: {why}")),
        }
    }

    fn stop(&mut self, code: i32, why: String) -> Action {
        self.over = true;
        Action::Exit { code, why }
    }
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |it| it.as_secs())
}

/// One poll: the primary's daemon answers [`Request::Status`] with `ok` within `within`. A
/// poll that hangs is a miss; its thread is left to end on its own.
fn poll(primary: SocketAddr, token: &str, within: Duration) -> Result<(), String> {
    let (done, wait) = crossbeam_channel::unbounded();
    // alloc: once per poll, every few seconds, the token its thread owns
    let token = token.to_owned();
    thread::spawn(move || {
        let result = Client::http(primary, token)
            .and_then(|mut client| {
                let mut status = false;
                let ok = client.call(Request::Status, |response| {
                    status |= matches!(response, Response::Status(_));
                })?;
                Ok((ok, status))
            })
            .map_err(|error| format!("{primary}: {error}")) // alloc: error path
            .and_then(|(ok, status)| match (ok, status) {
                (true, true) => Ok(()),
                // alloc: error path
                (true, false) => Err(format!("{primary} ended its status without one")),
                // alloc: error path
                (false, _) => Err(format!("{primary} answered its status with an error")),
            });
        // The receiver is gone when the poll took too long; nothing waits for it.
        let _ = done.send(result);
    });
    match wait.recv_timeout(within) {
        Ok(result) => result,
        // alloc: error path
        Err(_) => Err(format!("{primary} did not answer within {} s", within.as_secs())),
    }
}

/// The standby's loop; returns the exit code and what it says.
pub fn run(config: &StandbyConfig) -> Result<(i32, String), ConfigError> {
    let token = Secret::read(&config.token_file)?.text(&config.token_file)?;
    let within = Duration::from_secs(config.every_secs.max(1));
    let mut standby = Standby::new(config.every_secs, config.misses);
    let mut action = standby.step(now(), Input::Started);
    loop {
        let input = match action {
            Action::Poll { at } => {
                let wait = at.saturating_sub(now());
                if wait > 0 {
                    crate::hold(Duration::from_secs(wait));
                }
                let polled = poll(config.primary, &token, within);
                if let Err(why) = &polled {
                    log_miss(standby.misses() + 1, config.misses, why);
                }
                Input::Polled(polled)
            }
            Action::Latest => Input::Latest(snapshot::run_for_id(&config.latest).map_err(said)),
            Action::Restore { id } => Input::Restored(
                snapshot::run(&snapshot::substitute(&config.restore, "id", &id)).map(drop).map_err(said),
            ),
            Action::Promote => Input::Promoted(snapshot::run(&config.promote).map(drop).map_err(said)),
            Action::Exit { code, why } => return Ok((code, why)),
        };
        action = standby.step(now(), input);
    }
}

fn said(error: SnapshotError) -> String {
    error.what
}

#[allow(clippy::print_stderr)]
fn log_miss(miss: u32, of: u32, why: &str) {
    eprintln!("blaze-ci standby: miss {miss} of {of}: {why}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_primary_that_answers_is_polled_every_interval() {
        let mut standby = Standby::new(10, 3);
        assert_eq!(standby.step(100, Input::Started), Action::Poll { at: 100 });
        for at in [100, 110, 120, 130] {
            assert_eq!(standby.step(at, Input::Polled(Ok(()))), Action::Poll { at: at + 10 });
        }
    }

    #[test]
    fn misses_in_a_row_take_over_and_an_answer_resets_them() {
        let mut standby = Standby::new(10, 3);
        standby.step(0, Input::Started);
        let miss = || Input::Polled(Err("refused".to_owned()));
        assert_eq!(standby.step(0, miss()), Action::Poll { at: 10 });
        assert_eq!(standby.step(10, miss()), Action::Poll { at: 20 });
        assert_eq!(standby.step(20, Input::Polled(Ok(()))), Action::Poll { at: 30 });
        assert_eq!(standby.misses(), 0);
        assert_eq!(standby.step(30, miss()), Action::Poll { at: 40 });
        assert_eq!(standby.step(40, miss()), Action::Poll { at: 50 });
        assert_eq!(standby.step(50, miss()), Action::Latest);
        assert_eq!(
            standby.step(51, Input::Latest(Ok("snap-9".to_owned()))),
            Action::Restore { id: "snap-9".to_owned() }
        );
        assert_eq!(standby.step(52, Input::Restored(Ok(()))), Action::Promote);
        let Action::Exit { code, why } = standby.step(53, Input::Promoted(Ok(()))) else {
            panic!("no exit")
        };
        assert_eq!(code, 0);
        assert!(why.contains("3 polls") && why.contains("refused"), "{why}");
    }

    #[test]
    fn each_failed_step_of_the_takeover_exits_1_naming_it() {
        let cases = [
            (vec![], Input::Latest(Err("no snapshots".to_owned())), "newest snapshot"),
            (vec![Input::Latest(Ok("s".to_owned()))], Input::Restored(Err("disk".to_owned())), "restoring"),
            (
                vec![Input::Latest(Ok("s".to_owned())), Input::Restored(Ok(()))],
                Input::Promoted(Err("address".to_owned())),
                "promoting",
            ),
        ];
        for (before, failing, named) in cases {
            let mut standby = Standby::new(5, 1);
            standby.step(0, Input::Started);
            assert_eq!(standby.step(0, Input::Polled(Err("down".to_owned()))), Action::Latest);
            for input in before {
                standby.step(1, input);
            }
            let Action::Exit { code, why } = standby.step(2, failing) else { panic!("no exit") };
            assert_eq!(code, 1);
            assert!(why.contains(named), "{why}");
            assert!(matches!(standby.step(3, Input::Started), Action::Exit { code: 1, .. }));
        }
    }
}
