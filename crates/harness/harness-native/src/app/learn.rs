//! Learn, the app's home (scene/widgets/learn.flecs): what the main card shows while no session is selected.
//!
//! A person says what they want to learn; a tutor (a chat session of an agent, asked aside so its conversation stays
//! empty) writes the course's outline, then each lesson's steps when the lesson is opened; a lesson is walked one screen
//! at a time; what was learned is remembered and comes due again. The lessons, the memory and the walk are
//! `harness-learn`'s state machines; this file is their host: it draws the screen the state says, turns keys and presses
//! into inputs, asks the tutor, and keeps everything in `$HARNESS_HOME/learn.json`.

use super::*;
use super::chat::Draft;

mod page;
use harness_learn::{Course, Input, Lesson, Mark, Progress, Run, Step, tutor};
use harness_native_ui_layout_api::{Size, SizeKind, Sizing};

/// The name the app gives the tutor's session; a session by this name is never a sidebar row (src/app.rs).
pub(super) const TUTOR_NAME: &str = "Tutor";

/// A tutor's answer, or the session it runs in, come from the link's threads.
enum Arrived {
    /// The tutor's session was started.
    Tutor(u64),
    /// The outline asked for in generation `ask`.
    Outline { ask: u64, answer: Result<String, String> },
    /// The steps of `lesson` of `course` asked for in generation `ask`.
    Steps { ask: u64, course: String, lesson: String, answer: Result<String, String> },
}

/// What the tutor is being asked for.
#[derive(Clone)]
enum Want {
    Outline { topic: String },
    Steps { course: String, lesson: String },
}

/// Which screen shows.
enum Screen {
    Home,
    /// A course's path: its lessons, and where the person is on it.
    Course { course: String },
    /// A tutor is writing: what, in words.
    Making { kicker: String, title: String, sub: String },
    /// A lesson or a review is walked ([`Learn::run`]).
    Step,
    /// A run is over.
    Done { kicker: String, title: String, score: String, next: String, course: String },
}

/// What is kept between launches.
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct Saved {
    #[serde(default)]
    courses: Vec<Course>,
    #[serde(default)]
    progress: Progress,
    /// The tutor's session, reused while the daemon still has it.
    #[serde(default)]
    tutor: Option<u64>,
}

pub struct Learn {
    /// A shot's fixed state ([`App::shot_learn`]): nothing is read from or written to disk.
    pub(super) shot: bool,
    loaded: bool,
    saved: Saved,
    screen: Screen,
    run: Option<Run>,
    /// The start page's field: the chat input's editor (src/app/chat.rs Draft), single-line.
    draft: Draft,
    /// A fill-in step's field, the same editor; what it holds is the run's answer typed ([`Learn::field`]).
    fill: Draft,
    /// The field with the keys this frame, its text node (for copy's flash); 0 when none shows.
    field_node: std::cell::Cell<u64>,
    /// Why the last ask of the tutor failed, shown on the home.
    note: String,
    /// What the tutor is asked for now, and its generation: an answer of an older one (cancelled) is dropped.
    want: Option<Want>,
    ask: u64,
    inbox: Arc<Mutex<Vec<Arrived>>>,
    /// The tutor's session is being started (its id has not come yet): nothing starts another meanwhile.
    starting: bool,
    /// The step's scroll area this frame (hn.LearnStep's `content`; 0 when no step shows), and whether the step is new
    /// (it starts at its top).
    area: std::cell::Cell<u64>,
    fresh: std::cell::Cell<bool>,
    /// Where the arrow keys are in an order check: the item highlighted (its place in the list now).
    at: usize,
    /// The session this launch started, and how many it started: at most one, ever.
    started: Option<u64>,
    starts: u32,
    /// The web page that draws Learn (learn/page.rs): its view (0 until the host opened it), the node showing it this
    /// frame (0 when none), and the screen last sent to it.
    page_view: std::cell::Cell<u64>,
    page_node: std::cell::Cell<u64>,
    page_sent: String,
}

impl Default for Learn {
    fn default() -> Self {
        Self {
            shot: false,
            loaded: false,
            saved: Saved::default(),
            screen: Screen::Home,
            run: None,
            draft: Draft::default(),
            fill: Draft::default(),
            field_node: std::cell::Cell::new(0),
            note: String::new(),
            want: None,
            ask: 0,
            inbox: Arc::new(Mutex::new(Vec::new())),
            area: std::cell::Cell::new(0),
            fresh: std::cell::Cell::new(true),
            at: 0,
            starting: false,
            started: None,
            starts: 0,
            page_view: std::cell::Cell::new(0),
            page_node: std::cell::Cell::new(0),
            page_sent: String::new(),
        }
    }
}

fn path() -> std::path::PathBuf {
    crate::link::home().join("learn.json")
}

/// Today, as days since the Unix epoch in local time (so a day turns at the person's midnight).
fn today() -> harness_learn::Day {
    let now = model::now_ms() / 1000;
    // SAFETY: localtime_r writes only the `tm` it is given.
    let off = unsafe {
        let t = now as libc::time_t;
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        tm.tm_gmtoff
    };
    ((now as i64 + off as i64).max(0) / 86_400) as harness_learn::Day
}

impl Learn {
    fn load(&mut self) {
        if self.shot || std::mem::replace(&mut self.loaded, true) {
            return;
        }
        match std::fs::read(path()) {
            Ok(bytes) => match serde_json::from_slice::<Saved>(&bytes) {
                Ok(saved) => self.saved = saved,
                // Refused, never read as empty: the next save would write over what the person learned.
                Err(e) => {
                    self.note = format!("{} is not Learn's saved state ({e}); nothing is saved until it is moved away", path().display());
                    self.loaded = false;
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                self.note = format!("reading {}: {e}", path().display());
                self.loaded = false;
            }
        }
    }

    /// Write the saved state (through a temporary file and a rename). A failure is said on the home.
    fn save(&mut self) {
        if !self.loaded {
            return;
        }
        let p = path();
        let tmp = p.with_extension("json.tmp");
        let done = serde_json::to_vec_pretty(&self.saved)
            .map_err(|e| e.to_string())
            .and_then(|b| std::fs::write(&tmp, b).map_err(|e| e.to_string()))
            .and_then(|()| std::fs::rename(&tmp, &p).map_err(|e| e.to_string()));
        if let Err(e) = done {
            self.note = format!("saving {}: {e}", p.display());
        }
    }

    fn course(&self, id: &str) -> Option<&Course> {
        self.saved.courses.iter().find(|c| c.id == id)
    }

    /// The field that has the keys now: the start page's, or an unanswered fill-in's (kept in step with the run's
    /// answer: a new step's field starts from what the run holds).
    fn field(&mut self) -> Option<&mut Draft> {
        match (&self.screen, self.run.as_ref().and_then(Run::view)) {
            (Screen::Home, _) => Some(&mut self.draft),
            (Screen::Step, Some(v)) if v.mark.is_none() && matches!(v.step, Step::Fill { .. }) => {
                if self.fill.text != v.typed {
                    self.fill = line_draft(v.typed);
                }
                Some(&mut self.fill)
            }
            _ => None,
        }
    }

    /// Edit the field with the keys by `f`: one line (a newline or tab is a space), and a fill-in's words handed to the
    /// run as its answer. Whether a field took it.
    fn edit(&mut self, f: impl FnOnce(&mut Draft)) -> bool {
        let Some(d) = self.field() else { return false };
        f(d);
        if d.text.contains(['\n', '\r', '\t']) {
            d.text = d.text.replace(['\n', '\r', '\t'], " ");
        }
        if let (Screen::Step, Some(run)) = (&self.screen, self.run.as_mut()) {
            run.step(Input::Type(self.fill.text.clone()), &mut self.saved.progress, today());
        }
        true
    }

    /// Start walking `lesson` of `course` if its steps are written; whether it did.
    fn start_lesson(&mut self, course: &str, lesson: &str) -> bool {
        let Some(c) = self.course(course) else { return false };
        let Some(run) = c.lesson(lesson).and_then(|l| Run::lesson(c, l, model::now_ms())) else { return false };
        self.run = Some(run);
        self.screen = Screen::Step;
        true
    }

    /// The run in hand ended (its last step was left): the Done screen, and the progress saved.
    fn finish(&mut self) {
        let Some(run) = self.run.take() else { return };
        let Some(sum) = run.summary() else {
            self.run = Some(run);
            return;
        };
        let course = run.course_id().to_owned();
        let c = self.course(&course);
        let lesson = run.lesson_id().and_then(|id| c.and_then(|c| c.lesson(id)));
        let kicker = match lesson {
            Some(l) => l.title.clone(),
            None => "Review".to_owned(),
        };
        let title = match (sum.asked, sum.right_first) {
            (a, r) if a == r => "All of it, first try.".to_owned(),
            (a, r) if r * 2 >= a => "Done. That is learned.".to_owned(),
            _ => "Done. It will come back.".to_owned(),
        };
        let score = format!("{} of {} right the first time.", sum.right_first, sum.asked);
        let more = c.is_some_and(|c| self.saved.progress.next_lesson(c).is_some());
        let next = match (sum.missed.len(), more) {
            (0, true) => "Each of these comes back tomorrow, then further apart as you keep getting it.".to_owned(),
            (0, false) => "That was the course's last lesson. Reviews keep it: each comes back tomorrow, then further apart.".to_owned(),
            (n, _) => format!("{n} you missed {} due again today; the rest tomorrow.", if n == 1 { "is" } else { "are" }),
        };
        self.screen = Screen::Done { kicker, title, score, next, course };
        self.save();
    }
}

/// Typed text as a field takes it: no control characters, no AppKit function-key code points.
fn typed(text: &str) -> String {
    text.chars().filter(|c| !c.is_control() && !('\u{F700}'..='\u{F8FF}').contains(c)).collect()
}

fn percent(v: View<'_>, share: f32) {
    let want = Size { w: Sizing { kind: SizeKind::Percent, value: share.clamp(0.0, 1.0), min: 0.0, max: 0.0 }, h: Sizing::GROW };
    if v.entity().try_cloned::<&Size>() != Some(want) {
        v.set(want);
    }
}

/// One keyed child of `slot`: the primary button saying `label` that does `action`, or its faint stand-in when there
/// is nothing to do (`action` empty).
fn button(slot: View<'_>, label: &str, action: &str) {
    let (key, prefab) = if action.is_empty() { ("off", "hn.LearnButtonOff") } else { ("on", "hn.LearnButton") };
    view::for_each_of(slot, [(key, prefab, ())], |v, (), _| {
        if let Some(l) = v.part("label") {
            l.text(label);
        }
        if !action.is_empty() {
            v.action(action);
        }
    });
}

impl App {
    /// Whether Learn has the keyboard: the focused pane shows no object and nothing is open over it.
    pub(super) fn learn_focused(&self) -> bool {
        self.palette.is_none()
            && !self.tree_focus
            && self.focused_pane().is_some_and(|p| model::shows(self.world.world_ref(), p).is_none())
    }

    /// Whether a field on the start page or in a lesson has the keys: the chat input's edits act on it.
    pub(in crate::app) fn learn_editing(&self) -> bool {
        self.learn_focused() && !self.renaming() && self.learn.borrow_mut().field().is_some()
    }

    /// The chat input's edits on the field that has the keys ([`App::with_draft`]).
    pub(in crate::app) fn learn_draft(&mut self, f: impl FnOnce(&mut Draft)) -> bool {
        let took = self.learn.borrow_mut().edit(f);
        self.poke();
        took
    }

    /// An `act.Edit` on the field (a move, a selection, an erase); Return, Escape and the arrows Learn binds itself.
    pub(in crate::app) fn learn_edit(&mut self, op: &str) {
        self.learn_draft(|d| _ = d.edit_line(op));
    }

    /// Command-C on the field: its selection, and its node to flash.
    pub(in crate::app) fn learn_copy(&self) -> Option<Option<(String, chat::Copied)>> {
        if !self.learn_editing() {
            return None;
        }
        let mut l = self.learn.borrow_mut();
        let node = l.field_node.get();
        let text = l.field().map(|d| d.selected().to_owned()).unwrap_or_default();
        Some((!text.is_empty() && node != 0).then_some((text, chat::Copied::Field(node))))
    }

    /// The input method's marked text in the field, while one has the keys.
    pub(in crate::app) fn learn_marked(&self) -> Option<Option<String>> {
        if !self.learn_editing() {
            return None;
        }
        Some(self.learn.borrow_mut().field().and_then(|d| d.marked.clone()))
    }

    /// Take what the link's threads brought since the last frame.
    fn learn_arrivals(&self) {
        let mut l = self.learn.borrow_mut();
        let arrived: Vec<Arrived> = std::mem::take(&mut *l.inbox.lock().unwrap_or_else(|e| e.into_inner()));
        for a in arrived {
            match a {
                Arrived::Tutor(id) => {
                    l.starting = false;
                    l.started = Some(id);
                    l.saved.tutor = Some(id);
                    l.save();
                    self.link.rename(id, TUTOR_NAME);
                    self.learn_ask(&mut l);
                }
                Arrived::Outline { ask, answer } if ask == l.ask => {
                    l.want = None;
                    match answer.map_err(|e| e.to_string()).and_then(|t| Course::parse(&t).map_err(|e| e.to_string())) {
                        Ok(mut course) => {
                            // A second course on the same topic keeps its own progress.
                            let base = course.id.clone();
                            let mut n = 2;
                            while l.course(&course.id).is_some() {
                                course.id = format!("{base}-{n}");
                                n += 1;
                            }
                            let (cid, first) = (course.id.clone(), course.lessons[0].id.clone());
                            l.saved.courses.insert(0, course);
                            l.draft = Draft::default();
                            l.save();
                            self.learn_open(&mut l, &cid, &first);
                        }
                        Err(e) => {
                            l.note = e;
                            l.screen = Screen::Home;
                        }
                    }
                }
                Arrived::Steps { ask, course, lesson, answer } if ask == l.ask => {
                    l.want = None;
                    let made = answer.and_then(|t| {
                        let c = l.course(&course).ok_or_else(|| format!("course {course:?} is gone"))?;
                        let outline = c.lesson(&lesson).ok_or_else(|| format!("lesson {lesson:?} of course {course:?} is gone"))?;
                        outline.with_steps(&t).map_err(|e| e.to_string())
                    });
                    match made {
                        Ok(written) => {
                            if let Some(slot) = l.saved.courses.iter_mut().find(|c| c.id == course).and_then(|c| c.lessons.iter_mut().find(|x| x.id == lesson)) {
                                *slot = written;
                            }
                            l.save();
                            if !l.start_lesson(&course, &lesson) {
                                l.screen = Screen::Home;
                            }
                        }
                        Err(e) => {
                            l.note = e;
                            l.screen = Screen::Home;
                        }
                    }
                }
                // An answer to an ask the person cancelled.
                Arrived::Outline { .. } | Arrived::Steps { .. } => {}
            }
        }
    }

    /// Ask the tutor for what is wanted, starting its session first when there is none (the ask is sent when the
    /// session arrives).
    fn learn_ask(&self, l: &mut Learn) {
        let Some(want) = l.want.clone() else { return };
        // The session started this launch is the tutor (the daemon's list may not show it yet, so it is not looked up);
        // else the saved one, if the daemon still has it and it runs.
        let alive = l.started.or_else(|| l.saved.tutor.filter(|&sid| self.entity_of(sid).is_some_and(|e| self.status_of(e) == "running")));
        let Some(tutor) = alive else {
            // One start at a time, and one for the app's life: a session that cannot be started is said, never tried
            // again on its own (each try is an agent process).
            if l.starting {
                return;
            }
            if l.starts >= 1 {
                l.want = None;
                l.note = "The tutor's session could not be started; see the harness daemon's log.".to_owned();
                l.screen = Screen::Home;
                return;
            }
            l.starting = true;
            l.starts += 1;
            let inbox = l.inbox.clone();
            self.link.spawn("claude", None, None, None, 100, 30, move |id| {
                inbox.lock().unwrap_or_else(|e| e.into_inner()).push(Arrived::Tutor(id));
            });
            return;
        };
        let (inbox, ask) = (l.inbox.clone(), l.ask);
        match want {
            Want::Outline { topic } => {
                self.link.chat_aside(tutor, &tutor::outline(&topic), move |answer| {
                    inbox.lock().unwrap_or_else(|e| e.into_inner()).push(Arrived::Outline { ask, answer });
                });
            }
            Want::Steps { course, lesson } => {
                let Some(c) = l.course(&course) else { return };
                let Some(outline) = c.lesson(&lesson) else { return };
                let prompt = tutor::steps(c, outline, &l.saved.progress.shaky(c));
                self.link.chat_aside(tutor, &prompt, move |answer| {
                    inbox.lock().unwrap_or_else(|e| e.into_inner()).push(Arrived::Steps { ask, course, lesson, answer });
                });
            }
        }
    }

    /// Open `lesson` of `course`: walk it if it is written, else ask the tutor to write it.
    fn learn_open(&self, l: &mut Learn, course: &str, lesson: &str) {
        l.note.clear();
        if l.start_lesson(course, lesson) {
            return;
        }
        let Some((ct, lt)) = l.course(course).and_then(|c| Some((c.title.clone(), c.lesson(lesson)?.title.clone()))) else { return };
        l.ask += 1;
        l.want = Some(Want::Steps { course: course.to_owned(), lesson: lesson.to_owned() });
        l.screen = Screen::Making { kicker: ct, title: lt, sub: "Writing this lesson: a few short steps, mostly questions".to_owned() };
        self.learn_ask(l);
    }

    /// Continue `course` (the home's card): what is due first, as a review, then its next lesson.
    fn learn_continue(&self, l: &mut Learn, course: &str) {
        let Some(c) = l.course(course) else { return };
        let due = l.saved.progress.due(c, today());
        if let Some(run) = Run::review(c, &due, model::now_ms()) {
            l.run = Some(run);
            l.screen = Screen::Step;
            return;
        }
        let next = l.saved.progress.next_lesson(c).map(|x| x.id.clone());
        match next {
            Some(lesson) => self.learn_open(l, course, &lesson),
            None => l.note = format!("\u{201c}{}\u{201d} is finished and nothing is due today.", c.title),
        }
    }

    /// A press in Learn (`learn:<what>`).
    pub(super) fn learn_action(&mut self, what: &str) {
        self.tree_focus = false;
        if what == "show" {
            // The sidebar's Learn row: home, so nothing is selected and the main card shows Learn.
            self.home = true;
            self.select(None);
            self.poke();
            return;
        }
        if let Some(sid) = what.strip_prefix("from:").and_then(|s| s.parse::<u64>().ok()) {
            return self.learn_from_chat(sid);
        }
        // A course's row in the sidebar: its path in front, whatever was selected put down.
        if what.starts_with("course:") {
            self.home = true;
            self.select(None);
            self.poke();
        }
        // The start page's send button: what Return does there.
        if what == "next" && self.learn_start_ask(false) {
            return;
        }
        {
            let me = &*self;
            let mut l = me.learn.borrow_mut();
            let l = &mut *l;
            let (name, arg) = what.split_once(':').unwrap_or((what, ""));
            match name {
                "next" => me.learn_next(l),
                "back" | "home" => me.learn_leave(l),
                // A course's card on the home: its path.
                "course" => {
                    if l.course(arg).is_some() {
                        l.note.clear();
                        l.screen = Screen::Course { course: arg.to_owned() };
                    }
                }
                // The path's button: the next right thing in the course.
                "go" => me.learn_continue(l, arg),
                // A lesson on the path (`<course>/<lesson>`): walk it, again if it was finished.
                "lesson" => {
                    if let Some((course, lesson)) = arg.split_once('/') {
                        me.learn_open(l, course, lesson);
                    }
                }
                "pick" => {
                    if let (Some(run), Ok(i)) = (l.run.as_mut(), arg.parse::<usize>()) {
                        run.step(Input::Choose(i), &mut l.saved.progress, today());
                    }
                }
                "up" | "down" => {
                    if let (Some(run), Ok(i)) = (l.run.as_mut(), arg.parse::<usize>()) {
                        let to = if name == "up" { i.saturating_sub(1) } else { i + 1 };
                        run.step(Input::Move { from: i, to }, &mut l.saved.progress, today());
                    }
                }
                _ => eprintln!("harness-native: learn: no action {what:?}"),
            }
        }
        self.poke();
    }

    /// Return: start the course typed on the home, check the answer in hand, or go on.
    fn learn_next(&self, l: &mut Learn) {
        match &l.screen {
            Screen::Home => {
                let topic = l.draft.text.trim().to_owned();
                if topic.is_empty() {
                    return;
                }
                l.load();
                l.note.clear();
                l.ask += 1;
                l.want = Some(Want::Outline { topic: topic.clone() });
                // The first line of what was asked, kept short: a long ask (a conversation to understand) is not echoed.
                let named: String = if topic == harness_learn::intent::SURPRISE {
                    "Something worth knowing".to_owned()
                } else {
                    let first: String = topic.lines().next().unwrap_or("").chars().take(72).collect();
                    let first = first.trim_start_matches(|c: char| !c.is_alphanumeric());
                    // Said as a title: its first letter up.
                    let mut cs = first.chars();
                    cs.next().map(|c| c.to_uppercase().chain(cs).collect()).unwrap_or_default()
                };
                l.screen = Screen::Making { kicker: "New lesson".to_owned(), title: named, sub: "Writing your first lesson".to_owned() };
                self.learn_ask(l);
            }
            Screen::Making { .. } => {}
            Screen::Course { course } => {
                let course = course.clone();
                self.learn_continue(l, &course);
            }
            Screen::Step => {
                let Some(run) = l.run.as_mut() else { return };
                let marked = run.view().is_some_and(|v| v.mark.is_some() || matches!(v.step, Step::Read { .. }));
                run.step(Input::Next, &mut l.saved.progress, today());
                if marked {
                    // A new step: the arrows start from its first item, and it shows from its top.
                    l.at = 0;
                    l.fresh.set(true);
                }
                if run.view().is_none() {
                    l.finish();
                } else {
                    // A check's first answer is remembered the moment it is marked.
                    l.save();
                }
            }
            Screen::Done { course, .. } => {
                let course = course.clone();
                l.screen = Screen::Home;
                self.learn_continue(l, &course);
            }
        }
    }

    /// Escape: out of a lesson or a wait, back to the home (what was answered stays remembered).
    fn learn_leave(&self, l: &mut Learn) {
        l.ask += 1;
        l.want = None;
        l.run = None;
        l.screen = Screen::Home;
        l.save();
    }

    /// "Understand this" on chat `sid`'s composer: what was asked and done in that conversation becomes a course, so
    /// the person can explain and redo what an agent did for them. The tutor is given the conversation's words (the
    /// person's and the agent's, newest kept, tool output left out).
    pub(super) fn learn_from_chat(&mut self, sid: u64) {
        /// The most of the conversation the tutor is given, in bytes: its newest part.
        const CONTEXT_MAX: usize = 12_000;
        let mut said = String::new();
        if let Some(t) = self.chats.borrow().get(&sid) {
            for item in &t.items {
                match item {
                    model::chat::Item::User { text, .. } => said.push_str(&format!("\nPerson: {}\n", text.trim())),
                    model::chat::Item::Text { text, .. } => said.push_str(&format!("\nAgent: {}\n", text.trim())),
                    _ => {}
                }
            }
        }
        if said.trim().is_empty() {
            return;
        }
        let mut from = said.len().saturating_sub(CONTEXT_MAX);
        while !said.is_char_boundary(from) {
            from += 1;
        }
        let topic = format!(
            "Understanding what was done in the conversation below, well enough to explain it and do it again without \
help: the ideas it relied on, why each decision was made, and what would have gone wrong otherwise. Teach the ideas, \
not the transcript.\n\nThe conversation:\n{}",
            &said[from..]
        );
        self.learn_teach(&topic);
    }

    /// "Teach me …" typed into a chat: the lesson is made here, not said there. Home, and a course on `topic` started.
    pub(super) fn learn_teach(&mut self, topic: &str) {
        self.home = true;
        self.select(None);
        self.tree_focus = false;
        {
            let me = &*self;
            let mut l = me.learn.borrow_mut();
            let l = &mut *l;
            l.load();
            l.run = None;
            l.screen = Screen::Home;
            l.draft = line_draft(topic);
            me.learn_next(l);
        }
        self.poke();
    }

    /// A key of ctx.Learn (scene/20-keys.flecs).
    /// The start page in front, as cmd+N leaves it: its field ready, whatever lesson was open kept where it was.
    pub(super) fn learn_start_page(&mut self) {
        let mut l = self.learn.borrow_mut();
        l.load();
        l.note.clear();
        l.screen = Screen::Home;
    }

    /// Return (or cmd+Return, `course`) on the start page: what is typed is asked of an agent in a new chat, or, asked
    /// for (cmd+Return, or "teach me ..."), made into a course. False when the start page is not in front.
    fn learn_start_ask(&mut self, course: bool) -> bool {
        let text = {
            let l = self.learn.borrow();
            if !matches!(l.screen, Screen::Home) {
                return false;
            }
            l.draft.text.trim().to_owned()
        };
        if text.is_empty() {
            return true;
        }
        let topic = match harness_learn::intent::of(&text) {
            harness_learn::Intent::Learn { topic } => Some(topic),
            harness_learn::Intent::Do => course.then_some(text.clone()),
        };
        if let Some(topic) = topic {
            let me = &*self;
            let mut l = me.learn.borrow_mut();
            l.draft = line_draft(&topic);
            me.learn_next(&mut l);
            return true;
        }
        self.learn.borrow_mut().draft = Draft::default();
        self.ask_new_chat(text);
        true
    }

    pub(super) fn learn_key(&mut self, key: &str) {
        if matches!(key, "next" | "course") && self.learn_start_ask(key == "course") {
            return;
        }
        {
            let me = &*self;
            let mut l = me.learn.borrow_mut();
            let l = &mut *l;
            match key {
                "next" => me.learn_next(l),
                // The arrow goes on only where nothing is left to answer: past a read step, a marked check, a finished
                // run. It never submits and never starts a course.
                "forward" => {
                    let free = match (&l.screen, l.run.as_ref().and_then(Run::view)) {
                        (Screen::Step, Some(v)) => v.mark.is_some() || matches!(v.step, Step::Read { .. }),
                        (Screen::Done { .. }, _) => true,
                        _ => false,
                    };
                    if free {
                        me.learn_next(l);
                    } else {
                        // In a field, the right arrow is the caret's.
                        l.edit(|d| _ = d.edit_line("right"));
                    }
                }
                "up" | "down" | "move_up" | "move_down" => {
                    let down = key.ends_with("down");
                    let today = today();
                    if let (Screen::Step, Some(run)) = (&l.screen, l.run.as_mut()) {
                        // What the arrows act on now: a choice's options, or an order's items.
                        let on = run.view().filter(|v| v.mark.is_none()).map(|v| match v.step {
                            Step::Choice { options, .. } => (false, options.len(), v.picked),
                            Step::Order { items, .. } => (true, items.len(), None),
                            Step::Read { .. } | Step::Fill { .. } => (false, 0, None),
                        });
                        match on {
                            Some((false, n, picked)) if n > 0 && !key.starts_with("move") => {
                                let to = match (picked, down) {
                                    (None, true) => 0,
                                    (None, false) => n - 1,
                                    (Some(i), true) => (i + 1).min(n - 1),
                                    (Some(i), false) => i.saturating_sub(1),
                                };
                                run.step(Input::Choose(to), &mut l.saved.progress, today);
                            }
                            Some((true, n, _)) if n > 0 => {
                                let from = l.at.min(n - 1);
                                let to = if down { (from + 1).min(n - 1) } else { from.saturating_sub(1) };
                                if key.starts_with("move") {
                                    run.step(Input::Move { from, to }, &mut l.saved.progress, today);
                                }
                                l.at = to;
                            }
                            _ => {}
                        }
                    }
                }
                "back" => match l.screen {
                    Screen::Home => l.draft = Draft::default(),
                    _ => me.learn_leave(l),
                },
                // Backspace under Learn's binding: the field's erase, as the chat input's.
                "erase" => _ = l.edit(|d| _ = d.edit_line("backspace")),
                _ => eprintln!("harness-native: learn: no key {key:?}"),
            }
        }
        self.poke();
    }

    /// Text typed (a key no binding takes, an input method, a paste) while Learn has the keyboard: into the home's
    /// field or a fill's; a digit picks a choice's option. Whether Learn took it.
    pub(super) fn learn_text(&mut self, text: &str) -> bool {
        if !self.learn_focused() {
            return false;
        }
        let text = typed(text.lines().next().unwrap_or(""));
        if text.is_empty() {
            return true;
        }
        {
            let mut l = self.learn.borrow_mut();
            let l = &mut *l;
            match (&l.screen, l.run.as_mut()) {
                (Screen::Home, _) => _ = l.edit(|d| d.insert(&text)),
                (Screen::Step, Some(run)) => {
                    let Some(v) = run.view() else { return true };
                    match v.step {
                        Step::Choice { options, .. } => {
                            if let Some(n) = text.parse::<usize>().ok().filter(|n| (1..=options.len()).contains(n)) {
                                run.step(Input::Choose(n - 1), &mut l.saved.progress, today());
                            }
                        }
                        Step::Fill { .. } => _ = l.edit(|d| d.insert(&text)),
                        Step::Read { .. } | Step::Order { .. } => {}
                    }
                }
                _ => {}
            }
        }
        self.poke();
        true
    }

    /// A wheel over a lesson's step: its content scrolls (a window too short for a step). Whether Learn took it.
    pub(super) fn learn_wheel(&mut self, ev: &sidebar::Wheel) -> bool {
        if !self.learn_focused() {
            return false;
        }
        let area = self.learn.borrow().area.get();
        if area == 0 {
            return false;
        }
        let e = self.world.entity_from_id(area);
        if !e.is_alive() || !e.try_cloned::<&Laid>().is_some_and(|l| l.contains(self.pointer.x, self.pointer.y)) {
            return false;
        }
        let z = self.zoom();
        let k = if ev.precise { 1.0 / z } else { 1.0 };
        if self.learn_scroll.borrow_mut().wheel(e, [ev.dx * k, ev.dy * k], ev.precise, ev.phase, ev.momentum) {
            self.poke();
        }
        true
    }

    /// Step the lesson's scroll (its physics and its overlay scroller).
    pub(super) fn step_learn_scroll(&mut self) {
        let area = self.learn.borrow().area.get();
        if area == 0 {
            return;
        }
        let e = self.world.entity_from_id(area);
        if !e.is_alive() {
            self.learn.borrow().area.set(0);
            return;
        }
        let points_per_pixel = 1.0 / f64::from(self.grid.scale.max(0.5));
        if self.learn_scroll.borrow_mut().step(e, None, points_per_pixel) {
            self.poke();
        }
    }

    /// Draw Learn into `pane` (hn.LearnPane): the one screen the state says.
    /// The sidebar's courses (hn.SideNew `courses`): one row each, like a chat's, the course in front on the selection's
    /// plate; what is due today trails it.
    pub(super) fn fill_side_courses(&self, head: u64) {
        self.learn.borrow_mut().load();
        let w = self.world.world_ref();
        let Some(list) = View::of(w.entity_from_id(head)).part("courses") else { return };
        let l = self.learn.borrow();
        let day = today();
        let front = if self.home && self.selected().is_none() {
            match &l.screen {
                Screen::Course { course } | Screen::Done { course, .. } => Some(course.as_str()),
                Screen::Step => l.run.as_ref().map(Run::course_id),
                Screen::Home | Screen::Making { .. } => None,
            }
        } else {
            None
        };
        let rows = l.saved.courses.iter().map(|c| {
            let prefab = if front == Some(c.id.as_str()) { "hn.SideCourseOn" } else { "hn.SideCourse" };
            (format!("{}:{prefab}", c.id), prefab, c)
        });
        view::for_each_of(list, rows, |row, c, _| {
            row.action(&format!("learn:course:{}", c.id));
            set_text(row, "title", &c.title);
            let due = l.saved.progress.due(c, day).len();
            set_text(row, "due", &if due == 0 { String::new() } else { due.to_string() });
        });
    }

    /// Draw Learn into `pane` (hn.LearnPane): the web page (learn/page.rs) as one browser node filling it.
    pub(super) fn fill_learn_pane(&self, pane: u64) {
        self.learn.borrow_mut().load();
        self.learn_arrivals();
        let w = self.world.world_ref();
        let Some(page) = View::of(w.entity_from_id(pane)).part("page") else { return };
        let l = self.learn.borrow();
        let view = l.page_view.get();
        view::for_each_of(page, [("web", "hn.LearnWeb", ())], |v, (), _| {
            let want = harness_native_ui_browser_api::BrowserView { view, src: Default::default() };
            if v.entity().try_cloned::<&harness_native_ui_browser_api::BrowserView>().as_ref() != Some(&want) {
                v.set(want);
            }
            l.page_node.set(v.id());
        });
    }

    /// Give the page's node the keys (the pane in front has them).
    fn learn_page_focus(&mut self) {
        let node = self.learn.borrow().page_node.get();
        if node != 0 && !self.world.entity_from_id(node).has(harness_native_ui_browser_api::BrowserFocus::id()) {
            self.browser_focus(Some(node));
        }
    }

    /// The native screens, drawn by flecs (hn.LearnHome and the rest): what the web page replaces.
    #[expect(dead_code, reason = "deleted once the web page is verified in the app")]
    fn fill_learn_native(&self, pane: u64) {
        let w = self.world.world_ref();
        let Some(page) = View::of(w.entity_from_id(pane)).part("page") else { return };
        let l = self.learn.borrow();
        let l = &*l;
        if !matches!((&l.screen, &l.run), (Screen::Step, Some(_))) {
            l.area.set(0);
        }
        let (key, prefab) = match (&l.screen, &l.run) {
            (Screen::Step, Some(_)) => ("step", "hn.LearnStep"),
            (Screen::Making { .. }, _) => ("making", "hn.LearnMaking"),
            (Screen::Done { .. }, _) => ("done", "hn.LearnDone"),
            (Screen::Course { course }, _) if l.course(course).is_some() => ("course", "hn.LearnPath"),
            _ => ("home", "hn.LearnHome"),
        };
        view::for_each_of(page, [(key, prefab, ())], |v, (), _| match (&l.screen, &l.run) {
            (Screen::Step, Some(run)) => fill_step(v, l, run),
            (Screen::Making { kicker, title, sub }, _) => {
                set_text(v, "kicker", kicker);
                set_text(v, "title", title);
                if let Some(status) = v.part("status") {
                    set_text(status, "sub", sub);
                }
            }
            (Screen::Course { course }, _) if l.course(course).is_some() => {
                if let Some(c) = l.course(course) {
                    fill_path(v, l, c);
                }
            }
            (Screen::Done { kicker, title, score, next, course }, _) => {
                set_text(v, "kicker", kicker);
                set_text(v, "title", title);
                set_text(v, "score", score);
                set_text(v, "next", next);
                let more = l.course(course).is_some_and(|c| {
                    l.saved.progress.next_lesson(c).is_some() || !l.saved.progress.due(c, today()).is_empty()
                });
                if let Some(go) = v.part("row").and_then(|r| r.part("go")) {
                    if more {
                        button(go, "Keep going", "learn:next");
                    } else {
                        view::for_each_of(go, std::iter::empty::<(&str, &str, ())>(), |_, (), _| {});
                    }
                }
            }
            _ => fill_home(v, l),
        });
    }
}

/// A field's words (hn.LearnField's `text`), its caret after the last of them: the text primitive draws the caret
/// (TextEdit, in UTF-16 units), so it sits where the words end.
/// A field's words, caret and selection from its editor (byte offsets to the text node's UTF-16 units); its text node.
fn field_text(field: View<'_>, d: &Draft) -> u64 {
    use harness_native_ui_text_api::TextEdit;
    let Some(t) = field.part("text") else { return 0 };
    let units = |b: usize| d.text.get(..b).map_or(0, |s| s.encode_utf16().count()) as u32;
    let marked = d.marked.as_deref().unwrap_or("");
    let shown = if marked.is_empty() {
        std::borrow::Cow::Borrowed(d.text.as_str())
    } else {
        std::borrow::Cow::Owned(format!("{}{marked}{}", &d.text[..d.caret], &d.text[d.caret..])) // alloc: one field's words while an input method composes
    };
    t.text(&shown);
    let caret = units(d.caret);
    let mark_hi = caret + marked.encode_utf16().count() as u32;
    let was = t.entity().try_cloned::<&TextEdit>().unwrap_or_default();
    let next = TextEdit { caret: if marked.is_empty() { caret } else { mark_hi }, anchor: units(d.anchor), mark_lo: caret, mark_hi, focused: true, ..was };
    if next != was {
        t.set(next);
    }
    t.id()
}

/// A one-line editor holding `s`, the caret at its end.
fn line_draft(s: &str) -> Draft {
    Draft { text: s.to_owned(), caret: s.len(), anchor: s.len(), ..Draft::default() }
}

fn set_text(v: View<'_>, part: &str, s: &str) {
    if let Some(p) = v.part(part) {
        p.text(s);
    }
}

fn fill_home(v: View<'_>, l: &Learn) {
    let day = today();
    if let Some(f) = v.part("field") {
        l.field_node.set(field_text(f, &l.draft));
        set_text(f, "hint", if l.draft.text.is_empty() && l.draft.marked.is_none() { "Ask anything, or name something to learn\u{2026}" } else { "" });
        if let Some(go) = f.part("go") {
            let (key, prefab) = if l.draft.text.trim().is_empty() { ("off", "hn.ChatSendOff") } else { ("on", "hn.ChatSend") };
            view::for_each_of(go, [(key, prefab, ())], |b, (), _| {
                if key == "on" {
                    b.action("learn:next");
                }
            });
        }
    }
    set_text(v, "note", &l.note);
    set_text(v, "label", if l.saved.courses.is_empty() { "" } else { "Continue" });
    if let Some(list) = v.part("courses") {
        let rows = l.saved.courses.iter().take(3).map(|c| (c.id.as_str(), "hn.LearnCourse", c));
        view::for_each_of(list, rows, |row, c, _| {
            row.action(&format!("learn:course:{}", c.id));
            if let Some(head) = row.part("head") {
                set_text(head, "title", &c.title);
                let due = l.saved.progress.due(c, day).len();
                set_text(head, "due", &match due {
                    0 => String::new(),
                    n => format!("{n} to review"),
                });
            }
            let done = c.lessons.iter().filter(|x| l.saved.progress.finished(&c.id, &x.id)).count();
            let next = match l.saved.progress.next_lesson(c) {
                Some(x) => format!("Lesson {} of {} \u{b7} {}", done + 1, c.lessons.len(), x.title),
                None => format!("All {} lessons done \u{b7} {}", c.lessons.len(), c.goal),
            };
            set_text(row, "next", &next);
            if let Some(fill) = row.part("meter").and_then(|m| m.part("fill")) {
                percent(fill, l.saved.progress.mastery(c));
            }
        });
    }
    let streak = l.saved.progress.streak.on(day);
    set_text(v, "streak", &match streak {
        0 => String::new(),
        1 => "1 day in a row".to_owned(),
        n => format!("{n} days in a row"),
    });
}

/// A course's path (hn.LearnPath): where the person is, the course's title and goal, and its lessons as one line each,
/// with only the lesson to do now opened out as a card holding the button that starts it (or the review that is due).
fn fill_path(v: View<'_>, l: &Learn, c: &Course) {
    let day = today();
    let p = &l.saved.progress;
    let done = c.lessons.iter().filter(|x| p.finished(&c.id, &x.id)).count();
    let next = p.next_lesson(c).map(|x| x.id.as_str());
    let due = p.due(c, day).len();
    let kicker = match (next, due) {
        (Some(_), _) => format!("Lesson {} of {}", done + 1, c.lessons.len()),
        (None, 0) => "Course finished".to_owned(),
        (None, n) => format!("Course finished \u{b7} {n} to review"),
    };
    set_text(v, "kicker", &kicker);
    set_text(v, "title", &c.title);
    set_text(v, "goal", &c.goal);
    let Some(list) = v.part("lessons") else { return };
    let rows = c.lessons.iter().enumerate().map(|(i, x)| {
        let look = if next == Some(x.id.as_str()) {
            "hn.LearnLessonNext"
        } else if p.finished(&c.id, &x.id) {
            "hn.LearnLesson"
        } else {
            "hn.LearnLessonLater"
        };
        (format!("{}:{look}", x.id), look, (i, x, look))
    });
    view::for_each_of(list, rows, |row, (i, x, look), _| {
        let number = (i + 1).to_string();
        if look == "hn.LearnLessonNext" {
            if let Some(head) = row.part("head") {
                if let Some(d) = head.part("disc") {
                    set_text(d, "label", &number);
                }
                set_text(head, "title", &x.title);
            }
            if let Some(body) = row.part("body") {
                set_text(body, "about", &x.about);
            }
            if let Some(go) = row.part("foot").and_then(|f| f.part("go")) {
                let label = match (due, done) {
                    (0, 0) => "Start".to_owned(),
                    (0, _) => "Start lesson".to_owned(),
                    (n, _) => format!("Review {n} first"),
                };
                button(go, &label, &format!("learn:go:{}", c.id));
            }
            return;
        }
        if let Some(d) = row.part("disc") {
            set_text(d, "label", &number);
        }
        set_text(row, "title", &x.title);
        let held = if look == "hn.LearnLesson" { format!("{:.0}% held", p.lesson_mastery(&c.id, x) * 100.0) } else { String::new() };
        set_text(row, "state", &held);
        if look == "hn.LearnLesson" {
            row.action(&format!("learn:lesson:{}/{}", c.id, x.id));
        }
    });
}

fn fill_step(v: View<'_>, l: &Learn, run: &Run) {
    let Some(s) = run.view() else { return };
    if let Some(fill) = v.part("head").and_then(|h| h.part("bar")).and_then(|b| b.part("fill")) {
        percent(fill, s.progress);
    }
    let lesson: Option<&Lesson> = run.lesson_id().and_then(|id| l.course(run.course_id()).and_then(|c| c.lesson(id)));
    let kicker = match (s.again, lesson) {
        (true, _) => "Again".to_owned(),
        (false, Some(x)) => x.title.clone(),
        (false, None) => "Review".to_owned(),
    };
    let foot_of = v;
    let Some(v) = v.part("content") else { return };
    // The scroll area, for the wheel (App::learn_wheel); a new step starts at its top.
    l.area.set(v.id());
    if l.fresh.replace(false) {
        v.set(harness_native_ui_layout_api::ScrollState { x: 0.0, y: 0.0 });
    }
    set_text(v, "kicker", &kicker);
    let (title, body, code) = match s.step {
        Step::Read { title, body, code } => (title.as_str(), body.as_str(), code.as_str()),
        Step::Choice { prompt, code, .. } => (prompt.as_str(), "", code.as_str()),
        Step::Fill { prompt, .. } | Step::Order { prompt, .. } => (prompt.as_str(), "", ""),
    };
    set_text(v, "title", title);
    set_text(v, "body", body);
    if let Some(slot) = v.part("code") {
        view::for_each_of(slot, (!code.trim().is_empty()).then_some(("code", "hn.LearnCode", ())), |c, (), _| set_text(c, "text", code));
    }
    if let Some(list) = v.part("answers") {
        match s.step {
            Step::Read { .. } => view::for_each_of(list, std::iter::empty::<(String, &str, ())>(), |_, (), _| {}),
            Step::Choice { options, answer, .. } => {
                let rows = options.iter().enumerate().map(|(i, words)| {
                    let look = match (s.mark, s.picked == Some(i), i == *answer) {
                        (None, true, _) => "hn.LearnOptionPicked",
                        (None, false, _) => "hn.LearnOption",
                        (Some(_), _, true) => "hn.LearnOptionRight",
                        (Some(_), true, false) => "hn.LearnOptionWrong",
                        (Some(_), false, false) => "hn.LearnOptionDim",
                    };
                    // The key names the look too, so an option that changes look is a new node of that prefab.
                    (format!("{i}:{look}"), look, (i, words))
                });
                view::for_each_of(list, rows, |row, (i, words), _| {
                    set_text(row, "words", words);
                    if let Some(k) = row.part("key") {
                        set_text(k, "label", &(i + 1).to_string());
                    }
                    if s.mark.is_none() {
                        row.action(&format!("learn:pick:{i}"));
                    }
                });
            }
            Step::Fill { answer, .. } => {
                view::for_each_of(list, [("field".to_owned(), "hn.LearnField", ())], |f, (), _| {
                    let d = if l.fill.text == s.typed { l.fill.clone() } else { line_draft(s.typed) };
                    l.field_node.set(field_text(f, &d));
                    let hint = match (s.mark, s.typed.is_empty()) {
                        (Some(Mark::Wrong), _) => format!("   \u{2192} {answer}"),
                        (None, true) => "Type the missing part".to_owned(),
                        _ => String::new(),
                    };
                    set_text(f, "hint", &hint);
                });
            }
            Step::Order { items, .. } => {
                let n = s.order.len();
                let rows = s.order.iter().enumerate().map(|(at, &item)| {
                    let look = match s.mark {
                        None if at == l.at.min(n.saturating_sub(1)) => "hn.LearnItemPicked",
                        None => "hn.LearnItem",
                        Some(_) if at == item => "hn.LearnItemRight",
                        Some(_) => "hn.LearnItemWrong",
                    };
                    (format!("{item}:{look}"), look, (at, item))
                });
                view::for_each_of(list, rows, |row, (at, item), _| {
                    set_text(row, "words", &items[item]);
                    if let Some(k) = row.part("key") {
                        set_text(k, "label", &(at + 1).to_string());
                    }
                    if let Some(up) = row.part("up") {
                        up.action(if at > 0 { format!("learn:up:{at}") } else { String::new() }.as_str());
                    }
                    if let Some(down) = row.part("down") {
                        down.action(if at + 1 < n { format!("learn:down:{at}") } else { String::new() }.as_str());
                    }
                });
            }
        }
    }
    if let Some(foot) = foot_of.part("foot") {
        // What the marked check was and why, docked over the button; nothing before it is marked.
        if let Some(said) = foot.part("said") {
            let shown = s.mark.map(|m| match m {
                Mark::Right => ("right", "hn.LearnSaidRight", "Right."),
                Mark::Wrong => ("wrong", "hn.LearnSaidWrong", "Not quite."),
            });
            view::for_each_of(said, shown.map(|(key, prefab, verdict)| (key, prefab, verdict)), |card, verdict, _| {
                set_text(card, "verdict", verdict);
                set_text(card, "why", s.step.why());
            });
        }
        let read = matches!(s.step, Step::Read { .. });
        let (label, action) = match (read, s.mark, s.can_submit) {
            (true, _, _) | (false, Some(_), _) => ("Continue", "learn:next"),
            (false, None, true) => ("Check", "learn:next"),
            (false, None, false) => ("Check", ""),
        };
        let hint = match (s.step, s.mark) {
            (Step::Choice { .. }, None) => "\u{2191}\u{2193} or 1\u{2013}9 to pick \u{b7} return to check",
            (Step::Order { .. }, None) => "\u{2191}\u{2193} to choose \u{b7} shift \u{2191}\u{2193} to move \u{b7} return to check",
            _ => "return",
        };
        if let Some(row) = foot.part("row") {
            set_text(row, "hint", hint);
            if let Some(go) = row.part("go") {
                button(go, label, action);
            }
        }
    }
}

/// A small course written by hand, for shots (`HARNESS_NATIVE_SHOT_LEARN`): the screens are drawn without a tutor.
const DEMO: &str = r#"{"id": "rust-borrowing", "title": "Borrowing in Rust", "goal": "Read a borrow error and know what it wants",
"lessons": [
 {"id": "owner", "title": "One owner", "about": "Every value has exactly one owner", "steps": [
  {"kind": "read", "title": "Every value has one owner", "body": "In Rust a value belongs to exactly one variable at a time: its owner. When the owner goes out of scope, the value is dropped. Assigning it to another variable moves it: the old name can no longer be used.", "code": "let a = String::from(\"hi\");\nlet b = a; // moved: `a` is gone"},
  {"kind": "choice", "id": "c1", "prompt": "After this runs, which names can still be used?", "code": "let s = vec![1, 2, 3];\nlet t = s;", "options": ["Both s and t", "Only s", "Only t", "Neither"], "answer": 2, "why": "Assigning a Vec moves it. `t` is the owner now; using `s` is a compile error, not a copy."},
  {"kind": "fill", "id": "f1", "prompt": "When its owner goes out of scope, a value is ___.", "answer": "dropped", "accept": ["freed", "destroyed"], "why": "Rust runs the value's drop at the end of the owner's scope; there is no garbage collector."},
  {"kind": "order", "id": "o1", "prompt": "Put a String's life in order.", "items": ["Created by its owner", "Moved to a new owner", "New owner goes out of scope", "Dropped"], "why": "A move changes who owns it; only the last owner's scope ending drops it."}
 ]},
 {"id": "borrow", "title": "Shared borrows", "about": "A reference reads without owning"},
 {"id": "mut", "title": "One writer", "about": "A mutable borrow is exclusive"},
 {"id": "lifetimes", "title": "Lifetimes", "about": "A borrow cannot outlive its value"}
]}"#;

impl App {
    /// A shot's Learn (`HARNESS_NATIVE_SHOT_LEARN=<screen>`, src/shot.rs): the demo course at one of its screens,
    /// nothing read from or written to disk. Screens: `home` (a course in progress), `new` (nothing yet), `read`,
    /// `choice`, `picked`, `right`, `wrong`, `fill`, `order`, `done`, `making`.
    pub fn shot_learn(&mut self, screen: &str) -> Result<(), String> {
        let course = Course::parse(DEMO).map_err(|e| e.to_string())?;
        let mut l = self.learn.borrow_mut();
        let l = &mut *l;
        l.shot = true;
        let day = today();
        let mut p = Progress::default();
        let walk = |run: &mut Run, p: &mut Progress, inputs: &[Input]| inputs.iter().for_each(|i| run.step(i.clone(), p, day));
        let mut run = Run::lesson(&course, &course.lessons[0], 7).ok_or("the demo's first lesson has no steps")?;
        let to_choice = [Input::Next];
        let to_fill = [Input::Next, Input::Choose(2), Input::Submit, Input::Next];
        match screen {
            "new" => {}
            "home" | "course" => {
                p.finish_lesson(&course.id, "owner", day.saturating_sub(1));
                for c in ["c1", "f1", "o1"] {
                    p.record(&format!("{}/owner/{c}", course.id), true, day.saturating_sub(1));
                }
                l.draft = line_draft("how tcp congestion control works");
            }
            "making" => l.screen = Screen::Making { kicker: "New lesson".into(), title: "How TCP congestion control works".into(), sub: "Writing your first lesson".into() },
            "read" => {}
            "choice" => walk(&mut run, &mut p, &to_choice),
            "picked" => walk(&mut run, &mut p, &[Input::Next, Input::Choose(2)]),
            "right" => walk(&mut run, &mut p, &[Input::Next, Input::Choose(2), Input::Submit]),
            "wrong" => walk(&mut run, &mut p, &[Input::Next, Input::Choose(0), Input::Submit]),
            "fill" => {
                walk(&mut run, &mut p, &to_fill);
                walk(&mut run, &mut p, &[Input::Type("drop".into())]);
            }
            "order" => {
                walk(&mut run, &mut p, &to_fill);
                walk(&mut run, &mut p, &[Input::Type("dropped".into()), Input::Submit, Input::Next]);
            }
            "done" => {
                walk(&mut run, &mut p, &to_fill);
                walk(&mut run, &mut p, &[Input::Type("gone".into()), Input::Submit, Input::Next]);
                for want in 0..4 {
                    let from = run.view().and_then(|v| v.order.iter().position(|&x| x == want)).unwrap_or(want);
                    run.step(Input::Move { from, to: want }, &mut p, day);
                }
                walk(&mut run, &mut p, &[Input::Submit, Input::Next, Input::Type("dropped".into()), Input::Submit, Input::Next]);
            }
            other => return Err(format!("HARNESS_NATIVE_SHOT_LEARN {other:?}: not a screen (new, home, making, read, choice, picked, right, wrong, fill, order, done)")),
        }
        if screen == "course" {
            l.draft = Draft::default();
            l.screen = Screen::Course { course: course.id.clone() };
        }
        l.saved = Saved { courses: if screen == "new" { Vec::new() } else { vec![course] }, progress: p, tutor: None };
        if matches!(screen, "read" | "choice" | "picked" | "right" | "wrong" | "fill" | "order" | "done") {
            l.run = Some(run);
            l.screen = Screen::Step;
            if screen == "done" {
                l.finish();
            }
        }
        Ok(())
    }
}
