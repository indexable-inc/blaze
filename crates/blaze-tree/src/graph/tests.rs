use super::*;

/// Cargo's way: a `contains` before each push into the node's list of its kind, then the list
/// sorted by node. Ties (one node, public and not) sorted by `public`, as [`lay_out`] has them.
fn reference(len: usize, rank: &[u32], raw: &[Raw]) -> Vec<Vec<Edge>> {
    let mut groups = vec![Vec::new(); len * TAGS];
    for it in raw {
        let group: &mut Vec<Edge> = &mut groups[it.group()];
        if !group.contains(&it.to) {
            group.push(it.to);
        }
    }
    for group in &mut groups {
        group.sort_by_key(|edge| (rank[edge.node().index()], edge.public()));
    }
    groups
}

/// A small deterministic generator, so the cases are the same on every run.
struct Lcg(u64);

impl Lcg {
    fn below(&mut self, n: usize) -> usize {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((self.0 >> 33) as usize) % n
    }
}

fn laid_out(len: usize, rank: &[u32], raw: &[Raw]) -> Edges {
    let mut raw = raw.to_vec();
    let (mut sorted, mut counts, mut edges) = (Vec::new(), Vec::new(), Edges::default());
    lay_out(len, rank, &mut raw, &mut sorted, &mut counts, &mut edges);
    edges
}

#[test]
fn csr_is_cargos_sorted_and_deduplicated_lists() {
    let mut rng = Lcg(7);
    for len in [1, 2, 5, 33, 200] {
        // A rank that is not the node order, as a feature graph's is not.
        let mut rank: Vec<u32> = (0..len as u32).collect();
        for i in (1..len).rev() {
            rank.swap(i, rng.below(i + 1));
        }
        let raw: Vec<Raw> = (0..len * 6)
            .map(|_| Raw {
                from: NodeId(rng.below(len) as u32),
                to: Edge::new(NodeId(rng.below(len) as u32), rng.below(4) == 0),
                tag: rng.below(TAGS) as Tag,
            })
            .collect();
        let edges = laid_out(len, &rank, &raw);
        let expected = reference(len, &rank, &raw);
        assert_eq!(edges.offsets.len(), len * TAGS + 1);
        for node in 0..len {
            for tag in 0..TAGS {
                let got = &edges.targets[edges.range(NodeId(node as u32), tag..tag + 1)];
                assert_eq!(got, &expected[node * TAGS + tag][..], "node {node}, tag {tag}, {len} nodes");
            }
        }
    }
}

#[test]
fn an_edge_added_twice_is_kept_once_and_public_is_its_own_edge() {
    let rank = [0, 1];
    let to = |public| Raw { from: NodeId(0), to: Edge::new(NodeId(1), public), tag: NORMAL };
    let edges = laid_out(2, &rank, &[to(false), to(false), to(true), to(false)]);
    let got = &edges.targets[edges.range(NodeId(0), 0..1)];
    assert_eq!(got, &[Edge::new(NodeId(1), false), Edge::new(NodeId(1), true)]);
    assert!(edges.range(NodeId(1), 0..TAGS).is_empty());
}

#[test]
fn an_edge_keeps_its_node_and_whether_it_is_public() {
    let edge = Edge::new(NodeId(PUBLIC - 1), true);
    assert_eq!(edge.node(), NodeId(PUBLIC - 1));
    assert!(edge.public());
    assert!(!Edge::new(NodeId(3), false).public());
}

#[test]
fn bits_grow_and_forget() {
    let mut bits: Bits<usize> = Bits::new(3);
    assert!(bits.insert_growing(2));
    assert!(!bits.insert_growing(2));
    assert!(bits.insert_growing(130));
    assert!(bits.contains(130) && bits.contains(2) && !bits.contains(1) && !bits.contains(1000));
    bits.remove(2);
    assert!(!bits.contains(2));
}

#[test]
fn edge_kinds_are_their_sections_in_print_order() {
    let tags = [
        EdgeKind::Dep(DepKind::Normal).tag(),
        EdgeKind::Dep(DepKind::Build).tag(),
        EdgeKind::Dep(DepKind::Development).tag(),
        EdgeKind::Feature.tag(),
    ];
    assert_eq!(tags, [NORMAL, BUILD, DEV, FEATURE]);
}
