//! The rig: the parts the animation drives, a pose of them, and the matrices a pose puts each part at.
//!
//! The animation knows a character only by the roles of its parts, found by name in its model (`body`, `eye_l`, ...).
//! A character that has no part for a role is simply not moved there, so a plugin's creature with no antennae or no arms
//! still walks, blinks and looks. Every other part rides on its parent.
use super::model::{MAX_PARTS, Model};

/// What a part is to the animation, and its name in a model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Body,
    EyeL,
    EyeR,
    Mouth,
    AntennaL,
    AntennaR,
    ArmL,
    ArmR,
    FootL,
    FootR,
}

impl Role {
    pub const ALL: [Self; 10] = [Self::Body, Self::EyeL, Self::EyeR, Self::Mouth, Self::AntennaL, Self::AntennaR, Self::ArmL, Self::ArmR, Self::FootL, Self::FootR];

    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Body => "body",
            Self::EyeL => "eye_l",
            Self::EyeR => "eye_r",
            Self::Mouth => "mouth",
            Self::AntennaL => "antenna_l",
            Self::AntennaR => "antenna_r",
            Self::ArmL => "arm_l",
            Self::ArmR => "arm_r",
            Self::FootL => "foot_l",
            Self::FootR => "foot_r",
        }
    }
}

/// A part's motion over its rest: moved by `t` (cells, in its parent), turned by `r` (radians about X, Y, Z, after its
/// rest turn) and scaled by `s` about its pivot.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Joint {
    pub t: [f32; 3],
    pub r: [f32; 3],
    pub s: [f32; 3],
}

impl Default for Joint {
    fn default() -> Self {
        Self { t: [0.0; 3], r: [0.0; 3], s: [1.0; 3] }
    }
}

fn lerp3(a: [f32; 3], b: [f32; 3], w: f32) -> [f32; 3] {
    [a[0] + (b[0] - a[0]) * w, a[1] + (b[1] - a[1]) * w, a[2] + (b[2] - a[2]) * w]
}

impl Joint {
    #[must_use]
    pub fn lerp(&self, o: &Self, w: f32) -> Self {
        Self { t: lerp3(self.t, o.t, w), r: lerp3(self.r, o.r, w), s: lerp3(self.s, o.s, w) }
    }
}

/// A whole pose: every role's joint and the root's own motion.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pose {
    pub joints: [Joint; 10],
    /// Height of the feet over the ground, cells.
    pub lift: f32,
    /// The whole character's turn about the vertical (radians; positive turns its face to the right of the screen).
    pub yaw: f32,
    /// Squash (below 1) or stretch (above 1) along the vertical, about the ground; the width gives back the volume.
    pub squash: f32,
    /// How lit the antennae's tips are, 0 to 1.
    pub glow: f32,
}

impl Default for Pose {
    fn default() -> Self {
        Self { joints: [Joint::default(); 10], lift: 0.0, yaw: 0.0, squash: 1.0, glow: 0.0 }
    }
}

impl Pose {
    #[must_use]
    pub fn joint(&self, r: Role) -> &Joint {
        &self.joints[r as usize]
    }

    pub fn joint_mut(&mut self, r: Role) -> &mut Joint {
        &mut self.joints[r as usize]
    }

    #[must_use]
    pub fn lerp(&self, o: &Self, w: f32) -> Self {
        let mut joints = self.joints;
        for (j, b) in joints.iter_mut().zip(&o.joints) {
            *j = j.lerp(b, w);
        }
        let l = |a: f32, b: f32| a + (b - a) * w;
        Self { joints, lift: l(self.lift, o.lift), yaw: l(self.yaw, o.yaw), squash: l(self.squash, o.squash), glow: l(self.glow, o.glow) }
    }
}

/// An affine transform as its three rows (x, y, z of the result; the fourth column is the move).
pub type Affine = [[f32; 4]; 3];

pub const IDENTITY: Affine = [[1.0, 0.0, 0.0, 0.0], [0.0, 1.0, 0.0, 0.0], [0.0, 0.0, 1.0, 0.0]];

#[must_use]
pub fn mul(a: &Affine, b: &Affine) -> Affine {
    let mut o = [[0.0; 4]; 3];
    for (i, row) in o.iter_mut().enumerate() {
        for (j, v) in row.iter_mut().enumerate() {
            *v = a[i][0] * b[0][j] + a[i][1] * b[1][j] + a[i][2] * b[2][j] + if j == 3 { a[i][3] } else { 0.0 };
        }
    }
    o
}

#[must_use]
pub fn apply(m: &Affine, p: [f32; 3]) -> [f32; 3] {
    let r = |i: usize| m[i][0] * p[0] + m[i][1] * p[1] + m[i][2] * p[2] + m[i][3];
    [r(0), r(1), r(2)]
}

#[must_use]
pub fn translate(t: [f32; 3]) -> Affine {
    [[1.0, 0.0, 0.0, t[0]], [0.0, 1.0, 0.0, t[1]], [0.0, 0.0, 1.0, t[2]]]
}

#[must_use]
pub fn scale(s: [f32; 3]) -> Affine {
    [[s[0], 0.0, 0.0, 0.0], [0.0, s[1], 0.0, 0.0], [0.0, 0.0, s[2], 0.0]]
}

#[must_use]
pub fn rot_x(a: f32) -> Affine {
    let (s, c) = a.sin_cos();
    [[1.0, 0.0, 0.0, 0.0], [0.0, c, -s, 0.0], [0.0, s, c, 0.0]]
}

#[must_use]
pub fn rot_y(a: f32) -> Affine {
    let (s, c) = a.sin_cos();
    [[c, 0.0, s, 0.0], [0.0, 1.0, 0.0, 0.0], [-s, 0.0, c, 0.0]]
}

#[must_use]
pub fn rot_z(a: f32) -> Affine {
    let (s, c) = a.sin_cos();
    [[c, -s, 0.0, 0.0], [s, c, 0.0, 0.0], [0.0, 0.0, 1.0, 0.0]]
}

/// Turned about X, then Y, then Z.
#[must_use]
pub fn rot(r: [f32; 3]) -> Affine {
    mul(&rot_z(r[2]), &mul(&rot_y(r[1]), &rot_x(r[0])))
}

/// A model with its parts' roles found.
#[derive(Clone, Debug)]
pub struct Rig {
    pub model: Model,
    /// Each part's role, if the animation drives it.
    pub roles: Vec<Option<Role>>,
    /// Where each role is in `model.parts`.
    pub of: [Option<usize>; 10],
    /// Height of the character at rest, cells: from the ground to its highest cell.
    pub height: f32,
    /// Half its width at rest, cells.
    pub half_width: f32,
}

impl Rig {
    #[must_use]
    pub fn new(model: Model) -> Self {
        let roles: Vec<Option<Role>> = model.parts.iter().map(|p| Role::ALL.into_iter().find(|r| r.name() == p.name)).collect();
        let mut of = [None; 10];
        for (i, r) in roles.iter().enumerate() {
            if let Some(r) = r {
                of[*r as usize] = Some(i);
            }
        }
        let mut rig = Self { model, roles, of, height: 1.0, half_width: 1.0 };
        let world = rig.compose(&Pose::default());
        let (mut top, mut side) = (0.0f32, 0.0f32);
        for (p, m) in rig.model.parts.iter().zip(&world) {
            for c in p.cells.keys() {
                for corner in 0..8 {
                    let q = [c[0] as f32 + (corner & 1) as f32, c[1] as f32 + ((corner >> 1) & 1) as f32, c[2] as f32 + ((corner >> 2) & 1) as f32];
                    let w = apply(m, q);
                    top = top.max(w[1]);
                    side = side.max(w[0].abs());
                }
            }
        }
        rig.height = top.max(1.0);
        rig.half_width = side.max(0.5);
        rig
    }

    /// Each part's transform from its own cells to the character's space (the ground point between its feet at the
    /// origin, before its root motion's lift, turn and squash, which [`Self::root`] adds).
    #[must_use]
    pub fn compose(&self, pose: &Pose) -> Vec<Affine> {
        let mut out: Vec<Affine> = Vec::with_capacity(self.model.parts.len().min(MAX_PARTS));
        for (i, p) in self.model.parts.iter().enumerate().take(MAX_PARTS) {
            let j = self.roles[i].map_or(Joint::default(), |r| *pose.joint(r));
            let at = [p.at[0] + j.t[0], p.at[1] + j.t[1], p.at[2] + j.t[2]];
            let local = mul(&translate(at), &mul(&rot(p.rest), &mul(&rot(j.r), &scale(j.s))));
            let parent = p.parent.map_or(IDENTITY, |q| out[q]);
            out.push(mul(&parent, &local));
        }
        out
    }

    /// The root's motion: squash about the ground, turned, lifted.
    #[must_use]
    pub fn root(pose: &Pose) -> Affine {
        let sq = pose.squash.max(0.2);
        let wide = 1.0 / sq.sqrt();
        mul(&translate([0.0, pose.lift, 0.0]), &mul(&rot_y(pose.yaw), &scale([wide, sq, wide])))
    }
}
