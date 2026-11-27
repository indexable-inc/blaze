//! D106 piece Y: the lazy program (front2/program/lazy) answers the seam as the whole program
//! (`Front2Program`) does, item by item, over one program's solved names; and asking about ONE
//! body checks that body alone.
//!
//! The rule tested (AGENTS.md rule 78): one seam, two instantiations, tested equal. Both are
//! made over the SAME solved names, so the only difference is lazy against whole: a census
//! that tables every fn `main` reaches, against rows made one body at a time by `check_one`.
//! A row index differs between the two (the lazy one numbers rows in the order asked), so
//! answers are compared by item key; a type is compared by its node's kind and its ADT's key,
//! since the two programs' type tables are laid out in different orders.
//!
//! Expected lines (printed with `--nocapture`):
//!   "lazy = whole: N bodies, 0 differences"
//!   "planted (answers of another item): caught, K differences"
//!   "one body: checked 1, rows R, live 1; the whole program checked N"

#![feature(allocator_api)]

use blaze_ladder::crates::{Inputs, Toolchain};
use blaze_ladder::front2::program::Front2Program;
use blaze_ladder::front2::program::lazy::LazyProgram;
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::seam::FrontEnd;
use blaze_typeck::{ExprId, PNode, PTy, PTys, PatId};

/// A program with no library (`no_core`), so the test needs no toolchain: a chain of calls,
/// a struct, integer, string and float literals, and an integer pattern.
const PROGRAM: &str = "#![feature(no_core, lang_items)]
#![no_core]

#[lang = \"sized\"]
trait Sized {}

#[lang = \"copy\"]
trait Copy {}

struct Pair {
    a: i32,
    b: i32,
}

fn leaf() -> i32 {
    7
}

fn pair() -> Pair {
    Pair { a: leaf(), b: 2 }
}

fn pick(n: i32) -> i32 {
    match n {
        3 => 4,
        _ => leaf(),
    }
}

fn words() -> &'static str {
    \"seam\"
}

fn half() -> f64 {
    0.5
}

fn main() {
    let p = pair();
    let _ = pick(p.a);
    let _ = words();
    let _ = half();
}
";

/// A type's kind and, for an ADT, its key: what two differently laid out tables agree on.
fn ty_shown(types: PTys<'_>, t: PTy) -> String {
    match types.node(t) {
        Ok(PNode::Adt(k, _)) => format!("Adt({k:?})"),
        Ok(n) => {
            let d = format!("{n:?}");
            String::from(&d[..d.find(['(', ' ', '{']).unwrap_or(d.len())])
        }
        Err(e) => format!("unread: {e:?}"),
    }
}

/// Every answer of the seam about fn `wi` of `whole` and fn `li` of `lazy` that differs, one
/// line each. `wi`'s key is asked of both where a method takes a key.
fn differences(whole: &Front2Program<'_>, wi: usize, lazy: &LazyProgram<'_>, li: usize) -> Vec<String> {
    let mut out = Vec::new();
    let (w, l) = (whole.function(wi), lazy.function(li));
    let key = w.key;
    let mut same = |what: &str, a: String, b: String| {
        if a != b {
            out.push(format!("{key:?} {what}: whole {a}, lazy {b}"));
        }
    };
    same("key", format!("{:?}", w.key), format!("{:?}", l.key));
    same("path", w.path.into(), l.path.into());
    same("at", format!("{:?}", w.at), format!("{:?}", l.at));
    same("text", w.text.into(), l.text.into());
    same("kind", format!("{:?}", w.kind), format!("{:?}", l.kind));
    same("abi", format!("{:?}", w.abi), format!("{:?}", l.abi));
    same("symbol", w.symbol.into(), l.symbol.into());
    same("export", format!("{:?}", w.export), format!("{:?}", l.export));
    same("const_fn", format!("{}", w.const_fn), format!("{}", l.const_fn));
    same("code_of", format!("{:?}", whole.code_of(wi)), format!("{:?}", lazy.code_of(li)));
    same("intrinsic", format!("{:?}", whole.intrinsic(key)), format!("{:?}", lazy.intrinsic(key)));
    same("c_variadic", format!("{:?}", whole.c_variadic(key)), format!("{:?}", lazy.c_variadic(key)));
    same("item_named", format!("{:?}", whole.item_named(key)), format!("{:?}", lazy.item_named(key)));
    same("ctor_of", format!("{:?}", whole.ctor_of(key)), format!("{:?}", lazy.ctor_of(key)));
    let (ws, ls) = (whole.sig(wi), lazy.sig(li));
    same("sig counts", format!("{} {} {} {}", ws.parent_count, ws.own_count, ws.declared_count, ws.self_param), format!("{} {} {} {}", ls.parent_count, ls.own_count, ls.declared_count, ls.self_param));
    same("sig header", format!("{:?}", ws.header), format!("{:?}", ls.header));
    let shown = |types: PTys<'_>, inputs: &[PTy], output: PTy| {
        let mut s: Vec<String> = inputs.iter().map(|&t| ty_shown(types, t)).collect();
        s.push(ty_shown(types, output));
        s.join(", ")
    };
    same("sig types", shown(ws.types, ws.inputs, ws.output), shown(ls.types, ls.inputs, ls.output));
    // Every ADT the signature names: its row as the seam lends it.
    for &t in ws.inputs.iter().chain(core::iter::once(&ws.output)) {
        if let Ok(PNode::Adt(k, _)) = ws.types.node(t) {
            let adt = |a: Option<blaze_typeck::Adt<'_>>| a.map(|a| format!("{:?} {} {}", a.kind, a.generics, a.fields.len()));
            same("adt", format!("{:?}", adt(whole.adt(k))), format!("{:?}", adt(lazy.adt(k))));
            same("repr", format!("{:?}", whole.repr(k)), format!("{:?}", lazy.repr(k)));
            same("discriminant 0", format!("{:?}", whole.discriminant(k, 0)), format!("{:?}", lazy.discriminant(k, 0)));
        }
    }
    match (w.body, l.body) {
        (Some((wb, _)), Some((lb, _))) => {
            same("body exprs", format!("{}", wb.exprs.len()), format!("{}", lb.exprs.len()));
            same("body pats", format!("{}", wb.pats.len()), format!("{}", lb.pats.len()));
            for e in 0..wb.exprs.len() as u32 {
                let e = ExprId(e);
                same("int_lit", format!("{:?}", whole.int_lit(key, e)), format!("{:?}", lazy.int_lit(key, e)));
                same("string", format!("{:?}", whole.string(key, e)), format!("{:?}", lazy.string(key, e)));
                same("float_lit", format!("{:?}", whole.float_lit(key, e)), format!("{:?}", lazy.float_lit(key, e)));
                same("format_template", format!("{:?}", whole.format_template(key, e).map(<[_]>::len)), format!("{:?}", lazy.format_template(key, e).map(<[_]>::len)));
            }
            for p in 0..wb.pats.len() as u32 {
                let p = PatId(p);
                same("int_pat", format!("{:?}", whole.int_pat(key, p)), format!("{:?}", lazy.int_pat(key, p)));
                same("float_pat", format!("{:?}", whole.float_pat(key, p)), format!("{:?}", lazy.float_pat(key, p)));
                same("str_pat", format!("{:?}", whole.str_pat(key, p)), format!("{:?}", lazy.str_pat(key, p)));
            }
        }
        (a, b) => same("body", format!("{}", a.is_some()), format!("{}", b.is_some())),
    }
    out
}

/// The program's names, solved once, and its files, for both programs.
fn with_program(each: impl FnOnce(&blaze_scope::resolve::Names<'_>, &[SourceFile<'_>])) {
    let files = [SourceFile { path: "src/main.rs", text: PROGRAM }];
    let tc = Toolchain::none(blaze_own::Alloc::global());
    let new = blaze_ladder::run::NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::On };
    let texts = blaze_ladder::program_texts::ProgramTexts::new(&files, "src/main.rs", new, &tc, blaze_build::Panic::Unwind).unwrap_or_else(|e| panic!("the program's texts: {e}"));
    let user = texts.user();
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    each(&names, &files);
}

#[test]
fn lazy_answers_as_the_whole_program() {
    with_program(|names, files| {
        let (whole_frame, lazy_frame) = (blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("a frame"), blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("a frame"));
        let whole = Front2Program::new(names, &whole_frame, files, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("the whole program: {e}"));
        let entry = whole.function(whole.entry().expect("the whole program's entry")).key;
        let mut lazy = LazyProgram::new(names, &lazy_frame, files, Some(entry), blaze_own::testing::heap().alloc()).unwrap_or_else(|e| panic!("the lazy program: {e}"));
        // The whole program's lang answers, asked of both before any body.
        let mut diffs: Vec<String> = Vec::new();
        for (what, a, b) in [
            ("drop_trait", format!("{:?}", whole.drop_trait()), format!("{:?}", lazy.drop_trait())),
            ("lang_named sized", format!("{:?}", whole.lang_named("sized")), format!("{:?}", lazy.lang_named("sized"))),
            ("fmt_argument_adt", format!("{:?}", whole.fmt_argument_adt()), format!("{:?}", lazy.fmt_argument_adt())),
            ("evaluates_at_args", format!("{}", whole.evaluates_at_args()), format!("{}", lazy.evaluates_at_args())),
        ] {
            if a != b {
                diffs.push(format!("{what}: whole {a}, lazy {b}"));
            }
        }
        // Each fn the whole program tables WITH a body, asked of the lazy one, one at a time.
        let mut bodies = 0;
        let mut planted = 0;
        let mut last: Option<usize> = None;
        for wi in 0..whole.fn_count() {
            let key = whole.function(wi).key;
            if whole.function(wi).body.is_none() {
                continue;
            }
            let (li, checked) = lazy.check_one(key).unwrap_or_else(|e| panic!("check_one({key:?}): {e}")).unwrap_or_else(|| panic!("check_one({key:?}): no such fn"));
            bodies += 1;
            diffs.extend(differences(&whole, wi, &lazy, li));
            let (wr, lr) = (whole.body_reads(wi, &checked), lazy.body_reads(li, &checked));
            if wr != lr {
                diffs.push(format!("{key:?} body_reads: whole {wr:?}, lazy {lr:?}"));
            }
            // THE PLANTED DIFFERENCE: the lazy answers of ANOTHER item (the row asked before)
            // compared with this one's: the comparison must see it.
            if let Some(other) = last {
                planted += differences(&whole, wi, &lazy, other).len();
            }
            if let Some(prev) = last {
                lazy.done(prev);
            }
            last = Some(li);
        }
        assert_eq!(lazy.entry().map(|i| lazy.function(i).key), Some(entry), "the lazy entry is the whole program's");
        println!("lazy = whole: {bodies} bodies, {} differences", diffs.len());
        assert!(diffs.is_empty(), "the lazy program answered otherwise:\n{}", diffs.join("\n"));
        println!("planted (answers of another item): caught, {planted} differences");
        assert!(bodies > 1 && planted > 0, "the planted difference was not seen: {planted}");
    });
}

#[test]
fn one_body_checks_that_body_alone() {
    with_program(|names, files| {
        let (whole_frame, lazy_frame) = (blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("a frame"), blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("a frame"));
        let whole = Front2Program::new(names, &whole_frame, files, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("the whole program: {e}"));
        let checked_whole = (0..whole.fn_count()).filter(|&i| whole.function(i).body.is_some()).count();
        let pick = (0..whole.fn_count()).map(|i| whole.function(i)).find(|f| f.path.ends_with("pick")).expect("`pick` tabled").key;
        let mut lazy = LazyProgram::new(names, &lazy_frame, files, None, blaze_own::testing::heap().alloc()).unwrap_or_else(|e| panic!("the lazy program: {e}"));
        let (i, _) = lazy.check_one(pick).unwrap_or_else(|e| panic!("check_one(pick): {e}")).expect("pick is a fn");
        let c = lazy.counts();
        println!("one body: checked {}, rows {}, live {}; the whole program checked {checked_whole}", c.checked, c.rows, c.live);
        // `pick` and the one fn it calls (`leaf`): two rows, one body checked.
        assert_eq!((c.checked, c.rows, c.live), (1, 2, 1), "asking `pick` made more than its own body and its callee's head");
        assert!(lazy.function(i).body.is_some(), "the body asked for is live");
        lazy.done(i);
        assert_eq!(lazy.counts().live, 0, "done dropped the body");
        assert!(lazy.function(i).body.is_none(), "a done row lends no body");
        assert!(checked_whole > 1);
    });
}
