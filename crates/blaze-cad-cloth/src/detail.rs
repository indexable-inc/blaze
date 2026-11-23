//! Post-drape Blender-free garment tailoring and assembly:
//! 1. **Flat-felled seam trench relief**: subtly indents sewn seam lines (`0.6–1.2 mm`) so
//!    hardware path-tracing catches crisp, realistic stitched seam shadows.
//! 2. **Rim-aware physical fabric solidify**: gives open collars, cuffs, plackets, and hems real
//!    physical sheet thickness (`1.2–2.2 mm`) and optional contrasting inner lining (`mat_cordura`),
//!    without creating heavy double-sided inner walls across occluded areas.
//! 3. **Parametric 3D pockets, flaps & storm plackets**: constructed directly from the single-sided
//!    draped outer panel surface via `sample_panel_frame(s, t)` along smooth outward normals —
//!    completely eliminating shrinkwrap origami!
//! 4. **Rigid CAD hardware mounting**: places snap buttons, zipper rails/pulls, and Cobra belt
//!    buckles rigidly in the draped surface tangent frame `(T, B, N)`, tagged with a single rigid
//!    anchor point so animation never stretches or shears metal hardware!

#![blaze::sans_io]

use blaze_math::{Vec2, Vec3, cos, sin};
use blaze_own::{Alloc, AllocFailure, Vec};

use crate::grain::FabricGrain;
use crate::mesh2d::{ClothMesh, PanelSlice, recompute_normals, sample_panel_frame};
use crate::pattern::{HardwareKind, HardwareMount, PatternSet, SurfacePatchSpec};

/// Semantic kind of a named garment panel or tailoring detail inside [`GarmentMesh`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PanelKind {
    PatternPiece,
    RimLining,
    SurfacePatch,
    Hardware,
}

impl PanelKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PatternPiece => "pattern_piece",
            Self::RimLining => "rim_lining",
            Self::SurfacePatch => "surface_patch",
            Self::Hardware => "hardware",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "pattern_piece" => Some(Self::PatternPiece),
            "rim_lining" => Some(Self::RimLining),
            "surface_patch" => Some(Self::SurfacePatch),
            "hardware" => Some(Self::Hardware),
            _ => None,
        }
    }
}

/// Metadata for one named garment panel or tailoring detail inside [`GarmentMesh`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PanelMeta {
    pub id: u16,
    pub name: &'static str,
    pub kind: PanelKind,
    pub layer: u8,
    pub fabric_idx: u16,
    /// Optional host pattern piece name (for pockets, flaps, rim linings, hardware) so clicking
    /// a detail jumps straight to both the patch AND its parent CAD pattern piece!
    pub host_panel: Option<&'static str>,
}

/// Final render-ready garment mesh with UVs, smooth normals, baked cloth intelligence (`COLOR_0`),
/// material roles, grain-aligned physical rest coordinates (`rest_pos_m`), named panel metadata,
/// and optional rigid skinning anchor points (for metal hardware / buckles).
///
/// `colors` RGBA8 semantics (consumed directly by ASTRA's `human.slang` and `skin.slang`):
/// - `R`: fold-ridge & seam wear / edge desaturation (`0..255`)
/// - `G`: baked cloth micro-AO (`255` = open, lower inside deep fold troughs, seams & flap undercuts)
/// - `B`: contrast topstitch thread & bar-tack mask (`0..255`)
/// - `A`: cloth live-dynamics mobility (`0` = rigid hardware/anchored, `255` = free draped hem/folds)
pub struct GarmentMesh<'a> {
    pub positions: Vec<Vec3, Alloc<'a>>,
    pub normals: Vec<Vec3, Alloc<'a>>,
    pub uvs: Vec<Vec2, Alloc<'a>>,
    /// Undeformed 2D physical pattern coordinates in **metres** aligned to the fabric's
    /// `(weft, warp)` grain frame (rotated by `-grain_angle_rad` so `+x` = weft, `+y` = warp) —
    /// strictly independent of texture UVs (`uvs` / `TEXCOORD_0`)!
    pub rest_pos_m: Vec<Vec2, Alloc<'a>>,
    /// Baked per-vertex cloth intelligence (`COLOR_0`: `[wear, micro_ao, topstitch, mobility]`).
    pub colors: Vec<[u8; 4], Alloc<'a>>,
    /// Material/fabric index into `PatternSet::fabrics`.
    pub tri_fabric: Vec<u16, Alloc<'a>>,
    pub triangles: Vec<[u32; 3], Alloc<'a>>,
    /// If `Some(anchor)`, skinning samples weights at `anchor` uniformly (rigid hardware);
    /// if `None`, skinning samples smoothly at the vertex's own position + Laplacian smooths.
    pub rigid_anchor: Vec<Option<Vec3>, Alloc<'a>>,
    /// Garment dressing layer (`0` = undershirt, `1` = trousers, `2` = jacket, `3` = trim/hardware).
    pub tri_layer: Vec<u8, Alloc<'a>>,
    /// Named garment panels, rim linings, surface patches, and hardware pieces.
    pub panels: Vec<PanelMeta, Alloc<'a>>,
    /// Per-triangle panel ID indexing into `panels`.
    pub tri_panel: Vec<u16, Alloc<'a>>,
    /// Canonical topological vertex representative (`weld_rep[v] <= v`) linking UV-split sewn seam
    /// vertices into one continuous watertight manifold across Loop subdivision, skinning, normal
    /// computation, curvature baking, PSD morph deltas, and automated seam integrity checks!
    pub weld_rep: Vec<u32, Alloc<'a>>,
}

#[inline]
fn push<T>(v: &mut Vec<T, Alloc<'_>>, x: T) -> Result<(), AllocFailure> {
    v.try_push(x).map_err(|e| e.why)
}

impl<'a> GarmentMesh<'a> {
    pub fn new(alloc: Alloc<'a>) -> Self {
        Self {
            positions: Vec::new_in(alloc),
            normals: Vec::new_in(alloc),
            uvs: Vec::new_in(alloc),
            rest_pos_m: Vec::new_in(alloc),
            colors: Vec::new_in(alloc),
            tri_fabric: Vec::new_in(alloc),
            triangles: Vec::new_in(alloc),
            rigid_anchor: Vec::new_in(alloc),
            tri_layer: Vec::new_in(alloc),
            panels: Vec::new_in(alloc),
            tri_panel: Vec::new_in(alloc),
            weld_rep: Vec::new_in(alloc),
        }
    }

    /// Register a new named panel in [`GarmentMesh::panels`] and return its assigned panel ID.
    pub fn register_panel(
        &mut self,
        name: &'static str,
        kind: PanelKind,
        layer: u8,
        fabric_idx: u16,
        host_panel: Option<&'static str>,
    ) -> Result<u16, AllocFailure> {
        let id = self.panels.len() as u16;
        push(&mut self.panels, PanelMeta { id, name, kind, layer, fabric_idx, host_panel })?;
        Ok(id)
    }

    #[inline]
    pub fn weld_of(&self, v: u32) -> u32 {
        self.weld_rep.get(v as usize).copied().unwrap_or(v)
    }

    /// Append a draped [`ClothMesh`] with flat-felled seam trench relief, rim-aware solidify
    /// (clean physical thickness on hems/collars/cuffs), and all attached 3D surface patches
    /// (pockets, flaps, plackets) & rigid hardware mounts defined in `patterns`.
    pub fn append_draped_layer(
        &mut self,
        cloth: &ClothMesh<'_>,
        patterns: &PatternSet<'_>,
        solidify_rim_rings: u32,
        scratch: Alloc<'_>,
    ) -> Result<(), AllocFailure> {
        let n_src = cloth.pos.len();
        let base_v = self.positions.len() as u32;

        // Register one PatternPiece PanelMeta per PanelSlice and cache each panel's FabricGrain.
        let mut panel_id_map = Vec::new_in(scratch);
        let mut panel_grains = Vec::new_in(scratch);
        for pslice in cloth.panels.iter() {
            let pid = self.register_panel(
                pslice.name,
                PanelKind::PatternPiece,
                pslice.layer,
                pslice.fabric_idx as u16,
                None,
            )?;
            push(&mut panel_id_map, pid)?;
            let grain = patterns
                .find_panel(pslice.name)
                .map(|(_, spec)| spec.grain)
                .unwrap_or(FabricGrain::STRAIGHT);
            push(&mut panel_grains, grain)?;
        }

        // 1. Copy outer surface vertices with flat-felled seam trench relief + subtle deterministic
        //    seam micro-puckering evaluated strictly on the canonical welded root `r` so UV-split
        //    left/right seam twins stay bit-for-bit coincident (`0.000 mm` gap)!
        for i in 0..n_src {
            let r = cloth.weld_rep[i] as usize;
            let nrm = cloth.normals[r];
            let trench = cloth.seam_depth_m[r].max(cloth.seam_depth_m[i]);
            let uv_m_pt = cloth.rest_uv_m[i];
            let uv_m_rep = cloth.rest_uv_m[r];
            // Evaluate pucker strictly from canonical root `uv_m_rep` so both sides of a sewn seam
            // displace identically (never opposite signs!).
            let pucker = if trench > 1e-5 {
                0.00035 * sin((uv_m_rep.x + uv_m_rep.y) * 390.0) - 0.00025
            } else {
                0.0
            };
            let p = cloth.pos[r] - nrm * (trench - pucker);
            let fab = patterns.fabrics.get(cloth.vert_fabric[i] as usize);
            let uv_m = fab.map(|f| f.uv_meters.as_m()).unwrap_or(0.25).max(1e-4);
            let grain = panel_grains
                .get(cloth.vert_panel[i] as usize)
                .copied()
                .unwrap_or(FabricGrain::STRAIGHT);
            let uv = grain.to_texture_uv(uv_m_pt, uv_m);
            let rest_grain = grain.to_grain_rest(uv_m_pt);

            let is_seam = trench > 1e-5;
            let is_open = cloth.is_open_border[r] && cloth.is_open_border[i];
            let wear_u8: u8 = if is_seam || is_open { 105 } else { 20 };
            let ao_u8: u8 = if is_seam { 175 } else { 255 };
            let stitch_u8: u8 = if is_seam { 242 } else if is_open { 185 } else { 0 };
            let mob_u8: u8 = if cloth.inv_mass[r] <= 0.0 { 0 } else if is_open { 220 } else { 145 };

            push(&mut self.positions, p)?;
            push(&mut self.normals, nrm)?;
            push(&mut self.uvs, uv)?;
            push(&mut self.rest_pos_m, rest_grain)?;
            push(&mut self.colors, [wear_u8, ao_u8, stitch_u8, mob_u8])?;
            push(&mut self.rigid_anchor, None)?;
            push(&mut self.weld_rep, base_v + (r as u32))?;
        }

        for (ti, &[a, b, c]) in cloth.triangles.iter().enumerate() {
            let local_p = cloth.tri_panel[ti] as usize;
            let layer = cloth.panels[local_p].layer;
            let pid = panel_id_map[local_p];
            push(&mut self.triangles, [base_v + a, base_v + b, base_v + c])?;
            push(&mut self.tri_fabric, cloth.tri_fabric[ti])?;
            push(&mut self.tri_layer, layer)?;
            push(&mut self.tri_panel, pid)?;
        }

        // 2. Rim-aware solidify: build a clean physical thickness rim + shallow inner facing
        //    (only `solidify_rim_rings` grid rings inward from open hems/cuffs/collar edges!) so
        //    open edges show crisp physical thickness and contrasting inner lining with ZERO inner
        //    poke-through across the body!
        if solidify_rim_rings > 0 {
            self.append_rim_solidify(cloth, patterns, &panel_grains, solidify_rim_rings, scratch)?;
        }

        // 3. Build 3D pockets, flaps, storm plackets cleanly from the single-sided outer surface.
        for patch in patterns.patches.iter() {
            if let Some(pslice) = cloth.panels.iter().find(|p| p.name == patch.panel) {
                self.append_surface_patch(cloth, pslice, patch, patterns)?;
            }
        }

        // 4. Mount rigid hardware (snaps, zipper pull/rail, belt buckle) in draped (T, B, N) frames.
        for hw in patterns.hardware.iter() {
            if let Some(pslice) = cloth.panels.iter().find(|p| p.name == hw.panel) {
                self.append_hardware(cloth, pslice, hw, patterns)?;
            }
        }

        Ok(())
    }

    fn append_rim_solidify(
        &mut self,
        cloth: &ClothMesh<'_>,
        patterns: &PatternSet<'_>,
        panel_grains: &[FabricGrain],
        rim_rings: u32,
        scratch: Alloc<'_>,
    ) -> Result<(), AllocFailure> {
        let n_src = cloth.pos.len();
        // Compute graph distance (in edge hops) from any open border vertex via deterministic BFS.
        let mut border_dist = Vec::new_in(scratch);
        for i in 0..n_src {
            let r = cloth.weld_rep[i] as usize;
            let d = if cloth.is_open_border[i] || cloth.is_open_border[r] { 0u32 } else { u32::MAX };
            push(&mut border_dist, d)?;
        }
        // Propagate `rim_rings` steps across triangles.
        for step in 0..rim_rings {
            for &[a, b, c] in cloth.triangles.iter() {
                let da = border_dist[a as usize];
                let db = border_dist[b as usize];
                let dc = border_dist[c as usize];
                let m = da.min(db).min(dc);
                if m == step {
                    let next = step + 1;
                    if border_dist[a as usize] > next { border_dist[a as usize] = next; }
                    if border_dist[b as usize] > next { border_dist[b as usize] = next; }
                    if border_dist[c as usize] > next { border_dist[c as usize] = next; }
                }
            }
        }

        // Create inner offset vertices only for vertices within `rim_rings` of an open edge.
        let mut inner_map = Vec::new_in(scratch);
        for _ in 0..n_src {
            push(&mut inner_map, u32::MAX)?;
        }
        let mut outer_map = Vec::new_in(scratch);
        for _ in 0..n_src {
            push(&mut outer_map, u32::MAX)?;
        }

        for i in 0..n_src {
            if border_dist[i] <= rim_rings {
                let r = cloth.weld_rep[i] as usize;
                let nrm = cloth.normals[r];
                let fab = patterns.fabrics.get(cloth.vert_fabric[i] as usize);
                let thick = fab.map(|f| f.thickness.as_m()).unwrap_or(0.0016);
                let uv_m = fab.map(|f| f.uv_meters.as_m()).unwrap_or(0.25).max(1e-4);
                let grain = panel_grains
                    .get(cloth.vert_panel[i] as usize)
                    .copied()
                    .unwrap_or(FabricGrain::STRAIGHT);
                let uv = grain.to_texture_uv(cloth.rest_uv_m[i], uv_m);
                let rest_grain = grain.to_grain_rest(cloth.rest_uv_m[i]);
                let p_out = cloth.pos[r] - nrm * cloth.seam_depth_m[i];
                // Taper inner thickness smoothly to 0 at the inner rim edge so there is no step!
                let taper = 1.0 - (border_dist[i] as f64 / ((rim_rings + 1) as f64)) * 0.35;
                let p_in = p_out - nrm * (thick * taper);

                let oi = self.positions.len() as u32;
                push(&mut self.positions, p_out)?;
                push(&mut self.normals, nrm)?;
                push(&mut self.uvs, uv)?;
                push(&mut self.rest_pos_m, rest_grain)?;
                push(&mut self.colors, [110, 235, 160, 180])?;
                push(&mut self.rigid_anchor, None)?;
                push(&mut self.weld_rep, oi)?;
                outer_map[i] = oi;

                let ii = self.positions.len() as u32;
                push(&mut self.positions, p_in)?;
                push(&mut self.normals, -nrm)?;
                push(&mut self.uvs, uv)?;
                push(&mut self.rest_pos_m, rest_grain)?;
                push(&mut self.colors, [30, 125, 0, 160])?;
                push(&mut self.rigid_anchor, None)?;
                push(&mut self.weld_rep, ii)?;
                inner_map[i] = ii;
            }
        }

        // Lazily register one RimLining PanelMeta per panel that emits rim-solidified triangles.
        let mut rim_panel_ids = Vec::new_in(scratch);
        for _ in 0..cloth.panels.len() {
            push(&mut rim_panel_ids, u16::MAX)?;
        }

        // Emit reversed inner-facing triangles within `rim_rings` of open borders.
        for (ti, &[a, b, c]) in cloth.triangles.iter().enumerate() {
            let ia = inner_map[a as usize];
            let ib = inner_map[b as usize];
            let ic = inner_map[c as usize];
            if ia != u32::MAX && ib != u32::MAX && ic != u32::MAX {
                let p_idx = cloth.tri_panel[ti] as usize;
                let panel = cloth.panels[p_idx];
                let inner_fab = panel.lining_fabric_idx.unwrap_or(panel.fabric_idx) as u16;
                let rim_pid = if rim_panel_ids[p_idx] == u16::MAX {
                    let id = self.register_panel(
                        panel.name,
                        PanelKind::RimLining,
                        panel.layer,
                        inner_fab,
                        Some(panel.name),
                    )?;
                    rim_panel_ids[p_idx] = id;
                    id
                } else {
                    rim_panel_ids[p_idx]
                };
                push(&mut self.triangles, [ia, ic, ib])?;
                push(&mut self.tri_fabric, inner_fab)?;
                push(&mut self.tri_layer, panel.layer)?;
                push(&mut self.tri_panel, rim_pid)?;
            }
        }

        // Stitch open boundary edges with clean bevelled rim quads connecting outer -> inner.
        for (p_idx, panel) in cloth.panels.iter().enumerate() {
            let fab = panel.fabric_idx as u16;
            let layer = panel.layer;
            let inner_fab = panel.lining_fabric_idx.unwrap_or(panel.fabric_idx) as u16;
            // Walk all 4 borders in CCW outer order.
            let mut stitch_edge = |u: u32, v: u32, s: &mut Self| -> Result<(), AllocFailure> {
                if !cloth.is_open_border[u as usize] || !cloth.is_open_border[v as usize] {
                    return Ok(());
                }
                let ou = outer_map[u as usize];
                let ov = outer_map[v as usize];
                let iu = inner_map[u as usize];
                let iv = inner_map[v as usize];
                if ou == u32::MAX || ov == u32::MAX || iu == u32::MAX || iv == u32::MAX {
                    return Ok(());
                }
                let rim_pid = if rim_panel_ids[p_idx] == u16::MAX {
                    let id = s.register_panel(
                        panel.name,
                        PanelKind::RimLining,
                        layer,
                        inner_fab,
                        Some(panel.name),
                    )?;
                    rim_panel_ids[p_idx] = id;
                    id
                } else {
                    rim_panel_ids[p_idx]
                };
                let (e0, e1) = if !panel.flip_normal { (u, v) } else { (v, u) };
                let (oe0, oe1, ie0, ie1) = (outer_map[e0 as usize], outer_map[e1 as usize], inner_map[e0 as usize], inner_map[e1 as usize]);
                push(&mut s.triangles, [oe0, ie0, ie1])?;
                push(&mut s.tri_fabric, fab)?;
                push(&mut s.tri_layer, layer)?;
                push(&mut s.tri_panel, rim_pid)?;
                push(&mut s.triangles, [oe0, ie1, oe1])?;
                push(&mut s.tri_fabric, fab)?;
                push(&mut s.tri_layer, layer)?;
                push(&mut s.tri_panel, rim_pid)?;
                Ok(())
            };

            for i in 0..panel.nu {
                stitch_edge(panel.grid_vertex(i, 0), panel.grid_vertex(i + 1, 0), self)?;
                stitch_edge(panel.grid_vertex(panel.nu - i, panel.nv), panel.grid_vertex(panel.nu - i - 1, panel.nv), self)?;
            }
            for j in 0..panel.nv {
                stitch_edge(panel.grid_vertex(panel.nu, j), panel.grid_vertex(panel.nu, j + 1), self)?;
                stitch_edge(panel.grid_vertex(0, panel.nv - j), panel.grid_vertex(0, panel.nv - j - 1), self)?;
            }
        }
        Ok(())
    }

    /// Build a crisp, architectural 3D bellows pocket (with optional central vertical box-pleat,
    /// sharp folded gusset side walls, and a real detached overhanging flap casting a sharp shadow
    /// undercut gap!) OR a flat stitched storm placket / Cordura reinforcement panel — ZERO rubber
    /// pillow blobs!
    fn append_surface_patch(
        &mut self,
        cloth: &ClothMesh<'_>,
        panel: &PanelSlice,
        spec: &SurfacePatchSpec,
        patterns: &PatternSet<'_>,
    ) -> Result<(), AllocFailure> {
        let depth_m = spec.depth.as_m();
        let crown_m = spec.crown.as_m();
        let fab_idx = spec.fabric_idx as u16;
        let uv_m = patterns.fabrics.get(spec.fabric_idx).map(|f| f.uv_meters.as_m()).unwrap_or(0.25).max(1e-4);
        let body_pid = self.register_panel(spec.name, PanelKind::SurfacePatch, 3, fab_idx, Some(panel.name))?;

        // Emit crisp architectural pocket body / flat stitched panel (sharp split-normal gusset walls!).
        let body_t_max = if spec.flap_fraction > 0.01 {
            spec.t_max - (spec.t_max - spec.t_min) * (spec.flap_fraction * 0.25)
        } else {
            spec.t_max
        };
        self.emit_tailored_panel(
            cloth,
            panel,
            spec.s_min,
            spec.s_max,
            spec.t_min,
            body_t_max,
            0.0022,
            depth_m + 0.0020,
            crown_m,
            spec.bottom_chamfer,
            spec.box_pleat,
            false,
            fab_idx,
            uv_m,
            3,
            body_pid,
        )?;

        // Optional detached overhanging pocket flap with real 3.5 mm undercut shadow gap!
        if spec.flap_fraction > 0.01 {
            let flap_fab = spec.flap_fabric_idx.unwrap_or(spec.fabric_idx) as u16;
            let flap_pid = if flap_fab == fab_idx {
                body_pid
            } else {
                self.register_panel(spec.name, PanelKind::SurfacePatch, 3, flap_fab, Some(panel.name))?
            };
            let flap_uv_m = patterns
                .fabrics
                .get(flap_fab as usize)
                .map(|f| f.uv_meters.as_m())
                .unwrap_or(0.25)
                .max(1e-4);
            let flap_t_min = spec.t_max - (spec.t_max - spec.t_min) * spec.flap_fraction;
            let flap_over_s = 0.018 * (spec.s_max - spec.s_min);
            self.emit_tailored_panel(
                cloth,
                panel,
                spec.s_min - flap_over_s,
                spec.s_max + flap_over_s,
                flap_t_min,
                spec.t_max + 0.006 * (spec.t_max - spec.t_min),
                depth_m + 0.0018,
                depth_m + 0.0052,
                crown_m * 0.20,
                spec.bottom_chamfer * 1.15,
                false,
                true,
                flap_fab,
                flap_uv_m,
                3,
                flap_pid,
            )?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_tailored_panel(
        &mut self,
        cloth: &ClothMesh<'_>,
        panel: &PanelSlice,
        s_min: f64,
        s_max: f64,
        t_min: f64,
        t_max: f64,
        base_lift_m: f64,
        top_lift_m: f64,
        crown_m: f64,
        bottom_chamfer: f64,
        box_pleat: bool,
        is_flap: bool,
        fabric_idx: u16,
        uv_m: f64,
        layer: u8,
        panel_id: u16,
    ) -> Result<(), AllocFailure> {
        const NU: usize = 12;
        const NV: usize = 12;
        let mut grid_top = [[0u32; NU + 1]; NV + 1];
        let mut grid_bot = [[0u32; NU + 1]; NV + 1];
        let mut top_pos = [[Vec3::ZERO; NU + 1]; NV + 1];
        let mut bot_pos = [[Vec3::ZERO; NU + 1]; NV + 1];
        let mut surf_nrm = [[Vec3::new(0.0, 0.0, -1.0); NU + 1]; NV + 1];
        let mut grid_uv = [[Vec2::new(0.0, 0.0); NU + 1]; NV + 1];
        let mut grid_rest = [[Vec2::new(0.0, 0.0); NU + 1]; NV + 1];

        for j in 0..=NV {
            let fv = (j as f64) / (NV as f64);
            for i in 0..=NU {
                let fu = (i as f64) / (NU as f64);
                let mut su = fu;
                let sv = fv;
                if bottom_chamfer > 0.01 && fv < bottom_chamfer {
                    let inset = (bottom_chamfer - fv) * 0.85;
                    su = su.clamp(inset, 1.0 - inset);
                }
                let s = (s_min + (s_max - s_min) * su).clamp(0.0, 1.0);
                let tt = (t_min + (t_max - t_min) * sv).clamp(0.0, 1.0);
                let (pos, nrm, _, _) = sample_panel_frame(cloth, panel, s, tt);

                // Taut architectural canvas face (NOT a rubber balloon!):
                // - Subtle flat-topped bevel right at the stitched perimeter (5% inset)
                // - Optional crisp central military box-pleat channel (`fu` near 0.5)
                // - For detached flaps: hinges cleanly from the top stitch line (`fv = 1.0`) and
                //   hovers with a crisp air gap at the lower lip (`fv = 0.0`) casting a real shadow!
                let mut h_top;
                let mut h_bot = base_lift_m;
                if is_flap {
                    // Hinge smoothly from garment surface at top (`fv = 1.0`) out over pocket body!
                    let hinge = (1.0 - fv).clamp(0.0, 1.0);
                    let arch = 1.0 - (1.0 - hinge) * (1.0 - hinge) * (1.0 - hinge);
                    h_top = 0.0024 + (top_lift_m - 0.0024) * arch;
                    // Underside of flap stays 2.2 mm below outer flap surface -> real crisp hanging lip!
                    h_bot = (h_top - 0.0022).max(0.0018);
                } else {
                    // Subtle center crown (very restrained: crisp canvas, not marshmallow).
                    let bulge = sin(fu * core::f64::consts::PI) * sin(fv * core::f64::consts::PI);
                    h_top = top_lift_m + crown_m * 0.45 * bulge;
                    // Crisp stitched border tuck right on the perimeter ring (`i==0||i==NU||j==0||j==NV`).
                    if i == 0 || i == NU || j == 0 || j == NV {
                        h_top -= (top_lift_m - base_lift_m) * 0.12;
                    }
                    // Optional real military central vertical box-pleat fold!
                    if box_pleat {
                        let d_center = (fu - 0.5).abs();
                        if d_center < 0.14 {
                            let groove = 1.0 - (d_center / 0.14);
                            let crisp = groove * groove;
                            h_top -= (top_lift_m - base_lift_m) * 0.42 * crisp;
                        }
                    }
                }

                let rest_m = Vec2::new((s - s_min) * 0.45, (tt - t_min) * 0.45);
                let uv = rest_m * (1.0 / uv_m);
                top_pos[j][i] = pos + nrm * h_top;
                bot_pos[j][i] = pos + nrm * h_bot;
                surf_nrm[j][i] = nrm;
                grid_uv[j][i] = uv;
                grid_rest[j][i] = rest_m;

                let is_perimeter = i == 0 || i == NU || j == 0 || j == NV;
                let is_stitch_ring = i == 1 || i + 1 == NU || j == 1 || j + 1 == NV;
                let in_pleat_trough = box_pleat && (fu - 0.5).abs() < 0.12;
                let under_flap_shadow = !is_flap && fv > 0.68;
                let top_wear: u8 = if is_perimeter { 155 } else if is_flap && j <= 1 { 170 } else { 45 };
                let top_ao: u8 = if in_pleat_trough { 130 } else if under_flap_shadow { 115 } else if is_perimeter { 190 } else { 250 };
                let top_stitch: u8 = if is_stitch_ring { 235 } else { 0 };

                let v_top = self.positions.len() as u32;
                push(&mut self.positions, top_pos[j][i])?;
                push(&mut self.normals, nrm)?;
                push(&mut self.uvs, uv)?;
                push(&mut self.rest_pos_m, rest_m)?;
                push(&mut self.colors, [top_wear, top_ao, top_stitch, 40])?;
                push(&mut self.rigid_anchor, None)?;
                push(&mut self.weld_rep, v_top)?;
                grid_top[j][i] = v_top;

                if is_flap || is_perimeter {
                    let v_bot = self.positions.len() as u32;
                    push(&mut self.positions, bot_pos[j][i])?;
                    push(&mut self.normals, -nrm)?;
                    push(&mut self.uvs, uv)?;
                    push(&mut self.rest_pos_m, rest_m)?;
                    push(&mut self.colors, [20, 95, 0, 40])?;
                    push(&mut self.rigid_anchor, None)?;
                    push(&mut self.weld_rep, v_bot)?;
                    grid_bot[j][i] = v_bot;
                }
            }
        }

        // Top face quads (smoothly shaded across face, crisp crease at perimeter!).
        for j in 0..NV {
            for i in 0..NU {
                let a = grid_top[j][i];
                let b = grid_top[j][i + 1];
                let c = grid_top[j + 1][i + 1];
                let d = grid_top[j + 1][i];
                let flip = (top_pos[j][i + 1] - top_pos[j][i])
                    .cross(top_pos[j + 1][i + 1] - top_pos[j][i])
                    .dot(surf_nrm[j][i])
                    < 0.0;
                if !flip {
                    push(&mut self.triangles, [a, b, c])?;
                    push(&mut self.triangles, [a, c, d])?;
                } else {
                    push(&mut self.triangles, [a, c, b])?;
                    push(&mut self.triangles, [a, d, c])?;
                }
                push(&mut self.tri_fabric, fabric_idx)?;
                push(&mut self.tri_fabric, fabric_idx)?;
                push(&mut self.tri_layer, layer)?;
                push(&mut self.tri_layer, layer)?;
                push(&mut self.tri_panel, panel_id)?;
                push(&mut self.tri_panel, panel_id)?;

                // For detached overhanging flaps, emit the underside face too so camera angles
                // looking slightly upward see a solid crisp flap lip + real dark undercut shadow!
                if is_flap {
                    let ba = grid_bot[j][i];
                    let bb = grid_bot[j][i + 1];
                    let bc = grid_bot[j + 1][i + 1];
                    let bd = grid_bot[j + 1][i];
                    if !flip {
                        push(&mut self.triangles, [ba, bc, bb])?;
                        push(&mut self.triangles, [ba, bd, bc])?;
                    } else {
                        push(&mut self.triangles, [ba, bb, bc])?;
                        push(&mut self.triangles, [ba, bc, bd])?;
                    }
                    push(&mut self.tri_fabric, fabric_idx)?;
                    push(&mut self.tri_fabric, fabric_idx)?;
                    push(&mut self.tri_layer, layer)?;
                    push(&mut self.tri_layer, layer)?;
                    push(&mut self.tri_panel, panel_id)?;
                    push(&mut self.tri_panel, panel_id)?;
                }
            }
        }

        // Sharp split-normal folded gusset side walls (creates crisp 85° tailored pocket edges
        // instead of smooth marshmallow blobs!).
        let emit_crisp_wall = |p_t0: Vec3, p_t1: Vec3, p_b0: Vec3, p_b1: Vec3, uv0: Vec2, uv1: Vec2, r0: Vec2, r1: Vec2, s: &mut Self| -> Result<(), AllocFailure> {
            let wn = (p_b1 - p_b0).cross(p_t1 - p_b0).normalized().unwrap_or(Vec3::new(1.0, 0.0, 0.0));
            let base = s.positions.len() as u32;
            for (k, (p, uv, rm)) in [(p_b0, uv0, r0), (p_b1, uv1, r1), (p_t1, uv1, r1), (p_t0, uv0, r0)].into_iter().enumerate() {
                // Darken base crease along stitch line (`k < 2`), subtle edge wear along fold top (`k >= 2`).
                let c = if k < 2 { [40, 120, 190, 40] } else { [145, 220, 0, 40] };
                let vi = s.positions.len() as u32;
                push(&mut s.positions, p)?;
                push(&mut s.normals, wn)?;
                push(&mut s.uvs, uv)?;
                push(&mut s.rest_pos_m, rm)?;
                push(&mut s.colors, c)?;
                push(&mut s.rigid_anchor, None)?;
                push(&mut s.weld_rep, vi)?;
            }
            push(&mut s.triangles, [base, base + 1, base + 2])?;
            push(&mut s.triangles, [base, base + 2, base + 3])?;
            push(&mut s.tri_fabric, fabric_idx)?;
            push(&mut s.tri_fabric, fabric_idx)?;
            push(&mut s.tri_layer, layer)?;
            push(&mut s.tri_layer, layer)?;
            push(&mut s.tri_panel, panel_id)?;
            push(&mut s.tri_panel, panel_id)?;
            Ok(())
        };
        for i in 0..NU {
            emit_crisp_wall(top_pos[0][i], top_pos[0][i + 1], bot_pos[0][i], bot_pos[0][i + 1], grid_uv[0][i], grid_uv[0][i + 1], grid_rest[0][i], grid_rest[0][i + 1], self)?;
            emit_crisp_wall(top_pos[NV][i + 1], top_pos[NV][i], bot_pos[NV][i + 1], bot_pos[NV][i], grid_uv[NV][i + 1], grid_uv[NV][i], grid_rest[NV][i + 1], grid_rest[NV][i], self)?;
        }
        for j in 0..NV {
            emit_crisp_wall(top_pos[j][NU], top_pos[j + 1][NU], bot_pos[j][NU], bot_pos[j + 1][NU], grid_uv[j][NU], grid_uv[j + 1][NU], grid_rest[j][NU], grid_rest[j + 1][NU], self)?;
            emit_crisp_wall(top_pos[j + 1][0], top_pos[j][0], bot_pos[j + 1][0], bot_pos[j][0], grid_uv[j + 1][0], grid_uv[j][0], grid_rest[j + 1][0], grid_rest[j][0], self)?;
        }
        Ok(())
    }

    /// Mount rigid CAD hardware (snap button, zipper pull, buckle, zipper rail) cleanly in the
    /// local tangent frame `(T, B, N)` of the draped panel, with a single rigid anchor point so
    /// skinning moves it rigidly without stretching!
    fn append_hardware(
        &mut self,
        cloth: &ClothMesh<'_>,
        panel: &PanelSlice,
        hw: &HardwareMount,
        _patterns: &PatternSet<'_>,
    ) -> Result<(), AllocFailure> {
        let (pos, nrm, tu, tv) = sample_panel_frame(cloth, panel, hw.s, hw.t);
        let origin = pos + nrm * hw.normal_offset.as_m();
        let fab = hw.fabric_idx as u16;
        let anchor = Some(pos);
        let hw_pid = self.register_panel(hw.name, PanelKind::Hardware, 3, fab, Some(panel.name))?;

        match hw.kind {
            HardwareKind::SnapButton { radius, height } => {
                let r = radius.as_m();
                let h = height.as_m();
                const SEGS: usize = 12;
                let center_top = self.positions.len() as u32;
                push(&mut self.positions, origin + nrm * (h * 1.15))?;
                push(&mut self.normals, nrm)?;
                push(&mut self.uvs, Vec2::new(0.5, 0.5))?;
                push(&mut self.rest_pos_m, Vec2::new(0.0, 0.0))?;
                push(&mut self.colors, [80, 255, 0, 0])?;
                push(&mut self.rigid_anchor, anchor)?;
                push(&mut self.weld_rep, center_top)?;

                let ring_top_start = self.positions.len() as u32;
                for k in 0..SEGS {
                    let ang = (k as f64) * core::f64::consts::TAU / (SEGS as f64);
                    let dir = tu * cos(ang) + tv * sin(ang);
                    let vi = self.positions.len() as u32;
                    push(&mut self.positions, origin + dir * (r * 0.88) + nrm * h)?;
                    push(&mut self.normals, (nrm + dir * 0.3).normalized().unwrap_or(nrm))?;
                    push(&mut self.uvs, Vec2::new(0.5 + 0.4 * cos(ang), 0.5 + 0.4 * sin(ang)))?;
                    push(&mut self.rest_pos_m, Vec2::new(0.0, 0.0))?;
                    push(&mut self.colors, [140, 240, 0, 0])?;
                    push(&mut self.rigid_anchor, anchor)?;
                    push(&mut self.weld_rep, vi)?;
                }
                let ring_bot_start = self.positions.len() as u32;
                for k in 0..SEGS {
                    let ang = (k as f64) * core::f64::consts::TAU / (SEGS as f64);
                    let dir = tu * cos(ang) + tv * sin(ang);
                    let vi = self.positions.len() as u32;
                    push(&mut self.positions, origin + dir * r)?;
                    push(&mut self.normals, dir)?;
                    push(&mut self.uvs, Vec2::new(0.5 + 0.5 * cos(ang), 0.5 + 0.5 * sin(ang)))?;
                    push(&mut self.rest_pos_m, Vec2::new(0.0, 0.0))?;
                    push(&mut self.colors, [40, 150, 0, 0])?;
                    push(&mut self.rigid_anchor, anchor)?;
                    push(&mut self.weld_rep, vi)?;
                }
                for k in 0..SEGS {
                    let kn = (k + 1) % SEGS;
                    let t0 = ring_top_start + k as u32;
                    let t1 = ring_top_start + kn as u32;
                    let b0 = ring_bot_start + k as u32;
                    let b1 = ring_bot_start + kn as u32;
                    push(&mut self.triangles, [center_top, t0, t1])?;
                    push(&mut self.triangles, [b0, b1, t1])?;
                    push(&mut self.triangles, [b0, t1, t0])?;
                    for _ in 0..3 {
                        push(&mut self.tri_fabric, fab)?;
                        push(&mut self.tri_layer, 3)?;
                        push(&mut self.tri_panel, hw_pid)?;
                    }
                }
            }
            HardwareKind::ZipperPull { width, length, thickness } => {
                self.emit_oriented_box(
                    origin + nrm * (thickness.as_m() * 0.5) - tv * (length.as_m() * 0.35),
                    tu,
                    tv,
                    nrm,
                    width.as_m() * 0.5,
                    length.as_m() * 0.5,
                    thickness.as_m() * 0.5,
                    fab,
                    anchor,
                    hw_pid,
                )?;
            }
            HardwareKind::Buckle { width, height, thickness } => {
                self.emit_oriented_box(
                    origin + nrm * (thickness.as_m() * 0.5),
                    tu,
                    tv,
                    nrm,
                    width.as_m() * 0.5,
                    height.as_m() * 0.5,
                    thickness.as_m() * 0.5,
                    fab,
                    anchor,
                    hw_pid,
                )?;
            }
            HardwareKind::ZipperRail { width, length_m, thickness } => {
                self.emit_oriented_box(
                    origin + nrm * (thickness.as_m() * 0.5),
                    tu,
                    tv,
                    nrm,
                    width.as_m() * 0.5,
                    length_m * 0.5,
                    thickness.as_m() * 0.5,
                    fab,
                    anchor,
                    hw_pid,
                )?;
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_oriented_box(
        &mut self,
        center: Vec3,
        ax: Vec3,
        ay: Vec3,
        az: Vec3,
        hx: f64,
        hy: f64,
        hz: f64,
        fab: u16,
        anchor: Option<Vec3>,
        panel_id: u16,
    ) -> Result<(), AllocFailure> {
        let base = self.positions.len() as u32;
        let corners = [
            [-1.0, -1.0, -1.0],
            [1.0, -1.0, -1.0],
            [1.0, 1.0, -1.0],
            [-1.0, 1.0, -1.0],
            [-1.0, -1.0, 1.0],
            [1.0, -1.0, 1.0],
            [1.0, 1.0, 1.0],
            [-1.0, 1.0, 1.0],
        ];
        for c in corners {
            let p = center + ax * (c[0] * hx) + ay * (c[1] * hy) + az * (c[2] * hz);
            let n = (ax * c[0] + ay * c[1] + az * c[2]).normalized().unwrap_or(az);
            let vi = self.positions.len() as u32;
            push(&mut self.positions, p)?;
            push(&mut self.normals, n)?;
            push(&mut self.uvs, Vec2::new(0.5 + 0.5 * c[0], 0.5 + 0.5 * c[1]))?;
            push(&mut self.rest_pos_m, Vec2::new(0.0, 0.0))?;
            push(&mut self.colors, [100, 235, 0, 0])?;
            push(&mut self.rigid_anchor, anchor)?;
            push(&mut self.weld_rep, vi)?;
        }
        let faces = [
            [4, 5, 6, 7], // +Z front
            [0, 3, 2, 1], // -Z back
            [1, 2, 6, 5], // +X right
            [0, 4, 7, 3], // -X left
            [3, 7, 6, 2], // +Y top
            [0, 1, 5, 4], // -Y bottom
        ];
        for f in faces {
            push(&mut self.triangles, [base + f[0], base + f[1], base + f[2]])?;
            push(&mut self.triangles, [base + f[0], base + f[2], base + f[3]])?;
            push(&mut self.tri_fabric, fab)?;
            push(&mut self.tri_fabric, fab)?;
            push(&mut self.tri_layer, 3)?;
            push(&mut self.tri_layer, 3)?;
            push(&mut self.tri_panel, panel_id)?;
            push(&mut self.tri_panel, panel_id)?;
        }
        Ok(())
    }

    /// Recompute smooth area-weighted normals over the assembled garment mesh, unifying welded
    /// UV-seam vertices (`weld_rep`) so normals flow continuously across every sewn seam!
    pub fn recompute_smooth_normals(&mut self) {
        let n_verts = self.positions.len();
        for n in self.normals.iter_mut() {
            *n = Vec3::ZERO;
        }
        for &[a, b, c] in self.triangles.iter() {
            let ra = self.weld_of(a) as usize;
            let rb = self.weld_of(b) as usize;
            let rc = self.weld_of(c) as usize;
            let pa = self.positions[ra];
            let pb = self.positions[rb];
            let pc = self.positions[rc];
            let fnrm = (pb - pa).cross(pc - pa);
            self.normals[ra] = self.normals[ra] + fnrm;
            self.normals[rb] = self.normals[rb] + fnrm;
            self.normals[rc] = self.normals[rc] + fnrm;
        }
        for i in 0..n_verts {
            let r = self.weld_of(i as u32) as usize;
            let n = self.normals[r];
            let l = n.length();
            self.normals[i] = if l > 1e-12 { n * (1.0 / l) } else { Vec3::new(0.0, 0.0, -1.0) };
        }
    }

    /// Bake physical fold-ridge wear (`R`) and fold-trough micro-AO (`G`) directly from the draped
    /// mesh's signed mean curvature (`dot(p_i - p_avg, normal_i)`) across welded seam topology!
    pub fn bake_curvature_wear_and_ao(&mut self, scratch: Alloc<'_>) -> Result<(), AllocFailure> {
        let n = self.positions.len();
        let mut nbr_sum = Vec::new_in(scratch);
        let mut nbr_cnt = Vec::new_in(scratch);
        for _ in 0..n {
            push(&mut nbr_sum, Vec3::ZERO)?;
            push(&mut nbr_cnt, 0u32)?;
        }
        for &[a, b, c] in self.triangles.iter() {
            let ra = self.weld_of(a) as usize;
            let rb = self.weld_of(b) as usize;
            let rc = self.weld_of(c) as usize;
            let pa = self.positions[ra];
            let pb = self.positions[rb];
            let pc = self.positions[rc];
            nbr_sum[ra] = nbr_sum[ra] + pb + pc;
            nbr_cnt[ra] += 2;
            nbr_sum[rb] = nbr_sum[rb] + pa + pc;
            nbr_cnt[rb] += 2;
            nbr_sum[rc] = nbr_sum[rc] + pa + pb;
            nbr_cnt[rc] += 2;
        }
        for i in 0..n {
            let r = self.weld_of(i as u32) as usize;
            if nbr_cnt[r] == 0 || self.rigid_anchor[i].is_some() {
                continue;
            }
            let avg = nbr_sum[r] * (1.0 / (nbr_cnt[r] as f64));
            let lap = self.positions[r] - avg;
            // Positive = convex fold ridge; Negative = concave fold trough!
            let signed_h = lap.dot(self.normals[r]);
            let cur = self.colors[i];
            let ridge_wear = ((signed_h * 450.0).clamp(0.0, 1.0) * 180.0) as u8;
            let trough_dark = (((-signed_h) * 520.0).clamp(0.0, 1.0) * 120.0) as u8;
            self.colors[i] = [
                cur[0].saturating_add(ridge_wear).min(240),
                cur[1].saturating_sub(trough_dark).max(75),
                cur[2],
                cur[3],
            ];
        }
        Ok(())
    }
}

/// Keep `recompute_normals` exported for callers adjusting vertex positions.
pub fn refresh_cloth_normals(cloth: &mut ClothMesh<'_>) {
    recompute_normals(cloth);
}
