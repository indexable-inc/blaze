use std::mem::offset_of;

use blaze_v2_proof::{Rng, columns};

use super::{
    BufferColumns, COLD_HEADER, CORRUPT, Codec, ColdDictionary, HotColumn, HotRef, Packed,
    WarmColumn, WarmRef, Width, cold_codec, columns_of, compress, decode_cold, decode_cold_bytes,
    decode_raw, encode_cold, encode_cold_bytes, encode_raw, raw_cold,
};
use crate::{Aligned, FormatError, Record, Rel, Slice, Str, WordTable, Writer, leb, open, record, row_words};

const SEEDS: [u64; 4] = [1, 17, 4242, 0x9e37_79b9_7f4a_7c15];

record! {
    #[derive(Clone, Copy, Debug)]
    struct HotRoot: 1 {
        column: HotColumn,
    }
}

record! {
    #[derive(Clone, Copy, Debug)]
    struct WarmRoot: 1 {
        column: WarmColumn,
    }
}

fn hot_buffer(values: &[u32]) -> Aligned {
    let mut out = Writer::new(HotRoot::SCHEMA);
    let placed = out.push_hot(values);
    let at = out.next::<HotRoot>();
    let root = HotRoot { column: HotColumn::new(at + offset_of!(HotRoot, column) as u64, placed) };
    assert_eq!(out.push(&root), at);
    Aligned::copy_from(&out.finish(at))
}

fn warm_buffer(values: &[u32]) -> Aligned {
    let mut out = Writer::new(WarmRoot::SCHEMA);
    let placed = out.push_warm(values);
    let at = out.next::<WarmRoot>();
    let root =
        WarmRoot { column: WarmColumn::new(at + offset_of!(WarmRoot, column) as u64, placed) };
    assert_eq!(out.push(&root), at);
    Aligned::copy_from(&out.finish(at))
}

fn hot(buffer: &Aligned) -> Result<HotRef<'_>, FormatError> {
    open::<HotRoot>(buffer)?.column.follow(buffer)
}

fn warm(buffer: &Aligned) -> Result<WarmRef<'_>, FormatError> {
    open::<WarmRoot>(buffer)?.column.follow(buffer)
}

/// A column opened once and kept for the test's life, so reading it element by element does not
/// check the buffer again each time.
fn leaked(buffer: Aligned) -> &'static Aligned {
    Box::leak(Box::new(buffer))
}

#[test]
fn a_hot_column_round_trips_every_shape() {
    for seed in SEEDS {
        columns::round_trips(hot_buffer, |buffer| hot(buffer).map(|it| it.to_vec()), seed)
            .unwrap();
    }
}

#[test]
fn a_hot_column_reads_in_place() {
    for seed in SEEDS {
        columns::reads_in_place(
            |values| hot(leaked(hot_buffer(values))).unwrap(),
            |column, at| column.get(at),
            seed,
        )
        .unwrap();
    }
}

#[test]
fn a_hot_column_is_at_its_narrowest_width() {
    for seed in SEEDS {
        for (name, values) in columns::shapes(seed) {
            let buffer = hot_buffer(&values);
            let column = hot(&buffer).unwrap();
            assert_eq!(column.width(), Width::of(&values), "{name} of {}", values.len());
            assert_eq!(column.len(), values.len());
        }
    }
    assert_eq!(Width::of(&[]), Width::U8);
    assert_eq!(Width::of(&[0xff]), Width::U8);
    assert_eq!(Width::of(&[0x100]), Width::U16);
    assert_eq!(Width::of(&[0xffff]), Width::U16);
    assert_eq!(Width::of(&[0x1_0000]), Width::U32);
    let narrow = hot_buffer(&vec![200; 1000]);
    let wide = hot_buffer(&vec![70_000; 1000]);
    assert!(narrow.len() + 2500 < wide.len(), "{} against {}", narrow.len(), wide.len());
}

#[test]
fn a_hot_column_of_two_widths_is_refused() {
    let mut out = Writer::new(HotRoot::SCHEMA);
    let u8s = out.push_hot(&[1, 2, 3]);
    let u16s = out.push_hot(&[1000, 2000]);
    let at = out.next::<HotRoot>();
    let field = at + offset_of!(HotRoot, column) as u64;
    let mut column = HotColumn::new(field, u8s);
    column.u16s = HotColumn::new(field, u16s).u16s;
    assert_eq!(out.push(&HotRoot { column }), at);
    let buffer = Aligned::copy_from(&out.finish(at));
    assert!(matches!(
        hot(&buffer),
        Err(FormatError::Invalid { what: "a hot column of more than one width", .. })
    ));
}

#[test]
fn a_warm_column_round_trips_every_shape() {
    for seed in SEEDS {
        columns::round_trips(warm_buffer, |buffer| warm(buffer).map(|it| it.to_vec()), seed)
            .unwrap();
        columns::round_trips(Packed::pack, |packed| Ok::<_, FormatError>(packed.unpack()), seed)
            .unwrap();
    }
}

#[test]
fn a_warm_column_reads_in_place() {
    for seed in SEEDS {
        columns::reads_in_place(
            |values| warm(leaked(warm_buffer(values))).unwrap(),
            |column, at| column.get(at),
            seed,
        )
        .unwrap();
    }
}

#[test]
fn a_warm_column_unpacks_any_run() {
    let mut rng = Rng::new(7);
    for seed in SEEDS {
        for (name, values) in columns::shapes(seed) {
            let packed = Packed::pack(&values);
            let column = packed.as_ref();
            for _ in 0..16 {
                let start = rng.below(values.len() as u64 + 1) as usize;
                let len = rng.below((values.len() - start) as u64 + 1) as usize;
                let mut out = vec![0; len];
                column.unpack(start, &mut out);
                assert_eq!(out, &values[start..start + len], "{name} of {} at {start}", values.len());
            }
        }
    }
}

#[test]
fn a_warm_column_takes_the_bits_of_its_range() {
    for seed in SEEDS {
        for (name, values) in columns::shapes(seed) {
            let packed = Packed::pack(&values);
            let (min, max) = (
                values.iter().copied().min().unwrap_or(0),
                values.iter().copied().max().unwrap_or(0),
            );
            assert_eq!(packed.bits, u32::BITS - (max - min).leading_zeros(), "{name}");
            match packed.bits {
                32 => assert_eq!(packed.min, 0, "{name}"),
                bits => assert_eq!(packed.min, min.min(u32::MAX - ((1 << bits) - 1)), "{name}"),
            }
            assert_eq!(packed.words.len(), super::words_for(values.len(), packed.bits), "{name}");
        }
    }
    let far = Packed::pack(&[4_000_000_000, 4_000_000_003, 4_000_000_001]);
    assert_eq!((far.min, far.bits, far.words.len()), (4_000_000_000, 2, 2));
    let constant = Packed::pack(&[9; 100]);
    assert_eq!((constant.min, constant.bits, constant.words.len()), (9, 0, 0));
    assert_eq!(constant.unpack(), vec![9; 100]);
    let wide = Packed::pack(&[1, u32::MAX]);
    assert_eq!((wide.min, wide.bits), (0, 32));
    // Near the top, the minimum drops so that it plus any 4-bit offset is still a `u32`.
    let near = Packed::pack(&[0xffff_fff1, 0xffff_fff9]);
    assert_eq!((near.min, near.bits), (0xffff_fff0, 4));
    assert_eq!(near.unpack(), [0xffff_fff1, 0xffff_fff9]);
}

#[test]
fn a_warm_column_that_does_not_fit_its_words_is_refused() {
    let corrupt = |edit: fn(&mut WarmColumn)| {
        let mut out = Writer::new(WarmRoot::SCHEMA);
        let placed = out.push_warm(&[5, 6, 7, 1000]);
        let at = out.next::<WarmRoot>();
        let mut column = WarmColumn::new(at + offset_of!(WarmRoot, column) as u64, placed);
        edit(&mut column);
        assert_eq!(out.push(&WarmRoot { column }), at);
        let buffer = Aligned::copy_from(&out.finish(at));
        matches!(warm(&buffer), Err(FormatError::Invalid { what: "a warm column whose words don't fit it", .. }))
    };
    assert!(!corrupt(|_| {}));
    assert!(corrupt(|column| column.bits = 33));
    assert!(corrupt(|column| column.bits = 32));
    assert!(corrupt(|column| column.min = u32::MAX - 3));
    assert!(corrupt(|column| column.len = 1000));
    assert!(corrupt(|column| column.len = 0));
}

#[test]
fn a_cold_column_round_trips_every_shape() {
    for seed in SEEDS {
        columns::round_trips(|values| encode_cold(values, None), |bytes| decode_cold(bytes, None), seed)
            .unwrap();
    }
}

#[test]
fn every_codec_round_trips_every_shape_it_holds() {
    for seed in SEEDS {
        for (name, values) in columns::shapes(seed) {
            let sorted = values.windows(2).all(|pair| pair[0] <= pair[1]);
            for codec in Codec::ALL {
                let raw = encode_raw(&values, codec);
                assert_eq!(raw.is_some(), codec != Codec::Deltas || sorted, "{name} in {codec:?}");
                if let Some(raw) = raw {
                    assert_eq!(raw[0], codec as u8);
                    assert_eq!(decode_raw(&raw), Ok(values.clone()), "{name} of {} in {codec:?}", values.len());
                }
            }
        }
    }
}

#[test]
fn a_cold_column_takes_the_smallest_codec() {
    for seed in SEEDS {
        for (name, values) in columns::shapes(seed) {
            let chosen = raw_cold(&values);
            for codec in Codec::ALL {
                if let Some(raw) = encode_raw(&values, codec) {
                    assert!(chosen.len() <= raw.len(), "{name}: {:?} is smaller", codec);
                }
            }
        }
    }
    let mut rng = Rng::new(3);
    let tags: Vec<u32> = (0..4000).map(|i| (i / 100) as u32 % 3).collect();
    assert_eq!(cold_codec(&tags), Codec::Runs);
    let mut at = 0;
    let offsets: Vec<u32> = (0..4000)
        .map(|_| {
            at += 1_000 + rng.below(8) as u32;
            at
        })
        .collect();
    assert_eq!(cold_codec(&offsets), Codec::Deltas);
    let few = [3u32, 1 << 30, 7 << 20, u32::MAX - 5];
    let repeated: Vec<u32> = (0..4000).map(|_| few[rng.below(4) as usize]).collect();
    assert_eq!(cold_codec(&repeated), Codec::Dictionary);
    let uniform: Vec<u32> = (0..4000).map(|_| rng.next_u64() as u32).collect();
    assert_eq!(cold_codec(&uniform), Codec::Plain);
    assert!(encode_cold(&tags, None).len() < 128);
}

#[test]
fn every_codec_size_is_computed_exactly() {
    for seed in SEEDS {
        for (name, values) in columns::shapes(seed) {
            let shape = super::Shape::of(&values);
            for codec in Codec::ALL {
                assert_eq!(
                    shape.size(values.len(), codec),
                    encode_raw(&values, codec).map(|it| it.len()),
                    "{name} of {} in {codec:?}",
                    values.len()
                );
            }
        }
    }
}

#[test]
fn a_warm_block_is_in_simd_bp128_layout() {
    let values: Vec<u32> = (0..super::BLOCK_VALUES as u32 + 3).collect();
    let packed = Packed::pack(&values);
    assert_eq!((packed.min, packed.bits), (0, 8));
    let lanes = super::LANES;
    for lane in 0..lanes {
        for word in 0..8 {
            let expected = (0..4).fold(0u32, |acc, k| acc | ((4 * (4 * word + k) + lane) as u32) << (8 * k));
            assert_eq!(packed.words[word * lanes + lane], expected, "lane {lane} word {word}");
        }
    }
    let tail = &packed.words[8 * lanes..];
    assert_eq!(tail, &[128 | 129 << 8 | 130 << 16, 0]);
}

#[test]
fn equal_columns_are_equal_cold_bytes() {
    for (_, values) in columns::shapes(5) {
        assert_eq!(encode_cold(&values, None), encode_cold(&values.clone(), None));
    }
}

fn dictionary() -> ColdDictionary {
    let mut rng = Rng::new(11);
    let bases: Vec<Vec<u32>> = (0..20)
        .map(|_| (0..300).map(|_| [0, 1, 1, 2, 5, 9][rng.below(6) as usize]).collect())
        .collect();
    let samples: Vec<Vec<u8>> = (0..2000)
        .map(|_| {
            let mut tags = bases[rng.below(20) as usize].clone();
            for _ in 0..4 {
                let at = rng.below(tags.len() as u64) as usize;
                tags[at] = rng.below(10) as u32;
            }
            raw_cold(&tags)
        })
        .collect();
    ColdDictionary::train(&samples, 4096).unwrap()
}

#[test]
fn a_cold_column_round_trips_with_a_dictionary() {
    let dictionary = dictionary();
    assert_eq!(dictionary, ColdDictionary::new(dictionary.bytes.clone()));
    for seed in SEEDS {
        columns::round_trips(
            |values| encode_cold(values, Some(&dictionary)),
            |bytes| decode_cold(bytes, Some(&dictionary)),
            seed,
        )
        .unwrap();
    }
    let text = b"fn main() { println!(\"blaze\"); }".repeat(40);
    let cold = encode_cold_bytes(&text, Some(&dictionary));
    assert_eq!(decode_cold_bytes(&cold, Some(&dictionary)), Ok(text.clone()));
    assert_eq!(decode_cold_bytes(&encode_cold_bytes(&text, None), None), Ok(text));
}

#[test]
fn a_cold_column_is_read_only_with_its_dictionary() {
    let dictionary = dictionary();
    let other = ColdDictionary::new(dictionary.bytes.iter().rev().copied().collect());
    let values: Vec<u32> = (0..500).map(|i| i % 7).collect();
    let with = encode_cold(&values, Some(&dictionary));
    let without = encode_cold(&values, None);
    assert_eq!(
        decode_cold(&with, Some(&other)),
        Err(FormatError::Header("a cold column made with another dictionary"))
    );
    assert_eq!(
        decode_cold(&with, None),
        Err(FormatError::Header("a cold column made with a dictionary, read without"))
    );
    assert_eq!(
        decode_cold(&without, Some(&dictionary)),
        Err(FormatError::Header("a cold column made with another dictionary"))
    );
    assert_eq!(&with[1..33], &dictionary.id.0);
    assert_eq!(&without[1..33], &[0; 32]);
}

#[test]
fn a_corrupt_cold_column_is_refused() {
    let values: Vec<u32> = (0..1000).map(|i| i * 3).collect();
    let cold = encode_cold(&values, None);
    assert_eq!(decode_cold(&cold[..COLD_HEADER - 1], None), Err(FormatError::Header("a cold column shorter than its header")));
    let mut version = cold.clone();
    version[0] = super::COLD_VERSION + 1;
    assert_eq!(decode_cold(&version, None), Err(FormatError::Header("a cold column of another version")));
    let mut length = cold.clone();
    length[33..41].copy_from_slice(&(1u64 << 40).to_le_bytes());
    assert_eq!(decode_cold(&length, None), Err(CORRUPT));
    let mut short = cold.clone();
    let claimed = u64::from_le_bytes(short[33..41].try_into().unwrap());
    short[33..41].copy_from_slice(&(claimed - 1).to_le_bytes());
    assert_eq!(decode_cold(&short, None), Err(CORRUPT));
    assert!(decode_cold(&cold[..cold.len() - 1], None).is_err());

    let huge = |codec: Codec| {
        let mut raw = vec![codec as u8];
        leb::write_u64(&mut raw, 1 << 40);
        raw
    };
    for codec in Codec::ALL {
        assert_eq!(decode_raw(&huge(codec)), Err(CORRUPT), "{codec:?}");
        assert_eq!(decode_cold(&compress(&huge(codec), None), None), Err(CORRUPT));
    }
    let mut words = vec![Codec::Plain as u8];
    leb::write_u64(&mut words, 1 << 31);
    leb::write_u64(&mut words, 0);
    words.push(32);
    leb::write_u64(&mut words, 1 << 31);
    assert_eq!(decode_raw(&words), Err(CORRUPT));
    assert_eq!(decode_raw(&[9, 0]), Err(CORRUPT));
    assert_eq!(decode_raw(&[]), Err(CORRUPT));
    let mut trailing = raw_cold(&values);
    trailing.push(0);
    assert_eq!(decode_raw(&trailing), Err(CORRUPT));
}

#[test]
fn a_flipped_byte_never_panics_the_decoder() {
    for (name, values) in columns::shapes(23).into_iter().filter(|(_, it)| it.len() <= 129) {
        for codec in Codec::ALL {
            let Some(raw) = encode_raw(&values, codec) else { continue };
            for at in 0..raw.len() {
                for flip in [0x01, 0x80, 0xff] {
                    let mut bad = raw.clone();
                    bad[at] ^= flip;
                    if let Ok(back) = decode_raw(&bad) {
                        assert!(back.len() as u64 <= super::MOST_VALUES, "{name} in {codec:?} at {at}");
                    }
                }
            }
        }
    }
}

#[test]
fn the_tier_records_have_their_schemas() {
    assert_ne!(HotColumn::SCHEMA, WarmColumn::SCHEMA);
    assert_eq!(size_of::<HotColumn>(), 24);
    assert_eq!(size_of::<WarmColumn>(), 20);
}

record! {
    #[derive(Clone, Copy, Debug)]
    struct Node: 1 {
        id: u32,
        flag: bool,
        pad: [u8; 3],
        words: Slice<u32>,
        name: Str,
    }
}

record! {
    #[derive(Clone, Copy, Debug)]
    struct Tree: 1 {
        nodes: Slice<Node>,
        sizes: Slice<u32>,
        /// The words of `sizes` from the second on: one column with it.
        again: Slice<u32>,
        first: Rel<Node>,
        wide: Slice<u64>,
        /// The first four words of `wide`, two a row: one table with it, a word a row.
        pairs: Slice<[u32; 2]>,
    }
}

/// A tree of `nodes` nodes from `rng`, and where its word tables are: the nodes (six words a
/// row), each node's words that are not empty, `sizes`, and `wide`.
fn tree_buffer(rng: &mut Rng, nodes: usize) -> (Aligned, Vec<WordTable>) {
    let mut out = Writer::new(Tree::SCHEMA);
    let mut expected = Vec::new();
    let mut placed = Vec::new();
    for n in 0..nodes {
        let len = rng.below(300) as usize;
        let bits = rng.below(32);
        let spread = 1 + rng.below(1 << bits) as u32;
        let words: Vec<u32> = (0..len).map(|_| rng.below(spread as u64) as u32).collect();
        let at = out.push_slice(&words);
        if len > 0 {
            expected.push(WordTable { at: at as usize, rows: len, stride: 1 });
        }
        let name = format!("node {n}");
        placed.push((at, len, out.push_str(&name), name.len()));
    }
    let sizes: Vec<u32> = (0..nodes as u32 + 2).map(|it| it * 7).collect();
    let sizes_at = out.push_slice(&sizes);
    expected.push(WordTable { at: sizes_at as usize, rows: sizes.len(), stride: 1 });
    let wide_at = out.push_slice(&[u64::MAX, 3, 1 << 40]);
    expected.push(WordTable { at: wide_at as usize, rows: 6, stride: 1 });
    let nodes_at = out.next::<Node>();
    if nodes > 0 {
        const { assert!(size_of::<Node>() == 24) };
        expected.push(WordTable { at: nodes_at as usize, rows: nodes, stride: 6 });
    }
    let nodes: Vec<Node> = placed
        .iter()
        .enumerate()
        .map(|(n, &(words, len, name, name_len))| {
            let node_at = nodes_at + (n * size_of::<Node>()) as u64;
            Node {
                id: n as u32,
                flag: n % 2 == 0,
                pad: [0; 3],
                words: Slice::between(node_at + offset_of!(Node, words) as u64, words, len),
                name: Slice::between(node_at + offset_of!(Node, name) as u64, name, name_len),
            }
        })
        .collect();
    assert_eq!(out.push_slice(&nodes), nodes_at);
    let at = out.next::<Tree>();
    let field = |offset: usize| at + offset as u64;
    let root = Tree {
        nodes: Slice::between(field(offset_of!(Tree, nodes)), nodes_at, nodes.len()),
        sizes: Slice::between(field(offset_of!(Tree, sizes)), sizes_at, sizes.len()),
        again: Slice::between(field(offset_of!(Tree, again)), sizes_at + 4, sizes.len() - 1),
        first: match nodes.is_empty() {
            true => Rel::NONE,
            false => Rel::between(field(offset_of!(Tree, first)), nodes_at),
        },
        wide: Slice::between(field(offset_of!(Tree, wide)), wide_at, 3),
        pairs: Slice::between(field(offset_of!(Tree, pairs)), wide_at, 2),
    };
    assert_eq!(out.push(&root), at);
    expected.sort_unstable();
    (Aligned::copy_from(&out.finish(at)), expected)
}

#[test]
fn a_buffer_s_word_columns_are_found_from_its_schema() {
    const { assert!(Tree::COLUMNS && Node::COLUMNS && !<[u32; 4] as Record>::COLUMNS) };
    const { assert!(row_words::<Node>() == 6 && row_words::<u64>() == 2 && row_words::<u8>() == 0) };
    let mut rng = Rng::new(5);
    for nodes in [0, 1, 2, 40] {
        let (buffer, expected) = tree_buffer(&mut rng, nodes);
        assert_eq!(columns_of::<Tree>(&buffer, Tree::SCHEMA), Ok(expected), "{nodes} nodes");
    }
    let (buffer, _) = tree_buffer(&mut rng, 3);
    assert!(matches!(columns_of::<Tree>(&buffer, Node::SCHEMA), Err(FormatError::Schema { .. })));
}

#[test]
fn a_buffer_round_trips_warm_and_cold_exactly() {
    let forms = BufferColumns::of::<Tree>(Tree::SCHEMA);
    for seed in SEEDS {
        let mut rng = Rng::new(seed);
        for nodes in [0, 1, 7, 60] {
            let (buffer, _) = tree_buffer(&mut rng, nodes);
            let warm = forms.warm(&buffer).unwrap();
            assert_eq!(forms.hot_from_warm(&warm).unwrap(), &buffer[..], "{seed} {nodes} warm");
            let cold = forms.cold(&buffer).unwrap();
            assert_eq!(forms.hot_from_cold(&cold).unwrap(), &buffer[..], "{seed} {nodes} cold");
            assert!(cold.len() < buffer.len() || nodes < 60, "{seed} {nodes}: {} cold bytes", cold.len());
            let mut unaligned = vec![0u8];
            unaligned.extend_from_slice(&buffer);
            assert_eq!(forms.warm(&unaligned[1..]).unwrap(), warm, "{seed} {nodes} unaligned");
        }
    }
}

#[test]
fn a_buffer_round_trips_cold_with_a_dictionary_and_only_with_it() {
    let mut rng = Rng::new(21);
    let samples: Vec<Vec<u8>> = (0..300).map(|_| tree_buffer(&mut rng, 3).0.to_vec()).collect();
    let dictionary = ColdDictionary::train(&samples, 8 << 10).unwrap();
    let forms = BufferColumns::of::<Tree>(Tree::SCHEMA).with_dictionary(dictionary);
    let (buffer, _) = tree_buffer(&mut rng, 5);
    let cold = forms.cold(&buffer).unwrap();
    assert_eq!(forms.hot_from_cold(&cold).unwrap(), &buffer[..]);
    assert!(BufferColumns::of::<Tree>(Tree::SCHEMA).hot_from_cold(&cold).is_err());
}

#[test]
fn a_damaged_buffer_form_is_refused_or_decodes_never_panicking() {
    let forms = BufferColumns::of::<Tree>(Tree::SCHEMA);
    let (buffer, _) = tree_buffer(&mut Rng::new(8), 4);
    let warm = forms.warm(&buffer).unwrap();
    for at in 0..warm.len() {
        for flip in [0x01, 0x80, 0xff] {
            let mut bad = warm.clone();
            bad[at] ^= flip;
            if let Ok(back) = forms.hot_from_warm(&bad) {
                assert!(back.len() <= 2 * buffer.len() + (1 << 16), "a flip at {at}");
            }
        }
        assert!(forms.hot_from_warm(&warm[..at]).is_err(), "cut at {at}");
    }
    let mut longer = warm.clone();
    longer.push(0);
    assert!(forms.hot_from_warm(&longer).is_err());
    assert!(forms.hot_from_cold(&warm).is_err(), "a warm form read as cold");
}
