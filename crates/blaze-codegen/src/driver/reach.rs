//! What a root reaches in a build: the code a test's result depends on
//! ([`super::Built::reached`]).
//!
//! The walk's graph kept as flat arrays when the walk ends: functions in key order, then
//! statics in key order, each a node, with the edges of its references. A function's marks
//! are its code hash and its arguments' content ID, a static's its value and instance: every
//! input of compiled code the build has, since a callee its code builds in or inlines is a
//! reference of it too. What no node holds, the code of a C library or of `extern` symbols
//! outside blaze's code and the positions of the span tables, is not here.

use blaze_ids::ContentId;
use blaze_items::{CodeHash, ItemKey};

use crate::{InstanceKey, Target};

#[derive(PartialEq, Eq, Debug, Default)]
pub struct Reach {
    /// The functions, sorted: node `i` for `i < functions.len()`.
    functions: Vec<InstanceKey>,
    /// The statics, sorted: node `functions.len() + i`.
    statics: Vec<(ItemKey, ContentId)>,
    /// Per node, where its edges start in `edges`; one more than there are nodes.
    starts: Vec<u32>,
    edges: Vec<u32>,
    /// Per node, its two marks.
    marks: Vec<[ContentId; 2]>,
}

/// A node as [`Reach::new`] takes it: its marks and its references.
pub(crate) struct Node<'a> {
    pub(crate) marks: [ContentId; 2],
    pub(crate) refs: &'a [Target],
}

/// What [`Reach::functions_of`] reuses from one root to the next: a bit per node, of which
/// each walk clears only those it set, and the walk's stack and the nodes it visited.
#[derive(Default)]
pub(crate) struct Walk {
    seen: Vec<u64>,
    stack: Vec<u32>,
    visited: Vec<u32>,
}

impl Reach {
    /// The graph of `functions` and `statics`, each sorted by key, without duplicates.
    pub(crate) fn new<'a>(
        functions: impl Iterator<Item = (InstanceKey, Node<'a>)>,
        statics: impl Iterator<Item = ((ItemKey, ContentId), Node<'a>)>,
    ) -> Reach {
        let (function_count, static_count) = (functions.size_hint().0, statics.size_hint().0);
        let nodes = function_count + static_count;
        let mut reach = Reach {
            // alloc: per build, sized by its units
            functions: Vec::with_capacity(function_count),
            // alloc: per build, sized by its statics
            statics: Vec::with_capacity(static_count),
            // alloc: per build, sized by its units
            starts: Vec::with_capacity(nodes + 1),
            // alloc: per build, sized by its references, which only the nodes' lists count
            edges: Vec::new(),
            // alloc: per build, sized by its units
            marks: Vec::with_capacity(nodes),
        };
        // alloc: per build, each node's references until the keys are all known
        let mut refs: Vec<&'a [Target]> = Vec::with_capacity(nodes);
        for (key, node) in functions {
            reach.functions.push(key);
            reach.marks.push(node.marks);
            refs.push(node.refs);
        }
        for (key, node) in statics {
            reach.statics.push(key);
            reach.marks.push(node.marks);
            refs.push(node.refs);
        }
        assert!(reach.marks.len() < u32::MAX as usize, "more than 2^32 - 1 nodes");
        debug_assert!(reach.functions.is_sorted() && reach.statics.is_sorted());
        for targets in refs {
            reach.starts.push(reach.edges.len() as u32);
            let start = reach.edges.len();
            for target in targets {
                if let Some(at) = reach.node(target) {
                    reach.edges.push(at);
                }
            }
            reach.edges[start..].sort_unstable();
            let mut kept = start;
            for at in start..reach.edges.len() {
                if kept == start || reach.edges[kept - 1] != reach.edges[at] {
                    reach.edges[kept] = reach.edges[at];
                    kept += 1;
                }
            }
            reach.edges.truncate(kept);
        }
        assert!(reach.edges.len() <= u32::MAX as usize, "more than 2^32 references");
        reach.starts.push(reach.edges.len() as u32);
        reach
    }

    /// The node a reference is to: a function or static of the build. `None` for data,
    /// symbols outside blaze's code, positions and coverage, none of which is a node.
    fn node(&self, target: &Target) -> Option<u32> {
        match target {
            Target::Function(key) | Target::Direct(key) => {
                Some(self.functions.binary_search(key).ok()? as u32)
            }
            Target::Static { item, instance } => {
                let at = self.statics.binary_search(&(*item, *instance)).ok()?;
                Some((self.functions.len() + at) as u32)
            }
            Target::Data(_)
            | Target::Extern(_)
            | Target::Weak(_)
            | Target::Position(_)
            | Target::Coverage(_)
            | Target::Counter(_)
            | Target::ProfileCounters => None,
        }
    }

    /// The marks of every node `root` reaches, itself included, sorted and each once.
    /// `None` when `root` is no function of the build.
    pub(super) fn of(&self, root: &InstanceKey) -> Option<Vec<CodeHash>> {
        let root = self.functions.binary_search(root).ok()? as u32;
        let nodes = self.marks.len();
        // alloc: per call, a bit per node
        let mut seen = vec![0u64; nodes.div_ceil(64)];
        // alloc: per call, sized by what it reaches
        let mut stack = vec![root];
        seen[root as usize / 64] |= 1 << (root % 64);
        // alloc: per call, sized by what it reaches
        let mut marks = Vec::new();
        while let Some(at) = stack.pop() {
            marks.extend(self.marks[at as usize].map(CodeHash));
            let edges = &self.edges[self.starts[at as usize] as usize..self.starts[at as usize + 1] as usize];
            for &next in edges {
                let (word, bit) = (next as usize / 64, 1u64 << (next % 64));
                if seen[word] & bit == 0 {
                    seen[word] |= bit;
                    stack.push(next);
                }
            }
        }
        marks.sort_unstable();
        marks.dedup();
        Some(marks)
    }

    /// The functions `root` reaches, itself included, as their places in key order (those of
    /// [`super::Built::inputs`]), sorted: through statics too, whose values point at functions.
    /// `None` when `root` is no function of the build.
    pub(crate) fn functions_of(&self, root: &InstanceKey, walk: &mut Walk) -> Option<Vec<u32>> {
        let root = self.functions.binary_search(root).ok()? as u32;
        walk.seen.resize(self.marks.len().div_ceil(64), 0);
        walk.stack.clear();
        walk.visited.clear();
        walk.seen[root as usize / 64] |= 1 << (root % 64);
        walk.stack.push(root);
        walk.visited.push(root);
        while let Some(at) = walk.stack.pop() {
            let edges = &self.edges[self.starts[at as usize] as usize..self.starts[at as usize + 1] as usize];
            for &next in edges {
                let (word, bit) = (next as usize / 64, 1u64 << (next % 64));
                if walk.seen[word] & bit == 0 {
                    walk.seen[word] |= bit;
                    walk.stack.push(next);
                    walk.visited.push(next);
                }
            }
        }
        let functions = self.functions.len();
        let words = functions.div_ceil(64);
        // alloc: per root, the list its caller keeps
        let mut out = Vec::with_capacity(walk.visited.len());
        // A scan of the functions' bits when it reads fewer words than a sort would compare.
        if walk.visited.len() * 4 >= words {
            for (word, &bits) in walk.seen[..words].iter().enumerate() {
                let mut bits = match word + 1 == words && functions % 64 != 0 {
                    true => bits & ((1u64 << (functions % 64)) - 1),
                    false => bits,
                };
                while bits != 0 {
                    out.push((word * 64) as u32 + bits.trailing_zeros());
                    bits &= bits - 1;
                }
            }
        } else {
            out.extend(walk.visited.iter().copied().filter(|&it| (it as usize) < functions));
            out.sort_unstable();
        }
        for &at in &walk.visited {
            walk.seen[at as usize / 64] &= !(1u64 << (at % 64));
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(n: u8) -> InstanceKey {
        InstanceKey(ContentId([n; 32]))
    }

    fn mark(n: u8) -> ContentId {
        ContentId([n; 32])
    }

    fn statics<'a>() -> std::iter::Empty<((ItemKey, ContentId), Node<'a>)> {
        std::iter::empty()
    }

    #[test]
    fn a_root_reaches_its_callees_at_any_depth_and_nothing_else() {
        let a = [Target::Function(key(2)), Target::Extern("memcpy".to_owned())];
        let b = [Target::Direct(key(3)), Target::Function(key(1))];
        let none: [Target; 0] = [];
        let functions = [
            (key(1), Node { marks: [mark(10), mark(11)], refs: &a[..] }),
            (key(2), Node { marks: [mark(20), mark(11)], refs: &b[..] }),
            (key(3), Node { marks: [mark(30), mark(11)], refs: &none[..] }),
            (key(4), Node { marks: [mark(40), mark(11)], refs: &none[..] }),
        ];
        let reach = Reach::new(functions.into_iter(), statics());
        let got = reach.of(&key(1)).unwrap();
        let want: Vec<CodeHash> =
            [10, 11, 20, 30].into_iter().map(|it| CodeHash(mark(it))).collect();
        assert_eq!(got, want, "sorted, each once, the cycle walked once");
        assert_eq!(reach.of(&key(3)).unwrap(), vec![CodeHash(mark(11)), CodeHash(mark(30))]);
        assert_eq!(reach.of(&key(9)), None);
    }

    #[test]
    fn statics_are_nodes_and_what_they_point_at_is_reached() {
        let item = ItemKey(mark(7));
        let f = [Target::Static { item, instance: mark(8) }];
        let s = [Target::Function(key(2))];
        let none: [Target; 0] = [];
        let functions = [
            (key(1), Node { marks: [mark(10), mark(0)], refs: &f[..] }),
            (key(2), Node { marks: [mark(20), mark(0)], refs: &none[..] }),
        ];
        let statics = [((item, mark(8)), Node { marks: [mark(50), mark(8)], refs: &s[..] })];
        let reach = Reach::new(functions.into_iter(), statics.into_iter());
        let got = reach.of(&key(1)).unwrap();
        let want: Vec<CodeHash> =
            [0, 8, 10, 20, 50].into_iter().map(|it| CodeHash(mark(it))).collect();
        assert_eq!(got, want);
    }

    /// The functions a root reaches by place, through a static, the same with the walk reused
    /// and on both ways of listing them, against `of`'s marks.
    #[test]
    fn a_root_s_functions_are_its_places_in_key_order() {
        // Function `n` is the `n`th in key order.
        let numbered = |n: u32| {
            let mut bytes = [0u8; 32];
            bytes[..4].copy_from_slice(&n.to_be_bytes());
            InstanceKey(ContentId(bytes))
        };
        let item = ItemKey(mark(7));
        let none: [Target; 0] = [];
        let calls = [Target::Direct(numbered(2)), Target::Static { item, instance: mark(8) }];
        let to_four = [Target::Function(numbered(4))];
        // 300 functions: a root that reaches one lists it by a sort, one that reaches more by
        // its bits. 0 reaches 2, the static and through it 4; 299 reaches every function.
        let mut functions: Vec<(InstanceKey, Node<'_>)> =
            (0..300).map(|n| (numbered(n), Node { marks: [mark(1), mark(0)], refs: &none[..] })).collect();
        functions[0].1.refs = &calls[..];
        let all: Vec<Target> = (0..300).map(|n| Target::Direct(numbered(n))).collect();
        functions[299].1.refs = &all[..];
        let statics = [((item, mark(8)), Node { marks: [mark(90), mark(8)], refs: &to_four[..] })];
        let reach = Reach::new(functions.into_iter(), statics.into_iter());
        let mut walk = Walk::default();
        for _ in 0..2 {
            assert_eq!(reach.functions_of(&numbered(0), &mut walk).unwrap(), [0, 2, 4]);
            assert_eq!(reach.functions_of(&numbered(299), &mut walk).unwrap(), (0..300).collect::<Vec<u32>>());
            assert_eq!(reach.functions_of(&numbered(1), &mut walk).unwrap(), [1]);
        }
        assert_eq!(reach.functions_of(&numbered(300), &mut walk), None);
        assert!(walk.seen.iter().all(|it| *it == 0), "each walk clears what it set");
    }
}
