//! A `Starfield` (harness-native-ui-text-api, prims.flecs `ui.Sky`): a night sky over a node, for a pane with nothing
//! in it yet. Pixel art, as the step items are (block.rs): chunky texels, a few flat shades, dithered where one shade
//! meets the next. Drawn here, never read from a file, and always moving:
//! - a nebula along the galaxy's band, in a handful of shades of indigo, teal and rose, drifting slowly;
//! - far stars drifting a little faster and near ones faster still (so the sky has depth), the near ones twinkling,
//!   the brightest with a small cross of light;
//! - now and then a shooting star.
//!
//! Everything fades to the ground at the edges, so it has no frame. The nebula is made once a size, on a strip that
//! repeats sideways; a step of the clock ([`FPS`] a second) is that strip copied across at its place and the stars put
//! on it. The host draws it at its calm rate while it is in view. In the light appearance it draws nothing.
use crate::draw::Draw;
use crate::pack::ImageInst;
use std::rc::Rc;

/// Steps of the sky's motion a second.
const FPS: f32 = 12.0;
/// Points a texel: the item's own (block.rs draws 1.5; a sky is seen from further off).
const TEXEL: f32 = 3.0;
/// The most texels a side.
const MOST: usize = 640;
/// The ground the sky fades to (the theme's dark ground, #0A0B0D).
const GROUND: [f32; 3] = [10.0, 11.0, 13.0];
/// Texels a second each layer drifts: the nebula, the far stars, the near stars.
const DRIFT: [f32; 3] = [0.5, 1.1, 2.4];

fn hash(x: u32, y: u32, seed: u32) -> f32 {
    let mut h = x.wrapping_mul(0x9E37_79B1) ^ y.wrapping_mul(0x85EB_CA6B) ^ seed.wrapping_mul(0xC2B2_AE35);
    h ^= h >> 15;
    h = h.wrapping_mul(0x2C1B_3C6D);
    h ^= h >> 12;
    h = h.wrapping_mul(0x297A_2D39);
    h ^= h >> 15;
    (h >> 8) as f32 / (1u32 << 24) as f32
}

/// Smooth noise at (`x`, `y`) that repeats every `wrap` across: values on a grid, eased between.
fn noise(x: f32, y: f32, wrap: u32, seed: u32) -> f32 {
    let (xi, yi) = (x.floor(), y.floor());
    let (fx, fy) = (x - xi, y - yi);
    let (ux, uy) = (fx * fx * (3.0 - 2.0 * fx), fy * fy * (3.0 - 2.0 * fy));
    let at = |dx: f32, dy: f32| hash(((xi + dx) as i64).rem_euclid(i64::from(wrap.max(1))) as u32, (yi + dy) as i32 as u32, seed);
    let (a, b) = (at(0.0, 0.0) + (at(1.0, 0.0) - at(0.0, 0.0)) * ux, at(0.0, 1.0) + (at(1.0, 1.0) - at(0.0, 1.0)) * ux);
    a + (b - a) * uy
}

/// Noise in layers, each twice as fine and half as strong: clouds, repeating every `wrap` across.
fn clouds(x: f32, y: f32, wrap: u32, seed: u32) -> f32 {
    let (mut sum, mut amp, mut f) = (0.0, 0.5, 1u32);
    for k in 0..4 {
        sum += amp * noise(x * f as f32, y * f as f32, wrap * f, seed + k);
        amp *= 0.5;
        f *= 2;
    }
    sum / 0.9375
}

/// A star: where it is on its layer's strip, how bright, how fast it twinkles and from where in its cycle, its tint.
struct Star {
    x: f32,
    y: usize,
    bright: f32,
    speed: f32,
    phase: f32,
    tint: [f32; 3],
}

/// The sky at one size: its nebula's strip (as wide as the sky, repeating), its stars, and each column's fade.
struct Field {
    w: usize,
    h: usize,
    nebula: Vec<[u8; 3]>,
    far: Vec<Star>,
    near: Vec<Star>,
    edge: Vec<f32>,
}

impl Field {
    fn new(w: usize, h: usize) -> Self {
        // Grid cells across the strip: a whole number, so the clouds meet themselves where it repeats.
        let cells = ((w as f32 * 3.4 / h.max(1) as f32).round() as u32).max(2);
        let (sx, sy) = (cells as f32 / w as f32, 3.4 / h.max(1) as f32);
        // A 4 by 4 ordered dither: where one shade meets the next the texels alternate, pixel art's gradient.
        const BAYER: [[f32; 4]; 4] = [[0.0, 8.0, 2.0, 10.0], [12.0, 4.0, 14.0, 6.0], [3.0, 11.0, 1.0, 9.0], [15.0, 7.0, 13.0, 5.0]];
        let mut nebula = vec![[0u8; 3]; w * h]; // alloc: the strip, made once a size
        for y in 0..h {
            let v = y as f32 / h as f32;
            // The band runs level across the strip (it repeats sideways), a little under the middle; and the sky goes
            // back to the ground toward its top and foot.
            let d = (v - 0.56) * 3.0;
            let along = (-d * d).exp();
            let fade = (v.min(1.0 - v) * 5.0).clamp(0.0, 1.0);
            for x in 0..w {
                let (px, py) = (x as f32 * sx, y as f32 * sy);
                let cloud = clouds(px, py, cells, 11);
                let lanes = clouds(px * 2.0, py * 2.0 + 9.0, cells * 2, 37);
                let dense = ((cloud - 0.4) * 2.6).clamp(0.0, 1.0) * (0.2 + 0.8 * along) * (1.0 - 0.75 * ((lanes - 0.5) * 3.2).clamp(0.0, 1.0));
                let hue = clouds(px * 0.5, py * 0.5 + 40.0, cells, 71);
                let tint = if hue < 0.42 { [22.0, 84.0, 104.0] } else if hue < 0.62 { [46.0, 38.0, 110.0] } else { [120.0, 48.0, 96.0] };
                // Five flat shades, dithered between.
                let level = ((dense * fade * 0.8 * 5.0 + (BAYER[y % 4][x % 4] + 0.5) / 16.0).floor() / 5.0).min(1.0);
                let c = [0, 1, 2].map(|k| (GROUND[k] + (tint[k] - GROUND[k]) * level) as u8);
                nebula[y * w + x] = c;
            }
        }
        let stars = |n: usize, seed: u32, lo: f32, hi: f32| -> Vec<Star> {
            (0..n as u32)
                .map(|i| {
                    let y = (hash(i, 2, seed) * h as f32) as usize;
                    let warm = hash(i, 5, seed);
                    let tint = if warm > 0.82 { [1.0, 0.84, 0.66] } else if warm < 0.22 { [0.72, 0.84, 1.0] } else { [1.0, 1.0, 1.0] };
                    Star { x: hash(i, 1, seed) * w as f32, y: y.min(h.saturating_sub(1)), bright: lo + (hi - lo) * hash(i, 3, seed).powi(3), speed: 0.8 + 2.6 * hash(i, 4, seed), phase: hash(i, 6, seed) * 6.283, tint }
                })
                .collect()
        };
        let edge = (0..w)
            .map(|x| {
                let u = x as f32 / w as f32;
                let e = (u.min(1.0 - u) * 6.0).clamp(0.0, 1.0);
                e * e * (3.0 - 2.0 * e)
            })
            .collect();
        Self { w, h, nebula, far: stars(w * h / 90, 5, 26.0, 120.0), near: stars((w * h / 700).max(24), 99, 90.0, 255.0), edge }
    }

    /// The sky at `time` seconds.
    fn at(&self, time: f32) -> Vec<u8> {
        let (w, h) = (self.w, self.h);
        let mut px = vec![0u8; w * h * 4]; // alloc: one picture a step of the sky's clock
        let shift = (time * DRIFT[0]) as usize % w.max(1);
        for y in 0..h {
            let row = &self.nebula[y * w..(y + 1) * w];
            for x in 0..w {
                let c = row[(x + shift) % w];
                let e = self.edge[x];
                let at = (y * w + x) * 4;
                for k in 0..3 {
                    px[at + k] = (GROUND[k] + (f32::from(c[k]) - GROUND[k]) * e) as u8;
                }
                px[at + 3] = 255;
            }
        }
        let fade_y = |y: i64| ((y.min(h as i64 - 1 - y) as f32 / h as f32) * 6.0).clamp(0.0, 1.0);
        let mut add = |x: i64, y: i64, c: [f32; 3], level: f32| {
            if x < 0 || y < 0 || x >= w as i64 || y >= h as i64 {
                return;
            }
            let at = (y as usize * w + x as usize) * 4;
            let level = level * self.edge[x as usize] * fade_y(y);
            for k in 0..3 {
                px[at + k] = (f32::from(px[at + k]) + c[k] * level).min(255.0) as u8;
            }
        };
        for (layer, drift) in [(&self.far, DRIFT[1]), (&self.near, DRIFT[2])] {
            for s in layer {
                let x = (s.x - time * drift).rem_euclid(w as f32) as i64;
                let y = s.y as i64;
                // Twinkling in steps, as pixel art does: full, dimmer, dim.
                let wave = (time * s.speed + s.phase).sin();
                let b = s.bright * if wave > 0.3 { 1.0 } else if wave > -0.5 { 0.72 } else { 0.45 };
                add(x, y, s.tint, b);
                if s.bright > 170.0 && wave > 0.3 {
                    for (dx, dy) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
                        add(x + dx, y + dy, s.tint, b * 0.34);
                    }
                }
            }
        }
        // A shooting star: one about every twelve seconds, crossing for a little under a second.
        let (n, t) = ((time / 12.0).floor(), (time % 12.0) / 0.8);
        if t < 1.0 && hash(n as u32, 0, 123) > 0.25 {
            let (x0, y0) = (hash(n as u32, 1, 123) * w as f32 * 0.7 + w as f32 * 0.15, hash(n as u32, 2, 123) * h as f32 * 0.4 + h as f32 * 0.08);
            let dx = if hash(n as u32, 3, 123) > 0.5 { 1.0 } else { -1.0 };
            let reach = 0.3 * w as f32;
            let fade = (t * 6.0).min(1.0) * (1.0 - t);
            for k in 0..22 {
                let back = k as f32;
                let (x, y) = (x0 + dx * (reach * t - back), y0 + 0.45 * (reach * t - back));
                add(x as i64, y as i64, [1.0, 1.0, 1.0], 240.0 * fade * (1.0 - back / 22.0).powi(2));
            }
        }
        px
    }
}

/// Every `Starfield` node's sky: one field (they share a size nearly always) and the picture last drawn.
#[derive(Default)]
pub struct Sky {
    field: Option<Field>,
    last: Option<(u32, Rc<[u8]>)>,
    /// A sky was painted this frame: it moves.
    live: bool,
}

impl Sky {
    /// Paint the sky over `rect` at frame time `time`: [`TEXEL`] points a texel on whole device pixels (`sc` a point),
    /// centred, each a crisp block. Nothing in the light appearance.
    pub fn paint(&mut self, d: &mut Draw, rect: [f32; 4], alpha: f32, dark: bool, sc: f32, time: f32) {
        if !dark || rect[2] < 24.0 || rect[3] < 24.0 {
            return;
        }
        let px = (TEXEL * sc).round().max(1.0);
        let cell = px / sc;
        let (w, h) = (((rect[2] / cell).ceil() as usize).min(MOST), ((rect[3] / cell).ceil() as usize).min(MOST));
        if self.field.as_ref().is_none_or(|f| f.w != w || f.h != h) {
            self.field = Some(Field::new(w, h));
            self.last = None;
        }
        let Some(field) = &self.field else { return };
        let step = (time.max(0.0) * FPS) as u32;
        let texels = match &self.last {
            Some((s, p)) if *s == step => p.clone(),
            _ => {
                let p: Rc<[u8]> = field.at(step as f32 / FPS).into();
                self.last = Some((step, p.clone()));
                p
            }
        };
        self.live = true;
        let snap = |v: f32| (v * sc).round() / sc;
        let at = [snap(rect[0]), snap(rect[1]), w as f32 * cell, h as f32 * cell];
        // Cut to the node: the last column and row of texels may reach past it.
        let clip = [rect[1], rect[1] + rect[3], 0.0, 0.0];
        let clipx = [rect[0], rect[0] + rect[2], 0.0, 0.0];
        d.image(ImageInst { rect: at, size: [w as f32, h as f32, alpha, 0.0], clip, clipx, light: [0.0; 4], hdr: [0.0; 4] }, texels);
    }

    /// After a frame: whether a sky was painted in it (the host keeps drawing, at its calm rate). One no longer painted
    /// is let go.
    pub fn end_frame(&mut self) -> bool {
        let live = std::mem::take(&mut self.live);
        if !live {
            self.field = None;
            self.last = None;
        }
        live
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sky's corners are the ground (no frame), it shows a nebula in a few flat shades, it moves from second to
    /// second, its strip meets itself where it repeats, and it is the same sky every time.
    #[test]
    fn the_sky_is_framed_by_the_ground_and_moves() {
        let f = Field::new(200, 120);
        let frame = f.at(0.0);
        let px = |p: &[u8], x: usize, y: usize| [p[(y * 200 + x) * 4], p[(y * 200 + x) * 4 + 1], p[(y * 200 + x) * 4 + 2]];
        for (x, y) in [(0, 0), (199, 0), (0, 119), (199, 119)] {
            assert_eq!(px(&frame, x, y), [10, 11, 13], "the corner at {x},{y} is not the ground");
        }
        let mut shades: Vec<[u8; 3]> = f.nebula.clone();
        shades.sort_unstable();
        shades.dedup();
        assert!((4..=16).contains(&shades.len()), "{} shades in the nebula: flat shades, a few", shades.len());
        assert!(frame != f.at(2.0), "nothing moves");
        // Where the strip repeats, the cloud goes on: the column before the seam is like the first one.
        let differ = (0..120).filter(|&y| f.nebula[y * 200] != f.nebula[y * 200 + 199]).count();
        assert!(differ < 60, "the strip's two ends differ on {differ} of 120 rows: a seam");
        assert_eq!(Field::new(200, 120).at(3.0), f.at(3.0), "the same sky each time");
    }
}
