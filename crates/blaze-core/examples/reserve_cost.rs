//! What a record costs stored by `Put` (built whole in the heap, then one `Effect::Write`)
//! against `reserve` (written in place through `Effect::MapWrite`, then cut, synced and
//! renamed), on this machine's driver, under a directory of the disk to measure:
//!
//!     examplerun.sh blaze-core reserve_cost DIR
//!
//! For each size, the median wall time of each form over several records, each under a key of
//! its own, the bytes filled the same way in both. The size at which a task reserves rather
//! than puts is read from this table (docs/v2/plan/design/fetch.md).

#![forbid(unsafe_code)]
#![allow(clippy::print_stdout)]

use std::time::Instant;

use blaze_core::{
    Completion, Done, Driver, Effect, FileName, Tag,
    reserve::{Next, Reserve},
    run_until_idle,
};

fn ask(driver: &mut impl Driver, effect: Effect) -> Done {
    let mut effects = vec![effect];
    let mut got = None;
    run_until_idle(driver, &mut effects, |Completion { done, .. }, _| got = Some(done))
        .expect("the effect ends");
    let done = got.expect("one completion");
    assert!(!matches!(done, Done::Failed(_)), "{done:?}");
    done
}

fn fill(bytes: &mut [u8], seed: u8) {
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = seed.wrapping_add(i as u8);
    }
}

fn put(driver: &mut impl Driver, key: FileName, size: usize, seed: u8) {
    let mut bytes = vec![0u8; size];
    fill(&mut bytes, seed);
    ask(driver, Effect::Write { file: key, bytes, tag: Tag(1) });
}

fn reserve(driver: &mut impl Driver, key: FileName, size: usize, seed: u8) {
    let (mut reserve, first) = Reserve::new(key, size as u64, Tag(2));
    let mut done = ask(driver, first);
    loop {
        done = match reserve.answer(done).expect("the protocol") {
            Next::Write(mut mapping) => {
                fill(mapping.bytes_mut(), seed);
                let effect = reserve.finish(mapping, size as u64).expect("finish");
                ask(driver, effect)
            }
            Next::Ask(effect) => ask(driver, effect),
            Next::Stored => return,
            Next::Abandoned => panic!("abandoned"),
        };
    }
}

fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

fn main() {
    let dir = std::env::args().nth(1).expect("usage: reserve_cost DIR");
    let root = std::path::Path::new(&dir);
    for sub in ["put", "res"] {
        std::fs::create_dir_all(root.join(sub)).expect("a directory to measure in");
    }
    let mut driver = blaze_core::native(root, 2).expect("the native driver");
    println!("{:>10} {:>5} {:>12} {:>12} {:>7}", "size", "n", "put us", "reserve us", "ratio");
    let mut n = 0u32;
    for shift in [12, 14, 16, 17, 18, 19, 20, 21, 22, 24, 26] {
        let size = 1usize << shift;
        let reps = ((256usize << 20) / size).clamp(5, 60);
        let (mut puts, mut reserves) = (Vec::new(), Vec::new());
        for rep in 0..reps {
            n += 1;
            let seed = rep as u8;
            let began = Instant::now();
            put(&mut driver, FileName::new(&format!("put/{n}")).expect("a name"), size, seed);
            puts.push(began.elapsed().as_secs_f64() * 1e6);
            let began = Instant::now();
            reserve(&mut driver, FileName::new(&format!("res/{n}")).expect("a name"), size, seed);
            reserves.push(began.elapsed().as_secs_f64() * 1e6);
        }
        let (p, r) = (median(puts), median(reserves));
        println!("{size:>10} {reps:>5} {p:>12.0} {r:>12.0} {:>7.2}", r / p);
    }
}
