//! Macro calls in bodies, expanded through [`crate::by_name::ByName`] and lowered in place:
//! each position's fragment, the hygiene of locals, nested calls, eager builtins, and the
//! refusals by name.

use std::boxed::Box;
use std::string::String;
use std::vec::Vec as StdVec;
use std::{format, vec};

use blaze_expand::{
    DefAt, DefSite, Export, ExpandCx, HostAsk, HostError, HostWhy, IncludeAs, Included, LocalInner,
    MacroDefinition, Place, PlaceOf, Prepared, ProcCall, Produced, TokenStream, lex, prepare,
};
use blaze_id::CrateItemKey;
use blaze_ids::ContentId;
use blaze_own::Alloc;

use crate::by_name::ByName;
use crate::columns::View;
use crate::form::*;
use crate::{Cfg, CfgAnswer, Edition, Lowered, lower_body_expanding};

pub(crate) struct Unix;

impl Cfg for Unix {
    fn atom(&mut self, name: &str, value: Option<&str>) -> CfgAnswer {
        match (name, value) {
            ("unix", None) => CfgAnswer::Yes,
            _ => CfgAnswer::No,
        }
    }
}

/// The host of the tests' expansions: no environment, no files, line 1.
pub(crate) struct Host;

impl ExpandCx for Host {
    fn env(&self, _: &str) -> Result<Option<&str>, HostError> {
        Ok(None)
    }
    fn cfg(&self, _: &TokenStream<'_>) -> Result<bool, HostError> {
        Ok(false)
    }
    fn include(&self, _: &str, _: IncludeAs) -> Result<Included<'_>, HostError> {
        Ok(Included::Missing)
    }
    fn place(&self, _: PlaceOf) -> Result<Place<'_>, HostError> {
        Ok(Place::Number(1))
    }
    fn run_proc_macro<'a>(
        &self,
        _: &MacroDefinition<'_>,
        _: ProcCall<'_>,
        _: Alloc<'a>,
    ) -> Result<Produced<'a>, HostError> {
        Err(HostError { ask: HostAsk::ProcMacro, why: HostWhy::Failed })
    }
}

pub(crate) fn at() -> DefAt {
    DefAt {
        site: DefSite { krate: CrateItemKey(ContentId([1; 32])), module: None },
        edition: Edition::E2024,
        export: Export::Exported,
        local_inner: LocalInner::No,
        transparency: None,
    }
}

/// `text`'s body lowered with `defs` (`(name, rules)`, the rules without their braces):
/// the lowering, which reads its expansions' text through its view.
fn expanding<'t>(defs: &[(&'static str, &str)], text: &'t str) -> Lowered<'static, 't> {
    let defs: StdVec<Def> = defs.iter().map(|&(name, body)| Def { name, body, macro2: false }).collect();
    expanding_defs(&defs, text)
}

/// A definition of a test: `macro_rules! name { body }`, or `macro name { body }`.
#[derive(Clone, Copy)]
struct Def<'b> {
    name: &'static str,
    body: &'b str,
    macro2: bool,
}

fn expanding_defs<'t>(defs: &[Def<'_>], text: &'t str) -> Lowered<'static, 't> {
    let mut expander = by_name(defs);
    let l = match lower_body_expanding(text, Edition::E2024, &mut Unix, &mut crate::NoModule, &mut expander, Alloc::global()) {
        Ok(l) => l,
        Err(need) => panic!("{text}: needs {:?}", need.atoms),
    };
    l
}

/// The expander of `defs`: each prepared once, found by its last name.
fn by_name(defs: &[Def<'_>]) -> ByName<'static, 'static, Host> {
    by_name_with(defs, Host)
}

/// [`by_name`] over the host `host`.
fn by_name_with<C: ExpandCx>(defs: &[Def<'_>], host: C) -> ByName<'static, 'static, C> {
    let mut rules: StdVec<(&'static str, Prepared<'static>)> = StdVec::new();
    for d in defs {
        let stream = lex(d.body, Edition::E2024, Alloc::global()).expect("the rules lex");
        let def = match d.macro2 {
            false => MacroDefinition::rules(stream, at()),
            true => MacroDefinition::macro2(None, stream, at()),
        };
        let def: &'static MacroDefinition<'static> = Box::leak(Box::new(def));
        rules.push((d.name, prepare(def).expect("the rules compile")));
    }
    let rules: &'static [(&'static str, Prepared<'static>)] = Box::leak(rules.into_boxed_slice());
    ByName::new(rules, at(), host, Alloc::global())
}

/// A body's host as the ladder's is (front2 body_host.rs): `file!()` is `src/lib.rs`, and
/// `line!()`/`column!()` are answered only when told the written call (`place_of_call`), each
/// as `1000 + call` / `call` so the test reads which byte it was told.
struct Placing;

impl ExpandCx for Placing {
    fn env(&self, _: &str) -> Result<Option<&str>, HostError> {
        Ok(None)
    }
    fn cfg(&self, _: &TokenStream<'_>) -> Result<bool, HostError> {
        Ok(false)
    }
    fn include(&self, _: &str, _: IncludeAs) -> Result<Included<'_>, HostError> {
        Ok(Included::Missing)
    }
    fn place(&self, what: PlaceOf) -> Result<Place<'_>, HostError> {
        match what {
            PlaceOf::File => Ok(Place::Text("src/lib.rs")),
            _ => Err(HostError { ask: HostAsk::Place, why: HostWhy::Refused }),
        }
    }
    fn place_of_call(&self, what: PlaceOf, call: u32) -> Result<Place<'_>, HostError> {
        match what {
            PlaceOf::Line => Ok(Place::Number(1000 + call)),
            PlaceOf::Column => Ok(Place::Number(call)),
            _ => self.place(what),
        }
    }
    fn run_proc_macro<'a>(&self, _: &MacroDefinition<'_>, _: ProcCall<'_>, _: Alloc<'a>) -> Result<Produced<'a>, HostError> {
        Err(HostError { ask: HostAsk::ProcMacro, why: HostWhy::Failed })
    }
}

/// The dump of `text`'s body lowered with `defs` over [`Placing`], every form handled.
fn placed_dump(defs: &[(&'static str, &str)], text: &str) -> String {
    let defs: StdVec<Def> = defs.iter().map(|&(name, body)| Def { name, body, macro2: false }).collect();
    let mut expander = by_name_with(&defs, Placing);
    let l = match lower_body_expanding(text, Edition::E2024, &mut Unix, &mut crate::NoModule, &mut expander, Alloc::global()) {
        Ok(l) => l,
        Err(need) => panic!("{text}: needs {:?}", need.atoms),
    };
    let dump = dump_of(&l);
    let unhandled: StdVec<_> = l.view().unhandled().collect();
    assert!(unhandled.is_empty(), "{text}: unhandled {unhandled:?}\n{dump}");
    dump
}

/// tempo t1855d: tracing's `event!` writes `concat!(file!(), ":", line!())`, so `line!()` is
/// in eager input in a MACRO'S expansion in a body. Its line is the outermost WRITTEN call's
/// (core `line!`: "the first macro invocation leading up to the expansion"): `m!()` at the
/// body's byte 25, which the host is told through the nested eager call (by_name.rs
/// `BodyCx::written`). Twin: the `concat!` written in the body itself, at the same byte, gives
/// the same answer; and `column!()` the same byte. Control: over a host made for one call
/// ([`Host`], the default `place_of_call` is its `place`) the answer is that host's, 1.
/// Prints nothing; passes.
#[test]
fn line_in_eager_input_is_placed_at_the_written_call() {
    let m = ("m", "() => { concat!(file!(), \":\", line!()) }");
    let through = placed_dump(&[m], "fn f() -> &'static str { m!() }");
    assert!(through.contains("(lit \"src/lib.rs:1025\")"), "{through}");
    let written = placed_dump(&[], "fn f() -> &'static str { concat!(file!(), \":\", line!()) }");
    assert!(written.contains("(lit \"src/lib.rs:1025\")"), "twin: {written}");
    let column = placed_dump(&[], "fn f() -> &'static str { concat!(column!()) }");
    assert!(column.contains("(lit \"25\")"), "twin: {column}");
    assert_expands(&[], "fn f() -> &'static str { concat!(\"a\", line!()) }", &["(lit \"a1\")"]);
}

pub(crate) fn dump_of(l: &Lowered<'_, '_>) -> String {
    let mut out = String::new();
    crate::dump::dump(&l.view(), Alloc::global(), &mut out).expect("a String takes every write");
    out
}

/// Lowers with `defs`, asserts every form handled and `fragments` in the dump in order, and
/// gives the number of expansions read.
#[track_caller]
fn assert_expands(defs: &[(&'static str, &str)], text: &str, fragments: &[&str]) -> usize {
    let l = expanding(defs, text);
    let dump = dump_of(&l);
    let v = l.view();
    let unhandled: StdVec<_> = v.unhandled().collect();
    assert!(unhandled.is_empty(), "{text}: unhandled {unhandled:?}\n{dump}");
    assert!(v.diags.is_empty(), "{text}: diagnostics\n{dump}");
    let mut at = 0;
    for f in fragments {
        match dump[at..].find(f) {
            Some(i) => at += i + f.len(),
            None => panic!("{text}: `{f}` not found after byte {at} of\n{dump}"),
        }
    }
    assert_eq!(v.expansion_calls().count(), v.expansion_keys.len());
    v.expansion_keys.len()
}

fn whys(defs: &[(&'static str, &str)], text: &str) -> StdVec<Why> {
    let l = expanding(defs, text);
    l.view().unhandled().map(|u| u.why).collect()
}

fn binds(v: &View<'_>, name: &str) -> StdVec<PatId> {
    (0..v.pats.len() as u32)
        .map(PatId)
        .filter(|p| matches!(v.pat(*p), Ok(Pat::Bind { name: n, .. }) if v.name_text(n) == Ok(name)))
        .collect()
}

fn locals(v: &View<'_>) -> StdVec<PatId> {
    (0..v.exprs.len() as u32)
        .filter_map(|e| match v.expr(ExprId(e)) {
            Ok(Expr::Local { pat, .. }) => Some(pat),
            _ => None,
        })
        .collect()
}

const DOUBLE: (&str, &str) = ("double", "($e:expr) => { $e * 2 }");

#[test]
fn each_position_takes_its_fragment() {
    // An expression.
    let n = assert_expands(&[DOUBLE], "fn f(x: u8) -> u8 { double!(x) }", &["(tail (bin", "x@p0", "(lit 2)"]);
    assert_eq!(n, 1);
    // An `$e:expr` stays one operand: `(x + 1) * 2`, not `x + 1 * 2`.
    let l = expanding(&[DOUBLE], "fn f(x: u8) -> u8 { double!(x + 1) }");
    let dump = dump_of(&l);
    assert!(dump.contains("(bin Mul (paren (bin Add x@p0 (lit 1))) (lit 2))"), "{dump}");
    // Joint punctuation stays glued in the text: a path through `::`.
    assert_expands(&[("p", "() => { core::mem::drop }")], "fn f() { p!(); }", &["(path core mem drop)"]);
    // Statements: a call that is a statement of a block, its `let` binding in the block.
    let two = ("two", "($a:ident) => { let $a = 1; let b = $a; }");
    assert_expands(&[two], "fn f() -> u8 { two!(z); z }", &["(let z@p0 (lit 1))", "(tail z@p0)"]);
    // A pattern and a type.
    let some = ("some", "() => { Some(_) }");
    assert_expands(&[some], "fn f(o: Option<u8>) { if let some!() = o {} }", &["(path Some)"]);
    let byte = ("byte", "() => { u8 }");
    assert_expands(&[byte], "fn f() { let x: byte!() = 1; }", &["(path u8)"]);
}

#[test]
fn a_macros_locals_do_not_capture_the_callers() {
    // The macro's `x` is its own; the `x` passed in is the parameter, on both sides.
    let m = ("m", "($e:expr) => {{ let x = 1; x + $e }}");
    let text = "fn f(x: u8) -> u8 { m!(x) }";
    let l = expanding(&[m], text);
    let v = l.view();
    let xs = binds(&v, "x");
    assert_eq!(xs.len(), 2, "the parameter and the macro's own");
    assert_eq!(locals(&v), vec![xs[1], xs[0]], "the macro's `x` sees its let, `$e` the parameter\n{}", dump_of(&l));
    // A name the caller passes binds where the caller reads it: `two!(z); z`.
    let two = ("two", "($a:ident) => { let $a = 1; }");
    let l = expanding(&[two], "fn f() -> u8 { two!(z); z }");
    let v = l.view();
    assert_eq!(locals(&v), binds(&v, "z"));
    // A name the macro makes is not the caller's: `y` here is no local.
    let mk = ("mk", "() => { let y = 1; }");
    let l = expanding(&[mk], "fn f() { mk!(); y; }");
    assert!(locals(&l.view()).is_empty());
}

#[test]
fn nested_calls_expand_from_the_expansion() {
    let outer = ("outer", "($e:expr) => { double!($e) + 1 }");
    let n = assert_expands(&[DOUBLE, outer], "fn f(x: u8) -> u8 { outer!(x) }", &["(bin", "x@p0", "(lit 2)", "(lit 1)"]);
    assert_eq!(n, 2, "the outer call's expansion and the nested one's are both read");
}

#[test]
fn eager_builtins_take_their_inputs_expanded() {
    assert_expands(&[], "fn f() -> &'static str { concat!(\"a\", stringify!(b c), 1) }", &["(lit \"ab c1\")"]);
}

#[test]
fn refusals_are_named() {
    assert_eq!(whys(&[], "fn f() { nowhere!(); }"), vec![Why::NotExpanded]);
    assert_eq!(whys(&[], "fn f() { core::arch::global_asm!(\"nop\"); }"), vec![Why::Asm]);
    let forever = ("forever", "() => { forever!() }");
    assert_eq!(whys(&[forever], "fn f() { forever!(); }"), vec![Why::TooDeep]);
}

/// A body call whose expansion calls it twice stays under the depth bound for 2^128 calls:
/// the budget refuses the rest, each refused call kept as a row naming its path.
#[test]
fn a_body_that_fans_out_is_refused_at_the_budget() {
    // Two calls an expansion reach only 8,256 (128 * 129 / 2) before every chain is too deep;
    // sixteen pass the budget.
    let many = ("many", "() => { many!(); many!(); many!(); many!(); many!(); many!(); many!(); many!(); many!(); many!(); many!(); many!(); many!(); many!(); many!(); many!() }");
    let l = expanding(&[many], "fn f() { many!(); }");
    let v = l.view();
    let read = v.expansion_calls().count();
    assert_eq!(read, crate::EXPANSION_BUDGET as usize, "exactly the budget's expansions are read ({} keys)", v.expansion_keys.len());
    assert!(v.unhandled().any(|u| u.why == Why::TooMany), "the calls past the budget are refused as such");
}

fn lower_items(defs: &[(&'static str, &str)], text: &str) -> (crate::FileStats, Records) {
    let mut rules: StdVec<(&'static str, Prepared<'static>)> = StdVec::new();
    for (name, body) in defs {
        let stream = lex(body, Edition::E2024, Alloc::global()).expect("the rules lex");
        let def: &'static MacroDefinition<'static> = Box::leak(Box::new(MacroDefinition::rules(stream, at())));
        rules.push((name, prepare(def).expect("the rules compile")));
    }
    let rules: &'static [(&'static str, Prepared<'static>)] = Box::leak(rules.into_boxed_slice());
    let mut expander = ByName::new(rules, at(), Host, Alloc::global());
    let mut records = Records::default();
    let stats = crate::lower_file(text, Edition::E2024, &mut Unix, &mut crate::NoModule, &mut expander, &mut records, Alloc::global());
    (stats, records)
}

/// An item macro that calls itself is refused at the depth bound, by a typed refusal naming
/// it, after exactly [`crate::EXPANSION_DEPTH`] expansions.
#[test]
fn an_item_macro_chain_is_refused_at_the_depth_bound() {
    let (stats, records) = lower_items(&[("once", "() => { once!{} }")], "once!{}");
    assert_eq!(stats.macro_calls_expanded, u64::from(crate::EXPANSION_DEPTH));
    assert_eq!(records.1, [(String::from("once"), crate::Bound::Depth)]);
}

/// An item macro whose output calls it twice is refused at the item's budget: the depth bound
/// alone would allow 2^128 expansions.
#[test]
fn an_item_macro_that_fans_out_is_refused_at_the_budget() {
    let (stats, records) = lower_items(&[("twice", "() => { twice!{} twice!{} }")], "twice!{}");
    assert_eq!(stats.macro_calls_expanded, u64::from(crate::EXPANSION_BUDGET));
    assert!(stats.macro_calls_over_budget > 0);
    assert!(records.1.iter().all(|(path, _)| path == "twice"), "each refusal names the macro");
    assert!(records.1.contains(&(String::from("twice"), crate::Bound::Budget)));
}

/// Every range of a body whose expansions are longer than its item reads its own bytes through
/// the view, a node an expansion made sits at the call it came from, and a range of another
/// record or a record opened with another text is refused (rule 76: no reader slices a text).
/// `format_args!` whose template is a `concat!` or `stringify!` call: the expander expands the
/// template first and gives `builtin # format_args("..", ..)`, lowered as the same node.
#[test]
fn a_format_template_made_by_a_macro_is_one_node() {
    let l = expanding(&[], "fn f(x: u32) { format_args!(concat!(\"a\", \"{}\"), x); }");
    let dump = dump_of(&l);
    assert!(dump.contains("(format-args \"a{}\""), "{dump}");
    assert!(dump.contains("(arg x@p0)") || dump.contains("x@p0"), "{dump}");
    assert_eq!(l.view().unhandled().count(), 0, "{dump}");
    let l = expanding(&[], "fn f() { format_args!(stringify!(a b)); }");
    let dump = dump_of(&l);
    assert!(dump.contains("(format-args \"a b\""), "{dump}");
}

/// `unreachable!("m")` in edition 2021: a placeholder whose argument is another
/// `format_args!`, as core's `unreachable_2021` writes it.
#[test]
fn a_format_argument_may_be_a_format_call() {
    const UNREACHABLE: (&str, &str) = (
        "unreachable_2021",
        "($($t:tt)+) => { panic_2021!(\"internal error: entered unreachable code: {}\", $crate::format_args!($($t)+)) }",
    );
    const PANIC: (&str, &str) = ("panic_2021", "($($t:tt)+) => {{ $crate::const_format_args!($($t)+) }}");
    let l = expanding(&[UNREACHABLE, PANIC], "fn f() { unreachable_2021!(\"m\"); }");
    let dump = dump_of(&l);
    struct Infer;
    impl crate::typeck::Types for Infer {
        fn ty(
            &mut self,
            _: &View<'_>,
            _: TypeId,
            tys: &mut blaze_typeck::PortableTys<'_>,
        ) -> Result<blaze_typeck::PTy, crate::typeck::Refused> {
            tys.intern(blaze_typeck::PNode::Infer).map_err(crate::typeck::Refused::Check)
        }
        fn const_arg(&mut self, view: &View<'_>, c: ConstIdx, _: &mut blaze_typeck::PortableTys<'_>) -> Result<blaze_typeck::PTy, crate::typeck::Refused> {
            Err(crate::typeck::no_const_arg(view, c))
        }
        fn repeat_len(&mut self, _: &View<'_>, _: ExprId) -> Result<u64, crate::typeck::Refused> {
            Ok(0)
        }
    }
    let mut tys = blaze_typeck::PortableTys::new_in(Alloc::global());
    let converted = crate::typeck::to_typeck(&l.view(), blaze_typeck::ItemKey(0), &mut tys, &mut Infer, Alloc::global());
    assert!(converted.is_ok(), "{:?}\n{dump}", converted.err());
}

/// The body converted to the checker's form, every type left to inference: `Ok`, or the
/// refusal and the dump.
fn converts(l: &Lowered<'_, '_>) -> Result<(), String> {
    struct Infer;
    impl crate::typeck::Types for Infer {
        fn ty(
            &mut self,
            _: &View<'_>,
            _: TypeId,
            tys: &mut blaze_typeck::PortableTys<'_>,
        ) -> Result<blaze_typeck::PTy, crate::typeck::Refused> {
            tys.intern(blaze_typeck::PNode::Infer).map_err(crate::typeck::Refused::Check)
        }
        fn const_arg(&mut self, view: &View<'_>, c: ConstIdx, _: &mut blaze_typeck::PortableTys<'_>) -> Result<blaze_typeck::PTy, crate::typeck::Refused> {
            Err(crate::typeck::no_const_arg(view, c))
        }
        fn repeat_len(&mut self, _: &View<'_>, _: ExprId) -> Result<u64, crate::typeck::Refused> {
            Ok(0)
        }
    }
    let v = l.view();
    let unhandled: StdVec<_> = v.unhandled().collect();
    if !unhandled.is_empty() || !v.diags.is_empty() {
        return Err(format!("unhandled {unhandled:?}, diagnostics {:?}\n{}", v.diags, dump_of(l)));
    }
    let mut tys = blaze_typeck::PortableTys::new_in(Alloc::global());
    match crate::typeck::to_typeck(&v, blaze_typeck::ItemKey(0), &mut tys, &mut Infer, Alloc::global()) {
        Ok(_) => Ok(()),
        Err(e) => Err(format!("{e:?}\n{}", dump_of(l))),
    }
}

// core's macros the hosted hello's bodies call, as the pinned nightly's `core` writes them.
const CORE_DEBUG_ASSERT: Def<'static> = Def {
    name: "debug_assert",
    body: "($($arg:tt)*) => { if $crate::cfg!(debug_assertions) { $crate::assert!($($arg)*); } };",
    macro2: false,
};
const CORE_WRITE: Def<'static> = Def {
    name: "write",
    body: "($dst:expr, $($arg:tt)*) => { $dst.write_fmt($crate::format_args!($($arg)*)) };
           ($($arg:tt)*) => { compile_error!(\"requires a destination and format arguments\") };",
    macro2: false,
};
const CORE_CONST_EVAL_SELECT: Def<'static> = Def {
    name: "const_eval_select",
    body: "(
        @capture$([$($binders:tt)*])? { $($arg:ident : $ty:ty = $val:expr),* $(,)? } $( -> $ret:ty )? :
        if const
            $(#[$compiletime_attr:meta])* $compiletime:block
        else
            $(#[$runtime_attr:meta])* $runtime:block
    ) => {{
        #[inline]
        $(#[$runtime_attr])*
        fn runtime$(<$($binders)*>)?($($arg: $ty),*) $( -> $ret )? {
            $runtime
        }

        #[inline]
        $(#[$compiletime_attr])*
        const fn compiletime$(<$($binders)*>)?($($arg: $ty),*) $( -> $ret )? {
            $(let _ = $arg;)*

            $compiletime
        }

        const_eval_select(($($val,)*), compiletime, runtime)
    }},
    (
        @capture$([$($binders:tt)*])? { $($arg:ident : $ty:ty),* $(,)? } $( -> $ret:ty )? :
        if const
            $(#[$compiletime_attr:meta])* $compiletime:block
        else
            $(#[$runtime_attr:meta])* $runtime:block
    ) => {
        $crate::intrinsics::const_eval_select!(
            @capture$([$($binders)*])? { $($arg : $ty = $arg),* } $(-> $ret)? :
            if const
                $(#[$compiletime_attr])* $compiletime
            else
                $(#[$runtime_attr])* $runtime
        )
    },",
    macro2: true,
};
const CORE_PANIC_2021: Def<'static> =
    Def { name: "panic_2021", body: "($($t:tt)+) => {{ $crate::const_format_args!($($t)+) }}", macro2: false };

/// The hosted hello's census: `<*const T>::is_null` (core const_ptr.rs:26) calls core's
/// `macro const_eval_select`, a `macro` (2.0) whose second arm calls the first through
/// `$crate::intrinsics::`; it expands to a block of two local `fn`s (`runtime`, `compiletime`)
/// and a call of the `const_eval_select` intrinsic, which the evaluator answers with the const
/// arm and built code with the runtime arm: nothing of it is special to expansion.
#[test]
fn core_is_null_expands_its_const_eval_select() {
    let text = "pub const fn is_null(self) -> bool {
        let ptr = self as *const u8;
        const_eval_select!(
            @capture { ptr: *const u8 } -> bool:
            if const #[rustc_allow_const_fn_unstable(const_raw_ptr_comparison)] {
                match (ptr).guaranteed_eq(null_mut()) {
                    Some(res) => res,
                    None => panic!(\"null-ness of this pointer cannot be determined in const context\"),
                }
            } else {
                ptr.addr() == 0
            }
        )
    }";
    let l = expanding_defs(&[CORE_CONST_EVAL_SELECT, CORE_PANIC_2021], text);
    let dump = dump_of(&l);
    assert!(dump.contains("(call (path const_eval_select) (tuple ptr@p"), "{dump}");
    converts(&l).unwrap();
}

/// `Io::hosted` (bstd io.rs:678) calls `debug_assert!`: `if cfg!(debug_assertions) {
/// assert!(..); }`, `cfg!` answered by the host's cfg set.
#[test]
fn bstd_io_hosted_expands_its_debug_assert() {
    let text = "pub(crate) fn hosted(offers: Offers, limits: Limits) -> Io {
        debug_assert!(!offers.host, \"a hosted world has no host facts to offer\");
        Io { reach: Reach::Hosted, offers, limits, plant: Plant::None }
    }";
    let l = expanding_defs(&[CORE_DEBUG_ASSERT, CORE_PANIC_2021], text);
    let dump = dump_of(&l);
    assert!(dump.contains("(if (lit false)"), "{dump}");
    converts(&l).unwrap();
}

/// `<Result<T, E> as Outcome>::status` (bstd entry.rs:87) calls `write!(said, "{error}")`:
/// `said.write_fmt(format_args!("{error}"))`, its argument captured.
#[test]
fn bstd_outcome_status_expands_its_write() {
    let text = "fn status<W: fmt::Write + ?Sized>(self, said: &mut W) -> Status {
        match self {
            Ok(value) => value.status(said),
            Err(error) => {
                let _ = write!(said, \"{error}\");
                Status::FAILED
            }
        }
    }";
    let l = expanding_defs(&[CORE_WRITE], text);
    let dump = dump_of(&l);
    assert!(dump.contains("(method said@p1 write_fmt (format-args \"{error}\" {0:} (captured error error@p4))"), "{dump}");
    converts(&l).unwrap();
}

/// What [`crate::lower_file`] gave for each item: its text, which record, and that record's
/// dump with its unhandled rows, diagnostics and conversion.
#[derive(Default)]
struct Records(StdVec<(String, crate::What, String)>, StdVec<(String, crate::Bound)>);

impl crate::Sink for Records {
    fn record(&mut self, item: &crate::ItemRef<'_>, what: crate::What, _: ContentId, outcome: crate::Outcome<'_, '_>) {
        let crate::Outcome::Record { view, .. } = outcome else { panic!("{}: needs cfg", item.item_text()) };
        let mut out = String::new();
        crate::dump::dump(&view, Alloc::global(), &mut out).expect("a String takes every write");
        let unhandled: StdVec<_> = view.unhandled().collect();
        if !(unhandled.is_empty() && view.diags.is_empty()) {
            out = format!("UNCLEAN {unhandled:?} {:?}\n{out}", view.diags);
        }
        // A body word keeps `$crate` as written, its crate in its mark (token-mark-1,
        // blaze-expand's `body_words`): the text and the dump are compared as they are.
        self.0.push((String::from(item.item_text()), what, out));
    }

    fn refused(&mut self, _: &crate::ItemRef<'_>, refusal: crate::Refusal<'_>) {
        self.1.push((String::from(refusal.path), refusal.bound));
    }
}

/// bstd's `Placement` (cx.rs) and `Reach` (io.rs), each `#[derive(Clone)]`: the driver reads
/// the derive list, blaze-expand writes the impl the Reference specifies (a `match` of `self`,
/// each variant's fields cloned through `Clone::clone`, a unit variant made again), and the
/// impl is lowered as an item of its own, its `clone` a body.
#[test]
fn a_derive_clone_is_an_impl_lowered_with_its_item() {
    let text = "/// Where a context's task placement is held.
#[derive(Clone)]
enum Placement {
    /// Shared by the task's contexts.
    Task(Rc<Placed>),
    HostedRoot,
}

#[derive(Clone)]
enum Reach {
    Shared { world: Arc<Lock<World>>, env: Vars, args: ArgList },
    Hosted,
}
";
    let rules: &'static [(&'static str, Prepared<'static>)] = &[];
    let mut expander = ByName::new(rules, at(), Host, Alloc::global());
    let mut records = Records::default();
    let stats = crate::lower_file(text, Edition::E2024, &mut Unix, &mut crate::NoModule, &mut expander, &mut records, Alloc::global());
    assert_eq!((stats.macro_calls_expanded, stats.macro_calls_unsupported, stats.macro_calls_failed), (2, 0, 0));
    let unclean: StdVec<_> = records.0.iter().filter(|(_, _, d)| d.starts_with("UNCLEAN")).collect();
    assert!(unclean.is_empty(), "{unclean:#?}");
    let impl_of = |ty: &str| {
        records
            .0
            .iter()
            .find(|(t, w, d)| {
                *w == crate::What::Signature
                    && d.starts_with("(sig IMPL (attrs #[automatically_derived])")
                    && d.contains(&format!("(trait (path $crate clone Clone)) (self (path {ty})))"))
                    && t.contains("impl $crate :: clone :: Clone for")
            })
            .unwrap_or_else(|| panic!("no `impl Clone for {ty}` among {:#?}", records.0))
    };
    impl_of("Placement");
    impl_of("Reach");
    let bodies: StdVec<&String> =
        records.0.iter().filter(|(t, w, _)| *w == crate::What::Body && t.contains("fn clone")).map(|(_, _, d)| d).collect();
    assert_eq!(bodies.len(), 2, "{:#?}", records.0);
    let placement = bodies.iter().find(|d| d.contains("Task")).expect("Placement's clone");
    assert!(
        placement.contains(
            "(match self@p0 (arm (tuple-struct (path Placement Task) f0_self@p1) (call (path Placement Task) (call (path $crate clone Clone clone) f0_self@p1))) (arm (path Placement HostedRoot) (path Placement HostedRoot)))"
        ),
        "{placement}"
    );
    let reach = bodies.iter().find(|d| d.contains("Shared")).expect("Reach's clone");
    for field in ["world", "env", "args"] {
        assert!(reach.contains(field), "{field}: {reach}");
    }
}

/// The hosted hello's entry: `#[bstd::main]` is bstd's `macro_rules! main` (blaze-std
/// entry.rs), an `attr()` arm. On `fn main(cx: &Cx, out: Out)` (D104: a grant by name) it makes the process's
/// `main() -> Exit`, holding the written `main`, the `MANIFEST` static, and
/// `bstd_hosted_start` with its local `hosted`: every one an item lowered from the
/// expansion's words, its bodies clean.
#[test]
fn bstd_main_makes_the_hosted_entry() {
    const BSTD_MAIN: &str = "attr() (
        $(#[$meta:meta])*
        $vis:vis fn main($cx:tt : $cxty:ty $(, $name:tt : $ty:ty)* $(,)?) $(-> $returns:ty)? $body:block
    ) => {
        $(#[$meta])*
        $vis fn main() -> $crate::entry::Exit {
            fn main($cx: $cxty $(, $name: $ty)*) $(-> $returns)? $body
            static MANIFEST: $crate::grant::Manifest = $crate::grant::Manifest::new(&[
                $( $crate::grant::Param::new(stringify!($name), <$ty as $crate::grant::Grant>::KIND) ),*
            ]);
            #[unsafe(no_mangle)]
            pub extern \"C\" fn bstd_hosted_start(table: *const $crate::wire::HostTable) -> i32 {
                fn hosted(cx: &$crate::Cx, io: $crate::Io) -> $crate::entry::Status {
                    let ran = main(cx $(, match $crate::guest::grant::<$ty>(&io, stringify!($name)) {
                        ::core::option::Option::Some(granted) => granted,
                        ::core::option::Option::None => return $crate::entry::Status::REFUSED,
                    })*);
                    $crate::guest::finish(&io, ran)
                }
                $crate::guest::start(table, hosted)
            }
            $crate::entry::launch(&MANIFEST, $crate::launch_body!(main; $($name : $ty),*))
        }
    };";
    const LAUNCH_BODY: &str = "($main:ident; $($name:tt : $ty:ty),*) => {
        |given: &mut $crate::grant::Given<'_>| {
            $( given.check::<$ty>(stringify!($name)); )*
            let filled = given.filled()?;
            ::core::result::Result::Ok($main(filled.cx() $(, filled.make::<$ty>(stringify!($name)))*))
        }
    };";
    let text = "#[bstd::main]
pub fn main(cx: &Cx, out: Out) -> Result<(), Error> {
    out.line(cx, \"Hello, world!\")?;
    Ok(())
}
";
    let mut rules: StdVec<(&'static str, Prepared<'static>)> = StdVec::new();
    for (name, body) in [("main", BSTD_MAIN), ("launch_body", LAUNCH_BODY)] {
        let stream = lex(body, Edition::E2024, Alloc::global()).expect("the rules lex");
        let def: &'static MacroDefinition<'static> = Box::leak(Box::new(MacroDefinition::rules(stream, at())));
        rules.push((name, prepare(def).expect("the rules compile")));
    }
    let rules: &'static [(&'static str, Prepared<'static>)] = Box::leak(rules.into_boxed_slice());
    let mut expander = ByName::new(rules, at(), Host, Alloc::global());
    let mut records = Records::default();
    let stats = crate::lower_file(text, Edition::E2024, &mut Unix, &mut crate::NoModule, &mut expander, &mut records, Alloc::global());
    let unclean: StdVec<_> = records.0.iter().filter(|(_, _, d)| d.starts_with("UNCLEAN")).collect();
    assert!(unclean.is_empty(), "{unclean:#?}");
    assert_eq!((stats.macro_calls_expanded, stats.macro_calls_failed), (1, 0), "{:#?}", records.0);
    let sig = |head: &str| {
        records
            .0
            .iter()
            .any(|(_, w, d)| *w == crate::What::Signature && d.starts_with(head))
    };
    assert!(sig("(sig FN main (ty (path $crate entry Exit)))") || records.0.iter().any(|(_, _, d)| d.contains("(sig FN main") && d.contains("(path $crate entry Exit)")), "{:#?}", records.0);
    assert!(records.0.iter().any(|(_, _, d)| d.contains("(sig FN bstd_hosted_start")), "{:#?}", records.0);
    // The written `main`, `hosted` and the process's `main`, each a body.
    let bodies: StdVec<&String> = records.0.iter().filter(|(_, w, _)| *w == crate::What::Body).map(|(_, _, d)| d).collect();
    assert!(bodies.iter().any(|d| d.contains("line") && d.contains("\"Hello, world!\"")), "{bodies:#?}");
    assert!(bodies.iter().any(|d| d.contains("(path $crate guest finish)")), "{bodies:#?}");
    assert!(bodies.iter().any(|d| d.contains("(path $crate entry launch)")), "{bodies:#?}");
}

#[test]
fn a_node_ending_in_a_call_has_a_site_to_the_calls_end() {
    // tempo's census: 379 bodies refused "text Malformed" at `.map_err(|e| format!(..))`, an
    // item node whose last child is a call's expansion.
    let text = "fn f() -> u32 { let g = |e: u32| double!(e); 1 + double!(2) }";
    let l = expanding(&[DOUBLE], text);
    let v = l.view();
    let mut sites: StdVec<&str> = StdVec::new();
    for i in 0..v.exprs.len() as u32 {
        let r = v.expr_range(ExprId(i)).expect("each expression has a range");
        let site = v.site(r).expect("each range has a place in the item");
        sites.push(&text[site.start as usize..site.end as usize]);
    }
    assert!(sites.contains(&"|e: u32| double!(e)"), "{sites:?}");
    assert!(sites.contains(&"1 + double!(2)"), "{sites:?}");
}

#[test]
fn an_expanded_bodys_ranges_read_their_own_bytes() {
    let long = ("long", "() => { 1000001 + 2000002 + double!(3000003) + 4000004 }");
    let text = "fn f() -> u32 { long!() }";
    let l = expanding(&[DOUBLE, long], text);
    let v = l.view();
    assert!(v.expanded().len() > text.len(), "the expansions outgrow the item");
    let mut lits: StdVec<&str> = StdVec::new();
    for i in 0..v.exprs.len() as u32 {
        let r = v.expr_range(ExprId(i)).expect("each expression has a range");
        let s = v.text_at(r).expect("each range is a text of the record");
        let site = v.site(r).expect("each range has a place in the item");
        let at = &text[site.start as usize..site.end as usize];
        if r.in_expansion() {
            assert!(at.starts_with("long!"), "a made node sits at the outermost call: {at:?} for {s:?}");
        } else {
            assert_eq!(at, s, "an item node is at its own text");
        }
        if matches!(v.expr(ExprId(i)), Ok(Expr::Lit(_))) {
            lits.push(s);
        }
    }
    lits.sort_unstable();
    assert_eq!(lits, ["1000001", "2", "2000002", "3000003", "4000004"]);
    // The same range through another record's view: refused, not read as its own.
    let other_text = "fn ff() -> u32 { long!() }";
    let other = expanding(&[DOUBLE, long], other_text);
    let r = v.expr_range(ExprId(0)).expect("a range");
    assert!(matches!(other.view().text_at(r), Err(crate::TextAt::OtherView(_))));
    // A record opened with a text other than its own: refused once, at the open.
    let record = l.columns.to_record(Alloc::global());
    assert!(matches!(crate::open(&record, other_text), Err(crate::Open::ItemText { .. })));
    let reopened = crate::open(&record, text).expect("the record opens with its own text");
    let r = reopened.expr_range(ExprId(0)).expect("a range");
    assert_eq!(reopened.text_at(r), v.text_at(v.expr_range(ExprId(0)).expect("a range")));
}

/// The language's derives, every one bstd and blaze-core use, on the four shapes the Reference
/// names (a struct, a tuple struct, an enum with data and a `#[default]` unit variant, a
/// generic struct): each expands to its impl, lowered clean with its methods' bodies.
const SHAPES: &str = "
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct S { a: u8, b: u16 }

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct T(u8, u16);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum E { A(u8), B { x: u16 }, #[default] C }

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct G<X> { a: X }
";

fn derived(text: &str) -> (crate::FileStats, Records) {
    let rules: &'static [(&'static str, Prepared<'static>)] = &[];
    let mut expander = ByName::new(rules, at(), Host, Alloc::global());
    let mut records = Records::default();
    let stats = crate::lower_file(text, Edition::E2024, &mut Unix, &mut crate::NoModule, &mut expander, &mut records, Alloc::global());
    (stats, records)
}

/// `#[cfg_attr(pred, derive(..))]` runs its derives when `pred` holds under the unit's cfg set
/// (here `unix`), and none when it does not; a nested `cfg_attr` too.
#[test]
fn a_cfg_attr_derive_runs_under_the_cfg_set() {
    let text = "
#[cfg_attr(unix, derive(Clone))]
#[cfg_attr(windows, derive(Debug))]
#[cfg_attr(all(), cfg_attr(unix, derive(PartialEq)))]
struct S { a: u8 }
";
    let (stats, records) = derived(text);
    assert_eq!((stats.macro_calls_expanded, stats.macro_calls_unsupported, stats.macro_calls_failed), (2, 0, 0));
    let heads: StdVec<&str> = records
        .0
        .iter()
        .filter(|(_, w, _)| *w == crate::What::Signature)
        .filter_map(|(t, _, _)| t.strip_prefix("# [ automatically_derived ] impl "))
        .map(|t| t.split(" {").next().unwrap_or(t).trim_end())
        .collect();
    let mut heads = heads;
    heads.sort_unstable();
    assert_eq!(
        heads,
        ["$crate :: clone :: Clone for S", "$crate :: cmp :: PartialEq for S", "$crate :: marker :: StructuralPartialEq for S"]
    );
}

/// A derive sees its item as rustc hands it over: a field or variant whose `#[cfg]` is false
/// is gone, a `cfg_attr` that holds leaves its attribute, and no `#[derive]` is left: `Debug`
/// names only what is left.
#[test]
fn a_derive_sees_its_item_cfg_stripped() {
    let text = "
#[derive(Debug)]
#[cfg_attr(unix, derive(Clone))]
struct S {
    a: u8,
    #[cfg(windows)]
    gone: u16,
    #[cfg_attr(unix, serde(rename = \"bee\"))]
    #[cfg(unix)]
    b: u32,
}

#[derive(Debug)]
enum E {
    #[cfg(windows)]
    Gone(u8),
    Kept { #[cfg(windows)] x: u8, y: u8 },
}
";
    let (stats, records) = derived(text);
    assert_eq!((stats.macro_calls_expanded, stats.macro_calls_unsupported, stats.macro_calls_failed), (3, 0, 0));
    let unclean: StdVec<_> = records.0.iter().filter(|(_, _, d)| d.starts_with("UNCLEAN")).collect();
    assert!(unclean.is_empty(), "{unclean:#?}");
    let fmt = |of: &str| {
        records
            .0
            .iter()
            .find(|(t, w, _)| *w == crate::What::Body && t.starts_with("fn fmt") && t.contains(of))
            .map(|(t, _, _)| t.clone())
            .unwrap_or_else(|| panic!("no fmt of {of}: {:#?}", records.0))
    };
    let s = fmt("\"S\"");
    assert!(s.contains("S { a : a_self , b : b_self , }"), "{s}");
    assert!(!s.contains("gone"), "{s}");
    let e = fmt("E :: Kept");
    assert!(e.contains("E :: Kept { y : y_self , }"), "{e}");
    assert!(!e.contains("Gone") && !e.contains("x_self"), "{e}");
    // Clone (from the `cfg_attr`) sees the same item: two fields.
    let clone = records
        .0
        .iter()
        .find(|(t, w, _)| *w == crate::What::Body && t.contains("fn clone"))
        .map(|(t, _, _)| t.clone())
        .expect("S's clone");
    assert!(clone.contains("S { a : a_self , b : b_self , }") && !clone.contains("gone"), "{clone}");
}

/// Each derive's impl header as the Reference specifies it: no bound on a type without
/// parameters, and on `G<X>` the bound `X: Trait` added per type parameter. `PartialEq` also
/// makes `StructuralPartialEq`, unbounded.
#[test]
fn every_builtin_derive_is_its_impl_on_each_shape() {
    let (stats, records) = derived(SHAPES);
    assert_eq!((stats.macro_calls_expanded, stats.macro_calls_unsupported, stats.macro_calls_failed), (36, 0, 0));
    let unclean: StdVec<_> = records.0.iter().filter(|(_, _, d)| d.starts_with("UNCLEAN")).collect();
    assert!(unclean.is_empty(), "{unclean:#?}");
    let heads: StdVec<&str> = records
        .0
        .iter()
        .filter(|(_, w, _)| *w == crate::What::Signature)
        .filter_map(|(t, _, _)| t.strip_prefix("# [ automatically_derived ] impl "))
        .map(|t| t.split(" {").next().unwrap_or(t).trim_end())
        .collect();
    let traits = [
        "$crate :: clone :: Clone",
        "$crate :: marker :: Copy",
        "$crate :: fmt :: Debug",
        "$crate :: default :: Default",
        "$crate :: cmp :: PartialEq",
        "$crate :: cmp :: Eq",
        "$crate :: cmp :: PartialOrd",
        "$crate :: cmp :: Ord",
        "$crate :: hash :: Hash",
    ];
    let mut want: StdVec<String> = StdVec::new();
    for ty in ["S", "T", "E"] {
        want.extend(traits.iter().map(|tr| format!("{tr} for {ty}")));
        want.push(format!("$crate :: marker :: StructuralPartialEq for {ty}"));
    }
    want.extend(traits.iter().map(|tr| format!("< X : {tr} , > {tr} for G < X , >")));
    want.push(String::from("< X , > $crate :: marker :: StructuralPartialEq for G < X , >"));
    let mut got: StdVec<String> = heads.iter().map(|h| String::from(*h)).collect();
    got.sort_unstable();
    want.sort_unstable();
    assert_eq!(got, want);

    // Bodies, one per derive, across the shapes: the item's text as the derive writes it.
    let body = |start: &str, of: &str| {
        let hit: StdVec<&String> = records
            .0
            .iter()
            .filter(|(t, w, _)| *w == crate::What::Body && t.starts_with(start) && t.contains(of))
            .map(|(t, _, _)| t)
            .collect();
        assert_eq!(hit.len(), 1, "{start} … {of}: {:#?}", records.0);
        hit[0].clone()
    };
    // Clone of a Copy type without parameters is `*self` behind field asserts; with a type
    // parameter, field by field.
    assert_eq!(
        body("# [ inline ] fn clone", "G {"),
        "# [ inline ] fn clone ( & self ) -> Self { match self { G { a : a_self , } => G { a : $crate :: clone :: Clone :: clone ( a_self ) , } , } }"
    );
    let star = "# [ inline ] fn clone ( & self ) -> Self { let _ : $crate :: clone :: AssertParamIsClone < u8 >; let _ : $crate :: clone :: AssertParamIsClone < u16 >; * self }";
    assert_eq!(records.0.iter().filter(|(t, w, _)| *w == crate::What::Body && t == star).count(), 3, "S, T and E");
    // Debug: a struct variant through `debug_struct`, a tuple one through `debug_tuple`, a
    // unit one written.
    let debug = body("fn fmt", "E :: C");
    for part in [
        "E :: A ( f0_self , ) => { let mut ra_expand_builder = $crate :: fmt :: Formatter :: debug_tuple ( f , \"A\" ) ;",
        "$crate :: fmt :: DebugStruct :: field ( & mut ra_expand_builder , \"x\" , & x_self ) ;",
        "E :: C => $crate :: fmt :: Formatter :: write_str ( f , \"C\" ) ,",
    ] {
        assert!(debug.contains(part), "{part}: {debug}");
    }
    // Default: the `#[default]` variant; each field's default.
    assert_eq!(body("fn default", "E :: C"), "fn default ( ) -> Self { E :: C }");
    assert_eq!(
        body("fn default", "T ("),
        "fn default ( ) -> Self { T ( $crate :: default :: Default :: default ( ) , $crate :: default :: Default :: default ( ) , ) }"
    );
    // PartialEq on an enum: the discriminants first, then the fields of the same variant.
    let eq = body("fn eq", "E :: A");
    assert!(eq.contains("__self_discr == __arg1_discr && match ( self , other ) {"), "{eq}");
    assert!(eq.contains("_unused => true }"), "{eq}");
    // PartialOrd on a struct: lexicographic over fields, the first unequal answer returned.
    let partial_cmp = body("fn partial_cmp", "S {");
    assert!(
        partial_cmp.contains(
            "match $crate :: cmp :: PartialOrd :: partial_cmp ( & a_self , & a_other ) { $crate :: option :: Option :: Some ( $crate :: cmp :: Ordering :: Equal ) => { match $crate :: cmp :: PartialOrd :: partial_cmp ( & b_self , & b_other )"
        ),
        "{partial_cmp}"
    );
    // Ord on an enum: the discriminants' order, then the fields.
    let cmp = body("fn cmp", "E :: A");
    assert!(cmp.starts_with("fn cmp ( & self , other : & Self ) -> $crate :: cmp :: Ordering { match $crate :: cmp :: Ord :: cmp ( & $crate :: intrinsics :: discriminant_value ( self ) , & $crate :: intrinsics :: discriminant_value ( other ) )"), "{cmp}");
    // Hash on a tuple struct: each field in order.
    assert_eq!(
        body("fn hash", "T ("),
        "fn hash < __H : $crate :: hash :: Hasher > ( & self , ra_expand_state : & mut __H ) { match self { T ( f0_self , f1_self , ) => { $crate :: hash :: Hash :: hash ( f0_self , ra_expand_state ) ; $crate :: hash :: Hash :: hash ( f1_self , ra_expand_state ) ; } , } }"
    );
    // Hash on an enum hashes the discriminant first.
    assert!(body("fn hash", "E :: A").contains("{ $crate :: hash :: Hash :: hash ( & $crate :: mem :: discriminant ( self ) , ra_expand_state ) ; match self"));
}

/// Proc macros by the caller's [`crate::by_name::MacroNames`], after [`crate::by_name::Lists`]: `Marker` (a derive), `wrap` (an
/// attribute) and `seven` (function-like), each answered by the host as a wasm module's run
/// would be. Every token is made at the call site, except the attribute's item, passed on.
struct ProcHost;

impl ExpandCx for ProcHost {
    fn env(&self, _: &str) -> Result<Option<&str>, HostError> {
        Ok(None)
    }
    fn cfg(&self, _: &TokenStream<'_>) -> Result<bool, HostError> {
        Ok(false)
    }
    fn include(&self, _: &str, _: IncludeAs) -> Result<Included<'_>, HostError> {
        Ok(Included::Missing)
    }
    fn place(&self, _: PlaceOf) -> Result<Place<'_>, HostError> {
        Ok(Place::Number(1))
    }
    fn run_proc_macro<'a>(&self, def: &MacroDefinition<'_>, call: ProcCall<'_>, alloc: Alloc<'a>) -> Result<Produced<'a>, HostError> {
        let failed = HostError { ask: HostAsk::ProcMacro, why: HostWhy::Failed };
        let made = |text: &str| -> Result<(TokenStream<'a>, blaze_own::AVec<'a, blaze_expand::TokenOrigin>), HostError> {
            let tokens = lex(text, Edition::E2024, alloc).map_err(|_| failed)?;
            let mut origins = blaze_own::Vec::new_in(alloc);
            origins.extend((0..tokens.len()).map(|_| blaze_expand::TokenOrigin::Made(blaze_expand::Transparency::Transparent)));
            Ok((tokens, origins))
        };
        let (tokens, origins) = match def.proc_name() {
            Some("Marker") => made("impl Marker for S { fn marker(&self) -> u32 { 7 } }")?,
            Some("seven") => made("7")?,
            Some("wrap") => {
                let tokens = call.input.copy_in(0..call.input.len(), alloc).map_err(|_| failed)?;
                let mut origins = blaze_own::Vec::new_in(alloc);
                origins.extend((0..tokens.len() as u32).map(blaze_expand::TokenOrigin::Input));
                (tokens, origins)
            }
            _ => return Err(failed),
        };
        Ok(Produced {
            tokens,
            origins,
            reads: blaze_expand::Reads::new_in(alloc),
            diagnostics: blaze_own::Vec::new_in(alloc),
            text: blaze_own::Vec::new_in(alloc),
        })
    }
}

struct Procs;

impl<'d> crate::by_name::MacroNames<'d> for Procs {
    fn resolve<'a>(
        &self,
        path: &str,
        _: crate::by_name::PathAt,
        kind: blaze_expand::ProcKind,
        alloc: Alloc<'a>,
    ) -> Result<crate::by_name::Resolved<'d, 'a>, crate::by_name::NoMacro<'a>>
    where
        'd: 'a,
    {
        let name = path.rsplit(':').next().ok_or(crate::by_name::NoMacro::NotAPath)?;
        let known = matches!(
            (name, kind),
            ("Marker", blaze_expand::ProcKind::Derive) | ("wrap", blaze_expand::ProcKind::Attr) | ("seven", blaze_expand::ProcKind::FnLike)
        );
        let krate = CrateItemKey(ContentId([2; 32]));
        known
            .then(|| MacroDefinition::proc_macro(krate, kind, name, &[], at(), alloc).ok())
            .flatten()
            .map(crate::by_name::Resolved::Def)
            .ok_or(crate::by_name::NoMacro::NotListed)
    }
}

/// A derive, an attribute and a body's function-like call, each a proc macro the expander
/// resolved through `MacroNames`: the derive's impl and the attribute's item are lowered as
/// items, and the call's `7` is the body's value; a name `MacroNames` does not know stays
/// unsupported.
#[test]
fn proc_macros_by_their_names_expand_into_items_and_bodies() {
    let text = "#[derive(Marker)]
struct S;

#[wrap]
fn f() -> u32 { seven!() }

#[unknown_attr]
fn g() {}
";
    let lists = crate::by_name::Lists { rules: &[], builtin_at: at(), then: Some(&Procs) };
    let mut expander = ByName::with_names(&lists, ProcHost, Alloc::global());
    let mut records = Records::default();
    let stats = crate::lower_file(text, Edition::E2024, &mut Unix, &mut crate::NoModule, &mut expander, &mut records, Alloc::global());
    assert_eq!((stats.macro_calls_expanded, stats.macro_calls_failed), (2, 0), "{stats:#?}");
    assert_eq!(stats.body_calls_expanded, 1, "{stats:#?}");
    let unclean: StdVec<_> = records.0.iter().filter(|(_, _, d)| d.starts_with("UNCLEAN")).collect();
    assert!(unclean.is_empty(), "{unclean:#?}");
    assert!(records.0.iter().any(|(t, _, _)| t.contains("fn marker")), "{:#?}", records.0);
    assert!(records.0.iter().any(|(t, _, d)| t.contains("fn f") && d.contains('7')), "{:#?}", records.0);
}

/// A body's `cfg!` is answered by the cfg set the body is lowered under, the one its `#[cfg]`s
/// get, never by the expander's host. core's `cfg!` (library/core/src/macros/mod.rs): "The
/// syntax given to this macro is the same syntax as the [`cfg`] attribute. `cfg!`, unlike
/// `#[cfg]`, does not remove any code and only evaluates to true or false." The tests' `Host`
/// answers every `cfg!` `false` and the body's set (`Unix`) answers `unix`: the call is `true`.
/// front2's host refuses every `cfg!`: that refusal stopped core's `panic_bounds_check`
/// (`cfg!(panic = "immediate-abort")`) and 665 bodies of core, alloc and bstd.
#[test]
fn a_body_s_cfg_is_answered_by_the_body_s_cfg_set_not_the_host() {
    let l = expanding(&[], "fn f() -> bool { #[cfg(unix)] let a = cfg!(unix); cfg!(any(unix, windows)) && a }");
    let v = l.view();
    assert_eq!(v.unhandled().count(), 0, "{}", dump_of(&l));
    let dump = dump_of(&l);
    assert!(!dump.contains("false"), "a `cfg!` the body's set answers true expanded to `false`:\n{dump}");
    assert!(dump.contains("true"), "{dump}");
}

/// A `panic!` as a match arm: core's `panic_2021` (library/core/src/panic.rs) is
/// `($($t:tt)+) => ({ $crate::panicking::panic_fmt($crate::const_format_args!($($t)+)); })`,
/// a block whose one statement diverges (the Reference, "Block expressions": "the type of the
/// block is the type of the final operand, or `()` if the final operand is omitted"; "Never
/// type": "Expressions of type `!` can be coerced into any other type"). The arm's body is that
/// block, with no tail and one statement, and every node of it has a place: the call's range.
/// bstd's `None => panic!(..)` arms (cx.rs `cancellation`) came to the checker as
/// "DiagWithoutPlace { code: Mismatch, name: None }".
#[test]
fn a_panic_arm_is_its_block_at_the_call_s_range() {
    let panic = ("p", "($($t:tt)+) => ({ stop(format_args!($($t)+)); })");
    let text = "fn f(x: u32) -> u32 { match x { 0 => 1, _ => p!(\"no {x} {}\", x) } }";
    let l = expanding(&[panic], text);
    let v = l.view();
    assert_eq!(v.unhandled().count(), 0, "{}", dump_of(&l));
    let call = text.find("p!(").expect("the call");
    let call = &text[call..call + "p!(\"no {x} {}\", x)".len()];
    let mut arms = 0;
    for i in 0..v.exprs.len() as u32 {
        let Ok(Expr::Match { arms: list, .. }) = v.expr(ExprId(i)) else { continue };
        let rows = v.rows(list).expect("the arms");
        let arm = Arm::decode(rows[1]).expect("an arm");
        let Ok(Expr::Block { stmts, tail, .. }) = v.expr(arm.body) else {
            panic!("the arm is {:?}\n{}", v.expr(arm.body), dump_of(&l))
        };
        assert_eq!((stmts.len, tail), (1, None), "{}", dump_of(&l));
        let site = v.site(v.expr_range(arm.body).expect("a range")).expect("a place");
        assert_eq!(&text[site.start as usize..site.end as usize], call);
        arms += 1;
    }
    assert_eq!(arms, 1);
    // Every node has a place in the item (an empty one is a diagnostic without a place).
    for i in 0..v.exprs.len() as u32 {
        let site = v.site(v.expr_range(ExprId(i)).expect("a range")).expect("a place");
        assert!(site.end > site.start, "expression {i} {:?} has no place: {site:?}\n{}", v.expr(ExprId(i)), dump_of(&l));
    }
    for i in 0..v.pats.len() as u32 {
        let site = v.site(v.pat_range(PatId(i)).expect("a range")).expect("a place");
        assert!(site.end > site.start, "pattern {i} has no place: {site:?}\n{}", dump_of(&l));
    }
}

/// core's `quicksort::inst_partition` (library/core/src/slice/sort/unstable/quicksort.rs:139):
/// a braced `cfg_select!` as the only content of an `if` block, beside an inner `const` item,
/// the one call of core whose `cfg_select!` still reached the host ("Host(HostError { ask:
/// Cfg, why: Refused })"). core's `cfg_select!` doc: "the first arm whose predicate is true
/// expands; `_` matches anything".
#[test]
fn a_braced_cfg_select_in_a_block_is_answered_by_the_body_s_cfg_set() {
    let text = "const fn f() -> u32 {
    const MAX: usize = 96;
    if 1 <= MAX {
        cfg_select! {
            feature = \"optimize_for_size\" => 1,
            _ => 2,
        }
    } else {
        3
    }
}";
    let rules: &'static [(&'static str, Prepared<'static>)] = &[];
    let mut expander = ByName::new(rules, at(), Host, Alloc::global());
    let Ok(l) = lower_body_expanding(text, Edition::E2024, &mut Unix, &mut crate::NoModule, &mut expander, Alloc::global()) else { panic!("needs a cfg") };
    let f = expander.findings();
    assert!(f.is_empty(), "{f:#?}\n{}", dump_of(&l));
    assert_eq!(l.view().unhandled().count(), 0, "{}", dump_of(&l));
}

/// core's `cfg_select!` (core/src/macros/mod.rs: "Selects code at compile-time based on `cfg`
/// predicates ... the first arm whose predicate evaluates to true is selected") in item
/// position is answered by the set the file is lowered under, the one answer its `#[cfg]`s get
/// ([`crate::cfg_outcome`]), never by the expander's host: [`Host`] says every predicate is
/// false, `Unix` says `unix` holds, so the `unix` arm's item is the one made.
#[test]
fn an_item_position_cfg_select_is_answered_by_the_file_s_cfg_set() {
    let text = "cfg_select! {
    unix => { fn on_unix() {} }
    _ => { fn elsewhere() {} }
}
";
    let rules: &'static [(&'static str, Prepared<'static>)] = &[];
    let mut expander = ByName::new(rules, at(), Host, Alloc::global());
    let mut records = Records::default();
    let stats = crate::lower_file(text, Edition::E2024, &mut Unix, &mut crate::NoModule, &mut expander, &mut records, Alloc::global());
    assert_eq!((stats.macro_calls_expanded, stats.macro_calls_unsupported, stats.macro_calls_failed), (1, 0, 0));
    let made: StdVec<&String> = records.0.iter().filter(|(_, w, _)| *w == crate::What::Signature).map(|(t, _, _)| t).collect();
    assert!(made.iter().any(|t| t.contains("on_unix")) && !made.iter().any(|t| t.contains("elsewhere")), "{made:#?}");
}

/// std's `current_os_id` (library/std/src/sys/thread/unix.rs:349), cut to two arms: a braced
/// `cfg_select!` that is the fn body's TAIL, its taken arm a block of statements and a tail.
/// core's doc (core/src/macros/mod.rs): "This macro evaluates, at compile-time, a series of
/// `cfg` predicates, selects the first that is true, and emits the code guarded by that
/// predicate. The code guarded by other predicates is not emitted." The Reference, "Macros":
/// "When used as an item or a statement, the MacroInvocationSemi form is used where a semicolon
/// is required at the end when not using curly braces": a braced call ending a block is a
/// statement, so its expansion is statements whose last expression is the block's tail. Read
/// as an expression, `let mut id = 0u64; ..` was a `Syntax` diagnostic over the call (13
/// library fns, "things its lowering did not take"). `Unix` says `unix` holds and
/// `target_os = "linux"` does not, so the linux arm, and its `syntax!` call, never reach the
/// expander (no rules: a call that reached it would be a `NoMacro` finding).
/// Expected: passes; the body's block has 2 statements and the `if` as its tail.
#[test]
fn an_expression_cfg_select_with_a_block_arm_lowers() {
    let text = "fn current_os_id() -> Option<u64> {
    cfg_select! {
        any(target_os = \"linux\") => {
            syntax!(fn g() -> i32;);
            let id: i32 = 1;
            Some(id as u64)
        }
        unix => {
            let mut id = 0u64;
            id += 7;
            if id == 7 { Some(id) } else { None }
        }
        _ => None,
    }
}";
    let rules: &'static [(&'static str, Prepared<'static>)] = &[];
    let mut expander = ByName::new(rules, at(), Host, Alloc::global());
    let Ok(l) = lower_body_expanding(text, Edition::E2024, &mut Unix, &mut crate::NoModule, &mut expander, Alloc::global()) else { panic!("needs a cfg") };
    let f = expander.findings();
    assert!(f.is_empty(), "{f:#?}\n{}", dump_of(&l));
    let v = l.view();
    assert_eq!(v.unhandled().count(), 0, "{}", dump_of(&l));
    // The unix arm's statements are the body's own: two statements and the `if` as the tail
    // (an arm read as one expression, or the `_` arm taken, has no statements).
    let mut found = false;
    for i in 0..v.exprs.len() as u32 {
        if let Ok(Expr::Block { stmts, tail: Some(tail), .. }) = v.expr(ExprId(i))
            && stmts.len == 2
            && matches!(v.expr(tail), Ok(Expr::If { .. }))
        {
            found = true;
        }
    }
    assert!(found, "no block of 2 statements and an `if` tail\n{}", dump_of(&l));
}

/// The expanded `panic!` shapes, checked: a diverging block made by a macro is `!` as an arm,
/// as a `-> !` body's statement and as its tail, with no diagnostic (the type checker's
/// `a_macro_block_that_diverges_is_never` is the same shapes written by hand: clean).
#[test]
fn an_expanded_panic_block_checks_as_never() {
    use blaze_typeck::{InferCtx, Needs, Outcome, PortableTys, check};
    struct NoTypes;
    impl crate::typeck::Types for NoTypes {
        fn ty(&mut self, _: &View<'_>, _: crate::form::TypeId, _: &mut PortableTys<'_>) -> Result<blaze_typeck::PTy, crate::typeck::Refused> {
            Err(crate::typeck::Refused::Form { what: "a written type", range: Default::default() })
        }
        fn const_arg(&mut self, view: &View<'_>, c: ConstIdx, _: &mut blaze_typeck::PortableTys<'_>) -> Result<blaze_typeck::PTy, crate::typeck::Refused> {
            Err(crate::typeck::no_const_arg(view, c))
        }
        fn repeat_len(&mut self, _: &View<'_>, _: ExprId) -> Result<u64, crate::typeck::Refused> {
            Err(crate::typeck::Refused::Form { what: "a repeat length", range: Default::default() })
        }
    }
    let mut fx = blaze_typeck::fixture::Fx::new();
    let (u32_, never) = (fx.u32(), fx.never());
    fx.func("stop", 0, &[], never, StdVec::new());
    let f = fx.func("f", 0, &[u32_], u32_, StdVec::new());
    let g = fx.func("g", 0, &[], never, StdVec::new());
    fx.finish();
    let panic = ("p", "() => ({ stop(); })");
    for (owner, text) in [
        (f, "fn f(x: u32) -> u32 { match x { 0 => 1, _ => p!() } }"),
        (g, "fn g() -> ! { p!(); }"),
        (g, "fn g() -> ! { p!() }"),
    ] {
        let l = expanding(&[panic], text);
        let v = l.view();
        assert_eq!(v.unhandled().count(), 0, "{}", dump_of(&l));
        let alloc = Alloc::global();
        let mut tys = PortableTys::new_in(alloc);
        let body = match crate::typeck::to_typeck(&v, owner, &mut tys, &mut NoTypes, alloc) {
            Ok(b) => b,
            Err(e) => panic!("to_typeck refused {e:?} in\n{text}"),
        };
        let bv = body.view(tys.view());
        let mut cx = InferCtx::new_in(alloc);
        match check(&bv, &fx, &mut cx, &Needs::new_in(alloc), alloc) {
            Ok(Outcome::Done(c)) => assert!(c.diags.is_empty(), "{text}: {:?}\n{}", c.diags, dump_of(&l)),
            Ok(Outcome::Need(n)) => panic!("{text}: the world missed {:?}", n.questions),
            Err(e) => panic!("{text}: check refused {e:?}\n{}", dump_of(&l)),
        }
    }
}

/// A body call whose path names a macro and whose expansion fails is a finding naming its path
/// and the expansion's own typed error (here the call's `compile_error!` message), never a bare
/// "failed". core's `compile_error!`: "Causes compilation to fail with the given error message
/// when encountered."
#[test]
fn a_failed_body_call_is_a_finding_naming_its_path_and_error() {
    let rules: &'static [(&'static str, Prepared<'static>)] = &[];
    let mut expander = ByName::new(rules, at(), Host, Alloc::global());
    let text = "fn f() { compile_error!(\"no way\") }";
    let Ok(_) = lower_body_expanding(text, Edition::E2024, &mut Unix, &mut crate::NoModule, &mut expander, Alloc::global()) else { panic!("needs a cfg") };
    let f = expander.findings();
    assert_eq!(f.len(), 1, "{f:#?}");
    assert_eq!(f[0].path.as_str(), "compile_error");
    assert_eq!(format!("{}", f[0].why), "its expansion's error CompileError: `no way`");
}

/// core's built-in `pattern_type!` (library/core/src/pat.rs: "Creates a pattern type.
/// `type Positive = std::pat::pattern_type!(i32 is 1..);`") expands in a body's `let` to the
/// one form `TypeRef::Pat`: its base `u8` and its pattern the range `1..`; core's `NonNull`
/// pattern `!null` (`pattern_type!(*const T is !null)`) to `TyPat::NotNull`. No macro call
/// is left and nothing is counted unhandled.
#[test]
fn a_pattern_type_expands_to_its_one_form() {
    let l = expanding(&[], "fn f() { let a: pattern_type!(u8 is 1..) = x; let b: pattern_type!(*const u8 is !null) = y; }");
    let v = l.view();
    let tys: StdVec<TypeRef> = (0..v.types.len()).map(|i| v.ty(TypeId(i as u32)).expect("a type row")).collect();
    assert!(!tys.iter().any(|t| matches!(t, TypeRef::Macro(_) | TypeRef::Error)), "{tys:?}");
    let pats: StdVec<TypeRef> = tys.iter().copied().filter(|t| matches!(t, TypeRef::Pat { .. })).collect();
    assert_eq!(pats.len(), 2, "{tys:?}");
    match pats[0] {
        TypeRef::Pat { base, pat: TyPat::Of(p) } => {
            assert!(matches!(v.ty(base), Ok(TypeRef::Path(_))), "{:?}", v.ty(base));
            assert!(matches!(v.pat(p), Ok(Pat::Range { lo: Some(_), hi: None, inclusive: false })), "{:?}", v.pat(p));
        }
        other => panic!("{other:?}"),
    }
    match pats[1] {
        TypeRef::Pat { base, pat: TyPat::NotNull } => assert!(matches!(v.ty(base), Ok(TypeRef::Ptr { .. })), "{:?}", v.ty(base)),
        other => panic!("{other:?}"),
    }
}

fn label_diags(defs: &[(&'static str, &str)], text: &str) -> StdVec<DiagKind> {
    let l = expanding(defs, text);
    let v = l.view();
    let unhandled: StdVec<_> = v.unhandled().collect();
    assert!(unhandled.is_empty(), "{text}: unhandled {unhandled:?}\n{}", dump_of(&l));
    v.diags().filter_map(Result::ok).map(|d| d.kind).collect()
}

/// A label written at the call site keeps the call site's hygiene through the expansion; one
/// the macro writes does not see the caller's. The Reference, "Macros By Example", Hygiene:
/// "Macros by example have mixed-site hygiene. This means that loop labels, block labels,
/// and local variables are looked up at the macro definition site while other symbols are
/// looked up at the macro invocation site." A label token keeps the site that wrote it: one
/// the caller wrote and passed in is looked up at the caller's. So a `break 'outer` passed in
/// as `$b` is the caller's, and leaves the caller's loop (tempo's `'event_loop: loop {
/// tokio::select! { .. => { break 'event_loop; } } }`); a `break 'outer` in the macro's own
/// body is looked up at the definition, where no loop declares it: `UndeclaredLabel`.
/// The cause: an expansion's `'outer` is two tokens, `'` and `outer`, and one parser word;
/// the word began at the name, so the lowering read the label as `outer`, not `'outer`, and
/// never found the caller's loop. Expected: `test expand_tests::a_callers_label_resolves_through_a_macro ... ok`.
#[test]
fn a_callers_label_resolves_through_a_macro() {
    // (1) Passed in as a block: the caller's label.
    let pass = ("pass", "($b:block) => { $b }");
    let kinds = label_diags(&[pass], "fn f() { 'outer: loop { pass!({ break 'outer; }) } }");
    assert!(kinds.is_empty(), "the caller's `break 'outer` is the caller's: {kinds:?}");
    // Passed through a nested call as an expression, as `tokio::select!` passes `$handle`.
    let outer = ("again", "($e:expr) => { pass!({ $e }) }");
    let kinds = label_diags(&[pass, outer], "fn f() -> u8 { 'outer: loop { again!(break 'outer 1) } }");
    assert!(kinds.is_empty(), "through two expansions: {kinds:?}");
    // (2) Written by the macro: its `'outer` is not the caller's.
    let brk = ("brk", "() => { break 'outer; }");
    let kinds = label_diags(&[brk], "fn f() { 'outer: loop { brk!(); } }");
    assert_eq!(kinds, vec![DiagKind::UndeclaredLabel], "the macro's own `'outer` is hygienic");
    // A macro's own loop and label see each other.
    let spin = ("spin", "() => { 'l: loop { break 'l; } }");
    let kinds = label_diags(&[spin], "fn f() { spin!(); }");
    assert!(kinds.is_empty(), "the macro's label is its own: {kinds:?}");
    // (3) A label passed as `$l:lifetime` and used by the expansion: the caller's.
    let to = ("to", "($l:lifetime) => { break $l; }");
    let kinds = label_diags(&[to], "fn f() { 'outer: loop { to!('outer); } }");
    assert!(kinds.is_empty(), "a `$l:lifetime` is the caller's label: {kinds:?}");
    // And a nested call whose input ends in a lifetime takes the whole `'outer`.
    // (`to!($l);` with its `;`: a statement call, so `to!`'s `break $l;` is read as statements;
    // without it the call is the expansion's tail expression and the `;` in `to!`'s body is
    // not an expression's.)
    let fwd = ("fwd", "($l:lifetime) => { to!($l); }");
    let kinds = label_diags(&[to, fwd], "fn f() { 'outer: loop { fwd!('outer); } }");
    assert!(kinds.is_empty(), "a lifetime at a nested call's end is whole: {kinds:?}");
}

/// `inner!`, a `macro_rules!` that takes an expression after `k:` (as tracing's `callsite2!`
/// takes `kind: $kind:expr`), as a body call and as an item call.
const INNER: (&str, &str) = ("inner", "(k: $k:expr) => { $k }");
const INNER_ITEM: (&str, &str) = ("inner_item", "(k: $k:expr) => { const C: u8 = $k; }");

/// `text`, the copy of an item a macro wrote (front2 locals.rs), its body lowered with `defs`
/// and a mark of the defining crate at each byte of `marked` (where `mark_site::column_of_copy`
/// starts a word's row: a `$crate`'s `$`).
fn expanding_marked<'t>(defs: &[(&'static str, &str)], text: &'t str, marked: &[u32]) -> Lowered<'static, 't> {
    let defs: StdVec<Def> = defs.iter().map(|&(name, body)| Def { name, body, macro2: false }).collect();
    let mut expander = by_name(&defs);
    let mark = crate::mark_site::TextMark { site: at().site, transparency: blaze_expand::Transparency::SemiOpaque, edition: Edition::E2024 };
    let rows: StdVec<(u32, crate::mark_site::TextMark)> = marked.iter().map(|&at| (at, mark)).collect();
    let column = crate::mark_site::column_of_copy(text, &rows, Alloc::global()).expect("the copy's column writes");
    let column: &'static [u8] = Box::leak(StdVec::from(&column[..]).into_boxed_slice());
    let (column, _) = blaze_expand::origin_column::OriginsRef::open(column).expect("the copy's column opens");
    let marks = crate::mark_site::ItemMarks::of(text, blaze_scope::crate_roots::TextMarks { text, column }).expect("the marks are of the text");
    match crate::lower_body_expanding_marked(marks, Edition::E2024, &mut Unix, &mut crate::NoModule, &mut expander, Alloc::global()) {
        Ok(l) => l,
        Err(need) => panic!("{text}: needs {:?}", need.atoms),
    }
}

/// The unhandled rows' reasons of `l`, and its dump.
fn outcome(l: &Lowered<'_, '_>) -> (StdVec<Why>, String) {
    (l.view().unhandled().map(|u| u.why).collect(), dump_of(l))
}

/// A call written in the copy of an item a macro wrote takes its input with the copy's marks
/// (by_name.rs `ByName::input`, blaze-expand `lex_marked`): the `$crate` the macro wrote is one
/// identifier, `k: $k:expr` matches, and the path stays `$crate`'s. The Reference, "Macros By
/// Example", "Hygiene": `$crate` "refers to the crate defining the macro". The real site:
/// tracing 0.1.44's `event!` writes `static __CALLSITE: .. = $crate::callsite2! { .., kind:
/// $crate::metadata::Kind::EVENT, .. }` in a fn body (tempo t1500: 14 tests NOT RUN, "Binding:
/// `expected expression`"). NEGATIVE TWIN: the same text unmarked, a `$crate` written outside
/// any macro, stays the expansion's error (rustc: "expected expression, found `$`"). CONTROL:
/// the call with no `$crate` lowers clean marked or not. Expected: `test expand_tests::
/// a_copied_items_body_call_reads_its_dollar_crate_by_its_mark ... ok`.
#[test]
fn a_copied_items_body_call_reads_its_dollar_crate_by_its_mark() {
    let text = "fn f() -> u8 { inner!{ k: $crate::K } }";
    let dollar = u32::try_from(text.find("$crate").expect("the text writes `$crate`")).expect("a short text");
    let l = expanding_marked(&[INNER], text, &[dollar]);
    let (whys, dump) = outcome(&l);
    assert!(whys.is_empty(), "{whys:?}\n{dump}");
    assert!(l.view().diags.is_empty(), "{dump}");
    assert!(dump.contains("(path $crate K)"), "{dump}");

    let l = expanding_marked(&[INNER], text, &[]);
    let (whys, dump) = outcome(&l);
    assert_eq!(whys, vec![Why::ExpansionFailed], "an unmarked `$crate` is `$` and `crate`\n{dump}");

    let plain = "fn f() -> u8 { inner!{ k: K } }";
    for marked in [&[][..], &[0u32][..]] {
        let l = expanding_marked(&[INNER], plain, marked);
        let (whys, dump) = outcome(&l);
        assert!(whys.is_empty(), "{whys:?}\n{dump}");
        assert!(dump.contains("(path K)"), "{dump}");
    }
}

/// Each start of `words` in `text`, ascending: where the copy's column starts a marked word's
/// row (every word the expansion that wrote the item made, in the real site).
fn starts_of(text: &str, words: &[&str]) -> StdVec<u32> {
    let mut at: StdVec<u32> = words.iter().flat_map(|w| text.match_indices(w).map(|(i, _)| u32::try_from(i).expect("a short text"))).collect();
    at.sort_unstable();
    at.dedup();
    at
}

/// A local of a macro-written item's text keeps its binding through an inner call's input
/// (blaze-expand `TextMark::as_outer_mark`: a re-lexed word's mark is transparent, so its local
/// hygiene is the 0 the item text's own words have, machine.rs `hygiene_at`). The Reference,
/// "Macros By Example", "Hygiene": locals are looked up at the definition site; a binder and a
/// use one expansion wrote are of one context, whatever inner call the use is handed through.
/// The real site: proptest's `proptest!` writes the test fn with `let mut config = ..;` and
/// hands `config` to `$crate::` helper calls in the same body (tempo a1630: 590 E0425
/// "cannot find value `config`" and the like, in `proptest!`, `hardfork!`, `paste!`,
/// `bitflags!` bodies, after the call input took the copy's marks).
///
/// Shapes, each with every `config` (and `$crate`) marked as the expansion wrote them:
/// (1) the outer's `let` used inside the inner call's input (with tracing's `$crate` beside it,
/// which must stay `$crate`'s); (2) the inner call's input binding a name the outer's text then
/// reads. NEGATIVE TWIN: a local the INNER macro writes is not the outer's (rustc: E0425).
/// CONTROL: the same texts unmarked (the input lexed by `lex`, as before marks were kept)
/// bind the same. Expected: `test expand_tests::
/// a_copied_items_local_keeps_its_binding_through_an_inner_calls_input ... ok`.
#[test]
fn a_copied_items_local_keeps_its_binding_through_an_inner_calls_input() {
    let id = ("id", "(k: $k:expr) => { $k }");
    let bind = ("bind", "($i:ident) => { let $i = 1; }");
    let shape1 = "fn f() -> u8 { let config = 1; id!{ k: $crate::g(config) } }";
    let shape2 = "fn f() -> u8 { bind!(config); config }";
    for (how, marked) in [("marked", true), ("control: unmarked", false)] {
        let rows = |text: &str| if marked { starts_of(text, &["config", "$crate"]) } else { StdVec::new() };
        // (1) Without marks a `$crate` is `$` and `crate` (the d1 twin): the control's shape 1
        // has no `$crate`.
        let one = if marked { String::from(shape1) } else { shape1.replace("$crate::", "") };
        let l = expanding_marked(&[id], &one, &rows(&one));
        let (whys, dump) = outcome(&l);
        assert!(whys.is_empty(), "{how} (1): {whys:?}\n{dump}");
        let v = l.view();
        let configs = binds(&v, "config");
        assert_eq!(configs.len(), 1, "{how} (1): one `let config`\n{dump}");
        assert_eq!(locals(&v), configs, "{how} (1): the inner call's `config` is the outer's `let`\n{dump}");
        if marked {
            assert!(dump.contains("(path $crate g)"), "{how} (1): `$crate` stays the defining crate's\n{dump}");
        }
        // (2)
        let l = expanding_marked(&[bind], shape2, &rows(shape2));
        let (whys, dump) = outcome(&l);
        assert!(whys.is_empty(), "{how} (2): {whys:?}\n{dump}");
        let v = l.view();
        let configs = binds(&v, "config");
        assert_eq!(configs.len(), 1, "{how} (2): the input's `config` is bound\n{dump}");
        assert_eq!(locals(&v), configs, "{how} (2): the outer's `config` reads the input's binding\n{dump}");
        // NEGATIVE TWIN: the inner macro's own `y` is hidden from the outer's `y`.
        let mk = ("mk", "() => { let y = 1; }");
        let twin = "fn f() { mk!(); y; }";
        let l = expanding_marked(&[mk], twin, &if marked { starts_of(twin, &["y", "mk"]) } else { StdVec::new() });
        let (whys, dump) = outcome(&l);
        assert!(whys.is_empty(), "{how} twin: {whys:?}\n{dump}");
        assert_eq!(binds(&l.view(), "y").len(), 1, "{how} twin: the macro's `let y`\n{dump}");
        assert!(locals(&l.view()).is_empty(), "{how} twin: the outer's `y` is no local of the macro's\n{dump}");
    }
}

/// The same seam at item position (driver.rs `macro_call`, `Expand::expand`'s `input_marks`):
/// an item macro's expansion writes an item call whose input holds the macro's `$crate`; the
/// call's input is lexed with the expansion's word marks. NEGATIVE TWIN: the item call written
/// in the file with `$crate` fails (rustc: "expected expression, found `$`"). CONTROL: an
/// expansion writing the call with no `$crate` expands both calls. Expected: `test
/// expand_tests::an_expansions_item_call_reads_its_dollar_crate_by_its_mark ... ok`.
#[test]
fn an_expansions_item_call_reads_its_dollar_crate_by_its_mark() {
    let outer = ("outer", "() => { inner_item!{ k: $crate::K } }");
    let (stats, records) = lower_items(&[outer, INNER_ITEM], "outer!{}");
    assert_eq!((stats.macro_calls_expanded, stats.macro_calls_failed), (2, 0), "{:#?}", records.0);
    let unclean: StdVec<_> = records.0.iter().filter(|(_, _, d)| d.starts_with("UNCLEAN")).collect();
    assert!(unclean.is_empty(), "{unclean:#?}");
    assert!(records.0.iter().any(|(t, _, d)| t.contains("const C") && d.contains("(path $crate K)")), "{:#?}", records.0);

    let (stats, _) = lower_items(&[INNER_ITEM], "inner_item!{ k: $crate::K }");
    assert_eq!((stats.macro_calls_expanded, stats.macro_calls_failed), (0, 1), "a `$crate` written in a file is refused");

    let plain = ("plain", "() => { inner_item!{ k: K } }");
    let (stats, records) = lower_items(&[plain, INNER_ITEM], "plain!{}");
    assert_eq!((stats.macro_calls_expanded, stats.macro_calls_failed), (2, 0), "{:#?}", records.0);
}
