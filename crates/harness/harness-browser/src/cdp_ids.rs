//! CDP id renumbering, sans IO: every client's ids are its own, so two connections driving one view never mix their
//! answers. A message going to a browser gets a host id (unique across the host while it is pending); its answer comes
//! back with the client's own id restored and goes only to the connection that asked. Events (no id) go to every
//! connection attached to the view, which the driver decides; this core only tells answers from events.
//!
//! The host's own requests (`Target.getTargetInfo` after a view opens) take ids from the same space, with
//! [`Asker::Host`] as their asker.
use serde::Deserialize;
use serde_json::{Map, Value};
use std::collections::BTreeMap;

/// Who asked a pending request.
#[derive(Debug, Clone, PartialEq)]
pub enum Asker {
    /// A socket connection, with the id it gave (restored on the answer exactly as it was).
    Conn { conn: u64, id: Value },
    /// The host itself.
    Host(HostAsk),
}

/// The host's own requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostAsk {
    /// `Target.getTargetInfo`, to learn a new view's page target id.
    TargetInfo,
}

/// What a message from a browser is.
#[derive(Debug, PartialEq)]
pub enum Incoming {
    /// The answer to a pending request: `message` is the JSON text to hand on (the client's id restored), `value` the
    /// parsed message (for the host's own requests).
    Answer { asker: Asker, message: String, value: Map<String, Value> },
    /// An event: hand the text on unchanged to every attached connection.
    Event,
    /// An answer whose asker is gone (its connection dropped, its view closed): nobody gets it.
    Orphan { id: i64 },
}

/// The pending requests of every view.
///
/// A request whose connection dropped stays here, its asker gone (`None`), until the browser answers it or its view
/// closes: freeing its id at once would let a later request take it, and the late answer to the dropped one would go to
/// that request's connection as if it were its own.
#[derive(Debug)]
pub struct CdpIds {
    next: i32,
    /// The largest id handed out (i32::MAX: CEF takes a C int; smaller in tests, so ids wrap).
    max: i32,
    /// host id -> (view, asker, or `None` once the asker's connection dropped)
    pending: BTreeMap<i32, (u64, Option<Asker>)>,
    #[cfg(test)]
    plant_free_on_drop: bool,
}

impl Default for CdpIds {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Deserialize)]
struct Head {
    #[serde(default)]
    id: Option<i64>,
}

impl CdpIds {
    #[must_use]
    pub fn new() -> Self {
        Self::with_ids(i32::MAX)
    }

    fn with_ids(max: i32) -> Self {
        Self {
            next: 1,
            max,
            pending: BTreeMap::new(),
            #[cfg(test)]
            plant_free_on_drop: false,
        }
    }

    /// A fresh host id: the next of 1..=max not pending. Bounded: at most `max` steps.
    fn alloc(&mut self) -> Result<i32, String> {
        for _ in 0..self.max {
            let id = self.next;
            self.next = if self.next >= self.max { 1 } else { self.next + 1 };
            if !self.pending.contains_key(&id) {
                return Ok(id);
            }
        }
        Err(format!("every DevTools request id (1..={}) is pending", self.max))
    }

    /// Renumber connection `conn`'s CDP message `message` for view `view`: the host id and the JSON text to send to the
    /// browser.
    ///
    /// # Errors
    /// Text that is not a JSON object with an `id` and a string `method`, naming the connection, the view and what is
    /// missing.
    pub fn outgoing(&mut self, view: u64, conn: u64, message: &str) -> Result<(i32, String), String> {
        let mut obj: Map<String, Value> = serde_json::from_str(message)
            .map_err(|e| format!("connection {conn}'s CDP message for view {view} is not a JSON object: {e}"))?;
        let Some(client_id) = obj.get("id").cloned() else {
            return Err(format!("connection {conn}'s CDP message for view {view} has no \"id\""));
        };
        if !client_id.is_number() {
            return Err(format!("connection {conn}'s CDP message for view {view} has an \"id\" that is not a number: {client_id}"));
        }
        if !obj.get("method").is_some_and(Value::is_string) {
            return Err(format!("connection {conn}'s CDP message {client_id} for view {view} has no string \"method\""));
        }
        let id = self.alloc().map_err(|e| format!("connection {conn}'s CDP message {client_id} for view {view}: {e}"))?;
        obj.insert("id".into(), Value::from(id));
        self.pending.insert(id, (view, Some(Asker::Conn { conn, id: client_id })));
        Ok((id, Value::Object(obj).to_string()))
    }

    /// Forget host request `id` (the browser refused it, so no answer will come): its asker.
    pub fn cancel(&mut self, id: i32) -> Option<Asker> {
        self.pending.remove(&id).and_then(|(_, a)| a)
    }

    /// The host's own request `method` (with `params`) for view `view`: the JSON text to send.
    ///
    /// # Errors
    /// Every id is pending, naming the view and method.
    pub fn host_request(&mut self, view: u64, ask: HostAsk, method: &str, params: Value) -> Result<String, String> {
        let id = self.alloc().map_err(|e| format!("the host's {method} for view {view}: {e}"))?;
        self.pending.insert(id, (view, Some(Asker::Host(ask))));
        Ok(serde_json::json!({ "id": id, "method": method, "params": params }).to_string())
    }

    /// Classify a message view `view`'s browser sent.
    ///
    /// # Errors
    /// Bytes that are not a JSON object, or an answer that names another view's request, naming the view.
    pub fn incoming(&mut self, view: u64, message: &[u8]) -> Result<Incoming, String> {
        let head: Head = serde_json::from_slice(message).map_err(|e| format!("view {view}'s browser sent a DevTools message that is not JSON: {e}"))?;
        let Some(id) = head.id else { return Ok(Incoming::Event) };
        let Ok(host_id) = i32::try_from(id) else { return Ok(Incoming::Orphan { id }) };
        let Some((owner, asker)) = self.pending.remove(&host_id) else { return Ok(Incoming::Orphan { id }) };
        if owner != view {
            self.pending.insert(host_id, (owner, asker));
            return Err(format!("view {view}'s browser answered DevTools request {host_id}, which was sent to view {owner}"));
        }
        // Its connection dropped: the id is free now, and nobody gets the answer.
        let Some(asker) = asker else { return Ok(Incoming::Orphan { id }) };
        let mut value: Map<String, Value> = serde_json::from_slice(message).map_err(|e| format!("view {view}'s answer {host_id} is not a JSON object: {e}"))?;
        if let Asker::Conn { id: client_id, .. } = &asker {
            value.insert("id".into(), client_id.clone());
        }
        let message = Value::Object(value.clone()).to_string();
        Ok(Incoming::Answer { asker, message, value })
    }

    /// Drop every request connection `conn` has pending (it went away).
    /// Its requests' ids stay taken until each is answered or its view closes (the type's doc).
    pub fn forget_conn(&mut self, conn: u64) {
        #[cfg(test)]
        if self.plant_free_on_drop {
            self.pending.retain(|_, (_, a)| !matches!(a, Some(Asker::Conn { conn: c, .. }) if *c == conn));
            return;
        }
        for (_, a) in self.pending.values_mut() {
            if matches!(a, Some(Asker::Conn { conn: c, .. }) if *c == conn) {
                *a = None;
            }
        }
    }

    /// Drop every request pending on view `view` (it closed): the askers, for the driver to answer.
    pub fn forget_view(&mut self, view: u64) -> Vec<Asker> {
        let ids: Vec<i32> = self.pending.iter().filter(|(_, (v, _))| *v == view).map(|(id, _)| *id).collect(); // alloc: once per closed view
        ids.into_iter().filter_map(|id| self.pending.remove(&id).and_then(|(_, a)| a)).collect()
    }

    /// Requests pending, a dropped connection's among them.
    #[cfg(test)]
    fn pending(&self) -> usize {
        self.pending.len()
    }
}

#[cfg(test)]
mod dst;

#[cfg(test)]
mod tests {
    use super::*;

    fn sent_id(text: &str) -> i64 {
        serde_json::from_str::<Value>(text).unwrap()["id"].as_i64().unwrap()
    }

    trait Text {
        fn text(self) -> String;
    }
    impl Text for (i32, String) {
        fn text(self) -> String {
            assert_eq!(i64::from(self.0), sent_id(&self.1));
            self.1
        }
    }

    #[test]
    fn two_connections_with_the_same_ids_get_their_own_answers() {
        let mut c = CdpIds::new();
        let a = c.outgoing(7, 1, r#"{"id":1,"method":"Runtime.evaluate","params":{"expression":"1"}}"#).unwrap().text();
        let b = c.outgoing(7, 2, r#"{"id":1,"method":"Page.reload"}"#).unwrap().text();
        let (ha, hb) = (sent_id(&a), sent_id(&b));
        assert_ne!(ha, hb);
        let Incoming::Answer { asker, message, .. } = c.incoming(7, format!(r#"{{"id":{hb},"result":{{}}}}"#).as_bytes()).unwrap() else { panic!() };
        assert_eq!(asker, Asker::Conn { conn: 2, id: Value::from(1) });
        assert_eq!(sent_id(&message), 1);
        let Incoming::Answer { asker, .. } = c.incoming(7, format!(r#"{{"id":{ha},"result":{{"v":1}}}}"#).as_bytes()).unwrap() else { panic!() };
        assert_eq!(asker, Asker::Conn { conn: 1, id: Value::from(1) });
        assert_eq!(c.pending(), 0);
    }

    #[test]
    fn events_orphans_and_forgetting() {
        let mut c = CdpIds::new();
        assert_eq!(c.incoming(3, br#"{"method":"Page.loadEventFired","params":{}}"#).unwrap(), Incoming::Event);
        let a = c.outgoing(3, 9, r#"{"id":5,"method":"DOM.getDocument"}"#).unwrap().text();
        c.forget_conn(9);
        let id = sent_id(&a);
        assert_eq!(c.incoming(3, format!(r#"{{"id":{id},"result":{{}}}}"#).as_bytes()).unwrap(), Incoming::Orphan { id });
        let t = c.host_request(4, HostAsk::TargetInfo, "Target.getTargetInfo", serde_json::json!({})).unwrap();
        assert_eq!(c.forget_view(4), vec![Asker::Host(HostAsk::TargetInfo)]);
        let id = sent_id(&t);
        assert_eq!(c.incoming(4, format!(r#"{{"id":{id}}}"#).as_bytes()).unwrap(), Incoming::Orphan { id });
    }

    #[test]
    fn refuses_what_is_not_a_request_and_crossed_answers() {
        let mut c = CdpIds::new();
        assert!(c.outgoing(1, 1, "[]").unwrap_err().contains("connection 1"));
        assert!(c.outgoing(1, 1, r#"{"method":"X"}"#).unwrap_err().contains("no \"id\""));
        assert!(c.outgoing(1, 1, r#"{"id":"a","method":"X"}"#).unwrap_err().contains("not a number"));
        assert!(c.outgoing(1, 1, r#"{"id":2}"#).unwrap_err().contains("method"));
        let a = c.outgoing(1, 1, r#"{"id":2,"method":"X"}"#).unwrap().text();
        let id = sent_id(&a);
        assert!(c.incoming(2, format!(r#"{{"id":{id}}}"#).as_bytes()).unwrap_err().contains("sent to view 1"));
        assert_eq!(c.pending(), 1, "a crossed answer leaves the request pending for its own view");
    }

    #[test]
    fn ids_wrap_and_skip_pending() {
        let mut c = CdpIds::new();
        c.next = i32::MAX;
        let a = c.outgoing(1, 1, r#"{"id":1,"method":"X"}"#).unwrap().text();
        assert_eq!(sent_id(&a), i64::from(i32::MAX));
        let b = c.outgoing(1, 1, r#"{"id":2,"method":"X"}"#).unwrap().text();
        assert_eq!(sent_id(&b), 1);
        c.next = i32::MAX;
        let d = c.outgoing(1, 1, r#"{"id":3,"method":"X"}"#).unwrap().text();
        assert_eq!(sent_id(&d), 2, "skips MAX and 1, both pending");
    }
}
