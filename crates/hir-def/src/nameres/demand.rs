//! Bodies on demand. A block with items is made as the body it is in is lowered, so what is
//! declared in bodies is found one body at a time, from the body that is asked for, and the body
//! a block is in is found from syntax and the def map, without lowering any other body
//! (#1293, #806).
//!
//! What a driver calls:
//! - every body of a crate checked fully (the workspace's): [`every_body`], then
//!   [`body_blocks`] of each for the items its blocks declare;
//! - a body of a dependency that codegen reached: [`body_blocks`] of that body;
//! - the bodies of a dependency that may declare an item a build keeps whether or not anything
//!   reaches it (`#[used]`, `#[no_mangle]`, `#[export_name]`, `#[link_section]`, the
//!   allocator, ...): [`bodies_declaring_roots`].

use std::ops::ControlFlow;

use base_db::{Crate, Db};
use blaze_hash::HashSet;
use hir_expand::{ErasedAstId, InFile, name::Name};
use intern::Symbol;
use syntax::{
    SmolStr, SyntaxKind, T, TextRange, ast as tag,
    view::{self as ast, AstNode},
};

use crate::{
    AdtId, AssocItemId, AstId, BlockId, DefWithBodyId, HasModule, ItemContainerId, Lookup,
    MacroExpander, MacroId, ModuleDefId, ModuleId,
    expr_store::Body,
    item_scope::ItemScope,
    nameres::{
        DefMap,
        assoc::{ImplItems, TraitItems},
        crate_def_map,
    },
    signatures::EnumVariants,
    src::HasSource,
};

/// The items of `scope` that have a body, in the order the scope lists them: its functions,
/// consts (named or not) and statics, the variants of its enums written with a discriminant,
/// and the functions and consts of its traits and impls. A function or static of an `extern`
/// block has none.
pub fn scope_body_owners(db: &Db, scope: &ItemScope) -> Vec<DefWithBodyId> {
    // alloc: the query's result, once per execution (per crate or per body)
    let mut owners = Vec::new();
    push_scope_body_owners(db, scope, &mut owners);
    owners
}

fn push_scope_body_owners(
    db: &Db,
    scope: &ItemScope,
    owners: &mut Vec<DefWithBodyId>,
) {
    let _ = visit_scope_body_owners(db, scope, |it| {
        owners.push(it);
        ControlFlow::Continue(())
    });
}

/// Calls `each` with the items of `scope` that have a body, in [`scope_body_owners`]' order,
/// until it breaks.
fn visit_scope_body_owners(
    db: &Db,
    scope: &ItemScope,
    mut each: impl FnMut(DefWithBodyId) -> ControlFlow<DefWithBodyId>,
) -> ControlFlow<DefWithBodyId> {
    for it in scope.unnamed_consts() {
        each(it.into())?;
    }
    let in_extern_block =
        |container: ItemContainerId| matches!(container, ItemContainerId::ExternBlockId(_));
    for def in scope.declarations() {
        match def {
            ModuleDefId::FunctionId(it) if !in_extern_block(it.lookup(db).container) => {
                each(it.into())?
            }
            ModuleDefId::ConstId(it) => each(it.into())?,
            ModuleDefId::StaticId(it) if !in_extern_block(it.lookup(db).container) => {
                each(it.into())?
            }
            ModuleDefId::AdtId(AdtId::EnumId(it)) => {
                let variants = EnumVariants::of(db, it).0.with_explicit_discriminant();
                for (variant, _, explicit) in variants {
                    if explicit {
                        each(DefWithBodyId::VariantId(variant))?;
                    }
                }
            }
            ModuleDefId::TraitId(it) => visit_assoc(&TraitItems::query(db, it).items, &mut each)?,
            _ => {}
        }
    }
    for it in scope.impls() {
        visit_assoc(&ImplItems::of(db, it).0.items, &mut each)?;
    }
    ControlFlow::Continue(())
}

fn visit_assoc(
    items: &[(Name, AssocItemId)],
    each: &mut impl FnMut(DefWithBodyId) -> ControlFlow<DefWithBodyId>,
) -> ControlFlow<DefWithBodyId> {
    for &(_, item) in items {
        match item {
            AssocItemId::FunctionId(it) => each(it.into())?,
            AssocItemId::ConstId(it) => each(it.into())?,
            AssocItemId::TypeAliasId(_) => {}
        }
    }
    ControlFlow::Continue(())
}

/// The items with a body declared in `module`, outside any body. Lowers no body.
pub fn module_body_owners(db: &Db, module: ModuleId) -> Vec<DefWithBodyId> {
    scope_body_owners(db, module.def_map(db).module(db, module).scope(db))
}

/// The items with a body declared in the modules of `krate`, outside any body. Lowers no body.
pub fn crate_body_owners(db: &Db, krate: Crate) -> Vec<DefWithBodyId> {
    let def_map = crate_def_map(db, krate);
    // Counted first, so the result is allocated once at any number of bodies.
    let mut count = 0;
    for (_, module) in def_map.modules(db) {
        let _ = visit_scope_body_owners(db, module.scope(db), |_| {
            count += 1;
            ControlFlow::Continue(())
        });
    }
    // alloc: the result, once per call, at its length
    let mut owners = Vec::with_capacity(count);
    for (_, module) in def_map.modules(db) {
        push_scope_body_owners(db, module.scope(db), &mut owners);
    }
    owners
}

/// The blocks with items of one body, and the items with a body they declare.
#[derive(Debug)]
pub struct BodyBlocks<'db> {
    /// Each block of the body that declares items, nested ones included, with its def map.
    pub blocks: Vec<(BlockId, &'db DefMap)>,
    /// The items with a body declared in those blocks, in any of their modules. What these
    /// declare is in their own bodies, which this does not lower.
    pub owners: Vec<DefWithBodyId>,
}

/// The blocks of the body of `owner` and what they declare, by lowering that body and no other.
pub fn body_blocks(db: &Db, owner: DefWithBodyId) -> BodyBlocks<'_> {
    // alloc: the query's result, once per execution (per crate or per body)
    let blocks: Vec<_> = Body::of(db, owner).blocks(db).collect();
    // alloc: the query's result, once per execution (per crate or per body)
    let mut owners = Vec::new();
    for &(_, def_map) in &blocks {
        for (_, module) in def_map.modules(db) {
            push_scope_body_owners(db, module.scope(db), &mut owners);
        }
    }
    BodyBlocks { blocks, owners }
}

/// Visits the bodies of `roots` that `lower` accepts and, in turn, those of the items their
/// blocks declare, with their blocks; lowers no body `lower` refuses. A body is visited once.
pub fn walk_bodies<'db>(
    db: &'db Db,
    roots: Vec<DefWithBodyId>,
    mut lower: impl FnMut(DefWithBodyId) -> bool,
    mut each: impl FnMut(DefWithBodyId, &BodyBlocks<'db>),
) {
    // alloc: once per walk, at the roots' count
    let mut seen: HashSet<DefWithBodyId> =
        HashSet::with_capacity_and_hasher(roots.len(), Default::default());
    seen.extend(roots.iter().copied());
    let mut queue = roots;
    queue.reverse();
    while let Some(owner) = queue.pop() {
        if !lower(owner) {
            continue;
        }
        let blocks = body_blocks(db, owner);
        each(owner, &blocks);
        queue.extend(blocks.owners.iter().rev().copied().filter(|&it| seen.insert(it)));
    }
}

/// Every body of `krate`, those declared in bodies included, each lowered: for a crate that is
/// checked fully. The order is the def map's, each body before those declared in it.
pub fn every_body(db: &Db, krate: Crate) -> Vec<DefWithBodyId> {
    let roots = crate_body_owners(db, krate);
    // alloc: the result, once per call, at the roots' count: bodies declared in bodies are few
    let mut bodies = Vec::with_capacity(roots.len());
    walk_bodies(db, roots, |_| true, |owner, _| bodies.push(owner));
    bodies
}

/// The bodies of `krate` lowered to find the items in them that a build keeps whether or not
/// anything reaches them ([`may_declare_roots`]), and those declared in them that may too: the
/// only bodies of a dependency a build lowers before codegen reaches them. Lowers no other body.
pub fn bodies_declaring_roots(db: &Db, krate: Crate) -> Vec<DefWithBodyId> {
    // alloc: the query's result, once per execution (per crate or per body)
    let mut bodies = Vec::new();
    walk_bodies_saying(db, krate, Look::Roots, |owner, _| bodies.push(owner));
    bodies
}

/// Visits the bodies of `krate` whose text may say what `look` looks for ([`Look::may_say`]),
/// each with its blocks, and in turn those declared in them that may too. Lowers no other body.
pub(crate) fn walk_bodies_saying<'db>(
    db: &'db Db,
    krate: Crate,
    look: Look,
    mut each: impl FnMut(DefWithBodyId, &BodyBlocks<'db>),
) {
    // The macros of the blocks lowered so far: a `macro_rules!` of a block is seen by the
    // bodies declared in it, which are visited after it. Seen by the others too, which only
    // lowers more.
    let local = std::cell::RefCell::new(HashSet::<SmolStr>::default());
    let roots = crate_body_owners(db, krate);
    walk_bodies(
        db,
        roots,
        |owner| look.may_say(db, owner, &local.borrow()),
        |owner, blocks| {
            each(owner, blocks);
            for &(_, def_map) in &blocks.blocks {
                for (_, module) in def_map.modules(db) {
                    let found = look.macros_of_scope(db, module.scope(db), &local.borrow());
                    local.borrow_mut().extend(found.iter().map(|it| SmolStr::new(it.as_str())));
                }
            }
        },
    );
}

/// The body that `block` is in. Found from the syntax around the block, up to the innermost
/// item with a body (in a macro's expansion, up to its call), and that item from the def map of
/// the module the block is in; the one body lowered is that one's, to check that the block is
/// in it. A block in an expression of a signature (an array length, a const argument, a
/// field's default) is in no body.
pub fn block_owner(db: &Db, block: BlockId) -> Result<DefWithBodyId, String> {
    let item = owning_item(db, block.ast_id(db))?;
    let mut module = block.module(db).to_static();
    let owner = loop {
        if let Some(owner) = owner_in_module(db, module, item) {
            break owner;
        }
        // The module of a block in a block of the same body is the outer block's.
        let outer = match module.block(db) {
            Some(outer) if module.is_block_module(db) => outer.to_static(),
            _ => {
                // alloc: error path
                return Err(format!(
                    "the item a block is in is not declared around it ({:?})",
                    block.ast_id(db)
                ));
            }
        };
        module = outer.module(db).to_static();
    };
    if !Body::of(db, owner).blocks(db).any(|(it, _)| it == block) {
        // alloc: error path
        return Err(format!("a block not in the body of the item it is in ({owner:?})"));
    }
    Ok(owner)
}

/// The item with a body that `owner` is, as its id.
fn owner_ast_id(db: &Db, owner: DefWithBodyId) -> ErasedAstId {
    match owner {
        DefWithBodyId::FunctionId(it) => it.lookup(db).id.erase(),
        DefWithBodyId::ConstId(it) => it.lookup(db).id.erase(),
        DefWithBodyId::StaticId(it) => it.lookup(db).id.erase(),
        DefWithBodyId::VariantId(it) => it.lookup(db).id.erase(),
    }
}

fn owner_in_module(
    db: &Db,
    module: ModuleId,
    item: ErasedAstId,
) -> Option<DefWithBodyId> {
    let scope = module.def_map(db).module(db, module).scope(db);
    visit_scope_body_owners(db, scope, |it| {
        if owner_ast_id(db, it) == item {
            ControlFlow::Break(it)
        } else {
            ControlFlow::Continue(())
        }
    })
    .break_value()
}

/// The innermost item with a body whose body holds `block`: a function, a const, a static or
/// an enum variant's discriminant. Inside a macro's expansion with no such item, the call is
/// where the expansion is, and the search goes on from there in the file of the call.
fn owning_item(
    db: &Db,
    block: AstId<tag::BlockExpr>,
) -> Result<ErasedAstId, String> {
    fn range<'t, N: AstNode<'t>>(it: N) -> TextRange {
        it.syntax().text_range()
    }
    let mut file = block.file_id;
    let mut node: ast::Node<'_> = block.to_view(db).syntax();
    loop {
        let at = node.text_range();
        let map = file.ast_id_map(db);
        for ancestor in node.ancestors() {
            if let Some(variant) = ast::Variant::cast(ancestor) {
                within(variant.const_arg().map(range), at, "an enum variant's fields")?;
                return Ok(InFile::new(file, map.ast_id_of_view::<tag::Variant>(variant).erase()));
            }
            let Some(item) = ast::Item::cast(ancestor) else { continue };
            let (body, item) = match item {
                ast::Item::Fn(it) => {
                    (it.body().map(range), map.ast_id_of_view::<tag::Fn>(it).erase())
                }
                ast::Item::Const(it) => {
                    (it.body().map(range), map.ast_id_of_view::<tag::Const>(it).erase())
                }
                ast::Item::Static(it) => {
                    (it.body().map(range), map.ast_id_of_view::<tag::Static>(it).erase())
                }
                // In a body these are expressions or statements, with the block around them:
                // a macro call's arguments are tokens.
                ast::Item::MacroCall(_) | ast::Item::AsmExpr(_) => continue,
                // alloc: error path
                _ => return Err(format!("a block in the signature of an item ({block:?})")),
            };
            within(body, at, "the signature of an item")?;
            return Ok(InFile::new(file, item));
        }
        let Some(call) = file.call_node_view(db) else {
            // alloc: error path
            return Err(format!("a block in no item with a body ({block:?})"));
        };
        file = call.file_id;
        node = call.value;
    }
}

/// Whether `at` is in `body`; else it is in `elsewhere`, which has no body.
fn within(body: Option<TextRange>, at: TextRange, elsewhere: &str) -> Result<(), String> {
    match body {
        Some(body) if body.contains_range(at) => Ok(()),
        // alloc: error path
        _ => Err(format!("a block in {elsewhere}, which is in no body")),
    }
}

/// The words in a body that may declare an item a build keeps whether or not anything reaches
/// it: the attributes that export or keep an item, link one by name or make one the allocator,
/// and the trait of an allocator. They are looked for as words, so a body that only says one is
/// lowered all the same.
const ROOT_WORDS: &[&str] = &[
    "no_mangle",
    "export_name",
    "used",
    "link_section",
    "linkage",
    "rustc_std_internal_symbol",
    "panic_handler",
    "lang",
    "global_allocator",
    "alloc_error_handler",
    "GlobalAlloc",
];

blaze_phf::phf_table!(static ROOT_WORD = ROOT_WORDS;);

/// What a scan of bodies' text looks for, to lower only the bodies that may declare such an item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Look {
    /// An item a build keeps whether or not anything reaches it: [`ROOT_WORDS`].
    Roots,
    /// An item with a diagnostic item, `#[rustc_diagnostic_item = ".."]`.
    DiagnosticItems,
}

impl Look {
    /// Whether `word`, an identifier without `r#`, is one this looks for.
    fn says(self, word: &str) -> bool {
        match self {
            Look::Roots => ROOT_WORD.get(word).is_some(),
            Look::DiagnosticItems => word == "rustc_diagnostic_item",
        }
    }

    /// The names of the macros of `krate` and its dependencies that may write such an item.
    fn macros(self, db: &Db, krate: Crate) -> &[Symbol] {
        match self {
            Look::Roots => macros_declaring_roots(db, krate),
            Look::DiagnosticItems => macros_writing_diagnostic_items(db, krate),
        }
    }

    /// Whether the body of `owner` may declare such an item: its text says a word this looks
    /// for, or names a macro of its crate, of a crate it depends on (`Look::macros`) or of the
    /// blocks around it (`local`) that may write one. Reads the body's syntax and lowers no body.
    fn may_say(
        self,
        db: &Db,
        owner: DefWithBodyId,
        local: &HashSet<SmolStr>,
    ) -> bool {
        let Some(body) = body_syntax(db, owner) else { return false };
        let declaring = self.macros(db, owner.module(db).krate(db));
        any_word(body, |word| {
            self.says(word) || local.contains(word) || {
                let key = intern::order_key(word.as_bytes());
                declaring
                    .binary_search_by(|it| {
                        it.order_key().cmp(&key).then_with(|| it.as_str().cmp(word))
                    })
                    .is_ok()
            }
        })
    }

    /// The macros of `scope`, by every name it has them under, that may write such an item,
    /// given that those named in `declaring` may.
    fn macros_of_scope(
        self,
        db: &Db,
        scope: &ItemScope,
        declaring: &HashSet<SmolStr>,
    ) -> Vec<Symbol> {
        // alloc: the query's result, once per execution (per crate or per body)
        let mut found = Vec::new();
        // alloc: the query's result, once per execution (per crate or per body)
        let mut calling = Vec::new();
        for (name, id) in scope_macros(scope) {
            self.classify(db, name, id, &mut found, &mut calling);
        }
        settle(found, calling, |word| declaring.contains(word.as_str()))
    }

    fn classify(
        self,
        db: &Db,
        name: Symbol,
        id: MacroId,
        found: &mut Vec<Symbol>,
        calling: &mut Vec<(Symbol, Vec<Symbol>)>,
    ) {
        match macro_text(db, id) {
            MacroText::Anything => found.push(name),
            MacroText::Nothing => {}
            MacroText::Words(words) if words.iter().any(|it| self.says(it.as_str())) => {
                found.push(name)
            }
            MacroText::Words(words) => calling.push((name, words)),
        }
    }

    /// [`macros_declaring_roots`] and [`macros_writing_diagnostic_items`]: the macros of
    /// `krate` that may write such an item, with those of its dependencies.
    fn crate_macros(self, db: &Db, krate: Crate) -> Box<[Symbol]> {
        let mut known: HashSet<Symbol> = HashSet::default();
        for dep in &krate.data(db).dependencies {
            known.extend(self.macros(db, dep.crate_id).iter().copied());
        }
        // alloc: the query's result, once per execution (per crate or per body)
        let mut found = Vec::new();
        // alloc: the query's result, once per execution (per crate or per body)
        let mut calling = Vec::new();
        let mut seen = HashSet::default();
        for (_, module) in crate_def_map(db, krate).modules(db) {
            for (name, id) in scope_macros(module.scope(db)) {
                if seen.insert((name, id)) {
                    self.classify(db, name, id, &mut found, &mut calling);
                }
            }
        }
        let settled = settle(found, calling, |word| known.contains(word));
        known.extend(settled);
        // alloc: the query's result, once per execution (per crate or per body)
        let mut names: Vec<Symbol> = known.into_iter().collect();
        names.sort_unstable_by(Symbol::cmp_order);
        names.into()
    }
}

/// Whether `f` accepts a word of `node`: an identifier, but those of a macro's metavariables
/// (`$name`), without `r#`. Read in place from the tokens: a body's words are not interned, so
/// scanning a dependency's bodies writes nothing to the symbol table.
fn any_word(node: ast::Node<'_>, mut f: impl FnMut(&str) -> bool) -> bool {
    let mut after_dollar = false;
    for token in node.descendants_with_tokens().filter_map(|it| it.into_token()) {
        let kind = token.kind();
        if kind.is_trivia() {
            continue;
        }
        let dollar = std::mem::replace(&mut after_dollar, kind == T![$]);
        if kind == SyntaxKind::IDENT && !dollar {
            let text = token.text();
            if f(text.strip_prefix("r#").unwrap_or(text)) {
                return true;
            }
        }
    }
    false
}

/// The words of `node` ([`any_word`]), interned: for a macro's definition, read once per macro.
fn words(node: ast::Node<'_>) -> Vec<Symbol> {
    // alloc: the query's result, once per execution (per crate or per body)
    let mut words = Vec::new();
    any_word(node, |word| {
        words.push(Symbol::intern(word));
        false
    });
    words
}

/// The expression of the body of `owner`, as it is written.
fn body_syntax(db: &Db, owner: DefWithBodyId) -> Option<ast::Node<'_>> {
    match owner {
        DefWithBodyId::FunctionId(it) => {
            it.lookup(db).source_view(db).value.body().map(|it| it.syntax())
        }
        DefWithBodyId::ConstId(it) => {
            it.lookup(db).source_view(db).value.body().map(|it| it.syntax())
        }
        DefWithBodyId::StaticId(it) => {
            it.lookup(db).source_view(db).value.body().map(|it| it.syntax())
        }
        DefWithBodyId::VariantId(it) => {
            it.lookup(db).source_view(db).value.const_arg().map(|it| it.syntax())
        }
    }
}

/// Whether the body of `owner` may declare an item a build keeps whether or not anything
/// reaches it: its text says one of [`ROOT_WORDS`], or names a macro of its crate or of a
/// crate it depends on that may write one ([`macros_declaring_roots`]). Reads the body's
/// syntax, and lowers no body. A body of which this is false declares no such item, though it
/// may declare others; those are reached through it.
pub fn may_declare_roots(db: &Db, owner: DefWithBodyId) -> bool {
    Look::Roots.may_say(db, owner, &HashSet::default())
}

/// What a macro is made of, for whether it may write an item a build keeps.
enum MacroText {
    /// It may write anything: a procedural macro, or `include!`.
    Anything,
    /// It writes only what its definition says: the words of it.
    Words(Vec<Symbol>),
    /// A built-in macro that writes no item.
    Nothing,
}

fn macro_text(db: &Db, id: MacroId) -> MacroText {
    let (expander, node) = match id {
        MacroId::ProcMacroId(_) => return MacroText::Anything,
        // The body only: the macro's own attributes are not in what it writes.
        MacroId::MacroRulesId(it) => {
            let loc = it.lookup(db);
            (loc.expander, loc.id.to_view(db).token_tree())
        }
        MacroId::Macro2Id(it) => {
            let loc = it.lookup(db);
            (loc.expander, loc.id.to_view(db).body())
        }
    };
    match expander {
        MacroExpander::Declarative { .. } => {
            let Some(node) = node else { return MacroText::Nothing };
            let mut words = words(node.syntax());
            words.sort_unstable_by(Symbol::cmp_order);
            words.dedup();
            MacroText::Words(words)
        }
        MacroExpander::BuiltInEager(it) if it.is_include() => MacroText::Anything,
        MacroExpander::BuiltIn(_)
        | MacroExpander::BuiltInAttr(_)
        | MacroExpander::BuiltInDerive(_)
        | MacroExpander::BuiltInEager(_) => MacroText::Nothing,
        // Its expansion is unknown, so it may be anything.
        MacroExpander::UnimplementedBuiltIn => MacroText::Anything,
    }
}

fn scope_macros(scope: &ItemScope) -> impl Iterator<Item = (Symbol, MacroId)> + '_ {
    let named = scope.macros().map(|(name, it)| (*name.symbol(), it.def));
    let legacy = scope
        .legacy_macros()
        .flat_map(|(name, ids)| ids.iter().map(move |&id| (*name.symbol(), id)));
    named.chain(legacy)
}

/// `found`, with each of `calling` that names one of them, or one `known` says may write a
/// kept item, until no more do.
fn settle(
    mut found: Vec<Symbol>,
    mut calling: Vec<(Symbol, Vec<Symbol>)>,
    known: impl Fn(&Symbol) -> bool,
) -> Vec<Symbol> {
    // alloc: the query's result, once per execution (per crate or per body)
    let mut writing: HashSet<Symbol> = found.iter().copied().collect();
    loop {
        let before = found.len();
        calling.retain(|(name, words)| {
            let writes = words.iter().any(|it| known(it) || writing.contains(it));
            if writes {
                found.push(*name);
                writing.insert(*name);
            }
            !writes
        });
        if found.len() == before {
            return found;
        }
    }
}

/// The names of the macros of `krate` and of the crates it depends on that may write an item a
/// build keeps whether or not anything reaches it: a declarative one whose text says one of
/// [`ROOT_WORDS`] or names one that may, a procedural one, and `include!`. By name, under each
/// name a module has one, so a macro of the same name costs a lowering and nothing else.
/// Sorted. Reads def maps and macro definitions, no body.
#[blaze_query::tracked(returns(deref))]
pub fn macros_declaring_roots(db: &Db, krate: Crate) -> Box<[Symbol]> {
    Look::Roots.crate_macros(db, krate)
}

/// As [`macros_declaring_roots`], for the macros that may write an item with a diagnostic item
/// (`#[rustc_diagnostic_item = ".."]`): what [`crate::diagnostic_item`] reads bodies for.
#[blaze_query::tracked(returns(deref))]
pub fn macros_writing_diagnostic_items(db: &Db, krate: Crate) -> Box<[Symbol]> {
    Look::DiagnosticItems.crate_macros(db, krate)
}

#[cfg(test)]
mod root_word_tests {
    use super::{Look, ROOT_WORDS};

    /// The table says what `ROOT_WORDS.contains` did: every word, near misses of each, and
    /// other identifiers.
    #[test]
    fn roots_as_a_linear_search() {
        let mut words: Vec<String> = Vec::new();
        for word in ROOT_WORDS {
            words.extend([
                (*word).to_owned(),
                format!("{word}_"),
                format!("r#{word}"),
                word.to_uppercase(),
                word.to_lowercase(),
                word[..word.len() - 1].to_owned(),
                word[1..].to_owned(),
            ]);
        }
        words.extend(["", "fn", "rustc_diagnostic_item", "allocator", "Global"].map(String::from));
        for word in &words {
            assert_eq!(Look::Roots.says(word), ROOT_WORDS.contains(&word.as_str()), "{word}");
        }
        assert!(Look::DiagnosticItems.says("rustc_diagnostic_item"));
        assert!(!Look::DiagnosticItems.says("lang"));
    }

    /// Reading the words of a body allocates nothing: the walk reads the tree's columns and
    /// each word is a slice of the source. A metavariable's name is no word, and `r#` is cut.
    #[test]
    fn reading_a_bodys_words_allocates_nothing() {
        let parse = syntax::SourceFile::parse(
            "fn f() { let r#x = 1; m!($y, z); { struct S; } }",
            syntax::Edition::CURRENT,
        );
        let root = parse.root();
        let expected = ["f", "x", "m", "z", "S"];
        let mut at = 0;
        let stopped = blaze_alloc_count::assert_at_most(0, "a body's words", || {
            super::any_word(root, |word| {
                assert_eq!(word, expected[at]);
                at += 1;
                false
            })
        });
        assert!(!stopped);
        assert_eq!(at, expected.len());
    }
}
