//! Profile-guided function order: hot functions packed together in call order, cold ones
//! last (docs/v2/plan/design/code-layout.md).
//!
//! A full link sorts each code class's functions in three tiers, by the middle word of their
//! sort key ([`crate::place`]):
//!
//! 1. the functions a profile saw run, in the order C3 gives (Ottoni and Maher, "Optimizing
//!    Function Placement for Large-Scale Data-Center Applications", CGO 2017), as lld's
//!    `--call-graph-profile-sort` does it (`lld/ELF/CallGraphSort.cpp`): each function joins
//!    its heaviest caller's cluster unless that call is under a tenth of its weight, the
//!    cluster would pass 1 MiB, or the merge would thin the cluster eightfold; clusters go
//!    densest first;
//! 2. every other function, by content as without a profile;
//! 3. the functions codegen marked cold ([`crate::Change::cold`]) that the profile did not see
//!    run, by content. The profile wins: a function it saw run is hot whatever codegen
//!    inferred.
//!
//! Rust leaves where a function's code is, and the order of functions, unspecified: the
//! Reference ("Function pointer types") gives a function pointer no address guarantee, and
//! `core::ptr::fn_addr_eq` documents that one function may have several addresses and two
//! functions one. So no program can depend on this order, and its behaviour is the same.
//!
//! The order is a function of the functions' instance keys and code sizes, the heat and the
//! cold set alone: every tie falls to instance keys and densities compare as exact products,
//! so the same tree and profile give the same image whatever order anything came in.

use std::cmp::Ordering;

use blaze_codegen::InstanceKey;
use blaze_ids::{ContentId, Hasher};

/// The largest cluster C3 grows, in bytes: lld's `MAX_CLUSTER_SIZE`. Past it a merge no
/// longer keeps a call path within a few pages.
const MAX_CLUSTER: u64 = 1 << 20;
/// How far a merge may thin a cluster: lld's `MAX_DENSITY_DEGRADATION`.
const MAX_DEGRADATION: u128 = 8;
/// No caller.
const NONE: u32 = u32::MAX;

/// The middle sort key of a function the profile did not see and codegen did not mark cold:
/// after every hot one (whose keys are their places, below 2^32); ties by content.
pub(crate) const NEUTRAL: u64 = 1 << 32;
/// The middle sort key of a cold function: after every other function; ties by content.
pub(crate) const COLD: u64 = 2 << 32;

/// A profile of the program an image is, mapped onto its functions: each function's own
/// samples (it was running) and each caller-to-callee pair's (the callee was running, called
/// from the caller). What [`crate::Change::heat`] carries. Its [`Heat::id`] covers every
/// weight, so the image's lineage names the profile its order came from.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Heat {
    id: ContentId,
    functions: Vec<(InstanceKey, u64)>,
    calls: Vec<(InstanceKey, InstanceKey, u64)>,
}

impl Heat {
    /// Samples in any order, summed per function and per pair; weights of 0 dropped.
    pub fn new(
        functions: impl IntoIterator<Item = (InstanceKey, u64)>,
        calls: impl IntoIterator<Item = (InstanceKey, InstanceKey, u64)>,
    ) -> Heat {
        // alloc: once per profile, the heat its caller keeps across links
        let mut functions: Vec<(InstanceKey, u64)> =
            functions.into_iter().filter(|it| it.1 != 0).collect();
        functions.sort_unstable_by_key(|it| it.0);
        functions.dedup_by(|later, kept| {
            let same = later.0 == kept.0;
            if same {
                kept.1 = kept.1.saturating_add(later.1);
            }
            same
        });
        // alloc: once per profile, as above
        let mut calls: Vec<(InstanceKey, InstanceKey, u64)> =
            calls.into_iter().filter(|it| it.2 != 0).collect();
        calls.sort_unstable_by_key(|it| (it.0, it.1));
        calls.dedup_by(|later, kept| {
            let same = (later.0, later.1) == (kept.0, kept.1);
            if same {
                kept.2 = kept.2.saturating_add(later.2);
            }
            same
        });
        let mut hasher = Hasher::new("blaze v2 link heat 1");
        hasher.update(&(functions.len() as u64).to_le_bytes());
        for (key, weight) in &functions {
            hasher.id(&key.0).update(&weight.to_le_bytes());
        }
        hasher.update(&(calls.len() as u64).to_le_bytes());
        for (from, to, weight) in &calls {
            hasher.id(&from.0).id(&to.0).update(&weight.to_le_bytes());
        }
        Heat { id: hasher.finish(), functions, calls }
    }

    /// The content id of every weight: what the image's lineage takes of the profile.
    pub fn id(&self) -> ContentId {
        self.id
    }

    /// Each function's own samples, by instance key.
    pub fn functions(&self) -> &[(InstanceKey, u64)] {
        &self.functions
    }

    /// Each caller-to-callee pair's samples, by caller then callee.
    pub fn calls(&self) -> &[(InstanceKey, InstanceKey, u64)] {
        &self.calls
    }

    pub fn is_empty(&self) -> bool {
        self.functions.is_empty() && self.calls.is_empty()
    }
}

/// What a link's order is made from: the heat and the cold set its [`crate::Change`] gave,
/// the cold set sorted and once each, and the order once the first layout made it.
pub(crate) struct Pass<'a> {
    pub heat: Option<&'a Heat>,
    pub cold: &'a [InstanceKey],
    /// `None` until the stage's first layout; then `Some(None)` where the pass does not apply.
    pub order: Option<Option<Order>>,
}

impl<'a> Pass<'a> {
    pub(crate) fn new(heat: Option<&'a Heat>, cold: &'a [InstanceKey]) -> Pass<'a> {
        Pass { heat, cold, order: None }
    }

    /// The content id of what the order is made from, for the image's lineage: the heat's id
    /// (or none) and the cold set's keys in order.
    pub(crate) fn id(&self) -> ContentId {
        let mut hasher = Hasher::new("blaze v2 link layout 1");
        hasher.opt_part(self.heat.map(|it| it.id().0));
        hasher.update(&(self.cold.len() as u64).to_le_bytes());
        for key in self.cold {
            hasher.id(&key.0);
        }
        hasher.finish()
    }

    /// Whether anything asks for an order other than by content.
    pub(crate) fn wanted(&self) -> bool {
        self.heat.is_some_and(|it| !it.is_empty()) || !self.cold.is_empty()
    }
}

/// What the order did, for its span.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(crate) struct Summary {
    /// The functions ordered.
    pub functions: u64,
    /// The functions the profile saw run, placed first.
    pub hot: u64,
    /// The clusters C3 left them in.
    pub clusters: u64,
    /// The functions marked cold and not seen run, placed last.
    pub cold: u64,
    /// The functions marked cold that the profile saw run: placed hot.
    pub overruled: u64,
    /// The profile's functions and calls naming a function the image does not have.
    pub unmatched: u64,
}

/// Each function's middle sort key: its place for a hot one, [`COLD`] for a cold one,
/// [`NEUTRAL`] for the rest.
#[derive(Debug)]
pub(crate) struct Order {
    /// The hot and cold functions' keys, by instance key.
    keys: Vec<(InstanceKey, u64)>,
    pub summary: Summary,
}

impl Order {
    pub(crate) fn key(&self, function: InstanceKey) -> u64 {
        match self.keys.binary_search_by_key(&function, |it| it.0) {
            Ok(at) => self.keys[at].1,
            Err(_) => NEUTRAL,
        }
    }
}

/// Whether `a` (weight, size) is denser than `b`, as exact products: sizes are above 0.
fn density(a: (u64, u64), b: (u64, u64)) -> Ordering {
    (u128::from(a.0) * u128::from(b.1)).cmp(&(u128::from(b.0) * u128::from(a.1)))
}

/// The leader of `at`'s cluster, halving the path as it goes.
fn leader_of(leader: &mut [u32], mut at: u32) -> u32 {
    while leader[at as usize] != at {
        leader[at as usize] = leader[leader[at as usize] as usize];
        at = leader[at as usize];
    }
    at
}

/// The order of `functions` (instance key and code size, any order, keys once each) given
/// the heat and the cold set (sorted, once each).
pub(crate) fn order(
    functions: &mut [(InstanceKey, u64)],
    heat: Option<&Heat>,
    cold: &[InstanceKey],
) -> Order {
    functions.sort_unstable_by_key(|it| it.0);
    let n = functions.len();
    let index = |key: &InstanceKey| functions.binary_search_by_key(key, |it| it.0).ok();
    let mut summary = Summary { functions: n as u64, ..Summary::default() };
    // alloc: once per full link with a profile or a cold set, C3's columns over the functions
    let mut profiled = vec![false; n];
    let mut weight = vec![0u64; n];
    let mut best: Vec<(u32, u64)> = vec![(NONE, 0); n];
    if let Some(heat) = heat {
        for (key, samples) in heat.functions() {
            match index(key) {
                Some(at) => {
                    profiled[at] = true;
                    weight[at] = weight[at].saturating_add(*samples);
                }
                None => summary.unmatched += 1,
            }
        }
        // By caller then callee, so of a callee's heaviest callers the first is the least.
        for (from, to, samples) in heat.calls() {
            let (Some(from), Some(to)) = (index(from), index(to)) else {
                summary.unmatched += 1;
                continue;
            };
            profiled[from] = true;
            profiled[to] = true;
            // A function of no size is in no cluster: neither end of a merge.
            if from == to || functions[from].1 == 0 || functions[to].1 == 0 {
                continue;
            }
            if best[to].0 == NONE || best[to].1 < *samples {
                best[to] = (from as u32, *samples);
            }
        }
    }
    let initial = weight.clone();
    // alloc: as above
    let mut size: Vec<u64> = functions.iter().map(|it| it.1).collect();
    let mut next: Vec<u32> = (0..n as u32).collect();
    let mut prev = next.clone();
    let mut leader = next.clone();
    let nodes: Vec<u32> =
        (0..n as u32).filter(|at| profiled[*at as usize] && size[*at as usize] > 0).collect();
    let mut sorted = nodes.clone();
    // Densest first; a stable sort, so equal densities keep instance-key order.
    sorted.sort_by(|a, b| {
        density((weight[*b as usize], size[*b as usize]), (weight[*a as usize], size[*a as usize]))
    });
    for at in sorted {
        let l = at as usize;
        let (from, edge) = best[l];
        // An unlikely call: under a tenth of what the callee weighs.
        if from == NONE || u128::from(edge) * 10 <= u128::from(initial[l]) {
            continue;
        }
        let p = leader_of(&mut leader, from) as usize;
        if p == l || size[l] + size[p] > MAX_CLUSTER {
            continue;
        }
        // The merged cluster's density under an eighth of the caller's cluster's.
        let merged = u128::from(weight[p]) + u128::from(weight[l]);
        if merged * u128::from(size[p]) * MAX_DEGRADATION
            < u128::from(weight[p]) * u128::from(size[p] + size[l])
        {
            continue;
        }
        leader[l] = p as u32;
        // `l`'s chain after `p`'s: the circular lists' tails swap.
        let (tail_p, tail_l) = (prev[p], prev[l]);
        prev[p] = tail_l;
        next[tail_l as usize] = p as u32;
        prev[l] = tail_p;
        next[tail_p as usize] = l as u32;
        size[p] += size[l];
        weight[p] = weight[p].saturating_add(weight[l]);
        size[l] = 0;
        weight[l] = 0;
    }
    // alloc: as above
    let mut clusters: Vec<u32> =
        nodes.iter().copied().filter(|at| size[*at as usize] > 0).collect();
    clusters.sort_by(|a, b| {
        density((weight[*b as usize], size[*b as usize]), (weight[*a as usize], size[*a as usize]))
    });
    // alloc: once per full link, the order the layout looks keys up in
    let mut keys: Vec<(InstanceKey, u64)> = Vec::with_capacity(nodes.len() + cold.len());
    for head in &clusters {
        let mut at = *head;
        loop {
            keys.push((functions[at as usize].0, keys.len() as u64));
            at = next[at as usize];
            if at == *head {
                break;
            }
        }
    }
    summary.hot = keys.len() as u64;
    summary.clusters = clusters.len() as u64;
    for key in cold {
        let Some(at) = index(key) else { continue };
        if profiled[at] {
            summary.overruled += 1;
        } else {
            keys.push((*key, COLD));
            summary.cold += 1;
        }
    }
    keys.sort_unstable_by_key(|it| it.0);
    Order { keys, summary }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(n: u8) -> InstanceKey {
        InstanceKey(ContentId([n; 32]))
    }

    /// The functions in the order their keys sort them, ties by instance key as content does.
    fn laid_out(
        functions: &[(InstanceKey, u64)],
        heat: Option<&Heat>,
        cold: &[InstanceKey],
    ) -> Vec<InstanceKey> {
        let mut cold = cold.to_vec();
        cold.sort_unstable();
        cold.dedup();
        let order = order(&mut functions.to_vec(), heat, &cold);
        let mut out: Vec<InstanceKey> = functions.iter().map(|it| it.0).collect();
        out.sort_by_key(|it| (order.key(*it), *it));
        out
    }

    /// A callee called mostly from one caller lands right after it, and the pair before
    /// functions the profile never saw.
    #[test]
    fn a_hot_callee_follows_its_caller() {
        let functions: Vec<_> = (1..=6).map(|n| (key(n), 64)).collect();
        let heat = Heat::new([(key(5), 100), (key(2), 90)], [(key(5), key(2), 90)]);
        let order = laid_out(&functions, Some(&heat), &[]);
        assert_eq!(&order[..2], &[key(5), key(2)]);
        assert_eq!(&order[2..], &[key(1), key(3), key(4), key(6)]);
    }

    /// Without a profile only the cold functions move, to the end, in content order; with one,
    /// a cold function the profile saw run is placed hot.
    #[test]
    fn cold_goes_last_unless_the_profile_saw_it_run() {
        let functions: Vec<_> = (1..=5).map(|n| (key(n), 32)).collect();
        assert_eq!(
            laid_out(&functions, None, &[key(4), key(2), key(4)]),
            [key(1), key(3), key(5), key(2), key(4)]
        );
        let heat = Heat::new([(key(4), 10)], []);
        assert_eq!(
            laid_out(&functions, Some(&heat), &[key(4), key(2)]),
            [key(4), key(1), key(3), key(5), key(2)]
        );
    }

    /// The same functions, heat and cold set give the same order and heat id whatever order
    /// they came in.
    #[test]
    fn inputs_in_any_order_give_one_order() {
        let functions: Vec<_> = (1..=40).map(|n| (key(n), u64::from(n % 7) * 16 + 8)).collect();
        let samples: Vec<_> = (1..=40).map(|n| (key(n), u64::from(n) * 37 % 11)).collect();
        let calls: Vec<_> = (1..40).map(|n| (key(n), key(n + 1), u64::from(n) * 13 % 9)).collect();
        let cold = [key(3), key(17), key(39)];
        let heat = Heat::new(samples.iter().copied(), calls.iter().copied());
        let reversed = Heat::new(samples.iter().rev().copied(), calls.iter().rev().copied());
        assert_eq!(heat.id(), reversed.id());
        let mut backwards = functions.clone();
        backwards.reverse();
        let mut cold_backwards = cold;
        cold_backwards.reverse();
        assert_eq!(
            laid_out(&functions, Some(&heat), &cold),
            laid_out(&backwards, Some(&reversed), &cold_backwards)
        );
    }

    /// A merge that would grow a cluster past 1 MiB is not made.
    #[test]
    fn clusters_stop_at_a_mebibyte() {
        let functions = [(key(1), MAX_CLUSTER), (key(2), 64)];
        let heat = Heat::new([(key(1), 50), (key(2), 50)], [(key(1), key(2), 50)]);
        let mut functions = functions;
        assert_eq!(order(&mut functions, Some(&heat), &[]).summary.clusters, 2);
    }
}
