//! Drawn limits and typed reach points (D165, "a limit is a value a simulation draws").
//!
//! The class this closes: a cap written as a `pub const` that the sans-IO core reads directly,
//! so no tape can lower it and no case reaches the far side of it. Eighty-nine store simulations
//! passed that way, and the first real run on a large project committed a manifest its own reader
//! refused.
//!
//! # The pattern
//!
//! A component's caps are the fields of ONE struct, its limits, with the shipped values as one
//! constant instance and no `cfg(test)` variant. The core takes the struct as a value (from its
//! constructor's options), never a constant. A simulation draws the struct with
//! [`Tape::limits`], each field by [`Tape::limit`], which is biased hard to the smallest legal
//! value and the boundaries, and gives the shipped value now and then, so most cases run at caps
//! of 1 to 3 and the shipped value is still one of the draws:
//!
//! ```ignore
//! // In the component (production code):
//! #[derive(Clone, Copy, PartialEq, Eq, Debug)]
//! pub struct LogLimits { pub most_records: usize }
//! impl LogLimits { pub const SHIPPED: LogLimits = LogLimits { most_records: 4096 }; }
//!
//! // In its tests:
//! impl blaze_sim::Limits for LogLimits {
//!     const SHIPPED: LogLimits = LogLimits::SHIPPED;
//!     fn draw(tape: &Tape) -> LogLimits {
//!         LogLimits { most_records: tape.limit(LogLimits::SHIPPED.most_records, 1, "most records") }
//!     }
//! }
//! ```
//!
//! A reach point at each limit says the run got there: an enum per component declared with
//! [`points!`](crate::points), noted with [`Tape::reach`] where the case sees the cap hit (the
//! core answered "full"), and required with [`Config::must_reach_points`], so a run in which no
//! passing case hit the cap fails by the point's name (`Verdict::Unreached`) instead of passing
//! without having tested it. The worked example is `src/tests/limits.rs`.
//!
//! [`Tape::limits`]: crate::tape::Tape::limits
//! [`Tape::limit`]: crate::tape::Tape::limit
//! [`Tape::reach`]: crate::tape::Tape::reach
//! [`Config::must_reach_points`]: crate::search::Config::must_reach_points

#![blaze::sans_io]

use core::fmt::Debug;

use crate::tape::Tape;

/// A component's caps as one value: the fields are the caps, [`Limits::SHIPPED`] the values
/// shipped, and [`Limits::draw`] the simulation's draw of each field by [`Tape::limit`]. The
/// impl lives in the component's tests, beside its simulation; the component itself names its
/// own `SHIPPED` constant and takes the struct as a value.
pub trait Limits: Copy + Debug {
    /// The values the component ships with: one draw among the others.
    const SHIPPED: Self;
    /// Every field drawn with [`Tape::limit`] between its smallest legal value and its shipped one.
    fn draw(tape: &Tape) -> Self;
}

/// A reach point's identity: the path of its variant (`module::Enum::Variant`), made by
/// [`points!`](crate::points) and by nothing else, so a point is never a string a person typed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PointId {
    path: &'static str,
}

impl PointId {
    /// For [`points!`](crate::points) alone: the id of the variant at `path`, which the macro
    /// writes from `module_path!()` and the variant's tokens.
    #[doc(hidden)]
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub const fn from_points_macro(path: &'static str) -> PointId {
        PointId { path }
    }

    /// The variant's path: what a report prints of a point no case reached.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub const fn path(self) -> &'static str {
        self.path
    }
}

/// The reach points of one component: an enum declared with [`points!`](crate::points), which
/// writes this impl. `ALL` is every variant and `IDS` their ids, in declaration order.
pub trait Point: Copy + Eq + Debug + 'static {
    const ALL: &'static [Self];
    /// The ids of `ALL`: what [`Config::must_reach_points`] takes to require every point.
    ///
    /// [`Config::must_reach_points`]: crate::search::Config::must_reach_points
    const IDS: &'static [PointId];
    /// This point's id.
    fn id(self) -> PointId;
}

/// Declares a component's reach points as an enum and implements [`Point`] for it, with an
/// inherent `const fn id(self) -> PointId` for constant contexts:
///
/// ```ignore
/// blaze_sim::points! {
///     /// Where the log's simulation has to get.
///     pub(crate) enum LogPoint {
///         /// A push was refused: the log was at its drawn cap.
///         Full,
///         /// The case ran with the shipped limits.
///         AtShippedCap,
///     }
/// }
/// const REACH: &[blaze_sim::PointId] = &[LogPoint::Full.id()];
/// // or every point: Config::cases(256).must_reach_points(<LogPoint as Point>::IDS)
/// ```
///
/// The enum derives `Clone, Copy, PartialEq, Eq, Debug`; write no derive of those.
#[macro_export]
macro_rules! points {
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident {
            $( $(#[$vmeta:meta])* $variant:ident ),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, Debug)]
        $vis enum $name {
            $( $(#[$vmeta])* $variant, )+
        }

        impl $name {
            /// This point's id: its variant's path. (Used by the `Point` impl below, so never dead.)
            $vis const fn id(self) -> $crate::PointId {
                match self {
                    $( $name::$variant => $crate::PointId::from_points_macro(concat!(
                        module_path!(), "::", stringify!($name), "::", stringify!($variant)
                    )), )+
                }
            }
        }

        impl $crate::Point for $name {
            const ALL: &'static [$name] = &[$($name::$variant),+];
            const IDS: &'static [$crate::PointId] = &[$($name::$variant.id()),+];

            fn id(self) -> $crate::PointId {
                $name::id(self)
            }
        }
    };
}

/// The kinds of `Tape::limit`'s choice: sixteen, so the choice is one `below(16)`.
pub(crate) const LIMIT_KINDS: u64 = 16;

/// The value of a limit of kind `kind` (below [`LIMIT_KINDS`]) between `smallest` and `shipped`
/// on the ordered line (`smallest < shipped`), or `None` for the kind that draws uniformly (a
/// second choice, `Tape::limit`'s). Seven kinds in sixteen are the smallest legal value (the calm
/// one among them), four the next value up, two the one above that, one uniform, two the shipped
/// value: the boundaries first, the shipped value now and then.
#[blaze::pure]
#[blaze::nopanic]
#[blaze::noalloc]
#[blaze::nonblocking]
pub(crate) fn limit_of_kind(kind: u64, smallest: u128, shipped: u128) -> Option<u128> {
    match kind {
        0..=6 => Some(smallest),
        7..=10 => Some((smallest + 1).min(shipped)),
        11..=12 => Some(smallest.saturating_add(2).min(shipped)),
        13 => None,
        _ => Some(shipped),
    }
}
