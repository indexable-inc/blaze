//! Facts (docs/v2/plan/design/facts.md): what blaze has proved of an item, as one record per
//! item, read by the optimizer, the code generator and tools through one query,
//! [`facts_of`]. A consumer never works a fact out again: it asks, and where it changes code
//! because of the answer it records that it did ([`Facts::relied`],
//! [`crate::mir::Inlined::rely`]), so the code's key covers the fact (rule
//! `keys-cover-every-input`).
//!
//! hir-ty has the record and the query's slot; the analyses are above it (`hir-ty-pure`: the
//! effect summaries of its one walk, and the type facts), registered with
//! `inventory::submit!` as hir-ty-mir registers MIR's code ([`crate::providers`]). A crate
//! below `hir-ty-pure` that tests itself links no table: [`linked`] is `None` there, nothing
//! is proved, and no pass spends anything. Every binary that generates code links it
//! (`hir-ty-codegen` depends on `hir-ty-pure`).

use base_db::Db;
use blaze_ids::Hasher;
use hir_def::{AdtId, FunctionId};

/// 1: an item's key and the fact's tag.
const RELIED_DOMAIN: &str = "blaze fact relied 1";

/// A fact: one statement about an item, proved by a named analysis. The tag is the fact's
/// identity in every key and record, so a tag is never reused for another statement.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum Fact {
    /// Of a function: no call of it unwinds or aborts (no `panic` in its effect set).
    NoUnwind = 0,
    /// Of a function: no call of it reaches the global allocator.
    NoAlloc = 1,
    /// Of a function: no call of it blocks the thread.
    NoBlock = 2,
    /// Of a function: every call of it returns or panics; none runs for ever.
    Returns = 3,
    /// Of a function: its result depends on its arguments alone and a skipped call would do
    /// nothing else (`#[blaze::pure]`'s check, of a function nobody marked).
    Pure = 4,
    /// Of a type: its instances live until the program exits. (`Acyclic`, which the first
    /// draft had beside it, follows from a type's shape alone: a reflection trait with a
    /// blanket impl, built on `lang-int-1110`, never a fact here; rule `reflection-first`.)
    ProgramLifetime = 5,
}

impl Fact {
    pub const ALL: [Fact; 6] = [
        Fact::NoUnwind,
        Fact::NoAlloc,
        Fact::NoBlock,
        Fact::Returns,
        Fact::Pure,
        Fact::ProgramLifetime,
    ];

    /// The fact's name, in records, `blaze why` and tests.
    pub const fn name(self) -> &'static str {
        match self {
            Fact::NoUnwind => "no_unwind",
            Fact::NoAlloc => "no_alloc",
            Fact::NoBlock => "no_block",
            Fact::Returns => "returns",
            Fact::Pure => "pure",
            Fact::ProgramLifetime => "program_lifetime",
        }
    }
}

/// A set of [`Fact`]s, one bit each.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct FactSet(pub u16);

impl FactSet {
    pub const EMPTY: FactSet = FactSet(0);

    pub const fn one(fact: Fact) -> FactSet {
        FactSet(1 << fact as u8)
    }

    pub const fn has(self, fact: Fact) -> bool {
        self.0 & (1 << fact as u8) != 0
    }

    pub const fn with(self, fact: Fact) -> FactSet {
        FactSet(self.0 | 1 << fact as u8)
    }

    pub const fn union(self, other: FactSet) -> FactSet {
        FactSet(self.0 | other.0)
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Its facts, in tag order.
    pub fn iter(self) -> impl Iterator<Item = Fact> {
        Fact::ALL.into_iter().filter(move |&fact| self.has(fact))
    }
}

/// What a fact is asked of.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FactItem {
    /// A function as defined, in its own where-clauses: a fact of it holds of every instance.
    Fn(FunctionId),
    /// A struct, enum or union as defined: a fact of it holds for arguments that have it too.
    Adt(AdtId),
}

/// Every fact of one item, by how it is established. The three sets are disjoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Facts {
    /// The item's key (`blaze_items::item_key`): which item, the same in every process.
    pub item: [u8; 32],
    /// Proved by an analysis from the code alone, with no attribute believed: sound by
    /// construction, in every crate, whether or not it opted into anything.
    pub inferred: FactSet,
    /// Declared on the item and held by a check that refuses the item when it fails. The
    /// check is a lint (`deny`) today, which an `#[allow]` turns off, so a declared fact is
    /// not spent on code until its check is an error no attribute lowers.
    pub declared: FactSet,
    /// Holds only if a `trusted = "why"` on the way is true: nobody checked it.
    pub trusted: FactSet,
}

impl Facts {
    /// The record of an item nothing is proved of.
    pub const fn none(item: [u8; 32]) -> Facts {
        Facts { item, inferred: FactSet::EMPTY, declared: FactSet::EMPTY, trusted: FactSet::EMPTY }
    }

    /// Whether a pass may change code because of `fact`: it is inferred. A trusted fact is a
    /// promise made by hand and a declared one's check can be allowed away; code compiled on
    /// either would be wrong where they are, which no test of blaze could catch.
    pub const fn spendable(&self, fact: Fact) -> bool {
        self.inferred.has(fact)
    }

    /// Whether the fact holds as far as anyone has said: inferred, declared or trusted. What
    /// a lint or a tool reads.
    pub const fn holds(&self, fact: Fact) -> bool {
        self.inferred.has(fact) || self.declared.has(fact) || self.trusted.has(fact)
    }

    /// What a body that relied on `fact` of this item records ([`crate::mir::Inlined::rely`]):
    /// the item and the fact, nothing else of the item. The item's code may change freely
    /// under it; the day the fact stops holding the pass no longer relies on it, the digest
    /// leaves the body, and the body's key with it.
    pub fn relied(&self, fact: Fact) -> [u8; 32] {
        Hasher::new(RELIED_DOMAIN).update(&self.item).update(&[fact as u8]).finish().0
    }
}

/// How a call names what it calls, as a pass sees the site. A fact is asked of
/// [`Callee::Direct`] alone: for every other the site names no one item whose record could
/// hold of whatever runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Callee {
    /// A function with a body, called by name, of a Rust ABI: the site runs that body.
    Direct,
    /// A closure's body: it has no item of its own to ask a fact of yet (slice 2).
    Closure,
    /// Through a `dyn`: the target is whichever vtable the receiver carries. No bound on the
    /// trait proves anything of it yet (`#[blaze::may(..)]` on the trait is a declared fact).
    Dyn,
    /// Through a function pointer.
    Pointer,
    /// A foreign function: code blaze never saw.
    Foreign,
    /// A function whose ABI is not Rust's (`extern "C-unwind"`, `extern "C"`): unwinding out
    /// of it is the ABI's business, and rustc's rules for it are kept as they are.
    OtherAbi,
    /// A trait method no impl is chosen for in the generic body, an intrinsic, a shim.
    Unresolved,
}

impl Callee {
    /// Whether a fact of the callee may be asked at all.
    pub const fn asked(self) -> bool {
        matches!(self, Callee::Direct)
    }
}

/// A change a pass makes at a call because of what is proved of the callee. Each names every
/// fact it needs ([`Spend::needs`]); a pass makes it only through [`Facts::license`], which
/// gives the digests to record, so no change is made that the body's key does not cover.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Spend {
    /// The call's unwind edge and the cleanup only it reaches go: a callee that cannot
    /// unwind never takes the edge.
    UnwindEdge,
    /// A second call with the same arguments, the first's result still held, takes the
    /// first's result: a pure callee gives the same result, panics the same or runs for
    /// ever the same, and the first call returned.
    MergeRepeat,
    /// A call whose result nothing reads goes: skipping it must not skip a panic, a hang or
    /// anything else it does.
    DeadCall,
    /// A call moves to a place that runs when the call's own place might not (above the
    /// branch that guards it, out of a loop that may run no times): there it must do nothing
    /// a skipped call would not, so it may neither panic nor hang.
    Hoist,
}

/// A bug planted for the controls of the refinement's test (`hir-ty-pure`'s `facts::model`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Plant {
    /// No bug.
    Off,
    /// [`Spend::Hoist`] asks for purity alone: a call is moved above the branch that guards
    /// its panic.
    HoistAsksPureOnly,
    /// [`Facts::license`] gives no digests: a body changes on a fact its key does not name.
    RelyNotRecorded,
}

impl Spend {
    pub const ALL: [Spend; 4] =
        [Spend::UnwindEdge, Spend::MergeRepeat, Spend::DeadCall, Spend::Hoist];

    /// Every fact of the callee the change needs.
    pub const fn needs(self) -> FactSet {
        self.needs_with(Plant::Off)
    }

    #[doc(hidden)]
    pub const fn needs_with(self, plant: Plant) -> FactSet {
        const SKIPPED: FactSet =
            FactSet::one(Fact::Pure).with(Fact::NoUnwind).with(Fact::Returns);
        match self {
            Spend::UnwindEdge => FactSet::one(Fact::NoUnwind),
            Spend::MergeRepeat => FactSet::one(Fact::Pure),
            Spend::DeadCall => SKIPPED,
            Spend::Hoist => match plant {
                Plant::HoistAsksPureOnly => FactSet::one(Fact::Pure),
                Plant::Off | Plant::RelyNotRecorded => SKIPPED,
            },
        }
    }
}

/// What a pass that made a [`Spend`] records: a digest per fact it needed, in tag order.
pub type Relied = smallvec::SmallVec<[[u8; 32]; 3]>;

impl Facts {
    /// Whether a pass may make `spend` at a call of this item, and if so what the body it
    /// changes records ([`crate::mir::Inlined::rely`]): every fact the change needs is
    /// spendable. `None`: leave the call as it is.
    pub fn license(&self, spend: Spend) -> Option<Relied> {
        self.license_with(spend, Plant::Off)
    }

    #[doc(hidden)]
    pub fn license_with(&self, spend: Spend, plant: Plant) -> Option<Relied> {
        let needs = spend.needs_with(plant);
        if !needs.iter().all(|fact| self.spendable(fact)) {
            return None;
        }
        if plant == Plant::RelyNotRecorded {
            return Some(Relied::new());
        }
        Some(needs.iter().map(|fact| self.relied(fact)).collect())
    }
}

/// The analyses' code for [`facts_of`], registered by the crate that has them.
pub struct Provider {
    pub facts_of: for<'db> fn(&'db Db, FactItem) -> &'db Facts,
    /// `cfg!(test)` where the table is built, as [`crate::providers::Mir::own_tests`].
    pub own_tests: bool,
}

inventory::collect!(Provider);

/// The linked table, if the binary links the analyses: the test build's where a crate's own
/// tests link two.
pub fn linked() -> Option<&'static Provider> {
    #[expect(
        clippy::disallowed_types,
        reason = "the process's one facts provider table: selected from the linked providers"
    )]
    static ONE: std::sync::OnceLock<Option<&'static Provider>> = std::sync::OnceLock::new();
    *ONE.get_or_init(|| {
        let mut tested = inventory::iter::<Provider>().filter(|it| it.own_tests);
        if let Some(first) = tested.next() {
            assert!(tested.next().is_none(), "two test builds of the facts provider are linked");
            return Some(first);
        }
        let mut all = inventory::iter::<Provider>();
        let first = all.next()?;
        assert!(all.next().is_none(), "two facts providers are linked");
        Some(first)
    })
}

/// Every fact of `item`: the one query the optimizer, the code generator and tools read.
///
/// Panics, naming what to link, in a binary without the analyses: a caller that can run in
/// one (a MIR pass under a crate's own tests) asks [`linked`] first.
pub fn facts_of(db: &Db, item: FactItem) -> &Facts {
    let table =
        linked().unwrap_or_else(|| panic!("no facts provider is linked: link hir-ty-pure"));
    (table.facts_of)(db, item)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relied_names_the_item_and_the_fact_only() {
        let a = Facts { inferred: FactSet::one(Fact::NoUnwind), ..Facts::none([1; 32]) };
        let more = Facts { inferred: a.inferred.with(Fact::Pure).with(Fact::NoAlloc), ..a.clone() };
        // Proving more of the same item changes nothing a body that relied on one fact keyed.
        assert_eq!(a.relied(Fact::NoUnwind), more.relied(Fact::NoUnwind));
        assert_ne!(a.relied(Fact::NoUnwind), a.relied(Fact::Pure));
        assert_ne!(a.relied(Fact::NoUnwind), Facts::none([2; 32]).relied(Fact::NoUnwind));
    }

    #[test]
    fn only_an_inferred_fact_is_spent() {
        let facts = Facts {
            inferred: FactSet::one(Fact::NoUnwind),
            declared: FactSet::one(Fact::ProgramLifetime),
            trusted: FactSet::one(Fact::NoAlloc),
            ..Facts::none([0; 32])
        };
        assert!(facts.spendable(Fact::NoUnwind));
        assert!(!facts.spendable(Fact::ProgramLifetime) && facts.holds(Fact::ProgramLifetime));
        assert!(!facts.spendable(Fact::NoAlloc) && facts.holds(Fact::NoAlloc));
        assert!(!facts.holds(Fact::Pure));
    }

    #[test]
    fn a_spend_is_licensed_by_every_fact_it_needs_and_records_each() {
        let pure = Facts { inferred: FactSet::one(Fact::Pure), ..Facts::none([3; 32]) };
        assert_eq!(pure.license(Spend::MergeRepeat).as_deref(), Some(&[pure.relied(Fact::Pure)][..]));
        // A pure function may panic or hang: its call is neither skipped nor moved.
        assert_eq!(pure.license(Spend::DeadCall), None);
        assert_eq!(pure.license(Spend::Hoist), None);
        assert_eq!(pure.license(Spend::UnwindEdge), None);
        let total = Facts {
            inferred: pure.inferred.with(Fact::NoUnwind).with(Fact::Returns),
            ..pure.clone()
        };
        let relied = total.license(Spend::Hoist).unwrap();
        assert_eq!(
            relied.as_slice(),
            [total.relied(Fact::NoUnwind), total.relied(Fact::Returns), total.relied(Fact::Pure)]
        );
        // Trusted for it is not proved of it.
        let trusted = Facts { trusted: FactSet::one(Fact::NoUnwind), ..Facts::none([3; 32]) };
        assert_eq!(trusted.license(Spend::UnwindEdge), None);
    }

    #[test]
    fn only_a_direct_callee_is_asked() {
        use Callee::*;
        for callee in [Closure, Dyn, Pointer, Foreign, OtherAbi, Unresolved] {
            assert!(!callee.asked(), "{callee:?}");
        }
        assert!(Direct.asked());
    }
}
