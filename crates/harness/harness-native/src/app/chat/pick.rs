//! Den's model and effort picker in a chat's status bar under its composer (scene/widgets/chat.flecs `hn.ChatPick`,
//! in hn.ChatPane's `foot.status`), its context meter, and its plan meter (plan.rs).
//!
//! The status bar's leading edge shows one quiet control, the model's name, its effort as a level meter (a bar per effort the
//! model takes, those up to the effort lit) and a small chevron, on a wash under the pointer, its words and lit bars
//! easing toward the ink. Its tooltip says the model and effort and how to change them; never while its menu is open.
//!
//! The effort moves without opening anything: a sideways two-finger swipe over the control steps it a stop per detent
//! (harness_native_ui_view::level::Swipe: each `PickFeel::step` points the fingers travel, momentum never steps), with a
//! pulse on a Force Touch trackpad at each stop (`Haptic::LevelChange`, played by window.rs); Shift-Command-/ steps it
//! round. A moved effort is a [`Wish`]: shown at once (the control says its name after the meter while it moves and a
//! moment after), sent `PickFeel::commit` after the last step (one switch for a swipe through several stops, never two
//! requests racing), and dropped once the chat says it runs with it.
//!
//! A press opens the menu above it (hn.ChatPickMenu, a fixed width): the models (Command-/ opens them alone), then the
//! effort as a level slider (prims level.flecs `ui.LevelSlider`: press or drag to a stop, the arrows step it, a swipe
//! over it steps it as over the control; the menu stays open). The models are searchable (search_menu.flecs's filter,
//! harness_native_ui_view::search): typing filters them, the first match highlighted, up and down move, Return picks.
//! Each agent kind's models sit under its head led by its maker's mark, the chat's own kind first: a row is a model's
//! name with its version quiet after it, the one the chat runs ticked; the line about the row the pointer or the keys
//! are on (else the chat's own model) shows whole under the list, wrapping, never cut. A pick sends
//! `Request::SetModel` (link.rs `chat_choose`); the daemon has the agent switch (claude's `set_model` and
//! `apply_flag_settings` control requests, codex's next `turn/start`) and logs the choice, so a resumed chat keeps it. A
//! chat whose own log has no `models` event picks from the list its agent kind last gave the daemon (`list`'s `models`,
//! link.rs `agent_models`); an ended chat's pick is recorded and its resume starts with it. Until any agent of its kind
//! has listed, or for a model with no efforts, that part of the menu says why in a short line. Another kind's model
//! cannot switch this chat (its agent is one process for its life), so it starts a chat of that kind on that model in
//! the same directory (`chat:spawn:`), which replaces this one when nothing was sent in it yet. Every pick is remembered
//! ([`App::remember_choice`]): cmd+N starts on the last agent, model and effort. A press anywhere else, or Escape,
//! closes the menu.
//!
//! At the bar's trailing edge, the context meter: a small ring filling as the conversation fills its model's window
//! (den's: muted, the warning tone from [`WARN`], the failed tone from [`FULL`]); under the pointer, its popover
//! (context.rs) says how full, with the percentage, and with what. After it, how much of the person's plan is used
//! (plan.rs), once any agent of the chat's kind has said.
use super::*;
use harness_native_ui_look_api::Stroke;
use harness_native_ui_text_api::Vector;
use harness_proto::event::ModelChoice;
use harness_native_ui_view::level::{self, Swipe};
use harness_native_ui_view::search::{self, Edit, Entry};
use crate::haptic::Haptic;
use crate::look::PickFeel;
use std::collections::BTreeMap;

/// The share of the window from which the meter shows in the warning tone, and from which in the failed tone (den's).
const WARN: f64 = 0.8;
const FULL: f64 = 0.95;
/// The track under the ring: the ink at this alpha (den's).
pub(super) const TRACK_ALPHA: f32 = 0.18;

/// Which of the picker's menus is open: the models, the efforts, or both.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pick {
    Model,
    Effort,
    All,
}

impl Pick {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "model" => Some(Self::Model),
            "effort" => Some(Self::Effort),
            "all" => Some(Self::All),
            _ => None,
        }
    }
}

/// The models chat `t` (of agent kind `kind`) picks from: those its own log listed, else those its kind last listed to
/// the daemon (`kept`).
fn models_of<'a>(t: &'a Transcript, kind: &str, kept: &'a BTreeMap<String, Vec<ModelChoice>>) -> &'a [ModelChoice] {
    if !t.models.is_empty() {
        return &t.models;
    }
    let kind = if t.agent.is_empty() { kind } else { t.agent.as_str() };
    kept.get(kind).map_or(&[], Vec::as_slice)
}

/// Whose mark a model's row in the menu shows, as an `hn.Kind` child: Claude's for Anthropic's models, OpenAI's
/// (`codex`) for GPT and the o-series, else the agent kind's own (`agent`: a Cursor chat's `auto`), from the model's id
/// and the model it runs as (`cur`), else `model`.
fn maker(cur: Option<&ModelChoice>, model: &str, agent: &str) -> &'static str {
    let names = cur.map_or([model, ""], |c| [c.id.as_str(), c.model.as_str()]);
    let is = |pre: &[&str]| names.iter().any(|n| pre.iter().any(|p| n.to_ascii_lowercase().starts_with(p)));
    if is(&["claude", "opus", "sonnet", "haiku", "fable"]) {
        "claude"
    } else if is(&["gpt", "o1", "o3", "o4", "codex"]) {
        "codex"
    } else {
        match agent {
            "claude" => "claude",
            "codex" => "codex",
            "cursor" => "cursor",
            _ => "other",
        }
    }
}

/// The model the chat runs, among `models`: what it runs as, else what was last chosen (or started on), else `listed`
/// (the daemon's word for it).
fn current<'a>(t: &Transcript, models: &'a [ModelChoice], listed: &str) -> Option<&'a ModelChoice> {
    let by = |m: &str| if m.is_empty() { None } else { models.iter().find(|c| c.is(m)) };
    by(&t.model).or_else(|| by(&t.chosen_model)).or_else(|| by(listed))
}

/// The model and effort chat `t` shows: what its agent said it runs (its `session` event, or a choice), else what it was
/// chosen or started on ([`Transcript::start_on`]: a cmd+N's, until its agent is up), else `listed`, the daemon's listing
/// of the object (`(model, effort)`, empty while its agent has not said).
fn shown<'a>(t: &'a Transcript, listed: (&'a str, &'a str)) -> (&'a str, &'a str) {
    let or = |a: &'a str, b: &'a str| if a.is_empty() { b } else { a };
    if !t.model.is_empty() {
        (t.model.as_str(), or(t.effort.as_str(), listed.1))
    } else if !t.chosen_model.is_empty() || !t.chosen_effort.is_empty() {
        (or(t.chosen_model.as_str(), listed.0), or(t.chosen_effort.as_str(), listed.1))
    } else {
        listed
    }
}

/// What the picker's control says for a chat: the models it picks from, the one it runs, its name and effort, the
/// control's words, the effort's level (its place among the model's efforts, from its kind's kept list until its own
/// log lists them) and its tooltip. A chat with a choice shows it from its first frame; `name` is empty (no control)
/// only while nothing names a model.
struct Label<'a> {
    models: &'a [ModelChoice],
    cur: Option<&'a ModelChoice>,
    name: &'a str,
    effort: &'a str,
    words: String,
    level: Option<(usize, usize)>,
    tip: String,
}

impl<'a> Label<'a> {
    /// Chat `t`'s, an agent of kind `kind`, its kind's kept lists `kept`, the daemon's listing `listed`.
    fn of(t: &'a Transcript, kind: &'a str, kept: &'a BTreeMap<String, Vec<ModelChoice>>, listed: (&'a str, &'a str)) -> Self {
        let models = models_of(t, kind, kept);
        let (model, effort) = shown(t, listed);
        let cur = current(t, models, model);
        let name = cur.map_or(model, |c| if c.name.is_empty() { c.id.as_str() } else { c.name.as_str() });
        // The model's name and its effort's level; an effort with no place among its model's said in words
        // ("Opus · Medium"), and the model alone when the effort is unknown. The effort's name is the tooltip either way.
        let level = level(cur, effort);
        let words = match level {
            None if !effort.is_empty() => format!("{name} \u{b7} {}", effort_word(effort)),
            _ => name.to_owned(),
        };
        let tip = if effort.is_empty() {
            "Model and effort  \u{2318}/".to_owned()
        } else {
            format!("{name} \u{b7} {} effort  \u{2318}/", effort_word(effort))
        };
        Self { models, cur, name, effort, words, level, tip }
    }
}

/// An effort's word as den shows it: `xhigh` is Extra High, the rest capitalised.
fn effort_word(e: &str) -> String {
    if e == "xhigh" {
        return "Extra High".to_owned();
    }
    let mut c = e.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

/// A token count as the meter's popover says it: 950, 46.2k, 258k, 1M.
pub(super) fn tokens(n: u64) -> String {
    let (v, unit) = match n {
        1_000_000.. => (n as f64 / 1e6, "M"),
        1_000.. => (n as f64 / 1e3, "k"),
        _ => return n.to_string(),
    };
    let s = if v >= 100.0 { format!("{v:.0}") } else { format!("{v:.1}") };
    format!("{}{unit}", s.strip_suffix(".0").unwrap_or(&s))
}

/// The ring's arc for share `f` of the window: clockwise from the top of a 12-unit box, radius 5 (a hair at the least, the
/// whole circle at the most).
fn arc(f: f64) -> String {
    let f = f.clamp(0.01, 1.0);
    if f >= 0.999 {
        return "M6 1 A5 5 0 1 1 6 11 A5 5 0 1 1 6 1".to_owned();
    }
    let a = std::f64::consts::TAU * f;
    let (x, y) = (6.0 + 5.0 * a.sin(), 6.0 - 5.0 * a.cos());
    format!("M6 1 A5 5 0 {} 1 {x:.3} {y:.3}", u8::from(f > 0.5))
}

fn mix(a: Rgba, b: Rgba, k: f32) -> Rgba {
    let m = |x: f32, y: f32| x + (y - x) * k;
    Rgba { r: m(a.r, b.r), g: m(a.g, b.g), b: m(a.b, b.b), a: m(a.a, b.a) }
}

/// The Fill of scene prefab `path` (`hn.ChatControlInk.rest`); the scene always has it.
pub(super) fn fill_of(w: WorldRef<'_>, path: &str) -> Fill {
    view::prefab(w, path).and_then(|p| p.try_cloned::<&Fill>()).unwrap_or_else(|| panic!("scene/widgets/chat.flecs has no Fill on {path}"))
}

pub(super) fn set_fill(v: View<'_>, f: Fill) {
    if v.entity().try_cloned::<&Fill>() != Some(f) {
        v.set(f);
    }
}

/// The status bar's quiet controls' inks (hn.ChatControlInk): at rest, under the pointer, and a level's unlit bars.
pub(super) struct ControlInk {
    pub rest: Fill,
    pub hot: Fill,
    pub off: Fill,
}

impl ControlInk {
    pub(super) fn of(w: WorldRef<'_>) -> Self {
        Self {
            rest: fill_of(w, "hn.ChatControlInk.rest"),
            hot: fill_of(w, "hn.ChatControlInk.hot"),
            off: fill_of(w, "hn.ChatControlInk.off"),
        }
    }

    /// The ink of control `v` now: at rest eased toward hot by its hover spring (which starts with it).
    pub(super) fn now(&self, v: View<'_>) -> Fill {
        let e = v.entity();
        if !e.has(HoverAmt::id()) {
            e.set(HoverAmt::default());
        }
        let k = e.try_cloned::<&HoverAmt>().map_or(0.0, |h| h.spring.x.clamp(0.0, 1.0));
        Fill { dark: mix(self.rest.dark, self.hot.dark, k), light: mix(self.rest.light, self.hot.light, k) }
    }
}

/// Where `effort` stands among the efforts `model` takes (lowest first): its place and how many there are; None when
/// the model is unknown or does not list it (the control then says it in words).
fn level(model: Option<&ModelChoice>, effort: &str) -> Option<(usize, usize)> {
    let m = model?;
    let at = m.efforts.iter().position(|e| e.as_str() == effort)?;
    Some((at, m.efforts.len()))
}

/// What the ends of the effort's slider mean.
const FASTER: &str = "Faster";
const SMARTER: &str = "Smarter";
/// How long a sent effort may go unsaid by the chat before the control shows what the chat says again (the daemon's log
/// says why it did not take).
const UNSAID_SECS: f32 = 6.0;
/// AppKit's scroll phases (NSEventPhase) that end a swipe.
const ENDED: usize = 8 | 16;

/// An effort moved to here (a swipe, a drag, a key) that the chat does not say it runs with yet.
#[derive(Clone, Debug)]
pub struct Wish {
    effort: String,
    /// When it last moved (the app's clock).
    at: f32,
    /// Sent to the daemon.
    sent: bool,
    /// Still held: a drag or a swipe not let go (it is sent only once let go).
    held: bool,
}

/// A chat's effort as this app moves it: the wish not yet said back, the swipe over its control, when it last moved
/// (the control says it in words a moment after), and the effort shown last frame (what a step steps from).
#[derive(Default)]
pub struct EffortState {
    wish: Option<Wish>,
    swipe: Swipe,
    moved: Option<f32>,
    shown: String,
}

impl EffortState {
    /// The effort to show for a chat that says `said` (an ended one: what was last chosen for it, which its resume starts
    /// with): the wish while there is one. A sent wish goes once the chat says it, or [`UNSAID_SECS`] after it moved.
    fn shown(&mut self, t: &Transcript, ended: bool, said: &str, now: f32) -> String {
        let said = if ended && !t.chosen_effort.is_empty() { t.chosen_effort.as_str() } else { said };
        if self.wish.as_ref().is_some_and(|w| w.sent && (w.effort == said || now - w.at > UNSAID_SECS)) {
            self.wish = None;
        }
        let shown = self.wish.as_ref().map_or(said, |w| w.effort.as_str()).to_owned();
        self.shown.clone_from(&shown);
        shown
    }
}

/// The efforts the model chat `t` runs takes, lowest first (none when unknown or it takes none).
fn efforts_of(t: &Transcript, kind: &str, kept: &BTreeMap<String, Vec<ModelChoice>>) -> Vec<String> {
    current(t, models_of(t, kind, kept), shown(t, ("", "")).0).map(|m| m.efforts.clone()).unwrap_or_default()
}

impl App {
    /// The status bar's picker, context meter and plan meter for chat `sid` (`bar`: hn.ChatPane's foot.status; transcript
    /// `t`, an agent of kind `kind`, `listed` the daemon's `(model, effort)` for it, `ended` when no agent runs it), and
    /// its menu while `open`.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn fill_chat_pick(
        &self, bar: View<'_>, sid: u64, t: &Transcript, kind: &str, open: Option<Pick>, nav: &mut PickNav, eff: &mut EffortState,
        listed: (&str, &str), ended: bool, ctx_open: bool,
    ) {
        let w = self.world.world_ref();
        let kept = self.link.agent_models();
        let Label { models, cur, name, effort: said, .. } = Label::of(t, kind, &kept, listed);
        let effort = eff.shown(t, ended, said, self.time);
        let context = t.context.as_ref().filter(|c| c.window > 0);
        let (Some(pick), Some(meter_slot)) = (bar.part("pick"), bar.part("meter")) else { return };
        let ink = ControlInk::of(w);
        // The model's name and its effort's level; an effort with no place among its model's said in words
        // ("Opus · Medium"), and the model alone when the effort is unknown. While the effort moves, and a moment after,
        // its name after the meter.
        let level = level(cur, &effort);
        let words = match level {
            None if !effort.is_empty() => format!("{name} \u{b7} {}", effort_word(&effort)),
            _ => name.to_owned(),
        };
        let feel = crate::look::pick_feel(w);
        let moving = eff.wish.is_some() || eff.moved.is_some_and(|m| self.time - m < feel.word);
        let word = (moving && level.is_some()).then(|| effort_word(&effort));
        // Its tooltip says what it is and how to change it; none while the menu is open (it would cover the menu).
        let tip = match (open, level.is_some()) {
            (Some(_), _) => String::new(),
            (None, true) => format!("{name} \u{b7} {} effort  \u{2318}/\nSwipe sideways here to change the effort", effort_word(&effort)),
            (None, false) if !effort.is_empty() => format!("{name} \u{b7} {} effort  \u{2318}/", effort_word(&effort)),
            (None, false) => "Model and effort  \u{2318}/".to_owned(),
        };
        let shown = (!name.is_empty()).then_some(("model", "hn.ChatPick", ()));
        view::for_each_of(pick, shown, |v, (), _| {
            control(v, &format!("chat:pick:{sid}:all"), &words, &tip, level, &ink, word.as_deref());
        });
        // One ring: how full the context is, breathing while the agent works (the composer's one sign of work: Return sends,
        // Escape stops). Before the first turn says how full, a working agent's ring is an empty track.
        let working = t.busy && !ended;
        let shown = (context.is_some() || working).then_some(("context", "hn.ChatContext", ()));
        view::for_each_of(meter_slot, shown, |v, (), _| {
            meter(v, context.map_or((0, 1), |c| (c.used, c.window)), ink.hot);
            breathe(v, working);
            // A press opens the popover to the whole account (context.rs), another closes it.
            let action = Action { name: format!("chat:ctx:{sid}").as_str().into() };
            if v.entity().try_cloned::<&Action>() != Some(action.clone()) {
                v.entity().set(action);
            }
        });
        let agent = if t.agent.is_empty() { kind } else { t.agent.as_str() };
        // Under the pointer, the meter's popover (context.rs), anchored to the ring; never over the open menu.
        if let Some(pop) = bar.part("pop") {
            super::context::fill_context_pop(
                pop,
                meter_slot.part("context"),
                context,
                agent,
                open.is_some(),
                ctx_open,
                (self.pointer.x, self.pointer.y),
            );
        }
        // Open, the menu floats above the control, its leading edge on the control's.
        let trigger = open.and_then(|_| pick.part("model"));
        let Some(menu) = bar.part("menu") else { return };
        let page = open.filter(|_| trigger.is_some());
        view::for_each_of(menu, page.map(|_| ("plate", "hn.ChatPickMenu", ())), |_, (), _| {});
        let (Some(plate), Some(trigger), Some(page)) = (menu.part("plate"), trigger, page) else {
            nav.slider = None;
            return;
        };
        plate.anchored(trigger.id());
        let kind = if t.agent.is_empty() { kind } else { t.agent.as_str() };
        let hovered = w.entity_named(APP).target(Hovered::id(), 0);
        let titles = self.agent_titles();
        let ctx = MenuCtx { sid, own: models, kind, kept: &kept, cur, name, effort: &effort, titles: &titles };
        if let Some(slot) = plate.part("models") {
            view::for_each_of(slot, (page != Pick::Effort).then_some(("list", "hn.ChatPickModels", ())), |_, (), _| {});
            if let Some(list) = slot.part("list") {
                model_rows(w, list, &ctx, hovered, nav);
            }
        }
        if let Some(slot) = plate.part("effort") {
            effort_section(slot, &ctx, page, nav);
        }
    }

    /// Open chat `sid`'s menu `page`, or close it when that one is open (a press on its control again); any other chat's
    /// closes.
    pub(in crate::app) fn chat_pick_toggle(&mut self, sid: u64, page: Pick) {
        let mut states = self.chat.borrow_mut();
        for (s, st) in states.iter_mut() {
            if *s != sid {
                st.pick = None;
            }
        }
        let st = states.entry(sid).or_default();
        st.pick = if st.pick == Some(page) { None } else { Some(page) };
        st.pick_nav = PickNav::default();
        drop(states);
        self.poke();
    }

    /// An action from the picker: `<sid>:<page>` opens or closes a menu (`chat:pick:`).
    pub(in crate::app) fn chat_pick_action(&mut self, rest: &str) {
        let Some((sid, page)) = rest.split_once(':') else { return };
        if let (Ok(sid), Some(page)) = (sid.parse(), Pick::parse(page)) {
            self.chat_pick_toggle(sid, page);
        }
    }

    /// A press on the models' "older" row (`chat:older:<sid>:<kind>`): that kind's older models shown, the menu kept open
    /// and the highlight on the first of them.
    pub(in crate::app) fn chat_pick_older(&mut self, rest: &str) {
        let Some((sid, kind)) = rest.split_once(':') else { return };
        let Ok(sid) = sid.parse::<u64>() else { return };
        let mut states = self.chat.borrow_mut();
        let Some(st) = states.get_mut(&sid).filter(|st| st.pick.is_some()) else { return };
        if !st.pick_nav.older.iter().any(|k| k == kind) {
            st.pick_nav.older.push(kind.to_owned());
        }
        drop(states);
        self.poke();
    }

    /// A pick from the menu (`chat:set:<sid>:model:<id>` or `...:effort:<level>`): sent to the daemon, the menu closed.
    pub(in crate::app) fn chat_pick_set(&mut self, rest: &str) {
        let Some((sid, rest)) = rest.split_once(':') else { return };
        let Ok(sid) = sid.parse::<u64>() else { return };
        let (model, effort) = match rest.split_once(':') {
            Some(("model", m)) => (Some(m.to_owned()), None),
            Some(("effort", e)) => (None, Some(e.to_owned())),
            _ => return,
        };
        self.link.chat_choose(sid, model.clone(), effort.clone());
        self.remember_chat_choice(sid, model, effort);
        if let Some(st) = self.chat.borrow_mut().get_mut(&sid) {
            st.pick = None;
        }
        self.poke();
    }

    /// A pick of another agent kind's model (`chat:spawn:<sid>:<kind>:<model>`): a chat of that kind on that model in
    /// chat `sid`'s directory, selected once it exists; chat `sid` is archived in its place when nothing was sent in it.
    pub(in crate::app) fn chat_pick_spawn(&mut self, rest: &str) {
        let mut it = rest.splitn(3, ':');
        let (Some(sid), Some(kind), Some(m)) = (it.next().and_then(|s| s.parse::<u64>().ok()), it.next(), it.next()) else { return };
        let (kind, m) = (kind.to_owned(), m.to_owned());
        self.chat_pick_close();
        let e = self.entity_of(sid);
        let cwd = e
            .and_then(|e| self.world.entity_from_id(e).try_cloned::<&model::Cwd>())
            .map(|c| c.s.as_str().to_owned())
            .filter(|c| !c.is_empty());
        let fresh = !self.sent(sid);
        self.remember_choice(&kind, Some(&m), None);
        self.spawning(&kind);
        self.spawn_on(&kind, Some(m), None, cwd);
        if fresh && let Some(e) = e {
            self.link.kill(sid);
            self.archive_object(e);
        }
    }

    /// Chat `sid` now runs `model` (or keeps its own) at `effort` (or keeps its own), as the person chose: remembered
    /// for cmd+N under its agent kind. A new model without an effort starts at its own default.
    fn remember_chat_choice(&mut self, sid: u64, model: Option<String>, effort: Option<String>) {
        let (kind, model, effort) = {
            let chats = self.chats.borrow();
            let Some(t) = chats.get(&sid) else { return };
            let kept = self.link.agent_models();
            let kind = self.agent_kind_of(sid, t);
            let cur = current(t, models_of(t, &kind, &kept), "").map(|c| c.id.clone());
            let same = model.is_none() || model == cur;
            let effort = effort.or_else(|| Some(t.effort.clone()).filter(|e| same && !e.is_empty()));
            (kind, model.or(cur), effort)
        };
        self.remember_choice(&kind, model.as_deref(), effort.as_deref());
    }

    /// Chat `sid`'s agent kind: its transcript's, else its object's.
    fn agent_kind_of(&self, sid: u64, t: &Transcript) -> String {
        match self.entity_of(sid) {
            Some(e) if t.agent.is_empty() => model::kind_name(self.world.world_ref().entity_from_id(e)).to_owned(),
            _ => t.agent.clone(),
        }
    }

    /// Each agent kind's name, from the act.Spawn actions that have a title (scene/20-keys.flecs `SpawnKind`).
    pub(in crate::app) fn agent_titles(&self) -> BTreeMap<String, String> {
        self.spawns().into_iter().filter(|p| !p.title.is_empty()).map(|p| (p.kind, p.title)).collect()
    }

    /// Type into chat `sid`'s open searchable menu (the models', or both): whether it took the text.
    pub(in crate::app) fn chat_pick_type(&mut self, sid: u64, e: Edit<'_>) -> bool {
        let mut states = self.chat.borrow_mut();
        let Some(st) = states.get_mut(&sid).filter(|st| matches!(st.pick, Some(Pick::Model | Pick::All))) else { return false };
        st.pick_nav.search.edit(e);
        drop(states);
        self.poke();
        true
    }

    /// Open chat `sid`'s models with `why` at their top (cmd+N's remembered model is no longer offered).
    pub(in crate::app) fn chat_pick_refuse(&mut self, sid: u64, why: String) {
        let mut states = self.chat.borrow_mut();
        let st = states.entry(sid).or_default();
        st.pick = Some(Pick::Model);
        st.pick_nav = PickNav { error: Some(why), ..PickNav::default() };
        drop(states);
        self.poke();
    }

    /// Close every open picker menu. Whether one was open (Escape takes it then, before it would interrupt).
    pub(in crate::app) fn chat_pick_close(&mut self) -> bool {
        let mut any = false;
        for st in self.chat.borrow_mut().values_mut() {
            any |= st.pick.take().is_some();
        }
        if any {
            self.poke();
        }
        any
    }

    /// Each frame: a press off an open menu and off the picker closes it (before this frame's actions run, so a press on
    /// a control is that control's); a press or drag on the effort's slider moves the effort; a let-go wish is sent once
    /// it has rested `PickFeel::commit`, and the control keeps drawing while its effort's words show.
    pub(in crate::app) fn step_chat_pick(&mut self) {
        self.step_effort_drag();
        self.step_effort_commit();
        if !self.pointer.went_down || self.chat.borrow().values().all(|st| st.pick.is_none()) {
            return;
        }
        let w = self.world.world_ref();
        let menus = [view::prefab(w, "ui.Menu"), view::prefab(w, "ui.SearchMenu"), view::prefab(w, "hn.ChatPickMenu")];
        let ours = |n: EntityView<'_>| {
            n.try_cloned::<&Action>().is_some_and(|a| ["chat:pick:", "chat:set:", "chat:spawn:", "chat:older:"].iter().any(|p| a.name.as_str().starts_with(p)))
                || menus.iter().flatten().any(|m| n.owns((flecs::IsA::ID, *m.id())))
        };
        let hovered = w.entity_named(APP).target(Hovered::id(), 0);
        let inside = hovered.is_some_and(|h| std::iter::successors(Some(h), |n| n.parent()).take(64).any(ours));
        if !inside {
            self.chat_pick_close();
        }
    }

    /// The effort's slider under a press: the stop nearest the pointer, held until let go (then sent once it rests).
    fn step_effort_drag(&mut self) {
        let w = self.world.world_ref();
        let pressed = w.entity_named(APP).target(Pressed::id(), 0).map(|e| *e.id());
        let mut moves = Vec::new(); // alloc: at most one open menu's slider, once a frame
        for (sid, st) in self.chat.borrow_mut().iter_mut() {
            if !self.pointer.down {
                if let Some(wish) = st.effort.wish.as_mut().filter(|w| w.held) {
                    wish.held = false;
                    wish.at = self.time;
                }
                continue;
            }
            let Some(slider) = st.pick_nav.slider.filter(|s| pressed == Some(*s)) else { continue };
            let n = st.pick_nav.efforts.len();
            if let Some(at) = level::stop_at(View::of(w.entity_from_id(slider)), self.pointer.x, n) {
                moves.push((*sid, st.pick_nav.efforts[at].clone()));
            }
        }
        for (sid, e) in moves {
            self.chat_effort_to(sid, &e, true, true);
        }
    }

    /// Send each let-go wish that has rested `PickFeel::commit` (one switch for a swipe or a drag through several stops),
    /// and keep drawing while one waits or a control still says its effort in words.
    fn step_effort_commit(&mut self) {
        let feel = crate::look::pick_feel(self.world.world_ref());
        let now = self.time;
        let mut due = Vec::new(); // alloc: the wishes due this frame, rarely more than one
        let mut waiting = false;
        for (sid, st) in self.chat.borrow_mut().iter_mut() {
            if let Some(w) = st.effort.wish.as_mut().filter(|w| !w.sent) {
                if !w.held && now - w.at >= feel.commit {
                    w.sent = true;
                    due.push((*sid, w.effort.clone()));
                } else {
                    waiting = true;
                }
            }
            waiting |= st.effort.wish.is_some() || st.effort.moved.is_some_and(|m| now - m < feel.word + 0.1);
        }
        for (sid, e) in due {
            self.link.chat_choose(sid, None, Some(e.clone()));
            self.remember_chat_choice(sid, None, Some(e));
        }
        if waiting {
            self.poke_why("effort settling");
        }
    }

    /// Chat `sid`'s effort moved to `effort` here (shown at once, sent once let go and rested): `held` while a drag or a
    /// swipe goes on, `pulse` for a trackpad tick when it changes. Whether it changed.
    fn chat_effort_to(&mut self, sid: u64, effort: &str, held: bool, pulse: bool) -> bool {
        let now = self.time;
        let changed = {
            let mut states = self.chat.borrow_mut();
            let st = states.entry(sid).or_default();
            let changed = st.effort.shown != effort;
            if changed || st.effort.wish.is_some() {
                st.effort.wish = Some(Wish { effort: effort.to_owned(), at: now, sent: false, held });
            }
            if changed {
                st.effort.shown = effort.to_owned();
                st.effort.moved = Some(now);
            }
            changed
        };
        if changed && pulse {
            self.pulse(Haptic::LevelChange);
        }
        self.poke();
        changed
    }

    /// Chat `sid`'s effort `by` stops along its model's efforts from the one shown (held at the ends, or `round` past
    /// them). The efforts are its model's; None when it has none (or is not in the list), else whether it moved.
    fn chat_effort_step(&mut self, sid: u64, by: i32, round: bool, held: bool, pulse: bool) -> Option<bool> {
        let (efforts, shown) = {
            let chats = self.chats.borrow();
            let t = chats.get(&sid)?;
            let kept = self.link.agent_models();
            let efforts = efforts_of(t, &self.agent_kind_of(sid, t), &kept);
            let shown = self.chat.borrow().get(&sid).map(|st| st.effort.shown.clone()).unwrap_or_default();
            (efforts, shown)
        };
        let n = efforts.len() as i32;
        if n == 0 {
            return None;
        }
        let at = efforts.iter().position(|e| *e == shown).map(|i| i as i32);
        let to = match at {
            Some(i) if round => (i + by).rem_euclid(n),
            Some(i) => (i + by).clamp(0, n - 1),
            None => 0,
        };
        Some(self.chat_effort_to(sid, &efforts[to as usize], held, pulse))
    }

    /// A wheel event: a sideways two-finger swipe over a chat's picker control (or its open menu's slider) steps its
    /// effort a stop per detent, a pulse at each; a swipe that begins up or down is not the picker's. Whether it took it.
    pub(in crate::app) fn chat_pick_wheel(&mut self, ev: &sidebar::Wheel, z: f32) -> bool {
        const BEGIN: usize = 1 | 32;
        let begins = ev.phase & BEGIN != 0 || (!ev.precise && ev.phase == 0 && ev.momentum == 0);
        // A swipe stays with the chat it began over, wherever the pointer goes.
        let sid = match self.swiping {
            Some(sid) if !begins => Some(sid),
            _ => self.pick_under_pointer(),
        };
        self.swiping = sid.filter(|_| ev.precise);
        let Some(sid) = sid else { return false };
        let feel: PickFeel = crate::look::pick_feel(self.world.world_ref());
        // The fingers' way: AppKit's deltas follow the content, which follows the fingers only under natural scrolling.
        let k = if ev.precise { 1.0 / z } else { 1.0 } * if ev.inverted { 1.0 } else { -1.0 };
        let steps = self.chat.borrow_mut().entry(sid).or_default().effort.swipe.wheel(
            ev.dx * k,
            ev.dy * k,
            ev.precise,
            ev.phase,
            ev.momentum,
            feel.step,
        );
        let Some(steps) = steps else {
            self.swiping = None;
            return false;
        };
        let held = ev.precise && ev.phase & ENDED == 0;
        if steps != 0 {
            let moved = self.chat_effort_step(sid, steps.signum(), false, held, true);
            if moved == Some(false) {
                self.chat.borrow_mut().entry(sid).or_default().effort.swipe.hold();
            }
            for _ in 1..steps.unsigned_abs() {
                self.chat_effort_step(sid, steps.signum(), false, held, true);
            }
        }
        if !held && let Some(w) = self.chat.borrow_mut().entry(sid).or_default().effort.wish.as_mut().filter(|w| w.held) {
            w.held = false;
            w.at = self.time;
        }
        true
    }

    /// The chat whose picker control, or open menu's effort slider, is under the pointer.
    fn pick_under_pointer(&self) -> Option<u64> {
        let w = self.world.world_ref();
        let (x, y) = (self.pointer.x, self.pointer.y);
        let over = |e: EntityView<'_>| e.try_cloned::<&Laid>().is_some_and(|l| l.contains(x, y));
        let slider = self.chat.borrow().iter().find_map(|(sid, st)| {
            let s = st.pick_nav.slider?;
            over(w.entity_from_id(s)).then_some(*sid)
        });
        slider.or_else(|| {
            self.panes().into_iter().find_map(|p| {
                let o = model::shows(w, p)?;
                if model::pane_kind(o) != model::PaneKind::Chat {
                    return None;
                }
                if self.focused_pane() != Some(p) {
                    return None;
                }
                let control = View::of(w.entity_from_id(self.ids.side_chat)).part("pick")?.part("model")?;
                over(control.entity()).then(|| self.session_of(*o.id())).flatten()
            })
        })
    }

    /// Command-/ (`pick_model`): the focused chat's models, open or closed. Shift-Command-/ (`effort_next`): its effort
    /// steps to the next its model takes (round to the first); a model with none opens the effort's menu, which says so.
    pub(in crate::app) fn chat_pick_key(&mut self, op: &str) {
        let Some((_, _, sid)) = self.focused_chat() else { return };
        if op == "pick_model" {
            return self.chat_pick_toggle(sid, Pick::Model);
        }
        if self.chat_effort_step(sid, 1, true, false, false).is_none() {
            self.chat_pick_toggle(sid, Pick::Effort);
        }
    }

    /// A key while chat `sid`'s menu is open: up and down move its highlight, Return picks the highlighted row (or closes
    /// the menu when none is), left and right step the effort. Whether the menu took the key (it is open and the key is
    /// one of those).
    pub(in crate::app) fn chat_pick_nav(&mut self, sid: u64, op: &str) -> bool {
        let mut states = self.chat.borrow_mut();
        let Some(st) = states.get_mut(&sid).filter(|st| st.pick.is_some()) else { return false };
        let page = st.pick;
        let searching = page != Some(Pick::Effort);
        let n = &mut st.pick_nav;
        let edit = match op {
            "backspace" => Some(Edit::Erase),
            "word_erase" => Some(Edit::EraseWord),
            "line_erase" => Some(Edit::Clear),
            _ => None,
        };
        if let Some(e) = edit {
            if searching {
                n.search.edit(e);
            }
            drop(states);
            self.poke();
            return true;
        }
        match op {
            "up" | "down" => {
                let picks = &n.picks;
                n.search.nav.step(op == "down", picks.len(), |i| picks[i].is_some());
                drop(states);
                self.poke();
                true
            }
            "left" | "right" if page != Some(Pick::Model) => {
                drop(states);
                self.chat_effort_step(sid, if op == "right" { 1 } else { -1 }, false, false, false);
                true
            }
            "send" | "submit" => {
                let pick = n.search.nav.hot.and_then(|i| n.picks.get(i).cloned().flatten());
                drop(states);
                match pick.as_deref() {
                    Some(a) if a.starts_with("chat:set:") => self.chat_pick_set(&a["chat:set:".len()..]),
                    Some(a) if a.starts_with("chat:spawn:") => self.chat_pick_spawn(&a["chat:spawn:".len()..]),
                    Some(a) if a.starts_with("chat:older:") => self.chat_pick_older(&a["chat:older:".len()..]),
                    _ => {
                        self.chat_pick_close();
                    }
                }
                true
            }
            _ => false,
        }
    }

    /// For an offscreen shot (shot.rs `HARNESS_NATIVE_SHOT_CHAT_MENU`): chat `sid`'s menu `page` open.
    /// `page` may carry a query typed into it after a colon (`model:son`).
    pub fn shot_chat_pick(&mut self, sid: u64, page: &str) {
        let (page, query) = page.split_once(':').unwrap_or((page, ""));
        if let Some(p) = Pick::parse(page) {
            let mut states = self.chat.borrow_mut();
            let st = states.entry(sid).or_default();
            st.pick = Some(p);
            st.pick_nav.search.edit(Edit::Type(query));
        }
    }
}

/// The picker's control: its action, its tooltip (none: empty), its words, its effort's level (`level`: the effort's
/// place among how many) and, while the effort moves, its name after the meter (`word`); in the rest ink eased toward
/// the hot one by its hover spring, a level's unlit bars faint.
fn control(v: View<'_>, action: &str, words: &str, tip: &str, level: Option<(usize, usize)>, ink: &ControlInk, word: Option<&str>) {
    v.action(action);
    if v.entity().try_cloned::<&Tooltip>().is_none_or(|t| t.text.as_str() != tip) {
        v.set(Tooltip { text: tip.into() });
    }
    let now = ink.now(v);
    if let Some(p) = v.part("label") {
        p.text(words);
        set_fill(p, now);
    }
    if let Some(m) = v.part("effort") {
        effort_meter(m, level, now, ink.off);
    }
    if let Some(slot) = v.part("word") {
        view::for_each_of(slot, word.map(|w| ("word", "hn.ChatPickWord", w)), |p, w, _| {
            p.text(w);
        });
    }
}

/// The effort's level meter (hn.ChatEffort): a bar (hn.ChatEffortBar) per effort of `n`, their heights rising evenly
/// from the bar's least to its height, those up to `at` in `lit`, the rest in `off`; with no level, no bars and no room.
fn effort_meter(m: View<'_>, level: Option<(usize, usize)>, lit: Fill, off: Fill) {
    let w = m.entity().world();
    let bar = view::prefab(w, "hn.ChatEffortBar")
        .and_then(|b| b.try_cloned::<&Size>())
        .unwrap_or_else(|| panic!("scene/widgets/chat.flecs has no Size on hn.ChatEffortBar"));
    let (tall, short) = (bar.h.value, bar.h.min);
    let (at, n) = level.unwrap_or((0, 0));
    view::for_each_of(m, (0..n).map(|i| (i.to_string(), "hn.ChatEffortBar", i)), |b, i, _| {
        let share = if n > 1 { i as f32 / (n - 1) as f32 } else { 1.0 };
        let size = Size { w: bar.w, h: Sizing::fixed(short + (tall - short) * share) };
        if b.entity().try_cloned::<&Size>() != Some(size) {
            b.set(size);
        }
        set_fill(b, if i <= at { lit } else { off });
    });
    // Its room after the name and before the chevron only while it shows.
    let pad = if level.is_some() {
        view::prefab(w, "hn.ChatEffort")
            .and_then(|p| p.try_cloned::<&harness_native_ui_layout_api::Padding>())
            .unwrap_or_else(|| panic!("scene/widgets/chat.flecs has no Padding on hn.ChatEffort"))
    } else {
        harness_native_ui_layout_api::Padding::default()
    };
    if m.entity().try_cloned::<&harness_native_ui_layout_api::Padding>() != Some(pad) {
        m.set(pad);
    }
}

/// The context meter for `(used, window)` tokens: the ring's arc in its tone, the track in `ink` faintly. The number is
/// the popover's (context.rs).
/// The context ring's parts breathe while the agent works (down to chat_working_breath), and rest at full ink when it
/// does not (a breath of low 1 dims nothing).
fn breathe(v: View<'_>, working: bool) {
    let w = v.entity().world();
    let low = if working { crate::look::width(w, "chat_working_breath_at") } else { 1.0 };
    let Some(ring) = v.part("ring") else { return };
    for part in ["track", "arc"] {
        let Some(p) = ring.part(part) else { continue };
        let e = p.entity();
        match e.try_cloned::<&crate::look::Breath>() {
            Some(b) if (b.low - low).abs() < 1e-3 => {}
            Some(b) => _ = e.set(crate::look::Breath { low, ..b }),
            None => _ = e.set(crate::look::Breath { spring: Spring::at(1.0), up: false, low }),
        }
    }
}

fn meter(v: View<'_>, (used, window): (u64, u64), ink: Fill) {
    let w = v.entity().world();
    let f = used as f64 / window as f64;
    let tone = fill_of(w, if f >= FULL { "hn.ChatContextTone.full" } else if f >= WARN { "hn.ChatContextTone.warn" } else { "hn.ChatContextTone.ok" });
    let Some(ring) = v.part("ring") else { return };
    if let Some(track) = ring.part("track") {
        let s = Stroke { dark: Rgba { a: TRACK_ALPHA, ..ink.dark }, light: Rgba { a: TRACK_ALPHA, ..ink.light } };
        if track.entity().try_cloned::<&Stroke>() != Some(s) {
            track.set(s);
        }
    }
    if let Some(a) = ring.part("arc") {
        let s = Stroke { dark: tone.dark, light: tone.light };
        if a.entity().try_cloned::<&Stroke>() != Some(s) {
            a.set(s);
        }
        let d = arc(f);
        if let Some(mut vec) = a.entity().try_cloned::<&Vector>()
            && vec.d.as_str() != d
        {
            vec.d = d.as_str().into();
            a.set(vec);
        }
    }
}

/// The open menu's query and highlight (the pointer's and the arrow keys', harness_native_ui_view::search), what each of
/// its model rows picks (None for a head, a separator or a note) as last drawn (what Return picks), an error to show at
/// its top, and its effort slider (the node a press drags, and the efforts its stops are).
#[derive(Default)]
pub struct PickNav {
    search: search::Search,
    picks: Vec<Option<String>>,
    error: Option<String>,
    slider: Option<u64>,
    efforts: Vec<String>,
    /// The agent kinds whose older models are shown (their "older" row pressed), until the menu closes.
    older: Vec<String>,
    /// The highlight has been put on the model the chat runs, once per opening (so keys start from it).
    seeded: bool,
}

/// A model in the picker's menu: its name, its version (`meta`), the line about it (`detail`), whether the chat runs it,
/// and what picking it does.
struct Choice {
    label: String,
    meta: String,
    detail: String,
    on: bool,
    action: String,
}

/// What the menu is built from: chat `sid`'s own models (`own`, of agent kind `kind`), every kind's kept lists, the
/// model it runs (`cur`, named `name`) and its `effort` (as shown).
struct MenuCtx<'a> {
    sid: u64,
    own: &'a [ModelChoice],
    kind: &'a str,
    kept: &'a BTreeMap<String, Vec<ModelChoice>>,
    cur: Option<&'a ModelChoice>,
    name: &'a str,
    effort: &'a str,
    /// Each agent kind's name ([`App::agent_titles`]).
    titles: &'a BTreeMap<String, String>,
}

/// Agent kind `kind`'s name, as a head and in a note: its act.Spawn's `SpawnKind.title` (scene/20-keys.flecs, in
/// `titles`), else the kind itself.
fn kind_title<'a>(titles: &'a BTreeMap<String, String>, kind: &'a str) -> &'a str {
    titles.get(kind).map_or(kind, String::as_str)
}

/// The longest version that rides after a model's name; a longer head of its line goes into the line about it instead.
const META_MAX: usize = 12;

/// A model's line about it (`about`, "Opus 5.5 · Best for everyday, complex tasks") as its version after its `name`
/// ("5.5") and the rest ("Best for everyday, complex tasks"); a line with no head is all rest.
fn split_about(name: &str, about: &str) -> (String, String) {
    let Some((head, rest)) = about.split_once(" \u{b7} ") else { return (String::new(), about.trim().to_owned()) };
    let meta = head.strip_prefix(name).map(str::trim).filter(|m| !m.is_empty()).unwrap_or(head.trim());
    if meta.chars().count() > META_MAX || meta == name {
        return (String::new(), if meta == name { rest.trim().to_owned() } else { about.trim().to_owned() });
    }
    (meta.to_owned(), rest.trim().to_owned())
}

/// A model's row in the menu: its name, its version quiet after it, and the line about it. Claude lists a short name
/// ("Opus") with its version at the head of its line ("Opus 5.5 · Best for everyday, complex tasks"); Codex lists one
/// id-like name ("GPT-6.1-Sol"), read here as its family ("Sol") and version ("6.1"): no maker's prefix (the section's
/// head and mark say whose it is) and no dashes. A line that only says the name again ("GPT-6.1 Terra Codex model.")
/// is dropped.
fn row_words(m: &ModelChoice) -> (String, String, String) {
    let name = if m.name.is_empty() { m.id.as_str() } else { m.name.as_str() };
    if m.about.contains(" · ") {
        let (meta, about) = split_about(name, &m.about);
        return (name.to_owned(), meta, about);
    }
    let (family, version) = family_version(name);
    let about = m.about.trim();
    let restates = about.to_ascii_lowercase().starts_with(&name.to_ascii_lowercase());
    (family, version, if restates { String::new() } else { about.to_owned() })
}

/// An id-like model name read as its family and version: "GPT-6.1-Sol" and "GPT-6.1 Terra" are ("Sol", "6.1") and
/// ("Terra", "6.1"), "gpt-5.6" is ("GPT", "5.6"), "o4-mini" is ("o4 Mini", ""). The maker's prefix goes, a word with no
/// digit in it is capitalised, the first all-digit word is the version.
fn family_version(name: &str) -> (String, String) {
    let mut maker: Option<&str> = None;
    let mut version = String::new();
    let mut words: Vec<String> = Vec::new(); // alloc: a few words of one model's name, while the menu is open
    for (i, t) in name.split(['-', ' ', '_']).filter(|t| !t.is_empty()).enumerate() {
        match t.to_ascii_lowercase().as_str() {
            "gpt" if i == 0 => maker = Some("GPT"),
            "claude" if i == 0 => maker = Some("Claude"),
            _ if version.is_empty() && t.starts_with(|c: char| c.is_ascii_digit()) && t.chars().all(|c| c.is_ascii_digit() || c == '.') => {
                t.clone_into(&mut version);
            }
            _ if t.chars().any(|c| c.is_ascii_digit()) => words.push(t.to_owned()),
            _ => {
                let mut c = t.chars();
                words.push(c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default());
            }
        }
    }
    let family = if words.is_empty() { maker.unwrap_or(name).to_owned() } else { words.join(" ") };
    (family, version)
}

/// Version `a` is newer than `b` ("6.1" after "6", "6" after "5.6"); an empty one is never newer.
fn newer(a: &str, b: &str) -> bool {
    let parts = |v: &str| v.split('.').map(|p| p.parse::<u32>().unwrap_or(0)).collect::<Vec<_>>();
    !a.is_empty() && parts(a) > parts(b)
}

/// The menu's models (hn.ChatPickModels `list`): every kind's under its head led by its maker's mark, the chat's own
/// first, filtered by what is typed; what cannot be chosen says why instead. The highlight goes where the pointer (on
/// `hovered`) or the keys put it (`nav`); the line about the highlighted model (else the chat's own) shows under them.
fn model_rows(w: WorldRef<'_>, list: View<'_>, c: &MenuCtx<'_>, hovered: Option<EntityView<'_>>, nav: &mut PickNav) {
    let mut rows: Vec<Entry<Choice>> = Vec::with_capacity(c.own.len() + 16); // alloc: the open menu's rows, once a frame
    let mut marks: BTreeMap<String, &'static str> = BTreeMap::new(); // alloc: a mark per head, once a frame
    let others = c.kept.iter().filter(|(k, _)| k.as_str() != c.kind).map(|(k, m)| (k.as_str(), m.as_slice()));
    let mut own_detail = String::new();
    for (kind, models) in std::iter::once((c.kind, c.own)).chain(others) {
        if !rows.is_empty() {
            rows.push(Entry::Sep);
        }
        let title = kind_title(c.titles, kind);
        marks.insert(title.to_owned(), maker(None, "", kind));
        rows.push(Entry::Head(title.to_owned()));
        if models.is_empty() {
            rows.push(Entry::Note(format!("Model list arrives when a {title} chat starts.")));
            continue;
        }
        let own = kind == c.kind;
        let words: Vec<(String, String, String)> = models.iter().map(row_words).collect(); // alloc: a row's words per model
        // A model with a newer version of its family in the list is older: folded behind one quiet row until that row is
        // pressed (or anything is typed: search finds every model), unless the chat runs it.
        let searching = !nav.search.query.trim().is_empty();
        let older = |i: usize| words.iter().any(|(f, v, _)| *f == words[i].0 && newer(v, &words[i].1));
        let runs = |m: &ModelChoice| own && c.cur.is_some_and(|x| x.id == m.id);
        let fold = !searching && !nav.older.iter().any(|k| k == kind);
        let folded: Vec<bool> = (0..models.len()).map(|i| fold && older(i) && !runs(&models[i])).collect(); // alloc: per model
        let hidden = folded.iter().filter(|f| **f).count();
        // The newest first, then (opened) the older ones, each kept in the order the agent lists them.
        let order: Vec<usize> = (0..models.len()).filter(|&i| !older(i) || runs(&models[i])).chain((0..models.len()).filter(|&i| older(i) && !runs(&models[i]))).collect(); // alloc: per model
        let shown: Vec<(&ModelChoice, &(String, String, String))> = order.iter().filter(|&&i| !folded[i]).map(|&i| (&models[i], &words[i])).collect(); // alloc: per model
        rows.extend(shown.into_iter().map(|(m, (label, meta, about))| {
            let (label, meta, about) = (label.clone(), meta.clone(), about.clone());
            let on = runs(m);
            let detail = if own { about } else { format!("Starts a new {title} chat. {about}").trim_end().to_owned() };
            if on {
                own_detail.clone_from(&detail);
            }
            Entry::Item {
                text: format!("{label} {} {} {title}", m.id, m.about),
                data: Choice {
                    label,
                    meta,
                    detail,
                    on,
                    action: if own {
                        format!("chat:set:{}:model:{}", c.sid, m.id)
                    } else {
                        format!("chat:spawn:{}:{kind}:{}", c.sid, m.id)
                    },
                },
            }
        }));
        if hidden > 0 {
            rows.push(Entry::Item {
                text: String::new(),
                data: Choice {
                    label: format!("{hidden} older"),
                    meta: String::new(),
                    detail: "Earlier versions of these models.".to_owned(),
                    on: false,
                    action: format!("chat:older:{}:{kind}", c.sid),
                },
            });
        }
    }
    let rows = search::filter(&nav.search.query, rows);
    // An error leads (cmd+N's remembered model no longer offered), until something is typed.
    let rows: Vec<Entry<Choice>> = match nav.error.as_ref().filter(|_| nav.search.query.is_empty()) {
        Some(e) => std::iter::once(Entry::Note(e.clone())).chain(rows).collect(),
        None => rows,
    };
    // The highlight and the pointer's row are kept by what they pick, not by index: the rows are rebuilt as things are
    // typed, an error clears or the list arrives, and an index would land on another model beside the running one's tick.
    let at = |i: Option<usize>, picks: &[Option<String>]| i.and_then(|i| picks.get(i).cloned().flatten());
    let (was_hot, was_pointer) = (at(nav.search.nav.hot, &nav.picks), at(nav.search.nav.pointer_at(), &nav.picks));
    nav.picks.clear();
    nav.picks.extend(rows.iter().map(|r| match r {
        Entry::Item { data, .. } => Some(data.action.clone()),
        _ => None,
    }));
    let find = |a: Option<String>| a.and_then(|a| nav.picks.iter().position(|p| p.as_ref() == Some(&a)));
    let (hot, pointer) = (find(was_hot), find(was_pointer));
    nav.search.nav.retarget(hot, pointer);
    // Opened with nothing typed: the highlight starts on the model the chat runs (one row lit, the one ticked).
    if !nav.seeded && !rows.is_empty() {
        nav.seeded = true;
        if nav.search.query.is_empty() && nav.search.nav.hot.is_none() {
            nav.search.nav.retarget(rows.iter().position(|r| matches!(r, Entry::Item { data: Choice { on: true, .. }, .. })), None);
        }
    }
    let picks = &nav.picks;
    nav.search.first(picks.len(), |i| picks[i].is_some());
    search::show(list, &nav.search.query, "Search models");
    let Some(items) = list.part("items") else { return };
    // The row the pointer is on (one of the items' children, or inside one), if it can be chosen.
    let items_id = items.id();
    let under = hovered
        .and_then(|h| std::iter::successors(Some(h), |n| n.parent()).take(16).find(|n| n.parent().is_some_and(|p| p.id() == items_id)))
        .and_then(|row| row.name().strip_prefix('r').and_then(|i| i.parse::<usize>().ok()))
        .filter(|&i| nav.picks.get(i).is_some_and(Option::is_some));
    nav.search.nav.pointer(under);
    let hot = nav.search.nav.hot;
    let keyed = rows.iter().enumerate().map(|(i, r)| {
        let prefab = match r {
            Entry::Head(s) if marks.get(s).is_some_and(|m| *m != "other") => "ui.MenuHeadMarked",
            Entry::Head(_) => "ui.MenuHead",
            Entry::Sep => "ui.MenuSep",
            Entry::Item { data: Choice { on: true, .. }, .. } => "ui.MenuItemOn",
            // The fold's row ("4 older"): more of the list, not a model.
            Entry::Item { data, .. } if data.action.starts_with("chat:older:") => "ui.MenuItemQuiet",
            Entry::Item { .. } => "ui.MenuItem",
            Entry::Note(_) => "ui.MenuNote",
        };
        (format!("r{i}"), prefab, (i, r))
    });
    view::for_each_of(items, keyed, |v, (i, r), _| match r {
        Entry::Head(s) => {
            harness_native_ui_view::menu::light(v.entity(), false);
            if let Some(l) = v.part("label") {
                l.text(s);
            }
            if let (Some(icon), Some(m)) = (v.part("icon"), marks.get(s)) {
                crate::look::kind_mark(w, icon.entity(), m);
            }
        }
        Entry::Note(s) => {
            harness_native_ui_view::menu::light(v.entity(), false);
            if let Some(l) = v.part("label") {
                l.text(s);
            }
        }
        Entry::Item { data: c, .. } => {
            harness_native_ui_view::menu::light(v.entity(), hot == Some(i));
            v.action(&c.action);
            if let Some(l) = v.part("label") {
                l.text(&c.label);
            }
            if let Some(m) = v.part("meta") {
                m.text(&c.meta);
            }
        }
        Entry::Sep => harness_native_ui_view::menu::light(v.entity(), false),
    });
    // The line about the highlighted model, else the chat's own, whole under the list.
    let detail = match hot.and_then(|i| rows.get(i)) {
        Some(Entry::Item { data, .. }) => data.detail.as_str(),
        _ => own_detail.as_str(),
    };
    if let Some(slot) = list.part("detail") {
        view::for_each_of(slot, (!detail.is_empty()).then_some(("about", "hn.ChatPickDetail", ())), |v, (), _| {
            if let Some(p) = v.part("words") {
                p.text(detail);
            }
        });
    }
}

/// The menu's effort (its `effort` slot): unless it opened on the models alone, a hairline under the models (when they
/// show) and the level slider on the model's efforts (hn.ChatPickEffort), or a note saying why there is none.
fn effort_section(slot: View<'_>, c: &MenuCtx<'_>, page: Pick, nav: &mut PickNav) {
    let efforts: &[String] = c.cur.map_or(&[], |m| m.efforts.as_slice());
    let note = match c.cur {
        _ if page == Pick::Model => None,
        Some(_) if !efforts.is_empty() => None,
        Some(_) => Some(format!("{} has no effort levels.", c.name)),
        None if c.own.is_empty() => Some(format!("Model list arrives when a {} chat starts.", kind_title(c.titles, c.kind))),
        None => Some(format!("{} is not in the model list, so its efforts are unknown.", c.name)),
    };
    let slider = page != Pick::Model && note.is_none();
    let rows = [
        (page == Pick::All).then_some(("sep", "ui.MenuSep")),
        slider.then_some(("level", "hn.ChatPickEffort")),
        note.is_some().then_some(("note", "ui.MenuNote")),
    ];
    view::for_each_of(slot, rows.into_iter().flatten().map(|(k, p)| (k, p, ())), |_, (), _| {});
    if let (Some(n), Some(v)) = (note.as_deref(), slot.part("note").and_then(|v| v.part("label"))) {
        v.text(n);
    }
    let s = slot.part("level").and_then(|l| l.part("slider"));
    nav.slider = s.map(View::id);
    nav.efforts.clear();
    nav.efforts.extend(efforts.iter().cloned());
    let Some(s) = s else { return };
    // An effort the model does not list sits on its default, else the first.
    let at = efforts
        .iter()
        .position(|e| e == c.effort)
        .or_else(|| c.cur.and_then(|m| m.effort.as_ref()).and_then(|d| efforts.iter().position(|e| e == d)))
        .unwrap_or(0);
    let value = if c.effort.is_empty() { effort_word(&efforts[at]) } else { effort_word(c.effort) };
    level::show(s, at, efforts.len(), level::Words { label: "Effort", value: &value, lo: FASTER, hi: SMARTER });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_models_line_splits_into_its_version_and_the_rest() {
        let s = |n: &str, a: &str| split_about(n, a);
        assert_eq!(s("Opus", "Opus 5.5 \u{b7} Best for everyday, complex tasks"), ("5.5".into(), "Best for everyday, complex tasks".into()));
        assert_eq!(s("Opus", "Best for everyday tasks"), (String::new(), "Best for everyday tasks".into()));
        assert_eq!(s("gpt-6.1", "A frontier agentic coding model for long work \u{b7} reasoning"), (String::new(), "A frontier agentic coding model for long work \u{b7} reasoning".into()));
        assert_eq!(s("Sonnet", "Sonnet \u{b7} Fast"), (String::new(), "Fast".into()));
    }

    /// Codex's id-like names read as a family and a quiet version, no maker's prefix and no dashes; Claude's keep their
    /// name, the version from the head of their line; a line that only restates the name is dropped. Older versions of a
    /// family are told by their version.
    #[test]
    fn model_rows_read_as_a_family_and_a_version() {
        assert_eq!(family_version("GPT-6.1-Sol"), ("Sol".into(), "6.1".into()));
        assert_eq!(family_version("GPT-6.1 Terra"), ("Terra".into(), "6.1".into()));
        assert_eq!(family_version("GPT-6-Astra"), ("Astra".into(), "6".into()));
        assert_eq!(family_version("gpt-5.6"), ("GPT".into(), "5.6".into()));
        assert_eq!(family_version("o4-mini"), ("o4 Mini".into(), String::new()));
        let m = |name: &str, about: &str| ModelChoice { id: name.to_ascii_lowercase(), name: name.into(), about: about.into(), ..ModelChoice::default() };
        assert_eq!(row_words(&m("GPT-6.1 Terra", "GPT-6.1 Terra Codex model.")), ("Terra".into(), "6.1".into(), String::new()));
        assert_eq!(row_words(&m("GPT-6-Luna", "Fast and affordable model for easier tasks.")), ("Luna".into(), "6".into(), "Fast and affordable model for easier tasks.".into()));
        assert_eq!(row_words(&m("Opus", "Opus 5.5 · Best for everyday, complex tasks")), ("Opus".into(), "5.5".into(), "Best for everyday, complex tasks".into()));
        assert!(newer("6.1", "6") && newer("6", "5.6") && !newer("5.6", "6") && !newer("", "6") && !newer("6", "6"));
    }

    use harness_proto::event::{Body, Event};

    fn kept() -> BTreeMap<String, Vec<ModelChoice>> {
        let efforts = ["low", "medium", "high", "xhigh"].map(str::to_owned).to_vec();
        let opus = ModelChoice { id: "opus".into(), model: "claude-opus-5-5".into(), name: "Opus 5.5".into(), efforts, ..ModelChoice::default() };
        BTreeMap::from([("claude".to_owned(), vec![opus])])
    }

    /// A cmd+N chat's control says the remembered model and effort from its first frame (its transcript seeded, before
    /// the agent or the daemon's listing say anything), and the same once its agent says it runs them and lists its
    /// models: never blank, never the effort in words, the meter's bars there from the start. Without the seed the first
    /// frame has no control at all (the flash).
    #[test]
    fn a_new_chat_shows_its_remembered_choice_from_its_first_frame() {
        let kept = kept();
        let said = |l: &Label<'_>| (l.words.clone(), l.level, l.tip.clone());
        let unseeded = Transcript::default();
        assert_eq!(Label::of(&unseeded, "claude", &kept, ("", "")).name, "", "the cause: nothing names a model yet");

        let mut t = Transcript::default();
        t.start_on(Some("opus"), Some("high"));
        let first = said(&Label::of(&t, "claude", &kept, ("", "")));
        assert_eq!(first, ("Opus 5.5".to_owned(), Some((2, 4)), "Opus 5.5 \u{b7} High effort  \u{2318}/".to_owned()));

        // The agent is up: its `session` names the model as it runs; then the daemon's listing; then its own list.
        let ev = |body: Body| Event { ts: 1, body };
        t.apply(&ev(Body::Session { agent: "claude".into(), model: "claude-opus-5-5".into(), session_id: None, effort: Some("high".into()) }));
        assert_eq!(said(&Label::of(&t, "claude", &kept, ("", ""))), first);
        assert_eq!(said(&Label::of(&t, "claude", &kept, ("claude-opus-5-5", "high"))), first);
        t.apply(&ev(Body::Models { models: kept["claude"].clone() }));
        assert_eq!(said(&Label::of(&t, "claude", &kept, ("claude-opus-5-5", "high"))), first);

        // A choice in the log (a Codex spawn's, or one the person made) is the chat's own and wins over the seed.
        let mut c = Transcript::default();
        c.apply(&ev(Body::Choice { model: Some("opus".into()), effort: Some("low".into()) }));
        c.start_on(Some("opus"), Some("high"));
        assert_eq!(Label::of(&c, "claude", &kept, ("", "")).level, Some((0, 4)));
    }
}
