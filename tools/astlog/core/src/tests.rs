use crate::{Severity, Value, analyze};

type TestResult = Result<(), Box<dyn std::error::Error>>;

const RUST_SAMPLE: &str = r"
fn fallible() -> Result<u32, String> {
    let n = compute().unwrap();
    Ok(n + 1)
}

fn infallible() -> u32 {
    compute().unwrap()
}

fn compute() -> Result<u32, String> {
    Ok(41)
}
";

const UNWRAP_RULES: &str = r#"
; `<e>.unwrap()` call sites
(rule (unwrap-call call e)
  (match rust "
    (call_expression
      function: (field_expression value: (_) @e field: (field_identifier) @m)
      arguments: (arguments)) @call")
  (text m "unwrap"))

; functions whose return type is Result<...>
(rule (result-fn f)
  (match rust "
    (function_item return_type: (generic_type type: (type_identifier) @r)) @f")
  (text r "Result"))

; the join: an unwrap call lexically inside a Result-returning function
(rule (fixable call e)
  (unwrap-call call e)
  (result-fn f)
  (ancestor f call))

(rewrite unwrap-to-try (fixable call e)
  (replace call "{e}?"))
"#;

fn write_sample(dir: &tempfile::TempDir, name: &str, content: &str) -> TestResult {
    std::fs::write(dir.path().join(name), content)?;
    Ok(())
}

fn only_node_text(
    name: &str,
    source: &str,
    rules: &str,
    relation: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    write_sample(&dir, name, source)?;
    let analysis = analyze(rules, &[dir.path().to_path_buf()])?;
    let rows = analysis.database.relations[relation].rows();
    assert_eq!(rows.len(), 1, "{relation} should contain exactly one row");
    let Value::Node(node) = &rows[0][0] else {
        return Err(format!("{relation} column 0 should be a node").into());
    };
    Ok(analysis.corpus.node_text(*node).to_owned())
}

#[test]
fn unwrap_join_finds_only_result_functions() -> TestResult {
    let dir = tempfile::tempdir()?;
    write_sample(&dir, "sample.rs", RUST_SAMPLE)?;
    let analysis = analyze(UNWRAP_RULES, &[dir.path().to_path_buf()])?;

    let unwrap_calls = &analysis.database.relations["unwrap-call"];
    assert_eq!(unwrap_calls.rows().len(), 2, "both unwrap sites match");

    let fixable = &analysis.database.relations["fixable"];
    assert_eq!(fixable.rows().len(), 1, "only the Result-returning fn joins");
    let Value::Node(call) = &fixable.rows()[0][0] else {
        return Err("fixable column 0 should be a node".into());
    };
    assert_eq!(analysis.corpus.node_text(*call), "compute().unwrap()");
    Ok(())
}

#[test]
fn rewrite_splices_template() -> TestResult {
    let dir = tempfile::tempdir()?;
    write_sample(&dir, "sample.rs", RUST_SAMPLE)?;
    let analysis = analyze(UNWRAP_RULES, &[dir.path().to_path_buf()])?;

    assert_eq!(analysis.edits.len(), 1);
    let rewritten = analysis.rewritten();
    assert_eq!(rewritten.len(), 1);
    assert!(rewritten[0].content.contains("let n = compute()?;"));
    assert!(
        rewritten[0].content.contains("compute().unwrap()"),
        "the infallible fn keeps its unwrap"
    );
    let diff = analysis.diff();
    assert!(diff.contains("-    let n = compute().unwrap();"));
    assert!(diff.contains("+    let n = compute()?;"));
    Ok(())
}

#[test]
fn value_join_on_same_text() -> TestResult {
    let source = r#"
fn main() {
    let secret = std::env::var("KEY");
    let safe = "literal";
    run(secret);
    run(safe);
    run(other);
}
"#;
    // join let-bound names against call arguments on identifier text
    let rules = r#"
(rule (env-bound v)
  (match rust "
    (let_declaration
      pattern: (identifier) @v
      value: (call_expression function: (scoped_identifier name: (identifier) @f)))")
  (text f "var"))

(rule (call-arg call v)
  (match rust "(call_expression arguments: (arguments (identifier) @v)) @call"))

(rule (tainted-call call)
  (call-arg call v)
  (env-bound w)
  (same-text v w))
"#;
    let dir = tempfile::tempdir()?;
    write_sample(&dir, "sample.rs", source)?;
    let analysis = analyze(rules, &[dir.path().to_path_buf()])?;

    let tainted = &analysis.database.relations["tainted-call"];
    assert_eq!(tainted.rows().len(), 1, "only run(secret) joins");
    let Value::Node(call) = &tainted.rows()[0][0] else {
        return Err("tainted-call column 0 should be a node".into());
    };
    assert_eq!(analysis.corpus.node_text(*call), "run(secret)");
    Ok(())
}

#[test]
fn recursive_rules_reach_fixpoint() -> TestResult {
    let source = "fn main() { if true { loop { break; } } }\n";
    // `up` is recursive: parent plus transitive step. It must agree with the
    // ancestor builtin once both are restricted to the same endpoints.
    let rules = r#"
(rule (brk b) (match rust "(break_expression) @b"))
(rule (fun f) (match rust "(function_item) @f"))

(rule (up x y) (brk y) (parent x y))
(rule (up x z) (up y z) (parent x y))

(rule (reaches f b) (fun f) (brk b) (up f b))
(rule (reaches-builtin f b) (fun f) (brk b) (ancestor f b))
"#;
    let dir = tempfile::tempdir()?;
    write_sample(&dir, "sample.rs", source)?;
    let analysis = analyze(rules, &[dir.path().to_path_buf()])?;

    let recursive = &analysis.database.relations["reaches"];
    let builtin = &analysis.database.relations["reaches-builtin"];
    assert_eq!(recursive.rows().len(), 1);
    assert_eq!(recursive.rows(), builtin.rows());
    Ok(())
}

#[test]
fn predicates_are_rejected_with_guidance() {
    let rules = r#"
(rule (m x)
  (match rust "((identifier) @x (#eq? @x \"foo\"))"))
"#;
    let result = analyze(rules, &[]);
    let Err(error) = result else {
        panic!("predicate query must be rejected");
    };
    let message = error.to_string();
    assert!(message.contains("predicates"), "got: {message}");
}

#[test]
fn unknown_relation_is_a_load_error() {
    let rules = "(rule (a x) (no-such-rel x))";
    let Err(error) = analyze(rules, &[]) else {
        panic!("unknown relation must be rejected");
    };
    assert!(error.to_string().contains("no-such-rel"));
}

#[test]
fn query_comments_are_not_predicates() -> TestResult {
    // A `;` comment may contain `#` (and `@`): tree-sitter accepts it, so the
    // predicate scan and the capture scan must skip comments rather than
    // reject the rule or invent a capture name.
    let rules = "
(rule (brk b)
  (match rust \"
    ; matches #break statements, reported as @b
    (break_expression) @b\"))
";
    let dir = tempfile::tempdir()?;
    write_sample(&dir, "sample.rs", "fn main() { loop { break; } }\n")?;
    let analysis = analyze(rules, &[dir.path().to_path_buf()])?;
    assert_eq!(analysis.database.relations["brk"].rows().len(), 1);
    Ok(())
}

#[test]
fn builtin_arity_gets_the_builtin_message() {
    let rules = "(rule (a x) (text x))";
    let Err(error) = analyze(rules, &[]) else {
        panic!("wrong-arity builtin must be rejected");
    };
    let message = error.to_string();
    assert!(message.contains("builtin `text` takes 2 arguments, got 1"), "got: {message}");
}

#[test]
fn text_match_filters_by_regex() -> TestResult {
    let source = "fn main() { run(fetch_url); run(parse); run(fetch_git); }\n";
    let rules = r#"
(rule (fetchy x)
  (match rust "(call_expression arguments: (arguments (identifier) @x))")
  (text-match x "^fetch_"))
"#;
    let dir = tempfile::tempdir()?;
    write_sample(&dir, "sample.rs", source)?;
    let analysis = analyze(rules, &[dir.path().to_path_buf()])?;
    let fetchy = &analysis.database.relations["fetchy"];
    assert_eq!(fetchy.rows().len(), 2, "fetch_url and fetch_git match");
    Ok(())
}

#[test]
fn text_match_rejects_invalid_regex() -> TestResult {
    let rules = r#"
(rule (m x)
  (match rust "(identifier) @x")
  (text-match x "(unclosed"))
"#;
    let dir = tempfile::tempdir()?;
    write_sample(&dir, "sample.rs", "fn main() {}\n")?;
    let Err(error) = analyze(rules, &[dir.path().to_path_buf()]) else {
        return Err("invalid regex must be rejected".into());
    };
    let message = error.to_string();
    assert!(message.contains("invalid regex"), "got: {message}");
    assert!(message.contains("rules:4"), "got: {message}");
    Ok(())
}

#[test]
fn text_match_pattern_must_be_a_literal() {
    let rules = r#"
(rule (m x p)
  (match rust "(identifier) @x (identifier) @p")
  (text-match x p))
"#;
    let Err(error) = analyze(rules, &[]) else {
        panic!("variable text-match pattern must be rejected");
    };
    let message = error.to_string();
    assert!(message.contains("string literal"), "got: {message}");
}

#[test]
fn node_query_predicates_select_expected_source() -> TestResult {
    let cases = [
        (
            "sample.rs",
            "
fn clean() { safe(); }
fn dirty() { danger(); }
",
            r#"
(rule (danger-free f)
  (match rust "(function_item) @f")
  (no-descendant f "identifier" "danger"))
"#,
            "danger-free",
            "clean",
        ),
        (
            "sample.rs",
            "
#[inline]
fn documented() {}

/// a doc
#[inline]
fn with_doc() {}

#[inline]
// an explanatory comment
fn with_comment() {}

fn undocumented() {}
",
            r#"
(rule (a-def c) (match rust "(function_item) @c"))
(rule (inline-attached c) (a-def c) (attached-sibling c "identifier" "inline"))
(rule (not-inline c) (a-def c) (not (inline-attached c)))
"#,
            "not-inline",
            "undocumented",
        ),
        (
            "sample.rs",
            "
fn keep(x: u32) -> u32 { x }
fn drop(y: u32) -> u32 { y }
const KEPT: u32 = keep(1);
",
            r#"
(rule (def-name d n)
  (match rust "(function_item name: (identifier) @n) @d"))
(rule (called n)
  (match rust "(call_expression function: (identifier) @f)")
  (text f n))
(rule (uncalled d)
  (def-name d n)
  (text n t)
  (not (called t)))
"#,
            "uncalled",
            "drop",
        ),
    ];

    for (name, source, rules, relation, expected) in cases {
        let actual = only_node_text(name, source, rules, relation)?;
        assert!(
            actual.contains(expected),
            "{relation} should select source containing {expected:?}, got {actual:?}"
        );
    }
    Ok(())
}

#[test]
fn unsafe_negation_forms_are_rejected() {
    let cases = [
        (
            "unbound rule variable",
            r#"(rule (thing x) (match rust "(identifier) @x"))
(rule (bad x) (match rust "(identifier) @x") (not (thing y)))"#,
        ),
        (
            "binding after negation",
            r#"(rule (named n) (match rust "(identifier) @n"))
(rule (other t) (match rust "(identifier) @x") (text x t))
(rule (bad n) (named n) (not (other t)) (text n t))"#,
        ),
        (
            "unbound rewrite variable",
            r#"(rule (thing x) (match rust "(identifier) @x"))
(rewrite r (match rust "(identifier) @x") (not (thing y)) (replace x "z"))"#,
        ),
    ];
    for (name, rules) in cases {
        let error = analyze(rules, &[]).expect_err(name);
        assert!(
            error.to_string().contains("only inside"),
            "{name}: expected unsafe-negation error, got {error}"
        );
    }
}

#[test]
fn negation_through_recursion_is_rejected() {
    let rules = r#"
(rule (p x) (match rust "(identifier) @x"))
(rule (p x) (match rust "(identifier) @x") (not (p x)))
"#;
    let Err(error) = analyze(rules, &[]) else {
        panic!("negation through recursion must be rejected");
    };
    let message = error.to_string();
    assert!(
        message.contains("stratif") || message.contains("recursion"),
        "expected unstratifiable error, got: {message}"
    );
}

/// One rule flagging every `.unwrap()` receiver, lint-declared as an error
/// with a `{e}` splice.
const LINT_RULES: &str = r#"
(rule (unwrap-call call e)
  (match rust "
    (call_expression
      function: (field_expression value: (_) @e field: (field_identifier) @m)
      arguments: (arguments)) @call")
  (text m "unwrap"))

(lint unwrap-call error "do not unwrap `{e}`")
"#;

#[test]
fn lint_unknown_relation_is_a_load_error() {
    let rules = r#"(lint no-such-rel error "boom")"#;
    let Err(error) = analyze(rules, &[]) else {
        panic!("lint on an unknown relation must be rejected");
    };
    let message = error.to_string();
    assert!(message.contains("no-such-rel"), "got: {message}");
    assert!(message.contains("rules:1"), "got: {message}");
}

#[test]
fn lint_bad_severity_is_a_typed_error() {
    let rules = r#"
(rule (brk b) (match rust "(break_expression) @b"))
(lint brk fatal "no breaks")
"#;
    let Err(error) = analyze(rules, &[]) else {
        panic!("bad lint severity must be rejected");
    };
    let crate::Error::LintSeverity { got, line } = error else {
        panic!("expected LintSeverity, got: {error}");
    };
    assert_eq!(got, "fatal");
    assert_eq!(line, 3);
}

#[test]
fn lint_template_var_must_be_a_head_variable() {
    let rules = r#"
(rule (brk b) (match rust "(break_expression) @b"))
(lint brk error "break at {other}")
"#;
    let Err(error) = analyze(rules, &[]) else {
        panic!("lint message variable outside the head must be rejected");
    };
    let crate::Error::LintVar { relation, var, .. } = error else {
        panic!("expected LintVar, got: {error}");
    };
    assert_eq!(relation, "brk");
    assert_eq!(var, "other");
}

#[test]
fn scan_emits_located_findings_with_spliced_message() -> TestResult {
    let dir = tempfile::tempdir()?;
    write_sample(&dir, "sample.rs", RUST_SAMPLE)?;
    let analysis = analyze(LINT_RULES, &[dir.path().to_path_buf()])?;
    let findings = analysis.findings()?;
    assert_eq!(findings.len(), 2, "both unwrap sites become findings");
    let first = &findings[0];
    assert_eq!(first.rule, "unwrap-call");
    assert_eq!(first.severity, Severity::Error);
    assert_eq!(first.message, "do not unwrap `compute()`");
    assert_eq!(first.text, "compute().unwrap()");
    assert_eq!((first.line, first.column), (3, 13));
    assert!(first.end_line == 3 && first.end_column > first.column);
    assert!(
        findings
            .windows(2)
            .all(|pair| { (&pair[0].file, pair[0].line) <= (&pair[1].file, pair[1].line) }),
        "findings are sorted by position"
    );
    Ok(())
}

#[test]
fn scan_requires_a_node_column() -> TestResult {
    // The lint relation's only column is derived text, so a finding has no
    // location: a typed error, not a panic.
    let rules = r#"
(rule (kinds k)
  (match rust "(break_expression) @b")
  (kind b k))
(lint kinds error "saw kind {k}")
"#;
    let dir = tempfile::tempdir()?;
    write_sample(&dir, "sample.rs", "fn main() { loop { break; } }\n")?;
    let analysis = analyze(rules, &[dir.path().to_path_buf()])?;
    let Err(error) = analysis.findings() else {
        return Err("a lint row without a node column must fail scan".into());
    };
    let crate::Error::LintNoNode { rule, .. } = error else {
        return Err(format!("expected LintNoNode, got: {error}").into());
    };
    assert_eq!(rule, "kinds");
    Ok(())
}

#[test]
fn suppression_filters_same_line_and_line_below() -> TestResult {
    let source = "
fn f() -> u32 {
    a().unwrap(); // astlog-ignore
    // astlog-ignore
    b().unwrap();
    c().unwrap();
    0
}
";
    let dir = tempfile::tempdir()?;
    write_sample(&dir, "sample.rs", source)?;
    let analysis = analyze(LINT_RULES, &[dir.path().to_path_buf()])?;
    let findings = analysis.findings()?;
    assert_eq!(findings.len(), 1, "only the uncommented site survives");
    assert_eq!(findings[0].text, "c().unwrap()");
    // The rows themselves are untouched: suppression filters emission only.
    assert_eq!(analysis.database.relations["unwrap-call"].rows().len(), 3);
    Ok(())
}

#[test]
fn suppressed_reports_the_originating_comment() -> TestResult {
    // The trailing comment suppresses the unwrap on its own line; `findings`
    // omits that row and `suppressed` reports it with the comment that hid it.
    // A blank line below the suppressed site keeps the trailing comment's
    // line-below coverage off the second unwrap.
    let source = "
fn f() -> u32 {
    a().unwrap(); // astlog-ignore: unwrap-call (legacy, drop me)

    b().unwrap();
    0
}
";
    let dir = tempfile::tempdir()?;
    write_sample(&dir, "sample.rs", source)?;
    let analysis = analyze(LINT_RULES, &[dir.path().to_path_buf()])?;

    let findings = analysis.findings()?;
    assert_eq!(findings.len(), 1, "only the uncommented site is reported");
    assert_eq!(findings[0].text, "b().unwrap()");

    let suppressed = analysis.suppressed()?;
    assert_eq!(suppressed.len(), 1, "exactly the hidden site is listed");
    let only = &suppressed[0];
    assert_eq!(only.finding.text, "a().unwrap()");
    assert_eq!(only.finding.line, 3);
    assert_eq!(only.comment_line, 3, "trailing comment shares the row's line");
    assert_eq!(only.comment_text, "// astlog-ignore: unwrap-call (legacy, drop me)");
    Ok(())
}

#[test]
fn named_suppression_matches_only_its_rules() -> TestResult {
    // Blank lines isolate each site: a trailing suppression also covers the
    // line below it, and these cases must not bleed into each other.
    let source = "
fn f() -> u32 {
    a().unwrap(); // astlog-ignore: unwrap-call

    b().unwrap(); // astlog-ignore: some-other-rule

    c().unwrap(); // astlog-ignore: some-other-rule, unwrap-call

    0
}
";
    let dir = tempfile::tempdir()?;
    write_sample(&dir, "sample.rs", source)?;
    let analysis = analyze(LINT_RULES, &[dir.path().to_path_buf()])?;
    let findings = analysis.findings()?;
    assert_eq!(findings.len(), 1, "the wrong-name suppression must not suppress");
    assert_eq!(findings[0].text, "b().unwrap()");
    Ok(())
}

#[test]
fn bare_suppression_reports_comment_via_all_directive() -> TestResult {
    // A bare `astlog-ignore` (no rule list) takes the `Directive::All` branch,
    // which records the originating comment separately from the named path.
    // `suppressed` must still report that comment so an audit shows what an
    // unscoped ignore is hiding.
    let source = "
fn f() -> u32 {
    a().unwrap(); // astlog-ignore

    b().unwrap();
    0
}
";
    let dir = tempfile::tempdir()?;
    write_sample(&dir, "sample.rs", source)?;
    let analysis = analyze(LINT_RULES, &[dir.path().to_path_buf()])?;

    let suppressed = analysis.suppressed()?;
    assert_eq!(suppressed.len(), 1, "exactly the hidden site is listed");
    let only = &suppressed[0];
    assert_eq!(only.finding.text, "a().unwrap()");
    assert_eq!(only.comment_line, 3);
    assert_eq!(only.comment_text, "// astlog-ignore");
    Ok(())
}

#[test]
fn warning_severity_is_carried_through() -> TestResult {
    let rules = r#"
(rule (brk b) (match rust "(break_expression) @b"))
(lint brk warning "loop break")
"#;
    let dir = tempfile::tempdir()?;
    write_sample(&dir, "sample.rs", "fn main() { loop { break; } }\n")?;
    let analysis = analyze(rules, &[dir.path().to_path_buf()])?;
    let findings = analysis.findings()?;
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].severity, Severity::Warning);
    assert_eq!(findings[0].message, "loop break");
    Ok(())
}

#[test]
fn file_path_scopes_a_rule_to_a_tree() -> TestResult {
    let dir = tempfile::tempdir()?;
    std::fs::create_dir(dir.path().join("tests"))?;
    write_sample(&dir, "lib.rs", "fn a() {}\n")?;
    write_sample(&dir, "tests/it.rs", "fn b() {}\n")?;
    let rules = r#"
(rule (test-fn f)
  (match rust "(function_item) @f")
  (file-path f p)
  (text-match p "(^|/)tests/"))
"#;
    let analysis = analyze(rules, &[dir.path().to_path_buf()])?;
    let rows = analysis.database.relations["test-fn"].rows();
    assert_eq!(rows.len(), 1, "only the fn under tests/ joins");
    let Value::Node(node) = &rows[0][0] else {
        return Err("test-fn column 0 should be a node".into());
    };
    assert_eq!(analysis.corpus.node_text(*node), "fn b() {}");
    Ok(())
}

#[test]
fn no_descendant_is_strict() -> TestResult {
    // The root node itself never counts as its own descendant, even when
    // kind and text both coincide.
    let source = "fn main() { x; }\n";
    let rules = r#"
(rule (ident-without-self x)
  (match rust "(identifier) @x")
  (text x "x")
  (no-descendant x "identifier" "x"))
"#;
    let dir = tempfile::tempdir()?;
    write_sample(&dir, "sample.rs", source)?;
    let analysis = analyze(rules, &[dir.path().to_path_buf()])?;
    let rows = analysis.database.relations["ident-without-self"].rows();
    assert_eq!(rows.len(), 1, "the identifier has no matching descendant");
    Ok(())
}

#[test]
fn misgrouped_digits_flags_and_regroups_toml_literals() -> TestResult {
    // TOML lexes an underscore literal as one integer or float node, so one
    // literal is one node, one finding, one edit.
    let source = r#"ungrouped = 100000
misgrouped = 1_0000
misgroupedFrac = 1_0000.000001
fine = 10_000
short = 1000
frac = 0.000001
fracFine = 1_000.000_1
quoted = "100000"
"#;
    let rules = r#"
(rule (needs-grouping n fixed)
  (match toml "[(integer) (float)] @n")
  (misgrouped-digits n fixed))
(lint needs-grouping error "group digits in threes: `{fixed}`")
(rewrite regroup (needs-grouping n fixed)
  (replace n "{fixed}"))
"#;
    let dir = tempfile::tempdir()?;
    write_sample(&dir, "sample.toml", source)?;
    let analysis = analyze(rules, &[dir.path().to_path_buf()])?;

    let findings = analysis.findings()?;
    let texts: Vec<(&str, &str)> =
        findings.iter().map(|finding| (finding.text.as_str(), finding.message.as_str())).collect();
    assert_eq!(
        texts,
        vec![
            ("100000", "group digits in threes: `100_000`"),
            ("1_0000", "group digits in threes: `10_000`"),
            ("1_0000.000001", "group digits in threes: `10_000.000_001`"),
            ("0.000001", "group digits in threes: `0.000_001`"),
        ],
        "one finding per literal, none for grouped/short/string literals"
    );

    let rewritten = analysis.rewritten();
    assert_eq!(rewritten.len(), 1);
    let content = &rewritten[0].content;
    assert!(content.contains("ungrouped = 100_000\n"));
    assert!(content.contains("misgrouped = 10_000\n"));
    assert!(content.contains("misgroupedFrac = 10_000.000_001\n"));
    assert!(content.contains("frac = 0.000_001\n"));
    assert!(content.contains("fine = 10_000\n"), "grouped literal untouched");
    assert!(content.contains("short = 1000\n"), "four digits stay bare");
    assert!(content.contains("fracFine = 1_000.000_1\n"), "grouped fraction untouched");
    assert!(content.contains("quoted = \"100000\"\n"), "string contents untouched");
    Ok(())
}

#[test]
fn misgrouped_digits_handles_exponents_and_rejects_non_numbers() -> TestResult {
    // Exponent digits group from the right like the integer part; an array
    // element is still a literal of its own, and non-number text (the
    // array's text) never unifies.
    let source = "expUngrouped = 2.5e1_0
expFine = 2.5e10_000
applied = [100000, true]
";
    let rules = r#"
(rule (misgrouped-number n fixed)
  (match toml "[(integer) (float) (array)] @n")
  (misgrouped-digits n fixed))
"#;
    let dir = tempfile::tempdir()?;
    write_sample(&dir, "sample.toml", source)?;
    let analysis = analyze(rules, &[dir.path().to_path_buf()])?;
    let rows = analysis.database.relations["misgrouped-number"].rows();
    let mut derived: Vec<(String, String)> = rows
        .iter()
        .map(|row| {
            let Value::Node(node) = &row[0] else {
                panic!("misgrouped-number column 0 should be a node");
            };
            let Value::Text(fixed) = &row[1] else {
                panic!("misgrouped-number column 1 should be text");
            };
            (analysis.corpus.node_text(*node).to_owned(), fixed.to_string())
        })
        .collect();
    derived.sort();
    assert_eq!(
        derived,
        vec![
            ("100000".to_owned(), "100_000".to_owned()),
            ("2.5e1_0".to_owned(), "2.5e10".to_owned()),
        ],
        "the misgrouped exponent and the array's literal fire; the canonical exponent and the array do not"
    );
    Ok(())
}
