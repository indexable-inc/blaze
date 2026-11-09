//! What fits in an instruction's flag byte, and what a consumer may do with each bit.
//!
//! An operation's [`FlagClass`] (from [`Op::info`]) says which bits it may carry and what
//! each means. Every bit is one of three kinds ([`FlagKind`]):
//!
//! - a **promise** is opt-in UB the builder proved from Rust's semantics, and a consumer may
//!   drop it, which refines the program (`nsw`, `exact`, `inbounds`, fast-math);
//! - a **meaning** must be honoured or strengthened, never dropped: `volatile`, an atomic
//!   ordering (which may only be raised, up to `seq_cst`), a fence's single-thread scope (which
//!   may only widen to the system), a weak `cmpxchg` (which may be made strong);
//! - a **hint** may be dropped (`cold`, `expect_true`, `unpredictable`, `nontemporal`).
//!
//! The bits are those rustc's LLVM backend sets for the pinned nightly, and no more
//! (docs/v2/plan/design/lir.md, "Rust's facts, and where each goes").
//!
//! [`Op::info`]: crate::Op::info

use std::{fmt, ops::BitOr};

/// An instruction's flag byte. Which bits mean what depends on the operation's [`FlagClass`]:
/// the constants below are named for their meaning, and two of different classes may share a
/// bit.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
#[repr(transparent)]
pub struct Flags(pub u8);

impl Flags {
    pub const NONE: Flags = Flags(0);

    // Integer promises (`Wrap`, `Neg`, `Div`, `Rem`, `Or`, `Shl`, `Shr`, `Count`, `Cvt`), and
    // the pointer's (`PtrAdd`, `Mem`).
    /// No signed overflow (`unchecked_add` on a signed type, the checked arm of `+`); on a
    /// signed division, not `MIN / -1`.
    pub const NSW: Flags = Flags(1 << 0);
    /// No unsigned overflow; on `ptr_add`, the offset as unsigned does not wrap the address.
    pub const NUW: Flags = Flags(1 << 1);
    /// No remainder (`exact_div`), no bits shifted out (`lshr exact`).
    pub const EXACT: Flags = Flags(1 << 2);
    /// A division's divisor is not 0 (checked before it), a `clz`'s or `ctz`'s operand is
    /// not 0 (`ctlz_nonzero`).
    pub const NONZERO: Flags = Flags(1 << 3);
    /// A shift amount below the width (`unchecked_shl`), a float in its integer's range
    /// (`to_int_unchecked`).
    pub const IN_RANGE: Flags = Flags(1 << 4);
    /// An `or` of operands with no bit set in both (`disjoint_bitor`).
    pub const DISJOINT: Flags = Flags(1 << 5);
    /// The address stays in the bounds of its pointer's allocation (field projections,
    /// `ptr::add`, `ptr::offset`); on a load or store, its address plus its offset does.
    pub const INBOUNDS: Flags = Flags(1 << 6);

    // Memory: `Mem`, `Bulk`, `Atomic`, `Cmpxchg`.
    /// A meaning: the access is `read_volatile`/`write_volatile`, never merged, moved or
    /// removed.
    pub const VOLATILE: Flags = Flags(1 << 7);
    /// A hint: a store the cache need not keep (`nontemporal_store`).
    pub const NONTEMPORAL: Flags = Flags(1 << 0);
    /// A meaning: a `cmpxchg` that may fail spuriously (`compare_exchange_weak`). Strong is a
    /// strengthening.
    pub const WEAK: Flags = Flags(1 << 6);
    /// A meaning: a fence for the thread alone (`compiler_fence`). The system's is a
    /// strengthening.
    pub const SINGLE_THREAD: Flags = Flags(1 << 3);

    // Floats: `Float`, LLVM's fast-math flags (`fadd_fast`, `*_algebraic`), each a promise.
    pub const NNAN: Flags = Flags(1 << 0);
    pub const NINF: Flags = Flags(1 << 1);
    pub const NSZ: Flags = Flags(1 << 2);
    pub const ARCP: Flags = Flags(1 << 3);
    pub const CONTRACT: Flags = Flags(1 << 4);
    pub const AFN: Flags = Flags(1 << 5);
    pub const REASSOC: Flags = Flags(1 << 6);
    /// Every fast-math flag: what `fadd_fast` and its kin give LLVM.
    pub const FAST: Flags = Flags(0x7f);

    // Control hints: `Br`, `Select`, `Call`.
    /// The condition is likely true (`likely`): rustc's `cond_br_with_expect(.., Some(true))`.
    pub const EXPECT_TRUE: Flags = Flags(1 << 0);
    /// The condition is likely false (`unlikely`).
    pub const EXPECT_FALSE: Flags = Flags(1 << 1);
    /// The condition is not predictable (`select_unpredictable`).
    pub const UNPREDICTABLE: Flags = Flags(1 << 2);
    /// The callee is `#[cold]`.
    pub const COLD: Flags = Flags(1 << 0);

    pub const fn bits(self) -> u8 {
        self.0
    }

    pub const fn contains(self, other: Flags) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn union(self, other: Flags) -> Flags {
        Flags(self.0 | other.0)
    }

    pub const fn without(self, other: Flags) -> Flags {
        Flags(self.0 & !other.0)
    }

    /// The ordering of an `Atomic` or `Fence` class's flags, or a `Cmpxchg`'s on success.
    pub const fn ordering(self) -> Option<Ordering> {
        Ordering::from_bits(self.0 & 0x7)
    }

    /// A `Cmpxchg` class's ordering on failure.
    pub const fn failure(self) -> Option<Ordering> {
        Ordering::from_bits((self.0 >> 3) & 0x7)
    }

    /// These flags with `ordering` in the ordering field.
    pub const fn with_ordering(self, ordering: Ordering) -> Flags {
        Flags((self.0 & !0x7) | ordering as u8)
    }

    /// These flags with `ordering` in a `Cmpxchg`'s failure field.
    pub const fn with_failure(self, ordering: Ordering) -> Flags {
        Flags((self.0 & !(0x7 << 3)) | ((ordering as u8) << 3))
    }
}

impl BitOr for Flags {
    type Output = Flags;

    fn bitor(self, other: Flags) -> Flags {
        self.union(other)
    }
}

/// What a consumer may do with a flag bit.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum FlagKind {
    /// Opt-in UB: may be dropped.
    Promise,
    /// Must be honoured or strengthened.
    Meaning,
    /// May be dropped.
    Hint,
}

/// An atomic ordering. `SeqCst` is 0: an atomic that names none is sequentially consistent.
/// A consumer may raise an ordering (`Relaxed` to `Acquire`, anything to `SeqCst`), never
/// lower it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
#[repr(u8)]
pub enum Ordering {
    SeqCst = 0,
    Relaxed = 1,
    Acquire = 2,
    Release = 3,
    AcqRel = 4,
}

impl Ordering {
    pub const ALL: [Ordering; 5] =
        [Ordering::SeqCst, Ordering::Relaxed, Ordering::Acquire, Ordering::Release, Ordering::AcqRel];

    pub const fn from_bits(bits: u8) -> Option<Ordering> {
        match (bits as usize) < Ordering::ALL.len() {
            true => Some(Ordering::ALL[bits as usize]),
            false => None,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Ordering::SeqCst => "seq_cst",
            Ordering::Relaxed => "relaxed",
            Ordering::Acquire => "acquire",
            Ordering::Release => "release",
            Ordering::AcqRel => "acq_rel",
        }
    }

    pub fn from_name(name: &str) -> Option<Ordering> {
        Ordering::ALL.into_iter().find(|it| it.name() == name)
    }

    /// Whether `self` is at least as strong as `other`: whether a consumer may give `self`
    /// where LIR says `other`. `Acquire` and `Release` are unordered with each other.
    pub const fn at_least(self, other: Ordering) -> bool {
        const fn rank(it: Ordering) -> (u8, u8) {
            // (acquire side, release side)
            match it {
                Ordering::Relaxed => (0, 0),
                Ordering::Acquire => (1, 0),
                Ordering::Release => (0, 1),
                Ordering::AcqRel => (1, 1),
                Ordering::SeqCst => (2, 2),
            }
        }
        let (a, b) = (rank(self), rank(other));
        a.0 >= b.0 && a.1 >= b.1
    }
}

/// An integer comparison, as `icmp`'s first operand.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u8)]
pub enum IntCond {
    Eq,
    Ne,
    Ult,
    Ule,
    Ugt,
    Uge,
    Slt,
    Sle,
    Sgt,
    Sge,
}

impl IntCond {
    pub const ALL: [IntCond; 10] = [
        IntCond::Eq,
        IntCond::Ne,
        IntCond::Ult,
        IntCond::Ule,
        IntCond::Ugt,
        IntCond::Uge,
        IntCond::Slt,
        IntCond::Sle,
        IntCond::Sgt,
        IntCond::Sge,
    ];

    pub const fn from_word(word: u32) -> Option<IntCond> {
        match (word as usize) < IntCond::ALL.len() {
            true => Some(IntCond::ALL[word as usize]),
            false => None,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            IntCond::Eq => "eq",
            IntCond::Ne => "ne",
            IntCond::Ult => "ult",
            IntCond::Ule => "ule",
            IntCond::Ugt => "ugt",
            IntCond::Uge => "uge",
            IntCond::Slt => "slt",
            IntCond::Sle => "sle",
            IntCond::Sgt => "sgt",
            IntCond::Sge => "sge",
        }
    }

    pub fn from_name(name: &str) -> Option<IntCond> {
        IntCond::ALL.into_iter().find(|it| it.name() == name)
    }
}

/// A float comparison, as `fcmp`'s first operand: LLVM's sixteen, ordered (`o*`, false on a
/// NaN) and unordered (`u*`, true on a NaN).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u8)]
pub enum FloatCond {
    False,
    Oeq,
    Ogt,
    Oge,
    Olt,
    Ole,
    One,
    Ord,
    Uno,
    Ueq,
    Ugt,
    Uge,
    Ult,
    Ule,
    Une,
    True,
}

impl FloatCond {
    pub const ALL: [FloatCond; 16] = [
        FloatCond::False,
        FloatCond::Oeq,
        FloatCond::Ogt,
        FloatCond::Oge,
        FloatCond::Olt,
        FloatCond::Ole,
        FloatCond::One,
        FloatCond::Ord,
        FloatCond::Uno,
        FloatCond::Ueq,
        FloatCond::Ugt,
        FloatCond::Uge,
        FloatCond::Ult,
        FloatCond::Ule,
        FloatCond::Une,
        FloatCond::True,
    ];

    pub const fn from_word(word: u32) -> Option<FloatCond> {
        match (word as usize) < FloatCond::ALL.len() {
            true => Some(FloatCond::ALL[word as usize]),
            false => None,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            FloatCond::False => "false",
            FloatCond::Oeq => "oeq",
            FloatCond::Ogt => "ogt",
            FloatCond::Oge => "oge",
            FloatCond::Olt => "olt",
            FloatCond::Ole => "ole",
            FloatCond::One => "one",
            FloatCond::Ord => "ord",
            FloatCond::Uno => "uno",
            FloatCond::Ueq => "ueq",
            FloatCond::Ugt => "ugt",
            FloatCond::Uge => "uge",
            FloatCond::Ult => "ult",
            FloatCond::Ule => "ule",
            FloatCond::Une => "une",
            FloatCond::True => "true",
        }
    }

    pub fn from_name(name: &str) -> Option<FloatCond> {
        FloatCond::ALL.into_iter().find(|it| it.name() == name)
    }
}

/// What an `atomic_rmw` does, as its first operand: Rust's `atomic_xchg` and `atomic_xadd`
/// to `atomic_umin`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u8)]
pub enum AtomicRmw {
    Xchg,
    Add,
    Sub,
    And,
    Nand,
    Or,
    Xor,
    Max,
    Min,
    Umax,
    Umin,
}

impl AtomicRmw {
    pub const ALL: [AtomicRmw; 11] = [
        AtomicRmw::Xchg,
        AtomicRmw::Add,
        AtomicRmw::Sub,
        AtomicRmw::And,
        AtomicRmw::Nand,
        AtomicRmw::Or,
        AtomicRmw::Xor,
        AtomicRmw::Max,
        AtomicRmw::Min,
        AtomicRmw::Umax,
        AtomicRmw::Umin,
    ];

    pub const fn from_word(word: u32) -> Option<AtomicRmw> {
        match (word as usize) < AtomicRmw::ALL.len() {
            true => Some(AtomicRmw::ALL[word as usize]),
            false => None,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            AtomicRmw::Xchg => "xchg",
            AtomicRmw::Add => "add",
            AtomicRmw::Sub => "sub",
            AtomicRmw::And => "and",
            AtomicRmw::Nand => "nand",
            AtomicRmw::Or => "or",
            AtomicRmw::Xor => "xor",
            AtomicRmw::Max => "max",
            AtomicRmw::Min => "min",
            AtomicRmw::Umax => "umax",
            AtomicRmw::Umin => "umin",
        }
    }

    pub fn from_name(name: &str) -> Option<AtomicRmw> {
        AtomicRmw::ALL.into_iter().find(|it| it.name() == name)
    }
}

/// Which bits an operation's flags may have, and what they mean.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum FlagClass {
    /// No flags.
    None,
    /// `add`, `sub`, `mul`: `nsw`, `nuw`.
    Wrap,
    /// `neg`: `nsw`.
    Neg,
    /// `udiv`, `sdiv`: `exact`, `nonzero`, and `nsw` on `sdiv`.
    Div,
    /// `urem`, `srem`: `nonzero`, and `nsw` on `srem`.
    Rem,
    /// `or`: `disjoint`.
    Or,
    /// `shl`: `nsw`, `nuw`, `in_range`.
    Shl,
    /// `lshr`, `ashr`: `exact`, `in_range`.
    Shr,
    /// `clz`, `ctz`: `nonzero`.
    Count,
    /// `fptoui`, `fptosi`: `in_range`.
    Cvt,
    /// Float arithmetic: the fast-math flags.
    Float,
    /// `ptr_add`: `inbounds`, `nuw`.
    PtrAdd,
    /// `load`, `store`: `volatile`, `inbounds`, `nontemporal`.
    Mem,
    /// `memcpy`, `memmove`, `memset`: `volatile`.
    Bulk,
    /// `atomic_load`, `atomic_store`, `atomic_rmw`: an ordering, `volatile`.
    Atomic,
    /// `cmpxchg`: the orderings on success and failure, `weak`, `volatile`.
    Cmpxchg,
    /// `fence`: an ordering, `single_thread`.
    Fence,
    /// `br`: `expect_true`, `expect_false`, `unpredictable`.
    Br,
    /// `select`: `unpredictable`.
    Select,
    /// Calls: `cold`.
    Call,
}

/// A named flag bit of a class: its bit, its text name and its kind.
pub type Named = (Flags, &'static str, FlagKind);

const P: FlagKind = FlagKind::Promise;
const M: FlagKind = FlagKind::Meaning;
const H: FlagKind = FlagKind::Hint;

impl FlagClass {
    /// The single bits of the class, in the order the text form writes them. The ordering
    /// fields of `Atomic`, `Cmpxchg` and `Fence` are not among them ([`FlagClass::orderings`]).
    pub const fn named(self) -> &'static [Named] {
        match self {
            FlagClass::None => &[],
            FlagClass::Wrap => &[(Flags::NSW, "nsw", P), (Flags::NUW, "nuw", P)],
            FlagClass::Neg => &[(Flags::NSW, "nsw", P)],
            FlagClass::Div => &[
                (Flags::NSW, "nsw", P),
                (Flags::EXACT, "exact", P),
                (Flags::NONZERO, "nonzero", P),
            ],
            FlagClass::Rem => &[(Flags::NSW, "nsw", P), (Flags::NONZERO, "nonzero", P)],
            FlagClass::Or => &[(Flags::DISJOINT, "disjoint", P)],
            FlagClass::Shl => &[
                (Flags::NSW, "nsw", P),
                (Flags::NUW, "nuw", P),
                (Flags::IN_RANGE, "in_range", P),
            ],
            FlagClass::Shr => &[(Flags::EXACT, "exact", P), (Flags::IN_RANGE, "in_range", P)],
            FlagClass::Count => &[(Flags::NONZERO, "nonzero", P)],
            FlagClass::Cvt => &[(Flags::IN_RANGE, "in_range", P)],
            FlagClass::Float => &[
                (Flags::NNAN, "nnan", P),
                (Flags::NINF, "ninf", P),
                (Flags::NSZ, "nsz", P),
                (Flags::ARCP, "arcp", P),
                (Flags::CONTRACT, "contract", P),
                (Flags::AFN, "afn", P),
                (Flags::REASSOC, "reassoc", P),
            ],
            FlagClass::PtrAdd => &[(Flags::INBOUNDS, "inbounds", P), (Flags::NUW, "nuw", P)],
            FlagClass::Mem => &[
                (Flags::VOLATILE, "volatile", M),
                (Flags::INBOUNDS, "inbounds", P),
                (Flags::NONTEMPORAL, "nontemporal", H),
            ],
            FlagClass::Bulk => &[(Flags::VOLATILE, "volatile", M)],
            FlagClass::Atomic => &[(Flags::VOLATILE, "volatile", M)],
            FlagClass::Cmpxchg => &[(Flags::WEAK, "weak", M), (Flags::VOLATILE, "volatile", M)],
            FlagClass::Fence => &[(Flags::SINGLE_THREAD, "single_thread", M)],
            FlagClass::Br => &[
                (Flags::EXPECT_TRUE, "expect_true", H),
                (Flags::EXPECT_FALSE, "expect_false", H),
                (Flags::UNPREDICTABLE, "unpredictable", H),
            ],
            FlagClass::Select => &[(Flags::UNPREDICTABLE, "unpredictable", H)],
            FlagClass::Call => &[(Flags::COLD, "cold", H)],
        }
    }

    /// How many ordering fields lead the class's flags: 1 for `Atomic` and `Fence` (bits
    /// 0..3), 2 for `Cmpxchg` (success in 0..3, failure in 3..6).
    pub const fn orderings(self) -> u8 {
        match self {
            FlagClass::Atomic | FlagClass::Fence => 1,
            FlagClass::Cmpxchg => 2,
            FlagClass::None
            | FlagClass::Wrap
            | FlagClass::Neg
            | FlagClass::Div
            | FlagClass::Rem
            | FlagClass::Or
            | FlagClass::Shl
            | FlagClass::Shr
            | FlagClass::Count
            | FlagClass::Cvt
            | FlagClass::Float
            | FlagClass::PtrAdd
            | FlagClass::Mem
            | FlagClass::Bulk
            | FlagClass::Select
            | FlagClass::Br
            | FlagClass::Call => 0,
        }
    }

    /// The bits the ordering fields take.
    const fn ordering_mask(self) -> u8 {
        match self.orderings() {
            0 => 0,
            1 => 0x7,
            _ => 0x3f,
        }
    }

    /// Every bit the class allows.
    pub const fn allowed(self) -> Flags {
        Flags(self.mask_of(None) | self.ordering_mask())
    }

    /// The bits of one kind; the ordering fields are meanings.
    pub const fn of_kind(self, kind: FlagKind) -> Flags {
        let orderings = match kind {
            FlagKind::Meaning => self.ordering_mask(),
            FlagKind::Promise | FlagKind::Hint => 0,
        };
        Flags(self.mask_of(Some(kind)) | orderings)
    }

    const fn mask_of(self, kind: Option<FlagKind>) -> u8 {
        let named = self.named();
        let (mut mask, mut i) = (0, 0);
        while i < named.len() {
            let wanted = match kind {
                None => true,
                Some(kind) => kind as u8 == named[i].2 as u8,
            };
            if wanted {
                mask |= named[i].0.0;
            }
            i += 1;
        }
        mask
    }

    /// `flags` with every promise and hint dropped: what stencils and the wasm emitter keep,
    /// and what a fact-masking bisect gives LLVM.
    pub const fn meanings_only(self, flags: Flags) -> Flags {
        Flags(flags.0 & self.of_kind(FlagKind::Meaning).0)
    }

    /// Why `flags` are not flags of this class, `None` when they are.
    pub const fn check(self, flags: Flags) -> Option<&'static str> {
        if flags.0 & !self.allowed().0 != 0 {
            return Some("a flag bit its operation's class does not have");
        }
        match self.orderings() {
            0 => {}
            1 => {
                if flags.ordering().is_none() {
                    return Some("an ordering that is none of the five");
                }
            }
            _ => {
                let (Some(_), Some(failure)) = (flags.ordering(), flags.failure()) else {
                    return Some("an ordering that is none of the five");
                };
                if matches!(failure, Ordering::Release | Ordering::AcqRel) {
                    return Some("a cmpxchg whose failure ordering releases");
                }
            }
        }
        let both = Flags(Flags::EXPECT_TRUE.0 | Flags::EXPECT_FALSE.0);
        if matches!(self, FlagClass::Br) && flags.contains(both) {
            return Some("a branch expected both ways");
        }
        None
    }
}

/// The flags of an operation of class `class`, as the text form writes them after its name:
/// `.acquire`, `.nsw.nuw`, `.seq_cst.relaxed.weak`.
pub struct Shown {
    pub class: FlagClass,
    pub flags: Flags,
}

impl fmt::Display for Shown {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Shown { class, flags } = *self;
        if class.orderings() >= 1 {
            let ordering = flags.ordering().map_or("?", Ordering::name);
            write!(f, ".{ordering}")?;
        }
        if class.orderings() >= 2 {
            let ordering = flags.failure().map_or("?", Ordering::name);
            write!(f, ".{ordering}")?;
        }
        for &(bit, name, _) in class.named() {
            if flags.contains(bit) {
                write!(f, ".{name}")?;
            }
        }
        Ok(())
    }
}

/// The flags of class `class` the text form's dot-separated `names` say (each after the
/// operation's name): `Err` with the first name the class has not, or a missing ordering.
pub fn parse<'a>(class: FlagClass, names: &[&'a str]) -> Result<Flags, &'a str> {
    let mut flags = Flags::NONE;
    let mut rest = names;
    for field in 0..class.orderings() {
        let Some((&name, tail)) = rest.split_first() else { return Err("") };
        let ordering = Ordering::from_name(name).ok_or(name)?;
        flags = match field {
            0 => flags.with_ordering(ordering),
            _ => flags.with_failure(ordering),
        };
        rest = tail;
    }
    for &name in rest {
        let &(bit, _, _) = class.named().iter().find(|it| it.1 == name).ok_or(name)?;
        flags = flags | bit;
    }
    Ok(flags)
}

#[cfg(test)]
mod tests {
    use super::{FlagClass, FlagKind, Flags, Ordering};

    #[test]
    fn kinds_split_every_allowed_bit() {
        let classes = [
            FlagClass::None,
            FlagClass::Wrap,
            FlagClass::Neg,
            FlagClass::Div,
            FlagClass::Rem,
            FlagClass::Or,
            FlagClass::Shl,
            FlagClass::Shr,
            FlagClass::Count,
            FlagClass::Cvt,
            FlagClass::Float,
            FlagClass::PtrAdd,
            FlagClass::Mem,
            FlagClass::Bulk,
            FlagClass::Atomic,
            FlagClass::Cmpxchg,
            FlagClass::Fence,
            FlagClass::Br,
            FlagClass::Select,
            FlagClass::Call,
        ];
        for class in classes {
            let [p, m, h] =
                [FlagKind::Promise, FlagKind::Meaning, FlagKind::Hint].map(|it| class.of_kind(it).0);
            assert_eq!(p & m, 0, "{class:?}");
            assert_eq!(p & h, 0, "{class:?}");
            assert_eq!(m & h, 0, "{class:?}");
            assert_eq!(p | m | h, class.allowed().0, "{class:?}");
        }
    }

    #[test]
    fn orderings_only_strengthen() {
        assert!(Ordering::SeqCst.at_least(Ordering::Relaxed));
        assert!(Ordering::AcqRel.at_least(Ordering::Acquire));
        assert!(!Ordering::Acquire.at_least(Ordering::Release));
        assert!(!Ordering::Relaxed.at_least(Ordering::Acquire));
        let flags = Flags::NONE.with_ordering(Ordering::Acquire).with_failure(Ordering::Relaxed);
        assert_eq!(flags.ordering(), Some(Ordering::Acquire));
        assert_eq!(flags.failure(), Some(Ordering::Relaxed));
        assert!(FlagClass::Cmpxchg.check(flags.with_failure(Ordering::Release)).is_some());
    }
}
