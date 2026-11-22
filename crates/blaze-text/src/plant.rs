//! The bugs a control can plant in the writers (rule 53; D165, "a control is typed"). The
//! integer path takes one as a type ([`Planted<TextPlant>`](blaze_own::planted::Planted)), so
//! the shipped [`NoPlant`](blaze_own::planted::NoPlant) instantiation is the table read alone;
//! the debug walk crosses vtables and takes it as a value.

#![blaze::sans_io]

use blaze_own::planted::Plants;

/// A bug a control plants in blaze-text.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TextPlant {
    /// The writers as they are.
    None,
    /// The two-digit table is read one pair off: `r` writes the digits of `r + 1`.
    DigitPairOffByOne,
    /// The debug walk writes every named field as `field`.
    DebugFieldNameLost,
}

impl Plants for TextPlant {}
