//! A build script's run fetched from the global cache before it runs (fastcompile-global 3,
//! docs/v2/plan/lanes/fastcompile/global.md): a run of a crates.io package's script whose
//! shapes and result the store lacks asks the cache for them, each proven public by
//! [`blaze_global::Rule::run`] first, and runs only once nothing public is left to fetch.
//!
//! The order is the store's own ([`blaze_usercode::index`]): each shape slot the store has, its
//! result for the inputs as they are now; then the next slot. A slot the cache has comes back
//! as a shape whose result is asked next. A key is asked once; [`Fetched::Missing`] moves on,
//! and [`Fetched::Refused`] is an error of the run naming the key, never a run in its place.
//! The daemon owns the client: it fetches each [`Step::Fetch`], tells [`Fetch::fetched`] the
//! store's answer, and steps again until [`Step::Run`].

#![forbid(unsafe_code)]

use std::fmt;

use blaze_cas::{Corrupt, Fetched, Key};
use blaze_core::Traced;
use blaze_global::{Origin, Public, Rule, RunMade, RunPart};
use blaze_units::query::PackagePlan;
use blaze_usercode::{
    Given, RunKey,
    index::{self, Shape},
    trace::Inputs,
};

/// What a run asks of the global cache next.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Step {
    /// This key, proven public: step again once its answer is told.
    Fetch(Public),
    /// Nothing public is left to fetch: begin the run, which finds the result if the store
    /// has it now.
    Run,
}

/// What one run's key was made of, as [`Rule::run`] makes it again.
pub(crate) struct Made<'a> {
    pub(crate) run: &'a RunKey,
    /// What its key's `given` was made of ([`Given::finish`] of it is `run.given`).
    pub(crate) given: &'a Given,
    /// The script's package: both the code that runs and what it runs for.
    pub(crate) origin: &'a Origin,
}

impl Made<'_> {
    fn public(&self, rule: &Rule, traced: &Traced, part: RunPart) -> Option<Public> {
        let made = RunMade { run: *self.run, given: self.given, code: self.origin, caller: self.origin, traced };
        rule.run(&made, part).ok()
    }
}

/// The fetches of one run.
#[derive(Debug, Default)]
pub(crate) struct Fetch {
    /// Each key asked and answered, never asked again.
    asked: Vec<Key>,
    /// The key asked whose answer is not told yet.
    waiting: Option<Key>,
}

impl Fetch {
    /// The key asked whose answer is not told yet.
    pub(crate) fn waiting(&self) -> Option<Key> {
        self.waiting
    }

    /// What to ask next of the store as `get` reads it, with the inputs as `inputs` has them.
    pub(crate) fn step<'s, I>(
        &mut self,
        rule: &Rule,
        made: &Made<'_>,
        get: impl Fn(&Key) -> Result<Option<&'s [u8]>, Corrupt>,
        inputs: &mut I,
    ) -> Result<Step, String>
    where
        I: Inputs,
        I::Error: fmt::Display,
    {
        if let Some(key) = self.waiting {
            return Err(format!("the global cache's answer for {:x} is not in yet", key.0));
        }
        let read = |key: &Key| get(key).map_err(|corrupt| format!("the store: {corrupt}"));
        let mut n = 0;
        loop {
            let at = index::slot(made.run, n);
            let Some(bytes) = read(&at)? else {
                if self.asked.contains(&at) {
                    return Ok(Step::Run);
                }
                // A shape names no value of this machine: its key is public when the run is.
                return Ok(match made.public(rule, &Traced::default(), RunPart::Shape(n)) {
                    Some(public) => self.ask(public),
                    None => Step::Run,
                });
            };
            let shape = Shape::decode(bytes).map_err(|why| format!("slot {n} of the run's shapes: {why}"))?;
            let traced = shape.values(inputs).map_err(|error| error.to_string())?;
            let result = index::result_key(made.run, &traced);
            if read(&result)?.is_some() {
                return Ok(Step::Run);
            }
            // A result that read what is not public is no other machine's: the next shape may be.
            if !self.asked.contains(&result)
                && let Some(public) = made.public(rule, &traced, RunPart::Result)
            {
                return Ok(self.ask(public));
            }
            n += 1;
        }
    }

    fn ask(&mut self, public: Public) -> Step {
        self.waiting = Some(public.key());
        Step::Fetch(public)
    }

    /// The store's answer for `key`: whether it was this run's. A value the cache sent that is
    /// not the one published is an error naming the key.
    pub(crate) fn fetched(&mut self, key: &Key, fetched: &Fetched) -> Result<bool, String> {
        if self.waiting != Some(*key) {
            return Ok(false);
        }
        self.waiting = None;
        self.asked.push(*key);
        match fetched {
            Fetched::Kept(_) | Fetched::Missing => Ok(true),
            Fetched::Refused(why) => Err(format!("the global cache's value of {:x} is refused: {why}", key.0)),
        }
    }
}

/// Where `package` comes from as [`Rule`] names a public crate: a crates.io version, which
/// cargo names with no source and the lock file with its `.crate`'s SHA-256, `checksum`. `None`
/// for any other package: a workspace member, a path, git or other registry's, or one the lock
/// gives no checksum, none of which is fetched.
pub(crate) fn origin(package: &PackagePlan, checksum: Option<[u8; 32]>) -> Option<Origin> {
    match (package.is_member, &package.source, checksum) {
        (false, None, Some(checksum)) => {
            Some(Origin::CratesIo { name: package.name.clone(), version: package.version.clone(), checksum })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests;
