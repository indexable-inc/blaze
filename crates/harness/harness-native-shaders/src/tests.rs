use crate::mascot::{self, MascotFrame, MascotVertex, m_linear_to_srgb, m_srgb_to_linear};
use core::mem::{offset_of, size_of};
use gpu::prelude::*;
use harness_native_kip::{Character, Ink, Look, Pair, Uniforms, model::Vertex, rig::Pose};

/// The shader's buffers are the host's, byte for byte: harness-native-kip writes `Uniforms` and
/// `model::Vertex`, the shader reads `MascotFrame` and `MascotVertex`.
#[test]
fn mascot_layouts_match_the_host() {
    assert_eq!(size_of::<MascotFrame>(), size_of::<Uniforms>());
    assert_eq!(offset_of!(MascotFrame, parts), offset_of!(Uniforms, parts));
    assert_eq!(offset_of!(MascotFrame, normals), offset_of!(Uniforms, normals));
    assert_eq!(offset_of!(MascotFrame, inks), offset_of!(Uniforms, inks));
    assert_eq!(offset_of!(MascotFrame, key), offset_of!(Uniforms, key));
    assert_eq!(offset_of!(MascotFrame, key_color), offset_of!(Uniforms, key_color));
    assert_eq!(offset_of!(MascotFrame, sky), offset_of!(Uniforms, sky));
    assert_eq!(offset_of!(MascotFrame, ground), offset_of!(Uniforms, ground));
    assert_eq!(offset_of!(MascotFrame, rim), offset_of!(Uniforms, rim));
    assert_eq!(offset_of!(MascotFrame, rim_dir), offset_of!(Uniforms, rim_dir));
    assert_eq!(offset_of!(MascotFrame, shade), offset_of!(Uniforms, shade));
    assert_eq!(size_of::<MascotVertex>(), size_of::<Vertex>());
    assert_eq!(offset_of!(MascotVertex, pos), offset_of!(Vertex, pos));
    assert_eq!(offset_of!(MascotVertex, info), offset_of!(Vertex, info));
}

/// The binding table the host binds by: per entry point, per table, in parameter order.
#[test]
fn mascot_slots() {
    use gpu::{Slot, Table};
    let b = |index| Slot { table: Table::Buffer, index };
    assert_eq!(mascot::mascot_vs::mf, b(0));
    assert_eq!(mascot::mascot_vs::verts, b(1));
    assert_eq!(mascot::mascot_fs::mf, b(0));
    assert_eq!(mascot::mascot_quad_vs::frame, b(0));
    assert_eq!(mascot::mascot_quad_vs::quad, b(1));
    assert_eq!(mascot::mascot_quad_fs::quad, b(0));
    assert_eq!(mascot::mascot_quad_fs::src, Slot { table: Table::Texture, index: 0 });
    assert_eq!(mascot::mascot_quad_fs::smp, Slot { table: Table::Sampler, index: 0 });
    let names: Vec<&str> = crate::GPU_ENTRIES.0.iter().map(|e| e.desc.name).filter(|n| n.starts_with("mascot")).collect();
    assert_eq!(names, ["mascot_vs", "mascot_fs", "mascot_quad_vs", "mascot_quad_fs"]);
}

fn look() -> Look {
    let pair = |dark: [f32; 3], light: [f32; 3]| Pair { dark, light };
    Look {
        pitch: 0.35,
        yaw: -0.6,
        key_dir: [-0.4, 0.7, 0.6],
        key: pair([1.0, 0.97, 0.92], [1.0, 0.98, 0.95]),
        key_strength: 0.9,
        sky: pair([0.55, 0.6, 0.8], [0.7, 0.75, 0.9]),
        ground: pair([0.3, 0.28, 0.3], [0.5, 0.48, 0.5]),
        fill: 0.6,
        rim_dir: [0.3, 0.4, -0.8],
        rim: pair([0.8, 0.85, 1.0], [0.9, 0.9, 1.0]),
        rim_strength: 0.5,
        rim_power: 3.0,
        ao: 0.6,
        seams: 0.3,
        shadow: pair([0.0, 0.0, 0.0], [0.1, 0.1, 0.2]),
        shadow_alpha: [0.5, 0.25],
        inks: vec![Ink { name: "tip".to_owned(), color: pair([1.0, 0.8, 0.3], [1.0, 0.7, 0.2]), glow: 1.0 }],
        fade: 0.3,
    }
}

/// mascot_vs over every vertex of Kip at rest, with the uniforms and mesh the host really sends:
/// each corner lands inside his picture's clip volume (Metal's depth 0 to 1), with a unit normal
/// and one of the uniforms' inks. A mistake in the port's indexing (a part's rows, the ink's clamp)
/// or a layout disagreement puts corners outside or makes NaNs.
#[test]
fn mascot_vs_frames_kip_inside_his_picture() {
    let ch = Character::kip();
    let u = harness_native_kip::uniforms(&ch, &Pose::default(), &look(), 1.0, ch.side());
    // SAFETY: MascotFrame is Pod and the same size as Uniforms (mascot_layouts_match_the_host), so
    // its bytes are a MascotFrame; read_unaligned asks nothing of their alignment.
    let mf: MascotFrame = unsafe { core::ptr::read_unaligned((&raw const u).cast::<MascotFrame>()) };
    // SAFETY: MascotVertex is Pod with Vertex's size and alignment (both 32-byte structs of 4-byte
    // fields, checked above), so the mesh's elements are MascotVertexes, as many as it has.
    let verts: &[MascotVertex] = unsafe { core::slice::from_raw_parts(ch.mesh.as_ptr().cast(), ch.mesh.len()) };
    assert!(!verts.is_empty());
    for vid in 0..verts.len() {
        let o = mascot::mascot_vs(VertexIndex(vid as u32), &mf, verts);
        let p = o.pos.0;
        let inside = p.x.abs() <= 1.0 + 1e-4 && p.y.abs() <= 1.0 + 1e-4 && (-1e-4..=1.0 + 1e-4).contains(&p.z);
        assert!(inside && p.w == 1.0, "vertex {vid} ({:?}) lands at {p:?}, outside his picture", verts[vid]);
        assert!((o.normal.length() - 1.0).abs() < 1e-4, "vertex {vid}: normal {:?}", *o.normal);
        assert!(mf.inks.contains(&*o.color), "vertex {vid}: colour {:?} is no ink", *o.color);
        assert!((0.0..=1.0).contains(&o.ao), "vertex {vid}: occlusion {}", o.ao);
    }
}

/// The composite's corners cover the quad's rect: the first corner is its top left, the last its
/// bottom right, in clip space of a view `view` points across.
#[test]
fn mascot_quad_vs_covers_its_rect() {
    let frame = mascot::Frame {
        view: vec2(200.0, 100.0),
        scale: 2.0,
        time: 0.0,
        dark: 1.0,
        live: 2.0,
        textfx: Vec2::default(),
        tune: Vec4::default(),
        slug: Vec4::default(),
        place: vec4(0.0, 0.0, 0.0, 0.0),
    };
    let quad = mascot::MascotQuad { rect: vec4(0.0, 0.0, 100.0, 50.0), shape: Vec4::default(), color: Vec4::default() };
    let tl = mascot::mascot_quad_vs(VertexIndex(0), &frame, &quad);
    let br = mascot::mascot_quad_vs(VertexIndex(3), &frame, &quad);
    assert_eq!(tl.pos.0, vec4(-1.0, 1.0, 0.0, 1.0));
    assert_eq!(br.pos.0, vec4(0.0, 0.0, 0.0, 1.0));
    assert_eq!((tl.uv, br.uv), (vec2(0.0, 0.0), vec2(1.0, 1.0)));
}

/// The transfer functions are odd (extended sRGB carries negative values), invert each other over
/// the extended range, and meet the pinned points: 0, 1, and the encoding's 0.5 at 0.214 linear.
#[hegel::test]
fn mascot_transfer_functions_invert_each_other(tc: hegel::TestCase) {
    use hegel::generators as gs;
    let x = tc.draw(gs::floats::<f32>().min_value(-8.0).max_value(8.0).allow_nan(false).allow_infinity(false));
    let back = m_linear_to_srgb(m_srgb_to_linear(x));
    assert!((back - x).abs() <= 1e-5 * x.abs().max(1.0), "{x} decodes and encodes to {back}");
    assert_eq!(m_srgb_to_linear(-x), -m_srgb_to_linear(x));
    assert_eq!(m_linear_to_srgb(-x), -m_linear_to_srgb(x));
    assert_eq!(m_srgb_to_linear(0.0), 0.0);
    assert!((m_srgb_to_linear(1.0) - 1.0).abs() < 1e-6);
    assert!((m_srgb_to_linear(0.5) - 0.214_041).abs() < 1e-5);
}
