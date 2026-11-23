//! What a stat costs through this machine's driver, against the system call alone, and what a
//! column of them costs as one [`Effect::Stamp`], under a directory of the disk to measure:
//!
//!     examplerun.sh blaze-core stat_cost DIR [ROWS]
//!
//! Makes ROWS files (870 by default) under DIR/stat, then prints the median and mean of:
//! - `lstat`: `std::fs::symlink_metadata` of each file by its whole path, in this thread (the
//!   floor: the system call and the path walk, no driver);
//! - `one Stat`: one [`Effect::Stat`] submitted and polled to its completion, alone;
//! - `32 Stats`: 32 submitted at once, per stat (what of the round trip is the hand-off);
//! - `Stamp ROWS`: one [`Effect::Stamp`] of every file, whole and per row.

#![forbid(unsafe_code)]
#![allow(clippy::print_stdout)]

use std::{sync::Arc, time::Instant};

use blaze_core::{Completion, Done, Driver, Effect, FileName, Names, Tag, wait::Deadline};
use blaze_own::Share;

/// Submits `effects` and polls until each has completed; every one must have done.
fn round(driver: &mut impl Driver, effects: &mut Vec<Effect>, done: &mut Vec<Completion>) {
    let want = effects.len();
    driver.submit(effects);
    let mut got = 0;
    while got < want {
        let until = Deadline::after(driver.now(), std::time::Duration::from_secs(10));
        got += driver.poll(done, until);
        assert!(!until.passed(driver.now()) || got >= want, "the driver stalled");
    }
    for Completion { done, .. } in done.drain(..) {
        assert!(!matches!(done, Done::Failed(_)), "{done:?}");
    }
}

fn stats(mut us: Vec<f64>) -> (f64, f64) {
    us.sort_by(f64::total_cmp);
    (us[us.len() / 2], us.iter().sum::<f64>() / us.len() as f64)
}

fn row(what: &str, per: &str, (median, mean): (f64, f64)) {
    println!("{what:<14} {per:<9} median {median:>9.2} us   mean {mean:>9.2} us");
}

fn main() {
    let dir = std::env::args().nth(1).expect("usage: stat_cost DIR [ROWS]");
    let rows: usize = std::env::args().nth(2).map_or(870, |it| it.parse().expect("ROWS, a count"));
    let root = std::path::Path::new(&dir);
    std::fs::create_dir_all(root.join("stat")).expect("a directory to measure in");
    let mut names = Names::with_capacity(rows, rows * 16);
    let mut files = Vec::with_capacity(rows);
    for n in 0..rows {
        let name = FileName::new(&format!("stat/{n:05}.rs")).expect("a name");
        std::fs::write(root.join(name.as_str()), b"fn f() {}\n").expect("a file to stat");
        names.push(&name);
        files.push(name);
    }
    let names = Arc::new(names);
    let mut driver = blaze_core::native(root, 2).expect("the native driver");
    let mut done = Vec::with_capacity(64);
    let mut effects = Vec::with_capacity(64);

    // Warm: every inode in the cache, the driver's threads started.
    for _ in 0..3 {
        effects.push(Effect::Stamp { files: names.clone(), tag: Tag(0) });
        round(&mut driver, &mut effects, &mut done);
    }

    let mut us = Vec::with_capacity(rows * 4);
    for _ in 0..4 {
        for file in &files {
            let began = Instant::now();
            let meta = std::fs::symlink_metadata(root.join(file.as_str())).expect("there");
            us.push(began.elapsed().as_secs_f64() * 1e6);
            std::hint::black_box(meta);
        }
    }
    row("lstat", "per stat", stats(std::mem::take(&mut us)));

    for file in files.iter().cycle().take(4000) {
        let began = Instant::now();
        effects.push(Effect::Stat { file: file.share(), tag: Tag(1) });
        round(&mut driver, &mut effects, &mut done);
        us.push(began.elapsed().as_secs_f64() * 1e6);
    }
    row("one Stat", "per stat", stats(std::mem::take(&mut us)));

    for chunk in files.chunks_exact(32).cycle().take(400) {
        let began = Instant::now();
        effects.extend(chunk.iter().enumerate().map(|(at, file)| Effect::Stat { file: file.share(), tag: Tag(at as u64) }));
        round(&mut driver, &mut effects, &mut done);
        us.push(began.elapsed().as_secs_f64() * 1e6 / 32.0);
    }
    row("32 Stats", "per stat", stats(std::mem::take(&mut us)));

    let mut per_row = Vec::with_capacity(400);
    for _ in 0..400 {
        let began = Instant::now();
        effects.push(Effect::Stamp { files: names.clone(), tag: Tag(2) });
        round(&mut driver, &mut effects, &mut done);
        let whole = began.elapsed().as_secs_f64() * 1e6;
        us.push(whole);
        per_row.push(whole / rows as f64);
    }
    row(&format!("Stamp {rows}"), "whole", stats(us));
    row(&format!("Stamp {rows}"), "per row", stats(per_row));
}
