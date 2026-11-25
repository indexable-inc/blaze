//! Trait-lifetime tie metadata ([`Tie`], [`Ties`], [`TieMiss`]).
//!
//! Call-site trait-lifetime tying now runs exclusively through D156's unified `one_call.rs` /
//! `blaze-borrow` engine. These public types remain for `Facts::ties` (cross-checked in
//! `one_call.rs` via `OneRuleWhy::TieUnlaid`) and `Lower::own_alias_known`.

use blaze_typeck::ItemKey;
use blaze_typeck::regions::Region;

/// One trait bound with lifetime arguments of a fn: the bounded generic parameter `param` (a
/// trait method's `Self` is 0), the trait, and its lifetime arguments `Ties::regions[lo..hi]`
/// in the trait's parameter order, as regions of the fn's own numbering (D148: its clause's
/// column's own slots, `RegionFacts::clauses`). `read` false: the bound is known (the head's
/// clauses hold it) but its column is not (the head's regions were not read, or the column
/// holds another count than the trait declares).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Tie {
    pub param: u32,
    pub trait_: ItemKey,
    pub lo: u32,
    pub hi: u32,
    pub read: bool,
}

/// A fn's [`Tie`] rows, borrowed from whoever holds them ([`Facts::ties`]).
#[derive(Clone, Copy, Debug)]
pub struct Ties<'s> {
    pub rows: &'s [Tie],
    pub regions: &'s [Region],
}

/// Why a callee's trait lifetime could not be tied to the caller's (legacy refusal payload).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum TieMiss {
    /// The callee's own row was not read (a bound whose arguments the reader did not parse).
    CalleeUnread,
    /// The call's argument for the bounded parameter is neither a caller type parameter nor
    /// a concrete type whose impl the record gives (e.g. a projection).
    Concrete,
    /// The call's argument for the bounded parameter is concrete, and the record holds no
    /// unique impl of the bound's trait for it (`Checked::selected`).
    NoImpl,
    /// The selected impl's header was not read (an unsupported self type, trait reference, or
    /// where clause).
    ImplUnread,
    /// The selected impl's trait reference has a different lifetime count than the callee's tie.
    ImplCount,
    /// The selected impl's self type does not line up with the call's argument's positions.
    ImplUnaligned,
    /// The selected impl has a where clause not handled by the legacy tie path.
    ImplWhere,
    /// The body's own rows are not known (`Facts::ties` of the body gave `None`).
    OwnUnknown,
    /// The body writes no bound for that parameter and trait in its own text.
    NoOwnBound,
    /// The body's own row is not read, or two of its rows name that parameter and trait.
    OwnUnread,
    /// The two rows have different numbers of lifetime arguments.
    Count,
}
