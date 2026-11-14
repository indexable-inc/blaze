//! Live, agent-reported work mood. The app owns this ephemeral state; it never enters a replay journal.
use super::*;
use serde::{Deserialize, Serialize};

#[derive(Layout, Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
#[serde(rename_all = "snake_case")]
pub enum Emotion {
    #[default]
    Focused,
    Curious,
    Confident,
    Uncertain,
    Frustrated,
    Blocked,
    Relieved,
}

impl Emotion {
    pub fn name(self) -> &'static str {
        match self {
            Self::Focused => "focused", Self::Curious => "curious", Self::Confident => "confident",
            Self::Uncertain => "uncertain", Self::Frustrated => "frustrated", Self::Blocked => "blocked", Self::Relieved => "relieved",
        }
    }
    pub fn emoji(self) -> &'static str {
        match self {
            Self::Focused => "🎯", Self::Curious => "🧐", Self::Confident => "🙂", Self::Uncertain => "🤔",
            Self::Frustrated => "😖", Self::Blocked => "🚧", Self::Relieved => "😌",
        }
    }
    pub fn negative(self) -> bool { matches!(self, Self::Uncertain | Self::Frustrated | Self::Blocked) }
    /// The invariant: only entering a distinct negative state signals; repeating a mood, including with a new reason, is silent.
    pub fn cues_after(self, previous: Option<Self>) -> bool { self.negative() && previous != Some(self) }
}

#[derive(Component, Clone, Debug, Default, PartialEq)]
pub struct AgentEmotion {
    pub emotion: Emotion,
    pub reason: ScriptString,
}

#[derive(Debug)]
pub struct Update { pub session: u64, pub emotion: Emotion, pub changed: bool, pub cue: bool }
impl Update {
    pub fn json(&self) -> serde_json::Value {
        serde_json::json!({"session": self.session, "emotion": self.emotion.name(), "emoji": self.emotion.emoji(), "changed": self.changed})
    }
}

/// Apply one explicit live report on the UI thread, for exactly its session. No clock, IO or replay path is involved.
pub fn apply(w: WorldRef<'_>, device: &str, session: u64, emotion: Emotion, reason: &str) -> Result<Update, String> {
    if reason.chars().count() > 240 || reason.chars().any(char::is_control) {
        return Err(format!("updateEmotion for s{session}: reason must be one line of at most 240 characters"));
    }
    let mut found = None;
    w.new_query::<&model::Object>().each_entity(|e, object| {
        if object.id == session && std::iter::successors(e.parent(), |p| p.parent()).take(64)
            .any(|p| p.try_cloned::<&model::Device>().is_some_and(|d| d.sock.as_str() == device)) {
            found = Some(e);
        }
    });
    let e = found.ok_or_else(|| format!("updateEmotion: session s{session} on {device} is not present in this Harness window"))?;
    if !matches!(model::kind_name(e), "claude" | "codex" | "cursor") {
        return Err(format!("updateEmotion: s{session} is {}, not an agent", model::kind_name(e)));
    }
    let previous = e.try_cloned::<&AgentEmotion>();
    let cue = emotion.cues_after(previous.as_ref().map(|s| s.emotion));
    let now = AgentEmotion { emotion, reason: reason.into() };
    let changed = previous.as_ref() != Some(&now);
    if changed { e.set(now); }
    Ok(Update { session, emotion, changed, cue })
}

impl App {
    pub(super) fn sync_agent_emotion(&self, row: View<'_>, object: EntityView<'_>) {
        let Some(slot) = row.part("emotion") else { return };
        let status = object.try_cloned::<&AgentEmotion>();
        view::for_each_of(slot, status.as_ref().map(|s| ("mood", "hn.tree.Emotion", s)), |v, s, _| {
            v.text(s.emotion.emoji());
            let tip = if s.reason.as_str().is_empty() { format!("Agent mood: {}", s.emotion.name()) }
                else { format!("Agent mood: {}\n{}", s.emotion.name(), s.reason.as_str()) };
            v.set(harness_native_ui_input_api::Tooltip { text: tip.as_str().into() });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sequence(mut cue: impl FnMut(Emotion, Option<Emotion>) -> bool) -> Result<(), usize> {
        use Emotion::*;
        let reports = [(Focused, false), (Blocked, true), (Blocked, false), (Uncertain, true), (Uncertain, false), (Relieved, false), (Blocked, true), (Confident, false), (Frustrated, true), (Frustrated, false)];
        let mut previous = None;
        for (i, (mood, expected)) in reports.into_iter().enumerate() {
            if cue(mood, previous) != expected { return Err(i); }
            previous = Some(mood);
        }
        Ok(())
    }
    #[test]
    fn negative_mood_transitions_are_deduplicated() { assert_eq!(sequence(Emotion::cues_after), Ok(())); }
    #[test]
    fn the_oracle_catches_a_sound_on_every_negative_report() { assert_eq!(sequence(|m, _| m.negative()), Err(2)); }
    #[test]
    fn reports_target_one_agent_and_reason_updates_do_not_ring() {
        let world = super::super::scene_world().unwrap();
        let w = world.world_ref();
        let local = w.entity().set(model::Device { host: "local".into(), sock: "/local.sock".into() });
        let remote = w.entity().set(model::Device { host: "server".into(), sock: "/server.sock".into() });
        let a = w.entity().child_of(local).set(model::Object { id: 41 }).add((model::Kind::id(), model::Claude::id()));
        let b = w.entity().child_of(remote).set(model::Object { id: 41 }).add((model::Kind::id(), model::Codex::id()));
        assert!(apply(w, "/local.sock", 41, Emotion::Blocked, "Need a concrete success criterion").unwrap().cue);
        assert!(!apply(w, "/local.sock", 41, Emotion::Blocked, "Need the priority order").unwrap().cue);
        assert!(a.has(AgentEmotion::id()) && !b.has(AgentEmotion::id()));
        assert!(apply(w, "/local.sock", 99, Emotion::Focused, "").unwrap_err().contains("s99"));
    }
}
