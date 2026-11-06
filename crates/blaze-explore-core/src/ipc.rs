//! The drivers' Arrow IPC decoder: arrow-ipc's `StreamDecoder` as [`live::Stream`], the type
//! the drivers ([`crate::abi`], the tests) hand the cores ([`crate::state::Session`]'s `S`).
//!
//! Not a sans-IO core, and so unmarked: `StreamDecoder` keeps its dictionaries, and each schema
//! it reads its metadata, in std `HashMap`s, each seeded by `RandomState::new` (OS randomness
//! on a host; wasm32-unknown-unknown has none). The cores take the decoder as a type parameter,
//! so that seed is the driver's. Nothing the cores read depends on it: the dictionaries are
//! looked up by id, and the cores read no schema metadata.

use arrow_array::RecordBatch;
use arrow_buffer::Buffer;
use arrow_ipc::reader::StreamDecoder;
use arrow_schema::ArrowError;

use crate::live;

impl live::Stream for StreamDecoder {
    fn begin() -> StreamDecoder {
        StreamDecoder::new()
    }

    fn decode(&mut self, buffer: &mut Buffer) -> Result<Option<RecordBatch>, ArrowError> {
        // The inherent method, not this one.
        StreamDecoder::decode(self, buffer)
    }

    fn finish(&mut self) -> Result<(), ArrowError> {
        StreamDecoder::finish(self)
    }
}
