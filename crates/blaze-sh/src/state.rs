//! O(1) forkable, copy-on-write shell [`EngineState`].
//!
//! Every collection inside `EngineState` (`vars`, `defs`, `aliases`, `env`, `rust_cells`) is a
//! persistent [`PMap`] or [`PVec`] backed by `blaze_own::Rc`. Calling [`EngineState::fork`] is
//! an O(1) pointer copy, enabling:
//! - Branching a session from any earlier block in history (a tree of sessions);
//! - Transactional execution (run a line on a fork, commit back only on success);
//! - Snapshotting closure environments and parallel stages with zero deep copies.

use blaze_own::{ARc, AVec, Alloc, Rc, Share};

use crate::ast::{Block, ParamDef};
use crate::persist::{PMap, PVec};
use crate::span::Span;
use crate::value::{Record, Value};

/// Variable binding with mutability flag (`let` vs `mut`).
#[derive(Clone, Debug)]
pub struct VarSlot<'a> {
    pub value: Value<'a>,
    pub mutable: bool,
}

impl<'a> Share for VarSlot<'a> {}

/// User-defined command (`def name [params] { body }`).
#[derive(Clone, Debug)]
pub struct DefData<'a> {
    pub name: ARc<'a, str>,
    pub params: AVec<'a, ParamDef<'a>>,
    pub body: ARc<'a, Block<'a>>,
    pub span: Span,
}

/// Recorded Rust REPL cell definition kept in the session's persistent history so later
/// `rust { ... }` cells can reference earlier items, and forking the shell forks the REPL too.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RustCell<'a> {
    pub id: u64,
    pub code: ARc<'a, str>,
}

impl<'a> Share for RustCell<'a> {}

/// Complete persistent shell engine state. Cloning or calling [`EngineState::fork`] is O(1).
#[derive(Clone, Debug)]
pub struct EngineState<'a> {
    pub vars: PMap<'a, ARc<'a, str>, VarSlot<'a>>,
    pub defs: PMap<'a, ARc<'a, str>, ARc<'a, DefData<'a>>>,
    pub aliases: PMap<'a, ARc<'a, str>, ARc<'a, str>>,
    pub env: PMap<'a, ARc<'a, str>, ARc<'a, str>>,
    pub cwd: ARc<'a, str>,
    pub rust_cells: PVec<'a, RustCell<'a>>,
    pub last_exit: i32,
    pub generation: u64,
    alloc: Alloc<'a>,
}

impl<'a> Share for EngineState<'a> {}

impl<'a> EngineState<'a> {
    /// Construct a fresh engine state with an explicit initial `cwd` and `env` pairs (rule 75:
    /// nothing ambient is read from `std::env`).
    pub fn new(cwd: &str, env_pairs: &[(&str, &str)], alloc: Alloc<'a>) -> EngineState<'a> {
        let mut env = PMap::new(alloc);
        for &(k, v) in env_pairs {
            env.insert(Rc::from_str_in(k, alloc), Rc::from_str_in(v, alloc));
        }
        EngineState {
            vars: PMap::new(alloc),
            defs: PMap::new(alloc),
            aliases: PMap::new(alloc),
            env,
            cwd: Rc::from_str_in(cwd, alloc),
            rust_cells: PVec::new(alloc),
            last_exit: 0,
            generation: 0,
            alloc,
        }
    }

    #[inline]
    pub fn alloc(&self) -> Alloc<'a> {
        self.alloc
    }

    /// O(1) copy-on-write fork of the entire engine state (variables, defs, aliases, env,
    /// cwd, and Rust REPL cell history).
    #[inline]
    pub fn fork(&self) -> EngineState<'a> {
        self.share()
    }

    pub fn set_var(&mut self, name: &str, value: Value<'a>, mutable: bool) {
        let k = Rc::from_str_in(name, self.alloc);
        self.vars.insert(k, VarSlot { value, mutable });
        self.generation = self.generation.wrapping_add(1);
    }

    pub fn get_var(&self, name: &str) -> Option<&VarSlot<'a>> {
        self.vars.get(name)
    }

    pub fn set_env(&mut self, name: &str, value: &str) {
        self.env.insert(
            Rc::from_str_in(name, self.alloc),
            Rc::from_str_in(value, self.alloc),
        );
        self.generation = self.generation.wrapping_add(1);
    }

    pub fn get_env(&self, name: &str) -> Option<&str> {
        self.env.get(name).map(|s| s.as_ref())
    }

    pub fn set_cwd(&mut self, cwd: &str) {
        self.cwd = Rc::from_str_in(cwd, self.alloc);
        self.env.insert(
            Rc::from_str_in("PWD", self.alloc),
            self.cwd.share(),
        );
        self.generation = self.generation.wrapping_add(1);
    }

    pub fn push_rust_cell(&mut self, code: &str) -> u64 {
        let id = self.rust_cells.len() as u64 + 1;
        self.rust_cells.push(RustCell { id, code: Rc::from_str_in(code, self.alloc) });
        self.generation = self.generation.wrapping_add(1);
        id
    }

    /// Materialize `$env` as a sorted `Record` (deterministic key order, rule 25).
    pub fn env_record(&self) -> Record<'a> {
        let mut pairs = AVec::with_capacity_in(self.env.len(), self.alloc);
        self.env.collect_entries(&mut pairs);
        pairs.sort_unstable_by(|a, b| a.0.as_ref().cmp(b.0.as_ref()));
        let mut rec = Record::new(self.alloc);
        for (k, v) in pairs.into_iter() {
            rec.insert(k, Value::String(v));
        }
        rec
    }

    /// Capture current variable values into a `PMap` for closure lexical capture.
    pub fn capture_vars(&self) -> PMap<'a, ARc<'a, str>, Value<'a>> {
        let mut out = PMap::new(self.alloc);
        self.vars.for_each(&mut |k, slot| {
            out.insert(k.share(), slot.value.share());
        });
        out
    }
}
