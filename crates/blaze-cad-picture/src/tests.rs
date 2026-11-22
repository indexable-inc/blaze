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
    }
}

fn mesh_preview_case(tape: &Tape, plant: impl Planted<MeshPicturePlant> + Copy) -> Result<(), Broken<PictureBroken>> {
    use blaze_cad_mesh::{COS_32_DEGREES, MeshOpts, ShadeOpts, mesh, shade};
    use blaze_cad_out::{Material, Matrix, Transform};

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
