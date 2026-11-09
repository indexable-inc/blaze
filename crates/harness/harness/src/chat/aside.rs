//! An aside's one turn (`aside`, harness-proto's `Request::Aside`): a question asked of a fork of a chat's conversation,
//! outside it. The daemon (daemon.rs `aside`) runs the fork as a one-shot agent process through the chat's own adapter
//! (claude.rs, codex.rs: their events are the conversation's, so nothing about either agent is known here) and folds the
//! events it reads into the answer with a [`Turn`]; the fork's events go nowhere else, so the chat's log never sees them.
use harness_proto::event::Body;

/// What an aside's turn came to: the fork's last words (markdown), or why there are none.
pub type Answer = Result<String, String>;

/// The events of an aside's fork, read until its one turn ends: an error it reported on the way is kept, to name why the
/// turn failed when it did.
#[derive(Debug, Default)]
pub struct Turn {
    error: Option<String>,
}

impl Turn {
    /// One event of the fork's: its answer once the turn ends (its last words, or why it failed: the error it reported,
    /// else how the turn stopped), None until then.
    pub fn see(&mut self, b: Body) -> Option<Answer> {
        match b {
            Body::Error { text } => {
                self.error = Some(text);
                None
            }
            Body::TurnEnd { ok: true, text, .. } if !text.trim().is_empty() => Some(Ok(text)),
            Body::TurnEnd { ok: true, stop, .. } => Some(Err(format!("its turn ended ({stop}) without a word"))),
            Body::TurnEnd { stop, .. } => Some(Err(self.error.take().unwrap_or_else(|| format!("its turn ended {stop}, saying no more")))),
            _ => None,
        }
    }

    /// The last error the fork reported, for an aside whose fork ended before its turn did.
    #[must_use]
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::Adapter;
    use crate::chat::codex::Codex;
    use serde_json::{Value, json};

    /// The lines an adapter wrote, as JSON.
    fn sent(lines: &[String]) -> Vec<Value> {
        lines.iter().map(|l| serde_json::from_str(l).expect("a JSON-RPC line")).collect()
    }

    /// A Codex aside, as codex app-server answers it: the fork asked as an ephemeral `thread/fork` of the chat's thread
    /// (not a start or a resume, and no model list), its one `turn/start` on the fork once that is answered, carrying
    /// the chat's model and effort, and the turn's last words the answer.
    #[test]
    fn a_codex_aside_forks_the_thread_and_answers_with_its_last_words() {
        let mut a = Codex::fork("/tmp", "th-main").with_choice((Some("gpt-5.6".into()), Some("high".into())));
        let mut turn = Turn::default();
        let mut out = a.start();
        out.extend(a.user("what is 2+2?", &[]));
        let init = sent(&out);
        assert_eq!(init.len(), 1, "the turn waits for the fork: {init:?}");
        assert_eq!(init[0]["method"], "initialize");
        let mut feed = |a: &mut Codex, line: Value| -> (Vec<Value>, Option<Answer>) {
            let mut got = None;
            for b in a.events(&line.to_string()) {
                got = got.or(turn.see(b));
            }
            (sent(&a.replies()), got)
        };
        let (after_init, _) = feed(&mut a, json!({"id": init[0]["id"], "result": {}}));
        let methods: Vec<&str> = after_init.iter().filter_map(|v| v["method"].as_str()).collect();
        assert_eq!(methods, ["initialized", "thread/fork"]);
        let fork = &after_init[1];
        assert_eq!(fork["params"]["threadId"], "th-main");
        assert_eq!(fork["params"]["ephemeral"], true);
        let (after_fork, _) = feed(&mut a, json!({"id": fork["id"], "result": {"thread": {"id": "th-fork"}, "model": "gpt-5.6", "reasoningEffort": "high"}}));
        assert_eq!(after_fork.len(), 1, "{after_fork:?}");
        let start = &after_fork[0];
        assert_eq!(start["method"], "turn/start");
        assert_eq!(start["params"]["threadId"], "th-fork");
        assert_eq!((&start["params"]["model"], &start["params"]["effort"]), (&json!("gpt-5.6"), &json!("high")));
        feed(&mut a, json!({"id": start["id"], "result": {"turn": {"id": "tu1"}}}));
        feed(&mut a, json!({"method": "turn/started", "params": {"turn": {"id": "tu1"}}}));
        let (_, none) = feed(&mut a, json!({"method": "item/completed", "params": {"item": {"id": "m1", "type": "agentMessage", "text": "4."}}}));
        assert!(none.is_none(), "no answer before the turn ends");
        let (_, answer) = feed(&mut a, json!({"method": "turn/completed", "params": {"turn": {"status": "completed", "durationMs": 9}}}));
        assert_eq!(answer, Some(Ok("4.".to_owned())));
    }

    /// A failed turn's answer is the error the fork reported on the way, else how it stopped; one that ends well without a
    /// word says so rather than answering nothing.
    #[test]
    fn a_failed_or_silent_turn_says_why() {
        let end = |ok: bool, stop: &str, text: &str| Body::TurnEnd { ok, stop: stop.into(), duration_ms: None, cost_usd: None, usage: Value::Null, text: text.into() };
        let mut t = Turn::default();
        assert_eq!(t.see(Body::Error { text: "rate limited".into() }), None);
        assert_eq!(t.see(end(false, "error", "")), Some(Err("rate limited".to_owned())));
        let mut t = Turn::default();
        assert!(t.see(end(false, "interrupted", "")).is_some_and(|a| a.is_err_and(|e| e.contains("interrupted"))));
        let mut t = Turn::default();
        assert!(t.see(end(true, "end_turn", "  ")).is_some_and(|a| a.is_err_and(|e| e.contains("without a word"))));
    }
}
