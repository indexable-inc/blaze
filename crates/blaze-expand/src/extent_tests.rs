//! extent.rs and the derive's item reader on it (adt.rs), per element kind: the counterexamples
//! the hand walks this crate had got wrong (each case's CONTROL comment states the old walk's
//! answer on that input; the walk itself is gone), and a negative twin per kind: malformed
//! input is a typed [`ExtentError`] naming the element, never a silent cut (rule 23, 3, 19).
//!
//! `cargo test -p blaze-expand --lib extent_tests` prints
//! "extent: a field default `g::<A, B>()` is one field; a param default `HashMap<K, V>` one
//! param".

extern crate std;

use core::ops::Range;

use blaze_own::Alloc;
use parser::SyntaxKind;

use crate::adt::{self, FieldsKind, NotAnAdt, ParamKind};
use crate::extent::{Extent, ExtentError, ExtentWhy, Prefix, Words};
use crate::tokens::{Delim, Edition, TokenKind, TokenStream, lex};

fn alloc() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

fn tokens(text: &str) -> TokenStream<'static> {
    lex(text, Edition::E2021, alloc()).expect("the test's text lexes")
}

/// The tokens `range` of `stream`, one space between two: punctuation and delimiters by their
/// character.
fn text(stream: &TokenStream<'_>, range: Range<usize>) -> std::string::String {
    let mut out = std::string::String::new();
    for at in range {
        if !out.is_empty() {
            out.push(' ');
        }
        match stream.tokens()[at].kind {
            TokenKind::Punct { char, .. } => out.push(char),
            TokenKind::Open(Delim::Paren) => out.push('('),
            TokenKind::Close(Delim::Paren) => out.push(')'),
            TokenKind::Open(Delim::Brace) => out.push('{'),
            TokenKind::Close(Delim::Brace) => out.push('}'),
            TokenKind::Open(Delim::Bracket) => out.push('['),
            TokenKind::Close(Delim::Bracket) => out.push(']'),
            TokenKind::Open(Delim::Invisible) | TokenKind::Close(Delim::Invisible) => out.push('~'),
            TokenKind::Ident { .. } | TokenKind::Literal { .. } => out.push_str(stream.text_at(at)),
        }
    }
    out
}

/// Fields: a default whose expression holds a turbofish's comma, or a closure's, is one field.
/// CONTROL: adt.rs `split` (`Angles::UntilEq`) stopped counting `<` and `>` at the lone `=`,
/// so on `struct S { f: u8 = g::<A, B>(), h: u8 }` it cut the pieces `f: u8 = g::<A` and
/// `B>()` and refused the item as `Malformed`; on `f: fn(u8, u8) -> u8 = |a, b| a` it cut at
/// the closure's comma the same way.
#[test]
fn a_field_default_with_a_comma_is_one_field() {
    let item = tokens("struct S { f: u8 = g::<A, B>(), h: u8 }");
    let adt = adt::read(&item, alloc()).expect("the struct reads");
    assert_eq!(adt.fields.len(), 2);
    assert!(adt.fields[0].defaulted && !adt.fields[1].defaulted);
    assert_eq!(text(&item, adt.fields[0].ty.clone()), "u8");
    assert_eq!(text(&item, adt.fields[1].ty.clone()), "u8");
    let closure = tokens("struct S { f: fn(u8, u8) -> u8 = |a, b| a, h: HashMap<u8, u16> }");
    let adt = adt::read(&closure, alloc()).expect("the struct reads");
    assert_eq!(adt.fields.len(), 2);
    assert_eq!(text(&closure, adt.fields[1].ty.clone()), "HashMap < u8 , u16 >");
    // The negative twin: a field with no type is refused naming the field.
    let missing = tokens("struct S { f: , h: u8 }");
    match adt::read(&missing, alloc()) {
        Err(NotAnAdt::Syntax(ExtentError { what: Extent::Field, why: ExtentWhy::Refused, .. })) => {}
        other => panic!("a field with no type must be refused naming the field: {:?}", other.err()),
    }
    std::println!("extent: a field default `g::<A, B>()` is one field; a param default `HashMap<K, V>` one param");
}

/// Generic parameters: a type parameter's bounds and default holding commas inside `<..>`, a
/// const parameter's type and default, a lifetime's bounds. CONTROL: item.rs `element_end` on
/// `<#[cfg(x)] T = HashMap<K, V>, U>` stopped counting angles at the lone `=` and ended the
/// parameter at `K,`, leaving `V>, U>` in the derive's input (derive_input.rs has that case).
#[test]
fn generic_params_are_the_parsers() {
    let item = tokens("struct S<'a: 'b, T: Clone + Tr<A, B> = HashMap<K, V>, const N: usize = 3, U> { t: T }");
    let adt = adt::read(&item, alloc()).expect("the struct reads");
    assert_eq!(adt.params.len(), 4);
    assert_eq!(adt.params[0].kind, ParamKind::Lifetime);
    assert_eq!(text(&item, adt.params[0].decl.clone()), "' a : ' b");
    assert_eq!(text(&item, adt.params[0].name.clone()), "' a");
    let ParamKind::Type { bounds: Some(bounds) } = adt.params[1].kind.clone() else { panic!("T has bounds") };
    assert_eq!(text(&item, bounds), "Clone + Tr < A , B >");
    let ParamKind::Const { ty } = adt.params[2].kind.clone() else { panic!("N is a const parameter") };
    assert_eq!(text(&item, ty), "usize");
    assert_eq!(adt.params[3].kind, ParamKind::Type { bounds: None });
    // The negative twin: a default with no type is refused naming the parameter.
    let missing = tokens("struct S<T = > { t: T }");
    match adt::read(&missing, alloc()) {
        Err(NotAnAdt::Syntax(ExtentError { what: Extent::GenericParam, .. })) => {}
        other => panic!("a parameter default with no type must be refused naming it: {:?}", other.err()),
    }
}

/// Variants: a discriminant whose expression holds `<`, `>` and a comma. CONTROL: `split`
/// (`Angles::UntilEq`) read `A = f::<u8, u16>()` as the pieces `A = f::<u8` and `u16>()`.
#[test]
fn a_discriminant_with_a_comma_is_one_variant() {
    let item = tokens("enum E { A = f::<u8, u16>(), B(u8, HashMap<u8, u16>), #[default] C { c: u8 } }");
    let adt = adt::read(&item, alloc()).expect("the enum reads");
    assert_eq!(adt.variants.len(), 3);
    assert_eq!(adt.variants[1].kind, FieldsKind::Tuple);
    assert_eq!(adt.variants[1].fields.len(), 2);
    assert_eq!(adt.variants[2].kind, FieldsKind::Record);
    assert_eq!(adt.kind, adt::AdtKind::Enum { default_variant: Some(2) });
    let missing = tokens("enum E { A = , B }");
    match adt::read(&missing, alloc()) {
        Err(NotAnAdt::Syntax(ExtentError { what: Extent::Variant, .. })) => {}
        other => panic!("a discriminant with no expression must be refused naming the variant: {:?}", other.err()),
    }
}

/// A tuple field's visibility is the parser's: `pub (crate::A)` is `pub` then the type
/// `(crate::A)`. CONTROL: adt.rs `skip_vis` read `pub (crate` as the restriction `pub(crate)`
/// and took the group whole, leaving no type.
#[test]
fn a_tuple_field_visibility_is_the_parsers() {
    let item = tokens("struct S(pub (crate::A), pub(crate) u8);");
    let adt = adt::read(&item, alloc()).expect("the struct reads");
    assert_eq!(adt.fields.len(), 2);
    assert_eq!(text(&item, adt.fields[0].ty.clone()), "( crate : : A )");
    assert_eq!(text(&item, adt.fields[1].ty.clone()), "u8");
}

/// The path types rustc's derives bound (`find_type_parameters`): two segments or more whose
/// first is a type parameter, nested ones too; not `<T as Tr>::X`'s trait, not `::T::X`.
/// CONTROL: adt.rs `assoc_at` took only `T::X` of exactly two segments, so `T::A::B` was not
/// bounded, and skipped what was inside an `assoc` it took.
#[test]
fn associated_path_types_are_rustcs() {
    let item = tokens("struct S<T, const N: usize> { a: T::A::B, b: Vec<T::C<T::D>>, c: <T as Tr>::E, d: ::T::F, e: N::G }");
    let adt = adt::read(&item, alloc()).expect("the struct reads");
    let assoc: std::vec::Vec<_> = adt.assoc.iter().map(|r| text(&item, r.clone())).collect();
    assert_eq!(assoc, ["T : : A : : B", "T : : C < T : : D >", "T : : D"]);
}

/// An expression list's elements are the parser's: a turbofish's comma ends none, a trailing
/// comma is taken, and two elements with no comma between are refused.
#[test]
fn an_expression_list_is_the_parsers() {
    let stream = tokens("f::<A, B>(), |a, b| a, x < y,");
    let words = Words::new(&stream, alloc());
    let list = words.list(Prefix::Expr, 0..stream.len(), alloc()).expect("the list reads");
    let pieces: std::vec::Vec<_> = list.iter().map(|r| text(&stream, r.clone())).collect();
    assert_eq!(pieces, ["f : : < A , B > ( )", "| a , b | a", "x < y"]);
    let unseparated = tokens("a b");
    let words = Words::new(&unseparated, alloc());
    assert_eq!(
        words.list(Prefix::Expr, 0..unseparated.len(), alloc()).err(),
        Some(ExtentError { what: Extent::Expr, why: ExtentWhy::Unseparated, token: 1 })
    );
}

/// Nothing outside: the calls below are of `macro_rules!` and of builtins that ask nothing.
struct NoCx;

impl crate::ExpandCx for NoCx {
    fn env(&self, _: &str) -> Result<Option<&str>, crate::HostError> {
        Ok(None)
    }

    fn cfg(&self, _: &TokenStream<'_>) -> Result<bool, crate::HostError> {
        Ok(false)
    }

    fn include(&self, _: &str, _: crate::IncludeAs) -> Result<crate::Included<'_>, crate::HostError> {
        Ok(crate::Included::Missing)
    }

    fn place(&self, _: crate::PlaceOf) -> Result<crate::Place<'_>, crate::HostError> {
        Ok(crate::Place::Number(1))
    }

    fn run_proc_macro<'a>(
        &self,
        _: &crate::MacroDefinition<'_>,
        _: crate::ProcCall<'_>,
        _: Alloc<'a>,
    ) -> Result<crate::Produced<'a>, crate::HostError> {
        Err(crate::HostError { ask: crate::HostAsk::ProcMacro, why: crate::HostWhy::Failed })
    }
}

fn def_at(krate: u8) -> crate::DefAt {
    crate::DefAt {
        site: crate::DefSite { krate: blaze_id::CrateItemKey(blaze_id::ContentId([krate; 32])), module: None },
        edition: Edition::E2021,
        export: crate::Export::Local,
        local_inner: crate::LocalInner::No,
        transparency: None,
    }
}

fn site() -> crate::CallSite {
    crate::CallSite { edition: Edition::E2021, panic_edition: Edition::E2021, ctx: crate::Ctx::root(Edition::E2021) }
}

/// What `def` answers for the call with arguments `args`.
fn call(def: &crate::MacroDefinition<'_>, args: &TokenStream<'static>) -> Result<crate::Expanded<'static>, crate::ExpandError> {
    crate::expand(def, &crate::Call::fn_like(args, site()), &NoCx, alloc())
}

/// The expansion of the `macro_rules!` arms `body` (defined in crate `krate`) for `args`, one
/// that matched and was transcribed with no diagnostic.
fn expand_rules(body: &str, krate: u8, args: &TokenStream<'static>) -> TokenStream<'static> {
    match call(&crate::MacroDefinition::rules(tokens(body), def_at(krate)), args) {
        Ok(crate::Expanded::Expansion(expansion)) => {
            assert!(expansion.diagnostics.is_empty(), "the rules expand clean: {:?}", expansion.diagnostics);
            expansion.tokens
        }
        Ok(crate::Expanded::NeedsExpanded(nested)) => panic!("the rules need {} nested calls", nested.len()),
        Err(error) => panic!("the rules do not expand: {error:?}"),
    }
}

/// The arguments of the first call `name!(..)` (or `name! {..}`) in `stream`, inside its
/// delimiters, as a stream of their own: the expansion's tokens, its invisible groups as written,
/// sealed as a driver seals an expansion before it is another call's input (its own marks named
/// by a key, here one made of `name`'s bytes: each step of a chain calls a macro of its own).
fn args_of(stream: &TokenStream<'static>, name: &str) -> TokenStream<'static> {
    let list = stream.tokens();
    let at = (0..list.len().saturating_sub(2))
        .find(|&at| {
            matches!(list[at].kind, TokenKind::Ident { .. })
                && stream.text_at(at) == name
                && matches!(list[at + 1].kind, TokenKind::Punct { char: '!', .. })
        })
        .unwrap_or_else(|| panic!("no call of `{name}!` in {}", text(stream, 0..stream.len())));
    let close = stream.close_of(at + 2).expect("the call's arguments close");
    let mut args = stream.copy_in(at + 3..close, alloc()).expect("the arguments copy");
    let mut key = [0u8; 32];
    for (byte, from) in key.iter_mut().zip(name.bytes().cycle()) {
        *byte = from;
    }
    args.seal(blaze_id::ContentId(key));
    args
}

/// libc 0.2.191 `s2!` (src/macros.rs), reduced to its two steps that write the struct a derive
/// reads: `custom_struct!`'s plain-field arm, which takes each field apart as `$fvis:vis
/// $fname:ident: $fty:ty` and each attribute as `$(#[$attr:meta])*`, and
/// `emit_struct_definition!`, verbatim, which takes the attributes as metas again and the
/// fields as token trees. Each field's type reaches the derive as an invisible group; a meta and
/// a visibility are copied as they are (rules/matcher.rs `Origin::Ast`).
const LIBC_FIELDS: &str = "(
    $(#[$attr:meta])* $vis:vis $name:ident { $( $(#[$fattr:meta])* $fvis:vis $fname:ident: $fty:ty, )* }
) => {
    emit_struct_definition! {
        found_exhaustive_attr: true,
        body: {
            #[repr(C)]
            #[::core::prelude::v1::derive(::core::clone::Clone, ::core::marker::Copy, ::core::fmt::Debug,)]
            $(#[$attr])*
            $vis $name { $( $(#[$fattr])* $fvis $fname: $fty, )* }
        }
    }
};";

const LIBC_EMIT_STRUCT_DEFINITION: &str = "(
    found_exhaustive_attr: false,
    body: { $(#[$attr:meta])* $vis:vis $name:ident { $($field:tt)* } }
) => {
    #[allow(clippy::manual_non_exhaustive)]
    $(#[$attr])*
    $vis struct $name { $($field)* __non_exhaustive: () }
};
(
    found_exhaustive_attr: true,
    body: { $(#[$attr:meta])* $vis:vis $name:ident { $($field:tt)* } }
) => {
    $(#[$attr])*
    $vis struct $name { $($field)* }
};";

/// A struct a macro wrote from fragments is one item to the derive: libc's `stat` on Apple
/// (`pub st_uid: crate::uid_t` among its fields), through a real expansion of libc's own arms,
/// a `cfg_attr` and a field's `cfg` among its fragment metas. CONTROL: tempo `blaze check`
/// (2026-11-21) refused 336 derives "of `::core::marker::Copy` on `stat` in
/// vendor/libc-0.2.191/src/unix/bsd/apple/mod.rs": "an item does not parse: it cuts an
/// invisible group". extent.rs took a fragment's `Open(Invisible)` into every node starting at
/// its first word, so the path `crate` of the type `(~ crate::uid_t ~)` held the group's start
/// and not its end.
#[test]
fn a_struct_written_from_fragments_is_one_item() {
    let input = tokens(
        "#[cfg_attr(unix, repr(align(8)))] pub stat { pub st_dev: dev_t, #[cfg(unix)] pub st_uid: crate::uid_t, \
         #[cfg(windows)] pub st_gone: crate::gone_t, pub st_qspare: [i64; 2], }",
    );
    let written = expand_rules(LIBC_FIELDS, 1, &input);
    let item = expand_rules(LIBC_EMIT_STRUCT_DEFINITION, 1, &args_of(&written, "emit_struct_definition"));
    let invisible = item.tokens().iter().filter(|t| t.kind == TokenKind::Open(Delim::Invisible)).count();
    assert!(invisible >= 4, "the item is written from fragments: {}", text(&item, 0..item.len()));
    let mut unix = |name: &str, _: Option<&str>| Ok::<bool, ()>(name == "unix");
    let derives = crate::ItemAttrs::new(item)
        .derives(&mut unix)
        .expect("the item's attributes apply")
        .expect("the item is configured in");
    assert_eq!(derives.uses.len(), 3, "Clone, Copy and Debug");
    assert_eq!(derives.facts(&derives.uses[0]).copy, crate::AlsoCopy::Yes);
    let input = text(&derives.input, 0..derives.input.len());
    assert!(input.contains("repr ( align ( 8 ) )"), "the `cfg_attr` applied: {input}");
    assert!(!input.contains("cfg") && !input.contains("st_gone"), "the `cfg`s applied: {input}");
    let adt = adt::read(&derives.input, alloc()).expect("the struct reads");
    let tys: std::vec::Vec<_> = adt.fields.iter().map(|f| text(&derives.input, f.ty.clone())).collect();
    assert_eq!(tys, ["~ dev_t ~", "~ crate : : uid_t ~", "~ [ i64 ; 2 ] ~"]);
    let names: std::vec::Vec<_> = adt.fields.iter().map(|f| derives.input.text_at(f.name.expect("a named field"))).collect();
    assert_eq!(names, ["st_dev", "st_uid", "st_qspare"]);
    std::println!("extent: libc's `stat` written from fragments derives Copy; its fields' types are their fragments whole");
}

/// stdarch `static_assert_uimm_bits!` and `static_assert!` (crates/core_arch/src/macros.rs),
/// verbatim.
const STATIC_ASSERT_UIMM_BITS: &str = "($imm:ident, $bits:expr) => {
    #[allow(unused_comparisons)]
    {
        static_assert!(
            0 <= $imm && $imm < (1 << $bits),
            concat!(stringify!($imm), \" doesn't fit in \", stringify!($bits), \" bits (unsigned)\",)
        );
    }
};";

const STATIC_ASSERT: &str = "($e:expr) => { const { assert!($e); } }; ($e:expr, $msg:expr) => { const { assert!($e, $msg); } };";

/// core's `panic_2021` (library/core/src/panic.rs), its arms as `macro_rules!` writes them.
const PANIC_2021: &str = "() => ($crate::panicking::panic_default_msg());
    (\"{}\", $arg:expr $(,)?) => ({ $crate::panicking::panic_display(&$arg); });
    ($($t:tt)+) => ({ $crate::panicking::panic_fmt($crate::const_format_args!($($t)+)); });";

/// An `assert!` message passed as an `$msg:expr` fragment is the template of
/// `const_format_args!`: stdarch's `static_assert_uimm_bits!(IMM8, 8)` through real expansions
/// of stdarch's, blaze's `assert!` and core's `panic_2021` to a `const_format_args!` whose
/// first argument is `(~ concat!(..) ~)`, a `$t:tt` that carried the group whole; the template
/// is a nested call to expand first. CONTROL: tempo `blaze check` (2026-11-21) refused 110
/// tests at `vxarq_u64` and the rest: "path: \"$crate :: const_format_args\", why: \"its
/// expansion's error Builtin: `an argument is not an expression`\"": the path `concat` took the
/// group's `Open(Invisible)` and not its end. The negative twin: a template that is no
/// expression is refused with those words.
#[test]
fn an_assert_message_fragment_is_the_template() {
    let uimm = expand_rules(STATIC_ASSERT_UIMM_BITS, 3, &tokens("IMM8, 8"));
    let assert = expand_rules(STATIC_ASSERT, 3, &args_of(&uimm, "static_assert"));
    let assert_def = crate::MacroDefinition::builtin_fn(crate::BuiltinFn::Assert, def_at(2), alloc());
    let panic = match call(&assert_def, &args_of(&assert, "assert")) {
        Ok(crate::Expanded::Expansion(expansion)) => {
            assert!(expansion.diagnostics.is_empty(), "`assert!` reads its condition and message: {:?}", expansion.diagnostics);
            expansion.tokens
        }
        other => panic!("`assert!` expands at once: {:?}", other.err()),
    };
    let format = expand_rules(PANIC_2021, 2, &args_of(&panic, "panic_2021"));
    let template = args_of(&format, "const_format_args");
    assert!(
        template.tokens().first().is_some_and(|t| t.kind == TokenKind::Open(Delim::Invisible)),
        "the template is the message's fragment: {}",
        text(&template, 0..template.len())
    );
    let format_def = crate::MacroDefinition::builtin_fn(crate::BuiltinFn::ConstFormatArgs, def_at(2), alloc());
    match call(&format_def, &template) {
        Ok(crate::Expanded::NeedsExpanded(nested)) => {
            assert_eq!(nested.len(), 1, "the template's `concat!`, first");
            assert_eq!(template.text_at(nested[0].path.start as usize), "concat");
        }
        Ok(crate::Expanded::Expansion(expansion)) => panic!("the template must be expanded first: {:?}", expansion.diagnostics),
        Err(error) => panic!("`const_format_args!` does not expand: {error:?}"),
    }
    // The negative twin: no expression before the comma.
    let refused = expand_rules(PANIC_2021, 2, &tokens(", x"));
    match call(&format_def, &args_of(&refused, "const_format_args")) {
        Ok(crate::Expanded::Expansion(expansion)) => match expansion.diagnostics.as_slice() {
            [crate::Diagnostic::Builtin { message, .. }] => {
                assert_eq!(expansion.note_text(*message), "an argument is not an expression");
            }
            other => panic!("a template that is no expression must be refused: {other:?}"),
        },
        other => panic!("a refusal is the expansion's diagnostic: {:?}", other.err()),
    }
    std::println!("extent: stdarch's static_assert message reaches const_format_args as its template, a nested concat! first");
}

/// `range` of `stream` as [`text`] writes it, without the invisible delimiters.
fn plain(stream: &TokenStream<'_>, range: Range<usize>) -> std::string::String {
    let all = text(stream, range);
    let words: std::vec::Vec<&str> = all.split(' ').filter(|word| *word != "~").collect();
    words.join(" ")
}

/// The first node of `kind` in the item that is all of `stream`: its child nodes (not its
/// tokens), each one as its kind and its tokens as [`plain`] writes them (`PAREN_EXPR ( 1 )`).
fn child_nodes(stream: &TokenStream<'_>, kind: SyntaxKind) -> std::vec::Vec<std::string::String> {
    let all = || text(stream, 0..stream.len());
    let tree = crate::extent::item(stream, alloc()).unwrap_or_else(|error| panic!("the item reads: {error:?} in {}", all()));
    let node = (0..tree.len())
        .find(|&node| !tree.is_token(node) && tree.kind(node) == kind)
        .unwrap_or_else(|| panic!("no {kind:?} in {}", all()));
    tree.children(node)
        .filter(|&child| !tree.is_token(child))
        .map(|child| std::format!("{:?} {}", tree.kind(child), plain(stream, tree.range(child))))
        .collect()
}

/// The derive input of the one item `rules` write for `args`.
fn derive_input(rules: &str, args: &str) -> TokenStream<'static> {
    let item = expand_rules(rules, 1, &tokens(args));
    let mut none = |_: &str, _: Option<&str>| Ok::<bool, ()>(false);
    let derives = crate::ItemAttrs::new(item)
        .derives(&mut none)
        .expect("the item's attributes apply")
        .expect("the item has no `cfg`");
    derives.input
}

/// An `expr` fragment is one operand where it is put, read as rustc reads it (an invisible
/// group parsed whole): `2 * $e` of `1 + 2` is `2 * (1 + 2)` in a derive's input, `-$e` is
/// `-(1 + 2)`, `$e.method()` of `a + b` is `(a + b).method()`. The one mechanism is the
/// transcriber's parentheses (rules/transcribe.rs `var`, `Fragment::Expr`), which the body
/// lowering reads through the same words. CONTROL (by reading, not run): without them the
/// output is `2 * ~1 + 2~`, the parser reads `2 * 1` as a node that starts out of the group
/// and ends inside it, and the derive is refused as `Unbalanced` naming the field; this
/// file's earlier twin pinned that refusal of a valid program and is deleted. The twins: a
/// leaf, a group and a negative literal stay as they are, unparenthesized.
#[test]
fn an_expression_fragment_is_one_operand() {
    let input = derive_input("($e:expr) => { #[derive(Clone)] struct S { f: [u8; 2 * $e] } };", "1 + 2");
    assert_eq!(child_nodes(&input, SyntaxKind::BIN_EXPR), ["LITERAL 2", "PAREN_EXPR ( 1 + 2 )"]);
    let negated = expand_rules("($e:expr) => { const C: i32 = -$e; };", 1, &tokens("1 + 2"));
    assert_eq!(child_nodes(&negated, SyntaxKind::PREFIX_EXPR), ["PAREN_EXPR ( 1 + 2 )"]);
    let called = expand_rules("($e:expr) => { fn f() { $e.method(); } };", 1, &tokens("a + b"));
    let receiver = child_nodes(&called, SyntaxKind::METHOD_CALL_EXPR);
    assert_eq!(receiver.first().map(|node| node.as_str()), Some("PAREN_EXPR ( a + b )"), "{receiver:?}");
    // The twins: nothing to keep apart, nothing added.
    let leaf = expand_rules("($e:expr) => { const C: i32 = -$e; };", 1, &tokens("x"));
    assert_eq!(child_nodes(&leaf, SyntaxKind::PREFIX_EXPR), ["PATH_EXPR x"]);
    let group = expand_rules("($e:expr) => { const C: i32 = -$e; };", 1, &tokens("(1 + 2)"));
    assert_eq!(child_nodes(&group, SyntaxKind::PREFIX_EXPR), ["PAREN_EXPR ( 1 + 2 )"]);
    let negative = expand_rules("($e:expr) => { const C: i32 = 2 * $e; };", 1, &tokens("-1"));
    assert_eq!(child_nodes(&negative, SyntaxKind::BIN_EXPR), ["LITERAL 2", "PREFIX_EXPR - 1"]);
    std::println!("extent: `2 * $e` of `1 + 2` is `2 * (1 + 2)`, `-$e` and `$e.method()` take the fragment whole");
}

/// A `ty` fragment of bounds is one type where it is put: `&$t` of `A + B` is `&(A + B)`, not
/// `(&A) + B` (rules/input.rs `Joins`, the transcriber's parentheses as for an expression).
/// `A + B` is a bare trait object (the 2015 edition's; `dyn A + B` is the same tree). CONTROL
/// (by reading, not run): before `Joins`, a `ty` fragment was written in an invisible group
/// alone, `&~A + B~`; a reference's type takes no bounds (grammar/types.rs `ref_type`,
/// `type_no_bounds`), so the field's type `&A` ended inside the group and the derive was
/// refused. The twin: a type already in parentheses gets none added.
#[test]
fn a_type_of_bounds_is_one_operand() {
    let rules = "($t:ty) => { #[derive(Clone)] struct S<'a> { f: &'a $t } };";
    let bare = derive_input(rules, "A + B");
    let pointee = child_nodes(&bare, SyntaxKind::REF_TYPE);
    assert_eq!(pointee.last().map(|node| node.as_str()), Some("PAREN_TYPE ( A + B )"), "{pointee:?}");
    let dyn_ = derive_input(rules, "dyn A + Send");
    let pointee = child_nodes(&dyn_, SyntaxKind::REF_TYPE);
    assert_eq!(pointee.last().map(|node| node.as_str()), Some("PAREN_TYPE ( dyn A + Send )"), "{pointee:?}");
    assert_eq!(child_nodes(&dyn_, SyntaxKind::PAREN_TYPE), ["DYN_TRAIT_TYPE dyn A + Send"]);
    // The twin: one pair of parentheses, the call's own.
    let written = derive_input(rules, "(dyn A + Send)");
    assert_eq!(child_nodes(&written, SyntaxKind::PAREN_TYPE), ["DYN_TRAIT_TYPE dyn A + Send"]);
    std::println!("extent: `&$t` of `A + B` is `&(A + B)`");
}

/// A `pat` fragment that is an or-pattern is one pattern where it is put: in the tuple pattern
/// `(&$p, $p)` of `A | B` the first element is `&(A | B)`, not `(&A) | B`. CONTROL (by
/// reading, not run): a `pat` fragment was copied as it is, `(&A | B, A | B)`, the first
/// element the or-pattern of `&A` and `B`, which a body's lowering bound differently from
/// rustc. The twin: a single pattern gets no parentheses.
#[test]
fn an_or_pattern_fragment_is_one_pattern() {
    let rules = "($p:pat) => { fn f(t: (E, E)) { match t { (&$p, $p) => {} _ => {} } } };";
    let written = expand_rules(rules, 1, &tokens("A | B"));
    assert_eq!(child_nodes(&written, SyntaxKind::TUPLE_PAT), ["REF_PAT & ( A | B )", "PAREN_PAT ( A | B )"]);
    assert_eq!(child_nodes(&written, SyntaxKind::REF_PAT), ["PAREN_PAT ( A | B )"]);
    // The twin.
    let single = expand_rules(rules, 1, &tokens("A"));
    assert_eq!(child_nodes(&single, SyntaxKind::TUPLE_PAT), ["REF_PAT & A", "IDENT_PAT A"]);
    std::println!("extent: `&$p` of `A | B` is `&(A | B)`");
}
