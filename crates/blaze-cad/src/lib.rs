//! BLAZE-CAD (D183): the CAD library a program imports to build ships, one `use` away from every
//! piece: solids from blaze-cad-field, meshes from blaze-cad-mesh, pictures from
//! blaze-cad-picture, files from blaze-cad-out, all in blaze-units' quantities.
//!
//! # A model, in the shape an author (or a language model) writes it
//!
//! ```ignore
//! use blaze_cad::prelude::*;
//!
//! let mut model = Model::new(ModelOpts { alloc, .. });
//! let g = model.graph();
//! let plate = g.cuboid(BoxOpts { size: [Length::mm(600.0), Length::mm(400.0), Length::mm(12.0)], name: Some("deck"), .. })?;
//! let hole = g.cuboid(BoxOpts { size: [Length::mm(300.0), Length::mm(200.0), Length::mm(60.0)], round: Length::mm(25.0), .. })?;
//! let deck = g.difference(plate, hole)?;
//! let (_, deck) = model.add(PartOpts { solid: deck, name: "deck" })?;
//! let wall = min_wall(model.graph_ref(), &deck, MinWallOpts { wall: Length::mm(6.0), scratch, .. })?;
//! println!("deck, 6 mm: {}", Shown(&wall.verdict)); // "pass", or "thin wall of about 4.2 mm at (...) mm"
//! assert!(wall.verdict.passed());
//! ```
//!
//! Every argument is named (`T::new(TOpts { .. })`, rule 83), every length is a [`Length`]
//! (never a bare number of unknown unit), every choice is a field with no default and every
//! tunable has one, and every error names the function and the argument it refuses
//! ([`ModelError::Argument`]) or carries the kernel's own, which names the node.
//!
//! # The pieces
//!
//! | piece | what | where |
//! |---|---|---|
//! | [`Part`] | a named solid and its placement, kept apart so moving a part re-keys nothing of it | `part` |
//! | [`assemble`] | the union of placed parts, a balanced tree | `part` |
//! | [`round_edges`] | erode, solved redistance, grow: rounded edges that compile at once | `part` |
//! | [`mesh_part`] | a part's mesh in its own frame, pieces from a [`PieceCache`] | `part` |
//! | [`mass_properties`] | volume, mass, centre and inertia of a verified mesh at a density | `part` |
//! | [`min_wall`] [`no_interference`] [`fits_within`] | the design checks, three-way verdicts with keys | `check` |
//! | [`export_glb`] [`picture`] | a glTF binary of the parts, a picture of a solid | `export` |
//! | [`Model`] | the graph and its named parts together | here |
//!
//! # Records and checkpoints
//!
//! What is kept is said where it is made: mesh pieces under blaze-cad-mesh's `RegionKey` (in a
//! session's [`PieceCache`], written out at a checkpoint: a check, an export, a commit), check
//! verdicts under [`CheckKey`]. A cheap operation (adding a part, moving one, compiling a tape)
//! writes nothing: tapes and graphs are recomputed from the text in microseconds.

#![no_std]
#![feature(allocator_api, register_tool, custom_inner_attributes, core_float_math, default_field_values)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![deny(blaze::sans_io_core)]
#![deny(blaze::std_use)]
#![deny(blaze::implicit_allocator)]
#![deny(blaze::process_static)]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

mod check;
mod export;
mod part;

#[cfg(test)]
mod math_tests;
#[cfg(test)]
mod tests;

pub use blaze_cad_mesh::{PieceCache, PieceCacheOpts};
pub use check::{
    CHECK_SCHEMA, CheckKey, CheckKind, Checked, FitOpts, Interference, InterferenceOpts, MinWallOpts, Overhang, ThinWall, Unsettled,
    Verdict, fits_within, fits_within_planted, min_wall, min_wall_planted, no_interference, no_interference_planted,
};
/// A finding, a verdict or any other [`blaze_text::Show`] as a `Display`:
/// `println!("{}", Shown(&checked.verdict))` prints "thin wall of about 4.2 mm at (295.898,
/// 290.039, -0.586) mm".
pub use blaze_text::Shown;
pub use export::{ExportOpts, Exported, PartCount, STEEL, STEEL_ONLY, export_glb, picture, transform_of};
pub use part::{
    MassProperties, MeshPartOpts, Part, PartOpts, RoundEdgesOpts, assemble, mass_properties, mass_properties_planted, mesh_part,
    part_tape, placed, placed_tape, round_edges, round_edges_planted,
};

use blaze_cad_field::{Bound, CadError, CompileError, Ex, Graph, InspectError, Name, Placement, Solid, Weakens};
use blaze_cad_mesh::{DecimateError, MeshError};
use blaze_cad_out::{GlbError, PngError};
use blaze_cad_picture::PictureError;
use blaze_own::planted::{NoPlant, Plants};
use blaze_own::{Alloc, Vec};
use blaze_units::Length;

/// What the library refuses, each naming the thing.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum ModelError {
    /// An argument outside what `function` accepts.
    Argument { function: &'static str, arg: &'static str, accepted: &'static str },
    /// A solid with no points where one is needed.
    EmptySolid { function: &'static str, arg: &'static str },
    /// A part's name already taken in the model.
    DuplicateName { name: Name },
    /// A part id the model never gave.
    NoPart { id: u32 },
    /// More parts than the model's cap.
    TooManyParts { most: u32 },
    /// An allocation for `what` refused.
    Alloc { what: &'static str },
    /// The kernel refused a node (it names the node and the argument).
    Cad(CadError),
    Compile(CompileError),
    Inspect(InspectError),
    Mesh(MeshError),
    Decimate(DecimateError),
    Picture(PictureError),
    Glb(GlbError),
    Png(PngError),
}

// `?` on a layer's error gives the model's: one error type for an author to match.
impl From<CadError> for ModelError {
    fn from(e: CadError) -> ModelError {
        ModelError::Cad(e)
    }
}

impl From<CompileError> for ModelError {
    fn from(e: CompileError) -> ModelError {
        ModelError::Compile(e)
    }
}

impl From<InspectError> for ModelError {
    fn from(e: InspectError) -> ModelError {
        ModelError::Inspect(e)
    }
}

impl From<MeshError> for ModelError {
    fn from(e: MeshError) -> ModelError {
        ModelError::Mesh(e)
    }
}

impl From<PictureError> for ModelError {
    fn from(e: PictureError) -> ModelError {
        ModelError::Picture(e)
    }
}

impl From<DecimateError> for ModelError {
    fn from(e: DecimateError) -> ModelError {
        ModelError::Decimate(e)
    }
}

impl From<GlbError> for ModelError {
    fn from(e: GlbError) -> ModelError {
        ModelError::Glb(e)
    }
}

impl From<PngError> for ModelError {
    fn from(e: PngError) -> ModelError {
        ModelError::Png(e)
    }
}

/// [`Part::new`]'s planted bug.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PartPlant {
    None,
    /// The placement left in the solid: a moved part gets a new tape digest, and every piece
    /// and check record of it is recomputed.
    PlacementKept,
}

impl Plants for PartPlant {}

/// [`round_edges`]'s planted bug.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RoundPlant {
    None,
    /// Grown back by half the radius: every face moves in.
    GrowShort,
}

impl Plants for RoundPlant {}

/// [`mass_properties`]'s planted bug.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MassPlant {
    None,
    /// The inertia left per unit density.
    InertiaUnscaled,
}

impl Plants for MassPlant {}

/// [`min_wall`]'s planted bug.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WallPlant {
    None,
    /// Thin read as "a ball of diameter `wall` tangent at the point does not fit": every sharp
    /// convex edge of a thick part is reported (the false finding the inward chord removes).
    TangentBall,
    /// The bracketing removed: a sample is found by Newton's steps alone until its budget is
    /// spent, with no sign bracket and no split. On a field that is only a bound outside the
    /// solid the steps can go back and forth for ever (a ring cut by a taller cylinder, in the
    /// hole between the two caps' planes), and the place is left unsettled.
    NoBracket,
}

impl Plants for WallPlant {}

/// [`no_interference`]'s planted bug.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InterferencePlant {
    None,
    /// An unsettled gap (the budget spent, the clearance inside the interval) reported as a pass.
    UnknownPasses,
}

impl Plants for InterferencePlant {}

/// [`fits_within`]'s planted bug.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FitPlant {
    None,
    /// The envelope without the part searched, in place of the part without the envelope.
    Reversed,
}

impl Plants for FitPlant {}

/// A part's index in its [`Model`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct PartId(pub u32);

/// What [`Model::new`] takes.
#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct ModelOpts<'a> {
    /// Where the graph and the part list live.
    pub alloc: Alloc<'a>,
    /// The graph's bounds.
    pub limits: blaze_cad_field::Limits = blaze_cad_field::Limits::SHIPPED,
    /// The most parts (rule 18).
    pub most_parts: u32 = 1 << 16,
}

/// The graph and its named parts. Building solids is the graph's ([`Model::graph`]); the model
/// keeps which of them are parts, under unique names, and where each sits.
pub struct Model<'a> {
    graph: Graph<'a, NoPlant>,
    parts: Vec<Part, Alloc<'a>>,
    most_parts: u32,
}

impl<'a> Model<'a> {
    /// An empty model.
    pub fn new(o: ModelOpts<'a>) -> Model<'a> {
        Model {
            graph: Graph::new(blaze_cad_field::GraphOpts { alloc: o.alloc, limits: o.limits }),
            parts: Vec::new_in(o.alloc),
            most_parts: o.most_parts,
        }
    }

    /// The graph, to build solids in.
    pub fn graph(&mut self) -> &mut Graph<'a, NoPlant> {
        &mut self.graph
    }

    /// The graph, to compile from.
    pub fn graph_ref(&self) -> &Graph<'a, NoPlant> {
        &self.graph
    }

    /// The placed solid as a named part: its id in the model and the part at its own level (for
    /// the checks that need it, such as [`min_wall`]).
    pub fn add<K: Ex + Weakens<Bound>>(&mut self, o: PartOpts<'_, K>) -> Result<(PartId, Part<K>), ModelError> {
        let name = Name::new(o.name);
        if self.parts.iter().any(|p| p.name == name) {
            return Err(ModelError::DuplicateName { name });
        }
        let id = u32::try_from(self.parts.len()).map_err(|_| ModelError::TooManyParts { most: self.most_parts })?;
        if id >= self.most_parts {
            return Err(ModelError::TooManyParts { most: self.most_parts });
        }
        let part = Part::new(&mut self.graph, o)?;
        self.parts.try_push(part.weaken::<Bound>()).map_err(|_| ModelError::Alloc { what: "the model's parts" })?;
        Ok((PartId(id), part))
    }

    /// Every part, in the order added.
    pub fn parts(&self) -> &[Part] {
        &self.parts
    }

    /// One part.
    pub fn part(&self, id: PartId) -> Result<&Part, ModelError> {
        let at = usize::try_from(id.0).map_err(|_| ModelError::NoPart { id: id.0 })?;
        self.parts.get(at).ok_or(ModelError::NoPart { id: id.0 })
    }

    /// Move one part: only its placement changes.
    pub fn move_part(&mut self, id: PartId, placement: Placement) -> Result<(), ModelError> {
        let at = usize::try_from(id.0).map_err(|_| ModelError::NoPart { id: id.0 })?;
        let p = self.parts.get_mut(at).ok_or(ModelError::NoPart { id: id.0 })?;
        *p = p.moved_to(placement);
        Ok(())
    }

    /// Every part where it sits, as one solid.
    pub fn assembly(&mut self) -> Result<Solid<Bound>, ModelError> {
        assemble(&mut self.graph, &self.parts)
    }
}

/// Everything a model is written with, in one `use`.
pub mod prelude {
    pub use blaze_cad_field::{
        Axis, BlendOpts, Bound, BoxOpts, CapsuleOpts, ConeOpts, CylinderOpts, Detail, EllipsoidOpts, Exact, ExtrudeOpts, Graph,
        GraphOpts, HalfSpaceOpts, Inside, MirrorOpts, OffsetOpts, Outside, Placement, RepeatOpts, RevolveOpts, ScaleOpts, ShellOpts,
        Solid, SphereOpts, TorusOpts,
    };
    pub use blaze_cad_picture::{PictureOpts, Projection, View};
    pub use blaze_units::{Angle, Area, Density, Length, Mass, Volume};

    pub use crate::Shown;
    pub use crate::{
        ExportOpts, FitOpts, InterferenceOpts, MeshPartOpts, MinWallOpts, Model, ModelError, ModelOpts, Part, PartId, PartOpts,
        PieceCache, PieceCacheOpts, RoundEdgesOpts, Verdict, assemble, export_glb, fits_within, mass_properties, mesh_part, min_wall,
        mm3, no_interference, part_tape, picture, placed_tape, round_edges,
    };
}

/// Lengths in millimetres from three numbers: the commonest spelling of a size.
pub fn mm3(x: f64, y: f64, z: f64) -> [Length; 3] {
    [Length::mm(x), Length::mm(y), Length::mm(z)]
}
