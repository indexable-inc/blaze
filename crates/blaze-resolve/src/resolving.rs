//! The resolve as a sans-IO state machine (rule 14), in the shape of blaze-drive's `Request`:
//! the driver steps it with an [`Event`] and it pushes the [`Effect`]s it needs, ending with
//! [`Effect::Done`]. It never waits; every wait is its driver's, with that driver's deadline
//! and name (rules 15, 18, 44).
//!
//! What it asks for is an [`Ask`]: one dependency and how it matches ([`QueryKind`]), the
//! query cargo's resolver makes of its sources. The driver's [`Registry`] answers at once what
//! it holds (packages read, the lock file's pins, index files on disk); a query it answers
//! [`Lookup::Unknown`] is noted, and when the round ends every query noted in it is one
//! [`Effect::Query`], so the driver fetches the round's index files together.
//!
//! The state between steps is the answer table, not a stack of frames. The resolver is a
//! backtracking search whose position is a context cloned at every choice; cargo already
//! throws a round with pending queries away and runs the search again over more answers, and
//! the search is a pure function of the answers, so a round is that function over the table:
//! a dependency whose query is unknown is left out, the round's result is kept only if it met
//! no unknown query, and otherwise the round's asks go out and the next round runs when the
//! last of them is answered. A round always asks something no earlier round asked (an ask is
//! noted only when the table lacks it), so the rounds end; [`MOST_ROUNDS`] bounds them.
//!
//! Its cost is the floor's: with a lock file every query is the driver's table's and the first
//! round is the only one; making a lock file takes as many rounds as the graph's index files
//! are deep, each round fetching every file it can name, which is what the network allows.

use std::alloc::{Allocator, Global};
use std::cell::RefCell;
use std::fmt;
use std::marker::PhantomData;

use blaze_hash::{FixedState, Map, MapOpts, Set, SetOpts};
use blaze_own::Share;

use crate::dependency::Dependency;
use crate::error::ResolveError;
use crate::package_id_spec::PackageIdSpec;
use crate::registry::{IndexSummary, Lookup, QueryKind, Registry};
use crate::resolve::Resolve;
use crate::resolver::{CliFeatures, HasDevUnits};
use crate::source_id::SourceId;
use crate::workspace_resolve::{PackageRegistry, Workspace, resolve_ws};

/// The most rounds a resolve runs before it stops with [`ResolveError::Unsettled`]: a round is
/// one more layer of index files, and no dependency graph is near this deep.
pub const MOST_ROUNDS: u32 = 4096;

/// One query of the sources: a dependency and how it matches.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct Ask {
    pub dep: Dependency,
    pub kind: QueryKind,
}

impl Share for Ask {}

/// What the driver tells the machine.
#[derive(Debug)]
pub enum Event<A: Allocator> {
    /// Run the first round.
    Start,
    /// The answer to `ask`, one of the last [`Effect::Query`]'s: each summary of the source
    /// that matches it as its kind says, or why the source could not answer (which ends the
    /// resolve with that error).
    Answered { ask: Ask, answer: Result<Vec<IndexSummary, A>, ResolveError> },
}

/// What the machine asks of its driver.
#[derive(Debug)]
pub enum Effect<T, A: Allocator> {
    /// Answer each of `asks` with [`Event::Answered`], in any order; the next round runs when
    /// the last is in.
    Query { asks: Vec<Ask, A> },
    /// The resolve's answer: nothing follows it.
    Done(Result<T, ResolveError>),
}

/// A step the machine refuses: the driver's mistake, named (rule 19).
#[derive(Debug)]
pub enum Refused {
    /// [`Event::Start`] after the first.
    Started,
    /// An answer to a query the machine has not asked, or has had answered already.
    NotAsked { ask: Ask },
    /// A step after [`Effect::Done`].
    Finished,
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refused::Started => write!(f, "the resolve machine was started twice"),
            Refused::NotAsked { ask } => write!(
                f,
                "an answer for `{}` ({:?}) of {}, which the resolve machine has not asked for",
                ask.dep.package_name(),
                ask.kind,
                ask.dep.source_id()
            ),
            Refused::Finished => write!(f, "a step of the resolve machine after its answer"),
        }
    }
}

impl core::error::Error for Refused {}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    New,
    /// A round's asks are out: [`Resolving::asked`] holds those not answered yet.
    Asking,
    Done,
}

/// The resolve machine: `run` (one round of a resolve, over a [`Registry`]) run over the
/// driver's `sources` and the answers to what it asked.
pub struct Resolving<R, F, T, A: Allocator + Clone> {
    sources: R,
    run: F,
    /// The answers to every ask so far, kept for the rounds after.
    answers: Map<Ask, Vec<IndexSummary, A>, FixedState, A>,
    /// The last round's asks not answered yet.
    asked: Set<Ask, FixedState, A>,
    phase: Phase,
    rounds: u32,
    alloc: A,
    result: PhantomData<fn() -> T>,
}

impl<R, F, T, A> Resolving<R, F, T, A>
where
    R: Registry,
    F: Fn(&Asking<'_, R, A>) -> Result<T, ResolveError>,
    A: Allocator + Copy,
{
    pub fn new(sources: R, run: F, alloc: A) -> Self {
        Resolving {
            sources,
            run,
            answers: Map::new(MapOpts { hasher: FixedState, alloc, capacity: None }),
            asked: Set::new(SetOpts { hasher: FixedState, alloc, capacity: None }),
            phase: Phase::New,
            rounds: 0,
            alloc,
            result: PhantomData,
        }
    }

    /// The rounds run so far.
    pub fn rounds(&self) -> u32 {
        self.rounds
    }

    /// The driver's sources.
    pub fn sources(&self) -> &R {
        &self.sources
    }

    /// One event in; its effects pushed onto `out`.
    pub fn step(&mut self, event: Event<A>, out: &mut Vec<Effect<T, A>, A>) -> Result<(), Refused> {
        match (self.phase, event) {
            (Phase::Done, _) => Err(Refused::Finished),
            (Phase::New, Event::Start) => {
                self.round(out);
                Ok(())
            }
            (Phase::Asking, Event::Start) => Err(Refused::Started),
            (Phase::New, Event::Answered { ask, .. }) => Err(Refused::NotAsked { ask }),
            (Phase::Asking, Event::Answered { ask, answer }) => {
                if !self.asked.remove(&ask) {
                    return Err(Refused::NotAsked { ask });
                }
                match answer {
                    Ok(summaries) => {
                        self.answers.insert(ask, summaries);
                        if self.asked.is_empty() {
                            self.round(out);
                        }
                    }
                    Err(cause) => {
                        let (dep, source) = (ask.dep.package_name(), ask.dep.source_id());
                        let cause = Box::new_in(cause, Global);
                        self.phase = Phase::Done;
                        out.push(Effect::Done(Err(ResolveError::LoadSource { dep, source, cause })));
                    }
                }
                Ok(())
            }
        }
    }

    /// `run` over the table: its answer when it met no unknown query, else its asks.
    fn round(&mut self, out: &mut Vec<Effect<T, A>, A>) {
        self.rounds += 1;
        let asking = Asking {
            sources: &self.sources,
            answers: &self.answers,
            noted: RefCell::new(Noted {
                order: Vec::new_in(self.alloc),
                seen: Set::new(SetOpts { hasher: FixedState, alloc: self.alloc, capacity: None }),
            }),
        };
        let result = (self.run)(&asking);
        let asks = asking.noted.into_inner().order;
        if asks.is_empty() {
            self.phase = Phase::Done;
            out.push(Effect::Done(result));
            return;
        }
        if self.rounds >= MOST_ROUNDS {
            self.phase = Phase::Done;
            let unsettled = ResolveError::Unsettled { rounds: self.rounds, asks: asks.len() };
            out.push(Effect::Done(Err(unsettled)));
            return;
        }
        for ask in &asks {
            self.asked.insert(ask.share());
        }
        self.phase = Phase::Asking;
        out.push(Effect::Query { asks });
    }
}

/// The resolve machine of [`resolve_ws`]: cargo's two resolves of a workspace, over the
/// driver's `sources` with `[patch]` and the lock file's locks applied afresh each round.
#[allow(clippy::type_complexity)]
pub fn resolving_ws<'w, R, A>(
    sources: R,
    ws: &'w Workspace<'w>,
    lock: Option<&'w Resolve>,
    cli_features: &'w CliFeatures,
    specs: &'w [PackageIdSpec],
    has_dev_units: HasDevUnits,
    alloc: A,
) -> Resolving<
    R,
    impl Fn(&Asking<'_, R, A>) -> Result<(Resolve, Resolve), ResolveError> + 'w,
    (Resolve, Resolve),
    A,
>
where
    R: Registry,
    A: Allocator + Copy + 'w,
{
    let run = move |asking: &Asking<'_, R, A>| {
        let mut registry = PackageRegistry::new(asking);
        resolve_ws(&mut registry, ws, lock, cli_features, specs, has_dev_units)
    };
    Resolving::new(sources, run, alloc)
}

/// The queries a round met that no answer held, in the order it met them, each once.
struct Noted<A: Allocator> {
    order: Vec<Ask, A>,
    seen: Set<Ask, FixedState, A>,
}

/// What one round queries: the machine's answers first, then the driver's sources, noting
/// each query neither holds.
pub struct Asking<'m, R, A: Allocator> {
    sources: &'m R,
    answers: &'m Map<Ask, Vec<IndexSummary, A>, FixedState, A>,
    noted: RefCell<Noted<A>>,
}

impl<R: Registry, A: Allocator + Clone> Registry for Asking<'_, R, A> {
    fn query(
        &self,
        dep: &Dependency,
        kind: QueryKind,
        f: &mut impl FnMut(IndexSummary),
    ) -> Result<Lookup, ResolveError> {
        let ask = Ask { dep: dep.share(), kind };
        if let Some(found) = self.answers.get(&ask) {
            for summary in found {
                f(summary.share());
            }
            return Ok(Lookup::Answered(()));
        }
        match self.sources.query(dep, kind, f)? {
            Lookup::Answered(()) => Ok(Lookup::Answered(())),
            Lookup::Unknown => {
                let mut noted = self.noted.borrow_mut();
                if noted.seen.insert(ask.share()) {
                    noted.order.push(ask);
                }
                Ok(Lookup::Unknown)
            }
        }
    }

    fn describe_source(&self, source: SourceId) -> String {
        self.sources.describe_source(source)
    }

    fn is_replaced(&self, source: SourceId) -> bool {
        self.sources.is_replaced(source)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::package_id::PackageId;
    use crate::registry::MemRegistry;
    use crate::resolve::ResolveVersion;
    use crate::resolver::{self, ResolveOpts, ResolveSettings, VersionPreferences};
    use crate::summary::Summary;

    fn summary(name: &str, version: &str, deps: &[&str]) -> Summary {
        let id = PackageId::try_new(name, version, SourceId::crates_io()).unwrap();
        let deps = deps.iter().map(|name| dep(name)).collect();
        Summary::new(id, deps, &BTreeMap::new(), None::<&str>, None).unwrap()
    }

    fn dep(name: &str) -> Dependency {
        Dependency::parse(name, Some("1"), SourceId::crates_io()).unwrap()
    }

    /// Sources that hold nothing yet: every query is the driver's to answer.
    struct Unfetched;

    impl Registry for Unfetched {
        fn query(
            &self,
            _dep: &Dependency,
            _kind: QueryKind,
            _f: &mut impl FnMut(IndexSummary),
        ) -> Result<Lookup, ResolveError> {
            Ok(Lookup::Unknown)
        }

        fn describe_source(&self, source: SourceId) -> String {
            source.to_string()
        }

        fn is_replaced(&self, _source: SourceId) -> bool {
            false
        }
    }

    /// `root` resolved by the machine over `sources`, each ask answered from `fake`: the
    /// packages picked (sorted) and each effect's asks, as `name kind`.
    fn drive(
        sources: impl Registry,
        fake: &MemRegistry,
        root: &Summary,
    ) -> (Result<Vec<String>, ResolveError>, Vec<Vec<String>>) {
        let roots = [(root.clone(), ResolveOpts::everything())];
        let prefs = VersionPreferences::default();
        let settings = ResolveSettings::default();
        let run = |asking: &Asking<'_, _, Global>| {
            let version = ResolveVersion::with_rust_version(None);
            resolver::resolve(&roots, &[], asking, &prefs, version, &settings)
        };
        let mut machine = Resolving::new(sources, run, Global);
        let mut effects = Vec::new();
        let mut out = Vec::new_in(Global);
        machine.step(Event::Start, &mut out).unwrap();
        loop {
            match out.pop().expect("a step that is not the last answer of a round pushes one effect") {
                Effect::Done(result) => {
                    assert!(out.is_empty());
                    let picked = result.map(|resolve| {
                        let mut picked: Vec<String> = resolve.iter().map(|id| id.to_string()).collect();
                        picked.sort();
                        picked
                    });
                    return (picked, effects);
                }
                Effect::Query { asks } => {
                    effects.push(
                        asks.iter().map(|ask| format!("{} {:?}", ask.dep.package_name(), ask.kind)).collect(),
                    );
                    for ask in asks {
                        let mut found = Vec::new_in(Global);
                        fake.matching(&ask.dep, ask.kind, &mut |it| found.push(it));
                        machine.step(Event::Answered { ask, answer: Ok(found) }, &mut out).unwrap();
                    }
                }
            }
        }
    }

    fn graph() -> (MemRegistry, Summary) {
        let fake = [
            summary("a", "1.0.0", &["c"]),
            summary("b", "1.0.0", &["c"]),
            summary("c", "1.0.0", &[]),
        ]
        .into_iter()
        .collect();
        (fake, summary("root", "1.0.0", &["a", "b"]))
    }

    /// Prints nothing; fails if the machine asks for anything, or needs a second step, when
    /// its sources hold every answer.
    #[test]
    fn a_resolve_with_every_answer_held_finishes_in_one_step() {
        let (fake, root) = graph();
        let (picked, effects) = drive(&fake, &fake, &root);
        assert!(effects.is_empty(), "asked {effects:?}");
        assert_eq!(picked.unwrap().len(), 4);
    }

    /// Prints nothing; fails if a dependency's query is asked twice (`c`, which both `a` and
    /// `b` need) or the first round's two independent queries come as two effects.
    #[test]
    fn each_query_is_asked_once_and_a_round_asks_together() {
        let (fake, root) = graph();
        let (picked, effects) = drive(Unfetched, &fake, &root);
        assert_eq!(effects, [vec!["a Exact", "b Exact"], vec!["c Exact"]]);
        let picked = picked.unwrap();
        assert_eq!(picked.len(), 4, "{picked:?}");
        assert!(picked.iter().any(|it| it.starts_with("c v1.0.0")), "{picked:?}");
    }

    /// Prints nothing; fails if an answer for a query not asked, an answer twice, a second
    /// start or a step after the answer is taken instead of refused.
    #[test]
    fn an_answer_not_asked_for_is_refused() {
        let (_, root) = graph();
        let roots = [(root, ResolveOpts::everything())];
        let prefs = VersionPreferences::default();
        let settings = ResolveSettings::default();
        let run = |asking: &Asking<'_, Unfetched, Global>| {
            let version = ResolveVersion::with_rust_version(None);
            resolver::resolve(&roots, &[], asking, &prefs, version, &settings)
        };
        let mut machine = Resolving::new(Unfetched, run, Global);
        let mut out = Vec::new_in(Global);
        let stray = Ask { dep: dep("z"), kind: QueryKind::Exact };
        let answer = || Ok(Vec::new_in(Global));

        let early = machine.step(Event::Answered { ask: stray.clone(), answer: answer() }, &mut out);
        assert!(matches!(early, Err(Refused::NotAsked { .. })), "{early:?}");

        machine.step(Event::Start, &mut out).unwrap();
        let Some(Effect::Query { asks }) = out.pop() else { panic!("the first round asks") };
        assert_eq!(asks.len(), 2);
        assert!(matches!(machine.step(Event::Start, &mut out), Err(Refused::Started)));

        let stray_now = machine.step(Event::Answered { ask: stray, answer: answer() }, &mut out);
        assert!(matches!(stray_now, Err(Refused::NotAsked { .. })), "{stray_now:?}");

        let first = asks[0].clone();
        machine.step(Event::Answered { ask: first.clone(), answer: answer() }, &mut out).unwrap();
        assert!(out.is_empty(), "a round runs when its last ask is answered");
        let twice = machine.step(Event::Answered { ask: first, answer: answer() }, &mut out);
        assert!(matches!(twice, Err(Refused::NotAsked { .. })), "{twice:?}");

        let failed = Err(ResolveError::NotPinned { dep: "b".into(), source: "a test".into() });
        machine.step(Event::Answered { ask: asks[1].clone(), answer: failed }, &mut out).unwrap();
        let Some(Effect::Done(Err(ResolveError::LoadSource { dep, .. }))) = out.pop() else {
            panic!("a failed answer ends the resolve naming its dependency")
        };
        assert_eq!(dep.to_string(), "b");
        assert!(matches!(machine.step(Event::Start, &mut out), Err(Refused::Finished)));
    }
}
