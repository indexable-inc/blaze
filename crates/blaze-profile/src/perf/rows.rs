//! The `perf_runs` and `perf_functions` rows ([`crate::schema::PERF_RUNS`],
//! [`crate::schema::PERF_FUNCTIONS`]), written into the history's directory and read back.
//!
//! A run is two files, `perf-<run>.perf_functions.parquet` and then
//! `perf-<run>.perf_runs.parquet`: the runs file is written last, so a run whose runs file is
//! there is whole, and a reader lists runs by it alone. The history's cap never deletes them,
//! since their names are not a flush's (`<build>-<seq>`). Each run row carries [`FORMAT`]; a
//! reader refuses rows of another format by name, never reads them as this one's.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use arrow_array::{
    Array, ArrayRef, Int64Array, RecordBatch, StringArray, UInt32Array, UInt64Array,
    cast::AsArray,
    types::{Int64Type, UInt32Type, UInt64Type},
};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use serde_derive::Serialize;

use crate::{batches, history, schema};

/// The perf rows' format. Bumped when a column changes meaning; a reader refuses another.
pub const FORMAT: u32 = 1;

/// One profiled run: `perf_runs`' columns, as docs/profiling.md and the schema describe them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RunRow {
    pub run: String,
    pub format: u32,
    pub target: String,
    pub started_unix_ns: i64,
    pub reps: u32,
    pub tree: Option<String>,
    pub blaze: String,
    pub binary: Option<String>,
    pub build_id: Option<String>,
    pub host: String,
    pub arch: String,
    pub os: String,
    pub cpu_period_ns: Option<u64>,
    pub sampler: String,
    pub samples: u64,
    pub unmapped: u64,
    pub wall_ns: i64,
    pub maps: Option<String>,
    pub gaps: Option<String>,
}

/// One function in one repetition of a run: `perf_functions`' columns.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FnRow {
    pub run: String,
    pub rep: u32,
    pub function: String,
    pub module: Option<String>,
    pub identity: Option<String>,
    pub code: Option<String>,
    pub key: Option<String>,
    pub deep: Option<String>,
    pub self_samples: u64,
    pub total_samples: u64,
    pub calls: Option<u64>,
    pub instructions: Option<u64>,
    pub gap: Option<String>,
}

/// The run's file of its run row in the history `dir`.
pub fn runs_file(dir: &Path, run: &str) -> PathBuf {
    // alloc: a file's name, once a run
    dir.join(format!("perf-{run}.{}.parquet", schema::PERF_RUNS.name))
}

/// The run's file of its function rows in the history `dir`.
pub fn functions_file(dir: &Path, run: &str) -> PathBuf {
    // alloc: a file's name, once a run
    dir.join(format!("perf-{run}.{}.parquet", schema::PERF_FUNCTIONS.name))
}

/// Whether `run` is a run's name as [`super::record`] makes them: 16 hex digits, so it is
/// safe in a file's name.
pub fn is_run(run: &str) -> bool {
    run.len() == 16 && run.bytes().all(|it| matches!(it, b'0'..=b'9' | b'a'..=b'f'))
}

fn column(array: impl Array + 'static) -> ArrayRef {
    // alloc: a batch's column, once a run written
    Arc::new(array)
}

/// The `perf_runs` batch of `rows`.
pub fn runs_batch(rows: &[RunRow]) -> Result<RecordBatch, String> {
    let text = |it: fn(&RunRow) -> &str| column(StringArray::from_iter_values(rows.iter().map(it)));
    let opt = |it: fn(&RunRow) -> Option<&str>| column(StringArray::from_iter(rows.iter().map(it)));
    // alloc: a batch's columns, once a run written
    let columns: Vec<ArrayRef> = vec![
        text(|it| &it.run),
        column(UInt32Array::from_iter_values(rows.iter().map(|it| it.format))),
        text(|it| &it.target),
        column(Int64Array::from_iter_values(rows.iter().map(|it| it.started_unix_ns))),
        column(UInt32Array::from_iter_values(rows.iter().map(|it| it.reps))),
        opt(|it| it.tree.as_deref()),
        text(|it| &it.blaze),
        opt(|it| it.binary.as_deref()),
        opt(|it| it.build_id.as_deref()),
        text(|it| &it.host),
        text(|it| &it.arch),
        text(|it| &it.os),
        column(UInt64Array::from_iter(rows.iter().map(|it| it.cpu_period_ns))),
        text(|it| &it.sampler),
        column(UInt64Array::from_iter_values(rows.iter().map(|it| it.samples))),
        column(UInt64Array::from_iter_values(rows.iter().map(|it| it.unmapped))),
        column(Int64Array::from_iter_values(rows.iter().map(|it| it.wall_ns))),
        opt(|it| it.maps.as_deref()),
        opt(|it| it.gaps.as_deref()),
    ];
    RecordBatch::try_new(batches::schema_of(&schema::PERF_RUNS), columns)
        .map_err(|error| format!("the {} batch: {error}", schema::PERF_RUNS.name))
}

/// The `perf_functions` batch of `rows`.
pub fn functions_batch(rows: &[FnRow]) -> Result<RecordBatch, String> {
    let text = |it: fn(&FnRow) -> &str| column(StringArray::from_iter_values(rows.iter().map(it)));
    let opt = |it: fn(&FnRow) -> Option<&str>| column(StringArray::from_iter(rows.iter().map(it)));
    let count = |it: fn(&FnRow) -> Option<u64>| column(UInt64Array::from_iter(rows.iter().map(it)));
    // alloc: a batch's columns, once a run written
    let columns: Vec<ArrayRef> = vec![
        text(|it| &it.run),
        column(UInt32Array::from_iter_values(rows.iter().map(|it| it.rep))),
        text(|it| &it.function),
        opt(|it| it.module.as_deref()),
        opt(|it| it.identity.as_deref()),
        opt(|it| it.code.as_deref()),
        opt(|it| it.key.as_deref()),
        opt(|it| it.deep.as_deref()),
        column(UInt64Array::from_iter_values(rows.iter().map(|it| it.self_samples))),
        column(UInt64Array::from_iter_values(rows.iter().map(|it| it.total_samples))),
        count(|it| it.calls),
        count(|it| it.instructions),
        opt(|it| it.gap.as_deref()),
    ];
    RecordBatch::try_new(batches::schema_of(&schema::PERF_FUNCTIONS), columns)
        .map_err(|error| format!("the {} batch: {error}", schema::PERF_FUNCTIONS.name))
}

/// Writes the run `run` and its `functions` into the history `dir`, both files listed in its
/// manifest in one commit ([`history::write_listed`]): a reader sees the run with its functions
/// or neither. A run already there is refused: the same profiles recorded twice.
pub fn write(dir: &Path, run: &RunRow, functions: &[FnRow]) -> Result<(), String> {
    if !is_run(&run.run) {
        // alloc: an error
        return Err(format!("{:?} is not a run's name (16 hex digits)", run.run));
    }
    let runs = runs_file(dir, &run.run);
    if runs.try_exists().map_err(|error| format!("{}: {error}", runs.display()))? {
        // alloc: an error
        return Err(format!(
            "run {} is already recorded ({}): these profiles were recorded before",
            run.run,
            runs.display()
        ));
    }
    let file = functions_file(dir, &run.run);
    let function_rows = [functions_batch(functions)?];
    let run_rows = [runs_batch(std::slice::from_ref(run))?];
    history::write_listed(
        dir,
        &[(&file, &schema::PERF_FUNCTIONS, &function_rows), (&runs, &schema::PERF_RUNS, &run_rows)],
    )
    .map_err(|error| format!("writing run {} into {}: {error}", run.run, dir.display()))?;
    Ok(())
}

/// Every run recorded in the history `dir`, by when it began, then by name.
pub fn runs(dir: &Path) -> Result<Vec<RunRow>, String> {
    match dir.try_exists() {
        // alloc: no runs
        Ok(false) => return Ok(Vec::new()),
        Ok(true) => {}
        Err(error) => return Err(format!("{}: {error}", dir.display())),
    }
    let manifest = history::read_manifest(dir).map_err(|error| error.to_string())?;
    // alloc: once a listing, the runs files
    let mut files: Vec<std::path::PathBuf> = manifest
        .files(schema::PERF_RUNS.name)
        .filter(|name| {
            name.strip_prefix("perf-")
                .and_then(|it| it.split_once('.'))
                .is_some_and(|(run, _)| is_run(run))
        })
        .map(|name| dir.join(name))
        .collect();
    files.sort_unstable();
    // alloc: once a listing
    let mut out = Vec::with_capacity(files.len());
    for file in files {
        out.extend(read_runs(&file)?);
    }
    out.sort_by(|a, b| (a.started_unix_ns, &a.run).cmp(&(b.started_unix_ns, &b.run)));
    Ok(out)
}

/// The function rows of `run` in the history `dir`.
pub fn functions(dir: &Path, run: &str) -> Result<Vec<FnRow>, String> {
    if !is_run(run) {
        // alloc: an error
        return Err(format!("{run:?} is not a run's name (16 hex digits)"));
    }
    read_functions(&functions_file(dir, run))
}

fn batches_of(path: &Path) -> Result<Vec<RecordBatch>, String> {
    let file = std::fs::File::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let reader = ParquetRecordBatchReaderBuilder::try_new(file)
        .and_then(|it| it.build())
        .map_err(|error| format!("{}: {error}", path.display()))?;
    reader.collect::<Result<_, _>>().map_err(|error| format!("{}: {error}", path.display()))
}

/// The columns of one batch read back, each by name, with errors naming the file.
struct Columns<'a> {
    batch: &'a RecordBatch,
    path: &'a Path,
}

impl<'a> Columns<'a> {
    fn get(&self, name: &str) -> Result<&'a ArrayRef, String> {
        // alloc: an error
        self.batch
            .column_by_name(name)
            .ok_or_else(|| format!("{}: no column `{name}`", self.path.display()))
    }

    fn text(&self, name: &str) -> Result<&'a StringArray, String> {
        let it = self.get(name)?;
        // alloc: an error
        it.as_string_opt::<i32>().ok_or_else(|| {
            format!("{}: `{name}` is {}, not text", self.path.display(), it.data_type())
        })
    }

    fn u32(&self, name: &str) -> Result<&'a UInt32Array, String> {
        let it = self.get(name)?;
        // alloc: an error
        it.as_primitive_opt::<UInt32Type>().ok_or_else(|| {
            format!("{}: `{name}` is {}, not u32", self.path.display(), it.data_type())
        })
    }

    fn u64(&self, name: &str) -> Result<&'a UInt64Array, String> {
        let it = self.get(name)?;
        // alloc: an error
        it.as_primitive_opt::<UInt64Type>().ok_or_else(|| {
            format!("{}: `{name}` is {}, not u64", self.path.display(), it.data_type())
        })
    }

    fn i64(&self, name: &str) -> Result<&'a Int64Array, String> {
        let it = self.get(name)?;
        // alloc: an error
        it.as_primitive_opt::<Int64Type>().ok_or_else(|| {
            format!("{}: `{name}` is {}, not i64", self.path.display(), it.data_type())
        })
    }

    /// A column no row may leave empty: an empty one is a damaged file, named.
    fn present(&self, array: &dyn Array, name: &str, row: usize) -> Result<(), String> {
        if array.is_null(row) {
            // alloc: an error
            return Err(format!("{}: row {row} has no `{name}`", self.path.display()));
        }
        Ok(())
    }

    fn string(&self, name: &str, row: usize) -> Result<String, String> {
        let it = self.text(name)?;
        self.present(it, name, row)?;
        // alloc: a row read back
        Ok(it.value(row).to_owned())
    }

    fn opt_string(&self, name: &str, row: usize) -> Result<Option<String>, String> {
        let it = self.text(name)?;
        // alloc: a row read back
        Ok(it.is_valid(row).then(|| it.value(row).to_owned()))
    }

    fn opt_u64(&self, name: &str, row: usize) -> Result<Option<u64>, String> {
        let it = self.u64(name)?;
        Ok(it.is_valid(row).then(|| it.value(row)))
    }

    fn req_u64(&self, name: &str, row: usize) -> Result<u64, String> {
        let it = self.u64(name)?;
        self.present(it, name, row)?;
        Ok(it.value(row))
    }

    fn req_u32(&self, name: &str, row: usize) -> Result<u32, String> {
        let it = self.u32(name)?;
        self.present(it, name, row)?;
        Ok(it.value(row))
    }

    fn req_i64(&self, name: &str, row: usize) -> Result<i64, String> {
        let it = self.i64(name)?;
        self.present(it, name, row)?;
        Ok(it.value(row))
    }
}

/// The run rows of one runs file, each of [`FORMAT`] or refused.
pub fn read_runs(path: &Path) -> Result<Vec<RunRow>, String> {
    // alloc: a file read back
    let mut out = Vec::new();
    for batch in batches_of(path)? {
        let at = Columns { batch: &batch, path };
        for row in 0..batch.num_rows() {
            let run = at.string("run", row)?;
            let format = at.req_u32("format", row)?;
            if format != FORMAT {
                // alloc: an error
                return Err(format!(
                    "{}: run {run} is in perf format {format}, and this blaze reads format {FORMAT}; record it again \
                     with this blaze, or read it with the blaze that wrote it",
                    path.display()
                ));
            }
            out.push(RunRow {
                run,
                format,
                target: at.string("target", row)?,
                started_unix_ns: at.req_i64("started_unix_ns", row)?,
                reps: at.req_u32("reps", row)?,
                tree: at.opt_string("tree", row)?,
                blaze: at.string("blaze", row)?,
                binary: at.opt_string("binary", row)?,
                build_id: at.opt_string("build_id", row)?,
                host: at.string("host", row)?,
                arch: at.string("arch", row)?,
                os: at.string("os", row)?,
                cpu_period_ns: at.opt_u64("cpu_period_ns", row)?,
                sampler: at.string("sampler", row)?,
                samples: at.req_u64("samples", row)?,
                unmapped: at.req_u64("unmapped", row)?,
                wall_ns: at.req_i64("wall_ns", row)?,
                maps: at.opt_string("maps", row)?,
                gaps: at.opt_string("gaps", row)?,
            });
        }
    }
    Ok(out)
}

/// The function rows of one functions file.
pub fn read_functions(path: &Path) -> Result<Vec<FnRow>, String> {
    // alloc: a file read back
    let mut out = Vec::new();
    for batch in batches_of(path)? {
        let at = Columns { batch: &batch, path };
        for row in 0..batch.num_rows() {
            out.push(FnRow {
                run: at.string("run", row)?,
                rep: at.req_u32("rep", row)?,
                function: at.string("function", row)?,
                module: at.opt_string("module", row)?,
                identity: at.opt_string("identity", row)?,
                code: at.opt_string("code", row)?,
                key: at.opt_string("key", row)?,
                deep: at.opt_string("deep", row)?,
                self_samples: at.req_u64("self_samples", row)?,
                total_samples: at.req_u64("total_samples", row)?,
                calls: at.opt_u64("calls", row)?,
                instructions: at.opt_u64("instructions", row)?,
                gap: at.opt_string("gap", row)?,
            });
        }
    }
    Ok(out)
}
