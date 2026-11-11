//! Inline ray queries (docs/future/gpu/raytracing.md, "As built first"): a kernel traces a ray
//! through a top-level acceleration structure and gets the closest hit, or asks whether anything
//! is in the way. Both are opaque queries: every triangle counts, no candidate is shown to the
//! kernel, so the whole query is one call and no query object is ever a Rust value.
//!
//! The device backend lowers [`Tlas`]'s one query by its path (blaze-shader's intrinsics table) to
//! a naga ray query: initialized, proceeded until it says no more, its committed intersection
//! read. Only the device has a [`Tlas`], like a texture, so the host body panics naming itself.
use crate::vec::{Vec2, Vec3};

/// A top-level acceleration structure (Slang's `RaytracingAccelerationStructure`): what the host
/// built from its instances, bound in the buffer table (wgpu's Metal backend counts an
/// acceleration structure with a bind group's buffers, in entry order).
pub struct Tlas {
    _device_only: (),
}

/// A ray: from `origin` along `dir`, between `tmin` and `tmax` of `dir`'s length.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Ray {
    pub origin: Vec3,
    pub dir: Vec3,
    pub tmin: f32,
    pub tmax: f32,
}

/// The closest triangle a ray met.
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
    /// The triangle's index in its geometry (`CommittedPrimitiveIndex`).
    pub primitive_index: u32,
    /// The hit's barycentrics on the triangle: the weights of its second and third vertices.
    pub barycentrics: Vec2,
    /// Whether the ray met the triangle's front.
    pub front_face: bool,
}

/// Every intersection is opaque: no candidate is shown, the closest is committed.
const FORCE_OPAQUE: u32 = 0x1;
/// The first hit ends the search: enough to know something is in the way.
const ACCEPT_FIRST_HIT_AND_END_SEARCH: u32 = 0x4;
/// naga's committed intersection kind of a triangle (`RayQueryIntersection::Triangle`).
const TRIANGLE: u32 = 1;

/// A query's committed intersection, as the device backend lowers it: each field from naga's
/// `RayIntersection` member of its name (`instance_custom_index` from `instance_custom_data`).
/// `kind` is 0 for nothing, 1 for a triangle.
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
    /// The closest triangle `ray` meets among the instances whose mask shares a bit with `mask`
    /// (the low 8 bits count), or `None`: Slang's `RayQuery<RAY_FLAG_FORCE_OPAQUE>` traced and
    /// proceeded to its end.
    #[inline]
    pub fn trace(&self, ray: Ray, mask: u32) -> Option<Hit> {
        let c = self.query(ray, FORCE_OPAQUE, mask);
        if c.kind == TRIANGLE {
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
        self.query(ray, FORCE_OPAQUE | ACCEPT_FIRST_HIT_AND_END_SEARCH, mask).kind != 0
    }

    /// The one query the device backend lowers: initialized with `flags` and `mask`, proceeded to
    /// its end, its committed intersection read.
    #[inline(never)]
    fn query(&self, ray: Ray, flags: u32, mask: u32) -> Committed {
        let _ = (ray, flags, mask);
        crate::intrinsic::device_only("Tlas::query")
    }
}

impl crate::entry::Param for &Tlas {
    const KIND: crate::entry::Kind = crate::entry::Kind::Tlas;
}
