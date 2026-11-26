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

/// A deliberate bug planted into the storage, merge, pack, or wire engine to verify DST oracles.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GitEnginePlant {
    /// Skip CRC-32 verification when reading an object record from a `BGOB` slab.
    SkipSlabCrcCheck,
    /// Skip SHA-1 OID verification when unpacking an object from a wire PACK stream.
    SkipPackOidVerify,
    /// Always pick `ours` on overlapping 3-way line edits instead of marking a conflict.
    SkipThreeWayConflict,
    /// Skip comparing old-OID precondition on compare-and-swap ref updates (`git push`).
    SkipCasOldOidCheck,
    /// Corrupt the trailing SHA-1 checksum of generated PACK v2 streams.
    CorruptPackTrailer,
}

impl blaze_own::planted::Plants for GitEnginePlant {}
