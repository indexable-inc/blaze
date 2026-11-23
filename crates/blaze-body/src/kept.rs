//! The kept body: what the Bodies stage leaves beside its facts record (docs/v2/plan/design/
//! run.md, section R, piece R4; D119): the checked generic body in the flat form of
//! [`crate::record`] with the portable type table it indexes, in one blaze-format buffer. A
//! run builds a called instance from it (`blaze_body_build::build(kept.verified(),
//! kept.tys(), ..)`) and lowers nothing again.
//!
//! Two promises, each checked once:
//! - a body is written only beside the table it was built against ([`encode`]): the body
//!   names its table ([`TableId`], set by [`crate::BodyBuilder::new_with_types_in`]), so two
//!   tables of one size swapped are refused before any byte is written;
//! - a kept record opens ([`open`]) only when the table it holds is the one it names, the
//!   body's bases are that table's counts and the body is well formed ([`crate::verify_ref`]):
//!   the [`Kept`] it returns carries the [`Verified`] proof, so its readers check nothing again
//!   (rule 74).
//!
//! The body's own record bytes ([`crate::record::BodyRoot`]) are unchanged: the binding is the
//! kept record's `table`, beside the body, not inside it.
#![blaze::sans_io]

use blaze_format_core::{FormatError, Own, Placed, Record, Slice, Unfit, Writer, WriterOpts, open_as};
use blaze_own::Vec;
use blaze_typeck::{ContentHash, PNode, PTy, PTys, StableHasher};
use core::alloc::Allocator;
use core::mem::offset_of;

use crate::body::{BodyRef, NONE};
use crate::code::{TyWords, decode_ty, encode_ty};
use crate::record::{BodyRoot, EncodeError, OpenError, body_of, write_root};
use crate::verify::{Verified, VerifyError, verify_ref};
use crate::Body;

/// What a kept record's fields mean: raised whenever one changes meaning. A record of another
/// version is refused and the body checked again (rule 26). The body inside carries its own
/// [`crate::VERSION`], checked as its own record's is.
pub const KEPT_VERSION: u32 = 1;

/// The hasher domain of a [`TableId`]: no other value hashes alike by construction.
const TABLE_DOMAIN: &str = "blaze-body.kept.table.v1";

/// The name of one portable type table by its content (nodes then list entries, as
/// blaze-typeck's `ContentHash for PTys` feeds them): two tables of one size but other nodes
/// have other names. 64-bit FNV today (blaze-typeck hash.rs), becoming a `ContentId` with it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct TableId(u64);

impl TableId {
    /// The name of `table`.
    pub fn of(table: PTys<'_>) -> TableId {
        let mut h = StableHasher::new(TABLE_DOMAIN);
        table.hash_into(&mut h);
        TableId(h.finish())
    }

    /// The name as a number, as the record writes it.
    pub fn get(self) -> u64 {
        self.0
    }
}

blaze_format_core::record! {
    /// A kept record's root: the version, the table's name, the body's root (one), then the
    /// table's two columns, nodes in blaze-body's type words ([`crate::code::encode_ty`]) and
    /// list entries as handles.
    #[derive(Clone, Copy)]
    pub struct KeptRoot: 1 {
        pub version: u32,
        pub reserved: u32,
        pub table: u64,
        pub body: Slice<BodyRoot>,
        pub nodes: Slice<TyWords>,
        pub lists: Slice<u32>,
    }
}

/// A column of the kept record, as a refusal names it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KeptColumn {
    Body,
    Nodes,
    Lists,
}

/// Why a body could not be kept, or bytes are not a kept body. Each names the row or the
/// tables it is about; the caller adds the body's item (rule 19).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum KeptError {
    /// The body was built with no type table ([`crate::BodyBuilder::new_in`], or parsed), so
    /// nothing says which table its handles index: only a body from
    /// [`crate::BodyBuilder::new_with_types_in`] is kept.
    Unbound,
    /// The body was built against the table named `body` and is offered with the table named
    /// `given`: written together the handles would read another table's types.
    WrongTable { body: TableId, given: TableId },
    /// The table's node at `row` has no exact form in blaze-body's type words.
    Unwritable { row: u32 },
    /// The body's own tables passed the offset's reach.
    Body(EncodeError),
    /// A column of the kept record passed the offset's reach.
    TooFar { column: KeptColumn, unfit: Unfit },
    /// Not a kept record: blaze-format's check of the layout refused it.
    Format(FormatError),
    /// A kept record of another version.
    Version { found: u32, want: u32 },
    /// The record holds `found` body roots, not one.
    BodyCount { found: u32 },
    /// The body inside is not a body record of this version.
    Open(OpenError),
    /// The table's node at `row` decodes to no type.
    BadNode { row: u32 },
    /// The record names table `written` but holds table `found`: its bytes were changed, or a
    /// writer put one body's table name beside another table.
    TableMismatch { written: u64, found: TableId },
    /// The body's overlay starts at `base` (types, or list entries when `lists`), not after the
    /// table's `count`.
    Base { lists: bool, base: u32, count: u32 },
    /// The body inside is not well formed.
    Verify(VerifyError),
}

impl From<FormatError> for KeptError {
    fn from(e: FormatError) -> KeptError {
        KeptError::Format(e)
    }
}

/// Writes `body` with `table`, the portable type table it was built against (the checked
/// record's, `ck.types.view()`), as one kept record in `alloc`. Refused when the body names no
/// table or another one ([`KeptError::WrongTable`]); equal bodies and tables give equal bytes
/// (rule 25).
pub fn encode<A: Allocator, B: Allocator>(body: &Body<B>, table: PTys<'_>, alloc: A) -> Result<Vec<u8, A>, KeptError> {
    let given = TableId::of(table);
    match body.table() {
        None => Err(KeptError::Unbound),
        Some(named) if named != given => Err(KeptError::WrongTable { body: named, given }),
        Some(named) => write(&body.view(), named, table, alloc),
    }
}

/// The same bytes again from an opened record: what a restore is checked equal to a fresh
/// build by (rule 26).
pub fn encode_ref<A: Allocator, B: Allocator>(kept: &Kept<'_, B>, alloc: A) -> Result<Vec<u8, A>, KeptError> {
    write(&kept.verified.body(), kept.table, kept.tys(), alloc)
}

/// The writer under [`encode`] and [`encode_ref`]: the body's tables and root, the table's two
/// columns, then the kept root naming `named`. Trusts `named` to be `table`'s name; the public
/// writers make sure of it, and [`open`] checks it.
fn write<A: Allocator>(body: &BodyRef<'_>, named: TableId, table: PTys<'_>, alloc: A) -> Result<Vec<u8, A>, KeptError> {
    for (row, n) in table.nodes().enumerate() {
        if decode_ty(encode_ty(*n)) != Some(*n) {
            return Err(KeptError::Unwritable { row: row as u32 });
        }
    }
    let mut out = Writer::new(WriterOpts { schema: <KeptRoot as Record>::SCHEMA, alloc: Own(alloc), capacity: None });
    let body_at = write_root(&mut out, body).map_err(KeptError::Body)?;
    let body_col = Placed { at: body_at, len: 1 };
    let nodes = Placed::push_each(&mut out, table.len(), table.nodes().map(|n| encode_ty(*n)));
    let lists = Placed::push_each(&mut out, table.lists_len(), table.lists().map(|t| t.0));
    let at = out.next::<KeptRoot>();
    let root = KeptRoot {
        version: KEPT_VERSION,
        reserved: 0,
        table: named.get(),
        body: reach(body_col, at, offset_of!(KeptRoot, body), KeptColumn::Body)?,
        nodes: reach(nodes, at, offset_of!(KeptRoot, nodes), KeptColumn::Nodes)?,
        lists: reach(lists, at, offset_of!(KeptRoot, lists), KeptColumn::Lists)?,
    };
    let written = out.push(&root);
    debug_assert_eq!(written, at);
    Ok(out.finish(at))
}

/// The root's slice of column `p` for the field at `offset` in the root written at `at`.
fn reach<T>(p: Placed, at: u64, offset: usize, column: KeptColumn) -> Result<Slice<T>, KeptError> {
    p.slice_as::<T, i32>(at + offset as u64).map_err(|unfit| KeptError::TooFar { column, unfit })
}

/// A kept body, opened and checked: the body read in place in the record's bytes, its proof,
/// and its table decoded once into `A`.
pub struct Kept<'b, A: Allocator> {
    verified: Verified<'b>,
    nodes: Vec<PNode, A>,
    lists: Vec<PTy, A>,
    table: TableId,
}

impl<'b, A: Allocator> Kept<'b, A> {
    /// The body's proof: what `blaze_body_build::build` takes.
    pub fn verified(&self) -> Verified<'b> {
        self.verified
    }

    /// The table the body's handles index: what `build` takes as `from`.
    pub fn tys(&self) -> PTys<'_> {
        PTys::flat(&self.nodes, &self.lists)
    }

    /// The table's name.
    pub fn table(&self) -> TableId {
        self.table
    }
}

/// Opens a kept record: its layout checked once (blaze-format), the versions, the table decoded
/// into `alloc` and checked to be the one the record names, the body's bases checked against
/// the table's counts, and the body verified. Every refusal says which check refused.
pub fn open<'b, A: Allocator + Copy>(bytes: &'b [u8], alloc: A) -> Result<Kept<'b, A>, KeptError> {
    let root = open_as::<KeptRoot>(bytes, <KeptRoot as Record>::SCHEMA)?;
    if root.version != KEPT_VERSION {
        return Err(KeptError::Version { found: root.version, want: KEPT_VERSION });
    }
    let bodies: &'b [BodyRoot] = root.body.follow(bytes)?;
    let [body_root] = bodies else {
        return Err(KeptError::BodyCount { found: u32::try_from(bodies.len()).unwrap_or(u32::MAX) });
    };
    let body = body_of(body_root, bytes).map_err(KeptError::Open)?;
    let words: &'b [TyWords] = root.nodes.follow(bytes)?;
    let entries: &'b [u32] = root.lists.follow(bytes)?;
    let mut nodes = Vec::with_capacity_in(words.len(), alloc);
    for (row, w) in words.iter().enumerate() {
        match decode_ty(*w) {
            Some(n) => nodes.push(n),
            None => return Err(KeptError::BadNode { row: row as u32 }),
        }
    }
    let mut lists = Vec::with_capacity_in(entries.len(), alloc);
    for t in entries {
        lists.push(PTy(*t));
    }
    let table = TableId::of(PTys::flat(&nodes, &lists));
    if table.get() != root.table {
        return Err(KeptError::TableMismatch { written: root.table, found: table });
    }
    let (types, list_entries) = (count(nodes.len()), count(lists.len()));
    if body.ty_base != NONE && body.ty_base != types {
        return Err(KeptError::Base { lists: false, base: body.ty_base, count: types });
    }
    if body.list_base != NONE && body.list_base != list_entries {
        return Err(KeptError::Base { lists: true, base: body.list_base, count: list_entries });
    }
    let verified = verify_ref(body).map_err(KeptError::Verify)?;
    Ok(Kept { verified, nodes, lists, table })
}

/// A column's length as a row count; a column past `u32` was refused by the layout check.
fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use std::alloc::Global;

    use blaze_typeck::{PNode, PTys, UintTy};

    use super::{KeptError, TableId, encode, encode_ref, open, write};
    use crate::body::{Mutbl, PTy};
    use crate::{
        BlockKind, Body, BodyBuilder, ConstDecl, ConstKind, LocalDecl, LocalKind, Operand, Span, StmtKind, Value,
    };

    /// A record table: `t0` is `bool`, `t1` is `u8`.
    const A: [PNode; 2] = [PNode::Bool, PNode::Uint(UintTy::U8)];
    /// A table of the same size with the two swapped: `t0` is `u8`, `t1` is `bool`.
    const B: [PNode; 2] = [PNode::Uint(UintTy::U8), PNode::Bool];
    const NO_LISTS: [PTy; 0] = [];

    /// `fn f() -> u8 { bits }`, built against `table`.
    fn returns(bits: u128, table: &[PNode]) -> Body<Global> {
        let mut b = BodyBuilder::new_with_types_in(PTys::flat(table, &NO_LISTS), Global);
        let u8_ = b.intern_ty(PNode::Uint(UintTy::U8)).expect("the table's u8");
        let ret = b.new_local(LocalDecl { ty: u8_, kind: LocalKind::Return, mutbl: Mutbl::Not });
        let entry = b.new_block(BlockKind::Normal);
        b.start(entry);
        let c = b.new_const(ConstDecl { kind: ConstKind::Scalar { bits }, ty: u8_ });
        let dest = b.local_place(ret);
        b.assign(dest, Value::Use(Operand::Const(c)), Span::default());
        b.terminate(StmtKind::Return, Span::default());
        b.finish().expect("a whole body")
    }

    /// R4's first test: the restored body is the fresh one, row for row, with the same table,
    /// so `build` is handed equal inputs; the restore writes the same bytes again (rule 26).
    #[test]
    fn restored_body_is_the_fresh_one() {
        let fresh = returns(1, &A);
        let bytes = encode(&fresh, PTys::flat(&A, &NO_LISTS), Global).expect("kept");
        let kept = open(&bytes, Global).expect("opens");
        let restored = crate::encode(&kept.verified().body(), Global);
        let original = crate::encode(&fresh.view(), Global);
        assert!(restored[..] == original[..], "the restored body's record is the fresh one's");
        assert_eq!(kept.tys().nodes().copied().collect::<std::vec::Vec<_>>(), A);
        assert_eq!(kept.table(), TableId::of(PTys::flat(&A, &NO_LISTS)));
        let again = encode_ref(&kept, Global).expect("re-kept");
        assert!(again[..] == bytes[..], "a restore writes the bytes it was read from");
        // Planted: a body that differs is not equal after the round trip.
        let other = crate::encode(&returns(2, &A).view(), Global);
        assert!(restored[..] != other[..], "the check can tell two bodies apart");
    }

    /// Planted control from RV3 job 4: two fns whose one clean verdict is the same (`-> u8`,
    /// no finding) but return 1 and 2 are two kept records. An identity by verdict alone
    /// would take one for the other.
    #[test]
    fn a_verdict_only_identity_is_caught() {
        let table = PTys::flat(&A, &NO_LISTS);
        let one = encode(&returns(1, &A), table, Global).expect("kept");
        let one_again = encode(&returns(1, &A), table, Global).expect("kept");
        let two = encode(&returns(2, &A), table, Global).expect("kept");
        assert!(one[..] == one_again[..], "one body, one table: one record (rule 25)");
        assert!(one[..] != two[..], "`return 1` and `return 2` keep two records");
    }

    /// Planted control from RV3 job 4: two same-sized tables swapped. Offered with the other
    /// table, the body is refused before a byte is written; a record whose bytes put the other
    /// table under the body's table name is refused at open.
    #[test]
    fn same_sized_tables_swapped_are_refused() {
        let body = returns(1, &A);
        let (a, b) = (PTys::flat(&A, &NO_LISTS), PTys::flat(&B, &NO_LISTS));
        assert_ne!(TableId::of(a), TableId::of(b), "two tables of one size have two names");
        assert!(encode(&body, a, Global).is_ok(), "control: its own table is accepted");
        match encode(&body, b, Global) {
            Err(KeptError::WrongTable { body: named, given }) => {
                assert_eq!((named, given), (TableId::of(a), TableId::of(b)));
            }
            Err(e) => panic!("refused for another reason: {e:?}"),
            Ok(_) => panic!("a body was kept beside another table"),
        }
        // Planted at the byte level: the writer told the body's table name but given B.
        let planted = write(&body.view(), TableId::of(a), b, Global).expect("written");
        match open(&planted, Global) {
            Err(KeptError::TableMismatch { written, found }) => {
                assert_eq!((written, found), (TableId::of(a).get(), TableId::of(b)));
            }
            Err(e) => panic!("refused for another reason: {e:?}"),
            Ok(_) => panic!("a swapped table opened"),
        }
        // Control: the same writer with the matching table opens.
        let honest = write(&body.view(), TableId::of(a), a, Global).expect("written");
        assert!(open(&honest, Global).is_ok());
    }

    /// A body built with no table is not kept: nothing names what its handles index.
    #[test]
    fn a_body_without_a_table_is_refused() {
        let parsed = crate::parse("local _0: t0 return\nbb0:\n  return\n", Global).expect("parses");
        assert_eq!(parsed.table(), None);
        assert!(matches!(encode(&parsed, PTys::flat(&A, &NO_LISTS), Global), Err(KeptError::Unbound)));
        // Control: the same shape built against a table is kept.
        assert!(encode(&returns(1, &A), PTys::flat(&A, &NO_LISTS), Global).is_ok());
    }
}
