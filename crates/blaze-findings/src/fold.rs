//! A workspace fold: findings that need every item's rows (duplicates, results no test
//! observes, fields never read, code only tests call, a parameter whose values at every call lie
//! in a narrower type), updated by the delta of the items that moved, never recomputed.
//!
//! **Shape.** Rows are `(subject, contributor, value)`: the subject is what a finding is about
//! (a function's parameter, a field), the contributor is the item whose [`crate::kinds::ItemRows`]
//! record holds the row (a caller, a reader). Rows are bucketed by subject into `L` leaves; a leaf
//! holds its rows sorted and the verdict of each of its subjects; the root holds the leaf
//! digests. Each leaf and the root is a [`crate::kinds::FoldFindings`] record keyed by its digest,
//! so a leaf is a function of its contents and is shared between machines that hold the same rows
//! (rule 25).
//!
//! **Delta.** An item moved: its old rows (from its previous record) and its new rows name the
//! subjects it touches. Only those subjects' leaves are rewritten: the item's rows in them are
//! replaced, each touched subject's verdict is recomputed from that subject's rows alone (a join
//! need not be invertible: min and max are not), and the root's entries for those leaves change.
//!
//! **Cost against the floor.** The floor of an edit is the touched subjects' rows: an oracle
//! redoes each touched subject's join and nothing else, plus writing what changed. This design
//! rewrites each touched leaf whole (`R / L` rows) and the root (`32 L` bytes). With `L` sized so
//! a leaf holds about 64 rows (`L = R / 64`, a power of two), a one-function edit that touches
//! `t` subjects costs `t` leaves of 64 rows plus a root of `R / 2` bytes: for `R = 2^20` rows
//! (reth's size) that is 0.5 MB of root per edit, about 1 ms to hash, against an edit floor of
//! 10 to 30 ms: within 10% of the edit, not of the fold's own floor in bytes. Past `2^20` rows the
//! root needs a middle level (leaves of leaves), which brings it to `t * 64 * 32` bytes; not built
//! yet, said here rather than built around.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::alloc::Global;
use std::fmt::Debug;

use blaze_ids::{ContentId, Hasher};
use blaze_keyed::record::StableId;

use crate::{Plant, planted};

/// What a finding of a fold is about: a parameter, a field, a function.
pub type Subject = ContentId;

/// How the rows of one subject become its verdict.
pub trait Join {
    type Value: Copy + Eq + Ord + Debug;
    type Verdict: Copy + Eq + Debug;
    /// Associative and commutative: the order rows arrive in never shows.
    fn join(a: Self::Value, b: Self::Value) -> Self::Value;
    /// The finding the joined rows make, if any.
    fn decide(joined: Self::Value) -> Option<Self::Verdict>;
    /// The value's bytes for a leaf's digest.
    fn hash(value: &Self::Value, hasher: &mut Hasher);
}

/// One row: what `contributor` says about `subject`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Row<V> {
    pub subject: Subject,
    pub contributor: StableId,
    pub value: V,
}

struct Leaf<J: Join> {
    /// Sorted by (subject, contributor, value).
    rows: Vec<Row<J::Value>, Global>,
    /// Sorted by subject: each subject with a verdict.
    verdicts: Vec<(Subject, J::Verdict), Global>,
    digest: ContentId,
}

/// What a delta changed.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Changed<V> {
    /// Leaves rewritten, sorted: the records filed again.
    pub leaves: Vec<u32, Global>,
    /// Subjects whose verdict changed: (subject, before, after).
    pub verdicts: Vec<(Subject, Option<V>, Option<V>), Global>,
}

pub struct Fold<J: Join> {
    leaves: Vec<Leaf<J>, Global>,
}

/// The leaf of `subject` among `count` (a power of two).
pub fn leaf_of(subject: &Subject, count: u32) -> u32 {
    let mut word = [0u8; 4];
    word.copy_from_slice(&subject.0[..4]);
    u32::from_le_bytes(word) & (count - 1)
}

/// The leaf count for `rows` rows: a power of two, about 64 rows a leaf.
pub fn leaves_for(rows: u64) -> u32 {
    let wanted = (rows / 64).clamp(1, 1 << 24);
    u32::try_from(wanted.next_power_of_two()).unwrap_or(1 << 24)
}

impl<J: Join> Fold<J> {
    /// An empty fold of `count` leaves (rounded up to a power of two).
    pub fn new(count: u32) -> Fold<J> {
        let count = count.max(1).next_power_of_two();
        let mut leaves = Vec::with_capacity_in(count as usize, Global);
        for _ in 0..count {
            let mut leaf = Leaf { rows: Vec::new_in(Global), verdicts: Vec::new_in(Global), digest: ContentId::default() };
            leaf.digest = Self::leaf_digest(&leaf.rows);
            leaves.push(leaf);
        }
        Fold { leaves }
    }

    /// The reference: the fold of `rows` computed whole.
    pub fn from_scratch(count: u32, rows: &[Row<J::Value>]) -> Fold<J> {
        let mut fold = Fold::new(count);
        let n = fold.leaves.len() as u32;
        for row in rows {
            fold.leaves[leaf_of(&row.subject, n) as usize].rows.push(*row);
        }
        for leaf in &mut fold.leaves {
            // A leaf's rows are a set: a contributor saying one thing twice is one row.
            leaf.rows.sort_unstable();
            leaf.rows.dedup();
            leaf.verdicts.clear();
            let mut at = 0;
            while at < leaf.rows.len() {
                let subject = leaf.rows[at].subject;
                let end = at + leaf.rows[at..].partition_point(|row| row.subject == subject);
                if let Some(verdict) = Self::decide(&leaf.rows[at..end]) {
                    leaf.verdicts.push((subject, verdict));
                }
                at = end;
            }
            leaf.digest = Self::leaf_digest(&leaf.rows);
        }
        fold
    }

    fn decide(rows: &[Row<J::Value>]) -> Option<J::Verdict> {
        let mut values = rows.iter().map(|row| row.value);
        let first = values.next()?;
        J::decide(values.fold(first, J::join))
    }

    /// The digest of a leaf's rows: the key's input of its record.
    pub fn leaf_digest(rows: &[Row<J::Value>]) -> ContentId {
        let mut hasher = Hasher::new("blaze-findings fold leaf");
        for row in rows {
            hasher.id(&row.subject).id(&row.contributor.0);
            J::hash(&row.value, &mut hasher);
        }
        hasher.finish()
    }

    pub fn leaf_count(&self) -> u32 {
        self.leaves.len() as u32
    }

    /// The digest of the leaf digests: the root record's key input.
    pub fn root_digest(&self) -> ContentId {
        let mut hasher = Hasher::new("blaze-findings fold root");
        for leaf in &self.leaves {
            hasher.id(&leaf.digest);
        }
        hasher.finish()
    }

    pub fn leaf(&self, leaf: u32) -> (ContentId, &[Row<J::Value>]) {
        let leaf = &self.leaves[leaf as usize];
        (leaf.digest, &leaf.rows)
    }

    /// Every verdict, by subject.
    pub fn verdicts(&self) -> impl Iterator<Item = (Subject, J::Verdict)> + '_ {
        self.leaves.iter().flat_map(|leaf| leaf.verdicts.iter().copied())
    }

    pub fn verdict(&self, subject: &Subject) -> Option<J::Verdict> {
        let leaf = &self.leaves[leaf_of(subject, self.leaf_count()) as usize];
        let at = leaf.verdicts.binary_search_by(|(s, _)| s.cmp(subject)).ok()?;
        Some(leaf.verdicts[at].1)
    }

    /// `contributor` moved: its rows were `old` and are `new` (each as (subject, value)).
    pub fn apply(&mut self, contributor: StableId, old: &[(Subject, J::Value)], new: &[(Subject, J::Value)]) -> Changed<J::Verdict> {
        let n = self.leaf_count();
        let mut subjects = Vec::with_capacity_in(old.len() + new.len(), Global);
        for &(subject, _) in old.iter().chain(new) {
            subjects.push(subject);
        }
        subjects.sort_unstable();
        subjects.dedup();
        let mut leaves = Vec::new_in(Global);
        for subject in &subjects {
            leaves.push(leaf_of(subject, n));
        }
        leaves.sort_unstable();
        leaves.dedup();

        let mut verdicts = Vec::new_in(Global);
        for &index in &leaves {
            let leaf = &mut self.leaves[index as usize];
            if !planted(Plant::FoldKeepsStaleRows) {
                leaf.rows.retain(|row| row.contributor != contributor);
            }
            for &(subject, value) in new {
                if leaf_of(&subject, n) == index {
                    leaf.rows.push(Row { subject, contributor, value });
                }
            }
            leaf.rows.sort_unstable();
            leaf.rows.dedup();
            for &subject in subjects.iter().filter(|s| leaf_of(s, n) == index) {
                let from = leaf.rows.partition_point(|row| row.subject < subject);
                let to = leaf.rows.partition_point(|row| row.subject <= subject);
                let after = Self::decide(&leaf.rows[from..to]);
                let found = leaf.verdicts.binary_search_by(|(s, _)| s.cmp(&subject));
                let before = found.ok().map(|at| leaf.verdicts[at].1);
                match (found, after) {
                    (Ok(at), Some(verdict)) => leaf.verdicts[at].1 = verdict,
                    (Ok(at), None) => {
                        leaf.verdicts.remove(at);
                    }
                    (Err(at), Some(verdict)) => leaf.verdicts.insert(at, (subject, verdict)),
                    (Err(_), None) => {}
                }
                if before != after {
                    verdicts.push((subject, before, after));
                }
            }
            leaf.digest = Self::leaf_digest(&leaf.rows);
        }
        Changed { leaves, verdicts }
    }

    /// Whether two folds hold the same rows and verdicts: the oracle's comparison.
    pub fn same_as(&self, other: &Fold<J>) -> bool {
        self.leaves.len() == other.leaves.len()
            && self.leaves.iter().zip(&other.leaves).all(|(a, b)| a.digest == b.digest && a.verdicts == b.verdicts)
    }
}

/// The reference join of `could_be_narrower`: what every site of a parameter (or field, or
/// return) says about its values.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Observed {
    /// The declared integer type, from the item's own row: (signed, bits).
    pub declared: Option<(bool, u8)>,
    /// The values seen, `lo..=hi`; empty (`lo > hi`) when no site gave a known value.
    pub lo: i128,
    pub hi: i128,
    pub zero: bool,
    /// A site whose value is not known: no narrowing can be proved.
    pub any: bool,
    pub sites: u32,
}

impl Observed {
    pub fn declared(signed: bool, bits: u8) -> Observed {
        Observed { declared: Some((signed, bits)), lo: i128::MAX, hi: i128::MIN, zero: false, any: false, sites: 0 }
    }

    pub fn value(value: i128) -> Observed {
        Observed { declared: None, lo: value, hi: value, zero: value == 0, any: false, sites: 1 }
    }

    pub fn unknown() -> Observed {
        Observed { declared: None, lo: i128::MAX, hi: i128::MIN, zero: false, any: true, sites: 1 }
    }
}

/// What a narrower type would be.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Narrower {
    /// Every value is at least zero in a signed type.
    pub unsigned: bool,
    /// No value is zero: `NonZero`.
    pub nonzero: bool,
    /// The bits the values need, when fewer than declared (8, 16, 32 or 64).
    pub bits: Option<u8>,
    pub lo: i128,
    pub hi: i128,
}

pub struct Narrow;

/// The bits an integer type needs to hold `lo..=hi`: 8, 16, 32, 64 or 128.
pub fn bits_for(lo: i128, hi: i128) -> u8 {
    let signed = lo < 0;
    let mut bits = 8u8;
    loop {
        let fits = match signed {
            true => bits == 128 || (lo >= -(1i128 << (bits - 1)) && hi < (1i128 << (bits - 1))),
            false => bits == 128 || hi < (1i128 << bits),
        };
        if fits {
            return bits;
        }
        bits *= 2;
    }
}

impl Join for Narrow {
    type Value = Observed;
    type Verdict = Narrower;

    fn join(a: Observed, b: Observed) -> Observed {
        Observed {
            declared: a.declared.or(b.declared),
            lo: a.lo.min(b.lo),
            hi: a.hi.max(b.hi),
            zero: a.zero || b.zero,
            any: a.any || b.any,
            sites: a.sites.saturating_add(b.sites),
        }
    }

    fn decide(joined: Observed) -> Option<Narrower> {
        let (signed, declared_bits) = joined.declared?;
        if joined.any || joined.sites == 0 || joined.lo > joined.hi {
            return None;
        }
        let unsigned = signed && joined.lo >= 0;
        let nonzero = !joined.zero;
        let needed = bits_for(joined.lo, joined.hi);
        let bits = (needed < declared_bits).then_some(needed);
        (unsigned || nonzero || bits.is_some()).then_some(Narrower { unsigned, nonzero, bits, lo: joined.lo, hi: joined.hi })
    }

    fn hash(value: &Observed, hasher: &mut Hasher) {
        let (signed, bits) = value.declared.unwrap_or((false, 0));
        hasher
            .update(&[u8::from(value.declared.is_some()), u8::from(signed), bits, u8::from(value.zero), u8::from(value.any)])
            .update(&value.lo.to_le_bytes())
            .update(&value.hi.to_le_bytes())
            .update(&value.sites.to_le_bytes());
    }
}
