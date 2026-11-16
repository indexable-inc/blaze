//! Wake lists: what each pending item's last plan read, so a pass plans only the items whose
//! reads changed since (semi-naive evaluation over the pass loop).
//!
//! A plan is a function of the state it reads. Every read of a member's state is a key: a
//! module and a name (`lookup_module`'s declarations, named imports and glob result for that
//! name), or a module alone (its fragments, globs and calls: what an expansion, a glob or a
//! call changes). Each member stamps a key with the group's clock when it changes the state
//! the key covers; an item planned at clock `e` is clean while no key it read has a stamp
//! above `e`, and a clean item's plan is the one it made then, so it is not made again. An
//! item whose plan moved something, needed an answer or waited on one in progress stays
//! dirty: its inputs are not all member state.
//!
//! The floor this aims at: each pass plans the items that read a changed key, so the work of
//! a fill is proportional to the changes times the items that read each one.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use blaze_hash::HashMap;
use blaze_id::ItemKey;
use blaze_own::{AVec, Alloc, Vec};

use crate::facts::Name;

/// A read of a member's state: a module, and the name looked up in it (`None`: its fragments,
/// globs and calls).
pub(crate) type Key<N> = (ItemKey, Option<N>);

/// One pending item's last plan.
pub(crate) struct Watch<'a, N> {
    /// The clock it was planned at; `None` until it is planned in an early pass.
    pub(crate) epoch: Option<u64>,
    /// Its plan waited on an answer not given yet.
    pub(crate) wait: bool,
    /// Its plan moved something, or read an answer not given yet: planned again regardless.
    pub(crate) sticky: bool,
    /// The keys it read, sorted, once each.
    pub(crate) keys: AVec<'a, Key<N>>,
}

/// The stamps of one member's state, and its items' last plans.
pub(crate) struct Stamps<'a, N> {
    /// The group's clock, as last set by the loop.
    pub(crate) clock: u64,
    /// When the module's fragments, globs or calls last changed.
    pub(crate) wild: u64,
    /// When the state behind each name last changed (its named imports).
    pub(crate) names: HashMap<N, u64, Alloc<'a>>,
    pub(crate) imports: AVec<'a, Watch<'a, N>>,
    pub(crate) globs: AVec<'a, Watch<'a, N>>,
    pub(crate) calls: AVec<'a, Watch<'a, N>>,
}

impl<'a, N: Name> Stamps<'a, N> {
    pub(crate) fn new_in(alloc: Alloc<'a>) -> Stamps<'a, N> {
        Stamps {
            clock: 0,
            wild: 0,
            names: HashMap::new_in(alloc.clone()),
            imports: Vec::new_in(alloc.clone()),
            globs: Vec::new_in(alloc.clone()),
            calls: Vec::new_in(alloc),
        }
    }

    /// Everything forgotten: every item dirty, every stamp zero (a machine joining a group,
    /// whose clock starts again).
    pub(crate) fn reset(&mut self) {
        self.clock = 0;
        self.wild = 0;
        self.names.clear();
        for w in self.imports.iter_mut().chain(self.globs.iter_mut()).chain(self.calls.iter_mut()) {
            w.epoch = None;
        }
    }

    pub(crate) fn stamp_wild(&mut self) {
        self.wild = self.clock;
    }

    pub(crate) fn stamp_name(&mut self, name: N) {
        let clock = self.clock;
        self.names.insert(name, clock);
    }

    /// Whether a key of this module changed after `epoch`.
    pub(crate) fn changed(&self, name: Option<N>, epoch: u64) -> bool {
        if self.wild > epoch {
            return true;
        }
        match name {
            Some(n) => self.names.get(&n).is_some_and(|&at| at > epoch),
            None => false,
        }
    }
}

/// Which list of items a watch is in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Kind {
    Import,
    Glob,
    Call,
}

/// The watch of item `i` of `kind`, made if it is new.
pub(crate) fn watch_mut<'s, 'a, N: Name>(
    s: &'s mut Stamps<'a, N>,
    kind: Kind,
    i: usize,
    alloc: Alloc<'a>,
) -> &'s mut Watch<'a, N> {
    let list = match kind {
        Kind::Import => &mut s.imports,
        Kind::Glob => &mut s.globs,
        Kind::Call => &mut s.calls,
    };
    while list.len() <= i {
        list.push(Watch { epoch: None, wait: false, sticky: true, keys: Vec::new_in(alloc.clone()) });
    }
    &mut list[i]
}

/// The watch of item `i` of `kind`, if it was planned.
pub(crate) fn watch<'s, 'a, N>(s: &'s Stamps<'a, N>, kind: Kind, i: usize) -> Option<&'s Watch<'a, N>> {
    let list = match kind {
        Kind::Import => &s.imports,
        Kind::Glob => &s.globs,
        Kind::Call => &s.calls,
    };
    list.get(i)
}
