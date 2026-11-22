//! Tests of [`crate::Verified`] and of narrow offsets, on blaze-sim.
//!
//! The oracle of the fuzz is a reader of its own (`reference`), written from the format's
//! rules byte by byte and sharing no code with the checks it judges: `open` must refuse every
//! buffer that reader refuses, and every unchecked read of a buffer `open` accepts must equal
//! what that reader decodes. Each control plants a bug in `open`'s walk ([`crate::planted`]) and
//! is seen to be caught; the unchecked reads run only once the reference has accepted the
//! buffer, so a planted run never reads past what it may (the tests stay Miri-clean).

use std::borrow::ToOwned;
use std::format;
use std::string::String;
use std::vec::Vec;

use blaze_own::Alloc;
use blaze_sim::{Broke, Config, Tape};
use core::mem::{align_of, offset_of, size_of};

use crate::declared::{ByteEnum, Declared, FromByte};
use crate::planted::{self, Plant};
use crate::{
    FormatError, HEADER_LEN, MAGIC, Offset, Own, Part, Record, Rel, SchemaHash, Slice, Text,
    Unfit, Verified, Writer, WriterOpts, bytes_of_slice, field, open, record, schema, schema_of,
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
enum Kind {
    A = 0,
    B = 1,
    C = 5,
}

impl FromByte for Kind {
    const WHAT: &'static str = "a byte that is no Kind";
    fn from_byte(byte: u8) -> Option<Kind> {
        match byte {
            0 => Some(Kind::A),
            1 => Some(Kind::B),
            5 => Some(Kind::C),
            _ => None,
        }
    }
}

impl Declared for Kind {
    const SCHEMA: SchemaHash = schema("verified_tests Kind 1");
    type Layout = ByteEnum;
}

record! {
    #[derive(Clone, Copy, PartialEq, Debug)]
    struct Leaf: 1 {
        value: u32,
        flag: bool,
        kind: Kind,
        pad: [u8; 2],
    }
}

record! {
    /// Every kind of reference, wide and narrow, each with what it points at written just
    /// before the node, so the narrow ones fit.
    #[derive(Clone, Copy, PartialEq, Debug)]
    struct Node: 1 {
        words: Slice<u32>,
        leaves: Slice<Leaf>,
        name: Text,
        leaf: Rel<Leaf>,
        short: Slice<Leaf, i16>,
        near: Rel<Leaf, i16>,
        tiny: Slice<u32, i8>,
        small: Text<i8>,
        pad: [u8; 2],
    }
}

record! {
    #[derive(Clone, Copy, PartialEq, Debug)]
    struct Root: 1 {
        nodes: Slice<Rel<Node>>,
        first: Rel<Node>,
        title: Text,
    }
}

/// A value as the tests know it, whoever read it.
#[derive(Clone, PartialEq, Debug)]
struct LeafModel {
    value: u32,
    flag: bool,
    kind: Kind,
}

#[derive(Clone, PartialEq, Debug)]
struct NodeModel {
    words: Vec<u32>,
    leaves: Vec<LeafModel>,
    name: String,
    leaf: Option<LeafModel>,
    short: Vec<LeafModel>,
    near: Option<LeafModel>,
    tiny: Vec<u32>,
    small: String,
}

#[derive(Clone, PartialEq, Debug)]
struct RootModel {
    nodes: Vec<Option<NodeModel>>,
    first: Option<NodeModel>,
    title: String,
}

fn draw_leaf(tape: &Tape) -> LeafModel {
    LeafModel {
        value: tape.int("value"),
        flag: tape.coin("flag"),
        kind: *tape.one_of(&[Kind::A, Kind::B, Kind::C], "kind"),
    }
}

const TEXT: &str = "az\u{e9}\u{20ac}\u{1f600}";

fn draw_node(tape: &Tape) -> NodeModel {
    NodeModel {
        words: tape.vec(0..=8, "words", |tape| tape.int("word")),
        leaves: tape.vec(0..=4, "leaves", draw_leaf),
        name: tape.text_from(TEXT, 0..=12, "name"),
        leaf: tape.coin("has leaf").then(|| draw_leaf(tape)),
        short: tape.vec(0..=3, "short", draw_leaf),
        near: tape.coin("has near").then(|| draw_leaf(tape)),
        tiny: tape.vec(0..=6, "tiny", |tape| tape.int("tiny word")),
        small: tape.text_from(TEXT, 0..=6, "small"),
    }
}

fn draw_root(tape: &Tape) -> RootModel {
    let nodes: Vec<Option<NodeModel>> = tape.vec(0..=3, "nodes", |tape| Some(draw_node(tape)));
    let first = match nodes.first() {
        Some(node) if tape.coin("has first") => node.clone(),
        _ => None,
    };
    RootModel { nodes, first, title: tape.text_from(TEXT, 0..=8, "title") }
}

/// Where a buffer's references are, for the fuzz to aim at: each an integer `width` bytes long.
#[derive(Default)]
struct Aims {
    words: Vec<(u64, usize)>,
}

type Out = Writer<Own<Alloc<'static>>>;

/// A writer of `schema` on the test's heap.
fn out_of(schema: SchemaHash) -> Out {
    Writer::new(WriterOpts { schema, alloc: Own(blaze_own::testing::heap().alloc()), capacity: None })
}

fn leaf_of(model: &LeafModel) -> Leaf {
    Leaf { value: model.value, flag: model.flag, kind: model.kind, pad: [0; 2] }
}

fn push_leaves(out: &mut Out, leaves: &[LeafModel]) -> u64 {
    let leaves: Vec<Leaf> = leaves.iter().map(leaf_of).collect();
    out.push_slice(&leaves)
}

/// Aims at a reference's words: its offset, and its length after it when it is a slice.
fn aim<O: Offset>(aims: &mut Aims, field_at: u64, slice: bool) {
    aims.words.push((field_at, size_of::<O>()));
    if slice {
        aims.words.push((field_at + size_of::<O>() as u64, size_of::<O>()));
    }
}

fn write_node(out: &mut Out, aims: &mut Aims, node: &NodeModel) -> u64 {
    let words = out.push_slice(&node.words);
    let leaves = push_leaves(out, &node.leaves);
    let name = out.push_str(&node.name);
    let leaf = node.leaf.as_ref().map(|leaf| out.push(&leaf_of(leaf)));
    let short = push_leaves(out, &node.short);
    let near = node.near.as_ref().map(|leaf| out.push(&leaf_of(leaf)));
    let tiny = out.push_slice(&node.tiny);
    let small = out.push_str(&node.small);
    let at = out.next::<Node>();
    let field = |offset: usize| at + offset as u64;
    fn rel_to<O: Offset>(field_at: u64, target: Option<u64>) -> Result<Rel<Leaf, O>, Unfit> {
        match target {
            Some(target) => Rel::between_as(field_at, target),
            None => Ok(Rel::NONE),
        }
    }
    let rel = |offset: usize, target: Option<u64>| rel_to::<i32>(field(offset), target);
    let fit = "a test node's references are written to fit";
    let record = Node {
        words: Slice::between_as(field(offset_of!(Node, words)), words, node.words.len()).expect(fit),
        leaves: Slice::between_as(field(offset_of!(Node, leaves)), leaves, node.leaves.len())
            .expect(fit),
        name: Text::between_as(field(offset_of!(Node, name)), name, node.name.len()).expect(fit),
        leaf: rel(offset_of!(Node, leaf), leaf).expect(fit),
        short: Slice::between_as(field(offset_of!(Node, short)), short, node.short.len()).expect(fit),
        near: rel_to::<i16>(field(offset_of!(Node, near)), near).expect(fit),
        tiny: Slice::between_as(field(offset_of!(Node, tiny)), tiny, node.tiny.len()).expect(fit),
        small: Text::between_as(field(offset_of!(Node, small)), small, node.small.len()).expect(fit),
        pad: [0; 2],
    };
    aim::<i32>(aims, field(offset_of!(Node, words)), true);
    aim::<i32>(aims, field(offset_of!(Node, leaves)), true);
    aim::<i32>(aims, field(offset_of!(Node, name)), true);
    aim::<i32>(aims, field(offset_of!(Node, leaf)), false);
    aim::<i16>(aims, field(offset_of!(Node, short)), true);
    aim::<i16>(aims, field(offset_of!(Node, near)), false);
    aim::<i8>(aims, field(offset_of!(Node, tiny)), true);
    aim::<i8>(aims, field(offset_of!(Node, small)), true);
    let written = out.push(&record);
    assert_eq!(written, at);
    at
}

/// `model` as a buffer, and where its references are.
fn write(model: &RootModel) -> (Aligned, Aims) {
    let mut out = out_of(<Root as Record>::SCHEMA);
    let mut aims = Aims::default();
    let nodes: Vec<Option<u64>> = model
        .nodes
        .iter()
        .map(|node| node.as_ref().map(|node| write_node(&mut out, &mut aims, node)))
        .collect();
    let rels_at = out.next::<Rel<Node>>();
    let fit = "a test root's references are written to fit";
    let rels: Vec<Rel<Node>> = nodes
        .iter()
        .enumerate()
        .map(|(index, node)| {
            let field_at = rels_at + (index * size_of::<Rel<Node>>()) as u64;
            aims.words.push((field_at, 4));
            match node {
                Some(node) => Rel::between_as(field_at, *node).expect(fit),
                None => Rel::NONE,
            }
        })
        .collect();
    let rels_written = out.push_slice(&rels);
    assert!(rels.is_empty() || rels_written == rels_at);
    let title = out.push_str(&model.title);
    let at = out.next::<Root>();
    let field = |offset: usize| at + offset as u64;
    let first = match model.first {
        Some(_) => Rel::between_as(field(offset_of!(Root, first)), nodes[0].expect(fit)).expect(fit),
        None => Rel::NONE,
    };
    let root = Root {
        nodes: Slice::between_as(field(offset_of!(Root, nodes)), rels_at, rels.len()).expect(fit),
        first,
        title: Text::between_as(field(offset_of!(Root, title)), title, model.title.len())
            .expect(fit),
    };
    aim::<i32>(&mut aims, field(offset_of!(Root, nodes)), true);
    aim::<i32>(&mut aims, field(offset_of!(Root, first)), false);
    aim::<i32>(&mut aims, field(offset_of!(Root, title)), true);
    out.push(&root);
    let bytes = out.finish(at);
    (Aligned::of(&bytes), aims)
}

/// Bytes at an address aligned to 16, as a buffer is, that a test may change.
struct Aligned {
    words: Vec<u128>,
    len: usize,
}

impl Aligned {
    fn of(bytes: &[u8]) -> Aligned {
        let words = bytes
            .chunks(16)
            .map(|chunk| {
                let mut word = [0; 16];
                word[..chunk.len()].copy_from_slice(chunk);
                u128::from_le_bytes(word)
            })
            .collect();
        Aligned { words, len: bytes.len() }
    }

    fn bytes(&self) -> &[u8] {
        &bytes_of_slice(&self.words)[..self.len]
    }

    fn set(&mut self, at: usize, byte: u8) {
        let mut word = self.words[at / 16].to_le_bytes();
        word[at % 16] = byte;
        self.words[at / 16] = u128::from_le_bytes(word);
    }

    /// Writes `value`'s low `width` bytes at `at`, little-endian.
    fn set_int(&mut self, at: usize, width: usize, value: i64) {
        for (index, byte) in value.to_le_bytes().iter().take(width).enumerate() {
            if at + index < self.len {
                self.set(at + index, *byte);
            }
        }
    }
}

// The reads through the proof, with no check.

fn leaf_unchecked(leaf: &Verified<Leaf>) -> LeafModel {
    LeafModel {
        value: Verified::get(leaf).value,
        flag: *Verified::get(field!(leaf, Leaf, flag)),
        kind: *Verified::get(field!(leaf, Leaf, kind)),
    }
}

fn node_unchecked(node: &Verified<Node>) -> NodeModel {
    NodeModel {
        words: field!(node, Node, words).items().as_slice().to_vec(),
        leaves: field!(node, Node, leaves).items().iter().map(leaf_unchecked).collect(),
        name: field!(node, Node, name).as_str().to_owned(),
        leaf: field!(node, Node, leaf).target().map(leaf_unchecked),
        short: field!(node, Node, short).items().iter().map(leaf_unchecked).collect(),
        near: field!(node, Node, near).target().map(leaf_unchecked),
        tiny: field!(node, Node, tiny).items().as_slice().to_vec(),
        small: field!(node, Node, small).as_str().to_owned(),
    }
}

fn root_unchecked(root: &Verified<Root>) -> RootModel {
    let nodes = field!(root, Root, nodes).items();
    RootModel {
        nodes: nodes.iter().map(|rel| rel.target().map(node_unchecked)).collect(),
        first: field!(root, Root, first).target().map(node_unchecked),
        title: field!(root, Root, title).as_str().to_owned(),
    }
}

// The reads of the bytes `open` did not prove: each reference checked as it is followed.

fn leaf_checked(leaf: &Leaf) -> LeafModel {
    LeafModel { value: leaf.value, flag: leaf.flag, kind: leaf.kind }
}

fn node_checked(node: &Node, buffer: &[u8]) -> Result<NodeModel, FormatError> {
    Ok(NodeModel {
        words: node.words.follow(buffer)?.to_vec(),
        leaves: node.leaves.follow(buffer)?.iter().map(leaf_checked).collect(),
        name: node.name.as_str(buffer)?.to_owned(),
        leaf: node.leaf.follow(buffer)?.map(leaf_checked),
        short: node.short.follow(buffer)?.iter().map(leaf_checked).collect(),
        near: node.near.follow(buffer)?.map(leaf_checked),
        tiny: node.tiny.follow(buffer)?.to_vec(),
        small: node.small.as_str(buffer)?.to_owned(),
    })
}

fn root_checked(root: &Root, buffer: &[u8]) -> Result<RootModel, FormatError> {
    let nodes = root
        .nodes
        .follow(buffer)?
        .iter()
        .map(|rel| rel.follow(buffer)?.map(|node| node_checked(node, buffer)).transpose())
        .collect::<Result<_, _>>()?;
    Ok(RootModel {
        nodes,
        first: root.first.follow(buffer)?.map(|node| node_checked(node, buffer)).transpose()?,
        title: root.title.as_str(buffer)?.to_owned(),
    })
}

/// The format's rules, byte by byte, from scratch: the fuzz's oracle.
mod reference {
    use std::format;
    use std::string::String;
    use std::vec::Vec;

    use super::{Kind, LeafModel, NodeModel, RootModel};
    use crate::HEADER_LEN;

    pub(super) struct Reader<'a> {
        pub(super) buffer: &'a [u8],
    }

    type Read<T> = Result<T, String>;

    impl Reader<'_> {
        fn int(&self, at: usize, width: usize) -> Read<u64> {
            let bytes = self.buffer.get(at..at + width).ok_or(format!("{at}+{width}: past the end"))?;
            Ok(bytes.iter().rev().fold(0, |word, byte| word << 8 | u64::from(*byte)))
        }

        fn signed(&self, at: usize, width: usize) -> Read<i64> {
            let word = self.int(at, width)?;
            let shift = 64 - 8 * width as u32;
            Ok(((word << shift) as i64) >> shift)
        }

        /// The `count` items of `size` and `align` a reference at `field` with offset `offset`
        /// reaches: where the first is.
        fn target(&self, field: usize, offset: i64, count: u64, size: usize, align: usize) -> Read<usize> {
            let target = field as i64 + offset;
            if target < HEADER_LEN as i64 {
                return Err(format!("a reference at {field} to {target}, in the header"));
            }
            let end = (size as u64)
                .checked_mul(count)
                .and_then(|len| len.checked_add(target as u64))
                .ok_or(format!("a reference at {field} of {count} items: too long"))?;
            if end > field as u64 {
                return Err(format!("a reference at {field} reaching {end}, past itself"));
            }
            if (self.buffer.as_ptr() as usize + target as usize) % align != 0 {
                return Err(format!("a reference at {field} to {target}, misaligned"));
            }
            Ok(target as usize)
        }

        fn rel(&self, field: usize, width: usize, size: usize, align: usize) -> Read<Option<usize>> {
            match self.signed(field, width)? {
                0 => Ok(None),
                offset => self.target(field, offset, 1, size, align).map(Some),
            }
        }

        fn slice(&self, field: usize, width: usize, size: usize, align: usize) -> Read<(usize, usize)> {
            let offset = self.signed(field, width)?;
            let count = self.int(field + width, width)?;
            match count {
                0 => Ok((0, 0)),
                count => Ok((self.target(field, offset, count, size, align)?, count as usize)),
            }
        }

        fn text(&self, field: usize, width: usize) -> Read<String> {
            let (at, len) = self.slice(field, width, 1, 1)?;
            let bytes = &self.buffer[at..at + len];
            String::from_utf8(bytes.to_vec()).map_err(|_| format!("text at {at}: not UTF-8"))
        }

        fn words(&self, field: usize, width: usize) -> Read<Vec<u32>> {
            let (at, len) = self.slice(field, width, 4, 4)?;
            (0..len).map(|index| self.int(at + 4 * index, 4).map(|word| word as u32)).collect()
        }

        fn leaf(&self, at: usize) -> Read<LeafModel> {
            let flag = match self.int(at + 4, 1)? {
                0 => false,
                1 => true,
                other => return Err(format!("a bool at {} of {other}", at + 4)),
            };
            let kind = match self.int(at + 5, 1)? {
                0 => Kind::A,
                1 => Kind::B,
                5 => Kind::C,
                other => return Err(format!("a Kind at {} of {other}", at + 5)),
            };
            Ok(LeafModel { value: self.int(at, 4)? as u32, flag, kind })
        }

        fn leaves(&self, field: usize, width: usize) -> Read<Vec<LeafModel>> {
            let (at, len) = self.slice(field, width, 8, 4)?;
            (0..len).map(|index| self.leaf(at + 8 * index)).collect()
        }

        fn leaf_rel(&self, field: usize, width: usize) -> Read<Option<LeafModel>> {
            self.rel(field, width, 8, 4)?.map(|at| self.leaf(at)).transpose()
        }

        /// A `Node` at `at`: words 0, leaves 8, name 16, leaf 24, short 28, near 32, tiny 34,
        /// small 36.
        fn node(&self, at: usize) -> Read<NodeModel> {
            Ok(NodeModel {
                words: self.words(at, 4)?,
                leaves: self.leaves(at + 8, 4)?,
                name: self.text(at + 16, 4)?,
                leaf: self.leaf_rel(at + 24, 4)?,
                short: self.leaves(at + 28, 2)?,
                near: self.leaf_rel(at + 32, 2)?,
                tiny: self.words(at + 34, 1)?,
                small: self.text(at + 36, 1)?,
            })
        }

        fn node_rel(&self, field: usize) -> Read<Option<NodeModel>> {
            self.rel(field, 4, 40, 4)?.map(|at| self.node(at)).transpose()
        }

        /// The root the header names: nodes 0, first 8, title 12.
        pub(super) fn root(&self) -> Read<RootModel> {
            let at = self.int(48, 8)? as usize;
            if at < HEADER_LEN || at.checked_add(20).is_none_or(|end| end > self.buffer.len()) || at % 4 != 0 {
                return Err(format!("a root at {at}"));
            }
            let (nodes, count) = self.slice(at, 4, 4, 4)?;
            Ok(RootModel {
                nodes: (0..count).map(|index| self.node_rel(nodes + 4 * index)).collect::<Read<_>>()?,
                first: self.node_rel(at + 8)?,
                title: self.text(at + 12, 4)?,
            })
        }
    }
}

const _: () = {
    assert!(size_of::<Node>() == 40 && align_of::<Node>() == 4);
    assert!(offset_of!(Node, short) == 28 && offset_of!(Node, near) == 32);
    assert!(offset_of!(Node, tiny) == 34 && offset_of!(Node, small) == 36);
    assert!(size_of::<Root>() == 20 && offset_of!(Root, title) == 12);
};

#[test]
fn every_read_through_the_proof_is_the_checked_read() {
    blaze_sim::check(Config::cases(300), |tape| {
        let model = draw_root(tape);
        let (buffer, _) = write(&model);
        let buffer = buffer.bytes();
        let root = open::<Root>(buffer).map_err(|error| Broke::Oracle(format!("{error}")))?;
        let unchecked = root_unchecked(root);
        let checked = root_checked(root, buffer).map_err(|error| Broke::Oracle(format!("{error}")))?;
        match (unchecked == checked, checked == model) {
            (true, true) => Ok(()),
            (false, _) => Err(Broke::Oracle(format!(
                "the unchecked read {unchecked:?} is not the checked read {checked:?}"
            ))),
            (true, false) => Err(Broke::Oracle(format!("read {checked:?}, wrote {model:?}"))),
        }
    });
}

/// A valid buffer, changed in a few words: `open` refuses it, or the reference reads it too and
/// every unchecked read is the reference's. Whether `open` accepted it.
fn fuzz(tape: &Tape) -> Result<bool, Broke> {
    let model = draw_root(tape);
    let (mut buffer, aims) = write(&model);
    let changes = tape.int_in(1..=3usize, "changes");
    for _ in 0..changes {
        match aims.words.is_empty() || tape.one_in(4, "anywhere") {
            true => {
                let at = tape.int_in(HEADER_LEN..=buffer.len - 1, "at");
                buffer.set(at, tape.int("byte"));
            }
            false => {
                let &(at, width) = tape.one_of(&aims.words, "aim");
                let value = tape.int_in(-96i64..=96, "value");
                buffer.set_int(at as usize, width, value);
            }
        }
    }
    let buffer = buffer.bytes();
    let Ok(root) = open::<Root>(buffer) else {
        return Ok(false);
    };
    let reference = reference::Reader { buffer }.root().map_err(|why| {
        Broke::Oracle(format!("open accepted a buffer the reference reader refuses: {why}"))
    })?;
    // Only now, the reference having accepted the buffer, are the unchecked reads made.
    let unchecked = root_unchecked(root);
    let checked = root_checked(root, buffer).map_err(|error| {
        Broke::Oracle(format!("open accepted a buffer a checked read refuses: {error}"))
    })?;
    match unchecked == reference && checked == reference {
        true => Ok(true),
        false => Err(Broke::Oracle(format!(
            "an unchecked read {unchecked:?} or a checked one {checked:?} is not the reference's {reference:?}"
        ))),
    }
}

/// The refinement: `Verified::open` refuses every buffer the byte-by-byte reference reader refuses (including out-of-bounds references and invalid UTF-8), and every unchecked read agrees with the reference.
#[test]
fn open_refuses_every_buffer_an_unchecked_read_would_misread() {
    // A fuzz whose changes `open` always refused would read nothing unchecked: the cases it
    // accepted are counted, and some must be.
    let accepted = core::cell::Cell::new(0u32);
    blaze_sim::check(Config::cases(1000), |tape| -> Result<(), Broke> {
        let opened = fuzz(tape)?;
        accepted.set(accepted.get() + u32::from(opened));
        Ok(())
    });
    assert!(accepted.get() >= 50, "open accepted only {} changed buffers", accepted.get());
}

#[test]
fn the_oracle_catches_a_reference_past_its_field() {
    let run = |tape: &Tape, plant: Plant| planted::with(plant, || fuzz(tape)).map(drop);
    blaze_sim::control(Config::cases(1000), Plant::Clean, Plant::Forward, "refuses", run);
}

#[test]
fn the_oracle_catches_text_that_is_not_utf8() {
    let run = |tape: &Tape, plant: Plant| planted::with(plant, || fuzz(tape)).map(drop);
    blaze_sim::control(Config::cases(1000), Plant::Clean, Plant::NotUtf8, "refuses", run);
}

/// Whether a reference `distance` bytes back fits `O`, by `between`, against the range of `O`.
fn fits_back<O: Offset>(distance: u64) -> Result<(), Broke> {
    let field_at = 1 << 40;
    let target_at = field_at - distance;
    let fits = i64::try_from(distance).is_ok_and(|distance| O::of_distance(-distance).is_some());
    let rel = Rel::<u32, O>::between_as(field_at, target_at);
    let slice = Slice::<u32, O>::between_as(field_at, target_at, 1);
    let far = Unfit::Far { field_at, target_at, offset: O::NAME };
    match (fits, rel, slice) {
        (true, Ok(rel), Ok(slice)) if rel.offset.distance() == -(distance as i64)
            && slice.offset == rel.offset =>
        {
            Ok(())
        }
        (false, Err(rel), Err(slice)) if rel == far && slice == far => Ok(()),
        (_, rel, slice) => Err(Broke::Oracle(format!(
            "{distance} bytes back as {}: {rel:?} and {slice:?}",
            O::NAME
        ))),
    }
}

#[test]
fn a_narrow_reference_is_refused_past_its_width() {
    for (o, edge) in [("i8", 128u64), ("i16", 32_768), ("i32", 1 << 31)] {
        for distance in [1, edge - 1, edge, edge + 1, edge * 2] {
            let checked = match o {
                "i8" => fits_back::<i8>(distance),
                "i16" => fits_back::<i16>(distance),
                _ => fits_back::<i32>(distance),
            };
            assert_eq!(checked, Ok(()), "{distance} bytes back as {o}");
        }
    }
    blaze_sim::check(Config::cases(500), |tape| {
        let distance = tape.int_in(1u64..=70_000, "distance");
        fits_back::<i8>(distance)?;
        fits_back::<i16>(distance)?;
        fits_back::<i32>(distance)
    });
    // The length of a narrow slice is refused past its width, and a reference ahead of its
    // field always.
    assert_eq!(Slice::<u8, i8>::between_as(1000, 900, 255).map(|it| it.len), Ok(255));
    assert_eq!(
        Slice::<u8, i8>::between_as(1000, 900, 256),
        Err(Unfit::Long { field_at: 1000, len: 256, offset: "i8" })
    );
    assert_eq!(Slice::<u8, i16>::between_as(1000, 900, 65_535).map(|it| it.len), Ok(65_535));
    assert!(Slice::<u8, i16>::between_as(1000, 900, 65_536).is_err());
    assert_eq!(Rel::<u8, i8>::between_as(100, 100), Err(Unfit::Forward { field_at: 100, target_at: 100 }));
    assert_eq!(Slice::<u8, i8>::between_as(100, 100, 0).map(|it| it.offset), Ok(0));
    assert!(Slice::<u8, i8>::between_as(100, 101, 0).is_err());
}

#[test]
fn a_narrow_reference_at_its_edge_opens_and_reads() {
    // A `Rel<u32, i8>` exactly 128 bytes back, the farthest an `i8` reaches.
    record! {
        #[derive(Clone, Copy)]
        struct Edge: 1 {
            word: Rel<u32, i8>,
        }
    }
    let mut out = out_of(<Edge as Record>::SCHEMA);
    let word = out.push(&0xfeed_u32);
    out.push_slice(&[0u8; 124]);
    let at = out.next::<Edge>();
    assert_eq!(at - word, 128);
    let root = Edge { word: Rel::between_as(at, word).expect("128 bytes back fits an i8") };
    out.push(&root);
    let buffer = Aligned::of(&out.finish(at));
    let root = open::<Edge>(buffer.bytes()).expect("a buffer just written");
    assert_eq!(field!(root, Edge, word).target().map(|it| *Verified::get(it)), Some(0xfeed));
    assert_eq!(Rel::<u32, i8>::between_as(at + 1, word), Err(Unfit::Far { field_at: at + 1, target_at: word, offset: "i8" }));
}

#[test]
fn a_narrow_reference_has_a_schema_of_its_own() {
    let wide = schema_of(&[Part::Str("Rel"), Part::Schema(u32::SCHEMA)]);
    assert_eq!(Rel::<u32>::SCHEMA, wide, "the default width keeps the hash it had");
    assert_eq!(
        Slice::<u32>::SCHEMA,
        schema_of(&[Part::Str("Slice"), Part::Schema(u32::SCHEMA)])
    );
    let all = [
        Rel::<u32, i8>::SCHEMA,
        Rel::<u32, i16>::SCHEMA,
        Rel::<u32, i32>::SCHEMA,
        Slice::<u32, i8>::SCHEMA,
        Slice::<u32, i16>::SCHEMA,
        Slice::<u32, i32>::SCHEMA,
        Text::<i8>::SCHEMA,
        Text::<i32>::SCHEMA,
        <Slice<u8> as Record>::SCHEMA,
    ];
    for (index, a) in all.iter().enumerate() {
        assert!(all[index + 1..].iter().all(|b| a != b), "schema {index} repeats");
    }
    assert_eq!((size_of::<Rel<u32, i8>>(), size_of::<Slice<u32, i8>>()), (1, 2));
    assert_eq!((size_of::<Rel<u32, i16>>(), size_of::<Slice<u32, i16>>()), (2, 4));
    assert_eq!((size_of::<Rel<u32>>(), size_of::<Slice<u32>>()), (4, 8));
    let _ = MAGIC;
}
