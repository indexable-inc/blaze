//! Sizes and speeds of the profile's tables in each encoding and codec, on real profile files:
//! `BLAZE_PARQUET_MEASURE` names a directory of them (`<n>-<id>.<table>.parquet`, as
//! blaze-profile writes its history). Ignored unless asked for (`--ignored --nocapture`), on a
//! release build: it prints the crate doc's table.

#![feature(allocator_api)]

use std::{alloc::Global, collections::BTreeMap, time::Instant};

use blaze_columns::{Batch, Field};
use blaze_parquet::{Compression, Encoding, File, Options, write};

const CONFIGS: [(&str, Encoding, Compression); 5] = [
    ("plain, none", Encoding::Plain, Compression::None),
    ("encoded, none", Encoding::Auto, Compression::None),
    ("encoded, lz4_raw", Encoding::Auto, Compression::Lz4),
    ("encoded, zstd 1", Encoding::Auto, Compression::Zstd { level: 1 }),
    ("encoded, zstd 3", Encoding::Auto, Compression::Zstd { level: 3 }),
];

/// The least of `runs` timings of `f`, in seconds.
fn best(runs: usize, mut f: impl FnMut()) -> f64 {
    (0..runs)
        .map(|_| {
            let start = Instant::now();
            f();
            start.elapsed().as_secs_f64()
        })
        .fold(f64::MAX, f64::min)
}

#[derive(Default, Clone, Copy)]
struct Sums {
    bytes: usize,
    write_s: f64,
    read_s: f64,
}

#[test]
#[ignore = "a measurement: BLAZE_PARQUET_MEASURE names the files, on a release build"]
fn measure_the_profiles_tables() {
    let dir = std::env::var("BLAZE_PARQUET_MEASURE").expect("BLAZE_PARQUET_MEASURE names a directory of profile files");
    let mut paths: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|it| it.unwrap().path()).filter(|it| it.extension().is_some_and(|e| e == "parquet")).collect();
    paths.sort();
    // table -> (old bytes, files, rows, per config sums)
    let mut tables: BTreeMap<String, (usize, usize, i64, [Sums; CONFIGS.len()])> = BTreeMap::new();
    let mut refused = vec![];
    for path in &paths {
        let name = path.file_name().unwrap().to_str().unwrap();
        let table = name.split('.').nth(1).unwrap().to_owned();
        let old = std::fs::read(path).unwrap();
        let file = match File::open(&old, Global) {
            Ok(it) => it,
            Err(error) => {
                refused.push(format!("{name}: {error}"));
                continue;
            }
        };
        let batches = match file.read_all() {
            Ok(it) => it,
            Err(error) => {
                refused.push(format!("{name}: {error}"));
                continue;
            }
        };
        let fields: Vec<Field<Global>> = file.fields().to_vec();
        let batches: &[Batch<Global>] = &batches;
        let entry = tables.entry(table).or_insert((0, 0, 0, [Sums::default(); CONFIGS.len()]));
        entry.0 += old.len();
        entry.1 += 1;
        entry.2 += file.num_rows();
        for (at, (_, encoding, compression)) in CONFIGS.iter().enumerate() {
            let options = Options { encoding: *encoding, compression: *compression, ..Options::DEFAULT };
            let mut bytes = vec![];
            let write_s = best(3, || bytes = write(&fields, batches, &[], options, Global).unwrap().as_slice().to_vec());
            let read_s = best(3, || {
                let file = File::open(&bytes, Global).unwrap();
                std::hint::black_box(file.read_all().unwrap());
            });
            let sums = &mut entry.3[at];
            sums.bytes += bytes.len();
            sums.write_s += write_s;
            sums.read_s += read_s;
        }
    }
    let mb = |bytes: usize| f64::from(u32::try_from(bytes).unwrap()) / 1e6;
    println!("refused: {}", refused.len());
    for line in &refused {
        println!("  {line}");
    }
    print!("| table | files | rows | before (arrow-rs, zstd) |");
    for (name, ..) in CONFIGS {
        print!(" {name} |");
    }
    println!();
    let mut total = (0usize, 0usize, 0i64, [Sums::default(); CONFIGS.len()]);
    for (table, (old, files, rows, sums)) in &tables {
        print!("| {table} | {files} | {rows} | {:.2} |", mb(*old));
        for sum in sums {
            print!(" {:.2} |", mb(sum.bytes));
        }
        println!();
        total.0 += old;
        total.1 += files;
        total.2 += rows;
        for (at, sum) in sums.iter().enumerate() {
            total.3[at].bytes += sum.bytes;
            total.3[at].write_s += sum.write_s;
            total.3[at].read_s += sum.read_s;
        }
    }
    print!("| all | {} | {} | {:.2} |", total.1, total.2, mb(total.0));
    for sum in &total.3 {
        print!(" {:.2} |", mb(sum.bytes));
    }
    println!();
    // Speeds over the data's PLAIN, uncompressed bytes: one measure of work for every codec.
    let data = total.3[0].bytes;
    println!("\nMB/s of the {:.1} MB the tables are as PLAIN, uncompressed:", mb(data));
    println!("| codec | bytes (MB) | write (s) | write MB/s | full scan (s) | read MB/s |");
    for (at, (name, ..)) in CONFIGS.iter().enumerate() {
        let sum = total.3[at];
        println!(
            "| {name} | {:.2} | {:.3} | {:.0} | {:.3} | {:.0} |",
            mb(sum.bytes),
            sum.write_s,
            mb(data) / sum.write_s,
            sum.read_s,
            mb(data) / sum.read_s
        );
    }
}
