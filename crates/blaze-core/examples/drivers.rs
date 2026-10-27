//! The drivers on the same effects, under a directory of the disk to measure:
//!
//!     cargo run --release -p blaze-core --example drivers -- DIR [threads N] [uring]
//!
//! `commit`: one append of 3 KB and its sync, then the next, as the journal commits one
//! transaction at a time (2,000 of them; microseconds from the append asked to the sync done).
//! `batch`: 64 appends of 4 KB to 8 files and a sync of each, all in flight together (200
//! rounds; microseconds a round). `reads`: 4,096 reads of 4 KB at random places of a 64 MB
//! file, all in flight (milliseconds for all, five times; the file is read once first, so they
//! come from the page cache).

#![allow(clippy::print_stdout)]

use std::time::Instant;

use blaze_core::{
    Completion, Done, Driver, Effect, FileName, Tag, run_until_idle, threads::Threads,
};

fn percentile(values: &mut [f64], p: f64) -> f64 {
    values.sort_by(f64::total_cmp);
    values[((values.len() - 1) as f64 * p).round() as usize]
}

fn check(done: &Completion) {
    assert!(!matches!(done.done, Done::Failed(_)), "{done:?}");
}

fn commit(driver: &mut dyn Driver) -> Vec<f64> {
    let file = FileName::new("commit/journal").unwrap();
    let bytes = vec![7u8; 3 << 10];
    (0..2000)
        .map(|n| {
            let began = Instant::now();
            let mut effects =
                vec![Effect::Append { file: file.clone(), bytes: bytes.clone(), tag: Tag(n) }];
            run_until_idle(driver, &mut effects, |done, effects| {
                check(&done);
                if done.done == Done::Appended {
                    effects.push(Effect::Sync { file: file.clone(), tag: done.tag });
                }
            });
            began.elapsed().as_secs_f64() * 1e6
        })
        .collect()
}

fn batch(driver: &mut dyn Driver) -> Vec<f64> {
    let files: Vec<FileName> =
        (0..8).map(|n| FileName::new(&format!("batch/{n}")).unwrap()).collect();
    let bytes = vec![9u8; 4 << 10];
    (0..200)
        .map(|_| {
            let began = Instant::now();
            let mut effects: Vec<Effect> = (0..64)
                .map(|n| Effect::Append {
                    file: files[n % 8].clone(),
                    bytes: bytes.clone(),
                    tag: Tag(n as u64),
                })
                .collect();
            let mut left = [8usize; 8];
            run_until_idle(driver, &mut effects, |done, effects| {
                check(&done);
                if done.done == Done::Appended {
                    let at = done.tag.0 as usize % 8;
                    left[at] -= 1;
                    if left[at] == 0 {
                        effects.push(Effect::Sync {
                            file: files[at].clone(),
                            tag: Tag(100 + at as u64),
                        });
                    }
                }
            });
            began.elapsed().as_secs_f64() * 1e6
        })
        .collect()
}

fn reads(driver: &mut dyn Driver, dir: &std::path::Path) -> Vec<f64> {
    let name = "reads/data";
    let path = dir.join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, vec![3u8; 64 << 20]).unwrap();
    let _ = std::fs::read(&path).unwrap();
    let file = FileName::new(name).unwrap();
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    (0..5)
        .map(|_| {
            let mut effects: Vec<Effect> = (0..4096)
                .map(|n| {
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    let at = seed % ((64 << 20) - (4 << 10));
                    Effect::Read { file: file.clone(), at, len: 4 << 10, tag: Tag(n) }
                })
                .collect();
            let began = Instant::now();
            run_until_idle(driver, &mut effects, |done, _| match &done.done {
                Done::Read(bytes) => assert_eq!(bytes.len(), 4 << 10),
                _ => check(&done),
            });
            began.elapsed().as_secs_f64() * 1e3
        })
        .collect()
}

fn measure(name: &str, dir: &std::path::Path, driver: &mut dyn Driver) {
    let mut commits = commit(driver);
    let mut batches = batch(driver);
    let mut read = reads(driver, dir);
    println!(
        "{name:12} commit p50 {:7.1} us p99 {:7.1} us | batch p50 {:8.1} us p99 {:8.1} us | 4096 reads {:6.2} ms (best {:6.2})",
        percentile(&mut commits, 0.5),
        percentile(&mut commits, 0.99),
        percentile(&mut batches, 0.5),
        percentile(&mut batches, 0.99),
        percentile(&mut read, 0.5),
        percentile(&mut read, 0.0),
    );
}

fn main() {
    let mut args = std::env::args().skip(1);
    let root = std::path::PathBuf::from(args.next().expect("a directory to measure under"));
    let rest: Vec<String> = args.collect();
    let mut asked = rest.iter().peekable();
    let mut drivers: Vec<(String, Option<usize>)> = Vec::new();
    while let Some(name) = asked.next() {
        match name.as_str() {
            "threads" => {
                let n = asked
                    .next_if(|it| it.parse::<usize>().is_ok())
                    .map_or(4, |it| it.parse().unwrap());
                drivers.push((format!("threads {n}"), Some(n)));
            }
            "uring" => drivers.push(("uring".to_owned(), None)),
            other => panic!("no driver {other}"),
        }
    }
    if drivers.is_empty() {
        drivers = vec![("threads 4".to_owned(), Some(4)), ("uring".to_owned(), None)];
    }
    for (name, threads) in drivers {
        let dir = root.join(name.replace(' ', "-"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        match threads {
            Some(n) => measure(&name, &dir, &mut Threads::new(&dir, n)),
            #[cfg(target_os = "linux")]
            None => measure(&name, &dir, &mut blaze_core::uring::Uring::new(&dir).expect("a ring")),
            #[cfg(not(target_os = "linux"))]
            None => eprintln!("uring: Linux only"),
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
