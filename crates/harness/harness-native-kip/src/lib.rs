//! den's mascot, Kip, ported whole from den-native's src/mascot (github.com/andrewgazelka/den, 008da141): a small voxel
//! creature that keeps the person company (an empty chat, perched on the composer while an agent works, a `<Kip>` a cell
//! drew) and shows what the agent is doing. den's fed1eb428d:docs/research/mascot.md has the why of every choice here. Nothing here
//! touches a GPU or a clock: the host steps a [`Mascot`] by the frame's time and hands harness-native-render a [`MascotQuad`].
//!
//! - [`model`]: den's `.voxels` text format and its mesh (faces with per-corner ambient occlusion).
//! - [`rig`]: the parts the animation drives by role, a [`rig::Pose`] of them and the matrices it puts them at.
//! - [`anim`]: the moods' state machine, crossfades, walking and hopping with root motion, breath, blinks, look-at and
//!   springs, on the frame clock in fixed steps.
//! - This module: the character (Kip, baked in), the scene's look, and one frame's [`MascotQuad`]: the picture's rect in
//!   device pixels and everything its render reads.
//! - [`act`]: what a `<Kip mood="…"/>` asks of it, a mood or a few in turn.
//! - harness-native-render draws it with harness-native-shaders' `mascot` shaders (kip.rs there).
pub mod act;
pub mod anim;
pub mod model;
pub mod rig;

pub use anim::{Clip, Mascot, Mood, Motion, Tuning};
use bytemuck::{Pod, Zeroable};
use model::{MAX_INKS, MAX_PARTS, Model, Vertex};
use rig::{Affine, Pose, Rig};
use std::hash::{Hash, Hasher};
use std::sync::{Arc, LazyLock};

/// Kip, as checked in.
pub const KIP: &str = include_str!("../art/kip.voxels");

/// Cells of room around the character in its picture: above its head, at each side, and under its feet.
const ROOM: f32 = 2.0;

/// A character ready to draw: its rig, its mesh and a key that names the mesh.
#[derive(Debug)]
pub struct Character {
    pub rig: Rig,
    pub mesh: Arc<[Vertex]>,
    pub key: u64,
}

impl Character {
    /// A character from its `.voxels` text.
    pub fn parse(text: &str) -> Result<Self, String> {
        let model = Model::parse(text)?;
        let mesh: Arc<[Vertex]> = model.mesh().into();
        let mut h = std::collections::hash_map::DefaultHasher::new();
        text.hash(&mut h);
        Ok(Self { rig: Rig::new(model), mesh, key: h.finish() })
    }

    /// Kip.
    #[must_use]
    pub fn kip() -> Arc<Self> {
        static KIP_CHARACTER: LazyLock<Arc<Character>> = LazyLock::new(|| Arc::new(Character::parse(KIP).expect("kip.voxels parses")));
        KIP_CHARACTER.clone()
    }

    /// The side of its square picture, cells.
    #[must_use]
    pub fn side(&self) -> f32 {
        self.rig.height.max(2.0 * self.rig.half_width) + 2.0 * ROOM
    }
}

/// A colour of the scene in both appearances (sRGB-encoded, as the scene gives it).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Pair {
    pub dark: [f32; 3],
    pub light: [f32; 3],
}

impl Pair {
    #[must_use]
    pub fn at(&self, dark: f32) -> [f32; 3] {
        let d = dark.clamp(0.0, 1.0);
        std::array::from_fn(|i| self.light[i] + (self.dark[i] - self.light[i]) * d)
    }
}

/// A recolouring of one of a character's inks, from the scene's `MascotInk`.
#[derive(Clone, Debug, PartialEq)]
pub struct Ink {
    pub name: String,
    pub color: Pair,
    /// How much it glows when the character does (thinking), 0 to 1.
    pub glow: f32,
}

/// How a mascot is lit and seen, from the scene's `MascotLook` (angles in radians, directions toward the light in the
/// view's space: x right, y up, z toward the viewer).
#[derive(Clone, Debug, PartialEq)]
pub struct Look {
    pub pitch: f32,
    pub yaw: f32,
    pub key_dir: [f32; 3],
    pub key: Pair,
    pub key_strength: f32,
    pub sky: Pair,
    pub ground: Pair,
    pub fill: f32,
    pub rim_dir: [f32; 3],
    pub rim: Pair,
    pub rim_strength: f32,
    pub rim_power: f32,
    pub ao: f32,
    pub seams: f32,
    pub shadow: Pair,
    /// The shadow's alpha, dark and light.
    pub shadow_alpha: [f32; 2],
    pub inks: Vec<Ink>,
    /// Seconds it takes to come and go.
    pub fade: f32,
}

/// Everything the scene says about the mascot: its look and its motion.
#[derive(Clone, Debug, PartialEq)]
pub struct Setup {
    pub look: Look,
    pub tuning: Tuning,
}

/// `MascotFrame` in harness-native-shaders' `mascot`, byte for byte.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
pub struct Uniforms {
    pub parts: [[[f32; 4]; 3]; MAX_PARTS],
    pub normals: [[[f32; 4]; 3]; MAX_PARTS],
    pub inks: [[f32; 4]; MAX_INKS],
    pub key: [f32; 4],
    pub key_color: [f32; 4],
    pub sky: [f32; 4],
    pub ground: [f32; 4],
    pub rim: [f32; 4],
    pub rim_dir: [f32; 4],
    pub shade: [f32; 4],
}

impl Default for Uniforms {
    fn default() -> Self {
        Self::zeroed()
    }
}

fn linear(c: [f32; 3]) -> [f32; 3] {
    c.map(|v| if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) })
}

fn norm(v: [f32; 3]) -> [f32; 3] {
    let l = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt().max(1e-6);
    v.map(|x| x / l)
}

/// The inverse transpose of `m`'s 3 by 3 part: what turns a normal along with it.
fn normal_matrix(m: &Affine) -> [[f32; 4]; 3] {
    let a = |r: usize, c: usize| m[r][c];
    let cof = |r: usize, c: usize| {
        let (r1, r2, c1, c2) = ((r + 1) % 3, (r + 2) % 3, (c + 1) % 3, (c + 2) % 3);
        a(r1, c1) * a(r2, c2) - a(r1, c2) * a(r2, c1)
    };
    let det = a(0, 0) * cof(0, 0) + a(0, 1) * cof(0, 1) + a(0, 2) * cof(0, 2);
    let inv = if det.abs() < 1e-8 { 0.0 } else { 1.0 / det };
    // (A^-1)^T = cofactor matrix / det.
    std::array::from_fn(|r| [cof(r, 0) * inv, cof(r, 1) * inv, cof(r, 2) * inv, 0.0])
}

/// The view's turn: the character seen from `pitch` above and turned `yaw`.
#[must_use]
pub fn view(look: &Look) -> Affine {
    rig::mul(&rig::rot_x(look.pitch), &rig::rot_y(look.yaw))
}

/// One frame's uniforms: `ch` in `pose` (its lift left out: the picture moves for it), lit as `look` says in an
/// appearance `dark` of the way to dark, framed in a square `side` cells across.
#[must_use]
pub fn uniforms(ch: &Character, pose: &Pose, look: &Look, dark: f32, side: f32) -> Uniforms {
    let mut u = Uniforms::default();
    let grounded = Pose { lift: 0.0, ..*pose };
    let parts = ch.rig.compose(&grounded);
    let eye = rig::mul(&view(look), &Rig::root(&grounded));
    let half = side * 0.5;
    // The ground point sits ROOM cells over the picture's bottom edge; depth spans the picture's side either way.
    let cy = -ROOM + half;
    let proj: Affine = [[1.0 / half, 0.0, 0.0, 0.0], [0.0, 1.0 / half, 0.0, -cy / half], [0.0, 0.0, -0.5 / side, 0.5]];
    for (i, m) in parts.iter().enumerate().take(MAX_PARTS) {
        let v = rig::mul(&eye, m);
        u.parts[i] = rig::mul(&proj, &v);
        u.normals[i] = normal_matrix(&v);
    }
    for (i, (name, rgb)) in ch.rig.model.inks.iter().enumerate().take(MAX_INKS) {
        let (c, glow) = match look.inks.iter().find(|k| &k.name == name) {
            Some(k) => (k.color.at(dark), k.glow),
            None => (*rgb, 0.0),
        };
        let l = linear(c);
        u.inks[i] = [l[0], l[1], l[2], glow];
    }
    let with = |c: [f32; 3], s: f32, w: f32| {
        let l = linear(c);
        [l[0] * s, l[1] * s, l[2] * s, w]
    };
    let k = norm(look.key_dir);
    let r = norm(look.rim_dir);
    u.key = [k[0], k[1], k[2], 0.0];
    u.key_color = with(look.key.at(dark), look.key_strength, 0.0);
    u.sky = with(look.sky.at(dark), look.fill, 0.0);
    u.ground = with(look.ground.at(dark), look.fill, 0.0);
    u.rim = with(look.rim.at(dark), look.rim_strength, look.rim_power);
    u.rim_dir = [r[0], r[1], r[2], 0.0];
    u.shade = [look.ao.clamp(0.0, 1.0), look.seams.clamp(0.0, 1.0), 0.0, pose.glow.clamp(0.0, 1.0)];
    u
}

/// Where a mascot's picture goes: its feet at `feet` (points), `cell` points a cell, lifted `lift` cells, at `scale`
/// device pixels a point. The rect (points) lands on whole device pixels, its size a whole number of them.
#[must_use]
pub fn placement(ch: &Character, feet: [f32; 2], cell: f32, lift: f32, scale: f32) -> ([f32; 4], [u32; 2]) {
    let scale = scale.max(0.25);
    let side_pt = ch.side() * cell;
    let px = (side_pt * scale).ceil().max(1.0);
    let side = px / scale;
    let bottom = feet[1] - lift * cell + ROOM * cell;
    let x = ((feet[0] - side * 0.5) * scale).round() / scale;
    let y = ((bottom - side) * scale).round() / scale;
    ([x, y, side, side], [px as u32, px as u32])
}

/// The contact shadow under feet at `feet`: an ellipse as wide as the character, smaller and fainter the higher it is.
#[must_use]
pub fn shadow(ch: &Character, feet: [f32; 2], cell: f32, lift: f32) -> ([f32; 4], f32) {
    let away = 1.0 / (1.0 + 0.12 * lift.max(0.0));
    let rx = (ch.rig.half_width + 0.5) * cell * (0.55 + 0.45 * away);
    let ry = rx * 0.32;
    ([feet[0] - rx, feet[1] - ry * 0.85, 2.0 * rx, 2.0 * ry], away)
}

/// One mascot in one frame, for a renderer: its picture's rect and size, what renders it, and how it is set in.
#[derive(Clone, Debug, PartialEq)]
pub struct MascotQuad {
    /// Which mascot (its picture is kept from frame to frame and rendered again only when `uniforms` change).
    pub id: u64,
    pub mesh_key: u64,
    pub mesh: Arc<[Vertex]>,
    pub rect: [f32; 4],
    pub pixels: [u32; 2],
    pub alpha: f32,
    pub uniforms: Uniforms,
    /// The contact shadow's rect (points) and colour (extended sRGB, alpha).
    pub shadow: [f32; 4],
    pub shadow_color: [f32; 4],
}

/// The quad of mascot `id`: `ch` in `pose`, its feet at `feet`, `height` points tall, at `alpha`.
#[must_use]
pub fn quad(id: u64, ch: &Character, pose: &Pose, look: &Look, feet: [f32; 2], height: f32, alpha: f32, dark: f32, scale: f32) -> MascotQuad {
    let cell = height / ch.rig.height.max(1.0);
    let (rect, pixels) = placement(ch, feet, cell, pose.lift, scale);
    // The picture's side in cells as it was rounded to whole pixels, so a cell is the same size on screen.
    let side = rect[2] / cell;
    let (shadow, away) = shadow(ch, feet, cell, pose.lift);
    let s = look.shadow.at(dark);
    MascotQuad {
        id,
        mesh_key: ch.key,
        mesh: ch.mesh.clone(),
        rect,
        pixels,
        alpha,
        uniforms: uniforms(ch, pose, look, dark, side),
        shadow,
        shadow_color: [s[0], s[1], s[2], (look.shadow_alpha[1] + (look.shadow_alpha[0] - look.shadow_alpha[1]) * dark.clamp(0.0, 1.0)) * away],
    }
}

/// Cells of room over a Kip's head in his box (a hop for joy peaks 2.2 cells up) and under his feet (his shadow), and on
/// his right for the arm he waves with (den-native's ui/kip.rs).
pub const HOP_ROOM: f32 = 2.6;
pub const FOOT_ROOM: f32 = 0.7;
pub const ARM_ROOM: f32 = 5.0;
/// How far in from the box's left edge his feet stand, past his half width (cells): the turned view brings his near
/// corner out to about here, so his body lines up with the words' left edge.
pub const LEFT_IN: f32 = 0.6;
/// The heights he may be drawn at, points.
pub const SHORTEST: f32 = 20.0;
pub const TALLEST: f32 = 160.0;

/// The box (w, h, points) of `ch` `tall` points tall: room over his head for a hop and under his feet for his shadow, and
/// for a waving arm on his right (his feet at the left, as he stands in a column of words), or on both sides when
/// `centre`.
#[must_use]
pub fn gauge(ch: &Character, tall: f32, centre: bool) -> [f32; 2] {
    let rig = &ch.rig;
    let cell = tall / rig.height.max(1.0);
    let across = if centre { 2.0 * (rig.half_width + ARM_ROOM) } else { 2.0 * rig.half_width + LEFT_IN + ARM_ROOM };
    [across * cell, (rig.height + HOP_ROOM + FOOT_ROOM) * cell]
}

/// Where his feet stand in his box `at` (x, y, w, h, points) as [`gauge`] measured it, `tall` points tall.
#[must_use]
pub fn feet(ch: &Character, at: [f32; 4], tall: f32, centre: bool) -> [f32; 2] {
    let cell = tall / ch.rig.height.max(1.0);
    let x = if centre { at[0] + at[2] * 0.5 } else { at[0] + (ch.rig.half_width + LEFT_IN) * cell };
    [x, at[1] + at[3] - FOOT_ROOM * cell]
}
