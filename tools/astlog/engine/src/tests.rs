//! The engine against a synthetic source: a graph whose `(edge a b)` rows the test chooses and
//! whose `(succ a b)` is demand-driven on `a`.

use std::borrow::Cow;
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

use blaze_sim::{Config, Tape};

use crate::eval::{Facts, Row, Value};
use crate::program::{
    Language, Needs, Parsed, Program, Segment, TemplateSyntax, parse_template, terms,
};
use crate::{Error, Line, Sexpr};

#[derive(Debug)]
struct Graph;

#[derive(Debug)]
enum GraphAtom {
    Edge,
    Succ,
}

#[derive(Debug, snafu::Snafu)]
#[snafu(display("graph source failed"))]
struct GraphError;

impl Language for Graph {
    type Atom = GraphAtom;
    type Action = ();
    type Error = GraphError;
    const TEMPLATE: TemplateSyntax = TemplateSyntax::Dollar;

    fn claims(name: &str) -> bool {
        matches!(name, "edge" | "succ")
    }

    fn parse_atom(
        head: &str,
        args: &[Sexpr],
        _line: &Line,
    ) -> Result<Option<Parsed<GraphAtom>>, Error<GraphError>> {
        let (value, needs) = match head {
            "edge" => (GraphAtom::Edge, Needs::Nothing),
            "succ" => (GraphAtom::Succ, Needs::All(&[0])),
            _ => return Ok(None),
        };
        Ok(Some(Parsed { value, args: terms(args)?, needs }))
    }

    fn parse_action(
        _head: &str,
        _args: &[Sexpr],
        _line: &Line,
    ) -> Result<Option<Parsed<()>>, Error<GraphError>> {
        Ok(None)
    }
}

struct Edges(Vec<(u32, u32)>);

impl Facts for Edges {
    type Lang = Graph;
    type Node = u32;

    fn solve(
        &self,
        atom: &GraphAtom,
        _line: &Line,
        inputs: &[Option<Value<u32>>],
        out: &mut Vec<Row<u32>>,
    ) -> Result<(), Error<GraphError>> {
        match atom {
            GraphAtom::Edge => {
                out.extend(self.0.iter().map(|&(a, b)| vec![Value::Node(a), Value::Node(b)]));
            }
            GraphAtom::Succ => {
                let Some(Value::Node(a)) = inputs[0] else {
                    return Err(GraphError.into());
                };
                out.push(vec![Value::Node(a), Value::Node(a + 1)]);
            }
        }
        Ok(())
    }

    fn parent(&self, _node: u32) -> Option<u32> {
        None
    }

    fn kind(&self, _node: u32) -> Cow<'_, str> {
        Cow::Borrowed("vertex")
    }

    fn text(&self, node: u32) -> Cow<'_, str> {
        Cow::Owned(node.to_string())
    }

    fn span(&self, _node: u32) -> Option<(usize, Range<usize>)> {
        None
    }

    fn path(&self, _file: usize) -> Cow<'_, str> {
        Cow::Borrowed("graph")
    }
}

type Sets = BTreeMap<String, BTreeSet<Row<u32>>>;

fn semi_naive(program: &Program<Graph>, edges: &Edges) -> Result<Sets, Error<GraphError>> {
    let (db, _) = crate::run(program, edges)?;
    Ok(db
        .relations
        .into_iter()
        .map(|(name, relation)| (name, relation.rows().iter().cloned().collect()))
        .collect())
}

fn rows(sets: &Sets, name: &str) -> Vec<Vec<String>> {
    sets[name]
        .iter()
        .map(|row| {
            row.iter()
                .map(|value| match value {
                    Value::Node(n) => n.to_string(),
                    Value::Text(t) => t.to_string(),
                    Value::Int(i) => format!("#{i}"),
                })
                .collect()
        })
        .collect()
}

/// One generated rule for relation `r{head}`: positive atoms over `edge` and `r0..=r{head}`,
/// negations and counts over `r0..r{head}` only, so every generated program stratifies.
fn rule(tape: &Tape, head: usize) -> String {
    let vars = ["a", "b", "c"];
    let var = |tape: &Tape| vars[tape.index(vars.len(), "a variable")];
    let mut bound: Vec<&str> = Vec::new();
    let mut body = Vec::new();
    let atoms = tape.int_in(1..=3usize, "a rule's atoms");
    for at in 0..atoms {
        let pick = tape.int_in(0..=5u8, "an atom's kind");
        if at > 0 && pick == 5 && !bound.is_empty() {
            let x = bound[tape.index(bound.len(), "a bound variable")];
            let y = bound[tape.index(bound.len(), "a bound variable")];
            body.push(format!("(not (edge {x} {y}))"));
        } else if at > 0 && pick == 3 && head > 0 && !bound.is_empty() {
            let rel = tape.index(head, "a lower relation");
            let x = bound[tape.index(bound.len(), "a bound variable")];
            let y = bound[tape.index(bound.len(), "a bound variable")];
            body.push(format!("(not (r{rel} {x} {y}))"));
        } else if at > 0 && pick == 4 && head > 0 && !bound.is_empty() {
            let rel = tape.index(head, "a lower relation");
            let x = bound[tape.index(bound.len(), "a bound variable")];
            body.push(format!("(count n{at} (r{rel} {x} z{at}))"));
            body.push(format!("(lt n{at} 2)"));
        } else {
            let (x, y) = (var(tape), var(tape));
            let rel = if pick % 2 == 0 {
                "edge".to_owned()
            } else {
                format!("r{}", tape.index(head + 1, "a relation up to the head"))
            };
            body.push(format!("({rel} {x} {y})"));
            for v in [x, y] {
                if !bound.contains(&v) {
                    bound.push(v);
                }
            }
        }
    }
    let x = bound[tape.index(bound.len(), "a bound variable")];
    let y = bound[tape.index(bound.len(), "a bound variable")];
    format!("(rule (r{head} {x} {y}) {})", body.join(" "))
}

/// The refinement: semi-naive evaluation derives exactly what the naive fixpoint derives, for
/// any stratifiable program over any small graph.
#[test]
fn semi_naive_derives_what_naive_derives() {
    blaze_sim::check(Config::DEFAULT, |tape| {
        let edges: Vec<(u32, u32)> = tape.vec(0..=12, "edges", |tape| {
            (tape.int_in(0..=5u32, "an edge's start"), tape.int_in(0..=5u32, "an edge's end"))
        });
        let mut text = String::new();
        for head in 0..3 {
            // Every relation gets at least one rule so the generated references resolve.
            text.push_str(&format!("(rule (r{head} a b) (edge a b))\n"));
            let more = tape.int_in(0..=2usize, "a relation's further rules");
            for _ in 0..more {
                text.push_str(&rule(tape, head));
                text.push('\n');
            }
        }
        let program = match Program::<Graph>::parse(&text) {
            Ok(program) => program,
            Err(error) => panic!("generated program refused: {error}\n{text}"),
        };
        let edges = Edges(edges);
        let fast = semi_naive(&program, &edges).unwrap_or_else(|error| panic!("{error}\n{text}"));
        let reference = crate::naive::fixpoint(&program, &edges);
        assert_eq!(fast, reference, "{text}");
    });
}

#[test]
fn transitive_closure_reaches_every_path() -> Result<(), Error<GraphError>> {
    let program = Program::<Graph>::parse(
        "(rule (path a b) (edge a b))
         (rule (path a c) (path a b) (path b c))",
    )?;
    let sets = semi_naive(&program, &Edges(vec![(1, 2), (2, 3), (3, 4)]))?;
    assert_eq!(
        rows(&sets, "path"),
        [["1", "2"], ["1", "3"], ["1", "4"], ["2", "3"], ["2", "4"], ["3", "4"]]
    );
    Ok(())
}

#[test]
fn negation_reads_the_final_lower_stratum() -> Result<(), Error<GraphError>> {
    // `reach` is recursive; `unreached` negates it, so it must see every reach row, including
    // the ones only later rounds derive.
    let program = Program::<Graph>::parse(
        "(rule (reach b) (edge a b) (text a \"0\"))
         (rule (reach c) (reach b) (edge b c))
         (rule (vertex v) (edge v w))
         (rule (vertex w) (edge v w))
         (rule (unreached v) (vertex v) (not (reach v)))",
    )?;
    let edges = Edges(vec![(0, 1), (1, 2), (2, 3), (5, 6)]);
    let sets = semi_naive(&program, &edges)?;
    assert_eq!(rows(&sets, "unreached"), [["0"], ["5"], ["6"]]);
    Ok(())
}

#[test]
fn a_negated_source_atom_filters_bound_rows() -> Result<(), Error<GraphError>> {
    let program = Program::<Graph>::parse(
        "(rule (one-way a b) (edge a b) (not (edge b a)))",
    )?;
    let sets = semi_naive(&program, &Edges(vec![(1, 2), (2, 1), (2, 3)]))?;
    assert_eq!(rows(&sets, "one-way"), [["2", "3"]]);
    Ok(())
}

#[test]
fn a_negated_source_atom_with_an_unbound_variable_is_refused() {
    let error = Program::<Graph>::parse("(rule (r a) (edge a a) (not (edge a b)))").unwrap_err();
    assert!(matches!(error, Error::UnsafeNegation { ref var, .. } if var == "b"), "{error}");
}

#[test]
fn unstratifiable_program_names_its_cycle() {
    let error = Program::<Graph>::parse(
        "(rule (p a) (edge a b) (not (q a)))
         (rule (q a) (edge a b) (r a))
         (rule (r a) (p a))
         (rule (s a) (edge a b))",
    )
    .expect_err("p negates q, which reaches p");
    let message = error.to_string();
    assert!(message.contains("`p`, `q`, `r`"), "got: {message}");
    assert!(!message.contains("`s`"), "s is on no cycle: {message}");
}

/// `nested(n)` is `n` lists, each holding the next, around one atom.
fn nested(depth: usize) -> String {
    format!("{}x{}", "(".repeat(depth), ")".repeat(depth))
}

#[test]
fn a_rules_file_nested_past_the_depth_cap_is_refused_naming_its_line() {
    let max = crate::sexpr::MAX_DEPTH;
    let at_cap = crate::sexpr::parse::<GraphError>(&nested(max), None);
    assert!(at_cap.is_ok(), "{max} levels read: {at_cap:?}");
    let text = format!("(rule (r a) (edge a b))\n{}", nested(max + 1));
    let past = Program::<Graph>::parse(&text).expect_err("one level past the cap");
    assert!(matches!(past, Error::TooDeep { ref line, .. } if *line == 2), "{past}");
    assert!(past.to_string().starts_with("rules:2: ") && past.to_string().contains("nested"));
    // Far past the cap is refused the same way, not by the reader overflowing its stack.
    let far =
        crate::sexpr::parse::<GraphError>(&nested(1_000_000), None).expect_err("far past the cap");
    assert!(matches!(far, Error::TooDeep { ref line, .. } if *line == 1), "{far}");
}

#[test]
fn count_counts_rows_per_bound_key() -> Result<(), Error<GraphError>> {
    let program = Program::<Graph>::parse(
        "(rule (out a b) (edge a b))
         (rule (fanout a n) (edge a x) (count n (out a b)))
         (rule (hub a) (fanout a n) (lt 1 n))",
    )?;
    let sets = semi_naive(&program, &Edges(vec![(1, 2), (1, 3), (2, 3)]))?;
    assert_eq!(rows(&sets, "fanout"), [["1", "#2"], ["2", "#1"]]);
    assert_eq!(rows(&sets, "hub"), [["1"]]);
    Ok(())
}

#[test]
fn count_local_variable_cannot_escape() {
    let error = Program::<Graph>::parse(
        "(rule (out a b) (edge a b))
         (rule (bad a b) (edge a x) (count n (out a b)) (edge b a))",
    )
    .expect_err("b is local to the count");
    assert!(error.to_string().contains("local to the (count ...)"), "got: {error}");
}

#[test]
fn a_count_whose_result_it_also_counts_over_is_refused_naming_the_variable() {
    let error = Program::<Graph>::parse(
        "(rule (out a b) (edge a b))
         (rule (bad a x) (edge a y) (count x (out a x)))",
    )
    .expect_err("x is the count and a column it counts over: it derives nothing");
    let message = error.to_string();
    assert!(message.starts_with("rules:2: ") && message.contains("`x`"), "{message}");
    // A result bound before the count compares the count with it, which is meaningful.
    let compared = Program::<Graph>::parse(
        "(rule (out a b) (edge a b))
         (rule (two a) (edge a n) (count n (out a b)))",
    );
    assert!(compared.is_ok(), "{compared:?}");
}

#[test]
fn demanded_argument_must_be_bound_on_the_left() {
    let error = Program::<Graph>::parse("(rule (next a b) (succ a b) (edge a b))")
        .expect_err("succ is demand-driven on a");
    let message = error.to_string();
    assert!(message.contains("`succ` needs `a` bound"), "got: {message}");
    let program = Program::<Graph>::parse("(rule (next a b) (edge a x) (succ a b))");
    assert!(program.is_ok(), "bound on the left is fine");
}

/// [`Edges`] that counts its `succ` calls.
struct Counted {
    edges: Edges,
    succ_calls: Cell<usize>,
}

impl Facts for Counted {
    type Lang = Graph;
    type Node = u32;

    fn solve(
        &self,
        atom: &GraphAtom,
        line: &Line,
        inputs: &[Option<Value<u32>>],
        out: &mut Vec<Row<u32>>,
    ) -> Result<(), Error<GraphError>> {
        if matches!(atom, GraphAtom::Succ) {
            self.succ_calls.set(self.succ_calls.get() + 1);
        }
        self.edges.solve(atom, line, inputs, out)
    }

    fn parent(&self, node: u32) -> Option<u32> {
        self.edges.parent(node)
    }

    fn kind(&self, node: u32) -> Cow<'_, str> {
        self.edges.kind(node)
    }

    fn text(&self, node: u32) -> Cow<'_, str> {
        self.edges.text(node)
    }

    fn span(&self, node: u32) -> Option<(usize, Range<usize>)> {
        self.edges.span(node)
    }

    fn path(&self, file: usize) -> Cow<'_, str> {
        self.edges.path(file)
    }
}

#[test]
fn a_source_atom_asked_the_same_inputs_twice_is_called_once() -> Result<(), Error<GraphError>> {
    // `next` asks `succ` with `a` = 1, 1, 2, 2, 3: three distinct inputs in five asks.
    // `skip` asks the negated `succ` with (1, 3), (1, 4), (1, 4), (2, 4): three in four.
    let program = Program::<Graph>::parse(
        "(rule (next a b) (edge a x) (succ a b))
         (rule (skip a c) (edge a b) (edge b c) (not (succ a c)))",
    )?;
    let edges = Edges(vec![(1, 2), (1, 3), (2, 3), (2, 4), (3, 4)]);
    let facts = Counted { edges, succ_calls: Cell::new(0) };
    let (db, _) = crate::run(&program, &facts)?;
    assert_eq!(db.relations["next"].rows().len(), 3);
    assert_eq!(db.relations["skip"].rows().len(), 3);
    assert_eq!(facts.succ_calls.get(), 3 + 3, "one call per (atom occurrence, inputs)");
    Ok(())
}

#[test]
fn dollar_templates_splice_and_escape()-> Result<(), Error<GraphError>> {
    let template =
        parse_template::<GraphError>(TemplateSyntax::Dollar, "$a + ${b}c $$ $$$args", &Line::rules(1))?;
    assert_eq!(
        template.segments,
        [
            Segment::Var("a".to_owned()),
            Segment::Lit(" + ".to_owned()),
            Segment::Var("b".to_owned()),
            Segment::Lit("c $ ".to_owned()),
            Segment::Var("args".to_owned()),
        ]
    );
    let stray = parse_template::<GraphError>(TemplateSyntax::Dollar, "cost $5", &Line::rules(1));
    assert!(stray.is_err(), "`$5` is no variable");
    Ok(())
}

#[test]
fn a_regex_that_does_not_compile_or_is_past_the_size_cap_is_refused_at_load() {
    let unclosed = Program::<Graph>::parse(
        "(rule (r a) (edge a b))\n(rule (m a) (edge a b) (text-match a \"(unclosed\"))",
    )
    .expect_err("an unclosed group is refused when the file is read, before any fact");
    assert!(
        matches!(unclosed, Error::Regex { ref line, ref name, .. } if *line == 2 && name == "text-match"),
        "{unclosed}"
    );
    // The `regex` crate's own default takes this pattern; the cap is what refuses it.
    let big = r"(?:\w{100}){2}";
    assert!(regex::Regex::new(big).is_ok(), "within the crate's default limit");
    let rules = format!("(rule (m a) (edge a b) (text-match a \"{}\"))", big.replace('\\', "\\\\"));
    let past = Program::<Graph>::parse(&rules).expect_err("past MAX_REGEX_BYTES");
    assert!(matches!(past, Error::Regex { ref line, .. } if *line == 1), "{past}");
}

#[test]
fn text_capture_binds_each_group_and_holds_only_on_a_match() -> Result<(), Error<GraphError>> {
    let program = Program::<Graph>::parse(
        "(rule (digit a d) (edge a b) (text-capture a \"^1(\\\\d)?$\" d))",
    )?;
    let sets = semi_naive(&program, &Edges(vec![(1, 2), (12, 3), (5, 6), (123, 7)]))?;
    // `1` matches with the group unused (empty text); `12` binds `2`; `5` and `123` do not match.
    assert_eq!(rows(&sets, "digit"), [["1", ""], ["12", "2"]]);
    Ok(())
}

#[test]
fn text_capture_with_a_variable_count_other_than_its_groups_is_refused_at_load() {
    let error = Program::<Graph>::parse(
        "(rule (r a) (edge a b))\n(rule (two a x y) (edge a b) (text-capture a \"(\\\\d)\" x y))",
    )
    .expect_err("one group, two variables");
    let message = error.to_string();
    assert!(
        message.starts_with("rules:2: ") && message.contains("1 capture groups, but 2 variables"),
        "{message}"
    );
}

#[test]
fn text_replace_writes_every_match_with_its_groups() -> Result<(), Error<GraphError>> {
    let program = Program::<Graph>::parse(
        "(rule (r t u) (edge a b) (text a t) (text-replace t \"(\\\\d)(\\\\d)\" \"${2}${1}_$$\" u))",
    )?;
    let sets = semi_naive(&program, &Edges(vec![(1234, 1), (5, 6)]))?;
    // Both pairs of `1234` swap; `5` has no match and is its own text: the builtin always holds.
    assert_eq!(rows(&sets, "r"), [["1234", "21_$43_$"], ["5", "5"]]);
    Ok(())
}

#[test]
fn a_text_replace_dollar_that_names_no_group_is_refused_at_load() {
    let refused = |replacement: &str| {
        let rules = format!(
            "(rule (r a) (edge a b))\n(rule (s u) (edge a b) (text a t) \
             (text-replace t \"legacy_(.*)\" \"{replacement}\" u))"
        );
        Program::<Graph>::parse(&rules).err().map(|it| it.to_string())
    };
    // The `regex` crate reads `$1_new` as a group named `1_new` and writes empty text for it.
    let glued = refused("$1_new").expect("`$1_new` is no group");
    assert!(
        glued.starts_with("rules:2: ") && glued.contains("`$1_new`") && glued.contains("write `${1}`"),
        "{glued}"
    );
    let missing = refused("$2").expect("one group only");
    assert!(missing.contains("`$2` names no group") && missing.contains("1 groups"), "{missing}");
    let stray = refused("cost $ 5").expect("a lone `$`");
    assert!(stray.contains("write `$$`"), "{stray}");
    let open = refused("${1").expect("unclosed brace");
    assert!(open.contains("no closing"), "{open}");
    for fine in ["${1}_new", "$1", "$$1", "${0}", "new"] {
        assert_eq!(refused(fine), None, "{fine}");
    }
}

#[test]
fn case_splits_identifier_words_and_writes_each_style() {
    let cases = [
        ("legacy_parse_HTTP", "pascal", "LegacyParseHttp"),
        ("HTTPServer", "snake", "http_server"),
        ("utf8Error", "kebab", "utf8-error"),
        ("parse_v2", "camel", "parseV2"),
        ("fooBar", "upper-snake", "FOO_BAR"),
        ("_unused_value", "camel", "_unusedValue"),
        ("type_", "pascal", "Type_"),
        ("__", "snake", "__"),
        ("my-crate", "snake", "my_crate"),
    ];
    for (text, case, want) in cases {
        assert_eq!(crate::eval::recase(text, case).as_deref(), Some(want), "{text} as {case}");
    }
    assert_eq!(crate::eval::recase("r#type", "snake"), None);
    assert_eq!(crate::eval::recase("a b", "snake"), None);
}

#[test]
fn case_is_a_builtin_with_a_literal_style_and_refuses_text_that_is_no_identifier()
-> Result<(), Error<GraphError>> {
    let program = Program::<Graph>::parse(
        "(rule (r y) (edge a b) (case \"HTTPServer\" \"snake\" y))",
    )?;
    let sets = semi_naive(&program, &Edges(vec![(1, 2)]))?;
    assert_eq!(rows(&sets, "r"), [["http_server"]]);
    let style = Program::<Graph>::parse("(rule (r y) (edge a b) (case \"x\" \"Title\" y))")
        .expect_err("no such style");
    assert!(style.to_string().contains("`\"Title\"`") || style.to_string().contains("`Title`"), "{style}");
    let spaced = Program::<Graph>::parse("(rule (r y) (edge a b) (case \"a b\" \"snake\" y))")?;
    let error = semi_naive(&spaced, &Edges(vec![(1, 2)])).expect_err("`a b` has a space");
    assert!(matches!(error, Error::CaseNotIdentifier { ref line, ref text } if *line == 1 && text == "a b"), "{error}");
    Ok(())
}

#[test]
fn a_negated_builtin_holds_when_the_builtin_gives_no_row() -> Result<(), Error<GraphError>> {
    let program = Program::<Graph>::parse(
        "(rule (plain a) (edge a b) (not (text-match a \"^1\")))
         (rule (out a b) (edge a b))
         (rule (single a) (edge a x) (count n (out a b)) (not (lt 1 n)))
         (rule (kept a) (edge a b) (text a t) (not (same-text t \"12\")))",
    )?;
    let sets = semi_naive(&program, &Edges(vec![(1, 2), (1, 4), (12, 3), (5, 6)]))?;
    assert_eq!(rows(&sets, "plain"), [["5"]]);
    assert_eq!(rows(&sets, "single"), [["5"], ["12"]]);
    assert_eq!(rows(&sets, "kept"), [["1"], ["5"]]);
    Ok(())
}

#[test]
fn a_negated_builtin_with_an_unbound_variable_or_a_count_of_an_atom_with_an_unbound_input_is_refused() {
    let unbound =
        Program::<Graph>::parse("(rule (r a) (edge a b) (not (text-capture a \"(\\\\d)\" g)))")
            .expect_err("`g` is bound by nothing to the left");
    assert!(matches!(unbound, Error::UnsafeNegation { ref var, .. } if var == "g"), "{unbound}");
    let builtin = Program::<Graph>::parse("(rule (r a n) (edge a b) (count n (text x t)))")
        .expect_err("`text` needs its node, and `x` is bound by nothing before the count");
    assert!(
        matches!(builtin, Error::UnboundBuiltinArg { ref name, ref arg, .. } if name == "text" && arg == "x"),
        "{builtin}"
    );
    let source = Program::<Graph>::parse("(rule (r a n) (edge a b) (count n (succ x y)))")
        .expect_err("`succ` needs its first column, and `x` is bound by nothing before the count");
    assert!(
        matches!(source, Error::UnboundSourceArg { ref name, ref arg, .. } if name == "succ" && arg == "x"),
        "{source}"
    );
}

/// A count reads a source atom or a builtin as it reads a relation: the distinct rows that agree
/// with what is bound, none being a count of 0.
#[test]
fn count_counts_the_distinct_rows_of_a_source_atom_or_a_builtin() -> Result<(), Error<GraphError>> {
    let program = Program::<Graph>::parse(
        "(rule (fanout a n) (edge a x) (count n (edge a b)))
         (rule (next a n) (edge a x) (count n (succ a b)))
         (rule (busy a m) (fanout a n) (count m (lt 1 n)))
         (rule (named a n) (edge a x) (count n (text-capture a \"(\\\\d)\" d)))",
    )?;
    // `edge` answers (1, 2) twice: one distinct row.
    let sets = semi_naive(&program, &Edges(vec![(1, 2), (1, 2), (1, 3), (2, 3), (12, 1)]))?;
    assert_eq!(rows(&sets, "fanout"), [["1", "#2"], ["2", "#1"], ["12", "#1"]]);
    assert_eq!(rows(&sets, "next"), [["1", "#1"], ["2", "#1"], ["12", "#1"]]);
    assert_eq!(rows(&sets, "busy"), [["1", "#1"], ["2", "#0"], ["12", "#0"]]);
    assert_eq!(rows(&sets, "named"), [["1", "#1"], ["2", "#1"], ["12", "#1"]]);
    Ok(())
}

#[test]
fn a_run_past_its_row_cap_stops_naming_the_relation_and_the_rule() -> Result<(), Error<GraphError>> {
    let program = Program::<Graph>::parse(
        "(rule (path a b) (edge a b))
         (rule (path a c) (path a b) (path b c))",
    )?;
    let edges = Edges(vec![(1, 2), (2, 3), (3, 4), (4, 5)]);
    let run = |rows: usize| crate::run_with(&program, &edges, crate::Limits { rows });
    // 4 `edge` answers kept and 10 `path` rows.
    let (db, _) = run(20)?;
    assert_eq!(db.relations["path"].rows().len(), 10);
    let error = run(10).err().expect("10 rows hold the edges and the first 6 paths only");
    assert!(
        matches!(error, Error::TooManyRows { ref relation, ref line, cap: 10 } if *line == 2 && relation == "path"),
        "{error}"
    );
    let error = run(3).err().expect("the 4 `edge` answers alone are past 3");
    assert!(matches!(error, Error::TooManySourceRows { ref line, cap: 3 } if *line == 1), "{error}");
    Ok(())
}

/// Rules files by key, read from memory; `base` is what an include's path is relative to.
struct Library {
    base: crate::Base<'static>,
    files: Vec<(String, String)>,
}

impl crate::Includes for Library {
    fn base(&self) -> crate::Base<'_> {
        self.base
    }

    fn read<R>(&self, key: &str, with: impl FnOnce(&str) -> R) -> std::io::Result<Option<R>> {
        Ok(self.files.iter().find(|(path, _)| path == key).map(|(_, text)| with(text)))
    }
}

fn load(library: &Library, src: &str) -> Result<Program<Graph>, Error<GraphError>> {
    Program::<Graph>::load(src, library)
}

#[test]
fn an_include_beside_its_file_reads_the_path_relative_to_that_files_directory() {
    let files = vec![
        ("rules/lib/edges.astlog".to_owned(), "(include \"more/hop.astlog\")\n".to_owned()),
        ("rules/lib/more/hop.astlog".to_owned(), "(rule (hop a b) (edge a b))\n".to_owned()),
    ];
    let beside = Library { base: crate::Base::Beside(Some("rules/main.astlog")), files };
    let program = load(&beside, "(include \"lib/edges.astlog\")\n(rule (r a) (hop a b))")
        .expect("each path read beside the file that names it");
    assert_eq!(program.rules.len(), 2);
    assert_eq!(program.rules[0].line.to_string(), "rules/lib/more/hop.astlog:1");
    // The same rules text with no file of its own has nothing to read beside.
    let nowhere = Library { base: crate::Base::Beside(None), files: beside.files };
    let error = load(&nowhere, "(include \"lib/edges.astlog\")").err().expect("no base");
    assert!(matches!(error, Error::IncludeNowhere { .. }), "{error}");
    let error = Program::<Graph>::parse("(include \"lib/edges.astlog\")").err().expect("no base");
    assert!(matches!(error, Error::IncludeNowhere { .. }), "{error}");
}

#[test]
fn an_include_path_must_be_relative_slash_separated_and_stay_below_its_base() {
    let library = Library { base: crate::Base::Root, files: Vec::new() };
    for (path, why) in [
        ("", "empty"),
        ("/etc/rules.astlog", "absolute"),
        ("lib\\a.astlog", "`\\`"),
        ("lib//a.astlog", "empty segment"),
        ("lib/../a.astlog", "`..`"),
        ("./a.astlog", "`.`"),
    ] {
        let error = load(&library, &format!("(include \"{}\")", path.replace('\\', "\\\\")))
            .err()
            .expect(path);
        assert!(matches!(error, Error::IncludePath { .. }), "{path}: {error}");
        assert!(error.to_string().contains(why), "{path}: {error}");
    }
    let error = load(&library, "(include lib)").err().expect("an atom is no path");
    assert!(error.to_string().contains("include form is (include \"path\")"), "{error}");
}

#[test]
fn a_load_past_its_include_caps_is_refused_naming_the_include() {
    let max = crate::MAX_INCLUDED_FILES;
    // A chain: file i includes file i + 1.
    let chain = |files: usize| Library {
        base: crate::Base::Root,
        files: (0..files)
            .map(|at| {
                let next = if at + 1 < files { format!("(include \"f{}\")\n", at + 1) } else { String::new() };
                (format!("f{at}"), next)
            })
            .collect(),
    };
    assert!(load(&chain(max), "(include \"f0\")").is_ok(), "{max} files are read");
    let error = load(&chain(max + 1), "(include \"f0\")").err().expect("one file past the cap");
    assert!(
        matches!(error, Error::TooManyIncludes { ref line, ref path, max: m } if m == max && path == &format!("f{max}") && line.file() == Some(&*format!("f{}", max - 1))),
        "{error}"
    );
    let half = crate::MAX_RULES_BYTES / 2;
    let big = |n: usize| format!("{}\n", ";".repeat(n));
    let library = Library {
        base: crate::Base::Root,
        files: vec![("a".to_owned(), big(half)), ("b".to_owned(), big(half))],
    };
    assert!(load(&library, "(include \"a\")").is_ok(), "half the cap and the include");
    let error = load(&library, "(include \"a\")\n(include \"b\")").err().expect("past the byte cap");
    assert!(
        matches!(error, Error::TooMuchRules { ref line, ref path, .. } if *line == 2 && path == "b"),
        "{error}"
    );
}

#[test]
fn the_nesting_cap_holds_each_included_file_on_its_own() {
    let max = crate::sexpr::MAX_DEPTH;
    let library = Library {
        base: crate::Base::Root,
        files: vec![
            ("deep".to_owned(), nested(max)),
            ("deeper".to_owned(), format!("\n{}", nested(max + 1))),
        ],
    };
    // An include sits one list deep in the including file; the included file's forms start
    // from none, so `deep` reads and is refused only as no rule.
    let error = load(&library, "(include \"deep\")").err().expect("`((x))` is no form");
    assert!(error.to_string().starts_with("deep:1: top-level form must start"), "{error}");
    let error = load(&library, "(include \"deeper\")").err().expect("past the cap in its file");
    assert_eq!(
        error.to_string(),
        format!("deeper:2: this `(` is nested past {max} lists, the deepest a rules file may nest")
    );
}
