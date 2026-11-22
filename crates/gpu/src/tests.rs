// The fixtures here are read only through their descriptors.
#![allow(dead_code)]

use blaze_sim::Config;

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
#[test]
fn smoothstep_and_lerp_keep_their_ends() {
    blaze_sim::check(Config::DEFAULT, |tape| {
        let e0 = tape.f32_in(-100.0..=100.0, "e0");
        let e1 = e0 + tape.f32_in(1e-3..=100.0, "e1 past e0");
        let x = tape.f32_in(-300.0..=300.0, "x");
        let dx = tape.f32_in(0.0..=50.0, "dx");
        let (y, y2) = (smoothstep(e0, e1, x), smoothstep(e0, e1, x + dx));
        assert!((0.0..=1.0).contains(&y), "smoothstep({e0}, {e1}, {x}) = {y}");
        assert!(y2 + 1e-6 >= y, "smoothstep decreased from {x} ({y}) to {} ({y2})", x + dx);
        assert_eq!(smoothstep(e0, e1, e0 - dx), 0.0);
        assert_eq!(smoothstep(e0, e1, e1 + dx), 1.0);
        let (a, b) = (tape.f32_in(-1e3..=1e3, "a"), tape.f32_in(-1e3..=1e3, "b"));
        assert_eq!(lerp(a, b, 0.0), a);
        assert_eq!(lerp(vec3(a, b, a), vec3(b, a, b), 0.0), vec3(a, b, a));
    });
}

/// `asuint` is the bit pattern: bit 31 the sign (set for -0, which `<` can't see), the exponent and
/// mantissa below it, so it round-trips through `f32::from_bits` exactly.
#[test]
fn asuint_is_the_bits() {
    blaze_sim::check(Config::DEFAULT, |tape| {
        // Any `f32` but NaN: the infinities stay in.
        let x = tape.f32("x");
        tape.assume(!x.is_nan());
        let u = asuint(x);
        assert_eq!(f32::from_bits(u), x);
        assert_eq!(u >> 31, u32::from(x.is_sign_negative()), "{x}'s sign bit");
        assert_eq!(asuint(1.0), 127 << 23);
        assert_eq!(asuint(-0.0) >> 31, 1);
        assert_eq!(asuint(0.0), 0);
    });
}

/// `frac` is what `floor` leaves, 0 to 1 also for negatives; `sign` is -1, 0 or 1 and 0 at 0 and
/// NaN (where `signum` would say 1 and NaN), per component on a vector.
#[test]
fn frac_and_sign_are_slangs() {
    blaze_sim::check(Config::DEFAULT, |tape| {
        let x = tape.f32_in(-1e4..=1e4, "x");
        let f = frac(x);
        assert!((0.0..=1.0).contains(&f), "frac({x}) = {f}");
        assert_eq!(x - f, x.floor());
        assert_eq!(frac(-0.25), 0.75);
        let s = sign(x);
        assert_eq!(s, if x > 0.0 { 1.0 } else if x < 0.0 { -1.0 } else { 0.0 });
        assert_eq!((sign(0.0), sign(-0.0), sign(f32::NAN)), (0.0, 0.0, 0.0));
        assert_eq!(vec3(x, 0.0, -2.0).sign(), vec3(s, 0.0, -1.0));
    });
}

/// `lerp` with a vector `t` blends each component by its own `t`, as the scalar `lerp` would.
#[test]
fn lerp_by_a_vector_is_per_component() {
    blaze_sim::check(Config::DEFAULT, |tape| {
        let a = vec2(tape.f32_in(-1e3..=1e3, "a.x"), tape.f32_in(-1e3..=1e3, "a.y"));
        let b = vec2(tape.f32_in(-1e3..=1e3, "b.x"), tape.f32_in(-1e3..=1e3, "b.y"));
        let t = vec2(tape.f32_in(0.0..=1.0, "t.x"), tape.f32_in(0.0..=1.0, "t.y"));
        assert_eq!(lerp(a, b, t), vec2(lerp(a.x, b.x, t.x), lerp(a.y, b.y, t.y)));
        assert_eq!(lerp(a, b, vec2(0.0, 0.0)), a);
    });
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

#[repr(C)]
#[derive(Copy, Clone, crate::Pod)]
struct Lamp {
    o: Vec4,
    light: Vec4,
}

/// A kernel in light.slang's shape: a uniform, the TLAS, read-only storage, and storage it writes.
#[crate::kernel(threads = (8, 8))]
fn light_cs(t: Thread, view: &Globals, scene: &Tlas, lamps: &[Lamp], cells: &[UVec2], acc: &RawOut<UVec2>) {
    let _ = (t, view, scene, lamps, cells, acc);
}

#[crate::kernel(threads = 64)]
fn flat_cs(t: Thread, out: &RawOut<u32>) {
    let _ = (t, out);
}

/// A kernel's buffers, the TLAS among them, take buffer slots in parameter order (wgpu's Metal
/// backend counts an acceleration structure with the buffers); its Thread takes none; and its
/// RawOut counts as storage, so the sizes buffer follows the last buffer.
#[test]
fn a_kernels_buffers_and_tlas_count_together() {
    let slot = |table, index| Slot { table, index };
    assert_eq!(light_cs::t, slot(Table::None, 0));
    assert_eq!(light_cs::view, slot(Table::Buffer, 0));
    assert_eq!(light_cs::scene, slot(Table::Buffer, 1));
    assert_eq!(light_cs::lamps, slot(Table::Buffer, 2));
    assert_eq!(light_cs::cells, slot(Table::Buffer, 3));
    assert_eq!(light_cs::acc, slot(Table::Buffer, 4));
    assert_eq!(light_cs::ENTRY.sizes, Some(slot(Table::Buffer, 5)));
    // A RawOut alone is storage too.
    assert_eq!(flat_cs::out, slot(Table::Buffer, 0));
    assert_eq!(flat_cs::ENTRY.sizes, Some(slot(Table::Buffer, 1)));
}

/// `threads` is the workgroup's size, an axis left out 1; a kernel returns nothing.
#[test]
fn a_kernel_is_compute_with_its_threads() {
    assert_eq!(light_cs::ENTRY.stage, Stage::Compute { x: 8, y: 8, z: 1 });
    assert_eq!(light_cs::ENTRY.threads(), Some([8, 8, 1]));
    assert_eq!(flat_cs::ENTRY.threads(), Some([64, 1, 1]));
    assert_eq!(quad_fs::ENTRY.threads(), None);
    assert_eq!(light_cs::ENTRY.out, Out::Nothing);
    let kinds: Vec<Kind> = light_cs::ENTRY.params.iter().map(|p| p.kind).collect();
    assert_eq!(
        kinds,
        [
            Kind::Thread,
            Kind::Uniform { size: 32 },
            Kind::Tlas,
            Kind::Storage { stride: 32 },
            Kind::Storage { stride: 8 },
            Kind::RawOut { stride: 8 },
        ]
    );
}

/// What `check` refuses, each next to the same shape it accepts; the macros evaluate it in a
/// const, so each of these is a compile error with this message.
#[test]
fn check_refuses_what_a_stage_may_not_take() {
    use crate::{EntryDesc, ParamDesc, check, layout};
    fn refusal(e: EntryDesc) -> Option<String> {
        std::panic::catch_unwind(|| check(&e)).err().map(|p| {
            p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_default()
        })
    }
    static ONE: [ParamDesc; 1] = layout(["t"], [Kind::Thread]);
    static RAW: [ParamDesc; 1] = layout(["out"], [Kind::RawOut { stride: 4 }]);
    static VID: [ParamDesc; 1] = layout(["vid"], [Kind::VertexIndex]);
    let kernel = |x, y, z, params: &'static [ParamDesc], out| EntryDesc {
        name: "k",
        stage: Stage::Compute { x, y, z },
        params,
        out,
        sizes: None,
    };
    let fragment = |params: &'static [ParamDesc]| EntryDesc { name: "f", stage: Stage::Fragment, params, out: Out::Color, sizes: None };
    // Positive controls.
    assert_eq!(refusal(kernel(32, 32, 1, &ONE, Out::Nothing)), None);
    assert_eq!(refusal(kernel(1, 1, 1, &RAW, Out::Nothing)), None);
    // Workgroup bounds.
    assert!(refusal(kernel(32, 33, 1, &ONE, Out::Nothing)).unwrap().contains("at most 1024 threads"));
    assert!(refusal(kernel(8, 0, 1, &ONE, Out::Nothing)).unwrap().contains("at least one thread"));
    // What a kernel returns and takes.
    assert!(refusal(kernel(8, 8, 1, &ONE, Out::Color)).unwrap().contains("a kernel returns nothing"));
    assert!(refusal(kernel(8, 8, 1, &VID, Out::Nothing)).unwrap().contains("its identity is a Thread"));
    // What only a kernel takes.
    assert!(refusal(fragment(&ONE)).unwrap().contains("only a kernel"));
    assert!(refusal(fragment(&RAW)).unwrap().contains("only a kernel"));
    assert_eq!(refusal(fragment(&[])), None);
    let vertex_nothing = EntryDesc { name: "v", stage: Stage::Vertex, params: &[], out: Out::Nothing, sizes: None };
    assert!(refusal(vertex_nothing).unwrap().contains("a vertex stage returns"));
}

/// Every half but the NaNs goes to an `f32` and back to itself, and the ones on either side of
/// it in the `f32`s round as Metal does: to the nearest, ties to even.
#[test]
fn halves_round_trip_and_round_to_even() {
    for h in 0u32..=0xffff {
        let nan = (h >> 10) & 0x1f == 0x1f && h & 0x3ff != 0;
        let x = f16tof32(h);
        if nan {
            assert!(x.is_nan() && f32tof16(x) & 0x7c00 == 0x7c00 && f32tof16(x) & 0x3ff != 0, "{h:#06x}");
        } else {
            assert_eq!(f32tof16(x), h, "{h:#06x} through {x}");
        }
    }
    let one_ulp = 2f32.powi(-10);
    assert_eq!(f32tof16(1.0), 0x3c00);
    assert_eq!(f32tof16(1.0 + one_ulp / 2.0), 0x3c00, "a tie to the even 1.0");
    assert_eq!(f32tof16(1.0 + 1.5 * one_ulp), 0x3c02, "a tie to the even 1 + 2 ulp");
    assert_eq!(f32tof16(65504.0), 0x7bff);
    assert_eq!(f32tof16(65519.0), 0x7bff);
    assert_eq!(f32tof16(65520.0), 0x7c00, "a tie past the largest half is infinity");
    assert_eq!(f32tof16(2f32.powi(-24)), 0x0001);
    assert_eq!(f32tof16(2f32.powi(-25)), 0x0000, "a tie to the even 0");
    assert_eq!(f32tof16(1.5 * 2f32.powi(-25)), 0x0001);
    assert_eq!(f32tof16(-0.0), 0x8000);
    assert_eq!(f32tof16(f32::INFINITY), 0x7c00);
    assert_eq!(f16tof32(0x1_3c00), 1.0, "the bits above 16 are ignored");
}

/// Any `f32` in a half's range comes back within half a half's ulp of itself.
#[test]
fn f32tof16_is_the_nearest_half() {
    blaze_sim::check(Config::DEFAULT, |tape| {
        let x = tape.f32_in(-65504.0..=65504.0, "x");
        let back = f16tof32(f32tof16(x));
        // The gap between halves at x: 2^(e - 10) for a normal, 2^-24 for a subnormal.
        let e = x.abs().log2().floor().max(-14.0);
        let ulp = 2f32.powf(e - 10.0);
        assert!((back - x).abs() <= ulp / 2.0, "{x} came back as {back} (ulp {ulp})");
    });
}

/// A Thread gives back the ids the kernel ABI made it with.
#[test]
fn a_kernel_thread_reads_its_ids() {
    let t = Thread::new_for_tests(uvec3(3, 4, 0), uvec3(3, 4, 0), uvec3(0, 0, 0));
    assert_eq!((t.global().xy(), t.local(), t.group()), (uvec2(3, 4), uvec3(3, 4, 0), uvec3(0, 0, 0)));
}

// ---------------------------------------------------------------------------
// SOTA Upgrade Tests: pure proc_macro, #![no_std] math, matrices, Pod bytes,
// WGSL layout, safe compute, CPU simulator, RayQuery, 2x2 quads, MSL & WGSL IR
// ---------------------------------------------------------------------------

#[repr(transparent)]
#[derive(Copy, Clone, Debug, PartialEq, crate::Pod)]
struct TransparentPod(Vec4);

#[repr(C, align(16))]
#[derive(Copy, Clone, Debug, PartialEq, crate::Pod)]
struct AlignedPod {
    m: Mat4,
    v: Vec4,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, crate::Pod)]
struct UnitPod;

#[derive(Copy, Clone, Debug, crate::Varyings)]
struct IntVaryings {
    pos: Position,
    u2: Flat<UVec2>,
    u3: Flat<UVec3>,
    i3: Flat<IVec3>,
    i4: Flat<IVec4>,
}

#[crate::kernel(threads = (16,))]
fn tuple_1d_cs(mut t: Thread, out: &RawOut<u32>) {
    let idx = t.global().x;
    let _ = &mut t;
    let view = crate::compute::Out::new(out);
    view.try_write(&t, idx * 3 + 1);
}

#[crate::kernel(threads = (4, 4, 2))]
fn tuple_3d_cs(t: Thread, out: &RawOut<UVec3>) {
    let g = t.global();
    let idx = ((g.z * 4 + g.y) * 4 + g.x) as usize;
    if idx < out.len() {
        // SAFETY: Each 3D thread writes its unique flattened index `idx`.
        unsafe { out.write(idx, g) };
    }
}

#[crate::fragment]
fn sampled_fs(v: V, tex: &Texture2d, smp: &Sampler) -> Vec4 {
    tex.sample(smp, v.uv)
}

#[test]
fn pure_proc_macros_handle_all_forms() {
    assert_eq!(tuple_1d_cs::ENTRY.threads(), Some([16, 1, 1]));
    assert_eq!(tuple_3d_cs::ENTRY.threads(), Some([4, 4, 2]));
    assert_eq!(tuple_1d_cs::t, Slot { table: Table::None, index: 0 });
    assert_eq!(tuple_1d_cs::out, Slot { table: Table::Buffer, index: 0 });
    assert_eq!(core::mem::size_of::<TransparentPod>(), 16);
    assert_eq!(core::mem::size_of::<AlignedPod>(), 80);
    assert_eq!(core::mem::size_of::<UnitPod>(), 0);

    let f = <IntVaryings as crate::Varyings>::FIELDS;
    assert_eq!(f.len(), 5);
    assert_eq!(f[0].location, None);
    assert_eq!(f[1], Field { name: "u2", kind: FieldKind::Value { ty: ValueTy::UVec2, interp: Interp::Flat }, location: Some(0) });
    assert_eq!(f[2], Field { name: "u3", kind: FieldKind::Value { ty: ValueTy::UVec3, interp: Interp::Flat }, location: Some(1) });
    assert_eq!(f[3], Field { name: "i3", kind: FieldKind::Value { ty: ValueTy::IVec3, interp: Interp::Flat }, location: Some(2) });
    assert_eq!(f[4], Field { name: "i4", kind: FieldKind::Value { ty: ValueTy::IVec4, interp: Interp::Flat }, location: Some(3) });

    // Non-flat integer varying (e.g. UVec2 / IVec3 / IVec4) must be refused by `layout_fields`.
    for ty in [ValueTy::UVec2, ValueTy::UVec3, ValueTy::IVec3, ValueTy::IVec4] {
        let res = std::panic::catch_unwind(|| {
            crate::layout_fields(["v"], [FieldKind::Value { ty, interp: Interp::Perspective }])
        });
        assert!(res.is_err(), "{ty:?} without Flat must panic");
    }
}

#[test]
fn wgsl_flat_bindings_match_entry_desc() {
    // quad_vs(vid, iid, g, insts) -> resources at params 2, 3; sizes at binding 2.
    assert_eq!(quad_vs::ENTRY.wgsl_binding(0), None);
    assert_eq!(quad_vs::ENTRY.wgsl_binding(1), None);
    assert_eq!(quad_vs::ENTRY.wgsl_binding(2), Some((0, 0)));
    assert_eq!(quad_vs::ENTRY.wgsl_binding(3), Some((0, 1)));
    assert_eq!(quad_vs::ENTRY.wgsl_binding(4), None);
    assert_eq!(quad_vs::ENTRY.wgsl_sizes_binding(), Some((0, 2)));

    // quad_fs(v, insts, g, tex, smp, mask) -> 5 resources across Buffer/Texture/Sampler flattened to 0..=4; sizes at 5.
    assert_eq!(quad_fs::ENTRY.wgsl_binding(0), None);
    assert_eq!(quad_fs::ENTRY.wgsl_binding(1), Some((0, 0)));
    assert_eq!(quad_fs::ENTRY.wgsl_binding(2), Some((0, 1)));
    assert_eq!(quad_fs::ENTRY.wgsl_binding(3), Some((0, 2)));
    assert_eq!(quad_fs::ENTRY.wgsl_binding(4), Some((0, 3)));
    assert_eq!(quad_fs::ENTRY.wgsl_binding(5), Some((0, 4)));
    assert_eq!(quad_fs::ENTRY.wgsl_sizes_binding(), Some((0, 5)));

    assert_eq!(uniform_only_fs::ENTRY.wgsl_sizes_binding(), None);
    assert_eq!(light_cs::ENTRY.wgsl_binding(2), Some((0, 1))); // scene: &Tlas
    assert_eq!(light_cs::ENTRY.wgsl_sizes_binding(), Some((0, 5)));
}

#[test]
fn no_std_math_matches_std_within_tight_tolerance() {
    let close = |got: f32, want: f32, rel_tol: f32, abs_tol: f32| {
        (got - want).abs() <= abs_tol.max(rel_tol * want.abs())
    };
    blaze_sim::check(Config::DEFAULT, |tape| {
        let x = tape.f32_in(-40.0..=40.0, "x");
        assert!(close(exp(x), x.exp(), 1e-6, 1e-37), "exp({x}): {} vs {}", exp(x), x.exp());
        assert!(close(exp2(x), x.exp2(), 1e-6, 1e-37), "exp2({x}): {} vs {}", exp2(x), x.exp2());

        let pos = tape.f32_in(1e-20..=1e20, "pos");
        assert!(close(ln(pos), pos.ln(), 1e-6, 1e-6), "ln({pos}): {} vs {}", ln(pos), pos.ln());
        assert!(close(log2(pos), pos.log2(), 1e-6, 1e-6), "log2({pos}): {} vs {}", log2(pos), pos.log2());
        assert!(close(sqrt(pos), pos.sqrt(), 1e-6, 1e-37), "sqrt({pos})");
        assert!(close(rsqrt(pos), 1.0 / pos.sqrt(), 1e-6, 1e-37), "rsqrt({pos})");

        let base = tape.f32_in(1e-3..=1e3, "base");
        let e = tape.f32_in(-4.0..=4.0, "exp");
        assert!(close(pow(base, e), base.powf(e), 2e-6, 1e-6), "pow({base}, {e}): {} vs {}", pow(base, e), base.powf(e));

        let ang = tape.f32_in(-1000.0..=1000.0, "ang");
        assert!(close(sin(ang), ang.sin(), 1e-6, 1e-6), "sin({ang}): {} vs {}", sin(ang), ang.sin());
        assert!(close(cos(ang), ang.cos(), 1e-6, 1e-6), "cos({ang}): {} vs {}", cos(ang), ang.cos());
        if ang.cos().abs() > 1e-2 {
            assert!(close(tan(ang), ang.tan(), 2e-6, 1e-5), "tan({ang}): {} vs {}", tan(ang), ang.tan());
        }

        let ay = tape.f32_in(-100.0..=100.0, "ay");
        let ax = tape.f32_in(-100.0..=100.0, "ax");
        if ay != 0.0 || ax != 0.0 {
            assert!(close(atan2(ay, ax), ay.atan2(ax), 1e-6, 1e-6), "atan2({ay}, {ax})");
        }

        let f = tape.f32_in(-1e4..=1e4, "f");
        assert_eq!(abs(f), f.abs());
        assert_eq!(floor(f), f.floor());
        assert_eq!(ceil(f), f.ceil());
        assert_eq!(round(f), f.round());
        assert_eq!(trunc(f), f.trunc());
        assert_eq!(fract(f), frac(f));
        assert_eq!(fma(f, 1.5, -2.25), f.mul_add(1.5, -2.25));
        assert_eq!(asfloat(asuint(f)), f);
        assert_eq!(asint(f), f.to_bits() as i32);
    });

    // Edge cases
    assert_eq!(pow(-2.0, 3.0), -8.0);
    assert_eq!(pow(-2.0, 2.0), 4.0);
    assert!(pow(-2.0, 0.5).is_nan());
    assert_eq!(pow(0.0, 0.0), 1.0);
    assert_eq!(exp(0.0), 1.0);
    assert_eq!(ln(1.0), 0.0);
    assert_eq!(log2(1.0), 0.0);
    assert_eq!(log2(0.0), f32::NEG_INFINITY);
    assert!(ln(-1.0).is_nan());
    assert_eq!(atan2(0.0, 1.0), 0.0);
    assert!((atan2(1.0, 0.0) - core::f32::consts::FRAC_PI_2).abs() < 1e-6);
    assert!((atan2(-1.0, 0.0) + core::f32::consts::FRAC_PI_2).abs() < 1e-6);
    assert!((atan2(0.0, -1.0) - core::f32::consts::PI).abs() < 1e-6);
}

#[test]
fn vector_math_and_bitwise_ops_work() {
    assert_eq!(cross(vec3(1.0, 0.0, 0.0), vec3(0.0, 1.0, 0.0)), vec3(0.0, 0.0, 1.0));
    assert!((distance(vec3(1.0, 2.0, 3.0), vec3(4.0, 6.0, 3.0)) - 5.0).abs() < 1e-6);

    // Reflect downward diagonal ray off +Y normal.
    let i = normalize(vec3(1.0, -1.0, 0.0));
    let n = vec3(0.0, 1.0, 0.0);
    let r = reflect(i, n);
    assert!((r - normalize(vec3(1.0, 1.0, 0.0))).length() < 1e-6);

    // Refract normal incidence passes straight through; grazing internal reflection returns zero.
    let straight = refract(vec3(0.0, -1.0, 0.0), n, 0.75);
    assert!((straight - vec3(0.0, -1.0, 0.0)).length() < 1e-6);
    let tir = refract(normalize(vec3(0.99, -0.1, 0.0)), n, 2.0);
    assert_eq!(tir, Vec3::splat(0.0));

    assert_eq!(faceforward(n, vec3(0.0, -1.0, 0.0), n), n);
    assert_eq!(faceforward(n, vec3(0.0, 1.0, 0.0), n), -n);

    // Integer vectors & bitwise ops
    let a = uvec3(0b1100, 0b1010, 0b0011);
    let b = uvec3(0b1010, 0b0011, 0b1111);
    assert_eq!(a & b, uvec3(0b1000, 0b0010, 0b0011));
    assert_eq!(a | b, uvec3(0b1110, 0b1011, 0b1111));
    assert_eq!(a ^ b, uvec3(0b0110, 0b1001, 0b1100));
    assert_eq!(a << 1u32, uvec3(0b11000, 0b10100, 0b00110));
    assert_eq!(a >> 1u32, uvec3(0b0110, 0b0101, 0b0001));

    let iv = ivec3(1, -2, 3);
    assert_eq!((-iv, iv.xy(), iv.extend(4), iv.as_vec3()), (ivec3(-1, 2, -3), ivec2(1, -2), ivec4(1, -2, 3, 4), vec3(1.0, -2.0, 3.0)));
    assert_eq!(core::mem::size_of::<IVec3>(), 12);
}

#[test]
fn matrices_and_pod_byte_views() {
    // Mat2 rotation & inverse
    let r2 = Mat2::from_angle(core::f32::consts::FRAC_PI_2);
    let v2 = r2 * vec2(1.0, 0.0);
    assert!((v2 - vec2(0.0, 1.0)).length() < 1e-6);
    let inv2 = r2.inverse().expect("rotation is invertible");
    let prod2 = r2 * inv2;
    assert!((prod2.cols[0] - Mat2::IDENTITY.cols[0]).length() < 1e-6);
    assert!((prod2.cols[1] - Mat2::IDENTITY.cols[1]).length() < 1e-6);
    assert_eq!(Mat2::ZERO.inverse(), None);

    // Mat3 determinant & inverse
    let m3 = Mat3::from_cols(vec3(2.0, 1.0, 0.0), vec3(0.0, 3.0, 1.0), vec3(1.0, 0.0, 2.0));
    assert!((m3.determinant() - 13.0).abs() < 1e-5);
    let inv3 = m3.inverse().expect("m3 invertible");
    let id3 = m3 * inv3;
    for i in 0..3 {
        assert!((id3.cols[i] - Mat3::IDENTITY.cols[i]).length() < 1e-5);
    }

    // Mat4 translation * scale, look_at, perspective, inverse
    let m4 = Mat4::from_translation(vec3(3.0, -4.0, 5.0)) * Mat4::from_scale(vec3(2.0, 3.0, 4.0));
    let p = m4 * vec4(1.0, 1.0, 1.0, 1.0);
    assert_eq!(p, vec4(5.0, -1.0, 9.0, 1.0));
    let inv4 = m4.inverse().expect("affine matrix invertible");
    let back = inv4 * p;
    assert!((back - vec4(1.0, 1.0, 1.0, 1.0)).length() < 1e-5);
    assert_eq!(Mat4::ZERO.inverse(), None);

    let persp = Mat4::perspective_rh(core::f32::consts::FRAC_PI_2, 1.0, 1.0, 100.0);
    let near_pt = persp * vec4(0.0, 0.0, -1.0, 1.0);
    assert!((near_pt.z / near_pt.w).abs() < 1e-5, "near plane maps to z=0");
    let far_pt = persp * vec4(0.0, 0.0, -100.0, 1.0);
    assert!((far_pt.z / far_pt.w - 1.0).abs() < 1e-5, "far plane maps to z=1");

    let view = Mat4::look_at_rh(vec3(0.0, 0.0, 5.0), vec3(0.0, 0.0, 0.0), vec3(0.0, 1.0, 0.0));
    let origin_view = view * vec4(0.0, 0.0, 0.0, 1.0);
    assert!((origin_view - vec4(0.0, 0.0, -5.0, 1.0)).length() < 1e-5);

    // Pod::as_bytes, from_bytes (checked size & align), from_bytes_copy
    use crate::Pod as _;
    assert_eq!(core::mem::size_of::<Mat2>(), 16);
    assert_eq!(core::mem::size_of::<Mat3>(), 36);
    assert_eq!(core::mem::size_of::<Mat4>(), 64);
    let g = Globals { view: vec4(1.0, 2.0, 3.0, 4.0), tint: vec4(0.5, 0.25, 0.75, 1.0) };
    let bytes = g.as_bytes();
    assert_eq!(bytes.len(), 32);
    let borrowed = Globals::from_bytes(bytes).expect("aligned slice of exact size");
    assert_eq!(borrowed.view, g.view);
    assert_eq!(borrowed.tint, g.tint);
    assert!(Globals::from_bytes(&bytes[..31]).is_none());

    // Deliberately misaligned buffer (offset +1 from 16-byte alignment): from_bytes returns None, from_bytes_copy succeeds!
    #[repr(align(16))]
    struct AlignedBuf([u8; 36]);
    let mut unaligned = AlignedBuf([0u8; 36]);
    unaligned.0[1..33].copy_from_slice(bytes);
    assert!(Globals::from_bytes(&unaligned.0[1..33]).is_none());
    let copied = Globals::from_bytes_copy(&unaligned.0[1..33]).expect("unaligned copy succeeds");
    assert_eq!(copied.view, g.view);
    assert_eq!(copied.tint, g.tint);
}

#[test]
fn cpu_compute_dispatch_runs_gpu_kernel_and_checks_launches() {
    // 1. Run an actual `#[gpu::kernel]` function on CPU with Grid::cover!
    let mut buf = [0u32; 25];
    let grid = Grid::cover(buf.len() as u32, 16);
    let raw = RawOut::from_slice(&mut buf);
    crate::cpu::dispatch_compute(grid, |t| tuple_1d_cs(t, raw));
    for (i, &val) in buf.iter().enumerate() {
        assert_eq!(val, (i as u32) * 3 + 1);
    }

    // 2. Run 3D kernel on CPU.
    let mut buf3d = [UVec3::default(); 32];
    let grid3d = Grid::new(uvec3(1, 1, 1), uvec3(4, 4, 2));
    let proof = crate::check_launch(&tuple_3d_cs::ENTRY, grid3d, buf3d.len()).expect("valid launch");
    assert_eq!(proof.output_len(), 32);
    let raw3d = RawOut::from_slice(&mut buf3d);
    crate::cpu::dispatch_compute(grid3d, |t| tuple_3d_cs(t, raw3d));
    assert_eq!(buf3d[0], uvec3(0, 0, 0));
    assert_eq!(buf3d[31], uvec3(3, 3, 1));

    // 3. check_launch error cases
    assert_eq!(
        crate::check_launch(&quad_vs::ENTRY, grid3d, 32),
        Err(crate::LaunchError::NotComputeStage)
    );
    assert_eq!(
        crate::check_launch(&tuple_3d_cs::ENTRY, Grid::new(uvec3(1, 1, 1), uvec3(8, 1, 1)), 32),
        Err(crate::LaunchError::WorkgroupMismatch { expected: [4, 4, 2], got: [8, 1, 1] })
    );
    assert_eq!(
        crate::check_launch(&tuple_3d_cs::ENTRY, grid3d, 10),
        Err(crate::LaunchError::OutputTooSmall { required: 32, actual: 10 })
    );
    assert_eq!(
        crate::check_launch_grid(Grid::new(uvec3(1, 1, 1), uvec3(32, 33, 1)), 2000),
        Err(crate::LaunchError::TooManyThreadsPerGroup { got: 1056, max: crate::MAX_THREADS })
    );

    // 4. Chunks<4> and Tiles<2, 2> safe disjoint views
    let mut chunk_buf = [0u32; 16];
    let chunks = Chunks::<4>::new();
    let cgrid = Grid::new(uvec3(1, 1, 1), uvec3(4, 1, 1));
    assert_eq!(chunks.required_len(cgrid), 16);
    {
        let out = crate::compute::Out::from_slice(&mut chunk_buf);
        crate::cpu::dispatch_compute(cgrid, |t| {
            for k in 0..4 {
                out.write_chunk(&t, chunks, k, t.global().x * 10 + k as u32);
            }
        });
    }
    assert_eq!(chunk_buf, [0, 1, 2, 3, 10, 11, 12, 13, 20, 21, 22, 23, 30, 31, 32, 33]);

    let mut tile_buf = [0u32; 16];
    let tiles = Tiles::<2, 2>::new();
    let tgrid = Grid::cover_2d(2, 2, uvec2(2, 2)); // 2x2 threads, each owning a 2x2 tile -> 4x4 image
    {
        let out = crate::compute::Out::from_slice(&mut tile_buf);
        crate::cpu::dispatch_compute(tgrid, |t| {
            let tid = t.global().y * 2 + t.global().x + 1;
            for ly in 0..2 {
                for lx in 0..2 {
                    assert!(out.write_tile(&t, tiles, lx, ly, 4, 4, tid));
                }
            }
        });
    }
    assert_eq!(tile_buf, [1, 1, 2, 2, 1, 1, 2, 2, 3, 3, 4, 4, 3, 3, 4, 4]);
}

#[test]
fn phased_shared_memory_runs_workgroup_barrier_deterministically() {
    let input = [1u32, 2, 3, 4, 5, 6, 7, 8];
    let mut output = [0u32; 8];
    let shared = SharedMem::<u32, 8>::new(0);
    let grid = Grid::new(uvec3(1, 1, 1), uvec3(8, 1, 1));
    {
        let out = crate::compute::Out::from_slice(&mut output);
        crate::cpu::dispatch_workgroup_2phase(
            grid,
            &shared,
            |t, w| {
                // Phase 1 (SharedWrite): each thread writes only its own slot.
                w.write_own(t, input[t.global().x as usize]);
            },
            |t, r| {
                // Phase 2 (SharedRead): after barrier, every thread safely reads its neighbor!
                let lx = t.local().x as usize;
                let left = if lx > 0 { r.read(lx - 1) } else { 0 };
                let right = r.try_read(lx + 1).unwrap_or(0);
                out.write(t, left + r.read(lx) + right);
            },
        );
    }
    assert_eq!(output, [3, 6, 9, 12, 15, 18, 21, 15]);

    // Multi-pass cycle: SharedWrite -> barrier -> SharedRead -> barrier -> SharedWrite -> barrier -> SharedRead
    let t0 = Thread::new_for_tests(uvec3(0, 0, 0), uvec3(0, 0, 0), uvec3(0, 0, 0));
    let w1 = shared.begin_write();
    w1.write_own(&t0, 42);
    let r1 = w1.barrier();
    assert_eq!(r1.read(0), 42);
    let w2 = r1.barrier();
    w2.write_own(&t0, 99);
    let r2 = w2.barrier();
    assert_eq!(r2.read(0), 99);
}

#[test]
fn cpu_texture2d_and_sampler_filtering_and_mips() {
    // 2x2 mip 0 + 1x1 mip 1 = 5 texels total.
    let red = vec4(1.0, 0.0, 0.0, 1.0);
    let green = vec4(0.0, 1.0, 0.0, 1.0);
    let blue = vec4(0.0, 0.0, 1.0, 1.0);
    let white = vec4(1.0, 1.0, 1.0, 1.0);
    let mip1 = vec4(0.5, 0.5, 0.5, 1.0);
    let texels = [red, green, blue, white, mip1];
    assert_eq!(crate::mip_chain_texels(2, 2, 2), 5);
    let tex = Texture2d::from_rgba32f_mips(2, 2, 2, &texels);

    // Unfiltered load + out-of-bounds zero
    assert_eq!(tex.load(ivec2(0, 0), 0), red);
    assert_eq!(tex.load(ivec2(1, 0), 0), green);
    assert_eq!(tex.load(ivec2(0, 1), 0), blue);
    assert_eq!(tex.load(ivec2(1, 1), 0), white);
    assert_eq!(tex.load(ivec2(0, 0), 1), mip1);
    assert_eq!(tex.load(ivec2(-1, 0), 0), Vec4::splat(0.0));
    assert_eq!(tex.load(ivec2(2, 0), 0), Vec4::splat(0.0));

    // Linear bilinear sampling: exact at texel center (0.25, 0.25), exact average at (0.5, 0.5).
    let smp_lin = Sampler::new(Filter::Linear, AddressMode::ClampToEdge);
    assert!((tex.sample(&smp_lin, vec2(0.25, 0.25)) - red).length() < 1e-6);
    let center = tex.sample(&smp_lin, vec2(0.5, 0.5));
    let avg = (red + green + blue + white) * 0.25;
    assert!((center - avg).length() < 1e-6, "bilinear center {center:?} vs {avg:?}");

    // Trilinear mip sampling at level 0.5 blends mip 0 and mip 1 halfway.
    let tri = tex.sample_level(&smp_lin, vec2(0.5, 0.5), 0.5);
    let expect_tri = lerp(avg, mip1, 0.5);
    assert!((tri - expect_tri).length() < 1e-6);

    // Repeat & MirroredRepeat addressing
    let smp_rep = Sampler::new(Filter::Nearest, AddressMode::Repeat);
    assert_eq!(tex.sample(&smp_rep, vec2(1.25, 0.25)), red);
    let smp_mir = Sampler::new(Filter::Nearest, AddressMode::MirroredRepeat);
    assert_eq!(tex.sample(&smp_mir, vec2(1.25, 0.25)), green); // 1.25 mirrors to 0.75 -> x=1

    // Calling a #[gpu::fragment] shader directly on CPU with HostTexture2d!
    let v = V { uv: vec2(0.5, 0.5), pos: Position::default(), id: Flat(0), edge: Linear(vec2(0.0, 0.0)) };
    let frag = sampled_fs(v, &tex, &smp_lin);
    assert!((frag - avg).length() < 1e-6);
}

#[test]
fn cpu_tlas_and_inline_ray_query_triangles_culling_and_procedural() {
    // Two parallel triangles at z = -2 (front-facing CCW from +Z) and z = -4.
    let near_tri = Triangle::new(vec3(-1.0, -1.0, -2.0), vec3(1.0, -1.0, -2.0), vec3(0.0, 1.0, -2.0))
        .with_instance(1, 100)
        .with_geometry(0, 7)
        .with_mask(0x01)
        .with_opaque(false);
    let far_tri = Triangle::new(vec3(-2.0, -2.0, -4.0), vec3(2.0, -2.0, -4.0), vec3(0.0, 2.0, -4.0))
        .with_instance(2, 200)
        .with_geometry(1, 9)
        .with_mask(0x02)
        .with_opaque(true);
    let sphere_box = ProceduralAabb::new(vec3(-1.0, -1.0, -6.0), vec3(1.0, 1.0, -4.5))
        .with_instance(3, 300)
        .with_geometry(2, 42)
        .with_mask(0x04);

    let tris = [near_tri, far_tri];
    let aabbs = [sphere_box];
    let tlas = Tlas::from_geometry(&tris, &aabbs);

    let ray = Ray { origin: vec3(0.0, 0.0, 0.0), dir: vec3(0.0, 0.0, -1.0), tmin: 0.01, tmax: 100.0 };

    // 1. Opaque trace finds closest triangle (near_tri at t = 2.0).
    let hit = tlas.trace(ray, 0xFF).expect("hits near_tri");
    assert!((hit.t - 2.0).abs() < 1e-5);
    assert_eq!((hit.instance_index, hit.instance_custom_index, hit.geometry_index, hit.primitive_index), (1, 100, 0, 7));
    assert!(hit.front_face);
    // Check barycentrics reconstruct the hit point (0, 0, -2).
    let w0 = 1.0 - hit.barycentrics.x - hit.barycentrics.y;
    let pt = near_tri.v0 * w0 + near_tri.v1 * hit.barycentrics.x + near_tri.v2 * hit.barycentrics.y;
    assert!((pt - vec3(0.0, 0.0, -2.0)).length() < 1e-5);

    // 2. Mask filtering selects only far_tri.
    let hit_far = tlas.trace(ray, 0x02).expect("hits far_tri");
    assert!((hit_far.t - 4.0).abs() < 1e-5);
    assert_eq!(hit_far.instance_custom_index, 200);
    assert!(tlas.occluded(ray, 0x02));
    assert!(!tlas.occluded(Ray { origin: vec3(10.0, 10.0, 0.0), ..ray }, 0xFF));

    // 3. Face culling: ray from behind hits back face; CULL_BACK_FACING skips it.
    let back_ray = Ray { origin: vec3(0.0, 0.0, -10.0), dir: vec3(0.0, 0.0, 1.0), tmin: 0.01, tmax: 100.0 };
    let mut q_cull = RayQuery::new(&tlas, back_ray, RayFlags::FORCE_OPAQUE | RayFlags::CULL_BACK_FACING);
    while q_cull.proceed().is_some() {}
    assert!(q_cull.committed().is_none(), "back-facing triangles culled");

    let mut q_keep = RayQuery::new(&tlas, back_ray, RayFlags::FORCE_OPAQUE | RayFlags::CULL_FRONT_FACING);
    while q_keep.proceed().is_some() {}
    let back_hit = q_keep.committed().expect("back-facing triangle kept");
    assert!(!back_hit.front_face);

    // 4. Non-opaque candidate alpha test: reject near_tri, auto-commit far_tri!
    let mut q_alpha = RayQuery::new(&tlas, ray, RayFlags::NONE);
    let mut saw_near = false;
    while let Some(cand) = q_alpha.proceed() {
        if let Candidate::Triangle(h) = cand {
            assert_eq!(h.instance_custom_index, 100);
            saw_near = true;
            // Reject candidate (do not call `commit()`).
        }
    }
    assert!(saw_near);
    let committed = q_alpha.committed().expect("fell through to opaque far_tri");
    assert_eq!(committed.instance_custom_index, 200);
    assert!((committed.t - 4.0).abs() < 1e-5);

    // 5. Procedural AABB candidate with `commit_at(t)`.
    let mut q_proc = RayQuery::with_mask(&tlas, ray, RayFlags::NONE, 0x04);
    let mut saw_proc = false;
    while let Some(cand) = q_proc.proceed() {
        if let Candidate::Procedural { instance_custom_index, primitive_index, tmin, tmax, .. } = cand {
            saw_proc = true;
            assert_eq!((instance_custom_index, primitive_index), (300, 42));
            assert!((tmin - 4.5).abs() < 1e-5 && (tmax - 6.0).abs() < 1e-5);
            q_proc.commit_at(5.25);
        }
    }
    assert!(saw_proc);
    let proc_hit = q_proc.committed().expect("committed procedural hit");
    assert!((proc_hit.t - 5.25).abs() < 1e-5);
    assert_eq!(proc_hit.primitive_index, 42);
}

#[test]
fn cpu_quad_derivatives_discard_and_software_rasterizer() {
    // 1. Direct 2x2 quad evaluation: linear field f(x, y) = 3x + 5y has dpdx=3, dpdy=5, fwidth=8.
    let lanes = [
        vec2(0.0, 0.0), // TL (0,0)
        vec2(1.0, 0.0), // TR (1,0)
        vec2(0.0, 1.0), // BL (0,1)
        vec2(1.0, 1.0), // BR (1,1)
    ];
    let out = crate::cpu::eval_quad_2x2(&lanes, |ctx| {
        if ctx.lane() == 2 {
            return crate::cpu::FragOutcome::Discard;
        }
        let dx = ctx.dpdx(|p| 3.0 * p.x + 5.0 * p.y);
        let dy = ctx.dpdy(|p| 3.0 * p.x + 5.0 * p.y);
        let fw = ctx.fwidth(|p| 3.0 * p.x + 5.0 * p.y);
        crate::cpu::FragOutcome::Keep(vec4(dx, dy, fw, 1.0))
    });
    assert_eq!(out[0], Some(vec4(3.0, 5.0, 8.0, 1.0)));
    assert_eq!(out[1], Some(vec4(3.0, 5.0, 8.0, 1.0)));
    assert_eq!(out[2], None);
    assert_eq!(out[3], Some(vec4(3.0, 5.0, 8.0, 1.0)));

    // 2. Software triangle rasterizer into stack framebuffer (8x8) with depth buffer & discard.
    let mut fb = [Vec4::splat(0.0); 64];
    let mut zb = [1.0f32; 64];
    // Triangle 0: full-screen red at depth 0.8. Triangle 1: center-ish green at depth 0.3 (closer).
    let verts: [(Vec4, Vec4); 6] = [
        (vec4(-1.0, -1.0, 0.8, 1.0), vec4(1.0, 0.0, 0.0, 1.0)),
        (vec4(3.0, -1.0, 0.8, 1.0), vec4(1.0, 0.0, 0.0, 1.0)),
        (vec4(-1.0, 3.0, 0.8, 1.0), vec4(1.0, 0.0, 0.0, 1.0)),
        (vec4(-0.5, -0.5, 0.3, 1.0), vec4(0.0, 1.0, 0.0, 1.0)),
        (vec4(0.5, -0.5, 0.3, 1.0), vec4(0.0, 1.0, 0.0, 1.0)),
        (vec4(0.0, 0.5, 0.3, 1.0), vec4(0.0, 1.0, 0.0, 1.0)),
    ];
    crate::cpu::rasterize_triangles(
        8,
        8,
        2,
        1,
        &mut fb,
        Some(&mut zb),
        |vid, _iid| {
            let (p, c) = verts[vid.get() as usize];
            (Position(p), c)
        },
        |a, b, c, bary| *a * bary.x + *b * bary.y + *c * bary.z,
        |frag_pos, ctx| {
            // Discard pixel (0, 0) to verify discard leaves framebuffer untouched.
            if frag_pos.x < 1.0 && frag_pos.y < 1.0 {
                return crate::cpu::FragOutcome::Discard;
            }
            crate::cpu::FragOutcome::Keep(*ctx.value())
        },
    );
    assert_eq!(fb[0], Vec4::splat(0.0), "pixel (0, 0) was discarded");
    assert_eq!(fb[1], vec4(1.0, 0.0, 0.0, 1.0), "background pixel is red");
    assert_eq!(fb[4 * 8 + 4], vec4(0.0, 1.0, 0.0, 1.0), "closer green triangle wins depth test");
    assert!((zb[4 * 8 + 4] - 0.3).abs() < 1e-5);
}

#[test]
fn structured_ir_emits_valid_msl24_and_wgsl_matching_entry_desc() {
    use crate::emit::{
        IrBinOp, IrEntry, IrExpr, IrField, IrStmt, IrStruct, IrTy, ShaderModule, StackWriter, emit_msl, emit_wgsl,
    };

    const GLOBALS_FIELDS: [IrField; 2] = [
        IrField { name: "view", ty: IrTy::Vec4 },
        IrField { name: "tint", ty: IrTy::Vec4 },
    ];
    const INST_FIELDS: [IrField; 2] = [
        IrField { name: "rect", ty: IrTy::Vec4 },
        IrField { name: "info", ty: IrTy::UVec4 },
    ];
    const STRUCTS: [IrStruct; 2] = [
        IrStruct { name: "Globals", fields: &GLOBALS_FIELDS },
        IrStruct { name: "Inst", fields: &INST_FIELDS },
    ];

    // Fragment shader body sampling `tex` with `smp` and tinting by `g.tint`.
    let v_var = IrExpr::Var("v");
    let v_uv = IrExpr::Field(&v_var, "uv");
    let g_var = IrExpr::Var("g");
    let g_tint = IrExpr::Field(&g_var, "tint");
    let sampled = IrExpr::Sample2d { tex: "tex", smp: "smp", uv: &v_uv, level: None };
    let tinted = IrExpr::Binary(IrBinOp::Mul, &sampled, &g_tint);
    let fs_body = [IrStmt::Return(Some(tinted))];

    let fs_entry = IrEntry {
        desc: &quad_fs::ENTRY,
        param_types: &[
            IrTy::Void,              // v: V
            IrTy::Struct("Inst"),    // insts: &[Inst]
            IrTy::Struct("Globals"), // g: &Globals
            IrTy::Void,              // tex: &Texture2d
            IrTy::Void,              // smp: &Sampler
            IrTy::Void,              // mask: &Texture2d
        ],
        body: &fs_body,
    };

    // Compute + raytracing kernel body using `light_cs::ENTRY`.
    let tid = IrExpr::ThreadGlobal("t");
    let idx = IrExpr::Field(&tid, "x");
    let acc_len = IrExpr::BufferLen("acc");
    let in_bounds = IrExpr::Binary(IrBinOp::Lt, &idx, &acc_len);
    let acc_var = IrExpr::Var("acc");
    let acc_slot = IrExpr::Index(&acc_var, &idx);
    let val_expr = IrExpr::Construct(IrTy::UVec2, &[idx, IrExpr::LitU32(1)]);
    let write_stmt = [IrStmt::Assign { target: acc_slot, value: val_expr }];
    let cs_body = [
        IrStmt::WorkgroupBarrier,
        IrStmt::If { cond: in_bounds, then_body: &write_stmt, else_body: &[] },
    ];

    let cs_entry = IrEntry {
        desc: &light_cs::ENTRY,
        param_types: &[
            IrTy::Void,              // t: Thread
            IrTy::Struct("Globals"), // view: &Globals
            IrTy::Void,              // scene: &Tlas
            IrTy::Struct("Inst"),    // lamps: &[Lamp]
            IrTy::UVec2,             // cells: &[UVec2]
            IrTy::UVec2,             // acc: &RawOut<UVec2>
        ],
        body: &cs_body,
    };

    let module = ShaderModule { structs: &STRUCTS, helpers: &[], entries: &[fs_entry, cs_entry] };

    // Emit MSL 2.4 directly into a zero-heap stack buffer!
    let mut msl = StackWriter::<8192>::new();
    emit_msl(&module, &mut msl).expect("MSL emission fits in stack buffer");
    let msl_src = msl.as_str();
    assert!(msl_src.contains("#include <metal_stdlib>"));
    assert!(msl_src.contains("#include <metal_raytracing>"));
    assert!(msl_src.contains("float4 pos [[position]];"));
    assert!(msl_src.contains("uint id [[user(loc1), flat]];"));
    assert!(msl_src.contains("float2 edge [[user(loc2), center_no_perspective]];"));
    assert!(msl_src.contains("const device Inst* insts [[buffer(0)]]"));
    assert!(msl_src.contains("constant Globals& g [[buffer(1)]]"));
    assert!(msl_src.contains("texture2d<float> tex [[texture(0)]]"));
    assert!(msl_src.contains("sampler smp [[sampler(0)]]"));
    assert!(msl_src.contains("texture2d<float> mask [[texture(1)]]"));
    assert!(msl_src.contains("constant uint* _gpu_sizes [[buffer(2)]]"));
    assert!(msl_src.contains("tex.sample(smp, v.uv)"));
    assert!(msl_src.contains("metal::raytracing::instance_acceleration_structure scene [[buffer(1)]]"));
    assert!(msl_src.contains("device uint2* acc [[buffer(4)]]"));
    assert!(msl_src.contains("constant uint* _gpu_sizes [[buffer(5)]]"));
    assert!(msl_src.contains("threadgroup_barrier(mem_flags::mem_threadgroup);"));
    assert!(msl_src.contains("(_gpu_sizes[2] / 8u)"));

    // Emit WGSL directly into a zero-heap stack buffer!
    let mut wgsl = StackWriter::<8192>::new();
    emit_wgsl(&module, &mut wgsl).expect("WGSL emission fits in stack buffer");
    let wgsl_src = wgsl.as_str();
    assert!(wgsl_src.contains("@builtin(position) pos: vec4<f32>,"));
    assert!(wgsl_src.contains("@location(1) @interpolate(flat) id: u32,"));
    assert!(wgsl_src.contains("@location(2) @interpolate(linear) edge: vec2<f32>,"));
    assert!(wgsl_src.contains("@group(0) @binding(0) var<storage, read> insts: array<Inst>;"));
    assert!(wgsl_src.contains("@group(0) @binding(1) var<uniform> g: Globals;"));
    assert!(wgsl_src.contains("@group(0) @binding(2) var tex: texture_2d<f32>;"));
    assert!(wgsl_src.contains("@group(0) @binding(3) var smp: sampler;"));
    assert!(wgsl_src.contains("@group(0) @binding(4) var mask: texture_2d<f32>;"));
    assert!(wgsl_src.contains("@group(0) @binding(5) var<storage, read> _gpu_sizes: array<u32>;"));
    assert!(wgsl_src.contains("textureSample(tex, smp, v.uv)"));
    assert!(wgsl_src.contains("@compute @workgroup_size(8, 8, 1)"));
    assert!(wgsl_src.contains("@group(0) @binding(1) var scene: acceleration_structure;"));
    assert!(wgsl_src.contains("workgroupBarrier();"));
    assert!(wgsl_src.contains("arrayLength(&acc)"));
}
