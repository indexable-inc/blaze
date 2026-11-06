//! The socket's DST: the real [`CdpIds`] core, routed as host.rs routes it (`cdp`, `devtools`, `closed`, `dropped`),
//! between kernel connections and views' browsers, over the queues the host has: one FIFO of commands every
//! connection's reader thread posts to the UI thread (kernel.rs, `host::post`), each connection's FIFO writer queue,
//! and each browser answering its pending requests in any order. The id space is four ids, so ids wrap and are reused.
//!
//! The draws: connections connect, send requests (their own ids, colliding across connections) and drop; the UI
//! thread drains one command; a browser answers any one of its requests; a view closes and another opens (with the
//! host's own `Target.getTargetInfo`); a connection reads one message.
//!
//! The oracle: a connection gets only answers to requests it sent, each with its own id and its own request's result
//! (a token the request carried), each at most once; after the run settles (everything drained and answered) every
//! live connection has had exactly one answer or one error for each of its requests; a browser's answer is never
//! refused as crossed.
//!
//! Control: freeing a dropped connection's ids at once (the bug the tombstones fix) must be caught.
use super::*;
use std::collections::{BTreeSet, VecDeque};

const SPACE: i32 = 4;
const CONTROL_SEEDS: u64 = 3000;

enum Cmd {
    Kernel { conn: usize, view: u64, text: String },
    Dropped { conn: usize },
}

#[derive(Default)]
struct Conn {
    /// The reader still runs: it can post requests.
    live: bool,
    next_client: i64,
    /// client id -> token
    asked: BTreeMap<i64, u64>,
    /// The writer queue.
    inbox: VecDeque<String>,
}

struct World {
    ids: CdpIds,
    conns: Vec<Conn>,
    /// Connections the host knows (removed when it handles their Dropped).
    host_conns: BTreeSet<usize>,
    ui: VecDeque<Cmd>,
    /// Two open views (their ids), each with its browser's requests awaiting an answer: (host id, token).
    views: [(u64, Vec<(i32, Option<u64>)>); 2],
    next_view: u64,
    next_token: u64,
}


fn cdp_error(id: &Value) -> String {
    serde_json::json!({ "id": id, "error": { "code": -32000, "message": "refused" } }).to_string()
}

impl World {
    fn new(plant: bool) -> Result<Self, String> {
        let mut ids = CdpIds::with_ids(SPACE);
        ids.plant_free_on_drop = plant;
        let mut w = Self { ids, conns: Vec::new(), host_conns: BTreeSet::new(), ui: VecDeque::new(), views: [(0, Vec::new()), (0, Vec::new())], next_view: 0, next_token: 0 };
        w.open(0)?;
        w.open(1)?;
        Ok(w)
    }

    fn to_conn(&mut self, conn: usize, msg: String) {
        if self.host_conns.contains(&conn) {
            self.conns[conn].inbox.push_back(msg);
        }
    }

    /// host.rs's `open`: the view's own Target.getTargetInfo goes first.
    fn open(&mut self, slot: usize) -> Result<(), String> {
        self.next_view += 1;
        let view = self.next_view;
        let text = self.ids.host_request(view, HostAsk::TargetInfo, "Target.getTargetInfo", serde_json::json!({}));
        self.views[slot] = (view, Vec::new());
        match text {
            Ok(t) => {
                let id = serde_json::from_str::<Value>(&t).map_err(|e| e.to_string())?["id"].as_i64().ok_or("no id")?;
                self.views[slot].1.push((id as i32, None));
            }
            // Every id pending: host.rs closes the view; here it opens with nothing asked.
            Err(e) if e.contains("is pending") => {}
            Err(e) => return Err(e),
        }
        Ok(())
    }

    /// host.rs's `closed`: every pending request of the view is answered with an error; the browser's are gone.
    fn close(&mut self, slot: usize) -> Result<(), String> {
        let view = self.views[slot].0;
        for a in self.ids.forget_view(view) {
            if let Asker::Conn { conn, id } = a {
                self.to_conn(conn as usize, cdp_error(&id));
            }
        }
        self.open(slot)
    }

    fn connect(&mut self) {
        if self.conns.len() < 4 {
            self.host_conns.insert(self.conns.len());
            self.conns.push(Conn { live: true, next_client: 1, ..Conn::default() });
        }
    }

    fn send(&mut self, conn: usize, slot: usize) {
        let Some(c) = self.conns.get_mut(conn).filter(|c| c.live) else { return };
        let id = c.next_client;
        c.next_client += 1;
        self.next_token += 1;
        c.asked.insert(id, self.next_token);
        let text = serde_json::json!({ "id": id, "method": "Runtime.evaluate", "params": { "t": self.next_token } }).to_string();
        self.ui.push_back(Cmd::Kernel { conn, view: self.views[slot].0, text });
    }

    fn drop_conn(&mut self, conn: usize) {
        let Some(c) = self.conns.get_mut(conn).filter(|c| c.live) else { return };
        c.live = false;
        c.asked.clear();
        self.ui.push_back(Cmd::Dropped { conn });
    }

    /// The UI thread handles one command.
    fn drain(&mut self) -> Result<(), String> {
        match self.ui.pop_front() {
            None => Ok(()),
            Some(Cmd::Dropped { conn }) => {
                self.host_conns.remove(&conn);
                self.ids.forget_conn(conn as u64);
                // Its writer queue goes with it.
                self.conns[conn].inbox.clear();
                Ok(())
            }
            Some(Cmd::Kernel { conn, view, text }) => {
                let Some(slot) = self.views.iter().position(|v| v.0 == view) else {
                    let id = serde_json::from_str::<Value>(&text).map_err(|e| e.to_string())?["id"].clone();
                    self.to_conn(conn, cdp_error(&id));
                    return Ok(());
                };
                match self.ids.outgoing(view, conn as u64, &text) {
                    Ok((id, sent)) => {
                        let v: Value = serde_json::from_str(&sent).map_err(|e| e.to_string())?;
                        if v["id"].as_i64() != Some(i64::from(id)) {
                            return Err(format!("outgoing gave host id {id} but sent {sent}"));
                        }
                        self.views[slot].1.push((id, v["params"]["t"].as_u64()));
                    }
                    Err(_) => {
                        let id = serde_json::from_str::<Value>(&text).map_err(|e| e.to_string())?["id"].clone();
                        self.to_conn(conn, cdp_error(&id));
                    }
                }
                Ok(())
            }
        }
    }

    /// View `slot`'s browser answers request `pick` (mod its count): host.rs's `devtools`.
    fn answer(&mut self, slot: usize, pick: usize) -> Result<(), String> {
        let (view, reqs) = &mut self.views[slot];
        if reqs.is_empty() {
            return Ok(());
        }
        let (id, token) = reqs.remove(pick % reqs.len());
        let view = *view;
        let text = match token {
            Some(t) => serde_json::json!({ "id": id, "result": { "t": t } }).to_string(),
            None => serde_json::json!({ "id": id, "result": { "targetInfo": { "targetId": "T" } } }).to_string(),
        };
        match self.ids.incoming(view, text.as_bytes())? {
            Incoming::Answer { asker: Asker::Conn { conn, .. }, message, .. } => self.to_conn(conn as usize, message),
            Incoming::Answer { asker: Asker::Host(_), value, .. } => {
                if token.is_some() {
                    return Err(format!("view {view}'s answer to a connection's request {id} was taken as the host's own: {value:?}"));
                }
            }
            Incoming::Event => return Err(format!("an answer {text} was taken for an event")),
            Incoming::Orphan { .. } => {}
        }
        Ok(())
    }

    /// Connection `conn` reads one message: the oracle.
    fn read(&mut self, conn: usize) -> Result<(), String> {
        // A dropped connection reads nothing: its socket is closed.
        let Some(c) = self.conns.get_mut(conn).filter(|c| c.live) else { return Ok(()) };
        let Some(msg) = c.inbox.pop_front() else { return Ok(()) };
        let v: Value = serde_json::from_str(&msg).map_err(|e| format!("connection {conn} read {msg}: {e}"))?;
        let id = v["id"].as_i64().ok_or_else(|| format!("connection {conn} read an answer with no id: {msg}"))?;
        let Some(token) = c.asked.remove(&id) else {
            return Err(format!("connection {conn} got {msg}, an answer to id {id}, which it has not asked or already had answered"));
        };
        if v.get("error").is_none() && v["result"]["t"].as_u64() != Some(token) {
            return Err(format!("connection {conn} got {msg} for its request {id}, which carried token {token}: another request's answer"));
        }
        Ok(())
    }

    fn step(&mut self, op: u8, a: u8, b: u8) -> Result<(), String> {
        let conn = usize::from(a) % 4;
        let slot = usize::from(b) % 2;
        match op % 9 {
            0 => Ok(self.connect()),
            1 | 2 => Ok(self.send(conn, slot)),
            3 => Ok(if b % 3 == 0 { self.drop_conn(conn) }),
            4 | 5 => self.drain(),
            6 => self.answer(slot, usize::from(a)),
            7 => if a % 5 == 0 { self.close(slot) } else { Ok(()) },
            _ => self.read(conn),
        }
    }

    /// Drain everything, answer everything, read everything; every live connection is answered for every request.
    fn settle(&mut self) -> Result<(), String> {
        while !self.ui.is_empty() {
            self.drain()?;
        }
        for slot in 0..2 {
            while !self.views[slot].1.is_empty() {
                self.answer(slot, 0)?;
            }
        }
        for conn in 0..self.conns.len() {
            while !self.conns[conn].inbox.is_empty() {
                self.read(conn)?;
            }
            if let Some((id, _)) = self.conns[conn].asked.iter().next().filter(|_| self.conns[conn].live) {
                return Err(format!("live connection {conn}'s request {id} was never answered"));
            }
        }
        Ok(())
    }
}

fn run(ops: &[(u8, u8, u8)], plant: bool) -> Result<(), String> {
    let mut w = World::new(plant)?;
    w.connect();
    w.connect();
    for (i, &(op, a, b)) in ops.iter().enumerate() {
        w.step(op, a, b).map_err(|e| format!("step {i} {:?}: {e}", (op, a, b)))?;
    }
    w.settle()
}

fn plan_of(seed: u64) -> Vec<(u8, u8, u8)> {
    let mut s = seed;
    let mut next = || {
        s = s.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = s;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    };
    let n = 10 + next() % 90;
    (0..n).map(|_| { let r = next(); (r as u8, (r >> 8) as u8, (r >> 16) as u8) }).collect() // alloc: a test plan
}

#[hegel::test]
fn every_cdp_answer_reaches_only_its_asker_once_and_every_live_request_is_answered(tc: hegel::TestCase) {
    use hegel::generators as gs;
    let ops: Vec<(u8, u8, u8)> = tc.draw(gs::vecs(gs::tuples3(gs::integers(), gs::integers(), gs::integers())).max_size(150));
    if let Err(e) = run(&ops, false) {
        panic!("{e}");
    }
}

#[test]
fn seeded_plans_pass() {
    for seed in 0..CONTROL_SEEDS {
        if let Err(e) = run(&plan_of(seed), false) {
            panic!("seed {seed}: {e}");
        }
    }
}

#[test]
fn the_oracle_catches_a_dropped_connections_ids_freed_at_once() {
    for seed in 0..CONTROL_SEEDS {
        if let Err(e) = run(&plan_of(seed), true) {
            eprintln!("the planted free-on-drop caught at seed {seed}: {e}");
            return;
        }
    }
    panic!("the oracle caught the planted free-on-drop at none of seeds 0..{CONTROL_SEEDS}");
}
