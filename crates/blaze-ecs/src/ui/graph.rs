//! Relational ECS WebGPU graph & spatial scene engine built directly on [`crate::World`].
//!
//! Every remote entity is mirrored as a real [`crate::Entity`] in a client-side [`crate::World`]
//! with [`RemoteSlot`], [`GraphNode`], and [`SpatialBody`] components plus real `ChildOf` and
//! `IsA` relational pairs. [`step_and_pack`] runs deterministic physics (or projects live
//! `Transform`/`Velocity` coordinates) and writes tightly packed `f32` WebGPU instance buffers
//! directly for zero-copy upload from WASM linear memory.

use core::alloc::Allocator;
use core::fmt::Write as _;

use blaze_int::Int as _;
use blaze_own::{String as OwnString, Vec as OwnVec};

use crate::{Entity, World};

/// Client-side ECS component linking a UI entity to its server entity slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RemoteSlot(pub u32);

/// Force-directed relational graph node stored inside [`World`].
#[derive(Clone, Copy, Debug)]
pub struct GraphNode {
    pub x: f32,
    pub y: f32,
    pub vx: f32,
    pub vy: f32,
    pub radius: f32,
    pub pinned: bool,
    pub is_prefab: bool,
    pub comp_count: u16,
}

/// Live spatial transform & velocity mirrored from the server's `Transform`/`Velocity` components.
#[derive(Clone, Copy, Debug)]
pub struct SpatialBody {
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub dx: f32,
    pub dy: f32,
    pub dz: f32,
    pub has_transform: bool,
    pub has_velocity: bool,
    pub active: bool,
}

/// WebGPU viewport visualization mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphMode {
    /// Force-directed relational ECS graph (`ChildOf` hierarchy + `IsA` prefab inheritance).
    Relational,
    /// Live world-space `Transform (x, y, z)` + `Velocity (dx, dy, dz)` vectors.
    Spatial,
}

/// Snapshot metadata for one synced node (kept beside `World` for label lookup).
pub struct NodeEntry<A: Allocator + Copy> {
    pub entity: Entity,
    pub slot: u32,
    pub name: OwnString<A>,
    pub parent_slot: Option<u32>,
    pub prefab_slot: Option<u32>,
}

/// Packed GPU buffers and label JSON emitted by [`step_and_pack`].
pub struct GraphBuffers<A: Allocator + Copy> {
    /// 8 floats per node: `[x, y, radius, flags, r, g, b, a]` (16-byte aligned for WGSL `vec4`).
    pub nodes: OwnVec<f32, A>,
    /// 8 floats per edge: `[x0, y0, x1, y1, r, g, b, a]`.
    pub edges: OwnVec<f32, A>,
    /// Compact JSON array `[[slot, x, y, "Name", flags], ...]` for crisp 2D canvas labels.
    pub labels: OwnString<A>,
}

impl<A: Allocator + Copy> GraphBuffers<A> {
    pub fn new_in(alloc: A) -> Self {
        Self {
            nodes: OwnVec::new_in(alloc),
            edges: OwnVec::new_in(alloc),
            labels: OwnString::new_in(alloc),
        }
    }
}

#[derive(Clone, Copy)]
struct SimNode {
    entity: Entity,
    slot: u32,
    x: f32,
    y: f32,
    vx: f32,
    vy: f32,
    fx: f32,
    fy: f32,
    radius: f32,
    pinned: bool,
    is_prefab: bool,
    px: f32,
    py: f32,
    visible: bool,
    neighbor: bool,
}

/// Deterministic initial position on a golden-angle spiral so fresh graphs open balanced.
pub fn initial_spiral_pos(index: usize) -> (f32, f32) {
    let idx_u32 = index.truncate_to::<u32>();
    let k = f64::from(idx_u32) + 1.0;
    let r = 34.0 * blaze_math::sqrt(k);
    let theta = k * 2.399_963_229_728_653;
    let x = f64_to_f32(r * blaze_math::cos(theta));
    let y = f64_to_f32(r * blaze_math::sin(theta));
    (x, y)
}

#[inline]
pub fn f64_to_f32(v: f64) -> f32 {
    #[allow(clippy::cast_possible_truncation)]
    let out = v as f32;
    out
}

/// Advance the `World`-backed force-directed layout (or project live `SpatialBody` coordinates)
/// and pack WebGPU instance buffers directly in WASM memory (zero unchecked indexing).
pub fn step_and_pack<A: Allocator + Copy + 'static>(
    world: &mut World<A>,
    entries: &[NodeEntry<A>],
    mode: GraphMode,
    dt_raw: f32,
    selected_slot: Option<u32>,
    hovered_slot: Option<u32>,
    find_hits: &[u32],
    find_cur_slot: Option<u32>,
    light_theme: bool,
    out: &mut GraphBuffers<A>,
    alloc: A,
) {
    let dt = if dt_raw > 0.05 {
        0.05
    } else if dt_raw < 0.001 {
        0.016
    } else {
        dt_raw
    };

    let mut sim: OwnVec<SimNode, A> = OwnVec::new_in(alloc);
    for e in entries {
        let gb = world.get::<GraphNode>(e.entity).ok().flatten().copied().unwrap_or(GraphNode {
            x: 0.0,
            y: 0.0,
            vx: 0.0,
            vy: 0.0,
            radius: 8.0,
            pinned: false,
            is_prefab: false,
            comp_count: 0,
        });
        let _ = sim.try_push(SimNode {
            entity: e.entity,
            slot: e.slot,
            x: gb.x,
            y: gb.y,
            vx: gb.vx,
            vy: gb.vy,
            fx: 0.0,
            fy: 0.0,
            radius: gb.radius,
            pinned: gb.pinned,
            is_prefab: gb.is_prefab,
            px: gb.x,
            py: gb.y,
            visible: true,
            neighbor: false,
        });
    }

    // Collect relational edges straight from `World` via real `ChildOf` and `IsA` pairs.
    let child_of = world.builtins().child_of;
    let is_a = world.builtins().is_a;
    let mut rel_edges: OwnVec<(usize, usize, bool), A> = OwnVec::new_in(alloc);
    for (i, a) in entries.iter().enumerate() {
        for (j, b) in entries.iter().enumerate() {
            if i == j {
                continue;
            }
            if world.has_pair(a.entity, child_of, b.entity).unwrap_or(false) {
                let _ = rel_edges.try_push((i, j, false));
            }
            if world.has_pair(a.entity, is_a, b.entity).unwrap_or(false) {
                let _ = rel_edges.try_push((i, j, true));
            }
        }
    }

    if mode == GraphMode::Relational && !sim.is_empty() {
        let repel = 22_000.0f32;
        let len = sim.len();
        for i in 0..len {
            let (left, right) = sim.as_mut_slice().split_at_mut(i + 1);
            let Some(ni) = left.last_mut() else { continue };
            ni.fx -= ni.x * 0.35;
            ni.fy -= ni.y * 0.35;
            for nj in right.iter_mut() {
                let mut dx = nj.x - ni.x;
                let mut dy = nj.y - ni.y;
                let d2 = dx * dx + dy * dy + 24.0;
                let dist = blaze_math::sqrtf(d2);
                if dist > 0.001 {
                    dx /= dist;
                    dy /= dist;
                }
                let f = repel / d2;
                ni.fx -= dx * f;
                ni.fy -= dy * f;
                nj.fx += dx * f;
                nj.fy += dy * f;
            }
        }

        // Hooke springs along relational ECS edges (`ChildOf` tighter, `IsA` looser)
        for &(a, b, is_isa) in rel_edges.as_slice() {
            let (Some(na), Some(nb)) = (sim.get(a).copied(), sim.get(b).copied()) else {
                continue;
            };
            let dx = nb.x - na.x;
            let dy = nb.y - na.y;
            let dist = blaze_math::sqrtf(dx * dx + dy * dy + 0.01);
            let rest = if is_isa { 140.0f32 } else { 88.0f32 };
            let k = if is_isa { 1.4f32 } else { 3.2f32 };
            let force = (dist - rest) * k;
            let ux = dx / dist;
            let uy = dy / dist;
            if let Some(sa) = sim.get_mut(a) {
                sa.fx += ux * force;
                sa.fy += uy * force;
            }
            if let Some(sb) = sim.get_mut(b) {
                sb.fx -= ux * force;
                sb.fy -= uy * force;
            }
        }

        // Semi-implicit Euler integration with smooth damping
        let damp = 0.84f32;
        for s in sim.as_mut_slice() {
            if s.pinned {
                s.vx = 0.0;
                s.vy = 0.0;
            } else {
                s.vx = (s.vx + s.fx * dt) * damp;
                s.vy = (s.vy + s.fy * dt) * damp;
                let speed = blaze_math::sqrtf(s.vx * s.vx + s.vy * s.vy);
                if speed > 420.0 {
                    let scale = 420.0 / speed;
                    s.vx *= scale;
                    s.vy *= scale;
                }
                s.x += s.vx * dt;
                s.y += s.vy * dt;
            }
            if let Ok(Some(gb)) = world.get_mut::<GraphNode>(s.entity) {
                gb.x = s.x;
                gb.y = s.y;
                gb.vx = s.vx;
                gb.vy = s.vy;
            }
        }
    }

    // Compute displayed (px, py) positions based on mode
    for s in sim.as_mut_slice() {
        if mode == GraphMode::Spatial {
            if let Ok(Some(sb)) = world.get::<SpatialBody>(s.entity) {
                if sb.has_transform {
                    s.px = sb.x * 2.4 + sb.z * 0.65;
                    s.py = -sb.y * 2.4 - sb.z * 0.45;
                    s.visible = true;
                    continue;
                }
            }
            s.visible = false;
        } else {
            s.px = s.x;
            s.py = s.y;
            s.visible = true;
        }
    }

    // Highlight hovered node & relational neighbors
    let hover_idx = hovered_slot.and_then(|hs| sim.iter().position(|s| s.slot == hs));
    if let Some(hi) = hover_idx {
        if let Some(sh) = sim.get_mut(hi) {
            sh.neighbor = true;
        }
        for &(a, b, _) in rel_edges.as_slice() {
            if a == hi {
                if let Some(sb) = sim.get_mut(b) {
                    sb.neighbor = true;
                }
            } else if b == hi {
                if let Some(sa) = sim.get_mut(a) {
                    sa.neighbor = true;
                }
            }
        }
    }

    out.nodes.clear();
    out.edges.clear();
    out.labels.clear();

    // Pack WebGPU edge instances
    if mode == GraphMode::Relational {
        for &(a, b, is_isa) in rel_edges.as_slice() {
            let (Some(na), Some(nb)) = (sim.get(a), sim.get(b)) else { continue };
            let lit = hover_idx.is_none() || hover_idx == Some(a) || hover_idx == Some(b);
            let sel_touch = selected_slot == Some(na.slot) || selected_slot == Some(nb.slot);
            let (r, g, bl, al) = if is_isa {
                if lit || sel_touch {
                    (0.878f32, 0.275f32, 0.102f32, 0.52f32)
                } else {
                    (0.878f32, 0.275f32, 0.102f32, 0.10f32)
                }
            } else if light_theme {
                if sel_touch {
                    (0.15, 0.15, 0.16, 0.45)
                } else if lit {
                    (0.20, 0.20, 0.22, 0.22)
                } else {
                    (0.20, 0.20, 0.22, 0.06)
                }
            } else if sel_touch {
                (0.92, 0.91, 0.89, 0.42)
            } else if lit {
                (0.75, 0.74, 0.72, 0.20)
            } else {
                (0.75, 0.74, 0.72, 0.05)
            };
            push_edge(&mut out.edges, na.px, na.py, nb.px, nb.py, r, g, bl, al);
        }
    } else {
        for &(a, b, is_isa) in rel_edges.as_slice() {
            let (Some(na), Some(nb)) = (sim.get(a), sim.get(b)) else { continue };
            if !is_isa && na.visible && nb.visible {
                let (r, g, bl, al) = if light_theme {
                    (0.20, 0.20, 0.22, 0.16)
                } else {
                    (0.80, 0.79, 0.77, 0.14)
                };
                push_edge(&mut out.edges, na.px, na.py, nb.px, nb.py, r, g, bl, al);
            }
        }
        for s in sim.as_slice() {
            if !s.visible {
                continue;
            }
            if let Ok(Some(sb)) = world.get::<SpatialBody>(s.entity) {
                if sb.has_velocity && (sb.dx * sb.dx + sb.dy * sb.dy + sb.dz * sb.dz) > 0.001 {
                    let vx_end = s.px + (sb.dx * 2.4 + sb.dz * 0.65) * 1.4;
                    let vy_end = s.py + (-sb.dy * 2.4 - sb.dz * 0.45) * 1.4;
                    push_edge(&mut out.edges, s.px, s.py, vx_end, vy_end, 0.878, 0.275, 0.102, 0.78);
                }
            }
        }
    }

    // Pack WebGPU node instances + JSON labels
    let _ = out.labels.try_push_str("[");
    let mut first_label = true;
    for (s, entry) in sim.iter().zip(entries.iter()) {
        if !s.visible {
            continue;
        }
        let is_sel = selected_slot == Some(s.slot);
        let is_hov = hovered_slot == Some(s.slot);
        let is_find_cur = find_cur_slot == Some(s.slot);
        let is_find_hit = find_hits.iter().any(|&hs| hs == s.slot);
        let dimmed = hover_idx.is_some() && !s.neighbor && !is_sel;

        let base_r = if is_sel {
            s.radius + 2.2
        } else if is_hov {
            s.radius + 1.2
        } else {
            s.radius
        };

        let (r, g, b, a) = if is_sel || is_find_cur {
            (0.878f32, 0.275f32, 0.102f32, 1.0f32)
        } else if is_find_hit {
            (0.95f32, 0.46f32, 0.24f32, 0.95f32)
        } else if s.is_prefab {
            (0.878f32, 0.275f32, 0.102f32, if dimmed { 0.22 } else { 0.72 })
        } else if light_theme {
            if is_hov {
                (0.09, 0.09, 0.10, 1.0)
            } else {
                (0.24, 0.24, 0.26, if dimmed { 0.20 } else { 0.88 })
            }
        } else if is_hov {
            (0.94, 0.93, 0.91, 1.0)
        } else {
            (0.76, 0.75, 0.73, if dimmed { 0.18 } else { 0.88 })
        };

        let flags: f32 = if is_sel {
            1.0
        } else if is_hov {
            2.0
        } else if is_find_hit {
            3.0
        } else if s.is_prefab {
            4.0
        } else {
            0.0
        };

        let _ = out.nodes.try_push(s.px);
        let _ = out.nodes.try_push(s.py);
        let _ = out.nodes.try_push(base_r);
        let _ = out.nodes.try_push(flags);
        let _ = out.nodes.try_push(r);
        let _ = out.nodes.try_push(g);
        let _ = out.nodes.try_push(b);
        let _ = out.nodes.try_push(a);

        if !first_label {
            let _ = out.labels.try_push_str(",");
        }
        first_label = false;
        let label_flags: u8 = if is_sel {
            1
        } else if is_hov {
            2
        } else if dimmed {
            4
        } else {
            0
        };
        let _ = write!(out.labels, "[{},{:.2},{:.2},\"", s.slot, s.px, s.py);
        write_json_escaped(&mut out.labels, entry.name.as_str());
        let _ = write!(out.labels, "\",{}]", label_flags);
    }
    let _ = out.labels.try_push_str("]");
}

fn push_edge<A: Allocator + Copy>(
    edges: &mut OwnVec<f32, A>,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    r: f32,
    g: f32,
    b: f32,
    a: f32,
) {
    let _ = edges.try_push(x0);
    let _ = edges.try_push(y0);
    let _ = edges.try_push(x1);
    let _ = edges.try_push(y1);
    let _ = edges.try_push(r);
    let _ = edges.try_push(g);
    let _ = edges.try_push(b);
    let _ = edges.try_push(a);
}

/// Hit-test graph nodes in world coordinates, returning the matching server entity slot.
pub fn pick_node<A: Allocator + Copy + 'static>(
    world: &World<A>,
    entries: &[NodeEntry<A>],
    mode: GraphMode,
    wx: f32,
    wy: f32,
    pad: f32,
) -> Option<u32> {
    let mut best: Option<(u32, f32)> = None;
    for e in entries {
        let (nx, ny, nr) = if mode == GraphMode::Spatial {
            let Ok(Some(sb)) = world.get::<SpatialBody>(e.entity) else { continue };
            if !sb.has_transform {
                continue;
            }
            let r = world.get::<GraphNode>(e.entity).ok().flatten().map_or(8.0, |g| g.radius);
            (sb.x * 2.4 + sb.z * 0.65, -sb.y * 2.4 - sb.z * 0.45, r)
        } else {
            let Ok(Some(gb)) = world.get::<GraphNode>(e.entity) else { continue };
            (gb.x, gb.y, gb.radius)
        };
        let dx = wx - nx;
        let dy = wy - ny;
        let d = blaze_math::sqrtf(dx * dx + dy * dy);
        if d <= nr + pad && best.is_none_or(|(_, bd)| d < bd) {
            best = Some((e.slot, d));
        }
    }
    best.map(|(s, _)| s)
}

/// Compute `[center_x, center_y, span_w, span_h]` bounding box of active nodes for auto-fit.
pub fn fit_bounds<A: Allocator + Copy + 'static>(
    world: &World<A>,
    entries: &[NodeEntry<A>],
    mode: GraphMode,
) -> (f32, f32, f32, f32) {
    let mut min_x = f32::MAX;
    let mut max_x = f32::MIN;
    let mut min_y = f32::MAX;
    let mut max_y = f32::MIN;
    let mut any = false;
    for e in entries {
        let (x, y) = if mode == GraphMode::Spatial {
            let Ok(Some(sb)) = world.get::<SpatialBody>(e.entity) else { continue };
            if !sb.has_transform {
                continue;
            }
            (sb.x * 2.4 + sb.z * 0.65, -sb.y * 2.4 - sb.z * 0.45)
        } else {
            let Ok(Some(gb)) = world.get::<GraphNode>(e.entity) else { continue };
            (gb.x, gb.y)
        };
        any = true;
        if x < min_x { min_x = x; }
        if x > max_x { max_x = x; }
        if y < min_y { min_y = y; }
        if y > max_y { max_y = y; }
    }
    if !any {
        return (0.0, 0.0, 320.0, 240.0);
    }
    let cx = (min_x + max_x) * 0.5;
    let cy = (min_y + max_y) * 0.5;
    let w = (max_x - min_x).max(120.0) + 120.0;
    let h = (max_y - min_y).max(120.0) + 120.0;
    (cx, cy, w, h)
}

fn write_json_escaped<W: core::fmt::Write>(out: &mut W, s: &str) {
    for ch in s.chars() {
        match ch {
            '"' => { let _ = out.write_str("\\\""); }
            '\\' => { let _ = out.write_str("\\\\"); }
            '\n' => { let _ = out.write_str("\\n"); }
            '\r' => { let _ = out.write_str("\\r"); }
            '\t' => { let _ = out.write_str("\\t"); }
            c if c >= ' ' => { let _ = out.write_char(c); }
            _ => {}
        }
    }
}
