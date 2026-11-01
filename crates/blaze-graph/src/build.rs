//! A build as the plane's nodes (docs/driver.md steps 4 to 7, docs/v2/plan/lanes/core/plane.md "What moves onto
//! it"): the workspace load, codegen and its store writes, links and test runs, each a node of
//! one request with the kind, pool and waits the step gives it.
//!
//! - **Load** (step 4): the manifest and lock file, then a fetch per package and its build
//!   script, so a package's names wait for its own fetch and scripts, not for the whole load.
//! - **Codegen and the store** (step 5): each unit's compile is a node of [`Pool::Work`] keyed
//!   by the unit's key, so a unit asked by two programs or two requests compiles once; its write
//!   to the store is a node of [`Pool::Io`] after it, which nothing but the request's end waits
//!   for, so the write of one unit overlaps the compile of the next.
//! - **Link and tests** (step 6): a link waits for the compiles of its units and the standard
//!   library, not their writes (a value put in the store is readable at once); a test run waits
//!   for its link.
//! - **Inputs while work runs** (step 7): an edit is a new view and a new request; results are
//!   keyed by content, so the old request's finished work is the new one's memo, and the old one
//!   is cancelled ([`crate::plane::Plane::cancel`]) rather than waited for. Nothing waits for
//!   a barrier.

use blaze_protocol::events::NodeKind;

use crate::{
    machine::{NodeId, Pool, Spec},
    plane::Key,
};

/// The nodes of one build, numbered from the first id given.
#[derive(Debug, PartialEq, Eq)]
pub struct Build {
    next: u32,
    specs: Vec<Spec>,
}

/// A unit's compile and its write to the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unit {
    pub compile: NodeId,
    pub store: NodeId,
}

impl Build {
    /// A build whose nodes are `first`, `first + 1`, ...: ids no other graph of the plane has
    /// (`crate::exec::ids`).
    pub fn new(first: NodeId) -> Build {
        // alloc: empty until the first node; one vector per build, handed whole to the plane
        Build { next: first.0, specs: Vec::new() }
    }

    /// Its nodes, each after what it waits for, for [`crate::plane::PlaneInput::Add`].
    pub fn specs(self) -> Vec<Spec> {
        self.specs
    }

    pub fn len(&self) -> usize {
        self.specs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.specs.is_empty()
    }

    fn node(
        &mut self,
        kind: NodeKind,
        name: String,
        deps: impl Into<Vec<NodeId>>,
        pool: Pool,
        cost: u64,
        key: Option<Key>,
    ) -> NodeId {
        let id = NodeId(self.next);
        self.next += 1;
        self.specs.push(Spec { id, kind, name, deps: deps.into(), pool, cost, key, memory: 0 });
        id
    }

    /// Reading the manifests and the lock file: keyed by their contents, when the caller has
    /// them hashed.
    pub fn manifest(&mut self, workspace: &str, key: Option<Key>) -> NodeId {
        // alloc: the node's name, which its `Spec` owns: once per node as the build is laid out
        self.node(NodeKind::Manifest, format!("manifest {workspace}"), [], Pool::Io, 1, key)
    }

    /// A package's source, local or from a registry: keyed by its checksum where it has one.
    pub fn fetch(&mut self, package: &str, key: Option<Key>, manifest: NodeId) -> NodeId {
        // alloc: the node's name, which its `Spec` owns: once per node as the build is laid out
        self.node(NodeKind::Fetch, format!("fetch {package}"), [manifest], Pool::Io, 1, key)
    }

    /// The standard library of the toolchain, from the store or compiled.
    pub fn std(&mut self, key: Key, cost: u64) -> NodeId {
        self.node(NodeKind::Std, "std".into(), [], Pool::Work, cost, Some(key))
    }

    /// A build script compiled, after its package's source and what it is compiled with.
    pub fn script(&mut self, package: &str, key: Key, deps: &[NodeId], cost: u64) -> NodeId {
        // alloc: the node's name, which its `Spec` owns: once per node as the build is laid out
        self.node(NodeKind::Script, format!("build script {package}"), deps, Pool::Work, cost, Some(key))
    }

    /// A build script run in its sandbox: a process, so it waits rather than computes. Not
    /// keyed by the plane: its result's key includes what it read, known once it ran
    /// (`blaze-usercode`'s `RunKey`), and the run looks itself up before it runs.
    pub fn script_run(&mut self, package: &str, script: NodeId, deps: &[NodeId]) -> NodeId {
        let mut waits = Vec::with_capacity(1 + deps.len());
        waits.push(script);
        waits.extend_from_slice(deps);
        // alloc: the node's name, which its `Spec` owns: once per node as the build is laid out
        self.node(NodeKind::ScriptRun, format!("run build script {package}"), waits, Pool::Io, 1, None)
    }

    /// A proc macro's library, built and loaded.
    pub fn proc_macro(&mut self, package: &str, key: Key, deps: &[NodeId], cost: u64) -> NodeId {
        // alloc: the node's name, which its `Spec` owns: once per node as the build is laid out
        self.node(NodeKind::Macro, format!("proc macro {package}"), deps, Pool::Work, cost, Some(key))
    }

    /// A crate's names: after its source, its scripts' runs, its macros and its dependencies'
    /// names.
    pub fn names(&mut self, krate: &str, key: Option<Key>, deps: &[NodeId], cost: u64) -> NodeId {
        // alloc: the node's name, which its `Spec` owns: once per node as the build is laid out
        self.node(NodeKind::Names, format!("names {krate}"), deps, Pool::Work, cost, key)
    }

    /// A crate's bodies, after its names.
    pub fn bodies(&mut self, krate: &str, key: Option<Key>, names: NodeId, cost: u64) -> NodeId {
        // alloc: the node's name, which its `Spec` owns: once per node as the build is laid out
        self.node(NodeKind::Bodies, format!("bodies {krate}"), [names], Pool::Work, cost, key)
    }

    /// A unit compiled, keyed by its key, and its write to the store after it.
    pub fn unit(&mut self, unit: &str, key: Key, deps: &[NodeId], cost: u64) -> Unit {
        // alloc: the node's name, which its `Spec` owns: once per node as the build is laid out
        let compile = self.node(NodeKind::Codegen, format!("compile {unit}"), deps, Pool::Work, cost, Some(key));
        // alloc: the node's name, which its `Spec` owns: once per node as the build is laid out
        let store = self.node(NodeKind::Store, format!("store {unit}"), [compile], Pool::Io, 1, None);
        Unit { compile, store }
    }

    /// A program linked from its units' compiles and the standard library.
    pub fn link(&mut self, program: &str, key: Option<Key>, units: &[Unit], std: Option<NodeId>, cost: u64) -> NodeId {
        let mut deps = Vec::with_capacity(units.len() + usize::from(std.is_some()));
        deps.extend(units.iter().map(|it| it.compile));
        deps.extend(std);
        // alloc: the node's name, which its `Spec` owns: once per node as the build is laid out
        self.node(NodeKind::Link, format!("link {program}"), deps, Pool::Work, cost, key)
    }

    /// A test executable run, after its link: a process, never keyed by the plane (whether it
    /// runs at all is the test selection's, `blaze-usercode`'s `Reached`).
    pub fn test(&mut self, binary: &str, link: NodeId) -> NodeId {
        // alloc: the node's name, which its `Spec` owns: once per node as the build is laid out
        self.node(NodeKind::Test, format!("test {binary}"), [link], Pool::Io, 1, None)
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_types, reason = "a test's own bookkeeping, off every hot path")]
mod tests {
    use blaze_protocol::events::NodeState;

    use super::*;
    use crate::plane::{Plane, PlaneConfig, PlaneInput, PlaneOutput, RequestId};

    fn key(n: u8) -> Key {
        Key([n; 32])
    }

    /// One package with a build script, two units, a program and its test.
    fn build(first: u32) -> (Build, Unit, Unit, NodeId, NodeId) {
        let mut build = Build::new(NodeId(first));
        let manifest = build.manifest("ws", None);
        let fetch = build.fetch("pkg", Some(key(1)), manifest);
        let script = build.script("pkg", key(2), &[fetch], 5);
        let ran = build.script_run("pkg", script, &[]);
        let names = build.names("pkg", None, &[fetch, ran], 5);
        let bodies = build.bodies("pkg", None, names, 5);
        let std = build.std(key(3), 1);
        let a = build.unit("pkg::a", key(4), &[bodies], 10);
        let b = build.unit("pkg::b", key(5), &[bodies], 10);
        let link = build.link("pkg", None, &[a, b], Some(std), 3);
        let test = build.test("pkg", link);
        (build, a, b, link, test)
    }

    #[test]
    fn a_link_waits_for_compiles_not_their_writes() {
        let (build, a, b, link, test) = build(10);
        let specs = build.specs();
        let of = |id: NodeId| specs.iter().find(|it| it.id == id).unwrap();
        assert_eq!(of(link).deps[..2], [a.compile, b.compile]);
        assert!(!of(link).deps.contains(&a.store));
        assert_eq!((of(a.store).pool, of(a.store).deps.as_slice()), (Pool::Io, &[a.compile][..]));
        assert_eq!(of(test).deps, [link]);
        let ids: Vec<u32> = specs.iter().map(|it| it.id.0).collect();
        assert_eq!(ids, (10..10 + ids.len() as u32).collect::<Vec<_>>());
    }

    /// On a plane with one core, a unit's store write starts while the next unit compiles; a
    /// second request of the same build compiles nothing the database has.
    #[test]
    fn store_writes_overlap_compiles_and_a_second_request_reuses_them() {
        use std::{collections::HashMap, sync::Arc};

        use parking_lot::Mutex;

        let db: Arc<Mutex<HashMap<Key, crate::machine::Outcome>>> = Arc::default();
        let memo = {
            let db = Arc::clone(&db);
            move |key: Key| db.lock().get(&key).cloned()
        };
        let config = PlaneConfig { io: 1, work: 1, budget: None, starve_after: 4 };
        let mut plane = Plane::with_memo(config, memo).unwrap();
        let mut at = 0;
        let mut run = |plane: &mut Plane<_>, request: RequestId, specs: Vec<Spec>| {
            let keys: HashMap<NodeId, Key> =
                specs.iter().filter_map(|it| Some((it.id, it.key?))).collect();
            let mut ran = Vec::new();
            let mut running: Vec<NodeId> = Vec::new();
            // Each set of nodes that ran at once.
            let mut together: Vec<Vec<NodeId>> = Vec::new();
            let mut outputs = plane.step(PlaneInput::Open { at_ns: at, request, weight: 1, keep_going: false, idle: false }).unwrap();
            outputs.extend(plane.step(PlaneInput::Add { at_ns: at, request, nodes: specs }).unwrap());
            outputs.extend(plane.step(PlaneInput::Close { at_ns: at, request }).unwrap());
            loop {
                let mut finished = None;
                for output in outputs.drain(..) {
                    match output {
                        PlaneOutput::Run { id, .. } => running.push(id),
                        PlaneOutput::Finished { ok, .. } => finished = Some(ok),
                        PlaneOutput::Said { .. } => {}
                        PlaneOutput::Yield { .. } => unreachable!("no request is idle"),
                    }
                }
                if let Some(ok) = finished {
                    return (ran, together, ok);
                }
                together.push(running.clone());
                at += 1;
                let id = running.remove(0);
                ran.push(id);
                // The value is in the database before its Done, as a task's must be.
                if let Some(key) = keys.get(&id) {
                    db.lock().insert(*key, crate::machine::Outcome::Built);
                }
                outputs = plane
                    .step(PlaneInput::Done {
                        at_ns: at,
                        id,
                        outcome: crate::machine::Outcome::Built,
                        added: Vec::new(),
                        group: None,
                        peak: 0,
                    })
                    .unwrap();
            }
        };
        let (first, a, b, _, _) = build(0);
        let (ran, together, ok) = run(&mut plane, RequestId(1), first.specs());
        assert!(ok);
        assert!(ran.contains(&a.compile) && ran.contains(&b.compile));
        let (stores, compiles) = ([a.store, b.store], [a.compile, b.compile]);
        assert!(
            together.iter().any(|it| it.iter().any(|n| stores.contains(n))
                && it.iter().any(|n| compiles.contains(n))),
            "{together:?}"
        );
        let (second, a2, b2, _, _) = build(100);
        let (ran, _, ok) = run(&mut plane, RequestId(2), second.specs());
        assert!(ok);
        assert!(!ran.contains(&a2.compile) && !ran.contains(&b2.compile), "{ran:?}");
        assert_eq!(plane.state(a2.compile), Some(&NodeState::Cached { remote: false }));
        assert_eq!(plane.state(b2.store).map(NodeState::settled), Some(true));
    }
}
