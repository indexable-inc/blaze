//! GPU intrinsics: fragment-quad derivatives, `discard`, and 2D texture sampling/loading.
//!
//! On the device, the backend lowers each function by its path. On the host:
//! - [`Texture2d::sample`], [`Texture2d::sample_level`] and [`Texture2d::load`] execute real
//!   RGBA32F mip-chain filtering (`Nearest`, bilinear/trilinear `Linear`, `ClampToEdge`, `Repeat`,
//!   `MirroredRepeat`) against slice-backed textures with zero heap allocations.
//! - [`dpdx`], [`dpdy`], [`fwidth`] and [`discard`] require a 2×2 fragment quad; on the CPU use
//!   [`crate::cpu::eval_quad_2x2`] or [`crate::cpu::rasterize_triangles`] which supply explicit
//!   quad context with zero global mutable state.

use crate::entry::{AddressMode, Filter, Sampler, Texture2d};
use crate::math::{floor, fract, lerp};
use crate::vec::{IVec2, Vec2, Vec4};

#[cold]
#[inline(never)]
#[track_caller]
pub(crate) fn device_only(what: &str) -> ! {
    panic!(
        "gpu::{what} runs only on a GPU or inside cpu::eval_quad_2x2 / cpu::rasterize_triangles"
    )
}

/// How fast `v` changes across the quad, in x: Slang's `ddx`.
#[inline(never)]
pub fn dpdx<T: Copy>(v: T) -> T {
    let _ = v;
    device_only("intrinsic::dpdx")
}

/// How fast `v` changes across the quad, in y: Slang's `ddy`.
#[inline(never)]
pub fn dpdy<T: Copy>(v: T) -> T {
    let _ = v;
    device_only("intrinsic::dpdy")
}

/// `abs(dpdx(v)) + abs(dpdy(v))`: Slang's `fwidth`.
#[inline(never)]
pub fn fwidth<T: Copy>(v: T) -> T {
    let _ = v;
    device_only("intrinsic::fwidth")
}

/// Drop this fragment: Slang's `discard`.
#[inline(never)]
pub fn discard() -> ! {
    device_only("intrinsic::discard")
}

#[inline]
fn wrap_uv(u: f32, mode: AddressMode) -> f32 {
    match mode {
        AddressMode::ClampToEdge => u.clamp(0.0, 1.0),
        AddressMode::Repeat => fract(u),
        AddressMode::MirroredRepeat => {
            let t = fract(u * 0.5) * 2.0;
            if t > 1.0 { 2.0 - t } else { t }
        }
    }
}

#[inline]
fn wrap_texel(idx: i32, size: u32, mode: AddressMode) -> usize {
    let n = size as i32;
    match mode {
        AddressMode::ClampToEdge => idx.clamp(0, n - 1) as usize,
        AddressMode::Repeat => idx.rem_euclid(n) as usize,
        AddressMode::MirroredRepeat => {
            let period = 2 * n;
            let m = idx.rem_euclid(period);
            if m < n { m as usize } else { (period - 1 - m) as usize }
        }
    }
}

fn sample_mip(tex: &Texture2d, smp: &Sampler, uv: Vec2, level: u32) -> Vec4 {
    let (w, h, data) = tex.mip_slice(level);
    let stride = w as usize;
    match smp.filter {
        Filter::Nearest => {
            let u = wrap_uv(uv.x, smp.address_u);
            let v = wrap_uv(uv.y, smp.address_v);
            let ix = (floor(u * (w as f32)) as i32).clamp(0, (w as i32) - 1) as usize;
            let iy = (floor(v * (h as f32)) as i32).clamp(0, (h as i32) - 1) as usize;
            data[iy * stride + ix]
        }
        Filter::Linear => {
            let x = uv.x * (w as f32) - 0.5;
            let y = uv.y * (h as f32) - 0.5;
            let ix0 = floor(x) as i32;
            let iy0 = floor(y) as i32;
            let fx = x - (ix0 as f32);
            let fy = y - (iy0 as f32);

            let x0 = wrap_texel(ix0, w, smp.address_u);
            let x1 = wrap_texel(ix0 + 1, w, smp.address_u);
            let y0 = wrap_texel(iy0, h, smp.address_v);
            let y1 = wrap_texel(iy0 + 1, h, smp.address_v);

            let c00 = data[y0 * stride + x0];
            let c10 = data[y0 * stride + x1];
            let c01 = data[y1 * stride + x0];
            let c11 = data[y1 * stride + x1];

            let top = lerp(c00, c10, fx);
            let bot = lerp(c01, c11, fx);
            lerp(top, bot, fy)
        }
    }
}

impl Texture2d {
    /// The texture filtered at `uv` (0 to 1): Slang's `Sample`. On the device the mip level is
    /// chosen from the quad's derivatives; on the host it samples mip 0 with `smp`.
    #[inline(never)]
    pub fn sample(&self, smp: &Sampler, uv: Vec2) -> Vec4 {
        self.sample_level(smp, uv, 0.0)
    }

    /// The texture filtered at `uv` at mip `level`: Slang's `SampleLevel`. Supports bilinear and
    /// trilinear filtering across mip levels on the host.
    #[inline(never)]
    pub fn sample_level(&self, smp: &Sampler, uv: Vec2, level: f32) -> Vec4 {
        let max_mip = self.mip_levels().saturating_sub(1);
        let clamped = level.clamp(0.0, max_mip as f32);
        match smp.filter {
            Filter::Nearest => {
                let mip = crate::math::round(clamped) as u32;
                sample_mip(self, smp, uv, mip)
            }
            Filter::Linear => {
                let l0 = floor(clamped) as u32;
                let l1 = (l0 + 1).min(max_mip);
                let frac_l = clamped - (l0 as f32);
                let c0 = sample_mip(self, smp, uv, l0);
                if l0 == l1 || frac_l == 0.0 {
                    c0
                } else {
                    let c1 = sample_mip(self, smp, uv, l1);
                    lerp(c0, c1, frac_l)
                }
            }
        }
    }

    /// The texel at `at` of mip `level`, unfiltered: Slang's `Load`. Out-of-bounds coordinates
    /// return `Vec4::splat(0.0)`, matching Metal/WGSL/D3D12 robust texel load semantics.
    #[inline(never)]
    pub fn load(&self, at: IVec2, level: u32) -> Vec4 {
        if level >= self.mip_levels() {
            return Vec4::splat(0.0);
        }
        let (w, h, data) = self.mip_slice(level);
        if at.x < 0 || at.y < 0 || at.x >= w as i32 || at.y >= h as i32 {
            return Vec4::splat(0.0);
        }
        data[(at.y as usize) * (w as usize) + (at.x as usize)]
    }
}
