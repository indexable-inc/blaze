//! The view: what is joined to findings when a reply is made, and never stored in one.
//!
//! Levels come from the manifest (`[workspace.metadata.blaze.lints]`, a crate's own table, the
//! ladder's step, the command line) and are joined per finding by its lint and crate. The view
//! also decides the lint universe: a pass runs when any of its lints is above `allow` anywhere
//! (rustc's `lints_that_dont_need_to_run`, inverted), so turning a lint on computes it once and
//! turning it off computes nothing; changing `warn` to `deny` recomputes nothing at all (only the
//! class may move, [`crate::pass::class_of`]).

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::alloc::Global;

use blaze_ids::{ContentId, Hasher};

use crate::finding::{FindingId, LintId};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Level {
    Allow = 0,
    Warn = 1,
    Deny = 2,
    Forbid = 3,
}

/// A crate by its stable id, the scope a level can be set in.
pub type Crate = ContentId;

/// Levels by lint and scope (`None`: every crate), resolved: a crate's own level wins over the
/// workspace's.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct View {
    /// Sorted by (lint, scope), each once.
    levels: Vec<(LintId, Option<Crate>, Level), Global>,
}

impl Default for View {
    fn default() -> View {
        View::new()
    }
}

impl View {
    pub fn new() -> View {
        View { levels: Vec::new_in(Global) }
    }

    pub fn set(&mut self, lint: LintId, scope: Option<Crate>, level: Level) {
        match self.levels.binary_search_by(|&(l, s, _)| (l, s).cmp(&(lint, scope))) {
            Ok(at) => self.levels[at].2 = level,
            Err(at) => self.levels.insert(at, (lint, scope, level)),
        }
    }

    /// The lint's level in `krate`: its own setting, else the workspace's, else allow.
    pub fn level(&self, lint: LintId, krate: Crate) -> Level {
        let find = |scope| {
            self.levels.binary_search_by(|&(l, s, _)| (l, s).cmp(&(lint, scope))).ok().map(|at| self.levels[at].2)
        };
        find(Some(krate)).or_else(|| find(None)).unwrap_or(Level::Allow)
    }

    /// The highest level the lint has anywhere.
    pub fn highest(&self, lint: LintId) -> Level {
        let from = self.levels.partition_point(|&(l, _, _)| l < lint);
        let mut highest = Level::Allow;
        for &(l, _, level) in &self.levels[from..] {
            if l != lint {
                break;
            }
            highest = highest.max(level);
        }
        highest
    }

    /// Whether the lint is computed at all: above allow somewhere.
    pub fn in_universe(&self, lint: LintId) -> bool {
        self.highest(lint) > Level::Allow
    }

    /// The view's digest. Never a key's input: only a planted bug puts it in one.
    pub fn digest(&self) -> ContentId {
        let mut hasher = Hasher::new("blaze-findings view");
        for &(lint, scope, level) in &self.levels {
            hasher.update(&lint.0.to_le_bytes()).opt_part(scope.as_ref().map(|id| id.0)).update(&[level as u8]);
        }
        hasher.finish()
    }
}

/// A set of findings as one value: the base a later reply is a difference from.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Token(pub ContentId);

impl Token {
    /// `ids` sorted.
    pub fn of(ids: &[FindingId]) -> Token {
        let mut hasher = Hasher::new("blaze-findings token");
        for id in ids {
            hasher.id(&id.0);
        }
        Token(hasher.finish())
    }
}

/// What changed between a base and now, both sorted multisets of finding ids.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Delta {
    pub new: Vec<FindingId, Global>,
    pub fixed: Vec<FindingId, Global>,
    pub unchanged: u32,
    pub token: Token,
}

pub fn diff(base: &[FindingId], now: &[FindingId]) -> Delta {
    let (mut new, mut fixed, mut unchanged) = (Vec::new_in(Global), Vec::new_in(Global), 0u32);
    let (mut b, mut n) = (0, 0);
    while b < base.len() || n < now.len() {
        match (base.get(b), now.get(n)) {
            (Some(x), Some(y)) if x == y => {
                unchanged += 1;
                b += 1;
                n += 1;
            }
            (Some(x), Some(y)) if x < y => {
                fixed.push(*x);
                b += 1;
            }
            (Some(_), Some(y)) | (None, Some(y)) => {
                new.push(*y);
                n += 1;
            }
            (Some(x), None) => {
                fixed.push(*x);
                b += 1;
            }
            (None, None) => break,
        }
    }
    Delta { new, fixed, unchanged, token: Token::of(now) }
}
