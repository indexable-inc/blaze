//! Eager expansion related utils
//!
//! Here is a dump of a discussion from Vadim Petrochenkov about Eager Expansion and
//! Its name resolution :
//!
//! > Eagerly expanded macros (and also macros eagerly expanded by eagerly expanded macros,
//! > which actually happens in practice too!) are resolved at the location of the "root" macro
//! > that performs the eager expansion on its arguments.
//! > If some name cannot be resolved at the eager expansion time it's considered unresolved,
//! > even if becomes available later (e.g. from a glob import or other macro).
//!
//! > Eagerly expanded macros don't add anything to the module structure of the crate and
//! > don't build any speculative module structures, i.e. they are expanded in a "flat"
//! > way even if tokens in them look like modules.
//!
//! > In other words, it kinda works for simple cases for which it was originally intended,
//! > and we need to live with it because it's available on stable and widely relied upon.
//!
//!
//! See the full discussion : <https://rust-lang.zulipchat.com/#narrow/stream/131828-t-compiler/topic/Eager.20expansion.20of.20built-in.20macros>
use std::alloc::Allocator;

use base_db::{Db, Crate};
use span::SyntaxContext;
use syntax::{
    AstPtr, FlatTree, FlatTreeBuilder, NodeOrToken, TextSize, WalkEvent,
    view::{self, AstNode as _},
};
use syntax_bridge::DocCommentDesugarMode;
use triomphe::Arc;

use crate::{
    AstId, EagerCallInfo, ExpandError, ExpandErrorKind, ExpandResult, ExpandTo, ExpansionSpanMap,
    InFile, MacroCallId, MacroCallKind, MacroCallLoc, MacroDefId, MacroDefKind, ast,
    mod_path::ModPath,
    span_map::{InOrder, SpanMap},
};

/// A macro call inside an eager macro's input, expanded with it: where it is, and its ID. An
/// eager expansion appends one per call it expands, in the order it meets them.
pub type EagerCall = (InFile<(AstPtr<ast::MacroCall>, span::FileAstId<ast::MacroCall>)>, MacroCallId);

/// The resolver of the macro calls inside an eager macro's input. Not a generic: that would copy
/// the recursive eager expander (about 300 lines) into each of its nine callers.
// dyn: one call per nested macro call, given by hir-def, which hir-expand can't name
pub type EagerResolver<'a> = &'a dyn Fn(&ModPath) -> Option<MacroDefId>;

thread_local! {
    /// The span maps of eager inputs being expanded, one per nesting level, kept emptied.
    static ARG_MAPS: std::cell::RefCell<Vec<ExpansionSpanMap>> =
        // alloc: none until a map is given back; then one slot per nesting level.
        const { std::cell::RefCell::new(Vec::new()) };
}

/// A span map of the worker's, empty, given back when dropped.
pub(crate) struct ArgMap(Option<ExpansionSpanMap>);

impl ArgMap {
    pub(crate) fn take() -> ArgMap {
        let map = ARG_MAPS.with_borrow_mut(Vec::pop).unwrap_or_else(ExpansionSpanMap::empty);
        ArgMap(Some(map))
    }
}

impl std::ops::Deref for ArgMap {
    type Target = ExpansionSpanMap;
    fn deref(&self) -> &ExpansionSpanMap {
        self.0.as_ref().expect("held until dropped")
    }
}

impl std::ops::DerefMut for ArgMap {
    fn deref_mut(&mut self) -> &mut ExpansionSpanMap {
        self.0.as_mut().expect("held until dropped")
    }
}

impl Drop for ArgMap {
    fn drop(&mut self) {
        if let Some(mut map) = self.0.take() {
            map.clear();
            // A thread that is ending: the map is freed.
            let _ = ARG_MAPS.try_with(|maps| maps.borrow_mut().push(map));
        }
    }
}

/// Expands the macro calls inside the input of an eager macro's call (in preorder, the eager ones
/// eagerly and the rest lazily) and makes the call whose argument is the expanded input. Each
/// nested call it expands is appended to `eager_calls`.
pub fn expand_eager_macro_input_view(
    db: &Db,
    krate: Crate,
    macro_call: view::MacroCall<'_>,
    ast_id: AstId<ast::MacroCall>,
    def: MacroDefId,
    call_site: SyntaxContext,
    macro_depth: u32,
    recursion_limit: u32,
    resolver: EagerResolver<'_>,
    eager_calls: &mut Vec<EagerCall, impl Allocator>,
) -> ExpandResult<Option<MacroCallId>> {
    let expand_to = ExpandTo::of_call_site_view(macro_call);

    // The argument's own call: its file must exist before a nested call is expanded lazily.
    let loc = MacroCallLoc {
        def,
        krate,
        kind: MacroCallKind::FnLike { ast_id, expand_to: ExpandTo::Expr, eager: None },
        ctxt: call_site,
        macro_depth,
    };
    let arg_id = MacroCallId::new(db, loc);
    #[expect(deprecated, reason = "builtin eager macros are never derives")]
    let (_, _, span, _) = arg_id.macro_arg(db);
    let expand_to = match expand_to {
        Ok(it) => it,
        Err(parent) => {
            // alloc: an error's message, on the error path
            let message = format!("blaze can't tell what a macro call in a `{parent:?}` expands to");
            return ExpandResult::new(None, ExpandError::other(*span, message));
        }
    };
    let ExpandResult { value: (arg_exp, arg_exp_map), err: parse_err } =
        arg_id.parse_macro_expansion(db);

    let mut arg_map = ArgMap::take();

    let ExpandResult { value: expanded_eager_input, err } = eager_macro_recur_view(
        db,
        arg_exp_map,
        &mut arg_map,
        TextSize::new(0),
        InFile::new(arg_id.into(), arg_exp.root()),
        krate,
        call_site,
        macro_depth,
        recursion_limit,
        resolver,
        eager_calls,
    );
    let err = parse_err.clone().or(err); // clone: an `Option<ExpandError>`: an `Arc` count, only on error
    if cfg!(debug_assertions) {
        arg_map.finish();
    }

    let Some((expanded_eager_input, _mapping)) = expanded_eager_input else {
        return ExpandResult { value: None, err };
    };

    let mut subtree = syntax_bridge::call_input(
        expanded_eager_input.root(),
        InOrder::new(SpanMap::ExpansionSpanMap(&*arg_map)),
        *span,
        DocCommentDesugarMode::Mbe,
    );

    subtree.set_top_subtree_delimiter_kind(crate::tt::DelimiterKind::Invisible);

    let loc = MacroCallLoc {
        def,
        krate,
        kind: MacroCallKind::FnLike {
            ast_id,
            expand_to,
            eager: Some(Arc::new(EagerCallInfo { // alloc: the eager call's interned key, once per eager call
                arg: subtree,
                arg_id,
                error: err.clone(), // clone: an `Option<ExpandError>`: an `Arc` count, only on error
                span: *span,
            })),
        },
        ctxt: call_site,
        macro_depth,
    };

    ExpandResult { value: Some(MacroCallId::new(db, loc)), err }
}

/// An eager macro's input after its nested calls are expanded: as it was when none was
/// replaced, else a copy with them replaced.
enum EagerInput<'db> {
    Original(view::Node<'db>),
    Built(std::sync::Arc<FlatTree>),
}

impl EagerInput<'_> {
    fn root(&self) -> view::Node<'_> {
        match self {
            EagerInput::Original(node) => *node,
            EagerInput::Built(tree) => view::Node::root(tree),
        }
    }
}

/// Makes the lazy call of a macro call inside an eager input, appends it to `eager_calls`, and
/// gives its expansion's root and span map.
fn lazy_expand_view<'db>(
    db: &'db Db,
    def: &MacroDefId,
    macro_call: view::MacroCall<'_>,
    ast_id: AstId<ast::MacroCall>,
    expand_to: ExpandTo,
    krate: Crate,
    call_site: SyntaxContext,
    macro_depth: u32,
    eager_calls: &mut Vec<EagerCall, impl Allocator>,
) -> ExpandResult<(InFile<view::Node<'db>>, &'db ExpansionSpanMap)> {
    let id = def.make_call(
        db,
        krate,
        MacroCallKind::FnLike { ast_id, expand_to, eager: None },
        call_site,
        macro_depth,
    );
    // alloc: the caller's buffer, once per nested macro call
    eager_calls.push((ast_id.map(|ast_id| (AstPtr::of(macro_call), ast_id)), id));

    id.parse_macro_expansion(db)
        .as_ref()
        .map(|parse| (InFile::new(id.into(), parse.0.root()), &parse.1))
}

/// Expands the macro calls in `curr` from `offset` on, pushing the spans of the expanded input to
/// `expanded_map`: the input as it was when no call is replaced, else the copy built with them
/// replaced, and the offset past it.
fn eager_macro_recur_view<'db>(
    db: &'db Db,
    span_map: &ExpansionSpanMap,
    expanded_map: &mut ExpansionSpanMap,
    mut offset: TextSize,
    curr: InFile<view::Node<'db>>,
    krate: Crate,
    call_site: SyntaxContext,
    macro_depth: u32,
    recursion_limit: u32,
    macro_resolver: EagerResolver<'_>,
    eager_calls: &mut Vec<EagerCall, impl Allocator>,
) -> ExpandResult<Option<(EagerInput<'db>, TextSize)>> {
    let original = curr.value;

    let mut replacements = Vec::new(); // alloc: none unless a nested call is replaced

    // FIXME: We only report a single error inside of eager expansions
    let mut error = None;
    let mut children = original.preorder_with_tokens();
    let mut span_hint = 0;

    while let Some(child) = children.next() {
        let call = match child {
            WalkEvent::Enter(NodeOrToken::Node(child)) => match view::MacroCall::cast(child) {
                Some(it) => {
                    children.skip_subtree();
                    it
                }
                _ => continue,
            },
            WalkEvent::Enter(_) => continue,
            WalkEvent::Leave(child) => {
                if let NodeOrToken::Token(t) = child {
                    let start = t.text_range().start();
                    offset += t.text_range().len();
                    expanded_map.push(offset, span_map.span_at_from(start, &mut span_hint));
                }
                continue;
            }
        };

        let def = match call.path().and_then(|path| {
            ModPath::from_src_view(db, path, &mut |range| span_map.span_at(range.start()).ctx)
        }) {
            Some(path) => match macro_resolver(&path) {
                Some(def) => def,
                None => {
                    let edition = krate.data(db).edition;
                    error = Some(ExpandError::other(
                        span_map.span_at(call.syntax().text_range().start()),
                        format!("unresolved macro {}", path.display(db, edition)), // alloc: an error's message, on the error path
                    ));
                    offset += call.syntax().text_range().len();
                    continue;
                }
            },
            None => {
                error = Some(ExpandError::other(
                    span_map.span_at(call.syntax().text_range().start()),
                    "malformed macro invocation",
                ));
                offset += call.syntax().text_range().len();
                continue;
            }
        };
        let ast_id = curr.file_id.ast_id_map(db).ast_id_of_view::<ast::MacroCall>(call);

        if macro_depth > recursion_limit {
            return ExpandResult::only_err(ExpandError::new(
                span_map.span_at(call.syntax().text_range().start()),
                ExpandErrorKind::RecursionOverflow,
            ));
        }

        let ExpandResult { value, err } = match def.kind {
            MacroDefKind::BuiltInEager(..) => {
                let ExpandResult { value, err } = expand_eager_macro_input_view(
                    db,
                    krate,
                    call,
                    curr.with_value(ast_id),
                    def,
                    call_site,
                    macro_depth + 1,
                    recursion_limit,
                    macro_resolver,
                    eager_calls,
                );
                match value {
                    Some(call_id) => {
                        // alloc: the caller's buffer, once per nested macro call
                        eager_calls.push((
                            curr.with_value(ast_id).map(|ast_id| (AstPtr::of(call), ast_id)),
                            call_id,
                        ));
                        let ExpandResult { value: (parse, map), err: err2 } =
                            call_id.parse_macro_expansion(db);

                        map.iter().for_each(|(o, span)| expanded_map.push(o + offset, span));

                        let root = parse.root();
                        ExpandResult {
                            value: Some((
                                EagerInput::Original(root),
                                offset + root.text_range().len(),
                            )),
                            err: err.or_else(|| err2.clone()), // clone: an `Option<ExpandError>`: an `Arc` count, only on error
                        }
                    }
                    None => ExpandResult { value: None, err },
                }
            }
            MacroDefKind::Declarative(..)
            | MacroDefKind::BuiltIn(..)
            | MacroDefKind::BuiltInAttr(..)
            | MacroDefKind::BuiltInDerive(..)
            | MacroDefKind::ProcMacro(..)
            | MacroDefKind::UnimplementedBuiltIn(..) => {
                let expand_to = match ExpandTo::of_call_site_view(call) {
                    Ok(it) => it,
                    Err(parent) => {
                        error = Some(ExpandError::other(
                            span_map.span_at(call.syntax().text_range().start()),
                            // alloc: an error's message, on the error path
                            format!("blaze can't tell what a macro call in a `{parent:?}` expands to"),
                        ));
                        offset += call.syntax().text_range().len();
                        continue;
                    }
                };
                let ExpandResult { value: (parse, tm), err } = lazy_expand_view(
                    db,
                    &def,
                    call,
                    curr.with_value(ast_id),
                    expand_to,
                    krate,
                    call_site,
                    macro_depth,
                    eager_calls,
                );

                // replace macro inside
                let ExpandResult { value, err: error } = eager_macro_recur_view(
                    db,
                    tm,
                    expanded_map,
                    offset,
                    // FIXME: We discard parse errors here
                    parse,
                    krate,
                    call_site,
                    macro_depth + 1,
                    recursion_limit,
                    macro_resolver,
                    eager_calls,
                );
                let err = err.or(error);

                ExpandResult { value, err }
            }
        };
        if err.is_some() {
            error = err;
        }
        // check if the whole original syntax is replaced
        if call.syntax() == original {
            return ExpandResult { value, err: error };
        }

        match value {
            Some((insert, new_offset)) => {
                replacements.push((call, insert));
                offset = new_offset;
            }
            None => offset += call.syntax().text_range().len(),
        }
    }

    if replacements.is_empty() {
        return ExpandResult { value: Some((EagerInput::Original(original), offset)), err: error };
    }
    let expanded = replace_calls(original, &replacements);
    ExpandResult { value: Some((EagerInput::Built(expanded), offset)), err: error }
}

/// `original` with each call of `replacements` (in preorder, none inside another) replaced by
/// its expansion's root, the rest copied as it is, in one copy.
fn replace_calls(
    original: view::Node<'_>,
    replacements: &[(view::MacroCall<'_>, EagerInput<'_>)],
) -> std::sync::Arc<FlatTree> {
    // alloc: one tree for each input whose nested calls are replaced
    let mut builder = FlatTreeBuilder::with_capacity(usize::from(original.text_range().len()));
    let mut replacements = replacements.iter().peekable();
    let mut events = original.preorder_with_tokens();
    while let Some(event) = events.next() {
        match event {
            WalkEvent::Enter(NodeOrToken::Node(node)) => {
                match replacements.next_if(|(call, _)| call.syntax() == node) {
                    Some((_, insert)) => {
                        builder.view(NodeOrToken::Node(insert.root()));
                        events.skip_subtree();
                        let leave = events.next();
                        debug_assert!(matches!(
                            leave,
                            Some(WalkEvent::Leave(NodeOrToken::Node(it))) if it == node
                        ));
                    }
                    None => builder.start_node(node.raw_kind()),
                }
            }
            WalkEvent::Enter(NodeOrToken::Token(token)) => {
                builder.token(token.raw_kind(), token.text())
            }
            WalkEvent::Leave(NodeOrToken::Node(_)) => builder.finish_node(),
            WalkEvent::Leave(NodeOrToken::Token(_)) => {}
        }
    }
    assert!(replacements.next().is_none(), "a replaced call outside the eager input");
    std::sync::Arc::new(builder.finish()) // alloc: the tree's count, once per built tree
}
