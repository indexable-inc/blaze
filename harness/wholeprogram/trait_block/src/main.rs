trait Mix {
    fn seed(&self) -> u32;

    fn mixed(&self, rounds: u32) -> u32 {
        #[inline(never)]
        fn step(at: u32) -> u32 {
            at.rotate_left(5) ^ 0x9e37_79b9
        }
        static SALT: u32 = 17;
        let mut at = self.seed();
        for _ in 0..rounds {
            at = step(at).wrapping_add(SALT);
        }
        at
    }
}

struct Seeded(u32);

impl Mix for Seeded {
    fn seed(&self) -> u32 {
        self.0
    }
}

fn main() {
    let rounds = std::env::args().count() as u32 + 6;
    println!("{}", Seeded(3).mixed(rounds));
    std::process::exit((Seeded(11).mixed(rounds) % 7) as i32);
}
