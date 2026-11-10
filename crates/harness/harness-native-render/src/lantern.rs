//! A `Lantern` (harness-native-ui-text-api, prims.flecs `ui.Lamp`): a paper lantern hanging in the middle of a pane
//! with nothing in it yet, lit, its light flickering a little and falling on the ground under it. Pixel art, as the
//! step items are (block.rs): one small picture on the plain ground, a few flat shades, its glow dithered. Drawn
//! here, never read from a file.
//!
//! The candle's level changes a few times a second in three steps (steady mostly, a dip now and then); with it the
//! paper's brightness, the halo's reach and the pool of light under it. The host draws it at its calm rate while it is
//! in view. In the light appearance it draws nothing: a lit lantern on paper is no light at all.
use crate::draw::Draw;
use crate::pack::ImageInst;
use std::rc::Rc;

/// The picture's size in texels, and points a texel.
const W: usize = 72;
const H: usize = 84;
const TEXEL: f32 = 3.0;
/// Steps of the flame a second.
const FPS: f32 = 7.0;
/// Where the lantern's middle is, and its body's half width and half height.
const CX: f32 = 36.0;
const CY: f32 = 36.0;
const RX: f32 = 13.5;
const RY: f32 = 16.5;

fn hash(n: u32) -> f32 {
    let mut h = n.wrapping_mul(0x9E37_79B1) ^ 0x85EB_CA6B;
    h ^= h >> 15;
    h = h.wrapping_mul(0x2C1B_3C6D);
    h ^= h >> 12;
    (h >> 8) as f32 / (1u32 << 24) as f32
}

/// The flame's level at `step`: full mostly, a little low sometimes, a dip now and then.
fn flame(step: u32) -> f32 {
    match hash(step) {
        r if r > 0.93 => 0.72,
        r if r > 0.7 => 0.88,
        _ => 1.0,
    }
}

/// The lantern at flame level `f`: straight RGBA, clear where there is neither lantern nor light.
fn render(f: f32) -> Vec<u8> {
    const BAYER: [[f32; 4]; 4] = [[0.0, 8.0, 2.0, 10.0], [12.0, 4.0, 14.0, 6.0], [3.0, 11.0, 1.0, 9.0], [15.0, 7.0, 13.0, 5.0]];
    let mut px = vec![0u8; W * H * 4]; // alloc: one small picture a step of the flame
    let mut put = |x: usize, y: usize, c: [f32; 3], a: f32| {
        let at = (y * W + x) * 4;
        px[at..at + 4].copy_from_slice(&[c[0].clamp(0.0, 255.0) as u8, c[1].clamp(0.0, 255.0) as u8, c[2].clamp(0.0, 255.0) as u8, (a * 255.0) as u8]);
    };
    let (top, foot) = ((CY - RY) as usize, (CY + RY) as usize);
    for y in 0..H {
        for x in 0..W {
            let (dx, dy) = (x as f32 + 0.5 - CX, y as f32 + 0.5 - CY);
            let dither = (BAYER[y % 4][x % 4] + 0.5) / 16.0;
            let body = (dx / RX).powi(2) + (dy / RY).powi(2);
            // The cord it hangs by, its caps, its tassel.
            let cord = (x == 35 || x == 36) && y < top.saturating_sub(3);
            let cap = dx.abs() < 6.5 && ((y + 3 >= top && y < top) || (y > foot && y <= foot + 3));
            let tassel = (x == 35 || x == 36) && y > foot + 3 && y <= foot + 11;
            if cord {
                put(x, y, [58.0, 52.0, 48.0], 1.0);
            } else if cap {
                put(x, y, if dx.abs() > 5.0 || y + 3 == top || y == foot + 3 { [34.0, 28.0, 26.0] } else { [58.0, 46.0, 40.0] }, 1.0);
            } else if tassel {
                put(x, y, if y > foot + 8 { [150.0, 44.0, 40.0] } else { [186.0, 60.0, 50.0] }, 1.0);
            } else if body <= 1.0 {
                // The paper: brightest round the flame in its middle, redder toward its edge; a rib every fourth row.
                let glow = ((1.0 - body).sqrt() * f * 4.0 + dither * 0.9).floor().clamp(0.0, 4.0) / 4.0;
                let (dim, lit) = ([176.0, 78.0, 44.0], [255.0, 222.0, 150.0]);
                let rib = if (y + 1) % 4 == 0 { 0.86 } else { 1.0 };
                put(x, y, [0, 1, 2].map(|k| (dim[k] + (lit[k] - dim[k]) * glow) * rib), 1.0);
            } else {
                // Its halo, falling off from the paper, and the pool of light on the ground under it; both dithered.
                let d = ((dx / (RX + 20.0)).powi(2) + (dy / (RY + 20.0)).powi(2)).sqrt();
                let halo = ((1.0 - d).max(0.0).powf(1.6) * f * 4.6 + dither - 0.25).floor().clamp(0.0, 4.0) / 4.0 * 0.3;
                let (gx, gy) = (dx / 27.0, (y as f32 + 0.5 - (H as f32 - 6.0)) / 4.6);
                let pool = ((1.0 - (gx * gx + gy * gy)).max(0.0) * f * 3.4 + dither - 0.3).floor().clamp(0.0, 3.0) / 3.0 * 0.3;
                let a = halo.max(pool);
                if a > 0.0 {
                    put(x, y, [255.0, 176.0, 92.0], a);
                }
            }
        }
    }
    px
}

/// The lantern's picture at each flame level drawn so far.
#[derive(Default)]
pub struct Lanterns {
    made: Vec<(u32, Rc<[u8]>)>,
    /// A lantern was painted this frame: its flame moves.
    live: bool,
}

impl Lanterns {
    /// Paint the lantern in the middle of `rect`, a little above its centre, at frame time `time`: [`TEXEL`] points a
    /// texel on whole device pixels (`sc` a point). Nothing in the light appearance, or in a pane too small for it.
    pub fn paint(&mut self, d: &mut Draw, rect: [f32; 4], alpha: f32, dark: bool, sc: f32, time: f32) {
        let cell = (TEXEL * sc).round().max(1.0) / sc;
        let (w, h) = (W as f32 * cell, H as f32 * cell);
        if !dark || rect[2] < w || rect[3] < h * 1.2 {
            return;
        }
        let f = flame((time.max(0.0) * FPS) as u32);
        let key = (f * 100.0) as u32;
        let texels = match self.made.iter().find(|(k, _)| *k == key) {
            Some((_, p)) => p.clone(),
            None => {
                let p: Rc<[u8]> = render(f).into();
                self.made.push((key, p.clone())); // alloc: three pictures, one a flame level
                p
            }
        };
        self.live = true;
        let snap = |v: f32| (v * sc).round() / sc;
        let at = [snap(rect[0] + (rect[2] - w) * 0.5), snap(rect[1] + (rect[3] - h) * 0.42), w, h];
        d.image(ImageInst { rect: at, size: [W as f32, H as f32, alpha, 0.0], clip: [0.0; 4], clipx: [0.0; 4], light: [0.0; 4], hdr: [0.0; 4] }, texels);
    }

    /// After a frame: whether a lantern was painted in it (the host keeps drawing, at its calm rate).
    pub fn end_frame(&mut self) -> bool {
        std::mem::take(&mut self.live)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The lantern's corners are clear (it sits on the plain ground), its paper is opaque and warm, its light reaches
    /// past the paper, and a dip of the flame dims it.
    #[test]
    fn the_lantern_is_lit_and_flickers() {
        let (full, low) = (render(1.0), render(0.72));
        let at = |p: &[u8], x: usize, y: usize| [p[(y * W + x) * 4], p[(y * W + x) * 4 + 1], p[(y * W + x) * 4 + 2], p[(y * W + x) * 4 + 3]];
        assert_eq!(at(&full, 0, 0)[3], 0, "the corner is not clear");
        let mid = at(&full, CX as usize, CY as usize);
        assert!(mid[3] == 255 && mid[0] > 230 && mid[0] > mid[2], "the paper's middle is {mid:?}");
        let halo = full.chunks_exact(4).filter(|p| p[3] > 0 && p[3] < 255).count();
        assert!(halo > 400, "{halo} texels of light round it");
        assert!(full != low, "a dip changes nothing");
        assert!([0, 1, 2, 50].map(flame).iter().all(|f| (0.7..=1.0).contains(f)));
    }
}
