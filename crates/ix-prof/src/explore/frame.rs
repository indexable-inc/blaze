//! The bytes the server sends (explore-wire.md, "Frames").
//!
//! An answer, on the request's bidirectional stream: `u32 LE` header length, the header's JSON
//! padded with spaces so that what follows starts a multiple of [`ALIGN`] bytes from the
//! stream's start, then one Arrow IPC stream (schema, batches, end of stream) or nothing, then
//! FIN. The page views the Arrow buffers in place: every IPC buffer is 8-aligned within the
//! stream, so it is 8-aligned in the page's copy of the stream too.
//!
//! The live stream, unidirectional, carries frames back to back: `u32 LE` frame length, then a
//! frame laid out as an answer, its alignment counted from the frame's own first byte (the byte
//! after its length), since the page copies each frame out of the stream whole. A live frame's
//! IPC is not a whole stream: it is the next messages of its table's one stream for the session
//! (the source's encoder: blaze-profile's `live::Encoder` for the daemon), schema and
//! dictionaries once, then a batch and its dictionaries' new values a frame, and an
//! end-of-stream marker then a new schema where the stream starts again.

use std::alloc::Global;

use blaze_arrow_ipc::IpcError;
use blaze_columns::{Batch, Field};
use serde::Serialize;

use super::{Answer, Kind, OpError, session};

/// Where the Arrow IPC stream starts: a multiple of this from the start of the answer or frame.
pub const ALIGN: usize = 8;

/// Appends the head of an answer to `out`: the header's length and its JSON, padded so that
/// what is appended next starts [`ALIGN`]-aligned from where the head began. A header is what
/// serializes as one JSON object: a typed header of [`session`]'s, an answer's map.
pub fn head<H: Serialize + ?Sized>(out: &mut Vec<u8>, header: &H) -> Result<(), serde_json::Error> {
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
pub fn live_head<H: Serialize + ?Sized>(
    out: &mut Vec<u8>,
    header: &H,
    ipc_len: usize,
) -> Result<(), serde_json::Error> {
    let start = out.len();
    out.extend_from_slice(&[0; 4]);
    head(out, header)?;
    let len = (out.len() - start - 4 + ipc_len) as u32;
    out[start..start + 4].copy_from_slice(&len.to_le_bytes());
    Ok(())
}

/// Appends `batches`, all of `fields`, to `out` as one Arrow IPC stream (uncompressed, its
/// buffers 8-aligned). `out` must be [`ALIGN`]-aligned where the stream starts for the page to
/// view it in place; [`head`] leaves it so.
pub fn ipc(out: &mut Vec<u8>, fields: &[Field<Global>], batches: &[Batch<Global>]) -> Result<(), IpcError> {
    let stream = blaze_arrow_ipc::stream(fields, &[], batches, Global)?;
    // alloc: the answer's bytes, into the buffer the driver writes
    out.extend_from_slice(&stream);
    Ok(())
}

/// An answer as the wire carries it: its header, `rows` and `elapsed_ns` added
/// ([`session::answer_header`]), then its table's IPC; or the error it became. The session core
/// decides the header; the IPC is written here, outside the core, by the driver that holds the
/// answer's bytes.
pub fn answer_bytes(answer: Result<Answer, OpError>, elapsed_ns: u64) -> Vec<u8> {
    // alloc: once an answer, the bytes the driver writes
    let mut out = Vec::new();
    let encoded = answer.and_then(|mut answer| {
        session::answer_header(&mut answer, elapsed_ns);
        head(&mut out, &answer.header).expect("a JSON map always serializes");
        if let Some(table) = &answer.table {
            ipc(&mut out, &table.fields, &table.batches).map_err(|why| {
                OpError::new(Kind::Encode, format!("the answer as Arrow IPC: {why}"))
            })?;
        }
        Ok(())
    });
    if let Err(why) = encoded {
        out.clear();
        head(&mut out, &why.header()).expect("an error's header always serializes");
    }
    out
}

#[cfg(test)]
mod tests {
    use blaze_columns::{Column, Scalar, Type};
    use serde_json::{Map, Value};

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
            assert!(
                json.len() - json.trim_ascii_end().len() < ALIGN,
                "padded by less than {ALIGN}"
            );
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
        let fields = [
            Field::new("n", Type::One(Scalar::I64), false),
            Field::new("s", Type::One(Scalar::Text), true),
        ];
        let batches = blaze_own::Vec::from_iter_in(
            (0..3i64).map(|i| {
                let n = Column::values((0..100i64).map(|it| it * i), Global);
                let s: Vec<Option<String>> =
                    (0..100i64).map(|it| (it % 3 == 0).then(|| format!("row {it}"))).collect();
                let s = Column::opt_texts(&s, Global);
                Batch::try_new(
                    blaze_own::Vec::from_slice_in(&fields, Global),
                    blaze_own::Vec::from_array_in([n, s], Global),
                )
                .unwrap()
            }),
            Global,
        );
        let mut out = Vec::new();
        head(&mut out, &header("select n, s from t")).unwrap();
        let at = out.len();
        assert_eq!(at % ALIGN, 0);
        ipc(&mut out, &fields, &batches).unwrap();
        assert_eq!(out.len() % ALIGN, 0, "an 8-aligned IPC stream is a multiple of 8 long");
        let back = blaze_arrow_ipc::read_stream(&out[at..], Global).unwrap();
        assert_eq!(back.batches, batches);
    }
}
