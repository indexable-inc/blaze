//! A record's relocation table, written as [`crate::nameres::frozen`] writes what it names, and
//! the words of an expression store buffer that hold an id without going through
//! [`crate::hir::persist::Words`], moved to and from the table's indexes.
//!
//! Each entry of a table is its kind and the portable form of its id: a crate by its index in
//! the closure, a file by its path from a crate's root, a macro call and a syntax context as
//! the location they intern (each once, then by index), an item as its location. A block is
//! its ast id, the body that made it (none for a signature's), and its module: a module of the
//! crate's `DefMap`, or the root of the `DefMap` of a block the table names before it. Reading
//! interns each again in this database.
//!
//! A store's buffer holds five kinds of word outside its words: each block of `block_scopes`
//! (its bits), each macro call of `macro_calls` (its bits), the block of each `Expr::Block`
//! (two words of `extra`), and the syntax context of each ident and type of its hygiene tables.
//! Written, each becomes its index in the table (a block's or a call's plus one, so that none is
//! 0); read, the index is its id in this database
//! again, in a copy of the buffer. A store with none is read in place.

use base_db::{Crate, Db};
use blaze_format::{Aligned, FormatError, Persist, open_as};
use hir_expand::MacroCallId;
use blaze_query::db::{AsId, FromId};
use syntax::ast;

use crate::{
    AdtId, AstId, BlockId, BlockIdLt, LoweringMode, MacroId, ModuleDefId, ModuleId,
    expr_store::{ExpressionStore, persist::StoreRoot},
    hir::{
        columns::ExprKind,
        persist::{Kind, Relocation, entry_id, syntax_context},
    },
    nameres::{
        block_def_map,
        frozen::{Dec, Enc},
    },
};

type R<T = ()> = Result<T, String>;

fn id<T: FromId>(bits: u64) -> T {
    T::from_id(entry_id(bits))
}

/// Appends to `out` the macro calls `table` names: what a record of it is made from beyond
/// its crate's syntax.
pub(super) fn calls(table: &Relocation, out: &mut Vec<MacroCallId>) {
    for &(kind, bits) in table.entries() {
        if kind == Kind::MacroCall {
            out.push(id(bits));
        }
    }
}

/// The bytes of `table`, empty for an empty table.
pub(super) fn encode(db: &Db, krate: Crate, table: &Relocation) -> R<Vec<u8>> {
    if table.is_empty() {
        // alloc: none; an empty `Vec` holds no allocation.
        return Ok(Vec::new());
    }
    let mut enc = Enc::new(db, krate);
    enc.usize(table.len())?;
    for (index, &(kind, bits)) in table.entries().iter().enumerate() {
        enc.u8(kind as u8);
        match kind {
            Kind::Crate => enc.krate(id(bits))?,
            Kind::File => enc.file(id(bits))?,
            Kind::MacroCall => enc.call(id::<MacroCallId>(bits))?,
            Kind::Block => block(&mut enc, db, table, index, id(bits))?,
            Kind::Function => enc.def(ModuleDefId::FunctionId(id(bits)))?,
            Kind::Struct => enc.def(ModuleDefId::AdtId(AdtId::StructId(id(bits))))?,
            Kind::Union => enc.def(ModuleDefId::AdtId(AdtId::UnionId(id(bits))))?,
            Kind::Enum => enc.def(ModuleDefId::AdtId(AdtId::EnumId(id(bits))))?,
            Kind::Const => enc.def(ModuleDefId::ConstId(id(bits)))?,
            Kind::Static => enc.def(ModuleDefId::StaticId(id(bits)))?,
            Kind::Trait => enc.def(ModuleDefId::TraitId(id(bits)))?,
            Kind::TypeAlias => enc.def(ModuleDefId::TypeAliasId(id(bits)))?,
            Kind::Impl => enc.impl_id(id(bits))?,
            Kind::EnumVariant => enc.variant(id(bits))?,
            Kind::Macro2 => enc.def(ModuleDefId::MacroId(MacroId::Macro2Id(id(bits))))?,
            Kind::MacroRules => enc.def(ModuleDefId::MacroId(MacroId::MacroRulesId(id(bits))))?,
            Kind::ProcMacro => enc.def(ModuleDefId::MacroId(MacroId::ProcMacroId(id(bits))))?,
            Kind::Context => {
                let context = syntax_context(bits as u32).ok_or("a syntax context of u32::MAX")?;
                enc.context(context)?;
            }
            Kind::SpanFile => {
                let file = span::EditionedFileId::from_raw(bits as u32);
                enc.file(base_db::EditionedFileId::from_span_file_id(db, file))?;
            }
        }
    }
    enc.finish()
}

/// Writes `block`, entry `index` of `table`.
fn block(enc: &mut Enc<'_>, db: &Db, table: &Relocation, index: usize, block: BlockId) -> R {
    enc.ast_id(block.ast_id(db))?;
    match block.owner(db) {
        None => enc.u8(0),
        Some(owner) => {
            enc.u8(1);
            enc.body_owner(owner)?;
        }
    }
    let module = block.module(db);
    let Some(outer) = module.block(db) else {
        enc.u8(0);
        return enc.module(ModuleId::from_id(module.as_id()));
    };
    if block_def_map(db, outer).root_module_id().as_id() != module.as_id() {
        // alloc: the reason of a corrupt image's panic, or of a part that fails.
        return Err("a block in a module of a block other than its root".to_owned());
    }
    let outer = (Kind::Block, u64::from(outer.as_id().as_u32()));
    let at = (table.entries()[..index].iter().position(|&it| it == outer))
        .ok_or("a block in a block the table names after it, or not at all")?;
    enc.u8(1);
    enc.u32(at as u32);
    Ok(())
}

/// The table of `bytes`, [`encode`]'s, each id made again in this database. Run inside the
/// query whose value holds what the table names: a block is a tracked struct, made here.
pub(super) fn decode(db: &Db, krate: Crate, bytes: &[u8]) -> R<Relocation> {
    if bytes.is_empty() {
        return Ok(Relocation::new());
    }
    let mut dec = Dec::new_checked(db, krate, bytes)?;
    let len = dec.len()?;
    let mut entries: Vec<(Kind, u64)> = Vec::with_capacity(len.min(bytes.len()));
    for _ in 0..len {
        let tag = dec.u8()?;
        // alloc: the reason of a corrupt image's panic, or of a part that fails.
        let kind = *Kind::ALL.get(usize::from(tag)).ok_or_else(|| format!("id kind {tag}"))?;
        let bits = match kind {
            Kind::Crate => bits(dec.krate()?),
            Kind::File => bits(dec.file()?),
            Kind::MacroCall => bits(dec.call()?),
            Kind::Block => read_block(&mut dec, db, &entries)?,
            Kind::Impl => bits(dec.impl_id()?),
            Kind::EnumVariant => bits(dec.variant()?),
            Kind::Context => u64::from(dec.context()?.into_u32()),
            Kind::SpanFile => u64::from(dec.file()?.span_file_id(db).as_u32()),
            Kind::Function
            | Kind::Struct
            | Kind::Union
            | Kind::Enum
            | Kind::Const
            | Kind::Static
            | Kind::Trait
            | Kind::TypeAlias
            | Kind::Macro2
            | Kind::MacroRules
            | Kind::ProcMacro => def_bits(kind, dec.def()?)?,
        };
        entries.push((kind, bits));
    }
    dec.finish()?;
    Ok(Relocation::of(entries))
}

fn bits(it: impl AsId) -> u64 {
    u64::from(it.as_id().as_u32())
}

/// The bits of `def`, which must be of `kind`.
fn def_bits(kind: Kind, def: ModuleDefId) -> R<u64> {
    let found = match (kind, def) {
        (Kind::Function, ModuleDefId::FunctionId(it)) => bits(it),
        (Kind::Struct, ModuleDefId::AdtId(AdtId::StructId(it))) => bits(it),
        (Kind::Union, ModuleDefId::AdtId(AdtId::UnionId(it))) => bits(it),
        (Kind::Enum, ModuleDefId::AdtId(AdtId::EnumId(it))) => bits(it),
        (Kind::Const, ModuleDefId::ConstId(it)) => bits(it),
        (Kind::Static, ModuleDefId::StaticId(it)) => bits(it),
        (Kind::Trait, ModuleDefId::TraitId(it)) => bits(it),
        (Kind::TypeAlias, ModuleDefId::TypeAliasId(it)) => bits(it),
        (Kind::Macro2, ModuleDefId::MacroId(MacroId::Macro2Id(it))) => bits(it),
        (Kind::MacroRules, ModuleDefId::MacroId(MacroId::MacroRulesId(it))) => bits(it),
        (Kind::ProcMacro, ModuleDefId::MacroId(MacroId::ProcMacroId(it))) => bits(it),
        // alloc: the reason of a corrupt image's panic, or of a part that fails.
        _ => return Err(format!("an item of another kind than {kind:?}")),
    };
    Ok(found)
}

/// A block [`block`] wrote, made again; `entries` are those read before it.
fn read_block(dec: &mut Dec<'_>, db: &Db, entries: &[(Kind, u64)]) -> R<u64> {
    let ast_id: AstId<ast::BlockExpr> = dec.ast_id()?;
    let owner = match dec.u8()? {
        0 => None,
        1 => Some(dec.body_owner()?),
        // alloc: the reason of a corrupt image's panic, or of a part that fails.
        tag => return Err(format!("block owner tag {tag}")),
    };
    let module = match dec.u8()? {
        0 => dec.module()?,
        1 => {
            let at = dec.u32()? as usize;
            let outer = match entries.get(at) {
                Some(&(Kind::Block, bits)) => id::<BlockId>(bits),
                // alloc: the reason of a corrupt image's panic, or of a part that fails.
                _ => return Err(format!("block {at} of a table, which is no block read before")),
            };
            block_def_map(db, outer).root_module_id()
        }
        // alloc: the reason of a corrupt image's panic, or of a part that fails.
        tag => return Err(format!("block module tag {tag}")),
    };
    let token = LoweringMode::Analysis.allow_tracked_structs().expect("analysis makes blocks");
    Ok(bits(BlockIdLt::new(db, ast_id, module, owner, token)))
}

/// Where a store's buffer holds an id outside its words.
#[derive(Default)]
struct Sites {
    /// Each `block_scopes` entry, a `u64`.
    scopes: Vec<usize>,
    /// Each `macro_calls` entry, a `u64`.
    calls: Vec<usize>,
    /// The two words of each `Expr::Block`'s id, the low one first.
    blocks: Vec<usize>,
    /// Each ident and type hygiene context, a `u32`.
    contexts: Vec<usize>,
}

impl Sites {
    fn is_empty(&self) -> bool {
        self.scopes.is_empty()
            && self.calls.is_empty()
            && self.blocks.is_empty()
            && self.contexts.is_empty()
    }
}

fn place<T>(item: &T, buffer: &[u8]) -> usize {
    (item as *const T as usize) - (buffer.as_ptr() as usize)
}

/// The sites of the store buffer `buffer`.
fn sites(buffer: &[u8]) -> Result<Sites, FormatError> {
    let root = open_as::<StoreRoot>(buffer, <ExpressionStore as Persist>::SCHEMA)?;
    let mut sites = Sites::default();
    sites.scopes.extend(root.block_scopes.follow(buffer)?.iter().map(|it| place(it, buffer)));
    sites.calls.extend(root.macro_calls.follow(buffer)?.iter().map(|it| place(it, buffer)));
    let (kinds, data) = (root.expr_kind.follow(buffer)?, root.expr_data.follow(buffer)?);
    let extra = root.extra.follow(buffer)?;
    for (&kind, &at) in kinds.iter().zip(data) {
        if kind == ExprKind::Block as u8 {
            let low = extra.get(at as usize..at as usize + 2).ok_or(FormatError::Invalid {
                at: place(&kinds[0], buffer) as u64,
                what: "a block whose words are past `extra`",
            })?;
            sites.blocks.push(place(&low[0], buffer));
        }
    }
    for pairs in [root.ident_hygiene.follow(buffer)?, root.type_hygiene.follow(buffer)?] {
        sites.contexts.extend(pairs.iter().map(|it| place(&it.b, buffer)));
    }
    Ok(sites)
}

fn word(bytes: &[u8], at: usize) -> u32 {
    u32::from_ne_bytes(bytes[at..at + 4].try_into().expect("four bytes"))
}

fn set_word(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_ne_bytes());
}

fn wide(bytes: &[u8], at: usize) -> u64 {
    u64::from_ne_bytes(bytes[at..at + 8].try_into().expect("eight bytes"))
}

fn set_wide(bytes: &mut [u8], at: usize, value: u64) {
    bytes[at..at + 8].copy_from_slice(&value.to_ne_bytes());
}

/// Moves each id of the store buffer `bytes`, which this process wrote, to its index in
/// `table`, adding those it lacks.
pub(crate) fn relocate_store(bytes: &mut [u8], table: &mut Relocation) {
    let sites = sites(&Aligned::copy_from(bytes)).expect("a store buffer just written reads");
    for at in sites.scopes {
        let index = table.index(Kind::Block, wide(bytes, at));
        set_wide(bytes, at, u64::from(index) + 1);
    }
    for at in sites.calls {
        let index = table.index(Kind::MacroCall, wide(bytes, at));
        set_wide(bytes, at, u64::from(index) + 1);
    }
    for at in sites.blocks {
        let bits = u64::from(word(bytes, at)) | (u64::from(word(bytes, at + 4)) << 32);
        if bits != 0 {
            let index = table.index(Kind::Block, bits);
            set_word(bytes, at, index + 1);
            set_word(bytes, at + 4, 0);
        }
    }
    for at in sites.contexts {
        let index = table.index(Kind::Context, u64::from(word(bytes, at)));
        set_word(bytes, at, index);
    }
}

/// A store buffer as this database reads it.
pub(super) enum StoreBytes<'a> {
    InPlace(&'a [u8]),
    Moved(Aligned),
}

impl std::ops::Deref for StoreBytes<'_> {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match self {
            StoreBytes::InPlace(it) => it,
            StoreBytes::Moved(it) => it,
        }
    }
}

/// The store buffer `bytes` of a record, [`relocate_store`]'s, with each id of `table` in its
/// place: in place when it holds none.
pub(super) fn unrelocate_store<'a>(
    bytes: &'a [u8],
    table: &Relocation,
) -> Result<StoreBytes<'a>, FormatError> {
    let sites = sites(bytes)?;
    if sites.is_empty() {
        return Ok(StoreBytes::InPlace(bytes));
    }
    let invalid = |at: usize, what| FormatError::Invalid { at: at as u64, what };
    // The record is read-only, so a store that names blocks, calls or contexts is patched here.
    // alloc: one copy of a store that names ids, once a read
    let mut moved = bytes.to_vec();
    for at in sites.scopes {
        let index = wide(&moved, at).checked_sub(1).and_then(|it| u32::try_from(it).ok());
        let bits = index.and_then(|index| table.get(Kind::Block, index));
        let bits = bits.ok_or_else(|| invalid(at, "a block scope not in the table"))?;
        set_wide(&mut moved, at, bits);
    }
    for at in sites.calls {
        let index = wide(&moved, at).checked_sub(1).and_then(|it| u32::try_from(it).ok());
        let bits = index.and_then(|index| table.get(Kind::MacroCall, index));
        let bits = bits.ok_or_else(|| invalid(at, "a macro call not in the table"))?;
        set_wide(&mut moved, at, bits);
    }
    for at in sites.blocks {
        match (word(&moved, at), word(&moved, at + 4)) {
            (0, 0) => {}
            (index, 0) => {
                let bits = table.get(Kind::Block, index - 1);
                let bits = bits.ok_or_else(|| invalid(at, "a block not in the table"))?;
                set_word(&mut moved, at, bits as u32);
                set_word(&mut moved, at + 4, (bits >> 32) as u32);
            }
            _ => return Err(invalid(at, "a block that is not an index of the table")),
        }
    }
    for at in sites.contexts {
        let bits = table.get(Kind::Context, word(&moved, at));
        let bits = bits.ok_or_else(|| invalid(at, "a syntax context not in the table"))?;
        set_word(&mut moved, at, bits as u32);
    }
    Ok(StoreBytes::Moved(Aligned::copy_from(&moved)))
}
