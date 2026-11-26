//! Canonical Rust-authored UI WebGPU & Metal shaders emitted via [`gpu::emit`].
//!
//! Zero handwritten WGSL or MSL strings live anywhere in `blaze-ui` or its WASM host:
//! - Both `ui_sdf` (rounded rects with integrated AA borders, AA lines, AA circles/rings, soft
//!   SDF drop shadows, and fragment-shader rounded-rect clip table) and `ui_glyph` (HiDPI
//!   Berkeley Mono ligature font atlas with fragment-shader rounded-rect clipping) declare real
//!   `#[gpu::varyings]`, `#[gpu::vertex]`, and `#[gpu::fragment]` entries in Rust.
//! - [`emit_ui_sdf_wgsl`] / [`emit_ui_glyph_wgsl`] emit linked WebGPU WGSL via
//!   [`gpu::emit::emit_wgsl_pipeline`].
//! - [`emit_ui_sdf_msl`] / [`emit_ui_glyph_msl`] emit Metal Shading Language 2.4 via
//!   [`gpu::emit::emit_msl`].

use core::fmt;

use gpu::emit::{
    IrBinOp, IrEntry, IrExpr, IrField, IrFn, IrFnParam, IrStmt, IrStruct, IrTy, IrUnaryOp,
    ShaderModule, emit_msl, emit_wgsl_pipeline,
};
use gpu::prelude::{
    Flat, InstanceIndex, Position, Sampler, Texture2d, Vec2, Vec4, VertexIndex, vec2, vec4,
};

// =============================================================================
// Shared 16-Byte Aligned GPU Instance & Clip Structs (`#[repr(C)]`)
// =============================================================================

/// Uniform buffer at `@group(0) @binding(0)` (16 bytes).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ViewportUniform {
    pub size: [f32; 2],
    pub dpr: f32,
    pub time: f32,
}

/// GPU clip table entry at `@group(0) @binding(2)` (32 bytes).
///
/// Fragment shaders evaluate the rounded-rect SDF of `clips[clip_id]` so nested clip regions
/// never break batching or require WebGPU scissor-rect pipeline switches.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ClipRectGpu {
    /// `[x0, y0, x1, y1]` in logical CSS pixels.
    pub rect: [f32; 4],
    /// `[corner_radius, 0.0, 0.0, 0.0]`.
    pub params: [f32; 4],
}

/// SDF primitive kinds packed in `RectInst::params[3]`.
pub mod prim_kind {
    /// Filled rounded rect with optional integrated AA border (`params = [radius, border_w, 0, 0]`).
    pub const ROUND_RECT: f32 = 0.0;
    /// Anti-aliased SDF line segment (`rect = [x0, y0, x1, y1]`, `params = [thickness, 0, 0, 1]`).
    pub const LINE: f32 = 1.0;
    /// Anti-aliased SDF circle or ring (`params = [radius, ring_width, 0, 2]`).
    pub const CIRCLE: f32 = 2.0;
    /// Soft SDF drop shadow (`params = [radius, 0, blur, 3]`).
    pub const SHADOW: f32 = 3.0;
}

/// Instanced SDF geometry primitive (80 bytes, 16-byte aligned).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RectInst {
    /// `[x0, y0, x1, y1]` in logical CSS pixels (or segment endpoints for `LINE`).
    pub rect: [f32; 4],
    /// Primary fill / stroke / shadow RGBA color.
    pub color: [f32; 4],
    /// Integrated border RGBA color (used by `ROUND_RECT` when `params[1] > 0.0`).
    pub border_color: [f32; 4],
    /// `[radius_or_thickness, border_or_ring_width, shadow_blur, kind]`.
    pub params: [f32; 4],
    /// `[clip_id, flags, 0, 0]`.
    pub clip_info: [u32; 4],
}

/// Instanced HiDPI font-atlas glyph (64 bytes, 16-byte aligned).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GlyphInst {
    /// `[x0, y0, x1, y1]` destination quad in logical CSS pixels.
    pub rect: [f32; 4],
    /// `[u0, v0, u1, v1]` normalized atlas UV coordinates.
    pub uv: [f32; 4],
    /// Foreground RGBA color.
    pub color: [f32; 4],
    /// `[clip_id, flags, 0, 0]`.
    pub clip_info: [u32; 4],
}

const _: () = {
    assert!(core::mem::size_of::<ViewportUniform>() == 16);
    assert!(core::mem::size_of::<ClipRectGpu>() == 32);
    assert!(core::mem::size_of::<RectInst>() == 80);
    assert!(core::mem::size_of::<GlyphInst>() == 64);
};

// Safe `gpu::Pod` stand-in carrier types with identical sizes so `#![forbid(unsafe_code)]` holds.
pub type Pod16 = Vec4;
pub type Pod32 = [Vec4; 2];
pub type Pod64 = [Vec4; 4];
pub type Pod80 = [Vec4; 5];

// =============================================================================
// `#[gpu::varyings]`, `#[gpu::vertex]`, and `#[gpu::fragment]` Entry Declarations
// =============================================================================

#[derive(Clone, Copy, gpu::Varyings)]
pub struct SdfVaryings {
    pub pos: Position,
    pub px: Vec2,
    pub rect: Vec4,
    pub color: Vec4,
    pub border_color: Vec4,
    pub params: Vec4,
    pub clip_id: Flat<u32>,
}

#[derive(Clone, Copy, gpu::Varyings)]
pub struct GlyphVaryings {
    pub pos: Position,
    pub px: Vec2,
    pub uv: Vec2,
    pub color: Vec4,
    pub clip_id: Flat<u32>,
}

#[gpu::vertex]
pub fn ui_sdf_vs(
    vi: VertexIndex,
    ii: InstanceIndex,
    vp: &Pod16,
    insts: &[Pod80],
) -> SdfVaryings {
    let _ = (vi, ii, vp, insts);
    SdfVaryings {
        pos: Position(vec4(0.0, 0.0, 0.0, 1.0)),
        px: vec2(0.0, 0.0),
        rect: Vec4::splat(0.0),
        color: Vec4::splat(1.0),
        border_color: Vec4::splat(0.0),
        params: Vec4::splat(0.0),
        clip_id: Flat(0),
    }
}

#[gpu::fragment]
pub fn ui_sdf_fs(v: SdfVaryings, clips: &[Pod32]) -> Vec4 {
    let _ = clips;
    v.color
}

#[gpu::vertex]
pub fn ui_glyph_vs(
    vi: VertexIndex,
    ii: InstanceIndex,
    vp: &Pod16,
    glyphs: &[Pod64],
) -> GlyphVaryings {
    let _ = (vi, ii, vp, glyphs);
    GlyphVaryings {
        pos: Position(vec4(0.0, 0.0, 0.0, 1.0)),
        px: vec2(0.0, 0.0),
        uv: vec2(0.0, 0.0),
        color: Vec4::splat(1.0),
        clip_id: Flat(0),
    }
}

#[gpu::fragment]
pub fn ui_glyph_fs(
    v: GlyphVaryings,
    clips: &[Pod32],
    atlas_samp: &Sampler,
    atlas_tex: &Texture2d,
) -> Vec4 {
    let _ = (clips, atlas_samp, atlas_tex);
    v.color
}

// =============================================================================
// Shared Shader IR Structs & SDF Math Helpers
// =============================================================================

const VIEWPORT_FIELDS: [IrField; 3] = [
    IrField { name: "size", ty: IrTy::Vec2 },
    IrField { name: "dpr", ty: IrTy::F32 },
    IrField { name: "time", ty: IrTy::F32 },
];

const CLIP_FIELDS: [IrField; 2] = [
    IrField { name: "rect", ty: IrTy::Vec4 },
    IrField { name: "params", ty: IrTy::Vec4 },
];

const RECT_INST_FIELDS: [IrField; 5] = [
    IrField { name: "rect", ty: IrTy::Vec4 },
    IrField { name: "color", ty: IrTy::Vec4 },
    IrField { name: "border_color", ty: IrTy::Vec4 },
    IrField { name: "params", ty: IrTy::Vec4 },
    IrField { name: "clip_info", ty: IrTy::UVec4 },
];

const GLYPH_INST_FIELDS: [IrField; 4] = [
    IrField { name: "rect", ty: IrTy::Vec4 },
    IrField { name: "uv", ty: IrTy::Vec4 },
    IrField { name: "color", ty: IrTy::Vec4 },
    IrField { name: "clip_info", ty: IrTy::UVec4 },
];

const SDF_STRUCTS: [IrStruct; 3] = [
    IrStruct { name: "Viewport", fields: &VIEWPORT_FIELDS },
    IrStruct { name: "ClipRect", fields: &CLIP_FIELDS },
    IrStruct { name: "RectInst", fields: &RECT_INST_FIELDS },
];

const GLYPH_STRUCTS: [IrStruct; 3] = [
    IrStruct { name: "Viewport", fields: &VIEWPORT_FIELDS },
    IrStruct { name: "ClipRect", fields: &CLIP_FIELDS },
    IrStruct { name: "GlyphInst", fields: &GLYPH_INST_FIELDS },
];

const SD_ROUND_BOX_PARAMS: [IrFnParam; 4] = [
    IrFnParam { name: "p", ty: IrTy::Vec2 },
    IrFnParam { name: "center", ty: IrTy::Vec2 },
    IrFnParam { name: "half_ext", ty: IrTy::Vec2 },
    IrFnParam { name: "r", ty: IrTy::F32 },
];

const SD_SEGMENT_PARAMS: [IrFnParam; 3] = [
    IrFnParam { name: "p", ty: IrTy::Vec2 },
    IrFnParam { name: "a", ty: IrTy::Vec2 },
    IrFnParam { name: "b", ty: IrTy::Vec2 },
];

#[derive(Clone, Copy)]
enum TargetLang {
    Wgsl,
    Msl,
}

fn emit_sdf_shader(lang: TargetLang, out: &mut dyn fmt::Write) -> fmt::Result {
    // Helper 1: sd_round_box(p, center, half_ext, r) -> f32
    let p_var = IrExpr::Var("p");
    let c_var = IrExpr::Var("center");
    let he_var = IrExpr::Var("half_ext");
    let r_var = IrExpr::Var("r");
    let r_clamped_init = IrExpr::Call(
        "min",
        &[r_var, IrExpr::Call("min", &[IrExpr::Field(&he_var, "x"), IrExpr::Field(&he_var, "y")])],
    );
    let rc_var = IrExpr::Var("rc");
    let rc_vec = IrExpr::Construct(IrTy::Vec2, &[rc_var, rc_var]);
    let abs_diff = IrExpr::Call("abs", &[IrExpr::Binary(IrBinOp::Sub, &p_var, &c_var)]);
    let q_init = IrExpr::Binary(
        IrBinOp::Add,
        &IrExpr::Binary(IrBinOp::Sub, &abs_diff, &he_var),
        &rc_vec,
    );
    let q_var = IrExpr::Var("q");
    let zero2 = IrExpr::Construct(IrTy::Vec2, &[IrExpr::LitF32(0.0), IrExpr::LitF32(0.0)]);
    let outside = IrExpr::Call("length", &[IrExpr::Call("max", &[q_var, zero2])]);
    let inside = IrExpr::Call(
        "min",
        &[
            IrExpr::Call("max", &[IrExpr::Field(&q_var, "x"), IrExpr::Field(&q_var, "y")]),
            IrExpr::LitF32(0.0),
        ],
    );
    let sd_box_ret = IrExpr::Binary(
        IrBinOp::Sub,
        &IrExpr::Binary(IrBinOp::Add, &outside, &inside),
        &rc_var,
    );
    let sd_box_body = [
        IrStmt::Let { name: "rc", ty: None, mutable: false, init: r_clamped_init },
        IrStmt::Let { name: "q", ty: None, mutable: false, init: q_init },
        IrStmt::Return(Some(sd_box_ret)),
    ];

    // Helper 2: sd_segment(p, a, b) -> f32
    let a_var = IrExpr::Var("a");
    let b_var = IrExpr::Var("b");
    let pa_init = IrExpr::Binary(IrBinOp::Sub, &p_var, &a_var);
    let ba_init = IrExpr::Binary(IrBinOp::Sub, &b_var, &a_var);
    let pa_var = IrExpr::Var("pa");
    let ba_var = IrExpr::Var("ba");
    let denom = IrExpr::Call("max", &[IrExpr::Call("dot", &[ba_var, ba_var]), IrExpr::LitF32(0.0001)]);
    let h_init = IrExpr::Call(
        "clamp",
        &[
            IrExpr::Binary(IrBinOp::Div, &IrExpr::Call("dot", &[pa_var, ba_var]), &denom),
            IrExpr::LitF32(0.0),
            IrExpr::LitF32(1.0),
        ],
    );
    let h_var = IrExpr::Var("h");
    let seg_ret = IrExpr::Call(
        "length",
        &[IrExpr::Binary(IrBinOp::Sub, &pa_var, &IrExpr::Binary(IrBinOp::Mul, &ba_var, &h_var))],
    );
    let sd_seg_body = [
        IrStmt::Let { name: "pa", ty: None, mutable: false, init: pa_init },
        IrStmt::Let { name: "ba", ty: None, mutable: false, init: ba_init },
        IrStmt::Let { name: "h", ty: None, mutable: false, init: h_init },
        IrStmt::Return(Some(seg_ret)),
    ];

    let helpers = [
        IrFn { name: "sd_round_box", params: &SD_ROUND_BOX_PARAMS, ret: IrTy::F32, body: &sd_box_body },
        IrFn { name: "sd_segment", params: &SD_SEGMENT_PARAMS, ret: IrTy::F32, body: &sd_seg_body },
    ];

    // Vertex shader: expand quad by kind-specific AA/shadow pad and map CSS px -> NDC.
    let vi = IrExpr::Var("vi");
    let ii = IrExpr::Var("ii");
    let vp = IrExpr::Var("vp");
    let insts = IrExpr::Var("insts");
    let inst = IrExpr::Var("inst");
    let inst_rect = IrExpr::Field(&inst, "rect");
    let inst_col = IrExpr::Field(&inst, "color");
    let inst_bcol = IrExpr::Field(&inst, "border_color");
    let inst_params = IrExpr::Field(&inst, "params");
    let inst_meta = IrExpr::Field(&inst, "clip_info");
    let inst_xy = IrExpr::Field(&inst_rect, "xy");
    let inst_zw = IrExpr::Field(&inst_rect, "zw");
    let kind_v = IrExpr::Field(&inst_params, "w");

    let pmin_var = IrExpr::Var("pmin");
    let pmax_var = IrExpr::Var("pmax");

    let line_cond = IrExpr::Binary(
        IrBinOp::And,
        &IrExpr::Binary(IrBinOp::Gt, &kind_v, &IrExpr::LitF32(0.5)),
        &IrExpr::Binary(IrBinOp::Lt, &kind_v, &IrExpr::LitF32(1.5)),
    );
    let line_pad_init = IrExpr::Binary(IrBinOp::Add, &IrExpr::Field(&inst_params, "x"), &IrExpr::LitF32(2.0));
    let pad_var = IrExpr::Var("pad");
    let pad_vec = IrExpr::Construct(IrTy::Vec2, &[pad_var, pad_var]);
    let line_then = [
        IrStmt::Let { name: "pad", ty: None, mutable: false, init: line_pad_init },
        IrStmt::Assign {
            target: pmin_var,
            value: IrExpr::Binary(IrBinOp::Sub, &IrExpr::Call("min", &[inst_xy, inst_zw]), &pad_vec),
        },
        IrStmt::Assign {
            target: pmax_var,
            value: IrExpr::Binary(IrBinOp::Add, &IrExpr::Call("max", &[inst_xy, inst_zw]), &pad_vec),
        },
    ];

    let shadow_cond = IrExpr::Binary(IrBinOp::Gt, &kind_v, &IrExpr::LitF32(2.5));
    let shadow_pad_init = IrExpr::Binary(
        IrBinOp::Add,
        &IrExpr::Binary(IrBinOp::Mul, &IrExpr::Field(&inst_params, "z"), &IrExpr::LitF32(2.0)),
        &IrExpr::LitF32(2.0),
    );
    let shadow_then = [
        IrStmt::Let { name: "pad", ty: None, mutable: false, init: shadow_pad_init },
        IrStmt::Assign { target: pmin_var, value: IrExpr::Binary(IrBinOp::Sub, &pmin_var, &pad_vec) },
        IrStmt::Assign { target: pmax_var, value: IrExpr::Binary(IrBinOp::Add, &pmax_var, &pad_vec) },
    ];

    let default_pad = IrExpr::Construct(IrTy::Vec2, &[IrExpr::LitF32(1.5), IrExpr::LitF32(1.5)]);
    let default_else = [
        IrStmt::Assign { target: pmin_var, value: IrExpr::Binary(IrBinOp::Sub, &pmin_var, &default_pad) },
        IrStmt::Assign { target: pmax_var, value: IrExpr::Binary(IrBinOp::Add, &pmax_var, &default_pad) },
    ];
    let shadow_if = [IrStmt::If { cond: shadow_cond, then_body: &shadow_then, else_body: &default_else }];

    let quad_arr = IrExpr::Construct(
        IrTy::Array(&IrTy::Vec2, 6),
        &[
            IrExpr::Construct(IrTy::Vec2, &[IrExpr::LitF32(0.0), IrExpr::LitF32(0.0)]),
            IrExpr::Construct(IrTy::Vec2, &[IrExpr::LitF32(1.0), IrExpr::LitF32(0.0)]),
            IrExpr::Construct(IrTy::Vec2, &[IrExpr::LitF32(1.0), IrExpr::LitF32(1.0)]),
            IrExpr::Construct(IrTy::Vec2, &[IrExpr::LitF32(0.0), IrExpr::LitF32(0.0)]),
            IrExpr::Construct(IrTy::Vec2, &[IrExpr::LitF32(1.0), IrExpr::LitF32(1.0)]),
            IrExpr::Construct(IrTy::Vec2, &[IrExpr::LitF32(0.0), IrExpr::LitF32(1.0)]),
        ],
    );
    let quad_var = IrExpr::Var("quad");
    let k_var = IrExpr::Var("k");
    let px_init = IrExpr::Call("lerp", &[pmin_var, pmax_var, k_var]);
    let px_var = IrExpr::Var("px");
    let vp_size = IrExpr::Field(&vp, "size");
    let ndc_x = IrExpr::Binary(
        IrBinOp::Sub,
        &IrExpr::Binary(
            IrBinOp::Mul,
            &IrExpr::Binary(IrBinOp::Div, &IrExpr::Field(&px_var, "x"), &IrExpr::Field(&vp_size, "x")),
            &IrExpr::LitF32(2.0),
        ),
        &IrExpr::LitF32(1.0),
    );
    let ndc_y = IrExpr::Binary(
        IrBinOp::Sub,
        &IrExpr::LitF32(1.0),
        &IrExpr::Binary(
            IrBinOp::Mul,
            &IrExpr::Binary(IrBinOp::Div, &IrExpr::Field(&px_var, "y"), &IrExpr::Field(&vp_size, "y")),
            &IrExpr::LitF32(2.0),
        ),
    );
    let pos_out = IrExpr::Construct(IrTy::Vec4, &[ndc_x, ndc_y, IrExpr::LitF32(0.0), IrExpr::LitF32(1.0)]);
    let clip_id_out = IrExpr::Field(&inst_meta, "x");
    let vs_ret = IrExpr::Construct(
        IrTy::Struct("ui_sdf_vs_out"),
        &[pos_out, px_var, inst_rect, inst_col, inst_bcol, inst_params, clip_id_out],
    );

    let vs_body = [
        IrStmt::Let { name: "inst", ty: None, mutable: false, init: IrExpr::Index(&insts, &ii) },
        IrStmt::Let { name: "pmin", ty: Some(IrTy::Vec2), mutable: true, init: inst_xy },
        IrStmt::Let { name: "pmax", ty: Some(IrTy::Vec2), mutable: true, init: inst_zw },
        IrStmt::If { cond: line_cond, then_body: &line_then, else_body: &shadow_if },
        IrStmt::Let { name: "quad", ty: None, mutable: true, init: quad_arr },
        IrStmt::Let { name: "k", ty: None, mutable: false, init: IrExpr::Index(&quad_var, &vi) },
        IrStmt::Let { name: "px", ty: None, mutable: false, init: px_init },
        IrStmt::Return(Some(vs_ret)),
    ];

    // Fragment shader: fragment-level rounded clip + SDF primitive + premultiplied fill/border AA.
    let v = IrExpr::Var("v");
    let clips = IrExpr::Var("clips");
    let v_px = IrExpr::Field(&v, "px");
    let v_rect = IrExpr::Field(&v, "rect");
    let v_col = IrExpr::Field(&v, "color");
    let v_bcol = IrExpr::Field(&v, "border_color");
    let v_params = IrExpr::Field(&v, "params");
    let v_clip_id = IrExpr::Field(&v, "clip_id");

    let clip_var = IrExpr::Var("clip");
    let clip_rect = IrExpr::Field(&clip_var, "rect");
    let clip_params = IrExpr::Field(&clip_var, "params");
    let half = IrExpr::LitF32(0.5);
    let clip_center = IrExpr::Binary(
        IrBinOp::Mul,
        &IrExpr::Binary(IrBinOp::Add, &IrExpr::Field(&clip_rect, "xy"), &IrExpr::Field(&clip_rect, "zw")),
        &half,
    );
    let clip_he = IrExpr::Binary(
        IrBinOp::Mul,
        &IrExpr::Binary(IrBinOp::Sub, &IrExpr::Field(&clip_rect, "zw"), &IrExpr::Field(&clip_rect, "xy")),
        &half,
    );
    let clip_d = IrExpr::Call("sd_round_box", &[v_px, clip_center, clip_he, IrExpr::Field(&clip_params, "x")]);
    let clip_a_init = IrExpr::Call(
        "clamp",
        &[IrExpr::Binary(IrBinOp::Sub, &half, &clip_d), IrExpr::LitF32(0.0), IrExpr::LitF32(1.0)],
    );
    let clip_a_var = IrExpr::Var("clip_a");
    let clip_discard = [IrStmt::Discard];

    let kind_f = IrExpr::Field(&v_params, "w");
    let out_col_var = IrExpr::Var("out_col");
    let center_init = IrExpr::Binary(
        IrBinOp::Mul,
        &IrExpr::Binary(IrBinOp::Add, &IrExpr::Field(&v_rect, "xy"), &IrExpr::Field(&v_rect, "zw")),
        &half,
    );
    let he_init = IrExpr::Binary(
        IrBinOp::Mul,
        &IrExpr::Binary(IrBinOp::Sub, &IrExpr::Field(&v_rect, "zw"), &IrExpr::Field(&v_rect, "xy")),
        &half,
    );
    let center_var = IrExpr::Var("center");
    let he_var = IrExpr::Var("he");

    // Branch 0: ROUND_RECT with optional integrated AA border
    let d_var = IrExpr::Var("d");
    let outer_a_var = IrExpr::Var("outer_a");
    let bw_var = IrExpr::Var("bw");
    let inner_m_var = IrExpr::Var("inner_m");
    let border_t_var = IrExpr::Var("border_t");
    let fill_c_var = IrExpr::Var("fill_c");
    let bord_c_var = IrExpr::Var("bord_c");
    let premul_var = IrExpr::Var("premul");

    let fill_rgb = IrExpr::Field(&v_col, "rgb");
    let fill_a = IrExpr::Field(&v_col, "a");
    let bord_rgb = IrExpr::Field(&v_bcol, "rgb");
    let bord_a = IrExpr::Field(&v_bcol, "a");

    let has_border_then = [
        IrStmt::Let {
            name: "inner_m",
            ty: None,
            mutable: false,
            init: IrExpr::Call(
                "clamp",
                &[
                    IrExpr::Binary(IrBinOp::Sub, &half, &IrExpr::Binary(IrBinOp::Add, &d_var, &bw_var)),
                    IrExpr::LitF32(0.0),
                    IrExpr::LitF32(1.0),
                ],
            ),
        },
        IrStmt::Let {
            name: "border_t",
            ty: None,
            mutable: false,
            init: IrExpr::Binary(IrBinOp::Sub, &IrExpr::LitF32(1.0), &inner_m_var),
        },
        IrStmt::Let {
            name: "fill_c",
            ty: None,
            mutable: false,
            init: IrExpr::Binary(
                IrBinOp::Mul,
                &IrExpr::Construct(IrTy::Vec4, &[IrExpr::Binary(IrBinOp::Mul, &fill_rgb, &fill_a), fill_a]),
                &inner_m_var,
            ),
        },
        IrStmt::Let {
            name: "bord_c",
            ty: None,
            mutable: false,
            init: IrExpr::Binary(
                IrBinOp::Mul,
                &IrExpr::Construct(IrTy::Vec4, &[IrExpr::Binary(IrBinOp::Mul, &bord_rgb, &bord_a), bord_a]),
                &border_t_var,
            ),
        },
        IrStmt::Let {
            name: "premul",
            ty: None,
            mutable: false,
            init: IrExpr::Binary(IrBinOp::Mul, &IrExpr::Binary(IrBinOp::Add, &fill_c_var, &bord_c_var), &outer_a_var),
        },
        IrStmt::Assign {
            target: out_col_var,
            value: IrExpr::Construct(
                IrTy::Vec4,
                &[
                    IrExpr::Binary(
                        IrBinOp::Div,
                        &IrExpr::Field(&premul_var, "rgb"),
                        &IrExpr::Call("max", &[IrExpr::Field(&premul_var, "a"), IrExpr::LitF32(0.0001)]),
                    ),
                    IrExpr::Field(&premul_var, "a"),
                ],
            ),
        },
    ];
    let no_border_else = [IrStmt::Assign {
        target: out_col_var,
        value: IrExpr::Construct(IrTy::Vec4, &[fill_rgb, IrExpr::Binary(IrBinOp::Mul, &fill_a, &outer_a_var)]),
    }];
    let rr_then = [
        IrStmt::Let {
            name: "d",
            ty: None,
            mutable: false,
            init: IrExpr::Call("sd_round_box", &[v_px, center_var, he_var, IrExpr::Field(&v_params, "x")]),
        },
        IrStmt::Let {
            name: "outer_a",
            ty: None,
            mutable: false,
            init: IrExpr::Call(
                "clamp",
                &[IrExpr::Binary(IrBinOp::Sub, &half, &d_var), IrExpr::LitF32(0.0), IrExpr::LitF32(1.0)],
            ),
        },
        IrStmt::Let { name: "bw", ty: None, mutable: false, init: IrExpr::Field(&v_params, "y") },
        IrStmt::If {
            cond: IrExpr::Binary(IrBinOp::Gt, &bw_var, &IrExpr::LitF32(0.001)),
            then_body: &has_border_then,
            else_body: &no_border_else,
        },
    ];

    // Branch 1: AA LINE
    let line_fs_then = [
        IrStmt::Let {
            name: "d",
            ty: None,
            mutable: false,
            init: IrExpr::Binary(
                IrBinOp::Sub,
                &IrExpr::Call("sd_segment", &[v_px, IrExpr::Field(&v_rect, "xy"), IrExpr::Field(&v_rect, "zw")]),
                &IrExpr::Binary(IrBinOp::Mul, &IrExpr::Field(&v_params, "x"), &half),
            ),
        },
        IrStmt::Let {
            name: "outer_a",
            ty: None,
            mutable: false,
            init: IrExpr::Call(
                "clamp",
                &[IrExpr::Binary(IrBinOp::Sub, &half, &d_var), IrExpr::LitF32(0.0), IrExpr::LitF32(1.0)],
            ),
        },
        IrStmt::Assign {
            target: out_col_var,
            value: IrExpr::Construct(IrTy::Vec4, &[fill_rgb, IrExpr::Binary(IrBinOp::Mul, &fill_a, &outer_a_var)]),
        },
    ];

    // Branch 2: AA CIRCLE or RING
    let rw_var = IrExpr::Var("rw");
    let ring_then = [IrStmt::Assign {
        target: d_var,
        value: IrExpr::Binary(
            IrBinOp::Sub,
            &IrExpr::Call("abs", &[IrExpr::Binary(IrBinOp::Add, &d_var, &IrExpr::Binary(IrBinOp::Mul, &rw_var, &half))]),
            &IrExpr::Binary(IrBinOp::Mul, &rw_var, &half),
        ),
    }];
    let circle_fs_then = [
        IrStmt::Let {
            name: "d",
            ty: Some(IrTy::F32),
            mutable: true,
            init: IrExpr::Binary(
                IrBinOp::Sub,
                &IrExpr::Call("length", &[IrExpr::Binary(IrBinOp::Sub, &v_px, &center_var)]),
                &IrExpr::Field(&v_params, "x"),
            ),
        },
        IrStmt::Let { name: "rw", ty: None, mutable: false, init: IrExpr::Field(&v_params, "y") },
        IrStmt::If {
            cond: IrExpr::Binary(IrBinOp::Gt, &rw_var, &IrExpr::LitF32(0.001)),
            then_body: &ring_then,
            else_body: &[],
        },
        IrStmt::Let {
            name: "outer_a",
            ty: None,
            mutable: false,
            init: IrExpr::Call(
                "clamp",
                &[IrExpr::Binary(IrBinOp::Sub, &half, &d_var), IrExpr::LitF32(0.0), IrExpr::LitF32(1.0)],
            ),
        },
        IrStmt::Assign {
            target: out_col_var,
            value: IrExpr::Construct(IrTy::Vec4, &[fill_rgb, IrExpr::Binary(IrBinOp::Mul, &fill_a, &outer_a_var)]),
        },
    ];

    // Branch 3: SOFT DROP SHADOW
    let blur_var = IrExpr::Var("blur");
    let sh_a_init = IrExpr::Binary(
        IrBinOp::Sub,
        &IrExpr::LitF32(1.0),
        &IrExpr::Call("smoothstep", &[IrExpr::Unary(IrUnaryOp::Neg, &blur_var), blur_var, d_var]),
    );
    let shadow_fs_else = [
        IrStmt::Let {
            name: "d",
            ty: None,
            mutable: false,
            init: IrExpr::Call("sd_round_box", &[v_px, center_var, he_var, IrExpr::Field(&v_params, "x")]),
        },
        IrStmt::Let {
            name: "blur",
            ty: None,
            mutable: false,
            init: IrExpr::Call("max", &[IrExpr::Field(&v_params, "z"), IrExpr::LitF32(1.0)]),
        },
        IrStmt::Let { name: "outer_a", ty: None, mutable: false, init: sh_a_init },
        IrStmt::Assign {
            target: out_col_var,
            value: IrExpr::Construct(IrTy::Vec4, &[fill_rgb, IrExpr::Binary(IrBinOp::Mul, &fill_a, &outer_a_var)]),
        },
    ];

    let kind2_if = [IrStmt::If {
        cond: IrExpr::Binary(IrBinOp::Lt, &kind_f, &IrExpr::LitF32(2.5)),
        then_body: &circle_fs_then,
        else_body: &shadow_fs_else,
    }];
    let kind1_if = [IrStmt::If {
        cond: IrExpr::Binary(IrBinOp::Lt, &kind_f, &IrExpr::LitF32(1.5)),
        then_body: &line_fs_then,
        else_body: &kind2_if,
    }];

    let final_a_init = IrExpr::Binary(IrBinOp::Mul, &IrExpr::Field(&out_col_var, "a"), &clip_a_var);
    let final_a_var = IrExpr::Var("final_a");
    let fs_body = [
        IrStmt::Let { name: "clip", ty: None, mutable: false, init: IrExpr::Index(&clips, &v_clip_id) },
        IrStmt::Let { name: "clip_a", ty: None, mutable: false, init: clip_a_init },
        IrStmt::If {
            cond: IrExpr::Binary(IrBinOp::Le, &clip_a_var, &IrExpr::LitF32(0.002)),
            then_body: &clip_discard,
            else_body: &[],
        },
        IrStmt::Let { name: "center", ty: None, mutable: false, init: center_init },
        IrStmt::Let { name: "he", ty: None, mutable: false, init: he_init },
        IrStmt::Let {
            name: "out_col",
            ty: Some(IrTy::Vec4),
            mutable: true,
            init: IrExpr::Construct(
                IrTy::Vec4,
                &[IrExpr::LitF32(0.0), IrExpr::LitF32(0.0), IrExpr::LitF32(0.0), IrExpr::LitF32(0.0)],
            ),
        },
        IrStmt::If {
            cond: IrExpr::Binary(IrBinOp::Lt, &kind_f, &IrExpr::LitF32(0.5)),
            then_body: &rr_then,
            else_body: &kind1_if,
        },
        IrStmt::Let { name: "final_a", ty: None, mutable: false, init: final_a_init },
        IrStmt::If {
            cond: IrExpr::Binary(IrBinOp::Le, &final_a_var, &IrExpr::LitF32(0.002)),
            then_body: &clip_discard,
            else_body: &[],
        },
        IrStmt::Return(Some(IrExpr::Construct(
            IrTy::Vec4,
            &[IrExpr::Field(&out_col_var, "rgb"), final_a_var],
        ))),
    ];

    let entries = [
        IrEntry {
            desc: &ui_sdf_vs::ENTRY,
            param_types: &[IrTy::Void, IrTy::Void, IrTy::Struct("Viewport"), IrTy::Struct("RectInst")],
            body: &vs_body,
        },
        IrEntry {
            desc: &ui_sdf_fs::ENTRY,
            param_types: &[IrTy::Void, IrTy::Struct("ClipRect")],
            body: &fs_body,
        },
    ];
    let module = ShaderModule { structs: &SDF_STRUCTS, helpers: &helpers, entries: &entries };
    match lang {
        TargetLang::Wgsl => emit_wgsl_pipeline(&module, out),
        TargetLang::Msl => emit_msl(&module, out),
    }
}

fn emit_glyph_shader(lang: TargetLang, rgba_texture: bool, out: &mut dyn fmt::Write) -> fmt::Result {
    // Helper: sd_round_box for fragment rounded-rect glyph clipping
    let p_var = IrExpr::Var("p");
    let c_var = IrExpr::Var("center");
    let he_var = IrExpr::Var("half_ext");
    let r_var = IrExpr::Var("r");
    let rc_init = IrExpr::Call(
        "min",
        &[r_var, IrExpr::Call("min", &[IrExpr::Field(&he_var, "x"), IrExpr::Field(&he_var, "y")])],
    );
    let rc_var = IrExpr::Var("rc");
    let rc_vec = IrExpr::Construct(IrTy::Vec2, &[rc_var, rc_var]);
    let q_init = IrExpr::Binary(
        IrBinOp::Add,
        &IrExpr::Binary(
            IrBinOp::Sub,
            &IrExpr::Call("abs", &[IrExpr::Binary(IrBinOp::Sub, &p_var, &c_var)]),
            &he_var,
        ),
        &rc_vec,
    );
    let q_var = IrExpr::Var("q");
    let zero2 = IrExpr::Construct(IrTy::Vec2, &[IrExpr::LitF32(0.0), IrExpr::LitF32(0.0)]);
    let sd_box_ret = IrExpr::Binary(
        IrBinOp::Sub,
        &IrExpr::Binary(
            IrBinOp::Add,
            &IrExpr::Call("length", &[IrExpr::Call("max", &[q_var, zero2])]),
            &IrExpr::Call(
                "min",
                &[
                    IrExpr::Call("max", &[IrExpr::Field(&q_var, "x"), IrExpr::Field(&q_var, "y")]),
                    IrExpr::LitF32(0.0),
                ],
            ),
        ),
        &rc_var,
    );
    let sd_box_body = [
        IrStmt::Let { name: "rc", ty: None, mutable: false, init: rc_init },
        IrStmt::Let { name: "q", ty: None, mutable: false, init: q_init },
        IrStmt::Return(Some(sd_box_ret)),
    ];
    let helpers = [IrFn {
        name: "sd_round_box",
        params: &SD_ROUND_BOX_PARAMS,
        ret: IrTy::F32,
        body: &sd_box_body,
    }];

    // Vertex shader
    let vi = IrExpr::Var("vi");
    let ii = IrExpr::Var("ii");
    let vp = IrExpr::Var("vp");
    let glyphs = IrExpr::Var("glyphs");
    let g = IrExpr::Var("g");
    let g_rect = IrExpr::Field(&g, "rect");
    let g_uv = IrExpr::Field(&g, "uv");
    let g_col = IrExpr::Field(&g, "color");
    let g_meta = IrExpr::Field(&g, "clip_info");

    let quad_arr = IrExpr::Construct(
        IrTy::Array(&IrTy::Vec2, 6),
        &[
            IrExpr::Construct(IrTy::Vec2, &[IrExpr::LitF32(0.0), IrExpr::LitF32(0.0)]),
            IrExpr::Construct(IrTy::Vec2, &[IrExpr::LitF32(1.0), IrExpr::LitF32(0.0)]),
            IrExpr::Construct(IrTy::Vec2, &[IrExpr::LitF32(1.0), IrExpr::LitF32(1.0)]),
            IrExpr::Construct(IrTy::Vec2, &[IrExpr::LitF32(0.0), IrExpr::LitF32(0.0)]),
            IrExpr::Construct(IrTy::Vec2, &[IrExpr::LitF32(1.0), IrExpr::LitF32(1.0)]),
            IrExpr::Construct(IrTy::Vec2, &[IrExpr::LitF32(0.0), IrExpr::LitF32(1.0)]),
        ],
    );
    let quad_var = IrExpr::Var("quad");
    let k_var = IrExpr::Var("k");
    let px_init = IrExpr::Call("lerp", &[IrExpr::Field(&g_rect, "xy"), IrExpr::Field(&g_rect, "zw"), k_var]);
    let px_var = IrExpr::Var("px");
    let uv_out = IrExpr::Call("lerp", &[IrExpr::Field(&g_uv, "xy"), IrExpr::Field(&g_uv, "zw"), k_var]);
    let vp_size = IrExpr::Field(&vp, "size");
    let ndc_x = IrExpr::Binary(
        IrBinOp::Sub,
        &IrExpr::Binary(
            IrBinOp::Mul,
            &IrExpr::Binary(IrBinOp::Div, &IrExpr::Field(&px_var, "x"), &IrExpr::Field(&vp_size, "x")),
            &IrExpr::LitF32(2.0),
        ),
        &IrExpr::LitF32(1.0),
    );
    let ndc_y = IrExpr::Binary(
        IrBinOp::Sub,
        &IrExpr::LitF32(1.0),
        &IrExpr::Binary(
            IrBinOp::Mul,
            &IrExpr::Binary(IrBinOp::Div, &IrExpr::Field(&px_var, "y"), &IrExpr::Field(&vp_size, "y")),
            &IrExpr::LitF32(2.0),
        ),
    );
    let pos_out = IrExpr::Construct(IrTy::Vec4, &[ndc_x, ndc_y, IrExpr::LitF32(0.0), IrExpr::LitF32(1.0)]);
    let vs_ret = IrExpr::Construct(
        IrTy::Struct("ui_glyph_vs_out"),
        &[pos_out, px_var, uv_out, g_col, IrExpr::Field(&g_meta, "x")],
    );
    let vs_body = [
        IrStmt::Let { name: "g", ty: None, mutable: false, init: IrExpr::Index(&glyphs, &ii) },
        IrStmt::Let { name: "quad", ty: None, mutable: true, init: quad_arr },
        IrStmt::Let { name: "k", ty: None, mutable: false, init: IrExpr::Index(&quad_var, &vi) },
        IrStmt::Let { name: "px", ty: None, mutable: false, init: px_init },
        IrStmt::Return(Some(vs_ret)),
    ];

    // Fragment shader: sample glyph atlas alpha (outside non-uniform control flow!) + rounded clip.
    let v = IrExpr::Var("v");
    let clips = IrExpr::Var("clips");
    let v_px = IrExpr::Field(&v, "px");
    let v_uv = IrExpr::Field(&v, "uv");
    let v_col = IrExpr::Field(&v, "color");
    let v_clip_id = IrExpr::Field(&v, "clip_id");

    // IMPORTANT for WebGPU WGSL uniformity analysis: `textureSample` MUST run in uniform control
    // flow BEFORE any `if (...) { discard; }` branch!
    let tex_sample = IrExpr::Sample2d { tex: "atlas_tex", smp: "atlas_samp", uv: &v_uv, level: None };
    let tex_var = IrExpr::Var("tex_s");
    let tex_a_expr = IrExpr::Field(&tex_var, "a");

    let clip_var = IrExpr::Var("clip");
    let clip_rect = IrExpr::Field(&clip_var, "rect");
    let clip_params = IrExpr::Field(&clip_var, "params");
    let half = IrExpr::LitF32(0.5);
    let clip_center = IrExpr::Binary(
        IrBinOp::Mul,
        &IrExpr::Binary(IrBinOp::Add, &IrExpr::Field(&clip_rect, "xy"), &IrExpr::Field(&clip_rect, "zw")),
        &half,
    );
    let clip_he = IrExpr::Binary(
        IrBinOp::Mul,
        &IrExpr::Binary(IrBinOp::Sub, &IrExpr::Field(&clip_rect, "zw"), &IrExpr::Field(&clip_rect, "xy")),
        &half,
    );
    let clip_d = IrExpr::Call("sd_round_box", &[v_px, clip_center, clip_he, IrExpr::Field(&clip_params, "x")]);
    let clip_a_init = IrExpr::Call(
        "clamp",
        &[IrExpr::Binary(IrBinOp::Sub, &half, &clip_d), IrExpr::LitF32(0.0), IrExpr::LitF32(1.0)],
    );
    let clip_a_var = IrExpr::Var("clip_a");
    let final_a_init = IrExpr::Binary(
        IrBinOp::Mul,
        &IrExpr::Binary(IrBinOp::Mul, &IrExpr::Field(&v_col, "a"), &tex_a_expr),
        &clip_a_var,
    );
    let final_a_var = IrExpr::Var("final_a");
    let discard_stmt = [IrStmt::Discard];
    let out_rgb_rgba = IrExpr::Binary(
        IrBinOp::Mul,
        &IrExpr::Field(&tex_var, "rgb"),
        &IrExpr::Field(&v_col, "rgb"),
    );
    let out_rgb = if rgba_texture { out_rgb_rgba } else { IrExpr::Field(&v_col, "rgb") };

    let fs_body = [
        IrStmt::Let { name: "tex_s", ty: None, mutable: false, init: tex_sample },
        IrStmt::Let { name: "clip", ty: None, mutable: false, init: IrExpr::Index(&clips, &v_clip_id) },
        IrStmt::Let { name: "clip_a", ty: None, mutable: false, init: clip_a_init },
        IrStmt::Let { name: "final_a", ty: None, mutable: false, init: final_a_init },
        IrStmt::If {
            cond: IrExpr::Binary(IrBinOp::Le, &final_a_var, &IrExpr::LitF32(0.008)),
            then_body: &discard_stmt,
            else_body: &[],
        },
        IrStmt::Return(Some(IrExpr::Construct(IrTy::Vec4, &[out_rgb, final_a_var]))),
    ];

    let entries = [
        IrEntry {
            desc: &ui_glyph_vs::ENTRY,
            param_types: &[IrTy::Void, IrTy::Void, IrTy::Struct("Viewport"), IrTy::Struct("GlyphInst")],
            body: &vs_body,
        },
        IrEntry {
            desc: &ui_glyph_fs::ENTRY,
            param_types: &[IrTy::Void, IrTy::Struct("ClipRect"), IrTy::Void, IrTy::Void],
            body: &fs_body,
        },
    ];
    let module = ShaderModule { structs: &GLYPH_STRUCTS, helpers: &helpers, entries: &entries };
    match lang {
        TargetLang::Wgsl => emit_wgsl_pipeline(&module, out),
        TargetLang::Msl => emit_msl(&module, out),
    }
}

/// Emit the linked WebGPU SDF UI shader (`ui_sdf_vs` + `ui_sdf_fs`) as WGSL.
///
/// Bindings in `@group(0)`: `0 = Viewport`, `1 = insts: array<RectInst>`, `2 = clips: array<ClipRect>`.
pub fn emit_ui_sdf_wgsl(out: &mut dyn fmt::Write) -> fmt::Result {
    emit_sdf_shader(TargetLang::Wgsl, out)
}

/// Emit the SDF UI shader (`ui_sdf_vs` + `ui_sdf_fs`) as Metal Shading Language 2.4.
pub fn emit_ui_sdf_msl(out: &mut dyn fmt::Write) -> fmt::Result {
    emit_sdf_shader(TargetLang::Msl, out)
}

/// Emit the linked WebGPU Glyph Atlas UI shader (`ui_glyph_vs` + `ui_glyph_fs`) as WGSL.
///
/// Bindings in `@group(0)`: `0 = Viewport`, `1 = glyphs: array<GlyphInst>`, `2 = clips: array<ClipRect>`,
/// `3 = atlas_samp`, `4 = atlas_tex`.
pub fn emit_ui_glyph_wgsl(out: &mut dyn fmt::Write) -> fmt::Result {
    emit_glyph_shader(TargetLang::Wgsl, false, out)
}

/// Emit the Glyph Atlas UI shader (`ui_glyph_vs` + `ui_glyph_fs`) as Metal Shading Language 2.4.
pub fn emit_ui_glyph_msl(out: &mut dyn fmt::Write) -> fmt::Result {
    emit_glyph_shader(TargetLang::Msl, false, out)
}

/// Emit the RGBA Sprite/Icon Atlas UI shader (`ui_glyph_vs` + `ui_glyph_fs`) as MSL 2.4.
/// Samples full RGBA texels modulated by `GlyphInst::color` with rounded-rect clipping.
pub fn emit_ui_sprite_msl(out: &mut dyn fmt::Write) -> fmt::Result {
    emit_glyph_shader(TargetLang::Msl, true, out)
}
