//! The Rust port of shaders/harness.slang (harness-native-shaders) against what this crate uses today:
//! its buffer structs byte for byte pack.rs's, its binding table the one build.rs reads out of the
//! Slang-generated MSL (`gpu::BINDINGS`), and its HDR and sRGB curves hdr.rs's, value for value.
use core::mem::{align_of, offset_of, size_of};

// `::gpu` is the gpu crate; `crate::gpu` is this crate's Metal renderer.
use ::gpu::Table;
use ::gpu::vec::{Vec3, vec2, vec3};
use harness_native_shaders::harness as port;

use crate::{hdr, pack};

/// Same size, alignment and offset of every named field (the names are the same on both sides).
macro_rules! same_layout {
    ($host:ty, $port:ty, [$($f:ident),* $(,)?]) => {{
        let name = stringify!($host);
        assert_eq!(size_of::<$host>(), size_of::<$port>(), "{name}: size");
        assert_eq!(align_of::<$host>(), align_of::<$port>(), "{name}: alignment");
        $(assert_eq!(offset_of!($host, $f), offset_of!($port, $f), "{name}.{}: offset", stringify!($f));)*
    }};
}

#[test]
fn port_structs_are_packs() {
    same_layout!(pack::Frame, port::Frame, [view, scale, time, dark, live, textfx, tune, slug, place]);
    same_layout!(pack::PanelInst, port::PanelInst, [rect, fill_d, fill_l, edge_d, edge_l, shape, fx, fx2, lens, fx3, clip, clipx]);
    same_layout!(pack::LineInst, port::LineInst, [seg, around, col_d, col_l, shape, clip, clipx]);
    same_layout!(pack::PathInst, port::PathInst, [rect, col_d, col_l, shape, clip, clipx]);
    same_layout!(pack::ImageInst, port::ImageInst, [rect, size, clip, clipx, light, hdr]);
    same_layout!(pack::SurfaceInst, port::SurfaceInst, [rect, shape, clip, clipx, uv, hdr]);
    same_layout!(pack::SpriteInst, port::SpriteInst, [rect, shape, fg, bg]);
    same_layout!(pack::GlyphInst, port::GlyphInst, [rect, uv, col_d, col_l, anim, kind, clip, band, gdata, fx, fade, clipx]);
}

/// Every entry harness.slang has, the port has, with the same resource parameters in the same order and in
/// the same tables. Slang numbers a table's slots across the whole program and the port per entry from 0,
/// so the index the port gives is the host's slot less the entry's first slot in that table.
#[test]
fn port_binds_what_the_host_binds() {
    let table = |kind: &str| match kind {
        "buffer" => Table::Buffer,
        "texture" => Table::Texture,
        "sampler" => Table::Sampler,
        other => panic!("BINDINGS kind {other}: not buffer, texture or sampler"),
    };
    let entries: Vec<_> = harness_native_shaders::GPU_ENTRIES.0.iter().map(|e| e.desc).filter(|d| !d.name.starts_with("mascot")).collect();
    let mut host_names: Vec<&str> = crate::gpu::BINDINGS.iter().map(|b| b.0).collect();
    host_names.dedup();
    let port_names: Vec<&str> = entries.iter().map(|d| d.name).collect();
    assert_eq!(port_names, host_names, "the entries, in harness.slang's order");
    for d in entries {
        let host: Vec<_> = crate::gpu::BINDINGS.iter().filter(|b| b.0 == d.name).collect();
        let ours: Vec<_> = d.params.iter().filter(|p| p.slot.table != Table::None).collect();
        let host_sig: Vec<(&str, Table)> = host.iter().map(|b| (b.1, table(b.2))).collect();
        let our_sig: Vec<(&str, Table)> = ours.iter().map(|p| (p.name, p.slot.table)).collect();
        assert_eq!(our_sig, host_sig, "{}: resource parameters", d.name);
        for (b, p) in host.iter().zip(&ours) {
            let base = host.iter().filter(|o| o.2 == b.2).map(|o| o.3).min().unwrap_or(0);
            assert_eq!(p.slot.index, b.3 - base, "{}.{}: {} slot {} is the entry's #{}", d.name, b.1, b.2, b.3, b.3 - base);
        }
    }
}

fn v3(c: [f32; 3]) -> Vec3 {
    vec3(c[0], c[1], c[2])
}

/// The port's HDR and sRGB curves are hdr.rs's: the same operations in the same order, so the same floats.
#[hegel::test]
fn port_curves_are_hdrs(tc: hegel::TestCase) {
    use hegel::generators as gs;
    let f = |lo: f32, hi: f32| gs::floats::<f32>().min_value(lo).max_value(hi).allow_nan(false).allow_infinity(false);
    let h = tc.draw(f(0.0, 64.0));
    let peak = tc.draw(gs::sampled_from(vec![0.0f32, f32::INFINITY, 0.5, 1.0, 4.0, 49.26, 200.0]));
    let x = tc.draw(f(-4.0, 400.0));
    let rgb = [x, tc.draw(f(-4.0, 400.0)), tc.draw(f(-4.0, 400.0))];
    let e = [tc.draw(f(-0.5, 1.5)), tc.draw(f(-0.5, 1.5)), tc.draw(f(-0.5, 1.5))];

    assert_eq!(port::tone_map(x, peak, h), hdr::tone_map(x, peak, h), "tone_map({x}, {peak}, {h})");
    assert_eq!(port::tone_map_rgb(v3(rgb), peak, h), v3(hdr::tone_map_rgb(rgb, peak, h)), "tone_map_rgb({rgb:?})");
    assert_eq!(port::hdr_knee(h), hdr::knee(h));
    assert_eq!(port::pq_linear(v3(e)), v3(e.map(hdr::pq_linear)), "pq_linear({e:?})");
    assert_eq!(port::hlg_scene(v3(e)), v3(e.map(hdr::hlg_scene)), "hlg_scene({e:?})");
    assert_eq!(port::hlg_linear(v3(e)), v3(hdr::hlg_linear(e)), "hlg_linear({e:?})");
    assert_eq!(port::bt2020_to_709(v3(rgb)), v3(hdr::bt2020_to_709(rgb)), "bt2020_to_709({rgb:?})");
    // sign() where hdr.rs has copysign: the same for every finite value, zeros equal as floats.
    assert_eq!(port::srgb_encode_ext(v3(rgb)), v3(rgb.map(hdr::srgb_encode_ext)), "srgb_encode_ext({rgb:?})");
    assert_eq!(port::srgb_decode_ext(v3(e)), v3(e.map(hdr::srgb_decode_ext)), "srgb_decode_ext({e:?})");

    let frame = pack::Frame { textfx: [h, 0.0], ..pack::Frame::default() };
    let ours = port::Frame { textfx: vec2(h, 0.0), ..port::Frame::default() };
    assert_eq!(port::frame_headroom(&ours), frame.headroom());
    assert_eq!((port::HDR_WHITE_NITS, port::HDR_HLG_PEAK_NITS, port::HDR_KNEE), (hdr::WHITE_NITS, hdr::HLG_PEAK_NITS, hdr::KNEE));
}
