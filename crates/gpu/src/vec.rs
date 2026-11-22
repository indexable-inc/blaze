//! Vectors: `#[repr(C)]` structs of `f32` (or `u32`, `i32`) whose layout is the components in
//! order, so a `Vec4` in a uniform is Metal's `float4` byte for byte. The device backend lowers
//! each to a vector by its path, so arithmetic on one is one instruction there; here, on the host,
//! it is the plain `#![no_std]` Rust below.
//!
//! The swizzles the shaders read are methods (`.xy()`, `.zw()`, `.xyz()`); a swizzle written to
//! is a struct literal, as in WGSL. A conversion between float and integer vectors is a method
//! named for what it makes (`.as_ivec2()`, `.as_vec2()`), Slang's `int2(v)` and `float2(v)`.

use core::ops::{
    Add, AddAssign, BitAnd, BitAndAssign, BitOr, BitOrAssign, BitXor, BitXorAssign, Div, DivAssign, Mul, MulAssign,
    Neg, Shl, ShlAssign, Shr, ShrAssign, Sub, SubAssign,
};

/// Component-wise binary arithmetic operators with `Self` and with a scalar on either side.
macro_rules! ops {
    ($v:ident, $s:ty, $($f:ident),+) => {
        ops!(@one $v, $s, Add, add, AddAssign, add_assign, +, $($f),+);
        ops!(@one $v, $s, Sub, sub, SubAssign, sub_assign, -, $($f),+);
        ops!(@one $v, $s, Mul, mul, MulAssign, mul_assign, *, $($f),+);
        ops!(@one $v, $s, Div, div, DivAssign, div_assign, /, $($f),+);
    };
    (@one $v:ident, $s:ty, $tr:ident, $m:ident, $tra:ident, $ma:ident, $op:tt, $($f:ident),+) => {
        impl $tr for $v {
            type Output = $v;
            #[inline]
            fn $m(self, o: $v) -> $v {
                $v { $($f: self.$f $op o.$f),+ }
            }
        }
        impl $tr<$s> for $v {
            type Output = $v;
            #[inline]
            fn $m(self, o: $s) -> $v {
                $v { $($f: self.$f $op o),+ }
            }
        }
        impl $tr<$v> for $s {
            type Output = $v;
            #[inline]
            fn $m(self, o: $v) -> $v {
                $v { $($f: self $op o.$f),+ }
            }
        }
        impl $tra for $v {
            #[inline]
            fn $ma(&mut self, o: $v) {
                *self = *self $op o;
            }
        }
        impl $tra<$s> for $v {
            #[inline]
            fn $ma(&mut self, o: $s) {
                *self = *self $op o;
            }
        }
    };
}

/// Component-wise bitwise operators (`&`, `|`, `^`, `<<`, `>>`) for integer vectors.
macro_rules! int_bit_ops {
    ($v:ident, $s:ty, $($f:ident),+) => {
        int_bit_ops!(@one $v, $s, BitAnd, bitand, BitAndAssign, bitand_assign, &, $($f),+);
        int_bit_ops!(@one $v, $s, BitOr, bitor, BitOrAssign, bitor_assign, |, $($f),+);
        int_bit_ops!(@one $v, $s, BitXor, bitxor, BitXorAssign, bitxor_assign, ^, $($f),+);
        int_bit_ops!(@shift $v, $s, Shl, shl, ShlAssign, shl_assign, <<, $($f),+);
        int_bit_ops!(@shift $v, $s, Shr, shr, ShrAssign, shr_assign, >>, $($f),+);
    };
    (@one $v:ident, $s:ty, $tr:ident, $m:ident, $tra:ident, $ma:ident, $op:tt, $($f:ident),+) => {
        impl $tr for $v {
            type Output = $v;
            #[inline]
            fn $m(self, o: $v) -> $v {
                $v { $($f: self.$f $op o.$f),+ }
            }
        }
        impl $tr<$s> for $v {
            type Output = $v;
            #[inline]
            fn $m(self, o: $s) -> $v {
                $v { $($f: self.$f $op o),+ }
            }
        }
        impl $tra for $v {
            #[inline]
            fn $ma(&mut self, o: $v) {
                *self = *self $op o;
            }
        }
        impl $tra<$s> for $v {
            #[inline]
            fn $ma(&mut self, o: $s) {
                *self = *self $op o;
            }
        }
    };
    (@shift $v:ident, $s:ty, $tr:ident, $m:ident, $tra:ident, $ma:ident, $op:tt, $($f:ident),+) => {
        impl $tr for $v {
            type Output = $v;
            #[inline]
            fn $m(self, o: $v) -> $v {
                $v { $($f: self.$f $op o.$f),+ }
            }
        }
        impl $tr<u32> for $v {
            type Output = $v;
            #[inline]
            fn $m(self, o: u32) -> $v {
                $v { $($f: self.$f $op o),+ }
            }
        }
        impl $tra for $v {
            #[inline]
            fn $ma(&mut self, o: $v) {
                *self = *self $op o;
            }
        }
        impl $tra<u32> for $v {
            #[inline]
            fn $ma(&mut self, o: u32) {
                *self = *self $op o;
            }
        }
    };
}

macro_rules! signed_int_vector {
    ($v:ident, $($f:ident),+) => {
        impl Neg for $v {
            type Output = $v;
            #[inline]
            fn neg(self) -> $v {
                $v { $($f: -self.$f),+ }
            }
        }
    };
}

macro_rules! vector {
    ($(#[$doc:meta])* $v:ident, $ctor:ident, $s:ty, $($f:ident),+) => {
        $(#[$doc])*
        #[repr(C)]
        #[derive(Copy, Clone, Debug, Default, PartialEq)]
        pub struct $v {
            $(pub $f: $s),+
        }

        /// The vector of these components (Slang's `float4(..)` and the rest).
        #[inline]
        pub const fn $ctor($($f: $s),+) -> $v {
            $v { $($f),+ }
        }

        impl $v {
            /// Every component `s`.
            #[inline]
            pub const fn splat(s: $s) -> $v {
                $v { $($f: s),+ }
            }

            /// Component-wise minimum.
            #[inline]
            pub fn min(self, o: $v) -> $v {
                $v { $($f: if o.$f < self.$f { o.$f } else { self.$f }),+ }
            }

            /// Component-wise maximum.
            #[inline]
            pub fn max(self, o: $v) -> $v {
                $v { $($f: if o.$f > self.$f { o.$f } else { self.$f }),+ }
            }

            /// Each component clamped between `lo`'s and `hi`'s: `max(lo)` then `min(hi)`, as Slang's
            /// `clamp`, so `hi` wins where `hi < lo`.
            #[inline]
            pub fn clamp(self, lo: $v, hi: $v) -> $v {
                self.max(lo).min(hi)
            }
        }

        ops!($v, $s, $($f),+);
    };
}

macro_rules! float_vector {
    ($v:ident, $f0:ident $(, $f:ident)+) => {
        float_vector!(@ $v, $f0, $f0 $(, $f)+; $($f),+);
    };
    (@ $v:ident, $f0:ident, $($f:ident),+; $($rest:ident),+) => {
        impl $v {
            /// The dot product.
            #[inline]
            pub fn dot(self, o: $v) -> f32 {
                self.$f0 * o.$f0 $(+ self.$rest * o.$rest)+
            }

            /// Its length.
            #[inline]
            pub fn length(self) -> f32 {
                crate::math::sqrt(self.dot(self))
            }

            /// Distance to `o`.
            #[inline]
            pub fn distance(self, o: $v) -> f32 {
                (self - o).length()
            }

            /// It scaled to length 1 (Slang's `normalize`; NaN for the zero vector, as there).
            #[inline]
            pub fn normalize(self) -> $v {
                self * (1.0 / self.length())
            }

            /// Component-wise absolute value.
            #[inline]
            pub const fn abs(self) -> $v {
                $v { $($f: crate::math::abs(self.$f)),+ }
            }

            /// Component-wise floor.
            #[inline]
            pub fn floor(self) -> $v {
                $v { $($f: crate::math::floor(self.$f)),+ }
            }

            /// Component-wise ceiling.
            #[inline]
            pub fn ceil(self) -> $v {
                $v { $($f: crate::math::ceil(self.$f)),+ }
            }

            /// Component-wise round (half away from zero).
            #[inline]
            pub fn round(self) -> $v {
                $v { $($f: crate::math::round(self.$f)),+ }
            }

            /// Component-wise truncation toward zero.
            #[inline]
            pub fn trunc(self) -> $v {
                $v { $($f: crate::math::trunc(self.$f)),+ }
            }

            /// Component-wise fractional part (`x - floor(x)`).
            #[inline]
            pub fn fract(self) -> $v {
                $v { $($f: crate::math::fract(self.$f)),+ }
            }

            /// Component-wise sign: -1, 0 or 1 (Slang's `sign`; 0 for a zero or NaN component, unlike
            /// `f32::signum`).
            #[inline]
            pub fn sign(self) -> $v {
                $v { $($f: crate::math::sign(self.$f)),+ }
            }

            /// Each component clamped to 0 to 1.
            #[inline]
            pub fn saturate(self) -> $v {
                self.clamp($v::splat(0.0), $v::splat(1.0))
            }
        }

        impl Neg for $v {
            type Output = $v;
            #[inline]
            fn neg(self) -> $v {
                $v { $($f: -self.$f),+ }
            }
        }
    };
}

vector!(
    /// Slang's `float2`.
    Vec2, vec2, f32, x, y
);
vector!(
    /// Slang's `float3`. 12 bytes, so never a field of a uniform: Metal's `float3` there is 16.
    Vec3, vec3, f32, x, y, z
);
vector!(
    /// Slang's `float4`.
    Vec4, vec4, f32, x, y, z, w
);
vector!(
    /// Slang's `uint2`.
    UVec2, uvec2, u32, x, y
);
vector!(
    /// Slang's `uint3`. 12 bytes, so never a field of a uniform, like `Vec3`.
    UVec3, uvec3, u32, x, y, z
);
vector!(
    /// Slang's `uint4`.
    UVec4, uvec4, u32, x, y, z, w
);
vector!(
    /// Slang's `int2`.
    IVec2, ivec2, i32, x, y
);
vector!(
    /// Slang's `int3`. 12 bytes, so never a field of a uniform, like `Vec3`.
    IVec3, ivec3, i32, x, y, z
);
vector!(
    /// Slang's `int4`.
    IVec4, ivec4, i32, x, y, z, w
);

impl Eq for UVec2 {}
impl Eq for UVec3 {}
impl Eq for UVec4 {}
impl Eq for IVec2 {}
impl Eq for IVec3 {}
impl Eq for IVec4 {}

float_vector!(Vec2, x, y);
float_vector!(Vec3, x, y, z);
float_vector!(Vec4, x, y, z, w);

int_bit_ops!(UVec2, u32, x, y);
int_bit_ops!(UVec3, u32, x, y, z);
int_bit_ops!(UVec4, u32, x, y, z, w);
int_bit_ops!(IVec2, i32, x, y);
int_bit_ops!(IVec3, i32, x, y, z);
int_bit_ops!(IVec4, i32, x, y, z, w);

signed_int_vector!(IVec2, x, y);
signed_int_vector!(IVec3, x, y, z);
signed_int_vector!(IVec4, x, y, z, w);

impl Vec2 {
    /// It with a third component (Slang's `float3(v.xy, z)`); `.extend(z).extend(w)` is
    /// `float4(v.xy, z, w)`.
    #[inline]
    pub const fn extend(self, z: f32) -> Vec3 {
        vec3(self.x, self.y, z)
    }

    /// Each component converted to `i32`, toward zero (Slang's `int2(v)`). Out of range, Rust's `as`
    /// saturates and NaN is 0, where a GPU leaves it undefined.
    #[inline]
    pub fn as_ivec2(self) -> IVec2 {
        ivec2(self.x as i32, self.y as i32)
    }

    /// Each component converted to `u32`, toward zero.
    #[inline]
    pub fn as_uvec2(self) -> UVec2 {
        uvec2(self.x as u32, self.y as u32)
    }
}

impl IVec2 {
    /// Each component converted to `f32` (Slang's `float2(v)`).
    #[inline]
    pub fn as_vec2(self) -> Vec2 {
        vec2(self.x as f32, self.y as f32)
    }

    #[inline]
    pub const fn extend(self, z: i32) -> IVec3 {
        ivec3(self.x, self.y, z)
    }

    #[inline]
    pub const fn yx(self) -> IVec2 {
        ivec2(self.y, self.x)
    }
}

impl IVec3 {
    #[inline]
    pub const fn xy(self) -> IVec2 {
        ivec2(self.x, self.y)
    }

    #[inline]
    pub const fn extend(self, w: i32) -> IVec4 {
        ivec4(self.x, self.y, self.z, w)
    }

    #[inline]
    pub fn as_vec3(self) -> Vec3 {
        vec3(self.x as f32, self.y as f32, self.z as f32)
    }
}

impl UVec2 {
    /// Each component converted to `f32` (Slang's `float2(v)`).
    #[inline]
    pub fn as_vec2(self) -> Vec2 {
        vec2(self.x as f32, self.y as f32)
    }

    #[inline]
    pub const fn extend(self, z: u32) -> UVec3 {
        uvec3(self.x, self.y, z)
    }
}

impl UVec3 {
    #[inline]
    pub const fn xy(self) -> UVec2 {
        uvec2(self.x, self.y)
    }

    #[inline]
    pub const fn extend(self, w: u32) -> UVec4 {
        uvec4(self.x, self.y, self.z, w)
    }

    #[inline]
    pub fn as_vec3(self) -> Vec3 {
        vec3(self.x as f32, self.y as f32, self.z as f32)
    }
}

impl UVec4 {
    #[inline]
    pub const fn xy(self) -> UVec2 {
        uvec2(self.x, self.y)
    }

    #[inline]
    pub const fn zw(self) -> UVec2 {
        uvec2(self.z, self.w)
    }
}

impl IVec4 {
    #[inline]
    pub const fn xy(self) -> IVec2 {
        ivec2(self.x, self.y)
    }

    #[inline]
    pub const fn zw(self) -> IVec2 {
        ivec2(self.z, self.w)
    }

    #[inline]
    pub const fn xyz(self) -> IVec3 {
        ivec3(self.x, self.y, self.z)
    }
}

impl Vec3 {
    /// Cross product `self × o`.
    #[inline]
    pub const fn cross(self, o: Vec3) -> Vec3 {
        vec3(
            self.y * o.z - self.z * o.y,
            self.z * o.x - self.x * o.z,
            self.x * o.y - self.y * o.x,
        )
    }

    /// It with a fourth component (Slang's `float4(v.xyz, w)`).
    #[inline]
    pub const fn extend(self, w: f32) -> Vec4 {
        vec4(self.x, self.y, self.z, w)
    }

    #[inline]
    pub const fn xy(self) -> Vec2 {
        vec2(self.x, self.y)
    }

    #[inline]
    pub fn as_ivec3(self) -> IVec3 {
        ivec3(self.x as i32, self.y as i32, self.z as i32)
    }
}

impl Vec4 {
    #[inline]
    pub const fn xy(self) -> Vec2 {
        vec2(self.x, self.y)
    }

    #[inline]
    pub const fn zw(self) -> Vec2 {
        vec2(self.z, self.w)
    }

    #[inline]
    pub const fn xyz(self) -> Vec3 {
        vec3(self.x, self.y, self.z)
    }

    /// `.xyz()` under the name a colour reads it by.
    #[inline]
    pub const fn rgb(self) -> Vec3 {
        self.xyz()
    }
}
