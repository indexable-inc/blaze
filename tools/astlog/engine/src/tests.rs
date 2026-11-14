//! The engine against a synthetic source: a graph whose `(edge a b)` rows the test chooses and
//! whose `(succ a b)` is demand-driven on `a`.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

use blaze_sim::{Config, Tape};

use crate::eval::{Facts, Row, Value};
use crate::program::{
    Language, Needs, Parsed, Program, Segment, TemplateSyntax, parse_template, terms,
};
use crate::{Error, Sexpr};

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
        _line: usize,
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
        _line: usize,
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
        _line: usize,
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
    let at_cap = crate::sexpr::parse::<GraphError>(&nested(max));
    assert!(at_cap.is_ok(), "{max} levels read: {at_cap:?}");
    let text = format!("(rule (r a) (edge a b))\n{}", nested(max + 1));
    let past = Program::<Graph>::parse(&text).expect_err("one level past the cap");
    assert!(matches!(past, Error::TooDeep { line: 2, .. }), "{past}");
    assert!(past.to_string().starts_with("rules:2: ") && past.to_string().contains("nested"));
    // Far past the cap is refused the same way, not by the reader overflowing its stack.
    let far = crate::sexpr::parse::<GraphError>(&nested(1_000_000)).expect_err("far past the cap");
    assert!(matches!(far, Error::TooDeep { line: 1, .. }), "{far}");
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
fn demanded_argument_must_be_bound_on_the_left() {
    let error = Program::<Graph>::parse("(rule (next a b) (succ a b) (edge a b))")
        .expect_err("succ is demand-driven on a");
    let message = error.to_string();
    assert!(message.contains("`succ` needs `a` bound"), "got: {message}");
    let program = Program::<Graph>::parse("(rule (next a b) (edge a x) (succ a b))");
    assert!(program.is_ok(), "bound on the left is fine");
}

#[test]
fn dollar_templates_splice_and_escape() -> Result<(), Error<GraphError>> {
    let template =
        parse_template::<GraphError>(TemplateSyntax::Dollar, "$a + ${b}c $$ $$$args", 1)?;
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
    let stray = parse_template::<GraphError>(TemplateSyntax::Dollar, "cost $5", 1);
    assert!(stray.is_err(), "`$5` is no variable");
    Ok(())
}
