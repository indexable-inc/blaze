use hir_expand::fixture;

use crate::{db::HirDatabase, setup_tracing, test_db::TestDBExt as _};

fn lower_mir(#[rust_analyzer::rust_fixture] ra_fixture: &str) {
    let _tracing = setup_tracing();
    let fx = fixture::db(ra_fixture);
    let file_ids = fx.files().to_vec();
    let db = fx.into_db();
    crate::attach_db(&db, || {
        let file_id = *file_ids.last().unwrap();
        let module_id = db.module_for_file(file_id.file_id(&db));
        let def_map = module_id.def_map(&db);
        let scope = &def_map.module(&db, module_id).scope(&db);
        let funcs = scope.declarations().filter_map(|x| match x {
            hir_def::ModuleDefId::FunctionId(it) => Some(it),
            _ => None,
        });
        for func in funcs {
            _ = db.mir_body(func.into());
        }
    })
}

/// Lowers every function of the fixture, each of which has to lower.
fn lower_mir_all(#[rust_analyzer::rust_fixture] ra_fixture: &str) {
    let _tracing = setup_tracing();
    let fx = fixture::db(ra_fixture);
    let file_ids = fx.files().to_vec();
    let db = fx.into_db();
    crate::attach_db(&db, || {
        let file_id = *file_ids.last().unwrap();
        let module_id = db.module_for_file(file_id.file_id(&db));
        let def_map = module_id.def_map(&db);
        let scope = &def_map.module(&db, module_id).scope(&db);
        let funcs = scope.declarations().filter_map(|x| match x {
            hir_def::ModuleDefId::FunctionId(it) => Some(it),
            _ => None,
        });
        for func in funcs {
            if let Err(e) = db.mir_body(func.into()) {
                panic!("{func:?} does not lower: {e:?}");
            }
        }
    })
}

#[test]
fn builtin_macro_last_in_a_block() {
    // A builtin's expansion is rust-analyzer's own (`builtin #asm (..)`), not the statements
    // rustc parses: a call of one in parentheses at a block's end is not refused (#154).
    lower_mir_all(
        r#"
//- minicore: asm
fn f() {
    unsafe { core::arch::asm!("nop") }
}
"#,
    );
}

#[test]
fn dyn_projection_with_auto_traits_regression_next_solver() {
    lower_mir(
        r#"
//- minicore: sized, send
pub trait Deserializer {}

pub trait Strictest {
    type Object: ?Sized;
}

impl Strictest for dyn CustomValue {
    type Object = dyn CustomValue + Send;
}

pub trait CustomValue: Send {}

impl CustomValue for () {}

struct Box<T: ?Sized>;

type DeserializeFn<T> = fn(&mut dyn Deserializer) -> Box<T>;

fn foo() {
    (|deserializer| Box::new(())) as DeserializeFn<<dyn CustomValue as Strictest>::Object>;
}
    "#,
    );
}

#[test]
fn ref_pattern_on_unresolved_alias() {
    lower_mir(
        r#"
//- minicore: sized
trait Tr {
    type A;
}

fn f<T: Tr>(x: T::A) {
    let &() = x;
}
"#,
    );
}

#[test]
fn a_chain_of_calls_a_thousand_deep_lowers_on_a_small_stack() {
    // Each receiver is a place inside the next (`&mut *b.f()`), as in the builder chains of
    // build scripts, and a codegen thread has the default stack of 2 MiB.
    let chain = ".f()".repeat(1_000);
    let fixture = format!(
        "struct B;\nimpl B {{ fn f(&mut self) -> &mut B {{ self }} }}\nfn g(b: &mut B) {{ b{chain}; }}\n"
    );
    std::thread::Builder::new()
        .stack_size(2 << 20)
        .spawn(move || lower_mir_all(&fixture))
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn nested_closure_moves_local_of_enclosing_closure() {
    let fx = fixture::db(
        r#"
//- minicore: copy, fn
struct S;
fn main() {
    let outer = S;
    || {
        let local = outer;
        || { let moved = local; };
    };
}
"#,
    );
    let file_id = fx.only_file();
    let db = fx.into_db();
    crate::attach_db(&db, || {
        let module_id = db.module_for_file(file_id.file_id(&db));
        let scope = &module_id.def_map(&db).module(&db, module_id).scope(&db);
        for decl in scope.declarations() {
            if let hir_def::ModuleDefId::FunctionId(func) = decl {
                let mut closures = db.mir_body(func.into()).unwrap().closures.clone();
                while let Some(closure) = closures.pop() {
                    closures.extend(&db.mir_body_for_closure(closure).unwrap().closures);
                }
            }
        }
    })
}

/// Lowers every function of the fixture and every closure in it, each of which has to lower.
fn lower_mir_all_and_closures(#[rust_analyzer::rust_fixture] ra_fixture: &str) {
    let _tracing = setup_tracing();
    let fx = fixture::db(ra_fixture);
    let file_ids = fx.files().to_vec();
    let db = fx.into_db();
    crate::attach_db(&db, || {
        let file_id = *file_ids.last().unwrap();
        let module_id = db.module_for_file(file_id.file_id(&db));
        let scope = &module_id.def_map(&db).module(&db, module_id).scope(&db);
        for decl in scope.declarations() {
            let hir_def::ModuleDefId::FunctionId(func) = decl else { continue };
            let body = db.mir_body(func.into());
            let mut closures = match body {
                Ok(body) => body.closures.clone(),
                Err(e) => panic!("{func:?} does not lower: {e:?}"),
            };
            while let Some(closure) = closures.pop() {
                match db.mir_body_for_closure(closure) {
                    Ok(body) => closures.extend(&body.closures),
                    Err(e) => panic!("a closure of {func:?} does not lower: {e:?}"),
                }
            }
        }
    })
}

/// `proc_macro::bridge`'s `Decode` (rpc.rs), its impls for a handle, a struct, a pair and
/// `Result`, the last two written by a macro as `rpc_encode_decode!` and `compound_traits!`
/// write them, and a function taking a closure as `Bridge::with` does.
const BRIDGE: &str = r#"
//- minicore: sized, fn, result, copy
pub trait Decode<'a, 's, S>: Sized {
    fn decode(r: &mut &'a [u8], s: &'s mut S) -> Self;
}
pub struct Message;
impl<S> Decode<'_, '_, S> for Message {
    fn decode(_: &mut &[u8], _: &mut S) -> Self {
        Message
    }
}
pub struct Globals<Sp> {
    site: Sp,
}
impl<'a, S, A: for<'s> Decode<'a, 's, S>, B: for<'s> Decode<'a, 's, S>> Decode<'a, '_, S>
    for (A, B)
{
    fn decode(r: &mut &'a [u8], s: &mut S) -> Self {
        (Decode::decode(r, s), Decode::decode(r, s))
    }
}
macro_rules! rpc_encode_decode {
    (struct $name:ident <$($T:ident),+> { $($field:ident),* }) => {
        impl<'a, S, $($T: for<'s> Decode<'a, 's, S>),+> Decode<'a, '_, S> for $name<$($T),+> {
            fn decode(r: &mut &'a [u8], s: &mut S) -> Self {
                $name { $($field: Decode::decode(r, s)),* }
            }
        }
    };
    (enum $name:ident <$($T:ident),+> { $($variant:ident($field:ident)),* }) => {
        impl<'a, S, $($T: for<'s> Decode<'a, 's, S>),+> Decode<'a, '_, S> for $name<$($T),+> {
            fn decode(r: &mut &'a [u8], s: &mut S) -> Self {
                match u8::decode(r, s) {
                    0 => $name::Ok(Decode::decode(r, s)),
                    _ => $name::Err(Decode::decode(r, s)),
                }
            }
        }
    };
}
impl<S> Decode<'_, '_, S> for u8 {
    fn decode(_: &mut &[u8], _: &mut S) -> Self {
        0
    }
}
rpc_encode_decode!(struct Globals<Sp> { site });
rpc_encode_decode!(enum Result<T, E> { Ok(x), Err(e) });
pub fn with<R>(f: impl FnOnce(u8) -> R) -> R {
    f(0)
}
"#;

#[test]
fn a_type_relative_call_with_a_placeholder_lowers_in_a_closure() {
    // `define_client_side!`'s `Result::<_, PanicMessage>::decode` in `Bridge::with(|bridge| ..)`;
    // `outside` is the same call with no closure.
    lower_mir_all_and_closures(&format!(
        "{BRIDGE}
pub fn outside(input: &[u8]) -> Message {{
    let mut buf: &[u8] = input;
    let r = Result::<_, Message>::decode(&mut buf, &mut ());
    match r {{
        Ok(it) => it,
        Err(it) => it,
    }}
}}
pub fn inside(input: &[u8]) -> Message {{
    with(|_| {{
        let mut buf: &[u8] = input;
        let r = Result::<_, Message>::decode(&mut buf, &mut ());
        match r {{
            Ok(it) => it,
            Err(it) => it,
        }}
    }})
}}
"
    ));
}

#[test]
fn a_type_relative_call_lowers_in_a_closure_with_named_impl_lifetimes() {
    // `BRIDGE` with each impl's `'_` named: an impl header's elided lifetime is a parameter of
    // the impl, as rustc's `AnonymousCreateParameter` rib makes it, so both lower alike.
    let named = BRIDGE
        .replace("impl<S> Decode<'_, '_, S>", "impl<'x, 'y, S> Decode<'x, 'y, S>")
        .replace("impl<'a, S,", "impl<'a, 'y, S,")
        .replace("Decode<'a, '_, S>", "Decode<'a, 'y, S>");
    assert!(!named.contains("'_"), "every elided impl lifetime is named");
    lower_mir_all_and_closures(&format!(
        "{named}
pub fn inside(input: &[u8]) -> Message {{
    with(|_| {{
        let mut buf: &[u8] = input;
        let r = Result::<_, Message>::decode(&mut buf, &mut ());
        match r {{
            Ok(it) => it,
            Err(it) => it,
        }}
    }})
}}
pub fn anchored<A: for<'a, 's> Decode<'a, 's, ()>>(input: &[u8]) -> A {{
    with(|_| {{
        let mut buf: &[u8] = input;
        let (_globals, it) = <(Globals<Message>, A)>::decode(&mut buf, &mut ());
        it
    }})
}}
"
    ));
}

#[test]
fn a_type_anchored_call_on_a_bound_parameter_lowers_in_a_closure() {
    // `run_client`'s `<(ExpnGlobals<Span>, A)>::decode` in `panic::catch_unwind(|| ..)`, `A`
    // bound by `for<'a, 's> Decode<'a, 's, ()>`; `outside` is the same call with no closure.
    lower_mir_all_and_closures(&format!(
        "{BRIDGE}
pub fn outside<A: for<'a, 's> Decode<'a, 's, ()>>(input: &[u8]) -> A {{
    let mut buf: &[u8] = input;
    let reader = &mut buf;
    let (_globals, it) = <(Globals<Message>, A)>::decode(reader, &mut ());
    it
}}
pub fn inside<A: for<'a, 's> Decode<'a, 's, ()>>(input: &[u8]) -> A {{
    with(|_| {{
        let mut buf: &[u8] = input;
        let reader = &mut buf;
        let (_globals, it) = <(Globals<Message>, A)>::decode(reader, &mut ());
        it
    }})
}}
"
    ));
}
