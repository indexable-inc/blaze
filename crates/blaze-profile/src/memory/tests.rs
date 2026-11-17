use arrow_array::{Array, cast::AsArray, types};
use arrow_schema::DataType;

use super::*;

const G: f64 = GIB;

fn counter(build_id: Option<u64>, process: u32, name: &str, t_ns: i64, value: f64) -> Counter {
    Counter { build_id, process: Some(process), name: name.to_owned(), t_ns, value }
}

fn task(span_id: u64, function: Option<&str>, peak: Option<u64>) -> Span {
    Span {
        build_id: Some(1),
        process: Some(7),
        span_id: Some(span_id),
        name: "task".to_owned(),
        function: function.map(str::to_owned),
        peak_bytes: peak,
        dur_ns: 1_250_000_000,
        ..Span::default()
    }
}

fn sample(site: Option<&str>, tag: Option<&str>, bytes: u64, block_bytes: u64) -> Sample {
    Sample {
        build_id: Some(1),
        process: Some(7),
        t_ns: 0,
        bytes,
        tag: tag.map(str::to_owned),
        block_bytes,
        site: site.map(str::to_owned),
        read_ns: 0,
    }
}

/// The `limit` a part takes when none is given is the views' own.
#[test]
fn the_default_limit_is_the_views() {
    for part in Part::ALL {
        let view = crate::views::find(part.view()).unwrap();
        let limit = view.args.iter().find(|it| it.name == "limit");
        assert_eq!(limit.is_some(), part.limited(), "{}", part.view());
        if let Some(limit) = limit {
            assert_eq!(limit.default, Some(DEFAULT_LIMIT.to_string().as_str()), "{}", part.view());
        }
    }
}

/// The numbers are made as DuckDB makes them: `round` ties away from zero, a cast to `BIGINT`
/// ties to even, a share is null over nothing.
#[test]
fn the_numbers_round_as_duckdbs() {
    assert_eq!(round(0.125, 2), 0.13);
    assert_eq!(round(-0.125, 2), -0.13);
    assert_eq!((bigint(4.5), bigint(5.5), bigint(-4.5)), (4, 6, -4));
    assert_eq!(pct(1, 3), Some(33.3));
    assert_eq!(pct(2, 3), Some(66.7));
    assert_eq!(pct(1, 0), None);
}

/// Each part's rows, from rows in memory: the counters of no build stand in for a build only
/// where it has none of its own; a peak's tie goes to the earlier instant; tasks tie by name
/// with no name last; a site's tags are distinct and sorted, its mean block ties to even.
#[test]
fn each_part_is_its_views_aggregation() {
    let facts = Facts {
        builds: vec![
            Build { build_id: 1, started_ns: 0, dur_ns: Some(300_000_000_000) },
            Build { build_id: 2, started_ns: 400_000_000_000, dur_ns: Some(100_000_000_000) },
        ],
        counters: vec![
            counter(Some(1), 7, "footprint bytes", 0, 2.0 * G),
            counter(Some(1), 7, "footprint bytes", 60_000_000_000, 5.0 * G),
            counter(Some(1), 7, "footprint bytes", 120_000_000_000, 5.0 * G),
            counter(Some(1), 7, "pool running", 10_000_000_000, 4.5),
            counter(Some(1), 7, "tag live bytes: query", 0, 0.25 * G),
            counter(Some(1), 7, "tag live bytes: query", 60_000_000_000, 0.5 * G),
            counter(Some(1), 7, "tag heap allocated bytes: mir", 0, 1.0 * G),
            counter(Some(1), 7, "tag heap allocated bytes: mir", 50_000_000_000, 3.0 * G),
            counter(Some(1), 7, "tag heap allocated bytes: mir", 100_000_000_000, 9.0 * G),
            counter(None, 7, "footprint bytes", 410_000_000_000, 1.0 * G),
            counter(None, 7, "footprint bytes", 610_000_000_000, 8.0 * G),
        ],
        spans: vec![
            task(1, Some("lower"), Some(2 << 30)),
            task(2, Some("codegen"), Some(2 << 30)),
            task(3, None, Some(2 << 30)),
            task(4, Some("held"), None),
        ],
        alloc: vec![
            sample(Some("a"), Some("query"), 4 << 20, 10),
            sample(Some("a"), Some("mir"), 4 << 20, 11),
            sample(Some("a"), Some("query"), 4 << 20, 10),
            sample(Some("a"), Some("query"), 4 << 20, 11),
            sample(None, None, 8 << 30, 7),
        ],
    };

    let minutes = timeline(&facts, 1);
    assert_eq!(minutes.iter().map(|it| it.minute).collect::<Vec<_>>(), [0, 1, 2]);
    assert_eq!((minutes[0].footprint, minutes[0].running), (Some(2.0 * G), Some(4.5)));
    assert_eq!(timeline_batch(&minutes).unwrap()[0].num_rows(), 3);
    // Build 2 has no counter of its own: those of no build in its window, not after it.
    let two = timeline(&facts, 2);
    assert_eq!(two.len(), 1);
    assert_eq!(two[0].footprint, Some(1.0 * G));
    assert!(timeline(&facts, 3).is_empty(), "no build, no window, no row");

    let tasks: Vec<_> = peak_tasks(&facts, 1, 20).iter().map(|it| it.task).collect();
    assert_eq!(tasks, [Some("codegen"), Some("lower"), None]);
    assert_eq!(peak_tasks(&facts, 1, 1).len(), 1);

    let areas = at_peak(&facts, 1);
    let tags: Vec<_> = areas.iter().map(|it| (it.area.as_str(), it.process)).collect();
    assert_eq!(tags, [("query", false), ("mir", false), ("footprint", true)]);
    let mir = &areas[1];
    assert_eq!(mir.allocated, Some(2.0), "allocated from the first counter to the peak: {mir:?}");
    assert_eq!((areas[0].held, areas[2].held), (Some(0.5), Some(5.0)));

    let sites = sites(&facts.alloc, 20);
    let named: Vec<_> = sites.iter().map(|it| (it.site, it.tags.as_str(), it.samples)).collect();
    assert_eq!(named, [("(in the allocator)", "outside", 1), ("a", "mir, query", 4)]);
    assert_eq!(sites[1].mean_block_bytes, 10, "10.5 ties to even");
    assert_eq!(sites[0].gib, 8.0);
}

// --- Each part against its view, on one fixture -----------------------------------------

/// A cell of an answer, as both readers give it.
#[derive(Debug, PartialEq)]
enum Cell {
    Null,
    Int(i128),
    /// Its bits: equal means equal to the last bit.
    Float(u64),
    Text(String),
}

/// An answer's column names and rows.
fn cells_of(batches: &[RecordBatch]) -> (Vec<String>, Vec<Vec<Cell>>) {
    let Some(first) = batches.first() else { return (Vec::new(), Vec::new()) };
    let names = first.schema().fields().iter().map(|it| it.name().to_owned()).collect();
    let mut rows = Vec::new();
    for batch in batches {
        for row in 0..batch.num_rows() {
            rows.push(batch.columns().iter().map(|column| cell(column.as_ref(), row)).collect());
        }
    }
    (names, rows)
}

fn cell(column: &dyn Array, row: usize) -> Cell {
    if column.is_null(row) {
        return Cell::Null;
    }
    match column.data_type() {
        DataType::Int32 => Cell::Int(column.as_primitive::<types::Int32Type>().value(row).into()),
        DataType::Int64 => Cell::Int(column.as_primitive::<types::Int64Type>().value(row).into()),
        DataType::UInt64 => Cell::Int(column.as_primitive::<types::UInt64Type>().value(row).into()),
        DataType::Float64 => {
            Cell::Float(column.as_primitive::<types::Float64Type>().value(row).to_bits())
        }
        DataType::Utf8 => Cell::Text(column.as_string::<i32>().value(row).to_owned()),
        DataType::LargeUtf8 => Cell::Text(column.as_string::<i64>().value(row).to_owned()),
        DataType::Utf8View => Cell::Text(column.as_string_view().value(row).to_owned()),
        other => panic!("an answer's column of type {other}"),
    }
}

/// The fixture: build 1 with its own counters (two processes, a peak tied in value, derived
/// `kept`, tags), tasks and their unit phases, give-backs of the build and of no build in its
/// window, allocation samples with a tie in GiB; build 2 with only counters of no build in its
/// window; two readings of the held heap.
const FIXTURE: &str = "\
COPY (SELECT * FROM (VALUES (1::UBIGINT, 0::BIGINT, 300000000000::BIGINT, 'ok', '/work', 'build'), \
    (2, 400000000000, 100000000000, 'ok', '/work', 'build'), (2, 400000000000, NULL, 'running', '/work', 'build')) \
    AS t(build_id, started_ns, dur_ns, outcome, workspace, command)) TO '{d}/1-00000001.builds.parquet';
COPY (SELECT * FROM (VALUES \
    (1::UBIGINT, 7::UINTEGER, 'footprint bytes', 0::BIGINT, 2147483648.0::DOUBLE), \
    (1, 7, 'footprint bytes', 60000000000, 5368709120.0), \
    (1, 7, 'footprint bytes', 120000000000, 5368709120.0), \
    (1, 7, 'footprint bytes', 200000000000, 3221225472.0), \
    (1, 8, 'footprint bytes', 10000000000, 1073741824.0), \
    (1, 7, 'heap live bytes', 0, 1073741824.0), \
    (1, 7, 'heap live bytes', 60000000000, 2147483648.0), \
    (1, 7, 'live bytes', 30000000000, 536870912.0), \
    (1, 7, 'pool running', 10000000000, 4.5), \
    (1, 7, 'pool running', 70000000000, 3.0), \
    (1, 7, 'blaze-mem resident bytes', 60000000000, 1073741824.0), \
    (1, 7, 'blaze-mem resident bytes', 130000000000, 1610612736.0), \
    (1, 7, 'tag live bytes: query', 0, 104857600.0), \
    (1, 7, 'tag live bytes: query', 60000000000, 314572800.0), \
    (1, 7, 'tag heap bytes: query', 60000000000, 209715200.0), \
    (1, 7, 'tag heap allocated bytes: mir', 0, 1073741824.0), \
    (1, 7, 'tag heap allocated bytes: mir', 50000000000, 3221225472.0), \
    (1, 7, 'tag heap allocated bytes: mir', 100000000000, 9663676416.0), \
    (1, 7, 'tag heap freed bytes: mir', 0, 0.0), \
    (1, 7, 'tag heap freed bytes: mir', 60000000000, 1073741824.0), \
    (1, 7, 'tag odd bytes: codegen', 60000000000, 1.0), \
    (1, 7, 'heap committed bytes', 60000000000, 2684354560.0), \
    (1, 7, 'chunks warm bytes', 200000000000, 1073741824.0), \
    (NULL, 7, 'footprint bytes', 410000000000, 1073741824.0), \
    (NULL, 7, 'footprint bytes', 470000000000, 2147483648.0), \
    (NULL, 7, 'allocator kept bytes', 470000000000, 268435456.0), \
    (NULL, 7, 'footprint bytes', 600000000000, 8589934592.0)) \
    AS t(build_id, process, name, t_ns, value)) TO '{d}/1-00000001.counters.parquet';
COPY (SELECT * FROM (VALUES \
    (1::UBIGINT, 7::UINTEGER, 1::UINTEGER, 2::UBIGINT, NULL::UBIGINT, 0::USMALLINT, 'task', 1000000000::BIGINT, 25000000000::BIGINT, \
        'codegen', 'tokio', 'lib', 2147483648::UBIGINT, 3221225472::UBIGINT, 5368709120::UBIGINT, 4294967296::UBIGINT, NULL::VARCHAR), \
    (1, 7, 1, 1, NULL, 0, 'task', 2000000000, 30000000000, 'lower', NULL, NULL, 2147483648, NULL, NULL, NULL, NULL), \
    (1, 7, 1, 10, 1, 1, 'unit bodies', 2000000000, 1000000000, NULL, 'serde', 'lib', NULL, NULL, NULL, NULL, NULL), \
    (1, 7, 1, 11, 1, 1, 'unit codegen', 3000000000, 1000000000, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL), \
    (1, 7, 1, 3, NULL, 0, 'task', 4000000000, 1050000000, NULL, NULL, NULL, 1073741824, NULL, NULL, NULL, NULL), \
    (1, 7, 1, 4, NULL, 0, 'task', 5000000000, 1000000000, 'resolve', NULL, NULL, 0, NULL, 5, NULL, NULL), \
    (1, 7, 1, 5, NULL, 0, 'task', 6000000000, 1000000000, 'none', NULL, NULL, NULL, NULL, NULL, NULL, NULL), \
    (1, 7, 1, 6, NULL, 0, 'unit names', 6000000000, 1000000000, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL), \
    (1, 7, 1, 20, NULL, 0, 'memory purge', 100000000000, 1250000, NULL, NULL, NULL, NULL, NULL, NULL, NULL, 'freed=1'), \
    (NULL, 7, 1, 21, NULL, 0, 'memory give-back', 50000000000, 2000000, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL), \
    (NULL, 7, 1, 22, NULL, 0, 'memory give-back plan', 300000000000, 2000000, NULL, NULL, NULL, NULL, NULL, NULL, NULL, 'late'), \
    (2, 7, 1, 23, NULL, 0, 'kept front ends let go for memory', 450000000000, 3000000, NULL, NULL, NULL, NULL, NULL, NULL, NULL, 'what=front end')) \
    AS t(build_id, process, thread, span_id, parent, depth, name, start_ns, dur_ns, function, crate, unit, \
        peak_bytes, granted_bytes, heap_allocated_bytes, heap_freed_bytes, fields)) TO '{d}/1-00000001.spans.parquet';
COPY (SELECT * FROM (VALUES \
    (1::UBIGINT, 7::UINTEGER, 1::UINTEGER, 0::BIGINT, 4194304::UBIGINT, 'query', 10::UBIGINT, 'a'), \
    (1, 7, 1, 1, 4194304, 'mir', 11, 'a'), \
    (1, 7, 1, 2, 8388608, NULL, 7, NULL), \
    (1, 7, 1, 3, 8388608, 'query', 1, 'b'), \
    (2, 7, 1, 450000000000, 1073741824, 'query', 1, 'c')) \
    AS t(build_id, process, thread, t_ns, bytes, tag, block_bytes, site)) TO '{d}/1-00000001.alloc_samples.parquet';
COPY (SELECT * FROM (VALUES \
    (1::UBIGINT, 7::UINTEGER, 1::UINTEGER, 10000000000::BIGINT, 1073741824::UBIGINT, 'query', 100::UBIGINT, 'x', 50000000000::BIGINT), \
    (NULL, 7, 1, 20000000000, 1073741824, NULL, 101, 'x', 50000000000), \
    (1, 8, 1, 40000000000, 3221225472, 'mir', 7, 'y', 50000000000), \
    (1, 7, 1, 10000000000, 9663676416, 'query', 9, 'z', 130000000000)) \
    AS t(build_id, process, thread, t_ns, bytes, tag, block_bytes, site, read_ns)) TO '{d}/1-00000001.held_samples.parquet';
";

/// The invariant: each part of `blaze explain memory`, answered without DuckDB, is its view's
/// answer, cell for cell (to the last bit of a double) and column for column, for a build with
/// its own counters and one with only counters of no build, with a limit and without.
#[test]
#[ignore = "needs the pinned DuckDB: BLAZE_DUCKDB=<an absolute path to DuckDB 1.5.5>"]
fn each_part_answered_without_duckdb_is_its_views_answer() {
    let duckdb = std::path::PathBuf::from(
        std::env::var_os("BLAZE_DUCKDB").expect("BLAZE_DUCKDB names the pinned DuckDB CLI"),
    );
    let tmp = blaze_core::temp::TempDir::for_test("blaze-memory-");
    let dir = tmp.path().to_path_buf();
    crate::history::History::open(&dir, crate::history::CAP_MOST, crate::sampler::unsampled())
        .unwrap();
    let fixture = FIXTURE.replace("{d}", &dir.display().to_string());
    let written = std::process::Command::new(&duckdb)
        .args(["-batch", "-bail", "-init", "/dev/null", "-c", &fixture])
        .output()
        .unwrap();
    assert!(written.status.success(), "{}", String::from_utf8_lossy(&written.stderr));
    crate::history::list_written(&dir);

    assert_eq!(last_build(&dir).unwrap(), Some(2));
    assert_eq!(last_with_memory(&dir).unwrap(), Some(2), "build 2's alloc sample");

    let mut nonempty = 0;
    for (build, limit) in [(1, None), (1, Some(1)), (2, None), (3, None)] {
        let native = answer(&dir, build, limit.unwrap_or(DEFAULT_LIMIT)).unwrap();
        let mut sqls = Vec::new();
        for part in Part::ALL {
            let view = crate::views::find(part.view()).unwrap();
            let mut args = serde_json::Map::new();
            args.insert("build".to_owned(), build.into());
            if let Some(limit) = limit.filter(|_| part.limited()) {
                args.insert("limit".to_owned(), limit.into());
            }
            sqls.push(crate::views::render(view, &args).unwrap());
        }
        let sqls: Vec<&str> = sqls.iter().map(String::as_str).collect();
        let viewed = crate::sql::run_all(&duckdb, &dir, Scope::All, &sqls).unwrap();
        for ((part, ours), theirs) in native.iter().zip(&viewed) {
            let (ours, theirs) = (cells_of(ours), cells_of(theirs));
            let at = format!("{} of build {build}, limit {limit:?}", part.view());
            assert_eq!(ours.1, theirs.1, "{at}: the rows");
            if !theirs.1.is_empty() {
                assert_eq!(ours.0, theirs.0, "{at}: the columns");
                nonempty += 1;
            }
        }
    }
    assert!(nonempty >= 12, "the fixture reaches every part of build 1 twice: {nonempty}");
}
