//! The `perf_runs` and `perf_functions` rows ([`crate::schema::PERF_RUNS`],
//! [`crate::schema::PERF_FUNCTIONS`]), written into the history's directory and read back.
//!
//! A run is two files, `perf-<run>.perf_functions.parquet` and then
//! `perf-<run>.perf_runs.parquet`: the runs file is written last, so a run whose runs file is
//! there is whole, and a reader lists runs by it alone. The history's cap never deletes them,
//! since their names are not a flush's (`<build>-<seq>`). Each run row carries [`FORMAT`]; a
//! reader refuses rows of another format by name, never reads them as this one's.

use std::{
    alloc::Global,
    path::{Path, PathBuf},
};

use blaze_columns::{Prim, Strs};
use serde_derive::Serialize;

use crate::{
    columns::{self, Batch, Column},
    history, schema,
};

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

/// The `perf_runs` batch of `rows`.
pub fn runs_batch(rows: &[RunRow]) -> Result<Batch, String> {
    // alloc: a batch's column, once a run written
    let text = |it: fn(&RunRow) -> &str| Column::texts_of(rows.iter().map(it), Global);
    // alloc: a batch's column, once a run written
    let opt = |it: fn(&RunRow) -> Option<&str>| Column::opt_texts_of(rows.iter().map(it), Global);
    // alloc: a batch's columns, once a run written
    let columns: Vec<Column> = vec![
        text(|it| &it.run),
        Column::values(rows.iter().map(|it| it.format), Global),
        text(|it| &it.target),
        Column::values(rows.iter().map(|it| it.started_unix_ns), Global),
        Column::values(rows.iter().map(|it| it.reps), Global),
        opt(|it| it.tree.as_deref()),
        text(|it| &it.blaze),
        opt(|it| it.binary.as_deref()),
        opt(|it| it.build_id.as_deref()),
        text(|it| &it.host),
        text(|it| &it.arch),
        text(|it| &it.os),
        Column::options(rows.iter().map(|it| it.cpu_period_ns), Global),
        text(|it| &it.sampler),
        Column::values(rows.iter().map(|it| it.samples), Global),
        Column::values(rows.iter().map(|it| it.unmapped), Global),
        Column::values(rows.iter().map(|it| it.wall_ns), Global),
        opt(|it| it.maps.as_deref()),
        opt(|it| it.gaps.as_deref()),
    ];
    // alloc: an error
    columns::batch(&schema::PERF_RUNS, columns).map_err(|error| error.to_string())
}

/// The `perf_functions` batch of `rows`.
pub fn functions_batch(rows: &[FnRow]) -> Result<Batch, String> {
    // alloc: a batch's column, once a run written
    let text = |it: fn(&FnRow) -> &str| Column::texts_of(rows.iter().map(it), Global);
    // alloc: a batch's column, once a run written
    let opt = |it: fn(&FnRow) -> Option<&str>| Column::opt_texts_of(rows.iter().map(it), Global);
    // alloc: a batch's column, once a run written
    let count = |it: fn(&FnRow) -> Option<u64>| Column::options(rows.iter().map(it), Global);
    // alloc: a batch's columns, once a run written
    let columns: Vec<Column> = vec![
        text(|it| &it.run),
        Column::values(rows.iter().map(|it| it.rep), Global),
        text(|it| &it.function),
        opt(|it| it.module.as_deref()),
        opt(|it| it.identity.as_deref()),
        opt(|it| it.code.as_deref()),
        opt(|it| it.key.as_deref()),
        opt(|it| it.deep.as_deref()),
        Column::values(rows.iter().map(|it| it.self_samples), Global),
        Column::values(rows.iter().map(|it| it.total_samples), Global),
        count(|it| it.calls),
        count(|it| it.instructions),
        opt(|it| it.gap.as_deref()),
    ];
    // alloc: an error
    columns::batch(&schema::PERF_FUNCTIONS, columns).map_err(|error| error.to_string())
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

fn batches_of(path: &Path) -> Result<Vec<Batch>, String> {
    // alloc: a file's bytes, once a read
    let bytes = std::fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let file = blaze_parquet::File::open(&bytes, Global)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    let batches = file.read_all().map_err(|error| format!("{}: {error}", path.display()))?;
    // alloc: a file's batches, once a read
    Ok(batches.into_iter().collect())
}

/// The columns of one batch read back, each by name, with errors naming the file.
struct Columns<'a> {
    batch: &'a Batch,
    path: &'a Path,
}

impl<'a> Columns<'a> {
    fn get(&self, name: &str) -> Result<&'a Column, String> {
        // alloc: an error
        self.batch
            .column_by_name(name)
            .ok_or_else(|| format!("{}: no column `{name}`", self.path.display()))
    }

    fn text(&self, name: &str) -> Result<Strs<'a, Global>, String> {
        let it = self.get(name)?;
        // alloc: an error
        it.strs().ok_or_else(|| format!("{}: `{name}` is {:?}, not text", self.path.display(), it.ty()))
    }

    fn u32(&self, name: &str) -> Result<Prim<'a, u32>, String> {
        let it = self.get(name)?;
        // alloc: an error
        it.u32s().ok_or_else(|| format!("{}: `{name}` is {:?}, not u32", self.path.display(), it.ty()))
    }

    fn u64(&self, name: &str) -> Result<Prim<'a, u64>, String> {
        let it = self.get(name)?;
        // alloc: an error
        it.u64s().ok_or_else(|| format!("{}: `{name}` is {:?}, not u64", self.path.display(), it.ty()))
    }

    fn i64(&self, name: &str) -> Result<Prim<'a, i64>, String> {
        let it = self.get(name)?;
        // alloc: an error
        it.i64s().ok_or_else(|| format!("{}: `{name}` is {:?}, not i64", self.path.display(), it.ty()))
    }

    /// A cell no row may leave empty: an empty one is a damaged file, named.
    fn present<T>(&self, cell: Option<T>, name: &str, row: usize) -> Result<T, String> {
        // alloc: an error
        cell.ok_or_else(|| format!("{}: row {row} has no `{name}`", self.path.display()))
    }

    fn string(&self, name: &str, row: usize) -> Result<String, String> {
        let it = self.text(name)?;
        // alloc: a row read back
        Ok(self.present(it.get(row), name, row)?.to_owned())
    }

    fn opt_string(&self, name: &str, row: usize) -> Result<Option<String>, String> {
        // alloc: a row read back
        Ok(self.text(name)?.get(row).map(str::to_owned))
    }

    fn opt_u64(&self, name: &str, row: usize) -> Result<Option<u64>, String> {
        Ok(self.u64(name)?.get(row))
    }

    fn req_u64(&self, name: &str, row: usize) -> Result<u64, String> {
        self.present(self.u64(name)?.get(row), name, row)
    }

    fn req_u32(&self, name: &str, row: usize) -> Result<u32, String> {
        self.present(self.u32(name)?.get(row), name, row)
    }

    fn req_i64(&self, name: &str, row: usize) -> Result<i64, String> {
        self.present(self.i64(name)?.get(row), name, row)
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
