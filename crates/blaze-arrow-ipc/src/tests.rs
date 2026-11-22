use std::alloc::Global;

use blaze_columns::{Batch, Column, Field, Scalar, Texts, Type, Values};
use blaze_own::Vec;

use crate::{
    END, IpcError, Message, StreamReader, StreamWriter,
    fb::{Builder, Tab},
    file, messages, read_file, read_stream, stream,
};

fn texts(values: &[&str]) -> Texts<Global> {
    let mut out = Texts::new_in(Global);
    for value in values {
        out.push(value);
    }
    out
}

fn fields() -> std::vec::Vec<Field<Global>> {
    vec![
        Field::new("flag", Type::One(Scalar::Bool), true),
        Field::new("small", Type::One(Scalar::I32), true),
        Field::new("t_ns", Type::One(Scalar::I64), false),
        Field::new("depth", Type::One(Scalar::U16), false),
        Field::new("pid", Type::One(Scalar::U32), false),
        Field::new("id", Type::One(Scalar::U64), true),
        Field::new("ratio", Type::One(Scalar::F64), true),
        Field::new("text", Type::One(Scalar::Text), true),
        Field::new("kind", Type::One(Scalar::Dict), true),
        Field::new("stack", Type::List(Scalar::Dict), true),
        Field::new("pcs", Type::List(Scalar::U64), false),
    ]
}

/// Three rows naming `kinds` and `frames`, and a null of each nullable column.
fn batch(kinds: [&str; 2], frames: [&str; 3]) -> Batch<Global> {
    let columns = [
        Column::options([Some(true), None, Some(false)], Global),
        Column::options([Some(-1), None, Some(i32::MAX)], Global),
        Column::values([i64::MIN, 0, 5], Global),
        Column::values([0u16, 7, u16::MAX], Global),
        Column::values([u32::MAX, 0, 1], Global),
        Column::options([Some(u64::MAX), Some(1), None], Global),
        Column::options([Some(1.5), Some(-0.25), None], Global),
        Column::opt_texts(&[Some("x"), None, Some("")], Global),
        Column::dict([Some(1), None, Some(0)], texts(&kinds), Global).unwrap(),
        Column::list(
            Vec::from_slice_in(&[0, 2, 2, 3], Global),
            Some(Vec::from_slice_in(&[true, false, true], Global)),
            Values::Dict { keys: Vec::from_slice_in(&[0, 1, 2], Global), values: texts(&frames) },
        )
        .unwrap(),
        Column::list(Vec::from_slice_in(&[0, 1, 1, 3], Global), None, Values::U64(Vec::from_slice_in(&[1, u64::MAX, 7], Global)))
            .unwrap(),
    ];
    Batch::try_new(Vec::from_iter_in(fields(), Global), Vec::from_iter_in(columns, Global)).unwrap()
}

/// Every row of `batch` as text, so a column re-keyed onto a stream's dictionary compares by
/// what it says.
fn rows(batch: &Batch<Global>) -> std::vec::Vec<std::string::String> {
    (0..batch.num_rows())
        .map(|row| {
            let cells: std::vec::Vec<std::string::String> = batch
                .columns()
                .iter()
                .map(|column| match column.lists() {
                    Some(lists) if lists.is_null(row) => "null".to_owned(),
                    Some(lists) => match lists.texts(row) {
                        Some(texts) => format!("{:?}", texts.collect::<std::vec::Vec<_>>()),
                        None => format!("{:?}", lists.slice::<u64>(row)),
                    },
                    None if column.is_null(row) => "null".to_owned(),
                    None => match (column.text(row), column.int(row), column.float(row), column.bools()) {
                        (Some(text), ..) => format!("{text:?}"),
                        (_, Some(int), ..) => int.to_string(),
                        (_, _, Some(float), _) => float.to_string(),
                        (_, _, _, Some(bools)) => bools.value(row).to_string(),
                        _ => "?".to_owned(),
                    },
                })
                .collect();
            cells.join("|")
        })
        .collect()
}

#[test]
fn a_table_of_one_field_is_these_bytes() {
    let mut b = Builder::new_in(Global);
    let mut t = b.start();
    b.i32(&mut t, 0, 7);
    let root = b.end(t);
    let mut out = Vec::new_in(Global);
    b.finish(root, &mut out);
    // The root offset, padding, the vtable (its size, the table's, field 0 at 4), the table
    // (back 6 to its vtable, then 7).
    assert_eq!(out.as_slice(), [16, 0, 0, 0, 0, 0, 0, 0, 0, 0, 6, 0, 8, 0, 4, 0, 6, 0, 0, 0, 7, 0, 0, 0]);
    let tab = Tab::root(out.as_slice()).unwrap();
    assert_eq!((tab.i32(0, -1).unwrap(), tab.i32(1, -1).unwrap()), (7, -1));
}

#[test]
fn a_stream_reads_back_as_written() {
    let one = batch(["a", "bb"], ["main", "f", "g"]);
    let two = batch(["bb", "c"], ["main", "h", "f"]);
    let metadata = [("blaze.table", "spans"), ("blaze.schema", "5")];
    let bytes = stream(&fields(), &metadata, &[one.clone(), two.clone()], Global).unwrap();
    assert!(bytes.as_slice().ends_with(&END));
    let read = read_stream(bytes.as_slice(), Global).unwrap();
    assert_eq!(read.fields.as_slice(), fields().as_slice());
    assert_eq!(read.value("blaze.schema"), Some("5"));
    assert_eq!(read.batches.len(), 2);
    assert_eq!(rows(&read.batches[0]), rows(&one));
    assert_eq!(rows(&read.batches[1]), rows(&two));
    // Every message is 8-aligned.
    assert_eq!(bytes.len() % 8, 0);
}

#[test]
fn a_dictionary_crosses_once_and_then_as_deltas_of_new_values() {
    let mut writer = StreamWriter::new(&fields(), &[], Global);
    let mut out = Vec::new_in(Global);
    writer.write(&batch(["a", "bb"], ["main", "f", "g"]), &mut out).unwrap();
    let first = out.len();
    writer.write(&batch(["bb", "c"], ["main", "h", "f"]), &mut out).unwrap();
    let second = out.len();
    writer.write(&batch(["c", "a"], ["g", "h", "main"]), &mut out).unwrap();
    // `kind`: bb, a, c; `stack`: main, f, g, h.
    assert_eq!(writer.dictionary_size(), (3 + 4, 4 + 7));
    let got = messages(out.as_slice(), Global).unwrap();
    use Message::{Batch as B, Dictionary as D};
    assert_eq!(
        got.as_slice(),
        [
            Message::Schema,
            // `kind` names `bb` and `a` (row 1 is null), `stack` its three frames.
            D { id: 0, delta: false, rows: 2 },
            D { id: 1, delta: false, rows: 3 },
            B { rows: 3 },
            D { id: 0, delta: true, rows: 1 },
            D { id: 1, delta: true, rows: 1 },
            B { rows: 3 },
            B { rows: 3 },
        ]
    );
    // A client reads them a frame at a time.
    let mut reader = StreamReader::new(Global);
    let mut batches = Vec::new_in(Global);
    for frame in [&out.as_slice()[..first], &out.as_slice()[first..second], &out.as_slice()[second..]] {
        reader.read(frame, &mut batches).unwrap();
    }
    assert_eq!(rows(&batches[2]), rows(&batch(["c", "a"], ["g", "h", "main"])));
}

#[test]
fn a_new_stream_begins_after_the_end() {
    let one = stream(&fields(), &[], &[batch(["a", "b"], ["x", "y", "z"])], Global).unwrap();
    let two = stream(&fields(), &[], &[batch(["q", "r"], ["x", "w", "z"])], Global).unwrap();
    let mut reader = StreamReader::new(Global);
    let mut batches = Vec::new_in(Global);
    reader.read(one.as_slice(), &mut batches).unwrap();
    reader.read(two.as_slice(), &mut batches).unwrap();
    assert_eq!((reader.streams(), batches.len()), (2, 2));
    assert_eq!(rows(&batches[1]), rows(&batch(["q", "r"], ["x", "w", "z"])));
}

#[test]
fn a_file_reads_back_as_written() {
    let batches = [batch(["a", "bb"], ["main", "f", "g"]), batch(["bb", "c"], ["main", "h", "f"])];
    let bytes = file(&fields(), &[("ixprof.table", "spans")], &batches, Global).unwrap();
    assert!(bytes.as_slice().starts_with(b"ARROW1\0\0") && bytes.as_slice().ends_with(b"ARROW1"));
    let read = read_file(bytes.as_slice(), Global).unwrap();
    assert_eq!(read.value("ixprof.table"), Some("spans"));
    assert_eq!(read.batches.len(), 2);
    for (got, want) in read.batches.iter().zip(&batches) {
        assert_eq!(rows(got), rows(want));
    }
    let empty = file(&fields(), &[], &[], Global).unwrap();
    assert_eq!(read_file(empty.as_slice(), Global).unwrap().batches.len(), 0);
}

#[test]
fn same_batches_give_the_same_bytes() {
    let batches = [batch(["a", "bb"], ["main", "f", "g"])];
    assert_eq!(stream(&fields(), &[], &batches, Global).unwrap(), stream(&fields(), &[], &batches, Global).unwrap());
}

#[test]
fn a_cut_stream_and_another_batch_are_refused() {
    let bytes = stream(&fields(), &[], &[batch(["a", "b"], ["x", "y", "z"])], Global).unwrap();
    let cut = &bytes.as_slice()[..bytes.len() - 20];
    assert!(matches!(read_stream(cut, Global), Err(IpcError::Truncated { .. })), "{:?}", read_stream(cut, Global).err());
    let mut writer = StreamWriter::new(&fields()[..1], &[], Global);
    let mut out = Vec::new_in(Global);
    assert_eq!(writer.write(&batch(["a", "b"], ["x", "y", "z"]), &mut out), Err(IpcError::FieldsDiffer));
    assert!(out.is_empty());
}
