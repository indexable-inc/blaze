// The Game of Life of demo/life on a large grid, run flat out without a window, for timing
// what each level of compiled code makes of the same loop. On stdout it prints a checksum of
// the grid every 100 generations, which is the same whatever compiled it; on stderr the
// generations per second every half second, with the level of the functions the daemon
// tiered.
//
//   LIFE_GENERATIONS  generations to run (default: until LIFE_SECONDS are over)
//   LIFE_SECONDS      seconds to run (default 20)
//   LIFE_SIZE         the side of the grid (default 512)

use std::time::{Duration, Instant};

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

struct Life {
    side: usize,
    age: Vec<u32>,
    generation: u64,
    seed: u64,
}

impl Life {
    fn new(side: usize) -> Life {
        let mut life = Life { side, age: vec![0; side * side], generation: 0, seed: 12345 };
        life.reseed();
        life
    }

    fn rand(&mut self) -> u64 {
        self.seed = (self.seed.wrapping_mul(1103515245).wrapping_add(12345)) % 2147483648;
        (self.seed >> 8) % 100
    }

    fn reseed(&mut self) {
        for i in 0..self.side * self.side {
            self.age[i] = if self.rand() < 30 { 1 } else { 0 };
        }
    }

    fn neighbours(&self, x: usize, y: usize) -> u32 {
        let side = self.side;
        let mut n = 0;
        for dy in 0..3 {
            for dx in 0..3 {
                if dx != 1 || dy != 1 {
                    let xx = (x + side + dx - 1) % side;
                    let yy = (y + side + dy - 1) % side;
                    if self.age[yy * side + xx] > 0 {
                        n += 1;
                    }
                }
            }
        }
        n
    }

    fn step(&mut self) {
        let side = self.side;
        let mut next = vec![0; side * side];
        for y in 0..side {
            for x in 0..side {
                next[y * side + x] = next_age(self.age[y * side + x], self.neighbours(x, y));
            }
        }
        self.age = next;
        self.generation += 1;
        if self.age.iter().filter(|a| **a > 0).count() < 25 {
            self.reseed();
        }
    }

    fn checksum(&self) -> u64 {
        let mut sum = 0u64;
        for (i, age) in self.age.iter().enumerate() {
            sum = sum.wrapping_mul(31).wrapping_add(u64::from(*age) ^ i as u64);
        }
        sum
    }
}

fn number(name: &str) -> Option<u64> {
    std::env::var(name).ok()?.parse().ok()
}

/// The levels of the functions the daemon tiered, as one line: `step=2 neighbours=3`, and
/// the sampler's share of a core in thousandths. What the daemon did since `seen` is printed
/// as it comes, a line each, with how long after `started` (milliseconds since the epoch).
fn levels(seen: &mut u64, started: u64) -> String {
    let tiers = match blaze_live::tiers() {
        Ok(Some(tiers)) => tiers,
        Ok(None) => return String::new(),
        Err(why) => return format!("tiers_error={why:?}"),
    };
    let since = *seen;
    for event in tiers.events.iter().filter(|it| it.at > since) {
        let at = event.at.saturating_sub(started) as f64 / 1e3;
        eprintln!("tier: t={at:.1} {}", event.text);
        *seen = event.at;
    }
    let mut shown: Vec<String> = tiers
        .functions
        .iter()
        .filter(|it| it.level > 0)
        .map(|it| {
            let name = it.name.rsplit("::").next().unwrap_or(&it.name);
            format!("{name}={}", it.level)
        })
        .collect();
    shown.sort();
    shown.push(format!("sampler_permille={}", tiers.sampler_permille));
    shown.join(" ")
}

fn epoch_ms() -> u64 {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH);
    now.map_or(0, |it| it.as_millis() as u64)
}

fn main() {
    let side = number("LIFE_SIZE").unwrap_or(512) as usize;
    let generations = number("LIFE_GENERATIONS");
    let seconds = Duration::from_secs(number("LIFE_SECONDS").unwrap_or(20));
    match blaze_live::Live::start() {
        Ok(live) => live.apply_as_they_come(|outcome| eprintln!("live: {outcome}")),
        Err(why) => eprintln!("live: off: {why}"),
    }
    let mut life = Life::new(side);
    let start = Instant::now();
    let started = epoch_ms();
    let (mut last, mut last_generation) = (start, 0);
    let mut seen = 0;
    loop {
        life.step();
        if life.generation % 100 == 0 {
            println!("generation {} checksum {:016x}", life.generation, life.checksum());
        }
        let now = Instant::now();
        if now - last >= Duration::from_millis(500) {
            let rate = (life.generation - last_generation) as f64 / (now - last).as_secs_f64();
            let at = (now - start).as_secs_f64();
            eprintln!("rate t={at:.1} gens_per_s={rate:.1} {}", levels(&mut seen, started));
            (last, last_generation) = (now, life.generation);
        }
        let done = match generations {
            Some(generations) => life.generation >= generations,
            None => now - start >= seconds,
        };
        if done {
            break;
        }
    }
}
