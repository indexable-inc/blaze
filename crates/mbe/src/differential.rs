//! The expander against the one it replaced ([`crate::expander::model`]): the same expansion,
//! matched arm and error for every macro and input, over a corpus of the features, seeded
//! random macros and inputs, and the benchmark's real-world macros. And
//! [`crate::expect_fragment`], which parses a window of the level, against the model's, which
//! converts the whole rest of it: the same fragment, error and position after. And every input
//! and expansion parsed through the parser input made from the token trees' columns against
//! the old conversion's (`syntax_bridge::reference`): the same input, tree, errors and spans.

use std::fmt::Write;

use base_db::Db;
use blaze_ids::rng::SplitMix64;

use span::{
    Edition, EditionedFileId, FileId, ROOT_ERASED_FILE_AST_ID, Span, SpanAnchor, SyntaxContext,
};
use tt::{TextRange, TextSize};

use crate::{DeclarativeMacro, MacroCallStyle, expander};

const EDITIONS: [Edition; 3] = [Edition::Edition2015, Edition::Edition2021, Edition::Edition2024];
const STYLES: [MacroCallStyle; 3] =
    [MacroCallStyle::FnLike, MacroCallStyle::Attr, MacroCallStyle::Derive];

fn anchor(file: u32, edition: Edition) -> SpanAnchor {
    SpanAnchor {
        file_id: EditionedFileId::new(FileId::from_raw(file), edition),
        ast_id: ROOT_ERASED_FILE_AST_ID,
    }
}

fn token_tree(file: u32, edition: Edition, text: &str) -> Option<tt::TopSubtree> {
    syntax_bridge::parse_to_token_tree(
        edition,
        anchor(file, edition),
        SyntaxContext::root(Edition::CURRENT),
        text,
    )
}

fn call_site() -> Span {
    Span {
        range: TextRange::up_to(TextSize::from(1)),
        anchor: anchor(1, Edition::CURRENT),
        ctx: SyntaxContext::root(Edition::CURRENT),
    }
}

/// Expands `input` with `mac` by both expanders in every call style, with no marker and, when
/// `marked`, with one that moves every span, and asserts they agree. Returns the expansions.
fn assert_same(
    db: &Db,
    mac: &DeclarativeMacro,
    input: &tt::TopSubtree,
    marked: bool,
    what: &impl Fn() -> String,
) -> Vec<tt::TopSubtree> {
    let call_site = call_site();
    let mut expansions = Vec::new();
    assert_same_parses(db, input, &|| format!("{}\nthe input", what()));
    for style in STYLES {
        let new = expander::expand_rules(db, &mac.rules, input, |_| (), style, call_site)
            .map(|(tree, arm, _)| (tree, arm));
        let old = expander::model::expand_rules(db, &mac.rules, input, |_| (), style, call_site);
        assert_eq!(new, old, "{}\nstyle: {style:?}", what());
        assert_same_parses(db, &new.value.0, &|| format!("{}\nstyle: {style:?}", what()));
        expansions.push(new.value.0);
        if marked {
            let marker = |span: &mut Span| span.range = TextRange::empty(TextSize::from(7));
            let new = expander::expand_rules(db, &mac.rules, input, marker, style, call_site)
                .map(|(tree, arm, _)| (tree, arm));
            let old = expander::model::expand_rules(db, &mac.rules, input, marker, style, call_site);
            assert_eq!(new, old, "{}\nstyle: {style:?}, marked", what());
        }
    }
    expansions
}

const TOP_ENTRY_POINTS: usize = 5;

fn top_entry_point(idx: usize) -> parser::TopEntryPoint {
    use parser::TopEntryPoint::*;
    [MacroItems, MacroStmts, Expr, Pattern, Type]
        .into_iter()
        .nth(idx)
        .expect("an entry point index below `TOP_ENTRY_POINTS`")
}

/// What a closure gives, or that it panicked: the two conversions must fail alike too (a `'`
/// with no name after it).
fn caught<T>(f: impl FnOnce() -> T) -> Option<T> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).ok()
}

/// Parses `tt` as each kind of expansion through the parser input made in one pass over its
/// columns and through the one the old cursor conversion made, and asserts the trees, their
/// errors and their span maps are the same, and so are the inputs, as a parser's and as a
/// matcher's.
fn assert_same_parses(db: &Db, tt: &tt::TopSubtree, what: &impl Fn() -> String) {
    use syntax_bridge::reference;
    let mut edition = |ctx: SyntaxContext| ctx.edition(db);
    for view in [tt.view().strip_invisible(), tt.token_trees(), tt.view().as_token_trees()] {
        let new = caught(|| syntax_bridge::to_parser_input(view, &mut edition));
        let old = caught(|| reference::to_parser_input(view, &mut edition));
        match (&new, &old) {
            (Some(new), Some(old)) => {
                assert!(reference::same_input(new, old), "{}\nthe parser's input", what())
            }
            (None, None) => continue,
            _ => panic!("{}\none conversion panicked: new {}", what(), new.is_none()),
        }
        let new = syntax_bridge::to_matcher_input(view, &mut edition);
        let old = reference::to_matcher_input(view, &mut edition);
        assert!(reference::same_input(&new, &old), "{}\nthe matcher's input", what());
    }
    for idx in 0..TOP_ENTRY_POINTS {
        let new = caught(|| {
            let view = tt.view();
            let (parse, map) =
                syntax_bridge::token_tree_to_syntax_node(view, top_entry_point(idx), &mut edition);
            (parse.debug_dump(), map)
        });
        let old = caught(|| {
            let (parse, map) =
                reference::token_tree_to_syntax_node(tt, top_entry_point(idx), &mut edition);
            (parse.debug_dump(), map)
        });
        let context = || format!("{}\nparsed as {:?}", what(), top_entry_point(idx));
        match (new, old) {
            (Some((new_tree, new_map)), Some((old_tree, old_map))) => {
                assert_eq!(new_tree, old_tree, "{}", context());
                assert!(new_map == old_map, "{}: the span maps differ", context());
            }
            (None, None) => (),
            (new, _) => panic!("{}\none parse panicked: new {}", context(), new.is_none()),
        }
    }
}

const ENTRY_POINTS: usize = 10;

fn entry_point(idx: usize) -> parser::PrefixEntryPoint {
    use parser::PrefixEntryPoint::*;
    [Vis, Block, Stmt, Pat, PatTop, Ty, Expr, Path, Item, MetaItem]
        .into_iter()
        .nth(idx)
        .expect("an entry point index below `ENTRY_POINTS`")
}

/// Takes fragments of every kind from every position of `input` by both
/// [`crate::expect_fragment`] and the model's, and asserts they agree.
fn assert_same_fragments(
    db: &Db,
    input: &tt::TopSubtree,
    what: &impl Fn() -> String,
) {
    let delim_span = input.top_subtree().delimiter.delim_span();
    for idx in 0..ENTRY_POINTS {
        let mut new_iter = input.iter();
        let mut old_iter = input.iter();
        while !new_iter.is_empty() {
            let new = crate::expect_fragment(db, &mut new_iter, entry_point(idx), delim_span);
            let old =
                expander::model::expect_fragment(db, &mut old_iter, entry_point(idx), delim_span);
            let flat = |view: tt::TokenTreesView<'_>| view.iter_flat_tokens().collect::<Vec<_>>();
            let context = || format!("{}\nentry point: {:?}", what(), entry_point(idx));
            assert_eq!(flat(new.value), flat(old.value), "{}", context());
            assert_eq!(new.err, old.err, "{}", context());
            assert_eq!(new_iter.remaining().len(), old_iter.remaining().len(), "{}", context());
            if new.value.is_empty() {
                new_iter.next();
                old_iter.next();
            }
        }
    }
}

/// Macros (the bodies of `macro_rules!`) with inputs, between them using every feature the
/// matcher and the transcriber have.
const CORPUS: &[(&str, &[&str])] = &[
    (
        "($($tt:tt)*) => { $($tt)* };",
        &["fn main() { 1; 1.0; ((1,),).0.0; let x = 1; }", "", "'a 'b", ">>= .. ..= ::"],
    ),
    ("($($e:expr),* $(,)?) => { $($e);* ; };", &["_, const { 1 },", "1, a + b, f(x),", "", "1 2"]),
    (
        "($($i:ident $($e:expr),*);*) => { $(fn $i() { $($e);*; })* };",
        &["foo 1,2,3; bar 4,5,6", "foo; bar 1", "foo 1,", "foo 1 2"],
    ),
    (
        "(-1) => {-1}; (- 2) => {- 2}; (- 3.0) => {- 3.0}; (@$lit:literal) => {$lit}",
        &["-1", "- 1", "-2", "- 3.0", "@1", "@-1.0", "@--1.0", "@"],
    ),
    (
        "($a:ident, $b:ident, $c:tt) => { macro_rules! bar { ($bi:ident) => { fn $bi() -> u8 { $c } } } };",
        &["x, y, 1", "x, y", "x, y, (z)"],
    ),
    (
        "( $( $i:ident ),* ; $( [ $( $j:ident )* ] )* ) => { ${count($i)} ${count($j)} ${count($j, 1)} $( ${index()} ${ignore($i)} )* $( $( ${index(1)} $j )* )* };",
        &["a, b ; [x y] [] [z]", " ; ", "a ;", "a, b ; [x] [y z w]"],
    ),
    (
        "($a:ident, $b:literal, $t:tt) => { ${concat($a, _x, $b)} ${concat($t, y)} ${concat(a, b)} };",
        &["foo, 1, bar", "foo, \"s\", (x)", "foo, 1, fn", "foo, 1.5, 'a"],
    ),
    (
        "( $( $a:ident )=>* ) => { $( $a )=>* }; ( @ $( $l:lifetime )'sep* ) => { $( $l )'sep* }; ( # $( $x:tt ) and * ) => { $( $x ) and * }; ( % $( $x:tt ) 1 * ) => { $( $x ) 1 * }; ( ! $( $x:ident )::* ) => { $( $x )::* };",
        &[
            "a => b => c",
            "@ 'a 'sep 'b",
            "# x and y and z",
            "% x 1 y",
            "! a :: b :: c",
            "! a::b::c",
            "a =>",
            "! a ::",
        ],
    ),
    (
        "( $v:vis fn $name:ident ) => { $v fn $name () {} };",
        &["pub fn f", "fn g", "pub(crate) fn h", "pub", "pub(in a::b) fn i"],
    ),
    (
        "( a $x:ident b ) => { 1 $x }; ( a $x:ident $y:ident c ) => { 2 $x $y }; ( a $( $t:tt )* d ) => { 3 };",
        &["a x b", "a x y c", "a x y z", "a b c d", "a", ""],
    ),
    (
        "( $p:path ; $t:ty ; $pa:pat ; $pp:pat_param ; $s:stmt ; $b:block ; $m:meta ; $i:item ; $l:lifetime ; $li:literal ) => { $p $t $pa $pp $s $b $m $i $l $li $p < u8 > $t < u8 > };",
        &[
            "a::b ; Vec<T> ; Some(x) | None ; x ; let x = 1 ; { 2 } ; a(b) ; fn f() {} ; 'a ; 1",
            "a::b ; Vec<T>",
            "a<b>::c ; &'a str ; 1 | 2 ; _ ; x ; {} ; d = 1 ; struct S; ; 'static ; true",
        ],
    ),
    ("( $t:ty ) => { 1 as $t << 2 ; Vec<$t> };", &["u32", "&'a str", "fn() -> u8"]),
    (
        "( $e:expr ) => { $e . foo ( ) ; $e ? ; - $e ; $e ; };",
        &["-1", "a + b", "m!(x)", "{ 1 }", "1", "x.0.1", "|x| x + 1"],
    ),
    (
        "( [ $( $a:ident ),* ] { $b:tt } ( $( ( $c:tt ) )* ) ) => { [ $( $a ),* ] { $b } $( $c )* };",
        &["[a, b] { x } ((1) (2))", "[a] (x) ()", "[] { } ( )", "[a, b] { x y } ((1))"],
    ),
    ("( $a:ident ) => { $$ $a $b $crate :: x };", &["y", ""]),
    ("( $( $a:ident )* ) => { $a };", &["x y", "", "x"]),
    ("( $( $a:ident )* ; $( $b:ident )* ) => { $( $a $b )* };", &["x y ; z", "x ; y z", "x ; y"]),
    (
        "attr() ( $x:ident ) => { attr $x }; derive() ( $x:ident ) => { derive $x }; ( $x:ident ) => { fnlike $x };",
        &["x", "1"],
    ),
    (
        "( $a:ident += $b:expr ) => { $a = $a + $b }; ( $a:ident :: $b:ident ) => { $a :: $b }; ( $a:ident ..= $b:literal ) => { $a .. $b };",
        &["x += 1", "x + = 1", "a::b", "a :: b", "a ..= 1", "a ..=", "a"],
    ),
    (
        "( $( $a:ident )? ; $( , $b:tt )+ ) => { $( $a )? $( $b )+ };",
        &["x ; , 1 , 2", " ; , 1", "x ;", "x y ; , 1"],
    ),
    ("( $( $t:ident ),* as $ty:ident ) => { $( $t )* $ty };", &["as u32", "a, b as u32", "a b"]),
    (
        "( $( $k:ident = $v:expr ),* $(,)? ) => { $( let $k = $v ; )* }; ( $( $x:tt )* ) => { fallback $( $x )* };",
        &[
            "a = 1, b = 2 + 3, c = f(x),",
            "a = 1 b = 2",
            "a = , b = 2",
            "a = 1, b = 2, c = 3, d = 4, e = 5, f = 6, g = 7, h = 8, i = 9, j = 10, k = 11 + 12 * 13",
        ],
    ),
    (
        "( $s:stmt ; $i:item ; $m:meta ) => { $s ; $i $m };",
        &["let x = 1 ; fn f() {} ; a", "x ; struct S ; ; b(c)", "; ; ;"],
    ),
];

/// The editions of the definition and of the call the corpus is expanded in: each kind whose
/// meaning changes with the edition (`expr`, `pat`) in both meanings, and a call of a later
/// edition than the definition.
const EDITION_PAIRS: [(Edition, Edition); 3] = [
    (Edition::Edition2015, Edition::Edition2021),
    (Edition::Edition2021, Edition::Edition2024),
    (Edition::Edition2024, Edition::Edition2024),
];

#[test]
fn corpus() {
    let db = base_db::Db::for_tests();
    for &(decl, inputs) in CORPUS {
        for (pair, (def_edition, call_edition)) in EDITION_PAIRS.into_iter().enumerate() {
            let def = token_tree(0, def_edition, decl).expect("a corpus macro lexes");
            let mac = DeclarativeMacro::parse_macro_rules(&def, |_| def_edition);
            for &text in inputs {
                let input = token_tree(1, call_edition, text).expect("a corpus input lexes");
                let what = || {
                    format!(
                        "macro_rules! m {{ {decl} }} ({def_edition:?})\ninput: {text} ({call_edition:?})"
                    )
                };
                let marked = pair + 1 == EDITION_PAIRS.len();
                let expansions = assert_same(&db, &mac, &input, marked, &what);
                assert_same_fragments(&db, &input, &what);
                // Expansions hold the groups without delimiters an earlier expansion puts around
                // its fragments.
                for expansion in &expansions {
                    assert_same_fragments(&db, expansion, &|| {
                        format!("{}\nthe fragments of its expansion", what())
                    });
                }
            }
        }
    }
}

#[test]
fn long_fragments() {
    let db = base_db::Db::for_tests();
    let mut texts = vec![
        "a < b < c < d < e < f < g < h < i < j > > > > > > > > > , x".to_owned(),
        "Vec :: < u8 > :: new ( ) . iter ( ) . map ( | x | x + 1 ) . collect :: < Vec < _ > > ( ) ;"
            .to_owned(),
        "match x { _ => 1 } . foo ( ) + 2 ; fn f ( ) { } struct S ; pub ( crate ) fn g ( ) { }"
            .to_owned(),
        "#[a] fn f() {} let x = 1 ; let y = 2 ; 1 ..= 2 , 3 >>= <<= ..= ... x.0.1.2 'a : 'b"
            .to_owned(),
    ];
    // Fragments of many top-level trees, longer than any window but the last.
    for len in [1, 7, 8, 9, 15, 16, 17, 40, 200] {
        let sum = (0..len).map(|i| i.to_string()).collect::<Vec<_>>().join(" + ");
        texts.push(format!("{sum} , rest"));
        texts.push(sum);
        let path = (0..len).map(|i| format!("a{i}")).collect::<Vec<_>>().join("::");
        texts.push(format!("{path} ; {path} < u8 >"));
        let tuple = (0..len).map(|i| format!("x{i}")).collect::<Vec<_>>().join(" , ");
        texts.push(format!("( {tuple} ) | ( {tuple} ) => 1"));
    }
    for text in &texts {
        for edition in EDITIONS {
            let input = token_tree(1, edition, text).expect("a test input lexes");
            assert_same_fragments(&db, &input, &|| format!("input: {text} ({edition:?})"));
        }
    }
}

/// The real-world macros the benchmark expands, with its inputs.
#[test]
fn benchmark_macros() {
    if test_utils::skip_slow_tests() {
        return;
    }
    let db = base_db::Db::for_tests();
    let rules = crate::benchmark::macro_rules_fixtures();
    let invocations = crate::benchmark::invocation_fixtures(&db, &rules);
    for (id, input) in &invocations {
        assert_same(&db, &rules[id], input, false, &|| format!("benchmark macro {id}\n{input:?}"));
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Ident,
    Tt,
    Expr,
    Expr2021,
    Ty,
    Literal,
    Lifetime,
    Vis,
    Pat,
    PatParam,
    Path,
    Block,
    Item,
    Stmt,
    Meta,
}

const KINDS: [Kind; 15] = [
    Kind::Ident,
    Kind::Tt,
    Kind::Expr,
    Kind::Expr2021,
    Kind::Ty,
    Kind::Literal,
    Kind::Lifetime,
    Kind::Vis,
    Kind::Pat,
    Kind::PatParam,
    Kind::Path,
    Kind::Block,
    Kind::Item,
    Kind::Stmt,
    Kind::Meta,
];

/// The kinds a repetition's body holds: each takes a token or fails when it matches, so no
/// repetition goes round without taking input. Both expanders loop forever on one that can
/// (`validate` rejects only some), as a macro the definition's errors don't stop.
const REPEATED_KINDS: [Kind; 7] =
    [Kind::Ident, Kind::Tt, Kind::Expr, Kind::Ty, Kind::Literal, Kind::Lifetime, Kind::Path];

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Ident => "ident",
            Kind::Tt => "tt",
            Kind::Expr => "expr",
            Kind::Expr2021 => "expr_2021",
            Kind::Ty => "ty",
            Kind::Literal => "literal",
            Kind::Lifetime => "lifetime",
            Kind::Vis => "vis",
            Kind::Pat => "pat",
            Kind::PatParam => "pat_param",
            Kind::Path => "path",
            Kind::Block => "block",
            Kind::Item => "item",
            Kind::Stmt => "stmt",
            Kind::Meta => "meta",
        }
    }

    /// Inputs, most of the kind, some of another.
    fn samples(self) -> &'static [&'static str] {
        match self {
            Kind::Ident => &["foo", "bar", "r#type", "_", "1"],
            Kind::Tt => &["x", "1", "( a b )", "+=", "'a", "[ ]", "::"],
            Kind::Expr | Kind::Expr2021 => &[
                "1",
                "a + b",
                "f ( x )",
                "-1",
                "{ 2 }",
                "_",
                "const { 1 }",
                "x.0.1",
                "m ! ( y )",
                "| x | x",
            ],
            Kind::Ty => &["u8", "Vec < T >", "& 'a str", "( u8 , i8 )", "fn ( ) -> u8", "1"],
            Kind::Literal => &["1", "-2", "\"s\"", "true", "1.5", "x"],
            Kind::Lifetime => &["'a", "'static", "'_", "a"],
            Kind::Vis => &["", "pub", "pub ( crate )"],
            Kind::Pat | Kind::PatParam => &["_", "Some ( x )", "1 | 2", "x @ 1 ..= 5"],
            Kind::Path => &["a :: b", "Vec :: < u8 >", "crate :: x", "a < b >", "1"],
            Kind::Block => &["{ }", "{ x }", "{ let y = 1 ; y }", "x"],
            Kind::Item => &["fn f ( ) { }", "struct S ;", "use a :: b ;"],
            Kind::Stmt => &["let x = 1", "x", "fn f ( ) { }"],
            Kind::Meta => &["a", "b ( c )", "d = 1"],
        }
    }
}

const TOKENS: [&str; 11] = ["a", "b", "fn", ",", ";", "=>", "+", "::", "@", "1", "#"];
const SEPARATORS: [&str; 5] = [",", ";", "=>", "and", "::"];
const DELIMITERS: [(&str, &str); 3] = [("(", ")"), ("[", "]"), ("{", "}")];

enum Pat {
    Token(&'static str),
    Var(u32, Kind),
    Group(usize, Vec<Pat>),
    Rep { body: Vec<Pat>, separator: Option<&'static str>, op: &'static str },
}

struct Gen {
    rng: SplitMix64,
    vars: u32,
}

impl Gen {
    fn pick<T: Copy>(&mut self, items: &[T]) -> T {
        *self.rng.pick(items).expect("a list to pick from is never empty")
    }

    fn pattern(&mut self, len: usize, depth: u32, repeated: bool) -> Vec<Pat> {
        let mut out = Vec::with_capacity(len + 1);
        for _ in 0..len {
            let roll = self.rng.index(100);
            let pat = if roll < 35 {
                Pat::Token(self.pick(&TOKENS))
            } else if roll < 75 {
                let kind = if repeated {
                    self.pick(&REPEATED_KINDS)
                } else {
                    self.pick(&KINDS)
                };
                self.vars += 1;
                Pat::Var(self.vars - 1, kind)
            } else if roll < 87 && depth < 3 {
                let len = 1 + self.rng.index(3);
                Pat::Group(self.rng.index(DELIMITERS.len()), self.pattern(len, depth + 1, repeated))
            } else if depth < 3 {
                let len = 1 + self.rng.index(3);
                let body = self.pattern(len, depth + 1, true);
                let separator = (self.rng.index(100) < 50).then(|| self.pick(&SEPARATORS));
                Pat::Rep { body, separator, op: self.pick(&["*", "+", "?"]) }
            } else {
                Pat::Token(self.pick(&TOKENS))
            };
            out.push(pat);
        }
        if repeated && out.iter().all(|it| matches!(it, Pat::Rep { .. })) {
            out.push(Pat::Token(self.pick(&TOKENS)));
        }
        out
    }

    /// The template for `pats`, `nesting` repetitions deep: the pattern's variables at the depths
    /// it binds them, some of its tokens, and now and then a metavariable expression or a
    /// variable at the wrong depth.
    fn template(&mut self, pats: &[Pat], nesting: u32, vars: &[u32], out: &mut String) {
        for pat in pats {
            if self.rng.index(100) < 10 {
                out.push_str("0 ");
            }
            match pat {
                Pat::Token(token) => {
                    if self.rng.index(100) < 70 {
                        out.push_str(token);
                        out.push(' ');
                    }
                }
                Pat::Var(id, _) => write!(out, "$v{id} ").unwrap(),
                Pat::Group(delimiter, body) => {
                    let (open, close) = DELIMITERS[*delimiter];
                    write!(out, "{open} ").unwrap();
                    self.template(body, nesting, vars, out);
                    write!(out, "{close} ").unwrap();
                }
                Pat::Rep { body, separator, op } => {
                    let mut own = Vec::new();
                    direct_vars(body, &mut own);
                    out.push_str("$( ");
                    self.template(body, nesting + 1, vars, out);
                    if !own.is_empty() {
                        if self.rng.index(100) < 20 {
                            out.push_str("${index()} ");
                        }
                        if self.rng.index(100) < 15 {
                            write!(out, "${{ignore($v{})}} ", self.pick(&own)).unwrap();
                        }
                    }
                    out.push_str(") ");
                    if let Some(separator) = separator {
                        write!(out, "{separator} ").unwrap();
                    }
                    write!(out, "{op} ").unwrap();
                }
            }
        }
        // At the top only: inside a repetition, a variable bound outside it never ends the
        // repetition, which then runs to the transcriber's limit.
        if nesting == 0 && !vars.is_empty() {
            if self.rng.index(100) < 20 {
                let depth = self.rng.index(3);
                write!(out, "${{count($v{}, {depth})}} ", self.pick(vars)).unwrap();
            }
            if self.rng.index(100) < 15 {
                write!(out, "$v{} ", self.pick(vars)).unwrap();
            }
            if self.rng.index(100) < 5 {
                write!(out, "${{concat(x, $v{})}} ", self.pick(vars)).unwrap();
            }
        }
        if nesting == 0 && self.rng.index(100) < 5 {
            out.push_str("$$ $zz ");
        }
    }

    fn input(&mut self, pats: &[Pat], out: &mut Vec<&'static str>) {
        for pat in pats {
            match pat {
                Pat::Token(token) => out.push(*token),
                Pat::Var(_, kind) => {
                    let sample = self.pick(kind.samples());
                    if !sample.is_empty() {
                        out.push(sample);
                    }
                }
                Pat::Group(delimiter, body) => {
                    let (open, close) = DELIMITERS[*delimiter];
                    out.push(open);
                    self.input(body, out);
                    out.push(close);
                }
                Pat::Rep { body, separator, op } => {
                    let times = match *op {
                        "?" => self.rng.index(2),
                        "+" => 1 + self.rng.index(3),
                        _ => self.rng.index(4),
                    };
                    for time in 0..times {
                        if time > 0
                            && let Some(separator) = separator
                        {
                            out.push(*separator);
                        }
                        self.input(body, out);
                    }
                }
            }
        }
    }

    /// Drops, adds or doubles a token now and then, never a delimiter.
    fn mutate(&mut self, pieces: &mut Vec<&'static str>) {
        if self.rng.index(100) >= 35 {
            return;
        }
        let at = self.rng.index(pieces.len() + 1);
        let is_delimiter =
            |piece: &str| DELIMITERS.iter().any(|&(open, close)| piece == open || piece == close);
        let token = at < pieces.len() && !is_delimiter(pieces[at]);
        match self.rng.index(3) {
            0 if token => {
                pieces.remove(at);
            }
            1 if token => pieces.insert(at, pieces[at]),
            _ => pieces.insert(at, self.pick(&TOKENS)),
        }
    }
}

/// The variables of a repetition's body that are not in a repetition inside it.
fn direct_vars(pats: &[Pat], out: &mut Vec<u32>) {
    for pat in pats {
        match pat {
            Pat::Var(id, _) => out.push(*id),
            Pat::Group(_, body) => direct_vars(body, out),
            Pat::Token(_) | Pat::Rep { .. } => {}
        }
    }
}

fn all_vars(pats: &[Pat], out: &mut Vec<u32>) {
    for pat in pats {
        match pat {
            Pat::Var(id, _) => out.push(*id),
            Pat::Group(_, body) | Pat::Rep { body, .. } => all_vars(body, out),
            Pat::Token(_) => {}
        }
    }
}

fn render_pattern(pats: &[Pat], out: &mut String) {
    for pat in pats {
        match pat {
            Pat::Token(token) => write!(out, "{token} ").unwrap(),
            Pat::Var(id, kind) => write!(out, "$v{id}:{} ", kind.name()).unwrap(),
            Pat::Group(delimiter, body) => {
                let (open, close) = DELIMITERS[*delimiter];
                write!(out, "{open} ").unwrap();
                render_pattern(body, out);
                write!(out, "{close} ").unwrap();
            }
            Pat::Rep { body, separator, op } => {
                out.push_str("$( ");
                render_pattern(body, out);
                out.push_str(") ");
                if let Some(separator) = separator {
                    write!(out, "{separator} ").unwrap();
                }
                write!(out, "{op} ").unwrap();
            }
        }
    }
}

/// Random macros checked always, and with slow tests on. An error in a repetition leaves its
/// variables bound outside it (the match the error recovery keeps is the repetition's own), so
/// the transcriber runs that repetition to its limit, in both expanders alike: many inputs here
/// take that path.
const SEEDS: u64 = 40;
const SLOW_SEEDS: u64 = 400;
const INPUTS_PER_MACRO: usize = 5;

#[test]
fn random_macros() {
    let db = base_db::Db::for_tests();
    let seeds = if test_utils::skip_slow_tests() { SEEDS } else { SLOW_SEEDS };
    let mut checked = 0;
    for seed in 0..seeds {
        let mut generator = Gen { rng: SplitMix64::new(seed), vars: 0 };
        let def_edition = generator.pick(&EDITIONS);
        let call_edition = generator.pick(&EDITIONS);
        let mut decl = String::new();
        let mut patterns = Vec::new();
        for _ in 0..1 + generator.rng.index(3) {
            let len = 1 + generator.rng.index(5);
            let pattern = generator.pattern(len, 0, false);
            let mut vars = Vec::new();
            all_vars(&pattern, &mut vars);
            match generator.rng.index(12) {
                0 => decl.push_str("attr() "),
                1 => decl.push_str("derive() "),
                _ => {}
            }
            decl.push_str("( ");
            render_pattern(&pattern, &mut decl);
            decl.push_str(") => { ");
            generator.template(&pattern, 0, &vars, &mut decl);
            decl.push_str("} ; ");
            patterns.push(pattern);
        }
        let Some(def) = token_tree(0, def_edition, &decl) else { continue };
        let mac = DeclarativeMacro::parse_macro_rules(&def, |_| def_edition);
        for input_idx in 0..INPUTS_PER_MACRO {
            let pattern = &patterns[generator.rng.index(patterns.len())];
            let mut pieces = Vec::new();
            generator.input(pattern, &mut pieces);
            generator.mutate(&mut pieces);
            let text = pieces.join(" ");
            let Some(input) = token_tree(1, call_edition, &text) else { continue };
            let what = || {
                format!(
                    "seed {seed}: macro_rules! m {{ {decl} }} ({def_edition:?})\ninput: {text} ({call_edition:?})"
                )
            };
            assert_same(&db, &mac, &input, input_idx == 0, &what);
            if input_idx == 0 {
                assert_same_fragments(&db, &input, &what);
            }
            checked += 1;
        }
    }
    // The generator writes text that lexes; a change that breaks it must not pass by checking
    // nothing.
    let expected = seeds as usize * INPUTS_PER_MACRO * 9 / 10;
    assert!(checked >= expected, "only {checked} inputs checked");
}
