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

/// The sizes of an entry's storage buffers go in the buffer slot after its last buffer, and an
/// entry without a storage buffer has none.
#[test]
fn storage_sizes_take_the_next_buffer_slot() {
    assert_eq!(quad_vs::ENTRY.sizes, Some(Slot { table: Table::Buffer, index: 2 }));
    assert_eq!(quad_fs::ENTRY.sizes, Some(Slot { table: Table::Buffer, index: 2 }));
    assert_eq!(uniform_only_fs::ENTRY.sizes, None);
}

#[crate::fragment]
fn uniform_only_fs(v: V, g: &Globals) -> Vec4 {
    g.tint * v.uv.y
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

/// `asuint` is the bit pattern: bit 31 the sign (set for -0, which `<` can't see), the exponent and
/// mantissa below it, so it round-trips through `f32::from_bits` exactly.
#[hegel::test]
fn asuint_is_the_bits(tc: hegel::TestCase) {
    use hegel::generators as gs;
    let x = tc.draw(gs::floats::<f32>().allow_nan(false));
    let u = asuint(x);
    assert_eq!(f32::from_bits(u), x);
    assert_eq!(u >> 31, u32::from(x.is_sign_negative()), "{x}'s sign bit");
    assert_eq!(asuint(1.0), 127 << 23);
    assert_eq!(asuint(-0.0) >> 31, 1);
    assert_eq!(asuint(0.0), 0);
}

/// `frac` is what `floor` leaves, 0 to 1 also for negatives; `sign` is -1, 0 or 1 and 0 at 0 and
/// NaN (where `signum` would say 1 and NaN), per component on a vector.
#[hegel::test]
fn frac_and_sign_are_slangs(tc: hegel::TestCase) {
    use hegel::generators as gs;
    let x = tc.draw(gs::floats::<f32>().min_value(-1e4).max_value(1e4).allow_nan(false).allow_infinity(false));
    let f = frac(x);
    assert!((0.0..=1.0).contains(&f), "frac({x}) = {f}");
    assert_eq!(x - f, x.floor());
    assert_eq!(frac(-0.25), 0.75);
    let s = sign(x);
    assert_eq!(s, if x > 0.0 { 1.0 } else if x < 0.0 { -1.0 } else { 0.0 });
    assert_eq!((sign(0.0), sign(-0.0), sign(f32::NAN)), (0.0, 0.0, 0.0));
    assert_eq!(vec3(x, 0.0, -2.0).sign(), vec3(s, 0.0, -1.0));
}

/// `lerp` with a vector `t` blends each component by its own `t`, as the scalar `lerp` would.
#[hegel::test]
fn lerp_by_a_vector_is_per_component(tc: hegel::TestCase) {
    use hegel::generators as gs;
    let f = |lo: f32, hi: f32| gs::floats::<f32>().min_value(lo).max_value(hi).allow_nan(false).allow_infinity(false);
    let (a, b) = (vec2(tc.draw(f(-1e3, 1e3)), tc.draw(f(-1e3, 1e3))), vec2(tc.draw(f(-1e3, 1e3)), tc.draw(f(-1e3, 1e3))));
    let t = vec2(tc.draw(f(0.0, 1.0)), tc.draw(f(0.0, 1.0)));
    assert_eq!(lerp(a, b, t), vec2(lerp(a.x, b.x, t.x), lerp(a.y, b.y, t.y)));
    assert_eq!(lerp(a, b, vec2(0.0, 0.0)), a);
}

/// Float to integer vectors go toward zero (Slang's `int2(v)`), back exactly; `clamp` on an integer
/// vector is `max(lo)` then `min(hi)`, so an empty range gives `hi`, as Slang's.
#[test]
fn integer_vectors_convert_and_clamp() {
    assert_eq!(vec2(1.9, -1.9).as_ivec2(), ivec2(1, -1));
    assert_eq!(vec2(-0.5, 0.5).floor().as_ivec2(), ivec2(-1, 0));
    assert_eq!(ivec2(3, -4).as_vec2(), vec2(3.0, -4.0));
    assert_eq!(ivec2(-3, 9).clamp(ivec2(0, 0), ivec2(5, 5)), ivec2(0, 5));
    assert_eq!(ivec2(2, 2).clamp(ivec2(0, 0), ivec2(-1, -1)), ivec2(-1, -1));
    let r = ivec4(1, 2, 3, 4);
    assert_eq!((r.xy(), r.zw(), r.xy().yx()), (ivec2(1, 2), ivec2(3, 4), ivec2(2, 1)));
    assert_eq!(vec2(1.0, 2.0).extend(3.0).extend(4.0), vec4(1.0, 2.0, 3.0, 4.0));
    assert_eq!(size_of::<IVec4>(), 16);
}
