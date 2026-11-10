//! The device build end to end on fixtures: a small `gpu` crate with the real one's paths and
//! descriptor types (the reader and the intrinsics table go by path, field and variant name), its
//! descriptors written as `#[gpu::fragment]`, `#[gpu::vertex]` and `gpu::entries!` write them,
//! on a 32-bit target so `usize` is the device's `u32`.
//!
//! Each lowering test checks naga's validator accepts the module (`device_msl` validates before
//! it writes) and pins the MSL; each refusal test checks the construct and the rule named.
use base_db::{Crate, Db, all_crates};
use expect_test::{Expect, expect};
use hir_expand::fixture;
// Links the crate that provides the MIR queries.
use hir_ty_mir as _;

use crate::{
    device::{DeviceBuild, device_msl},
    entries::crate_name,
    refuse::{Error, Refusal, why},
};

/// A 32-bit little-endian data layout: the device target's `usize` is 32 bits.
const LAYOUT: &str = "e-m:e-p:32:32-i64:64-n32-S128";

const MINICORE: &str = "sized, copy, clone, derive, add, mul, unary_ops, ord, eq, option, iterator, deref, index, slice, sync, fn, builtin_impls, unsize, coerce_unsized, transmute";

/// The gpu crate's types the backend reads, at the real crate's paths.
const GPU: &str = r#"
pub mod vec {
    #[derive(Copy, Clone)]
    #[repr(C)]
    pub struct Vec2 { pub x: f32, pub y: f32 }
    #[derive(Copy, Clone)]
    #[repr(C)]
    pub struct Vec4 { pub x: f32, pub y: f32, pub z: f32, pub w: f32 }
    #[derive(Copy, Clone)]
    #[repr(C)]
    pub struct Vec3 { pub x: f32, pub y: f32, pub z: f32 }
    #[derive(Copy, Clone)]
    #[repr(C)]
    pub struct IVec4 { pub x: i32, pub y: i32, pub z: i32, pub w: i32 }
    #[derive(Copy, Clone)]
    #[repr(C)]
    pub struct UVec2 { pub x: u32, pub y: u32 }
    #[derive(Copy, Clone)]
    #[repr(C)]
    pub struct UVec3 { pub x: u32, pub y: u32, pub z: u32 }
    pub const fn vec3(x: f32, y: f32, z: f32) -> Vec3 { Vec3 { x, y, z } }
    pub const fn uvec2(x: u32, y: u32) -> UVec2 { UVec2 { x, y } }
    pub const fn vec2(x: f32, y: f32) -> Vec2 { Vec2 { x, y } }
    pub const fn vec4(x: f32, y: f32, z: f32, w: f32) -> Vec4 { Vec4 { x, y, z, w } }
    impl Vec4 {
        pub const fn xy(self) -> Vec2 { Vec2 { x: self.x, y: self.y } }
    }
}
pub mod math {
    pub fn f32tof16(x: f32) -> u32 { x as u32 }
    pub fn f16tof32(x: u32) -> f32 { x as f32 }
}
pub mod ray {
    use crate::vec::{Vec2, Vec3};
    pub struct Tlas { _device_only: () }
    #[derive(Copy, Clone)]
    pub struct Ray { pub origin: Vec3, pub dir: Vec3, pub tmin: f32, pub tmax: f32 }
    #[derive(Copy, Clone)]
    pub struct Hit {
        pub t: f32, pub instance_index: u32, pub instance_custom_index: u32, pub geometry_index: u32,
        pub primitive_index: u32, pub barycentrics: Vec2, pub front_face: bool,
    }
    #[derive(Copy, Clone)]
    struct Committed {
        kind: u32, t: f32, instance_custom_index: u32, instance_index: u32, geometry_index: u32,
        primitive_index: u32, barycentrics: Vec2, front_face: bool,
    }
    impl Tlas {
        pub fn trace(&self, ray: Ray, mask: u32) -> Option<Hit> {
            let c = self.query(ray, 1, mask);
            if c.kind == 1 {
                Some(Hit { t: c.t, instance_index: c.instance_index, instance_custom_index: c.instance_custom_index,
                    geometry_index: c.geometry_index, primitive_index: c.primitive_index, barycentrics: c.barycentrics,
                    front_face: c.front_face })
            } else {
                None
            }
        }
        pub fn occluded(&self, ray: Ray, mask: u32) -> bool { self.query(ray, 5, mask).kind != 0 }
        fn query(&self, ray: Ray, flags: u32, mask: u32) -> Committed { loop {} }
    }
}
pub mod intrinsic {
    pub fn dpdx<T: Copy>(v: T) -> T { v }
    pub fn discard() -> ! { loop {} }
}
pub mod entry {
    use crate::vec::{Vec2, Vec4};
    use crate::vec::UVec3;
    #[derive(Copy, Clone)] pub enum Stage { Vertex, Fragment, Compute { x: u32, y: u32, z: u32 } }
    #[derive(Copy, Clone)] pub enum ValueTy { F32, Vec2, Vec3, Vec4, U32, UVec4, I32, IVec2 }
    #[derive(Copy, Clone)] pub enum Interp { Perspective, Linear, Flat }
    #[derive(Copy, Clone)] pub enum FieldKind { Position, Value { ty: ValueTy, interp: Interp } }
    #[derive(Copy, Clone)] pub struct Field { pub name: &'static str, pub kind: FieldKind, pub location: Option<u32> }
    #[derive(Copy, Clone)] pub enum Kind { VertexIndex, InstanceIndex, Varyings(&'static [Field]), Uniform { size: u32 }, Storage { stride: u32 }, Texture2d, Sampler, Thread, RawOut { stride: u32 }, Tlas }
    #[derive(Copy, Clone)] pub enum Out { Varyings(&'static [Field]), Color, Nothing }
    #[derive(Copy, Clone)] pub enum Table { None, Buffer, Texture, Sampler }
    #[derive(Copy, Clone)] pub struct Slot { pub table: Table, pub index: u32 }
    #[derive(Copy, Clone)] pub struct ParamDesc { pub name: &'static str, pub kind: Kind, pub slot: Slot }
    #[derive(Copy, Clone)] pub struct EntryDesc { pub name: &'static str, pub stage: Stage, pub params: &'static [ParamDesc], pub out: Out, pub sizes: Option<Slot> }
    #[derive(Copy, Clone)] pub struct VertexIndex(pub u32);
    #[derive(Copy, Clone)] pub struct Position(pub Vec4);
    #[derive(Copy, Clone)] pub struct Flat<T>(pub T);
    impl<T> core::ops::Deref for Flat<T> {
        type Target = T;
        fn deref(&self) -> &T { &self.0 }
    }
    pub struct Texture2d { _device_only: () }
    pub struct Sampler { _device_only: () }
    impl Texture2d {
        pub fn sample_level(&self, s: &Sampler, uv: Vec2, level: f32) -> Vec4 { loop {} }
    }
    pub struct Thread { global: UVec3, local: UVec3, group: UVec3 }
    impl Thread {
        pub fn global(&self) -> UVec3 { self.global }
        pub fn local(&self) -> UVec3 { self.local }
        pub fn group(&self) -> UVec3 { self.group }
    }
    pub struct RawOut<T> { _device_only: (), _of: [T; 0] }
    impl<T> RawOut<T> {
        pub fn len(&self) -> usize { self.count() }
        pub unsafe fn read(&self, i: usize) -> T {
            if i >= self.len() { panic!() }
            self.load(i)
        }
        pub unsafe fn write(&self, i: usize, v: T) {
            if i >= self.len() { panic!() }
            self.store(i, v)
        }
        fn count(&self) -> usize { loop {} }
        fn load(&self, i: usize) -> T { loop {} }
        fn store(&self, i: usize, v: T) { loop {} }
    }
    #[derive(Copy, Clone)] pub struct Code(pub *const ());
    unsafe impl Sync for Code {}
    #[derive(Copy, Clone)] pub struct Entry { pub desc: &'static EntryDesc, pub code: Code }
    #[derive(Copy, Clone)] pub struct Entries(pub &'static [Entry]);
}
"#;

/// A fragment entry `fs(v: V, g: &Globals, data: &[IVec4]) -> Vec4` around `body`, with `items`
/// beside it, its descriptor as the attribute writes it.
fn fragment(items: &str, body: &str) -> String {
    format!(
        r#"
use gpu::entry::*;
use gpu::vec::*;
#[derive(Copy, Clone)]
#[repr(C)]
pub struct V {{ pub pos: Position, pub shade: Flat<f32> }}
#[derive(Copy, Clone)]
#[repr(C)]
pub struct Globals {{ pub tint: Vec4, pub count: u32, pub _p0: u32, pub _p1: u32, pub _p2: u32 }}
{items}
pub fn fs(v: V, g: &Globals, data: &[IVec4]) -> Vec4 {{
{body}
}}
pub mod fs {{
    use gpu::entry::*;
    pub const FIELDS: [Field; 2] = [
        Field {{ name: "pos", kind: FieldKind::Position, location: None }},
        Field {{ name: "shade", kind: FieldKind::Value {{ ty: ValueTy::F32, interp: Interp::Flat }}, location: Some(0) }},
    ];
    pub const PARAMS: [ParamDesc; 3] = [
        ParamDesc {{ name: "v", kind: Kind::Varyings(&FIELDS), slot: Slot {{ table: Table::None, index: 0 }} }},
        ParamDesc {{ name: "g", kind: Kind::Uniform {{ size: 32 }}, slot: Slot {{ table: Table::Buffer, index: 0 }} }},
        ParamDesc {{ name: "data", kind: Kind::Storage {{ stride: 16 }}, slot: Slot {{ table: Table::Buffer, index: 1 }} }},
    ];
    pub const ENTRY: EntryDesc = EntryDesc {{
        name: "fs",
        stage: Stage::Fragment,
        params: &PARAMS,
        out: Out::Color,
        sizes: Some(Slot {{ table: Table::Buffer, index: 2 }}),
    }};
}}
pub static GPU_ENTRIES: Entries = Entries(&[Entry {{ desc: &fs::ENTRY, code: Code(fs as *const ()) }}]);
"#
    )
}

/// A vertex entry `vs(i: VertexIndex) -> VOut` around `body`.
fn vertex(items: &str, body: &str) -> String {
    format!(
        r#"
use gpu::entry::*;
use gpu::vec::*;
#[derive(Copy, Clone)]
#[repr(C)]
pub struct VOut {{ pub pos: Position, pub uv: Vec2 }}
{items}
pub fn vs(i: VertexIndex) -> VOut {{
{body}
}}
pub mod vs {{
    use gpu::entry::*;
    pub const FIELDS: [Field; 2] = [
        Field {{ name: "pos", kind: FieldKind::Position, location: None }},
        Field {{ name: "uv", kind: FieldKind::Value {{ ty: ValueTy::Vec2, interp: Interp::Perspective }}, location: Some(0) }},
    ];
    pub const PARAMS: [ParamDesc; 1] = [
        ParamDesc {{ name: "i", kind: Kind::VertexIndex, slot: Slot {{ table: Table::None, index: 0 }} }},
    ];
    pub const ENTRY: EntryDesc = EntryDesc {{
        name: "vs",
        stage: Stage::Vertex,
        params: &PARAMS,
        out: Out::Varyings(&FIELDS),
        sizes: None,
    }};
}}
pub static GPU_ENTRIES: Entries = Entries(&[Entry {{ desc: &vs::ENTRY, code: Code(vs as *const ()) }}]);
"#
    )
}

/// A kernel `cs(t: Thread, g: &Globals, data: &[IVec4], acc: &RawOut<u32>)`, and `scene: &Tlas`
/// after `g` when `tlas`, around `body`, with workgroups of 8 by 8; its descriptor as
/// `#[gpu::kernel(threads = (8, 8))]` writes it.
fn kernel(items: &str, body: &str, tlas: bool) -> String {
    let (scene, scene_desc, n, after) = if tlas {
        ("scene: &gpu::ray::Tlas, ", "ParamDesc {{ name: \"scene\", kind: Kind::Tlas, slot: Slot {{ table: Table::Buffer, index: 1 }} }},", 5, 2)
    } else {
        ("", "", 4, 1)
    };
    let scene_desc = scene_desc.replace("{{", "{").replace("}}", "}");
    format!(
        r#"
use gpu::entry::*;
use gpu::vec::*;
use gpu::ray::*;
#[derive(Copy, Clone)]
#[repr(C)]
pub struct Globals {{ pub tint: Vec4, pub count: u32, pub _p0: u32, pub _p1: u32, pub _p2: u32 }}
{items}
pub fn cs(t: Thread, g: &Globals, {scene}data: &[IVec4], acc: &RawOut<u32>) {{
{body}
}}
pub mod cs {{
    use gpu::entry::*;
    pub const PARAMS: [ParamDesc; {n}] = [
        ParamDesc {{ name: "t", kind: Kind::Thread, slot: Slot {{ table: Table::None, index: 0 }} }},
        ParamDesc {{ name: "g", kind: Kind::Uniform {{ size: 32 }}, slot: Slot {{ table: Table::Buffer, index: 0 }} }},
        {scene_desc}
        ParamDesc {{ name: "data", kind: Kind::Storage {{ stride: 16 }}, slot: Slot {{ table: Table::Buffer, index: {after} }} }},
        ParamDesc {{ name: "acc", kind: Kind::RawOut {{ stride: 4 }}, slot: Slot {{ table: Table::Buffer, index: {acc} }} }},
    ];
    pub const ENTRY: EntryDesc = EntryDesc {{
        name: "cs",
        stage: Stage::Compute {{ x: 8, y: 8, z: 1 }},
        params: &PARAMS,
        out: Out::Nothing,
        sizes: Some(Slot {{ table: Table::Buffer, index: {sizes} }}),
    }};
}}
pub static GPU_ENTRIES: Entries = Entries(&[Entry {{ desc: &cs::ENTRY, code: Code(cs as *const ()) }}]);
"#,
        acc = after + 1,
        sizes = after + 2,
    )
}

fn db(main: &str) -> Db {
    let text = format!(
        "//- target_data_layout: {LAYOUT}\n//- minicore: {MINICORE}\n//- /gpu.rs crate:gpu\n{GPU}\n//- /lib.rs crate:main deps:gpu cfg:overflow_checks\n{main}"
    );
    fixture::db(&text).into_db()
}

fn main_crate(db: &Db) -> Crate {
    *all_crates(db).iter().find(|krate| crate_name(db, **krate) == "main").expect("the fixture has a crate `main`")
}

fn build(main: &str) -> Result<DeviceBuild, Error> {
    let db = db(main);
    device_msl(&db, main_crate(&db))
}

/// The MSL of `main`, which must lower and validate.
fn check_msl(main: &str, expect: Expect) {
    match build(main) {
        Ok(device) => expect.assert_eq(&device.msl),
        Err(e) => panic!("the device build failed:\n{e}"),
    }
}

/// The refusals of `main`, which must not lower.
fn refusals(main: &str) -> Vec<Refusal> {
    match build(main) {
        Err(Error::Refused(all)) => all,
        Err(e) => panic!("expected refusals, got: {e}"),
        Ok(device) => panic!("expected refusals, got MSL:\n{}", device.msl),
    }
}

/// One refusal of `main` whose construct mentions `construct`, under rule `rule`.
fn refused(main: &str, construct: &str, rule: &str) {
    let all = refusals(main);
    assert!(
        all.iter().any(|r| r.construct.contains(construct) && r.why == rule),
        "no refusal of `{construct}` under the rule; got:\n{}",
        all.iter().map(|r| r.to_string()).collect::<Vec<_>>().join("\n")
    );
    for r in &all {
        let entry = if main.contains("pub fn vs(") {
            "vs"
        } else if main.contains("pub fn cs(") {
            "cs"
        } else {
            "fs"
        };
        assert_eq!(r.entry, entry, "{r}");
    }
}

#[test]
fn fragment_with_varyings_uniform_and_storage() {
    check_msl(
        &fragment(
            "",
            "    let s = *v.shade;
    let d = data[g.count as usize];
    vec4(g.tint.x * s, g.tint.y + d.x as f32, g.tint.z, v.pos.0.w)",
        ),
        expect![[r#"
            // language: metal2.4
            #include <metal_stdlib>
            #include <simd/simd.h>

            using metal::uint;

            struct _mslBufferSizes {
                uint size1;
            };

            struct fs_in {
                metal::float4 pos;
                float shade;
                char _pad2[12];
            };
            struct Globals {
                metal::float4 f0_;
                uint f1_;
                uint f2_;
                uint f3_;
                uint f4_;
            };
            typedef metal::int4 type_4[1];
            struct Position {
                metal::float4 f0_;
            };
            struct Flat {
                float f0_;
            };
            struct V {
                Position f0_;
                Flat f1_;
            };

            struct fsInput {
                float shade [[user(loc0), flat]];
            };
            struct fsOutput {
                metal::float4 member [[color(0)]];
            };
            fragment fsOutput fs(
              fsInput varyings [[stage_in]]
            , metal::float4 pos [[position]]
            , constant Globals& fs_g [[buffer(0)]]
            , device type_4 const& fs_data [[buffer(1)]]
            , constant _mslBufferSizes& _buffer_sizes [[buffer(2)]]
            ) {
                const fs_in v = { pos, varyings.shade };
                V fs_1_ = V {};
                float fs_4_ = float {};
                uint fs_10_ = uint {};
                uint fs_9_ = uint {};
                uint fs_21_ = uint {};
                bool fs_20_ = bool {};
                metal::int4 fs_8_ = metal::int4 {};
                float fs_12_ = float {};
                float fs_13_ = float {};
                float fs_11_ = float {};
                float fs_15_ = float {};
                int fs_17_ = int {};
                float fs_16_ = float {};
                float fs_14_ = float {};
                float fs_18_ = float {};
                float fs_19_ = float {};
                float vec4_1_ = float {};
                float vec4_2_ = float {};
                float vec4_3_ = float {};
                float vec4_4_ = float {};
                float vec4_5_ = float {};
                float vec4_6_ = float {};
                float vec4_7_ = float {};
                float vec4_8_ = float {};
                metal::float4 vec4_0_ = metal::float4 {};
                metal::float4 fs_0_ = metal::float4 {};
                fs_1_ = V {Position {v.pos}, Flat {v.shade}};
                float _e11 = fs_1_.f1_.f0_;
                fs_4_ = _e11;
                uint _e16 = fs_g.f1_;
                fs_10_ = _e16;
                uint _e20 = fs_10_;
                fs_9_ = _e20;
                fs_21_ = 1 + (_buffer_sizes.size1 - 0 - 16) / 16;
                uint _e28 = fs_9_;
                uint _e30 = fs_21_;
                fs_20_ = _e28 < _e30;
                bool _e35 = fs_20_;
                if (_e35) {
                    uint _e39 = fs_9_;
                    metal::int4 _e41 = fs_data[_e39];
                    fs_8_ = _e41;
                    float _e47 = fs_g.f0_.x;
                    fs_12_ = _e47;
                    float _e51 = fs_4_;
                    fs_13_ = _e51;
                    float _e55 = fs_12_;
                    float _e57 = fs_13_;
                    fs_11_ = _e55 * _e57;
                    float _e64 = fs_g.f0_.y;
                    fs_15_ = _e64;
                    int _e69 = fs_8_.x;
                    fs_17_ = _e69;
                    int _e73 = fs_17_;
                    fs_16_ = static_cast<float>(_e73);
                    float _e78 = fs_15_;
                    float _e80 = fs_16_;
                    fs_14_ = _e78 + _e80;
                    float _e87 = fs_g.f0_.z;
                    fs_18_ = _e87;
                    float _e94 = fs_1_.f0_.f0_.w;
                    fs_19_ = _e94;
                    float _e98 = fs_11_;
                    vec4_1_ = _e98;
                    float _e102 = fs_14_;
                    vec4_2_ = _e102;
                    float _e106 = fs_18_;
                    vec4_3_ = _e106;
                    float _e110 = fs_19_;
                    vec4_4_ = _e110;
                    float _e114 = vec4_1_;
                    vec4_5_ = _e114;
                    float _e118 = vec4_2_;
                    vec4_6_ = _e118;
                    float _e122 = vec4_3_;
                    vec4_7_ = _e122;
                    float _e126 = vec4_4_;
                    vec4_8_ = _e126;
                    float _e130 = vec4_5_;
                    float _e132 = vec4_6_;
                    float _e134 = vec4_7_;
                    float _e136 = vec4_8_;
                    vec4_0_ = metal::float4(_e130, _e132, _e134, _e136);
                    metal::float4 _e141 = vec4_0_;
                    fs_0_ = _e141;
                    metal::float4 _e145 = fs_0_;
                    return fsOutput { _e145 };
                } else {
                    metal::discard_fragment();
                    return fsOutput { metal::float4 {} };
                }
                metal::discard_fragment();
                return fsOutput { metal::float4 {} };
            }
        "#]],
    );
}

#[test]
fn vertex_corners_from_a_local_array() {
    check_msl(
        &vertex(
            "",
            "    let corners = [vec2(-1.0, -1.0), vec2(3.0, -1.0), vec2(-1.0, 3.0)];
    let c = corners[i.0 as usize];
    VOut { pos: Position(vec4(c.x, c.y, 0.0, 1.0)), uv: c }",
        ),
        expect![[r#"
            // language: metal2.4
            #include <metal_stdlib>
            #include <simd/simd.h>

            using metal::uint;

            struct vs_out {
                metal::float4 pos;
                metal::float2 uv;
                char _pad2[8];
            };
            struct VertexIndex {
                uint f0_;
            };
            struct type_4 {
                metal::float2 inner[3];
            };
            struct Position {
                metal::float4 f0_;
            };
            struct VOut {
                Position f0_;
                metal::float2 f1_;
            };

            struct vsInput {
            };
            struct vsOutput {
                metal::float4 pos [[position]];
                metal::float2 uv [[user(loc0), center_perspective]];
            };
            vertex vsOutput vs(
              uint i [[vertex_id]]
            ) {
                VertexIndex vs_1_ = VertexIndex {};
                float vs_4_ = float {};
                float vs_5_ = float {};
                float vec2_1_ = float {};
                float vec2_2_ = float {};
                float vec2_3_ = float {};
                float vec2_4_ = float {};
                metal::float2 vec2_0_ = metal::float2 {};
                metal::float2 vs_3_ = metal::float2 {};
                float vs_7_ = float {};
                float vec2_1_1 = float {};
                float vec2_2_1 = float {};
                float vec2_3_1 = float {};
                float vec2_4_1 = float {};
                metal::float2 vec2_0_1 = metal::float2 {};
                metal::float2 vs_6_ = metal::float2 {};
                float vs_9_ = float {};
                float vec2_1_2 = float {};
                float vec2_2_2 = float {};
                float vec2_3_2 = float {};
                float vec2_4_2 = float {};
                metal::float2 vec2_0_2 = metal::float2 {};
                metal::float2 vs_8_ = metal::float2 {};
                type_4 vs_2_ = type_4 {};
                uint vs_12_ = uint {};
                uint vs_11_ = uint {};
                bool vs_18_ = bool {};
                metal::float2 vs_10_ = metal::float2 {};
                float vs_15_ = float {};
                float vs_16_ = float {};
                float vec4_1_ = float {};
                float vec4_2_ = float {};
                float vec4_3_ = float {};
                float vec4_4_ = float {};
                float vec4_5_ = float {};
                float vec4_6_ = float {};
                float vec4_7_ = float {};
                float vec4_8_ = float {};
                metal::float4 vec4_0_ = metal::float4 {};
                metal::float4 vs_14_ = metal::float4 {};
                Position vs_13_ = Position {};
                metal::float2 vs_17_ = metal::float2 {};
                VOut vs_0_ = VOut {};
                vs_1_ = VertexIndex {i};
                vs_4_ = -(1.0);
                vs_5_ = -(1.0);
                float _e13 = vs_4_;
                vec2_1_ = _e13;
                float _e17 = vs_5_;
                vec2_2_ = _e17;
                float _e21 = vec2_1_;
                vec2_3_ = _e21;
                float _e25 = vec2_2_;
                vec2_4_ = _e25;
                float _e29 = vec2_3_;
                float _e31 = vec2_4_;
                vec2_0_ = metal::float2(_e29, _e31);
                metal::float2 _e36 = vec2_0_;
                vs_3_ = _e36;
                vs_7_ = -(1.0);
                vec2_1_1 = 3.0;
                float _e47 = vs_7_;
                vec2_2_1 = _e47;
                float _e51 = vec2_1_1;
                vec2_3_1 = _e51;
                float _e55 = vec2_2_1;
                vec2_4_1 = _e55;
                float _e59 = vec2_3_1;
                float _e61 = vec2_4_1;
                vec2_0_1 = metal::float2(_e59, _e61);
                metal::float2 _e66 = vec2_0_1;
                vs_6_ = _e66;
                vs_9_ = -(1.0);
                float _e74 = vs_9_;
                vec2_1_2 = _e74;
                vec2_2_2 = 3.0;
                float _e81 = vec2_1_2;
                vec2_3_2 = _e81;
                float _e85 = vec2_2_2;
                vec2_4_2 = _e85;
                float _e89 = vec2_3_2;
                float _e91 = vec2_4_2;
                vec2_0_2 = metal::float2(_e89, _e91);
                metal::float2 _e96 = vec2_0_2;
                vs_8_ = _e96;
                metal::float2 _e100 = vs_3_;
                metal::float2 _e102 = vs_6_;
                metal::float2 _e104 = vs_8_;
                vs_2_ = type_4 {{_e100, _e102, _e104}};
                uint _e110 = vs_1_.f0_;
                vs_12_ = _e110;
                uint _e114 = vs_12_;
                vs_11_ = _e114;
                uint _e118 = vs_11_;
                vs_18_ = _e118 < 3u;
                bool _e124 = vs_18_;
                if (_e124) {
                    uint _e134 = vs_11_;
                    metal::float2 _e136 = vs_2_.inner[_e134];
                    vs_10_ = _e136;
                    float _e141 = vs_10_.x;
                    vs_15_ = _e141;
                    float _e146 = vs_10_.y;
                    vs_16_ = _e146;
                    float _e150 = vs_15_;
                    vec4_1_ = _e150;
                    float _e154 = vs_16_;
                    vec4_2_ = _e154;
                    vec4_3_ = 0.0;
                    vec4_4_ = 1.0;
                    float _e164 = vec4_1_;
                    vec4_5_ = _e164;
                    float _e168 = vec4_2_;
                    vec4_6_ = _e168;
                    float _e172 = vec4_3_;
                    vec4_7_ = _e172;
                    float _e176 = vec4_4_;
                    vec4_8_ = _e176;
                    float _e180 = vec4_5_;
                    float _e182 = vec4_6_;
                    float _e184 = vec4_7_;
                    float _e186 = vec4_8_;
                    vec4_0_ = metal::float4(_e180, _e182, _e184, _e186);
                    metal::float4 _e191 = vec4_0_;
                    vs_14_ = _e191;
                    metal::float4 _e195 = vs_14_;
                    vs_13_ = Position {_e195};
                    metal::float2 _e200 = vs_10_;
                    vs_17_ = _e200;
                    Position _e204 = vs_13_;
                    metal::float2 _e206 = vs_17_;
                    vs_0_ = VOut {_e204, _e206};
                    VOut _e211 = vs_0_;
                    const auto _tmp = vs_out {_e211.f0_.f0_, _e211.f1_};
                    return vsOutput { _tmp.pos, _tmp.uv };
                } else {
                    const auto _tmp_1 = vs_out {metal::float4(2.0, 2.0, 2.0, 1.0), metal::float2 {}};
                    return vsOutput { _tmp_1.pos, _tmp_1.uv };
                }
                const auto _tmp_2 = vs_out {metal::float4(2.0, 2.0, 2.0, 1.0), metal::float2 {}};
                return vsOutput { _tmp_2.pos, _tmp_2.uv };
            }
        "#]],
    );
}

/// What harness.slang's port leans on: a `for` over an iterator (`Option` and `Iterator::next`)
/// with `break`, an `&mut` out parameter, a nested function, run-time indices into a local
/// array, checked `i32` division, float-to-int `as`, an early return, a large struct by value.
#[test]
fn harness_constructs() {
    check_msl(
        &fragment(
            "pub struct Upto { pub at: u32, pub end: u32 }
impl Iterator for Upto {
    type Item = u32;
    fn next(&mut self) -> Option<u32> {
        if self.at < self.end { let at = self.at; self.at += 1; Some(at) } else { None }
    }
}
#[derive(Copy, Clone)]
pub struct Big { pub a: [Vec4; 4], pub b: [i32; 8], pub c: f32 }
fn widen(out: &mut Vec2, by: f32) { out.x -= by; out.y += by; }
fn pick(big: Big, k: usize) -> f32 { big.a[k % 4].x + big.b[k % 8] as f32 + big.c }",
            "    fn half(x: f32) -> f32 { x * 0.5 }
    if v.shade.0 < 0.0 {
        return vec4(0.0, 0.0, 0.0, 0.0);
    }
    let mut span = vec2(0.0, 0.0);
    widen(&mut span, half(v.shade.0));
    let mut sum = 0.0;
    for k in (Upto { at: 0, end: g.count }) {
        if k > 6 { break; }
        sum += k as f32;
    }
    let big = Big { a: [g.tint; 4], b: [1, 2, 3, 4, 5, 6, 7, 8], c: sum };
    let n = (v.pos.0.x as i32) / ((g.count as i32) | 1);
    vec4(span.x, span.y, pick(big, n as usize), (n % 3) as f32)",
        ),
        expect![[r#"
            // language: metal2.4
            #include <metal_stdlib>
            #include <simd/simd.h>

            using metal::uint;

            struct _mslBufferSizes {
                uint size1;
            };

            struct fs_in {
                metal::float4 pos;
                float shade;
                char _pad2[12];
            };
            struct Globals {
                metal::float4 f0_;
                uint f1_;
                uint f2_;
                uint f3_;
                uint f4_;
            };
            typedef metal::int4 type_4[1];
            struct Upto {
                uint f0_;
                uint f1_;
            };
            struct Position {
                metal::float4 f0_;
            };
            struct Flat {
                float f0_;
            };
            struct V {
                Position f0_;
                Flat f1_;
            };
            struct type_8 {
                int tag;
                char _pad1[12];
                uint v1_0_;
                char _pad2[12];
            };
            struct type_9 {
                uint f0_;
                bool f1_;
                char _pad2[3];
            };
            struct type_10 {
                metal::float4 inner[4];
            };
            struct type_11 {
                int inner[8];
            };
            struct Big {
                type_10 f0_;
                type_11 f1_;
                float f2_;
            };
            int naga_f2i32(float value) {
                return static_cast<int>(metal::clamp(value, -2147483600.0, 2147483500.0));
            }

            int naga_div(int lhs, int rhs) {
                return lhs / metal::select(rhs, 1, (lhs == (-2147483647 - 1) & rhs == -1) | (rhs == 0));
            }

            uint naga_mod(uint lhs, uint rhs) {
                return lhs % metal::select(rhs, 1u, rhs == 0u);
            }

            int naga_mod(int lhs, int rhs) {
                int divisor = metal::select(rhs, 1, (lhs == (-2147483647 - 1) & rhs == -1) | (rhs == 0));
                return lhs - (lhs / divisor) * divisor;
            }


            struct fsInput {
                float shade [[user(loc0), flat]];
            };
            struct fsOutput {
                metal::float4 member [[color(0)]];
            };
            fragment fsOutput fs(
              fsInput varyings [[stage_in]]
            , metal::float4 pos [[position]]
            , constant Globals& fs_g [[buffer(0)]]
            ) {
                const fs_in v = { pos, varyings.shade };
                metal::float2 fs_9_ = metal::float2 {};
                Upto fs_20_ = Upto {};
                V fs_1_ = V {};
                float fs_6_ = float {};
                bool fs_5_ = bool {};
                float vec2_1_ = float {};
                float vec2_2_ = float {};
                float vec2_3_ = float {};
                float vec2_4_ = float {};
                metal::float2 vec2_0_ = metal::float2 {};
                float fs_14_ = float {};
                float half_1_ = float {};
                float half_2_ = float {};
                float half_0_ = float {};
                float fs_13_ = float {};
                float widen_2_ = float {};
                float widen_4_ = float {};
                float widen_6_ = float {};
                float fs_15_ = float {};
                uint fs_19_ = uint {};
                Upto fs_18_ = Upto {};
                Upto into_iter_1_ = Upto {};
                Upto into_iter_0_ = Upto {};
                Upto fs_17_ = Upto {};
                uint next_3_ = uint {};
                uint next_4_ = uint {};
                bool next_2_ = bool {};
                type_8 next_0_ = type_8 {};
                uint next_5_ = uint {};
                type_9 next_8_ = type_9 {};
                uint next_7_ = uint {};
                type_8 fs_22_ = type_8 {};
                int fs_25_ = int {};
                uint fs_27_ = uint {};
                uint fs_30_ = uint {};
                bool fs_29_ = bool {};
                uint fs_35_ = uint {};
                float fs_34_ = float {};
                uint flag = 0u;
                metal::float4 fs_38_ = metal::float4 {};
                type_10 fs_37_ = type_10 {};
                type_11 fs_39_ = type_11 {};
                float fs_40_ = float {};
                Big fs_36_ = Big {};
                float fs_43_ = float {};
                int fs_42_ = int {};
                uint fs_46_ = uint {};
                int fs_45_ = int {};
                int fs_44_ = int {};
                bool fs_56_ = bool {};
                bool fs_57_ = bool {};
                bool fs_58_ = bool {};
                bool fs_59_ = bool {};
                int fs_41_ = int {};
                float fs_47_ = float {};
                float fs_48_ = float {};
                Big fs_50_ = Big {};
                int fs_52_ = int {};
                uint fs_51_ = uint {};
                Big pick_1_ = Big {};
                uint pick_2_ = uint {};
                uint pick_5_ = uint {};
                bool pick_12_ = bool {};
                uint pick_4_ = uint {};
                bool pick_13_ = bool {};
                float pick_6_ = float {};
                uint pick_9_ = uint {};
                bool pick_14_ = bool {};
                uint pick_8_ = uint {};
                bool pick_15_ = bool {};
                int pick_10_ = int {};
                float pick_7_ = float {};
                float pick_3_ = float {};
                float pick_11_ = float {};
                float pick_0_ = float {};
                float fs_49_ = float {};
                int fs_55_ = int {};
                bool fs_60_ = bool {};
                bool fs_61_ = bool {};
                bool fs_62_ = bool {};
                bool fs_63_ = bool {};
                int fs_54_ = int {};
                float fs_53_ = float {};
                float vec4_1_ = float {};
                float vec4_2_ = float {};
                float vec4_3_ = float {};
                float vec4_4_ = float {};
                float vec4_5_ = float {};
                float vec4_6_ = float {};
                float vec4_7_ = float {};
                float vec4_8_ = float {};
                metal::float4 vec4_0_ = metal::float4 {};
                metal::float4 fs_0_ = metal::float4 {};
                float vec4_1_1 = float {};
                float vec4_2_1 = float {};
                float vec4_3_1 = float {};
                float vec4_4_1 = float {};
                float vec4_5_1 = float {};
                float vec4_6_1 = float {};
                float vec4_7_1 = float {};
                float vec4_8_1 = float {};
                metal::float4 vec4_0_1 = metal::float4 {};
                fs_1_ = V {Position {v.pos}, Flat {v.shade}};
                float _e13 = fs_1_.f1_.f0_;
                fs_6_ = _e13;
                float _e17 = fs_6_;
                fs_5_ = _e17 < 0.0;
                bool _e23 = fs_5_;
                if (_e23) {
                    vec4_1_1 = 0.0;
                    vec4_2_1 = 0.0;
                    vec4_3_1 = 0.0;
                    vec4_4_1 = 0.0;
                    float _e597 = vec4_1_1;
                    vec4_5_1 = _e597;
                    float _e601 = vec4_2_1;
                    vec4_6_1 = _e601;
                    float _e605 = vec4_3_1;
                    vec4_7_1 = _e605;
                    float _e609 = vec4_4_1;
                    vec4_8_1 = _e609;
                    float _e613 = vec4_5_1;
                    float _e615 = vec4_6_1;
                    float _e617 = vec4_7_1;
                    float _e619 = vec4_8_1;
                    vec4_0_1 = metal::float4(_e613, _e615, _e617, _e619);
                    metal::float4 _e624 = vec4_0_1;
                    fs_0_ = _e624;
                    metal::float4 _e627 = fs_0_;
                    return fsOutput { _e627 };
                } else {
                    vec2_1_ = 0.0;
                    vec2_2_ = 0.0;
                    float _e31 = vec2_1_;
                    vec2_3_ = _e31;
                    float _e35 = vec2_2_;
                    vec2_4_ = _e35;
                    float _e39 = vec2_3_;
                    float _e41 = vec2_4_;
                    vec2_0_ = metal::float2(_e39, _e41);
                    metal::float2 _e46 = vec2_0_;
                    fs_9_ = _e46;
                    float _e51 = fs_1_.f1_.f0_;
                    fs_14_ = _e51;
                    float _e55 = fs_14_;
                    half_1_ = _e55;
                    float _e59 = half_1_;
                    half_2_ = _e59;
                    float _e63 = half_2_;
                    half_0_ = _e63 * 0.5;
                    float _e69 = half_0_;
                    fs_13_ = _e69;
                    float _e73 = fs_13_;
                    widen_2_ = _e73;
                    float _e77 = widen_2_;
                    widen_4_ = _e77;
                    float _e82 = fs_9_.x;
                    float _e84 = widen_4_;
                    fs_9_.x = _e82 - _e84;
                    float _e89 = widen_2_;
                    widen_6_ = _e89;
                    float _e94 = fs_9_.y;
                    float _e96 = widen_6_;
                    fs_9_.y = _e94 + _e96;
                    fs_15_ = 0.0;
                    uint _e105 = fs_g.f1_;
                    fs_19_ = _e105;
                    uint _e110 = fs_19_;
                    fs_18_ = Upto {0u, _e110};
                    Upto _e115 = fs_18_;
                    into_iter_1_ = _e115;
                    Upto _e119 = into_iter_1_;
                    into_iter_0_ = _e119;
                    Upto _e123 = into_iter_0_;
                    fs_17_ = _e123;
                    Upto _e127 = fs_17_;
                    fs_20_ = _e127;
                    uint2 loop_bound = uint2(4294967295u);
                    while(true) {
                        if (metal::all(loop_bound == uint2(0u))) { break; }
                        loop_bound -= uint2(loop_bound.y == 0u, 1u);
                        uint2 loop_bound_1 = uint2(4294967295u);
                        while(true) {
                            if (metal::all(loop_bound_1 == uint2(0u))) { break; }
                            loop_bound_1 -= uint2(loop_bound_1.y == 0u, 1u);
                            uint _e131 = fs_20_.f0_;
                            next_3_ = _e131;
                            uint _e136 = fs_20_.f1_;
                            next_4_ = _e136;
                            uint _e140 = next_3_;
                            uint _e142 = next_4_;
                            next_2_ = _e140 < _e142;
                            bool _e147 = next_2_;
                            if (_e147) {
                                uint _e155 = fs_20_.f0_;
                                next_5_ = _e155;
                                uint _e160 = fs_20_.f0_;
                                uint _e162 = _e160 + 1u;
                                next_8_ = type_9 {_e162, _e162 < _e160};
                                bool _e170 = next_8_.f1_;
                                if (!(_e170)) {
                                    uint _e174 = next_8_.f0_;
                                    fs_20_.f0_ = _e174;
                                    uint _e178 = next_5_;
                                    next_7_ = _e178;
                                    uint _e183 = next_7_;
                                    next_0_ = type_8 {1, {}, _e183};
                                } else {
                                    break;
                                }
                            } else {
                                next_0_ = type_8 {0, {}, uint {}};
                            }
                            type_8 _e187 = next_0_;
                            fs_22_ = _e187;
                            int _e192 = fs_22_.tag;
                            fs_25_ = _e192;
                            int _e196 = fs_25_;
                            if (_e196 == 0) {
                            } else {
                                int _e201 = fs_22_.tag;
                                fs_25_ = _e201;
                                int _e204 = fs_25_;
                                if (_e204 == 1) {
                                    uint _e209 = fs_22_.v1_0_;
                                    fs_27_ = _e209;
                                    uint _e213 = fs_27_;
                                    fs_30_ = _e213;
                                    uint _e217 = fs_30_;
                                    fs_29_ = _e217 > 6u;
                                    bool _e223 = fs_29_;
                                    if (_e223) {
                                    } else {
                                        uint _e225 = fs_27_;
                                        fs_35_ = _e225;
                                        uint _e229 = fs_35_;
                                        fs_34_ = static_cast<float>(_e229);
                                        float _e234 = fs_15_;
                                        float _e236 = fs_34_;
                                        fs_15_ = _e234 + _e236;
                                        flag = 36u;
                                        break;
                                    }
                                } else {
                                    break;
                                }
                            }
                            metal::float4 _e244 = fs_g.f0_;
                            fs_38_ = _e244;
                            metal::float4 _e248 = fs_38_;
                            fs_37_ = type_10 {{_e248, _e248, _e248, _e248}};
                            fs_39_ = type_11 {{1, 2, 3, 4, 5, 6, 7, 8}};
                            float _e264 = fs_15_;
                            fs_40_ = _e264;
                            type_10 _e268 = fs_37_;
                            type_11 _e270 = fs_39_;
                            float _e272 = fs_40_;
                            fs_36_ = Big {_e268, _e270, _e272};
                            float _e280 = fs_1_.f0_.f0_.x;
                            fs_43_ = _e280;
                            float _e284 = fs_43_;
                            fs_42_ = (_e284 != _e284) ? int {} : ((_e284 >= 2147483600.0) ? 2147483647 : naga_f2i32(metal::clamp(_e284, -2147483600.0, 2147483500.0)));
                            uint _e300 = fs_g.f1_;
                            fs_46_ = _e300;
                            uint _e304 = fs_46_;
                            fs_45_ = as_type<int>(_e304);
                            int _e309 = fs_45_;
                            fs_44_ = _e309 | 1;
                            int _e315 = fs_44_;
                            fs_56_ = _e315 == 0;
                            bool _e321 = fs_56_;
                            if (!(_e321)) {
                                int _e324 = fs_44_;
                                fs_57_ = _e324 == -1;
                                int _e330 = fs_42_;
                                fs_58_ = _e330 == (-2147483647 - 1);
                                bool _e336 = fs_57_;
                                bool _e338 = fs_58_;
                                fs_59_ = _e336 && _e338;
                                bool _e343 = fs_59_;
                                if (!(_e343)) {
                                    int _e346 = fs_42_;
                                    int _e348 = fs_44_;
                                    fs_41_ = naga_div(_e346, _e348);
                                    float _e354 = fs_9_.x;
                                    fs_47_ = _e354;
                                    float _e359 = fs_9_.y;
                                    fs_48_ = _e359;
                                    Big _e363 = fs_36_;
                                    fs_50_ = _e363;
                                    int _e367 = fs_41_;
                                    fs_52_ = _e367;
                                    int _e371 = fs_52_;
                                    fs_51_ = as_type<uint>(_e371);
                                    Big _e376 = fs_50_;
                                    pick_1_ = _e376;
                                    uint _e380 = fs_51_;
                                    pick_2_ = _e380;
                                    uint _e384 = pick_2_;
                                    pick_5_ = _e384;
                                    pick_12_ = 4u == 0u;
                                    bool _e393 = pick_12_;
                                    if (!(_e393)) {
                                        uint _e396 = pick_5_;
                                        pick_4_ = naga_mod(_e396, 4u);
                                        uint _e402 = pick_4_;
                                        pick_13_ = _e402 < 4u;
                                        bool _e408 = pick_13_;
                                        if (_e408) {
                                            uint _e412 = pick_4_;
                                            float _e415 = pick_1_.f0_.inner[_e412].x;
                                            pick_6_ = _e415;
                                            uint _e419 = pick_2_;
                                            pick_9_ = _e419;
                                            pick_14_ = 8u == 0u;
                                            bool _e428 = pick_14_;
                                            if (!(_e428)) {
                                                uint _e431 = pick_9_;
                                                pick_8_ = naga_mod(_e431, 8u);
                                                uint _e437 = pick_8_;
                                                pick_15_ = _e437 < 8u;
                                                bool _e443 = pick_15_;
                                                if (_e443) {
                                                    uint _e447 = pick_8_;
                                                    int _e449 = pick_1_.f1_.inner[_e447];
                                                    pick_10_ = _e449;
                                                    int _e453 = pick_10_;
                                                    pick_7_ = static_cast<float>(_e453);
                                                    float _e458 = pick_6_;
                                                    float _e460 = pick_7_;
                                                    pick_3_ = _e458 + _e460;
                                                    float _e466 = pick_1_.f2_;
                                                    pick_11_ = _e466;
                                                    float _e470 = pick_3_;
                                                    float _e472 = pick_11_;
                                                    pick_0_ = _e470 + _e472;
                                                    float _e477 = pick_0_;
                                                    fs_49_ = _e477;
                                                    int _e481 = fs_41_;
                                                    fs_55_ = _e481;
                                                    fs_60_ = 3 == 0;
                                                    bool _e490 = fs_60_;
                                                    if (!(_e490)) {
                                                        fs_61_ = 3 == -1;
                                                        int _e498 = fs_55_;
                                                        fs_62_ = _e498 == (-2147483647 - 1);
                                                        bool _e504 = fs_61_;
                                                        bool _e506 = fs_62_;
                                                        fs_63_ = _e504 && _e506;
                                                        bool _e511 = fs_63_;
                                                        if (!(_e511)) {
                                                            int _e514 = fs_55_;
                                                            fs_54_ = naga_mod(_e514, 3);
                                                            int _e520 = fs_54_;
                                                            fs_53_ = static_cast<float>(_e520);
                                                            float _e525 = fs_47_;
                                                            vec4_1_ = _e525;
                                                            float _e529 = fs_48_;
                                                            vec4_2_ = _e529;
                                                            float _e533 = fs_49_;
                                                            vec4_3_ = _e533;
                                                            float _e537 = fs_53_;
                                                            vec4_4_ = _e537;
                                                            float _e541 = vec4_1_;
                                                            vec4_5_ = _e541;
                                                            float _e545 = vec4_2_;
                                                            vec4_6_ = _e545;
                                                            float _e549 = vec4_3_;
                                                            vec4_7_ = _e549;
                                                            float _e553 = vec4_4_;
                                                            vec4_8_ = _e553;
                                                            float _e557 = vec4_5_;
                                                            float _e559 = vec4_6_;
                                                            float _e561 = vec4_7_;
                                                            float _e563 = vec4_8_;
                                                            vec4_0_ = metal::float4(_e557, _e559, _e561, _e563);
                                                            metal::float4 _e568 = vec4_0_;
                                                            fs_0_ = _e568;
                                                            metal::float4 _e572 = fs_0_;
                                                            return fsOutput { _e572 };
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            break;
                        }
                        uint _e574 = flag;
                        if (_e574 != 0u) {
                            uint _e578 = flag;
                            if (_e578 == 36u) {
                                flag = 0u;
                                continue;
                            }
                            break;
                        }
                        metal::discard_fragment();
                        return fsOutput { metal::float4 {} };
                        break;
                    }
                }
                metal::discard_fragment();
                return fsOutput { metal::float4 {} };
            }
        "#]],
    );
}

#[test]
fn transmute_is_a_bitcast() {
    check_msl(
        &fragment(
            "",
            "    let bits: u32 = unsafe { core::mem::transmute::<f32, u32>(v.shade.0) };
    vec4((bits >> 31) as f32, 0.0, 0.0, 1.0)",
        ),
        expect![[r#"
            // language: metal2.4
            #include <metal_stdlib>
            #include <simd/simd.h>

            using metal::uint;

            struct _mslBufferSizes {
                uint size1;
            };

            struct fs_in {
                metal::float4 pos;
                float shade;
                char _pad2[12];
            };
            struct Globals {
                metal::float4 f0_;
                uint f1_;
                uint f2_;
                uint f3_;
                uint f4_;
            };
            typedef metal::int4 type_4[1];
            struct Position {
                metal::float4 f0_;
            };
            struct Flat {
                float f0_;
            };
            struct V {
                Position f0_;
                Flat f1_;
            };

            struct fsInput {
                float shade [[user(loc0), flat]];
            };
            struct fsOutput {
                metal::float4 member [[color(0)]];
            };
            fragment fsOutput fs(
              fsInput varyings [[stage_in]]
            , metal::float4 pos [[position]]
            ) {
                const fs_in v = { pos, varyings.shade };
                V fs_1_ = V {};
                float fs_5_ = float {};
                uint fs_4_ = uint {};
                uint fs_8_ = uint {};
                uint fs_9_ = uint {};
                bool fs_10_ = bool {};
                uint fs_7_ = uint {};
                float fs_6_ = float {};
                float vec4_1_ = float {};
                float vec4_2_ = float {};
                float vec4_3_ = float {};
                float vec4_4_ = float {};
                float vec4_5_ = float {};
                float vec4_6_ = float {};
                float vec4_7_ = float {};
                float vec4_8_ = float {};
                metal::float4 vec4_0_ = metal::float4 {};
                metal::float4 fs_0_ = metal::float4 {};
                fs_1_ = V {Position {v.pos}, Flat {v.shade}};
                float _e11 = fs_1_.f1_.f0_;
                fs_5_ = _e11;
                float _e15 = fs_5_;
                fs_4_ = as_type<uint>(_e15);
                uint _e20 = fs_4_;
                fs_8_ = _e20;
                fs_9_ = as_type<uint>(31);
                uint _e28 = fs_9_;
                fs_10_ = _e28 < 32u;
                bool _e34 = fs_10_;
                if (_e34) {
                    uint _e37 = fs_8_;
                    fs_7_ = _e37 >> (as_type<uint>(31) & 31u);
                    uint _e46 = fs_7_;
                    fs_6_ = static_cast<float>(_e46);
                    float _e51 = fs_6_;
                    vec4_1_ = _e51;
                    vec4_2_ = 0.0;
                    vec4_3_ = 0.0;
                    vec4_4_ = 1.0;
                    float _e64 = vec4_1_;
                    vec4_5_ = _e64;
                    float _e68 = vec4_2_;
                    vec4_6_ = _e68;
                    float _e72 = vec4_3_;
                    vec4_7_ = _e72;
                    float _e76 = vec4_4_;
                    vec4_8_ = _e76;
                    float _e80 = vec4_5_;
                    float _e82 = vec4_6_;
                    float _e84 = vec4_7_;
                    float _e86 = vec4_8_;
                    vec4_0_ = metal::float4(_e80, _e82, _e84, _e86);
                    metal::float4 _e91 = vec4_0_;
                    fs_0_ = _e91;
                    metal::float4 _e95 = fs_0_;
                    return fsOutput { _e95 };
                } else {
                    metal::discard_fragment();
                    return fsOutput { metal::float4 {} };
                }
                metal::discard_fragment();
                return fsOutput { metal::float4 {} };
            }
        "#]],
    );
}

#[test]
fn derivatives_and_discard_in_a_fragment() {
    check_msl(
        &fragment(
            "",
            "    let d = gpu::intrinsic::dpdx(v.pos.0.xy());
    if d.x > 1.0 { gpu::intrinsic::discard(); }
    vec4(d.x, d.y, 0.0, 1.0)",
        ),
        expect![[r#"
            // language: metal2.4
            #include <metal_stdlib>
            #include <simd/simd.h>

            using metal::uint;

            struct _mslBufferSizes {
                uint size1;
            };

            struct fs_in {
                metal::float4 pos;
                float shade;
                char _pad2[12];
            };
            struct Globals {
                metal::float4 f0_;
                uint f1_;
                uint f2_;
                uint f3_;
                uint f4_;
            };
            typedef metal::int4 type_4[1];
            struct Position {
                metal::float4 f0_;
            };
            struct Flat {
                float f0_;
            };
            struct V {
                Position f0_;
                Flat f1_;
            };

            struct fsInput {
                float shade [[user(loc0), flat]];
            };
            struct fsOutput {
                metal::float4 member [[color(0)]];
            };
            fragment fsOutput fs(
              fsInput varyings [[stage_in]]
            , metal::float4 pos [[position]]
            ) {
                const fs_in v = { pos, varyings.shade };
                V fs_1_ = V {};
                metal::float4 fs_6_ = metal::float4 {};
                metal::float4 xy_1_ = metal::float4 {};
                float xy_2_ = float {};
                float xy_3_ = float {};
                metal::float2 xy_0_ = metal::float2 {};
                metal::float2 fs_5_ = metal::float2 {};
                metal::float2 fs_4_ = metal::float2 {};
                float fs_9_ = float {};
                bool fs_8_ = bool {};
                float fs_12_ = float {};
                float fs_13_ = float {};
                float vec4_1_ = float {};
                float vec4_2_ = float {};
                float vec4_3_ = float {};
                float vec4_4_ = float {};
                float vec4_5_ = float {};
                float vec4_6_ = float {};
                float vec4_7_ = float {};
                float vec4_8_ = float {};
                metal::float4 vec4_0_ = metal::float4 {};
                metal::float4 fs_0_ = metal::float4 {};
                fs_1_ = V {Position {v.pos}, Flat {v.shade}};
                metal::float4 _e11 = fs_1_.f0_.f0_;
                fs_6_ = _e11;
                metal::float4 _e15 = fs_6_;
                xy_1_ = _e15;
                float _e20 = xy_1_.x;
                xy_2_ = _e20;
                float _e25 = xy_1_.y;
                xy_3_ = _e25;
                float _e29 = xy_2_;
                float _e31 = xy_3_;
                xy_0_ = metal::float2(_e29, _e31);
                metal::float2 _e36 = xy_0_;
                fs_5_ = _e36;
                metal::float2 _e40 = fs_5_;
                metal::float2 _e41 = metal::dfdx(_e40);
                fs_4_ = _e41;
                float _e46 = fs_4_.x;
                fs_9_ = _e46;
                float _e50 = fs_9_;
                fs_8_ = _e50 > 1.0;
                bool _e56 = fs_8_;
                if (_e56) {
                    metal::discard_fragment();
                    return fsOutput { metal::float4 {} };
                } else {
                    float _e59 = fs_4_.x;
                    fs_12_ = _e59;
                    float _e64 = fs_4_.y;
                    fs_13_ = _e64;
                    float _e68 = fs_12_;
                    vec4_1_ = _e68;
                    float _e72 = fs_13_;
                    vec4_2_ = _e72;
                    vec4_3_ = 0.0;
                    vec4_4_ = 1.0;
                    float _e82 = vec4_1_;
                    vec4_5_ = _e82;
                    float _e86 = vec4_2_;
                    vec4_6_ = _e86;
                    float _e90 = vec4_3_;
                    vec4_7_ = _e90;
                    float _e94 = vec4_4_;
                    vec4_8_ = _e94;
                    float _e98 = vec4_5_;
                    float _e100 = vec4_6_;
                    float _e102 = vec4_7_;
                    float _e104 = vec4_8_;
                    vec4_0_ = metal::float4(_e98, _e100, _e102, _e104);
                    metal::float4 _e109 = vec4_0_;
                    fs_0_ = _e109;
                    metal::float4 _e113 = fs_0_;
                    return fsOutput { _e113 };
                }
                metal::discard_fragment();
                return fsOutput { metal::float4 {} };
            }
        "#]],
    );
}

/// Lowering is a function of the source: two databases, two builds, the same bytes
/// (`deterministic-output`).
#[test]
fn the_same_source_writes_the_same_msl() {
    let source = fragment(
        "fn helper(x: f32, y: f32) -> f32 { if x < y { x } else { y } }",
        "    let a = helper(v.shade.0, g.tint.x);
    let b = helper(g.tint.y, a);
    vec4(a, b, v.pos.0.z, 1.0)",
    );
    let first = build(&source).expect("the first build");
    let second = build(&source).expect("the second build");
    assert_eq!(first.msl, second.msl);
}

#[test]
fn a_u64_is_refused() {
    refused(&fragment("", "    let x: u64 = g.count as u64 + 1; vec4(x as f32, 0.0, 0.0, 1.0)"), "64-bit integer", why::WIDTH);
}

#[test]
fn a_function_pointer_call_is_refused() {
    refused(
        &fragment("fn twice(x: f32) -> f32 { x * 2.0 }", "    let f: fn(f32) -> f32 = twice; vec4(f(v.shade.0), 0.0, 0.0, 1.0)"),
        "function pointer",
        why::CALLS,
    );
}

#[test]
fn a_static_read_is_refused() {
    refused(&fragment("pub static K: f32 = 2.0;", "    vec4(K, 0.0, 0.0, 1.0)"), "static", why::STATICS);
}

#[test]
fn recursion_is_refused() {
    refused(
        &fragment(
            "fn down(n: u32) -> u32 { if n == 0 { 0 } else { down(n - 1) } }",
            "    vec4(down(g.count) as f32, 0.0, 0.0, 1.0)",
        ),
        "from inside itself",
        why::RECURSION,
    );
}

#[test]
fn a_derivative_in_a_vertex_stage_is_refused() {
    refused(
        &vertex("", "    let x = gpu::intrinsic::dpdx(i.0 as f32); VOut { pos: Position(vec4(x, 0.0, 0.0, 1.0)), uv: vec2(0.0, 0.0) }"),
        "vertex stage",
        why::STAGE,
    );
}

#[test]
fn a_reference_chosen_at_run_time_is_refused() {
    refused(
        &fragment(
            "",
            "    let a = 1.0; let b = 2.0;
    let r = if v.shade.0 > 0.0 { &a } else { &b };
    vec4(*r, 0.0, 0.0, 1.0)",
        ),
        "different place",
        why::POINTERS,
    );
}

#[test]
fn an_enum_in_a_storage_buffer_is_refused() {
    let source = fragment("", "    vec4(0.0, 0.0, 0.0, 1.0)")
        .replace("pub fn fs(v: V, g: &Globals, data: &[IVec4])", "#[derive(Copy, Clone)]\n#[repr(u32)]\npub enum Mode { A, B }\n#[derive(Copy, Clone)]\n#[repr(C)]\npub struct Item { pub mode: Mode, pub x: u32, pub y: u32, pub z: u32 }\npub fn fs(v: V, g: &Globals, data: &[Item])");
    refused(&source, "holding the enum", why::ENUMS);
}

/// A kernel: its thread's ids as built-in arguments, read-only storage read, a RawOut read and
/// written behind its bounds check, the halves packed; no ray query, so no RAY_QUERY capability.
#[test]
fn a_kernel_reads_storage_and_writes_a_raw_out() {
    check_msl(
        &kernel(
            "",
            "    let id = t.global();
    let at = (id.y * 8 + id.x) as usize;
    if at >= acc.len() {
        return;
    }
    let d = data[g.count as usize];
    let before = unsafe { acc.read(at) };
    let h = gpu::math::f32tof16(g.tint.x) | (gpu::math::f32tof16(gpu::math::f16tof32(before)) << 16);
    unsafe { acc.write(at, h + d.x as u32 + t.local().x + t.group().y) };",
            false,
        ),
        expect![[r#"
            // language: metal2.4
            #include <metal_stdlib>
            #include <simd/simd.h>

            using metal::uint;

            struct _mslBufferSizes {
                uint size1;
                uint size2;
            };

            struct Globals {
                metal::float4 f0_;
                uint f1_;
                uint f2_;
                uint f3_;
                uint f4_;
            };
            typedef metal::int4 type_4[1];
            typedef uint type_5[1];
            struct Thread {
                metal::packed_uint3 f0_;
                metal::packed_uint3 f1_;
                metal::packed_uint3 f2_;
            };
            struct type_7 {
                uint f0_;
                bool f1_;
                char _pad2[3];
            };
            uint naga_div(uint lhs, uint rhs) {
                return lhs / metal::select(rhs, 1u, rhs == 0u);
            }


            struct csInput {
            };
            [[max_total_threads_per_threadgroup(64)]] kernel void cs(
              metal::uint3 t_global [[thread_position_in_grid]]
            , metal::uint3 t_local [[thread_position_in_threadgroup]]
            , metal::uint3 t_group [[threadgroup_position_in_grid]]
            , constant Globals& cs_g [[buffer(0)]]
            , device type_4 const& cs_data [[buffer(1)]]
            , device type_5& cs_acc [[buffer(2)]]
            , constant _mslBufferSizes& _buffer_sizes [[buffer(3)]]
            ) {
                Thread cs_1_ = Thread {};
                metal::uint3 global_0_ = metal::uint3 {};
                metal::uint3 cs_5_ = metal::uint3 {};
                uint cs_10_ = uint {};
                type_7 cs_47_ = type_7 {};
                uint cs_9_ = uint {};
                uint cs_11_ = uint {};
                type_7 cs_48_ = type_7 {};
                uint cs_8_ = uint {};
                uint cs_7_ = uint {};
                uint cs_14_ = uint {};
                uint len_0_ = uint {};
                uint cs_15_ = uint {};
                bool cs_13_ = bool {};
                uint cs_21_ = uint {};
                uint cs_20_ = uint {};
                uint cs_50_ = uint {};
                bool cs_49_ = bool {};
                metal::int4 cs_19_ = metal::int4 {};
                uint cs_24_ = uint {};
                uint read_2_ = uint {};
                uint read_5_ = uint {};
                uint len_0_1 = uint {};
                uint read_6_ = uint {};
                bool read_4_ = bool {};
                uint read_10_ = uint {};
                uint read_0_ = uint {};
                uint cs_22_ = uint {};
                float cs_27_ = float {};
                uint cs_26_ = uint {};
                uint cs_31_ = uint {};
                float cs_30_ = float {};
                uint cs_29_ = uint {};
                uint cs_51_ = uint {};
                bool cs_52_ = bool {};
                uint cs_28_ = uint {};
                uint cs_25_ = uint {};
                uint cs_34_ = uint {};
                uint cs_38_ = uint {};
                int cs_40_ = int {};
                uint cs_39_ = uint {};
                type_7 cs_53_ = type_7 {};
                uint cs_37_ = uint {};
                metal::uint3 local_0_ = metal::uint3 {};
                metal::uint3 cs_41_ = metal::uint3 {};
                uint cs_43_ = uint {};
                type_7 cs_54_ = type_7 {};
                uint cs_36_ = uint {};
                metal::uint3 group_0_ = metal::uint3 {};
                metal::uint3 cs_44_ = metal::uint3 {};
                uint cs_46_ = uint {};
                type_7 cs_55_ = type_7 {};
                uint cs_35_ = uint {};
                uint write_2_ = uint {};
                uint write_3_ = uint {};
                bool write_13_ = bool {};
                uint write_6_ = uint {};
                uint len_0_2 = uint {};
                uint write_7_ = uint {};
                bool write_5_ = bool {};
                uint write_11_ = uint {};
                uint write_12_ = uint {};
                cs_1_ = Thread {t_global, t_local, t_group};
                metal::uint3 _e8 = cs_1_.f0_;
                global_0_ = _e8;
                metal::uint3 _e12 = global_0_;
                cs_5_ = _e12;
                uint _e17 = cs_5_.y;
                cs_10_ = _e17;
                uint _e21 = cs_10_;
                uint _e23 = _e21 * 8u;
                bool _e25 = _e21 != 0u;
                cs_47_ = type_7 {_e23, _e25 && (naga_div(_e23, _e25 ? _e21 : 1u) != 8u)};
                bool _e36 = cs_47_.f1_;
                if (!(_e36)) {
                    uint _e40 = cs_47_.f0_;
                    cs_9_ = _e40;
                    uint _e45 = cs_5_.x;
                    cs_11_ = _e45;
                    uint _e49 = cs_9_;
                    uint _e51 = cs_11_;
                    uint _e52 = _e49 + _e51;
                    cs_48_ = type_7 {_e52, _e52 < _e49};
                    bool _e60 = cs_48_.f1_;
                    if (!(_e60)) {
                        uint _e64 = cs_48_.f0_;
                        cs_8_ = _e64;
                        uint _e68 = cs_8_;
                        cs_7_ = _e68;
                        uint _e72 = cs_7_;
                        cs_14_ = _e72;
                        len_0_ = 1 + (_buffer_sizes.size2 - 0 - 4) / 4;
                        uint _e80 = len_0_;
                        cs_15_ = _e80;
                        uint _e84 = cs_14_;
                        uint _e86 = cs_15_;
                        cs_13_ = _e84 >= _e86;
                        bool _e91 = cs_13_;
                        if (_e91) {
                            return;
                        } else {
                            uint _e94 = cs_g.f1_;
                            cs_21_ = _e94;
                            uint _e98 = cs_21_;
                            cs_20_ = _e98;
                            cs_50_ = 1 + (_buffer_sizes.size1 - 0 - 16) / 16;
                            uint _e106 = cs_20_;
                            uint _e108 = cs_50_;
                            cs_49_ = _e106 < _e108;
                            bool _e113 = cs_49_;
                            if (_e113) {
                                uint _e116 = cs_20_;
                                metal::int4 _e118 = cs_data[_e116];
                                cs_19_ = _e118;
                                uint _e122 = cs_7_;
                                cs_24_ = _e122;
                                uint _e126 = cs_24_;
                                read_2_ = _e126;
                                uint _e130 = read_2_;
                                read_5_ = _e130;
                                len_0_1 = 1 + (_buffer_sizes.size2 - 0 - 4) / 4;
                                uint _e138 = len_0_1;
                                read_6_ = _e138;
                                uint _e142 = read_5_;
                                uint _e144 = read_6_;
                                read_4_ = _e142 >= _e144;
                                bool _e149 = read_4_;
                                if (_e149) {
                                } else {
                                    uint _e151 = read_2_;
                                    read_10_ = _e151;
                                    uint _e156 = read_10_;
                                    uint _e158 = cs_acc[_e156];
                                    read_0_ = _e158;
                                    uint _e162 = read_0_;
                                    cs_22_ = _e162;
                                    float _e168 = cs_g.f0_.x;
                                    cs_27_ = _e168;
                                    float _e172 = cs_27_;
                                    cs_26_ = as_type<uint>(half2(metal::float2(_e172, 0.0)));
                                    uint _e179 = cs_22_;
                                    cs_31_ = _e179;
                                    uint _e183 = cs_31_;
                                    cs_30_ = float2(as_type<half2>(_e183)).x;
                                    float _e189 = cs_30_;
                                    cs_29_ = as_type<uint>(half2(metal::float2(_e189, 0.0)));
                                    cs_51_ = as_type<uint>(16);
                                    uint _e200 = cs_51_;
                                    cs_52_ = _e200 < 32u;
                                    bool _e206 = cs_52_;
                                    if (_e206) {
                                        uint _e208 = cs_29_;
                                        cs_28_ = _e208 << (as_type<uint>(16) & 31u);
                                        uint _e217 = cs_26_;
                                        uint _e219 = cs_28_;
                                        cs_25_ = _e217 | _e219;
                                        uint _e224 = cs_7_;
                                        cs_34_ = _e224;
                                        uint _e228 = cs_25_;
                                        cs_38_ = _e228;
                                        int _e233 = cs_19_.x;
                                        cs_40_ = _e233;
                                        int _e237 = cs_40_;
                                        cs_39_ = as_type<uint>(_e237);
                                        uint _e242 = cs_38_;
                                        uint _e244 = cs_39_;
                                        uint _e245 = _e242 + _e244;
                                        cs_53_ = type_7 {_e245, _e245 < _e242};
                                        bool _e253 = cs_53_.f1_;
                                        if (!(_e253)) {
                                            uint _e257 = cs_53_.f0_;
                                            cs_37_ = _e257;
                                            metal::uint3 _e262 = cs_1_.f1_;
                                            local_0_ = _e262;
                                            metal::uint3 _e266 = local_0_;
                                            cs_41_ = _e266;
                                            uint _e271 = cs_41_.x;
                                            cs_43_ = _e271;
                                            uint _e275 = cs_37_;
                                            uint _e277 = cs_43_;
                                            uint _e278 = _e275 + _e277;
                                            cs_54_ = type_7 {_e278, _e278 < _e275};
                                            bool _e286 = cs_54_.f1_;
                                            if (!(_e286)) {
                                                uint _e290 = cs_54_.f0_;
                                                cs_36_ = _e290;
                                                metal::uint3 _e295 = cs_1_.f2_;
                                                group_0_ = _e295;
                                                metal::uint3 _e299 = group_0_;
                                                cs_44_ = _e299;
                                                uint _e304 = cs_44_.y;
                                                cs_46_ = _e304;
                                                uint _e308 = cs_36_;
                                                uint _e310 = cs_46_;
                                                uint _e311 = _e308 + _e310;
                                                cs_55_ = type_7 {_e311, _e311 < _e308};
                                                bool _e319 = cs_55_.f1_;
                                                if (!(_e319)) {
                                                    uint _e323 = cs_55_.f0_;
                                                    cs_35_ = _e323;
                                                    uint _e327 = cs_34_;
                                                    write_2_ = _e327;
                                                    uint _e331 = cs_35_;
                                                    write_3_ = _e331;
                                                    write_13_ = false;
                                                    write_13_ = true;
                                                    uint _e340 = write_2_;
                                                    write_6_ = _e340;
                                                    len_0_2 = 1 + (_buffer_sizes.size2 - 0 - 4) / 4;
                                                    uint _e348 = len_0_2;
                                                    write_7_ = _e348;
                                                    uint _e352 = write_6_;
                                                    uint _e354 = write_7_;
                                                    write_5_ = _e352 >= _e354;
                                                    bool _e359 = write_5_;
                                                    if (_e359) {
                                                    } else {
                                                        uint _e361 = write_2_;
                                                        write_11_ = _e361;
                                                        uint _e365 = write_3_;
                                                        write_12_ = _e365;
                                                        write_13_ = false;
                                                        uint _e372 = write_11_;
                                                        uint _e375 = write_12_;
                                                        cs_acc[_e372] = _e375;
                                                        write_13_ = false;
                                                        write_13_ = false;
                                                        return;
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                return;
                return;
            }
        "#]],
    );
}

/// The two queries light.slang makes: the closest hit, matched as an `Option<Hit>` with a guard
/// arm, and a shadow ray that ends at its first hit; each one naga ray query proceeded to its end.
#[test]
fn a_kernel_traces_rays() {
    check_msl(
        &kernel(
            "",
            "    let id = t.global();
    let o = vec3(id.x as f32, id.y as f32, 100.0);
    let dir = vec3(0.0, 0.0, -1.0);
    let (depth, inst) = match scene.trace(Ray { origin: o, dir, tmin: 0.0, tmax: 1e7 }, 0xff) {
        Some(hit) => (hit.t, hit.instance_index + hit.primitive_index),
        None if g.count > 0 => (-1.0, 0),
        None => return,
    };
    let shadow = Ray { origin: vec3(o.x, o.y, o.z - depth), dir: vec3(0.0, 0.0, 1.0), tmin: 0.01, tmax: depth };
    let lit = if scene.occluded(shadow, 0xff) { 0 } else { 1 };
    unsafe { acc.write(id.x as usize, inst * 2 + lit) };",
            true,
        ),
        expect![[r#"
            // language: metal2.4
            #include <metal_stdlib>
            #include <simd/simd.h>

            using metal::uint;

            struct _mslBufferSizes {
                uint size2;
                uint size3;
            };

            struct Globals {
                metal::float4 f0_;
                uint f1_;
                uint f2_;
                uint f3_;
                uint f4_;
            };
            typedef metal::int4 type_5[1];
            typedef uint type_6[1];
            struct Thread {
                metal::packed_uint3 f0_;
                metal::packed_uint3 f1_;
                metal::packed_uint3 f2_;
            };
            struct Ray {
                metal::packed_float3 f0_;
                metal::packed_float3 f1_;
                float f2_;
                float f3_;
            };
            struct RayDesc {
                uint flags;
                uint cull_mask;
                float tmin;
                float tmax;
                metal::float3 origin;
                metal::float3 dir;
            };
            struct RayIntersection {
                uint kind;
                float t;
                uint instance_custom_data;
                uint instance_index;
                uint sbt_record_offset;
                uint geometry_index;
                uint primitive_index;
                metal::float2 barycentrics;
                bool front_face;
                char _pad9[11];
                metal::float4x3 object_to_world;
                metal::float4x3 world_to_object;
            };
            struct Committed {
                uint f0_;
                float f1_;
                uint f2_;
                uint f3_;
                uint f4_;
                uint f5_;
                metal::float2 f6_;
                bool f7_;
                char _pad8[3];
            };
            struct Hit {
                float f0_;
                uint f1_;
                uint f2_;
                uint f3_;
                uint f4_;
                metal::float2 f5_;
                bool f6_;
                char _pad7[3];
            };
            struct type_14 {
                int tag;
                char _pad1[12];
                Hit v1_0_;
            };
            struct type_15 {
                float f0_;
                uint f1_;
            };
            struct type_16 {
                uint f0_;
                bool f1_;
                char _pad2[3];
            };
            RayIntersection ray_query_get_intersection_true(metal::raytracing::intersection_query<metal::raytracing::instancing, metal::raytracing::triangle_data> intersector
            , uint intersector_tracker
            ) {
                RayIntersection intersection = RayIntersection {};
                if (((intersector_tracker & 4) == 4)) {
                    metal::raytracing::intersection_type ty = intersector.get_committed_intersection_type();
                    if (ty == metal::raytracing::intersection_type::triangle) {
                        intersection.kind = 1;
                        intersection.barycentrics = intersector.get_committed_triangle_barycentric_coord();
                        intersection.front_face = intersector.is_committed_triangle_front_facing();
                    } else if (ty == metal::raytracing::intersection_type::bounding_box) {
                        intersection.kind = 2;
                    }
                    if (ty != metal::raytracing::intersection_type::none) {
                        intersection.t = intersector.get_committed_distance();
                        intersection.instance_custom_data = intersector.get_committed_user_instance_id();
                        intersection.instance_index = intersector.get_committed_instance_id();
                        intersection.geometry_index = intersector.get_committed_geometry_id();
                        intersection.primitive_index = intersector.get_committed_primitive_id();
                        intersection.object_to_world = intersector.get_committed_object_to_world_transform();
                        intersection.world_to_object = intersector.get_committed_world_to_object_transform();
                    }
                }
                return intersection;
            }
            uint naga_div(uint lhs, uint rhs) {
                return lhs / metal::select(rhs, 1u, rhs == 0u);
            }


            struct csInput {
            };
            [[max_total_threads_per_threadgroup(64)]] kernel void cs(
              metal::uint3 t_global [[thread_position_in_grid]]
            , metal::uint3 t_local [[thread_position_in_threadgroup]]
            , metal::uint3 t_group [[threadgroup_position_in_grid]]
            , constant Globals& cs_g [[buffer(0)]]
            , metal::raytracing::instance_acceleration_structure cs_scene [[buffer(1)]]
            , device type_6& cs_acc [[buffer(3)]]
            , constant _mslBufferSizes& _buffer_sizes [[buffer(4)]]
            ) {
                Thread cs_1_ = Thread {};
                metal::uint3 global_0_ = metal::uint3 {};
                metal::uint3 cs_6_ = metal::uint3 {};
                uint cs_10_ = uint {};
                float cs_9_ = float {};
                uint cs_12_ = uint {};
                float cs_11_ = float {};
                float vec3_1_ = float {};
                float vec3_2_ = float {};
                float vec3_3_ = float {};
                float vec3_4_ = float {};
                float vec3_5_ = float {};
                float vec3_6_ = float {};
                metal::float3 vec3_0_ = metal::float3 {};
                metal::float3 cs_8_ = metal::float3 {};
                float cs_14_ = float {};
                float vec3_1_1 = float {};
                float vec3_2_1 = float {};
                float vec3_3_1 = float {};
                float vec3_4_1 = float {};
                float vec3_5_1 = float {};
                float vec3_6_1 = float {};
                metal::float3 vec3_0_1 = metal::float3 {};
                metal::float3 cs_13_ = metal::float3 {};
                metal::float3 cs_21_ = metal::float3 {};
                metal::float3 cs_22_ = metal::float3 {};
                Ray cs_20_ = Ray {};
                Ray trace_2_ = Ray {};
                uint trace_3_ = uint {};
                Ray trace_6_ = Ray {};
                uint trace_7_ = uint {};
                metal::raytracing::intersection_query<metal::raytracing::instancing, metal::raytracing::triangle_data> query = {};
                uint naga_query_init_tracker_for_query = 0u;
                float naga_query_tmax_tracker_for_query = 0.0;
                Committed trace_4_ = Committed {};
                uint trace_9_ = uint {};
                bool trace_8_ = bool {};
                type_14 trace_0_ = type_14 {};
                float trace_11_ = float {};
                uint trace_12_ = uint {};
                uint trace_13_ = uint {};
                uint trace_14_ = uint {};
                uint trace_15_ = uint {};
                metal::float2 trace_16_ = metal::float2 {};
                bool trace_17_ = bool {};
                Hit trace_10_ = Hit {};
                type_14 cs_18_ = type_14 {};
                int cs_23_ = int {};
                uint cs_30_ = uint {};
                bool cs_29_ = bool {};
                float cs_31_ = float {};
                type_15 cs_17_ = type_15 {};
                uint flag = 0u;
                Hit cs_24_ = Hit {};
                float cs_25_ = float {};
                uint cs_27_ = uint {};
                uint cs_28_ = uint {};
                type_16 cs_54_ = type_16 {};
                uint cs_26_ = uint {};
                float cs_15_ = float {};
                uint cs_16_ = uint {};
                float cs_35_ = float {};
                float cs_36_ = float {};
                float cs_38_ = float {};
                float cs_39_ = float {};
                float cs_37_ = float {};
                float vec3_1_2 = float {};
                float vec3_2_2 = float {};
                float vec3_3_2 = float {};
                float vec3_4_2 = float {};
                float vec3_5_2 = float {};
                float vec3_6_2 = float {};
                metal::float3 vec3_0_2 = metal::float3 {};
                metal::float3 cs_34_ = metal::float3 {};
                float vec3_1_3 = float {};
                float vec3_2_3 = float {};
                float vec3_3_3 = float {};
                float vec3_4_3 = float {};
                float vec3_5_3 = float {};
                float vec3_6_3 = float {};
                metal::float3 vec3_0_3 = metal::float3 {};
                metal::float3 cs_40_ = metal::float3 {};
                float cs_41_ = float {};
                Ray cs_33_ = Ray {};
                Ray cs_45_ = Ray {};
                Ray occluded_2_ = Ray {};
                uint occluded_3_ = uint {};
                Ray occluded_7_ = Ray {};
                uint occluded_8_ = uint {};
                metal::raytracing::intersection_query<metal::raytracing::instancing, metal::raytracing::triangle_data> query_1 = {};
                uint naga_query_init_tracker_for_query_1 = 0u;
                float naga_query_tmax_tracker_for_query_1 = 0.0;
                Committed occluded_5_ = Committed {};
                uint occluded_4_ = uint {};
                bool occluded_0_ = bool {};
                bool cs_43_ = bool {};
                uint cs_42_ = uint {};
                uint cs_49_ = uint {};
                uint cs_48_ = uint {};
                uint cs_52_ = uint {};
                type_16 cs_55_ = type_16 {};
                uint cs_51_ = uint {};
                uint cs_53_ = uint {};
                type_16 cs_56_ = type_16 {};
                uint cs_50_ = uint {};
                uint write_2_ = uint {};
                uint write_3_ = uint {};
                bool write_13_ = bool {};
                uint write_6_ = uint {};
                uint len_0_ = uint {};
                uint write_7_ = uint {};
                bool write_5_ = bool {};
                uint write_11_ = uint {};
                uint write_12_ = uint {};
                cs_1_ = Thread {t_global, t_local, t_group};
                metal::uint3 _e8 = cs_1_.f0_;
                global_0_ = _e8;
                metal::uint3 _e12 = global_0_;
                cs_6_ = _e12;
                uint _e17 = cs_6_.x;
                cs_10_ = _e17;
                uint _e21 = cs_10_;
                cs_9_ = static_cast<float>(_e21);
                uint _e27 = cs_6_.y;
                cs_12_ = _e27;
                uint _e31 = cs_12_;
                cs_11_ = static_cast<float>(_e31);
                float _e36 = cs_9_;
                vec3_1_ = _e36;
                float _e40 = cs_11_;
                vec3_2_ = _e40;
                vec3_3_ = 100.0;
                float _e47 = vec3_1_;
                vec3_4_ = _e47;
                float _e51 = vec3_2_;
                vec3_5_ = _e51;
                float _e55 = vec3_3_;
                vec3_6_ = _e55;
                float _e59 = vec3_4_;
                float _e61 = vec3_5_;
                float _e63 = vec3_6_;
                vec3_0_ = metal::float3(_e59, _e61, _e63);
                metal::float3 _e68 = vec3_0_;
                cs_8_ = _e68;
                cs_14_ = -(1.0);
                vec3_1_1 = 0.0;
                vec3_2_1 = 0.0;
                float _e82 = cs_14_;
                vec3_3_1 = _e82;
                float _e86 = vec3_1_1;
                vec3_4_1 = _e86;
                float _e90 = vec3_2_1;
                vec3_5_1 = _e90;
                float _e94 = vec3_3_1;
                vec3_6_1 = _e94;
                float _e98 = vec3_4_1;
                float _e100 = vec3_5_1;
                float _e102 = vec3_6_1;
                vec3_0_1 = metal::float3(_e98, _e100, _e102);
                metal::float3 _e107 = vec3_0_1;
                cs_13_ = _e107;
                metal::float3 _e111 = cs_8_;
                cs_21_ = _e111;
                metal::float3 _e115 = cs_13_;
                cs_22_ = _e115;
                metal::float3 _e119 = cs_21_;
                metal::float3 _e121 = cs_22_;
                cs_20_ = Ray {_e119, _e121, 0.0, 10000000.0};
                Ray _e128 = cs_20_;
                trace_2_ = _e128;
                trace_3_ = 255u;
                Ray _e135 = trace_2_;
                trace_6_ = _e135;
                uint _e139 = trace_3_;
                trace_7_ = _e139;
                Ray _e144 = trace_6_;
                uint _e147 = trace_7_;
                RayDesc _e152 = RayDesc {1u, _e147, _e144.f2_, _e144.f3_, _e144.f0_, _e144.f1_};
                {
                    RayDesc desc = _e152;
                    metal::raytracing::intersection_params params;
                    metal::raytracing::opacity_cull_mode cull_mode = 
                        (desc.flags & 64) != 0 ? metal::raytracing::opacity_cull_mode::opaque : (
                            (desc.flags & 128) != 0 ? metal::raytracing::opacity_cull_mode::non_opaque : metal::raytracing::opacity_cull_mode::none
                        );
                    params.set_opacity_cull_mode(cull_mode);
                    bool force_opacity = cull_mode == metal::raytracing::opacity_cull_mode::none;
                    if (force_opacity) {
                        params.force_opacity(
                            (desc.flags & 1) != 0 ? metal::raytracing::forced_opacity::opaque : (
                                (desc.flags & 2) != 0 ? metal::raytracing::forced_opacity::non_opaque : metal::raytracing::forced_opacity::none
                            )
                        );
                    }
                    params.accept_any_intersection((desc.flags & 4) != 0);
                    metal::raytracing::ray ray = metal::raytracing::ray(desc.origin, desc.dir, desc.tmin, desc.tmax);
                    bool invalid_nan_infs = ((as_type<uint>(desc.origin.x) & 2139095040) == 2139095040) || ((as_type<uint>(desc.origin.y) & 2139095040) == 2139095040) || ((as_type<uint>(desc.origin.z) & 2139095040) == 2139095040) || ((as_type<uint>(desc.dir.x) & 2139095040) == 2139095040) || ((as_type<uint>(desc.dir.y) & 2139095040) == 2139095040) || ((as_type<uint>(desc.dir.z) & 2139095040) == 2139095040) || ((as_type<uint>(desc.tmin) & 2139095040) == 2139095040) || (((as_type<uint>(desc.tmax) & 2139095040) == 2139095040) && ((as_type<uint>(desc.tmax) & 0x7fffff) != 0));
                    bool invalid_t = (desc.tmin > desc.tmax) || (desc.tmin < 0.0);
                    bool invalid_dir = metal::all(metal::abs(desc.dir) == 0.0);
                    if (!(invalid_dir || invalid_t || invalid_nan_infs)) {
                        query.reset(ray,cs_scene, desc.cull_mask, params);
                        naga_query_init_tracker_for_query = 1;
                        naga_query_tmax_tracker_for_query = desc.tmax;
                    }
                }
                uint2 loop_bound = uint2(4294967295u);
                while(true) {
                    if (metal::all(loop_bound == uint2(0u))) { break; }
                    loop_bound -= uint2(loop_bound.y == 0u, 1u);
                    bool _e155 = false;
                    if (((naga_query_init_tracker_for_query & 1) == 1) && !((naga_query_init_tracker_for_query & 4) == 4)) {
                        _e155 = query.next();
                        naga_query_init_tracker_for_query = naga_query_init_tracker_for_query | (_e155 ? 2: 6);
                    }
                    if (!(_e155)) {
                        break;
                    }
                }
                RayIntersection _e158 = ray_query_get_intersection_true(query, naga_query_init_tracker_for_query);
                trace_4_ = Committed {_e158.kind, _e158.t, _e158.instance_custom_data, _e158.instance_index, _e158.geometry_index, _e158.primitive_index, _e158.barycentrics, _e158.front_face};
                uint _e172 = trace_4_.f0_;
                trace_9_ = _e172;
                uint _e176 = trace_9_;
                trace_8_ = _e176 == 1u;
                bool _e182 = trace_8_;
                if (_e182) {
                    float _e190 = trace_4_.f1_;
                    trace_11_ = _e190;
                    uint _e195 = trace_4_.f3_;
                    trace_12_ = _e195;
                    uint _e200 = trace_4_.f2_;
                    trace_13_ = _e200;
                    uint _e205 = trace_4_.f4_;
                    trace_14_ = _e205;
                    uint _e210 = trace_4_.f5_;
                    trace_15_ = _e210;
                    metal::float2 _e215 = trace_4_.f6_;
                    trace_16_ = _e215;
                    bool _e220 = trace_4_.f7_;
                    trace_17_ = _e220;
                    float _e224 = trace_11_;
                    uint _e226 = trace_12_;
                    uint _e228 = trace_13_;
                    uint _e230 = trace_14_;
                    uint _e232 = trace_15_;
                    metal::float2 _e234 = trace_16_;
                    bool _e236 = trace_17_;
                    trace_10_ = Hit {_e224, _e226, _e228, _e230, _e232, _e234, _e236};
                    Hit _e242 = trace_10_;
                    trace_0_ = type_14 {1, {}, _e242};
                } else {
                    trace_0_ = type_14 {0, {}, Hit {}};
                }
                type_14 _e246 = trace_0_;
                cs_18_ = _e246;
                uint2 loop_bound_1 = uint2(4294967295u);
                while(true) {
                    if (metal::all(loop_bound_1 == uint2(0u))) { break; }
                    loop_bound_1 -= uint2(loop_bound_1.y == 0u, 1u);
                    uint2 loop_bound_2 = uint2(4294967295u);
                    while(true) {
                        if (metal::all(loop_bound_2 == uint2(0u))) { break; }
                        loop_bound_2 -= uint2(loop_bound_2.y == 0u, 1u);
                        int _e251 = cs_18_.tag;
                        cs_23_ = _e251;
                        int _e255 = cs_23_;
                        if (_e255 == 1) {
                            Hit _e302 = cs_18_.v1_0_;
                            cs_24_ = _e302;
                            float _e307 = cs_24_.f0_;
                            cs_25_ = _e307;
                            uint _e312 = cs_24_.f1_;
                            cs_27_ = _e312;
                            uint _e317 = cs_24_.f4_;
                            cs_28_ = _e317;
                            uint _e321 = cs_27_;
                            uint _e323 = cs_28_;
                            uint _e324 = _e321 + _e323;
                            cs_54_ = type_16 {_e324, _e324 < _e321};
                            bool _e332 = cs_54_.f1_;
                            if (!(_e332)) {
                                uint _e338 = cs_54_.f0_;
                                cs_26_ = _e338;
                                float _e342 = cs_25_;
                                uint _e344 = cs_26_;
                                cs_17_ = type_15 {_e342, _e344};
                            } else {
                                flag = 3u;
                                break;
                            }
                        } else {
                            int _e260 = cs_18_.tag;
                            cs_23_ = _e260;
                            int _e263 = cs_23_;
                            if (_e263 == 0) {
                                uint _e268 = cs_g.f1_;
                                cs_30_ = _e268;
                                uint _e272 = cs_30_;
                                cs_29_ = _e272 > 0u;
                                bool _e278 = cs_29_;
                                if (_e278) {
                                    cs_31_ = -(1.0);
                                    float _e284 = cs_31_;
                                    cs_17_ = type_15 {_e284, 0u};
                                    break;
                                }
                            }
                            int _e291 = cs_18_.tag;
                            cs_23_ = _e291;
                            int _e294 = cs_23_;
                            if (_e294 == 0) {
                                return;
                            } else {
                                flag = 3u;
                                break;
                            }
                        }
                        break;
                    }
                    uint _e348 = flag;
                    if (_e348 != 0u) {
                        uint _e352 = flag;
                        if (_e352 == 3u) {
                            flag = 0u;
                            break;
                        }
                        break;
                    }
                    float _e359 = cs_17_.f0_;
                    cs_15_ = _e359;
                    uint _e364 = cs_17_.f1_;
                    cs_16_ = _e364;
                    float _e369 = cs_8_.x;
                    cs_35_ = _e369;
                    float _e374 = cs_8_.y;
                    cs_36_ = _e374;
                    float _e379 = cs_8_.z;
                    cs_38_ = _e379;
                    float _e383 = cs_15_;
                    cs_39_ = _e383;
                    float _e387 = cs_38_;
                    float _e389 = cs_39_;
                    cs_37_ = _e387 - _e389;
                    float _e394 = cs_35_;
                    vec3_1_2 = _e394;
                    float _e398 = cs_36_;
                    vec3_2_2 = _e398;
                    float _e402 = cs_37_;
                    vec3_3_2 = _e402;
                    float _e406 = vec3_1_2;
                    vec3_4_2 = _e406;
                    float _e410 = vec3_2_2;
                    vec3_5_2 = _e410;
                    float _e414 = vec3_3_2;
                    vec3_6_2 = _e414;
                    float _e418 = vec3_4_2;
                    float _e420 = vec3_5_2;
                    float _e422 = vec3_6_2;
                    vec3_0_2 = metal::float3(_e418, _e420, _e422);
                    metal::float3 _e427 = vec3_0_2;
                    cs_34_ = _e427;
                    vec3_1_3 = 0.0;
                    vec3_2_3 = 0.0;
                    vec3_3_3 = 1.0;
                    float _e440 = vec3_1_3;
                    vec3_4_3 = _e440;
                    float _e444 = vec3_2_3;
                    vec3_5_3 = _e444;
                    float _e448 = vec3_3_3;
                    vec3_6_3 = _e448;
                    float _e452 = vec3_4_3;
                    float _e454 = vec3_5_3;
                    float _e456 = vec3_6_3;
                    vec3_0_3 = metal::float3(_e452, _e454, _e456);
                    metal::float3 _e461 = vec3_0_3;
                    cs_40_ = _e461;
                    float _e465 = cs_15_;
                    cs_41_ = _e465;
                    metal::float3 _e469 = cs_34_;
                    metal::float3 _e471 = cs_40_;
                    float _e474 = cs_41_;
                    cs_33_ = Ray {_e469, _e471, 0.01, _e474};
                    Ray _e479 = cs_33_;
                    cs_45_ = _e479;
                    Ray _e483 = cs_45_;
                    occluded_2_ = _e483;
                    occluded_3_ = 255u;
                    Ray _e490 = occluded_2_;
                    occluded_7_ = _e490;
                    uint _e494 = occluded_3_;
                    occluded_8_ = _e494;
                    Ray _e499 = occluded_7_;
                    uint _e502 = occluded_8_;
                    RayDesc _e507 = RayDesc {5u, _e502, _e499.f2_, _e499.f3_, _e499.f0_, _e499.f1_};
                    {
                        RayDesc desc = _e507;
                        metal::raytracing::intersection_params params;
                        metal::raytracing::opacity_cull_mode cull_mode = 
                            (desc.flags & 64) != 0 ? metal::raytracing::opacity_cull_mode::opaque : (
                                (desc.flags & 128) != 0 ? metal::raytracing::opacity_cull_mode::non_opaque : metal::raytracing::opacity_cull_mode::none
                            );
                        params.set_opacity_cull_mode(cull_mode);
                        bool force_opacity = cull_mode == metal::raytracing::opacity_cull_mode::none;
                        if (force_opacity) {
                            params.force_opacity(
                                (desc.flags & 1) != 0 ? metal::raytracing::forced_opacity::opaque : (
                                    (desc.flags & 2) != 0 ? metal::raytracing::forced_opacity::non_opaque : metal::raytracing::forced_opacity::none
                                )
                            );
                        }
                        params.accept_any_intersection((desc.flags & 4) != 0);
                        metal::raytracing::ray ray = metal::raytracing::ray(desc.origin, desc.dir, desc.tmin, desc.tmax);
                        bool invalid_nan_infs = ((as_type<uint>(desc.origin.x) & 2139095040) == 2139095040) || ((as_type<uint>(desc.origin.y) & 2139095040) == 2139095040) || ((as_type<uint>(desc.origin.z) & 2139095040) == 2139095040) || ((as_type<uint>(desc.dir.x) & 2139095040) == 2139095040) || ((as_type<uint>(desc.dir.y) & 2139095040) == 2139095040) || ((as_type<uint>(desc.dir.z) & 2139095040) == 2139095040) || ((as_type<uint>(desc.tmin) & 2139095040) == 2139095040) || (((as_type<uint>(desc.tmax) & 2139095040) == 2139095040) && ((as_type<uint>(desc.tmax) & 0x7fffff) != 0));
                        bool invalid_t = (desc.tmin > desc.tmax) || (desc.tmin < 0.0);
                        bool invalid_dir = metal::all(metal::abs(desc.dir) == 0.0);
                        if (!(invalid_dir || invalid_t || invalid_nan_infs)) {
                            query_1.reset(ray,cs_scene, desc.cull_mask, params);
                            naga_query_init_tracker_for_query_1 = 1;
                            naga_query_tmax_tracker_for_query_1 = desc.tmax;
                        }
                    }
                    uint2 loop_bound_3 = uint2(4294967295u);
                    while(true) {
                        if (metal::all(loop_bound_3 == uint2(0u))) { break; }
                        loop_bound_3 -= uint2(loop_bound_3.y == 0u, 1u);
                        bool _e510 = false;
                        if (((naga_query_init_tracker_for_query_1 & 1) == 1) && !((naga_query_init_tracker_for_query_1 & 4) == 4)) {
                            _e510 = query_1.next();
                            naga_query_init_tracker_for_query_1 = naga_query_init_tracker_for_query_1 | (_e510 ? 2: 6);
                        }
                        if (!(_e510)) {
                            break;
                        }
                    }
                    RayIntersection _e513 = ray_query_get_intersection_true(query_1, naga_query_init_tracker_for_query_1);
                    occluded_5_ = Committed {_e513.kind, _e513.t, _e513.instance_custom_data, _e513.instance_index, _e513.geometry_index, _e513.primitive_index, _e513.barycentrics, _e513.front_face};
                    uint _e527 = occluded_5_.f0_;
                    occluded_4_ = _e527;
                    uint _e531 = occluded_4_;
                    occluded_0_ = _e531 != 0u;
                    bool _e537 = occluded_0_;
                    cs_43_ = _e537;
                    bool _e541 = cs_43_;
                    if (_e541) {
                        cs_42_ = 0u;
                    } else {
                        cs_42_ = 1u;
                    }
                    uint _e549 = cs_6_.x;
                    cs_49_ = _e549;
                    uint _e553 = cs_49_;
                    cs_48_ = _e553;
                    uint _e557 = cs_16_;
                    cs_52_ = _e557;
                    uint _e561 = cs_52_;
                    uint _e563 = _e561 * 2u;
                    bool _e565 = _e561 != 0u;
                    cs_55_ = type_16 {_e563, _e565 && (naga_div(_e563, _e565 ? _e561 : 1u) != 2u)};
                    bool _e576 = cs_55_.f1_;
                    if (!(_e576)) {
                        uint _e580 = cs_55_.f0_;
                        cs_51_ = _e580;
                        uint _e584 = cs_42_;
                        cs_53_ = _e584;
                        uint _e588 = cs_51_;
                        uint _e590 = cs_53_;
                        uint _e591 = _e588 + _e590;
                        cs_56_ = type_16 {_e591, _e591 < _e588};
                        bool _e599 = cs_56_.f1_;
                        if (!(_e599)) {
                            uint _e603 = cs_56_.f0_;
                            cs_50_ = _e603;
                            uint _e607 = cs_48_;
                            write_2_ = _e607;
                            uint _e611 = cs_50_;
                            write_3_ = _e611;
                            write_13_ = false;
                            write_13_ = true;
                            uint _e620 = write_2_;
                            write_6_ = _e620;
                            len_0_ = 1 + (_buffer_sizes.size3 - 0 - 4) / 4;
                            uint _e628 = len_0_;
                            write_7_ = _e628;
                            uint _e632 = write_6_;
                            uint _e634 = write_7_;
                            write_5_ = _e632 >= _e634;
                            bool _e639 = write_5_;
                            if (_e639) {
                            } else {
                                uint _e641 = write_2_;
                                write_11_ = _e641;
                                uint _e645 = write_3_;
                                write_12_ = _e645;
                                write_13_ = false;
                                uint _e652 = write_11_;
                                uint _e655 = write_12_;
                                cs_acc[_e652] = _e655;
                                write_13_ = false;
                                write_13_ = false;
                                return;
                            }
                        }
                    }
                    break;
                }
                return;
                return;
            }
        "#]],
    );
}

#[test]
fn a_derivative_in_a_kernel_is_refused() {
    // The same call in a fragment stage lowers: `derivatives_and_discard_in_a_fragment`.
    refused(&kernel("", "    let x = gpu::intrinsic::dpdx(t.global().x as f32); unsafe { acc.write(0, x as u32) };", false), "a kernel", why::STAGE);
}

#[test]
fn a_discard_in_a_kernel_is_refused() {
    // The same call in a fragment stage lowers: `derivatives_and_discard_in_a_fragment`.
    refused(&kernel("", "    if t.global().x > 3 { gpu::intrinsic::discard(); }", false), "in a kernel", why::STAGE);
}

/// A RawOut whose descriptor's stride is not its type's is refused, naming both; the kernels
/// above, whose strides agree, are the positive control.
#[test]
fn a_raw_out_with_the_wrong_stride_is_refused() {
    let source = kernel("", "    let _ = t.global();", false).replace("Kind::RawOut { stride: 4 }", "Kind::RawOut { stride: 8 }");
    refused(&source, "stride of 8", why::INTERFACE);
}
