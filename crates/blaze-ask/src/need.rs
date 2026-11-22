//! What a task lacks: a [`Need`], one fact of one subject, and its key.
//!
//! A need is the engine's cell in the checker's words (blaze-engine `CellId { of: Name { kind,
//! subject }, cell }`): [`Fact`] is the kind of record, [`Need::subject`] the record's subject
//! (an item's library key, a crate's, an opaque's), and [`Need::detail`] the 16 bytes naming the
//! cell inside it (a shape's digest for an impl question, a name for an inherent one, zero for a
//! whole head). [`Fact::engine_kind`] says which engine kind makes each, and names the one that
//! has no kind yet.

use core::fmt;

use blaze_ids::ContentId;

/// The domain of a need's key.
const NEED_DOMAIN: &str = "blaze-ask need v1";

/// The kinds of fact a task can lack.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Fact {
    /// An item's head: a fn's signature, an ADT, a trait, an impl's header, an alias.
    Head = 0,
    /// A trait's members part (its supertraits and associated items).
    Members = 1,
    /// The impls that can answer a trait at a shape, with their headers.
    ImplsOf = 2,
    /// A shape's inherent items of one name, with their impls' headers.
    Inherent = 3,
    /// A lang item's key.
    Lang = 4,
    /// A file's outline.
    Outline = 5,
    /// An opaque type's hidden type: another body's check.
    HiddenType = 6,
    /// A constant's value (an array length, a const argument, a discriminant).
    ConstValue = 7,
}

/// The number of [`Fact`]s: the length of every per-fact counter.
pub const FACTS: usize = 8;

/// Which record a fact is (typeck.md, "Item records and body records").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Record {
    /// Made from items' text alone: the gate settles it before a body is entered, so meeting
    /// one late is a gap of the gate, counted by fact.
    Item,
    /// Made by another body's check: only these are needs a body may meet after entry.
    Body,
}

/// The engine kind that makes a fact, by name (blaze-drive `Kind`): the ask is this crate's,
/// the producer the engine's. `None`: no kind makes it yet (D161 asks the engine for one).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EngineKind {
    Signature,
    ImplBucket,
    FileItems,
    BodyCheck,
    /// Not a kind today: lang items are read from the crate's names (`CrateScope`).
    NoneYet,
}

impl Fact {
    pub const ALL: [Fact; FACTS] =
        [Fact::Head, Fact::Members, Fact::ImplsOf, Fact::Inherent, Fact::Lang, Fact::Outline, Fact::HiddenType, Fact::ConstValue];

    /// The fact whose `as u8` is `byte`.
    pub const fn from_u8(byte: u8) -> Option<Fact> {
        match byte {
            0 => Some(Fact::Head),
            1 => Some(Fact::Members),
            2 => Some(Fact::ImplsOf),
            3 => Some(Fact::Inherent),
            4 => Some(Fact::Lang),
            5 => Some(Fact::Outline),
            6 => Some(Fact::HiddenType),
            7 => Some(Fact::ConstValue),
            _ => None,
        }
    }

    /// Whether the gate can settle it before entry: an item record whose subject the body's
    /// text names (a head, a trait's members, a lang item, an outline, and a trait's whole impl
    /// bucket). A shape's inherent items and an impl question at one shape (a [`Detail`] other
    /// than whole) depend on what inference finds, and a body record is another body's check:
    /// those are met after entry by design and asked through the one ask.
    pub const fn gated(self, detail: Detail) -> bool {
        match self {
            Fact::Head | Fact::Members | Fact::Lang | Fact::Outline => true,
            Fact::ImplsOf => detail.is_whole(),
            Fact::Inherent | Fact::HiddenType | Fact::ConstValue => false,
        }
    }

    /// Its place in per-fact counters.
    pub const fn index(self) -> usize {
        self as usize
    }

    pub const fn name(self) -> &'static str {
        match self {
            Fact::Head => "head",
            Fact::Members => "members",
            Fact::ImplsOf => "impls-of",
            Fact::Inherent => "inherent",
            Fact::Lang => "lang",
            Fact::Outline => "outline",
            Fact::HiddenType => "hidden-type",
            Fact::ConstValue => "const-value",
        }
    }

    /// Item or body record.
    pub const fn record(self) -> Record {
        match self {
            Fact::HiddenType | Fact::ConstValue => Record::Body,
            _ => Record::Item,
        }
    }

    /// The engine kind that makes it. A constant's value is made by a body check today
    /// (blaze-consteval inside head making, audit B2 F7): D161 wants a `ConstValue` kind.
    pub const fn engine_kind(self) -> EngineKind {
        match self {
            Fact::Head | Fact::Members => EngineKind::Signature,
            Fact::ImplsOf | Fact::Inherent => EngineKind::ImplBucket,
            Fact::Outline => EngineKind::FileItems,
            Fact::HiddenType | Fact::ConstValue => EngineKind::BodyCheck,
            Fact::Lang => EngineKind::NoneYet,
        }
    }
}

/// The subject of a fact: the record's key (an item's library key, a crate's key).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Subject(pub ContentId);

/// The cell inside the subject's record (16 bytes, the engine's `Cell`): zero for a whole head.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Detail(pub [u8; 16]);

impl Detail {
    pub const WHOLE: Detail = Detail([0; 16]);

    pub const fn is_whole(self) -> bool {
        let mut i = 0;
        while i < 16 {
            if self.0[i] != 0 {
                return false;
            }
            i += 1;
        }
        true
    }
}

/// One fact of one subject that a task lacks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Need {
    pub fact: Fact,
    pub subject: Subject,
    pub detail: Detail,
}

/// A need's key: the hash of its fact, subject and detail in [`NEED_DOMAIN`] (rule 21: the key
/// covers every part of the need, and nothing of the machine).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NeedKey(pub ContentId);

impl Need {
    pub const fn new(fact: Fact, subject: Subject, detail: Detail) -> Need {
        Need { fact, subject, detail }
    }

    /// A whole record's need (`Detail::WHOLE`).
    pub const fn whole(fact: Fact, subject: Subject) -> Need {
        Need { fact, subject, detail: Detail::WHOLE }
    }

    pub fn key(&self) -> NeedKey {
        let mut bytes = [0u8; 1 + 32 + 16];
        bytes[0] = self.fact as u8;
        bytes[1..33].copy_from_slice(&self.subject.0.0);
        bytes[33..].copy_from_slice(&self.detail.0);
        NeedKey(ContentId::of(NEED_DOMAIN, &bytes))
    }
}

impl NeedKey {
    /// The claim table's key: the digest as four words (the first picks the probe).
    pub fn claim_key(self) -> blaze_core::claim_table::Key {
        let b = self.0.0;
        let w = |i: usize| u64::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3], b[i + 4], b[i + 5], b[i + 6], b[i + 7]]);
        blaze_core::claim_table::Key([w(0), w(8), w(16), w(24)])
    }
}

impl fmt::Display for Need {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = &self.subject.0.0;
        write!(f, "{} of {:02x}{:02x}{:02x}{:02x}", self.fact.name(), s[0], s[1], s[2], s[3])?;
        if !self.detail.is_whole() {
            let d = &self.detail.0;
            write!(f, " at {:02x}{:02x}{:02x}{:02x}", d[0], d[1], d[2], d[3])?;
        }
        Ok(())
    }
}
