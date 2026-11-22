//! Deterministic `#![no_std]` CPU host GPU simulator:
//! - [`dispatch_compute`] runs any `#[gpu::kernel]` function across a [`Grid`] on CPU.
//! - [`dispatch_workgroup_2phase`] simulates phased [`SharedWrite`] → `.barrier()` → [`SharedRead`]
//!   workgroup execution deterministically across all threads in each workgroup.
//! - [`eval_quad_2x2`] and [`rasterize_triangles`] evaluate 2×2 fragment quads with real
//!   `dpdx`, `dpdy`, `fwidth`, perspective-correct interpolation, depth testing, and `discard`
//!   into caller-owned slices with zero heap allocations and zero global mutable state.

use crate::compute::{Grid, SharedMem, SharedRead, SharedWrite};
use crate::entry::{InstanceIndex, Position, Thread, VertexIndex};
use crate::math::{abs, ceil, floor};
use crate::vec::{Vec2, Vec3, Vec4, uvec3, vec3, vec4};

/// Executes `kernel` deterministically for every thread in `grid`, in workgroup order
/// (`z`, `y`, `x`) and local thread order (`z`, `y`, `x`).
///
/// Any `#[gpu::kernel]` function can be called directly inside the closure:
/// ```ignore
/// gpu::cpu::dispatch_compute(grid, |t| my_kernel(t, &uniforms, out));
/// ```
pub fn dispatch_compute(grid: Grid, mut kernel: impl FnMut(Thread)) {
    for gz in 0..grid.groups.z {
        for gy in 0..grid.groups.y {
            for gx in 0..grid.groups.x {
                let group = uvec3(gx, gy, gz);
                for lz in 0..grid.threads.z {
                    for ly in 0..grid.threads.y {
                        for lx in 0..grid.threads.x {
                            let local = uvec3(lx, ly, lz);
                            let global = uvec3(
                                gx * grid.threads.x + lx,
                                gy * grid.threads.y + ly,
                                gz * grid.threads.z + lz,
                            );
                            kernel(Thread::new(global, local, group));
                        }
                    }
                }
            }
        }
    }
}

/// Executes a two-phase workgroup compute kernel with phased shared memory:
/// for each workgroup in `grid`, runs `write_phase` for every thread in the workgroup,
/// synchronizes with `.barrier()`, and then runs `read_phase` for every thread in the workgroup.
pub fn dispatch_workgroup_2phase<T: Copy, const N: usize>(
    grid: Grid,
    shared: &SharedMem<T, N>,
    mut write_phase: impl FnMut(&Thread, &SharedWrite<'_, T, N>),
    mut read_phase: impl FnMut(&Thread, &SharedRead<'_, T, N>),
) {
    for gz in 0..grid.groups.z {
        for gy in 0..grid.groups.y {
            for gx in 0..grid.groups.x {
                let group = uvec3(gx, gy, gz);
                let writer = shared.begin_write();
                for lz in 0..grid.threads.z {
                    for ly in 0..grid.threads.y {
                        for lx in 0..grid.threads.x {
                            let local = uvec3(lx, ly, lz);
                            let global = uvec3(
                                gx * grid.threads.x + lx,
                                gy * grid.threads.y + ly,
                                gz * grid.threads.z + lz,
                            );
                            let t = Thread::new(global, local, group);
                            write_phase(&t, &writer);
                        }
                    }
                }
                let reader = writer.barrier();
                for lz in 0..grid.threads.z {
                    for ly in 0..grid.threads.y {
                        for lx in 0..grid.threads.x {
                            let local = uvec3(lx, ly, lz);
                            let global = uvec3(
                                gx * grid.threads.x + lx,
                                gy * grid.threads.y + ly,
                                gz * grid.threads.z + lz,
                            );
                            let t = Thread::new(global, local, group);
                            read_phase(&t, &reader);
                        }
                    }
                }
            }
        }
    }
}

/// Types that support screen-space finite-difference derivatives (`dpdx`, `dpdy`, `fwidth`) across
/// a 2×2 fragment quad.
pub trait QuadDeriv: Copy {
    fn sub_deriv(self, other: Self) -> Self;
    fn abs_deriv(self) -> Self;
    fn add_deriv(self, other: Self) -> Self;
}

impl QuadDeriv for f32 {
    #[inline]
    fn sub_deriv(self, other: Self) -> Self {
        self - other
    }
    #[inline]
    fn abs_deriv(self) -> Self {
        abs(self)
    }
    #[inline]
    fn add_deriv(self, other: Self) -> Self {
        self + other
    }
}

impl QuadDeriv for Vec2 {
    #[inline]
    fn sub_deriv(self, other: Self) -> Self {
        self - other
    }
    #[inline]
    fn abs_deriv(self) -> Self {
        self.abs()
    }
    #[inline]
    fn add_deriv(self, other: Self) -> Self {
        self + other
    }
}

impl QuadDeriv for Vec3 {
    #[inline]
    fn sub_deriv(self, other: Self) -> Self {
        self - other
    }
    #[inline]
    fn abs_deriv(self) -> Self {
        self.abs()
    }
    #[inline]
    fn add_deriv(self, other: Self) -> Self {
        self + other
    }
}

impl QuadDeriv for Vec4 {
    #[inline]
    fn sub_deriv(self, other: Self) -> Self {
        self - other
    }
    #[inline]
    fn abs_deriv(self) -> Self {
        self.abs()
    }
    #[inline]
    fn add_deriv(self, other: Self) -> Self {
        self + other
    }
}

/// Values across the 4 lanes of a 2×2 fragment quad: `[TL (0,0), TR (1,0), BL (0,1), BR (1,1)]`.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Quad<T>(pub [T; 4]);

impl<T: QuadDeriv> Quad<T> {
    /// Fine horizontal derivative (`TR - TL` on top row, `BR - BL` on bottom row).
    #[inline]
    pub fn dpdx(self) -> Self {
        let dx_top = self.0[1].sub_deriv(self.0[0]);
        let dx_bot = self.0[3].sub_deriv(self.0[2]);
        Self([dx_top, dx_top, dx_bot, dx_bot])
    }

    /// Fine vertical derivative (`BL - TL` on left column, `BR - TR` on right column).
    #[inline]
    pub fn dpdy(self) -> Self {
        let dy_left = self.0[2].sub_deriv(self.0[0]);
        let dy_right = self.0[3].sub_deriv(self.0[1]);
        Self([dy_left, dy_right, dy_left, dy_right])
    }

    /// Screen-space filter width: `abs(dpdx) + abs(dpdy)`.
    #[inline]
    pub fn fwidth(self) -> Self {
        let dx = self.dpdx();
        let dy = self.dpdy();
        Self([
            dx.0[0].abs_deriv().add_deriv(dy.0[0].abs_deriv()),
            dx.0[1].abs_deriv().add_deriv(dy.0[1].abs_deriv()),
            dx.0[2].abs_deriv().add_deriv(dy.0[2].abs_deriv()),
            dx.0[3].abs_deriv().add_deriv(dy.0[3].abs_deriv()),
        ])
    }
}

/// Result of shading one fragment lane on CPU.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum FragOutcome {
    Keep(Vec4),
    Discard,
}

/// Per-lane context passed into CPU fragment evaluation, providing `dpdx`, `dpdy`, and `fwidth`
/// across the 2×2 quad without any global mutable state.
pub struct QuadCtx<'a, V> {
    lanes: &'a [V; 4],
    lane: usize,
}

impl<'a, V> QuadCtx<'a, V> {
    /// Index of this lane in `[0..4]` (`0: TL, 1: TR, 2: BL, 3: BR`).
    #[inline]
    pub const fn lane(&self) -> usize {
        self.lane
    }

    /// This lane's interpolated varying payload.
    #[inline]
    pub fn value(&self) -> &'a V {
        &self.lanes[self.lane]
    }

    /// Fine horizontal derivative (`ddx`) of an expression evaluated over the quad's lanes.
    #[inline]
    pub fn dpdx<T: QuadDeriv>(&self, f: impl Fn(&V) -> T) -> T {
        let left = self.lane & !1;
        let right = self.lane | 1;
        f(&self.lanes[right]).sub_deriv(f(&self.lanes[left]))
    }

    /// Fine vertical derivative (`ddy`) of an expression evaluated over the quad's lanes.
    #[inline]
    pub fn dpdy<T: QuadDeriv>(&self, f: impl Fn(&V) -> T) -> T {
        let top = self.lane & !2;
        let bot = self.lane | 2;
        f(&self.lanes[bot]).sub_deriv(f(&self.lanes[top]))
    }

    /// `abs(dpdx(f)) + abs(dpdy(f))` across the 2×2 quad.
    #[inline]
    pub fn fwidth<T: QuadDeriv>(&self, f: impl Fn(&V) -> T) -> T {
        let dx = self.dpdx(&f).abs_deriv();
        let dy = self.dpdy(&f).abs_deriv();
        dx.add_deriv(dy)
    }
}

/// Evaluates a fragment shader across all 4 lanes of a 2×2 quad, returning `None` for any lane
/// that returned [`FragOutcome::Discard`].
pub fn eval_quad_2x2<V>(
    lanes: &[V; 4],
    mut fs: impl FnMut(&QuadCtx<'_, V>) -> FragOutcome,
) -> [Option<Vec4>; 4] {
    let mut out = [None; 4];
    for lane in 0..4 {
        let ctx = QuadCtx { lanes, lane };
        out[lane] = match fs(&ctx) {
            FragOutcome::Keep(c) => Some(c),
            FragOutcome::Discard => None,
        };
    }
    out
}

/// Software triangle rasterizer targeting caller-owned `color` (and optional `depth`) buffers
/// with zero heap allocations.
///
/// - Draws `triangle_count` triangles (`vertex_index` `0..3 * triangle_count`) for each of
///   `instance_count` instances.
/// - Clips out triangles with non-positive `w` or degenerate screen area.
/// - Evaluates fragments in aligned 2×2 pixel quads with perspective-correct barycentric
///   interpolation (so helper lanes provide accurate `dpdx`, `dpdy`, `fwidth` at triangle edges!).
/// - Applies `[0.0, 1.0]` depth range and `<` depth test when `depth` is `Some`.
pub fn rasterize_triangles<V: Copy>(
    width: u32,
    height: u32,
    triangle_count: u32,
    instance_count: u32,
    color: &mut [Vec4],
    mut depth: Option<&mut [f32]>,
    vs: impl Fn(VertexIndex, InstanceIndex) -> (Position, V),
    interp: impl Fn(&V, &V, &V, Vec3) -> V,
    mut fs: impl FnMut(Vec4, &QuadCtx<'_, V>) -> FragOutcome,
) {
    let pixel_count = (width as usize) * (height as usize);
    assert!(color.len() >= pixel_count, "color framebuffer too small for {width}x{height}");
    if let Some(ref d) = depth {
        assert!(d.len() >= pixel_count, "depth buffer too small for {width}x{height}");
    }
    if width == 0 || height == 0 {
        return;
    }

    let fw = width as f32;
    let fh = height as f32;

    for inst in 0..instance_count {
        let iid = InstanceIndex(inst);
        for tri in 0..triangle_count {
            let base = tri * 3;
            let (p0, v0) = vs(VertexIndex(base), iid);
            let (p1, v1) = vs(VertexIndex(base + 1), iid);
            let (p2, v2) = vs(VertexIndex(base + 2), iid);

            if p0.0.w <= 0.0 || p1.0.w <= 0.0 || p2.0.w <= 0.0 {
                continue;
            }
            let inv_w0 = 1.0 / p0.0.w;
            let inv_w1 = 1.0 / p1.0.w;
            let inv_w2 = 1.0 / p2.0.w;

            let s0 = vec3((p0.0.x * inv_w0 * 0.5 + 0.5) * fw, (0.5 - p0.0.y * inv_w0 * 0.5) * fh, p0.0.z * inv_w0);
            let s1 = vec3((p1.0.x * inv_w1 * 0.5 + 0.5) * fw, (0.5 - p1.0.y * inv_w1 * 0.5) * fh, p1.0.z * inv_w1);
            let s2 = vec3((p2.0.x * inv_w2 * 0.5 + 0.5) * fw, (0.5 - p2.0.y * inv_w2 * 0.5) * fh, p2.0.z * inv_w2);

            let area = edge_fn(s0.x, s0.y, s1.x, s1.y, s2.x, s2.y);
            if abs(area) < 1e-8 {
                continue;
            }
            let inv_area = 1.0 / area;

            let min_x = (floor(s0.x.min(s1.x).min(s2.x)) as i32).clamp(0, width as i32 - 1) & !1;
            let max_x = (ceil(s0.x.max(s1.x).max(s2.x)) as i32).clamp(0, width as i32);
            let min_y = (floor(s0.y.min(s1.y).min(s2.y)) as i32).clamp(0, height as i32 - 1) & !1;
            let max_y = (ceil(s0.y.max(s1.y).max(s2.y)) as i32).clamp(0, height as i32);

            let mut qy = min_y;
            while qy < max_y {
                let mut qx = min_x;
                while qx < max_x {
                    let coords = [
                        (qx, qy),
                        (qx + 1, qy),
                        (qx, qy + 1),
                        (qx + 1, qy + 1),
                    ];
                    let mut covered = [false; 4];
                    let mut frag_pos = [Vec4::default(); 4];
                    let mut bary_persp = [vec3(1.0, 0.0, 0.0); 4];
                    let mut any_covered = false;

                    for lane in 0..4 {
                        let (px, py) = coords[lane];
                        let fx = px as f32 + 0.5;
                        let fy = py as f32 + 0.5;
                        let l0 = edge_fn(s1.x, s1.y, s2.x, s2.y, fx, fy) * inv_area;
                        let l1 = edge_fn(s2.x, s2.y, s0.x, s0.y, fx, fy) * inv_area;
                        let l2 = edge_fn(s0.x, s0.y, s1.x, s1.y, fx, fy) * inv_area;

                        let inside = l0 >= 0.0 && l1 >= 0.0 && l2 >= 0.0;
                        let in_vp = px >= 0 && py >= 0 && px < width as i32 && py < height as i32;
                        let z = l0 * s0.z + l1 * s1.z + l2 * s2.z;
                        let inv_w = l0 * inv_w0 + l1 * inv_w1 + l2 * inv_w2;
                        let p_bary = if abs(inv_w) > 1e-12 {
                            vec3((l0 * inv_w0) / inv_w, (l1 * inv_w1) / inv_w, (l2 * inv_w2) / inv_w)
                        } else {
                            vec3(l0, l1, l2)
                        };

                        frag_pos[lane] = vec4(fx, fy, z, inv_w);
                        bary_persp[lane] = p_bary;
                        if inside && in_vp && (0.0..=1.0).contains(&z) {
                            covered[lane] = true;
                            any_covered = true;
                        }
                    }

                    if any_covered {
                        let lane_vals = [
                            interp(&v0, &v1, &v2, bary_persp[0]),
                            interp(&v0, &v1, &v2, bary_persp[1]),
                            interp(&v0, &v1, &v2, bary_persp[2]),
                            interp(&v0, &v1, &v2, bary_persp[3]),
                        ];
                        for lane in 0..4 {
                            if !covered[lane] {
                                continue;
                            }
                            let ctx = QuadCtx { lanes: &lane_vals, lane };
                            if let FragOutcome::Keep(c) = fs(frag_pos[lane], &ctx) {
                                let (px, py) = coords[lane];
                                let idx = (py as usize) * (width as usize) + (px as usize);
                                let z = frag_pos[lane].z;
                                let pass = match depth.as_deref_mut() {
                                    Some(db) => {
                                        if z < db[idx] {
                                            db[idx] = z;
                                            true
                                        } else {
                                            false
                                        }
                                    }
                                    None => true,
                                };
                                if pass {
                                    color[idx] = c;
                                }
                            }
                        }
                    }
                    qx += 2;
                }
                qy += 2;
            }
        }
    }
}

#[inline]
fn edge_fn(ax: f32, ay: f32, bx: f32, by: f32, px: f32, py: f32) -> f32 {
    (px - ax) * (by - ay) - (py - ay) * (bx - ax)
}
