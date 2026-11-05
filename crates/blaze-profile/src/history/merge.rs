//! A merge's bytes: the rows of Parquet files of one schema as one file, their column chunks
//! copied as they are, with no decode and no re-encode, so row groups stay the size their flush
//! wrote. Pure: the inputs are bytes in memory (a mapping of each, [`Input::of`]) and the
//! output is handed out a piece at a time ([`Concat::next`]) for its writer to append. Who
//! reads and writes the files is its caller: the history's core ([`super::files`]) through
//! effects, the gate through its own files.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::sync::Arc;

use bytes::Bytes;
use parquet::{
    column::writer::ColumnCloseResult,
    file::{
        metadata::{KeyValue, PageIndexPolicy, ParquetMetaData, ParquetMetaDataReader},
        properties::WriterProperties,
        writer::SerializedFileWriter,
    },
    schema::types::TypePtr,
};

use super::{CompactError, MERGED_ROWS};

/// The bytes of a mapped file, shared without a copy as a Parquet reader's [`Bytes`].
struct Mapped(blaze_core::Mapping);

impl AsRef<[u8]> for Mapped {
    fn as_ref(&self) -> &[u8] {
        self.0.bytes()
    }
}

/// The bytes `mapping` maps, as a Parquet reader's [`Bytes`], with no copy.
pub(crate) fn bytes_of(mapping: blaze_core::Mapping) -> Bytes {
    Bytes::from_owner(Mapped(mapping))
}

/// One input of a merge: its bytes and its footer.
pub(crate) struct Input {
    bytes: Bytes,
    metadata: ParquetMetaData,
}

impl Input {
    /// The input whose bytes `mapping` maps; its footer read, or the error saying why not.
    pub(crate) fn of(mapping: blaze_core::Mapping) -> Result<Input, CompactError> {
        Input::of_bytes(bytes_of(mapping))
    }

    /// The input of `bytes`, a whole Parquet file.
    pub(crate) fn of_bytes(bytes: Bytes) -> Result<Input, CompactError> {
        let metadata = ParquetMetaDataReader::new()
            .with_page_index_policy(PageIndexPolicy::Optional)
            .parse_and_finish(&bytes)
            .map_err(CompactError::Parquet)?;
        Ok(Input { bytes, metadata })
    }

    pub(crate) fn metadata(&self) -> &ParquetMetaData {
        &self.metadata
    }
}

/// How many bytes of output [`Concat::next`] gathers before it hands them out: the writer
/// appends a piece this size, never the whole merge at once.
const PIECE: usize = 8 << 20;

/// A merge under way: the output's writer and where it is in the inputs.
pub(crate) struct Concat {
    writer: Option<SerializedFileWriter<Vec<u8>>>,
    schema: TypePtr,
    inputs: Vec<Input>,
    /// The input and its row group copied next.
    at: (usize, usize),
    /// The rows the inputs' footers said, so far.
    rows: i64,
}

/// Where a merge failed: the input at `input` (its position in the merge's inputs), or the
/// output when `None`, and the step.
#[derive(Debug)]
pub(crate) struct Failed {
    pub(crate) input: Option<usize>,
    pub(crate) doing: &'static str,
    pub(crate) error: CompactError,
}

fn of_output(doing: &'static str) -> impl FnOnce(parquet::errors::ParquetError) -> Failed {
    move |error| Failed { input: None, doing, error: CompactError::Parquet(error) }
}

impl Concat {
    /// The merge of `inputs` (at least one, all of the first one's schema), written with
    /// `properties` (the table's). Its metadata says the rows the inputs' footers held
    /// ([`MERGED_ROWS`]), and the first input's other keys (the Arrow schema, `blaze.schema`,
    /// `blaze.table`, `blaze.clock`).
    pub(crate) fn new(inputs: Vec<Input>, properties: &WriterProperties) -> Result<Concat, Failed> {
        let Some(first) = inputs.first() else {
            return Err(Failed {
                input: None,
                doing: "starting a merge of no input",
                error: CompactError::SchemaDiffers,
            });
        };
        let schema = first.metadata.file_metadata().schema_descr().root_schema_ptr();
        // alloc: writer properties, once a merge
        let properties = Arc::new(properties.clone());
        // alloc: the output's buffer, drained piece by piece, once a merge
        let buffer = Vec::with_capacity(PIECE + (1 << 20));
        let mut writer = SerializedFileWriter::new(buffer, Arc::clone(&schema), properties)
            .map_err(of_output("starting to write"))?;
        // An input's own count of merged rows is its, not this merge's.
        for kv in first
            .metadata
            .file_metadata()
            .key_value_metadata()
            .into_iter()
            .flatten()
            .filter(|it| it.key != MERGED_ROWS)
        {
            // clone: a key and its value, a few a merge
            writer.append_key_value_metadata(kv.clone());
        }
        Ok(Concat { writer: Some(writer), schema, inputs, at: (0, 0), rows: 0 })
    }

    /// The next piece of the output's bytes, `None` once it was all handed out: at least
    /// [`PIECE`] bytes of row groups when there are that many left, and the footer with the last.
    pub(crate) fn next(&mut self) -> Result<Option<Vec<u8>>, Failed> {
        let Some(writer) = self.writer.as_mut() else { return Ok(None) };
        while self.at.0 < self.inputs.len() {
            let (input_at, group_at) = self.at;
            let input = &self.inputs[input_at];
            let metadata = &input.metadata;
            if group_at == 0 {
                if metadata.file_metadata().schema() != &*self.schema {
                    return Err(Failed {
                        input: Some(input_at),
                        doing: "checking the schema of the input",
                        error: CompactError::SchemaDiffers,
                    });
                }
                self.rows += metadata.file_metadata().num_rows();
            }
            if group_at == metadata.num_row_groups() {
                self.at = (input_at + 1, 0);
                continue;
            }
            let row_group = metadata.row_group(group_at);
            let index = metadata.page_index_for_row_group(group_at);
            let mut group =
                writer.next_row_group().map_err(of_output("starting a row group of"))?;
            for (column_at, column) in row_group.columns().iter().enumerate() {
                let close = ColumnCloseResult {
                    bytes_written: column.compressed_size().cast_unsigned(),
                    rows_written: row_group.num_rows().cast_unsigned(),
                    // clone: a column chunk's metadata, once a chunk copied
                    metadata: column.clone(),
                    bloom_filter: None,
                    column_index: index.column_index(column_at).cloned(),
                    offset_index: index.offset_index(column_at).cloned(),
                };
                group.append_column(&input.bytes, close).map_err(|error| Failed {
                    input: Some(input_at),
                    doing: "copying a column chunk of the input",
                    error: CompactError::Parquet(error),
                })?;
            }
            group.close().map_err(of_output("ending a row group of"))?;
            self.at = (input_at, group_at + 1);
            writer.flush().map_err(|error| Failed {
                input: None,
                doing: "writing",
                error: CompactError::Io(error),
            })?;
            if writer.inner().len() >= PIECE {
                // alloc: a piece of the output, handed to its writer
                let piece = std::mem::replace(writer.inner_mut(), Vec::with_capacity(PIECE));
                return Ok(Some(piece));
            }
        }
        let Some(mut writer) = self.writer.take() else { return Ok(None) };
        // alloc: a key and its value, once a merge
        writer.append_key_value_metadata(KeyValue::new(
            MERGED_ROWS.to_owned(),
            self.rows.to_string(),
        ));
        let last = writer.into_inner().map_err(of_output("ending"))?;
        Ok(Some(last))
    }

    /// The whole output at once: for a caller that writes it in one write (the gate).
    pub(crate) fn all(mut self) -> Result<Vec<u8>, Failed> {
        // alloc: the merged file, once a merge
        let mut out = Vec::new();
        while let Some(piece) = self.next()? {
            out.extend_from_slice(&piece);
        }
        Ok(out)
    }
}
