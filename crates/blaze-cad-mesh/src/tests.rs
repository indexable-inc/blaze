//! The crate's promises, each an oracle over drawn meshes, and the planted bugs each must catch:
//!
//! - the verifier accepts every generated closed mesh (subdivided boxes, icospheres, random
//!   convex hulls, boxes glued along a face with the shared faces removed) and rejects each
//!   planted defect by its name: a removed triangle (`OpenEdge`), a flipped one (`Inconsistent`),
//!   two boxes sharing one edge's vertex pair (`NonManifoldEdge`);
//! - mass properties equal the closed forms of boxes;
//! - welding and simplification are ORDER-FREE (a permuted, rotated, renumbered input gives
//!   byte-equal output); simplification's output is verified, meets its target, keeps every
//!   feature corner, is within its reported two-sided bound on sampled points, and changes the
//!   volume by no more than the bound's tube allows; an axis-aligned solid within a small error
//!   keeps at most four triangles per face plane however finely divided, and a fine cylinder, half
//!   tube or rounded box at most twice its sagitta count (the coarsest mesh within the target).
//!   The maps read a bounded number of corners per input triangle. Planted: `CrossCrease`
//!   (`CornerLost`/`NotVerified`), `TieByInsertion` (`OrderDependent`), `MovedOnly` and
//!   `MeasuresEverything` (`TooDense`);
//! - vertex-cache and fetch orders are permutations (equal order-free digests);
//! - clusters partition the triangles within their caps, with spheres that hold their vertices
//!   and cones that hold their normals. Planted: `OverCap` (`OverCap`);
//! - quantized positions and octahedral normals round-trip within their stated bounds. Planted:
//!   `TowardZero` (`QuantOutOfBound`);
//! - the hull holds every input point and is convex, and is order-free for points in general
//!   position. Planted: `SkipVertex` (`HullMissing`);
//! - the decomposition's pieces hold every vertex; levels of detail are verified, decreasing in
//!   size and increasing in switch distance.

use core::fmt;
use std::collections::BTreeMap;
use std::{format, string::String, vec, vec::Vec as StdVec};

use blaze_own::Alloc;
use blaze_sim::{Broken, Config, NoPlant, Planted, Tape, check, controls};

use crate::geom::{D3, P3, add, distance, dot, length, scale, sub, tri_normal, unit, wide};
use crate::*;

/// The test's allocator (D102): the process heap's handle, never `Global`.
fn heap() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

fn harness(e: impl fmt::Debug) -> Broken<MeshBroken> {
    Broken::Harness(format!("{e:?}"))
}

#[derive(Clone, PartialEq, Debug)]
enum MeshBroken {
    Rejected { what: &'static str, why: String },
    Accepted { defect: &'static str },
    WrongDefect { defect: &'static str, got: String },
    MassWrong { what: &'static str, got: f64, want: f64 },
    NotVerified { what: &'static str, why: String },
    OverTarget { got: u32, target: u32 },
    OverError { got: f64, target: f64 },
    CornerLost { at: P3 },
    DistanceOverBound { distance: f64, bound: f64 },
    VolumeOverBound { change: f64, bound: f64 },
    OrderDependent { what: &'static str },
    NotPermutation { what: &'static str },
    NotPartition { why: String },
    OverCap { cluster: usize, vertices: u32, triangles: u32 },
    BoundsWrong { cluster: usize, why: String },
    QuantOutOfBound { vertex: usize, axis: usize, error: f64, bound: f64 },
    OctOutOfBound { bits: u32, error: f64, bound: f64 },
    HullMissing { point: usize, height: f64, tolerance: f64 },
    HullWrong { why: String },
    PieceMissing { vertex: usize },
    LodWrong { why: String },
    WeldWrong { why: String },
    ShadeWrong { why: String },
    MeshDiffers { why: String },
    RecomputedWrong { recomputed: usize, changed: usize },
    FarVertex { vertex: usize, value: f64, bound: f64 },
    TooDense { what: &'static str, got: u64, bound: u64 },
    GridWrong { at: D3, got: f64, want: f64, bound: f64 },
    UvChartsOverlap { chart_a: usize, chart_b: usize, gap: f64, want: f64 },
    UvTangentWrong { vertex: usize, why: String },
    UvWrong { why: String },
}

impl fmt::Display for MeshBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

blaze_sim::plants! {
    impl DecimatePlant breaks MeshBroken {
        None,
        CrossCrease => MeshBroken::CornerLost { .. } | MeshBroken::NotVerified { .. },
        TieByInsertion => MeshBroken::OrderDependent { .. },
        MovedOnly => MeshBroken::TooDense { .. },
        MeasuresEverything => MeshBroken::TooDense { .. },
    }
}

blaze_sim::plants! {
    impl ClusterPlant breaks MeshBroken {
        None,
        OverCap => MeshBroken::OverCap { .. },
    }
}

blaze_sim::plants! {
    impl HullPlant breaks MeshBroken {
        None,
        SkipVertex => MeshBroken::HullMissing { .. },
    }
}

blaze_sim::plants! {
    impl ContourPlant breaks MeshBroken {
        None,
        KeyWithoutTolerance => MeshBroken::MeshDiffers { .. } | MeshBroken::RecomputedWrong { .. },
        NoResidualTest => MeshBroken::TooDense { .. },
    }
}

blaze_sim::plants! {
    impl RedistancePlant breaks MeshBroken {
        None,
        Unsigned => MeshBroken::GridWrong { .. },
    }
}

blaze_sim::plants! {
    impl QuantPlant breaks MeshBroken {
        None,
        TowardZero => MeshBroken::QuantOutOfBound { .. },
    }
}

blaze_sim::plants! {
    impl UvPlant breaks MeshBroken {
        None,
        OverlapCharts => MeshBroken::UvChartsOverlap { .. },
        SkipTangentOrtho => MeshBroken::UvTangentWrong { .. },
    }
}

// ---------------------------------------------------------------------------------------------
// Meshes the tests own.

#[derive(Clone, Debug, Default)]
struct Built {
    positions: StdVec<P3>,
    normals: StdVec<P3>,
    triangles: StdVec<[u32; 3]>,
    parts: StdVec<PartId>,
    roles: StdVec<MaterialRole>,
    creases: StdVec<VertexPair>,
}

impl Built {
    fn view(&self) -> Result<MeshView<'_>, Broken<MeshBroken>> {
        MeshView::new(Columns {
            positions: &self.positions,
            normals: &self.normals,
            tangents: &[],
            uvs: &[],
            colours: &[],
            triangles: &self.triangles,
            parts: &self.parts,
            roles: &self.roles,
            creases: &self.creases,
        })
        .map_err(harness)
    }

    fn of(view: &MeshView<'_>) -> Built {
        Built {
            positions: view.positions().to_vec(),
            normals: view.normals().to_vec(),
            triangles: view.triangles().to_vec(),
            parts: view.parts().to_vec(),
            roles: view.roles().to_vec(),
            creases: view.creases().to_vec(),
        }
    }
}

#[derive(Default)]
struct SoupBuf {
    corners: StdVec<[P3; 3]>,
    parts: StdVec<PartId>,
    roles: StdVec<MaterialRole>,
}

impl SoupBuf {
    fn push(&mut self, tri: [P3; 3], part: u32, role: u32) {
        self.corners.push(tri);
        self.parts.push(PartId(part));
        self.roles.push(MaterialRole(role));
    }

    fn weld(&self, twins: Twins) -> Result<Built, Broken<MeshBroken>> {
        let welded = weld(
            Soup { corners: &self.corners, parts: &self.parts, roles: &self.roles },
            WeldOpts { twins, limits: MeshLimits::SHIPPED, alloc: heap(), scratch: heap() },
        )
        .map_err(harness)?;
        Ok(Built::of(&welded.mesh.view()))
    }
}

fn f32s(p: D3) -> P3 {
    [p[0] as f32, p[1] as f32, p[2] as f32]
}

/// A box's six faces as an `n × n` grid each, diagonals alternating, outward; the cells of the
/// top face (`+z`) with `i < n / 2` get role 1 when `split_top`.
struct BoxSpec {
    lo: D3,
    hi: D3,
    n: u32,
    part: u32,
    split_top: bool,
}

fn box_soup(soup: &mut SoupBuf, spec: &BoxSpec) {
    let BoxSpec { lo, hi, n, part, split_top } = *spec;
    let coord = |axis: usize, i: u32| -> f64 {
        if i == 0 {
            lo[axis]
        } else if i == n {
            hi[axis]
        } else {
            lo[axis] + (hi[axis] - lo[axis]) * f64::from(i) / f64::from(n)
        }
    };
    for a in 0..3 {
        for side in 0..2 {
            let (u, v) = ((a + 1) % 3, (a + 2) % 3);
            let plane = if side == 0 { lo[a] } else { hi[a] };
            let point = |i: u32, j: u32| -> P3 {
                let mut p = [0.0f64; 3];
                p[a] = plane;
                p[u] = coord(u, i);
                p[v] = coord(v, j);
                f32s(p)
            };
            for i in 0..n {
                for j in 0..n {
                    let (p00, p10, p01, p11) = (point(i, j), point(i + 1, j), point(i, j + 1), point(i + 1, j + 1));
                    let tris = if (i + j) % 2 == 0 { [[p00, p10, p11], [p00, p11, p01]] } else { [[p00, p10, p01], [p10, p11, p01]] };
                    let role = if split_top && a == 2 && side == 1 && 2 * i < n { 1 } else { 0 };
                    for t in tris {
                        let t = if side == 1 { t } else { [t[0], t[2], t[1]] };
                        soup.push(t, part, role);
                    }
                }
            }
        }
    }
}

/// Crease marks on every side whose ends are on one edge of the box `lo..hi`.
fn mark_box_edges(mesh: &mut Built, lo: D3, hi: D3) {
    let at_bound = |p: P3, axis: usize| -> Option<f32> {
        let x = f64::from(p[axis]);
        if x == lo[axis] || x == hi[axis] { Some(p[axis]) } else { None }
    };
    let mut marks = StdVec::new();
    for tri in &mesh.triangles {
        for k in 0..3 {
            let (a, b) = (tri[k], tri[(k + 1) % 3]);
            let (p, q) = (mesh.positions[a as usize], mesh.positions[b as usize]);
            let shared = (0..3).filter(|&axis| at_bound(p, axis).is_some() && at_bound(p, axis) == at_bound(q, axis)).count();
            if shared >= 2 {
                marks.push(VertexPair::new(VertexId(a), VertexId(b)));
            }
        }
    }
    marks.sort_unstable();
    marks.dedup();
    mesh.creases = marks;
}

fn cube(tape: &Tape) -> Result<(Built, f64), Broken<MeshBroken>> {
    let n = tape.int_in(1..=5u32, "cube grid");
    let h = *tape.pick(&[0.5f64, 1.0, 3.0], "cube half side").unwrap_or(&1.0);
    let split_top = n >= 2 && tape.coin("split top face's role");
    let (lo, hi) = ([-h; 3], [h; 3]);
    let mut soup = SoupBuf::default();
    box_soup(&mut soup, &BoxSpec { lo, hi, n, part: 0, split_top });
    let mut mesh = soup.weld(Twins::Keep)?;
    if tape.coin("mark the box's edges") {
        mark_box_edges(&mut mesh, lo, hi);
    }
    Ok((mesh, 2.0 * h))
}

fn icosphere(tape: &Tape) -> Result<(Built, f64), Broken<MeshBroken>> {
    let level = tape.int_in(0..=2u32, "icosphere level");
    let radius = *tape.pick(&[1.0f64, 2.5], "radius").unwrap_or(&1.0);
    let t = (1.0 + 5.0f64.sqrt()) / 2.0;
    let v: [D3; 12] = [
        [-1.0, t, 0.0],
        [1.0, t, 0.0],
        [-1.0, -t, 0.0],
        [1.0, -t, 0.0],
        [0.0, -1.0, t],
        [0.0, 1.0, t],
        [0.0, -1.0, -t],
        [0.0, 1.0, -t],
        [t, 0.0, -1.0],
        [t, 0.0, 1.0],
        [-t, 0.0, -1.0],
        [-t, 0.0, 1.0],
    ];
    let faces: [[usize; 3]; 20] = [
        [0, 11, 5],
        [0, 5, 1],
        [0, 1, 7],
        [0, 7, 10],
        [0, 10, 11],
        [1, 5, 9],
        [5, 11, 4],
        [11, 10, 2],
        [10, 7, 6],
        [7, 1, 8],
        [3, 9, 4],
        [3, 4, 2],
        [3, 2, 6],
        [3, 6, 8],
        [3, 8, 9],
        [4, 9, 5],
        [2, 4, 11],
        [6, 2, 10],
        [8, 6, 7],
        [9, 8, 1],
    ];
    let norm = |p: D3| scale(p, 1.0 / length(p));
    let mut tris: StdVec<[D3; 3]> = faces.iter().map(|f| [norm(v[f[0]]), norm(v[f[1]]), norm(v[f[2]])]).collect();
    for _ in 0..level {
        let mut next = StdVec::new();
        for [a, b, c] in tris {
            let (ab, bc, ca) = (norm(add(a, b)), norm(add(b, c)), norm(add(c, a)));
            next.extend([[a, ab, ca], [b, bc, ab], [c, ca, bc], [ab, bc, ca]]);
        }
        tris = next;
    }
    let mut soup = SoupBuf::default();
    for [a, b, c] in tris {
        let tri = if dot(tri_normal(a, b, c), add(add(a, b), c)) > 0.0 { [a, b, c] } else { [a, c, b] };
        soup.push([f32s(scale(tri[0], radius)), f32s(scale(tri[1], radius)), f32s(scale(tri[2], radius))], 0, 0);
    }
    Ok((soup.weld(Twins::Keep)?, 2.0 * radius))
}

/// `count` points on the sphere of `radius`, from directions drawn in the cube and normalised.
fn sphere_points(tape: &Tape, count: usize, radius: f64) -> StdVec<P3> {
    let mut out = StdVec::new();
    while out.len() < count {
        let d = [tape.f64_in(-1.0..=1.0, "x"), tape.f64_in(-1.0..=1.0, "y"), tape.f64_in(-1.0..=1.0, "z")];
        let l = length(d);
        if l > 0.1 && l <= 1.0 {
            out.push(f32s(scale(d, radius / l)));
        }
    }
    out
}

fn hull_mesh(points: &[P3]) -> Result<Built, Broken<MeshBroken>> {
    let hull = convex_hull(points, HullOpts { limits: MeshLimits::SHIPPED, alloc: heap(), scratch: heap() }).map_err(harness)?;
    let mut mesh = Built {
        positions: hull.points.to_vec(),
        normals: vec![[0.0; 3]; hull.points.len()],
        triangles: hull.triangles.to_vec(),
        parts: vec![PartId(0); hull.triangles.len()],
        roles: vec![MaterialRole(0); hull.triangles.len()],
        creases: StdVec::new(),
    };
    let normals = vertex_normals(&mesh.view()?, heap()).map_err(harness)?;
    mesh.normals = normals.to_vec();
    Ok(mesh)
}

fn random_hull(tape: &Tape) -> Result<(Built, f64), Broken<MeshBroken>> {
    let count = tape.int_in(8..=40usize, "hull points");
    let points = sphere_points(tape, count, 1.0);
    Ok((hull_mesh(&points)?, 2.0))
}

/// Two boxes glued along `x = 2`, parts 0 and 1, the shared faces removed as twins.
fn glued(tape: &Tape) -> Result<(Built, f64), Broken<MeshBroken>> {
    let n = tape.int_in(1..=4u32, "glued grid");
    let mut soup = SoupBuf::default();
    box_soup(&mut soup, &BoxSpec { lo: [0.0, -1.0, -1.0], hi: [2.0, 1.0, 1.0], n, part: 0, split_top: false });
    box_soup(&mut soup, &BoxSpec { lo: [2.0, -1.0, -1.0], hi: [4.0, 1.0, 1.0], n, part: 1, split_top: false });
    let mut mesh = soup.weld(Twins::Remove)?;
    if tape.coin("mark the glued box's edges") {
        mark_box_edges(&mut mesh, [0.0, -1.0, -1.0], [4.0, 1.0, 1.0]);
    }
    Ok((mesh, 4.0))
}

fn solid(tape: &Tape) -> Result<(Built, f64), Broken<MeshBroken>> {
    match tape.below(4, "shape") {
        0 => cube(tape),
        1 => icosphere(tape),
        2 => glued(tape),
        _ => random_hull(tape),
    }
}

/// `mesh` with its vertices renumbered, its triangles reordered and some turned, each a list of
/// single moves drawn one by one: vertex swaps, triangle swaps, and turns named by the ORIGINAL
/// triangle. A failing case then shrinks by deleting moves until only the ones that matter are
/// left. (A whole `shuffle` is always `n − 1` draws, whatever order it gives, so the tape can
/// never get shorter; and a turn drawn per output position lands on another triangle whenever
/// an earlier shuffle draw changes, so no draw can be zeroed alone without moving the failure.)
fn permuted(tape: &Tape, mesh: &Built) -> Built {
    let n = mesh.positions.len();
    let t_count = mesh.triangles.len();
    let mut new_of_old: StdVec<u32> = (0..n as u32).collect();
    for (i, j) in tape.vec(0..=2 * n, "vertex swaps", |tp| (tp.index(n, "vertex"), tp.index(n, "swapped with vertex"))) {
        new_of_old.swap(i, j);
    }
    let mut order: StdVec<usize> = (0..t_count).collect();
    for (i, j) in tape.vec(0..=2 * t_count, "triangle swaps", |tp| (tp.index(t_count, "triangle"), tp.index(t_count, "swapped with triangle"))) {
        order.swap(i, j);
    }
    let mut turns = vec![0usize; t_count];
    for (t, k) in tape.vec(0..=t_count, "turns", |tp| (tp.index(t_count, "turned triangle"), 1 + tp.index(2, "turned by"))) {
        turns[t] = k;
    }
    let mut out = Built { positions: vec![[0.0; 3]; n], normals: vec![[0.0; 3]; n], ..Built::default() };
    for old in 0..n {
        out.positions[new_of_old[old] as usize] = mesh.positions[old];
        out.normals[new_of_old[old] as usize] = mesh.normals[old];
    }
    for t in order {
        let tri = mesh.triangles[t].map(|v| new_of_old[v as usize]);
        let turn = turns[t];
        out.triangles.push([tri[turn], tri[(turn + 1) % 3], tri[(turn + 2) % 3]]);
        out.parts.push(mesh.parts[t]);
        out.roles.push(mesh.roles[t]);
    }
    out.creases = mesh.creases.iter().map(|e| VertexPair::new(VertexId(new_of_old[e.lo().index()]), VertexId(new_of_old[e.hi().index()]))).collect();
    out.creases.sort_unstable();
    out
}

/// The crate's verifier (`check` alone names blaze-sim's runner here).
fn verify<'v>(view: MeshView<'v>) -> Result<Verified<'v>, Defect> {
    crate::check::check(view, CheckOpts { limits: MeshLimits::SHIPPED, scratch: heap() })
}

type Key = [u32; 3];

fn key(p: P3) -> Key {
    let z = |x: f32| if x == 0.0 { 0.0f32 } else { x };
    [z(p[0]).to_bits(), z(p[1]).to_bits(), z(p[2]).to_bits()]
}

/// The positions where the features meet other than as a chain: crease marks, part and role
/// borders and open edges counted per position, every count but 0 and 2.
fn corners(mesh: &Built) -> StdVec<P3> {
    let mut edges: BTreeMap<(Key, Key), StdVec<usize>> = BTreeMap::new();
    for (t, tri) in mesh.triangles.iter().enumerate() {
        for k in 0..3 {
            let (a, b) = (key(mesh.positions[tri[k] as usize]), key(mesh.positions[tri[(k + 1) % 3] as usize]));
            edges.entry(if a < b { (a, b) } else { (b, a) }).or_default().push(t);
        }
    }
    let mut marked = std::collections::BTreeSet::new();
    for e in &mesh.creases {
        let (a, b) = (key(mesh.positions[e.lo().index()]), key(mesh.positions[e.hi().index()]));
        marked.insert(if a < b { (a, b) } else { (b, a) });
    }
    let mut degree: BTreeMap<Key, u32> = BTreeMap::new();
    for (edge, tris) in &edges {
        let feature = tris.len() != 2
            || marked.contains(edge)
            || mesh.roles[tris[0]] != mesh.roles[tris[1]]
            || mesh.parts[tris[0]] != mesh.parts[tris[1]];
        if feature {
            *degree.entry(edge.0).or_default() += 1;
            *degree.entry(edge.1).or_default() += 1;
        }
    }
    degree.into_iter().filter(|&(_, d)| d != 2).map(|(k, _)| [f32::from_bits(k[0]), f32::from_bits(k[1]), f32::from_bits(k[2])]).collect()
}

/// The closest point of triangle `abc` to `p` (C. Ericson, Real-Time Collision Detection, 5.1.5).
fn closest(p: D3, tri: [D3; 3]) -> D3 {
    let [a, b, c] = tri;
    let (ab, ac, ap) = (sub(b, a), sub(c, a), sub(p, a));
    let (d1, d2) = (dot(ab, ap), dot(ac, ap));
    if d1 <= 0.0 && d2 <= 0.0 {
        return a;
    }
    let bp = sub(p, b);
    let (d3, d4) = (dot(ab, bp), dot(ac, bp));
    if d3 >= 0.0 && d4 <= d3 {
        return b;
    }
    let vc = d1 * d4 - d3 * d2;
    if vc <= 0.0 && d1 >= 0.0 && d3 <= 0.0 {
        return add(a, scale(ab, d1 / (d1 - d3)));
    }
    let cp = sub(p, c);
    let (d5, d6) = (dot(ab, cp), dot(ac, cp));
    if d6 >= 0.0 && d5 <= d6 {
        return c;
    }
    let vb = d5 * d2 - d1 * d6;
    if vb <= 0.0 && d2 >= 0.0 && d6 <= 0.0 {
        return add(a, scale(ac, d2 / (d2 - d6)));
    }
    let va = d3 * d6 - d5 * d4;
    if va <= 0.0 && (d4 - d3) >= 0.0 && (d5 - d6) >= 0.0 {
        return add(b, scale(sub(c, b), (d4 - d3) / ((d4 - d3) + (d5 - d6))));
    }
    let denom = 1.0 / (va + vb + vc);
    add(a, add(scale(ab, vb * denom), scale(ac, vc * denom)))
}

fn corners_of(mesh: &Built, t: usize) -> [D3; 3] {
    mesh.triangles[t].map(|v| wide(mesh.positions[v as usize]))
}

fn distance_to(mesh: &Built, p: D3) -> f64 {
    (0..mesh.triangles.len()).map(|t| distance(p, closest(p, corners_of(mesh, t)))).fold(f64::INFINITY, f64::min)
}

fn sample(tape: &Tape, mesh: &Built) -> D3 {
    let t = tape.index(mesh.triangles.len(), "sample triangle");
    let (mut u, mut v) = (tape.unit_f64("u"), tape.unit_f64("v"));
    if u + v > 1.0 {
        (u, v) = (1.0 - u, 1.0 - v);
    }
    let [a, b, c] = corners_of(mesh, t);
    add(a, add(scale(sub(b, a), u), scale(sub(c, a), v)))
}

// ---------------------------------------------------------------------------------------------
// The verifier and mass properties.

#[test]
fn verifier_accepts_generated_solids() {
    check(Config::cases(400), |tape| {
        let (mesh, _) = solid(tape)?;
        verify(mesh.view()?).map_err(|e| Broken::Oracle(MeshBroken::Rejected { what: "generated solid", why: format!("{e:?}") }))?;
        Ok::<(), Broken<MeshBroken>>(())
    });
}

#[test]
fn verifier_names_planted_defects() {
    check(Config::cases(400), |tape| {
        let (mesh, _) = solid(tape)?;
        let defect = tape.below(3, "defect");
        let mut bad = mesh.clone();
        let (name, want): (&'static str, fn(&Defect) -> bool) = match defect {
            0 => {
                let t = tape.index(bad.triangles.len(), "removed");
                bad.triangles.remove(t);
                bad.parts.remove(t);
                bad.roles.remove(t);
                ("removed triangle", |e| matches!(e, Defect::OpenEdge { .. }))
            }
            1 => {
                let t = tape.index(bad.triangles.len(), "flipped");
                bad.triangles[t].swap(1, 2);
                ("flipped triangle", |e| matches!(e, Defect::Inconsistent { .. }))
            }
            _ => {
                // A second box placed so one of its edges lies on one of the first's. It is three
                // times as long in `x`: two equal boxes are point-symmetric about the shared edge,
                // so at a vertex inside it their area vectors cancel exactly and `weld` refuses,
                // rightly, to give it a normal (`NoNormal`) before the verifier ever sees the
                // defect. Here the `y` parts are `(k_a − 3 k_b) / 2n²` with `k_a, k_b` in `1..=2`
                // incident triangles, never zero, and the corners keep their `z` part.
                let mut soup = SoupBuf::default();
                let n = tape.int_in(1..=3u32, "edge-sharing grid");
                box_soup(&mut soup, &BoxSpec { lo: [0.0; 3], hi: [1.0; 3], n, part: 0, split_top: false });
                box_soup(&mut soup, &BoxSpec { lo: [1.0, 1.0, 0.0], hi: [4.0, 2.0, 1.0], n, part: 0, split_top: false });
                bad = soup.weld(Twins::Keep)?;
                ("vertex pair shared by two boxes", |e| matches!(e, Defect::NonManifoldEdge { count: 4, .. }))
            }
        };
        match verify(bad.view()?) {
            Ok(_) => Err(Broken::Oracle(MeshBroken::Accepted { defect: name })),
            Err(e) if want(&e) => Ok(()),
            Err(e) => Err(Broken::Oracle(MeshBroken::WrongDefect { defect: name, got: format!("{e:?}") })),
        }
    });
}

#[test]
fn mass_properties_of_boxes() {
    check(Config::cases(200), |tape| {
        let glue = tape.coin("glued");
        let (mesh, (volume, centroid, inertia)) = if glue {
            let (mesh, _) = glued(tape)?;
            // A 4 × 2 × 2 box: V = 16, centre (2, 0, 0), I = V (b² + c²) / 12 per axis.
            (mesh, (16.0, [2.0, 0.0, 0.0], [16.0 * 8.0 / 12.0, 16.0 * 20.0 / 12.0, 16.0 * 20.0 / 12.0]))
        } else {
            let (mesh, side) = cube(tape)?;
            let v = side * side * side;
            (mesh, (v, [0.0; 3], [v * side * side / 6.0; 3]))
        };
        let view = mesh.view()?;
        let proof = verify(view).map_err(harness)?;
        let m = mass_properties(&proof);
        let near = |got: f64, want: f64, scale_: f64| (got - want).abs() <= 1e-9 * scale_;
        if !near(m.volume.as_m3(), volume, volume) {
            return Err(Broken::Oracle(MeshBroken::MassWrong { what: "volume", got: m.volume.as_m3(), want: volume }));
        }
        for a in 0..3 {
            if !near(m.centroid[a], centroid[a], 4.0) {
                return Err(Broken::Oracle(MeshBroken::MassWrong { what: "centroid", got: m.centroid[a], want: centroid[a] }));
            }
            if !near(m.inertia[a][a], inertia[a], inertia[a]) {
                return Err(Broken::Oracle(MeshBroken::MassWrong { what: "inertia diagonal", got: m.inertia[a][a], want: inertia[a] }));
            }
            let off = m.inertia[a][(a + 1) % 3];
            if !near(off, 0.0, inertia[a]) {
                return Err(Broken::Oracle(MeshBroken::MassWrong { what: "product of inertia", got: off, want: 0.0 }));
            }
        }
        Ok(())
    });
}

// ---------------------------------------------------------------------------------------------
// Welding, shading, digests.

#[test]
fn weld_is_order_free_and_exact() {
    check(Config::cases(300), |tape| {
        let n = tape.int_in(1..=4u32, "grid");
        let mut soup = SoupBuf::default();
        box_soup(&mut soup, &BoxSpec { lo: [-1.0; 3], hi: [1.0; 3], n, part: 0, split_top: false });
        let first = soup.weld(Twins::Keep)?;
        // `6n² + 2` vertices on a subdivided box surface (Euler: V − E + F = 2, F = 12n²).
        let euler = 6 * (n * n) as usize + 2;
        if first.positions.len() != euler || first.triangles.len() != 12 * (n * n) as usize {
            return Err(Broken::Oracle(MeshBroken::WeldWrong {
                why: format!("{} vertices and {} triangles, want {euler} and {}", first.positions.len(), first.triangles.len(), 12 * n * n),
            }));
        }
        let mut order: StdVec<usize> = (0..soup.corners.len()).collect();
        tape.shuffle(&mut order, "soup order");
        let mut shuffled = SoupBuf::default();
        for t in order {
            let c = soup.corners[t];
            let turn = tape.below(3, "turn") as usize;
            shuffled.push([c[turn], c[(turn + 1) % 3], c[(turn + 2) % 3]], soup.parts[t].0, soup.roles[t].0);
        }
        // A duplicate and a degenerate triangle are dropped and counted.
        shuffled.push(shuffled.corners[0], 0, 0);
        let p = shuffled.corners[0][0];
        shuffled.push([p, p, shuffled.corners[0][1]], 0, 0);
        let welded = weld(
            Soup { corners: &shuffled.corners, parts: &shuffled.parts, roles: &shuffled.roles },
            WeldOpts { twins: Twins::Keep, limits: MeshLimits::SHIPPED, alloc: heap(), scratch: heap() },
        )
        .map_err(harness)?;
        if welded.duplicates != 1 || welded.degenerate != 1 {
            return Err(Broken::Oracle(MeshBroken::WeldWrong { why: format!("{} duplicates, {} degenerate", welded.duplicates, welded.degenerate) }));
        }
        if MeshDigest::of(&welded.mesh.view()) != MeshDigest::of(&first.view()?) {
            return Err(Broken::Oracle(MeshBroken::OrderDependent { what: "weld" }));
        }
        Ok(())
    });
}

#[test]
fn shade_splits_box_normals_at_creases() {
    check(Config::cases(200), |tape| {
        let n = tape.int_in(1..=4u32, "grid");
        let mut soup = SoupBuf::default();
        box_soup(&mut soup, &BoxSpec { lo: [-1.0; 3], hi: [1.0; 3], n, part: 0, split_top: false });
        let mesh = soup.weld(Twins::Keep)?;
        let shaded = shade(&mesh.view()?, ShadeOpts { cos_crease: COS_32_DEGREES, alloc: heap(), scratch: heap() }).map_err(harness)?;
        let view = shaded.view();
        let want = 6 * ((n + 1) * (n + 1)) as usize;
        if view.positions().len() != want {
            return Err(Broken::Oracle(MeshBroken::ShadeWrong { why: format!("{} vertices, want {want}", view.positions().len()) }));
        }
        for nrm in view.normals() {
            let axis = nrm.iter().filter(|x| x.abs() == 1.0).count() == 1 && nrm.iter().filter(|&&x| x == 0.0).count() == 2;
            if !axis {
                return Err(Broken::Oracle(MeshBroken::ShadeWrong { why: format!("normal {nrm:?} is not an axis") }));
            }
        }
        verify(view).map_err(|e| Broken::Oracle(MeshBroken::NotVerified { what: "shaded box", why: format!("{e:?}") }))?;
        Ok(())
    });
}

#[test]
fn unordered_digest_ignores_order() {
    check(Config::cases(300), |tape| {
        let (mesh, _) = solid(tape)?;
        let moved = permuted(tape, &mesh);
        let (a, b) = (
            MeshDigest::of_unordered(&mesh.view()?, heap()).map_err(harness)?,
            MeshDigest::of_unordered(&moved.view()?, heap()).map_err(harness)?,
        );
        if a != b {
            return Err(Broken::Oracle(MeshBroken::OrderDependent { what: "unordered digest" }));
        }
        let canon_a = canonical(&mesh.view()?, CanonOpts { alloc: heap(), scratch: heap() }).map_err(harness)?;
        let canon_b = canonical(&moved.view()?, CanonOpts { alloc: heap(), scratch: heap() }).map_err(harness)?;
        if MeshDigest::of(&canon_a.view()) != MeshDigest::of(&canon_b.view()) {
            return Err(Broken::Oracle(MeshBroken::OrderDependent { what: "canonical form" }));
        }
        Ok(())
    });
}

// ---------------------------------------------------------------------------------------------
// Simplification.

/// `source` simplified to `target`; an output the verifier refuses is the oracle's finding.
fn simplified(
    source: Verified<'_>,
    target: Target,
    plant: impl Planted<DecimatePlant>,
) -> Result<Decimated<'static>, Broken<MeshBroken>> {
    match decimate_planted(
        Source::Verified(source),
        DecimateOpts { target, cos_crease: COS_32_DEGREES, limits: MeshLimits::SHIPPED, alloc: heap(), scratch: heap(), .. },
        plant,
    ) {
        Ok(out) => Ok(out),
        Err(DecimateError::LostProof(e)) => {
            Err(Broken::Oracle(MeshBroken::NotVerified { what: "simplified", why: format!("{e:?}") }))
        }
        Err(e) => Err(harness(e)),
    }
}

fn simplify_case(tape: &Tape, plant: impl Planted<DecimatePlant> + Copy) -> Result<(), Broken<MeshBroken>> {
    let (input, size) = solid(tape)?;
    let view = input.view()?;
    let proof = verify(view).map_err(harness)?;
    let t_in = view.triangle_count();
    let target = if tape.coin("target by count") {
        Target::Triangles(tape.int_in(4..=t_in, "target triangles"))
    } else {
        let scale_ = *tape.pick(&[1e-5f64, 1e-3, 0.1], "target error scale").unwrap_or(&0.1);
        Target::Error(crate::geom::length_up(size * scale_ * tape.f64_in(0.0..=1.0, "target error")))
    };
    let out = simplified(proof, target, plant)?;
    if out.verified().is_none() {
        return Err(Broken::Oracle(MeshBroken::NotVerified { what: "simplified", why: String::from("no proof") }));
    }
    let output = Built::of(&out.mesh.view());
    let bound = out.error_bound.as_m();
    // Flat faces collapse: an axis-aligned solid of one part and role, simplified within a
    // small error (far below any corner's cut, far above rounding), keeps at most four
    // triangles per plane its faces lie in, however finely it was divided.
    if let Target::Error(most) = target
        && (1e-6 * size..=1e-3 * size).contains(&most.as_m())
        && let Some(planes) = axis_planes(&input)
    {
        let got = u64::from(out.mesh.view().triangle_count());
        let most_tris = 4 * planes;
        if got > most_tris {
            return Err(Broken::Oracle(MeshBroken::TooDense { what: "flat faces after simplification", got, bound: most_tris }));
        }
    }
    match target {
        Target::Triangles(most) => {
            let got = out.mesh.view().triangle_count();
            if out.stop == Stop::Reached && got > most {
                return Err(Broken::Oracle(MeshBroken::OverTarget { got, target: most }));
            }
        }
        Target::Error(most) => {
            // Both rounded up to the nanometre from f64 metres: the bound may exceed the target
            // by the one nanometre of the target's own rounding.
            if out.error_bound > most + blaze_units::Length::nm(1) {
                return Err(Broken::Oracle(MeshBroken::OverError { got: bound, target: most.as_m() }));
            }
        }
        Target::Budget { max_triangles, max_error, .. } => {
            let got = out.mesh.view().triangle_count();
            if out.stop == Stop::Reached && got > max_triangles {
                return Err(Broken::Oracle(MeshBroken::OverTarget { got, target: max_triangles }));
            }
            if let Some(ce) = max_error
                && out.error_bound > ce + blaze_units::Length::nm(1)
            {
                return Err(Broken::Oracle(MeshBroken::OverError { got: bound, target: ce.as_m() }));
            }
        }
    }
    // Every feature corner is still a vertex, where it was.
    let have: std::collections::BTreeSet<Key> = output.positions.iter().map(|&p| key(p)).collect();
    for c in corners(&input) {
        if !have.contains(&key(c)) {
            return Err(Broken::Oracle(MeshBroken::CornerLost { at: c }));
        }
    }
    // The two-sided distance, sampled both ways.
    let slack = 1e-6 * size;
    for _ in 0..8 {
        for (from, to) in [(&input, &output), (&output, &input)] {
            let p = sample(tape, from);
            let d = distance_to(to, p);
            if d > bound + slack {
                return Err(Broken::Oracle(MeshBroken::DistanceOverBound { distance: d, bound }));
            }
        }
    }
    // The volume moves by at most the bound's tube around the input (Steiner's formula for each
    // triangle, summed).
    let before = mass_properties(&proof).volume.as_m3();
    let after = mass_properties(&out.verified().ok_or_else(|| harness("no proof"))?).volume.as_m3();
    let (mut area_sum, mut perimeter) = (0.0, 0.0);
    for t in 0..input.triangles.len() {
        let [a, b, c] = corners_of(&input, t);
        area_sum += length(tri_normal(a, b, c)) * 0.5;
        perimeter += distance(a, b) + distance(b, c) + distance(c, a);
    }
    let tube = 2.0 * bound * area_sum
        + core::f64::consts::FRAC_PI_2 * bound * bound * perimeter
        + 4.0 / 3.0 * core::f64::consts::PI * bound * bound * bound * input.triangles.len() as f64
        + 1e-9 * size * size * size;
    if (after - before).abs() > tube {
        return Err(Broken::Oracle(MeshBroken::VolumeOverBound { change: after - before, bound: tube }));
    }
    // Order-free: the same mesh renumbered, reordered and turned gives the same bytes.
    let moved = permuted(tape, &input);
    let moved_proof = verify(moved.view()?).map_err(harness)?;
    let again = simplified(moved_proof, target, plant)?;
    if MeshDigest::of(&again.mesh.view()) != MeshDigest::of(&out.mesh.view()) {
        return Err(Broken::Oracle(MeshBroken::OrderDependent { what: "simplify" }));
    }
    Ok(())
}

/// The number of distinct planes the faces of `mesh` lie in, when every triangle is axis-aligned
/// and all are of one part and one role; `None` otherwise.
fn axis_planes(mesh: &Built) -> Option<u64> {
    if mesh.parts.iter().any(|&p| p != mesh.parts[0]) || mesh.roles.iter().any(|&r| r != mesh.roles[0]) {
        return None;
    }
    let mut planes = std::collections::BTreeSet::new();
    for t in 0..mesh.triangles.len() {
        let [a, b, c] = corners_of(mesh, t);
        let n = tri_normal(a, b, c);
        let axes: StdVec<usize> = (0..3).filter(|&k| n[k] != 0.0).collect();
        let &[k] = axes.as_slice() else {
            return None;
        };
        planes.insert((k, n[k] > 0.0, a[k].to_bits()));
    }
    Some(planes.len() as u64)
}

/// A closed cylinder of radius `r` and length `l` along `z`: `m` segments around, `k` rings of
/// quads along, the ends fans from their centres.
fn cylinder_mesh(r: f64, l: f64, m: u32, k: u32) -> Result<Built, Broken<MeshBroken>> {
    let ring = |i: u32, j: u32| -> D3 {
        let angle = core::f64::consts::TAU * f64::from(i % m) / f64::from(m);
        [r * angle.cos(), r * angle.sin(), l * f64::from(j) / f64::from(k)]
    };
    let mut soup = SoupBuf::default();
    for i in 0..m {
        for j in 0..k {
            let (p00, p10, p01, p11) = (ring(i, j), ring(i + 1, j), ring(i, j + 1), ring(i + 1, j + 1));
            soup.push([f32s(p00), f32s(p10), f32s(p11)], 0, 0);
            soup.push([f32s(p00), f32s(p11), f32s(p01)], 0, 0);
        }
        soup.push([f32s([0.0, 0.0, 0.0]), f32s(ring(i + 1, 0)), f32s(ring(i, 0))], 0, 0);
        soup.push([f32s([0.0, 0.0, l]), f32s(ring(i, k)), f32s(ring(i + 1, k))], 0, 0);
    }
    soup.weld(Twins::Keep)
}

/// A cylinder of radius `R` meshed finely and simplified within `t` keeps at most twice the
/// triangles its curvature needs: `m = ⌈2π / θ⌉` chords of angle `θ = 2 acos(1 − t/R)` (the
/// most a chord may turn and stay within `t`), the side `2m` triangles, each end `m − 2`. The
/// control is `MovedOnly` in `simplify_plants_are_caught` (under it no chord grows past `t`).
#[test]
fn cylinder_simplifies_to_its_curvature_bound() {
    check(Config::cases(12), |tape| {
        let r = 1.0f64;
        let l = *tape.pick(&[0.5f64, 2.0], "length").unwrap_or(&0.5);
        let m0 = *tape.pick(&[96u32, 128], "segments").unwrap_or(&96);
        let k = tape.int_in(1..=4u32, "rings");
        let t = *tape.pick(&[0.01f64, 0.003], "tolerance").unwrap_or(&0.01);
        let input = cylinder_mesh(r, l, m0, k)?;
        let proof = verify(input.view()?).map_err(harness)?;
        let out = simplified(proof, Target::Error(crate::geom::length_up(t)), NoPlant)?;
        if out.error_bound.as_m() > t + 1e-9 {
            return Err(Broken::Oracle(MeshBroken::OverError { got: out.error_bound.as_m(), target: t }));
        }
        let theta = 2.0 * (1.0 - t / r).acos();
        let m = (core::f64::consts::TAU / theta).ceil() as u64;
        let bound = 2 * (2 * m + 2 * (m - 2));
        let got = u64::from(out.mesh.view().triangle_count());
        if got > bound {
            return Err(Broken::Oracle(MeshBroken::TooDense { what: "simplified cylinder", got, bound }));
        }
        Ok(())
    });
}

/// `t` turned so its normal is within 90 degrees of `want` (the generators below know each
/// patch's outward side, not each triangle's winding), pushed.
fn push_out(soup: &mut SoupBuf, t: [D3; 3], want: D3) {
    let t = if dot(tri_normal(t[0], t[1], t[2]), want) < 0.0 { [t[0], t[2], t[1]] } else { t };
    soup.push([f32s(t[0]), f32s(t[1]), f32s(t[2])], 0, 0);
}

/// The hull's skin in small: the half of a tube of radii `big` and `small` below its axis (along
/// `x`, length `l`), closed by its two flat rims on the axis plane and its two half-annulus ends.
/// `m` segments around each half circle, `k` rings of quads along.
fn half_tube_mesh(big: f64, small: f64, l: f64, m: u32, k: u32) -> Result<Built, Broken<MeshBroken>> {
    let arc = |rad: f64, i: u32| -> [f64; 2] {
        if i == 0 {
            return [-rad, 0.0];
        }
        if i == m {
            return [rad, 0.0];
        }
        let a = core::f64::consts::PI * (1.0 + f64::from(i) / f64::from(m));
        [rad * a.cos(), rad * a.sin()]
    };
    // The profile in `(y, z)`, closed: the outer arc under the axis, a rim, the inner arc back.
    let profile: StdVec<[f64; 2]> = (0..=m).map(|i| arc(big, i)).chain((0..=m).map(|i| arc(small, m - i))).collect();
    let n = profile.len();
    let along = |i: u32| l * f64::from(i) / f64::from(k);
    let mut soup = SoupBuf::default();
    for j in 0..n {
        let (q0, q1) = (profile[j], profile[(j + 1) % n]);
        // Outward: away from the axis on the outer arc, towards it on the inner, up on a rim.
        let mid = [0.0, (q0[0] + q1[0]) / 2.0, (q0[1] + q1[1]) / 2.0];
        let want = if q0[1] == 0.0 && q1[1] == 0.0 {
            [0.0, 0.0, 1.0]
        } else if j < m as usize {
            mid
        } else {
            scale(mid, -1.0)
        };
        for i in 0..k {
            let (p00, p10) = ([along(i), q0[0], q0[1]], [along(i + 1), q0[0], q0[1]]);
            let (p01, p11) = ([along(i), q1[0], q1[1]], [along(i + 1), q1[0], q1[1]]);
            push_out(&mut soup, [p00, p10, p11], want);
            push_out(&mut soup, [p00, p11, p01], want);
        }
    }
    for (x, want) in [(0.0, [-1.0, 0.0, 0.0]), (l, [1.0, 0.0, 0.0])] {
        for i in 0..m {
            let at = |q: [f64; 2]| [x, q[0], q[1]];
            let (o0, o1, i0, i1) = (at(arc(big, i)), at(arc(big, i + 1)), at(arc(small, i)), at(arc(small, i + 1)));
            push_out(&mut soup, [o0, o1, i1], want);
            push_out(&mut soup, [o0, i1, i0], want);
        }
    }
    soup.weld(Twins::Keep)
}

/// The hatch cover in small: the box of half sides `half` with every edge and corner rounded by
/// `round`. A grid on the unrounded box (`band` cells across each rounded strip, `middle` across
/// the flat between) is sent to the nearest point of the rounded one: the point on the core box
/// (`half − round`) nearest it, moved `round` towards it.
fn rounded_box_mesh(half: D3, round: f64, band: u32, middle: u32) -> Result<Built, Broken<MeshBroken>> {
    let core = [half[0] - round, half[1] - round, half[2] - round];
    let coords = |a: usize| -> StdVec<f64> {
        let (c, e) = (core[a], half[a]);
        let mut v = StdVec::new();
        for i in 0..band {
            v.push(-e + round * f64::from(i) / f64::from(band));
        }
        for i in 0..middle {
            v.push(-c + 2.0 * c * f64::from(i) / f64::from(middle));
        }
        for i in 0..band {
            v.push(c + round * f64::from(i) / f64::from(band));
        }
        v.push(e);
        v
    };
    let grid = [coords(0), coords(1), coords(2)];
    let place = |x: D3| -> D3 {
        let c = [x[0].clamp(-core[0], core[0]), x[1].clamp(-core[1], core[1]), x[2].clamp(-core[2], core[2])];
        let d = sub(x, c);
        add(c, scale(d, round / length(d)))
    };
    let mut soup = SoupBuf::default();
    for a in 0..3 {
        let (u, v) = ((a + 1) % 3, (a + 2) % 3);
        for s in [-1.0f64, 1.0] {
            let mut want = [0.0; 3];
            want[a] = s;
            // Every face reads the one grid per axis, so a point two faces share is one point.
            let at = |i: usize, j: usize| {
                let mut x = [0.0; 3];
                x[a] = if s < 0.0 { grid[a][0] } else { grid[a][grid[a].len() - 1] };
                x[u] = grid[u][i];
                x[v] = grid[v][j];
                place(x)
            };
            for i in 0..grid[u].len() - 1 {
                for j in 0..grid[v].len() - 1 {
                    let (p00, p10, p01, p11) = (at(i, j), at(i + 1, j), at(i, j + 1), at(i + 1, j + 1));
                    push_out(&mut soup, [p00, p10, p11], want);
                    push_out(&mut soup, [p00, p11, p01], want);
                }
            }
        }
    }
    soup.weld(Twins::Keep)
}

/// The most a chord of a circle of radius `r` may turn and stay within `t` of it.
fn chord_angle(r: f64, t: f64) -> f64 {
    2.0 * (1.0 - t / r).acos()
}

/// The corners simplification's maps may read per input triangle, per unit of
/// `1 + ln(T_in / T_out)` (`sagitta_case`).
const CORNER_BUDGET: f64 = 320.0;

/// Every vertex of `mesh` moved off its surface by up to `most`, along `p − centre(p)`, by a
/// hash of its index and `seed` (one draw, so a case replays; splitmix64's finaliser).
fn noisy(mesh: &mut Built, most: f64, seed: u64, centre: impl Fn(D3) -> D3) {
    for (i, p) in mesh.positions.iter_mut().enumerate() {
        let x = wide(*p);
        let c = centre(x);
        let d = sub(x, c);
        let l = length(d);
        if !(l > 0.0) {
            continue;
        }
        let mut h = seed ^ (i as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
        h = (h ^ (h >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        h = (h ^ (h >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        h ^= h >> 31;
        let u = (h >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0;
        *p = f32s(add(c, scale(d, 1.0 + most * u / l)));
    }
}

/// ORACLE: the hull's shapes in small, a half tube (the skin, the frames) and a rounded box (the
/// hatch cover), meshed finely and simplified within `t`, are within their reported bound (which
/// is at most `t`) on sampled points both ways, and keep at most twice their SAGITTA COUNT: the
/// triangles of the coarsest mesh whose chords are within `t`, with `⌈α / θ⌉` segments on an arc
/// of angle `α` and radius `r` (`θ = chord_angle(r, t)`).
/// - Half tube, `o` and `i` segments on its outer and inner half circles: `2o + 2i` on the curved
///   sides, 4 on the two rims, `o + i` on each end (a strip between the arcs): `4 (o + i) + 4`.
/// - Rounded box, `q` segments on each quarter round: 2 on each of 6 faces, `2q` on each of 12
///   edges, `q²` on each of 8 corners: `12 + 24 q + 8 q²`.
/// The input is either the tube in strips (`k` rings along it: already long along the axis) or, as
/// a contoured surface is, ISOTROPIC and NOISY: rings as long as the segments, and every vertex
/// moved off the surface by up to `t / 10` (along the radius, so rims and end caps stay planar; for
/// the box, away from its core). On that input freezing a triangle's pieces kept 3.4 times the
/// half tube's count (a prototype at 64 pieces; merging them by source, 1.5): the contoured hull
/// skin's 6.6 times.
/// A prototype of this simplifier kept 1.3 to 1.6 times the count on the half tube and 0.8 to 1.2
/// on the rounded box.
///
/// And the maps' WORK is bounded (simplify's COST): the corners they read, per input triangle, at
/// most `CORNER_BUDGET · (1 + ln(T_in / T_out))`. The prototypes read at most 108 per unit (the
/// noisy rounded box within 0.003), so the budget is three times that; a map scanning every live
/// piece reads about `T_in` times more.
///
/// The controls are in `simplify_plants_are_caught`: `MovedOnly` (no chord grows past `t`, and no
/// fine input coarsens) and `MeasuresEverything` (every map scans the whole map: quadratic).
fn sagitta_case(tape: &Tape, plant: impl Planted<DecimatePlant> + Copy) -> Result<(), Broken<MeshBroken>> {
    let t = *tape.pick(&[0.01f64, 0.003], "tolerance").unwrap_or(&0.01);
    let noisy_input = tape.coin("isotropic noisy input");
    let seed = u64::from(tape.int_in(0..=1_000_000u32, "noise seed"));
    let (input, count, what) = if tape.coin("rounded box") {
        let (half, round) = ([1.0, 0.7, 0.4], 0.2);
        let q = (core::f64::consts::FRAC_PI_2 / chord_angle(round, t)).ceil() as u64;
        let mut mesh = rounded_box_mesh(half, round, 6, 4)?;
        if noisy_input {
            let core_box = [half[0] - round, half[1] - round, half[2] - round];
            noisy(&mut mesh, t / 10.0, seed, |x| {
                [x[0].clamp(-core_box[0], core_box[0]), x[1].clamp(-core_box[1], core_box[1]), x[2].clamp(-core_box[2], core_box[2])]
            });
        }
        (mesh, 12 + 24 * q + 8 * q * q, "simplified rounded box")
    } else {
        let (big, small) = (1.0, 0.97);
        let o = (core::f64::consts::PI / chord_angle(big, t)).ceil() as u64;
        let i = (core::f64::consts::PI / chord_angle(small, t)).ceil() as u64;
        let mesh = if noisy_input {
            // 64 segments of about 0.049 on the outer arc; 20 rings of 0.05 along a length of 1.
            let mut mesh = half_tube_mesh(big, small, 1.0, 64, 20)?;
            noisy(&mut mesh, t / 10.0, seed, |x| [x[0], 0.0, 0.0]);
            mesh
        } else {
            let m0 = *tape.pick(&[96u32, 128], "segments").unwrap_or(&96);
            let k = tape.int_in(1..=2u32, "rings");
            half_tube_mesh(big, small, 2.0, m0, k)?
        };
        (mesh, 4 * (o + i) + 4, "simplified half tube")
    };
    let t_in = u32::try_from(input.triangles.len()).unwrap_or(u32::MAX);
    let proof = verify(input.view()?).map_err(harness)?;
    let out = simplified(proof, Target::Error(crate::geom::length_up(t)), plant)?;
    let t_out = out.mesh.view().triangle_count().max(1);
    let budget = (CORNER_BUDGET * f64::from(t_in) * (1.0 + (f64::from(t_in) / f64::from(t_out)).ln())) as u64;
    if out.stats.corners > budget {
        return Err(Broken::Oracle(MeshBroken::TooDense { what: "corners read by simplification's maps", got: out.stats.corners, bound: budget }));
    }
    let bound = out.error_bound.as_m();
    if bound > t + 1e-9 {
        return Err(Broken::Oracle(MeshBroken::OverError { got: bound, target: t }));
    }
    let output = Built::of(&out.mesh.view());
    // The shapes are about 2 across: the slack is the sampling's rounding, as in `simplify_case`.
    let slack = 2e-6;
    for _ in 0..8 {
        for (from, to) in [(&input, &output), (&output, &input)] {
            let p = sample(tape, from);
            let d = distance_to(to, p);
            if d > bound + slack {
                return Err(Broken::Oracle(MeshBroken::DistanceOverBound { distance: d, bound }));
            }
        }
    }
    let got = u64::from(out.mesh.view().triangle_count());
    if got > 2 * count {
        return Err(Broken::Oracle(MeshBroken::TooDense { what, got, bound: 2 * count }));
    }
    Ok(())
}

#[test]
fn half_tube_and_rounded_box_simplify_to_their_sagitta_counts() {
    check(Config::cases(8), |tape| sagitta_case(tape, NoPlant));
}

#[test]
fn simplify_keeps_its_promises() {
    check(Config::cases(300), |tape| simplify_case(tape, NoPlant));
}

#[test]
fn simplify_plants_are_caught() {
    // The sagitta case first: it catches `MovedOnly` and passes the others to `simplify_case`.
    controls(Config::cases(300), |tape, plant: DecimatePlant| {
        sagitta_case(tape, plant)?;
        simplify_case(tape, plant)
    });
}

#[test]
fn lod_chain_levels() {
    check(Config::cases(100), |tape| {
        let (mesh, _) = solid(tape)?;
        let proof = verify(mesh.view()?).map_err(harness)?;
        let chain = lod_chain(
            &proof,
            LodOpts {
                ratio: 0.5,
                fewest_triangles: 8,
                projection: Projection { focal_pixels: 935.3, most_error_pixels: 1.0 },
                cos_crease: COS_32_DEGREES,
                limits: MeshLimits::SHIPPED,
                alloc: heap(),
                scratch: heap(),
            },
        )
        .map_err(harness)?;
        let mut last: Option<(u32, blaze_units::Length)> = None;
        for (i, level) in chain.levels.iter().enumerate() {
            let view = level.mesh.view();
            verify(view).map_err(|e| Broken::Oracle(MeshBroken::LodWrong { why: format!("level {i}: {e:?}") }))?;
            let count = view.triangle_count();
            if let Some((c, d)) = last
                && (count >= c || level.switch_distance < d)
            {
                return Err(Broken::Oracle(MeshBroken::LodWrong {
                    why: format!("level {i}: {count} triangles at {:?} after {c} at {d:?}", level.switch_distance),
                }));
            }
            last = Some((count, level.switch_distance));
        }
        Ok(())
    });
}

// ---------------------------------------------------------------------------------------------
// Orders and clusters.

#[test]
fn cache_and_fetch_orders_are_permutations() {
    check(Config::cases(300), |tape| {
        let (mesh, _) = solid(tape)?;
        let view = mesh.view()?;
        let before = MeshDigest::of_unordered(&view, heap()).map_err(harness)?;
        let cache_size = tape.int_in(3..=32u32, "cache size");
        let cached = optimize_vertex_cache(&view, CacheOpts { cache_size, alloc: heap(), scratch: heap() }).map_err(harness)?;
        if MeshDigest::of_unordered(&cached.view(), heap()).map_err(harness)? != before {
            return Err(Broken::Oracle(MeshBroken::NotPermutation { what: "vertex cache order" }));
        }
        let fetched = optimize_vertex_fetch(&cached.view(), FetchOpts { alloc: heap(), scratch: heap() }).map_err(harness)?;
        if MeshDigest::of_unordered(&fetched.view(), heap()).map_err(harness)? != before {
            return Err(Broken::Oracle(MeshBroken::NotPermutation { what: "vertex fetch order" }));
        }
        let ratio = acmr(&fetched.view(), AcmrOpts { cache_size, scratch: heap() }).map_err(harness)?;
        if !(ratio > 0.0 && ratio <= 3.0) {
            return Err(Broken::Oracle(MeshBroken::NotPermutation { what: "acmr out of range" }));
        }
        Ok(())
    });
}

fn cluster_case(tape: &Tape, plant: impl Planted<ClusterPlant>) -> Result<(), Broken<MeshBroken>> {
    let (mesh, _) = solid(tape)?;
    let view = mesh.view()?;
    let caps = ClusterCaps { most_vertices: tape.int_in(3..=64u32, "vertex cap"), most_triangles: tape.int_in(1..=124u32, "triangle cap") };
    let out = clusters_planted(&view, ClusterOpts { caps, alloc: heap(), scratch: heap() }, plant).map_err(harness)?;
    let mut seen = vec![0u32; mesh.triangles.len()];
    for (i, c) in out.clusters.iter().enumerate() {
        if c.vertex_count > caps.most_vertices || c.triangle_count > caps.most_triangles {
            return Err(Broken::Oracle(MeshBroken::OverCap { cluster: i, vertices: c.vertex_count, triangles: c.triangle_count }));
        }
        let verts = &out.vertices[c.vertex_offset as usize..(c.vertex_offset + c.vertex_count) as usize];
        for local in 0..c.triangle_count as usize {
            let at = c.triangle_offset as usize + local;
            let source = out.sources[at].index();
            seen[source] += 1;
            let tri = out.triangles[at].map(|l| verts[usize::from(l)]);
            if tri != mesh.triangles[source] {
                return Err(Broken::Oracle(MeshBroken::NotPartition { why: format!("cluster {i} triangle {local} is {tri:?}") }));
            }
            let [a, b, cc] = corners_of(&mesh, source);
            if let Some(n) = unit(tri_normal(a, b, cc))
                && c.cone_cos > -1.0
                && dot(n, wide(c.cone_axis)) < f64::from(c.cone_cos) - 1e-6
            {
                return Err(Broken::Oracle(MeshBroken::BoundsWrong { cluster: i, why: String::from("a normal outside the cone") }));
            }
        }
        for &v in verts {
            if distance(wide(mesh.positions[v as usize]), wide(c.centre)) > f64::from(c.radius) {
                return Err(Broken::Oracle(MeshBroken::BoundsWrong { cluster: i, why: format!("vertex {v} outside the sphere") }));
            }
        }
    }
    if let Some(t) = seen.iter().position(|&n| n != 1) {
        return Err(Broken::Oracle(MeshBroken::NotPartition { why: format!("triangle {t} placed {} times", seen[t]) }));
    }
    Ok(())
}

#[test]
fn clusters_partition_within_caps() {
    check(Config::cases(400), |tape| cluster_case(tape, NoPlant));
}

#[test]
fn cluster_plants_are_caught() {
    controls(Config::cases(400), |tape, plant: ClusterPlant| cluster_case(tape, plant));
}

// ---------------------------------------------------------------------------------------------
// Quantization.

fn quant_case(tape: &Tape, plant: impl Planted<QuantPlant>) -> Result<(), Broken<MeshBroken>> {
    let spread = *tape.pick(&[1.0f32, 100.0, 0.01], "spread").unwrap_or(&1.0);
    let centre = *tape.pick(&[0.0f32, 5.0, -300.0], "centre").unwrap_or(&0.0);
    let positions = tape.vec(1..=64, "points", |t| {
        [0, 1, 2].map(|_| centre + spread * t.f32_in(-1.0..=1.0, "coordinate"))
    });
    let mesh = Built { normals: vec![[0.0, 0.0, 1.0]; positions.len()], positions, ..Built::default() };
    // Half the cases on a box handed in that is larger than the points' own.
    let within = if tape.coin("a box handed in") {
        let grow = spread * tape.f32_in(0.0..=2.0, "grow");
        Within::Box(Aabb { min: [centre - spread - grow; 3], max: [centre + spread + grow; 3] })
    } else {
        Within::OwnBounds
    };
    let q = quantize_positions_planted(&mesh.view()?, QuantOpts { within, alloc: heap() }, plant).map_err(harness)?;
    for (v, (&p, &code)) in mesh.positions.iter().zip(q.values.iter()).enumerate() {
        let back = q.dequantize.apply(code);
        for axis in 0..3 {
            let error = (f64::from(back[axis]) - f64::from(p[axis])).abs();
            if error > q.bound[axis] {
                return Err(Broken::Oracle(MeshBroken::QuantOutOfBound { vertex: v, axis, error, bound: q.bound[axis] }));
            }
        }
    }
    Ok(())
}

#[test]
fn quantized_positions_round_trip_within_bound() {
    check(Config::cases(1000), |tape| quant_case(tape, NoPlant));
}

#[test]
fn quant_plants_are_caught() {
    controls(Config::cases(1000), |tape, plant: QuantPlant| quant_case(tape, plant));
}

#[test]
fn octahedral_normals_round_trip_within_bound() {
    check(Config::cases(2000), |tape| {
        let n = [tape.f32_in(-1.0..=1.0, "x"), tape.f32_in(-1.0..=1.0, "y"), tape.f32_in(-1.0..=1.0, "z")];
        let Some(u) = unit(wide(n)) else { return Ok(()) };
        if let Some(code) = encode_oct16(n) {
            let error = distance(wide(decode_oct16(code)), u);
            if error > OCT16_BOUND {
                return Err(Broken::Oracle(MeshBroken::OctOutOfBound { bits: 16, error, bound: OCT16_BOUND }));
            }
        }
        if let Some(code) = encode_oct8(n) {
            let error = distance(wide(decode_oct8(code)), u);
            if error > OCT8_BOUND {
                return Err(Broken::Oracle(MeshBroken::OctOutOfBound { bits: 8, error, bound: OCT8_BOUND }));
            }
        }
        Ok::<(), Broken<MeshBroken>>(())
    });
}

// ---------------------------------------------------------------------------------------------
// Hulls and decomposition.

fn hull_case(tape: &Tape, plant: impl Planted<HullPlant>) -> Result<(), Broken<MeshBroken>> {
    let count = tape.int_in(4..=48usize, "points");
    let mut points = if tape.coin("on a sphere") {
        sphere_points(tape, count, 1.0)
    } else {
        (0..count).map(|_| [0, 1, 2].map(|_| tape.f32_in(-1.0..=1.0, "coordinate"))).collect()
    };
    if tape.one_in(4, "repeat a point") {
        let p = points[tape.index(points.len(), "repeated")];
        points.push(p);
    }
    let hull = match convex_hull_planted(&points, HullOpts { limits: MeshLimits::SHIPPED, alloc: heap(), scratch: heap() }, plant) {
        Ok(hull) => hull,
        // A drawn set within one plane is refused by name; nothing more to check.
        Err(HullError::Degenerate { .. }) => return Ok(()),
        Err(e) => return Err(harness(e)),
    };
    let slack = 4.0 * hull.tolerance + 1e-12;
    for (i, &p) in points.iter().enumerate() {
        for t in 0..hull.triangles.len() {
            let height = hull.height(t, p);
            if height > slack {
                return Err(Broken::Oracle(MeshBroken::HullMissing { point: i, height, tolerance: hull.tolerance }));
            }
        }
    }
    // The hull is a closed, outward surface.
    let mesh = hull_mesh(&points)?;
    verify(mesh.view()?).map_err(|e| Broken::Oracle(MeshBroken::HullWrong { why: format!("{e:?}") }))?;
    if !(hull.volume() > 0.0) {
        return Err(Broken::Oracle(MeshBroken::HullWrong { why: format!("volume {}", hull.volume()) }));
    }
    Ok(())
}

#[test]
fn hull_holds_every_point() {
    check(Config::cases(1000), |tape| hull_case(tape, NoPlant));
}

#[test]
fn hull_plants_are_caught() {
    controls(Config::cases(1000), |tape, plant: HullPlant| hull_case(tape, plant));
}

#[test]
fn hull_is_order_free_in_general_position() {
    check(Config::cases(300), |tape| {
        let count = tape.int_in(4..=40usize, "points");
        let points = sphere_points(tape, count, 1.0);
        let mut moved = points.clone();
        tape.shuffle(&mut moved, "point order");
        let opts = || HullOpts { limits: MeshLimits::SHIPPED, alloc: heap(), scratch: heap() };
        let (a, b) = match (convex_hull(&points, opts()), convex_hull(&moved, opts())) {
            (Ok(a), Ok(b)) => (a, b),
            _ => return Ok(()),
        };
        // Vertices are in input order, so compare the triangles as sets of position triples.
        let triples = |h: &Hull<'_>| {
            let mut out: StdVec<[Key; 3]> = h
                .triangles
                .iter()
                .map(|t| {
                    let k = t.map(|v| key(h.points[v as usize]));
                    let first = (0..3).min_by_key(|&i| k[i]).unwrap_or(0);
                    [k[first], k[(first + 1) % 3], k[(first + 2) % 3]]
                })
                .collect();
            out.sort_unstable();
            out
        };
        if triples(&a) != triples(&b) {
            return Err(Broken::Oracle(MeshBroken::OrderDependent { what: "hull" }));
        }
        Ok::<(), Broken<MeshBroken>>(())
    });
}

#[test]
fn hull_refuses_flat_input_by_name() {
    check(Config::cases(200), |tape| {
        let count = tape.int_in(4..=20usize, "points");
        let points: StdVec<P3> = (0..count).map(|_| [tape.f32_in(-1.0..=1.0, "x"), tape.f32_in(-1.0..=1.0, "y"), 0.5]).collect();
        match convex_hull(&points, HullOpts { limits: MeshLimits::SHIPPED, alloc: heap(), scratch: heap() }) {
            Err(HullError::Degenerate { .. }) => Ok(()),
            other => Err(Broken::Oracle(MeshBroken::HullWrong { why: format!("flat input gave {:?}", other.map(|h| h.points.len())) })),
        }
    });
}

#[test]
fn decomposition_holds_every_vertex() {
    check(Config::cases(60), |tape| {
        let (mesh, size) = match tape.below(3, "shape") {
            0 => cube(tape)?,
            1 => glued(tape)?,
            _ => icosphere(tape)?,
        };
        let proof = verify(mesh.view()?).map_err(harness)?;
        let mut limits = MeshLimits::SHIPPED;
        limits.most_pieces = tape.int_in(1..=8u32, "pieces");
        limits.most_split_depth = tape.int_in(0..=4u32, "depth");
        let parts = decompose(&proof, DecomposeOpts { concavity: 0.01, rays: 16, limits, alloc: heap(), scratch: heap() })
            .map_err(harness)?;
        if parts.pieces.len() > limits.most_pieces as usize {
            return Err(Broken::Oracle(MeshBroken::HullWrong { why: format!("{} pieces", parts.pieces.len()) }));
        }
        for (v, &p) in mesh.positions.iter().enumerate() {
            let held = parts.pieces.iter().any(|piece| {
                let slack = 4.0 * piece.hull.tolerance + 1e-6 * size;
                (0..piece.hull.triangles.len()).all(|t| piece.hull.height(t, p) <= slack)
            });
            if !held {
                return Err(Broken::Oracle(MeshBroken::PieceMissing { vertex: v }));
            }
        }
        Ok(())
    });
}

// ---------------------------------------------------------------------------------------------
// The mesher: a model of boxes and spheres, edited at one leaf or meshed at another tolerance.

/// A sphere of radius `size` or a cube of half-side `size` at `at`, millimetres.
#[derive(Clone, Copy, Debug)]
struct Blob {
    sphere: bool,
    at: [i64; 3],
    size: i64,
}

fn blobs(tape: &Tape) -> StdVec<Blob> {
    tape.vec(1..=3, "blobs", |tp| Blob {
        sphere: tp.coin("sphere"),
        at: [tp.int_in(-40..=40i64, "x"), tp.int_in(-40..=40i64, "y"), tp.int_in(-40..=40i64, "z")],
        size: tp.int_in(12..=30i64, "size"),
    })
}

/// The union of the blobs, compiled.
fn blob_tape(blobs: &[Blob]) -> Result<blaze_cad_field::Tape<'static>, Broken<MeshBroken>> {
    use blaze_cad_field::{BoxOpts, CompileOpts, Graph, GraphOpts, Outside, Placement, Solid, SphereOpts, compile};
    use blaze_units::Length;
    let mut g = Graph::new(GraphOpts { alloc: heap(), .. });
    let mut acc: Option<Solid<Outside>> = None;
    for b in blobs {
        let size = b.size as f64;
        let s = if b.sphere {
            g.sphere(SphereOpts { radius: Length::mm(size), .. })
        } else {
            g.cuboid(BoxOpts { size: [Length::mm(2.0 * size); 3], .. })
        }
        .map_err(harness)?;
        let at = Placement::shift(Length::mm(b.at[0] as f64), Length::mm(b.at[1] as f64), Length::mm(b.at[2] as f64));
        let s = g.place(s, at).map_err(harness)?;
        acc = Some(match acc {
            None => s.weaken::<Outside>(),
            Some(a) => g.union(a, s).map_err(harness)?,
        });
    }
    let root = acc.ok_or_else(|| harness("no blobs"))?;
    compile(&g, root.id(), CompileOpts { alloc: heap(), .. }).map_err(harness)
}

fn contour_opts(tolerance_mm: f64) -> MeshOpts<'static, 'static, 'static> {
    MeshOpts { alloc: heap(), scratch: heap(), tolerance: blaze_units::Length::mm(tolerance_mm), .. }
}

/// ORACLE: after an edit of one leaf (a blob moved or grown), a change of tolerance across an
/// octave, or both, (1) the mesh made with the first run's pieces in the cache equals a mesh made
/// from scratch, position for position and triangle for triangle; (2) the pieces it recomputed are
/// exactly the cells whose key is not among the first run's keys (both from scratch); (3) every
/// vertex of the scratch mesh is within its stated `vertex_bound` of the surface, read through
/// the field: the field is 1-Lipschitz, so `|f(v)|` is at most `v`'s distance to the surface.
/// The mesher returning at all means the verifier passed (it runs on every result).
fn contour_case(tape: &Tape, plant: impl Planted<ContourPlant> + Copy) -> Result<(), Broken<MeshBroken>> {
    let before = blobs(tape);
    let mut after = before.clone();
    let tolerances = [10.0f64, 20.0];
    let first = tape.index(2, "tolerance");
    let edit = tape.below(3, "edit");
    if edit != 1 {
        let i = tape.index(after.len(), "edited");
        if tape.coin("move") {
            let k = tape.index(3, "axis");
            after[i].at[k] += tape.int_in(1..=6i64, "by");
        } else {
            after[i].size += tape.int_in(1..=4i64, "grow");
        }
    }
    let second = if edit == 0 { first } else { 1 - first };
    let (t0, t1) = (blob_tape(&before)?, blob_tape(&after)?);
    let (o0, o1) = (contour_opts(tolerances[first]), contour_opts(tolerances[second]));
    let scratch0 = contour::mesh(&t0, o0).map_err(harness)?;
    let scratch1 = contour::mesh(&t1, o1).map_err(harness)?;
    let mut cache = PieceCache::new(PieceCacheOpts { alloc: heap(), .. });
    // Both from-scratch runs passed: a cached run that fails differs from them.
    let differs = |e: MeshError| Broken::Oracle(MeshBroken::MeshDiffers { why: format!("the cached run failed: {e:?}") });
    mesh_planted(&t0, o0, Some(&mut cache), plant).map_err(differs)?;
    let cached = mesh_planted(&t1, o1, Some(&mut cache), plant).map_err(differs)?;

    let (got, want) = (cached.mesh.view(), scratch1.mesh.view());
    if got.positions() != want.positions() || got.triangles() != want.triangles() {
        return Err(Broken::Oracle(MeshBroken::MeshDiffers {
            why: format!(
                "{} vertices and {} triangles cached, {} and {} from scratch ({} pieces reused)",
                got.vertex_count(),
                got.triangle_count(),
                want.vertex_count(),
                want.triangle_count(),
                cached.stats.reused
            ),
        }));
    }

    let old: std::collections::BTreeSet<RegionKey> = scratch0.keys.iter().map(|(k, _)| *k).collect();
    let mut changed: StdVec<[i64; 6]> = scratch1.keys.iter().filter(|(k, _)| !old.contains(k)).map(|(k, _)| k.cell).collect();
    let mut recomputed: StdVec<[i64; 6]> = cached.keys.iter().filter(|(_, made)| *made).map(|(k, _)| k.cell).collect();
    changed.sort_unstable();
    recomputed.sort_unstable();
    if changed != recomputed {
        return Err(Broken::Oracle(MeshBroken::RecomputedWrong { recomputed: recomputed.len(), changed: changed.len() }));
    }

    let mut sc = blaze_cad_field::Scratch::new(heap());
    let bound = scratch1.vertex_bound.as_m();
    for (v, p) in want.positions().iter().enumerate() {
        let value = blaze_cad_field::eval_point(&t1, blaze_math::Vec3::new(f64::from(p[0]), f64::from(p[1]), f64::from(p[2])), &mut sc);
        if !(value.abs() <= bound * (1.0 + 1e-9)) {
            return Err(Broken::Oracle(MeshBroken::FarVertex { vertex: v, value, bound }));
        }
    }
    Ok(())
}

#[test]
fn mesher_recomputes_only_changed_pieces() {
    check(Config::cases(30), |tape| contour_case(tape, NoPlant));
}

/// The octree count (contour's module documentation): `6 k A / s²` triangles for area `A`, with
/// `s` the largest power of two of nanometres at most `side_nm`, `k` the mean of
/// `|n_x| + |n_y| + |n_z|`.
fn octree_count(k: f64, area_mm2: f64, side_nm: f64) -> f64 {
    let s = (1u64 << (63 - (side_nm as u64).leading_zeros())) as f64 / 1e6;
    6.0 * k * area_mm2 / (s * s)
}

/// The leaf of a tolerance, nanometres (contour's grid).
fn leaf_of(tolerance_mm: f64) -> f64 {
    let half = (tolerance_mm * 1e6 / 2.0) as u64;
    (1u64 << (63 - half.leading_zeros())) as f64
}

/// Meshes `t` with the walk capped at `cap` cells and the triangles bounded by `bound`: a walk over
/// the cap is the same failure, caught early (the uniform walk at a fine leaf is millions of cells).
fn bounded_mesh(
    t: &blaze_cad_field::Tape<'_>,
    tolerance_mm: f64,
    bound: f64,
    what: &'static str,
    plant: impl Planted<ContourPlant> + Copy,
) -> Result<u64, Broken<MeshBroken>> {
    let bound = bound as u64;
    let cap = 2 * bound + 4096;
    let o = MeshOpts { most_cells: cap, ..contour_opts(tolerance_mm) };
    match mesh_planted(t, o, None, plant) {
        Ok(m) => {
            let got = u64::from(m.stats.triangles);
            if got > bound {
                return Err(Broken::Oracle(MeshBroken::TooDense { what, got, bound }));
            }
            Ok(got)
        }
        Err(MeshError::TooManyCells { most }) => Err(Broken::Oracle(MeshBroken::TooDense { what: "cells looked at", got: most + 1, bound: cap })),
        Err(e) => Err(harness(e)),
    }
}

/// ORACLE, ADAPTIVITY: the triangle count follows the geometry, not the tolerance.
///
/// - A CYLINDER of radius `R` meshed at tolerance `t` has at most four times the octree count
///   (contour's module documentation) with `s ≤ √(2 R T)`, `T` two leaves, `k = 4/π` (the caps
///   counted as curved too: an over-count). The adaptive walk looks at about `0.4 N` cells for
///   that count `N`; the uniform walk at least `(4/3)(s/leaf)²/6 · N ≥ 14 N`, since `s ≥ 8`
///   leaves for the radii drawn (`R ≥ 28` leaves).
/// - A BOX stops at the feature side on every face, edge and corner (planes meet exactly): at most
///   four times the octree count of its faces grown by two cells, and the count at a quarter of
///   the tolerance at most twice the count at the tolerance (O(1) per face).
///
/// Each bound is checked as its mesh finishes, the cylinder first; a walk that looks at more than
/// twice a bound's cells (`8 N`) is stopped by its cell cap and reported as too dense, so the
/// planted uniform walk fails in a fraction of a second.
fn adaptive_case(tape: &Tape, plant: impl Planted<ContourPlant> + Copy) -> Result<(), Broken<MeshBroken>> {
    use blaze_cad_field::{BoxOpts, CompileOpts, CylinderOpts, Graph, GraphOpts, compile};
    use blaze_units::Length;
    let tolerance = [0.5f64, 1.0, 2.0][tape.index(3, "tolerance")];
    let radius = tape.int_in(15..=40i64, "radius") as f64;
    let height = tape.int_in(10..=60i64, "height") as f64;
    let mut g = Graph::new(GraphOpts { alloc: heap(), .. });
    let cyl = g.cylinder(CylinderOpts { radius: Length::mm(radius), height: Length::mm(height), .. }).map_err(harness)?;
    let t = compile(&g, cyl.id(), CompileOpts { alloc: heap(), .. }).map_err(harness)?;
    let leaf = leaf_of(tolerance);
    let feature = f64::from(1u32 << 23);
    let side = (2.0 * radius * 1e6 * 2.0 * leaf).sqrt().min(feature);
    let area = 2.0 * core::f64::consts::PI * radius * height + 2.0 * core::f64::consts::PI * radius * radius;
    let bound = 4.0 * octree_count(4.0 / core::f64::consts::PI, area, side);
    bounded_mesh(&t, tolerance, bound, "cylinder triangles", plant)?;

    let half = tape.int_in(10..=30i64, "half") as f64;
    let mut g = Graph::new(GraphOpts { alloc: heap(), .. });
    let cube = g.cuboid(BoxOpts { size: [Length::mm(2.0 * half); 3], .. }).map_err(harness)?;
    let placed = g.place(cube, blaze_cad_field::Placement::shift(Length::mm(0.37), Length::mm(-1.13), Length::mm(2.71))).map_err(harness)?;
    let t = compile(&g, placed.id(), CompileOpts { alloc: heap(), .. }).map_err(harness)?;
    let s_mm = feature / 1e6;
    let grown = 2.0 * half + 2.0 * s_mm;
    let bound = 4.0 * octree_count(1.0, 6.0 * grown * grown, feature);
    let coarse = bounded_mesh(&t, 1.0, bound, "box triangles at 1 mm", plant)?;
    let fine = bounded_mesh(&t, 0.25, bound, "box triangles at 0.25 mm", plant)?;
    if fine > 2 * coarse {
        return Err(Broken::Oracle(MeshBroken::TooDense { what: "box triangles at 0.25 mm over twice those at 1 mm", got: fine, bound: 2 * coarse }));
    }
    Ok(())
}

#[test]
fn mesher_is_adaptive() {
    check(Config::cases(6), |tape| adaptive_case(tape, NoPlant));
}

#[test]
fn mesher_plants_are_caught() {
    // The adaptive case first: it ignores the key plant and catches the uniform walk.
    controls(Config::cases(40), |tape, plant: ContourPlant| {
        adaptive_case(tape, plant)?;
        contour_case(tape, plant)
    });
}

/// ORACLE: a redistanced cube's solved grid agrees with the cube's exact field within three grid
/// steps wherever the exact distance is under half the band (the grid's distance to the nearest
/// crossing is within `step √3/2` of the true distance, and trilinear interpolation of a
/// 1-Lipschitz field adds at most `step √3`).
fn redistance_case(tape: &Tape, plant: impl Planted<RedistancePlant> + Copy) -> Result<(), Broken<MeshBroken>> {
    use blaze_cad_field::{BoxOpts, CompileOpts, Graph, GraphOpts, compile, eval_point};
    use blaze_units::Length;
    let half = tape.int_in(8..=20i64, "half") as f64;
    let step_mm = tape.int_in(1..=3i64, "step") as f64;
    let band_mm = 4.0 * step_mm;
    let mut g = Graph::new(GraphOpts { alloc: heap(), .. });
    let cube = g.cuboid(BoxOpts { size: [Length::mm(2.0 * half); 3], .. }).map_err(harness)?;
    let r = g.redistance(cube).map_err(harness)?;
    let o = RedistanceOpts { scratch: heap(), step: Length::mm(step_mm), band: Length::mm(band_mm), .. };
    solve_redistance_planted(&mut g, r, o, plant).map_err(harness)?;
    let solved = compile(&g, r.id(), CompileOpts { alloc: heap(), .. }).map_err(harness)?;
    let exact = compile(&g, cube.id(), CompileOpts { alloc: heap(), .. }).map_err(harness)?;
    let mut sc = blaze_cad_field::Scratch::new(heap());
    let bound = 3.0 * step_mm / 1000.0;
    let reach = (half + band_mm / 2.0) / 1000.0;
    for _ in 0..tape.int_in(8..=32usize, "points") {
        let at: D3 = [tape.f64_in(-reach..=reach, "x"), tape.f64_in(-reach..=reach, "y"), tape.f64_in(-reach..=reach, "z")];
        let p = blaze_math::Vec3::new(at[0], at[1], at[2]);
        let want = eval_point(&exact, p, &mut sc);
        if !(want.abs() < band_mm / 2000.0) {
            continue;
        }
        let got = eval_point(&solved, p, &mut sc);
        if !((got - want).abs() <= bound) {
            return Err(Broken::Oracle(MeshBroken::GridWrong { at, got, want, bound }));
        }
    }
    Ok(())
}

#[test]
fn redistance_grid_matches_exact_field() {
    check(Config::cases(40), |tape| redistance_case(tape, NoPlant));
}

#[test]
fn redistance_plants_are_caught() {
    controls(Config::cases(80), |tape, plant: RedistancePlant| redistance_case(tape, plant));
}

#[test]
fn auto_grid_scales_for_cabin_and_respects_min_feature() {
    use blaze_math::{Aabb3, Vec3};
    use blaze_units::Length;
    // 6 m x 4 m x 3 m cabin wall at 2 mm tolerance: auto_grid picks ~128 mm feature / ~256 mm
    // piece instead of the legacy 8.4 mm fixed default (dramatically fewer flat-wall cells).
    let cabin_box = Aabb3::new(Vec3::new(-3.0, -2.0, 0.0), Vec3::new(3.0, 2.0, 3.0));
    let g = auto_grid(&cabin_box, Length::mm(2.0), None);
    assert!(g.feature >= Length::mm(64.0), "expected coarse feature on 6m cabin, got {:?}", g.feature);
    assert!(g.piece >= g.feature * 2.0, "piece {:?} must be at least 2x feature {:?}", g.piece, g.feature);
    assert!((g.feature.as_nm() as u64).is_power_of_two(), "feature must be dyadic power of two");
    assert!((g.piece.as_nm() as u64).is_power_of_two(), "piece must be dyadic power of two");

    // With explicit 25 mm minimum feature (e.g. window mullion / bolt hole), feature is capped <= 25 mm.
    let g_fine = auto_grid(&cabin_box, Length::mm(2.0), Some(Length::mm(25.0)));
    assert!(g_fine.feature <= Length::mm(25.0), "feature {:?} must respect min_feature 25mm", g_fine.feature);
    assert!(g_fine.feature >= Length::mm(8.0));
}

#[test]
fn simplifying_a_rounded_trapezoid_keeps_its_triangles_well_conditioned() {
    use blaze_cad_field::{CompileOpts, ExtrudeOpts, Graph, GraphOpts, OffsetOpts, Profile, ProfileOpts, compile};
    use blaze_units::Length;

    let mut g = Graph::new(GraphOpts { alloc: heap(), .. });
    let points = [[-16.0, -1.0], [14.8, -1.0], [18.1, 0.9], [-17.2, 0.9]].map(|p| p.map(Length::mm));
    let profile = g.profile(ProfileOpts { profile: Profile::Polygon { points: &points }, .. }).expect("trapezoid");
    let prism = g.extrude(profile, ExtrudeOpts { height: Length::mm(76.8), .. }).expect("extrude");
    let rounded = g.grow(prism, OffsetOpts { by: Length::mm(0.6), .. }).expect("round");
    let tape = compile(&g, rounded.id(), CompileOpts { alloc: heap(), .. }).expect("compile");
    let tolerance = Length::mm(0.16);
    let grid = auto_grid(&tape.aabb(), tolerance, None);
    let input = mesh(&tape, MeshOpts { tolerance, piece: grid.piece, feature: grid.feature, alloc: heap(), scratch: heap(), .. })
        .expect("mesh");
    let result = simplified(input.verified(), Target::Error(tolerance), NoPlant).expect("decimate");
    let output = result.mesh.view();
    assert!(output.triangle_count() < input.mesh.view().triangle_count());
    for view in [input.mesh.view(), output] {
        for tri in view.triangles() {
            let p = tri.map(|id| wide(view.positions()[id as usize]));
            let area = tri_normal(p[0], p[1], p[2]);
            for i in 0..3 {
                let a = sub(p[(i + 1) % 3], p[i]);
                let b = sub(p[(i + 2) % 3], p[i]);
                let sin_sq = dot(area, area) / (dot(a, a) * dot(b, b));
                assert!(sin_sq > 1e-10, "near-collinear triangle {p:?}, sin²(angle) = {sin_sq:e}");
            }
        }
    }
}

#[test]
fn target_budget_enforces_triangle_cap_and_error_ceiling() {
    use blaze_cad_field::{CompileOpts, Graph, GraphOpts, SphereOpts, compile};
    use blaze_units::Length;
    let mut g = Graph::new(GraphOpts { alloc: heap(), .. });
    let s = g.sphere(SphereOpts { radius: Length::mm(40.0), .. }).expect("sphere");
    let t = compile(&g, s.id(), CompileOpts { alloc: heap(), .. }).expect("compile");
    let meshed = mesh(&t, MeshOpts { alloc: heap(), scratch: heap(), tolerance: Length::mm(0.5), .. }).expect("mesh");
    let orig = meshed.mesh.view().triangle_count();
    assert!(orig > 400, "sphere should mesh to > 400 tris, got {orig}");

    // 1) Budget of 160 tris with no ceiling: reaches <= 160 tris and preserves Verified proof.
    let d1 = decimate(
        Source::Verified(meshed.verified()),
        DecimateOpts {
            target: Target::Budget { error: Length::mm(0.2), max_triangles: 160, max_error: None },
            cos_crease: COS_32_DEGREES,
            limits: MeshLimits::SHIPPED,
            alloc: heap(),
            scratch: heap(),
            ..
        },
    )
    .expect("budget decimate");
    assert_eq!(d1.stop, Stop::Reached);
    assert!(d1.mesh.view().triangle_count() <= 160);
    assert!(d1.verified().is_some());

    // 2) Impossible budget of 20 tris with tight 0.6 mm error ceiling: stops at ErrorCeiling and
    // keeps error_bound <= 0.6 mm + 1 nm!
    let ceiling = Length::mm(0.6);
    let d2 = decimate(
        Source::Verified(meshed.verified()),
        DecimateOpts {
            target: Target::Budget { error: Length::mm(0.1), max_triangles: 20, max_error: Some(ceiling) },
            cos_crease: COS_32_DEGREES,
            limits: MeshLimits::SHIPPED,
            alloc: heap(),
            scratch: heap(),
            ..
        },
    )
    .expect("ceiling decimate");
    assert_eq!(d2.stop, Stop::ErrorCeiling, "expected ErrorCeiling, got {:?}", d2.stop);
    assert!(d2.mesh.view().triangle_count() > 20);
    assert!(d2.error_bound <= ceiling + Length::nm(1), "error {:?} exceeded ceiling {:?}", d2.error_bound, ceiling);
}

#[test]
fn topology_counts_exact_holes_and_crease_profile_detects_bevels() {
    use blaze_cad_field::{BoxOpts, CompileOpts, CylinderOpts, Graph, GraphOpts, Placement, compile};
    use blaze_units::Length;
    let mut g = Graph::new(GraphOpts { alloc: heap(), .. });

    // Solid box -> genus 0, sharp creases, ~0 bevel edges.
    let plate = g.cuboid(BoxOpts { size: [Length::mm(80.0), Length::mm(50.0), Length::mm(12.0)], .. }).expect("plate");
    let t_box = compile(&g, plate.id(), CompileOpts { alloc: heap(), .. }).expect("compile box");
    let m_box = mesh(&t_box, MeshOpts { alloc: heap(), scratch: heap(), tolerance: Length::mm(1.0), .. }).expect("mesh box");
    let topo_box = topology(&m_box.mesh.view(), heap()).expect("topo box");
    assert_eq!(topo_box.components, 1);
    assert_eq!(topo_box.open_edges, 0);
    assert_eq!(topo_box.non_manifold_edges, 0);
    assert_eq!(topo_box.genus, Some(0), "solid box must have genus 0, got {topo_box:?}");

    // Plate with TWO through-holes -> genus 2!
    let h1 = g.cylinder(CylinderOpts { radius: Length::mm(8.0), height: Length::mm(30.0), .. }).expect("h1");
    let h1 = g.place(h1, Placement::shift(Length::mm(-18.0), Length::ZERO, Length::ZERO)).expect("p1");
    let h2 = g.cylinder(CylinderOpts { radius: Length::mm(8.0), height: Length::mm(30.0), .. }).expect("h2");
    let h2 = g.place(h2, Placement::shift(Length::mm(18.0), Length::ZERO, Length::ZERO)).expect("p2");
    let holes = g.union(h1, h2).expect("holes");
    let holed = g.difference(plate, holes).expect("holed");
    let t_holed = compile(&g, holed.id(), CompileOpts { alloc: heap(), .. }).expect("compile holed");
    let m_holed = mesh(&t_holed, MeshOpts { alloc: heap(), scratch: heap(), tolerance: Length::mm(1.0), .. }).expect("mesh holed");
    let topo_holed = topology(&m_holed.mesh.view(), heap()).expect("topo holed");
    assert_eq!(topo_holed.components, 1);
    assert_eq!(topo_holed.genus, Some(2), "plate with 2 through-holes must have genus 2, got {topo_holed:?}");

    // Rounded/beveled box vs sharp box: crease_profile sees bevel edges on the rounded box.
    let rounded = g
        .cuboid(BoxOpts { size: [Length::mm(60.0); 3], round: Length::mm(8.0), .. })
        .expect("rounded");
    let t_rnd = compile(&g, rounded.id(), CompileOpts { alloc: heap(), .. }).expect("compile rounded");
    let m_rnd = mesh(&t_rnd, MeshOpts { alloc: heap(), scratch: heap(), tolerance: Length::mm(0.8), .. }).expect("mesh rounded");
    let prof_sharp = crease_profile(&m_box.mesh.view(), CreaseProfileOpts { scratch: heap(), .. }).expect("prof sharp");
    let prof_rnd = crease_profile(&m_rnd.mesh.view(), CreaseProfileOpts { scratch: heap(), .. }).expect("prof rnd");
    assert!(prof_sharp.sharp_edge_length > Length::mm(100.0), "sharp box must have sharp creases");
    assert!(prof_rnd.bevel_edge_length > Length::mm(50.0), "rounded box must have bevel edges, got {prof_rnd:?}");
}

#[test]
fn feature_zone_refines_locally_updates_cache_keys_and_protects_decimation() {
    use blaze_cad_field::{BoxOpts, CompileOpts, Graph, GraphOpts, compile};
    use blaze_math::{Aabb3, Vec3};
    use blaze_units::Length;

    // 1) FeatureRegion signed distance and intersection sanity across Box, Ball, Capsule.
    let bx = FeatureRegion::Box(Aabb3::new(Vec3::new(-0.01, -0.01, -0.01), Vec3::new(0.01, 0.01, 0.01)));
    assert!(bx.contains(Vec3::new(0.0, 0.0, 0.0)));
    assert!(!bx.contains(Vec3::new(0.02, 0.0, 0.0)));
    let ball = FeatureRegion::Ball { center: Vec3::new(0.0, 0.0, 0.0), radius: Length::mm(10.0) };
    assert!((ball.signed_distance(Vec3::new(0.015, 0.0, 0.0)) - 0.005).abs() < 1e-9);
    let cap = FeatureRegion::Capsule {
        a: Vec3::new(-0.02, 0.0, 0.0),
        b: Vec3::new(0.02, 0.0, 0.0),
        radius: Length::mm(5.0),
    };
    assert!(cap.contains(Vec3::new(0.0, 0.004, 0.0)));
    assert!(!cap.contains(Vec3::new(0.0, 0.008, 0.0)));

    // 2) Local octree refinement & PieceCache soundness: a rounded bar with a critical zone on its
    // +X end refines locally there (more triangles inside +X than -X) and produces distinct
    // RegionKeys on zoned pieces while untouched -X pieces keep their cached keys.
    let mut g = Graph::new(GraphOpts { alloc: heap(), .. });
    let bar = g
        .cuboid(BoxOpts { size: [Length::mm(160.0), Length::mm(40.0), Length::mm(40.0)], round: Length::mm(8.0), .. })
        .expect("bar");
    let tape = compile(&g, bar.id(), CompileOpts { alloc: heap(), .. }).expect("compile");
    let grid = auto_grid(&tape.aabb(), Length::mm(1.2), None);

    let mut cache = PieceCache::new(PieceCacheOpts { alloc: heap(), .. });
    let plain = mesh_cached(
        &tape,
        MeshOpts { alloc: heap(), scratch: heap(), tolerance: Length::mm(1.2), piece: grid.piece, feature: grid.feature, .. },
        &mut cache,
    )
    .expect("plain mesh");

    let right_end = FeatureRegion::Box(Aabb3::new(Vec3::new(0.04, -0.03, -0.03), Vec3::new(0.09, 0.03, 0.03)));
    let mut zone = FeatureZone::new(right_end, FeatureProtect::Pinned);
    zone.max_error = Some(Length::mm(0.25));
    let zones = [zone];

    let zoned = mesh_cached(
        &tape,
        MeshOpts {
            alloc: heap(),
            scratch: heap(),
            tolerance: Length::mm(1.2),
            piece: grid.piece,
            feature: grid.feature,
            zones: &zones,
            ..
        },
        &mut cache,
    )
    .expect("zoned mesh");

    // Zoned mesh is watertight (Verified) even when sharing `cache` with a coarser `plain` run,
    // and preserves extra detail inside the +X zone.
    assert!(
        zoned.mesh.view().triangle_count() > plain.mesh.view().triangle_count(),
        "zoned mesh ({}) should preserve extra detail over plain ({})",
        zoned.mesh.view().triangle_count(),
        plain.mesh.view().triangle_count()
    );

    // Shifting only the +X zone (same leaf) reuses untouched -X pieces from `cache` while
    // recomputing only pieces touching the shifted zone!
    let right_end_shifted = FeatureRegion::Box(Aabb3::new(Vec3::new(0.045, -0.03, -0.03), Vec3::new(0.09, 0.03, 0.03)));
    let mut zone2 = FeatureZone::new(right_end_shifted, FeatureProtect::Pinned);
    zone2.max_error = Some(Length::mm(0.25));
    let zones2 = [zone2];
    let zoned2 = mesh_cached(
        &tape,
        MeshOpts {
            alloc: heap(),
            scratch: heap(),
            tolerance: Length::mm(1.2),
            piece: grid.piece,
            feature: grid.feature,
            zones: &zones2,
            ..
        },
        &mut cache,
    )
    .expect("shifted zoned mesh");
    assert!(zoned2.stats.reused > 0, "untouched pieces should reuse PieceCache: {:?}", zoned2.stats);
    assert!(zoned2.stats.recomputed > 0, "shifted zone pieces must recompute: {:?}", zoned2.stats);

    // 3) Protected decimation: under a tight budget, the Pinned +X end retains strictly more
    // triangles than the symmetrically identical unprotected -X end!
    let dec = decimate(
        Source::Verified(zoned.verified()),
        DecimateOpts {
            target: Target::Budget { error: Length::mm(1.2), max_triangles: 140, max_error: None },
            cos_crease: COS_32_DEGREES,
            zones: &zones,
            limits: MeshLimits::SHIPPED,
            alloc: heap(),
            scratch: heap(),
        },
    )
    .expect("zoned decimate");
    let v = dec.mesh.view();
    let pos = v.positions();
    let mut left_tris = 0u32;
    let mut right_tris = 0u32;
    for tri in v.triangles() {
        let cx = (pos[tri[0] as usize][0] + pos[tri[1] as usize][0] + pos[tri[2] as usize][0]) / 3.0;
        if cx > 0.04 {
            right_tris += 1;
        } else if cx < -0.04 {
            left_tris += 1;
        }
    }
    assert!(
        right_tris > left_tris * 2,
        "pinned +X zone ({right_tris} tris) must retain >2x geometry of unprotected -X end ({left_tris} tris)"
    );
}

// ---------------------------------------------------------------------------------------------
// UV unwrapping, tangent frames, multi-material shell simplification, and authored feature helpers.

fn find_root(parent: &mut [usize], mut x: usize) -> usize {
    while parent[x] != x {
        let up = parent[parent[x]];
        parent[x] = up;
        x = up;
    }
    x
}

fn check_uv_and_tangents(mapped: &UvMapped<'_>, gutter_px: u32, atlas_size: u32) -> Result<(), Broken<MeshBroken>> {
    let view = mapped.mesh.view();
    let Some(uvs) = view.uvs() else {
        return Err(Broken::Oracle(MeshBroken::UvWrong { why: String::from("missing uvs") }));
    };
    let Some(tangents) = view.tangents() else {
        return Err(Broken::Oracle(MeshBroken::UvTangentWrong { vertex: 0, why: String::from("missing tangents") }));
    };
    if mapped.chart_count == 0
        || !(mapped.texels_per_metre > 0.0)
        || !mapped.texels_per_metre.is_finite()
        || !(mapped.atlas_utilization > 0.0)
        || !mapped.atlas_utilization.is_finite()
    {
        return Err(Broken::Oracle(MeshBroken::UvWrong {
            why: format!(
                "invalid uv stats: charts={}, tpm={}, util={}",
                mapped.chart_count, mapped.texels_per_metre, mapped.atlas_utilization
            ),
        }));
    }

    for (v, &uv) in uvs.iter().enumerate() {
        if !uv[0].is_finite() || !uv[1].is_finite() || uv[0] < 0.0 || uv[0] > 1.0 || uv[1] < 0.0 || uv[1] > 1.0 {
            return Err(Broken::Oracle(MeshBroken::UvWrong {
                why: format!("vertex {v} uv out of [0, 1]^2: {uv:?}"),
            }));
        }
    }

    // Charts are vertex-disjoint in the output mesh, so triangle connected components over output
    // vertex indices recover the exact charts.
    let tris = view.triangles();
    let n_verts = view.positions().len();
    let mut parent: StdVec<usize> = (0..tris.len()).collect();
    let mut first_tri = vec![usize::MAX; n_verts];
    for (t, tri) in tris.iter().enumerate() {
        for &v in tri {
            let vi = v as usize;
            if first_tri[vi] == usize::MAX {
                first_tri[vi] = t;
            } else {
                let ra = find_root(&mut parent, first_tri[vi]);
                let rb = find_root(&mut parent, t);
                if ra < rb {
                    parent[rb] = ra;
                } else if rb < ra {
                    parent[ra] = rb;
                }
            }
        }
    }
    let mut chart_boxes: BTreeMap<usize, [f64; 4]> = BTreeMap::new();
    for (t, tri) in tris.iter().enumerate() {
        let root = find_root(&mut parent, t);
        let entry = chart_boxes.entry(root).or_insert([f64::INFINITY, f64::NEG_INFINITY, f64::INFINITY, f64::NEG_INFINITY]);
        for &v in tri {
            let uv = uvs[v as usize];
            let u = f64::from(uv[0]);
            let vc = f64::from(uv[1]);
            entry[0] = entry[0].min(u);
            entry[1] = entry[1].max(u);
            entry[2] = entry[2].min(vc);
            entry[3] = entry[3].max(vc);
        }
    }
    if chart_boxes.len() != mapped.chart_count as usize {
        return Err(Broken::Oracle(MeshBroken::UvWrong {
            why: format!("recovered {} chart components, expected {}", chart_boxes.len(), mapped.chart_count),
        }));
    }
    let boxes: StdVec<[f64; 4]> = chart_boxes.values().copied().collect();
    let want_gap = f64::from(gutter_px) / f64::from(atlas_size) - 1e-5;
    for i in 0..boxes.len() {
        for j in (i + 1)..boxes.len() {
            let a = boxes[i];
            let b = boxes[j];
            let sep_u = (b[0] - a[1]).max(a[0] - b[1]);
            let sep_v = (b[2] - a[3]).max(a[2] - b[3]);
            let gap = sep_u.max(sep_v);
            if gap < want_gap {
                return Err(Broken::Oracle(MeshBroken::UvChartsOverlap { chart_a: i, chart_b: j, gap, want: want_gap }));
            }
        }
    }

    // Verify orthonormal MikkTSpace tangent frame and handedness at every vertex.
    let mut expected_tan2 = vec![[0.0f64; 3]; n_verts];
    let positions = view.positions();
    for tri in tris {
        let i0 = tri[0] as usize;
        let i1 = tri[1] as usize;
        let i2 = tri[2] as usize;
        let p0 = wide(positions[i0]);
        let p1 = wide(positions[i1]);
        let p2 = wide(positions[i2]);
        let dp1 = sub(p1, p0);
        let dp2 = sub(p2, p0);
        let area = 0.5 * length(crate::geom::cross(dp1, dp2));
        if !(area > 0.0) {
            continue;
        }
        let du1 = f64::from(uvs[i1][0]) - f64::from(uvs[i0][0]);
        let dv1 = f64::from(uvs[i1][1]) - f64::from(uvs[i0][1]);
        let du2 = f64::from(uvs[i2][0]) - f64::from(uvs[i0][0]);
        let dv2 = f64::from(uvs[i2][1]) - f64::from(uvs[i0][1]);
        let det = du1 * dv2 - du2 * dv1;
        if det.abs() > 1e-16 {
            let b_dir = scale(sub(scale(dp2, du1), scale(dp1, du2)), area / det);
            for idx in [i0, i1, i2] {
                expected_tan2[idx] = add(expected_tan2[idx], b_dir);
            }
        }
    }

    let normals = view.normals();
    for v in 0..n_verts {
        let t = tangents[v];
        if !t.iter().all(|x| x.is_finite()) {
            return Err(Broken::Oracle(MeshBroken::UvTangentWrong {
                vertex: v,
                why: format!("non-finite tangent {t:?}"),
            }));
        }
        let t3 = [f64::from(t[0]), f64::from(t[1]), f64::from(t[2])];
        let t_len = length(t3);
        if (t_len - 1.0).abs() > 1e-4 {
            return Err(Broken::Oracle(MeshBroken::UvTangentWrong {
                vertex: v,
                why: format!("tangent not unit length: |T| = {t_len}"),
            }));
        }
        if t[3] != 1.0 && t[3] != -1.0 {
            return Err(Broken::Oracle(MeshBroken::UvTangentWrong {
                vertex: v,
                why: format!("tangent handedness w must be +-1, got {}", t[3]),
            }));
        }
        if let Some(n) = unit(wide(normals[v])) {
            let ndott = dot(n, t3).abs();
            if ndott > 1e-4 {
                return Err(Broken::Oracle(MeshBroken::UvTangentWrong {
                    vertex: v,
                    why: format!("tangent not orthogonal to normal: |T.N| = {ndott}"),
                }));
            }
            let b_len = length(expected_tan2[v]);
            if b_len > 1e-10 {
                let signed = dot(crate::geom::cross(n, t3), expected_tan2[v]);
                if signed.abs() > 1e-6 * b_len && signed * f64::from(t[3]) <= 0.0 {
                    return Err(Broken::Oracle(MeshBroken::UvTangentWrong {
                        vertex: v,
                        why: format!("wrong tangent handedness: signed={signed}, w={}", t[3]),
                    }));
                }
            }
        }
    }
    Ok(())
}

fn uv_case(tape: &Tape, plant: impl Planted<UvPlant> + Copy) -> Result<(), Broken<MeshBroken>> {
    let (mesh, _) = if tape.coin("uv shape: cube vs icosphere") { cube(tape)? } else { icosphere(tape)? };
    let gutter_px = 4u32;
    let atlas_size = 1024u32;
    let mapped = unwrap_uvs_planted(
        &mesh.view()?,
        UvOpts { alloc: heap(), scratch: heap(), gutter_px, atlas_size, .. },
        plant,
    )
    .map_err(harness)?;
    check_uv_and_tangents(&mapped, gutter_px, atlas_size)
}

#[test]
fn uv_unwrap_and_tangents_are_valid_non_overlapping_and_deterministic() {
    use blaze_cad_field::{CompileOpts, CylinderOpts, Graph, GraphOpts, SphereOpts, compile};
    use blaze_units::Length;

    // 1) Subdivided box with marked creases.
    let mut box_soup_buf = SoupBuf::default();
    box_soup(&mut box_soup_buf, &BoxSpec { lo: [-0.5; 3], hi: [0.5; 3], n: 4, part: 0, split_top: false });
    let mut box_mesh = box_soup_buf.weld(Twins::Keep).expect("weld box");
    mark_box_edges(&mut box_mesh, [-0.5; 3], [0.5; 3]);

    // 2) Curved contoured cylinder.
    let mut g = Graph::new(GraphOpts { alloc: heap(), .. });
    let cyl = g
        .cylinder(CylinderOpts { radius: Length::mm(30.0), height: Length::mm(80.0), .. })
        .expect("cylinder");
    let cyl_tape = compile(&g, cyl.id(), CompileOpts { alloc: heap(), .. }).expect("compile cyl");
    let cyl_meshed = mesh(&cyl_tape, MeshOpts { alloc: heap(), scratch: heap(), tolerance: Length::mm(1.2), .. }).expect("mesh cyl");

    // 3) Curved contoured sphere.
    let sph = g.sphere(SphereOpts { radius: Length::mm(40.0), .. }).expect("sphere");
    let sph_tape = compile(&g, sph.id(), CompileOpts { alloc: heap(), .. }).expect("compile sph");
    let sph_meshed = mesh(&sph_tape, MeshOpts { alloc: heap(), scratch: heap(), tolerance: Length::mm(1.2), .. }).expect("mesh sph");

    // 4) Multi-material box with per-vertex colours.
    let mut mm_soup = SoupBuf::default();
    box_soup(&mut mm_soup, &BoxSpec { lo: [-1.0; 3], hi: [1.0; 3], n: 4, part: 0, split_top: true });
    let mm_mesh = mm_soup.weld(Twins::Keep).expect("weld multi-material box");
    let mm_colours: StdVec<[u8; 4]> = (0..mm_mesh.positions.len())
        .map(|i| [(i % 251) as u8, ((i * 7) % 251) as u8, ((i * 13) % 251) as u8, 255])
        .collect();
    let mm_view = MeshView::new(Columns {
        positions: &mm_mesh.positions,
        normals: &mm_mesh.normals,
        tangents: &[],
        uvs: &[],
        colours: &mm_colours,
        triangles: &mm_mesh.triangles,
        parts: &mm_mesh.parts,
        roles: &mm_mesh.roles,
        creases: &mm_mesh.creases,
    })
    .expect("mm_view");

    let box_v = box_mesh.view().expect("box_v");
    let cyl_v = cyl_meshed.mesh.view();
    let sph_v = sph_meshed.mesh.view();
    let cases: [(&str, MeshView<'_>, bool); 4] = [
        ("box", box_v, false),
        ("cylinder", cyl_v, false),
        ("sphere", sph_v, false),
        ("multi_material_coloured", mm_view, true),
    ];

    for (name, view, has_colours) in cases {
        let m1 = unwrap_uvs(&view, UvOpts { alloc: heap(), scratch: heap(), .. }).expect(name);
        let m2 = unwrap_uvs(&view, UvOpts { alloc: heap(), scratch: heap(), .. }).expect(name);

        // Non-overlap, UV bounds, and orthonormal tangent frame invariants.
        check_uv_and_tangents(&m1, 4, 1024).unwrap_or_else(|e| panic!("{name} uv oracle failed: {e:?}"));

        // Exact bit-for-bit determinism across two independent runs.
        assert_eq!(MeshDigest::of(&m1.mesh.view()), MeshDigest::of(&m2.mesh.view()), "{name} digest must be bit-identical");
        assert_eq!(m1.texels_per_metre.to_bits(), m2.texels_per_metre.to_bits(), "{name} scale must be bit-identical");
        assert_eq!(m1.chart_count, m2.chart_count, "{name} chart count must be identical");
        assert_eq!(m1.atlas_utilization.to_bits(), m2.atlas_utilization.to_bits(), "{name} utilization must be identical");

        // Standalone compute_tangents agrees bit-for-bit with unwrap_uvs's tangents.
        let standalone = compute_tangents(&m1.mesh.view(), heap(), heap()).expect("compute_tangents");
        let embedded = m1.mesh.view().tangents().expect("embedded tangents");
        assert_eq!(standalone.len(), embedded.len());
        for (i, (a, b)) in standalone.iter().zip(embedded.iter()).enumerate() {
            for k in 0..4 {
                assert_eq!(a[k].to_bits(), b[k].to_bits(), "{name} tangent[{i}][{k}] mismatch");
            }
        }

        // Topology remains Verified and optional vertex colours survive seam splitting.
        verify(m1.mesh.view()).unwrap_or_else(|e| panic!("{name} lost Verified after unwrap_uvs: {e:?}"));
        if has_colours {
            assert_eq!(m1.mesh.view().colours().map(<[_]>::len), Some(m1.mesh.view().positions().len()));
        }

        // UVs, tangents, colours, roles, parts, and creases survive vertex-cache + vertex-fetch opt.
        let before_unordered = MeshDigest::of_unordered(&m1.mesh.view(), heap()).expect("unordered before");
        let cached = optimize_vertex_cache(&m1.mesh.view(), CacheOpts { cache_size: 32, alloc: heap(), scratch: heap() })
            .expect("optimize_vertex_cache");
        let fetched = optimize_vertex_fetch(&cached.view(), FetchOpts { alloc: heap(), scratch: heap() })
            .expect("optimize_vertex_fetch");
        assert!(fetched.view().uvs().is_some(), "{name} lost uvs in optimize_vertex_fetch");
        assert!(fetched.view().tangents().is_some(), "{name} lost tangents in optimize_vertex_fetch");
        if has_colours {
            assert!(fetched.view().colours().is_some(), "{name} lost colours in optimize_vertex_fetch");
        }
        let after_unordered = MeshDigest::of_unordered(&fetched.view(), heap()).expect("unordered after");
        assert_eq!(before_unordered, after_unordered, "{name} attributes/roles drifted across cache+fetch opt");
    }

    // Error paths: empty mesh and bad atlas size.
    let empty_v = MeshView::new(Columns {
        positions: &[],
        normals: &[],
        tangents: &[],
        uvs: &[],
        colours: &[],
        triangles: &[],
        parts: &[],
        roles: &[],
        creases: &[],
    })
    .expect("empty view");
    assert_eq!(unwrap_uvs(&empty_v, UvOpts { alloc: heap(), scratch: heap(), .. }).unwrap_err(), UvError::EmptyMesh);
    assert_eq!(
        unwrap_uvs(&box_v, UvOpts { alloc: heap(), scratch: heap(), atlas_size: 0, .. }).unwrap_err(),
        UvError::BadAtlasSize { size: 0 }
    );
}

#[test]
fn uv_plants_are_caught() {
    controls(Config::cases(40), |tape, plant: UvPlant| uv_case(tape, plant));
}

#[test]
fn multi_material_shell_roles_survive_simplification_and_cache_opt() {
    use blaze_math::{Aabb3, Vec3};
    use blaze_units::Length;

    // Build a single continuous closed 2m x 2m x 2m box shell (`n = 8` grid per face) with 3
    // distinct material roles on the `+z` face:
    // - Role 1: x in [-1.0, -0.25] (3 cells wide -> area 1.5 m^2)
    // - Role 2: x in [-0.25, 0.25] (2 cells wide -> area 1.0 m^2)
    // - Role 0: x in [0.25, 1.0] on +z (1.5 m^2) + all other 5 faces (20.0 m^2) = 21.5 m^2
    let mut soup = SoupBuf::default();
    let n = 8u32;
    let lo = [-1.0f64; 3];
    let hi = [1.0f64; 3];
    let coord = |axis: usize, i: u32| -> f64 {
        if i == 0 {
            lo[axis]
        } else if i == n {
            hi[axis]
        } else {
            lo[axis] + (hi[axis] - lo[axis]) * f64::from(i) / f64::from(n)
        }
    };
    for a in 0..3 {
        for side in 0..2 {
            let (u, v) = ((a + 1) % 3, (a + 2) % 3);
            let plane = if side == 0 { lo[a] } else { hi[a] };
            let point = |i: u32, j: u32| -> P3 {
                let mut p = [0.0f64; 3];
                p[a] = plane;
                p[u] = coord(u, i);
                p[v] = coord(v, j);
                f32s(p)
            };
            for i in 0..n {
                for j in 0..n {
                    let (p00, p10, p01, p11) = (point(i, j), point(i + 1, j), point(i, j + 1), point(i + 1, j + 1));
                    let tris = if (i + j) % 2 == 0 { [[p00, p10, p11], [p00, p11, p01]] } else { [[p00, p10, p01], [p10, p11, p01]] };
                    let role = if a == 2 && side == 1 {
                        if i < 3 {
                            1
                        } else if i < 5 {
                            2
                        } else {
                            0
                        }
                    } else {
                        0
                    };
                    for t in tris {
                        let t = if side == 1 { t } else { [t[0], t[2], t[1]] };
                        soup.push(t, 0, role);
                    }
                }
            }
        }
    }
    let shell = soup.weld(Twins::Keep).expect("weld shell");
    let shell_view = shell.view().expect("shell_view");
    let verified = verify(shell_view).expect("shell verified");
    assert_eq!(verified.proof().components(), 1, "must be one continuous shell");

    let role_areas = |v: &MeshView<'_>| -> [f64; 3] {
        let mut areas = [0.0f64; 3];
        for t in 0..v.triangle_count() {
            let c = v.corners(TriId(t));
            let a = 0.5 * length(tri_normal(wide(c[0]), wide(c[1]), wide(c[2])));
            let r = v.roles()[t as usize].0 as usize;
            areas[r] += a;
        }
        areas
    };

    // 1) Decimate the continuous shell aggressively: flat interiors collapse while role borders stay sharp!
    let dec = decimate(
        Source::Verified(verified),
        DecimateOpts {
            target: Target::Error(Length::mm(1.0)),
            cos_crease: COS_32_DEGREES,
            limits: MeshLimits::SHIPPED,
            alloc: heap(),
            scratch: heap(),
            ..
        },
    )
    .expect("decimate multi-material shell");
    assert!(dec.verified().is_some(), "decimated multi-material shell must remain Verified");
    assert!(
        dec.mesh.view().triangle_count() < shell_view.triangle_count() / 4,
        "flat shell should simplify substantially: {} -> {}",
        shell_view.triangle_count(),
        dec.mesh.view().triangle_count()
    );
    let dec_areas = role_areas(&dec.mesh.view());
    assert!((dec_areas[0] - 21.5).abs() < 1e-4, "role 0 area drifted: {}", dec_areas[0]);
    assert!((dec_areas[1] - 1.5).abs() < 1e-4, "role 1 area drifted: {}", dec_areas[1]);
    assert!((dec_areas[2] - 1.0).abs() < 1e-4, "role 2 area drifted: {}", dec_areas[2]);

    // Verify sharp straight role boundaries on the +z face: every role-1 triangle stays in x <= -0.25,
    // every role-2 triangle stays in [-0.25, 0.25].
    let dv = dec.mesh.view();
    for t in 0..dv.triangle_count() {
        let role = dv.roles()[t as usize];
        let c = dv.corners(TriId(t));
        for p in c {
            if role == MaterialRole(1) {
                assert!(p[0] <= -0.25 + 1e-5, "role 1 bled past x = -0.25: {p:?}");
            } else if role == MaterialRole(2) {
                assert!(p[0] >= -0.25 - 1e-5 && p[0] <= 0.25 + 1e-5, "role 2 bled outside [-0.25, 0.25]: {p:?}");
            }
        }
    }

    // 2) Pass through shade -> unwrap_uvs -> optimize_vertex_cache -> optimize_vertex_fetch.
    let shaded = shade(&dv, ShadeOpts { cos_crease: COS_32_DEGREES, alloc: heap(), scratch: heap() }).expect("shade");
    let unwrapped = unwrap_uvs(&shaded.view(), UvOpts { alloc: heap(), scratch: heap(), .. }).expect("unwrap_uvs");
    check_uv_and_tangents(&unwrapped, 4, 1024).expect("uv oracle on multi-material shell");
    assert!(unwrapped.chart_count >= 8, "6 faces with 3 bands on +z must produce >= 8 charts, got {}", unwrapped.chart_count);

    let cached = optimize_vertex_cache(&unwrapped.mesh.view(), CacheOpts { cache_size: 24, alloc: heap(), scratch: heap() })
        .expect("optimize_vertex_cache");
    let fetched = optimize_vertex_fetch(&cached.view(), FetchOpts { alloc: heap(), scratch: heap() })
        .expect("optimize_vertex_fetch");

    let final_areas = role_areas(&fetched.view());
    assert!((final_areas[0] - 21.5).abs() < 1e-4, "final role 0 area drifted: {}", final_areas[0]);
    assert!((final_areas[1] - 1.5).abs() < 1e-4, "final role 1 area drifted: {}", final_areas[1]);
    assert!((final_areas[2] - 1.0).abs() < 1e-4, "final role 2 area drifted: {}", final_areas[2]);
    assert_eq!(
        MeshDigest::of_unordered(&unwrapped.mesh.view(), heap()).expect("digest before"),
        MeshDigest::of_unordered(&fetched.view(), heap()).expect("digest after"),
        "cache + fetch optimization must preserve roles, UVs, and tangents exactly"
    );

    // 3) Verify authored-feature physical sizing constructors on FeatureZone.
    let region = FeatureRegion::Box(Aabb3::new(Vec3::new(-0.01, -0.01, -0.01), Vec3::new(0.01, 0.01, 0.01)));
    let groove = FeatureZone::for_groove(region, Length::mm(6.0), Length::mm(2.0), FeatureProtect::Critical);
    assert_eq!(groove.min_feature, Some(Length::mm(2.0)));
    assert_eq!(groove.max_error, Some(Length::mm(0.4)));
    assert!((groove.min_cos_sq.expect("groove min_cos_sq") - 0.956_772_728_821_300_4).abs() < 1e-12);
    assert_eq!(groove.protect, FeatureProtect::Critical);

    let bevel = FeatureZone::for_bevel(region, Length::mm(4.0), FeatureProtect::Pinned);
    assert_eq!(bevel.min_feature, Some(Length::mm(2.0)));
    assert_eq!(bevel.max_error, Some(Length::mm(0.14)));
    assert!((bevel.min_cos_sq.expect("bevel min_cos_sq") - 0.969_846_310_392_954_2).abs() < 1e-12);
    assert_eq!(bevel.protect, FeatureProtect::Pinned);

    let seam = FeatureZone::for_seam(region, Length::mm(5.0), Length::mm(2.0));
    assert_eq!(seam.protect, FeatureProtect::Pinned);
    assert_eq!(seam.min_feature, Some(Length::mm(2.0)));
    assert_eq!(seam.max_error, Some(Length::mm(0.5)));

    let stencil = FeatureZone::for_stencil(region, Length::mm(4.0));
    assert_eq!(stencil.min_feature, Some(Length::mm(2.0)));
    assert_eq!(stencil.max_error, Some(Length::mm(0.5)));
}
