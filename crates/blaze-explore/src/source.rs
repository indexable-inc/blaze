//! The daemon as an explorer source (`ix_prof::explore::Source`): its `hello` (the daemon's
//! version, blaze-profile's schema and clock, the `blaze` layer), a live feed per session over
//! blaze-profile's flight recorder ([`live::Feed`]), and the `blaze` layer's ops over the
//! history and the build world ([`ops`]). The server is ix-prof's; [`start`] starts it on this
//! source.

use std::sync::Arc;

use blaze_profile::{clock, live, schema};
use ix_prof::explore::{
    self, Answer, Explorer, Hello, Kind as Core, Layer, Live, OpError, Rule, Source, StartError,
    Stop, Tick,
};
use serde_json::Value;

use crate::{
    ops::{self, Context, History},
    wire,
};

/// The daemon's layer: builds, SQL over the history, the named views and the build world,
/// versioned by blaze-profile's schema.
pub const LAYERS: &[Layer] = &[Layer { name: "blaze", version: schema::VERSION }];

/// The command that prints the daemon's explorer link.
pub const NAME: &str = "blaze explore";

/// What the daemon's explorer serves.
pub struct Config {
    pub history: History,
    /// Flecs's world of the builds, for `world`; `None` answers `no_world`.
    pub world: Option<Arc<blaze_web::Graphs>>,
    /// Accept sessions from `ix_prof::explore::link::DEV_ORIGIN` too.
    pub dev_origin: bool,
    /// The daemon's version, as `hello` says it.
    pub daemon: &'static str,
}

/// Starts the daemon's explorer; it serves until the [`Explorer`] is dropped.
pub fn start(config: Config) -> Result<Explorer, StartError> {
    let Config { history, world, dev_origin, daemon } = config;
    let source = Daemon { cx: Context { history, world }, daemon };
    explore::serve(explore::Config {
        source: Arc::new(source),
        dev_origin,
        name: NAME,
        every: live::EVERY,
    })
}

/// The daemon's source.
pub(crate) struct Daemon {
    pub(crate) cx: Context,
    pub(crate) daemon: &'static str,
}

impl Source for Daemon {
    /// The daemon, whose builds the generic views read through `spans` and `sql`, and which
    /// alone has the `blaze` extension (builds, SQL, views, the build world): a page that reads
    /// `source` shows the extension's views only when it is listed
    /// (docs/v2/plan/lanes/observe/explore.md, "One explorer, many sources").
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
        Box::new(Feed { feed: live::Feed::new(), frame: live::Frame::default() })
    }

    fn op(&self, name: &str, args: &Value, stop: &Stop) -> Option<Result<Answer, OpError>> {
        Some(wire::parse(name, args)?.and_then(|request| ops::answer(&self.cx, request, stop)))
    }
}

/// A session's place in the flight recorder's rings and its stream of each table.
struct Feed {
    feed: live::Feed,
    /// The feed's last frame: each tick its buffers trade places with the tick's, so neither
    /// is copied nor made anew.
    frame: live::Frame,
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
        Ok(())
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
        Daemon { cx: Context { history, world: None }, daemon: "test" }
    }

    fn op(source: &Daemon, request: &str) -> Option<Result<Answer, OpError>> {
        let args: Value = serde_json::from_str(request).unwrap();
        let name = args["op"].as_str().unwrap().to_owned();
        source.op(&name, &args, &crossbeam_channel::never())
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
        let world = op(&source, r#"{"op":"world","path":"/entity/flecs"}"#).unwrap().unwrap_err();
        assert_eq!(world.kind, "no_world");
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
            [("spans", Rule::Append), ("counters", Rule::Append), ("open", Rule::Replace)]
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
