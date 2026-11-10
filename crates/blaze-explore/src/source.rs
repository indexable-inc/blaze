//! The daemon as an explorer source (`ix_prof::explore::Source`): its `hello` (the daemon's
//! version, blaze-profile's schema and clock, the `blaze` layer), a live feed per session over
//! blaze-profile's flight recorder ([`live::Feed`]), and the `blaze` layer's ops over the
//! history ([`ops`]). The server is ix-prof's; [`start`] starts it on this
//! source.

use std::sync::Arc;

use blaze_profile::{clock, live, schema};
use ix_prof::explore::{
    self, Answer, CoreWasm, Explorer, Hello, Kind as Core, Layer, Live, OpError, Rule, Source,
    StartError, Stop, Tick,
};
use serde_json::Value;

use crate::{
    ops::{self, Context, History},
    tasks::{self, Tasks},
    wire,
};

/// The daemon's layer: builds, SQL over the history and the named views,
/// versioned by blaze-profile's schema.
pub const LAYERS: &[Layer] = &[Layer { name: "blaze", version: schema::VERSION }];

/// The command that prints the daemon's explorer link.
pub const NAME: &str = "blaze explore";

/// What the daemon's explorer serves.
pub struct Config {
    pub history: History,
    /// Accept sessions from `ix_prof::explore::link::DEV_ORIGIN` too.
    pub dev_origin: bool,
    /// The daemon's version, as `hello` says it.
    pub daemon: &'static str,
    /// The daemon's tasks, as the plane's loop keeps them ([`Tasks`]).
    pub tasks: Arc<Tasks>,
    /// The explorer core the daemon serves the page, or why it has none ([`core_file`]).
    pub core: Result<CoreWasm, String>,
}

/// The name of the explorer core's file, beside the daemon's executable as the release
/// archive lays it out.
pub const CORE_FILE: &str = "explore-core.wasm";

/// The command that builds the explorer core, as an error saying it has none names it.
pub const CORE_BUILD: &str =
    "blaze build --release -p blaze-explore-core --target wasm32-unknown-unknown";

/// The explorer core beside `exe` ([`CORE_FILE`]), or why there is none: the error names the
/// path it looked at and the command that builds one.
pub fn core_file(exe: &std::path::Path) -> Result<CoreWasm, String> {
    let Some(dir) = exe.parent() else {
        return Err(format!("the daemon's executable {} has no directory to hold {CORE_FILE}", exe.display()));
    };
    let path = dir.join(CORE_FILE);
    let shown = path.display().to_string();
    match std::fs::read(&path) {
        Ok(bytes) => CoreWasm::new(bytes, &shown),
        Err(why) if why.kind() == std::io::ErrorKind::NotFound => Err(format!(
            "no explorer core at {shown}: a release archive has it beside the binary; in a checkout, `{CORE_BUILD}` makes it, then copy target/wasm32-unknown-unknown/release/blaze_explore_core.wasm to {shown}"
        )),
        Err(why) => Err(format!("the explorer core {shown} could not be read: {why}")),
    }
}

/// Starts the daemon's explorer; it serves until the [`Explorer`] is dropped.
pub fn start(config: Config) -> Result<Explorer, StartError> {
    let Config { history, dev_origin, daemon, tasks, core } = config;
    let source = Daemon { cx: Context { history }, daemon, tasks };
    explore::serve(explore::Config {
        source: Arc::new(source),
        dev_origin,
        name: NAME,
        every: live::EVERY,
        core,
    })
}

/// The daemon's source.
pub(crate) struct Daemon {
    pub(crate) cx: Context,
    pub(crate) daemon: &'static str,
    pub(crate) tasks: Arc<Tasks>,
}

impl Source for Daemon {
    /// The daemon, whose builds the generic views read through `spans` and `sql`, and which
    /// alone has the `blaze` extension (builds, SQL, views): a page that reads
    /// `source` shows the extension's views only when it is listed
    /// (docs/v2/plan/design/explore.md, "One explorer, many sources").
    fn hello(&self) -> Hello {
        Hello {
            // alloc: once a session
            daemon: self.daemon.to_owned(),
            schema: schema::VERSION,
            clock: schema::CLOCK,
            now_ns: clock::mono_ns(),
            kind: "blaze",
            extensions: &["blaze"],
            layers: LAYERS,
        }
    }

    fn live(&self) -> Box<dyn Live> {
        // alloc: once a session, its feed
        Box::new(Feed {
            feed: live::Feed::new(),
            frame: live::Frame::default(),
            // clone: an `Arc` of the tasks, once a session
            tasks: tasks::Feed::new(Arc::clone(&self.tasks)),
        })
    }

    fn op(&self, name: &str, args: &Value, stop: &Stop) -> Option<Result<Answer, OpError>> {
        Some(wire::parse(name, args)?.and_then(|request| ops::answer(&self.cx, request, stop)))
    }
}

/// A session's place in the flight recorder's rings and its stream of each table, and its
/// feed of the daemon's tasks.
struct Feed {
    feed: live::Feed,
    /// The feed's last frame: each tick its buffers trade places with the tick's, so neither
    /// is copied nor made anew.
    frame: live::Frame,
    tasks: tasks::Feed,
}

impl Live for Feed {
    fn tick(&mut self, _at_ns: i64, into: &mut Tick) -> Result<(), OpError> {
        self.feed
            .next(&mut self.frame)
            .map_err(|why| OpError::new(Core::Encode, format!("the live frame: {why}")))?;
        let live::Frame {
            at_ns,
            skipped,
            lost,
            spans,
            counters,
            open,
            spans_rows,
            counters_rows,
            open_rows,
        } = &mut self.frame;
        (into.at_ns, into.skipped, into.lost) = (*at_ns, *skipped, *lost);
        let tables = [
            ("spans", Rule::Append, *spans_rows, spans),
            ("counters", Rule::Append, *counters_rows, counters),
            ("open", Rule::Replace, *open_rows, open),
        ];
        for (name, rule, rows, ipc) in tables {
            let table = into.table(name, rule);
            table.rows = rows;
            std::mem::swap(&mut table.ipc, ipc);
        }
        let downloads = blaze_fetch::downloads::Downloads::process().shown();
        self.tasks.tick(*at_ns, &downloads, into)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::ops::Flush;

    fn daemon() -> Daemon {
        let history =
            History { duckdb: None, dir: PathBuf::from("/nowhere"), flush: Flush::new(|| Ok(0)) };
        Daemon { cx: Context { history }, daemon: "test", tasks: Arc::new(Tasks::new()) }
    }

    fn op(source: &Daemon, request: &str) -> Option<Result<Answer, OpError>> {
        let args: Value = serde_json::from_str(request).unwrap();
        let name = args["op"].as_str().unwrap().to_owned();
        source.op(&name, &args, &crossbeam_channel::never())
    }

    #[test]
    fn a_daemon_with_no_core_beside_it_says_where_it_looked_and_how_to_build_one() {
        let why = core_file(std::path::Path::new("/nowhere/blaze-engine")).unwrap_err();
        assert!(why.contains("/nowhere/explore-core.wasm") && why.contains(CORE_BUILD), "{why}");
    }

    #[test]
    fn hello_names_the_daemon_its_schema_and_the_blaze_layer() {
        let hello = daemon().hello();
        assert_eq!(hello.daemon, "test");
        assert_eq!((hello.schema, hello.clock), (schema::VERSION, schema::CLOCK));
        assert_eq!((hello.kind, hello.extensions), ("blaze", &["blaze"][..]));
        assert_eq!(hello.layers, &[Layer { name: "blaze", version: schema::VERSION }][..]);
    }

    #[test]
    fn the_blaze_ops_are_answered_and_others_are_not_the_daemons() {
        let source = daemon();
        let views = op(&source, r#"{"op":"views"}"#).unwrap().unwrap();
        assert_eq!(views.header["op"], "views");
        assert_eq!(views.table.unwrap().rows(), blaze_profile::views::VIEWS.len());
        let bad = op(&source, r#"{"op":"builds","extra":1}"#).unwrap().unwrap_err();
        assert_eq!(bad.kind, "bad_args");
        assert!(op(&source, r#"{"op":"frobnicate"}"#).is_none(), "the session names it");
    }

    #[test]
    fn a_live_tick_carries_what_the_flight_recorder_holds() {
        blaze_profile::plot(blaze_profile::Plot::Queued, 7);
        let mut feed = daemon().live();
        let mut tick = Tick::default();
        tick.clear(0);
        feed.tick(0, &mut tick).unwrap();
        let names: Vec<_> = tick.tables.iter().map(|it| (it.name, it.rule)).collect();
        assert_eq!(
            names,
            [
                ("spans", Rule::Append),
                ("counters", Rule::Append),
                ("open", Rule::Replace),
                ("tasks", Rule::Upsert),
                ("task_edges", Rule::Append),
            ]
        );
        let rows: usize = tick.tables.iter().map(|it| it.rows).sum();
        assert!(rows > 0, "the plot is in the rings: {tick:?}");
        for table in &tick.tables {
            let name = table.name;
            assert_eq!(table.rows == 0, table.ipc.is_empty(), "{name}: bytes exactly when rows");
        }
        let mut again = Tick::default();
        again.clear(1);
        let mut second = daemon().live();
        second.tick(1, &mut again).unwrap();
        let rows_again: usize = again.tables.iter().map(|it| it.rows).sum();
        assert!(rows_again > 0, "a second session's feed reads from its own place");
    }
}
