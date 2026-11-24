//! Rust-authored WebGPU shaders emitted to WGSL via Blaze's own [`gpu::emit::emit_wgsl`] pipeline
//! (zero handwritten WGSL strings in JavaScript).
//!
//! Each shader pair (`edge_vs`/`edge_fs` and `node_vs`/`node_fs`) declares its real
//! `#[gpu::varyings]`, `#[gpu::vertex]`, and `#[gpu::fragment]` entry signatures in Rust and
//! emits valid WGSL (`@group(0) @binding(0)` uniform `Cam` + `@group(0) @binding(1)` storage
//! instance array) straight into WASM linear memory for WebGPU.

use core::fmt;

use gpu::emit::{
    IrBinOp, IrEntry, IrExpr, IrField, IrStmt, IrStruct, IrTy, IrUnaryOp, ShaderModule, emit_wgsl,
};
use gpu::prelude::{InstanceIndex, Position, Vec2, Vec4, VertexIndex, vec2, vec4};

/// 32-byte GPU Pod carrier (`[Vec4; 2]`) matching `Cam`, `EdgeInst`, and `NodeInst` layouts
/// without introducing any `unsafe` into `#![forbid(unsafe_code)]` `blaze-ecs`.
pub type Pod32 = [Vec4; 2];

#[derive(Clone, Copy, gpu::Varyings)]
pub struct EdgeVaryings {
    pub pos: Position,
    pub col: Vec4,
    pub side: f32,
}

#[derive(Clone, Copy, gpu::Varyings)]
pub struct NodeVaryings {
    pub pos: Position,
    pub uv: Vec2,
    pub col: Vec4,
    pub flags: f32,
}

#[gpu::vertex]
pub fn edge_vs(vi: VertexIndex, ii: InstanceIndex, cam: &Pod32, edges: &[Pod32]) -> EdgeVaryings {
    let _ = (vi, ii, cam, edges);
    EdgeVaryings { pos: Position(vec4(0.0, 0.0, 0.0, 1.0)), col: Vec4::splat(1.0), side: 0.0 }
}

#[gpu::fragment]
pub fn edge_fs(v: EdgeVaryings) -> Vec4 {
    v.col
}

#[gpu::vertex]
pub fn node_vs(vi: VertexIndex, ii: InstanceIndex, cam: &Pod32, nodes: &[Pod32]) -> NodeVaryings {
    let _ = (vi, ii, cam, nodes);
    NodeVaryings {
        pos: Position(vec4(0.0, 0.0, 0.0, 1.0)),
        uv: vec2(0.0, 0.0),
        col: Vec4::splat(1.0),
        flags: 0.0,
    }
}

#[gpu::fragment]
pub fn node_fs(v: NodeVaryings) -> Vec4 {
    v.col
}

const CAM_FIELDS: [IrField; 4] = [
    IrField { name: "center", ty: IrTy::Vec2 },
    IrField { name: "scale", ty: IrTy::Vec2 },
    IrField { name: "viewport", ty: IrTy::Vec2 },
    IrField { name: "pad", ty: IrTy::Vec2 },
];

const EDGE_INST_FIELDS: [IrField; 3] = [
    IrField { name: "p0", ty: IrTy::Vec2 },
    IrField { name: "p1", ty: IrTy::Vec2 },
    IrField { name: "col", ty: IrTy::Vec4 },
];

// Matches our packed 8-float node buffer: `[x, y, radius, flags, r, g, b, a]` (aligned 16B for vec4 col!).
const NODE_INST_FIELDS: [IrField; 4] = [
    IrField { name: "xy", ty: IrTy::Vec2 },
    IrField { name: "radius", ty: IrTy::F32 },
    IrField { name: "flags", ty: IrTy::F32 },
    IrField { name: "col", ty: IrTy::Vec4 },
];

const EDGE_STRUCTS: [IrStruct; 2] = [
    IrStruct { name: "Cam", fields: &CAM_FIELDS },
    IrStruct { name: "EdgeInst", fields: &EDGE_INST_FIELDS },
];

const NODE_STRUCTS: [IrStruct; 2] = [
    IrStruct { name: "Cam", fields: &CAM_FIELDS },
    IrStruct { name: "NodeInst", fields: &NODE_INST_FIELDS },
];

/// Emit the instanced anti-aliased edge quad shader as WGSL using [`gpu::emit::emit_wgsl`].
pub fn emit_edge_wgsl(out: &mut dyn fmt::Write) -> fmt::Result {
    let cam = IrExpr::Var("cam");
    let edges = IrExpr::Var("edges");
    let ii = IrExpr::Var("ii");
    let vi = IrExpr::Var("vi");

    let q_arr = IrExpr::Construct(
        IrTy::Array(&IrTy::Vec2, 6),
        &[
            IrExpr::Construct(IrTy::Vec2, &[IrExpr::LitF32(0.0), IrExpr::LitF32(-1.0)]),
            IrExpr::Construct(IrTy::Vec2, &[IrExpr::LitF32(1.0), IrExpr::LitF32(-1.0)]),
            IrExpr::Construct(IrTy::Vec2, &[IrExpr::LitF32(1.0), IrExpr::LitF32(1.0)]),
            IrExpr::Construct(IrTy::Vec2, &[IrExpr::LitF32(0.0), IrExpr::LitF32(-1.0)]),
            IrExpr::Construct(IrTy::Vec2, &[IrExpr::LitF32(1.0), IrExpr::LitF32(1.0)]),
            IrExpr::Construct(IrTy::Vec2, &[IrExpr::LitF32(0.0), IrExpr::LitF32(1.0)]),
        ],
    );
    let quad_var = IrExpr::Var("quad");
    let q_var = IrExpr::Var("q");
    let e_var = IrExpr::Var("e");
    let cam_center = IrExpr::Field(&cam, "center");
    let cam_scale = IrExpr::Field(&cam, "scale");
    let cam_vp = IrExpr::Field(&cam, "viewport");
    let half = IrExpr::LitF32(0.5);
    let half_vp = IrExpr::Binary(IrBinOp::Mul, &cam_vp, &half);
    let scale_half_vp = IrExpr::Binary(IrBinOp::Mul, &cam_scale, &half_vp);

    let e_p0 = IrExpr::Field(&e_var, "p0");
    let e_p1 = IrExpr::Field(&e_var, "p1");
    let e_col = IrExpr::Field(&e_var, "col");

    let p0_rel = IrExpr::Binary(IrBinOp::Sub, &e_p0, &cam_center);
    let p1_rel = IrExpr::Binary(IrBinOp::Sub, &e_p1, &cam_center);
    let s0_init = IrExpr::Binary(IrBinOp::Mul, &p0_rel, &scale_half_vp);
    let s1_init = IrExpr::Binary(IrBinOp::Mul, &p1_rel, &scale_half_vp);

    let s0_var = IrExpr::Var("s0");
    let s1_var = IrExpr::Var("s1");
    let d_var = IrExpr::Var("d");
    let d_init = IrExpr::Binary(IrBinOp::Sub, &s1_var, &s0_var);
    let d_len = IrExpr::Call("length", &[d_var]);
    let l_init = IrExpr::Call("max", &[d_len, IrExpr::LitF32(0.001)]);
    let l_var = IrExpr::Var("l");

    let dx = IrExpr::Field(&d_var, "x");
    let dy = IrExpr::Field(&d_var, "y");
    let neg_dy = IrExpr::Unary(IrUnaryOp::Neg, &dy);
    let nx = IrExpr::Binary(IrBinOp::Div, &neg_dy, &l_var);
    let ny = IrExpr::Binary(IrBinOp::Div, &dx, &l_var);
    let n_init = IrExpr::Construct(IrTy::Vec2, &[nx, ny]);
    let n_var = IrExpr::Var("n");

    let qx = IrExpr::Field(&q_var, "x");
    let qy = IrExpr::Field(&q_var, "y");
    let base_pt = IrExpr::Call("lerp", &[s0_var, s1_var, qx]);
    let width = IrExpr::LitF32(1.35);
    let qy_w = IrExpr::Binary(IrBinOp::Mul, &qy, &width);
    let offset = IrExpr::Binary(IrBinOp::Mul, &n_var, &qy_w);
    let sp_init = IrExpr::Binary(IrBinOp::Add, &base_pt, &offset);
    let sp_var = IrExpr::Var("sp");

    let spx = IrExpr::Field(&sp_var, "x");
    let spy = IrExpr::Field(&sp_var, "y");
    let neg_spy = IrExpr::Unary(IrUnaryOp::Neg, &spy);
    let vpx = IrExpr::Field(&cam_vp, "x");
    let vpy = IrExpr::Field(&cam_vp, "y");
    let hvpx = IrExpr::Binary(IrBinOp::Mul, &vpx, &half);
    let hvpy = IrExpr::Binary(IrBinOp::Mul, &vpy, &half);
    let ndc_x = IrExpr::Binary(IrBinOp::Div, &spx, &hvpx);
    let ndc_y = IrExpr::Binary(IrBinOp::Div, &neg_spy, &hvpy);
    let clip_pos = IrExpr::Construct(
        IrTy::Vec4,
        &[ndc_x, ndc_y, IrExpr::LitF32(0.0), IrExpr::LitF32(1.0)],
    );
    let vs_ret = IrExpr::Construct(IrTy::Struct("edge_vs_out"), &[clip_pos, e_col, qy]);

    let vs_body = [
        IrStmt::Let { name: "quad", ty: None, mutable: true, init: q_arr },
        IrStmt::Let { name: "q", ty: None, mutable: false, init: IrExpr::Index(&quad_var, &vi) },
        IrStmt::Let { name: "e", ty: None, mutable: false, init: IrExpr::Index(&edges, &ii) },
        IrStmt::Let { name: "s0", ty: None, mutable: false, init: s0_init },
        IrStmt::Let { name: "s1", ty: None, mutable: false, init: s1_init },
        IrStmt::Let { name: "d", ty: None, mutable: false, init: d_init },
        IrStmt::Let { name: "l", ty: None, mutable: false, init: l_init },
        IrStmt::Let { name: "n", ty: None, mutable: false, init: n_init },
        IrStmt::Let { name: "sp", ty: None, mutable: false, init: sp_init },
        IrStmt::Return(Some(vs_ret)),
    ];

    // Fragment shader: crisp anti-aliased edge falloff
    let v_var = IrExpr::Var("v");
    let v_side = IrExpr::Field(&v_var, "side");
    let abs_side = IrExpr::Call("abs", &[v_side]);
    let ss = IrExpr::Call("smoothstep", &[IrExpr::LitF32(0.45), IrExpr::LitF32(1.0), abs_side]);
    let one = IrExpr::LitF32(1.0);
    let aa_init = IrExpr::Binary(IrBinOp::Sub, &one, &ss);
    let aa_var = IrExpr::Var("aa");
    let v_col = IrExpr::Field(&v_var, "col");
    let v_rgb = IrExpr::Field(&v_col, "rgb");
    let v_a = IrExpr::Field(&v_col, "a");
    let out_a = IrExpr::Binary(IrBinOp::Mul, &v_a, &aa_var);
    let fs_ret = IrExpr::Construct(IrTy::Vec4, &[v_rgb, out_a]);
    let fs_body = [
        IrStmt::Let { name: "aa", ty: None, mutable: false, init: aa_init },
        IrStmt::Return(Some(fs_ret)),
    ];

    let entries = [
        IrEntry {
            desc: &edge_vs::ENTRY,
            param_types: &[
                IrTy::Void,
                IrTy::Void,
                IrTy::Struct("Cam"),
                IrTy::Struct("EdgeInst"),
            ],
            body: &vs_body,
        },
        IrEntry {
            desc: &edge_fs::ENTRY,
            param_types: &[IrTy::Void],
            body: &fs_body,
        },
    ];
    let module = ShaderModule { structs: &EDGE_STRUCTS, helpers: &[], entries: &entries };
    emit_wgsl(&module, out)
}

/// Emit the instanced SDF circle + glow ring node shader as WGSL using [`gpu::emit::emit_wgsl`].
pub fn emit_node_wgsl(out: &mut dyn fmt::Write) -> fmt::Result {
    let cam = IrExpr::Var("cam");
    let nodes = IrExpr::Var("nodes");
    let ii = IrExpr::Var("ii");
    let vi = IrExpr::Var("vi");

    let q_arr = IrExpr::Construct(
        IrTy::Array(&IrTy::Vec2, 6),
        &[
            IrExpr::Construct(IrTy::Vec2, &[IrExpr::LitF32(-1.0), IrExpr::LitF32(-1.0)]),
            IrExpr::Construct(IrTy::Vec2, &[IrExpr::LitF32(1.0), IrExpr::LitF32(-1.0)]),
            IrExpr::Construct(IrTy::Vec2, &[IrExpr::LitF32(1.0), IrExpr::LitF32(1.0)]),
            IrExpr::Construct(IrTy::Vec2, &[IrExpr::LitF32(-1.0), IrExpr::LitF32(-1.0)]),
            IrExpr::Construct(IrTy::Vec2, &[IrExpr::LitF32(1.0), IrExpr::LitF32(1.0)]),
            IrExpr::Construct(IrTy::Vec2, &[IrExpr::LitF32(-1.0), IrExpr::LitF32(1.0)]),
        ],
    );
    let quad_var = IrExpr::Var("quad");
    let q_var = IrExpr::Var("q");
    let n_var = IrExpr::Var("node");

    let n_xy = IrExpr::Field(&n_var, "xy");
    let n_r = IrExpr::Field(&n_var, "radius");
    let n_flags = IrExpr::Field(&n_var, "flags");
    let n_col = IrExpr::Field(&n_var, "col");

    let f_gt = IrExpr::Binary(IrBinOp::Gt, &n_flags, &IrExpr::LitF32(0.5));
    let f_lt = IrExpr::Binary(IrBinOp::Lt, &n_flags, &IrExpr::LitF32(2.5));
    let is_lit = IrExpr::Binary(IrBinOp::And, &f_gt, &f_lt);
    let pad_init = IrExpr::Call("select", &[IrExpr::LitF32(1.45), IrExpr::LitF32(2.35), is_lit]);
    let pad_var = IrExpr::Var("pad");
    let pr_init = IrExpr::Binary(IrBinOp::Mul, &n_r, &pad_var);
    let pr_var = IrExpr::Var("pr");

    let cam_center = IrExpr::Field(&cam, "center");
    let cam_scale = IrExpr::Field(&cam, "scale");
    let cam_vp = IrExpr::Field(&cam, "viewport");
    let half = IrExpr::LitF32(0.5);
    let half_vp = IrExpr::Binary(IrBinOp::Mul, &cam_vp, &half);
    let scale_half_vp = IrExpr::Binary(IrBinOp::Mul, &cam_scale, &half_vp);
    let xy_rel = IrExpr::Binary(IrBinOp::Sub, &n_xy, &cam_center);
    let center_px = IrExpr::Binary(IrBinOp::Mul, &xy_rel, &scale_half_vp);
    let q_pr = IrExpr::Binary(IrBinOp::Mul, &q_var, &pr_var);
    let sp_init = IrExpr::Binary(IrBinOp::Add, &center_px, &q_pr);
    let sp_var = IrExpr::Var("sp");

    let spx = IrExpr::Field(&sp_var, "x");
    let spy = IrExpr::Field(&sp_var, "y");
    let neg_spy = IrExpr::Unary(IrUnaryOp::Neg, &spy);
    let vpx = IrExpr::Field(&cam_vp, "x");
    let vpy = IrExpr::Field(&cam_vp, "y");
    let hvpx = IrExpr::Binary(IrBinOp::Mul, &vpx, &half);
    let hvpy = IrExpr::Binary(IrBinOp::Mul, &vpy, &half);
    let ndc_x = IrExpr::Binary(IrBinOp::Div, &spx, &hvpx);
    let ndc_y = IrExpr::Binary(IrBinOp::Div, &neg_spy, &hvpy);
    let clip_pos = IrExpr::Construct(
        IrTy::Vec4,
        &[ndc_x, ndc_y, IrExpr::LitF32(0.0), IrExpr::LitF32(1.0)],
    );
    let uv_out = IrExpr::Binary(IrBinOp::Mul, &q_var, &pad_var);
    let vs_ret = IrExpr::Construct(IrTy::Struct("node_vs_out"), &[clip_pos, uv_out, n_col, n_flags]);

    let vs_body = [
        IrStmt::Let { name: "quad", ty: None, mutable: true, init: q_arr },
        IrStmt::Let { name: "q", ty: None, mutable: false, init: IrExpr::Index(&quad_var, &vi) },
        IrStmt::Let { name: "node", ty: None, mutable: false, init: IrExpr::Index(&nodes, &ii) },
        IrStmt::Let { name: "pad", ty: None, mutable: false, init: pad_init },
        IrStmt::Let { name: "pr", ty: None, mutable: false, init: pr_init },
        IrStmt::Let { name: "sp", ty: None, mutable: false, init: sp_init },
        IrStmt::Return(Some(vs_ret)),
    ];

    // Fragment shader: SDF disc + crisp selection/hover ring + prefab outer ring
    let v_var = IrExpr::Var("v");
    let v_uv = IrExpr::Field(&v_var, "uv");
    let v_col = IrExpr::Field(&v_var, "col");
    let v_flags = IrExpr::Field(&v_var, "flags");
    let d_init = IrExpr::Call("length", &[v_uv]);
    let d_var = IrExpr::Var("d");

    let one = IrExpr::LitF32(1.0);
    let ss_core = IrExpr::Call("smoothstep", &[IrExpr::LitF32(0.86), IrExpr::LitF32(1.02), d_var]);
    let core_init = IrExpr::Binary(IrBinOp::Sub, &one, &ss_core);
    let core_var = IrExpr::Var("core");
    let v_a = IrExpr::Field(&v_col, "a");
    let alpha_init = IrExpr::Binary(IrBinOp::Mul, &core_var, &v_a);
    let alpha_var = IrExpr::Var("alpha");

    // Selected / hovered halo ring + soft ember glow
    let ring1_a = IrExpr::Binary(
        IrBinOp::Sub,
        &one,
        &IrExpr::Call("smoothstep", &[IrExpr::LitF32(1.38), IrExpr::LitF32(1.56), d_var]),
    );
    let ring1_b = IrExpr::Call("smoothstep", &[IrExpr::LitF32(1.12), IrExpr::LitF32(1.28), d_var]);
    let ring1 = IrExpr::Binary(IrBinOp::Mul, &ring1_a, &ring1_b);
    let ring1_a_scaled = IrExpr::Binary(IrBinOp::Mul, &ring1, &IrExpr::LitF32(0.85));

    let d_minus = IrExpr::Binary(IrBinOp::Sub, &d_var, &IrExpr::LitF32(0.9));
    let d_clamped = IrExpr::Call("max", &[d_minus, IrExpr::LitF32(0.0)]);
    let neg_decay = IrExpr::Binary(IrBinOp::Mul, &IrExpr::Unary(IrUnaryOp::Neg, &d_clamped), &IrExpr::LitF32(2.4));
    let exp_val = IrExpr::Call("exp", &[neg_decay]);
    let glow = IrExpr::Binary(IrBinOp::Mul, &exp_val, &IrExpr::LitF32(0.34));
    let halo = IrExpr::Call("max", &[ring1_a_scaled, glow]);
    let lit_then = [IrStmt::Assign {
        target: alpha_var,
        value: IrExpr::Call("max", &[alpha_var, halo]),
    }];

    // Prefab subtle outer ring (flags > 3.5)
    let pring_a = IrExpr::Binary(
        IrBinOp::Sub,
        &one,
        &IrExpr::Call("smoothstep", &[IrExpr::LitF32(1.22), IrExpr::LitF32(1.36), d_var]),
    );
    let pring_b = IrExpr::Call("smoothstep", &[IrExpr::LitF32(1.06), IrExpr::LitF32(1.18), d_var]);
    let pring = IrExpr::Binary(IrBinOp::Mul, &pring_a, &pring_b);
    let pring_scaled = IrExpr::Binary(IrBinOp::Mul, &pring, &IrExpr::LitF32(0.55));
    let prefab_then = [IrStmt::Assign {
        target: alpha_var,
        value: IrExpr::Call("max", &[alpha_var, pring_scaled]),
    }];
    let prefab_cond = IrExpr::Binary(IrBinOp::Gt, &v_flags, &IrExpr::LitF32(3.5));
    let lit_else = [IrStmt::If {
        cond: prefab_cond,
        then_body: &prefab_then,
        else_body: &[],
    }];

    let lit_cond = IrExpr::Binary(
        IrBinOp::And,
        &IrExpr::Binary(IrBinOp::Gt, &v_flags, &IrExpr::LitF32(0.5)),
        &IrExpr::Binary(IrBinOp::Lt, &v_flags, &IrExpr::LitF32(2.5)),
    );
    let discard_cond = IrExpr::Binary(IrBinOp::Lt, &alpha_var, &IrExpr::LitF32(0.005));
    let discard_then = [IrStmt::Discard];
    let v_rgb = IrExpr::Field(&v_col, "rgb");
    let fs_ret = IrExpr::Construct(IrTy::Vec4, &[v_rgb, alpha_var]);

    let fs_body = [
        IrStmt::Let { name: "d", ty: None, mutable: false, init: d_init },
        IrStmt::Let { name: "core", ty: None, mutable: false, init: core_init },
        IrStmt::Let { name: "alpha", ty: Some(IrTy::F32), mutable: true, init: alpha_init },
        IrStmt::If { cond: lit_cond, then_body: &lit_then, else_body: &lit_else },
        IrStmt::If { cond: discard_cond, then_body: &discard_then, else_body: &[] },
        IrStmt::Return(Some(fs_ret)),
    ];

    let entries = [
        IrEntry {
            desc: &node_vs::ENTRY,
            param_types: &[
                IrTy::Void,
                IrTy::Void,
                IrTy::Struct("Cam"),
                IrTy::Struct("NodeInst"),
            ],
            body: &vs_body,
        },
        IrEntry {
            desc: &node_fs::ENTRY,
            param_types: &[IrTy::Void],
            body: &fs_body,
        },
    ];
    let module = ShaderModule { structs: &NODE_STRUCTS, helpers: &[], entries: &entries };
    emit_wgsl(&module, out)
}

#[cfg(test)]
mod tests {
    use gpu::emit::StackWriter;

    use super::{emit_edge_wgsl, emit_node_wgsl};

    #[test]
    fn rust_gpu_shaders_emit_valid_webgpu_wgsl() {
        let mut edge = StackWriter::<8192>::new();
        assert!(emit_edge_wgsl(&mut edge).is_ok());
        let es = edge.as_str();
        assert!(es.contains("@vertex"));
        assert!(es.contains("@fragment"));
        assert!(es.contains("@group(0) @binding(0) var<uniform> cam: Cam;"));
        assert!(es.contains("@group(0) @binding(1) var<storage, read> edges: array<EdgeInst>;"));

        let mut node = StackWriter::<8192>::new();
        assert!(emit_node_wgsl(&mut node).is_ok());
        let ns = node.as_str();
        assert!(ns.contains("@vertex"));
        assert!(ns.contains("@fragment"));
        assert!(ns.contains("@group(0) @binding(0) var<uniform> cam: Cam;"));
        assert!(ns.contains("@group(0) @binding(1) var<storage, read> nodes: array<NodeInst>;"));
        assert!(ns.contains("discard;"));
    }
}
