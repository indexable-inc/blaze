//! Comprehensive tests for `blaze-tab`: lossless round-trip across all `Value` variants,
//! homogeneous and sparse union-schema tables (`_` vs `null` fidelity), comments and optional
//! commas, typed `FromValue`/`ToValue` API, adversarial error and depth tests, and a 50-item
//! CI diagnostic byte-size comparison against compact JSON (`blaze-json`).

use core::alloc::Allocator;

use blaze_data::{Int, Map, Value, value};
use blaze_own::{Alloc, String, Vec, format_in};

use crate::{
    Duplicates, Expected, FromTabError, KeyOrder, Limits, ReadError, Style, TableMode, What, WriteError, WriteOptions,
    WriteWhat, line_col, read, read_typed, write, write_pretty, write_typed, write_typed_pretty, write_with,
};

fn heap() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

type V = Value<Alloc<'static>>;

fn parse(text: &str) -> V {
    match read(text.as_bytes(), Limits::SHIPPED, heap()) {
        Ok(v) => v,
        Err(err) => panic!("failed to parse {text:?}: {err}"),
    }
}

fn refuse(bytes: &[u8]) -> ReadError<Alloc<'static>> {
    match read(bytes, Limits::SHIPPED, heap()) {
        Ok(v) => panic!("expected error, got {v:?}"),
        Err(err) => err,
    }
}

fn round_trip(v: &V) -> String<Alloc<'static>> {
    let text = write(v, heap()).expect("compact write failed");
    let back = read(text.as_bytes(), Limits::SHIPPED, heap()).expect("compact read-back failed");
    assert_eq!(&back, v, "compact round-trip mismatch; text:\n{}", text.as_str());

    let pretty = write_pretty(v, heap()).expect("pretty write failed");
    let back_pretty = read(pretty.as_bytes(), Limits::SHIPPED, heap()).expect("pretty read-back failed");
    assert_eq!(&back_pretty, v, "pretty round-trip mismatch; text:\n{}", pretty.as_str());
    text
}

#[test]
fn lossless_round_trip_all_value_variants() {
    let a = heap();
    let mut all_bytes = Vec::with_capacity_in(256, a);
    for b in 0u8..=255u8 {
        all_bytes.push(b);
    }

    let mut root = Map::new_in(a);
    root.insert(String::from_str_in("null_val", a), V::Null);
    root.insert(String::from_str_in("bool_t", a), V::Bool(true));
    root.insert(String::from_str_in("bool_f", a), V::Bool(false));
    root.insert(String::from_str_in("int_zero", a), V::Int(Int::from_u64(0)));
    root.insert(String::from_str_in("int_min", a), V::Int(Int::from_i64(i64::MIN)));
    root.insert(String::from_str_in("int_max_i64", a), V::Int(Int::from_i64(i64::MAX)));
    root.insert(String::from_str_in("int_max_u64", a), V::Int(Int::from_u64(u64::MAX)));
    root.insert(String::from_str_in("int_exact_53", a), V::Int(Int::from_u64(9_007_199_254_740_993)));
    root.insert(String::from_str_in("float_pos_zero", a), V::Float(0.0));
    root.insert(String::from_str_in("float_neg_zero", a), V::Float(-0.0));
    root.insert(String::from_str_in("float_nan", a), V::Float(f64::NAN));
    root.insert(String::from_str_in("float_inf", a), V::Float(f64::INFINITY));
    root.insert(String::from_str_in("float_neg_inf", a), V::Float(f64::NEG_INFINITY));
    root.insert(String::from_str_in("float_min_pos", a), V::Float(f64::MIN_POSITIVE));
    root.insert(String::from_str_in("float_max", a), V::Float(f64::MAX));
    root.insert(String::from_str_in("float_min", a), V::Float(f64::MIN));
    root.insert(String::from_str_in("bytes_empty", a), V::Bytes(Vec::new_in(a)));
    root.insert(String::from_str_in("bytes_all", a), V::Bytes(all_bytes));
    root.insert(String::from_str_in("text_bare", a), V::Text(String::from_str_in("src/main.rs", a)));
    root.insert(String::from_str_in("text_url", a), V::Text(String::from_str_in("http://localhost:8080/v1", a)));
    root.insert(String::from_str_in("text_empty", a), V::Text(String::from_str_in("", a)));
    root.insert(String::from_str_in("text_spaces", a), V::Text(String::from_str_in("mismatched types", a)));
    root.insert(String::from_str_in("text_reserved_null", a), V::Text(String::from_str_in("null", a)));
    root.insert(String::from_str_in("text_reserved_true", a), V::Text(String::from_str_in("true", a)));
    root.insert(String::from_str_in("text_reserved_false", a), V::Text(String::from_str_in("false", a)));
    root.insert(String::from_str_in("text_reserved_inf", a), V::Text(String::from_str_in("inf", a)));
    root.insert(String::from_str_in("text_reserved_nan", a), V::Text(String::from_str_in("nan", a)));
    root.insert(String::from_str_in("text_reserved_absent", a), V::Text(String::from_str_in("_", a)));
    root.insert(String::from_str_in("text_hex_lookalike", a), V::Text(String::from_str_in("0x[deadbeef]", a)));
    root.insert(String::from_str_in("text_date_like", a), V::Text(String::from_str_in("2026-11-23", a)));
    root.insert(String::from_str_in("text_escapes", a), V::Text(String::from_str_in("\"\\\n\r\t\0\u{8}\u{c}\u{1b}é😀", a)));
    root.insert(String::from_str_in("key:with:colon", a), V::Int(Int::from_u64(7)));
    root.insert(String::from_str_in("null", a), V::Text(String::from_str_in("key_named_null", a)));
    root.insert(String::from_str_in("_", a), V::Text(String::from_str_in("key_named_underscore", a)));
    root.insert(String::from_str_in("empty_list", a), V::List(Vec::new_in(a)));
    root.insert(String::from_str_in("empty_map", a), V::Map(Map::new_in(a)));
    root.insert(
        String::from_str_in("mixed_list", a),
        value!(in a; [null, true, false, -42, 3.5, "hi there", "bare_id", [], {}]),
    );

    let v = V::Map(root);
    let compact = round_trip(&v);
    assert!(compact.as_str().contains("float_nan:nan"));
    assert!(compact.as_str().contains("float_inf:inf"));
    assert!(compact.as_str().contains("float_neg_inf:-inf"));
    assert!(compact.as_str().contains("float_neg_zero:-0.0"));
    assert!(compact.as_str().contains("bytes_empty:0x[]"));
    assert!(compact.as_str().contains("text_bare:src/main.rs"));
    assert!(compact.as_str().contains("text_reserved_absent:\"_\""));
    assert!(compact.as_str().contains("\"key:with:colon\":7"));
    assert!(compact.as_str().contains("\"null\":key_named_null"));
    assert!(compact.as_str().contains("\"_\":key_named_underscore"));
}

#[test]
fn numeric_separators_hex_formatting_and_string_ergonomics() {
    assert_eq!(parse("1_000_000").as_u64(), Some(1_000_000));
    assert_eq!(parse("-1_000_000").as_i64(), Some(-1_000_000));
    assert_eq!(parse("18_446_744_073_709_551_615").as_u64(), Some(u64::MAX));
    assert_eq!(parse("1_234.5_0").as_f64(), Some(1234.5));
    assert_eq!(parse("1_0e+1_0").as_f64(), Some(10e10));
    assert_eq!(parse("-0.0").as_f64().map(f64::to_bits), Some((-0.0f64).to_bits()));

    // Hex bytes accept whitespace, underscores, and mixed case; canonical write is lowercase compact.
    let bytes = parse("0x[DE AD_be\n\tef 00 FF]");
    assert_eq!(bytes.as_bytes(), Some(&[0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0xFF][..]));
    assert_eq!(write(&bytes, heap()).unwrap().as_str(), "0x[deadbeef00ff]");

    // Unicode escapes (both Rust `\u{...}` and JSON `\uXXXX` surrogate pairs), line continuation, and triple quotes.
    assert_eq!(parse(r#""\u{1F600} \uD83D\uDE00 \0""#).as_str(), Some("😀 😀 \0"));
    assert_eq!(parse("\"hello \\\n    world\"").as_str(), Some("hello world"));
    assert_eq!(parse("\"\"\"\nline 1\n\"quoted\" line 2\"\"\"").as_str(), Some("line 1\n\"quoted\" line 2"));
    assert_eq!(parse("\"\"\"ends with \"\"\"\"").as_str(), Some("ends with \""));
}

#[test]
fn homogeneous_table_pretty_and_compact() {
    let a = heap();
    let v = value!(in a; [
        {"kind": "error", "file": "src/main.rs", "line": 12, "col": 5, "message": "mismatched types"},
        {"kind": "warning", "file": "Cargo.toml", "line": 8, "col": 1, "message": "unused dep"},
    ]);

    let pretty_insertion = write_with(
        &v,
        WriteOptions { style: Style::Pretty { indent: 2 }, keys: KeyOrder::Insertion, ..WriteOptions::PRETTY },
        a,
    )
    .unwrap();

    assert_eq!(
        pretty_insertion.as_str(),
        "[[kind file line col message];\n  [error src/main.rs 12 5 \"mismatched types\"]\n  [warning Cargo.toml 8 1 \"unused dep\"]]"
    );

    let back = parse(pretty_insertion.as_str());
    assert_eq!(back, v);

    // Single-row list of maps also compresses to a table.
    let one = value!(in a; [{"a": 1, "b": "ok"}]);
    let one_text = round_trip(&one);
    assert_eq!(one_text.as_str(), "[[a b];[1 ok]]");

    // TableMode::Off writes standard map list instead.
    let off = write_with(&one, WriteOptions { tables: TableMode::Off, ..WriteOptions::COMPACT }, a).unwrap();
    assert_eq!(off.as_str(), "[{a:1 b:ok}]");
    assert_eq!(parse(off.as_str()), one);
}

#[test]
fn sparse_union_schema_table_distinguishes_absent_from_null_and_underscore_text() {
    let a = heap();
    let v = value!(in a; [
        {"id": 1, "msg": "ok"},
        {"id": 2, "msg": "warn", "note": null},
        {"id": 3, "msg": "fix", "note": "use &str"},
        {"id": 4, "msg": "literal", "note": "_"},
    ]);

    let compact = round_trip(&v);
    assert_eq!(
        compact.as_str(),
        "[[id msg note];[1 ok _] [2 warn null] [3 fix \"use &str\"] [4 literal \"_\"]]"
    );

    let parsed = parse(compact.as_str());
    let rows = parsed.as_list().unwrap();
    // Row 0: `note` MUST be absent (not present as null!)
    assert!(!rows[0].as_map().unwrap().contains_key("note"));
    // Row 1: `note` MUST be present with Value::Null
    assert_eq!(rows[1].get("note"), Some(&V::Null));
    // Row 2: `note` is "use &str"
    assert_eq!(rows[2].get("note").and_then(Value::as_str), Some("use &str"));
    // Row 3: `note` is literal string "_"
    assert_eq!(rows[3].get("note").and_then(Value::as_str), Some("_"));

    // TableMode::HomogeneousOnly does NOT compress sparse rows.
    let nuon_style =
        write_with(&v, WriteOptions { tables: TableMode::HomogeneousOnly, ..WriteOptions::COMPACT }, a).unwrap();
    assert!(nuon_style.as_str().starts_with("[{"));
    assert_eq!(parse(nuon_style.as_str()), v);

    // Completely disjoint schemas (0 shared keys) stay as maps even under TableMode::Auto.
    let disjoint = value!(in a; [{"a": 1}, {"b": 2}]);
    let disjoint_text = round_trip(&disjoint);
    assert_eq!(disjoint_text.as_str(), "[{a:1} {b:2}]");

    // Very sparse schema (< 50% coverage: 5 present out of 12 cells = 41.7%) stays as maps.
    let too_sparse = value!(in a; [
        {"k": 1, "a": 1},
        {"k": 2, "b": 2},
        {"k": 3, "c": 3, "d": 4},
    ]);
    let too_sparse_text = round_trip(&too_sparse);
    assert!(too_sparse_text.as_str().starts_with("[{"));

    // Empty maps list stays as maps `[{} {}]`.
    let empties = value!(in a; [{}, {}]);
    let empties_text = round_trip(&empties);
    assert_eq!(empties_text.as_str(), "[{} {}]");
}

#[test]
fn nested_tables_in_tables_and_nested_lists() {
    let a = heap();
    let v = value!(in a; [
        {
            "order": "ord-1",
            "items": [
                {"sku": "A1", "qty": 2},
                {"sku": "B2", "qty": 5, "gift": true},
            ],
        },
        {
            "order": "ord-2",
        },
    ]);

    let compact = round_trip(&v);
    assert!(compact.as_str().contains("[[gift qty sku];"));
    assert!(compact.as_str().contains("[_ ord-2]"));

    // Nested regular lists `[[a b] [c d]]` and `[[] []]` are NOT mistaken for tables.
    let nested_lists = value!(in a; [["a", "b"], ["c", "d"]]);
    let nl_text = round_trip(&nested_lists);
    assert_eq!(nl_text.as_str(), "[[a b] [c d]]");

    // A table wrapped directly inside an outer list `[[[x]; [1]]]`.
    let wrapped_table = parse("[[[x]; [1]]]");
    assert_eq!(wrapped_table, value!(in a; [[{"x": 1}]]));
}

#[test]
fn comments_and_optional_commas_everywhere() {
    let input = r#"
    # Top-level comment
    {
        # Map comment
        tags: [alpha, beta, gamma,] # trailing comma in list
        meta: { a: 1, b: 2, } # trailing comma in map
        rows: [
            [
                id,   # first col
                name, # second col
            ]; # header end
            [1, alice,], # first row
            [2, _,],     # second row with absent name
        ],
    } # EOF comment without newline"#;

    let v = parse(input);
    let a = heap();
    assert_eq!(
        v,
        value!(in a; {
            "tags": ["alpha", "beta", "gamma"],
            "meta": {"a": 1, "b": 2},
            "rows": [
                {"id": 1, "name": "alice"},
                {"id": 2},
            ],
        })
    );
}

struct Diagnostic<A: Allocator> {
    kind: String<A>,
    file: String<A>,
    line: u32,
    col: u32,
    message: String<A>,
    code: Option<String<A>>,
    suggestion: Option<String<A>>,
}
blaze_data::fields!(for<A> Diagnostic<A> { kind, file, line, col, message; code, suggestion });

#[test]
fn typed_api_round_trip_and_strict_errors() {
    let a = heap();
    let mut list = Vec::new_in(a);
    list.push(Diagnostic {
        kind: String::from_str_in("error", a),
        file: String::from_str_in("src/lib.rs", a),
        line: 42,
        col: 9,
        message: String::from_str_in("mismatched types", a),
        code: Some(String::from_str_in("E0308", a)),
        suggestion: None,
    });
    list.push(Diagnostic {
        kind: String::from_str_in("warning", a),
        file: String::from_str_in("src/read.rs", a),
        line: 108,
        col: 5,
        message: String::from_str_in("unused variable", a),
        code: None,
        suggestion: Some(String::from_str_in("prefix with `_`", a)),
    });

    let compact = write_typed(&list, a).unwrap();
    let pretty = write_typed_pretty(&list, a).unwrap();
    assert!(compact.as_str().starts_with("[["));
    assert!(pretty.as_str().contains('\n'));

    let back: Vec<Diagnostic<Alloc<'static>>, Alloc<'static>> =
        read_typed(compact.as_bytes(), Limits::SHIPPED, a).unwrap();
    assert_eq!(back.len(), 2);
    assert_eq!(back[0].code.as_ref().map(String::as_str), Some("E0308"));
    assert!(back[0].suggestion.is_none());
    assert!(back[1].code.is_none());
    assert_eq!(back[1].suggestion.as_ref().map(String::as_str), Some("prefix with `_`"));

    // Unknown field is strictly refused with ShapeError.
    let bad_field = read_typed::<Diagnostic<Alloc<'static>>>(
        b"{kind: error file: a.rs line: 1 col: 1 message: hi extra: 9}",
        Limits::SHIPPED,
        a,
    );
    assert!(matches!(bad_field, Err(FromTabError::Shape(_))));

    // Wrong kind is strictly refused with ShapeError.
    let bad_kind = read_typed::<Diagnostic<Alloc<'static>>>(
        b"{kind: error file: a.rs line: not_a_num col: 1 message: hi}",
        Limits::SHIPPED,
        a,
    );
    assert!(matches!(bad_kind, Err(FromTabError::Shape(_))));

    // Syntax error maps to FromTabError::Syntax.
    let bad_syntax = read_typed::<Diagnostic<Alloc<'static>>>(b"{kind:", Limits::SHIPPED, a);
    assert!(matches!(bad_syntax, Err(FromTabError::Syntax(_))));
}

#[test]
fn adversarial_errors_report_offset_line_col_and_path() {
    // Invalid UTF-8
    let err = refuse(b"\"ok\xff\"");
    assert_eq!(err.what, What::Utf8);
    assert_eq!((err.at, err.line, err.col), (3, 1, 4));

    // TooLarge
    let too_big = read(b"[1 2 3]", Limits { most_bytes: 3, ..Limits::SHIPPED }, heap()).unwrap_err();
    assert_eq!(too_big.what, What::TooLarge { len: 7, most: 3 });

    // Duplicate key in nested map reports exact path and line:col!
    let dup = refuse(b"{\n  outer: {\n    k: 1\n    k: 2\n  }\n}");
    assert_eq!(dup.what, What::DuplicateKey);
    assert_eq!(dup.path.as_str(), "outer.k");
    assert_eq!((dup.line, dup.col), (4, 5));
    let rendered = format_in!(heap(), "{dup}");
    assert!(rendered.as_str().contains("4:5") && rendered.as_str().contains("outer.k"));

    // Duplicates::LastWins accepts duplicate keys.
    let last_wins = read(
        b"{a: 1 a: 2}",
        Limits { duplicates: Duplicates::LastWins, ..Limits::SHIPPED },
        heap(),
    )
    .unwrap();
    assert_eq!(last_wins.get("a").and_then(Value::as_u64), Some(2));

    // Duplicate table column
    let dup_col = refuse(b"{tbl: [[a b a]; [1 2 3]]}");
    assert_eq!(dup_col.what, What::DuplicateColumn);
    assert_eq!(dup_col.path.as_str(), "tbl.a");

    // Empty table header
    let empty_hdr = refuse(b"[[]; []]");
    assert_eq!(empty_hdr.what, What::EmptyTableHeader);

    // Row arity mismatch: too few cells
    let too_few = refuse(b"{rows: [[a b c]; [1 2 3] [4 5]]}");
    assert_eq!(too_few.what, What::RowArity { row: 1, expected: 3, found: 2 });
    assert_eq!(too_few.path.as_str(), "rows[1].c");

    // Row arity mismatch: too many cells
    let too_many = refuse(b"[[a b]; [1 2 3]]");
    assert_eq!(too_many.what, What::RowArity { row: 0, expected: 2, found: 3 });

    // Absent sentinel outside table
    assert_eq!(refuse(b"_").what, What::AbsentOutsideTable);
    assert_eq!(refuse(b"[_]").what, What::AbsentOutsideTable);
    assert_eq!(refuse(b"{a: _}").what, What::AbsentOutsideTable);

    // Reserved keyword as bare key/column
    assert_eq!(refuse(b"{null: 1}").what, What::ReservedKey);
    assert_eq!(refuse(b"[[_]; [1]]").what, What::ReservedKey);

    // Unclosed containers and strings
    assert_eq!(refuse(b"[1 2").what, What::End { expected: Expected::SeparatorOrEndList });
    assert_eq!(refuse(b"{a: 1").what, What::End { expected: Expected::SeparatorOrEndMap });
    assert_eq!(refuse(b"[[a]; [1]").what, What::End { expected: Expected::SeparatorOrEndTable });
    assert_eq!(refuse(b"0x[ab").what, What::End { expected: Expected::EndBytes });
    assert_eq!(refuse(b"\"abc").what, What::End { expected: Expected::Quote });
    assert_eq!(refuse(b"\"\"\"abc").what, What::End { expected: Expected::TripleQuote });

    // Bad escapes and control characters
    assert_eq!(refuse(br#""\x""#).what, What::BadEscape { found: 'x' });
    assert_eq!(refuse(br#""\u{}""#).what, What::InvalidUnicodeEscape);
    assert_eq!(refuse(br#""\u{110000}""#).what, What::InvalidUnicodeEscape);
    assert_eq!(refuse(br#""\u{D800}""#).what, What::InvalidUnicodeEscape);
    assert_eq!(refuse(br#""\uD800""#).what, What::LoneSurrogate);
    assert_eq!(refuse(br#""\uDC00""#).what, What::LoneSurrogate);
    assert_eq!(refuse(b"\"a\nb\"").what, What::Control { byte: b'\n' });

    // Bad numbers
    assert_eq!(refuse(b"01").what, What::LeadingZero);
    assert_eq!(refuse(b"-01").what, What::LeadingZero);
    assert_eq!(refuse(b"1_").what, What::End { expected: Expected::Digit });
    assert_eq!(refuse(b"1__2").what, What::Unexpected { found: '_', expected: Expected::Digit });
    assert_eq!(refuse(b"18446744073709551616").what, What::IntegerOutOfRange);
    assert_eq!(refuse(b"-9223372036854775809").what, What::IntegerOutOfRange);
    assert_eq!(refuse(b"1e400").what, What::FloatOutOfRange);
    let mut long_num = String::new_in(heap());
    long_num.push('1');
    for _ in 0..100 {
        long_num.push('0');
    }
    assert_eq!(refuse(long_num.as_bytes()).what, What::NumberTooLong { most: 100 });

    // Bad hex bytes
    assert_eq!(refuse(b"0x[a]").what, What::OddHexBytes);
    assert_eq!(refuse(b"0x[zz]").what, What::Unexpected { found: 'z', expected: Expected::HexDigit });

    // Missing separators and bad commas
    assert_eq!(refuse(b"[123abc]").what, What::Unexpected { found: 'a', expected: Expected::SeparatorOrEndList });
    assert_eq!(refuse(b"[\"a\"\"b\"]").what, What::Unexpected { found: '"', expected: Expected::SeparatorOrEndList });
    assert_eq!(refuse(b"{a:1b:2}").what, What::Unexpected { found: 'b', expected: Expected::SeparatorOrEndMap });
    assert_eq!(refuse(b"[, 1]").what, What::Unexpected { found: ',', expected: Expected::ValueOrEndList });
    assert_eq!(refuse(b"[1,, 2]").what, What::Unexpected { found: ',', expected: Expected::Value });

    // Trailing tokens
    assert_eq!(refuse(b"1 2").what, What::Unexpected { found: '2', expected: Expected::End });
    assert_eq!(refuse(b"{a: 1} extra").what, What::Unexpected { found: 'e', expected: Expected::End });

    // line_col helper checks UTF-8 character columns
    assert_eq!(line_col("a\né😀x".as_bytes(), "a\né😀".len()), (2, 3));
}

#[test]
fn depth_limits_enforced_iteratively_without_stack_overflow() {
    let a = heap();
    // 129 nested lists exceeds shipped MOST_DEPTH (128).
    let mut deep_129 = String::new_in(a);
    for _ in 0..129 {
        deep_129.push('[');
    }
    for _ in 0..129 {
        deep_129.push(']');
    }
    let err = refuse(deep_129.as_bytes());
    assert_eq!(err.what, What::TooDeep { most: Limits::MOST_DEPTH });

    // Deep nesting (400 levels) succeeds cleanly with raised limit — proves zero recursion!
    let depth = 400usize;
    let mut deep = String::new_in(a);
    for _ in 0..depth {
        deep.push('[');
    }
    deep.push('1');
    for _ in 0..depth {
        deep.push(']');
    }
    let v = read(deep.as_bytes(), Limits { most_depth: depth, ..Limits::SHIPPED }, a).unwrap();

    // Writer enforces most_depth with a typed WriteError naming the path.
    let w_err: WriteError<Alloc<'static>> =
        write_with(&v, WriteOptions { most_depth: 5, ..WriteOptions::COMPACT }, a).unwrap_err();
    assert_eq!(w_err.what, WriteWhat::TooDeep { most: 5 });
    assert_eq!(w_err.path.as_str(), "[0][0][0][0][0]");

    // Writer succeeds at 400 levels without stack overflow.
    let written = write_with(&v, WriteOptions { most_depth: depth, ..WriteOptions::COMPACT }, a).unwrap();
    assert_eq!(written.as_str(), deep.as_str());
}

#[test]
fn benchmark_50_ci_diagnostics_at_least_40_percent_smaller_than_compact_json() {
    let a = heap();
    let packages = ["blaze-typeck", "blaze-solve", "blaze-lower", "blaze-emit", "blaze-cli"];
    let files = [
        "src/infer.rs",
        "src/canonical.rs",
        "src/body.rs",
        "src/aarch64.rs",
        "src/main.rs",
    ];
    let rules = ["type_mismatch", "borrow_conflict", "missing_trait", "unused_var", "non_exhaustive"];
    let messages = [
        "mismatched types",
        "double mutable borrow",
        "trait bound unsatisfied",
        "unused variable",
        "missing match arm",
    ];
    let codes = ["E0308", "E0499", "E0277", "E0004"];
    let suggestions = ["use try_from", "prefix with _", "add missing arm"];

    let mut rows = Vec::with_capacity_in(50, a);
    for i in 0usize..50 {
        let mut row = Map::new_in(a);
        let severity = if i % 4 == 0 { "warning" } else { "error" };
        row.insert(String::from_str_in("severity", a), V::Text(String::from_str_in(severity, a)));
        row.insert(String::from_str_in("package", a), V::Text(String::from_str_in(packages[i % packages.len()], a)));
        row.insert(String::from_str_in("file", a), V::Text(String::from_str_in(files[i % files.len()], a)));
        row.insert(String::from_str_in("line", a), V::Int(Int::from_u64(10 + (i as u64) * 7)));
        row.insert(String::from_str_in("col", a), V::Int(Int::from_u64(1 + ((i * 3) as u64) % 80)));
        row.insert(String::from_str_in("rule", a), V::Text(String::from_str_in(rules[i % rules.len()], a)));
        row.insert(String::from_str_in("message", a), V::Text(String::from_str_in(messages[i % messages.len()], a)));
        if i % 2 == 0 {
            row.insert(String::from_str_in("code", a), V::Text(String::from_str_in(codes[i % codes.len()], a)));
        }
        if i % 3 == 0 {
            row.insert(
                String::from_str_in("suggestion", a),
                V::Text(String::from_str_in(suggestions[i % suggestions.len()], a)),
            );
        }
        rows.push(V::Map(row));
    }

    let doc = V::List(rows);
    let json_text = blaze_json::to_string_in(&doc, blaze_json::WriteOptions::COMPACT, a).unwrap();
    let tab_text = write(&doc, a).unwrap();

    // Verify lossless round-trip on the full 50-item sparse-schema diagnostic table!
    let tab_back = read(tab_text.as_bytes(), Limits::SHIPPED, a).unwrap();
    assert_eq!(tab_back, doc);
    assert!(tab_text.as_str().starts_with("[[code col file line message package rule severity suggestion];"));

    let json_len = json_text.len();
    let tab_len = tab_text.len();
    let savings_pct = ((json_len - tab_len) * 100) / json_len;
    assert!(
        tab_len * 100 <= json_len * 60,
        "expected blaze-tab ({tab_len} B) to be >= 40% smaller than compact JSON ({json_len} B), actual savings: {savings_pct}%"
    );
    // Pin exact byte counts (4,381 B vs 7,949 B = 44.9% smaller) to catch accidental format bloat.
    assert_eq!((tab_len, json_len), (4381, 7949));
}

#[test]
fn edge_cases_empty_table_quoted_headers_and_smallest_limits() {
    let a = heap();
    // Empty and comment-only documents are refused.
    assert_eq!(refuse(b"").what, What::End { expected: Expected::Value });
    assert_eq!(refuse(b"# only a comment\n   ").what, What::End { expected: Expected::Value });

    // Zero-row table parses as an empty list.
    assert_eq!(parse("[[a b];]"), V::List(Vec::new_in(a)));

    // Quoted column names (spaces, colons, reserved words) round-trip in tables.
    let v = value!(in a; [
        {"col one": 1, "a:b": "x", "null": true},
        {"col one": 2, "a:b": "y", "null": false},
    ]);
    let compact = round_trip(&v);
    assert_eq!(compact.as_str(), "[[\"a:b\" \"col one\" \"null\"];[x 1 true] [y 2 false]]");

    // KeyOrder::Insertion preserves first-seen column order on sparse tables.
    let sparse = value!(in a; [
        {"z": 1, "a": 2},
        {"a": 3, "m": 4},
    ]);
    let ins = write_with(&sparse, WriteOptions { keys: KeyOrder::Insertion, ..WriteOptions::COMPACT }, a).unwrap();
    assert_eq!(ins.as_str(), "[[z a m];[1 2 _] [_ 3 4]]");
    assert_eq!(parse(ins.as_str()), sparse);

    // `-0` parses as Int(0); `-0.0` parses as Float(-0.0) with sign bit preserved.
    assert_eq!(parse("-0"), V::Int(Int::from_u64(0)));
    assert_eq!(parse("-0.0").as_f64().map(f64::to_bits), Some((-0.0f64).to_bits()));

    // Special floats inside tables round-trip losslessly.
    let floats = value!(in a; [
        {"k": "a", "v": f64::NAN},
        {"k": "b", "v": f64::INFINITY},
        {"k": "c", "v": f64::NEG_INFINITY},
        {"k": "d", "v": -0.0f64},
    ]);
    let floats_text = round_trip(&floats);
    assert_eq!(floats_text.as_str(), "[[k v];[a nan] [b inf] [c -inf] [d -0.0]]");

    // Limits::SMALLEST accepts a 1-byte scalar and refuses 2-byte inputs.
    assert_eq!(read(b"7", Limits::SMALLEST, a).unwrap().as_u64(), Some(7));
    assert_eq!(
        read(b"12", Limits::SMALLEST, a).unwrap_err().what,
        What::TooLarge { len: 2, most: 1 }
    );
}
