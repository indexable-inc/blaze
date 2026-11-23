//! Unified collision field combining:
//! 1. Deterministic BVH over arbitrary triangle meshes (`spacer.glb` body, `mag_boots.glb`,
//!    `wrist_unit.glb`, or an already-draped inner garment layer) with exact angle-weighted
//!    pseudonormals (Baerentzen & Aanaes 2005) for robust inside/outside sign classification.
//! 2. Optional smooth anatomical modesty bridge (prevents cloth from vacuum-sucking into chest
//!    cleavage, groin cleft, or buttock cleft during outer-garment draping).
//! 3. Native composition with any `blaze-cad-field::Tape` rigid CAD solid (`min(d_mesh, d_cad)`),
//!    so cloth collides directly against hard-surface Blaze-CAD equipment (holsters, plates, gear)
//!    with zero mesh conversion!

#![blaze::sans_io]

use blaze_cad_field::{Scratch, Tape, eval_grad, eval_point};
use blaze_cad_mesh::{TriangleRegion, closest_point_on_triangle};
use blaze_math::{Aabb3, Vec3, acos, sqrt};
use blaze_own::{Alloc, AllocFailure, Vec};

/// Result of querying the unified collision field at a 3D point `p`.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct ContactQuery {
    /// Signed distance (`< 0` inside obstacle, `> 0` outside obstacle).
    pub signed_dist: f64,
    /// Closest point on the obstacle surface.
    pub closest: Vec3,
    /// Unit outward normal pointing away from the obstacle interior.
    pub normal: Vec3,
}

#[derive(Clone, Copy)]
struct BvhNode {
    bounds: Aabb3,
    /// For leaves: `count > 0`, `first` indexes into `tri_order`.
    /// For internal nodes: `count == 0`, `first` is left child index (`right = first + 1`).
    first: u32,
    count: u32,
}

const LEAF_TRIS: usize = 8;

/// Deterministic BVH triangle collider with exact closest-point + angle-weighted pseudonormal sign.
pub struct MeshCollider<'a> {
    pub positions: Vec<Vec3, Alloc<'a>>,
    pub triangles: Vec<[u32; 3], Alloc<'a>>,
    /// Per-vertex angle-weighted pseudonormals.
    vert_normals: Vec<Vec3, Alloc<'a>>,
    /// Per-triangle face unit normals.
    face_normals: Vec<Vec3, Alloc<'a>>,
    /// Ordered triangle indices referenced by BVH leaves.
    tri_order: Vec<u32, Alloc<'a>>,
    nodes: Vec<BvhNode, Alloc<'a>>,
    /// Optional smooth anatomical modesty bridge capsules/ellipsoids (for outer garments).
    pub modesty_enabled: bool,
    /// When `true` (used for single-sided already-draped inner cloth layers), only applies
    /// outward push within a local surface band (`<= 24 mm`) so distant vertices across open
    /// hems/cuffs never suffer sign-inversion catapulting.
    pub is_open_sheet: bool,
}

#[inline]
fn push<T>(v: &mut Vec<T, Alloc<'_>>, x: T) -> Result<(), AllocFailure> {
    v.try_push(x).map_err(|e| e.why)
}

impl<'a> MeshCollider<'a> {
    /// Build a deterministic BVH collider from `positions` and `triangles`.
    pub fn build(
        positions: &[Vec3],
        triangles: &[[u32; 3]],
        modesty_enabled: bool,
        alloc: Alloc<'a>,
    ) -> Result<Self, AllocFailure> {
        let mut pos_copy = Vec::new_in(alloc);
        for &p in positions {
            push(&mut pos_copy, p)?;
        }
        let mut tri_copy = Vec::new_in(alloc);
        for &t in triangles {
            push(&mut tri_copy, t)?;
        }
        let n_verts = pos_copy.len();
        let n_tris = tri_copy.len();

        let mut vert_normals = Vec::new_in(alloc);
        for _ in 0..n_verts {
            push(&mut vert_normals, Vec3::ZERO)?;
        }
        let mut face_normals = Vec::new_in(alloc);
        let mut tri_order = Vec::new_in(alloc);
        let mut centroids = Vec::new_in(alloc);
        let mut tri_boxes = Vec::new_in(alloc);

        for (ti, &[ia, ib, ic]) in tri_copy.iter().enumerate() {
            let a = pos_copy[ia as usize];
            let b = pos_copy[ib as usize];
            let c = pos_copy[ic as usize];
            let ab = b - a;
            let ac = c - a;
            let bc = c - b;
            let cross = ab.cross(ac);
            let len = cross.length();
            let fnrm = if len > 1e-12 { cross * (1.0 / len) } else { Vec3::new(0.0, 1.0, 0.0) };
            push(&mut face_normals, fnrm)?;

            if len > 1e-12 {
                let ang_a = corner_angle(ab, ac);
                let ang_b = corner_angle(a - b, bc);
                let ang_c = corner_angle(a - c, b - c);
                vert_normals[ia as usize] = vert_normals[ia as usize] + fnrm * ang_a;
                vert_normals[ib as usize] = vert_normals[ib as usize] + fnrm * ang_b;
                vert_normals[ic as usize] = vert_normals[ic as usize] + fnrm * ang_c;
            }
            push(&mut tri_order, ti as u32)?;
            push(&mut centroids, (a + b + c) * (1.0 / 3.0))?;
            let lo = Vec3::new(a.x.min(b.x).min(c.x), a.y.min(b.y).min(c.y), a.z.min(b.z).min(c.z));
            let hi = Vec3::new(a.x.max(b.x).max(c.x), a.y.max(b.y).max(c.y), a.z.max(b.z).max(c.z));
            push(&mut tri_boxes, Aabb3::new(lo, hi))?;
        }
        for n in vert_normals.iter_mut() {
            let l = n.length();
            *n = if l > 1e-12 { *n * (1.0 / l) } else { Vec3::new(0.0, 1.0, 0.0) };
        }

        let mut nodes = Vec::new_in(alloc);
        if n_tris > 0 {
            // Iterative (non-recursive, Blaze rule 18) top-down median-split BVH construction.
            let root_box = range_bounds(&tri_order, &tri_boxes, 0, n_tris);
            push(&mut nodes, BvhNode { bounds: root_box, first: 0, count: n_tris as u32 })?;

            let mut stack = Vec::new_in(alloc);
            push(&mut stack, (0usize, 0usize, n_tris))?;

            while let Some((node_idx, start, end)) = stack.pop() {
                let count = end - start;
                if count <= LEAF_TRIS {
                    nodes[node_idx].first = start as u32;
                    nodes[node_idx].count = count as u32;
                    continue;
                }
                // Compute centroid bounds to choose longest axis deterministically.
                let mut cmin = Vec3::new(f64::INFINITY, f64::INFINITY, f64::INFINITY);
                let mut cmax = Vec3::new(f64::NEG_INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
                for &ti in &tri_order[start..end] {
                    let c = centroids[ti as usize];
                    cmin = Vec3::new(cmin.x.min(c.x), cmin.y.min(c.y), cmin.z.min(c.z));
                    cmax = Vec3::new(cmax.x.max(c.x), cmax.y.max(c.y), cmax.z.max(c.z));
                }
                let ext = cmax - cmin;
                let axis = if ext.x >= ext.y && ext.x >= ext.z {
                    0
                } else if ext.y >= ext.z {
                    1
                } else {
                    2
                };
                let slice = &mut tri_order[start..end];
                slice.sort_unstable_by(|&a, &b| {
                    let ca = axis_val(centroids[a as usize], axis);
                    let cb = axis_val(centroids[b as usize], axis);
                    ca.total_cmp(&cb).then_with(|| a.cmp(&b))
                });
                let mid = start + count / 2;
                let left_box = range_bounds(&tri_order, &tri_boxes, start, mid);
                let right_box = range_bounds(&tri_order, &tri_boxes, mid, end);
                let left_idx = nodes.len();
                push(&mut nodes, BvhNode { bounds: left_box, first: start as u32, count: (mid - start) as u32 })?;
                push(&mut nodes, BvhNode { bounds: right_box, first: mid as u32, count: (end - mid) as u32 })?;
                nodes[node_idx].first = left_idx as u32;
                nodes[node_idx].count = 0;
                push(&mut stack, (left_idx + 1, mid, end))?;
                push(&mut stack, (left_idx, start, mid))?;
            }
        }

        Ok(Self {
            positions: pos_copy,
            triangles: tri_copy,
            vert_normals,
            face_normals,
            tri_order,
            nodes,
            modesty_enabled,
            is_open_sheet: false,
        })
    }

    /// Build a deterministic BVH collider for an already-draped single-sided inner cloth layer.
    pub fn build_open_sheet(
        positions: &[Vec3],
        triangles: &[[u32; 3]],
        alloc: Alloc<'a>,
    ) -> Result<Self, AllocFailure> {
        let mut col = Self::build(positions, triangles, false, alloc)?;
        col.is_open_sheet = true;
        Ok(col)
    }

    /// Exact closest point and angle-weighted signed distance to this mesh (plus optional modesty
    /// bridge for outer garments).
    pub fn query(&self, p: Vec3) -> ContactQuery {
        let mut best_d2 = f64::INFINITY;
        let mut best_closest = p;
        let mut best_pseudo = Vec3::new(0.0, 1.0, 0.0);

        if !self.nodes.is_empty() {
            let mut stack = [0u32; 64];
            let mut sp = 0usize;
            stack[sp] = 0;
            sp += 1;

            while sp > 0 {
                sp -= 1;
                let ni = stack[sp] as usize;
                let node = self.nodes[ni];
                if box_dist_sq(node.bounds, p) >= best_d2 {
                    continue;
                }
                if node.count > 0 {
                    let s = node.first as usize;
                    let e = s + node.count as usize;
                    for &ti in &self.tri_order[s..e] {
                        let [ia, ib, ic] = self.triangles[ti as usize];
                        let a = self.positions[ia as usize];
                        let b = self.positions[ib as usize];
                        let c = self.positions[ic as usize];
                        let hit = closest_point_on_triangle(p, a, b, c);
                        let cp = hit.point;
                        let diff = p - cp;
                        let d2 = diff.dot(diff);
                        if d2 < best_d2 {
                            best_d2 = d2;
                            best_closest = cp;
                            best_pseudo = match hit.region {
                                TriangleRegion::Face => self.face_normals[ti as usize],
                                TriangleRegion::VertA => self.vert_normals[ia as usize],
                                TriangleRegion::VertB => self.vert_normals[ib as usize],
                                TriangleRegion::VertC => self.vert_normals[ic as usize],
                                TriangleRegion::EdgeAB => (self.vert_normals[ia as usize] + self.vert_normals[ib as usize]).normalized().unwrap_or(self.face_normals[ti as usize]),
                                TriangleRegion::EdgeBC => (self.vert_normals[ib as usize] + self.vert_normals[ic as usize]).normalized().unwrap_or(self.face_normals[ti as usize]),
                                TriangleRegion::EdgeCA => (self.vert_normals[ic as usize] + self.vert_normals[ia as usize]).normalized().unwrap_or(self.face_normals[ti as usize]),
                            };
                        }
                    }
                } else {
                    let left = node.first as usize;
                    let right = left + 1;
                    let dl = box_dist_sq(self.nodes[left].bounds, p);
                    let dr = box_dist_sq(self.nodes[right].bounds, p);
                    if dl < dr {
                        if dr < best_d2 && sp < 64 {
                            stack[sp] = right as u32;
                            sp += 1;
                        }
                        if dl < best_d2 && sp < 64 {
                            stack[sp] = left as u32;
                            sp += 1;
                        }
                    } else {
                        if dl < best_d2 && sp < 64 {
                            stack[sp] = left as u32;
                            sp += 1;
                        }
                        if dr < best_d2 && sp < 64 {
                            stack[sp] = right as u32;
                            sp += 1;
                        }
                    }
                }
            }
        }

        let dist = sqrt(best_d2);
        let diff = p - best_closest;
        let dot = diff.dot(best_pseudo);
        let mut sign = if dot < 0.0 { -1.0 } else { 1.0 };
        // A point strictly outside the mesh's root AABB can never be inside the solid
        // (prevents open-top/non-watertight meshes like hollow boots from classifying vertices
        // hovering above the rim as deeply inside).
        if !self.nodes.is_empty() && box_dist_sq(self.nodes[0].bounds, p) > 0.0 {
            sign = 1.0;
        }
        if self.is_open_sheet {
            // For an open single-sided shell (boots or already-draped inner cloth), never classify
            // points far away or behind an open border as "deep inside" — only resolve genuine
            // local surface contact (< 22 mm).
            if dist > 0.022 {
                sign = 1.0;
            }
        }
        let normal = if self.is_open_sheet {
            best_pseudo
        } else if dist > 1e-9 {
            let dir = diff * (sign / dist);
            // Blend gently with pseudonormal right near the surface to keep normals smooth.
            if dir.dot(best_pseudo) > 0.25 { dir } else { best_pseudo }
        } else {
            best_pseudo
        };
        let mut res = ContactQuery { signed_dist: sign * dist, closest: best_closest, normal };

        // Optional anatomical modesty bridge across sternum/chest & pelvis/buttock cleft so outer
        // jackets & trousers bridge naturally over concave body crevices.
        if self.modesty_enabled {
            if let Some(m) = query_modesty_bridge(p) {
                if m.signed_dist < res.signed_dist {
                    res = m;
                }
            }
        }
        res
    }

    /// Find the closest `k` triangles/vertices for skin weight transfer (`k <= 8`).
    pub fn nearest_surface_samples(&self, p: Vec3, out: &mut [(u32, Vec3, [f64; 3], f64)]) -> usize {
        let cap = out.len();
        if cap == 0 || self.nodes.is_empty() {
            return 0;
        }
        let mut count = 0usize;
        let mut worst_d2 = f64::INFINITY;

        let mut stack = [0u32; 64];
        let mut sp = 0usize;
        stack[sp] = 0;
        sp += 1;

        while sp > 0 {
            sp -= 1;
            let ni = stack[sp] as usize;
            let node = self.nodes[ni];
            if count == cap && box_dist_sq(node.bounds, p) >= worst_d2 {
                continue;
            }
            if node.count > 0 {
                let s = node.first as usize;
                let e = s + node.count as usize;
                for &ti in &self.tri_order[s..e] {
                    let [ia, ib, ic] = self.triangles[ti as usize];
                    let a = self.positions[ia as usize];
                    let b = self.positions[ib as usize];
                    let c = self.positions[ic as usize];
                    let cp = closest_point_on_triangle(p, a, b, c).point;
                    let diff = p - cp;
                    let d2 = diff.dot(diff);
                    if count < cap || d2 < worst_d2 {
                        let bary = barycentric(cp, a, b, c);
                        if count < cap {
                            out[count] = (ti, cp, bary, d2);
                            count += 1;
                        } else {
                            // Replace current worst.
                            let mut wi = 0usize;
                            for j in 1..cap {
                                if out[j].3 > out[wi].3 {
                                    wi = j;
                                }
                            }
                            out[wi] = (ti, cp, bary, d2);
                        }
                        if count == cap {
                            worst_d2 = out[0].3;
                            for j in 1..cap {
                                if out[j].3 > worst_d2 {
                                    worst_d2 = out[j].3;
                                }
                            }
                        }
                    }
                }
            } else {
                let left = node.first as usize;
                let right = left + 1;
                let dl = box_dist_sq(self.nodes[left].bounds, p);
                let dr = box_dist_sq(self.nodes[right].bounds, p);
                if dl < dr {
                    if (count < cap || dr < worst_d2) && sp < 64 {
                        stack[sp] = right as u32;
                        sp += 1;
                    }
                    if (count < cap || dl < worst_d2) && sp < 64 {
                        stack[sp] = left as u32;
                        sp += 1;
                    }
                } else {
                    if (count < cap || dl < worst_d2) && sp < 64 {
                        stack[sp] = left as u32;
                        sp += 1;
                    }
                    if (count < cap || dr < worst_d2) && sp < 64 {
                        stack[sp] = right as u32;
                        sp += 1;
                    }
                }
            }
        }
        // Sort deterministically by (d2, tri_idx).
        out[..count].sort_unstable_by(|a, b| a.3.total_cmp(&b.3).then_with(|| a.0.cmp(&b.0)));
        count
    }
}

/// Unified collider combining any number of [`MeshCollider`]s (body, boots, wrist unit, already-draped
/// inner cloth layers) AND any number of `blaze-cad-field::Tape` rigid CAD solids!
pub struct UnifiedCollider<'a, 'm> {
    pub meshes: &'m [&'m MeshCollider<'a>],
    /// Optional Blaze-CAD field tapes (rigid holsters, armor plates, belt hardware).
    pub cad_tapes: &'m [&'m Tape<'a>],
    /// Per-mesh extra clearance offset (e.g. `0.002` m over body, `0.004` m over inner cloth).
    pub mesh_offsets: &'m [f64],
}

impl<'a, 'm> UnifiedCollider<'a, 'm> {
    pub fn query(&self, p: Vec3, cad_scratch: &mut Scratch<'_>) -> ContactQuery {
        let mut best = ContactQuery {
            signed_dist: f64::INFINITY,
            closest: p,
            normal: Vec3::new(0.0, 1.0, 0.0),
        };
        for (idx, m) in self.meshes.iter().enumerate() {
            let mut q = m.query(p);
            let off = self.mesh_offsets.get(idx).copied().unwrap_or(0.0);
            q.signed_dist -= off;
            q.closest = q.closest + q.normal * off;
            if q.signed_dist < best.signed_dist {
                best = q;
            }
        }
        for tape in self.cad_tapes.iter() {
            let d = eval_point(tape, p, cad_scratch);
            if d < best.signed_dist {
                let (_, g) = eval_grad(tape, p, cad_scratch);
                let n = g.normalized().unwrap_or(Vec3::new(0.0, 1.0, 0.0));
                best = ContactQuery { signed_dist: d, closest: p - n * d, normal: n };
            }
        }
        best
    }
}

/// Smooth anatomical modesty envelopes bridging concave sternum cleavage and gluteal/groin cleft
/// for realistic outer garment draping (glTF Y-up MetaHuman frame).
fn query_modesty_bridge(p: Vec3) -> Option<ContactQuery> {
    // 1. Chest/sternum bridge ellipsoid around Y = 1.24..1.42 (smooth front torso plane).
    // 2. Pelvis/hip bridge around Y = 0.86..1.05 (bridges gluteal cleft & upper thigh crease).
    let mut best: Option<ContactQuery> = None;

    // Sternum/upper chest bridge (keeps jacket & shirt bridging smoothly across the chest).
    if p.y > 1.15 && p.y < 1.47 && p.x.abs() < 0.22 {
        let c = Vec3::new(0.0, 1.32, 0.008);
        let r = Vec3::new(0.160, 0.135, 0.118);
        let q = query_ellipsoid(p, c, r);
        best = Some(q);
    }

    // Lower torso / waist / hip / front-pelvis smooth modesty envelope: $C^\infty$ smooth bridge
    // over front pelvis & gluteal cleft (front at Z = -0.142) with zero open-mesh artifacts.
    if p.y > 0.72 && p.y < 1.18 && p.x.abs() < 0.24 {
        let c = Vec3::new(0.0, 0.91, -0.012);
        let r = Vec3::new(0.162, 0.160, 0.130);
        let q = query_ellipsoid(p, c, r);
        if best.map_or(true, |b| q.signed_dist < b.signed_dist) {
            best = Some(q);
        }
    }
    best
}

fn query_ellipsoid(p: Vec3, center: Vec3, r: Vec3) -> ContactQuery {
    let q = p - center;
    let k0 = Vec3::new(q.x / r.x, q.y / r.y, q.z / r.z).length();
    let k1 = Vec3::new(q.x / (r.x * r.x), q.y / (r.y * r.y), q.z / (r.z * r.z)).length();
    let d = if k1 > 1e-12 { k0 * (k0 - 1.0) / k1 } else { -r.x.min(r.y).min(r.z) };
    let grad = Vec3::new(q.x / (r.x * r.x), q.y / (r.y * r.y), q.z / (r.z * r.z));
    let gl = grad.length();
    let normal = if gl > 1e-12 { grad * (1.0 / gl) } else { Vec3::new(0.0, 0.0, -1.0) };
    ContactQuery { signed_dist: d, closest: p - normal * d, normal }
}

#[inline]
fn corner_angle(u: Vec3, v: Vec3) -> f64 {
    let lu = u.length();
    let lv = v.length();
    if lu <= 1e-12 || lv <= 1e-12 {
        return 0.0;
    }
    let c = (u.dot(v) / (lu * lv)).clamp(-1.0, 1.0);
    acos(c)
}

#[inline]
fn axis_val(v: Vec3, axis: usize) -> f64 {
    match axis {
        0 => v.x,
        1 => v.y,
        _ => v.z,
    }
}

fn range_bounds(order: &[u32], boxes: &[Aabb3], start: usize, end: usize) -> Aabb3 {
    let mut b = boxes[order[start] as usize];
    for &ti in &order[start + 1..end] {
        b = b.union(&boxes[ti as usize]);
    }
    b
}

#[inline]
fn box_dist_sq(b: Aabb3, p: Vec3) -> f64 {
    let dx = if p.x < b.lo.x { b.lo.x - p.x } else if p.x > b.hi.x { p.x - b.hi.x } else { 0.0 };
    let dy = if p.y < b.lo.y { b.lo.y - p.y } else if p.y > b.hi.y { p.y - b.hi.y } else { 0.0 };
    let dz = if p.z < b.lo.z { b.lo.z - p.z } else if p.z > b.hi.z { p.z - b.hi.z } else { 0.0 };
    dx * dx + dy * dy + dz * dz
}

fn barycentric(p: Vec3, a: Vec3, b: Vec3, c: Vec3) -> [f64; 3] {
    let v0 = b - a;
    let v1 = c - a;
    let v2 = p - a;
    let d00 = v0.dot(v0);
    let d01 = v0.dot(v1);
    let d11 = v1.dot(v1);
    let d20 = v2.dot(v0);
    let d21 = v2.dot(v1);
    let denom = d00 * d11 - d01 * d01;
    if denom.abs() <= 1e-18 {
        return [1.0 / 3.0, 1.0 / 3.0, 1.0 / 3.0];
    }
    let v = ((d11 * d20 - d01 * d21) / denom).clamp(0.0, 1.0);
    let w = ((d00 * d21 - d01 * d20) / denom).clamp(0.0, 1.0);
    let u = (1.0 - v - w).clamp(0.0, 1.0);
    let s = u + v + w;
    if s > 1e-12 { [u / s, v / s, w / s] } else { [1.0 / 3.0; 3] }
}
