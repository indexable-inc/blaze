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
    pub struct IVec4 { pub x: i32, pub y: i32, pub z: i32, pub w: i32 }
    pub const fn vec2(x: f32, y: f32) -> Vec2 { Vec2 { x, y } }
    pub const fn vec4(x: f32, y: f32, z: f32, w: f32) -> Vec4 { Vec4 { x, y, z, w } }
    impl Vec4 {
        pub const fn xy(self) -> Vec2 { Vec2 { x: self.x, y: self.y } }
    }
}
pub mod intrinsic {
    pub fn dpdx<T: Copy>(v: T) -> T { v }
    pub fn discard() -> ! { loop {} }
}
pub mod entry {
    use crate::vec::{Vec2, Vec4};
    #[derive(Copy, Clone)] pub enum Stage { Vertex, Fragment }
    #[derive(Copy, Clone)] pub enum ValueTy { F32, Vec2, Vec3, Vec4, U32, UVec4, I32, IVec2 }
    #[derive(Copy, Clone)] pub enum Interp { Perspective, Linear, Flat }
    #[derive(Copy, Clone)] pub enum FieldKind { Position, Value { ty: ValueTy, interp: Interp } }
    #[derive(Copy, Clone)] pub struct Field { pub name: &'static str, pub kind: FieldKind, pub location: Option<u32> }
    #[derive(Copy, Clone)] pub enum Kind { VertexIndex, InstanceIndex, Varyings(&'static [Field]), Uniform { size: u32 }, Storage { stride: u32 }, Texture2d, Sampler }
    #[derive(Copy, Clone)] pub enum Out { Varyings(&'static [Field]), Color }
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
        assert_eq!(r.entry, if main.contains("pub fn vs(") { "vs" } else { "fs" }, "{r}");
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
        expect![[r#""#]],
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
        expect![[r#""#]],
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
        expect![[r#""#]],
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
        expect![[r#""#]],
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
        expect![[r#""#]],
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
