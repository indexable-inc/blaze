//! A merge's bytes: the rows of Parquet files of one schema as one file, their column chunks
//! copied with no decode and no re-encode of their values (only their pages compressed, a
//! merge being cold), so row groups stay the size their flush wrote. Pure: the inputs are bytes in memory (a mapping of each, [`Input::of`]) and the
//! output is handed out a piece at a time ([`Concat::next`]) for its writer to append. Who
//! reads and writes the files is its caller: the history's core ([`super::files`]) through
//! effects, the gate through its own files. The copying is [`blaze_parquet::Concat`]'s; this is
//! the history's face of it: its piece size, its rows key and its errors.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::alloc::Global;

use super::{CompactError, MERGED_ROWS};

/// An input's bytes: a mapped file's, read in place, or bytes in memory (the gate's, a test's).
pub(crate) enum Bytes {
    Mapped(blaze_core::Mapping),
    Owned(Vec<u8>),
}

impl AsRef<[u8]> for Bytes {
    fn as_ref(&self) -> &[u8] {
        match self {
            Bytes::Mapped(it) => it.bytes(),
            Bytes::Owned(it) => it,
        }
    }
}

/// One input of a merge: its bytes and its footer.
pub(crate) struct Input(blaze_parquet::Input<Bytes, Global>);

impl Input {
    /// The input whose bytes `mapping` maps; its footer read, or the error saying why not.
    pub(crate) fn of(mapping: blaze_core::Mapping) -> Result<Input, CompactError> {
        Input::open(Bytes::Mapped(mapping))
    }

    /// The input of `bytes`, a whole Parquet file.
    pub(crate) fn of_bytes(bytes: Vec<u8>) -> Result<Input, CompactError> {
        Input::open(Bytes::Owned(bytes))
    }

    fn open(bytes: Bytes) -> Result<Input, CompactError> {
        blaze_parquet::Input::open(bytes, &Global).map(Input).map_err(CompactError::Parquet)
    }
}

/// How many bytes of output [`Concat::next`] gathers before it hands them out: the writer
/// appends a piece this size, never the whole merge at once.
const PIECE: usize = 8 << 20;

/// A merge under way.
pub(crate) struct Concat(blaze_parquet::Concat<Bytes, Global>);

/// Where a merge failed: the input at `input` (its position in the merge's inputs), or the
/// output when `None`, and the step.
#[derive(Debug)]
pub(crate) struct Failed {
    pub(crate) input: Option<usize>,
    pub(crate) doing: &'static str,
    pub(crate) error: CompactError,
}

impl From<blaze_parquet::Failed> for Failed {
    fn from(failed: blaze_parquet::Failed) -> Failed {
        let error = match failed.error {
            blaze_parquet::ParquetError::SchemaDiffers => CompactError::SchemaDiffers,
            other => CompactError::Parquet(other),
        };
        Failed { input: failed.input, doing: failed.doing, error }
    }
}

impl Concat {
    /// The merge of `inputs` (at least one, all of the first one's schema). Its metadata says
    /// the rows the inputs' footers held ([`MERGED_ROWS`]), and the first input's other keys
    /// (`blaze.schema`, `blaze.table`, `blaze.clock`). A merge is a cold file: its pages are
    /// zstd ([`blaze_parquet::Compression::COLD`]), a build's own files' recompressed.
    pub(crate) fn new(inputs: Vec<Input>) -> Result<Concat, Failed> {
        let inputs = blaze_own::Vec::from_iter_in(inputs.into_iter().map(|it| it.0), Global);
        blaze_parquet::Concat::new(inputs, MERGED_ROWS, PIECE, blaze_parquet::Compression::COLD, Global)
            .map(Concat)
            .map_err(Failed::from)
    }

    /// The next piece of the output's bytes, `None` once it was all handed out: at least
    /// [`PIECE`] bytes of row groups when there are that many left, and the footer with the last.
    pub(crate) fn next(&mut self) -> Result<Option<Vec<u8>>, Failed> {
        // alloc: a piece of the output, handed to its writer as the effect's bytes (a copy:
        // an effect takes std's `Vec`)
        Ok(self.0.next()?.map(|piece| piece.to_vec()))
    }

    /// The whole output at once: for a caller that writes it in one write (the gate).
    pub(crate) fn all(self) -> Result<Vec<u8>, Failed> {
        // alloc: the merged file, once a merge
        Ok(self.0.all()?.to_vec())
    }
}
