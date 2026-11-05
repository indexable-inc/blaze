// Conway's Game of Life under blaze. Edit this file while it
// runs: the daemon recompiles only the function you touched,
// and the next frame uses it, with the grid intact.
//
// Try:  next_age(): `n == 3` to `n == 3 || n == 6` (HighLife)
//       color():    change the numbers to repaint the cells
//       step_ms():  make it faster or slower
//       step_ms():  0 runs flat out; watch the Tiers panel promote step()
//
// Live edits run on macOS on Apple silicon, and Linux and
// Windows on x86-64; elsewhere the window runs and edits need a
// restart. The window and this editor are the other files in src/.
mod editor;
mod highlight;
mod theme;
mod tiers;
mod window;

use std::time::Instant;

const W: usize = 64;
const H: usize = 48;

/// Milliseconds between generations; 0 steps as often as a frame has time for.
#[inline(never)]
fn step_ms() -> u64 {
    80
}

/// The rule. `age` is 0 for a dead cell, else how many generations it has lived; `n` is the
/// number of live neighbours. Returns the new age.
#[inline(never)]
fn next_age(age: u32, n: u32) -> u32 {
    if age > 0 {
        if n == 2 || n == 3 { age + 1 } else { 0 }
    } else if n == 3 {
        1
    } else {
        0
    }
}

/// Colour of a live cell as 0xRRGGBB: young cells are bright and old ones fade to blue.
#[inline(never)]
fn color(age: u32) -> u32 {
    if age == 1 {
        0xffffff
    } else if age < 4 {
        0xffd23f
    } else if age < 12 {
        0x3ddc97
    } else {
        0x3a86ff
    }
}

// ---- State that lives across edits. --------------------------------------------------------

struct Life {
    age: Vec<u32>,
    generation: u64,
    seed: u64,
    next_step: Instant,
}

impl Life {
    fn new() -> Life {
        let mut life =
            Life { age: vec![0; W * H], generation: 0, seed: 12345, next_step: Instant::now() };
        life.reseed();
        life
    }

    fn rand(&mut self) -> u64 {
        self.seed = (self.seed.wrapping_mul(1103515245).wrapping_add(12345)) % 2147483648;
        (self.seed >> 8) % 100
    }

    fn reseed(&mut self) {
        for i in 0..W * H {
            self.age[i] = if self.rand() < 30 { 1 } else { 0 };
        }
    }

    fn neighbours(&self, x: usize, y: usize) -> u32 {
        let mut n = 0;
        for dy in 0..3 {
            for dx in 0..3 {
                if dx != 1 || dy != 1 {
                    let xx = (x + W + dx - 1) % W;
                    let yy = (y + H + dy - 1) % H;
                    if self.age[yy * W + xx] > 0 {
                        n += 1;
                    }
                }
            }
        }
        n
    }

    fn step(&mut self) {
        let mut next = vec![0; W * H];
        for y in 0..H {
            for x in 0..W {
                next[y * W + x] = next_age(self.age[y * W + x], self.neighbours(x, y));
            }
        }
        self.age = next;
        self.generation += 1;
        if self.age.iter().filter(|a| **a > 0).count() < 25 {
            self.reseed();
        }
    }
}

fn main() -> eframe::Result {
    window::run(Life::new())
}
