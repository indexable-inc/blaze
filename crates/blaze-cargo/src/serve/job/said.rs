//! What a build says with nothing to do, made of what a build that answered said: the `said`
//! of its request's certificate (docs/v2/plan/design/certificate.md, "When it is written").
//! No IO, no clock.
//!
//! A certificate is written at the end of every build that answered, a build that compiled
//! too. What it says again at a hit is not what that build said (`Compiling foo`, an
//! artifact that is not fresh, the work it did) but what a build of the same request says
//! when every unit is found kept:
//!
//! - a package's line is `Fresh` ([`Verb::Fresh`]: what `Jobs::started_unit` says of a unit
//!   found kept);
//! - an artifact is fresh (`Artifact { fresh: true, ..it }`: what a unit found kept gives,
//!   `unit/compile.rs`);
//! - what was fetched and what the build was doing are not said;
//! - its `Work` is of no work: every unit found, nothing compiled, missed, run again,
//!   linked, placed or served, no time, no build behind it;
//! - every diagnostic, every unit's begin and end, every build script's message: as said (a
//!   unit found kept says its record's diagnostics again).
//!
//! The invariant: `said` is a function of the request and of what the build read, never of
//! the build that happened to write it: not of which units it compiled, not of the order its
//! scheduler began them in. The answer is filed under its reads' digests, and two answers
//! under one key is a refusal (`request::Unwritten::TwoAnswers`), so a `said` that held one
//! build's order or counts would be refused at the next build of the same reads. So the
//! order is the plan's, not the build's: what was said before any unit's word, each
//! package's line by package, each build script's message by package, each unit's words by
//! unit (begin, its diagnostics in its own order, end), what was said after, each test
//! executable by path, the outputs, the work. A walk says the same words in its scheduler's
//! order; a hit says them in this one.

use blaze_api::{Artifact, LinkKind, PlacedBytes, Response, Tiers, UnitStarted, Verb, Worked};

/// A bug planted here, for a test's control: with it `said` holds what one build did, and
/// the test's oracle (two builds of one request say one thing) says so. Never planted
/// outside a test.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Plant {
    None,
    /// A package's line keeps the verb its build said (`Compiling`).
    KeepsVerbs,
    /// The words keep the order their build said them in.
    KeepsOrder,
    /// The `Work` keeps what its build counted.
    KeepsWork,
}

#[cfg(test)]
thread_local! {
    pub(super) static PLANT: std::cell::Cell<Plant> = const { std::cell::Cell::new(Plant::None) };
}

#[cfg(test)]
fn planted(plant: Plant) -> bool {
    PLANT.get() == plant
}

#[cfg(not(test))]
fn planted(plant: Plant) -> bool {
    let _ = plant;
    false
}

/// What a build says, heard as it says it ([`Saying::hear`]), and at its end what a build of
/// the same request says with nothing to do ([`Saying::end`]).
pub(super) struct Saying {
    /// Each word kept, in the form a build with nothing to do says it, with whether it was
    /// heard before any unit's word.
    words: Vec<(bool, Response)>,
    /// A unit's word was heard.
    units: bool,
    /// The build's `Work`, when it said one.
    worked: Option<Worked>,
    /// What it said that no build says with nothing to do: no certificate says it again.
    never: Option<&'static str>,
}

impl Saying {
    pub(super) fn new() -> Saying {
        // alloc: none until the build says something
        Saying { words: Vec::new(), units: false, worked: None, never: None }
    }

    /// A response the build said.
    pub(super) fn hear(&mut self, response: &Response) {
        if self.never.is_some() {
            return;
        }
        let word = match response {
            Response::Doing(_) | Response::Fetched { .. } => return,
            Response::Work(worked) => {
                self.worked = Some(worked.clone()); // clone: one `Work`, once a build
                return;
            }
            Response::Diagnostic(diagnostic) => {
                self.units |= diagnostic.unit.is_some();
                response.clone() // clone: kept to be said again
            }
            Response::Unit(started) => {
                self.units = true;
                let verb = if planted(Plant::KeepsVerbs) { started.verb } else { Verb::Fresh };
                // clone: kept to be said again
                Response::Unit(UnitStarted { verb, ..started.clone() })
            }
            Response::UnitBegan(_) | Response::UnitEnded(_) | Response::BuildScript(_) => {
                self.units = true;
                response.clone() // clone: kept to be said again
            }
            Response::TestReady(artifact) => {
                self.units = true;
                // clone: kept to be said again
                Response::TestReady(Artifact { fresh: true, ..artifact.clone() })
            }
            Response::Built { members, profile, artifacts } => Response::Built {
                members: *members,
                profile: profile.clone(), // clone: kept to be said again
                // clone: kept to be said again
                artifacts: artifacts.iter().map(|it| Artifact { fresh: true, ..it.clone() }).collect(),
            },
            Response::Unchecked(_) => return self.never("what a failed build left unchecked"),
            Response::TestResult(_) => return self.never("a test's result"),
            Response::Ran { .. } => return self.never("a program's output"),
            Response::Error(_) => return self.never("an error"),
            _ => return self.never("a response that is none of a build's"),
        };
        self.words.push((!self.units, word));
    }

    fn never(&mut self, what: &'static str) {
        self.never = Some(what);
        // What it said so far is of no use: let go now, not at its end.
        self.words = Vec::new(); // alloc: none
    }

    /// The build ended having answered: what a build of its request says with nothing to
    /// do, in the plan's order, its `Work` last. Or what it said that such a build never
    /// says.
    pub(super) fn end(self) -> Result<Vec<Response>, &'static str> {
        if let Some(what) = self.never {
            return Err(what);
        }
        let mut words = self.words;
        if !planted(Plant::KeepsOrder) {
            // Stable: words of one place keep the order they were said in.
            // alloc: the sort's buffer, once a build
            words.sort_by(|a, b| place(a.0, &a.1).cmp(&place(b.0, &b.1)));
        }
        let mut said: Vec<Response> = words.into_iter().map(|(_, it)| it).collect(); // alloc: once a build
        if let Some(worked) = self.worked {
            said.push(Response::Work(match planted(Plant::KeepsWork) {
                true => worked,
                false => no_work(worked),
            }));
        }
        Ok(said)
    }
}

/// Where a word is said among a build's: see the module's text. `early`: it was heard before
/// any unit's word.
fn place(early: bool, word: &Response) -> (u8, u64, &str, u8) {
    match word {
        Response::Diagnostic(diagnostic) => match (&diagnostic.unit, early) {
            (Some(unit), _) => (3, u64::from(unit.id), "", 1),
            (None, true) => (0, 0, "", 0),
            (None, false) => (4, 0, "", 0),
        },
        Response::Unit(started) => (1, u64::from(started.package), "", 0),
        Response::BuildScript(ran) => (2, 0, ran.package_id.as_str(), 0),
        Response::UnitBegan(unit) => (3, u64::from(unit.id), "", 0),
        Response::UnitEnded(unit) => (3, u64::from(unit.id), "", 2),
        Response::TestReady(artifact) => (5, 0, artifact.path.as_str(), 0),
        _ => (6, 0, "", 0),
    }
}

/// The `Work` of a build that found every unit kept and did nothing, in place of `worked`,
/// which one build did: of that, the goal and the count of units are the request's (a build
/// that answered began each unit once, found kept or compiled: `Jobs::started_unit`).
fn no_work(worked: Worked) -> Worked {
    Worked {
        why: None,
        link: LinkKind::Patched,
        found: worked.found.saturating_add(worked.compiled),
        green: 0,
        links: Vec::new(), // alloc: none
        reran: 0,
        misses: 0,
        placed: PlacedBytes::default(),
        served: Vec::new(), // alloc: none
        build_id: None,
        compiled: 0,
        steps_us: Vec::new(), // alloc: none
        critical_path_us: 0,
        tiers: worked.tiers.map(|it| Tiers::of(it.goal)),
        storage: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(package: u32, verb: Verb) -> Response {
        Response::Unit(UnitStarted {
            name: format!("p{package}"),
            verb,
            source: None,
            package,
            version: "0.1.0".to_owned(),
            package_id: format!("p{package} 0.1.0"),
        })
    }

    fn built() -> Response {
        Response::Built { members: 1, profile: "dev".to_owned(), artifacts: Vec::new() }
    }

    fn work(found: u64, compiled: u64) -> Response {
        Response::Work(Worked {
            why: None,
            link: if compiled > 0 { LinkKind::Placed } else { LinkKind::Patched },
            found,
            green: 40 - compiled,
            links: Vec::new(),
            reran: compiled * 9,
            misses: compiled,
            placed: PlacedBytes::default(),
            served: Vec::new(),
            build_id: Some(7 + compiled),
            compiled,
            steps_us: Vec::new(),
            critical_path_us: compiled * 1000,
            tiers: None,
            storage: None,
        })
    }

    /// What a build of three packages says: those of `compiled` compiled, the rest found
    /// kept, begun in `order`.
    fn build(order: [u32; 3], compiled: &[u32]) -> Vec<Response> {
        let mut says = Vec::new();
        for package in order {
            let verb = if compiled.contains(&package) { Verb::Compiling } else { Verb::Fresh };
            says.push(line(package, verb));
        }
        if !compiled.is_empty() {
            says.insert(1, Response::Fetched { verb: blaze_api::FetchVerb::Downloaded, what: "p1 v0.1.0".to_owned() });
        }
        says.push(built());
        says.push(work(3 - compiled.len() as u64, compiled.len() as u64));
        says
    }

    fn said(says: &[Response]) -> Vec<u8> {
        let mut saying = Saying::new();
        for it in says {
            saying.hear(it);
        }
        blaze_protocol::cbor::to_vec(&saying.end().unwrap()).unwrap()
    }

    /// The invariant: a build that compiled and a build that found everything kept, their
    /// units begun in any order, leave the same `said`, byte for byte, and it is what the
    /// second says but for its order and its counts of work.
    fn oracle() -> Result<(), String> {
        let orders = [[0, 1, 2], [2, 0, 1], [1, 2, 0]];
        let idle = said(&build([0, 1, 2], &[]));
        for order in orders {
            for compiled in [&[][..], &[1][..], &[0, 1, 2][..]] {
                let it = said(&build(order, compiled));
                if it != idle {
                    return Err(format!("a build in order {order:?} that compiled {compiled:?} leaves another `said`"));
                }
            }
        }
        let again: Vec<Response> = blaze_protocol::cbor::from_slice(&idle).unwrap();
        let lines: Vec<Response> = [0, 1, 2].into_iter().map(|it| line(it, Verb::Fresh)).collect();
        if again[..3] != lines[..] || again[3] != built() {
            return Err(format!("a hit says {again:?}"));
        }
        match &again[4] {
            Response::Work(worked) if worked.found == 3 && worked.compiled == 0 && worked.misses == 0 && worked.build_id.is_none() => Ok(()),
            other => Err(format!("a hit's work is {other:?}")),
        }
    }

    #[test]
    fn two_builds_of_one_request_leave_one_said() {
        oracle().unwrap();
    }

    #[test]
    fn the_oracle_catches_each_planted_bug() {
        for plant in [Plant::KeepsVerbs, Plant::KeepsOrder, Plant::KeepsWork] {
            PLANT.set(plant);
            let caught = oracle();
            PLANT.set(Plant::None);
            assert!(caught.is_err(), "{plant:?} was not caught");
        }
    }

    #[test]
    fn a_build_that_says_what_no_idle_build_says_leaves_none() {
        let mut saying = Saying::new();
        saying.hear(&line(0, Verb::Fresh));
        saying.hear(&Response::Done { ok: true });
        assert_eq!(saying.end().err(), Some("a response that is none of a build's"));
    }
}
