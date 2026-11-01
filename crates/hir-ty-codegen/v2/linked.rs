//! What the link takes of a build besides its code: the native libraries a crate's
//! `#[link(..)]`s name ([`HirFrontend::linked`]), and the span tables the code's positions
//! name with each instance's debug name ([`HirFrontend::link_inputs`]).

#[cfg(test)]
mod tests;

use super::{HashSource, Hashes, Kind, frontend::HirFrontend};
use base_db::{Crate, Db};
use blaze_codegen::{
    CodegenError, Init, InstanceKey, NativeLib, NativeLibKind, Target, driver::Built,
};
use blaze_format::cursor::Put;
use blaze_hash::{HashMap, HashSet};
use blaze_ids::ContentId;
use blaze_items::{ItemId, ItemKey, key::Resolved};
use hir_def::{
    AttrDefId, DefWithBodyId, ExpressionStoreOwnerId, FunctionId, ItemContainerId, Lookup,
    ModuleDefId,
    attrs::{AttrFlags, NativeLibKind as Written},
    expr_store::Body as BodyStore,
    nameres::crate_def_map,
    signatures::{FunctionSignature, StaticSignature},
};
use hir_ty_mir::mir::MirSpan;
use hir_ty_share::code::{self, TABLES};

/// What [`HirFrontend::link_inputs`] gives the link.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct LinkInputs {
    /// Each span table a [`Target::Position`] or a line of the code names, by its anchor,
    /// sorted, encoded as the link reads it (`blaze_link::position::SpanTable`).
    pub spans: Vec<(ContentId, Vec<u8>)>,
    /// Each instance's name for a debugger and a backtrace, by key.
    pub debug_names: Vec<(InstanceKey, String)>,
}

/// The start of a span table's encoding (`blaze_link::position`'s `MAGIC`).
const SPAN_TABLE_MAGIC: [u8; 8] = *b"BZSPANS1";

impl HirFrontend<'_> {
    /// The native libraries `krate`'s `#[link(..)]`s link it against, as rustc's
    /// `rustc_metadata::native_libs` collects them: each `extern` block's in the order
    /// written, the blocks in the order of their modules, without those whose `cfg(..)` the
    /// crate leaves out. A form blaze can't link is an error naming it.
    pub fn linked(&self, krate: Crate) -> Result<Vec<NativeLib>, CodegenError> {
        self.with(|db, _| linked(db, krate)).map_err(error)
    }

    /// The span tables `built`'s code names and its debug names.
    pub fn link_inputs(&self, built: &Built) -> Result<LinkInputs, CodegenError> {
        let spans = self.with(|db, _| span_tables(db, &self.hashes, built)).map_err(error)?;
        // clone: per link, the names the link is given; the build keeps its own
        Ok(LinkInputs { spans, debug_names: built.debug_names.clone() })
    }
}

fn error(what: String) -> CodegenError {
    CodegenError { unit: InstanceKey(ContentId::default()), item: ItemKey(ContentId::default()), what }
}

fn linked(db: &Db, krate: Crate) -> Result<Vec<NativeLib>, String> {
    let named = || match &krate.extra_data(db).display_name {
        // alloc: an error's name of the crate
        Some(name) => format!("crate `{name}`"),
        // alloc: an error's name of the crate
        None => format!("crate {krate:?}"),
    };
    // alloc: per crate linked, its libraries
    let mut libs = Vec::new();
    for (_, module) in crate_def_map(db, krate).modules(db) {
        for block in module.scope(db).extern_blocks() {
            if let Some(imported) = AttrFlags::wasm_import_module(db, block) {
                // alloc: an error
                return Err(format!(
                    "`#[link(wasm_import_module = \"{imported}\")]` on an `extern` block of {}: \
                     blaze links no wasm imports",
                    named()
                ));
            }
            for lib in AttrFlags::native_libs(db, block).iter() {
                let name = lib.name.as_str();
                let kind = match lib.kind {
                    Written::Unspecified | Written::Dylib => NativeLibKind::Dylib,
                    Written::Static => NativeLibKind::Static { whole: lib.whole_archive, bundle: lib.bundle },
                    Written::Framework => NativeLibKind::Framework,
                    Written::RawDylib => {
                        // alloc: an error
                        return Err(format!(
                            "`#[link(name = \"{name}\", kind = \"raw-dylib\")]` in {}: blaze \
                             links no `raw-dylib` (nor its `link_ordinal` and `import_name_type`)",
                            named()
                        ));
                    }
                    Written::Other(kind) => {
                        // alloc: an error
                        return Err(format!(
                            "`#[link(name = \"{name}\", kind = \"{kind}\")]` in {}: blaze links \
                             no `{kind}` library",
                            named()
                        ));
                    }
                };
                if let Some(on) = lib.as_needed {
                    let sign = if on { '+' } else { '-' };
                    // alloc: an error
                    return Err(format!(
                        "`#[link(name = \"{name}\", modifiers = \"{sign}as-needed\")]` in {}: \
                         blaze passes the linker no `as-needed`",
                        named()
                    ));
                }
                if let Some(imported) = &lib.import_name_type {
                    // alloc: an error
                    return Err(format!(
                        "`#[link(name = \"{name}\", import_name_type = \"{imported}\")]` in {}: \
                         blaze links no `raw-dylib`, which `import_name_type` is for",
                        named()
                    ));
                }
                if name.is_empty() {
                    // alloc: an error
                    return Err(format!("`#[link(name = \"\")]` in {}: a library has a name", named()));
                }
                if lib.whole_archive && !matches!(kind, NativeLibKind::Static { .. }) {
                    // alloc: an error
                    return Err(format!(
                        "`#[link(name = \"{name}\", modifiers = \"+whole-archive\")]` in {}: \
                         only a `static` library is linked whole",
                        named()
                    ));
                }
                // alloc: per library, its name for the link
                libs.push(NativeLib { name: name.to_owned(), kind, verbatim: lib.verbatim });
            }
        }
        for def in module.scope(db).declarations() {
            let (item, container, owner) = match def {
                ModuleDefId::FunctionId(it) => (
                    FunctionSignature::of(db, it).name.as_str(),
                    it.lookup(db).container,
                    AttrDefId::FunctionId(it),
                ),
                ModuleDefId::StaticId(it) => (
                    StaticSignature::of(db, it).name.as_str(),
                    it.lookup(db).container,
                    AttrDefId::StaticId(it),
                ),
                _ => continue,
            };
            if matches!(container, ItemContainerId::ExternBlockId(_))
                && AttrFlags::query(db, owner).contains(AttrFlags::HAS_LINK_ORDINAL)
            {
                // alloc: an error
                return Err(format!(
                    "`#[link_ordinal(..)]` on `{item}` in {}: blaze links no `raw-dylib`, \
                     which imports by ordinal",
                    named()
                ));
            }
        }
    }
    Ok(libs)
}

/// Every span table `built` names, encoded: of each body and function declaration of its
/// items whose anchor its code names. An anchor of no table found is an error naming it.
fn span_tables(db: &Db, hashes: &HashSource, built: &Built) -> Result<Vec<(ContentId, Vec<u8>)>, String> {
    tables(db, hashes, &built.items, named_anchors(built), &built.debug_names)
}

/// The span tables of `items` that `named` lists, encoded as the linker's `SpanTable::decode`
/// reads them: for code loaded in process with no [`Built`] to read the anchors from. An
/// anchor of no table found is an error naming it.
pub fn span_tables_of(
    db: &Db,
    hashes: &HashSource,
    items: &[ItemKey],
    named: impl IntoIterator<Item = ContentId>,
) -> Result<Vec<(ContentId, Vec<u8>)>, String> {
    // alloc: per call, the anchors asked for
    tables(db, hashes, items, named.into_iter().map(|it| (it, NamedBy::Caller)).collect(), &[])
}

/// The span tables of `items` that `named` lists; `debug_names` gives an error the name of
/// a function that names an anchor of none.
fn tables(
    db: &Db,
    hashes: &HashSource,
    items: &[ItemKey],
    mut named: HashMap<ContentId, NamedBy>,
    debug_names: &[(InstanceKey, String)],
) -> Result<Vec<(ContentId, Vec<u8>)>, String> {
    let mut names: HashMap<base_db::FileId, String> = HashMap::default();
    // alloc: per link, a table the code names each
    let mut spans = Vec::with_capacity(named.len());
    let mut owners: HashSet<DefWithBodyId> = HashSet::default();
    let mut declared: HashSet<FunctionId> = HashSet::default();
    // An `asm!` function's and the allocator shim's items are of no crate of the database.
    let mut unresolved: Option<(usize, ItemKey)> = None;
    for &item in items {
        let resolved = blaze_items::resolve(db, item);
        if resolved.is_none() {
            let (count, least) = unresolved.get_or_insert((0, item));
            *count += 1;
            *least = (*least).min(item);
        }
        let owner = match resolved {
            Some(Resolved::Item(ItemId::Function(func))) => {
                declared.insert(func);
                DefWithBodyId::FunctionId(func)
            }
            Some(Resolved::Item(ItemId::Static(it))) => DefWithBodyId::StaticId(it),
            Some(Resolved::Item(ItemId::Const(it))) => DefWithBodyId::ConstId(it),
            Some(Resolved::Variant(it)) => DefWithBodyId::VariantId(it),
            Some(Resolved::Closure { store: ExpressionStoreOwnerId::Body(owner), .. }) => owner,
            // No body: drop glue's ADT, a shim's trait, an item of the allocator shim.
            _ => continue,
        };
        owners.insert(owner);
    }
    for owner in owners {
        let anchors = hashes.anchors(db, owner)?;
        if !anchors.iter().any(|it| named.contains_key(it)) {
            continue;
        }
        let body = positions(db, owner).as_ref().map_err(Clone::clone)?;
        for (table, anchor) in anchors.into_iter().enumerate() {
            if named.remove(&anchor).is_some() {
                let rows = &body.rows[body.starts[table] as usize..body.starts[table + 1] as usize];
                spans.push((anchor, encode(db, hashes, &mut names, rows)?));
            }
        }
    }
    for func in declared {
        let anchor = hashes.declaration(db, func)?.anchor;
        if named.remove(&anchor).is_some() {
            let at = (*crate::declaration_location(db, func))
                // alloc: an error
                .ok_or_else(|| format!("{func:?} is declared nowhere a position can name"))?;
            spans.push((anchor, encode(db, hashes, &mut names, &[at])?));
        }
    }
    if let Some((anchor, by)) = named.iter().min_by_key(|it| *it.0) {
        let name = match by {
            NamedBy::Function { key, .. } => debug_names.iter().find(|it| it.0 == *key).map(|it| it.1.as_str()),
            NamedBy::Static(_) | NamedBy::Caller => None,
        };
        // alloc: an error
        let name = name.map_or_else(String::new, |it| format!(" `{it}`"));
        let unresolved = match unresolved {
            // alloc: an error
            Some((count, least)) => format!(
                "; {count} of the build's {} items resolve to no item of the database, among them \
                 {least:?}",
                items.len()
            ),
            None => String::new(),
        };
        // alloc: an error
        return Err(format!(
            "{} span tables named by the code are of no body or declaration of the build's {} \
             items, among them {anchor:?}, named by {by}{name}{unresolved}",
            named.len(),
            items.len()
        ));
    }
    spans.sort_unstable_by_key(|it| it.0);
    Ok(spans)
}

/// Every anchor the code of `built` names, in a position, a line or a static's bytes, with
/// what names it first, for an error.
fn named_anchors(built: &Built) -> HashMap<ContentId, NamedBy> {
    fn named_in(targets: &[Target]) -> impl Iterator<Item = ContentId> + '_ {
        targets.iter().filter_map(|it| match it {
            Target::Position(span) => Some(span.anchor),
            _ => None,
        })
    }
    let mut named: HashMap<ContentId, NamedBy> = HashMap::default();
    for function in &built.functions {
        let by = |what: &'static str| move |anchor: ContentId| (anchor, NamedBy::Function { key: function.key, what });
        let code = named_in(function.relocs.targets()).map(by("a position in"));
        let data = function.data.iter().flat_map(|it| named_in(it.relocs.targets())).map(by("the data of"));
        let lines = function.lines.iter().map(|it| it.span.anchor).map(by("a line row of"));
        for (anchor, by) in code.chain(data).chain(lines) {
            named.entry(anchor).or_insert(by);
        }
    }
    for static_ in &built.statics {
        let init: &[Target] = match &static_.init {
            Init::Bytes { relocs, .. } => relocs.targets(),
            Init::Zeroed { .. } => &[],
        };
        let data = static_.data.iter().flat_map(|it| named_in(it.relocs.targets()));
        for anchor in named_in(init).chain(data) {
            named.entry(anchor).or_insert(NamedBy::Static(static_.item));
        }
    }
    named
}

/// What names an anchor first, for an error.
#[derive(Clone, Copy, Debug)]
enum NamedBy {
    /// `what` says where in the function: its code, its data or its line rows.
    Function { key: InstanceKey, what: &'static str },
    Static(ItemKey),
    /// The caller of [`span_tables_of`], which named it.
    Caller,
}

impl std::fmt::Display for NamedBy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NamedBy::Function { key, what } => write!(f, "{what} the function {key:?}"),
            NamedBy::Static(item) => write!(f, "the static {item:?}"),
            NamedBy::Caller => f.write_str("the caller"),
        }
    }
}

/// A position: the file, its 1-based line and column, as `core::panic::Location` has them.
type Row = (base_db::FileId, u32, u32);

/// The positions of a body's spans, by table in [`hir_ty_share::code::table_of`]'s order:
/// table `t` is `rows[starts[t]..starts[t + 1]]`, by the span's index.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Positions {
    starts: [u32; TABLES + 1],
    rows: Box<[Row]>,
}

/// Each span's position, as the program observes it; a span with no syntax of its own (a
/// binding, [`MirSpan::Unknown`]) is where its owner is. Memoized, so a link after an edit
/// finds again only the bodies it changed.
#[blaze_query::tracked(returns(ref))]
fn positions(db: &Db, owner: DefWithBodyId) -> Result<Positions, String> {
    let store = BodyStore::of(db, owner);
    let at_owner = owner_location(db, owner);
    let row = |span: MirSpan| {
        crate::source_location(db, owner, span)
            .or(at_owner)
            // alloc: an error
            .ok_or_else(|| format!("{span:?} of {owner:?} has no position, nor has its owner"))
    };
    let mut starts = [0u32; TABLES + 1];
    // alloc: per body, its rows, memoized
    let mut rows = Vec::new();
    for table in 0..TABLES {
        starts[table] = rows.len() as u32;
        match table {
            code::EXPRS => for it in store.expr_ids() {
                rows.push(row(MirSpan::ExprId(it))?);
            },
            code::EXPR_ENDS => for it in store.expr_ids() {
                rows.push(row(MirSpan::ExprEnd(it))?);
            },
            code::PATS => for it in store.pat_ids() {
                rows.push(row(MirSpan::PatId(it))?);
            },
            code::BINDINGS => for (it, _) in store.bindings() {
                rows.push(row(MirSpan::BindingId(it))?);
            },
            code::SELF_PARAM => rows.push(row(MirSpan::SelfParam)?),
            code::UNKNOWN => rows.push(row(MirSpan::Unknown)?),
            _ => unreachable!("{TABLES} tables"),
        }
    }
    starts[TABLES] = rows.len() as u32;
    Ok(Positions { starts, rows: rows.into_boxed_slice() })
}

/// Where a body's owner is declared.
fn owner_location(db: &Db, owner: DefWithBodyId) -> Option<Row> {
    use hir_ty_mir::mir::location::{location_of, written_range};
    let source = match owner {
        DefWithBodyId::FunctionId(func) => return *crate::declaration_location(db, func),
        DefWithBodyId::StaticId(it) => crate::written_node(db, it.lookup(db)),
        DefWithBodyId::ConstId(it) => crate::written_node(db, it.lookup(db)),
        DefWithBodyId::VariantId(it) => crate::written_node(db, it.lookup(db)),
    };
    location_of(db, source.file_id, written_range(source.value))
}

/// `rows` as the link's span table: the files named in the order first named, then each
/// row as the file's index, the line and the column, little-endian.
fn encode(
    db: &Db,
    hashes: &HashSource,
    names: &mut HashMap<base_db::FileId, String>,
    rows: &[Row],
) -> Result<Vec<u8>, String> {
    // alloc: per table, the files it names, a few
    let mut files: Vec<base_db::FileId> = Vec::new();
    // alloc: per table, its rows' file indices
    let mut indices = Vec::with_capacity(rows.len());
    for &(file, _, _) in rows {
        let at = match files.iter().position(|it| *it == file) {
            Some(at) => at,
            None => {
                files.push(file);
                files.len() - 1
            }
        };
        indices.push(at as u32);
    }
    for &file in &files {
        if !names.contains_key(&file) {
            names.insert(file, hashes.file_name(db, file)?);
        }
    }
    let file_bytes: usize = files.iter().map(|it| 4 + names[it].len()).sum();
    // alloc: per table, its encoding for the link
    let mut out = Vec::with_capacity(8 + 4 + file_bytes + 4 + 12 * rows.len());
    out.put_bytes(&SPAN_TABLE_MAGIC);
    out.put_u32(files.len() as u32);
    for file in &files {
        out.put_bytes_u32(names[file].as_bytes());
    }
    out.put_u32(rows.len() as u32);
    for (&(_, line, column), file) in rows.iter().zip(indices) {
        for word in [file, line, column] {
            out.put_u32(word);
        }
    }
    Ok(out)
}

/// What a debugger and a backtrace call an instance: rustc's demangled name of it.
pub(super) fn debug_name(db: &Db, kind: &Kind) -> String {
    match kind {
        Kind::Code(instance, _) => hir_ty::attach_db(db, || crate::backtrace_name(db, instance)),
        // alloc: per instance described, its debug name
        Kind::Start(_) => crate::Target::current().entry().0.to_owned(),
        // alloc: per instance described, its debug name
        Kind::Allocator(forward) => forward.symbol.to_owned(),
        // alloc: per instance described, its debug name
        Kind::Asm(_) => "{global_asm}".to_owned(),
    }
}
