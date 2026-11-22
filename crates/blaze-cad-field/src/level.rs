//! EXACTNESS IN TYPES (D183, rule 76). A field `f` of a solid `S` is one of four levels:
//!
//! - [`Exact`]: `f(p)` is the signed distance from `p` to the boundary of `S`, negative inside.
//! - [`Outside`]: the sign is right everywhere, `|f| <= distance` everywhere, and `f` IS the
//!   distance wherever it is positive (outside `S`). A union of exact fields is this.
//! - [`Inside`]: the mirror of [`Outside`]: exact wherever negative. An intersection of exact
//!   fields is this.
//! - [`Bound`]: the sign is right and `|f| <= distance` everywhere: what sphere tracing, a
//!   cell's emptiness and the mesher's step need, and all a smooth blend or a non-rigid map
//!   keeps.
//!
//! The lattice: `Exact` is above `Outside` and `Inside`, both above `Bound`. The rules each
//! operation follows, with the reason:
//!
//! | operation | needs | gives | why |
//! |---|---|---|---|
//! | union (min) | - | both at least `Outside`: `Outside`, else `Bound` | outside the union the nearest boundary point is the nearer of the two |
//! | intersection (max) | - | both at least `Inside`: `Inside`, else `Bound` | the mirror |
//! | complement (`-f`) | - | swaps `Outside` and `Inside` | the sign flips |
//! | difference `a - b` | - | intersection of `a` with the complement of `b` | |
//! | grow by `r` (`f - r`) | at least `Outside` | `Outside` | outside, the offset surface is at `d - r`; inside, only a bound |
//! | erode by `r` (`f + r`) | at least `Inside` | `Inside` | the mirror |
//! | smooth or chamfered boolean | - | `Bound` | 1-Lipschitz, below the min or above the max |
//! | placement, uniform scale (`s f(p/s)`) | - | the same | distances are kept or scaled |
//! | mirrored copy (a fold) | the operand on one side (checked) | the same | reflecting a point of the far copy brings it nearer |
//! | linear repeat | the copies apart (checked) | `Inside` from `Exact` or `Inside`, else `Bound` | inside, only the copy folded to holds the point; outside, the nearest copy may be a third one, so the field takes the two nearest and a stated lower bound for the rest |
//! | circular repeat | - | a union of placed copies | |
//! | shell | `Exact` | `Outside` | in the wall, the inner surface's distance may be larger than `d + t` |
//! | affine, twist, bend, taper, loft, partial revolve, detail | - | `Bound` | divided by a stated Lipschitz factor |
//! | redistance | - | `Exact` | the mesher solves it (D183) |
//!
//! The marker types carry the table as associated types, so `union` of two `Solid<Exact>` is a
//! `Solid<Outside>` at compile time; every node also records its [`Level`] so a tape and a
//! record say what they hold.

/// The level of a field, as a value (what a node and a tape record).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Level {
    /// Correct sign, `|f| <= distance`.
    Bound = 0,
    /// Also exact where positive.
    Outside = 1,
    /// Also exact where negative.
    Inside = 2,
    /// The signed distance.
    Exact = 3,
}

impl Level {
    /// Whether this level gives at least what `other` promises.
    pub const fn at_least(self, other: Level) -> bool {
        match other {
            Level::Bound => true,
            Level::Outside => matches!(self, Level::Outside | Level::Exact),
            Level::Inside => matches!(self, Level::Inside | Level::Exact),
            Level::Exact => matches!(self, Level::Exact),
        }
    }

    /// The level of a union of fields of these levels.
    pub const fn union(self, o: Level) -> Level {
        if self.at_least(Level::Outside) && o.at_least(Level::Outside) { Level::Outside } else { Level::Bound }
    }

    /// The level of an intersection.
    pub const fn intersect(self, o: Level) -> Level {
        if self.at_least(Level::Inside) && o.at_least(Level::Inside) { Level::Inside } else { Level::Bound }
    }

    /// The level of the complement.
    pub const fn complement(self) -> Level {
        match self {
            Level::Outside => Level::Inside,
            Level::Inside => Level::Outside,
            l => l,
        }
    }

    /// The short name printed in an error.
    pub const fn name(self) -> &'static str {
        match self {
            Level::Bound => "a bound",
            Level::Outside => "exact outside",
            Level::Inside => "exact inside",
            Level::Exact => "exact",
        }
    }
}

mod sealed {
    pub trait Sealed {}
}

/// A level as a type: [`Exact`], [`Outside`], [`Inside`] or [`Bound`].
pub trait Ex: sealed::Sealed + Copy + Eq + core::fmt::Debug + 'static {
    /// The level as a value.
    const LEVEL: Level;
    /// The level of a complement.
    type Complement: Ex;
    /// The level of a linear repeat.
    type Repeated: Ex;
}

/// The level of a union of `Self` with `B`, and of an intersection.
pub trait Join<B: Ex>: Ex {
    /// The union's level.
    type Union: Ex;
    /// The intersection's level.
    type Intersect: Ex;
}

/// `Self` can be used where `K` is asked for (`K` is at or below `Self` in the lattice).
pub trait Weakens<K: Ex>: Ex {}

/// Exact everywhere.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Exact;
/// Exact where positive.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Outside;
/// Exact where negative.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Inside;
/// A bound.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Bound;

macro_rules! levels {
    ($($t:ident => $c:ident, $r:ident;)*) => {$(
        impl sealed::Sealed for $t {}
        impl Ex for $t {
            const LEVEL: Level = Level::$t;
            type Complement = $c;
            type Repeated = $r;
        }
    )*};
}

levels! {
    Exact => Exact, Inside;
    Outside => Inside, Bound;
    Inside => Outside, Inside;
    Bound => Bound, Bound;
}

impl Level {
    /// The level of a linear repeat of a field of this level.
    pub const fn repeated(self) -> Level {
        if self.at_least(Level::Inside) { Level::Inside } else { Level::Bound }
    }

    /// Whether every field of this level the graph makes is 1-Lipschitz (a detail relief needs
    /// it): exact fields and the mins and maxes of them.
    pub const fn lipschitz(self) -> bool {
        !matches!(self, Level::Bound)
    }
}

/// A level whose fields are 1-Lipschitz: [`Exact`], [`Outside`], [`Inside`].
pub trait Lipschitz: Ex {}
impl Lipschitz for Exact {}
impl Lipschitz for Outside {}
impl Lipschitz for Inside {}

macro_rules! joins {
    ($($a:ident, $b:ident => $u:ident, $i:ident;)*) => {$(
        impl Join<$b> for $a {
            type Union = $u;
            type Intersect = $i;
        }
    )*};
}

joins! {
    Exact, Exact => Outside, Inside;
    Exact, Outside => Outside, Bound;
    Exact, Inside => Bound, Inside;
    Exact, Bound => Bound, Bound;
    Outside, Exact => Outside, Bound;
    Outside, Outside => Outside, Bound;
    Outside, Inside => Bound, Bound;
    Outside, Bound => Bound, Bound;
    Inside, Exact => Bound, Inside;
    Inside, Outside => Bound, Bound;
    Inside, Inside => Bound, Inside;
    Inside, Bound => Bound, Bound;
    Bound, Exact => Bound, Bound;
    Bound, Outside => Bound, Bound;
    Bound, Inside => Bound, Bound;
    Bound, Bound => Bound, Bound;
}

macro_rules! weakens {
    ($($a:ident => $($k:ident),*;)*) => {$($(
        impl Weakens<$k> for $a {}
    )*)*};
}

weakens! {
    Exact => Exact, Outside, Inside, Bound;
    Outside => Outside, Bound;
    Inside => Inside, Bound;
    Bound => Bound;
}

/// A level at least [`Outside`]: what growing needs.
pub trait AtLeastOutside: Ex {}
impl AtLeastOutside for Exact {}
impl AtLeastOutside for Outside {}

/// A level at least [`Inside`]: what eroding needs.
pub trait AtLeastInside: Ex {}
impl AtLeastInside for Exact {}
impl AtLeastInside for Inside {}
