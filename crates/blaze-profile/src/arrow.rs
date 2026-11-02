//! The Arrow schema of each [`crate::schema`] table: what the Parquet files, the live feed's
//! IPC batches and the explorer's results are typed by.

use std::sync::Arc;

use arrow_schema::{DataType, Field, Schema};

use crate::schema::{self, Column, Table, Type};

/// Arrow's type for a column of `ty`.
pub fn data_type(ty: Type) -> DataType {
    // alloc: a schema, made once a table per process
    let dict = || DataType::Dictionary(Box::new(DataType::UInt32), Box::new(DataType::Utf8));
    match ty {
        Type::U16 => DataType::UInt16,
        Type::U32 => DataType::UInt32,
        Type::U64 => DataType::UInt64,
        Type::I64 => DataType::Int64,
        Type::F64 => DataType::Float64,
        Type::Utf8 => DataType::Utf8,
        Type::Dict => dict(),
        // alloc: a schema, made once a table per process
        Type::Stack => DataType::List(Arc::new(Field::new("frame", dict(), false))),
    }
}

pub fn field(column: &Column) -> Field {
    // alloc: a schema, made once a table per process
    Field::new(column.name, data_type(column.ty), column.nullable)
        .with_metadata([("doc", column.doc)])
}

/// The table's Arrow schema, with the schema's version and the clock in its metadata.
pub fn schema(table: &Table) -> Schema {
    // alloc: a schema, made once a table per process
    let fields: Vec<Field> = table.columns.iter().map(field).collect();
    // alloc: a schema, made once a table per process
    let version = schema::VERSION.to_string();
    let meta = [
        ("blaze.table", table.name),
        ("blaze.schema", version.as_str()),
        ("blaze.clock", schema::CLOCK),
    ];
    Schema::new(fields).with_metadata(meta)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_table_has_an_arrow_schema_with_its_columns_in_order() {
        for table in schema::TABLES.iter().chain(&schema::RESULTS).copied() {
            let arrow = schema(table);
            let names: Vec<&str> = arrow.fields().iter().map(|it| it.name().as_str()).collect();
            let want: Vec<&str> = table.columns.iter().map(|it| it.name).collect();
            assert_eq!(names, want);
            assert_eq!(arrow.metadata()["blaze.schema"], schema::VERSION.to_string());
        }
    }
}
