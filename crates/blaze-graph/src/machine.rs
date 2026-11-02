//! The build's graph as a state machine (docs/driver.md, "The step function").
//!
//! [`Machine::step`] takes an [`Input`]: nodes added, no more to be added, or what came of a
//! node it asked to run. It gives back [`Output`]s: the nodes to run now, the records of
//! [`blaze_protocol::events`] that say every node and every move, and the end of the build. It
//! does nothing else: the time is in the input, and the same inputs give the same outputs.
//!
//! Of the nodes that are ready, the one with the longest path of expected cost from it to the
//! end of the build runs first, so what the build waits for longest is never behind what it
//! does not wait for; ties go to the node added first.
//!
//! A machine is the [`Plane`] with one request (docs/v2/plan/lanes/core/plane.md): one scheduler, whose
//! one-build case this is, so a build script graph and a daemon's many requests are scheduled by
//! the same rules.

use std::fmt;

use blaze_protocol::events::{NodeKind, NodeState};
use serde::{Deserialize, Serialize};

use crate::plane::{Key, Plane, PlaneConfig, PlaneInput, PlaneOutput, RequestId};

/// A node's name in the graph, chosen by whoever adds it.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(transparent)]
pub struct NodeId(pub u32);

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{}", self.0)
    }
}

/// Where a node's work is done: [`Pool::Io`] waits on files, the network or a process;
/// [`Pool::Work`] uses a core.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Pool {
    Io,
    Work,
}

/// A node to add. Its `deps` are added with it, in the same input in any order, or before.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)] // clone: in an `Input` an opt-in trace keeps, and tests
pub struct Spec {
    pub id: NodeId,
    pub kind: NodeKind,
    pub name: String,
    pub deps: Vec<NodeId>,
    pub pool: Pool,
    /// What it is expected to cost, in a unit that is the same for every node of the build:
    /// it orders the ready nodes and nothing else.
    pub cost: u64,
    /// The hash of everything its value depends on, for the [`crate::plane::Plane`]'s memo and
    /// single flight; `None` for work that is not a pure function of its inputs.
    #[serde(default)]
    pub key: Option<Key>,
    /// Its predicted peak memory in bytes ([`crate::admission`]), for the plane's admission.
    #[serde(default)]
    pub memory: u64,
}

/// What came of a node's work.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)] // clone: a memo's copy, allocating only for `Failed`
pub enum Outcome {
    Built,
    Cached { remote: bool },
    Failed { why: String },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)] // clone: only into a trace the caller asked for, and tests
pub enum Input {
    /// Nodes the build has besides those it had.
    Add { at_ns: u64, nodes: Vec<Spec> },
    /// No more nodes are added but by [`Input::Done`]: the build ends once every node is
    /// settled.
    Close { at_ns: u64 },
    /// A node the machine asked to run is done, and its work found the nodes `added`.
    Done { at_ns: u64, id: NodeId, outcome: Outcome, added: Vec<Spec> },
}

impl Input {
    pub fn at_ns(&self) -> u64 {
        match self {
            Input::Add { at_ns, .. } | Input::Close { at_ns } | Input::Done { at_ns, .. } => *at_ns,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum Output {
    /// A record of the build: a node, a move, the critical path.
    Said(blaze_protocol::events::Record),
    /// Run `id`'s work on `pool`, then give the machine an [`Input::Done`] of it.
    Run { id: NodeId, pool: Pool },
    /// Every node is settled. Nothing more comes.
    Finished { ok: bool },
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// How many nodes of [`Pool::Io`] run at once.
    pub io: usize,
    /// How many nodes of [`Pool::Work`] run at once: the cores.
    pub work: usize,
    /// Whether nodes that do not wait for a failed one still run after it failed. When not,
    /// the build ends once what runs is done, as cargo's does.
    pub keep_going: bool,
}

/// An input the machine cannot take. The machine is as it was before it.
#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    NoThreads(Pool),
    Twice(NodeId),
    UnknownDep {
        node: NodeId,
        dep: NodeId,
    },
    /// Nodes of one input that wait for one another.
    Cycle(Vec<NodeId>),
    NotRunning(NodeId),
    Closed,
    Over,
    /// A plane's request opened twice.
    RequestTwice(u32),
    /// A plane's request opened with no share of the threads.
    NoWeight(u32),
    UnknownRequest(u32),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NoThreads(pool) => write!(f, "the {pool:?} pool runs no node at once"),
            Error::Twice(id) => write!(f, "node {id} is added twice"),
            Error::UnknownDep { node, dep } => {
                write!(f, "node {node} waits for {dep}, which is not added")
            }
            Error::Cycle(nodes) => {
                // alloc: an error's message, once
                let nodes: Vec<String> = nodes.iter().map(ToString::to_string).collect();
                write!(f, "nodes {} wait for one another", nodes.join(", "))
            }
            Error::NotRunning(id) => write!(f, "node {id} is said done, but it is not running"),
            Error::Closed => write!(f, "nodes added or the build closed after it was closed"),
            Error::Over => write!(f, "an input after the build finished"),
            Error::RequestTwice(id) => write!(f, "request {id} is opened twice"),
            Error::NoWeight(id) => write!(f, "request {id} is opened with a weight of 0"),
            Error::UnknownRequest(id) => write!(f, "request {id} is not open"),
        }
    }
}

impl std::error::Error for Error {}

/// The machine's one request of its plane.
const BUILD: RequestId = RequestId(0);

pub struct Machine {
    config: Config,
    plane: Plane,
    over: bool,
    /// The plane's outputs of a step, drained each step and kept for the next.
    said: Vec<PlaneOutput>,
}

impl Machine {
    pub fn new(config: Config) -> Result<Machine, Error> {
        let mut plane = Plane::new(PlaneConfig {
            io: config.io,
            work: config.work,
            budget: None,
            starve_after: u32::MAX,
        })?;
        let opened = plane.step(PlaneInput::Open {
            at_ns: 0,
            request: BUILD,
            weight: 1,
            keep_going: config.keep_going,
            idle: false,
        })?;
        debug_assert!(opened.is_empty(), "opening a request says nothing");
        // alloc: none, empty: the plane's outputs, kept across steps
        Ok(Machine { config, plane, over: false, said: Vec::new() })
    }

    pub fn config(&self) -> Config {
        self.config
    }

    /// The state of `id`, if it was added.
    pub fn state(&self, id: NodeId) -> Option<&NodeState> {
        self.plane.state(id)
    }

    pub fn is_over(&self) -> bool {
        self.over
    }

    pub fn step(&mut self, input: Input) -> Result<Vec<Output>, Error> {
        // alloc: the step's outputs, the API's payload; `step_into` takes a kept buffer
        let mut out = Vec::new();
        self.step_into(input, &mut out)?;
        Ok(out)
    }

    /// [`Machine::step`], with its outputs appended to `out`: a caller that keeps one buffer and
    /// drains it gets them with no allocation per step. An input refused appends nothing and
    /// changes nothing.
    pub fn step_into(&mut self, input: Input, out: &mut Vec<Output>) -> Result<(), Error> {
        if self.over {
            return Err(Error::Over);
        }
        let input = match input {
            Input::Add { at_ns, nodes } => PlaneInput::Add { at_ns, request: BUILD, nodes },
            Input::Close { at_ns } => PlaneInput::Close { at_ns, request: BUILD },
            Input::Done { at_ns, id, outcome, added } => {
                PlaneInput::Done { at_ns, id, outcome, added, group: None, peak: 0 }
            }
        };
        let mut said = std::mem::take(&mut self.said);
        let taken = self.plane.step_into(input, &mut said);
        if taken.is_ok() {
            out.extend(said.drain(..).map(|output| match output {
                PlaneOutput::Said { record, .. } => Output::Said(record),
                PlaneOutput::Run { id, pool, .. } => Output::Run { id, pool },
                PlaneOutput::Finished { ok, .. } => {
                    self.over = true;
                    Output::Finished { ok }
                }
                PlaneOutput::Yield { id } => {
                    unreachable!("node {id} asked to give way: the machine opens no idle request")
                }
            }));
        }
        said.clear();
        self.said = said;
        taken
    }
}
