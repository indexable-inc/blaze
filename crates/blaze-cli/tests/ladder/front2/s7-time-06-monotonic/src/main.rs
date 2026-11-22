//! s7-time-06: the monotonic clock and `Duration`. `Instant`: "Opaque and useful only with
//! Duration. Instants are always guaranteed, barring platform bugs, to be no less than any
//! previously measured instant when created" (std/src/time.rs); `thread::sleep`: "The thread may
//! sleep longer than the duration specified ... It will never sleep less." `Duration`'s `Debug`
//! (core/src/time.rs `fmt_decimal`): the largest unit with a non-zero integer part, trailing
//! zeros dropped, "µs" for microseconds, `{:.3?}` rounding to three digits. No wall-clock value
//! is printed.

use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn main() {
    let start = Instant::now();
    let mut prev = start;
    let mut backwards = 0;
    for _ in 0..10_000 {
        let now = Instant::now();
        if now < prev {
            backwards += 1;
        }
        prev = now;
    }
    println!("went backwards: {backwards}");
    thread::sleep(Duration::from_millis(20));
    let e = start.elapsed();
    println!("slept at least 20ms: {}", e >= Duration::from_millis(20));
    println!("{}", prev.duration_since(start) <= start.elapsed());
    let now = Instant::now();
    println!("{:?}", now.checked_duration_since(now + Duration::from_secs(1)));
    println!("{:?} {:?} {:?}", Duration::from_millis(1500), Duration::from_micros(250), Duration::from_nanos(7));
    println!("{:?}", Duration::from_secs(90) + Duration::from_millis(250));
    println!("{} {}", Duration::from_millis(2500).as_secs(), Duration::from_millis(2500).subsec_millis());
    println!("{:?}", Duration::from_secs(1).checked_sub(Duration::from_secs(2)));
    println!("{:.3?}", Duration::from_nanos(1_234_567));
    println!("{:?}", (UNIX_EPOCH + Duration::from_secs(10)).duration_since(UNIX_EPOCH));
    println!("{}", SystemTime::now() >= UNIX_EPOCH);
}
