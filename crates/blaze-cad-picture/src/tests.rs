//! The renderer's promise, an oracle over drawn models and views, and the planted bug it must
//! catch:
//!
//! - THE SILHOUETTE AGREES WITH POINT MEMBERSHIP along each ray within the step tolerance:
//!   (a) no point of a ray before its hit (or anywhere, for a background pixel) is inside the
//!   solid, sampled at a quarter of the thinnest wall; (b) at a hit, the field is at most four
//!   step tolerances (the field is 1-Lipschitz, so the surface is that near). Models are unions of
//!   boxes and spheres with, half the time, a wall thinner than a pixel's step. Planted:
//!   `PointStep` (the tracer trusts the field at a segment's end instead of its interval: it steps
//!   past the thin wall, or stops short of a surface), caught as `Inside` or `FarHit`.

use core::fmt;
use std::{format, string::String, vec::Vec as StdVec};

use blaze_cad_field::{BoxOpts, CompileOpts, Graph, GraphOpts, Outside, Placement, Scratch, Solid, SphereOpts, compile, eval_point};
use blaze_own::Alloc;
use blaze_sim::{Broken, Config, NoPlant, Planted, Tape, check, controls};
use blaze_units::{Angle, Length};

use crate::*;

/// The test's allocator (D102): the process heap's handle, never `Global`.
fn heap() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

fn harness(e: impl fmt::Debug) -> Broken<PictureBroken> {
    Broken::Harness(format!("{e:?}"))
}

#[derive(Clone, PartialEq, Debug)]
enum PictureBroken {
    /// A point before the hit (or on a background ray) is inside.
    Inside { x: u32, y: u32, t: f64, hit: f64 },
    /// The field at a hit is farther than the bound.
    FarHit { x: u32, y: u32, value: f64, bound: f64 },
    Wrong { why: String },
}

impl fmt::Display for PictureBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

blaze_sim::plants! {
    impl PicturePlant breaks PictureBroken {
        None,
        PointStep => PictureBroken::Inside { .. } | PictureBroken::FarHit { .. },
    }
}

/// The model: one to three blobs (spheres or cubes, millimetres) and maybe a thin square wall
/// through the origin; returns the tape and the thinnest wall in metres.
fn model(tape: &Tape) -> Result<(blaze_cad_field::Tape<'static>, f64), Broken<PictureBroken>> {
    let mut g = Graph::new(GraphOpts { alloc: heap(), .. });
    let mut acc: Option<Solid<Outside>> = None;
    let mut thinnest = 0.024f64;
    for _ in 0..tape.int_in(1..=3usize, "blobs") {
        let size = tape.int_in(12..=40i64, "size") as f64;
        let s = if tape.coin("sphere") {
            g.sphere(SphereOpts { radius: Length::mm(size), .. })
        } else {
            g.cuboid(BoxOpts { size: [Length::mm(2.0 * size); 3], .. })
        }
        .map_err(harness)?;
        let mut at = [0.0f64; 3];
        for c in &mut at {
            *c = tape.int_in(-60..=60i64, "at") as f64;
        }
        let s = g.place(s, Placement::shift(Length::mm(at[0]), Length::mm(at[1]), Length::mm(at[2]))).map_err(harness)?;
        acc = Some(match acc {
            None => s.weaken::<Outside>(),
            Some(a) => g.union(a, s).map_err(harness)?,
        });
    }
    if tape.coin("wall") {
        let thickness = tape.int_in(4..=10i64, "thickness") as f64 / 10.0;
        let axis = tape.index(3, "wall axis");
        let mut size = [Length::mm(120.0); 3];
        size[axis] = Length::mm(thickness);
        let wall = g.cuboid(BoxOpts { size, .. }).map_err(harness)?;
        thinnest = thinnest.min(thickness / 1000.0);
        acc = Some(match acc {
            None => wall.weaken::<Outside>(),
            Some(a) => g.union(a, wall).map_err(harness)?,
        });
    }
    let root = acc.ok_or_else(|| harness("no solid"))?;
    let t = compile(&g, root.id(), CompileOpts { alloc: heap(), .. }).map_err(harness)?;
    Ok((t, thinnest))
}

fn view(tape: &Tape) -> View {
    // An eye 400 mm out, never straight above or below (the up direction is +z).
    let turn = tape.int_in(0..=359i64, "azimuth") as f64;
    let rise = tape.int_in(-60..=60i64, "elevation") as f64;
    let (st, ct) = blaze_math::sin_cos(turn.to_radians());
    let (sr, cr) = blaze_math::sin_cos(rise.to_radians());
    let r = 400.0;
    let eye = [Length::mm(r * cr * ct), Length::mm(r * cr * st), Length::mm(r * sr)];
    let projection = if tape.coin("perspective") {
        Projection::Perspective { fov: Angle::degrees(tape.int_in(20..=60i64, "fov") as f64) }
    } else {
        Projection::Orthographic { height: Length::mm(tape.int_in(150..=300i64, "height") as f64) }
    };
    View { eye, target: [Length::ZERO; 3], projection, width: 24, height: 24, .. }
}

fn silhouette_case(tape: &Tape, plant: impl Planted<PicturePlant> + Copy) -> Result<(), Broken<PictureBroken>> {
    let (t, thinnest) = model(tape)?;
    let v = view(tape);
    let o = PictureOpts { alloc: heap(), scratch: heap(), .. };
    let pic = render_planted(&t, v, o, plant).map_err(harness)?;
    let cam = Camera::new(&v).map_err(harness)?;
    let mut sc = Scratch::new(heap());
    let (w, h) = (v.width, v.height);
    if pic.pixels.len() != (w * h * 4) as usize || pic.depth.len() != (w * h) as usize {
        return Err(Broken::Oracle(PictureBroken::Wrong { why: format!("{} bytes, {} depths", pic.pixels.len(), pic.depth.len()) }));
    }

    // (b) every hit is near the surface.
    for y in 0..h {
        for x in 0..w {
            let hit = pic.depth[(y * w + x) as usize];
            if !hit.is_finite() {
                continue;
            }
            let (origin, dir) = cam.ray(x, y);
            let value = eval_point(&t, origin + dir * hit, &mut sc);
            // The hit segment is at most a tolerance long and its interval reaches zero; the
            // interval of these fields over so small a box is within a few lengths of exact.
            let bound = 4.0 * cam.tolerance_at(hit, o.pixel_fraction);
            if !(value <= bound) {
                return Err(Broken::Oracle(PictureBroken::FarHit { x, y, value, bound }));
            }
        }
    }

    // (a) along drawn rays, nothing inside before the hit.
    let box_ = t.aabb();
    let spacing = thinnest / 4.0;
    for _ in 0..8 {
        let (x, y) = (tape.int_in(0..=w - 1, "x"), tape.int_in(0..=h - 1, "y"));
        let hit = pic.depth[(y * w + x) as usize];
        let (origin, dir) = cam.ray(x, y);
        let Some((t0, t1)) = crate::slab(&box_, origin, dir) else { continue };
        let end = if hit.is_finite() { hit } else { t1 };
        let mut s = t0;
        let mut samples = 0u32;
        while s < end && samples < 100_000 {
            if eval_point(&t, origin + dir * s, &mut sc) < 0.0 {
                return Err(Broken::Oracle(PictureBroken::Inside { x, y, t: s, hit }));
            }
            s += spacing;
            samples += 1;
        }
    }
    Ok(())
}

#[test]
fn silhouette_agrees_with_membership() {
    check(Config::cases(60), |tape| silhouette_case(tape, NoPlant));
}

#[test]
fn picture_plants_are_caught() {
    controls(Config::cases(120), |tape, plant: PicturePlant| silhouette_case(tape, plant));
}

/// The bytes are a function of the inputs: two renders of one view are equal, and the PNG of one
/// starts with the signature.
#[test]
fn render_is_deterministic() {
    check(Config::cases(10), |tape| {
        let (t, _) = model(tape)?;
        let v = view(tape);
        let o = PictureOpts { alloc: heap(), scratch: heap(), .. };
        let a = render(&t, v, o).map_err(harness)?;
        let b = render(&t, v, o).map_err(harness)?;
        if a.pixels[..] != b.pixels[..] {
            return Err(Broken::Oracle(PictureBroken::Wrong { why: String::from("two renders differ") }));
        }
        let png = a
            .png(blaze_cad_out::PngOpts {
                level: blaze_deflate::Level::Fast,
                limits: blaze_cad_out::Limits::SHIPPED,
                alloc: heap(),
                scratch: heap(),
            })
            .map_err(harness)?;
        let head: StdVec<u8> = png[..8].iter().copied().collect();
        if head != [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A] {
            return Err(Broken::Oracle(PictureBroken::Wrong { why: format!("png head {head:?}") }));
        }
        Ok(())
    });
}

fn luma(px: &[u8]) -> f64 {
    0.2126 * f64::from(px[0]) + 0.7152 * f64::from(px[1]) + 0.0722 * f64::from(px[2])
}

/// Tiled frustum culling and per-tile tape simplification agree with the untiled reference across
/// random models and perspective/orthographic views, and actively cull & simplify multi-part models.
#[test]
fn tiled_matches_untiled_parity() {
    check(Config::cases(40), |tape| {
        let (t, _) = model(tape)?;
        let mut v = view(tape);
        v.width = 48;
        v.height = 48;
        let o = PictureOpts { alloc: heap(), scratch: heap(), .. };
        let tiled = render(&t, v, o).map_err(harness)?;
        let untiled = render_untiled(&t, v, o).map_err(harness)?;
        let cam = Camera::new(&v).map_err(harness)?;
        if tiled.stats.rays != untiled.stats.rays {
            return Err(Broken::Oracle(PictureBroken::Wrong {
                why: format!("ray count mismatch: tiled={} untiled={}", tiled.stats.rays, untiled.stats.rays),
            }));
        }
        let mut sc = Scratch::new(heap());
        for y in 0..v.height {
            for x in 0..v.width {
                let idx = (y * v.width + x) as usize;
                let (dt, du) = (tiled.depth[idx], untiled.depth[idx]);
                match (dt.is_finite(), du.is_finite()) {
                    (true, true) => {
                        let tol = 2.0 * cam.tolerance_at(dt.max(du), o.pixel_fraction);
                        if (dt - du).abs() > tol {
                            return Err(Broken::Oracle(PictureBroken::Wrong {
                                why: format!("pixel ({x},{y}) depth diff: tiled={dt} untiled={du} tol={tol}"),
                            }));
                        }
                    }
                    (false, false) => {}
                    (true, false) | (false, true) => {
                        // Only possible at a grazing edge within step tolerance; verify the hit
                        // point is genuinely near zero and never falsely culled inside.
                        let d = if dt.is_finite() { dt } else { du };
                        let (origin, dir) = cam.ray(x, y);
                        let val = eval_point(&t, origin + dir * d, &mut sc);
                        let tol = 4.0 * cam.tolerance_at(d, o.pixel_fraction);
                        if !(val.abs() <= tol) {
                            return Err(Broken::Oracle(PictureBroken::Wrong {
                                why: format!("pixel ({x},{y}) hit/miss mismatch: tiled={dt} untiled={du} f={val} tol={tol}"),
                            }));
                        }
                    }
                }
            }
        }
        Ok(())
    });

    // Multi-part model exercises both tile culling and per-tile tape simplification.
    let mut g = Graph::new(GraphOpts { alloc: heap(), .. });
    let mut acc: Option<Solid<Outside>> = None;
    for i in 0..8 {
        let s = g.sphere(SphereOpts { radius: Length::mm(14.0), .. }).expect("sphere");
        let s = g.place(s, Placement::shift(Length::mm(45.0 * i as f64 - 157.5), Length::ZERO, Length::ZERO)).expect("place");
        acc = Some(match acc {
            None => s.weaken::<Outside>(),
            Some(a) => g.union(a, s).expect("union"),
        });
    }
    let tape = compile(&g, acc.expect("solid").id(), CompileOpts { alloc: heap(), .. }).expect("tape");
    let v = View {
        eye: [Length::mm(0.0), Length::mm(-500.0), Length::mm(120.0)],
        target: [Length::ZERO; 3],
        projection: Projection::Perspective { fov: Angle::degrees(40.0) },
        width: 96,
        height: 96,
        ..
    };
    let pic = render(&tape, v, PictureOpts { alloc: heap(), scratch: heap(), .. }).expect("render");
    assert!(pic.stats.tiles_culled > 0, "expected empty tiles to be culled");
    assert!(pic.stats.tiles_simplified > 0, "expected non-empty tiles to simplify the tape");
    assert!(pic.stats.hits > 0);
}

/// Distance-bound stride acceleration never skips past an ultra-thin (50 µm) wall in front of a
/// larger solid, from both head-on and oblique perspective/orthographic views.
#[test]
fn thin_wall_never_skipped_by_stride_acceleration() {
    let mut g = Graph::new(GraphOpts { alloc: heap(), .. });
    let wall = g
        .cuboid(BoxOpts { size: [Length::mm(0.05), Length::mm(120.0), Length::mm(120.0)], .. })
        .expect("thin wall");
    let back = g.cuboid(BoxOpts { size: [Length::mm(40.0), Length::mm(120.0), Length::mm(120.0)], .. }).expect("back");
    let back = g.place(back, Placement::shift(Length::mm(-40.0), Length::ZERO, Length::ZERO)).expect("place back");
    let solid = g.union(wall, back).expect("union");
    let tape = compile(&g, solid.id(), CompileOpts { alloc: heap(), .. }).expect("tape");

    let views = [
        View {
            eye: [Length::mm(300.0), Length::ZERO, Length::ZERO],
            target: [Length::ZERO; 3],
            projection: Projection::Perspective { fov: Angle::degrees(35.0) },
            width: 32,
            height: 32,
            ..
        },
        View {
            eye: [Length::mm(240.0), Length::mm(180.0), Length::mm(40.0)],
            target: [Length::ZERO; 3],
            projection: Projection::Orthographic { height: Length::mm(100.0) },
            width: 32,
            height: 32,
            ..
        },
    ];

    for v in views {
        let pic = render(&tape, v, PictureOpts { alloc: heap(), scratch: heap(), .. }).expect("render");
        let cam = Camera::new(&v).expect("cam");
        // Centre pixel must hit the front face of the thin wall at x = +0.025 mm, never the box at x = -20 mm.
        let (cx, cy) = (v.width / 2, v.height / 2);
        let d = pic.depth[(cy * v.width + cx) as usize];
        assert!(d.is_finite(), "centre ray must hit the thin wall");
        let (o, dir) = cam.ray(cx, cy);
        let p = o + dir * d;
        assert!(
            p.x > -0.0005 && p.x < 0.002,
            "ray skipped the 50 µm wall at x=0: hit at x={} m (back box is at x=-0.02 m)",
            p.x
        );
    }
}

/// Studio SDF ambient occlusion darkens concave 90° corners relative to AO-off while leaving
/// exposed exterior faces unaffected.
#[test]
fn studio_ao_darkens_concave_corner() {
    let mut g = Graph::new(GraphOpts { alloc: heap(), .. });
    let floor = g.cuboid(BoxOpts { size: [Length::mm(120.0), Length::mm(120.0), Length::mm(10.0)], .. }).expect("floor");
    let floor = g.place(floor, Placement::shift(Length::mm(50.0), Length::ZERO, Length::mm(-5.0))).expect("place floor");
    let wall = g.cuboid(BoxOpts { size: [Length::mm(10.0), Length::mm(120.0), Length::mm(120.0)], .. }).expect("wall");
    let wall = g.place(wall, Placement::shift(Length::mm(-5.0), Length::ZERO, Length::mm(55.0))).expect("place wall");
    let bracket = g.union(floor, wall).expect("bracket");
    let tape = compile(&g, bracket.id(), CompileOpts { alloc: heap(), .. }).expect("tape");

    let v = View {
        eye: [Length::mm(140.0), Length::ZERO, Length::mm(140.0)],
        target: [Length::mm(35.0), Length::ZERO, Length::ZERO],
        projection: Projection::Orthographic { height: Length::mm(110.0) },
        width: 64,
        height: 64,
        ..
    };
    let no_ao = render(
        &tape,
        v,
        PictureOpts {
            alloc: heap(),
            scratch: heap(),
            shading: Shading::Studio(StudioOpts { ao_strength: 0.0, shadow_strength: 0.0, .. }),
            ..
        },
    )
    .expect("no_ao");
    let with_ao = render(
        &tape,
        v,
        PictureOpts {
            alloc: heap(),
            scratch: heap(),
            shading: Shading::Studio(StudioOpts { ao_strength: 0.85, ao_steps: 5, shadow_strength: 0.0, .. }),
            ..
        },
    )
    .expect("with_ao");

    let cam = Camera::new(&v).expect("cam");
    let mut corner_delta = 0.0f64;
    let mut exposed_delta = f64::INFINITY;
    let mut corner_hits = 0u32;
    let mut exposed_hits = 0u32;
    for y in 0..v.height {
        for x in 0..v.width {
            let idx = (y * v.width + x) as usize;
            let d = with_ao.depth[idx];
            if !d.is_finite() {
                continue;
            }
            let (o, dir) = cam.ray(x, y);
            let p = o + dir * d;
            // Floor pixels (`z ≈ 0`): compare near the vertical wall (`x ∈ 1..8 mm`) vs exposed (`x > 55 mm`).
            if p.z.abs() < 0.001 && p.y.abs() < 0.02 {
                let l_off = luma(&no_ao.pixels[idx * 4..idx * 4 + 4]);
                let l_on = luma(&with_ao.pixels[idx * 4..idx * 4 + 4]);
                let delta = l_off - l_on;
                if p.x > 0.001 && p.x < 0.008 {
                    corner_hits += 1;
                    corner_delta = corner_delta.max(delta);
                } else if p.x > 0.055 && p.x < 0.095 {
                    exposed_hits += 1;
                    exposed_delta = exposed_delta.min(delta.abs());
                }
            }
        }
    }
    assert!(corner_hits > 0 && exposed_hits > 0, "expected both corner ({corner_hits}) and exposed ({exposed_hits}) floor hits");
    assert!(corner_delta > 12.0, "AO should darken concave corner noticeably, got delta={corner_delta}");
    assert!(exposed_delta < 2.0, "AO should not darken exposed open floor, got delta={exposed_delta}");
}

/// Studio soft SDF shadows darken the floor region occluded from the key light while leaving
/// unoccluded floor pixels bright.
#[test]
fn studio_soft_shadows_darken_occluded_floor() {
    let v = View {
        eye: [Length::mm(0.0), Length::mm(-220.0), Length::mm(180.0)],
        target: [Length::ZERO; 3],
        projection: Projection::Perspective { fov: Angle::degrees(40.0) },
        width: 64,
        height: 64,
        ..
    };
    let cam = Camera::new(&v).expect("cam");
    let key_dir = (cam.up * 0.72 - cam.right * 0.45 - cam.forward * 0.52).normalized().expect("key_dir");

    let mut g = Graph::new(GraphOpts { alloc: heap(), .. });
    let floor = g.cuboid(BoxOpts { size: [Length::mm(240.0), Length::mm(240.0), Length::mm(10.0)], .. }).expect("floor");
    let floor = g.place(floor, Placement::shift(Length::ZERO, Length::ZERO, Length::mm(-5.0))).expect("place floor");
    // Place an occluder sphere directly along `key_dir` from the floor centre `(0, 0, 0)`.
    let occluder = g.sphere(SphereOpts { radius: Length::mm(20.0), .. }).expect("occluder");
    let occluder = g
        .place(
            occluder,
            Placement::shift(Length::m(key_dir.x * 0.07), Length::m(key_dir.y * 0.07), Length::m(key_dir.z * 0.07)),
        )
        .expect("place occluder");
    let scene = g.union(floor, occluder).expect("scene");
    let tape = compile(&g, scene.id(), CompileOpts { alloc: heap(), .. }).expect("tape");

    let unshadowed = render(
        &tape,
        v,
        PictureOpts {
            alloc: heap(),
            scratch: heap(),
            shading: Shading::Studio(StudioOpts { ao_strength: 0.0, shadow_strength: 0.0, .. }),
            ..
        },
    )
    .expect("unshadowed");
    let shadowed = render(
        &tape,
        v,
        PictureOpts {
            alloc: heap(),
            scratch: heap(),
            shading: Shading::Studio(StudioOpts { ao_strength: 0.0, shadow_strength: 0.80, shadow_softness: 12.0, shadow_steps: 48, .. }),
            ..
        },
    )
    .expect("shadowed");

    // Centre pixel hits `(0, 0, 0)` on the floor directly beneath the key-light occluder.
    let centre_idx = ((v.height / 2) * v.width + (v.width / 2)) as usize;
    let l_unshadowed = luma(&unshadowed.pixels[centre_idx * 4..centre_idx * 4 + 4]);
    let l_shadowed = luma(&shadowed.pixels[centre_idx * 4..centre_idx * 4 + 4]);
    assert!(
        l_unshadowed - l_shadowed > 25.0,
        "occluded floor pixel should be much darker with shadows enabled: unshadowed={l_unshadowed}, shadowed={l_shadowed}"
    );

    // An unoccluded floor pixel on the far right should match closely.
    let lit_idx = ((v.height / 2) * v.width + (v.width - 8)) as usize;
    assert!(shadowed.depth[lit_idx].is_finite(), "far-right pixel should hit floor");
    let l_lit_off = luma(&unshadowed.pixels[lit_idx * 4..lit_idx * 4 + 4]);
    let l_lit_on = luma(&shadowed.pixels[lit_idx * 4..lit_idx * 4 + 4]);
    assert!(
        (l_lit_off - l_lit_on).abs() < 2.0,
        "unoccluded floor pixel should stay bright: off={l_lit_off}, on={l_lit_on}"
    );
}

/// 2×2 RGSS supersampling traces 4 rays per pixel and produces smooth intermediate edge colors
/// along curved silhouettes.
#[test]
fn supersample_2x2_smooths_silhouettes() {
    let mut g = Graph::new(GraphOpts { alloc: heap(), .. });
    let ball = g.sphere(SphereOpts { radius: Length::mm(40.0), .. }).expect("sphere");
    let tape = compile(&g, ball.id(), CompileOpts { alloc: heap(), .. }).expect("tape");

    let v = View {
        eye: [Length::mm(180.0), Length::ZERO, Length::ZERO],
        target: [Length::ZERO; 3],
        projection: Projection::Perspective { fov: Angle::degrees(38.0) },
        width: 48,
        height: 48,
        ..
    };
    let bg = [255, 255, 255, 255];
    let col = [20, 40, 80];

    let s1 = render(&tape, v, PictureOpts { alloc: heap(), scratch: heap(), background: bg, colour: col, supersample: 1, .. }).expect("1x");
    let s2 = render(&tape, v, PictureOpts { alloc: heap(), scratch: heap(), background: bg, colour: col, supersample: 2, .. }).expect("2x");

    assert_eq!(s1.stats.rays, u64::from(v.width * v.height));
    assert_eq!(s2.stats.rays, 4 * u64::from(v.width * v.height));

    let intermediate = |p: &Picture<'_>| {
        p.pixels.chunks_exact(4).filter(|px| px[0] >= 60 && px[0] <= 240).count()
    };
    assert_eq!(intermediate(&s1), 0, "1x1 classic render of dark solid on white bg has no blended silhouette pixels");
    assert!(intermediate(&s2) >= 20, "2x2 RGSS should produce smooth intermediate silhouette pixels, got {}", intermediate(&s2));

    // Invalid supersample values return typed errors.
    assert_eq!(
        render(&tape, v, PictureOpts { alloc: heap(), scratch: heap(), supersample: 0, .. }).unwrap_err(),
        PictureError::BadSupersample { supersample: 0 }
    );
    assert_eq!(
        render(&tape, v, PictureOpts { alloc: heap(), scratch: heap(), supersample: 3, .. }).unwrap_err(),
        PictureError::BadSupersample { supersample: 3 }
    );
}

/// Normal, Depth, and Matcap render modes produce accurate visual encodings.
#[test]
fn normals_depth_and_matcap_modes() {
    let mut g = Graph::new(GraphOpts { alloc: heap(), .. });
    let ball = g.sphere(SphereOpts { radius: Length::mm(40.0), .. }).expect("sphere");
    let tape = compile(&g, ball.id(), CompileOpts { alloc: heap(), .. }).expect("tape");

    let v = View {
        eye: [Length::mm(200.0), Length::ZERO, Length::ZERO],
        target: [Length::ZERO; 3],
        projection: Projection::Perspective { fov: Angle::degrees(35.0) },
        width: 48,
        height: 48,
        ..
    };
    let bg = [10, 20, 30, 255];

    let normals = render(&tape, v, PictureOpts { alloc: heap(), scratch: heap(), background: bg, mode: RenderMode::Normals, .. }).expect("normals");
    let c = ((v.height / 2) * v.width + (v.width / 2)) as usize;
    let cpx = &normals.pixels[c * 4..c * 4 + 4];
    // Front of sphere faces +X -> RGB ≈ [255, 128, 128, 255].
    assert!(cpx[0] >= 250, "front normal R should be ~255, got {}", cpx[0]);
    assert!((i32::from(cpx[1]) - 128).abs() <= 8, "front normal G should be ~128, got {}", cpx[1]);
    assert!((i32::from(cpx[2]) - 128).abs() <= 8, "front normal B should be ~128, got {}", cpx[2]);
    assert_eq!(&normals.pixels[0..4], &bg, "background pixel must stay background");

    let depth = render(&tape, v, PictureOpts { alloc: heap(), scratch: heap(), background: bg, mode: RenderMode::Depth, .. }).expect("depth");
    assert_eq!(depth.stats.grads, 0, "depth mode should not evaluate gradients");
    let near_val = depth.pixels[c * 4];
    // Find a hit pixel farther from centre (greater depth -> darker).
    let mut far_val = near_val;
    for x in 0..v.width / 2 {
        let idx = ((v.height / 2) * v.width + x) as usize;
        if depth.depth[idx].is_finite() {
            far_val = depth.pixels[idx * 4];
            break;
        }
    }
    assert!(near_val > far_val + 30, "nearer sphere centre ({near_val}) must be brighter than limb ({far_val})");

    let matcap = render(&tape, v, PictureOpts { alloc: heap(), scratch: heap(), background: bg, mode: RenderMode::Matcap, .. }).expect("matcap");
    assert_eq!(&matcap.pixels[0..4], &bg);
    assert_ne!(&matcap.pixels[c * 4..c * 4 + 4], &bg);
}

/// Studio shading with 2×2 RGSS and all render modes are strictly bit-for-bit deterministic.
#[test]
fn studio_and_modes_are_bit_deterministic() {
    let mut g = Graph::new(GraphOpts { alloc: heap(), .. });
    let box_ = g.cuboid(BoxOpts { size: [Length::mm(60.0), Length::mm(60.0), Length::mm(20.0)], round: Length::mm(3.0), .. }).expect("box");
    let ball = g.sphere(SphereOpts { radius: Length::mm(22.0), .. }).expect("ball");
    let ball = g.place(ball, Placement::shift(Length::mm(10.0), Length::mm(-8.0), Length::mm(20.0))).expect("place");
    let solid = g.union(box_, ball).expect("union");
    let tape = compile(&g, solid.id(), CompileOpts { alloc: heap(), .. }).expect("tape");

    let v = View {
        eye: [Length::mm(140.0), Length::mm(-120.0), Length::mm(100.0)],
        target: [Length::ZERO; 3],
        projection: Projection::Perspective { fov: Angle::degrees(38.0) },
        width: 36,
        height: 36,
        ..
    };

    let configs = [
        (RenderMode::Shaded, Shading::Studio(StudioOpts::DEFAULT), 2u8),
        (RenderMode::Normals, Shading::Classic, 2u8),
        (RenderMode::Depth, Shading::Classic, 1u8),
        (RenderMode::Matcap, Shading::Classic, 2u8),
    ];

    for (mode, shading, supersample) in configs {
        let o = PictureOpts { alloc: heap(), scratch: heap(), mode, shading, supersample, .. };
        let a = render(&tape, v, o).expect("render a");
        let b = render(&tape, v, o).expect("render b");
        assert_eq!(a.pixels[..], b.pixels[..], "pixels differ for {mode:?}");
        assert_eq!(a.stats, b.stats, "stats differ for {mode:?}");
        for (da, db) in a.depth.iter().zip(b.depth.iter()) {
            assert_eq!(da.to_bits(), db.to_bits(), "depth bits differ for {mode:?}");
        }
    }
}

blaze_sim::plants! {
    impl MeshPicturePlant breaks PictureBroken {
        None,
        IgnoreTransform => PictureBroken::Wrong { .. },
        FlatOnly => PictureBroken::Wrong { .. },
        IgnoreHighlights => PictureBroken::Wrong { .. },
        IgnoreTextures => PictureBroken::Wrong { .. },
    }
}

fn mesh_preview_case(tape: &Tape, plant: impl Planted<MeshPicturePlant> + Copy) -> Result<(), Broken<PictureBroken>> {
    use blaze_cad_mesh::{COS_32_DEGREES, FeatureRegion, MeshOpts, ShadeOpts, mesh, shade};
    use blaze_cad_out::{Material, Matrix, Transform};
    use blaze_math::Vec3;

    let radius_mm = tape.int_in(22..=34i64, "radius mm") as f64;
    let shift_x_mm = tape.int_in(40..=70i64, "shift x mm") as f64;
    let shift_z_mm = tape.int_in(-12..=12i64, "shift z mm") as f64;

    let mut g = Graph::new(GraphOpts { alloc: heap(), .. });
    let ball = g.sphere(SphereOpts { radius: Length::mm(radius_mm), .. }).map_err(harness)?;
    let tape_s = compile(&g, ball.id(), CompileOpts { alloc: heap(), .. }).map_err(harness)?;
    let meshed = mesh(&tape_s, MeshOpts { alloc: heap(), scratch: heap(), tolerance: Length::mm(1.5), .. }).map_err(harness)?;
    // Re-shade with smooth normals (`cos_crease = 0.0` so sphere normals interpolate smoothly).
    let smooth = shade(&meshed.mesh.view(), ShadeOpts { cos_crease: 0.0, alloc: heap(), scratch: heap() }).map_err(harness)?;
    let _ = COS_32_DEGREES;

    // Shift the sphere so IgnoreTransform fails to hit at the shifted target.
    let shifted = Transform::Matrix(Matrix {
        linear: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
        translation: [shift_x_mm / 1000.0, 0.0, shift_z_mm / 1000.0],
    });
    let parts = [MeshScenePart { mesh: smooth.view(), parent: None, transform: shifted, tint_override: None }];
    const MAT: [Material<'static>; 1] = [Material {
        name: "wood",
        base_colour: [0.55, 0.35, 0.18, 1.0],
        metallic: 0.0,
        roughness: 0.7,
        emissive: [0.0; 3],
        emissive_strength: None,
        clearcoat: None,
        double_sided: false,
    }];
    let v = View {
        eye: [Length::mm(shift_x_mm), Length::mm(-140.0), Length::mm(shift_z_mm)],
        target: [Length::mm(shift_x_mm), Length::ZERO, Length::mm(shift_z_mm)],
        projection: Projection::Orthographic { height: Length::mm(radius_mm * 2.8) },
        width: 32,
        height: 32,
        ..
    };

    let shaded = render_mesh_scene_planted(
        &parts,
        &MAT,
        v,
        MeshPictureOpts {
            alloc: heap(),
            scratch: heap(),
            mode: RenderMode::Normals,
            normal_source: MeshNormalSource::Shading,
            ..
        },
        plant,
    )
    .map_err(harness)?;

    let flat = render_mesh_scene_planted(
        &parts,
        &MAT,
        v,
        MeshPictureOpts {
            alloc: heap(),
            scratch: heap(),
            mode: RenderMode::Normals,
            normal_source: MeshNormalSource::Geometric,
            ..
        },
        plant,
    )
    .map_err(harness)?;

    let center = (16 * 32 + 16) as usize;
    if !shaded.depth[center].is_finite() {
        return Err(Broken::Oracle(PictureBroken::Wrong {
            why: String::from("center ray missed shifted sphere"),
        }));
    }
    let want_t = (140.0 - radius_mm) / 1000.0;
    let got_t = shaded.depth[center];
    if (got_t - want_t).abs() > 0.005 {
        return Err(Broken::Oracle(PictureBroken::Wrong {
            why: format!("center hit depth {got_t} far from {want_t} m"),
        }));
    }
    // Smooth shading normals must differ from flat geometric triangle normals on a coarse sphere.
    if shaded.pixels[..] == flat.pixels[..] {
        return Err(Broken::Oracle(PictureBroken::Wrong {
            why: String::from("Shading normals matched Geometric normals on sphere"),
        }));
    }

    // Highlight in part-local coordinates (front pole of the shifted sphere) must visibly alter
    // pixels under the shifted transform.
    let hl = [MeshHighlight {
        part_index: 0,
        region: FeatureRegion::Ball {
            center: Vec3::new(0.0, -radius_mm / 1000.0, 0.0),
            radius: Length::mm(radius_mm * 0.45),
        },
        style: HighlightStyle::Missed { colour: [250, 32, 64] },
    }];
    let highlighted = render_mesh_scene_planted(
        &parts,
        &MAT,
        v,
        MeshPictureOpts {
            alloc: heap(),
            scratch: heap(),
            mode: RenderMode::Normals,
            normal_source: MeshNormalSource::Shading,
            highlights: &hl,
            ..
        },
        plant,
    )
    .map_err(harness)?;
    if highlighted.pixels[..] == shaded.pixels[..] {
        return Err(Broken::Oracle(PictureBroken::Wrong {
            why: String::from("missed-feature highlight left rendered pixels unchanged"),
        }));
    }

    // Baked PBR base-colour texture on a UV-mapped quad must visibly alter rendered pixels
    // (catches `MeshPicturePlant::IgnoreTextures`).
    let quad_pos: [[f32; 3]; 4] = [[-0.05, 0.05, 0.0], [-0.05, -0.05, 0.0], [0.05, -0.05, 0.0], [0.05, 0.05, 0.0]];
    let quad_nrm: [[f32; 3]; 4] = [[0.0, 0.0, 1.0]; 4];
    let quad_uvs: [[f32; 2]; 4] = [[0.0, 0.0], [0.0, 1.0], [1.0, 1.0], [1.0, 0.0]];
    let quad_tris: [[u32; 3]; 2] = [[0, 1, 2], [0, 2, 3]];
    let quad_parts = [blaze_cad_mesh::PartId(0); 2];
    let quad_roles = [blaze_cad_mesh::MaterialRole(0); 2];
    let quad_view = blaze_cad_mesh::MeshView::new(blaze_cad_mesh::Columns {
        positions: &quad_pos,
        normals: &quad_nrm,
        tangents: &[],
        uvs: &quad_uvs,
        colours: &[],
        triangles: &quad_tris,
        parts: &quad_parts,
        roles: &quad_roles,
        creases: &[],
    })
    .map_err(harness)?;
    let quad_part = [MeshScenePart { mesh: quad_view, parent: None, transform: Transform::IDENTITY, tint_override: None }];
    const WHITE_MAT: [Material<'static>; 1] = [Material {
        name: "white",
        base_colour: [1.0, 1.0, 1.0, 1.0],
        metallic: 0.0,
        roughness: 0.6,
        emissive: [0.0; 3],
        emissive_strength: None,
        clearcoat: None,
        double_sided: false,
    }];
    let tex_rgba: [u8; 16] = [
        240, 20, 20, 255, 20, 40, 240, 255,
        240, 20, 20, 255, 20, 40, 240, 255,
    ];
    let maps = [MaterialMaps {
        base_colour: Some(TextureMap { width: 2, height: 2, rgba: &tex_rgba }),
        metallic_roughness: None,
        normal: None,
        normal_scale: 1.0,
    }];
    let qv = View {
        eye: [Length::ZERO, Length::ZERO, Length::mm(180.0)],
        target: [Length::ZERO; 3],
        up: [0.0, 1.0, 0.0],
        projection: Projection::Orthographic { height: Length::mm(80.0) },
        width: 16,
        height: 16,
        ..
    };
    let untextured = render_mesh_scene_planted(
        &quad_part,
        &WHITE_MAT,
        qv,
        MeshPictureOpts { alloc: heap(), scratch: heap(), shading: Shading::Classic, .. },
        plant,
    )
    .map_err(harness)?;
    let textured = render_mesh_scene_planted(
        &quad_part,
        &WHITE_MAT,
        qv,
        MeshPictureOpts { alloc: heap(), scratch: heap(), shading: Shading::Classic, material_maps: &maps, .. },
        plant,
    )
    .map_err(harness)?;
    if textured.pixels[..] == untextured.pixels[..] {
        return Err(Broken::Oracle(PictureBroken::Wrong {
            why: String::from("material_maps left rendered pixels unchanged"),
        }));
    }
    Ok(())
}

#[test]
fn mesh_preview_plants_are_caught() {
    controls(Config::cases(4), |tape, plant: MeshPicturePlant| mesh_preview_case(tape, plant));
}

#[test]
fn mesh_preview_matches_sdf_silhouette_and_draws_wireframe() {
    use blaze_cad_mesh::{MeshOpts, mesh};
    use blaze_cad_out::Material;

    let mut g = Graph::new(GraphOpts { alloc: heap(), .. });
    let cube = g.cuboid(BoxOpts { size: [Length::mm(50.0); 3], round: Length::mm(4.0), .. }).expect("cube");
    let tape = compile(&g, cube.id(), CompileOpts { alloc: heap(), .. }).expect("compile");
    let meshed = mesh(&tape, MeshOpts { alloc: heap(), scratch: heap(), tolerance: Length::mm(1.0), .. }).expect("mesh");

    let v = View {
        eye: [Length::mm(110.0), Length::mm(-110.0), Length::mm(90.0)],
        target: [Length::ZERO; 3],
        projection: Projection::Perspective { fov: Angle::degrees(35.0) },
        width: 48,
        height: 48,
        ..
    };

    const MAT: [Material<'static>; 1] = [Material {
        name: "steel",
        base_colour: [0.62, 0.64, 0.68, 1.0],
        metallic: 0.85,
        roughness: 0.35,
        emissive: [0.0; 3],
        emissive_strength: None,
        clearcoat: None,
        double_sided: false,
    }];

    let sdf_pic = render(&tape, v, PictureOpts { alloc: heap(), scratch: heap(), .. }).expect("sdf render");
    let mesh_pic1 = render_mesh(
        meshed.mesh.view(),
        &MAT,
        v,
        MeshPictureOpts { alloc: heap(), scratch: heap(), supersample: 2, .. },
    )
    .expect("mesh render 1");
    let mesh_pic2 = render_mesh(
        meshed.mesh.view(),
        &MAT,
        v,
        MeshPictureOpts { alloc: heap(), scratch: heap(), supersample: 2, .. },
    )
    .expect("mesh render 2");
    assert_eq!(mesh_pic1.pixels[..], mesh_pic2.pixels[..], "mesh preview must be bit-deterministic");

    // Silhouette hit agreement with the exact SDF render (interior pixels agree within tolerance).
    let mut both_hit = 0usize;
    for i in 0..(48 * 48) {
        if sdf_pic.depth[i].is_finite() && mesh_pic1.depth[i].is_finite() {
            both_hit += 1;
            let diff = (sdf_pic.depth[i] - mesh_pic1.depth[i]).abs();
            assert!(diff < 0.008, "pixel {i} depth diff {diff} too large");
        }
    }
    assert!(both_hit > 300, "expected substantial overlap with SDF render, got {both_hit}");

    // Wireframe overlay must visibly alter pixels relative to solid shading.
    let wire_pic = render_mesh(
        meshed.mesh.view(),
        &MAT,
        v,
        MeshPictureOpts {
            alloc: heap(),
            scratch: heap(),
            wireframe: Wireframe::Overlay { width_px: 1.2, colour: [20, 20, 25] },
            ..
        },
    )
    .expect("wireframe render");
    let solid_1x = render_mesh(
        meshed.mesh.view(),
        &MAT,
        v,
        MeshPictureOpts { alloc: heap(), scratch: heap(), .. },
    )
    .expect("solid 1x");
    assert_ne!(wire_pic.pixels[..], solid_1x.pixels[..], "wireframe overlay must draw visible triangle edges");
}

/// Zebra reflection stripes produce smooth alternating bands on a curved sphere, shift with phase,
/// match deterministically on both SDF and mesh tracers, and exhibit a sharp step kink across a
/// 90° creased box edge.
#[test]
fn zebra_mode_shows_smooth_stripes_on_sphere_and_kink_on_creased_box() {
    use blaze_cad_mesh::{MeshOpts, ShadeOpts, mesh, shade};
    use blaze_cad_out::Material;

    let mut gs = Graph::new(GraphOpts { alloc: heap(), .. });
    let ball = gs.sphere(SphereOpts { radius: Length::mm(45.0), .. }).expect("sphere");
    let sphere_tape = compile(&gs, ball.id(), CompileOpts { alloc: heap(), .. }).expect("sphere tape");

    let mut gb = Graph::new(GraphOpts { alloc: heap(), .. });
    let cube = gb.cuboid(BoxOpts { size: [Length::mm(80.0); 3], .. }).expect("box");
    let box_tape = compile(&gb, cube.id(), CompileOpts { alloc: heap(), .. }).expect("box tape");

    // Oblique view looking directly across the sharp 90° vertical edge between the -Y and +X faces.
    let v = View {
        eye: [Length::mm(160.0), Length::mm(-160.0), Length::mm(40.0)],
        target: [Length::ZERO; 3],
        projection: Projection::Orthographic { height: Length::mm(120.0) },
        width: 48,
        height: 48,
        ..
    };

    let zebra = ZebraOpts { stripes: 12.0, axis: [1.0, 0.0, 0.0], phase: 0.0, .. };
    let z_sphere_1 = render(
        &sphere_tape,
        v,
        PictureOpts { alloc: heap(), scratch: heap(), mode: RenderMode::Zebra(zebra), .. },
    )
    .expect("zebra sphere 1");
    let z_sphere_2 = render(
        &sphere_tape,
        v,
        PictureOpts { alloc: heap(), scratch: heap(), mode: RenderMode::Zebra(zebra), .. },
    )
    .expect("zebra sphere 2");
    assert_eq!(z_sphere_1.pixels[..], z_sphere_2.pixels[..], "SDF zebra render must be bit-deterministic");

    // Across the sphere's centre row, reflection stripes must alternate between dark and light bands.
    let row_y = v.height / 2;
    let mut transitions = 0u32;
    let mut prev_bright: Option<bool> = None;
    let mut min_luma = f64::INFINITY;
    let mut max_luma = 0.0f64;
    for x in 0..v.width {
        let idx = (row_y * v.width + x) as usize;
        if !z_sphere_1.depth[idx].is_finite() {
            continue;
        }
        let l = luma(&z_sphere_1.pixels[idx * 4..idx * 4 + 4]);
        min_luma = min_luma.min(l);
        max_luma = max_luma.max(l);
        let bright = l > 128.0;
        if let Some(pb) = prev_bright
            && pb != bright
        {
            transitions += 1;
        }
        prev_bright = Some(bright);
    }
    assert!(max_luma - min_luma > 180.0, "sphere zebra stripes should span dark to light: min={min_luma}, max={max_luma}");
    assert!(transitions >= 6, "sphere centre scanline should show multiple zebra bands, got {transitions}");

    // Shifting phase or switching to world space must visibly move the reflected stripes.
    let z_shifted = render(
        &sphere_tape,
        v,
        PictureOpts {
            alloc: heap(),
            scratch: heap(),
            mode: RenderMode::Zebra(ZebraOpts { stripes: 12.0, axis: [1.0, 0.0, 0.0], phase: 0.35, .. }),
            ..
        },
    )
    .expect("zebra shifted");
    assert_ne!(z_shifted.pixels[..], z_sphere_1.pixels[..], "zebra phase shift must roll stripes");

    let z_world = render(
        &sphere_tape,
        v,
        PictureOpts {
            alloc: heap(),
            scratch: heap(),
            mode: RenderMode::Zebra(ZebraOpts { stripes: 12.0, axis: [0.0, 0.0, 1.0], world_space: true, .. }),
            ..
        },
    )
    .expect("zebra world");
    assert_ne!(z_world.pixels[..], z_sphere_1.pixels[..], "world-space zebra axis must differ from camera-space");

    // Creased sharp box: the -Y face (left of centre) and +X face (right of centre) have opposite
    // reflection projections along camera-right, creating a sharp 1-pixel discontinuity at the crease.
    let z_box = render(
        &box_tape,
        v,
        PictureOpts { alloc: heap(), scratch: heap(), mode: RenderMode::Zebra(zebra), .. },
    )
    .expect("zebra box");
    let mut max_crease_jump = 0.0f64;
    let mut prev_hit_luma: Option<f64> = None;
    for x in 0..v.width {
        let idx = (row_y * v.width + x) as usize;
        if z_box.depth[idx].is_finite() {
            let l = luma(&z_box.pixels[idx * 4..idx * 4 + 4]);
            if let Some(pl) = prev_hit_luma {
                max_crease_jump = max_crease_jump.max((l - pl).abs());
            }
            prev_hit_luma = Some(l);
        } else {
            prev_hit_luma = None;
        }
    }
    assert!(
        max_crease_jump > 180.0,
        "sharp 90° box crease must create a discontinuous zebra stripe kink/jump, got {max_crease_jump}"
    );

    // Exported mesh tracer also supports deterministic zebra reflection mode.
    let meshed = mesh(&sphere_tape, MeshOpts { alloc: heap(), scratch: heap(), tolerance: Length::mm(1.5), .. }).expect("mesh");
    let smooth = shade(&meshed.mesh.view(), ShadeOpts { cos_crease: 0.0, alloc: heap(), scratch: heap() }).expect("shade");
    const MAT: [Material<'static>; 1] = [Material {
        name: "body",
        base_colour: [0.6, 0.6, 0.6, 1.0],
        metallic: 0.5,
        roughness: 0.3,
        emissive: [0.0; 3],
        emissive_strength: None,
        clearcoat: None,
        double_sided: false,
    }];
    let mesh_z1 = render_mesh(
        smooth.view(),
        &MAT,
        v,
        MeshPictureOpts { alloc: heap(), scratch: heap(), mode: RenderMode::Zebra(zebra), .. },
    )
    .expect("mesh zebra 1");
    let mesh_z2 = render_mesh(
        smooth.view(),
        &MAT,
        v,
        MeshPictureOpts { alloc: heap(), scratch: heap(), mode: RenderMode::Zebra(zebra), .. },
    )
    .expect("mesh zebra 2");
    assert_eq!(mesh_z1.pixels[..], mesh_z2.pixels[..], "mesh zebra render must be bit-deterministic");

    let mut mesh_min = f64::INFINITY;
    let mut mesh_max = 0.0f64;
    for i in 0..(v.width * v.height) as usize {
        if mesh_z1.depth[i].is_finite() {
            let l = luma(&mesh_z1.pixels[i * 4..i * 4 + 4]);
            mesh_min = mesh_min.min(l);
            mesh_max = mesh_max.max(l);
        }
    }
    assert!(mesh_max - mesh_min > 180.0, "mesh zebra must show high-contrast bands: min={mesh_min}, max={mesh_max}");
}

/// Movable studio strip lights add deterministic specular reflection ribbons on both SDF and mesh
/// studio renders, move when repositioned, and leave default `StudioOpts::DEFAULT` bit-identical.
#[test]
fn strip_lights_add_grazing_reflection_ribbons_deterministically() {
    use blaze_cad_mesh::{MeshOpts, ShadeOpts, mesh, shade};
    use blaze_cad_out::Material;

    assert_eq!(StudioOpts::DEFAULT.strip_lights, [None; 4]);

    let mut g = Graph::new(GraphOpts { alloc: heap(), .. });
    let ball = g.sphere(SphereOpts { radius: Length::mm(42.0), .. }).expect("sphere");
    let tape = compile(&g, ball.id(), CompileOpts { alloc: heap(), .. }).expect("tape");

    let v = View {
        eye: [Length::mm(180.0), Length::ZERO, Length::ZERO],
        target: [Length::ZERO; 3],
        projection: Projection::Perspective { fov: Angle::degrees(38.0) },
        width: 48,
        height: 48,
        ..
    };

    // Baseline StudioOpts::DEFAULT (no strip lights).
    let base_sdf = render(
        &tape,
        v,
        PictureOpts {
            alloc: heap(),
            scratch: heap(),
            colour: [80, 90, 105],
            shading: Shading::Studio(StudioOpts::DEFAULT),
            ..
        },
    )
    .expect("base sdf");
    let explicit_none = render(
        &tape,
        v,
        PictureOpts {
            alloc: heap(),
            scratch: heap(),
            colour: [80, 90, 105],
            shading: Shading::Studio(StudioOpts { strip_lights: [None; 4], .. }),
            ..
        },
    )
    .expect("explicit none");
    assert_eq!(base_sdf.pixels[..], explicit_none.pixels[..], "all-None strip lights must be bit-identical to default");

    // Overhead horizontal strip light vs grazing side strip light on SDF.
    let strip_horiz_opts = StudioOpts {
        strip_lights: [Some(StripLight::OVERHEAD_HORIZONTAL), None, None, None],
        ..
    };
    let strip_side_opts = StudioOpts {
        strip_lights: [Some(StripLight::GRAZING_SIDE), None, None, None],
        ..
    };

    let horiz_1 = render(
        &tape,
        v,
        PictureOpts { alloc: heap(), scratch: heap(), colour: [80, 90, 105], shading: Shading::Studio(strip_horiz_opts), .. },
    )
    .expect("horiz 1");
    let horiz_2 = render(
        &tape,
        v,
        PictureOpts { alloc: heap(), scratch: heap(), colour: [80, 90, 105], shading: Shading::Studio(strip_horiz_opts), .. },
    )
    .expect("horiz 2");
    assert_eq!(horiz_1.pixels[..], horiz_2.pixels[..], "strip light render must be bit-deterministic");

    let side_sdf = render(
        &tape,
        v,
        PictureOpts { alloc: heap(), scratch: heap(), colour: [80, 90, 105], shading: Shading::Studio(strip_side_opts), .. },
    )
    .expect("side sdf");
    assert_ne!(horiz_1.pixels[..], side_sdf.pixels[..], "repositioning strip light must move the reflection ribbon");

    let max_hit_delta = |a: &Picture<'_>, b: &Picture<'_>| -> f64 {
        let mut best = 0.0f64;
        for i in 0..(v.width * v.height) as usize {
            if a.depth[i].is_finite() {
                let d = luma(&b.pixels[i * 4..i * 4 + 4]) - luma(&a.pixels[i * 4..i * 4 + 4]);
                best = best.max(d);
            }
        }
        best
    };
    let sdf_boost = max_hit_delta(&base_sdf, &horiz_1);
    assert!(sdf_boost > 35.0, "strip light should add bright specular ribbon on SDF sphere, got max delta={sdf_boost}");

    // Same verification on exported mesh studio PBR path.
    let meshed = mesh(&tape, MeshOpts { alloc: heap(), scratch: heap(), tolerance: Length::mm(1.5), .. }).expect("mesh");
    let smooth = shade(&meshed.mesh.view(), ShadeOpts { cos_crease: 0.0, alloc: heap(), scratch: heap() }).expect("shade");
    const MAT: [Material<'static>; 1] = [Material {
        name: "brushed_metal",
        base_colour: [0.22, 0.25, 0.30, 1.0],
        metallic: 0.75,
        roughness: 0.22,
        emissive: [0.0; 3],
        emissive_strength: None,
        clearcoat: None,
        double_sided: false,
    }];
    let base_mesh = render_mesh(
        smooth.view(),
        &MAT,
        v,
        MeshPictureOpts { alloc: heap(), scratch: heap(), shading: Shading::Studio(StudioOpts::DEFAULT), .. },
    )
    .expect("base mesh");
    let strip_mesh_1 = render_mesh(
        smooth.view(),
        &MAT,
        v,
        MeshPictureOpts { alloc: heap(), scratch: heap(), shading: Shading::Studio(strip_side_opts), .. },
    )
    .expect("strip mesh 1");
    let strip_mesh_2 = render_mesh(
        smooth.view(),
        &MAT,
        v,
        MeshPictureOpts { alloc: heap(), scratch: heap(), shading: Shading::Studio(strip_side_opts), .. },
    )
    .expect("strip mesh 2");
    assert_eq!(strip_mesh_1.pixels[..], strip_mesh_2.pixels[..], "mesh strip light render must be bit-deterministic");

    let mesh_boost = max_hit_delta(&base_mesh, &strip_mesh_1);
    assert!(mesh_boost > 30.0, "grazing strip light should create bright ribbon on metallic mesh, got max delta={mesh_boost}");
}

/// Exported mesh renderer samples baked base-colour, metallic-roughness, and tangent-space normal
/// maps deterministically and catches `MeshPicturePlant::IgnoreTextures`.
#[test]
fn mesh_renderer_samples_baked_base_roughness_and_normal_maps_and_catches_plant() {
    use blaze_cad_mesh::{Columns, MaterialRole, MeshView, PartId};
    use blaze_cad_out::{Material, Transform};

    // Front-facing 100×100 mm quad in the z = 0 plane with unit +Z normals, +X tangents (w = +1),
    // and [0, 1]² UVs.
    let positions: [[f32; 3]; 4] = [[-0.05, 0.05, 0.0], [-0.05, -0.05, 0.0], [0.05, -0.05, 0.0], [0.05, 0.05, 0.0]];
    let normals: [[f32; 3]; 4] = [[0.0, 0.0, 1.0]; 4];
    let tangents: [[f32; 4]; 4] = [[1.0, 0.0, 0.0, 1.0]; 4];
    let uvs: [[f32; 2]; 4] = [[0.0, 0.0], [0.0, 1.0], [1.0, 1.0], [1.0, 0.0]];
    let triangles: [[u32; 3]; 2] = [[0, 1, 2], [0, 2, 3]];
    let parts_col = [PartId(0); 2];
    let roles_col = [MaterialRole(0); 2];

    let quad = MeshView::new(Columns {
        positions: &positions,
        normals: &normals,
        tangents: &tangents,
        uvs: &uvs,
        colours: &[],
        triangles: &triangles,
        parts: &parts_col,
        roles: &roles_col,
        creases: &[],
    })
    .expect("quad view");
    let scene_parts = [MeshScenePart { mesh: quad, parent: None, transform: Transform::IDENTITY, tint_override: None }];

    const MAT: [Material<'static>; 1] = [Material {
        name: "baked_pbr",
        base_colour: [0.95, 0.95, 0.95, 1.0],
        metallic: 1.0,
        roughness: 1.0,
        emissive: [0.0; 3],
        emissive_strength: None,
        clearcoat: None,
        double_sided: false,
    }];

    // 4×4 base-colour map: left 2 columns vivid red, right 2 columns vivid blue.
    let mut base_rgba = [0u8; 4 * 4 * 4];
    // 4×4 metallic-roughness map: top 2 rows smooth metallic (G=25, B=230), bottom 2 rows rough dielectric (G=240, B=10).
    let mut mr_rgba = [0u8; 4 * 4 * 4];
    // 4×4 tangent-space normal map: left 2 columns tilted -X (R=35, G=128, B=220), right 2 columns tilted +X (R=220, G=128, B=220).
    let mut normal_rgba = [0u8; 4 * 4 * 4];

    for y in 0..4usize {
        for x in 0..4usize {
            let off = (y * 4 + x) * 4;
            let bc = if x < 2 { [240, 28, 28, 255] } else { [28, 52, 240, 255] };
            base_rgba[off..off + 4].copy_from_slice(&bc);

            let mr = if y < 2 { [0, 25, 230, 255] } else { [0, 240, 10, 255] };
            mr_rgba[off..off + 4].copy_from_slice(&mr);

            let nm = if x < 2 { [35, 128, 220, 255] } else { [220, 128, 220, 255] };
            normal_rgba[off..off + 4].copy_from_slice(&nm);
        }
    }

    let v = View {
        eye: [Length::ZERO, Length::ZERO, Length::mm(200.0)],
        target: [Length::ZERO; 3],
        up: [0.0, 1.0, 0.0],
        projection: Projection::Orthographic { height: Length::mm(80.0) },
        width: 32,
        height: 32,
        ..
    };

    // 1. Base-colour map sampling (left red, right blue).
    let base_maps = [MaterialMaps {
        base_colour: Some(TextureMap { width: 4, height: 4, rgba: &base_rgba }),
        metallic_roughness: None,
        normal: None,
        normal_scale: 1.0,
    }];
    let pic_base = render_mesh_scene(
        &scene_parts,
        &MAT,
        v,
        MeshPictureOpts { alloc: heap(), scratch: heap(), shading: Shading::Classic, material_maps: &base_maps, .. },
    )
    .expect("base map render");
    let left_idx = (16 * 32 + 6) * 4;
    let right_idx = (16 * 32 + 25) * 4;
    assert!(
        pic_base.pixels[left_idx] > pic_base.pixels[left_idx + 2] + 80,
        "left half should sample red texels, got {:?}",
        &pic_base.pixels[left_idx..left_idx + 4]
    );
    assert!(
        pic_base.pixels[right_idx + 2] > pic_base.pixels[right_idx] + 80,
        "right half should sample blue texels, got {:?}",
        &pic_base.pixels[right_idx..right_idx + 4]
    );

    // 2. Tangent-space normal map sampling (visualized directly via RenderMode::Normals).
    let normal_maps = [MaterialMaps {
        base_colour: None,
        metallic_roughness: None,
        normal: Some(TextureMap { width: 4, height: 4, rgba: &normal_rgba }),
        normal_scale: 1.0,
    }];
    let pic_norm = render_mesh_scene(
        &scene_parts,
        &MAT,
        v,
        MeshPictureOpts { alloc: heap(), scratch: heap(), mode: RenderMode::Normals, material_maps: &normal_maps, .. },
    )
    .expect("normal map render");
    let left_nx = pic_norm.pixels[left_idx];
    let right_nx = pic_norm.pixels[right_idx];
    assert!(
        right_nx > left_nx + 80,
        "tangent-space normal map should tilt left normal -X ({left_nx}) and right normal +X ({right_nx})"
    );

    // 3. Metallic-roughness map sampling (top smooth metallic vs bottom rough dielectric under studio + strip light).
    let mr_maps = [MaterialMaps {
        base_colour: None,
        metallic_roughness: Some(TextureMap { width: 4, height: 4, rgba: &mr_rgba }),
        normal: None,
        normal_scale: 1.0,
    }];
    let mr_studio = StudioOpts {
        ao_strength: 0.0,
        shadow_strength: 0.0,
        strip_lights: [
            Some(StripLight {
                direction: [0.0, 0.35, 0.94],
                tangent: [1.0, 0.0, 0.0],
                half_length: 0.12,
                half_width: 0.05,
                intensity: 1.8,
                ..
            }),
            None,
            None,
            None,
        ],
        ..
    };
    let pic_mr = render_mesh_scene(
        &scene_parts,
        &MAT,
        v,
        MeshPictureOpts { alloc: heap(), scratch: heap(), shading: Shading::Studio(mr_studio), material_maps: &mr_maps, .. },
    )
    .expect("metallic_roughness render");
    let top_luma = luma(&pic_mr.pixels[(6 * 32 + 16) * 4..(6 * 32 + 16) * 4 + 4]);
    let bot_luma = luma(&pic_mr.pixels[(25 * 32 + 16) * 4..(25 * 32 + 16) * 4 + 4]);
    assert!(
        (bot_luma - top_luma).abs() > 25.0,
        "metallic-roughness map must produce distinct shading on top ({top_luma}) vs bottom ({bot_luma})"
    );

    // 4. Planted bug `MeshPicturePlant::IgnoreTextures` suppresses all baked maps and is caught.
    let full_maps = [MaterialMaps {
        base_colour: Some(TextureMap { width: 4, height: 4, rgba: &base_rgba }),
        metallic_roughness: Some(TextureMap { width: 4, height: 4, rgba: &mr_rgba }),
        normal: Some(TextureMap { width: 4, height: 4, rgba: &normal_rgba }),
        normal_scale: 1.0,
    }];
    let unplanted = render_mesh_scene_planted(
        &scene_parts,
        &MAT,
        v,
        MeshPictureOpts { alloc: heap(), scratch: heap(), material_maps: &full_maps, .. },
        NoPlant,
    )
    .expect("unplanted");
    let planted = render_mesh_scene_planted(
        &scene_parts,
        &MAT,
        v,
        MeshPictureOpts { alloc: heap(), scratch: heap(), material_maps: &full_maps, .. },
        MeshPicturePlant::IgnoreTextures,
    )
    .expect("planted");
    let no_maps = render_mesh_scene(
        &scene_parts,
        &MAT,
        v,
        MeshPictureOpts { alloc: heap(), scratch: heap(), .. },
    )
    .expect("no maps");
    assert_eq!(planted.pixels[..], no_maps.pixels[..], "IgnoreTextures plant should match untextured render");
    assert_ne!(unplanted.pixels[..], planted.pixels[..], "baked PBR maps must alter output vs IgnoreTextures plant");
}

/// Revision diff computes exact zero/non-zero metrics and heatmap, rejects mismatched sizes, and
/// `compose_side_by_side` / `compose_grid` place panels verbatim inside 1 px border frames.
#[test]
fn compare_diff_and_compose_side_by_side_and_grid() {
    let mut g1 = Graph::new(GraphOpts { alloc: heap(), .. });
    let s1 = g1.sphere(SphereOpts { radius: Length::mm(32.0), .. }).expect("s1");
    let t1 = compile(&g1, s1.id(), CompileOpts { alloc: heap(), .. }).expect("t1");

    let mut g2 = Graph::new(GraphOpts { alloc: heap(), .. });
    let s2 = g2.sphere(SphereOpts { radius: Length::mm(44.0), .. }).expect("s2");
    let t2 = compile(&g2, s2.id(), CompileOpts { alloc: heap(), .. }).expect("t2");

    let v = View {
        eye: [Length::mm(180.0), Length::ZERO, Length::ZERO],
        target: [Length::ZERO; 3],
        projection: Projection::Perspective { fov: Angle::degrees(38.0) },
        width: 24,
        height: 24,
        ..
    };
    let pic_a = render(&t1, v, PictureOpts { alloc: heap(), scratch: heap(), .. }).expect("pic_a");
    let pic_b = render(&t2, v, PictureOpts { alloc: heap(), scratch: heap(), .. }).expect("pic_b");
    let v_small = View { eye: v.eye, target: v.target, projection: v.projection, width: 16, height: 16, .. };
    let pic_small = render(&t1, v_small, PictureOpts { alloc: heap(), scratch: heap(), .. }).expect("small");

    // Identical pictures produce exact zero diff.
    let same = diff_pictures(&pic_a, &pic_a, 0, 4.0, heap()).expect("same diff");
    assert_eq!(same.rms_error, 0.0);
    assert_eq!(same.max_channel_delta, 0);
    assert_eq!(same.changed_pixels, 0);
    assert_eq!(same.total_pixels, 24 * 24);
    assert_eq!(same.changed_fraction, 0.0);
    assert_eq!((same.heatmap.width, same.heatmap.height), (24, 24));

    // Edited radius produces positive RMS, changed pixel count, and warm amber/crimson heatmap pixels.
    let edited = diff_pictures(&pic_a, &pic_b, 2, 4.0, heap()).expect("edited diff");
    assert!(edited.rms_error > 0.01, "expected non-zero RMS error, got {}", edited.rms_error);
    assert!(edited.max_channel_delta > 20, "expected large channel delta, got {}", edited.max_channel_delta);
    assert!(edited.changed_pixels > 20, "expected many changed pixels, got {}", edited.changed_pixels);
    assert!(edited.changed_fraction > 0.03 && edited.changed_fraction < 1.0);
    // Corner (0, 0) is background in both renders -> muted dark slate in heatmap.
    let bg_hm = &edited.heatmap.pixels[0..4];
    assert!(bg_hm[0] < 60 && bg_hm[1] < 70 && bg_hm[2] < 85 && bg_hm[3] == 255, "unchanged pixel should be dark slate: {bg_hm:?}");
    let hot_pixels = edited
        .heatmap
        .pixels
        .chunks_exact(4)
        .filter(|px| px[0] > 150 && px[0] > px[2] + 80)
        .count();
    assert_eq!(hot_pixels as u64, edited.changed_pixels, "every changed pixel should be highlighted in amber/crimson");

    // Dimension mismatch returns typed error.
    assert_eq!(
        diff_pictures(&pic_a, &pic_small, 0, 1.0, heap()).unwrap_err(),
        PictureError::DimensionMismatch { left: (24, 24), right: (16, 16) }
    );

    // Empty panel slice returns typed Empty error.
    assert_eq!(
        compose_side_by_side(&[], 4, [20, 20, 24, 255], heap()).unwrap_err(),
        PictureError::Empty { width: 0, height: 0 }
    );

    // Side-by-side composition places panels verbatim inside 1 px border frames with exact dimensions.
    let gap = 6u32;
    let bg = [240, 242, 244, 255];
    let sbs = compose_side_by_side(&[&pic_a, &pic_b], gap, bg, heap()).expect("sbs");
    let cell_w = 24 + 2 * PANEL_BORDER_PX;
    let cell_h = 24 + 2 * PANEL_BORDER_PX;
    assert_eq!(sbs.width, 2 * cell_w + gap);
    assert_eq!(sbs.height, cell_h);

    let px_at = |pic: &Picture<'_>, x: u32, y: u32| -> [u8; 4] {
        let off = ((y * pic.width + x) * 4) as usize;
        [pic.pixels[off], pic.pixels[off + 1], pic.pixels[off + 2], pic.pixels[off + 3]]
    };
    for y in 0..24 {
        for x in 0..24 {
            assert_eq!(px_at(&sbs, PANEL_BORDER_PX + x, PANEL_BORDER_PX + y), px_at(&pic_a, x, y));
            assert_eq!(px_at(&sbs, cell_w + gap + PANEL_BORDER_PX + x, PANEL_BORDER_PX + y), px_at(&pic_b, x, y));
        }
    }
    // Gutter pixel equals background.
    assert_eq!(px_at(&sbs, cell_w + 2, cell_h / 2), bg);

    // 2×2 contact sheet grid places all four panels at exact grid coordinates.
    let grid_gap = 4u32;
    let grid_bg = [24, 26, 30, 255];
    let grid = compose_grid(&[&pic_a, &pic_b, &edited.heatmap, &pic_a], 2, grid_gap, grid_bg, heap()).expect("grid");
    assert_eq!(grid.width, 2 * cell_w + grid_gap);
    assert_eq!(grid.height, 2 * cell_h + grid_gap);
    // Panel 2 (`edited.heatmap`) sits at row 1, col 0.
    let p2_x0 = PANEL_BORDER_PX;
    let p2_y0 = cell_h + grid_gap + PANEL_BORDER_PX;
    for y in 0..24 {
        for x in 0..24 {
            assert_eq!(px_at(&grid, p2_x0 + x, p2_y0 + y), px_at(&edited.heatmap, x, y));
        }
    }
    // Centre cross-gutter pixel equals grid background.
    assert_eq!(px_at(&grid, cell_w + 1, cell_h + 1), grid_bg);
}
