//! TRIANGLE MESHES for blaze's CAD library (docs/v2/plan/design/end-state.md, D183): the mesher
//! that turns the implicit kernel's field into a verified mesh ([`contour`]), and everything a
//! mesh, from there or anywhere else, needs before a game can use it. Only [`contour`] reads the
//! kernel (blaze-cad-field); every other module takes a mesh.
//!
//! # Positions are f32: an explicit quantisation
//!
//! A mesh's positions are `[f32; 3]` metres ([`geom::P3`]): the format glTF and every GPU read.
//! The kernel works in f64, so meshing ROUNDS each vertex to the nearest f32, and says by how
//! much: [`Meshed::quantum`] is half an f32 step at the model's largest coordinate (a
//! [`Length`](blaze_units::Length), rounded up to the nanometre), and [`Meshed::vertex_bound`]
//! adds it to the contouring error. A mesh from elsewhere arrives in f32 already. Every later
//! stage states its own bound beside the mesh (decimation's `error_bound`, quantisation's
//! `Within`), each a `Length` at the public boundary.
//!
//! | module | what |
//! |---|---|
//! | [`contour`] | [`mesh()`]: the field to a verified mesh by primal contouring on an adaptive absolute octree (a cell stops where the quadric's residual is under a leaf), in pieces keyed by [`RegionKey`] that a [`PieceCache`] keeps; [`solve_redistance`] |
//! | [`geom`] | the plain 3-vectors, box and `sqrt` this crate needs, in one module so blaze-math's types replace it whole |
//! | [`ids`] | [`VertexId`], [`TriId`], [`PartId`], [`MaterialRole`], [`VertexPair`]: typed ids, never bare integers |
//! | [`mesh`] | [`Mesh`] in columns, [`MeshView`] borrowed and shape-checked, [`Columns`], [`MeshVecs`] |
//! | [`digest`] | [`MeshDigest`]: BLAKE3 over the canonical bytes, the cache key of every later stage (rule 21) |
//! | [`check`] | [`check()`]: closed, manifold, oriented, no degenerate or duplicate triangle, finite; the proof [`Verified`] (rule 74) |
//! | [`measure`] | [`components`], [`area`], [`bounds`], [`mass_properties`] (volume, centroid, inertia by exact polyhedral integrals) |
//! | [`canon`] | the canonical triangle order the order-independent algorithms start from |
//! | [`weld`] | [`weld()`]: triangle soup to an indexed mesh by exact positions, degenerate, duplicate and twin triangles removed |
//! | [`shade`] | [`vertex_normals`] and [`shade()`]: normals split at a crease angle, at crease marks and at material borders |
//! | [`uv`] | [`unwrap_uvs`] and [`compute_tangents`]: deterministic chart UV unwrapping, shelf atlas packing with gutter, and MikkTSpace-compatible vertex tangents |
//! | [`simplify`] | [`decimate()`]: quadric error metrics by edge collapse, creases and borders kept, the error reported (named `decimate` so it is never confused with the kernel's `simplify` of a tape) |
//! | [`lod`] | [`lod_chain`]: levels of detail with each level's error and its switch distance |
//! | [`cache`] | [`optimize_vertex_cache`] (Tipsify), [`optimize_vertex_fetch`], [`acmr`] |
//! | [`cluster`] | [`clusters()`]: meshlets of at most 64 vertices and 124 triangles, with bounding spheres and normal cones |
//! | [`quant`] | 16-bit positions in the mesh's box, octahedral normals, each with its stated round-trip bound |
//! | [`hull`] | [`convex_hull`]: quickhull, deterministic, a stated tolerance, typed failures |
//! | [`decompose`] | [`decompose()`]: an approximate convex decomposition by axis-aligned splits |
//!
//! # The same bytes everywhere
//!
//! Output is a function of the input alone, on every machine and for every worker count (rule
//! 25): floating point is IEEE basic operations and `sqrt` in a fixed order (Rust never
//! contracts `a * b + c` into a fused multiply-add on its own), no hash table is iterated, every
//! sort's key is total (ties broken by ids), and nothing reads a clock, the environment or a
//! thread. Which algorithms are also independent of the ORDER their input triangles arrive in is
//! said on each: they start from [`canon`]'s order.
//!
//! # Memory
//!
//! Every function that allocates takes the allocator of its output (`alloc`) and, when it needs
//! one, the allocator of its scratch (`scratch`), and returns a typed error naming the failure;
//! nothing allocates through `Global` and nothing panics on a refusal. Every loop is bounded by
//! its input or by a field of [`MeshLimits`]; nothing recurses.

#![no_std]
// `core_float_math`: `core::f64::math::sqrt` and `round_ties_even` (correctly rounded IEEE
// operations, the same on every target) in a `no_std` crate.
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

pub mod cache;
pub mod canon;
pub mod check;
pub mod cluster;
pub mod contour;
pub mod decompose;
pub mod digest;
pub mod feature;
pub mod geom;
pub mod hull;
pub mod ids;
pub mod lod;
pub mod measure;
pub mod mesh;
mod plant;
pub mod quant;
pub mod shade;
pub mod simplify;
pub mod uv;
mod vecs;
pub mod weld;

#[cfg(test)]
mod tests;

pub use cache::{AcmrOpts, CacheError, CacheOpts, FetchOpts, acmr, optimize_vertex_cache, optimize_vertex_fetch};
pub use canon::{CanonError, CanonOpts, canonical};
pub use check::{CheckOpts, Defect, Proof, Verified, check};
pub use cluster::{Cluster, ClusterCaps, ClusterError, ClusterOpts, Clusters, clusters, clusters_planted};
pub use contour::{
    CONTOUR_SCHEMA, GridSizing, MeshError, MeshOpts, MeshStats, Meshed, PieceCache, PieceCacheOpts, QEF_RANK_CUT, RedistanceOpts,
    RegionKey, auto_grid, mesh, mesh_cached, mesh_planted, solve_redistance, solve_redistance_planted,
};
pub use decompose::{ConvexPiece, DecomposeError, DecomposeOpts, Decomposition, decompose};
pub use digest::MeshDigest;
pub use feature::{ClosestTrianglePoint, FeatureProtect, FeatureRegion, FeatureZone, TriangleRegion, closest_point_on_triangle};
pub use geom::Aabb;
pub use hull::{Flat, Hull, HullError, HullOpts, convex_hull, convex_hull_planted};
pub use ids::{MaterialRole, PartId, TriId, VertexId, VertexPair};
pub use lod::{LodChain, LodError, LodLevel, LodOpts, Projection, lod_chain};
pub use measure::{
    Components, CreaseProfile, CreaseProfileOpts, MassProps, Topology, area, bounds, components, crease_profile, mass_properties,
    topology,
};
pub use mesh::{Column, Columns, Mesh, MeshVecs, MeshView, ShapeError};
pub use plant::{ClusterPlant, ContourPlant, DecimatePlant, HullPlant, QuantPlant, RedistancePlant, UvPlant};
pub use quant::{
    Dequantize, OCT8_BOUND, OCT16_BOUND, OctBits, OctNormals, QuantError, QuantNormalsOpts, QuantOpts, QuantizedPositions,
    Within, decode_oct8, decode_oct16, encode_oct8, encode_oct16, quantize_normals, quantize_positions,
    quantize_positions_planted,
};
pub use shade::{COS_32_DEGREES, ShadeError, ShadeOpts, shade, vertex_normals};
pub use simplify::{DecimateError, DecimateOpts, DecimateStats, Decimated, Source, Stop, Target, decimate, decimate_planted};
pub use uv::{UvError, UvMapped, UvOpts, compute_tangents, compute_tangents_planted, unwrap_uvs, unwrap_uvs_planted};
pub use weld::{Soup, Twins, WeldError, WeldOpts, Welded, weld};

/// The caps of every algorithm here (D165: a field each, the shipped values one constant, drawn
/// down by the simulation). Passing one is a typed refusal naming it, never a truncated result.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MeshLimits {
    /// The most vertices a mesh handed in may have.
    pub most_vertices: u32,
    /// The most triangles a mesh handed in may have.
    pub most_triangles: u32,
    /// The most points a convex hull is asked of.
    pub most_hull_points: u32,
    /// The most vertices a convex hull may grow to (each one a step of quickhull).
    pub most_hull_vertices: u32,
    /// The most pieces a convex decomposition makes.
    pub most_pieces: u32,
    /// The most times a decomposition piece is split in two, from the whole mesh down.
    pub most_split_depth: u32,
    /// The most levels a level-of-detail chain has, level 0 (the input) included.
    pub most_lod_levels: u32,
}

impl MeshLimits {
    /// What blaze ships: 2^26 vertices and 2^27 triangles (a ship's whole hull at a fine
    /// tolerance is about 2^22), 2^22 hull points of which 4,096 may be vertices of the hull, 64
    /// decomposition pieces split at most 12 deep, 16 levels of detail.
    pub const SHIPPED: MeshLimits = MeshLimits {
        most_vertices: 1 << 26,
        most_triangles: 1 << 27,
        most_hull_points: 1 << 22,
        most_hull_vertices: 4096,
        most_pieces: 64,
        most_split_depth: 12,
        most_lod_levels: 16,
    };
}
