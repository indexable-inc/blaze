//! Deterministic 2D pattern triangulator using curvilinear Coons transfinite interpolation
//! and alternating-diagonal ("Union-Jack") quad splitting.
//!
//! Why alternating-diagonal Coons meshing is ideal for woven garments:
//! 1. **Exact 1-to-1 seam matching**: opposite sides carry identical segment counts matching every
//!    [`SeamSpec`], guaranteeing watertight topological welding with zero T-junctions.
//! 2. **Warp/weft alignment**: grid edges follow the fabric's warp (`v`) and weft (`u`) grainlines.
//! 3. **Zero diagonal bias**: alternating triangle diagonals `(i + j) & 1` eliminate the asymmetric
//!    wrinkle shear bias produced by unidirectional quad splits.
//! 4. **Exact 1:1 flat-pattern UVs**: every vertex carries its exact flat 2D metric pattern coordinate
//!    `rest_uv_m`, so continuum membrane strain and ASTRA's Green-Lagrange wrinkle shader match the
//!    true physical undeformed textile with zero UV distortion.

#![blaze::sans_io]

use blaze_math::{Vec2, Vec3};
use blaze_own::{Alloc, AllocFailure, Vec};

use crate::pattern::{PanelSpec, PatternSet, Side};

/// Errors produced when discretizing a [`PatternSet`] into a simulatable garment mesh.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mesh2DError {
    Alloc(AllocFailure),
    EmptySide { panel: &'static str, side: Side },
    OppositeSideMismatch { panel: &'static str, side_a: Side, segs_a: u32, side_b: Side, segs_b: u32 },
    MissingPanel { name: &'static str },
    MissingEdge { panel: &'static str, edge: &'static str },
    SeamSegmentMismatch { seam: &'static str, segs_a: u32, segs_b: u32 },
}

impl From<AllocFailure> for Mesh2DError {
    fn from(e: AllocFailure) -> Self {
        Self::Alloc(e)
    }
}

/// Indexing metadata for one discretized panel inside a [`ClothMesh`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PanelSlice {
    pub name: &'static str,
    pub layer: u8,
    pub fabric_idx: usize,
    pub lining_fabric_idx: Option<usize>,
    pub vert_start: u32,
    pub vert_count: u32,
    pub tri_start: u32,
    pub tri_count: u32,
    /// Number of segments along `u` (`Bottom`/`Top`) and `v` (`Left`/`Right`).
    pub nu: u32,
    pub nv: u32,
    pub flip_normal: bool,
}

impl PanelSlice {
    /// Grid vertex index `(i in 0..=nu, j in 0..=nv)` inside the global [`ClothMesh`] vertex arrays.
    #[inline]
    pub const fn grid_vertex(&self, i: u32, j: u32) -> u32 {
        self.vert_start + j * (self.nu + 1) + i
    }

    /// Global vertex index of the `k`-th vertex along `side`.
    #[inline]
    pub const fn side_vertex(&self, side: Side, k: u32) -> u32 {
        match side {
            Side::Bottom => self.grid_vertex(k, 0),
            Side::Right => self.grid_vertex(self.nu, k),
            Side::Top => self.grid_vertex(k, self.nv),
            Side::Left => self.grid_vertex(0, k),
        }
    }
}

/// One matched vertex pair along a seam joining two panel edges.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SeamPair {
    pub seam_idx: u32,
    pub a: u32,
    pub b: u32,
    pub weld: bool,
}

/// One internal stitch constraint pinning a sub-panel edge vertex `sub_vert` onto a host panel's
/// interior vertex `host_vert` with a small outward offset along the host normal (so real simulated
/// pocket flaps, collar leaves, and belt loops hang & settle dynamically under gravity!).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct InternalStitch {
    pub host_vert: u32,
    pub sub_vert: u32,
    pub normal_offset_m: f64,
}

/// One interior edge between two triangles (for Bridson/Bergou isometric dihedral bending).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct BendingHinge {
    /// Shared edge vertices `(v0, v1)`.
    pub v0: u32,
    pub v1: u32,
    /// Opposite vertices `(v2, v3)` of the two triangles `(v0, v1, v2)` and `(v1, v0, v3)`.
    pub v2: u32,
    pub v3: u32,
    /// Rest dihedral angle in radians (`0.0` for flat sheet, non-zero for folded collars/pleats).
    pub rest_angle: f64,
    /// Effective bending stiffness (`N·m`) for this hinge.
    pub stiffness: f64,
}

/// One elastic edge constraint (waistband, rib cuff, hem drawcord).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct ElasticLink {
    pub a: u32,
    pub b: u32,
    pub rest_len: f64,
}

/// Per-vertex pin/tether constraint.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct VertexPin {
    pub vertex: u32,
    pub target: Vec3,
    pub stiffness: f64,
    pub sewing_only: bool,
}

/// Complete simulatable garment mesh produced from a [`PatternSet`] for one dressing layer (or all
/// layers together).
pub struct ClothMesh<'a> {
    pub panels: Vec<PanelSlice, Alloc<'a>>,
    /// Current 3D vertex positions (metres, glTF Y-up).
    pub pos: Vec<Vec3, Alloc<'a>>,
    /// Initial 3D pre-sewing positions (kept for tethers / diagnostics).
    pub init_pos: Vec<Vec3, Alloc<'a>>,
    /// Smooth outward vertex normals (recomputed after simulation).
    pub normals: Vec<Vec3, Alloc<'a>>,
    /// Exact flat 2D metric pattern coordinates `(u, v)` in metres.
    pub rest_uv_m: Vec<Vec2, Alloc<'a>>,
    /// Normalized `(s, t) in [0, 1]^2` panel grid coordinates.
    pub panel_st: Vec<Vec2, Alloc<'a>>,
    /// Which panel owns each vertex.
    pub vert_panel: Vec<u16, Alloc<'a>>,
    /// Per-vertex inverse mass (`0.0` = hard-pinned).
    pub inv_mass: Vec<f64, Alloc<'a>>,
    /// Per-vertex fabric index.
    pub vert_fabric: Vec<u16, Alloc<'a>>,
    /// Canonical representative vertex index after topological seam welding (initially identity).
    pub weld_rep: Vec<u32, Alloc<'a>>,
    /// Whether this vertex lies on a sewn seam (used for flat-felled seam trench relief).
    pub seam_depth_m: Vec<f64, Alloc<'a>>,
    /// Whether this vertex lies on an unsewn open boundary hem/cuff/collar edge.
    pub is_open_border: Vec<bool, Alloc<'a>>,
    /// Triangles indexing into `pos` (CCW viewed from outside).
    pub triangles: Vec<[u32; 3], Alloc<'a>>,
    /// Per-triangle panel index.
    pub tri_panel: Vec<u16, Alloc<'a>>,
    /// Per-triangle fabric index.
    pub tri_fabric: Vec<u16, Alloc<'a>>,
    /// Precomputed 2D rest-state inverse edge matrices `Dm^{-1}` (`[d00, d01, d10, d11]`) and rest area
    /// for continuum anisotropic StVK/XPBD membrane simulation.
    pub tri_dm_inv: Vec<[f64; 4], Alloc<'a>>,
    pub tri_rest_area: Vec<f64, Alloc<'a>>,
    /// Matched seam vertex pairs.
    pub seam_pairs: Vec<SeamPair, Alloc<'a>>,
    /// Internal stitch constraints anchoring sub-panels (flaps, folded collar leaves, belt loops)
    /// onto interior `(s, t)` lines of host panels during physics simulation.
    pub internal_stitches: Vec<InternalStitch, Alloc<'a>>,
    /// Interior dihedral bending hinges (built both within panels and across welded seams).
    pub hinges: Vec<BendingHinge, Alloc<'a>>,
    /// Elastic/gather edge constraints.
    pub elastics: Vec<ElasticLink, Alloc<'a>>,
    /// Pinned / tethered vertices.
    pub pins: Vec<VertexPin, Alloc<'a>>,
}

#[inline]
fn push<T>(v: &mut Vec<T, Alloc<'_>>, x: T) -> Result<(), Mesh2DError> {
    v.try_push(x).map_err(|e| Mesh2DError::Alloc(e.why))
}

/// Discretize all panels of `patterns` matching `layer_filter` (`None` = all layers, `Some(l)` =
/// only layer `l`) into a deterministic [`ClothMesh`].
pub fn build_cloth_mesh<'a>(
    patterns: &PatternSet<'_>,
    layer_filter: Option<u8>,
    alloc: Alloc<'a>,
) -> Result<ClothMesh<'a>, Mesh2DError> {
    let mut mesh = ClothMesh {
        panels: Vec::new_in(alloc),
        pos: Vec::new_in(alloc),
        init_pos: Vec::new_in(alloc),
        normals: Vec::new_in(alloc),
        rest_uv_m: Vec::new_in(alloc),
        panel_st: Vec::new_in(alloc),
        vert_panel: Vec::new_in(alloc),
        inv_mass: Vec::new_in(alloc),
        vert_fabric: Vec::new_in(alloc),
        weld_rep: Vec::new_in(alloc),
        seam_depth_m: Vec::new_in(alloc),
        is_open_border: Vec::new_in(alloc),
        triangles: Vec::new_in(alloc),
        tri_panel: Vec::new_in(alloc),
        tri_fabric: Vec::new_in(alloc),
        tri_dm_inv: Vec::new_in(alloc),
        tri_rest_area: Vec::new_in(alloc),
        seam_pairs: Vec::new_in(alloc),
        internal_stitches: Vec::new_in(alloc),
        hinges: Vec::new_in(alloc),
        elastics: Vec::new_in(alloc),
        pins: Vec::new_in(alloc),
    };

    // 1. Discretize every selected panel using Coons transfinite interpolation.
    for panel in patterns.panels.iter() {
        if let Some(l) = layer_filter {
            if panel.layer != l {
                continue;
            }
        }
        add_panel(&mut mesh, panel, alloc)?;
    }

    // 2. Compute per-triangle continuum rest matrices Dm^{-1} and vertex lumped masses.
    compute_rest_mechanics(&mut mesh, patterns)?;

    // 3. Resolve seams between panels present in this ClothMesh.
    for (seam_idx, seam) in patterns.seams.iter().enumerate() {
        let Some((pa_local, pa_slice)) = find_panel_slice(&mesh.panels, seam.panel_a) else {
            continue;
        };
        let Some((pb_local, pb_slice)) = find_panel_slice(&mesh.panels, seam.panel_b) else {
            continue;
        };
        let _ = (pa_local, pb_local);
        let (_, pa_spec) = patterns.find_panel(seam.panel_a).ok_or(Mesh2DError::MissingPanel { name: seam.panel_a })?;
        let (_, pb_spec) = patterns.find_panel(seam.panel_b).ok_or(Mesh2DError::MissingPanel { name: seam.panel_b })?;
        let (side_a, off_a, ea) = pa_spec
            .find_edge(seam.edge_a)
            .ok_or(Mesh2DError::MissingEdge { panel: seam.panel_a, edge: seam.edge_a })?;
        let (side_b, off_b, eb) = pb_spec
            .find_edge(seam.edge_b)
            .ok_or(Mesh2DError::MissingEdge { panel: seam.panel_b, edge: seam.edge_b })?;
        if ea.segments != eb.segments {
            return Err(Mesh2DError::SeamSegmentMismatch {
                seam: seam.name,
                segs_a: ea.segments,
                segs_b: eb.segments,
            });
        }
        let segs = ea.segments;
        let depth_m = seam.topstitch_depth.as_m();
        for k in 0..=segs {
            let va = pa_slice.side_vertex(side_a, off_a + k);
            let kb = if seam.reversed { segs - k } else { k };
            let vb = pb_slice.side_vertex(side_b, off_b + kb);
            if va != vb {
                push(&mut mesh.seam_pairs, SeamPair { seam_idx: seam_idx as u32, a: va, b: vb, weld: seam.weld })?;
            }
            if depth_m > 0.0 {
                let ia = va as usize;
                let ib = vb as usize;
                mesh.seam_depth_m[ia] = mesh.seam_depth_m[ia].max(depth_m);
                mesh.seam_depth_m[ib] = mesh.seam_depth_m[ib].max(depth_m);
            }
        }
    }

    // 3b. Resolve internal stitch lines attaching sub-panels (hanging flaps, folded collar leaves,
    //     belt loops) onto interior `(s, t)` lines of host panels!
    for ispec in patterns.internal_seams.iter() {
        let Some((_, host_slice)) = find_panel_slice(&mesh.panels, ispec.host_panel) else {
            continue;
        };
        let Some((_, sub_slice)) = find_panel_slice(&mesh.panels, ispec.sub_panel) else {
            continue;
        };
        let (_, sub_spec) = patterns
            .find_panel(ispec.sub_panel)
            .ok_or(Mesh2DError::MissingPanel { name: ispec.sub_panel })?;
        let (sub_side, sub_off, sub_edge) = sub_spec
            .find_edge(ispec.sub_edge)
            .ok_or(Mesh2DError::MissingEdge { panel: ispec.sub_panel, edge: ispec.sub_edge })?;
        let segs = sub_edge.segments;
        let depth_m = ispec.topstitch_depth.as_m();
        let off_m = ispec.normal_offset.as_m();
        for k in 0..=segs {
            let f = if segs == 0 { 0.0 } else { (k as f64) / (segs as f64) };
            let hs = (ispec.host_s0 + (ispec.host_s1 - ispec.host_s0) * f).clamp(0.0, 1.0);
            let ht = (ispec.host_t0 + (ispec.host_t1 - ispec.host_t0) * f).clamp(0.0, 1.0);
            let hi = ((hs * (host_slice.nu as f64)).round() as u32).min(host_slice.nu);
            let hj = ((ht * (host_slice.nv as f64)).round() as u32).min(host_slice.nv);
            let host_v = host_slice.grid_vertex(hi, hj);
            let sk = if ispec.reversed { segs - k } else { k };
            let sub_v = sub_slice.side_vertex(sub_side, sub_off + sk);
            push(&mut mesh.internal_stitches, InternalStitch { host_vert: host_v, sub_vert: sub_v, normal_offset_m: off_m })?;
            if depth_m > 0.0 {
                mesh.seam_depth_m[host_v as usize] = mesh.seam_depth_m[host_v as usize].max(depth_m);
                mesh.seam_depth_m[sub_v as usize] = mesh.seam_depth_m[sub_v as usize].max(depth_m);
            }
        }
    }

    // 4. Mark open (unsewn) border vertices.
    mark_open_borders(&mut mesh);

    // 5. Resolve pins and elastics.
    for pin in patterns.pins.iter() {
        let Some((_, pslice)) = find_panel_slice(&mesh.panels, pin.panel) else {
            continue;
        };
        let (_, pspec) = patterns.find_panel(pin.panel).ok_or(Mesh2DError::MissingPanel { name: pin.panel })?;
        let (side, off, edge) = pspec
            .find_edge(pin.edge)
            .ok_or(Mesh2DError::MissingEdge { panel: pin.panel, edge: pin.edge })?;
        for k in 0..=edge.segments {
            let v = pslice.side_vertex(side, off + k);
            let target = mesh.init_pos[v as usize];
            push(
                &mut mesh.pins,
                VertexPin { vertex: v, target, stiffness: pin.stiffness, sewing_only: pin.sewing_only },
            )?;
            if !pin.sewing_only && pin.stiffness >= 1.0 {
                mesh.inv_mass[v as usize] = 0.0;
            }
        }
    }

    for el in patterns.elastics.iter() {
        let Some((_, pslice)) = find_panel_slice(&mesh.panels, el.panel) else {
            continue;
        };
        let (_, pspec) = patterns.find_panel(el.panel).ok_or(Mesh2DError::MissingPanel { name: el.panel })?;
        let (side, off, edge) = pspec
            .find_edge(el.edge)
            .ok_or(Mesh2DError::MissingEdge { panel: el.panel, edge: el.edge })?;
        for k in 0..edge.segments {
            let a = pslice.side_vertex(side, off + k);
            let b = pslice.side_vertex(side, off + k + 1);
            let rest_len = (mesh.rest_uv_m[b as usize] - mesh.rest_uv_m[a as usize]).length() * el.rest_scale;
            push(&mut mesh.elastics, ElasticLink { a, b, rest_len })?;
        }
    }

    // 6. Build intra-panel isometric dihedral bending hinges (post-weld cross-seam hinges are added
    //    automatically by the solver after topological welding).
    rebuild_bending_hinges(&mut mesh, patterns)?;
    recompute_normals(&mut mesh);

    Ok(mesh)
}

fn find_panel_slice<'a>(panels: &'a [PanelSlice], name: &str) -> Option<(usize, &'a PanelSlice)> {
    panels.iter().enumerate().find(|(_, p)| p.name == name)
}

fn add_panel<'a>(mesh: &mut ClothMesh<'a>, panel: &PanelSpec<'_>, scratch: Alloc<'a>) -> Result<(), Mesh2DError> {
    let nb = panel.side_segments(Side::Bottom);
    let nt = panel.side_segments(Side::Top);
    let nl = panel.side_segments(Side::Left);
    let nr = panel.side_segments(Side::Right);
    if nb == 0 {
        return Err(Mesh2DError::EmptySide { panel: panel.name, side: Side::Bottom });
    }
    if nl == 0 {
        return Err(Mesh2DError::EmptySide { panel: panel.name, side: Side::Left });
    }
    if nb != nt {
        return Err(Mesh2DError::OppositeSideMismatch {
            panel: panel.name,
            side_a: Side::Bottom,
            segs_a: nb,
            side_b: Side::Top,
            segs_b: nt,
        });
    }
    if nl != nr {
        return Err(Mesh2DError::OppositeSideMismatch {
            panel: panel.name,
            side_a: Side::Left,
            segs_a: nl,
            side_b: Side::Right,
            segs_b: nr,
        });
    }

    let nu = nb;
    let nv = nl;
    let mut b_pts = Vec::new_in(scratch);
    let mut t_pts = Vec::new_in(scratch);
    let mut l_pts = Vec::new_in(scratch);
    let mut r_pts = Vec::new_in(scratch);
    for i in 0..=nu {
        push(&mut b_pts, panel.eval_side_vertex(Side::Bottom, i))?;
        push(&mut t_pts, panel.eval_side_vertex(Side::Top, i))?;
    }
    for j in 0..=nv {
        push(&mut l_pts, panel.eval_side_vertex(Side::Left, j))?;
        push(&mut r_pts, panel.eval_side_vertex(Side::Right, j))?;
    }
    let p00 = b_pts[0];
    let p10 = b_pts[nu as usize];
    let p01 = t_pts[0];
    let p11 = t_pts[nu as usize];

    let panel_idx = mesh.panels.len() as u16;
    let vert_start = mesh.pos.len() as u32;
    let tri_start = mesh.triangles.len() as u32;

    for j in 0..=nv {
        let t = (j as f64) / (nv as f64);
        let omt = 1.0 - t;
        let lj = l_pts[j as usize];
        let rj = r_pts[j as usize];
        for i in 0..=nu {
            let s = (i as f64) / (nu as f64);
            let oms = 1.0 - s;
            let bi = b_pts[i as usize];
            let ti = t_pts[i as usize];
            // Exact curvilinear Coons patch transfinite interpolation.
            let uv = bi * omt + ti * t + lj * oms + rj * s
                - (p00 * (oms * omt) + p10 * (s * omt) + p01 * (oms * t) + p11 * (s * t));
            let p3 = panel.wrap.map(uv);
            let vid = mesh.pos.len() as u32;
            push(&mut mesh.pos, p3)?;
            push(&mut mesh.init_pos, p3)?;
            push(&mut mesh.normals, Vec3::new(0.0, 0.0, -1.0))?;
            push(&mut mesh.rest_uv_m, uv)?;
            push(&mut mesh.panel_st, Vec2::new(s, t))?;
            push(&mut mesh.vert_panel, panel_idx)?;
            push(&mut mesh.inv_mass, 1.0)?;
            push(&mut mesh.vert_fabric, panel.fabric_idx as u16)?;
            push(&mut mesh.weld_rep, vid)?;
            push(&mut mesh.seam_depth_m, 0.0)?;
            push(&mut mesh.is_open_border, false)?;
        }
    }

    let stride = nu + 1;
    for j in 0..nv {
        for i in 0..nu {
            let v00 = vert_start + j * stride + i;
            let v10 = v00 + 1;
            let v01 = v00 + stride;
            let v11 = v01 + 1;
            // Alternating diagonal ("Union-Jack") eliminates anisotropic triangulation bias.
            let (t0, t1) = if ((i + j) & 1) == 0 {
                if !panel.flip_normal { ([v00, v10, v11], [v00, v11, v01]) } else { ([v00, v11, v10], [v00, v01, v11]) }
            } else {
                if !panel.flip_normal { ([v00, v10, v01], [v10, v11, v01]) } else { ([v00, v01, v10], [v10, v01, v11]) }
            };
            push(&mut mesh.triangles, t0)?;
            push(&mut mesh.tri_panel, panel_idx)?;
            push(&mut mesh.tri_fabric, panel.fabric_idx as u16)?;
            push(&mut mesh.triangles, t1)?;
            push(&mut mesh.tri_panel, panel_idx)?;
            push(&mut mesh.tri_fabric, panel.fabric_idx as u16)?;
        }
    }

    let vert_count = (nu + 1) * (nv + 1);
    let tri_count = nu * nv * 2;
    push(
        &mut mesh.panels,
        PanelSlice {
            name: panel.name,
            layer: panel.layer,
            fabric_idx: panel.fabric_idx,
            lining_fabric_idx: panel.lining_fabric_idx,
            vert_start,
            vert_count,
            tri_start,
            tri_count,
            nu,
            nv,
            flip_normal: panel.flip_normal,
        },
    )?;

    Ok(())
}

fn compute_rest_mechanics(mesh: &mut ClothMesh<'_>, patterns: &PatternSet<'_>) -> Result<(), Mesh2DError> {
    let n_verts = mesh.pos.len();
    for m in mesh.inv_mass.iter_mut() {
        *m = 0.0; // accumulate real mass first, then invert
    }
    mesh.tri_dm_inv.clear();
    mesh.tri_rest_area.clear();

    for (ti, &[a, b, c]) in mesh.triangles.iter().enumerate() {
        let ua = mesh.rest_uv_m[a as usize];
        let ub = mesh.rest_uv_m[b as usize];
        let uc = mesh.rest_uv_m[c as usize];
        let d1 = ub - ua;
        let d2 = uc - ua;
        let det = d1.x * d2.y - d2.x * d1.y;
        let area = 0.5 * det.abs();
        let dm_inv = if det.abs() > 1e-12 {
            let inv_det = 1.0 / det;
            [d2.y * inv_det, -d2.x * inv_det, -d1.y * inv_det, d1.x * inv_det]
        } else {
            [0.0; 4]
        };
        push(&mut mesh.tri_dm_inv, dm_inv)?;
        push(&mut mesh.tri_rest_area, area)?;

        let fab_idx = mesh.tri_fabric[ti] as usize;
        let density = patterns.fabrics.get(fab_idx).map(|f| f.density_kg_m2()).unwrap_or(0.32);
        let tri_mass = (area * density).max(1e-6);
        let v_mass = tri_mass / 3.0;
        mesh.inv_mass[a as usize] += v_mass;
        mesh.inv_mass[b as usize] += v_mass;
        mesh.inv_mass[c as usize] += v_mass;
    }

    for i in 0..n_verts {
        let m = mesh.inv_mass[i];
        mesh.inv_mass[i] = if m > 1e-9 { 1.0 / m } else { 0.0 };
    }
    Ok(())
}

fn mark_open_borders(mesh: &mut ClothMesh<'_>) {
    for flag in mesh.is_open_border.iter_mut() {
        *flag = false;
    }
    for p in mesh.panels.iter() {
        for i in 0..=p.nu {
            mesh.is_open_border[p.grid_vertex(i, 0) as usize] = true;
            mesh.is_open_border[p.grid_vertex(i, p.nv) as usize] = true;
        }
        for j in 0..=p.nv {
            mesh.is_open_border[p.grid_vertex(0, j) as usize] = true;
            mesh.is_open_border[p.grid_vertex(p.nu, j) as usize] = true;
        }
    }
    for pair in mesh.seam_pairs.iter() {
        if pair.weld {
            mesh.is_open_border[pair.a as usize] = false;
            mesh.is_open_border[pair.b as usize] = false;
        }
    }
}

/// Build deterministic interior dihedral bending hinges across the current `weld_rep` topology.
/// Called both before sewing (intra-panel) and immediately after topological seam welding (so
/// cloth bends smoothly across sewn seams too!).
pub fn rebuild_bending_hinges(mesh: &mut ClothMesh<'_>, patterns: &PatternSet<'_>) -> Result<(), Mesh2DError> {
    mesh.hinges.clear();
    let alloc = *mesh.hinges.allocator();

    // Collect directed half-edges keyed by canonical welded vertex pair (lo, hi).
    #[derive(Clone, Copy)]
    struct EdgeEntry {
        lo: u32,
        hi: u32,
        v0: u32,
        v1: u32,
        opp: u32,
        tri_idx: u32,
    }
    let mut edges = Vec::new_in(alloc);
    for (ti, &[a, b, c]) in mesh.triangles.iter().enumerate() {
        let ra = mesh.weld_rep[a as usize];
        let rb = mesh.weld_rep[b as usize];
        let rc = mesh.weld_rep[c as usize];
        if ra == rb || rb == rc || rc == ra {
            continue;
        }
        let tri_edges = [(ra, rb, rc), (rb, rc, ra), (rc, ra, rb)];
        for (v0, v1, opp) in tri_edges {
            let (lo, hi) = if v0 < v1 { (v0, v1) } else { (v1, v0) };
            push(&mut edges, EdgeEntry { lo, hi, v0, v1, opp, tri_idx: ti as u32 })?;
        }
    }
    edges.sort_unstable_by(|x, y| (x.lo, x.hi, x.tri_idx).cmp(&(y.lo, y.hi, y.tri_idx)));

    // Pre-build a quick lookup for fold_angle_rad on welded seams.
    let mut i = 0usize;
    while i + 1 < edges.len() {
        let e0 = edges[i];
        let e1 = edges[i + 1];
        if e0.lo == e1.lo && e0.hi == e1.hi {
            // Skip non-manifold triples if any.
            let mut end = i + 2;
            while end < edges.len() && edges[end].lo == e0.lo && edges[end].hi == e0.hi {
                end += 1;
            }
            if end == i + 2 && e0.opp != e1.opp {
                let fab0 = mesh.tri_fabric[e0.tri_idx as usize] as usize;
                let fab1 = mesh.tri_fabric[e1.tri_idx as usize] as usize;
                let k0 = patterns.fabrics.get(fab0).map(|f| f.bending_stiffness).unwrap_or(0.015);
                let k1 = patterns.fabrics.get(fab1).map(|f| f.bending_stiffness).unwrap_or(0.015);
                let stiffness = 0.5 * (k0 + k1);
                // Orient so (v0, v1, v2) and (v1, v0, v3) share the edge consistently.
                let (v0, v1, v2, v3) = (e0.v0, e0.v1, e0.opp, e1.opp);
                let rest_angle = lookup_seam_fold_angle(mesh, patterns, e0.lo, e0.hi);
                push(&mut mesh.hinges, BendingHinge { v0, v1, v2, v3, rest_angle, stiffness })?;
            }
            i = end;
        } else {
            i += 1;
        }
    }
    Ok(())
}

fn lookup_seam_fold_angle(mesh: &ClothMesh<'_>, patterns: &PatternSet<'_>, lo: u32, hi: u32) -> f64 {
    for seam in patterns.seams.iter() {
        if seam.fold_angle_rad == 0.0 {
            continue;
        }
        // Check if both lo and hi belong to this seam's welded pairs.
        let mut has_lo = false;
        let mut has_hi = false;
        for pair in mesh.seam_pairs.iter() {
            if patterns.seams[pair.seam_idx as usize].name == seam.name {
                let r = mesh.weld_rep[pair.a as usize];
                if r == lo {
                    has_lo = true;
                }
                if r == hi {
                    has_hi = true;
                }
            }
        }
        if has_lo && has_hi {
            return seam.fold_angle_rad;
        }
    }
    0.0
}

/// Recompute area-weighted smooth vertex normals across welded representatives.
pub fn recompute_normals(mesh: &mut ClothMesh<'_>) {
    for n in mesh.normals.iter_mut() {
        *n = Vec3::ZERO;
    }
    for &[a, b, c] in mesh.triangles.iter() {
        let ra = mesh.weld_rep[a as usize] as usize;
        let rb = mesh.weld_rep[b as usize] as usize;
        let rc = mesh.weld_rep[c as usize] as usize;
        let pa = mesh.pos[ra];
        let pb = mesh.pos[rb];
        let pc = mesh.pos[rc];
        let fnrm = (pb - pa).cross(pc - pa);
        mesh.normals[ra] = mesh.normals[ra] + fnrm;
        mesh.normals[rb] = mesh.normals[rb] + fnrm;
        mesh.normals[rc] = mesh.normals[rc] + fnrm;
    }
    for i in 0..mesh.normals.len() {
        let r = mesh.weld_rep[i] as usize;
        let n = mesh.normals[r];
        let len = n.length();
        mesh.normals[i] = if len > 1e-12 { n * (1.0 / len) } else { Vec3::new(0.0, 0.0, -1.0) };
    }
}

/// Sample bilinearly interpolated `(position, outward_normal, tangent_u, tangent_v)` at normalized
/// coordinates `(s, t) in [0, 1]^2` on `panel_slice` of a draped [`ClothMesh`].
pub fn sample_panel_frame(mesh: &ClothMesh<'_>, panel: &PanelSlice, s: f64, t: f64) -> (Vec3, Vec3, Vec3, Vec3) {
    let su = (s.clamp(0.0, 1.0) * (panel.nu as f64)).min((panel.nu as f64) - 1e-9).max(0.0);
    let tv = (t.clamp(0.0, 1.0) * (panel.nv as f64)).min((panel.nv as f64) - 1e-9).max(0.0);
    let i0 = (su as u32).min(panel.nu.saturating_sub(1));
    let j0 = (tv as u32).min(panel.nv.saturating_sub(1));
    let fx = su - (i0 as f64);
    let fy = tv - (j0 as f64);

    let g = |i: u32, j: u32| -> (Vec3, Vec3) {
        let v = mesh.weld_rep[panel.grid_vertex(i, j) as usize] as usize;
        (mesh.pos[v], mesh.normals[v])
    };
    let (p00, n00) = g(i0, j0);
    let (p10, n10) = g(i0 + 1, j0);
    let (p01, n01) = g(i0, j0 + 1);
    let (p11, n11) = g(i0 + 1, j0 + 1);

    let p0 = p00 * (1.0 - fx) + p10 * fx;
    let p1 = p01 * (1.0 - fx) + p11 * fx;
    let pos = p0 * (1.0 - fy) + p1 * fy;

    let n0 = n00 * (1.0 - fx) + n10 * fx;
    let n1 = n01 * (1.0 - fx) + n11 * fx;
    let nrm = (n0 * (1.0 - fy) + n1 * fy).normalized().unwrap_or(Vec3::new(0.0, 0.0, -1.0));

    let du = (p10 - p00) * (1.0 - fy) + (p11 - p01) * fy;
    let dv = (p01 - p00) * (1.0 - fx) + (p11 - p10) * fx;
    // Project tangents orthogonal to smooth normal.
    let tu_raw = du - nrm * du.dot(nrm);
    let tu = tu_raw.normalized().unwrap_or(Vec3::new(1.0, 0.0, 0.0));
    let tv_raw = dv - nrm * dv.dot(nrm);
    let tv = tv_raw.normalized().unwrap_or_else(|| nrm.cross(tu).normalized().unwrap_or(Vec3::new(0.0, 1.0, 0.0)));
    (pos, nrm, tu, tv)
}
