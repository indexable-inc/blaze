//! Const macros (docs/future/const-macros.md): a proc macro whose body is a function of a
//! macro crate of the analysis graph, expanded by calling that function in blaze's const
//! evaluator, in this process.
//!
//! The function's parameters are its inputs, the attribute's first for an attribute: each is
//! `&const_ast::Item`, the input parsed to an item, or `&const_ast::TokenTree`, its tokens as
//! one token tree, both flat syntax trees read through `const-ast`'s views, which the macro
//! crate depends on as blaze's own copy ([`CONST_AST`]). It returns a `const_ast::Output`, the
//! tree a `const_ast::Builder` wrote, never text: the host reads it back ([`output`]), gives
//! each token the span the call's table holds for it, and refuses a tree that is not the
//! parser's tree of its own tokens, naming the macro and the element.
//!
//! A call has one span table, shared by its inputs: `def_site` 0, `call_site` 1, `mixed_site`
//! 2, then each distinct span of the inputs in the order it first appears ([`SpanTable`]). A
//! macro's span is an index into it, which it cannot make, join or look into.

use std::{any::Any, sync::Arc};

use base_db::{Crate, Db, all_crates};
use blaze_hash::HashMap;
use hir_def::{AdtId, FunctionId, MacroId, ModuleDefId, StructId, nameres::crate_def_map};
use hir_expand::{
    name::Name,
    proc_macro::{
        Expanded, ProcMacro, ProcMacroCall, ProcMacroExpander, ProcMacroExpansionError,
        ProcMacroKind,
    },
    tt,
};
use hir_ty::db::HirDatabase;
use intern::Symbol;
use rustc_type_ir::inherent::IntoKind;
use span::{FileId, Span};
use std::alloc::Global;
use syntax::{
    AstNode, Edition, SyntaxKind,
    ast::{self, HasAttrs, HasModuleItem, HasName},
};

use crate::{
    display::{DisplayTarget, HirDisplay},
    mir::{FlatInput, call_const_macro},
    mir_pretty::MirEvalErrorPretty,
    next_solver::TyKind,
};

mod output;
pub mod quote;

/// The name the macro crate depends on blaze's copy of `const-ast` by.
pub const CONST_AST_NAME: &str = "const_ast";

/// The files of blaze's copy of `const-ast`, by their path from its root, which every macro
/// crate depends on: the views a macro reads its input with and the builder it writes its
/// output with are the code the expander writes the input for and reads the output of, with
/// the same layout and kinds.
pub const CONST_AST: [(&str, &str); 4] = const_ast::SOURCE;

/// The expander of the const macro `name`, a function of the macro crate whose root is `root`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConstMacroExpander {
    pub root: FileId,
    pub name: Symbol,
    pub kind: ProcMacroKind,
}

/// The macros of the macro crate whose root file is `root`, of text `text`: its root's functions
/// marked `#[proc_macro_derive(Name)]`, `#[proc_macro_attribute]` or `#[proc_macro]`, as the
/// def map collector exports them from a proc-macro crate.
pub fn load(root: FileId, text: &str, edition: Edition) -> Vec<ProcMacro> {
    let file = ast::SourceFile::parse(text, edition).tree();
    let macros = file.items().filter_map(|item| {
        let ast::Item::Fn(func) = item else { return None };
        let fn_name = || Some(func.name()?.text().to_string());
        func.attrs().find_map(|attr| match attr.simple_name()?.as_str() {
            "proc_macro_derive" => {
                let (_, args) = attr.as_simple_call()?;
                let name = args
                    .syntax()
                    .children_with_tokens()
                    .filter_map(|it| it.into_token())
                    .find(|it| it.kind() == SyntaxKind::IDENT)?;
                Some((name.text().to_owned(), ProcMacroKind::CustomDerive))
            }
            "proc_macro_attribute" => Some((fn_name()?, ProcMacroKind::Attr)),
            "proc_macro" => Some((fn_name()?, ProcMacroKind::Bang)),
            _ => None,
        })
    });
    macros
        .map(|(name, kind)| {
            let name = Symbol::intern(&name);
            let expander = ConstMacroExpander { root, name: name.clone(), kind };
            ProcMacro { name, kind, expander: Arc::new(expander), disabled: false }
        })
        .collect()
}
impl ConstMacroExpander {
    /// The macro crate and the function that is the macro.
    pub(crate) fn function(&self, db: &Db) -> Result<(Crate, FunctionId), String> {
        let krate = all_crates(db)
            .iter()
            .copied()
            .find(|krate| {
                let data = krate.data(db);
                data.root_file_id == self.root && data.target_kind.is_proc_macro()
            })
            .ok_or_else(|| format!("no macro crate has the root of `{}`", self.name))?;
        let def_map = crate_def_map(db, krate);
        let root = def_map.module(db, def_map.root_module_id()).scope(db);
        let Some(MacroId::ProcMacroId(id)) =
            root.get(&Name::new_symbol_root(self.name.clone())).take_macros()
        else {
            return Err(format!("the macro crate defines no proc macro `{}`", self.name));
        };
        root.declarations()
            .find_map(|it| match it {
                ModuleDefId::FunctionId(func) if def_map.fn_as_proc_macro(func) == Some(id) => {
                    Some((krate, func))
                }
                _ => None,
            })
            .ok_or_else(|| format!("the proc macro `{}` is not a function", self.name))
    }

    /// What each parameter of `func`, of the macro crate `krate`, takes, its return type
    /// checked to be `const_ast::Output`.
    fn params(&self, db: &Db, krate: Crate, func: FunctionId) -> Result<Vec<Param>, String> {
        let views = const_ast(db, krate)?;
        let sig = db.callable_item_signature(func.into()).skip_binder().skip_binder();
        let target = DisplayTarget::from_crate(db, krate);
        let output = sig.output();
        if !matches!(output.kind(), TyKind::Adt(adt, _) if adt.def_id() == AdtId::StructId(views.built))
        {
            return Err(format!(
                "the const macro `{}` returns `{}`; it returns `{CONST_AST_NAME}::Output`",
                self.name,
                output.display(db, target)
            ));
        }
        sig.inputs()
            .iter()
            .map(|ty| {
                if let TyKind::Ref(_, pointee, rustc_ast_ir::Mutability::Not) = ty.kind()
                    && let TyKind::Adt(adt, _) = pointee.kind()
                {
                    if adt.def_id() == AdtId::StructId(views.item) {
                        return Ok(Param::Item);
                    }
                    if adt.def_id() == AdtId::StructId(views.token_tree) {
                        return Ok(Param::Tokens);
                    }
                }
                Err(format!(
                    "a parameter of the const macro `{}` is `{}`; it takes \
                     `&{CONST_AST_NAME}::Item` or `&{CONST_AST_NAME}::TokenTree`",
                    self.name,
                    ty.display(db, target)
                ))
            })
            .collect()
    }
}

/// What a parameter of a const macro takes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Param {
    /// `&const_ast::Item`: the tokens parsed to an item, as a flat tree.
    Item,
    /// `&const_ast::TokenTree`: the tokens as one token tree, `{ .. }`, as a flat tree.
    Tokens,
}

/// The types of the `const_ast` a macro crate depends on that a const macro's signature names.
struct ConstAst {
    /// `Item`, a parameter.
    item: StructId,
    /// `TokenTree`, a parameter.
    token_tree: StructId,
    /// `Built`, which the return type `Output` is (`Built<'static>`).
    built: StructId,
}

/// The views of the `const_ast` the macro crate `krate` depends on, which must be blaze's own
/// copy: a macro crate without it has no const macro.
fn const_ast(db: &Db, krate: Crate) -> Result<ConstAst, String> {
    let Some(dep) =
        krate.data(db).dependencies.iter().find(|it| it.name.as_str() == CONST_AST_NAME)
    else {
        return Err(format!(
            "the macro crate does not depend on `{CONST_AST_NAME}`: a const macro takes and \
             returns its trees"
        ));
    };
    let root = dep.crate_id.data(db).root_file_id;
    if **db.file_text(root).text(db) != *CONST_AST[0].1 {
        return Err(format!(
            "the macro crate's `{CONST_AST_NAME}` is not blaze's own: a const macro reads \
             its input with the views of the copy blaze writes it for"
        ));
    }
    let def_map = crate_def_map(db, dep.crate_id);
    let scope = def_map.module(db, def_map.root_module_id()).scope(db);
    let named = |name: &str| match scope.get(&Name::new_symbol_root(Symbol::intern(name))).take_types()
    {
        Some(ModuleDefId::AdtId(AdtId::StructId(id))) => Ok(id),
        _ => Err(format!("`{CONST_AST_NAME}` has no struct `{name}`")),
    };
    Ok(ConstAst { item: named("Item")?, token_tree: named("TokenTree")?, built: named("Built")? })
}

/// One call's span table: the three sites, then every other span of its inputs once, in the
/// order it first appears. A token's span in a `const_ast::Tree` is its index here, and a
/// written token's span is read back through it.
struct SpanTable {
    index: HashMap<Span, u32, Global>,
    spans: Vec<Span>,
}

impl SpanTable {
    /// The table of `sites` (`def_site`, `call_site`, `mixed_site`): each at its own index,
    /// 0, 1 and 2, even when two are the same span.
    fn new(sites: [Span; 3]) -> SpanTable {
        let mut index = HashMap::new_in(Global); // alloc: one call's table, dropped with it
        for (at, site) in (0..).zip(sites) {
            index.entry(site).or_insert(at);
        }
        SpanTable { index, spans: sites.to_vec() } // alloc: as `index`
    }

    /// `span`'s index, given the next one if it has none yet.
    fn intern(&mut self, span: Span) -> u32 {
        let next = self.spans.len() as u32;
        let at = *self.index.entry(span).or_insert(next);
        if at == next {
            self.spans.push(span);
        }
        at
    }
}

/// `subtree` parsed as items, as a flat tree whose token spans are `table`'s indices; the
/// element the view starts at is left 0 for the caller to set.
fn flat_tree(
    subtree: &tt::TopSubtree,
    edition: Edition,
    table: &mut SpanTable,
) -> Result<(FlatInput, syntax::Parse<syntax::view::Untyped>), String> {
    let (parse, span_map) = syntax_bridge::token_tree_to_syntax_node(
        subtree.view(),
        parser::TopEntryPoint::MacroItems,
        &mut |_| edition,
    );
    if let Some(error) = parse.errors().first() {
        return Err(format!("it is not items: {error}"));
    }
    let (tree, root) = parse.root().flat();
    let mut input = FlatInput::default();
    for at in root..tree.end(root) {
        let token = tree.is_token(at);
        input.kinds.push(tree.kind(at).0 | if token { const_ast::TOKEN } else { 0 });
        input.ends.push(tree.end(at) - root);
        if token {
            input.spans.push(table.intern(span_map.span_at(tree.range(at).start())));
            input.text.push_str(tree.token_text(at));
        } else {
            input.spans.push(const_ast::NO_SPAN);
        }
        input.text_ends.push(input.text.len() as u32);
    }
    Ok((input, parse))
}

/// `subtree` parsed to an item, as a flat tree: each token's span is its index in `table`.
fn flat_input(
    subtree: &tt::TopSubtree,
    edition: Edition,
    table: &mut SpanTable,
) -> Result<FlatInput, String> {
    let (mut input, parse) = flat_tree(subtree, edition, table)?;
    let (tree, root) = parse.root().flat();
    let mut items = (root + 1..tree.end(root))
        .filter(|&at| !tree.is_token(at) && tree.parent(at) == Some(root))
        .filter(|&at| ast::Item::can_cast(SyntaxKind::from(tree.kind(at).0)));
    let item = items.next().ok_or("the input has no item")?;
    if items.next().is_some() {
        return Err("the input is more than one item".to_owned());
    }
    input.item = item - root;
    Ok(input)
}

/// `subtree`'s tokens as one token tree, a flat tree: wrapped as `m!{ .. }` (the three tokens
/// added at `call_site`) and parsed as items, so the token tree and the token trees in it are
/// the parser's own nodes; the view starts at the call's `TOKEN_TREE`. Each token's span is
/// its index in `table`.
fn flat_tokens(
    subtree: &tt::TopSubtree,
    edition: Edition,
    table: &mut SpanTable,
    call_site: Span,
) -> Result<FlatInput, String> {
    let mut builder = tt::TopSubtreeBuilder::new(tt::Delimiter::invisible_spanned(call_site));
    builder.push(tt::Leaf::Ident(tt::Ident {
        sym: Symbol::intern("m"),
        span: call_site,
        is_raw: tt::IdentIsRaw::No,
    }));
    builder.push(tt::Leaf::Punct(tt::Punct {
        char: '!',
        spacing: tt::Spacing::Alone,
        span: call_site,
    }));
    builder.open(tt::DelimiterKind::Brace, call_site);
    builder.extend_with_tt(subtree.view().token_trees());
    builder.close(call_site);
    let wrapped = builder.build();
    let (mut input, _) = flat_tree(&wrapped, edition, table)?;
    let tree = input
        .kinds
        .iter()
        .position(|&kind| kind == SyntaxKind::TOKEN_TREE as u16)
        .ok_or("its tokens wrapped in a macro call have no token tree")?;
    input.item = tree as u32;
    Ok(input)
}

/// What a const macro's call states of its reads ([`ExpansionReads::Unknown`]).
///
/// [`ExpansionReads::Unknown`]: hir_expand::proc_macro::ExpansionReads::Unknown
const CONST_MACRO_READS: &str =
    "a const macro, run on the database's evaluator: no server traced what it read";

impl ProcMacroExpander for ConstMacroExpander {
    fn expand(
        &self,
        db: &Db,
        ProcMacroCall { subtree, attrs, def_site, call_site, mixed_site, .. }: ProcMacroCall<'_>,
    ) -> Result<Expanded, ProcMacroExpansionError> {
        let (krate, func) = self.function(db).map_err(ProcMacroExpansionError::System)?;
        let hir = db;
        let edition = krate.data(db).edition;
        let params = self.params(hir, krate, func).map_err(ProcMacroExpansionError::Panic)?;
        let inputs: Vec<&tt::TopSubtree> = attrs.into_iter().chain([subtree]).collect();
        if params.len() != inputs.len() {
            let given = match self.kind {
                ProcMacroKind::CustomDerive => "a derive is given 1, the item",
                ProcMacroKind::Attr => "an attribute macro is given 2, its tokens and the item",
                ProcMacroKind::Bang => "a function-like macro is given 1, its tokens",
            };
            return Err(ProcMacroExpansionError::Panic(format!(
                "the const macro `{}` has {} parameters; {given}",
                self.name,
                params.len(),
            )));
        }
        let mut table = SpanTable::new([def_site, call_site, mixed_site]);
        let mut flats = Vec::with_capacity(inputs.len()); // alloc: one call's inputs
        for (at, (param, input)) in params.iter().zip(&inputs).enumerate() {
            let (flat, takes) = match param {
                Param::Item => (flat_input(input, edition, &mut table), "Item"),
                Param::Tokens => {
                    (flat_tokens(input, edition, &mut table, call_site), "TokenTree")
                }
            };
            flats.push(flat.map_err(|why| {
                ProcMacroExpansionError::Panic(format!(
                    "the const macro `{}` takes `&{CONST_AST_NAME}::{takes}` as parameter {at}, \
                     and its input is refused: {why}",
                    self.name
                ))
            })?);
        }
        let inputs: Vec<&FlatInput> = flats.iter().collect(); // alloc: as `flats`
        let output = call_const_macro(hir, func, &inputs).map_err(|error| {
            let mut text = format!("the const macro `{}` failed: ", self.name);
            let display_target = DisplayTarget::from_crate(hir, krate);
            let printed = error.pretty_print(
                &mut text,
                hir,
                |file, range| format!("{file:?} {range:?}"),
                display_target,
            );
            match printed {
                Ok(()) => ProcMacroExpansionError::Panic(text),
                Err(_) => ProcMacroExpansionError::Panic(format!("{text}{error:?}")),
            }
        })?;
        let tree =
            output::expansion(&self.name, &output, &inputs, &table.spans, edition, call_site)
                .map_err(ProcMacroExpansionError::Panic)?;
        // It runs on the database's evaluator, whose reads are the database's own, tracked;
        // none is traced as a server traces what it read, so its calling crate gets no image.
        Ok(Expanded::untraced(tree, CONST_MACRO_READS))
    }

    // dyn: hir-expand's `ProcMacroExpander::eq_dyn`; compared when a crate's macros are set
    fn eq_dyn(&self, other: &dyn ProcMacroExpander) -> bool {
        (other as &dyn Any).downcast_ref::<Self>() == Some(self) // dyn: the downcast `eq_dyn` needs
    }
}
