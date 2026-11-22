//! The bugs a control plants (D165, rule 53): each is a mistake of a class the crate's oracles
//! exist to catch, behind one [`Planted`](blaze_own::planted::Planted) seam. Shipped code passes
//! `NoPlant`, which compiles every check away.

use blaze_own::planted::Plants;

/// One planted bug, or none.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CadPlant {
    /// The code as it is.
    None,
    /// An interval subtraction pairs its ends inward (`lo - lo`, `hi - hi`): not outward.
    IntervalNotOutward,
    /// `simplify` reads a minimum decided for its left operand as decided for its right.
    SimplifyWrongSide,
    /// `simplify` keeps both operands of every minimum, decided or not.
    SimplifyKeepsBothSides,
    /// An ellipsoid is declared exact (its field is a bound).
    EllipsoidExact,
    /// A primitive's digest leaves out its last parameter.
    DigestDropsParameter,
    /// A rounded union adds its blend term instead of taking it away.
    SmoothUnionWrongSign,
    /// The vector interpreter divides by multiplying with the reciprocal.
    VectorDivReciprocal,
    /// Lowering a placement drops its shift.
    PlacementShiftDropped,
    /// `split_placement` leaves a primitive's frame in its geometry.
    FrameKeptInGeometry,
    /// A commutative boolean keeps its operands in the order given.
    OperandsUnordered,
    /// Interning never finds an existing node.
    NoHashConsing,
    /// The gradient of `abs` ignores the sign of its operand.
    GradAbsSignIgnored,
    /// A tape's digest leaves out its constant pool.
    TapeDigestSkipsConsts,
    /// Encoding writes each constant as an f32.
    EncodeConstsAsF32,
}

impl Plants for CadPlant {}
