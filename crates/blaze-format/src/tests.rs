use std::mem::offset_of;

use blaze_ids::ContentId;

use crate::{
    Aligned, FormatError, HEADER_LEN, Persist, Record, Rel, Slice, SpanRef, Spans, Str, Writer,
    buffer_id, header, open, open_as, open_verified, read_verified, record, schema, to_bytes,
};

#[test]
fn plain_records_are_checked_by_their_bounds() {
    const { assert!(<u32 as Record>::PLAIN && <[u8; 7] as Record>::PLAIN) };
    const { assert!(<crate::RelSpan as Record>::PLAIN && <SpanRef as Record>::PLAIN) };
    const { assert!(<ContentId as Record>::PLAIN && <[ContentId; 2] as Record>::PLAIN) };
    const { assert!(!<bool as Record>::PLAIN && !<[bool; 2] as Record>::PLAIN) };
    const { assert!(!<Leaf as Record>::PLAIN && !<Root as Record>::PLAIN) };
    const { assert!(!<Rel<u32> as Record>::PLAIN && !<Slice<u32> as Record>::PLAIN) };
    const { assert!(<Rel<Leaf> as Record>::SHALLOW_PLAIN && <Slice<Leaf> as Record>::SHALLOW_PLAIN) };
    const { assert!(<Root as Record>::SHALLOW_PLAIN && <[Rel<Leaf>; 2] as Record>::SHALLOW_PLAIN) };
    const { assert!(<u32 as Record>::SHALLOW_PLAIN && <crate::RelSpan as Record>::SHALLOW_PLAIN) };
    const { assert!(!<Leaf as Record>::SHALLOW_PLAIN && !<bool as Record>::SHALLOW_PLAIN) };
    const { assert!(!<[Leaf; 2] as Record>::SHALLOW_PLAIN && !<Counted as Record>::SHALLOW_PLAIN) };
    let buffer = Aligned::copy_from(&written(1));
    let root = open::<Root>(&buffer).unwrap();
    assert_eq!(root.numbers.get(&buffer), &[1, 2, 3]);
    assert!(matches!(open::<Root>(&Aligned::copy_from(&written(2))), Err(FormatError::Invalid { .. })));
}

#[test]
fn a_verified_open_reads_only_the_buffer_its_key_names() {
    let buffer = Aligned::copy_from(&written(1));
    let key = buffer_id(&buffer);
    assert_eq!(key, ContentId::of(crate::BUFFER_DOMAIN, &buffer));
    let root = open_verified::<Root>(&buffer, Root::SCHEMA, key).unwrap();
    assert_eq!(root.numbers.get(&buffer), &[1, 2, 3]);

    let mut changed = written(1);
    let last = changed.len() - 1;
    changed[last] ^= 1;
    let changed = Aligned::copy_from(&changed);
    assert_eq!(
        open_verified::<Root>(&changed, Root::SCHEMA, key).err(),
        Some(FormatError::Hash { found: buffer_id(&changed), wanted: key })
    );
    assert!(matches!(
        open_verified::<Root>(&buffer, Leaf::SCHEMA, key),
        Err(FormatError::Schema { .. })
    ));

    let mut spans = Spans::new(ContentId::of("blaze v2 test 1", b"fn main"));
    spans.push(3, 4);
    let bytes = Aligned::copy_from(&to_bytes(&spans));
    let read = read_verified::<Spans>(&bytes, buffer_id(&bytes)).unwrap();
    assert_eq!(read.to_spans(), spans);
    assert!(matches!(read_verified::<Spans>(&bytes, key), Err(FormatError::Hash { .. })));
}

record! {
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    struct Leaf: 1 {
        value: u64,
        flag: bool,
        pad: [u8; 7],
    }
}

record! {
    #[derive(Clone, Copy, Debug)]
    struct Root: 1 {
        leaf: Rel<Leaf>,
        numbers: Slice<u32>,
        name: Str,
        none: Rel<Leaf>,
        leaves: Slice<Leaf>,
    }
}

fn written(flag: u8) -> Vec<u8> {
    written_by(Writer::new(Root::SCHEMA), flag)
}

fn written_by(mut out: Writer, flag: u8) -> Vec<u8> {
    let leaf = out.push(&Leaf { value: 7, flag: true, pad: [0; 7] });
    let numbers = out.push_slice(&[1u32, 2, 3]);
    let name = out.push_str("blaze");
    let leaves = out.push_slice(&[
        Leaf { value: 1, flag: false, pad: [0; 7] },
        Leaf { value: 2, flag: true, pad: [0; 7] },
    ]);
    if flag != 1 {
        let at = leaf as usize + offset_of!(Leaf, flag);
        out.bytes[at] = flag;
    }
    let at = out.next::<Root>();
    let field = |offset: usize| at + offset as u64;
    let root = Root {
        leaf: Rel::between(field(offset_of!(Root, leaf)), leaf),
        numbers: Slice::between(field(offset_of!(Root, numbers)), numbers, 3),
        name: Str::between(field(offset_of!(Root, name)), name, 5),
        none: Rel::NONE,
        leaves: Slice::between(field(offset_of!(Root, leaves)), leaves, 2),
    };
    assert_eq!(out.push(&root), at);
    out.finish(at)
}

#[test]
fn a_buffer_reads_in_place() {
    let buffer = Aligned::copy_from(&written(1));
    let root = open::<Root>(&buffer).unwrap();
    assert_eq!(root.leaf.get(&buffer), Some(&Leaf { value: 7, flag: true, pad: [0; 7] }));
    assert_eq!(root.numbers.get(&buffer), &[1, 2, 3]);
    assert_eq!(root.name.as_str(&buffer), Ok("blaze"));
    assert_eq!(root.none.get(&buffer), None);
    assert_eq!(root.leaves.get(&buffer).iter().map(|it| it.value).collect::<Vec<_>>(), [1, 2]);
    assert_eq!(header(&buffer).unwrap().len, buffer.len() as u64);
    assert_eq!(buffer.len() % crate::ALIGN, 0);
}

#[test]
fn equal_values_are_equal_bytes() {
    assert_eq!(written(1), written(1));
}

#[test]
fn a_slice_of_records_is_its_little_endian_bytes() {
    let words = [1u32, 0x0102_0304, u32::MAX];
    let le: Vec<u8> = words.iter().flat_map(|it| it.to_le_bytes()).collect();
    assert_eq!(crate::bytes_of_slice(&words), &le[..]);
    let spans = [crate::RelSpan { start: 3, len: 4 }];
    assert_eq!(crate::bytes_of_slice(&spans), &[3, 0, 0, 0, 4, 0, 0, 0]);
    assert_eq!(crate::bytes_of_slice::<u64>(&[]), &[] as &[u8]);
}

std::thread_local! {
    static CHECKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn checks() -> usize {
    CHECKS.with(|it| it.replace(0))
}

/// A `u32` that counts its checks.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(transparent)]
struct Counted(u32);

// SAFETY: a `u32`: no padding, every bit pattern a value; `check` only counts and checks the size.
unsafe impl Record for Counted {
    const SCHEMA: crate::SchemaHash = schema("Counted");
    fn check(bytes: &[u8], _: &[u8]) -> Result<(), FormatError> {
        CHECKS.with(|it| it.set(it.get() + 1));
        match bytes.len() == 4 {
            true => Ok(()),
            false => Err(FormatError::Invalid { at: 0, what: "not a Counted" }),
        }
    }
}

record! {
    #[derive(Clone, Copy, Debug)]
    struct Node: 1 {
        leaf: Rel<Counted>,
        more: Slice<Counted>,
    }
}

record! {
    #[derive(Clone, Copy, Debug)]
    struct Nodes: 1 {
        nodes: Slice<Node>,
    }
}

/// `open` checks each record once; after it, following each reference of the buffer once
/// checks each record once more, where each `get` used to check all its target reaches again.
#[test]
fn a_get_after_open_checks_only_what_it_reaches_directly() {
    const NODES: usize = 50;
    let mut out = Writer::new(Nodes::SCHEMA);
    let parts: Vec<(u64, u64)> = (0..NODES as u32)
        .map(|n| (out.push(&Counted(n)), out.push_slice(&[Counted(10 * n), Counted(10 * n + 1), Counted(10 * n + 2)])))
        .collect();
    let at = out.next::<Node>();
    let nodes: Vec<Node> = parts
        .iter()
        .enumerate()
        .map(|(n, &(leaf, more))| {
            let node = at + (n * size_of::<Node>()) as u64;
            Node {
                leaf: Rel::between(node + offset_of!(Node, leaf) as u64, leaf),
                more: Slice::between(node + offset_of!(Node, more) as u64, more, 3),
            }
        })
        .collect();
    assert_eq!(out.push_slice(&nodes), at);
    let root_at = out.next::<Nodes>();
    let root = Nodes { nodes: Slice::between(root_at + offset_of!(Nodes, nodes) as u64, at, NODES) };
    assert_eq!(out.push(&root), root_at);
    let buffer = Aligned::copy_from(&out.finish(root_at));

    checks();
    let root = open::<Nodes>(&buffer).unwrap();
    assert_eq!(checks(), 4 * NODES, "open checks every Counted once");
    let nodes = root.nodes.get(&buffer);
    assert_eq!(checks(), 0, "a slice of records holding only references is followed in O(1)");
    for (n, node) in nodes.iter().enumerate() {
        let n = n as u32;
        assert_eq!(node.leaf.get(&buffer), Some(&Counted(n)));
        assert_eq!(node.more.get(&buffer), &[Counted(10 * n), Counted(10 * n + 1), Counted(10 * n + 2)]);
    }
    assert_eq!(checks(), 4 * NODES, "each Counted checked once more, where it is reached");
}

#[test]
fn a_reused_buffer_writes_the_same_bytes_in_its_own_allocation() {
    let mut used = written(1);
    used.extend_from_slice(&[0xff; 4096]);
    let (at, capacity) = (used.as_ptr(), used.capacity());
    let again = written_by(Writer::with_buffer(Root::SCHEMA, used), 1);
    assert_eq!(again, written(1));
    assert_eq!((again.as_ptr(), again.capacity()), (at, capacity));
}

#[test]
fn a_buffer_reads_the_same_wherever_it_is() {
    let bytes = written(1);
    let (a, b) = (Aligned::copy_from(&bytes), Aligned::copy_from(&bytes));
    let (ra, rb) = (open::<Root>(&a).unwrap(), open::<Root>(&b).unwrap());
    assert_eq!(ra.numbers.get(&a), rb.numbers.get(&b));
    assert_eq!(ra.leaf.get(&a), rb.leaf.get(&b));
}

#[test]
fn a_reader_of_another_schema_refuses() {
    let buffer = Aligned::copy_from(&written(1));
    assert_eq!(
        open_as::<Root>(&buffer, schema("root 2")).unwrap_err(),
        FormatError::Schema { found: Root::SCHEMA, wanted: schema("root 2") }
    );
    assert!(matches!(open::<Leaf>(&buffer), Err(FormatError::Schema { .. })));
}

#[test]
fn not_a_buffer_is_refused() {
    let bytes = written(1);
    assert!(matches!(open::<Root>(&Aligned::copy_from(&bytes[..20])), Err(FormatError::Header(_))));
    let mut magic = bytes.clone();
    magic[0] = b'X';
    assert!(matches!(open::<Root>(&Aligned::copy_from(&magic)), Err(FormatError::Header(_))));
    let mut longer = bytes.clone();
    longer.extend([0; 16]);
    assert!(matches!(open::<Root>(&Aligned::copy_from(&longer)), Err(FormatError::Header(_))));
    let mut shifted = vec![0];
    shifted.extend(&bytes);
    let shifted = Aligned::copy_from(&shifted);
    assert_eq!(
        open::<Root>(&shifted[1..]).unwrap_err(),
        FormatError::Header("not at an address aligned to 16 bytes")
    );
}

#[test]
fn an_invalid_value_is_refused() {
    let buffer = Aligned::copy_from(&written(2));
    assert!(matches!(open::<Root>(&buffer), Err(FormatError::Invalid { .. })));
}

fn with_root(edit: impl FnOnce(&mut Vec<u8>, usize)) -> Aligned {
    let mut bytes = written(1);
    let root = header(&Aligned::copy_from(&bytes)).unwrap().root as usize;
    edit(&mut bytes, root);
    Aligned::copy_from(&bytes)
}

fn set_i32(bytes: &mut [u8], at: usize, value: i32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

#[test]
fn a_reference_that_leaves_is_refused() {
    let forward = with_root(|bytes, root| set_i32(bytes, root + offset_of!(Root, leaf), 8));
    assert!(matches!(open::<Root>(&forward), Err(FormatError::OutOfBounds { .. })));
    let into_header = with_root(|bytes, root| {
        set_i32(bytes, root + offset_of!(Root, leaf), -(root as i32) + 8)
    });
    assert!(matches!(open::<Root>(&into_header), Err(FormatError::OutOfBounds { .. })));
    let misaligned = with_root(|bytes, root| {
        let at = root + offset_of!(Root, numbers);
        let offset = i32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
        set_i32(bytes, at, offset + 1)
    });
    assert!(matches!(open::<Root>(&misaligned), Err(FormatError::OutOfBounds { .. })));
    let too_long = with_root(|bytes, root| {
        let at = root + offset_of!(Root, numbers) + 4;
        bytes[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    });
    assert!(matches!(open::<Root>(&too_long), Err(FormatError::OutOfBounds { .. })));
    let root_out = with_root(|bytes, _| {
        let at = offset_of!(crate::Header, root);
        bytes[at..at + 8].copy_from_slice(&(HEADER_LEN as u64 - 8).to_le_bytes());
    });
    assert!(matches!(open::<Root>(&root_out), Err(FormatError::OutOfBounds { .. })));
}

#[test]
fn text_that_is_not_utf8_is_refused() {
    let buffer = with_root(|bytes, root| {
        let at = root + offset_of!(Root, name);
        let offset = i32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
        bytes[(at as i64 + offset as i64) as usize] = 0xff;
    });
    let root = open::<Root>(&buffer).unwrap();
    assert!(matches!(root.name.as_str(&buffer), Err(FormatError::Invalid { .. })));
}

#[test]
#[should_panic(expected = "leaves the buffer")]
fn following_into_another_buffer_panics() {
    let (a, b) = (Aligned::copy_from(&written(1)), Aligned::copy_from(&written(1)));
    let root = open::<Root>(&a).unwrap();
    root.numbers.get(&b);
}

#[test]
fn spans_round_trip() {
    let anchor = ContentId::of("blaze v2 test 1", b"fn main");
    let mut spans = Spans::new(anchor);
    let a = spans.push(0, 7);
    let b = spans.push(3, 2);
    assert_eq!(b, SpanRef { anchor, index: 1 });
    let bytes = Aligned::copy_from(&to_bytes(&spans));
    let read = Spans::read(&bytes).unwrap();
    assert_eq!(read.anchor, anchor);
    assert_eq!(read.get(a), spans.get(a));
    assert_eq!(read.get(b).map(|it| (it.start, it.len)), Some((3, 2)));
    let other = SpanRef { anchor: ContentId::of("blaze v2 test 1", b"fn other"), index: 0 };
    assert_eq!(read.get(other), None);
    assert_eq!(read.to_spans(), spans);
    let empty = Spans::new(anchor);
    let bytes = Aligned::copy_from(&to_bytes(&empty));
    assert_eq!(Spans::read(&bytes).unwrap().spans, &[]);
}

#[test]
fn schemas_name_layouts() {
    assert_ne!(Root::SCHEMA, Leaf::SCHEMA);
    assert_ne!(<Rel<u32> as Record>::SCHEMA, <Rel<u64> as Record>::SCHEMA);
    assert_ne!(<Rel<u32> as Record>::SCHEMA, <Slice<u32> as Record>::SCHEMA);
    assert_ne!(<[u8; 4] as Record>::SCHEMA, <[u8; 5] as Record>::SCHEMA);
    assert_ne!(<Spans as Persist>::SCHEMA, <SpanRef as Record>::SCHEMA);
}

record! {
    #[derive(Clone, Copy, Debug)]
    struct Named: 1 {
        sym: crate::Sym,
        pad: u32,
    }
}

// A tree's kids are their own record, not `Tree`: a record holding a slice of itself has no
// `SCHEMA`, which would hash its own hash.
record! {
    #[derive(Clone, Copy, Debug)]
    struct Kid: 1 {
        name: crate::Sym,
        pad: u32,
        path: Slice<crate::Sym>,
        named: Rel<Named>,
        pad2: u32,
    }
}

record! {
    #[derive(Clone, Copy, Debug)]
    struct Tree: 1 {
        name: crate::Sym,
        pad: u32,
        kids: Slice<Kid>,
        path: Slice<crate::Sym>,
        named: Rel<Named>,
        pad2: u32,
    }
}

/// Every `Sym` a value reaches is found, inline, behind a `Rel`, in a slice of plain records
/// and in a slice of records that hold them behind references; a type that holds none is not
/// walked.
#[test]
fn symbols_are_found_wherever_a_value_holds_them() {
    use crate::Sym;
    const { assert!(<Sym as Record>::SYMBOLS && <Sym as Record>::PLAIN) };
    const { assert!(<Named as Record>::PLAIN && <Named as Record>::SYMBOLS_INLINE) };
    const { assert!(<Kid as Record>::SYMBOLS && <Kid as Record>::SYMBOLS_INLINE) };
    const { assert!(<Tree as Record>::SYMBOLS && <Tree as Record>::SYMBOLS_INLINE) };
    const { assert!(!<Slice<crate::Sym> as Record>::SYMBOLS_INLINE) };
    const { assert!(<Slice<Kid> as Record>::SYMBOLS) };
    const { assert!(<Slice<Named> as Record>::SYMBOLS && <[Sym; 2] as Record>::SYMBOLS) };
    const { assert!(!<u32 as Record>::SYMBOLS && !<Slice<u32> as Record>::SYMBOLS) };
    const { assert!(!<crate::RelSpan as Record>::SYMBOLS && !<Rel<Leaf> as Record>::SYMBOLS_INLINE) };

    let mut out = Writer::new(Tree::SCHEMA);
    let path = out.push_slice(&[Sym(11), Sym(12)]);
    let named = out.push(&Named { sym: Sym(20), pad: 0 });
    let kid_at = out.next::<Kid>();
    let kid = Kid {
        name: Sym(10),
        pad: 0,
        path: Slice::between(kid_at + offset_of!(Kid, path) as u64, path, 2),
        named: Rel::NONE,
        pad2: 0,
    };
    assert_eq!(out.push_slice(&[kid]), kid_at);
    let at = out.next::<Tree>();
    let root = Tree {
        name: Sym(1),
        pad: 0,
        kids: Slice::between(at + offset_of!(Tree, kids) as u64, kid_at, 1),
        path: Slice::EMPTY,
        named: Rel::between(at + offset_of!(Tree, named) as u64, named),
        pad2: 0,
    };
    assert_eq!(out.push(&root), at);
    let buffer = Aligned::copy_from(&out.finish(at));
    let mut found = Vec::new();
    crate::symbols_of::<Tree>(&buffer, Tree::SCHEMA, &mut found).unwrap();
    found.sort_unstable();
    assert_eq!(found, [1, 10, 11, 12, 20]);
    assert!(matches!(
        crate::symbols_of::<Tree>(&buffer, Named::SCHEMA, &mut found),
        Err(FormatError::Schema { .. })
    ));
}
