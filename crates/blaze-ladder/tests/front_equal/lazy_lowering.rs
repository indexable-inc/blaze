//! Included by make_one's library tests so the old lowering strategy stays test-only.

use core::fmt::Write;
use std::collections::HashSet;

use blaze_build::{OverflowChecks, Panic, UbChecks};
use blaze_emit::{Isa, Os};
use blaze_mem::{Frame, tag::Tag};
use blaze_own::{AVec, Alloc};
use blaze_scope::resolve::Names;
use blaze_typeck::{ItemKey, LangItem, List, PNode, PTy, PTys, PortableTys, Stmt, UintTy};

use crate::code_records::EntryView;
use crate::crates::{Inputs, Toolchain};
use crate::front2::keys::checker_of;
use crate::front2::program::{lazy::LazyProgram, run_entry::root_main};
use crate::program_texts::ProgramTexts;
use crate::refusal::SourceFile;
use crate::run::NewFront;
use crate::run::make_one::{CheckCount, LoweringCounts, MakerState, SecondCheck, make_instance};
use crate::seam::FrontEnd;
use crate::symbols::{decode_instance, encode_instance};

#[derive(Clone, Copy, Debug)]
enum Shape {
    Chain,
    Fan,
}

fn source(shape: Shape, n: usize) -> String {
    let mut text = String::from(r#"#![feature(no_core, lang_items)]
#![no_core]
#![no_main]
#[lang = "pointee_sized"] trait PointeeSized {}
#[lang = "meta_sized"] trait MetaSized: PointeeSized {}
#[lang = "sized"] trait Sized: MetaSized {}
#[lang = "copy"] trait Copy {}
impl Copy for u8 {}
impl Copy for u16 {}
#[unsafe(no_mangle)]
pub extern "C" fn main() -> u32 {
    let pair: (u8, u16) = (1u8, 2u16);
"#);
    let calls = match shape { Shape::Chain => 1, Shape::Fan => n };
    for i in 0..calls {
        writeln!(text, "    f{i}();").unwrap();
    }
    text.push_str("    pair.1 as u32\n}\n");
    for i in 0..n {
        // Different return types grow the head table after main's overlay was made.
        let ty = ["u32", "u64", "i16", "i64"][i % 4];
        write!(text, "fn f{i}() -> {ty} {{ ").unwrap();
        if matches!(shape, Shape::Chain) && i + 1 < n {
            write!(text, "f{}(); ", i + 1).unwrap();
        }
        writeln!(text, "1{ty} }}").unwrap();
    }
    text
}

struct Side<'n> {
    program: LazyProgram<'n>,
    state: MakerState,
    checks: CheckCount,
}

impl<'n> Side<'n> {
    fn new(program: LazyProgram<'n>, entry: ItemKey, alloc: Alloc<'static>) -> Self {
        let state = MakerState::new(&program, entry, Isa { lse: true, os: Os::MacOs }, OverflowChecks::Off, UbChecks::Off, Panic::Abort, alloc).unwrap();
        Self { program, state, checks: CheckCount::default() }
    }

    fn make(&mut self, item: ItemKey, name: &[u8], alloc: Alloc<'static>) -> AVec<'static, u8> {
        let mut bytes = AVec::new_in(alloc);
        make_instance(&mut self.program, &mut SecondCheck, &mut self.state, &mut self.checks, item, name, &mut bytes)
            .unwrap_or_else(|e| panic!("make instance {item:?}: {e}"));
        bytes
    }
}

fn with_programs(files: &[SourceFile<'_>], compare: impl for<'n> FnOnce(&'n Names<'n>, ItemKey, Side<'n>, Side<'n>, Alloc<'static>)) {
    let alloc = blaze_own::testing::heap().alloc();
    let tc = Toolchain::none(alloc);
    let opts = NewFront { package: "ladder", edition: "2024", shelf: None, overflow: OverflowChecks::Off };
    let texts = ProgramTexts::new(files, "src/main.rs", opts, &tc, Panic::Abort).unwrap();
    let user = texts.user();
    let frame = Frame::new(Tag::Infer).unwrap();
    let erased = blaze_own::Erased::new(&frame);
    let inputs = Inputs::new(&user, &tc).unwrap();
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, erased.bump()).unwrap();
    let entry = checker_of(root_main(&names).unwrap());
    let old_frame = Frame::new(Tag::Infer).unwrap();
    let new = Side::new(LazyProgram::new(&names, &frame, files, Some(entry), alloc).unwrap(), entry, alloc);
    let mut old_program = LazyProgram::new(&names, &old_frame, files, Some(entry), alloc).unwrap();
    old_program.use_refork_oracle();
    compare(&names, entry, new, Side::new(old_program, entry, alloc), alloc);
}

fn walk_equal(new: &mut Side<'_>, old: &mut Side<'_>, entry: ItemKey, limit: usize, alloc: Alloc<'static>) -> HashSet<(ItemKey, Vec<u8>)> {
    let mut main_name = AVec::new_in(alloc);
    encode_instance(entry, List::EMPTY, PTys::EMPTY, alloc, &mut main_name).unwrap();
    let mut todo = vec![(entry, main_name.to_vec())];
    let mut seen = HashSet::new();
    while let Some((item, name)) = todo.pop() {
        if !seen.insert((item, name.clone())) {
            continue;
        }
        assert!(seen.len() <= limit, "unexpected declared instance {item:?}");
        let bytes = new.make(item, &name, alloc);
        let expected = old.make(item, &name, alloc);
        assert_eq!(bytes.as_slice(), expected.as_slice(), "instance {item:?} at {name:?}");
        EntryView::read(&bytes).unwrap().each_declared(|d| {
            todo.push((d.tabled, d.name.to_vec()));
            Ok(())
        }, |e| e).unwrap();
    }
    seen
}

fn compare(shape: Shape, n: usize) {
    let text = source(shape, n);
    let files = [SourceFile { path: "src/main.rs", text: &text }];
    with_programs(&files, |_, entry, mut new, mut old, alloc| {
        compare_call_graph(shape, n, entry, &mut new, &mut old, alloc);
    });
}

fn compare_call_graph(shape: Shape, n: usize, entry: ItemKey, new: &mut Side<'_>, old: &mut Side<'_>, alloc: Alloc<'static>) {
    let seen = walk_equal(new, old, entry, n + 1, alloc);
    assert_eq!(seen.len(), n + 1, "main and every helper must have a code record");
    // Rebuild the oldest body after every later head was made. Its written tuple still
    // has the same two elements, including their order in its original type list.
    let i = new.program.entry().unwrap();
    let (body, _) = new.program.function(i).body.unwrap();
    let tys = new.program.body_types(i);
    let pair = body.stmts.iter().find_map(|s| match s { Stmt::Let { ty, .. } => *ty, _ => None }).unwrap();
    let PNode::Tuple(fields) = tys.node(pair).unwrap() else { panic!("main's annotated local is a tuple") };
    let [a, b] = tys.list(fields).unwrap() else { panic!("the tuple has two fields") };
    assert_eq!(tys.node(*a).unwrap(), PNode::Uint(UintTy::U8));
    assert_eq!(tys.node(*b).unwrap(), PNode::Uint(UintTy::U16));
    let (_, main_name) = seen.iter().find(|(item, _)| *item == entry).unwrap();
    assert_eq!(new.make(entry, main_name, alloc).as_slice(), old.make(entry, main_name, alloc).as_slice());

    let bodies = (n + 1) as u64;
    let counts = new.program.lowering_counts();
    let planted = old.program.lowering_counts();
    let bound = |c: LoweringCounts| c.bodies_lowered + c.lower_again <= bodies;
    assert!(bound(counts), "{shape:?}({n}): {counts}");
    assert_eq!(counts, LoweringCounts { bodies_lowered: bodies, lower_again: 0, forks: 0, typing: None });
    assert_eq!(new.program.counts().live as u64, bodies);
    assert_eq!(planted, LoweringCounts { bodies_lowered: bodies, lower_again: bodies * (bodies + 1) / 2, forks: bodies, typing: None });
    assert!(!bound(planted), "the same bound must catch the actual re-lower-all loop");
    println!("{shape:?} N={n}: {bodies} equal records, {counts}, control caught {} re-lowerings", planted.lower_again);
}

#[test]
fn chain_lowerings_are_linear() {
    for n in [8, 64, 256] {
        compare(Shape::Chain, n);
    }
}

#[test]
fn fan_lowerings_are_linear() {
    for n in [8, 64, 256] {
        compare(Shape::Fan, n);
    }
}

/// The annotation of a two-field tuple, interpreted in the supplied table.
fn pair_fields(types: PTys<'_>, ty: PTy) -> Option<[PNode; 2]> {
    let PNode::Tuple(list) = types.node(ty).ok()? else { return None };
    let [a, b] = types.list(list).ok()? else { return None };
    Some([types.node(*a).ok()?, types.node(*b).ok()?])
}

#[test]
fn ground_instances_read_their_owners_after_head_growth() {
    // The same stack size as the front_equal record walk through the async rung.
    let thread = std::thread::Builder::new().stack_size(256 << 20).spawn(compare_ground_instances).unwrap();
    if let Err(panic) = thread.join() {
        std::panic::resume_unwind(panic);
    }
}

fn compare_ground_instances() {
    let mut text = String::from(r#"#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures, arbitrary_self_types)]
#![no_core]
#![no_main]
mod prelude;
mod future;
use prelude::*;
use future::*;

fn project<T>(x: T) -> T {
    let pair: (T, u8) = (x, 1u8);
    pair.0
}
fn invoke<F: FnOnce() -> u32>(f: F) -> u32 { f() }
fn closed(v: u32) -> u32 {
    invoke(move || {
        let pair: (u32, bool) = (v, true);
        pair.0
    })
}
async fn ready(v: u32) -> u32 {
    let pair: (u32, u16) = (v, 2u16);
    pair.0
}
fn drive<F: Future<Output = u32>>(mut fut: F) -> u32 {
    let mut wakes = 0u32;
    let mut cx = Context { wakes: &mut wakes };
    let pinned = Pin { pointer: &mut fut };
    match <F as Future>::poll(pinned, &mut cx) {
        Poll::Ready(v) => v,
        Poll::Pending => 0,
    }
}
#[unsafe(no_mangle)]
pub extern "C" fn main() -> u32 {
    drive(ready(closed(project(3u8) as u32 + project(4u64) as u32)))
}
"#);
    // Checking grow reads these signatures. Each adds a distinct tuple to the head table,
    // including list entries. None of these helpers is needed by main.
    for width in 1..=32 {
        writeln!(text, "fn later{width}() -> ({}) {{ ({}) }}", "bool,".repeat(width), "false,".repeat(width)).unwrap();
    }
    text.push_str("fn grow() {");
    for width in 1..=32 {
        write!(text, "later{width}();").unwrap();
    }
    text.push('}');
    let files = [
        SourceFile { path: "src/main.rs", text: &text },
        SourceFile { path: "src/prelude.rs", text: include_str!("../../../blaze-cli/tests/ladder/front2/x0-async-01/src/prelude.rs") },
        SourceFile { path: "src/future.rs", text: include_str!("../../../blaze-cli/tests/ladder/front2/x0-async-01/src/future.rs") },
    ];
    with_programs(&files, |names, entry, mut new, mut old, alloc| {
        let key = |name: &str| {
            let (lib, _, _) = names.items(0).find(|(lib, _, _)| names.item_text(*lib).is_some_and(|t| t.name == Some(name))).unwrap();
            checker_of(lib)
        };
        let [project, closed, ready, grow] = ["project", "closed", "ready", "grow"].map(key);
        for owner in [project, closed, ready, grow] {
            for side in [&mut new, &mut old] {
                side.state.ensure(&mut side.program, &mut SecondCheck, owner, &mut side.checks).unwrap().expect("a fixture body");
            }
        }
        let u32_ty = PNode::Uint(UintTy::U32);
        for (owner, fields) in [
            (project, [PNode::Param(0), PNode::Uint(UintTy::U8)]),
            (closed, [u32_ty, PNode::Bool]),
            (ready, [u32_ty, PNode::Uint(UintTy::U16)]),
        ] {
            let i = new.program.fn_index(owner).unwrap();
            let types = new.program.body_types(i);
            let (body, _) = new.program.function(i).body.unwrap();
            let ty = body.stmts.iter().find_map(|stmt| match *stmt {
                Stmt::Let { ty: Some(t), .. } if pair_fields(types, t) == Some(fields) => Some(t),
                _ => None,
            }).expect("the written pair in its owner's table");
            let heads = new.program.types();
            assert!(ty.0 as usize >= types.base_len() && (ty.0 as usize) < heads.len(), "a later head must occupy {owner:?}'s tuple index");
            assert_ne!(pair_fields(heads, ty), Some(fields), "the reused head index must denote a different type");
        }

        let seen = walk_equal(&mut new, &mut old, entry, 128, alloc);
        let call_once = new.program.lang_trait_fn(LangItem::FnOnce, "call_once").unwrap();
        let poll = new.program.lang_item(LangItem::FuturePoll).unwrap().unwrap();
        let mut ground = PortableTys::new_in(alloc);
        let mut args_seen = Vec::new();
        let mut functions = HashSet::new();
        for (owner, name) in seen {
            let (item, args) = decode_instance(&name, &mut ground, alloc).unwrap();
            functions.insert((owner, item));
            if owner == project && item == project {
                let view = ground.view();
                let [arg] = view.list(args).unwrap() else { panic!("project takes one type argument") };
                let PNode::Uint(width) = view.node(*arg).unwrap() else { panic!("main passes unsigned integers to project") };
                args_seen.push(width);
            }
        }
        args_seen.sort();
        assert_eq!(args_seen, [UintTy::U8, UintTy::U64]);
        for record in [(closed, closed), (closed, call_once), (ready, ready), (ready, poll)] {
            assert!(functions.contains(&record), "missing record for (owner, function) {record:?}");
        }
        println!("after head index reuse: equal records for project<u8>, project<u64>, closure owner and call_once, async fn and poll");
    });
}
