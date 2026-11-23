//! Interactive ray-click panel inspector for named CAD garment pieces (`inspect_garment_ray`).
//!
//! Clicking any triangle on a draped or posed garment returns every intersected layer sorted
//! deterministically front-to-back, identifying:
//! - The `visible` surface panel, its material (`fabric_name`), and its CAD pattern piece name
//! - The triangle directly `underneath` along the viewing ray and their exact `separation`
//! - `layer_inversion` when an inner garment layer (e.g. undershirt or rim lining) pokes in front
//!   of an outer shell
//! - `source_pattern_piece` jumping directly from a clicked pocket, flap, rim lining, or hardware
//!   piece back to its parent CAD [`PanelSpec`](crate::pattern::PanelSpec) piece!

#![blaze::sans_io]

use blaze_math::{Vec2, Vec3};
use blaze_own::{Alloc, AllocFailure, Vec};
use blaze_units::Length;

use crate::detail::{GarmentMesh, PanelKind};
use crate::fabric::Fabric;

/// One ray-triangle intersection hit on a named garment panel.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct PanelHit {
    pub tri: u32,
    pub panel_id: u16,
    pub panel_name: &'static str,
    pub kind: PanelKind,
    pub host_panel: Option<&'static str>,
    pub fabric_idx: u16,
    pub fabric_name: &'static str,
    pub layer: u8,
    /// Distance along the normalized ray from `origin` to `position` in metres.
    pub depth_m: f64,
    pub position: Vec3,
    pub normal: Vec3,
    /// Interpolated grain-aligned physical rest coordinate `(weft_m, warp_m)` in metres.
    pub rest_pos_m: Vec2,
    pub barycentric: [f64; 3],
}

/// Complete ray-click inspection report identifying visible material, triangle underneath,
/// separation distance, layer inversion, and jump-to CAD pattern piece name.
pub struct PanelClickReport<'a> {
    pub hits: Vec<PanelHit, Alloc<'a>>,
    pub visible: Option<PanelHit>,
    pub underneath: Option<PanelHit>,
    pub separation: Option<Length>,
    /// `true` when `underneath.layer > visible.layer` (an inner layer or lining poked in front of
    /// an outer garment shell!).
    pub layer_inversion: bool,
    /// Parent CAD pattern piece name (`visible.host_panel.unwrap_or(visible.panel_name)`).
    pub source_pattern_piece: Option<&'static str>,
}

#[inline]
fn push<T>(v: &mut Vec<T, Alloc<'_>>, x: T) -> Result<(), AllocFailure> {
    v.try_push(x).map_err(|e| e.why)
}

/// Cast a ray `(origin, dir)` through `garment` (using `posed_positions` when provided, else bind
/// `garment.positions`), returning all positive-`t` intersections sorted deterministically
/// front-to-back with visible/underneath layer separation and source CAD pattern piece lookup.
pub fn inspect_garment_ray<'a>(
    garment: &GarmentMesh<'_>,
    posed_positions: Option<&[Vec3]>,
    fabrics: &[Fabric],
    origin: Vec3,
    dir: Vec3,
    alloc: Alloc<'a>,
) -> Result<PanelClickReport<'a>, AllocFailure> {
    let mut hits = Vec::new_in(alloc);
    let Some(d_hat) = dir.normalized() else {
        return Ok(PanelClickReport {
            hits,
            visible: None,
            underneath: None,
            separation: None,
            layer_inversion: false,
            source_pattern_piece: None,
        });
    };

    let positions = posed_positions.unwrap_or(&garment.positions);
    let has_rest = garment.rest_pos_m.len() == positions.len();

    // Exact double-sided Möller-Trumbore ray-triangle intersection across all garment triangles.
    const EPS_DET: f64 = 1e-12;
    const EPS_BARY: f64 = 1e-9;
    const EPS_T: f64 = 1e-9;

    for (ti, &[ia, ib, ic]) in garment.triangles.iter().enumerate() {
        let a = ia as usize;
        let b = ib as usize;
        let c = ic as usize;
        if a >= positions.len() || b >= positions.len() || c >= positions.len() {
            continue;
        }
        let v0 = positions[a];
        let v1 = positions[b];
        let v2 = positions[c];
        let e1 = v1 - v0;
        let e2 = v2 - v0;
        let pvec = d_hat.cross(e2);
        let det = e1.dot(pvec);
        if det.abs() <= EPS_DET {
            continue;
        }
        let inv_det = 1.0 / det;
        let tvec = origin - v0;
        let u = tvec.dot(pvec) * inv_det;
        if u < -EPS_BARY || u > 1.0 + EPS_BARY {
            continue;
        }
        let qvec = tvec.cross(e1);
        let v = d_hat.dot(qvec) * inv_det;
        if v < -EPS_BARY || u + v > 1.0 + EPS_BARY {
            continue;
        }
        let t = e2.dot(qvec) * inv_det;
        if t <= EPS_T {
            continue;
        }

        let w1 = u.clamp(0.0, 1.0);
        let w2 = v.clamp(0.0, 1.0);
        let w0 = (1.0 - w1 - w2).clamp(0.0, 1.0);
        let wsum = w0 + w1 + w2;
        let bary = if wsum > 1e-12 {
            [w0 / wsum, w1 / wsum, w2 / wsum]
        } else {
            [1.0 / 3.0; 3]
        };

        let position = origin + d_hat * t;
        let normal = e1.cross(e2).normalized().unwrap_or(Vec3::new(0.0, 0.0, -1.0));
        let rest_pos_m = if has_rest {
            garment.rest_pos_m[a] * bary[0]
                + garment.rest_pos_m[b] * bary[1]
                + garment.rest_pos_m[c] * bary[2]
        } else {
            Vec2::new(0.0, 0.0)
        };

        let panel_id = garment.tri_panel.get(ti).copied().unwrap_or(0);
        let (panel_name, kind, host_panel) = match garment.panels.get(panel_id as usize) {
            Some(pm) => (pm.name, pm.kind, pm.host_panel),
            None => ("unknown", PanelKind::PatternPiece, None),
        };
        let fabric_idx = garment.tri_fabric.get(ti).copied().unwrap_or(0);
        let fabric_name = fabrics.get(fabric_idx as usize).map(|f| f.name).unwrap_or("unknown");
        let layer = garment.tri_layer.get(ti).copied().unwrap_or(0);

        push(
            &mut hits,
            PanelHit {
                tri: ti as u32,
                panel_id,
                panel_name,
                kind,
                host_panel,
                fabric_idx,
                fabric_name,
                layer,
                depth_m: t,
                position,
                normal,
                rest_pos_m,
                barycentric: bary,
            },
        )?;
    }

    // Sort deterministically front-to-back by (depth_m, tri), then collapse shared-edge/vertex
    // double hits on the same panel sheet (within 1 µm) while preserving distinct panels even at
    // zero separation and genuine self-folds > 1 µm apart.
    hits.sort_unstable_by(|x, y| x.depth_m.total_cmp(&y.depth_m).then_with(|| x.tri.cmp(&y.tri)));
    const SAME_SHEET_EPS_M: f64 = 1e-6;
    let mut deduped = Vec::new_in(alloc);
    for &h in hits.iter() {
        let same_sheet_dup = deduped
            .last()
            .is_some_and(|prev: &PanelHit| prev.panel_id == h.panel_id && (h.depth_m - prev.depth_m).abs() <= SAME_SHEET_EPS_M);
        if !same_sheet_dup {
            push(&mut deduped, h)?;
        }
    }
    let hits = deduped;

    let visible = hits.first().copied();
    let underneath = hits.get(1).copied();
    let separation = match (visible, underneath) {
        (Some(vis), Some(under)) => Some(Length::m(under.depth_m - vis.depth_m)),
        _ => None,
    };
    let layer_inversion = match (visible, underneath) {
        (Some(vis), Some(under)) => under.layer > vis.layer,
        _ => false,
    };
    let source_pattern_piece = visible.map(|vis| vis.host_panel.unwrap_or(vis.panel_name));

    Ok(PanelClickReport {
        hits,
        visible,
        underneath,
        separation,
        layer_inversion,
        source_pattern_piece,
    })
}
