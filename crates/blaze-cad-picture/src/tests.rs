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
