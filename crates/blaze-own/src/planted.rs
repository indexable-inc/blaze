//! The plant seam (D165, rule 78: a strategy is a type parameter at one seam): how shipped code
//! takes a planted bug as a PARAMETER, so a control can plant it and the shipped build compiles
//! every plant check to `false`. Never a thread-local switch, never `#[cfg(test)]` in the body.
//!
//! It lives here, in the base crate every crate depends on (through blaze-mem and blaze-core,
//! which re-exports it as `blaze_core::planted`), because production crates cannot depend on
//! blaze-sim, a dev-dependency; blaze-sim re-exports it too, and its `Plant` trait (the test side:
//! the oracle's error type, `ALL`, the invariant each plant must trip) extends [`Plants`].
//!
//! The pattern:
//!
//! - The component's crate declares its plants as one enum with a `None` variant and implements
//!   [`Plants`] for it (in a test, blaze-sim's `plants!` writes that impl and blaze-sim's `Plant`).
//! - Each function that holds a plant check is generic over `S: Planted<TheEnum>` and asks
//!   `plant.has(TheEnum::Variant)` where the bug would be.
//! - Shipped code calls it with [`NoPlant`]; a control (`blaze_sim::controls`) calls it with the
//!   enum's values.
//!
//! ```
//! use blaze_own::planted::{NoPlant, Planted, Plants};
//!
//! /// The bugs a control can plant in `take_newest`.
//! #[derive(Clone, Copy, PartialEq, Eq, Debug)]
//! pub enum TakePlant {
//!     /// The code as it is.
//!     None,
//!     /// Takes one past the end.
//!     OffByOne,
//! }
//! impl Plants for TakePlant {}
//!
//! /// The newest `n` of `items` (at most `items.len()`), the plant taken through the seam.
//! pub fn take_newest<'a>(items: &'a [u32], n: usize, plant: impl Planted<TakePlant>) -> &'a [u32] {
//!     let n = if plant.has(TakePlant::OffByOne) { n + 1 } else { n };
//!     &items[items.len().saturating_sub(n)..]
//! }
//!
//! // Shipped code: no plant, and `has` is `false`, so the branch compiles away.
//! assert_eq!(take_newest(&[1, 2, 3], 2, NoPlant), &[2, 3]);
//! // A control plants the bug and its oracle must catch it.
//! assert_eq!(take_newest(&[1, 2, 3], 2, TakePlant::OffByOne), &[1, 2, 3]);
//! assert_eq!(take_newest(&[1, 2, 3], 2, TakePlant::None), &[2, 3]);
//! ```

/// An enum of a component's planted bugs: its values are the plants a control passes through a
/// [`Planted`] seam, `None` among them for the code as it is. A marker: blaze-sim's `Plant` adds
/// what a control needs to judge each one.
pub trait Plants: Copy + Eq + 'static {}

/// The seam a component's code takes its plant through: `has(plant)` is whether the bug `plant`
/// is planted. Implemented for every [`Plants`] enum (a value plants itself) and for [`NoPlant`].
pub trait Planted<P: Copy + Eq>: Copy {
    /// Whether the bug `plant` is planted.
    fn has(self, plant: P) -> bool;
}

impl<P: Plants> Planted<P> for P {
    #[inline]
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    fn has(self, plant: P) -> bool {
        self == plant
    }
}

/// No plant: the shipped instantiation of a [`Planted`] seam. Every `has` is `false`. It is not
/// a [`Plants`] enum (and no other crate may make it one), so the two impls never overlap.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct NoPlant;

impl<P: Copy + Eq> Planted<P> for NoPlant {
    #[inline]
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    fn has(self, _plant: P) -> bool {
        false
    }
}
