//! `#[blaze::contract_test]`, the builtin attribute `core::blaze::contract_test`
//! (docs/v2/plan/design/contracts.md, part 2): a property test of a function's native contract
//! clauses (`requires`, `ensures`, `#![feature(blaze_verify)]`), generated next to it.
//!
//! The function stays as written, and an ordinary test follows it:
//!
//! ```ignore (pseudo-rust)
//! #[blaze::contract_test]
//! fn f(x: u32, s: &str) -> (r: u32) requires x < 10 ensures r > x { .. }
//! // expands to the function and
//! #[cfg(test)]
//! #[::core::prelude::v1::test]
//! fn f_contract() {
//!     // per argument: traits whose `on_unimplemented` names the argument, the function and
//!     // what is missing (a Hegel default generator, `Debug`, `Clone`, `ToOwned`)
//!     ::hegel::Hegel::new(|__blaze_tc: ::hegel::TestCase| {
//!         let __blaze_arg0: u32 = <u32 as __BlazeDraw0>::__blaze_draw(&__blaze_tc);
//!         let __blaze_arg1: String = /* a `<str as ToOwned>::Owned` drawn */;
//!         let __blaze_case: String = /* the case as a plain `#[test]`, the values printed */;
//!         { let x: u32 = clone of arg0; let s: &str = borrow of arg1; __blaze_tc.assume(x < 10); }
//!         let __blaze_ret = catch_unwind(|| f(clone of arg0, borrow of arg1)); // a panic fails the case
//!         { let x: u32 = ..; let s: &str = ..; let r = __blaze_ret; if !(r > x) { fail } }
//!     }).__database_key(..).run();
//! }
//! ```
//!
//! Each argument is drawn with Hegel's default generator for its type; an argument `&T` is lent
//! from a drawn `<T as ToOwned>::Owned` (`&str` from a `String`, `&[T]` from a `Vec<T>`). An
//! owned argument is passed as a clone, so the drawn value is still there for the clauses and
//! the report: it must be `Clone`, and every argument `Debug`, each a compile error naming the
//! argument when it is not. The `requires` are assumed (a case where one is false is rejected,
//! not failed), each `ensures` is checked after the call with the named return bound to the
//! result. `recommends` and `decreases` are not checked: a `recommends` does not restrict the
//! callers of a function that runs, and termination is not a property one call can fail.
//!
//! A failure, on Hegel's last replay of the shrunk case, panics with a message naming the
//! function and the clause (or the function's own panic) and the case as a plain `#[test]` with
//! the arguments as literals, to paste next to the function and keep. A value whose `Debug` is
//! not its own Rust expression (a NaN, a map, a `Vec<String>`'s elements) prints as its `Debug`.
//!
//! What the test can't do is refused with an error naming the function and what it is: a
//! generic function, a method or associated function, one inside a block, an `async` or
//! `unsafe` one, an argument that is `&mut`, `impl Trait` or a pattern, and a crate built with
//! `cfg(test)` that has no `hegel` dependency. On a refusal the function is kept as written.
//!
//! `#[blaze::contract_test(skip)]` keeps the function and adds nothing.

use std::fmt::Write as _;

use base_db::Db;
use blaze_mem::Frame;
use intern::sym;
use parser::SyntaxKind;
use span::Span;
use syntax::view::{self as ast, AstNode, HasGenericParams, HasModuleItem, HasName};
use syntax_bridge::DocCommentDesugarMode;

use crate::{
    ExpandError, ExpandResult, MacroCallId, MacroCallKind,
    builtin::fn_macro::{source_joined, through_proc_macro, write_assert_condition},
    tt,
};

pub(super) fn contract_test_expand<'f>(
    db: &Db,
    frame: &'f Frame,
    id: MacroCallId,
    tt: &tt::TopSubtree,
    span: Span,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    let unchanged = || tt::TopSubtree::from_subtree_in(tt.view(), frame);
    let loc = id.loc(db);
    if let MacroCallKind::Attr { attr_args: Some(args), .. } = &loc.kind
        && !args.token_trees().is_empty()
    {
        let mut it = args.token_trees().iter();
        let is_skip = matches!(
            (it.next(), it.next()),
            (Some(tt::TtElement::Leaf(tt::Leaf::Ident(ident))), None) if ident.sym.as_str() == "skip"
        );
        if is_skip {
            return ExpandResult::ok(unchanged());
        }
        let given = ::tt::stringify(
            args.token_trees(),
            &|s, t| source_joined(db, s, t),
            through_proc_macro(db, id),
        );
        // alloc: an error's message, once per refused attribute
        let msg = format!(
            "`#[blaze::contract_test(..)]` takes one argument, `skip` (leave the function out), \
             and was given `{given}`"
        );
        return ExpandResult::new(unchanged(), ExpandError::other(span, msg));
    }
    let item = loc.to_node_item_view(db).value;
    let parent = item.syntax().parent().map(|it| it.kind());
    let has_hegel =
        || loc.krate.data(db).dependencies.iter().any(|dep| dep.name.symbol().as_str() == "hegel");
    let test_cfg = loc.krate.cfg_options(db).check_atom(&cfg::CfgAtom::Flag(sym::test));
    let generated = generate(db, id, tt, span, parent).and_then(|generated| {
        // Without `cfg(test)` the test is configured out and names nothing.
        match test_cfg && !has_hegel() {
            true => Err(format!(
                "`#[blaze::contract_test]` on `{}`: the generated test calls the `hegel` crate, and \
                 this crate, built with `cfg(test)`, has no dependency named `hegel`; add \
                 `hegeltest` as a dev-dependency",
                generated.function
            )),
            false => Ok(generated),
        }
    });
    let generated = match generated {
        Ok(it) => it,
        Err(msg) => return ExpandResult::new(unchanged(), ExpandError::other(span, msg)),
    };
    let mut out = tt::TopSubtreeBuilder::new(tt::Delimiter {
        kind: tt::DelimiterKind::Invisible,
        open: span,
        close: span,
    });
    out.extend_with_tt(whole(tt));
    splice(&mut out, generated.template.iter(), &generated.users);
    ExpandResult::ok(out.build_in(frame))
}

/// The tokens of `tree`, with its top group when that is a real one (a tree of one group
/// keeps it as its top).
fn whole(tree: &tt::TopSubtree) -> tt::TokenTreesView<'_> {
    match tree.top_subtree().delimiter.kind {
        tt::DelimiterKind::Invisible => tree.token_trees(),
        _ => tree.as_token_trees(),
    }
}

/// Copies `template` into `out`, each placeholder `__blaze_user_N` replaced by `users[N]`, the
/// function's own tokens with their spans.
fn splice(out: &mut tt::TopSubtreeBuilder, template: tt::TtIter<'_>, users: &[tt::TopSubtree]) {
    for element in template {
        match element {
            tt::TtElement::Leaf(tt::Leaf::Ident(ident))
                if let Some(at) = ident.sym.as_str().strip_prefix(PLACEHOLDER)
                    && let Ok(at) = at.parse::<usize>()
                    && let Some(user) = users.get(at) =>
            {
                out.extend_with_tt(whole(user));
            }
            tt::TtElement::Leaf(leaf) => out.push(leaf),
            tt::TtElement::Subtree(subtree, inner) => {
                out.open(subtree.delimiter.kind, subtree.delimiter.open);
                splice(out, inner, users);
                out.close(subtree.delimiter.close);
            }
        }
    }
}

const PLACEHOLDER: &str = "__blaze_user_";

/// The test, as a template whose placeholders stand for the function's tokens.
struct Generated {
    /// The function's name, for errors.
    function: String,
    template: tt::TopSubtree,
    users: Vec<tt::TopSubtree>,
}

/// How the template is written: the function's tokens become placeholders, and its text is
/// read from the source, as `stringify!` reads it.
struct Writer<'a> {
    db: &'a Db,
    span: Span,
    map: &'a span::SpanMap,
    through_proc_macro: bool,
    users: Vec<tt::TopSubtree>,
}

impl Writer<'_> {
    fn tokens(&self, node: ast::Node<'_>) -> tt::TopSubtree {
        syntax_bridge::call_input(node, self.map, self.span, DocCommentDesugarMode::ProcMacro)
    }

    /// A placeholder for `node`'s tokens, and the node's source text.
    fn user(&mut self, node: ast::Node<'_>) -> (String, String) {
        let tokens = self.tokens(node);
        let text = ::tt::stringify(
            whole(&tokens),
            &|span, text| source_joined(self.db, span, text),
            self.through_proc_macro,
        );
        let placeholder = format!("{PLACEHOLDER}{}", self.users.len());
        self.users.push(tokens); // alloc: one tree per spliced node of the function, once per expansion
        (placeholder, text)
    }
}

/// One argument of the function.
struct Arg {
    /// The name the clauses bind it by, a placeholder of the parameter's own name, or `_`.
    binding: String,
    /// The name the printed test binds it by.
    shown: String,
    /// The parameter's type, a placeholder, and its text.
    ty: String,
    ty_text: String,
    /// `&T`: lent from a drawn `<T as ToOwned>::Owned`, whose type `T` is.
    lent: Option<String>,
    /// What the printed test writes around the value's `Debug`.
    prefix: &'static str,
    suffix: &'static str,
}

fn generate(
    db: &Db,
    id: MacroCallId,
    tt: &tt::TopSubtree,
    span: Span,
    parent: Option<SyntaxKind>,
) -> Result<Generated, String> {
    let edition = span.ctx.edition(db);
    let (parsed, map, _) = blaze_mem::frame::task(blaze_mem::Tag::Expand, |frame| {
        crate::token_tree_to_syntax_node(db, frame, tt.view(), crate::ExpandTo::Items)
    });
    let not_fn = || {
        "`#[blaze::contract_test]` goes on a function with `requires` or `ensures` clauses, and \
         this item is not a function"
            .to_owned()
    };
    let function = ast::MacroItems::cast(parsed.root())
        .and_then(|items| items.items().next())
        .and_then(|item| ast::Fn::cast(item.syntax()))
        .ok_or_else(not_fn)?;
    let name = function.name().ok_or_else(not_fn)?;
    let mut w = Writer {
        db,
        span,
        map: &map,
        through_proc_macro: through_proc_macro(db, id),
        users: Vec::new(),
    };
    let (fn_ph, fn_text) = w.user(name.syntax());
    let refuse = |why: &str| Err(format!("`#[blaze::contract_test]` on `{fn_text}`: {why}"));

    match parent {
        None | Some(SyntaxKind::SOURCE_FILE | SyntaxKind::ITEM_LIST | SyntaxKind::MACRO_ITEMS) => {}
        Some(SyntaxKind::ASSOC_ITEM_LIST) => {
            return refuse(
                "an associated function is not supported yet: the generated test is a function of \
                 the module, which an `impl` or trait can't hold; test a free function that calls it",
            );
        }
        Some(_) => {
            return refuse(
                "a function inside a block is not supported: the test harness only runs tests of \
                 a module; move the function to a module",
            );
        }
    }
    if function.body().is_none() {
        return refuse("the function has no body to test");
    }
    if function.generic_param_list().is_some_and(|it| it.generic_params().next().is_some()) {
        return refuse(
            "a generic function is not supported: each argument is drawn as one type, and a \
             generic function has none; test a non-generic function that calls it with the types \
             to test",
        );
    }
    if function.async_token().is_some() || function.gen_token().is_some() {
        return refuse(
            "an `async` or `gen` function is not supported: the test calls it once and checks what it returns",
        );
    }
    if function.unsafe_token().is_some() {
        return refuse(
            "an `unsafe` function is not supported: its `requires` need not hold all of what makes \
             a call sound, and the test would call it on any arguments they allow",
        );
    }
    let params = function.param_list();
    if params.and_then(|it| it.self_param()).is_some() {
        return refuse("a method is not supported yet: test a free function that calls it");
    }

    // The clauses.
    let mut requires = Vec::new(); // alloc: one placeholder per clause expression, once per expansion
    let mut ensures = Vec::new(); // alloc: as above
    for clause in function.spec_clauses() {
        let list = match () {
            _ if clause.requires_token().is_some() => &mut requires,
            _ if clause.ensures_token().is_some() => &mut ensures,
            // `recommends` binds no caller of a function that runs; `decreases` is termination.
            _ => continue,
        };
        for expr in clause.exprs() {
            let (ph, text) = w.user(expr.syntax());
            let mut shown = String::new();
            write_assert_condition(&mut shown, &text, edition);
            list.push((ph, shown));
        }
    }
    if requires.is_empty() && ensures.is_empty() {
        return refuse(
            "the function has no `requires` or `ensures` clause, so there is no contract to test",
        );
    }
    let ret = function.ret_type().and_then(|it| it.pat()).map(|pat| w.user(pat.syntax()));

    // The arguments.
    let mut args = Vec::new(); // alloc: one per parameter, once per expansion
    for (at, param) in params.into_iter().flat_map(|it| it.params()).enumerate() {
        let (Some(pat), Some(ty)) = (param.pat(), param.ty()) else {
            return refuse("an argument without a name or type (`...`) is not supported");
        };
        let (binding, shown) = match pat {
            ast::Pat::IdentPat(ident) if ident.pat().is_none() && ident.ref_token().is_none() => {
                let Some(name) = ident.name() else { return refuse("an argument has no name") };
                let (ph, text) = w.user(name.syntax());
                (ph, text)
            }
            ast::Pat::WildcardPat(_) => ("_".to_owned(), format!("_arg{at}")),
            _ => {
                let (_, text) = w.user(pat.syntax());
                return refuse(&format!(
                    "argument `{text}` is a pattern, and the test names each argument to print the \
                     case; give it a name and destructure it in the body"
                ));
            }
        };
        let (ty_ph, ty_text) = w.user(ty.syntax());
        let mut prefix = "";
        let mut suffix = "";
        let lent = match ty {
            ast::Type::RefType(r) if r.mut_token().is_some() => {
                return refuse(&format!(
                    "argument `{shown}` is `{ty_text}`: an `ensures` about a `&mut` argument (its value \
                     after the call) is not supported yet"
                ));
            }
            ast::Type::RefType(r) => {
                let Some(inner) = r.ty() else {
                    return refuse(&format!("argument `{shown}` has no type"));
                };
                (prefix, suffix) = match inner {
                    ast::Type::PathType(p) if last_segment(p) == Some("str") => ("", ""),
                    ast::Type::SliceType(_) => ("&", ""),
                    ast::Type::PathType(p) if last_segment(p) == Some("String") => {
                        ("&String::from(", ")")
                    }
                    ast::Type::PathType(p) if last_segment(p) == Some("Vec") => ("&vec!", ""),
                    _ => ("&", ""),
                };
                let (inner_ph, _) = w.user(inner.syntax());
                Some(inner_ph)
            }
            ast::Type::ImplTraitType(_) => {
                return refuse(&format!(
                    "argument `{shown}` is `{ty_text}`, which is generic: each argument is drawn as \
                     one type; test a function that names it"
                ));
            }
            ast::Type::PathType(p) => {
                (prefix, suffix) = match last_segment(p) {
                    Some("String") => ("String::from(", ")"),
                    Some("Vec") => ("vec!", ""),
                    _ => ("", ""),
                };
                None
            }
            _ => None,
        };
        args.push(Arg { binding, shown, ty: ty_ph, ty_text, lent, prefix, suffix });
    }

    let template = write_template(&fn_ph, &fn_text, &args, &requires, &ensures, ret.as_ref());
    let template = syntax_bridge::parse_to_token_tree_static_span(edition, span, &template)
        .ok_or_else(|| {
            format!("`#[blaze::contract_test]` on `{fn_text}`: the generated test does not lex")
        })?;
    Ok(Generated { function: fn_text, template, users: w.users })
}

/// The last segment of a path type's path, `Vec` of `std::vec::Vec<u8>`.
fn last_segment<'t>(p: ast::PathType<'t>) -> Option<&'t str> {
    Some(p.path()?.segment()?.name_ref()?.text())
}

/// `text` as a Rust string literal.
fn lit(text: &str) -> String {
    format!("{text:?}")
}

/// The test's text; `fn_ph`, each argument's `binding` and `ty`, and the clauses are
/// placeholders for the function's own tokens.
fn write_template(
    fn_ph: &str,
    fn_text: &str,
    args: &[Arg],
    requires: &[(String, String)],
    ensures: &[(String, String)],
    ret: Option<&(String, String)>,
) -> String {
    let f = fn_text.strip_prefix("r#").unwrap_or(fn_text);
    let test = format!("{f}_contract");
    let mut t = String::new();
    // `fmt::Write` for a `String` does not fail.
    let mut w = |s: std::fmt::Arguments<'_>| _ = t.write_fmt(s);
    w(format_args!(
        "#[cfg(test)]\n#[::core::prelude::v1::test]\n\
         #[allow(unused, non_camel_case_types, non_snake_case, clippy::all)]\nfn {test}() {{\n"
    ));
    let message = |what: &str| lit(&format!("`#[blaze::contract_test]` on `{fn_text}`: {what}"));
    for (i, a) in args.iter().enumerate() {
        let n = &a.shown;
        let ty = &a.ty_text;
        let drawn = match &a.lent {
            Some(inner) => {
                w(format_args!(
                    "#[diagnostic::on_unimplemented(message = {})]\n\
                     trait __BlazeOwned{i} {{ type Owned; }}\n\
                     impl<__BlazeX: ?::core::marker::Sized + ::std::borrow::ToOwned> __BlazeOwned{i} for __BlazeX \
                     {{ type Owned = <__BlazeX as ::std::borrow::ToOwned>::Owned; }}\n",
                    message(&format!(
                        "argument `{n}` is `{ty}`, lent from an owned value the test draws, which needs \
                         `{{Self}}: ToOwned`"
                    )),
                ));
                format!("<{inner} as __BlazeOwned{i}>::Owned")
            }
            None => a.ty.clone(), // clone: a placeholder's name, once per argument
        };
        w(format_args!(
            "#[diagnostic::on_unimplemented(message = {}, note = {})]\n\
             trait __BlazeDraw{i}: ::core::marker::Sized {{ fn __blaze_draw(__blaze_tc: &::hegel::TestCase) -> Self; }}\n\
             impl<__BlazeX: ::hegel::generators::DefaultGenerator> __BlazeDraw{i} for __BlazeX {{\n\
             fn __blaze_draw(__blaze_tc: &::hegel::TestCase) -> Self {{ __blaze_tc.draw_silent(::hegel::generators::default::<__BlazeX>()) }}\n\
             }}\n",
            message(&format!(
                "argument `{n}` can't be drawn: `{{Self}}` has no Hegel default generator"
            )),
            lit(&format!(
                "derive or implement `hegel::DefaultGenerator` for `{{Self}}`, or leave `{fn_text}` out \
                 with `#[blaze::contract_test(skip)]`"
            )),
        ));
        w(format_args!(
            "#[diagnostic::on_unimplemented(message = {})]\n\
             trait __BlazeShow{i} {{ fn __blaze_show(&self) -> ::std::string::String; }}\n\
             impl<__BlazeX: ::core::fmt::Debug> __BlazeShow{i} for __BlazeX {{\n\
             fn __blaze_show(&self) -> ::std::string::String {{ ::std::format!(\"{{:?}}\", self) }}\n\
             }}\n",
            message(&format!(
                "argument `{n}` must be `Debug` to print a failing case, and `{{Self}}` is not"
            )),
        ));
        if a.lent.is_none() {
            w(format_args!(
                "#[diagnostic::on_unimplemented(message = {})]\n\
                 trait __BlazeClone{i}: ::core::marker::Sized {{ fn __blaze_clone(&self) -> Self; }}\n\
                 impl<__BlazeX: ::core::clone::Clone> __BlazeClone{i} for __BlazeX {{\n\
                 fn __blaze_clone(&self) -> Self {{ ::core::clone::Clone::clone(self) }}\n\
                 }}\n",
                message(&format!(
                    "argument `{n}` must be `Clone`: the test passes the function a clone and keeps \
                     the drawn value for the `ensures` and the report, and `{{Self}}` is not `Clone`"
                )),
            ));
        }
        a_drawn_type_is(&mut w, i, &drawn);
    }

    // The case.
    w(format_args!("::hegel::Hegel::new(|__blaze_tc: ::hegel::TestCase| {{\n"));
    for i in 0..args.len() {
        w(format_args!(
            "let __blaze_arg{i}: __BlazeDrawn{i} = <__BlazeDrawn{i} as __BlazeDraw{i}>::__blaze_draw(&__blaze_tc);\n"
        ));
    }
    // The case as a plain test, to paste next to the function.
    w(format_args!("let mut __blaze_case = ::std::string::String::new();\n"));
    let push = |w: &mut dyn FnMut(std::fmt::Arguments<'_>), text: &str| {
        w(format_args!("__blaze_case.push_str({});\n", lit(text)))
    };
    push(&mut w, &format!("#[test]\nfn {f}_contract_case() {{\n"));
    for (i, a) in args.iter().enumerate() {
        push(&mut w, &format!("    let {}: {} = {}", a.shown, a.ty_text, a.prefix));
        w(format_args!(
            "__blaze_case.push_str(&<__BlazeDrawn{i} as __BlazeShow{i}>::__blaze_show(&__blaze_arg{i}));\n"
        ));
        push(&mut w, &format!("{};\n", a.suffix));
    }
    let call_args = args
        .iter()
        .map(|a| match a.lent {
            Some(_) => a.shown.clone(), // clone: a name, once per argument
            None => format!("{}.clone()", a.shown),
        })
        .collect::<Vec<_>>()
        .join(", ");
    let bind = match ret {
        Some((_, pat)) => format!("let {pat} = "),
        None => String::new(),
    };
    push(&mut w, &format!("    {bind}{fn_text}({call_args});\n"));
    for (_, text) in ensures {
        push(&mut w, &format!("    assert!({text});\n"));
    }
    push(&mut w, "}\n");
    w(format_args!(
        "let __blaze_fail = |__blaze_what: &str| -> ! {{ ::core::panic!(\"{{}}{{}}{{}}\", __blaze_what, {}, __blaze_case) }};\n",
        lit(&format!("\n\nthe shrunk case, as a test to paste next to `{fn_text}`:\n\n")),
    ));
    let views = |w: &mut dyn FnMut(std::fmt::Arguments<'_>)| {
        for (i, a) in args.iter().enumerate() {
            let view = view(i, a);
            w(format_args!("let {}: {} = {view};\n", a.binding, a.ty));
        }
    };

    // `requires`: assumed, so a case where one is false is rejected rather than failed.
    if !requires.is_empty() {
        w(format_args!("{{\n"));
        views(&mut w);
        for (ph, _) in requires {
            w(format_args!("__blaze_tc.assume({ph});\n"));
        }
        w(format_args!("}}\n"));
    }

    // The call: a panic is the function's failure, reported as one.
    let passed = (0..args.len()).map(|i| view(i, &args[i])).collect::<Vec<_>>().join(", ");
    w(format_args!(
        "let __blaze_ret = match ::std::panic::catch_unwind(::std::panic::AssertUnwindSafe(|| {fn_ph}({passed}))) {{\n\
         ::core::result::Result::Ok(__blaze_ret) => __blaze_ret,\n\
         ::core::result::Result::Err(__blaze_payload) => {{\n\
         let __blaze_why: &str = if let ::core::option::Option::Some(__blaze_it) = __blaze_payload.downcast_ref::<&'static str>() {{ __blaze_it }}\n\
         else if let ::core::option::Option::Some(__blaze_it) = __blaze_payload.downcast_ref::<::std::string::String>() {{ __blaze_it.as_str() }}\n\
         else {{ \"a panic whose payload is not a string\" }};\n\
         __blaze_fail(&::std::format!(\"{{}}{{}}\", {}, __blaze_why))\n\
         }}\n\
         }};\n",
        message("the function panicked on arguments its `requires` allow: "),
    ));

    // `ensures`: each checked on the result, bound to the named return.
    w(format_args!("{{\n"));
    views(&mut w);
    match ret {
        Some((ph, _)) => w(format_args!("let {ph} = __blaze_ret;\n")),
        None => w(format_args!("let _ = __blaze_ret;\n")),
    }
    for (ph, text) in ensures {
        w(format_args!(
            "if !({ph}) {{ __blaze_fail({}) }}\n",
            message(&format!("`ensures {text}` does not hold")),
        ));
    }
    w(format_args!("}}\n"));

    w(format_args!(
        "}})\n.__database_key(::std::format!(\"{{}}::{{}}\", ::core::module_path!(), {}))\n.run();\n}}\n",
        lit(&test),
    ));
    t
}

/// `type __BlazeDrawnI = <the type argument I is drawn as>;`, the name the case uses for it.
fn a_drawn_type_is(w: &mut dyn FnMut(std::fmt::Arguments<'_>), i: usize, drawn: &str) {
    w(format_args!("type __BlazeDrawn{i} = {drawn};\n"));
}

/// What a clause, or the call, is given for argument `i`: a clone of an owned value, or a
/// borrow of the drawn value it is lent from.
fn view(i: usize, a: &Arg) -> String {
    match a.lent {
        Some(_) => format!("::core::borrow::Borrow::borrow(&__blaze_arg{i})"),
        None => format!("<__BlazeDrawn{i} as __BlazeClone{i}>::__blaze_clone(&__blaze_arg{i})"),
    }
}
