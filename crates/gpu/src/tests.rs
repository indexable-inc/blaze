// The fixtures here are read only through their descriptors.
#![allow(dead_code)]


use crate::prelude::*;
use crate::{Field, FieldKind, Interp, Kind, Out, Slot, Stage, Table, ValueTy};

#[repr(C)]
#[derive(Copy, Clone, crate::Pod)]
struct Globals {
    view: Vec4,
    tint: Vec4,
}

#[repr(C)]
#[derive(Copy, Clone, crate::Pod)]
struct Inst {
    rect: Vec4,
    info: UVec4,
}

#[derive(Copy, Clone, crate::Varyings)]
struct V {
    uv: Vec2,
    pos: Position,
    id: Flat<u32>,
    edge: Linear<Vec2>,
}

#[crate::vertex]
fn quad_vs(vid: VertexIndex, iid: InstanceIndex, g: &Globals, insts: &[Inst]) -> V {
    let r = insts[iid.get() as usize].rect;
    let c = vec2((vid.get() & 1) as f32, (vid.get() >> 1) as f32);
    let p = r.xy() + c * r.zw();
    V { uv: c, pos: Position(vec4(p.x / g.view.x, p.y / g.view.y, 0.0, 1.0)), id: Flat(iid.get()), edge: Linear(c) }
}

#[crate::fragment]
fn quad_fs(v: V, insts: &[Inst], g: &Globals, tex: &Texture2d, smp: &Sampler, mask: &Texture2d) -> Vec4 {
    let _ = (insts, tex, smp, mask);
    g.tint * v.uv.x
}

/// Slots are counted per entry point and per table, in parameter order; built-ins and the
/// varyings take none. This is the rule the host binds by and the device backend numbers by.
#[test]
fn slots_count_per_entry_and_table_in_parameter_order() {
    let slot = |table, index| Slot { table, index };
    let none = slot(Table::None, 0);
    assert_eq!(quad_vs::vid, none);
    assert_eq!(quad_vs::iid, none);
    assert_eq!(quad_vs::g, slot(Table::Buffer, 0));
    assert_eq!(quad_vs::insts, slot(Table::Buffer, 1));
    // The fragment stage starts again at 0: its first buffer is 0, whatever the vertex stage bound.
    assert_eq!(quad_fs::v, none);
    assert_eq!(quad_fs::insts, slot(Table::Buffer, 0));
    assert_eq!(quad_fs::g, slot(Table::Buffer, 1));
    assert_eq!(quad_fs::tex, slot(Table::Texture, 0));
    assert_eq!(quad_fs::smp, slot(Table::Sampler, 0));
    assert_eq!(quad_fs::mask, slot(Table::Texture, 1));
    assert_eq!(quad_fs::ENTRY.param("mask").map(|p| p.slot), Some(quad_fs::mask));
    assert_eq!(quad_fs::ENTRY.param("nothing"), None);
}

/// Each parameter's kind comes from its type: `&T` a uniform of T's size, `&[T]` storage of T's
/// stride, the built-ins and the varyings as themselves.
#[test]
fn kinds_come_from_the_types() {
    let kinds: Vec<Kind> = quad_vs::ENTRY.params.iter().map(|p| p.kind).collect();
    assert_eq!(
        kinds,
        [Kind::VertexIndex, Kind::InstanceIndex, Kind::Uniform { size: 32 }, Kind::Storage { stride: 32 }]
    );
    assert_eq!(quad_vs::ENTRY.stage, Stage::Vertex);
    assert_eq!(quad_fs::ENTRY.stage, Stage::Fragment);
    assert_eq!(quad_fs::ENTRY.out, Out::Color);
    assert_eq!(quad_fs::ENTRY.params[0].kind, Kind::Varyings(<V as crate::Varyings>::FIELDS));
}

/// Locations go to the values in field order, skipping the position; interpolation is the type.
#[test]
fn varyings_locations_skip_the_position() {
    let f = <V as crate::Varyings>::FIELDS;
    let value = |ty, interp| FieldKind::Value { ty, interp };
    assert_eq!(
        f,
        [
            Field { name: "uv", kind: value(ValueTy::Vec2, Interp::Perspective), location: Some(0) },
            Field { name: "pos", kind: FieldKind::Position, location: None },
            Field { name: "id", kind: value(ValueTy::U32, Interp::Flat), location: Some(1) },
            Field { name: "edge", kind: value(ValueTy::Vec2, Interp::Linear), location: Some(2) },
        ]
    );
    assert_eq!(quad_vs::ENTRY.out, Out::Varyings(f));
}

/// The entry points run on the host as plain Rust: the vertex stage's corner lands where the
/// instance's rect says.
#[test]
fn a_vertex_stage_runs_on_the_host() {
    let g = Globals { view: vec4(100.0, 50.0, 0.0, 0.0), tint: Vec4::splat(1.0) };
    let insts = [Inst { rect: vec4(10.0, 5.0, 20.0, 10.0), info: UVec4::default() }];
    let v = quad_vs(VertexIndex(3), InstanceIndex(0), &g, &insts);
    assert_eq!(v.pos.0, vec4(0.3, 0.3, 0.0, 1.0));
    assert_eq!(*v.id, 0);
    assert_eq!(v.uv, vec2(1.0, 1.0));
}

/// `smoothstep` is 0 and 1 at and past its edges, never leaves 0 to 1, and never decreases;
/// `lerp` is exact at its ends.
#[hegel::test]
fn smoothstep_and_lerp_keep_their_ends(tc: hegel::TestCase) {
    use hegel::generators as gs;
    let f = |lo: f32, hi: f32| gs::floats::<f32>().min_value(lo).max_value(hi).allow_nan(false).allow_infinity(false);
    let e0 = tc.draw(f(-100.0, 100.0));
    let e1 = e0 + tc.draw(f(1e-3, 100.0));
    let x = tc.draw(f(-300.0, 300.0));
    let dx = tc.draw(f(0.0, 50.0));
    let (y, y2) = (smoothstep(e0, e1, x), smoothstep(e0, e1, x + dx));
    assert!((0.0..=1.0).contains(&y), "smoothstep({e0}, {e1}, {x}) = {y}");
    assert!(y2 + 1e-6 >= y, "smoothstep decreased from {x} ({y}) to {} ({y2})", x + dx);
    assert_eq!(smoothstep(e0, e1, e0 - dx), 0.0);
    assert_eq!(smoothstep(e0, e1, e1 + dx), 1.0);
    let (a, b) = (tc.draw(f(-1e3, 1e3)), tc.draw(f(-1e3, 1e3)));
    assert_eq!(lerp(a, b, 0.0), a);
    assert_eq!(lerp(vec3(a, b, a), vec3(b, a, b), 0.0), vec3(a, b, a));
}
