//! THE MESHER (D183): a field tape (blaze-cad-field) to a closed, oriented, VERIFIED triangle
//! mesh, on an ADAPTIVE octree whose cells stop where the surface in them is flat enough, made in
//! PIECES a cache can keep.
//!
//! # The grid
//!
//! Every cell is a cube on ONE absolute dyadic grid of nanometres: the cell at shift `j` and
//! index `i` is `[i 2^j, (i + 1) 2^j]` nm on each axis. The grid never moves with the model, so
//! a far edit changes no cell's box, and every tolerance uses the same cells (only where the
//! walk stops differs).
//!
//! - LEAF: the largest power of two of nanometres at most half the tolerance (`leaf ≤ t/2`), so
//!   every tolerance of one octave meshes to the same bytes and shares pieces. The finest cell.
//! - FEATURE: [`MeshOpts::feature`] rounded down to a power of two (between the leaf and the
//!   piece): the largest cell a surface may stop in.
//! - PIECE: the cells of side [`MeshOpts::piece`] (rounded down to a power of two, at least two
//!   leaves): the unit that is keyed, cached and recomputed.
//! - ROOTS: the cells of the least shift whose side covers the tape's box grown by a leaf; at
//!   most two per axis. From the roots down to pieces a cell is kept when `eval_box` of the root
//!   tape over it does not exclude zero.
//!
//! # Where the walk stops (the residual test)
//!
//! Within a piece the walk goes down from the piece's cell. A cell `eval_box` excludes is empty or
//! full and stops at once, with its subtree. A cell of side `s` the surface may cross STOPS when it
//! is a leaf, or when all of these hold, and SPLITS into its eight children otherwise:
//!
//! 1. `s` is at most the feature side;
//! 2. ONE SHEET: on the 3×3×3 lattice of samples at half-side spacing, each of the 12 edges
//!    changes sign at most once along its three samples, each face's eight border samples change
//!    at most twice around (none: its centre agrees), the centre is not an island the border
//!    samples all disagree with, and the crossings of the 12 edges close into exactly one cycle;
//! 3. THE RESIDUAL: with each edge crossing `pᵢ` found to a leaf (below) and its unit normal `nᵢ`
//!    from `eval_grad`, the vertex `x` (the quadric's minimiser, below, clamped into the cell and
//!    moved onto the surface by at most two Newton steps) is within a leaf of every crossing's
//!    tangent plane, `|nᵢ · (x − pᵢ)| ≤ leaf`, and `|f(x)| ≤ leaf`.
//!
//! The residual is the sagitta of the cell's chord: on a surface of curvature radius `R` a cell of
//! side `s` spans an arc of chord up to `√2 s`, whose tangent planes miss a common point by about
//! `s²/(4R)`. So a flat face stops at the feature side whatever the tolerance; a curved one at
//! `s ≈ √(2 R T)` with `T = 2 leaf`; a crease or a corner stops as soon as the cell is one sheet
//! (the planes meet at a point, residual zero); only ROUNDS (small `R`) and THIN FEATURES (two
//! sheets in one cell) go deep. What can be missed: a closed feature that fits between the lattice
//! samples of a cell that stopped above the leaf, so smaller than half the feature side on every
//! axis. That is the trade [`MeshOpts::feature`] names; at the feature side equal to the leaf the
//! walk is the uniform one and misses nothing larger than a leaf.
//!
//! # How many triangles (the expected count, from geometry)
//!
//! A chord error `t` on a circle of radius `R` gives segments of angle `θ = 2 acos(1 − t/R)`,
//! chords `2 √(2Rt − t²) ≈ √(8Rt)`: the IDEAL (anisotropic) mesh of a cylinder of radius `R` has
//! `⌈2π/θ⌉` segments around, `4⌈2π/θ⌉ − 4` triangles, its sides and caps. The octree cannot
//! stretch its cells along the straight direction; it crosses about `(|n_x| + |n_y| + |n_z|) A/s²`
//! cells of side `s` for a surface of area `A` and normal `n` (`4/π` on average around a cylinder,
//! 1 for a face along an axis), and a cell's fan has as many triangles as its border has crossings
//! and facet points: 3 to 6 for a plane through a cube, about 6 counting the splits finer
//! neighbours add. So the OCTREE COUNT is `N ≈ 6 k A / s²` with `s` the power of two at most
//! `min(piece, feature, √(2 R T))`; [`crate::decimate`] at the same tolerance takes it toward the
//! ideal, and blaze-cad's export runs it by default. The tests hold a cylinder within four times
//! `N` and a box's count independent of the tolerance (the planted uniform walk breaks the first).
//!
//! The hull section of blaze-cad's example at `t = 1 mm`: leaf `2^18` nm (0.262 mm), `T` 0.524 mm,
//! `√(2·300·0.524) = 17.7` mm, so every face stops at the feature side, `2^23` nm (8.39 mm, 70.4
//! mm² a cell), except the rounds (the opening's `r = 25` mm at 4.19 mm, the cover's `r = 10` mm at
//! 2.10 mm). Skin: 1.11 m² curved (`k = 4/π`) and 0.03 m² flat, about 20,500 cells, 1.2·10⁵
//! triangles (the uniform walk made 4.2·10⁷). Frames: 0.48 m² weighted, about 4·10⁴ (1.4·10⁷).
//! Deck: 0.64 m², about 5.5·10⁴ (1.87·10⁷). Cover: 0.145 m² and 0.015 m² of rounds at 2.1 mm,
//! about 3·10⁴ (3.7·10⁶). The ideal at 1 mm (`θ = 9.4°` at `R = 300`): about 160, 590, 130 and 200.
//!
//! # Contouring: manifold by construction
//!
//! A corner is INSIDE when its value is below zero (zero is outside). Contouring reads only the
//! tree's stopped cells, never a fixed resolution, and no 2:1 balance is needed for it to be
//! crack-free:
//!
//! - FACETS. Two stopped cells that share part of a face share a FACET: the face of the smaller
//!   one (either, when equal). A cell's faces are cut into the facets of the cells across them.
//! - BORDERS. A facet's border is walked counter-clockwise about `+k` (`k` its normal axis), with
//!   every corner of every stopped cell that lies on it: the corners of all the cells around each
//!   border line, found by point location in the tree. So every segment between two border points
//!   is one segment of the grid, the same for every facet that has it: no T-junction can form.
//! - CROSSINGS. On each grid segment whose ends differ, one crossing: regula falsi with a
//!   half-leaf squeeze, then bisection, to a bracket of at most a leaf, its linear interpolation
//!   kept an eighth of a leaf from both ends. A function of the segment alone.
//! - SEGMENTS. On each facet, each inside arc of the border, from the crossing where the walk
//!   enters the inside (ENTRY) to the next (EXIT), is cut off by one segment: ENTRY to EXIT for the
//!   cell the facet is the high face of, EXIT to ENTRY for the other. A facet with one segment
//!   whose two tangent lines (the normals projected into the facet) meet at an angle of 30° or
//!   more, inside the facet, more than half a leaf off the chord and where `|f| ≤ leaf/2` gets
//!   that meeting point as a FACET POINT in its segment: a crease crossing the facet is kept.
//! - CYCLES. A cell's segments chain into cycles: each crossing on its border lies on one grid
//!   segment, which lies on exactly two of its facets, and is left once and entered once. A cycle
//!   of two points cuts nothing off and is dropped; each other cycle is a fan from one CENTRE.
//! - CENTRES. A cell with one cycle: the quadric's minimiser over its crossings; with several:
//!   each cycle's mean; either clamped into the cell shrunk by an eighth of a leaf, then moved
//!   toward the surface by at most two Newton steps (clamped, kept only when `|f|` drops).
//!
//! Why that is a closed 2-manifold: every segment is in the two cells sharing its facet, once each
//! way, so every edge of a fan's rim is in two triangles of opposite directions; every crossing is
//! entered and left once in each cell around its grid segment, so its triangles form one fan; a
//! centre's triangles are its cycle's fan. Every triangle is a centre and two points of one facet,
//! and the centre is at least an eighth of a leaf off that facet's plane, so none is degenerate;
//! crossings are a quarter leaf apart, facet points an eighth of a leaf inside their facet, centres
//! an eighth of a leaf inside their cell, and f32 keeps an eighth of a leaf ([`MeshError::TooFine`]),
//! so welding by exact position joins exactly the copies of one vertex. The argument is the
//! design; the PROOF of each result is the verifier, which runs on every mesh this module returns
//! ([`Meshed::verified`]).
//!
//! # The vertex: the quadric's minimiser
//!
//! `Σ (nᵢ · (x − pᵢ))²` over the crossings, solved about their mean `m` by the eigenvectors of
//! `Σ nᵢ nᵢᵀ` (Jacobi, a fixed order): directions whose eigenvalue is below [`QEF_RANK_CUT`] of the
//! largest are left at the mean. So a flat patch's vertex is its mean moved onto its plane, a
//! crease's is on the crease nearest the mean, and a corner's is the corner.
//!
//! # Seams are bit-identical by construction
//!
//! A piece is meshed from the ROOT tape simplified over the piece grown by one leaf (`simplify`
//! is bit-exact inside its box). Every number on a piece's boundary (a corner's value, a crossing,
//! its normal, a facet point) is a function of the boundary segment or facet alone, computed by
//! `eval_point` and `eval_grad` at the same points in the same order by both pieces, and the
//! borders' corners come from the trees of both. Each cell belongs to one piece and emits its own
//! fans, so each triangle is made exactly once, and the stitch is `weld` by exact position.
//!
//! # Records (D183: a record earns its place)
//!
//! | thing | kept as | key | cost to make | size |
//! |---|---|---|---|---|
//! | the skeleton (roots down to pieces) | RECOMPUTED every run | — | one `eval_box` of the root tape per kept cell above the pieces: about `8/7 × pieces` boxes | a list of cell indices |
//! | a piece's tree (its stopped and split cells) | the session's [`PieceCache`] | [`RegionKey`] with `around` zero: the region's tape, box, leaf, feature, schema | per cell looked at one `eval_box`; per cell tested 27 points, its crossings and normals, a 3×3 eigensolve | 32 bytes a cell |
//! | a piece's triangles | the session's [`PieceCache`]; to disk only at a checkpoint (commit, a passed check, an export) | [`RegionKey`]: the tree's key and `around`, the 26 neighbours' tape digests (their trees cut its facets) | per stopped cell: its facets' borders, a point per border corner, a crossing and a gradient per crossing, a centre | 36 bytes a triangle, about 6 a stopped cell |
//! | the welded, verified mesh and its levels of detail | CHECKPOINT only (an export or a passed check writes it): rebuilt from pieces by a weld, a sort over its corners | the model's key and the tolerance | a weld and a verify: `O(T log T)` | 12 bytes a vertex, 12 a triangle |
//!
//! Whether a piece or a tree came from the cache never changes a byte of the result: the tests
//! hold the cached mesh equal to a from-scratch one, digest for digest.
//!
//! # What the key covers (rule 21)
//!
//! The tape digest covers every instruction and constant of the region's field (its `Detail`
//! too, which the caller compiled it at); the box is the piece's; `leaf_nm` and `feature_nm` are
//! the tolerance and the feature side as the grid sees them; `around` covers the neighbours whose
//! trees cut the piece's border; `schema` is [`CONTOUR_SCHEMA`], raised whenever a rule above
//! changes. The halo (one leaf) is a function of the leaf and the box. Nothing of the machine is
//! in it.

#![blaze::sans_io]

use blaze_blake3::Hasher;
use blaze_cad_field::{
    CadError, CadPlant, CompileError, CompileOpts, Digest, EvalError, Exact, Graph, GridOpts, Lanes, Node, Points, Scratch, Solid, Tape,
    compile, eval_box, eval_grad, eval_point, eval_points, simplify,
};
use blaze_math::{Aabb3, Interval, Vec3};
use blaze_own::planted::{NoPlant, Planted};
use blaze_own::{Alloc, AllocFailure, Vec};
use blaze_units::Length;

use crate::MeshLimits;
use crate::check::{CheckOpts, Defect, Proof, Verified, check};
use crate::geom::{D3, M3, P3, add, dot, length_up, narrow, scale, sub, unit};
use crate::ids::{MaterialRole, PartId};
use crate::mesh::Mesh;
use crate::plant::{ContourPlant, RedistancePlant};
use crate::vecs::{V, copied, extend, filled, push};
use crate::weld::{Soup, Twins, WeldError, WeldOpts, weld};

/// The version of the rules above, held in every [`RegionKey`]: raised when a rule changes, so a
/// piece made by older rules is never read as a newer one's.
pub const CONTOUR_SCHEMA: u32 = 2;

/// The quadric's rank cut: an eigendirection of `Σ nᵢ nᵢᵀ` whose eigenvalue is below this share of
/// the largest is left at the crossings' mean (normals within about 14° are one plane).
pub const QEF_RANK_CUT: f64 = 1.0 / 32.0;

/// `√3`, a cell's diagonal over its side.
const SQRT_3: f64 = 1.732_050_807_568_877_3;

#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct MeshOpts<'a, 's> {
    /// Where the mesh and its key list live.
    pub alloc: Alloc<'a>,
    /// Where the walk's lists, the region tapes and the soup live.
    pub scratch: Alloc<'s>,
    /// The tolerance: sets the leaf (module documentation). At least 16 nm.
    pub tolerance: Length,
    /// The side of a piece, the unit of caching (rounded down to a power of two of nanometres, at
    /// least two leaves). 2^24 nm is about 17 mm.
    pub piece: Length = Length::nm(1 << 24),
    /// The largest cell a surface may stop in (rounded down to a power of two of nanometres,
    /// between the leaf and the piece): a closed feature smaller than half of it on every axis can
    /// fall between a stopped cell's samples (module documentation). 2^23 nm is about 8.4 mm.
    pub feature: Length = Length::nm(1 << 23),
    /// The most cells the walk may look at.
    pub most_cells: u64 = 1 << 28,
    /// The weld's and the verifier's caps.
    pub limits: MeshLimits = MeshLimits::SHIPPED,
}

/// What a piece is keyed by: the README's `RegionKey`, with the tolerance as the leaf it maps to
/// (so every tolerance of one octave shares pieces), the feature side, the neighbours and the
/// rules' version.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct RegionKey {
    /// `simplify(root, eval_box(root, piece grown by a leaf)).digest()`.
    pub tape: Digest,
    /// The piece's box, nanometres: low corner then high corner.
    pub cell: [i64; 6],
    /// The leaf's side, nanometres.
    pub leaf_nm: i64,
    /// The feature side, nanometres.
    pub feature_nm: i64,
    /// BLAKE3 of the 26 neighbouring pieces' tape digests (zeros for a piece the walk did not
    /// keep), in `z`, `y`, `x` order: their trees cut this piece's border. Zero in a tree's key.
    pub around: Digest,
    pub schema: u32,
}

/// What a run did, in work units, not time (D183).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct MeshStats {
    /// `eval_box` calls: cells looked at.
    pub boxes: u64,
    /// Stopped cells the surface may cross, in recomputed pieces: the cells contoured.
    pub leaves: u64,
    /// Points evaluated.
    pub points: u64,
    /// Gradients evaluated.
    pub grads: u64,
    /// Instructions run: each evaluation's tape length, summed.
    pub work: u64,
    pub pieces: u32,
    /// Pieces whose triangles were made in this run.
    pub recomputed: u32,
    /// Pieces whose triangles were read from the cache.
    pub reused: u32,
    /// Pieces whose tree was read from the cache.
    pub trees_reused: u32,
    /// Made pieces or trees the cache had no room for.
    pub not_stored: u32,
    /// Triangles in the result.
    pub triangles: u32,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum MeshError {
    /// A tolerance below 16 nm (a leaf must hold an eighth of itself in whole nanometres).
    BadTolerance { tolerance: Length },
    /// A piece side that is not positive.
    BadPiece { piece: Length },
    /// A feature side that is not positive.
    BadFeature { feature: Length },
    /// The solid's box is empty: there is no surface to mesh.
    Empty,
    /// The solid has no finite box (a half-space not cut off): there is nothing to bound a walk.
    Unbounded,
    /// The solid's box is beyond ±10^9 m.
    OutOfRange,
    /// At this extent f32 positions cannot keep vertices an eighth of a leaf apart: a coarser
    /// tolerance, or a model nearer the origin.
    TooFine { leaf: Length, extent: Length },
    TooManyCells { most: u64 },
    /// A cell's border segments did not close into cycles, or a point its border needs was not
    /// found: a bug here.
    Unpaired { cell: [i64; 3], shift: u32 },
    /// Two cycles of one cell put their centres at one f32 position.
    CoincidentCentres { cell: [i64; 3], shift: u32 },
    Compile(CompileError),
    Eval(EvalError),
    Weld(WeldError),
    /// The verifier refused the result (see the module documentation for why it should not).
    Verify(Defect),
    Alloc(AllocFailure),
    /// [`solve_redistance`] was given a solid whose node is not a redistance.
    NotRedistance,
    /// [`solve_redistance`]'s step is not positive or its band is negative.
    BadGrid { step: Length, band: Length },
    Graph(CadError),
    TooManyGridValues { values: u64, most: u64 },
}

impl From<AllocFailure> for MeshError {
    fn from(e: AllocFailure) -> MeshError {
        MeshError::Alloc(e)
    }
}

/// The mesh, its proof and its stated bounds.
#[derive(Debug)]
pub struct Meshed<'a> {
    pub mesh: Mesh<'a>,
    proof: Proof,
    /// THE f32 QUANTISATION, stated: positions are f32 metres, each rounded to nearest from the
    /// f64 vertex, so each is within half an f32 step at the model's largest coordinate, at most
    /// this (rounded up to the nanometre).
    pub quantum: Length,
    /// Every vertex `v` has `|f(v)|` at most this. The f64 crossings are within a leaf of a sign
    /// change along their segment and a leaf cell's centre within the leaf's diagonal of the
    /// surface, so for a 1-Lipschitz field they are under `√3 leaf`; facet points were taken only
    /// where `|f| ≤ leaf/2`; every centre's `|f|` is measured and the largest kept. The bound is
    /// the larger of `√3 leaf` and that largest, plus two quanta (the f32 rounding moves a vertex
    /// by at most `√3` quanta). For a field that is a distance it is a distance bound.
    pub vertex_bound: Length,
    /// Every piece's key, in the canonical order, and whether this run made its triangles.
    pub keys: Vec<(RegionKey, bool), Alloc<'a>>,
    pub stats: MeshStats,
}

impl Meshed<'_> {
    /// The mesh with the verifier's proof (run once on it, rule 74).
    pub fn verified(&self) -> Verified<'_> {
        Verified::with_proof(self.mesh.view(), self.proof)
    }
}

/// A cell of the grid: its shift and its index at that shift.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
struct Cell {
    shift: u32,
    i: [i64; 3],
}

/// A cell of a piece's tree: split into its children, or stopped (and possibly crossed).
#[derive(Clone, Copy, Debug)]
struct TreeCell {
    cell: Cell,
    split: bool,
}

/// A kept piece's triangles and the largest `|f|` at its centres.
#[derive(Clone, Copy, Debug)]
struct Span {
    start: u32,
    len: u32,
    far: f64,
}

/// The session's piece cache: bounded, read in place, sorted by key (no hash order anywhere).
/// Two tables: each piece's tree under its own key (`around` zero), and each piece's triangles
/// under its full key. What does not fit under its cap is not kept, and that is counted; it never
/// changes a result.
pub struct PieceCache<'c> {
    keys: V<'c, RegionKey>,
    spans: V<'c, Span>,
    tris: V<'c, [P3; 3]>,
    tree_keys: V<'c, RegionKey>,
    tree_spans: V<'c, (u32, u32)>,
    tree_cells: V<'c, TreeCell>,
    most_triangles: u32,
    most_cells: u32,
}

#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct PieceCacheOpts<'c> {
    pub alloc: Alloc<'c>,
    /// The most triangles kept, all pieces together (36 bytes each).
    pub most_triangles: u32 = 1 << 24,
    /// The most tree cells kept, all pieces together (32 bytes each).
    pub most_cells: u32 = 1 << 24,
}

impl<'c> PieceCache<'c> {
    pub fn new(o: PieceCacheOpts<'c>) -> PieceCache<'c> {
        PieceCache {
            keys: Vec::new_in(o.alloc),
            spans: Vec::new_in(o.alloc),
            tris: Vec::new_in(o.alloc),
            tree_keys: Vec::new_in(o.alloc),
            tree_spans: Vec::new_in(o.alloc),
            tree_cells: Vec::new_in(o.alloc),
            most_triangles: o.most_triangles,
            most_cells: o.most_cells,
        }
    }

    /// How many pieces' triangles are kept.
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// The triangles of the piece under `key`, if kept.
    pub fn get(&self, key: &RegionKey) -> Option<&[[P3; 3]]> {
        self.piece(key).map(|(tris, _)| tris)
    }

    fn piece(&self, key: &RegionKey) -> Option<(&[[P3; 3]], f64)> {
        let i = self.keys.binary_search(key).ok()?;
        let span = self.spans[i];
        let start = blaze_int::idx(span.start);
        Some((&self.tris[start..start + blaze_int::idx(span.len)], span.far))
    }

    /// Keeps `tris` under `key`; `false` when there is no room under the cap.
    fn put(&mut self, key: RegionKey, tris: &[[P3; 3]], far: f64) -> Result<bool, AllocFailure> {
        let Err(at) = self.keys.binary_search(&key) else { return Ok(true) };
        let (have, add) = (self.tris.len(), tris.len());
        if have + add > blaze_int::idx(self.most_triangles) {
            return Ok(false);
        }
        self.keys.try_reserve(1)?;
        self.spans.try_reserve(1)?;
        extend(&mut self.tris, tris)?;
        self.keys.insert_shift(at, key);
        // Both below `most_triangles`, a `u32`.
        self.spans.insert_shift(at, Span { start: have as u32, len: add as u32, far });
        Ok(true)
    }

    fn tree(&self, key: &RegionKey) -> Option<&[TreeCell]> {
        let i = self.tree_keys.binary_search(key).ok()?;
        let (start, len) = self.tree_spans[i];
        let start = blaze_int::idx(start);
        Some(&self.tree_cells[start..start + blaze_int::idx(len)])
    }

    fn put_tree(&mut self, key: RegionKey, cells: &[TreeCell]) -> Result<bool, AllocFailure> {
        let Err(at) = self.tree_keys.binary_search(&key) else { return Ok(true) };
        let (have, add) = (self.tree_cells.len(), cells.len());
        if have + add > blaze_int::idx(self.most_cells) {
            return Ok(false);
        }
        self.tree_keys.try_reserve(1)?;
        self.tree_spans.try_reserve(1)?;
        extend(&mut self.tree_cells, cells)?;
        self.tree_keys.insert_shift(at, key);
        // Both below `most_cells`, a `u32`.
        self.tree_spans.insert_shift(at, (have as u32, add as u32));
        Ok(true)
    }
}

/// The mesh of `t`'s solid, every piece made here.
pub fn mesh<'a>(t: &Tape<'_>, o: MeshOpts<'a, '_>) -> Result<Meshed<'a>, MeshError> {
    contour(t, o, None, NoPlant)
}

/// The mesh of `t`'s solid, each piece's tree and triangles read from `cache` when their keys are
/// there, and each made here kept in it. The bytes equal [`mesh`]'s.
pub fn mesh_cached<'a>(t: &Tape<'_>, o: MeshOpts<'a, '_>, cache: &mut PieceCache<'_>) -> Result<Meshed<'a>, MeshError> {
    contour(t, o, Some(cache), NoPlant)
}

/// [`mesh_cached`] with a planted bug, for the tests' controls.
pub fn mesh_planted<'a>(
    t: &Tape<'_>,
    o: MeshOpts<'a, '_>,
    cache: Option<&mut PieceCache<'_>>,
    plant: impl Planted<ContourPlant>,
) -> Result<Meshed<'a>, MeshError> {
    contour(t, o, cache, plant)
}

/// Nanometres to metres: THE conversion (blaze-units' `as_m`, blaze-cad-field's `m`).
fn m(nm: i64) -> f64 {
    nm as f64 / 1e9
}

/// A grid point, metres.
fn at_nm(p: [i64; 3]) -> Vec3 {
    Vec3::new(m(p[0]), m(p[1]), m(p[2]))
}

/// `⌊log2 x⌋` of a positive `x`.
fn floor_log2(x: i64) -> u32 {
    63 - (x as u64).leading_zeros()
}

/// `⌈log2 x⌉` of a positive `x`.
fn ceil_log2(x: i64) -> u32 {
    if x <= 1 { 0 } else { floor_log2(x - 1) + 1 }
}

/// The box of the cell at `shift` and index `i`, grown by `grow` nm on every side, in metres.
fn cell_box(i: [i64; 3], shift: u32, grow: i64) -> Aabb3 {
    let lo = |k: usize| m((i[k] << shift) - grow);
    let hi = |k: usize| m(((i[k] + 1) << shift) + grow);
    Aabb3::new(Vec3::new(lo(0), lo(1), lo(2)), Vec3::new(hi(0), hi(1), hi(2)))
}

/// The cell `[lo, lo + side]` (nm) shrunk by an eighth of a leaf, in metres.
fn shrunk(lo: [i64; 3], side: i64, leaf_nm: i64) -> (D3, D3) {
    let e = leaf_nm / 8;
    ([m(lo[0] + e), m(lo[1] + e), m(lo[2] + e)], [m(lo[0] + side - e), m(lo[1] + side - e), m(lo[2] + side - e)])
}

/// Whether an interval does not exclude zero (a NaN bound excludes nothing).
fn spans_zero(iv: Interval) -> bool {
    !(iv.lo() > 0.0) && !(iv.hi() < 0.0)
}

fn inside(v: f64) -> bool {
    v < 0.0
}

/// Child `d` (bit 0 x, 1 y, 2 z) of cell `c`, one shift down.
fn child(c: [i64; 3], d: i64) -> [i64; 3] {
    [2 * c[0] + (d & 1), 2 * c[1] + ((d >> 1) & 1), 2 * c[2] + ((d >> 2) & 1)]
}

/// The 12 edges of a cell by their corners (corner `x + 2y + 4z`): 0..4 along x, 4..8 along y,
/// 8..12 along z, each from its lower corner, lower corners in increasing order.
const EDGES: [(usize, usize); 12] =
    [(0, 1), (2, 3), (4, 5), (6, 7), (0, 2), (1, 3), (4, 6), (5, 7), (0, 4), (1, 5), (2, 6), (3, 7)];

/// A face's eight border samples on the 3×3 lattice, counter-clockwise.
const FACE_LOOP: [(usize, usize); 8] = [(0, 0), (1, 0), (2, 0), (2, 1), (2, 2), (1, 2), (0, 2), (0, 1)];

struct Grid {
    leaf: u32,
    piece: u32,
    feature: u32,
}

impl Grid {
    fn leaf_nm(&self) -> i64 {
        1i64 << self.leaf
    }
}

fn contour<'a>(
    t: &Tape<'_>,
    o: MeshOpts<'a, '_>,
    mut cache: Option<&mut PieceCache<'_>>,
    plant: impl Planted<ContourPlant>,
) -> Result<Meshed<'a>, MeshError> {
    let s = o.scratch;
    let tol = o.tolerance.as_nm();
    if tol < 16 {
        return Err(MeshError::BadTolerance { tolerance: o.tolerance });
    }
    if o.piece.as_nm() < 1 {
        return Err(MeshError::BadPiece { piece: o.piece });
    }
    if o.feature.as_nm() < 1 {
        return Err(MeshError::BadFeature { feature: o.feature });
    }
    let leaf = floor_log2(tol / 2);
    let piece = floor_log2(o.piece.as_nm()).max(leaf + 1);
    let feature = floor_log2(o.feature.as_nm()).max(leaf).min(piece);
    let leaf_nm = 1i64 << leaf;
    let b = t.aabb();
    if b.is_empty() {
        return Err(MeshError::Empty);
    }
    let (blo, bhi) = (b.lo.to_array(), b.hi.to_array());
    if !blo.iter().chain(bhi.iter()).all(|x| x.is_finite()) {
        return Err(MeshError::Unbounded);
    }
    let mut lo = [0i64; 3];
    let mut hi = [0i64; 3];
    for k in 0..3 {
        let (l, h) = (core::f64::math::floor(blo[k] * 1e9), core::f64::math::ceil(bhi[k] * 1e9));
        if !(l.abs() < 1e18 && h.abs() < 1e18) {
            return Err(MeshError::OutOfRange);
        }
        // Whole numbers below 2^60 in magnitude: exact.
        lo[k] = l as i64 - leaf_nm;
        hi[k] = h as i64 + leaf_nm;
    }
    let extent = (0..3).map(|k| hi[k] - lo[k]).max().unwrap_or(1).max(1);
    let root = piece.max(ceil_log2(extent));
    // f32 must keep vertices an eighth of a leaf apart: the f32 spacing at the largest coordinate
    // is at most `|x| 2^-23`, and a rounding moves a point by half of it.
    let most_abs = (0..3).map(|k| m(lo[k]).abs().max(m(hi[k]).abs())).fold(0.0f64, f64::max);
    let spacing = most_abs / 8_388_608.0;
    if !(m(leaf_nm) / 8.0 > 4.0 * spacing) {
        return Err(MeshError::TooFine { leaf: Length::nm(leaf_nm), extent: Length::nm(extent) });
    }

    let mut sc = Scratch::new(s);
    let mut stats = MeshStats::default();
    let instrs = t.len() as u64;

    // The skeleton: roots down to pieces.
    let mut cells: V<'_, [i64; 3]> = Vec::new_in(s);
    for z in (lo[2] >> root)..=(hi[2] >> root) {
        for y in (lo[1] >> root)..=(hi[1] >> root) {
            for x in (lo[0] >> root)..=(hi[0] >> root) {
                push(&mut cells, [x, y, z])?;
            }
        }
    }
    let mut shift = root;
    loop {
        let mut kept: V<'_, [i64; 3]> = Vec::new_in(s);
        for &c in cells.iter() {
            stats.boxes += 1;
            if stats.boxes > o.most_cells {
                return Err(MeshError::TooManyCells { most: o.most_cells });
            }
            stats.work += instrs;
            let (iv, _) = eval_box(t, &cell_box(c, shift, 0), &mut sc);
            if spans_zero(iv) {
                push(&mut kept, c)?;
            }
        }
        if shift == piece {
            cells = kept;
            break;
        }
        shift -= 1;
        let mut next: V<'_, [i64; 3]> = Vec::try_with_capacity_in(kept.len() * 8, s)?;
        for &c in kept.iter() {
            for d in 0..8 {
                push(&mut next, child(c, d))?;
            }
        }
        cells = next;
    }
    cells.sort_unstable();

    // Each kept piece's region tape.
    let mut regions: V<'_, Tape<'_>> = Vec::try_with_capacity_in(cells.len(), s)?;
    for &p in cells.iter() {
        stats.boxes += 1;
        stats.work += instrs;
        let (_, choices) = eval_box(t, &cell_box(p, piece, leaf_nm), &mut sc);
        let region = simplify(t, choices, s).map_err(MeshError::Compile)?;
        push(&mut regions, region)?;
    }
    let grid = Grid { leaf, piece, feature };
    let key_leaf = if plant.has(ContourPlant::KeyWithoutTolerance) { 0 } else { leaf_nm };
    let uniform = plant.has(ContourPlant::NoResidualTest);
    let own = |n: usize| {
        let p = cells[n];
        RegionKey {
            tape: regions[n].digest(),
            cell: [p[0] << piece, p[1] << piece, p[2] << piece, (p[0] + 1) << piece, (p[1] + 1) << piece, (p[2] + 1) << piece],
            leaf_nm: key_leaf,
            feature_nm: 1i64 << feature,
            around: [0u8; 32],
            schema: CONTOUR_SCHEMA,
        }
    };

    // Pass one: every piece's tree.
    let mut tree: V<'_, TreeCell> = Vec::new_in(s);
    let mut spans: V<'_, (usize, usize)> = Vec::try_with_capacity_in(cells.len(), s)?;
    for (n, &p) in cells.iter().enumerate() {
        let key = own(n);
        let start = tree.len();
        if let Some(c) = cache.as_deref()
            && let Some(kept) = c.tree(&key)
        {
            extend(&mut tree, kept)?;
            stats.trees_reused += 1;
        } else {
            piece_tree(&regions[n], p, &grid, uniform, &mut tree, &mut sc, &mut stats, s, o.most_cells)?;
            if let Some(c) = cache.as_deref_mut()
                && !c.put_tree(key, &tree[start..])?
            {
                stats.not_stored += 1;
            }
        }
        push(&mut spans, (start, tree.len()))?;
    }
    let mut splits: V<'_, Cell> = Vec::new_in(s);
    for tc in tree.iter() {
        if tc.split {
            push(&mut splits, tc.cell)?;
        }
    }
    splits.sort_unstable();

    // Pass two: every piece's triangles, in canonical order.
    let mut soup: V<'_, [P3; 3]> = Vec::new_in(s);
    let mut keys: V<'a, (RegionKey, bool)> = Vec::try_with_capacity_in(cells.len(), o.alloc)?;
    let mut stopped: V<'_, Cell> = Vec::new_in(s);
    let mut far = 0.0f64;
    for (n, &p) in cells.iter().enumerate() {
        let key = RegionKey { around: around(&cells, &regions, p), ..own(n) };
        stats.pieces += 1;
        if let Some(c) = cache.as_deref()
            && let Some((tris, piece_far)) = c.piece(&key)
        {
            extend(&mut soup, tris)?;
            push(&mut keys, (key, false))?;
            far = far.max(piece_far);
            stats.reused += 1;
            continue;
        }
        stopped.clear();
        let (a, z) = spans[n];
        for tc in &tree[a..z] {
            if !tc.split {
                push(&mut stopped, tc.cell)?;
            }
        }
        stats.leaves += stopped.len() as u64;
        let start = soup.len();
        let piece_far = piece_triangles(&regions[n], &stopped, &splits, &grid, &mut soup, &mut sc, &mut stats, s)?;
        far = far.max(piece_far);
        stats.recomputed += 1;
        push(&mut keys, (key, true))?;
        if let Some(c) = cache.as_deref_mut()
            && !c.put(key, &soup[start..], piece_far)?
        {
            stats.not_stored += 1;
        }
    }

    // The stitch: one weld by exact position, then the verifier.
    let parts = filled(soup.len(), PartId(0), s)?;
    let roles = filled(soup.len(), MaterialRole(0), s)?;
    let welded = weld(
        Soup { corners: &soup, parts: &parts, roles: &roles },
        WeldOpts { twins: Twins::Keep, limits: o.limits, alloc: o.alloc, scratch: s },
    )
    .map_err(MeshError::Weld)?;
    let mesh = welded.mesh;
    let proof = check(mesh.view(), CheckOpts { limits: o.limits, scratch: s }).map_err(MeshError::Verify)?.proof();
    stats.triangles = mesh.view().triangle_count();
    let quantum = length_up(most_abs / 16_777_216.0);
    let vertex_bound = length_up((m(leaf_nm) * SQRT_3).max(far)) + quantum + quantum;
    Ok(Meshed { mesh, proof, quantum, vertex_bound, keys, stats })
}

/// The `around` of piece `p`: BLAKE3 of its 26 neighbours' tape digests, zeros where the walk kept
/// no piece, in `z`, `y`, `x` order.
fn around(cells: &[[i64; 3]], regions: &[Tape<'_>], p: [i64; 3]) -> Digest {
    let mut h = Hasher::new_derive_key("blaze-cad-mesh 2026-11-21 contour piece neighbours");
    for dz in -1..=1i64 {
        for dy in -1..=1i64 {
            for dx in -1..=1i64 {
                if dx == 0 && dy == 0 && dz == 0 {
                    continue;
                }
                let d = match cells.binary_search(&[p[0] + dx, p[1] + dy, p[2] + dz]) {
                    Ok(i) => regions[i].digest(),
                    Err(_) => [0u8; 32],
                };
                h.update(&d);
            }
        }
    }
    h.finalize()
}

/// The stopped cell holding the point just inside `q` (a point `q` of the grid offset by less
/// than a leaf: the caller passes `p` or `p − 1` per axis to pick the side).
fn locate(splits: &[Cell], g: &Grid, q: [i64; 3]) -> Cell {
    let mut j = g.piece;
    loop {
        let c = Cell { shift: j, i: [q[0] >> j, q[1] >> j, q[2] >> j] };
        if j == g.leaf || splits.binary_search(&c).is_err() {
            return c;
        }
        j -= 1;
    }
}

/// Piece `p`'s tree, from its cell down (module documentation, "Where the walk stops"), appended
/// to `out` depth first: each cell `eval_box` keeps, split or stopped.
#[allow(clippy::too_many_arguments)]
fn piece_tree(
    r: &Tape<'_>,
    p: [i64; 3],
    g: &Grid,
    uniform: bool,
    out: &mut V<'_, TreeCell>,
    sc: &mut Scratch<'_>,
    stats: &mut MeshStats,
    s: Alloc<'_>,
    most_cells: u64,
) -> Result<(), MeshError> {
    let instrs = r.len() as u64;
    let mut stack: V<'_, Cell> = Vec::new_in(s);
    push(&mut stack, Cell { shift: g.piece, i: p })?;
    while let Some(c) = stack.pop() {
        stats.boxes += 1;
        if stats.boxes > most_cells {
            return Err(MeshError::TooManyCells { most: most_cells });
        }
        stats.work += instrs;
        let (iv, _) = eval_box(r, &cell_box(c.i, c.shift, 0), sc);
        if !spans_zero(iv) {
            continue;
        }
        let split = c.shift > g.leaf && (uniform || c.shift > g.feature || !settles(r, c, g, sc, stats)?);
        push(out, TreeCell { cell: c, split })?;
        if split {
            for d in (0..8).rev() {
                push(&mut stack, Cell { shift: c.shift - 1, i: child(c.i, d) })?;
            }
        }
    }
    Ok(())
}

/// Union-find over a cell's 12 edges.
fn root_of(parent: &[u8; 12], mut e: usize) -> usize {
    while usize::from(parent[e]) != e {
        e = usize::from(parent[e]);
    }
    e
}

fn join(parent: &mut [u8; 12], a: usize, b: usize) {
    let (ra, rb) = (root_of(parent, a), root_of(parent, b));
    // The lesser root stays: the result does not depend on the order of joins.
    let (keep, gone) = if ra < rb { (ra, rb) } else { (rb, ra) };
    // Below 12.
    parent[gone] = keep as u8;
}

/// Whether the surface in cell `c` (above the leaf) is one sheet its fan holds within a leaf: the
/// module documentation's tests 2 and 3.
fn settles(r: &Tape<'_>, c: Cell, g: &Grid, sc: &mut Scratch<'_>, stats: &mut MeshStats) -> Result<bool, MeshError> {
    let instrs = r.len() as u64;
    let leaf_nm = g.leaf_nm();
    let side = 1i64 << c.shift;
    let half = side / 2;
    let lo = [c.i[0] << c.shift, c.i[1] << c.shift, c.i[2] << c.shift];
    // Sample `n` is lattice point `(n % 3, n / 3 % 3, n / 9)`; every index below 27.
    let point = |n: usize| [lo[0] + (n % 3) as i64 * half, lo[1] + (n / 3 % 3) as i64 * half, lo[2] + (n / 9) as i64 * half];
    let lat = |x: usize, y: usize, z: usize| x + 3 * y + 9 * z;
    let corner = |k: usize| lat(2 * (k & 1), 2 * ((k >> 1) & 1), 2 * ((k >> 2) & 1));
    let mut xs = [0.0f64; 27];
    let mut ys = [0.0f64; 27];
    let mut zs = [0.0f64; 27];
    for n in 0..27 {
        let p = point(n);
        xs[n] = m(p[0]);
        ys[n] = m(p[1]);
        zs[n] = m(p[2]);
    }
    let mut v = [0.0f64; 27];
    eval_points(r, Points { x: &xs, y: &ys, z: &zs }, &mut v, Lanes::Eight, sc).map_err(MeshError::Eval)?;
    stats.points += 27;
    stats.work += 27 * instrs;
    let sign = |n: usize| inside(v[n]);

    // Nothing at the samples: the cell stops (and is contoured, should finer neighbours see more).
    if (1..27).all(|n| sign(n) == sign(0)) {
        return Ok(true);
    }
    // An island around the centre.
    if (0..27).filter(|&n| n != 13).all(|n| sign(n) == sign(0)) {
        return Ok(false);
    }
    // Each edge changes at most once.
    for &(ca, cb) in EDGES.iter() {
        let (la, lb) = (corner(ca), corner(cb));
        let lm = (la + lb) / 2;
        if sign(la) == sign(lb) && sign(lm) != sign(la) {
            return Ok(false);
        }
    }
    // Each face changes at most twice around; with none, its centre agrees.
    for face in 0..6usize {
        let (k, high) = (face / 2, face % 2);
        let (a, b) = ((k + 1) % 3, (k + 2) % 3);
        let at = |pa: usize, pb: usize| {
            let mut q = [0usize; 3];
            q[k] = 2 * high;
            q[a] = pa;
            q[b] = pb;
            lat(q[0], q[1], q[2])
        };
        let mut changes = 0;
        for n in 0..8 {
            let (p, q) = (FACE_LOOP[n], FACE_LOOP[(n + 1) % 8]);
            if sign(at(p.0, p.1)) != sign(at(q.0, q.1)) {
                changes += 1;
            }
        }
        if changes > 2 || (changes == 0 && sign(at(1, 1)) != sign(at(0, 0))) {
            return Ok(false);
        }
    }
    // The corners' crossings close into exactly one cycle (no face has four: its loop would
    // change four times).
    let crossing = |e: usize| sign(corner(EDGES[e].0)) != sign(corner(EDGES[e].1));
    let mut parent = [0u8; 12];
    for (e, p) in parent.iter_mut().enumerate() {
        // Below 12.
        *p = e as u8;
    }
    for face in 0..6usize {
        let (k, high) = (face / 2, face % 2);
        let mut on = [0usize; 2];
        let mut n = 0;
        for (e, &(ca, _)) in EDGES.iter().enumerate() {
            if e / 4 != k && (ca >> k) & 1 == high && crossing(e) {
                if n < 2 {
                    on[n] = e;
                }
                n += 1;
            }
        }
        if n == 2 {
            join(&mut parent, on[0], on[1]);
        } else if n != 0 {
            return Ok(false);
        }
    }
    let mut first_root = None;
    for e in 0..12 {
        if !crossing(e) {
            continue;
        }
        let r0 = root_of(&parent, e);
        match first_root {
            None => first_root = Some(r0),
            Some(x) if x != r0 => return Ok(false),
            Some(_) => {}
        }
    }
    if first_root.is_none() {
        return Ok(false);
    }

    // The residual.
    let mut pts = [[0.0f64; 3]; 12];
    let mut nrm = [None::<D3>; 12];
    let mut n = 0;
    for e in 0..12 {
        if !crossing(e) {
            continue;
        }
        let (la, lb) = (corner(EDGES[e].0), corner(EDGES[e].1));
        let lm = (la + lb) / 2;
        let (from, fa, fb) = if sign(la) != sign(lm) { (la, v[la], v[lm]) } else { (lm, v[lm], v[lb]) };
        let p = crossing_on(r, point(from), e / 4, half, fa, fb, leaf_nm, sc, stats);
        let (_, gr) = eval_grad(r, Vec3::new(p[0], p[1], p[2]), sc);
        stats.grads += 1;
        stats.work += instrs;
        // A point with no gradient (a kink exactly at it) gives no plane; the others still must.
        pts[n] = p;
        nrm[n] = unit([gr.x, gr.y, gr.z]);
        n += 1;
    }
    if nrm[..n].iter().all(|u| u.is_none()) {
        return Ok(false);
    }
    let (blo, bhi) = shrunk(lo, side, leaf_nm);
    let x = place(&pts[..n], &nrm[..n], mean_of(&pts[..n]));
    let (x, fx) = settle(r, x, blo, bhi, sc, stats);
    let tol = m(leaf_nm);
    if !(fx <= tol) {
        return Ok(false);
    }
    for k in 0..n {
        if let Some(u) = nrm[k]
            && !(dot(u, sub(x, pts[k])).abs() <= tol)
        {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The crossing on the grid segment from `a` along axis `e`, `len` nm long, whose ends' values
/// are `fa` (at `a`) and `fb`, of opposite insides: regula falsi (kept an eighth of the bracket
/// from its ends) with a half-leaf squeeze for eight rounds, then bisection, until the bracket is
/// at most a leaf; then the bracket's linear interpolation, kept an eighth of a leaf from both
/// ends of the segment. A function of the segment alone, in metres.
#[allow(clippy::too_many_arguments)]
fn crossing_on(
    r: &Tape<'_>,
    a: [i64; 3],
    e: usize,
    len: i64,
    fa: f64,
    fb: f64,
    leaf_nm: i64,
    sc: &mut Scratch<'_>,
    stats: &mut MeshStats,
) -> D3 {
    let instrs = r.len() as u64;
    let base = a[e] as f64;
    let at = |x: f64| {
        let mut p = [m(a[0]), m(a[1]), m(a[2])];
        p[e] = (base + x) / 1e9;
        p
    };
    let leaf = leaf_nm as f64;
    let (mut x0, mut x1, mut f0, mut f1) = (0.0f64, len as f64, fa, fb);
    let mut round = 0u32;
    while x1 - x0 > leaf {
        let w = x1 - x0;
        let x = if round < 8 { (x0 + w * (f0 / (f0 - f1))).max(x0 + w / 8.0).min(x1 - w / 8.0) } else { x0 + w / 2.0 };
        let p = at(x);
        let fx = eval_point(r, Vec3::new(p[0], p[1], p[2]), sc);
        stats.points += 1;
        stats.work += instrs;
        let low = inside(fx) == inside(f0);
        if low {
            (x0, f0) = (x, fx);
        } else {
            (x1, f1) = (x, fx);
        }
        if round < 8 && x1 - x0 > leaf {
            let y = if low { x0 + leaf / 2.0 } else { x1 - leaf / 2.0 };
            let p = at(y);
            let fy = eval_point(r, Vec3::new(p[0], p[1], p[2]), sc);
            stats.points += 1;
            stats.work += instrs;
            if inside(fy) == inside(f0) {
                (x0, f0) = (y, fy);
            } else {
                (x1, f1) = (y, fy);
            }
        }
        round += 1;
    }
    let f = f0 / (f0 - f1);
    let f = if f.is_nan() { 0.5 } else { f.max(0.0).min(1.0) };
    at((x0 + f * (x1 - x0)).max(leaf / 8.0).min(len as f64 - leaf / 8.0))
}

/// The mean of `points` (at least one), summed in order.
fn mean_of(points: &[D3]) -> D3 {
    let mut sum = [0.0f64; 3];
    for p in points {
        sum = add(sum, *p);
    }
    // At most a few hundred points: exact as f64.
    scale(sum, 1.0 / points.len().max(1) as f64)
}

/// The eigenvalues and eigenvectors (the columns of the matrix) of a symmetric 3×3 matrix, by
/// cyclic Jacobi rotations in a fixed order and count.
fn eigen3(mut a: M3) -> ([f64; 3], M3) {
    let mut v: M3 = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
    for _ in 0..8 {
        for (p, q) in [(0usize, 1usize), (0, 2), (1, 2)] {
            if a[p][q] == 0.0 {
                continue;
            }
            let theta = (a[q][q] - a[p][p]) / (2.0 * a[p][q]);
            let t = (if theta >= 0.0 { 1.0 } else { -1.0 }) / (theta.abs() + core::f64::math::sqrt(theta * theta + 1.0));
            let c = 1.0 / core::f64::math::sqrt(t * t + 1.0);
            let s = t * c;
            for row in a.iter_mut() {
                let (kp, kq) = (row[p], row[q]);
                row[p] = c * kp - s * kq;
                row[q] = s * kp + c * kq;
            }
            for k in 0..3 {
                let (pk, qk) = (a[p][k], a[q][k]);
                a[p][k] = c * pk - s * qk;
                a[q][k] = s * pk + c * qk;
            }
            for row in v.iter_mut() {
                let (kp, kq) = (row[p], row[q]);
                row[p] = c * kp - s * kq;
                row[q] = s * kp + c * kq;
            }
        }
    }
    ([a[0][0], a[1][1], a[2][2]], v)
}

/// The quadric's minimiser over `points` with `normals`, about `mean` (module documentation).
fn place(points: &[D3], normals: &[Option<D3>], mean: D3) -> D3 {
    let mut ata: M3 = [[0.0; 3]; 3];
    let mut atb: D3 = [0.0; 3];
    for (p, n) in points.iter().zip(normals) {
        let Some(n) = n else { continue };
        let r = dot(*n, sub(*p, mean));
        for i in 0..3 {
            for j in 0..3 {
                ata[i][j] += n[i] * n[j];
            }
            atb[i] += n[i] * r;
        }
    }
    let (values, vectors) = eigen3(ata);
    let top = values[0].max(values[1]).max(values[2]);
    if !(top > 0.0 && top.is_finite()) {
        return mean;
    }
    let mut x = mean;
    for k in 0..3 {
        if !(values[k] > QEF_RANK_CUT * top) {
            continue;
        }
        let vk = [vectors[0][k], vectors[1][k], vectors[2][k]];
        x = add(x, scale(vk, dot(vk, atb) / values[k]));
    }
    x
}

/// `x` clamped into `[lo, hi]`, then moved toward the zero set by at most two Newton steps, each
/// clamped and kept only when it lowers `|f|`: the vertex and its `|f|`.
fn settle(r: &Tape<'_>, x: D3, lo: D3, hi: D3, sc: &mut Scratch<'_>, stats: &mut MeshStats) -> (D3, f64) {
    let instrs = r.len() as u64;
    let clamp = |p: D3| [0usize, 1, 2].map(|k| p[k].max(lo[k]).min(hi[k]));
    let mut x = clamp(x);
    let (mut v, mut gr) = eval_grad(r, Vec3::new(x[0], x[1], x[2]), sc);
    stats.grads += 1;
    stats.work += instrs;
    for _ in 0..2 {
        let gg = gr.x * gr.x + gr.y * gr.y + gr.z * gr.z;
        if !(gg > 0.0 && gg.is_finite() && v.is_finite()) {
            break;
        }
        let y = clamp([x[0] - v * gr.x / gg, x[1] - v * gr.y / gg, x[2] - v * gr.z / gg]);
        let (w, g2) = eval_grad(r, Vec3::new(y[0], y[1], y[2]), sc);
        stats.grads += 1;
        stats.work += instrs;
        if !(w.abs() < v.abs()) {
            break;
        }
        (x, v, gr) = (y, w, g2);
    }
    (x, v.abs())
}

/// The points of the grid on the segment from `from` along axis `e`, `len` nm long, strictly
/// between its ends: the ends of the stopped cells in the four quadrants around it, ascending.
fn cuts_on(splits: &[Cell], g: &Grid, from: [i64; 3], e: usize, len: i64, cuts: &mut V<'_, i64>) -> Result<(), MeshError> {
    cuts.clear();
    let (u, w) = ((e + 1) % 3, (e + 2) % 3);
    let end = from[e] + len;
    for quad in 0..4 {
        let mut q = from;
        if quad & 1 == 1 {
            q[u] -= 1;
        }
        if quad & 2 == 2 {
            q[w] -= 1;
        }
        let mut x = from[e];
        loop {
            q[e] = x;
            let c = locate(splits, g, q);
            let next = (c.i[e] + 1) << c.shift;
            if next >= end {
                break;
            }
            push(cuts, next)?;
            x = next;
        }
    }
    cuts.sort_unstable();
    cuts.dedup();
    Ok(())
}

/// The border of the square at `lo` (side `2^j`, normal axis `k`), walked counter-clockwise about
/// `+k` with every grid point on it, appended to `walk` (the first corner once).
fn border(splits: &[Cell], g: &Grid, lo: [i64; 3], k: usize, j: u32, walk: &mut V<'_, [i64; 3]>, cuts: &mut V<'_, i64>) -> Result<(), MeshError> {
    let (a, b) = ((k + 1) % 3, (k + 2) % 3);
    let side = 1i64 << j;
    let mut c1 = lo;
    c1[a] += side;
    let mut c3 = lo;
    c3[b] += side;
    for (from, along, forward) in [(lo, a, true), (c1, b, true), (c3, a, false), (lo, b, false)] {
        cuts_on(splits, g, from, along, side, cuts)?;
        let mut first = from;
        if !forward {
            first[along] += side;
        }
        push(walk, first)?;
        let mut p = from;
        if forward {
            for &x in cuts.iter() {
                p[along] = x;
                push(walk, p)?;
            }
        } else {
            for &x in cuts.iter().rev() {
                p[along] = x;
                push(walk, p)?;
            }
        }
    }
    Ok(())
}

/// A facet of a cell: the square at `lo` of side `2^shift` on the cell's face of normal axis `k`
/// (its high face or its low one), and its border's span of the walk.
#[derive(Clone, Copy)]
struct Facet {
    k: usize,
    high: bool,
    lo: [i64; 3],
    shift: u32,
    start: usize,
    end: usize,
}

/// A grid segment of a border whose ends differ, low end first, and its ends' values.
#[derive(Clone, Copy)]
struct Seg {
    p: [i64; 3],
    q: [i64; 3],
    vp: f64,
    vq: f64,
}

/// One segment of a cell's cycles: crossing `from` to crossing `to` (indices of the piece's
/// crossings), through the facet point `mid` when it has one.
#[derive(Clone, Copy)]
struct Link {
    from: usize,
    to: usize,
    mid: Option<D3>,
}

/// The facet point of a facet whose one segment joins crossings `u` and `v` (module
/// documentation), when it has one.
fn facet_point(r: &Tape<'_>, f: &Facet, u: (D3, Option<D3>), v: (D3, Option<D3>), leaf_nm: i64, sc: &mut Scratch<'_>, stats: &mut MeshStats) -> Option<D3> {
    let (k, a, b) = (f.k, (f.k + 1) % 3, (f.k + 2) % 3);
    let (nu, nv) = (u.1?, v.1?);
    let flat = |n: D3| {
        let l = core::f64::math::sqrt(n[a] * n[a] + n[b] * n[b]);
        if l >= 0.25 { Some([n[a] / l, n[b] / l]) } else { None }
    };
    let (du, dv) = (flat(nu)?, flat(nv)?);
    let det = du[0] * dv[1] - du[1] * dv[0];
    if !(det.abs() >= 0.5) {
        return None;
    }
    let (pu, pv) = ([u.0[a], u.0[b]], [v.0[a], v.0[b]]);
    let (ru, rv) = (du[0] * pu[0] + du[1] * pu[1], dv[0] * pv[0] + dv[1] * pv[1]);
    let x = (ru * dv[1] - du[1] * rv) / det;
    let y = (du[0] * rv - dv[0] * ru) / det;
    let leaf = m(leaf_nm);
    let side = 1i64 << f.shift;
    let e = leaf_nm / 8;
    let (alo, ahi) = (m(f.lo[a] + e), m(f.lo[a] + side - e));
    let (blo, bhi) = (m(f.lo[b] + e), m(f.lo[b] + side - e));
    if !(x >= alo && x <= ahi && y >= blo && y <= bhi) {
        return None;
    }
    let ch = [pv[0] - pu[0], pv[1] - pu[1]];
    let cl = core::f64::math::sqrt(ch[0] * ch[0] + ch[1] * ch[1]);
    if !(cl > 0.0) || !(((x - pu[0]) * ch[1] - (y - pu[1]) * ch[0]).abs() / cl > leaf / 2.0) {
        return None;
    }
    let mut p = [0.0f64; 3];
    p[k] = m(f.lo[k]);
    p[a] = x;
    p[b] = y;
    let fv = eval_point(r, Vec3::new(p[0], p[1], p[2]), sc);
    stats.points += 1;
    stats.work += r.len() as u64;
    if fv.abs() <= leaf / 2.0 { Some(p) } else { None }
}

/// The triangles of a piece's stopped `cells` (module documentation, "Contouring") appended to
/// `soup`; the largest `|f|` at their centres.
#[allow(clippy::too_many_arguments)]
fn piece_triangles(
    r: &Tape<'_>,
    cells: &[Cell],
    splits: &[Cell],
    g: &Grid,
    soup: &mut V<'_, [P3; 3]>,
    sc: &mut Scratch<'_>,
    stats: &mut MeshStats,
    s: Alloc<'_>,
) -> Result<f64, MeshError> {
    let instrs = r.len() as u64;
    let leaf_nm = g.leaf_nm();

    // Each cell's facets and their borders.
    let mut facets: V<'_, Facet> = Vec::new_in(s);
    let mut walk: V<'_, [i64; 3]> = Vec::new_in(s);
    let mut ranges: V<'_, (usize, usize)> = Vec::try_with_capacity_in(cells.len(), s)?;
    let mut squares: V<'_, (i64, i64, u32)> = Vec::new_in(s);
    let mut cuts: V<'_, i64> = Vec::new_in(s);
    for c in cells.iter() {
        let first = facets.len();
        for face in 0..6usize {
            let (k, high) = (face / 2, face % 2 == 1);
            let (a, b) = ((k + 1) % 3, (k + 2) % 3);
            let w = (c.i[k] + i64::from(high)) << c.shift;
            squares.clear();
            push(&mut squares, (c.i[a] << c.shift, c.i[b] << c.shift, c.shift))?;
            while let Some((qa, qb, j)) = squares.pop() {
                let mut q = [0i64; 3];
                q[k] = if high { w } else { w - 1 };
                q[a] = qa;
                q[b] = qb;
                if locate(splits, g, q).shift < j {
                    let h = 1i64 << (j - 1);
                    for (da, db) in [(1i64, 1i64), (0, 1), (1, 0), (0, 0)] {
                        push(&mut squares, (qa + da * h, qb + db * h, j - 1))?;
                    }
                    continue;
                }
                let mut lo = [0i64; 3];
                lo[k] = w;
                lo[a] = qa;
                lo[b] = qb;
                let start = walk.len();
                border(splits, g, lo, k, j, &mut walk, &mut cuts)?;
                push(&mut facets, Facet { k, high, lo, shift: j, start, end: walk.len() })?;
            }
        }
        push(&mut ranges, (first, facets.len()))?;
    }

    // A value at every border point, each point once.
    let mut pts = copied(&walk, s)?;
    pts.sort_unstable();
    pts.dedup();
    let mut vals = filled(pts.len(), 0.0f64, s)?;
    for (n, p) in pts.iter().enumerate() {
        vals[n] = eval_point(r, at_nm(*p), sc);
    }
    stats.points += pts.len() as u64;
    stats.work += pts.len() as u64 * instrs;
    let mut wv = filled(walk.len(), 0.0f64, s)?;
    for (n, p) in walk.iter().enumerate() {
        match pts.binary_search(p) {
            Ok(i) => wv[n] = vals[i],
            Err(_) => return Err(MeshError::Unpaired { cell: [p[0], p[1], p[2]], shift: g.leaf }),
        }
    }

    // A crossing on every grid segment of a border whose ends differ, each segment once.
    let mut segs: V<'_, Seg> = Vec::new_in(s);
    for f in facets.iter() {
        let len = f.end - f.start;
        for n in 0..len {
            let (i, j) = (f.start + n, f.start + (n + 1) % len);
            if inside(wv[i]) == inside(wv[j]) {
                continue;
            }
            let seg = if walk[i] < walk[j] {
                Seg { p: walk[i], q: walk[j], vp: wv[i], vq: wv[j] }
            } else {
                Seg { p: walk[j], q: walk[i], vp: wv[j], vq: wv[i] }
            };
            push(&mut segs, seg)?;
        }
    }
    segs.sort_unstable_by(|x, y| (x.p, x.q).cmp(&(y.p, y.q)));
    segs.dedup_by(|x, y| x.p == y.p && x.q == y.q);
    let mut cross: V<'_, (D3, Option<D3>)> = Vec::try_with_capacity_in(segs.len(), s)?;
    for sg in segs.iter() {
        let e = (0..3).find(|&k| sg.p[k] != sg.q[k]).unwrap_or(0);
        let pos = crossing_on(r, sg.p, e, sg.q[e] - sg.p[e], sg.vp, sg.vq, leaf_nm, sc, stats);
        let (_, gr) = eval_grad(r, Vec3::new(pos[0], pos[1], pos[2]), sc);
        stats.grads += 1;
        stats.work += instrs;
        push(&mut cross, (pos, unit([gr.x, gr.y, gr.z])))?;
    }

    // Each cell's cycles and their fans.
    let mut links: V<'_, Link> = Vec::new_in(s);
    let mut marks: V<'_, (usize, bool)> = Vec::new_in(s);
    let mut poly: V<'_, D3> = Vec::new_in(s);
    let mut poly_n: V<'_, Option<D3>> = Vec::new_in(s);
    let mut cycles: V<'_, (usize, usize)> = Vec::new_in(s);
    let mut seen: V<'_, bool> = Vec::new_in(s);
    let mut centres: V<'_, P3> = Vec::new_in(s);
    let mut far = 0.0f64;
    for (ci, c) in cells.iter().enumerate() {
        let bug = MeshError::Unpaired { cell: c.i, shift: c.shift };
        links.clear();
        let (f0, f1) = ranges[ci];
        for f in &facets[f0..f1] {
            marks.clear();
            let len = f.end - f.start;
            for n in 0..len {
                let (i, j) = (f.start + n, f.start + (n + 1) % len);
                if inside(wv[i]) == inside(wv[j]) {
                    continue;
                }
                let key = if walk[i] < walk[j] { (walk[i], walk[j]) } else { (walk[j], walk[i]) };
                let id = segs.binary_search_by(|x| (x.p, x.q).cmp(&key)).map_err(|_| bug)?;
                // An ENTRY when the walk goes into the inside.
                push(&mut marks, (id, inside(wv[j])))?;
            }
            if marks.is_empty() {
                continue;
            }
            let count = marks.len();
            let Some(first) = marks.iter().position(|&(_, entry)| entry) else { return Err(bug) };
            if count % 2 != 0 {
                return Err(bug);
            }
            let pairs = count / 2;
            for n in 0..pairs {
                let (en, is_entry) = marks[(first + 2 * n) % count];
                let (ex, is_exit_entry) = marks[(first + 2 * n + 1) % count];
                if !is_entry || is_exit_entry {
                    return Err(bug);
                }
                let (from, to) = if f.high { (en, ex) } else { (ex, en) };
                // The two cells sharing the facet ask with the ends swapped: order them by
                // position, so both compute the same bits.
                let (u, v) = if cross[en].0 < cross[ex].0 { (cross[en], cross[ex]) } else { (cross[ex], cross[en]) };
                let mid = if pairs == 1 { facet_point(r, f, u, v, leaf_nm, sc, stats) } else { None };
                push(&mut links, Link { from, to, mid })?;
            }
        }
        if links.is_empty() {
            continue;
        }
        links.sort_unstable_by_key(|l| l.from);
        if links.windows(2).any(|w| w[0].from == w[1].from) {
            return Err(bug);
        }
        seen.clear();
        for _ in 0..links.len() {
            push(&mut seen, false)?;
        }
        poly.clear();
        poly_n.clear();
        cycles.clear();
        for start in 0..links.len() {
            if seen[start] {
                continue;
            }
            let p0 = poly.len();
            let mut at = start;
            loop {
                if seen[at] {
                    return Err(bug);
                }
                seen[at] = true;
                let l = links[at];
                push(&mut poly, cross[l.from].0)?;
                push(&mut poly_n, cross[l.from].1)?;
                if let Some(mp) = l.mid {
                    push(&mut poly, mp)?;
                    push(&mut poly_n, None)?;
                }
                at = links.binary_search_by_key(&l.to, |x| x.from).map_err(|_| bug)?;
                if at == start {
                    break;
                }
            }
            push(&mut cycles, (p0, poly.len()))?;
        }
        // A cycle of two points cuts nothing off.
        let live = cycles.iter().filter(|&&(p0, p1)| p1 - p0 >= 3).count();
        if live == 0 {
            continue;
        }
        let side = 1i64 << c.shift;
        let lo = [c.i[0] << c.shift, c.i[1] << c.shift, c.i[2] << c.shift];
        let (blo, bhi) = shrunk(lo, side, leaf_nm);
        centres.clear();
        for &(p0, p1) in cycles.iter() {
            if p1 - p0 < 3 {
                continue;
            }
            let ring = &poly[p0..p1];
            let x = if live == 1 { place(ring, &poly_n[p0..p1], mean_of(ring)) } else { mean_of(ring) };
            let (x, fx) = settle(r, x, blo, bhi, sc, stats);
            far = far.max(fx);
            let cx = narrow(x);
            if centres.iter().any(|q| *q == cx) {
                return Err(MeshError::CoincidentCentres { cell: c.i, shift: c.shift });
            }
            push(&mut centres, cx)?;
            for n in p0..p1 {
                let next = if n + 1 == p1 { p0 } else { n + 1 };
                push(soup, [cx, narrow(poly[n]), narrow(poly[next])])?;
            }
        }
    }
    Ok(far)
}

// ---- redistance --------------------------------------------------------------------------

#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct RedistanceOpts<'s> {
    /// Where the operand's tape, the samples and the crossings live.
    pub scratch: Alloc<'s>,
    /// The grid's spacing (the mesher's leaf is the natural choice).
    pub step: Length,
    /// How far from the surface the grid holds distances; beyond it, values are `±band`.
    pub band: Length,
    /// The most grid values.
    pub most_values: u64 = 1 << 24,
}

/// Solves the grid of a redistance node (`Graph::redistance` gave `r`) and attaches it, so the
/// node compiles.
///
/// The grid covers the operand's box grown by `band`, on the absolute `step` lattice (a far edit
/// does not move it). Each value is the distance from the grid point to the nearest crossing of
/// the operand's zero set with a grid edge (linear interpolation of the edge's values), signed by
/// the operand at the point, and clamped to `±band`: within `step · √3 / 2` of the true distance
/// where that is below `band` (the crossings sample the surface at the grid's spacing). Points
/// and sums in grid order; no hash order.
pub fn solve_redistance<P: Planted<CadPlant>>(g: &mut Graph<'_, P>, r: Solid<Exact>, o: RedistanceOpts<'_>) -> Result<(), MeshError> {
    solve_redistance_planted(g, r, o, NoPlant)
}

/// [`solve_redistance`] with a planted bug, for the tests' controls.
pub fn solve_redistance_planted<P: Planted<CadPlant>>(
    g: &mut Graph<'_, P>,
    r: Solid<Exact>,
    o: RedistanceOpts<'_>,
    plant: impl Planted<RedistancePlant>,
) -> Result<(), MeshError> {
    let s = o.scratch;
    let (step, band) = (o.step.as_nm(), o.band.as_nm());
    if step < 1 || band < 0 {
        return Err(MeshError::BadGrid { step: o.step, band: o.band });
    }
    let (_, local) = g.split_placement(r).map_err(MeshError::Graph)?;
    let inner = match g.node(local.id()) {
        Node::Redistance { inner } => *inner,
        _ => return Err(MeshError::NotRedistance),
    };
    let t = compile(g, inner, CompileOpts { alloc: s, .. }).map_err(MeshError::Compile)?;
    let b = g.aabb(inner);
    let (blo, bhi) = (b.lo.to_array(), b.hi.to_array());
    if b.is_empty() || !blo.iter().chain(bhi.iter()).all(|x| x.is_finite()) {
        return Err(MeshError::Unbounded);
    }
    let mut origin = [0i64; 3];
    let mut dims = [0u32; 3];
    for k in 0..3 {
        let (l, h) = (core::f64::math::floor(blo[k] * 1e9), core::f64::math::ceil(bhi[k] * 1e9));
        if !(l.abs() < 1e18 && h.abs() < 1e18) {
            return Err(MeshError::OutOfRange);
        }
        let lo = (l as i64 - band).div_euclid(step);
        let hi = (h as i64 + band).div_euclid(step) + 1;
        origin[k] = lo * step;
        let d = (hi - lo + 1).max(2);
        dims[k] = u32::try_from(d).map_err(|_| MeshError::TooManyGridValues { values: u64::MAX, most: o.most_values })?;
    }
    let n = u64::from(dims[0]) * u64::from(dims[1]) * u64::from(dims[2]);
    let most = o.most_values.min(u64::from(g.limits().most_grid_values));
    if n > most {
        return Err(MeshError::TooManyGridValues { values: n, most });
    }
    let (nx, ny, nz) = (blaze_int::idx(dims[0]), blaze_int::idx(dims[1]), blaze_int::idx(dims[2]));
    let point = |ix: usize, iy: usize, iz: usize| -> D3 {
        // Indices below 2^24, products in range: exact.
        [m(origin[0] + step * ix as i64), m(origin[1] + step * iy as i64), m(origin[2] + step * iz as i64)]
    };
    let mut sc = Scratch::new(s);
    let mut vals = filled(nx * ny * nz, 0.0f64, s)?;
    for iz in 0..nz {
        for iy in 0..ny {
            for ix in 0..nx {
                let p = point(ix, iy, iz);
                vals[ix + nx * (iy + ny * iz)] = eval_point(&t, Vec3::new(p[0], p[1], p[2]), &mut sc);
            }
        }
    }
    // The crossings, grouped by their edge's lower grid point (generated in that order).
    let mut starts = filled(nx * ny * nz + 1, 0u32, s)?;
    let mut crossings: V<'_, D3> = Vec::new_in(s);
    for iz in 0..nz {
        for iy in 0..ny {
            for ix in 0..nx {
                let here = ix + nx * (iy + ny * iz);
                // At most `most_values · 3` crossings, below 2^32 under the shipped caps.
                starts[here] = crossings.len() as u32;
                let pa = point(ix, iy, iz);
                let va = vals[here];
                for (axis, (ok, there)) in [(ix + 1 < nx, here + 1), (iy + 1 < ny, here + nx), (iz + 1 < nz, here + nx * ny)].into_iter().enumerate() {
                    if !ok || inside(va) == inside(vals[there]) {
                        continue;
                    }
                    let vb = vals[there];
                    let f = va / (va - vb);
                    let f = if f.is_nan() { 0.5 } else { f.max(0.0).min(1.0) };
                    let mut c = pa;
                    c[axis] = pa[axis] + f * m(step);
                    push(&mut crossings, c)?;
                }
            }
        }
    }
    starts[nx * ny * nz] = crossings.len() as u32;
    let reach = blaze_int::idx(u32::try_from(band / step + 2).unwrap_or(u32::MAX));
    let band_m = m(band);
    // Each grid point's least squared distance to a crossing whose edge's lower point is within
    // `reach` grid steps of it (every crossing within the band is), capped at the band's square.
    // Scattered from the crossings, not gathered at every point: the work is the crossings (a
    // surface) times one window, not every grid point (a volume) times one; the minimum over the
    // same set, so the same bits.
    let mut out = filled(nx * ny * nz, band_m * band_m, s)?;
    for qz in 0..nz {
        for qy in 0..ny {
            for qx in 0..nx {
                let q = qx + nx * (qy + ny * qz);
                let (a, z) = (blaze_int::idx(starts[q]), blaze_int::idx(starts[q + 1]));
                for c in &crossings[a..z] {
                    for iz in qz.saturating_sub(reach)..(qz + reach + 1).min(nz) {
                        for iy in qy.saturating_sub(reach)..(qy + reach + 1).min(ny) {
                            for ix in qx.saturating_sub(reach)..(qx + reach + 1).min(nx) {
                                let here = ix + nx * (iy + ny * iz);
                                let w = sub(*c, point(ix, iy, iz));
                                out[here] = out[here].min(dot(w, w));
                            }
                        }
                    }
                }
            }
        }
    }
    for (here, o) in out.iter_mut().enumerate() {
        let d = core::f64::math::sqrt(*o);
        *o = if inside(vals[here]) && !plant.has(RedistancePlant::Unsigned) { -d } else { d };
    }
    g.attach_grid(
        r,
        GridOpts { origin: [Length::nm(origin[0]), Length::nm(origin[1]), Length::nm(origin[2])], step: o.step, dims, values: &out, band: o.band, .. },
    )
    .map_err(MeshError::Graph)
}
