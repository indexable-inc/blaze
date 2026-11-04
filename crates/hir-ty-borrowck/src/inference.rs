//! Region inference: what each region holds, from where liveness says it is live and the
//! outlives constraints between regions, as rustc's `RegionInferenceContext` computes it on
//! the strongly connected components of the constraint graph.

use super::{
    bitset::{BitMatrix, BitSet, bits, matrix},
    intervals::IntervalSet,
    regions::{Outlives, Regions, Rv, UniversalKind},
    relation::Csr,
};
use crate::mem::{self, FVec, Frame, filled};

/// The points each region holds at, and the universal regions it outlives: one value for each
/// strongly connected component of the outlives graph, whose regions hold the same. The
/// universals are a bit matrix, a row of `universals / 64` words per component.
pub(super) struct RegionValues<'f> {
    scc: FVec<'f, u32>,
    points: FVec<'f, IntervalSet<'f>>,
    universals: BitMatrix<'f>,
    /// The components of a placeholder a higher-ranked relate assumes bounds of, which give
    /// what outlives them only their own universal regions.
    barriers: BitSet<'f>,
}

impl<'f> RegionValues<'f> {
    pub(super) fn contains_point(&self, rv: Rv, point: usize) -> bool {
        self.points[self.scc[rv] as usize].contains(point)
    }

    pub(super) fn points_of(&self, rv: Rv) -> &IntervalSet<'f> {
        &self.points[self.scc[rv] as usize]
    }

    pub(super) fn universals_of(&self, rv: Rv) -> impl Iterator<Item = usize> + '_ {
        self.universals.iter_row(self.scc[rv] as usize)
    }

    /// Whether `a` and `b` outlive each other: one component of the outlives graph.
    pub(super) fn same_component(&self, a: Rv, b: Rv) -> bool {
        self.scc[a] == self.scc[b]
    }

    /// Whether what `rv` is made to outlive reaches the regions that outlive `from` through
    /// it: not past a placeholder whose bounds are assumed (`infer`), unless it is `from`'s own.
    pub(super) fn passes_on(&self, from: Rv, rv: Rv) -> bool {
        let at = self.scc[rv];
        !self.barriers.contains(at as usize) || at == self.scc[from]
    }
}

/// Answers what a higher-ranked relate makes a region of the body outlive of a placeholder
/// whose bounds its more general side assumes (`Regions::placeholder_bounds`): a region made
/// to outlive `'x`, of a target that assumes `'r: 'x`, outlives `'r`, as every call of the fn
/// pointer proves `'r` outlives the lifetime it gives `'x`. Each such `'r` becomes a
/// constraint of its own, with the place and blame of the one it answers (recorded in
/// `Regions::answered_edges`), and the placeholder is recorded in
/// `Regions::answered_placeholders`, whose points region inference and Polonius give nothing
/// that outlives it: rustc gives a placeholder no point of the body, and a body region holding
/// one would hold every point, as the borrow of a local passed to a fn pointer
/// `for<'a> fn(&'a Row<'t>) -> &'a str` copied out of a struct, whose copy relates
/// higher-ranked and proves `'t: 'a` of the copied fn pointer.
///
/// One `'r` is enough, so one answers each constraint: the bound in the constraint's own
/// position, where it is what the relate proves of its less general side
/// (`Regions::positional_answers`). The copy of `for<'a> fn(&'a Local<'t>, &'a Local<'u>)`
/// proves `'t: 'a` and `'u: 'a`; answered by every bound, each would outlive both of the
/// copy's regions, which the copy makes `'t` and `'u`, so `'t` and `'u` would have to be one
/// lifetime, which rustc never asks. Where no position says which bound answers a constraint
/// (a region of the less general side outlives `'x` in the relate itself), each bound does.
///
/// A placeholder is answered when its component of the outlives graph has, for each of its
/// placeholders, an assumed `'r: 'x` whose `'r` is neither `'static`, which outlives every
/// `'x` and says nothing of it, nor a placeholder of the same component (`answers`). An `'r`
/// of the component itself needs no constraint. The constraints added can merge components,
/// so it runs until a round adds none; each round adds at least one of the finitely many from
/// a region to an assumed `'r`, which bounds the rounds; past that it answers none and says
/// so (`Regions::answers_unsettled`).
pub(super) fn answer_placeholders<'f>(
    frame: &'f Frame,
    regions: &mut Regions<'f, '_>,
    count: usize,
) {
    if regions.placeholder_bounds.is_empty() {
        return;
    }
    let bounds: FVec<'_, (Rv, Rv)> =
        mem::collect(frame, regions.placeholder_bounds.iter().copied());
    let placeholders: FVec<'_, Rv> = mem::collect(
        frame,
        regions
            .universals
            .iter()
            .filter(|&&(rv, kind)| rv < count && kind == UniversalKind::Placeholder)
            .map(|&(rv, _)| rv),
    );
    let static_rv = regions.static_rv;
    let answers = |sup: Rv, scc: &[u32], component: u32| {
        sup < count && sup != static_rv && !(scc[sup] == component && placeholders.contains(&sup))
    };
    let mut added: FVec<'_, Outlives> = Vec::new_in(frame);
    let mut answered_edges: FVec<'_, (Rv, Rv, Rv)> = Vec::new_in(frame);
    let fuel = count * bounds.len() + 1;
    for _ in 0..fuel {
        let edges = outlives_edges(frame, regions, count);
        let (scc, sccs) = strongly_connected(frame, &edges);
        let answered =
            answered_components(frame, &bounds, &placeholders, &scc, sccs, count, &answers);
        added.clear();
        answered_edges.clear();
        for at in 0..regions.outlives.len() {
            let (from, to) = (regions.outlives.sup(at), regions.outlives.sub(at));
            if from >= count || to >= count || scc[from] == scc[to] {
                continue;
            }
            let component = scc[to];
            if !answered.contains(component as usize) {
                continue;
            }
            let mut add = |sup: Rv| {
                if scc[sup] != component
                    && !edges.of(from).contains(&(sup as u32))
                    && !added.iter().any(|it| it.sup == from && it.sub == sup)
                {
                    let mut it = regions.outlives.get(at);
                    it.sub = sup;
                    it.from_closure = false;
                    added.push(it);
                    answered_edges.push((from, sup, to));
                }
            };
            for &placeholder in placeholders.iter() {
                if scc[placeholder] != component || !bounds.iter().any(|it| it.1 == placeholder) {
                    continue;
                }
                let positional = regions.positional_answers.iter().find(|it| {
                    it.sup == from
                        && it.sub == to
                        && it.placeholder == placeholder
                        && answers(it.answer, &scc, component)
                });
                match positional {
                    Some(it) => add(it.answer),
                    None => {
                        for &(sup, sub) in bounds.iter() {
                            if sub == placeholder && answers(sup, &scc, component) {
                                add(sup);
                            }
                        }
                    }
                }
            }
        }
        if added.is_empty() {
            for &rv in placeholders.iter() {
                if answered.contains(scc[rv] as usize) {
                    regions.answered_placeholders.push(rv);
                }
            }
            return;
        }
        regions.outlives.extend(added.iter().copied());
        regions.answered_edges.extend(answered_edges.iter().copied());
    }
    regions.answers_unsettled = Some(fuel);
}

/// The components of the outlives graph (`scc`) whose every placeholder with assumed bounds
/// has one that answers it (`answer_placeholders`), and that have one.
fn answered_components<'f>(
    frame: &'f Frame,
    bounds: &[(Rv, Rv)],
    placeholders: &[Rv],
    scc: &[u32],
    sccs: usize,
    count: usize,
    answers: impl Fn(Rv, &[u32], u32) -> bool,
) -> BitSet<'f> {
    let mut answered = bits(frame, sccs);
    let mut unanswered = bits(frame, sccs);
    for &rv in placeholders {
        if !bounds.iter().any(|&(sup, sub)| (sup == rv || sub == rv) && sup < count && sub < count)
        {
            continue;
        }
        let at = scc[rv];
        let assumed = bounds.iter().any(|&(sup, sub)| sub == rv && answers(sup, scc, at));
        match assumed {
            true => answered.insert(at as usize),
            false => unanswered.insert(at as usize),
        };
    }
    for at in 0..sccs {
        if unanswered.contains(at) {
            answered.remove(at);
        }
    }
    answered
}

/// Region inference: the values liveness gives grown along the outlives constraints until
/// each region holds wherever a region it outlives does. As rustc's, it works on the strongly
/// connected components of the graph, each once, those it outlives first.
pub(super) fn infer<'f>(
    frame: &'f Frame,
    regions: &Regions<'_, '_>,
    count: usize,
    points: usize,
    live: &[IntervalSet<'_>],
) -> RegionValues<'f> {
    let edges = outlives_edges(frame, regions, count);
    let (scc, sccs) = strongly_connected(frame, &edges);
    // A placeholder whose bounds the more general side of a higher-ranked relate assumes
    // (`Regions::placeholder_bounds`) gives what outlives it only itself and the universal
    // regions of its own component, not what it is made to outlive: `'r: 'x` and `'x: 's`,
    // each held by the target's `'r: 'x` and `'x: 's`, are not `'r: 's`, as `'x` stands for
    // each lifetime between the two, and for none when there is none. What `'x` is made to
    // outlive is checked against `'x` itself.
    let placeholder = |rv: Rv| {
        rv < count
            && regions
                .universal(rv)
                .is_some_and(|it| regions.universals[it].1 == UniversalKind::Placeholder)
    };
    let mut barriers = bits(frame, sccs);
    for &(sup, sub) in regions.placeholder_bounds.iter() {
        for rv in [sup, sub] {
            if placeholder(rv) {
                barriers.insert(scc[rv] as usize);
            }
        }
    }
    // The components of answered placeholders (`answer_placeholders`), which give what
    // outlives them neither the placeholder nor its points, only what the rest of the
    // component holds (`outward`): the placeholder's assumed bounds stand for it. The
    // component's own value (`values`) still holds every point, as a placeholder's does.
    let mut answered = bits(frame, 0);
    let mut outward = Vec::new_in(frame);
    if !regions.answered_placeholders.is_empty() {
        answered = bits(frame, sccs);
        for &rv in regions.answered_placeholders.iter() {
            answered.insert(scc[rv] as usize);
        }
        for &(sup, sub) in regions.placeholder_bounds.iter() {
            for rv in [sup, sub] {
                if placeholder(rv) && !regions.answered_placeholders.contains(&rv) {
                    answered.remove(scc[rv] as usize);
                }
            }
        }
        outward = filled(frame, sccs, IntervalSet::new(frame));
    }
    // Tarjan's algorithm numbers a component after every component it reaches.
    let mut values = filled(frame, sccs, IntervalSet::new(frame));
    let mut universals = matrix(frame, sccs, regions.universals.len());
    for (rv, it) in live.iter().enumerate() {
        values[scc[rv] as usize].union(it);
        if answered.contains(scc[rv] as usize) {
            outward[scc[rv] as usize].union(it);
        }
    }
    for (index, &(rv, kind)) in regions.universals.iter().enumerate() {
        if rv >= count {
            continue;
        }
        let at = scc[rv] as usize;
        if points > 0 {
            values[at].insert_range(0, points - 1);
            if answered.contains(at) && kind != UniversalKind::Placeholder {
                outward[at].insert_range(0, points - 1);
            }
        }
        universals.insert(at, index);
    }
    let members = Csr::build(frame, sccs, scc.iter().enumerate().map(|(rv, &at)| (at, rv as u32)));
    let seen = frame.alloc_slice_fill_with(sccs, |_| u32::MAX);
    for at in 0..sccs {
        for &rv in members.of(at) {
            for &sub in edges.of(rv as usize) {
                let from = scc[sub as usize] as usize;
                if from == at || seen[from] == at as u32 {
                    continue;
                }
                seen[from] = at as u32;
                debug_assert!(from < at);
                let (done, rest) = values.split_at_mut(at);
                if answered.contains(from) {
                    rest[0].union(&outward[from]);
                    if answered.contains(at) {
                        let (done, rest) = outward.split_at_mut(at);
                        rest[0].union(&done[from]);
                    }
                    for (index, &(rv, kind)) in regions.universals.iter().enumerate() {
                        if rv < count
                            && scc[rv] as usize == from
                            && kind != UniversalKind::Placeholder
                        {
                            universals.insert(at, index);
                        }
                    }
                    continue;
                }
                rest[0].union(&done[from]);
                if answered.contains(at) {
                    outward[at].union(&done[from]);
                }
                if !barriers.contains(from) {
                    universals.union_rows(at, from);
                    continue;
                }
                for (index, &(rv, _)) in regions.universals.iter().enumerate() {
                    if rv < count && scc[rv] as usize == from {
                        universals.insert(at, index);
                    }
                }
            }
        }
    }
    RegionValues { scc, points: values, universals, barriers }
}

/// The outlives graph: `sup: sub` makes `sup` hold what `sub` does, an edge from `sup` to
/// `sub`, appended in `sup` order, so no sort.
fn outlives_edges<'f>(frame: &'f Frame, regions: &Regions<'_, '_>, count: usize) -> Csr<'f> {
    let mut edges = Csr::with_capacity(frame, count, regions.outlives.len());
    for sup in 0..count {
        for sub in regions.outlives.subs_of(count, sup) {
            edges.push(sub as u32);
        }
        edges.end_node();
    }
    edges
}

/// The strongly connected components of a graph, by Tarjan's algorithm without recursion: the
/// component of each node, numbered so that every component a component reaches has a
/// smaller number, and how many there are.
fn strongly_connected<'f>(frame: &'f Frame, edges: &Csr<'_>) -> (FVec<'f, u32>, usize) {
    let count = edges.nodes();
    const NONE: u32 = u32::MAX;
    let index = frame.alloc_slice_fill_with(count, |_| NONE);
    let low = frame.alloc_slice_fill_with(count, |_| 0u32);
    let mut on_stack = bits(frame, count);
    let mut scc = filled(frame, count, NONE);
    // Each node is on each stack at most once, so `count` bounds both.
    let mut stack: FVec<'_, u32> = Vec::with_capacity_in(count, frame);
    let mut next_index = 0u32;
    let mut sccs = 0u32;
    // (node, the next of its edges to follow)
    let mut calls: FVec<'_, (u32, usize)> = Vec::with_capacity_in(count, frame);
    for root in 0..count as u32 {
        if index[root as usize] != NONE {
            continue;
        }
        calls.push((root, 0));
        index[root as usize] = next_index;
        low[root as usize] = next_index;
        next_index += 1;
        stack.push(root);
        on_stack.insert(root as usize);
        while let Some(&mut (node, ref mut next)) = calls.last_mut() {
            let n = node as usize;
            if let Some(&to) = edges.of(n).get(*next) {
                *next += 1;
                let t = to as usize;
                if index[t] == NONE {
                    index[t] = next_index;
                    low[t] = next_index;
                    next_index += 1;
                    stack.push(to);
                    on_stack.insert(t);
                    calls.push((to, 0));
                } else if on_stack.contains(t) {
                    low[n] = low[n].min(index[t]);
                }
                continue;
            }
            calls.pop();
            if let Some(&(parent, _)) = calls.last() {
                low[parent as usize] = low[parent as usize].min(low[n]);
            }
            if low[n] == index[n] {
                loop {
                    let member = stack.pop().unwrap();
                    on_stack.remove(member as usize);
                    scc[member as usize] = sccs;
                    if member == node {
                        break;
                    }
                }
                sccs += 1;
            }
        }
    }
    (scc, sccs as usize)
}
