//! THE IMPLICIT CAD KERNEL'S BOTTOM (D183): a solid is a field, negative inside, built as a
//! hash-consed graph of canonical nodes, compiled to a flat tape, and evaluated by interpreters
//! that agree with each other bit for bit.
//!
//! - [`Graph`]: nodes built from named options (`#[blaze::args]`), exactness in their types
//!   ([`Solid<Exact>`](Solid), [`Solid<Bound>`](Solid), see `level`), placement kept apart from
//!   geometry, parameters quantized to integers before they are keyed (nanometres,
//!   micro-arcseconds, counts), every node keyed by a BLAKE3 digest of its kind, its integers and
//!   its operands' digests. Changing one parameter makes new digests only along its path.
//!
//! The crate's README is the contract: the node table, the tape format, what each interpreter
//! promises, and the signatures the next crates (the mesher, the record keys, the picture, the
//! checks) are written against.

#![no_std]
#![feature(register_tool, custom_inner_attributes, portable_simd, core_float_math, default_field_values)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![deny(blaze::sans_io_core)]
#![deny(blaze::std_use)]
#![deny(blaze::implicit_allocator)]
#![deny(blaze::process_static)]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

mod error;
mod eval;
mod graph;
mod inspect;
mod level;
mod lower;
mod node;
mod placement;
mod plant;
mod profile;
mod tape;

#[cfg(test)]
mod tests;

pub use error::{CadError, CompileError, DecodeError, EvalError, InspectError, Name, NodeKind, Value};
pub use eval::{
    Choice, Choices, Lanes, Points, Scratch, eval_box, eval_box_with, eval_grad, eval_grad_with, eval_grads, eval_grads_with, eval_point,
    eval_points, eval_points_with, simplify, simplify_with,
};
pub use inspect::{
    Bounds, BoundsOpts, Class, DistanceOpts, DistanceStopOpts, Gap, Hit, RayOpts, bounds, classify, distance_between,
    distance_between_stopping, ray, section,
};
pub use lower::{CompileOpts, compile};
pub use tape::{INSTR_BYTES, Instr, Op, Tape, schema_digest};
pub use graph::{
    AffineOpts, BendOpts, BlendOpts, BoxOpts, CapsuleOpts, CircularOpts, ConeOpts, CylinderOpts, DetailOpts, EdgeSelect, EllipsoidOpts,
    ExtrudeOpts, Graph, GraphOpts, GridOpts, GuideRail, GuidedLoftOpts, GuidedStation, GyroidOpts, HalfSpaceOpts, LoftOpts, MirrorOpts,
    OffsetOpts, Prof, ProfileOpts, RadiusProfile, RadiusStop, RailPoint, RepeatOpts, RevolveOpts, RevolvePartOpts, RoundOpts, ScaleOpts,
    ShellOpts, Solid, SphereOpts, Station, SuperellipsoidOpts, SweepArcOpts, SweepCrossSection, SweepFrame, SweepFramesOpts,
    SweepPolylineOpts, TaperOpts, TorusOpts, TwistOpts, VariableBlendOpts, VariableOffsetOpts,
};
pub use level::{AtLeastInside, AtLeastOutside, Bound, Ex, Exact, Inside, Join, Level, Lipschitz, Outside, Weakens};
pub use node::{Boolean, Detail, Digest, Edge, LoftContinuity, Node, NodeId, ProfileId, Shape};
pub use placement::{Axis, LINEAR_GRID, Placement};
pub use plant::CadPlant;
pub use profile::{Cubic, Point2, Profile, Segment};

/// The bounds every table and walk of the crate keeps (rule 18): a graph, a compile, a decode
/// and an inspection stop with a typed error at these, never grow past them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Limits {
    /// Nodes in one graph.
    pub most_nodes: u32,
    /// Rows (segments, flattened points) in one profile.
    pub most_profile_points: u32,
    /// Instructions in one tape.
    pub most_instrs: u32,
    /// Values live at once in one tape: a register index is a `u16`.
    pub most_registers: u32,
    /// Copies in one repeat, points in one swept polyline.
    pub most_copies: u32,
    /// Stations in one loft.
    pub most_stations: u32,
    /// Values in one attached distance grid.
    pub most_grid_values: u32,
    /// Boxes one inspection (bounds, classify, distance) may look at.
    pub most_boxes: u32,
    /// Steps one ray may take.
    pub most_steps: u32,
}

impl Limits {
    /// The shipped bounds.
    pub const SHIPPED: Limits = Limits {
        most_nodes: 1 << 24,
        most_profile_points: 1 << 16,
        most_instrs: 1 << 26,
        most_registers: 65_535,
        most_copies: 1 << 16,
        most_stations: 1 << 12,
        most_grid_values: 1 << 27,
        most_boxes: 1 << 22,
        most_steps: 1 << 16,
    };
}
