//! Bytes-level tests of the Thrift encoder and the level encoding against vectors computed by
//! hand from the specs, and round trips of the writer through the reader and the merge.

use std::alloc::Global;

use blaze_columns::{Batch, Column, Field, Scalar, Texts, Type};
use blaze_own::Vec;

use crate::{
    Concat, File, Input, Options, ParquetError, levels,
    thrift::{In, Struct, zigzag32, zigzag64},
    write,
};

fn bytes(f: impl FnOnce(&mut Struct<'_, Global>)) -> std::vec::Vec<u8> {
    let mut out = Vec::new_in(Global);
    let mut s = Struct::new(&mut out);
    f(&mut s);
    s.end();
    out.as_slice().to_vec()
}

#[test]
fn zigzag_is_the_specs() {
    assert_eq!([zigzag32(0), zigzag32(-1), zigzag32(1), zigzag32(-2), zigzag32(i32::MAX)], [0, 1, 2, 3, u32::MAX - 1]);
    assert_eq!(zigzag64(i64::MIN), u64::MAX);
}

#[test]
fn a_field_header_holds_a_small_delta() {
    // i32 field 1 = 1: header 0001_0101, zigzag(1) = 2, stop.
    assert_eq!(bytes(|s| s.i32(1, 1)), [0x15, 0x02, 0x00]);
}

#[test]
fn a_far_field_says_its_id() {
    // i64 field 20 = -1: a delta past 15, so type 6 alone, then zigzag(20) = 40, then zigzag(-1) = 1.
    assert_eq!(bytes(|s| s.i64(20, -1)), [0x06, 0x28, 0x01, 0x00]);
}

#[test]
fn varints_bools_and_binaries() {
    // 300 zigzags to 600: 0b100_1011000 -> 0xD8 0x04. A bool is its header's type (1 true), a
    // binary its length and bytes.
    let got = bytes(|s| {
        s.i32(1, 300);
        s.bool(2, true);
        s.bool(3, false);
        s.binary(4, b"ab");
    });
    assert_eq!(got, [0x15, 0xD8, 0x04, 0x11, 0x12, 0x18, 0x02, b'a', b'b', 0x00]);
}

#[test]
fn lists_short_and_long() {
    // Three i32s: list header 0011_0101.
    assert_eq!(bytes(|s| s.i32s(1, [1, 2, 3].into_iter())), [0x19, 0x35, 0x02, 0x04, 0x06, 0x00]);
    // Fifteen: the size no longer fits the nibble, so 1111_0101 and a varint 15.
    let mut want = vec![0x19, 0xF5, 0x0F];
    want.extend([0; 15]);
    want.push(0x00);
    assert_eq!(bytes(|s| s.i32s(1, [0; 15].into_iter())), want);
}

#[test]
fn nested_structs_restart_their_deltas() {
    // Field 1 a struct of i32 field 1 = 0; field 2 an empty struct; the outer stop.
    let got = bytes(|s| {
        s.strukt(1, |s| s.i32(1, 0));
        s.empty(2);
    });
    assert_eq!(got, [0x1C, 0x15, 0x00, 0x00, 0x1C, 0x00, 0x00]);
}

#[test]
fn the_decoder_reads_what_the_encoder_wrote_and_skips_the_rest() {
    let got = bytes(|s| {
        s.i32(1, -7);
        s.binary(2, b"skip me");
        s.strukt(3, |s| s.i64(1, 1 << 40));
        s.i16(30, -3);
    });
    let mut input = In::new(&got);
    let (mut a, mut c, mut d) = (0, 0, 0);
    input
        .strukt(|input, id, _| {
            match id {
                1 => a = input.i32()?,
                3 => input.strukt(|input, _, _| {
                    c = input.i64()?;
                    Ok(true)
                })?,
                30 => d = input.i16()?,
                _ => return Ok(false),
            }
            Ok(true)
        })
        .unwrap();
    assert_eq!((a, c, d), (-7, 1 << 40, -3));
    assert_eq!(input.read(), got.len());
}

#[test]
fn a_truncated_or_deep_footer_is_refused() {
    let got = bytes(|s| s.binary(1, b"abcdef"));
    let cut = &got[..4];
    let mut input = In::new(cut);
    let err = input.strukt(|input, _, _| input.binary().map(|_| true)).unwrap_err();
    assert!(matches!(err, ParquetError::Truncated { .. }), "{err:?}");
    // Forty nested structs, past the depth.
    let deep = [0x1Cu8; 40];
    let err = In::new(&deep).skip(crate::thrift::STRUCT).unwrap_err();
    assert!(matches!(err, ParquetError::Thrift { .. }), "{err:?}");
}

fn encode(values: &[u8], width: u8) -> std::vec::Vec<u8> {
    let mut out = Vec::new_in(Global);
    levels::encode(&mut out, values, width);
    out.as_slice().to_vec()
}

fn decode(bytes: &[u8], width: u8, count: usize) -> std::vec::Vec<u8> {
    let mut out = vec![];
    levels::decode(bytes, width, count, |it| out.push(u8::try_from(it).unwrap())).unwrap();
    out
}

#[test]
fn widths() {
    assert_eq!([0, 1, 2, 3, 4, 7, 8].map(levels::bit_width), [0, 1, 2, 2, 3, 3, 4]);
}

#[test]
fn a_long_run_is_one_repeated_run() {
    // Ten 1s: header 10 << 1 = 20, the value in one byte.
    assert_eq!(encode(&[1; 10], 1), [0x14, 0x01]);
    assert_eq!(decode(&[0x14, 0x01], 1, 10), [1; 10]);
}

#[test]
fn short_runs_are_bit_packed() {
    // One group: header 1 << 1 | 1 = 3; 1,0,1,0,1 and three 0s of filling, LSB first: 0b0001_0101.
    assert_eq!(encode(&[1, 0, 1, 0, 1], 1), [0x03, 0x15]);
    assert_eq!(decode(&[0x03, 0x15], 1, 5), [1, 0, 1, 0, 1]);
    // A run of eleven after one value does not fill a group and eight more: all packed, two
    // groups (0b1111_1110, 0b0000_1111).
    let values = [0, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1];
    assert_eq!(encode(&values, 1), [0x05, 0xFE, 0x0F]);
    assert_eq!(decode(&[0x05, 0xFE, 0x0F], 1, 12), values);
}

#[test]
fn a_run_fills_the_packed_group_before_it_repeats() {
    // 2,2,2 then twenty 0s at width 2: the first five 0s fill the packed group
    // (2 | 2 << 2 | 2 << 4 = 0x2A, then 0x00), the other fifteen are one run (30, value 0).
    let mut values = vec![2, 2, 2];
    values.extend([0; 20]);
    let got = encode(&values, 2);
    assert_eq!(got, [0x03, 0x2A, 0x00, 0x1E, 0x00]);
    assert_eq!(decode(&got, 2, values.len()), values);
}

#[test]
fn levels_that_end_early_are_refused() {
    assert!(levels::decode(&[0x14], 1, 10, |_| {}).is_err());
    assert!(levels::decode(&[0x05, 0xFE], 1, 12, |_| {}).is_err());
}

fn fields() -> std::vec::Vec<Field<Global>> {
    vec![
        Field::new("flag", Type::One(Scalar::Bool), false),
        Field::new("small", Type::One(Scalar::I32), true),
        Field::new("t_ns", Type::One(Scalar::I64), false),
        Field::new("depth", Type::One(Scalar::U16), false),
        Field::new("pid", Type::One(Scalar::U32), true),
        Field::new("build_id", Type::One(Scalar::U64), true),
        Field::new("ratio", Type::One(Scalar::F64), true),
        Field::new("name", Type::One(Scalar::Text), true),
        Field::new("kind", Type::One(Scalar::Dict), false),
        Field::new("stack", Type::List(Scalar::Text), true),
        Field::new("pcs", Type::List(Scalar::U64), false),
    ]
}

fn texts(values: &[&str]) -> Texts<Global> {
    let mut out = Texts::new_in(Global);
    for value in values {
        out.push(value);
    }
    out
}

/// Three rows of every type, nulls and an empty list among them; `dict` keeps the kinds as a
/// dictionary column (as written) or as text (as read).
fn three(dict: bool) -> Batch<Global> {
    let kind = if dict {
        Column::dict([Some(1), Some(0), Some(1)], texts(&["a", "bb"]), Global).unwrap()
    } else {
        Column::texts(&["bb", "a", "bb"], Global)
    };
    let stack_offsets = Vec::from_slice_in(&[0, 2, 2, 2], Global);
    let stack = Column::list(
        stack_offsets,
        Some(Vec::from_slice_in(&[true, true, false], Global)),
        blaze_columns::Values::Text(texts(&["main", "f"])),
    )
    .unwrap();
    let pcs = Column::list(
        Vec::from_slice_in(&[0, 1, 1, 3], Global),
        None,
        blaze_columns::Values::U64(Vec::from_slice_in(&[1, u64::MAX, 7], Global)),
    )
    .unwrap();
    let columns = [
        Column::values([true, false, true], Global),
        Column::options([Some(-1), None, Some(i32::MAX)], Global),
        Column::values([i64::MIN, 0, 5], Global),
        Column::values([0u16, 7, u16::MAX], Global),
        Column::options([Some(u32::MAX), None, Some(1)], Global),
        Column::options([Some(u64::MAX), Some(1), None], Global),
        Column::options([Some(1.5), Some(-0.0), None], Global),
        Column::opt_texts(&[Some("x"), None, Some("")], Global),
        kind,
        stack,
        pcs,
    ];
    Batch::try_new(Vec::from_iter_in(fields(), Global), Vec::from_iter_in(columns, Global)).unwrap()
}

#[test]
fn a_file_reads_back_as_it_was_written() {
    let options = Options { row_group_rows: 2, page_rows: 1, ..Options::DEFAULT };
    let file = write(&fields(), &[three(true)], &[("blaze.schema", "5")], options, Global).unwrap();
    assert_eq!(&file[..4], b"PAR1");
    assert_eq!(&file[file.len() - 4..], b"PAR1");
    let read = File::open(&file, Global).unwrap();
    assert_eq!(read.value("blaze.schema"), Some("5"));
    assert_eq!((read.num_rows(), read.row_groups()), (3, 2));
    let batches = read.read_all().unwrap();
    let want = three(false);
    let mut row = 0;
    for batch in batches.iter() {
        for (at, column) in batch.columns().iter().enumerate() {
            for i in 0..batch.num_rows() {
                let want = want.column(at);
                assert_eq!(column.is_null(i), want.is_null(row + i), "{at} {i}");
                assert_eq!(column.int(i), want.int(row + i), "{at} {i}");
                assert_eq!(column.float(i).map(f64::to_bits), want.float(row + i).map(f64::to_bits));
                assert_eq!(column.text(i), want.text(row + i));
                if let (Some(got), Some(wanted)) = (column.lists(), want.lists()) {
                    assert_eq!(got.row(i).map(|it| it.len()), wanted.row(row + i).map(|it| it.len()));
                }
            }
        }
        row += batch.num_rows();
    }
    // Statistics, in each type's order: unsigned past i64 too.
    let build = read.column("build_id").unwrap();
    let stats = read.int_stats(0, build).unwrap();
    assert_eq!(stats.nulls, Some(0));
    assert_eq!(stats.range, Some((1, u64::MAX.into())));
    let stats = read.int_stats(1, build).unwrap();
    assert_eq!((stats.nulls, stats.range), (Some(1), None));
}

#[test]
fn the_same_batches_write_the_same_bytes() {
    let a = write(&fields(), &[three(true)], &[], Options::DEFAULT, Global).unwrap();
    let b = write(&fields(), &[three(true)], &[], Options::DEFAULT, Global).unwrap();
    assert_eq!(a.as_slice(), b.as_slice());
}

#[test]
fn an_empty_file_has_its_schema_and_no_row_group() {
    let file = write(&fields(), &[], &[], Options::DEFAULT, Global).unwrap();
    let read = File::open(&file, Global).unwrap();
    assert_eq!((read.num_rows(), read.row_groups()), (0, 0));
    let names: std::vec::Vec<_> = read.fields().iter().map(|it| it.name().to_owned()).collect();
    assert_eq!(names, fields().iter().map(|it| it.name().to_owned()).collect::<std::vec::Vec<_>>());
    assert_eq!(read.fields()[9].ty, Type::List(Scalar::Text));
}

#[test]
fn a_merge_holds_every_input_row_and_counts_them() {
    let options = Options { row_group_rows: 2, ..Options::DEFAULT };
    let a = write(&fields(), &[three(true)], &[("blaze.schema", "5"), ("rows", "9")], options, Global).unwrap();
    let b = write(&fields(), &[three(true)], &[("blaze.schema", "5")], options, Global).unwrap();
    let inputs = Vec::from_iter_in(
        [Input::open(a.as_slice(), &Global).unwrap(), Input::open(b.as_slice(), &Global).unwrap()],
        Global,
    );
    let merged = Concat::new(inputs, "rows", 1, Global).unwrap().all().unwrap();
    let read = File::open(&merged, Global).unwrap();
    assert_eq!((read.num_rows(), read.row_groups()), (6, 4));
    assert_eq!(read.value("rows"), Some("6"));
    assert_eq!(read.value("blaze.schema"), Some("5"));
    let all = read.read_all().unwrap();
    let names: std::vec::Vec<_> =
        all.iter().flat_map(|batch| (0..batch.num_rows()).map(|i| batch.column(7).text(i).map(str::to_owned))).collect();
    let one = [Some("x".to_owned()), None, Some(String::new())];
    assert_eq!(names, [one.clone(), one].concat());
}

#[test]
fn a_merge_of_another_schema_is_refused() {
    let a = write(&fields(), &[three(true)], &[], Options::DEFAULT, Global).unwrap();
    let other = [Field::new("flag", Type::One(Scalar::Bool), true)];
    let b = write(&other, &[], &[], Options::DEFAULT, Global).unwrap();
    let inputs = Vec::from_iter_in(
        [Input::open(a.as_slice(), &Global).unwrap(), Input::open(b.as_slice(), &Global).unwrap()],
        Global,
    );
    let err = Concat::new(inputs, "rows", 1 << 20, Global).unwrap().all().unwrap_err();
    assert_eq!((err.input, err.error), (Some(1), ParquetError::SchemaDiffers));
}

#[test]
fn a_cut_file_is_refused_not_read() {
    let file = write(&fields(), &[three(true)], &[], Options::DEFAULT, Global).unwrap();
    for cut in [0, 7, 11, file.len() / 2, file.len() - 1] {
        assert!(File::open(&file[..cut], Global).is_err(), "cut at {cut}");
    }
    // A footer that says its chunks lie elsewhere: the data before it cut out.
    let mut moved = file[..4].to_vec();
    let footer_len =
        usize::try_from(u32::from_le_bytes(file[file.len() - 8..file.len() - 4].try_into().unwrap())).unwrap();
    moved.extend_from_slice(&file[file.len() - 8 - footer_len..]);
    let read = File::open(&moved, Global).unwrap();
    assert!(read.read_all().is_err());
}
