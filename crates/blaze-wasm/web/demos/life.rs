//! Conway's Game of Life, compiled in this page by blaze. Click to drop a glider. Edit
//! `alive` to change the rules: the cells that are there carry on under the new ones.
#![no_std]

unsafe extern "C" {
    fn fill(red: u32, green: u32, blue: u32);
    fn rect(x: f64, y: f64, width: f64, height: f64);
    fn text(text: *const u8, len: usize, x: f64, y: f64);
}

const COLUMNS: usize = 64;
const ROWS: usize = 48;
/// Seconds between generations.
const PERIOD: f64 = 0.08;

struct Life {
    cells: [[bool; COLUMNS]; ROWS],
    since: f64,
    seeded: bool,
}

static mut LIFE: Life = Life { cells: [[false; COLUMNS]; ROWS], since: 0.0, seeded: false };

/// The rule: whether a cell lives on, given how many of its eight neighbours do.
fn alive(was: bool, neighbours: usize) -> bool {
    match (was, neighbours) {
        (true, 2 | 3) => true,
        (false, 3) => true,
        _ => false,
    }
}

const GLIDER: [(usize, usize); 5] = [(1, 0), (2, 1), (0, 2), (1, 2), (2, 2)];

impl Life {
    fn neighbours(&self, row: usize, column: usize) -> usize {
        let mut count = 0;
        for dy in [ROWS - 1, 0, 1] {
            for dx in [COLUMNS - 1, 0, 1] {
                if (dy, dx) != (0, 0) && self.cells[(row + dy) % ROWS][(column + dx) % COLUMNS] {
                    count += 1;
                }
            }
        }
        count
    }

    fn step(&mut self) {
        let mut next = [[false; COLUMNS]; ROWS];
        for row in 0..ROWS {
            for column in 0..COLUMNS {
                next[row][column] = alive(self.cells[row][column], self.neighbours(row, column));
            }
        }
        self.cells = next;
    }

    fn stamp(&mut self, shape: &[(usize, usize)], row: usize, column: usize) {
        for &(dx, dy) in shape {
            self.cells[(row + dy) % ROWS][(column + dx) % COLUMNS] = true;
        }
    }
}

fn label(text: &str, x: f64, y: f64) {
    unsafe {
        fill(230, 230, 230);
        crate::text(text.as_ptr(), text.len(), x, y);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn frame(width: f64, height: f64, dt: f64) {
    let life = unsafe { &mut *(&raw mut LIFE) };
    if !life.seeded {
        life.seeded = true;
        for at in 0..8 {
            life.stamp(&GLIDER, at * 5, at * 7);
        }
    }
    unsafe { SIZE = (width, height) };
    life.since += dt;
    if life.since >= PERIOD {
        life.since = 0.0;
        life.step();
    }
    let (w, h) = (width / COLUMNS as f64, height / ROWS as f64);
    unsafe {
        fill(24, 26, 32);
        rect(0.0, 0.0, width, height);
        fill(120, 220, 140);
    }
    for row in 0..ROWS {
        for column in 0..COLUMNS {
            if life.cells[row][column] {
                unsafe { rect(column as f64 * w, row as f64 * h, w - 1.0, h - 1.0) };
            }
        }
    }
    label("click to drop a glider", 16.0, 28.0);
}

#[unsafe(no_mangle)]
pub extern "C" fn click(x: f64, y: f64) {
    let life = unsafe { &mut *(&raw mut LIFE) };
    let (width, height) = unsafe { SIZE };
    let column = (x / width * COLUMNS as f64) as usize;
    let row = (y / height * ROWS as f64) as usize;
    life.stamp(&GLIDER, row, column);
}

/// The canvas as the last frame saw it, for a click to find its cell.
static mut SIZE: (f64, f64) = (1.0, 1.0);
