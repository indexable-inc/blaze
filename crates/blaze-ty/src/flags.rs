//! What a node holds, computed once when it is interned from its kind and its children's flags
//! (rustc_type_ir/src/flags.rs, `FlagComputation`), so "is this ground?", "does a substitution
//! change it?" and "does it have bound variables escaping depth d?" are one load and a mask.
//!
//! Twelve yes-or-no bits, and in the top four the outer exclusive binder (rustc's
//! `outer_exclusive_binder`): the least `d` such that no bound variable inside escapes `d`
//! binders above the node. It saturates at 15, which reads as "maybe any depth": a walk enters.

use blaze_int::Cast;

/// A node's flags.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Flags(pub(crate) u16);

impl Flags {
    /// A type, int or float inference variable.
    pub const TY_INFER: u16 = 1 << 0;
    /// A region variable.
    pub const RE_INFER: u16 = 1 << 1;
    /// A const variable.
    pub const CT_INFER: u16 = 1 << 2;
    /// A generic parameter (type, early-bound region, const).
    pub const PARAM: u16 = 1 << 3;
    /// A placeholder of any class.
    pub const PLACEHOLDER: u16 = 1 << 4;
    /// An alias of any kind, rigid or not, or an unevaluated const.
    pub const ALIAS: u16 = 1 << 5;
    /// An alias not known to be rigid: normalizing may change it.
    pub const NEEDS_NORMALIZE: u16 = 1 << 6;
    /// An opaque type.
    pub const OPAQUE: u16 = 1 << 7;
    /// The error type, region or const.
    pub const ERROR: u16 = 1 << 8;
    /// A region that is not bound (erased and error regions too: canonicalizing an input
    /// replaces them, as rustc's `HAS_FREE_REGIONS | HAS_RE_ERASED` says).
    pub const FREE_REGIONS: u16 = 1 << 9;
    /// A canonical variable (`kind::CANONICAL`).
    pub const CANONICAL: u16 = 1 << 10;
    /// A coroutine or its witness.
    pub const CORO: u16 = 1 << 11;

    /// Any inference variable.
    pub const INFER: u16 = Flags::TY_INFER | Flags::RE_INFER | Flags::CT_INFER;
    /// What a node with none of is the same in every inference context and every instance.
    pub const NOT_GROUND: u16 = Flags::INFER | Flags::PARAM | Flags::PLACEHOLDER | Flags::CANONICAL;

    const BOOLS: u16 = 0x0fff;
    const OUTER_SHIFT: u32 = 12;
    /// The saturated outer binder: "any".
    pub const OUTER_ANY: u32 = 15;

    pub const fn bits(self) -> u16 {
        self.0
    }

    /// Whether any of `bits` is set.
    #[inline(always)]
    pub const fn has(self, bits: u16) -> bool {
        self.0 & bits != 0
    }

    /// The outer exclusive binder (saturated at [`Flags::OUTER_ANY`]).
    #[inline(always)]
    pub const fn outer_binder(self) -> u32 {
        Cast::<u16, u32>::widen(self.0 >> Flags::OUTER_SHIFT)
    }

    /// Whether a bound variable inside may escape `depth` binders above the node.
    #[inline(always)]
    pub const fn escapes(self, depth: u32) -> bool {
        let o = self.outer_binder();
        o == Flags::OUTER_ANY || o > depth
    }

    /// Whether the node is the same in every context: no variable, parameter, placeholder or
    /// escaping bound variable.
    #[inline(always)]
    pub const fn is_ground(self) -> bool {
        !self.has(Flags::NOT_GROUND) && self.outer_binder() == 0
    }

    /// Flags with only `bits` and an outer binder of 0.
    pub(crate) const fn of(bits: u16) -> Flags {
        Flags(bits & Flags::BOOLS)
    }

    /// Flags of a bound variable `debruijn` binders up.
    pub(crate) fn bound(bits: u16, debruijn: u32) -> Flags {
        let outer = debruijn.saturating_add(1).min(Flags::OUTER_ANY);
        Flags::of(bits).with_outer(outer)
    }

    fn with_outer(self, outer: u32) -> Flags {
        // `outer` is at most 15: it fits the top four bits.
        let top = Cast::<u32, u16>::truncate(outer.min(Flags::OUTER_ANY)) << Flags::OUTER_SHIFT;
        Flags((self.0 & Flags::BOOLS) | top)
    }

    /// `self` with a child's flags added; `binder`: the child is under one more binder than
    /// `self`'s node, so its outer binder counts one less here.
    #[inline]
    pub(crate) fn with_child(self, child: Flags, binder: bool) -> Flags {
        let bools = (self.0 | child.0) & Flags::BOOLS;
        let c = child.outer_binder();
        let c = if c == Flags::OUTER_ANY {
            Flags::OUTER_ANY
        } else if binder {
            c.saturating_sub(1)
        } else {
            c
        };
        Flags(bools).with_outer(self.outer_binder().max(c))
    }
}

/// The solver's cache of cheap marker-trait answers about a type: a known-yes and a known-no
/// bit per trait. blaze-ty only stores them (`Tys::markers`, `Tys::set_markers`); they are
/// meaningful only for ground types and never rolled back.
pub mod markers {
    pub const SIZED_YES: u32 = 1 << 0;
    pub const SIZED_NO: u32 = 1 << 1;
    pub const COPY_YES: u32 = 1 << 2;
    pub const COPY_NO: u32 = 1 << 3;
    pub const SEND_YES: u32 = 1 << 4;
    pub const SEND_NO: u32 = 1 << 5;
    pub const SYNC_YES: u32 = 1 << 6;
    pub const SYNC_NO: u32 = 1 << 7;
    pub const UNPIN_YES: u32 = 1 << 8;
    pub const UNPIN_NO: u32 = 1 << 9;
    pub const FREEZE_YES: u32 = 1 << 10;
    pub const FREEZE_NO: u32 = 1 << 11;
}
