use std::path::{Path, PathBuf};

use ix_prof::{
    Profile,
    profile::{CpuSample, Frame, FrameKind, Interner, Meta, Thread},
};

use super::{
    Id,
    diff::{self, Cause, Verdict},
    map::{self, Map, Node},
    record::{self, Inputs},
    rows::{self, FnRow, RunRow},
};

const PROGRAM: &str = "app";
const IMAGE: &str = "/nowhere/app";
const PERIOD: u64 = 1_000_000;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("blaze-perf-{}-{name}", std::process::id()));
    _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn id(n: u8) -> Id {
    [n; 32]
}

/// A function of a code map: its name, identity, code (its key is its code with the same
/// settings), address and callees.
fn node(name: &str, identity: u8, code: u8, address: u64, edges: &[u32]) -> Node {
    Node {
        address,
        size: 0x100,
        identity: Some(id(identity)),
        key: id(code.wrapping_add(100)),
        code: id(code),
        name: name.to_owned(),
        edges: edges.to_vec(),
    }
}

/// A link's code map, written into `perf` under `build`.
fn link(perf: &Path, build: &str, nodes: Vec<Node>) {
    map::write(perf, build, &Map { image: IMAGE.to_owned(), nodes }).unwrap();
}

/// A repetition's profile: `stacks` are addresses outermost first, each sampled `count` times.
fn profile(build: &str, start_unix_ns: i64, stacks: &[(&[u64], u64)]) -> Vec<u8> {
    let mut interner = Interner::default();
    let mut samples = Vec::new();
    let mut t_ns = 0;
    for (addresses, count) in stacks {
        let frames: Vec<u32> = addresses
            .iter()
            .map(|address| {
                let mut frame = Frame::named(format!("sym_{address:x}"), FrameKind::Native);
                frame.module = Some(IMAGE.to_owned());
                frame.build_id = Some(build.to_owned());
                frame.address = Some(*address);
                interner.frame(frame)
            })
            .collect();
        let stack = interner.stack(frames).unwrap();
        for _ in 0..*count {
            t_ns += 1;
            samples.push(CpuSample {
                process: 1,
                thread: 1,
                t_ns,
                stack,
                span_id: None,
                on_cpu: true,
                weight_ns: PERIOD,
            });
        }
    }
    let profile = Profile {
        meta: Meta {
            program: PROGRAM.to_owned(),
            producer: "perf tests".to_owned(),
            clock: "test".to_owned(),
            start_ns: 0,
            end_ns: 1_000_000_000,
            start_unix_ns,
            arch: "x86_64".to_owned(),
            os: "linux".to_owned(),
            cpu_period_ns: Some(PERIOD),
            cpu_sampler: "test".to_owned(),
            ..Meta::default()
        },
        threads: vec![Thread {
            process: 1,
            process_name: PROGRAM.to_owned(),
            thread: 1,
            thread_name: None,
            cpu_ns: None,
        }],
        frames: interner.frame_rows,
        stacks: interner.stack_rows,
        cpu_samples: samples,
        ..Profile::default()
    };
    profile.to_ixprof().unwrap()
}

fn inputs(perf: &Path) -> Inputs {
    Inputs {
        perf: perf.to_owned(),
        tree: Ok("7".repeat(64)),
        blaze: "b".repeat(64),
        host: "test-host".to_owned(),
    }
}

const BUILD_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const BUILD_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

/// Two reps of a run of build `build`, `main` at `main` calling `work` at `work`: `main` runs
/// `own` samples itself and `work` 50.
fn run(
    perf: &Path,
    build: &str,
    start: i64,
    main: u64,
    work: u64,
    own: [u64; 2],
) -> (RunRow, Vec<FnRow>) {
    let reps: Vec<Vec<u8>> = own
        .iter()
        .enumerate()
        .map(|(at, own)| {
            profile(build, start + at as i64, &[(&[main + 4, work + 8], 50), (&[main + 4], *own)])
        })
        .collect();
    record::record(&reps, &inputs(perf)).unwrap()
}

#[test]
fn a_function_whose_code_did_not_change_lines_up_across_two_builds() {
    let dir = scratch("lines-up");
    let perf = dir.join("perf");
    // The same code at other addresses, under another build id.
    link(
        &perf,
        BUILD_A,
        vec![node("app::main", 1, 1, 0x1000, &[1]), node("app::work", 2, 2, 0x1100, &[])],
    );
    link(
        &perf,
        BUILD_B,
        vec![node("app::main", 1, 1, 0x3000, &[1]), node("app::work", 2, 2, 0x2000, &[])],
    );
    let a = run(&perf, BUILD_A, 1_000, 0x1000, 0x1100, [10, 12]);
    let b = run(&perf, BUILD_B, 2_000, 0x3000, 0x2000, [11, 10]);
    assert_eq!(a.0.unmapped, 0, "every sample is in a mapped function: {:?}", a.0.gaps);
    assert_eq!(b.0.unmapped, 0, "every sample is in a mapped function: {:?}", b.0.gaps);
    assert_eq!(
        run(&perf, BUILD_A, 1_000, 0x1000, 0x1100, [10, 12]),
        a,
        "the same profiles record the same rows"
    );

    let work = |rows: &[FnRow]| rows.iter().find(|it| it.function == "app::work").cloned().unwrap();
    let (wa, wb) = (work(&a.1), work(&b.1));
    assert_eq!(
        (&wa.identity, &wa.code, &wa.key, &wa.deep),
        (&wb.identity, &wb.code, &wb.key, &wb.deep)
    );

    let diff = diff::compare((&b.0, &b.1), (&a.0, &a.1)).unwrap();
    assert_eq!(diff.rows.len(), 2, "one row a function, joined by identity: {:#?}", diff.rows);
    for row in &diff.rows {
        assert_eq!(row.cause, Cause::SameCodeAndCallees, "{}", row.function);
        assert!(
            row.self_ns.base > 0.0 && row.self_ns.run > 0.0,
            "{} has time on both sides",
            row.function
        );
    }
}

#[test]
fn a_changed_function_is_classed_its_code_changed() {
    let dir = scratch("code-changed");
    let perf = dir.join("perf");
    link(
        &perf,
        BUILD_A,
        vec![node("app::main", 1, 1, 0x1000, &[1]), node("app::work", 2, 2, 0x1100, &[])],
    );
    // `work`'s code changed; `main`'s did not, but it calls `work`.
    link(
        &perf,
        BUILD_B,
        vec![node("app::main", 1, 1, 0x1000, &[1]), node("app::work", 2, 3, 0x1100, &[])],
    );
    let a = run(&perf, BUILD_A, 1_000, 0x1000, 0x1100, [10, 11]);
    let b = run(&perf, BUILD_B, 2_000, 0x1000, 0x1100, [300, 310]);
    let diff = diff::compare((&b.0, &b.1), (&a.0, &a.1)).unwrap();
    let row = |name: &str| diff.rows.iter().find(|it| it.function == name).unwrap();
    assert_eq!(row("app::work").cause, Cause::CodeChanged);
    let main = row("app::main");
    assert_eq!(main.cause, Cause::CalleeChanged);
    assert_eq!(main.verdict, Verdict::Slower);
    assert_eq!(main.changed_callees, ["app::work"], "{:?}", main.callee_note);
    assert_eq!(diff.rows[0].function, "app::main", "the change beyond noise comes first");
}

fn run_row(run: &str, reps: u32) -> RunRow {
    RunRow {
        run: run.to_owned(),
        format: rows::FORMAT,
        target: PROGRAM.to_owned(),
        started_unix_ns: 0,
        reps,
        tree: None,
        blaze: "b".repeat(64),
        binary: None,
        build_id: None,
        host: "test-host".to_owned(),
        arch: "x86_64".to_owned(),
        os: "linux".to_owned(),
        cpu_period_ns: Some(PERIOD),
        sampler: "test".to_owned(),
        samples: 0,
        unmapped: 0,
        wall_ns: 0,
        maps: None,
        gaps: None,
    }
}

fn fn_rows(run: &str, function: &str, identity: u8, self_samples: &[u64]) -> Vec<FnRow> {
    self_samples
        .iter()
        .enumerate()
        .map(|(rep, samples)| FnRow {
            run: run.to_owned(),
            rep: rep as u32,
            function: function.to_owned(),
            module: Some(IMAGE.to_owned()),
            identity: Some(super::hex(&id(identity))),
            code: Some("c".repeat(64)),
            key: Some("k".repeat(64)),
            deep: Some("d".repeat(64)),
            self_samples: *samples,
            total_samples: *samples,
            calls: None,
            instructions: None,
            gap: None,
        })
        .collect()
}

#[test]
fn a_difference_inside_the_noise_bound_is_reported_as_noise() {
    let (base, run) = ("000000000000000a", "000000000000000b");
    let mut base_rows = fn_rows(base, "app::steady", 1, &[100, 104, 98]);
    base_rows.extend(fn_rows(base, "app::doubled", 2, &[100, 100, 100]));
    let mut run_rows = fn_rows(run, "app::steady", 1, &[101, 99, 103]);
    run_rows.extend(fn_rows(run, "app::doubled", 2, &[200, 200, 200]));
    let diff =
        diff::compare((&run_row(run, 3), &run_rows), (&run_row(base, 3), &base_rows)).unwrap();
    let row = |name: &str| diff.rows.iter().find(|it| it.function == name).unwrap();
    let steady = row("app::steady");
    assert_eq!(steady.verdict, Verdict::WithinNoise, "{:?}", steady.self_ns);
    assert!(steady.self_ns.within_noise && steady.self_ns.delta.abs() <= steady.self_ns.bound);
    assert_eq!(row("app::doubled").verdict, Verdict::Slower);
    assert_eq!((diff.summary.slower, diff.summary.faster, diff.summary.within_noise), (1, 0, 1));
    assert!(!diff::render(&diff, false).contains("app::steady"), "noise is not listed as a change");

    // One repetition a side: the bound is sampling noise alone, and the notes say so.
    let one = diff::compare(
        (&run_row(run, 1), &fn_rows(run, "app::steady", 1, &[105])),
        (&run_row(base, 1), &fn_rows(base, "app::steady", 1, &[100])),
    )
    .unwrap();
    assert_eq!(one.rows[0].verdict, Verdict::WithinNoise);
    assert!(one.notes.iter().any(|it| it.contains("sampling noise only")), "{:?}", one.notes);
}

#[test]
fn perf_rows_round_trip_through_parquet() {
    let dir = scratch("round-trip");
    let perf = dir.join("perf");
    let history = dir.join("history");
    std::fs::create_dir_all(&history).unwrap();
    let map = Map {
        image: IMAGE.to_owned(),
        nodes: vec![
            node("app::main", 1, 1, 0x1000, &[1, 2]),
            node("app::work", 2, 2, 0x1100, &[1]),
            Node {
                address: 0,
                size: 0,
                identity: None,
                key: id(9),
                code: id(9),
                name: "a static".to_owned(),
                edges: vec![],
            },
        ],
    };
    assert_eq!(Map::decode(&map.encode()).unwrap(), map, "a code map reads back as written");
    map::write(&perf, BUILD_A, &map).unwrap();

    let (row, functions) = run(&perf, BUILD_A, 1_000, 0x1000, 0x1100, [10, 12]);
    rows::write(&history, &row, &functions).unwrap();
    assert_eq!(rows::runs(&history).unwrap(), [row.clone()]);
    assert_eq!(rows::functions(&history, &row.run).unwrap(), functions);
    assert!(rows::write(&history, &row, &functions).unwrap_err().contains("already recorded"));

    // Rows of another format are refused by name, never read as this one's.
    let mut other = run_row("00000000000000ff", 1);
    other.format = rows::FORMAT + 1;
    rows::write(&history, &other, &[]).unwrap();
    let error = rows::runs(&history).unwrap_err();
    assert!(error.contains(&format!("perf format {}", rows::FORMAT + 1)), "{error}");
}
