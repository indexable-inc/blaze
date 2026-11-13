//! External transcripts use the normal chat view, but own no daemon session and cannot send agent commands.
use super::*;
use harness_chat_library::{Parsed, Session};
use std::collections::BTreeMap;

const FIRST_EXTERNAL: u64 = 1 << 63;

struct Open {
    session: Session,
    entity: u64,
}

/// Local view state only. Neither these ids nor the source conversations are written by Harness.
#[derive(Default)]
pub(super) struct External {
    by_sid: BTreeMap<u64, Open>, // alloc: the external conversations the person opened, for this window
}

impl App {
    pub(super) fn external_session(&self, sid: u64) -> Option<&Session> {
        self.external.by_sid.get(&sid).map(|open| &open.session)
    }

    /// Show a source snapshot without starting or resuming its agent.
    pub(super) fn open_external(&mut self, parsed: Parsed) {
        self.genui_blur();
        self.world.lookup(APP).remove((harness_native_ui_input_api::Focused::id(), flecs::Wildcard::ID));
        self.update_external(parsed, true);
    }

    /// Refresh only a view that is still open, without changing any pane's focus or selection.
    pub(super) fn refresh_external(&mut self, parsed: Parsed) {
        self.update_external(parsed, false);
    }

    fn update_external(&mut self, parsed: Parsed, select: bool) {
        let Parsed { session, events, .. } = parsed;
        let known = self.external.by_sid.iter().find(|(_, open)| open.session.key == session.key).map(|(&sid, open)| (sid, open.entity));
        if !select && known.is_none() {
            // The read may have completed after the person closed its view.
            return;
        }
        let w = self.world.world_ref();
        let kind = match session.agent.as_str() {
            "claude" => *w.component_id::<model::Claude>(),
            "codex" => *w.component_id::<model::Codex>(),
            agent => {
                eprintln!("harness-native: external chat {}: provider {agent:?} has no transcript view", session.key);
                return;
            }
        };
        let (sid, entity) = match known {
            Some(known) => known,
            None => {
                let sid = match self.external.by_sid.last_key_value() {
                    Some((&sid, _)) => match sid.checked_add(1) {
                        Some(sid) => sid,
                        None => {
                            eprintln!("harness-native: external chat {}: the window exhausted its external view ids", session.key);
                            return;
                        }
                    },
                    None => FIRST_EXTERNAL,
                };
                let e = w.entity().set(model::Object { id: sid }).child_of(w.lookup(model::LOCAL));
                e.add(model::ChatMode::id()).add(model::Pinned::id());
                e.add((model::Status::id(), model::Exited::id()));
                (sid, *e.id())
            }
        };
        let e = w.entity_from_id(entity);
        e.add((model::Kind::id(), kind));
        e.set(model::Name { s: session.title.as_str().into() });
        e.set(model::Title { s: session.title.as_str().into() });
        e.set(model::Cwd { s: session.cwd.as_str().into() });
        e.set(model::AgentModel { s: session.model.as_str().into(), effort: session.effort.as_str().into() });
        e.set(model::Created { ms: session.updated_ms });
        e.set(model::Ended { ms: session.updated_ms });
        let mut transcript = model::chat::Transcript::default();
        for event in &events {
            transcript.apply(event);
        }
        // The source may still be running elsewhere. This view never advertises that Harness owns its live turn.
        transcript.busy = false;
        self.chats.borrow_mut().insert(sid, transcript);
        self.external.by_sid.insert(sid, Open { session, entity });
        if select {
            self.select(Some(entity));
        }
        self.poke();
    }

    /// Close only our view. No signal, archive request or filesystem write reaches the external conversation.
    pub(super) fn close_external(&mut self, entity: u64) -> bool {
        let Some(sid) = self.session_of(entity).filter(|sid| self.external.by_sid.contains_key(sid)) else { return false };
        if self.selected() == Some(entity) {
            self.select(None);
        }
        self.external.by_sid.remove(&sid);
        self.chats.borrow_mut().remove(&sid);
        self.chat.borrow_mut().remove(&sid);
        let e = self.world.entity_from_id(entity);
        if e.is_alive() {
            e.destruct();
        }
        self.poke();
        true
    }

    /// Replace the composer with a source label, restoring the normal composer when this pane changes chats.
    pub(super) fn external_footer(&self, pane: View<'_>, session: Option<&Session>) {
        let w = self.world.world_ref();
        let Some(foot) = pane.part("foot") else { return };
        if let Some(card) = foot.part("card") {
            if let Some(source) = view::prefab(w, "hn.ChatPane").and_then(|p| View::of(p).part("foot")).and_then(|p| p.part("card")) {
                if let Some(mut size) = source.entity().try_cloned::<&Size>() {
                    if session.is_some() {
                        size.h = Sizing::fixed(0.0);
                    }
                    if card.entity().try_cloned::<&Size>() != Some(size) {
                        card.set(size);
                    }
                }
                if let Some(mut padding) = source.entity().try_cloned::<&harness_native_ui_layout_api::Padding>() {
                    if session.is_some() {
                        padding = harness_native_ui_layout_api::Padding::default();
                    }
                    if card.entity().try_cloned::<&harness_native_ui_layout_api::Padding>() != Some(padding) {
                        card.set(padding);
                    }
                }
            }
            // Keep the composer out of the layout tree and its own root passes. Hidden then remains set and
            // excludes the entire subtree from drawing and hit testing, including lifted controls.
            if session.is_some() {
                card.entity().add(harness_native_ui_node_api::Clip::id());
                card.entity().add(harness_native_ui_node_api::Placed::id());
                card.entity().add(harness_native_ui_node_api::Offscreen::id());
                card.entity().add(harness_native_ui_node_api::Hidden::id());
            } else {
                for tag in [*w.component_id::<harness_native_ui_node_api::Clip>(), *w.component_id::<harness_native_ui_node_api::Placed>(), *w.component_id::<harness_native_ui_node_api::Offscreen>(), *w.component_id::<harness_native_ui_node_api::Hidden>()] {
                    if card.entity().has(tag) {
                        card.entity().remove(tag);
                    }
                }
            }
        }
        if let Some(slot) = foot.part("external") {
            view::for_each_of(slot, session.map(|s| ("source", "hn.ExternalChatNote", s)), |v, session, _| {
                let provider = if session.agent == "claude" { "Claude" } else { "Codex" };
                if let Some(words) = v.part("words") {
                    let error = self.external_source_error(&session.key);
                    words.text(&match error {
                        Some(_) => format!("{provider} chat · source update failed · showing last snapshot"),
                        None => format!("{provider} chat · read-only"),
                    });
                    words.entity().set(harness_native_ui_input_api::Tooltip {
                        text: format!("{}\n{}\n{}\n{}", session.title, session.chat_id, session.path.display(), error.unwrap_or("Opened from another app. Continue chatting in its original terminal.")).as_str().into(),
                    });
                }
            });
        }
        if session.is_some() {
            // A pane can have shown an ordinary chat immediately before this source snapshot.
            for name in ["activity", "activity_pop", "aside", "queued"] {
                if let Some(slot) = foot.part(name) {
                    view::for_each_of(slot, std::iter::empty::<(&str, &str, ())>(), |_, (), _| {});
                    if slot.entity().try_cloned::<&harness_native_ui_layout_api::Padding>().is_some_and(|p| p != harness_native_ui_layout_api::Padding::default()) {
                        slot.set(harness_native_ui_layout_api::Padding::default());
                    }
                }
            }
        }
        if session.is_some() && self.focused_pane() == Some(pane.id()) {
            let bar = View::of(w.entity_from_id(self.ids.side_chat));
            for slot in ["pick", "meter", "menu", "pop"] {
                if let Some(slot) = bar.part(slot) {
                    view::for_each_of(slot, std::iter::empty::<(&str, &str, ())>(), |_, (), _| {});
                }
            }
        }
    }

    /// A recorded cell may contain buttons and fields. Draw their recorded values without any event targets.
    pub(super) fn external_inert_cell(row: View<'_>) {
        fn clear(e: EntityView<'_>, depth: usize) {
            if depth > 64 {
                eprintln!("harness-native: external chat cell {} exceeds the read-only view depth of 64", *e.id());
                e.add(harness_native_ui_node_api::Hidden::id());
                return;
            }
            let w = e.world();
            for tag in [*w.component_id::<harness_native_ui_input_api::Action>(), *w.component_id::<harness_native_ui_input_api::Pressable>(), *w.component_id::<harness_native_ui_input_api::Focusable>(), *w.component_id::<chat::GenOn>()] {
                if e.has(tag) {
                    e.remove(tag);
                }
            }
            e.each_child(|child| clear(child, depth + 1));
        }
        clear(row.entity(), 0);
    }
}
