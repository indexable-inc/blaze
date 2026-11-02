//! `blaze debug hot`: the rings' sketches summed ([`read`]), since the process began or over
//! the last build ([`Since`]), as the keys that took most and each kind of wait's quantiles.

use std::sync::{Arc, atomic::AtomicU64};

use blaze_graph::wait::WaitKind;
use blaze_hash::HashMap;
use parking_lot::Mutex;

use super::{CountMin, Counters, DdSketch, PartRef, Quantile, Shared, record};
use crate::{
    clock,
    flight::{self, Sym},
};

/// Which stretch a read sums.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Since {
    /// Since the process began.
    Start,
    /// From the end of the build before the last to the end of the last ([`mark`]).
    LastBuild,
}

/// What a key's span is of.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Function {
    /// The span has no `function`.
    None,
    Named(Arc<str>),
    /// Its name's generation was let go ([`flight::name`]): the number it had.
    Forgotten(Sym),
}

/// A key and the time under it: `upper_ns` never under the truth (the count-min estimate),
/// `lower_ns` never over it (what the threads listing it counted since they listed it).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HotKey {
    pub span: &'static str,
    pub function: Function,
    pub upper_ns: u64,
    pub lower_ns: u64,
}

/// The waits of one kind and their quantiles, each within [`super::ALPHA`] of the exact.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HotWait {
    pub kind: WaitKind,
    pub count: u64,
    pub p50_ns: u64,
    pub p90_ns: u64,
    pub p99_ns: u64,
    pub max_ns: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hot {
    pub since: Since,
    /// Spans counted.
    pub spans: u64,
    /// The time of every span counted, nested ones again.
    pub total_ns: u64,
    /// How far over the truth an `upper_ns` is, but for a chance of 1.8%.
    pub error_ns: u64,
    /// Most time first; ties by span and function.
    pub keys: Vec<HotKey>,
    /// In [`WaitKind::ALL`]'s order, kinds with no waits left out.
    pub waits: Vec<HotWait>,
    /// List entries read while their thread replaced them: their keys are estimated, their
    /// exact ticks left out of `lower_ns`.
    pub torn: u64,
}

/// Why there is nothing to read.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum HotError {
    #[error("no build has ended in this process, so there is no last build to read the sketches of")]
    NoBuild,
}

/// Every ring's counters summed at one moment.
struct Summed {
    spans: u64,
    count_min: CountMin,
    waits: Vec<DdSketch>,
    /// Each ring's listed keys and their exact ticks, by ring number.
    listed: HashMap<(u16, u64), u64>,
    torn: u64,
}

impl Summed {
    fn empty() -> Summed {
        Summed {
            spans: 0,
            count_min: CountMin::new(),
            // alloc: a read's sum, once a read
            waits: (0..record::KINDS).map(|_| DdSketch::new()).collect(),
            listed: HashMap::default(),
            torn: 0,
        }
    }

    fn now() -> Summed {
        let mut summed = Summed::empty();
        flight::each_hot(|ring, cells| summed.add(ring, cells));
        summed
    }

    fn add(&mut self, ring: u16, cells: &[AtomicU64]) {
        let shared = Shared(cells);
        self.spans = self.spans.saturating_add(shared.get(record::SPANS));
        self.count_min.merge_cells(&PartRef { cells: &shared, from: record::COUNT_MIN_AT });
        for (index, waits) in self.waits.iter_mut().enumerate() {
            waits.merge_cells(&PartRef { cells: &shared, from: record::wait_at(index) });
        }
        record::listed(cells, |key, exact| match exact {
            Some(exact) => {
                self.listed.insert((ring, key), exact);
            }
            None => {
                self.torn += 1;
                self.listed.entry((ring, key)).or_insert(0);
            }
        });
    }

    /// What was counted after `earlier`.
    fn since(mut self, earlier: &Summed) -> Summed {
        self.spans = self.spans.saturating_sub(earlier.spans);
        self.count_min.since(&earlier.count_min);
        for (it, before) in self.waits.iter_mut().zip(&earlier.waits) {
            it.since(before);
        }
        // A key listed at both reads counted the ticks between them; one listed again since
        // counted only ticks since, so the difference stays under the truth either way.
        for (at, exact) in &mut self.listed {
            *exact = exact.saturating_sub(earlier.listed.get(at).copied().unwrap_or(0));
        }
        self
    }
}

/// The sums at the ends of the last two builds: the one before the last (`None`: the process's
/// start) and the last.
static MARKS: Mutex<Option<(Option<Summed>, Summed)>> = Mutex::new(None);

/// A build ended: what [`Since::LastBuild`] reads from now on ends here. Reads every ring once.
pub(crate) fn mark() {
    let now = Summed::now();
    let mut marks = MARKS.lock();
    let before = marks.take().map(|(_, last)| last);
    *marks = Some((before, now));
}

/// The `top` keys that took most and the quantiles of each kind of wait, over `since`.
pub fn read(top: usize, since: Since) -> Result<Hot, HotError> {
    let summed = match since {
        Since::Start => Summed::now(),
        Since::LastBuild => {
            let marks = MARKS.lock();
            let (before, last) = marks.as_ref().ok_or(HotError::NoBuild)?;
            let last = copy(last);
            match before {
                Some(before) => last.since(before),
                None => last,
            }
        }
    };
    Ok(hot(&summed, top, since))
}

/// A copy of `summed`, to subtract from without holding [`MARKS`].
fn copy(summed: &Summed) -> Summed {
    let mut copied = Summed::empty();
    copied.spans = summed.spans;
    copied.count_min.merge(&summed.count_min);
    for (it, from) in copied.waits.iter_mut().zip(&summed.waits) {
        it.merge(from);
    }
    // clone: a read's copy of the listed keys, once a read
    copied.listed = summed.listed.clone();
    copied.torn = summed.torn;
    copied
}

fn hot(summed: &Summed, top: usize, since: Since) -> Hot {
    let scale = clock::Scale::now();
    let mut lower: HashMap<u64, u64> = HashMap::default();
    for (&(_, key), exact) in &summed.listed {
        let it = lower.entry(key).or_default();
        *it = it.saturating_add(*exact);
    }
    // Keys whose names are one (a name given a new number in a later generation) are summed.
    let mut named: HashMap<(&'static str, Function), (u64, u64)> = HashMap::default();
    for (key, lower) in lower {
        let (site, function) = record::of(key);
        let Some(meta) = flight::meta(site) else { continue };
        let function = match function {
            0 => Function::None,
            sym => flight::name(sym).map_or(Function::Forgotten(sym), Function::Named),
        };
        let upper = summed.count_min.estimate(key);
        let it = named.entry((meta.name(), function)).or_default();
        *it = (it.0.saturating_add(upper), it.1.saturating_add(lower.min(upper)));
    }
    // alloc: a read's rows, once a read
    let mut keys: Vec<HotKey> = named
        .into_iter()
        .map(|((span, function), (upper, lower))| HotKey {
            span,
            function,
            upper_ns: scale.ns(upper),
            lower_ns: scale.ns(lower),
        })
        .collect();
    keys.sort_by(|a, b| {
        b.upper_ns.cmp(&a.upper_ns).then_with(|| a.span.cmp(b.span)).then_with(|| a.function.cmp(&b.function))
    });
    keys.truncate(top);
    let ns = |ticks: f64| scale.ns(ticks.round() as u64);
    let waits = WaitKind::ALL
        .iter()
        .zip(&summed.waits)
        .filter_map(|(&kind, sketch)| {
            let at = |q| sketch.quantile(q).map(ns);
            Some(HotWait {
                kind,
                count: sketch.count(),
                p50_ns: at(Quantile::P50)?,
                p90_ns: at(Quantile::P90)?,
                p99_ns: at(Quantile::P99)?,
                max_ns: at(Quantile::MAX)?,
            })
        })
        // alloc: a read's rows, once a read
        .collect();
    Hot {
        since,
        spans: summed.spans,
        total_ns: scale.ns(summed.count_min.total()),
        error_ns: scale.ns(summed.count_min.error_bound()),
        keys,
        waits,
        torn: summed.torn,
    }
}
