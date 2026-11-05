//! Sorting, one step a frame, compiled in this page by blaze. Click to shuffle and move on
//! to the next algorithm. Each is a `dyn Sorter`; edit one while it runs.
#![no_std]

unsafe extern "C" {
    fn fill(red: u32, green: u32, blue: u32);
    fn rect(x: f64, y: f64, width: f64, height: f64);
    fn text(text: *const u8, len: usize, x: f64, y: f64);
}

const LEN: usize = 48;
/// Steps of the sort in one frame.
const SPEED: usize = 2;

/// A sort that can be run a step at a time: all it remembers is in `State`.
trait Sorter {
    fn name(&self) -> &'static str;
    /// Does one comparison, and says which two bars it looked at.
    fn step(&self, bars: &mut [u32], state: &mut State) -> (usize, usize);
}

#[derive(Clone, Copy)]
struct State {
    i: usize,
    j: usize,
    done: bool,
}

struct Bubble;
struct Insertion;
struct Selection;

impl Sorter for Bubble {
    fn name(&self) -> &'static str {
        "bubble sort"
    }

    fn step(&self, bars: &mut [u32], state: &mut State) -> (usize, usize) {
        let (a, b) = (state.j, state.j + 1);
        if bars[a] > bars[b] {
            bars.swap(a, b);
        }
        state.j += 1;
        if state.j + 1 >= bars.len() - state.i {
            state.j = 0;
            state.i += 1;
            state.done = state.i + 1 >= bars.len();
        }
        (a, b)
    }
}

impl Sorter for Insertion {
    fn name(&self) -> &'static str {
        "insertion sort"
    }

    fn step(&self, bars: &mut [u32], state: &mut State) -> (usize, usize) {
        if state.i == 0 {
            state.i = 1;
            state.j = 1;
        }
        let at = state.j;
        if at > 0 && bars[at - 1] > bars[at] {
            bars.swap(at - 1, at);
            state.j -= 1;
        } else {
            state.i += 1;
            state.j = state.i;
            state.done = state.i >= bars.len();
        }
        (at, at.saturating_sub(1))
    }
}

impl Sorter for Selection {
    fn name(&self) -> &'static str {
        "selection sort"
    }

    fn step(&self, bars: &mut [u32], state: &mut State) -> (usize, usize) {
        if state.j <= state.i {
            state.j = state.i + 1;
        }
        let (a, b) = (state.i, state.j);
        if bars[b] < bars[a] {
            bars.swap(a, b);
        }
        state.j += 1;
        if state.j >= bars.len() {
            state.i += 1;
            state.j = state.i + 1;
            state.done = state.i + 1 >= bars.len();
        }
        (a, b)
    }
}

static SORTERS: [&(dyn Sorter + Sync); 3] = [&Bubble, &Insertion, &Selection];

struct World {
    bars: [u32; LEN],
    state: State,
    sorter: usize,
    seed: u32,
    looked: (usize, usize),
}

static mut WORLD: World = World {
    bars: [0; LEN],
    state: State { i: 0, j: 0, done: true },
    sorter: 2,
    seed: 12345,
    looked: (0, 0),
};

impl World {
    fn random(&mut self) -> u32 {
        self.seed ^= self.seed << 13;
        self.seed ^= self.seed >> 17;
        self.seed ^= self.seed << 5;
        self.seed
    }

    fn shuffle(&mut self) {
        for (at, bar) in self.bars.iter_mut().enumerate() {
            *bar = at as u32 + 1;
        }
        for at in (1..LEN).rev() {
            let other = self.random() as usize % (at + 1);
            self.bars.swap(at, other);
        }
        self.state = State { i: 0, j: 0, done: false };
        self.sorter = (self.sorter + 1) % SORTERS.len();
    }
}

fn label(text: &str, x: f64, y: f64) {
    unsafe {
        fill(230, 230, 230);
        crate::text(text.as_ptr(), text.len(), x, y);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn frame(width: f64, height: f64, _dt: f64) {
    let world = unsafe { &mut *(&raw mut WORLD) };
    if world.bars[0] == 0 {
        world.shuffle();
    }
    let sorter = SORTERS[world.sorter];
    for _ in 0..SPEED {
        if !world.state.done {
            world.looked = sorter.step(&mut world.bars, &mut world.state);
        }
    }
    unsafe {
        fill(24, 26, 32);
        rect(0.0, 0.0, width, height);
    }
    let bar = width / LEN as f64;
    for (at, &value) in world.bars.iter().enumerate() {
        let tall = (height - 60.0) * value as f64 / LEN as f64;
        let looked = !world.state.done && (at == world.looked.0 || at == world.looked.1);
        unsafe {
            if looked {
                fill(255, 200, 80);
            } else {
                fill(90, 150, 240);
            }
            rect(at as f64 * bar, height - tall, bar - 1.0, tall);
        }
    }
    label(sorter.name(), 16.0, 28.0);
    if world.state.done {
        label("sorted: click for the next", 16.0, 48.0);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn click(_x: f64, _y: f64) {
    let world = unsafe { &mut *(&raw mut WORLD) };
    world.shuffle();
}
