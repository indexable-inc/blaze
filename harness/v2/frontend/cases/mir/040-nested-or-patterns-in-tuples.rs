//@ expect: accept
#[derive(Clone, Copy)]
enum Color {
    Red,
    Green,
    Blue,
}

fn mix(a: Option<Color>, b: Result<u8, bool>) -> u32 {
    match (a, b) {
        (Some(Color::Red | Color::Green), Ok(1 | 2)) => 1,
        (Some(Color::Blue) | None, Ok(n @ (3..=5 | 9))) => 10 + u32::from(n),
        (None, Err(true | false)) => 20,
        (Some(c), Err(flag)) if flag || matches!(c, Color::Red) => 30,
        (Some(Color::Green | Color::Blue), Ok(0) | Err(_)) => 40,
        (_, Ok(n)) => 50 + u32::from(n),
        (Some(Color::Red), Err(_)) => 60,
    }
}

fn main() {
    let colors = [Some(Color::Red), Some(Color::Green), Some(Color::Blue), None];
    let results = [Ok(1), Ok(3), Ok(9), Ok(0), Ok(7), Err(true), Err(false)];
    for a in colors {
        for b in results {
            print!("{} ", mix(a, b));
        }
        println!();
    }
}
