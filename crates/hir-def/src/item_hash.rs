//! A body's HIR as the same bytes in every database, for lane item-hash's identities
//! (docs/v2/plan/design/item-hash.md, section 3.3): its [`ExpressionStore`] as
//! [`blaze_format::Persist`] writes it inside a [`relocating`] scope, each id outside the
//! store's words moved to its index in the same table as a signature record's store
//! ([`crate::deps::signatures::table::relocate_store`]), then its parameters. No span is
//! written: a store holds none, its source map is not written.
//!
//! Each id the bytes name is an [`Entry`] of the table, in the order the bytes first name it:
//! what an id is in this database, which the identity hashes by what it is in every database.
//!
//! The bytes are anonymous (section 3.3): every binding's and label's name is written empty,
//! and so are the leading segments of each path the caller hashes as what they resolve to, so
//! that a rename keeps them.

use base_db::{Crate, Db};
use hir_expand::{MacroCallId, mod_path::ModPath, name::Name};
use blaze_query::db::{AsId, FromId};
use span::SyntaxContext;

use crate::{
    BlockId, ConstId, EnumId, EnumVariantId, FunctionId, ImplId, Macro2Id, MacroRulesId,
    ModuleId, ProcMacroId, StaticId, StructId, TraitId, TypeAliasId, UnionId,
    deps::signatures::table::relocate_store,
    expr_store::{Body, ExpressionStore},
    hir::{
        ExprId,
        persist::{Anonymous, Kind, Relocation, anonymous, entry_id, relocating, syntax_context},
    },
    nameres::frozen::Enc,
};

/// An id a body's portable bytes name, by its index in [`PortableBody::entries`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Entry {
    Crate(Crate),
    File(base_db::EditionedFileId),
    MacroCall(MacroCallId),
    Block(BlockId),
    Function(FunctionId),
    Struct(StructId),
    Union(UnionId),
    Enum(EnumId),
    Const(ConstId),
    Static(StaticId),
    Trait(TraitId),
    TypeAlias(TypeAliasId),
    Impl(ImplId),
    EnumVariant(EnumVariantId),
    Macro2(Macro2Id),
    MacroRules(MacroRulesId),
    ProcMacro(ProcMacroId),
    /// A syntax context the store's hygiene tables or words name, the root ones included.
    Context(SyntaxContext),
    /// The file of a span the words name.
    SpanFile(span::EditionedFileId),
}

/// A body written so that the same body is the same bytes in every database: [`portable_body`].
pub struct PortableBody {
    bytes: Vec<u8>,
    table: Relocation,
}

impl PortableBody {
    /// The store's buffer, anonymous, its ids moved to their entries, then the parameters: their
    /// count, each formal and user-written pattern, and `1` and the `self` binding's two or `0`,
    /// each a little-endian `u32`.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn len(&self) -> usize {
        self.table.len()
    }

    pub fn is_empty(&self) -> bool {
        self.table.is_empty()
    }

    /// The ids the bytes name, in the order they first name them: entry `i` is index `i`.
    pub fn entries(&self) -> impl ExactSizeIterator<Item = Entry> + '_ {
        self.table.entries().iter().map(|&(kind, bits)| entry(kind, bits))
    }

    /// What each entry that is no item, variant or crate is in every database, as
    /// [`Enc::fingerprint`] writes it for a body of `krate`: a file by its path from its crate's
    /// root, a macro call and a syntax context as a hash of what they are, a block as its ast
    /// id and module. Each entry is its kind first; one the caller hashes by its key is its kind
    /// alone.
    pub fn fingerprint(&self, db: &Db, krate: Crate) -> Result<Vec<u8>, String> {
        // alloc: the encoder of one identity, its crate's closure once per identity computed
        let mut enc = Enc::new(db, krate);
        enc.fingerprint(|enc| {
            for &(kind, bits) in self.table.entries() {
                enc.u8(kind as u8);
                match kind {
                    Kind::File => enc.file(id(bits))?,
                    Kind::SpanFile => {
                        let file = span::EditionedFileId::from_raw(bits as u32);
                        enc.file(base_db::EditionedFileId::from_span_file_id(db, file))?
                    }
                    Kind::MacroCall => enc.call(id::<MacroCallId>(bits))?,
                    Kind::Context => {
                        let context =
                            syntax_context(bits as u32).ok_or("a syntax context of u32::MAX")?;
                        enc.context(context)?
                    }
                    Kind::Block => {
                        let block: BlockId = id(bits);
                        enc.ast_id(block.ast_id(db))?;
                        enc.module(ModuleId::from_id(block.module(db).as_id()))?
                    }
                    Kind::Crate
                    | Kind::Function
                    | Kind::Struct
                    | Kind::Union
                    | Kind::Enum
                    | Kind::Const
                    | Kind::Static
                    | Kind::Trait
                    | Kind::TypeAlias
                    | Kind::Impl
                    | Kind::EnumVariant
                    | Kind::Macro2
                    | Kind::MacroRules
                    | Kind::ProcMacro => {}
                }
            }
            Ok(())
        })
    }
}

fn id<T: FromId>(bits: u64) -> T {
    T::from_id(entry_id(bits))
}

/// `body` written as [`PortableBody::bytes`] describes, with the table of what it names.
/// `blank` holds, for each path of the store by [`mod_path_index`], how many of its leading
/// segments the caller hashes by what they resolve to; they are written empty. Each literal of
/// `slots` (what a location macro expanded to, whose value the records fill from where the
/// body is) is written as its kind and type, not its value. A named lifetime of `lifetimes`
/// (the owner's lifetime parameters in scope, outermost first) is written as its index there.
pub fn portable_body(
    body: &Body,
    blank: &[u16],
    slots: &[ExprId],
    lifetimes: &[Name],
) -> PortableBody {
    let Body { store, params, self_param } = body;
    let mut table = Relocation::new();
    let mut anonymous_as = Anonymous {
        // alloc: the scope's copy of `blank`, once per identity computed
        blank: blank.to_vec(),
        // alloc: the scope's slots, once per identity of a body that calls a location macro
        slots: slots.iter().map(|it| it.into_raw().into_u32()).collect(),
        // alloc: the scope's copy of `lifetimes`, once per identity computed
        lifetimes: lifetimes.to_vec(),
    };
    anonymous_as.slots.sort_unstable();
    // alloc: the one buffer of the body, once per identity computed
    let mut bytes = anonymous(&mut anonymous_as, || {
        relocating(&mut table, || blaze_format::to_bytes::<ExpressionStore>(store))
    });
    relocate_store(&mut bytes, &mut table);
    let mut word = |word: u32| bytes.extend_from_slice(&word.to_le_bytes());
    word(u32::try_from(params.len()).expect("more than 2^32 parameters"));
    for param in params.iter() {
        word(param.formal.into_raw().into_u32());
        word(param.user_written.into_raw().into_u32());
    }
    match self_param {
        Some(param) => {
            word(1);
            word(param.formal.into_raw().into_u32());
            word(param.user_written.into_raw().into_u32());
        }
        None => word(0),
    }
    PortableBody { bytes, table }
}

/// How many paths `store` has: the length of [`portable_body`]'s `blank` that names each.
pub fn path_count(store: &ExpressionStore) -> usize {
    store.types.paths.mod_paths.len()
}

/// The index of `path` among `store`'s paths, for [`portable_body`]'s `blank`: `None` for a
/// path that is not one of them (one of another store).
pub fn mod_path_index(store: &ExpressionStore, path: &ModPath) -> Option<usize> {
    let all = &store.types.paths.mod_paths[..];
    let start = all.as_ptr() as usize;
    let at = (path as *const ModPath as usize).checked_sub(start)?;
    let size = size_of::<ModPath>();
    (at % size == 0 && at / size < all.len()).then_some(at / size)
}

fn entry(kind: Kind, bits: u64) -> Entry {
    match kind {
        Kind::Crate => Entry::Crate(id(bits)),
        Kind::File => Entry::File(id(bits)),
        Kind::MacroCall => Entry::MacroCall(id(bits)),
        Kind::Block => Entry::Block(id(bits)),
        Kind::Function => Entry::Function(id(bits)),
        Kind::Struct => Entry::Struct(id(bits)),
        Kind::Union => Entry::Union(id(bits)),
        Kind::Enum => Entry::Enum(id(bits)),
        Kind::Const => Entry::Const(id(bits)),
        Kind::Static => Entry::Static(id(bits)),
        Kind::Trait => Entry::Trait(id(bits)),
        Kind::TypeAlias => Entry::TypeAlias(id(bits)),
        Kind::Impl => Entry::Impl(id(bits)),
        Kind::EnumVariant => Entry::EnumVariant(id(bits)),
        Kind::Macro2 => Entry::Macro2(id(bits)),
        Kind::MacroRules => Entry::MacroRules(id(bits)),
        Kind::ProcMacro => Entry::ProcMacro(id(bits)),
        Kind::Context => Entry::Context(
            syntax_context(bits as u32).expect("a context this process wrote is no u32::MAX"),
        ),
        Kind::SpanFile => Entry::SpanFile(span::EditionedFileId::from_raw(bits as u32)),
    }
}
