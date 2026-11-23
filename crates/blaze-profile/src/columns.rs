//! Each [`crate::schema`] table as blaze-columns fields: what the Parquet files
//! (blaze-parquet), the live feed's and the explorer's Arrow IPC (blaze-arrow-ipc) and every
//! reader of them are typed by. In place of the Arrow schema (arrow.rs until 2026-11-15).
//!
//! | schema | column | Parquet | Arrow IPC |
//! |---|---|---|---|
//! | `U16`, `U32`, `U64`, `I64`, `F64`, `Bool` | the same | INT32/INT64 (unsigned annotated), DOUBLE, BOOLEAN | the same |
//! | `Utf8` | `Text` | BYTE_ARRAY, STRING | Utf8 |
//! | `Dict` | `Dict` | BYTE_ARRAY, STRING (plain) | Dictionary<UInt32, Utf8> |
//! | `Stack` | `List(Dict)` | LIST of required STRING | List<Dictionary<UInt32, Utf8>> |
//! | `Pcs` | `List(U64)` | LIST of required UINT_64 | List<UInt64> |

use std::{alloc::Global, fmt, sync::OnceLock};

use blaze_columns::{ColumnError, Field, Scalar};

use crate::schema::{self, Table, Type};

/// A batch of a table's rows.
pub type Batch = blaze_columns::Batch<Global>;

/// A column of a batch.
pub type Column = blaze_columns::Column<Global>;

/// The column type of a schema column of `ty`.
pub const fn column_type(ty: Type) -> blaze_columns::Type {
    use blaze_columns::Type::{List, One};
    match ty {
        Type::U16 => One(Scalar::U16),
        Type::U32 => One(Scalar::U32),
        Type::U64 => One(Scalar::U64),
        Type::I64 => One(Scalar::I64),
        Type::F64 => One(Scalar::F64),
        Type::Bool => One(Scalar::Bool),
        Type::Utf8 => One(Scalar::Text),
        Type::Dict => One(Scalar::Dict),
        Type::Stack => List(Scalar::Dict),
        Type::Pcs => List(Scalar::U64),
    }
}

/// The field of a schema column.
pub const fn field(column: &schema::Column) -> Field<Global> {
    Field::new(column.name, column_type(column.ty), column.nullable)
}

/// The fields of `table`, in its order, made once a process.
pub fn fields(table: &'static Table) -> &'static [Field<Global>] {
    const ALL: usize = schema::TABLES.len() + schema::RESULTS.len();
    static FIELDS: [OnceLock<blaze_own::Vec<Field<Global>, Global>>; ALL] = [const { OnceLock::new() }; ALL];
    let at = schema::TABLES
        .iter()
        .chain(&schema::RESULTS)
        .position(|it| it.name == table.name)
        .unwrap_or_else(|| unreachable!("{} is a table of the schema", table.name));
    FIELDS[at].get_or_init(|| blaze_own::Vec::from_iter_in(table.columns.iter().map(field), Global))
}

/// The schema's version as text: `blaze.schema`'s value.
pub fn version() -> &'static str {
    static VERSION: OnceLock<String> = OnceLock::new();
    VERSION.get_or_init(|| schema::VERSION.to_string())
}

/// The key-value metadata every file and stream of `table` carries: its name, the schema's
/// version and the clock its times are on.
pub fn metadata(table: &'static Table) -> [(&'static str, &'static str); 3] {
    [("blaze.table", table.name), ("blaze.schema", version()), ("blaze.clock", schema::CLOCK)]
}

/// A batch of `table` of `columns`, in its order.
pub fn batch(table: &'static Table, columns: impl IntoIterator<Item = Column>) -> Result<Batch, BatchError> {
    let fields = blaze_own::Vec::from_iter_in(fields(table).iter().cloned(), Global);
    let columns = blaze_own::Vec::from_iter_in(columns, Global);
    Batch::try_new(fields, columns).map_err(BatchError::of(table))
}

/// A batch of `table` that could not be made, or a column of it read as the schema types it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BatchError {
    pub table: &'static str,
    pub error: ColumnError,
}

impl BatchError {
    /// The error of `table` a column error is.
    pub fn of(table: &'static Table) -> impl Fn(ColumnError) -> BatchError + Copy {
        move |error| BatchError { table: table.name, error }
    }
}

impl fmt::Display for BatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "a `{}` batch: {}", self.table, self.error)
    }
}

impl std::error::Error for BatchError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_table_has_fields_of_its_columns_in_order() {
        for table in schema::TABLES.iter().chain(&schema::RESULTS).copied() {
            let names: Vec<&str> = fields(table).iter().map(Field::name).collect();
            let want: Vec<&str> = table.columns.iter().map(|it| it.name).collect();
            assert_eq!(names, want);
        }
        assert_eq!(metadata(&schema::SPANS)[1], ("blaze.schema", version()));
    }

    /// Three rows of `table`: row 0 small values, row 1 null wherever the column may be, row
    /// 2 large ones; text says its column and row.
    fn three_rows(table: &'static Table) -> Batch {
        use blaze_columns::{Texts, Values};
        let column = |column: &schema::Column| -> Column {
            let null = |row: usize| column.nullable && row == 1;
            let text = |row: usize| format!("{} {row}", column.name);
            match column.ty {
                Type::U16 => Column::options([0u16, 1, 2].map(|row| (!null(row.into())).then_some(row * 30_000)), Global),
                Type::U32 => Column::options([0u32, 1, 2].map(|row| (row != 1 || !column.nullable).then_some(row * 2_000_000_000)), Global),
                Type::U64 => Column::options([0u64, 1, 2].map(|row| (row != 1 || !column.nullable).then_some(row << 62)), Global),
                Type::I64 => Column::options([0i64, 1, 2].map(|row| (row != 1 || !column.nullable).then_some((row - 1) << 62)), Global),
                Type::F64 => Column::options([0u8, 1, 2].map(|row| (!null(row.into())).then_some(f64::from(row) - 0.5)), Global),
                Type::Bool => Column::options((0..3).map(|row| (!null(row)).then_some(row == 2)), Global),
                Type::Utf8 => Column::opt_texts(&(0..3).map(|row| (!null(row)).then(|| text(row))).collect::<Vec<_>>(), Global),
                Type::Dict => {
                    let mut values = Texts::new_in(Global);
                    values.push(&text(0));
                    values.push(&text(2));
                    Column::dict([Some(0), (!null(1)).then_some(0), Some(1)], values, Global).unwrap()
                }
                Type::Stack | Type::Pcs => {
                    let offsets = blaze_own::Vec::from_slice_in(&[0, 2, 2, 3], Global);
                    let valid = column.nullable.then(|| blaze_own::Vec::from_slice_in(&[true, false, true], Global));
                    let values = match column.ty {
                        Type::Stack => {
                            let mut frames = Texts::new_in(Global);
                            for frame in ["main", "f", "g"] {
                                frames.push(frame);
                            }
                            Values::Dict { keys: blaze_own::Vec::from_slice_in(&[0, 1, 2], Global), values: frames }
                        }
                        _ => Values::U64(blaze_own::Vec::from_slice_in(&[0x10, 0x20, u64::MAX], Global)),
                    };
                    Column::list(offsets, valid, values).unwrap()
                }
            }
        };
        batch(table, table.columns.iter().map(column)).unwrap()
    }

    /// Each table's empty file and three-row file are the bytes kept in tests/golden: a change
    /// to how a table is written shows here as bytes, and is blessed on purpose
    /// (`BLAZE_BLESS=1`).
    #[test]
    fn each_tables_files_are_their_golden_bytes() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden");
        let bless = std::env::var_os("BLAZE_BLESS").is_some_and(|it| it == "1");
        let mut differ = Vec::new();
        for table in schema::TABLES.iter().copied() {
            // Golden bytes pin a platform-independent clock name so Linux and macOS produce
            // identical Parquet bytes for the same schema and rows.
            let golden_meta = [("blaze.table", table.name), ("blaze.schema", version()), ("blaze.clock", "darwin CLOCK_UPTIME_RAW")];
            for (name, batches) in [("empty", Vec::new()), ("three", vec![three_rows(table)])] {
                let bytes = blaze_parquet::write(fields(table), &batches, &golden_meta, blaze_parquet::Options::DEFAULT, Global).unwrap();
                // What is written reads back as written.
                let file = blaze_parquet::File::open(&bytes, Global).unwrap();
                assert_eq!(file.num_rows(), i64::try_from(batches.iter().map(Batch::num_rows).sum::<usize>()).unwrap());
                let path = dir.join(format!("{}.{name}.parquet", table.name));
                if bless {
                    std::fs::create_dir_all(&dir).unwrap();
                    std::fs::write(&path, bytes.as_slice()).unwrap();
                } else if std::fs::read(&path).ok().as_deref() != Some(bytes.as_slice()) {
                    differ.push(path.display().to_string());
                }
            }
        }
        assert!(differ.is_empty(), "these files differ from what is written now (run with BLAZE_BLESS=1 to keep the new bytes): {differ:?}");
    }
}
