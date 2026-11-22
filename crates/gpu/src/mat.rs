//! Column-major `#[repr(C)]` matrices (`Mat2`, `Mat3`, `Mat4`), matching Metal (`float2x2`,
//! `float3x3`, `float4x4`) and WGSL (`mat2x2<f32>`, `mat3x3<f32>`, `mat4x4<f32>`).

use crate::math::{abs, cos, sin, tan};
use crate::vec::{Vec2, Vec3, Vec4, vec2, vec3, vec4};
use core::ops::{Add, Mul, Sub};

/// A 2×2 column-major `f32` matrix (`[col0, col1]`).
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Mat2 {
    pub cols: [Vec2; 2],
}

impl Default for Mat2 {
    #[inline]
    fn default() -> Self {
        Self::IDENTITY
    }
}

impl Mat2 {
    pub const ZERO: Self = Self { cols: [vec2(0.0, 0.0), vec2(0.0, 0.0)] };
    pub const IDENTITY: Self = Self { cols: [vec2(1.0, 0.0), vec2(0.0, 1.0)] };

    #[inline]
    pub const fn from_cols(c0: Vec2, c1: Vec2) -> Self {
        Self { cols: [c0, c1] }
    }

    #[inline]
    pub const fn from_diagonal(d: Vec2) -> Self {
        Self { cols: [vec2(d.x, 0.0), vec2(0.0, d.y)] }
    }

    #[inline]
    pub fn from_angle(radians: f32) -> Self {
        let s = sin(radians);
        let c = cos(radians);
        Self { cols: [vec2(c, s), vec2(-s, c)] }
    }

    #[inline]
    pub const fn transpose(self) -> Self {
        let [a, b] = self.cols;
        Self { cols: [vec2(a.x, b.x), vec2(a.y, b.y)] }
    }

    #[inline]
    pub const fn determinant(self) -> f32 {
        self.cols[0].x * self.cols[1].y - self.cols[1].x * self.cols[0].y
    }

    #[inline]
    pub fn inverse(self) -> Option<Self> {
        let det = self.determinant();
        if abs(det) <= 1e-12 {
            return None;
        }
        let inv = 1.0 / det;
        let [a, b] = self.cols;
        Some(Self {
            cols: [vec2(b.y * inv, -a.y * inv), vec2(-b.x * inv, a.x * inv)],
        })
    }
}

impl Mul<Vec2> for Mat2 {
    type Output = Vec2;
    #[inline]
    fn mul(self, v: Vec2) -> Vec2 {
        self.cols[0] * v.x + self.cols[1] * v.y
    }
}

impl Mul<Mat2> for Mat2 {
    type Output = Mat2;
    #[inline]
    fn mul(self, rhs: Mat2) -> Mat2 {
        Mat2::from_cols(self * rhs.cols[0], self * rhs.cols[1])
    }
}

impl Mul<f32> for Mat2 {
    type Output = Mat2;
    #[inline]
    fn mul(self, s: f32) -> Mat2 {
        Mat2::from_cols(self.cols[0] * s, self.cols[1] * s)
    }
}

impl Add for Mat2 {
    type Output = Mat2;
    #[inline]
    fn add(self, rhs: Mat2) -> Mat2 {
        Mat2::from_cols(self.cols[0] + rhs.cols[0], self.cols[1] + rhs.cols[1])
    }
}

impl Sub for Mat2 {
    type Output = Mat2;
    #[inline]
    fn sub(self, rhs: Mat2) -> Mat2 {
        Mat2::from_cols(self.cols[0] - rhs.cols[0], self.cols[1] - rhs.cols[1])
    }
}

/// A 3×3 column-major `f32` matrix (`[col0, col1, col2]`).
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Mat3 {
    pub cols: [Vec3; 3],
}

impl Default for Mat3 {
    #[inline]
    fn default() -> Self {
        Self::IDENTITY
    }
}

impl Mat3 {
    pub const ZERO: Self = Self { cols: [vec3(0.0, 0.0, 0.0); 3] };
    pub const IDENTITY: Self = Self {
        cols: [vec3(1.0, 0.0, 0.0), vec3(0.0, 1.0, 0.0), vec3(0.0, 0.0, 1.0)],
    };

    #[inline]
    pub const fn from_cols(c0: Vec3, c1: Vec3, c2: Vec3) -> Self {
        Self { cols: [c0, c1, c2] }
    }

    #[inline]
    pub const fn from_diagonal(d: Vec3) -> Self {
        Self {
            cols: [vec3(d.x, 0.0, 0.0), vec3(0.0, d.y, 0.0), vec3(0.0, 0.0, d.z)],
        }
    }

    #[inline]
    pub const fn transpose(self) -> Self {
        let [a, b, c] = self.cols;
        Self {
            cols: [vec3(a.x, b.x, c.x), vec3(a.y, b.y, c.y), vec3(a.z, b.z, c.z)],
        }
    }

    #[inline]
    pub fn determinant(self) -> f32 {
        self.cols[0].dot(self.cols[1].cross(self.cols[2]))
    }

    #[inline]
    pub fn inverse(self) -> Option<Self> {
        let [c0, c1, c2] = self.cols;
        let r0 = c1.cross(c2);
        let r1 = c2.cross(c0);
        let r2 = c0.cross(c1);
        let det = c0.dot(r0);
        if abs(det) <= 1e-12 {
            return None;
        }
        let inv = 1.0 / det;
        Some(Self::from_cols(r0 * inv, r1 * inv, r2 * inv).transpose())
    }
}

impl Mul<Vec3> for Mat3 {
    type Output = Vec3;
    #[inline]
    fn mul(self, v: Vec3) -> Vec3 {
        self.cols[0] * v.x + self.cols[1] * v.y + self.cols[2] * v.z
    }
}

impl Mul<Mat3> for Mat3 {
    type Output = Mat3;
    #[inline]
    fn mul(self, rhs: Mat3) -> Mat3 {
        Mat3::from_cols(self * rhs.cols[0], self * rhs.cols[1], self * rhs.cols[2])
    }
}

impl Mul<f32> for Mat3 {
    type Output = Mat3;
    #[inline]
    fn mul(self, s: f32) -> Mat3 {
        Mat3::from_cols(self.cols[0] * s, self.cols[1] * s, self.cols[2] * s)
    }
}

impl Add for Mat3 {
    type Output = Mat3;
    #[inline]
    fn add(self, rhs: Mat3) -> Mat3 {
        Mat3::from_cols(self.cols[0] + rhs.cols[0], self.cols[1] + rhs.cols[1], self.cols[2] + rhs.cols[2])
    }
}

impl Sub for Mat3 {
    type Output = Mat3;
    #[inline]
    fn sub(self, rhs: Mat3) -> Mat3 {
        Mat3::from_cols(self.cols[0] - rhs.cols[0], self.cols[1] - rhs.cols[1], self.cols[2] - rhs.cols[2])
    }
}

/// A 4×4 column-major `f32` matrix (`[col0, col1, col2, col3]`, 64 bytes).
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Mat4 {
    pub cols: [Vec4; 4],
}

impl Default for Mat4 {
    #[inline]
    fn default() -> Self {
        Self::IDENTITY
    }
}

impl Mat4 {
    pub const ZERO: Self = Self { cols: [vec4(0.0, 0.0, 0.0, 0.0); 4] };
    pub const IDENTITY: Self = Self {
        cols: [
            vec4(1.0, 0.0, 0.0, 0.0),
            vec4(0.0, 1.0, 0.0, 0.0),
            vec4(0.0, 0.0, 1.0, 0.0),
            vec4(0.0, 0.0, 0.0, 1.0),
        ],
    };

    #[inline]
    pub const fn from_cols(c0: Vec4, c1: Vec4, c2: Vec4, c3: Vec4) -> Self {
        Self { cols: [c0, c1, c2, c3] }
    }

    #[inline]
    pub const fn from_diagonal(d: Vec4) -> Self {
        Self {
            cols: [
                vec4(d.x, 0.0, 0.0, 0.0),
                vec4(0.0, d.y, 0.0, 0.0),
                vec4(0.0, 0.0, d.z, 0.0),
                vec4(0.0, 0.0, 0.0, d.w),
            ],
        }
    }

    #[inline]
    pub const fn from_translation(t: Vec3) -> Self {
        Self {
            cols: [
                vec4(1.0, 0.0, 0.0, 0.0),
                vec4(0.0, 1.0, 0.0, 0.0),
                vec4(0.0, 0.0, 1.0, 0.0),
                vec4(t.x, t.y, t.z, 1.0),
            ],
        }
    }

    #[inline]
    pub const fn from_scale(s: Vec3) -> Self {
        Self::from_diagonal(vec4(s.x, s.y, s.z, 1.0))
    }

    /// Right-handed perspective projection mapping `[near, far]` to Metal/WGSL clip depth `[0, 1]`.
    #[inline]
    pub fn perspective_rh(fov_y_radians: f32, aspect: f32, near: f32, far: f32) -> Self {
        let f = 1.0 / tan(fov_y_radians * 0.5);
        let r = far / (near - far);
        Self {
            cols: [
                vec4(f / aspect, 0.0, 0.0, 0.0),
                vec4(0.0, f, 0.0, 0.0),
                vec4(0.0, 0.0, r, -1.0),
                vec4(0.0, 0.0, r * near, 0.0),
            ],
        }
    }

    /// Right-handed orthographic projection mapping `[near, far]` to `[0, 1]`.
    #[inline]
    pub fn orthographic_rh(left: f32, right: f32, bottom: f32, top: f32, near: f32, far: f32) -> Self {
        let rcp_w = 1.0 / (right - left);
        let rcp_h = 1.0 / (top - bottom);
        let rcp_d = 1.0 / (near - far);
        Self {
            cols: [
                vec4(2.0 * rcp_w, 0.0, 0.0, 0.0),
                vec4(0.0, 2.0 * rcp_h, 0.0, 0.0),
                vec4(0.0, 0.0, rcp_d, 0.0),
                vec4(-(right + left) * rcp_w, -(top + bottom) * rcp_h, near * rcp_d, 1.0),
            ],
        }
    }

    /// Right-handed view matrix looking from `eye` toward `target` with up direction `up`.
    #[inline]
    pub fn look_at_rh(eye: Vec3, target: Vec3, up: Vec3) -> Self {
        let f = (target - eye).normalize();
        let s = f.cross(up).normalize();
        let u = s.cross(f);
        Self {
            cols: [
                vec4(s.x, u.x, -f.x, 0.0),
                vec4(s.y, u.y, -f.y, 0.0),
                vec4(s.z, u.z, -f.z, 0.0),
                vec4(-s.dot(eye), -u.dot(eye), f.dot(eye), 1.0),
            ],
        }
    }

    #[inline]
    pub const fn transpose(self) -> Self {
        let [a, b, c, d] = self.cols;
        Self {
            cols: [
                vec4(a.x, b.x, c.x, d.x),
                vec4(a.y, b.y, c.y, d.y),
                vec4(a.z, b.z, c.z, d.z),
                vec4(a.w, b.w, c.w, d.w),
            ],
        }
    }

    #[inline]
    pub fn determinant(self) -> f32 {
        let [c0, c1, c2, c3] = self.cols;
        let a = c0.xyz();
        let b = c1.xyz();
        let c = c2.xyz();
        let d = c3.xyz();
        let x = c0.w;
        let y = c1.w;
        let z = c2.w;
        let w = c3.w;
        let s = a.cross(b);
        let t = c.cross(d);
        let u = a * y - b * x;
        let v = c * w - d * z;
        s.dot(v) + t.dot(u)
    }

    /// General 4×4 matrix inverse via 2×2 block cofactor decomposition (Eric Lengyel, FGED1).
    #[inline]
    pub fn inverse(self) -> Option<Self> {
        let [c0, c1, c2, c3] = self.cols;
        let a = c0.xyz();
        let b = c1.xyz();
        let c = c2.xyz();
        let d = c3.xyz();
        let x = c0.w;
        let y = c1.w;
        let z = c2.w;
        let w = c3.w;
        let s = a.cross(b);
        let t = c.cross(d);
        let u = a * y - b * x;
        let v = c * w - d * z;
        let det = s.dot(v) + t.dot(u);
        if abs(det) <= 1e-12 {
            return None;
        }
        let inv = 1.0 / det;
        let s = s * inv;
        let t = t * inv;
        let u = u * inv;
        let v = v * inv;
        let r0 = b.cross(v) + t * y;
        let r1 = v.cross(a) - t * x;
        let r2 = d.cross(u) + s * w;
        let r3 = u.cross(c) - s * z;
        Some(Self::from_cols(
            r0.extend(-b.dot(t)),
            r1.extend(a.dot(t)),
            r2.extend(-d.dot(s)),
            r3.extend(c.dot(s)),
        ).transpose())
    }
}

impl Mul<Vec4> for Mat4 {
    type Output = Vec4;
    #[inline]
    fn mul(self, v: Vec4) -> Vec4 {
        self.cols[0] * v.x + self.cols[1] * v.y + self.cols[2] * v.z + self.cols[3] * v.w
    }
}

impl Mul<Mat4> for Mat4 {
    type Output = Mat4;
    #[inline]
    fn mul(self, rhs: Mat4) -> Mat4 {
        Mat4::from_cols(
            self * rhs.cols[0],
            self * rhs.cols[1],
            self * rhs.cols[2],
            self * rhs.cols[3],
        )
    }
}

impl Mul<f32> for Mat4 {
    type Output = Mat4;
    #[inline]
    fn mul(self, s: f32) -> Mat4 {
        Mat4::from_cols(self.cols[0] * s, self.cols[1] * s, self.cols[2] * s, self.cols[3] * s)
    }
}

impl Add for Mat4 {
    type Output = Mat4;
    #[inline]
    fn add(self, rhs: Mat4) -> Mat4 {
        Mat4::from_cols(
            self.cols[0] + rhs.cols[0],
            self.cols[1] + rhs.cols[1],
            self.cols[2] + rhs.cols[2],
            self.cols[3] + rhs.cols[3],
        )
    }
}

impl Sub for Mat4 {
    type Output = Mat4;
    #[inline]
    fn sub(self, rhs: Mat4) -> Mat4 {
        Mat4::from_cols(
            self.cols[0] - rhs.cols[0],
            self.cols[1] - rhs.cols[1],
            self.cols[2] - rhs.cols[2],
            self.cols[3] - rhs.cols[3],
        )
    }
}
