//! Compaction: a finished top-level object (a run with its stages, or a PTY session) is written to
//! archive/objects/<root>.parquet (zstd) as line rows (object, stream, line_no, ts_ms, text), then its
//! streams are marked compacted in the index. Raw files are kept.
//!
//! Lines follow one rule shared with the REPL's live reader (src/sql.ts): split on '\n' (dropped),
//! a trailing piece without '\n' is a line when non-empty, bytes decoded as lossy UTF-8.
use crate::db;
use anyhow::{Context, Result};
use parquet::basic::{Compression, ZstdLevel};
use parquet::data_type::{ByteArray, ByteArrayType, Int64Type};
use parquet::file::properties::WriterProperties;
use parquet::file::writer::SerializedFileWriter;
use parquet::schema::parser::parse_message_type;
use std::io::BufRead;
use std::sync::Arc;

const SCHEMA: &str = "message line {
  required binary object (UTF8);
  required binary stream (UTF8);
  required int64 line_no;
  optional int64 ts_ms;
  required binary text (UTF8);
}";

const GROUP_ROWS: usize = 128 * 1024;

#[derive(Default)]
struct Rows {
    object: Vec<ByteArray>,
    stream: Vec<ByteArray>,
    line_no: Vec<i64>,
    text: Vec<ByteArray>,
}

fn flush(w: &mut SerializedFileWriter<std::fs::File>, rows: &mut Rows) -> Result<()> {
    if rows.line_no.is_empty() {
        return Ok(());
    }
    let n = rows.line_no.len();
    let mut rg = w.next_row_group()?;
    let mut i = 0;
    while let Some(mut col) = rg.next_column()? {
        match i {
            0 => { col.typed::<ByteArrayType>().write_batch(&rows.object, None, None)?; }
            1 => { col.typed::<ByteArrayType>().write_batch(&rows.stream, None, None)?; }
            2 => { col.typed::<Int64Type>().write_batch(&rows.line_no, None, None)?; }
            3 => { col.typed::<Int64Type>().write_batch(&[], Some(&vec![0i16; n]), None)?; } // ts_ms: all null for now
            _ => { col.typed::<ByteArrayType>().write_batch(&rows.text, None, None)?; }
        }
        col.close()?;
        i += 1;
    }
    rg.close()?;
    *rows = Rows::default();
    Ok(())
}

/// Write `root`'s streams to its parquet file and mark them compacted. Idempotent.
pub fn compact(root: &str) -> Result<()> {
    let streams = db::tree_streams(root);
    if streams.is_empty() {
        return Ok(());
    }
    let out = db::archive_path(root);
    std::fs::create_dir_all(out.parent().unwrap())?;
    let tmp = out.with_extension("tmp");
    let props = WriterProperties::builder().set_compression(Compression::ZSTD(ZstdLevel::try_new(3)?)).build();
    let mut w = SerializedFileWriter::new(std::fs::File::create(&tmp)?, Arc::new(parse_message_type(SCHEMA)?), Arc::new(props))?;
    let mut rows = Rows::default();
    for (object, stream, path) in &streams {
        let Ok(f) = std::fs::File::open(path) else { continue };
        let mut r = std::io::BufReader::with_capacity(1 << 20, f);
        let mut buf = vec![];
        let mut n = 0i64;
        loop {
            buf.clear();
            if r.read_until(b'\n', &mut buf).with_context(|| format!("read {}", path.display()))? == 0 {
                break;
            }
            if buf.last() == Some(&b'\n') {
                buf.pop();
            }
            n += 1;
            rows.object.push(ByteArray::from(object.as_str()));
            rows.stream.push(ByteArray::from(stream.as_str()));
            rows.line_no.push(n);
            rows.text.push(ByteArray::from(String::from_utf8_lossy(&buf).as_bytes().to_vec()));
            if rows.line_no.len() >= GROUP_ROWS {
                flush(&mut w, &mut rows)?;
            }
        }
    }
    flush(&mut w, &mut rows)?;
    w.close()?;
    // The rename makes the file visible to readers; until the index says compacted they skip it.
    std::fs::rename(&tmp, &out)?;
    db::set_compacted(root, &out, crate::runs::now_ms());
    Ok(())
}

/// Compact in the background, logging failures.
pub fn compact_bg(root: String) {
    std::thread::spawn(move || {
        if let Err(e) = compact(&root) {
            eprintln!("compact {root}: {e:#}");
        }
    });
}

/// Startup: compact every finished object that isn't yet.
pub fn compact_pending() {
    for root in db::uncompacted_roots() {
        if let Err(e) = compact(&root) {
            eprintln!("compact {root}: {e:#}");
        }
    }
}
