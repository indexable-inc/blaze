//! The blaze wordmark: "blaze" in pixel letters, set on fire.
//!
//! Copied from indexable-inc/anim's `src/blaze.rs` (itself a port of `docs/brand/fire/gen.py`, the README header), with
//! its frames drawn into plain RGBA instead of a tiny-skia pixmap. Same
//! letters, palettes and automaton, but run live instead of as a baked loop:
//! the letters drop in one by one, then the fire catches from the `b` to
//! the `e` and burns on.
//!
//! Everything sits on a grid of square cells. Each step a cell takes the heat
//! of a cell below it (shifted a cell left or right now and then) minus a
//! little; the cells just above the letters' top edges are the heat source.

const GLYPHS: [&[&str]; 5] = [
    // b
    &[
        "XX.....", "XX.....", "XX.....", "XX.....", "XXXXXX.", "XXXXXXX", "XX...XX", "XX...XX",
        "XX...XX", "XX...XX", "XXXXXXX", "XXXXXX.",
    ],
    // l
    &["XX", "XX", "XX", "XX", "XX", "XX", "XX", "XX", "XX", "XX", "XX", "XX"],
    // a
    &[
        ".......", ".......", ".......", ".......", ".XXXXX.", "XXXXXXX", ".....XX", ".XXXXXX",
        "XXXXXXX", "XX...XX", "XXXXXXX", ".XXXXXX",
    ],
    // z
    &[
        ".......", ".......", ".......", ".......", "XXXXXXX", "XXXXXXX", "....XXX", "...XXX.",
        "..XXX..", ".XXX...", "XXXXXXX", "XXXXXXX",
    ],
    // e
    &[
        ".......", ".......", ".......", ".......", ".XXXXX.", "XXXXXXX", "XX...XX", "XXXXXXX",
        "XXXXXXX", "XX.....", "XXXXXXX", ".XXXXX.",
    ],
];
const LETTER_GAP: usize = 2;
/// Room for the flames above the letters, beside them, and the shadow's row under them.
const ABOVE: usize = 13;
const SIDE: usize = 2;
const BELOW: usize = 1;
/// Rows at the very top the flames never reach (they are cooled hard there).
const CROP: usize = 3;

/// Automaton steps per second, as in the README header.
pub const FPS: f32 = 16.0;
pub const HEAT: f32 = 12.0;
pub const COOL_MIN: f32 = 0.4;
pub const COOL_MAX: f32 = 2.3;
/// Heat level (1..=HEAT) to palette index.
pub const LEVELS: [usize; 12] = [0, 1, 1, 2, 2, 2, 3, 3, 3, 4, 4, 4];
/// Seconds for the fire to catch across the whole word.
pub const IGNITE: f32 = 0.75;

// Timeline, seconds from when Blaze starts.
/// When each letter starts to fall, and how long the fall takes.
pub const DROP_START: [f32; 5] = [0.15, 0.31, 0.47, 0.63, 0.79];
pub const DROP_DUR: f32 = 0.36;
/// The fire catches a beat after the last letter lands.
pub const IGNITE_AT: f32 = DROP_START[4] + DROP_DUR + 0.2;
/// Each landing kicks the word, decaying over this long.
const HOP: f32 = 0.22;

// The exit, seconds from when it starts. The text under the logo burns
// first, then the burn front climbs into the letters: each cell glows white
// hot, turns to flame, chars and crumbles into embers that drift up.
/// When the letters' bottom row catches, how long the front takes to reach
/// their top row, and how ragged it is.
const BURN_START: f32 = 0.5;
const BURN_SPAN: f32 = 0.5;
const BURN_JITTER: f32 = 0.2;
/// How long a letter cell burns before it crumbles.
const CHAR: f32 = 0.4;
/// The last ember is out by then.
pub const EXIT: f32 = 3.0;

pub type Rgb = [u8; 3];

/// Letters, their shadow (a cell down and right) and five fire colours,
/// coolest first. From gen.py; on white the hottest colour is a step deeper.
struct Theme {
    ink: Rgb,
    shade: Rgb,
    fire: [Rgb; 5],
}

const ON_DARK: Theme = Theme {
    ink: [0xf0, 0xf6, 0xfc],
    shade: [0x6e, 0x2a, 0x14],
    fire: [
        [0xa8, 0x24, 0x0f],
        [0xe0, 0x46, 0x1a],
        [0xff, 0x8f, 0x1f],
        [0xff, 0xd4, 0x3b],
        [0xff, 0xf6, 0xc2],
    ],
};
const ON_LIGHT: Theme = Theme {
    ink: [0x1f, 0x23, 0x28],
    shade: [0xe8, 0xc3, 0xa0],
    fire: [
        [0x9a, 0x1f, 0x0c],
        [0xd6, 0x3c, 0x12],
        [0xf5, 0x7a, 0x10],
        [0xff, 0xae, 0x1a],
        [0xff, 0xd8, 0x4a],
    ],
};

#[derive(Clone, Copy, PartialEq)]
enum Cell {
    Air,
    /// Part of the `n`th letter, or of its shadow.
    Letter(u8),
    Shadow(u8),
}

/// One drawn frame: `w` by `h` pixels of straight RGBA8, row by row from the top, and its light: the same pixels'
/// colour with, in alpha, how hot each is (0: not lit; 255: the hottest the fire gets), for a renderer with headroom
/// over white to lift by (`harness-native-render`'s light layer).
pub struct Image {
    pub w: u32,
    pub h: u32,
    pub rgba: Vec<u8>,
    pub light: Vec<u8>,
}

/// How lit each fire colour is, coolest first: the deep reds stay at their colour, orange starts to shine, the
/// yellow-white core is the hottest.
const LIT: [u8; 5] = [0, 20, 90, 170, 255];

pub struct Blaze {
    w: usize,
    h: usize,
    cells: Vec<Cell>,
    /// Each burning column's source cell row, if the column has letters.
    source: Vec<Option<usize>>,
    heat: Vec<f32>,
    steps: u64,
    theme: &'static Theme,
    /// When each letter cell catches, in seconds after the exit starts.
    burn_at: Vec<f32>,
    /// When the exit started, in seconds after Blaze started.
    exit: Option<f32>,
    /// The mouse pointer, in cells of the uncropped grid, if it has been seen.
    pointer: Option<(f32, f32)>,
    /// Wind from the pointer sweeping sideways, positive to the right.
    gust: f32,
}

/// How far the pointer's breath reaches, and the gust from sweeping it, in cells.
const BREATH: f32 = 5.0;
const GUST_REACH: f32 = 10.0;

/// The fire colours for text on a dark or light background, coolest first,
/// and the ash they leave.
pub fn palette(dark_bg: bool) -> ([Rgb; 5], Rgb) {
    let theme = if dark_bg { &ON_DARK } else { &ON_LIGHT };
    (theme.fire, theme.shade)
}

impl Blaze {
    /// `dark_bg`: the terminal background is dark.
    pub fn new(dark_bg: bool) -> Self {
        let mut letters = Vec::new();
        let mut x0 = 0;
        for (i, rows) in GLYPHS.iter().enumerate() {
            for (y, row) in rows.iter().enumerate() {
                letters.extend(
                    row.bytes()
                        .enumerate()
                        .filter(|&(_, b)| b == b'X')
                        .map(|(dx, _)| (x0 + dx, y, i as u8)),
                );
            }
            x0 += rows[0].len() + if i + 1 < GLYPHS.len() { LETTER_GAP } else { 0 };
        }
        let (w, h) = (x0 + 2 * SIDE + 1, ABOVE + GLYPHS[0].len() + BELOW);
        let mut cells = vec![Cell::Air; w * h];
        let mut source = vec![None; w];
        for &(x, y, n) in &letters {
            let (x, y) = (x + SIDE, y + ABOVE);
            cells[y * w + x] = Cell::Letter(n);
            source[x] = Some(source[x].map_or(y - 1, |s: usize| s.min(y - 1)));
        }
        for &(x, y, n) in &letters {
            let i = (y + ABOVE + 1) * w + x + SIDE + 1;
            if cells[i] == Cell::Air {
                cells[i] = Cell::Shadow(n);
            }
        }
        let rows = GLYPHS[0].len();
        let burn_at = (0..w * h)
            .map(|i| {
                let (x, y) = (i % w, i / w);
                if !matches!(cells[i], Cell::Letter(_)) {
                    return f32::INFINITY;
                }
                let up = (ABOVE + rows - 1 - y) as f32 / (rows - 1) as f32;
                BURN_START + BURN_SPAN * up + BURN_JITTER * noise(3, x, y as u64, 2.5, 2.0)
            })
            .collect();
        Self {
            w,
            h,
            cells,
            source,
            heat: vec![0.0; w * h],
            steps: 0,
            theme: if dark_bg { &ON_DARK } else { &ON_LIGHT },
            burn_at,
            exit: None,
            pointer: None,
            gust: 0.0,
        }
    }

    /// The mouse pointer moved to (`x`, `y`), in cells of a rendered frame
    /// without lead. The flames lean away from it and part around it, and
    /// sweeping it drags them along.
    #[allow(dead_code)]
    pub fn point(&mut self, x: f32, y: f32) {
        let y = y + CROP as f32;
        if let Some((px, _)) = self.pointer {
            self.gust = (self.gust + 0.15 * (x - px)).clamp(-1.5, 1.5);
        }
        self.pointer = Some((x, y));
    }

    /// Sideways wind at cell (x, y), from -1 (left) to 1 (right), and extra
    /// cooling where the pointer parts the flames.
    fn wind(&self, x: usize, y: usize) -> (f32, f32) {
        let Some((px, py)) = self.pointer else { return (0.0, 0.0) };
        let (dx, dy) = (x as f32 + 0.5 - px, y as f32 + 0.5 - py);
        let d2 = dx * dx + dy * dy;
        let breath = 2.3 * dx / BREATH * (-d2 / (BREATH * BREATH)).exp();
        let gust = self.gust * (-d2 / (GUST_REACH * GUST_REACH)).exp();
        ((breath + gust).clamp(-1.0, 1.0), 3.0 * (-d2 / 4.0).exp())
    }

    /// Starts the exit at `at` seconds after Blaze started.
    pub fn burn(&mut self, at: f32) {
        self.exit = Some(at);
    }

    pub fn burning(&self) -> bool {
        self.exit.is_some()
    }

    /// Seconds since cell `i` caught at blaze time `t`, negative before.
    fn burnt(&self, i: usize, t: f32) -> f32 {
        self.exit.map_or(f32::NEG_INFINITY, |at| t - at - self.burn_at[i])
    }

    /// Whether the letters are still falling or shaking at `t`.
    pub fn settling(t: f32) -> bool {
        t < DROP_START[4] + DROP_DUR + HOP
    }

    /// Size of a rendered frame in cells, without lead.
    pub fn cells(&self) -> (usize, usize) {
        (self.w, self.h - CROP)
    }

    /// Puts the fire out, ready to catch again.
    fn reset(&mut self) {
        self.heat.fill(0.0);
        self.steps = 0;
    }

    /// Runs the automaton up to `t` seconds after ignition.
    fn advance(&mut self, t: f32) {
        let target = (t.max(0.0) * FPS) as u64;
        if target < self.steps {
            self.reset();
        }
        // After a long stall, skip ahead rather than replay every step.
        if target - self.steps > 64 {
            self.steps = target - 64;
        }
        while self.steps < target {
            self.step();
        }
    }

    fn step(&mut self) {
        let (w, h, t) = (self.w, self.h, self.steps);
        let since = t as f32 / FPS;
        // On the way out the fire roars up as it eats the letters, then
        // starves once they are gone.
        let exit = self.exit.map(|at| since + IGNITE_AT - at);
        let flare = exit.map_or(0.0, |e| (std::f32::consts::PI * (e / 1.6).clamp(0.0, 1.0)).sin());
        let fuel = exit.map_or(1.0, |e| 1.0 - ((e - 0.8) / 0.5).clamp(0.0, 1.0));
        let mut next = vec![0.0; w * h];
        for y in 0..h - 1 {
            for x in 0..w {
                if matches!(self.cells[y * w + x], Cell::Letter(_)) {
                    let u = self.burnt(y * w + x, since + IGNITE_AT);
                    if u < 0.0 {
                        continue;
                    }
                    // A burning cell feeds the flames above it; a burnt one is air.
                    if u < CHAR {
                        next[y * w + x] =
                            (HEAT * (0.45 + 0.5 * noise(4, x, y as u64 + t, 1.5, 2.0))).round();
                        continue;
                    }
                }
                if self.source[x] == Some(y) {
                    // The fire catches left to right, each column fading up.
                    let lit = (since / IGNITE * w as f32 - x as f32) / 4.0;
                    next[y * w + x] = (source_heat(x, t) * lit.clamp(0.0, 1.0) * fuel).round();
                    continue;
                }
                // A parcel of heat rises a cell a step, so y + t is constant
                // along its path: noise keyed by it makes tongues, not sparks.
                let k = y as u64 + t;
                // Wind makes the flames lean: heat drifts with it more often
                // and against it less.
                let (wind, parted) = self.wind(x, y);
                let right = 0.16 + 0.7 * wind.max(0.0) - 0.16 * (-wind).max(0.0);
                let left = 0.16 + 0.7 * (-wind).max(0.0) - 0.16 * wind.max(0.0);
                let sway = noise(1, x, k, 5.0, 4.0);
                let dx = if sway < left {
                    -1
                } else if sway > 1.0 - right {
                    1
                } else {
                    0
                };
                let sx = (x as isize - dx).clamp(0, w as isize - 1) as usize;
                let mut cool = (COOL_MIN
                    + (COOL_MAX - COOL_MIN) * noise(2, x, k, 2.2, 4.0).powf(1.5))
                    * (1.0 - 0.25 * flare)
                    + parted;
                // Near the top edge, so no flame is cut off flat.
                cool += 2.5 * 4usize.saturating_sub(y) as f32;
                next[y * w + x] = (self.heat[(y + 1) * w + sx] - cool).max(0.0);
            }
        }
        self.heat = next;
        self.steps += 1;
        self.gust *= 0.85;
    }

    /// The frame `t` seconds after Blaze starts, with each cell `s` pixels
    /// square, and `lead` extra rows of cells above it for the letters to
    /// fall through. Fractional sizes stay crisp: cell edges land on whole
    /// pixels.
    pub fn render(&mut self, t: f32, s: f32, lead: u32) -> Image {
        self.advance(t - IGNITE_AT);
        let (cw, ch) = self.cells();
        let lead = lead as f32;
        let (pw, ph) = (
            (cw as f32 * s).round().max(1.0) as usize,
            ((ch as f32 + lead) * s).round().max(1.0) as usize,
        );
        let mut px = vec![[0u8; 4]; pw * ph];
        let mut lit = vec![[0u8; 4]; pw * ph];
        let theme = self.theme;
        // How lit a colour is: a fire colour by its place in the palette, anything else (ink, ash) not at all.
        let glow = |rgb: Rgb| theme.fire.iter().position(|f| *f == rgb).map_or(0, |k| LIT[k]);
        // Fills the box from (x0, y0) to (x1, y1), in cells, snapped to whole pixels.
        let mut rect = |x0: f32, y0: f32, x1: f32, y1: f32, rgb: Rgb| {
            let c = [rgb[0], rgb[1], rgb[2], 255];
            let l = [rgb[0], rgb[1], rgb[2], glow(rgb)];
            let snap = |v: f32, max: usize| ((v * s).round().max(0.0) as usize).min(max);
            let (x0, x1) = (snap(x0, pw), snap(x1, pw));
            for py in snap(y0 + lead, ph)..snap(y1 + lead, ph) {
                px[py * pw + x0..py * pw + x1].fill(c);
                lit[py * pw + x0..py * pw + x1].fill(l);
            }
        };
        // Fire first, then shadows, then letters, so falling letters cover the
        // rest, then the embers of burnt letters over everything.
        let offsets: Vec<Option<f32>> =
            (0..GLYPHS.len()).map(|n| drop_offset(n, t, lead + (ABOVE - CROP) as f32)).collect();
        for pass in 0..3 {
            for y in 0..ch {
                for x in 0..cw {
                    let i = (y + CROP) * self.w + x;
                    let u = self.burnt(i, t);
                    let flame = || {
                        (self.heat[i] >= 1.0).then(|| {
                            theme.fire[LEVELS[(self.heat[i] as usize).min(LEVELS.len()) - 1]]
                        })
                    };
                    let (rgb, dy) = match (pass, self.cells[i]) {
                        (0, Cell::Air | Cell::Shadow(_)) => (flame(), Some(0.0)),
                        (0, Cell::Letter(_)) if u >= CHAR => (flame(), Some(0.0)),
                        // A shadow goes when the letter casting it catches.
                        (1, Cell::Shadow(n)) if self.burnt(i - self.w - 1, t) < 0.0 => {
                            (Some(theme.shade), offsets[n as usize])
                        }
                        (2, Cell::Letter(n)) => (self.letter(u), offsets[n as usize]),
                        _ => continue,
                    };
                    if let (Some(rgb), Some(dy)) = (rgb, dy) {
                        let y = y as f32 + dy;
                        rect(x as f32, y, (x + 1) as f32, y + 1.0, rgb);
                    }
                }
            }
        }
        if let Some(at) = self.exit {
            for (x, y, rgb, size) in self.embers(t - at) {
                let y = y - CROP as f32;
                if y + lead >= 0.0 {
                    rect(x - size / 2.0, y - size / 2.0, x + size / 2.0, y + size / 2.0, rgb);
                }
            }
        }
        Image {
            w: pw as u32,
            h: ph as u32,
            // In place, no copy: a byte-at-a-time flatten was the whole cost of an 8K video's frame.
            rgba: px.into_flattened(),
            light: lit.into_flattened(),
        }
    }

    /// A letter cell's colour `u` seconds after it caught: ink, white hot
    /// just before, then down the fire's colours to char, then gone.
    fn letter(&self, u: f32) -> Option<Rgb> {
        let fire = &self.theme.fire;
        match u {
            u if u < -0.12 => Some(self.theme.ink),
            u if u < 0.0 => Some(fire[4]),
            u if u < 0.6 * CHAR => Some(fire[3 - (u / (0.6 * CHAR) * 4.0) as usize]),
            u if u < CHAR => Some(self.theme.shade),
            _ => None,
        }
    }

    /// The embers in the air `e` seconds into the exit: centre and size in
    /// cells (rows counted from the top of the uncropped grid) and colour.
    /// Some letter cells throw one as they crumble. It floats up faster and
    /// faster, swaying, cools down the fire's colours, shrinks and flickers out.
    fn embers(&self, e: f32) -> Vec<(f32, f32, Rgb, f32)> {
        use std::f32::consts::TAU;
        let mut out = Vec::new();
        for (i, &at) in self.burn_at.iter().enumerate() {
            let r = |seed: u64| hash(seed, i as u64, 0);
            if !at.is_finite() || r(4) > 0.28 {
                continue;
            }
            let age = e - at - 0.7 * CHAR;
            let life = 0.7 + 0.7 * r(5);
            if !(0.0..life).contains(&age) {
                continue;
            }
            let f = age / life;
            if f > 0.5 && (TAU * (7.0 + 4.0 * r(9)) * age + TAU * r(10)).sin() < -0.3 {
                continue;
            }
            let (x0, y0) = ((i % self.w) as f32 + 0.5, (i / self.w) as f32 + 0.5);
            let x = x0
                + 2.0 * (r(6) - 0.5) * age
                + 0.6 * (TAU * (0.6 + 0.8 * r(7)) * age + TAU * r(8)).sin();
            let y = y0 - (2.0 + 4.0 * r(11)) * age - 3.0 * age * age;
            out.push((x, y, self.theme.fire[3 - (f * 4.0).min(3.0) as usize], 1.0 - 0.5 * f));
        }
        out
    }
}

/// How far letter `n` sits above its place at `t`, in cells, or `None`
/// before it starts to fall. It falls `height` cells, accelerating, then
/// bounces a little.
fn drop_offset(n: usize, t: f32, height: f32) -> Option<f32> {
    let start = DROP_START[n];
    if t < start {
        return None;
    }
    let p = ((t - start) / DROP_DUR).min(1.0);
    let mut dy = -height * (1.0 - p * p);
    let since = t - start - DROP_DUR;
    if (0.0..HOP).contains(&since) {
        let q = since / HOP;
        dy -= 0.8 * (std::f32::consts::PI * q).sin() * (1.0 - q);
    }
    Some(dy)
}

/// How much the word shakes at `t`, in cells, from the letters landing.
pub fn knock(t: f32) -> (f32, f32) {
    use std::f32::consts::TAU;
    let (mut kx, mut ky) = (0.0, 0.0);
    for (i, &start) in DROP_START.iter().enumerate() {
        let since = t - start - DROP_DUR;
        if (0.0..0.2).contains(&since) {
            let amp =
                if i == DROP_START.len() - 1 { 0.6 } else { 0.35 } * (1.0 - since / 0.2).powi(2);
            kx += amp * 0.4 * (TAU * 31.0 * since).sin();
            ky += amp * (TAU * 24.0 * since + 0.7).sin();
        }
    }
    (kx, ky)
}

/// How hot the base of the flame over column `x` burns at step `t`: a few
/// standing waves, so some columns feed tall tongues and the gaps breathe.
fn source_heat(x: usize, t: u64) -> f32 {
    use std::f32::consts::TAU;
    let p = TAU * (t % 32) as f32 / 32.0;
    let x = x as f32;
    let w = 0.55 * (TAU * x / 6.0 + 0.7).sin() * p.cos()
        + 0.35 * (TAU * x / 3.7 + 1.9).sin() * (2.0 * p + 0.5).cos()
        + 0.25 * (TAU * x / 9.0 + 0.3).sin() * (3.0 * p).sin();
    (HEAT * (0.72 + 0.4 * w)).round().clamp(0.0, HEAT)
}

/// Smooth value noise over (x, k), in [0, 1].
pub fn noise(seed: u64, x: usize, k: u64, sx: f32, sk: f32) -> f32 {
    let (fx, fk) = (x as f32 / sx, k as f32 / sk);
    let (x0, k0) = (fx.floor(), fk.floor());
    let smooth = |v: f32| v * v * (3.0 - 2.0 * v);
    let (tx, tk) = (smooth(fx - x0), smooth(fk - k0));
    let at = |i: f32, j: f32| hash(seed, i as u64, j as u64);
    let top = at(x0, k0) * (1.0 - tx) + at(x0 + 1.0, k0) * tx;
    let bottom = at(x0, k0 + 1.0) * (1.0 - tx) + at(x0 + 1.0, k0 + 1.0) * tx;
    top * (1.0 - tk) + bottom * tk
}

/// splitmix64 of the key, as a float in [0, 1].
pub fn hash(seed: u64, i: u64, j: u64) -> f32 {
    let mut z = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15)
        ^ i.wrapping_mul(0xbf58_476d_1ce4_e5b9)
        ^ j.wrapping_mul(0x94d0_49bb_1331_11eb);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^= z >> 31;
    (z >> 40) as f32 / (1u64 << 24) as f32
}
