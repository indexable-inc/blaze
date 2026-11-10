//! What the page asks of the `blaze` layer and its kinds of error (explore-wire.md, "Ops" and
//! "Errors"). A request is one UTF-8 JSON object on its own bidirectional stream, `{"op": ...,
//! ...args}`; the session (`ix_prof::explore::session`) answers `hello` and hands every other
//! request here whole. A field the op does not take is an error, never ignored.

use ix_prof::explore::{Kind as Core, OpError};
use serde::Deserialize as _;
use serde_json::{Map, Value};

use crate::tiles::Lane;

/// Every op of the `blaze` layer, as the page names it.
pub const OPS: [&str; 6] = ["builds", "sql", "views", "view", "spans", "tiles"];

/// A request, its arguments typed.
#[derive(Debug, PartialEq, serde_derive::Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    /// The `builds` table.
    Builds {},
    /// Any query over the history, as DuckDB runs it.
    Sql { sql: String },
    /// The named views: name, title, SQL and the JSON schema of their arguments.
    Views {},
    /// A named view run with `args`; the header's `sql` is what ran.
    View {
        name: String,
        #[serde(default)]
        args: Map<String, Value>,
    },
    /// The `spans` rows of build `build` overlapping `[t0_ns, t1_ns)`.
    Spans { build: u64, t0_ns: i64, t1_ns: i64 },
    /// The `tiles` of build `build` over `[t0_ns, t1_ns)`, `bucket_ns` a bucket: a lane per
    /// thread, or with `"lane": "job"` the `job_tiles`, a lane per fiber job.
    Tiles {
        build: u64,
        t0_ns: i64,
        t1_ns: i64,
        bucket_ns: i64,
        #[serde(default)]
        lane: Lane,
    },
}

/// What went wrong in the `blaze` layer, as the page tells it apart; the core's kinds
/// (`bad_args`, `encode`, `cancelled`, ...) are `ix_prof::explore::Kind`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// DuckDB refused the query or could not run; `detail` has its message and the query.
    Sql,
    NoSuchBuild,
    NoSuchView,
    /// The daemon could not flush the history before the query (`ops::Flush`), so the query
    /// was not run: it would have missed the rows recorded since the last flush.
    Flush,
}

impl Kind {
    pub const fn name(self) -> &'static str {
        match self {
            Kind::Sql => "sql",
            Kind::NoSuchBuild => "no_such_build",
            Kind::NoSuchView => "no_such_view",
            Kind::Flush => "flush",
        }
    }
}

impl From<Kind> for &'static str {
    fn from(kind: Kind) -> &'static str {
        kind.name()
    }
}

/// The request `args` of op `op`: `None` when the layer has no such op (the session answers
/// `unknown_op`); anything wrong with its arguments, `bad_args`, naming the op and what is
/// wrong.
pub fn parse(op: &str, args: &Value) -> Option<Result<Request, OpError>> {
    if !OPS.contains(&op) {
        return None;
    }
    Some(
        Request::deserialize(args)
            .map_err(|why| OpError::new(Core::BadArgs, format!("`{op}`: {why}")).with("op", op)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(request: &str) -> Option<Result<Request, OpError>> {
        let value: Value = serde_json::from_str(request).unwrap();
        let op = value["op"].as_str().unwrap().to_owned();
        super::parse(&op, &value)
    }

    #[test]
    fn each_op_parses_with_its_arguments() {
        assert_eq!(parse(r#"{"op":"builds"}"#), Some(Ok(Request::Builds {})));
        assert_eq!(parse(r#"{"op":"views"}"#), Some(Ok(Request::Views {})));
        assert_eq!(
            parse(r#"{"op":"sql","sql":"select 1"}"#),
            Some(Ok(Request::Sql { sql: "select 1".to_owned() }))
        );
        let Some(Ok(Request::View { name, args })) =
            parse(r#"{"op":"view","name":"slowest","args":{"n":5}}"#)
        else {
            panic!("a view request parses");
        };
        assert_eq!((name.as_str(), args.get("n")), ("slowest", Some(&Value::from(5))));
        assert_eq!(
            parse(r#"{"op":"tiles","build":3,"t0_ns":-5,"t1_ns":100,"bucket_ns":10}"#),
            Some(Ok(Request::Tiles {
                build: 3,
                t0_ns: -5,
                t1_ns: 100,
                bucket_ns: 10,
                lane: Lane::Thread
            }))
        );
        assert_eq!(
            parse(r#"{"op":"tiles","build":3,"t0_ns":0,"t1_ns":100,"bucket_ns":10,"lane":"job"}"#),
            Some(Ok(Request::Tiles {
                build: 3,
                t0_ns: 0,
                t1_ns: 100,
                bucket_ns: 10,
                lane: Lane::Job
            }))
        );
    }

    #[test]
    fn an_op_not_the_layers_is_none_and_bad_arguments_are_bad_args() {
        for unknown in [r#"{"op":"live"}"#, r#"{"op":"hello","token":"ab"}"#] {
            assert_eq!(parse(unknown), None, "{unknown}: the session's to answer");
        }
        for bad in [
            r#"{"op":"spans","build":1,"t0_ns":0}"#,
            r#"{"op":"builds","extra":1}"#,
            r#"{"op":"spans","build":-1,"t0_ns":0,"t1_ns":1}"#,
        ] {
            let error = parse(bad).unwrap().unwrap_err();
            assert_eq!(error.kind, "bad_args", "{bad}: {error}");
            assert!(error.detail.contains_key("op"), "{bad}: {error}");
        }
    }
}
