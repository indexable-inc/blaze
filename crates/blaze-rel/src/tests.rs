//! Tests of the engine: each operator with NULLs, the recursive provenance chain, the hash join
//! and aggregate against nested-loop and sort-based models (with a planted-bug control), and
//! every statement of the profile's views and named queries.

extern crate std;

use std::alloc::Global;
use std::string::String;
use std::vec::Vec as StdVec;
use std::{eprintln, format, vec};

use blaze_columns::{Batch, Column, Field, Name};
use blaze_own::Vec;
use blaze_sim::{Config, Tape, Verdict};

use crate::exec::plant::{self, Plant};
use crate::{Arg, Bounds, Catalog, ErrorKind, MemTable, Prelude, Session, run, run_with};

const VIEWS: &str = include_str!("../../blaze-profile/sql/views.sql");
const EXAMPLES: &str = include_str!("../../blaze-profile/sql/examples.sql");
/// The statements of views.sql and examples.sql not planned and run yet, one name a line with
/// the refusal after a tab. It may only shrink: a statement on it that runs fails the test.
const UNSUPPORTED: &str = include_str!("tests/unsupported.txt");

/// A table of one chunk of `columns`, every column nullable.
fn table(columns: StdVec<(&'static str, Column<Global>)>) -> MemTable<Global> {
    chunked(vec![columns])
}

/// A table of one chunk a list of columns.
fn chunked(chunks: StdVec<StdVec<(&'static str, Column<Global>)>>) -> MemTable<Global> {
    let mut fields = Vec::new_in(Global);
    let mut batches = Vec::new_in(Global);
    for chunk in chunks {
        let mut these = Vec::new_in(Global);
        let mut columns = Vec::new_in(Global);
        for (name, column) in chunk {
            these.push(Field { name: Name::Static(name), ty: column.ty(), nullable: true });
            columns.push(column);
        }
        if fields.is_empty() {
            fields = these.clone();
        }
        batches.push(Batch::try_new(these, columns).unwrap_or_else(|e| panic!("a test batch: {e:?}")));
    }
    MemTable::new(fields, batches).unwrap_or_else(|at| panic!("chunk {at} of a test table has other fields"))
}

fn ints(values: &[Option<i64>]) -> Column<Global> {
    Column::options(values.iter().copied(), Global)
}

fn texts(values: &[Option<&str>]) -> Column<Global> {
    Column::opt_texts(values, Global)
}

/// `sql`'s answer as JSON lines, or its refusal.
fn answer(catalog: &Catalog<'_, Global, Global>, sql: &str) -> Result<String, String> {
    match run(sql, catalog, Global) {
        Ok(rows) => {
            let mut out = String::new();
            rows.write_json(&mut out).map_err(|_| String::from("writing JSON"))?;
            Ok(out)
        }
        Err(error) => Err(format!("{error}")),
    }
}

#[track_caller]
fn expect(catalog: &Catalog<'_, Global, Global>, sql: &str, lines: &[&str]) {
    let got = answer(catalog, sql).unwrap_or_else(|e| panic!("{sql}\nrefused: {e}"));
    let mut want = String::new();
    for line in lines {
        want.push_str(line);
        want.push('\n');
    }
    assert_eq!(got, want, "{sql}");
}

/// `t(a, g)`: `(1, 'p'), (null, 'p'), (3, null)`; `u(k, w)`: `(1, 'one'), (null, 'nil'),
/// (5, 'five')`.
fn fixture() -> (MemTable<Global>, MemTable<Global>) {
    let t = table(vec![("a", ints(&[Some(1), None, Some(3)])), ("g", texts(&[Some("p"), Some("p"), None]))]);
    let u = table(vec![("k", ints(&[Some(1), None, Some(5)])), ("w", texts(&[Some("one"), Some("nil"), Some("five")]))]);
    (t, u)
}

#[test]
fn filter_keeps_only_true() {
    let (t, _) = fixture();
    let mut c = Catalog::new_in(Global);
    c.add("t", &t);
    expect(&c, "select a from t where a > 1", &[r#"{"a":3}"#]);
    expect(&c, "select a from t where not (a > 1)", &[r#"{"a":1}"#]);
    expect(&c, "select a from t where a is null", &[r#"{"a":null}"#]);
    expect(&c, "select a from t where a <> 1 or g = 'p'", &[r#"{"a":1}"#, r#"{"a":null}"#, r#"{"a":3}"#]);
}

#[test]
fn three_valued_logic() {
    let c = Catalog::new_in(Global);
    expect(
        &c,
        "select (null and false) as x, (null or true) as y, (null = null) as z, (2 in (1, null)) as w, \
         (1 in (1, null)) as v, (null is not distinct from null) as same",
        &[r#"{"x":false,"y":true,"z":null,"w":null,"v":true,"same":true}"#],
    );
}

#[test]
fn project_nulls_and_division() {
    let (t, _) = fixture();
    let mut c = Catalog::new_in(Global);
    c.add("t", &t);
    expect(
        &c,
        "select a + 1 as b, a / 2 as c, a / 0 as d, coalesce(a, 0) as e from t order by a",
        &[
            r#"{"b":2,"c":0.5,"d":null,"e":1}"#,
            r#"{"b":4,"c":1.5,"d":null,"e":3}"#,
            r#"{"b":null,"c":null,"d":null,"e":0}"#,
        ],
    );
}

#[test]
fn aggregates_skip_nulls() {
    let (t, u) = fixture();
    let mut c = Catalog::new_in(Global);
    c.add("t", &t);
    c.add("u", &u);
    expect(
        &c,
        "select count(*) as n, count(a) as m, sum(a) as s, min(a) as lo, max(a) as hi, avg(a) as mean from t",
        &[r#"{"n":3,"m":2,"s":4,"lo":1,"hi":3,"mean":2.0}"#],
    );
    expect(
        &c,
        "select count(*) as n, count(a) as m, sum(a) as s, max(a) as hi from t where a > 10",
        &[r#"{"n":0,"m":0,"s":null,"hi":null}"#],
    );
    expect(
        &c,
        "select g, count(*) as n, sum(a) as s from t group by g order by g",
        &[r#"{"g":"p","n":2,"s":1}"#, r#"{"g":null,"n":1,"s":3}"#],
    );
    // A key named by its alias is that select item, a scalar subquery in it and all.
    expect(
        &c,
        "select coalesce(a, (select max(k) from u)) as m, count(*) as n from t group by m order by m",
        &[r#"{"m":1,"n":1}"#, r#"{"m":3,"n":1}"#, r#"{"m":5,"n":1}"#],
    );
    // A relation's repeated name is its first column, as DuckDB renames the later `job_1`.
    expect(&c, "select job from (select a as job, g as job from t) order by job", &[
        r#"{"job":1}"#,
        r#"{"job":3}"#,
        r#"{"job":null}"#,
    ]);
    expect(&c, "select g, count(*) as n from t group by all having count(a) > 0 order by n desc", &[
        r#"{"g":"p","n":2}"#,
        r#"{"g":null,"n":1}"#,
    ]);
}

#[test]
fn joins_never_match_null_keys() {
    let (t, u) = fixture();
    let mut c = Catalog::new_in(Global);
    c.add("t", &t);
    c.add("u", &u);
    expect(&c, "select a, w from t join u on a = k", &[r#"{"a":1,"w":"one"}"#]);
    expect(&c, "select a, w from t, u where a = k", &[r#"{"a":1,"w":"one"}"#]);
    expect(&c, "select a, w from t left join u on a = k order by a", &[
        r#"{"a":1,"w":"one"}"#,
        r#"{"a":3,"w":null}"#,
        r#"{"a":null,"w":null}"#,
    ]);
    expect(&c, "select a, w from t join u on a is not distinct from k order by a", &[
        r#"{"a":1,"w":"one"}"#,
        r#"{"a":null,"w":"nil"}"#,
    ]);
    expect(&c, "select a, k from t full join u on a = k order by a, k", &[
        r#"{"a":1,"k":1}"#,
        r#"{"a":3,"k":null}"#,
        r#"{"a":null,"k":5}"#,
        r#"{"a":null,"k":null}"#,
        r#"{"a":null,"k":null}"#,
    ]);
    expect(&c, "select a, w from t right join u on a = k order by w", &[
        r#"{"a":null,"w":"five"}"#,
        r#"{"a":null,"w":"nil"}"#,
        r#"{"a":1,"w":"one"}"#,
    ]);
    expect(&c, "select a from t where exists (select 1 from u where k = a)", &[r#"{"a":1}"#]);
    expect(&c, "select a from t where a in (select k from u)", &[r#"{"a":1}"#]);
}

#[test]
fn sort_puts_nulls_last_and_limits() {
    let (t, _) = fixture();
    let mut c = Catalog::new_in(Global);
    c.add("t", &t);
    expect(&c, "select a from t order by a desc", &[r#"{"a":3}"#, r#"{"a":1}"#, r#"{"a":null}"#]);
    expect(&c, "select a from t order by a nulls first", &[r#"{"a":null}"#, r#"{"a":1}"#, r#"{"a":3}"#]);
    expect(&c, "select a from t order by a limit 1 offset 1", &[r#"{"a":3}"#]);
}

#[test]
fn set_operations_treat_nulls_as_equal() {
    let (t, u) = fixture();
    let mut c = Catalog::new_in(Global);
    c.add("t", &t);
    c.add("u", &u);
    expect(&c, "select * from (select g from t union select g from t) order by g", &[
        r#"{"g":"p"}"#,
        r#"{"g":null}"#,
    ]);
    expect(&c, "select a from t except select k from u", &[r#"{"a":3}"#]);
    expect(&c, "select * from (select a from t intersect select k from u) order by a", &[
        r#"{"a":1}"#,
        r#"{"a":null}"#,
    ]);
    expect(&c, "select count(*) as n from (select a from t union all select k from u)", &[r#"{"n":6}"#]);
}

#[test]
fn windows_number_and_shift() {
    let (t, _) = fixture();
    let mut c = Catalog::new_in(Global);
    c.add("t", &t);
    expect(
        &c,
        "select a, row_number() over (order by a) as r, lag(a) over (order by a) as prev, \
         sum(a) over (partition by g) as total from t order by a",
        &[
            r#"{"a":1,"r":1,"prev":null,"total":1}"#,
            r#"{"a":3,"r":2,"prev":1,"total":3}"#,
            r#"{"a":null,"r":3,"prev":3,"total":1}"#,
        ],
    );
    expect(&c, "select a from t qualify row_number() over (order by a desc) = 1", &[r#"{"a":3}"#]);
}

#[test]
fn scans_skip_chunks_and_keep_rows() {
    let t = chunked(vec![vec![("a", ints(&[Some(1), Some(2)]))], vec![("a", ints(&[Some(6), None, Some(7)]))]]);
    let mut c = Catalog::new_in(Global);
    c.add("t", &t);
    expect(&c, "select a from t where a > 5", &[r#"{"a":6}"#, r#"{"a":7}"#]);
    expect(&c, "select a from t where 2 >= a", &[r#"{"a":1}"#, r#"{"a":2}"#]);
}

/// The provenance chain: what each record was made from, followed to its roots.
fn edges(cycle: bool) -> MemTable<Global> {
    let mut child = vec![Some("a"), Some("b"), Some("c")];
    let mut parent = vec![Some("b"), Some("c"), Some("d")];
    if cycle {
        child.push(Some("d"));
        parent.push(Some("a"));
    }
    table(vec![("child", texts(&child)), ("parent", texts(&parent))])
}

#[test]
fn recursive_provenance_chain() {
    let e = edges(false);
    let mut c = Catalog::new_in(Global);
    c.add("edges", &e);
    expect(
        &c,
        "with recursive up(node, depth) as (select 'a', 0 union all \
         select e.parent, up.depth + 1 from up join edges e on e.child = up.node) \
         select node, depth from up order by depth",
        &[
            r#"{"node":"a","depth":0}"#,
            r#"{"node":"b","depth":1}"#,
            r#"{"node":"c","depth":2}"#,
            r#"{"node":"d","depth":3}"#,
        ],
    );
}

#[test]
fn recursion_over_a_cycle_settles_or_is_bounded() {
    let e = edges(true);
    let mut c = Catalog::new_in(Global);
    c.add("edges", &e);
    // `union` keeps distinct rows: the cycle adds nothing new and the fixpoint settles.
    expect(
        &c,
        "with recursive r(node) as (select 'a' union select e.parent from r join edges e on e.child = r.node) \
         select count(*) as n from r",
        &[r#"{"n":4}"#],
    );
    // `union all` grows forever: refused at the bound, naming the CTE.
    let prelude = Prelude::empty(Global);
    let session = Session { prelude: &prelude, params: &[], bounds: Bounds { rounds: 50, ..Bounds::PROFILE } };
    let sql = "with recursive r(node, n) as (select 'a', 0 union all \
               select e.parent, r.n + 1 from r join edges e on e.child = r.node) select count(*) from r";
    match run_with(sql, &session, &c, Global) {
        Err(error) => assert!(matches!(error.kind, ErrorKind::Recursion { cte: "r", .. }), "{error}"),
        Ok(_) => panic!("an unbounded recursion ran to an answer"),
    }
}

// ---- properties: the hash operators against plain models ----

/// A key column of `rows` rows: null one time in four, else one of three values.
fn keys(tape: &Tape, rows: usize, label: &'static str) -> StdVec<Option<i64>> {
    (0..rows)
        .map(|_| match tape.below(4, label) {
            0 => None,
            k => Some(i64::try_from(k).unwrap_or(0)),
        })
        .collect()
}

fn lines(text: &str) -> StdVec<String> {
    let mut out: StdVec<String> = text.lines().map(String::from).collect();
    out.sort();
    out
}

fn json_int(v: Option<i64>) -> String {
    v.map_or_else(|| String::from("null"), |it| format!("{it}"))
}

/// The hash join and the hash aggregate answer as a nested loop and a sort do.
fn hash_operators_match_models(tape: &Tape) -> Result<(), String> {
    let left_rows = usize::try_from(tape.below(6, "left rows")).unwrap_or(0);
    let right_rows = usize::try_from(tape.below(6, "right rows")).unwrap_or(0);
    let lk = keys(tape, left_rows, "left key");
    let rk = keys(tape, right_rows, "right key");
    let lv: StdVec<Option<i64>> = (0..left_rows).map(|i| Some(i64::try_from(i).unwrap_or(0))).collect();
    let rw: StdVec<Option<i64>> = (0..right_rows).map(|i| Some(i64::try_from(i + 100).unwrap_or(0))).collect();
    let l = table(vec![("k", ints(&lk)), ("v", ints(&lv))]);
    let r = table(vec![("k", ints(&rk)), ("w", ints(&rw))]);
    let mut c = Catalog::new_in(Global);
    c.add("l", &l);
    c.add("r", &r);

    // Inner and left join against the nested loop.
    let mut inner = StdVec::new();
    let mut left = StdVec::new();
    for (i, key) in lk.iter().enumerate() {
        let mut matched = false;
        for (j, other) in rk.iter().enumerate() {
            if key.is_some() && key == other {
                matched = true;
                inner.push(format!(r#"{{"v":{},"w":{}}}"#, json_int(lv[i]), json_int(rw[j])));
            }
        }
        if !matched {
            left.push(format!(r#"{{"v":{},"w":null}}"#, json_int(lv[i])));
        }
    }
    left.extend(inner.iter().cloned());
    inner.sort();
    left.sort();
    let got = answer(&c, "select l.v as v, r.w as w from l join r on l.k = r.k")?;
    if lines(&got) != inner {
        return Err(format!("inner join: {:?}, the nested loop: {inner:?}", lines(&got)));
    }
    let got = answer(&c, "select l.v as v, r.w as w from l left join r on l.k = r.k")?;
    if lines(&got) != left {
        return Err(format!("left join: {:?}, the nested loop: {left:?}", lines(&got)));
    }

    // The aggregate against a sort: equal keys (null among them) are one group.
    let mut sorted: StdVec<(Option<i64>, Option<i64>)> = lk.iter().copied().zip(lv.iter().copied()).collect();
    sorted.sort();
    let mut groups = StdVec::new();
    let mut at = 0;
    while at < sorted.len() {
        let key = sorted[at].0;
        let mut end = at;
        let mut sum = 0;
        while end < sorted.len() && sorted[end].0 == key {
            sum += sorted[end].1.unwrap_or(0);
            end += 1;
        }
        groups.push(format!(r#"{{"k":{},"n":{},"s":{sum}}}"#, json_int(key), end - at));
        at = end;
    }
    groups.sort();
    let got = answer(&c, "select k, count(*) as n, sum(v) as s from l group by k")?;
    if lines(&got) != groups {
        return Err(format!("aggregate: {:?}, the sort: {groups:?}", lines(&got)));
    }
    Ok(())
}

#[test]
fn hash_join_and_aggregate_match_models() {
    blaze_sim::check(Config::cases(300), hash_operators_match_models);
}

/// The control: with join keys made to match nulls, the property fails.
#[test]
fn planted_null_keys_match_is_caught() {
    let verdict = plant::with(Plant::NullKeysMatch, || blaze_sim::search(Config::cases(300), hash_operators_match_models));
    assert!(matches!(verdict, Verdict::Failed(_)), "the planted bug went unseen: {verdict:?}");
}

// ---- acceptance: the profile's workload ----

/// The named queries of examples.sql: each from its `-- name:` line to the next.
fn examples() -> StdVec<(&'static str, &'static str)> {
    let mut out = StdVec::new();
    let mut rest = EXAMPLES;
    while let Some(at) = rest.find("-- name: ") {
        let after = &rest[at + "-- name: ".len()..];
        let name_end = after.find('\n').unwrap_or(after.len());
        let name = after[..name_end].trim();
        let body = &after[name_end..];
        let end = body.find("-- name: ").unwrap_or(body.len());
        out.push((name, &body[..end]));
        rest = &body[end..];
    }
    out
}

#[test]
fn every_view_and_named_query_plans_and_runs() {
    let mut tables = StdVec::new();
    for table in blaze_profile::schema::TABLES.iter().chain(&blaze_profile::schema::RESULTS) {
        let fields = Vec::from_iter_in(blaze_profile::columns::fields(table).iter().cloned(), Global);
        let empty = MemTable::new(fields, Vec::new_in(Global)).unwrap_or_else(|_| unreachable!("no chunks"));
        tables.push((table.name, empty));
    }
    let mut catalog = Catalog::new_in(Global);
    for (name, table) in &tables {
        catalog.add(name, table);
    }
    let prelude = Prelude::parse(VIEWS, Global).unwrap_or_else(|e| panic!("views.sql: {e}"));
    let params = [
        ("build", Arg::Int(1)),
        ("builds", Arg::Int(5)),
        ("limit", Arg::Int(20)),
        ("min_ms", Arg::Float(1.0)),
        ("thread", Arg::Text("%")),
        ("within", Arg::Text("%")),
        ("earlier", Arg::Text("a")),
        ("later", Arg::Text("b")),
        ("before", Arg::Int(1)),
        ("after", Arg::Int(2)),
    ];
    let session = Session { prelude: &prelude, params: &params, bounds: Bounds::PROFILE };
    let mut statements: StdVec<(String, String)> = StdVec::new();
    for name in prelude.names(Global).iter() {
        statements.push((format!("view {name}"), format!("select * from {name}")));
    }
    for (name, text) in examples() {
        statements.push((format!("query {name}"), String::from(text)));
    }
    assert!(statements.len() >= 95, "{} statements in the workload", statements.len());
    let expected: StdVec<&str> =
        UNSUPPORTED.lines().filter(|it| !it.is_empty()).map(|it| it.split('\t').next().unwrap_or(it)).collect();
    let mut refused = StdVec::new();
    let mut ran = 0;
    for (name, text) in &statements {
        match run_with(text, &session, &catalog, Global) {
            Ok(_) => ran += 1,
            Err(error) => {
                let why = format!("{error}").replace('\n', " ");
                refused.push(format!("{name}\t{why}"));
            }
        }
    }
    eprintln!("{ran} of {} statements plan and run", statements.len());
    let mut unexpected = StdVec::new();
    for line in &refused {
        let name = line.split('\t').next().unwrap_or(line);
        if !expected.contains(&name) {
            unexpected.push(line.as_str());
        }
    }
    let now_run: StdVec<&&str> = expected.iter().filter(|name| !refused.iter().any(|it| it.split('\t').next() == Some(**name))).collect();
    assert!(
        unexpected.is_empty() && now_run.is_empty(),
        "refused but not listed in src/tests/unsupported.txt:\n{}\nlisted but now run (take them off the list):\n{:?}",
        unexpected.join("\n"),
        now_run,
    );
}
