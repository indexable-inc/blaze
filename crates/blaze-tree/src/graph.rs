//! The graph `cargo tree` prints: `src/ops/cargo_tree/graph.rs` of cargo 1.101.0 (cad07cf8e5),
//! on blaze-resolve's resolve and features. blaze reads no artifact dependencies, so a package
//! is built for the host or for the target asked, never for an artifact's.
//!
//! Nodes are made in the order cargo makes them, so a [`NodeId`] is the index cargo's graph
//! gives the same node, and cargo's feature walk (whose result depends on that order) is
//! followed step for step. Cargo's order of nodes, which it sorts edges by, is computed once
//! as a rank per node. Edges are collected flat while building and laid out once as CSR per
//! node and edge kind, sorted by rank and deduplicated in the same pass.

use std::cmp::Ordering;
use std::collections::hash_map::Entry;
use std::ops::Range;

use blaze_hash::{FixedState, HashMap};
use blaze_resolve::resolver::{CliFeatures, CompileKind, FeaturesFor, ResolvedFeatures};
use blaze_resolve::{
    DepKind, Dependency, FeatureMap, FeatureValue, InternedString, PackageId, Resolve, Summary,
};

use super::packages::{Pkg, Table};
use super::{Kinds, Options, Target};
use blaze_ids::bits::BitSet as Bits;

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct NodeId(u32);

impl NodeId {
    pub(crate) fn index(self) -> usize {
        self.0 as usize
    }
}

const NONE: NodeId = NodeId(u32::MAX);

/// A platform of [`Kinds`]: 0 is the host, `1 + i` the `i`th `--target` (a target given twice
/// is its first).
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) struct Kind(pub(crate) u16);

impl Kind {
    pub(crate) const HOST: Kind = Kind(0);
}

/// A range of [`Graph`]'s pool of package features.
#[derive(Debug, Copy, Clone)]
pub(crate) struct Span {
    start: u32,
    len: u32,
}

#[derive(Debug, Copy, Clone)]
pub(crate) enum Node {
    Package {
        pkg: Pkg,
        kind: Kind,
        /// Features that are enabled on this package.
        features: Span,
    },
    Feature {
        /// The package node this feature is for.
        of: NodeId,
        /// Name of the feature.
        name: InternedString,
    },
}

const _: () = assert!(size_of::<Node>() == 24);

const PUBLIC: u32 = 1 << 31;

/// An edge's target, with whether the dependency is public in its top bit.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) struct Edge(u32);

const _: () = assert!(size_of::<Edge>() == 4);

impl Edge {
    fn new(node: NodeId, public: bool) -> Edge {
        Edge(node.0 | if public { PUBLIC } else { 0 })
    }

    pub(crate) fn node(self) -> NodeId {
        NodeId(self.0 & !PUBLIC)
    }

    fn public(self) -> bool {
        self.0 & PUBLIC != 0
    }
}

/// The kind of edge, for separating dependencies into different sections.
#[derive(Debug, Copy, Hash, Eq, Clone, PartialEq)]
pub enum EdgeKind {
    Dep(DepKind),
    Feature,
}

/// An [`EdgeKind`] as the index of its section, in the order the sections print.
pub(crate) type Tag = u8;

pub(crate) const TAGS: usize = 4;
pub(crate) const NORMAL: Tag = 0;
pub(crate) const BUILD: Tag = 1;
pub(crate) const DEV: Tag = 2;
pub(crate) const FEATURE: Tag = 3;

impl EdgeKind {
    pub(crate) fn tag(self) -> Tag {
        match self {
            EdgeKind::Dep(kind) => dep_tag(kind),
            EdgeKind::Feature => FEATURE,
        }
    }
}

fn dep_tag(kind: DepKind) -> Tag {
    match kind {
        DepKind::Normal => NORMAL,
        DepKind::Build => BUILD,
        DepKind::Development => DEV,
    }
}

/// The tags of `opts.edge_kinds`, as one bit each.
fn edge_mask(opts: &Options) -> u8 {
    opts.edge_kinds.iter().fold(0, |mask, kind| mask | 1 << kind.tag())
}

/// An edge as it is added: from, kind and to.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
struct Raw {
    from: NodeId,
    to: Edge,
    tag: Tag,
}

const _: () = assert!(size_of::<Raw>() == 12);

impl Raw {
    const EMPTY: Raw = Raw { from: NodeId(0), to: Edge(0), tag: 0 };

    fn group(self) -> usize {
        self.from.index() * TAGS + self.tag as usize
    }
}

/// A package node's dependency by the name it gives it, for `dep_name/feat_name` features.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
struct DepName {
    name: InternedString,
    node: NodeId,
    optional: bool,
}

/// An interned string's identity: its pointer, as [`InternedString`]'s `Eq` compares it.
fn name_key(name: InternedString) -> usize {
    name.as_str().as_ptr() as usize
}

/// Outgoing edges of every node, CSR by node and tag.
#[derive(Default)]
struct Edges {
    /// `offsets[node * TAGS + tag]..offsets[node * TAGS + tag + 1]` is where the node's edges of
    /// `tag` are in `targets`, in cargo's node order.
    offsets: Vec<u32>,
    targets: Vec<Edge>,
}

impl Edges {
    fn range(&self, node: NodeId, tags: Range<usize>) -> Range<usize> {
        let at = node.index() * TAGS;
        self.offsets[at + tags.start] as usize..self.offsets[at + tags.end] as usize
    }
}

/// A graph of dependencies.
pub(crate) struct Graph<'a> {
    packages: &'a Table<'a>,
    nodes: Vec<Node>,
    /// The features of the package nodes, each node's a [`Span`] of it.
    features: Vec<InternedString>,
    /// Of each node, its place in cargo's order of nodes.
    rank: Vec<u32>,
    /// The package nodes in cargo's order.
    ordered: Vec<NodeId>,
    /// The edges as added, before [`Graph::lay_out`].
    raw: Vec<Raw>,
    /// [`Graph::lay_out`]'s buffers.
    sorted: Vec<Raw>,
    counts: Vec<u32>,
    edges: Edges,
    /// The feature nodes `--features` added.
    cli_features: Bits,
    /// Whether features are nodes of their own (`-e features`).
    graph_features: bool,
    /// Of each package node, its dependencies by name: a range of `dep_names`, sorted by name,
    /// then node, then optional, as cargo's `BTreeSet` of `(NodeId, bool)` per name.
    dep_ranges: Vec<(u32, u32)>,
    dep_names: Vec<DepName>,
    /// Of each package and each way it is built, its node: `slots[pkg * stride + slot]`, where
    /// slot 0 is a host dependency and `1 + kind` a normal one for `kind`.
    slots: Vec<NodeId>,
    stride: usize,
    /// The feature nodes by `(package node, name)`.
    feature_index: HashMap<(NodeId, usize), NodeId>,
    /// After [`Graph::restrict`], the nodes still in the graph.
    live: Option<Bits>,
}

impl<'a> Graph<'a> {
    fn new(packages: &'a Table<'a>, kinds: &Kinds, graph_features: bool) -> Graph<'a> {
        let len = packages.len();
        let nodes = if graph_features { 8 * len } else { len } + 16;
        let edges = if graph_features { 24 * len } else { 8 * len } + 16;
        let stride = 2 + kinds.targets.len();
        let mut slots = Vec::with_capacity(len * stride);
        slots.resize(len * stride, NONE);
        let names = if graph_features { 4 * len + 16 } else { 0 };
        Graph {
            packages,
            nodes: Vec::with_capacity(nodes),
            features: Vec::with_capacity(4 * len + 16),
            rank: Vec::with_capacity(nodes),
            ordered: Vec::with_capacity(len),
            raw: Vec::with_capacity(edges),
            sorted: Vec::with_capacity(edges),
            counts: Vec::with_capacity(TAGS * nodes + 1),
            edges: Edges { offsets: Vec::with_capacity(TAGS * nodes + 1), targets: Vec::with_capacity(edges) },
            cli_features: Bits::new(if graph_features { nodes } else { 0 }),
            graph_features,
            dep_ranges: Vec::with_capacity(if graph_features { nodes } else { 0 }),
            dep_names: Vec::with_capacity(names),
            slots,
            stride,
            feature_index: HashMap::with_capacity_and_hasher(if graph_features { nodes } else { 0 }, FixedState),
            live: None,
        }
    }

    pub(crate) fn packages(&self) -> &'a Table<'a> {
        self.packages
    }

    pub(crate) fn len(&self) -> usize {
        self.nodes.len()
    }

    pub(crate) fn node(&self, index: NodeId) -> Node {
        self.nodes[index.index()]
    }

    pub(crate) fn features(&self, span: Span) -> &[InternedString] {
        &self.features[span.start as usize..][..span.len as usize]
    }

    fn features_of(&self, node: NodeId) -> &[InternedString] {
        match self.node(node) {
            Node::Package { features, .. } => self.features(features),
            Node::Feature { .. } => panic!("unexpected feature node"),
        }
    }

    fn package_id_of(&self, node: NodeId) -> PackageId {
        match self.node(node) {
            Node::Package { pkg, .. } => self.packages.id(pkg),
            Node::Feature { .. } => panic!("unexpected feature node"),
        }
    }

    pub(crate) fn is_cli_feature(&self, index: NodeId) -> bool {
        self.cli_features.contains(index.index())
    }

    /// The nodes `from` points to by edges of `tag`, in cargo's node order.
    pub(crate) fn edges_of(&self, from: NodeId, tag: Tag) -> &[Edge] {
        let tag = tag as usize;
        &self.edges.targets[self.edges.range(from, tag..tag + 1)]
    }

    pub(crate) fn has_outgoing_edges(&self, index: NodeId) -> bool {
        !self.edges.range(index, 0..TAGS).is_empty()
    }

    fn slot(&self, pkg: Pkg, features_for: FeaturesFor, kind: Kind) -> usize {
        pkg.index() * self.stride
            + match features_for {
                FeaturesFor::HostDep => 0,
                FeaturesFor::NormalOrDev => 1 + kind.0 as usize,
            }
    }

    fn add_node(&mut self, node: Node) -> NodeId {
        let index = NodeId(match u32::try_from(self.nodes.len()) {
            Ok(index) if index < PUBLIC => index,
            _ => panic!("a tree of 2^31 nodes"),
        });
        self.nodes.push(node);
        if self.graph_features {
            self.dep_ranges.push((0, 0));
        }
        index
    }

    fn add_edge(&mut self, from: NodeId, tag: Tag, to: Edge) {
        self.raw.push(Raw { from, to, tag });
    }

    /// Adds a feature node between two nodes, `from -tag-> name -feature-> to`.
    ///
    /// Returns `(missing, index)`: whether the feature node is new, and its index.
    fn add_feature(&mut self, name: InternedString, from: Option<NodeId>, tag: Tag, to: Edge) -> (bool, NodeId) {
        // `to` *must* point to a package node.
        assert!(matches!(self.node(to.node()), Node::Package { .. }));
        let next = NodeId(self.nodes.len() as u32);
        let (missing, node_index) = match self.feature_index.entry((to.node(), name_key(name))) {
            Entry::Occupied(it) => (false, *it.get()),
            Entry::Vacant(it) => (true, *it.insert(next)),
        };
        if missing {
            let added = self.add_node(Node::Feature { of: to.node(), name });
            debug_assert_eq!(added, next);
        }
        if let Some(from) = from {
            self.add_edge(from, tag, Edge::new(node_index, to.public()));
        }
        self.add_edge(node_index, FEATURE, Edge::new(to.node(), true));
        (missing, node_index)
    }

    /// Keeps `pending[start..]` as `node`'s dependencies by name.
    fn keep_dep_names(&mut self, node: NodeId, pending: &mut Vec<DepName>, start: usize) {
        let mine = &mut pending[start..];
        mine.sort_unstable_by_key(|it| (name_key(it.name), it.node, it.optional));
        let from = self.dep_names.len();
        for &it in mine.iter() {
            if self.dep_names.len() == from || self.dep_names.last() != Some(&it) {
                self.dep_names.push(it);
            }
        }
        self.dep_ranges[node.index()] = (from as u32, self.dep_names.len() as u32);
        pending.truncate(start);
    }

    /// Where in `dep_names` the dependencies `node` names `name` are, in cargo's order.
    fn dep_names_of(&self, node: NodeId, name: InternedString) -> Range<usize> {
        let (start, end) = self.dep_ranges[node.index()];
        let (start, end) = (start as usize, end as usize);
        let names = &self.dep_names[start..end];
        let key = name_key(name);
        let lo = names.partition_point(|it| name_key(it.name) < key);
        let hi = lo + names[lo..].partition_point(|it| name_key(it.name) == key);
        start + lo..start + hi
    }

    /// Ranks every node in cargo's order of nodes: package nodes by package, features, then
    /// platform; then feature nodes by their package's node, then name. Packages come in order
    /// already, so only the few nodes of one package and the features of one node are sorted.
    fn order(&mut self, kinds: &Kinds) {
        let len = self.nodes.len();
        self.rank.clear();
        self.rank.resize(len, 0);
        self.ordered.clear();
        let mut group: Vec<NodeId> = Vec::with_capacity(self.stride);
        for pkg in 0..self.packages.len() {
            group.clear();
            for &node in &self.slots[pkg * self.stride..][..self.stride] {
                if node != NONE && !group.contains(&node) {
                    group.push(node);
                }
            }
            if group.len() > 1 {
                group.sort_unstable_by(|&a, &b| self.package_cmp(a, b, kinds));
            }
            for &node in &group {
                self.rank[node.index()] = self.ordered.len() as u32;
                self.ordered.push(node);
            }
        }

        let feature_nodes = len - self.ordered.len();
        if feature_nodes == 0 {
            return;
        }
        // A counting sort of the feature nodes by their package node, stable, so each package
        // node's features are in a run of their own.
        let mut ends: Vec<u32> = Vec::with_capacity(len + 1);
        ends.resize(len + 1, 0);
        for node in &self.nodes {
            if let Node::Feature { of, .. } = node {
                ends[of.index() + 1] += 1;
            }
        }
        for i in 1..=len {
            ends[i] += ends[i - 1];
        }
        let mut placed: Vec<NodeId> = Vec::with_capacity(feature_nodes);
        placed.resize(feature_nodes, NONE);
        for (i, node) in self.nodes.iter().enumerate() {
            if let Node::Feature { of, .. } = node {
                let at = &mut ends[of.index()];
                placed[*at as usize] = NodeId(i as u32);
                *at += 1;
            }
        }
        let name = |nodes: &[Node], node: NodeId| match nodes[node.index()] {
            Node::Feature { name, .. } => name,
            Node::Package { .. } => unreachable!("a package node among the features"),
        };
        let mut begin = 0;
        let mut next = self.ordered.len() as u32;
        for &end in &ends[..len] {
            let run = &mut placed[begin..end as usize];
            if run.len() > 1 {
                run.sort_unstable_by(|&a, &b| name(&self.nodes, a).cmp(&name(&self.nodes, b)));
            }
            for &node in run.iter() {
                self.rank[node.index()] = next;
                next += 1;
            }
            begin = end as usize;
        }
    }

    /// Cargo's order of two package nodes of the same package.
    fn package_cmp(&self, a: NodeId, b: NodeId, kinds: &Kinds) -> Ordering {
        match (self.node(a), self.node(b)) {
            (Node::Package { features: fa, kind: ka, .. }, Node::Package { features: fb, kind: kb, .. }) => {
                self.features(fa).cmp(self.features(fb)).then_with(|| kinds.cmp_kinds(ka, kb))
            }
            _ => unreachable!("a feature node among the packages"),
        }
    }

    /// Lays `raw` out as [`Edges`].
    fn lay_out(&mut self) {
        lay_out(self.nodes.len(), &self.rank, &mut self.raw, &mut self.sorted, &mut self.counts, &mut self.edges);
    }

    /// Lays the graph's edges out again, each `from -> to` of a node `keep` keeps, turned
    /// around when `invert`.
    fn relay(&mut self, keep: impl Fn(NodeId) -> bool, invert: bool) {
        self.raw.clear();
        for from in 0..self.nodes.len() {
            let from = NodeId(from as u32);
            if !keep(from) {
                continue;
            }
            for tag in 0..TAGS {
                for &to in &self.edges.targets[self.edges.range(from, tag..tag + 1)] {
                    self.raw.push(match invert {
                        false => Raw { from, to, tag: tag as Tag },
                        true => Raw { from: to.node(), to: Edge::new(from, to.public()), tag: tag as Tag },
                    });
                }
            }
        }
        self.lay_out();
    }

    /// Leaves only the nodes reachable from `roots`: cargo's `from_reachable`.
    pub(crate) fn restrict(&mut self, roots: &[NodeId]) {
        // Graph built with features does not (yet) support --duplicates.
        assert!(!self.graph_features);
        let mut live = Bits::new(self.nodes.len());
        let mut stack: Vec<NodeId> = Vec::with_capacity(self.nodes.len());
        for &root in roots {
            if live.insert_growing(root.index()) {
                stack.push(root);
            }
        }
        while let Some(node) = stack.pop() {
            for &edge in &self.edges.targets[self.edges.range(node, 0..TAGS)] {
                if live.insert_growing(edge.node().index()) {
                    stack.push(edge.node());
                }
            }
        }
        self.relay(|node| live.contains(node.index()), false);
        self.live = Some(live);
    }

    /// Inverts the direction of all edges.
    pub(crate) fn invert(&mut self) {
        self.relay(|_| true, true);
    }

    /// The package nodes of the packages in `pkgs`, in cargo's order.
    pub(crate) fn package_nodes(&self, pkgs: &Bits) -> Vec<NodeId> {
        let mut nodes = Vec::with_capacity(self.ordered.len());
        for &node in &self.ordered {
            if let Node::Package { pkg, .. } = self.node(node)
                && pkgs.contains(pkg.index())
            {
                nodes.push(node);
            }
        }
        nodes
    }

    /// The nodes of packages that are in the graph more than once (same name, with other
    /// versions, features or sources), not counting one on both the host and the target, in
    /// cargo's order. Nodes of one name are next to each other in that order, by package and
    /// features first, so a name is in the graph more than once when its first and last differ.
    pub(crate) fn find_duplicates(&self) -> Vec<NodeId> {
        assert!(!self.graph_features);
        let mut dupes = Vec::with_capacity(self.ordered.len());
        let mut start = 0;
        let mut name = None;
        for &node in &self.ordered {
            if self.live.as_ref().is_some_and(|live| !live.contains(node.index())) {
                continue;
            }
            let this = Some(self.package_id_of(node).name());
            if this != name {
                self.close_run(&mut dupes, start);
                start = dupes.len();
                name = this;
            }
            dupes.push(node);
        }
        self.close_run(&mut dupes, start);
        dupes
    }

    /// Drops `dupes[start..]`, the nodes of one name, unless they are of more than one package
    /// or set of features.
    fn close_run(&self, dupes: &mut Vec<NodeId>, start: usize) {
        let run = &dupes[start..];
        let more = match (run.first(), run.last()) {
            (Some(&first), Some(&last)) => match (self.node(first), self.node(last)) {
                (Node::Package { pkg: a, features: fa, .. }, Node::Package { pkg: b, features: fb, .. }) => {
                    a != b || self.features(fa) != self.features(fb)
                }
                _ => unreachable!("a feature node among the packages"),
            },
            _ => false,
        };
        if !more {
            dupes.truncate(start);
        }
    }
}

/// Lays `raw` out as CSR in `edges`: grouped by node and tag, each group in rank order of its
/// targets, duplicates dropped. Two stable counting sorts (by target, then by group) and one
/// pass: O(edges + nodes), no comparison.
fn lay_out(
    len: usize,
    rank: &[u32],
    raw: &mut Vec<Raw>,
    sorted: &mut Vec<Raw>,
    counts: &mut Vec<u32>,
    edges: &mut Edges,
) {
    let groups = len * TAGS;
    counting_sort(raw, sorted, 2 * len, |it| rank[it.to.node().index()] as usize * 2 + it.to.public() as usize, counts);
    counting_sort(sorted, raw, groups, Raw::group, counts);
    edges.offsets.clear();
    edges.offsets.resize(groups + 1, 0);
    edges.targets.clear();
    edges.targets.reserve(raw.len());
    let mut last = None;
    for &it in raw.iter() {
        if last == Some(it) {
            continue;
        }
        last = Some(it);
        edges.offsets[it.group() + 1] += 1;
        edges.targets.push(it.to);
    }
    for i in 1..=groups {
        edges.offsets[i] += edges.offsets[i - 1];
    }
}

/// A stable counting sort of `src` into `dst` by `key`, which is under `buckets`.
fn counting_sort(src: &[Raw], dst: &mut Vec<Raw>, buckets: usize, key: impl Fn(Raw) -> usize, counts: &mut Vec<u32>) {
    counts.clear();
    counts.resize(buckets + 1, 0);
    for &it in src {
        counts[key(it) + 1] += 1;
    }
    for i in 1..=buckets {
        counts[i] += counts[i - 1];
    }
    dst.clear();
    dst.resize(src.len(), Raw::EMPTY);
    for &it in src {
        let at = &mut counts[key(it)];
        dst[*at as usize] = it;
        *at += 1;
    }
}

/// What building the graph reads, and its buffers.
struct Builder<'g, 'a> {
    graph: Graph<'a>,
    resolve: &'g Resolve,
    resolved_features: &'g ResolvedFeatures,
    kinds: &'g Kinds,
    opts: &'g Options,
    edge_mask: u8,
    show_all_targets: bool,
    default: InternedString,
    /// The dependencies of the packages being added, each package's on top of its dependent's.
    deps: Vec<(Pkg, &'g Dependency)>,
    /// The dependencies by name of the packages being added, as `deps`.
    pending: Vec<DepName>,
    /// The command line's features of a member.
    values: Vec<FeatureValue>,
}

/// Builds the graph of `members_with_features`, the members and the features the command line
/// gives each.
pub(crate) fn build<'a>(
    resolve: &Resolve,
    resolved_features: &ResolvedFeatures,
    mut members_with_features: Vec<(&Summary, CliFeatures)>,
    kinds: &Kinds,
    requested_kinds: &[CompileKind],
    packages: &'a Table<'a>,
    opts: &Options,
) -> Graph<'a> {
    let len = packages.len();
    let mut builder = Builder {
        graph: Graph::new(packages, kinds, opts.graph_features),
        resolve,
        resolved_features,
        kinds,
        opts,
        edge_mask: edge_mask(opts),
        show_all_targets: opts.target == Target::All,
        default: InternedString::new("default"),
        deps: Vec::with_capacity(4 * len + 16),
        pending: Vec::with_capacity(if opts.graph_features { 4 * len + 16 } else { 0 }),
        values: Vec::with_capacity(if opts.graph_features { 16 } else { 0 }),
    };
    members_with_features.sort_unstable_by_key(|(member, _)| member.package_id());
    for (member, cli_features) in &members_with_features {
        let member_id = member.package_id();
        let member_pkg = packages.pkg(member_id);
        let proc_macro = packages.read(member_pkg).is_some_and(|it| it.any_proc_macro);
        let features_for = match proc_macro {
            true => FeaturesFor::HostDep,
            false => FeaturesFor::NormalOrDev,
        };
        for &kind in requested_kinds {
            let member_index = builder.add_pkg(member_pkg, features_for, kinds.kind(kind));
            if opts.graph_features {
                let fmap = resolve.summary(member_id).features();
                builder.add_cli_features(member_index, cli_features, fmap);
            }
        }
    }
    if opts.graph_features {
        builder.add_internal_features();
    }
    let mut graph = builder.graph;
    graph.order(kinds);
    graph.lay_out();
    graph
}

impl<'g> Builder<'g, '_> {
    /// Adds a single package node (if it does not already exist), and recursively all of its
    /// dependencies. Returns the index to the package node.
    fn add_pkg(&mut self, pkg: Pkg, features_for: FeaturesFor, requested: Kind) -> NodeId {
        let node_kind = match features_for {
            FeaturesFor::HostDep => Kind::HOST,
            FeaturesFor::NormalOrDev => requested,
        };
        let slot = self.graph.slot(pkg, features_for, node_kind);
        if self.graph.slots[slot] != NONE {
            return self.graph.slots[slot];
        }
        let package_id = self.graph.packages.id(pkg);
        // Written into the pool in place, and taken back off it when the node is another's.
        let start = self.graph.features.len();
        self.graph.features.extend(self.resolved_features.activated_features_iter(package_id, features_for));
        if node_kind == Kind::HOST {
            // A package built for the host as a host dependency and as a normal one is one
            // node when its features are the same.
            let other = match features_for {
                FeaturesFor::HostDep => self.graph.slot(pkg, FeaturesFor::NormalOrDev, Kind::HOST),
                FeaturesFor::NormalOrDev => self.graph.slot(pkg, FeaturesFor::HostDep, Kind::HOST),
            };
            let same = self.graph.slots[other];
            if same != NONE && self.graph.features_of(same) == &self.graph.features[start..] {
                self.graph.features.truncate(start);
                self.graph.slots[slot] = same;
                return same;
            }
        }
        let features = Span { start: start as u32, len: (self.graph.features.len() - start) as u32 };
        let from_index = self.graph.add_node(Node::Package { pkg, kind: node_kind, features });
        self.graph.slots[slot] = from_index;

        let resolve = self.resolve;
        let deps_start = self.deps.len();
        for (dep_id, deps) in resolve.deps(package_id) {
            let dep_pkg = self.graph.packages.pkg(dep_id);
            for dep in deps {
                if self.shows(dep, dep_pkg, package_id, features_for, node_kind) {
                    self.deps.push((dep_pkg, dep));
                }
            }
        }
        // Each package's dependencies in order, those of one package by kind and name.
        self.deps[deps_start..].sort_unstable_by(|(a, x), (b, y)| {
            a.cmp(b).then_with(|| x.kind().cmp(&y.kind())).then_with(|| x.name_in_toml().cmp(&y.name_in_toml()))
        });
        let deps_end = self.deps.len();
        let names_start = self.pending.len();
        for i in deps_start..deps_end {
            let (dep_pkg, dep) = self.deps[i];
            // A host dependency's dependencies, build dependencies too, are built for the host;
            // a normal one's are built as cargo's old rules have them: normal and dev
            // dependencies for the dependent's platform, build dependencies for the host.
            let dep_features_for = match features_for {
                FeaturesFor::HostDep => FeaturesFor::HostDep,
                FeaturesFor::NormalOrDev if dep.is_build() || self.graph.packages.any_proc_macro(dep_pkg) => {
                    FeaturesFor::HostDep
                }
                FeaturesFor::NormalOrDev => FeaturesFor::NormalOrDev,
            };
            let dep_index = self.add_pkg(dep_pkg, dep_features_for, requested);
            let tag = dep_tag(dep.kind());
            let new_edge = Edge::new(dep_index, dep.is_public());
            if self.graph.graph_features {
                // Add the dependency node with feature nodes in-between.
                self.pending.push(DepName { name: dep.name_in_toml(), node: dep_index, optional: dep.is_optional() });
                if dep.uses_default_features() {
                    self.graph.add_feature(self.default, Some(from_index), tag, new_edge);
                }
                for &feature in dep.features() {
                    self.graph.add_feature(feature, Some(from_index), tag, new_edge);
                }
                if !dep.uses_default_features() && dep.features().is_empty() {
                    // No features, use a direct connection.
                    self.graph.add_edge(from_index, tag, new_edge);
                }
            } else {
                self.graph.add_edge(from_index, tag, new_edge);
            }
        }
        self.deps.truncate(deps_start);
        if self.graph.graph_features {
            self.graph.keep_dep_names(from_index, &mut self.pending, names_start);
        }
        from_index
    }

    /// Whether `dep` of `package_id` is in the tree: *similar* to the filter of cargo's
    /// `unit_dependencies::compute_deps`.
    fn shows(
        &self,
        dep: &Dependency,
        dep_pkg: Pkg,
        package_id: PackageId,
        features_for: FeaturesFor,
        node_kind: Kind,
    ) -> bool {
        let kind = match (node_kind, dep.kind()) {
            (Kind::HOST, _) | (_, DepKind::Build) => Kind::HOST,
            (_, DepKind::Normal | DepKind::Development) => node_kind,
        };
        // Filter out inactivated targets.
        if !self.show_all_targets && !dep.platform().is_none_or(|platform| self.kinds.activated_kind(platform, kind)) {
            return false;
        }
        if self.edge_mask & (1 << dep_tag(dep.kind())) == 0 {
            return false;
        }
        if self.opts.no_proc_macro && self.graph.packages.any_proc_macro(dep_pkg) {
            return false;
        }
        if self.opts.public && !dep.is_public() {
            return false;
        }
        // An optional dependency the feature resolver does not enable is not used.
        !dep.is_optional() || self.resolved_features.is_dep_activated(package_id, features_for, dep.name_in_toml())
    }

    /// Adds nodes for the features the command line asks of the member, as roots: they come
    /// from the outside world, and usually only appear with `--invert`.
    fn add_cli_features(&mut self, package_index: NodeId, cli_features: &CliFeatures, feature_map: &FeatureMap) {
        // Recursive enabling of features is handled by add_internal_features. Sorted and
        // deduplicated, as cargo's `BTreeSet` of them.
        let mut values = std::mem::take(&mut self.values);
        values.clear();
        if cli_features.all_features {
            values.extend(feature_map.keys().map(|feat| FeatureValue::Feature(*feat)));
        }
        if cli_features.uses_default_features {
            values.push(FeatureValue::Feature(self.default));
        }
        values.extend(cli_features.features.iter().cloned());
        values.sort_unstable();
        values.dedup();
        let to_package = Edge::new(package_index, true);
        for fv in &values {
            match *fv {
                FeatureValue::Feature(feature) => {
                    let index = self.graph.add_feature(feature, None, FEATURE, to_package).1;
                    self.graph.cli_features.insert_growing(index.index());
                }
                // This is enforced by CliFeatures.
                FeatureValue::Dep { .. } => panic!("unexpected cli dep feature {fv}"),
                // `--features bar?/feat` where `bar` is not activated is ignored (no names
                // match); one that is not weak was refused before.
                FeatureValue::DepFeature { dep_name, dep_feature, weak: _ } => {
                    for i in self.graph.dep_names_of(package_index, dep_name) {
                        let DepName { node: dep_index, optional, .. } = self.graph.dep_names[i];
                        if optional {
                            // Activate the optional dep on self.
                            let index = self.graph.add_feature(dep_name, None, FEATURE, to_package).1;
                            self.graph.cli_features.insert_growing(index.index());
                        }
                        let dep_edge = Edge::new(dep_index, true);
                        let index = self.graph.add_feature(dep_feature, None, FEATURE, dep_edge).1;
                        self.graph.cli_features.insert_growing(index.index());
                    }
                }
            }
        }
        self.values = values;
    }

    /// Recursively adds connections between features in the `[features]` table of every
    /// package, from the feature nodes there are now (those dependencies or the command line
    /// activated), in the order they were made.
    fn add_internal_features(&mut self) {
        for i in 0..self.graph.nodes.len() {
            if let Node::Feature { of, name } = self.graph.nodes[i] {
                let package_id = self.graph.package_id_of(of);
                self.add_feature_rec(name, package_id, NodeId(i as u32), of);
            }
        }
    }

    /// Recursively add feature nodes for all features enabled by the given feature. `from` is
    /// the node that enables it, `package_index` the package node it is of.
    fn add_feature_rec(&mut self, feature_name: InternedString, package_id: PackageId, from: NodeId, package_index: NodeId) {
        let resolve = self.resolve;
        let feature_map = resolve.summary(package_id).features();
        let Some(fvs) = feature_map.get(&feature_name) else {
            return;
        };
        let to_package = Edge::new(package_index, true);
        for fv in fvs {
            match *fv {
                FeatureValue::Feature(dep_name) => {
                    let (missing, feat_index) = self.graph.add_feature(dep_name, Some(from), FEATURE, to_package);
                    // Don't recurse if the edge already exists, to deal with cycles.
                    if missing {
                        self.add_feature_rec(dep_name, package_id, feat_index, package_index);
                    }
                }
                // Dependencies are already shown in the graph as dep edges.
                FeatureValue::Dep { .. } => {}
                FeatureValue::DepFeature { dep_name, dep_feature, weak } => {
                    // `weak` is mostly handled where the graph is built, in `is_dep_activated`;
                    // here it only says whether the feature of the dependency's name is
                    // activated on self.
                    for i in self.graph.dep_names_of(package_index, dep_name) {
                        let DepName { node: dep_index, optional, .. } = self.graph.dep_names[i];
                        let dep_pkg_id = self.graph.package_id_of(dep_index);
                        if optional && !weak {
                            // Activate the optional dep on self.
                            self.graph.add_feature(dep_name, Some(from), FEATURE, to_package);
                        }
                        let dep_edge = Edge::new(dep_index, true);
                        let (missing, feat_index) = self.graph.add_feature(dep_feature, Some(from), FEATURE, dep_edge);
                        if missing {
                            self.add_feature_rec(dep_feature, dep_pkg_id, feat_index, dep_index);
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
