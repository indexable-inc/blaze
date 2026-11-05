//! The kernels' UI feeds: which kernel hears what the person does to the UI its cells drew. A kernel holds a
//! `ui_listen` connection for its REPL (`repl-<pid>-<t>`); the app's `ui_event` for cell `<repl>.<task>` goes to the
//! newest live feed of that REPL, or is refused naming the cell. A synchronous core over an injected sink, so the
//! property test below drives every interleaving of listens, hang-ups and events without a socket; daemon.rs is the
//! driver (a bounded channel per feed, its connection draining it).

/// One event as the kernel gets it.
#[derive(Debug, Clone, PartialEq)]
pub struct UiEv {
    pub cell: String,
    pub handler: String,
    pub value: serde_json::Value,
}

/// Why a sink took nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// Its queue is full: the kernel is not keeping up.
    Full,
    /// Its connection is gone.
    Closed,
}

/// Where a feed's events go (daemon.rs: a bounded tokio channel the connection drains).
pub trait Sink {
    /// Queue `ev`, or say why not, without waiting.
    fn offer(&self, ev: UiEv) -> Result<(), Refused>;
}

/// How many events a feed holds before the daemon refuses more (the kernel runs each handler then re-renders).
pub const FEED_CAP: usize = 256;

/// Every REPL's feed: the newest listener of each, by the id [`Feeds::listen`] gave it.
pub struct Feeds<S> {
    by_repl: std::collections::BTreeMap<String, (u64, S)>,
    next: u64,
}

impl<S> Default for Feeds<S> {
    fn default() -> Self {
        Self { by_repl: std::collections::BTreeMap::new(), next: 0 }
    }
}

/// The REPL a cell id (`<repl>.<task>`) belongs to.
pub fn repl_of(cell: &str) -> Option<&str> {
    cell.rsplit_once('.').map(|(r, _)| r).filter(|r| !r.is_empty())
}

impl<S: Sink> Feeds<S> {
    /// `repl`'s feed is `sink` from now on (an older one is dropped, which ends its connection); its id, for
    /// [`Feeds::gone`].
    pub fn listen(&mut self, repl: &str, sink: S) -> u64 {
        self.next += 1;
        self.by_repl.insert(repl.to_owned(), (self.next, sink));
        self.next
    }

    /// Feed `id` of `repl` hung up: forget it, unless a newer listener replaced it.
    pub fn gone(&mut self, repl: &str, id: u64) {
        if self.by_repl.get(repl).is_some_and(|(at, _)| *at == id) {
            self.by_repl.remove(repl);
        }
    }

    /// Hand `ev` to its cell's kernel.
    ///
    /// # Errors
    /// Naming the cell and the rule: the cell id is not `<repl>.<task>`, no kernel listens for its REPL (its agent ended,
    /// or its kernel never connected), the kernel's connection is gone, or its queue is full.
    pub fn deliver(&mut self, ev: UiEv) -> Result<(), String> {
        let cell = ev.cell.clone();
        let Some(repl) = repl_of(&cell) else {
            return Err(format!("ui_event for cell {cell:?}: a cell id is <repl>.<task>"));
        };
        let Some((_, sink)) = self.by_repl.get(repl) else {
            return Err(format!(
                "ui_event for cell {cell}: no kernel listens for {repl} (its agent has ended, or its kernel has not connected)"
            ));
        };
        match sink.offer(ev) {
            Ok(()) => Ok(()),
            Err(Refused::Full) => Err(format!(
                "ui_event for cell {cell}: kernel {repl} has {FEED_CAP} events queued and is not keeping up"
            )),
            Err(Refused::Closed) => {
                self.by_repl.remove(repl);
                Err(format!("ui_event for cell {cell}: kernel {repl} hung up"))
            }
        }
    }

    /// How many REPLs have a feed.
    pub fn len(&self) -> usize {
        self.by_repl.len()
    }

    /// Whether no REPL has a feed.
    pub fn is_empty(&self) -> bool {
        self.by_repl.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// A listener as the test sees it: what it got, whether its connection is gone, how much room it has.
    #[derive(Default)]
    struct Conn {
        got: Vec<UiEv>,
        closed: bool,
        room: usize,
    }

    #[derive(Clone)]
    struct TestSink(Rc<RefCell<Conn>>);

    impl Sink for TestSink {
        fn offer(&self, ev: UiEv) -> Result<(), Refused> {
            let mut c = self.0.borrow_mut();
            if c.closed {
                return Err(Refused::Closed);
            }
            if c.got.len() >= c.room {
                return Err(Refused::Full);
            }
            c.got.push(ev);
            Ok(())
        }
    }

    /// The planted bug the oracle must catch: an older listener's hang-up forgets the newer one.
    struct Buggy(Feeds<TestSink>);

    fn run(ops: &[(u8, u8, u8)], buggy: bool) -> Result<(), String> {
        let repls = ["repl-1-a", "repl-2-b", "repl-3-c"];
        let mut feeds: Feeds<TestSink> = Feeds::default();
        let mut bug = Buggy(Feeds::default());
        // Every listener made, by REPL: (its id, its connection), and which is the newest still connected (the model).
        let mut made: Vec<(usize, u64, Rc<RefCell<Conn>>)> = Vec::new();
        let mut newest: [Option<usize>; 3] = [None; 3];
        let mut sent = 0u32;
        for &(kind, a, b) in ops {
            let r = usize::from(a) % repls.len();
            match kind % 4 {
                // A kernel connects.
                0 => {
                    let conn = Rc::new(RefCell::new(Conn { room: 1 + usize::from(b) % 4, ..Conn::default() }));
                    let f = if buggy { &mut bug.0 } else { &mut feeds };
                    let id = f.listen(repls[r], TestSink(conn.clone()));
                    made.push((r, id, conn));
                    newest[r] = Some(made.len() - 1);
                }
                // A listener hangs up (the daemon sees its connection end).
                1 if !made.is_empty() => {
                    let k = usize::from(b) % made.len();
                    let (mr, id, conn) = (made[k].0, made[k].1, made[k].2.clone());
                    if conn.borrow().closed {
                        continue;
                    }
                    conn.borrow_mut().closed = true;
                    if buggy {
                        bug.0.by_repl.remove(repls[mr]);
                    } else {
                        feeds.gone(repls[mr], id);
                    }
                    if newest[mr] == Some(k) {
                        newest[mr] = None;
                    }
                }
                // The app sends an event for one of the REPL's cells.
                _ => {
                    sent += 1;
                    let ev = UiEv { cell: format!("{}.t{b}", repls[r]), handler: format!("h{sent}"), value: serde_json::json!(sent) };
                    let f = if buggy { &mut bug.0 } else { &mut feeds };
                    let out = f.deliver(ev.clone());
                    // The oracle: it lands with the newest connected listener of its REPL when that one has room, and
                    // only there; otherwise it is refused, naming the cell.
                    let want = newest[r].map(|k| made[k].2.clone());
                    match (&out, want) {
                        (Ok(()), Some(c)) => {
                            if c.borrow().got.last() != Some(&ev) {
                                return Err(format!("event {sent} accepted but not with the newest listener of {}", repls[r]));
                            }
                        }
                        (Ok(()), None) => return Err(format!("event {sent} accepted with no listener for {}", repls[r])),
                        (Err(e), Some(c)) => {
                            let c = c.borrow();
                            if c.got.len() < c.room {
                                return Err(format!("event {sent} refused ({e}) though {} had a listener with room", repls[r]));
                            }
                            if !e.contains(&ev.cell) {
                                return Err(format!("refusal {e:?} does not name the cell"));
                            }
                        }
                        (Err(e), None) => {
                            if !e.contains(&ev.cell) {
                                return Err(format!("refusal {e:?} does not name the cell"));
                            }
                        }
                    }
                    // No listener but the intended one ever got it.
                    for (i, (_, _, c)) in made.iter().enumerate() {
                        if Some(i) != newest[r] && c.borrow().got.last() == Some(&ev) {
                            return Err(format!("event {sent} reached listener {i}, not the newest of {}", repls[r]));
                        }
                    }
                }
            }
        }
        Ok(())
    }

    #[hegel::test]
    fn every_event_reaches_the_newest_live_listener_or_is_refused(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let ops: Vec<(u8, u8, u8)> = tc.draw(gs::vecs(gs::tuples3(gs::integers(), gs::integers(), gs::integers())).max_size(120));
        if let Err(e) = run(&ops, false) {
            panic!("{e}");
        }
    }

    /// The oracle catches the planted bug: listen twice on one REPL, the older hangs up, an event comes.
    #[test]
    fn the_oracle_catches_a_hang_up_that_forgets_the_newer_listener() {
        let ops = [(0, 0, 3), (0, 0, 3), (1, 0, 0), (2, 0, 0)];
        assert!(run(&ops, false).is_ok());
        let e = run(&ops, true).expect_err("the planted bug must be caught");
        assert!(e.contains("refused"), "{e}");
    }

    #[test]
    fn a_cell_names_its_repl() {
        assert_eq!(repl_of("repl-41-lz3.t7"), Some("repl-41-lz3"));
        assert_eq!(repl_of("t7"), None);
        assert_eq!(repl_of(".t7"), None);
    }
}
