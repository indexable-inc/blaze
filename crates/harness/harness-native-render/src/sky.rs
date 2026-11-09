//! A `Starfield` (harness-native-ui-text-api, prims.flecs `ui.Sky`): a night sky over a node, for a pane with nothing
//! in it yet. Drawn here, never read from a file:
//! - the ground it sits on, going to a faint nebula (layered noise in indigo, teal and rose) along a band across the
//!   sky, the galaxy's, and fading back to the ground at the edges so it has no frame;
//! - the far stars, thousands, a pixel each, thicker along the band;
//! - a few hundred nearer ones that twinkle, the brightest with a small cross of light;
//! - now and then a shooting star.
//!
//! The nebula and the far stars are made once a size; only the near stars and the shooting star are drawn again, a
//! few times a second, so it costs a copy of the picture and a few hundred pixels. The host draws it at its calm rate
//! while it is in view. In the light appearance it draws nothing: a night sky on paper is a grey smear.
use crate::draw::Draw;
use crate::pack::ImageInst;
use std::rc::Rc;

/// Steps of the sky's motion a second.
const FPS: f32 = 10.0;
/// The most texels a side: a larger node shows them stretched.
const MOST: usize = 1400;
/// The ground the sky fades to at its edges (the theme's dark ground, #0A0B0D).
const GROUND: [f32; 3] = [10.0, 11.0, 13.0];

fn hash(x: u32, y: u32, seed: u32) -> f32 {
    let mut h = x.wrapping_mul(0x9E37_79B1) ^ y.wrapping_mul(0x85EB_CA6B) ^ seed.wrapping_mul(0xC2B2_AE35);
    h ^= h >> 15;
    h = h.wrapping_mul(0x2C1B_3C6D);
    h ^= h >> 12;
    h = h.wrapping_mul(0x297A_2D39);
    h ^= h >> 15;
    (h >> 8) as f32 / (1u32 << 24) as f32
}

/// Smooth noise at (`x`, `y`): values on a grid, eased between.
fn noise(x: f32, y: f32, seed: u32) -> f32 {
    let (xi, yi) = (x.floor(), y.floor());
    let (fx, fy) = (x - xi, y - yi);
    let (ux, uy) = (fx * fx * (3.0 - 2.0 * fx), fy * fy * (3.0 - 2.0 * fy));
    let at = |dx: f32, dy: f32| hash((xi + dx) as i32 as u32, (yi + dy) as i32 as u32, seed);
    let (a, b) = (at(0.0, 0.0) + (at(1.0, 0.0) - at(0.0, 0.0)) * ux, at(0.0, 1.0) + (at(1.0, 1.0) - at(0.0, 1.0)) * ux);
    a + (b - a) * uy
}

/// Noise in layers, each twice as fine and half as strong: clouds.
fn clouds(x: f32, y: f32, seed: u32) -> f32 {
    let (mut sum, mut amp, mut f) = (0.0, 0.5, 1.0);
    for k in 0..5 {
        sum += amp * noise(x * f, y * f, seed + k);
        amp *= 0.5;
        f *= 2.03;
    }
    sum
}

/// A near star: where it is, how bright, how fast it twinkles and from where in its cycle, and its tint.
struct Star {
    x: usize,
    y: usize,
    bright: f32,
    speed: f32,
    phase: f32,
    tint: [f32; 3],
}

/// The sky at one size: what never changes, and the near stars.
struct Field {
    w: usize,
    h: usize,
    base: Vec<u8>,
    stars: Vec<Star>,
}

impl Field {
    fn new(w: usize, h: usize) -> Self {
        let mut base = vec![0u8; w * h * 4]; // alloc: the sky, made once a size
        let scale = 3.2 / h.max(1) as f32;
        // The band's line across the sky: from low at the left to high at the right.
        let band = |x: f32, y: f32| {
            let d = (y - (0.72 - 0.42 * x)) * 2.6;
            (-d * d).exp()
        };
        for y in 0..h {
            for x in 0..w {
                let (u, v) = (x as f32 / w as f32, y as f32 / h as f32);
                let (px, py) = (x as f32 * scale, y as f32 * scale);
                let along = band(u, v);
                // Clouds, thicker along the band, with dark lanes cut through them.
                let cloud = clouds(px, py, 11);
                let lanes = clouds(px * 2.1 + 9.0, py * 2.1, 37);
                let dense = ((cloud - 0.42) * 2.4).clamp(0.0, 1.0) * (0.25 + 0.75 * along) * (1.0 - 0.7 * ((lanes - 0.5) * 3.0).clamp(0.0, 1.0));
                // Its colour drifts between indigo, teal and rose.
                let hue = clouds(px * 0.6 + 40.0, py * 0.6 + 7.0, 71);
                let (indigo, teal, rose) = ([46.0, 38.0, 110.0], [22.0, 84.0, 104.0], [120.0, 48.0, 96.0]);
                let mix = |a: [f32; 3], b: [f32; 3], k: f32| [a[0] + (b[0] - a[0]) * k, a[1] + (b[1] - a[1]) * k, a[2] + (b[2] - a[2]) * k];
                let tint = if hue < 0.5 { mix(teal, indigo, (hue * 2.0).clamp(0.0, 1.0)) } else { mix(indigo, rose, ((hue - 0.5) * 2.4).clamp(0.0, 1.0)) };
                // Back to the ground toward every edge: no frame.
                let edge = (u.min(1.0 - u) * 6.0).min(v.min(1.0 - v) * 5.0).clamp(0.0, 1.0);
                let edge = edge * edge * (3.0 - 2.0 * edge);
                let glow = 0.72 * dense * edge;
                let mut c = mix(GROUND, tint, glow);
                // The far stars: a pixel each, more along the band, most of them faint.
                let r = hash(x as u32, y as u32, 5);
                if r > 0.9972 - 0.004 * along {
                    let b = hash(x as u32, y as u32, 6).powi(3) * 150.0 * edge;
                    c = [c[0] + b, c[1] + b, c[2] + b * 1.08];
                }
                let at = (y * w + x) * 4;
                base[at..at + 4].copy_from_slice(&[c[0].min(255.0) as u8, c[1].min(255.0) as u8, c[2].min(255.0) as u8, 255]);
            }
        }
        // The near stars: one about every 3000 texels, kept off the edges.
        let n = (w * h / 3000).clamp(40, 500);
        let stars = (0..n as u32)
            .map(|i| {
                let (x, y) = ((hash(i, 1, 99) * w as f32) as usize, (hash(i, 2, 99) * h as f32) as usize);
                let warm = hash(i, 5, 99);
                let tint = if warm > 0.8 { [1.0, 0.86, 0.7] } else if warm < 0.25 { [0.75, 0.86, 1.0] } else { [1.0, 1.0, 1.0] };
                Star { x: x.clamp(3, w.saturating_sub(4)), y: y.clamp(3, h.saturating_sub(4)), bright: 60.0 + 195.0 * hash(i, 3, 99).powi(3), speed: 0.6 + 2.4 * hash(i, 4, 99), phase: hash(i, 6, 99) * 6.283, tint }
            })
            .collect();
        Self { w, h, base, stars }
    }

    /// The sky at `time` seconds.
    fn at(&self, time: f32) -> Vec<u8> {
        let mut px = self.base.clone(); // alloc: one picture a step of the sky's clock
        let (w, h) = (self.w, self.h);
        let mut add = |x: i64, y: i64, c: [f32; 3], level: f32| {
            if x < 0 || y < 0 || x >= w as i64 || y >= h as i64 {
                return;
            }
            let at = (y as usize * w + x as usize) * 4;
            for k in 0..3 {
                px[at + k] = (f32::from(px[at + k]) + c[k] * level).min(255.0) as u8;
            }
        };
        for s in &self.stars {
            let twinkle = 0.62 + 0.38 * (time * s.speed + s.phase).sin();
            let b = s.bright * twinkle;
            let (x, y) = (s.x as i64, s.y as i64);
            add(x, y, s.tint, b);
            // The bright ones: a small cross of light.
            if s.bright > 150.0 {
                for (dx, dy) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
                    add(x + dx, y + dy, s.tint, b * 0.32);
                }
                if s.bright > 215.0 {
                    for (dx, dy) in [(2, 0), (-2, 0), (0, 2), (0, -2)] {
                        add(x + dx, y + dy, s.tint, b * 0.12);
                    }
                }
            }
        }
        // A shooting star: one every eighteen seconds or so, crossing for a little under a second.
        let (n, t) = ((time / 18.0).floor(), (time % 18.0) / 0.85);
        if t < 1.0 && hash(n as u32, 0, 123) > 0.35 {
            let (x0, y0) = (hash(n as u32, 1, 123) * w as f32 * 0.8 + w as f32 * 0.1, hash(n as u32, 2, 123) * h as f32 * 0.45);
            let (dx, dy) = (if hash(n as u32, 3, 123) > 0.5 { 1.0 } else { -1.0 }, 0.42);
            let reach = 0.22 * w as f32;
            let fade = (t * 6.0).min(1.0) * (1.0 - t);
            for k in 0..46 {
                let back = k as f32;
                let (x, y) = (x0 + dx * (reach * t - back * 1.6), y0 + dy * (reach * t - back * 1.6));
                add(x as i64, y as i64, [1.0, 1.0, 1.0], 230.0 * fade * (1.0 - back / 46.0).powi(2));
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
    /// Paint the sky over `rect` at frame time `time`, a texel a point (at most [`MOST`] a side), smoothed up to the
    /// display's pixels. Nothing in the light appearance.
    pub fn paint(&mut self, d: &mut Draw, rect: [f32; 4], alpha: f32, dark: bool, time: f32) {
        if !dark || rect[2] < 8.0 || rect[3] < 8.0 {
            return;
        }
        let (w, h) = ((rect[2].round() as usize).min(MOST), (rect[3].round() as usize).min(MOST));
        if self.field.as_ref().is_none_or(|f| f.w != w || f.h != h) {
            self.field = Some(Field::new(w, h));
            self.last = None;
        }
        let Some(field) = &self.field else { return };
        let step = (time.max(0.0) * FPS) as u32;
        let texels = match &self.last {
            Some((s, px)) if *s == step => px.clone(),
            _ => {
                let px: Rc<[u8]> = field.at(step as f32 / FPS).into();
                self.last = Some((step, px.clone()));
                px
            }
        };
        self.live = true;
        d.image(ImageInst { rect, size: [w as f32, h as f32, alpha, 1.0], clip: [0.0; 4], clipx: [0.0; 4], light: [0.0; 4], hdr: [0.0; 4] }, texels);
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

    /// The sky's edges are the ground (no frame), its middle is not flat, the stars twinkle from step to step, and it is
    /// the same sky every time.
    #[test]
    fn the_sky_fades_to_the_ground_and_twinkles() {
        let f = Field::new(320, 200);
        let px = |p: &[u8], x: usize, y: usize| [p[(y * 320 + x) * 4], p[(y * 320 + x) * 4 + 1], p[(y * 320 + x) * 4 + 2]];
        for (x, y) in [(0, 0), (319, 0), (0, 199), (160, 0)] {
            assert_eq!(px(&f.base, x, y), [10, 11, 13], "the edge at {x},{y} is not the ground");
        }
        let lit = f.base.chunks_exact(4).filter(|p| p[0] > 14 || p[1] > 15 || p[2] > 17).count();
        assert!(lit > 320 * 200 / 20, "{lit} texels of the sky show anything");
        assert!(f.at(0.0) != f.at(1.3), "nothing twinkles");
        assert_eq!(Field::new(320, 200).at(2.0), f.at(2.0), "the same sky each time");
    }
}
