//! `blaze audit fetch`: the advisory database brought to disk and pinned, as a sans-IO
//! machine. The driver does each [`Effect`] and hands back what came of it as an [`Event`];
//! the machine decides, so the order (fetch, hash, check against the pin, place, record) is
//! one place and runs under a test without a network.
//!
//! The steps: the repository at the revision asked is fetched into a fresh directory
//! ([`Effect::Fetch`]); its tree is hashed ([`crate::tree`]); when `blaze.lock` pins the
//! database at that same commit, the tree must be the pin, else the fetch is refused; the
//! directory is moved into place ([`Effect::Place`]); and the pin is what the driver writes
//! into `blaze.lock` ([`Effect::Record`]). The driver is not wired yet: blaze's HTTP and git
//! drivers are being replaced, and a fetch through the old ones would be a second route to
//! remove, so the command stops with [`Refused::NotWired`].

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::fmt;

use blaze_fetch::Digest;

use crate::pins::{DatabasePin, SourcePins, is_commit};

/// The database fetched when the workspace names none: RustSec's own.
pub const RUSTSEC: &str = "https://github.com/rustsec/advisory-db";

/// What the driver does next.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Effect {
    /// Fetch `url` at `rev` (a branch, a tag or a commit) into a fresh directory, and hash
    /// its tree in BLAKE3.
    Fetch { url: String, rev: String },
    /// Move the fetched directory to the database's place, replacing what was there.
    Place,
    /// Write `pin` as `[advisories]` of `blaze.lock`.
    Record(DatabasePin),
    /// Nothing more: the database is fetched and pinned.
    Done(DatabasePin),
}

/// What came of an effect.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Event {
    /// The repository was fetched at `commit`, whose tree is `tree`.
    Fetched { commit: String, tree: Digest },
    Placed,
    Recorded,
    /// The effect failed: what, in the driver's words.
    Failed(String),
}

/// Why `blaze audit fetch` stopped.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Refused {
    /// The command's driver is not wired in this blaze.
    NotWired,
    /// The repository served a tree at a commit `blaze.lock` pins that is not its pin.
    Mismatch { url: String, commit: String, recorded: Digest, found: Digest },
    /// The fetch named something that is no whole commit.
    NoCommit { url: String, commit: String },
    /// An effect failed.
    Failed { step: &'static str, why: String },
    /// An event came that the step does not take: a driver's bug.
    Unexpected { step: &'static str, event: String },
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refused::NotWired => f.write_str(
                "`blaze audit fetch` is not wired in this blaze: its fetch waits on the new HTTP and git drivers; \
                 place a clone of the database at the `database` setting's path and run `blaze audit pin`",
            ),
            Refused::Mismatch { url, commit, recorded, found } => write!(
                f,
                "the advisory database `{url}` at {commit} has the tree {found}, not the {recorded} `blaze.lock` \
                 pins (rule `{}`): the repository served another tree under that commit",
                crate::WEAK_SOURCE_PIN
            ),
            Refused::NoCommit { url, commit } => {
                write!(f, "the advisory database `{url}` was fetched at `{commit}`, which is no whole commit id")
            }
            Refused::Failed { step, why } => write!(f, "`blaze audit fetch` failed to {step}: {why}"),
            Refused::Unexpected { step, event } => {
                write!(f, "`blaze audit fetch` was told `{event}` while it was to {step}: a bug of its driver")
            }
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
enum Step {
    Start,
    Fetching,
    Placing(DatabasePin),
    Recording(DatabasePin),
    Done,
}

/// The machine.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Fetch {
    url: String,
    rev: String,
    /// The pin `blaze.lock` had when the fetch began.
    old: Option<DatabasePin>,
    step: Step,
}

impl Fetch {
    pub fn new(url: &str, rev: &str, pins: &SourcePins) -> Fetch {
        Fetch { url: url.to_owned(), rev: rev.to_owned(), old: pins.advisories.clone(), step: Step::Start }
    }

    /// The first effect.
    pub fn start(&mut self) -> Effect {
        self.step = Step::Fetching;
        Effect::Fetch { url: self.url.clone(), rev: self.rev.clone() }
    }

    /// What to do after `event`.
    pub fn step(&mut self, event: Event) -> Result<Effect, Refused> {
        let step = std::mem::replace(&mut self.step, Step::Done);
        let name = match &step {
            Step::Start => "start",
            Step::Fetching => "fetch",
            Step::Placing(_) => "place the database",
            Step::Recording(_) => "record its pin",
            Step::Done => "finish",
        };
        if let Event::Failed(why) = event {
            return Err(Refused::Failed { step: name, why });
        }
        match (step, event) {
            (Step::Fetching, Event::Fetched { commit, tree }) => {
                if !is_commit(&commit) {
                    return Err(Refused::NoCommit { url: self.url.clone(), commit });
                }
                if let Some(old) = &self.old
                    && old.from.as_ref().is_some_and(|(url, at)| url == &self.url && at == &commit)
                    && old.tree != tree
                {
                    return Err(Refused::Mismatch {
                        url: self.url.clone(),
                        commit,
                        recorded: old.tree.clone(),
                        found: tree,
                    });
                }
                let pin = DatabasePin { from: Some((self.url.clone(), commit)), tree };
                self.step = Step::Placing(pin);
                Ok(Effect::Place)
            }
            (Step::Placing(pin), Event::Placed) => {
                self.step = Step::Recording(pin.clone());
                Ok(Effect::Record(pin))
            }
            (Step::Recording(pin), Event::Recorded) => Ok(Effect::Done(pin)),
            (_, event) => Err(Refused::Unexpected { step: name, event: format!("{event:?}") }),
        }
    }
}
