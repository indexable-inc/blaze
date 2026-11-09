//! A `PixelBlock` (harness-native-ui-text-api, prims.flecs `ui.Block`): a small pixel-art item, a game's dropped item,
//! that floats and does what it stands for. Each item is drawn here, never read from a file: a few sprites of a
//! handful of colours, moved or swapped on a clock of [`FPS`] steps a second.
//!
//! - `exec` (and `cell`): a terminal, its cursor blinking after the prompt.
//! - `read` (and `skill`): an open book, a page turning.
//! - `edit` (and `write`, `delete`): a pencil writing a line.
//! - `search` (and `list`): a lens going over a page.
//! - `net`: the world, turning.
//! - `build` (and `tool`): a hammer striking an anvil, a spark at each blow.
//! - `spawn`: a rocket, its flame flickering.
//! - `wait`: an hourglass, its sand falling.
//! - `sql`: a drum of rows, a light running down it.
//! - `think`: a crystal, a glint crossing it.
//!
//! The item bobs a texel over its shadow, as a dropped item does. When the node's `face` changes the old item sinks
//! away and the new one rises in over `turn` seconds. It moves only while it is painted; the host draws it at its calm
//! rate.
use crate::draw::Draw;
use crate::pack::ImageInst;
use harness_native_ui_text_api::PixelBlock;
use std::collections::HashMap;
use std::rc::Rc;

/// The picture's side in texels.
const CANVAS: usize = 16;
/// Steps of an item's own motion a second.
const FPS: f32 = 6.0;
/// Seconds a bob up and down takes.
const BOB: f32 = 1.8;

type Rgb = [u8; 3];
/// A sprite: rows of characters, each a colour of its palette (`.` and any character not in it: clear).
type Sprite = &'static [&'static str];
type Palette = &'static [(u8, Rgb)];

/// The picture being drawn: straight RGBA.
struct Canvas(Vec<u8>);

impl Canvas {
    fn new() -> Self {
        Self(vec![0; CANVAS * CANVAS * 4]) // alloc: one small picture a step of the clock
    }

    fn put(&mut self, x: i32, y: i32, c: Rgb, a: u8) {
        if (0..CANVAS as i32).contains(&x) && (0..CANVAS as i32).contains(&y) {
            let at = (y as usize * CANVAS + x as usize) * 4;
            self.0[at..at + 4].copy_from_slice(&[c[0], c[1], c[2], a]);
        }
    }

    /// `sprite` in `pal`, its corner at (`x`, `y`).
    fn blit(&mut self, sprite: Sprite, pal: Palette, x: i32, y: i32) {
        for (j, row) in sprite.iter().enumerate() {
            for (i, ch) in row.bytes().enumerate() {
                if let Some(&(_, c)) = pal.iter().find(|(k, _)| *k == ch) {
                    self.put(x + i as i32, y + j as i32, c, 255);
                }
            }
        }
    }
}

const TERM_PAL: Palette = &[(b'#', [96, 100, 112]), (b'-', [58, 62, 74]), (b'o', [14, 15, 20]), (b'g', [112, 224, 122]), (b'r', [240, 110, 110]), (b'y', [240, 200, 90])];
const TERM: Sprite = &[
    "############",
    "#ry--------#",
    "############",
    "#oooooooooo#",
    "#ogoooooooo#",
    "#oogooooooo#",
    "#ogoooooooo#",
    "#oooooooooo#",
    "#oooooooooo#",
    "############",
];

const BOOK_PAL: Palette = &[(b'#', [112, 74, 44]), (b'p', [236, 228, 204]), (b'l', [150, 138, 116]), (b's', [196, 186, 160]), (b'r', [200, 70, 60])];
const BOOK: Sprite = &[
    ".pppp..pppp.",
    "pppppppppppp",
    "pllllsslllp.",
    "pppppsspppp.",
    "pllllsslllp.",
    "pppppsspppp.",
    "pllllsslllp.",
    "pppppsspppp.",
    "############",
    ".####rr####.",
];
/// A page standing mid-turn, over the spine.
const BOOK_PAGE: Sprite = &["p", "p", "p", "p", "p", "p", "p", "p"];

const PAPER_PAL: Palette = &[(b'p', [236, 232, 220]), (b's', [200, 196, 184]), (b'l', [130, 134, 146])];
const PAPER: Sprite = &[
    "ppppppppps..",
    "pppppppppps.",
    "ppppppppppp.",
    "ppppppppppp.",
    "ppppppppppp.",
    "ppppppppppp.",
    "ppppppppppp.",
    "ppppppppppp.",
    "ppppppppppp.",
    "ppppppppppp.",
];
const PENCIL_PAL: Palette = &[(b'y', [250, 204, 80]), (b'o', [214, 150, 50]), (b'w', [232, 206, 170]), (b'k', [50, 44, 40]), (b'r', [232, 120, 130])];
const PENCIL: Sprite = &[
    "....rr",
    "...yyr",
    "..yoy.",
    ".yoy..",
    "woy...",
    "kw....",
];
const LENS_PAL: Palette = &[(b'#', [70, 76, 92]), (b'g', [150, 200, 250]), (b'w', [236, 246, 255]), (b'h', [120, 90, 60])];
const LENS: Sprite = &[
    ".###...",
    "#gwg#..",
    "#wgg#..",
    "#ggg#..",
    ".###h..",
    ".....h.",
    "......h",
];

/// The world's land, wrapped round: a row of the globe reads this from where it has turned to.
const LAND: [&str; 10] = [
    "..##......###...........",
    ".####....#####....##....",
    "######...######..####...",
    ".#####....####....###...",
    "..###......##.....##....",
    "...##.....###...........",
    "...###....####......##..",
    "....##.....###.....####.",
    "....#.......#.......##..",
    "........................",
];
/// How far each of the globe's rows reaches from its middle, in texels: a disc ten across.
const DISC: [i32; 10] = [2, 4, 5, 5, 5, 5, 5, 5, 4, 2];

const ANVIL_PAL: Palette = &[(b'#', [86, 90, 100]), (b'l', [132, 136, 148]), (b'd', [54, 56, 64])];
const ANVIL: Sprite = &[
    "llllllllll..",
    "##########l.",
    ".########...",
    "...####.....",
    "...####.....",
    "..######....",
    ".dddddddd...",
];
const HAMMER_PAL: Palette = &[(b'#', [150, 154, 166]), (b'l', [196, 200, 210]), (b'h', [150, 104, 60])];
const HAMMER_UP: Sprite = &[
    ".ll...",
    "l##h..",
    ".##.h.",
    ".....h",
];
const HAMMER_DOWN: Sprite = &[
    "......",
    "lll...",
    "###hhh",
    "###...",
];
const SPARK_PAL: Palette = &[(b'y', [255, 226, 110]), (b'o', [255, 158, 44])];
const SPARK: Sprite = &["y.o.y", ".yoy.", "o.y.o"];

const ROCKET_PAL: Palette = &[(b'w', [236, 238, 244]), (b's', [176, 180, 192]), (b'r', [214, 70, 60]), (b'b', [90, 160, 240]), (b'y', [255, 226, 110]), (b'o', [255, 140, 44])];
const ROCKET: Sprite = &[
    "..rr..",
    ".rwwr.",
    ".wbbw.",
    ".wbbw.",
    ".wwws.",
    "rwwwsr",
    "r.ss.r",
];
const FLAME_A: Sprite = &[".yy.", ".oy.", "..o."];
const FLAME_B: Sprite = &[".yy.", ".yo.", ".o.."];

const GLASS_PAL: Palette = &[(b'#', [130, 96, 56]), (b'g', [200, 224, 240]), (b's', [232, 200, 120])];
const GLASS: Sprite = &[
    "########",
    ".gssssg.",
    ".gssssg.",
    "..gssg..",
    "...gg...",
    "...gg...",
    "..g..g..",
    ".g....g.",
    ".gssssg.",
    "########",
];

const DRUM_PAL: Palette = &[(b'#', [60, 96, 190]), (b'l', [120, 156, 236]), (b'd', [40, 66, 140]), (b'w', [226, 234, 252])];
const DRUM: Sprite = &[
    ".llllll.",
    "llllllll",
    "########",
    "dddddddd",
    "########",
    "dddddddd",
    "########",
    "dddddddd",
    "########",
    ".######.",
];

const GEM_PAL: Palette = &[(b'#', [124, 90, 196]), (b'l', [176, 150, 240]), (b'd', [84, 58, 150]), (b'w', [244, 236, 255])];
const GEM: Sprite = &[
    "...ll...",
    "..l##l..",
    ".ll###d.",
    ".l####d.",
    "ll####dd",
    "l#####dd",
    ".l###dd.",
    ".l###dd.",
    "..##dd..",
    "...dd...",
];

/// The item a face names: what a step does, and `think` for a turn between steps. A name not here is `build` (a tool).
pub fn item(face: &str) -> &'static str {
    match face {
        "exec" | "cell" => "exec",
        "read" | "skill" => "read",
        "edit" | "write" | "delete" => "edit",
        "search" | "list" => "search",
        "net" => "net",
        "spawn" => "spawn",
        "wait" => "wait",
        "sql" => "sql",
        "think" => "think",
        _ => "build",
    }
}

/// Item `name` at step `n` of its own motion, its corner lifted `lift` texels (its bob, or how far it has risen in).
fn draw(c: &mut Canvas, name: &str, n: usize, lift: i32) {
    let y = 3 - lift;
    match name {
        "exec" => {
            c.blit(TERM, TERM_PAL, 2, y);
            // The cursor after the prompt, on for half of each second.
            if n % FPS as usize * 2 < FPS as usize {
                c.put(6, y + 5, [112, 224, 122], 255);
                c.put(7, y + 5, [112, 224, 122], 255);
            }
        }
        "read" => {
            c.blit(BOOK, BOOK_PAL, 2, y + 1);
            // A page lifts from the right, stands over the spine, and lies down on the left.
            match n % 8 {
                4 => c.blit(BOOK_PAGE, BOOK_PAL, 10, y),
                5 => c.blit(BOOK_PAGE, BOOK_PAL, 8, y - 1),
                6 => c.blit(BOOK_PAGE, BOOK_PAL, 5, y),
                _ => {}
            }
        }
        "edit" => {
            c.blit(PAPER, PAPER_PAL, 2, y + 1);
            // The line the pencil has written so far, then the pencil at its end; a new line every pass.
            let k = (n % 7) as i32;
            for row in 0..=((n / 7) % 3) as i32 {
                let done = if row == ((n / 7) % 3) as i32 { k } else { 6 };
                for x in 0..done {
                    c.put(4 + x, y + 4 + row * 2, [130, 134, 146], 255);
                }
            }
            c.blit(PENCIL, PENCIL_PAL, 4 + k, y - 2 + ((n / 7) % 3) as i32 * 2);
        }
        "search" => {
            c.blit(PAPER, PAPER_PAL, 2, y + 1);
            for row in 0..3 {
                for x in 0..7 {
                    c.put(4 + x, y + 3 + row * 2, [130, 134, 146], 255);
                }
            }
            // The lens goes round over the lines.
            let (dx, dy) = [(0, 0), (1, 0), (2, 1), (2, 2), (1, 3), (0, 3), (-1, 2), (-1, 1)][n % 8];
            c.blit(LENS, LENS_PAL, 5 + dx, y + 1 + dy);
        }
        "net" => {
            // The disc, its land read from where the world has turned to; darker toward the limb it turns away from.
            for (j, half) in DISC.iter().enumerate() {
                for i in -half..*half {
                    let u = ((i + 5) as usize + n) % 24;
                    let land = LAND[j].as_bytes()[u] == b'#';
                    let rim = i == -half || i + 1 == *half;
                    let col: Rgb = match (land, rim) {
                        (true, false) => [98, 178, 84],
                        (true, true) => [70, 142, 62],
                        (false, false) => [62, 120, 200],
                        (false, true) => [44, 96, 174],
                    };
                    c.put(8 + i, y + 1 + j as i32, col, 255);
                }
            }
        }
        "spawn" => {
            let up = i32::from(n % 2 == 0);
            c.blit(ROCKET, ROCKET_PAL, 5, y - up);
            c.blit(if n % 2 == 0 { FLAME_A } else { FLAME_B }, ROCKET_PAL, 6, y + 7 - up);
        }
        "wait" => {
            c.blit(GLASS, GLASS_PAL, 4, y + 1);
            // A grain falls through the neck into the lower bulb.
            c.put(7 + (n % 2) as i32, y + 5 + (n % 4) as i32, [232, 200, 120], 255);
        }
        "sql" => {
            c.blit(DRUM, DRUM_PAL, 4, y + 1);
            // A row lights, one after the next, down the drum.
            let row = y + 3 + (n % 4) as i32 * 2;
            for x in 5..11 {
                c.put(x, row, [226, 234, 252], 255);
            }
        }
        "think" => {
            c.blit(GEM, GEM_PAL, 4, y + 1);
            // A glint crosses the crystal's face, then rests.
            let k = (n % 10) as i32;
            if k < 5 {
                c.put(6 + k, y + 3 + k, [244, 236, 255], 255);
                c.put(7 + k, y + 3 + k, [244, 236, 255], 255);
            }
        }
        _ => {
            c.blit(ANVIL, ANVIL_PAL, 2, y + 5);
            // The hammer up for three steps, down for one, a spark at the blow.
            if n % 4 == 3 {
                c.blit(HAMMER_DOWN, HAMMER_PAL, 4, y + 1);
                c.blit(SPARK, SPARK_PAL, 2, y + 2);
            } else {
                c.blit(HAMMER_UP, HAMMER_PAL, 6, y - 1);
            }
        }
    }
}

/// The item's shadow on the ground under it: shorter while the item is up.
fn shadow(c: &mut Canvas, lift: i32) {
    let half = if lift > 0 { 3 } else { 4 };
    for x in 8 - half..8 + half {
        c.put(x, CANVAS as i32 - 1, [0, 0, 0], 90);
    }
}

/// The picture at `time` seconds: item `to`, or `turn` of the way (0 to 1) from item `from` to it (the old one sinking
/// away through the first half, the new one rising in through the second).
fn render(from: &str, to: &str, turn: f32, time: f32) -> Vec<u8> {
    let n = (time.max(0.0) * FPS) as usize;
    let bob = i32::from((time * std::f32::consts::TAU / BOB).sin() > 0.3);
    let mut c = Canvas::new();
    if turn >= 1.0 || from == to {
        shadow(&mut c, bob);
        draw(&mut c, to, n, bob);
    } else if turn < 0.5 {
        // Sinking: down a texel every sixth of the turn, cut off at the ground.
        let down = (turn * 12.0) as i32;
        shadow(&mut c, -1);
        draw(&mut c, from, n, -down);
        clear_below(&mut c, CANVAS as i32 - 1 - down / 2);
    } else {
        let down = ((1.0 - turn) * 12.0) as i32;
        shadow(&mut c, -1);
        draw(&mut c, to, n, -down);
        clear_below(&mut c, CANVAS as i32 - 1 - down / 2);
    }
    c.0
}

/// Clear the rows from `y` down but the shadow's (an item sinking away is cut at the ground).
fn clear_below(c: &mut Canvas, y: i32) {
    for row in y.max(0)..CANVAS as i32 - 1 {
        for x in 0..CANVAS {
            let at = (row as usize * CANVAS + x) * 4;
            c.0[at..at + 4].copy_from_slice(&[0; 4]);
        }
    }
}

/// One block's change of item: what it showed, what it shows, and when the change began (frame clock).
struct Turn {
    from: &'static str,
    to: &'static str,
    began: f32,
}

/// Every `PixelBlock` node's item, by entity.
#[derive(Default)]
pub struct Blocks {
    turns: HashMap<u64, Turn>,
    /// The picture last drawn for each node and the step of the clock it was drawn at: drawn again only when the step
    /// moves on.
    last: HashMap<u64, (u32, Rc<[u8]>)>,
    /// A block was painted this frame: it moves.
    live: bool,
}

impl Blocks {
    /// Paint node `id`'s block `b` into `rect` at frame time `time`: its texels on whole device pixels (`sc` a point),
    /// centred. An item it did not show last time starts a change to it.
    pub fn paint(&mut self, d: &mut Draw, id: u64, b: &PixelBlock, rect: [f32; 4], alpha: f32, sc: f32, time: f32) {
        let name = item(b.face.as_str());
        let turn = self.turns.entry(id).or_insert(Turn { from: name, to: name, began: time - b.turn.max(0.0) }); // alloc: one entry a block in view
        if turn.to != name {
            turn.from = std::mem::replace(&mut turn.to, name);
            turn.began = time;
        }
        let p = if b.turn > 0.0 { ((time - turn.began) / b.turn).clamp(0.0, 1.0) } else { 1.0 };
        // A step of the clock: the item's own motion, and through a change every twelfth of it.
        let step = (time.max(0.0) * FPS * 4.0) as u32 ^ ((p * 12.0) as u32) << 24;
        let texels = match self.last.get(&id) {
            Some((s, px)) if *s == step => px.clone(),
            _ => {
                let px: Rc<[u8]> = render(turn.from, turn.to, p, time).into();
                self.last.insert(id, (step, px.clone())); // alloc: one picture a block in view
                px
            }
        };
        self.live = true;
        let px = (rect[2].min(rect[3]) * sc / CANVAS as f32).floor().max(1.0);
        let side = CANVAS as f32 * px / sc;
        let snap = |v: f32| (v * sc).round() / sc;
        let at = [snap(rect[0] + (rect[2] - side) * 0.5), snap(rect[1] + (rect[3] - side) * 0.5), side, side];
        let size = [CANVAS as f32, CANVAS as f32, alpha, 0.0];
        d.image(ImageInst { rect: at, size, clip: [0.0; 4], clipx: [0.0; 4], light: [0.0; 4], hdr: [0.0; 4] }, texels);
    }

    /// After a frame: whether a block was painted in it (it moves: the host keeps drawing, at its calm rate). Blocks no
    /// longer painted are forgotten.
    pub fn end_frame(&mut self) -> bool {
        let live = std::mem::take(&mut self.live);
        if !live {
            self.turns.clear();
            self.last.clear();
        }
        live
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every sprite's rows are one width (a short row would shear the sprite), every face names an item that draws
    /// something, a change starts as the old item and ends as the new one, and an item moves from step to step.
    #[test]
    fn items_are_whole_and_a_change_goes_from_one_to_the_other() {
        for (name, s) in [("TERM", TERM), ("BOOK", BOOK), ("PAPER", PAPER), ("PENCIL", PENCIL), ("LENS", LENS), ("ANVIL", ANVIL), ("ROCKET", ROCKET), ("GLASS", GLASS), ("DRUM", DRUM), ("GEM", GEM)] {
            assert!(s.iter().all(|r| r.len() == s[0].len()), "{name}'s rows are not one width");
        }
        assert!(LAND.iter().all(|r| r.len() == 24));
        for face in ["exec", "read", "edit", "search", "net", "build", "spawn", "wait", "sql", "think", "no such kind"] {
            let px = render(item(face), item(face), 1.0, 0.0);
            assert!(px.chunks_exact(4).filter(|p| p[3] == 255).count() > 20, "{face} draws next to nothing");
        }
        assert_eq!(item("no such kind"), "build");
        assert_eq!(render("exec", "net", 1.0, 0.0), render("net", "net", 1.0, 0.0), "a change ends as the item it shows");
        assert!(render("exec", "net", 0.25, 0.0) != render("exec", "exec", 1.0, 0.0), "mid-change the old item has sunk");
        assert!(render("net", "net", 1.0, 0.0) != render("net", "net", 1.0, 1.0), "the world turns");
    }
}
