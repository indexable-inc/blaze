//! Bytes-level tests of the Thrift encoder and the level encoding against vectors computed by
//! hand from the specs, and round trips of the writer through the reader and the merge.

use std::alloc::Global;

use blaze_columns::{Batch, Column, Field, Scalar, Texts, Type};
use blaze_own::Vec;

use blaze_columns::Values;
use blaze_int::Int;

use crate::{
    Compression, Concat, Encoding, File, Input, Options, ParquetError, delta, levels, lz4,
    thrift::{In, Struct, zigzag},
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
    assert_eq!([zigzag(0), zigzag(-1), zigzag(1), zigzag(-2), zigzag(i64::from(i32::MAX))], [0, 1, 2, 3, u64::from(u32::MAX) - 1]);
    assert_eq!(zigzag(i64::MIN), u64::MAX);
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
    levels::encode(&mut out, values.len(), width, |i| u32::from(values[i])).unwrap();
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
    let mut fields = fields();
    if !dict {
        fields[8] = Field::new("kind", Type::One(Scalar::Text), false);
    }
    Batch::try_new(Vec::from_iter_in(fields, Global), Vec::from_iter_in(columns, Global)).unwrap()
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
    let merged = Concat::new(inputs, "rows", 1, Compression::None, Global).unwrap().all().unwrap();
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
fn a_merge_recompresses_chunks_of_another_codec() {
    let want = [rows(&[many(3000)]), rows(&[many(3000)])].concat();
    for from in CODECS {
        for to in CODECS {
            let options = Options { row_group_rows: 1000, page_rows: 300, compression: from, ..Options::DEFAULT };
            let a = write(&fields(), &[many(3000)], &[], options, Global).unwrap();
            let b = write(&fields(), &[many(3000)], &[], Options { compression: to, ..options }, Global).unwrap();
            let inputs = Vec::from_iter_in(
                [Input::open(a.as_slice(), &Global).unwrap(), Input::open(b.as_slice(), &Global).unwrap()],
                Global,
            );
            let merged = Concat::new(inputs, "rows", 1 << 16, to, Global).unwrap().all().unwrap();
            let read = File::open(&merged, Global).unwrap();
            for group in read.meta().row_groups.iter() {
                let sum: i64 = group.columns.iter().map(|it| it.meta.as_ref().unwrap().total_compressed_size).sum();
                assert_eq!(group.total_compressed_size, Some(sum));
                assert!(group.columns.iter().all(|it| it.meta.as_ref().unwrap().codec == to.codec()));
            }
            assert_eq!(rows(&read.read_all().unwrap()), want, "{from:?} to {to:?}");
        }
    }
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
    let err = Concat::new(inputs, "rows", 1 << 20, Compression::None, Global).unwrap().all().unwrap_err();
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

fn delta_bytes(values: &[i64], narrow: bool) -> std::vec::Vec<u8> {
    let mut out = Vec::new_in(Global);
    delta::encode(&mut out, values.len(), narrow, |i| values[i]).unwrap();
    out.as_slice().to_vec()
}

fn delta_values(bytes: &[u8], most: usize) -> std::vec::Vec<i64> {
    let mut out = vec![];
    let (total, used) = delta::decode(bytes, most, |it| out.push(it)).unwrap();
    assert_eq!((total, used), (out.len(), bytes.len()));
    out
}

#[test]
fn delta_binary_packed_is_the_specs() {
    // The spec's 1..=5: header (128, 4, 5, zigzag 1 = 2), then one block: least delta 1
    // (zigzag 2), four widths of 0 and no miniblock bytes.
    let got = delta_bytes(&[1, 2, 3, 4, 5], false);
    assert_eq!(got, [0x80, 0x01, 0x04, 0x05, 0x02, 0x02, 0, 0, 0, 0]);
    assert_eq!(delta_values(&got, 5), [1, 2, 3, 4, 5]);
    // 7, 5, 3, 1, 2, 3, 4, 5: deltas -2 -2 -2 1 1 1 1, least -2 (zigzag 3), the rest 0 0 0 3 3 3 3
    // at width 2 in a whole miniblock of 32 (8 bytes), its last bytes zero.
    let got = delta_bytes(&[7, 5, 3, 1, 2, 3, 4, 5], false);
    assert_eq!(got[..10], [0x80, 0x01, 0x04, 0x08, 0x0E, 0x03, 2, 0, 0, 0]);
    assert_eq!(got[10..], [0b1100_0000, 0b0011_1111, 0, 0, 0, 0, 0, 0]);
    assert_eq!(delta_values(&got, 8), [7, 5, 3, 1, 2, 3, 4, 5]);
    // None, and one.
    assert_eq!(delta_values(&delta_bytes(&[], false), 0), [] as [i64; 0]);
    assert_eq!(delta_values(&delta_bytes(&[-9], false), 1), [-9]);
}

#[test]
fn delta_round_trips_across_blocks_and_extremes() {
    let mut seed = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let wide: std::vec::Vec<i64> =
        (0..1000).map(|i| if i % 97 == 0 { i64::MIN } else if i % 89 == 0 { i64::MAX } else { next().cast_signed() >> (i % 60) }).collect();
    assert_eq!(delta_values(&delta_bytes(&wide, false), wide.len()), wide);
    let rising: std::vec::Vec<i64> = (0..777).map(|i| 1_700_000_000_000_000_000 + i * 1_000 + (i % 13)).collect();
    let got = delta_bytes(&rising, false);
    assert!(got.len() < rising.len() * 2, "{} bytes for {} rising values", got.len(), rising.len());
    assert_eq!(delta_values(&got, rising.len()), rising);
    // INT32s wrap in 32 bits: the reader's 64-bit sums keep the right low half.
    let narrow: std::vec::Vec<i64> = (0..300).map(|i| i64::from(if i % 2 == 0 { i32::MIN } else { i32::MAX }) + i).collect();
    let back = delta_values(&delta_bytes(&narrow, true), narrow.len());
    let low: std::vec::Vec<i32> = back.iter().map(|it| it.truncate_to::<i32>()).collect();
    let want: std::vec::Vec<i32> = narrow.iter().map(|it| it.truncate_to::<i32>()).collect();
    assert_eq!(low, want);
    // More values than the page holds, and bytes cut short, are refused.
    let five = delta_bytes(&[1, 2, 3, 4, 50], false);
    assert!(delta::decode(&five, 4, |_| {}).is_err());
    assert!(delta::decode(&five[..five.len() - 1], 5, |_| {}).is_err());
}

#[test]
fn lz4_round_trips_and_refuses_what_lies() {
    let mut cases: std::vec::Vec<std::vec::Vec<u8>> = vec![vec![], b"a".to_vec(), b"abcdefghijkl".to_vec(), vec![7; 100_000]];
    cases.push((0..70_000u32).flat_map(|i| (i % 251).to_le_bytes()).collect());
    cases.push(b"crate::module::function_".repeat(500));
    let mut seed = 1u64;
    cases.push(
        (0..5000)
            .map(|_| {
                seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                (seed >> 56).truncate_to::<u8>()
            })
            .collect(),
    );
    for case in &cases {
        let mut packed = Vec::new_in(Global);
        lz4::compress(case, &mut packed);
        assert!(packed.len() <= lz4::bound(case.len()));
        let mut back = vec![0; case.len()];
        lz4::decompress(&packed, &mut back).unwrap();
        assert_eq!(&back, case);
        if case.len() > 1000 {
            let mut short = vec![0; case.len() - 1];
            assert!(lz4::decompress(&packed, &mut short).is_err());
            assert!(lz4::decompress(&packed[..packed.len() / 2], &mut back).is_err());
        }
    }
    let mut repeat = Vec::new_in(Global);
    lz4::compress(&[7; 100_000], &mut repeat);
    assert!(repeat.len() < 500, "{} bytes for 100 000 sevens", repeat.len());
    // An offset before the start.
    assert!(lz4::decompress(&[0x10, b'a', 0x05, 0x00], &mut [0; 5]).is_err());
}

/// Row `at` of `values` as text.
fn cell(values: &Values<Global>, at: usize) -> String {
    match values {
        Values::Bool(it) => it[at].to_string(),
        Values::I32(it) => it[at].to_string(),
        Values::U16(it) => it[at].to_string(),
        Values::U32(it) => it[at].to_string(),
        Values::I64(it) => it[at].to_string(),
        Values::U64(it) => it[at].to_string(),
        Values::F64(it) => format!("{:x}", it[at].to_bits()),
        Values::Text(it) => format!("{:?}", it.get(at)),
        Values::Dict { keys, values } => format!("{:?}", values.get(usize::try_from(keys[at]).unwrap())),
    }
}

/// Every row of `batches`, each column as text.
fn rows(batches: &[Batch<Global>]) -> std::vec::Vec<String> {
    let mut out = vec![];
    for batch in batches {
        for row in 0..batch.num_rows() {
            let mut line = String::new();
            for column in batch.columns() {
                if column.is_null(row) {
                    line.push_str("NULL");
                } else if let Some(lists) = column.lists() {
                    let range = lists.row(row).unwrap();
                    let items: std::vec::Vec<_> = range.map(|at| cell(lists.elements(), at)).collect();
                    line.push_str(&format!("{items:?}"));
                } else {
                    line.push_str(&cell(column.buffer(), row));
                }
                line.push('|');
            }
            out.push(line);
        }
    }
    out
}

/// `n` rows of every type, as a profile's: rising times, a few names, paths that share
/// prefixes, nulls and empty lists.
fn many(n: usize) -> Batch<Global> {
    let names: std::vec::Vec<String> = (0..n).map(|i| format!("blaze_query::passes::item_{}", (i * 7) % 40)).collect();
    let mut stack_texts = Texts::new_in(Global);
    let mut stack_offsets = Vec::new_in(Global);
    stack_offsets.push(0u32);
    let mut pcs = Vec::new_in(Global);
    let mut pc_offsets = Vec::new_in(Global);
    pc_offsets.push(0u32);
    for i in 0..n {
        for depth in 0..i % 4 {
            stack_texts.push(&format!("crates/blaze-front/src/f{}.rs", (i + depth) % 9));
        }
        stack_offsets.push(u32::try_from(stack_texts.len()).unwrap());
        for depth in 0..(i % 3) {
            pcs.push(0x1_0000_0000u64 + u64::try_from(i * 16 + depth).unwrap());
        }
        pc_offsets.push(u32::try_from(pcs.len()).unwrap());
    }
    let stack_valid: std::vec::Vec<bool> = (0..n).map(|i| i % 11 != 3).collect();
    let mut kinds = Texts::new_in(Global);
    for kind in ["span", "event", "counter"] {
        kinds.push(kind);
    }
    let columns = [
        Column::values((0..n).map(|i| i % 3 == 0 || i % 50 > 40), Global),
        Column::options((0..n).map(|i| (i % 5 != 0).then(|| i32::try_from(i * 7919 % 1000).unwrap() - 500)), Global),
        Column::values((0..n).map(|i| 1_700_000_000_000_000_000i64 + i64::try_from(i * 1_000 + i % 13).unwrap()), Global),
        Column::values((0..n).map(|i| u16::try_from(i % 7).unwrap()), Global),
        Column::options((0..n).map(|i| (i % 9 != 0).then_some(4242u32)), Global),
        Column::options((0..n).map(|i| (i % 17 != 0).then(|| u64::MAX - u64::try_from(i / 100).unwrap())), Global),
        Column::options((0..n).map(|i| (i % 6 != 0).then(|| f64::from(u32::try_from(i % 300).unwrap()) / 8.0)), Global),
        Column::opt_texts(&names.iter().enumerate().map(|(i, it)| (i % 13 != 0).then_some(it.as_str())).collect::<std::vec::Vec<_>>(), Global),
        Column::dict((0..n).map(|i| Some(u32::try_from(i % 3).unwrap())), kinds, Global).unwrap(),
        Column::list(stack_offsets, Some(Vec::from_iter_in(stack_valid, Global)), Values::Text(stack_texts)).unwrap(),
        Column::list(pc_offsets, None, Values::U64(pcs)).unwrap(),
    ];
    Batch::try_new(Vec::from_iter_in(fields(), Global), Vec::from_iter_in(columns, Global)).unwrap()
}

const ENCODINGS: [Encoding; 5] = [Encoding::Auto, Encoding::Plain, Encoding::Dictionary, Encoding::Delta, Encoding::DeltaLength];
const CODECS: [Compression; 3] = [Compression::None, Compression::Lz4, Compression::Zstd { level: 3 }];

#[test]
fn every_encoding_and_codec_reads_back_as_written() {
    let small = [three(true)];
    let big = [many(5000)];
    for encoding in ENCODINGS {
        for compression in CODECS {
            for (batches, page_rows) in [(&small[..], 1), (&big[..], 700)] {
                let options = Options { row_group_rows: 2000, page_rows, encoding, compression, ..Options::DEFAULT };
                let file = write(&fields(), batches, &[], options, Global).unwrap();
                let read = File::open(&file, Global).unwrap();
                let got = rows(&read.read_all().unwrap());
                assert_eq!(got, rows(batches), "{encoding:?} {compression:?} pages of {page_rows}");
            }
        }
    }
}

#[test]
fn auto_picks_the_encoding_each_column_suits() {
    let file = write(&fields(), &[many(5000)], &[], Options { row_group_rows: 10_000, ..Options::DEFAULT }, Global).unwrap();
    let read = File::open(&file, Global).unwrap();
    let mut got = vec![];
    for chunk in read.meta().row_groups[0].columns.iter() {
        let meta = chunk.meta.as_ref().unwrap();
        got.push((meta.path.last().unwrap().as_str().to_owned(), meta.encodings.as_slice().to_vec()));
    }
    let want: std::vec::Vec<(&str, std::vec::Vec<i32>)> = vec![
        ("flag", vec![3]),
        ("small", vec![3, 5]),
        ("t_ns", vec![3, 5]),
        ("depth", vec![3, 0, 8]),
        ("pid", vec![3, 0, 8]),
        // Slowly falling from u64::MAX: deltas of 0 and -1 pack smaller than a dictionary.
        ("build_id", vec![3, 5]),
        ("ratio", vec![3, 0, 8]),
        ("name", vec![3, 0, 8]),
        ("kind", vec![3, 0, 8]),
        ("element", vec![3, 0, 8]),
        ("element", vec![3, 5]),
    ];
    let got: std::vec::Vec<(&str, std::vec::Vec<i32>)> = got.iter().map(|(name, it)| (name.as_str(), it.clone())).collect();
    assert_eq!(got, want);
    // A dictionary past its bound gives way to the best encoding that is not one.
    let options = Options { row_group_rows: 10_000, page_rows: 500, dictionary_bytes: 16, ..Options::DEFAULT };
    let file = write(&fields(), &[many(5000)], &[], options, Global).unwrap();
    let read = File::open(&file, Global).unwrap();
    let name = read.meta().row_groups[0].columns[7].meta.as_ref().unwrap();
    assert!(!name.encodings.contains(&8), "{:?}", name.encodings.as_slice());
    assert_eq!(rows(&read.read_all().unwrap()), rows(&[many(5000)]));
}
