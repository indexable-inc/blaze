//! What the daemon gives back when memory is short: the thin driver of
//! `blaze_graph::giveback::Planner` (docs/v2/plan/design/memory.md, "Giving memory back").
//!
//! Once a loop turn ([`Jobs::give_back`], from `Jobs::apply_writes`) the planner is told the
//! limit, the last footprint reading and what a task waits for; when it would plan, the
//! sources are listed and its gives done:
//!
//! | rung | the source | the give |
//! |---|---|---|
//! | allocator | what the last reading says the allocators keep freed | a purge on the purger's thread (`Plane::ask_purge`), then a reading marked purged |
//! | pool | the pages the stores' pools hold (the `store` tag's committed bytes) | `Store::evict_pool` of that many bytes |
//! | values | each kept front end's values kept by bytes (`KeptDb::values`); the front ends of the builds under way, all or nothing | `KeptDb::give_values`; `Writer::short` |
//! | front end | each kept front end (`KeptDb::held`), the checkout built last the floor | dropped: its checkout's next build lowers anew, and its `blaze why` says so |
//!
//! Every give is a `memory give-back` span and event (what, whose, how much, why, what it
//! costs to make again), which `blaze explain memory` reads. A front end let go keeps the
//! span and the log line it had before this (`kept front ends let go for memory`, "is let go,
//! and its next build lowers anew"), which the guard counts (`scripts/src/bin/guards.rs`).
//!
//! Not listed yet: the caches rung (codegen's kept walks may be let go only while no walk is
//! in flight, which the loop cannot say).

use std::time::Duration;

use blaze_graph::giveback::{self, Give, Now, Outcome, Planner, Rung, Source, Why};

use super::{Jobs, KeptDb, Stage, Steps, World, key_of};
use crate::serve::{plane::ns, work::Writer as _};

/// The owner of the one source that is every build under way's front end: no checkout's
/// number.
const RUNNING: u32 = u32::MAX;

/// How often, at most, the sources are listed again after a plan that had nothing to give: a
/// listing weighs every kept front end's values.
const PLAN_EVERY: Duration = Duration::from_millis(250);

/// How often, at most, the builds under way are asked for a release
/// (`Writer::short`): whether one comes of it is the release's own rule
/// (`blaze_graph::admission::Squeeze::short`), so the ask's source does not fall at once.
const RUNNING_EVERY: Duration = Duration::from_secs(1);

/// What the give-back rule knows of one checkout.
#[derive(Clone, Copy, Default)]
struct Root {
    /// When its lowering from nothing began, until its front end is kept.
    began: Option<Duration>,
    /// What its front end took to make from nothing, lowering to kept, the last time; 0 until
    /// one was.
    cost_ns: u64,
}

/// The give-back rule's state, the daemon's one ([`Jobs::give_back`]).
pub(super) struct Giving {
    planner: Planner,
    /// Kept scratch.
    sources: Vec<Source>,
    gives: Vec<Give>,
    /// By root number.
    roots: Vec<Root>,
    /// The checkout whose front end was kept last: the floor.
    last_built: Option<u32>,
    /// A plan had nothing to give: no listing before this.
    idle_until: Option<Duration>,
    /// When the builds under way were last asked for a release.
    running_asked: Option<Duration>,
    /// The floor was said not to fit, in this stretch past the limit.
    floor_said: bool,
}

impl Giving {
    pub(super) fn new() -> Giving {
        Giving {
            planner: Planner::new(),
            sources: Vec::new(), // alloc: kept scratch, none until memory is short
            gives: Vec::new(),   // alloc: kept scratch, none until memory is short
            roots: Vec::new(),   // alloc: one entry a checkout
            last_built: None,
            idle_until: None,
            running_asked: None,
            floor_said: false,
        }
    }

    fn root(&mut self, root: u32) -> &mut Root {
        let at = root as usize;
        if self.roots.len() <= at {
            self.roots.resize(at + 1, Root::default()); // alloc: one entry a checkout
        }
        &mut self.roots[at]
    }

    /// A job of checkout `root` lowers at `now`, into its kept front end (`kept`) or from
    /// nothing: the time a front end made from nothing takes is what giving it back costs.
    pub(super) fn lowers(&mut self, root: u32, kept: bool, now: Duration) {
        self.root(root).began = (!kept).then_some(now);
    }

    /// Checkout `root`'s front end is kept again at `now`: it is the floor, and one made from
    /// nothing is held for a time that grows with what it took
    /// (`blaze_graph::giveback::hold_ns`).
    pub(super) fn kept(&mut self, root: u32, now: Duration) {
        self.last_built = Some(root);
        if let Some(began) = self.root(root).began.take() {
            let cost_ns = ns(now.saturating_sub(began));
            self.root(root).cost_ns = cost_ns;
            self.planner.remade(Rung::FrontEnd, root, ns(now), cost_ns);
        }
    }

    /// What the planner did so far, counted (`blaze explain memory`).
    pub(super) fn counts(&self) -> giveback::Counts {
        self.planner.counts()
    }
}

/// The last component of a checkout's path: the project, as a user names it.
fn project(checkout: &str) -> &str {
    checkout.trim_end_matches('/').rsplit('/').next().filter(|it| !it.is_empty()).unwrap_or(checkout)
}

impl<S: Steps> Jobs<S> {
    /// The checkout numbered `root`, by its path.
    fn checkout_of(&self, root: u32) -> &str {
        self.roots.ids.iter().find(|&(_, &id)| id == root).map_or("no checkout", |(name, _)| &**name)
    }

    /// One step of the give-back rule (the module's doc), `ask` the bytes a task waits for of
    /// what the process holds besides its tasks (`blaze_graph::Plane::short`). Called once a
    /// loop turn: with nothing short it reads two numbers and returns.
    pub(super) fn give_back(&mut self, w: &mut World<'_, S>, ask: u64) {
        let reading = w.plane.reading();
        let now = Now {
            at_ns: ns(w.now),
            limit: blaze_graph::admission::limit(),
            footprint: reading.footprint,
            ask,
            reading: reading.readings,
            purged: reading.purged,
        };
        let mut gives = std::mem::take(&mut self.giving.gives);
        gives.clear();
        if !self.giving.planner.due(&now) {
            // Enough, or measuring: the planner keeps its own state, with no source listed.
            let outcome = self.giving.planner.step(&now, &[], &mut gives);
            if outcome == Outcome::Enough {
                self.giving.floor_said = false;
                self.giving.idle_until = None;
            }
            self.giving.gives = gives;
            return;
        }
        if self.giving.idle_until.is_some_and(|until| w.now < until) {
            self.giving.gives = gives;
            return;
        }
        let span = tracing::info_span!(
            target: "blaze",
            "memory give-back plan",
            ask,
            limit = now.limit,
            footprint = now.footprint,
            allocator_kept = reading.kept,
            outcome = tracing::field::Empty,
        )
        .entered();
        let mut sources = std::mem::take(&mut self.giving.sources);
        sources.clear();
        self.give_sources(w, reading.kept, &mut sources);
        let outcome = self.giving.planner.step(&now, &sources, &mut gives);
        match outcome {
            Outcome::Gave { rung } => {
                span.record("outcome", rung.name());
                self.giving.idle_until = None;
                for give in &gives {
                    self.give(w, give);
                }
            }
            Outcome::Waits { need } => {
                span.record("outcome", "waits");
                self.giving.idle_until = Some(w.now + PLAN_EVERY);
                tracing::debug!(
                    target: "blaze",
                    need,
                    "a task waits for memory and nothing in proportion is left to give: it waits for a task to end, or starts alone"
                );
            }
            Outcome::FloorOver { over, limit, footprint, owner, kept } => {
                span.record("outcome", "the floor does not fit");
                self.giving.idle_until = Some(w.now + PLAN_EVERY);
                if !std::mem::replace(&mut self.giving.floor_said, true) {
                    let checkout = self.checkout_of(owner);
                    let line = giveback::floor_line(project(checkout), over, limit, footprint, kept);
                    tracing::warn!(
                        target: "blaze",
                        checkout,
                        over,
                        limit,
                        footprint,
                        kept,
                        "memory give-back: the floor does not fit under the limit"
                    );
                    crate::serve::said::logged!(WARN, "blaze serve: {line}");
                    // The checkout's next build says it too (`blaze why`), unless a loss
                    // waits there already.
                    let lost = &mut self.roots.lost[owner as usize];
                    if lost.is_none() {
                        *lost = Some(line);
                    }
                }
            }
            Outcome::Enough | Outcome::Measuring { .. } => {}
        }
        drop(span);
        self.giving.sources = sources;
        self.giving.gives = gives;
    }

    /// What every owner can give now (the module's table), added to `sources`.
    fn give_sources(&mut self, w: &World<'_, S>, allocator_kept: u64, sources: &mut Vec<Source>) {
        if allocator_kept != 0 {
            // alloc: none past the most sources, the scratch kept (every push below)
            sources.push(Source {
                rung: Rung::Allocator,
                owner: 0,
                gives: allocator_kept,
                grain: allocator_kept,
                remake_ns: 0,
                floor: false,
            });
        }
        let pool = blaze_mem::tag::snapshot().get(blaze_mem::Tag::Store).live;
        if pool != 0 {
            sources.push(Source { rung: Rung::Pool, owner: 0, gives: pool, grain: 1, remake_ns: 0, floor: false });
        }
        let floor = self.giving.last_built;
        for (at, front) in self.roots.fronts.iter_mut().enumerate() {
            let Some(front) = front else { continue };
            let owner = at as u32;
            let cost_ns = self.giving.roots.get(at).map_or(0, |it| it.cost_ns);
            let held = front.held();
            let values = front.values();
            if values != 0 {
                // What the values cost to make again is not summed from their queries' spans
                // yet: their part of the front end's own cost, by bytes, which is more than
                // they cost (a value is made again from its memo's inputs, kept).
                let remake_ns = match held {
                    0 => 0,
                    held => {
                        let part = u128::from(cost_ns) * u128::from(values.min(held)) / u128::from(held);
                        u64::try_from(part).unwrap_or(u64::MAX)
                    }
                };
                sources.push(Source { rung: Rung::Values, owner, gives: values, grain: 1, remake_ns, floor: false });
            }
            if held != 0 {
                sources.push(Source {
                    rung: Rung::FrontEnd,
                    owner,
                    gives: held,
                    grain: held,
                    remake_ns: cost_ns,
                    floor: floor == Some(owner),
                });
            }
        }
        // The builds under way: what their front ends hold in MIR and inference, which a
        // release lets go of where nothing still to compile reads it.
        let building = self.order.iter().any(|&id| {
            self.jobs.get(key_of(id)).is_some_and(|job| matches!(job.stage, Stage::Units(_)))
        });
        let lately = self.giving.running_asked.is_some_and(|at| w.now < at + RUNNING_EVERY);
        if building && !lately {
            let snapshot = blaze_mem::tag::snapshot();
            let of = |tag| {
                let it = snapshot.get(tag);
                it.live.saturating_add(it.heap)
            };
            let gives = of(blaze_mem::Tag::Mir).saturating_add(of(blaze_mem::Tag::Infer));
            if gives != 0 {
                sources.push(Source { rung: Rung::Values, owner: RUNNING, gives, grain: gives, remake_ns: 0, floor: false });
            }
        }
    }

    /// Does one give (the module's table), under a span that says what, whose, how much, why
    /// and what it costs to make again.
    fn give(&mut self, w: &mut World<'_, S>, give: &Give) {
        let (why, need) = match give.why {
            Why::Ask(bytes) => ("a task waits for memory the process holds besides its tasks", bytes),
            Why::Over(bytes) => ("the footprint is past the limit", bytes),
        };
        let checkout = match give.rung {
            Rung::Values | Rung::FrontEnd if give.owner != RUNNING => self.checkout_of(give.owner).to_owned(), // alloc: a give's name, once a give
            Rung::Values => "the builds under way".to_owned(), // alloc: a give's name, once a give
            _ => String::new(),                                // alloc: none, empty
        };
        let span = tracing::info_span!(
            target: "blaze",
            "memory give-back",
            what = give.rung.name(),
            owner = checkout.as_str(),
            bytes = give.bytes,
            why,
            need,
            limit = give.limit,
            footprint = give.footprint,
            remake_ns = give.remake_ns,
            again = give.again,
            gave = tracing::field::Empty,
        )
        .entered();
        let gave = match give.rung {
            Rung::Allocator => {
                w.plane.ask_purge();
                None
            }
            Rung::Pool => {
                let store = w.store.evict_pool(give.bytes);
                let std = w.std.evict_pool(give.bytes.saturating_sub(store));
                Some(store + std)
            }
            // No source of this rung is listed (the module's doc).
            Rung::Caches => Some(0),
            Rung::Values if give.owner == RUNNING => {
                self.giving.running_asked = Some(w.now);
                for &id in &self.order {
                    let Some(job) = self.jobs.get_mut(key_of(id)) else { continue };
                    if let Stage::Units(build) = &mut job.stage {
                        build.writer.short();
                    }
                }
                None
            }
            Rung::Values => self
                .roots
                .fronts
                .get_mut(give.owner as usize)
                .and_then(Option::as_mut)
                .map(|front| front.give_values(give.bytes)),
            Rung::FrontEnd => self.give_front_end(give, &checkout),
        };
        if let Some(gave) = gave {
            span.record("gave", gave);
        }
        tracing::info!(
            target: "blaze",
            what = give.rung.name(),
            owner = checkout.as_str(),
            bytes = give.bytes,
            why,
            need,
            limit = give.limit,
            footprint = give.footprint,
            remake_ns = give.remake_ns,
            again = give.again,
            "memory give-back"
        );
        if give.again {
            // The named failure (hysteresis): given, made again, and given again.
            crate::serve::said::logged!(WARN,
                "blaze serve: memory thrash: {} of {checkout} was given back, made again and is given back again ({} such so far)",
                give.rung.name(),
                self.giving.counts().thrash
            );
        }
    }

    /// Lets go of checkout `checkout`'s kept front end, whole: its next build lowers anew,
    /// and says why (`blaze why`, through `Roots::lost`). What its storage held.
    fn give_front_end(&mut self, give: &Give, checkout: &str) -> Option<u64> {
        let front = self.roots.fronts.get_mut(give.owner as usize)?.take()?;
        let gib = |bytes: u64| bytes as f64 / f64::from(1u32 << 30);
        // The span and the line a front end let go has had since before this rule: the guard
        // counts both (`scripts/src/bin/guards.rs`).
        let _span =
            tracing::info_span!(target: "blaze", "kept front ends let go for memory", short = give.why.need(), kept = 1)
                .entered();
        match give.why {
            Why::Ask(short) => crate::serve::said::logged!(WARN,
                "blaze serve: the kept front end of {checkout} is let go, and its next build lowers anew: \
                 a task waits for {:.2} GiB the process holds besides its tasks, and nothing cheaper is left to give \
                 ({:.2} GiB freed, {:.0} s to make again)",
                gib(short),
                gib(give.bytes),
                give.remake_ns as f64 / 1e9
            ),
            Why::Over(over) => crate::serve::said::logged!(WARN,
                "blaze serve: the kept front end of {checkout} is let go, and its next build lowers anew: \
                 blaze holds {:.2} GiB, {:.2} past its limit of {:.2} GiB, and nothing cheaper is left to give \
                 ({:.2} GiB freed, {:.0} s to make again)",
                gib(give.footprint),
                gib(over),
                gib(give.limit),
                gib(give.bytes),
                give.remake_ns as f64 / 1e9
            ),
        }
        let line = giveback::dropped_line(project(checkout), give);
        let lost = &mut self.roots.lost[give.owner as usize];
        *lost = Some(match lost.take() {
            Some(before) => format!("{before}; {line}"), // alloc: a note, once a front end let go
            None => line,
        });
        let held = front.held();
        drop(front);
        Some(held)
    }

    /// The system says memory is short (docs/memory-budget.md section 7, "Pressure"): every
    /// kept front end lets go of the values it keeps by bytes, all of them, under a
    /// `memory give-back` span each; none is dropped whole. The daemon purges and its pools
    /// give back at the same event (`Daemon::pressure`), and a limit that falls under what the
    /// daemon holds is [`Jobs::give_back`]'s to answer, in its order.
    pub(super) fn press_kept(&mut self) {
        for at in 0..self.roots.fronts.len() {
            let Some(front) = self.roots.fronts[at].as_mut() else { continue };
            let values = front.values();
            if values == 0 {
                continue;
            }
            let span = tracing::info_span!(
                target: "blaze",
                "memory give-back",
                what = Rung::Values.name(),
                owner = tracing::field::Empty,
                bytes = values,
                why = "the system says memory is short",
                gave = tracing::field::Empty,
            )
            .entered();
            let gave = front.give_values(values);
            span.record("gave", gave);
            span.record("owner", self.checkout_of(at as u32));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::project;

    #[test]
    fn a_project_is_named_by_its_checkouts_last_component() {
        assert_eq!(project("/root/wt/reth-1112/reth"), "reth");
        assert_eq!(project("/root/wt/tempo/"), "tempo");
        assert_eq!(project("reth"), "reth");
    }
}
