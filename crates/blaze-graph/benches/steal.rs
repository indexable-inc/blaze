//! The pool's [`Victims`] against each other at the end of a wave: one job on a worker fans out
//! `FAN` jobs, each of which fans out `FAN` leaves of about `LEAF_NS` of work, and the wave ends
//! when every leaf has run, so every worker goes idle and searches at once, as at the end of a
//! crate wave of the self-build.
//!
//! Each row is the best of `ROUNDS` runs of `WAVES` waves: the wall time of a wave, the time a
//! perfect split of its leaves over every worker would take, and the difference, the time
//! workers spent idle or searching at the wave's ends. The topology is this machine's, its
//! workers bound to their CPUs as the process's pool binds them.
//!
//! Run: `blaze bench -p blaze-graph --bench steal`.

use std::{
    hint::black_box,
    time::{Duration, Instant},
};

use blaze_graph::{
    pool::{Pool, Stealing, Victims},
    topology::Topology,
};

const FAN: usize = 64;
const WAVES: usize = 50;
const ROUNDS: usize = 5;
/// The work of one leaf, about.
const LEAF_NS: u64 = 20_000;

/// About `iterations` of a loop the optimizer keeps.
fn spin(iterations: u64) -> u64 {
    let mut sum = 0u64;
    for it in 0..iterations {
        sum = black_box(sum.wrapping_add(it));
    }
    sum
}

/// The iterations of [`spin`] that take about [`LEAF_NS`] on this thread.
fn calibrate() -> u64 {
    let probe = 1_000_000u64;
    let began = Instant::now();
    black_box(spin(probe));
    let took = began.elapsed().as_nanos().max(1) as u64;
    (probe * LEAF_NS / took).max(1)
}

/// The wall time of `WAVES` waves on `pool`.
fn round(pool: &Pool, iterations: u64) -> Duration {
    let began = Instant::now();
    for _ in 0..WAVES {
        pool.install(|| {
            pool.scope(|outer| {
                for _ in 0..FAN {
                    outer.submit(
                        None,
                        || {
                            pool.scope(|inner| {
                                for _ in 0..FAN {
                                    inner.submit(None, || black_box(spin(iterations)), |it| {
                                        assert!(it.is_ok(), "a leaf panicked");
                                    });
                                }
                            });
                        },
                        |it| assert!(it.is_ok(), "a fan-out job panicked"),
                    );
                }
            });
        });
    }
    began.elapsed()
}

fn main() {
    let topology = Topology::detect().expect("this machine's topology");
    let bind = cfg!(any(target_os = "linux", target_os = "macos"));
    let iterations = calibrate();
    let workers = topology.cpus.len() as u64;
    let ideal = Duration::from_nanos((FAN * FAN) as u64 * LEAF_NS / workers);
    println!(
        "{workers} workers in {} groups, {WAVES} waves of {FAN}x{FAN} leaves of ~{LEAF_NS} ns, best of {ROUNDS}",
        topology.groups()
    );
    println!("victims      wave (us)  ideal (us)  idle at ends (us)");
    for victims in [Victims::Walk, Victims::TwoChoices] {
        let stealing = Stealing { victims, seed: Stealing::DEFAULT.seed };
        let pool = Pool::with_stealing(&topology, bind, stealing).expect("the pool");
        let best =
            (0..ROUNDS).map(|_| round(&pool, iterations)).min().expect("at least one round");
        let wave = best / WAVES as u32;
        println!(
            "{:<11} {:>10.1} {:>11.1} {:>18.1}",
            format!("{victims:?}"), // alloc: a cell of the table
            wave.as_secs_f64() * 1e6,
            ideal.as_secs_f64() * 1e6,
            wave.saturating_sub(ideal).as_secs_f64() * 1e6
        );
    }
}
