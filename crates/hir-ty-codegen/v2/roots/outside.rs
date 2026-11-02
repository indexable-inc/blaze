//! Which tests reach the outside world through what neither the hashes of their code nor the
//! trace of their run say ([`super::RootTest::outside`]): a symbol that starts a process or
//! talks over the network (v1's classifier, [`crate::fncache::outside`]), or assembly. Files,
//! directories and the environment a test reads are in its run's trace, and a run that read
//! the network is not kept, so those are not this.
//!
//! What a test reaches is everything its function's references reach, through functions and
//! statics, so one walk of the graph of the program answers every test: the strongly
//! connected components (Tarjan's), each outside if a member or a component it reaches is.

use std::borrow::Cow;

use base_db::Db;
use blaze_hash::HashMap;

use super::{
    super::{HirFrontend, HirInstance, Kind, places::Ref, refs, statics},
    RootTest,
};
use crate::{
    fncache::{self, Kind as Reads},
    home::Homes,
};

struct Visit<'db> {
    /// Its place in the order of the walk, which is its place in [`Graph::visits`].
    index: u32,
    low: u32,
    on_stack: bool,
    /// Whether it or what it reaches is outside: final once its component is done.
    outside: bool,
    /// What it names, as `refs::walked`'s memo holds it; its functions and statics are the
    /// nodes it reaches.
    names: Cow<'db, [Ref]>,
}

struct Graph<'a, 'f, 'db> {
    frontend: &'a HirFrontend<'f>,
    /// Each node, a [`Ref::Function`] or [`Ref::Static`], by its place.
    places: HashMap<Ref, u32>,
    visits: Vec<Visit<'db>>,
    /// Tarjan's stack: the nodes of the components not yet done.
    stack: Vec<u32>,
}

/// For each of `tests`, whether what its function at its place in `roots` reaches is outside.
pub(super) fn of<'db>(
    frontend: &HirFrontend<'_>,
    db: &'db Db,
    homes: &mut Homes,
    roots: &[HirInstance],
    tests: &[RootTest],
) -> Result<Vec<bool>, String> {
    let mut graph = Graph {
        frontend,
        // alloc: once a harness, sized by the program it reaches
        places: HashMap::default(),
        // alloc: as above
        visits: Vec::new(),
        // alloc: as above
        stack: Vec::new(),
    };
    tests
        .iter()
        .map(|test| {
            // clone: once a test
            let at = graph.visit(db, homes, Ref::Function(roots[test.root].clone()))?;
            Ok(graph.visits[at as usize].outside)
        })
        // alloc: once a harness, a flag a test
        .collect()
}

impl<'db> Graph<'_, '_, 'db> {
    /// The place of `node`, once all it reaches is visited.
    fn visit(&mut self, db: &'db Db, homes: &mut Homes, node: Ref) -> Result<u32, String> {
        if let Some(&at) = self.places.get(&node) {
            return Ok(at);
        }
        let root = self.open(db, homes, node)?;
        // Each frame: a node, and how many of what it names are walked.
        // alloc: once a test not yet reached, as deep as its walk
        let mut frames: Vec<(u32, usize)> = vec![(root, 0)];
        while let Some(frame) = frames.last_mut() {
            let (at, next) = (frame.0, frame.1);
            if let Some(named) = self.visits[at as usize].names.get(next) {
                frame.1 += 1;
                if matches!(named, Ref::Extern(_) | Ref::Weak(_)) {
                    continue;
                }
                let Some(&to) = self.places.get(named) else {
                    let named = named.clone(); // clone: once a node of the program, its key
                    let to = self.open(db, homes, named)?;
                    frames.push((to, 0));
                    continue;
                };
                let target = &self.visits[to as usize];
                let (on_stack, index, outside) = (target.on_stack, target.index, target.outside);
                let visit = &mut self.visits[at as usize];
                if on_stack {
                    visit.low = visit.low.min(index);
                }
                // One of the same component gives its own by the end of it, and one done
                // gives what its component has.
                visit.outside |= outside;
                continue;
            }
            frames.pop();
            let visit = &mut self.visits[at as usize];
            // alloc: empty, which frees the names walked when they were the walk's own
            visit.names = Cow::Borrowed(&[]);
            let (low, outside) = (visit.low, visit.outside);
            if low == at {
                self.close(at);
            }
            if let Some(&(parent, _)) = frames.last() {
                let outside = outside || self.visits[at as usize].outside;
                let visit = &mut self.visits[parent as usize];
                visit.low = visit.low.min(low);
                visit.outside |= outside;
            }
        }
        Ok(root)
    }

    /// Ends the component whose first node is `at`: each of its nodes is outside if one is.
    fn close(&mut self, at: u32) {
        let from = (self.stack.iter().rposition(|&it| it == at))
            .expect("a component's root is on the stack");
        let outside = self.stack[from..].iter().any(|&it| self.visits[it as usize].outside);
        for member in self.stack.drain(from..) {
            let visit = &mut self.visits[member as usize];
            (visit.on_stack, visit.outside) = (false, outside);
        }
    }

    /// Places `node`, a function or a static, and reads what it names.
    fn open(&mut self, db: &'db Db, homes: &mut Homes, node: Ref) -> Result<u32, String> {
        let at = u32::try_from(self.visits.len())
            // alloc: the error's message
            .map_err(|_| "a program of more than 2^32 functions and statics".to_owned())?;
        let (outside, names) = self.read(db, homes, &node)?;
        self.places.insert(node, at);
        self.visits.push(Visit { index: at, low: at, on_stack: true, outside, names });
        self.stack.push(at);
        Ok(at)
    }

    /// Whether `node` is outside itself, and what it names.
    fn read(
        &self,
        db: &'db Db,
        homes: &mut Homes,
        node: &Ref,
    ) -> Result<(bool, Cow<'db, [Ref]>), String> {
        let names = match node {
            Ref::Function(instance) => match &instance.0 {
                Kind::Asm(..) => return Ok((true, Cow::Borrowed(&[]))),
                // The walk without MIR inlining reaches what the one with does, through the
                // callees it inlines.
                _ => refs::listed(db, instance, false)?,
            },
            // alloc: once a static of the program, the references its value gives
            Ref::Static(value) => {
                Cow::Owned(statics::value(db, homes, &self.frontend.hashes, value.0)?.refs)
            }
            Ref::Extern(_) | Ref::Weak(_) => unreachable!("a symbol outside blaze's code is no node"),
        };
        let outside = names.iter().any(|it| match it {
            Ref::Extern(name) | Ref::Weak(name) => {
                matches!(fncache::outside(name).map(|it| it.0), Some(Reads::Net | Reads::Process))
            }
            Ref::Function(_) | Ref::Static(_) => false,
        });
        Ok((outside, names))
    }
}
