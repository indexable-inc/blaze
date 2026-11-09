//! What was harness-native-render's shaders/mascot.slang (deleted when Kip's pass switched to this; the switch's parent
//! commit has it), in Rust, function for function.
//!
//! Kip, den's mascot (harness-native-kip): a voxel character drawn off screen into a picture of its
//! own device pixels (4x MSAA, with depth), then set into the frame as a premultiplied quad over a
//! soft contact shadow.
//!
//! Light is flat per face and worked in linear light: a key light, a fill from the sky above and
//! the ground below, a rim from behind, each corner's ambient occlusion from the mesher (Lysenko's
//! voxel AO, harness-native-kip's model.rs), and a faint line between cells that fades out once a
//! cell is only a few pixels across. Inks marked to glow (the antennae's tips) add their own light
//! while the character thinks. The picture is written as extended sRGB like every other pass.
use gpu::prelude::*;

pub const MAX_PARTS: i32 = 24;
pub const MAX_INKS: i32 = 16;

#[repr(C)]
#[derive(Copy, Clone, Debug, gpu::Pod)]
pub struct MascotFrame {
    /// Each part's transform from its cells to the picture's clip space (x, y, depth), three rows.
    pub parts: [Vec4; 72],
    /// Each part's transform of a normal into view space (the inverse transpose), three rows.
    pub normals: [Vec4; 72],
    /// Linear rgb; w how much it glows.
    pub inks: [Vec4; 16],
    /// xyz the direction toward the key light (view space), w unused.
    pub key: Vec4,
    /// Linear rgb times its strength.
    pub key_color: Vec4,
    /// The fill from above, linear rgb.
    pub sky: Vec4,
    /// The fill from below, linear rgb.
    pub ground: Vec4,
    /// The rim from behind, linear rgb times its strength; w how tight it is.
    pub rim: Vec4,
    /// xyz the direction toward the rim light (view space).
    pub rim_dir: Vec4,
    /// x ambient occlusion's strength, y the cell lines' strength, z unused, w glow (0 to 1).
    pub shade: Vec4,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, gpu::Pod)]
pub struct MascotVertex {
    /// The corner in its part's cells; w the part.
    pub pos: Vec4,
    /// Normal (0 +x, 1 -x, 2 +y, 3 -y, 4 +z, 5 -z), ink, occlusion (0 darkest to 3 open), corner.
    pub info: UVec4,
}

const NORMALS: [Vec3; 6] = [
    vec3(1.0, 0.0, 0.0),
    vec3(-1.0, 0.0, 0.0),
    vec3(0.0, 1.0, 0.0),
    vec3(0.0, -1.0, 0.0),
    vec3(0.0, 0.0, 1.0),
    vec3(0.0, 0.0, -1.0),
];
const FACE_UV: [Vec2; 4] = [vec2(0.0, 0.0), vec2(1.0, 0.0), vec2(1.0, 1.0), vec2(0.0, 1.0)];

pub fn m_srgb_to_linear(v: f32) -> f32 {
    let a = v.abs();
    let l = if a <= 0.04045 { a / 12.92 } else { pow((a + 0.055) / 1.055, 2.4) };
    if v < 0.0 { -l } else { l }
}

pub fn m_linear_to_srgb(v: f32) -> f32 {
    let a = v.abs();
    let s = if a <= 0.0031308 { a * 12.92 } else { 1.055 * pow(a, 1.0 / 2.4) - 0.055 };
    if v < 0.0 { -s } else { s }
}

#[derive(Copy, Clone, Debug, gpu::Varyings)]
pub struct MeshV {
    pub pos: Position,
    pub normal: Flat<Vec3>,
    pub color: Flat<Vec4>,
    pub ao: f32,
    pub uv: Vec2,
}

#[gpu::vertex]
pub fn mascot_vs(vid: VertexIndex, mf: &MascotFrame, verts: &[MascotVertex]) -> MeshV {
    let v = verts[vid.get() as usize];
    let p = (v.pos.w as i32).clamp(0, MAX_PARTS - 1) as usize;
    let q = v.pos.xyz().extend(1.0);
    let c = vec3(dot(mf.parts[p * 3], q), dot(mf.parts[p * 3 + 1], q), dot(mf.parts[p * 3 + 2], q));
    let n0 = NORMALS[v.info.x.min(5) as usize];
    let n = vec3(
        dot(mf.normals[p * 3].xyz(), n0),
        dot(mf.normals[p * 3 + 1].xyz(), n0),
        dot(mf.normals[p * 3 + 2].xyz(), n0),
    );
    MeshV {
        pos: Position(vec4(c.x, c.y, c.z, 1.0)),
        normal: Flat(normalize(n)),
        color: Flat(mf.inks[v.info.y.min((MAX_INKS - 1) as u32) as usize]),
        ao: v.info.z as f32 / 3.0,
        uv: FACE_UV[(v.info.w & 3) as usize],
    }
}

#[gpu::fragment]
pub fn mascot_fs(v: MeshV, mf: &MascotFrame) -> Vec4 {
    let n = normalize(*v.normal);
    let albedo = v.color.rgb();
    let ao = lerp(1.0 - mf.shade.x, 1.0, smoothstep(0.0, 1.0, v.ao));
    let fill = lerp(mf.ground.rgb(), mf.sky.rgb(), n.y * 0.5 + 0.5);
    let key = mf.key_color.rgb() * dot(n, mf.key.xyz()).max(0.0);
    let rim = mf.rim.rgb() * pow(dot(n, mf.rim_dir.xyz()).max(0.0), mf.rim.w.max(1.0));
    let mut lit = albedo * (fill * ao + key * lerp(1.0, ao, 0.5)) + rim * ao;
    lit += albedo * v.color.w * mf.shade.w * 1.4;

    // The line between cells: within a pixel of a cell's edge, gone once a cell is under about six pixels.
    let w = fwidth(v.uv).max(vec2(1e-4, 1e-4));
    let e = v.uv.min(1.0 - v.uv) / w;
    let seam = 1.0 - saturate(e.x.min(e.y) - 0.25);
    let cell = 1.0 / w.x.max(w.y);
    lit *= 1.0 - mf.shade.y * seam * saturate((cell - 6.0) / 10.0);

    vec4(m_linear_to_srgb(lit.x), m_linear_to_srgb(lit.y), m_linear_to_srgb(lit.z), 1.0)
}

// The composite: the picture, or the shadow under it.

/// harness.slang's Frame, which mascot.slang copies (its `textfx` named `pad1` there): one type here.
pub use crate::harness::Frame;

#[repr(C)]
#[derive(Copy, Clone, Debug, gpu::Pod)]
pub struct MascotQuad {
    /// x, y, w, h (points, y down).
    pub rect: Vec4,
    /// x opacity, y what it is (0 the picture, 1 the shadow), z, w unused.
    pub shape: Vec4,
    /// The shadow's colour (extended sRGB) and alpha.
    pub color: Vec4,
}

use crate::harness::CORNERS;

#[derive(Copy, Clone, Debug, gpu::Varyings)]
pub struct QuadV {
    pub pos: Position,
    pub uv: Vec2,
}

#[gpu::vertex]
pub fn mascot_quad_vs(vid: VertexIndex, frame: &Frame, quad: &MascotQuad) -> QuadV {
    let c = CORNERS[vid.get() as usize];
    let pt = quad.rect.xy() + c * quad.rect.zw() - frame.place.xy();
    QuadV {
        pos: Position(vec4(pt.x / frame.view.x * 2.0 - 1.0, 1.0 - pt.y / frame.view.y * 2.0, 0.0, 1.0)),
        uv: c,
    }
}

/// Premultiplied: the picture as it is, the shadow a soft ellipse darkest under the feet.
#[gpu::fragment]
pub fn mascot_quad_fs(v: QuadV, quad: &MascotQuad, src: &Texture2d, smp: &Sampler) -> Vec4 {
    if quad.shape.y < 0.5 {
        return src.sample(smp, v.uv) * quad.shape.x;
    }
    let d = length(v.uv * 2.0 - 1.0);
    let a = (1.0 - smoothstep(0.2, 1.0, d)) * quad.color.w * quad.shape.x;
    vec4(quad.color.x * a, quad.color.y * a, quad.color.z * a, a)
}
