//! Const macros (docs/future/const-macros.md, stage 0): a derive whose body is a function of a
//! macro crate, run by the const evaluator, generating an impl the using crate evaluates.

use std::sync::Arc;

use hir_expand::{
    EditionedFileId,
    proc_macro::{ProcMacro, ProcMacroKind},
};
use intern::Symbol;
use test_fixture::ChangeFixture;

use super::{eval_goal, pretty_print_err};
use crate::{
    const_macro::ConstMacroExpander, mir_pretty::MirEvalErrorPretty as _, test_db::TestDB,
};

/// What a macro crate's derive writes its output with: a `String` that is the three words the
/// standard library's is, made with `const_allocate`, the nightly's const heap.
const STRING: &str = r#"
#[rustc_intrinsic]
const unsafe fn const_allocate(size: usize, align: usize) -> *mut u8;

pub struct String { ptr: *mut u8, cap: usize, len: usize }

#[repr(C)]
struct Raw { ptr: *const u8, len: usize }

const fn raw(text: &str) -> Raw {
    unsafe { core::mem::transmute(text) }
}

const fn at(text: &Raw, i: usize) -> u8 {
    if i < text.len { unsafe { *((text.ptr as usize + i) as *const u8) } } else { 0 }
}

const fn push(out: &mut String, byte: u8) {
    unsafe { *((out.ptr as usize + out.len) as *mut u8) = byte };
    out.len += 1;
}

const fn push_str(out: &mut String, text: &str) {
    let text = raw(text);
    let mut i = 0;
    while i < text.len {
        push(out, at(&text, i));
        i += 1;
    }
}

const fn is_ident(byte: u8) -> bool {
    byte == b'_' || (byte >= b'a' && byte <= b'z') || (byte >= b'A' && byte <= b'Z')
        || (byte >= b'0' && byte <= b'9')
}
"#;

/// The derive `Named` of stage 0, which reads the name of the struct it is given as text and
/// implements `Named` for it with `LEN`, that name's length.
const NAMED: &str = r#"
#[proc_macro_derive(Named)]
pub const fn named(input: &str) -> String {
    let input = raw(input);
    let mut i = 0;
    while !(at(&input, i) == b's' && at(&input, i + 1) == b't' && at(&input, i + 2) == b'r'
        && at(&input, i + 3) == b'u' && at(&input, i + 4) == b'c' && at(&input, i + 5) == b't'
        && !is_ident(at(&input, i + 6)))
    {
        i += 1;
    }
    i += 6;
    while at(&input, i) == b' ' {
        i += 1;
    }
    let start = i;
    while is_ident(at(&input, i)) {
        i += 1;
    }
    let mut out = String { ptr: unsafe { const_allocate(256, 1) }, cap: 256, len: 0 };
    push_str(&mut out, "impl Named for ");
    let mut j = start;
    while j < i {
        push(&mut out, at(&input, j));
        j += 1;
    }
    push_str(&mut out, " { const LEN: usize = ");
    push(&mut out, b'0' + (i - start) as u8);
    push_str(&mut out, "; }");
    out
}
"#;

#[test]
fn a_const_derive_generates_an_impl() {
    let fixture = format!(
        "//- minicore: sized, copy, derive, transmute
//- /macros.rs crate:proc-macro:macros
{STRING}
{NAMED}
//- /main.rs crate:main deps:macros
pub trait Named {{ const LEN: usize; }}

#[derive(macros::Named)]
pub struct Point {{ x: i32, y: i32 }}

const GOAL: usize = <Point as Named>::LEN;
"
    );
    let (db, file_id, root) = load(&fixture, "Named");
    crate::attach_db(&db, || {
        let (krate, func) = expander_of(root, "Named").function(&db).expect("the macro's function");
        let input = crate::mir::MacroArg::Str("pub struct Point { x : i32 , y : i32 }");
        let output = crate::mir::call_const_macro(&db, func, &[input]).unwrap_or_else(|e| {
            let mut text = String::new();
            let target = crate::display::DisplayTarget::from_crate(&db, krate);
            _ = e.pretty_print(&mut text, &db, |file, range| format!("{file:?} {range:?}"), target);
            panic!("calling the macro: {text}");
        });
        assert_eq!(output, "impl Named for Point { const LEN: usize = 5; }");
        let goal = eval_goal(&db, file_id).unwrap_or_else(|e| {
            let main = *base_db::all_crates(&db).last().expect("the main crate");
            panic!("evaluating GOAL: {}\n{}", pretty_print_err(e, &db), derives(&db, main));
        });
        assert_eq!(goal.memory[..], 5usize.to_le_bytes()[..goal.memory.len()]);
    });
}

/// A fixture whose macro crate depends on `const_ast` and defines `macros`, and whose crate
/// `main` is `main`. `const_ast`'s files are blaze's to the byte, each ending in a newline.
fn with_const_ast(macros: &str, main: &str) -> String {
    let [(_, lib), (_, kinds), (_, views)] = crate::const_macro::CONST_AST;
    format!(
        "//- minicore: sized, copy, clone, derive, option, result, slice, index, coerce_unsized, str, iterator, panic, transmute
//- /const_ast/lib.rs crate:const_ast
{lib}//- /const_ast/kinds.rs
{kinds}//- /const_ast/views.rs
{views}//- /macros.rs crate:proc-macro:macros deps:const_ast
{STRING}
{macros}
//- /main.rs crate:main deps:macros
{main}"
    )
}

/// The derive `Fields` of stage 2, given the item as a flat tree: it implements `Fields` with
/// the struct's name, how many fields it has, and whether the name's span is one of the input's
/// (after the three sites in the span table).
const FIELDS: &str = r#"
#[proc_macro_derive(Fields)]
pub const fn fields(item: &const_ast::Item) -> String {
    let mut out = String { ptr: unsafe { const_allocate(256, 1) }, cap: 256, len: 0 };
    let (name, count) = match item.as_struct() {
        Some(adt) => (
            adt.name(),
            match adt.field_list() {
                Some(const_ast::FieldList::RecordFieldList(list)) => list.fields().count(),
                _ => 0,
            },
        ),
        None => (None, 0),
    };
    let (text, span) = match name {
        Some(name) => (
            name.text(),
            match name.token() {
                Some(token) => token.span(),
                None => 0,
            },
        ),
        None => ("", 0),
    };
    push_str(&mut out, "impl Fields for ");
    push_str(&mut out, text);
    push_str(&mut out, " { const COUNT: usize = ");
    push(&mut out, b'0' + count as u8);
    push_str(&mut out, "; const NAME_LEN: usize = ");
    push(&mut out, b'0' + raw(text).len as u8);
    push_str(&mut out, "; const INPUT_SPAN: bool = ");
    push_str(&mut out, if span >= 3 && span != const_ast::NO_SPAN { "true" } else { "false" });
    push_str(&mut out, "; }");
    out
}
"#;

#[test]
fn a_const_derive_reads_the_item_through_the_views() {
    let fixture = with_const_ast(
        FIELDS,
        "pub trait Fields { const COUNT: usize; const NAME_LEN: usize; const INPUT_SPAN: bool; }

#[derive(macros::Fields)]
pub struct Point {
    x: i32,
    #[allow(unused)]
    pub y: i32,
    pub(crate) z: [u8; 4],
}

const GOAL: usize = <Point as Fields>::COUNT * 100
    + <Point as Fields>::NAME_LEN * 10
    + <Point as Fields>::INPUT_SPAN as usize;
",
    );
    assert_eq!(goal(&fixture, "Fields")[..], 351usize.to_le_bytes()[..]);
}

#[test]
fn a_const_macro_takes_str_or_item() {
    let fixture = with_const_ast(
        "#[proc_macro_derive(Fields)]
pub const fn fields(item: &u8) -> String {
    String { ptr: 0 as *mut u8, cap: 0, len: 0 }
}",
        "pub trait Fields {}

#[derive(macros::Fields)]
pub struct Point;
",
    );
    let (db, _, _) = load(&fixture, "Fields");
    crate::attach_db(&db, || {
        let main = *base_db::all_crates(&db).last().expect("the main crate");
        let derives = derives(&db, main);
        assert!(
            derives.contains(
                "a parameter of the const macro `Fields` is `&u8`; it takes `&str` or \
                 `&const_ast::Item`"
            ),
            "{derives}"
        );
    });
}

/// A database of `fixture`, whose macro crate has the derive `name`, the file of its last
/// crate, and the macro crate's root.
fn load(fixture: &str, name: &str) -> (TestDB, EditionedFileId, span::FileId) {
    let mut fixture = ChangeFixture::parse(fixture);
    let graph = fixture.change.source_change.crate_graph.as_ref().expect("a crate graph");
    let macros = graph
        .iter()
        .find(|&id| graph[id].basic.target_kind.is_proc_macro())
        .expect("the macro crate");
    let root = graph[macros].basic.root_file_id;
    let expander = expander_of(root, name);
    fixture.change.proc_macros.get_or_insert_default().insert(
        macros,
        Ok(vec![ProcMacro {
            name: expander.name.clone(),
            kind: ProcMacroKind::CustomDerive,
            expander: Arc::new(expander),
            disabled: false,
        }]),
    );
    let file_id = *fixture.files.last().expect("the last crate's file");
    let mut db = TestDB::default();
    fixture.change.apply(&mut db);
    let file_id = EditionedFileId::from_span_file_id(&db, file_id);
    crate::const_macro::enable(&db);
    (db, file_id, root)
}

/// `GOAL` of the last file of `fixture`, whose macro crate has the derive `name`.
fn goal(fixture: &str, name: &str) -> Vec<u8> {
    let (db, file_id, _) = load(fixture, name);
    crate::attach_db(&db, || match eval_goal(&db, file_id) {
        Ok(goal) => goal.memory.to_vec(),
        Err(e) => {
            let main = *base_db::all_crates(&db).last().expect("the main crate");
            panic!("evaluating GOAL: {}\n{}", pretty_print_err(e, &db), derives(&db, main));
        }
    })
}

fn expander_of(root: span::FileId, name: &str) -> ConstMacroExpander {
    ConstMacroExpander { root, name: Symbol::intern(name), kind: ProcMacroKind::CustomDerive }
}

/// What the derives of `krate` expanded to, and why they did not, for a failure to say.
fn derives(db: &TestDB, krate: base_db::Crate) -> String {
    let def_map = hir_def::nameres::crate_def_map(db, krate);
    let mut out = format!("def map diagnostics: {:?}\n", def_map.diagnostics());
    for (_, module) in def_map.modules(db) {
        for (_, invocations) in module.scope(db).derive_macro_invocs() {
            for (_, _, calls) in invocations {
                for call in calls.iter().flatten() {
                    let either::Either::Left(call) = call else { continue };
                    let expansion = call.parse_macro_expansion(db);
                    out += &format!("derive expanded to: {}\n", expansion.value.0.syntax_node());
                    if let Some(err) = &expansion.err {
                        out += &format!("derive failed: {}\n", err.render_to_string(db).message);
                    }
                }
            }
        }
    }
    out
}
