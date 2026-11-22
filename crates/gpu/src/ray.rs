//! Inline ray queries (docs/future/gpu/raytracing.md): both one-shot opaque queries
//! ([`Tlas::trace`], [`Tlas::occluded`]) and full iterative inline [`RayQuery`] supporting
//! non-opaque triangle alpha testing, procedural AABB intersections, face culling, and
//! early termination.
//!
//! On the host, [`Tlas::from_triangles`] and [`Tlas::from_geometry`] construct a slice-backed
//! acceleration structure (zero heap allocations) that executes exact Möller–Trumbore
//! ray-triangle and slab ray-AABB intersection on CPU.

use crate::math::abs;
use crate::vec::{Vec2, Vec3, vec2};
use core::marker::PhantomData;
use core::ops::{BitAnd, BitOr, Deref};

/// Ray traversal flags (matching SPIR-V / DXR / Metal / WGSL ray query flags).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct RayFlags(pub u32);

impl RayFlags {
    pub const NONE: Self = Self(0);
    /// Treat every triangle as opaque: auto-commit without surfacing a candidate.
    pub const FORCE_OPAQUE: Self = Self(0x01);
    /// Treat every triangle as non-opaque: surface every hit as a [`Candidate::Triangle`].
    pub const FORCE_NO_OPAQUE: Self = Self(0x02);
    /// Stop traversal as soon as any intersection is committed (shadow rays).
    pub const TERMINATE_ON_FIRST_HIT: Self = Self(0x04);
    /// Ignore intersections with back-facing triangles.
    pub const CULL_BACK_FACING: Self = Self(0x10);
    /// Ignore intersections with front-facing triangles.
    pub const CULL_FRONT_FACING: Self = Self(0x20);

    #[inline]
    pub const fn bits(self) -> u32 {
        self.0
    }

    #[inline]
    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }
}

impl BitOr for RayFlags {
    type Output = Self;
    #[inline]
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl BitAnd for RayFlags {
    type Output = Self;
    #[inline]
    fn bitand(self, rhs: Self) -> Self {
        Self(self.0 & rhs.0)
    }
}

/// A triangle stored in a host-side [`Tlas`].
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Triangle {
    pub v0: Vec3,
    pub v1: Vec3,
    pub v2: Vec3,
    pub instance_index: u32,
    pub instance_custom_index: u32,
    pub geometry_index: u32,
    pub primitive_index: u32,
    /// 8-bit visibility mask tested against the query's mask (`(tri.mask as u32 & mask) != 0`).
    pub mask: u8,
    /// Whether this triangle is opaque by default (overridden by `FORCE_OPAQUE` / `FORCE_NO_OPAQUE`).
    pub opaque: bool,
}

impl Triangle {
    /// Creates an opaque triangle with visibility mask `0xFF` and zero indices.
    #[inline]
    pub const fn new(v0: Vec3, v1: Vec3, v2: Vec3) -> Self {
        Self {
            v0,
            v1,
            v2,
            instance_index: 0,
            instance_custom_index: 0,
            geometry_index: 0,
            primitive_index: 0,
            mask: 0xFF,
            opaque: true,
        }
    }

    #[inline]
    pub const fn with_instance(mut self, instance_index: u32, instance_custom_index: u32) -> Self {
        self.instance_index = instance_index;
        self.instance_custom_index = instance_custom_index;
        self
    }

    #[inline]
    pub const fn with_geometry(mut self, geometry_index: u32, primitive_index: u32) -> Self {
        self.geometry_index = geometry_index;
        self.primitive_index = primitive_index;
        self
    }

    #[inline]
    pub const fn with_mask(mut self, mask: u8) -> Self {
        self.mask = mask;
        self
    }

    #[inline]
    pub const fn with_opaque(mut self, opaque: bool) -> Self {
        self.opaque = opaque;
        self
    }
}

/// An axis-aligned bounding box for procedural ray-query candidates.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct ProceduralAabb {
    pub min: Vec3,
    pub max: Vec3,
    pub instance_index: u32,
    pub instance_custom_index: u32,
    pub geometry_index: u32,
    pub primitive_index: u32,
    pub mask: u8,
}

impl ProceduralAabb {
    #[inline]
    pub const fn new(min: Vec3, max: Vec3) -> Self {
        Self {
            min,
            max,
            instance_index: 0,
            instance_custom_index: 0,
            geometry_index: 0,
            primitive_index: 0,
            mask: 0xFF,
        }
    }

    #[inline]
    pub const fn with_instance(mut self, instance_index: u32, instance_custom_index: u32) -> Self {
        self.instance_index = instance_index;
        self.instance_custom_index = instance_custom_index;
        self
    }

    #[inline]
    pub const fn with_geometry(mut self, geometry_index: u32, primitive_index: u32) -> Self {
        self.geometry_index = geometry_index;
        self.primitive_index = primitive_index;
        self
    }

    #[inline]
    pub const fn with_mask(mut self, mask: u8) -> Self {
        self.mask = mask;
        self
    }
}

/// A top-level acceleration structure (Slang's `RaytracingAccelerationStructure`), bound in the
/// buffer table. On the host, construct one with [`Tlas::from_triangles`] or
/// [`Tlas::from_geometry`].
pub struct Tlas {
    tris_ptr: *const Triangle,
    tris_len: usize,
    aabbs_ptr: *const ProceduralAabb,
    aabbs_len: usize,
}

/// Lifetime-bound host handle owning a slice-backed [`Tlas`].
pub struct HostTlas<'a> {
    inner: Tlas,
    _borrow: PhantomData<(&'a [Triangle], &'a [ProceduralAabb])>,
}

// SAFETY: `HostTlas` holds only immutable borrows of `Sync` slices.
unsafe impl Send for HostTlas<'_> {}
// SAFETY: `HostTlas` is immutable and read-only.
unsafe impl Sync for HostTlas<'_> {}

impl Deref for HostTlas<'_> {
    type Target = Tlas;
    #[inline]
    fn deref(&self) -> &Tlas {
        &self.inner
    }
}

/// A ray: from `origin` along `dir`, between `tmin` and `tmax` of `dir`'s length.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Ray {
    pub origin: Vec3,
    pub dir: Vec3,
    pub tmin: f32,
    pub tmax: f32,
}

/// A triangle (or committed procedural) intersection hit.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Hit {
    /// How far along the ray, in units of its `dir` (Slang's `CommittedRayT`).
    pub t: f32,
    /// The instance's index in the TLAS (`CommittedInstanceIndex`).
    pub instance_index: u32,
    /// The instance's own 24-bit value (`CommittedInstanceID`).
    pub instance_custom_index: u32,
    /// The geometry's index in the instance's BLAS (`CommittedGeometryIndex`).
    pub geometry_index: u32,
    /// The primitive's index in its geometry (`CommittedPrimitiveIndex`).
    pub primitive_index: u32,
    /// The hit's barycentrics on the triangle: the weights of its second and third vertices
    /// (`Vec2::default()` for procedural commits).
    pub barycentrics: Vec2,
    /// Whether the ray met the triangle's front.
    pub front_face: bool,
}

/// A candidate intersection paused for shader inspection during [`RayQuery::proceed`].
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum Candidate {
    /// A non-opaque triangle candidate; call [`RayQuery::commit`] to accept it.
    Triangle(Hit),
    /// A procedural AABB hit; perform custom intersection and call [`RayQuery::commit_at`] if hit.
    Procedural {
        instance_index: u32,
        instance_custom_index: u32,
        geometry_index: u32,
        primitive_index: u32,
        tmin: f32,
        tmax: f32,
    },
}

/// naga's committed intersection kind of a triangle (`RayQueryIntersection::Triangle`).
const TRIANGLE_KIND: u32 = 1;

/// A query's committed intersection, as the device backend lowers it.
#[derive(Copy, Clone, Debug)]
struct Committed {
    kind: u32,
    t: f32,
    instance_custom_index: u32,
    instance_index: u32,
    geometry_index: u32,
    primitive_index: u32,
    barycentrics: Vec2,
    front_face: bool,
}

impl Tlas {
    /// Builds a slice-backed host TLAS from triangles (zero heap allocations).
    #[inline]
    pub fn from_triangles(triangles: &[Triangle]) -> HostTlas<'_> {
        Self::from_geometry(triangles, &[])
    }

    /// Builds a slice-backed host TLAS from triangles and procedural AABBs (zero heap allocations).
    #[inline]
    pub fn from_geometry<'a>(triangles: &'a [Triangle], procedural: &'a [ProceduralAabb]) -> HostTlas<'a> {
        HostTlas {
            inner: Tlas {
                tris_ptr: triangles.as_ptr(),
                tris_len: triangles.len(),
                aabbs_ptr: procedural.as_ptr(),
                aabbs_len: procedural.len(),
            },
            _borrow: PhantomData,
        }
    }

    #[inline]
    pub(crate) fn triangles(&self) -> &[Triangle] {
        // SAFETY: `Tlas` can only be created through `HostTlas<'a>` from a valid `&'a [Triangle]`.
        unsafe { core::slice::from_raw_parts(self.tris_ptr, self.tris_len) }
    }

    #[inline]
    pub(crate) fn procedural_aabbs(&self) -> &[ProceduralAabb] {
        // SAFETY: `Tlas` can only be created through `HostTlas<'a>` from a valid `&'a [ProceduralAabb]`.
        unsafe { core::slice::from_raw_parts(self.aabbs_ptr, self.aabbs_len) }
    }

    /// The closest triangle `ray` meets among the instances whose mask shares a bit with `mask`
    /// (the low 8 bits count), or `None`: Slang's `RayQuery<RAY_FLAG_FORCE_OPAQUE>` traced and
    /// proceeded to its end.
    #[inline]
    pub fn trace(&self, ray: Ray, mask: u32) -> Option<Hit> {
        let c = self.query(ray, RayFlags::FORCE_OPAQUE.bits(), mask);
        if c.kind == TRIANGLE_KIND {
            Some(Hit {
                t: c.t,
                instance_index: c.instance_index,
                instance_custom_index: c.instance_custom_index,
                geometry_index: c.geometry_index,
                primitive_index: c.primitive_index,
                barycentrics: c.barycentrics,
                front_face: c.front_face,
            })
        } else {
            None
        }
    }

    /// Whether `ray` meets any triangle among the instances `mask` selects, the search ended at
    /// the first: a shadow ray (`RAY_FLAG_ACCEPT_FIRST_HIT_AND_END_SEARCH`).
    #[inline]
    pub fn occluded(&self, ray: Ray, mask: u32) -> bool {
        let flags = (RayFlags::FORCE_OPAQUE | RayFlags::TERMINATE_ON_FIRST_HIT).bits();
        self.query(ray, flags, mask).kind != 0
    }

    /// The one query the device backend lowers: initialized with `flags` and `mask`, proceeded to
    /// its end, its committed intersection read. On the host, executes [`RayQuery`] over CPU slices.
    #[inline(never)]
    fn query(&self, ray: Ray, flags: u32, mask: u32) -> Committed {
        let mut q = RayQuery::with_mask(self, ray, RayFlags(flags), mask);
        while q.proceed().is_some() {}
        match q.committed() {
            Some(h) => Committed {
                kind: TRIANGLE_KIND,
                t: h.t,
                instance_custom_index: h.instance_custom_index,
                instance_index: h.instance_index,
                geometry_index: h.geometry_index,
                primitive_index: h.primitive_index,
                barycentrics: h.barycentrics,
                front_face: h.front_face,
            },
            None => Committed {
                kind: 0,
                t: ray.tmax,
                instance_custom_index: 0,
                instance_index: 0,
                geometry_index: 0,
                primitive_index: 0,
                barycentrics: Vec2::default(),
                front_face: false,
            },
        }
    }
}

/// Full inline ray query state machine executing over a [`Tlas`].
pub struct RayQuery<'a> {
    tlas: &'a Tlas,
    ray: Ray,
    flags: RayFlags,
    mask: u32,
    tri_cursor: usize,
    aabb_cursor: usize,
    done: bool,
    pending: Option<Candidate>,
    committed: Option<Hit>,
    current_tmax: f32,
}

impl<'a> RayQuery<'a> {
    /// Starts a new inline ray query with default visibility mask `0xFF`.
    #[inline]
    pub fn new(tlas: &'a Tlas, ray: Ray, flags: RayFlags) -> Self {
        Self::with_mask(tlas, ray, flags, 0xFF)
    }

    /// Starts a new inline ray query with an explicit 8-bit instance mask.
    #[inline]
    pub fn with_mask(tlas: &'a Tlas, ray: Ray, flags: RayFlags, mask: u32) -> Self {
        Self {
            tlas,
            ray,
            flags,
            mask: mask & 0xFF,
            tri_cursor: 0,
            aabb_cursor: 0,
            done: ray.tmin > ray.tmax,
            pending: None,
            committed: None,
            current_tmax: ray.tmax,
        }
    }

    /// Advances traversal until either a non-opaque / procedural [`Candidate`] needs shader
    /// inspection (`Some(candidate)`) or traversal completes (`None`).
    pub fn proceed(&mut self) -> Option<Candidate> {
        self.pending = None;
        if self.done {
            return None;
        }
        let tris = self.tlas.triangles();
        let force_opaque = self.flags.contains(RayFlags::FORCE_OPAQUE);
        let force_no_opaque = self.flags.contains(RayFlags::FORCE_NO_OPAQUE);
        let terminate_first = self.flags.contains(RayFlags::TERMINATE_ON_FIRST_HIT);
        let cull_back = self.flags.contains(RayFlags::CULL_BACK_FACING);
        let cull_front = self.flags.contains(RayFlags::CULL_FRONT_FACING);

        while self.tri_cursor < tris.len() {
            let tri = &tris[self.tri_cursor];
            self.tri_cursor += 1;
            if (u32::from(tri.mask) & self.mask) == 0 {
                continue;
            }
            let Some((t, bary, front_face)) =
                intersect_triangle(self.ray.origin, self.ray.dir, self.ray.tmin, self.current_tmax, tri)
            else {
                continue;
            };
            if (cull_back && !front_face) || (cull_front && front_face) {
                continue;
            }
            let hit = Hit {
                t,
                instance_index: tri.instance_index,
                instance_custom_index: tri.instance_custom_index,
                geometry_index: tri.geometry_index,
                primitive_index: tri.primitive_index,
                barycentrics: bary,
                front_face,
            };
            let is_opaque = force_opaque || (tri.opaque && !force_no_opaque);
            if is_opaque {
                self.current_tmax = t;
                self.committed = Some(hit);
                if terminate_first {
                    self.done = true;
                    return None;
                }
            } else {
                let cand = Candidate::Triangle(hit);
                self.pending = Some(cand);
                return Some(cand);
            }
        }

        let aabbs = self.tlas.procedural_aabbs();
        while self.aabb_cursor < aabbs.len() {
            let aabb = &aabbs[self.aabb_cursor];
            self.aabb_cursor += 1;
            if (u32::from(aabb.mask) & self.mask) == 0 {
                continue;
            }
            if let Some((t_enter, t_exit)) =
                intersect_aabb(self.ray.origin, self.ray.dir, self.ray.tmin, self.current_tmax, aabb)
            {
                let cand = Candidate::Procedural {
                    instance_index: aabb.instance_index,
                    instance_custom_index: aabb.instance_custom_index,
                    geometry_index: aabb.geometry_index,
                    primitive_index: aabb.primitive_index,
                    tmin: t_enter,
                    tmax: t_exit,
                };
                self.pending = Some(cand);
                return Some(cand);
            }
        }

        self.done = true;
        None
    }

    /// Commits the current [`Candidate::Triangle`] candidate returned by [`Self::proceed`].
    pub fn commit(&mut self) {
        if let Some(Candidate::Triangle(hit)) = self.pending
            && hit.t >= self.ray.tmin
            && hit.t <= self.current_tmax
        {
            self.current_tmax = hit.t;
            self.committed = Some(hit);
            if self.flags.contains(RayFlags::TERMINATE_ON_FIRST_HIT) {
                self.done = true;
            }
        }
    }

    /// Commits the current [`Candidate::Procedural`] candidate at ray parameter `t`.
    pub fn commit_at(&mut self, t: f32) {
        if let Some(Candidate::Procedural {
            instance_index,
            instance_custom_index,
            geometry_index,
            primitive_index,
            ..
        }) = self.pending
            && t >= self.ray.tmin
            && t <= self.current_tmax
        {
            self.current_tmax = t;
            self.committed = Some(Hit {
                t,
                instance_index,
                instance_custom_index,
                geometry_index,
                primitive_index,
                barycentrics: Vec2::default(),
                front_face: true,
            });
            if self.flags.contains(RayFlags::TERMINATE_ON_FIRST_HIT) {
                self.done = true;
            }
        }
    }

    /// The closest committed hit so far, if any.
    #[inline]
    pub const fn committed(&self) -> Option<Hit> {
        self.committed
    }
}

/// Möller–Trumbore ray-triangle intersection returning `(t, barycentrics(u, v), front_face)`.
fn intersect_triangle(
    origin: Vec3,
    dir: Vec3,
    tmin: f32,
    tmax: f32,
    tri: &Triangle,
) -> Option<(f32, Vec2, bool)> {
    const EPS: f32 = 1e-8;
    let e1 = tri.v1 - tri.v0;
    let e2 = tri.v2 - tri.v0;
    let pvec = dir.cross(e2);
    let det = e1.dot(pvec);
    if abs(det) < EPS {
        return None;
    }
    let front_face = det > 0.0;
    let inv_det = 1.0 / det;
    let tvec = origin - tri.v0;
    let u = tvec.dot(pvec) * inv_det;
    if !(0.0..=1.0).contains(&u) {
        return None;
    }
    let qvec = tvec.cross(e1);
    let v = dir.dot(qvec) * inv_det;
    if v < 0.0 || u + v > 1.0 {
        return None;
    }
    let t = e2.dot(qvec) * inv_det;
    if t < tmin || t > tmax {
        return None;
    }
    Some((t, vec2(u, v), front_face))
}

/// Slab ray-AABB intersection returning `(t_enter, t_exit)` clamped to `[tmin, tmax]`.
fn intersect_aabb(
    origin: Vec3,
    dir: Vec3,
    mut tmin: f32,
    mut tmax: f32,
    aabb: &ProceduralAabb,
) -> Option<(f32, f32)> {
    let axes = [
        (origin.x, dir.x, aabb.min.x, aabb.max.x),
        (origin.y, dir.y, aabb.min.y, aabb.max.y),
        (origin.z, dir.z, aabb.min.z, aabb.max.z),
    ];
    for (o, d, bmin, bmax) in axes {
        if abs(d) < 1e-12 {
            if o < bmin || o > bmax {
                return None;
            }
        } else {
            let inv = 1.0 / d;
            let mut t0 = (bmin - o) * inv;
            let mut t1 = (bmax - o) * inv;
            if t0 > t1 {
                core::mem::swap(&mut t0, &mut t1);
            }
            if t0 > tmin {
                tmin = t0;
            }
            if t1 < tmax {
                tmax = t1;
            }
            if tmin > tmax {
                return None;
            }
        }
    }
    Some((tmin, tmax))
}

impl crate::entry::Param for &Tlas {
    const KIND: crate::entry::Kind = crate::entry::Kind::Tlas;
}
