//! DuckDB reads what blaze-parquet writes, and blaze-parquet reads DuckDB's uncompressed
//! answers: when `BLAZE_DUCKDB` names a DuckDB binary. Without it each test says why it did not
//! run and passes, as the profile's other DuckDB tests do.

#![feature(allocator_api)]

use std::{alloc::Global, path::PathBuf, process::Command};

use blaze_columns::{Batch, Column, Field, Scalar, Texts, Type, Values};
use blaze_own::Vec;
use blaze_parquet::{Compression, Encoding, File, Options, write};

fn duckdb() -> Option<PathBuf> {
    match std::env::var_os("BLAZE_DUCKDB") {
        Some(it) if !it.is_empty() => Some(PathBuf::from(it)),
        _ => {
            eprintln!("skipped: BLAZE_DUCKDB names no DuckDB binary");
            None
        }
    }
}

/// A file of this test's own, under the system's temporary directory.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("blaze-parquet-duckdb-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

/// `sql`'s rows as DuckDB prints them: `|` between columns, `NULL` for a null, no header.
fn query(duckdb: &PathBuf, sql: &str) -> String {
    let out = Command::new(duckdb)
        .args(["-batch", "-list", "-noheader", "-separator", "|", "-nullvalue", "NULL", "-c", sql])
        .output()
        .unwrap();
    assert!(out.status.success(), "duckdb failed on {sql}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

fn texts(values: &[&str]) -> Texts<Global> {
    let mut out = Texts::new_in(Global);
    for value in values {
        out.push(value);
    }
    out
}

fn batch() -> (std::vec::Vec<Field<Global>>, Batch<Global>) {
    let fields = vec![
        Field::new("flag", Type::One(Scalar::Bool), false),
        Field::new("small", Type::One(Scalar::I32), true),
        Field::new("t_ns", Type::One(Scalar::I64), false),
        Field::new("depth", Type::One(Scalar::U16), false),
        Field::new("pid", Type::One(Scalar::U32), true),
        Field::new("build_id", Type::One(Scalar::U64), true),
        Field::new("ratio", Type::One(Scalar::F64), true),
        Field::new("name", Type::One(Scalar::Text), true),
        Field::new("kind", Type::One(Scalar::Dict), false),
        Field::new("stack", Type::List(Scalar::Text), true),
        Field::new("pcs", Type::List(Scalar::U64), false),
    ];
    let columns = [
        Column::values([true, false, true], Global),
        Column::options([Some(-1), None, Some(i32::MAX)], Global),
        Column::values([i64::MIN, 0, 5], Global),
        Column::values([0u16, 7, u16::MAX], Global),
        Column::options([Some(u32::MAX), None, Some(1)], Global),
        Column::options([Some(u64::MAX), Some(1), None], Global),
        Column::options([Some(1.5), Some(-0.25), None], Global),
        Column::opt_texts(&[Some("x"), None, Some("y z")], Global),
        Column::dict([Some(1), Some(0), Some(1)], texts(&["a", "bb"]), Global).unwrap(),
        Column::list(
            Vec::from_slice_in(&[0, 2, 2, 2], Global),
            Some(Vec::from_slice_in(&[true, true, false], Global)),
            Values::Text(texts(&["main", "f"])),
        )
        .unwrap(),
        Column::list(
            Vec::from_slice_in(&[0, 1, 1, 3], Global),
            None,
            Values::U64(Vec::from_slice_in(&[1, u64::MAX, 7], Global)),
        )
        .unwrap(),
    ];
    let batch = Batch::try_new(Vec::from_iter_in(fields.iter().cloned(), Global), Vec::from_iter_in(columns, Global)).unwrap();
    (fields, batch)
}

#[test]
fn duckdb_reads_every_type_and_null() {
    let Some(duckdb) = duckdb() else { return };
    let (fields, batch) = batch();
    // Two row groups and a page a row: every boundary is crossed.
    let options = Options { row_group_rows: 2, page_rows: 1, ..Options::DEFAULT };
    let bytes = write(&fields, &[batch], &[("blaze.schema", "5")], options, Global).unwrap();
    let path = scratch("every-type.parquet");
    std::fs::write(&path, bytes.as_slice()).unwrap();
    let path = path.display();
    let types = query(
        &duckdb,
        &format!("SELECT column_name, column_type, \"null\" FROM (DESCRIBE SELECT * FROM read_parquet('{path}'))"),
    );
    assert_eq!(
        types,
        "flag|BOOLEAN|YES\nsmall|INTEGER|YES\nt_ns|BIGINT|YES\ndepth|USMALLINT|YES\npid|UINTEGER|YES\n\
         build_id|UBIGINT|YES\nratio|DOUBLE|YES\nname|VARCHAR|YES\nkind|VARCHAR|YES\nstack|VARCHAR[]|YES\n\
         pcs|UBIGINT[]|YES\n",
    );
    let rows = query(&duckdb, &format!("SELECT * FROM read_parquet('{path}')"));
    assert_eq!(
        rows,
        "true|-1|-9223372036854775808|0|4294967295|18446744073709551615|1.5|x|bb|[main, f]|[1]\n\
         false|NULL|0|7|NULL|1|-0.25|NULL|a|[]|[]\n\
         true|2147483647|5|65535|1|NULL|NULL|y z|bb|NULL|[18446744073709551615, 7]\n",
    );
    let meta = query(
        &duckdb,
        &format!("SELECT decode(value) FROM parquet_kv_metadata('{path}') WHERE decode(key) = 'blaze.schema'"),
    );
    assert_eq!(meta, "5\n");
    // DuckDB prunes by the statistics: they must say the unsigned order.
    let pruned = query(&duckdb, &format!("SELECT count(*) FROM read_parquet('{path}') WHERE build_id > 2"));
    assert_eq!(pruned, "1\n");
}

const ENCODINGS: [Encoding; 5] = [Encoding::Auto, Encoding::Plain, Encoding::Dictionary, Encoding::Delta, Encoding::DeltaLength];
const CODECS: [Compression; 3] = [Compression::None, Compression::Lz4, Compression::Zstd { level: 3 }];

/// `n` rows of every type with lists and nulls, as a profile's (rising times, few names, paths
/// sharing prefixes): enough for many pages, delta blocks and a dictionary.
fn many(n: usize) -> Batch<Global> {
    let (fields, _) = batch();
    let mut stack = Texts::new_in(Global);
    let mut stack_offsets = Vec::from_slice_in(&[0u32], Global);
    let mut pcs = Vec::new_in(Global);
    let mut pc_offsets = Vec::from_slice_in(&[0u32], Global);
    for i in 0..n {
        for depth in 0..i % 4 {
            stack.push(&format!("crates/blaze-front/src/f{}.rs", (i + depth) % 9));
        }
        stack_offsets.push(u32::try_from(stack.len()).unwrap());
        for depth in 0..i % 3 {
            pcs.push(0x1_0000_0000u64 + u64::try_from(i * 16 + depth).unwrap());
        }
        pc_offsets.push(u32::try_from(pcs.len()).unwrap());
    }
    let names: std::vec::Vec<String> = (0..n).map(|i| format!("blaze_query::item_{}", (i * 7) % 40)).collect();
    let names: std::vec::Vec<Option<&str>> = names.iter().enumerate().map(|(i, it)| (i % 13 != 0).then_some(it.as_str())).collect();
    let columns = [
        Column::values((0..n).map(|i| i % 3 == 0 || i % 50 > 40), Global),
        Column::options((0..n).map(|i| (i % 5 != 0).then(|| i32::try_from(i * 7919 % 1000).unwrap() - 500)), Global),
        Column::values((0..n).map(|i| 1_700_000_000_000_000_000i64 + i64::try_from(i * 1_000 + i % 13).unwrap()), Global),
        Column::values((0..n).map(|i| u16::try_from(i % 7).unwrap()), Global),
        Column::options((0..n).map(|i| (i % 9 != 0).then_some(4242u32)), Global),
        Column::options((0..n).map(|i| (i % 17 != 0).then(|| u64::MAX - u64::try_from(i / 100).unwrap())), Global),
        Column::options((0..n).map(|i| (i % 6 != 0).then(|| f64::from(u32::try_from(i % 300).unwrap()) / 8.0)), Global),
        Column::opt_texts(&names, Global),
        Column::dict((0..n).map(|i| Some(u32::try_from(i % 3).unwrap())), texts(&["span", "event", "counter"]), Global).unwrap(),
        Column::list(stack_offsets, Some(Vec::from_iter_in((0..n).map(|i| i % 11 != 3), Global)), Values::Text(stack)).unwrap(),
        Column::list(pc_offsets, None, Values::U64(pcs)).unwrap(),
    ];
    Batch::try_new(Vec::from_iter_in(fields, Global), Vec::from_iter_in(columns, Global)).unwrap()
}

#[test]
fn duckdb_reads_every_encoding_and_codec() {
    let Some(duckdb) = duckdb() else { return };
    let (fields, _) = batch();
    let small_rows = "true|-1|-9223372036854775808|0|4294967295|18446744073709551615|1.5|x|bb|[main, f]|[1]\n\
         false|NULL|0|7|NULL|1|-0.25|NULL|a|[]|[]\n\
         true|2147483647|5|65535|1|NULL|NULL|y z|bb|NULL|[18446744073709551615, 7]\n";
    // The PLAIN, uncompressed file the others are held to (itself held to the rows above by
    // `duckdb_reads_every_type_and_null`).
    let plain = Options { row_group_rows: 2000, page_rows: 700, encoding: Encoding::Plain, ..Options::DEFAULT };
    let base = scratch("many-plain.parquet");
    std::fs::write(&base, write(&fields, &[many(5000)], &[], plain, Global).unwrap().as_slice()).unwrap();
    let base = base.display();
    for encoding in ENCODINGS {
        for compression in CODECS {
            let tag = format!("{encoding:?}-{compression:?}").replace([' ', '{', '}', ':'], "");
            let options = Options { row_group_rows: 2, page_rows: 1, encoding, compression, ..Options::DEFAULT };
            let path = scratch(&format!("small-{tag}.parquet"));
            std::fs::write(&path, write(&fields, &[batch().1], &[], options, Global).unwrap().as_slice()).unwrap();
            let rows = query(&duckdb, &format!("SELECT * FROM read_parquet('{}')", path.display()));
            assert_eq!(rows, small_rows, "{tag}");
            let options = Options { row_group_rows: 2000, page_rows: 700, encoding, compression, ..Options::DEFAULT };
            let path = scratch(&format!("many-{tag}.parquet"));
            std::fs::write(&path, write(&fields, &[many(5000)], &[], options, Global).unwrap().as_slice()).unwrap();
            let path = path.display();
            let differ = query(
                &duckdb,
                &format!(
                    "SELECT (SELECT count(*) FROM read_parquet('{path}')), \
                     (SELECT count(*) FROM (SELECT * FROM read_parquet('{path}') EXCEPT ALL SELECT * FROM read_parquet('{base}'))), \
                     (SELECT count(*) FROM (SELECT * FROM read_parquet('{base}') EXCEPT ALL SELECT * FROM read_parquet('{path}')))"
                ),
            );
            assert_eq!(differ, "5000|0|0\n", "{tag}");
        }
    }
}

#[test]
fn duckdb_answers_of_every_codec_and_page_version_read_back() {
    let Some(duckdb) = duckdb() else { return };
    for codec in ["uncompressed", "lz4_raw", "zstd"] {
        for version in ["v1", "v2"] {
            let path = scratch(&format!("answer-{codec}-{version}.parquet"));
            let sql = format!(
                "COPY (SELECT i::BIGINT AS i, (i % 3)::INTEGER AS small, 'k' || (i % 5) AS kind, 'path/to/' || i AS path, \
                 i % 2 = 0 AS even, CASE WHEN i % 7 = 1 THEN NULL ELSE [i::BIGINT, 7] END AS pair \
                 FROM range(3000) t(i)) TO '{}' (FORMAT parquet, COMPRESSION {codec}, PARQUET_VERSION {version})",
                path.display()
            );
            query(&duckdb, &sql);
            let bytes = std::fs::read(&path).unwrap();
            let file = File::open(&bytes, Global).unwrap();
            let mut got = vec![];
            for batch in file.read_all().unwrap().iter() {
                for row in 0..batch.num_rows() {
                    let pair = batch.column(5).lists().unwrap().slice::<i64>(row).map(<[i64]>::to_vec);
                    got.push((
                        batch.column(0).int(row),
                        batch.column(1).int(row),
                        batch.column(2).text(row).map(str::to_owned),
                        batch.column(3).text(row).map(str::to_owned),
                        matches!(batch.column(4).buffer(), Values::Bool(it) if it[row]),
                        pair,
                    ));
                }
            }
            let want: std::vec::Vec<_> = (0..3000i64)
                .map(|i| {
                    (
                        Some(i128::from(i)),
                        Some(i128::from(i % 3)),
                        Some(format!("k{}", i % 5)),
                        Some(format!("path/to/{i}")),
                        i % 2 == 0,
                        (i % 7 != 1).then(|| vec![i, 7]),
                    )
                })
                .collect();
            assert_eq!(got, want, "{codec} {version}");
        }
    }
}

#[test]
fn duckdb_reads_an_empty_file() {
    let Some(duckdb) = duckdb() else { return };
    let (fields, _) = batch();
    let bytes = write(&fields, &[], &[], Options::DEFAULT, Global).unwrap();
    let path = scratch("empty.parquet");
    std::fs::write(&path, bytes.as_slice()).unwrap();
    let rows = query(&duckdb, &format!("SELECT count(*), count(stack) FROM read_parquet('{}')", path.display()));
    assert_eq!(rows, "0|0\n");
}

#[test]
fn an_uncompressed_duckdb_answer_reads_back() {
    let Some(duckdb) = duckdb() else { return };
    let path = scratch("answer.parquet");
    let sql = format!(
        "COPY (SELECT i::BIGINT AS i, (i % 3)::UTINYINT AS small, 'k' || (i % 2) AS kind, (i * 1.5)::DECIMAL(9, 2) AS dec, \
         sum(i) OVER () AS total, [i::BIGINT, 7] AS pair, CASE WHEN i = 1 THEN NULL ELSE 'v' END AS maybe \
         FROM range(4) t(i)) TO '{}' (FORMAT parquet, COMPRESSION uncompressed)",
        path.display()
    );
    query(&duckdb, &sql);
    let bytes = std::fs::read(&path).unwrap();
    let file = File::open(&bytes, Global).unwrap();
    let names: std::vec::Vec<_> = file.fields().iter().map(|it| (it.name().to_owned(), it.ty)).collect();
    assert_eq!(
        names,
        [
            ("i".to_owned(), Type::One(Scalar::I64)),
            ("small".to_owned(), Type::One(Scalar::U16)),
            ("kind".to_owned(), Type::One(Scalar::Text)),
            ("dec".to_owned(), Type::One(Scalar::F64)),
            ("total".to_owned(), Type::One(Scalar::F64)),
            ("pair".to_owned(), Type::List(Scalar::I64)),
            ("maybe".to_owned(), Type::One(Scalar::Text)),
        ]
    );
    let mut got = vec![];
    for batch in file.read_all().unwrap().iter() {
        for row in 0..batch.num_rows() {
            let pair = batch.column(5).lists().unwrap().slice::<i64>(row).unwrap().to_vec();
            got.push((
                batch.column(0).int(row),
                batch.column(1).int(row),
                batch.column(2).text(row).map(str::to_owned),
                batch.column(3).float(row),
                batch.column(4).float(row),
                pair,
                batch.column(6).text(row).map(str::to_owned),
            ));
        }
    }
    let want: std::vec::Vec<_> = (0..4i64)
        .map(|i| {
            (
                Some(i128::from(i)),
                Some(i128::from(i % 3)),
                Some(format!("k{}", i % 2)),
                Some(f64::from(u8::try_from(i).unwrap()) * 1.5),
                Some(6.0),
                vec![i, 7],
                (i != 1).then(|| "v".to_owned()),
            )
        })
        .collect();
    assert_eq!(got, want);
}
