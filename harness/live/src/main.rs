use std::{
    io::Write,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[inline(never)]
fn answer() -> u64 {
    1
}

// What `live_latency.py replay` types over, called on every turn of the loop: a generic
// function (one instance per type it is called with) and a plain one.
#[inline(never)]
fn weigh<T: Copy + Into<u64>>(items: &[T]) -> u64 {
    let mut total = 0;
    for item in items {
        total += (*item).into();
    }
    total
}

#[inline(never)]
fn scale(x: u64) -> u64 {
    x * 2
}

/// State kept across edits: an edit of its fields asks for a restart.
struct Tally {
    turns: u64,
}

fn main() {
    let live = match blaze_live::Live::start() {
        Ok(live) => live,
        Err(why) => {
            println!("no live edits: {why}");
            std::process::exit(2);
        }
    };
    // Applied the moment it arrives, as the Game of Life window does.
    live.apply_as_they_come(|outcome| {
        // When it ran, for live_latency.py to line up with the daemon's log.
        let at = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_micros();
        println!("outcome: {outcome} at_us={at}");
        std::io::stdout().flush().unwrap();
    });
    println!("ready");
    let mut seen = 0;
    let mut tally = Tally { turns: 0 };
    loop {
        let now = answer();
        tally.turns += 1;
        std::hint::black_box(weigh(&[1u8, 2, 3]) + weigh(&[4u32, 5]) + scale(tally.turns));
        if now != seen {
            let at = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_micros();
            println!("answer {now} at_us={at}");
            seen = now;
        }
        std::io::stdout().flush().unwrap();
        std::thread::sleep(Duration::from_millis(1));
    }
}
