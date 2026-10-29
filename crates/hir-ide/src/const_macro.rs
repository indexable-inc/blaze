//! Const macros (docs/future/const-macros.md): a proc macro whose body is a function of a
//! macro crate of the analysis graph, expanded by calling that function in blaze's const
//! evaluator, in this process.
//!
//! The function's parameters are its inputs, the attribute's first for an attribute: each is
//! `&str`, the tokens as text (stage 0), or `&const_ast::Item`, the item as a flat syntax tree
//! read through `const-ast`'s views (stage 2), which the macro crate depends on as blaze's own
//! copy ([`CONST_AST`]). It returns its output as text, and every output token gets the call
//! site's span.

use std::{any::Any, sync::Arc};

use base_db::{Crate, Env, SourceDatabase, all_crates};
use hir_def::{AdtId, FunctionId, MacroId, ModuleDefId, StructId, nameres::crate_def_map};
use hir_expand::{
    name::Name,
    parse_to_token_tree_static_span,
    proc_macro::{ProcMacro, ProcMacroExpander, ProcMacroExpansionError, ProcMacroKind},
    tt,
};
use hir_ty::db::HirDatabase;
use intern::Symbol;
use rustc_hash::FxHashMap;
use rustc_type_ir::inherent::IntoKind;
use span::{FileId, Span};
use syntax::{
    AstNode, Edition, SyntaxKind,
    ast::{self, HasAttrs, HasModuleItem, HasName},
};

use crate::{
    display::{DisplayTarget, HirDisplay},
    mir::{FlatInput, MacroArg, call_const_macro},
    mir_pretty::MirEvalErrorPretty,
    next_solver::TyKind,
};

/// The name the macro crate depends on blaze's copy of `const-ast` by.
pub const CONST_AST_NAME: &str = "const_ast";

/// The files of blaze's copy of `const-ast`, by their path from its root, which every macro
/// crate depends on: the views a macro reads its input with are the code the expander writes
/// the input for, with the same layout and kinds.
pub const CONST_AST: [(&str, &str); 3] = const_ast::SOURCE;

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

/// Lets `db` run const macros: their expander is given the database as a `SourceDatabase`,
/// and the evaluator needs it as a `HirDatabase`.
pub fn enable(db: &dyn HirDatabase) {
    HirDatabase::zalsa_register_downcaster(db);
}

/// `db` as the `HirDatabase` it is.
///
/// # Panics
///
/// When [`enable`] was not called on `db`.
fn hir_db(db: &dyn SourceDatabase) -> &dyn HirDatabase {
    let caster = salsa::plumbing::views(db).downcaster_for::<dyn HirDatabase>();
    // SAFETY: a database's views are made for that database's own type.
    unsafe { caster.downcast_unchecked(db.into()) }
}

impl ConstMacroExpander {
    /// The macro crate and the function that is the macro.
    pub(crate) fn function(&self, db: &dyn SourceDatabase) -> Result<(Crate, FunctionId), String> {
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

    /// What each parameter of `func`, of the macro crate `krate`, takes.
    fn params(
        &self,
        db: &dyn HirDatabase,
        krate: Crate,
        func: FunctionId,
    ) -> Result<Vec<Param>, String> {
        let item = const_ast_item(db, krate)?;
        let sig = db.callable_item_signature(func.into()).skip_binder().skip_binder();
        let target = DisplayTarget::from_crate(db, krate);
        sig.inputs()
            .iter()
            .map(|ty| {
                if let TyKind::Ref(_, pointee, rustc_ast_ir::Mutability::Not) = ty.kind() {
                    match pointee.kind() {
                        TyKind::Str => return Ok(Param::Str),
                        TyKind::Adt(adt, _) if Some(adt.def_id()) == item.map(AdtId::StructId) => {
                            return Ok(Param::Item);
                        }
                        _ => {}
                    }
                }
                Err(format!(
                    "a parameter of the const macro `{}` is `{}`; it takes `&str` or \
                     `&{CONST_AST_NAME}::Item`",
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
    /// `&str`: the tokens as text.
    Str,
    /// `&const_ast::Item`: the tokens parsed to an item, as a flat tree.
    Item,
}

/// The `Item` view of the `const_ast` the macro crate `krate` depends on, if it does, which is
/// blaze's own copy.
fn const_ast_item(db: &dyn HirDatabase, krate: Crate) -> Result<Option<StructId>, String> {
    let Some(dep) =
        krate.data(db).dependencies.iter().find(|it| it.name.as_str() == CONST_AST_NAME)
    else {
        return Ok(None);
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
    match scope.get(&Name::new_symbol_root(Symbol::intern("Item"))).take_types() {
        Some(ModuleDefId::AdtId(AdtId::StructId(id))) => Ok(Some(id)),
        _ => Err(format!("`{CONST_AST_NAME}` has no struct `Item`")),
    }
}

/// `subtree` parsed to an item, as a flat tree: each token's span is its index in the call's
/// span table, which is `sites` (`def_site`, `call_site`, `mixed_site`) and then each distinct
/// span of the input in the order it first appears.
fn flat_input(
    subtree: &tt::TopSubtree,
    edition: Edition,
    sites: [Span; 3],
) -> Result<FlatInput, String> {
    let (parse, span_map) = syntax_bridge::token_tree_to_syntax_node(
        subtree,
        parser::TopEntryPoint::MacroItems,
        &mut |_| edition,
    );
    if let Some(error) = parse.errors().first() {
        return Err(format!("the input is not an item: {error}"));
    }
    let root = parse.syntax_node().to_flat();
    let (tree, _) = root.flat().ok_or("the input has no flat tree")?;
    let mut table: FxHashMap<Span, u32> = FxHashMap::default();
    for (index, site) in (0..).zip(sites) {
        table.entry(site).or_insert(index);
    }
    let mut next = sites.len() as u32;
    let mut input = FlatInput::default();
    for at in 0..tree.len() {
        let token = tree.is_token(at);
        input.kinds.push(tree.kind(at).0 | if token { const_ast::TOKEN } else { 0 });
        input.ends.push(tree.end(at));
        if token {
            let span = span_map.span_at(tree.range(at).start());
            let index = *table.entry(span).or_insert_with(|| {
                next += 1;
                next - 1
            });
            input.spans.push(index);
            input.text.push_str(tree.token_text(at));
        } else {
            input.spans.push(const_ast::NO_SPAN);
        }
        input.text_ends.push(input.text.len() as u32);
    }
    let mut items = root.children().filter(|it| ast::Item::can_cast(it.kind()));
    let item = items.next().ok_or("the input has no item")?;
    input.item = item.flat().ok_or("the input has no flat tree")?.1;
    if items.next().is_some() {
        return Err("the input is more than one item".to_owned());
    }
    Ok(input)
}

impl ProcMacroExpander for ConstMacroExpander {
    fn expand(
        &self,
        db: &dyn SourceDatabase,
        subtree: &tt::TopSubtree,
        attrs: Option<&tt::TopSubtree>,
        _env: &Env,
        def_site: Span,
        call_site: Span,
        mixed_site: Span,
        _current_dir: String,
    ) -> Result<tt::TopSubtree, ProcMacroExpansionError> {
        let (krate, func) = self.function(db).map_err(ProcMacroExpansionError::System)?;
        let hir = hir_db(db);
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
        let mut texts = Vec::new();
        let mut flats = Vec::new();
        for (param, input) in params.iter().zip(&inputs) {
            match param {
                Param::Str => texts.push(input.to_string()),
                Param::Item => flats.push(
                    flat_input(input, edition, [def_site, call_site, mixed_site]).map_err(
                        |why| {
                            ProcMacroExpansionError::Panic(format!(
                                "the const macro `{}` takes `&{CONST_AST_NAME}::Item`: {why}",
                                self.name
                            ))
                        },
                    )?,
                ),
            }
        }
        let (mut texts, mut flats) = (texts.iter(), flats.iter());
        let args: Vec<MacroArg<'_>> = params
            .iter()
            .filter_map(|param| match param {
                Param::Str => texts.next().map(|it| MacroArg::Str(it)),
                Param::Item => flats.next().map(MacroArg::Item),
            })
            .collect();
        let output = call_const_macro(hir, func, &args).map_err(|error| {
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
        parse_to_token_tree_static_span(edition, call_site, &output).ok_or_else(|| {
            ProcMacroExpansionError::Panic(format!(
                "the const macro `{}` returned text that is not tokens: {output:?}",
                self.name
            ))
        })
    }

    fn eq_dyn(&self, other: &dyn ProcMacroExpander) -> bool {
        (other as &dyn Any).downcast_ref::<Self>() == Some(self)
    }
}
