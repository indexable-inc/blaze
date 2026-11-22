//! A hull section, shared by the examples: a half-pipe skin, four ring frames inside it, a deck
//! with a rounded hatch opening, and the hatch cover in the opening. Millimetres throughout;
//! `z` up, the keel along `x`, the deck's underside at `z = 0`.

#![allow(dead_code, reason = "each example uses a different part of the shared model")]

use blaze_cad::prelude::*;

pub(crate) const LENGTH: f64 = 600.0;
pub(crate) const RADIUS: f64 = 300.0;
pub(crate) const SKIN: f64 = 10.0;
pub(crate) const FRAME_THICKNESS: f64 = 10.0;
pub(crate) const FRAME_DEPTH: f64 = 60.0;
pub(crate) const FRAME_STEP: f64 = 150.0;
pub(crate) const FRAMES: u32 = 4;
pub(crate) const DECK: f64 = 12.0;
pub(crate) const OPENING: [f64; 2] = [300.0, 200.0];
pub(crate) const OPENING_CORNER: f64 = 25.0;

/// The parts at their own levels (the wall check needs an exact inside) and the cover's id.
pub(crate) struct Hull {
    pub(crate) skin: Part<Inside>,
    pub(crate) frames: Part<Inside>,
    pub(crate) deck: Part<Inside>,
    pub(crate) cover: Part<Exact>,
    pub(crate) cover_id: PartId,
}

fn mm(x: f64) -> Length {
    Length::mm(x)
}

fn up(z: f64) -> Placement {
    Placement::shift(Length::ZERO, Length::ZERO, mm(z))
}

/// The hull section, its cover `gap` millimetres in from the opening on every side.
pub(crate) fn build(model: &mut Model<'_>, gap: f64) -> Result<Hull, ModelError> {
    let g = model.graph();
    let along_x = Placement::rotation(Axis::Y, Angle::degrees(90.0));
    // Everything under the deck: a box, so the cuts keep each part exact inside.
    let below = g.cuboid(BoxOpts { size: mm3(LENGTH + 200.0, 2.0 * RADIUS + 200.0, RADIUS + 100.0), name: Some("below the deck"), .. })?;
    let below = g.place(below, up(-(RADIUS + 100.0) / 2.0))?;

    // The skin: a tube cut to its lower half, open at both ends.
    let outer = g.cylinder(CylinderOpts { radius: mm(RADIUS), height: mm(LENGTH), name: Some("skin outside") })?;
    let inner = g.cylinder(CylinderOpts { radius: mm(RADIUS - SKIN), height: mm(LENGTH + 200.0), name: Some("skin inside") })?;
    let tube = g.difference(outer, inner)?;
    let tube = g.place(tube, along_x)?;
    let skin = g.intersect(tube, below)?;

    // The frames: one ring against the skin, its lower half, repeated along the keel.
    let ring_out = g.cylinder(CylinderOpts { radius: mm(RADIUS - SKIN), height: mm(FRAME_THICKNESS), .. })?;
    let ring_in = g.cylinder(CylinderOpts { radius: mm(RADIUS - SKIN - FRAME_DEPTH), height: mm(FRAME_THICKNESS + 20.0), .. })?;
    let ring = g.difference(ring_out, ring_in)?;
    let ring = g.place(ring, along_x)?;
    let frame = g.intersect(ring, below)?;
    let frames = g.linear_repeat(frame, RepeatOpts { axis: Axis::X, step: mm(FRAME_STEP), count: FRAMES, name: Some("frames") })?;
    let first = -FRAME_STEP * f64::from(FRAMES - 1) / 2.0;
    let frames = g.place(frames, Placement::shift(mm(first), Length::ZERO, Length::ZERO))?;

    // The deck: a plate with a hatch opening rounded at its corners (the cutter is tall enough
    // that its rounded top and bottom edges are clear of the plate).
    let plate = g.cuboid(BoxOpts { size: mm3(LENGTH, 2.0 * RADIUS, DECK), name: Some("deck plate"), .. })?;
    let plate = g.place(plate, up(DECK / 2.0))?;
    let opening = g.cuboid(BoxOpts {
        size: mm3(OPENING[0], OPENING[1], 100.0),
        round: mm(OPENING_CORNER),
        name: Some("hatch opening"),
    })?;
    let opening = g.place(opening, up(DECK / 2.0))?;
    let deck = g.difference(plate, opening)?;

    // The cover: in the opening, `gap` in from it, standing proud of the deck.
    let cover = g.cuboid(BoxOpts {
        size: mm3(OPENING[0] - 2.0 * gap, OPENING[1] - 2.0 * gap, 30.0),
        round: mm(10.0),
        name: Some("hatch cover"),
    })?;
    let cover = g.place(cover, up(DECK))?;

    let (_, skin) = model.add(PartOpts { solid: skin, name: "skin" })?;
    let (_, frames) = model.add(PartOpts { solid: frames, name: "frames" })?;
    let (_, deck) = model.add(PartOpts { solid: deck, name: "deck" })?;
    let (cover_id, cover) = model.add(PartOpts { solid: cover, name: "hatch cover" })?;
    Ok(Hull { skin, frames, deck, cover, cover_id })
}

/// A box the whole section must fit in (for transport): 800 x 800 x 500 mm.
pub(crate) fn envelope(model: &mut Model<'_>) -> Result<Solid<Exact>, ModelError> {
    let g = model.graph();
    let b = g.cuboid(BoxOpts { size: mm3(800.0, 800.0, 500.0), name: Some("transport envelope"), .. })?;
    Ok(g.place(b, up(-120.0))?)
}

/// Three-quarter view from above the bow quarter.
pub(crate) fn view(width: u32, height: u32) -> View {
    View {
        eye: [mm(900.0), mm(-1100.0), mm(700.0)],
        target: [Length::ZERO, Length::ZERO, mm(-100.0)],
        projection: Projection::Perspective { fov: Angle::degrees(35.0) },
        width,
        height,
        ..
    }
}
