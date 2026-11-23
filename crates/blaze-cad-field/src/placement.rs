//! PLACEMENT IS NOT GEOMETRY (D183, "what goes wrong", item 1). A [`Placement`] is a rigid map
//! (a rotation, maybe a mirror, and a shift) held EXACTLY on a grid: the linear part as whole
//! multiples of 2^-40, the shift as whole nanometres. Two placements are equal when their grid
//! values are, so a placement is a key; the interpreters' f64 values are the grid values
//! (`n * 2^-40` and `n * 10^-9`), each one fixed conversion.
//!
//! A rotation by a whole fraction of a turn that a quarter divides (every multiple of 90, 45 and
//! 30 degrees) is made from blaze-math's `sin_cos_turns`: quarter turns are signed permutations
//! with entries exactly 0 and 1 and compose with no rounding at all, so a part turned twice by
//! 90 degrees and once by 180 has one key. Any other angle snaps each entry to the grid; such a
//! matrix is orthonormal to within 2^-39, and its transpose is taken as its inverse: a field
//! through it is 1-Lipschitz to within a factor of `1 + 2^-38`, far below every tolerance the
//! kernel is asked for (this is stated, not hidden: a `Placement` is only ever said to keep
//! distances to that factor).

use blaze_math::{Aabb3, Affine3, Mat3, TurnUnit, Vec3, sin_cos_turns};
use blaze_units::{Angle, Length};

/// The grid of a placement's linear part: entries are whole multiples of 2^-40.
pub const LINEAR_GRID: f64 = 1.0 / 1_099_511_627_776.0;
const PER_UNIT: f64 = 1_099_511_627_776.0;

/// blaze-units' angle grid as blaze-math's turn unit: a multiple of 4 below 2^52, checked when
/// this crate compiles.
pub(crate) const ANGLE_UNIT: TurnUnit = match TurnUnit::new(Angle::<blaze_units::Plain>::PER_TURN) {
    Ok(u) => u,
    Err(_) => panic!("blaze-units' angle grid does not divide a turn by four"),
};

/// An axis of a frame.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Axis {
    X,
    Y,
    Z,
}

impl Axis {
    /// 0, 1 or 2.
    pub const fn index(self) -> usize {
        match self {
            Axis::X => 0,
            Axis::Y => 1,
            Axis::Z => 2,
        }
    }
}

/// A rigid placement on the grid (see the module).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Placement {
    /// The linear part by rows, in units of 2^-40: a rotation, or a rotation after a mirror.
    linear: [[i64; 3]; 3],
    /// The shift, in nanometres.
    shift: [i64; 3],
}

fn snap(v: f64) -> i64 {
    // Every entry of a rotation is in [-1, 1], so the product is far inside i64.
    core::f64::math::round(v * PER_UNIT) as i64
}

fn snap_nm(v: f64) -> i64 {
    core::f64::math::round(v * 1e9) as i64
}

impl Placement {
    /// No placement.
    pub const IDENTITY: Placement = Placement {
        linear: [[1 << 40, 0, 0], [0, 1 << 40, 0], [0, 0, 1 << 40]],
        shift: [0; 3],
    };

    /// A shift by `(x, y, z)`.
    pub const fn shift(x: Length, y: Length, z: Length) -> Placement {
        Placement { linear: Placement::IDENTITY.linear, shift: [x.as_nm(), y.as_nm(), z.as_nm()] }
    }

    /// A rotation about `axis` through the origin by `angle` (right-handed).
    pub fn rotation(axis: Axis, angle: Angle) -> Placement {
        let (s, c) = sin_cos_turns(angle.as_microarcsec(), ANGLE_UNIT);
        let m = match axis {
            Axis::X => Mat3::rotation_x(s, c),
            Axis::Y => Mat3::rotation_y(s, c),
            Axis::Z => Mat3::rotation_z(s, c),
        };
        Placement::from_linear(&m, [0; 3])
    }

    /// The frame whose local `z` axis points along `dir` and whose origin is `origin` (metres):
    /// local `x` is the component of world `z` (or world `x` when `dir` is vertical) across
    /// `dir`, local `y` completes a right-handed frame. Each entry is snapped to the grid.
    /// `None` when `dir` has no length.
    pub fn along(dir: Vec3, origin: Vec3) -> Option<Placement> {
        let z = dir.normalized()?;
        let up = if z.z.abs() > 0.9 { Vec3::new(1.0, 0.0, 0.0) } else { Vec3::new(0.0, 0.0, 1.0) };
        let x = (up - z * up.dot(z)).normalized()?;
        let y = z.cross(x);
        // Columns are the local axes in world terms: world = M local + origin.
        let m = Mat3::new([[x.x, y.x, z.x], [x.y, y.y, z.y], [x.z, y.z, z.z]]);
        Some(Placement::from_linear(&m, [snap_nm(origin.x), snap_nm(origin.y), snap_nm(origin.z)]))
    }

    /// The frame whose local axes are the orthonormal triple `(x, y, z)` and whose origin is
    /// `origin` (metres), snapped to the grid.
    pub(crate) fn from_axes(x: Vec3, y: Vec3, z: Vec3, origin: Vec3) -> Placement {
        let m = Mat3::new([[x.x, y.x, z.x], [x.y, y.y, z.y], [x.z, y.z, z.z]]);
        Placement::from_linear(&m, [snap_nm(origin.x), snap_nm(origin.y), snap_nm(origin.z)])
    }

    /// This placement with its shift multiplied by `factor` and rounded to the nanometre: what
    /// a placement becomes when hoisted above a uniform scale by `factor` (the only rounding a
    /// hoist does, at most half a nanometre per axis).
    pub fn with_shift_scaled(&self, factor: f64) -> Placement {
        let s = self.shift_m() * factor;
        Placement { linear: self.linear, shift: [snap_nm(s.x), snap_nm(s.y), snap_nm(s.z)] }
    }

    /// The same map as blaze-math's [`Affine3`] (metres).
    pub fn to_affine(&self) -> Affine3 {
        Affine3::rigid(self.linear(), self.shift_m())
    }

    /// The mirror across the plane through the origin normal to `axis`.
    pub const fn mirror(axis: Axis) -> Placement {
        let mut linear = Placement::IDENTITY.linear;
        let i = axis.index();
        linear[i][i] = -(1 << 40);
        Placement { linear, shift: [0; 3] }
    }

    fn from_linear(m: &Mat3, shift: [i64; 3]) -> Placement {
        let mut linear = [[0; 3]; 3];
        for (i, row) in linear.iter_mut().enumerate() {
            for (j, v) in row.iter_mut().enumerate() {
                *v = snap(m.rows[i][j]);
            }
        }
        Placement { linear, shift }
    }

    /// The linear part as f64 (exact grid values).
    pub fn linear(&self) -> Mat3 {
        let mut rows = [[0.0; 3]; 3];
        for (i, row) in rows.iter_mut().enumerate() {
            for (j, v) in row.iter_mut().enumerate() {
                *v = self.linear[i][j] as f64 * LINEAR_GRID;
            }
        }
        Mat3::new(rows)
    }

    /// The shift in metres.
    pub fn shift_m(&self) -> Vec3 {
        Vec3::new(self.shift[0] as f64 / 1e9, self.shift[1] as f64 / 1e9, self.shift[2] as f64 / 1e9)
    }

    /// The grid values, for a key: nine linear entries then three shifts.
    pub fn key(&self) -> [i64; 12] {
        let l = &self.linear;
        [l[0][0], l[0][1], l[0][2], l[1][0], l[1][1], l[1][2], l[2][0], l[2][1], l[2][2], self.shift[0], self.shift[1], self.shift[2]]
    }

    /// Whether this is the identity.
    pub fn is_identity(&self) -> bool {
        *self == Placement::IDENTITY
    }

    /// Whether the linear part mirrors (its determinant is negative).
    pub fn mirrors(&self) -> bool {
        self.linear().det() < 0.0
    }

    /// `self` after `inner`: `p -> self(inner(p))`, snapped to the grid. Exact when both
    /// linear parts are signed permutations.
    pub fn then_after(&self, inner: &Placement) -> Placement {
        let a = self.linear();
        let m = a.mul(&inner.linear());
        let t = a.apply(inner.shift_m());
        let shift = [
            snap_nm(t.x).saturating_add(self.shift[0]),
            snap_nm(t.y).saturating_add(self.shift[1]),
            snap_nm(t.z).saturating_add(self.shift[2]),
        ];
        Placement::from_linear(&m, shift)
    }

    /// The inverse: the transpose, and the shift turned back.
    pub fn inverse(&self) -> Placement {
        let lt = self.linear().transpose();
        let t = lt.apply(self.shift_m());
        let mut linear = [[0; 3]; 3];
        for (i, row) in linear.iter_mut().enumerate() {
            for (j, v) in row.iter_mut().enumerate() {
                *v = self.linear[j][i];
            }
        }
        Placement { linear, shift: [-snap_nm(t.x), -snap_nm(t.y), -snap_nm(t.z)] }
    }

    /// Where the point `p` (metres) goes.
    pub fn apply(&self, p: Vec3) -> Vec3 {
        self.linear().apply(p) + self.shift_m()
    }

    /// A box holding the image of `b`.
    pub fn apply_box(&self, b: &Aabb3) -> Aabb3 {
        if b.is_empty() {
            return *b;
        }
        let c = self.apply(b.center());
        let h = self.linear().abs().apply(b.half());
        Aabb3::around(c, h)
    }

    /// The local coordinates of a world point as the four constants of each row: local axis
    /// `i` is `rows[i][0] x + rows[i][1] y + rows[i][2] z + rows[i][3]` (the inverse map).
    pub fn local_rows(&self) -> [[f64; 4]; 3] {
        let inv = self.inverse();
        let m = inv.linear();
        let t = inv.shift_m().to_array();
        let mut rows = [[0.0; 4]; 3];
        for (i, row) in rows.iter_mut().enumerate() {
            *row = [m.rows[i][0], m.rows[i][1], m.rows[i][2], t[i]];
        }
        rows
    }
}
