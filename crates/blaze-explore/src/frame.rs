//! The bytes the daemon sends (explore-wire.md, "Frames").
//!
//! An answer, on the request's bidirectional stream: `u32 LE` header length, the header's JSON
//! padded with spaces so that what follows starts a multiple of [`ALIGN`] bytes from the
//! stream's start, then one Arrow IPC stream (schema, batches, end of stream) or nothing, then
//! FIN. The page views the Arrow buffers in place: every IPC buffer is 8-aligned within the
//! stream, so it is 8-aligned in the page's copy of the stream too.
//!
//! The live stream, unidirectional, carries frames back to back: `u32 LE` frame length, then a
//! frame laid out as an answer, its alignment counted from the frame's own first byte (the byte
//! after its length), since the page copies each frame out of the stream whole.

use arrow_array::RecordBatch;
use arrow_ipc::{
    CompressionType, MetadataVersion,
    writer::{IpcWriteOptions, StreamWriter},
};
use arrow_schema::{ArrowError, Schema};
use serde_json::{Map, Value};

/// Where the Arrow IPC stream starts: a multiple of this from the start of the answer or frame.
pub const ALIGN: usize = 8;

/// The zstd level of the IPC buffers: the fastest, since the bytes cross loopback, not a
/// network (docs/v2/plan/lanes/observe/explore.md, "Speed of light").
pub const ZSTD_LEVEL: i32 = 1;

/// Appends the head of an answer to `out`: the header's length and its JSON, padded so that
/// what is appended next starts [`ALIGN`]-aligned from where the head began.
pub fn head(out: &mut Vec<u8>, header: &Map<String, Value>) -> Result<(), serde_json::Error> {
    let start = out.len();
    out.extend_from_slice(&[0; 4]);
    serde_json::to_writer(&mut *out, header)?;
    let pad = (ALIGN - (out.len() - start) % ALIGN) % ALIGN;
    out.resize(out.len() + pad, b' ');
    let len = (out.len() - start - 4) as u32;
    out[start..start + 4].copy_from_slice(&len.to_le_bytes());
    Ok(())
}

/// Appends the head of a live frame to `out`: the frame's length, then [`head`], for a frame
/// whose IPC stream, sent right after, is `ipc_len` bytes.
pub fn live_head(out: &mut Vec<u8>, header: &Map<String, Value>, ipc_len: usize) -> Result<(), serde_json::Error> {
    let start = out.len();
    out.extend_from_slice(&[0; 4]);
    head(out, header)?;
    let len = (out.len() - start - 4 + ipc_len) as u32;
    out[start..start + 4].copy_from_slice(&len.to_le_bytes());
    Ok(())
}

/// The IPC options of every answer: 8-aligned buffers, metadata V5, zstd.
pub fn options() -> Result<IpcWriteOptions, ArrowError> {
    IpcWriteOptions::try_new(ALIGN, false, MetadataVersion::V5)?
        .try_with_compression(Some(CompressionType::ZSTD))?
        .try_with_compression_level(Some(ZSTD_LEVEL))
}

/// Appends `batches`, all of `schema`, to `out` as one Arrow IPC stream. `out` must be
/// [`ALIGN`]-aligned where the stream starts for the page to view it in place; [`head`] leaves
/// it so.
pub fn ipc(out: &mut Vec<u8>, schema: &Schema, batches: &[RecordBatch]) -> Result<(), ArrowError> {
    let mut writer = StreamWriter::try_new_with_options(&mut *out, schema, options()?)?;
    for batch in batches {
        writer.write(batch)?;
    }
    writer.finish()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arrow_array::{Int64Array, StringArray};
    use arrow_ipc::reader::StreamReader;
    use arrow_schema::{DataType, Field};

    use super::*;

    fn header(text: &str) -> Map<String, Value> {
        let mut header = Map::new();
        header.insert("op".to_owned(), "sql".into());
        header.insert("sql".to_owned(), text.into());
        header
    }

    #[test]
    fn the_arrow_bytes_start_aligned_whatever_the_header() {
        for n in 0..40 {
            let header = header(&"x".repeat(n));
            let mut out = Vec::new();
            head(&mut out, &header).unwrap();
            assert_eq!(out.len() % ALIGN, 0, "a header of {n}");
            let len = u32::from_le_bytes(out[..4].try_into().unwrap()) as usize;
            assert_eq!(len + 4, out.len());
            let json = &out[4..];
            assert!(json.len() - json.trim_ascii_end().len() < ALIGN, "padded by less than {ALIGN}");
            let back: Map<String, Value> = serde_json::from_slice(json).unwrap();
            assert_eq!(back, header);
        }
    }

    #[test]
    fn a_live_frame_is_its_length_then_an_answer() {
        for n in 0..20 {
            let header = header(&"y".repeat(n));
            let ipc = vec![7u8; 24 + n * 8];
            let mut out = vec![0xee; 3];
            live_head(&mut out, &header, ipc.len()).unwrap();
            out.extend_from_slice(&ipc);
            let frame = &out[3..];
            let len = u32::from_le_bytes(frame[..4].try_into().unwrap()) as usize;
            assert_eq!(len + 4, frame.len());
            let body = &frame[4..];
            let head_len = u32::from_le_bytes(body[..4].try_into().unwrap()) as usize;
            assert_eq!((4 + head_len) % ALIGN, 0, "the IPC starts aligned from the frame's body");
            assert_eq!(&body[4 + head_len..], &ipc[..]);
            let back: Map<String, Value> = serde_json::from_slice(&body[4..4 + head_len]).unwrap();
            assert_eq!(back, header);
        }
    }

    #[test]
    fn an_answer_reads_back_as_one_ipc_stream_at_an_aligned_offset() {
        let schema = Arc::new(Schema::new(vec![
            Field::new("n", DataType::Int64, false),
            Field::new("s", DataType::Utf8, true),
        ]));
        let batches: Vec<RecordBatch> = (0..3i64)
            .map(|i| {
                let n: Int64Array = (0..100i64).map(|it| it * i).collect();
                let s: StringArray = (0..100i64).map(|it| (it % 3 == 0).then(|| format!("row {it}"))).collect();
                RecordBatch::try_new(Arc::clone(&schema), vec![Arc::new(n), Arc::new(s)]).unwrap()
            })
            .collect();
        let mut out = Vec::new();
        head(&mut out, &header("select n, s from t")).unwrap();
        let at = out.len();
        assert_eq!(at % ALIGN, 0);
        ipc(&mut out, &schema, &batches).unwrap();
        assert_eq!(out.len() % ALIGN, 0, "an 8-aligned IPC stream is a multiple of 8 long");
        let back: Vec<RecordBatch> =
            StreamReader::try_new(&out[at..], None).unwrap().collect::<Result<_, _>>().unwrap();
        assert_eq!(back, batches);
    }
}
