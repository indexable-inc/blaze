//! The codec held to [`super::reference`], the one it replaced: the same bytes for every value
//! of every shape serde gives, and the same value or the same error for any bytes.

use std::fmt;

use blaze_ids::rng::SplitMix64;
use serde::{
    Deserialize, Serialize,
    de::{self, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor},
    ser::{
        SerializeMap, SerializeSeq, SerializeStruct, SerializeStructVariant, SerializeTuple,
        Serializer,
    },
};

use super::{Error, Writer, append, from_slice, key_order, name_order, reference, to_vec};

mod properties;

/// Shapes and bytes from a seed, the same in every run.
struct Random(SplitMix64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0.next_u64()
    }

    fn below(&mut self, n: u64) -> u64 {
        self.0.below(n)
    }

    fn text(&mut self) -> String {
        let len = self.below(12);
        (0..len)
            .map(|_| ['a', 'Z', '0', ' ', '\u{e9}', '\u{6c34}', '\u{1f525}'][self.below(7) as usize])
            .collect()
    }

    fn shuffle<T>(&mut self, items: &mut [T]) {
        self.0.shuffle(items);
    }
}

/// Field and variant names, in no order: some in their encoding's, most not.
const NAMES: [&str; 12] =
    ["schema", "id", "body", "a", "b", "aa", "ok", "kind", "mode", "path", "bytes", "packages"];

/// Every shape serde hands a serializer, said as a test wants it said.
#[derive(Clone, Debug)]
enum Shape {
    Unsigned(u64),
    Signed(i64),
    Wide(i128),
    Float(f64),
    Bool(bool),
    Null,
    Text(String),
    Bytes(Vec<u8>),
    /// Its length said up front, or not.
    Seq { said: bool, items: Vec<Shape> },
    Tuple(Vec<Shape>),
    /// The length it says, which may not be its own; its entries in this order.
    Map { said: Option<usize>, entries: Vec<(Shape, Shape)> },
    Struct(Vec<(usize, Shape)>),
    /// A unit variant, or a newtype one.
    Variant { name: usize, of: Option<Box<Shape>> },
    StructVariant { name: usize, fields: Vec<(usize, Shape)> },
}

impl Serialize for Shape {
    fn serialize<S: Serializer>(&self, to: S) -> Result<S::Ok, S::Error> {
        match self {
            Shape::Unsigned(n) => to.serialize_u64(*n),
            Shape::Signed(n) => to.serialize_i64(*n),
            Shape::Wide(n) => to.serialize_i128(*n),
            Shape::Float(x) => to.serialize_f64(*x),
            Shape::Bool(value) => to.serialize_bool(*value),
            Shape::Null => to.serialize_none(),
            Shape::Text(text) => to.serialize_str(text),
            Shape::Bytes(bytes) => to.serialize_bytes(bytes),
            Shape::Seq { said, items } => {
                let mut seq = to.serialize_seq(said.then_some(items.len()))?;
                for item in items {
                    seq.serialize_element(item)?;
                }
                seq.end()
            }
            Shape::Tuple(items) => {
                let mut tuple = to.serialize_tuple(items.len())?;
                for item in items {
                    tuple.serialize_element(item)?;
                }
                tuple.end()
            }
            Shape::Map { said, entries } => {
                let mut map = to.serialize_map(*said)?;
                for (key, value) in entries {
                    map.serialize_entry(key, value)?;
                }
                map.end()
            }
            Shape::Struct(fields) => {
                let mut map = to.serialize_struct("Shape", fields.len())?;
                for (name, value) in fields {
                    map.serialize_field(NAMES[*name], value)?;
                }
                map.end()
            }
            Shape::Variant { name, of: None } => {
                to.serialize_unit_variant("Shape", *name as u32, NAMES[*name])
            }
            Shape::Variant { name, of: Some(of) } => {
                to.serialize_newtype_variant("Shape", *name as u32, NAMES[*name], &**of)
            }
            Shape::StructVariant { name, fields } => {
                let mut map =
                    to.serialize_struct_variant("Shape", *name as u32, NAMES[*name], fields.len())?;
                for (field, value) in fields {
                    map.serialize_field(NAMES[*field], value)?;
                }
                map.end()
            }
        }
    }
}

fn count(random: &mut Random) -> usize {
    match random.below(40) {
        0 => 24 + random.below(300) as usize,
        _ => random.below(6) as usize,
    }
}

/// Keys of a map: distinct ones in a random order for most, any for a few (so some twice).
fn keys(random: &mut Random, n: usize) -> Vec<Shape> {
    if random.below(100) == 0 {
        return (0..n)
            .map(|_| match random.below(3) {
                0 => Shape::Unsigned(random.below(8)),
                1 => Shape::Text(NAMES[random.below(4) as usize].to_owned()),
                _ => Shape::Signed(-(random.below(4) as i64) - 1),
            })
            .collect();
    }
    let mut keys: Vec<Shape> = (0..n)
        .map(|at| match at % 4 {
            0 => Shape::Unsigned(at as u64 * 1000),
            1 => Shape::Signed(-(at as i64)),
            2 => Shape::Text(format!("{}{at}", random.text())),
            _ => Shape::Tuple(vec![Shape::Unsigned(at as u64), Shape::Null]),
        })
        .collect();
    random.shuffle(&mut keys);
    keys
}

/// Fields of a struct: distinct names in a random order, now and then one twice.
fn fields(random: &mut Random, depth: u32) -> Vec<(usize, Shape)> {
    let mut names: Vec<usize> = (0..NAMES.len()).collect();
    random.shuffle(&mut names);
    names.truncate(random.below(7) as usize);
    if random.below(100) == 0
        && let Some(&first) = names.first()
    {
        names.push(first);
    }
    names.into_iter().map(|name| (name, shape(random, depth - 1))).collect()
}

fn shape(random: &mut Random, depth: u32) -> Shape {
    let leaf = depth == 0 || random.below(3) == 0;
    let kind = if leaf { random.below(8) } else { 8 + random.below(6) };
    match kind {
        0 => Shape::Unsigned(random.next() >> random.below(64)),
        1 => Shape::Signed(random.next() as i64 >> random.below(64)),
        // Now and then one CBOR has no integer for.
        2 => Shape::Wide(match random.below(256) {
            0 => [i128::MIN, 1 << 64, -(1 << 64) - 1][random.below(3) as usize],
            1..=40 => -(1i128 << 64),
            41..=80 => (1i128 << 64) - 1,
            _ => i128::from(random.next() as i64) * 3,
        }),
        3 => Shape::Float(match random.below(4) {
            0 => f64::from_bits(random.next()),
            1 => f64::from(random.next() as u16) / 8.0,
            2 => f64::from(f32::from_bits(random.next() as u32)),
            _ => [f64::NAN, f64::INFINITY, -0.0, 65504.0, 5.960_464_477_539_063e-8]
                [random.below(5) as usize],
        }),
        4 => Shape::Bool(random.below(2) == 0),
        5 => Shape::Null,
        6 => Shape::Text(random.text()),
        7 => Shape::Bytes((0..random.below(40)).map(|_| random.next() as u8).collect()),
        8 => {
            let items = (0..count(random)).map(|_| shape(random, depth - 1)).collect();
            Shape::Seq { said: random.below(2) == 0, items }
        }
        9 => Shape::Tuple((0..random.below(5)).map(|_| shape(random, depth - 1)).collect()),
        10 => {
            let n = count(random);
            let said = match random.below(3) {
                0 => None,
                1 => Some(n),
                _ => Some(random.below(30) as usize),
            };
            let keys = keys(random, n);
            let entries = keys.into_iter().map(|key| (key, shape(random, depth - 1))).collect();
            Shape::Map { said, entries }
        }
        11 => Shape::Struct(fields(random, depth)),
        12 => {
            let name = random.below(NAMES.len() as u64) as usize;
            let of = (random.below(2) == 0).then(|| Box::new(shape(random, depth - 1)));
            Shape::Variant { name, of }
        }
        _ => {
            let name = random.below(NAMES.len() as u64) as usize;
            Shape::StructVariant { name, fields: fields(random, depth) }
        }
    }
}

/// Any item, as `deserialize_any` gives it.
#[derive(Debug, PartialEq)]
enum Tree {
    Unsigned(u64),
    Signed(i64),
    Wide(i128),
    /// Its bits.
    Float(u64),
    Bool(bool),
    Null,
    Text(String),
    Bytes(Vec<u8>),
    Seq(Vec<Tree>),
    Map(Vec<(Tree, Tree)>),
}

impl<'de> Deserialize<'de> for Tree {
    fn deserialize<D: Deserializer<'de>>(from: D) -> Result<Tree, D::Error> {
        from.deserialize_any(Trees)
    }
}

struct Trees;

impl<'de> Visitor<'de> for Trees {
    type Value = Tree;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("any item")
    }

    fn visit_bool<E: de::Error>(self, value: bool) -> Result<Tree, E> {
        Ok(Tree::Bool(value))
    }

    fn visit_u64<E: de::Error>(self, n: u64) -> Result<Tree, E> {
        Ok(Tree::Unsigned(n))
    }

    fn visit_i64<E: de::Error>(self, n: i64) -> Result<Tree, E> {
        Ok(Tree::Signed(n))
    }

    fn visit_i128<E: de::Error>(self, n: i128) -> Result<Tree, E> {
        Ok(Tree::Wide(n))
    }

    fn visit_f64<E: de::Error>(self, x: f64) -> Result<Tree, E> {
        Ok(Tree::Float(x.to_bits()))
    }

    fn visit_unit<E: de::Error>(self) -> Result<Tree, E> {
        Ok(Tree::Null)
    }

    fn visit_str<E: de::Error>(self, text: &str) -> Result<Tree, E> {
        Ok(Tree::Text(text.to_owned()))
    }

    fn visit_bytes<E: de::Error>(self, bytes: &[u8]) -> Result<Tree, E> {
        Ok(Tree::Bytes(bytes.to_vec()))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Tree, A::Error> {
        let mut items = Vec::new();
        while let Some(item) = seq.next_element()? {
            items.push(item);
        }
        Ok(Tree::Seq(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Tree, A::Error> {
        let mut entries = Vec::new();
        while let Some(entry) = map.next_entry()? {
            entries.push(entry);
        }
        Ok(Tree::Map(entries))
    }
}

/// A type that reads some fields and passes over the rest.
#[derive(Deserialize, Debug, PartialEq)]
struct Typed {
    id: Option<u64>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    body: Option<Tree>,
}

/// Holds the two decoders to the same answer for `bytes`, as a tree, typed, ignored, and as a
/// tuple that reads only part of an array.
#[track_caller]
fn same_reading(bytes: &[u8]) {
    assert_eq!(from_slice::<Tree>(bytes), reference::from_slice::<Tree>(bytes), "{bytes:02x?}");
    assert_eq!(from_slice::<Typed>(bytes), reference::from_slice::<Typed>(bytes), "{bytes:02x?}");
    assert_eq!(
        from_slice::<IgnoredAny>(bytes).map(|_| ()),
        reference::from_slice::<IgnoredAny>(bytes).map(|_| ()),
        "{bytes:02x?}"
    );
    assert_eq!(
        from_slice::<(Tree, Option<u8>)>(bytes),
        reference::from_slice::<(Tree, Option<u8>)>(bytes),
        "{bytes:02x?}"
    );
}

#[test]
fn every_shape_is_the_bytes_it_was() {
    let mut random = Random(SplitMix64::new(0x9e37_79b9_7f4a_7c15));
    let (mut written, mut refused) = (0, 0);
    for _ in 0..3000 {
        let shape = shape(&mut random, 4);
        let new = to_vec(&shape);
        assert_eq!(new, reference::to_vec(&shape), "{shape:?}");
        match new {
            Ok(bytes) => {
                written += 1;
                super::check(&bytes).unwrap();
                same_reading(&bytes);
                assert!(from_slice::<Tree>(&bytes).is_ok());
            }
            Err(_) => refused += 1,
        }
    }
    assert!(written > 300 && refused > 0, "{written} written, {refused} refused");
}

/// One writer for many values, each onto the end of one buffer: the same bytes as each on its
/// own, and a value with no encoding leaves the buffer as it was.
#[test]
fn a_kept_writer_writes_what_a_new_one_does() {
    let mut random = Random(SplitMix64::new(0x2545_f491_4f6c_dd1d));
    let mut writer = Writer::new();
    let (mut out, mut expected) = (vec![0xee], vec![0xee]);
    for _ in 0..1000 {
        let shape = shape(&mut random, 4);
        let before = out.len();
        match (writer.append(&mut out, &shape), reference::to_vec(&shape)) {
            (Ok(()), Ok(bytes)) => expected.extend(bytes),
            (Err(new), Err(old)) => {
                assert_eq!(new, old);
                assert_eq!(out.len(), before);
            }
            (new, old) => panic!("{shape:?}: {new:?} against {old:?}"),
        }
        let mut alone = vec![0xee];
        match append(&mut alone, &shape) {
            Ok(()) => assert_eq!(alone[1..], out[before..]),
            Err(_) => assert_eq!(alone, [0xee]),
        }
    }
    assert_eq!(out, expected);
    assert!(writer.permuted() > 0);
}

/// A struct declared in its keys' order is written as it comes; one declared otherwise is put
/// in order, to the same bytes.
#[test]
fn only_maps_out_of_order_are_put_in_order() {
    #[derive(Serialize)]
    struct InOrder {
        id: u8,
        body: u8,
        schema: u8,
    }
    #[derive(Serialize)]
    struct OutOfOrder {
        schema: u8,
        id: u8,
        body: u8,
    }
    let mut writer = Writer::new();
    let mut first = Vec::new();
    writer.append(&mut first, &InOrder { id: 2, body: 3, schema: 1 }).unwrap();
    assert_eq!(writer.permuted(), 0);
    let mut second = Vec::new();
    writer.append(&mut second, &OutOfOrder { schema: 1, id: 2, body: 3 }).unwrap();
    assert_eq!(writer.permuted(), 1);
    assert_eq!(first, second);
    assert_eq!(first, reference::to_vec(&OutOfOrder { schema: 1, id: 2, body: 3 }).unwrap());
}

/// A length not said up front is written as it is at the end, in its shortest head, however
/// wide that is.
#[test]
fn unsaid_lengths_get_their_shortest_heads() {
    for n in [0usize, 1, 23, 24, 255, 256, 65_535, 65_536] {
        let seq = Shape::Seq { said: false, items: vec![Shape::Null; n] };
        assert_eq!(to_vec(&seq), reference::to_vec(&seq), "{n}");
        let reversed = (0..n).rev().map(|at| (Shape::Unsigned(at as u64), Shape::Bool(true)));
        for said in [None, Some(0), Some(n), Some(n + 30_000)] {
            let map = Shape::Map { said, entries: reversed.clone().collect() };
            assert_eq!(to_vec(&map), reference::to_vec(&map), "{n} {said:?}");
        }
        let nested = Shape::Seq {
            said: false,
            items: vec![Shape::Map { said: None, entries: reversed.clone().take(30).collect() }; 3],
        };
        assert_eq!(to_vec(&nested), reference::to_vec(&nested), "{n}");
    }
}

#[test]
fn what_has_no_encoding_is_refused_as_it_was() {
    struct Short;
    impl Serialize for Short {
        fn serialize<S: Serializer>(&self, to: S) -> Result<S::Ok, S::Error> {
            let mut seq = to.serialize_seq(Some(3))?;
            seq.serialize_element(&1)?;
            seq.serialize_element(&2)?;
            seq.end()
        }
    }
    let twice_in_a_row = Shape::Struct(vec![(1, Shape::Null), (1, Shape::Null)]);
    let twice_apart =
        Shape::Struct(vec![(0, Shape::Null), (1, Shape::Null), (0, Shape::Null)]);
    let twice_then_wide = Shape::Struct(vec![
        (1, Shape::Null),
        (1, Shape::Null),
        (2, Shape::Wide(i128::MAX)),
    ]);
    assert_eq!(to_vec(&Short), reference::to_vec(&Short));
    assert!(to_vec(&Short).is_err());
    for shape in [twice_in_a_row, twice_apart, twice_then_wide] {
        let new = to_vec(&shape);
        assert!(new.is_err(), "{shape:?}");
        assert_eq!(new, reference::to_vec(&shape), "{shape:?}");
        let mut out = vec![1, 2, 3];
        assert!(append(&mut out, &shape).is_err());
        assert_eq!(out, [1, 2, 3]);
    }
}

fn mutate(random: &mut Random, bytes: &mut Vec<u8>) {
    for _ in 0..=random.below(3) {
        match random.below(5) {
            0 if !bytes.is_empty() => {
                let at = random.below(bytes.len() as u64) as usize;
                bytes[at] ^= 1 << random.below(8);
            }
            1 if !bytes.is_empty() => {
                let at = random.below(bytes.len() as u64) as usize;
                bytes[at] = random.next() as u8;
            }
            2 => bytes.truncate(random.below(bytes.len() as u64 + 1) as usize),
            3 => {
                let at = random.below(bytes.len() as u64 + 1) as usize;
                bytes.insert(at, random.next() as u8);
            }
            _ if bytes.len() >= 2 => {
                let at = random.below(bytes.len() as u64 - 1) as usize;
                bytes.swap(at, at + 1);
            }
            _ => bytes.push(0),
        }
    }
}

/// Bytes cut, flipped, grown and swapped decode to what the two passes made of them, error
/// for error.
#[test]
fn any_bytes_read_as_the_two_passes_read_them() {
    let mut random = Random(SplitMix64::new(0x0123_4567_89ab_cdef));
    for _ in 0..4000 {
        let shape = shape(&mut random, 3);
        let Ok(mut bytes) = to_vec(&shape) else { continue };
        mutate(&mut random, &mut bytes);
        same_reading(&bytes);
    }
    let cases: [&[u8]; 9] = [
        &[],
        &[0x1c],
        &[0x9f, 0xff],
        &[0xa2, 0x61, 0x62, 0x01, 0x61, 0x61, 0x02],
        &[0xa2, 0x61, 0x61, 0x01, 0x61, 0x61, 0x02],
        &[0x83, 0x01, 0x02, 0x18, 0x01],
        &[0x82, 0xa1, 0x62, 0x69, 0x64, 0x01, 0xf6, 0x00],
        &[0xc1, 0x00],
        &[0xfa, 0x3f, 0x80, 0x00, 0x00],
    ];
    for bytes in cases {
        same_reading(bytes);
    }
}

/// The depth limit holds where the type reads the item and where it passes over it.
#[test]
fn nesting_is_refused_past_the_same_depth() {
    #[derive(Serialize, Deserialize, Debug, PartialEq)]
    struct Deep {
        inner: Option<Box<Deep>>,
    }
    for depth in [250usize, 255, 256, 257, 258, 300] {
        let mut arrays = vec![0x81; depth];
        arrays.push(0xf6);
        same_reading(&arrays);
        let mut deep = Deep { inner: None };
        for _ in 0..depth {
            deep = Deep { inner: Some(Box::new(deep)) };
        }
        let bytes = to_vec(&deep).unwrap();
        assert_eq!(Ok(bytes.clone()), reference::to_vec(&deep));
        assert_eq!(from_slice::<Deep>(&bytes), reference::from_slice::<Deep>(&bytes), "{depth}");
        same_reading(&bytes);
    }
}

/// Text and byte strings are read where they lie, for a type that borrows them; an unknown
/// field is passed over.
#[test]
fn strings_are_borrowed_from_the_bytes() {
    #[derive(Deserialize)]
    struct Borrowed<'a> {
        path: &'a str,
        bytes: &'a [u8],
    }
    let shape = Shape::Struct(vec![
        (9, Shape::Text("src/lib.rs".to_owned())),
        (10, Shape::Bytes(vec![1, 2, 3])),
        (1, Shape::Unsigned(7)),
    ]);
    let bytes = to_vec(&shape).unwrap();
    let read = from_slice::<Borrowed<'_>>(&bytes).unwrap();
    let within = bytes.as_ptr_range();
    assert!(within.contains(&read.path.as_ptr()) && within.contains(&read.bytes.as_ptr()));
    assert_eq!((read.path, read.bytes), ("src/lib.rs", &[1u8, 2, 3][..]));
}

/// A list read in place holds the bytes it was read from, gives the items a `Vec` gives, and is
/// refused where a `Vec` is, by both codecs; JSON lends no item's bytes.
#[test]
fn a_list_is_read_where_it_lies_and_refused_as_a_vec_is() {
    use super::{List, Raw};

    #[derive(Deserialize)]
    struct Lent<'a> {
        #[serde(borrow)]
        names: List<'a, &'a str>,
        #[serde(borrow)]
        rows: List<'a, List<'a, u8>>,
    }
    #[derive(Serialize, Deserialize, Debug, PartialEq)]
    struct Owned {
        rows: Vec<Vec<u8>>,
        names: Vec<String>,
    }
    #[derive(Serialize)]
    struct Other<N> {
        rows: Vec<Vec<u8>>,
        names: N,
    }
    let mut random = Random(SplitMix64::new(0x6c69_7374));
    for _ in 0..300 {
        let names = (0..random.below(30)).map(|_| random.text()).collect();
        let rows = (0..random.below(4))
            .map(|_| (0..random.below(30)).map(|_| random.below(256) as u8).collect())
            .collect();
        let bytes = to_vec(&Owned { rows, names }).unwrap();
        let owned = from_slice::<Owned>(&bytes).unwrap();
        let lent = from_slice::<Lent<'_>>(&bytes).unwrap();
        assert_eq!(lent.names.iter().map(str::to_owned).collect::<Vec<_>>(), owned.names);
        let rows: Vec<Vec<u8>> = lent.rows.iter().map(|row| row.iter().collect()).collect();
        assert_eq!(rows, owned.rows);
        assert_eq!((lent.names.len(), lent.rows.len()), (owned.names.len(), owned.rows.len()));
        let within = bytes.as_ptr_range();
        assert!(lent.names.iter().all(|it| it.is_empty() || within.contains(&it.as_ptr())));
        let old = reference::from_slice::<Lent<'_>>(&bytes).unwrap();
        assert!(old.names == lent.names && old.rows == lent.rows);
    }
    let not_names = to_vec(&Other { rows: Vec::new(), names: ("a", 3u8) }).unwrap();
    assert!(from_slice::<Owned>(&not_names).is_err());
    let lent = from_slice::<Lent<'_>>(&not_names).err().expect("an integer is no name");
    assert!(lent.to_string().contains("integer `3`"), "{lent}");
    assert!(reference::from_slice::<Lent<'_>>(&not_names).is_err());
    let not_a_list = to_vec(&Other { rows: Vec::new(), names: "a" }).unwrap();
    assert!(from_slice::<Owned>(&not_a_list).is_err());
    assert!(from_slice::<Lent<'_>>(&not_a_list).is_err());
    assert!(serde_json::from_str::<Raw<'_>>("[1]").is_err());
}

/// A list's item past the depth limit is refused where a `Vec`'s is.
#[test]
fn a_list_keeps_the_depth_limit() {
    use super::List;

    #[derive(Deserialize)]
    struct Lent<'a> {
        #[serde(borrow)]
        _items: List<'a, IgnoredAny>,
    }
    for depth in [250usize, 255, 256, 257, 300] {
        let mut bytes = vec![0x81; depth];
        bytes.push(0xf6);
        let owned = from_slice::<Vec<IgnoredAny>>(&bytes).map(drop);
        let lent = from_slice::<List<'_, IgnoredAny>>(&bytes).map(drop);
        assert_eq!(owned, lent, "{depth}");
        assert_eq!(reference::from_slice::<List<'_, IgnoredAny>>(&bytes).map(drop), lent, "{depth}");
        let mut field = vec![0xa1, 0x66];
        field.extend_from_slice(b"_items");
        field.extend_from_slice(&bytes);
        let owned = from_slice::<Owned>(&field).map(drop);
        assert_eq!(from_slice::<Lent<'_>>(&field).map(drop), owned, "{depth}");
    }

    #[derive(Deserialize)]
    struct Owned {
        _items: Vec<IgnoredAny>,
    }
}

/// An offence is named where [`super::check`] names it, at the start of its item.
#[test]
fn an_offence_is_named_where_the_checker_names_it() {
    let what = "an argument in more bytes than it needs";
    assert_eq!(from_slice::<u8>(&[0x18, 0x17]), Err(Error::NotDeterministic { at: 0, what }));
    assert_eq!(
        from_slice::<Vec<u8>>(&[0x82, 0x01, 0x18, 0x02]),
        Err(Error::NotDeterministic { at: 2, what })
    );
}

/// A field is named as text, as it is written: not by its index, nor as bytes, so a value read
/// back is from the one item that encodes it.
#[test]
fn a_field_is_named_only_as_text() {
    let refused = Err(Error::NotDeterministic { at: 1, what: "a field or variant not named as text" });
    for bytes in [&[0xa1, 0x00, 0x07][..], &[0xa1, 0x42, b'i', b'd', 0x07]] {
        assert_eq!(from_slice::<Typed>(bytes), refused, "{bytes:02x?}");
        assert_eq!(reference::from_slice::<Typed>(bytes), refused, "{bytes:02x?}");
    }
    let named = Typed { id: Some(7), kind: None, body: None };
    assert_eq!(from_slice::<Typed>(&[0xa1, 0x62, b'i', b'd', 0x07]), Ok(named));
}

/// Keys are ordered by their first bytes alone only where those differ, so the order is the
/// bytes' order for any two keys, of one length or two, a prefix of the other or empty.
#[test]
fn keys_are_ordered_as_their_bytes_are() {
    let mut random = Random(SplitMix64::new(0x6b65_7973));
    let bytes = [0x60, 0x61, 0x62, 0x78, b'a', b'b', 0xff];
    let key = |random: &mut Random| -> Vec<u8> {
        let len = random.below(5) as usize;
        (0..len).map(|_| bytes[random.below(7) as usize]).collect()
    };
    for _ in 0..20_000 {
        let a = key(&mut random);
        let b = if random.below(4) == 0 { a.clone() } else { key(&mut random) };
        assert_eq!(key_order(&a, &b), a.cmp(&b), "{a:02x?} {b:02x?}");
    }
}

/// A struct's field names are ordered as their encodings are, by the names alone: of one
/// length or two, across a head's widths, and with characters of more than a byte.
#[test]
fn names_are_ordered_as_their_encodings_are() {
    let mut random = Random(SplitMix64::new(0x6e61_6d65));
    let name = |random: &mut Random| -> String {
        let len = [0, 1, 2, 11, 12, 23, 24, 25, 128, 255, 256][random.below(11) as usize];
        (0..len).map(|_| ['a', 'b', '\u{e9}'][random.below(3) as usize]).collect()
    };
    for _ in 0..20_000 {
        let a = name(&mut random);
        let b = if random.below(4) == 0 { a.clone() } else { name(&mut random) };
        let (mut x, mut y) = (Vec::new(), Vec::new());
        append(&mut x, a.as_str()).unwrap();
        append(&mut y, b.as_str()).unwrap();
        assert_eq!(name_order(&a, &b), x.cmp(&y), "{a:?} {b:?}");
    }
}
