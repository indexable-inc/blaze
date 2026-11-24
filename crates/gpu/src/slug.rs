//! Pure Rust GPU port of `den.slang`'s Slug vector font rendering shader + unified
//! WebGPU & Metal 2.4 SDF / Fillet / Ring / Soft-Shadow / Image-Atlas pipeline.
//!
//! Dogfoods `crates/gpu` + `crates/gpu-macros` end-to-end:
//! - Every shader function (`slug_root_code`, `slug_solve_horiz`, `slug_solve_vert`,
//!   `slug_site_vs`, `slug_site_fs`) is written ONCE in **real, type-checked Rust** using
//!   `#[derive(gpu::Pod, gpu::ShaderStruct)]`, `#[gpu::helper]`, `#[gpu::vertex(ir)]`, and
//!   `#[gpu::fragment(ir)]`.
//! - The exact same Rust functions run directly on CPU (including inside
//!   [`crate::cpu::rasterize_triangles`]) AND emit linked **WebGPU WGSL**
//!   ([`emit_slug_site_wgsl`]) and **Metal Shading Language 2.4** ([`emit_slug_site_msl`])
//!   with ZERO handwritten WGSL and ZERO Slang.
//!
//! Algorithm: Eric Lengyel, "GPU-Centered Font Rendering Directly from Glyph Outlines",
//! JCGT 6(2), 2017 (patent dedicated to the public domain; reference shaders MIT/Apache-2.0).

use core::fmt;

use crate as gpu;
use crate::emit::{IrEntry, IrTy, ShaderModule, emit_msl, emit_wgsl_pipeline};
use crate::prelude::*;

/// Primitive kind constants packed in [`SiteInst::fx`]`[0]`.
pub mod site_kind {
    /// `den` Slug vector outline glyph (evaluated from quadratic Bézier `curves` + `bands`).
    pub const SLUG_GLYPH: f32 = 0.0;
    /// Per-corner rounded rectangle (`a = [tl, tr, br, bl]` corner radii in device px).
    pub const PLATE: f32 = 1.0;
    /// Selection copy-pulse sweeping light band (`fx.z = phase`).
    pub const LIGHT: f32 = 2.0;
    /// Concave inner-corner selection fillet (`a.xy = towards` corner direction).
    pub const FILLET: f32 = 3.0;
    /// Premultiplied RGBA icon/signature from sprite atlas (`fx.z = 1.0` for ink-tinted mask).
    pub const IMAGE: f32 = 4.0;
    /// Inset ring / hollow circle marker (`fx.z = ring width in device px`).
    pub const RING: f32 = 5.0;
    /// Soft SDF drop shadow (`fx.z = blur radius in device px`).
    pub const SHADOW: f32 = 6.0;
}

/// Frame uniform buffer at `@group(0) @binding(0)` (64 bytes, 16-byte aligned).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, gpu::Pod, gpu::ShaderStruct)]
pub struct SiteUniform {
    /// `[viewport_w_dev, viewport_h_dev, origin_x_dev, origin_y_dev]`.
    pub view_origin: Vec4,
    /// `[pulse_center_x_dev, pulse_center_y_dev, pulse_scale, pulse_glow]`.
    pub pulse: Vec4,
    /// `[pulse_ink_r, pulse_ink_g, pulse_ink_b, dark_appearance_0_or_1]`.
    pub pulse_ink: Vec4,
    /// `den` Slug tuning: `[embolden_em, gamma_dark, gamma_light, embolden_max_px]`.
    pub slug: Vec4,
}

/// Unified instanced quad at `@group(0) @binding(1)` (96 bytes, 16-byte aligned).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, gpu::Pod, gpu::ShaderStruct)]
pub struct SiteInst {
    /// Quad `[x, y, w, h]` in device pixels from stage top-left.
    pub rect: Vec4,
    /// Kind-specific payload:
    /// - `SLUG_GLYPH`: `[em_x0, em_y1, em_w, -em_h]` (top-left em + signed em span, y up)
    /// - `PLATE`/`LIGHT`/`RING`/`SHADOW`: `[r_tl, r_tr, r_br, r_bl]` corner radii in device px
    /// - `FILLET`: `[towards_x, towards_y, 0, 0]`
    /// - `IMAGE`: `[u0, v0, uw, vh]` normalized atlas UV rect
    pub a: Vec4,
    /// Straight RGBA color `[r, g, b, a]`.
    pub color: Vec4,
    /// `[kind, pulse_weight, param_z, param_w]`.
    pub fx: Vec4,
    /// `den` Slug band transform `[scale_x, scale_y, offset_x, offset_y]`.
    pub band_tf: Vec4,
    /// `den` Slug band descriptor `[band_start, max_v_band, max_h_band, flags]`.
    pub gdata: UVec4,
}

const _: () = {
    assert!(core::mem::size_of::<SiteUniform>() == 64);
    assert!(core::mem::size_of::<SiteInst>() == 96);
};

#[derive(Clone, Copy, gpu::Varyings)]
pub struct SlugSiteVaryings {
    pub pos: Position,
    pub uv: Vec2,
    pub local: Vec2,
    pub color: Flat<Vec4>,
    pub half_ext: Flat<Vec2>,
    pub radii: Flat<Vec4>,
    pub fx: Flat<Vec4>,
    pub band_tf: Flat<Vec4>,
    pub gdata: Flat<UVec4>,
    pub ppem: Flat<f32>,
}

/// Port of `den.slang::slug_root_code` (Eric Lengyel `CalcRootCode`) in plain Rust.
#[gpu::helper]
pub fn slug_root_code(y1: f32, y2: f32, y3: f32) -> u32 {
    let mut s: u32 = 0u32;
    if y1 < 0.0 {
        s |= 1u32;
    }
    if y2 < 0.0 {
        s |= 2u32;
    }
    if y3 < 0.0 {
        s |= 4u32;
    }
    (0x2E74u32 >> s) & 0x0101u32
}

/// Port of `den.slang::slug_solve_horiz` (guarded quadratic root solver along `y = 0`) in plain Rust.
#[allow(unused_assignments, reason = "WGSL var requires initializer before conditional assignment")]
#[gpu::helper]
pub fn slug_solve_horiz(p12: Vec4, p3: Vec2) -> Vec2 {
    let a: Vec2 = p12.xy() - p12.zw() * 2.0 + p3;
    let b: Vec2 = p12.xy() - p12.zw();
    let mut t1: f32 = 0.0;
    let mut t2: f32 = 0.0;
    if abs(a.y) < 0.000015258789 {
        let mut denom_b: f32 = b.y;
        if denom_b == 0.0 {
            denom_b = 1.0;
        }
        t1 = (p12.y * 0.5) / denom_b;
        t2 = t1;
    } else {
        let d: f32 = sqrt((b.y * b.y - a.y * p12.y).max(0.0));
        let ra: f32 = 1.0 / a.y;
        t1 = (b.y - d) * ra;
        t2 = (b.y + d) * ra;
    }
    vec2(
        (a.x * t1 - b.x * 2.0) * t1 + p12.x,
        (a.x * t2 - b.x * 2.0) * t2 + p12.x,
    )
}

/// Port of `den.slang::slug_solve_vert` (guarded quadratic root solver along `x = 0`) in plain Rust.
#[allow(unused_assignments, reason = "WGSL var requires initializer before conditional assignment")]
#[gpu::helper]
pub fn slug_solve_vert(p12: Vec4, p3: Vec2) -> Vec2 {
    let a: Vec2 = p12.xy() - p12.zw() * 2.0 + p3;
    let b: Vec2 = p12.xy() - p12.zw();
    let mut t1: f32 = 0.0;
    let mut t2: f32 = 0.0;
    if abs(a.x) < 0.000015258789 {
        let mut denom_b: f32 = b.x;
        if denom_b == 0.0 {
            denom_b = 1.0;
        }
        t1 = (p12.x * 0.5) / denom_b;
        t2 = t1;
    } else {
        let d: f32 = sqrt((b.x * b.x - a.x * p12.x).max(0.0));
        let ra: f32 = 1.0 / a.x;
        t1 = (b.x - d) * ra;
        t2 = (b.x + d) * ra;
    }
    vec2(
        (a.y * t1 - b.y * 2.0) * t1 + p12.y,
        (a.y * t2 - b.y * 2.0) * t2 + p12.y,
    )
}

/// Unified vertex stage written in real Rust -> lowers directly to WGSL & MSL 2.4.
#[gpu::vertex(ir)]
pub fn slug_site_vs(
    vi: VertexIndex,
    ii: InstanceIndex,
    u: &SiteUniform,
    insts: &[SiteInst],
) -> SlugSiteVaryings {
    let inst = at(insts, ii.get());
    let mut cx: f32 = 0.0;
    let mut cy: f32 = 0.0;
    if vi.get() == 1u32 || vi.get() == 2u32 || vi.get() == 4u32 {
        cx = 1.0;
    }
    if vi.get() == 2u32 || vi.get() == 4u32 || vi.get() == 5u32 {
        cy = 1.0;
    }
    let corner: Vec2 = vec2(cx, cy);
    let grow: f32 = 1.0 + (u.pulse.z - 1.0) * inst.fx.y;
    let p0: Vec2 = inst.rect.xy() + corner * inst.rect.zw();
    let p: Vec2 = u.pulse.xy() + (p0 - u.pulse.xy()) * grow - u.view_origin.zw();
    let half_e: Vec2 = inst.rect.zw() * 0.5 * grow;
    let ndc_x: f32 = (p.x / u.view_origin.x) * 2.0 - 1.0;
    let ndc_y: f32 = 1.0 - (p.y / u.view_origin.y) * 2.0;
    let ppem_v: f32 = (inst.rect.z * grow) / abs(inst.a.z).max(0.000001);
    return SlugSiteVaryings {
        pos: Position(vec4(ndc_x, ndc_y, 0.0, 1.0)),
        uv: inst.a.xy() + corner * inst.a.zw(),
        local: (corner - vec2(0.5, 0.5)) * 2.0 * half_e,
        color: Flat(inst.color),
        half_ext: Flat(half_e),
        radii: Flat(inst.a),
        fx: Flat(inst.fx),
        band_tf: Flat(inst.band_tf),
        gdata: Flat(inst.gdata),
        ppem: Flat(ppem_v),
    };
}

/// Unified fragment stage (`den` Slug Bézier glyphs + per-corner SDF plates + selection fillets +
/// copy-pulse sweep + inset rings + soft drop shadows + SVG sprite atlas) written in real Rust!
#[gpu::fragment(ir)]
pub fn slug_site_fs(
    v: SlugSiteVaryings,
    u: &SiteUniform,
    curves: &[Vec4],
    bands: &[u32],
    atlas_samp: &Sampler,
    atlas_tex: &Texture2d,
) -> Vec4 {
    let k: f32 = v.fx.0.x;
    // Kind 0: `den` Slug GPU Quadratic Bézier Outline Glyph (sharp at ANY trackpad zoom!).
    if k < 0.5 {
        let render: Vec2 = v.uv;
        let ppem: f32 = v.ppem.0;
        let e: f32 = (ppem * u.slug.x).min(u.slug.w);
        let at_off: u32 = v.gdata.0.x;
        let max_v: u32 = v.gdata.0.y;
        let max_h: u32 = v.gdata.0.z;
        let bx_f: f32 = floor(render.x * v.band_tf.0.x + v.band_tf.0.z).clamp(0.0, to_f32(max_v));
        let by_f: f32 = floor(render.y * v.band_tf.0.y + v.band_tf.0.w).clamp(0.0, to_f32(max_h));
        let bx: u32 = to_u32(bx_f);
        let by: u32 = to_u32(by_f);

        // Horizontal ray pass
        let mut xcov: f32 = 0.0;
        let mut xwgt: f32 = 0.0;
        let hb: u32 = at_off + 2u32 * by;
        let hcount: u32 = at(bands, hb);
        let hlist: u32 = at_off + at(bands, hb + 1u32);
        let mut hi: u32 = 0u32;
        while hi < hcount {
            let slot: u32 = at(bands, hlist + hi);
            let c0: Vec4 = at(curves, slot);
            let c1: Vec4 = at(curves, slot + 1u32);
            let p12: Vec4 = c0 - vec4(render.x, render.y, render.x, render.y);
            let p3: Vec2 = c1.xy() - render;
            if p12.x.max(p12.z).max(p3.x) * ppem < -0.5 - e {
                hi = hcount;
            } else {
                let code: u32 = slug_root_code(p12.y, p12.w, p3.y);
                if code != 0u32 {
                    let r: Vec2 = slug_solve_horiz(p12, p3) * ppem;
                    if (code & 1u32) != 0u32 {
                        let rx: f32 = r.x - e;
                        xcov += saturate(rx + 0.5);
                        xwgt = xwgt.max(saturate(1.0 - abs(rx) * 2.0));
                    }
                    if code > 1u32 {
                        let ry: f32 = r.y + e;
                        xcov -= saturate(ry + 0.5);
                        xwgt = xwgt.max(saturate(1.0 - abs(ry) * 2.0));
                    }
                }
                hi += 1u32;
            }
        }

        // Vertical ray pass
        let mut ycov: f32 = 0.0;
        let mut ywgt: f32 = 0.0;
        let vb: u32 = at_off + 2u32 * (max_h + 1u32 + bx);
        let vcount: u32 = at(bands, vb);
        let vlist: u32 = at_off + at(bands, vb + 1u32);
        let mut vi_idx: u32 = 0u32;
        while vi_idx < vcount {
            let slot: u32 = at(bands, vlist + vi_idx);
            let c0: Vec4 = at(curves, slot);
            let c1: Vec4 = at(curves, slot + 1u32);
            let p12: Vec4 = c0 - vec4(render.x, render.y, render.x, render.y);
            let p3: Vec2 = c1.xy() - render;
            if p12.y.max(p12.w).max(p3.y) * ppem < -0.5 - e {
                vi_idx = vcount;
            } else {
                let code: u32 = slug_root_code(p12.x, p12.z, p3.x);
                if code != 0u32 {
                    let r: Vec2 = slug_solve_vert(p12, p3) * ppem;
                    if (code & 1u32) != 0u32 {
                        let rx: f32 = r.x + e;
                        ycov -= saturate(rx + 0.5);
                        ywgt = ywgt.max(saturate(1.0 - abs(rx) * 2.0));
                    }
                    if code > 1u32 {
                        let ry: f32 = r.y - e;
                        ycov += saturate(ry + 0.5);
                        ywgt = ywgt.max(saturate(1.0 - abs(ry) * 2.0));
                    }
                }
                vi_idx += 1u32;
            }
        }

        let wcov: f32 = abs(xcov * xwgt + ycov * ywgt) / (xwgt + ywgt).max(0.000015258789);
        let cov: f32 = saturate(wcov.max(abs(xcov).min(abs(ycov))));
        let mut gam: f32 = u.slug.z;
        if u.pulse_ink.w > 0.5 {
            gam = u.slug.y;
        }
        let ink: f32 = pow(cov, gam);
        let lit: f32 = u.pulse.w * v.fx.0.w;
        let a: f32 = ink * mix(v.color.0.w, 1.0, lit);
        let rgb: Vec3 = mix(v.color.0.xyz(), u.pulse_ink.xyz(), lit) * a;
        return vec4(rgb.x, rgb.y, rgb.z, a);
    }

    // Kind 3: Concave inner-corner selection fillet
    if k > 2.5 && k < 3.5 {
        let d: f32 = length(v.local - v.radii.0.xy() * v.half_ext.0) - 2.0 * v.half_ext.0.x;
        let a: f32 = saturate(d + 0.5) * v.color.0.w;
        let rgb: Vec3 = v.color.0.xyz() * a;
        return vec4(rgb.x, rgb.y, rgb.z, a);
    }

    // Kind 4: Premultiplied RGBA icon / signature atlas (derivative-free mip 0 sample)
    if k > 3.5 && k < 4.5 {
        let t: Vec4 = atlas_tex.sample_level(atlas_samp, v.uv, 0.0);
        if v.fx.0.z > 0.5 {
            let a: f32 = t.w * v.color.0.w;
            let rgb: Vec3 = v.color.0.xyz() * a;
            return vec4(rgb.x, rgb.y, rgb.z, a);
        }
        return t * v.color.0.w;
    }

    // Kinds 1, 2, 5, 6: Per-corner rounded rectangle SDF
    let is_r: bool = v.local.x > 0.0;
    let is_d: bool = v.local.y > 0.0;
    let mut rad: f32 = v.radii.0.x;
    if is_r && !is_d {
        rad = v.radii.0.y;
    }
    if is_r && is_d {
        rad = v.radii.0.z;
    }
    if !is_r && is_d {
        rad = v.radii.0.w;
    }
    let q: Vec2 = v.local.abs() - v.half_ext.0 + vec2(rad, rad);
    let d: f32 = q.x.max(q.y).min(0.0) + length(q.max(vec2(0.0, 0.0))) - rad;

    // Kind 6: Soft SDF drop shadow
    if k > 5.5 {
        let a: f32 = (1.0 - smoothstep(-v.fx.0.z, v.fx.0.z, d)) * v.color.0.w;
        let rgb: Vec3 = v.color.0.xyz() * a;
        return vec4(rgb.x, rgb.y, rgb.z, a);
    }

    // Kind 5: Inset ring / hollow circle marker
    if k > 4.5 {
        let outer_a: f32 = saturate(0.5 - d);
        let inner_a: f32 = saturate(0.5 - d - v.fx.0.z);
        let a: f32 = (outer_a - inner_a) * v.color.0.w;
        let rgb: Vec3 = v.color.0.xyz() * a;
        return vec4(rgb.x, rgb.y, rgb.z, a);
    }

    // Kinds 1 & 2: Filled rounded rect plate (plus copy-pulse sweeping light band when kind == 2)
    let mut a: f32 = saturate(0.5 - d) * v.color.0.w;
    if k > 1.5 {
        let w: f32 = v.half_ext.0.x * 2.0;
        let band: f32 = (w * 0.26).max(12.0);
        let at_x: f32 = -band + (w + 2.0 * band) * v.fx.0.z;
        a = a * saturate(1.0 - abs(v.local.x + v.half_ext.0.x - at_x) / band);
    }
    let rgb: Vec3 = v.color.0.xyz() * a;
    return vec4(rgb.x, rgb.y, rgb.z, a);
}

const SLUG_SITE_MODULE: ShaderModule<'static> = ShaderModule {
    structs: &[SiteUniform::IR, SiteInst::IR],
    helpers: &[slug_root_code::IR, slug_solve_horiz::IR, slug_solve_vert::IR],
    entries: &[
        IrEntry {
            desc: &slug_site_vs::ENTRY,
            param_types: &[IrTy::Void, IrTy::Void, IrTy::Struct("SiteUniform"), IrTy::Struct("SiteInst")],
            body: slug_site_vs::IR_BODY,
        },
        IrEntry {
            desc: &slug_site_fs::ENTRY,
            param_types: &[
                IrTy::Void,
                IrTy::Struct("SiteUniform"),
                IrTy::Vec4,
                IrTy::U32,
                IrTy::Void,
                IrTy::Void,
            ],
            body: slug_site_fs::IR_BODY,
        },
    ],
};

/// Emit linked WebGPU WGSL directly from the Rust `#[gpu::vertex(ir)]`/`#[gpu::fragment(ir)]` functions above.
pub fn emit_slug_site_wgsl(out: &mut dyn fmt::Write) -> fmt::Result {
    emit_wgsl_pipeline(&SLUG_SITE_MODULE, out)
}

/// Emit Metal Shading Language 2.4 directly from the Rust `#[gpu::vertex(ir)]`/`#[gpu::fragment(ir)]` functions above.
pub fn emit_slug_site_msl(out: &mut dyn fmt::Write) -> fmt::Result {
    emit_msl(&SLUG_SITE_MODULE, out)
}
