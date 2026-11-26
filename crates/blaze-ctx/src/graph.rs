//! Typed repository concept graph and 1-hop neighbor expansion.
//!
//! Connects crates, design docs, and agent notes across five typed edge relations:
//! - `designed_by`: `crate -> doc`
//! - `implemented_in`: `doc -> crate`
//! - `depends_on`: `crate -> crate`
//! - `annoyances_for`: `note -> crate | doc`
//! - `references_doc`: `doc -> doc`

use blaze_own::{ABTreeMap, ABTreeSet, AString, AVec, Alloc, format_in};

use crate::tree::{AgentNoteNode, CrateNode, DocNode};

/// Typed relation between two Level-1 concept nodes.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum EdgeKind {
    /// Crate is specified/designed by a design document (`crate -> doc`).
    DesignedBy,
    /// Design document is implemented in a crate (`doc -> crate`).
    ImplementedIn,
    /// Crate depends on another workspace crate (`crate -> crate`).
    DependsOn,
    /// Agent annoyance or howto applies to a crate or doc (`note -> crate | doc`).
    AnnoyancesFor,
    /// Document links to another document (`doc -> doc`).
    ReferencesDoc,
}

impl EdgeKind {
    /// Canonical lowercase label used in `blaze-tab`, JSON, and CBOR.
    pub const fn as_str(self) -> &'static str {
        match self {
            EdgeKind::DesignedBy => "designed_by",
            EdgeKind::ImplementedIn => "implemented_in",
            EdgeKind::DependsOn => "depends_on",
            EdgeKind::AnnoyancesFor => "annoyances_for",
            EdgeKind::ReferencesDoc => "references_doc",
        }
    }

    /// Parses a canonical edge label.
    pub fn from_label(s: &str) -> Option<Self> {
        match s {
            "designed_by" => Some(EdgeKind::DesignedBy),
            "implemented_in" => Some(EdgeKind::ImplementedIn),
            "depends_on" => Some(EdgeKind::DependsOn),
            "annoyances_for" => Some(EdgeKind::AnnoyancesFor),
            "references_doc" => Some(EdgeKind::ReferencesDoc),
            _ => None,
        }
    }

    /// Score decay multiplier when traversing this edge in 1-hop expansion.
    pub const fn expansion_weight(self) -> f64 {
        match self {
            EdgeKind::DesignedBy | EdgeKind::ImplementedIn => 0.65,
            EdgeKind::AnnoyancesFor => 0.55,
            EdgeKind::ReferencesDoc => 0.45,
            EdgeKind::DependsOn => 0.35,
        }
    }
}

/// Directed typed edge in the concept graph.
pub struct GraphEdge<'a> {
    pub from: AString<'a>,
    pub to: AString<'a>,
    pub kind: EdgeKind,
}

impl<'a> GraphEdge<'a> {
    pub fn clone_in<'b>(&self, alloc: Alloc<'b>) -> GraphEdge<'b> {
        GraphEdge {
            from: AString::from_str_in(self.from.as_str(), alloc),
            to: AString::from_str_in(self.to.as_str(), alloc),
            kind: self.kind,
        }
    }
}

/// A neighbor surfaced by 1-hop graph expansion from a seed hit.
pub struct ExpandedNeighbor<'a> {
    pub node_id: AString<'a>,
    pub score: f64,
    pub edge_kind: EdgeKind,
    /// Human-readable provenance, e.g. `designed_by(crate:blaze-ty)`.
    pub via: AString<'a>,
}

/// Builds the deduplicated, deterministically sorted graph edges from Level-1 nodes.
pub(crate) fn build_edges<'a>(
    crates: &[CrateNode<'_>],
    docs: &[DocNode<'_>],
    notes: &[AgentNoteNode<'_>],
    alloc: Alloc<'a>,
) -> AVec<'a, GraphEdge<'a>> {
    // Lookup tables of valid node IDs.
    let mut crate_ids: ABTreeMap<'a, AString<'a>, AString<'a>> = ABTreeMap::new_in(alloc);
    for c in crates {
        crate_ids.insert(
            AString::from_str_in(c.name.as_str(), alloc),
            AString::from_str_in(c.id.as_str(), alloc),
        );
    }
    let mut doc_ids: ABTreeMap<'a, AString<'a>, AString<'a>> = ABTreeMap::new_in(alloc);
    for d in docs {
        doc_ids.insert(
            AString::from_str_in(d.path.as_str(), alloc),
            AString::from_str_in(d.id.as_str(), alloc),
        );
    }

    // Deduplicate using a sorted set of (from, to, kind).
    let mut seen: ABTreeSet<'a, (AString<'a>, AString<'a>, EdgeKind)> = ABTreeSet::new_in(alloc);
    let mut add = |from: &str, to: &str, kind: EdgeKind| {
        if from != to {
            seen.insert((
                AString::from_str_in(from, alloc),
                AString::from_str_in(to, alloc),
                kind,
            ));
        }
    };

    // 1. Crate -> dep (`depends_on`) and Crate <-> design doc (`designed_by` / `implemented_in`).
    for c in crates {
        for dep in c.deps.as_slice() {
            if let Some(target_id) = crate_ids.get(dep.as_str()) {
                add(c.id.as_str(), target_id.as_str(), EdgeKind::DependsOn);
            }
        }
        for doc_path in c.design_docs.as_slice() {
            if let Some(doc_id) = doc_ids.get(doc_path.as_str()) {
                add(c.id.as_str(), doc_id.as_str(), EdgeKind::DesignedBy);
                add(doc_id.as_str(), c.id.as_str(), EdgeKind::ImplementedIn);
            }
        }
        // Heuristic naming convention: `crates/blaze-<stem>` <-> `docs/v2/plan/design/<stem>.md`.
        if let Some(stem) = c.name.as_str().strip_prefix("blaze-") {
            let candidate = format_in!(alloc, "docs/v2/plan/design/{stem}.md");
            if let Some(doc_id) = doc_ids.get(candidate.as_str()) {
                add(c.id.as_str(), doc_id.as_str(), EdgeKind::DesignedBy);
                add(doc_id.as_str(), c.id.as_str(), EdgeKind::ImplementedIn);
            }
        }
    }

    // 2. Doc -> Crate (`implemented_in` + inverse `designed_by`) and Doc -> Doc (`references_doc`).
    for d in docs {
        for crate_name in d.mentioned_crates.as_slice() {
            if let Some(crate_id) = crate_ids.get(crate_name.as_str()) {
                add(d.id.as_str(), crate_id.as_str(), EdgeKind::ImplementedIn);
                add(crate_id.as_str(), d.id.as_str(), EdgeKind::DesignedBy);
            }
        }
        for linked in d.linked_docs.as_slice() {
            if let Some(target_id) = doc_ids.get(linked.as_str()) {
                add(d.id.as_str(), target_id.as_str(), EdgeKind::ReferencesDoc);
            }
        }
    }

    // 3. AgentNote -> Crate | Doc (`annoyances_for`).
    for n in notes {
        for tag in n.tags.as_slice() {
            let t = tag.as_str();
            if let Some(crate_id) = crate_ids.get(t) {
                add(n.id.as_str(), crate_id.as_str(), EdgeKind::AnnoyancesFor);
            } else if let Some(doc_id) = doc_ids.get(t) {
                add(n.id.as_str(), doc_id.as_str(), EdgeKind::AnnoyancesFor);
            }
        }
    }

    let mut edges = AVec::with_capacity_in(seen.len(), alloc);
    for (from, to, kind) in seen {
        edges.push(GraphEdge { from, to, kind });
    }
    edges
}

/// Expands top seed hits by 1 hop across `edges` (both outgoing and incoming links).
///
/// Nodes already present in `seed_ids` are excluded from the returned expansion list.
/// When multiple seeds reach the same neighbor, the strongest path wins deterministically.
pub fn expand_1hop<'a>(
    edges: &[GraphEdge<'_>],
    seeds: &[(&str, f64)],
    max_neighbors: usize,
    alloc: Alloc<'a>,
) -> AVec<'a, ExpandedNeighbor<'a>> {
    if seeds.is_empty() || max_neighbors == 0 {
        return AVec::new_in(alloc);
    }

    let mut seed_scores: ABTreeMap<'a, AString<'a>, f64> = ABTreeMap::new_in(alloc);
    for &(id, score) in seeds {
        let key = AString::from_str_in(id, alloc);
        if let Some(prev) = seed_scores.get_mut(id) {
            if score > *prev {
                *prev = score;
            }
        } else {
            seed_scores.insert(key, score);
        }
    }

    // Best candidate per non-seed neighbor ID: (score, edge_kind, via).
    let mut best: ABTreeMap<'a, AString<'a>, (f64, EdgeKind, AString<'a>)> = ABTreeMap::new_in(alloc);

    let mut consider = |neighbor: &str, seed_id: &str, seed_score: f64, kind: EdgeKind, dir_label: &str| {
        if seed_scores.contains_key(neighbor) {
            return;
        }
        let cand_score = seed_score * kind.expansion_weight();
        let via = format_in!(alloc, "{dir_label}({seed_id})");
        if let Some(entry) = best.get_mut(neighbor) {
            if cand_score > entry.0 || (cand_score == entry.0 && via.as_str() < entry.2.as_str()) {
                *entry = (cand_score, kind, via);
            }
        } else {
            best.insert(AString::from_str_in(neighbor, alloc), (cand_score, kind, via));
        }
    };

    for edge in edges {
        let from = edge.from.as_str();
        let to = edge.to.as_str();
        if let Some(&s) = seed_scores.get(from) {
            consider(to, from, s, edge.kind, edge.kind.as_str());
        }
        if let Some(&s) = seed_scores.get(to) {
            // Traverse incoming edges too (e.g. crate/doc seed pulls in `annoyances_for` notes and dependents).
            let label = match edge.kind {
                EdgeKind::AnnoyancesFor => "annoyance_of",
                EdgeKind::DependsOn => "depended_by",
                EdgeKind::DesignedBy => "designs",
                EdgeKind::ImplementedIn => "implemented_by",
                EdgeKind::ReferencesDoc => "referenced_by",
            };
            consider(from, to, s, edge.kind, label);
        }
    }

    let mut out = AVec::with_capacity_in(best.len(), alloc);
    for (node_id, (score, edge_kind, via)) in best {
        out.push(ExpandedNeighbor { node_id, score, edge_kind, via });
    }
    out.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.node_id.as_str().cmp(b.node_id.as_str()))
    });
    if out.len() > max_neighbors {
        out.truncate(max_neighbors);
    }
    out
}
