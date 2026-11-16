//! The lowering on the ladder's programs (crates/blaze-cli/tests/ladder/*/src): each lowers, and
//! every body checks with no diagnostic; a construct outside the subset is a typed error.

use std::alloc::Global;
use std::boxed::Box;
use std::string::String;
use std::vec::Vec;

use blaze_own::Alloc;
use blaze_typeck::toy::ToyCtx;
use blaze_typeck::{Needs, Outcome, PNode};

use crate::lower::{self, Abi, LowerError, Program, SourceFile};

/// A rung's files, read from disk: `(path, text)` with paths relative to the package root.
fn rung(name: &str) -> Vec<(String, String), Global> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../blaze-cli/tests/ladder").join(name);
    let mut files = Vec::new_in(Global);
    let mut stack = std::vec![dir.join("src")];
    while let Some(d) = stack.pop() {
        let entries = std::fs::read_dir(&d).unwrap_or_else(|e| panic!("reading {}: {e}", d.display()));
        for entry in entries {
            let path = entry.expect("a directory entry").path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|x| x == "rs") {
                let rel = path.strip_prefix(&dir).expect("under the rung").to_string_lossy().into_owned();
                let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {rel}: {e}"));
                files.push((rel, text));
            }
        }
    }
    files.sort();
    files
}

fn lower_rung(name: &str) -> Program {
    let owned = rung(name);
    let files: Vec<SourceFile<'_>, Global> = {
        let mut v = Vec::new_in(Global);
        v.extend(owned.iter().map(|(p, t)| SourceFile { path: p, text: t }));
        v
    };
    match lower::lower(&files, "src/main.rs") {
        Ok(p) => p,
        Err(e) => panic!("{name}: {}", e.located(&files)),
    }
}

/// Lowers rung `name` and checks every body with no diagnostic.
fn check_rung(name: &str) -> Program {
    let program = lower_rung(name);
    for (i, f) in program.fns.iter().enumerate() {
        let Some(body) = f.body.as_ref() else { continue };
        let world = program.world(i);
        let out = blaze_typeck::check(
            &body.view(f.key),
            &world,
            &mut ToyCtx::new(),
            &Needs::new_in(Alloc::global()),
            Alloc::global(),
        );
        match out {
            Ok(Outcome::Done(c)) => assert!(c.diags.is_empty(), "{name}: `{}`: diagnostics {:?}", f.path, c.diags),
            Ok(Outcome::Need(n)) => panic!("{name}: `{}`: the world missed {:?}", f.path, n.questions),
            Err(e) => panic!("{name}: `{}`: {e:?}", f.path),
        }
    }
    program
}

fn output_is(program: &Program, f: usize, want: PNode) {
    let sig = program.sig(f);
    assert_eq!(sig.types.nodes.get(sig.output.0 as usize), Some(&want));
}

#[test]
fn empty() {
    let p = check_rung("1-empty");
    let main = p.entry().expect("a main");
    assert_eq!(p.fns[main].abi, Abi::Rust);
    output_is(&p, main, PNode::Tuple(blaze_typeck::List::EMPTY));
}

#[test]
fn exit() {
    let p = check_rung("2-exit");
    assert!(p.no_core && p.no_main);
    let main = p.entry().expect("a main");
    assert_eq!(p.fns[main].abi, Abi::C);
    assert!(p.fns[main].no_mangle);
    output_is(&p, main, PNode::Int(blaze_typeck::IntTy::I32));
    assert_eq!(p.fns.len(), 3);
}

#[test]
fn hello() {
    let p = check_rung("3-hello");
    let write = p.fns.iter().find(|f| f.path.as_ref() == "write").expect("the extern `write`");
    assert!(write.body.is_none() && write.is_unsafe && write.abi == Abi::C);
    assert_eq!(write.symbol(), Box::<str, Global>::from("write"));
    let main = &p.fns[p.entry().expect("a main")];
    let body = main.body.as_ref().expect("a body");
    let view = body.view(main.key);
    let strs: Vec<&[u8], Global> = {
        let mut v = Vec::new_in(Global);
        v.extend((0..view.exprs.len() as u32).filter_map(|e| body.byte_str(blaze_typeck::ExprId(e))));
        v
    };
    assert_eq!(strs, [b"hello\n".as_slice()]);
}

#[test]
fn modules() {
    let p = check_rung("4-modules");
    assert!(p.fns.iter().any(|f| f.path.as_ref() == "util::double"));
    assert!(p.fns.iter().any(|f| f.path.as_ref() == "util::add"));
    assert_eq!(p.structs.len(), 1);
    assert!(p.adt(p.structs[0].key).is_some());
    // `classify`'s arm patterns carry their values.
    let c = p.fns.iter().find(|f| f.path.as_ref() == "classify").expect("classify");
    let body = c.body.as_ref().expect("a body");
    let view = body.view(c.key);
    let mut values = Vec::new_in(Global);
    values.extend((0..view.pats.len() as u32).filter_map(|i| body.pat_int_value(blaze_typeck::PatId(i))));
    assert_eq!(values, [0, 1, 2]);
}

#[test]
fn generics() {
    let p = check_rung("5-generics");
    let f = |path: &str| p.fns.iter().position(|f| f.path.as_ref() == path).expect(path);
    for (path, n) in [("pick", 1), ("second", 1), ("pick_second", 1), ("main", 0)] {
        assert_eq!(p.sig(f(path)).own_count, n, "{path}");
    }
    // `pick<T>(c: bool, a: T, b: T) -> T`.
    let sig = p.sig(f("pick"));
    let node = |t: blaze_typeck::PTy| sig.types.nodes[t.0 as usize];
    assert_eq!(node(sig.inputs[1]), PNode::Param(0));
    assert_eq!(node(sig.output), PNode::Param(0));
    let pair = p.adt(p.structs[0].key).expect("Pair");
    assert_eq!(pair.generics, 1);
    assert!(pair.fields.iter().all(|fl| pair.types.nodes[fl.ty.0 as usize] == PNode::Param(0)));
}

#[test]
fn written_type_arguments_check() {
    let text = "struct P<T> { a: T }\nfn id<T>(x: T) -> T { x }\nfn get<U>(p: P<U>) -> U { p.a }\n\
                fn main() { let p: P<u8> = P::<u8> { a: id::<u8>(1) }; let _y: u8 = get(p); }";
    let files = [SourceFile { path: "src/main.rs", text }];
    let p = lower::lower(&files, "src/main.rs").unwrap_or_else(|e| panic!("{}", e.located(&files)));
    for (i, f) in p.fns.iter().enumerate() {
        let body = f.body.as_ref().expect("a body");
        let out = blaze_typeck::check(
            &body.view(f.key),
            &p.world(i),
            &mut ToyCtx::new(),
            &Needs::new_in(Alloc::global()),
            Alloc::global(),
        );
        match out {
            Ok(Outcome::Done(c)) => assert!(c.diags.is_empty(), "`{}`: diagnostics {:?}", f.path, c.diags),
            Ok(Outcome::Need(n)) => panic!("`{}`: the world missed {:?}", f.path, n.questions),
            Err(e) => panic!("`{}`: {e:?}", f.path),
        }
    }
}

#[test]
fn generics_outside_the_subset_are_named() {
    let refused = |text: &str| {
        let files = [SourceFile { path: "src/main.rs", text }];
        lower::lower(&files, "src/main.rs").err().expect("refused")
    };
    let what = |text: &str| match refused(text) {
        LowerError::Unsupported { what, .. } => what,
        other => panic!("{other:?}"),
    };
    assert_eq!(what("fn f<'a>() {}"), "a lifetime parameter");
    assert_eq!(what("fn f<const N: usize>() {}"), "a const generic parameter");
    assert_eq!(what("trait C {}\nfn f<T: C>() {}"), "a trait bound");
    assert_eq!(what("fn f<T>() where T: Sized {}"), "a `where` clause");
    let err = refused("struct P<T> { a: T }\nfn f(p: P) {}");
    assert!(matches!(err, LowerError::GenericArgCount { want: 1, got: 0, .. }), "{err:?}");
    let err = refused("fn f<T, T>() {}");
    assert!(matches!(err, LowerError::DuplicateParam { ref name, .. } if name.as_ref() == "T"), "{err:?}");
}

#[test]
fn a_closure_is_a_typed_error() {
    let text = "fn main() { let f = |x: i32| x; }";
    let files = [SourceFile { path: "src/main.rs", text }];
    let err = lower::lower(&files, "src/main.rs").err().expect("refused");
    assert_eq!(err, LowerError::Unsupported { file: 0, at: 20, what: "a closure" });
    let shown = std::format!("{}", err.located(&files));
    assert_eq!(shown, "src/main.rs:1:21: a closure is outside the ladder's subset");
}

#[test]
fn a_missing_module_file_is_named() {
    let files = [SourceFile { path: "src/main.rs", text: "mod gone;\nfn main() {}" }];
    let err = lower::lower(&files, "src/main.rs").err().expect("refused");
    assert!(matches!(err, LowerError::NoModuleFile { file: 0, at: 0, ref name } if name.as_ref() == "gone"), "{err:?}");
}
