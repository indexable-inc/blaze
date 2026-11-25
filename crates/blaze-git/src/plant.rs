//! Planted bugs for DST oracle controls (rule `dst-not-review`, `test-the-cause`).
//!
//! Production callers pass [`blaze_own::planted::NoPlant`], so every branch compiles away to
//! nothing. Only tests plant a variant here and assert that the oracle catches it.

#![blaze::sans_io]

/// A deliberate bug planted into [`crate::StatusMachine`] or its parsers to verify test oracles.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GitStatusPlant {
    /// Skip the racy-git check (`entry.mtime >= index.mtime`), trusting matching stat fields even
    /// when a file was modified in the same second as the index was written.
    SkipRacyHashCheck,
    /// Ignore stage > 0 unmerged entries when counting conflicts in `.git/index`.
    SkipConflictCount,
    /// Skip descending into `HEAD` tree vs index when cache-tree root differs from `HEAD` tree,
    /// reporting `staged = 0` falsely.
    SkipStagedDiff,
    /// Ignore `!pattern` negation lines in `.gitignore` rules.
    IgnoreNegation,
    /// Stop ahead/behind commit-graph traversal before checking the second parent of a merge
    /// commit.
    SkipMergeSecondParent,
}

impl blaze_own::planted::Plants for GitStatusPlant {}
