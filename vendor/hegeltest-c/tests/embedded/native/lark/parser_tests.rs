//! Direct-equivalent tests for `src/native/lark/parser.rs`.

use super::*;
use alloc::string::ToString;
use alloc::vec;

fn parse(text: &str) -> Vec<Statement> {
    parse_grammar(text).unwrap()
}

fn err(text: &str) -> String {
    parse_grammar(text).unwrap_err().message
}

fn rule_sym(name: &str) -> Expr {
    Expr::Symbol(Symbol::Rule(name.to_string()))
}

fn term_sym(name: &str) -> Expr {
    Expr::Symbol(Symbol::Terminal(name.to_string()))
}

fn lit(s: &str) -> Expr {
    Expr::Literal(Pattern::str(s))
}

fn alt(items: Vec<Expr>) -> Alternative {
    Alternative { items, alias: None }
}

fn alts(alternatives: Vec<Alternative>) -> Expr {
    Expr::Alternatives(alternatives)
}

fn repeat(inner: Expr, min: u32, max: Option<u32>) -> Expr {
    Expr::Repeat {
        inner: Box::new(inner),
        min,
        max,
    }
}

fn single_rule(text: &str) -> RuleDef {
    match parse(text).remove(0) {
        Statement::Define(Definition::Rule(rule)) => rule,
        other => panic!("expected a rule, got {other:?}"),
    }
}

fn single_term(text: &str) -> TermDef {
    match parse(text).remove(0) {
        Statement::Define(Definition::Terminal(term)) => term,
        other => panic!("expected a terminal, got {other:?}"),
    }
}

#[test]
fn rule_with_sequence_and_alternatives() {
    let rule = single_rule("start: a B \"c\" | d\n  | /e/i");
    assert_eq!(rule.name, "start");
    assert_eq!(
        rule.body,
        alts(vec![
            alt(vec![rule_sym("a"), term_sym("B"), lit("c")]),
            alt(vec![rule_sym("d")]),
            alt(vec![Expr::Literal(Pattern::re("e").with_flags("i"))]),
        ])
    );
    assert_eq!(rule.modifiers, RuleModifiers::default());
    assert_eq!(rule.priority, None);
    assert!(rule.params.is_empty());
}

#[test]
fn empty_alternatives() {
    let rule = single_rule("a: | \"x\" |");
    assert_eq!(
        rule.body,
        alts(vec![alt(vec![]), alt(vec![lit("x")]), alt(vec![])])
    );
    assert!(single_rule("a:").body.is_single_empty());
}

#[test]
fn modifiers_priority_and_params() {
    let rule = single_rule("?!tmpl{a, b}.3: a b");
    assert_eq!(rule.name, "tmpl");
    assert_eq!(rule.params, vec!["a".to_string(), "b".to_string()]);
    assert_eq!(rule.priority, Some(3));
    assert!(rule.modifiers.expand1);
    assert!(rule.modifiers.keep_all_tokens);
}

#[test]
fn inlined_rule_cannot_expand() {
    assert!(err("?_a: b").contains("Inlined rules (_rule) cannot use the ?rule modifier."));
}

#[test]
fn operators() {
    let rule = single_rule("a: b? c* d+ e~2 f~1..3 [g] (h | i)");
    assert_eq!(
        rule.body,
        alts(vec![alt(vec![
            repeat(rule_sym("b"), 0, Some(1)),
            repeat(rule_sym("c"), 0, None),
            repeat(rule_sym("d"), 1, None),
            repeat(rule_sym("e"), 2, Some(2)),
            repeat(rule_sym("f"), 1, Some(3)),
            repeat(alts(vec![alt(vec![rule_sym("g")])]), 0, Some(1)),
            alts(vec![alt(vec![rule_sym("h")]), alt(vec![rule_sym("i")])]),
        ])])
    );
}

#[test]
fn bad_repeat_ranges() {
    assert!(err("a: b~3..2").contains("Bad Range for b (3..2 isn't allowed)"));
    assert!(err("a: B~3..2").contains("Bad Range for B (3..2 isn't allowed)"));
    assert!(err("a: \"x\"~3..2").contains("Bad Range for \"x\" (3..2 isn't allowed)"));
    assert!(err("a: /x/~3..2").contains("Bad Range for /x/ (3..2 isn't allowed)"));
    assert!(err("a: t{b}~3..2").contains("Bad Range for t (3..2 isn't allowed)"));
    assert!(err("a: (b c)~3..2").contains("Bad Range for group (3..2 isn't allowed)"));
    assert!(err("a: [b]~3..2").contains("Bad Range for group (3..2 isn't allowed)"));
    assert!(err("a: b~-1").contains("Bad Range for repetition (-1 isn't allowed)"));
    assert!(err("a: b~2000000").contains("Bad Range for repetition (2000000 isn't allowed)"));
    assert!(err("a: b~x").contains("Expecting a number after '~'"));
    assert!(err("a: b~2..x").contains("Expecting a number after '..'"));
    assert!(err("a: b~2..\n").contains("Expecting a number after '..'"));
}

#[test]
fn a_line_continuation_ending_the_grammar_leaves_a_definition_unfinished() {
    assert!(err("a.\\").contains("Expecting a priority number after '.'"));
    assert!(err("a: b ~ \\").contains("Expecting a number after '~'"));
    assert!(err("a: \"a\"..\\").contains("Expecting a string literal after '..'"));
}

#[test]
fn rule_modifiers_before_a_terminal_name_are_rejected() {
    for text in ["?_A: \"x\"", "!_A: \"x\"", "%override ?_A: \"x\""] {
        let message = err(text);
        assert!(
            message.contains("Expecting rule or terminal definition"),
            "{text:?}: {message}"
        );
    }
}

#[test]
fn nesting_is_capped() {
    let nested = |depth: usize| format!("a: {}b{}", "(".repeat(depth), ")".repeat(depth));
    let rule = single_rule(&nested(MAX_NESTING));
    assert_eq!(rule.name, "a");
    let message = err(&nested(MAX_NESTING + 1));
    assert!(
        message.contains("Grammar nests more than 200 levels of parentheses"),
        "{message}"
    );
    assert!(message.contains("line 1 column 204"), "{message}");
    assert!(err(&format!("a: {}b", "[".repeat(MAX_NESTING + 1))).contains("nests more than"));
    let templated = |depth: usize| format!("a: {}b{}", "t{".repeat(depth), "}".repeat(depth));
    assert_eq!(single_rule(&templated(MAX_NESTING)).name, "a");
    let message = err(&templated(MAX_NESTING + 1));
    assert!(message.contains("nests more than"), "{message}");
    assert!(message.contains("line 1 column 404"), "{message}");
}

#[test]
fn aliases() {
    let rule = single_rule("a: b -> first\n  | c d -> second\n  | e");
    let aliases: Vec<Option<String>> = match rule.body {
        Expr::Alternatives(alts) => alts.into_iter().map(|a| a.alias).collect(),
        _ => unreachable!(),
    };
    assert_eq!(
        aliases,
        vec![Some("first".to_string()), Some("second".to_string()), None]
    );
}

#[test]
fn alias_must_be_a_rule_name() {
    assert!(err("a: b -> C").contains("Alias expects lowercase name"));
    assert!(err("a: -> \"a\"").contains("Alias expects lowercase name"));
}

#[test]
fn templates_and_ranges() {
    let rule = single_rule("a: sep{b, \"x\"} \"a\"..\"z\"");
    assert_eq!(
        rule.body,
        alts(vec![alt(vec![
            Expr::Template {
                name: "sep".to_string(),
                args: vec![rule_sym("b"), lit("x")],
            },
            Expr::Literal(Pattern::re("[a-z]")),
        ])])
    );
}

#[test]
fn template_argument_errors() {
    assert!(err("a: sep{}").contains("Expecting a value"));
    assert!(err("a: sep{b").contains("Expecting '}'"));
    assert!(err("a: sep{(b)}").contains("Expecting a value"));
    assert!(err("tmpl{A}: A").contains("Expecting a template parameter name"));
    assert!(err("tmpl{a: a").contains("Expecting '}'"));
}

#[test]
fn range_needs_a_second_string() {
    assert!(err("a: \"a\"..b").contains("Expecting a string literal after '..'"));
}

#[test]
fn terminal_definitions() {
    let term = single_term("A.2: \"a\" | /b/");
    assert_eq!(term.name, "A");
    assert_eq!(term.priority, 2);
    assert_eq!(
        term.body,
        alts(vec![
            alt(vec![lit("a")]),
            alt(vec![Expr::Literal(Pattern::re("b"))])
        ])
    );
    assert_eq!(single_term("A: \"a\"").priority, 0);
}

#[test]
fn terminal_name_with_dot() {
    assert!(err("A.B: \"a\"").contains("Terminal names cannot contain dots"));
}

#[test]
fn priority_needs_a_number() {
    assert!(err("a.x: b").contains("Expecting a priority number"));
}

#[test]
fn missing_colon() {
    for text in ["a\n", "A\n", "a->\n", "A->\n", "a A\n"] {
        assert!(
            err(text).contains("Expecting rule or terminal definition (missing colon)"),
            "{text:?}"
        );
    }
}

#[test]
fn unexpected_colon() {
    for text in ["a::\n", "a: b:\n", "a: B:\n", "a: \"a\":\n"] {
        assert!(err(text).contains("Unexpected colon"), "{text:?}");
    }
}

#[test]
fn misplaced_operators() {
    for text in [
        "a: b??",
        "a: b(?)",
        "a:+\n",
        "a:?\n",
        "a:*\n",
        "a:|*\n",
        "a: b ~ 1 ~ 2",
    ] {
        assert!(err(text).contains("Misplaced operator"), "{text:?}");
    }
}

#[test]
fn parentheses_errors() {
    assert!(err("a: (\n").contains("Unclosed parenthesis"));
    assert!(err("a: [b\n").contains("Unclosed parenthesis"));
    for text in ["a: )\n", "a: [)\n", "a: (]\n", "a: b)\n"] {
        assert!(
            err(text).contains("Unmatched closing parenthesis"),
            "{text:?}"
        );
    }
}

#[test]
fn other_statement_errors() {
    assert!(err("a: 1\n").contains("Incorrect type of value"));
    assert!(
        err("a:a\n()\n").contains("Expecting option (\"|\") or a new rule or terminal definition")
    );
    assert!(err("\"a\"\n").contains("Expecting rule or terminal definition"));
    assert!(err("/a/\n").contains("Expecting rule or terminal definition"));
    assert!(err("| a\n").contains("Expecting rule or terminal definition"));
    assert!(err("%import \"a\"\n").contains("%import expects a name"));
    assert!(err("%ignore %import\n").contains("%ignore expects a value"));
    assert!(err("a: b c, d").contains("Incorrect type of value"));
    assert!(err("%override 3").contains("Expecting rule or terminal definition"));
    assert!(err("%declare\n").contains("%declare expects at least one name"));
    assert!(err("+").contains("Misplaced operator"));
}

#[test]
fn error_messages_carry_position_and_line() {
    let message = err("start: a\nb: c:\n");
    assert!(
        message.contains("Unexpected colon, at line 2 column 5"),
        "{message}"
    );
    assert!(message.ends_with("b: c:"), "{message}");
}

#[test]
fn directives() {
    let statements = parse(
        "%ignore WS\n%ignore \" \" | /\\t/\n%declare A _B\n%import common.INT\n%import common.WS -> SPACE\n%import .rel.path (a, B)\n%override a: b\n%extend A: \"c\"",
    );
    assert_eq!(
        statements[0],
        Statement::Ignore(alts(vec![alt(vec![term_sym("WS")])]))
    );
    assert_eq!(
        statements[1],
        Statement::Ignore(alts(vec![
            alt(vec![lit(" ")]),
            alt(vec![Expr::Literal(Pattern::re("\t"))])
        ]))
    );
    assert_eq!(
        statements[2],
        Statement::Declare(vec![
            Symbol::Terminal("A".to_string()),
            Symbol::Terminal("_B".to_string())
        ])
    );
    assert_eq!(
        statements[3],
        Statement::Import(ImportStmt {
            path: vec!["common".to_string()],
            relative: false,
            names: ImportNames::Single {
                name: "INT".to_string(),
                alias: None
            },
        })
    );
    assert_eq!(
        statements[4],
        Statement::Import(ImportStmt {
            path: vec!["common".to_string()],
            relative: false,
            names: ImportNames::Single {
                name: "WS".to_string(),
                alias: Some("SPACE".to_string())
            },
        })
    );
    assert_eq!(
        statements[5],
        Statement::Import(ImportStmt {
            path: vec!["rel".to_string(), "path".to_string()],
            relative: true,
            names: ImportNames::Multi(vec!["a".to_string(), "B".to_string()]),
        })
    );
    assert!(matches!(&statements[6], Statement::Override(Definition::Rule(r)) if r.name == "a"));
    assert!(matches!(&statements[7], Statement::Extend(Definition::Terminal(t)) if t.name == "A"));
}

#[test]
fn declare_accepts_rule_names_for_the_builder_to_reject() {
    assert_eq!(
        parse("%declare a")[0],
        Statement::Declare(vec![Symbol::Rule("a".to_string())])
    );
}

#[test]
fn import_errors() {
    assert!(err("%import common\n").contains("Nothing was imported from grammar `common`"));
    assert!(err("%import common.\n").contains("Expecting a name"));
    assert!(err("%import common (A\n").contains("Unclosed parenthesis"));
    assert!(err("%import common (A, 1)\n").contains("Expecting a name"));
    assert!(err("%import common.A -> \n").contains("Expecting a name"));
}

#[test]
fn blank_lines_and_comments_between_statements() {
    let statements = parse("\n\n// c\na: b\n\n# d\n\nA: \"a\"\n\n");
    assert_eq!(statements.len(), 2);
}

#[test]
fn escape_errors_surface_from_the_parser() {
    assert!(err("a: \"\"").contains("Empty terminals are not allowed"));
    assert!(err("a: /a\nb/").contains("verbose"));
}

#[test]
fn errors_at_the_end_of_the_input_point_at_the_last_token() {
    let message = err("start: (");
    assert!(message.contains("line 1 column 8"), "{message}");
}

#[test]
fn stray_tokens_after_an_alias_are_rejected() {
    assert!(err("start: a -> b *").contains("Misplaced operator"));
    assert!(err("start: a -> b c").contains("Expecting option (\"|\")"));
}

#[test]
fn an_alternative_bar_cannot_start_a_statement() {
    let message = err("\n| a");
    assert!(
        message.contains("Expecting rule or terminal definition"),
        "{message}"
    );
}

#[test]
fn a_statement_cannot_start_with_a_closing_bracket() {
    assert!(err(") a").contains("Unmatched closing parenthesis"));
    assert!(err("] a").contains("Unmatched closing parenthesis"));
}

#[test]
fn a_bare_operator_is_not_a_value() {
    let message = err("start: a | ~2");
    assert!(message.contains("Misplaced operator"), "{message}");
    let message = err("start: tmpl{:}");
    assert!(message.contains("Expecting a value"), "{message}");
}

#[test]
fn an_operator_after_a_declare_is_misplaced() {
    assert!(err("%declare A *").contains("Misplaced operator"));
}

#[test]
fn a_value_missing_at_the_end_of_the_input() {
    let message = err("start: tmpl{");
    assert!(message.contains("Expecting a value"), "{message}");
    assert!(message.contains("line 1 column 13"), "{message}");
}
